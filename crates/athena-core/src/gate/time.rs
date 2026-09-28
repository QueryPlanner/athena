//! UTC timestamps from Unix seconds, without a date library.

/// `(year, month, day, hour, minute, second)` in UTC.
fn civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil_from_days, for days since 1970-01-01 >= 0.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day, rem / 3_600, rem % 3_600 / 60, rem % 60)
}

/// `2026-09-25T12:00:00Z`, for `state.json`.
pub fn rfc3339(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `20260925T120000Z`, for backup file names: sorts by time as text.
pub fn compact(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_as_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_790_337_600), "2026-09-25T12:00:00Z");
        assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
        assert_eq!(rfc3339(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(compact(1_790_337_600 + 3_723), "20260925T130203Z");
    }
}
