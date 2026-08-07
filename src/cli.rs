//! Pure argv → `Options` mapping (clap). No behavior logic.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "rust-less", about = "A clean-room Rust reimplementation of the core-pager subset of GNU less 668")]
struct Cli {
    /// -N / --LINE-NUMBERS: display a line number at the start of every line.
    #[arg(short = 'N', long = "LINE-NUMBERS")]
    line_numbers_on: bool,

    /// -n / --line-numbers: suppress line numbers (default).
    #[arg(short = 'n', long = "line-numbers")]
    line_numbers_off: bool,

    /// -S / --chop-long-lines: truncate long lines instead of wrapping.
    #[arg(short = 'S', long = "chop-long-lines")]
    chop_long_lines: bool,

    /// -R / --RAW-CONTROL-CHARS: pass ANSI color / OSC 8 sequences through raw.
    #[arg(short = 'R', long = "RAW-CONTROL-CHARS")]
    raw_control_chars: bool,

    /// -m: medium (percent) prompt.
    #[arg(short = 'm')]
    medium_prompt: bool,

    /// -M: long (byte/line-range/percent) prompt.
    #[arg(short = 'M')]
    long_prompt: bool,

    /// -q / --quiet: silence the bell for EOF/BOF-scroll errors only.
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// -Q / --QUIET: silence the bell entirely.
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
