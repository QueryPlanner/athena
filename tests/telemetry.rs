//! What Athena reports to OpenTelemetry, captured in memory around real
//! turns: the production agent in front of Rig's scripted model, the real
//! service and HTTP router, and `athena serve` exporting to a fake
//! OTLP backend and JSONL files. No network beyond loopback.

mod common;

use athena::http::{self, Hosts, USER_HEADER};
use athena::service::Service;
use athena::telemetry::{self, Exporters, Settings, Sinks, TRACE_ID_HEADER, Telemetry};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use http_body_util::BodyExt;
use opentelemetry::trace::{SpanId, Status};
use opentelemetry::{Key, Value as OtelValue};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter, SdkLogRecord};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use rig_agent::agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

// ---- capture ----

/// Keeps every exported span. Unlike the SDK's in-memory exporter it keeps
/// them through shutdown, so a test can check what shutdown flushed.
#[derive(Clone, Debug, Default)]
struct Spans(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Spans {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().unwrap().extend(batch);
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct Logs(Arc<Mutex<Vec<SdkLogRecord>>>);

impl LogExporter for Logs {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let records = batch.iter().map(|(record, _)| record.clone());
        self.0.lock().unwrap().extend(records);
        Ok(())
    }
}

/// What stderr would have shown.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Stderr {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Telemetry installed for this thread only; the tests run turns on a
/// current-thread runtime, so every task they spawn reports here too.
struct Captured {
    spans: Spans,
    logs: Logs,
    stderr: Stderr,
    telemetry: Option<Telemetry>,
    _installed: tracing::subscriber::DefaultGuard,
}

impl Captured {
    fn new(exporting: bool) -> Self {
        Self::in_env(exporting, "staging")
    }

    fn in_env(exporting: bool, environment: &str) -> Self {
        let (spans, logs, stderr) = (Spans::default(), Logs::default(), Stderr::default());
        let settings = Settings::from_vars(|name| match name {
            "ATHENA_VERSION" => Some("test-version".into()),
            "ATHENA_ENV" => Some(environment.into()),
            _ => None,
        })
        .unwrap();
        let mut sinks = Sinks::new(&settings);
        if exporting {
            sinks = sinks.with(Exporters {
                spans: spans.clone(),
                logs: logs.clone(),
            });
        }
        let writer = stderr.clone();
        let (layers, telemetry) = telemetry::layers(&settings, sinks, move || writer.clone());
        let subscriber = tracing_subscriber::registry().with(layers);
        Self {
            spans,
            logs,
            stderr,
            telemetry: Some(telemetry),
            _installed: tracing::subscriber::set_default(subscriber),
        }
    }

    /// Shut telemetry down, which exports what is buffered, and return the
    /// spans that reached the exporter.
    fn finish(&mut self) -> Vec<SpanData> {
        self.telemetry.take().unwrap().shutdown();
        self.spans.0.lock().unwrap().clone()
    }
}

// ---- span inspection ----

fn attribute(span: &SpanData, key: &str) -> Option<OtelValue> {
    span.attributes
        .iter()
        .find(|kv| kv.key == Key::new(key.to_string()))
        .map(|kv| kv.value.clone())
}

fn text(span: &SpanData, key: &str) -> String {
    attribute(span, key)
        .unwrap_or_else(|| panic!("{} has no {key}: {:?}", span.name, span.attributes))
        .to_string()
}

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name == name).collect()
}

fn one<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let found = named(spans, name);
    assert_eq!(found.len(), 1, "{name}: {:?}", names(spans));
    found[0]
}

fn names(spans: &[SpanData]) -> Vec<&str> {
    spans.iter().map(|s| s.name.as_ref()).collect()
}

fn children<'a>(spans: &'a [SpanData], parent: &SpanData) -> Vec<&'a str> {
    let id = parent.span_context.span_id();
    let mut found: Vec<&str> = spans
        .iter()
        .filter(|s| s.parent_span_id == id)
        .map(|s| s.name.as_ref())
        .collect();
    found.sort();
    found
}

/// No attribute of any span carries `needle`.
fn nowhere(spans: &[SpanData], needle: &str) {
    for span in spans {
        for kv in &span.attributes {
            let value = kv.value.to_string();
            assert!(
                !value.contains(needle),
                "{}.{} = {value}",
                span.name,
                kv.key
            );
        }
    }
}

// ---- requests ----

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALLER_SPAN: &str = "00f067aa0ba902b7";

fn api(service: &Arc<Service>, agent: Agent) -> Router {
    http::router(service.clone(), Arc::new(agent), Hosts::Any)
}

fn post(uri: &str, body: Value, traceparent: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "localhost")
        .header(USER_HEADER, "alice");
    if let Some(traceparent) = traceparent {
        builder = builder.header("traceparent", traceparent);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn session(router: &Router) -> String {
    let response = router
        .clone()
        .oneshot(post("/sessions", json!({"name": "s"}), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&bytes).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn caller_traceparent() -> String {
    format!("00-{TRACE_ID}-{CALLER_SPAN}-01")
}

// ---- tests ----

#[tokio::test]
async fn a_turn_is_one_invoke_agent_span_with_rigs_spans_under_it_in_the_callers_trace() {
    let mut captured = Captured::new(true);
    let tmp = TempDb::new();
    let service = Arc::new(tmp.service().0);
    let (agent, _) = mock_agent(&service, add_turns());
    let router = api(&service, agent);
    let id = session(&router).await;

    let prompt = "add 21 and 21";
    let response = router
        .clone()
        .oneshot(post(
            &format!("/sessions/{id}/messages"),
            json!({"text": prompt}),
            Some(&caller_traceparent()),
        ))
        .await
        .unwrap();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let reply: Value = serde_json::from_slice(&bytes).unwrap();
    let spans = captured.finish();

    // The response names the caller's trace, so an eval can find it.
    assert_eq!(headers[TRACE_ID_HEADER], TRACE_ID);
    let traceparent = headers["traceparent"].to_str().unwrap();
    assert!(
        traceparent.starts_with(&format!("00-{TRACE_ID}-")),
        "{traceparent}"
    );

    let messages = named(&spans, "POST /sessions/{id}/messages");
    let request = messages[0];
    assert_eq!(
        request.parent_span_id,
        SpanId::from_hex(CALLER_SPAN).unwrap()
    );
    assert_eq!(text(request, "http.response.status_code"), "200");
    assert_eq!(text(request, "http.route"), "/sessions/{id}/messages");
    assert!(traceparent.contains(&request.span_context.span_id().to_string()));

    let turn = one(&spans, "invoke_agent athena");
    assert_eq!(turn.parent_span_id, request.span_context.span_id());
    assert_eq!(turn.span_context.trace_id().to_string(), TRACE_ID);
    assert_eq!(text(turn, "gen_ai.operation.name"), "invoke_agent");
    assert_eq!(text(turn, "gen_ai.agent.name"), "athena");
    assert_eq!(text(turn, "gen_ai.conversation.id"), id);
    assert_eq!(text(turn, "athena.transport"), "http");
    assert_eq!(
        text(turn, "athena.run_id"),
        reply["run"]["run_id"].as_str().unwrap()
    );
    assert_eq!(text(turn, "gen_ai.usage.input_tokens"), "256");
    assert_eq!(text(turn, "gen_ai.usage.output_tokens"), "26");
    let pseudonym = text(turn, "enduser.pseudo.id");
    assert_eq!(pseudonym.len(), 32, "{pseudonym}");
    assert!(
        pseudonym.chars().all(|c| c.is_ascii_hexdigit()),
        "{pseudonym}"
    );
    assert_eq!(turn.status, Status::Unset);

    // Rig adopted Athena's span instead of opening its own.
    assert_eq!(
        named(&spans, "invoke_agent").len(),
        0,
        "{:?}",
        names(&spans)
    );
    assert_eq!(children(&spans, turn), ["chat", "chat", "execute_tool"]);
    assert_eq!(text(one(&spans, "execute_tool"), "gen_ai.tool.name"), "add");

    // Content capture is off: the prompt and the reply are nowhere.
    nowhere(&spans, prompt);
    nowhere(&spans, "alice");
    assert!(attribute(turn, "gen_ai.prompt").is_none());
}

#[tokio::test]
async fn a_streamed_turn_nests_rigs_streaming_spans_under_athenas() {
    let mut captured = Captured::new(true);
    let tmp = TempDb::new();
    let service = Arc::new(tmp.service().0);
    let user = cli_user(&service).await;
    let session = session_of(&service, &user).await;
    let (agent, _) = mock_stream_agent(service.memory(), streamed_add_turns());

    let mut stream = service
        .send_stream(Arc::new(agent), &user, &session, "add 21 and 21")
        .await
        .unwrap();
    while stream.next().await.is_some() {}
    service.idle().await;
    let spans = captured.finish();

    let turn = one(&spans, "invoke_agent athena");
    assert_eq!(turn.parent_span_id, SpanId::INVALID);
    assert_eq!(text(turn, "athena.transport"), "cli");
    assert_eq!(text(turn, "gen_ai.conversation.id"), session);
    assert!(!text(turn, "athena.run_id").is_empty());
    let nested = children(&spans, turn);
    assert!(nested.contains(&"execute_tool"), "{nested:?}");
    assert_eq!(nested.iter().filter(|n| **n == "chat_streaming").count(), 2);
}

async fn session_of(service: &Service, user: &athena::service::User) -> String {
    service.open_session(user, "s").await.unwrap().id
}

#[tokio::test]
async fn a_failed_turn_marks_its_span_as_an_error_and_logs_without_the_prompt() {
    let mut captured = Captured::new(true);
    let tmp = TempDb::new();
    let service = Arc::new(tmp.service().0);
    let (agent, _) = mock_agent(&service, [MockTurn::error("upstream unavailable")]);
    let router = api(&service, agent);
    let id = session(&router).await;

    let prompt = "a secret prompt";
    let response = router
        .clone()
        .oneshot(post(
            &format!("/sessions/{id}/messages"),
            json!({"text": prompt}),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let spans = captured.finish();

    let turn = one(&spans, "invoke_agent athena");
    assert!(
        matches!(turn.status, Status::Error { .. }),
        "{:?}",
        turn.status
    );
    assert_eq!(text(turn, "error.type"), "model");
    nowhere(&spans, prompt);

    // The server log went out as an OpenTelemetry log record, in the
    // turn's trace, and to stderr.
    let logs = captured.logs.0.lock().unwrap().clone();
    let error = logs
        .iter()
        .find(|r| r.severity_text() == Some("ERROR"))
        .unwrap_or_else(|| panic!("no error record in {} logs", logs.len()));
    let body = format!("{:?}", error.body());
    assert!(body.contains("upstream unavailable"), "{body}");
    assert!(!body.contains(prompt), "{body}");
    let trace = error.trace_context().unwrap().trace_id;
    assert_eq!(trace, turn.span_context.trace_id());
    assert!(captured.stderr.text().contains("upstream unavailable"));
}

#[tokio::test]
async fn with_content_capture_rig_records_the_prompt_on_athenas_span() {
    let spans = content_captured_turn("staging").await;
    let turn = one(&spans, "invoke_agent athena");
    assert_eq!(text(turn, "gen_ai.prompt"), "hi there");
}

#[tokio::test]
async fn prod_exports_no_content_even_with_content_capture_on() {
    let staging = content_captured_turn("staging").await;
    let captured_keys = content_keys(&staging);
    assert!(
        captured_keys.contains(&"gen_ai.prompt".to_string()),
        "{captured_keys:?}"
    );

    let prod = content_captured_turn("prod").await;
    assert_eq!(content_keys(&prod), Vec::<String>::new());
    nowhere(&prod, "hi there");
    // What is not content survives.
    let turn = one(&prod, "invoke_agent athena");
    assert_eq!(text(turn, "athena.transport"), "cli");
}

/// Every content attribute on any span or span event.
fn content_keys(spans: &[SpanData]) -> Vec<String> {
    let mut keys: Vec<String> = spans
        .iter()
        .flat_map(|s| {
            let events = s.events.iter().flat_map(|e| e.attributes.iter());
            s.attributes.iter().chain(events)
        })
        .map(|kv| kv.key.to_string())
        .filter(|k| telemetry::content::CONTENT_KEYS.contains(&k.as_str()))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// One turn with rig's content capture on, exported as `environment`.
async fn content_captured_turn(environment: &str) -> Vec<SpanData> {
    let mut captured = Captured::in_env(true, environment);
    let tmp = TempDb::new();
    let service = Arc::new(tmp.service().0);
    let user = cli_user(&service).await;
    let session = session_of(&service, &user).await;
    // What ATHENA_RECORD_CONTENT=1 switches on in `agent::configure`.
    let agent = AgentBuilder::new(MockCompletionModel::new([MockTurn::text("hello")]))
        .memory(service.memory())
        .record_content_telemetry(true)
        .build();

    service
        .send(&agent, &user, &session, "hi there")
        .await
        .unwrap();
    captured.finish()
}

#[tokio::test]
async fn without_an_exporter_nothing_is_traced_and_stderr_stays_readable() {
    let mut captured = Captured::new(false);
    let tmp = TempDb::new();
    let service = Arc::new(tmp.service().0);
    let (agent, _) = mock_agent(&service, [MockTurn::error("upstream unavailable")]);
    let router = api(&service, agent);
    let id = session(&router).await;

    let response = router
        .clone()
        .oneshot(post(
            &format!("/sessions/{id}/messages"),
            json!({"text": "hi"}),
            Some(&caller_traceparent()),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().get(TRACE_ID_HEADER).is_none());
    assert!(response.headers().get("traceparent").is_none());
    assert!(captured.finish().is_empty());
    assert!(captured.logs.0.lock().unwrap().is_empty());
    // One plain line per event: level and message, not JSON.
    let stderr = captured.stderr.text();
    let line = stderr.lines().find(|l| l.contains("upstream unavailable"));
    let line = line.unwrap_or_else(|| panic!("{stderr}"));
    assert!(line.contains("ERROR"), "{line}");
    assert!(!line.trim_start().starts_with('{'), "{line}");
}

// ---- the real binary ----

/// A fake OTLP/HTTP backend on loopback: each request line it was sent, in
/// order, followed by its `Authorization` header.
fn backend() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0;
            let mut authorization = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("authorization")
                {
                    authorization = value.trim().to_string();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            record
                .lock()
                .unwrap()
                .push(format!("{} | {authorization}", request_line.trim()));
            let reply = "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\n\
                         content-length: 0\r\nconnection: close\r\n\r\n";
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
    (addr, seen)
}

#[test]
fn athena_serve_exports_to_openobserve_and_files_and_flushes_on_sigterm() {
    let (backend, seen) = backend();
    let tmp = TempDb::new();
    let dir = WorkDir::new();
    let telemetry_dir = dir.path().join("telemetry");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_athena"))
        .args(["serve", "--addr", "127.0.0.1:0"])
        .current_dir(dir.path())
        .env("ATHENA_DB", tmp.path())
        .env("OPENROUTER_API_KEY", "unused-key")
        // OpenObserve's shape: an org path, basic auth with the space escaped.
        .env(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            format!("http://{backend}/api/default"),
        )
        .env(
            "OTEL_EXPORTER_OTLP_HEADERS",
            "Authorization=Basic%20dXNlcjpwYXNz",
        )
        .env("ATHENA_TELEMETRY_DIR", &telemetry_dir)
        .env("ATHENA_ENV", "staging")
        .env_remove("ATHENA_ADDR")
        .env_remove("RUST_LOG")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    let addr = line
        .trim()
        .strip_prefix("listening on http://")
        .unwrap_or_else(|| panic!("unexpected first line: {line}"))
        .to_string();

    let mut stream = std::net::TcpStream::connect(&addr).unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut health = String::new();
    stream.read_to_string(&mut health).unwrap();
    // Nothing is exported before the batch is due; shutdown must flush it.
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );

    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let (done, exited) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(child.wait()));
    let status = exited
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("athena serve did not exit after SIGTERM")
        .unwrap();
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).unwrap();

    assert!(killed.success());
    assert!(status.success(), "{status:?}: {rest}");
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(
        health.to_ascii_lowercase().contains("x-trace-id: "),
        "{health}"
    );
    let seen = seen.lock().unwrap().clone();
    for path in ["traces", "logs"] {
        let request = format!("POST /api/default/v1/{path} HTTP/1.1 | Basic dXNlcjpwYXNz");
        assert!(seen.contains(&request), "{seen:?}");
    }
    assert!(!rest.contains("warning: exporting"), "{rest}");

    // The same spans and logs, as JSON lines for DuckDB.
    let mut files: Vec<String> = std::fs::read_dir(&telemetry_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files[0].starts_with("logs-") && files[1].starts_with("traces-"));
    let spans = std::fs::read_to_string(telemetry_dir.join(&files[1])).unwrap();
    let probes: Vec<Value> = spans
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|span| span["name"] == "GET /health")
        .collect();
    assert_eq!(probes.len(), 1, "{spans}");
    assert_eq!(
        probes[0]["resource"]["deployment.environment.name"],
        "staging"
    );
    assert_eq!(probes[0]["attributes"]["http.route"], "/health");
}
