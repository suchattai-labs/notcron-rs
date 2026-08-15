//! The mount `Options=` menu: a modal list of the options that actually do
//! something in a `.mount` unit, filtered by the filesystem type, with the
//! composed option string shown live underneath.
//!
//! All the option semantics live in [`crate::unit::mountopts`]; this module
//! only turns them into rows and runs an event loop. Row composition is a
//! pure function so the layout can be exercised at any terminal size without
//! a terminal, exactly as the filesystem picker is.

use super::dialogs::{self, dialog_block, draw_over, Background};
use super::picker;
use super::term::{popup_rect, Key, Term};
use crate::fieldhelp;
use crate::unit::mountopts::{self, Kind, OptionSet, Spec};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};

/// A line in the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A non-selectable heading.
    Heading(String),
    /// A toggleable option.
    Opt(&'static Spec),
    /// The free-text entry holding every option the menu cannot represent.
    /// Its existence is what makes editing someone else's unit lossless.
    Extras,
    /// A non-selectable note. Points at the companion `.automount` toggle,
    /// which is the only thing that actually produces automount behaviour
    /// now that the inert `x-systemd.*` options are off the menu.
    Note(String),
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(self, Row::Opt(_) | Row::Extras)
    }
}

/// The rows for a set, in menu order: the generic options, then the ones
/// specific to this filesystem, then the free-text entry and the note.
pub fn rows(set: &OptionSet) -> Vec<Row> {
    let mut out = vec![Row::Heading("Generic".into())];
    out.extend(mountopts::GENERIC.iter().map(Row::Opt));
    let family = set.family();
    if !family.extras().is_empty() {
        out.push(Row::Heading(family.label().to_string()));
        out.extend(family.extras().iter().map(Row::Opt));
    }
    out.push(Row::Heading("Anything else".into()));
    out.push(Row::Extras);
    out.push(Row::Note(
        "x-systemd.* options are fstab-only and do nothing in a unit file \u{2014} \
         for automounting, use the builder's \"Companion .automount\" toggle."
            .into(),
    ));
    out
}

/// The text of one row, given the current state of the set.
pub fn label(set: &OptionSet, row: &Row) -> String {
    match row {
        Row::Heading(h) => h.clone(),
        Row::Note(n) => n.clone(),
        Row::Extras => {
            let extras = set.extras_text();
            format!(
                "[{}] other options{}",
                if extras.is_empty() { ' ' } else { 'x' },
                if extras.is_empty() {
                    String::new()
                } else {
                    format!("   {extras}")
                }
            )
        }
        Row::Opt(spec) => {
            let on = set.is_on(spec.key);
            let shown = match (spec.kind, set.value_of(spec.key)) {
                (Kind::Value, Some(v)) => format!("{}={v}", spec.key),
                (Kind::Value, None) if on => spec.key.to_string(),
                (Kind::Value, None) => format!("{}=\u{2026}", spec.key),
                (Kind::Flag, _) => spec.key.to_string(),
            };
            format!("[{}] {shown}", if on { 'x' } else { ' ' })
        }
    }
}

/// The one-line summary shown under the list for the focused row.
pub fn summary(row: &Row) -> String {
    match row {
        Row::Opt(spec) => fieldhelp::entry(spec.help)
            .map(|e| e.summary.clone())
            .unwrap_or_default(),
        Row::Extras => {
            "Options notcron does not offer, kept verbatim so nothing is lost.".to_string()
        }
        _ => String::new(),
    }
}

/// The menu's popup, sized like the picker's so the two feel like one thing.
pub fn menu_rect(area: Rect) -> Rect {
    popup_rect(
        area,
        area.width.saturating_sub(6).max(30),
        area.height.saturating_sub(2).max(10),
    )
}

/// Everything the menu paints, kept apart from the event loop so it can be
/// rendered at any size in a test.
pub struct State {
    pub set: OptionSet,
    pub rows: Vec<Row>,
    pub sel: usize,
    pub top: usize,
}

impl State {
    pub fn new(options: &str, fstype: &str) -> State {
        let set = OptionSet::new(options, fstype);
        let rows = rows(&set);
        let sel = rows.iter().position(Row::selectable).unwrap_or(0);
        State {
            set,
            rows,
            sel,
            top: 0,
        }
    }

    /// Move the selection by `delta`, skipping headings and notes and
    /// stopping at the ends rather than wrapping.
    pub fn step(&mut self, delta: isize) {
        let n = self.rows.len() as isize;
        let mut i = self.sel as isize;
        loop {
            i += delta;
            if i < 0 || i >= n {
                return;
            }
            if self.rows[i as usize].selectable() {
                self.sel = i as usize;
                return;
            }
        }
    }

    pub fn first(&mut self) {
        self.sel = self.rows.iter().position(Row::selectable).unwrap_or(0);
    }

    pub fn last(&mut self) {
        self.sel = self.rows.iter().rposition(Row::selectable).unwrap_or(0);
    }

    fn clamp(&mut self, visible: usize) {
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
        let visible = visible.max(1);
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + visible {
            self.top = self.sel + 1 - visible;
        }
    }

    fn current(&self) -> Option<&Row> {
        self.rows.get(self.sel)
    }
}

/// Paint one frame of the menu.
pub fn draw(f: &mut Frame, st: &mut State) {
    let area = menu_rect(f.area());
    f.render_widget(Clear, area);
    let block = dialog_block("Options=");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Three lines are reserved at the bottom: the composed string, the help
    // for the focused row, and the keybindings. Truncated rather than
    // wrapped, so none of them can push the others out of the popup.
    let reserved = 3usize;
    let visible = (inner.height as usize).saturating_sub(reserved);
    st.clamp(visible);
    let width = inner.width as usize;
    let cut = |s: String| s.chars().take(width).collect::<String>();

    let mut lines: Vec<Line> = st
        .rows
        .iter()
        .enumerate()
        .skip(st.top)
        .take(visible)
        .map(|(i, r)| {
            let text = match r {
                Row::Heading(_) | Row::Note(_) => format!(" {}", label(&st.set, r)),
                _ => format!(
                    " {} {}",
                    if i == st.sel { ">" } else { " " },
                    label(&st.set, r)
                ),
            };
            let style = if i == st.sel && r.selectable() {
                Style::new().bold().reversed()
            } else {
                match r {
                    Row::Heading(_) => Style::new().bold().fg(Color::Cyan),
                    Row::Note(_) => Style::new().fg(Color::Yellow),
                    _ => Style::new(),
                }
            };
            Line::from(Span::styled(cut(text), style))
        })
        .collect();
    while lines.len() < visible {
        lines.push(Line::default());
    }

    let composed = st.set.text();
    let footer: [(String, Style); 3] = [
        (
            format!(
                " Options={}",
                if composed.is_empty() {
                    "(none)"
                } else {
                    &composed
                }
            ),
            Style::new().bold(),
        ),
        (
            format!(" {}", st.current().map(summary).unwrap_or_default()),
            Style::new().fg(Color::DarkGray),
        ),
        (
            " Space toggles   Enter edits a value   Ctrl-S applies   Esc discards".to_string(),
            Style::new().fg(Color::DarkGray),
        ),
    ];
    for (text, style) in footer {
        if lines.len() < inner.height as usize {
            lines.push(Line::from(Span::styled(cut(text), style)));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Run the menu over `options`. Returns the new option string, or `None` if
/// the user discarded the changes or the terminal went away -- in which case
/// the field is left exactly as it was.
pub fn run(term: &mut Term, bg: Background, options: &str, fstype: &str) -> Option<String> {
    let mut st = State::new(options, fstype);
    let original = st.set.text();

    loop {
        draw_over(term, bg, &mut |f| draw(f, &mut st));
        let k = term.next_key()?;
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => st.step(d as isize),
            _ if k.is_ctrl('s') => return Some(st.set.text()),
            _ => match k.code() {
                Some(KeyCode::Esc) => {
                    if st.set.text() == original
                        || dialogs::confirm(
                            term,
                            bg,
                            "Discard option changes",
                            &format!(
                                "Go back to:\n  Options={}\n\ndiscarding:\n  Options={}",
                                if original.is_empty() {
                                    "(none)"
                                } else {
                                    &original
                                },
                                st.set.text()
                            ),
                        )
                    {
                        return None;
                    }
                }
                Some(KeyCode::Enter) => activate(term, bg, &mut st, true),
                Some(KeyCode::Char(' ')) => activate(term, bg, &mut st, false),
                Some(KeyCode::Up) | Some(KeyCode::BackTab) => st.step(-1),
                Some(KeyCode::Down) | Some(KeyCode::Tab) => st.step(1),
                Some(KeyCode::Home) => st.first(),
                Some(KeyCode::End) => st.last(),
                Some(KeyCode::Delete) => {
                    if let Some(Row::Opt(spec)) = st.current() {
                        let key = spec.key;
                        st.set.disable(key);
                    }
                }
                _ if k.is_char('k') => st.step(-1),
                _ if k.is_char('j') => st.step(1),
                _ if k.is_char('?') => explain(term, bg, &st),
                _ => {}
            },
        }
    }
}

/// Act on the focused row.
///
/// Space always toggles. Enter toggles a flag too, but on a value option it
/// opens the prompt, because "turn this on" and "set it to something" are the
/// same action for `vers=` and different ones for `ro`.
fn activate(term: &mut Term, bg: Background, st: &mut State, enter: bool) {
    match st.current().cloned() {
        Some(Row::Extras) => {
            let cur = st.set.extras_text();
            if let Some(v) = dialogs::prompt(
                term,
                bg,
                "Other options",
                "Comma-separated options notcron does not offer a toggle for. \
                 Kept verbatim, in place.",
                &cur,
                &dialogs::no_validation,
            ) {
                st.set.set_extras(&v);
            }
        }
        Some(Row::Opt(spec)) => {
            let on = st.set.is_on(spec.key);
            match spec.kind {
                Kind::Flag => st.set.toggle(spec, None),
                Kind::Value if !enter => {
                    st.set.toggle(spec, Some(mountopts::suggested_value(spec)));
                }
                Kind::Value => {
                    let cur = match st.set.value_of(spec.key) {
                        Some(v) if !v.is_empty() => v.to_string(),
                        _ if on => String::new(),
                        _ => mountopts::suggested_value(spec),
                    };
                    if let Some(v) = ask_value(term, bg, spec, &cur) {
                        if v.trim().is_empty() {
                            st.set.disable(spec.key);
                        } else {
                            st.set.enable(spec, Some(v.trim().to_string()));
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Ask for an option's value.
///
/// Path-valued options open the file picker first, since browsing is the
/// point of having one; Escape there falls through to the text prompt rather
/// than cancelling, so neither route is a dead end.
fn ask_value(term: &mut Term, bg: Background, spec: &Spec, current: &str) -> Option<String> {
    if spec.path {
        if let Some(p) = picker::browse(term, bg, spec.key, picker::Mode::File, current) {
            return Some(p);
        }
    }
    let help = fieldhelp::entry(spec.help);
    let hint = help
        .map(|e| format!("{}\n\n  Examples: {}", e.summary, e.examples))
        .unwrap_or_else(|| format!("Value for {}=.", spec.key));
    let title = format!("{}=", spec.key);
    dialogs::prompt(term, bg, &title, &hint, current, &|s: &str| {
        if s.contains(',') {
            Err("a value cannot contain a comma; it separates options".into())
        } else {
            Ok(())
        }
    })
}

/// `?` on a row: the full help paragraph for that option.
fn explain(term: &mut Term, bg: Background, st: &State) {
    let Some(Row::Opt(spec)) = st.current() else {
        return;
    };
    let Some(e) = fieldhelp::entry(spec.help) else {
        return;
    };
    let body = format!("{}\n\n{}\n\nExamples: {}", e.summary, e.detail, e.examples);
    dialogs::pager(term, bg, &e.label, &wrap(&body, 76));
}

/// Hard-wrap at `width` columns, since the pager does not wrap for us.
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push('\n');
            continue;
        }
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push_str(&line);
                out.push('\n');
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::model::MountPreset;
    use crate::unit::mountopts::Family;
    use ratatui::backend::TestBackend;

    fn state(options: &str, fstype: &str) -> State {
        State::new(options, fstype)
    }

    #[test]
    fn the_menu_lists_the_generic_set_plus_the_family_extras() {
        let st = state("", "nfs");
        let keys: Vec<&str> = st
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Opt(s) => Some(s.key),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), mountopts::GENERIC.len() + mountopts::NFS.len());
        assert!(keys.starts_with(&["ro", "rw"]));
        assert!(keys.contains(&"soft") && keys.contains(&"retrans"));
        assert!(!keys.contains(&"credentials"));

        // A plain block device gets no extras block at all.
        let st = state("", "ext4");
        assert_eq!(
            st.rows
                .iter()
                .filter(|r| matches!(r, Row::Heading(_)))
                .count(),
            2,
            "generic + 'anything else' only"
        );
    }

    #[test]
    fn every_menu_always_offers_the_free_text_entry_and_the_automount_note() {
        for p in MountPreset::ALL {
            let st = state(p.options(), p.fstype());
            assert!(st.rows.contains(&Row::Extras), "{}", p.label());
            assert!(
                st.rows
                    .iter()
                    .any(|r| matches!(r, Row::Note(n) if n.contains("automount"))),
                "{}",
                p.label()
            );
        }
    }

    #[test]
    fn labels_show_state_values_and_placeholders() {
        let st = state("rw,vers=4.2,x-systemd.automount", "nfs");
        let find = |key: &str| {
            st.rows
                .iter()
                .find(|r| matches!(r, Row::Opt(s) if s.key == key))
                .map(|r| label(&st.set, r))
                .expect(key)
        };
        assert_eq!(find("rw"), "[x] rw");
        assert_eq!(find("ro"), "[ ] ro");
        assert_eq!(find("vers"), "[x] vers=4.2");
        assert_eq!(find("timeo"), "[ ] timeo=\u{2026}");
        assert_eq!(
            label(&st.set, &Row::Extras),
            "[x] other options   x-systemd.automount"
        );
        let empty = state("rw", "ext4");
        assert_eq!(label(&empty.set, &Row::Extras), "[ ] other options");
    }

    #[test]
    fn every_option_row_has_a_summary_to_show() {
        for fstype in ["ext4", "nfs", "cifs", "none"] {
            let st = state("", fstype);
            for r in &st.rows {
                if matches!(r, Row::Opt(_)) {
                    assert!(!summary(r).is_empty(), "{}", label(&st.set, r));
                }
            }
        }
    }

    #[test]
    fn movement_skips_headings_and_notes_and_stops_at_the_ends() {
        let mut st = state("", "cifs");
        assert!(st.rows[st.sel].selectable());
        // Walking the whole menu never lands on an unselectable row.
        for _ in 0..st.rows.len() * 2 {
            st.step(1);
            assert!(st.rows[st.sel].selectable());
        }
        // The last selectable row is Extras (the note sits below it).
        assert_eq!(st.rows[st.sel], Row::Extras);
        for _ in 0..st.rows.len() * 2 {
            st.step(-1);
            assert!(st.rows[st.sel].selectable());
        }
        assert!(matches!(&st.rows[st.sel], Row::Opt(s) if s.key == "ro"));
        st.last();
        assert_eq!(st.rows[st.sel], Row::Extras);
        st.first();
        assert!(matches!(&st.rows[st.sel], Row::Opt(s) if s.key == "ro"));
    }

    /// ratatui panics on an out-of-bounds rect rather than clipping, so the
    /// menu has to survive terminals nobody sensible uses -- and the footer
    /// reserves three lines, which is where that goes wrong first.
    #[test]
    fn draws_at_every_size_without_panicking() {
        for (options, fstype) in [
            ("", "ext4"),
            ("rw,soft,timeo=100,noatime,_netdev", "nfs"),
            (
                "rw,credentials=/etc/creds,uid=0,x-systemd.automount",
                "cifs",
            ),
            ("bind", "none"),
        ] {
            let mut st = state(options, fstype);
            for (w, h) in [
                (1, 1),
                (2, 2),
                (3, 4),
                (10, 3),
                (20, 5),
                (40, 10),
                (80, 24),
                (200, 60),
            ] {
                let mut t = Terminal::new(TestBackend::new(w, h)).expect("backend");
                t.draw(|f| draw(f, &mut st)).expect("draw");
            }
        }
    }

    #[test]
    fn drawing_a_scrolled_selection_keeps_it_in_view() {
        let mut st = state("", "nfs");
        st.last();
        let mut t = Terminal::new(TestBackend::new(60, 12)).expect("backend");
        t.draw(|f| draw(f, &mut st)).expect("draw");
        assert!(st.sel >= st.top, "selection scrolled off the top");
        assert!(st.top > 0, "a 12-row frame cannot show the whole NFS menu");
    }

    #[test]
    fn menu_rect_never_leaves_the_frame() {
        for w in 0..12u16 {
            for h in 0..12u16 {
                let a = Rect {
                    x: 2,
                    y: 3,
                    width: w,
                    height: h,
                };
                let r = menu_rect(a);
                assert!(r.x >= a.x && r.y >= a.y, "{w}x{h}");
                assert!(r.x + r.width <= a.x + a.width, "{w}x{h}");
                assert!(r.y + r.height <= a.y + a.height, "{w}x{h}");
            }
        }
        let zero = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!((menu_rect(zero).width, menu_rect(zero).height), (0, 0));
    }

    #[test]
    fn help_wrapping_respects_the_width_and_keeps_the_words() {
        let src = "one two three four five six seven eight nine ten";
        let out = wrap(src, 20);
        for line in out.lines() {
            assert!(line.chars().count() <= 20, "{line:?}");
        }
        assert_eq!(
            out.split_whitespace().collect::<Vec<_>>(),
            src.split_whitespace().collect::<Vec<_>>()
        );
        // Blank lines between paragraphs survive.
        assert!(wrap("a\n\nb", 20).contains("\n\n"));
    }

    #[test]
    fn a_credentials_option_is_wired_to_the_picker() {
        let spec = mountopts::spec_for(Family::Cifs, "credentials").expect("credentials");
        assert!(spec.path, "credentials= must offer the file picker");
        // And nothing else claims to be a path, so the picker never opens
        // where typing is the only sensible route.
        for f in [Family::Generic, Family::Nfs, Family::Cifs, Family::Bind] {
            for s in mountopts::offered(f) {
                assert_eq!(s.path, s.key == "credentials", "{}", s.key);
            }
        }
    }
}
