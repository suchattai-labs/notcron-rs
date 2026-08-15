//! Modal dialogs: message box, confirm, single-line prompt, list picker and
//! a scrollable pager. Each runs a small event loop and repaints the
//! caller-supplied background underneath itself.

use super::term::{popup_rect, Key, Term};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

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
    let mut value = initial.to_string();
    let mut err = String::new();
    loop {
        let (t, h, v, e) = (
            title.to_string(),
            help.to_string(),
            value.clone(),
            err.clone(),
        );
        draw_over(term, bg, &mut |f| {
            // The width is fixed, so the wrapped height can be measured
            // before the rect is chosen. It has to be: a multi-line help --
            // a field summary plus its examples -- used to push the error
            // and the keybindings out of a box that was always nine rows,
            // which meant a rejected value looked like a dead key.
            let width = 74u16.min(f.area().width.saturating_sub(2)).max(1);
            let inner_w = width.saturating_sub(4) as usize;
            let shown: String = if v.chars().count() > inner_w {
                v.chars().skip(v.chars().count() - inner_w).collect()
            } else {
                v.clone()
            };
            let body = format!(
                "{h}\n\n  {shown}_\n\n{}\n  Enter accepts, Esc cancels",
                if e.is_empty() {
                    String::new()
                } else {
                    format!("  ! {e}")
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
        let k = term.next_key()?;
        match k {
            Key::Resize | Key::Scroll(_) | Key::Click(..) | Key::DoubleClick(..) => continue,
            _ => match k.code() {
                Some(KeyCode::Esc) => return None,
                Some(KeyCode::Enter) => match validate(&value) {
                    Ok(()) => return Some(value),
                    Err(e) => err = e,
                },
                Some(KeyCode::Backspace) => {
                    value.pop();
                    err.clear();
                }
                Some(KeyCode::Char(c)) if !k.is_ctrl(c) => {
                    value.push(c);
                    err.clear();
                }
                Some(KeyCode::Char('u')) if k.is_ctrl('u') => {
                    value.clear();
                    err.clear();
                }
                _ => {}
            },
        }
    }
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
}
