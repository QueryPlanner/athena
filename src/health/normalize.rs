//! Google Health data points, folded into one record per local day.
//!
//! A point is placed on a day by its civil time when Google gives one, else
//! by its instant converted to the user's time zone. Missing values stay
//! missing: a day has a key only for what Google reported. A sync feeds each
//! page of points through [`Days::add`] and keeps only running aggregates,
//! so memory holds one page at a time and a day's record stays small:
//!
//! - interval counts are summed (`steps`, `floors`, `distance_m`, ...);
//! - samples keep `{min, avg, max, n}` (`heart_rate`, `hrv_ms`, ...);
//! - body measurements keep the latest (`weight_kg`, `height_cm`, ...);
//! - daily summaries are kept as Google computed them (`hrv_daily`, ...);
//! - sessions keep a short summary each, with a cap per day (`workouts`,
//!   `sleep`);
//! - ECG, irregular rhythm, mood, symptom, ovulation and period records keep
//!   a count and labels only. No waveform, heart-beat series or
//!   minute-level series goes into a day's record; the raw points, ECG
//!   waveforms included, are kept whole in `health_points` (see
//!   [`super::points`]).
use super::catalog::{self, Filter};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub use super::catalog::TYPES;

/// The most workouts kept for one day; later ones are counted in
/// `workouts_omitted`.
const MAX_WORKOUTS: usize = 8;
/// The most sleep sessions (a night and naps) kept for one day.
const MAX_SLEEPS: usize = 4;
/// The most labels kept in one tally (moods, symptoms, ECG classes).
const MAX_LABELS: usize = 12;
/// The most bytes of JSON stored for one day.
pub const MAX_DAY_BYTES: usize = 4 * 1024;
/// What is dropped, first to last, from a day over [`MAX_DAY_BYTES`]: the
/// least useful to the model first.
const DROP_ORDER: [&str; 9] = [
    "hr_zones",
    "activity_level_min",
    "hr_zone_minutes",
    "moods",
    "symptoms",
    "ovulation_tests",
    "swim",
    "workouts",
    "sleep",
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

pub(super) fn text(value: &Value) -> Option<String> {
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

pub(super) fn instant(value: &Value) -> Option<Timestamp> {
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
pub(super) fn day_of(component: &Value, filter: Filter, zone: &TimeZone) -> Option<Date> {
    if filter == Filter::DailyDate
        && let Some(date) = date_of(&component["date"], zone)
    {
        return Some(date);
    }
    let interval = &component["interval"];
    let order = if filter == Filter::SleepEnd {
        ["civilEndTime", "endTime", "civilStartTime", "startTime"]
    } else if filter == Filter::EcgStart {
        // Old ECGs have no UTC offset, so their civil time is UTC's.
        ["startTime", "civilStartTime", "endTime", "civilEndTime"]
    } else {
        ["civilStartTime", "startTime", "civilEndTime", "endTime"]
    };
    order
        .iter()
        .find_map(|key| date_of(&interval[*key], zone))
        .or_else(|| date_of(&component["sampleTime"]["civilTime"], zone))
        .or_else(|| date_of(&component["sampleTime"]["physicalTime"], zone))
}

/// Count, sum and range of the samples of one day.
struct Stat {
    n: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Stat {
    fn new(first: f64) -> Self {
        Self {
            n: 1,
            sum: first,
            min: first,
            max: first,
        }
    }

    fn push(&mut self, value: f64) {
        self.n += 1;
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    fn to_json(&self) -> Value {
        json!({
            "min": round(self.min, 1),
            "avg": round(self.sum / self.n as f64, 1),
            "max": round(self.max, 1),
            "n": self.n,
        })
    }
}

/// Sums that are whole numbers: counts of steps, minutes, events.
fn is_whole(key: &str) -> bool {
    matches!(
        key,
        "steps"
            | "floors"
            | "active_min"
            | "zone_min"
            | "sedentary_min"
            | "swim.lengths"
            | "swim.strokes"
            | "nutrition.entries"
            | "menstrual_period_started"
            | "ecg.count"
            | "irn.count"
            | "moods.count"
            | "symptoms.count"
    ) || key.starts_with("activity_level_min.")
}

/// Put `value` at `key`, where a dot goes one object deeper.
fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    match key.split_once('.') {
        Some((head, rest)) => {
            let child = map.entry(head).or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(inner) = child {
                put(inner, rest, value);
            }
        }
        None => {
            map.insert(key.into(), value);
        }
    }
}

#[derive(Default)]
struct Day {
    /// Totals by key; see [`is_whole`] for the integers.
    sums: BTreeMap<String, f64>,
    stats: BTreeMap<&'static str, Stat>,
    /// Body measurements: the latest, with the time it was taken.
    latest: BTreeMap<&'static str, (String, f64)>,
    /// Daily summaries as Google computed them; the last one wins.
    scalars: BTreeMap<&'static str, Value>,
    /// How many times each label was seen.
    tallies: BTreeMap<&'static str, BTreeMap<String, u64>>,
    zone_minutes: BTreeMap<String, i64>,
    zones: Vec<Value>,
    workouts: Vec<Value>,
    workouts_omitted: u32,
    sleep: Vec<Value>,
    sleeps_omitted: u32,
    /// The points already counted, so a repeated page or point is not added
    /// twice.
    seen: BTreeSet<String>,
}

impl Day {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        for (key, total) in &self.sums {
            let value = if is_whole(key) {
                json!(total.round() as i64)
            } else {
                json!(round(*total, 1))
            };
            put(&mut m, key, value);
        }
        for (key, stat) in &self.stats {
            put(&mut m, key, stat.to_json());
        }
        for (key, (_, value)) in &self.latest {
            let places = if *key == "weight_kg" { 2 } else { 1 };
            put(&mut m, key, json!(round(*value, places)));
        }
        for (key, value) in &self.scalars {
            put(&mut m, key, value.clone());
        }
        for (key, labels) in &self.tallies {
            put(&mut m, key, json!(top_labels(labels)));
        }
        if !self.zone_minutes.is_empty() {
            m.insert("hr_zone_minutes".into(), json!(self.zone_minutes));
        }
        if !self.zones.is_empty() {
            m.insert("hr_zones".into(), json!(self.zones));
        }
        if !self.workouts.is_empty() {
            m.insert("workouts".into(), json!(self.workouts));
        }
        if self.workouts_omitted > 0 {
            m.insert("workouts_omitted".into(), self.workouts_omitted.into());
        }
        if !self.sleep.is_empty() {
            m.insert("sleep".into(), json!(self.sleep));
        }
        if self.sleeps_omitted > 0 {
            m.insert("sleeps_omitted".into(), self.sleeps_omitted.into());
        }
        shrink(&mut m);
        Value::Object(m)
    }

    fn sum(&mut self, key: &str, value: Option<f64>) {
        if let Some(value) = value {
            *self.sums.entry(key.into()).or_default() += value;
        }
    }

    fn stat(&mut self, key: &'static str, value: Option<f64>) {
        let Some(value) = value else { return };
        match self.stats.get_mut(key) {
            Some(stat) => stat.push(value),
            None => {
                self.stats.insert(key, Stat::new(value));
            }
        }
    }

    /// Keep the measurement with the latest time.
    fn latest(&mut self, key: &'static str, c: &Value, point: &Value, value: Option<f64>) {
        let Some(value) = value.map(|v| v.max(0.0)) else {
            return;
        };
        let when = text(&c["sampleTime"]["physicalTime"])
            .or_else(|| text(&point["name"]))
            .unwrap_or_default();
        if self
            .latest
            .get(key)
            .is_none_or(|(before, _)| when >= *before)
        {
            self.latest.insert(key, (when, value));
        }
    }

    fn tally(&mut self, key: &'static str, label: Option<String>) {
        if let Some(label) = label {
            *self
                .tallies
                .entry(key)
                .or_default()
                .entry(label)
                .or_default() += 1;
        }
    }

    /// Keep a summary object of the numbers that are present, rounded to two
    /// places; nothing if none is.
    fn object(&mut self, key: &'static str, fields: &[(&str, Option<f64>)]) {
        let object: Map<String, Value> = fields
            .iter()
            .filter_map(|(name, value)| Some(((*name).to_string(), json!(round((*value)?, 2)))))
            .collect();
        if !object.is_empty() {
            self.scalars.insert(key, Value::Object(object));
        }
    }

    fn add(&mut self, data_type: &str, c: &Value, point: &Value) {
        let milli = |v: &Value| num(v).map(|n| n.max(0.0) / 1000.0);
        let kcal = |v: &Value| num(v).map(|n| n.max(0.0));
        match data_type {
            "steps" => self.sum("steps", whole(&c["count"]).map(|n| n as f64)),
            "floors" => self.sum("floors", whole(&c["count"]).map(|n| n as f64)),
            "distance" => self.sum("distance_m", milli(&c["millimeters"])),
            // Google's value is a delta that may be negative: the day's sum is
            // the net change, not the climb.
            "altitude" => self.sum(
                "elevation_change_m",
                num(&c["gainMillimeters"]).map(|n| n / 1000.0),
            ),
            "active-energy-burned" => self.sum("active_kcal", kcal(&c["kcal"])),
            "basal-energy-burned" => self.sum("basal_kcal", kcal(&c["kcal"])),
            "active-minutes" => {
                let levels = c["activeMinutesByActivityLevel"].as_array();
                let total = levels.map(|l| {
                    l.iter()
                        .filter_map(|r| whole(&r["activeMinutes"]))
                        .sum::<i64>() as f64
                });
                self.sum("active_min", total);
            }
            "active-zone-minutes" => {
                self.sum("zone_min", whole(&c["activeZoneMinutes"]).map(|n| n as f64))
            }
            "activity-level" => {
                let minutes = interval_seconds(&c["interval"]).map(|s| s / 60.0);
                if let Some(kind) = label_of(&c["activityLevelType"]) {
                    self.sum(&format!("activity_level_min.{kind}"), minutes);
                }
            }
            "sedentary-period" => self.sum(
                "sedentary_min",
                interval_seconds(&c["interval"]).map(|s| s / 60.0),
            ),
            "swim-lengths-data" => {
                if self.first_time(point) {
                    self.sum("swim.lengths", Some(1.0));
                    self.sum("swim.strokes", whole(&c["strokeCount"]).map(|n| n as f64));
                }
            }
            "time-in-heart-rate-zone" => {
                let minutes = interval_seconds(&c["interval"]).map(|s| (s / 60.0).round() as i64);
                if let (Some(zone), Some(minutes)) = (label_of(&c["heartRateZoneType"]), minutes) {
                    *self.zone_minutes.entry(zone).or_default() += minutes;
                }
            }
            "hydration-log" => {
                if self.first_time(point) {
                    self.sum("hydration_ml", kcal(&c["amountConsumed"]["milliliters"]));
                }
            }
            "nutrition-log" => self.nutrition(c, point),
            "heart-rate" => self.stat("heart_rate", num(&c["beatsPerMinute"]).filter(|n| *n > 0.0)),
            "heart-rate-variability" => self.stat(
                "hrv_ms",
                num(&c["rootMeanSquareOfSuccessiveDifferencesMilliseconds"]),
            ),
            "oxygen-saturation" => self.stat("spo2_pct", num(&c["percentage"])),
            "respiratory-rate-sleep-summary" => self.stat(
                "resp_rate_sleep",
                num(&c["fullSleepStats"]["breathsPerMinute"]),
            ),
            "core-body-temperature" => self.stat("core_temp_c", num(&c["temperatureCelsius"])),
            "blood-glucose" => self.stat(
                "glucose_mgdl",
                num(&c["bloodGlucoseMilligramsPerDeciliter"]),
            ),
            "weight" => self.latest(
                "weight_kg",
                c,
                point,
                num(&c["weightGrams"]).map(|g| g / 1000.0),
            ),
            "body-fat" => self.latest("body_fat_pct", c, point, num(&c["percentage"])),
            "height" => self.latest(
                "height_cm",
                c,
                point,
                num(&c["heightMillimeters"]).map(|m| m / 10.0),
            ),
            "vo2-max" => self.latest("vo2max", c, point, num(&c["vo2Max"])),
            "run-vo2-max" => self.latest("run_vo2max", c, point, num(&c["runVo2Max"])),
            "daily-resting-heart-rate" => {
                if let Some(bpm) = whole(&c["beatsPerMinute"]) {
                    self.scalars.insert("resting_hr", bpm.into());
                }
            }
            "daily-heart-rate-variability" => self.object(
                "hrv_daily",
                &[
                    ("avg_ms", num(&c["averageHeartRateVariabilityMilliseconds"])),
                    (
                        "deep_rmssd_ms",
                        num(&c["deepSleepRootMeanSquareOfSuccessiveDifferencesMilliseconds"]),
                    ),
                    ("non_rem_hr", num(&c["nonRemHeartRateBeatsPerMinute"])),
                    ("entropy", num(&c["entropy"])),
                ],
            ),
            "daily-oxygen-saturation" => self.object(
                "spo2_daily",
                &[
                    ("avg", num(&c["averagePercentage"])),
                    ("min", num(&c["lowerBoundPercentage"])),
                    ("max", num(&c["upperBoundPercentage"])),
                ],
            ),
            "daily-respiratory-rate" => {
                if let Some(rate) = num(&c["breathsPerMinute"]) {
                    self.scalars
                        .insert("resp_rate_daily", json!(round(rate, 1)));
                }
            }
            "daily-sleep-temperature-derivations" => self.object(
                "sleep_temp",
                &[
                    ("nightly_c", num(&c["nightlyTemperatureCelsius"])),
                    ("baseline_c", num(&c["baselineTemperatureCelsius"])),
                    (
                        "rel_stddev_30d_c",
                        num(&c["relativeNightlyStddev30dCelsius"]),
                    ),
                ],
            ),
            "daily-vo2-max" => self.daily_vo2_max(c),
            "daily-heart-rate-zones" => self.thresholds(c),
            "exercise" => self.exercise(c, point),
            "sleep" => self.sleep(c, point),
            "electrocardiogram" => {
                if self.first_time(point) {
                    self.sum("ecg.count", Some(1.0));
                    self.tally("ecg.classes", label_of(&c["resultClassification"]));
                }
            }
            "irregular-rhythm-notification" => {
                if self.first_time(point) {
                    self.sum("irn.count", Some(1.0));
                }
            }
            "moods" => self.labelled(("moods.count", "moods.labels"), c, "moods", point),
            "symptoms" => {
                self.labelled(("symptoms.count", "symptoms.labels"), c, "symptoms", point)
            }
            "ovulation-test" => {
                if self.first_time(point) {
                    self.tally("ovulation_tests", label_of(&c["result"]));
                }
            }
            "menstrual-period" if self.first_time(point) => {
                self.sum("menstrual_period_started", Some(1.0));
            }
            _ => {}
        }
    }

    /// A record of labels (moods, symptoms): how many records, and how many
    /// times each label was logged. `list` is the name of the label list in
    /// the point.
    fn labelled(&mut self, keys: (&str, &'static str), c: &Value, list: &str, point: &Value) {
        if !self.first_time(point) {
            return;
        }
        self.sum(keys.0, Some(1.0));
        for label in c[list].as_array().into_iter().flatten() {
            self.tally(keys.1, label_of(label));
        }
    }

    /// One logged meal or snack: energy, carbohydrate, fat and a few
    /// nutrients, summed over the day.
    fn nutrition(&mut self, c: &Value, point: &Value) {
        if !self.first_time(point) {
            return;
        }
        self.sum("nutrition.entries", Some(1.0));
        self.sum("nutrition.kcal", num(&c["energy"]["kcal"]));
        self.sum("nutrition.carbs_g", num(&c["totalCarbohydrate"]["grams"]));
        self.sum("nutrition.fat_g", num(&c["totalFat"]["grams"]));
        for item in c["nutrients"].as_array().into_iter().flatten() {
            let grams = num(&item["quantity"]["grams"]).map(|g| g.max(0.0));
            match item["nutrient"].as_str() {
                Some("PROTEIN") => self.sum("nutrition.protein_g", grams),
                Some("DIETARY_FIBER") => self.sum("nutrition.fiber_g", grams),
                Some("SUGAR") => self.sum("nutrition.sugar_g", grams),
                Some("SODIUM") => self.sum("nutrition.sodium_mg", grams.map(|g| g * 1000.0)),
                _ => {}
            }
        }
    }

    fn daily_vo2_max(&mut self, c: &Value) {
        let mut object = Map::new();
        if let Some(value) = num(&c["vo2Max"]) {
            object.insert("value".into(), json!(round(value, 1)));
        }
        if let Some(level) = text(&c["cardioFitnessLevel"]) {
            object.insert("level".into(), level.into());
        }
        if let Some(estimated) = c["estimated"].as_bool() {
            object.insert("estimated".into(), estimated.into());
        }
        if !object.is_empty() {
            self.scalars.insert("vo2max_daily", Value::Object(object));
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
        if self.workouts.len() >= MAX_WORKOUTS {
            self.workouts_omitted += 1;
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
        if let Some(mm) = num(&summary["distanceMillimeters"]) {
            workout.insert("distance_m".into(), round(mm.max(0.0) / 1000.0, 1).into());
        }
        if let Some(bpm) = whole(&summary["averageHeartRateBeatsPerMinute"]).filter(|b| *b > 0) {
            workout.insert("avg_hr".into(), bpm.into());
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
        if self.sleep.len() >= MAX_SLEEPS {
            self.sleeps_omitted += 1;
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
        let mut sleep = json!({
            "minutes": minutes,
            "start": text(&c["interval"]["startTime"]),
            "end": text(&c["interval"]["endTime"]),
            "stages": stages,
        });
        if c["metadata"]["nap"] == true {
            sleep["nap"] = true.into();
        }
        self.sleep.push(sleep);
    }
}

/// The `MAX_LABELS` most frequent labels (ties by name) with their counts.
fn top_labels(labels: &BTreeMap<String, u64>) -> BTreeMap<&str, u64> {
    let mut all: Vec<(&String, &u64)> = labels.iter().collect();
    all.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    all.into_iter()
        .take(MAX_LABELS)
        .map(|(label, n)| (label.as_str(), *n))
        .collect()
}

/// A label from an enum-valued field: upper-case letters, digits and
/// underscores, at most 40. Google may add values, and what is stored is
/// shown to the model, so anything else is not kept.
fn label_of(value: &Value) -> Option<String> {
    let label = text(value)?;
    let ok = label.len() <= 40
        && label
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    ok.then_some(label)
}

/// Drop keys, least useful first, until `map` is within [`MAX_DAY_BYTES`].
/// What was dropped is named in `dropped`, so a missing key is not read as
/// "no data".
fn shrink(map: &mut Map<String, Value>) {
    let mut dropped = Vec::new();
    for key in DROP_ORDER {
        if Value::Object(map.clone()).to_string().len() <= MAX_DAY_BYTES {
            break;
        }
        if map.remove(key).is_some() {
            dropped.push(key);
        }
    }
    if !dropped.is_empty() {
        map.insert("dropped".into(), json!(dropped));
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
        let Some(spec) = catalog::find(data_type) else {
            return;
        };
        for point in points {
            let component = &point[spec.field];
            if !component.is_object() {
                continue;
            }
            if let Some(date) = day_of(component, spec.filter, &self.zone) {
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

/// One fixture per type family, in the shapes the code documents: a point
/// is `{name?, <field>: {...}}`, and each family's component is read from
/// the fields Google's v4 `DataPoint` union gives it.
#[cfg(test)]
mod family_tests {
    use super::*;

    fn paris() -> TimeZone {
        TimeZone::get("Europe/Paris").unwrap()
    }

    /// The days the points of each type make, for 1 to 19 October 2026.
    fn days_of(types: &[(&str, Vec<Value>)]) -> Vec<(String, Value)> {
        let mut days = Days::new(paris());
        for (data_type, points) in types {
            days.add(data_type, points);
        }
        days.finish("2026-10-01".parse().unwrap(), "2026-10-20".parse().unwrap())
            .into_iter()
            .map(|(date, metrics)| (date, serde_json::from_str(&metrics).unwrap()))
            .collect()
    }

    #[test]
    fn a_negative_altitude_change_is_kept_so_the_day_shows_the_net_change() {
        let change = |t: &str, mm: &str| json!({"altitude": {"interval": {"startTime": t}, "gainMillimeters": mm}});
        let rows = days_of(&[(
            "altitude",
            vec![
                change("2026-10-09T08:00:00Z", "12000"),
                change("2026-10-09T09:00:00Z", "-2500"),
            ],
        )]);
        assert_eq!(
            rows,
            [("2026-10-09".to_string(), json!({"elevation_change_m": 9.5}))]
        );
    }

    /// A logged meal with the nutrients the day keeps. `sodium` is in grams,
    /// as Google gives it.
    fn meal(name: &str, time: &str, protein: f64, sodium: f64) -> Value {
        json!({"name": name, "nutritionLog": {
        "interval": {"startTime": time},
        "energy": {"kcal": "300"},
        "totalCarbohydrate": {"grams": 40},
        "totalFat": {"grams": 10.5},
        "nutrients": [
            {"nutrient": "PROTEIN", "quantity": {"grams": protein}},
            {"nutrient": "DIETARY_FIBER", "quantity": {"grams": "5"}},
            {"nutrient": "SUGAR", "quantity": {"grams": 12}},
            {"nutrient": "SODIUM", "quantity": {"grams": sodium}},
            {"nutrient": "CALCIUM", "quantity": {"grams": 1}},
            {"nutrient": "PROTEIN"}
        ]}})
    }

    #[test]
    fn nutrition_sums_the_meals_of_a_day_and_converts_sodium_to_milligrams() {
        let rows = days_of(&[(
            "nutrition-log",
            vec![
                meal("n1", "2026-10-09T06:00:00Z", 20.0, 0.5),
                // The same point again, as a second page would bring it.
                meal("n1", "2026-10-09T06:00:00Z", 20.0, 0.5),
                meal("n2", "2026-10-09T18:00:00Z", 10.0, 0.25),
            ],
        )]);
        assert_eq!(rows[0].0, "2026-10-09");
        assert_eq!(
            rows[0].1["nutrition"],
            json!({
                "entries": 2,
                "kcal": 600.0,
                "carbs_g": 80.0,
                "fat_g": 21.0,
                "protein_g": 30.0,
                "fiber_g": 10.0,
                "sugar_g": 24.0,
                "sodium_mg": 750.0,
            })
        );
    }

    #[test]
    fn sleep_keeps_four_nights_on_a_day_and_counts_the_rest() {
        let night = |n: i64| {
            json!({"name": format!("s{n}"), "sleep": {
                "interval": {"startTime": format!("2026-10-09T0{n}:00:00Z"),
                             "endTime": "2026-10-10T06:00:00Z"},
                "summary": {"minutesAsleep": 100 + n}}})
        };
        let rows = days_of(&[("sleep", (1..=5).map(night).collect())]);
        assert_eq!(rows[0].0, "2026-10-10");
        assert_eq!(rows[0].1["sleeps_omitted"], 1);
        let kept: Vec<i64> = rows[0].1["sleep"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["minutes"].as_i64().unwrap())
            .collect();
        assert_eq!(kept, [101, 102, 103, 104]);
    }

    #[test]
    fn workouts_keep_eight_a_day_and_count_the_rest() {
        let run = |n: u32| {
            json!({"name": format!("w{n}"), "exercise": {
                "interval": {"startTime": format!("2026-10-09T{n:02}:00:00Z")},
                "activeDuration": "600s",
                "exerciseType": "RUNNING"}})
        };
        let rows = days_of(&[("exercise", (1..=9).map(run).collect())]);
        assert_eq!(rows[0].1["workouts_omitted"], 1);
        let workouts = rows[0].1["workouts"].as_array().unwrap();
        assert_eq!(workouts.len(), 8);
        assert!(
            workouts
                .iter()
                .all(|w| w == &json!({"type": "RUNNING", "minutes": 10}))
        );
    }

    #[test]
    fn mood_labels_must_be_upper_case_and_short_and_only_the_top_twelve_are_listed() {
        let mood = |name: &str, day: &str, labels: Vec<String>| {
            json!({"name": name, "moods": {
                "sampleTime": {"physicalTime": format!("{day}T08:00:00Z")},
                "moods": labels}})
        };
        // A label with a lower-case letter, a dash, or more than 40 bytes is
        // not kept.
        let bad = vec![
            "calm".to_string(),
            "STRESSED-OUT".to_string(),
            "A".repeat(41),
            "ANXIOUS".to_string(),
        ];
        // Label `L<n>` is logged n times, on distinct records: thirteen
        // labels, of which the twelve most frequent are listed.
        let mut many = Vec::new();
        for n in 1..=13 {
            for k in 0..n {
                many.push(mood(
                    &format!("L{n}-{k}"),
                    "2026-10-09",
                    vec![format!("L{n:02}")],
                ));
            }
        }
        let rows = days_of(&[
            ("moods", vec![mood("bad", "2026-10-10", bad)]),
            ("moods", many),
        ]);
        let by_day = |d: &str| rows.iter().find(|(date, _)| date == d).unwrap().1.clone();
        assert_eq!(
            by_day("2026-10-10")["moods"],
            json!({"count": 1, "labels": {"ANXIOUS": 1}})
        );
        let top: serde_json::Map<String, Value> =
            (2..=13).map(|n| (format!("L{n:02}"), json!(n))).collect();
        assert_eq!(
            by_day("2026-10-09")["moods"],
            json!({"count": 91, "labels": top})
        );
    }

    #[test]
    fn an_ecg_is_placed_by_its_start_in_the_users_zone_and_kept_as_a_count() {
        let ecg = |name: &str, start: &str, class: &str| {
            json!({"name": name, "electrocardiogram": {
                "interval": {"startTime": start},
                "resultClassification": class,
                "waveformSamples": [1, 2, 3]}})
        };
        let rows = days_of(&[(
            "electrocardiogram",
            vec![
                // 19:00 UTC is 21:00 in Paris on the 9th.
                ecg("e1", "2026-10-09T19:00:00Z", "SINUS_RHYTHM"),
                // 23:30 UTC is 01:30 on the 10th in Paris.
                ecg("e2", "2026-10-09T23:30:00Z", "AFIB"),
                ecg("e2", "2026-10-09T23:30:00Z", "AFIB"),
            ],
        )]);
        assert_eq!(
            rows,
            [
                (
                    "2026-10-09".to_string(),
                    json!({"ecg": {"count": 1, "classes": {"SINUS_RHYTHM": 1}}})
                ),
                (
                    "2026-10-10".to_string(),
                    json!({"ecg": {"count": 1, "classes": {"AFIB": 1}}})
                ),
            ]
        );
        assert!(!rows[0].1.to_string().contains("waveform"));
    }

    #[test]
    fn a_point_without_an_object_is_skipped_and_an_unknown_type_adds_nothing() {
        let rows = days_of(&[
            (
                "steps",
                vec![json!({"steps": 5}), json!({"other": {"count": 3}})],
            ),
            (
                "not-a-type",
                vec![json!({"not-a-type": {
                    "interval": {"startTime": "2026-10-09T08:00:00Z"}, "count": 3}})],
            ),
        ]);
        assert!(rows.is_empty());
    }

    #[test]
    fn a_day_over_four_kib_drops_the_least_useful_keys_and_names_them() {
        // Eighty HR-zone limits (over 4 KiB alone) and eight workouts with
        // 600-character names. `hr_zones` goes first; that is not enough, so
        // `workouts` goes too, and nothing else is touched.
        let zones: Vec<Value> = (0..80)
            .map(|i| {
                json!({"heartRateZoneType": "ZONE_NAME",
                       "minBeatsPerMinute": 100 + i, "maxBeatsPerMinute": 101 + i})
            })
            .collect();
        let zones_point = json!({"name": "z", "dailyHeartRateZones": {
            "date": "2026-10-09", "heartRateZones": zones}});
        let run = |n: u32| {
            json!({"name": format!("w{n}"), "exercise": {
                "interval": {"startTime": format!("2026-10-09T{n:02}:00:00Z")},
                "activeDuration": "600s",
                "displayName": "R".repeat(600)}})
        };
        let rows = days_of(&[
            ("daily-heart-rate-zones", vec![zones_point]),
            ("exercise", (1..=8).map(run).collect()),
        ]);
        let day = &rows[0].1;
        assert_eq!(day["dropped"], json!(["hr_zones", "workouts"]));
        assert!(day.get("hr_zones").is_none());
        assert!(day.get("workouts").is_none());
        assert!(day.to_string().len() <= MAX_DAY_BYTES);
    }
}

#[cfg(test)]
mod more_family_tests {
    use super::*;

    fn paris() -> TimeZone {
        TimeZone::get("Europe/Paris").unwrap()
    }

    /// The days the points of each type make, for 1 to 19 October 2026.
    fn days_of(types: &[(&str, Vec<Value>)]) -> Vec<(String, Value)> {
        let mut days = Days::new(paris());
        for (data_type, points) in types {
            days.add(data_type, points);
        }
        days.finish("2026-10-01".parse().unwrap(), "2026-10-20".parse().unwrap())
            .into_iter()
            .map(|(date, metrics)| (date, serde_json::from_str(&metrics).unwrap()))
            .collect()
    }

    /// The one day of `rows`, which must have exactly one.
    fn only(rows: Vec<(String, Value)>) -> Value {
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows.into_iter().next().unwrap().1
    }

    #[test]
    fn samples_keep_their_range_average_and_count_and_skip_zero_heart_rates() {
        let sample = |name: &str, field: &str, value: Value| {
            json!({"name": name, field: {
                "sampleTime": {"physicalTime": "2026-10-09T08:00:00Z"},
                "beatsPerMinute": value}})
        };
        let heart = vec![
            json!({"name": "h1", "heartRate": {
                "sampleTime": {"physicalTime": "2026-10-09T08:00:00Z"}, "beatsPerMinute": "60"}}),
            json!({"name": "h2", "heartRate": {
                "sampleTime": {"physicalTime": "2026-10-09T08:01:00Z"}, "beatsPerMinute": 90}}),
            // A zero is no reading.
            sample("h3", "heartRate", json!(0)),
        ];
        let rows = days_of(&[
            ("heart-rate", heart),
            (
                "heart-rate-variability",
                vec![json!({"name": "v", "heartRateVariability": {
                    "sampleTime": {"physicalTime": "2026-10-09T08:00:00Z"},
                    "rootMeanSquareOfSuccessiveDifferencesMilliseconds": 42.5}})],
            ),
            (
                "oxygen-saturation",
                vec![json!({"name": "o", "oxygenSaturation": {
                    "sampleTime": {"physicalTime": "2026-10-09T08:00:00Z"}, "percentage": 97.0}})],
            ),
            (
                "respiratory-rate-sleep-summary",
                vec![json!({"name": "r", "respiratoryRateSleepSummary": {
                    "sampleTime": {"physicalTime": "2026-10-09T08:00:00Z"},
                    "fullSleepStats": {"breathsPerMinute": 14}}})],
            ),
            (
                "core-body-temperature",
                vec![json!({"name": "t", "coreBodyTemperature": {
                    "sampleTime": {"physicalTime": "2026-10-09T08:00:00Z"},
                    "temperatureCelsius": 36.6}})],
            ),
            (
                "blood-glucose",
                vec![json!({"name": "g", "bloodGlucose": {
                    "sampleTime": {"physicalTime": "2026-10-09T08:00:00Z"},
                    "bloodGlucoseMilligramsPerDeciliter": 95}})],
            ),
        ]);
        assert_eq!(
            only(rows),
            json!({
                "heart_rate": {"min": 60.0, "avg": 75.0, "max": 90.0, "n": 2},
                "hrv_ms": {"min": 42.5, "avg": 42.5, "max": 42.5, "n": 1},
                "spo2_pct": {"min": 97.0, "avg": 97.0, "max": 97.0, "n": 1},
                "resp_rate_sleep": {"min": 14.0, "avg": 14.0, "max": 14.0, "n": 1},
                "core_temp_c": {"min": 36.6, "avg": 36.6, "max": 36.6, "n": 1},
                "glucose_mgdl": {"min": 95.0, "avg": 95.0, "max": 95.0, "n": 1},
            })
        );
    }

    #[test]
    fn activity_minutes_and_energy_by_level_and_period() {
        let span = |start: &str, end: &str| json!({"startTime": start, "endTime": end});
        let rows = days_of(&[
            (
                "activity-level",
                vec![
                    json!({"name": "a1", "activityLevel": {
                        "interval": span("2026-10-09T08:00:00Z", "2026-10-09T08:30:00Z"),
                        "activityLevelType": "LIGHTLY_ACTIVE"}}),
                    json!({"name": "a2", "activityLevel": {
                        "interval": span("2026-10-09T09:00:00Z", "2026-10-09T09:15:00Z"),
                        "activityLevelType": "LIGHTLY_ACTIVE"}}),
                    // A level Google might add in lower case is not kept.
                    json!({"name": "a3", "activityLevel": {
                        "interval": span("2026-10-09T10:00:00Z", "2026-10-09T10:05:00Z"),
                        "activityLevelType": "lightly active"}}),
                ],
            ),
            (
                "sedentary-period",
                vec![json!({"name": "s", "sedentaryPeriod": {
                    "interval": span("2026-10-09T10:00:00Z", "2026-10-09T11:00:00Z")}})],
            ),
            (
                "swim-lengths-data",
                vec![
                    json!({"name": "w1", "swimLengthsData": {
                        "interval": span("2026-10-09T06:00:00Z", "2026-10-09T06:05:00Z"),
                        "strokeCount": "20"}}),
                    json!({"name": "w2", "swimLengthsData": {
                        "interval": span("2026-10-09T06:05:00Z", "2026-10-09T06:10:00Z"),
                        "strokeCount": 15}}),
                    // The first length again, as a second page would bring it.
                    json!({"name": "w1", "swimLengthsData": {
                        "interval": span("2026-10-09T06:00:00Z", "2026-10-09T06:05:00Z"),
                        "strokeCount": "20"}}),
                ],
            ),
            (
                "hydration-log",
                vec![json!({"name": "hy", "hydrationLog": {
                    "interval": span("2026-10-09T07:00:00Z", "2026-10-09T07:01:00Z"),
                    "amountConsumed": {"milliliters": "250"}}})],
            ),
            (
                "basal-energy-burned",
                vec![json!({"name": "b", "basalEnergyBurned": {
                    "interval": span("2026-10-09T00:00:00Z", "2026-10-09T23:59:00Z"),
                    "kcal": 1500.5}})],
            ),
            (
                "floors",
                vec![json!({"name": "f", "floors": {
                    "interval": span("2026-10-09T08:00:00Z", "2026-10-09T08:10:00Z"),
                    "count": 3}})],
            ),
        ]);
        assert_eq!(
            only(rows),
            json!({
                "activity_level_min": {"LIGHTLY_ACTIVE": 45},
                "sedentary_min": 60,
                "swim": {"lengths": 2, "strokes": 35},
                "hydration_ml": 250.0,
                "basal_kcal": 1500.5,
                "floors": 3,
            })
        );
    }

    #[test]
    fn body_measurements_and_fitness_keep_the_latest_or_the_daily_summary() {
        let at = |time: &str| json!({"physicalTime": time});
        let rows = days_of(&[
            (
                "height",
                vec![json!({"name": "ht", "height": {
                    "sampleTime": at("2026-10-09T06:00:00Z"), "heightMillimeters": 1750}})],
            ),
            (
                "vo2-max",
                vec![json!({"name": "v", "vo2Max": {
                    "sampleTime": at("2026-10-09T06:00:00Z"), "vo2Max": 45.678}})],
            ),
            (
                "run-vo2-max",
                vec![json!({"name": "rv", "runVo2Max": {
                    "sampleTime": at("2026-10-09T06:00:00Z"), "runVo2Max": 50.0}})],
            ),
            (
                "daily-vo2-max",
                vec![json!({"name": "dv", "dailyVo2Max": {
                    "date": "2026-10-09", "vo2Max": 44.44,
                    "cardioFitnessLevel": "ABOVE_AVERAGE", "estimated": true}})],
            ),
            (
                "daily-oxygen-saturation",
                vec![json!({"name": "do", "dailyOxygenSaturation": {
                    "date": "2026-10-09", "averagePercentage": 96.5,
                    "lowerBoundPercentage": 92.0, "upperBoundPercentage": 99.0}})],
            ),
            (
                "daily-respiratory-rate",
                vec![json!({"name": "dr", "dailyRespiratoryRate": {
                    "date": "2026-10-09", "breathsPerMinute": 14.0}})],
            ),
            (
                "daily-sleep-temperature-derivations",
                vec![json!({"name": "st", "dailySleepTemperatureDerivations": {
                    "date": "2026-10-09", "nightlyTemperatureCelsius": 0.3,
                    "baselineTemperatureCelsius": -0.1,
                    "relativeNightlyStddev30dCelsius": 0.25}})],
            ),
            (
                "daily-heart-rate-variability",
                vec![json!({"name": "dh", "dailyHeartRateVariability": {
                    "date": "2026-10-09",
                    "averageHeartRateVariabilityMilliseconds": 40.0,
                    "deepSleepRootMeanSquareOfSuccessiveDifferencesMilliseconds": 45.5,
                    "nonRemHeartRateBeatsPerMinute": 55,
                    "entropy": 1.23456}})],
            ),
        ]);
        assert_eq!(
            only(rows),
            json!({
                "height_cm": 175.0,
                "vo2max": 45.7,
                "run_vo2max": 50.0,
                "vo2max_daily": {"value": 44.4, "level": "ABOVE_AVERAGE", "estimated": true},
                "spo2_daily": {"avg": 96.5, "min": 92.0, "max": 99.0},
                "resp_rate_daily": 14.0,
                "sleep_temp": {"nightly_c": 0.3, "baseline_c": -0.1, "rel_stddev_30d_c": 0.25},
                "hrv_daily": {"avg_ms": 40.0, "deep_rmssd_ms": 45.5, "non_rem_hr": 55.0, "entropy": 1.23},
            })
        );
    }

    #[test]
    fn cycle_symptom_and_rhythm_records_are_counted_once_each() {
        let at = |time: &str| json!({"physicalTime": time});
        let rows = days_of(&[
            (
                "irregular-rhythm-notification",
                vec![
                    json!({"name": "i1", "irregularRhythmNotification": {
                        "interval": {"startTime": "2026-10-09T08:00:00Z"}}}),
                    json!({"name": "i2", "irregularRhythmNotification": {
                        "interval": {"startTime": "2026-10-09T09:00:00Z"}}}),
                ],
            ),
            (
                "symptoms",
                vec![
                    json!({"name": "sy", "symptoms": {
                        "sampleTime": at("2026-10-09T08:00:00Z"),
                        "symptoms": ["CRAMPS", "headache"]}}),
                    // The same record again, as a second page would bring it.
                    json!({"name": "sy", "symptoms": {
                        "sampleTime": at("2026-10-09T08:00:00Z"),
                        "symptoms": ["CRAMPS", "headache"]}}),
                ],
            ),
            (
                "ovulation-test",
                vec![json!({"name": "ov", "ovulationTest": {
                    "sampleTime": at("2026-10-09T08:00:00Z"), "result": "POSITIVE"}})],
            ),
            (
                "menstrual-period",
                vec![json!({"name": "mp", "menstrualPeriod": {
                    "interval": {"startTime": "2026-10-09T00:00:00Z"}}})],
            ),
        ]);
        assert_eq!(
            only(rows),
            json!({
                "irn": {"count": 2},
                "symptoms": {"count": 1, "labels": {"CRAMPS": 1}},
                "ovulation_tests": {"POSITIVE": 1},
                "menstrual_period_started": 1,
            })
        );
    }

    #[test]
    fn a_workout_keeps_its_distance_heart_rate_and_zone_minutes_when_given() {
        let rows = days_of(&[(
            "exercise",
            vec![
                json!({"name": "r1", "exercise": {
                    "interval": {"startTime": "2026-10-09T08:00:00Z"},
                    "activeDuration": "1200s", "displayName": "Ride",
                    "metricsSummary": {
                        "caloriesKcal": 200,
                        "distanceMillimeters": 15200000,
                        "averageHeartRateBeatsPerMinute": "140",
                        "activeZoneMinutes": 20}}}),
                // A heart rate of zero is not kept.
                json!({"name": "r2", "exercise": {
                    "interval": {"startTime": "2026-10-09T12:00:00Z"},
                    "activeDuration": "60s",
                    "metricsSummary": {"averageHeartRateBeatsPerMinute": 0}}}),
            ],
        )]);
        assert_eq!(
            only(rows)["workouts"],
            json!([
                {"type": "Ride", "minutes": 20, "kcal": 200.0, "distance_m": 15200.0,
                 "avg_hr": 140, "zone_min": 20},
                {"type": "Workout", "minutes": 1},
            ])
        );
    }

    #[test]
    fn a_nap_is_marked_and_its_length_comes_from_the_interval_without_a_summary() {
        let rows = days_of(&[(
            "sleep",
            vec![json!({"name": "nap", "sleep": {
                "interval": {"startTime": "2026-10-09T13:00:00Z", "endTime": "2026-10-09T13:45:00Z"},
                "metadata": {"nap": true},
                "summary": {"stagesSummary": [{"type": "LIGHT", "minutes": 30}, {"type": "BAD"}]}}})],
        )]);
        assert_eq!(
            only(rows)["sleep"],
            json!([{
                "minutes": 45,
                "start": "2026-10-09T13:00:00Z",
                "end": "2026-10-09T13:45:00Z",
                "stages": [{"type": "LIGHT", "minutes": 30}],
                "nap": true,
            }])
        );
    }
}

#[cfg(test)]
mod unknown_type_tests {
    use super::*;

    #[test]
    fn a_day_ignores_a_type_it_does_not_know() {
        let mut day = Day::default();
        day.add("not-a-type", &json!({"count": 3}), &json!({"name": "p"}));
        assert_eq!(day.to_json(), json!({}));
    }
}
