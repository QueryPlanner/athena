//! SQLite operations for the Google Health connection: pending consent
//! links, the stored (encrypted) refresh token, sync bookkeeping and the
//! synced days. Times come in as arguments so tests and the scheduler own
//! the clock.
use super::Store;
use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

/// The longest `last_sync_error` kept, in characters.
const ERROR_CHARS: usize = 200;

fn ms(at: Timestamp) -> i64 {
    at.as_millisecond()
}

fn instant(ms: i64) -> Result<Timestamp> {
    Timestamp::from_millisecond(ms).context("a stored time is out of range")
}

fn short(why: &str) -> String {
    why.chars().take(ERROR_CHARS).collect()
}

/// A user's stored connection. `token` is the sealed refresh token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// `connected` or `revoked`.
    pub status: String,
    pub token: Vec<u8>,
    pub scopes: String,
    pub connected_at: Timestamp,
    pub last_synced_at: Option<Timestamp>,
    pub last_attempt_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

/// A connected user, as the daily schedule sees them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub owner: i64,
    /// Their Telegram chat (user id), to say something to them.
    pub chat: Option<i64>,
    pub last_attempt: Option<Timestamp>,
    /// Whether the last attempt failed.
    pub failed: bool,
}

/// One Google Health data point to store: Google's JSON, whole, and what
/// is needed to find it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointRow {
    pub key: String,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    /// The user's local day, `YYYY-MM-DD`.
    pub civil_date: Option<String>,
    pub value: String,
    pub source: Option<String>,
}

/// How far the history fetch of one data type has got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backfill {
    pub data_type: String,
    /// The earliest local day fetched so far, `YYYY-MM-DD`.
    pub oldest: String,
    /// Nothing older is looked for.
    pub done: bool,
    /// Chunks in a row that held no data.
    pub empty_run: i64,
}

/// What claiming a manual sync came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Granted,
    /// The last attempt was too recent; the earliest time to try again.
    Cooldown(Timestamp),
    /// The user has no connection.
    NotConnected,
    /// The connection was revoked.
    Revoked,
}

impl Store {
    /// Remember a consent link's `state` (by hash) for `owner` until
    /// `expires`. The owner's earlier links, and every expired one, are
    /// dropped: one live link per user.
    pub fn health_state_create(
        &self,
        owner: i64,
        hash: &str,
        now: Timestamp,
        expires: Timestamp,
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM health_oauth_states WHERE user_id = ?1 OR expires_at <= ?2",
            params![owner, ms(now)],
        )?;
        tx.execute(
            "INSERT INTO health_oauth_states (state_hash, user_id, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![hash, owner, ms(expires), ms(now)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Use up the link `hash` if it is `owner`'s and has not expired. One
    /// statement, so two uses cannot both succeed. Another user's link is
    /// left alone.
    pub fn health_state_consume(&self, owner: i64, hash: &str, now: Timestamp) -> Result<bool> {
        let used = self.db().execute(
            "DELETE FROM health_oauth_states
             WHERE state_hash = ?1 AND user_id = ?2 AND expires_at > ?3",
            params![hash, owner, ms(now)],
        )?;
        Ok(used == 1)
    }

    /// Store a freshly authorised connection, replacing any earlier one
    /// (and clearing its sync bookkeeping, so the first sync is due now).
    pub fn health_connect(
        &self,
        owner: i64,
        token: &[u8],
        scopes: &str,
        now: Timestamp,
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO health_connections
                 (user_id, encrypted_refresh_token, scopes, status, connected_at, updated_at)
             VALUES (?1, ?2, ?3, 'connected', ?4, ?4)
             ON CONFLICT (user_id) DO UPDATE SET
                 encrypted_refresh_token = ?2, scopes = ?3, status = 'connected',
                 connected_at = ?4, last_attempt_at = NULL, last_sync_error = NULL,
                 updated_at = ?4",
            params![owner, token, scopes, ms(now)],
        )?;
        // New scopes may open data types that were refused: look for history
        // again from the start. Stored points are not duplicated by that.
        tx.execute("DELETE FROM health_backfill WHERE user_id = ?1", [owner])?;
        tx.commit()?;
        Ok(())
    }

    /// The user's connection, if they ever made one.
    pub fn health_connection(&self, owner: i64) -> Result<Option<Connection>> {
        let row = self
            .db()
            .query_row(
                "SELECT status, encrypted_refresh_token, scopes, connected_at, last_synced_at,
                        last_attempt_at, last_sync_error
                 FROM health_connections WHERE user_id = ?1",
                [owner],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(status, token, scopes, connected, synced, attempt, last_error)| {
                Ok(Connection {
                    status,
                    token,
                    scopes,
                    connected_at: instant(connected)?,
                    last_synced_at: synced.map(instant).transpose()?,
                    last_attempt_at: attempt.map(instant).transpose()?,
                    last_error,
                })
            },
        )
        .transpose()
    }

    /// Delete the user's connection (and so the sealed token) and their
    /// pending links. The synced days stay. Returns the sealed token that
    /// was stored, or `None` if there was no connection.
    pub fn health_disconnect(&self, owner: i64) -> Result<Option<Vec<u8>>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let token = tx
            .query_row(
                "SELECT encrypted_refresh_token FROM health_connections WHERE user_id = ?1",
                [owner],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute("DELETE FROM health_connections WHERE user_id = ?1", [owner])?;
        tx.execute(
            "DELETE FROM health_oauth_states WHERE user_id = ?1",
            [owner],
        )?;
        tx.commit()?;
        Ok(token)
    }

    /// Start a manual sync if the last attempt, of any kind, was at least
    /// `cooldown` ago. Records the attempt.
    pub fn health_claim_manual(
        &self,
        owner: i64,
        now: Timestamp,
        cooldown: SignedDuration,
    ) -> Result<Claim> {
        let changed = self.db().execute(
            "UPDATE health_connections SET last_attempt_at = ?2, updated_at = ?2
             WHERE user_id = ?1 AND status = 'connected'
               AND (last_attempt_at IS NULL OR last_attempt_at <= ?3)",
            params![owner, ms(now), ms(now - cooldown)],
        )?;
        if changed == 1 {
            return Ok(Claim::Granted);
        }
        Ok(match self.health_connection(owner)? {
            None => Claim::NotConnected,
            Some(c) if c.status == "connected" => {
                let last = c.last_attempt_at.expect("a refused claim has an attempt");
                Claim::Cooldown(last + cooldown)
            }
            Some(_) => Claim::Revoked,
        })
    }

    /// Start the scheduled sync if the last attempt is still the one the
    /// scheduler saw (`seen`): of two passes, only one wins.
    pub fn health_claim_scheduled(
        &self,
        owner: i64,
        seen: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE health_connections SET last_attempt_at = ?2, updated_at = ?2
             WHERE user_id = ?1 AND status = 'connected' AND last_attempt_at IS ?3",
            params![owner, ms(now), seen.map(ms)],
        )?;
        Ok(changed == 1)
    }

    /// The connected users, by id.
    pub fn health_candidates(&self) -> Result<Vec<Candidate>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT c.user_id,
                    (SELECT external_id FROM user_identities
                     WHERE user_id = c.user_id AND transport = 'telegram'
                     ORDER BY created_at, rowid LIMIT 1),
                    c.last_attempt_at, c.last_sync_error IS NOT NULL
             FROM health_connections c WHERE c.status = 'connected' ORDER BY c.user_id",
        )?;
        let rows = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(owner, chat, attempt, failed)| {
                Ok(Candidate {
                    owner,
                    chat: chat
                        .map(|id| id.parse().context("a Telegram user id is not a number"))
                        .transpose()?,
                    last_attempt: attempt.map(instant).transpose()?,
                    failed,
                })
            })
            .collect()
    }

    /// Mark the connection revoked, keeping its sealed token. Returns
    /// whether this call did it: `false` if it already was, or is gone.
    pub fn health_mark_revoked(&self, owner: i64, why: &str, now: Timestamp) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE health_connections
             SET status = 'revoked', last_sync_error = ?2, updated_at = ?3
             WHERE user_id = ?1 AND status = 'connected'",
            params![owner, short(why), ms(now)],
        )?;
        Ok(changed == 1)
    }

    /// Record why the last sync failed.
    pub fn health_mark_failed(&self, owner: i64, why: &str, now: Timestamp) -> Result<()> {
        self.db().execute(
            "UPDATE health_connections SET last_sync_error = ?2, updated_at = ?3
             WHERE user_id = ?1 AND status = 'connected'",
            params![owner, short(why), ms(now)],
        )?;
        Ok(())
    }

    /// Replace the sealed token of a connected user (Google rotated it).
    pub fn health_update_token(&self, owner: i64, token: &[u8], now: Timestamp) -> Result<()> {
        self.db().execute(
            "UPDATE health_connections SET encrypted_refresh_token = ?2, updated_at = ?3
             WHERE user_id = ?1 AND status = 'connected'",
            params![owner, token, ms(now)],
        )?;
        Ok(())
    }

    /// Save a finished sync: the days from `start` up to but not including
    /// `end` become exactly `days` (date, metrics JSON), and the connection
    /// is marked synced. Writes nothing and returns `false` if the user
    /// disconnected or was revoked while it ran.
    pub fn health_store_sync(
        &self,
        owner: i64,
        start: &str,
        end: &str,
        days: &[(String, String)],
        now: Timestamp,
    ) -> Result<bool> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let marked = tx.execute(
            "UPDATE health_connections
             SET last_synced_at = ?2, last_sync_error = NULL, updated_at = ?2
             WHERE user_id = ?1 AND status = 'connected'",
            params![owner, ms(now)],
        )?;
        if marked != 1 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM health_daily WHERE user_id = ?1 AND date >= ?2 AND date < ?3",
            params![owner, start, end],
        )?;
        for (date, metrics) in days {
            tx.execute(
                "INSERT INTO health_daily (user_id, date, metrics, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![owner, date, metrics, ms(now)],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Store `rows` of `data_type` in one transaction. A point already
    /// stored (same key) is updated only if Google's version differs, so
    /// re-reading the recent window does not rewrite the table. Returns the
    /// number of rows inserted or changed.
    pub fn health_points_put(
        &self,
        owner: i64,
        data_type: &str,
        rows: &[PointRow],
        now: Timestamp,
    ) -> Result<usize> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut changed = 0;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO health_points
                     (user_id, data_type, point_key, start_ms, end_ms, civil_date, value,
                      source, ingested_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (user_id, data_type, point_key) DO UPDATE SET
                     start_ms = excluded.start_ms, end_ms = excluded.end_ms,
                     civil_date = excluded.civil_date, value = excluded.value,
                     source = excluded.source, ingested_at = excluded.ingested_at
                 WHERE health_points.value IS NOT excluded.value",
            )?;
            for row in rows {
                changed += put.execute(params![
                    owner,
                    data_type,
                    row.key,
                    row.start_ms,
                    row.end_ms,
                    row.civil_date,
                    row.value,
                    row.source,
                    ms(now),
                ])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Where the history fetch of each data type has got to for `owner`.
    pub fn health_backfill(&self, owner: i64) -> Result<Vec<Backfill>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT data_type, oldest_date, done, empty_run FROM health_backfill
             WHERE user_id = ?1",
        )?;
        let rows = q.query_map([owner], |r| {
            Ok(Backfill {
                data_type: r.get(0)?,
                oldest: r.get(1)?,
                done: r.get::<_, i64>(2)? == 1,
                empty_run: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record how far the history fetch of `progress.data_type` has got.
    /// `error` is why it stopped early, if it did.
    pub fn health_backfill_save(
        &self,
        owner: i64,
        progress: &Backfill,
        error: Option<&str>,
        now: Timestamp,
    ) -> Result<()> {
        self.db().execute(
            "INSERT INTO health_backfill
                 (user_id, data_type, oldest_date, done, empty_run, last_error, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (user_id, data_type) DO UPDATE SET
                 oldest_date = ?3, done = ?4, empty_run = ?5, last_error = ?6, updated_at = ?7",
            params![
                owner,
                progress.data_type,
                progress.oldest,
                progress.done as i64,
                progress.empty_run,
                error.map(short),
                ms(now)
            ],
        )?;
        Ok(())
    }

    /// The synced days from `start` to `end` inclusive, oldest first, as
    /// (date, metrics JSON).
    pub fn health_days(&self, owner: i64, start: &str, end: &str) -> Result<Vec<(String, String)>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT date, metrics FROM health_daily
             WHERE user_id = ?1 AND date >= ?2 AND date <= ?3 ORDER BY date",
        )?;
        let rows = q.query_map(params![owner, start, end], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn setup() -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let owner = store.user("telegram", "7").unwrap().id();
        (store, owner)
    }

    const NOW: &str = "2026-10-10T10:00:00Z";

    #[test]
    fn a_link_is_single_use_bound_to_its_user_and_expires() {
        let (store, owner) = setup();
        let other = store.user("telegram", "8").unwrap().id();
        let (now, later) = (at(NOW), at("2026-10-10T10:10:00Z"));
        store.health_state_create(owner, "h1", now, later).unwrap();
        // Another user cannot spend it, and failing leaves it usable.
        assert!(!store.health_state_consume(other, "h1", now).unwrap());
        assert!(!store.health_state_consume(owner, "nope", now).unwrap());
        assert!(store.health_state_consume(owner, "h1", now).unwrap());
        assert!(!store.health_state_consume(owner, "h1", now).unwrap());
        // Expired exactly at its expiry, and usable up to the millisecond before.
        store.health_state_create(owner, "h2", now, later).unwrap();
        let last_ms = later - SignedDuration::from_millis(1);
        assert!(!store.health_state_consume(owner, "h2", later).unwrap());
        assert!(store.health_state_consume(owner, "h2", last_ms).unwrap());
    }

    #[test]
    fn a_new_link_replaces_the_users_old_ones_and_clears_expired_links() {
        let (store, owner) = setup();
        let other = store.user("telegram", "8").unwrap().id();
        let (now, later) = (at(NOW), at("2026-10-10T10:10:00Z"));
        store.health_state_create(other, "x", now, now).unwrap();
        store.health_state_create(owner, "a", now, later).unwrap();
        store.health_state_create(owner, "b", now, later).unwrap();
        let hashes: Vec<String> = store
            .db_for_tests()
            .prepare("SELECT state_hash FROM health_oauth_states")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        // `x` had expired; `a` was replaced by `b`.
        assert_eq!(hashes, ["b"]);
    }

    #[test]
    fn connecting_stores_the_token_and_reconnecting_resets_the_attempt() {
        let (store, owner) = setup();
        assert_eq!(store.health_connection(owner).unwrap(), None);
        store
            .health_connect(owner, b"sealed1", "a b", at(NOW))
            .unwrap();
        store
            .health_claim_manual(owner, at(NOW), SignedDuration::from_hours(1))
            .unwrap();
        store.health_mark_failed(owner, "boom", at(NOW)).unwrap();
        let first = store.health_connection(owner).unwrap().unwrap();
        assert_eq!(first.token, b"sealed1");
        assert_eq!(first.scopes, "a b");
        assert_eq!(first.status, "connected");
        assert_eq!(first.last_attempt_at, Some(at(NOW)));
        assert_eq!(first.last_error.as_deref(), Some("boom"));

        let later = at("2026-10-11T10:00:00Z");
        store.health_connect(owner, b"sealed2", "a", later).unwrap();
        let again = store.health_connection(owner).unwrap().unwrap();
        assert_eq!(again.token, b"sealed2");
        assert_eq!(again.connected_at, later);
        assert_eq!((again.last_attempt_at, again.last_error), (None, None));
    }

    #[test]
    fn a_manual_claim_waits_out_the_cooldown_after_any_attempt() {
        let (store, owner) = setup();
        let hour = SignedDuration::from_hours(1);
        assert_eq!(
            store.health_claim_manual(owner, at(NOW), hour).unwrap(),
            Claim::NotConnected
        );
        store.health_connect(owner, b"t", "s", at(NOW)).unwrap();
        assert_eq!(
            store.health_claim_manual(owner, at(NOW), hour).unwrap(),
            Claim::Granted
        );
        let soon = at("2026-10-10T10:59:59Z");
        assert_eq!(
            store.health_claim_manual(owner, soon, hour).unwrap(),
            Claim::Cooldown(at("2026-10-10T11:00:00Z"))
        );
        let hourly = at("2026-10-10T11:00:00Z");
        assert_eq!(
            store.health_claim_manual(owner, hourly, hour).unwrap(),
            Claim::Granted
        );
        store.health_mark_revoked(owner, "gone", hourly).unwrap();
        let day = at("2026-10-12T00:00:00Z");
        assert_eq!(
            store.health_claim_manual(owner, day, hour).unwrap(),
            Claim::Revoked
        );
    }

    #[test]
    fn of_two_scheduled_claims_on_the_same_attempt_one_wins() {
        let (store, owner) = setup();
        store.health_connect(owner, b"t", "s", at(NOW)).unwrap();
        let (a, b) = (at("2026-10-11T05:30:00Z"), at("2026-10-11T05:30:30Z"));
        assert!(store.health_claim_scheduled(owner, None, a).unwrap());
        assert!(!store.health_claim_scheduled(owner, None, b).unwrap());
        assert!(store.health_claim_scheduled(owner, Some(a), b).unwrap());
    }

    #[test]
    fn candidates_are_the_connected_users_with_their_chat_and_failure() {
        let (store, owner) = setup();
        let other = store.user("telegram", "8").unwrap().id();
        let revoked = store.user("telegram", "9").unwrap().id();
        let cli = store.user("cli", "local").unwrap().id();
        for id in [owner, other, revoked, cli] {
            store.health_connect(id, b"t", "s", at(NOW)).unwrap();
        }
        store.health_mark_failed(other, "late", at(NOW)).unwrap();
        store.health_mark_revoked(revoked, "x", at(NOW)).unwrap();
        store.health_claim_scheduled(owner, None, at(NOW)).unwrap();
        let found = store.health_candidates().unwrap();
        let summary: Vec<_> = found
            .iter()
            .map(|c| (c.owner, c.chat, c.last_attempt, c.failed))
            .collect();
        // By user id: the CLI user is the first one in every database.
        assert_eq!(
            summary,
            [
                (cli, None, None, false),
                (owner, Some(7), Some(at(NOW)), false),
                (other, Some(8), None, true),
            ]
        );
    }

    #[test]
    fn revoking_happens_once_and_keeps_the_token() {
        let (store, owner) = setup();
        assert!(!store.health_mark_revoked(owner, "x", at(NOW)).unwrap());
        store
            .health_connect(owner, b"sealed", "s", at(NOW))
            .unwrap();
        let long = "e".repeat(500);
        assert!(store.health_mark_revoked(owner, &long, at(NOW)).unwrap());
        assert!(!store.health_mark_revoked(owner, "again", at(NOW)).unwrap());
        let c = store.health_connection(owner).unwrap().unwrap();
        assert_eq!(
            (c.status.as_str(), c.token.as_slice()),
            ("revoked", &b"sealed"[..])
        );
        assert_eq!(c.last_error.unwrap().len(), ERROR_CHARS);
        // A revoked connection takes no failure notes or new tokens.
        store.health_mark_failed(owner, "late", at(NOW)).unwrap();
        store.health_update_token(owner, b"new", at(NOW)).unwrap();
        let c = store.health_connection(owner).unwrap().unwrap();
        assert_eq!(c.token, b"sealed");
        assert_ne!(c.last_error.as_deref(), Some("late"));
    }

    #[test]
    fn a_rotated_token_replaces_the_stored_one() {
        let (store, owner) = setup();
        store.health_connect(owner, b"old", "s", at(NOW)).unwrap();
        store.health_update_token(owner, b"new", at(NOW)).unwrap();
        assert_eq!(
            store.health_connection(owner).unwrap().unwrap().token,
            b"new"
        );
    }

    fn day(date: &str, metrics: &str) -> (String, String) {
        (date.into(), metrics.into())
    }

    #[test]
    fn a_sync_replaces_its_window_and_nothing_else() {
        let (store, owner) = setup();
        let other = store.user("telegram", "8").unwrap().id();
        for id in [owner, other] {
            store.health_connect(id, b"t", "s", at(NOW)).unwrap();
        }
        let old = [day("2026-10-01", "{}"), day("2026-10-09", r#"{"steps":1}"#)];
        assert!(
            store
                .health_store_sync(owner, "2026-10-01", "2026-10-10", &old, at(NOW))
                .unwrap()
        );
        store
            .health_store_sync(
                other,
                "2026-10-01",
                "2026-10-10",
                &[day("2026-10-09", "{}")],
                at(NOW),
            )
            .unwrap();
        let later = at("2026-10-11T10:00:00Z");
        store.health_mark_failed(owner, "stale", later).unwrap();
        // The window now starts at the 5th: the 1st stays, the 9th is replaced
        // and the 10th is added.
        let fresh = [day("2026-10-09", r#"{"steps":2}"#), day("2026-10-10", "{}")];
        assert!(
            store
                .health_store_sync(owner, "2026-10-05", "2026-10-11", &fresh, later)
                .unwrap()
        );
        let days = store
            .health_days(owner, "2026-09-01", "2026-10-31")
            .unwrap();
        assert_eq!(
            days,
            [
                day("2026-10-01", "{}"),
                day("2026-10-09", r#"{"steps":2}"#),
                day("2026-10-10", "{}")
            ]
        );
        assert_eq!(
            store
                .health_days(owner, "2026-10-09", "2026-10-09")
                .unwrap(),
            [day("2026-10-09", r#"{"steps":2}"#)]
        );
        assert_eq!(
            store
                .health_days(other, "2026-10-01", "2026-10-31")
                .unwrap()
                .len(),
            1
        );
        let c = store.health_connection(owner).unwrap().unwrap();
        assert_eq!((c.last_synced_at, c.last_error), (Some(later), None));
    }

    #[test]
    fn a_sync_that_finishes_after_a_disconnect_or_revoke_writes_nothing() {
        let (store, owner) = setup();
        let days = [day("2026-10-10", "{}")];
        let write = |s: &Store| {
            s.health_store_sync(owner, "2026-10-01", "2026-10-11", &days, at(NOW))
                .unwrap()
        };
        assert!(!write(&store));
        store.health_connect(owner, b"t", "s", at(NOW)).unwrap();
        store.health_mark_revoked(owner, "x", at(NOW)).unwrap();
        assert!(!write(&store));
        store.health_disconnect(owner).unwrap();
        assert!(!write(&store));
        assert!(
            store
                .health_days(owner, "2026-01-01", "2026-12-31")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn disconnecting_returns_the_token_drops_links_and_keeps_the_days() {
        let (store, owner) = setup();
        assert_eq!(store.health_disconnect(owner).unwrap(), None);
        store
            .health_connect(owner, b"sealed", "s", at(NOW))
            .unwrap();
        store
            .health_store_sync(
                owner,
                "2026-10-01",
                "2026-10-11",
                &[day("2026-10-10", "{}")],
                at(NOW),
            )
            .unwrap();
        store
            .health_state_create(owner, "h", at(NOW), at("2026-10-10T10:10:00Z"))
            .unwrap();
        assert_eq!(
            store.health_disconnect(owner).unwrap(),
            Some(b"sealed".to_vec())
        );
        assert_eq!(store.health_connection(owner).unwrap(), None);
        assert!(!store.health_state_consume(owner, "h", at(NOW)).unwrap());
        assert_eq!(
            store
                .health_days(owner, "2026-10-01", "2026-10-31")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_stored_time_out_of_range_is_an_error() {
        let (store, owner) = setup();
        store.health_connect(owner, b"t", "s", at(NOW)).unwrap();
        store
            .db_for_tests()
            .execute(
                "UPDATE health_connections SET connected_at = 9223372036854775807",
                [],
            )
            .unwrap();
        let err = store.health_connection(owner).unwrap_err().to_string();
        assert!(err.contains("out of range"), "{err}");
    }
}

#[cfg(test)]
mod points_and_backfill_tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn setup() -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let owner = store.user("telegram", "7").unwrap().id();
        (store, owner)
    }

    const NOW: &str = "2026-10-10T10:00:00Z";
    const LATER: &str = "2026-10-11T10:00:00Z";

    fn point(key: &str, value: &str) -> PointRow {
        PointRow {
            key: key.into(),
            start_ms: Some(1),
            end_ms: Some(2),
            civil_date: Some("2026-10-09".into()),
            value: value.into(),
            source: None,
        }
    }

    /// `(point_key, value, ingested_at)` of every stored point of `data_type`.
    fn stored(store: &Store, data_type: &str) -> Vec<(String, String, i64)> {
        store
            .db_for_tests()
            .prepare(
                "SELECT point_key, value, ingested_at FROM health_points
                 WHERE data_type = ?1 ORDER BY point_key",
            )
            .unwrap()
            .query_map([data_type], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn storing_the_same_points_twice_changes_nothing_and_keeps_their_time() {
        let (store, owner) = setup();
        let rows = [point("a", "{}"), point("b", r#"{"x":1}"#)];
        assert_eq!(
            store
                .health_points_put(owner, "steps", &rows, at(NOW))
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .health_points_put(owner, "steps", &rows, at(LATER))
                .unwrap(),
            0
        );
        let now = at(NOW).as_millisecond();
        assert_eq!(
            stored(&store, "steps"),
            [
                ("a".to_string(), "{}".to_string(), now),
                ("b".to_string(), r#"{"x":1}"#.to_string(), now),
            ]
        );
    }

    #[test]
    fn a_changed_point_is_updated_and_only_that_one() {
        let (store, owner) = setup();
        store
            .health_points_put(
                owner,
                "steps",
                &[point("a", "{}"), point("b", "{}")],
                at(NOW),
            )
            .unwrap();
        let changed = [point("a", r#"{"v":2}"#), point("b", "{}")];
        assert_eq!(
            store
                .health_points_put(owner, "steps", &changed, at(LATER))
                .unwrap(),
            1
        );
        assert_eq!(
            stored(&store, "steps"),
            [
                (
                    "a".to_string(),
                    r#"{"v":2}"#.to_string(),
                    at(LATER).as_millisecond()
                ),
                ("b".to_string(), "{}".to_string(), at(NOW).as_millisecond()),
            ]
        );
    }

    #[test]
    fn a_point_key_is_unique_per_user_and_data_type() {
        let (store, owner) = setup();
        let other = store.user("telegram", "8").unwrap().id();
        let rows = [point("a", "{}")];
        assert_eq!(
            store
                .health_points_put(owner, "steps", &rows, at(NOW))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .health_points_put(owner, "weight", &rows, at(NOW))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .health_points_put(other, "steps", &rows, at(NOW))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .health_points_put(owner, "steps", &[], at(NOW))
                .unwrap(),
            0
        );
        // `stored` reads every user's rows: one steps row each, and one weight row.
        assert_eq!(stored(&store, "steps").len(), 2);
        assert_eq!(stored(&store, "weight").len(), 1);
    }

    #[test]
    fn stored_points_outlive_a_disconnect() {
        let (store, owner) = setup();
        store.health_connect(owner, b"t", "s", at(NOW)).unwrap();
        store
            .health_points_put(owner, "steps", &[point("a", "{}")], at(NOW))
            .unwrap();
        store.health_disconnect(owner).unwrap();
        assert_eq!(stored(&store, "steps").len(), 1);
    }

    #[test]
    fn the_history_cursor_of_each_type_is_saved_and_read_back() {
        let (store, owner) = setup();
        assert!(store.health_backfill(owner).unwrap().is_empty());
        let mut progress = Backfill {
            data_type: "steps".into(),
            oldest: "2026-09-27".into(),
            done: false,
            empty_run: 2,
        };
        store
            .health_backfill_save(owner, &progress, None, at(NOW))
            .unwrap();
        assert_eq!(store.health_backfill(owner).unwrap(), [progress.clone()]);

        // A later chunk moves the cursor; a failure is kept, cut to 200 characters.
        progress.oldest = "2026-09-20".into();
        progress.empty_run = 0;
        progress.done = true;
        let long = "e".repeat(300);
        store
            .health_backfill_save(owner, &progress, Some(&long), at(LATER))
            .unwrap();
        assert_eq!(store.health_backfill(owner).unwrap(), [progress.clone()]);
        let kept: String = store
            .db_for_tests()
            .query_row(
                "SELECT last_error FROM health_backfill WHERE user_id = ?1",
                [owner],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept.len(), ERROR_CHARS);

        // A clean save clears the error.
        store
            .health_backfill_save(owner, &progress, None, at(LATER))
            .unwrap();
        let cleared: Option<String> = store
            .db_for_tests()
            .query_row(
                "SELECT last_error FROM health_backfill WHERE user_id = ?1",
                [owner],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cleared, None);
    }

    #[test]
    fn reconnecting_starts_the_history_fetch_again() {
        let (store, owner) = setup();
        let other = store.user("telegram", "8").unwrap().id();
        let progress = Backfill {
            data_type: "sleep".into(),
            oldest: "2025-01-01".into(),
            done: true,
            empty_run: 0,
        };
        for id in [owner, other] {
            store
                .health_backfill_save(id, &progress, None, at(NOW))
                .unwrap();
        }
        store.health_connect(owner, b"t", "s", at(LATER)).unwrap();
        assert!(store.health_backfill(owner).unwrap().is_empty());
        // Another user's cursor is not touched.
        assert_eq!(store.health_backfill(other).unwrap(), [progress]);
    }
}
