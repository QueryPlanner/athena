//! Shared helpers for the integration tests. Each test binary uses a subset.
#![allow(dead_code)]

use athena::{agent, store};
use rig_agent::agent::{Agent, AgentBuilder};
use rig_core::completion::Usage;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rusqlite::Connection;
use std::path::PathBuf;

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

    /// Open through `store::open`, migrations and all.
    pub fn open(&self) -> Connection {
        store::open(self.path()).unwrap()
    }

    /// Open without migrating, to set up or inspect a database as it is.
    pub fn raw(&self) -> Connection {
        Connection::open(self.path()).unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path()));
        }
    }
}

pub fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        ..Default::default()
    }
}

/// The production agent, preamble and tools included, in front of a
/// scripted model. The returned model shares state with the agent's copy, so
/// `requests()` shows what the agent actually sent.
pub fn mock_agent(turns: impl IntoIterator<Item = MockTurn>) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::new(turns);
    (agent::configure(AgentBuilder::new(model.clone())), model)
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

/// A session's transcript exactly as stored, one JSON string per row.
pub fn raw_rows(db: &Connection, session: &str) -> Vec<String> {
    let mut q = db
        .prepare("SELECT json FROM messages WHERE session_id=?1 ORDER BY seq")
        .unwrap();
    q.query_map([session], |r| r.get(0))
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
pub fn runs(db: &Connection, session: &str) -> Vec<RunRow> {
    let mut q = db
        .prepare(
            "SELECT first_seq, last_seq, model_calls, status FROM runs
             WHERE session_id=?1 ORDER BY started_at, rowid",
        )
        .unwrap();
    q.query_map([session], |r| {
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
