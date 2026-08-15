//! cron -> systemd `OnCalendar` translation.
//!
//! A faithful port of the semantics in the original `notcron.sh`:
//!
//! ```text
//! cron fields:  minute hour day-of-month month day-of-week
//! systemd:      [DOW] YYYY-MM-DD HH:MM:SS
//! ```
//!
//! Supported: `*`, fixed values, `*/N` and `a-b/N` and `a/N` steps, ranges,
//! comma lists, month and day names, and the `@hourly` .. `@reboot` macros.

use std::fmt;

const CRON_MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const CRON_DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
/// systemd's canonical weekday order and spelling.
const SYSTEMD_DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronError(String);

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CronError {}

fn err<T>(msg: impl Into<String>) -> Result<T, CronError> {
    Err(CronError(msg.into()))
}

/// What a cron expression translates to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Translation {
    /// One or more `OnCalendar=` values. More than one means the expression
    /// restricted both day-of-month and day-of-week, which cron ORs and
    /// systemd ANDs -- so each branch gets its own line.
    Calendar(Vec<String>),
    /// `@reboot`, which becomes an `OnBootSec=` timer.
    Reboot,
}

/// Which cron field is being parsed; drives name lookups and range rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Min,
    Hour,
    Dom,
    Month,
    Dow,
}

impl Kind {
    fn range(self) -> (u32, u32) {
        match self {
            Kind::Min => (0, 59),
            Kind::Hour => (0, 23),
            Kind::Dom => (1, 31),
            Kind::Month => (1, 12),
            Kind::Dow => (0, 6),
        }
    }
}

/// One expanded field: either "the whole range" or an explicit sorted set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expanded {
    All,
    Set(Vec<u32>),
}

fn resolve_token(raw: &str, kind: Kind) -> Result<u32, CronError> {
    let (min, max) = kind.range();
    let tok = raw.to_ascii_lowercase();
    let mut num = if !tok.is_empty() && tok.bytes().all(|b| b.is_ascii_digit()) {
        // Leading zeros are padding, not octal.
        tok.trim_start_matches('0')
            .parse::<u32>()
            .unwrap_or(0)
            .to_owned()
    } else {
        match kind {
            Kind::Month => match CRON_MONTHS.iter().position(|m| *m == tok) {
                Some(i) => i as u32 + 1,
                None => return err(format!("invalid month name '{raw}'")),
            },
            Kind::Dow => match CRON_DAYS.iter().position(|d| *d == tok) {
                Some(i) => i as u32,
                None => return err(format!("invalid day-of-week name '{raw}'")),
            },
            _ => return err(format!("invalid value '{raw}' (expected a number)")),
        }
    };
    // cron accepts both 0 and 7 for Sunday.
    if kind == Kind::Dow && num == 7 {
        num = 0;
    }
    if num < min || num > max {
        return err(format!("value '{raw}' out of range ({min}-{max})"));
    }
    Ok(num)
}

/// Expand one cron field (a comma list of terms) into the integers it matches.
fn expand_field(field: &str, kind: Kind) -> Result<Expanded, CronError> {
    let (min, max) = kind.range();
    if field.is_empty() {
        return err("empty cron field");
    }
    if !field
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'*' | b',' | b'/' | b'-'))
    {
        return err(format!("invalid character in cron field '{field}'"));
    }

    let mut out: Vec<u32> = Vec::new();
    for term in field.split(',') {
        if term.is_empty() {
            return err(format!("empty term in cron field '{field}'"));
        }

        let mut step = 1u32;
        let mut range = term;
        if let Some((head, tail)) = term.split_once('/') {
            if tail.contains('/') {
                return err(format!("invalid nested step in cron field '{field}'"));
            }
            if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
                return err(format!("invalid step '/{tail}' in cron field '{field}'"));
            }
            step = tail.trim_start_matches('0').parse::<u32>().unwrap_or(0);
            if step < 1 {
                return err(format!("step must be >= 1 in cron field '{field}'"));
            }
            range = head;
        }

        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = split_range(range) {
            (resolve_token(a, kind)?, resolve_token(b, kind)?)
        } else {
            let v = resolve_token(range, kind)?;
            // `5/2` with no explicit end means 5 through the field maximum.
            (v, if term.contains('/') { max } else { v })
        };

        if lo > hi {
            // cron permits wrapping day-of-week ranges, e.g. fri-mon.
            if kind != Kind::Dow {
                return err(format!("inverted range '{range}' in cron field '{field}'"));
            }
            let mut v = lo;
            loop {
                out.push(v);
                if v == hi {
                    break;
                }
                v = (v + 1) % 7;
            }
            continue;
        }

        let mut v = lo;
        while v <= hi {
            out.push(v);
            v += step;
        }
    }

    out.sort_unstable();
    out.dedup();
    if out.len() as u32 == max - min + 1 {
        Ok(Expanded::All)
    } else {
        Ok(Expanded::Set(out))
    }
}

/// Split `a-b`, tolerating a leading `-` only where cron never puts one.
/// Cron fields have no negative values, so the first `-` is the separator.
fn split_range(s: &str) -> Option<(&str, &str)> {
    s.split_once('-')
}

/// `*` or a zero-padded comma list, e.g. `00,15,30,45`.
fn fmt_numeric(e: &Expanded) -> String {
    match e {
        Expanded::All => "*".into(),
        Expanded::Set(v) => v
            .iter()
            .map(|n| format!("{n:02}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Empty string for "any day", else `Mon,Wed` in systemd's Mon..Sun order.
fn fmt_dow(e: &Expanded) -> String {
    match e {
        Expanded::All => String::new(),
        Expanded::Set(v) => [1u32, 2, 3, 4, 5, 6, 0]
            .iter()
            .filter(|d| v.contains(d))
            .map(|d| SYSTEMD_DAYS[if *d == 0 { 6 } else { *d as usize - 1 }])
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Translate a 5-field cron expression or an `@macro` to `OnCalendar` values.
pub fn to_calendar(expr: &str) -> Result<Translation, CronError> {
    let squeezed = expr.split_whitespace().collect::<Vec<_>>().join(" ");

    let expanded: String = match squeezed.to_ascii_lowercase().as_str() {
        "@reboot" => return Ok(Translation::Reboot),
        "@yearly" | "@annually" => "0 0 1 1 *".into(),
        "@monthly" => "0 0 1 * *".into(),
        "@weekly" => "0 0 * * 0".into(),
        "@daily" | "@midnight" => "0 0 * * *".into(),
        "@hourly" => "0 * * * *".into(),
        other if other.starts_with('@') => {
            return err(format!(
                "unknown cron macro '{expr}' (try @hourly, @daily, @weekly, \
                 @monthly, @yearly, @reboot)"
            ))
        }
        _ => squeezed.clone(),
    };

    let fields: Vec<&str> = expanded.split(' ').filter(|f| !f.is_empty()).collect();
    if fields.len() != 5 {
        return err(format!(
            "cron expression needs 5 fields (minute hour day-of-month month \
             day-of-week), got {} in '{expr}'",
            fields.len()
        ));
    }

    let e_min = expand_field(fields[0], Kind::Min)?;
    let e_hour = expand_field(fields[1], Kind::Hour)?;
    let e_dom = expand_field(fields[2], Kind::Dom)?;
    let e_month = expand_field(fields[3], Kind::Month)?;
    let e_dow = expand_field(fields[4], Kind::Dow)?;

    let minute = fmt_numeric(&e_min);
    let hour = fmt_numeric(&e_hour);
    let dom = fmt_numeric(&e_dom);
    let month = fmt_numeric(&e_month);
    let dow = fmt_dow(&e_dow);
    let time = format!("{hour}:{minute}:00");

    // cron treats a restricted day-of-month and day-of-week as OR; systemd's
    // OnCalendar joins them with AND. Two OnCalendar= lines in one timer are
    // ORed by systemd, so emit one per branch to preserve cron semantics.
    if dom != "*" && !dow.is_empty() {
        return Ok(Translation::Calendar(vec![
            format!("*-{month}-{dom} {time}"),
            format!("{dow} *-{month}-* {time}"),
        ]));
    }

    Ok(Translation::Calendar(vec![if dow.is_empty() {
        format!("*-{month}-{dom} {time}")
    } else {
        format!("{dow} *-{month}-{dom} {time}")
    }]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `check_cron` in the shell suite's tests/run-tests.sh: the
    /// expected value is the OnCalendar lines joined with ';'.
    fn cron(expr: &str) -> String {
        match to_calendar(expr).expect("expression should translate") {
            Translation::Calendar(v) => v.join(";"),
            Translation::Reboot => "@reboot".into(),
        }
    }

    fn bad(expr: &str) {
        assert!(
            to_calendar(expr).is_err(),
            "expected '{expr}' to be rejected"
        );
    }

    #[test]
    fn wildcards_and_fixed_values() {
        assert_eq!(cron("* * * * *"), "*-*-* *:*:00");
        assert_eq!(cron("0 3 * * *"), "*-*-* 03:00:00");
        assert_eq!(cron("30 2 * * *"), "*-*-* 02:30:00");
        assert_eq!(cron("0 0 1 1 *"), "*-01-01 00:00:00");
        assert_eq!(cron("59 23 * * *"), "*-*-* 23:59:00");
    }

    #[test]
    fn steps() {
        assert_eq!(cron("*/15 * * * *"), "*-*-* *:00,15,30,45:00");
        assert_eq!(cron("*/30 * * * *"), "*-*-* *:00,30:00");
        assert_eq!(cron("0 */6 * * *"), "*-*-* 00,06,12,18:00:00");
        assert_eq!(cron("*/1 * * * *"), "*-*-* *:*:00");
        assert_eq!(cron("0 0 */10 * *"), "*-*-01,11,21,31 00:00:00");
    }

    #[test]
    fn ranges() {
        assert_eq!(
            cron("0 9-17 * * *"),
            "*-*-* 09,10,11,12,13,14,15,16,17:00:00"
        );
        assert_eq!(cron("0 0 1-5 * *"), "*-*-01,02,03,04,05 00:00:00");
        assert_eq!(cron("0-59 0 1 1 *"), "*-01-01 00:*:00");
    }

    #[test]
    fn ranges_with_a_step() {
        assert_eq!(cron("0 0 1-9/2 * *"), "*-*-01,03,05,07,09 00:00:00");
        assert_eq!(cron("0 8-18/4 * * *"), "*-*-* 08,12,16:00:00");
    }

    #[test]
    fn open_ended_step() {
        assert_eq!(cron("0 0 20/5 * *"), "*-*-20,25,30 00:00:00");
    }

    #[test]
    fn lists() {
        assert_eq!(cron("30 2 1,15 * *"), "*-*-01,15 02:30:00");
        assert_eq!(cron("0 6,12,18 * * *"), "*-*-* 06,12,18:00:00");
        assert_eq!(cron("0,30 * * * *"), "*-*-* *:00,30:00");
    }

    #[test]
    fn day_of_week_numeric_and_named() {
        assert_eq!(cron("0 0 * * 0"), "Sun *-*-* 00:00:00");
        assert_eq!(cron("0 0 * * 7"), "Sun *-*-* 00:00:00");
        assert_eq!(cron("0 0 * * 1"), "Mon *-*-* 00:00:00");
        assert_eq!(cron("0 0 * * sun"), "Sun *-*-* 00:00:00");
        assert_eq!(cron("0 0 * * SUN"), "Sun *-*-* 00:00:00");
        assert_eq!(cron("0 0 * * Mon"), "Mon *-*-* 00:00:00");
        assert_eq!(
            cron("0 9 * * mon-fri"),
            "Mon,Tue,Wed,Thu,Fri *-*-* 09:00:00"
        );
        assert_eq!(cron("0 9 * * 1-5"), "Mon,Tue,Wed,Thu,Fri *-*-* 09:00:00");
        assert_eq!(cron("0 9 * * sat,sun"), "Sat,Sun *-*-* 09:00:00");
        assert_eq!(cron("0 9 * * 6,0"), "Sat,Sun *-*-* 09:00:00");
    }

    #[test]
    fn wrapping_day_of_week_range() {
        // Days come out in systemd's canonical Mon..Sun order, not cron's
        // wrap order.
        assert_eq!(cron("0 9 * * fri-mon"), "Mon,Fri,Sat,Sun *-*-* 09:00:00");
        // All seven days collapses back to a wildcard.
        assert_eq!(cron("0 9 * * 0-6"), "*-*-* 09:00:00");
    }

    #[test]
    fn month_names() {
        assert_eq!(cron("0 0 1 jan *"), "*-01-01 00:00:00");
        assert_eq!(cron("0 0 1 DEC *"), "*-12-01 00:00:00");
        assert_eq!(cron("0 0 1 jan,jul *"), "*-01,07-01 00:00:00");
        assert_eq!(cron("0 0 1 mar-may *"), "*-03,04,05-01 00:00:00");
    }

    #[test]
    fn dom_and_dow_become_two_lines() {
        assert_eq!(cron("15 10 13 * fri"), "*-*-13 10:15:00;Fri *-*-* 10:15:00");
        assert_eq!(cron("0 0 1 * mon"), "*-*-01 00:00:00;Mon *-*-* 00:00:00");
    }

    #[test]
    fn macros() {
        assert_eq!(cron("@hourly"), "*-*-* *:00:00");
        assert_eq!(cron("@daily"), "*-*-* 00:00:00");
        assert_eq!(cron("@midnight"), "*-*-* 00:00:00");
        assert_eq!(cron("@weekly"), "Sun *-*-* 00:00:00");
        assert_eq!(cron("@monthly"), "*-*-01 00:00:00");
        assert_eq!(cron("@yearly"), "*-01-01 00:00:00");
        assert_eq!(cron("@annually"), "*-01-01 00:00:00");
        assert_eq!(cron("@DAILY"), "*-*-* 00:00:00");
        assert_eq!(to_calendar("@reboot").unwrap(), Translation::Reboot);
    }

    #[test]
    fn whitespace_tolerance() {
        assert_eq!(cron("  0   3  *  *  * "), "*-*-* 03:00:00");
        assert_eq!(cron("0\t3 * * *"), "*-*-* 03:00:00");
    }

    #[test]
    fn invalid_input_is_rejected() {
        bad("60 3 * * *"); // minute out of range
        bad("99 3 * * *");
        bad("0 24 * * *"); // hour out of range
        bad("0 3 0 * *"); // day-of-month zero
        bad("0 3 32 * *"); // day-of-month too large
        bad("0 3 * 13 *"); // month out of range
        bad("0 3 * 0 *"); // month zero
        bad("0 3 * * 8"); // day-of-week out of range
        bad("0 3 * * xyz"); // unknown day name
        bad("0 3 * jann *"); // unknown month name
        bad("*/0 * * * *"); // zero step
        bad("5-1 * * * *"); // inverted range
        bad("a b c d e"); // non-numeric field
        bad("0 3 * *"); // too few fields
        bad("0 3 * * * *"); // too many fields
        bad(""); // empty expression
        bad("@bogus"); // unknown macro
        bad("0 3 * * mon;rm"); // illegal character
        bad("0,,5 * * * *"); // empty list term
        bad("*/2/3 * * * *"); // nested step
    }

    #[test]
    fn error_messages_are_descriptive() {
        let e = to_calendar("60 3 * * *").unwrap_err().to_string();
        assert!(e.contains("out of range"), "{e}");
        let e = to_calendar("0 3 * *").unwrap_err().to_string();
        assert!(e.contains("5 fields"), "{e}");
    }
}
