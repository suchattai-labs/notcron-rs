//! The manual-entry escape hatch: a small multi-line text editor.
//!
//! Used for free-form unit directives (whole `[Section]` blocks are fine) and
//! for `Environment=` lists, one `KEY=VALUE` per line. The buffer logic is
//! separated from the event loop so it can be tested without a terminal.

use super::dialogs::{dialog_block, draw_over, Background};
use super::term::{popup_rect, Key, Term};
use crossterm::event::KeyCode;
use ratatui::widgets::{Clear, Paragraph};

/// A line-oriented text buffer with a single cursor. Column positions are
/// character indices, not bytes, so non-ASCII text edits correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
}

impl Buffer {
    pub fn new(text: &str) -> Buffer {
        let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Buffer {
            lines,
            row: 0,
            col: 0,
        }
    }

    /// The buffer as text, with trailing blank lines trimmed. An all-blank
    /// buffer becomes the empty string, which is how "no manual directives"
    /// is represented in the model.
    pub fn text(&self) -> String {
        let mut lines = self.lines.clone();
        while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            lines.pop();
        }
        if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n")
        }
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines.get(row).map(|l| l.chars().count()).unwrap_or(0)
    }

    fn clamp(&mut self) {
        if self.row >= self.lines.len() {
            self.row = self.lines.len().saturating_sub(1);
        }
        self.col = self.col.min(self.line_len(self.row));
    }

    /// Byte offset of the cursor column in the current line.
    fn byte_at(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(self.lines[row].len())
    }

    pub fn insert(&mut self, c: char) {
        self.clamp();
        let b = self.byte_at(self.row, self.col);
        self.lines[self.row].insert(b, c);
        self.col += 1;
    }

    pub fn newline(&mut self) {
        self.clamp();
        let b = self.byte_at(self.row, self.col);
        let rest = self.lines[self.row].split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        self.clamp();
        if self.col > 0 {
            let b = self.byte_at(self.row, self.col - 1);
            self.lines[self.row].remove(b);
            self.col -= 1;
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len(self.row);
            self.lines[self.row].push_str(&cur);
        }
    }

    pub fn delete(&mut self) {
        self.clamp();
        if self.col < self.line_len(self.row) {
            let b = self.byte_at(self.row, self.col);
            self.lines[self.row].remove(b);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    /// Delete the current line, the way Ctrl-K works here.
    pub fn kill_line(&mut self) {
        self.clamp();
        self.lines.remove(self.row);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        if self.row >= self.lines.len() {
            self.row = self.lines.len() - 1;
        }
        self.col = 0;
    }

    pub fn move_up(&mut self) {
        self.row = self.row.saturating_sub(1);
        self.clamp();
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
        }
        self.clamp();
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len(self.row);
        }
    }

    pub fn move_right(&mut self) {
        if self.col < self.line_len(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.line_len(self.row);
    }
}

/// Run the editor. Returns the new text on Ctrl-S, or `None` on Escape.
pub fn edit(
    term: &mut Term,
    bg: Background,
    title: &str,
    help: &str,
    initial: &str,
) -> Option<String> {
    let mut buf = Buffer::new(initial);
    let mut top = 0usize;
    loop {
        let (t, h) = (title.to_string(), help.to_string());
        let lines = buf.lines.clone();
        let (row, col) = (buf.row, buf.col);
        let mut visible = 1usize;
        draw_over(term, bg, &mut |f| {
            let area = popup_rect(f.area(), 80, 22);
            f.render_widget(Clear, area);
            let inner_h = area.height.saturating_sub(4) as usize;
            visible = inner_h.max(1);
            // Scroll just enough to keep the cursor line on screen.
            let t0 = if row < top {
                row
            } else if row >= top + visible {
                row + 1 - visible
            } else {
                top
            };
            let mut text = format!("{h}\n\n");
            for (i, l) in lines.iter().enumerate().skip(t0).take(visible) {
                if i == row {
                    // A visible caret, since the popup owns no real cursor.
                    let mut chars: Vec<char> = l.chars().collect();
                    let at = col.min(chars.len());
                    chars.insert(at, '\u{2502}');
                    text.push_str(&chars.into_iter().collect::<String>());
                } else {
                    text.push_str(l);
                }
                text.push('\n');
            }
            f.render_widget(
                Paragraph::new(text).block(dialog_block(&format!(
                    "{t}  --  Ctrl-S saves, Esc cancels, Ctrl-K kills a line"
                ))),
                area,
            );
        });
        // Recompute the scroll offset with the height the frame actually had.
        if buf.row < top {
            top = buf.row;
        } else if buf.row >= top + visible {
            top = buf.row + 1 - visible;
        }

        let k = term.next_key()?;
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => {
                if d < 0 {
                    buf.move_up();
                } else {
                    buf.move_down();
                }
            }
            _ if k.is_ctrl('s') => return Some(buf.text()),
            _ if k.is_ctrl('k') => buf.kill_line(),
            _ => match k.code() {
                Some(KeyCode::Esc) => return None,
                Some(KeyCode::Enter) => buf.newline(),
                Some(KeyCode::Backspace) => buf.backspace(),
                Some(KeyCode::Delete) => buf.delete(),
                Some(KeyCode::Up) => buf.move_up(),
                Some(KeyCode::Down) => buf.move_down(),
                Some(KeyCode::Left) => buf.move_left(),
                Some(KeyCode::Right) => buf.move_right(),
                Some(KeyCode::Home) => buf.home(),
                Some(KeyCode::End) => buf.end(),
                Some(KeyCode::Tab) => {
                    for _ in 0..4 {
                        buf.insert(' ');
                    }
                }
                Some(KeyCode::Char(c)) => buf.insert(c),
                _ => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_starts_with_one_blank_line() {
        let b = Buffer::new("");
        assert_eq!(b.lines, [""]);
        assert_eq!(b.text(), "");
    }

    #[test]
    fn typing_and_newlines_build_text() {
        let mut b = Buffer::new("");
        for c in "[Service]".chars() {
            b.insert(c);
        }
        b.newline();
        for c in "Nice=19".chars() {
            b.insert(c);
        }
        assert_eq!(b.text(), "[Service]\nNice=19");
        assert_eq!((b.row, b.col), (1, 7));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut b = Buffer::new("ab\ncd");
        b.row = 1;
        b.col = 0;
        b.backspace();
        assert_eq!(b.lines, ["abcd"]);
        assert_eq!((b.row, b.col), (0, 2));
    }

    #[test]
    fn delete_at_end_of_line_joins_the_next() {
        let mut b = Buffer::new("ab\ncd");
        b.row = 0;
        b.col = 2;
        b.delete();
        assert_eq!(b.lines, ["abcd"]);
    }

    #[test]
    fn kill_line_never_empties_the_buffer() {
        let mut b = Buffer::new("only");
        b.kill_line();
        assert_eq!(b.lines, [""]);
        assert_eq!(b.row, 0);
    }

    #[test]
    fn trailing_blank_lines_are_trimmed_on_save() {
        let mut b = Buffer::new("Nice=19");
        b.end();
        b.newline();
        b.newline();
        assert_eq!(b.text(), "Nice=19");
    }

    #[test]
    fn editing_is_character_oriented_not_byte_oriented() {
        let mut b = Buffer::new("caf\u{e9}");
        b.end();
        assert_eq!(b.col, 4);
        b.backspace();
        assert_eq!(b.text(), "caf");
        b.insert('\u{e9}');
        b.insert('s');
        assert_eq!(b.text(), "caf\u{e9}s");
    }

    #[test]
    fn cursor_motion_stays_in_bounds() {
        let mut b = Buffer::new("ab\ncdef");
        b.move_up();
        assert_eq!((b.row, b.col), (0, 0));
        b.move_left();
        assert_eq!((b.row, b.col), (0, 0));
        b.end();
        b.move_right(); // wraps to the next line
        assert_eq!((b.row, b.col), (1, 0));
        b.end();
        b.move_right(); // already at the very end
        assert_eq!((b.row, b.col), (1, 4));
        b.move_down();
        assert_eq!(b.row, 1);
    }

    #[test]
    fn a_long_line_clamps_the_column_when_moving_up() {
        let mut b = Buffer::new("ab\nlonger line");
        b.row = 1;
        b.end();
        b.move_up();
        assert_eq!((b.row, b.col), (0, 2));
    }
}
