//! Pure rendering: turns a window of `buffer` lines plus `Options` into a
//! grid of display rows, each row a sequence of styled spans.
//!
//! Owns: tab expansion (8-column stops), caret-notation / hex-fallback
//! control-char display, `-S` chop-with-`>`-marker, default wrap (no
//! continuation indicator), `-R` ANSI/OSC8 passthrough with the malformed-
//! sequence silent-drop rule, and `unicode-width`-based column accounting.

use crate::buffer::Buffer;
use regex::Regex;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub reverse: bool,
}

pub type DisplayRow = Vec<Span>;

#[derive(Debug, Clone)]
pub struct RenderOpts {
    pub width: usize,
    pub line_numbers: bool,
    pub chop: bool,
    pub raw_ansi: bool,
    /// Display columns cut from the start of every line (horizontal
    /// scroll, `ESC-)`/RIGHTARROW). Line numbers are not shifted.
    pub hshift: usize,
}

/// One content-producing unit inside a display row, before spans are
/// coalesced by reverse/non-reverse runs.
struct Piece<'a> {
    text: PieceText<'a>,
    width: usize,
    reverse: bool,
}

/// A piece's text, kept allocation-free for every common case (a character
/// borrowed from the line, a tab's spaces, a short caret/hex token); only a
/// raw escape sequence holding invalid UTF-8 needs an owned lossy copy.
/// Building one `String` per character used to make the allocator the
/// dominant cost of rendering.
enum PieceText<'a> {
    Borrowed(&'a str),
    Spaces(usize),
    Inline([u8; 4], u8),
    Owned(String),
}

impl PieceText<'_> {
    fn inline(s: &str) -> Self {
        let mut buf = [0u8; 4];
        buf[..s.len()].copy_from_slice(s.as_bytes());
        PieceText::Inline(buf, s.len() as u8)
    }

    fn push_to(&self, out: &mut String) {
        match self {
            PieceText::Borrowed(s) => out.push_str(s),
            PieceText::Spaces(n) => out.extend(std::iter::repeat_n(' ', *n)),
            PieceText::Inline(buf, len) => out.push_str(
                std::str::from_utf8(&buf[..*len as usize]).expect("inline text is ASCII"),
            ),
            PieceText::Owned(s) => out.push_str(s),
        }
    }
}

/// LESSANSIMIDCHARS default set: digits, `:`, `;`, `[`, `?`, `!`, `"`, `'`,
/// `#`, `%`, `(`, `)`, `*`, `+`, space. LESSANSIENDCHARS default: `m` only.
fn is_ansi_midchar(b: u8) -> bool {
    b.is_ascii_digit()
        || matches!(
            b,
            b':' | b';'
                | b'['
                | b'?'
                | b'!'
                | b'"'
                | b'\''
                | b'#'
                | b'%'
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b' '
        )
}

/// Scans one ESC-prefixed sequence starting at `bytes[i]` (`bytes[i] ==
/// 0x1b`). Returns `(end_index, raw_bytes_if_kept)`: `end_index` is the byte
/// index just past the sequence; `raw_bytes_if_kept` is `Some` for a valid
/// CSI-color (`ESC[...m`) or OSC 8 sequence that must be passed through raw,
/// and `None` for a malformed near-ANSI sequence that must be silently
/// dropped (`opt-raw-control-chars`: `ESC[2;5H` vanishes entirely — neither
/// raw nor caret-escaped, see docs/RECONSTRUCTION_SPEC.md §3 item 6).
fn scan_escape(bytes: &[u8], i: usize) -> Option<(usize, Option<&[u8]>)> {
    debug_assert_eq!(bytes[i], 0x1b);
    if bytes.get(i + 1) == Some(&b'[') {
        let mut j = i + 2;
        while j < bytes.len() && is_ansi_midchar(bytes[j]) {
            j += 1;
        }
        if j >= bytes.len() {
            // Ran off the end without a terminator: drop the fragment.
            return Some((bytes.len(), None));
        }
        if bytes[j] == b'm' {
            return Some((j + 1, Some(&bytes[i..=j])));
        }
        // Valid mid-chars but a non-`m` terminator: silently dropped.
        return Some((j + 1, None));
    }
    if bytes.get(i + 1) == Some(&b']') {
        // OSC 8 hyperlink: terminated by BEL (0x07) or ST (`ESC \`).
        let mut j = i + 2;
        while j < bytes.len() {
            if bytes[j] == 0x07 {
                return Some((j + 1, Some(&bytes[i..=j])));
            }
            if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                return Some((j + 2, Some(&bytes[i..j + 2])));
            }
            j += 1;
        }
        // Unterminated: drop the rest of the line's raw bytes as-is.
        return Some((bytes.len(), None));
    }
    None
}

/// Decodes the next "piece" (UTF-8 char / malformed byte / control char) at
/// `bytes[i]`. `col` is the running column position across the whole
/// logical line (needed for tab-stop math); it is NOT reset per wrapped
/// display row (§ no scenario disambiguates cross-row tab stops, so the
/// simplest, most conventional choice — one running counter per logical
/// line — is used).
// Forced inline (and `decode_utf8_char` below): the renderer runs this per
// character, and with two instantiations of `render_line_impl` the
// compiler otherwise outlines it, costing several percent of every frame
// (measured with callgrind on `make bench`).
#[inline(always)]
fn decode_piece(bytes: &[u8], i: usize, col: usize) -> (Piece<'_>, usize) {
    let b = bytes[i];
    if b == b'\t' {
        let amount = 8 - (col % 8);
        return (
            Piece {
                text: PieceText::Spaces(amount),
                width: amount,
                reverse: false,
            },
            1,
        );
    }
    // Try to decode one UTF-8 scalar value starting here.
    let remaining = &bytes[i..];
    if let Some((ch, text)) = decode_utf8_char(remaining) {
        let len = text.len();
        if (ch as u32) < 0x20 || ch as u32 == 0x7f {
            return (caret_piece(ch), len);
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        return (
            Piece {
                text: PieceText::Borrowed(text),
                width: w,
                reverse: false,
            },
            len,
        );
    }
    // Malformed byte: LESSBINFMT per-byte hex fallback, e.g. `<E4>`.
    (
        Piece {
            text: hex_token(b),
            width: 4,
            reverse: true,
        },
        1,
    )
}

/// `<XX>` (uppercase hex), identical to `format!("<{:02X}>", b)`.
fn hex_token(b: u8) -> PieceText<'static> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    PieceText::Inline(
        [b'<', HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize], b'>'],
        4,
    )
}

fn caret_piece(ch: char) -> Piece<'static> {
    let code = ch as u32 as u8;
    let printable = code ^ 0x40; // 0x00->'@', 0x01->'A', ... 0x1f->'_', 0x7f->'?'
    Piece {
        text: PieceText::Inline([b'^', printable, 0, 0], 2),
        width: 2,
        reverse: true,
    }
}

/// Decodes a single UTF-8 scalar value at the start of `bytes`, if valid,
/// returning it along with its encoded text (borrowed from `bytes`).
/// Returns `None` for a byte sequence that isn't a valid encoding start
/// (including a truncated/incomplete multi-byte sequence) — the caller then
/// falls back to a per-byte hex token, matching the LESSBINFMT behavior
/// confirmed by `utf8-malformed` (not LESSUTFBINFMT's `<U+%04lX>`).
#[inline(always)]
fn decode_utf8_char(bytes: &[u8]) -> Option<(char, &str)> {
    let b0 = *bytes.first()?;
    let len = if b0 < 0x80 {
        // ASCII fast path: a lone byte < 0x80 is always valid UTF-8.
        let text = std::str::from_utf8(&bytes[..1]).expect("ASCII is valid UTF-8");
        return Some((b0 as char, text));
    } else if b0 & 0xE0 == 0xC0 {
        2
    } else if b0 & 0xF0 == 0xE0 {
        3
    } else if b0 & 0xF8 == 0xF0 {
        4
    } else {
        return None;
    };
    if bytes.len() < len {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..len]).ok()?;
    text.chars().next().map(|c| (c, text))
}

/// Renders one logical line into one or more display rows.
///
/// Rendering stops once `max_rows` rows are complete: the caller never
/// displays more than that, and rows are produced strictly left to right,
/// so the rows returned are exactly the first `max_rows` rows a full render
/// would produce (a very long wrapped line no longer costs time and memory
/// proportional to its whole length when only a window's worth is shown).
fn render_logical_line(
    bytes: &[u8],
    line_no: usize,
    opts: &RenderOpts,
    max_rows: usize,
) -> Vec<DisplayRow> {
    // Two instantiations: the unshifted one (every frame unless the user
    // scrolled sideways) compiles to the loop with no trace of the shift.
    if opts.hshift > 0 {
        render_line_impl::<true>(bytes, line_no, opts, max_rows)
    } else {
        render_line_impl::<false>(bytes, line_no, opts, max_rows)
    }
}

fn render_line_impl<const SHIFTED: bool>(
    bytes: &[u8],
    line_no: usize,
    opts: &RenderOpts,
    max_rows: usize,
) -> Vec<DisplayRow> {
    let number_prefix_width = if opts.line_numbers { 8 } else { 0 };
    let content_width = opts.width.saturating_sub(number_prefix_width).max(1);

    let mut rows: Vec<Vec<Piece>> = Vec::new();
    let mut current: Vec<Piece> = Vec::new();
    let mut row_col = 0usize;
    let mut line_col = 0usize;
    let mut i = 0usize;

    if SHIFTED {
        // Horizontal scroll: drop the first `hshift` columns once, up front.
        (i, line_col, row_col) = skip_left_columns(bytes, opts, &mut current);
    }

    'outer: while i < bytes.len() {
        if opts.raw_ansi
            && bytes[i] == 0x1b
            && let Some((next_i, raw)) = scan_escape(bytes, i)
        {
            if let Some(seq) = raw {
                current.push(Piece {
                    text: match String::from_utf8_lossy(seq) {
                        std::borrow::Cow::Borrowed(s) => PieceText::Borrowed(s),
                        std::borrow::Cow::Owned(s) => PieceText::Owned(s),
                    },
                    width: 0,
                    reverse: false,
                });
            }
            i = next_i;
            continue;
        }
        let (piece, consumed) = decode_piece(bytes, i, line_col);
        i += consumed;
        line_col += piece.width;

        if opts.chop {
            if row_col + piece.width > content_width {
                // Free exactly one column for the '>' marker, then stop.
                if row_col == content_width {
                    current.pop();
                }
                current.push(Piece {
                    text: PieceText::inline(">"),
                    width: 1,
                    reverse: false,
                });
                rows.push(std::mem::take(&mut current));
                break 'outer;
            }
        } else if row_col + piece.width > content_width && row_col > 0 {
            rows.push(std::mem::take(&mut current));
            if rows.len() >= max_rows {
                // `piece` (and everything after it) would start row
                // `max_rows + 1`, which no caller displays.
                break 'outer;
            }
            row_col = 0;
        }
        row_col += piece.width;
        current.push(piece);
    }
    // The chop branch above already pushes its final (truncated) row and
    // resets `current` before breaking; only push here for the ordinary
    // wrap-to-completion path (also covers a genuinely empty line, which
    // never enters the loop and must still produce exactly one row).
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }

    rows.into_iter()
        .map(|pieces| finalize_row(pieces, line_no, opts))
        .collect()
}

/// Drops the first `opts.hshift` display columns of `bytes`, keeping
/// escape sequences (colour still applies after the cut) in `current`. A
/// wide character or tab cut by the left edge leaves its visible part as
/// blanks. Returns the byte index, line column and row column to continue
/// rendering from.
#[inline(never)]
fn skip_left_columns<'a>(
    bytes: &'a [u8],
    opts: &RenderOpts,
    current: &mut Vec<Piece<'a>>,
) -> (usize, usize, usize) {
    let mut i = 0;
    let mut line_col = 0;
    let mut row_col = 0;
    while i < bytes.len() && line_col < opts.hshift {
        if opts.raw_ansi
            && bytes[i] == 0x1b
            && let Some((next_i, raw)) = scan_escape(bytes, i)
        {
            if let Some(seq) = raw {
                current.push(Piece {
                    text: match String::from_utf8_lossy(seq) {
                        std::borrow::Cow::Borrowed(s) => PieceText::Borrowed(s),
                        std::borrow::Cow::Owned(s) => PieceText::Owned(s),
                    },
                    width: 0,
                    reverse: false,
                });
            }
            i = next_i;
            continue;
        }
        let (width, consumed) = piece_width(bytes, i, line_col);
        i += consumed;
        line_col += width;
        if line_col > opts.hshift {
            let visible = line_col - opts.hshift;
            current.push(Piece {
                text: PieceText::Spaces(visible),
                width: visible,
                reverse: false,
            });
            row_col = visible;
        }
    }
    (i, line_col, row_col)
}

fn finalize_row(pieces: Vec<Piece>, line_no: usize, opts: &RenderOpts) -> DisplayRow {
    let mut spans: Vec<Span> = Vec::new();
    if opts.raw_ansi {
        // "Color resets to normal at the start of every line" (§2
        // `opt-raw-control-chars-R`): an unclosed sequence (`ESC[35m` with
        // no matching reset) must not bleed into the next row. Rows are
        // drawn via cursor positioning, not literal newlines, so the
        // terminal's SGR state otherwise persists across them regardless of
        // wrap vs. logical-line boundaries — reset unconditionally at the
        // start of every row (harmless no-op when already default).
        spans.push(Span {
            text: "\x1b[0m".to_string(),
            reverse: false,
        });
    }
    if opts.line_numbers {
        spans.push(Span {
            text: format!("{:>7} ", line_no),
            reverse: false,
        });
    }
    for piece in pieces {
        match spans.last_mut() {
            Some(last) if last.reverse == piece.reverse => piece.text.push_to(&mut last.text),
            _ => {
                let mut text = String::new();
                piece.text.push_to(&mut text);
                spans.push(Span {
                    text,
                    reverse: piece.reverse,
                });
            }
        }
    }
    spans
}

/// Applies search-match highlighting to a rendered content row: any span of
/// the row's *plain, non-reverse* text matching `pattern` is split out and
/// marked reverse (search-highlight-matches: ALL on-screen occurrences of
/// the last pattern, not just the one landed on).
fn highlight_matches(row: DisplayRow, pattern: &Regex) -> DisplayRow {
    let mut out = DisplayRow::new();
    for span in row {
        // Raw ANSI passthrough (including the per-row reset span) is never
        // search-match content; also protects against a pattern
        // incidentally matching inside literal escape bytes.
        if span.reverse || span.text.starts_with('\x1b') {
            out.push(span);
            continue;
        }
        let mut last = 0;
        for m in pattern.find_iter(&span.text) {
            if m.start() > last {
                out.push(Span {
                    text: span.text[last..m.start()].to_string(),
                    reverse: false,
                });
            }
            if m.end() > m.start() {
                out.push(Span {
                    text: span.text[m.start()..m.end()].to_string(),
                    reverse: true,
                });
            }
            last = m.end();
        }
        if last < span.text.len() {
            out.push(Span {
                text: span.text[last..].to_string(),
                reverse: false,
            });
        }
    }
    out
}

/// Display width of a whole logical line as the core view draws it (tabs
/// expanded, control characters in caret notation, escape sequences free
/// under `-R`). Used to find how far right a horizontal scroll can go.
pub fn line_display_width(bytes: &[u8], raw_ansi: bool) -> usize {
    let mut col = 0;
    let mut i = 0;
    while i < bytes.len() {
        if raw_ansi
            && bytes[i] == 0x1b
            && let Some((next_i, _)) = scan_escape(bytes, i)
        {
            i = next_i;
            continue;
        }
        let (width, consumed) = piece_width(bytes, i, col);
        i += consumed;
        col += width;
    }
    col
}

/// The display width and byte length of the piece `decode_piece` would
/// decode at `bytes[i]`, without building it. Kept separate so the
/// renderer's per-character loop stays the only caller of `decode_piece`
/// (a second caller stopped the compiler inlining it there).
fn piece_width(bytes: &[u8], i: usize, col: usize) -> (usize, usize) {
    if bytes[i] == b'\t' {
        return (8 - (col % 8), 1);
    }
    // The same decoding as `decode_utf8_char`, written out here so that
    // function also keeps a single caller.
    let b0 = bytes[i];
    let len = match b0 {
        0x00..=0x7f => 1,
        _ if b0 & 0xE0 == 0xC0 => 2,
        _ if b0 & 0xF0 == 0xE0 => 3,
        _ if b0 & 0xF8 == 0xF0 => 4,
        _ => return (4, 1),
    };
    let Some(ch) = bytes
        .get(i..i + len)
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|t| t.chars().next())
    else {
        return (4, 1); // malformed: shown as a 4-column <XX> token
    };
    if (ch as u32) < 0x20 || ch as u32 == 0x7f {
        (2, len)
    } else {
        (UnicodeWidthChar::width(ch).unwrap_or(1), len)
    }
}

pub struct WindowRender {
    pub rows: Vec<DisplayRow>,
    /// Logical line number of the last line represented in `rows` (the one
    /// the final displayed row belongs to).
    pub bottom_line: usize,
    pub at_eof: bool,
}

/// Renders a window of `content_rows` display rows starting at logical line
/// `top` (1-indexed). Reads ahead through `buffer` as needed. Full logical
/// lines are accumulated (never split mid-wrap across a window boundary in
/// this implementation — a design choice with no baseline scenario coverage,
/// declared UNVERIFIED in docs/RECONSTRUCTION_SPEC.md §5); the row list is
/// truncated to exactly `content_rows` before returning so a long line
/// straddling the boundary does not overflow the window.
pub fn render_window(
    buffer: &mut Buffer,
    top: usize,
    content_rows: usize,
    opts: &RenderOpts,
    highlight: Option<&Regex>,
) -> Result<WindowRender, String> {
    render_window_impl(buffer, top, content_rows, opts, highlight, |_| true)
}

/// A line predicate: lines it rejects are not displayed (`&` filter).
pub type LineFilter<'a> = &'a dyn Fn(&[u8]) -> bool;

/// `render_window`, showing only the lines `keep` accepts (all when `None`).
pub fn render_window_filtered(
    buffer: &mut Buffer,
    top: usize,
    content_rows: usize,
    opts: &RenderOpts,
    highlight: Option<&Regex>,
    keep: Option<LineFilter>,
) -> Result<WindowRender, String> {
    match keep {
        None => render_window_impl(buffer, top, content_rows, opts, highlight, |_| true),
        Some(f) => render_window_impl(buffer, top, content_rows, opts, highlight, f),
    }
}

#[inline]
fn render_window_impl(
    buffer: &mut Buffer,
    top: usize,
    content_rows: usize,
    opts: &RenderOpts,
    highlight: Option<&Regex>,
    keep: impl Fn(&[u8]) -> bool,
) -> Result<WindowRender, String> {
    let mut rows: Vec<DisplayRow> = Vec::new();
    let mut row_lines: Vec<usize> = Vec::new();
    let mut line_no = top.max(1);
    let mut at_eof = false;

    loop {
        buffer.ensure_lines_read(line_no)?;
        let Some(raw) = buffer.line(line_no) else {
            at_eof = true;
            break;
        };
        if !keep(raw) {
            line_no += 1;
            continue;
        }
        // `rows.len() < content_rows` here (the loop exits as soon as the
        // window is full), so at least one row is always requested.
        let mut line_rows = render_logical_line(raw, line_no, opts, content_rows - rows.len());
        if let Some(pat) = highlight {
            line_rows = line_rows
                .into_iter()
                .map(|r| highlight_matches(r, pat))
                .collect();
        }
        for r in line_rows {
            row_lines.push(line_no);
            rows.push(r);
        }
        if rows.len() >= content_rows {
            break;
        }
        line_no += 1;
    }

    let bottom_line = if rows.len() > content_rows {
        rows.truncate(content_rows);
        row_lines.truncate(content_rows);
        row_lines.last().copied().unwrap_or(top)
    } else {
        row_lines
            .last()
            .copied()
            .unwrap_or(top.saturating_sub(1).max(1))
    };

    // The exact-fit case (the window filled to precisely `content_rows`
    // without ever asking for the next line) leaves EOF genuinely unknown:
    // peek one line further so a file whose tail exactly fills the window
    // (e.g. `nav-boundary-eof-bof`'s clamp to top=178 on a 200-line file,
    // 23 content rows) still shows `(END)`.
    if !at_eof && rows.len() == content_rows {
        let mut next = line_no + 1;
        loop {
            buffer.ensure_lines_read(next)?;
            match buffer.line(next) {
                None => {
                    at_eof = true;
                    break;
                }
                Some(raw) if keep(raw) => break,
                Some(_) => next += 1,
            }
        }
    }

    Ok(WindowRender {
        rows,
        bottom_line,
        at_eof,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(width: usize) -> RenderOpts {
        RenderOpts {
            width,
            line_numbers: false,
            chop: false,
            raw_ansi: false,
            hshift: 0,
        }
    }

    fn text_only(row: &DisplayRow) -> String {
        row.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn wraps_at_exact_width_no_indicator() {
        let line = "a".repeat(81);
        let rows = render_logical_line(line.as_bytes(), 1, &opts(80), usize::MAX);
        assert_eq!(rows.len(), 2);
        assert_eq!(text_only(&rows[0]).chars().count(), 80);
        assert_eq!(text_only(&rows[1]).chars().count(), 1);
    }

    #[test]
    fn chop_marks_overflow_with_gt_and_leaves_exact_width_unmarked() {
        let exact = "a".repeat(80);
        let mut o = opts(80);
        o.chop = true;
        let rows = render_logical_line(exact.as_bytes(), 1, &o, usize::MAX);
        assert_eq!(rows.len(), 1);
        assert!(!text_only(&rows[0]).ends_with('>'));
        assert_eq!(text_only(&rows[0]).chars().count(), 80);

        let over = "a".repeat(81);
        let rows2 = render_logical_line(over.as_bytes(), 1, &o, usize::MAX);
        assert_eq!(rows2.len(), 1);
        let t = text_only(&rows2[0]);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('>'));
        assert_eq!(&t[..79], &"a".repeat(79));
    }

    #[test]
    fn tab_stops_at_8_columns() {
        let rows = render_logical_line(b"a\tb\tc\td\t", 1, &opts(80), usize::MAX);
        let t = text_only(&rows[0]);
        assert_eq!(t.find('b'), Some(8));
        assert_eq!(t.find('c'), Some(16));
        assert_eq!(t.find('d'), Some(24));
    }

    #[test]
    fn caret_notation_for_control_chars() {
        let rows = render_logical_line(
            b"ctrl:[\x01][\x02][\x07][\x1f]end",
            1,
            &opts(80),
            usize::MAX,
        );
        let t = text_only(&rows[0]);
        assert_eq!(t, "ctrl:[^A][^B][^G][^_]end");
    }

    #[test]
    fn malformed_utf8_becomes_per_byte_hex() {
        let rows = render_logical_line(b"line before\n", 1, &opts(80), usize::MAX);
        let _ = rows; // sanity: no panic on plain ascii
        let rows2 = render_logical_line(&[0xE4, 0xB8], 1, &opts(80), usize::MAX);
        let t = text_only(&rows2[0]);
        assert_eq!(t, "<E4><B8>");
        assert!(rows2[0].iter().all(|s| s.reverse || s.text.is_empty()));
    }

    #[test]
    fn cjk_wraps_by_display_width_not_char_count() {
        let line: String = "\u{6f22}".repeat(45);
        let rows = render_logical_line(line.as_bytes(), 1, &opts(80), usize::MAX);
        assert_eq!(rows.len(), 2);
        assert_eq!(text_only(&rows[0]).chars().count(), 40);
        assert_eq!(text_only(&rows[1]).chars().count(), 5);
    }

    #[test]
    fn line_number_prefix_is_fixed_7_chars_plus_space_every_row() {
        let mut o = opts(80);
        o.line_numbers = true;
        let line = "x".repeat(150);
        let rows = render_logical_line(line.as_bytes(), 4, &o, usize::MAX);
        assert!(rows.len() >= 2);
        for row in &rows {
            assert!(text_only(row).starts_with("      4 "));
        }
    }

    #[test]
    fn raw_ansi_color_passthrough_and_malformed_sequence_dropped() {
        let mut o = opts(80);
        o.raw_ansi = true;
        let rows = render_logical_line(b"a\x1b[31mb\x1b[0mc", 1, &o, usize::MAX);
        let t = text_only(&rows[0]);
        assert_eq!(t, "\x1b[0ma\x1b[31mb\x1b[0mc");

        let rows2 = render_logical_line(b"before \x1b[2;5H after", 1, &o, usize::MAX);
        let t2 = text_only(&rows2[0]);
        assert_eq!(t2, "\x1b[0mbefore  after");
    }

    /// Regression test for the cycle-2 finding: an unclosed color sequence
    /// (`ESC[35m` with no matching reset) must not bleed into the next row.
    /// `opt-raw-control-chars/ansi-raw-view.attrs.txt`: the "Magenta start
    /// (unclosed)" line stays magenta through its own end, but the
    /// following line renders in plain/default color.
    #[test]
    fn raw_ansi_row_resets_at_start_so_unclosed_color_does_not_bleed() {
        let mut o = opts(80);
        o.raw_ansi = true;
        let unclosed = render_logical_line(b"\x1b[35mMagenta start (unclosed)", 1, &o, usize::MAX);
        let next = render_logical_line(b"next line after unclosed color", 2, &o, usize::MAX);
        assert!(text_only(&unclosed[0]).starts_with("\x1b[0m\x1b[35m"));
        assert!(
            text_only(&next[0]).starts_with("\x1b[0mnext"),
            "next row must open with a reset before its own (absent) color codes"
        );
    }

    #[test]
    fn ansi_escape_is_zero_width_for_wrap_accounting() {
        let mut o = opts(10);
        o.raw_ansi = true;
        let content = format!("\x1b[31m{}", "a".repeat(10));
        let rows = render_logical_line(content.as_bytes(), 1, &o, usize::MAX);
        assert_eq!(
            rows.len(),
            1,
            "escape sequence must not count toward the 10-col wrap width"
        );
    }

    #[test]
    fn search_highlight_marks_all_occurrences_on_a_row() {
        let re = Regex::new("[cb]at").unwrap();
        let rows = render_logical_line(
            b"regex sample cat bat hat sat mat",
            1,
            &opts(80),
            usize::MAX,
        );
        let highlighted = highlight_matches(rows[0].clone(), &re);
        let reversed: Vec<&str> = highlighted
            .iter()
            .filter(|s| s.reverse)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(reversed, vec!["cat", "bat"]);
    }

    /// The `max_rows` cap must yield exactly the first `max_rows` rows of
    /// an uncapped render, for every option combination and for content
    /// mixing wide chars, tabs, control chars, malformed UTF-8 and ANSI.
    #[test]
    fn capped_render_is_a_prefix_of_full_render() {
        let mut samples: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"short".to_vec(),
            "x".repeat(400).into_bytes(),
            "\u{6f22}a".repeat(90).into_bytes(),
            b"\t".repeat(50),
        ];
        let mut mixed = Vec::new();
        for i in 0..60u8 {
            mixed.extend_from_slice(b"ab\tc\x1b[31mred\x1b[0m\x01");
            mixed.extend_from_slice("\u{6f22}".as_bytes());
            mixed.push(0xE4);
            mixed.push(b'0' + i % 10);
            mixed.extend_from_slice(b"\x1b[2;5H\x1b]8;;http://x\x07L\x1b]8;;\x07");
        }
        samples.push(mixed);
        for width in [1usize, 2, 7, 10, 80] {
            for flags in 0..8u8 {
                let o = RenderOpts {
                    width,
                    line_numbers: flags & 1 != 0,
                    chop: flags & 2 != 0,
                    raw_ansi: flags & 4 != 0,
                    hshift: 0,
                };
                for bytes in &samples {
                    let full = render_logical_line(bytes, 7, &o, usize::MAX);
                    for cap in [
                        1,
                        2,
                        3,
                        full.len().saturating_sub(1).max(1),
                        full.len(),
                        full.len() + 1,
                    ] {
                        let capped = render_logical_line(bytes, 7, &o, cap);
                        let expect = &full[..cap.min(full.len())];
                        assert_eq!(capped, expect, "width={width} flags={flags} cap={cap}");
                    }
                }
            }
        }
    }

    /// A window over one huge wrapped line still fills exactly and is not
    /// at EOF while the same line continues below the window.
    #[test]
    fn window_over_long_wrapped_line_matches_uncapped_semantics() {
        let mut content = "y".repeat(80 * 100 + 3);
        content.push_str("\nnext\n");
        let path = temp_file(&content);
        let mut buf = Buffer::new(crate::buffer::Source::File(path.clone()));
        let win = render_window(&mut buf, 1, 23, &opts(80), None).unwrap();
        assert_eq!(win.rows.len(), 23);
        assert_eq!(win.bottom_line, 1);
        assert!(!win.at_eof);
        assert!(win.rows.iter().all(|r| text_only(r) == "y".repeat(80)));
        let _ = std::fs::remove_file(path);
    }

    fn temp_file(contents: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rust-less-screen-test-{}-{}.txt",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::write(&path, contents).unwrap();
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

    /// Regression test for a bug caught during self-review: a window that
    /// fills *exactly* to `content_rows` on a logical-line boundary must
    /// still detect true EOF by peeking one line further, matching
    /// `nav-boundary-eof-bof/after-300j-clamped-to-eof` (200-line fixture,
    /// 23 content rows, clamped top=178 shows `(END)` on row 24).
    #[test]
    fn window_that_exactly_fills_content_rows_still_detects_eof() {
        let mut content = String::new();
        for n in 1..=200 {
            content.push_str(&format!("line {}\n", n));
        }
        let path = temp_file(&content);
        let mut buf = Buffer::new(crate::buffer::Source::File(path.clone()));
        let win = render_window(&mut buf, 178, 23, &opts(80), None).unwrap();
        assert_eq!(win.rows.len(), 23);
        assert_eq!(win.bottom_line, 200);
        assert!(
            win.at_eof,
            "200 lines starting at top=178 with 23 content rows exactly reaches EOF"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn window_with_more_content_below_is_not_at_eof() {
        let mut content = String::new();
        for n in 1..=200 {
            content.push_str(&format!("line {}\n", n));
        }
        let path = temp_file(&content);
        let mut buf = Buffer::new(crate::buffer::Source::File(path.clone()));
        let win = render_window(&mut buf, 1, 23, &opts(80), None).unwrap();
        assert_eq!(win.bottom_line, 23);
        assert!(!win.at_eof);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn horizontal_shift_cuts_columns_from_the_left_only() {
        let mut o = opts(10);
        o.chop = true;
        o.hshift = 5;
        let rows = render_logical_line(b"0123456789abcdefghij", 1, &o, usize::MAX);
        assert_eq!(text_only(&rows[0]), "56789abcd>");
        o.line_numbers = true;
        o.width = 14;
        let rows = render_logical_line(b"0123456789", 42, &o, usize::MAX);
        assert_eq!(text_only(&rows[0]), "     42 56789");
        // Shifted past the end: an empty row, not a missing one.
        o.hshift = 50;
        let rows = render_logical_line(b"short", 1, &o, usize::MAX);
        assert_eq!(rows.len(), 1);
        assert_eq!(text_only(&rows[0]), "      1 ");
    }

    #[test]
    fn a_wide_character_cut_by_the_left_edge_becomes_blank() {
        let mut o = opts(10);
        o.hshift = 1;
        let rows = render_logical_line("\u{6f22}ab".as_bytes(), 1, &o, usize::MAX);
        assert_eq!(text_only(&rows[0]), " ab");
    }

    #[test]
    fn line_display_width_counts_tabs_carets_and_skips_escapes() {
        assert_eq!(line_display_width(b"a\tb", false), 9);
        assert_eq!(line_display_width(b"\x01x", false), 3);
        assert_eq!(line_display_width(b"\x1b[31mred", true), 3);
        assert_eq!(line_display_width("\u{6f22}".as_bytes(), false), 2);
    }

    #[test]
    fn filtered_window_skips_rejected_lines_and_knows_the_end() {
        let mut content = Vec::new();
        for n in 1..=10 {
            content.extend_from_slice(format!("line {n}\n").as_bytes());
        }
        let mut buf = Buffer::new(crate::buffer::Source::Memory {
            name: None,
            bytes: content,
        });
        let even = |raw: &[u8]| raw.last().is_some_and(|b| (b - b'0').is_multiple_of(2));
        let w = render_window_filtered(&mut buf, 1, 4, &opts(20), None, Some(&even)).unwrap();
        let texts: Vec<_> = w.rows.iter().map(text_only).collect();
        assert_eq!(texts, ["line 2", "line 4", "line 6", "line 8"]);
        assert_eq!(w.bottom_line, 8);
        // Only "line 10" ends in 0 and follows; the window is exactly full
        // with 4 rows starting at 4, so the peek must find the end.
        let w = render_window_filtered(&mut buf, 4, 4, &opts(20), None, Some(&even)).unwrap();
        assert_eq!(w.bottom_line, 10);
        assert!(w.at_eof);
    }

    #[test]
    fn piece_width_agrees_with_decode_piece() {
        let sample = "a\tb\u{1}\u{7f}\u{6f22}\u{301}e\u{1F600}"
            .as_bytes()
            .to_vec();
        let mut bytes = sample.clone();
        bytes.extend_from_slice(b"\xff\xe4\x80x");
        let mut i = 0;
        let mut col = 0;
        while i < bytes.len() {
            let (piece, consumed) = decode_piece(&bytes, i, col);
            assert_eq!(
                piece_width(&bytes, i, col),
                (piece.width, consumed),
                "at byte {i}"
            );
            i += consumed;
            col += piece.width;
        }
    }
}
