#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

mod common;

struct Serve {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    reaped: bool,
}

#[derive(Debug)]
struct Reply {
    status: u8,
    max_rss_kb: u32,
    missing: Vec<String>,
    body: String,
}

impl Serve {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lumis"))
            .arg("--data-dir")
            .arg(common::data_dir())
            .arg("serve")
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
        }
    }

    fn send(&mut self, frame: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        let size = u32::try_from(frame.len()).unwrap();
        stdin.write_all(&size.to_be_bytes()).unwrap();
        stdin.write_all(frame).unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, language: &str, source: &str, cpu_limit_ms: u32) {
        self.send(&highlight_request(language, source, cpu_limit_ms));
    }

    fn highlight(&mut self, language: &str, source: &str) -> Reply {
        self.request(language, source, 0);
        self.reply()
    }

    fn reply(&mut self) -> Reply {
        let mut size = [0; 4];
        self.stdout.read_exact(&mut size).unwrap();
        let mut frame = vec![0; u32::from_be_bytes(size) as usize];
        self.stdout.read_exact(&mut frame).unwrap();

        let (&status, rest) = frame.split_first().unwrap();
        let (max_rss_kb, rest) = rest.split_first_chunk::<4>().unwrap();
        let (count, mut rest) = rest.split_first_chunk::<2>().unwrap();
        let mut missing = Vec::new();
        for _ in 0..u16::from_be_bytes(*count) {
            let (size, tail) = rest.split_first_chunk::<2>().unwrap();
            let (name, tail) = tail.split_at(usize::from(u16::from_be_bytes(*size)));
            missing.push(String::from_utf8(name.to_vec()).unwrap());
            rest = tail;
        }

        Reply {
            status,
            max_rss_kb: u32::from_be_bytes(*max_rss_kb),
            missing,
            body: String::from_utf8(rest.to_vec()).unwrap(),
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

    fn stderr(&mut self) -> String {
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        stderr
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

fn highlight_request(language: &str, source: &str, cpu_limit_ms: u32) -> Vec<u8> {
    let mut frame = vec![1];
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame.extend_from_slice(&cpu_limit_ms.to_be_bytes());
    frame.extend_from_slice(&u16::try_from(language.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(language.as_bytes());
    frame.extend_from_slice(source.as_bytes());
    frame
}

/// A guard whose `or` chain the Elixir locals query matches in quadratic time.
fn slow_elixir(terms: usize) -> String {
    let guard = (0..terms)
        .map(|term| format!("x === {term}"))
        .collect::<Vec<_>>()
        .join(" or ");
    format!("def f(x) when {guard}, do: x\n")
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
        .args(["highlight", "-f", "html-linked", path])
        .output()
        .unwrap();
    assert!(expected.status.success());

    let mut serve = Serve::start(&[]);
    let reply = serve.highlight("lib/elixir.ex", &source);

    assert_eq!(reply.status, 0, "{reply:?}");
    assert!(reply.missing.is_empty());
    assert!(reply.max_rss_kb > 0);
    assert_eq!(reply.body, String::from_utf8(expected.stdout).unwrap());
    assert!(reply
        .body
        .contains(r#"<span class="l-keyword-function">defmodule</span>"#));
}

#[test]
fn answers_requests_in_order() {
    let mut serve = Serve::start(&["--preload", "elixir,json"]);
    serve.request("a.ex", ":one", 0);
    serve.request("a.json", "[2]", 0);
    serve.request("a.ex", ":three", 0);

    assert!(serve.reply().body.contains(":one"));
    assert!(serve.reply().body.contains("language-json"));
    assert!(serve.reply().body.contains(":three"));
}

#[test]
fn escapes_a_language_it_does_not_know_as_plain_text() {
    let mut serve = Serve::start(&[]);
    let reply = serve.highlight("notes.unknown-extension", "x < y\n");

    assert_eq!(reply.status, 0, "{reply:?}");
    assert!(reply.body.contains("language-plaintext"));
    assert!(reply.body.contains("x &lt; y"));
}

#[test]
fn reports_a_language_that_is_not_cached() {
    let mut serve = Serve::start(&[]);
    let reply = serve.highlight("src/app.erl", "-module(app).\n");

    assert_eq!(reply.status, 2, "{reply:?}");
    assert_eq!(reply.missing, ["erlang"]);
    assert_eq!(reply.body, "");
    assert_eq!(serve.highlight("a.ex", ":ok").status, 0);
}

#[test]
fn lists_injected_languages_that_are_not_cached() {
    let mut serve = Serve::start(&[]);
    let reply = serve.highlight(
        "README.md",
        "# Title\n\n```haskell\nmain = pure ()\n```\n\n```elixir\n:ok\n```\n",
    );

    assert_eq!(reply.status, 0, "{reply:?}");
    assert_eq!(reply.missing, ["haskell"]);
    assert!(reply.body.contains("main = pure ()"));
    assert!(reply
        .body
        .contains(r#"<span class="l-string-special-symbol">"#));
}

#[test]
fn answers_a_malformed_request_with_an_error() {
    let mut serve = Serve::start(&[]);

    serve.send(&[1, 0]);
    let reply = serve.reply();
    assert_eq!(reply.status, 1);
    assert_eq!(reply.body, "malformed request");

    serve.send(&[9]);
    assert_eq!(serve.reply().body, "unknown request type 9");

    serve.send(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, b'x', 0xff]);
    assert_eq!(serve.reply().body, "source is not UTF-8");

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
    let mut serve = Serve::start(&["--preload", "elixir"]);
    assert_eq!(serve.highlight("a.ex", ":ok").status, 0);

    serve.request("a.ex", &slow_elixir(400), 0);
    std::thread::sleep(Duration::from_millis(500));
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
    let mut serve = Serve::start(&["--preload", "elixir"]);
    assert_eq!(serve.highlight("a.ex", ":ok").status, 0);

    let started = Instant::now();
    serve.request("a.ex", &slow_elixir(400), 1000);

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
    let mut serve = Serve::start(&["--preload", "elixir"]);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(2500) {
        assert_eq!(serve.highlight("a.ex", &slow_elixir(60)).status, 0);
    }

    serve.request("a.ex", ":ok", 1000);
    assert_eq!(serve.reply().status, 0);
    serve.close_stdin();

    let (status, cpu_time) = serve.wait_with_cpu_time();
    assert_eq!(status.code(), Some(0));
    assert!(
        cpu_time > Duration::from_millis(1500),
        "used {cpu_time:?} of CPU before the limited request"
    );
}

#[test]
fn preload_skips_a_language_that_is_not_cached() {
    let mut serve = Serve::start(&["--preload", "erlang,elixir"]);
    assert_eq!(serve.highlight("a.ex", ":ok").status, 0);
    serve.close_stdin();

    assert_eq!(serve.wait().code(), Some(0));
    assert!(serve.stderr().contains("could not preload erlang"));
}
