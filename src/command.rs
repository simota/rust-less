//! Main loop / state machine. Owns current top-of-window line, sticky
//! half-window N, current file index and the multi-file list, the
//! "first draw of this process" flag, and dispatches decoded `Action`s to
//! `buffer`/`screen`/`search`/`prompt`.
//!
//! Beyond the less-parity core it owns the paging features layered on top:
//! the `&` line filter (any format), marks, horizontal scroll, option
//! toggles, follow mode, per-file remembered positions, the help page,
//! `:e`, and the structured views' column sort.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use regex::{Regex, RegexBuilder};

use crate::buffer::{Buffer, Source};
use crate::cli::{BellMode, CaseMode, Options, PromptMode};
use crate::input::{Action, Decoded, Decoder};
use crate::projection::{ProjectionEngine, SortKey, clip_to_width, escape_controls};
use crate::prompt::{self, PromptContext};
use crate::screen::{self, RenderOpts};
use crate::search::{self, Direction};
use crate::selector::Query;
use crate::sniffer::{DetectedFormat, FormatSniffer};

/// A sorted snapshot of a structured file (column mode `s`): the sorted
/// lines live in their own in-memory buffer, so every view, search,
/// filter and mark works on it unchanged.
struct SortedView {
    buffer: Buffer,
    column: String,
    descending: bool,
}

struct FileEntry {
    buffer: Buffer,
    sorted: Option<SortedView>,
    /// The view of this file when another file was last switched to, so
    /// `:p` returns to where the user was (as less does).
    saved_view: Option<ViewState>,
}

impl FileEntry {
    fn new(source: Source) -> Self {
        FileEntry {
            buffer: Buffer::new(source),
            sorted: None,
            saved_view: None,
        }
    }

    fn display(&self) -> Option<String> {
        self.buffer.display_name()
    }

    /// The buffer the views read: the sorted snapshot when there is one.
    fn buf(&mut self) -> &mut Buffer {
        match &mut self.sorted {
            Some(s) => &mut s.buffer,
            None => &mut self.buffer,
        }
    }
}

/// The file being shown: the help page while it is open, else the current
/// file. A free function over the two fields (not a method on `App`) so a
/// caller can hold the buffer while reading other `App` fields.
fn entry_mut<'a>(
    files: &'a mut [FileEntry],
    index: usize,
    help: &'a mut Option<HelpState>,
) -> &'a mut FileEntry {
    match help {
        Some(h) => &mut h.entry,
        None => &mut files[index],
    }
}

/// The per-file part of the view, kept across file switches.
struct ViewState {
    top: usize,
    projection: ProjectionEngine,
    tabular_view: bool,
    active_filter: Option<ActiveFilter>,
    focused_column: usize,
    sniffer_checked: bool,
}

impl ViewState {
    fn fresh(raw_ansi: bool) -> Self {
        let mut projection = ProjectionEngine::new(DetectedFormat::PlainText);
        projection.raw_ansi = raw_ansi;
        ViewState {
            top: 1,
            projection,
            tabular_view: false,
            active_filter: None,
            focused_column: 0,
            sniffer_checked: false,
        }
    }
}

/// The help page, shown in place of the current file until `q`.
struct HelpState {
    entry: FileEntry,
    saved_view: ViewState,
    saved_hshift: usize,
}

/// The `&` filter: only lines that match (or, with `&!`, that do not) are
/// shown, scrolled through and searched.
#[derive(Debug, Clone)]
struct ActiveFilter {
    /// What was typed after `&` (and after `!`), kept to rebuild the query
    /// when the format or the case rule changes.
    source: String,
    invert: bool,
    query: Query,
}

impl ActiveFilter {
    fn keeps(&self, raw: &[u8], projection: &ProjectionEngine) -> bool {
        let lookup = |name: &str| projection.column_index(name);
        self.query
            .matches_line_with(raw, projection.format, &lookup)
            != self.invert
    }

    fn label(&self) -> String {
        format!("&{}{}", if self.invert { "!" } else { "" }, self.source)
    }
}

/// A place in the file list: file and top line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    file_index: usize,
    top: usize,
}

/// Bell classes per §2 `prompt-error-bell`'s documented rule: `-q` silences
/// only the EOF/BOF-scroll bell, `-Q` silences all bells. Screen state is
/// verified identical across default/-q/-Q by the baseline (bell audibility
/// itself is unverifiable by the capture harness — see docs §1), so this is
/// implemented per the man-page claim rather than diffed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BellClass {
    EofBof,
    Error,
}

/// The last search, kept for `n`/`N` and (regex only) match highlighting.
#[derive(Debug, Clone)]
enum SearchPattern {
    /// The typed pattern is kept to recompile it when the case rule changes.
    Regex(Regex, String),
    /// A selector query (`/.status >= 500`) on a structured file.
    Structured(Query),
}

/// How many leading lines the format sniffer and column derivation sample.
const SNIFF_SAMPLE_LINES: usize = 16;

/// How often follow mode (`F`) looks for new input.
const FOLLOW_POLL: Duration = Duration::from_millis(200);

const FOLLOW_PROMPT: &str = "Waiting for data... (any key stops following)";
const HELP_PROMPT: &str = "HELP -- Press q when done";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyOutcome {
    Quit,
    Continue,
}

/// `half_window = ceil(rows / 2)`, derived from the *full* terminal row
/// count, not the content-row count (§3 item 2/8): confirmed on an even
/// (24 -> 12) and an odd (25 -> 13) pane height.
pub fn default_half_window(total_rows: u16) -> u64 {
    (total_rows as u64).div_ceil(2)
}

pub struct App {
    opts: Options,
    files: Vec<FileEntry>,
    file_index: usize,
    top: usize,
    sticky_half: Option<u64>,
    first_render_done: bool,
    filename_shown: bool,
    pending_message: Option<String>,
    last_search: Option<(SearchPattern, Direction)>,
    width: u16,
    height: u16,
    decoder: Decoder,
    /// Structured-view state of the *current* file; saved with the rest of
    /// its view on `:n`/`:p` and restored on return.
    projection: ProjectionEngine,
    tabular_view: bool,
    active_filter: Option<ActiveFilter>,
    /// Index into `projection.columns` of the column-mode focus.
    focused_column: usize,
    /// Whether the current file's format has been sniffed (once per file,
    /// on its first draw).
    sniffer_checked: bool,
    /// Horizontal scroll in display columns (all files share it, as in less).
    hshift: usize,
    /// Case rule of `/`, `?` and `&` (`-i`/`-I`, toggled while paging).
    case: CaseMode,
    marks: HashMap<char, Position>,
    /// Where the last jump (goto, search, mark, file switch) started, for `''`.
    last_jump: Option<Position>,
    /// Follow mode (`F`) is on: new input is shown as it arrives.
    following: bool,
    help: Option<HelpState>,
    /// The last frame's bottom line and end-of-input state (for `=`).
    last_bottom: (usize, bool),
    /// `v` asked to open this file at this line in the editor; the main
    /// loop, which owns the terminal, runs it.
    pending_edit: Option<(PathBuf, usize)>,
    /// The last frame ran out of piped input that has not arrived yet: the
    /// main loop watches for it and redraws as it comes.
    awaiting_input: bool,
    /// Reused frame buffer for `draw` (kept to avoid regrowing it per frame).
    frame_buf: Vec<u8>,
}

impl App {
    pub fn new(opts: Options) -> Self {
        let files: Vec<FileEntry> = if opts.files.is_empty() {
            vec![FileEntry::new(Source::Stdin)]
        } else {
            opts.files
                .iter()
                .map(|p: &PathBuf| FileEntry::new(Source::File(p.clone())))
                .collect()
        };
        let view = ViewState::fresh(opts.raw_control_chars);
        let case = opts.case;
        App {
            opts,
            files,
            file_index: 0,
            top: view.top,
            sticky_half: None,
            first_render_done: false,
            filename_shown: true,
            pending_message: None,
            last_search: None,
            width: 80,
            height: 24,
            decoder: Decoder::new(),
            projection: view.projection,
            tabular_view: view.tabular_view,
            active_filter: view.active_filter,
            focused_column: view.focused_column,
            sniffer_checked: view.sniffer_checked,
            hshift: 0,
            case,
            marks: HashMap::new(),
            last_jump: None,
            following: false,
            help: None,
            last_bottom: (1, false),
            awaiting_input: false,
            pending_edit: None,
            frame_buf: Vec::new(),
        }
    }

    /// `-X`: page in the normal screen instead of the alternate one.
    pub fn no_init(&self) -> bool {
        self.opts.no_init
    }

    /// `-F`: exit at once when the input fits on one screen.
    pub fn quit_if_one_screen(&self) -> bool {
        self.opts.quit_if_one_screen
    }

    pub fn set_size(&mut self, w: u16, h: u16) {
        self.width = w.max(1);
        self.height = h.max(1);
    }

    fn content_rows(&self) -> usize {
        (self.height as usize).saturating_sub(1).max(1)
    }

    fn half_window(&self) -> u64 {
        self.sticky_half
            .unwrap_or_else(|| default_half_window(self.height))
    }

    fn render_opts(&self) -> RenderOpts {
        RenderOpts {
            width: self.width as usize,
            line_numbers: self.opts.line_numbers,
            chop: self.opts.chop_long_lines,
            raw_ansi: self.opts.raw_control_chars,
            hshift: self.hshift,
        }
    }

    fn current(&mut self) -> &mut FileEntry {
        entry_mut(&mut self.files, self.file_index, &mut self.help)
    }

    fn buffer(&mut self) -> &mut Buffer {
        self.current().buf()
    }

    /// Eagerly validates the very first file before any terminal state is
    /// touched, matching the observed less behavior of exiting near-
    /// instantly on a missing file (§2 `exit-codes`).
    pub fn validate_first_file(&mut self) -> Result<(), String> {
        self.files[0].buffer.ensure_lines_read(1)?;
        Ok(())
    }

    /// `-F`: when there is a single input and it fits on one screen,
    /// returns its rendered rows (as terminal bytes, one row per line) so
    /// the caller can print them and exit without paging. `None` means the
    /// pager is needed (more than a screen, several files, or content that
    /// would first ask "may be a binary file").
    pub fn one_screen_output(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.files.len() != 1 {
            return Ok(None);
        }
        if !self.opts.raw_control_chars && self.files[0].buffer.sniff_binary()? {
            return Ok(None);
        }
        let rows = self.content_rows();
        let opts = self.render_opts();
        let win = screen::render_window(&mut self.files[0].buffer, 1, rows + 1, &opts, None)?;
        if !win.at_eof || win.rows.len() > rows {
            return Ok(None);
        }
        let mut out = Vec::new();
        for row in &win.rows {
            write_spans(&mut out, row).map_err(|e| e.to_string())?;
            if opts.raw_ansi {
                out.extend_from_slice(b"\x1b[0m");
            }
            out.push(b'\n');
        }
        Ok(Some(out))
    }

    fn binary_prompt_text(&self) -> String {
        let name = self.files[0].display().unwrap_or_default();
        format!("\"{}\" may be a binary file.  See it anyway?", name)
    }

    /// Runs `+cmd`/`-p`/`--filter` commands as if typed, before the first
    /// screen is drawn. A command left waiting for input (`/pattern`) is
    /// submitted. An error is shown on the first screen.
    fn run_initial_commands(&mut self) -> KeyOutcome {
        let commands = std::mem::take(&mut self.opts.initial_commands);
        if commands.is_empty() {
            return KeyOutcome::Continue;
        }
        self.ensure_format_sniffed();
        for cmd in commands {
            let mut keys: Vec<KeyCode> = cmd.chars().map(KeyCode::Char).collect();
            keys.push(KeyCode::Enter);
            for (i, code) in keys.into_iter().enumerate() {
                // The trailing Enter only submits an unfinished prompt.
                if code == KeyCode::Enter && i > 0 && !self.decoder.in_text_entry() {
                    break;
                }
                if let Decoded::Action(action) = self.decoder.feed(code, KeyModifiers::NONE) {
                    if action == Action::Quit {
                        return KeyOutcome::Quit;
                    }
                    if let Err(msg) = self.apply(action) {
                        self.pending_message = Some(format!("{}  (press RETURN)", msg));
                    }
                }
            }
            self.decoder.reset();
        }
        KeyOutcome::Continue
    }

    /// Runs the interactive loop until quit. Returns the process exit code.
    pub fn run_interactive<W: Write>(&mut self, out: &mut W) -> io::Result<i32> {
        if !self.opts.raw_control_chars {
            let is_binary = self.files[0].buffer.sniff_binary().unwrap_or(false);
            if is_binary {
                let msg = self.binary_prompt_text();
                self.draw_single_message(out, &msg)?;
                loop {
                    if let Event::Key(k) = event::read()? {
                        if k.kind != KeyEventKind::Press {
                            continue;
                        }
                        match k.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => break,
                            _ => return Ok(1),
                        }
                    }
                }
            }
        }

        if self.run_initial_commands() == KeyOutcome::Quit {
            return Ok(0);
        }
        self.draw(out)?;
        loop {
            if (self.following || self.awaiting_input) && !event::poll(FOLLOW_POLL)? {
                let changed = if self.following {
                    self.follow_tick()?
                } else {
                    self.input_arrived()
                };
                if changed {
                    self.draw(out)?;
                }
                continue;
            }
            match event::read()? {
                Event::Resize(w, h) => {
                    self.set_size(w, h);
                    if self.following {
                        self.follow_tick()?;
                    }
                    self.draw(out)?;
                }
                Event::Key(k) => {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    match self.handle_key(out, k.code, k.modifiers)? {
                        KeyOutcome::Quit => return Ok(0),
                        KeyOutcome::Continue => {}
                    }
                    if let Some((path, line)) = self.pending_edit.take() {
                        self.run_editor(out, &path, line)?;
                    }
                }
                _ => {}
            }
        }
    }

    /// Handles one keypress outside the binary-confirmation gate: dismisses
    /// a `pending_message` (any key dismisses it; a non-Enter key is also
    /// fed through as the start of the next command — see the inline note
    /// below), decodes and applies the resulting action, and redraws.
    fn handle_key<W: Write>(
        &mut self,
        out: &mut W,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> io::Result<KeyOutcome> {
        if self.following {
            // Any key ends follow mode and is consumed by doing so.
            self.following = false;
            self.decoder.reset();
            self.draw(out)?;
            return Ok(KeyOutcome::Continue);
        }
        if self.pending_message.is_some() {
            // Any key dismisses a "(press RETURN)" message; per
            // `nav-goto-line/after-50G-mid-file` (baseline top lands on 50
            // exactly like a fresh `50g`, sent right after the `9999g`
            // "Cannot seek..." message with no intervening Enter), a
            // non-Enter key is NOT merely consumed as the dismissal — it is
            // simultaneously the first character of the next command. Enter
            // itself is untested in this position; treated as a pure
            // dismissal (matches the literal "press RETURN" wording) rather
            // than risking an extra scroll.
            self.pending_message = None;
            if code == KeyCode::Enter {
                self.draw(out)?;
                return Ok(KeyOutcome::Continue);
            }
        }
        match self.decoder.feed(code, modifiers) {
            Decoded::Action(Action::Quit) if self.help.is_some() => {
                self.close_help();
                self.draw(out)?;
            }
            Decoded::Action(Action::Quit) => return Ok(KeyOutcome::Quit),
            Decoded::Action(action) => {
                let was_scroll = matches!(
                    action,
                    Action::ScrollLines(_)
                        | Action::ScrollWindow(_, _)
                        | Action::ScrollHalfWindow(_, _)
                );
                let old_top = self.top;
                match self.apply(action) {
                    Ok(()) => {
                        if was_scroll && self.top == old_top {
                            self.ring_bell(out, BellClass::EofBof)?;
                        }
                    }
                    Err(msg) => {
                        self.pending_message = Some(format!("{}  (press RETURN)", msg));
                        self.ring_bell(out, BellClass::Error)?;
                    }
                }
                self.draw(out)?;
            }
            Decoded::Redraw => self.draw(out)?,
            Decoded::Nothing => {}
        }
        Ok(KeyOutcome::Continue)
    }

    /// Dispatches `action`, then marks the fresh-process bottom-anchor
    /// window closed once the view actually moves (top-of-window line or
    /// current file changes). §3 item 1's flag must NOT be consumed by the
    /// mere act of drawing: `prompt-error-bell-*` shows a failed EOF-clamp
    /// `j` (no-op, screen byte-identical) must still redraw bottom-anchored
    /// with no tilde — only a real position/file change may switch to
    /// top-anchor+tilde on the next draw.
    ///
    /// Jumps (goto, search, marks, file switches) also remember where they
    /// started, for `''`.
    fn apply(&mut self, action: Action) -> Result<(), String> {
        let old_top = self.top;
        let old_file_index = self.file_index;
        let was_help = self.help.is_some();
        let is_jump = matches!(
            action,
            Action::GotoLine(_)
                | Action::GotoLast(_)
                | Action::GotoPercent(_)
                | Action::SearchInput(..)
                | Action::RepeatSearch(_)
                | Action::RepeatSearchReverse(_)
                | Action::NextFile(_)
                | Action::PrevFile(_)
                | Action::GotoFile(_)
                | Action::Examine(_)
        );
        let result = self.apply_inner(action);
        let moved = self.top != old_top || self.file_index != old_file_index;
        if result.is_ok() && moved {
            self.first_render_done = true;
            if is_jump && !was_help && self.help.is_none() {
                self.last_jump = Some(Position {
                    file_index: old_file_index,
                    top: old_top,
                });
            }
        }
        result
    }

    fn apply_inner(&mut self, action: Action) -> Result<(), String> {
        if self.help.is_some()
            && matches!(
                action,
                Action::NextFile(_)
                    | Action::PrevFile(_)
                    | Action::GotoFile(_)
                    | Action::Examine(_)
                    | Action::Reload
                    | Action::Follow
                    | Action::SetMark(_)
                    | Action::GotoMark(_)
            )
        {
            self.close_help();
        }
        match action {
            Action::ScrollLines(n) => {
                if self.active_filter.is_some() {
                    self.scroll_visible(n.unsigned_abs(), n >= 0)?;
                } else {
                    let proposed = shift(self.top, n);
                    self.top = self.clamp_relative(proposed)?;
                }
            }
            Action::ScrollWindow(n, forward) => {
                let mag = n.unwrap_or(self.content_rows() as u64);
                if self.active_filter.is_some() {
                    self.scroll_visible(mag, forward)?;
                } else {
                    let signed = if forward { mag as i64 } else { -(mag as i64) };
                    let proposed = shift(self.top, signed);
                    self.top = self.clamp_relative(proposed)?;
                }
            }
            Action::ScrollHalfWindow(n, forward) => {
                let mag = match n {
                    Some(v) => {
                        self.sticky_half = Some(v);
                        v
                    }
                    None => self.half_window(),
                };
                if self.active_filter.is_some() {
                    self.scroll_visible(mag, forward)?;
                } else {
                    let signed = if forward { mag as i64 } else { -(mag as i64) };
                    let proposed = shift(self.top, signed);
                    self.top = self.clamp_relative(proposed)?;
                }
            }
            Action::GotoLine(n) => {
                let target = n.unwrap_or(1);
                self.top = self.goto_absolute(target)?;
                if n.is_none() && self.active_filter.is_some() {
                    self.top = self.next_visible_from(1)?.unwrap_or(1);
                }
            }
            Action::GotoLast(n) => match n {
                Some(target) => {
                    self.top = self.goto_absolute(target)?;
                }
                None => {
                    self.buffer().ensure_full_read()?;
                    self.top = self.tail_top()?;
                }
            },
            Action::GotoPercent(n) => {
                let percent = n.unwrap_or(0).min(100) as usize;
                let total = self.buffer().total_lines()?;
                let target = (total * percent / 100).max(1);
                let target = if self.active_filter.is_some() {
                    self.next_visible_from(target)?
                        .or(self.prev_visible_from(target)?)
                        .unwrap_or(target)
                } else {
                    target
                };
                self.top = target.min(self.tail_top()?).max(1);
            }
            Action::SearchInput(pattern, dir, count) => {
                self.run_search(&pattern, dir, count, true)?;
            }
            Action::RepeatSearch(count) => self.repeat_search(count, false)?,
            Action::RepeatSearchReverse(count) => self.repeat_search(count, true)?,
            Action::ToggleTabular => {
                self.tabular_view = !self.tabular_view;
                let format = self.projection.format;
                self.pending_message = Some(if !self.tabular_view {
                    "View mode: raw stream".to_string()
                } else if format.is_structured() {
                    format!("View mode: tabular ({})", format.name())
                } else {
                    format!("View mode: tabular (no effect on plain text; {PICK_FORMAT_HINT})")
                });
            }
            Action::ToggleExpand => {
                // Only JSON lines expand; elsewhere the key would change
                // nothing visible, so say why instead.
                if self.projection.format == DetectedFormat::JsonLines {
                    self.projection.toggle_expand(self.top);
                } else {
                    self.pending_message =
                        Some(format!("Expand: JSON lines only ({PICK_FORMAT_HINT})"));
                }
            }
            Action::ToggleStickyHeader => {
                self.projection.sticky_header = !self.projection.sticky_header;
                let state = if self.projection.sticky_header {
                    "on"
                } else {
                    "off"
                };
                self.pending_message = Some(if self.projection.format.is_structured() {
                    format!("Sticky header: {state}")
                } else {
                    format!("Sticky header: {state} (no effect on plain text; {PICK_FORMAT_HINT})")
                });
            }
            Action::ToggleFormat => {
                let next = self.projection.format.next_cycle();
                let sample = self.sample_lines();
                self.projection.set_format(next);
                self.projection.update_columns_from_lines(&sample);
                self.focused_column = 0;
                self.rebuild_filter();
                self.pending_message = Some(format!("Format: {}", next.name()));
            }
            Action::ToggleColumnMode => {
                self.focused_column = 0;
            }
            Action::PrevColumn => {
                if self.focused_column > 0 {
                    self.focused_column -= 1;
                }
            }
            Action::NextColumn => {
                if self.focused_column + 1 < self.projection.columns.len() {
                    self.focused_column += 1;
                }
            }
            Action::PinColumn => {
                if let Some(col) = self.projection.columns.get_mut(self.focused_column) {
                    col.pinned = !col.pinned;
                    let state = if col.pinned { "pinned" } else { "unpinned" };
                    let name = escape_controls(&col.name, false);
                    self.pending_message = Some(format!("Column '{}' {}", name, state));
                }
            }
            Action::HideColumn => {
                if let Some(col) = self.projection.columns.get_mut(self.focused_column) {
                    col.hidden = !col.hidden;
                    let state = if col.hidden { "hidden" } else { "shown" };
                    let name = escape_controls(&col.name, false);
                    self.pending_message = Some(format!("Column '{}' {}", name, state));
                }
            }
            Action::SortColumn => self.cycle_sort()?,
            Action::FilterInput(buf) => self.set_filter(&buf)?,
            Action::NextFile(n) => self.switch_file(self.file_index as i64 + n as i64)?,
            Action::PrevFile(n) => self.switch_file(self.file_index as i64 - n as i64)?,
            Action::GotoFile(n) => {
                let target = n.unwrap_or(1) as i64 - 1;
                if target as usize >= self.files.len() {
                    return Err("No such file".to_string());
                }
                if target as usize != self.file_index {
                    self.switch_file(target)?;
                }
            }
            Action::Examine(name) => self.examine(name.trim())?,
            Action::ScrollHorizontal(n, right) => {
                let step = n.map_or_else(|| (self.width as usize / 2).max(1), |v| v as usize);
                if right {
                    let max = self.max_hshift()?;
                    self.hshift = self.hshift.saturating_add(step).min(max.max(self.hshift));
                } else {
                    self.hshift = self.hshift.saturating_sub(step);
                }
            }
            Action::HorizontalHome => self.hshift = 0,
            Action::HorizontalEnd => self.hshift = self.max_hshift()?,
            Action::SetMark(c) => {
                if !c.is_ascii_alphabetic() {
                    return Err("Invalid mark letter".to_string());
                }
                self.marks.insert(
                    c,
                    Position {
                        file_index: self.file_index,
                        top: self.top,
                    },
                );
                self.pending_message = Some(format!("Mark '{c}' set at line {}", self.top));
            }
            Action::GotoMark(c) => self.goto_mark(c)?,
            Action::ToggleOption(c) => self.toggle_option(c)?,
            Action::FileInfo => {
                // Cut on the right: the name and position come first and
                // matter most (a prompt would lose them to a left cut).
                let info = clip_to_width(self.file_info()?, self.width as usize);
                self.pending_message = Some(info);
            }
            Action::Edit => {
                if self.help.is_some() {
                    return Err("Cannot edit the help page".to_string());
                }
                let entry = &self.files[self.file_index];
                let Some(path) = entry.buffer.path() else {
                    return Err("Cannot edit standard input".to_string());
                };
                if entry.sorted.is_some() {
                    return Err("Cannot edit a sorted view (x s to unsort)".to_string());
                }
                self.pending_edit = Some((path.clone(), self.top));
            }
            Action::Reload => self.reload()?,
            Action::Follow => self.start_follow()?,
            Action::Help => self.open_help(),
            Action::Quit => unreachable!("handled by caller"),
        }
        Ok(())
    }

    /// Relative-scroll clamp: reads just far enough ahead to know whether
    /// the proposed target overruns EOF; if so, clamps to the position that
    /// fills the window with the file's last lines (§3 item 3: relative
    /// scroll clamps silently, unlike `g`/`G`).
    fn clamp_relative(&mut self, proposed: usize) -> Result<usize, String> {
        let content_rows = self.content_rows();
        let lookahead = proposed
            .saturating_add(content_rows.saturating_sub(1))
            .max(1);
        let buffer = self.buffer();
        buffer.ensure_lines_read(lookahead)?;
        if buffer.is_eof() {
            let total = buffer.lines_cached();
            let max_top = if total > content_rows {
                total - content_rows + 1
            } else {
                1
            };
            Ok(proposed.max(1).min(max_top))
        } else {
            Ok(proposed.max(1))
        }
    }

    /// Absolute-jump: errors (leaving position unchanged) instead of
    /// clamping when `target` is beyond EOF (§3 item 3). `ensure_lines_read`
    /// only stops short of `target` when it has hit true EOF, so a single
    /// call is enough to distinguish "not yet read" from "doesn't exist".
    fn goto_absolute(&mut self, target: u64) -> Result<usize, String> {
        let t = target as usize;
        let buffer = self.buffer();
        buffer.ensure_lines_read(t)?;
        if buffer.line(t).is_some() {
            Ok(t)
        } else {
            Err(format!("Cannot seek to line number {}", target))
        }
    }

    // --- The `&` filter and scrolling through the lines it shows --------

    /// Whether line `n` is shown under the current filter; `None` past the
    /// end of input.
    fn line_visible(&mut self, n: usize) -> Result<Option<bool>, String> {
        let buffer = entry_mut(&mut self.files, self.file_index, &mut self.help).buf();
        buffer.ensure_lines_read(n)?;
        let Some(raw) = buffer.line(n) else {
            return Ok(None);
        };
        Ok(Some(
            self.active_filter
                .as_ref()
                .is_none_or(|f| f.keeps(raw, &self.projection)),
        ))
    }

    /// First shown line at or after `n`.
    fn next_visible_from(&mut self, n: usize) -> Result<Option<usize>, String> {
        let mut line = n.max(1);
        loop {
            match self.line_visible(line)? {
                None => return Ok(None),
                Some(true) => return Ok(Some(line)),
                Some(false) => line += 1,
            }
        }
    }

    /// Last shown line at or before `n` (reading only what is cached past
    /// the end).
    fn prev_visible_from(&mut self, n: usize) -> Result<Option<usize>, String> {
        let mut line = n.min(self.buffer().lines_cached());
        while line >= 1 {
            if self.line_visible(line)? == Some(true) {
                return Ok(Some(line));
            }
            line -= 1;
        }
        Ok(None)
    }

    /// The top line that shows the last window of input: the last
    /// `content_rows` shown lines, over what has been read so far (the
    /// caller reads to the end first when that is what it needs).
    fn tail_top(&mut self) -> Result<usize, String> {
        let content_rows = self.content_rows();
        let total = self.buffer().lines_cached();
        if self.active_filter.is_none() {
            return Ok(if total > content_rows {
                total - content_rows + 1
            } else {
                1
            });
        }
        let mut seen = 0;
        let mut top = None;
        let mut line = total;
        while line >= 1 && seen < content_rows {
            if self.line_visible(line)? == Some(true) {
                seen += 1;
                top = Some(line);
            }
            line -= 1;
        }
        Ok(top.unwrap_or(1))
    }

    /// Scrolls `n` shown lines under the filter, stopping where the last
    /// window of shown lines begins (the filtered counterpart of
    /// `clamp_relative`).
    fn scroll_visible(&mut self, n: u64, forward: bool) -> Result<(), String> {
        let mut cur = self.next_visible_from(self.top)?.unwrap_or(self.top);
        let mut moved = 0;
        let mut line = cur;
        while moved < n {
            if forward {
                line += 1;
            } else if line <= 1 {
                break;
            } else {
                line -= 1;
            }
            match self.line_visible(line)? {
                None => break,
                Some(true) => {
                    cur = line;
                    moved += 1;
                }
                Some(false) => {}
            }
        }
        if forward {
            // Clamp only when fewer than a window of shown lines remain.
            let rows = self.content_rows();
            let mut ahead = 0;
            let mut probe = cur;
            while ahead < rows {
                match self.line_visible(probe)? {
                    None => break,
                    Some(true) => ahead += 1,
                    Some(false) => {}
                }
                probe += 1;
            }
            if ahead < rows {
                cur = cur.min(self.tail_top()?);
            }
        }
        self.top = cur.max(1);
        Ok(())
    }

    /// Parses the filter `source` for the current format and case rule. On
    /// plain text a selector-shaped pattern is ordinary text to find.
    fn compile_filter(&self, source: &str) -> Query {
        let ignore_case = self.case.ignores_case(source);
        if self.projection.format.is_structured() {
            Query::parse_with_case(source, ignore_case)
        } else {
            Query::regex(source, ignore_case)
        }
    }

    fn set_filter(&mut self, input: &str) -> Result<(), String> {
        if input.trim().is_empty() {
            self.active_filter = None;
            self.pending_message = Some("Filter cleared".to_string());
            return Ok(());
        }
        let (invert, source) = match input.strip_prefix('!') {
            Some(rest) => (true, rest.to_string()),
            None => (false, input.to_string()),
        };
        let query = self.compile_filter(&source);
        // Reject before applying: a pattern that cannot compile matches
        // nothing and would blank the view silently.
        if !query.is_valid() {
            return Err("Invalid pattern".to_string());
        }
        let filter = ActiveFilter {
            source,
            invert,
            query,
        };
        let label = filter.label();
        self.active_filter = Some(filter);
        match self.next_visible_from(1)? {
            None => {
                self.pending_message = Some(format!("Filter: {label} (no matching lines)"));
            }
            Some(first) => {
                // Keep the place when it is still shown, else move to the
                // nearest shown line.
                self.top = match self.next_visible_from(self.top)? {
                    Some(line) => line,
                    None => self.prev_visible_from(self.top)?.unwrap_or(first),
                };
                self.pending_message = Some(format!("Filter: {label}"));
            }
        }
        Ok(())
    }

    /// Re-parses the filter and the last search after the format or the
    /// case rule changed.
    fn rebuild_filter(&mut self) {
        if let Some(f) = self.active_filter.take() {
            let query = self.compile_filter(&f.source);
            self.active_filter = Some(ActiveFilter { query, ..f });
        }
        if let Some((SearchPattern::Regex(_, src), dir)) = &self.last_search
            && let Ok(re) = self.compile_regex(src)
        {
            self.last_search = Some((SearchPattern::Regex(re, src.clone()), *dir));
        }
    }

    fn compile_regex(&self, pattern: &str) -> Result<Regex, String> {
        RegexBuilder::new(pattern)
            .case_insensitive(self.case.ignores_case(pattern))
            .build()
            .map_err(|_| "Invalid pattern".to_string())
    }

    // --- Search -----------------------------------------------------------

    fn run_search(
        &mut self,
        pattern_src: &str,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        // A selector query only means something on a structured file; on
        // plain text `/.status >= 500` is an ordinary regex, as in less.
        if self.projection.format.is_structured() {
            let q = Query::parse(pattern_src);
            if let Query::Structured(_) = q {
                return self.run_search_query(&q, dir, count, fresh);
            }
        }
        let pattern = self.compile_regex(pattern_src)?;
        self.run_search_regex(&pattern, pattern_src, dir, count, fresh)
    }

    fn repeat_search(&mut self, count: u64, reverse: bool) -> Result<(), String> {
        let Some((pattern, dir)) = self.last_search.clone() else {
            return Ok(());
        };
        let dir = if reverse { dir.reversed() } else { dir };
        match pattern {
            SearchPattern::Regex(re, src) => self.run_search_regex(&re, &src, dir, count, false),
            SearchPattern::Structured(q) => self.run_search_query(&q, dir, count, false),
        }
    }

    /// Line a search starts from; see `run_search_regex`.
    fn search_start(&self, dir: Direction, fresh: bool) -> usize {
        match (fresh, dir) {
            (true, Direction::Backward) => self.top + self.content_rows().saturating_sub(1),
            _ => self.top,
        }
    }

    /// Runs one search scan with `is_match`, skipping lines the filter
    /// hides. Returns the matching line.
    fn find_line(
        &mut self,
        is_match: impl Fn(&[u8], DetectedFormat) -> bool,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<Option<usize>, String> {
        let start = self.search_start(dir, fresh);
        let filter = &self.active_filter;
        let projection = &self.projection;
        let format = projection.format;
        let buffer = entry_mut(&mut self.files, self.file_index, &mut self.help).buf();
        search::find_by(
            buffer,
            |line| {
                is_match(line, format) && filter.as_ref().is_none_or(|f| f.keeps(line, projection))
            },
            dir,
            start,
            fresh,
            count,
        )
    }

    fn run_search_query(
        &mut self,
        query: &Query,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        let columns: Vec<String> = self
            .projection
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();
        let lookup = |name: &str| columns.iter().position(|c| c == name);
        let found = self.find_line(
            |line, format| query.matches_line_with(line, format, &lookup),
            dir,
            count,
            fresh,
        )?;
        // Only an original `/`/`?` (with the direction it was typed in) is
        // remembered; `N` reverses relative to it, never to itself.
        if fresh {
            self.last_search = Some((SearchPattern::Structured(query.clone()), dir));
        }
        match found {
            Some(line) => {
                self.top = line;
                Ok(())
            }
            None => Err("Pattern not found".to_string()),
        }
    }

    /// Fresh `/`/`?` searches start at the top or bottom of the *currently
    /// displayed* window depending on direction (§2 `search-forward`/
    /// `search-backward`: "starts at the first/last displayed line").
    /// Repeats (`n`/`N`) always reference `self.top` directly, since a
    /// prior match already repositioned the window there — confirmed by
    /// `search-forward-multi-match-repeat`'s chain (20 -> 80 -> 140 -> 80),
    /// which only reproduces if the repeat's start point is the previous
    /// match's line, not a re-derived window bottom.
    fn run_search_regex(
        &mut self,
        pattern: &Regex,
        source: &str,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        let found = if self.active_filter.is_some() {
            self.find_line(
                |line, _| search::line_matches(line, pattern),
                dir,
                count,
                fresh,
            )?
        } else {
            let start = self.search_start(dir, fresh);
            search::find(self.buffer(), pattern, dir, start, fresh, count)?
        };
        // Remember only the typed search: storing the reversed direction
        // of an `N` would make a following `n` run backwards too.
        if fresh {
            self.last_search = Some((
                SearchPattern::Regex(pattern.clone(), source.to_string()),
                dir,
            ));
        }
        match found {
            Some(line) => {
                self.top = line;
                Ok(())
            }
            None => Err("Pattern not found".to_string()),
        }
    }

    // --- Files -------------------------------------------------------------

    /// Moves the current file's view out of `App`, leaving a fresh one.
    fn take_view(&mut self) -> ViewState {
        let fresh = ViewState::fresh(self.opts.raw_control_chars);
        ViewState {
            top: std::mem::replace(&mut self.top, fresh.top),
            projection: std::mem::replace(&mut self.projection, fresh.projection),
            tabular_view: std::mem::replace(&mut self.tabular_view, fresh.tabular_view),
            active_filter: std::mem::replace(&mut self.active_filter, fresh.active_filter),
            focused_column: std::mem::replace(&mut self.focused_column, fresh.focused_column),
            sniffer_checked: std::mem::replace(&mut self.sniffer_checked, fresh.sniffer_checked),
        }
    }

    fn put_view(&mut self, view: ViewState) {
        self.top = view.top;
        self.projection = view.projection;
        self.tabular_view = view.tabular_view;
        self.active_filter = view.active_filter;
        self.focused_column = view.focused_column;
        self.sniffer_checked = view.sniffer_checked;
    }

    fn switch_file(&mut self, target: i64) -> Result<(), String> {
        if target < 0 {
            return Err("No previous file".to_string());
        }
        if target as usize >= self.files.len() {
            return Err("No next file".to_string());
        }
        let view = self.take_view();
        self.files[self.file_index].saved_view = Some(view);
        self.file_index = target as usize;
        // A file seen before comes back as it was left (position, format,
        // table, filter); a new one starts at the top with its own format
        // sniff on its first draw.
        if let Some(view) = self.files[self.file_index].saved_view.take() {
            self.put_view(view);
        }
        self.filename_shown = true;
        self.last_search = None;
        // A file switch always leaves the fresh-process bottom-anchor
        // window (§3 item 1), regardless of whether `top` numerically
        // changed (a fresh file also starts at line 1).
        self.first_render_done = true;
        Ok(())
    }

    /// `:e name`: shows `name`, adding it to the file list after the
    /// current file (or switching to it when it is already listed).
    fn examine(&mut self, name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("No file name given".to_string());
        }
        let path = PathBuf::from(name);
        if let Some(i) = self
            .files
            .iter()
            .position(|f| f.buffer.path() == Some(&path))
        {
            if i != self.file_index {
                self.switch_file(i as i64)?;
            }
            return Ok(());
        }
        let mut entry = FileEntry::new(Source::File(path));
        entry.buffer.ensure_lines_read(1)?;
        let at = self.file_index + 1;
        self.files.insert(at, entry);
        // Marks and saved views refer to indices; shift those after `at`.
        for m in self.marks.values_mut() {
            if m.file_index >= at {
                m.file_index += 1;
            }
        }
        if let Some(p) = &mut self.last_jump
            && p.file_index >= at
        {
            p.file_index += 1;
        }
        self.switch_file(at as i64)
    }

    // --- Marks ---------------------------------------------------------------

    fn goto_mark(&mut self, c: char) -> Result<(), String> {
        let here = Position {
            file_index: self.file_index,
            top: self.top,
        };
        let target = match c {
            '\'' => self.last_jump.ok_or("No previous position")?,
            '^' => Position { top: 1, ..here },
            '$' => {
                self.buffer().ensure_full_read()?;
                Position {
                    top: self.tail_top()?,
                    ..here
                }
            }
            c if c.is_ascii_alphabetic() => *self.marks.get(&c).ok_or("Mark not set")?,
            _ => return Err("Invalid mark letter".to_string()),
        };
        if target.file_index != self.file_index {
            self.switch_file(target.file_index as i64)?;
        }
        let buffer = self.buffer();
        buffer.ensure_lines_read(target.top)?;
        // The file may have been reloaded shorter since the mark was set.
        self.top = target.top.min(buffer.lines_cached()).max(1);
        if here != target {
            self.last_jump = Some(here);
        }
        self.first_render_done = true;
        Ok(())
    }

    // --- Options, info, reload ---------------------------------------------

    fn toggle_option(&mut self, c: char) -> Result<(), String> {
        let message = match c {
            'N' => {
                self.opts.line_numbers = !self.opts.line_numbers;
                if self.opts.line_numbers {
                    "Line numbers: on"
                } else {
                    "Line numbers: off"
                }
            }
            'S' => {
                self.opts.chop_long_lines = !self.opts.chop_long_lines;
                if self.opts.chop_long_lines {
                    "Long lines: chopped (LEFT/RIGHT scroll sideways)"
                } else {
                    "Long lines: wrapped"
                }
            }
            'i' => {
                self.case = if self.case == CaseMode::Smart {
                    CaseMode::Sensitive
                } else {
                    CaseMode::Smart
                };
                self.rebuild_filter();
                if self.case == CaseMode::Smart {
                    "Case: ignored in searches unless the pattern has uppercase"
                } else {
                    "Case: significant in searches"
                }
            }
            'I' => {
                self.case = if self.case == CaseMode::Always {
                    CaseMode::Sensitive
                } else {
                    CaseMode::Always
                };
                self.rebuild_filter();
                if self.case == CaseMode::Always {
                    "Case: ignored in searches"
                } else {
                    "Case: significant in searches"
                }
            }
            other => {
                return Err(format!(
                    "No -{} toggle while paging (-N -S -i -I)",
                    escape_controls(&other.to_string(), false)
                ));
            }
        };
        self.pending_message = Some(message.to_string());
        Ok(())
    }

    /// `=`: where the view is and what is applied to it.
    fn file_info(&mut self) -> Result<String, String> {
        let (bottom, at_eof) = self.last_bottom;
        let help = self.help.is_some();
        let file_count = self.files.len();
        let file_index = self.file_index;
        let following = self.following;
        let top = self.top;
        let entry = self.current();
        let name = entry
            .display()
            .unwrap_or_else(|| "Standard input".to_string());
        let sorted = entry
            .sorted
            .as_ref()
            .map(|s| (s.column.clone(), s.descending));
        let buffer = entry.buf();
        // Totals need the whole input; a pipe still open would block.
        let complete = buffer.is_eof() || buffer.path().is_some() || help || sorted.is_some();
        let mut parts = vec![name];
        if file_count > 1 && !help {
            parts.push(format!("(file {} of {})", file_index + 1, file_count));
        }
        if complete {
            let total = buffer.total_lines()?;
            let bytes = buffer.total_bytes()?;
            let offset = buffer.byte_offset_through(bottom);
            parts.push(format!("lines {top}-{bottom}/{total}"));
            parts.push(format!("byte {offset}/{bytes}"));
            let percent = (bottom.min(total) * 100).checked_div(total).unwrap_or(100);
            parts.push(format!("{percent}%"));
        } else {
            parts.push(format!("lines {top}-{bottom}/{}+", buffer.lines_cached()));
        }
        if at_eof {
            parts.push("(END)".to_string());
        }
        if self.projection.format.is_structured() {
            parts.push(format!("[{}]", self.projection.format.name()));
        }
        if let Some(f) = &self.active_filter {
            parts.push(format!("[filter {}]", f.label()));
        }
        if let Some((column, descending)) = sorted {
            let dir = if descending { "desc" } else { "asc" };
            parts.push(format!(
                "[sorted by {} {dir}]",
                escape_controls(&column, false)
            ));
        }
        if following {
            parts.push("[following]".to_string());
        }
        Ok(parts.join(" "))
    }

    fn reload(&mut self) -> Result<(), String> {
        let entry = &mut self.files[self.file_index];
        if !entry.buffer.reload() {
            return Err("Cannot reload standard input".to_string());
        }
        entry.sorted = None;
        // Re-derive columns (the header may have changed) on the next draw.
        self.sniffer_checked = false;
        let fmt = self.projection.format;
        let sample = self.sample_lines();
        if fmt.is_structured() {
            let sticky = self.projection.sticky_header;
            self.projection.set_format(fmt);
            self.projection.sticky_header = sticky;
            self.projection.update_columns_from_lines(&sample);
            self.sniffer_checked = true;
        }
        let top = self.top;
        let buffer = self.buffer();
        buffer.ensure_lines_read(top)?;
        if top > buffer.lines_cached() {
            self.top = self.tail_top()?;
        }
        self.pending_message = Some("Reloaded".to_string());
        Ok(())
    }

    // --- Follow mode -----------------------------------------------------------

    fn start_follow(&mut self) -> Result<(), String> {
        let entry = &mut self.files[self.file_index];
        let was_sorted = entry.sorted.take().is_some();
        if !entry.buffer.is_stdin() {
            // A file never blocks: start from its true end.
            entry.buffer.ensure_full_read()?;
        }
        if was_sorted {
            self.top = 1;
        }
        self.following = true;
        self.follow_tick().map_err(|e| e.to_string())?;
        if was_sorted {
            self.pending_message = Some("Sort cleared to follow new input".to_string());
        }
        Ok(())
    }

    /// One follow-mode step: reads what has arrived and moves to the end.
    /// Returns whether the screen needs a redraw.
    fn follow_tick(&mut self) -> io::Result<bool> {
        let grew = match self.buffer().read_new_input() {
            Ok(grew) => grew,
            Err(e) => {
                self.following = false;
                self.pending_message = Some(format!("{e}  (press RETURN)"));
                return Ok(true);
            }
        };
        let old_top = self.top;
        self.top = self.tail_top().unwrap_or(self.top);
        if self.top != old_top {
            self.first_render_done = true;
        }
        Ok(grew || self.top != old_top)
    }

    /// Whether piped input the last frame was missing has arrived since
    /// (new lines, or the end of input).
    fn input_arrived(&mut self) -> bool {
        let want = self.top + self.content_rows() + 1;
        let buffer = self.buffer();
        let before = (buffer.lines_cached(), buffer.is_eof());
        buffer.set_nonblocking(true);
        let result = buffer.ensure_lines_available(want);
        buffer.set_nonblocking(false);
        result.is_err() || (buffer.lines_cached(), buffer.is_eof()) != before
    }

    /// `v`: hands the terminal to the editor (`$VISUAL`, else `$EDITOR`,
    /// else `vi`) at `line`, then takes it back and re-reads the file.
    fn run_editor<W: Write>(
        &mut self,
        out: &mut W,
        path: &std::path::Path,
        line: usize,
    ) -> io::Result<()> {
        use crossterm::terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        };
        let argv = editor_command(
            std::env::var("VISUAL").ok().as_deref(),
            std::env::var("EDITOR").ok().as_deref(),
            path,
            line,
        );
        let alternate = !self.opts.no_init;
        if alternate {
            execute!(out, LeaveAlternateScreen)?;
        }
        disable_raw_mode()?;
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status();
        enable_raw_mode()?;
        if alternate {
            execute!(out, EnterAlternateScreen)?;
        }
        match status {
            Ok(s) if s.success() => {
                if let Err(e) = self.reload() {
                    self.pending_message = Some(format!("{e}  (press RETURN)"));
                } else {
                    self.pending_message = None;
                }
            }
            Ok(s) => {
                self.pending_message = Some(format!("Editor exited with {s}  (press RETURN)"));
            }
            Err(e) => {
                self.pending_message = Some(format!("Cannot run {}: {e}  (press RETURN)", argv[0]));
            }
        }
        self.draw(out)
    }

    // --- Help ------------------------------------------------------------------

    fn open_help(&mut self) {
        if self.help.is_some() {
            return;
        }
        let saved_view = self.take_view();
        let entry = FileEntry::new(Source::Memory {
            name: Some("HELP".to_string()),
            bytes: crate::cli::help_page().into_bytes(),
        });
        self.help = Some(HelpState {
            entry,
            saved_view,
            saved_hshift: std::mem::take(&mut self.hshift),
        });
        // The help page is plain text: no sniffing.
        self.sniffer_checked = true;
        self.first_render_done = true;
    }

    fn close_help(&mut self) {
        if let Some(h) = self.help.take() {
            self.put_view(h.saved_view);
            self.hshift = h.saved_hshift;
            self.decoder.reset();
        }
    }

    // --- Horizontal scroll -------------------------------------------------------

    /// How far right the view can scroll: until the end of the widest line
    /// (or table row) in the window reaches the right edge.
    fn max_hshift(&mut self) -> Result<usize, String> {
        let width = self.width as usize;
        if self.use_projection() && self.tabular_view && !self.projection.columns.is_empty() {
            let (pinned, rest): (Vec<_>, Vec<_>) =
                self.projection.display_columns().partition(|c| c.pinned);
            let cells = |cols: &[&crate::projection::ColumnInfo]| {
                cols.iter().map(|c| c.width + 1).sum::<usize>()
            };
            let room = width.saturating_sub(cells(&pinned));
            return Ok(cells(&rest).saturating_sub(room));
        }
        let number_width = if self.opts.line_numbers { 8 } else { 0 };
        let room = width.saturating_sub(number_width).max(1);
        let raw_ansi = self.opts.raw_control_chars;
        let rows = self.content_rows();
        let mut widest = 0;
        let mut line = self.top;
        let mut shown = 0;
        while shown < rows {
            match self.line_visible(line)? {
                None => break,
                Some(false) => {}
                Some(true) => {
                    let raw = self.buffer().line(line).unwrap_or_default();
                    widest = widest.max(screen::line_display_width(raw, raw_ansi));
                    shown += 1;
                }
            }
            line += 1;
        }
        Ok(widest.saturating_sub(room))
    }

    // --- Column sort -------------------------------------------------------------

    /// Column mode `s`: ascending, then descending, then the file order.
    fn cycle_sort(&mut self) -> Result<(), String> {
        let Some(col) = self.projection.columns.get(self.focused_column) else {
            return Ok(());
        };
        let column = col.name.clone();
        let shown = escape_controls(&column, false).into_owned();
        let entry = &self.files[self.file_index];
        if self.help.is_some() {
            return Ok(());
        }
        let next = match &entry.sorted {
            Some(s) if s.column == column && !s.descending => Some(true),
            Some(s) if s.column == column => None,
            _ => Some(false),
        };
        match next {
            None => {
                self.files[self.file_index].sorted = None;
                self.top = 1;
                self.pending_message = Some(format!("Unsorted ('{shown}' sort cleared)"));
            }
            Some(descending) => {
                let lines = self.sort_by(&column, descending)?;
                self.top = 1;
                let dir = if descending {
                    "descending"
                } else {
                    "ascending"
                };
                self.pending_message = Some(format!("Sorted {lines} lines by '{shown}' ({dir})"));
            }
        }
        self.first_render_done = true;
        Ok(())
    }

    /// Replaces the view of the current file with a snapshot sorted by
    /// `column` (stable, so equal rows keep file order). A CSV/TSV header
    /// row stays first. Returns how many lines were sorted.
    fn sort_by(&mut self, column: &str, descending: bool) -> Result<usize, String> {
        let entry = &mut self.files[self.file_index];
        entry.buffer.ensure_full_read()?;
        let total = entry.buffer.lines_cached();
        let has_header = matches!(
            self.projection.format,
            DetectedFormat::Csv | DetectedFormat::Tsv
        );
        let first = if has_header { 2 } else { 1 };
        let mut keyed: Vec<(Option<SortKey>, usize)> = Vec::with_capacity(total);
        for n in first..=total {
            let line = entry.buffer.line(n).unwrap_or_default();
            let value = self.projection.field_value(column, line);
            keyed.push((SortKey::of(&value), n));
        }
        keyed.sort_by(|a, b| SortKey::compare(&a.0, &b.0, descending));
        let mut bytes = Vec::with_capacity(entry.buffer.byte_offset_through(total) + total);
        let mut push = |n: usize| {
            bytes.extend_from_slice(entry.buffer.line(n).unwrap_or_default());
            bytes.push(b'\n');
        };
        if has_header && total >= 1 {
            push(1);
        }
        for (_, n) in &keyed {
            push(*n);
        }
        let name = entry.display();
        entry.sorted = Some(SortedView {
            buffer: Buffer::new(Source::Memory { name, bytes }),
            column: column.to_string(),
            descending,
        });
        Ok(keyed.len())
    }

    // --- Drawing -----------------------------------------------------------------

    fn ring_bell<W: Write>(&self, out: &mut W, class: BellClass) -> io::Result<()> {
        let silenced = match class {
            BellClass::EofBof => matches!(self.opts.bell, BellMode::Quiet | BellMode::VeryQuiet),
            BellClass::Error => matches!(self.opts.bell, BellMode::VeryQuiet),
        };
        if !silenced {
            write!(out, "\x07")?;
            out.flush()?;
        }
        Ok(())
    }

    fn draw_single_message<W: Write>(&self, out: &mut W, text: &str) -> io::Result<()> {
        execute!(out, Clear(ClearType::All), MoveTo(0, 0), Print(text))?;
        out.flush()
    }

    /// Draws one full frame. The frame is assembled in memory and handed to
    /// `out` with a single `write_all` + `flush`, so the terminal receives
    /// it in one write instead of one per 1 KiB of stdout's line buffer
    /// (fewer syscalls, and no partially drawn frame is ever visible).
    fn draw<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        let mut frame = std::mem::take(&mut self.frame_buf);
        frame.clear();
        let result = self.draw_frame(&mut frame);
        let written = result.and_then(|()| {
            out.write_all(&frame)?;
            out.flush()
        });
        self.frame_buf = frame;
        written
    }

    fn draw_frame(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let content_rows = self.content_rows();
        let width = self.width as usize;
        self.ensure_format_sniffed();

        // `Clear(All)` already blanks the whole pane, so the fresh-launch
        // bottom-anchor case (§3 item 1) needs no explicit fill: leaving
        // `blank_above` rows untouched before the content starts is enough.
        queue!(out, Clear(ClearType::All))?;

        // Piped input is drawn as far as it has arrived; the main loop
        // redraws as more comes (files never wait, so nothing changes).
        self.buffer().set_nonblocking(true);
        let painted = if self.use_projection() {
            self.paint_projection(out, content_rows, width)
        } else {
            self.paint_text(out, content_rows)
        };
        let buffer = self.buffer();
        buffer.set_nonblocking(false);
        let (bottom_line, at_eof) = painted?;
        self.awaiting_input =
            buffer.is_stdin() && !buffer.is_eof() && bottom_line >= buffer.lines_cached();
        // Running out of input that is still coming is not the end.
        let at_eof = at_eof && !self.awaiting_input;
        self.last_bottom = (bottom_line, at_eof);

        // While a multi-key command (numeric prefix, `:x`, `/pattern`) is
        // still being typed, echo it in place of the computed prompt; not
        // baseline-verified (every capture is taken post-Enter) but this is
        // the conventional, low-risk less UX and never fires in any tested
        // sequence's captured steps. Column mode shows its own status line
        // unless a message (pin/hide feedback, an error) is pending.
        let (prompt_text, highlighted) = match self.decoder.preview() {
            Some(preview) => (clip_to_width(preview, width), false),
            None if self.decoder.in_column_mode() && self.pending_message.is_none() => {
                (self.column_mode_prompt(width), false)
            }
            None if self.pending_message.is_none() && self.following => {
                (clip_to_width(FOLLOW_PROMPT.to_string(), width), true)
            }
            None if self.pending_message.is_none() && self.help.is_some() => {
                (clip_to_width(HELP_PROMPT.to_string(), width), true)
            }
            None => self.build_prompt(bottom_line, at_eof, width),
        };
        queue!(out, MoveTo(0, content_rows as u16))?;
        if highlighted {
            write_standout(out, &prompt_text)?;
        } else {
            queue!(out, Print(&prompt_text))?;
        }

        Ok(())
    }

    /// The structured views (table, expanded lines, filter) replace the
    /// less-compatible text view only on a structured file and only once
    /// one of them is switched on.
    fn use_projection(&self) -> bool {
        self.projection.format.is_structured()
            && (self.tabular_view
                || !self.projection.expanded_lines.is_empty()
                || self.active_filter.is_some())
    }

    /// The first `SNIFF_SAMPLE_LINES` lines of the current file (fewer if
    /// it is shorter or unreadable; the read error itself surfaces on the
    /// draw that needs those lines).
    fn sample_lines(&mut self) -> Vec<Vec<u8>> {
        let buffer = self.buffer();
        let _ = buffer.ensure_lines_available(SNIFF_SAMPLE_LINES);
        (1..=SNIFF_SAMPLE_LINES)
            .map_while(|i| buffer.line(i).map(<[u8]>::to_vec))
            .collect()
    }

    fn ensure_format_sniffed(&mut self) {
        if self.sniffer_checked {
            return;
        }
        self.sniffer_checked = true;
        let sample = self.sample_lines();
        let detected = FormatSniffer::default().sniff(&sample);
        if detected.is_structured() {
            self.projection.set_format(detected);
            self.projection.update_columns_from_lines(&sample);
            self.rebuild_filter();
        }
    }

    /// Paints the less-compatible text view. Returns `(bottom_line, at_eof)`
    /// for the prompt.
    fn paint_text(&mut self, out: &mut Vec<u8>, content_rows: usize) -> io::Result<(usize, bool)> {
        let render_opts = self.render_opts();
        let top = self.top;
        let highlight = match &self.last_search {
            Some((SearchPattern::Regex(re, _), _)) => Some(re),
            _ => None,
        };
        let filter = &self.active_filter;
        let projection = &self.projection;
        let keep = |raw: &[u8]| filter.as_ref().is_none_or(|f| f.keeps(raw, projection));
        let buffer = entry_mut(&mut self.files, self.file_index, &mut self.help).buf();
        let win = screen::render_window_filtered(
            buffer,
            top,
            content_rows,
            &render_opts,
            highlight,
            filter.is_some().then_some(&keep as screen::LineFilter),
        )
        .map_err(io::Error::other)?;

        // NOT consumed here: only `apply()` retires this flag, and only once
        // the view actually moves (see its doc comment). A redraw that
        // changes nothing (e.g. a failed EOF-clamp scroll) must keep
        // rendering bottom-anchored.
        let is_first_draw = !self.first_render_done;
        let short_content = win.rows.len() < content_rows;

        let blank_above = if short_content && is_first_draw {
            content_rows - win.rows.len()
        } else {
            0
        };
        let mut row_idx = blank_above;
        for row in &win.rows {
            queue!(out, MoveTo(0, row_idx as u16))?;
            write_spans(out, row)?;
            row_idx += 1;
        }
        if short_content && !is_first_draw {
            for r in row_idx..content_rows {
                queue!(out, MoveTo(0, r as u16), Print("~"))?;
            }
        }
        Ok((win.bottom_line, win.at_eof))
    }

    /// Paints a structured view (rows already escaped and width-clipped by
    /// `ProjectionEngine::render_window`). Returns `(bottom_line, at_eof)`.
    fn paint_projection(
        &mut self,
        out: &mut Vec<u8>,
        content_rows: usize,
        width: usize,
    ) -> io::Result<(usize, bool)> {
        self.projection.hshift = self.hshift;
        let filter = &self.active_filter;
        let format = self.projection.format;
        let columns: Vec<String> = self
            .projection
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();
        let keep = |raw: &[u8]| {
            filter.as_ref().is_none_or(|f| {
                let lookup = |name: &str| columns.iter().position(|c| c == name);
                f.query.matches_line_with(raw, format, &lookup) != f.invert
            })
        };
        let buffer = entry_mut(&mut self.files, self.file_index, &mut self.help).buf();
        let win = self
            .projection
            .render_window(
                buffer,
                self.top,
                content_rows,
                width,
                filter.is_some().then_some(&keep as screen::LineFilter),
                self.tabular_view,
            )
            .map_err(io::Error::other)?;
        let mut row = 0u16;
        if let Some(header) = &win.header {
            queue!(out, MoveTo(0, 0))?;
            write_standout(out, header)?;
            row = 1;
        }
        for text in &win.rows {
            queue!(out, MoveTo(0, row), Print(text))?;
            row += 1;
        }
        Ok((win.bottom_line, win.at_eof))
    }

    /// Status line of column mode: which column has focus (by name, with
    /// its state) and the keys that act on it.
    fn column_mode_prompt(&self, width: usize) -> String {
        let columns = &self.projection.columns;
        let text = match columns.get(self.focused_column) {
            None => "Columns: none (needs a structured file) - q done".to_string(),
            Some(col) => {
                let state = match (col.pinned, col.hidden) {
                    (true, true) => " (pinned, hidden)",
                    (true, false) => " (pinned)",
                    (false, true) => " (hidden)",
                    (false, false) => "",
                };
                format!(
                    "Columns: {}/{} {}{} - h/l move  p pin  . hide  s sort  q done",
                    self.focused_column + 1,
                    columns.len(),
                    escape_controls(&col.name, false),
                    state
                )
            }
        };
        clip_to_width(text, width)
    }

    fn build_prompt(&mut self, bottom_line: usize, at_eof: bool, width: usize) -> (String, bool) {
        let mode = self.opts.prompt_mode;
        let needs_totals = matches!(mode, PromptMode::Medium | PromptMode::Long);
        let file_count = self.files.len();
        let file_index = self.file_index + 1;
        let next_filename = if at_eof && self.file_index + 1 < self.files.len() {
            self.files[self.file_index + 1].display()
        } else {
            None
        };
        let filename = self.current().display();

        let (total_lines, total_bytes, byte_offset) = if needs_totals {
            let buffer = self.buffer();
            let total_lines = buffer.total_lines().ok();
            let total_bytes = buffer.total_bytes().ok();
            let byte_offset = buffer.byte_offset_through(bottom_line);
            (total_lines, total_bytes, byte_offset)
        } else {
            (None, None, 0)
        };

        let ctx = PromptContext {
            filename: filename.as_deref(),
            show_filename: self.filename_shown,
            file_index,
            file_count,
            top_line: self.top,
            bottom_line,
            total_lines,
            at_eof,
            next_filename: next_filename.as_deref(),
            byte_offset,
            total_bytes,
        };
        let (text, highlighted) =
            prompt::render(self.pending_message.as_deref(), mode, &ctx, width);
        self.filename_shown = false;
        (text, highlighted)
    }
}

/// Appended to feedback for a structured-only action taken on plain text.
const PICK_FORMAT_HINT: &str = "s picks a format";

#[cfg(test)]
impl App {
    /// Benchmark hooks (src/bench.rs): drive `apply`/`draw` from outside
    /// this module without widening their visibility for the real binary.
    pub(crate) fn bench_apply(&mut self, action: Action) {
        let _ = self.apply(action);
    }

    pub(crate) fn bench_draw<W: Write>(&mut self, out: &mut W) {
        self.draw(out).unwrap();
    }
}

/// The editor command line for `v`: `$VISUAL`, else `$EDITOR`, else `vi`
/// (split on whitespace, so `code --wait` works), then `+LINE` and the file.
fn editor_command(
    visual: Option<&str>,
    editor: Option<&str>,
    path: &std::path::Path,
    line: usize,
) -> Vec<String> {
    let chosen = [visual, editor]
        .into_iter()
        .flatten()
        .find(|e| !e.trim().is_empty())
        .unwrap_or("vi");
    let mut argv: Vec<String> = chosen.split_whitespace().map(str::to_string).collect();
    argv.push(format!("+{line}"));
    argv.push(path.display().to_string());
    argv
}

fn shift(top: usize, delta: i64) -> usize {
    if delta >= 0 {
        top.saturating_add(delta as usize)
    } else {
        top.saturating_sub((-delta) as usize).max(1)
    }
}

fn write_spans<W: Write>(out: &mut W, row: &screen::DisplayRow) -> io::Result<()> {
    for span in row {
        if span.reverse {
            write_standout(out, &span.text)?;
        } else {
            queue!(out, Print(&span.text))?;
        }
    }
    Ok(())
}

/// less 668's standout/highlight SGR pair, byte-for-byte: dark-grey-on-amber
/// (`ESC[38;5;235m ESC[48;5;214m`), closed with paired resets
/// (`ESC[39m ESC[49m`) rather than a single `ESC[0m`/`ESC[7m` reverse-video
/// toggle. Confirmed directly, not inferred: `search-forward-basic/
/// initial.attrs.txt`'s final line and every other `compare_attrs: true`
/// capture use this exact byte sequence. Used uniformly for search-match
/// highlighting, the initial-filename/special prompt row, and the
/// malformed-UTF-8/caret-notation binary-fallback tokens (screen.rs).
fn write_standout<W: Write>(out: &mut W, text: &str) -> io::Result<()> {
    write!(out, "\x1b[38;5;235m\x1b[48;5;214m{}\x1b[39m\x1b[49m", text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_window_is_ceil_of_total_rows_not_content_rows() {
        assert_eq!(default_half_window(24), 12);
        assert_eq!(default_half_window(25), 13);
        assert_eq!(default_half_window(30), 15);
        assert_eq!(default_half_window(23), 12);
    }

    #[test]
    fn shift_clamps_backward_at_one() {
        assert_eq!(shift(1, -5), 1);
        assert_eq!(shift(10, -3), 7);
        assert_eq!(shift(1, 50), 51);
    }

    // --- Rendered-output integration tests --------------------------------
    // crossterm's queue!/execute! macros write plain ANSI byte streams to
    // any `impl Write`, so `App::draw` can be exercised without a real tty.
    // These reconstruct each terminal row's visible text from the byte
    // stream (locating crossterm's `MoveTo` CSI sequences) to check the
    // single highest-risk behavior in the spec (§3 item 1: fresh-process
    // bottom-anchor, no tilde) end-to-end, not just its constituent pieces.

    fn temp_file(contents: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rust-less-cmd-test-{}-{}.txt",
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

    fn test_opts(files: Vec<PathBuf>) -> Options {
        Options {
            files,
            ..Options::default()
        }
    }

    /// Reconstructs each 0-indexed terminal row's plain visible text from a
    /// crossterm byte stream by locating `MoveTo` CSI sequences
    /// (`ESC[{row+1};{col+1}H`) and stripping SGR (`ESC[...m`) runs from the
    /// text that follows each one, up to the next escape sequence.
    fn rows_from_output(bytes: &[u8]) -> std::collections::BTreeMap<u16, String> {
        let s = String::from_utf8_lossy(bytes);
        let move_re = Regex::new(r"\x1b\[(\d+);(\d+)H").unwrap();
        let sgr_re = Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        let mut rows: std::collections::BTreeMap<u16, String> = std::collections::BTreeMap::new();
        let matches: Vec<_> = move_re.captures_iter(&s).collect();
        for (i, caps) in matches.iter().enumerate() {
            let full = caps.get(0).unwrap();
            let row: u16 = caps[1].parse::<u16>().unwrap() - 1;
            let seg_start = full.end();
            let seg_end = matches
                .get(i + 1)
                .map(|m| m.get(0).unwrap().start())
                .unwrap_or(s.len());
            let raw_segment = &s[seg_start..seg_end];
            let clean: String = sgr_re.replace_all(raw_segment, "").into_owned();
            rows.entry(row).or_default().push_str(&clean);
        }
        rows
    }

    #[test]
    fn fresh_short_file_bottom_anchors_with_blank_padding_no_tilde() {
        let mut content = String::new();
        for n in 1..=5 {
            content.push_str(&format!("short file - line {} of 5\n", n));
        }
        let path = temp_file(&content);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(160, 24);

        let mut out: Vec<u8> = Vec::new();
        app.draw(&mut out).unwrap();
        let rows = rows_from_output(&out);

        // content_rows = 23 (rows 0..22), prompt on row 23.
        // 5 content lines bottom-anchored -> blank rows 0..17, content on 18..22.
        for r in 0..18u16 {
            assert_eq!(
                rows.get(&r).map(String::as_str).unwrap_or(""),
                "",
                "row {r} should be blank"
            );
        }
        assert_eq!(rows[&18], "short file - line 1 of 5");
        assert_eq!(rows[&22], "short file - line 5 of 5");
        assert!(
            !rows.values().any(|t| t.contains('~')),
            "fresh short-file draw must not tilde-fill (§3 item 1)"
        );
        assert!(rows[&23].starts_with(path.display().to_string().as_str()));
        assert!(rows[&23].contains("(END)"));

        let _ = std::fs::remove_file(path);
    }

    /// Regression test for the cycle-1 `prompt-error-bell-*` cluster: a
    /// failed EOF-clamp scroll (`j` at EOF on a fresh short file, a true
    /// no-op) must redraw byte-identically — still bottom-anchored, no
    /// tilde — not flip to top-anchor+tilde merely because a second draw
    /// happened.
    #[test]
    fn failed_noop_scroll_on_fresh_short_file_keeps_bottom_anchor() {
        let mut content = String::new();
        for n in 1..=5 {
            content.push_str(&format!("short file - line {} of 5\n", n));
        }
        let path = temp_file(&content);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);

        let mut first: Vec<u8> = Vec::new();
        app.draw(&mut first).unwrap();

        // j at EOF on a whole-file-visible short file: top stays at 1.
        app.apply(Action::ScrollLines(1)).unwrap();
        assert_eq!(app.top, 1);

        let mut second: Vec<u8> = Vec::new();
        app.draw(&mut second).unwrap();
        let rows = rows_from_output(&second);

        for r in 0..18u16 {
            assert_eq!(
                rows.get(&r).map(String::as_str).unwrap_or(""),
                "",
                "row {r} should still be blank"
            );
        }
        assert_eq!(rows[&18], "short file - line 1 of 5");
        assert!(
            !rows.values().any(|t| t.contains('~')),
            "a no-op redraw must not switch to tilde-fill"
        );

        let _ = std::fs::remove_file(path);
    }

    /// A frame reaches the terminal as one write followed by one flush.
    #[test]
    fn draw_emits_each_frame_in_a_single_write() {
        struct Counting {
            writes: usize,
            flushes: usize,
            bytes: Vec<u8>,
        }
        impl Write for Counting {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(160, 48);
        app.apply(Action::ScrollLines(1)).unwrap();
        let mut a = Counting {
            writes: 0,
            flushes: 0,
            bytes: Vec::new(),
        };
        app.draw(&mut a).unwrap();
        assert_eq!((a.writes, a.flushes), (1, 1));
        assert!(
            a.bytes.len() > 1024,
            "frame larger than stdout's line buffer"
        );
        // The first frame shows the filename prompt; later frames of the
        // same view are identical to each other.
        let mut b: Vec<u8> = Vec::new();
        app.draw(&mut b).unwrap();
        let mut c: Vec<u8> = Vec::new();
        app.draw(&mut c).unwrap();
        assert!(b == c, "redraws of an unchanged view must be identical");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn second_file_after_switch_top_anchors_with_tilde_fill() {
        let path1 = temp_file("multi file 1 - line 1\n");
        let mut content2 = String::new();
        for n in 1..=5 {
            content2.push_str(&format!("multi file 2 - line {}\n", n));
        }
        let path2 = temp_file(&content2);

        let mut app = App::new(test_opts(vec![path1.clone(), path2.clone()]));
        app.set_size(80, 24);
        let mut discard: Vec<u8> = Vec::new();
        app.draw(&mut discard).unwrap(); // consume the "first draw of process" slot

        app.switch_file(1).unwrap();
        let mut out: Vec<u8> = Vec::new();
        app.draw(&mut out).unwrap();
        let rows = rows_from_output(&out);

        assert_eq!(rows[&0], "multi file 2 - line 1");
        assert_eq!(rows[&4], "multi file 2 - line 5");
        assert_eq!(rows[&5], "~");
        assert_eq!(rows[&22], "~");

        let _ = std::fs::remove_file(path1);
        let _ = std::fs::remove_file(path2);
    }

    fn lines_fixture(total: usize) -> PathBuf {
        let mut content = String::new();
        for n in 1..=total {
            content.push_str(&format!(
                "{:03} line {} of {} :: plain filler content\n",
                n, n, total
            ));
        }
        temp_file(&content)
    }

    #[test]
    fn relative_scroll_clamps_exactly_at_eof_no_error() {
        // nav-boundary-eof-bof: 300j from top=1 on a 200-line file (23
        // content rows) clamps to top=178, and a further j is a true no-op.
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        app.apply(Action::ScrollLines(300)).unwrap();
        assert_eq!(app.top, 178);
        app.apply(Action::ScrollLines(1)).unwrap();
        assert_eq!(
            app.top, 178,
            "j at EOF must be a silent no-op, not an error"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn absolute_goto_beyond_eof_errors_and_leaves_position_unchanged() {
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        let err = app.apply(Action::GotoLine(Some(9999))).unwrap_err();
        assert_eq!(err, "Cannot seek to line number 9999");
        assert_eq!(
            app.top, 1,
            "position must be unchanged after a failed absolute jump"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn halfwindow_sticky_default_matches_capture_sequence() {
        // nav-halfwindow: 1 -> d -> 13 -> d -> 25 -> u -> 13; 5d -> 18; d -> 23; u -> 18.
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        app.apply(Action::ScrollHalfWindow(None, true)).unwrap();
        assert_eq!(app.top, 13);
        app.apply(Action::ScrollHalfWindow(None, true)).unwrap();
        assert_eq!(app.top, 25);
        app.apply(Action::ScrollHalfWindow(None, false)).unwrap();
        assert_eq!(app.top, 13);
        app.apply(Action::ScrollHalfWindow(Some(5), true)).unwrap();
        assert_eq!(app.top, 18);
        app.apply(Action::ScrollHalfWindow(None, true)).unwrap();
        assert_eq!(app.top, 23);
        app.apply(Action::ScrollHalfWindow(None, false)).unwrap();
        assert_eq!(app.top, 18);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn search_not_found_errors_and_leaves_position_unchanged() {
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        let err = app
            .apply(Action::SearchInput(
                "NOPE_NOT_PRESENT".to_string(),
                Direction::Forward,
                1,
            ))
            .unwrap_err();
        assert_eq!(err, "Pattern not found");
        assert_eq!(app.top, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prompt_row_is_reverse_video_for_filename_but_not_for_bare_colon() {
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);

        let mut out: Vec<u8> = Vec::new();
        app.draw(&mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("\x1b[38;5;235m\x1b[48;5;214m"),
            "first prompt (filename) must use the 235/214 standout pair"
        );

        app.apply(Action::ScrollLines(1)).unwrap();
        let mut out2: Vec<u8> = Vec::new();
        app.draw(&mut out2).unwrap();
        let rows = rows_from_output(&out2);
        assert_eq!(rows[&23], ":");
        let s2 = String::from_utf8_lossy(&out2);
        let prompt_part = &s2[s2.rfind("24;1H").unwrap()..];
        assert!(
            !prompt_part.contains("\x1b[38;5;235m"),
            "bare ':' prompt must carry zero attrs"
        );

        let _ = std::fs::remove_file(path);
    }

    fn key(c: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Regression test for the cycle-1 hang cluster: `9999g` sets a
    /// "Cannot seek..." pending_message, and the *next* key ('5' of a fresh
    /// `50G`, no Enter in between) must both dismiss it and be processed as
    /// the start of that command — not be swallowed. Matches
    /// `nav-goto-line/after-50G-mid-file` (baseline top=50) and explains why
    /// `nav-numeric-prefix-edge`/`startup-multi-file` hung: a bare `q` sent
    /// right after such a message never reached the quit handler before
    /// this fix.
    #[test]
    fn pending_message_dismissed_by_next_command_key_not_swallowed() {
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        let mut out: Vec<u8> = Vec::new();

        app.apply(Action::GotoLine(Some(9999))).unwrap_err();
        app.pending_message = Some("Cannot seek to line number 9999  (press RETURN)".to_string());

        for c in "50".chars() {
            let (code, m) = key(c);
            let outcome = app.handle_key(&mut out, code, m).unwrap();
            assert_eq!(outcome, KeyOutcome::Continue);
        }
        assert!(
            app.pending_message.is_none(),
            "message must be dismissed by the first non-Enter key"
        );
        let (code, m) = key('G');
        app.handle_key(&mut out, code, m).unwrap();
        assert_eq!(app.top, 50);

        let (code, m) = key('q');
        let outcome = app.handle_key(&mut out, code, m).unwrap();
        assert_eq!(outcome, KeyOutcome::Quit);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pending_message_dismissed_by_enter_alone_does_not_also_scroll() {
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        let mut out: Vec<u8> = Vec::new();
        app.pending_message = Some("Pattern not found  (press RETURN)".to_string());
        let top_before = app.top;
        let outcome = app
            .handle_key(&mut out, KeyCode::Enter, KeyModifiers::NONE)
            .unwrap();
        assert_eq!(outcome, KeyOutcome::Continue);
        assert!(app.pending_message.is_none());
        assert_eq!(
            app.top, top_before,
            "Enter must only dismiss the message, not also scroll"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn jsonl_structured_search_and_filter() {
        let content = [
            r#"{"ts":"2026-08-23T00:01:00Z","level":"INFO","status":200,"service":"auth"}"#,
            r#"{"ts":"2026-08-23T00:02:00Z","level":"DEBUG","status":200,"service":"cart"}"#,
            r#"{"ts":"2026-08-23T00:03:00Z","level":"ERROR","status":500,"service":"payment"}"#,
            r#"{"ts":"2026-08-23T00:04:00Z","level":"INFO","status":200,"service":"auth"}"#,
            r#"{"ts":"2026-08-23T00:05:00Z","level":"ERROR","status":503,"service":"gateway"}"#,
        ]
        .join("\n");

        let p = std::env::temp_dir().join(format!("test_structured_{}.jsonl", std::process::id()));
        std::fs::write(&p, content.as_bytes()).unwrap();

        let mut app = App::new(test_opts(vec![p.clone()]));
        app.set_size(120, 24);
        let mut out: Vec<u8> = Vec::new();

        // 1. Initial draw triggers format sniffer
        app.draw(&mut out).unwrap();
        assert_eq!(app.projection.format, DetectedFormat::JsonLines);

        // 2. Structured search: /.status >= 500 should jump directly to line 3
        app.apply(Action::SearchInput(
            "/.status >= 500".to_string(),
            Direction::Forward,
            1,
        ))
        .unwrap();
        assert_eq!(app.top, 3);

        // 3. Structured filter: &.level == "ERROR"
        app.apply(Action::FilterInput(".level == \"ERROR\"".to_string()))
            .unwrap();
        assert!(app.active_filter.is_some());

        // 4. Tabular toggle: 'c'
        app.apply(Action::ToggleTabular).unwrap();
        assert!(app.tabular_view);

        // 5. Expand toggle: 'z'
        app.apply(Action::ToggleExpand).unwrap();
        assert!(app.projection.expanded_lines.contains(&3));

        // 6. Draw with projection
        out.clear();
        app.draw(&mut out).unwrap();
        let rendered = String::from_utf8_lossy(&out);
        assert!(rendered.contains("500") || rendered.contains("payment"));

        let _ = std::fs::remove_file(p);
    }

    // --- Structured-view regressions (docs/sqi/SQI_REPORT.md, probe T04-T17)

    fn jsonl_fixture() -> PathBuf {
        let levels = ["INFO", "DEBUG", "ERROR", "WARN"];
        let statuses = [200, 200, 500, 404];
        let mut content = String::new();
        for i in 1..=60 {
            content.push_str(&format!(
                "{{\"level\":\"{}\",\"status\":{},\"msg\":\"request {}\"}}\n",
                levels[i % 4],
                statuses[i % 4],
                i
            ));
        }
        temp_file(&content)
    }

    fn drawn_rows(app: &mut App) -> std::collections::BTreeMap<u16, String> {
        let mut out: Vec<u8> = Vec::new();
        app.draw(&mut out).unwrap();
        rows_from_output(&out)
    }

    #[test]
    fn selector_shaped_search_on_plain_text_is_an_ordinary_regex() {
        let mut content = String::new();
        for n in 1..=30 {
            content.push_str(&format!("line {n} .status >= 500 literal\n"));
        }
        let path = temp_file(&content);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app); // sniff: plain text
        app.apply(Action::ScrollLines(1)).unwrap();
        app.apply(Action::SearchInput(
            ".status >= 500".to_string(),
            Direction::Forward,
            1,
        ))
        .expect("less finds this text, so must rless");
        assert_eq!(app.top, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn structured_search_is_repeated_by_n_and_reversed_by_n_upper() {
        let path = jsonl_fixture();
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app); // sniff: jsonl
        app.apply(Action::SearchInput(
            ".status >= 500".to_string(),
            Direction::Forward,
            1,
        ))
        .unwrap();
        assert_eq!(app.top, 2);
        app.apply(Action::RepeatSearch(1)).unwrap();
        assert_eq!(app.top, 6);
        app.apply(Action::RepeatSearch(1)).unwrap();
        assert_eq!(app.top, 10);
        app.apply(Action::RepeatSearchReverse(1)).unwrap();
        assert_eq!(app.top, 6);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn n_after_upper_n_keeps_the_original_search_direction() {
        let path = lines_fixture(200);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        // Lines 010, 100, 110..119, 200... contain "10"; use a sparse one.
        app.apply(Action::SearchInput(
            "^0[2468]0 ".to_string(),
            Direction::Forward,
            1,
        ))
        .unwrap();
        assert_eq!(app.top, 20);
        app.apply(Action::RepeatSearch(1)).unwrap();
        assert_eq!(app.top, 40);
        app.apply(Action::RepeatSearch(1)).unwrap();
        assert_eq!(app.top, 60);
        app.apply(Action::RepeatSearchReverse(1)).unwrap();
        assert_eq!(app.top, 40);
        app.apply(Action::RepeatSearch(1)).unwrap();
        assert_eq!(app.top, 60, "n after N must search forward again");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn each_file_gets_its_own_format_and_columns() {
        let jsonl = jsonl_fixture();
        let csv = temp_file("name,age,city\nalice,30,tokyo\nbob,25,osaka\n");
        let mut app = App::new(test_opts(vec![jsonl.clone(), csv.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app);
        assert_eq!(app.projection.format, DetectedFormat::JsonLines);
        app.apply(Action::NextFile(1)).unwrap();
        app.apply(Action::ToggleTabular).unwrap();
        let rows = drawn_rows(&mut app);
        assert_eq!(app.projection.format, DetectedFormat::Csv);
        let names: Vec<_> = app.projection.columns.iter().map(|c| &c.name).collect();
        assert_eq!(names, ["name", "age", "city"]);
        assert_eq!(rows[&1], "alice│30  │tokyo│");
        let _ = std::fs::remove_file(jsonl);
        let _ = std::fs::remove_file(csv);
    }

    #[test]
    fn invalid_filter_is_rejected_and_the_previous_filter_kept() {
        let path = jsonl_fixture();
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app);
        app.apply(Action::FilterInput("ERROR".to_string())).unwrap();
        let err = app
            .apply(Action::FilterInput("(unclosed".to_string()))
            .unwrap_err();
        assert_eq!(err, "Invalid pattern");
        let kept = app.active_filter.as_ref().map(|f| f.source.as_str());
        assert_eq!(kept, Some("ERROR"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn plain_text_feedback_does_not_claim_a_structured_effect() {
        let path = lines_fixture(50);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app);
        app.apply(Action::ToggleTabular).unwrap();
        assert!(
            app.pending_message
                .as_deref()
                .unwrap()
                .contains("no effect")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn column_mode_prompt_names_the_focus_and_clamps_it() {
        let path = jsonl_fixture();
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        let mut out: Vec<u8> = Vec::new();
        drawn_rows(&mut app);
        for c in ['c', 'x', 'l', 'l', 'l', 'l', 'l'] {
            let (code, m) = key(c);
            app.handle_key(&mut out, code, m).unwrap();
        }
        assert_eq!(app.focused_column, 2, "3 columns: focus stops at the last");
        let rows = drawn_rows(&mut app);
        assert!(rows[&23].starts_with("Columns: 3/3 msg"), "{}", rows[&23]);

        // Feedback of an action in column mode is shown, not hidden.
        let (code, m) = key('p');
        app.handle_key(&mut out, code, m).unwrap();
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&23], "Column 'msg' pinned");
        let (code, m) = key('s');
        app.handle_key(&mut out, code, m).unwrap();
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&23], "Sorted 60 lines by 'msg' (ascending)");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn column_names_in_messages_are_escaped() {
        let path = temp_file("{\"le\u{7}vel\":\"x\"}\n{\"le\u{7}vel\":\"y\"}\n");
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app);
        assert_eq!(app.projection.format, DetectedFormat::JsonLines);
        app.apply(Action::PinColumn).unwrap();
        assert_eq!(
            app.pending_message.as_deref(),
            Some("Column 'le^Gvel' pinned")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn toggles_without_a_visible_effect_explain_themselves() {
        let path = lines_fixture(30);
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app);
        app.apply(Action::ToggleExpand).unwrap();
        assert!(
            app.pending_message
                .as_deref()
                .unwrap()
                .contains("JSON lines only")
        );
        assert!(app.projection.expanded_lines.is_empty());
        app.apply(Action::ToggleStickyHeader).unwrap();
        assert!(
            app.pending_message
                .as_deref()
                .unwrap()
                .contains("no effect")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cycling_the_format_derives_that_formats_columns() {
        let path = temp_file("a,b\n1,2\n3,4\n");
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app);
        assert_eq!(app.projection.format, DetectedFormat::Csv);
        // csv -> tsv -> logfmt -> plain -> jsonl -> csv
        for _ in 0..5 {
            app.apply(Action::ToggleFormat).unwrap();
        }
        assert_eq!(app.projection.format, DetectedFormat::Csv);
        let names: Vec<_> = app.projection.columns.iter().map(|c| &c.name).collect();
        assert_eq!(names, ["a", "b"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn projected_rows_never_exceed_the_terminal_width() {
        let path = jsonl_fixture();
        let mut app = App::new(test_opts(vec![path.clone()]));
        app.set_size(20, 6);
        drawn_rows(&mut app);
        app.apply(Action::ToggleTabular).unwrap();
        app.apply(Action::ToggleStickyHeader).unwrap();
        let rows = drawn_rows(&mut app);
        for (r, text) in &rows {
            assert!(
                unicode_width::UnicodeWidthStr::width(text.as_str()) <= 20,
                "row {r} overflows: {text:?}"
            );
        }
        assert!(rows[&1].ends_with('>'), "a cut row carries the chop marker");
        let _ = std::fs::remove_file(path);
    }

    // --- Feature expansion: filter, marks, sideways scroll, options, ----
    // --- follow, help, :e, sort, initial commands, -F -------------------

    fn app_on(path: &std::path::Path, w: u16, h: u16) -> App {
        let mut app = App::new(test_opts(vec![path.to_path_buf()]));
        app.set_size(w, h);
        drawn_rows(&mut app);
        app
    }

    #[test]
    fn plain_text_filter_shows_scrolls_and_searches_only_matching_lines() {
        // Every 10th line carries TEN; the window has 5 content rows.
        let mut content = String::new();
        for n in 1..=100 {
            let tag = if n % 10 == 0 { " TEN" } else { "" };
            content.push_str(&format!("line {n}{tag}\n"));
        }
        let path = temp_file(&content);
        let mut app = app_on(&path, 40, 6);
        app.apply(Action::FilterInput("TEN".to_string())).unwrap();
        assert_eq!(app.top, 10, "moves to the first shown line");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "line 10 TEN");
        assert_eq!(rows[&4], "line 50 TEN");

        app.apply(Action::ScrollLines(2)).unwrap();
        assert_eq!(app.top, 30, "j counts shown lines");
        app.apply(Action::ScrollWindow(None, true)).unwrap();
        assert_eq!(app.top, 60, "clamped to the last window of shown lines");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&4], "line 100 TEN");
        assert!(app.last_bottom.1, "at the end");
        app.apply(Action::ScrollLines(-1)).unwrap();
        assert_eq!(app.top, 50);
        app.apply(Action::GotoLine(None)).unwrap();
        assert_eq!(app.top, 10);
        app.apply(Action::GotoLast(None)).unwrap();
        assert_eq!(app.top, 60);

        // A search only lands on shown lines: "line 5" first matches line 5,
        // which is hidden; the first shown match is line 50.
        app.apply(Action::GotoLine(None)).unwrap();
        app.apply(Action::SearchInput("line 5".into(), Direction::Forward, 1))
            .unwrap();
        assert_eq!(app.top, 50);

        // Inverted, then cleared.
        app.apply(Action::FilterInput("!TEN".to_string())).unwrap();
        let rows = drawn_rows(&mut app);
        assert!(
            rows.values().take(5).all(|r| !r.contains("TEN")),
            "{rows:?}"
        );
        assert_eq!(app.pending_message.as_deref(), Some("Filter: &!TEN"));
        app.apply(Action::FilterInput(String::new())).unwrap();
        assert!(app.active_filter.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_filter_with_no_match_says_so() {
        let path = lines_fixture(30);
        let mut app = app_on(&path, 80, 24);
        app.apply(Action::FilterInput("NOWHERE".to_string()))
            .unwrap();
        assert_eq!(
            app.pending_message.as_deref(),
            Some("Filter: &NOWHERE (no matching lines)")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn csv_filter_and_search_use_header_column_names() {
        let path = temp_file("id,status,path\n1,200,/a\n2,503,/b\n3,200,/c\n4,500,/d\n");
        let mut app = app_on(&path, 80, 24);
        assert_eq!(app.projection.format, DetectedFormat::Csv);
        app.apply(Action::SearchInput(
            ".status >= 500".into(),
            Direction::Forward,
            1,
        ))
        .unwrap();
        assert_eq!(app.top, 3);
        app.apply(Action::FilterInput(".status >= 500".to_string()))
            .unwrap();
        app.apply(Action::ToggleTabular).unwrap();
        let rows = drawn_rows(&mut app);
        let shown: Vec<_> = rows.values().filter(|r| r.contains('/')).cloned().collect();
        assert_eq!(shown.len(), 2, "{rows:?}");
        assert!(shown[0].contains("503") && shown[1].contains("500"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn marks_and_quote_quote_return_to_earlier_places() {
        let path = lines_fixture(200);
        let mut app = app_on(&path, 80, 24);
        app.apply(Action::GotoLine(Some(50))).unwrap();
        app.apply(Action::SetMark('a')).unwrap();
        app.apply(Action::GotoLine(Some(120))).unwrap();
        app.apply(Action::GotoMark('a')).unwrap();
        assert_eq!(app.top, 50);
        app.apply(Action::GotoMark('\'')).unwrap();
        assert_eq!(app.top, 120, "'' goes back to before the mark jump");
        app.apply(Action::GotoMark('\'')).unwrap();
        assert_eq!(app.top, 50, "and '' again toggles");
        app.apply(Action::GotoMark('$')).unwrap();
        assert_eq!(app.top, 178);
        app.apply(Action::GotoMark('^')).unwrap();
        assert_eq!(app.top, 1);
        assert_eq!(
            app.apply(Action::GotoMark('z')).unwrap_err(),
            "Mark not set"
        );
        assert_eq!(
            app.apply(Action::SetMark('1')).unwrap_err(),
            "Invalid mark letter"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn marks_and_positions_are_kept_per_file() {
        let a = lines_fixture(200);
        let b = lines_fixture(100);
        let mut app = App::new(test_opts(vec![a.clone(), b.clone()]));
        app.set_size(80, 24);
        app.apply(Action::GotoLine(Some(40))).unwrap();
        app.apply(Action::SetMark('m')).unwrap();
        app.apply(Action::NextFile(1)).unwrap();
        assert_eq!(app.top, 1);
        app.apply(Action::GotoLine(Some(30))).unwrap();
        app.apply(Action::PrevFile(1)).unwrap();
        assert_eq!(app.top, 40, ":p returns to where file 1 was left");
        app.apply(Action::NextFile(1)).unwrap();
        assert_eq!(app.top, 30);
        app.apply(Action::GotoMark('m')).unwrap();
        assert_eq!((app.file_index, app.top), (0, 40), "a mark switches file");
        app.apply(Action::GotoFile(Some(2))).unwrap();
        assert_eq!(app.file_index, 1);
        assert_eq!(
            app.apply(Action::GotoFile(Some(9))).unwrap_err(),
            "No such file"
        );
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn percent_goes_proportionally_and_never_past_the_last_window() {
        let path = lines_fixture(200);
        let mut app = app_on(&path, 80, 24);
        app.apply(Action::GotoPercent(Some(50))).unwrap();
        assert_eq!(app.top, 100);
        app.apply(Action::GotoPercent(Some(100))).unwrap();
        assert_eq!(app.top, 178);
        app.apply(Action::GotoPercent(None)).unwrap();
        assert_eq!(app.top, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn option_toggles_change_the_view_and_the_case_rule() {
        let path = temp_file("alpha\nBeta\ngamma\n");
        let mut app = app_on(&path, 80, 24);
        assert_eq!(
            app.apply(Action::SearchInput("beta".into(), Direction::Forward, 1))
                .unwrap_err(),
            "Pattern not found"
        );
        app.apply(Action::ToggleOption('i')).unwrap();
        app.apply(Action::SearchInput("beta".into(), Direction::Forward, 1))
            .unwrap();
        assert_eq!(app.top, 2, "-i: lowercase pattern ignores case");
        app.apply(Action::GotoLine(None)).unwrap();
        assert!(
            app.apply(Action::SearchInput("BETA".into(), Direction::Forward, 1))
                .is_err(),
            "-i: an uppercase letter makes the pattern case-sensitive"
        );
        app.apply(Action::ToggleOption('I')).unwrap();
        app.apply(Action::SearchInput("BETA".into(), Direction::Forward, 1))
            .unwrap();
        assert_eq!(app.top, 2);

        app.apply(Action::ToggleOption('N')).unwrap();
        let rows = drawn_rows(&mut app);
        assert!(rows[&0].starts_with("      2 Beta"), "{:?}", rows[&0]);
        assert_eq!(app.pending_message.as_deref(), Some("Line numbers: on"));
        assert!(app.apply(Action::ToggleOption('z')).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sideways_scroll_moves_by_half_a_screen_and_stops_at_the_longest_line() {
        let line = format!("start{}end", "-".repeat(100));
        let path = temp_file(&format!("{line}\nshort\n"));
        let mut opts = test_opts(vec![path.clone()]);
        opts.chop_long_lines = true;
        let mut app = App::new(opts);
        app.set_size(40, 6);
        app.apply(Action::ScrollHorizontal(None, true)).unwrap();
        assert_eq!(app.hshift, 20);
        // A fresh short file is bottom-anchored: the long line is row 3.
        let rows = drawn_rows(&mut app);
        assert!(rows[&3].starts_with("-----"), "{rows:?}");
        app.apply(Action::HorizontalEnd).unwrap();
        assert_eq!(app.hshift, 108 - 40);
        let rows = drawn_rows(&mut app);
        assert!(rows[&3].ends_with("end"), "{:?}", rows[&3]);
        app.apply(Action::ScrollHorizontal(None, true)).unwrap();
        assert_eq!(app.hshift, 68, "no scrolling into emptiness");
        app.apply(Action::ScrollHorizontal(Some(8), false)).unwrap();
        assert_eq!(app.hshift, 60);
        app.apply(Action::HorizontalHome).unwrap();
        assert_eq!(app.hshift, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn help_page_replaces_the_view_until_q() {
        let path = lines_fixture(200);
        let mut app = app_on(&path, 80, 24);
        app.apply(Action::GotoLine(Some(77))).unwrap();
        let mut out = Vec::new();
        let (c, m) = key('h');
        app.handle_key(&mut out, c, m).unwrap();
        let rows = drawn_rows(&mut app);
        assert!(rows[&0].starts_with("rless "), "{:?}", rows[&0]);
        assert_eq!(rows[&23], HELP_PROMPT);
        app.apply(Action::SearchInput("follow".into(), Direction::Forward, 1))
            .unwrap();
        let (c, m) = key('q');
        assert_eq!(
            app.handle_key(&mut out, c, m).unwrap(),
            KeyOutcome::Continue,
            "q closes help instead of quitting"
        );
        assert!(app.help.is_none());
        assert_eq!(app.top, 77);
        let (c, m) = key('q');
        assert_eq!(app.handle_key(&mut out, c, m).unwrap(), KeyOutcome::Quit);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn examine_opens_a_new_file_after_the_current_one() {
        let a = lines_fixture(10);
        let b = temp_file("other file\n");
        let mut app = app_on(&a, 80, 24);
        app.apply(Action::Examine(b.display().to_string())).unwrap();
        assert_eq!((app.file_index, app.files.len()), (1, 2));
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "other file");
        // Examining a listed file switches to it.
        app.apply(Action::Examine(a.display().to_string())).unwrap();
        assert_eq!((app.file_index, app.files.len()), (0, 2));
        let err = app
            .apply(Action::Examine("/nonexistent/rless-x".into()))
            .unwrap_err();
        assert!(err.contains("No such file"), "{err}");
        assert_eq!(app.files.len(), 2, "a missing file is not added");
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn column_sort_cycles_ascending_descending_and_off() {
        let path = temp_file("name,ms\nb,250\na,1200\nc,90\nd,\n");
        let mut app = app_on(&path, 80, 24);
        app.apply(Action::ToggleTabular).unwrap();
        app.apply(Action::NextColumn).unwrap(); // focus "ms"
        app.apply(Action::SortColumn).unwrap();
        let order = |app: &mut App| {
            let rows = drawn_rows(app);
            (0..5u16).map(|r| rows[&r].clone()).collect::<Vec<_>>()
        };
        let rows = order(&mut app);
        assert!(rows[0].starts_with("name"), "header stays first: {rows:?}");
        assert!(rows[1].starts_with("c "));
        assert!(rows[2].starts_with("b "));
        assert!(rows[3].starts_with("a "));
        assert!(rows[4].starts_with("d "), "blank sorts last");
        assert_eq!(
            app.pending_message.as_deref(),
            Some("Sorted 4 lines by 'ms' (ascending)")
        );
        app.apply(Action::SortColumn).unwrap();
        let rows = order(&mut app);
        assert!(rows[1].starts_with("a ") && rows[4].starts_with("d "));
        app.apply(Action::SortColumn).unwrap();
        let rows = order(&mut app);
        assert!(rows[1].starts_with("b "), "file order again: {rows:?}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_info_reports_position_size_and_applied_views() {
        let path = jsonl_fixture();
        let mut app = app_on(&path, 200, 24);
        app.apply(Action::FilterInput(".status >= 500".into()))
            .unwrap();
        drawn_rows(&mut app);
        app.apply(Action::FileInfo).unwrap();
        let info = app.pending_message.clone().unwrap();
        assert!(info.contains(&path.display().to_string()), "{info}");
        assert!(info.contains("lines 2-"), "{info}");
        assert!(info.contains("/60 byte "), "{info}");
        assert!(info.contains("[jsonl] [filter &.status >= 500]"), "{info}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn follow_shows_lines_appended_to_the_file() {
        let path = lines_fixture(30);
        let mut app = app_on(&path, 80, 11);
        app.apply(Action::Follow).unwrap();
        assert!(app.following);
        assert_eq!(app.top, 21);
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&10], FOLLOW_PROMPT);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"appended 31\nappended 32\n").unwrap();
        drop(f);
        assert!(app.follow_tick().unwrap());
        assert_eq!(app.top, 23);
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&9], "appended 32");
        assert!(!app.follow_tick().unwrap(), "nothing new, no redraw");
        let mut out = Vec::new();
        let (c, m) = key('j');
        app.handle_key(&mut out, c, m).unwrap();
        assert!(!app.following, "a key stops following");
        assert_eq!(app.top, 23, "and is consumed by stopping");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_picks_up_a_rewritten_file() {
        let path = temp_file("old 1\nold 2\n");
        let mut app = app_on(&path, 80, 24);
        std::fs::write(&path, "new 1\n").unwrap();
        app.apply(Action::Reload).unwrap();
        let rows = drawn_rows(&mut app);
        assert!(rows.values().any(|r| r == "new 1"), "{rows:?}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn initial_commands_run_before_the_first_screen() {
        let path = lines_fixture(200);
        let mut opts = test_opts(vec![path.clone()]);
        opts.initial_commands = vec!["/^050 ".to_string(), "5j".to_string()];
        let mut app = App::new(opts);
        app.set_size(80, 24);
        assert_eq!(app.run_initial_commands(), KeyOutcome::Continue);
        assert_eq!(app.top, 55);

        let mut opts = test_opts(vec![path.clone()]);
        opts.initial_commands = vec!["/NOT_THERE".to_string()];
        let mut app = App::new(opts);
        app.run_initial_commands();
        assert_eq!(
            app.pending_message.as_deref(),
            Some("Pattern not found  (press RETURN)")
        );

        let mut opts = test_opts(vec![path.clone()]);
        opts.initial_commands = vec!["q".to_string()];
        assert_eq!(App::new(opts).run_initial_commands(), KeyOutcome::Quit);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn quit_if_one_screen_prints_short_input_only() {
        let short = temp_file("a\tb\nsecond \u{1}\n");
        let mut app = App::new(test_opts(vec![short.clone()]));
        app.set_size(80, 24);
        let out = app.one_screen_output().unwrap().unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("a       b\nsecond "), "{text:?}");
        assert!(text.ends_with("^A\x1b[39m\x1b[49m\n"), "{text:?}");

        let long = lines_fixture(30);
        let mut app = App::new(test_opts(vec![long.clone()]));
        app.set_size(80, 24);
        assert!(app.one_screen_output().unwrap().is_none());
        let _ = std::fs::remove_file(short);
        let _ = std::fs::remove_file(long);
    }

    #[test]
    fn editor_command_prefers_visual_then_editor_then_vi() {
        let p = std::path::Path::new("/tmp/f.log");
        assert_eq!(
            editor_command(Some("code --wait"), Some("nano"), p, 42),
            ["code", "--wait", "+42", "/tmp/f.log"]
        );
        assert_eq!(
            editor_command(Some(" "), Some("nano"), p, 1),
            ["nano", "+1", "/tmp/f.log"]
        );
        assert_eq!(editor_command(None, None, p, 7), ["vi", "+7", "/tmp/f.log"]);
    }

    #[test]
    fn edit_is_refused_for_stdin_and_queued_for_a_file() {
        let path = lines_fixture(30);
        let mut app = app_on(&path, 80, 24);
        app.apply(Action::GotoLine(Some(9))).unwrap();
        app.apply(Action::Edit).unwrap();
        assert_eq!(app.pending_edit, Some((path.clone(), 9)));
        let mut stdin_app = App::new(test_opts(vec![]));
        assert_eq!(
            stdin_app.apply(Action::Edit).unwrap_err(),
            "Cannot edit standard input"
        );
        let _ = std::fs::remove_file(path);
    }
}
