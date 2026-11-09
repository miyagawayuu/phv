//! Dates in time zones (`std.time`), with the IANA database of `chrono-tz`:
//! the fields of an instant in a zone, the instant of a local date and time,
//! formatting with `date()`'s letters, and parsing ISO 8601 text.

use super::{int, s};
use chrono::{DateTime, Datelike, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use phv_vm::{BuiltinFn, Value, Vm, VmError};

pub(crate) fn zone(vm: &mut Vm, name: &str) -> Result<Tz, VmError> {
    name.parse::<Tz>().map_err(|_| {
        vm.throw("ValueError", format!("unknown time zone `{name}` (use a name like \"Asia/Tokyo\" or \"UTC\")"))
    })
}

pub(crate) fn instant(vm: &mut Vm, ts: i64, tz: Tz) -> Result<DateTime<Tz>, VmError> {
    match Utc.timestamp_opt(ts, 0) {
        LocalResult::Single(t) => Ok(t.with_timezone(&tz)),
        _ => Err(vm.throw("ValueError", "the time is out of range")),
    }
}

/// The instant a local date and time stands for in `tz`. A time that occurs
/// twice (clocks turned back) is the earlier one; a time that does not occur
/// (clocks turned forward) is moved past the gap.
fn local(tz: Tz, naive: NaiveDateTime) -> Option<i64> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => Some(t.timestamp()),
        LocalResult::Ambiguous(a, b) => Some(a.timestamp().min(b.timestamp())),
        LocalResult::None => {
            let later = naive.checked_add_signed(chrono::Duration::hours(1))?;
            tz.from_local_datetime(&later).earliest().map(|t| t.timestamp())
        }
    }
}

/// `2026-11-05`, `2026-11-05 10:00`, `2026-11-05T10:00:30`, and RFC 3339 with
/// an offset or `Z` (which then decides the instant, not `tz`).
pub fn parse(text: &str, tz: Tz) -> Option<i64> {
    let t = text.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Some(d.timestamp());
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, f) {
            return local(tz, n);
        }
    }
    let d = NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()?;
    local(tz, d.and_hms_opt(0, 0, 0)?)
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__tz_check" => |vm, a| {
            zone(vm, s(&a[0]))?;
            Ok(Value::Null)
        },
        // [year, month, day, hour, minute, second, weekday (1 = Monday), UTC offset in seconds]
        "__tz_fields" => |vm, a| {
            let tz = zone(vm, s(&a[1]))?;
            let t = instant(vm, int(&a[0]), tz)?;
            let off = t.offset().fix().local_minus_utc() as i64;
            let fields = [
                t.year() as i64,
                t.month() as i64,
                t.day() as i64,
                t.hour() as i64,
                t.minute() as i64,
                t.second() as i64,
                t.weekday().number_from_monday() as i64,
                off,
            ];
            Ok(Value::list(fields.into_iter().map(Value::Int).collect()))
        },
        "__tz_make" => |vm, a| {
            let tz = zone(vm, s(&a[6]))?;
            let n: Vec<i64> = a[..6].iter().map(int).collect();
            let date = i32::try_from(n[0])
                .ok()
                .and_then(|y| NaiveDate::from_ymd_opt(y, n[1].try_into().ok()?, n[2].try_into().ok()?));
            let naive =
                date.and_then(|d| d.and_hms_opt(n[3].try_into().ok()?, n[4].try_into().ok()?, n[5].try_into().ok()?));
            let Some(naive) = naive else {
                return Err(vm.throw(
                    "ValueError",
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} is not a valid date and time",
                        n[0], n[1], n[2], n[3], n[4], n[5]
                    ),
                ));
            };
            match local(tz, naive) {
                Some(ts) => Ok(Value::Int(ts)),
                None => Err(vm.throw("ValueError", "the date is out of range")),
            }
        },
        "__tz_format" => |vm, a| {
            let tz = zone(vm, s(&a[1]))?;
            let t = instant(vm, int(&a[0]), tz)?;
            super::sys::reserve_date(vm, s(&a[2]))?;
            Ok(Value::str(super::sys::format_date(s(&a[2]), &t)))
        },
        "__tz_parse" => |vm, a| {
            let tz = zone(vm, s(&a[1]))?;
            Ok(parse(s(&a[0]), tz).map_or(Value::Null, Value::Int))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_and_daylight_saving() {
        let tokyo: Tz = "Asia/Tokyo".parse().unwrap();
        let ny: Tz = "America/New_York".parse().unwrap();
        assert_eq!(parse("2026-01-01 09:00", tokyo), Some(1_767_225_600));
        assert_eq!(parse("2026-01-01T00:00:00Z", ny), Some(1_767_225_600), "an offset in the text wins");
        // 2026-03-08 02:30 does not exist in New York: moved past the gap (03:30 EDT).
        let gap = parse("2026-03-08 02:30", ny).unwrap();
        assert_eq!(Utc.timestamp_opt(gap, 0).unwrap().with_timezone(&ny).hour(), 3);
        // 2026-11-01 01:30 happens twice: the earlier one (EDT).
        let twice = parse("2026-11-01 01:30", ny).unwrap();
        assert_eq!(Utc.timestamp_opt(twice, 0).unwrap().with_timezone(&ny).offset().fix().local_minus_utc(), -4 * 3600);
        assert_eq!(parse("tomorrow", ny), None);
    }
}
