//! The Google Health tools: `health_status`, `health_summary` and
//! `health_sync_now`. They act for the owner of the host's session, and are
//! registered whether or not Google Health is configured: without it they
//! say so.
//!
//! What `health_summary` returns is the user's personal health data. It goes
//! to the model and is stored in the conversation like any tool result.
use super::sync::{Health, Outcome};
use crate::scheduler::{Clock, SystemClock};
use crate::store::Store;
use crate::timezone::{NoArgs, for_owner};
use anyhow::{Result, ensure};
use jiff::{ToSpan, civil::Date, tz::TimeZone};
use rig_agent::{
    agent::{AgentBuilder, WithBuilderTools},
    tool::{Tool, ToolContext, ToolExecutionError},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Days `health_summary` returns when none are asked for.
pub const DEFAULT_DAYS: i64 = 7;
/// The most days it returns.
pub const MAX_DAYS: i64 = 30;

const NOT_CONFIGURED: &str = "Google Health is not set up on this server.";
const CONNECT: &str = "The user connects with /connect_health in their Telegram chat with the bot.";

#[derive(Clone)]
struct Ctx {
    store: Store,
    health: Option<Arc<Health>>,
    clock: Arc<dyn Clock>,
}

/// Add the three tools. `health` is `None` when Google Health is not
/// configured.
pub fn register(
    builder: AgentBuilder<WithBuilderTools>,
    store: Store,
    health: Option<Arc<Health>>,
) -> AgentBuilder<WithBuilderTools> {
    let clock = health.as_ref().map_or_else(
        || Arc::new(SystemClock) as Arc<dyn Clock>,
        |h| h.shared_clock(),
    );
    register_with_clock(builder, store, health, clock)
}

/// [`register`], reading today's date from `clock`: tests set it.
pub fn register_with_clock(
    builder: AgentBuilder<WithBuilderTools>,
    store: Store,
    health: Option<Arc<Health>>,
    clock: Arc<dyn Clock>,
) -> AgentBuilder<WithBuilderTools> {
    let ctx = Ctx {
        store,
        health,
        clock,
    };
    builder
        .tool(Status(ctx.clone()))
        .tool(Summary(ctx.clone()))
        .tool(SyncNow(ctx))
}

fn when(at: jiff::Timestamp, zone: &TimeZone) -> String {
    at.to_zoned(zone.clone())
        .strftime("%Y-%m-%dT%H:%M%:z")
        .to_string()
}

/// One stored day as the model sees it: sleep as minutes asleep and minutes
/// per stage, without the heart-rate zone limits and clock times.
fn compact(date: &str, metrics: &str) -> Map<String, Value> {
    let mut day: Map<String, Value> = serde_json::from_str(metrics).unwrap_or_default();
    day.remove("hr_zones");
    if let Some(sleeps) = day.remove("sleep").as_ref().and_then(Value::as_array) {
        let mut stages: std::collections::BTreeMap<String, i64> = Default::default();
        let mut total = 0;
        for sleep in sleeps {
            total += sleep["minutes"].as_i64().unwrap_or(0);
            for stage in sleep["stages"].as_array().into_iter().flatten() {
                let kind = stage["type"].as_str().unwrap_or_default().to_string();
                *stages.entry(kind).or_default() += stage["minutes"].as_i64().unwrap_or(0);
            }
        }
        day.insert("sleep_min".into(), total.into());
        if !stages.is_empty() {
            day.insert("sleep_stage_min".into(), json!(stages));
        }
    }
    if let Some(m) = day.remove("distance_m").and_then(|m| m.as_f64()) {
        day.insert("distance_km".into(), json!((m / 10.0).round() / 100.0));
    }
    let mut out = Map::new();
    out.insert("date".into(), date.into());
    out.extend(day);
    out
}

/// The metric names a day can carry, and so the names `metrics` accepts.
/// Objects hold their parts: `heart_rate` is `{min, avg, max, n}`,
/// `nutrition` the day's totals, `moods` a count and labels.
pub const METRICS: [&str; 40] = [
    "steps",
    "floors",
    "distance_km",
    "elevation_change_m",
    "active_kcal",
    "basal_kcal",
    "active_min",
    "zone_min",
    "sedentary_min",
    "activity_level_min",
    "swim",
    "hydration_ml",
    "nutrition",
    "heart_rate",
    "hrv_ms",
    "spo2_pct",
    "resp_rate_sleep",
    "glucose_mgdl",
    "core_temp_c",
    "resting_hr",
    "hrv_daily",
    "spo2_daily",
    "vo2max_daily",
    "resp_rate_daily",
    "sleep_temp",
    "hr_zone_minutes",
    "weight_kg",
    "body_fat_pct",
    "height_cm",
    "vo2max",
    "run_vo2max",
    "workouts",
    "sleep_min",
    "sleep_stage_min",
    "ecg",
    "irn",
    "moods",
    "symptoms",
    "ovulation_tests",
    "menstrual_period_started",
];

/// What is averaged over the days that have it: (name, JSON pointer into a
/// day).
const AVERAGED: [(&str, &str); 11] = [
    ("steps", "/steps"),
    ("active_min", "/active_min"),
    ("zone_min", "/zone_min"),
    ("resting_hr", "/resting_hr"),
    ("sleep_min", "/sleep_min"),
    ("floors", "/floors"),
    ("active_kcal", "/active_kcal"),
    ("hrv_ms", "/hrv_daily/avg_ms"),
    ("spo2_pct", "/spo2_daily/avg"),
    ("resp_rate", "/resp_rate_daily"),
    ("heart_rate_avg", "/heart_rate/avg"),
];

/// The most bytes `health_summary` returns, with room under
/// [`crate::policy::MAX_RESULT_BYTES`] for the envelope around it.
pub const SUMMARY_BYTES: usize = 56 * 1024;

/// The days and their averages, for `rows` of (date, metrics JSON).
pub fn summarize(rows: &[(String, String)]) -> Value {
    summarize_metrics(rows, &[])
}

/// [`summarize`], keeping only `metrics` (all if empty) and no more than
/// [`SUMMARY_BYTES`]: if the days do not fit, the oldest are left out and
/// `truncated_days` says how many.
pub fn summarize_metrics(rows: &[(String, String)], metrics: &[String]) -> Value {
    let mut maps: Vec<Map<String, Value>> = rows.iter().map(|(d, m)| compact(d, m)).collect();
    if !metrics.is_empty() {
        for map in &mut maps {
            // A day keeps its date and the note of what was dropped.
            map.retain(|k, _| {
                let owner = k.strip_suffix("_omitted").unwrap_or(k);
                k == "date" || k == "dropped" || metrics.iter().any(|m| m == k || m == owner)
            });
        }
    }
    let mut days: Vec<Value> = maps.into_iter().map(Value::Object).collect();
    let mut out = json!({"days": days, "averages": averages(&days)});
    let mut cut = 0;
    while out.to_string().len() > SUMMARY_BYTES && days.len() > 1 {
        days.remove(0);
        cut += 1;
        out = json!({"days": days, "averages": averages(&days)});
    }
    if cut > 0 {
        out["truncated_days"] = cut.into();
    }
    out
}

fn averages(days: &[Value]) -> Value {
    let mut averages = Map::new();
    for (name, pointer) in AVERAGED {
        let values: Vec<f64> = days
            .iter()
            .filter_map(|d| d.pointer(pointer)?.as_f64())
            .collect();
        if !values.is_empty() {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            averages.insert(
                name.into(),
                json!({"average": (mean * 10.0).round() / 10.0, "days_with_data": values.len()}),
            );
        }
    }
    Value::Object(averages)
}

/// The `days` local days ending `today`, as inclusive dates.
fn range(today: Date, days: i64) -> (String, String) {
    let start = today.saturating_sub((days - 1).days());
    (start.to_string(), today.to_string())
}

struct Status(Ctx);

impl Tool for Status {
    const NAME: &'static str = "health_status";
    type Args = NoArgs;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "Whether the user's Google Health (Fitbit-compatible sleep, activity and heart-rate \
         data) is connected, when it last synced and whether the last sync failed."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn call(&self, context: &mut ToolContext, _: NoArgs) -> Result<Value, Self::Error> {
        let configured = self.0.health.is_some();
        for_owner(&self.0.store, context, move |store, owner| {
            let Some(c) = store.health_connection(owner)? else {
                let hint = if configured { CONNECT } else { NOT_CONFIGURED };
                return Ok(
                    json!({"configured": configured, "status": "not_connected", "hint": hint}),
                );
            };
            let zone = store.timezone(owner)?;
            let mut out = json!({
                "configured": configured,
                "status": c.status,
                "connected_at": when(c.connected_at, &zone),
                "last_synced_at": c.last_synced_at.map(|t| when(t, &zone)),
                "last_sync_error": c.last_error,
            });
            if c.status == "revoked" {
                out["hint"] = format!("Google no longer accepts the connection. {CONNECT}").into();
            }
            Ok(out)
        })
        .await
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryArgs {
    pub days: Option<i64>,
    /// Names from [`METRICS`]; all of them if absent or empty.
    pub metrics: Option<Vec<String>>,
}

struct Summary(Ctx);

impl Tool for Summary {
    const NAME: &'static str = "health_summary";
    type Args = SummaryArgs;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        format!(
            "The user's synced Google Health data for the last N local days (1 to {MAX_DAYS}, \
             default {DEFAULT_DAYS}), oldest first, today included, as per-day totals and \
             summaries, plus averages: steps, floors, distance_km, active_min, zone_min \
             (active zone minutes), active_kcal, basal_kcal, resting_hr, sleep_min and \
             sleep_stage_min, workouts, weight_kg, body_fat_pct, hr_zone_minutes, and, when the \
             user's device and consent give them, heart_rate and hrv_ms ({{min, avg, max, n}}), \
             spo2_pct, spo2_daily, hrv_daily, resp_rate_daily, sleep_temp, vo2max_daily, \
             glucose_mgdl, core_temp_c, nutrition, hydration_ml, ecg, irn, moods, symptoms and \
             more. Pass `metrics` to get only some. A key is missing when Google reported \
             nothing for it that day; a `dropped` list names what was left out of a day for \
             size. Data is synced once a day, so today can be partial. Use it for recovery, \
             sleep and activity context when advising on training."
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
               "days":{"type":"integer",
                       "description":"Local days to return, 1 to 30; default 7"},
               "metrics":{"type":"array","items":{"type":"string","enum":METRICS.as_slice()},
                          "description":"Only these metrics (default: all). Ask for a few \
                                         when asking for many days."}},
               "additionalProperties":false})
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        args: SummaryArgs,
    ) -> Result<Value, Self::Error> {
        let days = args.days.unwrap_or(DEFAULT_DAYS);
        let metrics = args.metrics.unwrap_or_default();
        let (configured, now) = (self.0.health.is_some(), self.0.clock.now());
        for_owner(&self.0.store, context, move |store, owner| {
            ensure!(
                (1..=MAX_DAYS).contains(&days),
                "days must be between 1 and {MAX_DAYS}"
            );
            if let Some(unknown) = metrics.iter().find(|m| !METRICS.contains(&m.as_str())) {
                anyhow::bail!(
                    "unknown metric `{unknown}`; the metrics are {}",
                    METRICS.join(", ")
                );
            }
            let Some(c) = store.health_connection(owner)? else {
                let hint = if configured { CONNECT } else { NOT_CONFIGURED };
                return Ok(json!({"status": "not_connected", "hint": hint}));
            };
            let zone = store.timezone(owner)?;
            let today = now.to_zoned(zone.clone()).date();
            let (start, end) = range(today, days);
            let mut out = summarize_metrics(&store.health_days(owner, &start, &end)?, &metrics);
            out["status"] = c.status.clone().into();
            out["today"] = today.to_string().into();
            out["last_synced_at"] = c.last_synced_at.map(|t| when(t, &zone)).into();
            if c.status == "revoked" {
                out["hint"] = format!(
                    "Google no longer accepts the connection, so this is old data. {CONNECT}"
                )
                .into();
            }
            Ok(out)
        })
        .await
    }
}

struct SyncNow(Ctx);

impl Tool for SyncNow {
    const NAME: &'static str = "health_sync_now";
    type Args = NoArgs;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "Fetch the user's latest Google Health data now, instead of waiting for the daily sync. \
         Allowed once an hour; only when the user asks, or when health_summary is clearly stale."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn call(&self, context: &mut ToolContext, _: NoArgs) -> Result<Value, Self::Error> {
        let Some(health) = self.0.health.clone() else {
            return Err(ToolExecutionError::other(NOT_CONFIGURED));
        };
        let owner = for_owner(&self.0.store, context, |_, owner| Ok(owner)).await?;
        let outcome = health.sync_manual(owner).await;
        Ok(describe(&outcome, health.now()))
    }
}

/// What a sync's outcome tells the model.
fn describe(outcome: &Outcome, now: jiff::Timestamp) -> Value {
    match outcome {
        Outcome::Synced { days, unavailable } => {
            json!({"status": "synced", "days_synced": days, "unavailable_data_types": unavailable})
        }
        Outcome::Revoked => json!({"status": "revoked", "hint":
            format!("Google no longer accepts the connection. Tell the user to reconnect. {CONNECT}")}),
        Outcome::AlreadyRevoked => json!({"status": "revoked", "hint":
            format!("The connection was revoked earlier. {CONNECT}")}),
        Outcome::NotConnected => json!({"status": "not_connected", "hint": CONNECT}),
        Outcome::Cooldown(until) => {
            let minutes = (until.as_second() - now.as_second() + 59).max(0) / 60;
            json!({"status": "cooldown", "retry_after_minutes": minutes})
        }
        Outcome::Running => json!({"status": "already_running"}),
        Outcome::Failed(why) => json!({"status": "failed", "error": why}),
    }
}

#[cfg(test)]
mod tests;
