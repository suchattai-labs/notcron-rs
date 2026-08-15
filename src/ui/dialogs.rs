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
            let area = popup_rect(f.area(), 74, 9);
            f.render_widget(Clear, area);
            // Keep the tail of a long value visible while typing.
            let inner_w = area.width.saturating_sub(4) as usize;
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
