//! Rendering the model back out as systemd unit files.
//!
//! The output ordering here is a contract: [`super::parse`] reconstructs the
//! model from it, and the round trip must be byte-for-byte stable.

use super::model::{
    Body, MountUnit, Schedule, ServiceOpts, StandaloneService, TimerJob, Unit, HOMEPAGE,
    MANUAL_HEADER, MARKER, X_MARKER,
};

/// A file to write: name relative to the unit directory, plus its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    pub name: String,
    pub body: String,
}

fn header(out: &mut String, extra_comments: &[String]) {
    out.push_str(MARKER);
    out.push('\n');
    out.push_str(&format!("# {HOMEPAGE}\n"));
    for c in extra_comments {
        out.push_str(&format!("# {c}\n"));
    }
}

fn kv(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(value);
    out.push('\n');
}

fn kv_opt(out: &mut String, key: &str, value: &Option<String>) {
    if let Some(v) = value {
        if !v.trim().is_empty() {
            kv(out, key, v);
        }
    }
}

/// Append the free-text manual block, if any. Trailing whitespace is trimmed
/// so the round trip is idempotent.
fn manual(out: &mut String, text: &str) {
    let trimmed = text.trim_end_matches(['\n', ' ', '\t']);
    if trimmed.is_empty() {
        return;
    }
    out.push('\n');
    out.push_str(MANUAL_HEADER);
    out.push('\n');
    out.push_str(trimmed);
    out.push('\n');
}

fn service_section(out: &mut String, s: &ServiceOpts, include_type: bool) {
    out.push_str("\n[Service]\n");
    if include_type {
        kv(out, "Type", s.service_type.as_str());
    }
    kv_opt(out, "ExecStartPre", &s.exec_start_pre);
    kv(out, "ExecStart", &s.exec_start);
    kv_opt(out, "ExecStopPost", &s.exec_stop_post);
    kv_opt(out, "WorkingDirectory", &s.working_directory);
    kv_opt(out, "User", &s.run_as);
    kv_opt(out, "Group", &s.group);
    for e in &s.environment {
        if !e.trim().is_empty() {
            kv(out, "Environment", e);
        }
    }
}

fn render_timer_service(u: &Unit, t: &TimerJob) -> RenderedFile {
    let stem = u.stem().unwrap_or_default();
    let mut out = String::new();
    let comments = if t.source.is_empty() {
        vec![]
    } else {
        vec![format!("schedule: {}", t.source)]
    };
    header(&mut out, &comments);
    out.push_str("\n[Unit]\n");
    kv(&mut out, "Description", &u.description);
    out.push_str(X_MARKER);
    out.push('\n');
    service_section(&mut out, &t.service, true);
    kv(&mut out, "StandardOutput", "journal");
    kv(&mut out, "StandardError", "journal");
    kv(&mut out, "SyslogIdentifier", &stem);
    manual(&mut out, &t.service_manual);
    RenderedFile {
        name: format!("{stem}.service"),
        body: out,
    }
}

fn render_timer(u: &Unit, t: &TimerJob) -> RenderedFile {
    let stem = u.stem().unwrap_or_default();
    let mut out = String::new();
    header(&mut out, &[]);
    out.push_str("\n[Unit]\n");
    kv(
        &mut out,
        "Description",
        &format!("{} (timer)", u.description),
    );
    out.push_str(X_MARKER);
    out.push('\n');
    out.push_str("\n[Timer]\n");
    match &t.schedule {
        Schedule::Calendar(specs) => {
            for c in specs {
                if !c.trim().is_empty() {
                    kv(&mut out, "OnCalendar", c);
                }
            }
        }
        Schedule::Every { every, boot } => {
            kv(&mut out, "OnBootSec", boot);
            kv(&mut out, "OnUnitActiveSec", every);
        }
        Schedule::Boot { boot } => kv(&mut out, "OnBootSec", boot),
    }
    // Persistent= only has meaning for calendar timers.
    if matches!(t.schedule, Schedule::Calendar(_)) && t.persistent {
        kv(&mut out, "Persistent", "true");
    }
    kv_opt(&mut out, "RandomizedDelaySec", &t.randomized_delay);
    kv(&mut out, "AccuracySec", "1s");
    kv(&mut out, "Unit", &format!("{stem}.service"));
    out.push_str("\n[Install]\n");
    kv(&mut out, "WantedBy", "timers.target");
    manual(&mut out, &t.timer_manual);
    RenderedFile {
        name: format!("{stem}.timer"),
        body: out,
    }
}

fn render_standalone(u: &Unit, s: &StandaloneService) -> RenderedFile {
    let stem = u.stem().unwrap_or_default();
    let mut out = String::new();
    header(&mut out, &[]);
    out.push_str("\n[Unit]\n");
    kv(&mut out, "Description", &u.description);
    out.push_str(X_MARKER);
    out.push('\n');
    service_section(&mut out, &s.service, true);
    kv(&mut out, "Restart", s.service.restart.as_str());
    kv_opt(&mut out, "RestartSec", &s.service.restart_sec);
    kv(&mut out, "StandardOutput", "journal");
    kv(&mut out, "StandardError", "journal");
    kv(&mut out, "SyslogIdentifier", &stem);
    out.push_str("\n[Install]\n");
    kv(&mut out, "WantedBy", &s.wanted_by);
    manual(&mut out, &s.manual);
    RenderedFile {
        name: format!("{stem}.service"),
        body: out,
    }
}

fn render_mount(u: &Unit, m: &MountUnit) -> RenderedFile {
    let stem = u.stem().unwrap_or_default();
    let mut out = String::new();
    header(&mut out, &[]);
    out.push_str("\n[Unit]\n");
    kv(&mut out, "Description", &u.description);
    out.push_str(X_MARKER);
    out.push('\n');
    out.push_str("\n[Mount]\n");
    kv(&mut out, "What", &m.what);
    kv(&mut out, "Where", &m.where_);
    kv(&mut out, "Type", &m.fstype);
    if !m.options.trim().is_empty() {
        kv(&mut out, "Options", &m.options);
    }
    out.push_str("\n[Install]\n");
    kv(&mut out, "WantedBy", "multi-user.target");
    manual(&mut out, &m.manual);
    RenderedFile {
        name: format!("{stem}.mount"),
        body: out,
    }
}

fn render_automount(u: &Unit, m: &MountUnit) -> RenderedFile {
    let stem = u.stem().unwrap_or_default();
    let mut out = String::new();
    header(&mut out, &[]);
    out.push_str("\n[Unit]\n");
    kv(
        &mut out,
        "Description",
        &format!("{} (automount)", u.description),
    );
    out.push_str(X_MARKER);
    out.push('\n');
    out.push_str("\n[Automount]\n");
    kv(&mut out, "Where", &m.where_);
    kv_opt(&mut out, "TimeoutIdleSec", &m.timeout_idle);
    out.push_str("\n[Install]\n");
    kv(&mut out, "WantedBy", "multi-user.target");
    manual(&mut out, &m.automount_manual);
    RenderedFile {
        name: format!("{stem}.automount"),
        body: out,
    }
}

/// Render every file for a unit. The order matches [`Unit::filenames`].
pub fn render(u: &Unit) -> Vec<RenderedFile> {
    match &u.body {
        Body::Timer(t) => vec![render_timer(u, t), render_timer_service(u, t)],
        Body::Service(s) => vec![render_standalone(u, s)],
        Body::Mount(m) => {
            if m.automount {
                vec![render_automount(u, m), render_mount(u, m)]
            } else {
                vec![render_mount(u, m)]
            }
        }
    }
}

/// The whole unit as one previewable blob, with per-file headings.
pub fn preview(u: &Unit, dir: &str) -> String {
    render(u)
        .into_iter()
        .map(|f| format!("# {dir}/{}\n{}", f.name, f.body))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::model::{MountPreset, RestartPolicy, Scope, ServiceType};

    fn sample_timer() -> Unit {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        u.description = "nightly backup".into();
        if let Body::Timer(t) = &mut u.body {
            t.source = "cron: 0 3 * * *".into();
            t.schedule = Schedule::Calendar(vec!["*-*-* 03:00:00".into()]);
            t.service.exec_start = "/usr/local/bin/backup.sh --full".into();
            t.service.working_directory = Some("/srv".into());
            t.service.environment = vec!["TZ=UTC".into(), "LANG=C".into()];
        }
        u
    }

    #[test]
    fn timer_pair_renders_both_files_marked() {
        let files = render(&sample_timer());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "notcron-backup.timer");
        assert_eq!(files[1].name, "notcron-backup.service");
        for f in &files {
            assert!(f.body.starts_with(MARKER), "{}", f.body);
            assert!(f.body.contains(X_MARKER));
        }
    }

    #[test]
    fn timer_file_content_is_stable() {
        let files = render(&sample_timer());
        assert_eq!(
            files[0].body,
            "# Generated by notcron\n\
             # https://github.com/suchattai-labs/notcron-rs\n\
             \n[Unit]\n\
             Description=nightly backup (timer)\n\
             X-NotCron=1\n\
             \n[Timer]\n\
             OnCalendar=*-*-* 03:00:00\n\
             Persistent=true\n\
             AccuracySec=1s\n\
             Unit=notcron-backup.service\n\
             \n[Install]\n\
             WantedBy=timers.target\n"
        );
        assert_eq!(
            files[1].body,
            "# Generated by notcron\n\
             # https://github.com/suchattai-labs/notcron-rs\n\
             # schedule: cron: 0 3 * * *\n\
             \n[Unit]\n\
             Description=nightly backup\n\
             X-NotCron=1\n\
             \n[Service]\n\
             Type=oneshot\n\
             ExecStart=/usr/local/bin/backup.sh --full\n\
             WorkingDirectory=/srv\n\
             Environment=TZ=UTC\n\
             Environment=LANG=C\n\
             StandardOutput=journal\n\
             StandardError=journal\n\
             SyslogIdentifier=notcron-backup\n"
        );
    }

    #[test]
    fn persistent_is_omitted_for_non_calendar_schedules() {
        let mut u = sample_timer();
        if let Body::Timer(t) = &mut u.body {
            t.schedule = Schedule::Every {
                every: "15min".into(),
                boot: "15min".into(),
            };
        }
        let body = &render(&u)[0].body;
        assert!(!body.contains("Persistent="));
        assert!(body.contains("OnUnitActiveSec=15min"));
        assert!(body.contains("OnBootSec=15min"));
    }

    #[test]
    fn manual_block_is_appended_last() {
        let mut u = sample_timer();
        if let Body::Timer(t) = &mut u.body {
            t.service_manual = "[Service]\nNice=19\n\n".into();
        }
        let body = &render(&u)[1].body;
        assert!(body.contains(MANUAL_HEADER));
        assert!(body.ends_with("Nice=19\n"), "{body}");
    }

    #[test]
    fn standalone_service_renders_install_and_restart() {
        let mut u = Unit::new_service(Scope::System);
        u.name = "webhook".into();
        u.description = "webhook listener".into();
        if let Body::Service(s) = &mut u.body {
            s.service.service_type = ServiceType::Simple;
            s.service.exec_start = "/usr/local/bin/webhook".into();
            s.service.restart = RestartPolicy::OnFailure;
            s.service.restart_sec = Some("5s".into());
        }
        let f = &render(&u)[0];
        assert_eq!(f.name, "notcron-webhook.service");
        assert!(f.body.contains("Type=simple\n"));
        assert!(f.body.contains("Restart=on-failure\n"));
        assert!(f.body.contains("RestartSec=5s\n"));
        assert!(f.body.contains("WantedBy=multi-user.target\n"));
    }

    #[test]
    fn automount_renders_both_files() {
        let mut u = Unit::new_mount();
        u.description = "media share".into();
        if let Body::Mount(m) = &mut u.body {
            m.preset = MountPreset::Nfs;
            m.what = "nas:/export/media".into();
            m.where_ = "/mnt/media".into();
            m.fstype = MountPreset::Nfs.fstype().into();
            m.options = MountPreset::Nfs.options().into();
            m.automount = true;
            m.timeout_idle = Some("120".into());
        }
        let files = render(&u);
        assert_eq!(files[0].name, "mnt-media.automount");
        assert_eq!(files[1].name, "mnt-media.mount");
        assert!(files[0].body.contains("TimeoutIdleSec=120\n"));
        assert!(files[1].body.contains("What=nas:/export/media\n"));
        assert!(files[1].body.contains("Where=/mnt/media\n"));
        assert!(files[1].body.contains("Type=nfs\n"));
    }

    #[test]
    fn preview_labels_each_file_with_its_target_path() {
        let p = preview(&sample_timer(), "/home/x/.config/systemd/user");
        assert!(p.contains("# /home/x/.config/systemd/user/notcron-backup.timer"));
        assert!(p.contains("# /home/x/.config/systemd/user/notcron-backup.service"));
    }
}
