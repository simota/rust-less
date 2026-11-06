//! Lazy, incremental line access to a single input source (file or stdin).
//!
//! Reads are performed on demand so that opening a large file does not block
//! the first screen draw (`startup-large-file`, 5000-line fixture): only as
//! many lines as a caller actually asks for are ever read from disk.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

pub enum Source {
    File(PathBuf),
    /// Read through a `StdinPump`: crossterm reads keyboard events from the
    /// controlling tty on Unix, not from `std::io::stdin()`, so piping
    /// content over stdin does not conflict with key handling.
    Stdin,
    /// Content held in memory (the help page, a sorted snapshot), shown
    /// under `name`.
    Memory {
        name: Option<String>,
        bytes: Vec<u8>,
    },
}

/// Reads stdin on a helper thread, one chunk per request. A blocking read
/// (`Read::read`) asks for a chunk and waits for it, exactly like reading
/// stdin directly, so input is still pulled lazily and never buffered
/// ahead of demand beyond one chunk. `poll_ready` asks without waiting,
/// which is what lets follow mode (`F`) watch a pipe that is still open
/// (`tail -f log | rless`) while the keyboard stays responsive.
struct StdinPump {
    requests: Sender<()>,
    chunks: Receiver<io::Result<Vec<u8>>>,
    in_flight: bool,
    stash: Vec<u8>,
    stash_pos: usize,
    /// The reader thread reported end of input (or is gone).
    finished: bool,
}

impl StdinPump {
    fn spawn() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<()>();
        let (chunk_tx, chunk_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdin = io::stdin();
            while req_rx.recv().is_ok() {
                let mut buf = vec![0u8; READ_BUF_CAPACITY];
                let result = stdin.read(&mut buf).map(|n| {
                    buf.truncate(n);
                    buf
                });
                let done = !matches!(&result, Ok(b) if !b.is_empty());
                if chunk_tx.send(result).is_err() || done {
                    break;
                }
            }
        });
        StdinPump {
            requests: req_tx,
            chunks: chunk_rx,
            in_flight: false,
            stash: Vec::new(),
            stash_pos: 0,
            finished: false,
        }
    }

    fn accept(&mut self, chunk: io::Result<Vec<u8>>) -> io::Result<()> {
        self.in_flight = false;
        let chunk = chunk?;
        if chunk.is_empty() {
            self.finished = true;
        }
        self.stash = chunk;
        self.stash_pos = 0;
        Ok(())
    }

    fn request(&mut self) {
        if !self.in_flight && !self.finished {
            self.in_flight = self.requests.send(()).is_ok();
            if !self.in_flight {
                self.finished = true;
            }
        }
    }

    /// True when a `read` would return at once (data or end of input).
    fn poll_ready(&mut self) -> bool {
        if self.stash_pos < self.stash.len() || self.finished {
            return true;
        }
        self.request();
        match self.chunks.try_recv() {
            Ok(chunk) => {
                // A read error ends the input, as end of file does.
                if self.accept(chunk).is_err() {
                    self.finished = true;
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                true
            }
        }
    }
}

impl Read for StdinPump {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.stash_pos >= self.stash.len() {
            if self.finished {
                return Ok(0);
            }
            self.request();
            if !self.in_flight {
                return Ok(0);
            }
            match self.chunks.recv() {
                Ok(chunk) => self.accept(chunk)?,
                Err(_) => {
                    self.finished = true;
                    return Ok(0);
                }
            }
        }
        let available = &self.stash[self.stash_pos..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.stash_pos += n;
        Ok(n)
    }
}

enum Reader {
    File(BufReader<File>),
    Stdin(BufReader<StdinPump>),
    Memory(io::Cursor<Vec<u8>>),
}

impl Reader {
    fn as_bufread(&mut self) -> &mut dyn BufRead {
        match self {
            Reader::File(r) => r,
            Reader::Stdin(r) => r,
            Reader::Memory(r) => r,
        }
    }

    /// Whether reading now would not block: always for files and memory;
    /// for stdin, only when buffered or newly arrived data (or the end of
    /// input) is at hand.
    fn ready(&mut self) -> bool {
        match self {
            Reader::Stdin(r) => !r.buffer().is_empty() || r.get_mut().poll_ready(),
            _ => true,
        }
    }
}

/// Capacity of the read buffer wrapped around the source. Larger than
/// `BufReader`'s 8 KiB default to cut read syscalls when a whole file is
/// consumed (`G`, backward search, `-m`/`-M` totals); `read` on a pipe or
/// tty still returns whatever is available, so it never waits to fill it.
const READ_BUF_CAPACITY: usize = 64 * 1024;

/// Each cached line starts at a multiple of this offset in the byte store.
/// Word-aligned line starts keep `str::from_utf8` (run on every line by
/// search and filters) on its aligned word-at-a-time path; unaligned starts
/// measurably slowed line scans. Costs 3.5 padding bytes per line on
/// average, far below the per-line allocation overhead this layout replaces.
const LINE_ALIGN: usize = 8;

/// Most bytes one follow-mode poll reads before giving the keyboard a turn.
const FOLLOW_READ_BUDGET: usize = 8 * 1024 * 1024;

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
    /// The last cached line ended at end of input without a newline: if
    /// the input grows (follow mode), the new bytes continue that line.
    last_line_open: bool,
    /// While set, `ensure_lines_read` stops at input that has not arrived
    /// yet instead of waiting for it (see `set_nonblocking`).
    nonblocking: bool,
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
            last_line_open: false,
            nonblocking: false,
        }
    }

    /// Drawing a screen sets this so that a pipe whose writer is still
    /// running shows what has arrived instead of a blank screen until a
    /// full window's worth does. Files never wait, so this changes nothing
    /// for them. `ensure_full_read` (`G`, totals) still waits for the end.
    pub fn set_nonblocking(&mut self, on: bool) {
        self.nonblocking = on;
    }

    /// Reads from stdin (the only source that can stall)?
    pub fn is_stdin(&self) -> bool {
        matches!(self.source, Source::Stdin)
    }

    pub fn display_name(&self) -> Option<String> {
        match &self.source {
            Source::File(p) => Some(p.display().to_string()),
            Source::Stdin => None,
            Source::Memory { name, .. } => name.clone(),
        }
    }

    /// The file this buffer reads, if it reads one.
    pub fn path(&self) -> Option<&PathBuf> {
        match &self.source {
            Source::File(p) => Some(p),
            _ => None,
        }
    }

    /// Discards everything read so far, so the file is read afresh (`R`).
    /// Only a file can be read again; returns false for stdin and memory.
    pub fn reload(&mut self) -> bool {
        if !matches!(self.source, Source::File(_)) {
            return false;
        }
        self.reader = None;
        self.open_error = None;
        self.data.clear();
        self.line_ends.clear();
        self.byte_totals.clear();
        self.eof = false;
        self.last_line_open = false;
        true
    }

    fn ensure_open(&mut self) -> Result<(), String> {
        if self.reader.is_some() || self.open_error.is_some() {
            return match &self.open_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            };
        }
        let reader = match &mut self.source {
            Source::File(path) => match File::open(&*path) {
                Ok(f) => Reader::File(BufReader::with_capacity(READ_BUF_CAPACITY, f)),
                Err(e) => {
                    let msg = format!("{}: {}", path.display(), describe_io_error(&e));
                    self.open_error = Some(msg.clone());
                    return Err(msg);
                }
            },
            Source::Stdin => Reader::Stdin(BufReader::with_capacity(
                READ_BUF_CAPACITY,
                StdinPump::spawn(),
            )),
            Source::Memory { bytes, .. } => Reader::Memory(io::Cursor::new(std::mem::take(bytes))),
        };
        self.reader = Some(reader);
        Ok(())
    }

    /// Reads lines until at least `up_to` lines are cached, or EOF.
    pub fn ensure_lines_read(&mut self, up_to: usize) -> Result<(), String> {
        self.ensure_open()?;
        while !self.eof && self.line_ends.len() < up_to {
            if self.nonblocking && !self.reader.as_mut().is_some_and(Reader::ready) {
                break;
            }
            self.read_line()?;
        }
        Ok(())
    }

    /// Like `ensure_lines_read`, but past the first line it reads only
    /// input that is already available: a pipe whose writer is still
    /// running (`tail -f log | rless`) must not hold up the first screen
    /// while the binary check and the format sniff wait for a full sample.
    pub fn ensure_lines_available(&mut self, up_to: usize) -> Result<(), String> {
        self.ensure_lines_read(up_to.min(1))?;
        while !self.eof
            && self.line_ends.len() < up_to
            && self.reader.as_mut().is_some_and(Reader::ready)
        {
            self.read_line()?;
        }
        Ok(())
    }

    /// Reads one more line into the cache, or marks EOF. A line left open
    /// at a previous EOF (no trailing newline yet) is continued rather than
    /// a new one started.
    fn read_line(&mut self) -> Result<(), String> {
        let reader = self.reader.as_mut().expect("opened by caller").as_bufread();
        let reopen = std::mem::take(&mut self.last_line_open);
        let prev_end = self.data.len();
        let start = if reopen {
            // Take the open line back out of the index; its bytes stay at
            // the end of `data` and the new input is appended to them.
            self.line_ends.pop();
            self.byte_totals.pop();
            self.line_ends.last().map_or(0, |&e| line_start(e))
        } else {
            let start = line_start(self.data.len());
            self.data.resize(start, 0);
            start
        };
        let before = self.data.len();
        let n = match reader.read_until(b'\n', &mut self.data) {
            Ok(n) => n,
            Err(e) => {
                // read_until keeps whatever it read before failing; drop
                // that partial input, as the per-line buffer used to.
                if reopen {
                    self.data.truncate(before);
                    self.push_line(start, false);
                } else {
                    self.data.truncate(prev_end);
                }
                return Err(describe_io_error(&e));
            }
        };
        if n == 0 {
            if reopen {
                self.push_line(start, false);
            } else {
                self.data.truncate(prev_end);
            }
            self.eof = true;
            return Ok(());
        }
        let had_newline = self.data.last() == Some(&b'\n');
        if had_newline {
            self.data.pop();
        }
        self.push_line(start, had_newline);
        Ok(())
    }

    /// Indexes the line occupying `data[start..]`.
    fn push_line(&mut self, start: usize, had_newline: bool) {
        let len = self.data.len() - start;
        let prev_total = self.byte_totals.last().copied().unwrap_or(0);
        self.byte_totals
            .push(prev_total + len + usize::from(had_newline));
        self.line_ends.push(self.data.len());
        self.last_line_open = !had_newline;
    }

    /// Follow mode (`F`): reads whatever input has arrived since EOF was
    /// last seen, without blocking on a pipe that has nothing to offer.
    /// Returns true when the cache changed (new lines, or more text on a
    /// line that had no newline yet).
    pub fn read_new_input(&mut self) -> Result<bool, String> {
        self.ensure_open()?;
        let before = (self.line_ends.len(), self.data.len());
        // Also drains the part of the input not read yet (a follow started
        // before the end of a pipe was reached), in bounded steps so a
        // producer that never pauses (`yes | rless`) cannot hold the
        // keyboard off: the rest is read on the next poll.
        let budget = self.data.len() + FOLLOW_READ_BUDGET;
        while self.data.len() < budget {
            let reader = self.reader.as_mut().expect("opened above");
            if !reader.ready() {
                break;
            }
            self.eof = false;
            self.read_line()?;
            if self.eof {
                break;
            }
        }
        Ok((self.line_ends.len(), self.data.len()) != before)
    }

    /// Reads the entire remaining input. Used only when a true-EOF-dependent
    /// answer is required (absolute goto beyond a known line, `G` with no
    /// count, or window-clamping near the tail) — see §3 item 3 of the spec.
    pub fn ensure_full_read(&mut self) -> Result<(), String> {
        self.ensure_open()?;
        while !self.eof {
            self.read_line()?;
        }
        Ok(())
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
        self.ensure_lines_available(64)?;
        // `data` holds the cached lines' bytes (newlines excluded) plus zero
        // padding, which can never be ESC.
        Ok(self.data.contains(&0x1b))
    }
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
    fn read_new_input_picks_up_appended_lines_and_continues_an_open_line() {
        let path = temp_file(b"one\ntw");
        let mut buf = Buffer::new(Source::File(path.clone()));
        buf.ensure_full_read().unwrap();
        assert_eq!(buf.lines_cached(), 2);
        assert_eq!(buf.line(2), Some(&b"tw"[..]));
        assert!(!buf.read_new_input().unwrap(), "nothing new yet");

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"o\nthree\nfou").unwrap();
        drop(f);
        assert!(buf.read_new_input().unwrap());
        assert!(buf.is_eof());
        assert_eq!(buf.lines_cached(), 4);
        assert_eq!(buf.line(2), Some(&b"two"[..]));
        assert_eq!(buf.line(3), Some(&b"three"[..]));
        assert_eq!(buf.line(4), Some(&b"fou"[..]));
        assert_eq!(buf.total_bytes().unwrap(), 17);
        assert_eq!(buf.byte_offset_through(2), 8);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_reads_a_changed_file_afresh() {
        let path = temp_file(b"a\nb\n");
        let mut buf = Buffer::new(Source::File(path.clone()));
        assert_eq!(buf.total_lines().unwrap(), 2);
        std::fs::write(&path, b"x\n").unwrap();
        assert!(buf.reload());
        assert_eq!(buf.total_lines().unwrap(), 1);
        assert_eq!(buf.line(1), Some(&b"x"[..]));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn memory_source_reads_like_a_file_and_cannot_reload() {
        let mut buf = Buffer::new(Source::Memory {
            name: Some("HELP".into()),
            bytes: b"l1\nl2".to_vec(),
        });
        assert_eq!(buf.display_name().as_deref(), Some("HELP"));
        assert_eq!(buf.total_lines().unwrap(), 2);
        assert_eq!(buf.line(2), Some(&b"l2"[..]));
        assert!(!buf.reload());
    }
}
