//! SQLite operations for user-owned training records.
//!
//! `workout_sets` and `rowing_results` have no `user_id` of their own: every
//! read joins `workout_sessions` on its owner, and every write to them
//! happens in the transaction that first changed an owned session row.
use super::{Store, now_millis};
use crate::calories::{Range, date};
use crate::workouts::{
    self, DayType, History, Kind, Last, Lifted, Log, Prepared, Progress, Remove, Update,
    estimated_1rm, exercise_key, format_time, split_ms, top_set,
};
use anyhow::{Result, ensure};
use jiff::{Timestamp, ToSpan};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ACTIVE: &str = "user_id = ?1 AND deleted_at IS NULL";
// A `?` alone on a line is a line no test reaches, so the SQL that would
// push one there is named here.
const SETS: &str = "SELECT exercise_index, exercise, set_index, reps, weight_kg, target_reps,
     is_warmup, notes FROM workout_sets WHERE session_id = ?1 ORDER BY exercise_index, set_index";
const SESSION: &str = "SELECT id, session_date, day_type, notes, version, created_at, updated_at,
     deleted_at FROM workout_sessions WHERE user_id = ?1 AND id = ?2";
const INSERT_SET: &str = "INSERT INTO workout_sets (session_id, exercise_index, set_index,
     exercise, exercise_key, reps, weight_kg, target_reps, is_warmup, notes)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";
const INSERT_ROWING: &str =
    "INSERT INTO rowing_results (session_id, distance_m, time_ms) VALUES (?1, ?2, ?3)";
const LAST_VO2: &str = "SELECT MAX(session_date) FROM workout_sessions
     WHERE user_id = ?1 AND deleted_at IS NULL AND day_type = 'vo2' AND session_date <= ?2";

/// One stored set, with the block it belongs to.
struct Row {
    block: i64,
    exercise: String,
    set_index: i64,
    lifted: Lifted,
    notes: Option<String>,
}

fn rows(db: &Connection, session_id: i64) -> Result<Vec<Row>> {
    let mut q = db.prepare(SETS)?;
    let rows = q.query_map([session_id], |r| {
        Ok(Row {
            block: r.get(0)?,
            exercise: r.get(1)?,
            set_index: r.get(2)?,
            lifted: Lifted {
                reps: r.get(3)?,
                weight_kg: r.get(4)?,
                target_reps: r.get(5)?,
                is_warmup: r.get(6)?,
            },
            notes: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The exercise blocks of a session, in order. With `suggest`, each block
/// carries what progressive overload suggests next time.
fn exercises(db: &Connection, session_id: i64, suggest: bool) -> Result<Vec<Value>> {
    let mut blocks: Vec<(String, Vec<Row>)> = Vec::new();
    let mut current = -1;
    for row in rows(db, session_id)? {
        if row.block != current {
            current = row.block;
            blocks.push((row.exercise.clone(), Vec::new()));
        }
        blocks.last_mut().expect("pushed above").1.push(row);
    }
    Ok(blocks
        .into_iter()
        .map(|(name, sets)| {
            let lifted: Vec<Lifted> = sets.iter().map(|s| s.lifted.clone()).collect();
            let mut block = json!({"name": name, "sets": sets.iter().map(|s| json!({
                "set": s.set_index, "reps": s.lifted.reps, "weight_kg": s.lifted.weight_kg,
                "target_reps": s.lifted.target_reps, "is_warmup": s.lifted.is_warmup, "notes": s.notes,
            })).collect::<Vec<_>>()});
            if suggest {
                block["progression"] = workouts::progression(&lifted);
            }
            block
        })
        .collect())
}

fn rowing(distance_m: u32, time_ms: i64) -> Value {
    json!({"distance_m": distance_m, "time": format_time(time_ms), "time_ms": time_ms,
           "split_500m": format_time(split_ms(time_ms, distance_m))})
}

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id": r.get::<_, i64>(0)?, "session_date": r.get::<_, String>(1)?,
        "day_type": r.get::<_, String>(2)?, "notes": r.get::<_, Option<String>>(3)?,
        "version": r.get::<_, i64>(4)?, "created_at": r.get::<_, i64>(5)?,
        "updated_at": r.get::<_, i64>(6)?, "deleted_at": r.get::<_, Option<i64>>(7)?}),
    )
}

/// A whole session of `owner`, deleted or not: the record a tool returns.
fn session(db: &Connection, owner: i64, id: i64, suggest: bool) -> Result<Value> {
    let mut value = db.query_row(SESSION, params![owner, id], session_row)?;
    value["exercises"] = json!(exercises(db, id, suggest)?);
    value["rowing"] = db
        .query_row(
            "SELECT distance_m, time_ms FROM rowing_results WHERE session_id = ?1",
            [id],
            |r| Ok(rowing(r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or(Value::Null);
    Ok(value)
}

/// Write a prepared workout's sets and rowing piece under a session the
/// caller has just inserted or changed for its owner.
fn write_children(db: &Connection, session_id: i64, prepared: &Prepared) -> Result<()> {
    let mut insert = db.prepare(INSERT_SET)?;
    for (block, exercise) in prepared.workout.exercises.iter().enumerate() {
        for (index, set) in exercise.sets.iter().enumerate() {
            insert.execute(params![
                session_id,
                block as i64 + 1,
                index as i64 + 1,
                exercise.name,
                exercise_key(&exercise.name),
                set.reps,
                set.weight_kg,
                set.target_reps,
                set.is_warmup,
                set.notes
            ])?;
        }
    }
    if let (Some(r), Some(ms)) = (&prepared.workout.rowing, prepared.time_ms) {
        db.execute(INSERT_ROWING, params![session_id, r.distance_m, ms])?;
    }
    Ok(())
}

/// The newest active session of `day_type` dated before `before`.
fn last_before(
    db: &Connection,
    owner: i64,
    day_type: DayType,
    before: &str,
) -> Result<Option<Value>> {
    let id: Option<i64> = db
        .query_row(
            &format!(
                "SELECT id FROM workout_sessions WHERE {ACTIVE} AND day_type = ?2
                 AND session_date < ?3 ORDER BY session_date DESC, id DESC LIMIT 1"
            ),
            params![owner, day_type.as_str(), before],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| session(db, owner, id, true)).transpose()
}

fn mutation_ids(id: i64, expected_version: i64) -> Result<()> {
    ensure!(
        id > 0 && expected_version > 0,
        "id and expected_version must be positive"
    );
    Ok(())
}

const CONFLICT: &str = "session missing, deleted, or version conflicts";

impl Store {
    pub fn workout_log(&self, owner: i64, args: Log) -> Result<Value> {
        crate::calories::text(&args.request_key, 128)?;
        let prepared = args.workout.prepare()?;
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&prepared.hash_input())?)
        );
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, request_hash FROM workout_sessions WHERE user_id=?1 AND request_key=?2",
                params![owner, args.request_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (id, existed) = match prior {
            Some((id, original)) => {
                ensure!(
                    original == hash,
                    "request_key conflicts with a different original workout"
                );
                (id, true)
            }
            None => {
                let w = &prepared.workout;
                tx.execute(
                    "INSERT INTO workout_sessions (user_id, request_key, request_hash, session_date,
                         day_type, notes, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![
                        owner,
                        args.request_key,
                        hash,
                        w.session_date,
                        w.day_type.as_str(),
                        w.notes,
                        now_millis()
                    ],
                )?;
                let id = tx.last_insert_rowid();
                write_children(&tx, id, &prepared)?;
                (id, false)
            }
        };
        let result =
            json!({"session": session(&tx, owner, id, false)?, "already_existed": existed});
        tx.commit()?;
        Ok(result)
    }

    /// The previous session of a day type, before `args.before_date` or,
    /// by default, before the user's today at `at`.
    pub fn workout_last(&self, owner: i64, args: Last, at: Timestamp) -> Result<Value> {
        let before = match args.before_date {
            Some(before) => {
                date(&before)?;
                before
            }
            None => self.today(owner, at)?.to_string(),
        };
        let db = self.db();
        Ok(
            json!({"before_date": before, "session": last_before(&db, owner, args.day_type, &before)?}),
        )
    }

    /// What the user trains on their today at `at`. Sessions dated after
    /// today are ignored; the rotation counts only lifting days before
    /// today, so a push day logged this morning is still today's push day.
    pub fn workout_next(&self, owner: i64, at: Timestamp) -> Result<Value> {
        let today = self.today(owner, at)?;
        let window_start = today.checked_sub(6.days())?.to_string();
        let today = today.to_string();
        let db = self.db();
        let last_lifting: Option<(String, String)> = db
            .query_row(
                &format!(
                    "SELECT session_date, day_type FROM workout_sessions WHERE {ACTIVE}
                     AND day_type IN ('push', 'pull', 'legs') AND session_date < ?2
                     ORDER BY session_date DESC, id DESC LIMIT 1"
                ),
                params![owner, today],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let last_type = last_lifting
            .as_ref()
            .map(|(_, t)| DayType::parse(t))
            .transpose()?;
        let next = DayType::after(last_type);
        let vo2 = |r: &rusqlite::Row<'_>| r.get(0);
        let last_vo2: Option<String> = db.query_row(LAST_VO2, params![owner, today], vo2)?;
        let mut q = db.prepare(&format!(
            "SELECT id, day_type, version FROM workout_sessions WHERE {ACTIVE}
             AND session_date = ?2 ORDER BY id"
        ))?;
        let logged_today = q
            .query_map(params![owner, today], |r| {
                Ok(
                    json!({"id": r.get::<_, i64>(0)?, "day_type": r.get::<_, String>(1)?,
                          "version": r.get::<_, i64>(2)?}),
                )
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({
            "today": today,
            "next_day_type": next,
            "last_lifting": last_lifting.map(|(d, t)| json!({"session_date": d, "day_type": t})),
            "vo2": {
                "due": last_vo2.as_ref().is_none_or(|d| *d < window_start),
                "last_date": last_vo2,
            },
            "logged_today": logged_today,
            "previous": last_before(&db, owner, next, &today)?,
        }))
    }

    pub fn exercise_progress(&self, owner: i64, args: Progress) -> Result<Value> {
        let limit = args.limit.unwrap_or(10);
        ensure!((1..=50).contains(&limit), "limit must be between 1 and 50");
        let db = self.db();
        match args.kind {
            Kind::Lift => {
                ensure!(
                    args.distance_m.is_none(),
                    "distance_m is for kind rowing only"
                );
                let name = args
                    .exercise
                    .ok_or_else(|| anyhow::anyhow!("kind lift needs an exercise name"))?;
                lift_progress(&db, owner, &name, limit as usize)
            }
            Kind::Rowing => {
                ensure!(
                    args.exercise.is_none(),
                    "exercise is for kind lift only; rowing is selected by distance_m"
                );
                let distance = args.distance_m.unwrap_or(workouts::DEFAULT_DISTANCE_M);
                rowing_progress(&db, owner, distance, limit as usize)
            }
        }
    }

    pub fn workout_history(&self, owner: i64, args: History) -> Result<Value> {
        Range {
            start_date: args.start_date.clone(),
            end_date: args.end_date.clone(),
        }
        .validate()?;
        let limit = args.limit.unwrap_or(10);
        ensure!((1..=20).contains(&limit), "limit must be between 1 and 20");
        ensure!(
            args.before_id.is_none_or(|id| id > 0),
            "before_id must be positive"
        );
        let db = self.db();
        let mut q = db.prepare(&format!(
            "SELECT id FROM workout_sessions WHERE {ACTIVE} AND session_date BETWEEN ?2 AND ?3
             AND (?4 IS NULL OR id < ?4) ORDER BY id DESC LIMIT ?5"
        ))?;
        let bindings = params![
            owner,
            args.start_date,
            args.end_date,
            args.before_id,
            limit + 1
        ];
        let mut ids = q
            .query_map(bindings, |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if ids.len() > limit as usize {
            ids.truncate(limit as usize);
            ids.last().copied()
        } else {
            None
        };
        let sessions = ids
            .into_iter()
            .map(|id| session(&db, owner, id, false))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"sessions": sessions, "next_before_id": next}))
    }

    pub fn workout_update(&self, owner: i64, args: Update) -> Result<Value> {
        let prepared = args.workout.prepare()?;
        mutation_ids(args.id, args.expected_version)?;
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let w = &prepared.workout;
        // The owner-scoped version check comes first: children are only
        // touched once this session is known to be the caller's.
        let changed = tx.execute(
            "UPDATE workout_sessions SET session_date = ?1, day_type = ?2, notes = ?3,
                 updated_at = ?4, version = version + 1
             WHERE user_id = ?5 AND id = ?6 AND version = ?7 AND deleted_at IS NULL",
            params![
                w.session_date,
                w.day_type.as_str(),
                w.notes,
                now_millis(),
                owner,
                args.id,
                args.expected_version
            ],
        )?;
        ensure!(changed == 1, CONFLICT);
        tx.execute("DELETE FROM workout_sets WHERE session_id = ?1", [args.id])?;
        tx.execute(
            "DELETE FROM rowing_results WHERE session_id = ?1",
            [args.id],
        )?;
        write_children(&tx, args.id, &prepared)?;
        let result = session(&tx, owner, args.id, false)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn workout_remove(&self, owner: i64, args: Remove) -> Result<Value> {
        mutation_ids(args.id, args.expected_version)?;
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE workout_sessions SET deleted_at = ?1, updated_at = ?1, version = version + 1
             WHERE user_id = ?2 AND id = ?3 AND version = ?4 AND deleted_at IS NULL",
            params![now_millis(), owner, args.id, args.expected_version],
        )?;
        ensure!(changed == 1, CONFLICT);
        let result = session(&tx, owner, args.id, false)?;
        tx.commit()?;
        Ok(result)
    }
}

fn change(newest: Option<f64>, oldest: Option<f64>, entries: usize) -> Option<f64> {
    match (newest, oldest) {
        (Some(n), Some(o)) if entries > 1 => Some(((n - o) * 10.0).round() / 10.0),
        _ => None,
    }
}

fn lift_progress(db: &Connection, owner: i64, name: &str, limit: usize) -> Result<Value> {
    crate::calories::text(name, 64)?;
    let key = exercise_key(name);
    let mut q = db.prepare(&format!(
        "SELECT s.id, s.session_date, w.exercise, w.reps, w.weight_kg, w.target_reps, w.is_warmup
         FROM workout_sessions s JOIN workout_sets w ON w.session_id = s.id
         WHERE s.{ACTIVE} AND w.exercise_key = ?2
         ORDER BY s.session_date DESC, s.id DESC, w.exercise_index, w.set_index"
    ))?;
    let mut sessions: Vec<(i64, String, String, Vec<Lifted>)> = Vec::new();
    let found = q.query_map(params![owner, key], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            Lifted {
                reps: r.get(3)?,
                weight_kg: r.get(4)?,
                target_reps: r.get(5)?,
                is_warmup: r.get(6)?,
            },
        ))
    })?;
    for row in found {
        let (id, date, exercise, lifted) = row?;
        match sessions.last_mut() {
            Some(last) if last.0 == id => last.3.push(lifted),
            _ => sessions.push((id, date, exercise, vec![lifted])),
        }
    }
    let Some(display) = sessions.first().map(|s| s.2.clone()) else {
        let mut known = db.prepare(&format!(
            // With MAX, SQLite takes the bare `exercise` from the newest row:
            // each lift under the name it was last logged as.
            "SELECT w.exercise, MAX(s.session_date) AS latest
             FROM workout_sessions s JOIN workout_sets w ON w.session_id = s.id
             WHERE s.{ACTIVE} GROUP BY w.exercise_key
             ORDER BY latest DESC, w.exercise_key LIMIT 50"
        ))?;
        let known = known
            .query_map([owner], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(
            json!({"kind": "lift", "exercise": name, "sessions": [], "best": null,
                         "estimated_1rm_change_kg": null, "known_exercises": known}),
        );
    };
    let summarize = |(id, date, _, sets): &(i64, String, String, Vec<Lifted>)| {
        let top = top_set(sets);
        json!({"session_id": id, "session_date": date,
               "top_set": top.map(|t| json!({"weight_kg": t.weight_kg, "reps": t.reps})),
               "estimated_1rm_kg": top.and_then(|t| estimated_1rm(t.weight_kg, t.reps)),
               "working_sets": sets.iter().filter(|s| !s.is_warmup && s.reps > 0).count()})
    };
    let all: Vec<Value> = sessions.iter().map(summarize).collect();
    let rank = |v: &Value| {
        (
            v["estimated_1rm_kg"].as_f64().unwrap_or(-1.0),
            v["top_set"]["weight_kg"].as_f64().unwrap_or(-1.0),
            v["top_set"]["reps"].as_u64().unwrap_or(0),
        )
    };
    let best = all
        .iter()
        .filter(|v| !v["top_set"].is_null())
        .max_by(|a, b| {
            rank(a)
                .partial_cmp(&rank(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();
    let window: Vec<Value> = all.into_iter().take(limit).collect();
    let e1rm = |v: Option<&Value>| v.and_then(|v| v["estimated_1rm_kg"].as_f64());
    Ok(json!({
        "kind": "lift",
        "exercise": display,
        "estimated_1rm_change_kg": change(e1rm(window.first()), e1rm(window.last()), window.len()),
        "best": best,
        "sessions": window,
    }))
}

fn rowing_progress(db: &Connection, owner: i64, distance: u32, limit: usize) -> Result<Value> {
    let mut q = db.prepare(&format!(
        "SELECT s.id, s.session_date, r.time_ms FROM workout_sessions s
         JOIN rowing_results r ON r.session_id = s.id
         WHERE s.{ACTIVE} AND r.distance_m = ?2 ORDER BY s.session_date DESC, s.id DESC"
    ))?;
    let pieces = q
        .query_map(params![owner, distance], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let entry = |(id, date, ms): &(i64, String, i64)| {
        let mut v = rowing(distance, *ms);
        v["session_id"] = json!(id);
        v["session_date"] = json!(date);
        v
    };
    let best = pieces.iter().min_by_key(|p| p.2).map(entry);
    let window: Vec<&(i64, String, i64)> = pieces.iter().take(limit).collect();
    let seconds = |p: Option<&&(i64, String, i64)>| p.map(|p| p.2 as f64 / 1000.0);
    Ok(json!({
        "kind": "rowing",
        "distance_m": distance,
        "time_change_s": change(seconds(window.first()), seconds(window.last()), window.len()),
        "best": best,
        "sessions": window.into_iter().map(entry).collect::<Vec<_>>(),
    }))
}
