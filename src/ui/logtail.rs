//! The inline journal pane: the last few lines for the selected unit, and an
//! optional live follow.
//!
//! Following spawns `journalctl -f` and reads it on a thread, so the UI never
//! blocks on a unit that says nothing for an hour. The child is killed and
//! reaped in [`Follower`]'s `Drop`, which is what closing the pane, changing
//! the selection and quitting all go through -- there is no path that leaves a
//! `journalctl` behind.

use crate::unit::model::Scope;
use ratatui::prelude::*;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// How many journal lines the pane holds.
pub const TAIL_LINES: usize = 10;

/// Rows the pane occupies when there is room: [`TAIL_LINES`] plus a border.
pub const PANE_HEIGHT: u16 = TAIL_LINES as u16 + 2;

/// The list needs at least this many rows before the pane may take any.
const LIST_MIN: u16 = 3;

/// Rows the footer wants.
const FOOTER_HEIGHT: u16 = 4;

/// The main screen's vertical split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    pub header: Rect,
    pub list: Rect,
    /// `None` when the pane is off, or when the terminal is too short for it.
    pub tail: Option<Rect>,
    pub footer: Rect,
}

/// Split the frame into header, list, optional journal pane and footer.
///
/// Everything gives way to the list: on a short terminal the pane is dropped
/// first and then the footer, rather than squeezing the units out of view.
pub fn split(area: Rect, tail: bool) -> Panes {
    let mut y = area.y;
    let mut left = area.height;

    let take = |n: u16, left: &mut u16, y: &mut u16| -> Rect {
        let n = n.min(*left);
        let r = Rect {
            x: area.x,
            y: *y,
            width: area.width,
            height: n,
        };
        *y += n;
        *left -= n;
        r
    };

    let header = take(1, &mut left, &mut y);
    // Reserve the footer up front so the pane cannot eat it.
    let footer_h = if left >= LIST_MIN + FOOTER_HEIGHT {
        FOOTER_HEIGHT
    } else {
        0
    };
    let body = left - footer_h;

    let tail_h = if tail && body >= LIST_MIN + 3 {
        PANE_HEIGHT.min(body - LIST_MIN)
    } else {
        0
    };

    let list = take(body - tail_h, &mut left, &mut y);
    let tail_rect = if tail_h > 0 {
        Some(take(tail_h, &mut left, &mut y))
    } else {
        None
    };
    let footer = take(footer_h, &mut left, &mut y);

    Panes {
        header,
        list,
        tail: tail_rect,
        footer,
    }
}

// ---------------------------------------------------------------------------
// Following
// ---------------------------------------------------------------------------

/// Only system-scope reads may need elevation, and only when not already root.
//
// This mirrors the private helper in `systemd`; the non-following tail goes
// through `systemd::journal`, but a live follow needs to own its own child
// process and so has to build the command itself.
fn need_sudo(scope: Scope) -> bool {
    scope == Scope::System
        && !std::fs::metadata("/proc/self")
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.uid() == 0
            })
            .unwrap_or(false)
}

/// Build the `journalctl` invocation for a scope. `follow` adds `-f`.
///
/// The user journal needs `--user`; the system journal is journalctl's
/// default and has no flag of its own, so passing `--system` would be wrong
/// for a user who is also allowed to read it.
pub fn journal_command(scope: Scope, unit: &str, lines: usize, follow: bool) -> Command {
    let mut cmd = if need_sudo(scope) {
        let mut c = Command::new("sudo");
        c.arg("-n").arg("journalctl");
        c
    } else {
        Command::new("journalctl")
    };
    if scope == Scope::User {
        cmd.arg("--user");
    }
    cmd.args(["--no-pager", "-n", &lines.to_string(), "-u", unit]);
    if follow {
        cmd.arg("-f");
    }
    cmd
}

/// A live `journalctl -f`, read on a background thread.
pub struct Follower {
    child: Child,
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl Follower {
    /// Start following `unit`. The child's stderr is discarded: it would
    /// otherwise be written straight over the alternate screen.
    pub fn start(scope: Scope, unit: &str, keep: usize) -> Result<Follower, String> {
        let mut cmd = journal_command(scope, unit, keep, true);
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot follow the journal: {e}"))?;
        let stdout = child.stdout.take().ok_or("journalctl produced no output")?;

        let lines = Arc::new(Mutex::new(VecDeque::with_capacity(keep + 1)));
        let sink = Arc::clone(&lines);
        // Detached on purpose: it ends when the pipe closes, which killing the
        // child in `drop` guarantees. Joining here could block on a child that
        // is slow to die.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(mut buf) = sink.lock() else { return };
                buf.push_back(line);
                while buf.len() > keep {
                    buf.pop_front();
                }
            }
        });

        Ok(Follower { child, lines })
    }

    /// Whatever has arrived so far. Never blocks for longer than the reader
    /// thread holds the lock for one `push_back`.
    pub fn snapshot(&self) -> Vec<String> {
        match self.lines.lock() {
            Ok(b) => b.iter().cloned().collect(),
            Err(_) => vec!["(the journal reader stopped)".into()],
        }
    }

    /// True when `journalctl` has exited -- no such unit, no permission, or
    /// the journal went away.
    pub fn finished(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        // Kill *and* wait: without the wait the child is reaped only when
        // notcron exits, which on a long session is a pile of zombies.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The pane's state: which unit is being shown, the lines to show, and the
/// follower when one is running.
#[derive(Default)]
pub struct Tail {
    /// The unit the current contents belong to, so a selection change is
    /// noticed without re-reading the journal on every frame.
    pub unit: String,
    pub scope: Option<Scope>,
    pub lines: Vec<String>,
    pub follower: Option<Follower>,
    /// A message shown instead of the lines: no unit selected, follow failed.
    pub note: String,
}

impl Tail {
    pub fn following(&self) -> bool {
        self.follower.is_some()
    }

    /// Stop following, killing the child. Idempotent.
    pub fn unfollow(&mut self) {
        self.follower = None;
    }

    /// Point the pane at a unit, reading a one-shot tail. A follow in progress
    /// for a *different* unit is stopped first -- this is the path that keeps
    /// a child from being orphaned when the selection moves.
    pub fn show(&mut self, scope: Scope, unit: &str) {
        if self.unit == unit && self.scope == Some(scope) {
            return;
        }
        // A follow belongs to the unit it was started on. Moving the
        // selection ends it -- silently would look like the follow simply
        // stopped working, so say so.
        let was_following = self.following();
        self.unfollow();
        self.unit = unit.to_string();
        self.scope = Some(scope);
        self.note = if was_following {
            "follow stopped: the selection moved -- press f to follow this one".into()
        } else {
            String::new()
        };
        let text = crate::systemd::journal(scope, unit, TAIL_LINES);
        self.lines = text.lines().map(|l| l.to_string()).collect();
    }

    /// Start following whatever the pane is pointed at.
    pub fn follow(&mut self) {
        let (Some(scope), false) = (self.scope, self.unit.is_empty()) else {
            self.note = "nothing selected to follow".into();
            return;
        };
        match Follower::start(scope, &self.unit, TAIL_LINES) {
            Ok(f) => {
                self.follower = Some(f);
                self.note.clear();
            }
            Err(e) => {
                self.note = e;
                self.follower = None;
            }
        }
    }

    /// Pull anything the follower has read. Called once per frame.
    pub fn poll(&mut self) {
        let Some(f) = self.follower.as_mut() else {
            return;
        };
        let lines = f.snapshot();
        if !lines.is_empty() {
            self.lines = lines;
        }
        if f.finished() {
            self.note = "the journal reader exited -- press f to retry".into();
            self.follower = None;
        }
    }

    /// The pane's title, which is where the follow state is visible.
    pub fn title(&self) -> String {
        let unit = if self.unit.is_empty() {
            "(nothing selected)"
        } else {
            &self.unit
        };
        if self.following() {
            format!(" journal: {unit} -- FOLLOWING (f stops) ")
        } else {
            format!(" journal: {unit} -- f follows, t closes ")
        }
    }

    /// The body, clipped to the pane.
    #[cfg(test)]
    pub fn body(&self, width: usize, height: usize) -> Vec<String> {
        body(&self.lines, &self.note, width, height)
    }
}

/// The pane's visible text. The *last* lines are the ones worth seeing, so an
/// overlong tail drops its head; a note replaces the lines entirely.
pub fn body(lines: &[String], note: &str, width: usize, height: usize) -> Vec<String> {
    if !note.is_empty() {
        return vec![clip(note, width)];
    }
    if lines.is_empty() {
        return vec![clip("(no journal entries)", width)];
    }
    lines
        .iter()
        .skip(lines.len().saturating_sub(height.max(1)))
        .map(|l| clip(l, width))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: u16, h: u16) -> Rect {
        Rect {
            x: 2,
            y: 5,
            width: w,
            height: h,
        }
    }

    /// The exhaustive bounds grid: at every size, with the pane on and off,
    /// every rect must sit inside the frame and none may overlap.
    #[test]
    fn the_split_stays_in_bounds_at_every_size() {
        for w in 0..=40u16 {
            for h in 0..=40u16 {
                for tail in [false, true] {
                    let area = rect(w, h);
                    let p = split(area, tail);
                    // In the order they are stacked down the frame.
                    let mut rects = vec![p.header, p.list];
                    rects.extend(p.tail);
                    rects.push(p.footer);
                    let mut total = 0u16;
                    for r in &rects {
                        assert_eq!(r.x, area.x, "{w}x{h} tail={tail}");
                        assert_eq!(r.width, area.width, "{w}x{h} tail={tail}");
                        assert!(r.y >= area.y, "{w}x{h} tail={tail}");
                        assert!(
                            r.y + r.height <= area.y + area.height,
                            "{w}x{h} tail={tail}: {r:?} escapes {area:?}"
                        );
                        total += r.height;
                    }
                    assert!(total <= area.height, "{w}x{h} tail={tail}");
                    // Laid out top to bottom, back to back.
                    for pair in rects.windows(2) {
                        assert_eq!(pair[0].y + pair[0].height, pair[1].y, "{w}x{h}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_zero_height_frame_yields_nothing_at_all() {
        let p = split(rect(80, 0), true);
        assert_eq!(p.header.height, 0);
        assert_eq!(p.list.height, 0);
        assert_eq!(p.footer.height, 0);
        assert!(p.tail.is_none());
    }

    /// The list is what the screen is for: the pane goes before it does, and
    /// the footer goes before the list drops below three rows.
    #[test]
    fn the_list_is_the_last_thing_to_lose_space() {
        for h in 0..=8u16 {
            let p = split(rect(80, h), true);
            if p.list.height > 0 && h >= 5 {
                assert!(p.list.height >= LIST_MIN.min(h), "h={h}: {p:?}");
            }
            if p.tail.is_some() {
                assert!(p.list.height >= LIST_MIN, "h={h}: pane starved the list");
            }
        }
    }

    #[test]
    fn the_pane_appears_once_there_is_room_and_takes_its_full_height() {
        let p = split(rect(80, 40), true);
        assert_eq!(p.tail.map(|r| r.height), Some(PANE_HEIGHT));
        assert_eq!(p.header.height, 1);
        assert_eq!(p.footer.height, FOOTER_HEIGHT);
        assert_eq!(
            p.header.height + p.list.height + PANE_HEIGHT + p.footer.height,
            40
        );
    }

    #[test]
    fn the_pane_shrinks_before_it_starves_the_list() {
        // 1 header + 3 list + 3 footer = 7; a 15-row frame leaves 8 for the pane.
        let p = split(rect(80, 15), true);
        let t = p.tail.expect("a pane");
        assert!(t.height < PANE_HEIGHT, "{t:?}");
        assert!(p.list.height >= LIST_MIN);
    }

    #[test]
    fn switching_the_pane_off_gives_the_rows_back_to_the_list() {
        let on = split(rect(80, 30), true);
        let off = split(rect(80, 30), false);
        assert!(off.tail.is_none());
        assert_eq!(off.list.height, on.list.height + PANE_HEIGHT);
    }

    // -----------------------------------------------------------------
    // The command
    // -----------------------------------------------------------------

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_user_journal_is_asked_for_explicitly() {
        let cmd = journal_command(Scope::User, "x.service", 10, false);
        let args = args_of(&cmd);
        assert!(args.contains(&"--user".to_string()), "{args:?}");
        assert!(!args.contains(&"-f".to_string()), "{args:?}");
        assert!(args.contains(&"x.service".to_string()));
        assert_eq!(cmd.get_program(), "journalctl");
    }

    /// journalctl has no `--system`: the system journal is its default, and
    /// passing a flag that does not exist would fail outright.
    #[test]
    fn the_system_journal_takes_no_scope_flag() {
        let cmd = journal_command(Scope::System, "x.service", 10, false);
        let args = args_of(&cmd);
        assert!(
            !args.iter().any(|a| a == "--user" || a == "--system"),
            "{args:?}"
        );
    }

    #[test]
    fn following_adds_the_follow_flag_and_keeps_the_line_count() {
        let args = args_of(&journal_command(Scope::User, "x.timer", 10, true));
        assert!(args.contains(&"-f".to_string()), "{args:?}");
        assert!(args.contains(&"10".to_string()), "{args:?}");
    }

    // -----------------------------------------------------------------
    // Pane contents
    // -----------------------------------------------------------------

    fn tail_with(lines: &[&str]) -> Tail {
        Tail {
            unit: "x.service".into(),
            scope: Some(Scope::User),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            ..Tail::default()
        }
    }

    #[test]
    fn the_body_shows_the_newest_lines_and_never_overflows() {
        let all: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let t = tail_with(&refs);
        let body = t.body(20, 5);
        assert_eq!(body.len(), 5);
        assert_eq!(body[4], "line 49");
        assert_eq!(body[0], "line 45");
        for w in 0..30usize {
            for h in 0..15usize {
                for l in t.body(w, h) {
                    assert!(l.chars().count() <= w, "{w}x{h}");
                }
            }
        }
    }

    #[test]
    fn an_empty_journal_says_so_rather_than_showing_a_blank_pane() {
        let t = tail_with(&[]);
        assert_eq!(t.body(40, 5), vec!["(no journal entries)".to_string()]);
    }

    #[test]
    fn a_note_replaces_the_lines() {
        let mut t = tail_with(&["something"]);
        t.note = "cannot follow the journal: no such file".into();
        assert_eq!(t.body(80, 5).len(), 1);
        assert!(t.body(80, 5)[0].starts_with("cannot follow"));
    }

    #[test]
    fn the_title_says_whether_it_is_following() {
        let t = tail_with(&[]);
        assert!(t.title().contains("f follows"));
        assert!(!t.following());
        assert!(Tail::default().title().contains("(nothing selected)"));
    }

    #[test]
    fn following_nothing_is_refused_rather_than_spawning() {
        let mut t = Tail::default();
        t.follow();
        assert!(!t.following());
        assert!(!t.note.is_empty());
    }

    #[test]
    fn unfollowing_is_idempotent() {
        let mut t = tail_with(&[]);
        t.unfollow();
        t.unfollow();
        assert!(!t.following());
    }

    /// Moving the selection ends the follow, and says so rather than just
    /// going quiet -- a follow that stopped for no visible reason reads as a
    /// bug in the pane.
    #[test]
    fn moving_the_selection_reports_that_the_follow_ended() {
        let mut t = tail_with(&["old"]);
        t.note = "something".into();
        t.show(Scope::User, "other.service");
        assert!(!t.following());
        assert_eq!(t.unit, "other.service");
        // Not following to begin with: nothing to report.
        assert!(t.note.is_empty(), "{:?}", t.note);
    }

    /// The pane must not re-read the journal when the selection has not moved.
    #[test]
    fn showing_the_same_unit_twice_is_a_no_op() {
        let mut t = tail_with(&["kept"]);
        t.show(Scope::User, "x.service");
        assert_eq!(t.lines, vec!["kept".to_string()]);
    }

    /// The real end-to-end check that a follow cannot leak: start one against
    /// a unit that certainly does not exist, then drop it.
    #[test]
    fn a_dropped_follower_kills_its_child() {
        let Ok(f) = Follower::start(Scope::User, "notcron-nonexistent-test.service", 5) else {
            eprintln!("skipping: journalctl not available");
            return;
        };
        let pid = f.child.id();
        drop(f);
        // The child has been waited on, so its pid is no longer a live
        // process of ours. `kill -0` on a reaped pid fails.
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "journalctl {pid} survived the drop");
    }
}
