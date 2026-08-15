//! The main screen: every unit notcron manages in the current scope, with
//! lifecycle actions and the entry point into the builder.

use super::builder;
use super::dialogs;
use super::term::{Key, Term};
use crate::systemd::{self, Entry};
use crate::unit::model::{Scope, Unit};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};

const HELP: &str = "\
Navigation
  Up/Down, j/k     move            u        switch user <-> system scope
  Enter or e       edit unit       f        show/hide foreign units
  n                new unit        r        systemctl daemon-reload
  q or Esc         quit            ?        this help

Lifecycle (selected unit)
  s   start                S   stop
  a   enable               d   disable
  x   remove (stop, disable, delete files, daemon-reload)

Inspect
  i   systemctl status     l   journal
  v   view the raw unit files

Foreign units -- anything without notcron's marker comment -- are shown
greyed out. They can be inspected but never edited or removed.";

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
            "{}\nn new  Enter edit  s start  S stop  a enable  d disable  x remove  \
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
                _ if k.is_char('?') => dialogs::pager(term, &mut bg, "Help", HELP),
                _ if k.is_char('n') => new_unit(term, &mut bg, &mut app),
                _ if k.is_char('e') => edit_selected(term, &mut bg, &mut app),
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
        "Timer + service   -- run a command on a schedule".to_string(),
        "Service           -- a standalone service unit".to_string(),
        "Mount / automount -- a filesystem mount (system scope)".to_string(),
    ];
    let Some(kind) = dialogs::pick(term, bg, "What do you want to build?", &items, 0) else {
        return;
    };
    let mut u = match kind {
        0 => Unit::new_timer(app.scope),
        1 => Unit::new_service(app.scope),
        _ => Unit::new_mount(),
    };
    let title = match kind {
        0 => "New timer",
        1 => "New service",
        _ => "New mount",
    };
    if builder::run(term, bg, &mut u, title) {
        app.scope = u.scope;
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
}
