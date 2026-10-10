//! A fake Google for tests: the token, revoke and Health API endpoints on one
//! loopback port. It records each request and answers from a script, or with
//! an empty success. Nothing here sleeps or reads the clock.
use super::client::Endpoints;
use super::sync::Health;
use super::{Config, config};
use crate::scheduler::Clock;
use crate::store::Store;
use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use jiff::Timestamp;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub const CLIENT_ID: &str = "test-client-id";
pub const CLIENT_SECRET: &str = "test-client-secret";

/// A key of 32 bytes in the form a Fernet key has.
pub fn key() -> String {
    URL_SAFE.encode([7u8; 32])
}

pub fn test_config() -> Config {
    config(
        Some(CLIENT_ID.into()),
        Some(CLIENT_SECRET.into()),
        Some(key()),
        None,
    )
    .unwrap()
    .unwrap()
}

/// A request as the fake saw it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub form: HashMap<String, String>,
    pub authorization: Option<String>,
}

type Hook = Arc<dyn Fn(&Seen) + Send + Sync>;
type Script = HashMap<String, VecDeque<(u16, String)>>;

#[derive(Default)]
struct State {
    seen: Mutex<Vec<Seen>>,
    script: Mutex<Script>,
    /// Run when a request arrives, before it is answered.
    hook: Mutex<Option<Hook>>,
}

pub struct FakeGoogle {
    pub url: String,
    state: Arc<State>,
}

/// The default token answer.
pub fn token_body(access: &str, refresh: Option<&str>) -> String {
    let mut body = serde_json::json!({
        "access_token": access,
        "expires_in": 3600,
        "scope": "https://www.googleapis.com/auth/googlehealth.sleep.readonly",
    });
    if let Some(refresh) = refresh {
        body["refresh_token"] = refresh.into();
    }
    body.to_string()
}

impl FakeGoogle {
    pub async fn start() -> Self {
        let state = Arc::new(State::default());
        let shared = state.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let state = shared.clone();
                async move { answer(&state, method, uri, headers, body) }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, state }
    }

    pub fn endpoints(&self) -> Endpoints {
        Endpoints::at(&self.url)
    }

    /// Answer the next request for `key` (`token`, `revoke`, or a data type
    /// such as `steps`) with `status` and `body`. Requests with nothing
    /// scripted get `200 {}`; `token` gets a token for `at-1` and `rt-1`.
    pub fn answer(&self, key: &str, status: u16, body: &str) {
        let mut script = self.state.script.lock().unwrap();
        script
            .entry(key.into())
            .or_default()
            .push_back((status, body.into()));
    }

    /// Run `hook` on each request as it arrives, before the answer: how a
    /// test changes the world in the middle of a sync, deterministically.
    pub fn on_request(&self, hook: impl Fn(&Seen) + Send + Sync + 'static) {
        *self.state.hook.lock().unwrap() = Some(Arc::new(hook));
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }

    /// The requests whose path ends with `suffix`.
    pub fn seen_at(&self, suffix: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.path.ends_with(suffix))
            .collect()
    }
}

fn answer(
    state: &State,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, String) {
    let pairs = |raw: &[u8]| -> HashMap<String, String> {
        url::form_urlencoded::parse(raw)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    };
    let path = uri.path().to_string();
    let request = Seen {
        method: method.to_string(),
        path: path.clone(),
        query: pairs(uri.query().unwrap_or_default().as_bytes()),
        form: pairs(&body),
        authorization: headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string()),
    };
    state.seen.lock().unwrap().push(request.clone());
    let hook = state.hook.lock().unwrap().clone();
    if let Some(hook) = hook {
        hook(&request);
    }
    let key = match path.as_str() {
        "/token" => "token".to_string(),
        "/revoke" => "revoke".to_string(),
        other => other.split('/').nth(5).unwrap_or_default().to_string(),
    };
    let scripted = state
        .script
        .lock()
        .unwrap()
        .get_mut(&key)
        .and_then(VecDeque::pop_front);
    let (status, body) = scripted.unwrap_or_else(|| match key.as_str() {
        "token" => (200, token_body("at-1", Some("rt-1"))),
        _ => (200, "{}".into()),
    });
    let mut out = HeaderMap::new();
    // A 3xx answers with a `Location`, to show it is not followed.
    if (300..400).contains(&status) {
        out.insert(header::LOCATION, HeaderValue::from_static("/elsewhere"));
    }
    (StatusCode::from_u16(status).unwrap(), out, body)
}

/// A clock a test sets.
pub struct SetClock(Mutex<Timestamp>);

impl SetClock {
    pub fn at(time: &str) -> Arc<Self> {
        Arc::new(Self(Mutex::new(time.parse().unwrap())))
    }

    pub fn set(&self, time: &str) {
        *self.0.lock().unwrap() = time.parse().unwrap();
    }
}

impl Clock for SetClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}

/// A [`Health`] on `store` against `fake`, reading `clock`.
pub fn health(store: &Store, fake: &FakeGoogle, clock: Arc<SetClock>) -> Arc<Health> {
    Arc::new(Health::new(test_config(), store.clone(), fake.endpoints()).clock(clock))
}
