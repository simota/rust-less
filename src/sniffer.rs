//! Statistical multi-line format sniffer for structured stream detection.
//!
//! Evaluates candidate lines (up to 16 lines) against structural format heuristics
//! (JSONL, CSV, TSV, Logfmt) with a configurable confidence threshold (default: 0.80).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetectedFormat {
    #[default]
    PlainText,
    JsonLines,
    Csv,
    Tsv,
    Logfmt,
}

impl DetectedFormat {
    pub fn name(&self) -> &'static str {
        match self {
            Self::PlainText => "plain",
            Self::JsonLines => "jsonl",
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Logfmt => "logfmt",
        }
    }

    pub fn is_structured(&self) -> bool {
        !matches!(self, Self::PlainText)
    }

    pub fn next_cycle(&self) -> Self {
        match self {
            Self::PlainText => Self::JsonLines,
            Self::JsonLines => Self::Csv,
            Self::Csv => Self::Tsv,
            Self::Tsv => Self::Logfmt,
            Self::Logfmt => Self::PlainText,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnifferConfig {
    pub max_sample_lines: usize,
    pub confidence_threshold: f64,
}

impl Default for SnifferConfig {
    fn default() -> Self {
        Self {
            max_sample_lines: 16,
            confidence_threshold: 0.80,
        }
    }
}

pub struct FormatSniffer {
    config: SnifferConfig,
}

impl Default for FormatSniffer {
    fn default() -> Self {
        Self::new(SnifferConfig::default())
    }
}

impl FormatSniffer {
    pub fn new(config: SnifferConfig) -> Self {
        Self { config }
    }

    /// Sniffs format from a slice of line byte slices.
    pub fn sniff<B: AsRef<[u8]>>(&self, sample_lines: &[B]) -> DetectedFormat {
        if sample_lines.is_empty() {
            return DetectedFormat::PlainText;
        }

        let total = sample_lines.len().min(self.config.max_sample_lines);
        let sample = &sample_lines[..total];

        let mut json_hits = 0;
        let mut csv_comma_hits = 0;
        let mut csv_tab_hits = 0;
        let mut logfmt_hits = 0;
        let mut non_empty_lines = 0;

        let mut first_comma_count: Option<usize> = None;
        let mut first_tab_count: Option<usize> = None;

        for line_ref in sample {
            let line = line_ref.as_ref();
            let trimmed = trim_ascii_whitespace(line);
            if trimmed.is_empty() {
                continue;
            }
            non_empty_lines += 1;

            if is_likely_json_line(trimmed) {
                json_hits += 1;
            }

            if is_likely_logfmt_line(trimmed) {
                logfmt_hits += 1;
            }

            let comma_count = trimmed.iter().filter(|&&b| b == b',').count();
            if comma_count > 0 {
                match first_comma_count {
                    None => {
                        first_comma_count = Some(comma_count);
                        csv_comma_hits += 1;
                    }
                    Some(expected) if expected == comma_count => {
                        csv_comma_hits += 1;
                    }
                    _ => {}
                }
            }

            let tab_count = trimmed.iter().filter(|&&b| b == b'\t').count();
            if tab_count > 0 {
                match first_tab_count {
                    None => {
                        first_tab_count = Some(tab_count);
                        csv_tab_hits += 1;
                    }
                    Some(expected) if expected == tab_count => {
                        csv_tab_hits += 1;
                    }
                    _ => {}
                }
            }
        }

        if non_empty_lines == 0 {
            return DetectedFormat::PlainText;
        }

        let thresh = (non_empty_lines as f64) * self.config.confidence_threshold;

        if (json_hits as f64) >= thresh {
            return DetectedFormat::JsonLines;
        }

        if (csv_tab_hits as f64) >= thresh && first_tab_count.unwrap_or(0) >= 1 {
            return DetectedFormat::Tsv;
        }

        if (csv_comma_hits as f64) >= thresh && first_comma_count.unwrap_or(0) >= 1 {
            return DetectedFormat::Csv;
        }

        if (logfmt_hits as f64) >= thresh {
            return DetectedFormat::Logfmt;
        }

        DetectedFormat::PlainText
    }
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

fn is_likely_json_line(line: &[u8]) -> bool {
    if line.len() < 2 {
        return false;
    }
    (line[0] == b'{' && line[line.len() - 1] == b'}')
        || (line[0] == b'[' && line[line.len() - 1] == b']')
}

fn is_likely_logfmt_line(line: &[u8]) -> bool {
    let mut equals_count = 0;
    let mut in_quote = false;
    let mut prev_char = b' ';

    for &b in line {
        if b == b'"' && prev_char != b'\\' {
            in_quote = !in_quote;
        } else if !in_quote && b == b'=' {
            equals_count += 1;
        }
        prev_char = b;
    }

    equals_count >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_jsonlines() {
        let sniffer = FormatSniffer::default();
        let lines = vec![
            r#"{"ts":"2026-08-23T00:00:00Z","level":"info","msg":"service started"}"#,
            r#"{"ts":"2026-08-23T00:00:01Z","level":"warn","msg":"slow query","duration_ms":42}"#,
            r#"{"ts":"2026-08-23T00:00:02Z","level":"error","msg":"db failure","code":500}"#,
        ];
        assert_eq!(sniffer.sniff(&lines), DetectedFormat::JsonLines);
    }

    #[test]
    fn detects_csv_with_consistent_commas() {
        let sniffer = FormatSniffer::default();
        let lines = vec![
            "id,name,role,department",
            "1,Alice,Engineer,Platform",
            "2,Bob,Manager,Operations",
            "3,Charlie,Designer,Product",
        ];
        assert_eq!(sniffer.sniff(&lines), DetectedFormat::Csv);
    }

    #[test]
    fn detects_tsv() {
        let sniffer = FormatSniffer::default();
        let lines = vec![
            "id\tname\trole\tdepartment",
            "1\tAlice\tEngineer\tPlatform",
            "2\tBob\tManager\tOperations",
        ];
        assert_eq!(sniffer.sniff(&lines), DetectedFormat::Tsv);
    }

    #[test]
    fn detects_logfmt() {
        let sniffer = FormatSniffer::default();
        let lines = vec![
            r#"ts=2026-08-23T00:00:00Z level=info msg="hello world" user_id=42"#,
            r#"ts=2026-08-23T00:00:01Z level=warn msg="retry attempt" attempt=3"#,
        ];
        assert_eq!(sniffer.sniff(&lines), DetectedFormat::Logfmt);
    }

    #[test]
    fn plain_text_fallback_on_irregular_commas() {
        let sniffer = FormatSniffer::default();
        let lines = vec![
            "This is a normal paragraph, with some commas.",
            "Another line, perhaps, but irregular in count, structure, and type.",
            "No commas here.",
        ];
        assert_eq!(sniffer.sniff(&lines), DetectedFormat::PlainText);
    }
}
