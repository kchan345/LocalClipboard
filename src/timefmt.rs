//! Small, dependency-free calendar helpers (RFC 3339 timestamps, DOS zip times).

use std::time::{SystemTime, UNIX_EPOCH};

/// Converts days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
/// Algorithm from Howard Hinnant's `civil_from_days`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Splits Unix seconds into calendar fields (UTC).
pub fn fields(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Formats a `SystemTime` as RFC 3339 UTC with millisecond precision.
pub fn rfc3339(t: SystemTime) -> String {
    let ms = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    };
    let (y, mo, d, h, mi, s) = fields(ms.div_euclid(1000));
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        ms.rem_euclid(1000)
    )
}

/// Converts Unix milliseconds to MS-DOS `(time, date)` as used in zip headers.
/// Values outside the representable 1980..=2107 range are clamped.
pub fn dos_datetime(unix_ms: Option<i64>) -> (u16, u16) {
    const MIN: (u16, u16) = (0, (1 << 5) | 1);
    let Some(ms) = unix_ms else { return MIN };
    let (y, mo, d, h, mi, s) = fields(ms.div_euclid(1000));
    if y < 1980 {
        return MIN;
    }
    if y > 2107 {
        return ((23 << 11) | (59 << 5) | 29, (127 << 9) | (12 << 5) | 31);
    }
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | (s as u16 / 2);
    let date = (((y - 1980) as u16) << 9) | ((mo as u16) << 5) | d as u16;
    (time, date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_724), (2026, 9, 28));
    }

    #[test]
    fn rfc3339_format() {
        let t = UNIX_EPOCH + Duration::from_millis(1_790_624_465_123);
        assert_eq!(rfc3339(t), "2026-09-28T19:41:05.123Z");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn dos_times() {
        assert_eq!(dos_datetime(None), (0, 33));
        assert_eq!(dos_datetime(Some(0)), (0, 33));
        // 2026-09-28 19:41:06 UTC
        let (t, d) = dos_datetime(Some(1_790_624_466_000));
        assert_eq!(d >> 9, 46);
        assert_eq!((d >> 5) & 0xf, 9);
        assert_eq!(d & 0x1f, 28);
        assert_eq!(t >> 11, 19);
        assert_eq!((t >> 5) & 0x3f, 41);
        assert_eq!((t & 0x1f) * 2, 6);
    }
}
