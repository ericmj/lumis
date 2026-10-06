//! `lumis serve`: highlight for a parent process over stdin and stdout.
//!
//! Every message is a frame: a 4-byte big-endian length, then that many bytes,
//! which is how an Erlang port opened with `{:packet, 4}` frames them.
//!
//! Once it has loaded the languages it was asked to preload, the process writes
//! `<<2, max_rss_kb::32>>`, so a parent can wait for that before sending work.
//!
//! A request is
//! `<<kind, match_limit::32, time_limit_ms::32, cpu_limit_ms::32, language_size::16, language::binary-size(language_size), source::binary>>`.
//! `language` is a language name or a file path, as `lumis highlight -l` takes
//! it. A `match_limit` of 0 means the default, and a `time_limit_ms` or
//! `cpu_limit_ms` of 0 means no limit.
//!
//! - Kind 1 highlights a document as html-linked HTML.
//! - Kind 2 highlights the source as lines, each an html-linked fragment
//!   without the `<pre>` and `<code>` around it.
//!
//! A reply is `<<status, max_rss_kb::32, body::binary>>`.
//!
//! - Status 0: for kind 1, `body` is the HTML. For kind 2, it is the lines, each
//!   as `<<size::32, line::binary-size(size)>>`.
//! - Status 1: `body` is an error message.
//!
//! `max_rss_kb` is the peak resident set size of the process so far.
//!
//! A language whose parser is not in a `--parser-dir` renders as plain text,
//! the same as in the Elixir NIF. The process never downloads.
//!
//! The process exits as soon as stdin closes, also in the middle of a
//! highlight, so closing the port stops the work.

use anyhow::{Context, Result};
use lumis_core::annotations::{compose_annotations, Annotation};
use lumis_core::events::HighlightEvent;
use lumis_core::formatter::html::{render_lines_from_events, scope_to_class};
use lumis_core::formatter::{BudgetExhausted, Formatter, HtmlLinkedBuilder};
use lumis_core::highlights::HIGHLIGHT_NAMES;
use lumis_core::languages::Language;
use lumis_wasm_runtime::{
    catalog, package_suffix, HighlightOptions, LanguageStore, NoNetwork, Runtime, RuntimeError,
    StoreConfig, DEFAULT_MATCH_LIMIT,
};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const DOCUMENT: u8 = 1;
const LINES: u8 = 2;

const OK: u8 = 0;
const ERROR: u8 = 1;
const READY: u8 = 2;

/// The stack the Elixir NIF gives its highlighting threads. Nested injections
/// recurse once per layer.
const HIGHLIGHT_STACK_SIZE: usize = 8 * 1024 * 1024;

#[derive(clap::Args)]
pub(crate) struct ServeArgs {
    /// Directory holding installed parsers and their `lumis.json`, such as the
    /// `priv/parsers` of a `lumis_wasm_*` Hex package. Repeat it for each one
    #[arg(long = "parser-dir")]
    parser_dirs: Vec<PathBuf>,

    /// Load every language in the parser directories before the first request
    #[arg(long)]
    preload_installed: bool,

    /// Heap limit of the process in megabytes, applied with `RLIMIT_DATA`
    #[cfg(target_os = "linux")]
    #[arg(long)]
    max_data_mb: Option<u64>,
}

pub(crate) fn run(data_dir: PathBuf, args: ServeArgs) -> Result<()> {
    abort_on_panic();

    #[cfg(target_os = "linux")]
    if let Some(megabytes) = args.max_data_mb {
        limit_data(megabytes * 1024 * 1024).context("could not set the data limit")?;
    }

    let (sender, requests) = mpsc::channel();
    thread::Builder::new()
        .name("lumis-serve".into())
        .stack_size(HIGHLIGHT_STACK_SIZE)
        .spawn(move || {
            if let Err(error) = serve(data_dir, &args, &requests) {
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

fn serve(data_dir: PathBuf, args: &ServeArgs, requests: &mpsc::Receiver<Vec<u8>>) -> Result<()> {
    let runtime = new_runtime(data_dir, args.parser_dirs.clone())?;
    if args.preload_installed {
        for language in installed_languages(&args.parser_dirs) {
            if let Err(error) = runtime.load_named_language(language) {
                eprintln!("lumis serve: could not preload {language}: {error}");
            }
        }
    }

    let mut stdout = io::stdout().lock();
    if write_frame(&mut stdout, READY, &[]).is_err() {
        exit_now(0);
    }
    for frame in requests {
        let (status, body) = match answer(&runtime, &frame) {
            Ok(body) => (OK, body),
            Err(message) => (ERROR, message.into_bytes()),
        };
        if write_frame(&mut stdout, status, &body).is_err() {
            // The parent is gone.
            exit_now(0);
        }
    }
    Ok(())
}

/// The runtime the Elixir NIF builds, over a store that only reads the parser
/// directories.
fn new_runtime(data_dir: PathBuf, parser_dirs: Vec<PathBuf>) -> Result<Runtime> {
    let store = LanguageStore::new(
        StoreConfig {
            cache_dir: data_dir,
            installed_dirs: Some(parser_dirs),
        },
        Box::new(NoNetwork),
    );
    let runtime = Runtime::with_worker_limit(1)?.with_store(store);
    for language in catalog::LANGUAGES {
        runtime.declare_language(language.id, language.aliases);
    }
    Ok(runtime)
}

/// Catalog languages whose package metadata is in one of `parser_dirs`.
fn installed_languages(parser_dirs: &[PathBuf]) -> impl Iterator<Item = &'static str> + '_ {
    catalog::LANGUAGES
        .iter()
        .filter(|language| {
            package_suffix(language.package_name).is_some_and(|suffix| {
                let file = format!("{suffix}.lumis.json");
                parser_dirs.iter().any(|dir| dir.join(&file).is_file())
            })
        })
        .map(|language| language.id)
}

struct Request<'a> {
    kind: u8,
    match_limit: u32,
    time_limit_ms: Option<u64>,
    cpu_limit_ms: u32,
    language: &'a str,
    source: &'a str,
}

impl<'a> Request<'a> {
    fn decode(frame: &'a [u8]) -> Result<Self, String> {
        let malformed = || "malformed request".to_string();
        let (&kind, rest) = frame.split_first().ok_or_else(malformed)?;
        if kind != DOCUMENT && kind != LINES {
            return Err(format!("unknown request kind {kind}"));
        }
        let (match_limit, rest) = rest.split_first_chunk::<4>().ok_or_else(malformed)?;
        let (time_limit_ms, rest) = rest.split_first_chunk::<4>().ok_or_else(malformed)?;
        let (cpu_limit_ms, rest) = rest.split_first_chunk::<4>().ok_or_else(malformed)?;
        let (language_size, rest) = rest.split_first_chunk::<2>().ok_or_else(malformed)?;
        let language_size = usize::from(u16::from_be_bytes(*language_size));
        if rest.len() < language_size {
            return Err(malformed());
        }
        let (language, source) = rest.split_at(language_size);

        Ok(Self {
            kind,
            match_limit: match u32::from_be_bytes(*match_limit) {
                0 => DEFAULT_MATCH_LIMIT,
                limit => limit,
            },
            time_limit_ms: match u32::from_be_bytes(*time_limit_ms) {
                0 => None,
                limit => Some(u64::from(limit)),
            },
            cpu_limit_ms: u32::from_be_bytes(*cpu_limit_ms),
            language: std::str::from_utf8(language).map_err(|_| "language is not UTF-8")?,
            source: std::str::from_utf8(source).map_err(|_| "source is not UTF-8")?,
        })
    }
}

fn answer(runtime: &Runtime, frame: &[u8]) -> Result<Vec<u8>, String> {
    let request = Request::decode(frame)?;
    let _cpu_limit = CpuLimit::arm(request.cpu_limit_ms)
        .map_err(|error| format!("could not set the CPU limit: {error}"))?;

    let language = Language::guess(Some(request.language), request.source);
    let (events, budget) = syntax_events(runtime, &request, language)?;
    let events = compose_annotations::<()>(request.source, &events, &[] as &[Annotation<()>])
        .map_err(|error| error.to_string())?;

    if request.kind == LINES {
        return Ok(encode_lines(&render_lines_from_events(
            request.source,
            &events,
            |scope_index, _language| {
                format!(
                    r#"class="{}""#,
                    scope_to_class(HIGHLIGHT_NAMES[scope_index])
                )
            },
        )));
    }

    let formatter = HtmlLinkedBuilder::new()
        .language(language)
        .build()
        .map_err(|error| format!("{error:?}"))?;
    let mut html = Vec::new();
    formatter
        .render_budgeted_or(request.source, &events, &mut html, budget)
        .map_err(|error| error.to_string())?;
    Ok(html)
}

/// The Elixir NIF's `syntax_events`: plain text for a document with no parser
/// to walk it with, including one whose parser is not installed.
fn syntax_events(
    runtime: &Runtime,
    request: &Request<'_>,
    language: Language,
) -> Result<(Vec<HighlightEvent<'static>>, Option<BudgetExhausted>), String> {
    if language == Language::PlainText {
        return Ok((plain_text(request.source), None));
    }

    let options = HighlightOptions {
        match_limit: request.match_limit,
        time_limit_ms: request.time_limit_ms,
        ..HighlightOptions::default()
    };
    match runtime.highlight_with(request.source, language.id_name(), &options) {
        Ok(output) => Ok((output.events, output.budget)),
        Err(error) if is_parser_failure(&error) => Ok((plain_text(request.source), None)),
        Err(error) => Err(error.to_string()),
    }
}

fn plain_text(source: &str) -> Vec<HighlightEvent<'static>> {
    vec![HighlightEvent::Source {
        start: 0,
        end: source.len(),
    }]
}

/// The failures the Elixir NIF renders as plain text rather than reporting:
/// the ones a missing or broken parser causes.
fn is_parser_failure(error: &RuntimeError) -> bool {
    matches!(
        error,
        RuntimeError::Store { .. }
            | RuntimeError::Parser { .. }
            | RuntimeError::StoreFull { .. }
            | RuntimeError::Query { .. }
            | RuntimeError::LanguageNotLoaded(_)
            | RuntimeError::UnknownLanguage(_)
            | RuntimeError::LanguageNotCached(_)
            | RuntimeError::LanguageStoreUnavailable
    )
}

fn encode_lines(lines: &[String]) -> Vec<u8> {
    let mut body = Vec::with_capacity(lines.iter().map(|line| line.len() + 4).sum());
    for line in lines {
        body.extend_from_slice(&u32::try_from(line.len()).unwrap_or(u32::MAX).to_be_bytes());
        body.extend_from_slice(line.as_bytes());
    }
    body
}

fn write_frame(out: &mut impl Write, status: u8, body: &[u8]) -> io::Result<()> {
    let size = u32::try_from(1 + 4 + body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "reply is too large"))?;

    out.write_all(&size.to_be_bytes())?;
    out.write_all(&[status])?;
    out.write_all(&max_rss_kb().to_be_bytes())?;
    out.write_all(body)?;
    out.flush()
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

/// Sets both limits, so the process cannot raise its heap limit again.
#[cfg(target_os = "linux")]
fn limit_data(bytes: u64) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    // SAFETY: `setrlimit` only reads the `rlimit` the pointer points at.
    if unsafe { libc::setrlimit(libc::RLIMIT_DATA, &raw const limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
