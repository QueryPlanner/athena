//! The Google Health data types a sync reads, and how each is asked for.
//!
//! Source: the v4 `DataPoint` union
//! (<https://developers.google.com/health/reference/rest/v4/users.dataTypes.dataPoints>)
//! and the list method's filter and page-size rules
//! (<https://developers.google.com/health/reference/rest/v4/users.dataTypes.dataPoints/list>,
//! <https://developers.google.com/health/filters>).
//!
//! Left out on purpose: `food` and `food-measurement-unit`. They are a
//! catalogue of foods and units, not something the user did: they carry no
//! time, so they cannot be placed on a day. What the user ate is
//! `nutrition-log`. Also left out: types that exist only in `v4beta`.

/// How a data type is placed in time, which decides the filter field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Instants: `{type}.interval.start_time`.
    IntervalStart,
    /// Instants: `{type}.sample_time.physical_time`.
    SamplePhysical,
    /// Dates: `{type}.date`.
    DailyDate,
    /// Session types, in the user's civil dates:
    /// `{type}.interval.civil_start_time`.
    SessionCivilStart,
    /// Sleep: `sleep.interval.end_time`, so a night belongs to the day it
    /// ended on.
    SleepEnd,
    /// ECG: `electrocardiogram.interval.start_time`, `>=` only.
    EcgStart,
}

/// One data type.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// The name in the request path, in kebab case.
    pub name: &'static str,
    /// The field of a data point that holds the values.
    pub field: &'static str,
    pub filter: Filter,
    /// Data points asked for per page.
    pub page_size: u32,
    /// The most pages read for this type in one sync.
    pub max_pages: usize,
    /// A type added after the first release (the 12 in [`core`] were read
    /// from the start). Its filter is less proven, and for women's health
    /// the docs list only create, update and delete, so an HTTP 400 or 404
    /// means "not available", reported with the status, not a failed sync.
    pub optional: bool,
}

/// Data points asked for per page, for the types with no limit of their own.
/// The documented maximum is 10 000, but a page is read whole under
/// `PAGE_BODY_LIMIT`, and minute-level points are several hundred bytes.
pub const PAGE_SIZE: u32 = 1000;
/// The most pages read for one data type in one sync, unless it has a limit
/// of its own.
pub const MAX_PAGES: usize = 60;
/// Documented maximum page size for `exercise` and `sleep`.
const SESSION_PAGE: u32 = 25;
/// ECG points carry a waveform of tens of thousands of samples.
const ECG_PAGE: u32 = 10;
/// Heart rate is the densest type: one point every few seconds.
const HEART_RATE_PAGES: usize = 400;

fn spec(name: &'static str, field: &'static str, filter: Filter) -> Spec {
    Spec {
        name,
        field,
        filter,
        page_size: PAGE_SIZE,
        max_pages: MAX_PAGES,
        optional: true,
    }
}

fn page(mut spec: Spec, page_size: u32) -> Spec {
    spec.page_size = page_size;
    spec
}

/// A type PR #48 already read: its failure fails the sync.
fn core(mut spec: Spec) -> Spec {
    spec.optional = false;
    spec
}

fn pages(mut spec: Spec, max_pages: usize) -> Spec {
    spec.max_pages = max_pages;
    spec
}

use std::sync::LazyLock;

use Filter::{DailyDate, EcgStart, IntervalStart, SamplePhysical, SessionCivilStart, SleepEnd};

/// Every data type a sync reads, in the order it reads them.
fn specs() -> Vec<Spec> {
    vec![
        // Activity and fitness: `activity_and_fitness.readonly`.
        core(spec("steps", "steps", IntervalStart)),
        spec("floors", "floors", IntervalStart),
        core(spec("distance", "distance", IntervalStart)),
        spec("altitude", "altitude", IntervalStart),
        core(spec(
            "active-energy-burned",
            "activeEnergyBurned",
            IntervalStart,
        )),
        spec("basal-energy-burned", "basalEnergyBurned", IntervalStart),
        core(spec("active-minutes", "activeMinutes", IntervalStart)),
        core(spec(
            "active-zone-minutes",
            "activeZoneMinutes",
            IntervalStart,
        )),
        spec("activity-level", "activityLevel", IntervalStart),
        spec("sedentary-period", "sedentaryPeriod", IntervalStart),
        spec("swim-lengths-data", "swimLengthsData", IntervalStart),
        core(spec(
            "time-in-heart-rate-zone",
            "timeInHeartRateZone",
            IntervalStart,
        )),
        page(
            core(spec("exercise", "exercise", SessionCivilStart)),
            SESSION_PAGE,
        ),
        spec("vo2-max", "vo2Max", SamplePhysical),
        spec("run-vo2-max", "runVo2Max", SamplePhysical),
        spec("daily-vo2-max", "dailyVo2Max", DailyDate),
        // Sleep: `sleep.readonly`.
        page(core(spec("sleep", "sleep", SleepEnd)), SESSION_PAGE),
        // Health metrics and measurements:
        // `health_metrics_and_measurements.readonly`.
        pages(
            spec("heart-rate", "heartRate", SamplePhysical),
            HEART_RATE_PAGES,
        ),
        spec(
            "heart-rate-variability",
            "heartRateVariability",
            SamplePhysical,
        ),
        spec("oxygen-saturation", "oxygenSaturation", SamplePhysical),
        spec(
            "respiratory-rate-sleep-summary",
            "respiratoryRateSleepSummary",
            SamplePhysical,
        ),
        spec(
            "core-body-temperature",
            "coreBodyTemperature",
            SamplePhysical,
        ),
        spec("blood-glucose", "bloodGlucose", SamplePhysical),
        core(spec("weight", "weight", SamplePhysical)),
        core(spec("body-fat", "bodyFat", SamplePhysical)),
        spec("height", "height", SamplePhysical),
        core(spec(
            "daily-resting-heart-rate",
            "dailyRestingHeartRate",
            DailyDate,
        )),
        spec(
            "daily-heart-rate-variability",
            "dailyHeartRateVariability",
            DailyDate,
        ),
        spec(
            "daily-oxygen-saturation",
            "dailyOxygenSaturation",
            DailyDate,
        ),
        spec("daily-respiratory-rate", "dailyRespiratoryRate", DailyDate),
        spec(
            "daily-sleep-temperature-derivations",
            "dailySleepTemperatureDerivations",
            DailyDate,
        ),
        core(spec(
            "daily-heart-rate-zones",
            "dailyHeartRateZones",
            DailyDate,
        )),
        // Nutrition: `nutrition.readonly`.
        page(
            spec("nutrition-log", "nutritionLog", SessionCivilStart),
            200,
        ),
        spec("hydration-log", "hydrationLog", SessionCivilStart),
        // ECG and irregular rhythm: `ecg.readonly`, `irn.readonly`.
        page(
            spec("electrocardiogram", "electrocardiogram", EcgStart),
            ECG_PAGE,
        ),
        page(
            spec(
                "irregular-rhythm-notification",
                "irregularRhythmNotification",
                SessionCivilStart,
            ),
            SESSION_PAGE,
        ),
        // Women's health, symptoms and mood: `reproductive_health.readonly`,
        // `logged_symptoms.readonly`, `mindfulness.readonly`.
        spec("menstrual-period", "menstrualPeriod", IntervalStart),
        spec("ovulation-test", "ovulationTest", SamplePhysical),
        spec("symptoms", "symptoms", SamplePhysical),
        spec("moods", "moods", SamplePhysical),
    ]
}

/// Every data type a sync reads, in the order it reads them. Built at
/// first use (not `const`, so the builders are ordinary code).
pub static SPECS: LazyLock<Vec<Spec>> = LazyLock::new(specs);

/// `(name, field)` of every data type, in [`SPECS`] order.
pub static TYPES: LazyLock<Vec<(&'static str, &'static str)>> =
    LazyLock::new(|| SPECS.iter().map(|s| (s.name, s.field)).collect());

/// The spec of `name`, or `None` for a type this build does not read.
pub fn find(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

/// How an unknown type name (only ever a test's) is asked for.
static UNKNOWN: LazyLock<Spec> = LazyLock::new(|| spec("", "", IntervalStart));

/// The spec of `name`, or the plain interval defaults for an unknown one.
pub fn of(name: &str) -> &'static Spec {
    find(name).unwrap_or(&UNKNOWN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet};

    /// Every type's filter, as the list in [`specs`] places it in time. A
    /// type moved to another kind of filter fails here, not on Google's side.
    const FILTERS: [(&str, Filter); 40] = [
        ("steps", IntervalStart),
        ("floors", IntervalStart),
        ("distance", IntervalStart),
        ("altitude", IntervalStart),
        ("active-energy-burned", IntervalStart),
        ("basal-energy-burned", IntervalStart),
        ("active-minutes", IntervalStart),
        ("active-zone-minutes", IntervalStart),
        ("activity-level", IntervalStart),
        ("sedentary-period", IntervalStart),
        ("swim-lengths-data", IntervalStart),
        ("time-in-heart-rate-zone", IntervalStart),
        ("exercise", SessionCivilStart),
        ("vo2-max", SamplePhysical),
        ("run-vo2-max", SamplePhysical),
        ("daily-vo2-max", DailyDate),
        ("sleep", SleepEnd),
        ("heart-rate", SamplePhysical),
        ("heart-rate-variability", SamplePhysical),
        ("oxygen-saturation", SamplePhysical),
        ("respiratory-rate-sleep-summary", SamplePhysical),
        ("core-body-temperature", SamplePhysical),
        ("blood-glucose", SamplePhysical),
        ("weight", SamplePhysical),
        ("body-fat", SamplePhysical),
        ("height", SamplePhysical),
        ("daily-resting-heart-rate", DailyDate),
        ("daily-heart-rate-variability", DailyDate),
        ("daily-oxygen-saturation", DailyDate),
        ("daily-respiratory-rate", DailyDate),
        ("daily-sleep-temperature-derivations", DailyDate),
        ("daily-heart-rate-zones", DailyDate),
        ("nutrition-log", SessionCivilStart),
        ("hydration-log", SessionCivilStart),
        ("electrocardiogram", EcgStart),
        ("irregular-rhythm-notification", SessionCivilStart),
        ("menstrual-period", IntervalStart),
        ("ovulation-test", SamplePhysical),
        ("symptoms", SamplePhysical),
        ("moods", SamplePhysical),
    ];

    #[test]
    fn there_are_forty_types_with_unique_names_and_fields() {
        assert_eq!(SPECS.len(), 40);
        assert_eq!(TYPES.len(), 40);
        let names: HashSet<&str> = SPECS.iter().map(|s| s.name).collect();
        let fields: HashSet<&str> = SPECS.iter().map(|s| s.field).collect();
        assert_eq!(names.len(), 40);
        assert_eq!(fields.len(), 40);
        // Kebab case, the form the request path takes.
        assert!(SPECS.iter().all(|s| {
            s.name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }));
    }

    #[test]
    fn every_type_has_the_filter_kind_its_time_field_needs() {
        assert_eq!(FILTERS.len(), SPECS.len());
        let expected: BTreeMap<&str, Filter> = FILTERS.into_iter().collect();
        for spec in SPECS.iter() {
            assert_eq!(spec.filter, expected[spec.name], "{}", spec.name);
        }
    }

    #[test]
    fn twelve_core_types_are_required_and_the_rest_optional() {
        let core: Vec<&str> = SPECS
            .iter()
            .filter(|s| !s.optional)
            .map(|s| s.name)
            .collect();
        assert_eq!(
            core,
            [
                "steps",
                "distance",
                "active-energy-burned",
                "active-minutes",
                "active-zone-minutes",
                "time-in-heart-rate-zone",
                "exercise",
                "sleep",
                "weight",
                "body-fat",
                "daily-resting-heart-rate",
                "daily-heart-rate-zones",
            ]
        );
        assert_eq!(SPECS.iter().filter(|s| s.optional).count(), 28);
    }

    #[test]
    fn session_and_dense_types_have_their_own_page_sizes() {
        let page = |name: &str| find(name).unwrap().page_size;
        for small in ["exercise", "sleep", "irregular-rhythm-notification"] {
            assert_eq!(page(small), 25, "{small}");
        }
        assert_eq!(page("electrocardiogram"), 10);
        assert_eq!(page("nutrition-log"), 200);
        for spec in SPECS.iter() {
            let own = [
                "exercise",
                "sleep",
                "irregular-rhythm-notification",
                "electrocardiogram",
                "nutrition-log",
            ]
            .contains(&spec.name);
            if !own {
                assert_eq!(spec.page_size, PAGE_SIZE, "{}", spec.name);
            }
        }
    }

    #[test]
    fn heart_rate_may_read_four_hundred_pages_and_the_rest_sixty() {
        assert_eq!(find("heart-rate").unwrap().max_pages, HEART_RATE_PAGES);
        assert_eq!(HEART_RATE_PAGES, 400);
        assert_eq!(MAX_PAGES, 60);
        for spec in SPECS.iter().filter(|s| s.name != "heart-rate") {
            assert_eq!(spec.max_pages, MAX_PAGES, "{}", spec.name);
        }
    }

    #[test]
    fn an_unknown_type_gets_the_plain_interval_defaults() {
        assert!(find("not-a-type").is_none());
        let unknown = of("not-a-type");
        assert_eq!(unknown.name, "");
        assert_eq!(unknown.field, "");
        assert_eq!(unknown.filter, IntervalStart);
        assert_eq!(unknown.page_size, PAGE_SIZE);
        assert_eq!(unknown.max_pages, MAX_PAGES);
        assert!(unknown.optional);
        assert_eq!(of("steps").field, "steps");
    }
}
