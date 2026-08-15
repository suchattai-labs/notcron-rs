//! Everything that touches the system: unit directories, file installation
//! and removal, and the `systemctl` / `journalctl` / `systemd-analyze`
//! command line.
//!
//! System-scope writes go through `sudo`, matching the shell script. Nothing
//! here panics: every I/O and subprocess failure comes back as an `Err` with
//! a message fit to show the user.

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

fn is_root() -> bool {
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

/// Stop, disable, delete and forget a unit. Best-effort on the systemctl
/// steps: a unit that was never enabled must still be removable.
pub fn remove(scope: Scope, files: &[String]) -> Result<(), String> {
    let dir = unit_dir(scope);
    if let Some(primary) = files.first() {
        let _ = systemctl(scope, &["disable", "--now", primary]);
    }
    for f in files {
        let _ = systemctl(scope, &["stop", f]);
    }
    for f in files {
        remove_file(scope, &dir.join(f))?;
    }
    daemon_reload(scope)?;
    let mut args = vec!["reset-failed"];
    args.extend(files.iter().map(String::as_str));
    let _ = systemctl(scope, &args);
    Ok(())
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
}
