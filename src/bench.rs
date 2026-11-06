//! Opt-in micro/macro benchmarks for the hot paths (buffer ingest, window
//! render, full `App::draw`, regex/structured search, regex filter).
//!
//! Not run by `cargo test`; run explicitly in release mode:
//!
//! ```text
//! cargo test --release bench:: -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each benchmark prints min / median / p95 wall time over its iterations
//! and the process peak RSS (`VmHWM`) so far — run one benchmark per process
//! (filter by name) when comparing peak memory.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::buffer::{Buffer, Source};
use crate::cli::Options;
use crate::command::App;
use crate::input::Action;
use crate::screen::{self, RenderOpts};
use crate::search::{self, Direction};

fn fixture(name: &str, build: impl FnOnce(&mut Vec<u8>)) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rust-less-bench-{}-{}", std::process::id(), name));
    let mut data = Vec::new();
    build(&mut data);
    std::fs::write(&path, &data).unwrap();
    path
}

fn plain_lines(n: usize) -> PathBuf {
    fixture(&format!("plain-{n}"), |d| {
        for i in 1..=n {
            writeln!(
                d,
                "{i:07} line {i} of {n} :: lorem ipsum dolor sit amet\tconsectetur ✓ 漢字"
            )
            .unwrap();
        }
    })
}

fn jsonl_lines(n: usize) -> PathBuf {
    fixture(&format!("jsonl-{n}"), |d| {
        for i in 1..=n {
            let level = if i % 1000 == 0 { "ERROR" } else { "INFO" };
            let status = if i % 1000 == 0 { 500 } else { 200 };
            writeln!(
                d,
                r#"{{"ts":"2026-08-23T00:{:02}:00Z","level":"{level}","status":{status},"service":"svc{}","latency_ms":{},"msg":"request handled"}}"#,
                i % 60,
                i % 7,
                i % 400
            )
            .unwrap();
        }
    })
}

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let pct = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
    println!(
        "BENCH {name:<34} n={:<4} min={:>10.3?} p50={:>10.3?} p95={:>10.3?} peak_rss={}KB",
        samples.len(),
        samples[0],
        pct(0.5),
        pct(0.95),
        peak_rss_kb()
    );
}

fn run(name: &str, iters: usize, mut f: impl FnMut()) {
    f(); // warm-up (page cache, allocator)
    let samples = (0..iters)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    report(name, samples);
}

fn opts(files: Vec<PathBuf>) -> Options {
    Options {
        files,
        ..Options::default()
    }
}

/// Counts `write` calls reaching the underlying sink, as a stand-in for
/// terminal write syscalls when wrapped the way `main` wraps stdout.
struct CountingSink {
    writes: usize,
    bytes: usize,
}

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        self.bytes += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
#[ignore]
fn bench_buffer_full_read_1m_lines() {
    let path = plain_lines(1_000_000);
    run("buffer_full_read_1m", 5, || {
        let mut b = Buffer::new(Source::File(path.clone()));
        b.ensure_full_read().unwrap();
        assert_eq!(b.lines_cached(), 1_000_000);
    });
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_render_window_plain() {
    let path = plain_lines(10_000);
    let mut b = Buffer::new(Source::File(path.clone()));
    b.ensure_full_read().unwrap();
    let o = RenderOpts {
        width: 80,
        line_numbers: true,
        chop: false,
        raw_ansi: false,
        hshift: 0,
    };
    let mut top = 1;
    run("render_window_plain_x1000", 20, || {
        for _ in 0..1000 {
            let w = screen::render_window(&mut b, top, 23, &o, None).unwrap();
            assert!(!w.rows.is_empty());
            top = top % 9000 + 1;
        }
    });
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_render_window_huge_single_line() {
    let path = fixture("huge-line", |d| {
        d.extend(std::iter::repeat_n(b'x', 4_000_000));
        d.push(b'\n');
    });
    let mut b = Buffer::new(Source::File(path.clone()));
    let o = RenderOpts {
        width: 80,
        line_numbers: false,
        chop: false,
        raw_ansi: false,
        hshift: 0,
    };
    run("render_window_4mb_line", 10, || {
        let w = screen::render_window(&mut b, 1, 23, &o, None).unwrap();
        assert_eq!(w.rows.len(), 23);
    });
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_draw_scroll_plain() {
    let path = plain_lines(10_000);
    let mut app = App::new(opts(vec![path.clone()]));
    app.set_size(160, 48);
    let mut sink = std::io::LineWriter::new(CountingSink {
        writes: 0,
        bytes: 0,
    });
    let mut draws = 0usize;
    run("draw_scroll_plain_x500", 20, || {
        for _ in 0..500 {
            app.bench_apply(Action::ScrollLines(1));
            app.bench_draw(&mut sink);
            draws += 1;
        }
    });
    let s = sink.get_ref();
    println!(
        "      writes/draw={:.2} bytes/draw={:.0}",
        s.writes as f64 / draws as f64,
        s.bytes as f64 / draws as f64
    );
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_search_forward_regex() {
    let path = plain_lines(1_000_000);
    let mut b = Buffer::new(Source::File(path.clone()));
    b.ensure_full_read().unwrap();
    let re = regex::Regex::new("line 999999 of").unwrap();
    run("search_forward_regex_1m", 5, || {
        let f = search::find(&mut b, &re, Direction::Forward, 1, true, 1).unwrap();
        assert_eq!(f, Some(999_999));
    });
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_search_backward_near_top() {
    let path = plain_lines(1_000_000);
    let re = regex::Regex::new("line 3 of").unwrap();
    // Fresh buffer each time: measures what a `?` near the top of a
    // not-yet-fully-read file costs (cold, lazily-read buffer).
    run("search_backward_near_top_1m", 5, || {
        let mut b = Buffer::new(Source::File(path.clone()));
        let f = search::find(&mut b, &re, Direction::Backward, 23, true, 1).unwrap();
        assert_eq!(f, Some(3));
    });
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_filter_regex_draw() {
    let path = jsonl_lines(200_000);
    let mut app = App::new(opts(vec![path.clone()]));
    app.set_size(160, 24);
    let mut sink: Vec<u8> = Vec::new();
    app.bench_draw(&mut sink); // sniff -> jsonl
    // Regex (non-structured) filter: matches ~1 in 1000 lines.
    app.bench_apply(Action::FilterInput("ERROR".to_string()));
    run("filter_regex_draw_200k", 5, || {
        sink.clear();
        app.bench_draw(&mut sink);
    });
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_structured_search() {
    let path = jsonl_lines(200_000);
    let mut app = App::new(opts(vec![path.clone()]));
    app.set_size(160, 24);
    let mut sink: Vec<u8> = Vec::new();
    app.bench_draw(&mut sink); // sniff -> jsonl
    run("structured_search_200k", 5, || {
        app.bench_apply(Action::GotoLine(Some(1)));
        app.bench_apply(Action::SearchInput(
            "/.level == \"NOPE\"".to_string(),
            Direction::Forward,
            1,
        ));
    });
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore]
fn bench_tabular_draw() {
    let path = jsonl_lines(10_000);
    let mut app = App::new(opts(vec![path.clone()]));
    app.set_size(200, 48);
    let mut sink: Vec<u8> = Vec::new();
    app.bench_draw(&mut sink); // sniff -> jsonl, columns from the sample
    app.bench_apply(Action::ToggleTabular);
    run("tabular_draw_x200", 20, || {
        for _ in 0..200 {
            app.bench_apply(Action::ScrollLines(1));
            sink.clear();
            app.bench_draw(&mut sink);
        }
    });
    let _ = std::fs::remove_file(path);
}
