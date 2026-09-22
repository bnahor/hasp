//! A five-field cron expression: `min hour day-of-month month day-of-week`.
//!
//! Local time, not UTC: a personal harness's `0 9 * * 1-5` means *my* 09:00,
//! and a schedule that silently runs at 03:00 would be worse than none. DST is
//! whatever the platform reports for each queried instant, so the rule tracks
//! the clock rather than a fixed offset.
//!
//! Supported per field: `*`, `N`, `A-B`, comma lists, and steps (`*/2`, `5/15`,
//! `1-30/5`). Day-of-week runs 0-7 with both 0 and 7 meaning Sunday. Month and
//! weekday names are deliberately absent — numeric forms cover this harness's
//! needs and keep the parser small.

/// One field's allowed values as a bitset; 60 values is the most any field
/// holds, so a `u64` covers them all.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    bits: u64,
    /// `*` (with or without a step) rather than an explicit list — this decides
    /// the day-of-month vs day-of-week OR rule.
    star: bool,
}

impl Field {
    fn has(&self, v: u32) -> bool {
        self.bits & (1 << v) != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSpec {
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
}

pub fn parse(expr: &str) -> Result<CronSpec, String> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        return Err(format!(
            "cron needs 5 fields (min hour day-of-month month day-of-week), got {}",
            f.len()
        ));
    }
    Ok(CronSpec {
        minute: parse_field(f[0], "minute", 0, 59, false)?,
        hour: parse_field(f[1], "hour", 0, 23, false)?,
        dom: parse_field(f[2], "day-of-month", 1, 31, false)?,
        month: parse_field(f[3], "month", 1, 12, false)?,
        // 0 and 7 both mean Sunday; 7 is folded onto 0.
        dow: parse_field(f[4], "day-of-week", 0, 7, true)?,
    })
}

fn parse_field(s: &str, name: &str, lo: u32, hi: u32, fold7: bool) -> Result<Field, String> {
    if s.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if let Some(step_str) = s.strip_prefix("*/") {
        let step = parse_num(step_str, name, 1, hi)?;
        let mut bits = 0;
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
        return Ok(Field { bits, star: true });
    }
    if s == "*" {
        let mut bits = 0;
        for v in lo..=hi {
            bits |= 1 << (if fold7 && v == 7 { 0 } else { v });
        }
        return Ok(Field { bits, star: true });
    }

    let mut bits = 0u64;
    for part in s.split(',') {
        if part.is_empty() {
            return Err(format!("{name}: empty list item"));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, parse_num(st, name, 1, hi)?),
            None => (part, 1),
        };
        let (a, b) = match range.split_once('-') {
            Some((a, b)) => (parse_num(a, name, lo, hi)?, parse_num(b, name, lo, hi)?),
            None => {
                let n = parse_num(range, name, lo, hi)?;
                // `5/15` means "starting at 5, every 15" — i.e. 5..=hi by 15.
                (n, if step > 1 { hi } else { n })
            }
        };
        if a > b {
            return Err(format!("{name}: range {a}-{b} is backwards"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << (if fold7 && v == 7 { 0 } else { v });
            v += step;
        }
    }
    if bits == 0 {
        return Err(format!("{name} selects nothing"));
    }
    Ok(Field { bits, star: false })
}

fn parse_num(s: &str, name: &str, lo: u32, hi: u32) -> Result<u32, String> {
    let n: u32 = s
        .parse()
        .map_err(|_| format!("{name}: “{s}” is not a number"))?;
    if n < lo || n > hi {
        return Err(format!("{name}: {n} is outside {lo}-{hi}"));
    }
    Ok(n)
}

impl CronSpec {
    pub fn matches(&self, month: u32, dom: u32, dow: u32, hour: u32, minute: u32) -> bool {
        // Vixie rule: when both day fields are restricted, either may match, so
        // `0 0 13 * 5` is the 13th OR any Friday. When only one is restricted,
        // it must match.
        let day = if !self.dom.star && !self.dow.star {
            self.dom.has(dom) || self.dow.has(dow)
        } else {
            self.dom.has(dom) && self.dow.has(dow)
        };
        day && self.month.has(month) && self.hour.has(hour) && self.minute.has(minute)
    }

    /// Next occurrence strictly after `from_ms`, in epoch ms, local time.
    /// `None` when the expression has no occurrence within the search window
    /// (366 days) — the runtime treats that as simply not due.
    pub fn next_due(&self, from_ms: u64) -> Option<u64> {
        self.next_due_with_offset(from_ms, local_offset_secs((from_ms / 1000) as i64))
    }

    /// The same search against a fixed UTC offset, so behaviour can be tested
    /// without depending on the machine's timezone.
    pub fn next_due_with_offset(&self, from_ms: u64, offset_secs: i64) -> Option<u64> {
        // Cron has minute granularity: search the next boundary strictly after
        // `from`, never `from` itself.
        let mut t = (from_ms / 60_000 * 60 + 60) as i64;
        let limit = t + 366 * 24 * 3600;
        while t <= limit {
            let (_, month, dom, hour, min, dow) = utc_fields(t + offset_secs);
            if self.matches(month, dom, dow, hour, min) {
                return Some(t as u64 * 1000);
            }
            t += 60;
        }
        None
    }
}

/// `(year, month, day, hour, min, dow)` for a UTC timestamp; `dow` has 0=Sunday.
fn utc_fields(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday (4).
    let dow = (days.rem_euclid(7) + 4) % 7;
    (
        y,
        m as u32,
        d as u32,
        (sod / 3600) as u32,
        ((sod % 3600) / 60) as u32,
        dow as u32,
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The platform's UTC offset for a given instant, DST decided per instant.
fn local_offset_secs(secs: i64) -> i64 {
    #[cfg(unix)]
    unsafe {
        let t: libc::time_t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        let as_utc = days_from_civil(
            i64::from(tm.tm_year) + 1900,
            i64::from(tm.tm_mon) + 1,
            i64::from(tm.tm_mday),
        ) * 86_400
            + i64::from(tm.tm_hour) * 3600
            + i64::from(tm.tm_min) * 60
            + i64::from(tm.tm_sec);
        as_utc - secs
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Wait before retrying a schedule whose turns keep failing: doubling per
/// consecutive failure and capped at an hour, so one broken schedule cannot
/// spend all day at full rate. A success resets the count, so the next failure
/// starts from the interval again.
pub fn backoff_ms(failures: u32, interval_ms: u64) -> u64 {
    let multiplier = 1u64 << failures.min(6);
    interval_ms.saturating_mul(multiplier).min(3_600_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;
    const DAY: u64 = 86_400_000;

    #[test]
    fn failing_schedules_back_off_and_cap() {
        assert_eq!(backoff_ms(0, 1_000), 1_000, "first failure: the interval");
        assert_eq!(backoff_ms(1, 1_000), 2_000);
        assert_eq!(backoff_ms(3, 1_000), 8_000);
        // Doubling stops after six shifts, so a short interval tops out at
        // 64x — the hour cap only bites for long ones.
        assert_eq!(backoff_ms(20, 1_000), 64_000);
        assert_eq!(
            backoff_ms(20, 7_200_000),
            3_600_000,
            "a long interval is capped at an hour"
        );
        assert_eq!(backoff_ms(6, 120_000), 3_600_000, "cap binds here too");
    }

    #[test]
    fn parses_the_common_forms() {
        for ok in [
            "* * * * *",
            "0 9 * * 1-5",
            "*/15 * * * *",
            "5/15 0 1 1 0",
            "1,30 4 * * *",
            "0 0 29 2 *",
            "0 0 * * 7",
            "1-30/5 * * * *",
        ] {
            assert!(parse(ok).is_ok(), "should parse: {ok}");
        }
    }

    #[test]
    fn rejects_bad_shapes_with_field_names() {
        for bad in [
            "* * * *",     // four fields
            "* * * * * *", // six fields
            "60 * * * *",  // minute out of range
            "* 24 * * *",  // hour
            "* * 0 * *",   // day-of-month starts at 1
            "* * * 13 *",  // month
            "* * * * 8",   // day-of-week ends at 7
            "a * * * *",   // not a number
            "5-2 * * * *", // backwards range
            "*/0 * * * *", // step of zero
            "5, * * * *",  // dangling comma
        ] {
            let err = parse(bad).unwrap_err();
            assert!(!err.is_empty(), "error should say something for {bad}");
        }
        assert!(parse("60 * * * *").unwrap_err().contains("minute"));
        assert!(parse("* * * 13 *").unwrap_err().contains("month"));
    }

    #[test]
    fn seven_is_sunday_like_zero() {
        let seven = parse("0 0 * * 7").unwrap();
        let zero = parse("0 0 * * 0").unwrap();
        assert_eq!(seven, zero);
        assert!(seven.matches(1, 1, 0, 0, 0));
        assert!(!seven.matches(1, 1, 1, 0, 0));
    }

    #[test]
    fn both_day_fields_restricted_means_either() {
        // The 13th, or any Friday — whichever comes.
        let spec = parse("0 0 13 * 5").unwrap();
        assert!(spec.matches(6, 13, 3, 0, 0), "the 13th is enough");
        assert!(spec.matches(6, 14, 5, 0, 0), "a Friday is enough");
        assert!(!spec.matches(6, 14, 3, 0, 0), "neither");

        // Only the day-of-month restricted: it must match.
        let dom_only = parse("0 0 13 * *").unwrap();
        assert!(dom_only.matches(6, 13, 3, 0, 0));
        assert!(!dom_only.matches(6, 14, 5, 0, 0), "dow alone is not enough");
    }

    #[test]
    fn every_minute_lands_on_the_next_boundary() {
        let spec = parse("* * * * *").unwrap();
        let from = 1_789_000_000_123u64; // arbitrary mid-minute instant
        let due = spec.next_due_with_offset(from, 0).unwrap();
        assert!(due > from);
        assert_eq!(due % 60_000, 0, "on a minute boundary");
        assert!(due - from <= 60_000, "no more than a minute away");
    }

    #[test]
    fn weekdays_skip_the_weekend() {
        let spec = parse("0 9 * * 1-5").unwrap();
        let mut due = spec.next_due_with_offset(1_789_000_000_000, 0).unwrap();
        let mut gaps = Vec::new();
        for _ in 0..19 {
            let next = spec.next_due_with_offset(due, 0).unwrap();
            gaps.push(next - due);
            due = next;
        }
        // Weekday 09:00s are a day apart, three across a weekend, never two.
        for g in &gaps {
            assert_eq!(g % HOUR, 0, "aligned to whole hours");
            assert!(*g == DAY || *g == 3 * DAY, "gap of {g} ms");
        }
        assert!(
            gaps.iter().filter(|g| **g == 3 * DAY).count() >= 2,
            "twenty weekday mornings must cross a weekend: {gaps:?}"
        );
    }

    #[test]
    fn finds_feb_29_within_the_window_and_admits_none_beyond() {
        let spec = parse("0 0 29 2 *").unwrap();
        let june_2027 = days_from_civil(2027, 6, 1) * 86_400_000;
        let feb_29_2028 = days_from_civil(2028, 2, 29) * 86_400_000;
        let due = spec.next_due_with_offset(june_2027 as u64, 0).unwrap();
        assert_eq!(due, feb_29_2028 as u64);

        // From March 2026 the next Feb 29 (2028) is 729 days out, beyond the
        // 366-day window: `None`, not a wrong date and not a hang. (From March
        // 2027 it is only 365 days out, so that start date must find it.)
        let march_2026 = days_from_civil(2026, 3, 1) * 86_400_000;
        assert_eq!(spec.next_due_with_offset(march_2026 as u64, 0), None);
    }

    #[test]
    fn fixed_offset_shifts_the_answer() {
        // 09:00 local at UTC+8 is 01:00 UTC.
        let spec = parse("0 9 * * *").unwrap();
        let from = days_from_civil(2026, 9, 20) * 86_400_000;
        let due = spec.next_due_with_offset(from as u64, 8 * 3600).unwrap();
        assert_eq!(due as i64, from + 3_600_000);
    }
}
