//! A line diff, written out rather than imported.
//!
//! Unit files are tens of lines long, so the textbook LCS table is both fast
//! enough and easy to trust. The only concession to size is [`MAX_LINES`]:
//! past that the quadratic table stops being a good idea and the file is
//! reported as replaced wholesale, which is the honest summary anyway.

use super::dialogs::{self, Background};
use super::term::{popup_rect, Key, Term};
use crossterm::event::KeyCode;
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Above this many lines on either side, fall back to a wholesale replace.
const MAX_LINES: usize = 2000;

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Present in both, unchanged.
    Same(String),
    /// Only in the old text.
    Del(String),
    /// Only in the new text.
    Add(String),
    /// `n` unchanged lines elided by [`with_context`].
    Skip(usize),
}

impl Change {
    /// The marker column: `-`, `+` or a space.
    pub fn sigil(&self) -> char {
        match self {
            Change::Same(_) => ' ',
            Change::Del(_) => '-',
            Change::Add(_) => '+',
            Change::Skip(_) => '@',
        }
    }

    fn text(&self) -> String {
        match self {
            Change::Same(s) | Change::Del(s) | Change::Add(s) => s.clone(),
            Change::Skip(n) => {
                format!("@@ {n} unchanged line{} @@", if *n == 1 { "" } else { "s" })
            }
        }
    }
}

/// Diff two texts by line. A trailing newline is not a line of its own, so
/// `"a\n"` and `"a"` compare equal -- the difference is invisible in a unit
/// file and flagging it would be noise.
pub fn diff(old: &str, new: &str) -> Vec<Change> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    diff_lines(&a, &b)
}

/// [`diff`] over lines that have already been split.
pub fn diff_lines(a: &[&str], b: &[&str]) -> Vec<Change> {
    if a.is_empty() && b.is_empty() {
        return Vec::new();
    }
    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        let mut out: Vec<Change> = a.iter().map(|l| Change::Del(l.to_string())).collect();
        out.extend(b.iter().map(|l| Change::Add(l.to_string())));
        return out;
    }

    // lcs[i][j] = length of the longest common subsequence of a[i..] and b[j..].
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    // Walk the table forwards, emitting deletions before insertions so a
    // changed line reads as `-old` then `+new`.
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(Change::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(Change::Del(a[i].to_string()));
            i += 1;
        } else {
            out.push(Change::Add(b[j].to_string()));
            j += 1;
        }
    }
    while i < n {
        out.push(Change::Del(a[i].to_string()));
        i += 1;
    }
    while j < m {
        out.push(Change::Add(b[j].to_string()));
        j += 1;
    }
    out
}

/// True when nothing actually changed.
pub fn is_empty(changes: &[Change]) -> bool {
    changes
        .iter()
        .all(|c| matches!(c, Change::Same(_) | Change::Skip(_)))
}

/// `(added, removed)` line counts.
pub fn stats(changes: &[Change]) -> (usize, usize) {
    let add = changes
        .iter()
        .filter(|c| matches!(c, Change::Add(_)))
        .count();
    let del = changes
        .iter()
        .filter(|c| matches!(c, Change::Del(_)))
        .count();
    (add, del)
}

/// Collapse runs of unchanged lines to `context` on each side of a change,
/// replacing the middle with a [`Change::Skip`].
///
/// A run is only worth eliding if it saves more lines than the marker costs,
/// so short runs are left alone.
pub fn with_context(changes: &[Change], context: usize) -> Vec<Change> {
    // Which unchanged lines are close enough to a change to keep.
    let keep: Vec<bool> = (0..changes.len())
        .map(|i| {
            if !matches!(changes[i], Change::Same(_)) {
                return true;
            }
            let lo = i.saturating_sub(context);
            let hi = (i + context).min(changes.len().saturating_sub(1));
            (lo..=hi).any(|j| !matches!(changes[j], Change::Same(_)))
        })
        .collect();

    let mut out = Vec::new();
    let mut run = 0usize;
    for (i, c) in changes.iter().enumerate() {
        if keep[i] {
            if run > 0 {
                flush_skip(&mut out, changes, i - run, run);
                run = 0;
            }
            out.push(c.clone());
        } else {
            run += 1;
        }
    }
    if run > 0 {
        flush_skip(&mut out, changes, changes.len() - run, run);
    }
    out
}

/// Emit an elided run, or the lines themselves when eliding would not pay.
fn flush_skip(out: &mut Vec<Change>, changes: &[Change], start: usize, run: usize) {
    if run > 1 {
        out.push(Change::Skip(run));
    } else {
        out.extend_from_slice(&changes[start..start + run]);
    }
}

/// The diff as plain text, one line per change -- the shape the tests assert
/// against, where colour cannot be seen.
#[cfg(test)]
pub fn render_text(changes: &[Change]) -> String {
    changes
        .iter()
        .map(|c| match c {
            Change::Skip(_) => c.text(),
            _ => format!("{}{}", c.sigil(), c.text()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The diff as coloured ratatui lines: green additions, red deletions,
/// unchanged text dimmed so the changes carry the eye.
pub fn render_lines(changes: &[Change], width: usize) -> Vec<Line<'static>> {
    changes
        .iter()
        .map(|c| {
            let style = match c {
                Change::Add(_) => Style::new().fg(Color::Green),
                Change::Del(_) => Style::new().fg(Color::Red),
                Change::Same(_) => Style::new().fg(Color::Gray),
                Change::Skip(_) => Style::new().fg(Color::DarkGray).italic(),
            };
            let text = match c {
                Change::Skip(_) => c.text(),
                _ => format!("{}{}", c.sigil(), c.text()),
            };
            Line::from(Span::styled(clip(&text, width), style))
        })
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

/// A whole file's worth of diff, with the heading the dialog shows.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub name: String,
    pub changes: Vec<Change>,
    /// True when the on-disk text differs from what was read when the editor
    /// opened -- someone else wrote the file underneath us.
    pub drifted: bool,
}

impl FileDiff {
    pub fn changed(&self) -> bool {
        !is_empty(&self.changes)
    }
}

/// Build the diff for one file: `baseline` is what the editor read when it
/// opened, `disk` what is there now, `new` what is about to be written.
///
/// `baseline` is `None` for a file that did not exist when the editor opened.
pub fn file_diff(name: &str, baseline: Option<&str>, disk: Option<&str>, new: &str) -> FileDiff {
    let old = disk.unwrap_or("");
    // A file that appeared, vanished or changed under the editor has drifted.
    let drifted = baseline.map(str::to_string) != disk.map(str::to_string);
    FileDiff {
        name: name.to_string(),
        changes: with_context(&diff(old, new), 3),
        drifted,
    }
}

// ---------------------------------------------------------------------------
// Baselines
// ---------------------------------------------------------------------------

/// What each unit file held when the editor opened. `None` means the file was
/// not there.
pub type Baseline = BTreeMap<PathBuf, Option<String>>;

/// Read the current contents of `paths` as a baseline.
pub fn snapshot<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Baseline {
    paths
        .into_iter()
        .map(|p| (p.to_path_buf(), std::fs::read_to_string(p).ok()))
        .collect()
}

/// Diff what is about to be written against what is on disk, file by file.
pub fn against_disk(baseline: &Baseline, targets: &[(PathBuf, String)]) -> Vec<FileDiff> {
    targets
        .iter()
        .map(|(path, body)| {
            let disk = std::fs::read_to_string(path).ok();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let base = baseline.get(path).cloned().flatten();
            file_diff(&name, base.as_deref(), disk.as_deref(), body)
        })
        .collect()
}

/// True when any file changed under the editor.
pub fn any_drift(diffs: &[FileDiff]) -> bool {
    diffs.iter().any(|d| d.drifted)
}

/// True when writing would actually change something on disk.
pub fn any_change(diffs: &[FileDiff]) -> bool {
    diffs.iter().any(FileDiff::changed)
}

/// The whole review as lines, with a heading per file. Returned rather than
/// rendered so the height can be measured and the text can be tested.
pub fn review_lines(diffs: &[FileDiff], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for d in diffs {
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        let (add, del) = stats(&d.changes);
        let heading = if d.changed() {
            format!("{}  (+{add} -{del})", d.name)
        } else {
            format!("{}  (unchanged)", d.name)
        };
        out.push(Line::from(Span::styled(
            clip(&heading, width),
            Style::new().bold().underlined(),
        )));
        if d.drifted {
            out.push(Line::from(Span::styled(
                clip(
                    "! this file changed on disk while you were editing -- \
                     the diff below is against the new contents",
                    width,
                ),
                Style::new().fg(Color::Yellow).bold(),
            )));
        }
        if d.changed() {
            out.extend(render_lines(&d.changes, width));
        } else {
            out.push(Line::from(Span::styled(
                clip("  (no change)", width),
                Style::new().fg(Color::DarkGray),
            )));
        }
    }
    out
}

/// The review dialog's rect: nearly the whole frame, sized down rather than
/// clipped.
pub fn diff_rect(area: Rect) -> Rect {
    popup_rect(
        area,
        area.width.saturating_sub(4).max(1),
        area.height.saturating_sub(2).max(1),
    )
}

/// Show the diff and ask. Returns true only on an explicit yes.
///
/// Scrolls with the usual keys; `y` confirms, `n`, `q` and Escape refuse. A
/// lost terminal counts as a refusal, because writing unit files is not
/// something to do on a guess.
pub fn review(term: &mut Term, bg: Background, title: &str, diffs: &[FileDiff]) -> bool {
    let mut top = 0usize;
    loop {
        let (t, ds, tp) = (title.to_string(), diffs.to_vec(), top);
        let mut page = 1usize;
        let mut total = 1usize;
        dialogs::draw_over(term, bg, &mut |f| {
            let area = diff_rect(f.area());
            f.render_widget(Clear, area);
            let heading = format!(
                " {t} -- y writes, n cancels{} ",
                if any_drift(&ds) {
                    ", CHANGED ON DISK"
                } else {
                    ""
                }
            );
            let block = dialogs::dialog_block(&heading);
            let inner = block.inner(area);
            f.render_widget(block, area);
            let lines = review_lines(&ds, inner.width as usize);
            total = lines.len().max(1);
            page = (inner.height as usize).max(1);
            let shown: Vec<Line> = lines.into_iter().skip(tp).take(page).collect();
            f.render_widget(Paragraph::new(shown), inner);
        });
        let max_top = total.saturating_sub(1);
        let Some(k) = term.next_key() else {
            return false;
        };
        match k {
            Key::Resize | Key::Click(..) | Key::DoubleClick(..) => continue,
            Key::Scroll(d) => {
                top = top.saturating_add_signed(d as isize).min(max_top);
            }
            _ => match k.code() {
                Some(KeyCode::Esc) => return false,
                Some(KeyCode::Up) => top = top.saturating_sub(1),
                Some(KeyCode::Down) => top = (top + 1).min(max_top),
                Some(KeyCode::PageUp) => top = top.saturating_sub(page),
                Some(KeyCode::PageDown) => top = (top + page).min(max_top),
                Some(KeyCode::Home) => top = 0,
                Some(KeyCode::End) => top = max_top,
                _ if k.is_char('y') => return true,
                _ if k.is_char('n') || k.is_char('q') => return false,
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
    fn identical_texts_have_no_changes() {
        let d = diff("a\nb\nc", "a\nb\nc");
        assert!(is_empty(&d));
        assert_eq!(stats(&d), (0, 0));
        assert_eq!(d.len(), 3);
    }

    #[test]
    fn a_trailing_newline_is_not_a_change() {
        assert!(is_empty(&diff("a\nb\n", "a\nb")));
        assert!(is_empty(&diff("", "")));
    }

    #[test]
    fn an_added_line_shows_as_an_addition() {
        let d = diff("a\nc", "a\nb\nc");
        assert_eq!(render_text(&d), " a\n+b\n c");
        assert_eq!(stats(&d), (1, 0));
        assert!(!is_empty(&d));
    }

    #[test]
    fn a_removed_line_shows_as_a_deletion() {
        let d = diff("a\nb\nc", "a\nc");
        assert_eq!(render_text(&d), " a\n-b\n c");
        assert_eq!(stats(&d), (0, 1));
    }

    /// A changed line is a deletion immediately followed by an insertion, in
    /// that order -- the reader wants the old value before the new one.
    #[test]
    fn a_changed_line_reads_old_then_new() {
        let d = diff(
            "[Service]\nExecStart=/bin/old\nUser=bob",
            "[Service]\nExecStart=/bin/new\nUser=bob",
        );
        assert_eq!(
            render_text(&d),
            " [Service]\n-ExecStart=/bin/old\n+ExecStart=/bin/new\n User=bob"
        );
        assert_eq!(stats(&d), (1, 1));
    }

    #[test]
    fn creating_a_file_is_all_additions() {
        let d = diff("", "a\nb");
        assert_eq!(render_text(&d), "+a\n+b");
        assert_eq!(stats(&d), (2, 0));
    }

    #[test]
    fn emptying_a_file_is_all_deletions() {
        let d = diff("a\nb", "");
        assert_eq!(stats(&d), (0, 2));
    }

    /// The LCS must find the longest run, not the first plausible one.
    #[test]
    fn a_moved_block_diffs_minimally() {
        let d = diff("a\nb\nc\nd\ne", "a\nc\nd\ne\nf");
        assert_eq!(stats(&d), (1, 1));
        assert_eq!(render_text(&d), " a\n-b\n c\n d\n e\n+f");
    }

    #[test]
    fn interleaved_changes_keep_their_order() {
        let d = diff("1\n2\n3\n4", "1\nX\n3\nY");
        assert_eq!(render_text(&d), " 1\n-2\n+X\n 3\n-4\n+Y");
    }

    #[test]
    fn a_repeated_line_is_not_confused_with_its_twin() {
        let d = diff("x\nx\nx", "x\nx");
        assert_eq!(stats(&d), (0, 1));
    }

    // -----------------------------------------------------------------
    // Context folding
    // -----------------------------------------------------------------

    #[test]
    fn long_unchanged_runs_are_elided() {
        let old: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        let mut new = old.clone();
        new[20] = "line twenty, changed".into();
        let a: Vec<&str> = old.iter().map(String::as_str).collect();
        let b: Vec<&str> = new.iter().map(String::as_str).collect();
        let folded = with_context(&diff_lines(&a, &b), 3);
        assert!(matches!(folded[0], Change::Skip(_)), "{folded:?}");
        assert!(matches!(folded.last(), Some(Change::Skip(_))), "{folded:?}");
        // The changed line and three lines either side survive.
        assert_eq!(stats(&folded), (1, 1));
        let text = render_text(&folded);
        assert!(
            text.contains("line 17") && text.contains("line 23"),
            "{text}"
        );
        assert!(!text.contains("line 5"), "{text}");
        assert!(folded.len() < 20, "{} lines", folded.len());
    }

    #[test]
    fn a_short_unchanged_run_is_not_worth_eliding() {
        // One line between two changes: the marker would be no shorter.
        let d = with_context(&diff("a\nb\nc", "X\nb\nY"), 0);
        assert_eq!(render_text(&d), "-a\n+X\n b\n-c\n+Y");
    }

    #[test]
    fn folding_an_unchanged_file_elides_everything() {
        let d = with_context(&diff("a\nb\nc\nd\ne", "a\nb\nc\nd\ne"), 3);
        assert_eq!(d, vec![Change::Skip(5)]);
        assert!(is_empty(&d));
    }

    #[test]
    fn folding_empty_input_is_empty() {
        assert!(with_context(&[], 3).is_empty());
    }

    #[test]
    fn the_skip_marker_agrees_on_plurals() {
        assert_eq!(render_text(&[Change::Skip(1)]), "@@ 1 unchanged line @@");
        assert_eq!(render_text(&[Change::Skip(9)]), "@@ 9 unchanged lines @@");
    }

    // -----------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------

    #[test]
    fn additions_are_green_and_deletions_red() {
        let d = diff("a", "b");
        let lines = render_lines(&d, 40);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn rendered_lines_never_exceed_the_width() {
        let d = diff("a very long line indeed, quite unreasonably so", "short");
        for w in 0..60usize {
            for l in render_lines(&d, w) {
                assert!(l.spans[0].content.chars().count() <= w, "width {w}");
            }
        }
    }

    // -----------------------------------------------------------------
    // Drift
    // -----------------------------------------------------------------

    #[test]
    fn a_file_untouched_under_the_editor_has_not_drifted() {
        let fd = file_diff("x.service", Some("a\nb"), Some("a\nb"), "a\nc");
        assert!(!fd.drifted);
        assert!(fd.changed());
    }

    #[test]
    fn a_file_rewritten_under_the_editor_has_drifted() {
        let fd = file_diff("x.service", Some("a\nb"), Some("a\nZZZ"), "a\nc");
        assert!(fd.drifted);
    }

    #[test]
    fn a_file_deleted_under_the_editor_has_drifted() {
        let fd = file_diff("x.service", Some("a\nb"), None, "a\nb");
        assert!(fd.drifted);
        // The new text is entirely an addition, since there is nothing there.
        assert_eq!(stats(&fd.changes), (2, 0));
    }

    #[test]
    fn a_file_created_under_the_editor_has_drifted() {
        let fd = file_diff("x.service", None, Some("a"), "a");
        assert!(fd.drifted);
        assert!(!fd.changed());
    }

    #[test]
    fn a_brand_new_file_has_not_drifted() {
        let fd = file_diff("x.service", None, None, "a\nb");
        assert!(!fd.drifted);
        assert!(fd.changed());
    }

    // -----------------------------------------------------------------
    // Baselines and the review
    // -----------------------------------------------------------------

    /// The rendered text of a set of lines. Blank separator lines carry no
    /// spans at all, so they cannot simply be indexed.
    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .first()
                    .map(|s| s.content.to_string())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_baseline_records_missing_files_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let there = dir.path().join("a.service");
        let missing = dir.path().join("b.service");
        std::fs::write(&there, "[Service]\n").unwrap();
        let base = snapshot([there.as_path(), missing.as_path()]);
        assert_eq!(base[&there], Some("[Service]\n".to_string()));
        assert_eq!(base[&missing], None);
    }

    #[test]
    fn diffing_against_disk_notices_an_edit_made_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.service");
        std::fs::write(&path, "[Service]\nExecStart=/bin/old\n").unwrap();
        let base = snapshot([path.as_path()]);

        // Someone else edits the file while the builder is open.
        std::fs::write(&path, "[Service]\nExecStart=/bin/theirs\n").unwrap();

        let targets = vec![(path.clone(), "[Service]\nExecStart=/bin/mine\n".to_string())];
        let diffs = against_disk(&base, &targets);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].name, "a.service");
        assert!(any_drift(&diffs), "the edit underneath was not noticed");
        assert!(any_change(&diffs));
        // The diff is against what is there now, not against the stale copy.
        let text = render_text(&diffs[0].changes);
        assert!(text.contains("-ExecStart=/bin/theirs"), "{text}");
        assert!(text.contains("+ExecStart=/bin/mine"), "{text}");
        assert!(!text.contains("/bin/old"), "{text}");
    }

    #[test]
    fn an_untouched_file_does_not_report_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.service");
        std::fs::write(&path, "[Service]\nExecStart=/bin/old\n").unwrap();
        let base = snapshot([path.as_path()]);
        let targets = vec![(path.clone(), "[Service]\nExecStart=/bin/new\n".to_string())];
        let diffs = against_disk(&base, &targets);
        assert!(!any_drift(&diffs));
        assert!(any_change(&diffs));
    }

    #[test]
    fn writing_the_same_bytes_back_is_no_change_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.service");
        let body = "[Service]\nExecStart=/bin/true\n";
        std::fs::write(&path, body).unwrap();
        let base = snapshot([path.as_path()]);
        let diffs = against_disk(&base, &[(path.clone(), body.to_string())]);
        assert!(!any_change(&diffs));
        assert!(!any_drift(&diffs));
    }

    /// A file deleted underneath the editor is drift, and the whole unit
    /// reads as new -- which is exactly what writing it would do.
    #[test]
    fn a_file_deleted_underneath_is_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.service");
        std::fs::write(&path, "[Service]\n").unwrap();
        let base = snapshot([path.as_path()]);
        std::fs::remove_file(&path).unwrap();
        let diffs = against_disk(&base, &[(path.clone(), "[Service]\n".to_string())]);
        assert!(any_drift(&diffs));
    }

    #[test]
    fn the_review_names_each_file_and_counts_its_changes() {
        let diffs = vec![
            file_diff("a.timer", Some("a"), Some("a"), "a\nb"),
            file_diff("a.service", Some("x"), Some("x"), "x"),
        ];
        let text = text_of(&review_lines(&diffs, 80));
        assert!(text.contains("a.timer  (+1 -0)"), "{text}");
        assert!(text.contains("a.service  (unchanged)"), "{text}");
        assert!(text.contains("(no change)"), "{text}");
    }

    /// Drift is called out in the review itself, not just in the title bar.
    #[test]
    fn the_review_warns_about_a_file_that_changed_on_disk() {
        let diffs = vec![file_diff("a.timer", Some("a"), Some("b"), "c")];
        let text = text_of(&review_lines(&diffs, 100));
        assert!(text.contains("changed on disk"), "{text}");
    }

    #[test]
    fn review_lines_never_exceed_their_width() {
        let diffs = vec![
            file_diff("a-very-long-unit-name.service", Some("a"), Some("b"), "c"),
            file_diff("b.timer", None, None, &"x".repeat(200)),
        ];
        for w in 0..80usize {
            for l in review_lines(&diffs, w) {
                for span in &l.spans {
                    assert!(span.content.chars().count() <= w, "width {w}");
                }
            }
        }
    }

    #[test]
    fn an_empty_review_renders_nothing_rather_than_panicking() {
        assert!(review_lines(&[], 80).is_empty());
        assert!(!any_change(&[]));
        assert!(!any_drift(&[]));
    }

    /// The exhaustive bounds grid for the review dialog.
    #[test]
    fn the_review_dialog_stays_inside_every_frame() {
        for w in 0..=40u16 {
            for h in 0..=40u16 {
                let area = Rect {
                    x: 4,
                    y: 1,
                    width: w,
                    height: h,
                };
                let r = diff_rect(area);
                assert!(r.x >= area.x && r.y >= area.y, "{w}x{h}: {r:?}");
                assert!(r.x + r.width <= area.x + area.width, "{w}x{h}: {r:?}");
                assert!(r.y + r.height <= area.y + area.height, "{w}x{h}: {r:?}");
            }
        }
    }

    #[test]
    fn a_huge_file_falls_back_to_a_wholesale_replace() {
        let old: Vec<String> = (0..MAX_LINES + 1).map(|i| i.to_string()).collect();
        let a: Vec<&str> = old.iter().map(String::as_str).collect();
        let d = diff_lines(&a, &["one line"]);
        assert_eq!(stats(&d), (1, MAX_LINES + 1));
    }
}
