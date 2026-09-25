//! The HTTP transport: a JSON API with server-sent events for streamed
//! turns, over [`Service`]. Nothing here touches the database directly.
//!
//! It is unauthenticated. Whoever sends a request names the user they act
//! as in the `X-Athena-User` header, and that header is trusted. [`Caller`]
//! is the one place a request becomes a [`User`], so real authentication
//! replaces that extractor and nothing else. Until then the server must only
//! listen where every client is trusted; README "Known limits" says why.
//!
//! The endpoints and their JSON are listed in README "HTTP API".

use crate::service::{
    self, RunRecord, Service, Session, SessionSummary, SessionUsage, Turn, TurnEvent, TurnStream,
    User,
};
use crate::telemetry;
use anyhow::{Context, bail};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::{HeaderMap, StatusCode, header, request::Parts};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures_util::Stream;
use rig_agent::agent::Agent;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The header that says who is asking. See the module docs.
pub const USER_HEADER: &str = "x-athena-user";

/// Where `athena serve` listens without `--addr` or `ATHENA_ADDR`.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// Longer ids are refused: every distinct id becomes a stored user.
const MAX_USER_ID: usize = 256;

/// Printed when the server listens beyond this machine.
pub const EXPOSED_WARNING: &str = "\
WARNING: athena is listening on a non-loopback address.
WARNING: The HTTP API is UNAUTHENTICATED. Anyone who can reach it can act as
WARNING: any user, read every session, spend your model credit and use the
WARNING: agent's tools, including read_file on this machine.
WARNING: Listen on 127.0.0.1 unless something in front of it authenticates.";

/// Which `Host` headers the API answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosts {
    /// Only `localhost`, `127.0.0.1` and `[::1]`. A web page that points its
    /// own domain at 127.0.0.1 (DNS rebinding) sends its domain as the
    /// host, so it cannot use a server listening on loopback.
    Loopback,
    /// Any. For a server listening beyond loopback, reached by other names.
    Any,
}

#[derive(Clone)]
struct App {
    service: Arc<Service>,
    agent: Arc<Agent>,
    hosts: Hosts,
}

/// The API. `agent` must be built with `service.memory()`.
pub fn router(service: Arc<Service>, agent: Arc<Agent>, hosts: Hosts) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}/messages", get(messages).post(send))
        .route("/sessions/{id}/messages/stream", post(send_stream))
        .route("/usage", get(usage))
        .layer(axum::middleware::from_fn(traced))
        .with_state(App {
            service,
            agent,
            hosts,
        })
}

/// Serve the API on `listener` until `shutdown` resolves, then stop
/// accepting, let open requests finish, and wait for every turn still
/// running, including those whose client has gone.
pub async fn serve(
    listener: TcpListener,
    service: Arc<Service>,
    agent: Arc<Agent>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let hosts = hosts_for(listener.local_addr()?);
    axum::serve(listener, router(service.clone(), agent, hosts))
        .with_graceful_shutdown(shutdown)
        .await?;
    service.idle().await;
    Ok(())
}

/// `athena serve [--addr HOST:PORT]`.
///
/// `interrupt` returns a future that resolves on the next stop signal
/// (SIGINT or SIGTERM); see [`serve_until_interrupted`].
pub async fn run<F: Future<Output = ()> + Send + 'static>(
    args: &[String],
    service: Arc<Service>,
    agent: Arc<Agent>,
    interrupt: impl Fn() -> F,
) -> anyhow::Result<()> {
    let addr = addr(args, std::env::var("ATHENA_ADDR").ok())?;
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    announce(listener.local_addr()?, &mut std::io::stderr());
    serve_until_interrupted(listener, service, agent, interrupt).await
}

/// [`serve`] until the first interrupt, then shut down gracefully. A second
/// interrupt quits at once, without waiting for the turns in flight: those
/// are lost, as with a crash.
pub async fn serve_until_interrupted<F: Future<Output = ()> + Send + 'static>(
    listener: TcpListener,
    service: Arc<Service>,
    agent: Arc<Agent>,
    interrupt: impl Fn() -> F,
) -> anyhow::Result<()> {
    let (stopping, stopped) = tokio::sync::oneshot::channel();
    let first = interrupt();
    let shutdown = async move {
        first.await;
        tracing::info!(
            "shutting down after the turns in flight; signal again (Ctrl-C) to quit now"
        );
        // Nobody listens once `run` has returned.
        let _ = stopping.send(());
    };
    let forced = async {
        // Only a signal after the first one counts. An error means `serve`
        // ended without a shutdown, and then it has already won the select.
        let _ = stopped.await;
        interrupt().await;
    };
    tokio::select! {
        served = serve(listener, service, agent, shutdown) => Ok(served?),
        () = forced => bail!("interrupted twice; quit without waiting for the turns in flight"),
    }
}

/// A server listening on loopback only answers loopback host names.
fn hosts_for(addr: SocketAddr) -> Hosts {
    match addr.ip().is_loopback() {
        true => Hosts::Loopback,
        false => Hosts::Any,
    }
}

/// The address to listen on: `--addr`, else `ATHENA_ADDR`, else the default.
fn addr(args: &[String], configured: Option<String>) -> anyhow::Result<String> {
    match args {
        [] => Ok(configured.unwrap_or_else(|| DEFAULT_ADDR.into())),
        [flag, addr] if flag == "--addr" => Ok(addr.clone()),
        _ => bail!("usage: athena serve [--addr HOST:PORT]"),
    }
}

/// Say where the server listens, and warn loudly if that is not loopback.
fn announce(addr: SocketAddr, out: &mut impl Write) {
    // Nowhere left to report a failure to write to stderr.
    let _ = writeln!(out, "listening on http://{addr}");
    if !addr.ip().is_loopback() {
        let _ = writeln!(out, "{EXPOSED_WARNING}");
    }
}

// ---- identity ----

/// The user a request acts as. The only place identity is decided.
///
/// Today: `X-Athena-User: <id>` is the user `("http", id)`, trusted as sent.
/// Replace this extractor to add authentication.
struct Caller(User);

impl FromRequestParts<App> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, ApiError> {
        if app.hosts == Hosts::Loopback {
            loopback_host(&parts.headers)?;
        }
        let id = user_id(&parts.headers)?;
        Ok(Caller(app.service.user("http", id).await?))
    }
}

fn user_id(headers: &HeaderMap) -> Result<&str, ApiError> {
    let missing = || {
        ApiError::invalid(format!(
            "send the user you act as in the {USER_HEADER} header"
        ))
    };
    let value = headers.get(USER_HEADER).ok_or_else(missing)?;
    let id = value
        .to_str()
        .map_err(|_| ApiError::invalid(format!("{USER_HEADER} must be printable ASCII")))?
        .trim();
    if id.is_empty() {
        return Err(missing());
    }
    if id.len() > MAX_USER_ID {
        return Err(ApiError::invalid(format!(
            "{USER_HEADER} must be at most {MAX_USER_ID} characters"
        )));
    }
    Ok(id)
}

fn loopback_host(headers: &HeaderMap) -> Result<(), ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Drop the port: `[::1]:8080` keeps its brackets, `localhost:8080` its name.
    let name = match host.find(']') {
        Some(end) => &host[..=end],
        None => host.split(':').next().unwrap_or_default(),
    };
    match name {
        "localhost" | "127.0.0.1" | "[::1]" => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "forbidden_host",
            message: format!(
                "this server only answers requests addressed to localhost, not `{host}`"
            ),
        }),
    }
}

// ---- errors ----

/// An error response: `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn invalid(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid",
            message,
        }
    }

    fn body(&self) -> Value {
        json!({"error": {"code": self.code, "message": self.message}})
    }
}

impl From<service::Error> for ApiError {
    fn from(e: service::Error) -> Self {
        use service::Error::*;
        let (status, code) = match &e {
            NotFound => (StatusCode::NOT_FOUND, "not_found"),
            AlreadyExists(_) => (StatusCode::CONFLICT, "already_exists"),
            Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
            Model(_) => (StatusCode::BAD_GATEWAY, "model"),
            Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "storage"),
        };
        // Provider and database errors can carry internals an
        // unauthenticated client has no business seeing. The server log and
        // the run's `error` column have them.
        let message = match &e {
            Model(_) | Storage(_) => {
                log(&e.to_string());
                format!("the {code} failed; the server log has the details")
            }
            _ => e.to_string(),
        };
        Self {
            status,
            code,
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

fn log(message: &str) {
    tracing::error!("{message}");
}

// ---- tracing ----

/// Run each request in a server span, parented to the caller's W3C
/// `traceparent` if it sent one, and name the trace in the response's
/// `x-trace-id` and `traceparent` headers so a client can find it. Without
/// OpenTelemetry there is no trace, and no such headers.
async fn traced(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = request.method().clone();
    // The route, not the path: session ids would make every name unique.
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", |path| path.as_str())
        .to_string();
    let span = tracing::info_span!(
        "http.request",
        otel.name = format!("{method} {route}"),
        otel.kind = "server",
        http.request.method = method.as_str(),
        http.route = route,
        http.response.status_code = tracing::field::Empty,
    );
    // Only fails when OpenTelemetry is off, when there is no trace to join.
    let _ = span.set_parent(telemetry::remote_context(request.headers()));
    let mut response = next.run(request).instrument(span.clone()).await;
    span.record("http.response.status_code", response.status().as_u16());
    response
        .headers_mut()
        .extend(telemetry::trace_headers(&span));
    response
}

// ---- handlers ----

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn create_session(
    State(app): State<App>,
    Caller(user): Caller,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let name = field(&body, "name")?;
    let session = app.service.create_session(&user, &name).await?;
    Ok((StatusCode::CREATED, Json(session_json(&session))))
}

async fn list_sessions(
    State(app): State<App>,
    Caller(user): Caller,
) -> Result<Json<Value>, ApiError> {
    let sessions = app.service.sessions(&user).await?;
    let sessions: Vec<Value> = sessions.iter().map(summary_json).collect();
    Ok(Json(json!({"sessions": sessions})))
}

async fn messages(
    State(app): State<App>,
    Caller(user): Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let messages = app.service.history(&user, &id).await?;
    Ok(Json(json!({"messages": messages})))
}

async fn send(
    State(app): State<App>,
    Caller(user): Caller,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let text = field(&body, "text")?;
    let turn = app
        .service
        .send_detached(app.agent.clone(), &user, &id, &text)
        .await?;
    Ok(Json(turn_json(&turn)))
}

async fn send_stream(
    State(app): State<App>,
    Caller(user): Caller,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let text = field(&body, "text")?;
    let turn = app
        .service
        .send_stream(app.agent.clone(), &user, &id, &text)
        .await?;
    let events = Sse::new(sse_events(turn)).keep_alive(KeepAlive::default());
    // Asks nginx and similar proxies not to hold deltas back.
    Ok(([("x-accel-buffering", "no")], events).into_response())
}

async fn usage(State(app): State<App>, Caller(user): Caller) -> Result<Json<Value>, ApiError> {
    let usage = app.service.usage(&user).await?;
    let usage: Vec<Value> = usage.iter().map(usage_json).collect();
    Ok(Json(json!({"usage": usage})))
}

/// A string field of a JSON object body.
fn field(body: &[u8], name: &str) -> Result<String, ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::invalid(format!("the body must be JSON: {e}")))?;
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError::invalid(format!(
                "the body must be a JSON object with a string `{name}`"
            ))
        })
}

// ---- server-sent events ----

/// A streamed turn as SSE: `delta` and `tool_call` events as they happen,
/// then exactly one `done` or `error`, then the end of the stream.
fn sse_events(turn: TurnStream) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(Some(turn), |turn| async move {
        let mut turn = turn?;
        let (event, last) = sse_event(turn.next().await);
        Some((Ok(event), (!last).then_some(turn)))
    })
}

/// One SSE event, and whether it is the last.
fn sse_event(event: Option<TurnEvent>) -> (Event, bool) {
    let (name, data, last) = match event {
        Some(TurnEvent::Text(text)) => ("delta", json!({"text": text}), false),
        Some(TurnEvent::ToolCall { name, arguments }) => (
            "tool_call",
            json!({"name": name, "arguments": arguments}),
            false,
        ),
        Some(TurnEvent::Done(Ok(turn))) => ("done", turn_json(&turn), true),
        Some(TurnEvent::Done(Err(e))) => ("error", ApiError::from(e).body(), true),
        // The turn's task panicked; tokio has already printed why.
        None => (
            "error",
            json!({"error": {
                "code": "internal",
                "message": "the turn ended unexpectedly; the server log has the details"
            }}),
            true,
        ),
    };
    (Event::default().event(name).data(data.to_string()), last)
}

// ---- JSON shapes ----

fn session_json(s: &Session) -> Value {
    json!({"id": s.id, "name": s.name, "created_at": s.created_at})
}

fn summary_json(s: &SessionSummary) -> Value {
    let mut value = session_json(&s.session);
    value["messages"] = json!(s.messages);
    value
}

fn usage_json(u: &SessionUsage) -> Value {
    json!({
        "session_id": u.session_id,
        "name": u.name,
        "runs": u.runs,
        "model_calls": u.model_calls,
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cached_input_tokens": u.cached_input_tokens,
    })
}

fn turn_json(turn: &Turn) -> Value {
    json!({"reply": turn.reply, "run": run_json(&turn.run)})
}

/// A run without `calls_json`, which holds raw provider responses.
fn run_json(r: &RunRecord) -> Value {
    json!({
        "run_id": r.run_id,
        "session_id": r.session_id,
        "status": r.status,
        "error": r.error,
        "model": r.model,
        "first_seq": r.first_seq,
        "last_seq": r.last_seq,
        "model_calls": r.model_calls,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "total_tokens": r.total_tokens,
        "cached_input_tokens": r.cached_input_tokens,
        "reasoning_tokens": r.reasoning_tokens,
        "started_at": r.started_at,
        "ended_at": r.ended_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn headers(pairs: &[(&'static str, HeaderValue)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.clone());
        }
        map
    }

    #[test]
    fn the_address_comes_from_the_flag_then_the_environment_then_the_default() {
        let env = || Some("0.0.0.0:1".to_string());
        assert_eq!(addr(&args(&[]), None).unwrap(), DEFAULT_ADDR);
        assert_eq!(addr(&args(&[]), env()).unwrap(), "0.0.0.0:1");
        assert_eq!(
            addr(&args(&["--addr", "[::1]:9"]), env()).unwrap(),
            "[::1]:9"
        );
        for bad in [&["--addr"][..], &["--port", "1"][..], &["x", "y", "z"][..]] {
            let err = addr(&args(bad), None).unwrap_err().to_string();
            assert!(err.contains("usage: athena serve"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_non_loopback_address_is_announced_with_a_warning() {
        for (addr, warned) in [
            ("127.0.0.1:8080", false),
            ("[::1]:8080", false),
            ("0.0.0.0:8080", true),
            ("192.168.1.5:80", true),
        ] {
            let addr: SocketAddr = addr.parse().unwrap();
            let mut out = Vec::new();
            announce(addr, &mut out);
            let out = String::from_utf8(out).unwrap();
            assert!(out.starts_with(&format!("listening on http://{addr}\n")));
            assert_eq!(out.contains("UNAUTHENTICATED"), warned, "{addr}");
            let expected = if warned { Hosts::Any } else { Hosts::Loopback };
            assert_eq!(hosts_for(addr), expected, "{addr}");
        }
    }

    #[test]
    fn the_user_header_must_name_someone_briefly_in_ascii() {
        let ok = headers(&[(USER_HEADER, HeaderValue::from_static("  alice "))]);
        assert_eq!(user_id(&ok).unwrap(), "alice");
        let longest = "a".repeat(MAX_USER_ID);
        let ok = headers(&[(USER_HEADER, HeaderValue::from_str(&longest).unwrap())]);
        assert_eq!(user_id(&ok).unwrap(), longest);

        for (map, why) in [
            (headers(&[]), "header"),
            (
                headers(&[(USER_HEADER, HeaderValue::from_static("   "))]),
                "header",
            ),
            (
                headers(&[(USER_HEADER, HeaderValue::from_bytes(b"\xc3\xa9").unwrap())]),
                "printable ASCII",
            ),
            (
                headers(&[(
                    USER_HEADER,
                    HeaderValue::from_str(&"a".repeat(MAX_USER_ID + 1)).unwrap(),
                )]),
                "at most",
            ),
        ] {
            let err = user_id(&map).unwrap_err();
            assert_eq!((err.status, err.code), (StatusCode::BAD_REQUEST, "invalid"));
            assert!(err.message.contains(why), "{}", err.message);
        }
    }

    #[test]
    fn a_loopback_server_answers_only_loopback_host_names() {
        for host in [
            "localhost",
            "LOCALHOST:8080",
            "127.0.0.1:1",
            "[::1]",
            "[::1]:8080",
        ] {
            let map = headers(&[("host", HeaderValue::from_static(host))]);
            assert!(loopback_host(&map).is_ok(), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:8080",
            "127.0.0.1.nip.io",
            "[::2]:1",
        ] {
            let map = headers(&[("host", HeaderValue::from_static(host))]);
            let err = loopback_host(&map).unwrap_err();
            assert_eq!(
                (err.status, err.code),
                (StatusCode::FORBIDDEN, "forbidden_host")
            );
            assert!(err.message.contains(host), "{}", err.message);
        }
        assert!(loopback_host(&headers(&[])).is_err());
    }

    #[test]
    fn service_errors_map_to_statuses_and_hide_internals() {
        use service::Error;
        for (error, status, code, message) in [
            (Error::NotFound, 404, "not_found", "no such session"),
            (
                Error::AlreadyExists("n".into()),
                409,
                "already_exists",
                "a session named `n` already exists",
            ),
            (
                Error::Conflict("moved".into()),
                409,
                "conflict",
                "moved; the reply was not saved, send it again",
            ),
            (Error::Invalid("bad".into()), 400, "invalid", "bad"),
            (
                Error::Model(anyhow::anyhow!("secret upstream body")),
                502,
                "model",
                "the model failed; the server log has the details",
            ),
            (
                Error::Storage(anyhow::anyhow!("/private/path.db locked")),
                500,
                "storage",
                "the storage failed; the server log has the details",
            ),
        ] {
            let api = ApiError::from(error);
            assert_eq!(api.status.as_u16(), status);
            assert_eq!(
                api.body(),
                json!({"error": {"code": code, "message": message}})
            );
        }
    }

    #[test]
    fn a_turn_that_ends_without_done_is_reported_as_an_internal_error() {
        let (_, last) = sse_event(None);
        assert!(last);
        let (_, last) = sse_event(Some(TurnEvent::Text("x".into())));
        assert!(!last);
    }

    #[test]
    fn a_body_must_be_a_json_object_with_the_named_string() {
        assert_eq!(field(br#"{"text":"hi","x":1}"#, "text").unwrap(), "hi");
        for (body, why) in [
            (&b"not json"[..], "must be JSON"),
            (&br#"{"text": 5}"#[..], "string `text`"),
            (&br#"["text"]"#[..], "string `text`"),
            (&b""[..], "must be JSON"),
        ] {
            let err = field(body, "text").unwrap_err();
            assert_eq!(err.code, "invalid");
            assert!(err.message.contains(why), "{}", err.message);
        }
    }
}
