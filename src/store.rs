//! Session persistence. Two tables: the conversation, and what each run cost.
//!
//! `messages` holds the transcript the model sees next turn, as opaque Rig
//! JSON. `runs` holds per-run telemetry that Rig reports and the transcript
//! cannot express — token usage, model-call count, finish reasons.
//!
//! The split is deliberate: `messages` must be exact or the model loses the
//! thread, while `runs` is telemetry that could be dropped without breaking
//! the agent. A failed metadata write can never corrupt a conversation.

use anyhow::Result;
use rig_agent::prelude::Message;
use rusqlite::Connection;

/// One `runner::turn()` call: the model calls it made and what they cost.
///
/// Token fields are plain integers rather than Rig's `Usage` so this module
/// stays independent of the agent crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub run_id: String,
    pub session_id: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub model: String,
    /// `ok`, `error`.
    pub status: String,
    pub error: Option<String>,
    /// Range of `messages.seq` this run appended. Equal bounds mean one
    /// message; `last_seq < first_seq` means the run appended none.
    pub first_seq: i64,
    pub last_seq: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub reasoning_tokens: i64,
    pub tool_use_prompt_tokens: i64,
    /// Number of completion requests Rig issued. Not the message count: one
    /// tool-using run makes several model calls and appends more messages.
    pub model_calls: i64,
    /// `Vec<CompletionCall>` as opaque Rig JSON, for per-call detail.
    pub calls_json: String,
}

pub fn open(path: &str) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
             session_id TEXT NOT NULL,
             seq        INTEGER NOT NULL,
             json       TEXT NOT NULL,
             PRIMARY KEY (session_id, seq)
         );
         CREATE TABLE IF NOT EXISTS runs (
             run_id     TEXT PRIMARY KEY,
             session_id TEXT NOT NULL,
             started_at INTEGER NOT NULL,
             ended_at   INTEGER NOT NULL,
             model      TEXT NOT NULL,
             status     TEXT NOT NULL,
             error      TEXT,
             first_seq  INTEGER NOT NULL,
             last_seq   INTEGER NOT NULL,
             input_tokens                INTEGER NOT NULL DEFAULT 0,
             output_tokens               INTEGER NOT NULL DEFAULT 0,
             total_tokens                INTEGER NOT NULL DEFAULT 0,
             cached_input_tokens         INTEGER NOT NULL DEFAULT 0,
             cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
             reasoning_tokens            INTEGER NOT NULL DEFAULT 0,
             tool_use_prompt_tokens      INTEGER NOT NULL DEFAULT 0,
             model_calls INTEGER NOT NULL DEFAULT 0,
             calls_json  TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS runs_by_session ON runs (session_id, started_at);",
    )?;
    Ok(db)
}

pub fn load(db: &Connection, session: &str) -> Result<Vec<Message>> {
    let mut q = db.prepare("SELECT json FROM messages WHERE session_id=?1 ORDER BY seq")?;
    let rows = q.query_map([session], |r| r.get::<_, String>(0))?;
    rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
}

pub fn save(db: &Connection, session: &str, history: &[Message]) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    tx.execute("DELETE FROM messages WHERE session_id=?1", [session])?;
    for (i, m) in history.iter().enumerate() {
        tx.execute(
            "INSERT INTO messages (session_id, seq, json) VALUES (?1, ?2, ?3)",
            rusqlite::params![session, i as i64, serde_json::to_string(m)?],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn save_run(db: &Connection, run: &RunRecord) -> Result<()> {
    db.execute(
        "INSERT INTO runs (
             run_id, session_id, started_at, ended_at, model, status, error,
             first_seq, last_seq, input_tokens, output_tokens, total_tokens,
             cached_input_tokens, cache_creation_input_tokens, reasoning_tokens,
             tool_use_prompt_tokens, model_calls, calls_json
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        rusqlite::params![
            run.run_id,
            run.session_id,
            run.started_at,
            run.ended_at,
            run.model,
            run.status,
            run.error,
            run.first_seq,
            run.last_seq,
            run.input_tokens,
            run.output_tokens,
            run.total_tokens,
            run.cached_input_tokens,
            run.cache_creation_input_tokens,
            run.reasoning_tokens,
            run.tool_use_prompt_tokens,
            run.model_calls,
            run.calls_json,
        ],
    )?;
    Ok(())
}

pub fn sessions(db: &Connection) -> Result<Vec<(String, i64)>> {
    let mut q = db.prepare(
        "SELECT session_id, COUNT(*) FROM messages GROUP BY session_id ORDER BY session_id",
    )?;
    let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Per-session rollup of the `runs` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUsage {
    pub session_id: String,
    pub runs: i64,
    pub model_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

pub fn usage(db: &Connection) -> Result<Vec<SessionUsage>> {
    let mut q = db.prepare(
        "SELECT session_id, COUNT(*), SUM(model_calls), SUM(input_tokens),
                SUM(output_tokens), SUM(cached_input_tokens)
         FROM runs GROUP BY session_id ORDER BY session_id",
    )?;
    let rows = q.query_map([], |r| {
        Ok(SessionUsage {
            session_id: r.get(0)?,
            runs: r.get(1)?,
            model_calls: r.get(2)?,
            input_tokens: r.get(3)?,
            output_tokens: r.get(4)?,
            cached_input_tokens: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE messages (session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                 json TEXT NOT NULL, PRIMARY KEY (session_id, seq));",
        )
        .unwrap();
        db.execute_batch(
            "CREATE TABLE runs (run_id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                 started_at INTEGER NOT NULL, ended_at INTEGER NOT NULL, model TEXT NOT NULL,
                 status TEXT NOT NULL, error TEXT, first_seq INTEGER NOT NULL,
                 last_seq INTEGER NOT NULL, input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0, total_tokens INTEGER NOT NULL DEFAULT 0,
                 cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                 reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                 tool_use_prompt_tokens INTEGER NOT NULL DEFAULT 0,
                 model_calls INTEGER NOT NULL DEFAULT 0, calls_json TEXT NOT NULL);",
        )
        .unwrap();
        db
    }

    fn run_record(run_id: &str, session: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.into(),
            session_id: session.into(),
            started_at: 1_000,
            ended_at: 2_000,
            model: "test/model".into(),
            status: "ok".into(),
            error: None,
            first_seq: 0,
            last_seq: 3,
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cached_input_tokens: 64,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 8,
            tool_use_prompt_tokens: 0,
            model_calls: 2,
            calls_json: "[]".into(),
        }
    }

    #[test]
    fn open_creates_both_tables() {
        let path = std::env::temp_dir().join(format!("athena-{}.db", uuid::Uuid::new_v4()));
        let db = open(path.to_str().unwrap()).unwrap();
        let mut q = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = q
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        drop(q);
        drop(db);
        let _ = std::fs::remove_file(&path);
        assert!(tables.contains(&"messages".to_string()));
        assert!(tables.contains(&"runs".to_string()));
    }

    #[test]
    fn save_then_load_round_trips_messages_in_order() {
        let db = db();
        let history = vec![
            Message::user("first"),
            Message::assistant("second"),
            Message::user("third"),
        ];
        save(&db, "s", &history).unwrap();
        assert_eq!(load(&db, "s").unwrap(), history);
    }

    #[test]
    fn save_replaces_rather_than_appends() {
        let db = db();
        save(&db, "s", &[Message::user("a"), Message::user("b")]).unwrap();
        save(&db, "s", &[Message::user("only")]).unwrap();
        assert_eq!(load(&db, "s").unwrap(), vec![Message::user("only")]);
    }

    #[test]
    fn sessions_are_isolated_from_each_other() {
        let db = db();
        save(&db, "a", &[Message::user("a1")]).unwrap();
        save(&db, "b", &[Message::user("b1"), Message::user("b2")]).unwrap();
        assert_eq!(load(&db, "a").unwrap().len(), 1);
        assert_eq!(load(&db, "b").unwrap().len(), 2);
        assert!(load(&db, "missing").unwrap().is_empty());
        assert_eq!(
            sessions(&db).unwrap(),
            vec![("a".to_string(), 1), ("b".to_string(), 2)]
        );
    }

    #[test]
    fn usage_sums_token_columns_per_session() {
        let db = db();
        save_run(&db, &run_record("r1", "s")).unwrap();
        save_run(&db, &run_record("r2", "s")).unwrap();
        save_run(&db, &run_record("r3", "other")).unwrap();

        let rows = usage(&db).unwrap();
        assert_eq!(rows.len(), 2);
        let s = &rows[1];
        assert_eq!(s.session_id, "s");
        assert_eq!((s.runs, s.model_calls), (2, 4));
        assert_eq!(
            (s.input_tokens, s.output_tokens, s.cached_input_tokens),
            (200, 40, 128)
        );
    }

    #[test]
    fn run_ids_are_unique() {
        let db = db();
        save_run(&db, &run_record("dup", "s")).unwrap();
        assert!(save_run(&db, &run_record("dup", "s")).is_err());
    }

    #[test]
    fn telemetry_is_not_required_for_a_session_to_load() {
        let db = db();
        save(&db, "s", &[Message::user("hi")]).unwrap();
        assert!(usage(&db).unwrap().is_empty());
        assert_eq!(load(&db, "s").unwrap().len(), 1);
    }
}
