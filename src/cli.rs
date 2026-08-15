//! The headless command line. Three verbs only -- `list`, `remove` and
//! `add` -- because the TUI is the primary interface; these exist so notcron
//! can be driven from a provisioning script.

use crate::cron::{self, Translation};
use crate::systemd;
use crate::unit::escape;
use crate::unit::model::{Body, Schedule, Scope, Unit};
use clap::{Args, Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "notcron",
    version,
    about = "systemd timers, services and mounts without the crontab",
    long_about = "Build and manage systemd timer/service pairs, standalone services \
                  and mount units.\n\nRun with no arguments for the interactive builder."
)]
pub struct Cli {
    /// start the TUI, draw one frame and exit (a smoke test for the terminal)
    #[arg(long, hide = true)]
    pub self_check: bool,

    #[command(flatten)]
    pub scope: ScopeArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// list the units notcron manages
    List(ListArgs),
    /// stop, disable and delete a notcron-managed unit
    Remove(RemoveArgs),
    /// create a timer + service pair from a cron expression
    Add(AddArgs),
}

/// `--user` (the default) or `--system`.
///
/// Declared once on the root command and marked global, so both
/// `notcron --system list` and `notcron list --system` parse. Defining it
/// per-subcommand instead would reject the first form, which is the one a
/// sysadmin reaches for.
#[derive(Args, Clone, Copy)]
pub struct ScopeArgs {
    /// operate on user units in ~/.config/systemd/user (default)
    #[arg(long, global = true, conflicts_with = "system")]
    user: bool,
    /// operate on system units in /etc/systemd/system (uses sudo)
    #[arg(long, global = true)]
    system: bool,
}

impl ScopeArgs {
    /// The chosen scope, or an error when both flags were given.
    ///
    /// clap's `conflicts_with` only fires when both occurrences land at the
    /// same level, so `notcron --user --system list` is caught by clap but
    /// `notcron --user list --system` is not -- a global arg simply merges
    /// into the root matches. Checking here covers every arrangement with
    /// one message.
    pub fn scope(self) -> Result<Scope, String> {
        match (self.user, self.system) {
            (true, true) => Err("--user and --system are mutually exclusive".into()),
            (_, true) => Ok(Scope::System),
            _ => Ok(Scope::User),
        }
    }
}

#[derive(Args)]
pub struct ListArgs {
    /// also list units notcron does not own (read-only)
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
pub struct RemoveArgs {
    /// unit name, with or without the notcron- prefix
    name: String,
}

#[derive(Args)]
pub struct AddArgs {
    /// unit name (default: derived from the command)
    #[arg(long)]
    name: Option<String>,

    /// unit Description=
    #[arg(long)]
    description: Option<String>,

    /// run the command through /bin/sh -c (implied by shell metacharacters)
    #[arg(long)]
    shell: bool,

    /// catch up on runs missed while powered off (default: on)
    #[arg(long, overrides_with = "no_persistent")]
    persistent: bool,
    /// do not catch up on missed runs
    #[arg(long = "no-persistent")]
    no_persistent: bool,

    /// RandomizedDelaySec=, e.g. 30s or 5m
    #[arg(long)]
    random_delay: Option<String>,

    /// WorkingDirectory=
    #[arg(long)]
    workdir: Option<String>,

    /// Environment=KEY=VALUE, repeatable
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,

    /// User= to run the command as (system units only)
    #[arg(long)]
    run_as: Option<String>,

    /// print the unit files instead of installing them
    #[arg(long)]
    dry_run: bool,

    /// 5-field cron expression, or @hourly/@daily/@weekly/@monthly/@yearly/@reboot
    schedule: String,

    /// the command to run, and its arguments
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("notcron: error: {msg}");
    ExitCode::FAILURE
}

pub fn run(cmd: Command, scope: Scope) -> ExitCode {
    match cmd {
        Command::List(a) => list(a, scope),
        Command::Remove(a) => remove(a, scope),
        Command::Add(a) => add(a, scope),
    }
}

fn list(a: ListArgs, scope: Scope) -> ExitCode {
    let entries = match systemd::list(scope, a.all) {
        Ok(e) => e,
        Err(e) => return fail(e),
    };
    if entries.is_empty() {
        println!("no notcron units installed ({} scope)", scope.as_str());
        return ExitCode::SUCCESS;
    }
    println!(
        "{:<34} {:<10} {:<10} {:<9} SCHEDULE",
        "UNIT", "KIND", "ACTIVE", "OWNED"
    );
    for e in entries {
        println!(
            "{:<34} {:<10} {:<10} {:<9} {}",
            e.primary,
            e.kind,
            if e.active.is_empty() {
                "unknown"
            } else {
                &e.active
            },
            if e.owned { "notcron" } else { "foreign" },
            e.schedule
        );
    }
    ExitCode::SUCCESS
}

fn remove(a: RemoveArgs, scope: Scope) -> ExitCode {
    let entries = match systemd::list(scope, true) {
        Ok(e) => e,
        Err(e) => return fail(e),
    };
    let wanted = crate::unit::model::prefixed(&a.name);
    let hit = entries.iter().find(|e| {
        e.primary == a.name
            || e.primary.rsplit_once('.').map(|(s, _)| s) == Some(wanted.as_str())
            || e.primary.rsplit_once('.').map(|(s, _)| s) == Some(a.name.as_str())
    });
    let Some(hit) = hit else {
        return fail(format!(
            "no unit named '{}' in {} scope (try: notcron list)",
            a.name,
            scope.as_str()
        ));
    };
    if !hit.owned {
        return fail(format!(
            "'{}' was not created by notcron and will not be removed",
            hit.primary
        ));
    }
    match systemd::remove(scope, &hit.files) {
        Ok(()) => {
            println!("removed {}", hit.files.join(", "));
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// Build the `ExecStart=` value and a name hint from a command line.
fn build_exec(command: &[String], force_shell: bool) -> (String, String) {
    let joined = command.join(" ");
    if force_shell || (command.len() == 1 && escape::needs_shell(&joined)) {
        let hint = joined
            .split_whitespace()
            .next()
            .unwrap_or("job")
            .rsplit('/')
            .next()
            .unwrap_or("job")
            .to_string();
        return (format!("/bin/sh -c {}", escape::exec_quote(&joined)), hint);
    }
    let hint = command[0].rsplit('/').next().unwrap_or("job").to_string();
    let exec = command
        .iter()
        .map(|a| escape::exec_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    (exec, hint)
}

fn add(a: AddArgs, scope: Scope) -> ExitCode {
    let (schedule, source) = match cron::to_calendar(&a.schedule) {
        Ok(Translation::Calendar(specs)) => {
            for s in &specs {
                if let Err(e) = systemd::check_calendar(s) {
                    return fail(format!("generated calendar spec '{s}' is invalid: {e}"));
                }
            }
            (
                Schedule::Calendar(specs),
                format!("cron: {}", a.schedule.trim()),
            )
        }
        Ok(Translation::Reboot) => (
            Schedule::Boot {
                boot: "1min".into(),
            },
            "@reboot".to_string(),
        ),
        Err(e) => return fail(e),
    };

    if let Some(d) = &a.random_delay {
        if let Err(e) = systemd::check_timespan(d) {
            return fail(e);
        }
    }
    for e in &a.env {
        if !e.contains('=') {
            return fail(format!("--env expects KEY=VALUE, got '{e}'"));
        }
    }

    let (exec, hint) = build_exec(&a.command, a.shell);
    let name = escape::slugify(a.name.as_deref().unwrap_or(&hint));

    let mut u = Unit::new_timer(scope);
    u.name = name.clone();
    u.description = a
        .description
        .unwrap_or_else(|| format!("{name} ({source})"));
    if let Body::Timer(t) = &mut u.body {
        t.schedule = schedule;
        t.source = source;
        t.persistent = !a.no_persistent;
        t.randomized_delay = a.random_delay;
        t.service.exec_start = exec;
        t.service.working_directory = a.workdir;
        t.service.run_as = a.run_as;
        t.service.environment = a.env;
    }
    if let Err(e) = u.validate() {
        return fail(e);
    }

    if a.dry_run {
        print!(
            "{}",
            crate::unit::generate::preview(&u, &systemd::unit_dir(scope).to_string_lossy())
        );
        return ExitCode::SUCCESS;
    }

    match systemd::install(&u, true, true) {
        Ok(r) => {
            for p in &r.written {
                println!("wrote {}", p.display());
            }
            for w in &r.warnings {
                eprintln!("notcron: warning: {w}");
            }
            match u.primary_unit() {
                Ok(p) => println!("enabled and started {p}"),
                Err(e) => return fail(e),
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn plain_commands_become_argv_exec_lines() {
        let cmd: Vec<String> = ["/usr/local/bin/backup.sh", "--full"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (exec, hint) = build_exec(&cmd, false);
        assert_eq!(exec, "/usr/local/bin/backup.sh --full");
        assert_eq!(hint, "backup.sh");
    }

    #[test]
    fn arguments_needing_quotes_get_them() {
        let cmd: Vec<String> = ["/bin/echo", "hello world"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (exec, _) = build_exec(&cmd, false);
        assert_eq!(exec, "/bin/echo \"hello world\"");
    }

    #[test]
    fn shell_syntax_is_wrapped() {
        let cmd = vec!["df -h | mail -s disk me@example.com".to_string()];
        let (exec, hint) = build_exec(&cmd, false);
        assert_eq!(exec, "/bin/sh -c \"df -h | mail -s disk me@example.com\"");
        assert_eq!(hint, "df");
        // --shell forces it even for an argv that would not need one.
        let cmd = vec!["/bin/true".to_string()];
        let (exec, _) = build_exec(&cmd, true);
        assert_eq!(exec, "/bin/sh -c /bin/true");
    }

    #[test]
    fn add_parses_a_cron_expression_and_a_trailing_command() {
        let cli = Cli::try_parse_from([
            "notcron",
            "add",
            "--name",
            "backup",
            "0 3 * * *",
            "/usr/local/bin/backup.sh",
            "--full",
        ])
        .expect("should parse");
        let Some(Command::Add(a)) = cli.command else {
            panic!("expected add")
        };
        assert_eq!(a.schedule, "0 3 * * *");
        assert_eq!(a.command, ["/usr/local/bin/backup.sh", "--full"]);
        assert_eq!(a.name.as_deref(), Some("backup"));
    }

    fn scope_of(argv: &[&str]) -> Scope {
        Cli::try_parse_from(argv)
            .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"))
            .scope
            .scope()
            .unwrap_or_else(|e| panic!("{argv:?} should resolve a scope: {e}"))
    }

    #[test]
    fn scope_defaults_to_user() {
        assert_eq!(scope_of(&["notcron"]), Scope::User);
        assert_eq!(scope_of(&["notcron", "list"]), Scope::User);
        assert_eq!(scope_of(&["notcron", "remove", "x"]), Scope::User);
        assert_eq!(
            scope_of(&["notcron", "add", "@daily", "/bin/true"]),
            Scope::User
        );
    }

    /// `notcron --system list` and `notcron list --system` must be the same
    /// command; a global arg on the root command is what makes both parse.
    #[test]
    fn scope_flags_work_before_and_after_the_subcommand() {
        for (before, after) in [
            (
                vec!["notcron", "--system", "list"],
                vec!["notcron", "list", "--system"],
            ),
            (
                vec!["notcron", "--system", "list", "--all"],
                vec!["notcron", "list", "--all", "--system"],
            ),
            (
                vec!["notcron", "--system", "remove", "backup"],
                vec!["notcron", "remove", "backup", "--system"],
            ),
            (
                vec!["notcron", "--system", "add", "@daily", "/bin/true"],
                vec!["notcron", "add", "--system", "@daily", "/bin/true"],
            ),
        ] {
            assert_eq!(scope_of(&before), Scope::System, "{before:?}");
            assert_eq!(scope_of(&after), Scope::System, "{after:?}");
        }
    }

    #[test]
    fn explicit_user_flag_works_in_both_positions_too() {
        assert_eq!(scope_of(&["notcron", "--user", "list"]), Scope::User);
        assert_eq!(scope_of(&["notcron", "list", "--user"]), Scope::User);
        assert_eq!(
            scope_of(&["notcron", "--user", "add", "@daily", "/bin/true"]),
            Scope::User
        );
    }

    /// The scope also applies to the TUI, so `notcron --system` opens there.
    #[test]
    fn the_bare_tui_honours_the_scope_flag() {
        let cli = Cli::try_parse_from(["notcron", "--system"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.scope.scope(), Ok(Scope::System));
    }

    #[test]
    fn user_and_system_together_are_rejected() {
        for argv in [
            vec!["notcron", "--user", "--system"],
            vec!["notcron", "--user", "--system", "list"],
            vec!["notcron", "list", "--user", "--system"],
            // clap catches the cases above at parse time; this one reaches
            // ScopeArgs::scope, which must reject it just the same.
            vec!["notcron", "--user", "list", "--system"],
            vec![
                "notcron",
                "--system",
                "add",
                "--user",
                "@daily",
                "/bin/true",
            ],
        ] {
            let rejected = match Cli::try_parse_from(&argv) {
                Err(_) => true,
                Ok(cli) => cli.scope.scope().is_err(),
            };
            assert!(rejected, "{argv:?} should be rejected");
        }
    }

    /// Repeating the flag on both sides is harmless, not a duplicate-arg
    /// error -- a global arg accepts the same value twice.
    #[test]
    fn repeating_the_same_scope_flag_is_accepted() {
        assert_eq!(
            scope_of(&["notcron", "--system", "list", "--system"]),
            Scope::System
        );
    }

    #[test]
    fn a_scope_flag_after_the_command_is_still_part_of_the_command() {
        // Everything after the executable is the job's own argv, including
        // things that look like notcron's flags.
        let cli =
            Cli::try_parse_from(["notcron", "add", "@daily", "/bin/echo", "--system"]).unwrap();
        let Some(Command::Add(a)) = cli.command else {
            panic!("expected add")
        };
        assert_eq!(a.command, ["/bin/echo", "--system"]);
        assert_eq!(cli.scope.scope(), Ok(Scope::User));
    }

    #[test]
    fn no_arguments_means_the_tui() {
        let cli = Cli::try_parse_from(["notcron"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.self_check);
    }
}
