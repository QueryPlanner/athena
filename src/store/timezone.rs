//! Each user's time zone, and the current time and date in it.
use super::{Store, now_millis};
use crate::timezone::{self, DEFAULT_TIMEZONE};
use anyhow::{Context, Result};
use jiff::{Timestamp, Zoned, civil::Date, tz::TimeZone};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// The zone name stored for `owner`, or the default when they never set one.
fn stored(db: &Connection, owner: i64) -> Result<String> {
    let name: Option<String> = db
        .query_row(
            "SELECT timezone FROM user_settings WHERE user_id = ?1",
            [owner],
            |r| r.get(0),
        )
        .optional()?;
    Ok(name.unwrap_or_else(|| DEFAULT_TIMEZONE.into()))
}

impl Store {
    /// The user's time zone: the one they set with `timezone_set`, else
    /// [`DEFAULT_TIMEZONE`].
    ///
    /// Fails, rather than guessing, if the stored name no longer resolves
    /// (the host's time zone database lost it).
    pub fn timezone(&self, owner: i64) -> Result<TimeZone> {
        let name = stored(&self.db(), owner)?;
        timezone::parse(&name).with_context(|| {
            format!("stored time zone {name} no longer resolves; set it again with timezone_set")
        })
    }

    /// Store `name` as the user's time zone. Returns the name it replaced
    /// (the default if they had none) and the zone now in force, under its
    /// database name. An invalid name writes nothing. The user's repeating
    /// reminders next run at their time of day in the new zone.
    pub fn set_timezone(&self, owner: i64, name: &str) -> Result<(String, TimeZone)> {
        let zone = timezone::parse(name)?;
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = stored(&tx, owner)?;
        tx.execute(
            "INSERT INTO user_settings (user_id, timezone, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id) DO UPDATE SET timezone = ?2, updated_at = ?3",
            params![owner, timezone::name(&zone), now_millis()],
        )?;
        // Repeating reminders keep their wall-clock time in the new zone.
        Store::reschedule(&tx, owner, &zone, Timestamp::now())?;
        tx.commit()?;
        Ok((previous, zone))
    }

    /// The instant `at` on the user's wall clock.
    pub fn local_now(&self, owner: i64, at: Timestamp) -> Result<Zoned> {
        Ok(at.to_zoned(self.timezone(owner)?))
    }

    /// The user's calendar date at the instant `at`: what "today" means to
    /// them. Every feature that needs the user's today (workouts, reminders,
    /// summaries) uses this with `Timestamp::now()`, so the date is computed
    /// here and never guessed by the model.
    pub fn today(&self, owner: i64, at: Timestamp) -> Result<Date> {
        Ok(self.local_now(owner, at)?.date())
    }
}
