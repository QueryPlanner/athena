//! Cassettes: recorded model responses, and the two models that use them.
//!
//! [`RecordingModel`] wraps a real model and notes every call: what the agent
//! sent last (the prompt, or the tool results it is answering) and what the
//! model replied. [`ReplayModel`] serves those replies back, in order, to the
//! same agent, and checks each request against the recording first. The
//! agent runs its real tools in between, so a replayed case exercises the
//! production preamble, tool registry, agent loop, memory and database.
//!
//! A request that no longer matches its recording is **trajectory drift**:
//! the agent's code changed in a way the recorded model never saw (a tool
//! returns something else, a tool was removed, a turn was added). Replay
//! stops at the first mismatch and the case fails with a message that starts
//! with `trajectory drift`. The fix is to re-record the case.
//!
//! What is compared is deliberately narrow, so editing the preamble or adding
//! an unrelated tool does not invalidate every cassette: the last message's
//! text and tool results ([`input`]), and that every tool a recorded reply
//! calls is still offered. Prompt quality is graded by live runs, not replay.

use anyhow::{Context, Result};
use rig_core::completion::{
    AssistantContent, CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
};
use rig_core::message::{Message, ToolResultContent, UserContent};
use rig_core::streaming::StreamingCompletionResponse;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The model calls one case made, in order, across all its turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cassette {
    pub eval_case_id: String,
    /// The model the recording was made with, reported as `model` in results.
    pub model: String,
    pub interactions: Vec<Interaction>,
}

/// One model call: the request's [`input`] and the model's response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interaction {
    pub input: Vec<String>,
    pub response: CompletionResponse,
}

impl Cassette {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading cassette {}; record it with `athena eval record`",
                path.display()
            )
        })?;
        serde_json::from_str(&text).with_context(|| format!("parsing cassette {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(Path::new(""));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        // Cannot fail: plain structs, strings and JSON values.
        let json = serde_json::to_string_pretty(self).expect("a cassette serialises");
        std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))
    }
}

/// What a request is answering, one line per part of its last message:
/// `user: <text>` or `tool <name>: <result>`. Tool call ids are left out:
/// providers mint new ones on every recording.
pub fn input(request: &CompletionRequest) -> Vec<String> {
    match request.chat_history.last() {
        Some(Message::User { content }) => content.iter().map(user_part).collect(),
        Some(other) => vec![json(other)],
        None => Vec::new(),
    }
}

fn user_part(part: &UserContent) -> String {
    match part {
        UserContent::Text(text) => format!("user: {}", text.text),
        UserContent::ToolResult(result) => {
            let content: Vec<String> = result.content.iter().map(tool_result_part).collect();
            format!("tool {}: {}", result.name, content.join("\n"))
        }
        other => json(other),
    }
}

fn tool_result_part(part: &ToolResultContent) -> String {
    match part {
        ToolResultContent::Text(text) => text.text.clone(),
        ToolResultContent::Json { value } => value.to_string(),
        other => json(other),
    }
}

fn json(value: &impl Serialize) -> String {
    // Cannot fail: rig's message types are plain data with string keys.
    serde_json::to_string(value).expect("a rig message serialises")
}

/// Serves a [`Cassette`]; see the module docs.
#[derive(Clone)]
pub struct ReplayModel {
    state: Arc<Mutex<ReplayState>>,
}

struct ReplayState {
    remaining: VecDeque<Interaction>,
    served: usize,
    drift: Option<String>,
}

impl ReplayModel {
    pub fn new(cassette: &Cassette) -> Self {
        Self {
            state: Arc::new(Mutex::new(ReplayState {
                remaining: cassette.interactions.iter().cloned().collect(),
                served: 0,
                drift: None,
            })),
        }
    }

    /// Why the replay diverged from the recording, once the case is over:
    /// the first mismatched request, or recorded calls that were never made.
    pub fn drift(&self) -> Option<String> {
        let state = self.state.lock().unwrap();
        match (&state.drift, state.remaining.len()) {
            (Some(why), _) => Some(why.clone()),
            (None, 0) => None,
            (None, n) => Some(format!(
                "trajectory drift: the agent stopped after {} model calls; {n} more were recorded",
                state.served
            )),
        }
    }
}

impl ReplayState {
    fn serve(&mut self, request: &CompletionRequest) -> Result<CompletionResponse, String> {
        let call = self.served;
        self.served += 1;
        let actual = input(request);
        let Some(next) = self.remaining.pop_front() else {
            return Err(format!(
                "trajectory drift: model call {call} was not recorded (input {actual:?})"
            ));
        };
        if next.input != actual {
            return Err(format!(
                "trajectory drift at model call {call}: recorded input {:?}, got {actual:?}",
                next.input
            ));
        }
        for content in &next.response.choice {
            if let AssistantContent::ToolCall(tc) = content {
                let name = &tc.function.name;
                if !request.tools.iter().any(|t| &t.name == name) {
                    return Err(format!(
                        "trajectory drift at model call {call}: the recording calls `{name}`, \
                         which the agent no longer offers"
                    ));
                }
            }
        }
        Ok(next.response)
    }
}

impl CompletionModel for ReplayModel {
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        let mut state = self.state.lock().unwrap();
        state.serve(&request).map_err(|why| {
            state.drift.get_or_insert(why.clone());
            CompletionError::ProviderError(why)
        })
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        Err(not_streamed())
    }
}

fn not_streamed() -> CompletionError {
    CompletionError::ProviderError("eval models answer blocking requests only".into())
}

/// Passes every call to `inner` and keeps a copy for a [`Cassette`].
#[derive(Clone)]
pub struct RecordingModel<M> {
    inner: M,
    log: Arc<Mutex<Vec<Interaction>>>,
}

impl<M> RecordingModel<M> {
    pub fn new(inner: M) -> Self {
        Self {
            inner,
            log: Arc::default(),
        }
    }

    /// Everything recorded so far, oldest first.
    pub fn interactions(&self) -> Vec<Interaction> {
        self.log.lock().unwrap().clone()
    }
}

impl<M: CompletionModel> CompletionModel for RecordingModel<M> {
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        let input = input(&request);
        let response = self.inner.completion(request).await?;
        // The provider's raw payload is large and replay never reads it.
        let mut kept = response.clone();
        kept.raw = serde_json::Value::Null;
        self.log.lock().unwrap().push(Interaction {
            input,
            response: kept,
        });
        Ok(response)
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        Err(not_streamed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::{ToolDefinition, Usage};
    use rig_core::message::{Text, ToolResult};
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    fn request(history: Vec<Message>, tools: &[&str]) -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: None,
            chat_history: history,
            documents: Vec::new(),
            tools: tools
                .iter()
                .map(|t| ToolDefinition {
                    name: t.to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
                .collect(),
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        }
    }

    fn text(t: &str) -> CompletionResponse {
        CompletionResponse::new(vec![AssistantContent::text(t)], Usage::new(), "test")
    }

    fn cassette(interactions: Vec<Interaction>) -> Cassette {
        Cassette {
            eval_case_id: "c".into(),
            model: "m".into(),
            interactions,
        }
    }

    #[test]
    fn input_names_prompts_and_tool_results_but_not_call_ids() {
        assert!(input(&request(vec![], &[])).is_empty());
        assert_eq!(
            input(&request(vec![Message::user("hi")], &[])),
            ["user: hi"]
        );

        let result = |content| {
            UserContent::ToolResult(ToolResult {
                call: rig_core::message::ToolCallId::new("call_9").unwrap(),
                provider: None,
                name: "add".into(),
                content,
            })
        };
        let image = || rig_core::message::Image {
            data: rig_core::message::DocumentSourceKind::Url("u".into()),
            media_type: None,
            detail: None,
            additional_params: None,
        };
        let history = vec![Message::User {
            content: vec![
                result(vec![ToolResultContent::Text(Text::new("42"))]),
                result(vec![
                    ToolResultContent::Json {
                        value: serde_json::json!({"n": 1}),
                    },
                    ToolResultContent::Image(image()),
                ]),
                UserContent::Image(image()),
            ],
        }];
        let parts = input(&request(history, &[]));
        assert_eq!(parts[0], "tool add: 42");
        let json_then_image = "tool add: {\"n\":1}\n{";
        assert!(parts[1].starts_with(json_then_image), "{}", parts[1]);
        assert!(parts[1].contains("\"u\""), "{}", parts[1]);
        assert!(!parts.join("").contains("call_9"));
        assert!(parts[2].contains("\"u\""), "{}", parts[2]);

        let assistant = input(&request(vec![Message::assistant("x")], &[]));
        assert!(assistant[0].contains("\"x\""), "{assistant:?}");
    }

    #[tokio::test]
    async fn replay_serves_the_recording_in_order_and_reports_no_drift() {
        let model = ReplayModel::new(&cassette(vec![
            Interaction {
                input: vec!["user: a".into()],
                response: text("one"),
            },
            Interaction {
                input: vec!["user: b".into()],
                response: text("two"),
            },
        ]));
        let first = model
            .completion(request(vec![Message::user("a")], &[]))
            .await;
        assert_eq!(first.unwrap().choice, text("one").choice);
        assert!(
            model
                .drift()
                .unwrap()
                .contains("stopped after 1 model calls; 1 more")
        );
        let second = model
            .completion(request(vec![Message::user("b")], &[]))
            .await;
        assert_eq!(second.unwrap().choice, text("two").choice);
        assert_eq!(model.drift(), None);
    }

    #[tokio::test]
    async fn replay_fails_loudly_on_a_different_request_an_extra_call_or_a_missing_tool() {
        let model = ReplayModel::new(&cassette(vec![Interaction {
            input: vec!["user: a".into()],
            response: text("one"),
        }]));
        let err = model
            .completion(request(vec![Message::user("changed")], &[]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("trajectory drift at model call 0"), "{err}");
        assert!(err.contains("changed"), "{err}");
        // The first divergence is the one reported.
        let extra = model
            .completion(request(vec![Message::user("a")], &[]))
            .await;
        assert!(extra.unwrap_err().to_string().contains("was not recorded"));
        assert!(model.drift().unwrap().contains("recorded input"));

        let call = CompletionResponse::new(
            vec![AssistantContent::tool_call(
                "id",
                "gone",
                serde_json::json!({}),
            )],
            Usage::new(),
            "test",
        );
        let model = ReplayModel::new(&cassette(vec![Interaction {
            input: vec!["user: a".into()],
            response: call,
        }]));
        let err = model
            .completion(request(vec![Message::user("a")], &["add"]))
            .await
            .unwrap_err()
            .to_string();
        let why = "`gone`, which the agent no longer offers";
        assert!(err.contains(why), "{err}");

        let err = model.stream(request(vec![], &[])).await.err().unwrap();
        assert!(err.to_string().contains("blocking requests only"));
    }

    #[tokio::test]
    async fn recording_passes_calls_through_and_keeps_them_without_raw() {
        let inner = MockCompletionModel::new([
            MockTurn::text("hello").with_raw(serde_json::json!({"big": "payload"})),
            MockTurn::error("provider down"),
        ]);
        let model = RecordingModel::new(inner);
        let reply = model
            .completion(request(vec![Message::user("hi")], &[]))
            .await
            .unwrap();
        assert_eq!(reply.raw, serde_json::json!({"big": "payload"}));
        let failed = model
            .completion(request(vec![Message::user("again")], &[]))
            .await;
        assert!(failed.is_err());
        let log = model.interactions();
        assert_eq!(log.len(), 1, "a failed call is not recorded");
        assert_eq!(log[0].input, ["user: hi"]);
        assert!(log[0].response.raw.is_null());
        assert!(model.stream(request(vec![], &[])).await.is_err());
    }

    #[test]
    fn a_cassette_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("athena-cassette-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested/c.json");
        let original = cassette(vec![Interaction {
            input: vec!["user: a".into()],
            response: text("one"),
        }]);
        original.write(&path).unwrap();
        let read = Cassette::read(&path).unwrap();
        assert_eq!(read.interactions[0].response.choice, text("one").choice);

        std::fs::write(&path, "{").unwrap();
        let err = format!("{:#}", Cassette::read(&path).unwrap_err());
        assert!(err.contains("parsing cassette"), "{err}");
        let err = format!("{:#}", Cassette::read(&dir.join("none.json")).unwrap_err());
        assert!(err.contains("athena eval record"), "{err}");

        // A file where a directory must go.
        let err = original.write(&path.join("x.json")).unwrap_err();
        assert!(format!("{err:#}").contains("creating"));
        let err = original.write(&dir).unwrap_err();
        assert!(format!("{err:#}").contains("writing"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
