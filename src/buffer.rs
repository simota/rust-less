//! Lazy, incremental line access to a single input source (file or stdin).
//!
//! Reads are performed on demand so that opening a large file does not block
//! the first screen draw (`startup-large-file`, 5000-line fixture): only as
//! many lines as a caller actually asks for are ever read from disk.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

pub enum Source {
    File(PathBuf),
    /// Read through an ordinary `BufReader`: crossterm reads keyboard
    /// events from the controlling tty on Unix, not from `std::io::stdin()`,
    /// so piping content over stdin does not conflict with key handling.
    Stdin,
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
    reader: Option<Box<dyn BufRead>>,
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
        }
    }

    pub fn display_name(&self) -> Option<String> {
        match &self.source {
            Source::File(p) => Some(p.display().to_string()),
            Source::Stdin => None,
        }
    }

    fn ensure_open(&mut self) -> Result<(), String> {
        if self.reader.is_some() || self.open_error.is_some() {
            return match &self.open_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            };
        }
        let reader: Box<dyn BufRead> = match &self.source {
            Source::File(path) => match File::open(path) {
                Ok(f) => Box::new(BufReader::with_capacity(READ_BUF_CAPACITY, f)),
                Err(e) => {
                    let msg = format!("{}: {}", path.display(), describe_io_error(&e));
                    self.open_error = Some(msg.clone());
                    return Err(msg);
                }
            },
            Source::Stdin => Box::new(BufReader::with_capacity(READ_BUF_CAPACITY, io::stdin())),
        };
        self.reader = Some(reader);
        Ok(())
    }

    /// Reads lines until at least `up_to` lines are cached, or EOF.
    pub fn ensure_lines_read(&mut self, up_to: usize) -> Result<(), String> {
        self.ensure_open()?;
        if self.eof || self.line_ends.len() >= up_to {
            return Ok(());
        }
        let reader = self.reader.as_mut().expect("opened above");
        while self.line_ends.len() < up_to {
            let prev_end = self.data.len();
            self.data.resize(line_start(prev_end), 0);
            let n = match reader.read_until(b'\n', &mut self.data) {
                Ok(n) => n,
                Err(e) => {
                    // read_until keeps whatever it read before failing;
                    // drop that partial line, as the per-line buffer used to.
                    self.data.truncate(prev_end);
                    return Err(describe_io_error(&e));
                }
            };
            if n == 0 {
                self.data.truncate(prev_end);
                self.eof = true;
                break;
            }
            let had_newline = self.data.last() == Some(&b'\n');
            if had_newline {
                self.data.pop();
            }
            let len = self.data.len() - line_start(prev_end);
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
    pub fn ensure_full_read(&mut self) -> Result<(), String> {
        self.ensure_open()?;
        while !self.eof {
            self.ensure_lines_read(self.line_ends.len() + 4096)?;
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
        self.ensure_lines_read(64)?;
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
}
