//! notcron -- systemd timers, services and mounts without the crontab.
//!
//! With no arguments this is a full-screen builder for systemd units. The
//! three subcommands (`list`, `remove`, `add`) exist so the same thing can be
//! driven from a script.

mod cli;
mod cron;
mod fieldhelp;
mod systemd;
mod ui;
mod unit;

// Undo-for-remove, the lingering check and unit export. Same story as below:
// the `allow(dead_code)` goes when the TUI calls into them.
#[allow(dead_code)]
mod export;
#[allow(dead_code)]
mod linger;
#[allow(dead_code)]
mod trash;

// Pure logic backing the builder's completion, validation and templates.
mod complete;
mod templates;
mod validate;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = cli::Cli::parse();
    // --user/--system are global, so they apply to the TUI as well as to the
    // subcommands; the TUI can still switch scope at runtime.
    let scope = match args.scope.scope() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("notcron: error: {e}");
            return ExitCode::from(2);
        }
    };

    if args.self_check {
        return ui::self_check(scope);
    }

    match args.command {
        Some(cmd) => cli::run(cmd, scope),
        None => ui::run(scope),
    }
}
