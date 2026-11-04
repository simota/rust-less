//! Pure argv → `Options` mapping (clap). No behavior logic.

use clap::Parser;
use std::path::PathBuf;

/// Interactive keys, shown after the options in `--help`. There is no
/// in-pager help screen (`h` is out of scope), so this is where the keys,
/// above all the non-less structured-view keys, can be found.
const KEYS_HELP: &str = "\
Keys (as in less):
  j k Enter Down Up      forward / backward one line (N j: N lines)
  f b Space PgDn PgUp    forward / backward one window
  d u                    half window (N d sets the default)
  g G                    first / last line (N g: line N)
  /pat ?pat n N          search forward / backward, repeat, repeat reversed
  :n :p                  next / previous file
  q Q :q ZZ              quit

Structured input (JSON lines, CSV, TSV, logfmt; detected automatically):
  c                      table view on / off
  s                      cycle the format (plain, jsonl, csv, tsv, logfmt)
  z                      expand the top JSON line
  H                      sticky header row on / off
  &expr                  show only matching lines (empty: clear)
  x                      column mode: h/l move, p pin, . hide, q done
  /.field OP value       selector search, also in &: OP is == != > >= < <= =~
                         e.g. /.status >= 500   &.level == \"ERROR\"   &.ms > 250ms";

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

#[derive(Debug, Clone)]
pub struct Options {
    pub line_numbers: bool,
    pub chop_long_lines: bool,
    pub raw_control_chars: bool,
    pub prompt_mode: PromptMode,
    pub bell: BellMode,
    pub files: Vec<PathBuf>,
}

pub fn parse() -> Options {
    let cli = Cli::parse();
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
        files: cli.files,
    }
}
