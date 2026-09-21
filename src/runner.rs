//! The agent loop. Load history, run a turn, save history and telemetry.

use crate::store::{self, RunRecord};
use anyhow::Result;
use rig_agent::agent::{Agent, PromptResponse};
use rig_agent::completion::PromptError;
use rig_agent::prelude::{Message, Prompt};
use rusqlite::Connection;
use std::io::{BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// One agent run, returning everything Rig reports rather than just the reply.
///
/// `Chat::chat` is Rig's convenience surface: it hands back the assistant text
/// and drops `usage`, `completion_calls` and `content` on the way out. This
/// template needs those for the `runs` table, so it drives the underlying
/// request directly. The trait exists so `turn` stays testable without a
/// provider behind it.
pub trait Run {
    fn run(
        &self,
        prompt: &str,
        history: Vec<Message>,
    ) -> impl std::future::Future<Output = Result<PromptResponse, PromptError>>;
}

impl Run for Agent {
    async fn run(
        &self,
        prompt: &str,
        history: Vec<Message>,
    ) -> Result<PromptResponse, PromptError> {
        self.prompt(prompt)
            .history(history)
            .extended_details()
            .await
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Whether to keep each completion call's raw provider response.
///
/// The raw payload carries provider detail Rig does not model (OpenRouter
/// reports upstream routing and cost there), but it is an unredacted copy of
/// the response and it dominates row size. On by default, off via
/// `RUNS_STORE_RAW=0` for deployments where either matters.
fn store_raw() -> bool {
    !matches!(
        std::env::var("RUNS_STORE_RAW").as_deref(),
        Ok("0") | Ok("false")
    )
}

/// Serialise the run's completion calls, dropping `raw` unless it is wanted.
fn calls_json(response: &PromptResponse, keep_raw: bool) -> String {
    let mut value = match serde_json::to_value(&response.completion_calls) {
        Ok(value) => value,
        // Telemetry must never fail a turn that already succeeded.
        Err(e) => return format!(r#"{{"serialize_error":"{e}"}}"#),
    };
    if !keep_raw && let Some(calls) = value.as_array_mut() {
        for call in calls {
            if let Some(obj) = call.as_object_mut() {
                obj.remove("raw");
            }
        }
    }
    value.to_string()
}

fn record(
    run_id: String,
    session: &str,
    model: &str,
    started_at: i64,
    first_seq: i64,
    outcome: &Result<PromptResponse, PromptError>,
) -> RunRecord {
    let mut rec = RunRecord {
        run_id,
        session_id: session.to_string(),
        started_at,
        ended_at: now_millis(),
        model: model.to_string(),
        status: "ok".into(),
        error: None,
        first_seq,
        // A run that appends nothing leaves last_seq below first_seq.
        last_seq: first_seq - 1,
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reasoning_tokens: 0,
        tool_use_prompt_tokens: 0,
        model_calls: 0,
        calls_json: "[]".into(),
    };

    match outcome {
        Ok(response) => {
            let u = &response.usage;
            rec.input_tokens = u.input_tokens as i64;
            rec.output_tokens = u.output_tokens as i64;
            rec.total_tokens = u.total_tokens as i64;
            rec.cached_input_tokens = u.cached_input_tokens as i64;
            rec.cache_creation_input_tokens = u.cache_creation_input_tokens as i64;
            rec.reasoning_tokens = u.reasoning_tokens as i64;
            rec.tool_use_prompt_tokens = u.tool_use_prompt_tokens as i64;
            rec.model_calls = response.completion_calls.len() as i64;
            rec.calls_json = calls_json(response, store_raw());
            let appended = response.messages.as_ref().map_or(0, Vec::len) as i64;
            rec.last_seq = first_seq + appended - 1;
        }
        Err(e) => {
            rec.status = "error".into();
            rec.error = Some(e.to_string());
        }
    }
    rec
}

/// One turn: load, run, save transcript, save telemetry.
///
/// The telemetry write is last and its failure is reported but not fatal —
/// losing a cost row must not cost the user their reply.
pub async fn turn<R: Run>(
    agent: &R,
    db: &Connection,
    session: &str,
    model: &str,
    prompt: &str,
) -> Result<String> {
    let started_at = now_millis();
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut history = store::load(db, session)?;
    let first_seq = history.len() as i64;

    let outcome = agent.run(prompt, history.clone()).await;
    let rec = record(run_id, session, model, started_at, first_seq, &outcome);

    let reply = match outcome {
        Ok(response) => {
            if let Some(messages) = response.messages {
                history.extend(messages);
            }
            store::save(db, session, &history)?;
            Ok(response.output)
        }
        Err(e) => Err(anyhow::Error::from(e)),
    };

    if let Err(e) = store::save_run(db, &rec) {
        eprintln!("warning: run telemetry not saved: {e}");
    }
    reply
}

pub async fn repl<R: Run>(agent: &R, db: &Connection, session: &str, model: &str) -> Result<()> {
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" {
            break;
        }
        println!("{}\n", turn(agent, db, session, model, input).await?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::agent::CompletionCall;
    use rig_core::completion::Usage;

    fn usage() -> Usage {
        Usage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cached_input_tokens: 64,
            cache_creation_input_tokens: 4,
            tool_use_prompt_tokens: 2,
            reasoning_tokens: 8,
        }
    }

    /// A two-model-call run appending four messages — the `add 21 and 21`
    /// shape: prompt, tool call, tool result, reply.
    fn tool_using_response() -> PromptResponse {
        let mut response = PromptResponse::new("42", usage());
        response.messages = Some(vec![
            Message::user("add 21 and 21"),
            Message::assistant("<tool call>"),
            Message::user("<tool result>"),
            Message::assistant("42"),
        ]);
        response.completion_calls = vec![
            CompletionCall::new(0, usage()).with_raw(serde_json::json!({"secret": "payload"})),
            CompletionCall::new(1, usage()).with_raw(serde_json::json!({"secret": "payload"})),
        ];
        response
    }

    fn max_turns_error() -> PromptError {
        PromptError::MaxTurnsError {
            max_turns: 20,
            chat_history: Box::new(vec![]),
            prompt: Box::new(Message::user("x")),
        }
    }

    #[test]
    fn record_copies_every_usage_field() {
        let rec = record(
            "run".into(),
            "s",
            "test/model",
            0,
            0,
            &Ok(tool_using_response()),
        );
        assert_eq!(rec.status, "ok");
        assert_eq!(rec.input_tokens, 100);
        assert_eq!(rec.output_tokens, 20);
        assert_eq!(rec.total_tokens, 120);
        assert_eq!(rec.cached_input_tokens, 64);
        assert_eq!(rec.cache_creation_input_tokens, 4);
        assert_eq!(rec.tool_use_prompt_tokens, 2);
        assert_eq!(rec.reasoning_tokens, 8);
        assert_eq!(rec.model, "test/model");
    }

    #[test]
    fn model_calls_are_counted_separately_from_messages() {
        let response = tool_using_response();
        let appended = response.messages.as_ref().unwrap().len();
        let rec = record("run".into(), "s", "m", 0, 0, &Ok(response));
        // Two HTTP requests produced four transcript rows. Conflating the two
        // is the mistake the runs table exists to prevent.
        assert_eq!(rec.model_calls, 2);
        assert_eq!(appended, 4);
        assert_eq!(rec.last_seq - rec.first_seq + 1, appended as i64);
    }

    #[test]
    fn seq_range_is_offset_by_existing_history() {
        let rec = record("run".into(), "s", "m", 0, 10, &Ok(tool_using_response()));
        assert_eq!((rec.first_seq, rec.last_seq), (10, 13));
    }

    #[test]
    fn a_failed_run_is_recorded_with_its_error_and_no_messages() {
        let rec = record("run".into(), "s", "m", 0, 7, &Err(max_turns_error()));
        assert_eq!(rec.status, "error");
        assert!(rec.error.unwrap().contains("max turns"));
        assert_eq!(rec.model_calls, 0);
        // Nothing was appended, so the range is empty rather than one row.
        assert!(rec.last_seq < rec.first_seq);
    }

    #[test]
    fn raw_payloads_are_kept_when_enabled_and_dropped_when_not() {
        let response = tool_using_response();

        let kept = calls_json(&response, true);
        assert!(kept.contains("secret"));

        let dropped = calls_json(&response, false);
        assert!(!dropped.contains("secret"));
        // Dropping raw must not drop the calls themselves or their usage.
        let parsed: serde_json::Value = serde_json::from_str(&dropped).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["usage"]["input_tokens"], 100);
    }

    struct FakeAgent(std::cell::RefCell<Option<Result<PromptResponse, PromptError>>>);

    impl Run for FakeAgent {
        async fn run(
            &self,
            _prompt: &str,
            _history: Vec<Message>,
        ) -> Result<PromptResponse, PromptError> {
            self.0.borrow_mut().take().expect("run called twice")
        }
    }

    fn test_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE messages (session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                 json TEXT NOT NULL, PRIMARY KEY (session_id, seq));
             CREATE TABLE runs (run_id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
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

    #[tokio::test]
    async fn turn_persists_transcript_and_telemetry_together() {
        let db = test_db();
        let agent = FakeAgent(std::cell::RefCell::new(Some(Ok(tool_using_response()))));

        let reply = turn(&agent, &db, "s", "test/model", "add 21 and 21")
            .await
            .unwrap();

        assert_eq!(reply, "42");
        assert_eq!(store::load(&db, "s").unwrap().len(), 4);
        let rows = store::usage(&db).unwrap();
        assert_eq!(rows.len(), 1);
        let u = &rows[0];
        assert_eq!((u.session_id.as_str(), u.runs, u.model_calls), ("s", 1, 2));
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cached_input_tokens),
            (100, 20, 64)
        );
    }

    #[tokio::test]
    async fn a_failed_turn_leaves_the_transcript_untouched_but_is_still_recorded() {
        let db = test_db();
        store::save(&db, "s", &[Message::user("earlier")]).unwrap();
        let agent = FakeAgent(std::cell::RefCell::new(Some(Err(max_turns_error()))));

        assert!(turn(&agent, &db, "s", "m", "boom").await.is_err());

        // The conversation is exactly as it was.
        assert_eq!(
            store::load(&db, "s").unwrap(),
            vec![Message::user("earlier")]
        );
        // But the failure is visible in telemetry.
        let mut q = db.prepare("SELECT status, error FROM runs").unwrap();
        let rows: Vec<(String, Option<String>)> = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "error");
        assert!(rows[0].1.as_ref().unwrap().contains("max turns"));
    }
}
