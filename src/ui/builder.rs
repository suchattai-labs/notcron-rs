//! The detailed unit builder: a focus-driven field list where Enter opens the
//! right editor for whatever is selected -- a text prompt, a choice list, a
//! toggle, the schedule sub-builder, or the free-text manual editor.

use super::dialogs::{self, Background};
use super::editor;
use super::optmenu;
use super::picker;
use super::term::{Key, Term};
use crate::complete::{self, Accounts, Completion};
use crate::cron::{self, Translation};
use crate::fieldhelp;
use crate::systemd;
use crate::templates;
use crate::unit::escape;
use crate::unit::generate;
use crate::unit::model::{
    Body, MountPreset, RestartPolicy, Schedule, Scope, ServiceOpts, ServiceType, Unit,
};
use crate::validate;
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Identifies what activating a row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id {
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
    Group,
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

pub struct Row {
    pub id: Id,
    pub label: String,
    pub value: String,
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
    rows.push(row(Id::Group, "Group", opt(&s.group)));
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

// ---------------------------------------------------------------------------
// The side pane
// ---------------------------------------------------------------------------

/// What the pane beside the form is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Off,
    /// The help document's entry for the focused row.
    Help,
    /// The generated unit files, re-rendered on every edit.
    Preview,
    /// The next five firings of the schedule.
    Runs,
    /// The advisory checks. Never blocks a save.
    Checks,
}

impl Pane {
    /// The order `v` walks, ending at `Off` so the key is also the way out.
    pub const CYCLE: [Pane; 5] = [
        Pane::Help,
        Pane::Preview,
        Pane::Runs,
        Pane::Checks,
        Pane::Off,
    ];

    pub fn next(self) -> Pane {
        let i = Pane::CYCLE.iter().position(|p| *p == self).unwrap_or(0);
        Pane::CYCLE[(i + 1) % Pane::CYCLE.len()]
    }

    pub fn title(self) -> &'static str {
        match self {
            Pane::Off => "",
            Pane::Help => "Field help",
            Pane::Preview => "Unit files",
            Pane::Runs => "Next runs",
            Pane::Checks => "Checks",
        }
    }
}

/// Columns the form keeps for itself before a pane may take any. Below this
/// the value column stops showing enough of a path to be worth reading.
const FORM_MIN: u16 = 46;
/// A pane narrower than this is not worth the space it costs the form.
const PANE_MIN: u16 = 26;
const PANE_MAX: u16 = 48;

/// The width at or above which the pane is on when the builder opens.
///
/// A permanent split does not fit 80x24: the form needs 46 columns to show a
/// path next to a 22-column label, which leaves 32 for a pane that wants to
/// display unit-file lines. So the split is the default only on a terminal
/// with room for both, and everywhere else it is one keypress (`v`) away.
const PANE_DEFAULT_WIDTH: u16 = 100;

pub fn default_pane(width: u16) -> Pane {
    if width >= PANE_DEFAULT_WIDTH {
        Pane::Help
    } else {
        Pane::Off
    }
}

/// Split the builder's area into the form and, if it fits, the side pane.
///
/// Returns `(form, None)` whenever the pane is off or the terminal is too
/// narrow to carry one -- the form is never squeezed to make room.
pub fn split_panes(area: Rect, pane_on: bool) -> (Rect, Option<Rect>) {
    if !pane_on || area.height < 3 || area.width < FORM_MIN + PANE_MIN {
        return (area, None);
    }
    let want = (area.width * 2 / 5).clamp(PANE_MIN, PANE_MAX);
    let w = want.min(area.width - FORM_MIN);
    if w < PANE_MIN {
        return (area, None);
    }
    let form = Rect {
        width: area.width - w,
        ..area
    };
    let pane = Rect {
        x: area.x + (area.width - w),
        width: w,
        ..area
    };
    (form, Some(pane))
}

// ---------------------------------------------------------------------------
// Derived state
// ---------------------------------------------------------------------------

/// The next-run preview's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runs {
    /// This schedule has no `OnCalendar=` to analyse -- an interval or boot
    /// timer, or a body that is not a timer at all. Not an error.
    NotCalendar(String),
    /// `systemd-analyze` is missing. Also not an error: the user has done
    /// nothing wrong and must not be told they have.
    Unavailable,
    /// systemd rejected the spec.
    Invalid(String),
    Times(Vec<systemd::NextRun>),
}

/// How many firings the preview shows.
pub const RUNS_SHOWN: usize = 5;

/// Everything computed *from* the unit rather than typed into it.
///
/// Recomputed on a change rather than on a keystroke: the rendered files are
/// their own fingerprint, so an edit that changes nothing costs nothing, and
/// the calendar preview -- which shells out -- only reruns when the schedule's
/// specs actually differ.
pub struct Derived {
    pub preview: String,
    pub diags: Vec<validate::Diagnostic>,
    pub runs: Runs,
    runs_key: Option<Vec<String>>,
    computed: bool,
}

/// The calendar specs a unit's schedule would be analysed from, if any.
fn calendar_specs(u: &Unit) -> Option<Vec<String>> {
    let Body::Timer(t) = &u.body else { return None };
    match &t.schedule {
        Schedule::Calendar(specs) if specs.iter().any(|s| !s.trim().is_empty()) => {
            Some(specs.clone())
        }
        _ => None,
    }
}

/// Why there is nothing to analyse, in the user's terms.
fn no_calendar_reason(u: &Unit) -> String {
    match &u.body {
        Body::Timer(t) => match &t.schedule {
            Schedule::Every { every, boot } => format!(
                "Every {every}, starting {boot} after boot.\n\nAn interval timer has no calendar \
                 to analyse: when it fires depends on when the machine came up."
            ),
            Schedule::Boot { boot } => format!(
                "At boot + {boot}.\n\nA boot timer has no calendar to analyse: it fires once, \
                 relative to startup."
            ),
            Schedule::Calendar(_) => "No OnCalendar= set yet.".to_string(),
        },
        Body::Service(_) => "A standalone service has no schedule; it is started by its \
                             target or by hand."
            .to_string(),
        Body::Mount(_) => "A mount unit has no schedule.".to_string(),
    }
}

impl Derived {
    pub fn new(u: &Unit) -> Derived {
        let mut d = Derived {
            preview: String::new(),
            diags: Vec::new(),
            runs: Runs::NotCalendar(String::new()),
            runs_key: None,
            computed: false,
        };
        d.refresh(u);
        d
    }

    /// Bring everything in line with `u`, doing no work where nothing moved.
    pub fn refresh(&mut self, u: &Unit) {
        let text = generate::preview(u, &systemd::unit_dir(u.scope).to_string_lossy());
        if !self.computed || text != self.preview {
            self.preview = text;
            // Advisory only: `Unit::validate` remains the gate on saving and
            // this never blocks anything.
            self.diags = validate::check_unit(u);
        }
        let specs = calendar_specs(u);
        if !self.computed || specs != self.runs_key {
            self.runs = match &specs {
                None => Runs::NotCalendar(no_calendar_reason(u)),
                Some(s) => match systemd::next_runs_multi(s, RUNS_SHOWN) {
                    Ok(v) => Runs::Times(v),
                    // A missing systemd-analyze is a fact about the host, not
                    // a mistake by the user.
                    Err(systemd::PreviewError::Unavailable) => Runs::Unavailable,
                    Err(systemd::PreviewError::Invalid(m)) => Runs::Invalid(m),
                },
            };
            self.runs_key = specs;
        }
        self.computed = true;
    }

    /// Diagnostics that carry a one-key fix, worst first.
    pub fn fixable(&self) -> Vec<&validate::Diagnostic> {
        self.diags
            .iter()
            .filter(|d| validate::autofix(d).is_some())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Hard-wrap `text` at `width`, keeping blank lines as paragraph breaks.
pub fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            // A word longer than the pane is cut rather than allowed to
            // overflow the block and corrupt the frame.
            if word.chars().count() > width {
                line.extend(word.chars().take(width));
                out.push(std::mem::take(&mut line));
            } else {
                line.push_str(word);
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// The help document's entry for a row, as pane lines.
fn help_lines(id: Id, width: usize) -> Vec<String> {
    let Some(e) = help_key(id).and_then(fieldhelp::entry) else {
        return vec!["(no help for this row)".into()];
    };
    let mut out = wrap_lines(&e.label, width);
    out.push(String::new());
    out.extend(wrap_lines(&e.summary, width));
    out.push(String::new());
    out.extend(wrap_lines(&e.detail, width));
    if !e.examples.is_empty() {
        out.push(String::new());
        out.extend(wrap_lines(&format!("Examples: {}", e.examples), width));
    }
    out
}

/// The next-run list, as lines. Shared by the pane and by the live note under
/// the OnCalendar prompt, so the two can never disagree.
pub fn runs_lines(runs: &Runs, width: usize) -> Vec<String> {
    match runs {
        Runs::NotCalendar(why) => wrap_lines(why, width),
        Runs::Unavailable => wrap_lines(
            "Preview unavailable: systemd-analyze is not installed. The schedule itself is fine.",
            width,
        ),
        Runs::Invalid(m) => {
            let mut out = vec!["! systemd rejected this spec:".to_string(), String::new()];
            out.extend(wrap_lines(m, width));
            out
        }
        Runs::Times(v) if v.is_empty() => wrap_lines(
            "This spec has no future elapse -- it will never fire again.",
            width,
        ),
        Runs::Times(v) => {
            let mut out = Vec::new();
            for (i, r) in v.iter().enumerate() {
                out.extend(wrap_lines(&format!("{}. {}", i + 1, r.local), width));
                if !r.from_now.is_empty() {
                    out.extend(wrap_lines(&format!("   {}", r.from_now), width));
                }
            }
            out
        }
    }
}

/// The advisory checks, as lines.
fn check_lines(d: &Derived, width: usize) -> Vec<String> {
    if d.diags.is_empty() {
        return wrap_lines(
            "No advisories.\n\nThese checks are advice, never a gate: anything here can still \
             be saved.",
            width,
        );
    }
    let mut out = Vec::new();
    for diag in &d.diags {
        out.extend(wrap_lines(
            &format!("{}: {}", diag.level.as_str(), diag.message),
            width,
        ));
        if let Some(fix) = validate::autofix(diag) {
            out.extend(wrap_lines(
                &format!("  -> c, then f: {}", fix.label()),
                width,
            ));
        }
        out.push(String::new());
    }
    out.extend(wrap_lines(
        "Advice only -- none of this blocks a save.",
        width,
    ));
    out
}

/// Everything the pane shows, already wrapped to `width`.
pub fn pane_body(pane: Pane, id: Id, d: &Derived, width: usize) -> Vec<String> {
    match pane {
        Pane::Off => Vec::new(),
        Pane::Help => help_lines(id, width),
        // Unit files are code: clipped, never re-wrapped.
        Pane::Preview => d
            .preview
            .lines()
            .map(|l| l.chars().take(width).collect())
            .collect(),
        Pane::Runs => runs_lines(&d.runs, width),
        Pane::Checks => check_lines(d, width),
    }
}

/// The whole builder screen, kept apart from the event loop so it can be
/// rendered at any terminal size in a test.
pub struct View {
    pub title: String,
    pub rows: Vec<Row>,
    pub sel: usize,
    pub top: usize,
    pub pane: Pane,
    pub pane_top: usize,
    pub status: String,
    /// A one-shot message that outranks the status line until the next key.
    pub flash: Option<String>,
    pub browsable: bool,
    pub derived: Derived,
}

impl View {
    pub fn new(u: &Unit, title: &str, pane: Pane) -> View {
        let rows = rows_for(u);
        let sel = first_selectable(&rows);
        let mut v = View {
            title: title.to_string(),
            rows,
            sel,
            top: 0,
            pane,
            pane_top: 0,
            status: String::new(),
            flash: None,
            browsable: false,
            derived: Derived::new(u),
        };
        v.sync(u);
        v
    }

    /// Rebuild everything that follows from the unit after an edit.
    pub fn sync(&mut self, u: &Unit) {
        self.rows = rows_for(u);
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
        if self.rows.get(self.sel).map(|r| r.id) == Some(Id::Heading) {
            self.sel = step(&self.rows, self.sel, 1);
        }
        self.derived.refresh(u);
        self.refocus(u);
    }

    /// Everything that depends on *which row is focused* rather than on the
    /// unit's contents. Cheap, and run every frame: moving the selection has
    /// to move the picker hint and the status line with it.
    pub fn refocus(&mut self, u: &Unit) {
        self.browsable = browsable(u, self.current()).is_some();
        self.status = status_line(u, &self.derived, self.current());
    }

    pub fn current(&self) -> Id {
        self.rows.get(self.sel).map(|r| r.id).unwrap_or(Id::Heading)
    }

    pub fn step(&mut self, delta: isize) {
        self.sel = step(&self.rows, self.sel, delta);
        self.pane_top = 0;
    }
}

/// Paint one frame.
pub fn draw(f: &mut Frame, v: &mut View) {
    let area = f.area();
    // The builder covers the screen it was opened from. Without this, a
    // heading row -- which is a few characters long -- leaves the rest of its
    // line showing whatever the list had painted there.
    f.render_widget(Clear, area);
    // Four lines, not three: the status and the help line each need one, and
    // at three the status pushed the keybindings off screen entirely whenever
    // the unit was still incomplete -- which is always, when the form has
    // just opened.
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).split(area);
    let (form_area, pane_area) = split_panes(chunks[0], v.pane != Pane::Off);

    let block = Block::default()
        .title(format!(" {} ", v.title))
        .title_style(Style::new().bold())
        .borders(Borders::ALL);
    let inner = block.inner(form_area);
    f.render_widget(block, form_area);

    let visible = inner.height.max(1) as usize;
    if v.sel < v.top {
        v.top = v.sel;
    } else if v.sel >= v.top + visible {
        v.top = v.sel + 1 - visible;
    }
    let label_w = 22usize;
    let value_w = (inner.width as usize).saturating_sub(label_w + 4);
    let lines: Vec<Line> = v
        .rows
        .iter()
        .enumerate()
        .skip(v.top)
        .take(visible)
        .map(|(i, r)| {
            if r.id == Id::Heading {
                return Line::from(Span::styled(
                    format!(" {}", r.label),
                    Style::new().bold().fg(Color::Cyan),
                ));
            }
            let value = truncate(&r.value, value_w);
            let label = &r.label;
            let text = format!(
                " {} {label:<label_w$} {value}",
                if i == v.sel { ">" } else { " " }
            );
            Line::from(Span::styled(
                truncate(&text, inner.width as usize),
                if i == v.sel {
                    Style::new().bold().reversed()
                } else {
                    Style::new()
                },
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);

    if let Some(rect) = pane_area {
        draw_pane(f, v, rect);
    }

    // Status and help get a line each and are truncated rather than wrapped,
    // so neither can ever push the other off the screen.
    let footer = Block::default().borders(Borders::ALL);
    let fi = footer.inner(chunks[1]);
    f.render_widget(footer, chunks[1]);
    let fr = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(fi);
    let status = v.flash.as_deref().unwrap_or(&v.status);
    f.render_widget(Paragraph::new(truncate(status, fi.width as usize)), fr[0]);
    let (browse, browse_style) = if v.browsable {
        ("b browses this path", Style::new().bold().fg(Color::Cyan))
    } else {
        ("b browses paths", Style::new().fg(Color::DarkGray))
    };
    let mut spans: Vec<Span> = Vec::new();
    for hint in key_hints(browse, fi.width as usize) {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let style = if hint == browse {
            browse_style
        } else {
            Style::new().fg(Color::DarkGray)
        };
        spans.push(Span::styled(hint, style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), fr[1]);
}

/// The keybinding line, as separate hints.
///
/// Too narrow to show them all and the least important are dropped whole,
/// rather than the line being clipped mid-word -- which is how "Esc cancel"
/// used to disappear on an 80-column terminal, leaving no visible way out.
pub fn key_hints(browse: &str, width: usize) -> Vec<String> {
    let mut parts: Vec<String> = [
        "Enter edit",
        "Tab move",
        browse,
        "v pane",
        "? help",
        "^S save",
        "Esc cancel",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // Two spaces between hints, none after the last.
    let used = |p: &[String]| {
        p.iter().map(|x| x.chars().count()).sum::<usize>() + 2 * p.len().saturating_sub(1)
    };
    // In drop order. `b` is not among them: it is the least guessable key
    // here, and only the focused row advertises it.
    for hint in ["? help", "v pane", browse, "Tab move"] {
        if used(&parts) <= width {
            break;
        }
        parts.retain(|p| p != hint);
    }
    parts
}

fn draw_pane(f: &mut Frame, v: &mut View, rect: Rect) {
    let block = Block::default()
        .title(format!(" {} ", v.pane.title()))
        .title_style(Style::new().bold())
        .borders(Borders::ALL);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let body = pane_body(v.pane, v.current(), &v.derived, inner.width as usize);
    let height = inner.height as usize;
    v.pane_top = v.pane_top.min(body.len().saturating_sub(1));
    let style = match v.pane {
        Pane::Checks if !v.derived.diags.is_empty() => Style::new().fg(Color::Yellow),
        Pane::Preview => Style::new().fg(Color::Gray),
        _ => Style::new(),
    };
    let lines: Vec<Line> = body
        .into_iter()
        .skip(v.pane_top)
        .take(height)
        .map(|l| Line::from(Span::styled(l, style)))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Help keys
// ---------------------------------------------------------------------------

/// The `docs/field-help.md` entry backing a row.
///
/// Every selectable row has one, and a test asserts it -- the document is the
/// only copy of this text, so a row without a key is a row whose help silently
/// disappeared.
pub fn help_key(id: Id) -> Option<&'static str> {
    Some(match id {
        Id::Heading => return None,
        Id::Name => "unit.name",
        Id::Description => "unit.description",
        Id::Scope => "unit.scope",
        Id::Schedule => "timer.schedule",
        Id::Persistent => "timer.persistent",
        Id::RandomDelay => "timer.randomized_delay",
        Id::ServiceType => "service.type",
        Id::ExecStart => "service.exec_start",
        Id::ShellWrap => "service.shell_wrap",
        Id::ExecStartPre => "service.exec_start_pre",
        Id::ExecStopPost => "service.exec_stop_post",
        Id::Restart => "service.restart",
        Id::RestartSec => "service.restart_sec",
        Id::WorkDir => "service.working_directory",
        Id::RunAs => "service.user",
        Id::Group => "service.group",
        Id::Env => "service.environment",
        Id::WantedBy => "service.wanted_by",
        Id::Preset => "mount.preset",
        Id::What => "mount.what",
        Id::Where => "mount.where",
        Id::FsType => "mount.fstype",
        Id::Options => "mount.options",
        Id::Automount => "mount.automount",
        Id::TimeoutIdle => "mount.timeout_idle",
        Id::ManualPrimary => "unit.manual_primary",
        Id::ManualSecondary => "unit.manual_secondary",
        Id::Preview => "builder.preview",
        Id::Save => "builder.save",
    })
}

/// The schedule chooser, in menu order, keyed to the help document.
pub const SCHEDULE_KEYS: [&str; 8] = [
    "schedule.every_minutes",
    "schedule.every_hours",
    "schedule.daily",
    "schedule.weekly",
    "schedule.monthly",
    "schedule.boot",
    "schedule.cron",
    "schedule.oncalendar",
];

/// `?` on a row: the full help entry, in a pager, for when the pane is off or
/// too narrow for the detail paragraph.
fn explain(term: &mut Term, bg: Background, id: Id) {
    let Some(e) = help_key(id).and_then(fieldhelp::entry) else {
        dialogs::msgbox(term, bg, "Help", "No help for this row.");
        return;
    };
    let body = format!("{}\n\n{}\n\nExamples: {}", e.summary, e.detail, e.examples);
    dialogs::pager(term, bg, &e.label, &wrap_lines(&body, 76).join("\n"));
}

// ---------------------------------------------------------------------------
// Advisory checks
// ---------------------------------------------------------------------------

/// The directive a row writes, for matching a [`validate::Diagnostic`] to it.
pub fn directive(id: Id) -> Option<&'static str> {
    Some(match id {
        Id::ExecStart => "ExecStart",
        Id::ExecStartPre => "ExecStartPre",
        Id::ExecStopPost => "ExecStopPost",
        Id::WorkDir => "WorkingDirectory",
        Id::RunAs => "User",
        Id::Group => "Group",
        Id::Env => "Environment",
        _ => return None,
    })
}

/// The current value of the directive a fix applies to.
fn directive_value(u: &Unit, field: &str) -> Option<String> {
    let s = match &u.body {
        Body::Timer(t) => &t.service,
        Body::Service(s) => &s.service,
        Body::Mount(_) => return None,
    };
    Some(match field {
        "ExecStart" => s.exec_start.clone(),
        "ExecStartPre" => s.exec_start_pre.clone().unwrap_or_default(),
        "ExecStopPost" => s.exec_stop_post.clone().unwrap_or_default(),
        _ => return None,
    })
}

/// Apply a suggested fix. Returns what changed, for the status line.
fn apply_fix(u: &mut Unit, fix: &validate::Fix) -> Option<String> {
    let field = fix.field().to_string();
    let current = directive_value(u, &field)?;
    let new = fix.apply(&current);
    if new == current {
        return None;
    }
    if matches!(u.body, Body::Mount(_)) {
        return None;
    }
    let s = service_mut(u);
    match field.as_str() {
        "ExecStart" => s.exec_start = new,
        "ExecStartPre" => s.exec_start_pre = Some(new),
        "ExecStopPost" => s.exec_stop_post = Some(new),
        _ => return None,
    }
    Some(format!("{field}=: {}", fix.label()))
}

/// The fixable diagnostic that belongs to the focused row, if any.
fn fix_for_row(d: &Derived, id: Id) -> Option<(&validate::Diagnostic, validate::Fix)> {
    let field = directive(id)?;
    d.diags.iter().find_map(|diag| {
        let fix = validate::autofix(diag)?;
        (fix.field() == field).then_some((diag, fix))
    })
}

/// `f`: apply the focused row's suggested fix.
fn quick_fix(u: &mut Unit, d: &Derived, id: Id) -> String {
    match fix_for_row(d, id) {
        Some((_, fix)) => match apply_fix(u, &fix) {
            Some(msg) => format!("fixed: {msg}"),
            None => "nothing to change".into(),
        },
        None if d.fixable().is_empty() => {
            "no suggested fix for this field (c reviews all checks)".into()
        }
        None => "no suggested fix for this field; c reviews the ones there are".into(),
    }
}

/// `c`: walk the advisory checks, applying fixes. Returns true if the unit
/// changed.
fn review_checks(term: &mut Term, bg: Background, u: &mut Unit) -> bool {
    let mut changed = false;
    let mut sel = 0usize;
    loop {
        let diags = validate::check_unit(u);
        if diags.is_empty() {
            dialogs::msgbox(
                term,
                bg,
                "Checks",
                "No advisories.\n\nThese checks are advice about what systemd will do \
                 differently from a shell. They never block a save.",
            );
            return changed;
        }
        let labels: Vec<String> = diags
            .iter()
            .map(|d| {
                let fix = validate::autofix(d).is_some();
                format!(
                    "{} {}{}",
                    if d.level == validate::Level::Error {
                        "error  "
                    } else {
                        "warning"
                    },
                    truncate(&d.message, 90),
                    if fix { "   [Enter fixes]" } else { "" }
                )
            })
            .collect();
        let Some(i) = dialogs::pick(term, bg, "Checks (advice only)", &labels, sel) else {
            return changed;
        };
        sel = i;
        match validate::autofix(&diags[i]) {
            Some(fix) => {
                if apply_fix(u, &fix).is_some() {
                    changed = true;
                } else {
                    dialogs::msgbox(term, bg, "Cannot apply", &format!("{}", diags[i]));
                }
            }
            None => dialogs::msgbox(
                term,
                bg,
                diags[i].level.as_str(),
                &wrap_lines(&diags[i].message, 66).join("\n"),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Status line
// ---------------------------------------------------------------------------

/// The line under the form: the hard gate first, then the focused row's
/// suggested fix, then advisories, then when the timer next fires.
fn status_line(u: &Unit, d: &Derived, id: Id) -> String {
    if let Err(e) = u.validate() {
        return format!("! {e}");
    }
    if let Some((diag, fix)) = fix_for_row(d, id) {
        return format!("{}: {} -- f applies it", diag.level.as_str(), fix.label());
    }
    if !d.diags.is_empty() {
        let errs = if validate::has_errors(&d.diags) {
            d.diags
                .iter()
                .filter(|x| x.level == validate::Level::Error)
                .count()
        } else {
            0
        };
        let warns = d.diags.len() - errs;
        let mut parts = Vec::new();
        if errs > 0 {
            parts.push(format!("{errs} error(s)"));
        }
        if warns > 0 {
            parts.push(format!("{warns} warning(s)"));
        }
        return format!(
            "{} from the advisory checks -- c reviews them",
            parts.join(", ")
        );
    }
    match &d.runs {
        Runs::Times(v) if !v.is_empty() => {
            let next = &v[0];
            if next.from_now.is_empty() {
                format!("ready to install   next: {}", next.local)
            } else {
                format!(
                    "ready to install   next: {} ({})",
                    next.local, next.from_now
                )
            }
        }
        Runs::Invalid(m) => format!("! {}", m.lines().next().unwrap_or("invalid calendar spec")),
        Runs::Unavailable => {
            "ready to install (systemd-analyze missing: schedules unchecked)".into()
        }
        _ => "ready to install".into(),
    }
}

// ---------------------------------------------------------------------------
// The event loop
// ---------------------------------------------------------------------------

/// Run the builder. Returns `true` when the unit was installed.
pub fn run(term: &mut Term, bg: Background, u: &mut Unit, title: &str) -> bool {
    let width = term.terminal.size().map(|s| s.width).unwrap_or(80);
    let mut v = View::new(u, title, default_pane(width));

    loop {
        v.refocus(u);
        dialogs::draw_over(term, bg, &mut |f| draw(f, &mut v));

        let Some(k) = term.next_key() else {
            return false;
        };
        v.flash = None;
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => v.step(d as isize),
            _ if k.is_ctrl('s') => {
                if save(term, bg, u) {
                    return true;
                }
                v.sync(u);
            }
            _ => match k.code() {
                Some(KeyCode::Esc) => {
                    if dialogs::confirm(term, bg, "Discard", "Discard this unit and go back?") {
                        return false;
                    }
                }
                Some(KeyCode::Up) | Some(KeyCode::BackTab) => v.step(-1),
                Some(KeyCode::Down) | Some(KeyCode::Tab) => v.step(1),
                Some(KeyCode::PageUp) => v.pane_top = v.pane_top.saturating_sub(PANE_SCROLL),
                Some(KeyCode::PageDown) => v.pane_top += PANE_SCROLL,
                Some(KeyCode::Enter) => {
                    if v.current() == Id::Save {
                        if save(term, bg, u) {
                            return true;
                        }
                    } else {
                        activate(term, bg, u, v.current());
                    }
                    v.sync(u);
                }
                _ if k.is_char('b') => {
                    if browsable(u, v.current()).is_some() {
                        browse_field(term, bg, u, v.current());
                        v.sync(u);
                    } else {
                        v.flash = Some(
                            "b browses only path fields: ExecStart, ExecStartPre, \
                             ExecStopPost, WorkingDirectory, What and Where"
                                .into(),
                        );
                    }
                }
                _ if k.is_char('p') => preview(term, bg, u),
                _ if k.is_char('v') => {
                    v.pane = v.pane.next();
                    v.pane_top = 0;
                    v.flash = Some(match v.pane {
                        Pane::Off => "pane off -- v brings it back".into(),
                        p => format!("pane: {}", p.title()),
                    });
                }
                _ if k.is_char('c') => {
                    review_checks(term, bg, u);
                    v.sync(u);
                }
                _ if k.is_char('f') => {
                    let msg = quick_fix(u, &v.derived, v.current());
                    v.sync(u);
                    v.flash = Some(msg);
                }
                _ if k.is_char('?') => explain(term, bg, v.current()),
                _ if k.is_char('k') => v.step(-1),
                _ if k.is_char('j') => v.step(1),
                _ => {}
            },
        }
    }
}

/// Lines PgUp/PgDn move the pane by.
const PANE_SCROLL: usize = 10;

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
                // The two managers have different target sets, so a target
                // that was valid before the switch may not be reachable
                // after it. Moving it to the new scope's default is visible
                // on the WantedBy row; leaving it would not be.
                if let Body::Service(s) = &mut u.body {
                    if !u.scope.has_install_target(&s.wanted_by) {
                        s.wanted_by = u.scope.default_install_target().to_string();
                    }
                }
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
            let comp = |s: &str, _: bool| complete_exec(s);
            if let Some(v) = dialogs::prompt_ext(
                term,
                bg,
                "ExecStart",
                "Absolute path plus arguments. Shell syntax needs the wrapper below.",
                &cur,
                &|s: &str| {
                    if s.trim().is_empty() {
                        Err("ExecStart must not be empty".into())
                    } else {
                        Ok(())
                    }
                },
                dialogs::PromptOpts {
                    complete: Some(&comp),
                    ..Default::default()
                },
            ) {
                service_mut(u).exec_start = v.clone();
                if let Some(n) = suggested_name(&u.name, &v) {
                    u.name = n;
                }
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
            let comp = |s: &str, _: bool| complete_exec(s);
            service_mut(u).exec_start_pre = ask_optional_ext(
                term,
                bg,
                "ExecStartPre",
                "Runs before ExecStart. Empty clears it.",
                &cur,
                "",
                dialogs::PromptOpts {
                    complete: Some(&comp),
                    ..Default::default()
                },
            );
        }
        Id::ExecStopPost => {
            let cur = service_mut(u).exec_stop_post.clone();
            let comp = |s: &str, _: bool| complete_exec(s);
            service_mut(u).exec_stop_post = ask_optional_ext(
                term,
                bg,
                "ExecStopPost",
                "Runs after the service stops, success or failure. Empty clears it.",
                &cur,
                "",
                dialogs::PromptOpts {
                    complete: Some(&comp),
                    ..Default::default()
                },
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
            let comp = |s: &str, _: bool| complete::complete_path(s, true);
            // Unset fields open on the directory notcron was started in --
            // a prefilled suggestion the user can accept, edit or clear.
            let seed = templates::suggest_working_directory().unwrap_or_default();
            service_mut(u).working_directory = ask_optional_ext(
                term,
                bg,
                "WorkingDirectory",
                "Absolute path the command runs in. Empty clears it.",
                &cur,
                &seed,
                dialogs::PromptOpts {
                    complete: Some(&comp),
                    ..Default::default()
                },
            );
        }
        Id::RunAs => {
            let cur = service_mut(u).run_as.clone();
            let comp = |s: &str, all: bool| {
                complete::complete_user(s, if all { Accounts::All } else { Accounts::Login })
            };
            service_mut(u).run_as = ask_optional_ext(
                term,
                bg,
                "User",
                "User= (system units only). Empty clears it.",
                &cur,
                "",
                dialogs::PromptOpts {
                    complete: Some(&comp),
                    toggle: Some("system accounts"),
                    ..Default::default()
                },
            );
        }
        Id::Group => {
            let cur = service_mut(u).group.clone();
            let comp = |s: &str, all: bool| {
                complete::complete_group(s, if all { Accounts::All } else { Accounts::Login })
            };
            service_mut(u).group = ask_optional_ext(
                term,
                bg,
                "Group",
                "Group= (system units only). Empty clears it.",
                &cur,
                "",
                dialogs::PromptOpts {
                    complete: Some(&comp),
                    toggle: Some("system groups"),
                    ..Default::default()
                },
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
            // Only the targets this scope's manager actually has: enabling
            // into one it does not know about is accepted and then never
            // pulls the unit in.
            let Body::Service(_) = &u.body else { return };
            let current = current_wanted_by(u);
            let choices = wanted_by_choices(u.scope, &current);
            let labels: Vec<String> = choices.iter().map(|(_, l)| l.clone()).collect();
            let cur = choices.iter().position(|(v, _)| *v == current).unwrap_or(0);
            if let Some(i) = dialogs::pick(term, bg, "WantedBy=", &labels, cur) {
                if let Body::Service(s) = &mut u.body {
                    s.wanted_by = choices[i].0.clone();
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
            let Body::Mount(m) = &mut u.body else { return };
            let help = format!("What to mount, e.g. {}", m.preset.what_hint());
            let is_path = m.preset.what_is_path();
            let comp = |s: &str, _: bool| complete::complete_path(s, false);
            if let Some(v) = dialogs::prompt_ext(
                term,
                bg,
                "What",
                &help,
                &m.what,
                &|s: &str| {
                    if s.trim().is_empty() {
                        Err("What= must not be empty".into())
                    } else {
                        Ok(())
                    }
                },
                dialogs::PromptOpts {
                    // Only a local path can be completed; an NFS or CIFS
                    // remote is not one, so the key stays inert there.
                    complete: is_path.then_some(&comp as dialogs::Completer),
                    ..Default::default()
                },
            ) {
                m.what = v;
                if let Some(w) = suggested_where(&m.where_, &m.what) {
                    m.where_ = w;
                }
                u.name = u.stem().unwrap_or_default();
            }
        }
        Id::Where => {
            let Body::Mount(m) = &mut u.body else { return };
            let comp = |s: &str, _: bool| complete::complete_path(s, true);
            let seed = if m.where_.trim().is_empty() {
                templates::suggest_where(&m.what).unwrap_or_default()
            } else {
                m.where_.clone()
            };
            if let Some(v) = dialogs::prompt_ext(
                term,
                bg,
                "Where",
                "Absolute mount point. The unit filename is derived from it.",
                &seed,
                &|s: &str| escape::escape_path(s).map(|_| ()),
                dialogs::PromptOpts {
                    complete: Some(&comp),
                    ..Default::default()
                },
            ) {
                m.where_ = v;
                u.name = u.stem().unwrap_or_default();
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
            let Body::Mount(m) = &u.body else { return };
            let (options, fstype) = (m.options.clone(), m.fstype.clone());
            if let Some(v) = optmenu::run(term, bg, &options, &fstype) {
                if let Body::Mount(m) = &mut u.body {
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
// Install target
// ---------------------------------------------------------------------------

/// The `WantedBy=` a unit currently carries; empty for bodies that have none.
fn current_wanted_by(u: &Unit) -> String {
    match &u.body {
        Body::Service(s) => s.wanted_by.clone(),
        _ => String::new(),
    }
}

/// `(value, label)` for every target offerable in `scope`.
///
/// A target the unit already names but this scope does not provide is kept at
/// the end and flagged, rather than dropped: it is what the file on disk says
/// and hiding it would make the picker lie about the current value. Everything
/// above it is a target the manager can genuinely reach.
fn wanted_by_choices(scope: Scope, current: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = scope
        .install_targets()
        .iter()
        .map(|t| (t.to_string(), t.to_string()))
        .collect();
    if !current.is_empty() && !scope.has_install_target(current) {
        out.push((
            current.to_string(),
            format!("{current}   (no such target in {} scope)", scope.as_str()),
        ));
    }
    out
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

// ---------------------------------------------------------------------------
// Smart defaults
// ---------------------------------------------------------------------------

/// A unit name derived from a command line, or `None` when the unit already
/// has one.
///
/// Suggestion, never overwrite: something the user typed is never replaced,
/// and the derivation is [`templates::suggest_name`], which shares
/// `cli::build_exec` with `notcron add` -- so the same job named in the TUI
/// and on the command line comes out identical.
fn suggested_name(current: &str, exec: &str) -> Option<String> {
    if !current.trim().is_empty() {
        return None;
    }
    let name = templates::suggest_name(exec);
    (!name.is_empty()).then_some(name)
}

/// A mount point derived from `What=`, or `None` when one is already set.
fn suggested_where(current: &str, what: &str) -> Option<String> {
    if !current.trim().is_empty() {
        return None;
    }
    templates::suggest_where(what)
}

/// Complete only the last whitespace-separated token of `value`, putting the
/// untouched head back on every candidate.
///
/// [`Completion`]'s fields are full replacements for the field text, so the
/// head has to travel with them -- appending a candidate to what is already
/// there would double it.
fn complete_tail(value: &str, f: &dyn Fn(&str) -> Completion) -> Completion {
    let cut = value.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let (head, tail) = value.split_at(cut);
    let c = f(tail);
    Completion {
        candidates: c.candidates.iter().map(|x| format!("{head}{x}")).collect(),
        common: format!("{head}{}", c.common),
    }
}

/// Completion for an `Exec*=` line: the program must be executable, but an
/// argument is just as likely to be a path, so only the first token is
/// filtered by the executable bit.
fn complete_exec(value: &str) -> Completion {
    if value.contains(char::is_whitespace) {
        complete_tail(value, &|t| complete::complete_path(t, false))
    } else {
        complete_tail(value, &complete::complete_executable)
    }
}

/// [`ask_optional`] with completion, and a `seed` shown when the field is
/// unset -- a suggestion the user accepts by pressing Enter and refuses by
/// clearing the line. An already-set field is never seeded over.
fn ask_optional_ext(
    term: &mut Term,
    bg: Background,
    title: &str,
    help: &str,
    cur: &Option<String>,
    seed: &str,
    opts: dialogs::PromptOpts<'_>,
) -> Option<String> {
    let start = cur.clone().unwrap_or_else(|| seed.to_string());
    match dialogs::prompt_ext(term, bg, title, help, &start, &dialogs::no_validation, opts) {
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

/// One line of the schedule chooser, taken from the help document so the
/// menu and the field help cannot describe the same preset differently.
fn schedule_item(key: &str) -> String {
    match fieldhelp::entry(key) {
        Some(e) => format!("{:<34} {}", e.label, e.summary),
        None => key.to_string(),
    }
}

/// The next-run lines shown live under a schedule prompt.
///
/// Only ever called once typing has paused, since each call shells out to
/// `systemd-analyze`.
fn note_runs(specs: &[String]) -> Vec<String> {
    let runs = match systemd::next_runs_multi(specs, RUNS_SHOWN) {
        Ok(v) => Runs::Times(v),
        Err(systemd::PreviewError::Unavailable) => Runs::Unavailable,
        // A half-typed spec is not an error worth shouting about: the
        // prompt's own validator says so on Enter.
        Err(systemd::PreviewError::Invalid(_)) => return Vec::new(),
    };
    let mut out = vec!["  Next runs:".to_string()];
    out.extend(runs_lines(&runs, 66).into_iter().map(|l| format!("  {l}")));
    out
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Ask for a schedule, replacing `schedule` and `source` if the user commits.
fn edit_schedule(term: &mut Term, bg: Background, schedule: &mut Schedule, source: &mut String) {
    let items: Vec<String> = SCHEDULE_KEYS.iter().map(|k| schedule_item(k)).collect();

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
        6 => dialogs::prompt_ext(
            term,
            bg,
            "Cron expression",
            "5 fields (minute hour day-of-month month day-of-week), or @daily etc.",
            "",
            &|s: &str| cron::to_calendar(s).map(|_| ()).map_err(|e| e.to_string()),
            dialogs::PromptOpts {
                note: Some(&|s: &str| match cron::to_calendar(s) {
                    Ok(Translation::Calendar(specs)) => {
                        let mut out =
                            vec![format!("  OnCalendar={}", specs.join("\n  OnCalendar="))];
                        out.extend(note_runs(&specs));
                        out
                    }
                    Ok(Translation::Reboot) => {
                        vec!["  OnBootSec=1min -- fires once per boot".into()]
                    }
                    Err(_) => Vec::new(),
                }),
                ..Default::default()
            },
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
        _ => dialogs::prompt_ext(
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
            dialogs::PromptOpts {
                note: Some(&|s: &str| note_runs(&[s.to_string()])),
                ..Default::default()
            },
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

    /// The bug this guards: the builder used to offer the system manager's
    /// target list whatever the scope was, so a user-scope service could be
    /// enabled into `multi-user.target` -- accepted by systemctl, and then
    /// never started, because the user manager has no such target.
    #[test]
    fn wanted_by_offers_only_targets_the_scope_actually_has() {
        let user: Vec<String> = wanted_by_choices(Scope::User, "")
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        for absent in [
            "multi-user.target",
            "graphical.target",
            "network-online.target",
        ] {
            assert!(
                !user.contains(&absent.to_string()),
                "{absent} offered to a user unit"
            );
        }
        assert!(user.contains(&"default.target".to_string()));
        assert!(user.contains(&"basic.target".to_string()));

        let system: Vec<String> = wanted_by_choices(Scope::System, "")
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert!(system.contains(&"multi-user.target".to_string()));
        for absent in ["default.target", "basic.target", "graphical-session.target"] {
            assert!(
                !system.contains(&absent.to_string()),
                "{absent} offered to a system unit"
            );
        }

        // timers.target is the one both managers provide.
        assert!(user.contains(&"timers.target".to_string()));
        assert!(system.contains(&"timers.target".to_string()));
    }

    #[test]
    fn a_new_service_defaults_to_a_target_its_scope_can_reach() {
        for scope in [Scope::User, Scope::System] {
            let u = Unit::new_service(scope);
            let target = current_wanted_by(&u);
            assert!(
                scope.has_install_target(&target),
                "{scope:?} defaults to {target}, which it does not have"
            );
        }
        assert_eq!(
            current_wanted_by(&Unit::new_service(Scope::User)),
            "default.target"
        );
        assert_eq!(
            current_wanted_by(&Unit::new_service(Scope::System)),
            "multi-user.target"
        );
    }

    /// A target read off disk that this scope cannot reach is still shown --
    /// it is what the file says -- but flagged, and it never displaces a real
    /// choice at the top of the list.
    #[test]
    fn a_foreign_target_is_kept_and_flagged_rather_than_hidden() {
        let choices = wanted_by_choices(Scope::User, "multi-user.target");
        assert_eq!(choices.len(), Scope::User.install_targets().len() + 1);
        let (value, label) = choices.last().expect("the foreign target");
        assert_eq!(value, "multi-user.target");
        assert!(label.contains("no such target"), "{label}");
        // A target the scope does have is not duplicated.
        assert_eq!(
            wanted_by_choices(Scope::User, "default.target").len(),
            Scope::User.install_targets().len()
        );
    }

    #[test]
    fn only_service_bodies_report_a_wanted_by() {
        assert_eq!(current_wanted_by(&Unit::new_timer(Scope::User)), "");
        assert_eq!(current_wanted_by(&Unit::new_mount()), "");
    }

    #[test]
    fn the_mount_options_row_opens_the_menu_for_the_current_fstype() {
        use crate::unit::mountopts::{self, Family};
        // The row the menu hangs off exists on every mount, and the fstype
        // it is opened with is the one that decides the offered set.
        for preset in MountPreset::ALL {
            let mut u = Unit::new_mount();
            if let Body::Mount(m) = &mut u.body {
                m.preset = preset;
                m.fstype = preset.fstype().into();
                m.options = preset.options().into();
            }
            assert!(rows_for(&u).iter().any(|r| r.id == Id::Options));
            let Body::Mount(m) = &u.body else {
                unreachable!()
            };
            let st = optmenu::State::new(&m.options, &m.fstype);
            assert_eq!(st.set.text(), m.options, "{}", preset.label());
            let expected = match preset {
                MountPreset::Nfs => Family::Nfs,
                MountPreset::Cifs => Family::Cifs,
                MountPreset::Bind => Family::Bind,
                MountPreset::Block => Family::Generic,
            };
            assert_eq!(st.set.family(), expected, "{}", preset.label());
            assert_eq!(
                mountopts::family_for(&m.fstype),
                expected,
                "{}",
                preset.label()
            );
        }
    }

    #[test]
    fn status_line_reports_validation_failures_first() {
        let u = Unit::new_timer(Scope::User);
        let d = Derived::new(&u);
        assert!(status_line(&u, &d, Id::Name).starts_with('!'));
    }

    // -----------------------------------------------------------------
    // Field help
    // -----------------------------------------------------------------

    fn all_bodies() -> [Unit; 3] {
        [
            Unit::new_timer(Scope::User),
            Unit::new_service(Scope::User),
            Unit::new_mount(),
        ]
    }

    /// Every row the builder can show is described by `docs/field-help.md`.
    /// This is the guard against a new row shipping with a blank help pane,
    /// and it is the same docs/code agreement test the options menu has.
    #[test]
    fn every_builder_row_has_a_help_entry() {
        for u in all_bodies() {
            for r in rows_for(&u) {
                if r.id == Id::Heading {
                    assert!(help_key(r.id).is_none(), "headings need no help");
                    continue;
                }
                let key = help_key(r.id).unwrap_or_else(|| panic!("{:?} has no help key", r.id));
                let e = fieldhelp::entry(key)
                    .unwrap_or_else(|| panic!("{:?} -> {key}: no such entry", r.id));
                assert!(!e.label.is_empty(), "{key} has no label");
                assert!(!e.summary.is_empty(), "{key} has no summary");
                assert!(!e.detail.is_empty(), "{key} has no detail");
            }
        }
    }

    /// The shell-wrap row has no stable value to show -- it renders
    /// "(press Enter)" -- so its help is keyed off the row, not the value.
    #[test]
    fn the_shell_wrap_row_is_documented_despite_having_no_value() {
        let u = Unit::new_timer(Scope::User);
        let row = rows_for(&u)
            .into_iter()
            .find(|r| r.id == Id::ShellWrap)
            .expect("the wrap row");
        assert_eq!(row.value, "(press Enter)");
        let e = fieldhelp::entry(help_key(Id::ShellWrap).unwrap()).expect("help");
        assert!(!e.summary.is_empty());
    }

    #[test]
    fn every_schedule_preset_is_documented_and_offered() {
        for key in SCHEDULE_KEYS {
            let e = fieldhelp::entry(key).unwrap_or_else(|| panic!("{key}: no such entry"));
            assert!(!e.summary.is_empty(), "{key}");
            let item = schedule_item(key);
            assert!(item.contains(&e.label), "{key} -> {item}");
            assert!(item.contains(&e.summary), "{key} -> {item}");
        }
        // The chooser's arms and the key list must stay the same length --
        // `edit_schedule` indexes one by the other.
        assert_eq!(SCHEDULE_KEYS.len(), 8);
    }

    #[test]
    fn help_lines_fall_back_rather_than_panicking() {
        assert_eq!(help_key(Id::Heading), None);
        assert_eq!(help_lines(Id::Heading, 40), vec!["(no help for this row)"]);
        let lines = help_lines(Id::ExecStart, 40);
        assert!(lines.iter().any(|l| l.contains("ExecStart")));
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
    }

    // -----------------------------------------------------------------
    // Layout
    // -----------------------------------------------------------------

    fn rect(w: u16, h: u16) -> Rect {
        Rect {
            x: 3,
            y: 2,
            width: w,
            height: h,
        }
    }

    #[test]
    fn the_pane_never_escapes_the_area_or_starves_the_form() {
        for w in 0..=140u16 {
            for h in [0u16, 1, 2, 3, 5, 24, 40] {
                let a = rect(w, h);
                for on in [false, true] {
                    let (form, pane) = split_panes(a, on);
                    assert!(form.x >= a.x && form.y == a.y, "{w}x{h}");
                    assert!(form.x + form.width <= a.x + a.width, "{w}x{h}");
                    assert!(form.height == a.height, "{w}x{h}");
                    match pane {
                        None => assert_eq!(form.width, a.width, "{w}x{h} on={on}"),
                        Some(p) => {
                            assert!(on, "a pane appeared while off");
                            assert_eq!(form.x + form.width, p.x, "{w}x{h}: a gap or an overlap");
                            assert_eq!(p.x + p.width, a.x + a.width, "{w}x{h}");
                            assert!(p.width >= PANE_MIN, "{w}x{h}: {} wide", p.width);
                            assert!(form.width >= FORM_MIN, "{w}x{h}: form starved");
                        }
                    }
                }
            }
        }
    }

    /// A permanent split does not fit an 80-column terminal, so it is off
    /// there and one keypress away. Anything roomier opens with it on.
    #[test]
    fn the_pane_is_off_by_default_on_a_narrow_terminal() {
        assert_eq!(default_pane(80), Pane::Off);
        assert_eq!(default_pane(99), Pane::Off);
        assert_eq!(default_pane(100), Pane::Help);
        assert_eq!(default_pane(200), Pane::Help);
        // ...but it still fits when asked for at 80 columns.
        let (form, pane) = split_panes(rect(78, 20), true);
        assert!(pane.is_some(), "v must work at 80 columns");
        assert!(form.width >= FORM_MIN);
    }

    /// The way out has to stay visible. At 80 columns the full hint line is
    /// exactly as wide as the footer; anything narrower drops whole hints
    /// from the least important end rather than clipping the last one.
    #[test]
    fn the_keybinding_line_never_clips_the_way_out() {
        for browse in ["b browses this path", "b browses paths"] {
            for width in 0..=120usize {
                let hints = key_hints(browse, width);
                assert!(hints.contains(&"Esc cancel".to_string()), "{width}");
                assert!(hints.contains(&"^S save".to_string()), "{width}");
                assert!(hints.contains(&"Enter edit".to_string()), "{width}");
                let used = hints.iter().map(|h| h.chars().count()).sum::<usize>()
                    + 2 * hints.len().saturating_sub(1);
                // Either it fits, or nothing droppable is left.
                assert!(
                    used <= width || hints.len() == 3,
                    "{width}: {used} wide, {hints:?}"
                );
            }
        }
        // A standard 80-column terminal keeps every hint.
        assert_eq!(key_hints("b browses this path", 78).len(), 7);
    }

    #[test]
    fn cycling_the_pane_visits_every_mode_and_returns() {
        let mut p = Pane::Help;
        let mut seen = vec![p];
        for _ in 0..Pane::CYCLE.len() {
            p = p.next();
            seen.push(p);
        }
        assert_eq!(seen.last(), Some(&Pane::Help), "the cycle must close");
        for mode in Pane::CYCLE {
            assert!(seen.contains(&mode), "{mode:?} unreachable");
        }
        assert!(Pane::Off.title().is_empty());
    }

    /// Everything the pane paints fits inside it, at any width.
    #[test]
    fn pane_bodies_are_clipped_to_the_pane() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        if let Body::Timer(t) = &mut u.body {
            t.service.exec_start = "/usr/bin/rsync -aHAX --delete /home/ /srv/backup/home/".into();
        }
        let d = Derived::new(&u);
        for width in [8usize, 12, 26, 40, 80] {
            for pane in Pane::CYCLE {
                for id in [Id::Name, Id::ExecStart, Id::Schedule, Id::Save] {
                    for line in pane_body(pane, id, &d, width) {
                        assert!(
                            line.chars().count() <= width.max(8),
                            "{pane:?} {id:?} at {width}: {line:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wrapping_breaks_paragraphs_and_never_overflows() {
        let out = wrap_lines("one two three four five", 10);
        assert!(out.iter().all(|l| l.chars().count() <= 10), "{out:?}");
        assert_eq!(out.join(" ").split_whitespace().count(), 5);
        // Blank lines survive as paragraph breaks.
        assert_eq!(wrap_lines("a\n\nb", 20), vec!["a", "", "b"]);
        // An unbreakable word is cut rather than allowed to overflow.
        let long = wrap_lines(&"x".repeat(40), 10);
        assert!(long.iter().all(|l| l.chars().count() <= 10), "{long:?}");
        assert!(wrap_lines("", 10).len() <= 1);
    }

    /// The whole screen, painted at every size down to 1x1, in every pane
    /// mode and for every body. Nothing here may panic or overflow a buffer.
    #[test]
    fn the_builder_draws_at_any_terminal_size() {
        use ratatui::backend::TestBackend;
        for u in all_bodies() {
            for pane in Pane::CYCLE {
                let mut v = View::new(&u, "Test", pane);
                for w in [1u16, 2, 5, 20, 46, 71, 72, 80, 100, 160] {
                    for h in [1u16, 2, 3, 4, 5, 6, 24, 50] {
                        let mut t = Terminal::new(TestBackend::new(w, h)).expect("backend");
                        v.pane_top = 0;
                        t.draw(|f| draw(f, &mut v)).expect("draw");
                        // ...and again, scrolled past the end of the body.
                        v.pane_top = 9_999;
                        t.draw(|f| draw(f, &mut v)).expect("draw scrolled");
                    }
                }
            }
        }
    }

    #[test]
    fn the_form_still_shows_a_value_column_at_eighty_columns() {
        use ratatui::backend::TestBackend;
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        if let Body::Timer(t) = &mut u.body {
            t.service.exec_start = "/opt/bk.sh".into();
        }
        let mut v = View::new(&u, "Test", Pane::Help);
        let mut t = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        t.draw(|f| draw(f, &mut v)).expect("draw");
        let text: String = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("/opt/bk.sh"), "the value column vanished");
        assert!(text.contains("Field help"), "the pane vanished");
    }

    // -----------------------------------------------------------------
    // Derived state
    // -----------------------------------------------------------------

    #[test]
    fn derived_preview_is_the_generated_files() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        if let Body::Timer(t) = &mut u.body {
            t.service.exec_start = "/bin/true".into();
        }
        let d = Derived::new(&u);
        assert_eq!(
            d.preview,
            generate::preview(&u, &systemd::unit_dir(u.scope).to_string_lossy())
        );
        assert!(d.preview.contains("[Timer]") && d.preview.contains("[Service]"));
    }

    /// The live preview follows the form, manual block included.
    #[test]
    fn the_preview_follows_every_edit_including_the_manual_block() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        let mut d = Derived::new(&u);
        let before = d.preview.clone();
        if let Body::Timer(t) = &mut u.body {
            t.service_manual = "Nice=19\n".into();
        }
        d.refresh(&u);
        assert_ne!(d.preview, before);
        assert!(d.preview.contains("Nice=19"), "{}", d.preview);
        assert!(d.preview.contains("notcron:manual"));
    }

    /// An interval or boot timer has no calendar, and must not be shown an
    /// empty run list as though something had gone wrong.
    #[test]
    fn schedules_without_a_calendar_explain_themselves() {
        for (schedule, needle) in [
            (
                Schedule::Every {
                    every: "15min".into(),
                    boot: "5min".into(),
                },
                "interval timer",
            ),
            (
                Schedule::Boot {
                    boot: "1min".into(),
                },
                "boot timer",
            ),
        ] {
            let mut u = Unit::new_timer(Scope::User);
            u.name = "x".into();
            if let Body::Timer(t) = &mut u.body {
                t.schedule = schedule;
            }
            let d = Derived::new(&u);
            let Runs::NotCalendar(why) = &d.runs else {
                panic!("expected no calendar, got {:?}", d.runs);
            };
            assert!(why.contains(needle), "{why}");
            let lines = runs_lines(&d.runs, 40);
            assert!(!lines.is_empty() && lines.iter().all(|l| l.chars().count() <= 40));
        }
        // Neither does a mount or a standalone service.
        for u in [Unit::new_service(Scope::User), Unit::new_mount()] {
            assert!(matches!(Derived::new(&u).runs, Runs::NotCalendar(_)));
        }
    }

    /// A missing systemd-analyze is a fact about the host, not a mistake by
    /// the user, and must never read as one.
    #[test]
    fn an_unavailable_analyzer_is_not_reported_as_a_user_error() {
        let lines = runs_lines(&Runs::Unavailable, 60).join(" ");
        assert!(lines.contains("unavailable"), "{lines}");
        assert!(!lines.contains('!'), "{lines}");
        // An invalid spec, by contrast, is flagged.
        let bad = runs_lines(&Runs::Invalid("no such day".into()), 60).join(" ");
        assert!(bad.contains('!') && bad.contains("no such day"));
    }

    #[test]
    fn a_calendar_that_never_fires_again_says_so() {
        let lines = runs_lines(&Runs::Times(Vec::new()), 60).join(" ");
        assert!(lines.contains("never"), "{lines}");
    }

    #[test]
    fn run_times_are_numbered_and_carry_the_relative_form() {
        let runs = Runs::Times(vec![systemd::NextRun {
            local: "Mon 2026-08-17 03:00:00 CEST".into(),
            utc: String::new(),
            from_now: "1 day 23h left".into(),
        }]);
        let lines = runs_lines(&runs, 60);
        assert!(lines[0].starts_with("1. Mon 2026-08-17"), "{lines:?}");
        assert!(lines[1].contains("1 day 23h left"), "{lines:?}");
    }

    // -----------------------------------------------------------------
    // Advisory checks
    // -----------------------------------------------------------------

    /// The advisory checks never gate a save: a unit that trips every one of
    /// them still passes the structural validation that does.
    #[test]
    fn advisories_do_not_block_saving() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "x".into();
        if let Body::Timer(t) = &mut u.body {
            t.schedule = Schedule::Calendar(vec!["*-*-* 03:00:00".into()]);
            t.service.exec_start = "definitely-not-a-real-binary".into();
        }
        let d = Derived::new(&u);
        assert!(!d.diags.is_empty(), "expected an advisory");
        assert!(u.validate().is_ok(), "the hard gate must still pass");
    }

    #[test]
    fn the_bare_command_warning_carries_a_one_key_fix() {
        let Some(real) = validate::which("true") else {
            eprintln!("skipping: no 'true' on PATH");
            return;
        };
        let mut u = Unit::new_timer(Scope::User);
        u.name = "x".into();
        if let Body::Timer(t) = &mut u.body {
            t.service.exec_start = "true --quiet".into();
        }
        let d = Derived::new(&u);
        let (_, fix) = fix_for_row(&d, Id::ExecStart).expect("a fix for ExecStart");
        assert_eq!(fix.field(), "ExecStart");
        assert!(fix.label().contains(&real.display().to_string()));
        // Applying it keeps the arguments.
        let msg = quick_fix(&mut u, &d, Id::ExecStart);
        assert!(msg.starts_with("fixed:"), "{msg}");
        let Body::Timer(t) = &u.body else {
            unreachable!()
        };
        assert_eq!(t.service.exec_start, format!("{} --quiet", real.display()));
        // ...and the advisory is gone afterwards.
        assert!(fix_for_row(&Derived::new(&u), Id::ExecStart).is_none());
    }

    #[test]
    fn a_row_with_no_fix_says_so_instead_of_doing_nothing() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "x".into();
        if let Body::Timer(t) = &mut u.body {
            t.service.exec_start = "/bin/sh".into();
        }
        let d = Derived::new(&u);
        let msg = quick_fix(&mut u, &d, Id::Description);
        assert!(msg.contains("no suggested fix"), "{msg}");
    }

    #[test]
    fn only_rows_that_write_a_directive_can_match_a_diagnostic() {
        assert_eq!(directive(Id::ExecStart), Some("ExecStart"));
        assert_eq!(directive(Id::RunAs), Some("User"));
        assert_eq!(directive(Id::Group), Some("Group"));
        assert_eq!(directive(Id::Heading), None);
        assert_eq!(directive(Id::Save), None);
    }

    #[test]
    fn the_status_line_offers_the_focused_rows_fix() {
        if validate::which("true").is_none() {
            eprintln!("skipping: no 'true' on PATH");
            return;
        }
        let mut u = Unit::new_timer(Scope::User);
        u.name = "x".into();
        if let Body::Timer(t) = &mut u.body {
            t.schedule = Schedule::Calendar(vec!["*-*-* 03:00:00".into()]);
            t.service.exec_start = "true".into();
        }
        let d = Derived::new(&u);
        let on_exec = status_line(&u, &d, Id::ExecStart);
        assert!(on_exec.contains("f applies it"), "{on_exec}");
        // On another row the same advisory is still counted, not hidden.
        let elsewhere = status_line(&u, &d, Id::Description);
        assert!(elsewhere.contains("advisory checks"), "{elsewhere}");
    }

    #[test]
    fn a_clean_unit_reports_readiness_rather_than_advice() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "x".into();
        if let Body::Timer(t) = &mut u.body {
            t.schedule = Schedule::Calendar(vec!["*-*-* 03:00:00".into()]);
            t.service.exec_start = "/bin/true".into();
        }
        let d = Derived::new(&u);
        let line = status_line(&u, &d, Id::ExecStart);
        assert!(line.starts_with("ready to install"), "{line}");
    }

    #[test]
    fn the_checks_pane_says_it_is_advice_either_way() {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "x".into();
        if let Body::Timer(t) = &mut u.body {
            t.service.exec_start = "/bin/true".into();
        }
        let clean = pane_body(Pane::Checks, Id::ExecStart, &Derived::new(&u), 60).join(" ");
        assert!(clean.contains("No advisories"), "{clean}");
        assert!(
            clean.contains("never a gate") || clean.contains("advice"),
            "{clean}"
        );
    }

    // -----------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------

    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Tree {
            use std::os::unix::fs::PermissionsExt;
            let d = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(d.path().join("bin")).unwrap();
            std::fs::write(d.path().join("bin/plain"), "x").unwrap();
            let run = d.path().join("bin/runner");
            std::fs::write(&run, "x").unwrap();
            std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
            Tree(d)
        }
        fn at(&self, rel: &str) -> String {
            format!("{}/{rel}", self.0.path().display())
        }
    }

    /// Candidates are full replacements for the *field*, not for the token --
    /// so the untouched head of the line has to come back with them.
    #[test]
    fn completing_an_argument_keeps_the_rest_of_the_command_line() {
        let t = Tree::new();
        let line = format!("/bin/echo {}", t.at("bin/pl"));
        let c = complete_exec(&line);
        assert_eq!(c.common, format!("/bin/echo {}", t.at("bin/plain")));
        assert!(
            c.candidates.iter().all(|x| x.starts_with("/bin/echo ")),
            "{c:?}"
        );
    }

    /// The program is filtered by the executable bit; an argument is not.
    #[test]
    fn only_the_program_is_filtered_by_the_executable_bit() {
        let t = Tree::new();
        let prog = complete_exec(&t.at("bin/"));
        assert_eq!(prog.candidates, vec![t.at("bin/runner")]);
        let arg = complete_exec(&format!("/bin/echo {}", t.at("bin/")));
        assert_eq!(arg.candidates.len(), 2, "{arg:?}");
    }

    #[test]
    fn no_match_leaves_the_field_exactly_as_typed() {
        let t = Tree::new();
        let line = format!("/bin/echo {}", t.at("zzz"));
        let c = complete_exec(&line);
        assert!(c.is_empty());
        // Blind assignment of `common` is safe: it is the input again.
        assert_eq!(c.common, line);
    }

    #[test]
    fn completing_a_bare_program_replaces_the_whole_field() {
        let t = Tree::new();
        let c = complete_exec(&t.at("bin/run"));
        assert!(c.is_unique());
        assert_eq!(c.common, t.at("bin/runner"));
    }

    // -----------------------------------------------------------------
    // Smart defaults
    // -----------------------------------------------------------------

    #[test]
    fn a_name_is_suggested_only_for_an_unnamed_unit() {
        assert_eq!(
            suggested_name("", "/usr/local/bin/backup.sh --full"),
            Some("backup-sh".to_string())
        );
        // Anything the user typed is left alone.
        assert_eq!(suggested_name("mine", "/usr/local/bin/backup.sh"), None);
        assert_eq!(suggested_name("  ", "  "), None);
    }

    /// The TUI and `notcron add` must derive the same name from the same
    /// command, because both go through `cli::build_exec`.
    #[test]
    fn the_suggested_name_matches_what_the_cli_would_pick() {
        for cmd in [
            "/usr/local/bin/backup.sh --full",
            "/usr/bin/rsync -a /a /b",
            "/usr/bin/env python3 /opt/job.py",
        ] {
            let args: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
            let (_, hint) = crate::cli::build_exec(&args, false);
            assert_eq!(
                suggested_name("", cmd),
                Some(escape::slugify(&hint)),
                "{cmd}"
            );
        }
    }

    #[test]
    fn a_mount_point_is_suggested_only_for_a_mount_without_one() {
        assert_eq!(
            suggested_where("", "//server/share"),
            Some("/mnt/share".to_string())
        );
        assert_eq!(suggested_where("/srv/here", "//server/share"), None);
        assert_eq!(suggested_where("", ""), None);
    }
}
