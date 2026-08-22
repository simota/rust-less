//! JQ-style polymorphic dot-selector bytecode compiler and evaluator.
//!
//! Enables zero-copy queries directly in `/` (search) and `&` (filter) prompts:
//! e.g., `/.status >= 500`, `&.level == "error"`, `&dur > 500ms`, `/.user.id != null`.

use crate::simd::{FieldSpan, FieldType, StructuralClassifier};
use crate::sniffer::DetectedFormat;

#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    Equal,
    NotEqual,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Contains,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    String(String),
    Number(f64),
    Boolean(bool),
    Null,
    DurationMillis(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectorPredicate {
    pub field_path: String,
    pub op: Operator,
    pub operand: Operand,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Regex(String),
    Structured(Vec<SelectorPredicate>),
}

impl Query {
    /// Parses a raw search or filter prompt string into either a JQ dot-selector Query or a plain regex.
    pub fn parse(input: &str) -> Self {
        let trimmed = input.trim();
        let body = trimmed
            .trim_start_matches('/')
            .trim_start_matches('&')
            .trim();
        if (body.starts_with('.') || body.starts_with('@') || body.starts_with("dur"))
            && let Some(pred) = parse_single_predicate(body)
        {
            return Query::Structured(vec![pred]);
        }
        Query::Regex(input.to_string())
    }

    /// Evaluates the query against a raw line and its format.
    pub fn matches_line(&self, line: &[u8], format: DetectedFormat) -> bool {
        match self {
            Query::Regex(pat) => {
                if let Ok(re) = regex::Regex::new(pat)
                    && let Ok(s) = std::str::from_utf8(line)
                {
                    return re.is_match(s);
                }
                false
            }
            Query::Structured(preds) => {
                if !format.is_structured() {
                    return false;
                }
                let mut spans = Vec::new();
                match format {
                    DetectedFormat::JsonLines => {
                        StructuralClassifier::scan_json_line(line, &mut spans);
                    }
                    DetectedFormat::Csv | DetectedFormat::Tsv => {
                        let delim = if format == DetectedFormat::Csv {
                            b','
                        } else {
                            b'\t'
                        };
                        StructuralClassifier::scan_csv_line(line, delim, &mut spans);
                    }
                    DetectedFormat::Logfmt => {
                        StructuralClassifier::scan_logfmt_line(line, &mut spans);
                    }
                    DetectedFormat::PlainText => return false,
                }

                preds
                    .iter()
                    .all(|pred| evaluate_predicate(pred, line, &spans))
            }
        }
    }
}

fn parse_single_predicate(s: &str) -> Option<SelectorPredicate> {
    let s = s.trim();

    let mut in_quote = false;
    let mut escaped = false;
    let mut found_op: Option<(usize, usize, Operator)> = None;

    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        let b = bytes[i];
        if b == b'"' && !escaped {
            in_quote = !in_quote;
        } else if b == b'\\' && in_quote {
            escaped = !escaped;
            i += 1;
            continue;
        }

        if escaped {
            escaped = false;
        }

        if !in_quote {
            // Check 2-char operators first
            if i + 1 < len {
                let two = &s[i..i + 2];
                let op = match two {
                    ">=" => Some(Operator::GreaterThanOrEqual),
                    "<=" => Some(Operator::LessThanOrEqual),
                    "==" => Some(Operator::Equal),
                    "!=" => Some(Operator::NotEqual),
                    "=~" => Some(Operator::Contains),
                    _ => None,
                };
                if let Some(op) = op {
                    found_op = Some((i, 2, op));
                    break;
                }
            }

            // Check 1-char operators
            let one = match b {
                b'>' => Some(Operator::GreaterThan),
                b'<' => Some(Operator::LessThan),
                _ => None,
            };
            if let Some(op) = one {
                found_op = Some((i, 1, op));
                break;
            }
        }
        i += 1;
    }

    let (op_idx, op_len, op) = found_op?;
    let raw_field = s[..op_idx].trim();
    let raw_val = s[op_idx + op_len..].trim();

    let field_path = raw_field
        .trim_start_matches('.')
        .trim_start_matches('@')
        .to_string();
    let operand = parse_operand(raw_val)?;

    Some(SelectorPredicate {
        field_path,
        op,
        operand,
    })
}

fn parse_operand(s: &str) -> Option<Operand> {
    let s = s.trim();
    if s == "null" {
        return Some(Operand::Null);
    }
    if s == "true" {
        return Some(Operand::Boolean(true));
    }
    if s == "false" {
        return Some(Operand::Boolean(false));
    }

    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        return Some(Operand::String(s[1..s.len() - 1].to_string()));
    }

    // Check duration literals: e.g. 500ms, 2s, 1m, 1h
    if let Some(ms) = parse_duration_literal(s) {
        return Some(Operand::DurationMillis(ms));
    }

    // Try float/integer
    if let Ok(num) = s.parse::<f64>() {
        return Some(Operand::Number(num));
    }

    // Bare string literal fallback
    Some(Operand::String(s.to_string()))
}

fn parse_duration_literal(s: &str) -> Option<u64> {
    if let Some(stripped) = s.strip_suffix("ms") {
        let num = stripped.parse::<u64>().ok()?;
        Some(num)
    } else if let Some(stripped) = s.strip_suffix('s') {
        let num = stripped.parse::<u64>().ok()?;
        Some(num * 1000)
    } else if let Some(stripped) = s.strip_suffix('m') {
        let num = stripped.parse::<u64>().ok()?;
        Some(num * 60 * 1000)
    } else if let Some(stripped) = s.strip_suffix('h') {
        let num = stripped.parse::<u64>().ok()?;
        Some(num * 3600 * 1000)
    } else {
        None
    }
}

fn evaluate_predicate(pred: &SelectorPredicate, line: &[u8], spans: &[FieldSpan]) -> bool {
    let target_span = spans.iter().find(|span| {
        let k = span.key(line);
        if let Ok(s) = std::str::from_utf8(k) {
            s.trim_matches('"') == pred.field_path
        } else {
            false
        }
    });

    match &pred.operand {
        Operand::Null => {
            let exists = target_span.is_some() && target_span.unwrap().val_type != FieldType::Null;
            match pred.op {
                Operator::Equal => !exists,
                Operator::NotEqual => exists,
                _ => false,
            }
        }
        Operand::String(target_s) => {
            if let Some(span) = target_span {
                let raw_val = span.value(line);
                let val_str = String::from_utf8_lossy(raw_val)
                    .trim_matches('"')
                    .to_string();
                match pred.op {
                    Operator::Equal => &val_str == target_s,
                    Operator::NotEqual => &val_str != target_s,
                    Operator::Contains => val_str.contains(target_s),
                    _ => false,
                }
            } else {
                false
            }
        }
        Operand::Number(target_n) => {
            if let Some(span) = target_span {
                let raw_val = span.value(line);
                if let Ok(s) = std::str::from_utf8(raw_val)
                    && let Ok(num) = s.trim_matches('"').parse::<f64>()
                {
                    return match pred.op {
                        Operator::Equal => (num - target_n).abs() < f64::EPSILON,
                        Operator::NotEqual => (num - target_n).abs() >= f64::EPSILON,
                        Operator::GreaterThan => num > *target_n,
                        Operator::GreaterThanOrEqual => num >= *target_n,
                        Operator::LessThan => num < *target_n,
                        Operator::LessThanOrEqual => num <= *target_n,
                        _ => false,
                    };
                }
            }
            false
        }
        Operand::Boolean(target_b) => {
            if let Some(span) = target_span {
                let raw_val = span.value(line);
                let b_val = raw_val == b"true";
                match pred.op {
                    Operator::Equal => b_val == *target_b,
                    Operator::NotEqual => b_val != *target_b,
                    _ => false,
                }
            } else {
                false
            }
        }
        Operand::DurationMillis(target_ms) => {
            if let Some(span) = target_span {
                let raw_val = span.value(line);
                if let Ok(s) = std::str::from_utf8(raw_val)
                    && let Ok(num) = s.trim_matches('"').parse::<f64>()
                {
                    let ms = num as u64;
                    return match pred.op {
                        Operator::Equal => ms == *target_ms,
                        Operator::NotEqual => ms != *target_ms,
                        Operator::GreaterThan => ms > *target_ms,
                        Operator::GreaterThanOrEqual => ms >= *target_ms,
                        Operator::LessThan => ms < *target_ms,
                        Operator::LessThanOrEqual => ms <= *target_ms,
                        _ => false,
                    };
                }
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_predicate() {
        let q = Query::parse("/.status >= 500");
        assert!(matches!(q, Query::Structured(_)));

        let line = br#"{"status":500,"msg":"Internal Server Error"}"#;
        assert!(q.matches_line(line, DetectedFormat::JsonLines));

        let line_200 = br#"{"status":200,"msg":"OK"}"#;
        assert!(!q.matches_line(line_200, DetectedFormat::JsonLines));
    }

    #[test]
    fn parses_string_equality() {
        let q = Query::parse("&.level == \"ERROR\"");
        let line = br#"{"level":"ERROR","msg":"failed to connect"}"#;
        assert!(q.matches_line(line, DetectedFormat::JsonLines));

        let line_info = br#"{"level":"INFO","msg":"started"}"#;
        assert!(!q.matches_line(line_info, DetectedFormat::JsonLines));
    }

    #[test]
    fn parses_duration_literals() {
        let q = Query::parse("&.latency_ms > 250ms");
        let line_fast = br#"{"latency_ms":50,"service":"auth"}"#;
        let line_slow = br#"{"latency_ms":320,"service":"auth"}"#;

        assert!(!q.matches_line(line_fast, DetectedFormat::JsonLines));
        assert!(q.matches_line(line_slow, DetectedFormat::JsonLines));
    }

    #[test]
    fn parses_string_with_embedded_operators() {
        let q = Query::parse(r#"&.msg == "retry >= 3""#);
        assert!(matches!(q, Query::Structured(_)));

        let line_match = br#"{"msg":"retry >= 3","attempt":3}"#;
        let line_mismatch = br#"{"msg":"retry <= 2","attempt":2}"#;

        assert!(q.matches_line(line_match, DetectedFormat::JsonLines));
        assert!(!q.matches_line(line_mismatch, DetectedFormat::JsonLines));
    }
}
