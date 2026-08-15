//! The main screen: every unit notcron manages in the current scope, with
//! lifecycle actions and the entry point into the builder.

use super::builder;
use super::dialogs;
use super::term::{Key, Term};
use crate::fieldhelp;
use crate::systemd::{self, Entry};
use crate::templates::{self, TemplateId};
use crate::unit::model::{Body, Scope, Unit};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};

const NAV_HELP: &str = "\
Navigation
  Up/Down, j/k     move            u        switch user <-> system scope
  Enter or e       edit unit       f        show/hide foreign units
  n                new unit        r        systemctl daemon-reload
  y                duplicate       ?        this help
  q or Esc         quit

Foreign units -- anything without notcron's marker comment -- are shown
greyed out. They can be inspected but never edited or removed.
";

/// Key -> help-document entry for every lifecycle action on this screen.
/// A test asserts each one resolves, so the keys and the document cannot
/// drift apart.
pub const LIFECYCLE_KEYS: [(&str, &str); 9] = [
    ("s", "lifecycle.start"),
    ("S", "lifecycle.stop"),
    ("a", "lifecycle.enable"),
    ("d", "lifecycle.disable"),
    ("x", "lifecycle.remove"),
    ("i", "lifecycle.status"),
    ("l", "lifecycle.logs"),
    ("v", "lifecycle.view"),
    ("r", "lifecycle.daemon_reload"),
];

/// The `?` help: navigation, then every lifecycle action described by
/// `docs/field-help.md` rather than by a second copy of the same sentences.
pub fn help_text() -> String {
    let mut out = String::from(NAV_HELP);
    out.push_str("\nActions on the selected unit\n");
    for (key, help) in LIFECYCLE_KEYS {
        let Some(e) = fieldhelp::entry(help) else {
            continue;
        };
        out.push_str(&format!("\n  {key}   {} -- {}\n", e.label, e.summary));
        for line in builder::wrap_lines(&e.detail, 68) {
            out.push_str(&format!("      {line}\n"));
        }
    }
    out
}

pub struct App {
    pub scope: Scope,
    pub show_foreign: bool,
    pub entries: Vec<Entry>,
    pub sel: usize,
    pub top: usize,
    pub status: String,
}

impl App {
    pub fn new(scope: Scope) -> App {
        let mut app = App {
            scope,
            show_foreign: false,
            entries: Vec::new(),
            sel: 0,
            top: 0,
            status: String::new(),
        };
        app.reload();
        app
    }

    pub fn reload(&mut self) {
        match systemd::list(self.scope, self.show_foreign) {
            Ok(e) => {
                self.entries = e;
                if self.sel >= self.entries.len() {
                    self.sel = self.entries.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.entries.clear();
                self.status = e;
            }
        }
    }

    fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.sel)
    }
}

fn state_style(e: &Entry) -> Style {
    if !e.owned {
        Style::new().fg(Color::DarkGray)
    } else if e.active == "active" || e.active == "activating" {
        Style::new().fg(Color::Green)
    } else if e.active == "failed" {
        Style::new().fg(Color::Red)
    } else {
        Style::new()
    }
}

/// Paint the list. Split out so `--self-check` can render one frame without
/// entering the event loop.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(area);

    let dir = systemd::unit_dir(app.scope);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" notcron ", Style::new().bold().reversed()),
            Span::raw(format!(
                " {} scope -- {}{}",
                app.scope.as_str(),
                dir.display(),
                if app.show_foreign {
                    "  [showing foreign units]"
                } else {
                    ""
                }
            )),
        ])),
        chunks[0],
    );

    let block = Block::default()
        .title(format!(" Units ({}) ", app.entries.len()))
        .title_style(Style::new().bold())
        .borders(Borders::ALL);
    let inner = block.inner(chunks[1]);
    f.render_widget(block, chunks[1]);

    if app.entries.is_empty() {
        f.render_widget(
            Paragraph::new(
                "\n  Nothing here yet.\n\n  Press n to build a timer, service or mount.\n  \
                 Press f to include units notcron does not own.",
            )
            .wrap(Wrap { trim: false }),
            inner,
        );
    } else {
        let visible = inner.height.max(1) as usize;
        let top = app.top.min(app.sel);
        let top = if app.sel >= top + visible {
            app.sel + 1 - visible
        } else {
            top
        };
        let width = inner.width as usize;
        let lines: Vec<Line> = app
            .entries
            .iter()
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(i, e)| {
                let text = format!(
                    " {} {:<34} {:<10} {:<9} {:<9} {}",
                    if i == app.sel { ">" } else { " " },
                    clip(&e.primary, 34),
                    e.kind,
                    if e.active.is_empty() { "-" } else { &e.active },
                    if e.enabled.is_empty() {
                        "-"
                    } else {
                        &e.enabled
                    },
                    if e.schedule.is_empty() {
                        e.description.clone()
                    } else {
                        e.schedule.clone()
                    }
                );
                let style = state_style(e);
                Line::from(Span::styled(
                    clip(&text, width),
                    if i == app.sel {
                        style.bold().reversed()
                    } else {
                        style
                    },
                ))
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    let hint = match app.entries.get(app.sel) {
        Some(e) if !e.owned => "read-only: not created by notcron".to_string(),
        Some(e) => e.description.clone(),
        None => String::new(),
    };
    f.render_widget(
        Paragraph::new(format!(
            "{}\nn new  y copy  Enter edit  s start  S stop  a enable  d disable  x remove  \
             i status  l logs  v view  u scope  f foreign  r reload  ? help  q quit",
            if app.status.is_empty() {
                hint
            } else {
                app.status.clone()
            }
        ))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL)),
        chunks[2],
    );
}

fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w).collect()
    }
}

/// Run the main loop until the user quits.
pub fn run(term: &mut Term, scope: Scope) {
    let mut app = App::new(scope);

    loop {
        let snapshot = |a: &App| {
            let scope = a.scope;
            let show_foreign = a.show_foreign;
            let entries = a.entries.clone();
            let sel = a.sel;
            let top = a.top;
            let status = a.status.clone();
            App {
                scope,
                show_foreign,
                entries,
                sel,
                top,
                status,
            }
        };
        let frozen = snapshot(&app);
        let mut bg = move |f: &mut Frame| draw(f, &frozen);
        let _ = term.terminal.draw(|f| bg(f));

        let Some(k) = term.next_key() else { return };
        app.status.clear();
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => move_sel(&mut app, d as isize),
            _ => match k.code() {
                Some(KeyCode::Up) => move_sel(&mut app, -1),
                Some(KeyCode::Down) => move_sel(&mut app, 1),
                Some(KeyCode::Home) => app.sel = 0,
                Some(KeyCode::End) => app.sel = app.entries.len().saturating_sub(1),
                Some(KeyCode::Esc) => return,
                Some(KeyCode::Enter) => edit_selected(term, &mut bg, &mut app),
                _ if k.is_char('q') => return,
                _ if k.is_char('k') => move_sel(&mut app, -1),
                _ if k.is_char('j') => move_sel(&mut app, 1),
                _ if k.is_char('?') => dialogs::pager(term, &mut bg, "Help", &help_text()),
                _ if k.is_char('n') => new_unit(term, &mut bg, &mut app),
                _ if k.is_char('e') => edit_selected(term, &mut bg, &mut app),
                _ if k.is_char('y') => duplicate_selected(term, &mut bg, &mut app),
                _ if k.is_char('u') => {
                    app.scope = if app.scope == Scope::User {
                        Scope::System
                    } else {
                        Scope::User
                    };
                    app.sel = 0;
                    app.reload();
                }
                _ if k.is_char('f') => {
                    app.show_foreign = !app.show_foreign;
                    app.sel = 0;
                    app.reload();
                }
                _ if k.is_char('r') => {
                    app.status = match systemd::daemon_reload(app.scope) {
                        Ok(_) => "daemon-reload done".into(),
                        Err(e) => e.trim().to_string(),
                    };
                    app.reload();
                }
                _ if k.is_char('s') => lifecycle(term, &mut bg, &mut app, &["start"]),
                _ if k.is_char('S') => lifecycle(term, &mut bg, &mut app, &["stop"]),
                _ if k.is_char('a') => lifecycle(term, &mut bg, &mut app, &["enable"]),
                _ if k.is_char('d') => lifecycle(term, &mut bg, &mut app, &["disable"]),
                _ if k.is_char('x') => remove_selected(term, &mut bg, &mut app),
                _ if k.is_char('i') => show_status(term, &mut bg, &app),
                _ if k.is_char('l') => show_logs(term, &mut bg, &app),
                _ if k.is_char('v') => view_files(term, &mut bg, &app),
                _ => {}
            },
        }
    }
}

fn move_sel(app: &mut App, delta: isize) {
    if app.entries.is_empty() {
        return;
    }
    let n = app.entries.len() as isize;
    app.sel = ((app.sel as isize + delta).rem_euclid(n)) as usize;
}

fn new_unit(term: &mut Term, bg: dialogs::Background, app: &mut App) {
    let items = vec![
        "Timer + service    -- run a command on a schedule".to_string(),
        "Service            -- a standalone service unit".to_string(),
        "Mount / automount  -- a filesystem mount (system scope)".to_string(),
        "From a template    -- a working job to edit rather than invent".to_string(),
        "From an existing unit -- copy one of yours under a new name".to_string(),
    ];
    let Some(kind) = dialogs::pick(term, bg, "What do you want to build?", &items, 0) else {
        return;
    };
    let (mut u, title) = match kind {
        0 => (Unit::new_timer(app.scope), "New timer".to_string()),
        1 => (Unit::new_service(app.scope), "New service".to_string()),
        2 => (Unit::new_mount(), "New mount".to_string()),
        3 => match pick_template(term, bg, app.scope) {
            Some(u) => {
                let t = format!("New from template: {}", u.name);
                (u, t)
            }
            None => return,
        },
        _ => match pick_clone(term, bg, app) {
            Some(u) => (u, "New from existing".to_string()),
            None => return,
        },
    };
    if builder::run(term, bg, &mut u, &title) {
        app.scope = u.scope;
        app.reload();
    }
}

/// The template chooser. Each entry is a unit that would already work.
fn pick_template(term: &mut Term, bg: dialogs::Background, scope: Scope) -> Option<Unit> {
    let items: Vec<String> = TemplateId::ALL
        .iter()
        .map(|t| format!("{:<16} {}", t.label(), t.detail()))
        .collect();
    let i = dialogs::pick(term, bg, "Start from a template", &items, 0)?;
    Some(TemplateId::ALL[i].build(scope))
}

/// The names a clone must not collide with: unit names for timers and
/// services, mount points for mounts, since a mount's filename comes from
/// `Where=`.
fn taken_names(app: &App, mount: bool) -> Vec<String> {
    app.entries
        .iter()
        .filter_map(|e| match (e.unit.as_ref().map(|u| &u.body), mount) {
            (Some(Body::Mount(m)), true) => Some(m.where_.clone()),
            (Some(Body::Mount(_)), false) | (None, _) => None,
            (Some(_), true) => None,
            (Some(_), false) => e.unit.as_ref().map(|u| u.name.clone()),
        })
        .collect()
}

/// "New from existing": copy an owned unit into the builder under a fresh
/// name. Nothing about the original's installed state comes with it -- the
/// copy is not installed until it is saved.
fn pick_clone(term: &mut Term, bg: dialogs::Background, app: &App) -> Option<Unit> {
    let owned: Vec<&Entry> = app
        .entries
        .iter()
        .filter(|e| e.owned && e.unit.is_some())
        .collect();
    if owned.is_empty() {
        dialogs::msgbox(
            term,
            bg,
            "Nothing to copy",
            "There are no notcron-managed units in this scope to copy.",
        );
        return None;
    }
    let items: Vec<String> = owned
        .iter()
        .map(|e| format!("{:<34} {}", e.primary, e.description))
        .collect();
    let i = dialogs::pick(term, bg, "Copy which unit?", &items, 0)?;
    let src = owned[i].unit.as_ref()?;
    let mount = matches!(src.body, Body::Mount(_));
    Some(templates::clone_unit(src, &taken_names(app, mount)))
}

/// `y` on the list: the same copy, without going through the new-unit menu.
fn duplicate_selected(term: &mut Term, bg: dialogs::Background, app: &mut App) {
    let Some(e) = app.selected() else { return };
    if !e.owned {
        app.status = format!(
            "{} was not created by notcron and cannot be copied",
            e.primary
        );
        return;
    }
    let Some(src) = e.unit.clone() else {
        app.status = format!("{} could not be parsed into the builder's model", e.primary);
        return;
    };
    let mount = matches!(src.body, Body::Mount(_));
    let mut copy = templates::clone_unit(&src, &taken_names(app, mount));
    if builder::run(term, bg, &mut copy, "Copy of an existing unit") {
        app.scope = copy.scope;
        app.reload();
    }
}

fn edit_selected(term: &mut Term, bg: dialogs::Background, app: &mut App) {
    let Some(e) = app.selected() else { return };
    if !e.owned {
        dialogs::msgbox(
            term,
            bg,
            "Read-only",
            &format!(
                "{} was not created by notcron.\n\nOnly units carrying notcron's marker \
                 comment can be edited or removed. Press v to view it.",
                e.primary
            ),
        );
        return;
    }
    let Some(mut u) = e.unit.clone() else {
        dialogs::msgbox(
            term,
            bg,
            "Cannot edit",
            &format!(
                "{} could not be parsed into the builder's model.",
                e.primary
            ),
        );
        return;
    };
    let title = format!("Edit {}", e.primary);
    let old_files = e.files.clone();
    let old_scope = e.scope;
    if builder::run(term, bg, &mut u, &title) {
        // A rename or a scope change leaves the old files behind; clean up.
        let new_files = u.filenames().unwrap_or_default();
        if old_scope != u.scope || new_files != old_files {
            let _ = systemd::remove(old_scope, &old_files);
        }
        app.scope = u.scope;
        app.reload();
    }
}

fn lifecycle(term: &mut Term, bg: dialogs::Background, app: &mut App, args: &[&str]) {
    let Some(e) = app.selected() else { return };
    if !e.owned {
        app.status = format!("{} is not managed by notcron", e.primary);
        return;
    }
    let mut full: Vec<&str> = args.to_vec();
    full.push(&e.primary);
    let verb = args[0];
    let name = e.primary.clone();
    let scope = e.scope;
    match systemd::systemctl(scope, &full) {
        Ok(_) => app.status = format!("{verb} {name}: ok"),
        Err(err) => dialogs::msgbox(term, bg, &format!("{verb} failed"), err.trim()),
    }
    app.reload();
}

fn remove_selected(term: &mut Term, bg: dialogs::Background, app: &mut App) {
    let Some(e) = app.selected() else { return };
    if !e.owned {
        dialogs::msgbox(
            term,
            bg,
            "Read-only",
            &format!(
                "{} was not created by notcron and will not be removed.",
                e.primary
            ),
        );
        return;
    }
    let (scope, files, name) = (e.scope, e.files.clone(), e.primary.clone());
    let body = format!(
        "Stop, disable and delete {name}?\n\nFiles:\n  {}",
        files.join("\n  ")
    );
    if !dialogs::confirm(term, bg, "Remove unit", &body) {
        return;
    }
    match systemd::remove(scope, &files) {
        Ok(()) => app.status = format!("removed {name}"),
        Err(err) => dialogs::msgbox(term, bg, "Remove failed", &err),
    }
    app.reload();
}

fn show_status(term: &mut Term, bg: dialogs::Background, app: &App) {
    let Some(e) = app.selected() else { return };
    let mut out = String::new();
    for f in &e.files {
        out.push_str(&systemd::systemctl_lossy(
            e.scope,
            &["status", "--no-pager", "--full", f],
        ));
        out.push_str("\n\n");
    }
    dialogs::pager(term, bg, &format!("status: {}", e.primary), out.trim_end());
}

fn show_logs(term: &mut Term, bg: dialogs::Background, app: &App) {
    let Some(e) = app.selected() else { return };
    let unit = match &e.unit {
        Some(u) => u.log_unit().unwrap_or_else(|_| e.primary.clone()),
        None => e.primary.clone(),
    };
    let out = systemd::journal(e.scope, &unit, 500);
    dialogs::pager(term, bg, &format!("journal: {unit}"), &out);
}

fn view_files(term: &mut Term, bg: dialogs::Background, app: &App) {
    let Some(e) = app.selected() else { return };
    let dir = systemd::unit_dir(e.scope);
    let mut out = String::new();
    for f in &e.files {
        let path = dir.join(f);
        out.push_str(&format!("# {}\n", path.display()));
        match std::fs::read_to_string(&path) {
            Ok(body) => out.push_str(&body),
            Err(err) => out.push_str(&format!("(cannot read: {err})\n")),
        }
        out.push('\n');
    }
    dialogs::pager(term, bg, &format!("view: {}", e.primary), out.trim_end());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn app_with(entries: Vec<Entry>) -> App {
        App {
            scope: Scope::User,
            show_foreign: false,
            entries,
            sel: 0,
            top: 0,
            status: String::new(),
        }
    }

    fn entry(name: &str, owned: bool) -> Entry {
        Entry {
            primary: name.into(),
            files: vec![name.into()],
            scope: Scope::User,
            owned,
            description: "a unit".into(),
            kind: "timer",
            schedule: "*-*-* 03:00:00".into(),
            unit: None,
            active: "active".into(),
            enabled: "enabled".into(),
        }
    }

    /// The layout must survive terminals far smaller than anyone sensible
    /// uses; ratatui panics on out-of-bounds rects rather than clipping.
    #[test]
    fn draws_at_every_size_without_panicking() {
        let app = app_with(vec![
            entry("notcron-backup.timer", true),
            entry("sshd.service", false),
        ]);
        for (w, h) in [(1, 1), (2, 3), (10, 4), (20, 5), (40, 10), (200, 60)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &app)).unwrap();
        }
    }

    #[test]
    fn draws_an_empty_list_without_panicking() {
        let app = app_with(Vec::new());
        for (w, h) in [(1, 1), (30, 6), (120, 40)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &app)).unwrap();
        }
    }

    #[test]
    fn draws_a_list_longer_than_the_viewport() {
        let mut app = app_with(
            (0..200)
                .map(|i| entry(&format!("u{i}.timer"), true))
                .collect(),
        );
        app.sel = 199;
        let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
        t.draw(|f| draw(f, &app)).unwrap();
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut app = app_with(vec![entry("a.timer", true), entry("b.timer", true)]);
        move_sel(&mut app, -1);
        assert_eq!(app.sel, 1);
        move_sel(&mut app, 1);
        assert_eq!(app.sel, 0);
    }

    #[test]
    fn moving_an_empty_selection_is_a_no_op() {
        let mut app = app_with(Vec::new());
        move_sel(&mut app, 1);
        assert_eq!(app.sel, 0);
    }

    #[test]
    fn foreign_units_are_greyed_out() {
        assert_eq!(state_style(&entry("x", false)).fg, Some(Color::DarkGray));
        assert_eq!(state_style(&entry("x", true)).fg, Some(Color::Green));
    }

    /// An entry carrying a parsed unit, for the copy flows.
    fn entry_with(name: &str, unit: Unit) -> Entry {
        Entry {
            unit: Some(unit),
            ..entry(name, true)
        }
    }

    fn named_timer(name: &str) -> Unit {
        let mut u = Unit::new_timer(Scope::User);
        u.name = name.into();
        u.description = "nightly".into();
        u
    }

    fn mount_at(where_: &str) -> Unit {
        let mut u = Unit::new_mount();
        if let Body::Mount(m) = &mut u.body {
            m.what = "/dev/sdb1".into();
            m.where_ = where_.into();
        }
        u.name = u.stem().unwrap_or_default();
        u
    }

    // -----------------------------------------------------------------
    // Help, from the document rather than a second copy of it
    // -----------------------------------------------------------------

    #[test]
    fn every_lifecycle_action_is_documented() {
        let text = help_text();
        for (key, help) in LIFECYCLE_KEYS {
            let e = fieldhelp::entry(help).unwrap_or_else(|| panic!("{key} -> {help}: missing"));
            assert!(!e.summary.is_empty(), "{help}");
            assert!(!e.detail.is_empty(), "{help}");
            assert!(
                text.contains(&e.label),
                "{help}: label absent from the help"
            );
            assert!(text.contains(&e.summary), "{help}: summary absent");
        }
        // The keys named are the keys the loop actually binds.
        for key in ["s", "S", "a", "d", "x", "i", "l", "v", "r"] {
            assert!(
                LIFECYCLE_KEYS.iter().any(|(k, _)| *k == key),
                "{key} is bound but undocumented"
            );
        }
        assert!(text.contains("y                duplicate"), "{text}");
    }

    // -----------------------------------------------------------------
    // Copying an existing unit
    // -----------------------------------------------------------------

    /// Timers and services collide on unit names; mounts collide on mount
    /// points, because that is what their filename is derived from.
    #[test]
    fn the_names_a_copy_must_avoid_depend_on_what_is_being_copied() {
        let app = app_with(vec![
            entry_with("notcron-backup.timer", named_timer("backup")),
            entry_with("notcron-sync.timer", named_timer("sync")),
            entry_with("srv-data.mount", mount_at("/srv/data")),
            entry("foreign.service", false),
        ]);
        let mut names = taken_names(&app, false);
        names.sort();
        assert_eq!(names, vec!["backup".to_string(), "sync".to_string()]);
        assert_eq!(taken_names(&app, true), vec!["/srv/data".to_string()]);
    }

    #[test]
    fn a_copied_timer_gets_a_free_name_and_no_install_state() {
        let app = app_with(vec![
            entry_with("notcron-backup.timer", named_timer("backup")),
            entry_with("notcron-backup-copy.timer", named_timer("backup-copy")),
        ]);
        let src = app.entries[0].unit.clone().expect("a unit");
        let copy = templates::clone_unit(&src, &taken_names(&app, false));
        assert_eq!(copy.name, "backup-copy-2");
        assert!(copy.description.contains("(copy)"), "{}", copy.description);
        // `Unit` models file contents only, so nothing about the original
        // being active or enabled can have come along.
        assert_eq!(copy.scope, src.scope);
        assert!(copy.filenames().is_ok());
    }

    #[test]
    fn a_copied_mount_is_renamed_by_its_mount_point() {
        let app = app_with(vec![entry_with("srv-data.mount", mount_at("/srv/data"))]);
        let src = app.entries[0].unit.clone().expect("a unit");
        let copy = templates::clone_unit(&src, &taken_names(&app, true));
        let Body::Mount(m) = &copy.body else {
            panic!("not a mount")
        };
        assert_eq!(m.where_, "/srv/data-copy");
        assert_ne!(copy.stem().unwrap(), src.stem().unwrap());
    }

    // -----------------------------------------------------------------
    // Templates
    // -----------------------------------------------------------------

    /// Every template builds a unit that would install as it stands.
    #[test]
    fn every_template_is_offered_and_valid_in_both_scopes() {
        for scope in [Scope::User, Scope::System] {
            for t in TemplateId::ALL {
                let u = t.build(scope);
                assert!(u.validate().is_ok(), "{}: {:?}", t.label(), u.validate());
                assert_eq!(u.scope, scope);
                assert!(!t.label().is_empty() && !t.detail().is_empty());
                assert!(u.filenames().is_ok(), "{}", t.label());
            }
        }
    }
}
