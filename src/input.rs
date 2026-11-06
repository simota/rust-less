//! Decodes crossterm key events into a logical `Action`, accumulating a
//! numeric prefix (digits typed before a command char) and the small
//! multi-key sequences (`:n`, `:p`, `:q`, `:Q`, `ZZ`, `/pattern<Enter>`,
//! `?pattern<Enter>`, `ESC-v`/`ESC-<`/`ESC->`) less itself supports.
//!
//! Arrow/PageUp/PageDown map directly onto the same `Action`s as
//! j/k/b/f (§2 `nav-arrow-pgup-pgdn-bindings`) — a declared divergence from
//! terminfo-driven binding (crossterm already decodes named keys), see
//! docs/RECONSTRUCTION_SPEC.md §4.
//!
//! Window/half-window/goto commands carry `Option<u64>` rather than a
//! resolved magnitude: their default (window height, sticky half-window,
//! first/last line) depends on terminal size and `command`'s sticky state,
//! neither of which this module knows about — resolution happens in
//! `command`. Single-line scroll (`j`/`k`) and search-repeat/next-file
//! counts default to a size-independent `1`, so they resolve their default
//! here.

use crate::search::Direction;
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    ScrollLines(i64),
    /// (explicit count, forward?)
    ScrollWindow(Option<u64>, bool),
    /// (explicit count, forward?)
    ScrollHalfWindow(Option<u64>, bool),
    GotoLine(Option<u64>),
    GotoLast(Option<u64>),
    SearchInput(String, Direction, u64),
    FilterInput(String),
    RepeatSearch(u64),
    RepeatSearchReverse(u64),
    NextFile(u64),
    PrevFile(u64),
    ToggleTabular,
    ToggleExpand,
    ToggleStickyHeader,
    ToggleFormat,
    ToggleColumnMode,
    PrevColumn,
    NextColumn,
    PinColumn,
    HideColumn,
    SortColumn,
    /// (explicit column count, rightward?)
    ScrollHorizontal(Option<u64>, bool),
    /// `ESC-{`: back to the first column.
    HorizontalHome,
    /// `ESC-}`: far enough right to show the end of the longest line.
    HorizontalEnd,
    /// `N%` / `Np`: go to N percent into the file.
    GotoPercent(Option<u64>),
    /// `m<letter>`
    SetMark(char),
    /// `'<letter>`; `''` returns to the position before the last jump.
    GotoMark(char),
    /// `-<letter>`: toggle an option while paging.
    ToggleOption(char),
    /// `=` / `^G` / `:f`
    FileInfo,
    /// `R`: discard what was read and read the file again.
    Reload,
    /// `F`: go to the end and keep reading as the input grows.
    Follow,
    /// `h`: the key reference.
    Help,
    /// `v`: open the file in the editor at the top line.
    Edit,
    /// `:e name`: open another file.
    Examine(String),
    /// `N:x`: go to file N (the first file without N).
    GotoFile(Option<u64>),
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// A complete command ready to execute.
    Action(Action),
    /// Input state changed but no command is ready yet (mid-prefix, typing
    /// a search pattern, first half of `ZZ`, first half of `:x`, ESC seen).
    Redraw,
    /// Key had no effect (unmapped key with no pending state).
    Nothing,
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Normal,
    EscPending,
    ZPending,
    Colon,
    Pattern {
        dir: Direction,
        buf: String,
        count: u64,
    },
    FilterPattern {
        buf: String,
    },
    /// Column mode (`x`). The focused column itself lives in `command`,
    /// which knows how many columns exist.
    Column,
    /// After `m`: the next key names the mark to set.
    Mark,
    /// After `'`: the next key names the mark to go to.
    GotoMark,
    /// After `-`: the next key names the option to toggle.
    OptionPrefix,
    /// `:e` file name being typed.
    Examine {
        buf: String,
    },
}

pub struct Decoder {
    prefix: String,
    mode: Mode,
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder {
            prefix: String::new(),
            mode: Mode::Normal,
        }
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Live text to show on the prompt row while a multi-key command is
    /// still being typed, or `None` if there is nothing to preview.
    pub fn preview(&self) -> Option<String> {
        match &self.mode {
            Mode::Pattern { dir, buf, .. } => {
                let lead = match dir {
                    Direction::Forward => '/',
                    Direction::Backward => '?',
                };
                Some(format!("{}{}", lead, buf))
            }
            Mode::FilterPattern { buf } => Some(format!("&{}", buf)),
            Mode::Examine { buf } => Some(format!("Examine: {}", buf)),
            Mode::Colon => Some(format!("{}:", self.prefix)),
            Mode::Mark => Some("mark: ".to_string()),
            Mode::GotoMark => Some("goto mark: ".to_string()),
            Mode::OptionPrefix => Some("-".to_string()),
            Mode::Normal if !self.prefix.is_empty() => Some(self.prefix.clone()),
            _ => None,
        }
    }

    /// True while column mode (`x`) is active; `command` draws its status.
    pub fn in_column_mode(&self) -> bool {
        self.mode == Mode::Column
    }

    /// True while a text prompt (`/`, `?`, `&`, `:e`) is being typed.
    pub fn in_text_entry(&self) -> bool {
        matches!(
            self.mode,
            Mode::Pattern { .. } | Mode::FilterPattern { .. } | Mode::Examine { .. }
        )
    }

    /// Abandons any half-typed command (prefix, prompt, pending key).
    pub fn reset(&mut self) {
        self.prefix.clear();
        self.mode = Mode::Normal;
    }

    /// Resolves the prefix to a concrete count, default `1` when absent or
    /// explicitly `0` (used by commands whose default doesn't depend on
    /// terminal size: line-scroll, search-repeat, next/prev-file).
    fn take_count(&mut self) -> u64 {
        let n = self.prefix.parse::<u64>().unwrap_or(0);
        self.prefix.clear();
        if n == 0 { 1 } else { n }
    }

    /// Like `take_count`, but preserves the "no explicit prefix" signal as
    /// `None` for commands whose default is resolved by the caller
    /// (window height, sticky half-window, first/last line). `0` is folded
    /// into `None` too — §2 `nav-numeric-prefix`: `0g` clamps to line 1
    /// exactly like a bare `g`, it is not an explicit "line 0" target.
    fn take_optional_count(&mut self) -> Option<u64> {
        if self.prefix.is_empty() {
            return None;
        }
        let n = self.prefix.parse::<u64>().unwrap_or(0);
        self.prefix.clear();
        if n == 0 { None } else { Some(n) }
    }

    pub fn feed(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Decoded {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);

        match &mut self.mode {
            Mode::Pattern { buf, .. } | Mode::FilterPattern { buf } | Mode::Examine { buf } => {
                return match edit_line(buf, code) {
                    LineEdit::Editing => Decoded::Redraw,
                    LineEdit::Cancel => {
                        self.mode = Mode::Normal;
                        Decoded::Redraw
                    }
                    LineEdit::Submit => match std::mem::replace(&mut self.mode, Mode::Normal) {
                        Mode::Pattern { dir, buf, count } => {
                            Decoded::Action(Action::SearchInput(buf, dir, count))
                        }
                        Mode::FilterPattern { buf } => Decoded::Action(Action::FilterInput(buf)),
                        Mode::Examine { buf } => Decoded::Action(Action::Examine(buf)),
                        _ => unreachable!("matched a text-entry mode above"),
                    },
                };
            }
            Mode::Column => match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('x') => {
                    self.mode = Mode::Normal;
                    return Decoded::Redraw;
                }
                KeyCode::Char('h') | KeyCode::Left => return Decoded::Action(Action::PrevColumn),
                KeyCode::Char('l') | KeyCode::Right => return Decoded::Action(Action::NextColumn),
                KeyCode::Char('p') => return Decoded::Action(Action::PinColumn),
                KeyCode::Char('.') => return Decoded::Action(Action::HideColumn),
                KeyCode::Char('s') => return Decoded::Action(Action::SortColumn),
                _ => return Decoded::Redraw,
            },
            Mode::Mark | Mode::GotoMark | Mode::OptionPrefix => {
                let mode = std::mem::replace(&mut self.mode, Mode::Normal);
                self.prefix.clear();
                let KeyCode::Char(c) = code else {
                    return Decoded::Redraw; // Esc (or any non-character) cancels
                };
                return Decoded::Action(match mode {
                    Mode::Mark => Action::SetMark(c),
                    Mode::GotoMark => Action::GotoMark(c),
                    _ => Action::ToggleOption(c),
                });
            }
            Mode::Colon => {
                self.mode = Mode::Normal;
                if code == KeyCode::Char('x') {
                    return Decoded::Action(Action::GotoFile(self.take_optional_count()));
                }
                let count = self.take_count();
                return match code {
                    KeyCode::Char('n') => Decoded::Action(Action::NextFile(count)),
                    KeyCode::Char('p') => Decoded::Action(Action::PrevFile(count)),
                    KeyCode::Char('f') => Decoded::Action(Action::FileInfo),
                    KeyCode::Char('e') => {
                        self.mode = Mode::Examine { buf: String::new() };
                        Decoded::Redraw
                    }
                    KeyCode::Char('q') | KeyCode::Char('Q') => Decoded::Action(Action::Quit),
                    _ => Decoded::Redraw,
                };
            }
            Mode::EscPending => {
                self.mode = Mode::Normal;
                return match code {
                    KeyCode::Char('v') => {
                        Decoded::Action(Action::ScrollWindow(self.take_optional_count(), false))
                    }
                    KeyCode::Char('<') => {
                        Decoded::Action(Action::GotoLine(self.take_optional_count()))
                    }
                    KeyCode::Char('>') => {
                        Decoded::Action(Action::GotoLast(self.take_optional_count()))
                    }
                    KeyCode::Char(')') => {
                        Decoded::Action(Action::ScrollHorizontal(self.take_optional_count(), true))
                    }
                    KeyCode::Char('(') => {
                        Decoded::Action(Action::ScrollHorizontal(self.take_optional_count(), false))
                    }
                    KeyCode::Char('{') => Decoded::Action(Action::HorizontalHome),
                    KeyCode::Char('}') => Decoded::Action(Action::HorizontalEnd),
                    _ => Decoded::Redraw,
                };
            }
            Mode::ZPending => {
                self.mode = Mode::Normal;
                return match code {
                    KeyCode::Char('Z') => Decoded::Action(Action::Quit),
                    _ => Decoded::Nothing,
                };
            }
            Mode::Normal => {}
        }

        if let KeyCode::Char(c) = code
            && c.is_ascii_digit()
            && !ctrl
        {
            self.prefix.push(c);
            return Decoded::Redraw;
        }

        if ctrl && let KeyCode::Char(c) = code {
            let action = match c {
                'n' | 'e' | 'j' => Some(Action::ScrollLines(self.take_count() as i64)),
                'y' | 'p' | 'k' => Some(Action::ScrollLines(-(self.take_count() as i64))),
                'v' | 'f' => Some(Action::ScrollWindow(self.take_optional_count(), true)),
                'b' => Some(Action::ScrollWindow(self.take_optional_count(), false)),
                'd' => Some(Action::ScrollHalfWindow(self.take_optional_count(), true)),
                'u' => Some(Action::ScrollHalfWindow(self.take_optional_count(), false)),
                'g' => Some(Action::FileInfo),
                'l' | 'r' => {
                    self.prefix.clear();
                    return Decoded::Redraw; // repaint
                }
                _ => None,
            };
            return match action {
                Some(a) => Decoded::Action(a),
                None => {
                    self.prefix.clear();
                    Decoded::Nothing
                }
            };
        }

        let result = match code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => {
                Some(Action::ScrollLines(self.take_count() as i64))
            }
            KeyCode::Char('k') | KeyCode::Up => {
                Some(Action::ScrollLines(-(self.take_count() as i64)))
            }
            KeyCode::Char('e') => Some(Action::ScrollLines(self.take_count() as i64)),
            KeyCode::Char('y') => Some(Action::ScrollLines(-(self.take_count() as i64))),
            KeyCode::Char(' ') | KeyCode::PageDown | KeyCode::Char('f') => {
                Some(Action::ScrollWindow(self.take_optional_count(), true))
            }
            KeyCode::Char('b') | KeyCode::PageUp => {
                Some(Action::ScrollWindow(self.take_optional_count(), false))
            }
            KeyCode::Char('d') => Some(Action::ScrollHalfWindow(self.take_optional_count(), true)),
            KeyCode::Char('u') => Some(Action::ScrollHalfWindow(self.take_optional_count(), false)),
            KeyCode::Char('g') | KeyCode::Char('<') => {
                Some(Action::GotoLine(self.take_optional_count()))
            }
            KeyCode::Char('G') | KeyCode::Char('>') => {
                Some(Action::GotoLast(self.take_optional_count()))
            }
            KeyCode::Char('n') => Some(Action::RepeatSearch(self.take_count())),
            KeyCode::Char('N') => Some(Action::RepeatSearchReverse(self.take_count())),
            KeyCode::Char('c') => Some(Action::ToggleTabular),
            KeyCode::Char('s') => Some(Action::ToggleFormat),
            KeyCode::Char('z') => Some(Action::ToggleExpand),
            KeyCode::Char('H') => Some(Action::ToggleStickyHeader),
            KeyCode::Char('x') => {
                self.mode = Mode::Column;
                Some(Action::ToggleColumnMode)
            }
            KeyCode::Right => Some(Action::ScrollHorizontal(self.take_optional_count(), true)),
            KeyCode::Left => Some(Action::ScrollHorizontal(self.take_optional_count(), false)),
            KeyCode::Char('p') | KeyCode::Char('%') => {
                Some(Action::GotoPercent(self.take_optional_count()))
            }
            KeyCode::Char('=') => Some(Action::FileInfo),
            KeyCode::Char('R') => Some(Action::Reload),
            KeyCode::Char('F') => Some(Action::Follow),
            KeyCode::Char('h') => Some(Action::Help),
            KeyCode::Char('v') => Some(Action::Edit),
            KeyCode::Char('r') => {
                self.prefix.clear();
                return Decoded::Redraw; // repaint
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
            _ => None,
        };
        if let Some(a) = result {
            return Decoded::Action(a);
        }

        match code {
            KeyCode::Char('&') => {
                self.mode = Mode::FilterPattern { buf: String::new() };
                Decoded::Redraw
            }
            KeyCode::Char('/') => {
                let count = self.take_count();
                self.mode = Mode::Pattern {
                    dir: Direction::Forward,
                    buf: String::new(),
                    count,
                };
                Decoded::Redraw
            }
            KeyCode::Char('?') => {
                let count = self.take_count();
                self.mode = Mode::Pattern {
                    dir: Direction::Backward,
                    buf: String::new(),
                    count,
                };
                Decoded::Redraw
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Colon;
                Decoded::Redraw
            }
            KeyCode::Char('m') => {
                self.mode = Mode::Mark;
                Decoded::Redraw
            }
            KeyCode::Char('\'') => {
                self.mode = Mode::GotoMark;
                Decoded::Redraw
            }
            KeyCode::Char('-') => {
                self.mode = Mode::OptionPrefix;
                Decoded::Redraw
            }
            KeyCode::Char('Z') => {
                self.mode = Mode::ZPending;
                Decoded::Redraw
            }
            KeyCode::Esc => {
                self.mode = Mode::EscPending;
                Decoded::Redraw
            }
            _ => {
                if !self.prefix.is_empty() {
                    self.prefix.clear();
                    Decoded::Redraw
                } else {
                    Decoded::Nothing
                }
            }
        }
    }
}

enum LineEdit {
    Editing,
    Submit,
    Cancel,
}

/// Shared line editing for the `/`, `?` and `&` prompts: Enter submits,
/// Esc cancels, Backspace deletes, characters append, other keys are
/// ignored.
fn edit_line(buf: &mut String, code: KeyCode) -> LineEdit {
    match code {
        KeyCode::Enter => return LineEdit::Submit,
        KeyCode::Esc => return LineEdit::Cancel,
        KeyCode::Backspace => {
            buf.pop();
        }
        KeyCode::Char(c) => buf.push(c),
        _ => {}
    }
    LineEdit::Editing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn bare_j_defaults_to_one() {
        let mut d = Decoder::new();
        let (c, m) = key('j');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::ScrollLines(1)));
    }

    #[test]
    fn multi_digit_prefix_applies_to_next_command() {
        let mut d = Decoder::new();
        for c in "50".chars() {
            let (c, m) = key(c);
            assert_eq!(d.feed(c, m), Decoded::Redraw);
        }
        let (c, m) = key('j');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::ScrollLines(50)));
    }

    #[test]
    fn prefix_is_consumed_and_does_not_leak_to_next_command() {
        let mut d = Decoder::new();
        for c in "5".chars() {
            let (c, m) = key(c);
            d.feed(c, m);
        }
        let (c, m) = key('d');
        assert_eq!(
            d.feed(c, m),
            Decoded::Action(Action::ScrollHalfWindow(Some(5), true))
        );
        let (c, m) = key('u');
        assert_eq!(
            d.feed(c, m),
            Decoded::Action(Action::ScrollHalfWindow(None, false))
        );
    }

    #[test]
    fn zero_prefix_on_goto_is_treated_as_default_not_explicit_zero() {
        let mut d = Decoder::new();
        let (c, m) = key('0');
        d.feed(c, m);
        let (c, m) = key('g');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::GotoLine(None)));
    }

    #[test]
    fn huge_numeric_prefix_parses_without_overflow_panic() {
        let mut d = Decoder::new();
        for c in "999999999999".chars() {
            let (c, m) = key(c);
            d.feed(c, m);
        }
        let (c, m) = key('g');
        assert_eq!(
            d.feed(c, m),
            Decoded::Action(Action::GotoLine(Some(999999999999)))
        );
    }

    #[test]
    fn colon_n_with_prefix_produces_count() {
        let mut d = Decoder::new();
        let (c, m) = key('2');
        d.feed(c, m);
        let (c, m) = key(':');
        assert_eq!(d.feed(c, m), Decoded::Redraw);
        let (c, m) = key('n');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::NextFile(2)));
    }

    #[test]
    fn zz_quits_only_on_second_z() {
        let mut d = Decoder::new();
        let (c, m) = key('Z');
        assert_eq!(d.feed(c, m), Decoded::Redraw);
        let (c, m) = key('Z');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::Quit));
    }

    #[test]
    fn single_z_then_other_key_does_not_quit() {
        let mut d = Decoder::new();
        let (c, m) = key('Z');
        d.feed(c, m);
        let (c, m) = key('j');
        assert_eq!(d.feed(c, m), Decoded::Nothing);
    }

    #[test]
    fn search_pattern_accumulates_until_enter() {
        let mut d = Decoder::new();
        let (c, m) = key('/');
        assert_eq!(d.feed(c, m), Decoded::Redraw);
        for ch in "SOLO_NEEDLE".chars() {
            let (c, m) = key(ch);
            assert_eq!(d.feed(c, m), Decoded::Redraw);
        }
        assert_eq!(d.preview(), Some("/SOLO_NEEDLE".to_string()));
        let (c, m) = (KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            d.feed(c, m),
            Decoded::Action(Action::SearchInput(
                "SOLO_NEEDLE".to_string(),
                Direction::Forward,
                1
            ))
        );
    }

    #[test]
    fn arrow_and_pgup_pgdn_alias_j_k_b_f() {
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(KeyCode::Down, KeyModifiers::NONE),
            Decoded::Action(Action::ScrollLines(1))
        );
        assert_eq!(
            d.feed(KeyCode::Up, KeyModifiers::NONE),
            Decoded::Action(Action::ScrollLines(-1))
        );
        assert_eq!(
            d.feed(KeyCode::PageDown, KeyModifiers::NONE),
            Decoded::Action(Action::ScrollWindow(None, true))
        );
        assert_eq!(
            d.feed(KeyCode::PageUp, KeyModifiers::NONE),
            Decoded::Action(Action::ScrollWindow(None, false))
        );
    }

    #[test]
    fn ctrl_n_aliases_forward_line() {
        let mut d = Decoder::new();
        let (c, m) = ctrl('n');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::ScrollLines(1)));
    }

    #[test]
    fn window_scroll_with_explicit_count_does_not_touch_sticky_half_window() {
        let mut d = Decoder::new();
        for c in "10".chars() {
            let (c, m) = key(c);
            d.feed(c, m);
        }
        let (c, m) = key('f');
        assert_eq!(
            d.feed(c, m),
            Decoded::Action(Action::ScrollWindow(Some(10), true))
        );
    }

    #[test]
    fn column_mode_emits_column_actions_until_q() {
        let mut d = Decoder::new();
        let (c, m) = key('x');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::ToggleColumnMode));
        assert!(d.in_column_mode());
        assert_eq!(d.preview(), None, "command draws the column status");
        assert_eq!(
            d.feed(KeyCode::Right, KeyModifiers::NONE),
            Decoded::Action(Action::NextColumn)
        );
        let (c, m) = key('h');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::PrevColumn));
        let (c, m) = key('q');
        assert_eq!(d.feed(c, m), Decoded::Redraw);
        assert!(!d.in_column_mode());
        let (c, m) = key('q');
        assert_eq!(d.feed(c, m), Decoded::Action(Action::Quit));
    }

    #[test]
    fn filter_prompt_edits_like_the_search_prompt() {
        let mut d = Decoder::new();
        for ch in "&ab".chars() {
            let (c, m) = key(ch);
            d.feed(c, m);
        }
        d.feed(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(d.preview(), Some("&a".to_string()));
        assert_eq!(
            d.feed(KeyCode::Enter, KeyModifiers::NONE),
            Decoded::Action(Action::FilterInput("a".to_string()))
        );
        let (c, m) = key('/');
        d.feed(c, m);
        assert_eq!(d.feed(KeyCode::Esc, KeyModifiers::NONE), Decoded::Redraw);
        assert_eq!(d.preview(), None, "Esc cancels the prompt");
    }

    fn feed_all(d: &mut Decoder, keys: &str) -> Decoded {
        let mut last = Decoded::Nothing;
        for ch in keys.chars() {
            let (c, m) = key(ch);
            last = d.feed(c, m);
        }
        last
    }

    #[test]
    fn marks_options_and_percent_take_their_argument_key() {
        let mut d = Decoder::new();
        assert_eq!(
            feed_all(&mut d, "ma"),
            Decoded::Action(Action::SetMark('a'))
        );
        assert_eq!(
            feed_all(&mut d, "'a"),
            Decoded::Action(Action::GotoMark('a'))
        );
        assert_eq!(
            feed_all(&mut d, "''"),
            Decoded::Action(Action::GotoMark('\''))
        );
        assert_eq!(
            feed_all(&mut d, "-N"),
            Decoded::Action(Action::ToggleOption('N'))
        );
        assert_eq!(
            feed_all(&mut d, "50%"),
            Decoded::Action(Action::GotoPercent(Some(50)))
        );
        assert_eq!(
            feed_all(&mut d, "p"),
            Decoded::Action(Action::GotoPercent(None))
        );
        let (c, m) = key('m');
        d.feed(c, m);
        assert_eq!(d.preview(), Some("mark: ".to_string()));
        assert_eq!(d.feed(KeyCode::Esc, KeyModifiers::NONE), Decoded::Redraw);
        assert_eq!(d.preview(), None, "Esc cancels a pending mark");
    }

    #[test]
    fn horizontal_scroll_keys() {
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(KeyCode::Right, KeyModifiers::NONE),
            Decoded::Action(Action::ScrollHorizontal(None, true))
        );
        assert_eq!(feed_all(&mut d, "8"), Decoded::Redraw);
        assert_eq!(
            d.feed(KeyCode::Left, KeyModifiers::NONE),
            Decoded::Action(Action::ScrollHorizontal(Some(8), false))
        );
        d.feed(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            feed_all(&mut d, "}"),
            Decoded::Action(Action::HorizontalEnd)
        );
        d.feed(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            feed_all(&mut d, "{"),
            Decoded::Action(Action::HorizontalHome)
        );
    }

    #[test]
    fn colon_commands_examine_first_file_and_info() {
        let mut d = Decoder::new();
        assert_eq!(
            feed_all(&mut d, ":x"),
            Decoded::Action(Action::GotoFile(None))
        );
        assert_eq!(
            feed_all(&mut d, "3:x"),
            Decoded::Action(Action::GotoFile(Some(3)))
        );
        assert_eq!(feed_all(&mut d, ":f"), Decoded::Action(Action::FileInfo));
        feed_all(&mut d, ":eother.log");
        assert!(d.in_text_entry());
        assert_eq!(d.preview(), Some("Examine: other.log".to_string()));
        assert_eq!(
            d.feed(KeyCode::Enter, KeyModifiers::NONE),
            Decoded::Action(Action::Examine("other.log".to_string()))
        );
        assert_eq!(feed_all(&mut d, "="), Decoded::Action(Action::FileInfo));
        assert_eq!(
            d.feed(KeyCode::Char('g'), KeyModifiers::CONTROL),
            Decoded::Action(Action::FileInfo)
        );
    }
}
