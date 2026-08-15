pub mod builder;
pub mod dialogs;
pub mod editor;
pub mod list;
pub mod picker;
pub mod term;

use crate::unit::model::Scope;
use std::process::ExitCode;

/// Launch the TUI.
pub fn run(scope: Scope) -> ExitCode {
    let mut term = match term::Term::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("notcron: cannot set up the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    list::run(&mut term, scope);
    ExitCode::SUCCESS
}

/// `--self-check`: set the terminal up, paint one frame, tear it down again.
/// Enough to catch a broken layout or a terminal that cannot be entered,
/// without needing an interactive session.
pub fn self_check(scope: Scope) -> ExitCode {
    let app = list::App::new(scope);
    let mut term = match term::Term::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("notcron: cannot set up the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let res = term.terminal.draw(|f| list::draw(f, &app)).map(|_| ());
    drop(term);
    match res {
        Ok(()) => {
            println!("notcron: TUI self-check ok ({} units)", app.entries.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("notcron: TUI self-check failed: {e}");
            ExitCode::FAILURE
        }
    }
}
