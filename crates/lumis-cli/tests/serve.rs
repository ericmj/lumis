#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

mod common;

const DOCUMENT: u8 = 1;
const LINES: u8 = 2;

struct Serve {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    reaped: bool,
    _compile_cache: tempfile::TempDir,
}

#[derive(Debug)]
struct Reply {
    status: u8,
    max_rss_kb: u32,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }

    fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let mut rest = self.body.as_slice();
        while let Some((size, tail)) = rest.split_first_chunk::<4>() {
            let (line, tail) = tail.split_at(u32::from_be_bytes(*size) as usize);
            lines.push(String::from_utf8(line.to_vec()).unwrap());
            rest = tail;
        }
        lines
    }
}

struct Request<'a> {
    kind: u8,
    language: &'a str,
    source: &'a str,
    match_limit: u32,
    time_limit_ms: u32,
    cpu_limit_ms: u32,
}

impl<'a> Request<'a> {
    fn document(language: &'a str, source: &'a str) -> Self {
        Self {
            kind: DOCUMENT,
            language,
            source,
            match_limit: 0,
            time_limit_ms: 0,
            cpu_limit_ms: 0,
        }
    }

    fn lines(language: &'a str, source: &'a str) -> Self {
        Self {
            kind: LINES,
            ..Self::document(language, source)
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut frame = vec![self.kind];
        frame.extend_from_slice(&self.match_limit.to_be_bytes());
        frame.extend_from_slice(&self.time_limit_ms.to_be_bytes());
        frame.extend_from_slice(&self.cpu_limit_ms.to_be_bytes());
        frame.extend_from_slice(&u16::try_from(self.language.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(self.language.as_bytes());
        frame.extend_from_slice(self.source.as_bytes());
        frame
    }
}

impl Serve {
    /// Starts `lumis serve` over the test parsers and waits until it is ready.
    fn start(args: &[&str]) -> Self {
        let mut serve = Self::spawn(args);
        let ready = serve.reply();
        assert_eq!(ready.status, 2, "{ready:?}");
        assert!(ready.body.is_empty());
        serve
    }

    fn spawn(args: &[&str]) -> Self {
        let compile_cache = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_lumis"))
            .arg("--data-dir")
            .arg(compile_cache.path())
            .arg("serve")
            .arg("--parser-dir")
            .arg(common::data_dir().join("parsers"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        Self {
            child,
            stdin,
            stdout,
            reaped: false,
            _compile_cache: compile_cache,
        }
    }

    fn send(&mut self, frame: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        let size = u32::try_from(frame.len()).unwrap();
        stdin.write_all(&size.to_be_bytes()).unwrap();
        stdin.write_all(frame).unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, request: &Request<'_>) -> Reply {
        self.send(&request.encode());
        self.reply()
    }

    fn highlight(&mut self, language: &str, source: &str) -> Reply {
        self.request(&Request::document(language, source))
    }

    fn reply(&mut self) -> Reply {
        let mut size = [0; 4];
        self.stdout.read_exact(&mut size).unwrap();
        let mut frame = vec![0; u32::from_be_bytes(size) as usize];
        self.stdout.read_exact(&mut frame).unwrap();

        let (&status, rest) = frame.split_first().unwrap();
        let (max_rss_kb, body) = rest.split_first_chunk::<4>().unwrap();
        Reply {
            status,
            max_rss_kb: u32::from_be_bytes(*max_rss_kb),
            body: body.to_vec(),
        }
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait(&mut self) -> ExitStatus {
        self.child.wait().unwrap()
    }

    fn wait_with_cpu_time(&mut self) -> (ExitStatus, Duration) {
        let pid = libc::pid_t::try_from(self.child.id()).unwrap();
        let mut status = 0;
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: `wait4` writes an exit status and a `rusage` through pointers
        // to one of each.
        let usage = unsafe {
            assert_eq!(
                libc::wait4(pid, &raw mut status, 0, usage.as_mut_ptr()),
                pid
            );
            usage.assume_init()
        };
        self.reaped = true;

        let duration = |time: libc::timeval| {
            Duration::new(
                u64::try_from(time.tv_sec).unwrap(),
                u32::try_from(time.tv_usec).unwrap() * 1000,
            )
        };
        (
            ExitStatus::from_raw(status),
            duration(usage.ru_utime) + duration(usage.ru_stime),
        )
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A guard whose `or` chain takes the Elixir highlight query a while.
fn slow_elixir(terms: usize) -> String {
    let guard = (0..terms)
        .map(|term| format!("x === {term} or f(g(h({term})))"))
        .collect::<Vec<_>>()
        .join(" or ");
    format!("def f(x) when {guard}, do: x\n").repeat(40)
}

#[test]
fn highlights_like_lumis_highlight() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../samples/elixir.ex");
    let source = std::fs::read_to_string(path).unwrap();
    let expected = assert_cmd::Command::new(env!("CARGO_BIN_EXE_lumis"))
        .env(
            "LUMIS_CONFIG",
            common::source_fixtures_dir().join("missing-config.toml"),
        )
        .env("LUMIS_DATA_DIR", common::data_dir())
        .args([
            "highlight",
            "-f",
            "html-linked",
            "--budget-time-limit",
            "0",
            path,
        ])
        .output()
        .unwrap();
    assert!(expected.status.success());

    let mut serve = Serve::start(&[]);
    let reply = serve.highlight("lib/elixir.ex", &source);

    assert_eq!(reply.status, 0, "{reply:?}");
    assert!(reply.max_rss_kb > 0);
    assert_eq!(reply.text(), String::from_utf8(expected.stdout).unwrap());
    assert!(reply
        .text()
        .contains(r#"<span class="l-keyword-function">defmodule</span>"#));
}

#[test]
fn highlights_lines_as_fragments() {
    let mut serve = Serve::start(&[]);
    let reply = serve.request(&Request::lines("a.ex", "value = <b>\n\"two\nlines\"\n\n"));

    assert_eq!(reply.status, 0, "{reply:?}");
    let lines = reply.lines();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].starts_with(r#"<span class="l-variable">value</span>"#));
    assert!(lines[0].contains("&lt;") && lines[0].contains("&gt;"));
    assert!(lines[1].starts_with(r#"<span class="l-string">"#));
    assert!(lines[2].starts_with(r#"<span class="l-string">lines"#));
    assert_eq!(lines[3], "");
    assert!(lines.iter().all(|line| !line.contains("<pre")));
}

#[test]
fn answers_requests_in_order() {
    let mut serve = Serve::start(&["--preload-installed"]);
    serve.send(&Request::document("a.ex", ":one").encode());
    serve.send(&Request::document("a.json", "[2]").encode());
    serve.send(&Request::document("a.ex", ":three").encode());

    assert!(serve.reply().text().contains(":one"));
    assert!(serve.reply().text().contains("language-json"));
    assert!(serve.reply().text().contains(":three"));
}

#[test]
fn renders_a_language_it_does_not_know_as_plain_text() {
    let mut serve = Serve::start(&[]);
    let reply = serve.highlight("notes.unknown-extension", "x < y\n");

    assert_eq!(reply.status, 0, "{reply:?}");
    assert!(reply.text().contains("language-plaintext"));
    assert!(reply.text().contains("x &lt; y"));
}

#[test]
fn renders_a_language_that_is_not_installed_as_plain_text() {
    let mut serve = Serve::start(&[]);
    let reply = serve.highlight("src/app.erl", "-module(app).\n");

    assert_eq!(reply.status, 0, "{reply:?}");
    assert!(reply.text().contains("language-erlang"));
    assert!(reply.text().contains("-module(app)."));
    assert!(!reply.text().contains("l-keyword"));
    assert_eq!(serve.highlight("a.ex", ":ok").status, 0);
}

#[test]
fn marks_a_document_the_time_limit_stopped() {
    let mut serve = Serve::start(&["--preload-installed"]);
    let source = slow_elixir(200);
    let reply = serve.request(&Request {
        time_limit_ms: 1,
        ..Request::document("a.ex", &source)
    });

    assert_eq!(reply.status, 0, "{reply:?}");
    assert!(reply.text().contains(r#"data-lumis-budget="time""#));
    assert!(!reply.text().contains("l-keyword"));
}

#[test]
fn answers_a_malformed_request_with_an_error() {
    let mut serve = Serve::start(&[]);

    serve.send(&[1, 0]);
    let reply = serve.reply();
    assert_eq!(reply.status, 1);
    assert_eq!(reply.text(), "malformed request");

    serve.send(&[9]);
    assert_eq!(serve.reply().text(), "unknown request kind 9");

    let mut frame = Request::document("x", "").encode();
    frame.push(0xff);
    serve.send(&frame);
    assert_eq!(serve.reply().text(), "source is not UTF-8");

    assert_eq!(serve.highlight("a.ex", ":ok").status, 0);
}

#[test]
fn exits_when_stdin_closes() {
    let mut serve = Serve::start(&[]);
    serve.close_stdin();

    assert_eq!(serve.wait().code(), Some(0));
}

#[test]
fn exits_when_stdin_closes_during_a_highlight() {
    let mut serve = Serve::start(&["--preload-installed"]);
    serve.send(&Request::document("a.ex", &slow_elixir(2000)).encode());
    std::thread::sleep(Duration::from_millis(300));
    let closed = Instant::now();
    serve.close_stdin();

    assert_eq!(serve.wait().code(), Some(0));
    assert!(
        closed.elapsed() < Duration::from_secs(1),
        "exited {:?} after stdin closed",
        closed.elapsed()
    );
}

#[test]
fn cpu_limit_kills_a_long_highlight() {
    let mut serve = Serve::start(&["--preload-installed"]);
    let started = Instant::now();
    serve.send(
        &Request {
            cpu_limit_ms: 1000,
            ..Request::document("a.ex", &slow_elixir(20_000))
        }
        .encode(),
    );

    assert_eq!(serve.wait().signal(), Some(libc::SIGXCPU));
    // RLIMIT_CPU counts whole seconds, so a 1 s limit can take up to 2 s of
    // CPU, and the kernel checks it on a timer tick.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "killed after {:?}",
        started.elapsed()
    );
}

#[test]
fn cpu_limit_counts_from_the_start_of_each_request() {
    let mut serve = Serve::start(&["--preload-installed"]);
    let source = slow_elixir(200);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(2500) {
        assert_eq!(serve.highlight("a.ex", &source).status, 0);
    }

    let reply = serve.request(&Request {
        cpu_limit_ms: 1000,
        ..Request::document("a.ex", ":ok")
    });
    assert_eq!(reply.status, 0);
    serve.close_stdin();

    let (status, cpu_time) = serve.wait_with_cpu_time();
    assert_eq!(status.code(), Some(0));
    assert!(
        cpu_time > Duration::from_millis(1500),
        "used {cpu_time:?} of CPU before the limited request"
    );
}

#[test]
fn preload_installed_loads_every_installed_language() {
    let mut serve = Serve::start(&["--preload-installed"]);
    let preloaded = serve.highlight("a.ex", ":ok").max_rss_kb;

    let mut cold = Serve::start(&[]);
    let not_preloaded = cold.highlight("a.ex", ":ok").max_rss_kb;

    assert!(
        preloaded > not_preloaded,
        "{preloaded} KB preloaded, {not_preloaded} KB without"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn max_data_mb_fails_allocations_past_the_limit() {
    // Room to compile the JSON parser, not to parse this much nesting.
    let mut serve = Serve::start(&["--max-data-mb", "256"]);
    assert_eq!(serve.highlight("a.json", "[1]").status, 0);

    serve.send(&Request::document("a.json", &"[".repeat(16_000_000)).encode());

    // SIGABRT, from the allocation that failed.
    assert_eq!(serve.wait().signal(), Some(libc::SIGABRT));
}
