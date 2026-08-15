//! Everything that touches the system: unit directories, file installation
//! and removal, and the `systemctl` / `journalctl` / `systemd-analyze`
//! command line.
//!
//! System-scope writes go through `sudo`, matching the shell script. Nothing
//! here panics: every I/O and subprocess failure comes back as an `Err` with
//! a message fit to show the user.

use crate::trash::{PrunePolicy, StashRequest, Trash, TrashEntry};
use crate::unit::generate::{self, RenderedFile};
use crate::unit::model::{Scope, Unit};
use crate::unit::parse::{self, SourceFile};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Unit file suffixes notcron knows how to model.
const SUFFIXES: [&str; 4] = ["timer", "service", "mount", "automount"];

/// Where units of a given scope live.
pub fn unit_dir(scope: Scope) -> PathBuf {
    match scope {
        Scope::System => PathBuf::from("/etc/systemd/system"),
        Scope::User => {
            if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
                if !x.is_empty() {
                    return PathBuf::from(x).join("systemd/user");
                }
            }
            let home = std::env::var_os("HOME").map(PathBuf::from);
            home.unwrap_or_else(|| PathBuf::from("."))
                .join(".config/systemd/user")
        }
    }
}

fn need_sudo(scope: Scope) -> bool {
    // Only system-scope writes need elevation, and only when not already root.
    scope == Scope::System && !is_root()
}

/// True when the process is running as root, so no `sudo` is needed.
/// [`crate::trash`] asks the same question before elevating its own writes.
pub fn is_root() -> bool {
    // Cheap and dependency-free: /proc/self is owned by the process euid, but
    // reading the effective uid from `id -u` output is not worth a fork. The
    // USER-independent check is the metadata of /proc/self.
    std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid() == 0
        })
        .unwrap_or(false)
}

fn run(cmd: &mut Command) -> Result<Output, String> {
    cmd.output()
        .map_err(|e| format!("failed to run {:?}: {e}", cmd.get_program()))
}

/// Combined stdout+stderr of a command, regardless of exit status.
fn combined(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&err);
    }
    s
}

/// Run `systemctl` in the given scope. Returns the combined output; a
/// non-zero exit is an `Err` carrying that output.
pub fn systemctl(scope: Scope, args: &[&str]) -> Result<String, String> {
    let mut cmd = if need_sudo(scope) {
        let mut c = Command::new("sudo");
        c.arg("-n").arg("systemctl");
        c
    } else {
        Command::new("systemctl")
    };
    cmd.arg(scope.flag()).args(args);
    let out = run(&mut cmd)?;
    let text = combined(&out);
    if out.status.success() {
        Ok(text)
    } else {
        Err(if text.trim().is_empty() {
            format!("systemctl {} failed", args.join(" "))
        } else {
            text
        })
    }
}

/// Like [`systemctl`] but tolerates failure, returning the output either way.
/// Used for `status`, which exits non-zero for inactive units by design.
pub fn systemctl_lossy(scope: Scope, args: &[&str]) -> String {
    match systemctl(scope, args) {
        Ok(s) | Err(s) => s,
    }
}

pub fn daemon_reload(scope: Scope) -> Result<String, String> {
    systemctl(scope, &["daemon-reload"])
}

/// Whether a unit is enabled. `is-enabled` exits non-zero for every state
/// that is not "enabled", so the exit status is not the answer — the word it
/// prints is. `enabled-runtime` counts, since the unit really is wired up.
pub fn is_enabled(scope: Scope, unit: &str) -> bool {
    matches!(
        systemctl_lossy(scope, &["is-enabled", unit]).trim(),
        "enabled" | "enabled-runtime"
    )
}

/// Whether a unit is currently running. `is-active` exits non-zero for an
/// inactive unit by design, so again the printed word is the answer.
pub fn is_active(scope: Scope, unit: &str) -> bool {
    matches!(
        systemctl_lossy(scope, &["is-active", unit]).trim(),
        "active" | "activating" | "reloading"
    )
}

/// Recent journal entries for a unit.
pub fn journal(scope: Scope, unit: &str, lines: usize) -> String {
    let n = lines.to_string();
    let mut cmd = if need_sudo(scope) {
        let mut c = Command::new("sudo");
        c.arg("-n").arg("journalctl");
        c
    } else {
        Command::new("journalctl")
    };
    // journalctl has no --system flag with the same meaning; the default is
    // the system journal, so only the user flag is passed explicitly.
    if scope == Scope::User {
        cmd.arg("--user");
    }
    cmd.args(["--no-pager", "-n", &n, "-u", unit]);
    match run(&mut cmd) {
        Ok(o) => {
            let text = combined(&o);
            if text.trim().is_empty() {
                "(no journal entries)".into()
            } else {
                text
            }
        }
        Err(e) => e,
    }
}

/// True when `systemd-analyze` can be invoked at all.
// Only the tests consult this; the UI degrades on the `Unavailable` error
// instead of asking first, which avoids a second process spawn per preview.
#[cfg(test)]
pub fn has_analyze() -> bool {
    Command::new("systemd-analyze")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Validate an `OnCalendar=` spec. On success returns the next elapse, if
/// systemd reported one. Without `systemd-analyze` the spec is accepted.
pub fn check_calendar(spec: &str) -> Result<Option<String>, String> {
    let out = match Command::new("systemd-analyze")
        .args(["calendar", spec])
        .output()
    {
        Ok(o) => o,
        // No systemd-analyze: nothing to validate against, so do not block.
        Err(_) => return Ok(None),
    };
    if !out.status.success() {
        let msg = combined(&out);
        return Err(if msg.trim().is_empty() {
            format!("systemd rejected the calendar spec '{spec}'")
        } else {
            msg.trim().to_string()
        });
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Next elapse:"))
        .map(|s| s.trim().to_string()))
}

/// Validate a time span such as `15min` or `90s`.
pub fn check_timespan(spec: &str) -> Result<(), String> {
    let out = match Command::new("systemd-analyze")
        .args(["timespan", spec])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("'{spec}' is not a valid time span"))
    }
}

// ---------------------------------------------------------------------------
// Next-run preview
// ---------------------------------------------------------------------------

/// One future firing of a calendar spec, as reported by `systemd-analyze`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextRun {
    /// The elapse in the local timezone, e.g. `Mon 2026-08-17 03:00:00 CEST`.
    pub local: String,
    /// The same instant in UTC, e.g. `Mon 2026-08-17 01:00:00 UTC`. Empty if
    /// systemd did not print it (it omits the line when TZ is already UTC).
    pub utc: String,
    /// Relative form, e.g. `1 day 23h left`. Empty if not reported.
    pub from_now: String,
}

impl NextRun {
    /// A lexicographically sortable `YYYY-MM-DD HH:MM:SS` key.
    ///
    /// Derived from the UTC line so that specs in different timezones (and
    /// firings either side of a DST change) still order correctly; falls back
    /// to the local line when systemd printed no UTC form.
    pub fn sort_key(&self) -> &str {
        let src = if self.utc.is_empty() {
            &self.local
        } else {
            &self.utc
        };
        // "Mon 2026-08-17 01:00:00 UTC" -> "2026-08-17 01:00:00"
        let rest = src.split_once(' ').map(|(_, r)| r).unwrap_or(src);
        rest.rsplit_once(' ').map(|(l, _)| l).unwrap_or(rest)
    }
}

/// Why a next-run preview could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewError {
    /// `systemd-analyze` is not installed or could not be executed. The
    /// caller should degrade gracefully rather than treat this as a mistake
    /// by the user.
    Unavailable,
    /// systemd rejected the spec; the string is its own message.
    Invalid(String),
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreviewError::Unavailable => f.write_str("systemd-analyze is not available"),
            PreviewError::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for PreviewError {}

/// Pull the elapse list out of `systemd-analyze calendar --iterations=N` output.
///
/// The shape parsed is:
///
/// ```text
/// Normalized form: Mon *-*-* 03:00:00
///     Next elapse: Mon 2026-08-17 03:00:00 CEST
///        (in UTC): Mon 2026-08-17 01:00:00 UTC
///        From now: 1 day 23h left
///    Iteration #2: Mon 2026-08-24 03:00:00 CEST
///        ...
/// ```
///
/// `Next elapse: never` (a spec that can no longer fire) yields no entries.
fn parse_iterations(text: &str) -> Vec<NextRun> {
    let mut runs: Vec<NextRun> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        let Some((key, value)) = t.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let is_elapse = key == "Next elapse" || key.starts_with("Iteration #");
        if is_elapse {
            if value.is_empty() || value.eq_ignore_ascii_case("never") {
                continue;
            }
            runs.push(NextRun {
                local: value.to_string(),
                utc: String::new(),
                from_now: String::new(),
            });
        } else if key == "(in UTC)" {
            if let Some(last) = runs.last_mut() {
                last.utc = value.to_string();
            }
        } else if key == "From now" {
            if let Some(last) = runs.last_mut() {
                last.from_now = value.to_string();
            }
        }
    }
    runs
}

/// The next `count` firings of a single `OnCalendar=` spec.
pub fn next_runs(spec: &str, count: usize) -> Result<Vec<NextRun>, PreviewError> {
    let n = count.max(1).to_string();
    let out = Command::new("systemd-analyze")
        .args(["calendar", &format!("--iterations={n}"), spec])
        .output()
        .map_err(|_| PreviewError::Unavailable)?;
    if !out.status.success() {
        let msg = combined(&out);
        return Err(PreviewError::Invalid(if msg.trim().is_empty() {
            format!("systemd rejected the calendar spec '{spec}'")
        } else {
            msg.trim().to_string()
        }));
    }
    Ok(parse_iterations(&String::from_utf8_lossy(&out.stdout)))
}

/// The next `count` firings of a whole schedule.
///
/// A timer may carry several `OnCalendar=` lines and systemd fires on their
/// **union**, so the per-spec results are merged, sorted by absolute time,
/// deduplicated (two specs can name the same instant) and truncated back to
/// `count` — which is what the timer will actually do.
///
/// Any single invalid spec fails the whole preview, since that is what
/// systemd will complain about at load time too.
pub fn next_runs_multi(specs: &[String], count: usize) -> Result<Vec<NextRun>, PreviewError> {
    let live: Vec<&String> = specs.iter().filter(|s| !s.trim().is_empty()).collect();
    if live.is_empty() {
        return Ok(Vec::new());
    }
    let mut all: Vec<NextRun> = Vec::new();
    for s in live {
        all.extend(next_runs(s, count)?);
    }
    all.sort_by(|a, b| a.sort_key().cmp(b.sort_key()).then(a.local.cmp(&b.local)));
    all.dedup_by(|a, b| a.local == b.local);
    all.truncate(count.max(1));
    Ok(all)
}

// ---------------------------------------------------------------------------
// Installing and removing
// ---------------------------------------------------------------------------

fn write_file(scope: Scope, path: &std::path::Path, body: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("unit path has no parent directory")?;
    if need_sudo(scope) {
        let mk = run(Command::new("sudo").args(["-n", "mkdir", "-p"]).arg(dir))?;
        if !mk.status.success() {
            return Err(format!(
                "sudo mkdir -p {}: {}",
                dir.display(),
                combined(&mk)
            ));
        }
        let mut child = Command::new("sudo")
            .args(["-n", "tee"])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run sudo tee: {e}"))?;
        child
            .stdin
            .as_mut()
            .ok_or("sudo tee has no stdin")?
            .write_all(body.as_bytes())
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("sudo tee: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "writing {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    } else {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
    }
}

fn remove_file(scope: Scope, path: &std::path::Path) -> Result<(), String> {
    if need_sudo(scope) {
        let out = run(Command::new("sudo").args(["-n", "rm", "-f"]).arg(path))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("removing {}: {}", path.display(), combined(&out)))
        }
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("removing {}: {e}", path.display())),
        }
    }
}

/// What happened during an install, for reporting back to the user.
#[derive(Debug, Default)]
pub struct InstallReport {
    pub written: Vec<PathBuf>,
    /// Non-fatal problems, e.g. `enable` failing on a unit with no [Install].
    pub warnings: Vec<String>,
}

/// Write a unit's files, reload the manager, and optionally enable/start it.
pub fn install(u: &Unit, enable: bool, start: bool) -> Result<InstallReport, String> {
    u.validate()?;
    let dir = unit_dir(u.scope);
    let files: Vec<RenderedFile> = generate::render(u);
    let mut report = InstallReport::default();
    for f in &files {
        let path = dir.join(&f.name);
        write_file(u.scope, &path, &f.body)?;
        report.written.push(path);
    }
    daemon_reload(u.scope)?;

    let primary = u.primary_unit()?;
    if enable {
        if let Err(e) = systemctl(u.scope, &["enable", &primary]) {
            report
                .warnings
                .push(format!("enable {primary}: {}", e.trim()));
        }
    }
    if start {
        if let Err(e) = systemctl(u.scope, &["start", &primary]) {
            report
                .warnings
                .push(format!("start {primary}: {}", e.trim()));
        }
    }
    Ok(report)
}

/// What a removal did, beyond succeeding.
#[derive(Debug, Default)]
pub struct RemoveReport {
    /// The trash entry the unit's files were stashed into, so the caller can
    /// offer an undo. `None` only when there was nothing on disk to stash.
    pub trashed: Option<TrashEntry>,
    /// Non-fatal problems, e.g. the retention prune failing. The removal
    /// itself happened.
    pub warnings: Vec<String>,
}

/// Stop, disable, delete and forget a unit.
///
/// This is the only path that deletes unit files, and it never deletes them
/// outright: the files are *moved* into the scope's trash
/// ([`crate::trash`]) so the removal can be undone. Both the `notcron remove`
/// CLI and the TUI go through here, so undo exists regardless of entry point.
///
/// # Stash failure aborts the removal
///
/// If the files cannot be stashed the unit is **not** deleted and an error is
/// returned. Refusing to delete what could not be backed up is the only
/// choice that cannot lose a user's work; the alternative — deleting anyway
/// with a warning — turns a full disk or a missing `sudo` rule into silent
/// data loss, on the one operation a user is most likely to regret. A stash
/// is all-or-nothing (see [`Trash::stash`]), so an abort leaves every file
/// where it was. The unit may already have been stopped and disabled by the
/// time the stash is attempted, which the error says.
///
/// Best-effort on the systemctl steps otherwise: a unit that was never
/// enabled must still be removable.
pub fn remove(scope: Scope, files: &[String]) -> Result<(), String> {
    remove_reporting(scope, files).map(|_| ())
}

/// [`remove`] with the trash entry and any warnings, for callers that offer
/// an undo affordance.
pub fn remove_reporting(scope: Scope, files: &[String]) -> Result<RemoveReport, String> {
    // Ask before disabling, or the answer is always "no".
    let primary = files.first().cloned().unwrap_or_default();
    let (was_enabled, was_active) = if primary.is_empty() {
        (false, false)
    } else {
        (is_enabled(scope, &primary), is_active(scope, &primary))
    };

    if !primary.is_empty() {
        let _ = systemctl(scope, &["disable", "--now", &primary]);
    }
    for f in files {
        let _ = systemctl(scope, &["stop", f]);
    }

    let report = stash_and_delete(
        scope,
        &unit_dir(scope),
        &Trash::for_scope(scope),
        files,
        was_enabled,
        was_active,
    )?;

    daemon_reload(scope)?;
    let mut args = vec!["reset-failed"];
    args.extend(files.iter().map(String::as_str));
    let _ = systemctl(scope, &args);
    Ok(report)
}

/// The half of [`remove_reporting`] that touches files, with the unit
/// directory and the trash passed in so it can be exercised against a
/// temporary directory instead of the real system paths.
fn stash_and_delete(
    scope: Scope,
    dir: &std::path::Path,
    trash: &Trash,
    files: &[String],
    was_enabled: bool,
    was_active: bool,
) -> Result<RemoveReport, String> {
    let paths: Vec<PathBuf> = files.iter().map(|f| dir.join(f)).collect();
    let mut report = RemoveReport::default();

    let present: Vec<PathBuf> = paths.iter().filter(|p| p.exists()).cloned().collect();
    if !present.is_empty() {
        let req = StashRequest {
            scope,
            unit: files.first().cloned().unwrap_or_default(),
            files: present,
            was_enabled,
            was_active,
        };
        match trash.stash(&req) {
            Ok(entry) => report.trashed = Some(entry),
            Err(e) => {
                return Err(format!(
                    "{} was stopped and disabled but NOT deleted: its files could \
                     not be moved to the trash at {}, and notcron will not delete \
                     what it cannot undo.\n\n{e}",
                    files.first().map(String::as_str).unwrap_or("the unit"),
                    trash.root().display()
                ))
            }
        }
        // Keep the trash from growing without bound. A failure here is
        // cosmetic: the removal already succeeded.
        if let Err(e) = trash.prune(PrunePolicy::DEFAULT) {
            report.warnings.push(format!("pruning the trash: {e}"));
        }
    }

    // The stash moved the files out, so this only catches anything it left
    // behind: a path that reappeared, or a unit whose files were already gone.
    for p in &paths {
        remove_file(scope, p)?;
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// One row in the unit list.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The unit systemctl acts on, e.g. `notcron-backup.timer`.
    pub primary: String,
    /// Every file the entry owns, primary first.
    pub files: Vec<String>,
    pub scope: Scope,
    /// True when the notcron marker is present: only these may be edited.
    pub owned: bool,
    pub description: String,
    pub kind: &'static str,
    pub schedule: String,
    /// `None` when the files could not be modelled; the row is read-only.
    pub unit: Option<Unit>,
    pub active: String,
    pub enabled: String,
}

/// Suffix -> priority, so a timer outranks its service when grouping.
fn primary_rank(suffix: &str) -> u8 {
    match suffix {
        "timer" => 0,
        "automount" => 1,
        "service" => 2,
        "mount" => 3,
        _ => 9,
    }
}

/// Read and model every unit notcron understands in a scope.
///
/// `include_foreign` also returns units without the marker; they come back
/// with `owned == false` and must be treated as read-only.
pub fn list(scope: Scope, include_foreign: bool) -> Result<Vec<Entry>, String> {
    let dir = unit_dir(scope);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("reading {}: {e}", dir.display())),
    };

    // stem -> suffix -> file body
    let mut groups: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for ent in rd.flatten() {
        if !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some((stem, suffix)) = name.rsplit_once('.') else {
            continue;
        };
        if !SUFFIXES.contains(&suffix) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(ent.path()) else {
            continue; // unreadable (permissions, binary): not our business
        };
        groups
            .entry(stem.to_string())
            .or_default()
            .insert(suffix.to_string(), body);
    }

    let mut entries = Vec::new();
    for (stem, files) in groups {
        let mut suffixes: Vec<&String> = files.keys().collect();
        suffixes.sort_by_key(|s| primary_rank(s));
        let Some(primary_suffix) = suffixes.first().map(|s| s.to_string()) else {
            continue;
        };
        // A .service next to a .timer, or a .mount next to an .automount, is
        // a companion; the group is one entry either way.
        let mut ordered: Vec<String> = vec![primary_suffix.clone()];
        for s in &suffixes[1..] {
            ordered.push((*s).clone());
        }
        let sources: Vec<SourceFile> = ordered
            .iter()
            .map(|s| SourceFile {
                name: format!("{stem}.{s}"),
                body: files[s].clone(),
            })
            .collect();

        let (unit, owned) = match parse::parse(scope, &sources) {
            Ok((u, owned)) => (Some(u), owned),
            Err(_) => (
                None,
                sources.iter().any(|f| {
                    f.body
                        .lines()
                        .any(|l| l.trim() == crate::unit::model::MARKER)
                }),
            ),
        };
        if !owned && !include_foreign {
            continue;
        }

        let (description, kind, schedule) = match &unit {
            Some(u) => (
                u.description.clone(),
                u.body.kind_label(),
                match &u.body {
                    crate::unit::model::Body::Timer(t) => t.schedule.summary(),
                    crate::unit::model::Body::Mount(m) => m.what.clone(),
                    crate::unit::model::Body::Service(_) => String::new(),
                },
            ),
            None => (String::new(), "unknown", String::new()),
        };

        entries.push(Entry {
            primary: format!("{stem}.{primary_suffix}"),
            files: sources.iter().map(|f| f.name.clone()).collect(),
            scope,
            owned,
            description,
            kind,
            schedule,
            unit,
            active: String::new(),
            enabled: String::new(),
        });
    }

    fill_states(scope, &mut entries);
    Ok(entries)
}

/// Fill in ActiveState/UnitFileState for every entry with one systemctl call.
fn fill_states(scope: Scope, entries: &mut [Entry]) {
    if entries.is_empty() {
        return;
    }
    let names: Vec<&str> = entries.iter().map(|e| e.primary.as_str()).collect();
    let mut args = vec!["show", "--property=Id,ActiveState,UnitFileState"];
    args.extend(&names);
    let Ok(out) = systemctl(scope, &args) else {
        return;
    };
    let mut by_id: BTreeMap<String, (String, String)> = BTreeMap::new();
    for block in out.split("\n\n") {
        let (mut id, mut active, mut enabled) = (None, String::new(), String::new());
        for line in block.lines() {
            match line.split_once('=') {
                Some(("Id", v)) => id = Some(v.to_string()),
                Some(("ActiveState", v)) => active = v.to_string(),
                Some(("UnitFileState", v)) => enabled = v.to_string(),
                _ => {}
            }
        }
        if let Some(id) = id {
            by_id.insert(id, (active, enabled));
        }
    }
    for e in entries.iter_mut() {
        if let Some((a, en)) = by_id.get(&e.primary) {
            e.active = a.clone();
            e.enabled = en.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_unit_dir_follows_xdg_config_home() {
        // std::env::set_var is process-global; keep this test self-contained
        // by only asserting on the shape of the default path.
        let d = unit_dir(Scope::User);
        assert!(d.ends_with("systemd/user"), "{}", d.display());
        assert_eq!(
            unit_dir(Scope::System),
            PathBuf::from("/etc/systemd/system")
        );
    }

    // -----------------------------------------------------------------
    // Removal stashes instead of deleting
    //
    // These drive `stash_and_delete`, the half of `remove` that touches
    // files, against a temporary unit directory and a temporary trash. The
    // systemctl calls `remove` wraps around it are best-effort and cannot be
    // staged in a unit test; what must not regress is that no unit file is
    // ever deleted without a restorable copy existing first.
    // -----------------------------------------------------------------

    use crate::trash::Trash;
    use tempfile::TempDir;

    const TIMER: &str = "notcron-e2e.timer";
    const SERVICE: &str = "notcron-e2e.service";

    /// A unit directory holding two files with distinctive bodies.
    fn staged(dir: &std::path::Path) -> (String, String) {
        std::fs::create_dir_all(dir).unwrap();
        let timer = "[Unit]\nDescription=e2e\n\n[Timer]\nOnCalendar=*-*-* 03:00:00\n";
        let service = "[Unit]\nDescription=e2e\n\n[Service]\nExecStart=/bin/true\n";
        std::fs::write(dir.join(TIMER), timer).unwrap();
        std::fs::write(dir.join(SERVICE), service).unwrap();
        (timer.into(), service.into())
    }

    fn names() -> Vec<String> {
        vec![TIMER.to_string(), SERVICE.to_string()]
    }

    #[test]
    fn removal_stashes_the_files_instead_of_deleting_them() {
        let tmp = TempDir::new().unwrap();
        let units = tmp.path().join("units");
        let trash = Trash::at(tmp.path().join("trash"));
        staged(&units);

        let report = stash_and_delete(Scope::User, &units, &trash, &names(), true, false).unwrap();

        // Gone from the unit directory...
        assert!(!units.join(TIMER).exists());
        assert!(!units.join(SERVICE).exists());
        // ...but recoverable.
        let entry = report.trashed.expect("removal should have stashed");
        assert_eq!(entry.unit, TIMER);
        assert_eq!(entry.files.len(), 2);
        assert!(entry.was_enabled);
        assert!(!entry.was_active);
        // The metadata is on disk, not just in the returned value.
        assert_eq!(trash.list().unwrap()[0].id, entry.id);
    }

    #[test]
    fn remove_then_restore_returns_the_original_bytes() {
        let tmp = TempDir::new().unwrap();
        let units = tmp.path().join("units");
        let trash = Trash::at(tmp.path().join("trash"));
        let (timer_body, service_body) = staged(&units);

        let report = stash_and_delete(Scope::User, &units, &trash, &names(), true, true).unwrap();
        let id = report.trashed.unwrap().id;

        let restored = trash.restore(&id, false).unwrap();
        assert_eq!(restored.restored.len(), 2);
        // Restore reports what it saw at removal time, so a caller can offer
        // to re-enable and restart.
        assert!(restored.was_enabled);
        assert!(restored.was_active);

        assert_eq!(
            std::fs::read_to_string(units.join(TIMER)).unwrap(),
            timer_body
        );
        assert_eq!(
            std::fs::read_to_string(units.join(SERVICE)).unwrap(),
            service_body
        );
        // A restored entry leaves nothing behind in the trash.
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn a_failed_stash_aborts_the_removal_and_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let units = tmp.path().join("units");
        staged(&units);

        // A trash root that cannot be created: the path runs *through* a
        // regular file, so mkdir -p fails with ENOTDIR.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let trash = Trash::at(blocker.join("trash"));

        let err = stash_and_delete(Scope::User, &units, &trash, &names(), false, false)
            .expect_err("a trash that cannot be written must abort the removal");
        assert!(err.contains("NOT deleted"), "{err}");

        // The whole point: refusing to back up means refusing to delete.
        assert!(units.join(TIMER).exists());
        assert!(units.join(SERVICE).exists());
    }

    #[test]
    fn a_unit_whose_files_are_already_gone_removes_without_a_trash_entry() {
        let tmp = TempDir::new().unwrap();
        let units = tmp.path().join("units");
        std::fs::create_dir_all(&units).unwrap();
        let trash = Trash::at(tmp.path().join("trash"));

        // Nothing on disk is not a failure: there is nothing to lose, so the
        // removal proceeds and simply has no undo to offer.
        let report = stash_and_delete(Scope::User, &units, &trash, &names(), false, false).unwrap();
        assert!(report.trashed.is_none());
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn removal_prunes_the_trash_to_the_default_retention() {
        let tmp = TempDir::new().unwrap();
        let units = tmp.path().join("units");
        let trash = Trash::at(tmp.path().join("trash"));

        // More removals than PrunePolicy::DEFAULT keeps.
        for _ in 0..55 {
            staged(&units);
            stash_and_delete(Scope::User, &units, &trash, &names(), false, false).unwrap();
        }
        assert_eq!(trash.list().unwrap().len(), 50);
    }

    #[test]
    fn primary_rank_puts_the_timer_first() {
        assert!(primary_rank("timer") < primary_rank("service"));
        assert!(primary_rank("automount") < primary_rank("mount"));
    }

    #[test]
    fn calendar_specs_notcron_emits_are_accepted_by_systemd() {
        if !has_analyze() {
            eprintln!("skipping: systemd-analyze not available");
            return;
        }
        let exprs = [
            "* * * * *",
            "0 3 * * *",
            "*/15 * * * *",
            "0 9-17 * * mon-fri",
            "30 2 1,15 * *",
            "0 0 1 jan,jul *",
            "15 10 13 * fri",
            "@weekly",
            "@monthly",
            "0 0 1-9/2 * *",
            "0 9 * * fri-mon",
            "0 0 */10 * *",
            "0-59 0 1 1 *",
            "0 0 1 mar-may *",
            "0 8-18/4 * * *",
            "0 0 20/5 * *",
        ];
        for e in exprs {
            let crate::cron::Translation::Calendar(specs) = crate::cron::to_calendar(e).unwrap()
            else {
                panic!("{e} should be a calendar");
            };
            for s in specs {
                check_calendar(&s)
                    .unwrap_or_else(|err| panic!("systemd rejected '{s}' from '{e}': {err}"));
            }
        }
    }

    #[test]
    fn bogus_calendar_specs_are_rejected() {
        if !has_analyze() {
            return;
        }
        assert!(check_calendar("not a calendar spec at all").is_err());
    }

    #[test]
    fn timespans_are_checked() {
        if !has_analyze() {
            return;
        }
        assert!(check_timespan("15min").is_ok());
        assert!(check_timespan("90s").is_ok());
        assert!(check_timespan("wibble").is_err());
    }

    const SAMPLE: &str = "\
Normalized form: Mon *-*-* 03:00:00
    Next elapse: Mon 2026-08-17 03:00:00 CEST
       (in UTC): Mon 2026-08-17 01:00:00 UTC
       From now: 1 day 23h left
   Iteration #2: Mon 2026-08-24 03:00:00 CEST
       (in UTC): Mon 2026-08-24 01:00:00 UTC
       From now: 1 week 1 day left
   Iteration #3: Mon 2026-08-31 03:00:00 CEST
       (in UTC): Mon 2026-08-31 01:00:00 UTC
       From now: 2 weeks 1 day left
";

    #[test]
    fn iteration_output_is_parsed() {
        let runs = parse_iterations(SAMPLE);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].local, "Mon 2026-08-17 03:00:00 CEST");
        assert_eq!(runs[0].utc, "Mon 2026-08-17 01:00:00 UTC");
        assert_eq!(runs[0].from_now, "1 day 23h left");
        assert_eq!(runs[2].local, "Mon 2026-08-31 03:00:00 CEST");
        // The "Normalized form:" line must not be mistaken for an elapse.
        assert!(!runs.iter().any(|r| r.local.contains('*')));
    }

    #[test]
    fn sort_key_is_the_utc_instant() {
        let runs = parse_iterations(SAMPLE);
        assert_eq!(runs[0].sort_key(), "2026-08-17 01:00:00");
        // Falls back to the local line when no UTC form was printed.
        let local_only = NextRun {
            local: "Mon 2026-08-17 03:00:00 CEST".into(),
            utc: String::new(),
            from_now: String::new(),
        };
        assert_eq!(local_only.sort_key(), "2026-08-17 03:00:00");
        let mut keys: Vec<&str> = runs.iter().map(|r| r.sort_key()).collect();
        let sorted = {
            let mut k = keys.clone();
            k.sort_unstable();
            k
        };
        keys.dedup();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn a_spec_that_never_fires_yields_no_runs() {
        let text = "Normalized form: 2000-01-01 00:00:00\n    Next elapse: never\n";
        assert!(parse_iterations(text).is_empty());
    }

    #[test]
    fn stray_lines_are_ignored() {
        assert!(parse_iterations("").is_empty());
        assert!(parse_iterations("no colons here\nOriginal form: whatever\n").is_empty());
        // Continuation lines with no elapse before them must not panic.
        assert!(parse_iterations("       (in UTC): Mon 2026-08-17 01:00:00 UTC\n").is_empty());
    }

    #[test]
    fn next_runs_returns_the_requested_count() {
        if !has_analyze() {
            eprintln!("skipping: systemd-analyze not available");
            return;
        }
        let runs = next_runs("*-*-* 03:00:00", 5).unwrap();
        assert_eq!(runs.len(), 5);
        assert!(runs.iter().all(|r| r.local.contains("03:00:00")));
        // Strictly increasing.
        for w in runs.windows(2) {
            assert!(w[0].sort_key() < w[1].sort_key(), "{:?}", w);
        }
    }

    #[test]
    fn next_runs_reports_an_invalid_spec() {
        if !has_analyze() {
            return;
        }
        match next_runs("not a calendar spec at all", 3) {
            Err(PreviewError::Invalid(m)) => assert!(!m.is_empty()),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn multiple_calendars_are_merged_and_resorted() {
        if !has_analyze() {
            return;
        }
        let specs = vec!["*-*-* 03:00:00".to_string(), "*-*-* 15:00:00".to_string()];
        let runs = next_runs_multi(&specs, 4).unwrap();
        assert_eq!(runs.len(), 4);
        for w in runs.windows(2) {
            assert!(w[0].sort_key() <= w[1].sort_key(), "{:?}", w);
        }
        // The union really is both specs, not just the first.
        assert!(runs.iter().any(|r| r.local.contains("03:00:00")));
        assert!(runs.iter().any(|r| r.local.contains("15:00:00")));
    }

    #[test]
    fn identical_specs_are_deduplicated() {
        if !has_analyze() {
            return;
        }
        let specs = vec!["*-*-* 03:00:00".to_string(), "*-*-* 03:00:00".to_string()];
        let runs = next_runs_multi(&specs, 3).unwrap();
        assert_eq!(runs.len(), 3);
        for w in runs.windows(2) {
            assert_ne!(w[0].local, w[1].local);
        }
    }

    #[test]
    fn empty_and_blank_specs_preview_as_nothing() {
        assert!(next_runs_multi(&[], 5).unwrap().is_empty());
        assert!(next_runs_multi(&["  ".to_string()], 5).unwrap().is_empty());
    }

    #[test]
    fn one_bad_spec_fails_the_whole_preview() {
        if !has_analyze() {
            return;
        }
        let specs = vec!["*-*-* 03:00:00".to_string(), "nonsense!!".to_string()];
        assert!(matches!(
            next_runs_multi(&specs, 3),
            Err(PreviewError::Invalid(_))
        ));
    }
}
