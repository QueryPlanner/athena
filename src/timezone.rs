//! The user's time zone and the current time in it, as tools.
//!
//! The model has no clock. `now` gives it the user's date, time and weekday;
//! `timezone_set` changes the zone. Both act for the owner of the host's
//! session ([`Conversation`]), never for a user the model names.
use crate::{runner::Conversation, store::Store};
use anyhow::{Result, anyhow, ensure};
use jiff::{Timestamp, Zoned, tz::TimeZone};
use rig_agent::{
    agent::{AgentBuilder, WithBuilderTools},
    tool::{Tool, ToolContext, ToolExecutionError},
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The zone of a user who never set one. Part of the contract: see the
/// note on migration 10 in `store.rs` before changing it.
pub const DEFAULT_TIMEZONE: &str = "Asia/Kolkata";

pub const NAMES: [&str; 2] = [Now::NAME, TimezoneSet::NAME];

/// Resolve an IANA time zone name, such as `America/New_York` or `UTC`.
///
/// Lookup ignores case. Names that are not a place (`localtime`,
/// `posixrules`, `EST`, the `posix/` and `right/` copies) and fixed offsets
/// such as `+05:30` are refused: a fixed offset gets daylight saving wrong.
pub fn parse(name: &str) -> Result<TimeZone> {
    let refused = || {
        anyhow!(
            "`{name}` is not an IANA time zone name such as Asia/Kolkata or \
             America/New_York; ask the user which city's time they keep"
        )
    };
    let lower = name.to_ascii_lowercase();
    let place = lower == "utc"
        || (lower.contains('/') && !lower.starts_with("posix/") && !lower.starts_with("right/"));
    ensure!(
        place && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_graphic()),
        refused()
    );
    jiff::tz::db().get(name).map_err(|_| refused())
}

/// The database name of a zone [`parse`] returned.
pub fn name(zone: &TimeZone) -> &str {
    zone.iana_name().unwrap_or_default()
}

/// What `now` reports: the wall-clock time, its date and weekday, the zone
/// and its current offset from UTC.
pub fn describe(now: &Zoned) -> Value {
    json!({
        "datetime": now.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        "date": now.date().to_string(),
        "weekday": now.strftime("%A").to_string(),
        "timezone": name(now.time_zone()),
        "utc_offset": now.strftime("%:z").to_string(),
    })
}

pub fn register(
    builder: AgentBuilder<WithBuilderTools>,
    store: Store,
) -> AgentBuilder<WithBuilderTools> {
    builder.tool(Now(store.clone())).tool(TimezoneSet(store))
}

/// Run `f` for the owner of the host's session, off the async executor.
async fn for_owner<T: Send + 'static>(
    store: &Store,
    context: &mut ToolContext,
    f: impl FnOnce(&Store, i64) -> Result<T> + Send + 'static,
) -> Result<T, ToolExecutionError> {
    let session = context.require::<Conversation>()?.0.clone();
    store
        .call(move |store| {
            let owner = store
                .session_owner(&session)?
                .ok_or_else(|| anyhow!("unknown session"))?;
            f(store, owner)
        })
        .await
        .map_err(|e| ToolExecutionError::other(e.to_string()))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

pub struct Now(pub Store);
impl Tool for Now {
    const NAME: &'static str = "now";
    type Args = NoArgs;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "The user's current date, time, weekday and IANA time zone. Call it before \
         reasoning about today, yesterday, weekdays or any date relative to now."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn call(&self, context: &mut ToolContext, _: NoArgs) -> Result<Value, Self::Error> {
        for_owner(&self.0, context, |store, owner| {
            Ok(describe(&store.local_now(owner, Timestamp::now())?))
        })
        .await
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTimezone {
    pub timezone: String,
}

pub struct TimezoneSet(pub Store);
impl Tool for TimezoneSet {
    const NAME: &'static str = "timezone_set";
    type Args = SetTimezone;
    type Output = Value;
    type Error = ToolExecutionError;
    fn description(&self) -> String {
        "Set the user's time zone to an IANA name such as Asia/Kolkata or Europe/London, \
         when they tell you where they are or which time they keep. Returns the zone it \
         replaced and the current time in the new one."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"timezone":{"type":"string"}},
               "required":["timezone"],"additionalProperties":false})
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        args: SetTimezone,
    ) -> Result<Value, Self::Error> {
        for_owner(&self.0, context, move |store, owner| {
            let (previous, zone) = store.set_timezone(owner, &args.timezone)?;
            Ok(json!({
                "previous_timezone": previous,
                "now": describe(&Timestamp::now().to_zoned(zone)),
            }))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str, zone: &str) -> Value {
        describe(
            &rfc3339
                .parse::<Timestamp>()
                .unwrap()
                .to_zoned(parse(zone).unwrap()),
        )
    }

    #[test]
    fn describes_the_wall_clock_in_the_users_zone() {
        assert_eq!(
            at("2026-10-09T20:00:00Z", "Asia/Kolkata"),
            json!({"datetime": "2026-10-10T01:30:00+05:30", "date": "2026-10-10",
                   "weekday": "Saturday", "timezone": "Asia/Kolkata", "utc_offset": "+05:30"})
        );
        // The same instant is still the previous day in New York.
        assert_eq!(
            at("2026-10-09T20:00:00Z", "America/New_York")["date"],
            "2026-10-09"
        );
        assert_eq!(at("2026-10-09T20:00:00Z", "UTC")["utc_offset"], "+00:00");
    }

    #[test]
    fn follows_daylight_saving() {
        // New York leaves daylight saving at 06:00 UTC on 2026-11-01.
        let before = at("2026-11-01T05:59:59Z", "America/New_York");
        let after = at("2026-11-01T06:00:00Z", "America/New_York");
        assert_eq!(before["datetime"], "2026-11-01T01:59:59-04:00");
        assert_eq!(after["datetime"], "2026-11-01T01:00:00-05:00");
    }

    #[test]
    fn accepts_iana_names_under_their_database_spelling() {
        assert_eq!(name(&parse("Asia/Kolkata").unwrap()), "Asia/Kolkata");
        assert_eq!(
            name(&parse("america/new_york").unwrap()),
            "America/New_York"
        );
        assert_eq!(name(&parse("utc").unwrap()), "UTC");
        // A backward-compatible alias resolves and keeps its own name.
        assert_eq!(name(&parse("Asia/Calcutta").unwrap()), "Asia/Calcutta");
        assert_eq!(name(&parse(DEFAULT_TIMEZONE).unwrap()), DEFAULT_TIMEZONE);
    }

    /// A host without /usr/share/zoneinfo falls back to the copy built in
    /// by `tzdb-bundle-always`; without that feature this would be empty.
    #[test]
    fn the_bundled_database_is_compiled_in() {
        let bundled = jiff::tz::TimeZoneDatabase::bundled();
        assert_eq!(
            bundled.get(DEFAULT_TIMEZONE).unwrap().iana_name(),
            Some(DEFAULT_TIMEZONE)
        );
    }

    #[test]
    fn refuses_what_is_not_a_place() {
        for bad in [
            "",
            "+05:30",
            "UTC+5",
            "IST",
            "EST",
            "localtime",
            "posixrules",
            "Factory",
            "posix/Asia/Kolkata",
            "right/UTC",
            "Mars/Olympus_Mons",
            "Asia/ Kolkata",
            "Asia/Kolkata\n",
            "../../etc/passwd",
            &format!("Asia/{}", "x".repeat(64)),
        ] {
            let e = parse(bad).unwrap_err().to_string();
            assert!(e.contains("not an IANA time zone name"), "{bad}: {e}");
        }
    }
}
