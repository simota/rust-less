//! Which buffer lines a filtered or sorted view shows, and in what order.
//!
//! The core pager shows every buffer line in order and needs none of this.
//! Once a filter (`&`) or a column sort is active, navigation, search and
//! drawing all work on *positions* in a `ViewIndex` instead of raw line
//! numbers, so `j`/`k`/`G`/`n` can never land on a hidden line.
//!
//! A filter-only view is built lazily: lines are examined only as far as a
//! caller asks, so a filter on a huge file or a live stream costs nothing
//! beyond what is displayed (and lines appended later by follow mode are
//! picked up naturally). A sorted view needs every line and is built in
//! one pass, then rebuilt if the buffer grows.

use crate::buffer::Buffer;
use crate::selector::{Query, RegexQuery};
use crate::sniffer::DetectedFormat;
use std::cmp::Ordering;

/// An `&` filter. The source is interpreted per file: as a selector
/// expression on a structured file, as a regex otherwise, so one filter
/// stays meaningful across `:n` into a file of another format. A leading
/// `!` on a regex hides the matching lines instead (as in less).
#[derive(Debug, Clone)]
pub struct LineFilter {
    pub source: String,
    structured: Option<Query>,
    regex: RegexQuery,
    negate_regex: bool,
}

impl LineFilter {
    pub fn new(source: &str, case_insensitive: bool) -> Self {
        let structured = match Query::parse_with_case(source, case_insensitive) {
            q @ Query::Structured(_) => Some(q),
            Query::Regex(_) => None,
        };
        let (negate_regex, pattern) = match source.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, source),
        };
        LineFilter {
            source: source.to_string(),
            structured,
            regex: RegexQuery::with_case(pattern.to_string(), case_insensitive),
            negate_regex,
        }
    }

    /// True when the filter is read as a selector expression on `format`.
    pub fn is_structured_on(&self, format: DetectedFormat) -> bool {
        format.is_structured() && self.structured.is_some()
    }

    /// An error message when the filter cannot be applied on `format`.
    pub fn check(&self, format: DetectedFormat) -> Result<(), String> {
        if self.is_structured_on(format) || self.regex.regex().is_some() {
            Ok(())
        } else {
            Err("Invalid pattern".to_string())
        }
    }

    /// `columns` names a CSV/TSV line's positional fields.
    pub fn matches(&self, line: &[u8], format: DetectedFormat, columns: &[String]) -> bool {
        if let Some(q) = self.structured.as_ref().filter(|_| format.is_structured()) {
            return q.matches_fields(line, format, columns);
        }
        self.regex.is_match_bytes(line) != self.negate_regex
    }
}

/// A sortable cell value: numbers (and durations) order numerically and
/// before text; a missing value always sorts last.
#[derive(Debug, Clone, PartialEq)]
pub enum SortKey {
    Number(f64),
    Text(String),
    Missing,
}

impl SortKey {
    pub fn from_value(value: Option<&str>) -> SortKey {
        match value {
            None => SortKey::Missing,
            Some(v) if v.is_empty() || v == "null" => SortKey::Missing,
            Some(v) => match v
                .parse::<f64>()
                .ok()
                .or_else(|| crate::selector::parse_duration_millis(v))
            {
                Some(n) if n.is_finite() => SortKey::Number(n),
                _ => SortKey::Text(v.to_string()),
            },
        }
    }

    fn cmp_present(&self, other: &SortKey) -> Ordering {
        match (self, other) {
            (SortKey::Number(a), SortKey::Number(b)) => a.total_cmp(b),
            (SortKey::Number(_), SortKey::Text(_)) => Ordering::Less,
            (SortKey::Text(_), SortKey::Number(_)) => Ordering::Greater,
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

pub struct ViewIndex {
    filter: Option<LineFilter>,
    format: DetectedFormat,
    /// Column names of a CSV/TSV file, for selector filters.
    columns: Vec<String>,
    /// CSV/TSV header line, kept as the first row of any view.
    keep_first: bool,
    /// Visible buffer lines, in view order.
    lines: Vec<usize>,
    /// Filter-only: buffer lines examined so far.
    scanned: usize,
    /// Sorted view: `position[line]` (usize::MAX when hidden), and the
    /// buffer line count it was built for.
    sorted: Option<(Vec<usize>, usize)>,
}

impl ViewIndex {
    /// A lazily-built filtered view (buffer order).
    pub fn filtered(
        filter: LineFilter,
        format: DetectedFormat,
        columns: Vec<String>,
        keep_first: bool,
    ) -> Self {
        ViewIndex {
            filter: Some(filter),
            format,
            columns,
            keep_first,
            lines: Vec::new(),
            scanned: 0,
            sorted: None,
        }
    }

    /// A complete view of every visible line, ordered by `key` (stable:
    /// equal keys keep buffer order), descending if asked. Missing values
    /// go last in both directions.
    pub fn sorted(
        buffer: &mut Buffer,
        filter: Option<LineFilter>,
        format: DetectedFormat,
        columns: Vec<String>,
        keep_first: bool,
        descending: bool,
        mut key: impl FnMut(&[u8]) -> SortKey,
    ) -> Result<Self, String> {
        buffer.ensure_full_read()?;
        let total = buffer.lines_cached();
        let mut keyed: Vec<(SortKey, usize)> = Vec::new();
        let mut head = Vec::new();
        for n in 1..=total {
            let raw = buffer.line(n).expect("fully read");
            if keep_first && n == 1 {
                head.push(1);
                continue;
            }
            if filter
                .as_ref()
                .is_none_or(|f| f.matches(raw, format, &columns))
            {
                keyed.push((key(raw), n));
            }
        }
        keyed.sort_by(|(a, _), (b, _)| match (a, b) {
            (SortKey::Missing, SortKey::Missing) => Ordering::Equal,
            (SortKey::Missing, _) => Ordering::Greater,
            (_, SortKey::Missing) => Ordering::Less,
            _ if descending => b.cmp_present(a),
            _ => a.cmp_present(b),
        });
        let lines: Vec<usize> = head
            .into_iter()
            .chain(keyed.into_iter().map(|(_, n)| n))
            .collect();
        let mut position = vec![usize::MAX; total + 1];
        for (pos, &line) in lines.iter().enumerate() {
            position[line] = pos;
        }
        Ok(ViewIndex {
            filter,
            format,
            columns,
            keep_first,
            lines,
            scanned: total,
            sorted: Some((position, total)),
        })
    }

    pub fn is_sorted(&self) -> bool {
        self.sorted.is_some()
    }

    /// A sorted view is a snapshot: true once the buffer has more lines
    /// than it was built for (follow mode), so the caller rebuilds it.
    pub fn is_stale(&self, buffer: &Buffer) -> bool {
        self.sorted
            .as_ref()
            .is_some_and(|(_, built)| buffer.lines_cached() > *built)
    }

    fn visible(&self, n: usize, raw: &[u8]) -> bool {
        (self.keep_first && n == 1)
            || self
                .filter
                .as_ref()
                .is_none_or(|f| f.matches(raw, self.format, &self.columns))
    }

    /// Examines lines until at least `want` visible lines are known or the
    /// input ends.
    fn fill(&mut self, buffer: &mut Buffer, want: usize) -> Result<(), String> {
        if self.sorted.is_some() {
            return Ok(());
        }
        while self.lines.len() < want {
            let n = self.scanned + 1;
            buffer.ensure_lines_read(n)?;
            let Some(raw) = buffer.line(n) else { break };
            if self.visible(n, raw) {
                self.lines.push(n);
            }
            self.scanned = n;
        }
        Ok(())
    }

    /// Buffer line at view position `pos` (0-based).
    pub fn line_at(&mut self, buffer: &mut Buffer, pos: usize) -> Result<Option<usize>, String> {
        self.fill(buffer, pos.saturating_add(1))?;
        Ok(self.lines.get(pos).copied())
    }

    /// Up to `count` buffer lines starting at view position `pos`.
    pub fn lines_from(
        &mut self,
        buffer: &mut Buffer,
        pos: usize,
        count: usize,
    ) -> Result<&[usize], String> {
        self.fill(buffer, pos.saturating_add(count))?;
        let start = pos.min(self.lines.len());
        let end = pos.saturating_add(count).min(self.lines.len());
        Ok(&self.lines[start..end])
    }

    /// Number of visible lines (reads the whole input).
    pub fn total(&mut self, buffer: &mut Buffer) -> Result<usize, String> {
        self.fill(buffer, usize::MAX)?;
        Ok(self.lines.len())
    }

    /// Visible lines known so far, and whether that is all of them.
    pub fn known(&self, buffer: &Buffer) -> (usize, bool) {
        let complete =
            self.sorted.is_some() || (buffer.is_eof() && self.scanned >= buffer.lines_cached());
        (self.lines.len(), complete)
    }

    /// Like `known`, but not counting a kept CSV/TSV header row: the
    /// number of data rows the view shows.
    pub fn known_rows(&self, buffer: &Buffer) -> (usize, bool) {
        let (n, complete) = self.known(buffer);
        let header = self.keep_first && self.lines.first() == Some(&1);
        (n - usize::from(header), complete)
    }

    /// View position of buffer line `line`; for a line the view hides, the
    /// position of the next visible line after it in buffer order (filter)
    /// or `None` (sorted, or nothing visible after it).
    pub fn position_of(
        &mut self,
        buffer: &mut Buffer,
        line: usize,
    ) -> Result<Option<usize>, String> {
        if let Some((position, _)) = &self.sorted {
            return Ok(position.get(line).copied().filter(|&p| p != usize::MAX));
        }
        while self.lines.last().is_none_or(|&last| last < line) {
            let before = self.scanned;
            self.fill(buffer, self.lines.len() + 1)?;
            if self.scanned == before {
                break;
            }
        }
        let pos = self.lines.partition_point(|&l| l < line);
        Ok((pos < self.lines.len()).then_some(pos))
    }

    /// Every visible buffer line, in view order (reads the whole input).
    pub fn all_lines(&mut self, buffer: &mut Buffer) -> Result<&[usize], String> {
        self.fill(buffer, usize::MAX)?;
        Ok(&self.lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Source;
    use std::path::PathBuf;

    fn buffer_with(contents: &str) -> (Buffer, PathBuf) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rust-less-view-test-{}-{}.txt",
            std::process::id(),
            contents.len() ^ (contents.as_ptr() as usize)
        ));
        std::fs::write(&path, contents).unwrap();
        (Buffer::new(Source::File(path.clone())), path)
    }

    #[test]
    fn filtered_view_is_lazy_and_maps_positions_both_ways() {
        let text: String = (1..=100)
            .map(|n| {
                if n % 10 == 0 {
                    format!("ERROR {n}\n")
                } else {
                    format!("info {n}\n")
                }
            })
            .collect();
        let (mut buf, path) = buffer_with(&text);
        let mut view = ViewIndex::filtered(
            LineFilter::new("ERROR", false),
            DetectedFormat::PlainText,
            Vec::new(),
            false,
        );
        assert_eq!(view.line_at(&mut buf, 0).unwrap(), Some(10));
        // Only as far as asked.
        assert!(buf.lines_cached() < 100);
        assert_eq!(view.lines_from(&mut buf, 1, 2).unwrap(), &[20, 30]);
        assert_eq!(view.position_of(&mut buf, 25).unwrap(), Some(2));
        assert_eq!(view.position_of(&mut buf, 30).unwrap(), Some(2));
        assert_eq!(view.position_of(&mut buf, 101).unwrap(), None);
        assert_eq!(view.total(&mut buf).unwrap(), 10);
        assert_eq!(view.known(&buf), (10, true));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn negated_regex_hides_matching_lines() {
        let f = LineFilter::new("!DEBUG", false);
        assert!(!f.matches(b"DEBUG x", DetectedFormat::PlainText, &[]));
        assert!(f.matches(b"INFO x", DetectedFormat::PlainText, &[]));
        let ci = LineFilter::new("error", true);
        assert!(ci.matches(b"ERROR", DetectedFormat::PlainText, &[]));
    }

    #[test]
    fn structured_filter_falls_back_to_regex_on_plain_text() {
        let f = LineFilter::new(".level == \"ERROR\"", false);
        assert!(f.is_structured_on(DetectedFormat::JsonLines));
        assert!(!f.is_structured_on(DetectedFormat::PlainText));
        assert!(f.matches(br#"{"level":"ERROR"}"#, DetectedFormat::JsonLines, &[]));
        assert!(!f.matches(br#"{"level":"INFO"}"#, DetectedFormat::JsonLines, &[]));
        // On plain text the same source is an ordinary regex.
        assert!(!f.matches(b"level ERROR", DetectedFormat::PlainText, &[]));
        assert!(
            LineFilter::new("(bad", false)
                .check(DetectedFormat::PlainText)
                .is_err()
        );
    }

    #[test]
    fn sorted_view_orders_numbers_then_text_and_missing_last() {
        let text = "h\n5\n10\nabc\n\n2\n";
        let (mut buf, path) = buffer_with(text);
        let key = |raw: &[u8]| {
            SortKey::from_value(std::str::from_utf8(raw).ok().filter(|s| !s.is_empty()))
        };
        let mut asc = ViewIndex::sorted(
            &mut buf,
            None,
            DetectedFormat::Csv,
            Vec::new(),
            true,
            false,
            key,
        )
        .unwrap();
        assert_eq!(asc.all_lines(&mut buf).unwrap(), &[1, 6, 2, 3, 4, 5]);
        let mut desc = ViewIndex::sorted(
            &mut buf,
            None,
            DetectedFormat::Csv,
            Vec::new(),
            true,
            true,
            key,
        )
        .unwrap();
        assert_eq!(desc.all_lines(&mut buf).unwrap(), &[1, 4, 3, 2, 6, 5]);
        assert_eq!(desc.position_of(&mut buf, 3).unwrap(), Some(2));
        assert!(!desc.is_stale(&buf));
        let _ = std::fs::remove_file(path);
    }
}
