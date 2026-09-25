//! Shared helpers for the integration tests. Each test binary uses a subset.
#![allow(dead_code)]

use athena::agent;
use athena::service::{Service, Session, User};
use athena::store::Store;
use rig_agent::agent::{Agent, AgentBuilder};
use rig_core::completion::Usage;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A database file in the system temp dir, removed with its WAL files on drop.
///
/// A real file rather than `:memory:` so tests can close and reopen it, the
/// way separate CLI processes do.
pub struct TempDb(PathBuf);

impl TempDb {
    pub fn new() -> Self {
        Self(std::env::temp_dir().join(format!("athena-test-{}.db", uuid::Uuid::new_v4())))
    }

    pub fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }

    /// Open through `Store::open`, migrations and all.
    pub fn open(&self) -> Store {
        Store::open(self.path()).unwrap()
    }

    /// Open without migrating, to set up or inspect a database as it is.
    pub fn raw(&self) -> Connection {
        Connection::open(self.path()).unwrap()
    }

    /// A service over a fresh `Store` on this file, as a new process would
    /// have. Its warnings are collected rather than printed.
    pub fn service(&self) -> (Service, Warnings) {
        let warnings = Warnings::default();
        let sink = warnings.clone();
        let service = Service::new(self.open(), "m", move |w| {
            sink.lock().unwrap().push(w.to_string())
        });
        (service, warnings)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path()));
        }
    }
}

/// An empty directory to run the binary in, removed on drop.
///
/// The binary loads `.env` from its working directory, so it must never run
/// in the repository, where a developer's real `.env` holds real keys.
pub struct WorkDir(PathBuf);

impl WorkDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("athena-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    /// Write `contents` as this directory's `.env`.
    pub fn env_file(&self, contents: &str) {
        std::fs::write(self.0.join(".env"), contents).unwrap();
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub type Warnings = Arc<Mutex<Vec<String>>>;

pub fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        ..Default::default()
    }
}

/// The production agent, preamble and tools included, with the service's
/// memory, in front of a scripted model. The returned model shares state
/// with the agent's copy, so `requests()` shows what the agent actually sent.
pub fn mock_agent(
    service: &Service,
    turns: impl IntoIterator<Item = MockTurn>,
) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::new(turns);
    let builder = AgentBuilder::new(model.clone()).memory(service.memory());
    (agent::configure(builder), model)
}

/// `add 21 and 21` as a real model plays it: a tool call, then the answer.
/// Token counts are the ones OpenRouter reported for that exchange.
pub fn add_turns() -> Vec<MockTurn> {
    vec![
        MockTurn::tool_call("call_1", "add", serde_json::json!({"a": 21, "b": 21}))
            .with_usage(usage(111, 21)),
        MockTurn::text("42").with_usage(usage(145, 5)),
    ]
}

/// The CLI's own user, which owns every session from before users existed.
pub async fn cli_user(service: &Service) -> User {
    service.user("cli", "local").await.unwrap()
}

/// `user`'s session `name`, created if new.
pub async fn session(service: &Service, user: &User, name: &str) -> Session {
    service.open_session(user, name).await.unwrap()
}

/// The id of the session `name` owned by `transport:external_id`.
pub fn session_id(db: &Connection, transport: &str, external_id: &str, name: &str) -> String {
    db.query_row(
        "SELECT s.id FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE u.transport = ?1 AND u.external_id = ?2 AND s.name = ?3",
        [transport, external_id, name],
        |r| r.get(0),
    )
    .unwrap()
}

/// A session's transcript exactly as stored, one JSON string per row.
pub fn raw_rows(db: &Connection, session_id: &str) -> Vec<String> {
    let mut q = db
        .prepare("SELECT json FROM messages WHERE session_id=?1 ORDER BY seq")
        .unwrap();
    q.query_map([session_id], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub struct RunRow {
    pub first_seq: i64,
    pub last_seq: i64,
    pub model_calls: i64,
    pub status: String,
}

/// A session's `runs` rows, oldest first.
pub fn runs(db: &Connection, session_id: &str) -> Vec<RunRow> {
    let mut q = db
        .prepare(
            "SELECT first_seq, last_seq, model_calls, status FROM runs
             WHERE session_id=?1 ORDER BY started_at, rowid",
        )
        .unwrap();
    q.query_map([session_id], |r| {
        Ok(RunRow {
            first_seq: r.get(0)?,
            last_seq: r.get(1)?,
            model_calls: r.get(2)?,
            status: r.get(3)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

pub fn run_row(first_seq: i64, last_seq: i64, model_calls: i64, status: &str) -> RunRow {
    RunRow {
        first_seq,
        last_seq,
        model_calls,
        status: status.into(),
    }
}

pub fn user_version(db: &Connection) -> i64 {
    db.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

pub fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// The production agent in front of a scripted streaming model, with any
/// conversation memory (the service's, or a wrapper around it). A mock
/// model scripts blocking or streaming turns, not both.
pub fn mock_stream_agent(
    memory: impl rig_core::memory::ConversationMemory + 'static,
    turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_stream_turns(turns);
    let builder = AgentBuilder::new(model.clone()).memory(memory);
    (agent::configure(builder), model)
}

/// One streamed model turn: `chunks` of text, then the provider's terminal
/// record. Without that record Rig treats the turn as truncated.
pub fn streamed_text(chunks: &[&str], usage: Usage) -> Vec<MockStreamEvent> {
    let mut events: Vec<MockStreamEvent> =
        chunks.iter().map(|c| MockStreamEvent::text(*c)).collect();
    events.push(MockStreamEvent::final_response(usage));
    events
}

/// `add 21 and 21`, streamed: a tool call, then "42" in two deltas.
pub fn streamed_add_turns() -> Vec<Vec<MockStreamEvent>> {
    vec![
        vec![
            MockStreamEvent::tool_call("call_1", "add", serde_json::json!({"a": 21, "b": 21})),
            MockStreamEvent::final_response(usage(111, 21)),
        ],
        streamed_text(&["4", "2"], usage(145, 5)),
    ]
}

/// [`mock_agent`] with any conversation memory, such as a wrapper that
/// parks a turn at a chosen point.
pub fn mock_agent_with_memory(
    memory: impl rig_core::memory::ConversationMemory + 'static,
    turns: impl IntoIterator<Item = MockTurn>,
) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::new(turns);
    let builder = AgentBuilder::new(model.clone()).memory(memory);
    (agent::configure(builder), model)
}

/// Interrupts a test sends by hand, standing in for SIGINT or
/// SIGTERM. Each call of the returned function waits for the next one.
pub fn interrupts() -> (
    tokio::sync::mpsc::UnboundedSender<()>,
    impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
) {
    let (send, receive) = tokio::sync::mpsc::unbounded_channel::<()>();
    let receive = Arc::new(tokio::sync::Mutex::new(receive));
    let next = move || {
        let receive = receive.clone();
        Box::pin(async move {
            receive.lock().await.recv().await;
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    };
    (send, next)
}
