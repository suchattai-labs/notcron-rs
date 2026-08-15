//! The filesystem picker behind every path field: a modal, single-directory
//! browser. `b` opens it from the focused builder row, Enter descends or
//! selects, Esc cancels and leaves the field alone.
//!
//! Listing, sorting, start-directory resolution and row composition are pure
//! functions so they can be unit-tested without a terminal; `browse` is the
//! only part that touches one.

use super::dialogs::{dialog_block, draw_over, Background};
use super::term::{popup_rect, Key, Term};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use std::path::{Path, PathBuf};

/// What the calling field wants out of the picker. Set explicitly per call
/// site rather than guessed from the value, because the same-looking string
/// means different things in different fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Confirms on a directory: `WorkingDirectory=`, a mount's `Where=`.
    /// Files are not listed, so there is nothing to descend into by mistake.
    Directory,
    /// Confirms on a file: `ExecStart=` and friends, a mount's `What=` when
    /// it names a block device.
    File,
    /// Either will do; the current directory can also be confirmed.
    Any,
}

impl Mode {
    /// Whether the directory being browsed is itself a valid answer.
    fn takes_dir(self) -> bool {
        matches!(self, Mode::Directory | Mode::Any)
    }

    /// Whether files should be listed at all.
    fn takes_file(self) -> bool {
        matches!(self, Mode::File | Mode::Any)
    }
}

/// One real directory entry. Symlinks are resolved for the `is_dir` flag so
/// following one behaves the way it looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// A line in the picker: the two synthetic actions, or a real entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Confirms the directory currently being browsed.
    UseCurrent,
    /// Ascends. Absent at `/`.
    Parent,
    Entry(Entry),
}

impl Row {
    /// The rendered text. Directories carry a trailing `/`.
    pub fn label(&self) -> String {
        match self {
            Row::UseCurrent => "[ use this directory ]".into(),
            Row::Parent => "../".into(),
            Row::Entry(e) if e.is_dir => format!("{}/", e.name),
            Row::Entry(e) => e.name.clone(),
        }
    }

    fn is_dirlike(&self) -> bool {
        !matches!(self, Row::Entry(e) if !e.is_dir)
    }
}

/// Read `dir`, dropping dotfiles unless `hidden`. Directories sort before
/// files, each group alphabetically. Entries that cannot be stat'ed are
/// skipped rather than failing the whole listing.
pub fn list_dir(dir: &Path, hidden: bool) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for dent in std::fs::read_dir(dir)? {
        let Ok(dent) = dent else { continue };
        let name = dent.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = match dent.file_type() {
            Ok(ft) if ft.is_symlink() => std::fs::metadata(dent.path())
                .map(|m| m.is_dir())
                .unwrap_or(false),
            Ok(ft) => ft.is_dir(),
            Err(_) => continue,
        };
        out.push(Entry { name, is_dir });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// Compose the visible rows for `dir` in `mode`.
pub fn rows(mode: Mode, dir: &Path, entries: Vec<Entry>) -> Vec<Row> {
    let mut out = Vec::new();
    if mode.takes_dir() {
        out.push(Row::UseCurrent);
    }
    if dir.parent().is_some() {
        out.push(Row::Parent);
    }
    out.extend(
        entries
            .into_iter()
            .filter(|e| e.is_dir || mode.takes_file())
            .map(Row::Entry),
    );
    out
}

/// The parent of `dir`, clamped at `/`: ascending from the root stays at the
/// root instead of escaping it or panicking.
pub fn parent_of(dir: &Path) -> PathBuf {
    dir.parent()
        .map_or_else(|| dir.to_path_buf(), Path::to_path_buf)
}

/// `$HOME` if it is a readable directory, else `/`.
pub fn home_or_root() -> PathBuf {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) if h.is_dir() => h,
        _ => PathBuf::from("/"),
    }
}

/// Where to open the picker for a field currently holding `value`.
///
/// An absolute path that is a directory is used as-is, one that names a file
/// yields its parent, and one that does not exist walks up to its first
/// existing ancestor. Anything else -- empty, relative, a bare command name
/// -- falls back to `$HOME`, then `/`.
pub fn start_dir(value: &str) -> PathBuf {
    let value = value.trim();
    if !value.starts_with('/') {
        return home_or_root();
    }
    let mut cur = Path::new(value);
    loop {
        match std::fs::metadata(cur) {
            Ok(m) if m.is_dir() => return cur.to_path_buf(),
            Ok(_) => return parent_of(cur),
            Err(_) => match cur.parent() {
                Some(p) => cur = p,
                None => return home_or_root(),
            },
        }
    }
}

/// Split a command line into its program and the rest of its arguments, so a
/// picked binary can replace the program without losing what follows it.
pub fn split_command(cmd: &str) -> (String, String) {
    let cmd = cmd.trim();
    match cmd.split_once(char::is_whitespace) {
        Some((prog, rest)) => (prog.to_string(), rest.trim_start().to_string()),
        None => (cmd.to_string(), String::new()),
    }
}

/// Inverse of [`split_command`].
pub fn join_command(program: &str, args: &str) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {args}")
    }
}

/// The picker's popup: nearly the whole frame, but routed through
/// `popup_rect` so a degenerate frame yields a degenerate rect instead of the
/// out-of-bounds rect ratatui panics on.
pub fn picker_rect(area: Rect) -> Rect {
    popup_rect(
        area,
        area.width.saturating_sub(6).max(30),
        area.height.saturating_sub(2).max(10),
    )
}

/// Everything the picker paints, kept apart from the event loop so the
/// rendering can be exercised at any terminal size in a test.
pub struct State {
    /// The field being filled in, shown in the border so the picker never
    /// floats free of what it is for.
    pub field: String,
    pub mode: Mode,
    pub dir: PathBuf,
    pub rows: Vec<Row>,
    pub sel: usize,
    pub top: usize,
    pub hidden: bool,
    /// A read failure to show instead of the help line, e.g. a denied
    /// directory. Never fatal: the picker stays where it was.
    pub err: String,
}

impl State {
    fn hint(&self) -> String {
        let confirm = if self.mode.takes_dir() {
            "Enter descends/selects   Ctrl-S takes this dir"
        } else {
            "Enter descends/selects"
        };
        format!("{confirm}   h/\u{2190} up   H hidden   Esc cancels")
    }

    /// Keep the selection inside the row list and the viewport around it.
    fn clamp(&mut self, visible: usize) {
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
        let visible = visible.max(1);
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + visible {
            self.top = self.sel + 1 - visible;
        }
    }

    fn select_named(&mut self, name: &str) {
        if let Some(i) = self
            .rows
            .iter()
            .position(|r| matches!(r, Row::Entry(e) if e.name == name))
        {
            self.sel = i;
        }
    }
}

/// Paint one frame of the picker.
pub fn draw(f: &mut Frame, st: &mut State) {
    let area = picker_rect(f.area());
    f.render_widget(Clear, area);
    let title = format!(
        "{} \u{2014} {}{}",
        st.field,
        st.dir.display(),
        if st.hidden { "  (hidden shown)" } else { "" }
    );
    let block = dialog_block(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // One line is reserved at the bottom for the hint or the error.
    let visible = inner.height.saturating_sub(1) as usize;
    st.clamp(visible);
    let width = inner.width as usize;

    let mut lines: Vec<Line> = st
        .rows
        .iter()
        .enumerate()
        .skip(st.top)
        .take(visible)
        .map(|(i, r)| {
            let text: String = format!(" {} {}", if i == st.sel { ">" } else { " " }, r.label())
                .chars()
                .take(width)
                .collect();
            let style = if i == st.sel {
                Style::new().bold().reversed()
            } else if r.is_dirlike() {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new()
            };
            Line::from(Span::styled(text, style))
        })
        .collect();

    if st.rows.is_empty() && visible > 0 {
        lines.push(Line::from(Span::styled(
            " (empty)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    while lines.len() < visible {
        lines.push(Line::default());
    }
    if inner.height > 0 {
        let (text, style) = if st.err.is_empty() {
            (st.hint(), Style::new().fg(Color::DarkGray))
        } else {
            (format!("! {}", st.err), Style::new().fg(Color::Red))
        };
        lines.push(Line::from(Span::styled(
            text.chars().take(width).collect::<String>(),
            style,
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Load `dir` into rows, turning an I/O failure into a message to show.
fn load(dir: &Path, mode: Mode, hidden: bool) -> Result<Vec<Row>, String> {
    match list_dir(dir, hidden) {
        Ok(entries) => Ok(rows(mode, dir, entries)),
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

/// Run the picker over `initial`. Returns the chosen path, or `None` on
/// Escape or a lost terminal -- in which case the field is left untouched.
pub fn browse(
    term: &mut Term,
    bg: Background,
    field: &str,
    mode: Mode,
    initial: &str,
) -> Option<String> {
    let mut st = State {
        field: field.to_string(),
        mode,
        dir: start_dir(initial),
        rows: Vec::new(),
        sel: 0,
        top: 0,
        hidden: false,
        err: String::new(),
    };
    // An unreadable starting directory falls back rather than opening blank.
    match load(&st.dir, mode, st.hidden) {
        Ok(r) => st.rows = r,
        Err(e) => {
            st.err = e;
            st.dir = home_or_root();
            st.rows = load(&st.dir, mode, st.hidden).unwrap_or_default();
        }
    }

    loop {
        draw_over(term, bg, &mut |f| draw(f, &mut st));

        let k = term.next_key()?;
        let last = st.rows.len().saturating_sub(1);
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => st.sel = st.sel.saturating_add_signed(d as isize).min(last),
            _ if k.is_ctrl('s') && mode.takes_dir() => {
                return Some(st.dir.to_string_lossy().into_owned())
            }
            _ => match k.code() {
                Some(KeyCode::Esc) => return None,
                Some(KeyCode::Up) => st.sel = st.sel.saturating_sub(1),
                Some(KeyCode::Down) => st.sel = (st.sel + 1).min(last),
                Some(KeyCode::PageUp) => st.sel = st.sel.saturating_sub(10),
                Some(KeyCode::PageDown) => st.sel = (st.sel + 10).min(last),
                Some(KeyCode::Home) => st.sel = 0,
                Some(KeyCode::End) => st.sel = last,
                Some(KeyCode::Left) | Some(KeyCode::Backspace) => ascend(&mut st),
                Some(KeyCode::Enter) | Some(KeyCode::Right) => {
                    if let Some(chosen) = activate(&mut st) {
                        return Some(chosen);
                    }
                }
                _ if k.is_char('k') => st.sel = st.sel.saturating_sub(1),
                _ if k.is_char('j') => st.sel = (st.sel + 1).min(last),
                _ if k.is_char('h') => ascend(&mut st),
                _ if k.is_char('l') => {
                    if let Some(chosen) = activate(&mut st) {
                        return Some(chosen);
                    }
                }
                _ if k.is_char('H') => {
                    st.hidden = !st.hidden;
                    reload(&mut st);
                }
                _ => {}
            },
        }
    }
}

/// Re-read the current directory in place, keeping the picker open if it
/// fails.
fn reload(st: &mut State) {
    match load(&st.dir, st.mode, st.hidden) {
        Ok(r) => {
            st.rows = r;
            st.err.clear();
            st.sel = 0;
            st.top = 0;
        }
        Err(e) => st.err = e,
    }
}

/// Move to `dir`, or report why not and stay put.
fn enter(st: &mut State, dir: PathBuf, select: Option<&str>) {
    match load(&dir, st.mode, st.hidden) {
        Ok(r) => {
            st.dir = dir;
            st.rows = r;
            st.err.clear();
            st.sel = 0;
            st.top = 0;
            if let Some(name) = select {
                st.select_named(name);
            }
        }
        Err(e) => st.err = e,
    }
}

fn ascend(st: &mut State) {
    let child = st.dir.file_name().map(|n| n.to_string_lossy().into_owned());
    let up = parent_of(&st.dir);
    if up == st.dir {
        return; // already at /
    }
    enter(st, up, child.as_deref());
}

/// Act on the selected row. `Some` means the picker is done.
fn activate(st: &mut State) -> Option<String> {
    match st.rows.get(st.sel)?.clone() {
        Row::UseCurrent => Some(st.dir.to_string_lossy().into_owned()),
        Row::Parent => {
            ascend(st);
            None
        }
        Row::Entry(e) if e.is_dir => {
            let target = st.dir.join(&e.name);
            enter(st, target, None);
            None
        }
        Row::Entry(e) => Some(st.dir.join(&e.name).to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::os::unix::fs::PermissionsExt;

    /// Fixture: beta/, alpha/, .hidden/, zz.txt, aa.txt, .dotfile
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        for d in ["beta", "alpha", ".hidden"] {
            std::fs::create_dir(p.join(d)).expect("mkdir");
        }
        for f in ["zz.txt", "aa.txt", ".dotfile"] {
            std::fs::write(p.join(f), b"").expect("write");
        }
        dir
    }

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::label).collect()
    }

    #[test]
    fn listing_sorts_dirs_first_then_alphabetically() {
        let fix = fixture();
        let got = list_dir(fix.path(), false).expect("list");
        assert_eq!(
            got.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta", "aa.txt", "zz.txt"]
        );
        assert!(got[0].is_dir && !got[2].is_dir);
    }

    #[test]
    fn hidden_entries_appear_only_when_asked_for() {
        let fix = fixture();
        assert_eq!(list_dir(fix.path(), false).expect("list").len(), 4);
        let all = list_dir(fix.path(), true).expect("list");
        assert_eq!(all.len(), 6);
        // Still dirs-first, and the dot entries sort inside their group.
        assert_eq!(all[0].name, ".hidden");
        assert_eq!(all[3].name, ".dotfile");
    }

    #[test]
    fn symlinked_directories_count_as_directories() {
        let fix = fixture();
        std::os::unix::fs::symlink("alpha", fix.path().join("link")).expect("symlink");
        let got = list_dir(fix.path(), false).expect("list");
        let link = got.iter().find(|e| e.name == "link").expect("link listed");
        assert!(link.is_dir);
        // ... so it sorts with the directories.
        assert_eq!(got[2].name, "link");
    }

    #[test]
    fn an_unreadable_directory_is_an_error_not_a_panic() {
        if reads_denied_dirs() {
            return; // root reads everything; nothing to assert
        }
        let fix = fixture();
        let locked = fix.path().join("alpha");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        assert!(list_dir(&locked, false).is_err());
        assert!(load(&locked, Mode::Any, false).is_err());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(list_dir(&locked, false).is_ok());
    }

    /// Root reads a 0o000 directory happily, which would make the
    /// permission-denied tests assert nothing. Probe for it directly rather
    /// than pulling in libc just to call `geteuid`.
    fn reads_denied_dirs() -> bool {
        let Ok(t) = tempfile::tempdir() else {
            return true;
        };
        let p = t.path().join("probe");
        if std::fs::create_dir(&p).is_err()
            || std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).is_err()
        {
            return true;
        }
        let readable = std::fs::read_dir(&p).is_ok();
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
        readable
    }

    #[test]
    fn directory_mode_hides_files_and_offers_the_current_dir() {
        let fix = fixture();
        let e = list_dir(fix.path(), false).expect("list");
        let r = rows(Mode::Directory, fix.path(), e);
        assert_eq!(
            labels(&r),
            ["[ use this directory ]", "../", "alpha/", "beta/"]
        );
    }

    #[test]
    fn file_mode_lists_files_and_never_confirms_a_directory() {
        let fix = fixture();
        let e = list_dir(fix.path(), false).expect("list");
        let r = rows(Mode::File, fix.path(), e);
        assert_eq!(labels(&r), ["../", "alpha/", "beta/", "aa.txt", "zz.txt"]);
        assert!(!r.contains(&Row::UseCurrent));
    }

    #[test]
    fn any_mode_offers_both() {
        let fix = fixture();
        let e = list_dir(fix.path(), false).expect("list");
        let r = rows(Mode::Any, fix.path(), e);
        assert_eq!(r[0], Row::UseCurrent);
        assert!(r.iter().any(|x| x.label() == "aa.txt"));
    }

    #[test]
    fn the_root_directory_offers_no_way_up() {
        let r = rows(Mode::Directory, Path::new("/"), Vec::new());
        assert_eq!(labels(&r), ["[ use this directory ]"]);
        assert!(!r.contains(&Row::Parent));
    }

    #[test]
    fn ascending_from_root_stays_at_root() {
        assert_eq!(parent_of(Path::new("/")), PathBuf::from("/"));
        assert_eq!(parent_of(Path::new("/usr/lib")), PathBuf::from("/usr"));
        assert_eq!(parent_of(Path::new("/usr")), PathBuf::from("/"));

        let mut st = State {
            field: "Where".into(),
            mode: Mode::Directory,
            dir: PathBuf::from("/"),
            rows: rows(Mode::Directory, Path::new("/"), Vec::new()),
            sel: 0,
            top: 0,
            hidden: false,
            err: String::new(),
        };
        ascend(&mut st);
        assert_eq!(st.dir, PathBuf::from("/"));
    }

    #[test]
    fn ascending_reselects_the_directory_just_left() {
        let fix = fixture();
        let start = fix.path().join("alpha");
        let mut st = State {
            field: "Where".into(),
            mode: Mode::Directory,
            dir: start.clone(),
            rows: Vec::new(),
            sel: 0,
            top: 0,
            hidden: false,
            err: String::new(),
        };
        enter(&mut st, start, None);
        ascend(&mut st);
        assert_eq!(st.dir, fix.path());
        assert_eq!(st.rows[st.sel].label(), "alpha/");
    }

    #[test]
    fn start_directory_resolution() {
        let fix = fixture();
        let d = fix.path();

        // A directory is browsed directly.
        assert_eq!(start_dir(&d.to_string_lossy()), d);
        // A file opens its parent.
        assert_eq!(start_dir(&d.join("aa.txt").to_string_lossy()), d);
        // A path that does not exist walks up to one that does.
        assert_eq!(start_dir(&d.join("nope/deeper/x").to_string_lossy()), d);
        // Whitespace is ignored.
        assert_eq!(start_dir(&format!("  {}  ", d.display())), d);
        // Non-paths fall back.
        let fallback = home_or_root();
        assert_eq!(start_dir(""), fallback);
        assert_eq!(start_dir("systemctl"), fallback);
        assert_eq!(start_dir("relative/path"), fallback);
        // The fallback is always a real directory.
        assert!(fallback.is_dir());
    }

    #[test]
    fn start_directory_of_root_is_root() {
        assert_eq!(start_dir("/"), PathBuf::from("/"));
    }

    #[test]
    fn command_lines_keep_their_arguments() {
        assert_eq!(
            split_command("/usr/bin/rsync -a /src /dst"),
            ("/usr/bin/rsync".into(), "-a /src /dst".into())
        );
        assert_eq!(split_command("/bin/true"), ("/bin/true".into(), "".into()));
        assert_eq!(split_command("  "), ("".into(), "".into()));
        assert_eq!(
            join_command("/bin/borg", "create ::x /home"),
            "/bin/borg create ::x /home"
        );
        assert_eq!(join_command("/bin/true", ""), "/bin/true");
        // Round trip: picking the same binary changes nothing.
        let cmd = "/usr/bin/env FOO=1 /bin/sh -c 'echo hi'";
        let (p, a) = split_command(cmd);
        assert_eq!(join_command(&p, &a), cmd);
    }

    #[test]
    fn picker_rect_never_leaves_the_frame() {
        for w in 0..12u16 {
            for h in 0..12u16 {
                let a = Rect {
                    x: 2,
                    y: 3,
                    width: w,
                    height: h,
                };
                let p = picker_rect(a);
                assert!(p.x >= a.x && p.y >= a.y, "{w}x{h}");
                assert!(p.x + p.width <= a.x + a.width, "{w}x{h}");
                assert!(p.y + p.height <= a.y + a.height, "{w}x{h}");
            }
        }
        let zero = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(picker_rect(zero).width, 0);
        assert_eq!(picker_rect(zero).height, 0);
    }

    fn state_for(dir: &Path, mode: Mode) -> State {
        let entries = list_dir(dir, false).unwrap_or_default();
        State {
            field: "test".into(),
            mode,
            dir: dir.to_path_buf(),
            rows: rows(mode, dir, entries),
            sel: 0,
            top: 0,
            hidden: false,
            err: String::new(),
        }
    }

    /// ratatui panics on out-of-bounds rects rather than clipping, so the
    /// picker has to survive terminals nobody sensible uses.
    #[test]
    fn draws_at_every_size_without_panicking() {
        let fix = fixture();
        for mode in [Mode::Directory, Mode::File, Mode::Any] {
            let mut st = state_for(fix.path(), mode);
            for (w, h) in [(1, 1), (2, 2), (3, 4), (10, 3), (40, 10), (200, 60)] {
                let mut t = Terminal::new(TestBackend::new(w, h)).expect("backend");
                t.draw(|f| draw(f, &mut st)).expect("draw");
            }
        }
    }

    #[test]
    fn draws_an_empty_and_an_errored_directory() {
        let empty = tempfile::tempdir().expect("tempdir");
        let mut st = state_for(empty.path(), Mode::File);
        assert!(st.rows.iter().all(|r| *r == Row::Parent));
        for (w, h) in [(1, 1), (30, 6), (120, 40)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).expect("backend");
            t.draw(|f| draw(f, &mut st)).expect("draw");
        }
        st.err = "permission denied".into();
        let mut t = Terminal::new(TestBackend::new(60, 12)).expect("backend");
        t.draw(|f| draw(f, &mut st)).expect("draw");
    }

    #[test]
    fn a_long_listing_scrolls_the_selection_into_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..200 {
            std::fs::write(dir.path().join(format!("f{i:03}")), b"").expect("write");
        }
        let mut st = state_for(dir.path(), Mode::File);
        st.sel = st.rows.len() - 1;
        let mut t = Terminal::new(TestBackend::new(80, 12)).expect("backend");
        t.draw(|f| draw(f, &mut st)).expect("draw");
        assert!(st.top > 0);
        assert!(st.sel >= st.top);
    }

    #[test]
    fn activating_rows_descends_ascends_and_selects() {
        let fix = fixture();
        let mut st = state_for(fix.path(), Mode::Any);

        // [ use this directory ] confirms where we are.
        assert_eq!(st.sel, 0);
        assert_eq!(
            activate(&mut st).as_deref(),
            Some(fix.path().to_string_lossy().as_ref())
        );

        // A directory row descends instead of confirming.
        st.select_named("alpha");
        assert_eq!(activate(&mut st), None);
        assert_eq!(st.dir, fix.path().join("alpha"));

        // ../ climbs back out.
        st.sel = st.rows.iter().position(|r| *r == Row::Parent).expect("..");
        assert_eq!(activate(&mut st), None);
        assert_eq!(st.dir, fix.path());

        // A file row is the answer.
        st.select_named("aa.txt");
        assert_eq!(
            activate(&mut st).as_deref(),
            Some(fix.path().join("aa.txt").to_string_lossy().as_ref())
        );
    }

    #[test]
    fn a_failed_descent_keeps_the_picker_where_it_was() {
        if reads_denied_dirs() {
            return;
        }
        let fix = fixture();
        let locked = fix.path().join("alpha");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let mut st = state_for(fix.path(), Mode::Directory);
        st.select_named("alpha");
        assert_eq!(activate(&mut st), None);
        assert_eq!(st.dir, fix.path(), "stays put on a denied directory");
        assert!(st.err.contains("alpha"), "reports why: {}", st.err);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}
