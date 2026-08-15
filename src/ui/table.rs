//! Column layout for the unit table.
//!
//! The table must stay readable at 80 columns, so it sheds columns rather
//! than wrapping. Columns are fitted in strict priority order and the first
//! one that does not fit ends the row -- no lower-priority column sneaks into
//! a gap, which would make the layout jump about as the terminal is resized.
//!
//! The name column takes whatever is left over, because a truncated unit name
//! is the one thing that makes a row useless.

/// A column of the table, in the order it is offered space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    /// Always present.
    Name,
    /// The last run's outcome. The whole point of the table, so it goes first.
    Last,
    /// The next elapse.
    Next,
    /// `ActiveState`.
    State,
    /// `OnCalendar=` summary, or the description when there is no schedule.
    Schedule,
    /// `UnitFileState`.
    Enabled,
    /// timer / service / mount.
    Kind,
}

impl Col {
    pub fn title(self) -> &'static str {
        match self {
            Col::Name => "UNIT",
            Col::Last => "LAST",
            Col::Next => "NEXT",
            Col::State => "STATE",
            Col::Schedule => "SCHEDULE",
            Col::Enabled => "ENABLED",
            Col::Kind => "KIND",
        }
    }

    /// The narrowest this column is worth showing at.
    fn min_width(self) -> usize {
        match self {
            Col::Name => 8,
            Col::Last => 8,
            Col::Next => 9,
            Col::State => 8,
            Col::Schedule => 12,
            Col::Enabled => 8,
            Col::Kind => 9,
        }
    }
}

/// Everything after the name, in the order it earns space.
const OPTIONAL: [Col; 6] = [
    Col::Last,
    Col::Next,
    Col::State,
    Col::Schedule,
    Col::Enabled,
    Col::Kind,
];

/// The three-character selection marker every row carries.
pub const MARKER_WIDTH: usize = 3;

/// Widest the name column is allowed to grow before spare space goes to the
/// schedule instead.
const NAME_MAX: usize = 40;

/// A fitted set of columns and their widths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub cols: Vec<(Col, usize)>,
    /// The frame width the plan was fitted to.
    pub width: usize,
}

impl Plan {
    pub fn has(&self, c: Col) -> bool {
        self.cols.iter().any(|(k, _)| *k == c)
    }

    #[cfg(test)]
    pub fn width_of(&self, c: Col) -> usize {
        self.cols
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, w)| *w)
            .unwrap_or(0)
    }

    /// The width the rendered row occupies, marker included. Clamped to the
    /// frame, so a frame too narrow even for the marker still yields a row
    /// that fits.
    pub fn total(&self) -> usize {
        let sum: usize = self.cols.iter().map(|(_, w)| w).sum();
        (MARKER_WIDTH + sum + self.cols.len().saturating_sub(1)).min(self.width)
    }
}

/// Fit the table to `width`.
pub fn plan(width: usize) -> Plan {
    // The name's opening bid: a third of the terminal, within reason, and
    // never more than there is room for once the marker is paid for.
    let avail = width.saturating_sub(MARKER_WIDTH);
    let name = (width / 3).clamp(Col::Name.min_width(), 24).min(avail);
    let mut cols = vec![(Col::Name, name)];
    let mut spare = avail.saturating_sub(name);

    for c in OPTIONAL {
        // Each column costs its width plus the space separating it.
        let cost = c.min_width() + 1;
        if spare < cost {
            break;
        }
        spare -= cost;
        cols.push((c, c.min_width()));
    }

    // Leftovers widen the name first, then the schedule, so long paths and
    // long calendar specs both get room on a wide terminal.
    let mut plan = Plan { cols, width };
    if spare > 0 {
        let grow = spare.min(NAME_MAX.saturating_sub(name));
        widen(&mut plan, Col::Name, grow);
        spare -= grow;
    }
    if spare > 0 {
        let target = if plan.has(Col::Schedule) {
            Col::Schedule
        } else {
            Col::Name
        };
        widen(&mut plan, target, spare);
    }
    plan
}

fn widen(plan: &mut Plan, col: Col, by: usize) {
    if let Some((_, w)) = plan.cols.iter_mut().find(|(k, _)| *k == col) {
        *w += by;
    }
}

/// The cell values for one row, already stringified.
#[derive(Debug, Clone, Default)]
pub struct Cells {
    pub name: String,
    pub last: String,
    pub next: String,
    pub state: String,
    pub schedule: String,
    pub enabled: String,
    pub kind: String,
}

impl Cells {
    fn get(&self, c: Col) -> &str {
        match c {
            Col::Name => &self.name,
            Col::Last => &self.last,
            Col::Next => &self.next,
            Col::State => &self.state,
            Col::Schedule => &self.schedule,
            Col::Enabled => &self.enabled,
            Col::Kind => &self.kind,
        }
    }
}

/// Render one row: the marker, then each cell padded to its column.
///
/// The result is clipped to the plan's total width, so it can never overflow
/// the area it was fitted for.
pub fn row(plan: &Plan, marker: char, cells: &Cells) -> String {
    let mut out = String::new();
    if MARKER_WIDTH > 0 {
        out.push(' ');
        out.push(marker);
        out.push(' ');
    }
    let last = plan.cols.len().saturating_sub(1);
    for (i, (c, w)) in plan.cols.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let text = clip(cells.get(*c), *w);
        if i == last {
            out.push_str(&text);
        } else {
            out.push_str(&format!("{text:<w$}"));
        }
    }
    clip(&out, plan.total())
}

/// The header row, in the same columns.
pub fn header(plan: &Plan) -> String {
    let cells = Cells {
        name: Col::Name.title().into(),
        last: Col::Last.title().into(),
        next: Col::Next.title().into(),
        state: Col::State.title().into(),
        schedule: Col::Schedule.title().into(),
        enabled: Col::Enabled.title().into(),
        kind: Col::Kind.title().into(),
    };
    row(plan, ' ', &cells)
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

    fn cells() -> Cells {
        Cells {
            name: "notcron-backup.timer".into(),
            last: "exit 2".into(),
            next: "in 1h 49m".into(),
            state: "failed".into(),
            schedule: "*-*-* 03:00:00".into(),
            enabled: "enabled".into(),
            kind: "timer".into(),
        }
    }

    /// The bounds test that matters: nothing the table renders may ever be
    /// wider than the space it was given, at any width at all.
    #[test]
    fn no_plan_or_row_ever_exceeds_its_width() {
        for w in 0..=250usize {
            let p = plan(w);
            assert!(p.total() <= w, "plan for {w} totals {}", p.total());
            assert!(p.has(Col::Name), "{w}: the name column is not optional");
            for (c, cw) in &p.cols {
                assert!(*cw <= w, "{w}: {c:?} is {cw} wide");
            }
            let r = row(&p, '>', &cells());
            assert!(r.chars().count() <= w, "row for {w}: {:?}", r);
            let h = header(&p);
            assert!(h.chars().count() <= w, "header for {w}: {:?}", h);
        }
    }

    /// A cell longer than its column is truncated, never wrapped, and never
    /// pushes the columns after it out of alignment.
    #[test]
    fn overlong_cells_are_truncated_in_place() {
        let p = plan(120);
        let c = Cells {
            name: "a".repeat(300),
            last: "b".repeat(300),
            next: "c".repeat(300),
            state: "d".repeat(300),
            schedule: "e".repeat(300),
            enabled: "f".repeat(300),
            kind: "g".repeat(300),
        };
        let r = row(&p, '>', &c);
        // Every column is full, so the row fills its width exactly.
        assert_eq!(r.chars().count(), p.total());
        assert!(!r.contains('\n'));
        // A short value in the final column simply leaves the tail blank.
        let short = row(&p, '>', &cells());
        assert!(short.chars().count() <= p.total());
    }

    /// Empty cells still hold their columns open, so a row with nothing to
    /// say lines up with the rows around it.
    #[test]
    fn empty_cells_keep_the_columns_aligned() {
        let p = plan(120);
        let mut short = cells();
        short.name = "a".into();
        let a = row(&p, '>', &cells());
        let b = row(&p, '>', &short);
        // "exit 2" is in the Last column in both, at the same offset.
        assert_eq!(a.find("exit 2"), b.find("exit 2"));
        assert!(a.find("exit 2").is_some());
        // A row whose cells are all empty is pure padding, then nothing.
        assert_eq!(row(&p, ' ', &Cells::default()).trim(), "");
    }

    #[test]
    fn columns_are_dropped_in_reverse_priority_as_the_terminal_narrows() {
        // Wide: everything.
        let wide = plan(160);
        for c in OPTIONAL {
            assert!(wide.has(c), "{c:?} missing at 160");
        }
        // 80 columns: the health columns survive, which is the requirement.
        let eighty = plan(80);
        assert!(eighty.has(Col::Last) && eighty.has(Col::Next));
        assert!(eighty.has(Col::State));
        // Narrow: the health columns are the last to go.
        let sixty = plan(60);
        assert!(sixty.has(Col::Last) && sixty.has(Col::Next));
        assert!(!sixty.has(Col::Schedule), "{:?}", sixty.cols);
        let forty = plan(40);
        assert!(forty.has(Col::Last));
        assert!(!forty.has(Col::State), "{:?}", forty.cols);
        // Very narrow: the name, and at most the last-run outcome.
        assert!(plan(20).cols.len() <= 2, "{:?}", plan(20).cols);
        assert_eq!(plan(12).cols.len(), 1, "{:?}", plan(12).cols);
    }

    /// Narrowing the terminal must only ever remove columns, never add one --
    /// otherwise the layout flickers as it is resized.
    #[test]
    fn column_sets_only_shrink_as_the_width_shrinks() {
        let mut previous: Vec<Col> = plan(250).cols.iter().map(|(c, _)| *c).collect();
        for w in (0..=250usize).rev() {
            let now: Vec<Col> = plan(w).cols.iter().map(|(c, _)| *c).collect();
            for c in &now {
                assert!(previous.contains(c), "{w}: {c:?} appeared as it narrowed");
            }
            previous = now;
        }
    }

    #[test]
    fn the_name_column_takes_the_slack_on_a_wide_terminal() {
        assert!(plan(200).width_of(Col::Name) >= 40);
        // ...but not without limit; the schedule gets the rest.
        assert_eq!(plan(250).width_of(Col::Name), NAME_MAX);
        assert!(plan(250).width_of(Col::Schedule) > Col::Schedule.min_width());
    }

    #[test]
    fn every_column_has_a_title_that_fits_its_minimum() {
        for c in OPTIONAL.iter().chain(std::iter::once(&Col::Name)) {
            assert!(
                c.title().len() <= c.min_width(),
                "{c:?}: {:?} does not fit {}",
                c.title(),
                c.min_width()
            );
        }
    }

    #[test]
    fn the_marker_column_shows_the_selection() {
        let p = plan(100);
        assert!(row(&p, '>', &cells()).starts_with(" > "));
        assert!(row(&p, ' ', &cells()).starts_with("   "));
    }

    #[test]
    fn degenerate_widths_do_not_panic() {
        for w in [0usize, 1, 2, 3, 4] {
            let p = plan(w);
            assert!(p.total() <= w);
            assert!(row(&p, '>', &cells()).chars().count() <= w);
        }
    }

    #[test]
    fn the_header_names_exactly_the_columns_that_are_shown() {
        let p = plan(160);
        let h = header(&p);
        for (c, _) in &p.cols {
            assert!(h.contains(c.title()), "{:?} missing from {h:?}", c);
        }
        let narrow = header(&plan(45));
        assert!(!narrow.contains("SCHEDULE"), "{narrow}");
    }
}
