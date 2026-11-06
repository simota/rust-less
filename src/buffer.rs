//! Lazy, incremental line access to a single input source (file or stdin).
//!
//! Reads are performed on demand so that opening a large file does not block
//! the first screen draw (`startup-large-file`, 5000-line fixture): only as
//! many lines as a caller actually asks for are ever read from disk.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};

pub enum Source {
    File(PathBuf),
    /// Read on a background thread and handed over in chunks (see
    /// `StreamReader`): crossterm reads keyboard events from the controlling
    /// tty on Unix, not from `std::io::stdin()`, so piping content over
    /// stdin does not conflict with key handling, and follow mode can ask
    /// "is a new line ready?" without blocking.
    Stdin,
}

/// Capacity of the read buffer wrapped around the source. Larger than
/// `BufReader`'s 8 KiB default to cut read syscalls when a whole file is
/// consumed (`G`, backward search, `-m`/`-M` totals); `read` on a pipe or
/// tty still returns whatever is available, so it never waits to fill it.
const READ_BUF_CAPACITY: usize = 64 * 1024;

/// Chunks the stdin thread may read ahead of the pager (back-pressure:
/// a huge pipe is not slurped into memory before anyone scrolls).
const STREAM_CHANNEL_CHUNKS: usize = 8;

/// A read from a pipe waits for more data only while the producer keeps
/// sending: once it has been idle this long, the read returns what has
/// arrived (a live `kubectl logs -f | rless` shows its first screen
/// instead of hanging; `cat big | rless` never idles, so `G` still reaches
/// its true end).
const STREAM_IDLE: std::time::Duration = std::time::Duration::from_millis(150);

/// How long a stream read may wait for its next chunk.
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// Until the producer has been idle for `STREAM_IDLE`.
    WhileFlowing,
    /// Not at all.
    No,
}

/// Each cached line starts at a multiple of this offset in the byte store.
/// Word-aligned line starts keep `str::from_utf8` (run on every line by
/// search and filters) on its aligned word-at-a-time path; unaligned starts
/// measurably slowed line scans. Costs 3.5 padding bytes per line on
/// average, far below the per-line allocation overhead this layout replaces.
const LINE_ALIGN: usize = 8;

/// What a follow-mode poll found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Growth {
    /// Nothing new.
    None,
    /// New lines were appended.
    Appended,
    /// The file was truncated or replaced (log rotation) and has been
    /// re-read from the start: every line number may now mean another line.
    Reset,
}

enum ReadOutcome {
    /// One line (with its newline, if it had one) was appended.
    Line,
    /// The input has ended.
    Eof,
    /// No complete line is available yet (pipes, and files in follow mode).
    Pending,
}

/// Stdin, read on a background thread in chunks.
struct StreamReader {
    rx: Receiver<Result<Vec<u8>, String>>,
    chunk: Vec<u8>,
    pos: usize,
    /// `chunk[pos..scanned]` is known to hold no newline.
    scanned: usize,
    closed: bool,
}

impl StreamReader {
    fn spawn() -> Self {
        let (tx, rx) = mpsc::sync_channel(STREAM_CHANNEL_CHUNKS);
        std::thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                let mut buf = vec![0u8; READ_BUF_CAPACITY];
                let msg = match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(describe_io_error(&e)),
                };
                let failed = msg.is_err();
                if tx.send(msg).is_err() || failed {
                    break;
                }
            }
        });
        StreamReader {
            rx,
            chunk: Vec::new(),
            pos: 0,
            scanned: 0,
            closed: false,
        }
    }

    /// Appends the next line to `out`, waiting for data as `wait` allows;
    /// returns `Pending` when no complete line has arrived in time (a
    /// partial line stays here until its newline does).
    fn read_line(&mut self, out: &mut Vec<u8>, wait: Wait) -> Result<ReadOutcome, String> {
        loop {
            let from = self.scanned.max(self.pos);
            if let Some(i) = self.chunk[from..].iter().position(|&b| b == b'\n') {
                let end = from + i + 1;
                out.extend_from_slice(&self.chunk[self.pos..end]);
                self.pos = end;
                self.scanned = end;
                return Ok(ReadOutcome::Line);
            }
            self.scanned = self.chunk.len();
            if self.closed {
                if self.pos == self.chunk.len() {
                    return Ok(ReadOutcome::Eof);
                }
                out.extend_from_slice(&self.chunk[self.pos..]);
                self.pos = self.chunk.len();
                return Ok(ReadOutcome::Line);
            }
            let next = match wait {
                Wait::WhileFlowing => match self.rx.recv_timeout(STREAM_IDLE) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => return Ok(ReadOutcome::Pending),
                    Err(RecvTimeoutError::Disconnected) => None,
                },
                Wait::No => match self.rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => return Ok(ReadOutcome::Pending),
                    Err(TryRecvError::Disconnected) => None,
                },
            };
            match next {
                Some(Ok(c)) => {
                    self.chunk.drain(..self.pos);
                    self.scanned -= self.pos;
                    self.pos = 0;
                    self.chunk.extend_from_slice(&c);
                }
                Some(Err(e)) => return Err(e),
                None => self.closed = true,
            }
        }
    }
}

enum Reader {
    File {
        inner: BufReader<File>,
        /// (device, inode) of the opened file, to notice log rotation.
        identity: Option<(u64, u64)>,
    },
    Stream(StreamReader),
}

/// One input file (or stdin), with lines read incrementally and cached.
///
/// Cached lines live in one contiguous byte store (newlines stripped, each
/// line starting at a `LINE_ALIGN` boundary) indexed by per-line end
/// offsets, rather than one heap allocation per line: this removes a
/// 24-byte `Vec` header, an allocator header and growth slack per line, and
/// lets `read_until` append straight into the store with no intermediate
/// copy.
pub struct Buffer {
    source: Source,
    reader: Option<Reader>,
    open_error: Option<String>,
    /// Content of every cached line, newlines excluded, with zero padding
    /// between lines up to the next `LINE_ALIGN` boundary.
    data: Vec<u8>,
    /// `line_ends[n - 1]` is the end offset in `data` of line N (1-indexed);
    /// line N starts at the previous line's end rounded up to `LINE_ALIGN`
    /// (line 1 starts at 0).
    line_ends: Vec<usize>,
    /// Cumulative byte count through the end of line N (1-indexed), including
    /// that line's trailing newline. `byte_totals[0]` is unused (index 0
    /// slot kept absent; use `byte_totals.get(n - 1)`).
    byte_totals: Vec<usize>,
    eof: bool,
    /// Follow mode: never block, and hold back a line until its newline
    /// arrives (it is still being written).
    follow: bool,
    /// Bytes of a file's unfinished last line, held back in follow mode.
    partial: Vec<u8>,
}

impl Buffer {
    pub fn new(source: Source) -> Self {
        Buffer {
            source,
            reader: None,
            open_error: None,
            data: Vec::new(),
            line_ends: Vec::new(),
            byte_totals: Vec::new(),
            eof: false,
            follow: false,
            partial: Vec::new(),
        }
    }

    pub fn display_name(&self) -> Option<String> {
        match &self.source {
            Source::File(p) => Some(p.display().to_string()),
            Source::Stdin => None,
        }
    }

    pub fn is_stream(&self) -> bool {
        matches!(self.source, Source::Stdin)
    }

    fn ensure_open(&mut self) -> Result<(), String> {
        if self.reader.is_some() || self.open_error.is_some() {
            return match &self.open_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            };
        }
        let reader = match &self.source {
            Source::File(path) => match File::open(path) {
                Ok(f) => Reader::File {
                    identity: file_identity(&f),
                    inner: BufReader::with_capacity(READ_BUF_CAPACITY, f),
                },
                Err(e) => {
                    let msg = format!("{}: {}", path.display(), describe_io_error(&e));
                    self.open_error = Some(msg.clone());
                    return Err(msg);
                }
            },
            Source::Stdin => Reader::Stream(StreamReader::spawn()),
        };
        self.reader = Some(reader);
        Ok(())
    }

    /// Reads lines until at least `up_to` lines are cached, or EOF. A pipe
    /// may stop short of both when its producer is idle (see
    /// `STREAM_IDLE`); in follow mode nothing waits at all.
    pub fn ensure_lines_read(&mut self, up_to: usize) -> Result<(), String> {
        let wait = if self.follow {
            Wait::No
        } else {
            Wait::WhileFlowing
        };
        self.read_lines(up_to, wait)
    }

    /// Reads, without waiting, the lines up to `up_to` that have already
    /// arrived. True when any line was added.
    pub fn read_ready(&mut self, up_to: usize) -> Result<bool, String> {
        let before = self.line_ends.len();
        self.read_lines(up_to, Wait::No)?;
        Ok(self.line_ends.len() > before)
    }

    /// True for a pipe that may still deliver more lines.
    pub fn is_open_stream(&self) -> bool {
        self.is_stream() && !self.eof && self.open_error.is_none()
    }

    fn read_lines(&mut self, up_to: usize, wait: Wait) -> Result<(), String> {
        self.ensure_open()?;
        if self.eof || self.line_ends.len() >= up_to {
            return Ok(());
        }
        let hold_partial = self.follow;
        let reader = self.reader.as_mut().expect("opened above");
        while self.line_ends.len() < up_to {
            let prev_end = self.data.len();
            let start = line_start(prev_end);
            self.data.resize(start, 0);
            self.data.append(&mut self.partial);
            let outcome = match reader {
                Reader::File { inner, .. } => match inner.read_until(b'\n', &mut self.data) {
                    Ok(_) if self.data.len() > start => Ok(ReadOutcome::Line),
                    Ok(_) => Ok(ReadOutcome::Eof),
                    Err(e) => Err(describe_io_error(&e)),
                },
                Reader::Stream(stream) => stream.read_line(&mut self.data, wait),
            };
            let outcome = match outcome {
                Ok(o) => o,
                Err(e) => {
                    // read_until keeps whatever it read before failing;
                    // drop that partial line, as the per-line buffer used to.
                    self.data.truncate(prev_end);
                    return Err(e);
                }
            };
            match outcome {
                ReadOutcome::Eof => {
                    self.data.truncate(prev_end);
                    self.eof = true;
                    break;
                }
                ReadOutcome::Pending => {
                    self.data.truncate(prev_end);
                    break;
                }
                ReadOutcome::Line => {}
            }
            let had_newline = self.data.last() == Some(&b'\n');
            if !had_newline && hold_partial && matches!(reader, Reader::File { .. }) {
                // Still being written: keep it back until its newline comes.
                self.partial = self.data.split_off(start);
                self.data.truncate(prev_end);
                break;
            }
            if had_newline {
                self.data.pop();
            }
            let len = self.data.len() - start;
            let prev_total = self.byte_totals.last().copied().unwrap_or(0);
            self.byte_totals
                .push(prev_total + len + if had_newline { 1 } else { 0 });
            self.line_ends.push(self.data.len());
        }
        Ok(())
    }

    /// Reads the entire remaining input. Used only when a true-EOF-dependent
    /// answer is required (absolute goto beyond a known line, `G` with no
    /// count, or window-clamping near the tail) — see §3 item 3 of the spec.
    /// On a pipe whose producer is idle, and in follow mode, "entire"
    /// means what has arrived.
    pub fn ensure_full_read(&mut self) -> Result<(), String> {
        self.ensure_open()?;
        while !self.eof {
            let before = self.line_ends.len();
            self.ensure_lines_read(before + 4096)?;
            if self.line_ends.len() == before && !self.eof {
                break;
            }
        }
        Ok(())
    }

    /// Switches follow mode on or off. On: reads never block, and an
    /// unfinished last line is taken back so its continuation joins it.
    pub fn set_follow(&mut self, on: bool) {
        if on == self.follow {
            return;
        }
        self.follow = on;
        if on {
            self.take_back_partial_last_line();
            self.eof = false;
        } else if !self.partial.is_empty() {
            // Show the unfinished line again, as a line without newline.
            let start = line_start(self.data.len());
            self.data.resize(start, 0);
            self.data.append(&mut self.partial);
            let len = self.data.len() - start;
            let prev_total = self.byte_totals.last().copied().unwrap_or(0);
            self.byte_totals.push(prev_total + len);
            self.line_ends.push(self.data.len());
        }
    }

    fn take_back_partial_last_line(&mut self) {
        let n = self.line_ends.len();
        if n == 0 || !matches!(self.source, Source::File(_)) {
            return;
        }
        let len = self.line(n).map_or(0, <[u8]>::len);
        let before = if n >= 2 { self.byte_totals[n - 2] } else { 0 };
        if self.byte_totals[n - 1] - before == len {
            // No newline: the file's last line may still be growing.
            let start = if n == 1 {
                0
            } else {
                line_start(self.line_ends[n - 2])
            };
            self.partial = self.data.split_off(start);
            self.data
                .truncate(if n == 1 { 0 } else { self.line_ends[n - 2] });
            self.line_ends.pop();
            self.byte_totals.pop();
        }
    }

    /// Follow mode: picks up whatever was appended since the last read. A
    /// file that shrank or was replaced at its path (log rotation) is
    /// re-opened and re-read from the start.
    pub fn poll_growth(&mut self) -> Result<Growth, String> {
        self.ensure_open()?;
        let mut reset = false;
        if let (Source::File(path), Some(Reader::File { identity, .. })) =
            (&self.source, &self.reader)
            && let Ok(meta) = std::fs::metadata(path)
        {
            let consumed = self.byte_totals.last().copied().unwrap_or(0) + self.partial.len();
            let replaced = identity.is_some() && metadata_identity(&meta) != *identity;
            reset = replaced || (meta.len() as usize) < consumed;
        }
        if reset {
            self.reader = None;
            self.data.clear();
            self.line_ends.clear();
            self.byte_totals.clear();
            self.partial.clear();
            self.ensure_open()?;
        }
        let before = self.line_ends.len();
        self.eof = false;
        self.ensure_lines_read(usize::MAX)?;
        Ok(if reset {
            Growth::Reset
        } else if self.line_ends.len() > before {
            Growth::Appended
        } else {
            Growth::None
        })
    }

    pub fn line(&self, n: usize) -> Option<&[u8]> {
        if n == 0 {
            return None;
        }
        let end = *self.line_ends.get(n - 1)?;
        let start = if n == 1 {
            0
        } else {
            line_start(self.line_ends[n - 2])
        };
        Some(&self.data[start..end])
    }

    pub fn lines_cached(&self) -> usize {
        self.line_ends.len()
    }

    pub fn is_eof(&self) -> bool {
        self.eof
    }

    pub fn byte_offset_through(&self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        self.byte_totals
            .get(n - 1)
            .copied()
            .unwrap_or_else(|| self.byte_totals.last().copied().unwrap_or(0))
    }

    pub fn total_bytes(&mut self) -> Result<usize, String> {
        self.ensure_full_read()?;
        Ok(self.byte_totals.last().copied().unwrap_or(0))
    }

    pub fn total_lines(&mut self) -> Result<usize, String> {
        self.ensure_full_read()?;
        Ok(self.line_ends.len())
    }

    /// Best-effort "does this look like binary content" check, used to gate
    /// the `"<file>" may be a binary file.  See it anyway?` prompt when `-R`
    /// is not given. Evidence-derived rule (no less source consulted): the
    /// only byte that distinguishes a flagged fixture (`ansi.txt`, several
    /// raw ESC bytes) from an unflagged one containing other control bytes
    /// (`tabs-ctrl.txt`: \x01 \x02 \x07 \x1f, never flagged) is the presence
    /// of ESC (0x1b) — see `opt-raw-control-chars-without-R` vs
    /// `default-tabs-and-control-chars` in parity/baseline/.
    pub fn sniff_binary(&mut self) -> Result<bool, String> {
        self.ensure_lines_read(64)?;
        // `data` holds the cached lines' bytes (newlines excluded) plus zero
        // padding, which can never be ESC.
        Ok(self.data.contains(&0x1b))
    }
}

#[cfg(unix)]
fn metadata_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn metadata_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn file_identity(f: &File) -> Option<(u64, u64)> {
    f.metadata().ok().as_ref().and_then(metadata_identity)
}

/// Offset at which the line following one that ends at `prev_end` starts.
#[inline]
fn line_start(prev_end: usize) -> usize {
    prev_end.next_multiple_of(LINE_ALIGN)
}

fn describe_io_error(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "No such file or directory".to_string(),
        io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(contents: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rust-less-test-{}-{}.txt",
            std::process::id(),
            rand_suffix()
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        path
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        nanos.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn reads_lines_incrementally() {
        let path = temp_file(b"a\nb\nc\n");
        let mut buf = Buffer::new(Source::File(path.clone()));
        buf.ensure_lines_read(2).unwrap();
        assert_eq!(buf.lines_cached(), 2);
        assert_eq!(buf.line(1), Some(&b"a"[..]));
        assert_eq!(buf.line(2), Some(&b"b"[..]));
        assert!(!buf.is_eof());
        buf.ensure_full_read().unwrap();
        assert!(buf.is_eof());
        assert_eq!(buf.lines_cached(), 3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_file_errors() {
        let mut buf = Buffer::new(Source::File(PathBuf::from(
            "/nonexistent/does-not-exist-rust-less",
        )));
        assert!(buf.ensure_lines_read(1).is_err());
    }

    #[test]
    fn sniff_binary_detects_esc_only() {
        let path = temp_file(b"plain text\x01\x02\x07\x1fwith ctrl\n");
        let mut buf = Buffer::new(Source::File(path.clone()));
        assert!(!buf.sniff_binary().unwrap());
        let _ = std::fs::remove_file(path);

        let path2 = temp_file(b"\x1b[31mred\x1b[0m\n");
        let mut buf2 = Buffer::new(Source::File(path2.clone()));
        assert!(buf2.sniff_binary().unwrap());
        let _ = std::fs::remove_file(path2);
    }

    #[test]
    fn line_access_and_offsets_with_edge_content() {
        // Empty lines, CRLF (CR is content), no trailing newline.
        let path = temp_file(b"\n\nab\r\n\x1bz\nlast");
        let mut buf = Buffer::new(Source::File(path.clone()));
        assert_eq!(buf.line(1), None);
        assert_eq!(buf.byte_offset_through(1), 0);
        buf.ensure_lines_read(3).unwrap();
        assert_eq!(buf.line(0), None);
        assert_eq!(buf.line(1), Some(&b""[..]));
        assert_eq!(buf.line(2), Some(&b""[..]));
        assert_eq!(buf.line(3), Some(&b"ab\r"[..]));
        assert_eq!(buf.line(4), None);
        assert_eq!(buf.byte_offset_through(2), 2);
        assert_eq!(buf.byte_offset_through(3), 6);
        assert_eq!(buf.byte_offset_through(99), 6);
        assert_eq!(buf.total_lines().unwrap(), 5);
        assert_eq!(buf.line(4), Some(&b"\x1bz"[..]));
        assert_eq!(buf.line(5), Some(&b"last"[..]));
        assert_eq!(buf.line(6), None);
        assert_eq!(buf.byte_offset_through(4), 9);
        assert_eq!(buf.byte_offset_through(5), 13);
        assert_eq!(buf.byte_offset_through(6), 13);
        assert_eq!(buf.total_bytes().unwrap(), 13);
        assert!(buf.sniff_binary().unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_input_has_no_lines() {
        let path = temp_file(b"");
        let mut buf = Buffer::new(Source::File(path.clone()));
        assert_eq!(buf.total_lines().unwrap(), 0);
        assert_eq!(buf.total_bytes().unwrap(), 0);
        assert_eq!(buf.line(1), None);
        assert!(!buf.sniff_binary().unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn many_lines_of_varied_length_round_trip() {
        let mut content = Vec::new();
        let mut expected: Vec<Vec<u8>> = Vec::new();
        let mut seed: u32 = 12345;
        for i in 0..3000u32 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let len = (seed >> 16) as usize % 40;
            let line: Vec<u8> = (0..len)
                .map(|j| b'a' + ((i as usize + j) % 26) as u8)
                .collect();
            content.extend_from_slice(&line);
            content.push(b'\n');
            expected.push(line);
        }
        content.extend_from_slice(b"tail");
        expected.push(b"tail".to_vec());
        let path = temp_file(&content);
        let mut buf = Buffer::new(Source::File(path.clone()));
        // Mix incremental and full reads.
        buf.ensure_lines_read(17).unwrap();
        buf.ensure_lines_read(1000).unwrap();
        buf.ensure_full_read().unwrap();
        assert_eq!(buf.lines_cached(), expected.len());
        let mut total = 0;
        for (i, line) in expected.iter().enumerate() {
            let n = i + 1;
            assert_eq!(buf.line(n), Some(line.as_slice()), "line {n}");
            total += line.len() + usize::from(n < expected.len());
            assert_eq!(buf.byte_offset_through(n), total, "offset {n}");
        }
        assert_eq!(buf.total_bytes().unwrap(), content.len());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn byte_offset_tracking() {
        let path = temp_file(b"abc\nde\n");
        let mut buf = Buffer::new(Source::File(path.clone()));
        buf.ensure_full_read().unwrap();
        assert_eq!(buf.byte_offset_through(1), 4); // "abc\n"
        assert_eq!(buf.byte_offset_through(2), 7); // "abc\nde\n"
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn follow_mode_holds_an_unfinished_line_and_joins_its_continuation() {
        let path = temp_file(b"one\ntw");
        let mut buf = Buffer::new(Source::File(path.clone()));
        buf.ensure_full_read().unwrap();
        assert_eq!(buf.line(2), Some(&b"tw"[..]), "shown while not following");
        buf.set_follow(true);
        assert_eq!(buf.lines_cached(), 1, "taken back: it may still grow");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"o\nthr").unwrap();
        f.flush().unwrap();
        assert_eq!(buf.poll_growth().unwrap(), Growth::Appended);
        assert_eq!(buf.line(2), Some(&b"two"[..]));
        assert_eq!(buf.lines_cached(), 2, "\"thr\" waits for its newline");
        assert_eq!(buf.poll_growth().unwrap(), Growth::None);
        buf.set_follow(false);
        assert_eq!(
            buf.line(3),
            Some(&b"thr"[..]),
            "shown again once not following"
        );
        assert_eq!(buf.byte_offset_through(3), 11);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn follow_mode_rereads_a_truncated_or_replaced_file() {
        let path = temp_file(b"a\nb\nc\n");
        let mut buf = Buffer::new(Source::File(path.clone()));
        buf.ensure_full_read().unwrap();
        buf.set_follow(true);
        std::fs::write(&path, b"x\n").unwrap();
        assert_eq!(buf.poll_growth().unwrap(), Growth::Reset);
        assert_eq!(buf.lines_cached(), 1);
        assert_eq!(buf.line(1), Some(&b"x"[..]));
        // Replaced by a new file at the same path (rename-style rotation).
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, b"new 1\nnew 2\nnew 3\nnew 4\n").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert_eq!(buf.poll_growth().unwrap(), Growth::Reset);
        assert_eq!(buf.lines_cached(), 4);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stream_reader_waits_for_newlines_and_reports_pending() {
        let (tx, rx) = mpsc::sync_channel(4);
        let mut stream = StreamReader {
            rx,
            chunk: Vec::new(),
            pos: 0,
            scanned: 0,
            closed: false,
        };
        let mut out = Vec::new();
        tx.send(Ok(b"ab".to_vec())).unwrap();
        assert!(matches!(
            stream.read_line(&mut out, Wait::No),
            Ok(ReadOutcome::Pending)
        ));
        assert!(out.is_empty(), "a partial line is not handed out");
        tx.send(Ok(b"c\nd".to_vec())).unwrap();
        assert!(matches!(
            stream.read_line(&mut out, Wait::No),
            Ok(ReadOutcome::Line)
        ));
        assert_eq!(out, b"abc\n");
        out.clear();
        drop(tx);
        assert!(matches!(
            stream.read_line(&mut out, Wait::WhileFlowing),
            Ok(ReadOutcome::Line)
        ));
        assert_eq!(
            out, b"d",
            "the last line needs no newline once the pipe closes"
        );
        assert!(matches!(
            stream.read_line(&mut out, Wait::No),
            Ok(ReadOutcome::Eof)
        ));
    }
}
