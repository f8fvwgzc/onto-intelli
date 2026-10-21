//! Millisecond timestamps and time-ordered ids without a date library.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Time-ordered, collision-resistant id such as `evt_0199f3a1b2c00002a4f1`:
/// milliseconds, then a process-wide sequence (keeps order within one
/// millisecond), then random bits.
pub fn new_id(prefix: &str) -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) & 0xff_ffff;
    format!("{prefix}_{:011x}{seq:06x}{:04x}", now_ms(), fastrand::u16(..))
}

/// RFC 3339 UTC rendering of a millisecond timestamp.
pub fn fmt_ts(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

/// Date part only, e.g. `2026-10-17`.
pub fn fmt_date(ms: i64) -> String {
    fmt_ts(ms)[..10].to_string()
}

/// Milliseconds since the epoch of an ISO 8601 / RFC 3339 date or date-time
/// as databases print them: `2026-10-17`, `2026-10-17T12:00:00Z`,
/// `2026-10-17 12:00:00.250+02:00`, `2026-10-17T12:00` (no offset: UTC).
/// `None` for anything else.
pub fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from + len)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    let mut ms = days_from_civil(y, m as u32, d as u32) * 86_400_000;
    if b.len() == 10 {
        return Some(ms);
    }
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let (h, mi) = (num(11, 2)?, num(14, 2)?);
    if b.get(13) != Some(&b':') || h > 23 || mi > 59 {
        return None;
    }
    let mut i = 16;
    let mut sec = 0;
    if b.get(i) == Some(&b':') {
        sec = num(i + 1, 2)?;
        if sec > 60 {
            return None;
        }
        i += 3;
    }
    let mut frac_ms = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let digits = &s[start..i];
        if digits.is_empty() {
            return None;
        }
        let padded = format!("{:0<3}", &digits[..digits.len().min(3)]);
        frac_ms = padded.parse::<i64>().ok()?;
    }
    ms += ((h * 60 + mi) * 60 + sec) * 1000 + frac_ms;
    match &s[i..] {
        "" | "Z" | "z" => Some(ms),
        zone => {
            let sign = match zone.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
            if !digits.chars().all(|c| c.is_ascii_digit()) || !matches!(digits.len(), 2 | 4) {
                return None;
            }
            let oh: i64 = digits[..2].parse().ok()?;
            let om: i64 = if digits.len() == 4 { digits[2..].parse().ok()? } else { 0 };
            Some(ms - sign * (oh * 60 + om) * 60_000)
        }
    }
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's days-from-civil.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// Howard Hinnant's days-from-civil inverse.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(fmt_ts(0), "1970-01-01T00:00:00Z");
        assert_eq!(fmt_ts(1_792_238_400_000), "2026-10-17T12:00:00Z");
        assert_eq!(fmt_date(951_782_400_000), "2000-02-29");
    }

    #[test]
    fn parses_dates_and_date_times() {
        assert_eq!(parse_ts("1970-01-01"), Some(0));
        assert_eq!(parse_ts("2026-10-17T12:00:00Z"), Some(1_792_238_400_000));
        assert_eq!(parse_ts("2026-10-17 12:00:00"), Some(1_792_238_400_000));
        assert_eq!(parse_ts("2026-10-17T14:00:00+02:00"), Some(1_792_238_400_000));
        assert_eq!(parse_ts("2026-10-17T07:30-0430"), Some(1_792_238_400_000));
        assert_eq!(parse_ts("2026-10-17T12:00:00.25+00"), Some(1_792_238_400_250));
        assert_eq!(parse_ts("2000-02-29"), Some(951_782_400_000));
        for bad in ["2026-02-30", "2026-13-01", "17/10/2026", "2026-10-17T25:00", "2026-10-17Tnoon", "infinity", ""] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
        for ms in [0, 951_782_400_000, 1_792_238_400_000, -86_400_000] {
            assert_eq!(parse_ts(&fmt_ts(ms)), Some(ms));
        }
    }

    #[test]
    fn ids_are_prefixed_and_unique() {
        let a = new_id("req");
        let b = new_id("req");
        assert!(a.starts_with("req_"));
        assert_ne!(a, b);
    }
}
