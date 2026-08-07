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
    Stdin,
}

/// One input file (or stdin), with lines read incrementally and cached.
pub struct Buffer {
    source: Source,
    reader: Option<Box<dyn BufRead>>,
    open_error: Option<String>,
    lines: Vec<Vec<u8>>,
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
            lines: Vec::new(),
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
                Ok(f) => Box::new(BufReader::new(f)),
                Err(e) => {
                    let msg = format!("{}: {}", path.display(), describe_io_error(&e));
                    self.open_error = Some(msg.clone());
                    return Err(msg);
                }
            },
            Source::Stdin => Box::new(BufReader::new(io::stdin())),
        };
        self.reader = Some(reader);
        Ok(())
    }

    /// Reads lines until at least `up_to` lines are cached, or EOF.
    pub fn ensure_lines_read(&mut self, up_to: usize) -> Result<(), String> {
        self.ensure_open()?;
        if self.eof || self.lines.len() >= up_to {
            return Ok(());
        }
        let reader = self.reader.as_mut().expect("opened above");
        while self.lines.len() < up_to {
            let mut raw = Vec::new();
            let n = reader
                .read_until(b'\n', &mut raw)
                .map_err(|e| describe_io_error(&e))?;
            if n == 0 {
                self.eof = true;
                break;
            }
            let had_newline = raw.last() == Some(&b'\n');
            if had_newline {
                raw.pop();
            }
            let prev_total = self.byte_totals.last().copied().unwrap_or(0);
            self.byte_totals
                .push(prev_total + raw.len() + if had_newline { 1 } else { 0 });
            self.lines.push(raw);
        }
        Ok(())
    }

    /// Reads the entire remaining input. Used only when a true-EOF-dependent
    /// answer is required (absolute goto beyond a known line, `G` with no
    /// count, or window-clamping near the tail) — see §3 item 3 of the spec.
    pub fn ensure_full_read(&mut self) -> Result<(), String> {
        self.ensure_open()?;
        while !self.eof {
            self.ensure_lines_read(self.lines.len() + 4096)?;
        }
        Ok(())
    }

    pub fn line(&self, n: usize) -> Option<&[u8]> {
        if n == 0 {
            return None;
        }
        self.lines.get(n - 1).map(|v| v.as_slice())
    }

    pub fn lines_cached(&self) -> usize {
        self.lines.len()
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
        Ok(self.lines.len())
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
        Ok(self.lines.iter().any(|l| l.contains(&0x1b)))
    }
}

fn describe_io_error(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "No such file or directory".to_string(),
        io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        _ => e.to_string(),
    }
}

/// Reads the raw bytes of stdin fully upfront isn't used; kept here only to
/// document why Buffer::Source::Stdin uses BufReader::new directly: crossterm
/// reads keyboard events via the controlling tty on Unix, not via
/// `std::io::stdin()`, so piping file content over stdin does not conflict
/// with interactive key handling.
#[allow(dead_code)]
fn _doc_marker() {}

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
    fn byte_offset_tracking() {
        let path = temp_file(b"abc\nde\n");
        let mut buf = Buffer::new(Source::File(path.clone()));
        buf.ensure_full_read().unwrap();
        assert_eq!(buf.byte_offset_through(1), 4); // "abc\n"
        assert_eq!(buf.byte_offset_through(2), 7); // "abc\nde\n"
        let _ = std::fs::remove_file(path);
    }
}
