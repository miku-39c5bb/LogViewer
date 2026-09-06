//! logviewer - terminal large log viewer/searcher

mod app;
mod core;
mod history;
mod keymap;
mod layout;
mod search;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: logviewer <log-file>");
        return ExitCode::from(2);
    }
    match app::App::open(&args[1]) {
        Ok(mut app) => match app.run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("run error: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("cannot open {}: {e}", args[1]);
            ExitCode::FAILURE
        }
    }
}