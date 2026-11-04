//! Structured-view projection: column derivation, per-line projection
//! (tabular row / expanded JSON tree / raw fallback) and assembly of a
//! window of projected rows from a `Buffer`. This is the structured
//! counterpart of `screen::render_window`; `command` only paints the
//! result.
//!
//! Every byte that comes from the input is escaped before it reaches the
//! terminal: control characters become caret notation (`^G`, `^M`, `^[`),
//! as in the core view. With `-R`, ESC is let through so ANSI colour
//! survives, the same choice `-R` makes for the core view.

use crate::buffer::Buffer;
use crate::selector::Query;
use crate::simd::{FieldSpan, FieldType, StructuralClassifier};
use crate::sniffer::DetectedFormat;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub width: usize,
    pub pinned: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ViewportSpanPool {
    pub spans: Vec<FieldSpan>,
}

impl ViewportSpanPool {
    pub fn new() -> Self {
        Self {
            spans: Vec::with_capacity(256),
        }
    }

    pub fn reset(&mut self) {
        self.spans.clear();
    }
}

/// A window of projected rows, ready to paint. Rows are already escaped
/// and clipped to the terminal width.
pub struct ProjectedWindow {
    /// Sticky header row (column names in display order), if shown.
    pub header: Option<String>,
    pub rows: Vec<String>,
    /// Buffer line that the last displayed row belongs to.
    pub bottom_line: usize,
    /// True when nothing follows the last displayed row.
    pub at_eof: bool,
}

pub struct ProjectionEngine {
    pub format: DetectedFormat,
    pub columns: Vec<ColumnInfo>,
    pub expanded_lines: HashSet<usize>,
    pub sticky_header: bool,
    /// `-R`: keep ESC (ANSI colour) in projected text instead of escaping it.
    pub raw_ansi: bool,
    pub span_pool: ViewportSpanPool,
}

impl ProjectionEngine {
    pub fn new(format: DetectedFormat) -> Self {
        Self {
            format,
            columns: Vec::new(),
            expanded_lines: HashSet::new(),
            sticky_header: false,
            raw_ansi: false,
            span_pool: ViewportSpanPool::new(),
        }
    }

    pub fn set_format(&mut self, format: DetectedFormat) {
        self.format = format;
        self.columns.clear();
        self.expanded_lines.clear();
    }

    pub fn toggle_expand(&mut self, buffer_line: usize) {
        if self.expanded_lines.contains(&buffer_line) {
            self.expanded_lines.remove(&buffer_line);
        } else {
            self.expanded_lines.insert(buffer_line);
        }
    }

    /// Visible columns in the order they are drawn: pinned columns first,
    /// then the rest, each group in its original order. The header and
    /// every table row use this one ordering, so they cannot disagree.
    pub fn display_columns(&self) -> impl Iterator<Item = &ColumnInfo> {
        let visible = |pinned: bool| {
            self.columns
                .iter()
                .filter(move |c| c.pinned == pinned && !c.hidden)
        };
        visible(true).chain(visible(false))
    }

    /// Derives or updates column definitions from a set of lines.
    ///
    /// CSV/TSV: the first sampled line names the columns when no columns
    /// exist yet. Every sampled line, the header included, then widens the
    /// column it fills, so a value longer than its header is not cut.
    pub fn update_columns_from_lines<B: AsRef<[u8]>>(&mut self, lines: &[B]) {
        if !self.format.is_structured() || lines.is_empty() {
            return;
        }

        let is_delimited = matches!(self.format, DetectedFormat::Csv | DetectedFormat::Tsv);
        let mut seen_keys: Vec<String> = Vec::new();
        let mut key_widths: BTreeMap<String, usize> = BTreeMap::new();
        let mut spans = Vec::new();

        for (i, line_ref) in lines.iter().enumerate() {
            let line = line_ref.as_ref();
            StructuralClassifier::scan_fields(self.format, line, &mut spans);
            for (col_idx, span) in spans.iter().enumerate() {
                let (name, width) = if is_delimited {
                    let name = if i == 0 && self.columns.is_empty() {
                        let raw = String::from_utf8_lossy(span.value(line));
                        let clean = raw.trim_matches('"');
                        if clean.is_empty() {
                            format!("col_{}", col_idx + 1)
                        } else {
                            clean.to_string()
                        }
                    } else if let Some(col) = self.columns.get(col_idx) {
                        col.name.clone()
                    } else if let Some(header) = seen_keys.get(col_idx) {
                        header.clone()
                    } else {
                        format!("col_{}", col_idx + 1)
                    };
                    (name, span.display_width as usize)
                } else {
                    let name = String::from_utf8_lossy(span.key(line)).to_string();
                    let width = display_width_strip_ansi(span.value(line)).max(name.len());
                    (name, width)
                };
                // Cells show control characters escaped (`^G` is 2 cells),
                // so size the column for the escaped text.
                let width = width + self.escape_overhead(span.value(line));
                if !seen_keys.contains(&name) {
                    seen_keys.push(name.clone());
                }
                let entry = key_widths.entry(name).or_default();
                *entry = (*entry).max(width);
            }
        }

        if self.columns.is_empty() {
            self.columns = seen_keys
                .into_iter()
                .map(|name| {
                    let w = key_widths.get(&name).copied().unwrap_or(8).clamp(4, 40);
                    ColumnInfo {
                        name,
                        width: w,
                        pinned: false,
                        hidden: false,
                    }
                })
                .collect();
        } else {
            for col in &mut self.columns {
                if let Some(&w) = key_widths.get(&col.name) {
                    col.width = col.width.max(w).min(40);
                }
            }
        }
    }

    /// The sticky header row: column names in display order.
    fn header_text(&self) -> String {
        let mut header = String::new();
        for col in self.display_columns() {
            header.push_str(&pad_or_truncate_cell(&self.escape(&col.name), col.width));
            header.push('│');
        }
        header
    }

    /// Extra display columns `escape` adds to `value`.
    fn escape_overhead(&self, value: &[u8]) -> usize {
        let text = String::from_utf8_lossy(value);
        if !text.chars().any(char::is_control) {
            return 0;
        }
        let escaped = display_width_strip_ansi(self.escape(&text).as_bytes());
        escaped.saturating_sub(display_width_strip_ansi(text.as_bytes()))
    }

    fn escape<'a>(&self, text: &'a str) -> Cow<'a, str> {
        escape_controls(text, self.raw_ansi)
    }

    /// Projects a single raw line into one or more rendered display lines.
    pub fn project_line(
        &mut self,
        line: &[u8],
        buffer_line_idx: usize,
        tabular: bool,
    ) -> Vec<String> {
        let raw_row = |this: &Self| vec![this.escape(&String::from_utf8_lossy(line)).into_owned()];
        if !self.format.is_structured() {
            return raw_row(self);
        }

        self.span_pool.reset();
        StructuralClassifier::scan_fields(self.format, line, &mut self.span_pool.spans);

        if self.span_pool.spans.is_empty() {
            return raw_row(self);
        }

        let is_expanded = self.expanded_lines.contains(&buffer_line_idx);

        if is_expanded && self.format == DetectedFormat::JsonLines {
            let mut out_rows = Vec::new();
            out_rows.push(format!("{{ /* line {} */", buffer_line_idx));
            let last = self.span_pool.spans.len() - 1;
            for (i, span) in self.span_pool.spans.iter().enumerate() {
                let (key, val) = (
                    String::from_utf8_lossy(span.key(line)),
                    String::from_utf8_lossy(span.value(line)),
                );
                let (key, val) = (self.escape(&key), self.escape(&val));
                if span.val_type == FieldType::Object || span.val_type == FieldType::Array {
                    out_rows.push(format!("  \"{}\": {} ...", key, val));
                } else {
                    let comma = if i == last { "" } else { "," };
                    out_rows.push(format!("  \"{}\": {}{}", key, val, comma));
                }
            }
            out_rows.push("}".to_string());
            return out_rows;
        }

        if tabular && !self.columns.is_empty() {
            // The reset after each cell only matters when `-R` lets colour
            // through: it stops a cell's colour bleeding into the next one.
            let separator = if self.raw_ansi { "\x1b[0m│" } else { "│" };
            let mut row_buf = String::new();
            for col in self.display_columns() {
                let value = self.find_field_value(&col.name, line);
                row_buf.push_str(&pad_or_truncate_cell(&self.escape(&value), col.width));
                row_buf.push_str(separator);
            }
            return vec![row_buf];
        }

        raw_row(self)
    }

    fn find_field_value(&self, col_name: &str, line: &[u8]) -> String {
        // Keys are compared in place (borrowed) rather than materialized as
        // a String per span per cell; only the matched value is allocated.
        for (i, span) in self.span_pool.spans.iter().enumerate() {
            let is_match = match self.format {
                DetectedFormat::JsonLines | DetectedFormat::Logfmt => {
                    String::from_utf8_lossy(span.key(line)).as_ref() == col_name
                }
                DetectedFormat::Csv | DetectedFormat::Tsv => {
                    if i < self.columns.len() {
                        self.columns[i].name == col_name
                    } else {
                        format!("col_{}", i + 1) == col_name
                    }
                }
                _ => col_name.is_empty(),
            };

            if is_match {
                return String::from_utf8_lossy(span.value(line))
                    .trim_matches('"')
                    .to_string();
            }
        }
        String::new()
    }

    /// Assembles the window starting at buffer line `top`: the optional
    /// sticky header, then projected rows of lines that pass `filter`,
    /// until `content_rows` rows are filled or input ends. Read errors are
    /// returned, not swallowed, as `screen::render_window` does.
    pub fn render_window(
        &mut self,
        buffer: &mut Buffer,
        top: usize,
        content_rows: usize,
        width: usize,
        filter: Option<&Query>,
        tabular: bool,
    ) -> Result<ProjectedWindow, String> {
        let header = (self.sticky_header && !self.columns.is_empty())
            .then(|| clip_to_width(self.header_text(), width));
        let capacity = content_rows.saturating_sub(header.is_some() as usize);
        // A CSV/TSV file's first line is the header row the column names
        // came from; under the sticky header it would show twice.
        let header_line = (header.is_some()
            && matches!(self.format, DetectedFormat::Csv | DetectedFormat::Tsv))
        .then_some(1);

        let mut rows = Vec::new();
        let mut bottom_line = top;
        let mut line_no = top.max(1);
        let mut at_eof = false;
        let mut cut_short = false;
        while rows.len() < capacity {
            buffer.ensure_lines_read(line_no)?;
            let Some(raw) = buffer.line(line_no) else {
                at_eof = true;
                break;
            };
            if Some(line_no) != header_line
                && filter.is_none_or(|q| q.matches_line(raw, self.format))
            {
                let sub_rows = self.project_line(raw, line_no, tabular);
                let room = capacity - rows.len();
                cut_short = sub_rows.len() > room;
                rows.extend(
                    sub_rows
                        .into_iter()
                        .take(room)
                        .map(|row| clip_to_width(row, width)),
                );
                bottom_line = line_no;
            }
            line_no += 1;
        }
        if !at_eof && !cut_short {
            // The window filled exactly: peek one line to know whether the
            // input ends here (the same peek `screen::render_window` does).
            buffer.ensure_lines_read(line_no)?;
            at_eof = buffer.line(line_no).is_none();
        }

        Ok(ProjectedWindow {
            header,
            rows,
            bottom_line,
            at_eof,
        })
    }
}

/// Replaces control characters with caret notation (`\x07` → `^G`,
/// DEL → `^?`) so input bytes cannot move the cursor, ring the bell or
/// start an escape sequence. Tab becomes one space (cells are fixed width).
/// With `keep_esc`, ESC passes through so `-R` colour still works.
pub fn escape_controls(text: &str, keep_esc: bool) -> Cow<'_, str> {
    // Byte-level pre-check: every control character is a C0 byte, DEL, or
    // (C1, U+0080..U+009F) starts with lead byte 0xC2. A 0xC2 that turns
    // out to be ordinary Latin-1 just takes the exact path below.
    if !text.bytes().any(|b| b < 0x20 || b == 0x7f || b == 0xc2) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\t' => out.push(' '),
            '\x1b' if keep_esc => out.push(c),
            '\x00'..='\x1f' | '\x7f' => {
                out.push('^');
                out.push(((c as u8) ^ 0x40) as char);
            }
            // C1 controls (U+0080..U+009F) can also act as escapes on some
            // terminals; show them as their code point.
            c if c.is_control() => out.push_str(&format!("<U+{:04X}>", c as u32)),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Cuts `text` to at most `width` display columns. A cut row ends in `>`,
/// the marker the core view's `-S` uses for a chopped line. Escape
/// sequences (only present with `-R`) take no width; a cut inside coloured
/// text is followed by a reset.
pub fn clip_to_width(text: String, width: usize) -> String {
    if display_width(&text) <= width {
        return text;
    }
    let limit = width.saturating_sub(1);
    let mut out = String::with_capacity(text.len().min(width * 4));
    let mut used = 0;
    let mut in_esc = false;
    let mut saw_esc = false;
    for ch in text.chars() {
        if ch == '\x1b' {
            in_esc = true;
            saw_esc = true;
            out.push(ch);
            continue;
        }
        if in_esc {
            out.push(ch);
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > limit {
            break;
        }
        out.push(ch);
        used += w;
    }
    if saw_esc {
        out.push_str("\x1b[0m");
    }
    if width > 0 {
        out.push('>');
    }
    out
}

/// Display width of already-escaped projected text (no control characters
/// other than ESC), escape sequences excluded, without allocating.
fn display_width(text: &str) -> usize {
    if text.is_ascii() && !text.contains('\x1b') {
        // Projected text is already escaped, so ASCII here is printable:
        // one column per byte.
        return text.len();
    }
    if !text.contains('\x1b') {
        return UnicodeWidthStr::width(text);
    }
    let mut width = 0;
    let mut in_esc = false;
    for c in text.chars() {
        if c == '\x1b' {
            in_esc = true;
        } else if in_esc {
            in_esc = !c.is_ascii_alphabetic();
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

pub fn display_width_strip_ansi(bytes: &[u8]) -> usize {
    if let Ok(s) = std::str::from_utf8(bytes) {
        let stripped = strip_ansi_escapes(s);
        UnicodeWidthStr::width(stripped.as_str())
    } else {
        bytes.len()
    }
}

pub fn strip_ansi_escapes(s: &str) -> String {
    let mut clean = String::with_capacity(s.len());
    let mut in_esc = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_esc = true;
        } else if in_esc {
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else {
            clean.push(c);
        }
    }
    clean
}

fn pad_or_truncate_cell(text: &str, target_width: usize) -> String {
    let cur_width = display_width(text);

    if cur_width <= target_width {
        let mut out = text.to_string();
        for _ in 0..(target_width - cur_width) {
            out.push(' ');
        }
        out
    } else {
        let max_content_width = target_width.saturating_sub(1);
        let mut out = String::new();
        let mut w = 0;
        let mut in_esc = false;

        for ch in text.chars() {
            if ch == '\x1b' {
                in_esc = true;
                out.push(ch);
                continue;
            }
            if in_esc {
                out.push(ch);
                if ch.is_ascii_alphabetic() {
                    in_esc = false;
                }
                continue;
            }

            let ch_w = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]));
            if w + ch_w > max_content_width {
                out.push('…');
                w += 1;
                break;
            }
            out.push(ch);
            w += ch_w;
        }

        if in_esc {
            out.push_str("\x1b[0m");
        }

        while w < target_width {
            out.push(' ');
            w += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_tabular_jsonl_with_cjk_alignment() {
        let mut proj = ProjectionEngine::new(DetectedFormat::JsonLines);
        let line1 = r#"{"user":"田中太郎","status":200,"role":"admin"}"#;
        let line2 = r#"{"user":"Bob","status":500,"role":"dev"}"#;
        let lines = vec![line1.as_bytes(), line2.as_bytes()];
        proj.update_columns_from_lines(&lines);

        let row1 = proj.project_line(lines[0], 1, true);
        assert_eq!(row1.len(), 1);
        assert!(row1[0].contains("田中太郎"));
        assert!(row1[0].contains("200"));
    }

    #[test]
    fn expands_inline_json_tree() {
        let mut proj = ProjectionEngine::new(DetectedFormat::JsonLines);
        let line = br#"{"level":"error","req":{"path":"/login"},"code":500}"#;
        proj.toggle_expand(1);

        let rows = proj.project_line(line, 1, false);
        assert!(rows.len() >= 4);
        assert_eq!(rows[0], "{ /* line 1 */");
        assert!(rows.iter().any(|r| r.contains("\"level\": \"error\"")));
        assert!(
            rows.iter()
                .any(|r| r.contains("\"req\": {\"path\":\"/login\"} ..."))
        );
        assert_eq!(rows.last().unwrap(), "}");
    }

    #[test]
    fn pad_or_truncate_cell_exact_width_cjk() {
        let text = "こんにちは世界";
        let target_width = 10;
        let padded = pad_or_truncate_cell(text, target_width);
        let actual_width = display_width_strip_ansi(padded.as_bytes());
        assert_eq!(actual_width, target_width);
        assert_eq!(padded, "こんにち… ");
    }

    #[test]
    fn control_characters_are_escaped_like_the_core_view() {
        assert_eq!(escape_controls("plain", false), "plain");
        assert_eq!(
            escape_controls("a\x07b\rc\x1b[2Jd\x7f", false),
            "a^Gb^Mc^[[2Jd^?"
        );
        assert_eq!(escape_controls("a\tb", false), "a b");
        assert_eq!(escape_controls("\u{9b}x", false), "<U+009B>x");
        // -R keeps ESC (colour), still escapes the rest.
        assert_eq!(escape_controls("\x1b[31mred\x07", true), "\x1b[31mred^G");
    }

    #[test]
    fn tabular_and_raw_rows_never_carry_raw_control_bytes() {
        let mut proj = ProjectionEngine::new(DetectedFormat::JsonLines);
        let lines: [&[u8]; 2] = [br#"{"msg":"bell\u0007"}"#, b"{\"msg\":\"cr\rX\x1b[2J\"}"];
        proj.update_columns_from_lines(&lines);
        for line in lines {
            for tabular in [true, false] {
                for row in proj.project_line(line, 1, tabular) {
                    assert!(!row.chars().any(char::is_control), "{row:?}");
                }
            }
        }
        proj.toggle_expand(2);
        for row in proj.project_line(lines[1], 2, false) {
            assert!(!row.chars().any(char::is_control), "{row:?}");
        }
    }

    #[test]
    fn clip_to_width_marks_cut_rows_and_respects_wide_characters() {
        assert_eq!(clip_to_width("abc".into(), 3), "abc");
        assert_eq!(clip_to_width("abcdef".into(), 4), "abc>");
        // 3 double-width chars = 6 columns; 5 columns leave room for 2 + '>'.
        assert_eq!(clip_to_width("日本語".into(), 5), "日本>");
        assert_eq!(
            clip_to_width("\x1b[31mabcdef".into(), 3),
            "\x1b[31mab\x1b[0m>"
        );
    }

    #[test]
    fn csv_columns_come_from_the_header_and_fit_their_values() {
        let mut proj = ProjectionEngine::new(DetectedFormat::Csv);
        let lines: [&[u8]; 3] = [b"name,age,city", b"alice,30,tokyo", b"bob,25,osaka"];
        proj.update_columns_from_lines(&lines);
        let cols: Vec<_> = proj
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.width))
            .collect();
        assert_eq!(cols, [("name", 5), ("age", 4), ("city", 5)]);
        assert_eq!(proj.project_line(lines[1], 2, true), ["alice│30  │tokyo│"]);
    }

    #[test]
    fn header_and_rows_share_the_pinned_first_display_order() {
        let mut proj = ProjectionEngine::new(DetectedFormat::JsonLines);
        let lines: [&[u8]; 1] = [br#"{"a":"1","b":"2","c":"3"}"#];
        proj.update_columns_from_lines(&lines);
        proj.columns[2].pinned = true;
        proj.columns[0].hidden = true;
        let order: Vec<_> = proj.display_columns().map(|c| c.name.as_str()).collect();
        assert_eq!(order, ["c", "b"]);
        assert_eq!(proj.header_text(), "c   │b   │");
        assert_eq!(proj.project_line(lines[0], 1, true), ["3   │2   │"]);
    }

    #[test]
    fn csv_header_line_is_not_repeated_under_the_sticky_header() {
        let path =
            std::env::temp_dir().join(format!("rust-less-proj-csv-{}.csv", std::process::id()));
        std::fs::write(&path, "name,age\nalice,30\nbob,25\n").unwrap();
        let mut buf = Buffer::new(crate::buffer::Source::File(path.clone()));
        let mut proj = ProjectionEngine::new(DetectedFormat::Csv);
        proj.update_columns_from_lines(&[b"name,age", b"alice,30"]);
        let win = proj.render_window(&mut buf, 1, 5, 80, None, true).unwrap();
        assert_eq!(win.header, None);
        assert_eq!(win.rows[0], "name │age │", "no sticky header: line 1 stays");
        proj.sticky_header = true;
        let win = proj.render_window(&mut buf, 1, 5, 80, None, true).unwrap();
        assert_eq!(win.header.as_deref(), Some("name │age │"));
        assert_eq!(win.rows, ["alice│30  │", "bob  │25  │"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expanded_json_has_no_trailing_comma() {
        let mut proj = ProjectionEngine::new(DetectedFormat::JsonLines);
        proj.toggle_expand(1);
        let rows = proj.project_line(br#"{"a":1,"b":2}"#, 1, false);
        assert_eq!(rows, ["{ /* line 1 */", "  \"a\": 1,", "  \"b\": 2", "}"]);
    }

    #[test]
    fn render_window_reports_the_last_displayed_line_under_a_header() {
        let path =
            std::env::temp_dir().join(format!("rust-less-proj-{}.jsonl", std::process::id()));
        let content: String = (1..=10).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        std::fs::write(&path, content).unwrap();
        let mut buf = Buffer::new(crate::buffer::Source::File(path.clone()));
        let mut proj = ProjectionEngine::new(DetectedFormat::JsonLines);
        proj.update_columns_from_lines(&[br#"{"n":1}"#]);
        proj.sticky_header = true;

        let win = proj.render_window(&mut buf, 1, 5, 80, None, true).unwrap();
        assert!(win.header.is_some());
        assert_eq!(win.rows.len(), 4, "header takes one of the 5 rows");
        assert_eq!(win.bottom_line, 4);
        assert!(!win.at_eof);

        let win = proj.render_window(&mut buf, 7, 5, 80, None, true).unwrap();
        assert_eq!(win.bottom_line, 10);
        assert!(win.at_eof, "the window ends exactly at the last line");

        let only_even = Query::parse(".n >= 9");
        let win = proj
            .render_window(&mut buf, 1, 5, 80, Some(&only_even), true)
            .unwrap();
        assert_eq!(win.rows.len(), 2);
        assert_eq!(win.bottom_line, 10);
        assert!(win.at_eof);
        let _ = std::fs::remove_file(path);
    }
}
