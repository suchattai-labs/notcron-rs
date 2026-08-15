//! Reading unit files back into the model.
//!
//! Every directive the model understands is lifted into a typed field.
//! Everything else -- and anything already inside a `notcron:manual` block --
//! is preserved verbatim in the unit's manual text, so opening a unit in the
//! builder and saving it again never silently drops a directive.

use super::escape;
use super::model::{
    Body, MountPreset, MountUnit, RestartPolicy, Schedule, Scope, ServiceOpts, ServiceType,
    StandaloneService, TimerJob, Unit, MANUAL_HEADER, MARKER, X_MARKER,
};

/// One unit file, decomposed.
#[derive(Debug, Default)]
struct Scanned {
    /// `# schedule: ...` and friends, in order, without the `# `.
    comments: Vec<String>,
    /// `(section, key, value)` in file order.
    entries: Vec<(String, String, String)>,
    /// Everything after the manual header, verbatim.
    manual: String,
    owned: bool,
}

fn scan(body: &str) -> Scanned {
    let mut s = Scanned::default();
    let mut section = String::new();
    let mut in_manual = false;
    let mut manual_lines: Vec<&str> = Vec::new();

    for line in body.lines() {
        if in_manual {
            manual_lines.push(line);
            continue;
        }
        let t = line.trim();
        if t == MANUAL_HEADER.trim() || t.starts_with("# --- notcron:manual") {
            in_manual = true;
            continue;
        }
        if t.is_empty() {
            continue;
        }
        if t == MARKER {
            s.owned = true;
            continue;
        }
        if let Some(rest) = t.strip_prefix('#').or_else(|| t.strip_prefix(';')) {
            s.comments.push(rest.trim().to_string());
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') {
            section = t[1..t.len() - 1].to_string();
            continue;
        }
        if t == X_MARKER {
            s.owned = true;
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            s.entries
                .push((section.clone(), k.trim().to_string(), v.trim().to_string()));
        }
    }
    s.manual = manual_lines.join("\n").trim_end().to_string();
    s
}

/// Collects directives the model does not model, re-emitting their section
/// headers so the result is a valid unit fragment.
#[derive(Default)]
struct Leftovers {
    text: String,
    last_section: Option<String>,
}

impl Leftovers {
    fn push(&mut self, section: &str, key: &str, value: &str) {
        if self.last_section.as_deref() != Some(section) {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str(&format!("[{section}]\n"));
            self.last_section = Some(section.to_string());
        }
        self.text.push_str(&format!("{key}={value}\n"));
    }

    /// Merge with any pre-existing manual block from the same file.
    fn finish(self, existing: &str) -> String {
        let a = self.text.trim_end();
        let b = existing.trim_end();
        match (a.is_empty(), b.is_empty()) {
            (true, true) => String::new(),
            (true, false) => b.to_string(),
            (false, true) => a.to_string(),
            (false, false) => format!("{b}\n{a}"),
        }
    }
}

/// Pull the `[Service]` knobs out, returning the directives left over.
fn take_service(entries: &[(String, String, String)], svc: &mut ServiceOpts, left: &mut Leftovers) {
    for (sec, k, v) in entries {
        if sec != "Service" {
            continue;
        }
        match k.as_str() {
            "Type" => match ServiceType::parse(v) {
                Some(t) => svc.service_type = t,
                None => left.push(sec, k, v),
            },
            "ExecStart" => svc.exec_start = v.clone(),
            "ExecStartPre" => svc.exec_start_pre = Some(v.clone()),
            "ExecStopPost" => svc.exec_stop_post = Some(v.clone()),
            "WorkingDirectory" => svc.working_directory = Some(v.clone()),
            "User" => svc.run_as = Some(v.clone()),
            "Group" => svc.group = Some(v.clone()),
            "Environment" => svc.environment.push(v.clone()),
            "Restart" => match RestartPolicy::parse(v) {
                Some(r) => svc.restart = r,
                None => left.push(sec, k, v),
            },
            "RestartSec" => svc.restart_sec = Some(v.clone()),
            // Always re-emitted by the generator; not worth modelling.
            "StandardOutput" | "StandardError" | "SyslogIdentifier"
                if matches!(v.as_str(), "journal") || k == "SyslogIdentifier" => {}
            _ => left.push(sec, k, v),
        }
    }
}

fn first(entries: &[(String, String, String)], sec: &str, key: &str) -> Option<String> {
    entries
        .iter()
        .find(|(s, k, _)| s == sec && k == key)
        .map(|(_, _, v)| v.clone())
}

/// A unit read off disk, with the files it came from.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub name: String,
    pub body: String,
}

/// Parse a unit from its files. The primary file (`.timer`, `.automount` or
/// `.service`/`.mount`) must come first, matching [`Unit::filenames`].
///
/// Returns an error only for input that is not a unit at all; unrecognised
/// directives are preserved rather than rejected.
pub fn parse(scope: Scope, files: &[SourceFile]) -> Result<(Unit, bool), String> {
    let primary = files.first().ok_or("no unit files given")?;
    let stem = primary
        .name
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .ok_or_else(|| format!("'{}' has no unit suffix", primary.name))?;

    let scans: Vec<Scanned> = files.iter().map(|f| scan(&f.body)).collect();
    let owned = scans.iter().any(|s| s.owned);

    let unit = if primary.name.ends_with(".timer") {
        parse_timer(scope, &stem, &scans)?
    } else if primary.name.ends_with(".automount") || primary.name.ends_with(".mount") {
        parse_mount(scope, &stem, &scans, primary.name.ends_with(".automount"))
    } else if primary.name.ends_with(".service") {
        parse_service(scope, &stem, &scans)
    } else {
        return Err(format!("unsupported unit type '{}'", primary.name));
    };
    Ok((unit, owned))
}

fn description(s: &Scanned, strip_suffix: &str) -> String {
    let d = first(&s.entries, "Unit", "Description").unwrap_or_default();
    d.strip_suffix(strip_suffix).unwrap_or(&d).to_string()
}

fn parse_timer(scope: Scope, stem: &str, scans: &[Scanned]) -> Result<Unit, String> {
    let timer = &scans[0];
    let empty = Scanned::default();
    let service = scans.get(1).unwrap_or(&empty);

    let mut t = TimerJob {
        persistent: false,
        ..TimerJob::default()
    };
    let mut tleft = Leftovers::default();

    let mut calendars: Vec<String> = Vec::new();
    let mut on_boot: Option<String> = None;
    let mut on_active: Option<String> = None;
    for (sec, k, v) in &timer.entries {
        match (sec.as_str(), k.as_str()) {
            ("Timer", "OnCalendar") => calendars.push(v.clone()),
            ("Timer", "OnBootSec") => on_boot = Some(v.clone()),
            ("Timer", "OnUnitActiveSec") => on_active = Some(v.clone()),
            ("Timer", "Persistent") => t.persistent = matches!(v.as_str(), "true" | "yes" | "1"),
            ("Timer", "RandomizedDelaySec") => t.randomized_delay = Some(v.clone()),
            // Emitted unconditionally by the generator.
            ("Timer", "AccuracySec") | ("Timer", "Unit") => {}
            ("Install", "WantedBy") if v == "timers.target" => {}
            ("Unit", "Description") => {}
            _ => tleft.push(sec, k, v),
        }
    }

    t.schedule = if !calendars.is_empty() {
        Schedule::Calendar(calendars)
    } else if let (Some(boot), Some(every)) = (on_boot.clone(), on_active) {
        Schedule::Every { every, boot }
    } else if let Some(boot) = on_boot {
        Schedule::Boot { boot }
    } else {
        return Err(format!("{stem}.timer has no schedule directives"));
    };
    t.timer_manual = tleft.finish(&timer.manual);

    let mut sleft = Leftovers::default();
    take_service(&service.entries, &mut t.service, &mut sleft);
    for (sec, k, v) in &service.entries {
        if sec != "Service" && !(sec == "Unit" && k == "Description") {
            sleft.push(sec, k, v);
        }
    }
    t.service_manual = sleft.finish(&service.manual);
    t.source = service
        .comments
        .iter()
        .find_map(|c| c.strip_prefix("schedule:").map(|s| s.trim().to_string()))
        .unwrap_or_default();

    Ok(Unit {
        name: super::model::unprefixed(stem).to_string(),
        description: description(service, ""),
        scope,
        body: Body::Timer(Box::new(t)),
    })
}

fn parse_service(scope: Scope, stem: &str, scans: &[Scanned]) -> Unit {
    let s = &scans[0];
    let mut svc = StandaloneService {
        wanted_by: first(&s.entries, "Install", "WantedBy").unwrap_or_default(),
        ..StandaloneService::default()
    };
    let mut left = Leftovers::default();
    take_service(&s.entries, &mut svc.service, &mut left);
    for (sec, k, v) in &s.entries {
        match (sec.as_str(), k.as_str()) {
            ("Service", _) | ("Unit", "Description") | ("Install", "WantedBy") => {}
            _ => left.push(sec, k, v),
        }
    }
    svc.manual = left.finish(&s.manual);

    Unit {
        name: super::model::unprefixed(stem).to_string(),
        description: description(s, ""),
        scope,
        body: Body::Service(Box::new(svc)),
    }
}

fn parse_mount(scope: Scope, stem: &str, scans: &[Scanned], automount: bool) -> Unit {
    let empty = Scanned::default();
    let (am, mnt) = if automount {
        (&scans[0], scans.get(1).unwrap_or(&empty))
    } else {
        (&empty, &scans[0])
    };

    let mut m = MountUnit {
        automount,
        what: first(&mnt.entries, "Mount", "What").unwrap_or_default(),
        where_: first(&mnt.entries, "Mount", "Where")
            .unwrap_or_else(|| escape::unescape_path(stem)),
        fstype: first(&mnt.entries, "Mount", "Type").unwrap_or_default(),
        options: first(&mnt.entries, "Mount", "Options").unwrap_or_default(),
        timeout_idle: first(&am.entries, "Automount", "TimeoutIdleSec"),
        ..MountUnit::default()
    };
    m.preset = MountPreset::ALL
        .into_iter()
        .find(|p| p.fstype() == m.fstype)
        .unwrap_or(MountPreset::Block);

    let mut left = Leftovers::default();
    for (sec, k, v) in &mnt.entries {
        match (sec.as_str(), k.as_str()) {
            ("Mount", "What" | "Where" | "Type" | "Options")
            | ("Unit", "Description")
            | ("Install", "WantedBy") => {}
            _ => left.push(sec, k, v),
        }
    }
    m.manual = left.finish(&mnt.manual);

    let mut aleft = Leftovers::default();
    for (sec, k, v) in &am.entries {
        match (sec.as_str(), k.as_str()) {
            ("Automount", "Where" | "TimeoutIdleSec")
            | ("Unit", "Description")
            | ("Install", "WantedBy") => {}
            _ => aleft.push(sec, k, v),
        }
    }
    m.automount_manual = aleft.finish(&am.manual);

    let desc_from = if automount { am } else { mnt };
    Unit {
        name: stem.to_string(),
        description: description(desc_from, " (automount)"),
        scope,
        body: Body::Mount(Box::new(m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::generate::render;

    fn round_trip(u: &Unit) -> Unit {
        let files: Vec<SourceFile> = render(u)
            .into_iter()
            .map(|f| SourceFile {
                name: f.name,
                body: f.body,
            })
            .collect();
        let (back, owned) = parse(u.scope, &files).expect("generated units should parse");
        assert!(owned, "generated units must be detected as owned");
        back
    }

    fn assert_lossless(u: &Unit) {
        let back = round_trip(u);
        assert_eq!(&back, u, "model changed across a round trip");
        assert_eq!(render(&back), render(u), "rendered bytes changed");
    }

    fn timer_fixture() -> Unit {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        u.description = "nightly backup".into();
        if let Body::Timer(t) = &mut u.body {
            t.source = "cron: 0 3 * * *".into();
            t.schedule =
                Schedule::Calendar(vec!["*-*-13 10:15:00".into(), "Fri *-*-* 10:15:00".into()]);
            t.persistent = true;
            t.randomized_delay = Some("30s".into());
            t.service.exec_start = "/bin/sh -c \"df -h | mail me\"".into();
            t.service.working_directory = Some("/srv".into());
            t.service.run_as = Some("nobody".into());
            t.service.environment = vec!["TZ=UTC".into(), "PATH=/usr/bin".into()];
            t.service.exec_start_pre = Some("/bin/true".into());
        }
        u
    }

    #[test]
    fn timer_round_trip_is_lossless() {
        assert_lossless(&timer_fixture());
    }

    #[test]
    fn interval_and_boot_timers_round_trip() {
        for sched in [
            Schedule::Every {
                every: "15min".into(),
                boot: "5min".into(),
            },
            Schedule::Boot {
                boot: "2min".into(),
            },
        ] {
            let mut u = timer_fixture();
            if let Body::Timer(t) = &mut u.body {
                t.schedule = sched;
                // Persistent is meaningless here and is not re-emitted.
                t.persistent = false;
            }
            assert_lossless(&u);
        }
    }

    #[test]
    fn standalone_service_round_trip_is_lossless() {
        let mut u = Unit::new_service(Scope::System);
        u.name = "webhook".into();
        u.description = "webhook listener".into();
        if let Body::Service(s) = &mut u.body {
            s.service.service_type = ServiceType::Notify;
            s.service.exec_start = "/usr/local/bin/webhook --port 9000".into();
            s.service.exec_stop_post = Some("/usr/local/bin/cleanup".into());
            s.service.restart = RestartPolicy::Always;
            s.service.restart_sec = Some("5s".into());
            s.service.environment = vec!["RUST_LOG=info".into()];
            s.wanted_by = "multi-user.target".into();
        }
        assert_lossless(&u);
    }

    #[test]
    fn mount_and_automount_round_trip_is_lossless() {
        for automount in [false, true] {
            let mut u = Unit::new_mount();
            u.description = "media share".into();
            if let Body::Mount(m) = &mut u.body {
                m.preset = MountPreset::Cifs;
                m.what = "//nas/media".into();
                m.where_ = "/mnt/my-media".into();
                m.fstype = "cifs".into();
                m.options = MountPreset::Cifs.options().into();
                m.automount = automount;
                m.timeout_idle = if automount { Some("120".into()) } else { None };
            }
            u.name = u.stem().unwrap();
            assert_lossless(&u);
        }
    }

    #[test]
    fn manual_directives_survive_the_round_trip() {
        let mut u = timer_fixture();
        if let Body::Timer(t) = &mut u.body {
            t.service_manual = "[Service]\nNice=19\nIOSchedulingClass=idle".into();
            t.timer_manual = "[Timer]\nFixedRandomDelay=true".into();
        }
        assert_lossless(&u);
    }

    #[test]
    fn unmodelled_directives_land_in_the_manual_block() {
        let files = [SourceFile {
            name: "notcron-x.service".into(),
            body: "# Generated by notcron\n\
                   [Unit]\n\
                   Description=x\n\
                   X-NotCron=1\n\
                   After=network.target\n\
                   [Service]\n\
                   Type=simple\n\
                   ExecStart=/bin/true\n\
                   PrivateTmp=yes\n"
                .into(),
        }];
        let (u, owned) = parse(Scope::User, &files).unwrap();
        assert!(owned);
        let Body::Service(s) = &u.body else {
            panic!("expected a service")
        };
        assert_eq!(s.service.exec_start, "/bin/true");
        assert!(s.manual.contains("After=network.target"), "{}", s.manual);
        assert!(s.manual.contains("PrivateTmp=yes"), "{}", s.manual);
        assert!(s.manual.contains("[Unit]"));
        assert!(s.manual.contains("[Service]"));
        // Re-rendering keeps them, and settles after one pass.
        let rendered = render(&u);
        let again = parse(
            Scope::User,
            &rendered
                .iter()
                .map(|f| SourceFile {
                    name: f.name.clone(),
                    body: f.body.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
        .0;
        assert_eq!(render(&again), rendered);
    }

    #[test]
    fn foreign_units_are_not_owned() {
        let files = [SourceFile {
            name: "sshd.service".into(),
            body: "[Unit]\nDescription=OpenSSH server\n[Service]\nExecStart=/usr/sbin/sshd -D\n"
                .into(),
        }];
        let (u, owned) = parse(Scope::System, &files).unwrap();
        assert!(!owned);
        assert_eq!(u.description, "OpenSSH server");
    }

    #[test]
    fn a_timer_without_a_schedule_is_an_error() {
        let files = [SourceFile {
            name: "x.timer".into(),
            body: "[Unit]\nDescription=x\n[Timer]\n".into(),
        }];
        assert!(parse(Scope::User, &files).is_err());
    }

    #[test]
    fn unknown_suffixes_are_rejected() {
        let files = [SourceFile {
            name: "x.socket".into(),
            body: String::new(),
        }];
        assert!(parse(Scope::User, &files).is_err());
        let files = [SourceFile {
            name: "noSuffix".into(),
            body: String::new(),
        }];
        assert!(parse(Scope::User, &files).is_err());
    }

    /// `Group=` is modelled rather than swept into the manual block, so it
    /// survives an edit in place.
    #[test]
    fn group_round_trips_as_a_modelled_directive() {
        let mut u = timer_fixture();
        if let Body::Timer(t) = &mut u.body {
            t.service.run_as = Some("backup".into());
            t.service.group = Some("backup".into());
        }
        assert_lossless(&u);
        let files = render(&u);
        let body = &files
            .iter()
            .find(|f| f.name.ends_with(".service"))
            .unwrap()
            .body;
        assert!(body.contains("\nGroup=backup\n"), "{body}");
        // ...and it is not duplicated into the manual block.
        assert!(!body.contains("notcron:manual"), "{body}");
    }
}
