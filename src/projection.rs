//! Viewport projection, column formatting, dual-layer coordinates, and in-memory sort snapshots.

use crate::simd::{FieldSpan, FieldType, StructuralClassifier};
use crate::sniffer::DetectedFormat;
use std::collections::{BTreeMap, HashSet};
use unicode_width::UnicodeWidthStr;

/// Dual coordinate identifier for a rendered row.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LineCoordinate {
    pub buffer_line: usize,
    pub sub_row: usize,
}

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

pub struct ProjectionEngine {
    pub format: DetectedFormat,
    pub columns: Vec<ColumnInfo>,
    pub expanded_lines: HashSet<usize>,
    pub sticky_header: bool,
    pub sorted_by_column: Option<(String, bool)>, // (column_name, ascending)
    pub projection_index: Vec<usize>,             // Sorted indices of buffer lines
    pub span_pool: ViewportSpanPool,
}

impl ProjectionEngine {
    pub fn new(format: DetectedFormat) -> Self {
        Self {
            format,
            columns: Vec::new(),
            expanded_lines: HashSet::new(),
            sticky_header: false,
            sorted_by_column: None,
            projection_index: Vec::new(),
            span_pool: ViewportSpanPool::new(),
        }
    }

    pub fn set_format(&mut self, format: DetectedFormat) {
        self.format = format;
        self.columns.clear();
        self.expanded_lines.clear();
        self.sorted_by_column = None;
        self.projection_index.clear();
    }

    pub fn toggle_expand(&mut self, buffer_line: usize) {
        if self.expanded_lines.contains(&buffer_line) {
            self.expanded_lines.remove(&buffer_line);
        } else {
            self.expanded_lines.insert(buffer_line);
        }
    }

    #[allow(dead_code)]
    pub fn is_expanded(&self, buffer_line: usize) -> bool {
        self.expanded_lines.contains(&buffer_line)
    }

    /// Derives or updates column definitions from a set of lines.
    pub fn update_columns_from_lines<B: AsRef<[u8]>>(&mut self, lines: &[B]) {
        if !self.format.is_structured() || lines.is_empty() {
            return;
        }

        let mut seen_keys = Vec::new();
        let mut key_widths: BTreeMap<String, usize> = BTreeMap::new();

        let mut temp_spans = Vec::new();

        for (i, line_ref) in lines.iter().enumerate() {
            let line = line_ref.as_ref();
            temp_spans.clear();
            match self.format {
                DetectedFormat::JsonLines => {
                    StructuralClassifier::scan_json_line(line, &mut temp_spans);
                    for span in &temp_spans {
                        let k = String::from_utf8_lossy(span.key(line)).to_string();
                        let val_slice = span.value(line);
                        let w = display_width_strip_ansi(val_slice).max(k.len());
                        if !seen_keys.contains(&k) {
                            seen_keys.push(k.clone());
                        }
                        let entry = key_widths.entry(k).or_default();
                        *entry = (*entry).max(w);
                    }
                }
                DetectedFormat::Csv | DetectedFormat::Tsv => {
                    let delim = if self.format == DetectedFormat::Csv {
                        b','
                    } else {
                        b'\t'
                    };
                    StructuralClassifier::scan_csv_line(line, delim, &mut temp_spans);
                    if i == 0 && self.columns.is_empty() {
                        // Use header row as column names if available
                        for (col_idx, span) in temp_spans.iter().enumerate() {
                            let raw_name = String::from_utf8_lossy(span.value(line)).to_string();
                            let clean_name = raw_name.trim_matches('"').to_string();
                            let name = if clean_name.is_empty() {
                                format!("col_{}", col_idx + 1)
                            } else {
                                clean_name
                            };
                            seen_keys.push(name.clone());
                            key_widths.insert(name, span.display_width as usize);
                        }
                    } else {
                        for (col_idx, span) in temp_spans.iter().enumerate() {
                            let name = if col_idx < self.columns.len() {
                                self.columns[col_idx].name.clone()
                            } else {
                                format!("col_{}", col_idx + 1)
                            };
                            if !seen_keys.contains(&name) {
                                seen_keys.push(name.clone());
                            }
                            let entry = key_widths.entry(name).or_default();
                            *entry = (*entry).max(span.display_width as usize);
                        }
                    }
                }
                DetectedFormat::Logfmt => {
                    StructuralClassifier::scan_logfmt_line(line, &mut temp_spans);
                    for span in &temp_spans {
                        let k = String::from_utf8_lossy(span.key(line)).to_string();
                        let val_slice = span.value(line);
                        let w = display_width_strip_ansi(val_slice).max(k.len());
                        if !seen_keys.contains(&k) {
                            seen_keys.push(k.clone());
                        }
                        let entry = key_widths.entry(k).or_default();
                        *entry = (*entry).max(w);
                    }
                }
                DetectedFormat::PlainText => {}
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
            // Update existing widths
            for col in &mut self.columns {
                if let Some(&w) = key_widths.get(&col.name) {
                    col.width = col.width.max(w).min(40);
                }
            }
        }
    }

    /// Projects a single raw line into one or more rendered display lines.
    pub fn project_line(
        &mut self,
        line: &[u8],
        buffer_line_idx: usize,
        tabular: bool,
    ) -> Vec<String> {
        if !self.format.is_structured() {
            return vec![String::from_utf8_lossy(line).to_string()];
        }

        self.span_pool.reset();
        match self.format {
            DetectedFormat::JsonLines => {
                StructuralClassifier::scan_json_line(line, &mut self.span_pool.spans);
            }
            DetectedFormat::Csv | DetectedFormat::Tsv => {
                let delim = if self.format == DetectedFormat::Csv {
                    b','
                } else {
                    b'\t'
                };
                StructuralClassifier::scan_csv_line(line, delim, &mut self.span_pool.spans);
            }
            DetectedFormat::Logfmt => {
                StructuralClassifier::scan_logfmt_line(line, &mut self.span_pool.spans);
            }
            DetectedFormat::PlainText => {}
        }

        if self.span_pool.spans.is_empty() {
            return vec![String::from_utf8_lossy(line).to_string()];
        }

        let is_expanded = self.expanded_lines.contains(&buffer_line_idx);

        if is_expanded && self.format == DetectedFormat::JsonLines {
            let mut out_rows = Vec::new();
            out_rows.push(format!("{{ /* line {} */", buffer_line_idx));
            for span in &self.span_pool.spans {
                let key = String::from_utf8_lossy(span.key(line));
                let val = String::from_utf8_lossy(span.value(line));
                if span.val_type == FieldType::Object || span.val_type == FieldType::Array {
                    out_rows.push(format!("  \"{}\": {} ...", key, val));
                } else {
                    out_rows.push(format!("  \"{}\": {},", key, val));
                }
            }
            out_rows.push("}".to_string());
            return out_rows;
        }

        if tabular && !self.columns.is_empty() {
            let mut row_buf = String::new();
            // Pinned columns first
            for col in self.columns.iter().filter(|c| c.pinned && !c.hidden) {
                let val_str = self.find_field_value(&col.name, line);
                let w = col.width;
                let padded = pad_or_truncate_cell(&val_str, w);
                row_buf.push_str(&padded);
                row_buf.push_str("\x1b[0m│");
            }
            // Unpinned columns
            for col in self.columns.iter().filter(|c| !c.pinned && !c.hidden) {
                let val_str = self.find_field_value(&col.name, line);
                let w = col.width;
                let padded = pad_or_truncate_cell(&val_str, w);
                row_buf.push_str(&padded);
                row_buf.push_str("\x1b[0m│");
            }
            return vec![row_buf];
        }

        vec![String::from_utf8_lossy(line).to_string()]
    }

    fn find_field_value(&self, col_name: &str, line: &[u8]) -> String {
        for (i, span) in self.span_pool.spans.iter().enumerate() {
            let k = match self.format {
                DetectedFormat::JsonLines | DetectedFormat::Logfmt => {
                    String::from_utf8_lossy(span.key(line)).to_string()
                }
                DetectedFormat::Csv | DetectedFormat::Tsv => {
                    if i < self.columns.len() {
                        self.columns[i].name.clone()
                    } else {
                        format!("col_{}", i + 1)
                    }
                }
                _ => String::new(),
            };

            if k == col_name {
                let raw_val = String::from_utf8_lossy(span.value(line)).to_string();
                return raw_val.trim_matches('"').to_string();
            }
        }
        String::new()
    }
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
    let stripped = strip_ansi_escapes(text);
    let cur_width = UnicodeWidthStr::width(stripped.as_str());

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
}
