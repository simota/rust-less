//! Pure argv → `Options` mapping (clap). No behavior logic.

use clap::Parser;
use std::path::PathBuf;

/// Interactive keys, shown after the options in `--help` and on the
/// in-pager help screen (`h`). The text is this project's own.
pub const KEYS_HELP: &str = "\
Moving (as in less; N is an optional count typed first):
  j k Enter Down Up      forward / backward one line (N j: N lines)
  f b Space PgDn PgUp    forward / backward one window
  d u                    half window (N d sets the default)
  g G                    first / last line (N g: line N)
  Right Left  ESC-) ESC-( scroll sideways (with -S, and in the table view)
  m<a-z>  '<a-z>  ''     set a mark, go to a mark, go back to where you were
  F                      follow: keep reading as the file grows (any key stops)

Searching and filtering:
  /pat ?pat n N          search forward / backward, repeat, repeat reversed
  &pat                   show only lines matching pat (&!pat: hide them;
                         empty: clear). Works on any file.

Files, options, output:
  :n :p                  next / previous file
  :w file                save the current view (filtered / sorted) to file
                         (:w! file overwrites)
  -N -S -i -I            toggle line numbers, chopping, smart / full case folding
  h                      this help
  q Q :q ZZ              quit

Structured input (JSON lines, CSV, TSV, logfmt; detected automatically):
  c                      table view on / off
  s                      cycle the format (plain, jsonl, csv, tsv, logfmt)
  z                      expand the top JSON line
  H                      sticky header row on / off
  x                      column mode: h/l move, p pin, . hide,
                         s sort (ascending, descending, off), i statistics, q done
  /expr  &expr           selector search and filter:
     .field OP value     OP is == != > >= < <= =~ (regex)
     .a.b                nested JSON field; a bare path tests that it exists
     && || ! ( )         combine: &.level == \"ERROR\" && .ms > 250ms
                         values: \"text\", 42, 1.5s, 250ms, true, false, null";

#[derive(Parser, Debug)]
#[command(
    name = "rless",
    about = "A clean-room Rust reimplementation of the core-pager subset of GNU less 668, \
             with structured-log views",
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

    /// Quit at once (printing the input) if it fits on one screen.
    #[arg(short = 'F', long = "quit-if-one-screen")]
    quit_if_one_screen: bool,

    /// Do not use the alternate screen: the last page stays visible after quitting.
    #[arg(short = 'X', long = "no-init")]
    no_init: bool,

    /// Start at the first line matching PATTERN (like +/PATTERN).
    #[arg(short = 'p', long = "pattern", value_name = "PATTERN")]
    pattern: Option<String>,

    /// Input files, read from stdin when empty. An argument +CMD runs CMD at
    /// startup: +G (end), +50 (line 50), +/pat, +?pat, +&pat (filter), +F (follow).
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

/// Case folding for searches and filters (`-i`, `-I`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    Sensitive,
    /// `-i`: fold case unless the pattern contains an uppercase letter.
    Smart,
    /// `-I`: always fold case.
    Always,
}

impl CaseMode {
    pub fn folds(self, pattern: &str) -> bool {
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
    /// Commands run before the first screen (`+CMD`, `-p`), in order.
    pub initial_commands: Vec<String>,
    pub files: Vec<PathBuf>,
}

impl Options {
    /// Defaults for everything (no flags, no files): what tests start from.
    #[cfg(test)]
    pub fn for_files(files: Vec<PathBuf>) -> Options {
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
            files,
        }
    }
}

/// Single-letter flags rless understands, as they may appear in `LESS`.
const ENV_FLAGS: &str = "NnSRmMqQiIFX";

/// The flags of a `LESS` value rless supports, as one `-...` argument.
/// Anything else (options rless lacks, option values such as `-x4`, long
/// options) is skipped, so a `LESS` written for GNU less never makes rless
/// refuse to start.
pub fn flags_from_less_env(value: &str) -> Option<String> {
    let mut flags = String::new();
    for token in value.split_whitespace() {
        if token.starts_with("--") || token.starts_with('+') {
            continue;
        }
        for c in token.trim_start_matches('-').chars() {
            if ENV_FLAGS.contains(c) {
                flags.push(c);
            } else {
                // Unknown flag: what follows may be its value.
                break;
            }
        }
    }
    (!flags.is_empty()).then(|| format!("-{flags}"))
}

/// Splits `+CMD` arguments off argv (they are commands, not file names);
/// everything after `--` is left alone.
fn split_plus_commands(args: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut rest = Vec::new();
    let mut commands = Vec::new();
    let mut after_dashdash = false;
    for (i, arg) in args.into_iter().enumerate() {
        if i > 0 && !after_dashdash && arg.len() > 1 && arg.starts_with('+') {
            commands.push(arg[1..].to_string());
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
    let mut args: Vec<String> = std::env::args().collect();
    if let Some(flags) = std::env::var("LESS")
        .ok()
        .as_deref()
        .and_then(flags_from_less_env)
    {
        args.insert(1.min(args.len()), flags);
    }
    parse_from(args)
}

fn parse_from(args: Vec<String>) -> Options {
    let (args, mut initial_commands) = split_plus_commands(args);
    let cli = Cli::parse_from(args);
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
    let case = if cli.ignore_case_always {
        CaseMode::Always
    } else if cli.ignore_case_smart {
        CaseMode::Smart
    } else {
        CaseMode::Sensitive
    };
    if let Some(p) = cli.pattern {
        initial_commands.insert(0, format!("/{p}"));
    }
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

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn less_env_keeps_only_supported_flags() {
        assert_eq!(flags_from_less_env("-FRX").as_deref(), Some("-FRX"));
        assert_eq!(flags_from_less_env("FRSX").as_deref(), Some("-FRSX"));
        // -x4 has a value; -j is unknown; long options are skipped.
        assert_eq!(
            flags_from_less_env("-R -x4 -j5 --mouse -i").as_deref(),
            Some("-Ri")
        );
        assert_eq!(flags_from_less_env("-Rx4S").as_deref(), Some("-R"));
        assert_eq!(flags_from_less_env("--use-color"), None);
        assert_eq!(flags_from_less_env(""), None);
    }

    #[test]
    fn plus_commands_and_pattern_become_initial_commands() {
        let o = parse_from(argv("rless +G -i a.log +/ERROR -p start b.log -- +literal"));
        assert_eq!(o.initial_commands, ["/start", "G", "/ERROR"]);
        assert_eq!(o.case, CaseMode::Smart);
        assert_eq!(
            o.files,
            [
                PathBuf::from("a.log"),
                PathBuf::from("b.log"),
                PathBuf::from("+literal")
            ]
        );
    }

    #[test]
    fn smart_case_folds_only_lowercase_patterns() {
        assert!(CaseMode::Smart.folds("error"));
        assert!(!CaseMode::Smart.folds("Error"));
        assert!(CaseMode::Always.folds("Error"));
        assert!(!CaseMode::Sensitive.folds("error"));
    }
}
