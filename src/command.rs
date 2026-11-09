//! Main loop / state machine. Owns current top-of-window line, sticky
//! half-window N, current file index and the multi-file list, the
//! "first draw of this process" flag, and dispatches decoded `Action`s to
//! `buffer`/`screen`/`search`/`prompt`.
//!
//! With no filter and no sort active, every command keeps the exact
//! less-compatible line semantics the parity baseline checks. Once a filter
//! (`&`) or a column sort is active, navigation and search run over the
//! positions of a `view::ViewIndex` instead, so hidden lines are never
//! visited.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use regex::{Regex, RegexBuilder};

use crate::buffer::{Buffer, Growth, Source};
use crate::cli::{BellMode, CaseMode, KEYS_HELP, Options, PromptMode};
use crate::input::{Action, Decoded, Decoder};
use crate::projection::{ProjectionEngine, WindowLines, clip_to_width, escape_controls};
use crate::prompt::{self, PromptContext};
use crate::screen::{self, RenderOpts};
use crate::search::{self, Direction};
use crate::selector::Query;
use crate::sniffer::{DetectedFormat, FormatSniffer};
use crate::view::{LineFilter, SortKey, ViewIndex};

struct FileEntry {
    buffer: Buffer,
    /// How this file was being viewed when another file was switched to,
    /// so `:p`/`:n` back returns to the same place (as less does).
    saved: Option<SavedView>,
}

impl FileEntry {
    fn new(source: Source) -> Self {
        FileEntry {
            buffer: Buffer::new(source),
            saved: None,
        }
    }
}

/// The per-file part of the view, kept across file switches. The filter is
/// not part of it: it applies to every file, as in less.
struct SavedView {
    top: usize,
    projection: ProjectionEngine,
    tabular_view: bool,
    sort: Option<SortSpec>,
    focused_column: usize,
    sniffer_checked: bool,
    hscroll: usize,
}

impl FileEntry {
    fn display(&self) -> Option<String> {
        self.buffer.display_name()
    }
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
    Regex(Regex),
    /// A selector query (`/.status >= 500`) on a structured file.
    Structured(Query),
}

/// The column the view is sorted by.
#[derive(Debug, Clone, PartialEq)]
struct SortSpec {
    column: String,
    descending: bool,
}

/// A full-screen text page drawn over the file (help, column statistics).
struct Overlay {
    title: String,
    lines: Vec<String>,
    top: usize,
}

/// A place in the input: for marks and `''`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    file: usize,
    line: usize,
}

/// How many leading lines the format sniffer and column derivation sample.
const SNIFF_SAMPLE_LINES: usize = 16;

/// How often follow mode looks for new input.
const FOLLOW_POLL: Duration = Duration::from_millis(200);

/// Most-frequent values listed by the column statistics page.
const STATS_TOP_VALUES: usize = 15;

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
    /// Structured-view state of the *current* file; reset on `:n`/`:p`.
    projection: ProjectionEngine,
    tabular_view: bool,
    /// The `&` filter. Kept across files (as in less); read as a selector
    /// on a structured file and as a regex elsewhere.
    filter: Option<LineFilter>,
    /// Column sort of the current file (cleared on `:n`/`:p`).
    sort: Option<SortSpec>,
    /// Lines shown while a filter or sort is active (derived state: `None`
    /// whenever it must be rebuilt).
    view: Option<ViewIndex>,
    case: CaseMode,
    /// Index into `projection.columns` of the column-mode focus.
    focused_column: usize,
    /// Whether the current file's format has been sniffed (once per file,
    /// on its first draw).
    sniffer_checked: bool,
    marks: HashMap<char, Position>,
    /// Where the last jump (g, G, search, mark) started, for `''`.
    previous_position: Option<Position>,
    /// Display columns scrolled off to the left.
    hscroll: usize,
    following: bool,
    overlay: Option<Overlay>,
    /// The last frame's bottom line and end-of-input state (for `=`).
    last_bottom: (usize, bool),
    /// `v` asked to open this file at this line in the editor; the main
    /// loop, which owns the terminal, runs it.
    pending_edit: Option<(PathBuf, usize)>,
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
        let mut projection = ProjectionEngine::new(DetectedFormat::PlainText);
        projection.raw_ansi = opts.raw_control_chars;
        let case = opts.case;
        App {
            opts,
            files,
            file_index: 0,
            top: 1,
            sticky_half: None,
            first_render_done: false,
            filename_shown: true,
            pending_message: None,
            last_search: None,
            width: 80,
            height: 24,
            decoder: Decoder::new(),
            projection,
            tabular_view: false,
            filter: None,
            sort: None,
            view: None,
            case,
            focused_column: 0,
            sniffer_checked: false,
            marks: HashMap::new(),
            previous_position: None,
            hscroll: 0,
            following: false,
            overlay: None,
            last_bottom: (1, false),
            pending_edit: None,
            frame_buf: Vec::new(),
        }
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
            hscroll: self.hscroll,
        }
    }

    fn current(&mut self) -> &mut FileEntry {
        &mut self.files[self.file_index]
    }

    fn position(&self) -> Position {
        Position {
            file: self.file_index,
            line: self.top,
        }
    }

    /// Eagerly validates the very first file before any terminal state is
    /// touched, matching the observed less behavior of exiting near-
    /// instantly on a missing file (§2 `exit-codes`).
    pub fn validate_first_file(&mut self) -> Result<(), String> {
        self.files[0].buffer.ensure_lines_read(1)?;
        Ok(())
    }

    /// `-F`: when there is one input and it fits on one screen, returns it
    /// rendered as the pager would show it (so the caller can print it and
    /// exit instead of starting the pager).
    pub fn one_screen_output(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.files.len() != 1 {
            return Ok(None);
        }
        let rows = self.content_rows();
        let opts = self.render_opts();
        let buffer = &mut self.files[0].buffer;
        // A pipe must not look short just because its producer is slow to
        // start (`git log` computing): wait for a screenful or the end.
        while buffer.is_open_stream() && buffer.lines_cached() <= rows {
            buffer.ensure_lines_read(rows + 1)?;
        }
        let win = screen::render_window(buffer, 1, rows, &opts, None)?;
        if !win.at_eof || win.rows.len() > rows {
            return Ok(None);
        }
        let mut out = Vec::new();
        for row in &win.rows {
            write_spans(&mut out, row).map_err(|e| e.to_string())?;
            if self.opts.raw_control_chars {
                out.extend_from_slice(b"\x1b[0m");
            }
            out.push(b'\n');
        }
        // An empty line renders as no row at all; an empty input prints nothing.
        if buffer.lines_cached() == 0 {
            out.clear();
        }
        Ok(Some(out))
    }

    /// Runs the `+CMD` / `-p` startup commands before the first screen, as
    /// if their keys had been typed. A failure becomes the first screen's
    /// message.
    pub fn run_initial_commands(&mut self) {
        let commands = std::mem::take(&mut self.opts.initial_commands);
        if commands.is_empty() {
            return;
        }
        self.ensure_format_sniffed();
        for cmd in commands {
            let mut actions = Vec::new();
            if !cmd.is_empty() && cmd.chars().all(|c| c.is_ascii_digit()) {
                actions.push(Action::GotoLine(cmd.parse().ok()));
            } else {
                for c in cmd.chars() {
                    if let Decoded::Action(a) =
                        self.decoder.feed(KeyCode::Char(c), KeyModifiers::NONE)
                    {
                        actions.push(a);
                    }
                }
                // A pattern or file name typed on the command line ends there.
                if self.decoder.preview().is_some()
                    && let Decoded::Action(a) =
                        self.decoder.feed(KeyCode::Enter, KeyModifiers::NONE)
                {
                    actions.push(a);
                }
            }
            for action in actions {
                if action == Action::Quit {
                    continue;
                }
                if let Err(msg) = self.apply(action) {
                    self.pending_message = Some(format!("{}  (press RETURN)", msg));
                }
            }
        }
    }

    fn binary_prompt_text(&self) -> String {
        let name = self.files[0].display().unwrap_or_default();
        format!("\"{}\" may be a binary file.  See it anyway?", name)
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

        self.run_initial_commands();
        self.draw(out)?;
        loop {
            let waiting = self.following || self.files[self.file_index].buffer.is_open_stream();
            if waiting && !event::poll(FOLLOW_POLL)? {
                let redraw = if self.following {
                    self.follow_tick()
                } else {
                    self.stream_tick()
                };
                if redraw {
                    self.draw(out)?;
                }
                continue;
            }
            match event::read()? {
                Event::Resize(w, h) => {
                    self.set_size(w, h);
                    self.draw(out)?;
                }
                Event::Key(k) => {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    if self.following {
                        // Any key stops following; keys that are not
                        // "stop" keys then also run as usual (so `q` quits).
                        self.stop_follow();
                        let is_stop = matches!(k.code, KeyCode::Esc | KeyCode::Char('F'))
                            || (k.code == KeyCode::Char('c')
                                && k.modifiers.contains(KeyModifiers::CONTROL));
                        if is_stop {
                            self.draw(out)?;
                            continue;
                        }
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
        if self.overlay.is_some() {
            self.overlay_key(code);
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

    /// Keys while the help or statistics page is shown: scroll it, or close
    /// it with any other key.
    fn overlay_key(&mut self, code: KeyCode) {
        let page = self.content_rows();
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        let max_top = overlay.lines.len().saturating_sub(page);
        match code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => overlay.top += 1,
            KeyCode::Char('k') | KeyCode::Up => overlay.top = overlay.top.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::Char('f') | KeyCode::PageDown => overlay.top += page,
            KeyCode::Char('b') | KeyCode::PageUp => overlay.top = overlay.top.saturating_sub(page),
            KeyCode::Char('g') => overlay.top = 0,
            KeyCode::Char('G') => overlay.top = max_top,
            _ => {
                self.overlay = None;
                return;
            }
        }
        overlay.top = overlay.top.min(max_top);
    }

    /// Dispatches `action`, then marks the fresh-process bottom-anchor
    /// window closed once the view actually moves (top-of-window line or
    /// current file changes). §3 item 1's flag must NOT be consumed by the
    /// mere act of drawing: `prompt-error-bell-*` shows a failed EOF-clamp
    /// `j` (no-op, screen byte-identical) must still redraw bottom-anchored
    /// with no tilde — only a real position/file change may switch to
    /// top-anchor+tilde on the next draw.
    fn apply(&mut self, action: Action) -> Result<(), String> {
        let old_top = self.top;
        let old_file_index = self.file_index;
        let is_jump = matches!(
            action,
            Action::GotoLine(_)
                | Action::GotoLast(_)
                | Action::SearchInput(..)
                | Action::RepeatSearch(_)
                | Action::RepeatSearchReverse(_)
                | Action::GotoMark(_)
                | Action::GotoPercent(_)
                | Action::GotoFile(_)
                | Action::Examine(_)
        );
        let before = self.position();
        let result = self.apply_inner(action);
        let moved = self.top != old_top || self.file_index != old_file_index;
        if result.is_ok() && moved {
            self.first_render_done = true;
            if is_jump {
                self.previous_position = Some(before);
            }
        }
        result
    }

    fn apply_inner(&mut self, action: Action) -> Result<(), String> {
        let is_nav = matches!(
            action,
            Action::ScrollLines(_)
                | Action::ScrollWindow(..)
                | Action::ScrollHalfWindow(..)
                | Action::GotoLine(_)
                | Action::GotoLast(_)
        );
        if is_nav {
            self.ensure_view()?;
            if self.view.is_some() {
                return self.apply_view_nav(action);
            }
        }
        match action {
            Action::ScrollLines(n) => {
                let proposed = shift(self.top, n);
                self.top = self.clamp_relative(proposed)?;
            }
            Action::ScrollWindow(n, forward) => {
                let mag = n.unwrap_or(self.content_rows() as u64) as i64;
                let signed = if forward { mag } else { -mag };
                let proposed = shift(self.top, signed);
                self.top = self.clamp_relative(proposed)?;
            }
            Action::ScrollHalfWindow(n, forward) => {
                let mag = self.half_window_magnitude(n);
                let signed = if forward { mag } else { -mag };
                let proposed = shift(self.top, signed);
                self.top = self.clamp_relative(proposed)?;
            }
            Action::GotoLine(n) => {
                let target = n.unwrap_or(1);
                self.top = self.goto_absolute(target)?;
            }
            Action::GotoLast(n) => match n {
                Some(target) => {
                    self.top = self.goto_absolute(target)?;
                }
                None => {
                    let total = self.current().buffer.total_lines()?;
                    let content_rows = self.content_rows();
                    self.top = if total > content_rows {
                        total - content_rows + 1
                    } else {
                        1
                    };
                }
            },
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
                    self.ensure_view()?;
                    if self.view.is_some() {
                        self.normalize_view_top()?;
                    }
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
                self.sort = None;
                self.view = None;
                self.pending_message = Some(format!("Format: {}", next.name()));
            }
            Action::ToggleColumnMode => {
                self.focused_column = 0;
            }
            Action::PrevColumn => {
                if self.focused_column > 0 {
                    self.focused_column -= 1;
                }
                self.scroll_focus_into_view();
            }
            Action::NextColumn => {
                if self.focused_column + 1 < self.projection.columns.len() {
                    self.focused_column += 1;
                }
                self.scroll_focus_into_view();
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
            Action::ColumnStats => self.show_column_stats()?,
            Action::FilterInput(buf) => self.set_filter(&buf)?,
            Action::NextFile(n) => self.switch_file(n as i64)?,
            Action::PrevFile(n) => self.switch_file(-(n as i64))?,
            Action::Follow => self.start_follow()?,
            Action::SetMark(c) => {
                self.marks.insert(c, self.position());
                self.pending_message = Some(format!("Mark '{c}' set at line {}", self.top));
            }
            Action::GotoMark(c) => self.goto_mark(c)?,
            Action::Help => self.show_help(),
            Action::ToggleOption(c) => self.toggle_option(c)?,
            Action::HScroll(n, right) => self.horizontal_scroll(n, right),
            Action::SaveView { path, force } => self.save_view(&path, force)?,
            Action::GotoPercent(n) => self.goto_percent(n.unwrap_or(0))?,
            Action::FileInfo => {
                // Cut on the right: the name and position come first and
                // matter most (a prompt would lose them to a left cut).
                let info = self.file_info()?;
                self.pending_message = Some(clip_to_width(info, self.width as usize));
            }
            Action::Reload => self.reload()?,
            Action::Edit => {
                let entry = &self.files[self.file_index];
                let Some(path) = entry.buffer.path() else {
                    return Err("Cannot edit standard input".to_string());
                };
                self.pending_edit = Some((path.clone(), self.top));
            }
            Action::Examine(name) => self.examine(&name)?,
            Action::GotoFile(n) => {
                let target = n.unwrap_or(1).saturating_sub(1) as usize;
                if target >= self.files.len() {
                    return Err("No such file".to_string());
                }
                self.switch_file(target as i64 - self.file_index as i64)?;
            }
            Action::HScrollHome => {
                self.hscroll = 0;
                self.projection.hscroll = 0;
            }
            Action::HScrollEnd => {
                if !self.opts.chop_long_lines && !self.table_shown() {
                    self.pending_message =
                        Some("Nothing to scroll: lines wrap (try -S)".to_string());
                } else {
                    self.hscroll = self.max_hscroll()?;
                    self.projection.hscroll = self.hscroll;
                }
            }
            Action::Quit => unreachable!("handled by caller"),
        }
        Ok(())
    }

    fn half_window_magnitude(&mut self, n: Option<u64>) -> i64 {
        (match n {
            Some(v) => {
                self.sticky_half = Some(v);
                v
            }
            None => self.half_window(),
        }) as i64
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
        let buffer = &mut self.current().buffer;
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
        let buffer = &mut self.current().buffer;
        buffer.ensure_lines_read(t)?;
        if buffer.line(t).is_some() {
            Ok(t)
        } else {
            Err(format!("Cannot seek to line number {}", target))
        }
    }

    // ------------------------------------------------- filtered / sorted view

    /// True when a filter or an applicable sort asks for a view.
    fn view_wanted(&self) -> bool {
        self.filter.is_some() || self.sort_applies()
    }

    fn sort_applies(&self) -> bool {
        self.sort.as_ref().is_some_and(|s| {
            self.projection.format.is_structured()
                && self.projection.columns.iter().any(|c| c.name == s.column)
        })
    }

    /// Builds the view if one is wanted and none is current (or a sorted
    /// one went stale because follow mode appended lines).
    fn ensure_view(&mut self) -> Result<(), String> {
        if !self.view_wanted() {
            self.view = None;
            return Ok(());
        }
        if self
            .view
            .as_ref()
            .is_some_and(|v| !v.is_stale(&self.files[self.file_index].buffer))
        {
            return Ok(());
        }
        let columns = self.column_names();
        let buffer = &mut self.files[self.file_index].buffer;
        let format = self.projection.format;
        let keep_first = matches!(format, DetectedFormat::Csv | DetectedFormat::Tsv);
        let sort = self.sort.clone().filter(|s| {
            format.is_structured() && self.projection.columns.iter().any(|c| c.name == s.column)
        });
        let view = match sort {
            Some(spec) => {
                let proj = &self.projection;
                let mut spans = Vec::new();
                ViewIndex::sorted(
                    buffer,
                    self.filter.clone(),
                    format,
                    columns,
                    keep_first,
                    spec.descending,
                    |raw| {
                        SortKey::from_value(
                            proj.field_value(&spec.column, raw, &mut spans).as_deref(),
                        )
                    },
                )?
            }
            None => ViewIndex::filtered(
                self.filter.clone().expect("view_wanted: a filter"),
                format,
                columns,
                keep_first,
            ),
        };
        self.view = Some(view);
        Ok(())
    }

    /// Column names of a positional (CSV/TSV) file, for selector queries;
    /// empty for formats whose fields carry their own keys.
    fn column_names(&self) -> Vec<String> {
        match self.projection.format {
            DetectedFormat::Csv | DetectedFormat::Tsv => self
                .projection
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn view_and_buffer(&mut self) -> (&mut ViewIndex, &mut Buffer) {
        (
            self.view.as_mut().expect("caller checked the view"),
            &mut self.files[self.file_index].buffer,
        )
    }

    /// Position of the window top in the view, moving `top` onto a visible
    /// line when it sits on a hidden one (the next visible line; the last
    /// page if none follows; the first row of a sorted view). `None` when
    /// the view is empty.
    fn normalize_view_top(&mut self) -> Result<Option<usize>, String> {
        let rows = self.content_rows();
        let top = self.top;
        let (view, buffer) = self.view_and_buffer();
        let pos = match view.position_of(buffer, top)? {
            Some(p) => p,
            None if view.is_sorted() => 0,
            None => view.total(buffer)?.saturating_sub(rows),
        };
        let line = view.line_at(buffer, pos)?;
        Ok(line.map(|l| {
            self.top = l;
            pos
        }))
    }

    /// Moves the top to view position `pos`, clamped so a window never
    /// starts past the last full page (like `clamp_relative`).
    fn view_move_to(&mut self, proposed: usize) -> Result<(), String> {
        let rows = self.content_rows();
        let (view, buffer) = self.view_and_buffer();
        view.lines_from(buffer, proposed, rows)?;
        let (known, complete) = view.known(buffer);
        let target = if complete {
            proposed.min(known.saturating_sub(rows))
        } else {
            proposed
        };
        if let Some(line) = view.line_at(buffer, target)? {
            self.top = line;
        }
        Ok(())
    }

    fn apply_view_nav(&mut self, action: Action) -> Result<(), String> {
        let Some(pos) = self.normalize_view_top()? else {
            return Ok(()); // nothing visible: nowhere to go
        };
        let rows = self.content_rows() as i64;
        let delta = match action {
            Action::ScrollLines(n) => n,
            Action::ScrollWindow(n, forward) => {
                let mag = n.map_or(rows, |v| v as i64);
                if forward { mag } else { -mag }
            }
            Action::ScrollHalfWindow(n, forward) => {
                let mag = self.half_window_magnitude(n);
                if forward { mag } else { -mag }
            }
            Action::GotoLine(None) => return self.view_move_to(0),
            Action::GotoLine(Some(n)) | Action::GotoLast(Some(n)) => return self.view_goto(n),
            Action::GotoLast(None) => {
                let (view, buffer) = self.view_and_buffer();
                let total = view.total(buffer)?;
                return self.view_move_to(total.saturating_sub(rows as usize));
            }
            _ => unreachable!("only navigation reaches here"),
        };
        self.view_move_to((pos as i64 + delta).max(0) as usize)
    }

    /// `N g` in a view: row N of a sorted view; in a filtered view, buffer
    /// line N (or the next line the filter shows), since `-N` numbers the
    /// rows by buffer line.
    fn view_goto(&mut self, n: u64) -> Result<(), String> {
        let not_found = || format!("Cannot seek to line number {}", n);
        let (view, buffer) = self.view_and_buffer();
        let line = if view.is_sorted() {
            view.line_at(buffer, n.saturating_sub(1) as usize)?
        } else {
            let t = n as usize;
            buffer.ensure_lines_read(t)?;
            if buffer.line(t).is_none() {
                return Err(not_found());
            }
            match view.position_of(buffer, t)? {
                Some(p) => view.line_at(buffer, p)?,
                None => return Err(format!("No line at or after line {} matches the filter", n)),
            }
        };
        self.top = line.ok_or_else(not_found)?;
        Ok(())
    }

    /// Search over the view's positions (hidden lines never match).
    fn view_search(
        &mut self,
        is_match: &dyn Fn(&[u8]) -> bool,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<Option<usize>, String> {
        let Some(top_pos) = self.normalize_view_top()? else {
            return Ok(None);
        };
        let rows = self.content_rows();
        let (view, buffer) = self.view_and_buffer();
        let mut remaining = count.max(1);
        let mut found = None;
        match dir {
            Direction::Forward => {
                let mut p = if fresh { top_pos } else { top_pos + 1 };
                while let Some(line) = view.line_at(buffer, p)? {
                    if is_match(buffer.line(line).unwrap_or_default()) {
                        found = Some(line);
                        remaining -= 1;
                        if remaining == 0 {
                            break;
                        }
                    }
                    p += 1;
                }
            }
            Direction::Backward => {
                let total = view.total(buffer)?;
                let start = if fresh {
                    Some((top_pos + rows - 1).min(total.saturating_sub(1)))
                } else {
                    top_pos.checked_sub(1)
                };
                if let Some(start) = start {
                    for p in (0..=start).rev() {
                        let line = view.line_at(buffer, p)?.expect("p < total");
                        if is_match(buffer.line(line).unwrap_or_default()) {
                            found = Some(line);
                            remaining -= 1;
                            if remaining == 0 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok(found)
    }

    fn set_filter(&mut self, buf: &str) -> Result<(), String> {
        if buf.trim().is_empty() {
            self.filter = None;
            self.view = None;
            self.pending_message = Some("Filter cleared".to_string());
            return Ok(());
        }
        let filter = LineFilter::new(buf, self.case.folds(buf));
        let format = self.projection.format;
        // Reject before applying: a pattern that cannot compile matches
        // nothing and would blank the view silently.
        filter.check(format)?;
        let kind = if filter.is_structured_on(format) {
            "selector"
        } else if buf.starts_with('!') {
            "hiding regex matches"
        } else {
            "regex"
        };
        self.filter = Some(filter);
        self.view = None;
        self.first_render_done = true;
        self.ensure_view()?;
        let shown = escape_controls(buf, false);
        self.pending_message = Some(match self.normalize_view_top()? {
            Some(_) => format!("Filter: &{shown} ({kind})"),
            None => format!("Filter: &{shown} ({kind}): no matching lines"),
        });
        Ok(())
    }

    /// Column mode `s`: ascending, then descending, then off.
    fn cycle_sort(&mut self) -> Result<(), String> {
        let Some(col) = self.projection.columns.get(self.focused_column) else {
            return Err(format!(
                "Sort: needs a structured file ({PICK_FORMAT_HINT})"
            ));
        };
        let name = col.name.clone();
        let shown = escape_controls(&name, false).into_owned();
        self.sort = match &self.sort {
            Some(s) if s.column == name && !s.descending => Some(SortSpec {
                column: name,
                descending: true,
            }),
            Some(s) if s.column == name => None,
            _ => Some(SortSpec {
                column: name,
                descending: false,
            }),
        };
        self.view = None;
        self.first_render_done = true;
        self.projection.sorted_by = self.sort.as_ref().map(|s| (s.column.clone(), s.descending));
        if self.sort.is_some()
            && let Some(col) = self.projection.columns.get_mut(self.focused_column)
        {
            // Room for the header's sort arrow next to the full name.
            let name_width =
                unicode_width::UnicodeWidthStr::width(escape_controls(&col.name, false).as_ref());
            col.width = col.width.max(name_width + 1);
        }
        self.ensure_view()?;
        match self.sort.as_ref().map(|s| s.descending) {
            None => {
                self.pending_message = Some("Sort off".to_string());
                if self.view.is_some() {
                    self.normalize_view_top()?;
                }
            }
            Some(desc) => {
                let (view, buffer) = self.view_and_buffer();
                view.total(buffer)?;
                let total = view.known_rows(buffer).0;
                if let Some(first) = view.line_at(buffer, 0)? {
                    self.top = first;
                }
                let dir = if desc { "descending" } else { "ascending" };
                self.pending_message = Some(format!("Sorted by '{shown}' {dir} ({total} lines)"));
            }
        }
        Ok(())
    }

    /// Column mode `i`: a page with the focused column's value counts and,
    /// for numbers and durations, a numeric summary. Covers the lines the
    /// view shows (all lines without a filter).
    fn show_column_stats(&mut self) -> Result<(), String> {
        let Some(col) = self.projection.columns.get(self.focused_column) else {
            return Err(format!(
                "Statistics: need a structured file ({PICK_FORMAT_HINT})"
            ));
        };
        let name = col.name.clone();
        self.ensure_view()?;
        let format = self.projection.format;
        let skip_header = matches!(format, DetectedFormat::Csv | DetectedFormat::Tsv);
        let lines: Vec<usize> = match self.view.as_mut() {
            Some(view) => view
                .all_lines(&mut self.files[self.file_index].buffer)?
                .to_vec(),
            None => {
                let total = self.files[self.file_index].buffer.total_lines()?;
                (1..=total).collect()
            }
        };
        let buffer = &self.files[self.file_index].buffer;
        let mut spans = Vec::new();
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut numbers: Vec<f64> = Vec::new();
        let (mut rows, mut missing) = (0usize, 0usize);
        for &n in &lines {
            if skip_header && n == 1 {
                continue;
            }
            let Some(raw) = buffer.line(n) else { continue };
            rows += 1;
            match self.projection.field_value(&name, raw, &mut spans) {
                None => missing += 1,
                Some(v) => {
                    if let SortKey::Number(x) = SortKey::from_value(Some(&v)) {
                        numbers.push(x);
                    }
                    *counts.entry(v).or_default() += 1;
                }
            }
        }
        let shown_name = escape_controls(&name, false).into_owned();
        let mut out = Vec::new();
        let scope = match &self.filter {
            Some(f) => format!("lines matching &{}", escape_controls(&f.source, false)),
            None => "all lines".to_string(),
        };
        out.push(format!(
            "Column '{shown_name}' ({}), {scope}",
            format.name()
        ));
        out.push(String::new());
        let present = rows - missing;
        out.push(format!(
            "  rows {rows}   with a value {present}   missing {missing}   distinct {}",
            counts.len()
        ));
        if !numbers.is_empty() && numbers.len() * 2 >= present {
            numbers.sort_by(f64::total_cmp);
            let pct = |q: f64| numbers[((numbers.len() - 1) as f64 * q).round() as usize];
            let mean = numbers.iter().sum::<f64>() / numbers.len() as f64;
            out.push(format!(
                "  numeric ({} values): min {}  p50 {}  p90 {}  p99 {}  max {}  mean {}",
                numbers.len(),
                fmt_num(numbers[0]),
                fmt_num(pct(0.5)),
                fmt_num(pct(0.9)),
                fmt_num(pct(0.99)),
                fmt_num(numbers[numbers.len() - 1]),
                fmt_num(mean)
            ));
        }
        out.push(String::new());
        let mut top: Vec<(String, usize)> = counts.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let listed = top.len().min(STATS_TOP_VALUES);
        out.push(format!(
            "  most frequent values ({listed} of {}):",
            top.len()
        ));
        let max = top.first().map_or(1, |t| t.1).max(1);
        for (value, n) in top.into_iter().take(STATS_TOP_VALUES) {
            let share = 100.0 * n as f64 / rows.max(1) as f64;
            let bar = "#".repeat((20 * n).div_ceil(max));
            out.push(format!(
                "  {n:>8} {share:>5.1}%  {bar:<20}  {}",
                escape_controls(&value, false)
            ));
        }
        self.overlay = Some(Overlay {
            title: format!("Statistics: '{shown_name}'"),
            lines: out,
            top: 0,
        });
        Ok(())
    }

    fn show_help(&mut self) {
        let mut lines = vec![
            "rless keys (also listed by rless --help)".to_string(),
            String::new(),
        ];
        lines.extend(KEYS_HELP.lines().map(str::to_string));
        self.overlay = Some(Overlay {
            title: "Help".to_string(),
            lines,
            top: 0,
        });
    }

    // ------------------------------------------------------------ follow

    fn start_follow(&mut self) -> Result<(), String> {
        self.current().buffer.set_follow(true);
        self.following = true;
        self.first_render_done = true;
        if let Err(e) = self.follow_to_end() {
            self.stop_follow();
            return Err(e);
        }
        Ok(())
    }

    fn stop_follow(&mut self) {
        if self.following {
            self.following = false;
            self.current().buffer.set_follow(false);
        }
    }

    /// Reads what has arrived and shows the last page of it.
    fn follow_to_end(&mut self) -> Result<(), String> {
        let growth = self.current().buffer.poll_growth()?;
        if growth == Growth::Reset {
            self.view = None;
            self.top = 1;
            self.projection.expanded_lines.clear();
        }
        self.apply_inner(Action::GotoLast(None))
    }

    /// One follow-mode poll; true when the screen needs a redraw.
    fn follow_tick(&mut self) -> bool {
        let growth = match self.current().buffer.poll_growth() {
            Ok(g) => g,
            Err(e) => {
                self.stop_follow();
                self.pending_message = Some(format!("{e}  (press RETURN)"));
                return true;
            }
        };
        if growth == Growth::None {
            return false;
        }
        if growth == Growth::Reset {
            self.view = None;
            self.top = 1;
            self.projection.expanded_lines.clear();
            self.pending_message =
                Some("File was truncated or replaced: reading it again".to_string());
        }
        if let Err(e) = self.apply_inner(Action::GotoLast(None)) {
            self.pending_message = Some(format!("{e}  (press RETURN)"));
        }
        true
    }

    /// A pipe still delivering (not in follow mode): reads what has arrived
    /// for the lines the screen shows; true when the screen needs a redraw.
    fn stream_tick(&mut self) -> bool {
        let want = if self.view.is_some() {
            self.files[self.file_index].buffer.lines_cached() + 4096
        } else {
            self.top + self.content_rows()
        };
        let buffer = &mut self.files[self.file_index].buffer;
        let was_open = buffer.is_open_stream();
        match buffer.read_ready(want) {
            Ok(grew) => grew || (was_open && !buffer.is_open_stream()),
            Err(e) => {
                self.pending_message = Some(format!("{e}  (press RETURN)"));
                true
            }
        }
    }

    // ------------------------------------------------- marks, options, misc

    fn goto_mark(&mut self, c: char) -> Result<(), String> {
        let target = if c == '\'' {
            self.previous_position
                .ok_or_else(|| "No previous position".to_string())?
        } else {
            *self
                .marks
                .get(&c)
                .ok_or_else(|| format!("Mark '{c}' is not set"))?
        };
        if target.file != self.file_index {
            self.switch_file(target.file as i64 - self.file_index as i64)?;
        }
        self.current().buffer.ensure_lines_read(target.line)?;
        self.top = target.line.max(1);
        Ok(())
    }

    fn toggle_option(&mut self, c: char) -> Result<(), String> {
        let on_off = |b: bool| if b { "on" } else { "off" };
        let msg = match c {
            'N' => {
                self.opts.line_numbers = !self.opts.line_numbers;
                format!("Line numbers: {}", on_off(self.opts.line_numbers))
            }
            'S' => {
                self.opts.chop_long_lines = !self.opts.chop_long_lines;
                if !self.opts.chop_long_lines {
                    self.hscroll = 0;
                    self.projection.hscroll = 0;
                }
                format!("Chop long lines: {}", on_off(self.opts.chop_long_lines))
            }
            'i' | 'I' => {
                let wanted = if c == 'i' {
                    CaseMode::Smart
                } else {
                    CaseMode::Always
                };
                self.case = if self.case == wanted {
                    CaseMode::Sensitive
                } else {
                    wanted
                };
                if let Some(f) = &self.filter {
                    let source = f.source.clone();
                    self.filter = Some(LineFilter::new(&source, self.case.folds(&source)));
                    self.view = None;
                }
                match self.case {
                    CaseMode::Sensitive => "Case: searches and filters match case".to_string(),
                    CaseMode::Smart => {
                        "Case: ignored unless the pattern has an uppercase letter".to_string()
                    }
                    CaseMode::Always => "Case: always ignored".to_string(),
                }
            }
            _ => {
                return Err(format!(
                    "No such option: -{} (in the pager: -N -S -i -I)",
                    escape_controls(&c.to_string(), false)
                ));
            }
        };
        self.pending_message = Some(msg);
        Ok(())
    }

    fn table_shown(&self) -> bool {
        self.use_projection() && self.tabular_view
    }

    fn horizontal_scroll(&mut self, n: Option<u64>, right: bool) {
        if !self.opts.chop_long_lines && !self.table_shown() {
            self.pending_message = Some("Nothing to scroll: lines wrap (try -S)".to_string());
            return;
        }
        let step = n.map_or((self.width as usize / 2).max(1), |v| v as usize);
        self.hscroll = if right {
            self.hscroll.saturating_add(step)
        } else {
            self.hscroll.saturating_sub(step)
        };
        self.projection.hscroll = self.hscroll;
    }

    /// In the table view, scrolls sideways just enough to show the focused
    /// column.
    fn scroll_focus_into_view(&mut self) {
        if !self.table_shown() {
            return;
        }
        let Some(focused) = self.projection.columns.get(self.focused_column) else {
            return;
        };
        if focused.hidden {
            return;
        }
        if focused.pinned {
            return; // pinned cells never scroll
        }
        // Only the cells after the pinned ones scroll, in the room they
        // leave.
        let mut start = 0;
        for col in self.projection.display_columns().filter(|c| !c.pinned) {
            if std::ptr::eq(col, focused) {
                break;
            }
            start += col.width + 1;
        }
        let end = start + focused.width + 1;
        let width = (self.width as usize)
            .saturating_sub(self.projection.pinned_width())
            .max(1);
        if start < self.hscroll {
            self.hscroll = start;
        } else if end > self.hscroll + width {
            self.hscroll = end.saturating_sub(width).min(start);
        }
        self.projection.hscroll = self.hscroll;
    }

    /// `N%`: N percent into the view (rows of a filtered or sorted view,
    /// else lines), never past the last full page.
    fn goto_percent(&mut self, percent: u64) -> Result<(), String> {
        let percent = percent.min(100) as usize;
        self.ensure_view()?;
        if self.view.is_some() {
            let rows = self.content_rows();
            let (view, buffer) = self.view_and_buffer();
            let total = view.total(buffer)?;
            let pos = (total * percent / 100).min(total.saturating_sub(rows));
            return self.view_move_to(pos);
        }
        let total = self.current().buffer.total_lines()?;
        let rows = self.content_rows();
        let last_page = total.saturating_sub(rows) + 1;
        self.top = (total * percent / 100).clamp(1, last_page.max(1));
        Ok(())
    }

    /// `=`: where the view is and what is applied to it.
    fn file_info(&mut self) -> Result<String, String> {
        let (bottom, at_eof) = self.last_bottom;
        let top = self.top;
        let tag = self.view_tag();
        let mut parts = vec![
            self.files[self.file_index]
                .display()
                .unwrap_or_else(|| "Standard input".to_string()),
        ];
        if self.files.len() > 1 {
            parts.push(format!(
                "(file {} of {})",
                self.file_index + 1,
                self.files.len()
            ));
        }
        let buffer = &mut self.files[self.file_index].buffer;
        if buffer.is_open_stream() {
            // Totals would wait for a pipe that has not ended.
            parts.push(format!("lines {top}-{bottom}/{}+", buffer.lines_cached()));
        } else {
            let total = buffer.total_lines()?;
            let bytes = buffer.total_bytes()?;
            let offset = buffer.byte_offset_through(bottom);
            let percent = (bottom.min(total) * 100).checked_div(total).unwrap_or(100);
            parts.push(format!("lines {top}-{bottom}/{total}"));
            parts.push(format!("byte {offset}/{bytes}"));
            parts.push(format!("{percent}%"));
        }
        if at_eof {
            parts.push("(END)".to_string());
        }
        if self.projection.format.is_structured() {
            parts.push(format!("[{}]", self.projection.format.name()));
        }
        parts.extend(tag);
        Ok(parts.join(" "))
    }

    /// `R`: reads the file again (it changed on disk), keeping the place
    /// when it still exists.
    fn reload(&mut self) -> Result<(), String> {
        if !self.current().buffer.reload() {
            return Err("Cannot reload standard input".to_string());
        }
        self.view = None;
        self.projection.expanded_lines.clear();
        // Columns may have changed: derive them again on the next draw.
        let format = self.projection.format;
        if format.is_structured() {
            let sample = self.sample_lines();
            let sticky = self.projection.sticky_header;
            self.projection.set_format(format);
            self.projection.sticky_header = sticky;
            self.projection.update_columns_from_lines(&sample);
        } else {
            self.sniffer_checked = false;
        }
        let top = self.top;
        let buffer = &mut self.current().buffer;
        buffer.ensure_lines_read(top)?;
        if top > buffer.lines_cached() {
            self.apply_inner(Action::GotoLast(None))?;
        }
        self.pending_message = Some("Reloaded".to_string());
        Ok(())
    }

    /// `:e name`: shows `name`, adding it to the file list after the
    /// current file (or switching to it when it is already listed).
    fn examine(&mut self, name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("Usage: :e file".to_string());
        }
        let path = PathBuf::from(name);
        if let Some(i) = self
            .files
            .iter()
            .position(|f| f.buffer.path().is_some_and(|p| same_file(p, &path)))
        {
            return self.switch_file(i as i64 - self.file_index as i64);
        }
        let mut entry = FileEntry::new(Source::File(path));
        entry.buffer.ensure_lines_read(1)?;
        let at = self.file_index + 1;
        self.files.insert(at, entry);
        // Marks and `''` name files by index: shift those after `at`.
        for m in self.marks.values_mut() {
            if m.file >= at {
                m.file += 1;
            }
        }
        if let Some(p) = &mut self.previous_position
            && p.file >= at
        {
            p.file += 1;
        }
        self.switch_file(1)
    }

    /// `v`: hands the terminal to the editor (`$VISUAL`, else `$EDITOR`,
    /// else `vi`) at `line`, then takes it back and re-reads the file.
    fn run_editor<W: Write>(&mut self, out: &mut W, path: &Path, line: usize) -> io::Result<()> {
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
        self.pending_message = match status {
            Ok(s) if s.success() => self.reload().err().map(|e| format!("{e}  (press RETURN)")),
            Ok(s) => Some(format!("Editor exited with {s}  (press RETURN)")),
            Err(e) => Some(format!("Cannot run {}: {e}  (press RETURN)", argv[0])),
        };
        self.draw(out)
    }

    /// How far right `ESC-}` scrolls: until the end of the widest table
    /// row, or of the longest line in the window, reaches the right edge.
    fn max_hscroll(&mut self) -> Result<usize, String> {
        let width = self.width as usize;
        if self.table_shown() {
            let pinned = self.projection.pinned_width();
            let scrolling: usize = self
                .projection
                .display_columns()
                .filter(|c| !c.pinned)
                .map(|c| c.width + 1)
                .sum();
            return Ok(scrolling.saturating_sub(width.saturating_sub(pinned)));
        }
        let number_width = if self.opts.line_numbers { 8 } else { 0 };
        let room = width.saturating_sub(number_width).max(1);
        let raw_ansi = self.opts.raw_control_chars;
        let rows = self.content_rows();
        let top = self.top;
        let buffer = &mut self.files[self.file_index].buffer;
        buffer.ensure_lines_read(top + rows)?;
        let widest = (top..top + rows)
            .filter_map(|n| buffer.line(n))
            .map(|raw| screen::line_display_width(raw, raw_ansi))
            .max()
            .unwrap_or(0);
        Ok(widest.saturating_sub(room))
    }

    /// `:w path`: writes the lines the view shows (all lines without a
    /// filter or sort), in view order, as they are in the input.
    fn save_view(&mut self, path: &str, force: bool) -> Result<(), String> {
        if path.is_empty() {
            return Err("Usage: :w file  (:w! file overwrites)".to_string());
        }
        let target = Path::new(path);
        let shown = escape_controls(path, false).into_owned();
        if let Some(current) = self.files[self.file_index].display()
            && same_file(Path::new(&current), target)
        {
            return Err(format!("{shown} is the file being viewed"));
        }
        if target.exists() && !force {
            return Err(format!("{shown} exists (:w! {shown} overwrites)"));
        }
        self.ensure_view()?;
        let lines: Vec<usize> = match self.view.as_mut() {
            Some(view) => view
                .all_lines(&mut self.files[self.file_index].buffer)?
                .to_vec(),
            None => (1..=self.current().buffer.total_lines()?).collect(),
        };
        let buffer = &self.files[self.file_index].buffer;
        let write = || -> io::Result<()> {
            let mut w = io::BufWriter::new(std::fs::File::create(target)?);
            for &n in &lines {
                w.write_all(buffer.line(n).unwrap_or_default())?;
                w.write_all(b"\n")?;
            }
            w.flush()
        };
        write().map_err(|e| format!("{shown}: {e}"))?;
        self.pending_message = Some(format!("Saved {} lines to {shown}", lines.len()));
        Ok(())
    }

    fn run_search(
        &mut self,
        pattern_src: &str,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        let fold = self.case.folds(pattern_src);
        // A selector query only means something on a structured file; on
        // plain text `/.status >= 500` is an ordinary regex, as in less.
        if self.projection.format.is_structured() {
            let q = Query::parse_with_case(pattern_src, fold);
            if let Query::Structured(_) = q {
                return self.run_search_query(&q, dir, count, fresh);
            }
        }
        let pattern = RegexBuilder::new(pattern_src)
            .case_insensitive(fold)
            .build()
            .map_err(|_| "Invalid pattern".to_string())?;
        self.run_search_regex(&pattern, dir, count, fresh)
    }

    fn repeat_search(&mut self, count: u64, reverse: bool) -> Result<(), String> {
        let Some((pattern, dir)) = self.last_search.clone() else {
            return Ok(());
        };
        let dir = if reverse { dir.reversed() } else { dir };
        match pattern {
            SearchPattern::Regex(re) => self.run_search_regex(&re, dir, count, false),
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

    fn run_search_query(
        &mut self,
        query: &Query,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        let format = self.projection.format;
        let columns = self.column_names();
        self.ensure_view()?;
        let found = if self.view.is_some() {
            self.view_search(
                &|line| query.matches_fields(line, format, &columns),
                dir,
                count,
                fresh,
            )?
        } else {
            let start = self.search_start(dir, fresh);
            let buffer = &mut self.files[self.file_index].buffer;
            search::find_by(
                buffer,
                |line| query.matches_fields(line, format, &columns),
                dir,
                start,
                fresh,
                count,
            )?
        };
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
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        self.ensure_view()?;
        let found = if self.view.is_some() {
            self.view_search(
                &|line| search::line_matches(line, pattern),
                dir,
                count,
                fresh,
            )?
        } else {
            let start = self.search_start(dir, fresh);
            let buffer = &mut self.current().buffer;
            search::find(buffer, pattern, dir, start, fresh, count)?
        };
        // Remember only the typed search: storing the reversed direction
        // of an `N` would make a following `n` run backwards too.
        if fresh {
            self.last_search = Some((SearchPattern::Regex(pattern.clone()), dir));
        }
        match found {
            Some(line) => {
                self.top = line;
                Ok(())
            }
            None => Err("Pattern not found".to_string()),
        }
    }

    fn switch_file(&mut self, delta: i64) -> Result<(), String> {
        let target = self.file_index as i64 + delta;
        if target < 0 {
            return Err("No previous file".to_string());
        }
        if target as usize >= self.files.len() {
            return Err("No next file".to_string());
        }
        self.stop_follow();
        // Keep how this file was being viewed, for coming back to it.
        let mut fresh = ProjectionEngine::new(DetectedFormat::PlainText);
        fresh.raw_ansi = self.opts.raw_control_chars;
        let saved = SavedView {
            top: self.top,
            projection: std::mem::replace(&mut self.projection, fresh),
            tabular_view: std::mem::take(&mut self.tabular_view),
            sort: self.sort.take(),
            focused_column: std::mem::take(&mut self.focused_column),
            sniffer_checked: std::mem::take(&mut self.sniffer_checked),
            hscroll: std::mem::take(&mut self.hscroll),
        };
        self.files[self.file_index].saved = Some(saved);
        self.file_index = target as usize;
        self.top = 1;
        self.filename_shown = true;
        self.last_search = None;
        self.view = None;
        // A file seen before comes back as it was left (position, format,
        // table, sort); a new one gets its own format sniff and columns on
        // its first draw. The filter is kept (as in less) and re-read for
        // the new file's format.
        if let Some(v) = self.files[self.file_index].saved.take() {
            self.top = v.top;
            self.projection = v.projection;
            self.tabular_view = v.tabular_view;
            self.sort = v.sort;
            self.focused_column = v.focused_column;
            self.sniffer_checked = v.sniffer_checked;
            self.hscroll = v.hscroll;
        }
        // A file switch always leaves the fresh-process bottom-anchor
        // window (§3 item 1), regardless of whether `top` numerically
        // changed (a fresh file also starts at line 1).
        self.first_render_done = true;
        Ok(())
    }

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

        if self.overlay.is_some() {
            return self.paint_overlay(out, content_rows, width);
        }

        if let Err(e) = self.ensure_view() {
            // Keep the screen usable: drop the view and say why.
            self.filter = None;
            self.sort = None;
            self.view = None;
            self.pending_message = Some(format!("{e}  (press RETURN)"));
        }

        let (bottom_line, at_eof) = if self.use_projection() {
            self.paint_projection(out, content_rows, width)?
        } else {
            self.paint_text(out, content_rows)?
        };
        // A pipe that is still open has no end yet, whatever has arrived.
        let at_eof = at_eof && !self.files[self.file_index].buffer.is_open_stream();
        self.last_bottom = (bottom_line, at_eof);

        // While a multi-key command (numeric prefix, `:x`, `/pattern`) is
        // still being typed, echo it in place of the computed prompt; not
        // baseline-verified (every capture is taken post-Enter) but this is
        // the conventional, low-risk less UX and never fires in any tested
        // sequence's captured steps. Column mode shows its own status line
        // unless a message (pin/hide feedback, an error) is pending.
        let (prompt_text, highlighted) = match self.decoder.preview() {
            Some(preview) => (preview, false),
            None if self.decoder.in_column_mode() && self.pending_message.is_none() => {
                (self.column_mode_prompt(width), false)
            }
            None if self.following && self.pending_message.is_none() => {
                let tag = self.view_tag().map(|t| format!("{t} ")).unwrap_or_default();
                (
                    clip_to_width(
                        format!("{tag}Following: waiting for data... (any key stops)"),
                        width,
                    ),
                    true,
                )
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

    fn paint_overlay(
        &mut self,
        out: &mut Vec<u8>,
        content_rows: usize,
        width: usize,
    ) -> io::Result<()> {
        let overlay = self.overlay.as_ref().expect("checked by caller");
        let end = (overlay.top + content_rows).min(overlay.lines.len());
        for (row, line) in overlay.lines[overlay.top..end].iter().enumerate() {
            queue!(
                out,
                MoveTo(0, row as u16),
                Print(clip_to_width(line.clone(), width))
            )?;
        }
        let status = format!(
            "{} - lines {}-{} of {} - j/k/Space/b scroll, any other key closes",
            overlay.title,
            overlay.top + 1,
            end,
            overlay.lines.len()
        );
        queue!(out, MoveTo(0, content_rows as u16))?;
        write_standout(out, &clip_to_width(status, width))
    }

    /// The structured views (table, expanded lines, filter, sort) replace
    /// the less-compatible text view only on a structured file and only
    /// once one of them is switched on.
    fn use_projection(&self) -> bool {
        self.projection.format.is_structured()
            && (self.tabular_view
                || !self.projection.expanded_lines.is_empty()
                || self.filter.is_some()
                || self.sort_applies())
    }

    /// The first `SNIFF_SAMPLE_LINES` lines of the current file (fewer if
    /// it is shorter or unreadable; the read error itself surfaces on the
    /// draw that needs those lines).
    fn sample_lines(&mut self) -> Vec<Vec<u8>> {
        let buffer = &mut self.current().buffer;
        let _ = buffer.ensure_lines_read(SNIFF_SAMPLE_LINES);
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
            // The filter now reads as a selector: rebuild the view.
            self.view = None;
        }
    }

    /// Paints the less-compatible text view. Returns `(bottom_line, at_eof)`
    /// for the prompt.
    fn paint_text(&mut self, out: &mut Vec<u8>, content_rows: usize) -> io::Result<(usize, bool)> {
        if self.view.is_some() {
            return self.paint_text_view(out, content_rows);
        }
        let render_opts = self.render_opts();
        let top = self.top;
        let highlight = match &self.last_search {
            Some((SearchPattern::Regex(re), _)) => Some(re),
            _ => None,
        };
        let buffer = &mut self.files[self.file_index].buffer;
        let win = screen::render_window(buffer, top, content_rows, &render_opts, highlight)
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

    /// The text view of a filtered file: the core renderer (wrapping, `-N`
    /// with buffer line numbers, highlighting) over the view's lines.
    fn paint_text_view(
        &mut self,
        out: &mut Vec<u8>,
        content_rows: usize,
    ) -> io::Result<(usize, bool)> {
        let pos = self.normalize_view_top().map_err(io::Error::other)?;
        let render_opts = self.render_opts();
        let (view, buffer) = self.view_and_buffer();
        let lines = match pos {
            Some(p) => view
                .lines_from(buffer, p, content_rows + 1)
                .map_err(io::Error::other)?
                .to_vec(),
            None => Vec::new(),
        };
        let highlight = match &self.last_search {
            Some((SearchPattern::Regex(re), _)) => Some(re),
            _ => None,
        };
        let buffer = &self.files[self.file_index].buffer;
        let (rows, bottom, complete) =
            screen::render_lines(buffer, &lines, content_rows, &render_opts, highlight);
        for (i, row) in rows.iter().enumerate() {
            queue!(out, MoveTo(0, i as u16))?;
            write_spans(out, row)?;
        }
        for r in rows.len()..content_rows {
            queue!(out, MoveTo(0, r as u16), Print("~"))?;
        }
        let at_eof = complete && lines.len() <= content_rows;
        Ok((bottom.unwrap_or(self.top), at_eof))
    }

    /// Paints a structured view (rows already escaped and width-clipped by
    /// `ProjectionEngine::render_window`). Returns `(bottom_line, at_eof)`.
    fn paint_projection(
        &mut self,
        out: &mut Vec<u8>,
        content_rows: usize,
        width: usize,
    ) -> io::Result<(usize, bool)> {
        let pos = if self.view.is_some() {
            Some(self.normalize_view_top().map_err(io::Error::other)?)
        } else {
            None
        };
        let buffer = &mut self.files[self.file_index].buffer;
        let lines = match (pos, self.view.as_mut()) {
            (Some(Some(p)), Some(view)) => WindowLines::View(view, p),
            (Some(None), Some(view)) => WindowLines::View(view, usize::MAX),
            _ => WindowLines::From(self.top),
        };
        let win = self
            .projection
            .render_window(buffer, lines, content_rows, width, self.tabular_view)
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
        if self.view.is_some() {
            for r in row as usize..content_rows {
                queue!(out, MoveTo(0, r as u16), Print("~"))?;
            }
        }
        let bottom = if win.bottom_line == 0 {
            self.top
        } else {
            win.bottom_line
        };
        Ok((bottom, win.at_eof))
    }

    /// Status line of column mode: which column has focus (by name, with
    /// its state) and the keys that act on it.
    fn column_mode_prompt(&self, width: usize) -> String {
        let columns = &self.projection.columns;
        let text = match columns.get(self.focused_column) {
            None => "Columns: none (needs a structured file) - q done".to_string(),
            Some(col) => {
                let mut flags = Vec::new();
                if col.pinned {
                    flags.push("pinned");
                }
                if col.hidden {
                    flags.push("hidden");
                }
                match &self.sort {
                    Some(s) if s.column == col.name && s.descending => flags.push("sorted desc"),
                    Some(s) if s.column == col.name => flags.push("sorted asc"),
                    _ => {}
                }
                let state = if flags.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", flags.join(", "))
                };
                format!(
                    "Columns: {}/{} {}{} - h/l move  p pin  . hide  s sort  i stats  q done",
                    self.focused_column + 1,
                    columns.len(),
                    escape_controls(&col.name, false),
                    state
                )
            }
        };
        clip_to_width(text, width)
    }

    /// `[&filter, sort col asc, 42 lines, col 41+]`: the active view state,
    /// shown ahead of the prompt so a filtered or sorted screen is never
    /// mistaken for the whole file. `None` when nothing is active.
    fn view_tag(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(f) = &self.filter {
            let src = escape_controls(&f.source, false);
            let src = clip_to_width(src.into_owned(), 40);
            parts.push(format!("&{src}"));
        }
        if let Some(s) = self.sort.as_ref().filter(|_| self.sort_applies()) {
            let dir = if s.descending { "desc" } else { "asc" };
            parts.push(format!("sort {} {dir}", escape_controls(&s.column, false)));
        }
        if let Some(view) = &self.view {
            let (n, complete) = view.known_rows(&self.files[self.file_index].buffer);
            if complete {
                parts.push(format!("{n} {}", if n == 1 { "line" } else { "lines" }));
            }
        }
        if self.hscroll > 0 {
            parts.push(format!("col {}+", self.hscroll + 1));
        }
        (!parts.is_empty()).then(|| format!("[{}]", parts.join(", ")))
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
        let view_tag = self.view_tag();

        let (mut total_lines, total_bytes, byte_offset) = if needs_totals {
            let buffer = &mut self.current().buffer;
            let total_lines = buffer.total_lines().ok();
            let total_bytes = buffer.total_bytes().ok();
            let byte_offset = buffer.byte_offset_through(bottom_line);
            (total_lines, total_bytes, byte_offset)
        } else {
            (None, None, 0)
        };
        // A sorted view's rows are not in line order: count rows, not
        // buffer lines, so `lines a-b/N` and the percentage stay true.
        let (mut top_line, mut bottom_line) = (self.top, bottom_line);
        if needs_totals && self.view.as_ref().is_some_and(ViewIndex::is_sorted) {
            let (top, bottom) = (top_line, bottom_line);
            let (view, buffer) = self.view_and_buffer();
            if let (Ok(Some(t)), Ok(Some(b)), Ok(n)) = (
                view.position_of(buffer, top),
                view.position_of(buffer, bottom),
                view.total(buffer),
            ) {
                (top_line, bottom_line, total_lines) = (t + 1, b + 1, Some(n));
            }
        }

        let ctx = PromptContext {
            filename: filename.as_deref(),
            show_filename: self.filename_shown,
            file_index,
            file_count,
            top_line,
            bottom_line,
            total_lines,
            at_eof,
            next_filename: next_filename.as_deref(),
            byte_offset,
            total_bytes,
            view_tag: view_tag.as_deref(),
        };
        let (text, highlighted) =
            prompt::render(self.pending_message.as_deref(), mode, &ctx, width);
        self.filename_shown = false;
        (text, highlighted)
    }
}

/// True when `a` and `b` name the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The editor command line for `v`: `$VISUAL`, else `$EDITOR`, else `vi`
/// (split on whitespace, so `code --wait` works), then `+LINE` and the file.
fn editor_command(
    visual: Option<&str>,
    editor: Option<&str>,
    path: &Path,
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

/// A statistic without needless decimals: `200`, `12.5`, `0.333`.
fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let s = format!("{x:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
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
        Options::for_files(files)
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
        assert!(app.filter.is_some());

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
        assert_eq!(
            app.filter.as_ref().map(|f| f.source.as_str()),
            Some("ERROR")
        );
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
        // A filter works on plain text too (as in less): it says what it is.
        app.apply(Action::FilterInput("line".to_string())).unwrap();
        assert_eq!(
            app.pending_message.as_deref(),
            Some("Filter: &line (regex)")
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
        assert_eq!(rows[&23], "Sorted by 'msg' ascending (60 lines)");
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

    // --- Feature expansion (docs/fvi/FVI_REPORT.md, tasks T03-T21) -------

    fn feed(app: &mut App, keys: &str) {
        let mut out: Vec<u8> = Vec::new();
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            };
            app.handle_key(&mut out, code, KeyModifiers::NONE).unwrap();
        }
    }

    fn app_for(path: &Path, w: u16, h: u16) -> App {
        let mut app = App::new(test_opts(vec![path.to_path_buf()]));
        app.set_size(w, h);
        drawn_rows(&mut app);
        app
    }

    fn mixed_log() -> PathBuf {
        let levels = ["INFO", "DEBUG", "ERROR", "WARN"];
        let content: String = (1..=100)
            .map(|i| format!("{} request {}\n", levels[i % 4], i))
            .collect();
        temp_file(&content)
    }

    #[test]
    fn plain_text_filter_shows_only_matching_lines_with_their_numbers() {
        let path = mixed_log();
        let mut app = App::new(Options {
            line_numbers: true,
            ..test_opts(vec![path.clone()])
        });
        app.set_size(60, 6);
        feed(&mut app, "&ERROR\n");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0].trim_start(), "2 ERROR request 2");
        assert_eq!(rows[&1].trim_start(), "6 ERROR request 6");
        assert_eq!(rows[&5], "Filter: &ERROR (regex)");
        feed(&mut app, "j");
        assert_eq!(
            app.top, 6,
            "j moves to the next shown line, never a hidden one"
        );
        feed(&mut app, "G");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&4].trim_start(), "98 ERROR request 98");
        assert_eq!(rows[&5], "[&ERROR, 25 lines] (END)");
        // Past the end: clamps, the top stays (the bell rings).
        let top = app.top;
        feed(&mut app, "j");
        assert_eq!(app.top, top);
        feed(&mut app, "g");
        assert_eq!(app.top, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn negated_filter_hides_lines_and_empty_filter_clears() {
        let path = mixed_log();
        let mut app = app_for(&path, 60, 5);
        feed(&mut app, "&!INFO|DEBUG\n");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "ERROR request 2");
        assert_eq!(rows[&1], "WARN request 3");
        assert_eq!(rows[&2], "ERROR request 6");
        feed(&mut app, "&\n");
        assert!(app.filter.is_none());
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "ERROR request 2", "clearing keeps the place");
        assert_eq!(rows[&1], "WARN request 3");
        assert_eq!(rows[&2], "INFO request 4");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn search_and_window_moves_inside_a_filter_skip_hidden_lines() {
        let path = mixed_log();
        let mut app = app_for(&path, 60, 5);
        feed(&mut app, "&ERROR|WARN\n");
        // "request 1" also appears in hidden lines (1, 10, 12...).
        feed(&mut app, "/request 1\n");
        assert_eq!(app.top, 10, "line 1 is hidden, 10 is ERROR");
        feed(&mut app, "n");
        assert_eq!(app.top, 11);
        feed(&mut app, "N");
        assert_eq!(app.top, 10);
        feed(&mut app, "g f");
        // 4 content rows: f moves 4 shown lines (2,3,6,7 -> 10).
        feed(&mut app, "g");
        feed(&mut app, "f");
        assert_eq!(app.top, 10);
        feed(&mut app, "50g");
        assert_eq!(app.top, 50, "50 is ERROR: shown");
        feed(&mut app, "52g");
        assert_eq!(app.top, 54, "52 is hidden: the next shown line");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn filter_on_a_line_with_no_match_says_so() {
        let path = mixed_log();
        let mut app = app_for(&path, 60, 5);
        feed(&mut app, "&NOPE\n");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "~");
        assert_eq!(rows[&4], "Filter: &NOPE (regex): no matching lines");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn compound_and_nested_selector_filters() {
        let content: String = (1..=20)
            .map(|i| {
                format!(
                    "{{\"level\":\"{}\",\"ms\":{},\"user\":{{\"id\":{}}},\"msg\":\"{}\"}}\n",
                    if i % 2 == 0 { "ERROR" } else { "INFO" },
                    i * 50,
                    i % 3,
                    if i % 5 == 0 { "db timeout" } else { "ok" }
                )
            })
            .collect();
        let path = temp_file(&content);
        let mut app = app_for(&path, 120, 12);
        feed(&mut app, "&.level == \"ERROR\" && .user.id == 1\n");
        let view_lines = |app: &mut App| {
            let (view, buffer) = app.view_and_buffer();
            view.all_lines(buffer).unwrap().to_vec()
        };
        assert_eq!(view_lines(&mut app), [4, 10, 16]);
        feed(&mut app, "&.msg =~ \"time(out)?\" || .ms >= 900\n");
        assert_eq!(view_lines(&mut app), [5, 10, 15, 18, 19, 20]);
        feed(&mut app, "&!(.level == \"INFO\") && .ms < 0.3s\n");
        assert_eq!(view_lines(&mut app), [2, 4]);
        let _ = std::fs::remove_file(path);
    }

    fn scores_csv() -> PathBuf {
        temp_file("id,name,score\n1,ann,40\n2,bob,95\n3,cy,\n4,dee,7\n5,eve,95\n")
    }

    #[test]
    fn selector_filter_and_search_work_on_csv_columns() {
        let path = scores_csv();
        let mut app = app_for(&path, 80, 10);
        assert_eq!(app.projection.format, DetectedFormat::Csv);
        feed(&mut app, "&.score >= 90\n");
        let (view, buffer) = app.view_and_buffer();
        assert_eq!(
            view.all_lines(buffer).unwrap(),
            [1, 3, 6],
            "header row kept first"
        );
        feed(&mut app, "&\n");
        feed(&mut app, "/.name == \"dee\"\n");
        assert_eq!(app.top, 5);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn column_sort_cycles_ascending_descending_off() {
        let path = scores_csv();
        let mut app = app_for(&path, 80, 10);
        feed(&mut app, "cH");
        feed(&mut app, "xlls");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&9], "Sorted by 'score' ascending (5 lines)");
        assert!(rows[&0].contains("score↑"), "{}", rows[&0]);
        // Numeric order; the missing value last.
        let names: Vec<String> = (1..=5).map(|r| rows[&r].clone()).collect();
        assert!(names[0].starts_with("4 "), "{names:?}");
        assert!(names[1].starts_with("1 "), "{names:?}");
        assert!(
            names[4].starts_with("3 "),
            "missing value sorts last: {names:?}"
        );
        feed(&mut app, "s");
        let rows = drawn_rows(&mut app);
        assert!(
            rows[&1].starts_with("2 "),
            "descending, ties keep file order"
        );
        assert!(rows[&2].starts_with("5 "));
        assert!(rows[&5].starts_with("3 "), "missing still last");
        feed(&mut app, "s");
        assert_eq!(app.pending_message.as_deref(), Some("Sort off"));
        let rows = drawn_rows(&mut app);
        assert!(rows[&1].starts_with("1 "));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn column_statistics_page_counts_values_and_summarises_numbers() {
        let path = scores_csv();
        let mut app = app_for(&path, 100, 16);
        feed(&mut app, "xlli");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "Column 'score' (csv), all lines");
        assert!(rows[&2].contains("rows 5   with a value 4   missing 1   distinct 3"));
        assert!(
            rows[&3].contains("min 7") && rows[&3].contains("max 95"),
            "{}",
            rows[&3]
        );
        assert!(
            rows[&6].trim_start().starts_with("2  40.0%"),
            "{}",
            rows[&6]
        );
        assert!(rows[&6].ends_with("95"));
        assert!(rows[&15].starts_with("Statistics: 'score'"));
        // Any other key closes the page; the file is back.
        feed(&mut app, "q");
        assert!(app.overlay.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn marks_and_quote_quote_return_to_earlier_places() {
        let path = lines_fixture(200);
        let mut app = app_for(&path, 80, 24);
        feed(&mut app, "30gma");
        feed(&mut app, "150g");
        feed(&mut app, "'a");
        assert_eq!(app.top, 30);
        feed(&mut app, "''");
        assert_eq!(app.top, 150, "'' goes back to before the last jump");
        feed(&mut app, "''");
        assert_eq!(app.top, 30);
        feed(&mut app, "'z");
        assert_eq!(
            app.pending_message.as_deref(),
            Some("Mark 'z' is not set  (press RETURN)")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn help_page_lists_the_keys_and_scrolls() {
        let path = lines_fixture(10);
        let mut app = app_for(&path, 80, 10);
        feed(&mut app, "h");
        let rows = drawn_rows(&mut app);
        assert!(rows[&0].starts_with("rless keys"));
        assert!(rows[&9].starts_with("Help - lines 1-9 of"));
        feed(&mut app, "j");
        let rows = drawn_rows(&mut app);
        assert!(rows[&9].starts_with("Help - lines 2-10 of"));
        feed(&mut app, "x");
        assert!(app.overlay.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn options_toggle_inside_the_pager() {
        let path = mixed_log();
        let mut app = app_for(&path, 40, 5);
        feed(&mut app, "-N");
        assert_eq!(app.pending_message.as_deref(), Some("Line numbers: on"));
        feed(&mut app, "\n");
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "      1 DEBUG request 1");
        feed(&mut app, "-i");
        feed(&mut app, "/error\n");
        assert_eq!(app.top, 2, "-i folds case for a lowercase pattern");
        feed(&mut app, "/Error\n");
        assert!(
            app.pending_message
                .as_deref()
                .unwrap()
                .starts_with("Pattern not found"),
            "smart case: an uppercase letter makes it exact"
        );
        feed(&mut app, "\n-Z");
        assert!(
            app.pending_message
                .as_deref()
                .unwrap()
                .starts_with("No such option: -Z")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn horizontal_scroll_with_chop_and_in_the_table() {
        let path = lines_fixture(10);
        let mut app = App::new(Options {
            chop_long_lines: true,
            ..test_opts(vec![path.clone()])
        });
        app.set_size(20, 5);
        drawn_rows(&mut app);
        app.apply(Action::HScroll(Some(4), true)).unwrap();
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "line 1 of 10 :: pla>");
        assert_eq!(rows[&4], "[col 5+]");
        app.apply(Action::HScroll(None, false)).unwrap();
        assert_eq!(app.hscroll, 0);
        let _ = std::fs::remove_file(path);

        // Wrapped text has nothing hidden: say so instead of shifting.
        let path = lines_fixture(10);
        let mut app = app_for(&path, 20, 5);
        app.apply(Action::HScroll(None, true)).unwrap();
        assert_eq!(app.hscroll, 0);
        assert_eq!(
            app.pending_message.as_deref(),
            Some("Nothing to scroll: lines wrap (try -S)")
        );
        let _ = std::fs::remove_file(path);

        let path = jsonl_fixture();
        let mut app = app_for(&path, 20, 5);
        feed(&mut app, "c\n");
        app.apply(Action::HScroll(Some(7), true)).unwrap();
        let rows = drawn_rows(&mut app);
        assert!(rows[&0].starts_with("│200   │"), "{}", rows[&0]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_writes_the_current_view_and_protects_existing_files() {
        let path = mixed_log();
        let mut app = app_for(&path, 60, 5);
        let out = std::env::temp_dir().join(format!("rust-less-save-{}.log", rand_suffix()));
        let out_s = out.display().to_string();
        feed(&mut app, "&WARN\n");
        app.apply(Action::SaveView {
            path: out_s.clone(),
            force: false,
        })
        .unwrap();
        let saved = std::fs::read_to_string(&out).unwrap();
        assert_eq!(saved.lines().count(), 25);
        assert!(saved.starts_with("WARN request 3\nWARN request 7\n"));
        assert_eq!(
            app.pending_message,
            Some(format!("Saved 25 lines to {out_s}"))
        );
        let err = app
            .apply(Action::SaveView {
                path: out_s.clone(),
                force: false,
            })
            .unwrap_err();
        assert!(err.contains("exists"), "{err}");
        feed(&mut app, "\n&\n");
        app.apply(Action::SaveView {
            path: out_s.clone(),
            force: true,
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 100);
        let err = app
            .apply(Action::SaveView {
                path: path.display().to_string(),
                force: true,
            })
            .unwrap_err();
        assert!(err.ends_with("is the file being viewed"), "{err}");
        let _ = std::fs::remove_file(out);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn follow_shows_appended_lines_and_survives_truncation() {
        use std::io::Write as _;
        let path = lines_fixture(30);
        let mut app = app_for(&path, 80, 6);
        app.apply(Action::Follow).unwrap();
        assert!(app.following);
        assert_eq!(app.top, 26, "follow starts at the last page");
        assert!(!app.follow_tick(), "nothing new");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"new line A\nunfinished").unwrap();
        f.flush().unwrap();
        assert!(app.follow_tick());
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&4], "new line A", "the unfinished line is held back");
        assert!(rows[&5].contains("Following"), "{}", rows[&5]);
        f.write_all(b" now done\n").unwrap();
        f.flush().unwrap();
        assert!(app.follow_tick());
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&4], "unfinished now done", "joined, not split in two");
        // Truncated in place (log rotation by copytruncate).
        std::fs::write(&path, "fresh 1\nfresh 2\n").unwrap();
        assert!(app.follow_tick());
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&0], "fresh 1");
        assert!(rows[&5].starts_with("File was truncated or replaced"));
        app.stop_follow();
        assert!(!app.following);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn follow_keeps_a_filter_live() {
        use std::io::Write as _;
        let path = mixed_log();
        let mut app = app_for(&path, 60, 4);
        feed(&mut app, "&ERROR\nF");
        assert!(app.following);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"INFO quiet\nERROR loud\n").unwrap();
        f.flush().unwrap();
        assert!(app.follow_tick());
        let rows = drawn_rows(&mut app);
        assert_eq!(rows[&2], "ERROR loud");
        assert_eq!(rows[&1], "ERROR request 98");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn startup_commands_position_the_first_screen() {
        let path = lines_fixture(200);
        let run = |cmds: &[&str]| {
            let mut app = App::new(Options {
                initial_commands: cmds.iter().map(|c| c.to_string()).collect(),
                ..test_opts(vec![path.clone()])
            });
            app.set_size(80, 24);
            app.run_initial_commands();
            app
        };
        assert_eq!(run(&["G"]).top, 178);
        assert_eq!(run(&["50"]).top, 50);
        assert_eq!(run(&["/line 120 "]).top, 120);
        let filtered = run(&["&7 of"]);
        assert!(filtered.filter.is_some());
        assert_eq!(filtered.top, 7);
        let bad = run(&["9999"]);
        assert_eq!(
            bad.pending_message.as_deref(),
            Some("Cannot seek to line number 9999  (press RETURN)")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn quit_if_one_screen_prints_short_input_only() {
        let short = temp_file("a\x07b\nsecond\n");
        let mut app = App::new(test_opts(vec![short.clone()]));
        app.set_size(80, 24);
        let out = app.one_screen_output().unwrap().unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("^G"),
            "control characters stay escaped: {text:?}"
        );
        assert!(text.ends_with("second\n"));
        let long = lines_fixture(30);
        let mut app = App::new(test_opts(vec![long.clone()]));
        app.set_size(80, 24);
        assert!(app.one_screen_output().unwrap().is_none());
        let _ = std::fs::remove_file(short);
        let _ = std::fs::remove_file(long);
    }

    #[test]
    fn filter_follows_into_the_next_file_and_rereads_for_its_format() {
        let jsonl = jsonl_fixture();
        let plain = mixed_log();
        let mut app = App::new(test_opts(vec![jsonl.clone(), plain.clone()]));
        app.set_size(80, 6);
        drawn_rows(&mut app);
        feed(&mut app, "&ERROR\n");
        feed(&mut app, ":n");
        let rows = drawn_rows(&mut app);
        assert_eq!(
            rows[&0], "ERROR request 2",
            "the filter is kept, as in less"
        );
        assert!(rows[&5].contains("[&ERROR"), "and shown: {}", rows[&5]);
        feed(&mut app, ":p");
        let rows = drawn_rows(&mut app);
        assert!(rows[&0].contains("\"level\":\"ERROR\""), "{}", rows[&0]);
        let _ = std::fs::remove_file(jsonl);
        let _ = std::fs::remove_file(plain);
    }

    #[test]
    fn long_prompt_counts_rows_of_a_sorted_view() {
        let path = scores_csv();
        let mut app = App::new(Options {
            prompt_mode: PromptMode::Long,
            ..test_opts(vec![path.clone()])
        });
        app.set_size(80, 4);
        drawn_rows(&mut app);
        feed(&mut app, "cxllsq\n");
        let rows = drawn_rows(&mut app);
        // q dismissed the sort message, so Enter moved one row down.
        assert!(rows[&3].contains("lines 2-4/6 66%"), "{}", rows[&3]);
        let _ = std::fs::remove_file(path);
    }

    // --- Ported: %, =, R, v, :e, N:x, ESC-{ ESC-}, per-file views, pins --

    fn app_on(path: &Path, w: u16, h: u16) -> App {
        let mut app = App::new(test_opts(vec![path.to_path_buf()]));
        app.set_size(w, h);
        drawn_rows(&mut app);
        app
    }

    #[test]
    fn percent_goes_proportionally_and_never_past_the_last_page() {
        let path = lines_fixture(200);
        let mut app = app_on(&path, 80, 24);
        app.apply(Action::GotoPercent(Some(50))).unwrap();
        assert_eq!(app.top, 100);
        app.apply(Action::GotoPercent(Some(100))).unwrap();
        assert_eq!(app.top, 178);
        app.apply(Action::GotoPercent(None)).unwrap();
        assert_eq!(app.top, 1);
        app.apply(Action::GotoMark('\'')).unwrap();
        assert_eq!(app.top, 178, "a percent jump is a jump for ''");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn percent_counts_rows_of_a_filtered_view() {
        // Lines 10, 20, ..., 200 match: 20 rows; 50% is row 10 = line 100.
        let mut content = String::new();
        for n in 1..=200 {
            let tag = if n % 10 == 0 { " TEN" } else { "" };
            content.push_str(&format!("line {n}{tag}\n"));
        }
        let path = temp_file(&content);
        let mut app = app_on(&path, 40, 6);
        app.apply(Action::FilterInput("TEN".into())).unwrap();
        app.apply(Action::GotoPercent(Some(50))).unwrap();
        assert_eq!(app.top, 110);
        app.apply(Action::GotoPercent(Some(100))).unwrap();
        assert_eq!(app.top, 160, "last page of 5 rows: lines 160..200");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_info_reports_position_size_and_view_state() {
        let path = jsonl_fixture();
        let mut app = app_on(&path, 200, 24);
        app.apply(Action::FilterInput(".status >= 500".into()))
            .unwrap();
        drawn_rows(&mut app);
        app.apply(Action::FileInfo).unwrap();
        let info = app.pending_message.clone().unwrap();
        assert!(info.starts_with(&path.display().to_string()), "{info}");
        assert!(info.contains("/60 byte "), "{info}");
        assert!(info.contains("[jsonl] [&.status >= 500"), "{info}");
        // On a narrow screen the start (name, position) is what remains.
        let mut narrow = app_on(&path, 30, 24);
        narrow.apply(Action::FileInfo).unwrap();
        let info = narrow.pending_message.clone().unwrap();
        assert!(info.starts_with("/"), "{info}");
        assert!(info.ends_with('>'), "{info}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_picks_up_a_rewritten_file_and_stdin_cannot() {
        let path = temp_file("old 1\nold 2\n");
        let mut app = app_on(&path, 80, 24);
        std::fs::write(&path, "new 1\n").unwrap();
        app.apply(Action::Reload).unwrap();
        let rows = drawn_rows(&mut app);
        assert!(rows.values().any(|r| r == "new 1"), "{rows:?}");
        let mut stdin_app = App::new(test_opts(vec![]));
        assert_eq!(
            stdin_app.apply(Action::Reload).unwrap_err(),
            "Cannot reload standard input"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn edit_is_queued_for_a_file_and_refused_for_stdin() {
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

    #[test]
    fn editor_command_prefers_visual_then_editor_then_vi() {
        let p = Path::new("/tmp/f.log");
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
    fn examine_opens_a_file_after_the_current_one_and_goto_file_jumps() {
        let a = lines_fixture(10);
        let b = temp_file("other file\n");
        let mut app = app_on(&a, 80, 24);
        app.apply(Action::SetMark('a')).unwrap();
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
        app.apply(Action::GotoFile(Some(2))).unwrap();
        assert_eq!(app.file_index, 1);
        app.apply(Action::GotoFile(None)).unwrap();
        assert_eq!(app.file_index, 0);
        assert_eq!(
            app.apply(Action::GotoFile(Some(9))).unwrap_err(),
            "No such file"
        );
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn a_file_switched_back_to_keeps_its_place_and_table() {
        let a = jsonl_fixture();
        let b = lines_fixture(100);
        let mut app = App::new(test_opts(vec![a.clone(), b.clone()]));
        app.set_size(80, 24);
        drawn_rows(&mut app);
        app.apply(Action::ToggleTabular).unwrap();
        app.apply(Action::GotoLine(Some(30))).unwrap();
        app.apply(Action::NextFile(1)).unwrap();
        assert_eq!(app.top, 1);
        assert!(!app.tabular_view, "the next file starts in its own view");
        app.apply(Action::GotoLine(Some(40))).unwrap();
        app.apply(Action::PrevFile(1)).unwrap();
        assert_eq!(app.top, 30);
        assert!(app.tabular_view);
        assert_eq!(app.projection.format, DetectedFormat::JsonLines);
        app.apply(Action::NextFile(1)).unwrap();
        assert_eq!(app.top, 40);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn esc_braces_scroll_to_the_end_of_the_longest_line_and_back() {
        let line = format!("start{}end", "-".repeat(100));
        let path = temp_file(&format!("{line}\nshort\n"));
        let mut opts = test_opts(vec![path.clone()]);
        opts.chop_long_lines = true;
        let mut app = App::new(opts);
        app.set_size(40, 6);
        app.apply(Action::HScrollEnd).unwrap();
        assert_eq!(app.hscroll, 108 - 40);
        let rows = drawn_rows(&mut app);
        assert!(rows.values().any(|r| r.ends_with("end")), "{rows:?}");
        app.apply(Action::HScrollHome).unwrap();
        assert_eq!(app.hscroll, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pinned_columns_stay_put_while_the_table_scrolls() {
        let path = jsonl_fixture();
        let mut app = app_on(&path, 30, 8);
        app.apply(Action::ToggleTabular).unwrap();
        // Columns: level, status, msg. Pin the first.
        app.apply(Action::PinColumn).unwrap();
        app.apply(Action::HScroll(Some(4), true)).unwrap();
        let rows = drawn_rows(&mut app);
        assert!(
            rows[&0].starts_with("DEBUG  │"),
            "pinned cell kept: {:?}",
            rows[&0]
        );
        assert!(!rows[&0].contains("200"), "status scrolled: {:?}", rows[&0]);
        let _ = std::fs::remove_file(path);
    }
}
