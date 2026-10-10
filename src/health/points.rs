//! Google Health data points as rows of `health_points`: the point's JSON
//! whole, with the few fields needed to find it again (when it happened,
//! which local day, where from). Nothing is dropped or summarised here; the
//! per-day aggregates in [`super::normalize`] are separate.
use super::catalog::{self, Filter};
use super::normalize::{day_of, instant, text};
use crate::store::PointRow;
use jiff::tz::TimeZone;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The rows for `points` of `data_type`. A point whose data field is not an
/// object, and a type this build does not know, give no row.
pub fn rows(data_type: &str, points: &[Value], zone: &TimeZone) -> Vec<PointRow> {
    let Some(spec) = catalog::find(data_type) else {
        return Vec::new();
    };
    points
        .iter()
        .filter(|point| point[spec.field].is_object())
        .map(|point| row(data_type, spec.filter, &point[spec.field], point, zone))
        .collect()
}

fn row(data_type: &str, filter: Filter, c: &Value, point: &Value, zone: &TimeZone) -> PointRow {
    let date = day_of(c, filter, zone);
    let (start, end) = if filter == Filter::DailyDate {
        // A daily summary starts at its date's local midnight.
        let midnight = date
            .and_then(|d| d.to_zoned(zone.clone()).ok())
            .map(|z| z.timestamp().as_millisecond());
        (midnight, midnight)
    } else if c["sampleTime"].is_object() {
        let at = instant(&c["sampleTime"]["physicalTime"]).map(|t| t.as_millisecond());
        (at, at)
    } else {
        (
            instant(&c["interval"]["startTime"]).map(|t| t.as_millisecond()),
            instant(&c["interval"]["endTime"]).map(|t| t.as_millisecond()),
        )
    };
    let source = source(point);
    let value = point.to_string();
    let key = text(&point["name"]).unwrap_or_else(|| {
        let mut hash = Sha256::new();
        for part in [
            data_type,
            &start.map(|s| s.to_string()).unwrap_or_default(),
            source.as_deref().unwrap_or_default(),
            &value,
        ] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        format!("sha256:{:x}", hash.finalize())
    });
    PointRow {
        key,
        start_ms: start,
        end_ms: end,
        civil_date: date.map(|d| d.to_string()),
        value,
        source,
    }
}

/// `PLATFORM:device name`, from whatever of the two the point has.
fn source(point: &Value) -> Option<String> {
    let source = &point["dataSource"];
    let parts: Vec<String> = [
        text(&source["platform"]),
        text(&source["device"]["displayName"]),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        text(&source["recordingMethod"])
    } else {
        Some(parts.join(":"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;
    use serde_json::json;

    fn kolkata() -> TimeZone {
        TimeZone::get("Asia/Kolkata").unwrap()
    }

    fn ms(s: &str) -> Option<i64> {
        Some(s.parse::<Timestamp>().unwrap().as_millisecond())
    }

    #[test]
    fn an_interval_point_keeps_its_span_named_by_google() {
        let point = json!({
            "name": "users/me/dataTypes/steps/dataPoints/p1",
            "steps": {
                "interval": {"startTime": "2026-10-09T08:00:00Z", "endTime": "2026-10-09T08:05:00Z"},
                "count": "12"
            },
            "dataSource": {"platform": "FITBIT", "device": {"displayName": "Charge 6"}}
        });
        let rows = rows("steps", std::slice::from_ref(&point), &kolkata());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.key, "users/me/dataTypes/steps/dataPoints/p1");
        assert_eq!(row.start_ms, ms("2026-10-09T08:00:00Z"));
        assert_eq!(row.end_ms, ms("2026-10-09T08:05:00Z"));
        // 08:00 UTC is 13:30 in Kolkata, the same day.
        assert_eq!(row.civil_date.as_deref(), Some("2026-10-09"));
        assert_eq!(row.source.as_deref(), Some("FITBIT:Charge 6"));
        // The whole point is stored, not only its values.
        assert_eq!(row.value, point.to_string());
    }

    #[test]
    fn a_sample_point_starts_and_ends_at_its_instant_and_is_placed_in_the_users_zone() {
        // 20:00 UTC is 01:30 on the 10th in Kolkata.
        let point = json!({
            "name": "w1",
            "weight": {"sampleTime": {"physicalTime": "2026-10-09T20:00:00Z"}, "weightGrams": 80000}
        });
        let rows = rows("weight", &[point], &kolkata());
        assert_eq!(rows[0].start_ms, ms("2026-10-09T20:00:00Z"));
        assert_eq!(rows[0].end_ms, rows[0].start_ms);
        assert_eq!(rows[0].civil_date.as_deref(), Some("2026-10-10"));
        // No dataSource at all: no source.
        assert_eq!(rows[0].source, None);
    }

    #[test]
    fn a_daily_point_starts_at_its_dates_local_midnight() {
        let point = json!({
            "name": "d1",
            "dailyRestingHeartRate": {"date": {"year": 2026, "month": 10, "day": 9}, "beatsPerMinute": "52"}
        });
        let rows = rows("daily-resting-heart-rate", &[point], &kolkata());
        // Midnight in Kolkata is 18:30 UTC on the day before.
        assert_eq!(rows[0].start_ms, ms("2026-10-08T18:30:00Z"));
        assert_eq!(rows[0].end_ms, rows[0].start_ms);
        assert_eq!(rows[0].civil_date.as_deref(), Some("2026-10-09"));
    }

    #[test]
    fn a_sleep_point_is_placed_on_the_day_it_ended() {
        let point = json!({
            "name": "s1",
            "sleep": {"interval": {
                "startTime": "2026-10-09T20:30:00Z",
                "endTime": "2026-10-10T04:30:00Z"}}
        });
        let rows = rows("sleep", &[point], &kolkata());
        assert_eq!(rows[0].start_ms, ms("2026-10-09T20:30:00Z"));
        assert_eq!(rows[0].end_ms, ms("2026-10-10T04:30:00Z"));
        assert_eq!(rows[0].civil_date.as_deref(), Some("2026-10-10"));
    }

    #[test]
    fn the_source_is_platform_and_device_or_else_the_recording_method() {
        let source = |data_source: Value| {
            let point = json!({"name": "p", "steps": {"count": "1"}, "dataSource": data_source});
            rows("steps", &[point], &kolkata()).remove(0).source
        };
        assert_eq!(
            source(json!({"platform": "FITBIT", "device": {"displayName": "Inspire 3"}}))
                .as_deref(),
            Some("FITBIT:Inspire 3")
        );
        assert_eq!(
            source(json!({"platform": "FITBIT"})).as_deref(),
            Some("FITBIT")
        );
        assert_eq!(
            source(json!({"recordingMethod": "MANUAL"})).as_deref(),
            Some("MANUAL")
        );
        assert_eq!(source(json!({"platform": "  "})), None);
    }

    #[test]
    fn a_point_without_a_name_gets_a_stable_hash_of_what_it_says() {
        let p = |count: &str| json!({"steps": {"interval": {"startTime": "2026-10-09T08:00:00Z"}, "count": count}});
        let zone = kolkata();
        let first = rows("steps", &[p("5")], &zone).remove(0).key;
        let again = rows("steps", &[p("5")], &zone).remove(0).key;
        let other = rows("steps", &[p("6")], &zone).remove(0).key;
        assert!(first.starts_with("sha256:"), "{first}");
        assert_eq!(first.len(), "sha256:".len() + 64);
        assert_eq!(first, again);
        assert_ne!(first, other);
    }

    #[test]
    fn a_point_whose_data_is_not_an_object_and_an_unknown_type_give_no_row() {
        let zone = kolkata();
        let not_object = json!({"steps": 5});
        assert!(rows("steps", &[not_object], &zone).is_empty());
        let good = json!({"steps": {"count": "1"}});
        assert!(rows("not-a-type", &[good], &zone).is_empty());
        assert!(rows("steps", &[], &zone).is_empty());
    }
}
