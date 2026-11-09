//! Prompt-row format-string logic for the three prompt templates (default
//! short `:`, `-m` medium, `-M` long — quoted verbatim in
//! docs/RECONSTRUCTION_SPEC.md §2 `prompt-short-colon`/
//! `prompt-byte-percent-rules`), the special-message override path, and the
//! left-truncation rule (§3 item 4) applied when the rendered text exceeds
//! terminal width.
//!
//! A generic `?`/`%` escape interpreter was considered (per §4's
//! architecture note) but is not implemented: only three concrete format
//! strings are ever produced, and hand-writing their conditional segments
//! directly is both simpler and easier to verify byte-for-byte against the
//! captured evidence than a general parser for syntax no other format
//! string in scope exercises.

pub use crate::cli::PromptMode as Mode;
use unicode_width::UnicodeWidthChar;

pub struct PromptContext<'a> {
    pub filename: Option<&'a str>,
    /// `?n`: true only for the first prompt drawn for the current file.
    pub show_filename: bool,
    pub file_index: usize,
    pub file_count: usize,
    pub top_line: usize,
    pub bottom_line: usize,
    pub total_lines: Option<usize>,
    pub at_eof: bool,
    pub next_filename: Option<&'a str>,
    pub byte_offset: usize,
    pub total_bytes: Option<usize>,
    /// Active filter / sort state (`[&ERROR, 42 lines]`), shown first.
    /// `None` keeps the less-compatible prompt exactly.
    pub view_tag: Option<&'a str>,
}

/// Renders the full prompt row: `pending` (a transient message such as
/// "Pattern not found  (press RETURN)") takes over the entire row when
/// present, regardless of `mode`. Returns `(text, highlighted)`, already
/// left-truncated to `width` if needed. `highlighted` is false only for the
/// bare `:` fallback (`prompt-initial-filename`: the plain colon prompt
/// carries zero attrs; every other row shown here does).
pub fn render(
    pending: Option<&str>,
    mode: Mode,
    ctx: &PromptContext,
    width: usize,
) -> (String, bool) {
    let (text, highlighted) = if let Some(msg) = pending {
        (msg.to_string(), true)
    } else {
        let joined = build_segments(mode, ctx).join(" ");
        if joined.is_empty() {
            (":".to_string(), false)
        } else {
            (joined, true)
        }
    };
    (truncate_left(&text, width), highlighted)
}

fn build_segments(mode: Mode, ctx: &PromptContext) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(tag) = ctx.view_tag {
        parts.push(tag.to_string());
    }
    match mode {
        Mode::Short => {
            if ctx.show_filename {
                if let Some(f) = ctx.filename {
                    parts.push(f.to_string());
                }
                if ctx.file_count > 1 {
                    parts.push(format!("(file {} of {})", ctx.file_index, ctx.file_count));
                }
            }
            push_eof_or_nothing(&mut parts, ctx);
        }
        Mode::Medium => {
            if ctx.show_filename {
                if let Some(f) = ctx.filename {
                    parts.push(f.to_string());
                }
                if ctx.file_count > 1 {
                    parts.push(format!("(file {} of {})", ctx.file_index, ctx.file_count));
                }
            }
            if ctx.at_eof {
                push_eof_or_nothing(&mut parts, ctx);
            } else {
                parts.push(percent_or_byte(ctx));
            }
        }
        Mode::Long => {
            if let Some(f) = ctx.filename {
                parts.push(f.to_string());
            }
            if ctx.show_filename && ctx.file_count > 1 {
                parts.push(format!("(file {} of {})", ctx.file_index, ctx.file_count));
            }
            if let Some(total) = ctx.total_lines {
                parts.push(format!(
                    "lines {}-{}/{}",
                    ctx.top_line, ctx.bottom_line, total
                ));
            }
            if ctx.at_eof {
                parts.push("(END)".to_string());
                if let Some(next) = ctx.next_filename {
                    parts.push(format!("- Next: {}", next));
                }
            } else {
                parts.push(percent_only(ctx));
            }
        }
    }
    parts
}

/// `?e(END) ?x- Next\: %x.` — used identically by short and medium modes.
fn push_eof_or_nothing(parts: &mut Vec<String>, ctx: &PromptContext) {
    if ctx.at_eof {
        parts.push("(END)".to_string());
        if let Some(next) = ctx.next_filename {
            parts.push(format!("- Next: {}", next));
        }
    }
}

fn percent_of(ctx: &PromptContext) -> Option<u64> {
    let total = ctx.total_lines?;
    if total == 0 {
        return Some(0);
    }
    Some((ctx.bottom_line.min(total) as u64 * 100) / total as u64)
}

/// `:?pB%pB\%:byte %bB?s/%s..` (medium): percent when known, else a byte
/// fallback. No baseline scenario exercises the byte branch (percent is
/// always known once `total_lines` has been read), so it is implemented
/// per the declared format string but is not independently capture-verified.
fn percent_or_byte(ctx: &PromptContext) -> String {
    match percent_of(ctx) {
        Some(p) => format!("{}%", p),
        None => match ctx.total_bytes {
            Some(total) => format!("byte {}/{}", ctx.byte_offset, total),
            None => format!("byte {}", ctx.byte_offset),
        },
    }
}

/// `?pB%pB\%..` (long): same percent rule, no byte fallback observed in the
/// long format string's trailing branch (untested edge, kept minimal).
fn percent_only(ctx: &PromptContext) -> String {
    match percent_of(ctx) {
        Some(p) => format!("{}%", p),
        None => String::new(),
    }
}

/// §3 item 4: prompt truncation drops LEADING characters, keeping the
/// tail. Counted in display columns, so a double-width character counts
/// twice and a wide filename cannot push the row past `width` (for ASCII
/// this is the same as counting characters).
fn truncate_left(text: &str, width: usize) -> String {
    let char_width = |c: char| UnicodeWidthChar::width(c).unwrap_or(0);
    if text.chars().map(char_width).sum::<usize>() <= width {
        return text.to_string();
    }
    let mut used = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        let w = char_width(c);
        if used + w > width {
            break;
        }
        used += w;
        start = i;
    }
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_ctx() -> PromptContext<'static> {
        PromptContext {
            filename: Some("parity/fixtures/lines200.txt"),
            show_filename: true,
            file_index: 1,
            file_count: 1,
            top_line: 1,
            bottom_line: 23,
            total_lines: Some(200),
            at_eof: false,
            next_filename: None,
            byte_offset: 0,
            total_bytes: None,
            view_tag: None,
        }
    }

    #[test]
    fn plain_colon_prompt_has_zero_attrs_and_bare_colon_text() {
        let mut ctx = base_ctx();
        ctx.show_filename = false;
        let (text, highlighted) = render(None, Mode::Short, &ctx, 80);
        assert_eq!(text, ":");
        assert!(!highlighted);
    }

    #[test]
    fn first_prompt_shows_filename_highlighted() {
        let ctx = base_ctx();
        let (text, highlighted) = render(None, Mode::Short, &ctx, 80);
        assert_eq!(text, "parity/fixtures/lines200.txt");
        assert!(highlighted);
    }

    #[test]
    fn short_prompt_end_marker_alone_when_not_first_prompt() {
        let mut ctx = base_ctx();
        ctx.show_filename = false;
        ctx.at_eof = true;
        ctx.top_line = 178;
        ctx.bottom_line = 200;
        let (text, _) = render(None, Mode::Short, &ctx, 80);
        assert_eq!(text, "(END)");
    }

    #[test]
    fn multi_file_indicator_joins_with_filename_and_next() {
        let mut ctx = base_ctx();
        ctx.filename = Some("parity/fixtures/multi-1.txt");
        ctx.file_count = 3;
        ctx.at_eof = true;
        ctx.next_filename = Some("parity/fixtures/multi-2.txt");
        let (text, _) = render(None, Mode::Short, &ctx, 200);
        assert_eq!(
            text,
            "parity/fixtures/multi-1.txt (file 1 of 3) (END) - Next: parity/fixtures/multi-2.txt"
        );
    }

    #[test]
    fn medium_prompt_percent_matches_capture_evidence() {
        let ctx = base_ctx();
        let (text, _) = render(None, Mode::Medium, &ctx, 80);
        assert_eq!(text, "parity/fixtures/lines200.txt 11%");

        let mut mid = base_ctx();
        mid.show_filename = false;
        mid.top_line = 50;
        mid.bottom_line = 72;
        let (text2, _) = render(None, Mode::Medium, &mid, 80);
        assert_eq!(text2, "36%");

        let mut eof = base_ctx();
        eof.show_filename = false;
        eof.at_eof = true;
        eof.top_line = 178;
        eof.bottom_line = 200;
        let (text3, _) = render(None, Mode::Medium, &eof, 80);
        assert_eq!(text3, "(END)");
    }

    #[test]
    fn long_prompt_lines_segment_persists_at_eof_unlike_percent() {
        let mut eof = base_ctx();
        eof.top_line = 178;
        eof.bottom_line = 200;
        eof.at_eof = true;
        let (text, _) = render(None, Mode::Long, &eof, 80);
        assert_eq!(text, "parity/fixtures/lines200.txt lines 178-200/200 (END)");

        let mut mid = base_ctx();
        mid.show_filename = false;
        mid.top_line = 50;
        mid.bottom_line = 72;
        let (text2, _) = render(None, Mode::Long, &mid, 80);
        assert_eq!(text2, "parity/fixtures/lines200.txt lines 50-72/200 36%");
    }

    #[test]
    fn pending_message_overrides_whatever_mode_and_is_highlighted() {
        let ctx = base_ctx();
        let (text, highlighted) = render(
            Some("Cannot seek to line number 9999  (press RETURN)"),
            Mode::Long,
            &ctx,
            80,
        );
        assert_eq!(text, "Cannot seek to line number 9999  (press RETURN)");
        assert!(highlighted);
    }

    #[test]
    fn long_prompt_truncates_from_the_left_when_too_wide() {
        let mut ctx = base_ctx();
        ctx.filename = Some("parity/fixtures/multi-1.txt");
        ctx.file_count = 3;
        ctx.at_eof = true;
        ctx.next_filename = Some("parity/fixtures/multi-2.txt");
        let (text, _) = render(None, Mode::Short, &ctx, 80);
        assert_eq!(text.chars().count(), 80);
        assert_eq!(
            text,
            "ity/fixtures/multi-1.txt (file 1 of 3) (END) - Next: parity/fixtures/multi-2.txt"
        );
    }

    #[test]
    fn truncation_counts_display_columns_for_wide_characters() {
        // 10 double-width characters = 20 columns; keep the tail that fits.
        let text = "日本語のファイル名です";
        let cut = truncate_left(text, 9);
        assert_eq!(cut, "ル名です");
        assert!(truncate_left("abc", 3) == "abc");
        assert_eq!(truncate_left("abcdef", 4), "cdef");
    }

    #[test]
    fn short_text_is_not_truncated() {
        let mut ctx = base_ctx();
        ctx.filename = Some("parity/fixtures/multi-3.txt");
        ctx.file_count = 3;
        ctx.file_index = 3;
        ctx.at_eof = true;
        let (text, _) = render(None, Mode::Short, &ctx, 80);
        assert_eq!(text, "parity/fixtures/multi-3.txt (file 3 of 3) (END)");
    }
}
