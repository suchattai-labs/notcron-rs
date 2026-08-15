//! The headless command line. Six verbs only -- `list`, `remove`, `add`,
//! `export`, `trash` and `restore` -- because the TUI is the primary
//! interface; these exist so notcron can be driven from a provisioning
//! script. `trash` and `restore` are the scriptable half of the TUI's undo:
//! whatever `remove` stashed can be listed and put back without a terminal.

use crate::cron::{self, Translation};
use crate::export as exporter;
use crate::linger;
use crate::systemd;
use crate::trash::{RestoreError, Trash, TrashEntry};
use crate::unit::escape;
use crate::unit::model::{Body, Schedule, Scope, Unit};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
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
    /// print a unit's files, or write them to a directory
    Export(ExportArgs),
    /// list the removals still held in the trash
    Trash(TrashArgs),
    /// put a removed unit back, files and state
    Restore(RestoreArgs),
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
pub struct ExportArgs {
    /// write the files into this directory instead of printing them
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,

    /// overwrite files that already exist in --dir
    #[arg(long)]
    force: bool,

    /// unit name, with or without the notcron- prefix
    name: String,
}

/// `notcron trash` and `notcron trash list` are the same command.
///
/// Listing is the only thing the CLI does to the trash as a whole -- entries
/// expire on their own, and the TUI is where you go to throw one away early
/// -- so the bare verb does the obvious thing. The explicit `list` exists so
/// a script reads unambiguously and so a later `trash prune` has somewhere to
/// live without changing what `notcron trash` means today.
#[derive(Args)]
pub struct TrashArgs {
    #[command(subcommand)]
    what: Option<TrashCommand>,
}

#[derive(Subcommand)]
pub enum TrashCommand {
    /// list the removals still held in the trash (the default)
    List,
}

#[derive(Args)]
pub struct RestoreArgs {
    /// put the files back but leave the unit neither enabled nor started
    #[arg(long)]
    no_enable: bool,

    /// overwrite unit files that exist again under the original names
    #[arg(long)]
    force: bool,

    /// trash id from `notcron trash`, or an unambiguous prefix of one
    id: String,
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
        Command::Export(a) => export(a, scope),
        Command::Trash(a) => trash_list(a, scope),
        Command::Restore(a) => restore(a, scope),
    }
}

/// Match a user-typed name against a listing, accepting the bare stem
/// (`backup`), the prefixed stem (`notcron-backup`) or the full unit name
/// (`notcron-backup.timer`).
pub(crate) fn matches_name(e: &systemd::Entry, name: &str) -> bool {
    let wanted = crate::unit::model::prefixed(name);
    let stem = e.primary.rsplit_once('.').map(|(s, _)| s);
    e.primary == name || stem == Some(wanted.as_str()) || stem == Some(name)
}

/// Find one unit by name, or produce the "no such unit" message.
fn find(scope: Scope, name: &str) -> Result<systemd::Entry, String> {
    let entries = systemd::list(scope, true)?;
    entries
        .into_iter()
        .find(|e| matches_name(e, name))
        .ok_or_else(|| {
            format!(
                "no unit named '{name}' in {} scope (try: notcron list)",
                scope.as_str()
            )
        })
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
    let hit = match find(scope, &a.name) {
        Ok(e) => e,
        Err(e) => return fail(e),
    };
    if !hit.owned {
        return fail(format!(
            "'{}' was not created by notcron and will not be removed",
            hit.primary
        ));
    }
    match systemd::remove_reporting(scope, &hit.files) {
        Ok(r) => {
            println!("removed {}", hit.files.join(", "));
            for w in &r.warnings {
                eprintln!("notcron: warning: {w}");
            }
            // The files are not gone, only stashed; say where, and how to
            // change your mind.
            if let Some(t) = &r.trashed {
                // The id printed here is the exact token `restore` takes.
                println!("kept in the trash as {}", t.id);
                println!("undo with: notcron restore {}{}", t.id, scope_suffix(scope));
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------------------
// trash / restore
// ---------------------------------------------------------------------------

/// What to append to a suggested command line so it stays in this scope.
/// User scope is the default, so it needs nothing.
pub(crate) fn scope_suffix(scope: Scope) -> &'static str {
    match scope {
        Scope::System => " --system",
        Scope::User => "",
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An age as a single whitespace-free token, so `awk`-style column splitting
/// keeps working. The TUI's "3m ago" is friendlier to read and worse to parse.
pub(crate) fn age_token(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The state a unit was in when it was removed, as one token: `enabled`,
/// `active`, `enabled,active` or `-`. Never empty, so the column is always
/// present for a script splitting on whitespace.
pub(crate) fn state_token(enabled: bool, active: bool) -> String {
    match (enabled, active) {
        (true, true) => "enabled,active".into(),
        (true, false) => "enabled".into(),
        (false, true) => "active".into(),
        (false, false) => "-".into(),
    }
}

pub(crate) fn trash_header() -> String {
    format!(
        "{:<42} {:<26} {:<6} {:>4} {:>5} STATE",
        "ID", "UNIT", "SCOPE", "AGE", "FILES"
    )
}

/// One listing row. The id comes first and is never truncated: it is the
/// token `notcron restore` takes, so `notcron trash | awk '{print $1}'` has
/// to yield something usable.
pub(crate) fn trash_row(e: &TrashEntry, now: u64) -> String {
    format!(
        "{:<42} {:<26} {:<6} {:>4} {:>5} {}",
        e.id,
        e.unit,
        e.scope.as_str(),
        age_token(e.age_secs(now)),
        e.files.len(),
        state_token(e.was_enabled, e.was_active)
    )
}

/// Turn what the user typed into exactly one trash id.
///
/// An exact id always wins. Otherwise the token is treated as a prefix --
/// ids are timestamped and long, and the timestamp alone usually picks out
/// one removal. An ambiguous prefix is an error listing the candidates:
/// guessing which of two removals to undo is not a decision notcron may make
/// on the user's behalf.
pub(crate) fn resolve_trash_id(entries: &[TrashEntry], token: &str) -> Result<String, String> {
    if token.is_empty() {
        return Err("no trash id given (try: notcron trash)".into());
    }
    if let Some(e) = entries.iter().find(|e| e.id == token) {
        return Ok(e.id.clone());
    }
    let hits: Vec<&TrashEntry> = entries.iter().filter(|e| e.id.starts_with(token)).collect();
    match hits.len() {
        0 => Err(format!(
            "no trash entry matching '{token}' (try: notcron trash)"
        )),
        1 => Ok(hits[0].id.clone()),
        _ => {
            let names: Vec<&str> = hits.iter().map(|e| e.id.as_str()).collect();
            Err(format!(
                "'{token}' matches {} entries; use a longer id:\n  {}",
                hits.len(),
                names.join("\n  ")
            ))
        }
    }
}

fn trash_list(a: TrashArgs, scope: Scope) -> ExitCode {
    // Only one thing to do so far; the match keeps a later verb honest.
    match a.what {
        None | Some(TrashCommand::List) => {}
    }
    let entries = match Trash::for_scope(scope).list() {
        Ok(e) => e,
        Err(e) => return fail(e),
    };
    if entries.is_empty() {
        println!("the trash is empty ({} scope)", scope.as_str());
        return ExitCode::SUCCESS;
    }
    let now = now_secs();
    println!("{}", trash_header());
    for e in &entries {
        println!("{}", trash_row(e, now));
    }
    ExitCode::SUCCESS
}

/// `notcron restore <id>` -- the scriptable half of the TUI's undo.
///
/// Restoring puts the unit back the way it was, enabled and running included:
/// a caller who scripted `remove` and then `restore` expects the timer to be
/// firing again afterwards, and a unit that is on disk but inert is the
/// failure mode that goes unnoticed until the job silently stops running.
/// `--no-enable` is there for the case where the files are all that is
/// wanted.
fn restore(a: RestoreArgs, scope: Scope) -> ExitCode {
    let trash = Trash::for_scope(scope);
    let entries = match trash.list() {
        Ok(e) => e,
        Err(e) => return fail(e),
    };
    let id = match resolve_trash_id(&entries, &a.id) {
        Ok(id) => id,
        Err(e) => return fail(e),
    };

    let report = match trash.restore(&id, a.force) {
        Ok(r) => r,
        Err(RestoreError::Conflict(paths)) => {
            let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            return fail(format!(
                "{} already exist(s); nothing was moved. Re-run with --force to replace.",
                names.join(", ")
            ));
        }
        Err(e) => return fail(e),
    };

    for p in &report.restored {
        println!("restored {}", p.display());
    }
    for p in &report.overwritten {
        eprintln!("notcron: warning: replaced {}", p.display());
    }
    if let Err(e) = systemd::daemon_reload(scope) {
        eprintln!("notcron: warning: daemon-reload: {}", e.trim());
    }

    let want = crate::ui::trashview::describe_state(report.was_enabled, report.was_active);
    if !(report.was_enabled || report.was_active) {
        return ExitCode::SUCCESS;
    }
    if a.no_enable {
        println!(
            "{} was {want} when removed; --no-enable left it alone",
            report.unit
        );
        return ExitCode::SUCCESS;
    }
    // enable --now covers both halves in one call; a unit that was running
    // but never enabled only wants a start.
    let mut args: Vec<&str> = match (report.was_enabled, report.was_active) {
        (true, true) => vec!["enable", "--now"],
        (true, false) => vec!["enable"],
        _ => vec!["start"],
    };
    args.push(&report.unit);
    match systemd::systemctl(scope, &args) {
        // The files are back either way, so a failure here is a warning, not
        // a failed restore -- exactly as `add` treats a failed enable.
        Ok(_) => {
            println!("{} is {want} again", report.unit);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "notcron: warning: {} is restored but not {want}: {}",
                report.unit,
                e.trim()
            );
            ExitCode::SUCCESS
        }
    }
}

/// `notcron export <name>` -- print the unit's files, or write them to a
/// directory with `--dir`.
///
/// Restricted to notcron-owned units, like `remove`: the bytes are re-rendered
/// from notcron's own model, so exporting a foreign unit would hand back
/// something subtly different from what is installed. `notcron list --all`
/// plus `cat` is the honest way to look at those.
fn export(a: ExportArgs, scope: Scope) -> ExitCode {
    let hit = match find(scope, &a.name) {
        Ok(e) => e,
        Err(e) => return fail(e),
    };
    if !hit.owned {
        return fail(format!(
            "'{}' was not created by notcron; export re-renders from notcron's \
             model and would not reproduce it faithfully",
            hit.primary
        ));
    }
    let Some(u) = hit.unit else {
        return fail(format!(
            "'{}' could not be modelled by notcron and cannot be exported",
            hit.primary
        ));
    };

    let Some(dir) = a.dir else {
        // stdout: a broken pipe (`notcron export foo | head`) is success.
        let mut out = std::io::stdout();
        return match exporter::write_text(&u, &mut out) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        };
    };

    match exporter::export(&u, &dir, a.force) {
        Ok(r) => {
            for p in &r.written {
                println!("wrote {}", p.display());
            }
            ExitCode::SUCCESS
        }
        Err(exporter::ExportError::Exists(paths)) => {
            let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            fail(format!(
                "{} already exist(s); nothing was written. Re-run with --force to replace.",
                names.join(", ")
            ))
        }
        Err(e) => fail(e),
    }
}

/// Build the `ExecStart=` value and a name hint from a command line.
pub(crate) fn build_exec(command: &[String], force_shell: bool) -> (String, String) {
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
            // A user timer that outlives no login session is the commonest way
            // for a freshly added job to silently never run. The TUI prompts;
            // the CLI has nowhere to prompt from, so it warns and moves on.
            if let Some(w) = linger::check(scope).warning() {
                eprintln!("notcron: warning: {w}");
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
            (
                vec!["notcron", "--system", "trash"],
                vec!["notcron", "trash", "--system"],
            ),
            (
                vec!["notcron", "--system", "trash", "list"],
                vec!["notcron", "trash", "list", "--system"],
            ),
            (
                vec!["notcron", "--system", "restore", "20260815T105403Z-x.timer"],
                vec!["notcron", "restore", "20260815T105403Z-x.timer", "--system"],
            ),
            (
                vec!["notcron", "--system", "restore", "--force", "abc"],
                vec!["notcron", "restore", "abc", "--force", "--system"],
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

    // -----------------------------------------------------------------
    // export
    // -----------------------------------------------------------------

    fn export_args(argv: &[&str]) -> ExportArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Export(a)) => a,
            _ => panic!("expected export in {argv:?}"),
        }
    }

    #[test]
    fn export_defaults_to_stdout_and_refuses_to_clobber() {
        let a = export_args(&["notcron", "export", "backup"]);
        assert_eq!(a.name, "backup");
        assert!(a.dir.is_none(), "no --dir means print to stdout");
        assert!(!a.force, "overwriting must be opt-in");
    }

    #[test]
    fn export_takes_a_directory_and_a_force_flag() {
        let a = export_args(&[
            "notcron", "export", "--dir", "/tmp/out", "--force", "backup",
        ]);
        assert_eq!(a.dir, Some(PathBuf::from("/tmp/out")));
        assert!(a.force);
        assert_eq!(a.name, "backup");
    }

    /// `--user`/`--system` are global, so they work on export from either
    /// side, exactly as they do on list and remove.
    #[test]
    fn export_honours_the_global_scope_flags() {
        assert_eq!(
            scope_of(&["notcron", "--system", "export", "backup"]),
            Scope::System
        );
        assert_eq!(
            scope_of(&["notcron", "export", "backup", "--system"]),
            Scope::System
        );
        assert_eq!(scope_of(&["notcron", "export", "backup"]), Scope::User);
    }

    #[test]
    fn export_needs_a_unit_name() {
        assert!(Cli::try_parse_from(["notcron", "export"]).is_err());
    }

    /// The lookup `export` and `remove` share: a user may type the bare stem,
    /// the prefixed stem or the full unit name.
    #[test]
    fn a_unit_is_found_by_bare_prefixed_or_full_name() {
        let e = systemd::Entry {
            primary: "notcron-backup.timer".into(),
            files: vec!["notcron-backup.timer".into()],
            scope: Scope::User,
            owned: true,
            description: String::new(),
            kind: "timer",
            schedule: String::new(),
            unit: None,
            active: String::new(),
            enabled: String::new(),
        };
        for name in ["backup", "notcron-backup", "notcron-backup.timer"] {
            assert!(matches_name(&e, name), "{name} should match");
        }
        for name in ["backu", "backup.timer", "other"] {
            assert!(!matches_name(&e, name), "{name} should not match");
        }
    }

    /// The CLI's own promise on top of `export::export`: a refused overwrite
    /// names every offending file and leaves all of them untouched.
    #[test]
    fn exporting_over_existing_files_writes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("out");
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        u.description = "backup".into();
        if let Body::Timer(t) = &mut u.body {
            t.schedule = Schedule::Calendar(vec!["*-*-* 03:00:00".into()]);
            t.service.exec_start = "/bin/true".into();
        }

        // A clean export first, so we know what the file names are.
        let first = exporter::export(&u, &dir, false).unwrap();
        assert!(first.written.len() >= 2, "{:?}", first.written);
        for p in &first.written {
            std::fs::write(p, "PRE-EXISTING\n").unwrap();
        }

        match exporter::export(&u, &dir, false) {
            Err(exporter::ExportError::Exists(paths)) => {
                assert_eq!(paths.len(), first.written.len());
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        // Every file still holds what it held before the refused export.
        for p in &first.written {
            assert_eq!(std::fs::read_to_string(p).unwrap(), "PRE-EXISTING\n");
        }

        // --force is the documented way through.
        let forced = exporter::export(&u, &dir, true).unwrap();
        assert_eq!(forced.replaced.len(), first.written.len());
        for p in &forced.written {
            assert_ne!(std::fs::read_to_string(p).unwrap(), "PRE-EXISTING\n");
        }
    }

    // -----------------------------------------------------------------
    // trash / restore
    // -----------------------------------------------------------------

    fn restore_args(argv: &[&str]) -> RestoreArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Restore(a)) => a,
            _ => panic!("expected restore in {argv:?}"),
        }
    }

    #[test]
    fn restore_takes_an_id_and_defaults_to_putting_the_state_back() {
        let a = restore_args(&["notcron", "restore", "20260815T105403Z-notcron-foo.timer"]);
        assert_eq!(a.id, "20260815T105403Z-notcron-foo.timer");
        assert!(!a.force, "clobbering must be opt-in");
        assert!(
            !a.no_enable,
            "restoring the enabled/active state is the default"
        );
    }

    #[test]
    fn restore_accepts_its_two_flags_in_any_order() {
        for argv in [
            vec!["notcron", "restore", "--force", "--no-enable", "abc"],
            vec!["notcron", "restore", "abc", "--no-enable", "--force"],
        ] {
            let a = restore_args(&argv);
            assert_eq!(a.id, "abc", "{argv:?}");
            assert!(a.force && a.no_enable, "{argv:?}");
        }
    }

    #[test]
    fn restore_needs_an_id() {
        assert!(Cli::try_parse_from(["notcron", "restore"]).is_err());
    }

    /// `notcron trash` and `notcron trash list` are the same command, and
    /// nothing else is a trash subcommand.
    #[test]
    fn the_bare_trash_verb_means_list() {
        for argv in [
            vec!["notcron", "trash"],
            vec!["notcron", "trash", "list"],
            vec!["notcron", "trash", "list", "--system"],
        ] {
            match Cli::try_parse_from(&argv).unwrap().command {
                Some(Command::Trash(_)) => {}
                _ => panic!("expected trash in {argv:?}"),
            }
        }
        assert!(Cli::try_parse_from(["notcron", "trash", "empty"]).is_err());
    }

    // Ids as the trash really makes them: a compact UTC stamp, a dash, the
    // unit name.
    fn entry(id: &str, unit: &str) -> TrashEntry {
        TrashEntry {
            id: id.into(),
            unit: unit.into(),
            scope: Scope::User,
            removed_at: 1_755_255_243,
            was_enabled: true,
            was_active: true,
            files: vec![crate::trash::TrashedFile {
                stored: unit.into(),
                original: PathBuf::from("/tmp").join(unit),
            }],
        }
    }

    #[test]
    fn an_exact_id_resolves_to_itself() {
        let es = vec![
            entry("20260815T105403Z-notcron-foo.timer", "notcron-foo.timer"),
            entry("20260815T105501Z-notcron-bar.timer", "notcron-bar.timer"),
        ];
        assert_eq!(
            resolve_trash_id(&es, "20260815T105403Z-notcron-foo.timer").unwrap(),
            "20260815T105403Z-notcron-foo.timer"
        );
    }

    #[test]
    fn an_unambiguous_prefix_is_enough() {
        let es = vec![
            entry("20260815T105403Z-notcron-foo.timer", "notcron-foo.timer"),
            entry("20260815T105501Z-notcron-bar.timer", "notcron-bar.timer"),
        ];
        assert_eq!(
            resolve_trash_id(&es, "20260815T1054").unwrap(),
            "20260815T105403Z-notcron-foo.timer"
        );
        // Down to the single character that still separates them.
        assert_eq!(
            resolve_trash_id(&es, "20260815T1055").unwrap(),
            "20260815T105501Z-notcron-bar.timer"
        );
    }

    /// Two removals in the same second differ only by a counter suffix, which
    /// is exactly when a short prefix stops being safe. Guessing would undo
    /// the wrong removal, so it is an error that names the candidates.
    #[test]
    fn an_ambiguous_prefix_is_refused_rather_than_guessed() {
        let es = vec![
            entry("20260815T105403Z-notcron-foo.timer", "notcron-foo.timer"),
            entry("20260815T105403Z-notcron-foo.timer-1", "notcron-foo.timer"),
        ];
        let err = resolve_trash_id(&es, "20260815T1054").unwrap_err();
        assert!(err.contains("matches 2 entries"), "{err}");
        assert!(
            err.contains("20260815T105403Z-notcron-foo.timer-1"),
            "{err}"
        );
        // The shorter id is still reachable exactly, even though it is a
        // prefix of the longer one.
        assert_eq!(
            resolve_trash_id(&es, "20260815T105403Z-notcron-foo.timer").unwrap(),
            "20260815T105403Z-notcron-foo.timer"
        );
    }

    #[test]
    fn an_unknown_or_empty_id_says_where_to_look() {
        let es = vec![entry("20260815T105403Z-notcron-foo.timer", "x.timer")];
        for token in ["", "nope", "20260816"] {
            let err = resolve_trash_id(&es, token).unwrap_err();
            assert!(err.contains("notcron trash"), "{token}: {err}");
        }
        assert!(resolve_trash_id(&[], "anything").is_err());
    }

    #[test]
    fn the_listing_puts_the_id_first_and_splits_on_whitespace() {
        let e = entry("20260815T105403Z-notcron-foo.timer", "notcron-foo.timer");
        let row = trash_row(&e, e.removed_at + 90);
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cols[0], e.id, "the id must be the first column: {row}");
        assert_eq!(cols[1], "notcron-foo.timer");
        assert_eq!(cols[2], "user");
        assert_eq!(cols[3], "1m");
        assert_eq!(cols[4], "1");
        assert_eq!(cols[5], "enabled,active");
        assert_eq!(cols.len(), 6, "{row}");
        // Same column count in the header, so a script can trust the shape.
        assert_eq!(trash_header().split_whitespace().count(), 6);
    }

    #[test]
    fn every_removed_state_prints_one_word() {
        assert_eq!(state_token(true, true), "enabled,active");
        assert_eq!(state_token(true, false), "enabled");
        assert_eq!(state_token(false, true), "active");
        assert_eq!(state_token(false, false), "-");
        for (e, a) in [(true, true), (true, false), (false, true), (false, false)] {
            assert!(!state_token(e, a).contains(' '));
        }
    }

    #[test]
    fn ages_are_one_token_at_every_scale() {
        assert_eq!(age_token(0), "0s");
        assert_eq!(age_token(59), "59s");
        assert_eq!(age_token(60), "1m");
        assert_eq!(age_token(3599), "59m");
        assert_eq!(age_token(3600), "1h");
        assert_eq!(age_token(86_399), "23h");
        assert_eq!(age_token(86_400), "1d");
        assert_eq!(age_token(30 * 86_400), "30d");
    }

    #[test]
    fn a_system_scope_hint_carries_the_flag_and_a_user_one_does_not() {
        assert_eq!(scope_suffix(Scope::System), " --system");
        assert_eq!(scope_suffix(Scope::User), "");
    }

    /// The whole point of the feature: the id `remove` prints is the token
    /// `restore` takes. This walks the real path -- stash into a temporary
    /// trash, list it, resolve the printed id -- because a mismatch anywhere
    /// in that chain makes the verb useless.
    #[test]
    fn the_id_a_removal_prints_is_the_id_a_restore_accepts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let units = tmp.path().join("units");
        std::fs::create_dir_all(&units).unwrap();
        let timer = units.join("notcron-foo.timer");
        let service = units.join("notcron-foo.service");
        std::fs::write(&timer, "[Timer]\nOnCalendar=daily\n").unwrap();
        std::fs::write(&service, "[Service]\nExecStart=/bin/true\n").unwrap();

        let trash = Trash::at(tmp.path().join("trash"));
        let entry = trash
            .stash(&crate::trash::StashRequest {
                scope: Scope::User,
                unit: "notcron-foo.timer".into(),
                files: vec![timer.clone(), service.clone()],
                was_enabled: true,
                was_active: true,
            })
            .unwrap();

        // What `remove` prints.
        let printed = entry.id.clone();
        let listed = trash.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, printed);
        assert!(trash_row(&listed[0], now_secs()).starts_with(&printed));

        // What `restore` does with it, in full and by prefix.
        assert_eq!(resolve_trash_id(&listed, &printed).unwrap(), printed);
        assert_eq!(resolve_trash_id(&listed, &printed[..16]).unwrap(), printed);

        let report = trash.restore(&printed, false).unwrap();
        assert_eq!(report.restored, vec![timer.clone(), service.clone()]);
        assert!(report.was_enabled && report.was_active);
        assert_eq!(
            std::fs::read_to_string(&timer).unwrap(),
            "[Timer]\nOnCalendar=daily\n"
        );
        assert!(trash.list().unwrap().is_empty());
    }

    /// The CLI's promise on a conflict, mirroring `export --force`: the
    /// refusal names every offending path and moves nothing, and only
    /// `--force` goes through.
    #[test]
    fn restoring_over_a_unit_that_exists_again_moves_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let units = tmp.path().join("units");
        std::fs::create_dir_all(&units).unwrap();
        let timer = units.join("notcron-foo.timer");
        std::fs::write(&timer, "ORIGINAL\n").unwrap();

        let trash = Trash::at(tmp.path().join("trash"));
        let entry = trash
            .stash(&crate::trash::StashRequest {
                scope: Scope::User,
                unit: "notcron-foo.timer".into(),
                files: vec![timer.clone()],
                was_enabled: false,
                was_active: true,
            })
            .unwrap();
        assert!(!timer.exists(), "the stash moves the file out");

        // The name is taken again by the time the undo is attempted.
        std::fs::write(&timer, "REBUILT\n").unwrap();
        match trash.restore(&entry.id, false) {
            Err(RestoreError::Conflict(paths)) => assert_eq!(paths, vec![timer.clone()]),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&timer).unwrap(), "REBUILT\n");
        assert!(
            !trash.list().unwrap().is_empty(),
            "a refused restore keeps the entry"
        );

        let report = trash.restore(&entry.id, true).unwrap();
        assert_eq!(report.overwritten, vec![timer.clone()]);
        assert_eq!(std::fs::read_to_string(&timer).unwrap(), "ORIGINAL\n");
    }
}
