//! A fake OpenSandbox server: the lifecycle API and, behind its proxy, a
//! fake execd, with every request recorded.
//!
//! It answers the way OpenSandbox 0.2.3 did when probed: the execd endpoint
//! comes back without a scheme, execd streams bare JSON objects separated
//! by blank lines, and an unknown bash session is a 500 saying "not found".
//! Commands are not run: by default each one prints `ran: <command>`, and
//! [`FakeSandbox::reply_next`] scripts the next stream body instead.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

/// One request a client made.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Value,
    pub headers: HeaderMap,
}

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Inner {
    host: String,
    requests: Mutex<Vec<Recorded>>,
    /// Live sandboxes and their state.
    sandboxes: Mutex<HashMap<String, String>>,
    next: Mutex<usize>,
    /// States a new sandbox reports: the first on create, the rest on each
    /// later GET, the last one repeating.
    startup: Mutex<Vec<String>>,
    bash: Mutex<HashSet<String>>,
    contexts: Mutex<HashSet<String>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    replies: Mutex<VecDeque<(StatusCode, String)>>,
    /// The next response to a request whose path ends with the key.
    faults: Mutex<HashMap<String, (StatusCode, String)>>,
    endpoint: Mutex<Option<Value>>,
    on_create: Mutex<Option<Hook>>,
}

pub struct FakeSandbox {
    pub url: String,
    inner: Arc<Inner>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeSandbox {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeSandbox {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let inner = Arc::new(Inner {
            host: host.clone(),
            startup: Mutex::new(vec!["Running".into()]),
            ..Default::default()
        });
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url: format!("http://{host}"),
            inner,
            server,
        }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.requests.lock().unwrap().clone()
    }

    /// Requests whose method and path match, e.g. `("POST", "/command")`
    /// matches every sandbox's `/command`.
    pub fn requests_to(&self, method: &str, path_end: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method && r.path.ends_with(path_end))
            .collect()
    }

    /// Ids of the sandboxes that exist now.
    pub fn live(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .inner
            .sandboxes
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    /// The states the next sandbox reports as it starts.
    pub fn start_as(&self, states: &[&str]) {
        *self.inner.startup.lock().unwrap() = states.iter().map(|s| s.to_string()).collect();
    }

    /// The sandbox expires or is deleted behind Athena's back.
    pub fn kill(&self, id: &str) {
        self.inner.sandboxes.lock().unwrap().remove(id);
    }

    /// execd restarts: every bash session and code context is gone.
    pub fn restart_execd(&self) {
        self.inner.bash.lock().unwrap().clear();
        self.inner.contexts.lock().unwrap().clear();
    }

    /// The next execution streams `body` instead of `ran: ...`.
    pub fn reply_next(&self, body: impl Into<String>) {
        self.inner
            .replies
            .lock()
            .unwrap()
            .push_back((StatusCode::OK, body.into()));
    }

    /// The next request whose path ends with `path_end` gets this answer.
    pub fn fail_next(&self, path_end: &str, status: u16, body: &str) {
        self.inner.faults.lock().unwrap().insert(
            path_end.to_string(),
            (StatusCode::from_u16(status).unwrap(), body.to_string()),
        );
    }

    /// What the endpoint lookup answers, instead of the proxy address.
    pub fn endpoint_answers(&self, body: Value) {
        *self.inner.endpoint.lock().unwrap() = Some(body);
    }

    /// Run `hook` while the next create request is being answered.
    pub fn on_create(&self, hook: impl FnOnce() + Send + 'static) {
        *self.inner.on_create.lock().unwrap() = Some(Box::new(hook));
    }

    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.inner.files.lock().unwrap().get(path).cloned()
    }

    pub fn put_file(&self, path: &str, content: &[u8]) {
        self.inner
            .files
            .lock()
            .unwrap()
            .insert(path.to_string(), content.to_vec());
    }
}

/// execd's stream: init, one stdout event, completion.
pub fn printed(text: &str) -> String {
    [
        json!({"type": "init", "text": "exec-1"}),
        json!({"type": "ping", "text": "pong"}),
        json!({"type": "stdout", "text": text}),
        json!({"type": "execution_complete", "execution_time": 1}),
    ]
    .iter()
    .map(|e| format!("{e}\n\n"))
    .collect()
}

fn reply(status: StatusCode, body: impl Into<Body>) -> Response {
    (status, body.into()).into_response()
}

fn json_reply(value: Value) -> Response {
    axum::Json(value).into_response()
}

fn not_found(what: &str) -> Response {
    reply(
        StatusCode::NOT_FOUND,
        json!({"code": "NOT_FOUND", "message": format!("{what} not found")}).to_string(),
    )
}

async fn handle(State(inner): State<Arc<Inner>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let headers = request.headers().clone();

    // Multipart bodies are read as a form; everything else as JSON.
    let (body, upload) = if path.ends_with("/files/upload") {
        (Value::Null, Some(read_upload(request).await))
    } else {
        let bytes = to_bytes(request.into_body(), usize::MAX).await.unwrap();
        (serde_json::from_slice(&bytes).unwrap_or(Value::Null), None)
    };
    inner.requests.lock().unwrap().push(Recorded {
        method: method.to_string(),
        path: path.clone(),
        query: query.clone(),
        body: body.clone(),
        headers: headers.clone(),
    });

    let fault = {
        let mut faults = inner.faults.lock().unwrap();
        let key = faults.keys().find(|k| path.ends_with(k.as_str())).cloned();
        key.and_then(|k| faults.remove(&k))
    };
    if let Some((status, body)) = fault {
        return reply(status, body);
    }

    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        (Method::POST, ["v1", "sandboxes"]) => create(&inner),
        (Method::GET, ["v1", "sandboxes", id]) => get(&inner, id),
        (Method::DELETE, ["v1", "sandboxes", id]) => {
            match inner.sandboxes.lock().unwrap().remove(*id) {
                Some(_) => reply(StatusCode::NO_CONTENT, ""),
                None => not_found("sandbox"),
            }
        }
        (Method::POST, ["v1", "sandboxes", id, "renew-expiration"]) => {
            if inner.sandboxes.lock().unwrap().contains_key(*id) {
                json_reply(json!({"expiresAt": body["expiresAt"]}))
            } else {
                not_found("sandbox")
            }
        }
        (Method::GET, ["v1", "sandboxes", id, "endpoints", port]) => {
            match inner.endpoint.lock().unwrap().clone() {
                Some(answer) => json_reply(answer),
                None => json_reply(json!({
                    "endpoint": format!("{}/v1/sandboxes/{id}/proxy/{port}", inner.host),
                    "headers": {}
                })),
            }
        }
        (method, ["v1", "sandboxes", id, "proxy", "44772", rest @ ..]) => {
            if !inner.sandboxes.lock().unwrap().contains_key(*id) {
                return reply(StatusCode::BAD_GATEWAY, "<html>no such sandbox</html>");
            }
            execd(&inner, method, rest, &body, &query, &headers, upload).await
        }
        _ => not_found("route"),
    }
}

fn create(inner: &Inner) -> Response {
    if let Some(hook) = inner.on_create.lock().unwrap().take() {
        hook();
    }
    let id = {
        let mut next = inner.next.lock().unwrap();
        *next += 1;
        format!("sbx-{next}")
    };
    let state = inner.startup.lock().unwrap()[0].clone();
    inner
        .sandboxes
        .lock()
        .unwrap()
        .insert(id.clone(), state.clone());
    json_reply(json!({
        "id": id,
        "status": {"state": state},
        "expiresAt": "2030-01-01T00:00:00Z"
    }))
}

fn get(inner: &Inner, id: &str) -> Response {
    let mut sandboxes = inner.sandboxes.lock().unwrap();
    let Some(state) = sandboxes.get_mut(id) else {
        return not_found("sandbox");
    };
    // Move one step along the startup script.
    let mut startup = inner.startup.lock().unwrap();
    if startup.len() > 1 {
        startup.remove(0);
    }
    *state = startup[0].clone();
    json_reply(json!({"id": id, "status": {"state": state.clone()}}))
}

async fn read_upload(request: Request) -> (String, Vec<u8>) {
    let mut form = Multipart::from_request(request, &()).await.unwrap();
    let (mut path, mut content) = (String::new(), Vec::new());
    while let Some(field) = form.next_field().await.unwrap() {
        match field.name().unwrap() {
            "metadata" => {
                // As the real execd: metadata must be a file part.
                assert_eq!(field.file_name(), Some("metadata.json"));
                let meta: Value = serde_json::from_str(&field.text().await.unwrap()).unwrap();
                path = meta["path"].as_str().unwrap().to_string();
            }
            _ => content = field.bytes().await.unwrap().to_vec(),
        }
    }
    (path, content)
}

async fn execd(
    inner: &Inner,
    method: Method,
    rest: &[&str],
    body: &Value,
    query: &str,
    headers: &HeaderMap,
    upload: Option<(String, Vec<u8>)>,
) -> Response {
    let stream = |what: String| {
        let (status, text) = inner
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| (StatusCode::OK, printed(&format!("ran: {what}\n"))));
        reply(status, text)
    };
    match (method, rest) {
        (Method::POST, ["command"]) => stream(body["command"].as_str().unwrap().to_string()),
        (Method::POST, ["session"]) => {
            let id = format!("bash-{}", uuid::Uuid::new_v4().simple());
            inner.bash.lock().unwrap().insert(id.clone());
            json_reply(json!({"session_id": id}))
        }
        (Method::POST, ["session", id, "run"]) => {
            if !inner.bash.lock().unwrap().contains(*id) {
                // What execd 1.x does: a runtime error, not a 404.
                return reply(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"code": "RUNTIME_ERROR", "message": format!("session {id} not found")})
                        .to_string(),
                );
            }
            stream(body["command"].as_str().unwrap().to_string())
        }
        (Method::POST, ["code", "context"]) => {
            let id = format!("ctx-{}", uuid::Uuid::new_v4().simple());
            inner.contexts.lock().unwrap().insert(id.clone());
            json_reply(json!({"id": id, "language": body["language"]}))
        }
        (Method::POST, ["code"]) => {
            let id = body["context"]["id"].as_str().unwrap();
            if !inner.contexts.lock().unwrap().contains(id) {
                return not_found("context");
            }
            stream(body["code"].as_str().unwrap().to_string())
        }
        (Method::POST, ["files", "upload"]) => {
            let (path, content) = upload.unwrap();
            inner.files.lock().unwrap().insert(path, content);
            reply(StatusCode::OK, "")
        }
        (Method::GET, ["files", "download"]) => {
            let path = url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == "path")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            match inner.files.lock().unwrap().get(&path) {
                None => not_found("file"),
                Some(content) if content.is_empty() => reply(StatusCode::RANGE_NOT_SATISFIABLE, ""),
                Some(content) => {
                    // `bytes=0-N`: the first N + 1 bytes.
                    let end = headers["range"].to_str().unwrap();
                    let end: usize = end.strip_prefix("bytes=0-").unwrap().parse().unwrap();
                    let part = content[..content.len().min(end + 1)].to_vec();
                    reply(StatusCode::PARTIAL_CONTENT, part)
                }
            }
        }
        _ => not_found("execd route"),
    }
}
