//! The main screen: every unit notcron manages in the current scope, with
//! lifecycle actions, health at a glance, an inline journal pane and the
//! entry point into the builder.

use super::dialogs::Background;
use super::health::{self, Health};
use super::table;
use super::term::{Key, Term};
use super::{builder, dialogs, logtail, picker, trashview};
use crate::systemd::{self, Entry};
use crate::templates::{self, TemplateId};
use crate::unit::model::{Body, Scope, Unit};
use crate::{export, fieldhelp, linger};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::time::Duration;

const NAV_HELP: &str = "\
Navigation and view
  Up/Down, j/k     move            u        switch user <-> system scope
  Enter or e       edit unit       F        show/hide foreign units
  n                new unit        r        systemctl daemon-reload
  y                duplicate       ?        this help
  /                filter          Esc      clear the filter, then quit
  q                quit

The journal pane
  t                open or close the pane for the selected unit
  f                follow it live (press f again to stop)

`f` follows the journal because that is where a follow belongs; the
foreign-unit toggle it used to hold moved to shift-F.

Health columns
  LAST is the outcome of the last run -- ok, or how it failed, in red.
  NEXT is when the timer fires next. Failed units are sorted to the top.
  Columns are dropped as the terminal narrows rather than wrapped.

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

/// Actions with no entry in `docs/field-help.md`, described here instead.
const EXTRA_HELP: [(&str, &str); 3] = [
    (
        "R",
        "Run now -- start the selected unit's service immediately, without \
         waiting for its schedule. The timer is left alone, so the next \
         scheduled run still happens.",
    ),
    (
        "E",
        "Export -- write the selected unit's files to a directory of your \
         choosing, or show them. An existing file is never replaced without \
         being named first.",
    ),
    (
        "U",
        "Undo a removal -- browse what x put in the trash and restore it. \
         Restoring never overwrites a unit that exists again without asking.",
    ),
];

/// The `?` help: navigation, then every action described by
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
    for (key, detail) in EXTRA_HELP {
        out.push_str(&format!("\n  {key}   "));
        for (i, line) in builder::wrap_lines(detail, 68).into_iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("{line}\n"));
            } else {
                out.push_str(&format!("      {line}\n"));
            }
        }
    }
    out
}

/// Every binding the footer can show, most useful first. `? help` is always
/// kept, so the way out is never the thing that gets dropped.
pub const KEY_SEGMENTS: [&str; 17] = [
    "Enter edit",
    "n new",
    "/ filter",
    "R run",
    "t tail",
    "x remove",
    "s start",
    "S stop",
    "a enable",
    "d disable",
    "E export",
    "U undo",
    "i status",
    "l logs",
    "v view",
    "y copy",
    "u scope",
];

/// The footer's binding line, fitted to `width`.
///
/// Bindings are added until they stop fitting; `? help` and `q quit` are
/// reserved first so a narrow terminal never hides the way out or the way to
/// find everything else. Nothing is ever half-drawn.
pub fn key_line(width: usize) -> String {
    let tail = "? help  q quit";
    if width < tail.len() {
        return tail.chars().take(width).collect();
    }
    let mut out = String::new();
    for seg in KEY_SEGMENTS {
        let extra = if out.is_empty() {
            seg.len()
        } else {
            seg.len() + 2
        };
        if out.len() + extra + 2 + tail.len() > width {
            break;
        }
        if !out.is_empty() {
            out.push_str("  ");
        }
        out.push_str(seg);
    }
    if out.is_empty() {
        tail.to_string()
    } else {
        format!("{out}  {tail}")
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One line of the table: a unit and what systemd says about it.
#[derive(Debug, Clone)]
pub struct Row {
    pub entry: Entry,
    pub health: Health,
}

pub struct App {
    pub scope: Scope,
    pub show_foreign: bool,
    pub rows: Vec<Row>,
    /// Indices into `rows` that survive the filter, in display order.
    pub view: Vec<usize>,
    /// Index into `view`, not into `rows`.
    pub sel: usize,
    pub top: usize,
    pub status: String,
    pub filter: String,
    /// True while `/` is taking keystrokes.
    pub typing: bool,
    pub tail_on: bool,
    pub tail: logtail::Tail,
    /// Cached wall clock, refreshed once per redraw so every column in a
    /// frame is relative to the same instant.
    pub now: u64,
    linger_checked: bool,
}

impl App {
    pub fn new(scope: Scope) -> App {
        let mut app = App {
            scope,
            show_foreign: false,
            rows: Vec::new(),
            view: Vec::new(),
            sel: 0,
            top: 0,
            status: String::new(),
            filter: String::new(),
            typing: false,
            tail_on: false,
            tail: logtail::Tail::default(),
            now: health::now_usec(),
            linger_checked: false,
        };
        app.reload();
        app
    }

    /// Re-read the unit directory and re-ask systemd about every unit in it.
    pub fn reload(&mut self) {
        match systemd::list(self.scope, self.show_foreign) {
            Ok(entries) => {
                let names: Vec<String> = entries.iter().flat_map(|e| e.files.clone()).collect();
                let by_unit = health::fetch(self.scope, &names);
                self.rows = entries
                    .into_iter()
                    .map(|entry| {
                        let health = health::merge(&entry.files, &by_unit);
                        Row { entry, health }
                    })
                    .collect();
                // Failures first; everything else keeps its alphabetical order.
                self.rows.sort_by_key(|r| health::sort_rank(&r.health));
            }
            Err(e) => {
                self.rows.clear();
                self.status = e;
            }
        }
        self.refilter();
    }

    /// Recompute the visible subset, keeping the selection on the same unit
    /// where it still exists.
    pub fn refilter(&mut self) {
        let was = self.selected().map(|r| r.entry.primary.clone());
        self.view = (0..self.rows.len())
            .filter(|i| matches(&self.rows[*i], &self.filter))
            .collect();
        self.sel = was
            .and_then(|name| {
                self.view
                    .iter()
                    .position(|i| self.rows[*i].entry.primary == name)
            })
            .unwrap_or(0)
            .min(self.view.len().saturating_sub(1));
    }

    pub fn selected(&self) -> Option<&Row> {
        self.view.get(self.sel).and_then(|i| self.rows.get(*i))
    }

    fn entry(&self) -> Option<&Entry> {
        self.selected().map(|r| &r.entry)
    }

    /// Keep `top` such that the selection is on screen in `visible` rows.
    pub fn scroll_to_selection(&mut self, visible: usize) {
        let visible = visible.max(1);
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + visible {
            self.top = self.sel + 1 - visible;
        }
        let max_top = self.view.len().saturating_sub(visible);
        self.top = self.top.min(max_top);
    }
}

/// Does this row survive `needle`?
///
/// Case-insensitive substring over everything the row shows, so what is on
/// screen is what can be searched -- typing `failed` finds the failures and
/// typing `03:00` finds the three-in-the-morning jobs.
pub fn matches(row: &Row, needle: &str) -> bool {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    let e = &row.entry;
    let haystack = format!(
        "{} {} {} {} {} {} {}",
        e.primary,
        e.description,
        e.kind,
        e.schedule,
        e.active,
        e.enabled,
        row.health.last_label()
    )
    .to_lowercase();
    // Every whitespace-separated word must appear somewhere.
    needle.split_whitespace().all(|w| haystack.contains(w))
}

fn state_style(row: &Row) -> Style {
    let e = &row.entry;
    if !e.owned {
        Style::new().fg(Color::DarkGray)
    } else if row.health.failed() {
        Style::new().fg(Color::Red)
    } else if e.active == "active" || e.active == "activating" {
        Style::new().fg(Color::Green)
    } else {
        Style::new()
    }
}

/// The cells for one row, given the clock the frame is drawn against.
pub fn cells(row: &Row, now: u64) -> table::Cells {
    let e = &row.entry;
    table::Cells {
        name: e.primary.clone(),
        last: row.health.last_label(),
        next: row.health.next_label(now),
        state: if e.active.is_empty() {
            "-".into()
        } else {
            e.active.clone()
        },
        schedule: if e.schedule.is_empty() {
            e.description.clone()
        } else {
            e.schedule.clone()
        },
        enabled: if e.enabled.is_empty() {
            "-".into()
        } else {
            e.enabled.clone()
        },
        kind: e.kind.to_string(),
    }
}

/// The unit `R` starts: the service a timer drives, or the unit itself when
/// it is one that runs something. A bare timer with no service has nothing
/// to run, and neither does an automount.
pub fn run_target(files: &[String]) -> Option<&str> {
    files
        .iter()
        .find(|f| f.ends_with(".service"))
        .or_else(|| files.iter().find(|f| f.ends_with(".mount")))
        .map(String::as_str)
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// The rect the unit rows occupy, inside the list block's border.
pub fn list_inner(area: Rect, tail_on: bool) -> Rect {
    let panes = logtail::split(area, tail_on);
    Block::default().borders(Borders::ALL).inner(panes.list)
}

/// How many unit rows fit, once the column header has taken its line.
pub fn visible_rows(area: Rect, tail_on: bool) -> usize {
    (list_inner(area, tail_on).height as usize).saturating_sub(1)
}

/// Which row a click at `(_, y)` lands on, given the frame and the scroll
/// offset the frame was drawn with. `None` for a click outside the rows.
pub fn row_at(area: Rect, tail_on: bool, top: usize, y: u16) -> Option<usize> {
    let inner = list_inner(area, tail_on);
    // The first inner line is the column header, not a unit.
    let first = inner.y.checked_add(1)?;
    if inner.height < 2 || y < first || y >= inner.y + inner.height {
        return None;
    }
    Some(top + (y - first) as usize)
}

/// Everything a frame needs, detached from the live [`App`].
///
/// The app owns a running `journalctl` and so cannot be cloned; dialogs need
/// to repaint the list behind themselves, which needs an owned value. The
/// snapshot is that value.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub scope: Scope,
    pub show_foreign: bool,
    pub rows: Vec<Row>,
    pub view: Vec<usize>,
    pub sel: usize,
    pub top: usize,
    pub status: String,
    pub filter: String,
    pub typing: bool,
    pub tail_on: bool,
    pub tail_title: String,
    pub tail_following: bool,
    pub tail_lines: Vec<String>,
    pub tail_note: String,
    pub hint: String,
    pub now: u64,
}

impl App {
    pub fn snapshot(&self) -> Snapshot {
        let hint = match self.selected() {
            Some(r) if !r.entry.owned => "read-only: not created by notcron".to_string(),
            Some(r) => {
                let detail = r.health.detail(self.now);
                if detail.is_empty() {
                    r.entry.description.clone()
                } else {
                    detail
                }
            }
            None => String::new(),
        };
        Snapshot {
            scope: self.scope,
            show_foreign: self.show_foreign,
            rows: self.rows.clone(),
            view: self.view.clone(),
            sel: self.sel,
            top: self.top,
            status: self.status.clone(),
            filter: self.filter.clone(),
            typing: self.typing,
            tail_on: self.tail_on,
            tail_title: self.tail.title(),
            tail_following: self.tail.following(),
            tail_lines: self.tail.lines.clone(),
            tail_note: self.tail.note.clone(),
            hint,
            now: self.now,
        }
    }
}

/// Paint the list. Split out so `--self-check` can render one frame without
/// entering the event loop.
pub fn draw(f: &mut Frame, app: &Snapshot) {
    let area = f.area();
    let panes = logtail::split(area, app.tail_on);

    // Header.
    let dir = systemd::unit_dir(app.scope);
    let mut header = vec![
        Span::styled(" notcron ", Style::new().bold().reversed()),
        Span::raw(format!(
            " {} scope -- {}",
            app.scope.as_str(),
            dir.display()
        )),
    ];
    if app.show_foreign {
        header.push(Span::styled(
            "  [foreign]",
            Style::new().fg(Color::DarkGray),
        ));
    }
    if app.typing || !app.filter.is_empty() {
        header.push(Span::styled(
            format!("  /{}{}", app.filter, if app.typing { "_" } else { "" }),
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(header)), panes.header);

    // The unit table.
    let title = if app.filter.is_empty() {
        format!(" Units ({}) ", app.rows.len())
    } else {
        format!(" Units ({}/{}) ", app.view.len(), app.rows.len())
    };
    let block = Block::default()
        .title(title)
        .title_style(Style::new().bold())
        .borders(Borders::ALL);
    let inner = block.inner(panes.list);
    f.render_widget(block, panes.list);

    if app.view.is_empty() {
        let msg = if app.rows.is_empty() {
            "\n  Nothing here yet.\n\n  Press n to build a timer, service or mount.\n  \
             Press F to include units notcron does not own."
        } else {
            "\n  Nothing matches the filter.\n\n  Press Esc to clear it."
        };
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: false }), inner);
    } else {
        let plan = table::plan(inner.width as usize);
        let visible = (inner.height as usize).saturating_sub(1);
        let mut lines = vec![Line::from(Span::styled(
            table::header(&plan),
            Style::new().fg(Color::DarkGray).bold(),
        ))];
        lines.extend(
            app.view
                .iter()
                .skip(app.top)
                .take(visible)
                .enumerate()
                .map(|(offset, i)| {
                    let row = &app.rows[*i];
                    let selected = app.top + offset == app.sel;
                    let text = table::row(
                        &plan,
                        if selected { '>' } else { ' ' },
                        &cells(row, app.now),
                    );
                    let style = state_style(row);
                    Line::from(Span::styled(
                        text,
                        if selected {
                            style.bold().reversed()
                        } else {
                            style
                        },
                    ))
                }),
        );
        f.render_widget(Paragraph::new(lines), inner);
    }

    // The journal pane.
    if let Some(rect) = panes.tail {
        let block = Block::default()
            .title(app.tail_title.clone())
            .title_style(if app.tail_following {
                Style::new().fg(Color::Yellow).bold()
            } else {
                Style::new().bold()
            })
            .borders(Borders::ALL);
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let body = logtail::body(
            &app.tail_lines,
            &app.tail_note,
            inner.width as usize,
            inner.height as usize,
        )
        .join("\n");
        f.render_widget(Paragraph::new(body), inner);
    }

    // Footer: what is going on, then the bindings.
    let hint = app.hint.clone();
    let block = Block::default().borders(Borders::ALL);
    let finner = block.inner(panes.footer);
    f.render_widget(block, panes.footer);
    let width = finner.width as usize;
    let first = if app.typing {
        format!("/{}   Enter keeps it, Esc clears it", app.filter)
    } else if app.status.is_empty() {
        hint
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(clip(&first, width)),
            Line::from(Span::styled(
                clip(&key_line(width), width),
                Style::new().fg(Color::DarkGray),
            )),
        ]),
        finner,
    );
}

fn clip(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w).collect()
    }
}

// ---------------------------------------------------------------------------
// The event loop
// ---------------------------------------------------------------------------

/// How often the screen repaints while following the journal.
const FOLLOW_TICK: Duration = Duration::from_millis(250);

/// Consecutive silent ticks before a follow gives up. A terminal that has
/// died reads as a timeout rather than as EOF, so without this a follow would
/// spin at four frames a second forever.
const FOLLOW_IDLE_LIMIT: u32 = 2400;

/// Run the main loop until the user quits.
pub fn run(term: &mut Term, scope: Scope) {
    let mut app = App::new(scope);
    let mut idle = 0u32;
    {
        let frozen = app.snapshot();
        let mut bg = move |f: &mut Frame| draw(f, &frozen);
        offer_lingering(term, &mut bg, &mut app);
    }

    loop {
        app.now = health::now_usec();
        let area = term
            .terminal
            .size()
            .map(|s| Rect {
                x: 0,
                y: 0,
                width: s.width,
                height: s.height,
            })
            .unwrap_or(Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            });
        app.scroll_to_selection(visible_rows(area, app.tail_on));
        sync_tail(&mut app);

        let frozen = app.snapshot();
        let mut bg = move |f: &mut Frame| draw(f, &frozen);
        let _ = term.terminal.draw(|f| bg(f));

        let k = if app.tail.following() {
            match term.poll_key(FOLLOW_TICK) {
                Some(k) => {
                    idle = 0;
                    k
                }
                None => {
                    idle += 1;
                    if idle >= FOLLOW_IDLE_LIMIT {
                        app.tail.unfollow();
                        app.tail.note = "follow stopped after a long silence".into();
                    }
                    continue;
                }
            }
        } else {
            idle = 0;
            match term.next_key() {
                Some(k) => k,
                None => return,
            }
        };

        if app.typing && filter_key(&mut app, k) {
            continue;
        }
        app.status.clear();
        match k {
            Key::Resize => continue,
            Key::Scroll(d) => move_sel(&mut app, d as isize),
            Key::Click(_, y) | Key::DoubleClick(_, y) => {
                if let Some(i) = row_at(area, app.tail_on, app.top, y) {
                    if i < app.view.len() {
                        app.sel = i;
                        if matches!(k, Key::DoubleClick(..)) {
                            edit_selected(term, &mut bg, &mut app);
                        }
                    }
                }
            }
            _ => match k.code() {
                Some(KeyCode::Up) => move_sel(&mut app, -1),
                Some(KeyCode::Down) => move_sel(&mut app, 1),
                Some(KeyCode::Home) => app.sel = 0,
                Some(KeyCode::End) => app.sel = app.view.len().saturating_sub(1),
                Some(KeyCode::Esc) => {
                    // Esc unwinds one layer at a time: the filter, then the
                    // program. Quitting out from under a filter would lose
                    // the fact that most units were hidden.
                    if app.filter.is_empty() {
                        return;
                    }
                    app.filter.clear();
                    app.refilter();
                }
                Some(KeyCode::Enter) => edit_selected(term, &mut bg, &mut app),
                _ if k.is_char('q') => return,
                _ if k.is_char('k') => move_sel(&mut app, -1),
                _ if k.is_char('j') => move_sel(&mut app, 1),
                _ if k.is_char('/') => {
                    app.typing = true;
                }
                _ if k.is_char('?') => dialogs::pager(term, &mut bg, "Help", &help_text()),
                _ if k.is_char('n') => new_unit(term, &mut bg, &mut app),
                _ if k.is_char('e') => edit_selected(term, &mut bg, &mut app),
                _ if k.is_char('y') => duplicate_selected(term, &mut bg, &mut app),
                _ if k.is_char('t') => {
                    app.tail_on = !app.tail_on;
                    if !app.tail_on {
                        app.tail.unfollow();
                    }
                }
                _ if k.is_char('f') => {
                    // Following implies the pane: asking to follow with it
                    // shut would otherwise look like a dead key.
                    app.tail_on = true;
                    sync_tail(&mut app);
                    if app.tail.following() {
                        app.tail.unfollow();
                        app.status = "stopped following".into();
                    } else {
                        app.tail.follow();
                    }
                }
                _ if k.is_char('u') => {
                    app.scope = if app.scope == Scope::User {
                        Scope::System
                    } else {
                        Scope::User
                    };
                    app.sel = 0;
                    app.top = 0;
                    app.tail.unfollow();
                    app.tail.unit.clear();
                    app.reload();
                }
                _ if k.is_char('F') => {
                    app.show_foreign = !app.show_foreign;
                    app.sel = 0;
                    app.top = 0;
                    app.reload();
                }
                _ if k.is_char('r') => {
                    app.status = match systemd::daemon_reload(app.scope) {
                        Ok(_) => "daemon-reload done".into(),
                        Err(e) => e.trim().to_string(),
                    };
                    app.reload();
                }
                _ if k.is_char('R') => run_now(term, &mut bg, &mut app),
                _ if k.is_char('E') => export_selected(term, &mut bg, &mut app),
                _ if k.is_char('U') => {
                    if let Some(msg) = trashview::run(term, &mut bg, app.scope) {
                        app.status = msg;
                        app.reload();
                    }
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

/// Handle a keystroke while `/` is taking input. Returns true when the key
/// was consumed by the filter.
pub fn filter_key(app: &mut App, k: Key) -> bool {
    match k {
        Key::Press(KeyCode::Esc, _) => {
            app.typing = false;
            app.filter.clear();
            app.refilter();
            true
        }
        Key::Press(KeyCode::Enter, _) => {
            app.typing = false;
            true
        }
        Key::Press(KeyCode::Backspace, _) => {
            app.filter.pop();
            app.refilter();
            true
        }
        // Navigation stays live while typing, so the filter can be narrowed
        // and the result picked without leaving the field.
        Key::Press(KeyCode::Up, _) | Key::Press(KeyCode::Down, _) | Key::Scroll(_) => false,
        Key::Press(KeyCode::Char(c), m) if !m.contains(crossterm::event::KeyModifiers::CONTROL) => {
            app.filter.push(c);
            app.refilter();
            true
        }
        _ => false,
    }
}

/// Point the journal pane at whatever is selected, stopping any follow that
/// belonged to a different unit.
fn sync_tail(app: &mut App) {
    if !app.tail_on {
        app.tail.unfollow();
        return;
    }
    match app.selected() {
        Some(r) => {
            let unit = match &r.entry.unit {
                Some(u) => u.log_unit().unwrap_or_else(|_| r.entry.primary.clone()),
                None => r.entry.primary.clone(),
            };
            let scope = r.entry.scope;
            app.tail.show(scope, &unit);
        }
        None => {
            app.tail.unfollow();
            app.tail.unit.clear();
            app.tail.lines.clear();
            app.tail.note = "nothing selected".into();
        }
    }
    app.tail.poll();
}

fn move_sel(app: &mut App, delta: isize) {
    if app.view.is_empty() {
        return;
    }
    let n = app.view.len() as isize;
    app.sel = ((app.sel as isize + delta).rem_euclid(n)) as usize;
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// On the first user-scope run, warn that user timers die at logout and offer
/// to fix it.
///
/// `should_prompt` is false when the state is unknown, so a container or a
/// non-systemd host is never nagged. Enabling is an explicit yes, never a
/// side effect of starting notcron.
fn offer_lingering(term: &mut Term, bg: Background, app: &mut App) {
    if app.linger_checked {
        return;
    }
    app.linger_checked = true;
    let check = linger::check(app.scope);
    let Some(warning) = check.warning() else {
        return;
    };
    let Some(user) = check.user.clone() else {
        return;
    };
    let body = format!(
        "{warning}\n\nEnabling lingering runs:\n\n  loginctl enable-linger {user}\n\n\
         It writes {}. Nothing else about your system changes.\n\nEnable it now?",
        linger::marker_path(&user).display()
    );
    if !dialogs::confirm(term, bg, "User timers stop at logout", &body) {
        app.status = "lingering left off -- user timers will not survive logout".into();
        return;
    }
    app.status = match linger::enable(&user) {
        Ok(()) => format!("lingering enabled for {user}"),
        Err(e) => {
            dialogs::msgbox(term, bg, "Could not enable lingering", &e);
            String::new()
        }
    };
}

/// `R`: start the selected unit's service now, without touching its schedule.
fn run_now(term: &mut Term, bg: Background, app: &mut App) {
    let Some(e) = app.entry() else { return };
    if !e.owned {
        app.status = format!("{} is not managed by notcron", e.primary);
        return;
    }
    let Some(target) = run_target(&e.files).map(str::to_string) else {
        app.status = format!("{} has nothing to run", e.primary);
        return;
    };
    let scope = e.scope;
    match systemd::systemctl(scope, &["start", &target]) {
        Ok(_) => app.status = format!("started {target}"),
        Err(err) => dialogs::msgbox(term, bg, "Run failed", err.trim()),
    }
    // The health columns are stale the instant the job runs; re-ask.
    app.reload();
}

/// `E`: write the selected unit's files somewhere, or just show them.
fn export_selected(term: &mut Term, bg: Background, app: &mut App) {
    let Some(row) = app.selected() else { return };
    let (name, scope, files) = (
        row.entry.primary.clone(),
        row.entry.scope,
        row.entry.files.clone(),
    );
    let unit = row.entry.unit.clone();

    // A unit notcron can model is re-rendered; anything else is exported as
    // the bytes on disk, which is all there is to go on.
    let rendered = match &unit {
        Some(u) => match export::files(u) {
            Ok(r) => r,
            Err(e) => {
                dialogs::msgbox(term, bg, "Cannot export", &e.to_string());
                return;
            }
        },
        None => read_rendered(scope, &files),
    };
    if rendered.is_empty() {
        app.status = format!("{name}: nothing to export");
        return;
    }

    let items = vec![
        "Write the files to a directory".to_string(),
        "Show them".to_string(),
    ];
    match dialogs::pick(term, bg, &format!("Export {name}"), &items, 0) {
        Some(1) => {
            let text = export::to_text(&rendered);
            dialogs::pager(term, bg, &format!("export: {name}"), &text);
        }
        Some(0) => {
            let Some(dir) = picker::browse(term, bg, "Export to", picker::Mode::Directory, "")
            else {
                return;
            };
            let path = std::path::PathBuf::from(&dir);
            app.status = write_export(term, bg, &rendered, &path);
        }
        _ => {}
    }
}

/// Read a unit's files off disk as they stand, for units notcron cannot model.
fn read_rendered(scope: Scope, files: &[String]) -> Vec<crate::unit::generate::RenderedFile> {
    let dir = systemd::unit_dir(scope);
    files
        .iter()
        .filter_map(|f| {
            std::fs::read_to_string(dir.join(f)).ok().map(|body| {
                crate::unit::generate::RenderedFile {
                    name: f.clone(),
                    body,
                }
            })
        })
        .collect()
}

/// Export, asking before replacing anything. `Exists` comes back before a
/// single byte is written, so a refusal really does leave the directory alone.
fn write_export(
    term: &mut Term,
    bg: Background,
    rendered: &[crate::unit::generate::RenderedFile],
    dir: &std::path::Path,
) -> String {
    let report = match export::export_files(rendered, dir, false) {
        Ok(r) => r,
        Err(export::ExportError::Exists(paths)) => {
            let body = format!(
                "These already exist in {}:\n\n  {}\n\nReplace them?",
                dir.display(),
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
            if !dialogs::confirm(term, bg, "Export would overwrite", &body) {
                return "export cancelled -- nothing was written".into();
            }
            match export::export_files(rendered, dir, true) {
                Ok(r) => r,
                Err(e) => {
                    dialogs::msgbox(term, bg, "Export failed", &e.to_string());
                    return String::new();
                }
            }
        }
        Err(e) => {
            dialogs::msgbox(term, bg, "Export failed", &e.to_string());
            return String::new();
        }
    };
    let mut msg = format!(
        "exported {} file{} to {}",
        report.written.len(),
        if report.written.len() == 1 { "" } else { "s" },
        report.dir.display()
    );
    if !report.replaced.is_empty() {
        msg.push_str(&format!(" ({} replaced)", report.replaced.len()));
    }
    msg
}

fn new_unit(term: &mut Term, bg: Background, app: &mut App) {
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
fn pick_template(term: &mut Term, bg: Background, scope: Scope) -> Option<Unit> {
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
    app.rows
        .iter()
        .map(|r| &r.entry)
        .filter_map(|e| match (e.unit.as_ref().map(|u| &u.body), mount) {
            (Some(Body::Mount(m)), true) => Some(m.where_.clone()),
            (Some(Body::Mount(_)), false) | (None, _) => None,
            (Some(_), true) => None,
            (Some(_), false) => e.unit.as_ref().map(|u| u.name.clone()),
        })
        .collect()
}

/// "New from existing": copy an owned unit into the builder under a fresh
/// name. Nothing about the original's installed state comes with it.
fn pick_clone(term: &mut Term, bg: Background, app: &App) -> Option<Unit> {
    let owned: Vec<&Entry> = app
        .rows
        .iter()
        .map(|r| &r.entry)
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
fn duplicate_selected(term: &mut Term, bg: Background, app: &mut App) {
    let Some(e) = app.entry() else { return };
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

fn edit_selected(term: &mut Term, bg: Background, app: &mut App) {
    let Some(e) = app.entry() else { return };
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

fn lifecycle(term: &mut Term, bg: Background, app: &mut App, args: &[&str]) {
    let Some(e) = app.entry() else { return };
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

fn remove_selected(term: &mut Term, bg: Background, app: &mut App) {
    let Some(e) = app.entry() else { return };
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
        "Stop, disable and delete {name}?\n\nFiles:\n  {}\n\nA copy is kept in the trash; \
         press U to bring it back.",
        files.join("\n  ")
    );
    if !dialogs::confirm(term, bg, "Remove unit", &body) {
        return;
    }
    match systemd::remove_reporting(scope, &files) {
        Ok(report) => {
            app.status = match &report.trashed {
                Some(t) => format!("removed {name} -- press U to undo ({})", t.id),
                None => format!("removed {name}"),
            };
            for w in &report.warnings {
                app.status.push_str(&format!("  [{w}]"));
            }
        }
        Err(err) => dialogs::msgbox(term, bg, "Remove failed", &err),
    }
    app.reload();
}

fn show_status(term: &mut Term, bg: Background, app: &App) {
    let Some(e) = app.entry() else { return };
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

fn show_logs(term: &mut Term, bg: Background, app: &App) {
    let Some(e) = app.entry() else { return };
    let unit = match &e.unit {
        Some(u) => u.log_unit().unwrap_or_else(|_| e.primary.clone()),
        None => e.primary.clone(),
    };
    let out = systemd::journal(e.scope, &unit, 500);
    dialogs::pager(term, bg, &format!("journal: {unit}"), &out);
}

fn view_files(term: &mut Term, bg: Background, app: &App) {
    let Some(e) = app.entry() else { return };
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

    fn row(name: &str, owned: bool) -> Row {
        Row {
            entry: Entry {
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
            },
            health: Health {
                active: "active".into(),
                sub: "waiting".into(),
                result: "success".into(),
                ..Health::default()
            },
        }
    }

    fn app_with(rows: Vec<Row>) -> App {
        let mut app = App {
            scope: Scope::User,
            show_foreign: false,
            rows,
            view: Vec::new(),
            sel: 0,
            top: 0,
            status: String::new(),
            filter: String::new(),
            typing: false,
            tail_on: false,
            tail: logtail::Tail::default(),
            now: 2_000_000_000_000_000,
            linger_checked: true,
        };
        app.refilter();
        app
    }

    fn failed_row(name: &str) -> Row {
        let mut r = row(name, true);
        r.entry.active = "failed".into();
        r.health = Health {
            active: "failed".into(),
            sub: "failed".into(),
            result: "exit-code".into(),
            exit_status: Some(2),
            ..Health::default()
        };
        r
    }

    // -----------------------------------------------------------------
    // Layout
    // -----------------------------------------------------------------

    /// The layout must survive terminals far smaller than anyone sensible
    /// uses; ratatui panics on out-of-bounds rects rather than clipping.
    #[test]
    fn draws_at_every_size_without_panicking() {
        let mut app = app_with(vec![
            failed_row("notcron-backup.timer"),
            row("sshd.service", false),
        ]);
        for tail in [false, true] {
            app.tail_on = tail;
            app.tail.lines = vec!["Aug 15 04:00:00 host x[1]: hello".into()];
            for (w, h) in [
                (1, 1),
                (2, 3),
                (10, 4),
                (20, 5),
                (40, 10),
                (80, 24),
                (200, 60),
            ] {
                let snap = app.snapshot();
                let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                t.draw(|f| draw(f, &snap)).unwrap();
            }
        }
    }

    /// The exhaustive grid: every size from nothing up to a normal terminal,
    /// with the pane on and off and with a filter active.
    #[test]
    fn draws_at_every_tiny_size_in_every_mode() {
        let mut app = app_with(vec![failed_row("a.timer"), row("b.timer", true)]);
        for tail in [false, true] {
            for typing in [false, true] {
                app.tail_on = tail;
                app.typing = typing;
                app.filter = if typing { "a".into() } else { String::new() };
                app.refilter();
                let snap = app.snapshot();
                for w in 0..=24u16 {
                    for h in 0..=24u16 {
                        let mut t = Terminal::new(TestBackend::new(w.max(1), h.max(1))).unwrap();
                        t.draw(|f| draw(f, &snap)).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn draws_an_empty_list_without_panicking() {
        let app = app_with(Vec::new());
        for (w, h) in [(1, 1), (30, 6), (120, 40)] {
            let snap = app.snapshot();
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &snap)).unwrap();
        }
    }

    #[test]
    fn draws_a_list_longer_than_the_viewport() {
        let mut app = app_with(
            (0..200)
                .map(|i| row(&format!("u{i}.timer"), true))
                .collect(),
        );
        app.sel = 199;
        app.scroll_to_selection(10);
        let snap = app.snapshot();
        let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
        t.draw(|f| draw(f, &snap)).unwrap();
    }

    // -----------------------------------------------------------------
    // The footer bindings -- a clipped binding line is a bug, not a cosmetic
    // -----------------------------------------------------------------

    #[test]
    fn the_binding_line_never_exceeds_its_width() {
        for w in 0..=250usize {
            let line = key_line(w);
            assert!(line.chars().count() <= w, "width {w}: {line:?}");
        }
    }

    /// However narrow it gets, the way out and the way to the full help are
    /// the last things to go.
    #[test]
    fn the_binding_line_always_keeps_help_and_quit() {
        for w in 14..=250usize {
            let line = key_line(w);
            assert!(line.contains("? help"), "width {w}: {line:?}");
            assert!(line.contains("q quit"), "width {w}: {line:?}");
        }
    }

    /// Bindings appear whole or not at all -- never half a word.
    #[test]
    fn the_binding_line_never_shows_half_a_binding() {
        for w in 14..=250usize {
            let line = key_line(w);
            for seg in line.split("  ").filter(|s| !s.is_empty()) {
                assert!(
                    KEY_SEGMENTS.contains(&seg) || seg == "? help" || seg == "q quit",
                    "width {w}: {seg:?} is not a whole binding ({line:?})"
                );
            }
        }
    }

    #[test]
    fn a_wide_terminal_shows_every_binding() {
        let line = key_line(200);
        for seg in KEY_SEGMENTS {
            assert!(line.contains(seg), "{seg} missing from {line:?}");
        }
    }

    // -----------------------------------------------------------------
    // Health in the table
    // -----------------------------------------------------------------

    #[test]
    fn failures_are_sorted_to_the_top_carrying_their_status() {
        let mut app = app_with(vec![
            row("aaa-clean.timer", true),
            failed_row("zzz-broken.timer"),
            row("bbb-clean.timer", true),
        ]);
        app.rows.sort_by_key(|r| health::sort_rank(&r.health));
        app.refilter();
        assert_eq!(app.rows[0].entry.primary, "zzz-broken.timer");
        assert_eq!(app.rows[0].health.last_label(), "exit 2");
        // The clean ones keep their alphabetical order behind it.
        assert_eq!(app.rows[1].entry.primary, "aaa-clean.timer");
        assert_eq!(app.rows[2].entry.primary, "bbb-clean.timer");
    }

    #[test]
    fn a_failed_row_is_red_and_a_foreign_row_is_grey() {
        assert_eq!(state_style(&failed_row("x")).fg, Some(Color::Red));
        assert_eq!(state_style(&row("x", false)).fg, Some(Color::DarkGray));
        assert_eq!(state_style(&row("x", true)).fg, Some(Color::Green));
    }

    /// A foreign unit that has failed is still shown as foreign: it is not
    /// notcron's to fix, and colouring it red would suggest otherwise.
    #[test]
    fn a_foreign_failure_stays_grey() {
        let mut r = failed_row("sshd.service");
        r.entry.owned = false;
        assert_eq!(state_style(&r).fg, Some(Color::DarkGray));
    }

    #[test]
    fn the_cells_carry_the_health_columns() {
        let now = 2_000_000_000_000_000u64;
        let mut r = failed_row("a.timer");
        r.health.next = Some(now + 3_600_000_000);
        let c = cells(&r, now);
        assert_eq!(c.name, "a.timer");
        assert_eq!(c.last, "exit 2");
        assert_eq!(c.next, "in 1h");
        assert_eq!(c.state, "failed");
    }

    /// A unit systemd said nothing about must not claim to be fine.
    #[test]
    fn an_unknown_unit_shows_dashes_rather_than_ok() {
        let mut r = row("a.timer", true);
        r.entry.active.clear();
        r.entry.enabled.clear();
        r.health = Health::default();
        let c = cells(&r, 0);
        assert_eq!(c.last, "-");
        assert_eq!(c.next, "-");
        assert_eq!(c.state, "-");
        assert_eq!(c.enabled, "-");
    }

    // -----------------------------------------------------------------
    // Filtering
    // -----------------------------------------------------------------

    #[test]
    fn an_empty_filter_matches_everything() {
        let r = row("a.timer", true);
        assert!(matches(&r, ""));
        assert!(matches(&r, "   "));
    }

    #[test]
    fn the_filter_is_case_insensitive_and_searches_what_is_on_screen() {
        let r = failed_row("notcron-BACKUP.timer");
        assert!(matches(&r, "backup"));
        assert!(matches(&r, "BACKUP"));
        assert!(matches(&r, "timer"));
        assert!(matches(&r, "a unit")); // the description
        assert!(matches(&r, "exit 2")); // the LAST column
        assert!(matches(&r, "failed")); // the STATE column
        assert!(!matches(&r, "nonsense"));
    }

    /// Several words all have to match, in any order -- that is how anyone
    /// who has used a fuzzy finder expects a filter to behave.
    #[test]
    fn every_word_of_the_filter_must_match() {
        let r = failed_row("notcron-backup.timer");
        assert!(matches(&r, "backup failed"));
        assert!(matches(&r, "failed backup"));
        assert!(!matches(&r, "backup nonsense"));
    }

    #[test]
    fn filtering_narrows_the_view_without_touching_the_rows() {
        let mut app = app_with(vec![
            row("notcron-backup.timer", true),
            row("notcron-sync.timer", true),
            row("other.service", true),
        ]);
        assert_eq!(app.view.len(), 3);
        app.filter = "notcron".into();
        app.refilter();
        assert_eq!(app.view.len(), 2);
        assert_eq!(app.rows.len(), 3, "the rows themselves are untouched");
        app.filter.clear();
        app.refilter();
        assert_eq!(app.view.len(), 3);
    }

    /// Narrowing the filter must not leave the selection pointing off the
    /// end of the view -- that is how a list picks the wrong unit.
    #[test]
    fn the_selection_stays_in_bounds_as_the_filter_narrows() {
        let mut app = app_with(vec![
            row("a.timer", true),
            row("b.timer", true),
            row("c.timer", true),
        ]);
        app.sel = 2;
        app.filter = "a.timer".into();
        app.refilter();
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.sel, 0);
        assert_eq!(app.selected().unwrap().entry.primary, "a.timer");
    }

    /// A filter that still contains the selected unit keeps it selected.
    #[test]
    fn the_selection_follows_its_unit_through_a_filter() {
        let mut app = app_with(vec![
            row("a.timer", true),
            row("b.timer", true),
            row("c.timer", true),
        ]);
        app.sel = 2;
        app.filter = "timer".into();
        app.refilter();
        assert_eq!(app.selected().unwrap().entry.primary, "c.timer");
    }

    #[test]
    fn a_filter_that_matches_nothing_selects_nothing_rather_than_panicking() {
        let mut app = app_with(vec![row("a.timer", true)]);
        app.filter = "nothing at all".into();
        app.refilter();
        assert!(app.view.is_empty());
        assert!(app.selected().is_none());
        move_sel(&mut app, 1);
        assert_eq!(app.sel, 0);
        let snap = app.snapshot();
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| draw(f, &snap)).unwrap();
    }

    // -----------------------------------------------------------------
    // The filter's keystrokes
    // -----------------------------------------------------------------

    fn press(c: char) -> Key {
        Key::Press(KeyCode::Char(c), crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn typing_builds_the_filter_incrementally() {
        let mut app = app_with(vec![row("abc.timer", true), row("xyz.timer", true)]);
        app.typing = true;
        // The filter searches every column, so a short prefix still matches
        // both rows -- "ab" is in "enabled". It only bites once enough has
        // been typed to tell the two units apart.
        assert!(filter_key(&mut app, press('a')));
        assert_eq!(app.filter, "a");
        assert_eq!(app.view.len(), 2);
        assert!(filter_key(&mut app, press('b')));
        assert_eq!(app.filter, "ab");
        assert_eq!(app.view.len(), 2);
        assert!(filter_key(&mut app, press('c')));
        assert_eq!(app.filter, "abc");
        assert_eq!(app.view.len(), 1);
        assert!(filter_key(
            &mut app,
            Key::Press(KeyCode::Backspace, crossterm::event::KeyModifiers::NONE)
        ));
        assert_eq!(app.filter, "ab");
        assert_eq!(app.view.len(), 2);
    }

    #[test]
    fn enter_keeps_the_filter_and_escape_clears_it() {
        let mut app = app_with(vec![row("abc.timer", true), row("xyz.timer", true)]);
        app.typing = true;
        for c in ['a', 'b', 'c'] {
            filter_key(&mut app, press(c));
        }
        filter_key(
            &mut app,
            Key::Press(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
        );
        assert!(!app.typing);
        assert_eq!(app.filter, "abc");
        assert_eq!(app.view.len(), 1);

        app.typing = true;
        filter_key(
            &mut app,
            Key::Press(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
        );
        assert!(!app.typing);
        assert!(app.filter.is_empty());
        assert_eq!(app.view.len(), 2);
    }

    /// Arrows must fall through while typing, so a match can be picked
    /// without leaving the field.
    #[test]
    fn navigation_is_not_swallowed_by_the_filter() {
        let mut app = app_with(vec![row("a.timer", true)]);
        app.typing = true;
        for k in [
            Key::Press(KeyCode::Up, crossterm::event::KeyModifiers::NONE),
            Key::Press(KeyCode::Down, crossterm::event::KeyModifiers::NONE),
            Key::Scroll(1),
        ] {
            assert!(!filter_key(&mut app, k), "{k:?} was swallowed");
        }
    }

    /// A control chord is a binding, not text: Ctrl-C must not end up in the
    /// filter box.
    #[test]
    fn control_chords_are_not_filter_text() {
        let mut app = app_with(vec![row("a.timer", true)]);
        app.typing = true;
        assert!(!filter_key(
            &mut app,
            Key::Press(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL)
        ));
        assert!(app.filter.is_empty());
    }

    // -----------------------------------------------------------------
    // Scrolling and the mouse
    // -----------------------------------------------------------------

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut app = app_with(vec![row("a.timer", true), row("b.timer", true)]);
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
    fn scrolling_keeps_the_selection_on_screen() {
        let mut app = app_with(
            (0..100)
                .map(|i| row(&format!("u{i}.timer"), true))
                .collect(),
        );
        app.sel = 50;
        app.scroll_to_selection(10);
        assert!(app.top <= app.sel && app.sel < app.top + 10, "{}", app.top);
        app.sel = 0;
        app.scroll_to_selection(10);
        assert_eq!(app.top, 0);
        app.sel = 99;
        app.scroll_to_selection(10);
        assert_eq!(app.top, 90);
    }

    #[test]
    fn scrolling_a_short_list_never_leaves_a_gap_at_the_top() {
        let mut app = app_with(vec![row("a.timer", true)]);
        app.top = 40;
        app.scroll_to_selection(10);
        assert_eq!(app.top, 0);
    }

    /// A click must land on the unit that was drawn on that row -- the header
    /// line offset is exactly the sort of thing that goes wrong.
    #[test]
    fn a_click_lands_on_the_row_that_was_drawn() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let inner = list_inner(area, false);
        // The first inner line is the column header, so the first unit is
        // one below it.
        assert_eq!(
            row_at(area, false, 0, inner.y),
            None,
            "the header is not a unit"
        );
        assert_eq!(row_at(area, false, 0, inner.y + 1), Some(0));
        assert_eq!(row_at(area, false, 0, inner.y + 3), Some(2));
        // With the list scrolled, the same row means a later unit.
        assert_eq!(row_at(area, false, 10, inner.y + 1), Some(10));
    }

    #[test]
    fn clicks_outside_the_rows_select_nothing() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let inner = list_inner(area, false);
        assert_eq!(row_at(area, false, 0, 0), None, "the header bar");
        assert_eq!(row_at(area, false, 0, inner.y + inner.height), None);
        assert_eq!(row_at(area, false, 0, 23), None, "the footer");
    }

    /// Opening the journal pane moves the rows; a click must follow them.
    #[test]
    fn clicks_track_the_journal_pane() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        };
        let with = list_inner(area, true);
        let without = list_inner(area, false);
        assert!(with.height < without.height);
        assert_eq!(row_at(area, true, 0, with.y + 1), Some(0));
        // A click in the pane itself is not a unit.
        let panes = logtail::split(area, true);
        let tail = panes.tail.expect("a pane");
        assert_eq!(row_at(area, true, 0, tail.y + 1), None);
    }

    /// No frame size may make the hit test return a row that was not drawn.
    #[test]
    fn hit_testing_never_escapes_the_drawn_rows() {
        for h in 0..=40u16 {
            for tail in [false, true] {
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: h,
                };
                let visible = visible_rows(area, tail);
                for y in 0..=h {
                    if let Some(i) = row_at(area, tail, 0, y) {
                        assert!(i < visible.max(1), "{h}x tail={tail} y={y} -> {i}");
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Run now
    // -----------------------------------------------------------------

    #[test]
    fn run_now_targets_the_service_not_the_timer() {
        assert_eq!(
            run_target(&["a.timer".into(), "a.service".into()]),
            Some("a.service")
        );
        // Order in the group must not matter.
        assert_eq!(
            run_target(&["a.service".into(), "a.timer".into()]),
            Some("a.service")
        );
        assert_eq!(run_target(&["a.service".into()]), Some("a.service"));
    }

    #[test]
    fn a_mount_runs_as_itself_and_a_bare_timer_has_nothing_to_run() {
        assert_eq!(
            run_target(&["srv-data.automount".into(), "srv-data.mount".into()]),
            Some("srv-data.mount")
        );
        assert_eq!(run_target(&["a.timer".into()]), None);
        assert_eq!(run_target(&[]), None);
    }

    // -----------------------------------------------------------------
    // Help
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
        for key in ["s", "S", "a", "d", "x", "i", "l", "v", "r"] {
            assert!(
                LIFECYCLE_KEYS.iter().any(|(k, _)| *k == key),
                "{key} is bound but undocumented"
            );
        }
        assert!(text.contains("y                duplicate"), "{text}");
    }

    /// The new actions have no entry in the field-help document, so the help
    /// screen is the only place they are described. It must describe them.
    #[test]
    fn the_new_actions_are_documented_too() {
        let text = help_text();
        for (key, _) in EXTRA_HELP {
            assert!(
                text.contains(&format!("\n  {key}   ")),
                "{key} undocumented"
            );
        }
        for phrase in ["Run now", "Export", "Undo a removal"] {
            assert!(text.contains(phrase), "{phrase} absent");
        }
    }

    /// The `f` collision: it follows the journal now, and the foreign toggle
    /// moved to shift-F. Both must be documented, and the old meaning must
    /// not still be claimed.
    #[test]
    fn the_f_binding_documents_where_the_foreign_toggle_went() {
        let text = help_text();
        assert!(text.contains("F        show/hide foreign units"), "{text}");
        assert!(text.contains("f                follow it live"), "{text}");
        assert!(text.contains("moved to shift-F"), "{text}");
        assert!(
            !text.contains("f        show/hide foreign units"),
            "the help still claims f toggles foreign units"
        );
    }

    /// Every key named in the footer is a key the loop actually binds.
    #[test]
    fn the_footer_names_only_real_bindings() {
        let bound = [
            "Enter", "n", "/", "R", "t", "x", "s", "S", "a", "d", "E", "U", "i", "l", "v", "y",
            "u", "?", "q",
        ];
        for seg in KEY_SEGMENTS {
            let key = seg.split_whitespace().next().unwrap();
            assert!(bound.contains(&key), "{seg}: {key} is not bound");
        }
    }

    // -----------------------------------------------------------------
    // Copying, which the new row indirection must not have broken
    // -----------------------------------------------------------------

    fn row_with(name: &str, unit: Unit) -> Row {
        let mut r = row(name, true);
        r.entry.unit = Some(unit);
        r
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

    #[test]
    fn the_names_a_copy_must_avoid_depend_on_what_is_being_copied() {
        let app = app_with(vec![
            row_with("notcron-backup.timer", named_timer("backup")),
            row_with("notcron-sync.timer", named_timer("sync")),
            row_with("srv-data.mount", mount_at("/srv/data")),
            row("foreign.service", false),
        ]);
        let mut names = taken_names(&app, false);
        names.sort();
        assert_eq!(names, vec!["backup".to_string(), "sync".to_string()]);
        assert_eq!(taken_names(&app, true), vec!["/srv/data".to_string()]);
    }

    #[test]
    fn a_copied_timer_gets_a_free_name_and_no_install_state() {
        let app = app_with(vec![
            row_with("notcron-backup.timer", named_timer("backup")),
            row_with("notcron-backup-copy.timer", named_timer("backup-copy")),
        ]);
        let src = app.rows[0].entry.unit.clone().expect("a unit");
        let copy = templates::clone_unit(&src, &taken_names(&app, false));
        assert_eq!(copy.name, "backup-copy-2");
        assert!(copy.description.contains("(copy)"), "{}", copy.description);
        assert_eq!(copy.scope, src.scope);
        assert!(copy.filenames().is_ok());
    }

    #[test]
    fn a_copied_mount_is_renamed_by_its_mount_point() {
        let app = app_with(vec![row_with("srv-data.mount", mount_at("/srv/data"))]);
        let src = app.rows[0].entry.unit.clone().expect("a unit");
        let copy = templates::clone_unit(&src, &taken_names(&app, true));
        let Body::Mount(m) = &copy.body else {
            panic!("not a mount")
        };
        assert_eq!(m.where_, "/srv/data-copy");
        assert_ne!(copy.stem().unwrap(), src.stem().unwrap());
    }

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
