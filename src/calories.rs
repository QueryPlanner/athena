//! User-owned food records. Tool ownership comes from the host's session.
use crate::{runner::Conversation, store::Store};
use anyhow::{Result, ensure};
use rig_agent::{
    agent::{AgentBuilder, WithBuilderTools},
    tool::{Tool, ToolContext, ToolExecutionError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Values in kcal and grams. None means unknown, not zero.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Meal {
    pub description: String,
    pub consumed_date: String,
    pub calories: Option<f64>,
    pub protein_g: Option<f64>,
    pub carbs_g: Option<f64>,
    pub fat_g: Option<f64>,
    pub meal_type: Option<String>,
    pub source: Source,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    Estimated,
}

pub(crate) fn date(value: &str) -> Result<()> {
    let b = value.as_bytes();
    ensure!(
        b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && b.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()),
        "date must be YYYY-MM-DD"
    );
    let year: u32 = value[..4].parse()?;
    let month: u32 = value[5..7].parse()?;
    let day: u32 = value[8..].parse()?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    ensure!(
        year > 0 && day > 0 && day <= days,
        "date must be a real calendar date"
    );
    Ok(())
}
fn text(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "text must be nonblank, printable, and at most {max} bytes"
    );
    Ok(())
}
impl Meal {
    pub(crate) fn validate(&self) -> Result<()> {
        text(&self.description, 512)?;
        date(&self.consumed_date)?;
        if let Some(kind) = &self.meal_type {
            text(kind, 64)?;
        }
        for n in [self.calories, self.protein_g, self.carbs_g, self.fat_g]
            .into_iter()
            .flatten()
        {
            ensure!(
                n.is_finite() && (0.0..=1_000_000.0).contains(&n),
                "nutrients must be finite, nonnegative, and at most 1000000"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    pub request_key: String,
    pub meal: Meal,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub start_date: String,
    pub end_date: String,
}
impl Range {
    pub(crate) fn validate(&self) -> Result<()> {
        date(&self.start_date)?;
        date(&self.end_date)?;
        ensure!(
            self.start_date <= self.end_date,
            "start_date must precede end_date"
        );
        Ok(())
    }
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
    pub meal: Meal,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remove {
    pub id: i64,
    pub expected_version: i64,
}
impl Log {
    pub(crate) fn validate(&self) -> Result<()> {
        text(&self.request_key, 128)?;
        self.meal.validate()
    }
}

pub const NAMES: [&str; 5] = [
    "calorie_log",
    "calorie_history",
    "calorie_summary",
    "calorie_update",
    "calorie_remove",
];
pub fn register(
    builder: AgentBuilder<WithBuilderTools>,
    store: Store,
) -> AgentBuilder<WithBuilderTools> {
    builder
        .tool(CalorieLog(store.clone()))
        .tool(CalorieHistory(store.clone()))
        .tool(CalorieSummary(store.clone()))
        .tool(CalorieUpdate(store.clone()))
        .tool(CalorieRemove(store))
}
fn meal_schema() -> Value {
    json!({"type":"object","properties":{
        "description":{"type":"string"},"consumed_date":{"type":"string","description":"Explicit local calendar date YYYY-MM-DD; ask if unclear"},
        "calories":{"type":["number","null"]},"protein_g":{"type":["number","null"]},"carbs_g":{"type":["number","null"]},"fat_g":{"type":["number","null"]},
        "meal_type":{"type":["string","null"]},"source":{"type":"string","enum":["user","estimated"]}},"required":["description","consumed_date","source"]})
}
fn range_properties() -> Value {
    json!({"start_date":{"type":"string"},"end_date":{"type":"string"}})
}
fn mutation_properties() -> Value {
    json!({"id":{"type":"integer"},"expected_version":{"type":"integer"}})
}
macro_rules! tool {
    ($ty:ident,$name:literal,$args:ty,$method:ident,$description:literal,$props:expr,$required:expr) => {
        pub struct $ty(pub Store);
        impl Tool for $ty {
            const NAME: &'static str = $name;
            type Args = $args; type Output = Value; type Error = ToolExecutionError;
            fn description(&self) -> String { $description.into() }
            fn parameters(&self) -> Value { json!({"type":"object","properties":$props,"required":$required,"additionalProperties":false}) }
            async fn call(&self, context: &mut ToolContext, args: Self::Args) -> Result<Value,Self::Error> {
                let session = context.require::<Conversation>()?.0.clone();
                self.0.call(move |store| {
                    let owner = store.session_owner(&session)?.ok_or_else(|| anyhow::anyhow!("unknown session"))?;
                    store.$method(owner,args)
                }).await.map_err(|e| ToolExecutionError::other(e.to_string()))
            }
        }
    }
}
tool!(
    CalorieLog,
    "calorie_log",
    Log,
    calorie_log,
    "Log one food occurrence. Use a fresh opaque request_key for each occurrence; reuse that key only to retry the same original meal. Unknown nutrients stay null; kcal and grams. Deleted retries remain deleted.",
    json!({"request_key":{"type":"string"},"meal":meal_schema()}),
    ["request_key", "meal"]
);
tool!(
    CalorieHistory,
    "calorie_history",
    History,
    calorie_history,
    "Read active food entries in an inclusive local date interval, newest logged first. Default limit 20, maximum 50; paginate with next_before_id.",
    {
        let mut p = range_properties();
        p["limit"] = json!({"type":"integer"});
        p["before_id"] = json!({"type":"integer"});
        p
    },
    ["start_date", "end_date"]
);
tool!(
    CalorieSummary,
    "calorie_summary",
    Range,
    calorie_summary,
    "Total active meals in an inclusive local date interval. Null totals mean no known values; missing counts disclose incomplete totals.",
    range_properties(),
    ["start_date", "end_date"]
);
tool!(
    CalorieUpdate,
    "calorie_update",
    Update,
    calorie_update,
    "Replace a food entry using its current version from history. A stale version conflicts; read again before correcting.",
    {
        let mut p = mutation_properties();
        p["meal"] = meal_schema();
        p
    },
    ["id", "expected_version", "meal"]
);
tool!(
    CalorieRemove,
    "calorie_remove",
    Remove,
    calorie_remove,
    "Soft-delete a food entry using its current version. Deleted entries disappear from history and totals; stale versions conflict.",
    mutation_properties(),
    ["id", "expected_version"]
);
