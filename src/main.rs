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
mod view;

use std::io::{IsTerminal, Write, stdout};

use command::App;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};

fn restore_terminal(alternate_screen: bool) {
    let mut out = stdout();
    if alternate_screen {
        let _ = execute!(out, LeaveAlternateScreen);
    }
    let _ = disable_raw_mode();
    if !alternate_screen {
        // -X: the last page stays on the screen; start the shell's prompt
        // on a fresh line below it.
        let _ = out.write_all(b"\r\n");
        let _ = out.flush();
    }
}

/// Output is not a terminal (`rless file | grep x`): behave like `cat`, as
/// less does, instead of writing screen-control sequences into a pipe.
fn copy_through(opts: &cli::Options) -> i32 {
    let mut out = stdout().lock();
    let mut code = 0;
    if opts.files.is_empty() {
        if std::io::copy(&mut std::io::stdin().lock(), &mut out).is_err() {
            code = 1;
        }
    } else {
        for path in &opts.files {
            match std::fs::File::open(path) {
                Ok(mut f) => {
                    if std::io::copy(&mut f, &mut out).is_err() {
                        code = 1;
                    }
                }
                Err(e) => {
                    eprintln!("{}: {}", path.display(), e);
                    code = 1;
                }
            }
        }
    }
    let _ = out.flush();
    code
}

fn main() {
    let opts = cli::parse();
    if !stdout().is_terminal() {
        std::process::exit(copy_through(&opts));
    }
    // With no file and nothing piped in there is nothing to page (and the
    // keyboard and the input would both be the terminal), as in less.
    if opts.files.is_empty() && std::io::stdin().is_terminal() {
        eprintln!("Missing filename (\"rless --help\" for help)");
        std::process::exit(1);
    }
    let alternate_screen = !opts.no_init;
    let quit_if_one_screen = opts.quit_if_one_screen;
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

    // -F: an input that fits on one screen is printed, and that is all.
    if quit_if_one_screen {
        match app.one_screen_output() {
            Ok(Some(text)) => {
                let mut out = stdout();
                let _ = out.write_all(&text);
                let _ = out.flush();
                std::process::exit(0);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(alternate_screen);
        default_hook(info);
    }));

    let mut out = stdout();
    let entered = enable_raw_mode().is_ok()
        && (!alternate_screen || execute!(out, EnterAlternateScreen).is_ok());
    if !entered {
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
