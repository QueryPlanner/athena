//! The Telegram transport end to end against a fake Bot API server
//! (`telegram/fake_api.rs`): in-process with the mock model, and as the real
//! `athena telegram` binary for everything that needs no model.

mod common;
#[path = "telegram/fake_api.rs"]
mod fake_api;
// Only some of the fake sandbox is needed here.
#[allow(dead_code)]
#[path = "sandbox/fake_server.rs"]
mod fake_server;

use athena::agent;
use athena::compaction::ContextHook;
use athena::http::{self, Hosts, USER_HEADER};
use athena::media;
use athena::runner::{Request, Run};
use athena::sandbox::Sandboxes;
use athena::service::Service;
use athena::telegram::{self, Log, Telegram};
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::*;
use fake_api::{FakeApi, media_from, photo_sizes, text_from};
use fake_server::{FakeSandbox, SCREENSHOT};
use http_body_util::BodyExt;
use rig_agent::agent::AgentBuilder;
use rig_agent::agent::{Agent, PromptResponse};
use rig_agent::completion::PromptError;
use rig_core::test_utils::MockCompletionModel;
use rig_core::test_utils::MockTurn;
use serde_json::json;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use teloxide::Bot;
use tokio::sync::{Semaphore, mpsc};
use tower::ServiceExt;

/// The secret half of the test token: 35+ characters of `[A-Za-z0-9_-]`,
/// which is what teloxide's redaction looks for. Built at runtime and
/// obviously fake, so the source never holds a token-shaped literal for
/// secret scanners to flag.
fn secret() -> String {
    format!("not_a_real_secret_{}", "x".repeat(18))
}

/// Shaped like a real token, so teloxide's redaction applies to it.
fn token() -> String {
    format!("123456789:{}", secret())
}

type Logged = Arc<Mutex<Vec<String>>>;

fn bot(url: &str) -> Bot {
    Bot::new(token()).set_api_url(url::Url::parse(url).unwrap())
}

fn app<R: Run + 'static>(
    tmp: &TempDb,
    make: impl FnOnce(&Service) -> R,
) -> (Arc<Telegram<R>>, Logged) {
    let store = tmp.open();
    let service = Service::new(store.clone(), "m", |_| {});
    let agent = make(&service);
    let logged = Logged::default();
    let sink = logged.clone();
    let log: Log = Arc::new(move |m| sink.lock().unwrap().push(m.to_string()));
    (
        Arc::new(Telegram::new(Arc::new(service), store, agent, log)),
        logged,
    )
}

type Settle = Box<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>;

/// A running bot: `serve` in a task, stopped through its shutdown token.
struct Running {
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    stop: teloxide::dispatching::ShutdownToken,
    settle: Settle,
}

async fn run<R: Run + 'static>(api: &FakeApi, app: Arc<Telegram<R>>) -> Running {
    let bot = bot(&api.url);
    let mut dispatcher = telegram::dispatcher(bot.clone(), app.clone());
    let stop = dispatcher.shutdown_token();
    let settle_app = app.clone();
    let settle: Settle = Box::new(move || {
        let app = settle_app.clone();
        Box::pin(async move { app.finish().await })
    });
    let task = tokio::spawn(async move { telegram::serve(&mut dispatcher, bot, &app).await });
    // Polling has started once the first long poll arrives.
    api.wait_for("the first getUpdates", |calls| {
        calls.iter().any(|c| c.method == "getUpdates")
    })
    .await;
    Running { task, stop, settle }
}

impl Running {
    /// Wait until every turn started so far has finished, which includes
    /// giving up the user's busy slot. The bot's reply reaches the fake API
    /// before the turn ends, so a follow-up sent on seeing the reply could
    /// otherwise be refused as BUSY. Call this between a turn's reply and
    /// the same user's next message.
    async fn settled(&self) {
        (self.settle)().await;
    }

    async fn stop(self) -> anyhow::Result<()> {
        self.stop.shutdown().unwrap().await;
        self.task.await.unwrap()
    }
}

fn sessions(tmp: &TempDb, user: &str) -> Vec<(String, i64)> {
    tmp.raw()
        .prepare(
            "SELECT s.name, (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE u.transport = 'telegram' AND u.external_id = ?1 ORDER BY s.name",
        )
        .unwrap()
        .query_map([user], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn selected(tmp: &TempDb, user: &str) -> Option<String> {
    tmp.raw()
        .query_row(
            "SELECT s.name FROM selected_sessions c
             JOIN sessions s ON s.id = c.session_id
             JOIN users u ON u.id = c.user_id
             WHERE u.transport = 'telegram' AND u.external_id = ?1",
            [user],
            |r| r.get(0),
        )
        .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_and_commands_round_trip_through_the_bot_api() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let (app, logged) = app(&tmp, |s| {
        mock_agent(s, [MockTurn::text("hello back"), MockTurn::text("noted")]).0
    });
    let bot = run(&api, app).await;

    api.push(text_from(1, "hello"));
    assert_eq!(api.messages_to(1, 1).await, ["hello back"]);
    bot.settled().await;
    api.push(text_from(1, "/new work"));
    api.push(text_from(1, "remember this"));
    // A turn replies on its own; wait for it before asking for the list.
    api.messages_to(1, 3).await;
    api.push(text_from(1, "/sessions"));
    let replies = api.messages_to(1, 4).await;
    bot.stop().await.unwrap();

    assert!(replies[1].contains("Started session `work`"), "{replies:?}");
    assert_eq!(replies[2], "noted");
    assert_eq!(
        replies[3],
        "Your sessions:\n  default (2 messages)\n* work (2 messages)"
    );
    // The command menu was registered, and every call carried our token.
    let menu = &api.calls_to("setMyCommands")[0].body["commands"];
    assert_eq!(menu.as_array().unwrap().len(), telegram::COMMANDS.len());
    assert!(
        api.calls()
            .iter()
            .all(|c| c.token == format!("bot{}", token()))
    );
    // "typing" went out before each reply to a prompt, in that chat.
    let order: Vec<String> = api
        .calls()
        .into_iter()
        .filter(|c| c.method == "sendChatAction" || c.method == "sendMessage")
        .map(|c| c.method)
        .collect();
    assert_eq!(
        order,
        [
            "sendChatAction",
            "sendMessage",
            "sendMessage",
            "sendChatAction",
            "sendMessage",
            "sendMessage"
        ]
    );
    assert_eq!(api.calls_to("sendChatAction")[0].body["action"], "typing");
    assert_eq!(
        sessions(&tmp, "1"),
        [("default".to_string(), 2), ("work".to_string(), 2)]
    );
    assert_eq!(selected(&tmp, "1").as_deref(), Some("work"));
    assert!(logged.lock().unwrap().is_empty(), "{logged:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linked_api_continues_the_telegram_session_and_survives_a_restart() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let mut telegram_model = None;
    let (app, logged) = app(&tmp, |s| {
        let (agent, model) = mock_agent(
            s,
            [
                MockTurn::text("telegram reply"),
                MockTurn::text("continued"),
            ],
        );
        telegram_model = Some(model);
        agent
    });
    let bot = run(&api, app).await;
    // Group commands are ignored and cannot claim an identity.
    let mut group = text_from(42, "/link group-api");
    group["message"]["chat"] = json!({"id": -1001, "type": "supergroup", "title": "g"});
    api.push(group);
    api.push(text_from(42, "/new notes"));
    api.push(text_from(42, "telegram message"));
    api.messages_to(42, 2).await;
    bot.settled().await;
    api.push(text_from(42, "/link my-api"));
    let replies = api.messages_to(42, 3).await;
    assert!(replies[2].contains("X-Athena-User: my-api"), "{replies:?}");

    // Separate database connection, as used by the HTTP service process.
    let service = Arc::new(tmp.service().0);
    let (agent, model) = mock_agent(&service, [MockTurn::text("api reply")]);
    let router = http::router(service.clone(), Arc::new(agent), Hosts::Any);
    let owner = service.user("telegram", "42").await.unwrap();
    let notes = service.sessions(&owner).await.unwrap()[0].session.clone();
    let request = |method: &str, path: &str, user: &str, body: Body| {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header(USER_HEADER, user)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    };
    let listed = router
        .clone()
        .oneshot(request("GET", "/sessions", "my-api", Body::empty()))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed: serde_json::Value =
        serde_json::from_slice(&listed.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(listed["sessions"][0]["id"], notes.id);
    let path = format!("/sessions/{}/messages", notes.id);
    let posted = router
        .clone()
        .oneshot(request(
            "POST",
            &path,
            "my-api",
            Body::from(json!({"text":"api message"}).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(posted.status(), StatusCode::OK);
    assert!(
        model.requests()[0]
            .chat_history
            .contains(&rig_agent::prelude::Message::user("telegram message"))
    );
    api.push(text_from(42, "continue in telegram"));
    assert_eq!(api.messages_to(42, 4).await[3], "continued");
    assert!(
        telegram_model.unwrap().requests()[1]
            .chat_history
            .contains(&rig_agent::prelude::Message::user("api message"))
    );
    // Another API identity still cannot access the shared session.
    let denied = router
        .clone()
        .oneshot(request("GET", &path, "stranger", Body::empty()))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(selected(&tmp, "42").as_deref(), Some("notes"));
    assert_eq!(
        count(
            &tmp.raw(),
            "SELECT COUNT(*) FROM user_identities WHERE external_id = 'group-api'"
        ),
        0
    );
    bot.stop().await.unwrap();
    drop((router, service));

    let restarted = Arc::new(tmp.service().0);
    let http = restarted.user("http", "my-api").await.unwrap();
    assert_eq!(http.id(), owner.id());
    let history = restarted.history(&http, &notes.id).await.unwrap();
    assert_eq!(history.len(), 6);
    assert!(history.contains(&rig_agent::prelude::Message::user("telegram message")));
    assert!(history.contains(&rig_agent::prelude::Message::user("api message")));
    assert!(history.contains(&rig_agent::prelude::Message::user("continue in telegram")));
    assert!(
        logged
            .lock()
            .unwrap()
            .iter()
            .all(|line| line.contains("only private chats"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_telegram_users_get_separate_sessions_and_groups_are_ignored() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let (app, logged) = app(&tmp, |s| {
        mock_agent(s, [MockTurn::text("one"), MockTurn::text("two")]).0
    });
    let bot = run(&api, app).await;

    let mut group = text_from(1, "hello group");
    group["message"]["chat"] = json!({"id": -1001, "type": "supergroup", "title": "g"});
    api.push(group);
    api.push(text_from(1, "/new secret"));
    api.push(text_from(1, "mine"));
    // One turn at a time, so the scripted replies go to the expected user.
    let first = api.messages_to(1, 2).await;
    api.push(text_from(2, "/switch secret"));
    api.push(text_from(2, "theirs"));
    let second = api.messages_to(2, 2).await;
    bot.stop().await.unwrap();

    assert_eq!(first[1], "one");
    assert!(second[0].starts_with("You have no session named `secret`"));
    assert_eq!(second[1], "two");
    assert_eq!(sessions(&tmp, "1"), [("secret".to_string(), 2)]);
    assert_eq!(sessions(&tmp, "2"), [("default".to_string(), 2)]);
    // Nothing was said in the group, and nothing was stored for it.
    assert!(api.calls_to("sendMessage").iter().all(|c| c.chat_id() > 0));
    assert_eq!(
        count(
            &tmp.raw(),
            "SELECT COUNT(*) FROM users WHERE external_id = '-1001'"
        ),
        0
    );
    assert_eq!(
        logged.lock().unwrap().clone(),
        ["ignoring a message in chat -1001: only private chats are served"]
    );
}

/// A model that says when it has started and then waits for a permit.
struct Parked {
    inner: Agent,
    started: mpsc::UnboundedSender<String>,
    gate: Arc<Semaphore>,
}

impl Run for Parked {
    async fn run(
        &self,
        prompt: &Request,
        conversation: &str,
        context: Option<ContextHook>,
    ) -> Result<PromptResponse, PromptError> {
        self.started.send(prompt.text.clone()).unwrap();
        self.gate.acquire().await.unwrap().forget();
        self.inner.run(prompt, conversation, context).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_turn_blocks_neither_other_users_nor_its_own_commands() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let gate = Arc::new(Semaphore::new(0));
    let (started, mut starts) = mpsc::unbounded_channel();
    let (app, _) = app(&tmp, |s| Parked {
        inner: mock_agent(s, [MockTurn::text("slow answer"), MockTurn::text("fast")]).0,
        started,
        gate: gate.clone(),
    });
    let bot = run(&api, app).await;

    api.push(text_from(1, "take your time"));
    assert_eq!(starts.recv().await.as_deref(), Some("take your time"));
    // User 1's turn is parked inside the model. Everyone else carries on.
    api.push(text_from(1, "are you there?"));
    api.push(text_from(1, "/usage"));
    api.push(text_from(2, "/sessions"));
    let busy = api.messages_to(1, 2).await;
    let other = api.messages_to(2, 1).await;
    gate.add_permits(1);
    let all = api.messages_to(1, 3).await;
    bot.stop().await.unwrap();

    assert_eq!(busy, [telegram::BUSY, "No turns yet."]);
    assert_eq!(other, ["Your sessions:\n* default (0 messages)"]);
    assert_eq!(all[2], "slow answer");
    // The refused message never reached the model or the transcript.
    assert!(starts.try_recv().is_err());
    assert_eq!(sessions(&tmp, "1"), [("default".to_string(), 2)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_waits_for_a_turn_in_flight_to_reply() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let gate = Arc::new(Semaphore::new(0));
    let (started, mut starts) = mpsc::unbounded_channel();
    let (app, _) = app(&tmp, |s| Parked {
        inner: mock_agent(s, [MockTurn::text("finished anyway")]).0,
        started,
        gate: gate.clone(),
    });
    let Running { task, stop, .. } = run(&api, app).await;

    api.push(text_from(1, "long job"));
    starts.recv().await.unwrap();
    let stopping = tokio::spawn(async move { stop.shutdown().unwrap().await });
    // Polling stops with a last getUpdates that asks for nothing new.
    api.wait_for("the final getUpdates", |calls| {
        calls
            .iter()
            .any(|c| c.method == "getUpdates" && c.body["timeout"] == 0)
    })
    .await;
    assert!(
        !task.is_finished(),
        "serve returned with a turn still running"
    );
    gate.add_permits(1);
    task.await.unwrap().unwrap();
    stopping.await.unwrap();

    // The reply went out before serve returned.
    let sent = api.calls_to("sendMessage");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text(), "finished anyway");
    assert_eq!(sessions(&tmp, "1"), [("default".to_string(), 2)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_signal_before_polling_is_ignored_and_the_next_one_stops_the_bot() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let (app, _) = app(&tmp, |s| mock_agent(s, []).0);
    let bot = bot(&api.url);
    let mut dispatcher = telegram::dispatcher(bot.clone(), app.clone());
    // Signals the test sends by hand; `waits` counts the waits begun.
    let (signal, signals) = mpsc::unbounded_channel::<()>();
    let signals = Arc::new(tokio::sync::Mutex::new(signals));
    let waits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = waits.clone();
    let stopper = telegram::stop_on(dispatcher.shutdown_token(), move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let signals = signals.clone();
        async move {
            signals.lock().await.recv().await;
        }
    });

    // Not polling yet. Once a second wait begins, that signal was handled.
    signal.send(()).unwrap();
    let handled = async {
        while waits.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), handled)
        .await
        .expect("an early signal ended stop_on instead of being ignored");
    let task = tokio::spawn(async move { telegram::serve(&mut dispatcher, bot, &app).await });
    polls(&api, 1).await;
    assert!(!task.is_finished(), "the early signal stopped the bot");

    signal.send(()).unwrap();
    task.await.unwrap().unwrap();
    stopper.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn bot_api_errors_are_logged_retried_or_survived() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let retry = json!({"ok": false, "error_code": 429,
                       "description": "Too Many Requests: retry after 0",
                       "parameters": {"retry_after": 0}});
    api.fail_next(
        "setMyCommands",
        json!({"ok": false, "error_code": 400, "description": "Bad Request: nope"}),
    );
    api.fail_next("getUpdates", retry.clone());
    api.fail_next(
        "sendChatAction",
        json!({"ok": false, "error_code": 400, "description": "Bad Request: no typing"}),
    );
    api.fail_next("sendMessage", retry);
    let (app, logged) = app(&tmp, |s| mock_agent(s, [MockTurn::text("delivered")]).0);
    let bot = run(&api, app).await;

    api.push(text_from(1, "hi"));
    let replies = api.messages_to(1, 2).await;
    api.push(json!({"edited_message": {
        "message_id": 1, "date": 1_790_000_000, "edit_date": 1_790_000_001,
        "chat": {"id": 1, "type": "private", "first_name": "Tester"},
        "from": {"id": 1, "is_bot": false, "first_name": "Tester"},
        "text": "hi (edited)"
    }}));
    api.wait_for("the edited message to be skipped", |calls| {
        calls
            .iter()
            .filter(|c| c.method == "getUpdates" && c.body["offset"] == 3)
            .count()
            > 0
    })
    .await;
    bot.stop().await.unwrap();

    // The 429 on the reply was retried: the same text went out twice.
    assert_eq!(replies, ["delivered", "delivered"]);
    let logged = logged.lock().unwrap().clone();
    let has = |needle: &str| logged.iter().any(|l| l.contains(needle));
    assert!(has("registering the command menu failed"), "{logged:?}");
    assert!(has("getting updates failed: Retry after 0s"), "{logged:?}");
    assert!(has("sending the typing indicator failed"), "{logged:?}");
    assert!(has("ignoring update 2: not a message"), "{logged:?}");
    assert!(logged.iter().all(|l| !l.contains(&secret())), "{logged:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_or_unreachable_api_fails_serve_without_leaking_the_token() {
    let tmp = TempDb::new();

    // Telegram answers, but refuses the token.
    let api = FakeApi::start().await;
    api.fail_next(
        "getMe",
        json!({"ok": false, "error_code": 401, "description": "Unauthorized"}),
    );
    let (refused, _) = app(&tmp, |s| mock_agent(s, []).0);
    let refused_bot = bot(&api.url);
    let mut dispatcher = telegram::dispatcher(refused_bot.clone(), refused.clone());
    let err = telegram::serve(&mut dispatcher, refused_bot, &refused)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("check TELEGRAM_BOT_TOKEN"),
        "{err:#}"
    );

    // Nothing listens at all: every request is a network error.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (unreachable, logged) = app(&tmp, |s| mock_agent(s, []).0);
    let bot = bot(&closed);
    let mut dispatcher = telegram::dispatcher(bot.clone(), unreachable.clone());
    let err = telegram::serve(&mut dispatcher, bot, &unreachable)
        .await
        .unwrap_err();
    let text = format!("{err:#} {:?}", logged.lock().unwrap());
    assert!(
        text.contains("registering the command menu failed"),
        "{text}"
    );
    assert!(!text.contains(&secret()), "{text}");
}

// ---------------- the real binary ----------------

/// `athena telegram` against the fake API, with a key the model is never
/// called with: these tests only use commands. It runs in `dir`, an empty
/// directory, so a developer's `.env` in the repository never reaches it.
fn start_binary(dir: &WorkDir, tmp: &TempDb, api: &FakeApi) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_athena"));
    cmd.arg("telegram")
        .current_dir(dir.path())
        .env("ATHENA_DB", tmp.path())
        .env("TELEGRAM_BOT_TOKEN", token())
        .env("TELEGRAM_API_URL", &api.url)
        .env("OPENROUTER_API_KEY", "unused-by-these-tests")
        .env_remove("AGENT_MODEL");
    // Google Health off, whatever the developer's shell has.
    for name in GOOGLE_HEALTH_VARS {
        cmd.env_remove(name);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Send the process `signal` (`INT` for Ctrl-C, `TERM` as `docker stop`
/// sends) and return its stderr once it has exited cleanly.
async fn stop(child: Child, signal: &str) -> String {
    let pid = child.id().to_string();
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &pid])
        .status()
        .unwrap();
    assert!(status.success());
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "athena telegram failed: {stderr}");
    stderr
}

async fn polls(api: &FakeApi, n: usize) {
    api.wait_for(&format!("getUpdates number {n}"), |calls| {
        calls.iter().filter(|c| c.method == "getUpdates").count() >= n
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_serves_commands_and_remembers_the_session_across_a_restart() {
    let tmp = TempDb::new();
    let dir = WorkDir::new();
    let api = FakeApi::start().await;

    let child = start_binary(&dir, &tmp, &api);
    polls(&api, 1).await;
    api.push(text_from(77, "/new work"));
    api.push(text_from(77, "/new notes"));
    api.push(text_from(77, "/switch work"));
    let first = api.messages_to(77, 3).await;
    let stderr = stop(child, "TERM").await;

    assert_eq!(first[2], "Switched to `work` (0 messages).");
    assert!(stderr.contains("polling for messages"), "{stderr}");
    assert!(!stderr.contains(&secret()), "{stderr}");
    assert_eq!(selected(&tmp, "77").as_deref(), Some("work"));

    // A new process picks up where the old one left off.
    let before = api.calls_to("getUpdates").len();
    let child = start_binary(&dir, &tmp, &api);
    polls(&api, before + 1).await;
    api.push(text_from(77, "/sessions"));
    let all = api.messages_to(77, 4).await;
    stop(child, "INT").await;

    assert_eq!(
        all[3],
        "Your sessions:\n  notes (0 messages)\n* work (0 messages)"
    );
    // The first process's updates were not handled a second time.
    assert_eq!(all.len(), 4);
    assert_eq!(
        sessions(&tmp, "77"),
        [("notes".to_string(), 0), ("work".to_string(), 0)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_google_health_the_binary_says_so_and_stores_nothing() {
    let tmp = TempDb::new();
    let dir = WorkDir::new();
    let api = FakeApi::start().await;

    let child = start_binary(&dir, &tmp, &api);
    polls(&api, 1).await;
    // Each command is answered before the next is sent, so no turn can overlap.
    api.push(text_from(77, "/connect_health"));
    let connect = api.messages_to(77, 1).await;
    api.push(text_from(77, "/disconnect_health"));
    let replies = api.messages_to(77, 2).await;
    let stderr = stop(child, "TERM").await;

    let not_set_up = athena::telegram::health::NOT_SET_UP;
    assert_eq!(connect, [not_set_up]);
    assert_eq!(replies, [not_set_up, not_set_up]);
    // The commands are in the menu the bot registered at startup.
    let menu = &api.calls_to("setMyCommands")[0].body["commands"];
    let names: Vec<&str> = menu
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"connect_health"), "{names:?}");
    assert!(names.contains(&"disconnect_health"), "{names:?}");
    // Nothing was said to the model: a turn would have saved a message.
    let saved: i64 = tmp
        .raw()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(saved, 0);
    assert!(!stderr.contains(&secret()), "{stderr}");
}

#[test]
fn the_binary_checks_its_settings_before_touching_the_database() {
    let tmp = TempDb::new();
    let dir = WorkDir::new();
    let run = |args: &[&str], token: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_athena"));
        cmd.args(args)
            .current_dir(dir.path())
            .env("ATHENA_DB", tmp.path())
            .env("OPENROUTER_API_KEY", "unused")
            .env_remove("TELEGRAM_API_URL");
        match token {
            Some(token) => cmd.env("TELEGRAM_BOT_TOKEN", token),
            None => cmd.env_remove("TELEGRAM_BOT_TOKEN"),
        };
        let out = cmd.output().unwrap();
        assert!(!out.status.success());
        String::from_utf8(out.stderr).unwrap()
    };

    let missing = run(&["telegram"], None);
    let extra = run(&["telegram", "now"], Some(&token()));

    assert!(
        missing.contains("TELEGRAM_BOT_TOKEN is not set"),
        "{missing}"
    );
    assert!(extra.contains("takes no arguments"), "{extra}");
    assert!(!std::path::Path::new(tmp.path()).exists());
}

#[test]
fn the_binary_refuses_to_run_the_bot_without_an_absolute_database_path() {
    let dir = WorkDir::new();

    let out = Command::new(env!("CARGO_BIN_EXE_athena"))
        .arg("telegram")
        .current_dir(dir.path())
        .env("ATHENA_DB", "agent.db")
        .env("TELEGRAM_BOT_TOKEN", token())
        .env("OPENROUTER_API_KEY", "unused")
        .env_remove("TELEGRAM_API_URL")
        .output()
        .unwrap();

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success());
    assert!(stderr.contains("must be an absolute path"), "{stderr}");
    assert!(!dir.path().join("agent.db").exists());
}

// ---- photos and files ----

/// The bot with the sandbox tools on `sandbox`, in front of a mock model
/// scripted with `turns`, putting users' files in the same sandboxes.
fn sandboxed(
    tmp: &TempDb,
    sandbox: &FakeSandbox,
    turns: Vec<MockTurn>,
) -> (Arc<Telegram<Agent>>, Logged, MockCompletionModel) {
    let store = tmp.open();
    let service = Service::new(store.clone(), "m", |_| {});
    let sandboxes = Arc::new(Sandboxes::new(
        fake_server::config(&sandbox.url),
        store.clone(),
    ));
    let model = MockCompletionModel::new(turns);
    let builder = AgentBuilder::new(media::Vision(model.clone())).memory(service.memory());
    let agent = agent::configure_with(builder, Some(sandboxes.clone()));
    let logged = Logged::default();
    let sink = logged.clone();
    let log: Log = Arc::new(move |m| sink.lock().unwrap().push(m.to_string()));
    let app = Telegram::new(Arc::new(service), store, agent, log)
        .sandboxes(Some(sandboxes))
        .album_wait(std::time::Duration::from_millis(200));
    (Arc::new(app), logged, model)
}

/// The user's message as the model got it in its `n`th request, as JSON.
fn prompt(model: &MockCompletionModel, n: usize) -> serde_json::Value {
    let request = &model.requests()[n];
    serde_json::to_value(request.chat_history.last().unwrap()).unwrap()
}

/// The path a prompt's file note says the file is at.
fn staged_path(note: &str) -> String {
    let (_, rest) = note.split_once("in your sandbox at ").unwrap();
    rest.split(". ").next().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_reaches_the_sandbox_and_the_model_and_the_models_files_come_back() {
    const PHOTO: &[u8] = b"\xff\xd8\xff\xe0 a small jpeg";
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let sandbox = FakeSandbox::start().await;
    sandbox.put_file("/w/chart.png", SCREENSHOT);
    sandbox.put_file("/w/data.csv", b"a,b\n");
    api.host_file("big", "photos/file_7.jpg", PHOTO);
    let (app, logged, model) = sandboxed(
        &tmp,
        &sandbox,
        vec![
            MockTurn::tool_call(
                "c1",
                "send_photo",
                json!({"path": "/w/chart.png", "caption": "your chart"}),
            ),
            MockTurn::tool_call("c2", "send_file", json!({"path": "/w/data.csv"})),
            MockTurn::text("done"),
        ],
    );
    let running = run(&api, app).await;

    let sizes = photo_sizes("big", PHOTO.len() as u64);
    api.push(media_from(11, ("photo", sizes), Some("chart this")));
    api.wait_for("the file", |calls| {
        calls.iter().any(|c| c.method == "sendDocument")
    })
    .await;
    running.stop().await.unwrap();

    // The largest size was fetched, saved in the sandbox and shown.
    assert_eq!(api.calls_to("getFile")[0].body["file_id"], "big");
    let prompt = prompt(&model, 0);
    let note = prompt["content"][0]["text"].as_str().unwrap();
    let attached = format!(
        "chart this\n\n[The user attached `photo.jpg` (image/jpeg, {} bytes).",
        PHOTO.len()
    );
    assert!(note.starts_with(&attached), "{note}");
    assert!(note.ends_with("It is shown to you below.]"), "{note}");
    let path = staged_path(note);
    assert!(
        path.starts_with("/tmp/athena-inbox/") && path.ends_with("-photo.jpg"),
        "{path}"
    );
    assert_eq!(sandbox.file(&path).unwrap(), PHOTO);
    assert_eq!(prompt["content"][1]["data"]["value"], media::base64(PHOTO));

    // The reply, then the files, as uploads with their names.
    let order: Vec<String> = api
        .calls()
        .into_iter()
        .map(|c| c.method)
        .filter(|m| m.starts_with("send") && m != "sendChatAction")
        .collect();
    assert_eq!(order, ["sendMessage", "sendPhoto", "sendDocument"]);
    let photo = &api.calls_to("sendPhoto")[0];
    assert_eq!(photo.chat_id(), 11);
    assert_eq!(photo.body["caption"], "your chart");
    assert_eq!(photo.body["photo"]["file_name"], "chart.png");
    assert_eq!(photo.body["photo"]["bytes"], json!(SCREENSHOT));
    let document = &api.calls_to("sendDocument")[0];
    assert_eq!(document.body["document"]["file_name"], "data.csv");
    assert_eq!(document.body["document"]["bytes"], json!(b"a,b\n"));
    assert!(
        document.body.get("caption").is_none(),
        "{:?}",
        document.body
    );
    assert!(
        logged.lock().unwrap().is_empty(),
        "{:?}",
        logged.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn files_that_cannot_be_fetched_or_saved_are_described_to_the_model() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let sandbox = FakeSandbox::start().await;
    api.host_file("doc", "documents/file_1.pdf", b"%PDF");
    // Reported as small, but longer than a bot may download.
    let oversized = vec![b'x'; telegram::DOWNLOAD_LIMIT + 1];
    api.host_file("liar", "documents/file_2.bin", &oversized);
    let (app, logged, model) = sandboxed(
        &tmp,
        &sandbox,
        vec![
            MockTurn::text("a"),
            MockTurn::text("b"),
            MockTurn::text("c"),
        ],
    );
    let running = run(&api, app).await;
    let document = |id: &str, name: Option<&str>| {
        let mut doc = json!({"file_id": id, "file_unique_id": id, "file_size": 4});
        if let Some(name) = name {
            doc["file_name"] = json!(name);
            doc["mime_type"] = json!("application/pdf");
        }
        doc
    };

    // The sandbox cannot make the inbox.
    sandbox.fail_next("/command", 500, "sandbox down");
    api.push(media_from(
        12,
        ("document", document("doc", Some("q3 report.pdf"))),
        None,
    ));
    api.messages_to(12, 1).await;
    running.settled().await;
    // Telegram does not know the file.
    api.push(media_from(12, ("document", document("gone", None)), None));
    api.messages_to(12, 2).await;
    running.settled().await;
    api.push(media_from(12, ("document", document("liar", None)), None));
    api.messages_to(12, 3).await;
    running.stop().await.unwrap();

    let saved = prompt(&model, 0)["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        saved.starts_with("[The user attached `q3 report.pdf` (application/pdf, 4 bytes). It is not in your sandbox: saving it failed: "),
        "{saved}"
    );
    assert!(saved.contains("HTTP 500: sandbox down"), "{saved}");
    for n in [1, 2] {
        let fetched = prompt(&model, n)["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            fetched,
            "[The user attached `file` (type unknown, 4 bytes). It is not in your sandbox: \
             it could not be downloaded from Telegram.]"
        );
    }
    let logged = logged.lock().unwrap().clone();
    assert!(
        logged[0].starts_with("saving a file in the sandbox failed: "),
        "{logged:?}"
    );
    assert!(logged[1].contains("invalid file_id"), "{logged:?}");
    assert!(
        logged[2].ends_with("the file is over 20971520 bytes"),
        "{logged:?}"
    );
    assert_eq!(logged.len(), 3, "{logged:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_captioned_video_is_neither_a_prompt_nor_a_command() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let (app, _) = app(&tmp, |s| mock_agent(s, []).0);
    let running = run(&api, app).await;
    let video = json!({
        "file_id": "v", "file_unique_id": "v", "width": 640, "height": 360, "duration": 3
    });
    api.push(media_from(14, ("video", video), Some("/new x")));
    let replies = api.messages_to(14, 1).await;
    running.stop().await.unwrap();
    assert_eq!(replies, [telegram::NOT_TEXT]);
    assert_eq!(selected(&tmp, "14"), None);
}

// ---- voice notes ----

/// What a fake Workers AI endpoint was sent: the path, the Authorization
/// header and the JSON body.
type Transcribed = Arc<Mutex<Vec<(String, String, serde_json::Value)>>>;

/// A fake Cloudflare Workers AI on loopback that hears `text` in every
/// note. Returns its base URL and what it was sent.
async fn fake_cloudflare(text: &'static str) -> (String, Transcribed) {
    let seen = Transcribed::default();
    let sink = seen.clone();
    let app = axum::Router::new().fallback(
        move |uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let sink = sink.clone();
            async move {
                let auth = headers["authorization"].to_str().unwrap().to_string();
                let body = serde_json::from_slice(&body).unwrap();
                sink.lock()
                    .unwrap()
                    .push((uri.path().to_string(), auth, body));
                axum::Json(json!({"success": true, "errors": [], "result": {"text": text}}))
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/client/v4", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_notes_and_audio_files_become_turns_through_cloudflare() {
    const VOICE: &[u8] = b"OggS\0\x02 opus voice";
    const AUDIO: &[u8] = b"ID3\x04 an mp3";
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let (cf_url, transcribed) = fake_cloudflare("log a 5k run").await;
    api.host_file("vn", "voice/file_1.oga", VOICE);
    api.host_file("au", "music/file_2.mp3", AUDIO);
    // Reported as small, but longer than a voice note may be.
    let oversized = vec![b'x'; telegram::voice::VOICE_LIMIT + 1];
    api.host_file("liar", "voice/file_3.oga", &oversized);
    let store = tmp.open();
    let service = Service::new(store.clone(), "m", |_| {});
    let (agent, model) = mock_agent(&service, [MockTurn::text("one"), MockTurn::text("two")]);
    let logged = Logged::default();
    let sink = logged.clone();
    let log: Log = Arc::new(move |m| sink.lock().unwrap().push(m.to_string()));
    let whisper = telegram::voice::Whisper::new("acct1", "cf-test-token", &cf_url).unwrap();
    let app = Telegram::new(Arc::new(service), store, agent, log).voice(Some(Arc::new(whisper)));
    let running = run(&api, Arc::new(app)).await;
    let note = |kind: &str, id: &str, mime: &str| {
        let file = json!({
            "file_id": id, "file_unique_id": id, "duration": 3, "mime_type": mime, "file_size": 20
        });
        (kind.to_string(), file)
    };

    let (kind, file) = note("voice", "vn", "audio/ogg");
    api.push(media_from(40, (&kind, file), Some("/new cardio")));
    api.messages_to(40, 1).await;
    running.settled().await;
    let (kind, file) = note("audio", "au", "audio/mpeg");
    api.push(media_from(40, (&kind, file), None));
    api.messages_to(40, 2).await;
    running.settled().await;
    let (kind, file) = note("voice", "liar", "audio/ogg");
    api.push(media_from(40, (&kind, file), None));
    let replies = api.messages_to(40, 3).await;
    running.stop().await.unwrap();

    assert_eq!(replies, ["one", "two", telegram::NOT_HEARD]);
    let fetched: Vec<_> = api
        .calls_to("getFile")
        .iter()
        .map(|c| c.body["file_id"].clone())
        .collect();
    assert_eq!(fetched, [json!("vn"), json!("au"), json!("liar")]);
    // The oversized note never reached Cloudflare.
    let transcribed = transcribed.lock().unwrap().clone();
    assert_eq!(transcribed.len(), 2);
    for ((path, auth, body), audio) in transcribed.iter().zip([VOICE, AUDIO]) {
        assert_eq!(
            path,
            "/client/v4/accounts/acct1/ai/run/@cf/openai/whisper-large-v3-turbo"
        );
        assert_eq!(auth, "Bearer cf-test-token");
        assert_eq!(
            *body,
            json!({"audio": media::base64(audio), "task": "transcribe"})
        );
    }
    // The caption leads the prompt and is never a command.
    let text = |n| prompt(&model, n)["content"][0]["text"].clone();
    assert_eq!(text(0), "/new cardio\n\nlog a 5k run");
    assert_eq!(text(1), "log a 5k run");
    assert_eq!(selected(&tmp, "40"), None);
    assert_eq!(sessions(&tmp, "40"), [("default".to_string(), 4)]);
    let logged = logged.lock().unwrap().clone();
    assert_eq!(
        logged,
        [
            "transcribing a voice note failed: downloading it from Telegram failed: \
          the file is over 2097152 bytes"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_lost_on_the_way_is_not_sent_again() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let sandbox = FakeSandbox::start().await;
    sandbox.put_file("/w/chart.png", SCREENSHOT);
    let (app, logged, _) = sandboxed(
        &tmp,
        &sandbox,
        vec![
            MockTurn::tool_call("c1", "send_photo", json!({"path": "/w/chart.png"})),
            MockTurn::text("sent"),
        ],
    );
    // Not an error Telegram gave: an answer teloxide cannot read.
    api.fail_next("sendPhoto", json!({"unexpected": true}));
    let running = run(&api, app).await;
    api.push(text_from(15, "send it"));
    let replies = api.messages_to(15, 2).await;
    running.stop().await.unwrap();

    assert_eq!(replies, ["sent", "(I could not send you `chart.png`.)"]);
    assert!(api.calls_to("sendDocument").is_empty());
    let logged = logged.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with("sending a file failed: "),
        "{logged:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_telegram_refuses_is_sent_as_a_file() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let sandbox = FakeSandbox::start().await;
    sandbox.put_file("/w/tall.png", SCREENSHOT);
    let (app, logged, _) = sandboxed(
        &tmp,
        &sandbox,
        vec![
            MockTurn::tool_call("c1", "send_photo", json!({"path": "/w/tall.png"})),
            MockTurn::text("sent"),
        ],
    );
    api.fail_next(
        "sendPhoto",
        json!({"ok": false, "error_code": 400, "description": "Bad Request: PHOTO_INVALID_DIMENSIONS"}),
    );
    let running = run(&api, app).await;
    api.push(text_from(13, "send it"));
    api.wait_for("the file", |calls| {
        calls.iter().any(|c| c.method == "sendDocument")
    })
    .await;
    running.stop().await.unwrap();

    let document = &api.calls_to("sendDocument")[0];
    assert_eq!(document.body["document"]["file_name"], "tall.png");
    let logged = logged.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with("sending a photo failed, sending it as a file: "),
        "{logged:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_albums_photos_reach_the_model_in_one_turn() {
    const PHOTO: &[u8] = b"\xff\xd8\xff\xe0 album photo";
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let sandbox = FakeSandbox::start().await;
    api.host_file("one", "photos/file_1.jpg", PHOTO);
    api.host_file("two", "photos/file_2.jpg", PHOTO);
    let (app, logged, model) = sandboxed(&tmp, &sandbox, vec![MockTurn::text("both seen")]);
    let running = run(&api, app).await;
    for (id, caption) in [("one", Some("which is sharper?")), ("two", None)] {
        let mut update = media_from(16, ("photo", photo_sizes(id, PHOTO.len() as u64)), caption);
        update["message"]["media_group_id"] = json!("13579");
        api.push(update);
    }
    let replies = api.messages_to(16, 1).await;
    running.stop().await.unwrap();

    assert_eq!(replies, ["both seen"]);
    assert_eq!(model.requests().len(), 1);
    let prompt = prompt(&model, 0);
    let parts = prompt["content"].as_array().unwrap();
    assert!(
        parts[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("which is sharper?")
    );
    assert_eq!(parts.iter().filter(|p| p["type"] == "image").count(), 2);
    assert_eq!(api.calls_to("getFile").len(), 2);
    assert!(
        logged.lock().unwrap().is_empty(),
        "{:?}",
        logged.lock().unwrap()
    );
}

/// The entities of a recorded `sendMessage`, as `(type, offset, length)`.
fn entities(call: &fake_api::Call) -> Vec<(String, u64, u64)> {
    call.body["entities"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|e| {
                    (
                        e["type"].as_str().unwrap().to_string(),
                        e["offset"].as_u64().unwrap(),
                        e["length"].as_u64().unwrap(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn entity(kind: &str, offset: u64, length: u64) -> (String, u64, u64) {
    (kind.to_string(), offset, length)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_markdown_reply_reaches_the_bot_api_as_text_with_entities() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let reply = "# Plan\n\n**Bold** \u{1F600} and `code`, see [docs](https://example.com/a).\n\n\
                 ```rust\nfn main() {}\n```\n\n2 * 3 and snake_case stay as written.";
    let (app, logged) = app(&tmp, |s| mock_agent(s, [MockTurn::text(reply)]).0);
    let running = run(&api, app).await;

    api.push(text_from(21, "plan it"));
    let replies = api.messages_to(21, 1).await;
    running.stop().await.unwrap();

    assert_eq!(
        replies,
        [
            "Plan\n\nBold \u{1F600} and code, see docs.\n\nfn main() {}\n\n\
             2 * 3 and snake_case stay as written."
        ]
    );
    let sent = &api.calls_to("sendMessage")[0];
    // The text goes as it is, with no parse mode: Telegram parses nothing.
    assert!(sent.body.get("parse_mode").is_none());
    // Offsets count UTF-16 units: the emoji is two of them.
    assert_eq!(
        entities(sent),
        [
            entity("bold", 0, 4),
            entity("bold", 6, 4),
            entity("code", 18, 4),
            entity("text_link", 28, 4),
            entity("pre", 35, 12),
        ]
    );
    assert_eq!(sent.body["entities"][3]["url"], "https://example.com/a");
    assert_eq!(sent.body["entities"][4]["language"], "rust");
    assert!(logged.lock().unwrap().is_empty(), "{logged:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_telegram_refuses_to_format_is_sent_again_as_plain_text() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    api.fail_next(
        "sendMessage",
        json!({"ok": false, "error_code": 400,
               "description": "Bad Request: can't parse entities: Can't find end of Bold entity"}),
    );
    let (app, logged) = app(&tmp, |s| {
        mock_agent(s, [MockTurn::text("**Hello** there")]).0
    });
    let running = run(&api, app).await;

    api.push(text_from(22, "hi"));
    api.messages_to(22, 2).await;
    running.stop().await.unwrap();

    let sends = api.calls_to("sendMessage");
    assert_eq!(sends.len(), 2);
    assert_eq!(entities(&sends[0]), [entity("bold", 0, 5)]);
    // The same words, markup already gone, and no entities this time.
    assert_eq!(sends[1].text(), "Hello there");
    assert!(sends[1].body.get("entities").is_none());
    let logged = logged.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with("sending formatted text failed, sending it as plain text: "),
        "{logged:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_bot_does_not_resend_formatted_text() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    api.fail_next(
        "sendMessage",
        json!({"ok": false, "error_code": 403,
               "description": "Forbidden: bot was blocked by the user"}),
    );
    let (app, logged) = app(&tmp, |s| mock_agent(s, [MockTurn::text("**Hello**")]).0);
    let running = run(&api, app).await;

    api.push(text_from(23, "hi"));
    api.wait_for("the send", |calls| {
        calls.iter().any(|c| c.method == "sendMessage")
    })
    .await;
    running.stop().await.unwrap();

    assert_eq!(api.calls_to("sendMessage").len(), 1);
    let logged = logged.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with("sending a message failed: "),
        "{logged:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_reply_with_a_code_block_across_the_cut_is_sent_in_valid_messages() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let code: Vec<String> = (0..260)
        .map(|i| format!("let value_{i} = compute({i}, \"item\");"))
        .collect();
    let code = code.join("\n");
    let reply = format!(
        "Here is the code.\n\n```rust\n{code}\n```\n\n**That** is all, {}.",
        "and then some words ".repeat(40)
    );
    assert!(reply.len() > 10_000);
    let (app, logged) = app(&tmp, |s| mock_agent(s, [MockTurn::text(reply)]).0);
    let running = run(&api, app).await;

    api.push(text_from(24, "long please"));
    api.wait_for("the whole reply", |calls| {
        let sends = calls.iter().filter(|c| c.method == "sendMessage");
        sends.filter(|c| c.text().ends_with("some words .")).count() == 1
    })
    .await;
    running.stop().await.unwrap();

    // The fake refuses messages that break Telegram's entity rules or
    // exceed 4096 units. None was refused, so none was sent again.
    let sends = api.calls_to("sendMessage");
    assert!(sends.len() >= 3, "{}", sends.len());
    assert!(logged.lock().unwrap().is_empty(), "{logged:?}");
    let pre: Vec<String> = sends
        .iter()
        .flat_map(|call| {
            let text: Vec<u16> = call.text().encode_utf16().collect();
            entities(call)
                .into_iter()
                .filter(|e| e.0 == "pre")
                .map(move |(_, at, len)| {
                    String::from_utf16(&text[at as usize..(at + len) as usize]).unwrap()
                })
        })
        .collect();
    assert!(pre.len() >= 2, "the block continues in the next message");
    assert_eq!(pre.join("\n"), code);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_request_teloxide_has_no_name_for_also_resends_the_text_plain() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    api.fail_next(
        "sendMessage",
        json!({"ok": false, "error_code": 400,
               "description": "Bad Request: entity beginning at 3 is not allowed"}),
    );
    let (app, logged) = app(&tmp, |s| mock_agent(s, [MockTurn::text("*Hello* there")]).0);
    let running = run(&api, app).await;

    api.push(text_from(25, "hi"));
    api.messages_to(25, 2).await;
    running.stop().await.unwrap();

    let sends = api.calls_to("sendMessage");
    assert_eq!(sends.len(), 2);
    assert_eq!(entities(&sends[0]), [entity("italic", 0, 5)]);
    assert_eq!(sends[1].text(), "Hello there");
    assert!(sends[1].body.get("entities").is_none());
    let logged = logged.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with("sending formatted text failed, sending it as plain text: "),
        "{logged:?}"
    );
}

/// Schedule a reminder for Telegram user `user`, as of two minutes ago.
fn overdue(tmp: &TempDb, user: &str, args: serde_json::Value) -> i64 {
    let store = tmp.open();
    let owner = store.user("telegram", user).unwrap().id();
    let args: athena::reminders::Create = serde_json::from_value(args).unwrap();
    let earlier = jiff::Timestamp::now() - jiff::SignedDuration::from_mins(2);
    let shown = store.create_reminder(owner, "s", &args, earlier).unwrap();
    let id = shown["id"].as_i64().unwrap();
    // A task is confirmed as its user would, with its id and code.
    if let Some(code) = shown["confirmation_code"].as_str() {
        let said = format!("confirm #{id} {code}");
        store
            .confirm_reminder(owner, "s", id, code, &said, earlier)
            .unwrap();
    }
    id
}

fn job_status(tmp: &TempDb, id: i64) -> (String, Option<String>) {
    tmp.raw()
        .query_row(
            "SELECT status, last_error FROM jobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_delivers_a_due_reminder_and_stops_its_scheduler_on_sigterm() {
    let tmp = TempDb::new();
    let dir = WorkDir::new();
    let api = FakeApi::start().await;
    let id = overdue(
        &tmp,
        "88",
        json!({"kind": "notify", "text": "water the plants", "in_minutes": 1}),
    );

    let child = start_binary(&dir, &tmp, &api);
    let sent = api.messages_to(88, 1).await;
    let stderr = stop(child, "TERM").await;

    assert_eq!(sent, ["Reminder: water the plants"]);
    assert!(stderr.contains("running reminders"), "{stderr}");
    assert_eq!(job_status(&tmp, id), ("done".into(), None));
}

#[tokio::test(flavor = "multi_thread")]
async fn reminders_go_through_the_bot_api_and_a_block_stops_them() {
    let tmp = TempDb::new();
    let api = FakeApi::start().await;
    let (app, _) = app(&tmp, |s| {
        mock_agent(s, [MockTurn::text("the news is quiet")]).0
    });
    let blocked = overdue(
        &tmp,
        "91",
        json!({"kind": "notify", "text": "a", "repeat": "daily", "time": "00:00"}),
    );
    // Due now, whatever the time of day.
    tmp.raw()
        .execute("UPDATE jobs SET next_run_at = 0 WHERE id = ?1", [blocked])
        .unwrap();
    // Its send is refused for good.
    api.fail_next(
        "sendMessage",
        json!({"ok": false, "error_code": 403,
               "description": "Forbidden: bot was blocked by the user"}),
    );

    let scheduler = telegram::jobs::scheduler(app.clone(), bot(&api.url), tmp.open())
        .every(std::time::Duration::from_millis(10));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(scheduler.run(async move {
        let _ = stopped.await;
    }));
    api.wait_for("the blocked reminder", |calls| {
        calls
            .iter()
            .any(|c| c.method == "sendMessage" && c.chat_id() == 91)
    })
    .await;
    // Scheduled while the loop runs: a later poll finds it.
    let task = overdue(
        &tmp,
        "92",
        json!({"kind": "agent_task", "text": "check the news", "in_minutes": 1}),
    );
    let reply = api.messages_to(92, 1).await;
    stop.send(()).unwrap();
    running.await.unwrap();

    assert_eq!(reply, ["the news is quiet"]);
    assert_eq!(job_status(&tmp, task), ("done".into(), None));
    let (status, error) = job_status(&tmp, blocked);
    assert_eq!(status, "failed");
    assert!(error.unwrap().contains("blocked"));
    assert_eq!(sessions(&tmp, "92"), [("default".to_string(), 2)]);
}
