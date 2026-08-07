mod buffer;
mod cli;
mod command;
mod input;
mod prompt;
mod screen;
mod search;

use std::io::stdout;

use command::App;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};

fn restore_terminal() {
    let mut out = stdout();
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

fn main() {
    let opts = cli::parse();
    let mut app = App::new(opts);

    // File-not-found must exit before any terminal state is touched (§2
    // `exit-codes`: less exits so fast the capture harness's pane
    // disappeared before the first step ran).
    if let Err(e) = app.validate_first_file() {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut out = stdout();
    if enable_raw_mode().is_err() || execute!(out, EnterAlternateScreen).is_err() {
        eprintln!("failed to initialize terminal");
        std::process::exit(1);
    }

    if let Ok((w, h)) = size() {
        app.set_size(w, h);
    }

    let result = app.run_interactive(&mut out);

    restore_terminal();

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}
