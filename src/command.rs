//! Main loop / state machine. Owns current top-of-window line, sticky
//! half-window N, current file index and the multi-file list, the
//! "first draw of this process" flag, and dispatches decoded `Action`s to
//! `buffer`/`screen`/`search`/`prompt`.

use std::io::{self, Write};
use std::path::PathBuf;

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use regex::Regex;

use crate::buffer::{Buffer, Source};
use crate::cli::{BellMode, Options, PromptMode};
use crate::input::{Action, Decoded, Decoder};
use crate::projection::ProjectionEngine;
use crate::prompt::{self, PromptContext};
use crate::screen::{self, RenderOpts};
use crate::search::{self, Direction};
use crate::selector::Query;
use crate::sniffer::{DetectedFormat, FormatSniffer};

struct FileEntry {
    buffer: Buffer,
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
    last_search: Option<(Regex, Direction)>,
    width: u16,
    height: u16,
    decoder: Decoder,
    pub projection: ProjectionEngine,
    pub tabular_view: bool,
    pub active_filter: Option<Query>,
    pub focused_column: usize,
    pub sniffer_checked: bool,
    /// Reused frame buffer for `draw` (kept to avoid regrowing it per frame).
    frame_buf: Vec<u8>,
}

impl App {
    pub fn new(opts: Options) -> Self {
        let files: Vec<FileEntry> = if opts.files.is_empty() {
            vec![FileEntry {
                buffer: Buffer::new(Source::Stdin),
            }]
        } else {
            opts.files
                .iter()
                .map(|p: &PathBuf| FileEntry {
                    buffer: Buffer::new(Source::File(p.clone())),
                })
                .collect()
        };
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
            projection: ProjectionEngine::new(DetectedFormat::PlainText),
            tabular_view: false,
            active_filter: None,
            focused_column: 0,
            sniffer_checked: false,
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
        }
    }

    fn current(&mut self) -> &mut FileEntry {
        &mut self.files[self.file_index]
    }

    /// Eagerly validates the very first file before any terminal state is
    /// touched, matching the observed less behavior of exiting near-
    /// instantly on a missing file (§2 `exit-codes`).
    pub fn validate_first_file(&mut self) -> Result<(), String> {
        self.files[0].buffer.ensure_lines_read(1)?;
        Ok(())
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

        self.draw(out)?;
        loop {
            match event::read()? {
                Event::Resize(w, h) => {
                    self.set_size(w, h);
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
        let result = self.apply_inner(action);
        if result.is_ok() && (self.top != old_top || self.file_index != old_file_index) {
            self.first_render_done = true;
        }
        result
    }

    fn apply_inner(&mut self, action: Action) -> Result<(), String> {
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
                let mag = match n {
                    Some(v) => {
                        self.sticky_half = Some(v);
                        v
                    }
                    None => self.half_window(),
                } as i64;
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
            Action::RepeatSearch(count) => {
                let Some((pattern, dir)) = self.last_search.clone() else {
                    return Ok(());
                };
                self.run_search_regex(&pattern, dir, count, false)?;
            }
            Action::RepeatSearchReverse(count) => {
                let Some((pattern, dir)) = self.last_search.clone() else {
                    return Ok(());
                };
                self.run_search_regex(&pattern, dir.reversed(), count, false)?;
            }
            Action::ToggleTabular => {
                self.tabular_view = !self.tabular_view;
                let mode_str = if self.tabular_view {
                    "tabular"
                } else {
                    "raw stream"
                };
                self.pending_message = Some(format!("View mode: {}", mode_str));
            }
            Action::ToggleExpand => {
                self.projection.toggle_expand(self.top);
            }
            Action::ToggleStickyHeader => {
                self.projection.sticky_header = !self.projection.sticky_header;
            }
            Action::ToggleFormat => {
                let next = self.projection.format.next_cycle();
                self.projection.set_format(next);
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
                    self.pending_message =
                        Some(format!("Column '{}' pinned: {}", col.name, col.pinned));
                }
            }
            Action::HideColumn => {
                if let Some(col) = self.projection.columns.get_mut(self.focused_column) {
                    col.hidden = !col.hidden;
                    self.pending_message =
                        Some(format!("Column '{}' hidden: {}", col.name, col.hidden));
                }
            }
            Action::SortColumn => {
                if let Some(col) = self.projection.columns.get(self.focused_column) {
                    let col_name = col.name.clone();
                    self.pending_message = Some(format!("[SORT: {} (snapshot)]", col_name));
                }
            }
            Action::FilterInput(buf) => {
                if buf.trim().is_empty() {
                    self.active_filter = None;
                    self.pending_message = Some("Filter cleared".to_string());
                } else {
                    let q = Query::parse(&buf);
                    self.active_filter = Some(q);
                    self.pending_message = Some(format!("Filter: &{}", buf));
                }
            }
            Action::NextFile(n) => self.switch_file(n as i64)?,
            Action::PrevFile(n) => self.switch_file(-(n as i64))?,
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

    fn run_search(
        &mut self,
        pattern_src: &str,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        let q = Query::parse(pattern_src);
        if let Query::Structured(_) = q {
            return self.run_search_query(&q, dir, count, fresh);
        }
        let pattern = Regex::new(pattern_src).map_err(|_| "Invalid pattern".to_string())?;
        self.run_search_regex(&pattern, dir, count, fresh)
    }

    fn run_search_query(
        &mut self,
        query: &Query,
        dir: Direction,
        count: u64,
        fresh: bool,
    ) -> Result<(), String> {
        let start = if fresh {
            match dir {
                Direction::Forward => self.top,
                Direction::Backward => self.top + self.content_rows().saturating_sub(1),
            }
        } else {
            self.top
        };
        let format = self.projection.format;
        let buffer = &mut self.files[self.file_index].buffer;
        let mut found = 0u64;
        let mut last_match = None;

        match dir {
            Direction::Forward => {
                let mut line = if fresh { start } else { start + 1 };
                loop {
                    buffer.ensure_lines_read(line)?;
                    let Some(raw) = buffer.line(line) else { break };
                    if query.matches_line(raw, format) {
                        found += 1;
                        last_match = Some(line);
                        if found >= count {
                            break;
                        }
                    }
                    line += 1;
                }
            }
            Direction::Backward => {
                buffer.ensure_full_read()?;
                let total = buffer.lines_cached();
                let upper = start.min(total);
                for line in (1..=upper).rev() {
                    let Some(raw) = buffer.line(line) else {
                        continue;
                    };
                    if query.matches_line(raw, format) {
                        found += 1;
                        last_match = Some(line);
                        if found >= count {
                            break;
                        }
                    }
                }
            }
        }

        match last_match {
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
        let start = if fresh {
            match dir {
                Direction::Forward => self.top,
                Direction::Backward => self.top + self.content_rows().saturating_sub(1),
            }
        } else {
            self.top
        };
        let buffer = &mut self.current().buffer;
        let found = search::find(buffer, pattern, dir, start, fresh, count)?;
        self.last_search = Some((pattern.clone(), dir));
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
        self.file_index = target as usize;
        self.top = 1;
        self.filename_shown = true;
        self.last_search = None;
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
        let render_opts = self.render_opts();
        let last_search = self.last_search.clone();
        let top = self.top;

        if !self.sniffer_checked {
            let buffer = &mut self.current().buffer;
            let _ = buffer.ensure_lines_read(16);
            let sample_lines: Vec<_> = (1..=16)
                .filter_map(|i| buffer.line(i).map(|s| s.to_vec()))
                .collect();
            let sniffer = FormatSniffer::default();
            let detected = sniffer.sniff(&sample_lines);
            if detected.is_structured() {
                self.projection.set_format(detected);
                self.projection.update_columns_from_lines(&sample_lines);
            }
            self.sniffer_checked = true;
        }

        let is_structured = self.projection.format.is_structured();
        let use_projection = is_structured
            && (self.tabular_view
                || !self.projection.expanded_lines.is_empty()
                || self.active_filter.is_some());

        // `Clear(All)` already blanks the whole pane, so the fresh-launch
        // bottom-anchor case (§3 item 1) needs no explicit fill: leaving
        // `blank_above` rows untouched before the content starts is enough.
        queue!(out, Clear(ClearType::All))?;

        let win = if use_projection {
            let mut raw_lines = Vec::new();
            let mut cur_line = top;
            let format = self.projection.format;
            let filter = self.active_filter.as_ref();
            let tabular = self.tabular_view;
            let mut at_eof = false;

            {
                let buf = &mut self.files[self.file_index].buffer;
                while raw_lines.len() < content_rows {
                    if buf.ensure_lines_read(cur_line).is_err() {
                        break;
                    }
                    let Some(raw) = buf.line(cur_line) else {
                        at_eof = buf.is_eof();
                        break;
                    };

                    let matches_filter = match filter {
                        Some(q) => q.matches_line(raw, format),
                        None => true,
                    };

                    if matches_filter {
                        raw_lines.push((cur_line, raw.to_vec()));
                    }
                    cur_line += 1;
                }
            }

            let mut projected_rows = Vec::new();
            let mut last_buffer_line = top;
            for (line_idx, raw) in &raw_lines {
                last_buffer_line = *line_idx;
                let sub_rows = self.projection.project_line(raw, *line_idx, tabular);
                for sub in sub_rows {
                    projected_rows.push(sub);
                    if projected_rows.len() >= content_rows {
                        break;
                    }
                }
            }

            let mut row_idx = 0;
            if self.projection.sticky_header && !self.projection.columns.is_empty() {
                let mut header = String::new();
                for col in self.projection.columns.iter().filter(|c| !c.hidden) {
                    let padded = format!("{:width$}", col.name, width = col.width);
                    header.push_str(&padded);
                    header.push('│');
                }
                queue!(out, MoveTo(0, 0))?;
                write_standout(out, &header)?;
                row_idx = 1;
            }

            for row_text in projected_rows {
                if row_idx >= content_rows {
                    break;
                }
                queue!(out, MoveTo(0, row_idx as u16), Print(&row_text))?;
                row_idx += 1;
            }

            screen::WindowRender {
                rows: Vec::new(),
                bottom_line: last_buffer_line,
                at_eof,
            }
        } else {
            let buffer = &mut self.current().buffer;
            let win = screen::render_window(
                buffer,
                top,
                content_rows,
                &render_opts,
                last_search.as_ref().map(|(p, _)| p),
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
            win
        };

        // While a multi-key command (numeric prefix, `:x`, `/pattern`) is
        // still being typed, echo it in place of the computed prompt; not
        // baseline-verified (every capture is taken post-Enter) but this is
        // the conventional, low-risk less UX and never fires in any tested
        // sequence's captured steps.
        let (prompt_text, highlighted) = match self.decoder.preview() {
            Some(preview) => (preview, false),
            None => self.build_prompt(&win, width),
        };
        queue!(out, MoveTo(0, content_rows as u16))?;
        if highlighted {
            write_standout(out, &prompt_text)?;
        } else {
            queue!(out, Print(&prompt_text))?;
        }

        Ok(())
    }

    fn build_prompt(&mut self, win: &screen::WindowRender, width: usize) -> (String, bool) {
        let mode = match self.opts.prompt_mode {
            PromptMode::Short => prompt::Mode::Short,
            PromptMode::Medium => prompt::Mode::Medium,
            PromptMode::Long => prompt::Mode::Long,
        };
        let needs_totals = matches!(mode, prompt::Mode::Medium | prompt::Mode::Long);
        let file_count = self.files.len();
        let file_index = self.file_index + 1;
        let next_filename = if win.at_eof && self.file_index + 1 < self.files.len() {
            self.files[self.file_index + 1].display()
        } else {
            None
        };
        let filename = self.current().display();

        let (total_lines, total_bytes, byte_offset) = if needs_totals {
            let buffer = &mut self.current().buffer;
            let total_lines = buffer.total_lines().ok();
            let total_bytes = buffer.total_bytes().ok();
            let byte_offset = buffer.byte_offset_through(win.bottom_line);
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
            bottom_line: win.bottom_line,
            total_lines,
            at_eof: win.at_eof,
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
        Options {
            line_numbers: false,
            chop_long_lines: false,
            raw_control_chars: false,
            prompt_mode: PromptMode::Short,
            bell: BellMode::Normal,
            files,
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
}
