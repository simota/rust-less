//! Pure argv → `Options` mapping (clap). No behavior logic.

use clap::Parser;
use std::path::PathBuf;

/// Interactive keys, shown after the options in `--help` and on the
/// in-pager help page (`h`).
const KEYS_HELP: &str = "\
Keys (N = optional number typed first):
  j k Enter Down Up      forward / backward one line (N j: N lines)
  f b Space PgDn PgUp    forward / backward one window
  d u                    half window (N d sets the default)
  g G                    first / last line (N g: line N)
  N% Np                  N percent into the file
  Right Left             scroll sideways half a screen (N: N columns)
  ESC-{ ESC-}            first column / end of the longest line
  /pat ?pat n N          search forward / backward, repeat, repeat reversed
  &pat &!pat &           show only matching / non-matching lines; & clears
  m<a-z>  '<a-z>         set a mark / go to it ('' back, '^ start, '$ end)
  -N -S -i -I            toggle line numbers, chopping, case-insensitive search
  =  ^G  :f              file information (position, size, filter, sort)
  F                      follow: show new input as it arrives (any key stops)
  R                      reload the file;   r ^L  repaint
  v                      edit the file at this line ($VISUAL, $EDITOR or vi)
  :n :p  N:x  :e file    next / previous / Nth file, open another file
  h                      this help;   q Q :q ZZ  quit

Structured input (JSON lines, CSV, TSV, logfmt; detected automatically):
  c                      table view on / off
  s                      cycle the format (plain, jsonl, csv, tsv, logfmt)
  z                      expand the top JSON line
  H                      sticky header row on / off
  x                      column mode: h/l move, p pin (stays in place when
                         scrolling sideways), . hide, s sort asc/desc/off, q done
  /.field OP value       selector search, also in &: OP is == != > >= < <= =~
                         e.g. /.status >= 500   &.level == \"ERROR\"   &.ms > 250ms
  A && B   A || B        combine selectors (&& binds tighter); .a.b reaches into
                         nested JSON; CSV/TSV fields are named by the header row";

/// The text of the in-pager help page.
pub fn help_page() -> String {
    format!(
        "rless {} - help\n\n{}\n\n\
Startup options worth knowing: -N -S -R -i -I -F -X -m -M, -p PATTERN,\n\
--filter EXPR, and +cmd (e.g. +G, +/error, +150). See rless --help.\n",
        env!("CARGO_PKG_VERSION"),
        KEYS_HELP
    )
}

#[derive(Parser, Debug)]
#[command(
    name = "rless",
    about = "A clean-room Rust reimplementation of the core-pager subset of GNU less 668",
    after_help = KEYS_HELP
)]
struct Cli {
    /// Display a line number at the start of every line.
    #[arg(short = 'N', long = "LINE-NUMBERS")]
    line_numbers_on: bool,

    /// Suppress line numbers (default).
    #[arg(short = 'n', long = "line-numbers")]
    line_numbers_off: bool,

    /// Truncate long lines instead of wrapping.
    #[arg(short = 'S', long = "chop-long-lines")]
    chop_long_lines: bool,

    /// Pass ANSI color / OSC 8 sequences through raw.
    #[arg(short = 'R', long = "RAW-CONTROL-CHARS")]
    raw_control_chars: bool,

    /// Medium (percent) prompt.
    #[arg(short = 'm')]
    medium_prompt: bool,

    /// Long (byte/line-range/percent) prompt.
    #[arg(short = 'M')]
    long_prompt: bool,

    /// Silence the bell for EOF/BOF-scroll errors only.
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// Silence the bell entirely.
    #[arg(short = 'Q', long = "QUIET")]
    very_quiet: bool,

    /// Searches and filters ignore case unless the pattern has an uppercase letter.
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case_smart: bool,

    /// Searches and filters always ignore case.
    #[arg(short = 'I', long = "IGNORE-CASE")]
    ignore_case_always: bool,

    /// Print the input and exit if it fits on one screen (no pager needed).
    #[arg(short = 'F', long = "quit-if-one-screen")]
    quit_if_one_screen: bool,

    /// Do not use the alternate screen: the last page stays in the terminal after quitting.
    #[arg(short = 'X', long = "no-init")]
    no_init: bool,

    /// Start at the first line matching PATTERN (same as +/PATTERN).
    #[arg(short = 'p', long = "pattern", value_name = "PATTERN")]
    pattern: Option<String>,

    /// Show only the lines matching EXPR (same as typing &EXPR at startup).
    #[arg(long = "filter", value_name = "EXPR")]
    filter: Option<String>,

    /// Input files. When empty, input is read from stdin.
    files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Short,
    Medium,
    Long,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BellMode {
    Normal,
    Quiet,
    VeryQuiet,
}

/// How `/`, `?` and `&` patterns treat letter case (`-i`, `-I`, or the
/// `-i`/`-I` toggles typed inside the pager).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    #[default]
    Sensitive,
    /// `-i`: ignore case unless the pattern contains an uppercase letter.
    Smart,
    /// `-I`: always ignore case.
    Always,
}

impl CaseMode {
    /// Whether `pattern` should be matched case-insensitively.
    pub fn ignores_case(self, pattern: &str) -> bool {
        match self {
            CaseMode::Sensitive => false,
            CaseMode::Smart => !pattern.chars().any(char::is_uppercase),
            CaseMode::Always => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub line_numbers: bool,
    pub chop_long_lines: bool,
    pub raw_control_chars: bool,
    pub prompt_mode: PromptMode,
    pub bell: BellMode,
    pub case: CaseMode,
    pub quit_if_one_screen: bool,
    pub no_init: bool,
    /// Commands run, in order, once the first file is shown (`+cmd`,
    /// `-p`, `--filter`), as the keys a user would type. A command left
    /// waiting for input (`/err`) is submitted as if Enter followed.
    pub initial_commands: Vec<String>,
    pub files: Vec<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            line_numbers: false,
            chop_long_lines: false,
            raw_control_chars: false,
            prompt_mode: PromptMode::Short,
            bell: BellMode::Normal,
            case: CaseMode::Sensitive,
            quit_if_one_screen: false,
            no_init: false,
            initial_commands: Vec::new(),
            files: Vec::new(),
        }
    }
}

/// Splits `+cmd` arguments (less's initial commands, e.g. `+G`, `+/error`,
/// `+150`) out of argv before clap sees them: clap would otherwise take
/// them for file names. A lone `+` and everything after `--` are left
/// alone. A purely numeric command means "go to that line" (`+150` is
/// `150g`), the common `vi +N` habit.
fn split_plus_commands(args: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut rest = Vec::with_capacity(args.len());
    let mut commands = Vec::new();
    let mut after_dashdash = false;
    for (i, arg) in args.into_iter().enumerate() {
        if i > 0 && !after_dashdash && arg.len() > 1 && arg.starts_with('+') {
            let cmd = &arg[1..];
            if cmd.chars().all(|c| c.is_ascii_digit()) {
                commands.push(format!("{cmd}g"));
            } else {
                commands.push(cmd.to_string());
            }
            continue;
        }
        if arg == "--" {
            after_dashdash = true;
        }
        rest.push(arg);
    }
    (rest, commands)
}

pub fn parse() -> Options {
    parse_from(std::env::args().collect())
}

fn parse_from(args: Vec<String>) -> Options {
    let (args, mut initial_commands) = split_plus_commands(args);
    let cli = Cli::parse_from(args);
    if let Some(p) = &cli.pattern {
        initial_commands.push(format!("/{p}"));
    }
    if let Some(f) = &cli.filter {
        initial_commands.push(format!("&{f}"));
    }
    let case = if cli.ignore_case_always {
        CaseMode::Always
    } else if cli.ignore_case_smart {
        CaseMode::Smart
    } else {
        CaseMode::Sensitive
    };
    let prompt_mode = if cli.long_prompt {
        PromptMode::Long
    } else if cli.medium_prompt {
        PromptMode::Medium
    } else {
        PromptMode::Short
    };
    let bell = if cli.very_quiet {
        BellMode::VeryQuiet
    } else if cli.quiet {
        BellMode::Quiet
    } else {
        BellMode::Normal
    };
    Options {
        line_numbers: cli.line_numbers_on && !cli.line_numbers_off,
        chop_long_lines: cli.chop_long_lines,
        raw_control_chars: cli.raw_control_chars,
        prompt_mode,
        bell,
        case,
        quit_if_one_screen: cli.quit_if_one_screen,
        no_init: cli.no_init,
        initial_commands,
        files: cli.files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plus_commands_are_split_from_file_names() {
        let o = parse_from(args(&["rless", "+G", "a.txt", "+/err", "+150", "b.txt"]));
        assert_eq!(o.initial_commands, vec!["G", "/err", "150g"]);
        assert_eq!(
            o.files,
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]
        );
    }

    #[test]
    fn plus_after_double_dash_is_a_file_name() {
        let o = parse_from(args(&["rless", "--", "+notcmd"]));
        assert!(o.initial_commands.is_empty());
        assert_eq!(o.files, vec![PathBuf::from("+notcmd")]);
    }

    #[test]
    fn pattern_and_filter_become_initial_commands_after_plus_commands() {
        let o = parse_from(args(&[
            "rless", "-p", "ERROR", "--filter", ".ms > 5", "+G", "f",
        ]));
        assert_eq!(o.initial_commands, vec!["G", "/ERROR", "&.ms > 5"]);
    }

    #[test]
    fn case_flags_pick_the_strongest_mode() {
        assert_eq!(parse_from(args(&["rless", "-i"])).case, CaseMode::Smart);
        assert_eq!(
            parse_from(args(&["rless", "-i", "-I"])).case,
            CaseMode::Always
        );
        assert_eq!(parse_from(args(&["rless"])).case, CaseMode::Sensitive);
    }

    #[test]
    fn smart_case_respects_uppercase_in_the_pattern() {
        assert!(CaseMode::Smart.ignores_case("error"));
        assert!(!CaseMode::Smart.ignores_case("Error"));
        assert!(CaseMode::Always.ignores_case("Error"));
        assert!(!CaseMode::Sensitive.ignores_case("error"));
    }
}
