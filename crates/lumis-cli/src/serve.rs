//! `lumis serve`: highlight for a parent process over stdin and stdout.
//!
//! Every message is a frame: a 4-byte big-endian length, then that many bytes,
//! which is how an Erlang port opened with `{:packet, 4}` frames them.
//!
//! A request is
//! `<<1, match_limit::32, cpu_limit_ms::32, language_size::16, language::binary-size(language_size), source::binary>>`.
//! `language` is a language name or a file path, as `lumis highlight -l` takes
//! it. A `match_limit` of 0 means the default, and a `cpu_limit_ms` of 0 means
//! no limit.
//!
//! A reply is
//! `<<status, max_rss_kb::32, missing_count::16, missing::binary, body::binary>>`.
//! `missing` is `missing_count` language names, each `<<size::16, name::binary-size(size)>>`.
//!
//! - Status 0: `body` is html-linked HTML. `missing` names injected languages
//!   that are not cached, whose blocks were left unhighlighted.
//! - Status 1: `body` is an error message.
//! - Status 2: the requested language is not cached, and `missing` names it.
//!
//! `max_rss_kb` is the peak resident set size of the process so far.
//!
//! The process exits as soon as stdin closes, also in the middle of a
//! highlight, so closing the port stops the work. It never downloads; languages
//! come from `lumis languages cache`.

use anyhow::{Context, Result};
use lumis_core::events::HighlightEvent;
use lumis_core::formatter::{Formatter, HtmlLinkedBuilder};
use lumis_core::languages::Language;
use lumis_wasm_runtime::{
    catalog, HighlightOptions, LanguageStore, NoNetwork, Runtime, RuntimeError, StoreConfig,
    DEFAULT_MATCH_LIMIT,
};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const HIGHLIGHT: u8 = 1;

const OK: u8 = 0;
const ERROR: u8 = 1;
const NOT_CACHED: u8 = 2;

/// The stack the Elixir NIF gives its highlighting threads. Nested injections
/// recurse once per layer.
const HIGHLIGHT_STACK_SIZE: usize = 8 * 1024 * 1024;

#[derive(clap::Args)]
pub(crate) struct ServeArgs {
    /// Languages to load before the first request, e.g. elixir,erlang
    #[arg(long, value_delimiter = ',')]
    preload: Vec<String>,
}

pub(crate) fn run(data_dir: PathBuf, args: ServeArgs) -> Result<()> {
    abort_on_panic();

    let (sender, requests) = mpsc::channel();
    thread::Builder::new()
        .name("lumis-serve".into())
        .stack_size(HIGHLIGHT_STACK_SIZE)
        .spawn(move || {
            if let Err(error) = serve(data_dir, &args.preload, &requests) {
                eprintln!("lumis serve: {error:#}");
                exit_now(1);
            }
        })
        .context("could not spawn the highlight thread")?;

    read_requests(&sender);
    exit_now(0)
}

/// A panic on either thread would leave the other one waiting forever.
fn abort_on_panic() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        std::process::abort();
    }));
}

/// Leaves without running destructors or `atexit` handlers, which could wait on
/// a lock the highlight thread holds.
fn exit_now(code: i32) -> ! {
    // SAFETY: `_exit` takes no pointers and does not return.
    unsafe { libc::_exit(code) }
}

fn read_requests(requests: &mpsc::Sender<Vec<u8>>) {
    let mut stdin = io::stdin().lock();
    let mut header = [0; 4];
    while stdin.read_exact(&mut header).is_ok() {
        let mut frame = vec![0; u32::from_be_bytes(header) as usize];
        if stdin.read_exact(&mut frame).is_err() || requests.send(frame).is_err() {
            return;
        }
    }
}

fn serve(data_dir: PathBuf, preload: &[String], requests: &mpsc::Receiver<Vec<u8>>) -> Result<()> {
    let runtime = new_runtime(data_dir)?;
    for language in preload {
        if let Err(error) = runtime.load_named_language(language) {
            eprintln!("lumis serve: could not preload {language}: {error}");
        }
    }

    let mut stdout = io::stdout().lock();
    for frame in requests {
        let reply = answer(&runtime, &frame);
        if write_reply(&mut stdout, &reply).is_err() {
            // The parent is gone.
            exit_now(0);
        }
    }
    Ok(())
}

/// The runtime the Elixir NIF builds, over a store that only reads `data_dir`.
fn new_runtime(data_dir: PathBuf) -> Result<Runtime> {
    let store = LanguageStore::new(
        StoreConfig {
            cache_dir: data_dir,
        },
        Box::new(NoNetwork),
    );
    let runtime = Runtime::with_worker_limit(1)?.with_store(store);
    for language in catalog::LANGUAGES {
        runtime.declare_language(language.id, language.aliases);
    }
    Ok(runtime)
}

struct Request<'a> {
    match_limit: u32,
    cpu_limit_ms: u32,
    language: &'a str,
    source: &'a str,
}

impl<'a> Request<'a> {
    fn decode(frame: &'a [u8]) -> Result<Self, String> {
        let malformed = || "malformed request".to_string();
        let (&kind, rest) = frame.split_first().ok_or_else(malformed)?;
        if kind != HIGHLIGHT {
            return Err(format!("unknown request type {kind}"));
        }
        let (match_limit, rest) = rest.split_first_chunk::<4>().ok_or_else(malformed)?;
        let (cpu_limit_ms, rest) = rest.split_first_chunk::<4>().ok_or_else(malformed)?;
        let (language_size, rest) = rest.split_first_chunk::<2>().ok_or_else(malformed)?;
        let language_size = usize::from(u16::from_be_bytes(*language_size));
        if rest.len() < language_size {
            return Err(malformed());
        }
        let (language, source) = rest.split_at(language_size);

        Ok(Self {
            match_limit: match u32::from_be_bytes(*match_limit) {
                0 => DEFAULT_MATCH_LIMIT,
                limit => limit,
            },
            cpu_limit_ms: u32::from_be_bytes(*cpu_limit_ms),
            language: std::str::from_utf8(language).map_err(|_| "language is not UTF-8")?,
            source: std::str::from_utf8(source).map_err(|_| "source is not UTF-8")?,
        })
    }
}

enum Reply {
    Html { html: Vec<u8>, missing: Vec<String> },
    Error(String),
    NotCached(String),
}

fn answer(runtime: &Runtime, frame: &[u8]) -> Reply {
    let request = match Request::decode(frame) {
        Ok(request) => request,
        Err(message) => return Reply::Error(message),
    };
    let _cpu_limit = match CpuLimit::arm(request.cpu_limit_ms) {
        Ok(limit) => limit,
        Err(error) => return Reply::Error(format!("could not set the CPU limit: {error}")),
    };
    highlight(runtime, &request)
}

/// The same steps as the Elixir NIF's `highlight` with the html-linked
/// formatter and its defaults, so the output is the same.
fn highlight(runtime: &Runtime, request: &Request<'_>) -> Reply {
    let language = Language::guess(Some(request.language), request.source);
    let (events, missing) = if language == Language::PlainText {
        let events: Vec<HighlightEvent<'static>> = vec![HighlightEvent::Source {
            start: 0,
            end: request.source.len(),
        }];
        (events, Vec::new())
    } else {
        let options = HighlightOptions {
            match_limit: request.match_limit,
            ..HighlightOptions::default()
        };
        match runtime.highlight_with(request.source, language.id_name(), &options) {
            Ok(output) => (output.events, output.unresolved),
            Err(RuntimeError::Parser { .. }) if !is_cached(runtime, language.id_name()) => {
                return Reply::NotCached(language.id_name().to_string());
            }
            Err(error) => return Reply::Error(error.to_string()),
        }
    };

    let formatter = match HtmlLinkedBuilder::new().language(language).build() {
        Ok(formatter) => formatter,
        Err(error) => return Reply::Error(error.to_string()),
    };
    let mut html = Vec::new();
    match formatter.render(request.source, &events, &mut html) {
        Ok(()) => Reply::Html { html, missing },
        Err(error) => Reply::Error(error.to_string()),
    }
}

/// Whether `language` loads without the network. Only asked after a load
/// failed, since it reads and hashes the parser.
fn is_cached(runtime: &Runtime, language: &str) -> bool {
    let Some(store) = runtime.store() else {
        return false;
    };
    catalog::find(language)
        .and_then(|location| store.local_package(location.package_name))
        .is_some_and(|package| store.local_parser(&package).is_some())
}

fn write_reply(out: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let (status, missing, body) = match reply {
        Reply::Html { html, missing } => (OK, missing.as_slice(), html.as_slice()),
        Reply::Error(message) => (ERROR, [].as_slice(), message.as_bytes()),
        Reply::NotCached(language) => (NOT_CACHED, std::slice::from_ref(language), [].as_slice()),
    };

    let mut header = vec![status];
    header.extend_from_slice(&max_rss_kb().to_be_bytes());
    header.extend_from_slice(&short_length(missing.len())?.to_be_bytes());
    for name in missing {
        header.extend_from_slice(&short_length(name.len())?.to_be_bytes());
        header.extend_from_slice(name.as_bytes());
    }
    let size = u32::try_from(header.len() + body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "reply is too large"))?;

    out.write_all(&size.to_be_bytes())?;
    out.write_all(&header)?;
    out.write_all(body)?;
    out.flush()
}

fn short_length(length: usize) -> io::Result<u16> {
    u16::try_from(length).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too long"))
}

/// Peak resident set size in kilobytes, from `getrusage`, so it needs no `/proc`.
fn max_rss_kb() -> u32 {
    let max_rss = u64::try_from(resource_usage().ru_maxrss).unwrap_or(0);
    // Linux reports kilobytes, macOS bytes.
    let kilobytes = if cfg!(target_os = "macos") {
        max_rss / 1024
    } else {
        max_rss
    };
    u32::try_from(kilobytes).unwrap_or(u32::MAX)
}

fn cpu_time() -> Duration {
    let usage = resource_usage();
    let duration = |time: libc::timeval| {
        Duration::new(
            u64::try_from(time.tv_sec).unwrap_or(0),
            u32::try_from(time.tv_usec).unwrap_or(0) * 1000,
        )
    };
    duration(usage.ru_utime) + duration(usage.ru_stime)
}

fn resource_usage() -> libc::rusage {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `getrusage` writes a `rusage` through the pointer, which points
    // at one. On failure the zeroed value stays.
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    }
}

/// Bounds one request's CPU time with `RLIMIT_CPU`, which the kernel enforces
/// without this process having to get scheduled: once the process has used
/// `cpu_limit_ms` more than when the request began, it gets `SIGXCPU`, which
/// kills it. The limit is whole seconds, so it rounds up. Dropping it lifts the
/// limit again.
struct CpuLimit;

impl CpuLimit {
    fn arm(cpu_limit_ms: u32) -> io::Result<Option<Self>> {
        if cpu_limit_ms == 0 {
            return Ok(None);
        }
        let deadline = cpu_time() + Duration::from_millis(u64::from(cpu_limit_ms));
        let seconds = deadline.as_secs() + u64::from(deadline.subsec_nanos() > 0);
        set_soft_cpu_limit(seconds)?;
        Ok(Some(Self))
    }
}

impl Drop for CpuLimit {
    fn drop(&mut self) {
        let _ = set_soft_cpu_limit(libc::RLIM_INFINITY);
    }
}

fn set_soft_cpu_limit(seconds: libc::rlim_t) -> io::Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: both calls only read or write the `rlimit` the pointer points at.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_CPU, &raw mut limit) != 0 {
            return Err(io::Error::last_os_error());
        }
        limit.rlim_cur = seconds.min(limit.rlim_max);
        if libc::setrlimit(libc::RLIMIT_CPU, &raw const limit) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
