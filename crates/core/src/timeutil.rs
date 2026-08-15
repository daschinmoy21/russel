//! Minimal civil-date / RFC3339 helpers (no `chrono` dependency).
//!
//! All timestamps are UTC, second precision, `Z`-suffixed, zero-padded.
//! The civil-date conversion uses the Howard Hinnant public-domain algorithm.

/// Format UNIX seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC, second precision,
/// zero-padded, `Z` suffix). Deterministic and byte-stable for a given input.
pub fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let hour = tod / 3_600;
    let min = (tod % 3_600) / 60;
    let sec = tod % 60;

    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` (UTC, second precision) into UNIX seconds.
///
/// Returns `None` for malformed input (too short, no `Z` suffix, non-numeric
/// fields, or an out-of-range calendar date). Mirrors the control-plane's
/// uptime back-dating parser: hour/minute/second ranges are not validated here
/// (the caller decides whether an impossible wall-clock is "in the future").
pub fn rfc3339_to_unix_secs(rfc3339: &str) -> Option<i64> {
    let bytes = rfc3339.as_bytes();
    if bytes.len() < 20 || bytes[19] != b'Z' {
        return None;
    }

    let year: i64 = rfc3339[0..4].parse().ok()?;
    let month: u32 = rfc3339[5..7].parse().ok()?;
    let day: u32 = rfc3339[8..10].parse().ok()?;
    let hour: u32 = rfc3339[11..13].parse().ok()?;
    let minute: u32 = rfc3339[14..16].parse().ok()?;
    let second: u32 = rfc3339[17..19].parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    Some(days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64)
}

/// Days since Unix epoch → `(year, month, day)` UTC.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// `(year, month, day)` → days since Unix epoch, or `None` for out-of-range dates.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

#[cfg(test)]
mod tests {
    use super::{rfc3339_from_unix, rfc3339_to_unix_secs};

    #[test]
    fn unix_epoch_formats_as_zulu() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_timestamp_formats() {
        assert_eq!(rfc3339_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn zero_padding_is_byte_stable() {
        // 61s = 00:01:01 — every component must be zero-padded to width.
        assert_eq!(rfc3339_from_unix(61), "1970-01-01T00:01:01Z");
        // Year/month/day padding: 100 days after epoch → 1970-04-11.
        assert_eq!(rfc3339_from_unix(8_640_000), "1970-04-11T00:00:00Z");
        // Every formatted value ends in Z (wire suffix).
        for secs in [0, 1, 59, 61, 3_599, 3_601, 86_399, 86_400, 1_700_000_000] {
            assert!(rfc3339_from_unix(secs).ends_with('Z'));
        }
    }

    #[test]
    fn parses_known_timestamps() {
        assert_eq!(rfc3339_to_unix_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_to_unix_secs("2023-11-14T22:13:20Z"),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn round_trips_both_directions() {
        for secs in [0, 1, 61, 3_601, 86_400, 1_700_000_000, 1_999_999_999] {
            let formatted = rfc3339_from_unix(secs);
            assert_eq!(
                rfc3339_to_unix_secs(&formatted),
                Some(secs as i64),
                "round-trip failed for {secs}"
            );
        }
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "not-a-date",
            "2023-01-01T00:00:00",       // missing Z
            "2023-01-01T00:00:00+00:00", // offset, not Z
            "2023-13-01T00:00:00Z",      // month out of range
            "2023-00-01T00:00:00Z",      // month zero
            "2023-01-32T00:00:00Z",      // day out of range
            "abcd-01-01T00:00:00Z",      // non-numeric year
        ] {
            assert_eq!(rfc3339_to_unix_secs(bad), None, "expected None for {bad:?}");
        }
    }
}
