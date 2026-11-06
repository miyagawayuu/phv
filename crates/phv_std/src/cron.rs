//! Schedules of `[[schedule]]` in phv.toml: the five fields of cron (minute,
//! hour, day of the month, month, day of the week) in a time zone.
//!
//! Each field is `*`, a number, a range `a-b`, a step `*/n` or `a-b/n`, or a
//! list of them separated by commas. Days of the week are 0 to 7 (0 and 7 are
//! Sunday). As in cron, when both the day of the month and the day of the week
//! are restricted, a day matching either one matches. Names (`MON`, `JAN`) and
//! the `@daily` forms are not accepted.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// The day of the month and the day of the week were both restricted.
    either_day: bool,
    tz: Tz,
}

/// One field: the set of its values, as bits.
fn field(text: &str, min: u32, max: u32, name: &str) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let mut any = false;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let s: u32 = s.parse().map_err(|_| format!("{name}: \"{part}\" has an invalid step"))?;
                if s == 0 || s > max {
                    return Err(format!("{name}: the step of \"{part}\" must be between 1 and {max}"));
                }
                (r, s)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            any |= step == 1;
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a: u32 = a.parse().map_err(|_| format!("{name}: \"{part}\" is not a number or range"))?;
            let b: u32 = b.parse().map_err(|_| format!("{name}: \"{part}\" is not a number or range"))?;
            (a, b)
        } else {
            let a: u32 = range.parse().map_err(|_| format!("{name}: \"{part}\" is not a number or range"))?;
            // `5/15`: from 5 to the end, every 15.
            (a, if step > 1 { max } else { a })
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("{name}: \"{part}\" is outside {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, any))
}

impl Cron {
    /// Parses `"0 3 * * *"` in the time zone `tz` (an IANA name; None is UTC).
    pub fn parse(spec: &str, tz: Option<&str>) -> Result<Cron, String> {
        let parts: Vec<&str> = spec.split_whitespace().collect();
        let [mi, h, dom, mo, dow] = parts[..] else {
            return Err(format!(
                "\"{spec}\" must have five fields: minute, hour, day of the month, month, day of the week"
            ));
        };
        let tz = match tz {
            None => Tz::UTC,
            Some(name) => name.parse::<Tz>().map_err(|_| format!("unknown time zone \"{name}\""))?,
        };
        let (minutes, _) = field(mi, 0, 59, "minute")?;
        let (hours, _) = field(h, 0, 23, "hour")?;
        let (days, any_day) = field(dom, 1, 31, "day of the month")?;
        let (months, _) = field(mo, 1, 12, "month")?;
        let (mut weekdays, any_weekday) = field(dow, 0, 7, "day of the week")?;
        // 7 is Sunday too.
        if weekdays & (1 << 7) != 0 {
            weekdays |= 1;
        }
        Ok(Cron {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: (weekdays & 0x7f) as u8,
            either_day: !any_day && !any_weekday,
            tz,
        })
    }

    /// Whether the schedule runs in the minute that `at` falls in.
    pub fn matches(&self, at: DateTime<Utc>) -> bool {
        let t = self.tz.from_utc_datetime(&at.naive_utc());
        let day = self.days & (1 << t.day()) != 0;
        let weekday = self.weekdays & (1 << t.weekday().num_days_from_sunday()) != 0;
        let day_ok = if self.either_day { day || weekday } else { day && weekday };
        self.minutes & (1 << t.minute()) != 0
            && self.hours & (1 << t.hour()) != 0
            && self.months & (1 << t.month()) != 0
            && day_ok
    }

    /// The minutes (Unix time of their start, in seconds) after `from` up to and
    /// including `to` in which the schedule runs; at most `max` of them.
    pub fn due(&self, from: i64, to: i64, max: usize) -> Vec<i64> {
        let mut out = Vec::new();
        let mut m = (from.div_euclid(60) + 1) * 60;
        while m <= to && out.len() < max {
            if let Some(at) = Utc.timestamp_opt(m, 0).single() {
                if self.matches(at) {
                    out.push(m);
                }
            }
            m += 60;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn fields_and_matches() {
        let c = Cron::parse("0 3 * * *", None).unwrap();
        assert!(c.matches(at("2026-11-06T03:00:30Z")));
        assert!(!c.matches(at("2026-11-06T03:01:00Z")));
        let c = Cron::parse("*/15 9-17 * * 1-5", None).unwrap();
        assert!(c.matches(at("2026-11-06T09:45:00Z")), "a Friday");
        assert!(!c.matches(at("2026-11-07T09:45:00Z")), "a Saturday");
        assert!(!c.matches(at("2026-11-06T09:44:00Z")));
        // 7 is Sunday as 0 is.
        assert!(Cron::parse("0 0 * * 7", None).unwrap().matches(at("2026-11-08T00:00:00Z")));
        // Day of the month or day of the week, when both are given.
        let c = Cron::parse("0 0 1 * 1", None).unwrap();
        assert!(c.matches(at("2026-12-01T00:00:00Z")), "the 1st (a Tuesday)");
        assert!(c.matches(at("2026-11-09T00:00:00Z")), "a Monday");
        assert!(!c.matches(at("2026-11-10T00:00:00Z")));
        // The last minutes of a month and a step from a number.
        let c = Cron::parse("5/20 23 31 * *", None).unwrap();
        assert!(c.matches(at("2026-12-31T23:45:00Z")));
        assert!(!c.matches(at("2026-11-30T23:45:00Z")), "November has no 31st");
        for bad in [
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
            "MON * * * *",
            "@daily",
        ] {
            assert!(Cron::parse(bad, None).is_err(), "{bad}");
        }
        assert!(Cron::parse("* * * * *", Some("Mars/Base")).unwrap_err().contains("unknown time zone"));
    }

    /// In a time zone: Tokyo is UTC+9; New York skips 02:00-03:00 when summer
    /// time begins and has 01:00-02:00 twice when it ends.
    #[test]
    fn time_zones_and_summer_time() {
        let c = Cron::parse("0 3 * * *", Some("Asia/Tokyo")).unwrap();
        assert!(c.matches(at("2026-11-05T18:00:00Z")));
        let c = Cron::parse("30 2 * * *", Some("America/New_York")).unwrap();
        let day = |d: &str| {
            c.due(at(&format!("{d}T00:00:00Z")).timestamp(), at(&format!("{d}T23:59:00Z")).timestamp() + 86_400, 10)
                .len()
        };
        assert_eq!(day("2026-03-08"), 1, "the day before the change and none on it");
        let c = Cron::parse("30 1 * * *", Some("America/New_York")).unwrap();
        let from = at("2026-11-01T04:00:00Z").timestamp();
        assert_eq!(c.due(from, from + 4 * 3600, 10).len(), 2, "01:30 comes twice when summer time ends");
    }

    #[test]
    fn due_lists_the_minutes_after_from() {
        let c = Cron::parse("*/10 * * * *", None).unwrap();
        let from = at("2026-11-06T00:00:00Z").timestamp();
        assert_eq!(c.due(from, from + 3600, 100).len(), 6, "00:10 ... 01:00");
        assert_eq!(c.due(from - 1, from, 100), vec![from]);
        assert_eq!(c.due(from, from + 86_400, 3).len(), 3);
    }
}
