//! The HTTP API over the real service and a real database file, with Rig's
//! scripted model behind the production agent. Handlers are driven
//! in-process through the router; the server itself over real TCP; and
//! `athena serve` as the built binary. No network beyond loopback.

mod common;

use athena::http::{self, Hosts, USER_HEADER};
use athena::service::Service;
use athena::store::SqliteMemory;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::*;
use futures_util::FutureExt;
use http_body_util::BodyExt;
use rig_agent::agent::Agent;
use rig_agent::prelude::Message;
use rig_core::memory::{ConversationMemory, MemoryError};
use rig_core::test_utils::{MockStreamEvent, MockTurn};
use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};
use tower::ServiceExt;

fn api(service: &Arc<Service>, agent: Agent) -> Router {
    http::router(service.clone(), Arc::new(agent), Hosts::Any)
}

fn request(method: &str, uri: &str, user: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost");
    if let Some(user) = user {
        builder = builder.header(USER_HEADER, user);
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    builder.body(body).unwrap()
}

fn get(uri: &str, user: &str) -> Request<Body> {
    request("GET", uri, Some(user), None)
}

fn post(uri: &str, user: &str, body: Value) -> Request<Body> {
    request("POST", uri, Some(user), Some(body))
}

/// Status and JSON body. Every response of this API is JSON, errors included.
async fn call(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&bytes)));
    (status, body)
}

/// A streamed response: status, headers, and its events as (name, data).
async fn stream(
    router: &Router,
    request: Request<Body>,
) -> (StatusCode, HeaderMap, Vec<(String, Value)>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        sse_events(std::str::from_utf8(&bytes).unwrap()),
    )
}

/// Parse SSE framing: blocks separated by a blank line, each with one
/// `event:` and one `data:` line. Comment lines (keep-alives) are skipped.
fn sse_events(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .filter_map(|block| {
            let mut name = None;
            let mut data = None;
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event: ") {
                    name = Some(v.to_string());
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data = Some(serde_json::from_str(v).unwrap());
                }
            }
            Some((name?, data.unwrap()))
        })
        .collect()
}

fn names(events: &[(String, Value)]) -> Vec<&str> {
    events.iter().map(|(n, _)| n.as_str()).collect()
}

/// A service on a fresh database file, shared the way `athena serve` shares it.
fn service(tmp: &TempDb) -> Arc<Service> {
    Arc::new(tmp.service().0)
}

/// Create `name` for `user` through the API and return its id.
async fn create(router: &Router, user: &str, name: &str) -> String {
    let (status, body) = call(router, post("/sessions", user, json!({"name": name}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

/// The `runs` row for a session, as the API reports a run.
fn stored_run(tmp: &TempDb, session_id: &str) -> Value {
    tmp.raw()
        .query_row(
            "SELECT run_id, status, first_seq, last_seq, model_calls, input_tokens, output_tokens
             FROM runs WHERE session_id = ?1",
            [session_id],
            |r| {
                Ok(json!({
                    "run_id": r.get::<_, String>(0)?,
                    "status": r.get::<_, String>(1)?,
                    "first_seq": r.get::<_, i64>(2)?,
                    "last_seq": r.get::<_, i64>(3)?,
                    "model_calls": r.get::<_, i64>(4)?,
                    "input_tokens": r.get::<_, i64>(5)?,
                    "output_tokens": r.get::<_, i64>(6)?,
                }))
            },
        )
        .unwrap()
}

fn same_run(reported: &Value, stored: &Value) {
    for (key, value) in stored.as_object().unwrap() {
        assert_eq!(&reported[key], value, "run field `{key}`: {reported}");
    }
}

// ---- identity ----

#[tokio::test]
async fn the_health_check_needs_no_user() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, _) = mock_agent(&service, []);

    let (status, body) = call(&api(&service, agent), request("GET", "/health", None, None)).await;

    assert_eq!((status, body), (StatusCode::OK, json!({"status": "ok"})));
    assert_eq!(
        count(
            &tmp.raw(),
            "SELECT COUNT(*) FROM users WHERE transport = 'http'"
        ),
        0
    );
}

#[tokio::test]
async fn the_version_needs_no_user_and_ignores_the_host() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, _) = mock_agent(&service, []);
    let router = http::router(service.clone(), Arc::new(agent), Hosts::Loopback);
    let mut request = request("GET", "/version", None, None);
    request
        .headers_mut()
        .insert(header::HOST, "attacker.example".parse().unwrap());

    let (status, body) = call(&router, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"version": athena::ops::version()}));
    assert_eq!(
        count(
            &tmp.raw(),
            "SELECT COUNT(*) FROM users WHERE transport = 'http'"
        ),
        0
    );
}

#[tokio::test]
async fn every_user_endpoint_refuses_a_request_that_does_not_say_who_is_asking() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, model) = mock_agent(&service, []);
    let router = api(&service, agent);
    let text = json!({"text": "hi"});

    for (method, uri, body) in [
        ("GET", "/sessions", None),
        ("POST", "/sessions", Some(json!({"name": "n"}))),
        ("GET", "/sessions/x/messages", None),
        ("POST", "/sessions/x/messages", Some(text.clone())),
        ("POST", "/sessions/x/messages/stream", Some(text.clone())),
        ("GET", "/usage", None),
    ] {
        for user in [None, Some(""), Some("   ")] {
            let (status, body) = call(&router, request(method, uri, user, body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri} {user:?}");
            assert_eq!(body["error"]["code"], "invalid");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(USER_HEADER),
                "{body}"
            );
        }
    }
    let db = tmp.raw();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM users WHERE transport = 'http'"),
        0
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 0);
    assert_eq!(model.request_count(), 0);
}

#[tokio::test]
async fn a_loopback_server_refuses_requests_addressed_to_other_hosts() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, _) = mock_agent(&service, []);
    let router = http::router(service.clone(), Arc::new(agent), Hosts::Loopback);
    let rebound = |uri: &str| {
        let mut r = get(uri, "alice");
        r.headers_mut()
            .insert(header::HOST, "attacker.example".parse().unwrap());
        r
    };

    let (status, body) = call(&router, rebound("/sessions")).await;
    let (ok, _) = call(&router, get("/sessions", "alice")).await;
    let (health, _) = call(&router, rebound("/health")).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden_host");
    assert_eq!((ok, health), (StatusCode::OK, StatusCode::OK));
    // Only the request addressed to localhost created a user.
    assert_eq!(
        count(
            &tmp.raw(),
            "SELECT COUNT(*) FROM users WHERE transport = 'http'"
        ),
        1
    );
}

// ---- sessions and blocking turns ----

#[tokio::test]
async fn sessions_are_created_once_per_name_and_listed() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, _) = mock_agent(&service, []);
    let router = api(&service, agent);

    let id = create(&router, "alice", "notes").await;
    let (duplicate, dup_body) = call(
        &router,
        post("/sessions", "alice", json!({"name": "notes"})),
    )
    .await;
    let (_, list) = call(&router, get("/sessions", "alice")).await;

    assert_eq!(id, session_id(&tmp.raw(), "http", "alice", "notes"));
    assert_eq!(duplicate, StatusCode::CONFLICT);
    assert_eq!(dup_body["error"]["code"], "already_exists");
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        (
            &sessions[0]["id"],
            &sessions[0]["name"],
            &sessions[0]["messages"]
        ),
        (&json!(id), &json!("notes"), &json!(0))
    );
    assert!(sessions[0]["created_at"].as_i64().unwrap() > 0);

    for body in [
        json!({"name": ""}),
        json!({"name": 3}),
        json!({}),
        json!("notes"),
    ] {
        let (status, err) = call(&router, post("/sessions", "alice", body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(err["error"]["code"], "invalid");
    }
    let mut not_json = post("/sessions", "alice", json!(null));
    *not_json.body_mut() = Body::from("name=notes");
    assert_eq!(call(&router, not_json).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM sessions"), 1);
}

#[tokio::test]
async fn a_message_gets_the_reply_and_the_run_it_was_recorded_as() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, model) = mock_agent(&service, add_turns());
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;

    let (status, body) = call(
        &router,
        post(
            &format!("/sessions/{id}/messages"),
            "alice",
            json!({"text": "add 21 and 21"}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reply"], "42");
    same_run(&body["run"], &stored_run(&tmp, &id));
    assert_eq!(body["run"]["first_seq"], 0);
    assert_eq!(body["run"]["last_seq"], 3);
    assert_eq!(body["run"]["session_id"], json!(id));
    assert!(
        body["run"].get("calls_json").is_none(),
        "raw provider data leaked"
    );
    assert_eq!(model.request_count(), 2);

    // The transcript, tool call and result included, as Rig stored it.
    let (_, history) = call(&router, get(&format!("/sessions/{id}/messages"), "alice")).await;
    let messages = history["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0], json!(Message::user("add 21 and 21")));
    let stored: Value = serde_json::from_str(&raw_rows(&tmp.raw(), &id)[1]).unwrap();
    assert_eq!(messages[1], stored);

    let (_, usage) = call(&router, get("/usage", "alice")).await;
    assert_eq!(
        usage["usage"],
        json!([{"session_id": id, "name": "s", "runs": 1, "model_calls": 2,
                "input_tokens": 256, "output_tokens": 26, "cached_input_tokens": 0}])
    );
}

#[tokio::test]
async fn failures_map_to_statuses_without_leaking_their_details() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, _) = mock_agent(&service, [MockTurn::error("upstream secret")]);
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;
    let uri = format!("/sessions/{id}/messages");

    let (model, model_body) = call(&router, post(&uri, "alice", json!({"text": "hi"}))).await;
    let (empty, empty_body) = call(&router, post(&uri, "alice", json!({"text": " "}))).await;
    tmp.raw().execute_batch("DROP TABLE runs").unwrap();
    let (storage, storage_body) = call(&router, get("/usage", "alice")).await;

    assert_eq!(model, StatusCode::BAD_GATEWAY);
    assert_eq!(model_body["error"]["code"], "model");
    assert!(!model_body.to_string().contains("secret"), "{model_body}");
    assert_eq!(empty, StatusCode::BAD_REQUEST);
    assert_eq!(empty_body["error"]["message"], "message must not be empty");
    assert_eq!(storage, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(storage_body["error"]["code"], "storage");
    assert!(!storage_body.to_string().contains("runs"), "{storage_body}");
}

#[tokio::test]
async fn users_never_see_or_touch_each_others_sessions() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, model) = mock_agent(&service, add_turns());
    let router = api(&service, agent);
    let (streaming, stream_model) = mock_stream_agent(service.memory(), streamed_add_turns());
    let streamer = api(&service, streaming);
    let hers = create(&router, "alice", "default").await;
    call(
        &router,
        post(
            &format!("/sessions/{hers}/messages"),
            "alice",
            json!({"text": "add"}),
        ),
    )
    .await;
    let before = raw_rows(&tmp.raw(), &hers);

    let missing = call(&router, get("/sessions/no-such-id/messages", "bob")).await;
    let text = json!({"text": "what did alice say?"});
    for response in [
        call(&router, get(&format!("/sessions/{hers}/messages"), "bob")).await,
        call(
            &router,
            post(&format!("/sessions/{hers}/messages"), "bob", text.clone()),
        )
        .await,
        call(
            &streamer,
            post(
                &format!("/sessions/{hers}/messages/stream"),
                "bob",
                text.clone(),
            ),
        )
        .await,
    ] {
        // Exactly what a session that does not exist gets: ids do not leak.
        assert_eq!(response, missing);
    }
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&router, get("/sessions", "bob")).await.1,
        json!({"sessions": []})
    );
    assert_eq!(
        call(&router, get("/usage", "bob")).await.1,
        json!({"usage": []})
    );

    // The same name is a different session for bob.
    let his = create(&router, "bob", "default").await;
    assert_ne!(his, hers);
    assert_eq!(
        call(&router, get("/sessions", "alice")).await.1["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Nothing of bob's reached the model, and alice's transcript is intact.
    assert_eq!(
        (model.request_count(), stream_model.request_count()),
        (2, 0)
    );
    assert_eq!(raw_rows(&tmp.raw(), &hers), before);
    assert_eq!(runs(&tmp.raw(), &hers).len(), 1);
}

// ---- streamed turns ----

#[tokio::test]
async fn a_streamed_turn_sends_deltas_in_order_then_done_with_its_run() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, _) = mock_stream_agent(service.memory(), streamed_add_turns());
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;

    let (status, headers, events) = stream(
        &router,
        post(
            &format!("/sessions/{id}/messages/stream"),
            "alice",
            json!({"text": "add 21 and 21"}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(headers["x-accel-buffering"], "no");
    assert_eq!(names(&events), ["tool_call", "delta", "delta", "done"]);
    assert_eq!(
        events[0].1,
        json!({"name": "add", "arguments": {"a": 21, "b": 21}})
    );
    assert_eq!(
        (&events[1].1, &events[2].1),
        (&json!({"text": "4"}), &json!({"text": "2"}))
    );
    let done = &events[3].1;
    assert_eq!(done["reply"], "42");
    same_run(&done["run"], &stored_run(&tmp, &id));
    assert_eq!(
        (&done["run"]["first_seq"], &done["run"]["last_seq"]),
        (&json!(0), &json!(3))
    );
    assert_eq!(done["run"]["status"], "ok");
    // Saved exactly as a blocking turn would be.
    let db = tmp.raw();
    assert_eq!(raw_rows(&db, &id).len(), 4);
    assert_eq!(runs(&db, &id), [run_row(0, 3, 2, "ok")]);
}

#[tokio::test]
async fn a_model_failure_mid_stream_ends_it_with_an_error_event_and_is_recorded() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, _) = mock_stream_agent(
        service.memory(),
        [vec![
            MockStreamEvent::text("par"),
            MockStreamEvent::error("upstream secret"),
        ]],
    );
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;

    let (status, _, events) = stream(
        &router,
        post(
            &format!("/sessions/{id}/messages/stream"),
            "alice",
            json!({"text": "hi"}),
        ),
    )
    .await;

    // The stream had started, so the status is already 200.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(names(&events), ["delta", "error"]);
    assert_eq!(events[1].1["error"]["code"], "model");
    assert!(
        !events[1].1.to_string().contains("secret"),
        "{:?}",
        events[1]
    );
    let db = tmp.raw();
    assert!(raw_rows(&db, &id).is_empty());
    assert_eq!(runs(&db, &id), [run_row(0, -1, 0, "error")]);
}

#[tokio::test]
async fn a_bad_stream_request_is_refused_before_streaming_starts() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (agent, model) = mock_stream_agent(service.memory(), streamed_add_turns());
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;
    let uri = format!("/sessions/{id}/messages/stream");

    for (request, status) in [
        (
            post(&uri, "alice", json!({"text": ""})),
            StatusCode::BAD_REQUEST,
        ),
        (
            post(&uri, "alice", json!({"message": "hi"})),
            StatusCode::BAD_REQUEST,
        ),
        (
            post(
                "/sessions/nope/messages/stream",
                "alice",
                json!({"text": "hi"}),
            ),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }
    assert_eq!(model.request_count(), 0);
    assert!(runs(&tmp.raw(), &id).is_empty());
}

/// Parks each turn just before its transcript is written, until released,
/// and says when it gets there. Everything else is the service's memory.
struct ParkedAppend {
    inner: SqliteMemory,
    gate: Arc<Semaphore>,
    appending: mpsc::UnboundedSender<()>,
}

impl ConversationMemory for ParkedAppend {
    fn load<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        self.inner.load(id)
    }

    fn append<'a>(
        &'a self,
        id: &'a str,
        messages: Vec<Message>,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async move {
            self.appending.send(()).unwrap();
            self.gate.acquire().await.unwrap().forget();
            self.inner.append(id, messages).await
        })
    }

    fn clear<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        self.inner.clear(id)
    }
}

fn parked(service: &Service) -> (ParkedAppend, Arc<Semaphore>, mpsc::UnboundedReceiver<()>) {
    let gate = Arc::new(Semaphore::new(0));
    let (appending, parked) = mpsc::unbounded_channel();
    let memory = ParkedAppend {
        inner: service.memory(),
        gate: gate.clone(),
        appending,
    };
    (memory, gate, parked)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_disconnects_mid_stream_still_gets_its_turn_saved() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (memory, gate, mut appending) = parked(&service);
    let (agent, _) = mock_stream_agent(memory, [streamed_text(&["hel", "lo"], usage(9, 2))]);
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;

    let response = router
        .clone()
        .oneshot(post(
            &format!("/sessions/{id}/messages/stream"),
            "alice",
            json!({"text": "hi"}),
        ))
        .await
        .unwrap();
    let mut body = response.into_body();
    let mut received = String::new();
    while !received.contains("event: delta") {
        let frame = body.frame().await.unwrap().unwrap();
        received.push_str(std::str::from_utf8(frame.data_ref().unwrap()).unwrap());
    }
    // The model has answered and the turn is about to save it.
    appending.recv().await.unwrap();
    // The client goes away, the way hyper drops a body whose socket closed.
    drop(body);
    // Still running, and counted: a server shutting down now waits for it.
    assert!(service.idle().now_or_never().is_none());
    gate.add_permits(1);
    service.idle().await;

    let db = tmp.raw();
    assert_eq!(raw_rows(&db, &id).len(), 2);
    assert!(
        raw_rows(&db, &id)[1].contains("hello"),
        "{:?}",
        raw_rows(&db, &id)
    );
    assert_eq!(runs(&db, &id), [run_row(0, 1, 1, "ok")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_gives_up_on_a_blocking_message_still_gets_its_turn_saved() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (memory, gate, mut appending) = parked(&service);
    let (agent, _) = common::mock_agent_with_memory(memory, add_turns());
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;

    let request = post(
        &format!("/sessions/{id}/messages"),
        "alice",
        json!({"text": "add"}),
    );
    tokio::select! {
        response = router.clone().oneshot(request) => {
            panic!("the turn cannot finish while parked: {response:?}")
        }
        parked = appending.recv() => parked.unwrap(),
    }
    // The request future, and the handler with it, is gone. The turn is not.
    assert!(service.idle().now_or_never().is_none());
    gate.add_permits(1);
    service.idle().await;

    let db = tmp.raw();
    assert_eq!(raw_rows(&db, &id).len(), 4);
    assert_eq!(runs(&db, &id), [run_row(0, 3, 2, "ok")]);
}

/// A memory whose appends panic: a bug inside a turn.
struct PanicsOnAppend(SqliteMemory);

impl ConversationMemory for PanicsOnAppend {
    fn load<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        self.0.load(id)
    }

    fn append<'a>(
        &'a self,
        _: &'a str,
        _: Vec<Message>,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async { panic!("a bug in the turn") })
    }

    fn clear<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        self.0.clear(id)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_panics_mid_stream_still_ends_the_stream_with_an_error_event() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let memory = PanicsOnAppend(service.memory());
    let (agent, _) = mock_stream_agent(memory, [streamed_text(&["partial"], usage(1, 1))]);
    let router = api(&service, agent);
    let id = create(&router, "alice", "s").await;

    let (_, _, events) = stream(
        &router,
        post(
            &format!("/sessions/{id}/messages/stream"),
            "alice",
            json!({"text": "hi"}),
        ),
    )
    .await;

    assert_eq!(names(&events), ["delta", "error"]);
    assert_eq!(events[1].1["error"]["code"], "internal");
    service.idle().await;
    assert!(raw_rows(&tmp.raw(), &id).is_empty());
}

// ---- the server ----

/// Interrupts a test sends by hand, standing in for Ctrl-C.
fn interrupts() -> (
    mpsc::UnboundedSender<()>,
    impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
) {
    let (send, receive) = mpsc::unbounded_channel::<()>();
    let receive = Arc::new(tokio::sync::Mutex::new(receive));
    let next = move || {
        let receive = receive.clone();
        Box::pin(async move {
            receive.lock().await.recv().await;
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    };
    (send, next)
}

/// Send a raw HTTP/1.1 request that asks the server to close afterwards.
async fn send_raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    user: &str,
    body: &str,
) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{USER_HEADER}: {user}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn read_all(mut stream: tokio::net::TcpStream) -> String {
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_answers_over_tcp_and_finishes_turns_in_flight_before_it_stops() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (memory, gate, mut appending) = parked(&service);
    let (agent, _) = common::mock_agent_with_memory(memory, add_turns());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (interrupt, next) = interrupts();
    let server = tokio::spawn(http::serve_until_interrupted(
        listener,
        Hosts::Loopback,
        service.clone(),
        Arc::new(agent),
        next,
    ));
    let alice = service.user("http", "alice").await.unwrap();
    let id = service.create_session(&alice, "s").await.unwrap().id;

    let health = read_all(send_raw(addr, "GET", "/health", "alice", "").await).await;
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(health.ends_with(r#"{"status":"ok"}"#), "{health}");

    let client = send_raw(
        addr,
        "POST",
        &format!("/sessions/{id}/messages"),
        "alice",
        r#"{"text":"add"}"#,
    )
    .await;
    appending.recv().await.unwrap();
    interrupt.send(()).unwrap();
    // The client is gone before the turn ends; the turn is not.
    drop(client);
    gate.add_permits(1);

    server.await.unwrap().unwrap();
    // `serve_until_interrupted` returned, so the turn is already saved.
    let db = tmp.raw();
    assert_eq!(raw_rows(&db, &id).len(), 4);
    assert_eq!(runs(&db, &id), [run_row(0, 3, 2, "ok")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_interrupt_quits_without_waiting_for_turns_in_flight() {
    let tmp = TempDb::new();
    let service = service(&tmp);
    let (memory, gate, mut appending) = parked(&service);
    let (agent, _) = common::mock_agent_with_memory(memory, add_turns());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (interrupt, next) = interrupts();
    let server = tokio::spawn(http::serve_until_interrupted(
        listener,
        Hosts::Loopback,
        service.clone(),
        Arc::new(agent),
        next,
    ));
    let alice = service.user("http", "alice").await.unwrap();
    let id = service.create_session(&alice, "s").await.unwrap().id;

    let _client = send_raw(
        addr,
        "POST",
        &format!("/sessions/{id}/messages"),
        "alice",
        r#"{"text":"add"}"#,
    )
    .await;
    appending.recv().await.unwrap();
    interrupt.send(()).unwrap();
    interrupt.send(()).unwrap();

    // The turn is still parked, so only the second interrupt can end this.
    let err = server.await.unwrap().unwrap_err();
    assert!(err.to_string().contains("interrupted twice"), "{err}");
    assert!(raw_rows(&tmp.raw(), &id).is_empty());
    gate.add_permits(1);
    service.idle().await;
}

// ---- the real binary ----

/// `athena serve` in `dir`, an empty directory, so a developer's `.env` in
/// the repository can never reach it.
fn athena_serve(
    dir: &WorkDir,
    tmp: &TempDb,
    args: &[&str],
    key: Option<&str>,
) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_athena"));
    command
        .arg("serve")
        .args(args)
        .current_dir(dir.path())
        .env("ATHENA_DB", tmp.path())
        .env_remove("AGENT_MODEL")
        .env_remove("ATHENA_ADDR")
        .env_remove("ATHENA_ALLOWED_HOSTS")
        .env_remove("ATHENA_VERSION")
        .env_remove("OPENROUTER_API_KEY");
    if let Some(key) = key {
        command.env("OPENROUTER_API_KEY", key);
    }
    command
}

fn blocking_request(addr: &str, request: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn the_binary_serves_until_ctrl_c_and_then_exits_cleanly() {
    serves_until("INT");
}

#[test]
fn the_binary_serves_until_sigterm_and_then_exits_cleanly() {
    serves_until("TERM");
}

/// A running `athena serve` on a free loopback port: the process, the
/// address it announced, and the rest of its stderr.
struct Serving {
    child: std::process::Child,
    addr: String,
    stderr: std::io::BufReader<std::process::ChildStderr>,
}

/// Start `athena serve` on 127.0.0.1 with `env` set. A key that is never
/// used: nothing here reaches the model.
fn start_serving(dir: &WorkDir, tmp: &TempDb, env: &[(&str, &str)]) -> Serving {
    let mut command = athena_serve(dir, tmp, &["--addr", "127.0.0.1:0"], Some("unused-key"));
    command.envs(env.iter().copied());
    let mut child = command
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
    Serving {
        child,
        addr,
        stderr,
    }
}

impl Serving {
    /// `GET path` as `alice`, addressed to `host`.
    fn get(&self, path: &str, host: &str) -> String {
        blocking_request(
            &self.addr,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\n{USER_HEADER}: alice\r\n\
                 Connection: close\r\n\r\n"
            ),
        )
    }

    /// Stop it with `signal` and return the rest of its stderr once it has
    /// exited cleanly.
    fn stop(mut self, signal: &str) -> String {
        let interrupted = std::process::Command::new("kill")
            .args([&format!("-{signal}"), &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(interrupted.success());
        // The timeout only turns a hang into a failure.
        let (done, exited) = std::sync::mpsc::channel();
        let mut child = self.child;
        std::thread::spawn(move || done.send(child.wait()));
        let status = exited
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("athena serve did not exit after the signal")
            .unwrap();
        let mut rest = String::new();
        self.stderr.read_to_string(&mut rest).unwrap();
        assert!(status.success(), "{status:?}: {rest}");
        rest
    }
}

/// Start `athena serve`, use it, send it `signal`, and check it shut down
/// gracefully rather than being killed.
fn serves_until(signal: &str) {
    let tmp = TempDb::new();
    let dir = WorkDir::new();
    let server = start_serving(&dir, &tmp, &[("ATHENA_VERSION", "sha-test")]);
    let addr = server.addr.clone();

    let health = server.get("/health", "localhost");
    let version = server.get("/version", "localhost");
    let body = r#"{"name":"notes"}"#;
    let created = blocking_request(
        &addr,
        &format!(
            "POST /sessions HTTP/1.1\r\nHost: localhost\r\n{USER_HEADER}: alice\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    let rest = server.stop(signal);

    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(version.ends_with(r#"{"version":"sha-test"}"#), "{version}");
    assert!(created.starts_with("HTTP/1.1 201 Created"), "{created}");
    assert!(rest.contains("shutting down"), "{rest}");
    assert!(!rest.contains("WARNING"), "loopback must not warn: {rest}");
    assert!(!session_id(&tmp.raw(), "http", "alice", "notes").is_empty());
}

#[test]
fn the_binary_refuses_to_serve_without_a_key_or_with_bad_arguments() {
    let tmp = TempDb::new();

    for (args, key, why) in [
        (&[][..], None, "OPENROUTER_API_KEY"),
        (
            &["--port", "1"][..],
            Some("unused-key"),
            "usage: athena serve",
        ),
        (
            &["--addr", "not an address"][..],
            Some("unused-key"),
            "listening on not an address",
        ),
    ] {
        let out = athena_serve(&WorkDir::new(), &tmp, args, key)
            .output()
            .unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(stderr.contains(why), "{args:?}: {stderr}");
        assert!(!stderr.contains("listening on http"), "{args:?}: {stderr}");
    }
}

#[test]
fn the_binary_answers_only_allowlisted_hosts_and_does_not_warn() {
    let tmp = TempDb::new();
    let dir = WorkDir::new();
    let server = start_serving(
        &dir,
        &tmp,
        &[("ATHENA_ALLOWED_HOSTS", "athena.test, other.test:1")],
    );
    let port = server.addr.rsplit(':').next().unwrap().to_string();

    let listed = server.get("/sessions", &format!("athena.test:{port}"));
    let loopback = server.get("/sessions", "localhost");
    let wrong_port = server.get("/sessions", &format!("other.test:{port}"));
    let foreign = server.get("/sessions", "evil.example");
    let rest = server.stop("TERM");

    assert!(listed.starts_with("HTTP/1.1 200 OK"), "{listed}");
    assert!(loopback.starts_with("HTTP/1.1 200 OK"), "{loopback}");
    for refused in [&wrong_port, &foreign] {
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert!(refused.contains("forbidden_host"), "{refused}");
    }
    assert!(
        rest.starts_with("answering only Host: athena.test, other.test:1\n"),
        "{rest}"
    );
    assert!(!rest.contains("WARNING"), "{rest}");
}

#[test]
fn the_binary_refuses_to_serve_without_an_absolute_database_path() {
    let dir = WorkDir::new();
    let tmp = TempDb::new();

    for (db, why) in [
        (None, "ATHENA_DB is not set"),
        (Some("agent.db"), "must be an absolute path"),
    ] {
        let mut command = athena_serve(&dir, &tmp, &[], Some("unused-key"));
        match db {
            Some(db) => command.env("ATHENA_DB", db),
            None => command.env_remove("ATHENA_DB"),
        };
        let out = command.output().unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(!out.status.success(), "{db:?}");
        assert!(stderr.contains(why), "{db:?}: {stderr}");
    }
    assert!(!dir.path().join("agent.db").exists());
}

#[test]
fn the_binary_refuses_a_malformed_host_allowlist_before_listening() {
    let tmp = TempDb::new();
    let out = athena_serve(&WorkDir::new(), &tmp, &[], Some("unused-key"))
        .env("ATHENA_ALLOWED_HOSTS", "https://athena.test")
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();

    assert!(!out.status.success());
    assert!(stderr.contains("without a scheme"), "{stderr}");
    assert!(!stderr.contains("listening on http"), "{stderr}");
}
