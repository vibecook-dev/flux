//! Small internal helpers (time, dates, hashing) to avoid pulling in a date crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch (0 if the clock is before 1970).
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// UTC days since the Unix epoch.
pub(crate) fn utc_day(unix_secs: u64) -> u64 {
    unix_secs / 86_400
}

/// Civil (year, month, day) from days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`, valid for the proleptic Gregorian calendar).
pub(crate) fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// A cheap pseudo-random value in `0.0..1.0` for backoff jitter (no `rand` dependency).
pub(crate) fn jitter_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .subsec_nanos();
    let h = xxhash_rust::xxh3::xxh3_64(&nanos.to_le_bytes());
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Hex xxh3-128 of some bytes (cache keys, prompt hashes, content fingerprints).
pub(crate) fn hash_hex(bytes: &[u8]) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes))
}

/// The year of a `YYYY-MM-DD` (or `YYYY`) date string, if plausible.
pub(crate) fn year_of(date: Option<&str>) -> Option<u16> {
    let y: u16 = date?.get(..4)?.parse().ok()?;
    (1870..=2200).contains(&y).then_some(y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-10-18 is day 20744.
        assert_eq!(civil_from_days(20_744), (2026, 10, 18));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn years() {
        assert_eq!(year_of(Some("1999-03-30")), Some(1999));
        assert_eq!(year_of(Some("")), None);
        assert_eq!(year_of(None), None);
        assert_eq!(year_of(Some("0001-01-01")), None);
    }
}
