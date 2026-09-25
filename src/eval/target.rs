//! Where a case runs, and what came back: an [`Observation`].
//!
//! Every target rebuilds the tool trajectory the same way, from the session's
//! stored transcript ([`tool_calls`]), so a replayed case and a live one are
//! graded on the same evidence production would leave behind.

use super::case::EvalCase;
use super::cassette::{Cassette, RecordingModel, ReplayModel};
use crate::agent;
use crate::service::Service;
use crate::store::Store;
use anyhow::{Context, Result};
use reqwest::header::HeaderMap;
use rig_agent::agent::AgentBuilder;
use rig_core::completion::{AssistantContent, CompletionModel};
use rig_core::message::Message;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// A tool call the model made, as stored in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

/// What one sample of a case did. Fields a target cannot report stay `None`.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// One reply per turn that completed.
    pub replies: Vec<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Why the sample stopped early, if it did.
    pub error: Option<String>,
    /// Replay only: how the agent diverged from the cassette.
    pub drift: Option<String>,
    pub model_calls: Option<i64>,
    pub total_tokens: Option<i64>,
    /// Each model call's finish reason, where the target reports them.
    pub finish_reasons: Vec<String>,
    pub latency_ms: u64,
    /// The last turn's run.
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub trace_id: Option<String>,
    pub model: Option<String>,
}

impl Observation {
    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }
}

/// Every tool call in a transcript, oldest first.
pub fn tool_calls(history: &[Message]) -> Vec<ToolCall> {
    history
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|c| match c {
            AssistantContent::ToolCall(tc) => Some(ToolCall {
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The finish reason of each call in a run's `calls_json`.
fn finish_reasons(calls_json: &str) -> Vec<String> {
    let calls: Vec<Value> = serde_json::from_str(calls_json).unwrap_or_default();
    calls
        .iter()
        .filter_map(|c| c.get("finish_reason"))
        .map(|r| r.as_str().map_or_else(|| r.to_string(), str::to_string))
        .collect()
}

/// Run `case` through the production agent in front of `model`, with a fresh
/// in-memory database, as the user `eval:<user>`.
async fn in_process(
    case: &EvalCase,
    model: impl CompletionModel + 'static,
    model_name: &str,
    user: &str,
) -> Result<Observation> {
    let service = Service::new(Store::open_in_memory()?, model_name, crate::cli::warn);
    let agent = agent::configure(AgentBuilder::new(model).memory(service.memory()));
    let user = service.user("eval", user).await?;
    let session = service.create_session(&user, &case.eval_case_id).await?;
    let mut obs = Observation {
        session_id: Some(session.id.clone()),
        model: Some(model_name.to_string()),
        model_calls: Some(0),
        total_tokens: Some(0),
        ..Observation::default()
    };
    let started = Instant::now();
    for turn in &case.turns {
        match service.send(&agent, &user, &session.id, turn).await {
            Ok(done) => {
                obs.replies.push(done.reply);
                *obs.model_calls.get_or_insert(0) += done.run.model_calls;
                *obs.total_tokens.get_or_insert(0) += done.run.total_tokens;
                obs.finish_reasons
                    .extend(finish_reasons(&done.run.calls_json));
                obs.run_id = Some(done.run.run_id);
            }
            Err(e) => {
                obs.error = Some(e.to_string());
                break;
            }
        }
    }
    obs.latency_ms = started.elapsed().as_millis() as u64;
    obs.tool_calls = tool_calls(&service.history(&user, &session.id).await?);
    Ok(obs)
}

/// Replay `case` from its cassette. A missing or foreign cassette fails the
/// sample, not the run.
pub async fn replay(case: &EvalCase, user: &str) -> Result<Observation> {
    let cassette = match Cassette::read(&case.cassette_path()) {
        Ok(c) if c.eval_case_id == case.eval_case_id => c,
        Ok(c) => {
            return Ok(Observation::failed(format!(
                "trajectory drift: {} is the cassette of `{}`",
                case.cassette_path().display(),
                c.eval_case_id
            )));
        }
        Err(e) => return Ok(Observation::failed(format!("{e:#}"))),
    };
    let model = ReplayModel::new(&cassette);
    let mut obs = in_process(case, model.clone(), &cassette.model, user).await?;
    obs.drift = model.drift();
    Ok(obs)
}

/// Run `case` against a real model, recording what it said.
pub async fn record<M: CompletionModel + Clone + 'static>(
    case: &EvalCase,
    model: M,
    model_name: &str,
    user: &str,
) -> Result<(Cassette, Observation)> {
    let recorder = RecordingModel::new(model);
    let obs = in_process(case, recorder.clone(), model_name, user).await?;
    let cassette = Cassette {
        eval_case_id: case.eval_case_id.clone(),
        model: model_name.to_string(),
        interactions: recorder.interactions(),
    };
    Ok((cassette, obs))
}

/// A deployed Athena, driven through its HTTP API.
pub struct Http {
    client: reqwest::Client,
    base: String,
    user: String,
}

/// How long one request may take: a turn can run many tools.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

impl Http {
    /// `base` is the server's root URL, such as `http://127.0.0.1:18081`.
    pub fn new(base: &str, user: &str) -> Result<Self> {
        Ok(Self {
            client: client(REQUEST_TIMEOUT)?,
            base: base.trim_end_matches('/').to_string(),
            user: user.to_string(),
        })
    }

    /// One sample of `case`, in a new session.
    pub async fn run(&self, case: &EvalCase) -> Observation {
        let mut obs = Observation::default();
        let started = Instant::now();
        if let Err(e) = self.turns(case, &mut obs).await {
            obs.error = Some(format!("{e:#}"));
        }
        obs.latency_ms = started.elapsed().as_millis() as u64;
        obs
    }

    async fn turns(&self, case: &EvalCase, obs: &mut Observation) -> Result<()> {
        let name = format!("eval-{}-{}", case.eval_case_id, uuid::Uuid::new_v4());
        let session = self.call("/sessions", Some(json!({"name": name}))).await?;
        let id = session.body["id"]
            .as_str()
            .context("the session has no id")?
            .to_string();
        obs.session_id = Some(id.clone());
        let path = format!("/sessions/{id}/messages");
        let (mut calls, mut tokens) = (0, 0);
        for turn in &case.turns {
            let reply = self.call(&path, Some(json!({"text": turn}))).await?;
            let run = &reply.body["run"];
            obs.replies
                .push(reply.body["reply"].as_str().unwrap_or_default().to_string());
            calls += run["model_calls"].as_i64().unwrap_or_default();
            tokens += run["total_tokens"].as_i64().unwrap_or_default();
            obs.model_calls = Some(calls);
            obs.total_tokens = Some(tokens);
            obs.run_id = run["run_id"].as_str().map(str::to_string);
            obs.model = run["model"].as_str().map(str::to_string);
            obs.trace_id = reply.trace_id.or(obs.trace_id.take());
        }
        let history = self.call(&path, None).await?;
        let messages: Vec<Message> = serde_json::from_value(history.body["messages"].clone())
            .context("reading the session's messages")?;
        obs.tool_calls = tool_calls(&messages);
        Ok(())
    }

    /// POST `body` to `path`, or GET it without one.
    async fn call(&self, path: &str, body: Option<Value>) -> Result<Reply> {
        let url = format!("{}{path}", self.base);
        send(&self.client, &url, &self.user, body).await
    }
}

/// A reqwest client with this timeout per request.
pub(crate) fn client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(timeout).build()?)
}

/// A successful JSON response, and the trace it was served under, if the
/// server said.
pub(crate) struct Reply {
    pub body: Value,
    pub trace_id: Option<String>,
}

/// One JSON request to Athena's API as `user`. A non-2xx status is an error
/// carrying the response body.
pub(crate) async fn send(
    client: &reqwest::Client,
    url: &str,
    user: &str,
    body: Option<Value>,
) -> Result<Reply> {
    let request = match body {
        Some(body) => client.post(url).json(&body),
        None => client.get(url),
    };
    let response = request
        .header(crate::http::USER_HEADER, user)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = response.status();
    let trace_id = trace_id(response.headers());
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{url} answered {status}: {text}");
    }
    let body = serde_json::from_str(&text).with_context(|| format!("{url} sent non-JSON"))?;
    Ok(Reply { body, trace_id })
}

/// The trace id from a W3C `traceparent` header, or an `x-trace-id` header.
fn trace_id(headers: &HeaderMap) -> Option<String> {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    match header("traceparent") {
        Some(tp) => tp.split('-').nth(1).map(str::to_string),
        None => header("x-trace-id").map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::case::load;
    use crate::eval::cassette::Interaction;
    use reqwest::header::HeaderValue;
    use rig_core::completion::{CompletionResponse, FinishReason, Usage};
    use rig_core::test_utils::{MockCompletionModel, MockTurn};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-target-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("cases")).unwrap();
        dir
    }

    fn case(dir: &Path, id: &str, turns: &[&str]) -> EvalCase {
        let json = json!({"eval_case_id": id, "kind": "trajectory", "turns": turns});
        let file = dir.join("cases").join(format!("{id}.json"));
        std::fs::write(&file, json.to_string()).unwrap();
        load(&file).unwrap().remove(0)
    }

    fn add_turns() -> Vec<MockTurn> {
        vec![
            MockTurn::tool_call("call_1", "add", json!({"a": 21, "b": 21})).with_usage(Usage {
                total_tokens: 10,
                ..Usage::new()
            }),
            MockTurn::text("42")
                .with_finish_reason(FinishReason::Length)
                .with_usage(Usage {
                    total_tokens: 5,
                    ..Usage::new()
                }),
        ]
    }

    #[test]
    fn finish_reasons_are_read_from_calls_json_and_bad_json_has_none() {
        assert_eq!(
            finish_reasons(r#"[{"finish_reason":"length"},{},{"finish_reason":{"other":"x"}}]"#),
            ["length", r#"{"other":"x"}"#]
        );
        assert!(finish_reasons("not json").is_empty());
    }

    #[test]
    fn trace_ids_come_from_traceparent_then_x_trace_id() {
        let mut headers = HeaderMap::new();
        assert_eq!(trace_id(&headers), None);
        headers.insert("x-trace-id", HeaderValue::from_static("abc"));
        assert_eq!(trace_id(&headers).as_deref(), Some("abc"));
        headers.insert(
            "traceparent",
            HeaderValue::from_static("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        assert_eq!(
            trace_id(&headers).as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
    }

    #[tokio::test]
    async fn recording_then_replaying_reproduces_the_trajectory_without_drift() {
        let dir = temp_dir();
        let case = case(&dir, "add", &["add 21 and 21"]);
        let (cassette, live) = record(&case, MockCompletionModel::new(add_turns()), "m", "u")
            .await
            .unwrap();
        assert_eq!(live.error, None);
        assert_eq!(live.replies, ["42"]);
        assert_eq!(cassette.interactions.len(), 2);
        assert_eq!(cassette.interactions[0].input, ["user: add 21 and 21"]);
        assert_eq!(cassette.interactions[1].input, ["tool add: 42.0"]);
        cassette.write(&case.cassette_path()).unwrap();

        let replayed = replay(&case, "u").await.unwrap();
        assert_eq!((replayed.error, replayed.drift), (None, None));
        assert_eq!(replayed.replies, ["42"]);
        assert_eq!(
            replayed.tool_calls,
            [ToolCall {
                name: "add".into(),
                arguments: json!({"a": 21, "b": 21})
            }]
        );
        assert_eq!(replayed.model_calls, Some(2));
        assert_eq!(replayed.total_tokens, Some(15));
        assert_eq!(replayed.finish_reasons, ["length"]);
        assert_eq!(replayed.model.as_deref(), Some("m"));
        assert!(replayed.run_id.is_some() && replayed.session_id.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_changed_case_drifts_and_a_missing_or_foreign_cassette_fails() {
        let dir = temp_dir();
        let missing = replay(&case(&dir, "none", &["hi"]), "u").await.unwrap();
        assert!(missing.error.unwrap().contains("athena eval record"));

        let text =
            |t: &str| CompletionResponse::new(vec![AssistantContent::text(t)], Usage::new(), "x");
        let recorded = Cassette {
            eval_case_id: "two".into(),
            model: "m".into(),
            interactions: vec![Interaction {
                input: vec!["user: first".into()],
                response: text("ok"),
            }],
        };
        // The case gained a second turn since it was recorded.
        let two = case(&dir, "two", &["first", "second"]);
        recorded.write(&two.cassette_path()).unwrap();
        let drifted = replay(&two, "u").await.unwrap();
        assert_eq!(drifted.replies, ["ok"]);
        assert!(drifted.error.unwrap().contains("trajectory drift"));
        assert!(drifted.drift.unwrap().contains("was not recorded"));

        let foreign = case(&dir, "foreign", &["first"]);
        recorded.write(&foreign.cassette_path()).unwrap();
        let err = replay(&foreign, "u").await.unwrap().error.unwrap();
        assert!(err.contains("is the cassette of `two`"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A real `athena serve` router on loopback, with a scripted model.
    async fn server(turns: Vec<MockTurn>) -> String {
        let service = Arc::new(Service::new(
            Store::open_in_memory().unwrap(),
            "srv",
            crate::cli::warn,
        ));
        let model = MockCompletionModel::new(turns);
        let agent = agent::configure(AgentBuilder::new(model).memory(service.memory()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(crate::http::serve(
            listener,
            service,
            Arc::new(agent),
            std::future::pending(),
        ));
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn the_http_target_rebuilds_the_trajectory_from_the_api() {
        let dir = temp_dir();
        let case = case(&dir, "add", &["add 21 and 21"]);
        let url = server(add_turns()).await;
        let obs = Http::new(&url, "eval").unwrap().run(&case).await;
        assert_eq!(obs.error, None);
        assert_eq!(obs.replies, ["42"]);
        assert_eq!(obs.tool_calls.len(), 1);
        assert_eq!(obs.tool_calls[0].name, "add");
        assert_eq!((obs.model_calls, obs.total_tokens), (Some(2), Some(15)));
        assert_eq!(obs.model.as_deref(), Some("srv"));
        assert!(obs.run_id.is_some() && obs.session_id.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn http_failures_become_the_samples_error() {
        let dir = temp_dir();
        let case = case(&dir, "x", &["hi"]);
        // No scripted turns: the model fails, the API answers 502.
        let url = server(vec![]).await;
        let err = Http::new(&url, "eval")
            .unwrap()
            .run(&case)
            .await
            .error
            .unwrap();
        assert!(err.contains("502"), "{err}");

        let err = Http::new("http://127.0.0.1:1", "eval")
            .unwrap()
            .run(&case)
            .await
            .error
            .unwrap();
        let why = "requesting http://127.0.0.1:1/sessions";
        assert!(err.contains(why), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Answers every request with the same status and body.
    async fn fixed(status: u16, body: &'static str) -> String {
        let router = axum::Router::new().fallback(move || async move {
            (axum::http::StatusCode::from_u16(status).unwrap(), body)
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn malformed_api_responses_are_errors_not_panics() {
        let dir = temp_dir();
        let case = case(&dir, "x", &["hi"]);
        for (body, why) in [
            ("not json", "sent non-JSON"),
            ("{}", "the session has no id"),
            (
                r#"{"id": "s", "reply": "r", "messages": 5}"#,
                "reading the session's messages",
            ),
        ] {
            let url = fixed(200, body).await;
            let err = Http::new(&url, "eval")
                .unwrap()
                .run(&case)
                .await
                .error
                .unwrap();
            assert!(err.contains(why), "{body}: {err}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
