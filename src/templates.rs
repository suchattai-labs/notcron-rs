//! Canned units and the smart defaults the builder offers.
//!
//! A template is a fully populated [`Unit`], not a skeleton: the schedule,
//! description and `[Service]` knobs are all set to something that would
//! actually be right for that kind of job, so the user edits paths rather
//! than inventing a whole unit. The `ExecStart=` lines are deliberately
//! obvious placeholders — real paths, real flags, clearly meant to be
//! replaced.

use crate::unit::escape;
use crate::unit::model::{Body, MountUnit, Schedule, Scope, ServiceType, Unit};

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

/// Identifies a template. Stable strings, so a caller can persist a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateId {
    Backup,
    Sync,
    CacheWarmer,
}

impl TemplateId {
    pub const ALL: [TemplateId; 3] = [
        TemplateId::Backup,
        TemplateId::Sync,
        TemplateId::CacheWarmer,
    ];

    /// Stable machine-readable id.
    pub fn key(self) -> &'static str {
        match self {
            TemplateId::Backup => "backup",
            TemplateId::Sync => "sync",
            TemplateId::CacheWarmer => "cache-warmer",
        }
    }

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            TemplateId::Backup => "Nightly backup",
            TemplateId::Sync => "Periodic sync",
            TemplateId::CacheWarmer => "Cache warmer",
        }
    }

    /// One-line explanation of what the template sets up.
    pub fn detail(self) -> &'static str {
        match self {
            TemplateId::Backup => "rsync to a backup target at 02:30, catching up after downtime",
            TemplateId::Sync => "rsync to a remote every 15 minutes, jittered",
            TemplateId::CacheWarmer => "hit an endpoint every 5 minutes to keep a cache hot",
        }
    }

    pub fn parse(key: &str) -> Option<TemplateId> {
        TemplateId::ALL.into_iter().find(|t| t.key() == key)
    }

    /// Build the unit for this template.
    pub fn build(self, scope: Scope) -> Unit {
        match self {
            TemplateId::Backup => backup_job(scope),
            TemplateId::Sync => sync_job(scope),
            TemplateId::CacheWarmer => cache_warmer(scope),
        }
    }
}

/// Set the timer body of a freshly created unit.
fn timer(
    scope: Scope,
    name: &str,
    description: &str,
    f: impl FnOnce(&mut crate::unit::model::TimerJob),
) -> Unit {
    let mut u = Unit::new_timer(scope);
    u.name = name.to_string();
    u.description = description.to_string();
    if let Body::Timer(t) = &mut u.body {
        f(t);
    }
    u
}

/// A nightly rsync backup.
///
/// `Persistent=true` is the point of this one: a laptop that was asleep at
/// 02:30 runs the backup as soon as it comes back, which is what a person
/// means by "nightly backup". The randomized delay keeps a fleet of machines
/// from hitting the same NAS on the same second.
pub fn backup_job(scope: Scope) -> Unit {
    timer(scope, "backup", "Nightly backup", |t| {
        t.schedule = Schedule::Calendar(vec!["*-*-* 02:30:00".into()]);
        t.source = "nightly at 02:30".into();
        t.persistent = true;
        t.randomized_delay = Some("15min".into());
        t.service.service_type = ServiceType::Oneshot;
        t.service.exec_start = "/usr/bin/rsync -aHAX --delete /home/ /srv/backup/home/".into();
        // A backup that silently writes to an unmounted mount point is the
        // classic failure; fail loudly instead.
        t.service_manual = "# Refuse to run if the backup target is not mounted.\n\
                            ExecStartPre=/usr/bin/mountpoint -q /srv/backup\n\
                            Nice=10\n\
                            IOSchedulingClass=idle\n"
            .into();
    })
}

/// A frequent one-way sync to a remote.
///
/// Interval-driven rather than calendar-driven: "every 15 minutes from when
/// the machine came up" is the honest description, and it means no thundering
/// herd on the quarter hour. `Persistent=` does not apply to interval timers,
/// so it is off.
pub fn sync_job(scope: Scope) -> Unit {
    timer(scope, "sync", "Periodic sync to remote", |t| {
        t.schedule = Schedule::Every {
            every: "15min".into(),
            boot: "5min".into(),
        };
        t.source = "every 15 minutes".into();
        t.persistent = false;
        t.randomized_delay = Some("2min".into());
        t.service.service_type = ServiceType::Oneshot;
        t.service.exec_start =
            "/usr/bin/rsync -az --delete /srv/data/ user@remote:/srv/data/".into();
        t.service_manual = "# Do not let a stalled transfer overlap the next run.\n\
                            TimeoutStartSec=10min\n"
            .into();
    })
}

/// A cache warmer: cheap, frequent, and worthless if run late.
///
/// So: a short interval, no `Persistent=` catch-up (a warm-up for a time that
/// has passed is pointless), and a hard timeout.
pub fn cache_warmer(scope: Scope) -> Unit {
    timer(
        scope,
        "cache-warmer",
        "Keep the application cache warm",
        |t| {
            t.schedule = Schedule::Every {
                every: "5min".into(),
                boot: "1min".into(),
            };
            t.source = "every 5 minutes".into();
            t.persistent = false;
            t.service.service_type = ServiceType::Oneshot;
            t.service.exec_start =
                "/usr/bin/curl -fsS -o /dev/null --max-time 30 http://localhost:8080/warm".into();
            t.service_manual = "TimeoutStartSec=1min\n".into();
        },
    )
}

// ---------------------------------------------------------------------------
// Smart defaults
// ---------------------------------------------------------------------------

/// Suggest a `WorkingDirectory=` from the directory notcron was started in.
/// `None` when the cwd is gone or not absolute, in which case the field is
/// better left empty than filled with something systemd will reject.
pub fn suggest_working_directory() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    if !cwd.is_absolute() {
        return None;
    }
    Some(cwd.to_string_lossy().into_owned())
}

/// Turn one path component into something safe to put in a mount point,
/// reusing the unit-name slug rules. `None` if nothing usable survives.
fn mount_component(raw: &str) -> Option<String> {
    let slug = escape::slugify(raw);
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

/// Suggest a mount point (`Where=`) from a mount source (`What=`).
///
/// Handles the forms notcron's presets produce:
///
/// | `What=`                          | suggestion         |
/// |----------------------------------|--------------------|
/// | `//server/share`                 | `/mnt/share`       |
/// | `server:/export/data`            | `/mnt/data`        |
/// | `nfs://server/export/data`       | `/mnt/data`        |
/// | `/dev/sdb1`                      | `/mnt/sdb1`        |
/// | `/dev/disk/by-label/photos`      | `/mnt/photos`      |
/// | `/dev/disk/by-uuid/1234abcd-...` | `/mnt/disk-1234abcd` |
/// | `/srv/source` (bind)             | `/mnt/source`      |
///
/// Returns `None` when there is no meaningful last component to name the
/// mount point after.
pub fn suggest_where(what: &str) -> Option<String> {
    let what = what.trim().trim_end_matches('/');
    if what.is_empty() {
        return None;
    }
    // nfs://server/export/data and cifs://server/share
    let stripped = what.split_once("://").map(|(_, rest)| rest).unwrap_or(what);
    // server:/export/data -- a host prefix, not a Windows-style drive.
    let stripped = match stripped.split_once(":/") {
        Some((host, rest)) if !host.is_empty() && !host.starts_with('/') => rest,
        _ => stripped,
    };

    let last = stripped.rsplit('/').find(|c| !c.is_empty())?;

    // by-uuid/by-partuuid components are unreadable; abbreviate them.
    let parent = stripped
        .trim_end_matches(last)
        .trim_end_matches('/')
        .rsplit('/')
        .find(|c| !c.is_empty())
        .unwrap_or("");
    let name = if matches!(parent, "by-uuid" | "by-partuuid") {
        let short: String = last.chars().take(8).collect();
        mount_component(&format!("disk-{short}"))?
    } else {
        mount_component(last)?
    };
    Some(format!("/mnt/{name}"))
}

/// Suggest a unit name from a command line.
///
/// Delegates the "which token names this job" decision to the same
/// [`crate::cli::build_exec`] the `add` subcommand uses, so a job created in
/// the TUI and the same job created from the shell get the same name.
pub fn suggest_name(command_line: &str) -> String {
    let line = command_line.trim();
    if line.is_empty() {
        return String::new();
    }
    let (_, hint) = if escape::needs_shell(line) {
        crate::cli::build_exec(&[line.to_string()], true)
    } else {
        let args = crate::validate::split_exec(line);
        if args.is_empty() {
            return String::new();
        }
        crate::cli::build_exec(&args, false)
    };
    escape::slugify(&hint)
}

// ---------------------------------------------------------------------------
// Cloning
// ---------------------------------------------------------------------------

/// Pick a name like `foo-copy`, then `foo-copy-2`, that no existing unit has.
///
/// `existing` may be given with or without the `notcron-` prefix; both are
/// compared unprefixed, since that is the name the user actually types.
pub fn dedup_name(base: &str, existing: &[String]) -> String {
    let taken: Vec<&str> = existing
        .iter()
        .map(|e| crate::unit::model::unprefixed(e))
        .collect();
    let base = base.trim_end_matches('-');
    let stem = if base.is_empty() { "unit" } else { base };
    let mut candidate = format!("{stem}-copy");
    let mut n = 1;
    while taken.iter().any(|t| *t == candidate) {
        n += 1;
        candidate = format!("{stem}-copy-{n}");
    }
    candidate
}

/// The same, for the *mount point* of a mount unit: `/mnt/data` becomes
/// `/mnt/data-copy`, since a mount unit's name is dictated by `Where=`.
fn dedup_where(where_: &str, existing: &[String]) -> String {
    let trimmed = where_.trim_end_matches('/');
    let (parent, last) = match trimmed.rsplit_once('/') {
        Some((p, l)) if !l.is_empty() => (p, l),
        _ => return dedup_name(trimmed, existing),
    };
    let taken: Vec<&str> = existing.iter().map(|e| e.as_str()).collect();
    let mut n = 1;
    let mut candidate = format!("{parent}/{last}-copy");
    while taken.iter().any(|t| *t == candidate) {
        n += 1;
        candidate = format!("{parent}/{last}-copy-{n}");
    }
    candidate
}

/// "New from existing": a copy of `src` ready to be edited and installed
/// under a new identity.
///
/// The copy gets a fresh, non-colliding name (`existing` is the list of names
/// already in use — unit names for timers and services, mount points for
/// mounts) and a description marked as a copy. Nothing about the original's
/// installed state comes along: [`Unit`] models only the file contents, so
/// the caller must not carry over the enabled/active columns it read from
/// [`crate::systemd::Entry`] — the clone is not installed until it is written.
pub fn clone_unit(src: &Unit, existing: &[String]) -> Unit {
    let mut copy = src.clone();
    match &mut copy.body {
        Body::Mount(m) => {
            m.where_ = dedup_where(&m.where_, existing);
        }
        _ => {
            copy.name = dedup_name(&src.name, existing);
        }
    }
    copy.description = if src.description.trim().is_empty() {
        "copy".into()
    } else {
        format!("{} (copy)", src.description.trim())
    };
    copy
}

/// A mount unit prefilled from a `What=`, with the preset's defaults and a
/// suggested mount point. Convenience for the builder's "new mount" flow.
pub fn mount_from_what(preset: crate::unit::model::MountPreset, what: &str) -> Unit {
    let m = MountUnit {
        preset,
        what: what.trim().to_string(),
        where_: suggest_where(what).unwrap_or_default(),
        fstype: preset.fstype().into(),
        options: preset.options().into(),
        ..MountUnit::default()
    };
    let mut u = Unit::new_mount();
    u.description = if m.where_.is_empty() {
        String::new()
    } else {
        format!("Mount {} at {}", m.what, m.where_)
    };
    u.body = Body::Mount(Box::new(m));
    u
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::generate;
    use crate::unit::model::ServiceOpts;

    fn svc(u: &Unit) -> &ServiceOpts {
        match &u.body {
            Body::Timer(t) => &t.service,
            Body::Service(s) => &s.service,
            Body::Mount(_) => panic!("not a service"),
        }
    }

    #[test]
    fn every_template_is_valid_and_renders() {
        for id in TemplateId::ALL {
            let u = id.build(Scope::User);
            u.validate()
                .unwrap_or_else(|e| panic!("{} invalid: {e}", id.key()));
            assert!(!u.name.is_empty());
            assert!(!u.description.is_empty());
            assert!(svc(&u).exec_start.starts_with('/'), "{}", id.key());
            let files = generate::render(&u);
            assert_eq!(files.len(), 2, "{}", id.key());
            assert!(files[0].body.contains("[Timer]"));
            assert!(files[1].body.contains("ExecStart="));
        }
    }

    #[test]
    fn template_ids_round_trip() {
        for id in TemplateId::ALL {
            assert_eq!(TemplateId::parse(id.key()), Some(id));
            assert!(!id.label().is_empty());
            assert!(!id.detail().is_empty());
        }
        assert_eq!(TemplateId::parse("nope"), None);
    }

    #[test]
    fn backup_catches_up_and_the_cache_warmer_does_not() {
        let b = backup_job(Scope::System);
        let Body::Timer(t) = &b.body else {
            panic!("timer")
        };
        assert!(t.persistent);
        assert_eq!(t.schedule.calendars(), ["*-*-* 02:30:00"]);
        assert!(t.randomized_delay.is_some());
        assert!(t.service_manual.contains("mountpoint"));

        let c = cache_warmer(Scope::User);
        let Body::Timer(t) = &c.body else {
            panic!("timer")
        };
        assert!(!t.persistent);
        assert!(matches!(&t.schedule, Schedule::Every { every, .. } if every == "5min"));
    }

    #[test]
    fn sync_runs_on_an_interval() {
        let Body::Timer(t) = &sync_job(Scope::User).body else {
            panic!("timer")
        };
        assert!(matches!(
            &t.schedule,
            Schedule::Every { every, boot } if every == "15min" && boot == "5min"
        ));
        assert!(!t.persistent);
    }

    #[test]
    fn templates_honour_the_requested_scope() {
        assert_eq!(backup_job(Scope::System).scope, Scope::System);
        assert_eq!(backup_job(Scope::User).scope, Scope::User);
    }

    #[test]
    fn suggests_the_current_directory() {
        let wd = suggest_working_directory().expect("a cwd");
        assert!(wd.starts_with('/'), "{wd}");
        assert_eq!(wd, std::env::current_dir().unwrap().to_string_lossy());
    }

    #[test]
    fn suggests_mount_points_for_every_source_form() {
        for (what, want) in [
            ("//server/share", "/mnt/share"),
            ("//server/share/", "/mnt/share"),
            ("//server/Team Share", "/mnt/team-share"),
            ("server:/export/data", "/mnt/data"),
            ("nfs://server/export/data", "/mnt/data"),
            ("/dev/sdb1", "/mnt/sdb1"),
            ("/dev/disk/by-label/photos", "/mnt/photos"),
            ("/dev/disk/by-uuid/1234abcd-5678-90ef", "/mnt/disk-1234abcd"),
            ("/srv/source", "/mnt/source"),
            ("/srv/media_files", "/mnt/media-files"),
        ] {
            assert_eq!(suggest_where(what).as_deref(), Some(want), "what {what}");
        }
    }

    #[test]
    fn unsuggestable_sources_return_none() {
        assert_eq!(suggest_where(""), None);
        assert_eq!(suggest_where("   "), None);
        assert_eq!(suggest_where("/"), None);
        assert_eq!(suggest_where("///"), None);
        assert_eq!(suggest_where("!!!"), None);
    }

    #[test]
    fn suggested_mount_points_are_escapable() {
        for what in ["//server/Team Share", "/dev/disk/by-uuid/1234abcd-5678"] {
            let w = suggest_where(what).unwrap();
            escape::escape_path(&w).unwrap_or_else(|e| panic!("{w}: {e}"));
        }
    }

    #[test]
    fn suggests_names_from_command_lines() {
        assert_eq!(suggest_name("/usr/local/bin/backup.sh --full"), "backup-sh");
        assert_eq!(suggest_name("rsync -a /a /b"), "rsync");
        assert_eq!(suggest_name("/usr/bin/curl http://x/y"), "curl");
        // A shell pipeline is named after the first program in it.
        assert_eq!(suggest_name("df -h | mail -s disk root"), "df");
        assert_eq!(suggest_name(""), "");
        assert_eq!(suggest_name("   "), "");
    }

    #[test]
    fn suggested_names_are_valid_unit_names() {
        for line in [
            "/usr/local/bin/Backup Script.sh",
            "echo hi > /tmp/x",
            "/opt/app/bin/run --flag=1",
        ] {
            let n = suggest_name(line);
            assert!(!n.is_empty(), "{line}");
            crate::unit::model::validate_name(&n).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
    }

    #[test]
    fn dedup_appends_copy_then_numbers() {
        assert_eq!(dedup_name("backup", &[]), "backup-copy");
        assert_eq!(
            dedup_name("backup", &["backup".into(), "backup-copy".into()]),
            "backup-copy-2"
        );
        assert_eq!(
            dedup_name(
                "backup",
                &[
                    "backup-copy".into(),
                    "backup-copy-2".into(),
                    "backup-copy-3".into()
                ]
            ),
            "backup-copy-4"
        );
        // Prefixed names are compared unprefixed.
        assert_eq!(
            dedup_name("backup", &["notcron-backup-copy".into()]),
            "backup-copy-2"
        );
        assert_eq!(dedup_name("", &[]), "unit-copy");
    }

    #[test]
    fn cloning_a_timer_renames_and_marks_the_description() {
        let mut src = backup_job(Scope::System);
        src.description = "Nightly backup".into();
        let c = clone_unit(&src, &["backup".into(), "backup-copy".into()]);
        assert_eq!(c.name, "backup-copy-2");
        assert_eq!(c.description, "Nightly backup (copy)");
        assert_eq!(c.scope, src.scope);
        c.validate().unwrap();
        // Everything else is carried over verbatim.
        assert_eq!(svc(&c).exec_start, svc(&src).exec_start);
        match (&c.body, &src.body) {
            (Body::Timer(a), Body::Timer(b)) => assert_eq!(a.schedule, b.schedule),
            _ => panic!("timer"),
        }
        // The original is untouched.
        assert_eq!(src.name, "backup");
    }

    #[test]
    fn cloning_a_mount_renames_the_mount_point() {
        let mut src = Unit::new_mount();
        if let Body::Mount(m) = &mut src.body {
            m.what = "//server/share".into();
            m.where_ = "/mnt/share".into();
        }
        let c = clone_unit(&src, &["/mnt/share".into(), "/mnt/share-copy".into()]);
        let Body::Mount(m) = &c.body else {
            panic!("mount")
        };
        assert_eq!(m.where_, "/mnt/share-copy-2");
        assert_eq!(m.what, "//server/share");
        assert_eq!(c.description, "copy");
        c.validate().unwrap();
        assert_eq!(c.stem().unwrap(), "mnt-share\\x2dcopy\\x2d2");
    }

    #[test]
    fn mount_from_what_fills_in_the_preset() {
        let u = mount_from_what(crate::unit::model::MountPreset::Cifs, "//server/share");
        let Body::Mount(m) = &u.body else {
            panic!("mount")
        };
        assert_eq!(m.where_, "/mnt/share");
        assert_eq!(m.fstype, "cifs");
        assert!(m.options.contains("_netdev"));
        assert_eq!(u.description, "Mount //server/share at /mnt/share");
        u.validate().unwrap();

        // An unsuggestable source leaves Where= empty rather than guessing.
        let bad = mount_from_what(crate::unit::model::MountPreset::Block, "!!!");
        let Body::Mount(m) = &bad.body else {
            panic!("mount")
        };
        assert!(m.where_.is_empty());
        assert!(bad.validate().is_err());
    }
}
