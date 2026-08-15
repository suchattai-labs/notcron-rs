//! Modal dialogs: message box, confirm, single-line prompt, list picker and
//! a scrollable pager. Each runs a small event loop and repaints the
//! caller-supplied background underneath itself.

use super::term::{popup_rect, Key, Term};
use crate::complete::Completion;
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::time::Duration;

pub type Background<'a> = &'a mut dyn FnMut(&mut Frame);

pub fn dialog_block(title: &str) -> Block<'_> {
    Block::default()
        .title(format!(" {title} "))
        .title_style(Style::new().bold())
        .borders(Borders::ALL)
}

pub fn draw_over(term: &mut Term, bg: Background, draw: &mut dyn FnMut(&mut Frame)) {
    let _ = term.terminal.draw(|f| {
        bg(f);
        draw(f);
    });
}

/// Modal message box; any key dismisses it.
pub fn msgbox(term: &mut Term, bg: Background, title: &str, body: &str) {
    loop {
        let (t, b) = (title.to_string(), body.to_string());
        draw_over(term, bg, &mut |f| {
            let lines = b.lines().count().min(20) as u16;
            let area = popup_rect(f.area(), 70, lines + 4);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("{b}\n\nPress any key to close"))
                    .wrap(Wrap { trim: false })
                    .block(dialog_block(&t)),
                area,
            );
        });
        match term.next_key() {
            None | Some(Key::Press(..)) | Some(Key::Click(..)) | Some(Key::DoubleClick(..)) => {
                return
            }
            Some(Key::Resize) | Some(Key::Scroll(_)) => continue,
        }
    }
}

/// Modal yes/no. `false` is the default and the Escape answer.
pub fn confirm(term: &mut Term, bg: Background, title: &str, body: &str) -> bool {
    let mut yes = false;
    loop {
        let (t, b) = (title.to_string(), body.to_string());
        draw_over(term, bg, &mut |f| {
            let lines = b.lines().count().min(16) as u16;
            let area = popup_rect(f.area(), 70, lines + 5);
            f.render_widget(Clear, area);
            let btn = |label: &str, focused: bool| {
                if focused {
                    format!("[({label})]")
                } else {
                    format!("[ {label} ]")
                }
            };
            let text = format!("{b}\n\n  {}   {}", btn("Yes", yes), btn("No", !yes));
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(dialog_block(&t)),
                area,
            );
        });
        match term.next_key() {
            None => return false,
            Some(Key::Resize) | Some(Key::Scroll(_)) => continue,
            Some(Key::Click(..)) | Some(Key::DoubleClick(..)) => continue,
            Some(k) => match k.code() {
                Some(KeyCode::Left | KeyCode::Right | KeyCode::Tab) => yes = !yes,
                Some(KeyCode::Enter) => return yes,
                Some(KeyCode::Esc) => return false,
                _ if k.is_char('y') || k.is_char('Y') => return true,
                _ if k.is_char('n') || k.is_char('N') => return false,
                _ => {}
            },
        }
    }
}

/// Tab completion for a field: the whole field text plus the state of the
/// Ctrl-A toggle in, a set of full replacements for it out.
pub type Completer<'a> = &'a dyn Fn(&str, bool) -> Completion;

/// Extra lines under the field, computed from what is typed so far.
pub type Note<'a> = &'a dyn Fn(&str) -> Vec<String>;

/// Optional behaviours a prompt can take on.
///
/// Kept as a struct rather than more arguments because most prompts want
/// none of it: [`prompt`] is the plain form and passes `PromptOpts::default()`.
#[derive(Default)]
pub struct PromptOpts<'a> {
    /// Tab completion for this field. The `bool` is the state of the
    /// Ctrl-A toggle, for completers that offer a wider set on request.
    pub complete: Option<Completer<'a>>,
    /// What Ctrl-A widens to, e.g. `all accounts`. `None` hides the toggle
    /// and pins the completer's flag to `false`.
    pub toggle: Option<&'a str>,
    /// Extra lines under the field, recomputed once typing pauses. Used for
    /// the next-run preview, which costs a subprocess and must not run on
    /// every keystroke.
    pub note: Option<Note<'a>>,
}

/// How long typing has to stop before a [`PromptOpts::note`] is recomputed.
const NOTE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Single-line text prompt. Returns `None` on Escape.
///
/// `validate` runs on Enter; returning `Err` keeps the dialog open and shows
/// the message, which is how invalid calendar specs and time spans are caught
/// before they reach a unit file.
pub fn prompt(
    term: &mut Term,
    bg: Background,
    title: &str,
    help: &str,
    initial: &str,
    validate: &dyn Fn(&str) -> Result<(), String>,
) -> Option<String> {
    prompt_ext(
        term,
        bg,
        title,
        help,
        initial,
        validate,
        PromptOpts::default(),
    )
}

/// The full prompt: [`prompt`] plus completion and a live note.
///
/// Tab completes to the longest common prefix first and lists the candidates
/// on the next press, which is the shell contract. Tab is free here because
/// the prompt is modal -- it still moves between fields in the form behind it.
#[allow(clippy::too_many_arguments)]
pub fn prompt_ext(
    term: &mut Term,
    bg: Background,
    title: &str,
    help: &str,
    initial: &str,
    validate: &dyn Fn(&str) -> Result<(), String>,
    opts: PromptOpts<'_>,
) -> Option<String> {
    let mut value = initial.to_string();
    let mut err = String::new();
    // Candidates are only listed once completing has stopped making progress,
    // so a unique match never flashes a one-item list at the user.
    let mut candidates: Vec<String> = Vec::new();
    let mut all = false;
    let mut note: Vec<String> = Vec::new();
    // The value `note` describes; `None` means "stale, recompute when the
    // user pauses".
    let mut note_for: Option<String> = None;

    loop {
        let (t, h, v, e) = (
            title.to_string(),
            help.to_string(),
            value.clone(),
            err.clone(),
        );
        let cands = candidates.clone();
        let note_now = note.clone();
        let keys = keybinding_line(&opts, all);
        draw_over(term, bg, &mut |f| {
            // The width is fixed, so the wrapped height can be measured
            // before the rect is chosen. It has to be: a multi-line help --
            // a field summary plus its examples -- used to push the error
            // and the keybindings out of a box that was always nine rows,
            // which meant a rejected value looked like a dead keyboard.
            let width = 74u16.min(f.area().width.saturating_sub(2)).max(1);
            let inner_w = width.saturating_sub(4) as usize;
            let shown: String = if v.chars().count() > inner_w {
                v.chars().skip(v.chars().count() - inner_w).collect()
            } else {
                v.clone()
            };
            let body = format!(
                "{h}\n\n  {shown}_\n{}{}\n{}{keys}",
                block_of(&candidate_lines(&cands, inner_w)),
                block_of(&note_now),
                if e.is_empty() {
                    String::new()
                } else {
                    format!("  ! {e}\n")
                }
            );
            let height = wrapped_height(&body, inner_w.max(1)) as u16 + 2;
            let area = popup_rect(f.area(), width, height);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(body)
                    .wrap(Wrap { trim: false })
                    .block(dialog_block(&t)),
                area,
            );
        });

        // A stale note is recomputed only once typing pauses: it may cost a
        // subprocess, and one per keystroke would be unusable.
        let k = match (&opts.note, note_for.as_deref() == Some(value.as_str())) {
            (Some(f), false) => match term.poll_key(NOTE_DEBOUNCE) {
                Some(k) => k,
                None => {
                    note = f(&value);
                    note_for = Some(value.clone());
                    continue;
                }
            },
            _ => term.next_key()?,
        };

        match k {
            Key::Resize | Key::Scroll(_) | Key::Click(..) | Key::DoubleClick(..) => continue,
            _ if k.is_ctrl('a') && opts.toggle.is_some() => {
                all = !all;
                candidates.clear();
                err.clear();
            }
            _ if k.is_ctrl('u') => {
                value.clear();
                candidates.clear();
                err.clear();
            }
            _ => match k.code() {
                Some(KeyCode::Esc) => return None,
                Some(KeyCode::Enter) => match validate(&value) {
                    Ok(()) => return Some(value),
                    Err(e) => err = e,
                },
                Some(KeyCode::Tab) | Some(KeyCode::BackTab) => {
                    if let Some(f) = opts.complete {
                        let c = f(&value, all && opts.toggle.is_some());
                        candidates.clear();
                        err.clear();
                        if c.is_empty() {
                            err = "no completions".into();
                        } else if c.common != value {
                            // `common` is a full replacement for the field,
                            // never something to append to it.
                            value = c.common;
                        } else if !c.is_unique() {
                            candidates = c.candidates;
                        }
                    }
                }
                Some(KeyCode::Backspace) => {
                    value.pop();
                    candidates.clear();
                    err.clear();
                }
                Some(KeyCode::Char(c)) if !k.is_ctrl(c) => {
                    value.push(c);
                    candidates.clear();
                    err.clear();
                }
                _ => {}
            },
        }
    }
}

/// Render a group of lines as a block, or nothing at all when it is empty --
/// so an absent note costs no vertical space.
fn block_of(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", lines.join("\n"))
    }
}

/// Candidates as displayable lines: at most [`CANDIDATE_ROWS`] of them, each
/// clipped to the box, with a count of whatever did not fit.
pub fn candidate_lines(candidates: &[String], width: usize) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let width = width.max(8);
    let mut out: Vec<String> = candidates
        .iter()
        .take(CANDIDATE_ROWS)
        .map(|c| {
            let text = format!("  - {c}");
            text.chars().take(width).collect()
        })
        .collect();
    if candidates.len() > CANDIDATE_ROWS {
        out.push(format!(
            "  ... and {} more",
            candidates.len() - CANDIDATE_ROWS
        ));
    }
    out
}

/// How many completion candidates the prompt lists before summarising the
/// rest. Small enough that the box still fits an 80x24 terminal.
pub const CANDIDATE_ROWS: usize = 8;

/// The bottom line of a prompt, naming only the keys this prompt has.
pub fn keybinding_line(opts: &PromptOpts<'_>, all: bool) -> String {
    let mut parts = vec!["  Enter accepts".to_string(), "Esc cancels".to_string()];
    if opts.complete.is_some() {
        parts.push("Tab completes".into());
    }
    if let Some(label) = opts.toggle {
        parts.push(format!(
            "Ctrl-A {} {label}",
            if all { "hides" } else { "shows" }
        ));
    }
    parts.join(", ")
}

/// Anything is accepted.
pub fn no_validation(_: &str) -> Result<(), String> {
    Ok(())
}

/// How many lines `text` occupies once word-wrapped at `width`, matching what
/// `Paragraph` with `Wrap` does closely enough to size a box around it.
///
/// A word longer than the width is not broken by this count but is by the
/// renderer, so it is charged the rows it will actually take.
pub fn wrapped_height(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 0usize;
    for line in text.split('\n') {
        let mut used = 0usize;
        let mut wrote = false;
        for word in line.split_whitespace() {
            let w = word.chars().count();
            if wrote && used + 1 + w > width {
                rows += 1;
                used = 0;
                wrote = false;
            }
            if wrote {
                used += 1;
            }
            // An over-long word spills onto further rows of its own.
            if w > width {
                rows += (w - 1) / width;
                used = w % width;
            } else {
                used += w;
            }
            wrote = true;
        }
        rows += 1;
    }
    rows
}

/// Vertical list picker. Returns the chosen index, or `None` on Escape.
pub fn pick(
    term: &mut Term,
    bg: Background,
    title: &str,
    items: &[String],
    initial: usize,
) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let mut sel = initial.min(items.len() - 1);
    loop {
        let (t, rows) = (title.to_string(), items.to_vec());
        let s = sel;
        draw_over(term, bg, &mut |f| {
            let h = (rows.len() as u16).min(18) + 2;
            let w = rows.iter().map(|r| r.len()).max().unwrap_or(20) as u16 + 8;
            let area = popup_rect(f.area(), w.max(30), h);
            f.render_widget(Clear, area);
            let visible = area.height.saturating_sub(2) as usize;
            let top = s.saturating_sub(visible.saturating_sub(1));
            let text: String = rows
                .iter()
                .enumerate()
                .skip(top)
                .take(visible)
                .map(|(i, r)| {
                    if i == s {
                        format!(" > {r}\n")
                    } else {
                        format!("   {r}\n")
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(text).block(dialog_block(&t)), area);
        });
        let k = term.next_key()?;
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => {
                sel = sel.saturating_add_signed(d as isize).min(items.len() - 1);
            }
            _ => match k.code() {
                Some(KeyCode::Esc) => return None,
                Some(KeyCode::Enter) => return Some(sel),
                Some(KeyCode::Up) => sel = sel.saturating_sub(1),
                Some(KeyCode::Down) => sel = (sel + 1).min(items.len() - 1),
                Some(KeyCode::Home) => sel = 0,
                Some(KeyCode::End) => sel = items.len() - 1,
                Some(KeyCode::Char(c)) if c.is_ascii_digit() && c != '0' => {
                    let i = c as usize - '1' as usize;
                    if i < items.len() {
                        return Some(i);
                    }
                }
                _ if k.is_char('k') => sel = sel.saturating_sub(1),
                _ if k.is_char('j') => sel = (sel + 1).min(items.len() - 1),
                _ => {}
            },
        }
    }
}

/// Scrollable read-only text view, for previews, `systemctl status` and the
/// journal.
pub fn pager(term: &mut Term, bg: Background, title: &str, body: &str) {
    let lines: Vec<String> = body.lines().map(|l| l.to_string()).collect();
    let mut top = 0usize;
    loop {
        let (t, rows) = (title.to_string(), lines.clone());
        let tp = top;
        let mut page = 1usize;
        draw_over(term, bg, &mut |f| {
            let area = popup_rect(f.area(), f.area().width.saturating_sub(4), f.area().height);
            f.render_widget(Clear, area);
            let visible = area.height.saturating_sub(2) as usize;
            page = visible.max(1);
            let text: String = rows
                .iter()
                .skip(tp)
                .take(visible)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            f.render_widget(
                Paragraph::new(text).block(dialog_block(&format!(
                    "{t}  [{}/{}]  q closes",
                    (tp + 1).min(rows.len().max(1)),
                    rows.len().max(1)
                ))),
                area,
            );
        });
        let max_top = lines.len().saturating_sub(1);
        let Some(k) = term.next_key() else { return };
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => {
                top = top.saturating_add_signed(3 * d as isize).min(max_top);
            }
            _ => match k.code() {
                Some(KeyCode::Esc | KeyCode::Enter) => return,
                Some(KeyCode::Up) => top = top.saturating_sub(1),
                Some(KeyCode::Down) => top = (top + 1).min(max_top),
                Some(KeyCode::PageUp) => top = top.saturating_sub(page),
                Some(KeyCode::PageDown) => top = (top + page).min(max_top),
                Some(KeyCode::Home) => top = 0,
                Some(KeyCode::End) => top = max_top,
                _ if k.is_char('q') => return,
                _ if k.is_char('k') => top = top.saturating_sub(1),
                _ if k.is_char('j') => top = (top + 1).min(max_top),
                _ => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_counts_plain_lines() {
        assert_eq!(wrapped_height("", 20), 1);
        assert_eq!(wrapped_height("short", 20), 1);
        assert_eq!(wrapped_height("a\nb\nc", 20), 3);
        // Blank lines still occupy a row.
        assert_eq!(wrapped_height("a\n\nb", 20), 3);
    }

    #[test]
    fn wrapping_counts_the_rows_a_paragraph_really_takes() {
        // 30 characters of words at width 10 cannot fit on fewer than 3 rows.
        let text = "one two three four five six seven";
        for width in [8usize, 10, 20, 40] {
            let rows = wrapped_height(text, width);
            assert!(rows >= text.len().div_ceil(width), "{width}: {rows}");
            assert!(rows <= text.split_whitespace().count(), "{width}: {rows}");
        }
        // A single unbreakable word spills across rows.
        assert_eq!(wrapped_height(&"x".repeat(25), 10), 3);
        assert_eq!(wrapped_height(&"x".repeat(10), 10), 1);
    }

    /// The prompt sizes itself from its help text. A field summary plus its
    /// examples runs to several lines, and the box has to keep the error and
    /// the keybindings visible underneath -- otherwise a rejected value is
    /// indistinguishable from a dead keyboard.
    #[test]
    fn a_prompt_box_grows_to_fit_a_long_help_and_its_error() {
        let help = "Read username/password from a root-only file instead of the \
                    options string.\n\n  Examples: credentials=/etc/cifs-credentials, \
                    credentials=/root/.smbcreds-nas";
        let body = format!(
            "{help}\n\n  /etc/creds_\n\n  ! a value cannot contain a comma; it \
             separates options\n  Enter accepts, Esc cancels"
        );
        let inner_w = 70usize;
        let height = wrapped_height(&body, inner_w) + 2;
        assert!(height > 9, "the old fixed height would have clipped this");
        // Everything fits: the box is at least as tall as the text plus its
        // two border rows.
        assert!(height >= body.lines().count() + 2);
        // And it still fits on a normal terminal.
        assert!(
            height <= 24,
            "{height} rows is too tall for an 80x24 screen"
        );
    }

    // -----------------------------------------------------------------
    // Completion and the live note
    // -----------------------------------------------------------------

    #[test]
    fn candidates_are_listed_up_to_a_limit_and_then_counted() {
        assert!(candidate_lines(&[], 40).is_empty());
        let few: Vec<String> = (0..3).map(|i| format!("/bin/x{i}")).collect();
        let lines = candidate_lines(&few, 40);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("/bin/x0"));

        let many: Vec<String> = (0..30).map(|i| format!("/bin/x{i}")).collect();
        let lines = candidate_lines(&many, 40);
        assert_eq!(lines.len(), CANDIDATE_ROWS + 1);
        assert!(lines.last().unwrap().contains("22 more"), "{lines:?}");
    }

    /// A long candidate is clipped rather than allowed to widen the box.
    #[test]
    fn candidates_are_clipped_to_the_box() {
        let long = vec!["/".to_string() + &"a".repeat(200)];
        for width in [8usize, 20, 40, 74] {
            for l in candidate_lines(&long, width) {
                assert!(l.chars().count() <= width.max(8), "{width}: {l}");
            }
        }
    }

    #[test]
    fn a_prompt_only_advertises_the_keys_it_has() {
        let plain = PromptOpts::default();
        let line = keybinding_line(&plain, false);
        assert!(line.contains("Enter accepts") && line.contains("Esc cancels"));
        assert!(!line.contains("Tab"), "{line}");
        assert!(!line.contains("Ctrl-A"), "{line}");

        let f = |_: &str, _: bool| Completion::default();
        let full = PromptOpts {
            complete: Some(&f),
            toggle: Some("system accounts"),
            ..Default::default()
        };
        let line = keybinding_line(&full, false);
        assert!(line.contains("Tab completes"), "{line}");
        assert!(line.contains("Ctrl-A shows system accounts"), "{line}");
        assert!(
            keybinding_line(&full, true).contains("Ctrl-A hides"),
            "{line}"
        );
    }

    /// An absent note or candidate list costs no vertical space at all.
    #[test]
    fn empty_blocks_take_no_rows() {
        assert_eq!(block_of(&[]), "");
        assert_eq!(block_of(&["a".to_string()]), "\na\n");
    }

    /// The box has to grow for the candidate list too, or completing a
    /// directory pushes the keybindings out of sight -- the same failure the
    /// option help caused.
    #[test]
    fn a_prompt_box_grows_to_fit_its_candidates() {
        let many: Vec<String> = (0..30).map(|i| format!("/usr/bin/thing{i}")).collect();
        let body = format!(
            "help line\n\n  /usr/bin/th_\n{}\n\n  Enter accepts, Esc cancels",
            block_of(&candidate_lines(&many, 70))
        );
        let height = wrapped_height(&body, 70) + 2;
        assert!(height >= body.lines().count() + 2, "{height}");
        assert!(height <= 24, "{height} rows will not fit an 80x24 screen");
    }
}
