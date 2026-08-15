//! Terminal session: raw mode + alternate screen + mouse capture, restored on
//! drop and on panic so a crash never leaves the shell unusable.

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

pub type Backend = CrosstermBackend<Stdout>;

pub struct Term {
    pub terminal: Terminal<Backend>,
    last_click: Option<(u16, u16, Instant)>,
}

fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

impl Term {
    pub fn new() -> io::Result<Term> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            default_hook(info);
        }));

        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Term {
            terminal,
            last_click: None,
        })
    }

    /// Next input event, or `None` if `timeout` elapsed without one.
    ///
    /// This is what lets a prompt debounce expensive work: wait a beat for
    /// the next keystroke and, only when it does not arrive, spend a
    /// subprocess on a preview. Events the UI ignores (mouse motion, key
    /// releases) do not reset the clock -- the deadline is absolute, so a
    /// terminal streaming motion reports cannot hold the preview off forever.
    /// A dead terminal reads as a timeout here rather than as EOF; quitting
    /// stays [`Term::next_key`]'s job.
    pub fn poll_key(&mut self, timeout: Duration) -> Option<Key> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            match event::poll(left) {
                Ok(true) => {}
                _ => return None,
            }
            match event::read() {
                Ok(ev) => {
                    if let Some(k) = self.classify(ev) {
                        return Some(k);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Next input event. Key releases and mouse moves are filtered; left
    /// clicks become `Click`/`DoubleClick` and the wheel becomes `Scroll`.
    /// `None` on EOF or a lost terminal -- callers treat that as quit.
    pub fn next_key(&mut self) -> Option<Key> {
        loop {
            match event::read() {
                Ok(ev) => {
                    if let Some(k) = self.classify(ev) {
                        return Some(k);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Map a crossterm event onto a [`Key`], or `None` for one the UI ignores.
    fn classify(&mut self, ev: Event) -> Option<Key> {
        match ev {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) if kind != KeyEventKind::Release => Some(Key::Press(code, modifiers)),
            Event::Mouse(MouseEvent {
                kind, column, row, ..
            }) => match kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let now = Instant::now();
                    let double = self.last_click.is_some_and(|(x, y, t)| {
                        x == column
                            && y == row
                            && now.duration_since(t) < Duration::from_millis(400)
                    });
                    self.last_click = if double {
                        None
                    } else {
                        Some((column, row, now))
                    };
                    Some(if double {
                        Key::DoubleClick(column, row)
                    } else {
                        Key::Click(column, row)
                    })
                }
                MouseEventKind::ScrollUp => Some(Key::Scroll(-1)),
                MouseEventKind::ScrollDown => Some(Key::Scroll(1)),
                _ => None,
            },
            Event::Resize(..) => Some(Key::Resize),
            _ => None,
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Press(KeyCode, KeyModifiers),
    Click(u16, u16),
    DoubleClick(u16, u16),
    Scroll(i8),
    Resize,
}

impl Key {
    pub fn is_char(&self, c: char) -> bool {
        matches!(self, Key::Press(KeyCode::Char(x), m)
                 if *x == c && !m.contains(KeyModifiers::CONTROL))
    }

    pub fn is_ctrl(&self, c: char) -> bool {
        matches!(self, Key::Press(KeyCode::Char(x), m)
                 if x.eq_ignore_ascii_case(&c) && m.contains(KeyModifiers::CONTROL))
    }

    pub fn code(&self) -> Option<KeyCode> {
        match self {
            Key::Press(c, _) => Some(*c),
            _ => None,
        }
    }
}

/// A centered popup rect that never exceeds the frame. Sized down rather than
/// clipped, so tiny terminals still render something usable.
pub fn popup_rect(area: Rect, w: u16, h: u16) -> Rect {
    // The final `.min(area.*)` matters: a 0x0 frame must yield a 0x0 rect,
    // because ratatui panics on a rect that falls outside the buffer.
    let w = w.min(area.width.saturating_sub(2)).max(1).min(area.width);
    let h = h.min(area.height.saturating_sub(2)).max(1).min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_never_exceeds_a_tiny_frame() {
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        };
        let p = popup_rect(tiny, 60, 20);
        assert!(p.width <= tiny.width && p.height <= tiny.height);
        assert!(p.width >= 1 && p.height >= 1);
        // Every smaller frame stays in bounds too, including degenerate ones.
        for w in 0..8u16 {
            for h in 0..8u16 {
                let a = Rect {
                    x: 3,
                    y: 4,
                    width: w,
                    height: h,
                };
                let p = popup_rect(a, 60, 20);
                assert!(p.x + p.width <= a.x + a.width, "{w}x{h}");
                assert!(p.y + p.height <= a.y + a.height, "{w}x{h}");
            }
        }
        assert!(p.x + p.width <= tiny.x + tiny.width);
        assert!(p.y + p.height <= tiny.y + tiny.height);
    }

    #[test]
    fn popup_is_centered_in_a_normal_frame() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let p = popup_rect(area, 60, 20);
        assert_eq!((p.width, p.height), (60, 20));
        assert_eq!((p.x, p.y), (20, 10));
    }

    #[test]
    fn zero_sized_frames_do_not_panic() {
        let z = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        let p = popup_rect(z, 10, 10);
        assert_eq!((p.width, p.height), (0, 0));
    }

    #[test]
    fn one_by_one_frames_stay_in_bounds() {
        let one = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let p = popup_rect(one, 60, 20);
        assert_eq!((p.x, p.y, p.width, p.height), (0, 0, 1, 1));
    }
}
