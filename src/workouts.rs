//! User-owned training records: a Push / Pull / Legs split, a weekly VO2
//! session (a rowing piece), and what progressive overload suggests next.
//!
//! Weights are kilograms only. A rowing piece is its distance and time; the
//! 500 m split is derived, never stored. Tool ownership comes from the
//! host's session, never from arguments, as for calories.
use crate::calories::{date, text};
use crate::{runner::Conversation, store::Store, timezone::NoArgs};
use anyhow::{Result, bail, ensure};
use jiff::Timestamp;
use rig_agent::{
    agent::{AgentBuilder, WithBuilderTools},
    tool::{Tool, ToolContext, ToolExecutionError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_EXERCISES: usize = 30;
pub const MAX_SETS: usize = 20;
pub const MAX_WEIGHT_KG: f64 = 1000.0;
pub const MAX_REPS: u32 = 1000;
pub const DEFAULT_DISTANCE_M: u32 = 2000;
/// Plates usually go up in 2.5 kg steps; the suggestion adds one.
pub const WEIGHT_STEP_KG: f64 = 2.5;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DayType {
    Push,
    Pull,
    Legs,
    Vo2,
    Other,
}

impl DayType {
    pub fn as_str(self) -> &'static str {
        match self {
            DayType::Push => "push",
            DayType::Pull => "pull",
            DayType::Legs => "legs",
            DayType::Vo2 => "vo2",
            DayType::Other => "other",
        }
    }

    /// A stored value. The column has no CHECK, so a type this build does
    /// not know is an error rather than a guess.
    pub fn parse(value: &str) -> Result<DayType> {
        Ok(match value {
            "push" => DayType::Push,
            "pull" => DayType::Pull,
            "legs" => DayType::Legs,
            "vo2" => DayType::Vo2,
            "other" => DayType::Other,
            _ => bail!("unknown day type `{value}`"),
        })
    }

    /// The lifting day after this one in the Push → Pull → Legs rotation.
    /// VO2 and other days are not part of the rotation.
    pub fn after(last_lifting: Option<DayType>) -> DayType {
        match last_lifting {
            Some(DayType::Push) => DayType::Pull,
            Some(DayType::Pull) => DayType::Legs,
            _ => DayType::Push,
        }
    }
}

/// One set. `weight_kg` 0 means bodyweight or no added load.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Set {
    pub reps: u32,
    pub weight_kg: f64,
    pub target_reps: Option<u32>,
    #[serde(default)]
    pub is_warmup: bool,
    pub notes: Option<String>,
}

/// One exercise block: a lift and its sets, in order. The same lift may
/// appear in more than one block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Exercise {
    pub name: String,
    pub sets: Vec<Set>,
}

fn default_distance() -> u32 {
    DEFAULT_DISTANCE_M
}

/// A rowing piece: its distance and the time it took, nothing else.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rowing {
    #[serde(default = "default_distance")]
    pub distance_m: u32,
    /// `M:SS`, `M:SS.f` (up to milliseconds) or `H:MM:SS(.f)`.
    pub time: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Workout {
    pub session_date: String,
    pub day_type: DayType,
    pub notes: Option<String>,
    #[serde(default)]
    pub exercises: Vec<Exercise>,
    pub rowing: Option<Rowing>,
}

/// A workout as stored: validated, weights rounded to 0.01 kg, the rowing
/// time in milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Prepared {
    pub workout: Workout,
    pub time_ms: Option<i64>,
}

impl Prepared {
    /// What a retry key is bound to: the normalized workout, so `7:05` and
    /// `7:05.0`, or `-0` and `0` kg, are the same original.
    pub(crate) fn hash_input(&self) -> Value {
        let w = &self.workout;
        json!({
            "session_date": w.session_date,
            "day_type": w.day_type,
            "notes": w.notes,
            "exercises": w.exercises,
            "rowing": w.rowing.as_ref().map(|r| json!({"distance_m": r.distance_m, "time_ms": self.time_ms})),
        })
    }
}

fn round_kg(kg: f64) -> f64 {
    // `+ 0.0` turns IEEE negative zero into zero.
    (kg * 100.0).round() / 100.0 + 0.0
}

impl Workout {
    pub(crate) fn prepare(mut self) -> Result<Prepared> {
        date(&self.session_date)?;
        if let Some(notes) = &self.notes {
            text(notes, 512)?;
        }
        ensure!(
            !self.exercises.is_empty() || self.rowing.is_some(),
            "a workout needs at least one exercise or a rowing piece"
        );
        ensure!(
            self.exercises.len() <= MAX_EXERCISES,
            "at most {MAX_EXERCISES} exercise blocks"
        );
        for exercise in &mut self.exercises {
            text(&exercise.name, 64)?;
            ensure!(
                (1..=MAX_SETS).contains(&exercise.sets.len()),
                "each exercise needs 1 to {MAX_SETS} sets"
            );
            for set in &mut exercise.sets {
                ensure!(
                    set.weight_kg.is_finite() && (0.0..=MAX_WEIGHT_KG).contains(&set.weight_kg),
                    "weight_kg must be finite, in kilograms, between 0 and {MAX_WEIGHT_KG}"
                );
                set.weight_kg = round_kg(set.weight_kg);
                ensure!(set.reps <= MAX_REPS, "reps must be at most {MAX_REPS}");
                ensure!(
                    set.target_reps.is_none_or(|t| (1..=MAX_REPS).contains(&t)),
                    "target_reps must be between 1 and {MAX_REPS}"
                );
                if let Some(notes) = &set.notes {
                    text(notes, 256)?;
                }
            }
        }
        let time_ms = match &self.rowing {
            Some(rowing) => {
                ensure!(
                    (100..=100_000).contains(&rowing.distance_m),
                    "distance_m must be between 100 and 100000"
                );
                let ms = parse_time(&rowing.time)?;
                let split = split_ms(ms, rowing.distance_m);
                ensure!(
                    (60_000..=600_000).contains(&split),
                    "a {} m piece in {} is a {} split per 500 m; check the time and distance",
                    rowing.distance_m,
                    rowing.time,
                    format_time(split)
                );
                Some(ms)
            }
            None => None,
        };
        Ok(Prepared {
            workout: self,
            time_ms,
        })
    }
}

/// How exercises are matched across sessions: case and spacing ignored,
/// the display name kept as typed.
pub fn exercise_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A rowing time in milliseconds: `M:SS`, `H:MM:SS`, either with up to
/// three decimals on the seconds. Seconds (and minutes, with hours) must be
/// under 60.
pub fn parse_time(value: &str) -> Result<i64> {
    let invalid = || anyhow::anyhow!("time must be M:SS, M:SS.f or H:MM:SS, e.g. 7:05.3");
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (value, ""),
    };
    let parts: Vec<&str> = whole.split(':').collect();
    let digits = |s: &str, max_len: usize| {
        !s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit())
    };
    ensure!(
        (2..=3).contains(&parts.len())
            && digits(parts[0], 3)
            && parts[1..].iter().all(|p| p.len() == 2 && digits(p, 2))
            && fraction.len() <= 3
            && (fraction.is_empty() || digits(fraction, 3))
            && !(value.contains('.') && fraction.is_empty()),
        invalid()
    );
    let numbers: Vec<i64> = parts.iter().map(|p| p.parse().unwrap_or(0)).collect();
    ensure!(numbers[1..].iter().all(|n| *n < 60), invalid());
    let seconds = numbers.iter().fold(0, |total, n| total * 60 + n);
    let millis = format!("{fraction:0<3}").parse::<i64>().unwrap_or(0);
    let ms = seconds * 1000 + millis;
    ensure!(ms > 0, "time must be more than zero");
    Ok(ms)
}

/// `M:SS.t`, or `H:MM:SS.t` from an hour, to the nearest tenth as a rowing
/// monitor shows it.
pub fn format_time(ms: i64) -> String {
    let tenths = (ms + 50) / 100;
    let (seconds, tenth) = (tenths / 10, tenths % 10);
    let (minutes, second) = (seconds / 60, seconds % 60);
    if minutes >= 60 {
        format!("{}:{:02}:{second:02}.{tenth}", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}:{second:02}.{tenth}")
    }
}

/// The time per 500 m of a piece, in milliseconds.
pub fn split_ms(time_ms: i64, distance_m: u32) -> i64 {
    (time_ms * 500 + i64::from(distance_m) / 2) / i64::from(distance_m)
}

/// Estimated one-rep max by Epley, to 0.1 kg. One rep is the weight itself;
/// zero reps is no estimate.
pub fn estimated_1rm(weight_kg: f64, reps: u32) -> Option<f64> {
    match reps {
        0 => None,
        1 => Some(weight_kg),
        _ => Some((weight_kg * (1.0 + f64::from(reps) / 30.0) * 10.0).round() / 10.0),
    }
}

/// A set as read back, for ranking and suggestions.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Lifted {
    pub reps: u32,
    pub weight_kg: f64,
    pub target_reps: Option<u32>,
    pub is_warmup: bool,
}

/// The best working set: warm-ups and zero-rep sets excluded, ranked by
/// estimated 1RM, then weight, then reps (so bodyweight sets rank by reps).
pub(crate) fn top_set(sets: &[Lifted]) -> Option<&Lifted> {
    let key = |s: &Lifted| {
        (
            estimated_1rm(s.weight_kg, s.reps).unwrap_or(0.0),
            s.weight_kg,
            s.reps,
        )
    };
    sets.iter()
        .filter(|s| !s.is_warmup && s.reps > 0)
        .max_by(|a, b| {
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// What progressive overload suggests after these sets: when the top set
/// reached its target reps, the same reps with 2.5 kg more; otherwise (or
/// with no target, or bodyweight) one more rep at the same weight.
pub(crate) fn progression(sets: &[Lifted]) -> Value {
    let Some(top) = top_set(sets) else {
        return Value::Null;
    };
    let hit = top.target_reps.map(|t| top.reps >= t);
    let (weight_kg, reps) = match (hit, top.target_reps) {
        (Some(true), Some(target)) if top.weight_kg > 0.0 => {
            (round_kg(top.weight_kg + WEIGHT_STEP_KG), target)
        }
        _ => (top.weight_kg, top.reps + 1),
    };
    json!({
        "top_set": {"weight_kg": top.weight_kg, "reps": top.reps, "target_reps": top.target_reps},
        "hit_target": hit,
        "suggested": {"weight_kg": weight_kg, "reps": reps},
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    pub request_key: String,
    pub workout: Workout,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Last {
    pub day_type: DayType,
    /// Exclusive. Defaults to the user's today, so today's own session is
    /// never what "last time" recalls.
    pub before_date: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Lift,
    Rowing,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    #[serde(default)]
    pub kind: Kind,
    pub exercise: Option<String>,
    pub distance_m: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub start_date: String,
    pub end_date: String,
    pub limit: Option<u32>,
    pub before_id: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub id: i64,
    pub expected_version: i64,
    pub workout: Workout,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remove {
    pub id: i64,
    pub expected_version: i64,
}

pub const NAMES: [&str; 7] = [
    "workout_log",
    "workout_last",
    "workout_next",
    "exercise_progress",
    "workout_history",
    "workout_update",
    "workout_remove",
];

pub fn register(
    builder: AgentBuilder<WithBuilderTools>,
    store: Store,
) -> AgentBuilder<WithBuilderTools> {
    builder
        .tool(WorkoutLog(store.clone()))
        .tool(WorkoutLast(store.clone()))
        .tool(WorkoutNext(store.clone()))
        .tool(ExerciseProgress(store.clone()))
        .tool(WorkoutHistory(store.clone()))
        .tool(WorkoutUpdate(store.clone()))
        .tool(WorkoutRemove(store))
}

fn day_type_schema() -> Value {
    json!({"type":"string","enum":["push","pull","legs","vo2","other"]})
}

fn workout_schema() -> Value {
    json!({"type":"object","properties":{
        "session_date":{"type":"string","description":"The user's local date YYYY-MM-DD; call now if unsure"},
        "day_type":day_type_schema(),
        "notes":{"type":["string","null"]},
        "exercises":{"type":"array","items":{"type":"object","properties":{
            "name":{"type":"string","description":"Reuse the exact name from earlier sessions"},
            "sets":{"type":"array","items":{"type":"object","properties":{
                "reps":{"type":"integer"},
                "weight_kg":{"type":"number","description":"Kilograms; convert pounds (x 0.4536). 0 for bodyweight"},
                "target_reps":{"type":["integer","null"]},
                "is_warmup":{"type":"boolean"},
                "notes":{"type":["string","null"]}},
                "required":["reps","weight_kg"],"additionalProperties":false}}},
            "required":["name","sets"],"additionalProperties":false}},
        "rowing":{"type":["object","null"],"properties":{
            "distance_m":{"type":"integer","description":"Default 2000"},
            "time":{"type":"string","description":"M:SS.f, e.g. 7:05.3"}},
            "required":["time"],"additionalProperties":false}},
        "required":["session_date","day_type"],"additionalProperties":false})
}

fn mutation_properties() -> Value {
    json!({"id":{"type":"integer"},"expected_version":{"type":"integer"}})
}

macro_rules! tool {
    ($ty:ident,$name:literal,$args:ty,$call:expr,$description:literal,$props:expr,$required:expr) => {
        pub struct $ty(pub Store);
        impl Tool for $ty {
            const NAME: &'static str = $name;
            type Args = $args;
            type Output = Value;
            type Error = ToolExecutionError;
            fn description(&self) -> String {
                $description.into()
            }
            fn parameters(&self) -> Value {
                json!({"type":"object","properties":$props,"required":$required,"additionalProperties":false})
            }
            async fn call(
                &self,
                context: &mut ToolContext,
                args: Self::Args,
            ) -> Result<Value, Self::Error> {
                let session = context.require::<Conversation>()?.0.clone();
                self.0
                    .call(move |store| {
                        let owner = store
                            .session_owner(&session)?
                            .ok_or_else(|| anyhow::anyhow!("unknown session"))?;
                        let call: fn(&Store, i64, $args) -> Result<Value> = $call;
                        call(store, owner, args)
                    })
                    .await
                    .map_err(|e| ToolExecutionError::other(e.to_string()))
            }
        }
    };
}

tool!(
    WorkoutLog,
    "workout_log",
    Log,
    |s, o, a| s.workout_log(o, a),
    "Log one training session: its exercises with every set (reps, weight in kg), or a rowing piece (distance and time only). Use a fresh opaque request_key per session; reuse it only to retry the same original. Deleted retries remain deleted.",
    json!({"request_key":{"type":"string"},"workout":workout_schema()}),
    ["request_key", "workout"]
);
tool!(
    WorkoutLast,
    "workout_last",
    Last,
    |s, o, a| s.workout_last(o, a, Timestamp::now()),
    "The user's previous session of a day type, before today unless before_date (exclusive) is given, with every exercise, set and weight, and a progressive-overload suggestion per exercise.",
    json!({"day_type":day_type_schema(),"before_date":{"type":["string","null"]}}),
    ["day_type"]
);
tool!(
    WorkoutNext,
    "workout_next",
    NoArgs,
    |s, o, _| s.workout_next(o, Timestamp::now()),
    "What the user trains today: the next day in the Push, Pull, Legs rotation, whether the weekly VO2 session is due, sessions already logged today, and the previous session of the day due with suggestions.",
    json!({}),
    [] as [&str; 0]
);
tool!(
    ExerciseProgress,
    "exercise_progress",
    Progress,
    |s, o, a| s.exercise_progress(o, a),
    "Progress over time. kind lift (default) needs exercise: each session's top set and estimated 1RM, the best ever, and the change. kind rowing: times and 500 m splits at distance_m (default 2000). Newest first; limit default 10, maximum 50.",
    json!({"kind":{"type":"string","enum":["lift","rowing"]},"exercise":{"type":["string","null"]},
           "distance_m":{"type":["integer","null"]},"limit":{"type":["integer","null"]}}),
    [] as [&str; 0]
);
tool!(
    WorkoutHistory,
    "workout_history",
    History,
    |s, o, a| s.workout_history(o, a),
    "Read active sessions in an inclusive local date interval, newest logged first, with their sets and versions. Default limit 10, maximum 20; paginate with next_before_id.",
    json!({"start_date":{"type":"string"},"end_date":{"type":"string"},"limit":{"type":"integer"},"before_id":{"type":"integer"}}),
    ["start_date", "end_date"]
);
tool!(
    WorkoutUpdate,
    "workout_update",
    Update,
    |s, o, a| s.workout_update(o, a),
    "Replace a session (all its sets and rowing) using its current version from history or workout_last. A stale version conflicts; read again before correcting.",
    {
        let mut p = mutation_properties();
        p["workout"] = workout_schema();
        p
    },
    ["id", "expected_version", "workout"]
);
tool!(
    WorkoutRemove,
    "workout_remove",
    Remove,
    |s, o, a| s.workout_remove(o, a),
    "Soft-delete a session using its current version. It disappears from history, recall and progress; stale versions conflict.",
    mutation_properties(),
    ["id", "expected_version"]
);

#[cfg(test)]
mod tests {
    use super::*;

    fn lifted(weight_kg: f64, reps: u32, target_reps: Option<u32>, is_warmup: bool) -> Lifted {
        Lifted {
            reps,
            weight_kg,
            target_reps,
            is_warmup,
        }
    }

    fn set(reps: u32, weight_kg: f64) -> Set {
        Set {
            reps,
            weight_kg,
            target_reps: None,
            is_warmup: false,
            notes: None,
        }
    }

    fn workout() -> Workout {
        Workout {
            session_date: "2026-10-05".into(),
            day_type: DayType::Push,
            notes: None,
            exercises: vec![Exercise {
                name: "Bench Press".into(),
                sets: vec![set(5, 80.0)],
            }],
            rowing: None,
        }
    }

    #[test]
    fn day_types_round_trip_and_rotate() {
        for t in [
            DayType::Push,
            DayType::Pull,
            DayType::Legs,
            DayType::Vo2,
            DayType::Other,
        ] {
            assert_eq!(DayType::parse(t.as_str()).unwrap(), t);
            assert_eq!(serde_json::to_value(t).unwrap(), t.as_str());
        }
        assert!(DayType::parse("Push").is_err());
        assert_eq!(DayType::after(None), DayType::Push);
        assert_eq!(DayType::after(Some(DayType::Push)), DayType::Pull);
        assert_eq!(DayType::after(Some(DayType::Pull)), DayType::Legs);
        assert_eq!(DayType::after(Some(DayType::Legs)), DayType::Push);
    }

    #[test]
    fn exercise_names_match_ignoring_case_and_spacing() {
        assert_eq!(exercise_key("  Bench\t Press "), "bench press");
        assert_eq!(exercise_key("ÉCARTÉ"), "écarté");
        assert_ne!(exercise_key("bench"), exercise_key("bench press"));
    }

    #[test]
    fn rowing_times_parse_to_milliseconds() {
        for (text, ms) in [
            ("7:05", 425_000),
            ("7:05.3", 425_300),
            ("7:05.30", 425_300),
            ("7:05.305", 425_305),
            ("0:59", 59_000),
            ("125:00", 7_500_000),
            ("1:02:03.4", 3_723_400),
        ] {
            assert_eq!(parse_time(text).unwrap(), ms, "{text}");
        }
        for bad in [
            "",
            "7",
            "7:5",
            "7:60",
            "1:60:00",
            "7:05.",
            "7:05.1234",
            "7:05.a",
            "a:05",
            "1:2:3:4",
            "-7:05",
            "0:00",
            "0:00.0",
            "1234:00",
            " 7:05",
        ] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn times_and_splits_format_to_tenths() {
        assert_eq!(format_time(425_300), "7:05.3");
        assert_eq!(format_time(425_349), "7:05.3");
        assert_eq!(format_time(425_350), "7:05.4");
        assert_eq!(format_time(59_960), "1:00.0");
        assert_eq!(format_time(3_723_400), "1:02:03.4");
        // 7:05.3 for 2000 m is 1:46.3 per 500 m.
        assert_eq!(split_ms(425_300, 2000), 106_325);
        assert_eq!(format_time(split_ms(425_300, 2000)), "1:46.3");
        assert_eq!(split_ms(1001, 1000), 501);
    }

    #[test]
    fn epley_estimates_and_ranks_working_sets() {
        assert_eq!(estimated_1rm(100.0, 0), None);
        assert_eq!(estimated_1rm(100.0, 1), Some(100.0));
        assert_eq!(estimated_1rm(100.0, 5), Some(116.7));
        assert_eq!(estimated_1rm(0.0, 10), Some(0.0));
        let sets = [
            lifted(140.0, 5, None, true),
            lifted(100.0, 0, None, false),
            lifted(90.0, 8, None, false),
            lifted(100.0, 5, None, false),
        ];
        // 90 x 8 = 114.0 < 100 x 5 = 116.7; the warm-up and the failed set never count.
        assert_eq!(top_set(&sets).unwrap().weight_kg, 100.0);
        let bodyweight = [lifted(0.0, 8, None, false), lifted(0.0, 10, None, false)];
        assert_eq!(top_set(&bodyweight).unwrap().reps, 10);
        assert!(top_set(&[lifted(60.0, 5, None, true)]).is_none());
    }

    #[test]
    fn progression_adds_weight_after_a_hit_target_and_a_rep_otherwise() {
        let hit = progression(&[lifted(80.0, 8, Some(8), false)]);
        assert_eq!(hit["hit_target"], true);
        assert_eq!(hit["suggested"], json!({"weight_kg": 82.5, "reps": 8}));
        let missed = progression(&[lifted(80.0, 6, Some(8), false)]);
        assert_eq!(missed["hit_target"], false);
        assert_eq!(missed["suggested"], json!({"weight_kg": 80.0, "reps": 7}));
        let untargeted = progression(&[lifted(61.24, 5, None, false)]);
        assert!(untargeted["hit_target"].is_null());
        assert_eq!(
            untargeted["suggested"],
            json!({"weight_kg": 61.24, "reps": 6})
        );
        let bodyweight = progression(&[lifted(0.0, 12, Some(10), false)]);
        assert_eq!(bodyweight["hit_target"], true);
        assert_eq!(
            bodyweight["suggested"],
            json!({"weight_kg": 0.0, "reps": 13})
        );
        assert_eq!(
            progression(&[lifted(61.24, 5, Some(5), false)])["suggested"]["weight_kg"],
            63.74
        );
        assert!(progression(&[lifted(40.0, 10, None, true)]).is_null());
    }

    #[test]
    fn preparing_rounds_weights_and_normalizes_the_rowing_time() {
        let mut w = workout();
        w.exercises[0].sets[0].weight_kg = 61.234_9;
        w.exercises[0].sets.push(set(10, -0.0));
        w.rowing = Some(Rowing {
            distance_m: DEFAULT_DISTANCE_M,
            time: "7:05".into(),
        });
        let p = w.clone().prepare().unwrap();
        assert_eq!(p.workout.exercises[0].sets[0].weight_kg, 61.23);
        assert!(p.workout.exercises[0].sets[1].weight_kg.is_sign_positive());
        assert_eq!(p.time_ms, Some(425_000));
        let mut same = w;
        same.rowing.as_mut().unwrap().time = "7:05.000".into();
        same.exercises[0].sets[1].weight_kg = 0.0;
        assert_eq!(same.prepare().unwrap().hash_input(), p.hash_input());
        assert!(p.hash_input()["rowing"]["time_ms"] == 425_000);
        assert!(workout().prepare().unwrap().hash_input()["rowing"].is_null());
    }

    #[test]
    fn invalid_workouts_are_refused() {
        type Change = Box<dyn Fn(&mut Workout)>;
        let cases: Vec<(&str, Change)> = vec![
            ("date", Box::new(|w| w.session_date = "2026-02-30".into())),
            ("notes", Box::new(|w| w.notes = Some("a\nb".into()))),
            ("empty", Box::new(|w| w.exercises.clear())),
            (
                "too many exercises",
                Box::new(|w| w.exercises = vec![w.exercises[0].clone(); MAX_EXERCISES + 1]),
            ),
            ("name", Box::new(|w| w.exercises[0].name = " ".into())),
            (
                "long name",
                Box::new(|w| w.exercises[0].name = "a".repeat(65)),
            ),
            ("no sets", Box::new(|w| w.exercises[0].sets.clear())),
            (
                "too many sets",
                Box::new(|w| w.exercises[0].sets = vec![set(5, 80.0); MAX_SETS + 1]),
            ),
            (
                "negative",
                Box::new(|w| w.exercises[0].sets[0].weight_kg = -1.0),
            ),
            (
                "heavy",
                Box::new(|w| w.exercises[0].sets[0].weight_kg = 1000.5),
            ),
            (
                "nan",
                Box::new(|w| w.exercises[0].sets[0].weight_kg = f64::NAN),
            ),
            ("reps", Box::new(|w| w.exercises[0].sets[0].reps = 1001)),
            (
                "target 0",
                Box::new(|w| w.exercises[0].sets[0].target_reps = Some(0)),
            ),
            (
                "target high",
                Box::new(|w| w.exercises[0].sets[0].target_reps = Some(1001)),
            ),
            (
                "set notes",
                Box::new(|w| w.exercises[0].sets[0].notes = Some("a".repeat(257))),
            ),
            (
                "short row",
                Box::new(|w| {
                    w.rowing = Some(Rowing {
                        distance_m: 99,
                        time: "0:20".into(),
                    })
                }),
            ),
            (
                "long row",
                Box::new(|w| {
                    w.rowing = Some(Rowing {
                        distance_m: 100_001,
                        time: "9:00:00".into(),
                    })
                }),
            ),
            (
                "bad time",
                Box::new(|w| {
                    w.rowing = Some(Rowing {
                        distance_m: 2000,
                        time: "7.05".into(),
                    })
                }),
            ),
            (
                "split for total",
                Box::new(|w| {
                    w.rowing = Some(Rowing {
                        distance_m: 2000,
                        time: "1:46.3".into(),
                    })
                }),
            ),
            (
                "hours for minutes",
                Box::new(|w| {
                    w.rowing = Some(Rowing {
                        distance_m: 2000,
                        time: "7:05:00".into(),
                    })
                }),
            ),
        ];
        for (label, change) in cases {
            let mut w = workout();
            change(&mut w);
            assert!(w.prepare().is_err(), "{label}");
        }
        // A rowing piece alone is a workout; the boundaries are accepted.
        let mut w = workout();
        w.exercises.clear();
        w.rowing = Some(Rowing {
            distance_m: 2000,
            time: "7:05.3".into(),
        });
        w.prepare().unwrap();
        let mut w = workout();
        w.exercises[0].sets[0].weight_kg = 1000.0;
        w.exercises[0].sets[0].reps = 0;
        w.exercises[0].sets[0].target_reps = Some(1000);
        w.exercises = vec![w.exercises[0].clone(); MAX_EXERCISES];
        w.prepare().unwrap();
    }

    #[test]
    fn the_wire_format_defaults_and_refuses_unknown_fields() {
        let w: Workout = serde_json::from_value(json!({
            "session_date": "2026-10-05", "day_type": "vo2",
            "rowing": {"time": "7:05.3"}
        }))
        .unwrap();
        assert_eq!(w.rowing.unwrap().distance_m, DEFAULT_DISTANCE_M);
        assert!(w.exercises.is_empty());
        let s: Set = serde_json::from_value(json!({"reps": 5, "weight_kg": 80})).unwrap();
        assert!(!s.is_warmup);
        assert!(
            serde_json::from_value::<Rowing>(json!({"time": "7:05", "split": "1:46"})).is_err()
        );
        assert!(serde_json::from_value::<Set>(json!({"reps": 5, "weight_lb": 80})).is_err());
        assert!(
            serde_json::from_value::<Workout>(
                json!({"session_date": "2026-10-05", "day_type": "arms"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<Set>(json!({"reps": -1, "weight_kg": 80})).is_err());
        let p: Progress = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p.kind, Kind::Lift);
    }
}
