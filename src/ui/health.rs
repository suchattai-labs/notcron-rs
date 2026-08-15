//! Last-run result and next elapse for the unit list.
//!
//! Two sources, one call each:
//!
//! * `systemctl show` gives `ActiveState`, `SubState`, `Result` and
//!   `ExecMainStatus` -- everything needed to say whether the last run was
//!   clean and, when it was not, how it failed.
//! * `systemctl list-timers --output=json` gives the next elapse and last
//!   trigger as raw epoch microseconds. The human-readable
//!   `NextElapseUSecRealtime` that `show` returns is a formatted local
//!   timestamp with only a timezone *abbreviation*, which cannot be turned
//!   back into an instant without a date library; the JSON form can, so the
//!   relative "in 3h" the user actually wants costs no dependency.
//!
//! Every parser here takes text, so the tests run against captured output and
//! need no live units.

use crate::unit::model::Scope;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// The properties one `systemctl show` call asks for.
const SHOW_PROPERTIES: &str =
    "--property=Id,ActiveState,SubState,Result,ExecMainStatus,ExecMainExitTimestamp";

/// What the list needs to know about a unit's health.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    pub active: String,
    pub sub: String,
    /// systemd's `Result`: `success`, `exit-code`, `signal`, `timeout`, ...
    pub result: String,
    /// The process exit status of the last run, when there was one.
    pub exit_status: Option<i32>,
    /// Next elapse, epoch microseconds. Timers only.
    pub next: Option<u64>,
    /// Last trigger, epoch microseconds. Timers only.
    pub last: Option<u64>,
    /// `ExecMainExitTimestamp` as systemd formatted it, for units the timer
    /// listing does not cover.
    pub last_stamp: String,
}

impl Health {
    /// True when this unit's last run did not end cleanly, or when the unit
    /// itself is in the failed state.
    pub fn failed(&self) -> bool {
        self.active == "failed"
            || self.sub == "failed"
            || (!self.result.is_empty() && self.result != "success")
            || self.exit_status.is_some_and(|s| s != 0)
    }

    /// True when the unit is mid-run right now.
    pub fn running(&self) -> bool {
        self.sub == "running" || self.active == "activating"
    }

    /// Whether anything is known at all -- an empty `Health` means the
    /// manager did not answer, and the columns should stay blank rather than
    /// claim the unit is fine.
    pub fn known(&self) -> bool {
        !self.active.is_empty() || self.next.is_some() || self.last.is_some()
    }

    /// The "Last" column: the outcome of the most recent run, short enough to
    /// live in ten characters.
    pub fn last_label(&self) -> String {
        if self.running() {
            return "running".into();
        }
        if !self.known() {
            return "-".into();
        }
        match self.result.as_str() {
            "success" | "" => {
                if self.active == "failed" {
                    "failed".into()
                } else if self.last.is_some() || !self.last_stamp.is_empty() {
                    "ok".into()
                } else {
                    "-".into()
                }
            }
            "exit-code" => match self.exit_status {
                Some(s) => format!("exit {s}"),
                None => "exit".into(),
            },
            "signal" | "core-dump" => "killed".into(),
            "timeout" => "timeout".into(),
            "watchdog" => "watchdog".into(),
            "start-limit-hit" => "throttled".into(),
            "resources" => "resources".into(),
            other => other.to_string(),
        }
    }

    /// The "Next" column: when this fires next, relative to `now`.
    pub fn next_label(&self, now_usec: u64) -> String {
        match self.next {
            Some(t) => format!("in {}", span(t.saturating_sub(now_usec))),
            None => "-".into(),
        }
    }

    /// A sentence for the detail line under the list.
    pub fn detail(&self, now_usec: u64) -> String {
        let mut parts = Vec::new();
        if !self.active.is_empty() {
            parts.push(if self.sub.is_empty() {
                self.active.clone()
            } else {
                format!("{} ({})", self.active, self.sub)
            });
        }
        match (self.last, self.last_stamp.as_str()) {
            (Some(t), _) if t > 0 => parts.push(format!(
                "last {} ago, {}",
                span(now_usec.saturating_sub(t)),
                self.last_label()
            )),
            (_, s) if !s.is_empty() => {
                parts.push(format!("last {}, {}", short_stamp(s), self.last_label()))
            }
            _ => {}
        }
        if let Some(t) = self.next {
            parts.push(format!("next in {}", span(t.saturating_sub(now_usec))));
        }
        parts.join("  --  ")
    }
}

/// Now, in epoch microseconds.
pub fn now_usec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// A duration in microseconds, as systemd would phrase it: the two largest
/// units that matter and no more.
pub fn span(usec: u64) -> String {
    let secs = usec / 1_000_000;
    if secs == 0 {
        return "now".into();
    }
    let (d, h, m, s) = (
        secs / 86_400,
        (secs % 86_400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    if d > 0 {
        if h > 0 {
            format!("{d}d {h}h")
        } else {
            format!("{d}d")
        }
    } else if h > 0 {
        if m > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{h}h")
        }
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// `Fri 2026-08-14 19:44:20 CEST` -> `08-14 19:44`. Anything that does not
/// look like that comes back unchanged, clipped.
pub fn short_stamp(s: &str) -> String {
    let mut it = s.split_whitespace();
    let (Some(_dow), Some(date), Some(time)) = (it.next(), it.next(), it.next()) else {
        return s.chars().take(11).collect();
    };
    let date = date.get(5..).unwrap_or(date);
    let time = time.get(..5).unwrap_or(time);
    format!("{date} {time}")
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Split `systemctl show` output into one property map per unit, keyed by
/// `Id`. Blocks are separated by a blank line; a block without an `Id` is
/// dropped, since there is nothing to attach it to.
pub fn parse_show(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for block in text.split("\n\n") {
        let mut props = BTreeMap::new();
        for line in block.lines() {
            if let Some((k, v)) = line.split_once('=') {
                props.insert(k.trim().to_string(), v.to_string());
            }
        }
        if let Some(id) = props.get("Id").cloned() {
            if !id.is_empty() {
                out.insert(id, props);
            }
        }
    }
    out
}

/// Turn one unit's properties into a [`Health`].
pub fn health_from_props(props: &BTreeMap<String, String>) -> Health {
    let get = |k: &str| props.get(k).cloned().unwrap_or_default();
    let stamp = get("ExecMainExitTimestamp");
    Health {
        active: get("ActiveState"),
        sub: get("SubState"),
        result: get("Result"),
        exit_status: props
            .get("ExecMainStatus")
            .and_then(|s| s.trim().parse().ok()),
        next: None,
        last: None,
        // systemd prints `n/a` for a unit that has never run.
        last_stamp: if stamp == "n/a" { String::new() } else { stamp },
    }
}

/// One row of `systemctl list-timers --output=json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerRow {
    pub unit: String,
    /// Next elapse, epoch microseconds. `None` for a timer that will not fire.
    pub next: Option<u64>,
    /// Last trigger, epoch microseconds. `None` if it never has.
    pub last: Option<u64>,
}

/// Parse `systemctl list-timers --output=json`.
///
/// The shape is an array of flat objects, so a full JSON parser would be
/// three orders of magnitude more machinery than the job needs. Anything that
/// does not scan cleanly yields no rows and the columns simply stay blank.
pub fn parse_list_timers(text: &str) -> Vec<TimerRow> {
    parse_flat_objects(text)
        .into_iter()
        .filter_map(|o| {
            let unit = o.get("unit")?.clone();
            if unit.is_empty() {
                return None;
            }
            Some(TimerRow {
                unit,
                next: o.get("next").and_then(|s| usec(s)),
                last: o.get("last").and_then(|s| usec(s)),
            })
        })
        .collect()
}

/// A timestamp field: a positive epoch value, or `None` for null, zero and
/// the `UINT64_MAX` systemd uses to mean "never".
fn usec(s: &str) -> Option<u64> {
    match s.parse::<u64>() {
        Ok(0) | Ok(u64::MAX) => None,
        Ok(n) => Some(n),
        Err(_) => None,
    }
}

/// Scan an array of flat JSON objects into key -> raw-value maps. String
/// values are unquoted and unescaped; numbers, booleans and null come back as
/// their literal text.
fn parse_flat_objects(text: &str) -> Vec<BTreeMap<String, String>> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '{' {
            i += 1;
            continue;
        }
        let (obj, next) = scan_object(&chars, i);
        if let Some(obj) = obj {
            out.push(obj);
        }
        // A malformed object still advances, so a truncated tail cannot spin.
        i = next.max(i + 1);
    }
    out
}

/// Read one `{...}` starting at `start`, returning it and the index after it.
fn scan_object(chars: &[char], start: usize) -> (Option<BTreeMap<String, String>>, usize) {
    let mut map = BTreeMap::new();
    let mut i = start + 1;
    loop {
        // key
        while i < chars.len() && chars[i] != '"' && chars[i] != '}' {
            i += 1;
        }
        if i >= chars.len() {
            return (None, i);
        }
        if chars[i] == '}' {
            return (Some(map), i + 1);
        }
        let (key, next) = scan_string(chars, i);
        i = next;
        while i < chars.len() && chars[i] != ':' {
            i += 1;
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            return (None, i);
        }
        // value
        if chars[i] == '"' {
            let (val, next) = scan_string(chars, i);
            map.insert(key, val);
            i = next;
        } else {
            let s = i;
            while i < chars.len() && chars[i] != ',' && chars[i] != '}' {
                i += 1;
            }
            let val: String = chars[s..i].iter().collect();
            map.insert(key, val.trim().to_string());
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == ',' {
            i += 1;
        }
    }
}

/// Read one JSON string starting at the opening quote.
fn scan_string(chars: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '"' => return (out, i + 1),
            '\\' if i + 1 < chars.len() => {
                out.push(match chars[i + 1] {
                    'n' => '\n',
                    't' => '\t',
                    c => c,
                });
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, i)
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// Health for every named unit, keyed by unit name.
///
/// `units` should list every file in every entry, not just the primaries: a
/// timer's last-run result lives on its service, not on the timer.
///
/// Nothing here is fatal. A host with no running systemd returns an empty map
/// and the columns stay blank.
pub fn fetch(scope: Scope, units: &[String]) -> BTreeMap<String, Health> {
    if units.is_empty() {
        return BTreeMap::new();
    }
    let mut args = vec!["show", SHOW_PROPERTIES];
    args.extend(units.iter().map(String::as_str));
    let mut out: BTreeMap<String, Health> = match crate::systemd::systemctl(scope, &args) {
        Ok(text) => parse_show(&text)
            .into_iter()
            .map(|(id, props)| (id, health_from_props(&props)))
            .collect(),
        Err(_) => BTreeMap::new(),
    };

    // Only ask for the timer listing if there are timers to look up.
    if units.iter().any(|u| u.ends_with(".timer")) {
        if let Ok(text) =
            crate::systemd::systemctl(scope, &["list-timers", "--all", "--output=json"])
        {
            for row in parse_list_timers(&text) {
                let h = out.entry(row.unit).or_default();
                h.next = row.next;
                h.last = row.last;
            }
        }
    }
    out
}

/// Merge the health of every file in one entry into the single picture the
/// row shows: the timer supplies the schedule, the service the outcome.
pub fn merge(files: &[String], by_unit: &BTreeMap<String, Health>) -> Health {
    let mut merged = Health::default();
    for f in files {
        let Some(h) = by_unit.get(f) else { continue };
        // The timer owns "next"; whichever file has one owns "last".
        if h.next.is_some() {
            merged.next = h.next;
        }
        if h.last.is_some() && merged.last.is_none() {
            merged.last = h.last;
        }
        if merged.last_stamp.is_empty() {
            merged.last_stamp = h.last_stamp.clone();
        }
        // The outcome comes from the unit that actually runs a process, which
        // is the service; a timer's own Result is almost always `success`.
        let is_exec = !f.ends_with(".timer");
        if is_exec || merged.active.is_empty() {
            if is_exec || merged.result.is_empty() {
                merged.result = h.result.clone();
                merged.exit_status = h.exit_status;
            }
            merged.active = h.active.clone();
            merged.sub = h.sub.clone();
        }
        // A failure anywhere in the group is a failure for the row.
        if h.failed() {
            merged.result = h.result.clone();
            merged.exit_status = h.exit_status;
            merged.active = h.active.clone();
            merged.sub = h.sub.clone();
        }
    }
    merged
}

/// Sort key: failed units first, then everything else, each group left in the
/// order it arrived (which is alphabetical, from the directory listing).
pub fn sort_rank(h: &Health) -> u8 {
    if h.failed() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `systemctl show` output for a timer and its service.
    const SHOW: &str = "\
Result=success
ExecMainExitTimestamp=n/a
Id=notcron-backup.timer
ActiveState=active
SubState=waiting

Result=exit-code
ExecMainExitTimestamp=Fri 2026-08-14 19:44:20 CEST
ExecMainStatus=2
Id=notcron-backup.service
ActiveState=failed
SubState=failed
";

    #[test]
    fn show_output_is_split_per_unit() {
        let m = parse_show(SHOW);
        assert_eq!(m.len(), 2);
        assert_eq!(m["notcron-backup.timer"]["ActiveState"], "active");
        assert_eq!(m["notcron-backup.service"]["ExecMainStatus"], "2");
    }

    #[test]
    fn a_block_without_an_id_is_dropped() {
        assert!(parse_show("ActiveState=active\nSubState=waiting\n").is_empty());
        assert!(parse_show("").is_empty());
        assert!(parse_show("no equals sign here").is_empty());
    }

    #[test]
    fn a_clean_service_reads_as_ok() {
        let m = parse_show(SHOW);
        let h = health_from_props(&m["notcron-backup.timer"]);
        assert!(!h.failed());
        assert_eq!(h.result, "success");
        // Never run: no timestamp, so no outcome to report.
        assert_eq!(h.last_stamp, "");
        assert_eq!(h.last_label(), "-");
    }

    #[test]
    fn a_failed_service_carries_its_exit_status() {
        let m = parse_show(SHOW);
        let h = health_from_props(&m["notcron-backup.service"]);
        assert!(h.failed());
        assert_eq!(h.exit_status, Some(2));
        assert_eq!(h.last_label(), "exit 2");
    }

    #[test]
    fn every_systemd_result_word_gets_a_label() {
        let label = |result: &str, status: Option<i32>| {
            Health {
                active: "inactive".into(),
                result: result.into(),
                exit_status: status,
                last_stamp: "Fri 2026-08-14 19:44:20 CEST".into(),
                ..Health::default()
            }
            .last_label()
        };
        assert_eq!(label("success", Some(0)), "ok");
        assert_eq!(label("exit-code", Some(1)), "exit 1");
        assert_eq!(label("exit-code", None), "exit");
        assert_eq!(label("signal", None), "killed");
        assert_eq!(label("core-dump", None), "killed");
        assert_eq!(label("timeout", None), "timeout");
        assert_eq!(label("watchdog", None), "watchdog");
        assert_eq!(label("start-limit-hit", None), "throttled");
        assert_eq!(label("resources", None), "resources");
        // An unknown word from a future systemd is shown, not swallowed.
        assert_eq!(label("something-new", None), "something-new");
        // Every label fits the narrowest column the table ever gives it.
        for r in [
            "success",
            "exit-code",
            "signal",
            "timeout",
            "watchdog",
            "start-limit-hit",
            "resources",
        ] {
            assert!(label(r, Some(137)).chars().count() <= 10, "{r}");
        }
    }

    #[test]
    fn a_running_unit_says_so_rather_than_reporting_the_last_run() {
        let h = Health {
            active: "active".into(),
            sub: "running".into(),
            result: "success".into(),
            ..Health::default()
        };
        assert_eq!(h.last_label(), "running");
        assert!(h.running());
        assert!(!h.failed());
    }

    #[test]
    fn an_unknown_unit_claims_nothing() {
        let h = Health::default();
        assert!(!h.known());
        assert!(!h.failed());
        assert_eq!(h.last_label(), "-");
        assert_eq!(h.next_label(0), "-");
        assert_eq!(h.detail(0), "");
    }

    #[test]
    fn a_failed_active_state_counts_even_with_a_clean_result() {
        let h = Health {
            active: "failed".into(),
            result: "success".into(),
            ..Health::default()
        };
        assert!(h.failed());
        assert_eq!(h.last_label(), "failed");
    }

    // -----------------------------------------------------------------
    // The timer listing
    // -----------------------------------------------------------------

    const TIMERS: &str = r#"[{"next":1786762200000000,"left":1786762200000000,"last":1786761601585463,"passed":6589082235104,"unit":"sysstat-collect.timer","activates":"sysstat-collect.service"},{"next":null,"left":null,"last":0,"passed":null,"unit":"never.timer","activates":"never.service"}]"#;

    #[test]
    fn the_timer_listing_is_parsed() {
        let rows = parse_list_timers(TIMERS);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].unit, "sysstat-collect.timer");
        assert_eq!(rows[0].next, Some(1786762200000000));
        assert_eq!(rows[0].last, Some(1786761601585463));
    }

    #[test]
    fn a_timer_that_will_not_fire_has_no_next_elapse() {
        let rows = parse_list_timers(TIMERS);
        assert_eq!(rows[1].unit, "never.timer");
        assert_eq!(rows[1].next, None);
        assert_eq!(rows[1].last, None);
    }

    #[test]
    fn the_never_sentinel_is_not_a_timestamp() {
        let text = format!(r#"[{{"next":{},"last":0,"unit":"x.timer"}}]"#, u64::MAX);
        let rows = parse_list_timers(&text);
        assert_eq!(rows[0].next, None);
    }

    #[test]
    fn malformed_or_empty_listings_yield_nothing_rather_than_panicking() {
        for text in [
            "",
            "[]",
            "not json at all",
            "{",
            "{\"unit\"",
            "{\"unit\":",
            r#"[{"unit":"x.timer""#,
            r#"[{"no":"unit"}]"#,
            r#"[{"unit":""}]"#,
            "{}{}{}",
            r#"[{"unit":"a.timer","next":"not a number"}]"#,
        ] {
            let rows = parse_list_timers(text);
            assert!(rows.len() <= 1, "{text}: {rows:?}");
        }
        // A truncated object must not lose the fields it did have.
        assert!(parse_list_timers(r#"[{"unit":"a.timer","next":"x"}]"#)[0]
            .next
            .is_none());
    }

    #[test]
    fn escaped_strings_survive_the_scanner() {
        let rows = parse_list_timers(r#"[{"unit":"od\"d.timer","next":5}]"#);
        assert_eq!(rows[0].unit, "od\"d.timer");
        assert_eq!(rows[0].next, Some(5));
    }

    // -----------------------------------------------------------------
    // Merging a group
    // -----------------------------------------------------------------

    fn by_unit() -> BTreeMap<String, Health> {
        let mut m: BTreeMap<String, Health> = parse_show(SHOW)
            .into_iter()
            .map(|(id, p)| (id, health_from_props(&p)))
            .collect();
        m.get_mut("notcron-backup.timer").unwrap().next = Some(2_000_000_000_000_000);
        m.get_mut("notcron-backup.timer").unwrap().last = Some(1_000_000_000_000_000);
        m
    }

    /// The timer supplies the schedule and the service the outcome, so the
    /// row shows a next elapse *and* the exit status that matters.
    #[test]
    fn a_group_merges_the_timer_schedule_with_the_service_outcome() {
        let files = vec![
            "notcron-backup.timer".to_string(),
            "notcron-backup.service".to_string(),
        ];
        let h = merge(&files, &by_unit());
        assert_eq!(h.next, Some(2_000_000_000_000_000));
        assert_eq!(h.last, Some(1_000_000_000_000_000));
        assert_eq!(h.last_label(), "exit 2");
        assert!(h.failed());
    }

    /// File order must not change the answer.
    #[test]
    fn merging_is_order_independent() {
        let m = by_unit();
        let fwd = merge(
            &[
                "notcron-backup.timer".into(),
                "notcron-backup.service".into(),
            ],
            &m,
        );
        let rev = merge(
            &[
                "notcron-backup.service".into(),
                "notcron-backup.timer".into(),
            ],
            &m,
        );
        assert_eq!(fwd, rev);
    }

    #[test]
    fn merging_units_nobody_reported_on_yields_nothing_known() {
        let h = merge(&["ghost.timer".into()], &BTreeMap::new());
        assert!(!h.known());
        assert!(merge(&[], &by_unit()) == Health::default());
    }

    #[test]
    fn a_healthy_group_is_not_marked_failed() {
        let mut m = by_unit();
        let svc = m.get_mut("notcron-backup.service").unwrap();
        svc.result = "success".into();
        svc.exit_status = Some(0);
        svc.active = "inactive".into();
        svc.sub = "dead".into();
        let h = merge(
            &[
                "notcron-backup.timer".into(),
                "notcron-backup.service".into(),
            ],
            &m,
        );
        assert!(!h.failed());
        assert_eq!(h.last_label(), "ok");
    }

    #[test]
    fn failures_sort_above_everything_else() {
        let mut names: Vec<(&str, Health)> = vec![
            (
                "clean",
                Health {
                    active: "active".into(),
                    result: "success".into(),
                    ..Health::default()
                },
            ),
            (
                "broken",
                Health {
                    active: "failed".into(),
                    result: "exit-code".into(),
                    exit_status: Some(1),
                    ..Health::default()
                },
            ),
            (
                "also-clean",
                Health {
                    active: "inactive".into(),
                    ..Health::default()
                },
            ),
            (
                "also-broken",
                Health {
                    result: "timeout".into(),
                    ..Health::default()
                },
            ),
        ];
        names.sort_by_key(|(_, h)| sort_rank(h));
        let order: Vec<&str> = names.iter().map(|(n, _)| *n).collect();
        assert_eq!(order, vec!["broken", "also-broken", "clean", "also-clean"]);
    }

    /// The sort must be stable: two failures keep their alphabetical order.
    #[test]
    fn sorting_is_stable_within_a_rank() {
        let failed = Health {
            result: "exit-code".into(),
            exit_status: Some(1),
            ..Health::default()
        };
        let mut v = [("a", failed.clone()), ("b", failed.clone()), ("c", failed)];
        v.sort_by_key(|(_, h)| sort_rank(h));
        assert_eq!(
            v.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    // -----------------------------------------------------------------
    // Formatting
    // -----------------------------------------------------------------

    #[test]
    fn spans_read_the_way_systemd_phrases_them() {
        let s = |secs: u64| span(secs * 1_000_000);
        assert_eq!(s(0), "now");
        assert_eq!(s(1), "1s");
        assert_eq!(s(59), "59s");
        assert_eq!(s(60), "1m");
        assert_eq!(s(3599), "59m");
        assert_eq!(s(3600), "1h");
        assert_eq!(s(3600 + 2940), "1h 49m");
        assert_eq!(s(86_400), "1d");
        assert_eq!(s(86_400 + 7200), "1d 2h");
        assert_eq!(s(86_400 * 400), "400d");
    }

    #[test]
    fn spans_stay_short_enough_for_the_column() {
        for secs in [0u64, 7, 90, 3600, 90_000, 86_400 * 999] {
            assert!(span(secs * 1_000_000).chars().count() <= 8, "{secs}");
        }
    }

    #[test]
    fn a_next_elapse_in_the_past_does_not_underflow() {
        let h = Health {
            next: Some(5),
            ..Health::default()
        };
        assert_eq!(h.next_label(1_000_000_000), "in now");
    }

    #[test]
    fn timestamps_shorten_to_month_day_and_time() {
        assert_eq!(short_stamp("Fri 2026-08-14 19:44:20 CEST"), "08-14 19:44");
        // Anything unexpected is clipped rather than mangled.
        assert_eq!(short_stamp("n/a"), "n/a");
        assert_eq!(short_stamp(""), "");
        assert_eq!(short_stamp("one two"), "one two");
        assert!(short_stamp("a very long unexpected value").chars().count() <= 11);
    }

    #[test]
    fn the_detail_line_names_state_last_and_next() {
        let now = 2_000_000_000_000_000u64;
        let h = Health {
            active: "active".into(),
            sub: "waiting".into(),
            result: "success".into(),
            next: Some(now + 3_600_000_000),
            last: Some(now - 7_200_000_000),
            ..Health::default()
        };
        let d = h.detail(now);
        assert!(d.contains("active (waiting)"), "{d}");
        assert!(d.contains("last 2h ago"), "{d}");
        assert!(d.contains("next in 1h"), "{d}");
    }

    #[test]
    fn a_unit_with_only_a_formatted_timestamp_still_reports_its_last_run() {
        let h = Health {
            active: "inactive".into(),
            sub: "dead".into(),
            result: "exit-code".into(),
            exit_status: Some(3),
            last_stamp: "Fri 2026-08-14 19:44:20 CEST".into(),
            ..Health::default()
        };
        let d = h.detail(now_usec());
        assert!(d.contains("last 08-14 19:44, exit 3"), "{d}");
    }

    #[test]
    fn the_clock_is_sane() {
        // 2020-01-01 in microseconds; anything below means a broken clock.
        assert!(now_usec() > 1_577_836_800_000_000);
    }
}
