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
    Regex(RegexQuery),
    /// Alternatives joined by `||`, each a list of predicates joined by
    /// `&&`: a line matches when every predicate of any one group holds.
    Structured(Vec<Vec<SelectorPredicate>>),
}

/// A plain-regex query: the source pattern plus its compiled form, built
/// lazily on first use and then reused for every line it is matched against
/// (a filter is evaluated per line on every redraw, so recompiling per line
/// dominated filtered draws). An invalid pattern caches `None` and matches
/// nothing, exactly as before.
#[derive(Debug, Clone)]
pub struct RegexQuery {
    pub source: String,
    ignore_case: bool,
    compiled: std::cell::OnceCell<Option<regex::Regex>>,
}

impl RegexQuery {
    pub fn new(source: String, ignore_case: bool) -> Self {
        RegexQuery {
            source,
            ignore_case,
            compiled: std::cell::OnceCell::new(),
        }
    }

    fn regex(&self) -> Option<&regex::Regex> {
        self.compiled
            .get_or_init(|| {
                regex::RegexBuilder::new(&self.source)
                    .case_insensitive(self.ignore_case)
                    .build()
                    .ok()
            })
            .as_ref()
    }
}

/// Equality is defined by the source pattern and case rule alone; the
/// compiled cache is derived state.
impl PartialEq for RegexQuery {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.ignore_case == other.ignore_case
    }
}

thread_local! {
    /// Reusable span scratch for structured matching (one allocation per
    /// thread instead of one per evaluated line).
    static SPAN_SCRATCH: std::cell::RefCell<Vec<FieldSpan>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Maps a field name to its position for formats whose fields are
/// positional (CSV/TSV: the header row names the columns).
pub type ColumnLookup<'a> = &'a dyn Fn(&str) -> Option<usize>;

#[cfg(test)]
fn no_columns(_: &str) -> Option<usize> {
    None
}

impl Query {
    /// Parses a raw search or filter prompt string into either a JQ
    /// dot-selector Query or a plain (case-sensitive) regex.
    pub fn parse(input: &str) -> Self {
        Self::parse_with_case(input, false)
    }

    /// Like `parse`; a plain-regex query ignores letter case when
    /// `ignore_case` is set (`-i`/`-I`).
    pub fn parse_with_case(input: &str, ignore_case: bool) -> Self {
        let trimmed = input.trim();
        let body = trimmed
            .trim_start_matches('/')
            .trim_start_matches('&')
            .trim();
        if let Some(groups) = parse_predicate_groups(body) {
            return Query::Structured(groups);
        }
        Query::Regex(RegexQuery::new(input.to_string(), ignore_case))
    }

    /// `input` as a plain regex, even when it looks like a selector (on
    /// plain text, `.status >= 500` is text to find, as in less).
    pub fn regex(input: &str, ignore_case: bool) -> Self {
        Query::Regex(RegexQuery::new(input.to_string(), ignore_case))
    }

    /// False for a plain-regex query whose pattern does not compile (such a
    /// query matches nothing, so applying it as a filter would blank the
    /// view without saying why).
    pub fn is_valid(&self) -> bool {
        match self {
            Query::Regex(q) => q.regex().is_some(),
            Query::Structured(_) => true,
        }
    }

    /// Evaluates the query against a raw line and its format.
    #[cfg(test)]
    pub fn matches_line(&self, line: &[u8], format: DetectedFormat) -> bool {
        self.matches_line_with(line, format, &no_columns)
    }

    /// `matches_line`, resolving CSV/TSV field names through `columns`
    /// (those formats have no keys in the line itself).
    pub fn matches_line_with(
        &self,
        line: &[u8],
        format: DetectedFormat,
        columns: ColumnLookup,
    ) -> bool {
        match self {
            Query::Regex(q) => {
                if let Some(re) = q.regex()
                    && let Ok(s) = std::str::from_utf8(line)
                {
                    return re.is_match(s);
                }
                false
            }
            Query::Structured(groups) => {
                if !format.is_structured() {
                    return false;
                }
                SPAN_SCRATCH.with(|scratch| {
                    let spans = &mut *scratch.borrow_mut();
                    StructuralClassifier::scan_fields(format, line, spans);
                    groups.iter().any(|preds| {
                        preds
                            .iter()
                            .all(|pred| evaluate_predicate(pred, line, spans, format, columns))
                    })
                })
            }
        }
    }
}

/// Splits `s` on the two-character `sep` wherever it is outside a quoted
/// string.
fn split_unquoted<'a>(s: &'a str, sep: &str) -> Vec<&'a str> {
    let bytes = s.as_bytes();
    let sep = sep.as_bytes();
    let mut parts = Vec::new();
    let mut in_quote = false;
    let mut escaped = false;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_quote = false;
            }
        } else if b == b'"' {
            in_quote = true;
        } else if bytes[i..].starts_with(sep) {
            parts.push(&s[start..i]);
            i += sep.len();
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(&s[start..]);
    parts
}

/// `.a == 1 && .b > 2 || .c == "x"`: `&&` binds tighter than `||`. Every
/// operand must be a field predicate, or the whole input is not a selector
/// (it is then searched as a regex, as in less).
fn parse_predicate_groups(body: &str) -> Option<Vec<Vec<SelectorPredicate>>> {
    let looks_like_selector =
        |p: &str| p.starts_with('.') || p.starts_with('@') || p.starts_with("dur");
    let mut groups = Vec::new();
    for alt in split_unquoted(body, "||") {
        let mut group = Vec::new();
        for part in split_unquoted(alt, "&&") {
            let part = part.trim();
            if !looks_like_selector(part) {
                return None;
            }
            group.push(parse_single_predicate(part)?);
        }
        groups.push(group);
    }
    Some(groups)
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

/// The value of `path` in `line`: a top-level key (JSON lines, logfmt),
/// a header-named column (CSV/TSV, via `columns`), or a dotted path into
/// nested JSON objects (`user.id`).
#[inline]
fn lookup_field<'l>(
    path: &str,
    line: &'l [u8],
    spans: &[FieldSpan],
    format: DetectedFormat,
    columns: ColumnLookup,
) -> Option<(&'l [u8], FieldType)> {
    if matches!(format, DetectedFormat::Csv | DetectedFormat::Tsv) {
        let idx = columns(path).or_else(|| {
            // `col_3` names the third column of a header-less file.
            path.strip_prefix("col_")?
                .parse::<usize>()
                .ok()?
                .checked_sub(1)
        })?;
        let span = spans.get(idx)?;
        return Some((span.value(line), span.val_type));
    }
    match spans.iter().find(|span| key_is(span, line, path)) {
        Some(span) => Some((span.value(line), span.val_type)),
        None if format == DetectedFormat::JsonLines && path.contains('.') => {
            lookup_nested(path, line, spans)
        }
        None => None,
    }
}

#[inline]
fn key_is(span: &FieldSpan, line: &[u8], key: &str) -> bool {
    std::str::from_utf8(span.key(line)).is_ok_and(|k| k.trim_matches('"') == key)
}

/// `a.b.c` in a JSON line: the value of `b.c` inside the object at `a`.
#[cold]
fn lookup_nested<'l>(
    path: &str,
    line: &'l [u8],
    spans: &[FieldSpan],
) -> Option<(&'l [u8], FieldType)> {
    let (head, rest) = path.split_once('.')?;
    let span = spans
        .iter()
        .find(|span| span.val_type == FieldType::Object && key_is(span, line, head))?;
    let start = span.val_offset as usize;
    let inner = &line[start..start + span.val_len as usize];
    let mut inner_spans = Vec::new();
    StructuralClassifier::scan_json_line(inner, &mut inner_spans);
    if let Some(span) = inner_spans.iter().find(|span| key_is(span, inner, rest)) {
        return Some((span.value(inner), span.val_type));
    }
    lookup_nested(rest, inner, &inner_spans)
}

fn evaluate_predicate(
    pred: &SelectorPredicate,
    line: &[u8],
    spans: &[FieldSpan],
    format: DetectedFormat,
    columns: ColumnLookup,
) -> bool {
    let target = lookup_field(&pred.field_path, line, spans, format, columns);

    match &pred.operand {
        Operand::Null => {
            let exists = target.is_some_and(|(_, ty)| ty != FieldType::Null);
            match pred.op {
                Operator::Equal => !exists,
                Operator::NotEqual => exists,
                _ => false,
            }
        }
        Operand::String(target_s) => {
            if let Some((raw_val, _)) = target {
                let lossy = String::from_utf8_lossy(raw_val);
                let val_str = lossy.trim_matches('"');
                match pred.op {
                    Operator::Equal => val_str == target_s,
                    Operator::NotEqual => val_str != target_s,
                    Operator::Contains => val_str.contains(target_s),
                    _ => false,
                }
            } else {
                false
            }
        }
        Operand::Number(target_n) => {
            if let Some((raw_val, _)) = target
                && let Ok(s) = std::str::from_utf8(raw_val)
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
            false
        }
        Operand::Boolean(target_b) => {
            if let Some((raw_val, _)) = target {
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
            if let Some((raw_val, _)) = target
                && let Ok(s) = std::str::from_utf8(raw_val)
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
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_query_matches_reuse_compiled_pattern_and_keep_semantics() {
        let q = Query::parse("ERR(OR)?");
        assert!(matches!(q, Query::Regex(_)));
        for _ in 0..3 {
            assert!(q.matches_line(b"an ERROR here", DetectedFormat::JsonLines));
            assert!(q.matches_line(b"ERR", DetectedFormat::PlainText));
            assert!(!q.matches_line(b"info", DetectedFormat::PlainText));
        }
        // Non-UTF-8 lines never match a regex filter.
        assert!(!q.matches_line(b"ERROR \xff", DetectedFormat::PlainText));
        // An invalid pattern matches nothing (and keeps doing so).
        let bad = Query::parse("(unclosed");
        assert!(!bad.matches_line(b"(unclosed", DetectedFormat::PlainText));
        assert!(!bad.matches_line(b"(unclosed", DetectedFormat::PlainText));
        // Equality is by source pattern, independent of compile state.
        assert_eq!(Query::parse("ERR(OR)?"), q);
        assert_ne!(Query::parse("ERR"), q);
    }

    #[test]
    fn structured_queries_on_all_formats() {
        let q = Query::parse("&.status >= 500");
        assert!(q.matches_line(b"status=503 msg=x", DetectedFormat::Logfmt));
        assert!(!q.matches_line(b"status=200 msg=x", DetectedFormat::Logfmt));
        assert!(!q.matches_line(b"status=503", DetectedFormat::PlainText));
        let s = Query::parse(r#"&.level != "INFO""#);
        assert!(s.matches_line(br#"{"level":"WARN"}"#, DetectedFormat::JsonLines));
        assert!(!s.matches_line(br#"{"level":"INFO"}"#, DetectedFormat::JsonLines));
        assert!(!s.matches_line(br#"{"other":"x"}"#, DetectedFormat::JsonLines));
    }

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

    #[test]
    fn and_or_combine_predicates_with_and_binding_tighter() {
        let q = Query::parse(r#"&.level == "ERROR" && .ms > 250 || .status >= 500"#);
        let Query::Structured(groups) = &q else {
            panic!("expected a selector query");
        };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        let f = DetectedFormat::JsonLines;
        assert!(q.matches_line(br#"{"level":"ERROR","ms":300}"#, f));
        assert!(!q.matches_line(br#"{"level":"ERROR","ms":100}"#, f));
        assert!(q.matches_line(br#"{"level":"INFO","status":503}"#, f));
        assert!(!q.matches_line(br#"{"level":"INFO","status":200}"#, f));
    }

    #[test]
    fn operators_inside_quotes_do_not_split_predicates() {
        let q = Query::parse(r#".msg == "a && b || c""#);
        let Query::Structured(groups) = &q else {
            panic!("expected a selector query");
        };
        assert_eq!(groups.len(), 1);
        assert!(q.matches_line(br#"{"msg":"a && b || c"}"#, DetectedFormat::JsonLines));
    }

    #[test]
    fn a_non_selector_operand_makes_the_whole_input_a_regex() {
        assert!(matches!(Query::parse(".a == 1 && plain"), Query::Regex(_)));
    }

    #[test]
    fn dotted_paths_reach_into_nested_json_objects() {
        let q = Query::parse(".user.id == 42");
        let f = DetectedFormat::JsonLines;
        assert!(q.matches_line(br#"{"user":{"id":42,"name":"a"},"x":1}"#, f));
        assert!(!q.matches_line(br#"{"user":{"id":7},"x":1}"#, f));
        assert!(!q.matches_line(br#"{"user":"42"}"#, f));
        let deep = Query::parse(r#".a.b.c == "z""#);
        assert!(deep.matches_line(br#"{"a":{"b":{"c":"z"}}}"#, f));
        // A literal dotted key still wins over a nested lookup.
        let literal = Query::parse(".a.b == 1");
        assert!(literal.matches_line(br#"{"a.b":1}"#, f));
    }

    #[test]
    fn csv_fields_are_found_by_header_name_or_position() {
        let names = ["id", "status", "path"];
        let lookup = |name: &str| names.iter().position(|n| *n == name);
        let q = Query::parse(".status >= 500");
        assert!(q.matches_line_with(b"1,503,/a", DetectedFormat::Csv, &lookup));
        assert!(!q.matches_line_with(b"2,200,/b", DetectedFormat::Csv, &lookup));
        let by_pos = Query::parse(".col_3 == /b");
        assert!(by_pos.matches_line(b"2,200,/b", DetectedFormat::Csv));
        assert!(by_pos.matches_line(b"2\t200\t/b", DetectedFormat::Tsv));
    }

    #[test]
    fn regex_queries_can_ignore_case() {
        let q = Query::parse_with_case("error", true);
        assert!(q.matches_line(b"an ERROR here", DetectedFormat::PlainText));
        assert_ne!(q, Query::parse("error"));
        assert!(!Query::parse("error").matches_line(b"ERROR", DetectedFormat::PlainText));
    }
}
