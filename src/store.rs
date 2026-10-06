//! Persistence: who the users are, which sessions they own, each session's
//! transcript, and what each run cost.
//!
//! `messages` holds the transcript the model sees next turn, as opaque Rig
//! JSON. It is append-only: a turn inserts its new rows and never rewrites
//! earlier ones. `runs` holds per-run telemetry that Rig reports and the
//! transcript cannot express — token usage, model-call count, finish reasons.
//!
//! The split is deliberate: `messages` must be exact or the model loses the
//! thread, while `runs` is telemetry that could be dropped without breaking
//! the agent. A failed metadata write can never corrupt a conversation.
//!
//! [`Store`] is the one handle to the database a process holds. Everything
//! outside this file goes through it, so the choice of handle (one connection
//! behind a mutex, see the README) can change here without touching callers.

use crate::{compaction, media};
use anyhow::{Context, Result, bail};
use rig_agent::prelude::Message;
use rig_core::memory::{ConversationMemory, MemoryError};
use rig_core::wasm_compat::WasmBoxedFuture;
use rusqlite::{Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One turn: the model calls it made and what they cost.
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

/// A shared owner and the transport identity used for this request.
///
/// The fields are private and there is no constructor outside this module:
/// a `User` only comes from [`Store::user`], so a transport cannot build one
/// for somebody else and read their sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    id: i64,
    transport: String,
    external_id: String,
}

impl User {
    pub fn id(&self) -> i64 {
        self.id
    }

    /// `cli`, and later `http`, `telegram`, ...
    pub fn transport(&self) -> &str {
        &self.transport
    }

    /// The transport's own id for this person: a Telegram chat id, say.
    pub fn external_id(&self) -> &str {
        &self.external_id
    }
}

/// One conversation. Belongs to exactly one user; its name is unique among
/// that user's sessions, its id unique everywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub name: String,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session: Session,
    pub messages: i64,
}

/// Per-session rollup of the `runs` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUsage {
    pub session_id: String,
    pub name: String,
    pub runs: i64,
    pub model_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

/// The sandbox a session's tools run in. See `sandbox::Sandboxes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRow {
    pub session_id: String,
    pub sandbox_id: String,
    pub bash_session: Option<String>,
    pub code_language: Option<String>,
    pub code_context: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
    pub expires_at: i64,
}

/// A compaction: a summary standing in for every message of a session up to
/// and including `through_seq`. The messages themselves stay in `messages`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub through_seq: i64,
    pub summary: String,
    /// The model that wrote the summary.
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
}

/// Why an append wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendError {
    /// The transcript grew between this turn's load and its append: another
    /// process ran a turn on the same session. Writing now would interleave
    /// two conversations that never saw each other.
    Conflict { expected: i64, found: i64 },
    /// The write itself failed.
    Storage(String),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { expected, found } => write!(
                f,
                "the session gained messages during this turn (expected the next \
                 seq to be {expected}, found {found})"
            ),
            Self::Storage(e) => write!(f, "transcript not saved: {e}"),
        }
    }
}

impl std::error::Error for AppendError {}

fn storage(e: impl std::fmt::Display) -> AppendError {
    AppendError::Storage(e.to_string())
}

/// What the conversation memory did during one turn.
///
/// Rig calls [`SqliteMemory`] itself and only logs a failed append, then
/// returns the reply as if it had been saved. The service reads this after
/// the run so that an unsaved transcript is an error, not a log line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The seq the turn's first message will get, as the load saw it.
    pub loaded_next: Option<i64>,
    /// The seq of each message the load returned, in order. After a
    /// compaction the first is the summary, which has the seq of the last
    /// message it covers.
    pub loaded: Vec<i64>,
    /// Whether the first loaded message is a compaction's summary.
    pub loaded_summary: bool,
    /// The seq range the append wrote, or why it wrote nothing.
    pub appended: Option<std::result::Result<(i64, i64), AppendError>>,
}

/// Where the database lives: `ATHENA_DB`, or `agent.db` in the working directory.
pub fn path() -> String {
    path_or_default(std::env::var("ATHENA_DB").ok())
}

fn path_or_default(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| "agent.db".into())
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
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
    // 3: users and the sessions they own.
    //
    // Every session that already has messages or runs is given to the CLI's
    // local user, under its old id as both id and name, so no `messages` row
    // changes. New sessions get uuid ids.
    //
    // `runs` is rebuilt, because SQLite cannot add NOT NULL to `run_id` (a
    // TEXT PRIMARY KEY accepts NULL) or a foreign key to an existing table.
    // Rows keep their rowid; a NULL run_id, which no build writes, becomes
    // `legacy-<rowid>` rather than failing the upgrade.
    //
    // `messages` is not rebuilt: copying every transcript to add a foreign
    // key is a risk to the one table that must stay exact. Two triggers give
    // it the same guarantee instead.
    "CREATE TABLE users (
         id          INTEGER PRIMARY KEY,
         transport   TEXT NOT NULL,
         external_id TEXT NOT NULL,
         created_at  INTEGER NOT NULL,
         UNIQUE (transport, external_id)
     );
     CREATE TABLE sessions (
         id         TEXT NOT NULL PRIMARY KEY,
         user_id    INTEGER NOT NULL REFERENCES users (id),
         name       TEXT NOT NULL,
         created_at INTEGER NOT NULL,
         UNIQUE (user_id, name)
     );
     INSERT INTO users (transport, external_id, created_at)
     VALUES ('cli', 'local', CAST(unixepoch('subsec') * 1000 AS INTEGER));
     INSERT INTO sessions (id, user_id, name, created_at)
     SELECT existing.session_id, users.id, existing.session_id, users.created_at
     FROM (SELECT session_id FROM messages UNION SELECT session_id FROM runs) AS existing,
          users
     WHERE users.transport = 'cli' AND users.external_id = 'local';

     CREATE TABLE runs_v3 (
         run_id     TEXT NOT NULL PRIMARY KEY,
         session_id TEXT NOT NULL REFERENCES sessions (id),
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
     INSERT INTO runs_v3 (
         rowid, run_id, session_id, started_at, ended_at, model, status, error,
         first_seq, last_seq, input_tokens, output_tokens, total_tokens,
         cached_input_tokens, cache_creation_input_tokens, reasoning_tokens,
         tool_use_prompt_tokens, model_calls, calls_json
     )
     SELECT rowid, COALESCE(run_id, 'legacy-' || rowid), session_id, started_at,
            ended_at, model, status, error, first_seq, last_seq, input_tokens,
            output_tokens, total_tokens, cached_input_tokens,
            cache_creation_input_tokens, reasoning_tokens, tool_use_prompt_tokens,
            model_calls, calls_json
     FROM runs;
     DROP TABLE runs;
     ALTER TABLE runs_v3 RENAME TO runs;
     CREATE INDEX runs_by_session ON runs (session_id, started_at);

     CREATE TRIGGER messages_need_a_session BEFORE INSERT ON messages
     WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id = NEW.session_id)
     BEGIN SELECT RAISE(ABORT, 'no such session'); END;
     CREATE TRIGGER messages_keep_a_session BEFORE UPDATE OF session_id ON messages
     WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id = NEW.session_id)
     BEGIN SELECT RAISE(ABORT, 'no such session'); END;",
    // 4: the session each user is currently talking in, for transports that
    // keep a conversation going across messages (Telegram). At most one per
    // user. Nothing enforces here that the session is the user's own:
    // `Store::select_session` only writes one that is, and
    // `Store::selected_session` only reads one that is.
    "CREATE TABLE selected_sessions (
         user_id     INTEGER NOT NULL PRIMARY KEY REFERENCES users (id),
         session_id  TEXT NOT NULL REFERENCES sessions (id),
         selected_at INTEGER NOT NULL
     );",
    // 5: the OpenSandbox sandbox each session's tools run in, at most one
    // per session. `bash_session` and `code_context` are execd's ids for the
    // persistent shell and interpreter inside that sandbox, created on first
    // use. `expires_at` is when Athena last asked the sandbox to expire, in
    // Athena's clock; the server's own clock decides.
    "CREATE TABLE sandboxes (
         session_id    TEXT NOT NULL PRIMARY KEY REFERENCES sessions (id),
         sandbox_id    TEXT NOT NULL,
         bash_session  TEXT,
         code_language TEXT,
         code_context  TEXT,
         created_at    INTEGER NOT NULL,
         expires_at    INTEGER NOT NULL
     );",
    // 6: signing in to websites through a session's browser. A link's
    // token opens that session's browser for the user until `expires_at`;
    // `url` is where it starts. `browser_states` is each user's saved
    // browser state (agent-browser's `state save` JSON: cookies and local
    // storage), loaded into every new sandbox of theirs.
    "CREATE TABLE browser_links (
         token      TEXT NOT NULL PRIMARY KEY,
         session_id TEXT NOT NULL REFERENCES sessions (id),
         url        TEXT NOT NULL,
         expires_at INTEGER NOT NULL
     );
     CREATE TABLE browser_states (
         user_id  INTEGER NOT NULL PRIMARY KEY REFERENCES users (id),
         state    BLOB NOT NULL,
         saved_at INTEGER NOT NULL
     );",
    // 7: compactions. A row says that `summary` stands in for every message
    // of the session up to and including `through_seq`; the messages are
    // not touched. A session loads its newest checkpoint (the highest
    // `through_seq`) followed by the rows after it. The key stops two
    // processes that compact the same range from both writing it.
    "CREATE TABLE compactions (
         session_id    TEXT NOT NULL REFERENCES sessions (id),
         through_seq   INTEGER NOT NULL,
         summary       TEXT NOT NULL,
         model         TEXT NOT NULL,
         input_tokens  INTEGER NOT NULL DEFAULT 0,
         output_tokens INTEGER NOT NULL DEFAULT 0,
         created_at    INTEGER NOT NULL,
         PRIMARY KEY (session_id, through_seq)
     );",
    // 8: multiple transport identities can resolve to one existing user id.
    // The original identity in users remains unchanged for compatibility;
    // user_identities is now the authority for identity lookup. No sessions,
    // transcripts or saved browser states move during this migration.
    "CREATE TABLE user_identities (
         transport   TEXT NOT NULL,
         external_id TEXT NOT NULL,
         user_id     INTEGER NOT NULL REFERENCES users (id),
         created_at  INTEGER NOT NULL,
         PRIMARY KEY (transport, external_id)
     );
     INSERT INTO user_identities (transport, external_id, user_id, created_at)
     SELECT transport, external_id, id, created_at FROM users;",
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

const RUN_COLUMNS: &[&str] = &[
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
];

/// The columns each table must have once migrated, in order.
///
/// `CREATE TABLE IF NOT EXISTS` accepts an existing table of any shape, so
/// a table left behind by some older build would otherwise pass unnoticed.
const EXPECTED_COLUMNS: &[(&str, &[&str])] = &[
    ("messages", &["session_id", "seq", "json"]),
    ("runs", RUN_COLUMNS),
    ("users", &["id", "transport", "external_id", "created_at"]),
    (
        "user_identities",
        &["transport", "external_id", "user_id", "created_at"],
    ),
    ("sessions", &["id", "user_id", "name", "created_at"]),
    (
        "selected_sessions",
        &["user_id", "session_id", "selected_at"],
    ),
    (
        "sandboxes",
        &[
            "session_id",
            "sandbox_id",
            "bash_session",
            "code_language",
            "code_context",
            "created_at",
            "expires_at",
        ],
    ),
    (
        "browser_links",
        &["token", "session_id", "url", "expires_at"],
    ),
    ("browser_states", &["user_id", "state", "saved_at"]),
    (
        "compactions",
        &[
            "session_id",
            "through_seq",
            "summary",
            "model",
            "input_tokens",
            "output_tokens",
            "created_at",
        ],
    ),
];

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Settings every connection needs, file or memory.
///
/// Foreign keys are enforced per connection in SQLite, not per database, so
/// a connection opened any other way (the `sqlite3` shell included) does not
/// check them.
fn configure(db: &Connection) -> Result<()> {
    // Wait for another process's write lock instead of failing immediately.
    db.busy_timeout(BUSY_TIMEOUT)?;
    // Cannot be changed inside a transaction, so it goes before migrate.
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

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
    let orphans: i64 = tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
        r.get(0)
    })?;
    if orphans > 0 {
        bail!("{orphans} rows reference a user or session that does not exist");
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

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        name: r.get(1)?,
        created_at: r.get(2)?,
    })
}

fn sandbox_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SandboxRow> {
    Ok(SandboxRow {
        session_id: r.get(0)?,
        sandbox_id: r.get(1)?,
        bash_session: r.get(2)?,
        code_language: r.get(3)?,
        code_context: r.get(4)?,
        created_at: r.get(5)?,
        expires_at: r.get(6)?,
    })
}

fn read_history(db: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let mut q = db.prepare("SELECT json FROM messages WHERE session_id=?1 ORDER BY seq")?;
    let rows = q.query_map([session_id], |r| r.get::<_, String>(0))?;
    rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
}

/// The transcript a turn starts from: the newest compaction's summary, if
/// there is one, then every message after it.
struct Visible {
    messages: Vec<Message>,
    /// The seq of each message. The summary has the seq of the last message
    /// it stands for.
    seqs: Vec<i64>,
    summarized: bool,
}

fn newest_checkpoint(db: &Connection, session_id: &str) -> rusqlite::Result<Option<Checkpoint>> {
    db.query_row(
        "SELECT through_seq, summary, model, input_tokens, output_tokens, created_at
         FROM compactions WHERE session_id = ?1
         ORDER BY through_seq DESC LIMIT 1",
        [session_id],
        |r| {
            Ok(Checkpoint {
                through_seq: r.get(0)?,
                summary: r.get(1)?,
                model: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                created_at: r.get(5)?,
            })
        },
    )
    .optional()
}

fn read_visible(db: &Connection, session_id: &str) -> Result<Visible> {
    let checkpoint = newest_checkpoint(db, session_id)?;
    let mut visible = Visible {
        messages: Vec::new(),
        seqs: Vec::new(),
        summarized: checkpoint.is_some(),
    };
    let mut after = -1;
    if let Some(checkpoint) = checkpoint {
        visible
            .messages
            .push(compaction::summary_message(&checkpoint.summary));
        visible.seqs.push(checkpoint.through_seq);
        after = checkpoint.through_seq;
    }
    let sql = "SELECT seq, json FROM messages WHERE session_id = ?1 AND seq > ?2 ORDER BY seq";
    let mut q = db.prepare(sql)?;
    let rows = q.query_map(rusqlite::params![session_id, after], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (seq, json) = row?;
        visible.messages.push(serde_json::from_str(&json)?);
        visible.seqs.push(seq);
    }
    Ok(visible)
}

fn next_seq_in(db: &Connection, session_id: &str) -> rusqlite::Result<i64> {
    db.query_row(
        "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
}

type TurnLock = Arc<tokio::sync::Mutex<()>>;

/// The process's handle to one database. Cheap to clone; clones share it.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    /// The one connection. Held only for synchronous SQL, never across an
    /// `.await`; see [`Store::call`].
    db: Mutex<Connection>,
    /// Memory receipts of the turns in flight, by session id.
    receipts: Mutex<HashMap<String, Receipt>>,
    /// One lock per session with a turn in flight or waiting. Lives with the
    /// store, not the service, so every service sharing a store also shares
    /// the locks that make its receipts safe to key by session.
    turns: Mutex<HashMap<String, TurnLock>>,
}

/// Lock a std mutex, carrying on past a panic in a previous holder: every
/// write here is a transaction, and rusqlite rolls back one it drops.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("opening {path}"))?;
        configure(&db)?;
        // WAL cannot be switched inside a transaction, so it goes before migrate.
        enable_wal(&db, BUSY_TIMEOUT).context("switching to WAL")?;
        migrate(&db).context("migrating schema")?;
        Ok(Self::new(db))
    }

    /// A fully migrated database that lives only as long as the store.
    pub fn open_in_memory() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        configure(&db)?;
        migrate(&db)?;
        Ok(Self::new(db))
    }

    fn new(db: Connection) -> Self {
        Self {
            inner: Arc::new(Inner {
                db: Mutex::new(db),
                receipts: Mutex::default(),
                turns: Mutex::default(),
            }),
        }
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        lock(&self.inner.db)
    }

    /// The raw connection, for tests that set up or inspect state directly.
    #[cfg(test)]
    pub(crate) fn db_for_tests(&self) -> MutexGuard<'_, Connection> {
        self.db()
    }

    /// Rig conversation memory over this store's `messages` table. Give it
    /// to the agent's builder: `AgentBuilder::memory(store.memory())`.
    pub fn memory(&self) -> SqliteMemory {
        SqliteMemory {
            store: self.clone(),
        }
    }

    /// Run blocking store work off the async executor.
    ///
    /// Each call is short, but another process holding the write lock can
    /// make it wait up to the busy timeout; on a runtime worker thread that
    /// wait would stall every other task scheduled there. A panic in `f` is
    /// re-raised in the caller.
    pub async fn call<T: Send + 'static>(&self, f: impl FnOnce(&Store) -> T + Send + 'static) -> T {
        let store = self.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
    }

    /// The user a transport knows by `external_id`, created on first sight.
    pub fn user(&self, transport: &str, external_id: &str) -> Result<User> {
        let db = self.db();
        // Serialize lookup/create with linking, including in other processes.
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        let found: Option<i64> = tx
            .query_row(
                "SELECT user_id FROM user_identities WHERE transport = ?1 AND external_id = ?2",
                [transport, external_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = match found {
            Some(id) => id,
            None => {
                let created_at = now_millis();
                tx.execute(
                    "INSERT INTO users (transport, external_id, created_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![transport, external_id, created_at],
                )?;
                let id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO user_identities (transport, external_id, user_id, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![transport, external_id, id, created_at],
                )?;
                id
            }
        };
        tx.commit()?;
        Ok(User {
            id,
            transport: transport.to_string(),
            external_id: external_id.to_string(),
        })
    }

    /// Bind a never-used HTTP identity to this owner. Repeating the same
    /// binding succeeds; an identity belonging to another owner never moves.
    /// The service validates the name and restricts this to Telegram callers.
    pub(crate) fn link_http_user(&self, owner: &User, external_id: &str) -> Result<bool> {
        let db = self.db();
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO user_identities (transport, external_id, user_id, created_at)
             VALUES ('http', ?1, ?2, ?3)
             ON CONFLICT (transport, external_id) DO NOTHING",
            rusqlite::params![external_id, owner.id, now_millis()],
        )?;
        let linked: i64 = tx.query_row(
            "SELECT user_id FROM user_identities WHERE transport = 'http' AND external_id = ?1",
            [external_id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(linked == owner.id)
    }

    /// A new session for `user`, or `None` if they already have one by that name.
    pub fn create_session(&self, user: &User, name: &str) -> Result<Option<Session>> {
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            created_at: now_millis(),
        };
        let inserted = self.db().execute(
            "INSERT INTO sessions (id, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user_id, name) DO NOTHING",
            rusqlite::params![session.id, user.id, session.name, session.created_at],
        )?;
        Ok((inserted == 1).then_some(session))
    }

    /// `user`'s session called `name`, created if they have none.
    pub fn open_session(&self, user: &User, name: &str) -> Result<Session> {
        // The insert and the read share one write lock, so two processes
        // opening the same new name both get the one session.
        let db = self.db();
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO sessions (id, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user_id, name) DO NOTHING",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                user.id,
                name,
                now_millis()
            ],
        )?;
        let session = tx.query_row(
            "SELECT id, name, created_at FROM sessions WHERE user_id = ?1 AND name = ?2",
            rusqlite::params![user.id, name],
            session_row,
        )?;
        tx.commit()?;
        Ok(session)
    }

    /// The session with this id, if `user` owns it. Someone else's session
    /// and a missing one look the same.
    pub fn session(&self, user: &User, id: &str) -> Result<Option<Session>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, name, created_at FROM sessions WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![id, user.id],
                session_row,
            )
            .optional()?)
    }

    /// `user`'s sessions by name, with message counts. Empty sessions included.
    pub fn sessions(&self, user: &User) -> Result<Vec<SessionSummary>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT s.id, s.name, s.created_at, COUNT(m.seq)
             FROM sessions AS s LEFT JOIN messages AS m ON m.session_id = s.id
             WHERE s.user_id = ?1 GROUP BY s.id ORDER BY s.name",
        )?;
        let rows = q.query_map([user.id], |r| {
            Ok(SessionSummary {
                session: session_row(r)?,
                messages: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Token totals for each of `user`'s sessions that has at least one run.
    pub fn usage(&self, user: &User) -> Result<Vec<SessionUsage>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT s.id, s.name, COUNT(*), SUM(r.model_calls), SUM(r.input_tokens),
                    SUM(r.output_tokens), SUM(r.cached_input_tokens)
             FROM runs AS r JOIN sessions AS s ON s.id = r.session_id
             WHERE s.user_id = ?1 GROUP BY s.id ORDER BY s.name",
        )?;
        let rows = q.query_map([user.id], |r| {
            Ok(SessionUsage {
                session_id: r.get(0)?,
                name: r.get(1)?,
                runs: r.get(2)?,
                model_calls: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cached_input_tokens: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Make `session_id` the session `user` is talking in. Returns false,
    /// and changes nothing, unless `user` owns that session.
    pub fn select_session(&self, user: &User, session_id: &str) -> Result<bool> {
        let changed = self.db().execute(
            "INSERT INTO selected_sessions (user_id, session_id, selected_at)
             SELECT user_id, id, ?3 FROM sessions WHERE id = ?2 AND user_id = ?1
             ON CONFLICT (user_id) DO UPDATE
             SET session_id = excluded.session_id, selected_at = excluded.selected_at",
            rusqlite::params![user.id, session_id, now_millis()],
        )?;
        Ok(changed == 1)
    }

    /// The session `user` last selected, if any and if it is still theirs.
    pub fn selected_session(&self, user: &User) -> Result<Option<Session>> {
        Ok(self
            .db()
            .query_row(
                "SELECT s.id, s.name, s.created_at
                 FROM selected_sessions AS c
                 JOIN sessions AS s ON s.id = c.session_id AND s.user_id = c.user_id
                 WHERE c.user_id = ?1",
                [user.id],
                session_row,
            )
            .optional()?)
    }

    /// The id of the user who owns a session, if the session exists.
    pub fn session_owner(&self, session_id: &str) -> Result<Option<i64>> {
        Ok(self
            .db()
            .query_row(
                "SELECT user_id FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The sandbox recorded for a session, if any.
    pub fn sandbox(&self, session_id: &str) -> Result<Option<SandboxRow>> {
        Ok(self
            .db()
            .query_row(
                "SELECT session_id, sandbox_id, bash_session, code_language, code_context,
                        created_at, expires_at
                 FROM sandboxes WHERE session_id = ?1",
                [session_id],
                sandbox_row,
            )
            .optional()?)
    }

    /// Record a session's sandbox. Returns false, and changes nothing, if
    /// the session already has one: another process got there first.
    pub fn insert_sandbox(&self, row: &SandboxRow) -> Result<bool> {
        let inserted = self.db().execute(
            "INSERT INTO sandboxes (session_id, sandbox_id, bash_session, code_language,
                                    code_context, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (session_id) DO NOTHING",
            rusqlite::params![
                row.session_id,
                row.sandbox_id,
                row.bash_session,
                row.code_language,
                row.code_context,
                row.created_at,
                row.expires_at
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Update a session's sandbox row, but only while it still names
    /// `row.sandbox_id`: a replacement recorded meanwhile is left alone.
    pub fn update_sandbox(&self, row: &SandboxRow) -> Result<()> {
        self.db().execute(
            "UPDATE sandboxes SET bash_session = ?3, code_language = ?4, code_context = ?5,
                                  expires_at = ?6
             WHERE session_id = ?1 AND sandbox_id = ?2",
            rusqlite::params![
                row.session_id,
                row.sandbox_id,
                row.bash_session,
                row.code_language,
                row.code_context,
                row.expires_at
            ],
        )?;
        Ok(())
    }

    /// Forget a session's sandbox, if it is still `sandbox_id`.
    pub fn remove_sandbox(&self, session_id: &str, sandbox_id: &str) -> Result<()> {
        self.db().execute(
            "DELETE FROM sandboxes WHERE session_id = ?1 AND sandbox_id = ?2",
            [session_id, sandbox_id],
        )?;
        Ok(())
    }

    /// Record a sign-in link to a session's browser, starting at `url`.
    pub fn insert_browser_link(
        &self,
        token: &str,
        session_id: &str,
        url: &str,
        expires_at: i64,
    ) -> Result<()> {
        self.db().execute(
            "INSERT INTO browser_links (token, session_id, url, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![token, session_id, url, expires_at],
        )?;
        Ok(())
    }

    /// The session and start URL a sign-in link opens, if it has not
    /// expired by `now`.
    pub fn browser_link(&self, token: &str, now: i64) -> Result<Option<(String, String)>> {
        Ok(self
            .db()
            .query_row(
                "SELECT session_id, url FROM browser_links
                 WHERE token = ?1 AND expires_at > ?2",
                rusqlite::params![token, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Keep `state` as the browser state of the session's owner, replacing
    /// the last one they saved from any session.
    pub fn save_browser_state(&self, session_id: &str, state: &[u8], saved_at: i64) -> Result<()> {
        let saved = self.db().execute(
            "INSERT INTO browser_states (user_id, state, saved_at)
             SELECT user_id, ?2, ?3 FROM sessions WHERE id = ?1
             ON CONFLICT (user_id) DO UPDATE SET state = excluded.state,
                                                 saved_at = excluded.saved_at",
            rusqlite::params![session_id, state, saved_at],
        )?;
        if saved == 0 {
            bail!("no session `{session_id}`");
        }
        Ok(())
    }

    /// The browser state the session's owner saved last, if any.
    pub fn browser_state(&self, session_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db()
            .query_row(
                "SELECT b.state FROM browser_states b
                 JOIN sessions s ON s.user_id = b.user_id
                 WHERE s.id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// A session's transcript, oldest first. Does not check ownership:
    /// callers look the session up through [`Store::session`] first.
    pub(crate) fn load(&self, session_id: &str) -> Result<Vec<Message>> {
        read_history(&self.db(), session_id)
    }

    /// The seq a session's next message will get.
    pub(crate) fn next_seq(&self, session_id: &str) -> Result<i64> {
        Ok(next_seq_in(&self.db(), session_id)?)
    }

    /// Record that `checkpoint.summary` stands in for the session's messages
    /// up to `checkpoint.through_seq`. Returns whether it was written: a
    /// checkpoint that is not newer than the session's newest (another
    /// process got there first) changes nothing. One that points at a message
    /// the session does not have is an error.
    pub(crate) fn save_checkpoint(
        &self,
        session_id: &str,
        checkpoint: &Checkpoint,
    ) -> Result<bool> {
        let db = self.db();
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        let next = next_seq_in(&tx, session_id)?;
        if !(0..next).contains(&checkpoint.through_seq) {
            bail!(
                "a checkpoint through seq {} does not fit a session whose next seq is {next}",
                checkpoint.through_seq
            );
        }
        // The first message the session loads after the summary must not be
        // a tool result: its call would be hidden, and a provider refuses
        // that on every turn. Nothing deletes a checkpoint, so this is the
        // only place to stop it.
        let first_kept = tx
            .query_row(
                "SELECT json FROM messages WHERE session_id = ?1 AND seq = ?2",
                rusqlite::params![session_id, checkpoint.through_seq + 1],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if let Some(json) = first_kept
            && !compaction::may_start_the_kept(&serde_json::from_str(&json)?)
        {
            bail!(
                "a checkpoint through seq {} would leave a tool result without its call",
                checkpoint.through_seq
            );
        }
        let newest = newest_checkpoint(&tx, session_id)?;
        if newest.is_some_and(|n| n.through_seq >= checkpoint.through_seq) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO compactions (
                 session_id, through_seq, summary, model, input_tokens, output_tokens, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                session_id,
                checkpoint.through_seq,
                checkpoint.summary,
                checkpoint.model,
                checkpoint.input_tokens,
                checkpoint.output_tokens,
                checkpoint.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The prompt tokens the provider reported for the last model call of
    /// the session's newest run that saved messages, which is how big the
    /// session's context was when that run ended. `None` if no run reported
    /// any. Compaction runs append nothing and are skipped.
    ///
    /// Also `None` when that run compacted (a summary call that succeeded
    /// began while it ran) but no checkpoint was saved since it began: its
    /// last call was sent with the summary, but the next load is the whole
    /// transcript, which that number is far too small for.
    pub(crate) fn last_prompt_tokens(&self, session_id: &str) -> Result<Option<u64>> {
        let newest: Option<(Option<i64>, bool)> = self
            .db()
            .query_row(
                "SELECT json_extract(r.calls_json, '$[#-1].usage.input_tokens'),
                        EXISTS (SELECT 1 FROM runs s
                                WHERE s.session_id = r.session_id AND s.status = 'ok'
                                      AND s.last_seq < s.first_seq
                                      AND s.started_at BETWEEN r.started_at AND r.ended_at
                                      AND json_extract(s.calls_json, '$[0].purpose') = 'compaction')
                        AND NOT EXISTS (SELECT 1 FROM compactions c
                                        WHERE c.session_id = r.session_id
                                              AND c.created_at >= r.started_at)
                 FROM runs r
                 WHERE r.session_id = ?1 AND r.status = 'ok' AND r.last_seq >= r.first_seq
                       AND r.model_calls > 0
                 ORDER BY r.started_at DESC, r.rowid DESC LIMIT 1",
                [session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(newest
            .filter(|(_, unsaved)| !unsaved)
            .and_then(|(tokens, _)| tokens)
            .and_then(|t| u64::try_from(t).ok()))
    }

    /// Add `messages` after the session's last row, in one transaction.
    ///
    /// With `expected_next`, refuses to write unless the next seq is still
    /// that one. Earlier rows are never touched. Returns the seq range written.
    /// Images are not stored (see [`media::strip_images`]): the turn they
    /// came in has used them, and a later turn would pay for them again.
    pub(crate) fn append(
        &self,
        session_id: &str,
        expected_next: Option<i64>,
        messages: &[Message],
    ) -> std::result::Result<(i64, i64), AppendError> {
        let db = self.db();
        let tx =
            Transaction::new_unchecked(&db, TransactionBehavior::Immediate).map_err(storage)?;
        let next = next_seq_in(&tx, session_id).map_err(storage)?;
        if let Some(expected) = expected_next
            && expected != next
        {
            return Err(AppendError::Conflict {
                expected,
                found: next,
            });
        }
        for (i, m) in media::strip_images(messages).iter().enumerate() {
            tx.execute(
                "INSERT INTO messages (session_id, seq, json) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    session_id,
                    next + i as i64,
                    serde_json::to_string(m).map_err(storage)?
                ],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok((next, next + messages.len() as i64 - 1))
    }

    pub(crate) fn save_run(&self, run: &RunRecord) -> Result<()> {
        self.db().execute(
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

    /// Wait for the session's turn lock. Holding the guard is holding the
    /// session: a second turn on it waits here until the first one ends.
    ///
    /// Owned, so a streaming turn can move it into the task that drives it.
    pub(crate) async fn lock_session(&self, session_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let session_lock = {
            let mut turns = lock(&self.inner.turns);
            // Only the map holds a lock nobody is using or waiting for. Drop
            // those so the map stays as small as the turns in flight.
            turns.retain(|_, l| Arc::strong_count(l) > 1);
            turns.entry(session_id.to_string()).or_default().clone()
        };
        session_lock.lock_owned().await
    }

    /// How many turns hold or wait for a session's lock.
    #[cfg(test)]
    pub(crate) fn turns_on(&self, session_id: &str) -> usize {
        lock(&self.inner.turns)
            .get(session_id)
            .map_or(0, |l| Arc::strong_count(l) - 1)
    }

    /// Start a turn's receipt from empty. Call with the session lock held.
    pub(crate) fn begin_turn(&self, session_id: &str) {
        lock(&self.inner.receipts).insert(session_id.to_string(), Receipt::default());
    }

    /// What the memory did since [`Store::begin_turn`]. Call with the session lock held.
    pub(crate) fn take_receipt(&self, session_id: &str) -> Receipt {
        lock(&self.inner.receipts)
            .remove(session_id)
            .unwrap_or_default()
    }

    /// What the memory has done so far this turn, leaving it in place. Call
    /// with the session lock held.
    pub(crate) fn peek_receipt(&self, session_id: &str) -> Receipt {
        lock(&self.inner.receipts)
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    fn note(&self, session_id: &str, f: impl FnOnce(&mut Receipt)) {
        f(lock(&self.inner.receipts)
            .entry(session_id.to_string())
            .or_default());
    }

    fn load_for_turn(&self, session_id: &str) -> Result<Vec<Message>> {
        // One read transaction, so the history and the next seq describe the
        // same moment even if another process is appending.
        let (visible, next) = {
            let db = self.db();
            let tx = db.unchecked_transaction()?;
            (
                read_visible(&tx, session_id)?,
                next_seq_in(&tx, session_id)?,
            )
        };
        self.note(session_id, |r| {
            r.loaded_next = Some(next);
            r.loaded = visible.seqs;
            r.loaded_summary = visible.summarized;
        });
        Ok(visible.messages)
    }

    fn append_for_turn(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> std::result::Result<(), AppendError> {
        let expected = lock(&self.inner.receipts)
            .get(session_id)
            .and_then(|r| r.loaded_next);
        let result = self.append(session_id, expected, messages);
        self.note(session_id, |r| r.appended = Some(result.clone()));
        result.map(|_| ())
    }
}

/// Rig's [`ConversationMemory`] over the `messages` table.
///
/// The conversation id is the session id. `append` only ever inserts, and
/// refuses if another process appended to the session since this turn's
/// `load`. Both record what they did in the store's [`Receipt`] for the
/// session, because Rig does not report a failed append to its caller.
#[derive(Clone)]
pub struct SqliteMemory {
    store: Store,
}

impl SqliteMemory {
    /// The store this memory writes to, which holds the rest of the
    /// agent's state too (its sessions' sandboxes).
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }
}

impl ConversationMemory for SqliteMemory {
    fn load<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, std::result::Result<Vec<Message>, MemoryError>> {
        let id = conversation_id.to_string();
        Box::pin(async move {
            self.store
                .call(move |s| s.load_for_turn(&id))
                .await
                .map_err(MemoryError::backend)
        })
    }

    fn append<'a>(
        &'a self,
        conversation_id: &'a str,
        messages: Vec<Message>,
    ) -> WasmBoxedFuture<'a, std::result::Result<(), MemoryError>> {
        let id = conversation_id.to_string();
        Box::pin(async move {
            self.store
                .call(move |s| s.append_for_turn(&id, &messages))
                .await
                .map_err(MemoryError::backend)
        })
    }

    /// Refused: transcripts are append-only.
    fn clear<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, std::result::Result<(), MemoryError>> {
        Box::pin(async {
            Err(MemoryError::Policy(
                "athena transcripts are append-only; clear is not supported".into(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
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

    /// A store with one user and one session, and that session's id.
    fn with_session() -> (Store, User, String) {
        let store = store();
        let user = store.user("cli", "local").unwrap();
        let id = store.open_session(&user, "s").unwrap().id;
        (store, user, id)
    }

    fn user_version(db: &Connection) -> i64 {
        db.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn temp_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("athena-{}.db", uuid::Uuid::new_v4()))
    }

    #[test]
    fn open_creates_every_table_and_enforces_foreign_keys() {
        let path = temp_path();
        let store = Store::open(path.to_str().unwrap()).unwrap();
        let db = store.db();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let fk: bool = db
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        drop(db);
        drop(store);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            tables,
            [
                "browser_links",
                "browser_states",
                "compactions",
                "messages",
                "runs",
                "sandboxes",
                "selected_sessions",
                "sessions",
                "user_identities",
                "users"
            ]
        );
        assert!(fk);
    }

    #[test]
    fn athena_db_overrides_the_default_path() {
        assert_eq!(path_or_default(Some("/tmp/x.db".into())), "/tmp/x.db");
        assert_eq!(path_or_default(None), "agent.db");
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let (store, user, id) = with_session();
        store.save_run(&run_record("r1", &id)).unwrap();
        migrate(&store.db()).unwrap();
        assert_eq!(user_version(&store.db()), SCHEMA_VERSION as i64);
        assert_eq!(store.usage(&user).unwrap().len(), 1);
        // Still one cli/local user: the migration's insert did not run again.
        let users: i64 = store
            .db()
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(users, 1);
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
    fn a_runs_table_missing_columns_is_refused_and_left_as_found() {
        let db = Connection::open_in_memory().unwrap();
        // Enough columns for migration 2's index to build, so only the copy
        // in migration 3 stands between this table and acceptance.
        db.execute_batch("CREATE TABLE runs (session_id TEXT, started_at INTEGER)")
            .unwrap();
        let err = migrate(&db).unwrap_err().to_string();
        assert!(err.contains("no such column"), "{err}");
        // Rolled back: no version bump, no messages or users table.
        assert_eq!(user_version(&db), 0);
        assert!(columns(&db, "messages").unwrap().is_empty());
        assert!(columns(&db, "users").unwrap().is_empty());
    }

    #[test]
    fn a_messages_table_of_the_wrong_shape_is_refused() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE messages (session_id, seq, json, extra)")
            .unwrap();
        let err = migrate(&db).unwrap_err().to_string();
        assert!(err.contains("table `messages` has columns"), "{err}");
        assert_eq!(user_version(&db), 0);
        assert!(columns(&db, "runs").unwrap().is_empty());
    }

    /// The schema as main (74095ac) left it: migrations 1 and 2 only.
    fn at_version_2() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(MIGRATIONS[0]).unwrap();
        db.execute_batch(MIGRATIONS[1]).unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        db
    }

    #[test]
    fn a_null_run_id_is_given_a_legacy_id_instead_of_failing_the_upgrade() {
        let db = at_version_2();
        db.execute_batch(
            "INSERT INTO runs (run_id, session_id, started_at, ended_at, model, status,
                               first_seq, last_seq, calls_json)
             VALUES (NULL, 's', 1, 2, 'm', 'ok', 0, 1, '[]'),
                    ('kept', 's', 3, 4, 'm', 'ok', 2, 3, '[]');",
        )
        .unwrap();
        configure(&db).unwrap();

        migrate(&db).unwrap();

        let ids: Vec<(i64, String)> = db
            .prepare("SELECT rowid, run_id FROM runs ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(ids, [(1, "legacy-1".into()), (2, "kept".into())]);
        // And NULL is no longer accepted.
        let err = db
            .execute(
                "INSERT INTO runs (run_id, session_id, started_at, ended_at, model, status,
                                   first_seq, last_seq, calls_json)
                 VALUES (NULL, 's', 1, 2, 'm', 'ok', 0, 1, '[]')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("NOT NULL"), "{err}");
    }

    #[test]
    fn a_dangling_reference_is_refused_and_left_as_found() {
        let store = store();
        let db = store.db();
        db.pragma_update(None, "foreign_keys", false).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES ('orphan', 999, 'orphan', 0)",
            [],
        )
        .unwrap();

        let err = migrate(&db).unwrap_err().to_string();

        assert!(err.contains("1 rows reference"), "{err}");
        assert_eq!(user_version(&db), SCHEMA_VERSION as i64);
    }

    /// A fresh file with a reader holding it open in rollback-journal mode,
    /// which is what a racing process's first open looks like.
    fn locked_fresh_db() -> (std::path::PathBuf, Connection) {
        let path = temp_path();
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
    fn a_user_is_created_once_per_transport_and_external_id() {
        let store = store();
        let first = store.user("telegram", "42").unwrap();
        let again = store.user("telegram", "42").unwrap();
        let other_transport = store.user("http", "42").unwrap();

        assert_eq!(first, again);
        assert_ne!(first.id(), other_transport.id());
        assert_eq!((first.transport(), first.external_id()), ("telegram", "42"));
    }

    #[test]
    fn linking_shares_the_owner_without_creating_or_moving_users() {
        let store = store();
        let owner = store.user("telegram", "42").unwrap();
        let session = store.open_session(&owner, "notes").unwrap();
        store
            .append(&session.id, None, &[Message::user("remember")])
            .unwrap();
        store
            .save_browser_state(&session.id, b"saved state", 1)
            .unwrap();

        assert!(store.link_http_user(&owner, "my-api").unwrap());
        assert!(store.link_http_user(&owner, "my-api").unwrap());
        let http = store.user("http", "my-api").unwrap();
        assert_eq!(http.id(), owner.id());
        assert_eq!((http.transport(), http.external_id()), ("http", "my-api"));
        assert_eq!(
            store.session(&http, &session.id).unwrap(),
            Some(session.clone())
        );
        assert_eq!(store.session_owner(&session.id).unwrap(), Some(owner.id()));
        assert_eq!(
            store.load(&session.id).unwrap(),
            [Message::user("remember")]
        );
        assert_eq!(
            store.browser_state(&session.id).unwrap(),
            Some(b"saved state".to_vec())
        );
        assert_eq!(store.user("telegram", "42").unwrap(), owner);
        let users: i64 = store
            .db()
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(users, 2);
    }

    #[test]
    fn linking_never_claims_an_existing_other_owner_even_when_empty() {
        let store = store();
        let owner = store.user("telegram", "42").unwrap();
        let existing = store.user("http", "used").unwrap();
        assert!(!store.link_http_user(&owner, "used").unwrap());
        assert_eq!(store.user("http", "used").unwrap(), existing);
        let private = store.open_session(&existing, "private").unwrap();
        store
            .save_browser_state(&private.id, b"other state", 2)
            .unwrap();
        assert!(!store.link_http_user(&owner, "used").unwrap());
        assert_eq!(
            store.session_owner(&private.id).unwrap(),
            Some(existing.id())
        );
        assert_eq!(store.session(&owner, &private.id).unwrap(), None);
        assert_eq!(
            store.browser_state(&private.id).unwrap(),
            Some(b"other state".to_vec())
        );
    }

    #[test]
    fn a_failed_identity_insert_rolls_back_the_new_user() {
        let store = store();
        store
            .db()
            .execute_batch(
                "CREATE TRIGGER reject_identity BEFORE INSERT ON user_identities
             BEGIN SELECT RAISE(ABORT, 'fixture failure'); END;",
            )
            .unwrap();
        assert!(
            store
                .user("http", "new")
                .unwrap_err()
                .to_string()
                .contains("fixture failure")
        );
        assert!(
            store
                .link_http_user(&store.user("cli", "local").unwrap(), "new")
                .is_err()
        );
        let users: i64 = store
            .db()
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(users, 1);
    }

    #[test]
    fn session_names_are_unique_per_user_not_globally() {
        let store = store();
        let alice = store.user("cli", "alice").unwrap();
        let bob = store.user("cli", "bob").unwrap();

        let a = store.create_session(&alice, "notes").unwrap().unwrap();
        assert_eq!(store.create_session(&alice, "notes").unwrap(), None);
        let b = store.create_session(&bob, "notes").unwrap().unwrap();

        assert_ne!(a.id, b.id);
        assert_eq!(store.open_session(&alice, "notes").unwrap(), a);
        assert_eq!(store.open_session(&bob, "notes").unwrap(), b);
    }

    #[test]
    fn a_session_is_visible_only_to_its_owner() {
        let (store, owner, id) = with_session();
        let stranger = store.user("telegram", "7").unwrap();

        assert_eq!(store.session(&owner, &id).unwrap().unwrap().name, "s");
        assert_eq!(store.session(&stranger, &id).unwrap(), None);
        assert!(store.sessions(&stranger).unwrap().is_empty());
        assert_eq!(store.session(&owner, "no-such-id").unwrap(), None);
    }

    #[test]
    fn sessions_are_listed_by_name_with_counts_including_empty_ones() {
        let store = store();
        let user = store.user("cli", "local").unwrap();
        let b = store.open_session(&user, "b").unwrap();
        let a = store.open_session(&user, "a").unwrap();
        store
            .append(&b.id, None, &[Message::user("1"), Message::assistant("2")])
            .unwrap();

        let listed: Vec<(String, i64)> = store
            .sessions(&user)
            .unwrap()
            .into_iter()
            .map(|s| (s.session.id, s.messages))
            .collect();

        assert_eq!(listed, [(a.id, 0), (b.id, 2)]);
    }

    #[test]
    fn append_adds_after_the_last_row_and_never_rewrites_earlier_ones() {
        let (store, _, id) = with_session();
        assert_eq!(store.next_seq(&id).unwrap(), 0);

        let first = store
            .append(&id, Some(0), &[Message::user("a"), Message::assistant("b")])
            .unwrap();
        let before: String = store
            .db()
            .query_row("SELECT json FROM messages WHERE seq = 0", [], |r| r.get(0))
            .unwrap();
        let second = store.append(&id, Some(2), &[Message::user("c")]).unwrap();

        assert_eq!((first, second), ((0, 1), (2, 2)));
        assert_eq!(
            store.load(&id).unwrap(),
            [
                Message::user("a"),
                Message::assistant("b"),
                Message::user("c")
            ]
        );
        let after: String = store
            .db()
            .query_row("SELECT json FROM messages WHERE seq = 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn images_are_not_stored_but_the_rest_of_the_message_is() {
        use rig_core::message::UserContent;
        let (store, _, id) = with_session();
        let photo = media::image(b"\x89PNG\r\n\x1a\nrest").unwrap();
        let sent = Message::User {
            content: vec![UserContent::text("look"), UserContent::Image(photo)],
        };
        store.append(&id, Some(0), &[sent]).unwrap();
        assert_eq!(
            store.load(&id).unwrap(),
            [Message::User {
                content: vec![
                    UserContent::text("look"),
                    UserContent::text(media::NOT_KEPT)
                ],
            }]
        );
    }

    #[test]
    fn append_refuses_when_the_session_moved_since_it_was_loaded() {
        let (store, _, id) = with_session();
        store.append(&id, None, &[Message::user("a")]).unwrap();

        // This turn loaded an empty session; another process appended since.
        let err = store
            .append(&id, Some(0), &[Message::user("b")])
            .unwrap_err();

        assert_eq!(
            err,
            AppendError::Conflict {
                expected: 0,
                found: 1
            }
        );
        assert!(err.to_string().contains("expected the next seq to be 0"));
        assert_eq!(store.load(&id).unwrap(), [Message::user("a")]);
    }

    #[test]
    fn an_append_that_fails_midway_writes_nothing() {
        let (store, _, id) = with_session();
        store.append(&id, None, &[Message::user("a")]).unwrap();
        // Let the first insert through, then fail, as a full disk would.
        store
            .db()
            .execute_batch(
                "CREATE TRIGGER fail BEFORE INSERT ON messages WHEN NEW.seq = 2
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();

        let err = store
            .append(&id, Some(1), &[Message::user("x"), Message::user("y")])
            .unwrap_err();

        assert_eq!(err, AppendError::Storage("disk full".into()));
        assert!(err.to_string().contains("transcript not saved: disk full"));
        assert_eq!(store.load(&id).unwrap(), [Message::user("a")]);
    }

    #[test]
    fn messages_and_runs_cannot_name_a_session_that_does_not_exist() {
        let (store, _, id) = with_session();

        let err = store
            .append("no-such-session", None, &[Message::user("x")])
            .unwrap_err();
        assert_eq!(err, AppendError::Storage("no such session".into()));

        store.append(&id, None, &[Message::user("x")]).unwrap();
        let moved = store.db().execute(
            "UPDATE messages SET session_id = 'no-such-session' WHERE session_id = ?1",
            [&id],
        );
        assert!(moved.unwrap_err().to_string().contains("no such session"));

        let err = store
            .save_run(&run_record("r", "no-such-session"))
            .unwrap_err();
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    }

    #[test]
    fn a_session_must_belong_to_a_user_that_exists() {
        let store = store();
        let err = store
            .db()
            .execute("INSERT INTO sessions VALUES ('x', 999, 'x', 0)", [])
            .unwrap_err();
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    }

    #[test]
    fn listing_reports_a_missing_table_as_an_error() {
        let (store, user, id) = with_session();
        store
            .db()
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DROP TABLE runs; DROP TABLE messages; DROP TABLE selected_sessions;
                 DROP TABLE sessions; DROP TABLE user_identities; DROP TABLE users;",
            )
            .unwrap();
        assert!(store.sessions(&user).is_err());
        assert!(store.usage(&user).is_err());
        assert!(store.session(&user, &id).is_err());
        assert!(store.open_session(&user, "s").is_err());
        assert!(store.create_session(&user, "t").is_err());
        assert!(store.user("cli", "local").is_err());
        assert!(store.select_session(&user, &id).is_err());
        assert!(store.selected_session(&user).is_err());
        assert!(store.load(&id).is_err());
        assert!(store.next_seq(&id).is_err());
        assert!(matches!(
            store.append(&id, None, &[]),
            Err(AppendError::Storage(_))
        ));
    }

    #[test]
    fn a_user_can_select_only_their_own_session_and_reselect_later() {
        let store = store();
        let user = store.user("telegram", "1").unwrap();
        let stranger = store.user("telegram", "2").unwrap();
        let a = store.open_session(&user, "a").unwrap();
        let b = store.open_session(&user, "b").unwrap();
        let theirs = store.open_session(&stranger, "a").unwrap();

        assert_eq!(store.selected_session(&user).unwrap(), None);
        assert!(store.select_session(&user, &a.id).unwrap());
        assert_eq!(store.selected_session(&user).unwrap(), Some(a));
        assert!(store.select_session(&user, &b.id).unwrap());
        assert_eq!(store.selected_session(&user).unwrap(), Some(b.clone()));

        // Someone else's session and a missing one change nothing.
        assert!(!store.select_session(&user, &theirs.id).unwrap());
        assert!(!store.select_session(&user, "no-such-id").unwrap());
        assert_eq!(store.selected_session(&user).unwrap(), Some(b));
        assert_eq!(store.selected_session(&stranger).unwrap(), None);
        let rows: i64 = store
            .db()
            .query_row("SELECT COUNT(*) FROM selected_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_selection_pointing_at_another_users_session_is_never_read() {
        // Only a raw write can make one; the read must still not follow it.
        let store = store();
        let user = store.user("telegram", "1").unwrap();
        let stranger = store.user("telegram", "2").unwrap();
        let theirs = store.open_session(&stranger, "secret").unwrap();
        store
            .db()
            .execute(
                "INSERT INTO selected_sessions VALUES (?1, ?2, 0)",
                rusqlite::params![user.id(), theirs.id],
            )
            .unwrap();

        assert_eq!(store.selected_session(&user).unwrap(), None);
    }

    fn sandbox_for(session_id: &str, sandbox_id: &str) -> SandboxRow {
        SandboxRow {
            session_id: session_id.into(),
            sandbox_id: sandbox_id.into(),
            bash_session: None,
            code_language: None,
            code_context: None,
            created_at: 1,
            expires_at: 2,
        }
    }

    #[test]
    fn a_session_has_at_most_one_sandbox_and_stale_writers_change_nothing() {
        let (store, user, id) = with_session();
        assert_eq!(store.session_owner(&id).unwrap(), Some(user.id()));
        assert_eq!(store.session_owner("missing").unwrap(), None);
        assert_eq!(store.sandbox(&id).unwrap(), None);

        let first = sandbox_for(&id, "sbx-1");
        assert!(store.insert_sandbox(&first).unwrap());
        // A second process creating one for the same session loses.
        assert!(!store.insert_sandbox(&sandbox_for(&id, "sbx-2")).unwrap());
        assert_eq!(store.sandbox(&id).unwrap(), Some(first.clone()));

        let updated = SandboxRow {
            bash_session: Some("bash-1".into()),
            code_language: Some("python".into()),
            code_context: Some("ctx-1".into()),
            expires_at: 99,
            ..first.clone()
        };
        store.update_sandbox(&updated).unwrap();
        assert_eq!(store.sandbox(&id).unwrap(), Some(updated.clone()));

        // Writers holding another sandbox id touch nothing.
        store.update_sandbox(&sandbox_for(&id, "sbx-2")).unwrap();
        store.remove_sandbox(&id, "sbx-2").unwrap();
        assert_eq!(store.sandbox(&id).unwrap(), Some(updated));

        store.remove_sandbox(&id, "sbx-1").unwrap();
        assert_eq!(store.sandbox(&id).unwrap(), None);

        // A sandbox needs a real session.
        assert!(store.insert_sandbox(&sandbox_for("missing", "x")).is_err());
    }

    #[test]
    fn a_sign_in_link_opens_its_session_until_it_expires() {
        let (store, _, id) = with_session();
        store
            .insert_browser_link("tok", &id, "https://example.com/login", 100)
            .unwrap();
        let opened = Some((id.clone(), "https://example.com/login".to_string()));
        assert_eq!(store.browser_link("tok", 99).unwrap(), opened);
        assert_eq!(store.browser_link("tok", 100).unwrap(), None);
        assert_eq!(store.browser_link("other", 0).unwrap(), None);
        // A token is used once, and only for a real session.
        assert!(store.insert_browser_link("tok", &id, "u", 1).is_err());
        assert!(store.insert_browser_link("t2", "missing", "u", 1).is_err());
    }

    #[test]
    fn a_users_browser_state_is_shared_by_their_sessions_and_replaced_by_the_next_save() {
        let (store, user, first) = with_session();
        let second = store.open_session(&user, "other").unwrap().id;
        let stranger = store.user("cli", "someone-else").unwrap();
        let theirs = store.open_session(&stranger, "s").unwrap().id;
        assert_eq!(store.browser_state(&first).unwrap(), None);

        store.save_browser_state(&first, b"one", 1).unwrap();
        store.save_browser_state(&second, b"two", 2).unwrap();
        assert_eq!(store.browser_state(&first).unwrap(), Some(b"two".to_vec()));
        assert_eq!(store.browser_state(&second).unwrap(), Some(b"two".to_vec()));
        // Another user's sessions never see it.
        assert_eq!(store.browser_state(&theirs).unwrap(), None);
        let rows: Vec<(i64, i64)> = store
            .db()
            .prepare("SELECT user_id, saved_at FROM browser_states")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows, [(user.id(), 2)]);

        let err = store.save_browser_state("missing", b"x", 1).unwrap_err();
        assert!(err.to_string().contains("no session `missing`"), "{err}");
        assert_eq!(store.browser_state("missing").unwrap(), None);
    }

    #[test]
    fn usage_sums_token_columns_per_session_of_one_user() {
        let store = store();
        let user = store.user("cli", "local").unwrap();
        let stranger = store.user("telegram", "1").unwrap();
        let s = store.open_session(&user, "s").unwrap().id;
        let other = store.open_session(&user, "other").unwrap().id;
        let theirs = store.open_session(&stranger, "s").unwrap().id;
        store.save_run(&run_record("r1", &s)).unwrap();
        store.save_run(&run_record("r2", &s)).unwrap();
        store.save_run(&run_record("r3", &other)).unwrap();
        store.save_run(&run_record("r4", &theirs)).unwrap();

        let rows = store.usage(&user).unwrap();

        assert_eq!(rows.len(), 2);
        let row = &rows[1];
        assert_eq!(
            (row.name.as_str(), row.session_id.as_str()),
            ("s", s.as_str())
        );
        assert_eq!((row.runs, row.model_calls), (2, 4));
        assert_eq!(
            (row.input_tokens, row.output_tokens, row.cached_input_tokens),
            (200, 40, 128)
        );
        assert_eq!(store.usage(&stranger).unwrap().len(), 1);
    }

    #[test]
    fn run_ids_are_unique() {
        let (store, _, id) = with_session();
        store.save_run(&run_record("dup", &id)).unwrap();
        assert!(store.save_run(&run_record("dup", &id)).is_err());
    }

    #[tokio::test]
    async fn memory_load_and_append_record_a_receipt() {
        let (store, _, id) = with_session();
        store.append(&id, None, &[Message::user("old")]).unwrap();
        let memory = store.memory();
        store.begin_turn(&id);

        let history = memory.load(&id).await.unwrap();
        memory
            .append(&id, vec![Message::user("q"), Message::assistant("a")])
            .await
            .unwrap();

        assert_eq!(history, [Message::user("old")]);
        assert_eq!(
            store.take_receipt(&id),
            Receipt {
                loaded_next: Some(1),
                loaded: vec![0],
                loaded_summary: false,
                appended: Some(Ok((1, 2))),
            }
        );
        // Taken, not copied: the next turn starts clean.
        assert_eq!(store.take_receipt(&id), Receipt::default());
        assert_eq!(store.load(&id).unwrap().len(), 3);
    }

    fn checkpoint(through_seq: i64, summary: &str) -> Checkpoint {
        Checkpoint {
            through_seq,
            summary: summary.into(),
            model: "test/summarizer".into(),
            input_tokens: 50,
            output_tokens: 7,
            created_at: 3_000,
        }
    }

    /// `n` messages, `m0` to `m{n-1}`, saved as seqs 0 to n-1.
    fn with_messages(n: usize) -> (Store, String) {
        let (store, _, id) = with_session();
        let messages: Vec<Message> = (0..n).map(|i| Message::user(format!("m{i}"))).collect();
        store.append(&id, None, &messages).unwrap();
        (store, id)
    }

    #[tokio::test]
    async fn a_checkpointed_session_loads_its_summary_and_the_rows_after_it() {
        let (store, id) = with_messages(6);
        assert!(
            store
                .save_checkpoint(&id, &checkpoint(3, "so far"))
                .unwrap()
        );
        store.begin_turn(&id);

        let loaded = store.memory().load(&id).await.unwrap();

        assert_eq!(
            loaded,
            [
                compaction::summary_message("so far"),
                Message::user("m4"),
                Message::user("m5"),
            ]
        );
        // The summary has the seq of the last message it stands for, and the
        // conflict check still compares against the database's next seq.
        let receipt = store.peek_receipt(&id);
        assert_eq!(receipt.loaded, [3, 4, 5]);
        assert!(receipt.loaded_summary);
        assert_eq!(receipt.loaded_next, Some(6));
        // Peeking leaves it for the turn's end.
        assert_eq!(store.take_receipt(&id), receipt);
    }

    #[tokio::test]
    async fn a_compaction_hides_nothing_from_history_and_the_next_append() {
        let (store, id) = with_messages(4);
        store.save_checkpoint(&id, &checkpoint(2, "s")).unwrap();
        let memory = store.memory();
        store.begin_turn(&id);

        memory.load(&id).await.unwrap();
        memory
            .append(&id, vec![Message::user("new")])
            .await
            .unwrap();

        // Every original row is still there, and the new one follows them.
        let history = store.load(&id).unwrap();
        assert_eq!(history.len(), 5);
        assert_eq!(history[0], Message::user("m0"));
        assert_eq!(store.next_seq(&id).unwrap(), 5);
        assert_eq!(store.take_receipt(&id).appended, Some(Ok((4, 4))));
    }

    #[tokio::test]
    async fn the_newest_checkpoint_wins_and_an_unsummarized_session_has_none() {
        let (store, id) = with_messages(5);
        store.begin_turn(&id);
        store.memory().load(&id).await.unwrap();
        let before = store.take_receipt(&id);
        assert_eq!(before.loaded, [0, 1, 2, 3, 4]);
        assert!(!before.loaded_summary);

        assert!(store.save_checkpoint(&id, &checkpoint(1, "first")).unwrap());
        assert!(
            store
                .save_checkpoint(&id, &checkpoint(3, "second"))
                .unwrap()
        );
        store.begin_turn(&id);
        let loaded = store.memory().load(&id).await.unwrap();

        assert_eq!(
            loaded,
            [compaction::summary_message("second"), Message::user("m4")]
        );
        assert_eq!(
            newest_checkpoint(&store.db(), &id).unwrap(),
            Some(checkpoint(3, "second"))
        );
    }

    #[test]
    fn a_checkpoint_that_is_not_newer_changes_nothing() {
        let (store, id) = with_messages(5);
        assert!(store.save_checkpoint(&id, &checkpoint(3, "mine")).unwrap());

        // Another process got to the same range, or a later one, first.
        assert!(
            !store
                .save_checkpoint(&id, &checkpoint(3, "theirs"))
                .unwrap()
        );
        assert!(!store.save_checkpoint(&id, &checkpoint(2, "older")).unwrap());

        let db = store.db();
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM compactions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(
            newest_checkpoint(&db, &id).unwrap().unwrap().summary,
            "mine"
        );
    }

    #[test]
    fn a_checkpoint_must_point_at_a_message_the_session_has() {
        let (store, id) = with_messages(3);

        for through_seq in [3, 40, -1] {
            let err = store
                .save_checkpoint(&id, &checkpoint(through_seq, "x"))
                .unwrap_err();
            assert!(err.to_string().contains("does not fit"), "{err}");
        }
        let err = store
            .save_checkpoint("no-such-session", &checkpoint(0, "x"))
            .unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
        assert!(store.save_checkpoint(&id, &checkpoint(2, "x")).unwrap());
    }

    #[test]
    fn a_checkpoint_may_not_hide_a_tool_call_from_its_result() {
        let (store, _, id) = with_session();
        let call = Message::Assistant {
            id: None,
            content: vec![rig_core::message::AssistantContent::tool_call(
                "c1",
                "add",
                serde_json::json!({}),
            )],
        };
        let messages = [
            Message::user("add"),
            call,
            Message::tool_result("c1", "add", "3"),
            Message::assistant("3"),
        ];
        store.append(&id, None, &messages).unwrap();

        // Through the call, the first row the session loads would be its result.
        let err = store.save_checkpoint(&id, &checkpoint(1, "x")).unwrap_err();
        assert!(err.to_string().contains("without its call"), "{err}");
        // Before the call, or after the result, is fine.
        assert!(store.save_checkpoint(&id, &checkpoint(0, "x")).unwrap());
        assert!(store.save_checkpoint(&id, &checkpoint(2, "y")).unwrap());
    }

    #[test]
    fn a_checkpoint_before_a_row_that_is_not_a_message_is_refused() {
        let (store, id) = with_messages(3);
        store
            .db()
            .execute("INSERT INTO messages VALUES (?1, 3, 'not json')", [&id])
            .unwrap();

        assert!(store.save_checkpoint(&id, &checkpoint(2, "x")).is_err());
        // The rows before it are fine to summarize.
        assert!(store.save_checkpoint(&id, &checkpoint(1, "x")).unwrap());
    }

    #[test]
    fn a_checkpoint_cannot_name_a_session_that_does_not_exist() {
        let store = store();
        let err = store
            .db()
            .execute(
                "INSERT INTO compactions VALUES ('nope', 0, 's', 'm', 0, 0, 0)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    }

    fn run_with_calls(store: &Store, id: &str, run_id: &str, tweak: impl FnOnce(&mut RunRecord)) {
        let mut run = run_record(run_id, id);
        tweak(&mut run);
        store.save_run(&run).unwrap();
    }

    #[test]
    fn the_last_prompt_size_is_the_last_call_of_the_newest_run_that_saved_messages() {
        let (store, _, id) = with_session();
        assert_eq!(store.last_prompt_tokens(&id).unwrap(), None);

        let calls = |sizes: &[u64]| {
            let calls: Vec<_> = sizes
                .iter()
                .map(|n| serde_json::json!({"call_index": 0, "usage": {"input_tokens": n}}))
                .collect();
            serde_json::Value::Array(calls).to_string()
        };
        run_with_calls(&store, &id, "old", |r| r.calls_json = calls(&[10, 20]));
        assert_eq!(store.last_prompt_tokens(&id).unwrap(), Some(20));

        run_with_calls(&store, &id, "new", |r| {
            r.started_at = 5_000;
            r.calls_json = calls(&[100, 300, 700]);
        });
        // None of these says how big the session's context is: a run that
        // failed, one that saved nothing (a compaction's summary), one
        // that made no calls.
        run_with_calls(&store, &id, "failed", |r| {
            r.started_at = 6_000;
            r.status = "error".into();
            r.calls_json = calls(&[5]);
        });
        run_with_calls(&store, &id, "summary", |r| {
            r.started_at = 7_000;
            (r.first_seq, r.last_seq) = (4, 3);
            r.calls_json = calls(&[9]);
        });
        run_with_calls(&store, &id, "nothing", |r| {
            r.started_at = 8_000;
            r.model_calls = 0;
        });

        assert_eq!(store.last_prompt_tokens(&id).unwrap(), Some(700));
    }

    #[test]
    fn a_run_that_compacted_without_a_checkpoint_does_not_say_how_big_the_session_is() {
        let (store, _, id) = with_session();
        let calls = r#"[{"call_index": 0, "usage": {"input_tokens": 700}}]"#;
        let summary_calls = r#"[{"purpose": "compaction"}]"#;
        // A turn from 1 000 to 2 000 ms whose summary call began at 1 500: the
        // 700 tokens its last call reported were for the summary and what it
        // kept, not for the whole transcript.
        run_with_calls(&store, &id, "turn", |r| r.calls_json = calls.into());
        run_with_calls(&store, &id, "summary", |r| {
            (r.started_at, r.ended_at) = (1_500, 1_600);
            (r.first_seq, r.last_seq) = (0, -1);
            r.calls_json = summary_calls.into();
        });

        // Nothing was saved for the summary, so the next load is the whole
        // transcript and that number is no guide to it.
        assert_eq!(store.last_prompt_tokens(&id).unwrap(), None);

        // Once a checkpoint from that turn exists, the next load is the
        // summary and the rows after it, which the number does describe.
        store
            .append(&id, None, &[Message::user("a"), Message::user("b")])
            .unwrap();
        let mut saved = checkpoint(0, "s");
        saved.created_at = 1_500;
        store.save_checkpoint(&id, &saved).unwrap();
        assert_eq!(store.last_prompt_tokens(&id).unwrap(), Some(700));
    }

    #[test]
    fn a_failed_summary_or_one_from_another_turn_leaves_the_prompt_size_alone() {
        let (store, _, id) = with_session();
        let calls = r#"[{"call_index": 0, "usage": {"input_tokens": 700}}]"#;
        let summary_calls = r#"[{"purpose": "compaction"}]"#;
        run_with_calls(&store, &id, "turn", |r| {
            (r.started_at, r.ended_at) = (1_000, 2_000);
            r.calls_json = calls.into();
        });
        // A summary that failed changed nothing the turn sent.
        run_with_calls(&store, &id, "failed", |r| {
            (r.started_at, r.ended_at) = (1_500, 1_600);
            (r.first_seq, r.last_seq) = (0, -1);
            r.status = "error".into();
            r.calls_json = summary_calls.into();
        });
        // Nor did one from before the turn began or after it ended.
        for (name, at) in [("before", 500), ("after", 2_500)] {
            run_with_calls(&store, &id, name, |r| {
                (r.started_at, r.ended_at) = (at, at + 100);
                (r.first_seq, r.last_seq) = (0, -1);
                r.calls_json = summary_calls.into();
            });
        }

        assert_eq!(store.last_prompt_tokens(&id).unwrap(), Some(700));
    }

    #[test]
    fn a_run_without_usage_says_nothing_about_the_prompt_size() {
        let (store, _, id) = with_session();
        run_with_calls(&store, &id, "r", |r| {
            r.calls_json = r#"[{"call_index": 0}]"#.into();
        });
        assert_eq!(store.last_prompt_tokens(&id).unwrap(), None);

        run_with_calls(&store, &id, "later", |r| {
            r.started_at = 2_000;
            r.calls_json = "[]".into();
        });
        assert_eq!(store.last_prompt_tokens(&id).unwrap(), None);
    }

    #[test]
    fn the_prompt_size_of_a_session_with_no_runs_table_is_an_error() {
        let (store, _, id) = with_session();
        store.db().execute_batch("DROP TABLE runs").unwrap();
        assert!(store.last_prompt_tokens(&id).is_err());
    }

    #[tokio::test]
    async fn a_memory_append_after_another_writer_is_refused_and_recorded() {
        let (store, _, id) = with_session();
        let memory = store.memory();
        store.begin_turn(&id);
        memory.load(&id).await.unwrap();
        // Another process's turn lands between this turn's load and append.
        store.append(&id, None, &[Message::user("theirs")]).unwrap();

        let err = memory
            .append(&id, vec![Message::user("mine")])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("gained messages"), "{err}");
        assert!(matches!(
            store.take_receipt(&id).appended,
            Some(Err(AppendError::Conflict {
                expected: 0,
                found: 1
            }))
        ));
        assert_eq!(store.load(&id).unwrap(), [Message::user("theirs")]);
    }

    #[tokio::test]
    async fn a_memory_append_without_a_load_just_appends() {
        let (store, _, id) = with_session();
        store.append(&id, None, &[Message::user("a")]).unwrap();

        store
            .memory()
            .append(&id, vec![Message::user("b")])
            .await
            .unwrap();

        assert_eq!(store.load(&id).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_failed_memory_load_is_a_backend_error() {
        let (store, _, id) = with_session();
        store
            .db()
            .execute("INSERT INTO messages VALUES (?1, 0, 'not json')", [&id])
            .unwrap();

        let err = store.memory().load(&id).await.unwrap_err();

        assert!(matches!(err, MemoryError::Backend(_)), "{err}");
        assert_eq!(store.take_receipt(&id), Receipt::default());
    }

    #[tokio::test]
    async fn memory_clear_is_refused_and_deletes_nothing() {
        let (store, _, id) = with_session();
        store.append(&id, None, &[Message::user("a")]).unwrap();

        let err = store.memory().clear(&id).await.unwrap_err();

        assert!(err.to_string().contains("append-only"), "{err}");
        assert_eq!(store.load(&id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_panic_in_blocking_store_work_reaches_the_caller() {
        let store = store();
        let caught = tokio::spawn(async move { store.call(|_| panic!("boom")).await })
            .await
            .unwrap_err();
        assert!(caught.is_panic());
    }

    #[test]
    fn a_panic_while_holding_the_connection_does_not_brick_the_store() {
        let (store, _, id) = with_session();
        let clone = store.clone();
        let _ = std::thread::spawn(move || {
            let _held = clone.db();
            panic!("poison the connection mutex");
        })
        .join();

        assert_eq!(store.next_seq(&id).unwrap(), 0);
    }

    #[tokio::test]
    async fn turn_locks_are_dropped_once_nobody_holds_them() {
        let store = store();
        let guard = store.lock_session("a").await;
        assert_eq!(store.turns_on("a"), 1);
        drop(guard);
        // Still in the map until the next acquisition prunes it.
        assert_eq!(store.turns_on("a"), 0);

        let _b = store.lock_session("b").await;

        assert!(!lock(&store.inner.turns).contains_key("a"));
        assert_eq!(store.turns_on("b"), 1);
    }
}
