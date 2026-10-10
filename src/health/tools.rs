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
fn compact(date: &str, metrics: &str) -> Value {
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
    Value::Object(out)
}

/// The days and their averages, for `rows` of (date, metrics JSON).
pub fn summarize(rows: &[(String, String)]) -> Value {
    let days: Vec<Value> = rows.iter().map(|(d, m)| compact(d, m)).collect();
    let mut averages = Map::new();
    for key in ["steps", "active_min", "zone_min", "resting_hr", "sleep_min"] {
        let values: Vec<f64> = days.iter().filter_map(|d| d[key].as_f64()).collect();
        if !values.is_empty() {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            averages.insert(
                key.into(),
                json!({"average": (mean * 10.0).round() / 10.0, "days_with_data": values.len()}),
            );
        }
    }
    json!({"days": days, "averages": averages})
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
             default {DEFAULT_DAYS}), oldest first, today included: steps, distance_km, \
             active_min, zone_min (active zone minutes), active_kcal, resting_hr, sleep_min and \
             sleep_stage_min, workouts, weight_kg, body_fat_pct, hr_zone_minutes, plus averages. \
             A key is missing when Google reported nothing for it that day. Data is synced once \
             a day, so today can be partial. Use it for recovery, sleep and activity context \
             when advising on training."
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"days":{"type":"integer",
               "description":"Local days to return, 1 to 30; default 7"}},
               "additionalProperties":false})
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        args: SummaryArgs,
    ) -> Result<Value, Self::Error> {
        let days = args.days.unwrap_or(DEFAULT_DAYS);
        let (configured, now) = (self.0.health.is_some(), self.0.clock.now());
        for_owner(&self.0.store, context, move |store, owner| {
            ensure!(
                (1..=MAX_DAYS).contains(&days),
                "days must be between 1 and {MAX_DAYS}"
            );
            let Some(c) = store.health_connection(owner)? else {
                let hint = if configured { CONNECT } else { NOT_CONFIGURED };
                return Ok(json!({"status": "not_connected", "hint": hint}));
            };
            let zone = store.timezone(owner)?;
            let today = now.to_zoned(zone.clone()).date();
            let (start, end) = range(today, days);
            let mut out = summarize(&store.health_days(owner, &start, &end)?);
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
