//! Dot-selector query language for the `/` (search) and `&` (filter)
//! prompts on structured input, with a plain-regex fallback.
//!
//! Grammar (whitespace is free between tokens):
//!
//! ```text
//! expr   := and ( "||" and )*
//! and    := unary ( "&&" unary )*
//! unary  := "!" unary | "(" expr ")" | atom
//! atom   := path [ OP value ]          -- a bare path tests that the field exists
//! path   := ("." | "@") name ( "." name )*   -- nested JSON objects: .user.id
//! OP     := == != > >= < <= =~        -- =~ is a regex match on the value
//! value  := "string" | number | duration (250ms 2s 1m 1h 1.5s) | true | false | null | bare
//! ```
//!
//! e.g. `/.status >= 500`, `&.level == "error" && .service != "auth"`,
//! `&.ms > 250ms || .status >= 500`, `&!.user.id`, `&.msg =~ "timeout|refused"`.
//! Anything that does not parse as such an expression is a plain regex.

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
    /// `=~`: the value matches a regex.
    Matches,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    String(String),
    Number(f64),
    Boolean(bool),
    Null,
    DurationMillis(f64),
    /// `=~` operand, compiled once at parse time.
    Regex(RegexQuery),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectorPredicate {
    /// Dotted field path without the leading `.`/`@` (`user.id`).
    pub field_path: String,
    pub op: Operator,
    pub operand: Operand,
}

/// A boolean combination of field predicates.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Pred(SelectorPredicate),
    /// A bare path: true when the field is present and not `null`.
    Exists(String),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Regex(RegexQuery),
    Structured(Expr),
}

/// A plain-regex query: the source pattern plus its compiled form, built
/// lazily on first use and then reused for every line it is matched against
/// (a filter is evaluated per line on every redraw, so recompiling per line
/// dominated filtered draws). An invalid pattern caches `None` and matches
/// nothing.
#[derive(Debug, Clone)]
pub struct RegexQuery {
    pub source: String,
    case_insensitive: bool,
    compiled: std::cell::OnceCell<Option<regex::Regex>>,
}

impl RegexQuery {
    pub fn with_case(source: String, case_insensitive: bool) -> Self {
        RegexQuery {
            source,
            case_insensitive,
            compiled: std::cell::OnceCell::new(),
        }
    }

    pub fn regex(&self) -> Option<&regex::Regex> {
        self.compiled
            .get_or_init(|| {
                regex::RegexBuilder::new(&self.source)
                    .case_insensitive(self.case_insensitive)
                    .build()
                    .ok()
            })
            .as_ref()
    }

    pub fn is_match_bytes(&self, line: &[u8]) -> bool {
        match (self.regex(), std::str::from_utf8(line)) {
            (Some(re), Ok(s)) => re.is_match(s),
            _ => false,
        }
    }
}

/// Equality is defined by the source pattern and case mode alone; the
/// compiled cache is derived state.
impl PartialEq for RegexQuery {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.case_insensitive == other.case_insensitive
    }
}

thread_local! {
    /// Reusable span scratch for structured matching (one allocation per
    /// thread instead of one per evaluated line).
    static SPAN_SCRATCH: std::cell::RefCell<Vec<FieldSpan>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl Query {
    /// Parses a raw search or filter prompt string into either a selector
    /// expression or a plain (case-sensitive) regex.
    #[cfg(test)]
    pub fn parse(input: &str) -> Self {
        Self::parse_with_case(input, false)
    }

    /// As `parse`; a regex (the fallback, or an `=~` operand) folds case
    /// when `case_insensitive`.
    pub fn parse_with_case(input: &str, case_insensitive: bool) -> Self {
        let trimmed = input.trim();
        let body = trimmed
            .trim_start_matches('/')
            .trim_start_matches('&')
            .trim();
        if let Some(expr) = parse_expression(body, case_insensitive) {
            return Query::Structured(expr);
        }
        Query::Regex(RegexQuery::with_case(input.to_string(), case_insensitive))
    }

    /// Evaluates the query against a raw line and its format.
    #[cfg(test)]
    pub fn matches_line(&self, line: &[u8], format: DetectedFormat) -> bool {
        self.matches_fields(line, format, &[])
    }

    /// As `matches_line`; `columns` names the positional fields of a
    /// CSV/TSV line (its header row), so `.score > 90` works there too.
    pub fn matches_fields(&self, line: &[u8], format: DetectedFormat, columns: &[String]) -> bool {
        match self {
            Query::Regex(q) => q.is_match_bytes(line),
            Query::Structured(expr) => {
                if !format.is_structured() {
                    return false;
                }
                SPAN_SCRATCH.with(|scratch| {
                    let spans = &mut *scratch.borrow_mut();
                    StructuralClassifier::scan_fields(format, line, spans);
                    let ctx = Fields { format, columns };
                    evaluate(expr, line, spans, &ctx)
                })
            }
        }
    }
}

// ------------------------------------------------------------------ parser

struct Parser<'a> {
    s: &'a str,
    pos: usize,
    case_insensitive: bool,
}

/// Parses `body` as a complete selector expression, or `None` when it is
/// not one (the caller then treats the input as a regex).
fn parse_expression(body: &str, case_insensitive: bool) -> Option<Expr> {
    let first = body.chars().next()?;
    // Only inputs that start like a selector are tried: a regex such as
    // `ERROR|WARN` must never be reinterpreted.
    if !matches!(first, '.' | '@' | '!' | '(') && !body.starts_with("dur") {
        return None;
    }
    let mut p = Parser {
        s: body,
        pos: 0,
        case_insensitive,
    };
    let expr = p.parse_or()?;
    p.skip_ws();
    (p.pos == p.s.len()).then_some(expr)
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.s[self.pos..]
    }

    fn skip_ws(&mut self) {
        let trimmed = self.rest().trim_start();
        self.pos = self.s.len() - trimmed.len();
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_ws();
        if self.rest().starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    fn parse_or(&mut self) -> Option<Expr> {
        let mut terms = vec![self.parse_and()?];
        while self.eat("||") {
            terms.push(self.parse_and()?);
        }
        Some(if terms.len() == 1 {
            terms.pop().expect("one term")
        } else {
            Expr::Or(terms)
        })
    }

    fn parse_and(&mut self) -> Option<Expr> {
        let mut terms = vec![self.parse_unary()?];
        while self.eat("&&") {
            terms.push(self.parse_unary()?);
        }
        Some(if terms.len() == 1 {
            terms.pop().expect("one term")
        } else {
            Expr::And(terms)
        })
    }

    fn parse_unary(&mut self) -> Option<Expr> {
        self.skip_ws();
        if self.rest().starts_with('!') && !self.rest().starts_with("!=") {
            self.pos += 1;
            return Some(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if self.eat("(") {
            let inner = self.parse_or()?;
            return self.eat(")").then_some(inner);
        }
        self.parse_atom()
    }

    /// One predicate: the text up to the next top-level `&&`, `||` or `)`,
    /// quotes respected.
    fn parse_atom(&mut self) -> Option<Expr> {
        self.skip_ws();
        let rest = self.rest();
        let bytes = rest.as_bytes();
        let mut in_quote = false;
        let mut escaped = false;
        let mut end = bytes.len();
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
            } else if b == b')' || rest[i..].starts_with("&&") || rest[i..].starts_with("||") {
                end = i;
                break;
            }
            i += 1;
        }
        let text = rest[..end].trim();
        let expr = parse_atom_text(text, self.case_insensitive)?;
        self.pos += end;
        Some(expr)
    }
}

fn parse_atom_text(text: &str, case_insensitive: bool) -> Option<Expr> {
    if !(text.starts_with('.') || text.starts_with('@') || text.starts_with("dur")) {
        return None;
    }
    match find_operator(text) {
        Some((idx, len, op)) => {
            let field_path = normalize_path(text[..idx].trim())?;
            let raw_val = text[idx + len..].trim();
            let operand = if op == Operator::Matches {
                let src = unquote(raw_val).unwrap_or(raw_val).to_string();
                let re = RegexQuery::with_case(src, case_insensitive);
                re.regex()?;
                Operand::Regex(re)
            } else {
                parse_operand(raw_val)?
            };
            Some(Expr::Pred(SelectorPredicate {
                field_path,
                op,
                operand,
            }))
        }
        None => Some(Expr::Exists(normalize_path(text)?)),
    }
}

/// `.user.id` / `@user.id` / `dur` → `user.id`; `None` unless every
/// segment is a plain name (so a regex like `.*error` is not a path).
fn normalize_path(raw: &str) -> Option<String> {
    let path = raw.trim_start_matches('.').trim_start_matches('@');
    let valid = !path.is_empty()
        && path.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '$' | ':'))
        });
    valid.then(|| path.to_string())
}

/// The first comparison operator outside quotes: `(index, length, op)`.
fn find_operator(s: &str) -> Option<(usize, usize, Operator)> {
    let bytes = s.as_bytes();
    let mut in_quote = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_quote = false;
            }
            continue;
        }
        if b == b'"' {
            in_quote = true;
            continue;
        }
        let two = s.get(i..i + 2).unwrap_or("");
        let op = match two {
            ">=" => Some(Operator::GreaterThanOrEqual),
            "<=" => Some(Operator::LessThanOrEqual),
            "==" => Some(Operator::Equal),
            "!=" => Some(Operator::NotEqual),
            "=~" => Some(Operator::Matches),
            _ => None,
        };
        if let Some(op) = op {
            return Some((i, 2, op));
        }
        match b {
            b'>' => return Some((i, 1, Operator::GreaterThan)),
            b'<' => return Some((i, 1, Operator::LessThan)),
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> Option<&str> {
    (s.len() >= 2 && s.starts_with('"') && s.ends_with('"')).then(|| &s[1..s.len() - 1])
}

fn parse_operand(s: &str) -> Option<Operand> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    match s {
        "null" => return Some(Operand::Null),
        "true" => return Some(Operand::Boolean(true)),
        "false" => return Some(Operand::Boolean(false)),
        _ => {}
    }
    if let Some(inner) = unquote(s) {
        return Some(Operand::String(inner.replace("\\\"", "\"")));
    }
    if let Some(ms) = parse_duration_millis(s) {
        return Some(Operand::DurationMillis(ms));
    }
    if let Ok(num) = s.parse::<f64>() {
        return Some(Operand::Number(num));
    }
    // Bare string literal fallback
    Some(Operand::String(s.to_string()))
}

/// `250ms`, `2s`, `1.5s`, `1m`, `1h`, `300us` → milliseconds. A bare
/// number is not a duration here (see `value_as_millis`).
pub fn parse_duration_millis(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, factor) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix("us").or_else(|| s.strip_suffix("µs")) {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix("ns") {
        (n, 0.000_001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000.0)
    } else {
        (s.strip_suffix('h')?, 3_600_000.0)
    };
    let value: f64 = num.trim().parse().ok()?;
    value.is_finite().then_some(value * factor)
}

/// A field value as milliseconds: a bare number is taken as milliseconds,
/// a duration literal (`"1.5s"`) is converted.
fn value_as_millis(s: &str) -> Option<f64> {
    s.parse::<f64>().ok().or_else(|| parse_duration_millis(s))
}

// --------------------------------------------------------------- evaluator

/// How field names resolve on one line.
struct Fields<'a> {
    format: DetectedFormat,
    /// Header names of a positional (CSV/TSV) line.
    columns: &'a [String],
}

fn evaluate(expr: &Expr, line: &[u8], spans: &[FieldSpan], ctx: &Fields) -> bool {
    match expr {
        Expr::Pred(pred) => evaluate_predicate(pred, line, spans, ctx),
        Expr::Exists(path) => lookup(path, line, spans, ctx, &mut |_, span| {
            span.val_type != FieldType::Null
        }),
        Expr::Not(inner) => !evaluate(inner, line, spans, ctx),
        Expr::And(terms) => terms.iter().all(|t| evaluate(t, line, spans, ctx)),
        Expr::Or(terms) => terms.iter().any(|t| evaluate(t, line, spans, ctx)),
    }
}

/// Finds the field at `path` and applies `f(bytes, span)` to it; false when
/// the field is absent. A dotted path is first tried as a literal key (logfmt
/// keys such as `http.status`), then walked through nested JSON objects.
fn lookup(
    path: &str,
    line: &[u8],
    spans: &[FieldSpan],
    ctx: &Fields,
    f: &mut dyn FnMut(&[u8], &FieldSpan) -> bool,
) -> bool {
    if matches!(ctx.format, DetectedFormat::Csv | DetectedFormat::Tsv) {
        return match ctx.columns.iter().position(|c| c == path) {
            Some(i) => spans.get(i).is_some_and(|span| f(line, span)),
            None => false,
        };
    }
    let format = ctx.format;
    let key_of = |span: &FieldSpan| -> &[u8] {
        let k = span.key(line);
        k.strip_prefix(b"\"")
            .and_then(|k| k.strip_suffix(b"\""))
            .unwrap_or(k)
    };
    if let Some(span) = spans.iter().find(|s| key_of(s) == path.as_bytes()) {
        return f(line, span);
    }
    if format != DetectedFormat::JsonLines {
        return false;
    }
    let Some((head, tail)) = path.split_once('.') else {
        return false;
    };
    let Some(span) = spans
        .iter()
        .find(|s| key_of(s) == head.as_bytes() && s.val_type == FieldType::Object)
    else {
        return false;
    };
    let inner = span.value(line);
    let mut inner_spans = Vec::new();
    StructuralClassifier::scan_json_line(inner, &mut inner_spans);
    lookup(tail, inner, &inner_spans, ctx, f)
}

fn evaluate_predicate(
    pred: &SelectorPredicate,
    line: &[u8],
    spans: &[FieldSpan],
    ctx: &Fields,
) -> bool {
    let mut found = false;
    let mut result = false;
    lookup(&pred.field_path, line, spans, ctx, &mut |line, span| {
        found = true;
        result = compare(pred, span, span.value(line));
        result
    });
    if !found {
        // A missing field only satisfies `== null`: a line without the
        // field is not evidence for any comparison (`.level != "INFO"`
        // keeps lines that *have* another level).
        return pred.op == Operator::Equal && pred.operand == Operand::Null;
    }
    result
}

fn compare(pred: &SelectorPredicate, span: &FieldSpan, raw_val: &[u8]) -> bool {
    let lossy = String::from_utf8_lossy(raw_val);
    let val_str = lossy.trim_matches('"');
    let ord = |a: f64, b: f64| match pred.op {
        Operator::Equal => (a - b).abs() < f64::EPSILON,
        Operator::NotEqual => (a - b).abs() >= f64::EPSILON,
        Operator::GreaterThan => a > b,
        Operator::GreaterThanOrEqual => a >= b,
        Operator::LessThan => a < b,
        Operator::LessThanOrEqual => a <= b,
        Operator::Matches => false,
    };
    match &pred.operand {
        Operand::Null => {
            let is_null = span.val_type == FieldType::Null;
            match pred.op {
                Operator::Equal => is_null,
                Operator::NotEqual => !is_null,
                _ => false,
            }
        }
        Operand::String(target) => match pred.op {
            Operator::Equal => val_str == target,
            Operator::NotEqual => val_str != target,
            Operator::GreaterThan => val_str > target.as_str(),
            Operator::GreaterThanOrEqual => val_str >= target.as_str(),
            Operator::LessThan => val_str < target.as_str(),
            Operator::LessThanOrEqual => val_str <= target.as_str(),
            Operator::Matches => false,
        },
        Operand::Regex(re) => re.regex().is_some_and(|r| r.is_match(val_str)),
        Operand::Number(target) => match val_str.parse::<f64>() {
            Ok(num) => ord(num, *target),
            Err(_) => false,
        },
        Operand::Boolean(target) => {
            let b = raw_val == b"true";
            match pred.op {
                Operator::Equal => b == *target,
                Operator::NotEqual => b != *target,
                _ => false,
            }
        }
        Operand::DurationMillis(target) => match value_as_millis(val_str) {
            Some(ms) => ord(ms, *target),
            None => false,
        },
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

    fn jl(q: &str, line: &str) -> bool {
        Query::parse(q).matches_line(line.as_bytes(), DetectedFormat::JsonLines)
    }

    #[test]
    fn boolean_operators_with_precedence_and_parentheses() {
        let line = r#"{"level":"ERROR","service":"auth","ms":320}"#;
        assert!(jl(r#".level == "ERROR" && .service == "auth""#, line));
        assert!(!jl(r#".level == "ERROR" && .service == "cart""#, line));
        assert!(jl(r#".level == "INFO" || .ms > 300"#, line));
        // && binds tighter than ||.
        assert!(jl(
            r#".level == "INFO" && .ms > 0 || .service == "auth""#,
            line
        ));
        assert!(!jl(
            r#".level == "INFO" && (.ms > 0 || .service == "auth")"#,
            line
        ));
        assert!(jl(r#"!(.level == "INFO")"#, line));
        assert!(!jl(r#"!.level == "ERROR""#, line));
        // Operators inside quotes are text.
        assert!(!jl(r#".service == "a && b""#, line));
    }

    #[test]
    fn nested_paths_and_existence_tests() {
        let line = r#"{"user":{"id":7,"geo":{"cc":"JP"}},"trace":null}"#;
        assert!(jl(".user.id == 7", line));
        assert!(jl(r#".user.geo.cc == "JP""#, line));
        assert!(!jl(".user.id == 8", line));
        assert!(jl(".user.geo", line), "a bare path tests existence");
        assert!(!jl(".user.missing", line));
        assert!(!jl(".trace", line), "null does not count as present");
        assert!(jl("!.trace", line));
        // A logfmt key may itself contain dots.
        let q = Query::parse(".http.status >= 500");
        assert!(q.matches_line(b"http.status=502 path=/x", DetectedFormat::Logfmt));
    }

    #[test]
    fn regex_match_strings_and_durations() {
        let line = r#"{"msg":"connection Refused by db","ts":"2026-08-23T00:10:00Z","dur":"1.5s","ms":1500}"#;
        assert!(
            !jl(r#".msg =~ "timeout|refused""#, line),
            "case-sensitive by default"
        );
        assert!(
            Query::parse_with_case(r#".msg =~ "timeout|refused""#, true)
                .matches_line(line.as_bytes(), DetectedFormat::JsonLines)
        );
        assert!(jl(".msg =~ ^connection", line));
        assert!(
            jl(r#".ts >= "2026-08-23T00:05""#, line),
            "text compares in order"
        );
        assert!(!jl(r#".ts < "2026-08-23T00:05""#, line));
        assert!(jl(".dur > 1s", line), "a duration value converts");
        assert!(
            jl(".ms == 1.5s", line),
            "a bare number counts as milliseconds"
        );
        assert!(!jl(".dur > 2s", line));
    }

    #[test]
    fn non_selector_input_stays_a_regex() {
        for src in [
            "(ERROR|WARN)",
            ".*timeout",
            "!DEBUG",
            "ERROR && WARN",
            ".",
            "(.a == 1",
        ] {
            assert!(matches!(Query::parse(src), Query::Regex(_)), "{src}");
        }
        // An invalid `=~` regex does not make a selector.
        assert!(matches!(Query::parse(".msg =~ \"(\""), Query::Regex(_)));
    }
}
