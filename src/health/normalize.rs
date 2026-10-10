//! Google Health data points, folded into one record per local day.
//!
//! A point is placed on a day by its civil time when Google gives one, else
//! by its instant converted to the user's time zone. Missing values stay
//! missing: a day has a key only for what Google reported. A sync feeds each
//! page of points through [`Days::add`] and keeps only the totals, so memory
//! holds one page at a time.
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The data types a sync reads, with the field of each point that holds its
/// values.
pub const TYPES: [(&str, &str); 12] = [
    ("steps", "steps"),
    ("distance", "distance"),
    ("active-energy-burned", "activeEnergyBurned"),
    ("active-minutes", "activeMinutes"),
    ("active-zone-minutes", "activeZoneMinutes"),
    ("exercise", "exercise"),
    ("sleep", "sleep"),
    ("daily-resting-heart-rate", "dailyRestingHeartRate"),
    ("daily-heart-rate-zones", "dailyHeartRateZones"),
    ("time-in-heart-rate-zone", "timeInHeartRateZone"),
    ("weight", "weight"),
    ("body-fat", "bodyFat"),
];

/// A number in JSON, or in a string (Google writes 64-bit integers as
/// strings). Not a boolean, not infinite.
fn num(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|n| n.is_finite())
}

fn whole(value: &Value) -> Option<i64> {
    num(value).map(|n| n.max(0.0) as i64)
}

fn round(n: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (n * scale).round() / scale
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A civil date given as `{year, month, day}`, or holding one in `date`.
fn civil(value: &Value) -> Option<Date> {
    let parts = |v: &Value| {
        let n = |key| num(&v[key]).map(|n| n as i16);
        Date::new(n("year")?, n("month")? as i8, n("day")? as i8).ok()
    };
    parts(value).or_else(|| parts(&value["date"]))
}

fn instant(value: &Value) -> Option<Timestamp> {
    value.as_str()?.parse().ok()
}

/// The local date `value` names: a civil date object, an instant in the
/// user's zone, or a plain `YYYY-MM-DD`.
fn date_of(value: &Value, zone: &TimeZone) -> Option<Date> {
    civil(value).or_else(|| {
        let s = value.as_str()?;
        match s.parse::<Timestamp>() {
            Ok(at) => Some(at.to_zoned(zone.clone()).date()),
            Err(_) => s.parse().ok(),
        }
    })
}

fn interval_seconds(interval: &Value) -> Option<f64> {
    let (start, end) = (
        instant(&interval["startTime"])?,
        instant(&interval["endTime"])?,
    );
    let seconds = end.as_second() - start.as_second();
    (seconds >= 0).then_some(seconds as f64)
}

/// `"3600.5s"` as seconds.
fn duration_seconds(value: &Value) -> Option<f64> {
    let seconds: f64 = value.as_str()?.strip_suffix('s')?.parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds)
}

/// The day a point belongs to: sleep goes on the day it ended.
fn day_of(component: &Value, data_type: &str, zone: &TimeZone) -> Option<Date> {
    let daily = matches!(
        data_type,
        "daily-resting-heart-rate" | "daily-heart-rate-zones"
    );
    if daily && let Some(date) = date_of(&component["date"], zone) {
        return Some(date);
    }
    let interval = &component["interval"];
    let order = if data_type == "sleep" {
        ["civilEndTime", "endTime", "civilStartTime", "startTime"]
    } else {
        ["civilStartTime", "startTime", "civilEndTime", "endTime"]
    };
    order
        .iter()
        .find_map(|key| date_of(&interval[*key], zone))
        .or_else(|| date_of(&component["sampleTime"]["civilTime"], zone))
        .or_else(|| date_of(&component["sampleTime"]["physicalTime"], zone))
}

#[derive(Default)]
struct Day {
    steps: Option<i64>,
    distance_m: Option<f64>,
    active_kcal: Option<f64>,
    active_min: Option<i64>,
    zone_min: Option<i64>,
    resting_hr: Option<i64>,
    zone_minutes: BTreeMap<String, i64>,
    zones: Vec<Value>,
    weight: Option<(String, f64)>,
    body_fat: Option<(String, f64)>,
    workouts: Vec<Value>,
    sleep: Vec<Value>,
    /// The points already counted, so a repeated page or point is not added
    /// twice.
    seen: BTreeSet<String>,
}

impl Day {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                m.insert(key.into(), value);
            }
        };
        put("steps", self.steps.map(Value::from));
        put("distance_m", self.distance_m.map(|n| json!(round(n, 1))));
        put("active_kcal", self.active_kcal.map(|n| json!(round(n, 1))));
        put("active_min", self.active_min.map(Value::from));
        put("zone_min", self.zone_min.map(Value::from));
        put("resting_hr", self.resting_hr.map(Value::from));
        put(
            "hr_zone_minutes",
            (!self.zone_minutes.is_empty()).then(|| json!(self.zone_minutes)),
        );
        put(
            "hr_zones",
            (!self.zones.is_empty()).then(|| json!(self.zones)),
        );
        put(
            "weight_kg",
            self.weight.as_ref().map(|w| json!(round(w.1, 2))),
        );
        put(
            "body_fat_pct",
            self.body_fat.as_ref().map(|w| json!(round(w.1, 1))),
        );
        put(
            "workouts",
            (!self.workouts.is_empty()).then(|| json!(self.workouts)),
        );
        put("sleep", (!self.sleep.is_empty()).then(|| json!(self.sleep)));
        Value::Object(m)
    }

    fn add(&mut self, data_type: &str, c: &Value, point: &Value) {
        match data_type {
            "steps" => add(&mut self.steps, whole(&c["count"])),
            "distance" => {
                let meters = num(&c["millimeters"]).map(|n| n.max(0.0) / 1000.0);
                add_f(&mut self.distance_m, meters);
            }
            "active-energy-burned" => {
                add_f(&mut self.active_kcal, num(&c["kcal"]).map(|n| n.max(0.0)))
            }
            "active-minutes" => {
                let levels = c["activeMinutesByActivityLevel"].as_array();
                let total =
                    levels.map(|l| l.iter().filter_map(|r| whole(&r["activeMinutes"])).sum());
                add(&mut self.active_min, total);
            }
            "active-zone-minutes" => add(&mut self.zone_min, whole(&c["activeZoneMinutes"])),
            "time-in-heart-rate-zone" => {
                let minutes = interval_seconds(&c["interval"]).map(|s| (s / 60.0).round() as i64);
                if let (Some(zone), Some(minutes)) = (text(&c["heartRateZoneType"]), minutes) {
                    *self.zone_minutes.entry(zone).or_default() += minutes;
                }
            }
            "daily-resting-heart-rate" => {
                if let Some(bpm) = whole(&c["beatsPerMinute"]) {
                    self.resting_hr = Some(bpm);
                }
            }
            "daily-heart-rate-zones" => self.thresholds(c),
            "exercise" => self.exercise(c, point),
            "sleep" => self.sleep(c, point),
            "weight" => latest(
                &mut self.weight,
                c,
                point,
                num(&c["weightGrams"]).map(|g| g / 1000.0),
            ),
            _ => latest(&mut self.body_fat, c, point, num(&c["percentage"])),
        }
    }

    /// The day's heart-rate zone limits: the latest set Google sent.
    fn thresholds(&mut self, c: &Value) {
        let zones: Vec<Value> = c["heartRateZones"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|z| {
                Some(json!({
                    "type": text(&z["heartRateZoneType"])?,
                    "min_bpm": whole(&z["minBeatsPerMinute"])?,
                    "max_bpm": whole(&z["maxBeatsPerMinute"])?,
                }))
            })
            .collect();
        if !zones.is_empty() {
            self.zones = zones;
        }
    }

    fn first_time(&mut self, point: &Value) -> bool {
        let key = text(&point["name"]).unwrap_or_else(|| point.to_string());
        self.seen.insert(key)
    }

    fn exercise(&mut self, c: &Value, point: &Value) {
        let seconds =
            duration_seconds(&c["activeDuration"]).or_else(|| interval_seconds(&c["interval"]));
        let Some(seconds) = seconds else { return };
        if !self.first_time(point) {
            return;
        }
        let kind = text(&c["displayName"])
            .or_else(|| text(&c["exerciseType"]))
            .unwrap_or_else(|| "Workout".into());
        let summary = &c["metricsSummary"];
        let mut workout = Map::new();
        workout.insert("type".into(), kind.into());
        workout.insert("minutes".into(), ((seconds / 60.0).round() as i64).into());
        if let Some(kcal) = num(&summary["caloriesKcal"]) {
            workout.insert("kcal".into(), round(kcal.max(0.0), 1).into());
        }
        if let Some(zone) = whole(&summary["activeZoneMinutes"]) {
            workout.insert("zone_min".into(), zone.into());
        }
        self.workouts.push(Value::Object(workout));
    }

    fn sleep(&mut self, c: &Value, point: &Value) {
        let summary = &c["summary"];
        let minutes = whole(&summary["minutesAsleep"])
            .or_else(|| interval_seconds(&c["interval"]).map(|s| (s / 60.0).round() as i64));
        let Some(minutes) = minutes else { return };
        if !self.first_time(point) {
            return;
        }
        let stages: Vec<Value> = summary["stagesSummary"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                Some(json!({"type": text(&s["type"])?, "minutes": whole(&s["minutes"])?}))
            })
            .collect();
        self.sleep.push(json!({
            "minutes": minutes,
            "start": text(&c["interval"]["startTime"]),
            "end": text(&c["interval"]["endTime"]),
            "stages": stages,
        }));
    }
}

fn add(total: &mut Option<i64>, value: Option<i64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0) + value);
    }
}

fn add_f(total: &mut Option<f64>, value: Option<f64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0.0) + value);
    }
}

/// Keep the measurement with the latest time.
fn latest(slot: &mut Option<(String, f64)>, c: &Value, point: &Value, value: Option<f64>) {
    let Some(value) = value.map(|v| v.max(0.0)) else {
        return;
    };
    let when = text(&c["sampleTime"]["physicalTime"])
        .or_else(|| text(&point["name"]))
        .unwrap_or_default();
    if slot.as_ref().is_none_or(|(before, _)| when >= *before) {
        *slot = Some((when, value));
    }
}

/// The days found so far, in a user's time zone.
pub struct Days {
    zone: TimeZone,
    days: BTreeMap<Date, Day>,
}

impl Days {
    pub fn new(zone: TimeZone) -> Self {
        Self {
            zone,
            days: BTreeMap::new(),
        }
    }

    /// Fold `points` of `data_type` in. Points without a usable value or
    /// date, and types this build does not know, are skipped.
    pub fn add(&mut self, data_type: &str, points: &[Value]) {
        let Some((_, field)) = TYPES.iter().find(|(name, _)| *name == data_type) else {
            return;
        };
        for point in points {
            let component = &point[*field];
            if !component.is_object() {
                continue;
            }
            if let Some(date) = day_of(component, data_type, &self.zone) {
                self.days
                    .entry(date)
                    .or_default()
                    .add(data_type, component, point);
            }
        }
    }

    /// The days from `start` up to but not including `end`, as (date,
    /// metrics JSON). A day with nothing in it is left out.
    pub fn finish(self, start: Date, end: Date) -> Vec<(String, String)> {
        self.days
            .into_iter()
            .filter(|(date, _)| *date >= start && *date < end)
            .map(|(date, day)| (date, day.to_json()))
            .filter(|(_, metrics)| metrics.as_object().is_some_and(|m| !m.is_empty()))
            .map(|(date, metrics)| (date.to_string(), metrics.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paris() -> TimeZone {
        TimeZone::get("Europe/Paris").unwrap()
    }

    fn day(date: &str) -> Date {
        date.parse().unwrap()
    }

    fn run(types: &[(&str, Value)]) -> Vec<(String, Value)> {
        let mut days = Days::new(paris());
        for (data_type, points) in types {
            days.add(data_type, points.as_array().unwrap());
        }
        days.finish(day("2026-10-01"), day("2026-10-20"))
            .into_iter()
            .map(|(d, m)| (d, serde_json::from_str(&m).unwrap()))
            .collect()
    }

    fn civil_time(date: &str) -> Value {
        let d = day(date);
        json!({"date": {"year": d.year(), "month": d.month(), "day": d.day()},
               "time": {"hours": 8}})
    }

    #[test]
    fn numbers_may_be_strings_but_not_booleans_or_infinite() {
        assert_eq!(num(&json!(3)), Some(3.0));
        assert_eq!(num(&json!(" 4.5 ")), Some(4.5));
        assert_eq!(num(&json!("x")), None);
        assert_eq!(num(&json!(true)), None);
        assert_eq!(num(&json!("inf")), None);
        assert_eq!(num(&json!(null)), None);
        assert_eq!(whole(&json!(-3)), Some(0));
    }

    #[test]
    fn dates_come_from_civil_objects_instants_or_plain_dates() {
        let zone = paris();
        let d = |v: Value| date_of(&v, &zone);
        assert_eq!(
            d(json!({"year": 2026, "month": 10, "day": 9})),
            Some(day("2026-10-09"))
        );
        assert_eq!(d(civil_time("2026-10-09")), Some(day("2026-10-09")));
        // 23:30 UTC is already the next day in Paris.
        assert_eq!(d(json!("2026-10-09T23:30:00Z")), Some(day("2026-10-10")));
        assert_eq!(d(json!("2026-10-09")), Some(day("2026-10-09")));
        assert_eq!(d(json!({"year": 2026, "month": 13, "day": 1})), None);
        assert_eq!(d(json!("yesterday")), None);
        assert_eq!(d(json!(5)), None);
    }

    #[test]
    fn intervals_and_durations_must_be_well_formed() {
        let ok = json!({"startTime": "2026-10-09T10:00:00Z", "endTime": "2026-10-09T10:30:00Z"});
        assert_eq!(interval_seconds(&ok), Some(1800.0));
        let backwards =
            json!({"startTime": "2026-10-09T10:30:00Z", "endTime": "2026-10-09T10:00:00Z"});
        assert_eq!(interval_seconds(&backwards), None);
        assert_eq!(
            interval_seconds(&json!({"startTime": "2026-10-09T10:00:00Z"})),
            None
        );
        assert_eq!(duration_seconds(&json!("90.5s")), Some(90.5));
        for bad in [json!("90"), json!("-5s"), json!("xs"), json!(90)] {
            assert_eq!(duration_seconds(&bad), None, "{bad}");
        }
    }

    #[test]
    fn steps_distance_energy_and_minutes_add_up_within_a_local_day() {
        let at = |t: &str, field: &str, v: Value| json!({"interval": {"startTime": t, "endTime": t}, field: v});
        let rows = run(&[
            (
                "steps",
                json!([
                    {"steps": at("2026-10-09T08:00:00Z", "count", json!("1200"))},
                    {"steps": at("2026-10-09T09:00:00Z", "count", json!(800))},
                    // 22:30 UTC is the 10th in Paris.
                    {"steps": at("2026-10-09T22:30:00Z", "count", json!(50))},
                    {"steps": at("2026-10-09T09:00:00Z", "count", json!("n/a"))},
                    {"steps": "not an object"},
                    {"steps": {"count": 5}},
                ]),
            ),
            (
                "distance",
                json!([{"distance": at("2026-10-09T08:00:00Z", "millimeters", json!("2500000"))}]),
            ),
            (
                "active-energy-burned",
                json!([{"activeEnergyBurned": at("2026-10-09T08:00:00Z", "kcal", json!(210.55))}]),
            ),
            (
                "active-minutes",
                json!([{"activeMinutes": {
                "interval": {"startTime": "2026-10-09T08:00:00Z"},
                "activeMinutesByActivityLevel": [{"activeMinutes": "20"}, {"activeMinutes": 15}, {"x": 1}]}}]),
            ),
            (
                "active-zone-minutes",
                json!([{"activeZoneMinutes": at("2026-10-09T08:00:00Z", "activeZoneMinutes", json!(33))}]),
            ),
            ("unknown-type", json!([{"x": 1}])),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "2026-10-09");
        assert_eq!(
            rows[0].1,
            json!({
            "steps": 2000, "distance_m": 2500.0, "active_kcal": 210.6,
            "active_min": 35, "zone_min": 33})
        );
        assert_eq!(rows[1], ("2026-10-10".into(), json!({"steps": 50})));
    }

    #[test]
    fn a_civil_start_time_wins_over_the_instant() {
        let point = json!({"steps": {
            "interval": {"civilStartTime": civil_time("2026-10-07"),
                         "startTime": "2026-10-09T08:00:00Z"},
            "count": 10}});
        assert_eq!(run(&[("steps", json!([point]))])[0].0, "2026-10-07");
    }

    #[test]
    fn heart_rate_values_zones_and_time_in_zone() {
        let rows = run(&[
            (
                "daily-resting-heart-rate",
                json!([
                    {"dailyRestingHeartRate": {"date": {"year": 2026, "month": 10, "day": 9}, "beatsPerMinute": "52"}},
                    {"dailyRestingHeartRate": {"date": "2026-10-09", "beatsPerMinute": 51}},
                    {"dailyRestingHeartRate": {"date": "2026-10-08", "beatsPerMinute": "x"}},
                ]),
            ),
            (
                "daily-heart-rate-zones",
                json!([
                    {"dailyHeartRateZones": {"date": "2026-10-09", "heartRateZones": [
                        {"heartRateZoneType": "FAT_BURN", "minBeatsPerMinute": 100, "maxBeatsPerMinute": 120},
                        {"heartRateZoneType": "CARDIO", "minBeatsPerMinute": 120},
                        "junk"]}},
                    {"dailyHeartRateZones": {"date": "2026-10-09", "heartRateZones": []}},
                ]),
            ),
            (
                "time-in-heart-rate-zone",
                json!([
                    {"timeInHeartRateZone": {"heartRateZoneType": "CARDIO", "interval": {
                        "startTime": "2026-10-09T08:00:00Z", "endTime": "2026-10-09T08:12:00Z"}}},
                    {"timeInHeartRateZone": {"heartRateZoneType": "CARDIO", "interval": {
                        "startTime": "2026-10-09T09:00:00Z", "endTime": "2026-10-09T09:03:00Z"}}},
                    {"timeInHeartRateZone": {"interval": {
                        "startTime": "2026-10-09T09:00:00Z", "endTime": "2026-10-09T09:03:00Z"}}},
                ]),
            ),
        ]);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0].1,
            json!({
            "resting_hr": 51,
            "hr_zone_minutes": {"CARDIO": 15},
            "hr_zones": [{"type": "FAT_BURN", "min_bpm": 100, "max_bpm": 120}]})
        );
    }

    #[test]
    fn workouts_are_listed_once_with_their_numbers() {
        let long = json!({"exercise": {
            "interval": {"startTime": "2026-10-09T08:00:00Z", "endTime": "2026-10-09T09:00:00Z"},
            "activeDuration": "2700s", "displayName": " Run ",
            "metricsSummary": {"caloriesKcal": "410.25", "activeZoneMinutes": 40}}});
        let rows = run(&[(
            "exercise",
            json!([
                long, long,
                {"name": "w2", "exercise": {"interval": {"startTime": "2026-10-09T10:00:00Z", "endTime": "2026-10-09T10:20:00Z"},
                    "exerciseType": "WALKING"}},
                {"name": "w3", "exercise": {"interval": {"startTime": "2026-10-09T11:00:00Z", "endTime": "2026-10-09T11:10:00Z"}}},
                {"name": "w4", "exercise": {"interval": {"startTime": "2026-10-09T11:00:00Z"}}},
            ]),
        )]);
        assert_eq!(
            rows[0].1["workouts"],
            json!([
                {"type": "Run", "minutes": 45, "kcal": 410.3, "zone_min": 40},
                {"type": "WALKING", "minutes": 20},
                {"type": "Workout", "minutes": 10},
            ])
        );
    }

    #[test]
    fn sleep_lands_on_the_day_it_ended_and_is_counted_once() {
        let night = json!({"name": "s1", "sleep": {
            "interval": {"startTime": "2026-10-09T20:30:00Z", "endTime": "2026-10-10T04:30:00Z"},
            "summary": {"minutesAsleep": "450", "stagesSummary": [
                {"type": "DEEP", "minutes": 80}, {"type": "REM"}, {"minutes": 3}]}}});
        let nap = json!({"name": "s2", "sleep": {
            "interval": {"startTime": "2026-10-10T11:00:00Z", "endTime": "2026-10-10T11:30:00Z"}}});
        let rows = run(&[("sleep", json!([night, night, nap, {"sleep": {}}]))]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "2026-10-10");
        assert_eq!(
            rows[0].1["sleep"],
            json!([
                {"minutes": 450, "start": "2026-10-09T20:30:00Z", "end": "2026-10-10T04:30:00Z",
                 "stages": [{"type": "DEEP", "minutes": 80}]},
                {"minutes": 30, "start": "2026-10-10T11:00:00Z", "end": "2026-10-10T11:30:00Z", "stages": []},
            ])
        );
    }

    #[test]
    fn the_latest_weight_and_body_fat_of_a_day_win() {
        let weight = |t: &str, g: Value| json!({"weight": {"sampleTime": {"physicalTime": t}, "weightGrams": g}});
        let fat = |name: &str, p: Value| json!({"name": name, "bodyFat": {"sampleTime": {"civilTime": civil_time("2026-10-09")}, "percentage": p}});
        let rows = run(&[
            (
                "weight",
                json!([
                    weight("2026-10-09T09:00:00Z", json!(80500)),
                    weight("2026-10-09T07:00:00Z", json!(81000)),
                    weight("2026-10-09T08:00:00Z", json!("bad")),
                ]),
            ),
            // Without a physical time the point's name orders them.
            (
                "body-fat",
                json!([fat("b", json!(18.26)), fat("a", json!(19.0))]),
            ),
        ]);
        assert_eq!(rows[0].1, json!({"weight_kg": 80.5, "body_fat_pct": 18.3}));
    }

    #[test]
    fn days_outside_the_window_or_without_values_are_dropped() {
        let steps = |t: &str, n: i64| json!({"steps": {"interval": {"startTime": t}, "count": n}});
        let mut days = Days::new(paris());
        days.add(
            "steps",
            &[
                steps("2026-09-30T12:00:00Z", 1),
                steps("2026-10-05T12:00:00Z", 2),
                steps("2026-10-06T12:00:00Z", 3),
            ],
        );
        days.add("exercise", &[json!({"exercise": {}})]);
        days.add(
            "steps",
            &[json!({"steps": {"interval": {"startTime": "2026-10-04T12:00:00Z"}, "count": "x"}})],
        );
        let kept = days.finish(day("2026-10-05"), day("2026-10-06"));
        assert_eq!(
            kept,
            [("2026-10-05".to_string(), r#"{"steps":2}"#.to_string())]
        );
    }
}
