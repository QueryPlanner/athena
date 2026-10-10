//! SQLite operations for scheduled jobs: the reminder tools' rows, and the
//! claims and outcomes of the scheduler that delivers them.
use super::Store;
use crate::reminders::{
    self, CONFIRM_WINDOW, Create, Kind, MAX_ACTIVE, MAX_ACTIVE_TASKS, MAX_CREATED_PER_DAY,
    Recurrence,
};
use crate::scheduler::{BACKOFF, LEASE, MAX_ATTEMPTS};
use anyhow::{Context, Result, bail, ensure};
use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

/// A job the scheduler claimed: it is leased to this process until
/// [`LEASE`] from the claim, or until one of the outcome calls below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub id: i64,
    pub owner: i64,
    /// As stored; [`Kind::parse`] says whether this build knows it.
    pub kind: String,
    pub payload: String,
    /// The occurrence's due time, kept while it is retried or deferred.
    pub due: Timestamp,
    pub recurrence: Option<String>,
    /// Failed deliveries of this occurrence so far.
    pub attempts: i64,
    /// The owner's Telegram chat: their Telegram user id, for a private
    /// chat. None if they have no Telegram identity.
    pub chat: Option<i64>,
}

fn ms(at: Timestamp) -> i64 {
    at.as_millisecond()
}

fn instant(ms: i64) -> rusqlite::Result<Timestamp> {
    Timestamp::from_millisecond(ms).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, e.into())
    })
}

/// The owner's Telegram user id. Their first Telegram identity, should a
/// user ever have two.
fn telegram_chat(db: &Connection, owner: i64) -> Result<Option<i64>> {
    let id: Option<String> = db
        .query_row(
            "SELECT external_id FROM user_identities
             WHERE user_id = ?1 AND transport = 'telegram'
             ORDER BY created_at, rowid LIMIT 1",
            [owner],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| id.parse().context("a Telegram user id is not a number"))
        .transpose()
}

fn count(db: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<i64> {
    Ok(db.query_row(sql, args, |r| r.get(0))?)
}

/// One reminder as the tools show it.
fn describe(row: &rusqlite::Row<'_>, zone: &TimeZone) -> rusqlite::Result<Value> {
    let recurrence: Option<String> = row.get(4)?;
    // A schedule this build cannot read is shown as stored.
    let repeat = recurrence.map_or_else(
        || "once".to_string(),
        |stored| Recurrence::parse(&stored).map_or(stored, |r| r.describe()),
    );
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "kind": row.get::<_, String>(1)?,
        "text": row.get::<_, String>(2)?,
        "next_run": reminders::local(instant(row.get(3)?)?, zone),
        "repeat": repeat,
        "status": row.get::<_, String>(5)?,
        "last_error": row.get::<_, Option<String>>(6)?,
    }))
}

const SHOW_ONE: &str = "SELECT id, kind, payload, next_run_at, recurrence, status, last_error
                        FROM jobs WHERE id = ?1";
const SHOW_ACTIVE: &str = "SELECT id, kind, payload, next_run_at, recurrence, status, last_error
                           FROM jobs WHERE user_id = ?1 AND status = 'active'
                           ORDER BY next_run_at, id";
const SHOW_PENDING: &str = "SELECT id, kind, payload, next_run_at, recurrence, status, last_error
                            FROM jobs WHERE user_id = ?1 AND status = 'pending'
                              AND confirm_expires_at > ?2
                            ORDER BY id";
const SHOW_FAILED: &str = "SELECT id, kind, payload, next_run_at, recurrence, status, last_error
                           FROM jobs WHERE user_id = ?1 AND status = 'failed' AND updated_at > ?2
                           ORDER BY updated_at DESC, id";
/// Active reminders, and tasks still waiting for a confirmation that can
/// come: both count against the limits.
const LIVE: &str = "SELECT COUNT(*) FROM jobs WHERE user_id = ?1
                    AND (status = 'active' OR (status = 'pending' AND confirm_expires_at > ?2))";
const LIVE_TASKS: &str = "SELECT COUNT(*) FROM jobs WHERE user_id = ?1 AND kind = 'agent_task'
                          AND (status = 'active'
                               OR (status = 'pending' AND confirm_expires_at > ?2))";
const CREATED_SINCE: &str = "SELECT COUNT(*) FROM jobs WHERE user_id = ?1 AND created_at > ?2";
const REPEATING: &str = "SELECT id, recurrence FROM jobs
                         WHERE user_id = ?1 AND status IN ('active', 'pending')
                           AND recurrence IS NOT NULL";
const INSERT: &str = "INSERT INTO jobs (user_id, kind, payload, next_run_at, recurrence, status,
                                        created_at, updated_at, confirm_code, confirm_session,
                                        confirm_expires_at)
                      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10)";
const MOVE: &str = "UPDATE jobs SET next_run_at = ?2, updated_at = ?3 WHERE id = ?1";
const AWAITING: &str = "SELECT confirm_code, confirm_session, confirm_expires_at, next_run_at,
                               recurrence
                        FROM jobs WHERE id = ?1 AND user_id = ?2 AND status = 'pending'";
const ACTIVATE: &str = "UPDATE jobs SET status = 'active', next_run_at = ?2, updated_at = ?3,
                            confirm_code = NULL, confirm_session = NULL,
                            confirm_expires_at = NULL
                        WHERE id = ?1";

/// A code of 8 characters a person can read back without mixing up `0` and
/// `O` or `1` and `I`. The same alphabet as the skill confirmations.
fn new_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let random = uuid::Uuid::new_v4();
    random.as_bytes()[8..]
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

/// Whether `said` has `word` as a whole word, ignoring case and the
/// punctuation around it (`#12`, `12.`).
fn says(said: &str, word: &str) -> bool {
    said.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case(word))
}

impl Store {
    /// Schedule a reminder for `owner`, as of `now`. Fails, writing nothing,
    /// if the arguments are invalid, the owner has no Telegram chat to
    /// deliver to, or a limit is reached.
    ///
    /// A `notify` is active at once. An `agent_task` is stored `pending`
    /// with a fresh confirmation code, bound to `session`, that expires
    /// after [`CONFIRM_WINDOW`]; it never runs until
    /// [`Store::confirm_reminder`] activates it. The result says which.
    pub fn create_reminder(
        &self,
        owner: i64,
        session: &str,
        args: &Create,
        now: Timestamp,
    ) -> Result<Value> {
        let zone = self.timezone(owner)?;
        let (recurrence, first) = args.schedule(now, &zone)?;
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            telegram_chat(&tx, owner)?.is_some(),
            "reminders are delivered in Telegram, and this user has never messaged the \
             Telegram bot; ask them to message it first"
        );
        let live = count(&tx, LIVE, params![owner, ms(now)])?;
        ensure!(
            live < MAX_ACTIVE,
            "the user already has {MAX_ACTIVE} active reminders; cancel one first"
        );
        let task = args.kind == Kind::AgentTask;
        if task {
            let tasks = count(&tx, LIVE_TASKS, params![owner, ms(now)])?;
            ensure!(
                tasks < MAX_ACTIVE_TASKS,
                "the user already has {MAX_ACTIVE_TASKS} active agent_task reminders; cancel one first"
            );
        }
        let day_ago = ms(now) - SignedDuration::from_hours(24).as_millis() as i64;
        let created = count(&tx, CREATED_SINCE, params![owner, day_ago])?;
        ensure!(
            created < MAX_CREATED_PER_DAY,
            "{MAX_CREATED_PER_DAY} reminders were created for this user in the last 24 hours; \
             try again later"
        );
        let code = task.then(new_code);
        let expires = task.then(|| ms(now + CONFIRM_WINDOW));
        let row = params![
            owner,
            args.kind.as_str(),
            args.text,
            ms(first),
            recurrence.as_ref().map(Recurrence::to_stored),
            if task { "pending" } else { "active" },
            ms(now),
            code,
            task.then_some(session),
            expires
        ];
        tx.execute(INSERT, row)?;
        let id = tx.last_insert_rowid();
        let mut shown = tx.query_row(SHOW_ONE, [id], |r| describe(r, &zone))?;
        tx.commit()?;
        if let Some(code) = code {
            shown["confirmation_code"] = json!(code);
            shown["next_step"] = json!(format!(
                "Nothing is scheduled yet. Show the user the whole task text, when it runs \
                 and this code, and ask them to reply `confirm #{id} {code}` if they want it. \
                 Only when their own reply contains both, call reminder_confirm with id {id} \
                 and the code. The code expires in {} minutes.",
                CONFIRM_WINDOW.as_mins()
            ));
        }
        Ok(shown)
    }

    /// Activate `owner`'s pending task `id` with `code`, as of `now`, if
    /// `said`, the user's own message in this turn, contains both the code
    /// and the id, and the code was issued in `session` and has not
    /// expired. A task whose first run has passed meanwhile moves to its
    /// next run if it repeats, and is refused if it does not. The code works
    /// once: the task is no longer pending after.
    pub fn confirm_reminder(
        &self,
        owner: i64,
        session: &str,
        id: i64,
        code: &str,
        said: &str,
        now: Timestamp,
    ) -> Result<Value> {
        let code = code.trim().to_ascii_uppercase();
        ensure!(
            !code.is_empty() && says(said, &code) && says(said, &id.to_string()),
            "Not confirmed: the user's own latest message must contain #{id} and the code. \
             Show them the task and the code, and wait for their reply."
        );
        let zone = self.timezone(owner)?;
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let awaiting: Option<(String, String, i64, i64, Option<String>)> = tx
            .query_row(AWAITING, params![id, owner], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let Some((expected, issued_in, expires, first, recurrence)) = awaiting else {
            bail!("the user has no task {id} waiting for confirmation; see reminder_list");
        };
        ensure!(
            expected == code && issued_in == session && expires > ms(now),
            "that code does not confirm task {id}, or it expired; create the task again for \
             a new code"
        );
        let first = match recurrence {
            _ if first > ms(now) => first,
            Some(stored) => ms(Recurrence::parse(&stored)?.next_after(now, &zone)?),
            None => bail!("task {id}'s time passed before it was confirmed; create it again"),
        };
        tx.execute(ACTIVATE, params![id, first, ms(now)])?;
        let mut shown = tx.query_row(SHOW_ONE, [id], |r| describe(r, &zone))?;
        tx.commit()?;
        shown["confirmed"] = json!(true);
        Ok(shown)
    }

    /// `owner`'s active reminders, soonest first, the tasks waiting for a
    /// confirmation that can still come, and the reminders that failed in
    /// the last seven days, as of `now`.
    pub fn reminders(&self, owner: i64, now: Timestamp) -> Result<Value> {
        let zone = self.timezone(owner)?;
        let week_ago = ms(now) - SignedDuration::from_hours(24 * 7).as_millis() as i64;
        let db = self.db();
        let list = |sql: &str, args: &[i64]| -> Result<Vec<Value>> {
            let mut q = db.prepare(sql)?;
            let rows = q.query_map(rusqlite::params_from_iter(args), |r| describe(r, &zone))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        Ok(json!({
            "active": list(SHOW_ACTIVE, &[owner])?,
            "awaiting_confirmation": list(SHOW_PENDING, &[owner, ms(now)])?,
            "failed": list(SHOW_FAILED, &[owner, week_ago])?,
        }))
    }

    /// Cancel `owner`'s active or pending reminder `id`. A reminder that is
    /// running now still finishes this run.
    pub fn cancel_reminder(&self, owner: i64, id: i64, now: Timestamp) -> Result<Value> {
        let changed = self.db().execute(
            "UPDATE jobs SET status = 'cancelled', lease_until = NULL, confirm_code = NULL,
                             updated_at = ?3
             WHERE id = ?1 AND user_id = ?2 AND status IN ('active', 'pending')",
            params![id, owner, ms(now)],
        )?;
        ensure!(
            changed == 1,
            "the user has no active reminder {id}; see reminder_list"
        );
        Ok(json!({"cancelled": id}))
    }

    /// Recompute when `owner`'s repeating reminders next run, after their
    /// time zone changed to `zone`, so the next run is already at their new
    /// wall-clock time.
    pub(super) fn reschedule(
        db: &Connection,
        owner: i64,
        zone: &TimeZone,
        now: Timestamp,
    ) -> Result<()> {
        let mut repeating = db.prepare(REPEATING)?;
        let rows = repeating.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let rows: Vec<(i64, String)> = rows.collect::<rusqlite::Result<_>>()?;
        for (id, stored) in rows {
            let next = Recurrence::parse(&stored)?.next_after(now, zone)?;
            db.execute(MOVE, params![id, ms(next), ms(now)])?;
        }
        Ok(())
    }

    /// Lease up to `limit` jobs that are due at `now` and not leased, the
    /// longest overdue first.
    pub fn claim_jobs(&self, now: Timestamp, limit: i64) -> Result<Vec<Due>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut claimed: Vec<Due> = tx
            .prepare(
                "SELECT id, user_id, kind, payload, next_run_at, recurrence, attempts FROM jobs
                 WHERE status = 'active' AND next_run_at <= ?1
                   AND (lease_until IS NULL OR lease_until <= ?1)
                 ORDER BY next_run_at, id LIMIT ?2",
            )?
            .query_map(params![ms(now), limit], |r| {
                Ok(Due {
                    id: r.get(0)?,
                    owner: r.get(1)?,
                    kind: r.get(2)?,
                    payload: r.get(3)?,
                    due: instant(r.get(4)?)?,
                    recurrence: r.get(5)?,
                    attempts: r.get(6)?,
                    chat: None,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        for job in &mut claimed {
            tx.execute(
                "UPDATE jobs SET lease_until = ?2 WHERE id = ?1",
                params![job.id, ms(now) + LEASE.as_millis() as i64],
            )?;
            job.chat = telegram_chat(&tx, job.owner)?;
        }
        tx.commit()?;
        Ok(claimed)
    }

    /// When `job` runs next after `now` on its owner's wall clock: None if
    /// it does not repeat.
    pub fn next_run(&self, job: &Due, now: Timestamp) -> Result<Option<Timestamp>> {
        let Some(stored) = &job.recurrence else {
            return Ok(None);
        };
        let recurrence = Recurrence::parse(stored)?;
        Ok(Some(
            recurrence.next_after(now, &self.timezone(job.owner)?)?,
        ))
    }

    /// Close `job`'s occurrence: it ran at `now` if `sent`, else it was
    /// skipped because of `why`. The job then waits for `next`, or is done.
    /// A job cancelled meanwhile stays cancelled.
    pub fn job_done(
        &self,
        job: i64,
        now: Timestamp,
        next: Option<Timestamp>,
        sent: bool,
        why: Option<&str>,
    ) -> Result<()> {
        self.db().execute(
            "UPDATE jobs SET
                 status = CASE WHEN ?3 IS NULL THEN 'done' ELSE status END,
                 next_run_at = COALESCE(?3, next_run_at),
                 sent_at = CASE WHEN ?4 THEN ?2 ELSE sent_at END,
                 lease_until = NULL, attempts = 0, last_error = ?5, updated_at = ?2
             WHERE id = ?1 AND status = 'active'",
            params![job, ms(now), next.map(ms), sent, why],
        )?;
        Ok(())
    }

    /// A delivery of `job` failed for a reason that may pass: try again
    /// after a backoff, or give up after [`MAX_ATTEMPTS`]. Returns whether
    /// it will be retried.
    pub fn job_retry(&self, job: &Due, now: Timestamp, why: &str) -> Result<bool> {
        let attempts = job.attempts + 1;
        if attempts >= MAX_ATTEMPTS {
            self.job_failed(job.id, now, why)?;
            return Ok(false);
        }
        // Doubling from BACKOFF: at most 2^(MAX_ATTEMPTS - 2) times it.
        let wait = BACKOFF * (1_i32 << (attempts - 1));
        self.db().execute(
            "UPDATE jobs SET attempts = ?2, lease_until = ?3, last_error = ?4, updated_at = ?5
             WHERE id = ?1 AND status = 'active'",
            params![
                job.id,
                attempts,
                ms(now) + wait.as_millis() as i64,
                why,
                ms(now)
            ],
        )?;
        Ok(true)
    }

    /// `job` can never be delivered: stop it.
    pub fn job_failed(&self, job: i64, now: Timestamp, why: &str) -> Result<()> {
        self.db().execute(
            "UPDATE jobs SET status = 'failed', lease_until = NULL, last_error = ?3,
                             updated_at = ?2
             WHERE id = ?1 AND status = 'active'",
            params![job, ms(now), why],
        )?;
        Ok(())
    }

    /// Leave `job` due but unclaimed until `until`.
    pub fn job_defer(&self, job: i64, until: Timestamp) -> Result<()> {
        self.db().execute(
            "UPDATE jobs SET lease_until = ?2 WHERE id = ?1 AND status = 'active'",
            params![job, ms(until)],
        )?;
        Ok(())
    }

    /// How many `agent_task` runs `owner` started after `since`, whatever
    /// became of their reminders since.
    pub fn task_runs_since(&self, owner: i64, since: Timestamp) -> Result<i64> {
        count(
            &self.db(),
            "SELECT COUNT(*) FROM jobs
             WHERE user_id = ?1 AND kind = 'agent_task' AND sent_at > ?2",
            params![owner, ms(since)],
        )
    }
}
