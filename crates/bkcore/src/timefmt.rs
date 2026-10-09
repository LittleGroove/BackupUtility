//! Turning a Unix timestamp into text a human can read.
//!
//! A backup tool needs this in exactly two places — naming attic folders and
//! printing run reports — which is not enough to justify a date-time
//! dependency, so the civil-date conversion is here. The algorithm is Howard
//! Hinnant's `civil_from_days`, which is exact for the whole proleptic
//! Gregorian range and needs no lookup tables.
//!
//! Everything is UTC. An attic folder named in local time would jump backwards
//! at a DST transition and could collide with an earlier run's folder.

/// Seconds in a day.
const DAY: i64 = 86_400;

/// `(year, month, day)` for a count of days since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since 1970-01-01 for a calendar date. The exact inverse of
/// [`civil_from_days`], so a timestamp survives a round trip through text.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    // Shift so the year starts in March and leap days land at the cycle's end.
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // March-based
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parse a stamp produced by [`stamp`] back into a Unix timestamp.
///
/// Used to work out how old an attic folder is from its name, so pruning does
/// not need a separate index that could drift out of step with the folders
/// actually on disk.
pub fn parse_stamp(s: &str) -> Option<u64> {
    // A stamp may carry a `-2`, `-3`, ... suffix when two runs began in the
    // same second. Parse the fixed-width prefix and require any remainder to
    // be exactly that: a hyphen and digits. Anything else is not ours.
    let (head, rest) = s.split_at(s.len().min(15));
    let suffix_ok = rest.is_empty()
        || (rest.starts_with('-')
            && rest.len() > 1
            && rest[1..].bytes().all(|b| b.is_ascii_digit()));
    if !suffix_ok {
        return None;
    }
    let (date, time) = head.split_once('-')?;
    if date.len() != 8 || time.len() != 6 {
        return None;
    }
    let num = |t: &str| t.parse::<u32>().ok();
    let (y, mo, d) = (
        date[0..4].parse::<i64>().ok()?,
        num(&date[4..6])?,
        num(&date[6..8])?,
    );
    let (h, mi, sec) = (num(&time[0..2])?, num(&time[2..4])?, num(&time[4..6])?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    let total = days * DAY + (h * 3600 + mi * 60 + sec) as i64;
    (total >= 0).then_some(total as u64)
}

/// Split a Unix timestamp into its calendar and clock parts, in UTC.
pub fn parts(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    // `div_euclid`/`rem_euclid` keep pre-1970 timestamps correct; plain
    // division would round towards zero and land on the wrong day.
    let days = unix_secs.div_euclid(DAY);
    let secs = unix_secs.rem_euclid(DAY);
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

/// `20260904-153000` — sortable, filename-safe. Used for attic folder names.
pub fn stamp(unix_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = parts(unix_secs as i64);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// `2026-09-04 15:30:00Z` — for run reports and log lines.
pub fn readable(unix_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = parts(unix_secs as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// `2h 14m 07s` — an elapsed duration, at the precision a human cares about.
pub fn duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// The current Unix time in seconds. The one place in `bkcore` that reads the
/// clock, so the rest of the engine stays deterministic and testable.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_itself() {
        assert_eq!(parts(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(readable(0), "1970-01-01 00:00:00Z");
    }

    #[test]
    fn known_timestamps_convert_correctly() {
        // 2024-02-29T12:34:56Z — a leap day, the case naive code gets wrong.
        assert_eq!(parts(1_709_210_096), (2024, 2, 29, 12, 34, 56));
        // 2026-09-04T00:00:00Z
        assert_eq!(parts(1_788_480_000), (2026, 9, 4, 0, 0, 0));
        // 2000-03-01T00:00:00Z — just past the century leap year.
        assert_eq!(parts(951_868_800), (2000, 3, 1, 0, 0, 0));
        // 2100-03-01T00:00:00Z — 2100 is NOT a leap year.
        assert_eq!(parts(4_107_542_400), (2100, 3, 1, 0, 0, 0));
    }

    #[test]
    fn pre_epoch_timestamps_do_not_land_on_the_wrong_day() {
        // Truncating division would give 1969-12-32-ish nonsense here.
        assert_eq!(parts(-1), (1969, 12, 31, 23, 59, 59));
        assert_eq!(parts(-86_400), (1969, 12, 31, 0, 0, 0));
    }

    #[test]
    fn stamps_sort_chronologically() {
        let a = stamp(1_700_000_000);
        let b = stamp(1_800_000_000);
        assert!(a < b, "{a} should sort before {b}");
        assert_eq!(a.len(), 15);
        assert!(a.chars().all(|c| c.is_ascii_digit() || c == '-'));
    }

    #[test]
    fn stamps_round_trip_back_into_timestamps() {
        // Attic pruning works out a folder's age from its name, so this has to
        // be exact — an off-by-one-day error would delete the wrong backups.
        for t in [
            0u64,
            1_700_000_000,
            1_709_210_096, // a leap day
            1_788_480_000,
            4_107_542_400, // 2100, which is not a leap year
        ] {
            assert_eq!(parse_stamp(&stamp(t)), Some(t), "round trip failed for {t}");
        }
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for days in [-10_000i64, -1, 0, 1, 19_000, 20_000, 50_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "failed at {days}");
        }
    }

    #[test]
    fn disambiguating_suffixes_still_parse() {
        // Two runs starting in the same second get `-2`, `-3`, ... appended.
        // If pruning could not read those names it would never clean them up.
        let base = stamp(1_700_000_000);
        assert_eq!(parse_stamp(&base), Some(1_700_000_000));
        assert_eq!(parse_stamp(&format!("{base}-2")), Some(1_700_000_000));
        assert_eq!(parse_stamp(&format!("{base}-17")), Some(1_700_000_000));
        // But a suffix that is not a plain number is not one of ours.
        assert_eq!(parse_stamp(&format!("{base}-keep")), None);
        assert_eq!(parse_stamp(&format!("{base}x")), None);
    }

    #[test]
    fn malformed_stamps_are_rejected_rather_than_guessed_at() {
        // A stray directory in the attic must not be parsed into some
        // arbitrary date and then pruned on the strength of it.
        for bad in [
            "",
            "notastamp",
            "2026-09-04",
            "20260904",
            "20260904-1159",
            "20261304-115908", // month 13
            "20260932-115908", // day 32
            "20260904-256008", // hour 25
            "20260904-119908", // minute 99
            "abcdefgh-ijklmn",
        ] {
            assert_eq!(parse_stamp(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(125), "2m 05s");
        assert_eq!(duration(8047), "2h 14m 07s");
    }
}
