//! Forward/backward line search across `buffer` (regex, or any line
//! predicate). The sticky last-pattern state for `n`/`N` lives in `command`.

use crate::buffer::Buffer;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

impl Direction {
    pub fn reversed(self) -> Direction {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }
}

/// Finds the `count`-th line matching `pattern`, scanning in `direction`
/// starting at `start_line` (1-indexed). `inclusive` controls whether
/// `start_line` itself is a candidate match (true for a fresh `/`/`?`
/// search per §2 `search-forward`'s "starts at the first displayed line";
/// false for `n`/`N` repeats, which must not re-match the line already
/// landed on).
pub fn find(
    buffer: &mut Buffer,
    pattern: &Regex,
    direction: Direction,
    start_line: usize,
    inclusive: bool,
    count: u64,
) -> Result<Option<usize>, String> {
    find_by(
        buffer,
        |raw| line_matches(raw, pattern),
        direction,
        start_line,
        inclusive,
        count,
    )
}

/// `find` with any line predicate (the structured selector search uses
/// this), so both kinds of search share one scan and one set of
/// start/inclusive/count rules.
#[inline]
pub fn find_by(
    buffer: &mut Buffer,
    mut is_match: impl FnMut(&[u8]) -> bool,
    direction: Direction,
    start_line: usize,
    inclusive: bool,
    count: u64,
) -> Result<Option<usize>, String> {
    let count = count.max(1);
    let mut found = 0u64;
    let mut last_match = None;
    match direction {
        Direction::Forward => {
            let mut line = if inclusive {
                start_line
            } else {
                start_line + 1
            };
            loop {
                buffer.ensure_lines_read(line)?;
                let Some(raw) = buffer.line(line) else { break };
                if is_match(raw) {
                    found += 1;
                    last_match = Some(line);
                    if found >= count {
                        break;
                    }
                }
                line += 1;
            }
        }
        Direction::Backward => {
            buffer.ensure_full_read()?;
            let total = buffer.lines_cached();
            if total == 0 {
                return Ok(None);
            }
            let start = if inclusive {
                start_line.min(total)
            } else {
                start_line.saturating_sub(1)
            };
            let mut line = start;
            while line >= 1 {
                if let Some(raw) = buffer.line(line)
                    && is_match(raw)
                {
                    found += 1;
                    last_match = Some(line);
                    if found >= count {
                        break;
                    }
                }
                if line == 1 {
                    break;
                }
                line -= 1;
            }
        }
    }
    Ok(last_match)
}

#[inline]
pub fn line_matches(raw: &[u8], pattern: &Regex) -> bool {
    // Search over a lossy UTF-8 view; malformed bytes cannot participate in
    // a text match anyway, and no baseline scenario searches such content.
    // Valid lines (the norm) are checked with `str::from_utf8`, whose
    // word-at-a-time ASCII path is far cheaper than `from_utf8_lossy`'s
    // byte-at-a-time chunk walk; for a valid line both yield the same text.
    match std::str::from_utf8(raw) {
        Ok(text) => pattern.is_match(text),
        Err(_) => pattern.is_match(&String::from_utf8_lossy(raw)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Source;
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;

    fn temp_file(contents: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rust-less-search-test-{}-{}.txt",
            std::process::id(),
            rand_suffix()
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
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

    fn lines_fixture() -> String {
        let mut s = String::new();
        for n in 1..=200 {
            if n == 20 || n == 80 || n == 140 {
                s.push_str(&format!(
                    "{:03} line {} of 200 :: ALPHA_TOKEN filler\n",
                    n, n
                ));
            } else {
                s.push_str(&format!(
                    "{:03} line {} of 200 :: plain filler content\n",
                    n, n
                ));
            }
        }
        s
    }

    #[test]
    fn forward_search_lands_on_first_match_inclusive_of_start() {
        let path = temp_file(&lines_fixture());
        let mut buf = Buffer::new(Source::File(path.clone()));
        let re = Regex::new("ALPHA_TOKEN").unwrap();
        let found = find(&mut buf, &re, Direction::Forward, 1, true, 1).unwrap();
        assert_eq!(found, Some(20));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn repeat_search_sequence_matches_capture_evidence() {
        // search-forward-multi-match-repeat: /ALPHA_TOKEN -> 20; n -> 80; n -> 140; N -> 80.
        let path = temp_file(&lines_fixture());
        let mut buf = Buffer::new(Source::File(path.clone()));
        let re = Regex::new("ALPHA_TOKEN").unwrap();

        let m1 = find(&mut buf, &re, Direction::Forward, 1, true, 1)
            .unwrap()
            .unwrap();
        assert_eq!(m1, 20);
        let m2 = find(&mut buf, &re, Direction::Forward, m1, false, 1)
            .unwrap()
            .unwrap();
        assert_eq!(m2, 80);
        let m3 = find(&mut buf, &re, Direction::Forward, m2, false, 1)
            .unwrap()
            .unwrap();
        assert_eq!(m3, 140);
        // N reverses direction relative to the original forward search.
        let m4 = find(&mut buf, &re, Direction::Backward, m3, false, 1)
            .unwrap()
            .unwrap();
        assert_eq!(m4, 80);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn backward_search_from_eof() {
        let path = temp_file(&lines_fixture());
        let mut buf = Buffer::new(Source::File(path.clone()));
        let re = Regex::new("ALPHA_TOKEN").unwrap();
        let found = find(&mut buf, &re, Direction::Backward, 200, true, 1).unwrap();
        assert_eq!(found, Some(140));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_utf8_lines_are_matched_through_the_lossy_view() {
        let re = Regex::new("a\u{FFFD}b").unwrap();
        assert!(line_matches(b"xa\xffb", &re));
        assert!(!line_matches(b"xab", &re));
        let dot = Regex::new("^a.b$").unwrap();
        assert!(line_matches(b"a\xffb", &dot));
        assert!(line_matches("a\u{6f22}b".as_bytes(), &dot));
        assert!(!line_matches(b"a\xff\xfeb", &dot));
    }

    #[test]
    fn no_match_returns_none() {
        let path = temp_file(&lines_fixture());
        let mut buf = Buffer::new(Source::File(path.clone()));
        let re = Regex::new("NOPE_NOT_HERE").unwrap();
        let found = find(&mut buf, &re, Direction::Forward, 1, true, 1).unwrap();
        assert_eq!(found, None);
        let _ = std::fs::remove_file(path);
    }
}
