#[cfg(test)]
mod bench;
mod buffer;
mod cli;
mod command;
mod input;
mod projection;
mod prompt;
mod screen;
mod search;
mod selector;
mod simd;
mod sniffer;

use std::io::{IsTerminal, Write, stdout};

use command::App;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size,
};

fn restore_terminal(alternate_screen: bool) {
    let mut out = stdout();
    if alternate_screen {
        let _ = execute!(out, LeaveAlternateScreen);
    } else if let Ok((_, h)) = size() {
        // `-X`: the last page stays on screen; only the prompt row goes,
        // and the shell prompt starts on it.
        let _ = execute!(
            out,
            MoveTo(0, h.saturating_sub(1)),
            Clear(ClearType::CurrentLine)
        );
    }
    let _ = disable_raw_mode();
}

fn main() {
    let opts = cli::parse();
    // With no file and nothing piped in, there is nothing to page (and the
    // keyboard and the input would both be the terminal), as in less.
    if opts.files.is_empty() && std::io::stdin().is_terminal() {
        eprintln!("Missing filename (\"rless --help\" for help)");
        std::process::exit(1);
    }
    let mut app = App::new(opts);

    // File-not-found must exit before any terminal state is touched (§2
    // `exit-codes`: less exits so fast the capture harness's pane
    // disappeared before the first step ran).
    if let Err(e) = app.validate_first_file() {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    if let Ok((w, h)) = size() {
        app.set_size(w, h);
    }

    // `-F`: input that fits on one screen is printed, not paged.
    if app.quit_if_one_screen() {
        match app.one_screen_output() {
            Ok(Some(text)) => {
                let mut out = stdout();
                let _ = out.write_all(&text).and_then(|()| out.flush());
                std::process::exit(0);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }

    let alternate_screen = !app.no_init();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(alternate_screen);
        default_hook(info);
    }));

    let mut out = stdout();
    if enable_raw_mode().is_err()
        || (alternate_screen && execute!(out, EnterAlternateScreen).is_err())
    {
        eprintln!("failed to initialize terminal");
        std::process::exit(1);
    }

    let result = app.run_interactive(&mut out);

    restore_terminal(alternate_screen);

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}
