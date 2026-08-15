//! notcron -- systemd timers, services and mounts without the crontab.
//!
//! With no arguments this is a full-screen builder for systemd units. The
//! three subcommands (`list`, `remove`, `add`) exist so the same thing can be
//! driven from a script.

mod cli;
mod cron;
mod systemd;
mod ui;
mod unit;

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
