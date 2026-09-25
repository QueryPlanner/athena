//! The Telegram transport end to end against a fake Bot API server
//! (`telegram/fake_api.rs`): in-process with the mock model, and as the real
//! `athena telegram` binary for everything that needs no model.

mod common;
#[path = "telegram/fake_api.rs"]
mod fake_api;

use athena::runner::Run;
use athena::service::Service;
use athena::telegram::{self, Log, Telegram};
use common::*;
use fake_api::{FakeApi, text_from};
use rig_agent::agent::{Agent, PromptResponse};
use rig_agent::completion::PromptError;
use rig_core::test_utils::MockTurn;
use serde_json::json;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use teloxide::Bot;
use tokio::sync::{Semaphore, mpsc};

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

/// A running bot: `serve` in a task, stopped through its shutdown token.
struct Running {
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    stop: teloxide::dispatching::ShutdownToken,
}

async fn run<R: Run + 'static>(api: &FakeApi, app: Arc<Telegram<R>>) -> Running {
    let bot = bot(&api.url);
    let mut dispatcher = telegram::dispatcher(bot.clone(), app.clone(), false);
    let stop = dispatcher.shutdown_token();
    let task = tokio::spawn(async move { telegram::serve(&mut dispatcher, bot, &app).await });
    // Polling has started once the first long poll arrives.
    api.wait_for("the first getUpdates", |calls| {
        calls.iter().any(|c| c.method == "getUpdates")
    })
    .await;
    Running { task, stop }
}

impl Running {
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
    async fn run(&self, prompt: &str, conversation: &str) -> Result<PromptResponse, PromptError> {
        self.started.send(prompt.to_string()).unwrap();
        self.gate.acquire().await.unwrap().forget();
        self.inner.run(prompt, conversation).await
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
    let Running { task, stop } = run(&api, app).await;

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
    let mut dispatcher = telegram::dispatcher(refused_bot.clone(), refused.clone(), false);
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
    let mut dispatcher = telegram::dispatcher(bot.clone(), unreachable.clone(), false);
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
    Command::new(env!("CARGO_BIN_EXE_athena"))
        .arg("telegram")
        .current_dir(dir.path())
        .env("ATHENA_DB", tmp.path())
        .env("TELEGRAM_BOT_TOKEN", token())
        .env("TELEGRAM_API_URL", &api.url)
        .env("OPENROUTER_API_KEY", "unused-by-these-tests")
        .env_remove("AGENT_MODEL")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Ctrl-C the process and return its stderr once it has exited cleanly.
async fn interrupt(child: Child) -> String {
    let pid = child.id().to_string();
    let status = Command::new("kill").args(["-INT", &pid]).status().unwrap();
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
    let stderr = interrupt(child).await;

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
    interrupt(child).await;

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
