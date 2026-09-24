//! Session persistence. Two tables: the conversation, and what each run cost.
//!
//! `messages` holds the transcript the model sees next turn, as opaque Rig
//! JSON. `runs` holds per-run telemetry that Rig reports and the transcript
//! cannot express — token usage, model-call count, finish reasons.
//!
//! The split is deliberate: `messages` must be exact or the model loses the
//! thread, while `runs` is telemetry that could be dropped without breaking
//! the agent. A failed metadata write can never corrupt a conversation.

use anyhow::{Context, Result, bail};
use rig_agent::prelude::Message;
use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};
use std::time::{Duration, Instant};

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

/// Where the database lives: `ATHENA_DB`, or `agent.db` in the working directory.
pub fn path() -> String {
    path_or_default(std::env::var("ATHENA_DB").ok())
}

fn path_or_default(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| "agent.db".into())
}

/// Schema history, oldest first. `PRAGMA user_version` counts how many ran.
///
/// Append only. Never edit or reorder a shipped entry: existing databases
/// have already applied it. Every new migration also needs a fixture of the
/// schema before it and an upgrade test; see TESTING.md.
const MIGRATIONS: &[&str] = &[
    // 1: the transcript. IF NOT EXISTS because databases created before
    // migrations existed already have this table at user_version 0.
    "CREATE TABLE IF NOT EXISTS messages (
         session_id TEXT NOT NULL,
         seq        INTEGER NOT NULL,
         json       TEXT NOT NULL,
         PRIMARY KEY (session_id, seq)
     );",
    // 2: per-run telemetry. IF NOT EXISTS for the same reason: early builds
    // of this table created it without bumping user_version.
    //
    // Migrations after this one are plain DDL.
    "CREATE TABLE IF NOT EXISTS runs (
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
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

/// The columns each table must have once migrated, in order.
///
/// `CREATE TABLE IF NOT EXISTS` accepts an existing table of any shape, so
/// a table left behind by some older build would otherwise pass unnoticed.
const EXPECTED_COLUMNS: &[(&str, &[&str])] = &[
    ("messages", &["session_id", "seq", "json"]),
    (
        "runs",
        &[
            "run_id",
            "session_id",
            "started_at",
            "ended_at",
            "model",
            "status",
            "error",
            "first_seq",
            "last_seq",
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "cached_input_tokens",
            "cache_creation_input_tokens",
            "reasoning_tokens",
            "tool_use_prompt_tokens",
            "model_calls",
            "calls_json",
        ],
    ),
];

pub fn open(path: &str) -> Result<Connection> {
    let db = Connection::open(path).with_context(|| format!("opening {path}"))?;
    // Wait for another process's write lock instead of failing immediately.
    db.busy_timeout(BUSY_TIMEOUT)?;
    // WAL cannot be switched inside a transaction, so it goes before migrate.
    enable_wal(&db, BUSY_TIMEOUT).context("switching to WAL")?;
    migrate(&db).context("migrating schema")?;
    Ok(db)
}

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Switch to WAL, retrying while another connection holds the database.
///
/// SQLite answers the journal-mode switch with SQLITE_BUSY without calling
/// the busy handler, so `busy_timeout` does not cover it. Two processes
/// opening a new database at once hit this.
fn enable_wal(db: &Connection, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match db.pragma_update(None, "journal_mode", "WAL") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::DatabaseBusy && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return Ok(result?),
        }
    }
}

/// A fully migrated database that lives only as long as the connection.
pub fn open_in_memory() -> Result<Connection> {
    let db = Connection::open_in_memory()?;
    migrate(&db)?;
    Ok(db)
}

/// Bring the schema up to `SCHEMA_VERSION`, then check its shape.
///
/// Refuses a database from a newer build rather than writing into a schema
/// this code does not understand.
fn migrate(db: &Connection) -> Result<()> {
    // IMMEDIATE takes the write lock before the version is read, so two
    // processes opening a fresh database cannot both run a migration.
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let version = usize::try_from(version)?;
    if version > SCHEMA_VERSION {
        bail!(
            "database is at schema version {version}, newer than this build \
             ({SCHEMA_VERSION}); refusing to open it with an older athena"
        );
    }
    for sql in &MIGRATIONS[version..] {
        tx.execute_batch(sql)?;
    }
    // Checked before commit, so a refused database is left exactly as found.
    for (table, expected) in EXPECTED_COLUMNS {
        let actual = columns(&tx, table)?;
        if actual != *expected {
            bail!("table `{table}` has columns {actual:?}, expected {expected:?}");
        }
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION as i64)?;
    tx.commit()?;
    Ok(())
}

fn columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    let mut q = db.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    let rows = q.query_map([table], |r| r.get(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
        open_in_memory().unwrap()
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
    fn athena_db_overrides_the_default_path() {
        assert_eq!(path_or_default(Some("/tmp/x.db".into())), "/tmp/x.db");
        assert_eq!(path_or_default(None), "agent.db");
    }

    fn user_version(db: &Connection) -> i64 {
        db.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let db = db();
        save_run(&db, &run_record("r1", "s")).unwrap();
        migrate(&db).unwrap();
        assert_eq!(user_version(&db), SCHEMA_VERSION as i64);
        assert_eq!(usage(&db).unwrap().len(), 1);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "user_version", SCHEMA_VERSION as i64 + 1)
            .unwrap();
        let err = migrate(&db).unwrap_err().to_string();
        assert!(err.contains("newer than this build"), "{err}");
        // Nothing was created in a schema this build does not understand.
        assert!(columns(&db, "messages").unwrap().is_empty());
    }

    #[test]
    fn a_corrupt_schema_version_is_refused() {
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "user_version", -1).unwrap();
        assert!(migrate(&db).is_err());
    }

    #[test]
    fn a_table_of_the_wrong_shape_is_refused_not_silently_accepted() {
        let db = Connection::open_in_memory().unwrap();
        // Enough columns for the index to build, so only the shape check
        // stands between this table and a silent IF NOT EXISTS.
        db.execute_batch("CREATE TABLE runs (session_id TEXT, started_at INTEGER)")
            .unwrap();
        let err = migrate(&db).unwrap_err().to_string();
        assert!(err.contains("table `runs` has columns"), "{err}");
        // Rolled back: no version bump, no messages table.
        assert_eq!(user_version(&db), 0);
        assert!(columns(&db, "messages").unwrap().is_empty());
    }

    /// A fresh file with a reader holding it open in rollback-journal mode,
    /// which is what a racing process's first open looks like.
    fn locked_fresh_db() -> (std::path::PathBuf, Connection) {
        let path = std::env::temp_dir().join(format!("athena-{}.db", uuid::Uuid::new_v4()));
        let holder = Connection::open(&path).unwrap();
        holder
            .execute_batch("CREATE TABLE t (x); BEGIN; SELECT * FROM t;")
            .unwrap();
        (path, holder)
    }

    /// A connection whose busy handler gives up at once, so SQLITE_BUSY
    /// reaches `enable_wal` the way it does in a real open race.
    fn impatient(path: &std::path::Path) -> Connection {
        let db = Connection::open(path).unwrap();
        db.busy_timeout(Duration::ZERO).unwrap();
        db
    }

    #[test]
    fn enable_wal_waits_out_a_lock_the_busy_handler_does_not_cover() {
        let (path, holder) = locked_fresh_db();
        let db = impatient(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            holder.execute_batch("COMMIT").unwrap();
        });

        enable_wal(&db, Duration::from_secs(5)).unwrap();

        release.join().unwrap();
        let mode: String = db
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn enable_wal_gives_up_at_its_deadline() {
        let (path, holder) = locked_fresh_db();
        let db = impatient(&path);

        let err = enable_wal(&db, Duration::from_millis(30)).unwrap_err();

        assert!(err.to_string().contains("locked"), "{err}");
        drop((db, holder));
        let _ = std::fs::remove_file(&path);
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
    fn a_save_that_fails_midway_keeps_the_previous_history() {
        let db = db();
        let before = vec![Message::user("a"), Message::assistant("b")];
        save(&db, "s", &before).unwrap();
        // Let the DELETE and the first INSERT through, then fail, as a full
        // disk would.
        db.execute_batch(
            "CREATE TRIGGER fail BEFORE INSERT ON messages WHEN NEW.seq = 1
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();

        let err = save(&db, "s", &[Message::user("x"), Message::user("y")]).unwrap_err();

        assert!(err.to_string().contains("disk full"), "{err}");
        assert_eq!(load(&db, "s").unwrap(), before);
    }

    #[test]
    fn listing_reports_a_missing_table_as_an_error() {
        let db = db();
        db.execute_batch("DROP TABLE messages; DROP TABLE runs;")
            .unwrap();
        assert!(sessions(&db).is_err());
        assert!(usage(&db).is_err());
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
