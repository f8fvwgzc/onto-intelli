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
    fn ids_are_prefixed_and_unique() {
        let a = new_id("req");
        let b = new_id("req");
        assert!(a.starts_with("req_"));
        assert_ne!(a, b);
    }
}
