//! Undo for `x`: what removal put in the trash, and how to get it back.
//!
//! Restoring never clobbers. [`crate::trash::Trash::conflicts`] is asked
//! first, the user is shown exactly which paths would be replaced, and only
//! then is `restore` called with `overwrite`. A refused prompt moves nothing.

use super::dialogs::{self, Background};
use super::term::{popup_rect, Key, Term};
use crate::systemd;
use crate::trash::{RestoreError, Trash, TrashEntry};
use crate::unit::model::Scope;
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Now, in epoch seconds -- the unit `TrashEntry::removed_at` uses.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Newest first: the thing just deleted by mistake is the thing wanted back.
pub fn newest_first(mut entries: Vec<TrashEntry>) -> Vec<TrashEntry> {
    entries.sort_by(|a, b| b.removed_at.cmp(&a.removed_at).then(a.id.cmp(&b.id)));
    entries
}

/// A human age: the coarsest unit that still says something useful.
pub fn age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// One row of the list, clipped to `width`.
pub fn row(e: &TrashEntry, now: u64, width: usize) -> String {
    let mut flags = String::new();
    if e.was_enabled {
        flags.push_str(" enabled");
    }
    if e.was_active {
        flags.push_str(" active");
    }
    let text = format!(
        "{:<32} {:>10}  {} file{}{}",
        e.unit,
        age(e.age_secs(now)),
        e.files.len(),
        if e.files.len() == 1 { "" } else { "s" },
        flags
    );
    clip(&text, width)
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

/// The view's rect: a wide popup, sized down rather than clipped so a tiny
/// terminal still renders something in bounds.
pub fn trash_rect(area: Rect) -> Rect {
    popup_rect(
        area,
        area.width.saturating_sub(4).max(1),
        area.height.saturating_sub(4).max(1),
    )
}

/// The empty-trash message, which is a screen in its own right: an empty list
/// with no explanation reads as a broken feature.
const EMPTY: &str = "\
Nothing in the trash.

Units removed with x are stashed here first, so a removal can be
undone. They are pruned as newer ones arrive.";

/// Run the trash browser. Returns a status line for the caller to show, or
/// `None` when nothing happened.
pub fn run(term: &mut Term, bg: Background, scope: Scope) -> Option<String> {
    let trash = Trash::for_scope(scope);
    let mut entries = match trash.list() {
        Ok(e) => newest_first(e),
        Err(e) => {
            dialogs::msgbox(term, bg, "Trash unreadable", &e);
            return None;
        }
    };
    if entries.is_empty() {
        dialogs::msgbox(term, bg, "Trash", EMPTY);
        return None;
    }

    let mut sel = 0usize;
    let mut status = String::new();
    loop {
        let now = now_secs();
        let (rows, s) = (entries.clone(), sel);
        let note = status.clone();
        // The row each list line sits on, so a click can be mapped back.
        let mut first_row = 0u16;
        let mut visible = 1usize;
        dialogs::draw_over(term, bg, &mut |f| {
            let area = trash_rect(f.area());
            f.render_widget(Clear, area);
            let title = format!(
                " Trash ({}) -- Enter restores, x discards, q closes ",
                rows.len()
            );
            let block = dialogs::dialog_block(&title);
            let inner = block.inner(area);
            f.render_widget(block, area);
            first_row = inner.y;
            let body_h = inner.height.saturating_sub(2) as usize;
            visible = body_h.max(1);
            let top = s.saturating_sub(visible.saturating_sub(1));
            let width = inner.width as usize;
            let mut lines: Vec<Line> = rows
                .iter()
                .enumerate()
                .skip(top)
                .take(visible)
                .map(|(i, e)| {
                    let text = format!(
                        "{} {}",
                        if i == s { '>' } else { ' ' },
                        row(e, now, width.saturating_sub(2))
                    );
                    Line::from(Span::styled(
                        clip(&text, width),
                        if i == s {
                            Style::new().bold().reversed()
                        } else {
                            Style::new()
                        },
                    ))
                })
                .collect();
            if !note.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    clip(&note, width),
                    Style::new().fg(Color::Yellow),
                )));
            }
            f.render_widget(Paragraph::new(lines), inner);
        });

        let k = term.next_key()?;
        status.clear();
        match k {
            Key::Resize => continue,
            Key::Scroll(d) => sel = step(sel, d as isize, entries.len()),
            Key::Click(_, y) | Key::DoubleClick(_, y) => {
                // The visible window starts at whichever row keeps the
                // selection on screen; map the click through the same offset.
                let top = sel.saturating_sub(visible.saturating_sub(1));
                if y >= first_row {
                    let i = top + (y - first_row) as usize;
                    if i < entries.len() {
                        sel = i;
                        if matches!(k, Key::DoubleClick(..)) {
                            let e = entries[sel].clone();
                            status = restore(term, bg, &trash, &e, &mut entries, &mut sel);
                        }
                    }
                }
            }
            _ => match k.code() {
                Some(KeyCode::Esc) => return finish(status),
                Some(KeyCode::Up) => sel = step(sel, -1, entries.len()),
                Some(KeyCode::Down) => sel = step(sel, 1, entries.len()),
                Some(KeyCode::Home) => sel = 0,
                Some(KeyCode::End) => sel = entries.len().saturating_sub(1),
                Some(KeyCode::Enter) => {
                    let e = entries[sel].clone();
                    status = restore(term, bg, &trash, &e, &mut entries, &mut sel);
                    if entries.is_empty() {
                        return finish(status);
                    }
                }
                _ if k.is_char('q') => return finish(status),
                _ if k.is_char('k') => sel = step(sel, -1, entries.len()),
                _ if k.is_char('j') => sel = step(sel, 1, entries.len()),
                _ if k.is_char('r') => {
                    let e = entries[sel].clone();
                    status = restore(term, bg, &trash, &e, &mut entries, &mut sel);
                    if entries.is_empty() {
                        return finish(status);
                    }
                }
                _ if k.is_char('x') => {
                    status = discard(term, bg, &trash, &mut entries, &mut sel);
                    if entries.is_empty() {
                        return finish(status);
                    }
                }
                _ => {}
            },
        }
    }
}

fn finish(status: String) -> Option<String> {
    if status.is_empty() {
        None
    } else {
        Some(status)
    }
}

fn step(sel: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    ((sel as isize + delta).rem_euclid(len as isize)) as usize
}

/// The consent text for an overwrite. Split out so the test can assert every
/// path the user is about to lose is named.
pub fn conflict_body(unit: &str, paths: &[std::path::PathBuf]) -> String {
    format!(
        "Restoring {unit} would replace {} file{} that exist{} again:\n\n  {}\n\n\
         Overwrite them?",
        paths.len(),
        if paths.len() == 1 { "" } else { "s" },
        if paths.len() == 1 { "s" } else { "" },
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ")
    )
}

/// Restore one entry, asking before replacing anything.
fn restore(
    term: &mut Term,
    bg: Background,
    trash: &Trash,
    entry: &TrashEntry,
    entries: &mut Vec<TrashEntry>,
    sel: &mut usize,
) -> String {
    // Ask what is in the way *before* touching anything.
    let overwrite = match trash.conflicts(&entry.id) {
        Ok(paths) if paths.is_empty() => false,
        Ok(paths) => {
            if !dialogs::confirm(
                term,
                bg,
                "Restore would overwrite",
                &conflict_body(&entry.unit, &paths),
            ) {
                return "restore cancelled -- nothing was moved".into();
            }
            true
        }
        Err(e) => {
            dialogs::msgbox(term, bg, "Cannot restore", &e.to_string());
            return String::new();
        }
    };

    let report = match trash.restore(&entry.id, overwrite) {
        Ok(r) => r,
        // A unit that reappeared between the check and the move: ask again
        // rather than forcing, and only retry once.
        Err(RestoreError::Conflict(paths)) => {
            if !dialogs::confirm(
                term,
                bg,
                "Restore would overwrite",
                &conflict_body(&entry.unit, &paths),
            ) {
                return "restore cancelled -- nothing was moved".into();
            }
            match trash.restore(&entry.id, true) {
                Ok(r) => r,
                Err(e) => {
                    dialogs::msgbox(term, bg, "Restore failed", &e.to_string());
                    return String::new();
                }
            }
        }
        Err(e) => {
            dialogs::msgbox(term, bg, "Restore failed", &e.to_string());
            return String::new();
        }
    };

    entries.retain(|e| e.id != entry.id);
    *sel = (*sel).min(entries.len().saturating_sub(1));

    let _ = systemd::daemon_reload(report.scope);
    let mut msg = format!(
        "restored {} ({} file{})",
        report.unit,
        report.restored.len(),
        if report.restored.len() == 1 { "" } else { "s" }
    );
    if !report.overwritten.is_empty() {
        msg.push_str(&format!(", {} overwritten", report.overwritten.len()));
    }

    // The unit is back on disk but not enabled or running; offer what it was.
    if report.was_enabled || report.was_active {
        let want = describe_state(report.was_enabled, report.was_active);
        if dialogs::confirm(
            term,
            bg,
            "Put it back as it was",
            &format!(
                "{} was {want} when it was removed.\n\nRestore that too?",
                report.unit
            ),
        ) {
            let mut args: Vec<&str> = vec!["enable"];
            if report.was_active {
                args.push("--now");
            }
            if !report.was_enabled {
                args = vec!["start"];
            }
            args.push(&report.unit);
            match systemd::systemctl(report.scope, &args) {
                Ok(_) => msg.push_str(&format!(", {want} again")),
                Err(e) => dialogs::msgbox(term, bg, "Could not restore its state", e.trim()),
            }
        }
    }
    msg
}

/// "enabled", "running" or "enabled and running".
pub fn describe_state(enabled: bool, active: bool) -> &'static str {
    match (enabled, active) {
        (true, true) => "enabled and running",
        (true, false) => "enabled",
        (false, true) => "running",
        (false, false) => "neither enabled nor running",
    }
}

/// Delete one entry from the trash for good.
fn discard(
    term: &mut Term,
    bg: Background,
    trash: &Trash,
    entries: &mut Vec<TrashEntry>,
    sel: &mut usize,
) -> String {
    let Some(entry) = entries.get(*sel).cloned() else {
        return String::new();
    };
    if !dialogs::confirm(
        term,
        bg,
        "Discard from trash",
        &format!(
            "Delete the stashed copy of {} for good?\n\nThis cannot be undone.",
            entry.unit
        ),
    ) {
        return String::new();
    }
    match trash.discard(&entry.id) {
        Ok(()) => {
            entries.retain(|e| e.id != entry.id);
            *sel = (*sel).min(entries.len().saturating_sub(1));
            format!("discarded {}", entry.unit)
        }
        Err(e) => {
            dialogs::msgbox(term, bg, "Discard failed", &e);
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash::TrashedFile;
    use std::path::PathBuf;

    fn entry(id: &str, unit: &str, removed_at: u64, files: usize) -> TrashEntry {
        TrashEntry {
            id: id.into(),
            unit: unit.into(),
            scope: Scope::User,
            removed_at,
            was_enabled: true,
            was_active: false,
            files: (0..files)
                .map(|i| TrashedFile {
                    stored: format!("{i}.unit"),
                    original: PathBuf::from(format!("/tmp/{unit}.{i}")),
                })
                .collect(),
        }
    }

    #[test]
    fn the_newest_removal_is_offered_first() {
        let v = newest_first(vec![
            entry("a", "old.timer", 100, 1),
            entry("c", "newest.timer", 300, 1),
            entry("b", "middling.timer", 200, 1),
        ]);
        assert_eq!(
            v.iter().map(|e| e.unit.as_str()).collect::<Vec<_>>(),
            vec!["newest.timer", "middling.timer", "old.timer"]
        );
    }

    /// Two removals in the same second must still come out in a fixed order.
    #[test]
    fn entries_removed_at_the_same_instant_order_by_id() {
        let v = newest_first(vec![
            entry("z", "z.timer", 100, 1),
            entry("a", "a.timer", 100, 1),
        ]);
        assert_eq!(v[0].id, "a");
        assert_eq!(newest_first(Vec::new()).len(), 0);
    }

    #[test]
    fn ages_read_in_the_coarsest_useful_unit() {
        assert_eq!(age(0), "0s ago");
        assert_eq!(age(59), "59s ago");
        assert_eq!(age(60), "1m ago");
        assert_eq!(age(3599), "59m ago");
        assert_eq!(age(3600), "1h ago");
        assert_eq!(age(86_399), "23h ago");
        assert_eq!(age(86_400), "1d ago");
        assert_eq!(age(86_400 * 400), "400d ago");
    }

    #[test]
    fn a_row_names_the_unit_its_age_and_what_it_was() {
        let e = entry("x", "notcron-backup.timer", 1_000, 2);
        let r = row(&e, 4_600, 200);
        assert!(r.contains("notcron-backup.timer"), "{r}");
        assert!(r.contains("1h ago"), "{r}");
        assert!(r.contains("2 files"), "{r}");
        assert!(r.contains("enabled"), "{r}");
        assert!(!r.contains(" active"), "{r}");
    }

    #[test]
    fn a_single_file_entry_is_not_pluralised() {
        let r = row(&entry("x", "a.service", 0, 1), 0, 200);
        assert!(r.contains("1 file "), "{r}");
    }

    #[test]
    fn rows_never_exceed_their_width() {
        let e = entry("x", &"a".repeat(200), 0, 3);
        for w in 0..90usize {
            assert!(row(&e, 500, w).chars().count() <= w, "width {w}");
        }
    }

    /// An age computed against a clock behind the removal must not underflow.
    #[test]
    fn a_removal_in_the_future_does_not_panic() {
        let e = entry("x", "a.timer", 9_000, 1);
        assert!(!row(&e, 0, 80).is_empty());
    }

    // -----------------------------------------------------------------
    // Layout
    // -----------------------------------------------------------------

    #[test]
    fn the_view_stays_inside_every_frame() {
        for w in 0..=40u16 {
            for h in 0..=40u16 {
                let area = Rect {
                    x: 3,
                    y: 2,
                    width: w,
                    height: h,
                };
                let r = trash_rect(area);
                assert!(r.x >= area.x && r.y >= area.y, "{w}x{h}: {r:?}");
                assert!(r.x + r.width <= area.x + area.width, "{w}x{h}: {r:?}");
                assert!(r.y + r.height <= area.y + area.height, "{w}x{h}: {r:?}");
            }
        }
    }

    #[test]
    fn the_view_fills_a_normal_frame() {
        let r = trash_rect(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        });
        assert_eq!((r.width, r.height), (96, 36));
    }

    // -----------------------------------------------------------------
    // Consent
    // -----------------------------------------------------------------

    /// Every path about to be replaced must be named: this is the text that
    /// stands between the user and losing a file.
    #[test]
    fn the_overwrite_prompt_names_every_path() {
        let paths = vec![
            PathBuf::from("/home/x/.config/systemd/user/a.timer"),
            PathBuf::from("/home/x/.config/systemd/user/a.service"),
        ];
        let body = conflict_body("a.timer", &paths);
        for p in &paths {
            assert!(body.contains(&p.display().to_string()), "{body}");
        }
        assert!(body.contains("2 files"), "{body}");
        assert!(body.contains("Overwrite"), "{body}");
    }

    #[test]
    fn the_overwrite_prompt_is_grammatical_for_one_file() {
        let body = conflict_body("a.timer", &[PathBuf::from("/tmp/a.timer")]);
        assert!(body.contains("1 file that exists again"), "{body}");
    }

    #[test]
    fn restored_state_is_described_in_words() {
        assert_eq!(describe_state(true, true), "enabled and running");
        assert_eq!(describe_state(true, false), "enabled");
        assert_eq!(describe_state(false, true), "running");
        assert_eq!(describe_state(false, false), "neither enabled nor running");
    }

    #[test]
    fn selection_wraps_and_survives_an_empty_list() {
        assert_eq!(step(0, -1, 3), 2);
        assert_eq!(step(2, 1, 3), 0);
        assert_eq!(step(0, 1, 0), 0);
        assert_eq!(step(5, -1, 0), 0);
    }

    #[test]
    fn a_status_that_says_nothing_is_not_reported() {
        assert_eq!(finish(String::new()), None);
        assert_eq!(
            finish("restored a.timer".into()),
            Some("restored a.timer".into())
        );
    }
}
