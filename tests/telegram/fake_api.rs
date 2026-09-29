//! A fake Telegram Bot API server: enough of it for teloxide's long polling
//! and the calls athena makes, with every call recorded.
//!
//! Tests queue updates with [`FakeApi::push`], wait for calls with
//! [`FakeApi::wait_for`] (a condition, never a sleep), and can make the next
//! call to a method fail with [`FakeApi::fail_next`]. Files users "sent" are
//! registered with [`FakeApi::host_file`] and served as the Bot API does:
//! `getFile`, then `GET /file/bot<token>/<file_path>`. Uploads
//! (`sendPhoto`, `sendDocument`) are multipart; each file part is recorded
//! as `{"file_name": ..., "bytes": [...]}`.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, watch};

/// One call a client made: the method and its JSON parameters.
#[derive(Debug, Clone)]
pub struct Call {
    pub token: String,
    pub method: String,
    pub body: Value,
}

impl Call {
    pub fn chat_id(&self) -> i64 {
        self.body["chat_id"].as_i64().unwrap()
    }

    pub fn text(&self) -> &str {
        self.body["text"].as_str().unwrap()
    }
}

#[derive(Default)]
struct Inner {
    /// Updates not yet confirmed by a `getUpdates` with a higher offset.
    updates: Mutex<Vec<Value>>,
    next_update: Mutex<i64>,
    arrived: Notify,
    calls: Mutex<Vec<Call>>,
    faults: Mutex<HashMap<String, VecDeque<Value>>>,
    changed: Option<watch::Sender<usize>>,
    next_message: Mutex<i64>,
    /// file_id -> (file_path, content).
    files: Mutex<HashMap<String, (String, Vec<u8>)>>,
}

pub struct FakeApi {
    pub url: String,
    inner: Arc<Inner>,
    seen: watch::Receiver<usize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeApi {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeApi {
    pub async fn start() -> Self {
        let (changed, seen) = watch::channel(0);
        let inner = Arc::new(Inner {
            changed: Some(changed),
            next_update: Mutex::new(1),
            ..Default::default()
        });
        let app = Router::new()
            .route("/{token}/{method}", post(handle))
            .route("/file/{token}/{path}", get(download))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            inner,
            seen,
            server,
        }
    }

    /// Queue an update; its `update_id` is assigned here.
    pub fn push(&self, mut update: Value) {
        let mut next = self.inner.next_update.lock().unwrap();
        update["update_id"] = json!(*next);
        *next += 1;
        self.inner.updates.lock().unwrap().push(update);
        self.inner.arrived.notify_waiters();
    }

    /// Answer the next call to `method` with this error instead.
    pub fn fail_next(&self, method: &str, error: Value) {
        self.inner
            .faults
            .lock()
            .unwrap()
            .entry(method.to_string())
            .or_default()
            .push_back(error);
    }

    /// A file a user sent, as Telegram stores it: `getFile` for `id` gives
    /// `path`, and downloading `path` gives `content`.
    pub fn host_file(&self, id: &str, path: &str, content: &[u8]) {
        self.inner
            .files
            .lock()
            .unwrap()
            .insert(id.into(), (path.into(), content.to_vec()));
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.calls.lock().unwrap().clone()
    }

    /// Calls to one method, in order.
    pub fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.method == method)
            .collect()
    }

    /// Wait until `done` holds for the calls so far. The timeout only turns
    /// a hang into a failure.
    pub async fn wait_for(&self, what: &str, done: impl Fn(&[Call]) -> bool) {
        let mut seen = self.seen.clone();
        let inner = self.inner.clone();
        let waited = tokio::time::timeout(
            Duration::from_secs(30),
            seen.wait_for(|_| done(&inner.calls.lock().unwrap())),
        )
        .await;
        assert!(
            waited.is_ok(),
            "timed out waiting for {what}: {:#?}",
            self.calls()
        );
    }

    /// Wait for `n` messages sent to `chat`, and return their texts.
    pub async fn messages_to(&self, chat: i64, n: usize) -> Vec<String> {
        let sent_to = |calls: &[Call]| {
            calls
                .iter()
                .filter(|c| c.method == "sendMessage" && c.chat_id() == chat)
                .map(|c| c.text().to_string())
                .collect::<Vec<_>>()
        };
        self.wait_for(&format!("{n} messages to {chat}"), |calls| {
            sent_to(calls).len() >= n
        })
        .await;
        sent_to(&self.calls())
    }
}

/// A private text message from `user`, as Telegram delivers it.
pub fn text_from(user: u64, text: &str) -> Value {
    json!({
        "message": {
            "message_id": 1,
            "date": 1_790_000_000,
            "chat": {"id": user, "type": "private", "first_name": "Tester"},
            "from": {"id": user, "is_bot": false, "first_name": "Tester"},
            "text": text,
        }
    })
}

/// A private message from `user` carrying `media` (e.g. `"photo"` and its
/// sizes), with `caption`.
pub fn media_from(user: u64, media: (&str, Value), caption: Option<&str>) -> Value {
    let mut update = text_from(user, "");
    let message = update["message"].as_object_mut().unwrap();
    message.remove("text");
    message.insert(media.0.into(), media.1);
    if let Some(caption) = caption {
        message.insert("caption".into(), json!(caption));
    }
    update
}

/// A photo's sizes as Telegram lists them, smallest first: `id` is the
/// largest.
pub fn photo_sizes(id: &str, size: u64) -> Value {
    json!([
        {"file_id": "thumb", "file_unique_id": "t", "width": 90, "height": 60, "file_size": 900},
        {"file_id": id, "file_unique_id": "u", "width": 1280, "height": 853, "file_size": size},
        {"file_id": "mid", "file_unique_id": "m", "width": 320, "height": 213, "file_size": 9000},
    ])
}

pub fn bot_user() -> Value {
    json!({"id": 4242, "is_bot": true, "first_name": "athena", "username": "athena_test_bot"})
}

/// A multipart body as JSON: text parts as strings, file parts as their
/// name and bytes.
async fn form(request: Request) -> Value {
    let mut form = Multipart::from_request(request, &()).await.unwrap();
    let mut body = serde_json::Map::new();
    while let Some(field) = form.next_field().await.unwrap() {
        let name = field.name().unwrap().to_string();
        let value = match field.file_name().map(str::to_string) {
            Some(file_name) => {
                json!({"file_name": file_name, "bytes": field.bytes().await.unwrap().to_vec()})
            }
            None => {
                let text = field.text().await.unwrap();
                serde_json::from_str(&text).unwrap_or(json!(text))
            }
        };
        body.insert(name, value);
    }
    // A field may name another part as `attach://<part>`, as the Bot API
    // allows and teloxide does: put the part in its place.
    let references: Vec<(String, String)> = body
        .iter()
        .filter_map(|(k, v)| {
            Some((
                k.clone(),
                v.as_str()?.strip_prefix("attach://")?.to_string(),
            ))
        })
        .collect();
    for (field, part) in references {
        let file = body.remove(&part).unwrap();
        body.insert(field, file);
    }
    Value::Object(body)
}

async fn download(
    State(inner): State<Arc<Inner>>,
    Path((_token, path)): Path<(String, String)>,
) -> Response {
    let files = inner.files.lock().unwrap();
    match files.values().find(|(p, _)| *p == path) {
        Some((_, content)) => content.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn handle(
    State(inner): State<Arc<Inner>>,
    Path((token, method)): Path<(String, String)>,
    request: Request,
) -> Json<Value> {
    // Bot API method names are case-insensitive and teloxide sends
    // `SendMessage`; record them as the docs spell them, `sendMessage`.
    let method = method[..1].to_lowercase() + &method[1..];
    let multipart = request
        .headers()
        .get("content-type")
        .is_some_and(|t| t.to_str().unwrap().starts_with("multipart/"));
    let body: Value = if multipart {
        form(request).await
    } else {
        let bytes = Bytes::from_request(request, &()).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    let call = Call {
        token,
        method: method.clone(),
        body: body.clone(),
    };
    inner.calls.lock().unwrap().push(call);
    inner.changed.as_ref().unwrap().send_modify(|n| *n += 1);

    let fault = inner
        .faults
        .lock()
        .unwrap()
        .get_mut(&method)
        .and_then(VecDeque::pop_front);
    if let Some(error) = fault {
        return Json(error);
    }
    let result = match method.as_str() {
        "getMe" => {
            let mut me = bot_user();
            me["can_join_groups"] = json!(false);
            me["can_read_all_group_messages"] = json!(false);
            me["supports_inline_queries"] = json!(false);
            me["has_main_web_app"] = json!(false);
            me
        }
        "getWebhookInfo" => {
            json!({"url": "", "has_custom_certificate": false, "pending_update_count": 0})
        }
        "getUpdates" => get_updates(&inner, &body).await,
        "sendMessage" => {
            let mut sent = sent_message(&inner, &body);
            sent["text"] = body["text"].clone();
            sent
        }
        "sendPhoto" => {
            let mut sent = sent_message(&inner, &body);
            sent["photo"] = photo_sizes("sent", 1);
            sent
        }
        "sendDocument" => {
            let mut sent = sent_message(&inner, &body);
            sent["document"] = json!({"file_id": "sent", "file_unique_id": "s"});
            sent
        }
        "getFile" => {
            let id = body["file_id"].as_str().unwrap();
            let files = inner.files.lock().unwrap();
            let Some((path, content)) = files.get(id) else {
                return Json(json!({
                    "ok": false,
                    "error_code": 400,
                    "description": "Bad Request: invalid file_id"
                }));
            };
            json!({
                "file_id": id,
                "file_unique_id": "u",
                "file_size": content.len(),
                "file_path": path,
            })
        }
        _ => json!(true),
    };
    Json(json!({"ok": true, "result": result}))
}

/// The message a send method answers with, before its content.
fn sent_message(inner: &Inner, body: &Value) -> Value {
    let mut id = inner.next_message.lock().unwrap();
    *id += 1;
    json!({
        "message_id": *id,
        "date": 1_790_000_000,
        "chat": {"id": body["chat_id"], "type": "private", "first_name": "Tester"},
        "from": bot_user(),
    })
}

/// Updates at or after `offset`, waiting up to `timeout` seconds for one.
async fn get_updates(inner: &Inner, body: &Value) -> Value {
    let offset = body["offset"].as_i64().unwrap_or(0);
    let timeout = body["timeout"].as_u64().unwrap_or(0);
    // Asking from `offset` confirms every earlier update: Telegram never
    // sends it again, even to a new process.
    inner
        .updates
        .lock()
        .unwrap()
        .retain(|u| u["update_id"].as_i64().unwrap() >= offset);
    let pending = || inner.updates.lock().unwrap().clone();
    let wait = async {
        loop {
            let arrived = inner.arrived.notified();
            let now = pending();
            if !now.is_empty() {
                return now;
            }
            arrived.await;
        }
    };
    tokio::time::timeout(Duration::from_secs(timeout), wait)
        .await
        .map(Value::from)
        .unwrap_or_else(|_| json!([]))
}
