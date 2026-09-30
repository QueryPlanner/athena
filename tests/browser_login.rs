//! Signing in through a session's browser: the link, the sign-in page's
//! endpoints and the saved state, through the real HTTP router against the
//! fake OpenSandbox (`sandbox/fake_server.rs`) and a real database.

mod common;
// Only some of the fake sandbox is needed here.
#[allow(dead_code)]
#[path = "sandbox/fake_server.rs"]
mod fake_server;

use athena::http::{self, Hosts};
use athena::sandbox::Sandboxes;
use athena::sandbox::login::{MAX_SCREEN_BYTES, MAX_STATE_BYTES, SCREEN_PATH, STATE_PATH};
use athena::sandbox::tools::cli_command;
use athena::service::{Service, User};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use fake_server::{FakeSandbox, SCREENSHOT, config, printed};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const STATE: &[u8] = br#"{"cookies":[{"name":"sid","value":"abc"}],"origins":[]}"#;
const LINK_PREFIX: &str = "http://athena.test:18080/browser/";

struct Harness {
    fake: FakeSandbox,
    tmp: TempDb,
    service: Arc<Service>,
    sandboxes: Arc<Sandboxes>,
    user: User,
}

async fn harness() -> Harness {
    let fake = FakeSandbox::start().await;
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let sandboxes = Arc::new(Sandboxes::new(config(&fake.url), tmp.open()));
    let user = service.user("telegram", "7").await.unwrap();
    Harness {
        fake,
        tmp,
        service: Arc::new(service),
        sandboxes,
        user,
    }
}

impl Harness {
    async fn session(&self, name: &str) -> String {
        session(&self.service, &self.user, name).await.id
    }

    fn router(&self, hosts: Hosts) -> Router {
        let (agent, _) = mock_agent(&self.service, []);
        http::router_with(
            self.service.clone(),
            Arc::new(agent),
            hosts,
            Some(self.sandboxes.clone()),
        )
    }

    /// A session with a sign-in link to it; returns the session and token.
    async fn linked(&self, name: &str) -> (String, String) {
        let s = self.session(name).await;
        let link = self
            .sandboxes
            .login_link(&s, "https://shop.example/login")
            .await
            .unwrap();
        let token = link.strip_prefix(LINK_PREFIX).unwrap().to_string();
        (s, token)
    }

    /// Every command line run in any sandbox, in order.
    fn commands(&self) -> Vec<String> {
        self.fake
            .requests_to("POST", "/command")
            .iter()
            .map(|r| r.body["command"].as_str().unwrap().to_string())
            .collect()
    }

    fn last_command(&self) -> String {
        self.commands().pop().unwrap()
    }
}

/// The command line that runs agent-browser with `args` on `session`.
fn browser(session: &str, args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    cli_command(session, &args).unwrap()
}

/// execd's stream for a command that failed.
fn failed(evalue: &str) -> String {
    let error = json!({"type": "error", "error": {"ename": "CommandExecError", "evalue": evalue}});
    format!("{error}\n\n")
}

fn request(method: &str, uri: &str, host: &str, body: Option<Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

/// Status, headers and body bytes.
async fn send(router: &Router, request: Request<Body>) -> (StatusCode, header::HeaderMap, Vec<u8>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    (parts.status, parts.headers, bytes)
}

async fn post(router: &Router, token: &str, action: &str, body: Value) -> (StatusCode, Value) {
    let uri = format!("/browser/{token}/{action}");
    let (status, _, bytes) = send(router, request("POST", &uri, "localhost", Some(body))).await;
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn a_link_opens_its_start_page_and_serves_the_sign_in_page() {
    let h = harness().await;
    let (s, token) = h.linked("shop").await;
    assert_eq!(token.len(), 32);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
    // No saved state yet, so nothing is loaded: the page is opened.
    assert_eq!(
        h.commands(),
        [browser(&s, &["open", "https://shop.example/login"])]
    );

    let router = h.router(Hosts::Loopback);
    let (status, headers, body) = send(
        &router,
        request("GET", &format!("/browser/{token}"), "localhost", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let page = String::from_utf8(body).unwrap();
    assert!(page.contains("<title>Sign in for Athena</title>"), "{page}");
}

#[tokio::test]
async fn unknown_expired_and_misaddressed_links_open_nothing() {
    let h = harness().await;
    let (_, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);
    let before = h.commands().len();

    let (status, body) = post(&router, "0123456789abcdef", "start", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ask Athena for a new one")
    );

    // A web page pointing its own name at this server gets nothing.
    let uri = format!("/browser/{token}");
    let (status, _, _) = send(&router, request("GET", &uri, "evil.example", None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A server without sandboxes has no sign-in pages.
    let (agent, _) = mock_agent(&h.service, []);
    let bare = http::router(h.service.clone(), Arc::new(agent), Hosts::Loopback);
    let (status, _, _) = send(&bare, request("GET", &uri, "localhost", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    h.tmp
        .raw()
        .execute("UPDATE browser_links SET expires_at = 0", [])
        .unwrap();
    let (status, _) = post(&router, &token, "start", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.commands().len(), before);
}

#[tokio::test]
async fn start_reopens_the_start_page_only_when_the_browser_shows_nothing() {
    let h = harness().await;
    let (s, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);
    let get_url = browser(&s, &["get", "url"]);
    let open = browser(&s, &["open", "https://shop.example/login"]);

    // The sandbox expired and the new one's browser shows nothing.
    h.fake
        .reply_next(printed("[agent-browser] launched browser\nabout:blank\n"));
    let (status, body) = post(&router, &token, "start", json!({})).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"ok": true})));
    assert_eq!(h.commands()[1..], [get_url.clone(), open.clone()]);

    // Showing a page: left as it is.
    h.fake.reply_next(printed("https://shop.example/login\n"));
    let (status, _) = post(&router, &token, "start", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.commands()[3..], [get_url]);

    h.fake.reply_next(failed("exit status 1"));
    let (status, body) = post(&router, &token, "start", json!({})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"]["code"], "sandbox");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("get url: CommandExecError: exit status 1")
    );
}

#[tokio::test]
async fn the_screen_is_a_fresh_png_of_the_browser_or_an_error() {
    let h = harness().await;
    let (s, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);
    let uri = format!("/browser/{token}/screen");
    h.fake.writes_on("'screenshot' ", Some(SCREENSHOT));

    let (status, headers, body) = send(&router, request("GET", &uri, "localhost", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(body, SCREENSHOT);
    assert_eq!(
        h.last_command(),
        format!(
            "rm -f {SCREEN_PATH} && {}",
            browser(&s, &["screenshot", SCREEN_PATH])
        )
    );

    // agent-browser saved nothing: the last frame was removed first, so it
    // is not served again as if it were new.
    h.fake.writes_on("'screenshot' ", None);
    let (status, _, _) = send(&router, request("GET", &uri, "localhost", None)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(h.fake.file(SCREEN_PATH), None);

    h.fake.reply_next(failed("no browser"));
    let (status, _, body) = send(&router, request("GET", &uri, "localhost", None)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("screenshot: CommandExecError: no browser")
    );

    let huge = vec![0u8; MAX_SCREEN_BYTES + 1];
    h.fake.writes_on("'screenshot' ", Some(&huge));
    let (status, _, body) = send(&router, request("GET", &uri, "localhost", None)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("the screenshot is over")
    );
}

#[tokio::test]
async fn taps_typing_keys_scrolls_and_urls_reach_the_sessions_browser() {
    let h = harness().await;
    let (s, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);
    for (action, body, steps) in [
        (
            "click",
            json!({"x": 10, "y": 20}),
            vec![
                browser(&s, &["mouse", "move", "10", "20"]),
                browser(&s, &["mouse", "down"]),
                browser(&s, &["mouse", "up"]),
            ],
        ),
        (
            "type",
            json!({"text": "-p4ss 'word' $(id)"}),
            vec![browser(&s, &["keyboard", "type", "-p4ss 'word' $(id)"])],
        ),
        (
            "press",
            json!({"key": "Enter"}),
            vec![browser(&s, &["press", "Enter"])],
        ),
        (
            "scroll",
            json!({"direction": "down"}),
            vec![browser(&s, &["scroll", "down", "400"])],
        ),
        (
            "open",
            json!({"url": "https://x.example"}),
            vec![browser(&s, &["open", "https://x.example/"])],
        ),
    ] {
        let (status, reply) = post(&router, &token, action, body).await;
        assert_eq!(
            (status, reply),
            (StatusCode::OK, json!({"ok": true})),
            "{action}"
        );
        assert_eq!(h.last_command(), steps.join(" && "), "{action}");
    }
    // The chain is plain shell text the sandbox runs as one command.
    assert!(h.commands()[1].starts_with(&format!(
        "'agent-browser' '--session' '{s}' '--content-boundaries' 'mouse' 'move' '10' '20' && "
    )));

    h.fake.reply_next(failed("element not focusable"));
    let (status, body) = post(&router, &token, "type", json!({"text": "x"})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("element not focusable")
    );
}

#[tokio::test]
async fn bad_input_is_refused_before_anything_runs() {
    let h = harness().await;
    let (_, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);
    let before = h.commands().len();
    let long = "x".repeat(1001);
    for (action, body) in [
        ("click", json!({"x": -1, "y": 0})),
        ("click", json!({"x": 1, "y": 10_001})),
        ("click", json!({"x": "1", "y": 1})),
        ("type", json!({"text": ""})),
        ("type", json!({"text": long})),
        ("type", json!("not an object")),
        ("press", json!({"key": "a b"})),
        ("press", json!({"key": "Enter; reboot"})),
        ("scroll", json!({"direction": "sideways"})),
        ("open", json!({"url": "file:///etc/passwd"})),
        ("open", json!({"url": "javascript:alert(1)"})),
    ] {
        let (status, reply) = post(&router, &token, action, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{action} {body}");
        assert_eq!(reply["error"]["code"], "invalid", "{action} {body}");
    }
    let uri = format!("/browser/{token}/click");
    let raw = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "localhost")
        .body(Body::from("not json"))
        .unwrap();
    let (status, _, _) = send(&router, raw).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(h.commands().len(), before);
}

#[tokio::test]
async fn done_saves_the_sign_ins_for_every_new_sandbox_of_the_user_only() {
    let h = harness().await;
    let (s, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);
    h.fake.writes_on("'state' 'save' ", Some(STATE));

    let (status, body) = post(&router, &token, "done", json!({})).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({"saved_bytes": STATE.len()}))
    );
    assert_eq!(
        h.last_command(),
        browser(&s, &["state", "save", STATE_PATH])
    );
    assert_eq!(h.tmp.open().browser_state(&s).unwrap().unwrap(), STATE);

    // Another conversation of the same user: its new sandbox gets the
    // state before its first command runs.
    let other = h.session("mail").await;
    let uploads = h.fake.requests_to("POST", "/files/upload").len();
    h.sandboxes
        .command(&other, "echo hi", std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        h.fake.requests_to("POST", "/files/upload").len(),
        uploads + 1
    );
    assert_eq!(h.fake.file(STATE_PATH).unwrap(), STATE);
    let n = h.commands().len();
    assert_eq!(
        h.commands()[n - 2..],
        [
            browser(&other, &["state", "load", STATE_PATH]),
            "echo hi".to_string()
        ]
    );

    // Someone else's conversation gets nothing.
    let stranger = h.service.user("telegram", "8").await.unwrap();
    let theirs = session(&h.service, &stranger, "shop").await.id;
    h.sandboxes
        .command(&theirs, "echo hi", std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(h.last_command(), "echo hi");
    assert_eq!(h.commands().len(), n + 1);
    assert_eq!(
        h.fake.requests_to("POST", "/files/upload").len(),
        uploads + 1
    );
}

#[tokio::test]
async fn a_state_that_was_not_saved_or_is_too_large_is_not_kept() {
    let h = harness().await;
    let (s, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);

    h.fake.reply_next(failed("exit status 1"));
    let (status, body) = post(&router, &token, "done", json!({})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("state save: CommandExecError: exit status 1")
    );

    let huge = vec![b'x'; MAX_STATE_BYTES + 1];
    h.fake.writes_on("'state' 'save' ", Some(&huge));
    let (status, body) = post(&router, &token, "done", json!({})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not saved")
    );
    assert_eq!(h.tmp.open().browser_state(&s).unwrap(), None);
}

#[tokio::test]
async fn done_after_the_sandbox_was_replaced_saves_nothing_and_says_so() {
    let h = harness().await;
    let (s, token) = h.linked("shop").await;
    let router = h.router(Hosts::Loopback);
    h.tmp.open().save_browser_state(&s, STATE, 1).unwrap();
    h.fake
        .writes_on("'state' 'save' ", Some(b"the old state, reloaded"));
    // The sandbox the user signed in to expired before they pressed Done.
    for id in h.fake.live() {
        h.fake.kill(&id);
    }

    let (status, body) = post(&router, &token, "done", json!({})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("the sign-in was lost; ask Athena for a new link")
    );
    assert_eq!(h.tmp.open().browser_state(&s).unwrap().unwrap(), STATE);
    // The new sandbox loaded the saved state, and nothing was saved over it.
    assert_eq!(
        h.last_command(),
        browser(&s, &["state", "load", STATE_PATH])
    );
}

#[tokio::test]
async fn a_link_to_a_new_sandbox_loads_the_saved_state_once() {
    let h = harness().await;
    let s = h.session("shop").await;
    h.tmp.open().save_browser_state(&s, STATE, 1).unwrap();
    h.sandboxes
        .login_link(&s, "https://mail.example/")
        .await
        .unwrap();
    assert_eq!(
        h.commands(),
        [
            browser(&s, &["state", "load", STATE_PATH]),
            browser(&s, &["open", "https://mail.example/"]),
        ]
    );
}

#[tokio::test]
async fn a_sandbox_that_cannot_restore_sign_ins_is_still_used() {
    let h = harness().await;
    let s = h.session("shop").await;
    h.tmp.open().save_browser_state(&s, STATE, 1).unwrap();

    h.fake.fail_next("/files/upload", 500, "disk full");
    let out = h
        .sandboxes
        .command(&s, "echo hi", std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.stdout, "ran: echo hi\n");
    assert_eq!(h.commands(), ["echo hi"]);

    // A new sandbox whose browser refuses the state: still used.
    for id in h.fake.live() {
        h.fake.kill(&id);
    }
    h.fake.reply_next(failed("bad state file"));
    let out = h
        .sandboxes
        .command(&s, "echo again", std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.stdout, "ran: echo again\n");
    assert_eq!(
        h.commands()[1..],
        [
            browser(&s, &["state", "load", STATE_PATH]),
            "echo again".to_string()
        ]
    );
}

#[tokio::test]
async fn a_new_link_loads_the_latest_saved_state_before_opening_its_page() {
    let h = harness().await;
    let s = h.session("shop").await;
    // The sandbox exists before anything was saved.
    h.sandboxes
        .command(&s, "true", std::time::Duration::from_secs(5))
        .await
        .unwrap();
    // Another conversation saved sign-ins meanwhile.
    h.tmp.open().save_browser_state(&s, STATE, 1).unwrap();

    h.sandboxes
        .login_link(&s, "https://mail.example/")
        .await
        .unwrap();
    assert_eq!(
        h.commands()[1..],
        [
            browser(&s, &["state", "load", STATE_PATH]),
            browser(&s, &["open", "https://mail.example/"]),
        ]
    );
    assert_eq!(h.fake.file(STATE_PATH).unwrap(), STATE);

    // A page that does not open gives no link.
    h.fake.reply_next(printed("loaded"));
    h.fake.reply_next(failed("net::ERR_NAME_NOT_RESOLVED"));
    let err = h
        .sandboxes
        .login_link(&s, "https://nowhere.example/")
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("open: CommandExecError: net::ERR_NAME_NOT_RESOLVED"),
        "{err}"
    );
    let links: i64 = h
        .tmp
        .raw()
        .query_row("SELECT COUNT(*) FROM browser_links", [], |r| r.get(0))
        .unwrap();
    assert_eq!(links, 1);
}
