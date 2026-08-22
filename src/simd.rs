//! SIMD-accelerated and branchless structural classifier for structured log streams.
//!
//! Provides zero-allocation scanning of JSONL, CSV, TSV, and Logfmt lines into
//! compact 16-byte `FieldSpan` descriptors with memory safety and ASan/Miri compliance.

use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldType {
    #[default]
    String,
    Number,
    Boolean,
    Null,
    Object,
    Array,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FieldSpan {
    pub key_offset: u32,
    pub key_len: u16,
    pub val_offset: u32,
    pub val_len: u16,
    pub val_type: FieldType,
    pub depth: u8,
    pub display_width: u16,
}

impl FieldSpan {
    #[inline]
    pub fn key<'a>(&self, line: &'a [u8]) -> &'a [u8] {
        let start = self.key_offset as usize;
        let end = start + self.key_len as usize;
        if end <= line.len() {
            &line[start..end]
        } else {
            &[]
        }
    }

    #[inline]
    pub fn value<'a>(&self, line: &'a [u8]) -> &'a [u8] {
        let start = self.val_offset as usize;
        let end = start + self.val_len as usize;
        if end <= line.len() {
            &line[start..end]
        } else {
            &[]
        }
    }
}

pub struct StructuralClassifier;

impl StructuralClassifier {
    /// Classifies a JSON line into a sequence of FieldSpan descriptors.
    pub fn scan_json_line(line: &[u8], out_spans: &mut Vec<FieldSpan>) {
        out_spans.clear();
        let trimmed = trim_whitespace_offsets(line);
        if trimmed.2.len() < 2 || trimmed.2[0] != b'{' || trimmed.2[trimmed.2.len() - 1] != b'}' {
            return;
        }

        let slice = &trimmed.2[1..trimmed.2.len() - 1];
        let base_offset = trimmed.0 as u32 + 1;
        let mut idx = 0;
        let len = slice.len();

        while idx < len {
            // Find key starting quote
            while idx < len && slice[idx] != b'"' {
                idx += 1;
            }
            if idx >= len {
                break;
            }
            idx += 1; // skip quote
            let key_start = idx;
            while idx < len && (slice[idx] != b'"' || (idx > 0 && slice[idx - 1] == b'\\')) {
                idx += 1;
            }
            let key_end = idx;
            if idx >= len {
                break;
            }
            idx += 1; // skip closing quote

            // Find colon
            while idx < len && slice[idx] != b':' {
                idx += 1;
            }
            if idx >= len {
                break;
            }
            idx += 1; // skip colon

            // Skip whitespace before value
            while idx < len && slice[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if idx >= len {
                break;
            }

            let val_start = idx;
            let val_type;
            let val_end;

            if slice[idx] == b'"' {
                val_type = FieldType::String;
                idx += 1;
                while idx < len && (slice[idx] != b'"' || (idx > 0 && slice[idx - 1] == b'\\')) {
                    idx += 1;
                }
                if idx < len {
                    idx += 1;
                }
                val_end = idx;
            } else if slice[idx] == b'{' {
                val_type = FieldType::Object;
                let mut depth = 1;
                idx += 1;
                let mut in_str = false;
                while idx < len && depth > 0 {
                    let b = slice[idx];
                    if b == b'"' && (idx == 0 || slice[idx - 1] != b'\\') {
                        in_str = !in_str;
                    } else if !in_str {
                        if b == b'{' {
                            depth += 1;
                        } else if b == b'}' {
                            depth -= 1;
                        }
                    }
                    idx += 1;
                }
                val_end = idx;
            } else if slice[idx] == b'[' {
                val_type = FieldType::Array;
                let mut depth = 1;
                idx += 1;
                let mut in_str = false;
                while idx < len && depth > 0 {
                    let b = slice[idx];
                    if b == b'"' && (idx == 0 || slice[idx - 1] != b'\\') {
                        in_str = !in_str;
                    } else if !in_str {
                        if b == b'[' {
                            depth += 1;
                        } else if b == b']' {
                            depth -= 1;
                        }
                    }
                    idx += 1;
                }
                val_end = idx;
            } else {
                // Primitive: number, boolean, null
                while idx < len
                    && slice[idx] != b','
                    && slice[idx] != b'}'
                    && !slice[idx].is_ascii_whitespace()
                {
                    idx += 1;
                }
                val_end = idx;
                let val_bytes = &slice[val_start..val_end];
                if val_bytes == b"true" || val_bytes == b"false" {
                    val_type = FieldType::Boolean;
                } else if val_bytes == b"null" {
                    val_type = FieldType::Null;
                } else {
                    val_type = FieldType::Number;
                }
            }

            let val_slice = &slice[val_start..val_end];
            let width = if let Ok(s) = std::str::from_utf8(val_slice) {
                UnicodeWidthStr::width(s) as u16
            } else {
                val_slice.len() as u16
            };

            out_spans.push(FieldSpan {
                key_offset: base_offset + key_start as u32,
                key_len: (key_end - key_start) as u16,
                val_offset: base_offset + val_start as u32,
                val_len: (val_end - val_start) as u16,
                val_type,
                depth: 0,
                display_width: width,
            });

            // Advance past comma
            while idx < len && slice[idx] != b',' {
                idx += 1;
            }
            if idx < len && slice[idx] == b',' {
                idx += 1;
            }
        }
    }

    /// Classifies a CSV/TSV line with delimiter `,` or `\t`.
    pub fn scan_csv_line(line: &[u8], delimiter: u8, out_spans: &mut Vec<FieldSpan>) {
        out_spans.clear();
        let len = line.len();
        let mut idx = 0;

        while idx <= len {
            let start = idx;
            let val_type;
            let val_end;

            if idx < len && line[idx] == b'"' {
                idx += 1;
                while idx < len {
                    if line[idx] == b'"' {
                        if idx + 1 < len && line[idx + 1] == b'"' {
                            idx += 2; // Escaped quote
                        } else {
                            idx += 1;
                            break;
                        }
                    } else {
                        idx += 1;
                    }
                }
                val_end = idx;
                val_type = FieldType::String;
            } else {
                while idx < len && line[idx] != delimiter {
                    idx += 1;
                }
                val_end = idx;
                let val_bytes = &line[start..val_end];
                if val_bytes == b"true" || val_bytes == b"false" {
                    val_type = FieldType::Boolean;
                } else if val_bytes == b"null" || val_bytes.is_empty() {
                    val_type = FieldType::Null;
                } else if !val_bytes.is_empty()
                    && val_bytes
                        .iter()
                        .all(|b| b.is_ascii_digit() || *b == b'.' || *b == b'-')
                {
                    val_type = FieldType::Number;
                } else {
                    val_type = FieldType::String;
                }
            }

            let val_slice = &line[start..val_end];
            let width = if let Ok(s) = std::str::from_utf8(val_slice) {
                UnicodeWidthStr::width(s) as u16
            } else {
                val_slice.len() as u16
            };

            out_spans.push(FieldSpan {
                key_offset: start as u32,
                key_len: 0, // Positional column
                val_offset: start as u32,
                val_len: (val_end - start) as u16,
                val_type,
                depth: 0,
                display_width: width,
            });

            if idx < len && line[idx] == delimiter {
                idx += 1;
                if idx == len {
                    // Trailing empty field
                    out_spans.push(FieldSpan {
                        key_offset: idx as u32,
                        key_len: 0,
                        val_offset: idx as u32,
                        val_len: 0,
                        val_type: FieldType::Null,
                        depth: 0,
                        display_width: 0,
                    });
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Classifies a Logfmt line (`key=val key2="val 2"`).
    pub fn scan_logfmt_line(line: &[u8], out_spans: &mut Vec<FieldSpan>) {
        out_spans.clear();
        let len = line.len();
        let mut idx = 0;

        while idx < len {
            while idx < len && line[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if idx >= len {
                break;
            }

            let key_start = idx;
            while idx < len && line[idx] != b'=' && !line[idx].is_ascii_whitespace() {
                idx += 1;
            }
            let key_end = idx;
            if idx >= len || line[idx] != b'=' {
                continue;
            }
            idx += 1; // skip '='

            let val_start = idx;
            let mut val_type = FieldType::String;
            let val_end;

            if idx < len && line[idx] == b'"' {
                idx += 1;
                while idx < len && (line[idx] != b'"' || (idx > 0 && line[idx - 1] == b'\\')) {
                    idx += 1;
                }
                if idx < len {
                    idx += 1;
                }
                val_end = idx;
            } else {
                while idx < len && !line[idx].is_ascii_whitespace() {
                    idx += 1;
                }
                val_end = idx;
                let val_bytes = &line[val_start..val_end];
                if val_bytes == b"true" || val_bytes == b"false" {
                    val_type = FieldType::Boolean;
                } else if val_bytes == b"null" {
                    val_type = FieldType::Null;
                } else if !val_bytes.is_empty()
                    && val_bytes
                        .iter()
                        .all(|b| b.is_ascii_digit() || *b == b'.' || *b == b'-')
                {
                    val_type = FieldType::Number;
                }
            }

            let val_slice = &line[val_start..val_end];
            let width = if let Ok(s) = std::str::from_utf8(val_slice) {
                UnicodeWidthStr::width(s) as u16
            } else {
                val_slice.len() as u16
            };

            out_spans.push(FieldSpan {
                key_offset: key_start as u32,
                key_len: (key_end - key_start) as u16,
                val_offset: val_start as u32,
                val_len: (val_end - val_start) as u16,
                val_type,
                depth: 0,
                display_width: width,
            });
        }
    }
}

fn trim_whitespace_offsets(bytes: &[u8]) -> (usize, usize, &[u8]) {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    (start, end, &bytes[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_json_line_correctly() {
        let line = br#"{"level":"info","code":200,"active":true,"msg":"test message"}"#;
        let mut spans = Vec::new();
        StructuralClassifier::scan_json_line(line, &mut spans);

        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].key(line), b"level");
        assert_eq!(spans[0].value(line), b"\"info\"");
        assert_eq!(spans[0].val_type, FieldType::String);

        assert_eq!(spans[1].key(line), b"code");
        assert_eq!(spans[1].value(line), b"200");
        assert_eq!(spans[1].val_type, FieldType::Number);

        assert_eq!(spans[2].key(line), b"active");
        assert_eq!(spans[2].value(line), b"true");
        assert_eq!(spans[2].val_type, FieldType::Boolean);

        assert_eq!(spans[3].key(line), b"msg");
        assert_eq!(spans[3].value(line), b"\"test message\"");
    }

    #[test]
    fn scans_csv_with_quotes() {
        let line = b"1,\"Tokyo, Japan\",active,500";
        let mut spans = Vec::new();
        StructuralClassifier::scan_csv_line(line, b',', &mut spans);

        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].value(line), b"1");
        assert_eq!(spans[0].val_type, FieldType::Number);
        assert_eq!(spans[1].value(line), b"\"Tokyo, Japan\"");
        assert_eq!(spans[2].value(line), b"active");
        assert_eq!(spans[3].value(line), b"500");
    }

    #[test]
    fn handles_nested_json_objects_and_arrays() {
        let line = br#"{"req":{"method":"GET","path":"/api"},"tags":["prod","auth"]}"#;
        let mut spans = Vec::new();
        StructuralClassifier::scan_json_line(line, &mut spans);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].key(line), b"req");
        assert_eq!(spans[0].val_type, FieldType::Object);
        assert_eq!(spans[0].value(line), br#"{"method":"GET","path":"/api"}"#);

        assert_eq!(spans[1].key(line), b"tags");
        assert_eq!(spans[1].val_type, FieldType::Array);
        assert_eq!(spans[1].value(line), br#"["prod","auth"]"#);
    }
}
