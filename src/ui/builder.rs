//! The detailed unit builder: a focus-driven field list where Enter opens the
//! right editor for whatever is selected -- a text prompt, a choice list, a
//! toggle, the schedule sub-builder, or the free-text manual editor.

use super::dialogs::{self, Background};
use super::editor;
use super::picker;
use super::term::Term;
use crate::cron::{self, Translation};
use crate::systemd;
use crate::unit::escape;
use crate::unit::generate;
use crate::unit::model::{
    Body, MountPreset, RestartPolicy, Schedule, Scope, ServiceOpts, ServiceType, Unit,
};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

/// Identifies what activating a row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Id {
    /// A non-selectable section heading.
    Heading,
    Name,
    Description,
    Scope,
    Schedule,
    Persistent,
    RandomDelay,
    ServiceType,
    ExecStart,
    ShellWrap,
    ExecStartPre,
    ExecStopPost,
    Restart,
    RestartSec,
    WorkDir,
    RunAs,
    Env,
    WantedBy,
    Preset,
    What,
    Where,
    FsType,
    Options,
    Automount,
    TimeoutIdle,
    ManualPrimary,
    ManualSecondary,
    Preview,
    Save,
}

struct Row {
    id: Id,
    label: String,
    value: String,
}

fn row(id: Id, label: &str, value: impl Into<String>) -> Row {
    Row {
        id,
        label: label.to_string(),
        value: value.into(),
    }
}

fn heading(label: &str) -> Row {
    row(Id::Heading, label, "")
}

fn opt(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "(unset)".into())
}

fn yesno(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn service_rows(s: &ServiceOpts, rows: &mut Vec<Row>, with_restart: bool) {
    rows.push(row(Id::ServiceType, "Type", s.service_type.as_str()));
    rows.push(row(Id::ExecStart, "ExecStart", s.exec_start.clone()));
    rows.push(row(Id::ShellWrap, "Wrap in /bin/sh -c", "(press Enter)"));
    rows.push(row(
        Id::ExecStartPre,
        "ExecStartPre",
        opt(&s.exec_start_pre),
    ));
    rows.push(row(
        Id::ExecStopPost,
        "ExecStopPost",
        opt(&s.exec_stop_post),
    ));
    if with_restart {
        rows.push(row(Id::Restart, "Restart", s.restart.as_str()));
        rows.push(row(Id::RestartSec, "RestartSec", opt(&s.restart_sec)));
    }
    rows.push(row(
        Id::WorkDir,
        "WorkingDirectory",
        opt(&s.working_directory),
    ));
    rows.push(row(Id::RunAs, "User", opt(&s.run_as)));
    rows.push(row(
        Id::Env,
        "Environment",
        if s.environment.is_empty() {
            "(none)".into()
        } else {
            s.environment.join("  ")
        },
    ));
}

fn rows_for(u: &Unit) -> Vec<Row> {
    let mut rows = Vec::new();
    let stem = u.stem().unwrap_or_else(|_| "(invalid)".into());
    match &u.body {
        Body::Timer(t) => {
            rows.push(heading("Unit"));
            rows.push(row(Id::Name, "Name", format!("{} -> {stem}", u.name)));
            rows.push(row(Id::Description, "Description", u.description.clone()));
            rows.push(row(Id::Scope, "Scope", u.scope.as_str()));
            rows.push(heading("Schedule"));
            rows.push(row(Id::Schedule, "When", t.schedule.summary()));
            rows.push(row(Id::Persistent, "Persistent", yesno(t.persistent)));
            rows.push(row(
                Id::RandomDelay,
                "RandomizedDelaySec",
                opt(&t.randomized_delay),
            ));
            rows.push(heading("Service"));
            service_rows(&t.service, &mut rows, false);
            rows.push(heading("Manual directives"));
            rows.push(row(
                Id::ManualPrimary,
                "Extra [Service] lines",
                summarize(&t.service_manual),
            ));
            rows.push(row(
                Id::ManualSecondary,
                "Extra [Timer] lines",
                summarize(&t.timer_manual),
            ));
        }
        Body::Service(s) => {
            rows.push(heading("Unit"));
            rows.push(row(Id::Name, "Name", format!("{} -> {stem}", u.name)));
            rows.push(row(Id::Description, "Description", u.description.clone()));
            rows.push(row(Id::Scope, "Scope", u.scope.as_str()));
            rows.push(heading("Service"));
            service_rows(&s.service, &mut rows, true);
            rows.push(heading("Install"));
            rows.push(row(Id::WantedBy, "WantedBy", s.wanted_by.clone()));
            rows.push(heading("Manual directives"));
            rows.push(row(Id::ManualPrimary, "Extra lines", summarize(&s.manual)));
        }
        Body::Mount(m) => {
            rows.push(heading("Mount (system scope only)"));
            rows.push(row(Id::Preset, "Preset", m.preset.label()));
            rows.push(row(Id::What, "What", m.what.clone()));
            rows.push(row(Id::Where, "Where", format!("{} -> {stem}", m.where_)));
            rows.push(row(Id::FsType, "Type", m.fstype.clone()));
            rows.push(row(Id::Options, "Options", m.options.clone()));
            rows.push(row(Id::Description, "Description", u.description.clone()));
            rows.push(heading("Automount"));
            rows.push(row(
                Id::Automount,
                "Companion .automount",
                yesno(m.automount),
            ));
            rows.push(row(Id::TimeoutIdle, "TimeoutIdleSec", opt(&m.timeout_idle)));
            rows.push(heading("Manual directives"));
            rows.push(row(
                Id::ManualPrimary,
                "Extra [Mount] lines",
                summarize(&m.manual),
            ));
            rows.push(row(
                Id::ManualSecondary,
                "Extra [Automount] lines",
                summarize(&m.automount_manual),
            ));
        }
    }
    rows.push(heading(""));
    rows.push(row(Id::Preview, "Preview the unit files", ""));
    rows.push(row(Id::Save, "Save and install", ""));
    rows
}

fn summarize(text: &str) -> String {
    let n = text.lines().filter(|l| !l.trim().is_empty()).count();
    match n {
        0 => "(none)".into(),
        1 => text.trim().to_string(),
        _ => format!("({n} lines)"),
    }
}

/// The status line under the form: validation state, and the next elapse for
/// calendar timers.
fn status_line(u: &Unit) -> String {
    if let Err(e) = u.validate() {
        return format!("! {e}");
    }
    if !systemd::has_analyze() {
        return "ready to install (systemd-analyze missing: schedules unchecked)".into();
    }
    if let Body::Timer(t) = &u.body {
        if let Schedule::Calendar(specs) = &t.schedule {
            let mut parts = Vec::new();
            for s in specs {
                match systemd::check_calendar(s) {
                    Ok(Some(next)) => parts.push(format!("next: {next}")),
                    Ok(None) => {}
                    Err(e) => return format!("! {}", e.lines().next().unwrap_or("invalid")),
                }
            }
            if !parts.is_empty() {
                return parts.join("   ");
            }
        }
    }
    "ready to install".into()
}

fn first_selectable(rows: &[Row]) -> usize {
    rows.iter().position(|r| r.id != Id::Heading).unwrap_or(0)
}

fn step(rows: &[Row], from: usize, delta: isize) -> usize {
    let n = rows.len() as isize;
    if n == 0 {
        return 0;
    }
    let mut i = from as isize;
    for _ in 0..n {
        i = (i + delta).rem_euclid(n);
        if rows[i as usize].id != Id::Heading {
            return i as usize;
        }
    }
    from
}

/// Run the builder. Returns `true` when the unit was installed.
pub fn run(term: &mut Term, bg: Background, u: &mut Unit, title: &str) -> bool {
    let mut sel = first_selectable(&rows_for(u));
    let mut top = 0usize;
    let mut status = status_line(u);

    loop {
        let rows = rows_for(u);
        sel = sel.min(rows.len().saturating_sub(1));
        if rows[sel].id == Id::Heading {
            sel = step(&rows, sel, 1);
        }
        let (t, st) = (title.to_string(), status.clone());
        // Surface the picker on the row it applies to, so it is discoverable
        // without reading the help.
        let (browse, browse_style) = if browsable(u, rows[sel].id).is_some() {
            ("b browses this path", Style::new().bold().fg(Color::Cyan))
        } else {
            ("b browses path fields", Style::new().fg(Color::DarkGray))
        };
        let s = sel;
        let mut visible = 1usize;
        let painted: Vec<(Id, String, String)> = rows
            .iter()
            .map(|r| (r.id, r.label.clone(), r.value.clone()))
            .collect();
        let tp = top;
        let _ = term.terminal.draw(|f| {
            bg(f);
            let area = f.area();
            // Four lines, not three: the status and the help line each need
            // one, and at three the status pushed the keybindings off screen
            // entirely whenever the unit was still incomplete -- which is
            // always, when the form has just opened.
            let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).split(area);
            let block = Block::default()
                .title(format!(" {t} "))
                .title_style(Style::new().bold())
                .borders(Borders::ALL);
            let inner = block.inner(chunks[0]);
            f.render_widget(block, chunks[0]);
            visible = inner.height.max(1) as usize;
            let t0 = if s < tp {
                s
            } else if s >= tp + visible {
                s + 1 - visible
            } else {
                tp
            };
            let label_w = 22usize;
            let value_w = (inner.width as usize).saturating_sub(label_w + 4);
            let lines: Vec<Line> = painted
                .iter()
                .enumerate()
                .skip(t0)
                .take(visible)
                .map(|(i, (id, label, value))| {
                    if *id == Id::Heading {
                        return Line::from(Span::styled(
                            format!(" {label}"),
                            Style::new().bold().fg(Color::Cyan),
                        ));
                    }
                    let v = truncate(value, value_w);
                    let text = format!(" {} {label:<label_w$} {v}", if i == s { ">" } else { " " });
                    Line::from(Span::styled(
                        text,
                        if i == s {
                            Style::new().bold().reversed()
                        } else {
                            Style::new()
                        },
                    ))
                })
                .collect();
            f.render_widget(Paragraph::new(lines), inner);

            // Status and help get a line each and are truncated rather than
            // wrapped, so neither can ever push the other off the screen.
            let footer = Block::default().borders(Borders::ALL);
            let fi = footer.inner(chunks[1]);
            f.render_widget(footer, chunks[1]);
            let fr = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(fi);
            f.render_widget(Paragraph::new(st.as_str()), fr[0]);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw("Enter edits   Tab/arrows move   "),
                    Span::styled(browse, browse_style),
                    Span::raw("   p previews   Ctrl-S saves   Esc cancels"),
                ]))
                .style(Style::new().fg(Color::DarkGray)),
                fr[1],
            );
        });
        if sel < top {
            top = sel;
        } else if sel >= top + visible {
            top = sel + 1 - visible;
        }

        let Some(k) = term.next_key() else {
            return false;
        };
        use super::term::Key;
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => sel = step(&rows, sel, d as isize),
            _ if k.is_ctrl('s') => {
                if save(term, bg, u) {
                    return true;
                }
                status = status_line(u);
            }
            _ => match k.code() {
                Some(KeyCode::Esc) => {
                    if dialogs::confirm(term, bg, "Discard", "Discard this unit and go back?") {
                        return false;
                    }
                }
                Some(KeyCode::Up) | Some(KeyCode::BackTab) => sel = step(&rows, sel, -1),
                Some(KeyCode::Down) | Some(KeyCode::Tab) => sel = step(&rows, sel, 1),
                Some(KeyCode::Enter) => {
                    if rows[sel].id == Id::Save {
                        if save(term, bg, u) {
                            return true;
                        }
                    } else {
                        activate(term, bg, u, rows[sel].id);
                    }
                    status = status_line(u);
                }
                _ if k.is_char('b') => {
                    if browsable(u, rows[sel].id).is_some() {
                        browse_field(term, bg, u, rows[sel].id);
                        status = status_line(u);
                    } else {
                        status = "b browses only path fields: ExecStart, ExecStartPre, \
                                  ExecStopPost, WorkingDirectory, What and Where"
                            .into();
                    }
                }
                _ if k.is_char('p') => preview(term, bg, u),
                _ if k.is_char('k') => sel = step(&rows, sel, -1),
                _ if k.is_char('j') => sel = step(&rows, sel, 1),
                _ => {}
            },
        }
    }
}

fn truncate(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

fn preview(term: &mut Term, bg: Background, u: &Unit) {
    let dir = systemd::unit_dir(u.scope);
    let body = match u.validate() {
        Ok(()) => generate::preview(u, &dir.to_string_lossy()),
        Err(e) => format!(
            "(incomplete: {e})\n\n{}",
            generate::preview(u, &dir.to_string_lossy())
        ),
    };
    dialogs::pager(term, bg, "Preview", &body);
}

fn save(term: &mut Term, bg: Background, u: &Unit) -> bool {
    if let Err(e) = u.validate() {
        dialogs::msgbox(term, bg, "Cannot install", &e);
        return false;
    }
    let files = match u.filenames() {
        Ok(f) => f,
        Err(e) => {
            dialogs::msgbox(term, bg, "Cannot install", &e);
            return false;
        }
    };
    let dir = systemd::unit_dir(u.scope);
    let existing: Vec<&String> = files.iter().filter(|f| dir.join(f).exists()).collect();
    let body = format!(
        "Write into {}:\n  {}\n{}",
        dir.display(),
        files.join("\n  "),
        if existing.is_empty() {
            String::new()
        } else {
            format!(
                "\nOverwrites: {}\n",
                existing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    if !dialogs::confirm(term, bg, "Install", &body) {
        return false;
    }
    let start = !matches!(u.body, Body::Mount(_))
        || dialogs::confirm(term, bg, "Start now", "Mount it immediately as well?");
    match systemd::install(u, true, start) {
        Ok(r) => {
            let mut msg = format!(
                "Installed:\n  {}",
                r.written
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
            for w in &r.warnings {
                msg.push_str(&format!("\n\nwarning: {w}"));
            }
            dialogs::msgbox(term, bg, "Installed", &msg);
            true
        }
        Err(e) => {
            dialogs::msgbox(term, bg, "Install failed", &e);
            false
        }
    }
}

/// Edit one field.
fn activate(term: &mut Term, bg: Background, u: &mut Unit, id: Id) {
    // Shared helpers, written so each arm stays a couple of lines.
    macro_rules! text {
        ($title:expr, $help:expr, $cur:expr, $val:expr) => {
            dialogs::prompt(term, bg, $title, $help, $cur, $val)
        };
    }

    match id {
        Id::Heading => {}
        Id::Name => {
            if let Some(v) = text!(
                "Unit name",
                "Short name; notcron- is prepended automatically.",
                &u.name,
                &|s: &str| crate::unit::model::validate_name(&escape::slugify(s))
            ) {
                u.name = escape::slugify(&v);
            }
        }
        Id::Description => {
            if let Some(v) = text!(
                "Description",
                "Shown by systemctl status and in the journal.",
                &u.description,
                &dialogs::no_validation
            ) {
                u.description = v;
            }
        }
        Id::Scope => {
            let items = vec![
                "user   -- ~/.config/systemd/user, no sudo, needs lingering".to_string(),
                "system -- /etc/systemd/system, installed via sudo".to_string(),
            ];
            let cur = if u.scope == Scope::System { 1 } else { 0 };
            if let Some(i) = dialogs::pick(term, bg, "Scope", &items, cur) {
                u.scope = if i == 1 { Scope::System } else { Scope::User };
            }
        }
        Id::Schedule => {
            if let Body::Timer(t) = &mut u.body {
                edit_schedule(term, bg, &mut t.schedule, &mut t.source);
            }
        }
        Id::Persistent => {
            if let Body::Timer(t) = &mut u.body {
                t.persistent = !t.persistent;
            }
        }
        Id::RandomDelay => {
            if let Body::Timer(t) = &mut u.body {
                t.randomized_delay = ask_timespan(
                    term,
                    bg,
                    "RandomizedDelaySec",
                    "Spread the start over a window, e.g. 30s or 5m. Empty clears it.",
                    &t.randomized_delay,
                );
            }
        }
        Id::ServiceType => {
            let items: Vec<String> = ServiceType::ALL
                .iter()
                .map(|t| t.as_str().to_string())
                .collect();
            let svc = service_mut(u);
            let cur = ServiceType::ALL
                .iter()
                .position(|t| *t == svc.service_type)
                .unwrap_or(0);
            if let Some(i) = dialogs::pick(term, bg, "Type=", &items, cur) {
                service_mut(u).service_type = ServiceType::ALL[i];
            }
        }
        Id::ExecStart => {
            let cur = service_mut(u).exec_start.clone();
            if let Some(v) = text!(
                "ExecStart",
                "Absolute path plus arguments. Shell syntax needs the wrapper below.",
                &cur,
                &|s: &str| {
                    if s.trim().is_empty() {
                        Err("ExecStart must not be empty".into())
                    } else {
                        Ok(())
                    }
                }
            ) {
                service_mut(u).exec_start = v;
            }
        }
        Id::ShellWrap => {
            let cur = service_mut(u).exec_start.clone();
            let already = cur.starts_with("/bin/sh -c ");
            let body = if already {
                "Unwrap this command back to a plain argv line?"
            } else {
                "Run this command through /bin/sh -c, so pipes, redirects and \
                 globs work?"
            };
            if dialogs::confirm(term, bg, "Shell wrapper", body) {
                service_mut(u).exec_start = if already {
                    unwrap_shell(&cur)
                } else {
                    format!("/bin/sh -c {}", escape::exec_quote(&cur))
                };
            }
        }
        Id::ExecStartPre => {
            let cur = service_mut(u).exec_start_pre.clone();
            service_mut(u).exec_start_pre = ask_optional(
                term,
                bg,
                "ExecStartPre",
                "Runs before ExecStart. Empty clears it.",
                &cur,
            );
        }
        Id::ExecStopPost => {
            let cur = service_mut(u).exec_stop_post.clone();
            service_mut(u).exec_stop_post = ask_optional(
                term,
                bg,
                "ExecStopPost",
                "Runs after the service stops, success or failure. Empty clears it.",
                &cur,
            );
        }
        Id::Restart => {
            let items: Vec<String> = RestartPolicy::ALL
                .iter()
                .map(|r| r.as_str().to_string())
                .collect();
            let cur = RestartPolicy::ALL
                .iter()
                .position(|r| *r == service_mut(u).restart)
                .unwrap_or(0);
            if let Some(i) = dialogs::pick(term, bg, "Restart=", &items, cur) {
                service_mut(u).restart = RestartPolicy::ALL[i];
            }
        }
        Id::RestartSec => {
            let cur = service_mut(u).restart_sec.clone();
            service_mut(u).restart_sec = ask_timespan(
                term,
                bg,
                "RestartSec",
                "Delay before restarting, e.g. 5s. Empty clears it.",
                &cur,
            );
        }
        Id::WorkDir => {
            let cur = service_mut(u).working_directory.clone();
            service_mut(u).working_directory = ask_optional(
                term,
                bg,
                "WorkingDirectory",
                "Absolute path the command runs in. Empty clears it.",
                &cur,
            );
        }
        Id::RunAs => {
            let cur = service_mut(u).run_as.clone();
            service_mut(u).run_as = ask_optional(
                term,
                bg,
                "User",
                "User= (system units only). Empty clears it.",
                &cur,
            );
        }
        Id::Env => {
            let cur = service_mut(u).environment.join("\n");
            if let Some(v) = editor::edit(term, bg, "Environment", "One KEY=VALUE per line.", &cur)
            {
                service_mut(u).environment = v
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
            }
        }
        Id::WantedBy => {
            if let Body::Service(s) = &mut u.body {
                let items = vec![
                    "multi-user.target".to_string(),
                    "default.target".to_string(),
                    "graphical.target".to_string(),
                    "network-online.target".to_string(),
                ];
                let cur = items.iter().position(|i| *i == s.wanted_by).unwrap_or(0);
                if let Some(i) = dialogs::pick(term, bg, "WantedBy=", &items, cur) {
                    s.wanted_by = items[i].clone();
                }
            }
        }
        Id::Preset => {
            if let Body::Mount(m) = &mut u.body {
                let items: Vec<String> = MountPreset::ALL
                    .iter()
                    .map(|p| format!("{:<32} {}", p.label(), p.options()))
                    .collect();
                let cur = MountPreset::ALL
                    .iter()
                    .position(|p| *p == m.preset)
                    .unwrap_or(0);
                if let Some(i) = dialogs::pick(term, bg, "Filesystem preset", &items, cur) {
                    m.preset = MountPreset::ALL[i];
                    m.fstype = m.preset.fstype().into();
                    m.options = m.preset.options().into();
                }
            }
        }
        Id::What => {
            if let Body::Mount(m) = &mut u.body {
                let help = format!("What to mount, e.g. {}", m.preset.what_hint());
                if let Some(v) = dialogs::prompt(term, bg, "What", &help, &m.what, &|s: &str| {
                    if s.trim().is_empty() {
                        Err("What= must not be empty".into())
                    } else {
                        Ok(())
                    }
                }) {
                    m.what = v;
                }
            }
        }
        Id::Where => {
            if let Body::Mount(m) = &mut u.body {
                if let Some(v) = dialogs::prompt(
                    term,
                    bg,
                    "Where",
                    "Absolute mount point. The unit filename is derived from it.",
                    &m.where_,
                    &|s: &str| escape::escape_path(s).map(|_| ()),
                ) {
                    m.where_ = v;
                    u.name = u.stem().unwrap_or_default();
                }
            }
        }
        Id::FsType => {
            if let Body::Mount(m) = &mut u.body {
                if let Some(v) = dialogs::prompt(
                    term,
                    bg,
                    "Type",
                    "Filesystem type, e.g. ext4, nfs, cifs, auto, none (for bind).",
                    &m.fstype,
                    &|s: &str| {
                        if s.trim().is_empty() {
                            Err("Type= must not be empty".into())
                        } else {
                            Ok(())
                        }
                    },
                ) {
                    m.fstype = v;
                }
            }
        }
        Id::Options => {
            if let Body::Mount(m) = &mut u.body {
                if let Some(v) = dialogs::prompt(
                    term,
                    bg,
                    "Options",
                    "Comma-separated mount options, as in fstab.",
                    &m.options,
                    &dialogs::no_validation,
                ) {
                    m.options = v;
                }
            }
        }
        Id::Automount => {
            if let Body::Mount(m) = &mut u.body {
                m.automount = !m.automount;
                if m.automount && m.timeout_idle.is_none() {
                    m.timeout_idle = Some("120".into());
                }
            }
        }
        Id::TimeoutIdle => {
            if let Body::Mount(m) = &mut u.body {
                m.timeout_idle = ask_timespan(
                    term,
                    bg,
                    "TimeoutIdleSec",
                    "Unmount after this much idle time, e.g. 120 or 5min. Empty clears it.",
                    &m.timeout_idle,
                );
            }
        }
        Id::ManualPrimary | Id::ManualSecondary => {
            let (title, help) = match (&u.body, id) {
                (Body::Timer(_), Id::ManualPrimary) => (
                    "Extra .service directives",
                    "Free-form lines appended to the .service file. Include a [Section] header.",
                ),
                (Body::Timer(_), _) => (
                    "Extra .timer directives",
                    "Free-form lines appended to the .timer file. Include a [Section] header.",
                ),
                (Body::Mount(_), Id::ManualPrimary) => (
                    "Extra .mount directives",
                    "Free-form lines appended to the .mount file. Include a [Section] header.",
                ),
                (Body::Mount(_), _) => (
                    "Extra .automount directives",
                    "Free-form lines appended to the .automount file. Include a [Section] header.",
                ),
                _ => (
                    "Extra directives",
                    "Free-form lines appended to the unit file. Include a [Section] header.",
                ),
            };
            let cur = manual_field(u, id).to_string();
            if let Some(v) = editor::edit(term, bg, title, help, &cur) {
                *manual_field_mut(u, id) = v;
            }
        }
        Id::Preview => preview(term, bg, u),
        Id::Save => {}
    }
}

// ---------------------------------------------------------------------------
// Filesystem picker
// ---------------------------------------------------------------------------

/// How a picked path lands in the field it was picked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Replaces the whole value.
    Whole,
    /// Replaces the program of a command line, keeping its arguments.
    Program,
}

/// What `b` browses for on this row, if the row holds a path at all.
fn browsable(u: &Unit, id: Id) -> Option<(&'static str, picker::Mode, Fill)> {
    match id {
        Id::ExecStart => Some(("ExecStart", picker::Mode::File, Fill::Program)),
        Id::ExecStartPre => Some(("ExecStartPre", picker::Mode::File, Fill::Program)),
        Id::ExecStopPost => Some(("ExecStopPost", picker::Mode::File, Fill::Program)),
        Id::WorkDir => Some(("WorkingDirectory", picker::Mode::Directory, Fill::Whole)),
        Id::Where => Some(("Where", picker::Mode::Directory, Fill::Whole)),
        // `What=` is only a local path for block devices and bind mounts. For
        // NFS and CIFS it is `server:/export` or `//server/share`, which no
        // picker can produce, so browsing is not offered there at all and
        // typing stays the only -- and correct -- route.
        Id::What => match &u.body {
            Body::Mount(m) if m.preset.what_is_path() => {
                Some(("What", picker::Mode::Any, Fill::Whole))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The current value of a browsable field.
fn path_field(u: &Unit, id: Id) -> String {
    let svc = match &u.body {
        Body::Timer(t) => Some(&t.service),
        Body::Service(s) => Some(&s.service),
        Body::Mount(_) => None,
    };
    match (id, svc, &u.body) {
        (Id::ExecStart, Some(s), _) => s.exec_start.clone(),
        (Id::ExecStartPre, Some(s), _) => opt_or_empty(&s.exec_start_pre),
        (Id::ExecStopPost, Some(s), _) => opt_or_empty(&s.exec_stop_post),
        (Id::WorkDir, Some(s), _) => opt_or_empty(&s.working_directory),
        (Id::What, _, Body::Mount(m)) => m.what.clone(),
        (Id::Where, _, Body::Mount(m)) => m.where_.clone(),
        _ => String::new(),
    }
}

fn opt_or_empty(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn set_path_field(u: &mut Unit, id: Id, v: String) {
    match id {
        Id::ExecStart => service_mut(u).exec_start = v,
        Id::ExecStartPre => service_mut(u).exec_start_pre = Some(v),
        Id::ExecStopPost => service_mut(u).exec_stop_post = Some(v),
        Id::WorkDir => service_mut(u).working_directory = Some(v),
        Id::What => {
            if let Body::Mount(m) = &mut u.body {
                m.what = v;
            }
        }
        Id::Where => {
            if let Body::Mount(m) = &mut u.body {
                m.where_ = v;
                // The unit filename is derived from the mount point.
                u.name = u.stem().unwrap_or_default();
            }
        }
        _ => {}
    }
}

/// Apply a picked path to the value a field already held.
fn fill_value(fill: Fill, current: &str, picked: &str) -> String {
    match fill {
        Fill::Whole => picked.to_string(),
        Fill::Program => picker::join_command(picked, &picker::split_command(current).1),
    }
}

/// `b` on a path row: browse for it, then write the result back. Escaping the
/// picker leaves the field exactly as it was.
fn browse_field(term: &mut Term, bg: Background, u: &mut Unit, id: Id) -> Option<String> {
    let (label, mode, fill) = browsable(u, id)?;
    let current = path_field(u, id);
    let seed = match fill {
        Fill::Program => picker::split_command(&current).0,
        Fill::Whole => current.clone(),
    };
    let picked = picker::browse(term, bg, label, mode, &seed)?;
    set_path_field(u, id, fill_value(fill, &current, &picked));
    Some(picked)
}

fn manual_field(u: &Unit, id: Id) -> &str {
    match (&u.body, id) {
        (Body::Timer(t), Id::ManualPrimary) => &t.service_manual,
        (Body::Timer(t), _) => &t.timer_manual,
        (Body::Service(s), _) => &s.manual,
        (Body::Mount(m), Id::ManualPrimary) => &m.manual,
        (Body::Mount(m), _) => &m.automount_manual,
    }
}

fn manual_field_mut(u: &mut Unit, id: Id) -> &mut String {
    match (&mut u.body, id) {
        (Body::Timer(t), Id::ManualPrimary) => &mut t.service_manual,
        (Body::Timer(t), _) => &mut t.timer_manual,
        (Body::Service(s), _) => &mut s.manual,
        (Body::Mount(m), Id::ManualPrimary) => &mut m.manual,
        (Body::Mount(m), _) => &mut m.automount_manual,
    }
}

/// The `[Service]` block of whichever body this unit has. Mounts have none,
/// but no service row is ever shown for them, so the fallback is unreachable
/// in practice and still safe.
fn service_mut(u: &mut Unit) -> &mut ServiceOpts {
    match &mut u.body {
        Body::Timer(t) => &mut t.service,
        Body::Service(s) => &mut s.service,
        Body::Mount(_) => unreachable!("mount units have no [Service] section"),
    }
}

/// Turn `/bin/sh -c "cmd"` back into `cmd`.
fn unwrap_shell(exec: &str) -> String {
    let rest = exec.trim_start_matches("/bin/sh -c ").trim();
    if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
        let inner = &rest[1..rest.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut esc = false;
        for c in inner.chars() {
            if esc {
                out.push(c);
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else {
                out.push(c);
            }
        }
        out
    } else {
        rest.to_string()
    }
}

fn ask_optional(
    term: &mut Term,
    bg: Background,
    title: &str,
    help: &str,
    cur: &Option<String>,
) -> Option<String> {
    let start = cur.clone().unwrap_or_default();
    match dialogs::prompt(term, bg, title, help, &start, &dialogs::no_validation) {
        Some(v) if v.trim().is_empty() => None,
        Some(v) => Some(v),
        None => cur.clone(),
    }
}

fn ask_timespan(
    term: &mut Term,
    bg: Background,
    title: &str,
    help: &str,
    cur: &Option<String>,
) -> Option<String> {
    let start = cur.clone().unwrap_or_default();
    let validate = |s: &str| {
        if s.trim().is_empty() {
            Ok(())
        } else {
            systemd::check_timespan(s.trim())
        }
    };
    match dialogs::prompt(term, bg, title, help, &start, &validate) {
        Some(v) if v.trim().is_empty() => None,
        Some(v) => Some(v.trim().to_string()),
        None => cur.clone(),
    }
}

// ---------------------------------------------------------------------------
// Schedule sub-builder
// ---------------------------------------------------------------------------

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Ask for a schedule, replacing `schedule` and `source` if the user commits.
fn edit_schedule(term: &mut Term, bg: Background, schedule: &mut Schedule, source: &mut String) {
    let items: Vec<String> = [
        "Every N minutes",
        "Every N hours",
        "Daily at HH:MM",
        "Weekly on a weekday at HH:MM",
        "Monthly on a day of the month at HH:MM",
        "At boot, after a delay",
        "Cron expression (translated to OnCalendar)",
        "Raw OnCalendar spec",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let Some(choice) = dialogs::pick(term, bg, "When should it run?", &items, 0) else {
        return;
    };

    let calendar = |specs: Vec<String>, src: String| Some((Schedule::Calendar(specs), src));

    let result: Option<(Schedule, String)> = match choice {
        0 => ask_count(term, bg, "Every how many minutes?", 15, 1, 1440).map(|n| {
            (
                Schedule::Every {
                    every: format!("{n}min"),
                    boot: format!("{n}min"),
                },
                format!("every {n} minute(s)"),
            )
        }),
        1 => ask_count(term, bg, "Every how many hours?", 6, 1, 168).map(|n| {
            (
                Schedule::Every {
                    every: format!("{n}h"),
                    boot: format!("{n}h"),
                },
                format!("every {n} hour(s)"),
            )
        }),
        2 => ask_time(term, bg).and_then(|(h, m)| {
            calendar(
                vec![format!("*-*-* {h:02}:{m:02}:00")],
                format!("daily at {h:02}:{m:02}"),
            )
        }),
        3 => {
            let days: Vec<String> = WEEKDAYS.iter().map(|d| d.to_string()).collect();
            dialogs::pick(term, bg, "Which weekday?", &days, 0).and_then(|d| {
                ask_time(term, bg).and_then(|(h, m)| {
                    calendar(
                        vec![format!("{} *-*-* {h:02}:{m:02}:00", WEEKDAYS[d])],
                        format!("weekly on {} at {h:02}:{m:02}", WEEKDAYS[d]),
                    )
                })
            })
        }
        4 => ask_count(term, bg, "On which day of the month?", 1, 1, 31).and_then(|d| {
            ask_time(term, bg).and_then(|(h, m)| {
                calendar(
                    vec![format!("*-*-{d:02} {h:02}:{m:02}:00")],
                    format!("monthly on day {d} at {h:02}:{m:02}"),
                )
            })
        }),
        5 => ask_timespan(
            term,
            bg,
            "Delay after boot",
            "OnBootSec=, e.g. 1min, 30s, 2h.",
            &Some("1min".into()),
        )
        .map(|b| (Schedule::Boot { boot: b.clone() }, format!("at boot + {b}"))),
        6 => dialogs::prompt(
            term,
            bg,
            "Cron expression",
            "5 fields (minute hour day-of-month month day-of-week), or @daily etc.",
            "",
            &|s: &str| cron::to_calendar(s).map(|_| ()).map_err(|e| e.to_string()),
        )
        .and_then(|expr| match cron::to_calendar(&expr) {
            Ok(Translation::Calendar(specs)) => calendar(specs, format!("cron: {}", expr.trim())),
            Ok(Translation::Reboot) => Some((
                Schedule::Boot {
                    boot: "1min".into(),
                },
                "@reboot".to_string(),
            )),
            // Unreachable: the prompt validator already rejected it.
            Err(_) => None,
        }),
        _ => dialogs::prompt(
            term,
            bg,
            "OnCalendar",
            "A systemd calendar spec, e.g. Mon..Fri *-*-* 09:00:00.",
            schedule
                .calendars()
                .first()
                .map(String::as_str)
                .unwrap_or(""),
            &|s: &str| systemd::check_calendar(s).map(|_| ()),
        )
        .and_then(|spec| calendar(vec![spec.clone()], format!("OnCalendar: {spec}"))),
    };

    if let Some((s, src)) = result {
        // Every generated calendar spec is checked before it reaches a file.
        if let Schedule::Calendar(specs) = &s {
            for c in specs {
                if let Err(e) = systemd::check_calendar(c) {
                    dialogs::msgbox(term, bg, "Invalid calendar spec", &e);
                    return;
                }
            }
        }
        *schedule = s;
        *source = src;
    }
}

fn ask_count(
    term: &mut Term,
    bg: Background,
    title: &str,
    default: u32,
    lo: u32,
    hi: u32,
) -> Option<u32> {
    let validate = |s: &str| match s.trim().parse::<u32>() {
        Ok(n) if n >= lo && n <= hi => Ok(()),
        Ok(_) => Err(format!("must be between {lo} and {hi}")),
        Err(_) => Err("must be a whole number".into()),
    };
    dialogs::prompt(
        term,
        bg,
        title,
        &format!("A number between {lo} and {hi}."),
        &default.to_string(),
        &validate,
    )
    .and_then(|s| s.trim().parse().ok())
}

/// Ask for a time of day, returning `(hour, minute)`.
fn ask_time(term: &mut Term, bg: Background) -> Option<(u32, u32)> {
    let validate = |s: &str| parse_hhmm(s).map(|_| ());
    dialogs::prompt(
        term,
        bg,
        "Time of day",
        "24-hour HH:MM, e.g. 03:30.",
        "03:00",
        &validate,
    )
    .and_then(|s| parse_hhmm(&s).ok())
}

fn parse_hhmm(s: &str) -> Result<(u32, u32), String> {
    let (h, m) = s
        .trim()
        .split_once(':')
        .ok_or_else(|| "use HH:MM".to_string())?;
    let h: u32 = h.trim().parse().map_err(|_| "hour must be a number")?;
    let m: u32 = m.trim().parse().map_err(|_| "minute must be a number")?;
    if h > 23 {
        return Err("hour must be 0-23".into());
    }
    if m > 59 {
        return Err("minute must be 0-59".into());
    }
    Ok((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_are_skipped_when_moving() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "x".into();
        let rows = rows_for(&u);
        assert_eq!(rows[0].id, Id::Heading);
        let first = first_selectable(&rows);
        assert_eq!(rows[first].id, Id::Name);
        // Stepping never lands on a heading, in either direction.
        let mut i = first;
        for _ in 0..(rows.len() * 2) {
            i = step(&rows, i, 1);
            assert_ne!(rows[i].id, Id::Heading);
        }
        for _ in 0..(rows.len() * 2) {
            i = step(&rows, i, -1);
            assert_ne!(rows[i].id, Id::Heading);
        }
    }

    #[test]
    fn every_body_offers_preview_and_save() {
        for u in [
            Unit::new_timer(Scope::User),
            Unit::new_service(Scope::User),
            Unit::new_mount(),
        ] {
            let rows = rows_for(&u);
            assert!(rows.iter().any(|r| r.id == Id::Preview));
            assert!(rows.iter().any(|r| r.id == Id::Save));
            assert!(rows.iter().any(|r| r.id == Id::ManualPrimary));
        }
    }

    #[test]
    fn only_the_relevant_rows_appear_per_body() {
        let ids = |u: &Unit| rows_for(u).into_iter().map(|r| r.id).collect::<Vec<_>>();
        let timer = ids(&Unit::new_timer(Scope::User));
        assert!(timer.contains(&Id::Schedule) && timer.contains(&Id::Persistent));
        assert!(!timer.contains(&Id::Restart) && !timer.contains(&Id::What));

        let svc = ids(&Unit::new_service(Scope::User));
        assert!(svc.contains(&Id::Restart) && svc.contains(&Id::WantedBy));
        assert!(!svc.contains(&Id::Schedule));

        let mount = ids(&Unit::new_mount());
        assert!(mount.contains(&Id::What) && mount.contains(&Id::Automount));
        assert!(!mount.contains(&Id::ExecStart));
    }

    #[test]
    fn shell_wrapping_round_trips() {
        let cmd = "df -h | mail -s \"disk\" me@example.com";
        let wrapped = format!("/bin/sh -c {}", escape::exec_quote(cmd));
        assert_eq!(unwrap_shell(&wrapped), cmd);
        // An unquoted wrapper argument is handled too.
        assert_eq!(unwrap_shell("/bin/sh -c /bin/true"), "/bin/true");
    }

    #[test]
    fn hhmm_parsing_rejects_nonsense() {
        assert_eq!(parse_hhmm("03:30"), Ok((3, 30)));
        assert_eq!(parse_hhmm(" 3:5 "), Ok((3, 5)));
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("12:60").is_err());
        assert!(parse_hhmm("1230").is_err());
        assert!(parse_hhmm("ab:cd").is_err());
    }

    #[test]
    fn truncation_fits_the_column() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd\u{2026}");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn manual_summaries_are_compact() {
        assert_eq!(summarize(""), "(none)");
        assert_eq!(summarize("Nice=19"), "Nice=19");
        assert_eq!(summarize("[Service]\nNice=19"), "(2 lines)");
    }

    #[test]
    fn every_path_field_is_browsable_with_the_right_mode() {
        use picker::Mode;
        let timer = Unit::new_timer(Scope::User);
        for (id, mode, fill) in [
            (Id::ExecStart, Mode::File, Fill::Program),
            (Id::ExecStartPre, Mode::File, Fill::Program),
            (Id::ExecStopPost, Mode::File, Fill::Program),
            (Id::WorkDir, Mode::Directory, Fill::Whole),
        ] {
            let got = browsable(&timer, id).unwrap_or_else(|| panic!("{id:?} must browse"));
            assert_eq!((got.1, got.2), (mode, fill), "{id:?}");
        }
        // The same service fields on a standalone service, not just a timer.
        let svc = Unit::new_service(Scope::User);
        assert!(browsable(&svc, Id::ExecStart).is_some());
        assert!(browsable(&svc, Id::WorkDir).is_some());

        // A mount's Where is always a directory.
        let mount = Unit::new_mount();
        assert_eq!(
            browsable(&mount, Id::Where).map(|b| b.1),
            Some(Mode::Directory)
        );
    }

    /// Every row the builder can show is either browsable or explicitly not;
    /// nothing holding a path may be missed.
    #[test]
    fn no_path_row_is_left_without_a_picker() {
        let path_rows = [
            Id::ExecStart,
            Id::ExecStartPre,
            Id::ExecStopPost,
            Id::WorkDir,
            Id::What,
            Id::Where,
        ];
        for u in [
            Unit::new_timer(Scope::User),
            Unit::new_service(Scope::User),
            Unit::new_mount(),
        ] {
            for r in rows_for(&u) {
                if path_rows.contains(&r.id) {
                    continue;
                }
                assert!(
                    browsable(&u, r.id).is_none(),
                    "{:?} browses unexpectedly",
                    r.id
                );
            }
        }
    }

    #[test]
    fn mount_what_browses_only_when_it_is_a_local_path() {
        let mut u = Unit::new_mount();
        for (preset, browses) in [
            (MountPreset::Block, true),
            (MountPreset::Bind, true),
            (MountPreset::Nfs, false),
            (MountPreset::Cifs, false),
        ] {
            if let Body::Mount(m) = &mut u.body {
                m.preset = preset;
            }
            assert_eq!(
                browsable(&u, Id::What).is_some(),
                browses,
                "{}",
                preset.label()
            );
        }
    }

    #[test]
    fn picking_a_binary_keeps_the_arguments() {
        assert_eq!(
            fill_value(Fill::Program, "/bin/old -a --b c", "/usr/bin/new"),
            "/usr/bin/new -a --b c"
        );
        assert_eq!(
            fill_value(Fill::Program, "/bin/old", "/bin/new"),
            "/bin/new"
        );
        assert_eq!(fill_value(Fill::Program, "", "/bin/new"), "/bin/new");
        // A whole-value field ignores whatever was there.
        assert_eq!(fill_value(Fill::Whole, "/old/dir", "/new/dir"), "/new/dir");
    }

    #[test]
    fn path_fields_round_trip_through_the_accessors() {
        let mut u = Unit::new_timer(Scope::User);
        for id in [
            Id::ExecStart,
            Id::ExecStartPre,
            Id::ExecStopPost,
            Id::WorkDir,
        ] {
            set_path_field(&mut u, id, "/opt/thing".into());
            assert_eq!(path_field(&u, id), "/opt/thing", "{id:?}");
        }
        let mut m = Unit::new_mount();
        set_path_field(&mut m, Id::What, "/dev/sdb1".into());
        assert_eq!(path_field(&m, Id::What), "/dev/sdb1");
        // Where re-derives the unit name, exactly as typing it does.
        set_path_field(&mut m, Id::Where, "/mnt/backup".into());
        assert_eq!(path_field(&m, Id::Where), "/mnt/backup");
        assert_eq!(m.name, m.stem().expect("stem"));
        assert!(
            m.name.contains("mnt"),
            "name follows the mount point: {}",
            m.name
        );
    }

    #[test]
    fn status_line_reports_validation_failures_first() {
        let u = Unit::new_timer(Scope::User);
        assert!(status_line(&u).starts_with('!'));
    }
}
