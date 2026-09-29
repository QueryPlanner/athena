//! The sandbox and browser tools through the real agent loop, against a
//! fake OpenSandbox server (`sandbox/fake_server.rs`) and a real database.

mod common;
#[path = "sandbox/fake_server.rs"]
mod fake_server;

use athena::agent;
use athena::media::{self, Kind, Outbox};
use athena::runner::Request;
use athena::sandbox::tools::{CAPTION_LIMIT, PHOTO_LIMIT, READ_LIMIT};
use athena::sandbox::{Config, Error, INBOX_DIR, Sandboxes};
use athena::service::{Service, User};
use athena::store::SandboxRow;
use common::*;
use fake_server::{FakeSandbox, SCREENSHOT, config, printed};
use reqwest::header::HeaderValue;
use rig_agent::agent::AgentBuilder;
use rig_core::message::AssistantContent;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct Harness {
    fake: FakeSandbox,
    tmp: TempDb,
    service: Service,
    sandboxes: Arc<Sandboxes>,
    user: User,
}

async fn harness_with(configure: impl FnOnce(&mut Config)) -> Harness {
    let fake = FakeSandbox::start().await;
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let mut config = config(&fake.url);
    configure(&mut config);
    let sandboxes = Arc::new(Sandboxes::new(config, tmp.open()));
    let user = service.user("telegram", "7").await.unwrap();
    Harness {
        fake,
        tmp,
        service,
        sandboxes,
        user,
    }
}

async fn harness() -> Harness {
    harness_with(|_| {}).await
}

/// What a tool result says, as the model was sent it.
fn tool_result(request: &rig_core::completion::CompletionRequest) -> String {
    let last = serde_json::to_value(request.chat_history.last().unwrap()).unwrap();
    let content = &last["content"][0];
    assert_eq!(content["type"], "toolresult", "{last}");
    content["content"][0]["text"].as_str().unwrap().to_string()
}

impl Harness {
    async fn session(&self, name: &str) -> String {
        session(&self.service, &self.user, name).await.id
    }

    /// The production agent in front of the mock model, with the same
    /// [`media::Vision`] adapter as the real provider gets.
    fn agent(&self, turns: Vec<MockTurn>) -> (rig_agent::agent::Agent, MockCompletionModel) {
        let model = MockCompletionModel::new(turns);
        let builder = AgentBuilder::new(media::Vision(model.clone())).memory(self.service.memory());
        let agent = agent::configure_with(builder, Some(self.sandboxes.clone()));
        (agent, model)
    }

    /// One turn of `request` in which the model calls `tool` with `args`;
    /// returns the request the model got next, holding the tool's result.
    async fn turn(
        &self,
        session_id: &str,
        tool: &str,
        args: Value,
        request: Request,
    ) -> rig_core::completion::CompletionRequest {
        let (agent, model) = self.agent(vec![
            MockTurn::tool_call("call_1", tool, args),
            MockTurn::text("done"),
        ]);
        self.service
            .send(&agent, &self.user, session_id, request)
            .await
            .unwrap();
        model.requests().remove(1)
    }

    /// One turn in which the model calls `tool` with `args`; returns what
    /// the tool answered.
    async fn call(&self, session_id: &str, tool: &str, args: Value) -> String {
        tool_result(&self.turn(session_id, tool, args, "go".into()).await)
    }

    fn row(&self, session_id: &str) -> Option<SandboxRow> {
        self.tmp.open().sandbox(session_id).unwrap()
    }
}

#[tokio::test]
async fn the_first_tool_call_creates_the_sessions_sandbox_and_later_calls_reuse_it() {
    let h = harness().await;
    let s = h.session("work").await;

    assert_eq!(
        h.call(&s, "shell", json!({"command": "cd /tmp"})).await,
        "ran: cd /tmp"
    );
    assert_eq!(
        h.call(&s, "shell", json!({"command": "pwd"})).await,
        "ran: pwd"
    );

    // One sandbox, made the way the config says, labelled with its owner.
    let creates = h.fake.requests_to("POST", "/v1/sandboxes");
    assert_eq!(creates.len(), 1);
    let body = &creates[0].body;
    assert_eq!(body["image"]["uri"], "athena-sandbox:test");
    assert_eq!(body["timeout"], 600);
    assert_eq!(
        body["resourceLimits"],
        json!({"cpu": "500m", "memory": "1Gi"})
    );
    assert_eq!(body["entrypoint"][0], "/bin/sh");
    assert_eq!(
        body["metadata"],
        json!({"app": "athena", "env": "test", "session": s, "user": h.user.id().to_string()})
    );
    assert_eq!(body["env"]["JUPYTER_HOST"], "http://127.0.0.1:44771");
    assert_eq!(body["env"]["JUPYTER_TOKEN"].as_str().unwrap().len(), 32);
    // No key configured, none sent.
    assert!(creates[0].headers.get("open-sandbox-api-key").is_none());

    // One bash session, both commands in it, with the default timeout.
    assert_eq!(h.fake.requests_to("POST", "/session").len(), 1);
    let runs = h.fake.requests_to("POST", "/run");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].path, runs[1].path);
    assert_eq!(runs[1].body, json!({"command": "pwd", "timeout": 120_000}));

    // The second call renewed the sandbox rather than making another.
    let renewals = h.fake.requests_to("POST", "/renew-expiration");
    assert_eq!(renewals.len(), 1);
    assert!(
        renewals[0].body["expiresAt"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    let row = h.row(&s).unwrap();
    assert_eq!(row.sandbox_id, "sbx-1");
    assert!(row.bash_session.unwrap().starts_with("bash-"));
    assert_eq!(h.fake.live(), ["sbx-1"]);

    // The transcript holds the tool result like any other.
    let rows = raw_rows(&h.tmp.raw(), &s);
    assert!(rows[2].contains("ran: cd /tmp"), "{}", rows[2]);
}

#[tokio::test]
async fn each_session_gets_its_own_sandbox_and_a_new_process_finds_it() {
    let h = harness().await;
    let (a, b) = (h.session("a").await, h.session("b").await);
    h.call(&a, "shell", json!({"command": "true"})).await;
    h.call(&b, "shell", json!({"command": "true"})).await;
    assert_eq!(h.row(&a).unwrap().sandbox_id, "sbx-1");
    assert_eq!(h.row(&b).unwrap().sandbox_id, "sbx-2");

    // Another process (a fresh Sandboxes on the same database) reuses it.
    let other = Arc::new(Sandboxes::new(config(&h.fake.url), h.tmp.open()));
    other.shell(&a, "ls", Duration::from_secs(5)).await.unwrap();
    assert_eq!(h.fake.requests_to("POST", "/v1/sandboxes").len(), 2);
    assert_eq!(h.fake.requests_to("POST", "/session").len(), 2);
}

#[tokio::test]
async fn a_sandbox_the_server_no_longer_has_is_replaced() {
    let h = harness().await;
    let s = h.session("s").await;
    h.call(&s, "shell", json!({"command": "true"})).await;
    h.fake.kill("sbx-1");

    assert_eq!(
        h.call(&s, "shell", json!({"command": "ls"})).await,
        "ran: ls"
    );
    let row = h.row(&s).unwrap();
    assert_eq!(row.sandbox_id, "sbx-2");
    // The new sandbox got its own bash session.
    assert_eq!(h.fake.requests_to("POST", "/session").len(), 2);
    assert_eq!(h.fake.live(), ["sbx-2"]);
}

#[tokio::test]
async fn a_bash_session_lost_to_an_execd_restart_is_recreated_once() {
    let h = harness().await;
    let s = h.session("s").await;
    h.call(&s, "shell", json!({"command": "true"})).await;
    let before = h.row(&s).unwrap().bash_session;
    h.fake.restart_execd();

    assert_eq!(
        h.call(&s, "shell", json!({"command": "ls"})).await,
        "ran: ls"
    );
    let after = h.row(&s).unwrap().bash_session;
    assert_ne!(after, before);
    assert_eq!(h.fake.requests_to("POST", "/run").len(), 3);
}

#[tokio::test]
async fn shell_output_errors_and_timeouts_reach_the_model() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.reply_next(
        "{\"type\":\"stdout\",\"text\":\"partial\\n\"}\n\n\
         {\"type\":\"stderr\",\"text\":\"boom\\n\"}\n\n\
         {\"type\":\"error\",\"error\":{\"ename\":\"CommandExecError\",\"evalue\":\"exit status 3\"}}\n\n",
    );
    let out = h
        .call(&s, "shell", json!({"command": "false", "timeout_secs": 5}))
        .await;
    assert_eq!(
        out,
        "partial\n[stderr]\nboom\n[error]\nCommandExecError: exit status 3"
    );
    assert_eq!(h.fake.requests_to("POST", "/run")[0].body["timeout"], 5000);

    // A body whose last event has no newline after it still counts.
    h.fake.reply_next("{\"type\":\"stdout\",\"text\":\"last\"}");
    let out = h.call(&s, "shell", json!({"command": "x"})).await;
    assert_eq!(out, "last\n[the execution did not report completion]");

    let out = h
        .call(&s, "shell", json!({"command": "x", "timeout_secs": 0}))
        .await;
    assert!(
        out.contains("timeout_secs must be between 1 and 1800"),
        "{out}"
    );
    assert_eq!(h.fake.requests_to("POST", "/run").len(), 2);
}

#[tokio::test]
async fn output_beyond_the_readable_limit_is_cut_off() {
    let h = harness().await;
    let s = h.session("s").await;
    // More than the 8 MiB the client reads, in one stdout event.
    h.fake.reply_next(printed(&"x".repeat(9 * 1024 * 1024)));
    let out = h.call(&s, "shell", json!({"command": "yes"})).await;
    assert!(
        out.contains("stopped reading after 8388608 bytes"),
        "{}",
        &out[out.len() - 200..]
    );
    assert!(out.contains("[output truncated:"));
}

#[tokio::test]
async fn run_code_keeps_one_interpreter_and_replaces_a_lost_one() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.reply_next(
        "{\"type\":\"result\",\"results\":{\"text/plain\":\"4\"}}\n\n{\"type\":\"execution_complete\"}\n\n",
    );
    let out = h
        .call(
            &s,
            "run_code",
            json!({"language": "python", "code": "2 + 2"}),
        )
        .await;
    assert_eq!(out, "[result]\n4");
    h.call(
        &s,
        "run_code",
        json!({"language": "python", "code": "x = 1"}),
    )
    .await;
    assert_eq!(h.fake.requests_to("POST", "/code/context").len(), 1);
    let runs = h.fake.requests_to("POST", "/code");
    assert_eq!(runs[0].body["context"], runs[1].body["context"]);
    assert_eq!(runs[1].body["code"], "x = 1");
    assert_eq!(h.row(&s).unwrap().code_language.as_deref(), Some("python"));

    // execd restarted: a fresh context, and the call still succeeds.
    h.fake.restart_execd();
    let out = h
        .call(
            &s,
            "run_code",
            json!({"language": "python", "code": "print(1)"}),
        )
        .await;
    assert_eq!(out, "ran: print(1)");
    assert_eq!(h.fake.requests_to("POST", "/code/context").len(), 2);

    let out = h
        .call(&s, "run_code", json!({"language": "ruby", "code": "1"}))
        .await;
    assert!(out.contains("language `ruby` is not available"), "{out}");
    let out = h
        .call(
            &s,
            "run_code",
            json!({"language": "python", "code": "1", "timeout_secs": 9999}),
        )
        .await;
    assert!(out.contains("timeout_secs"), "{out}");
}

#[tokio::test]
async fn a_stored_interpreter_for_another_language_is_replaced() {
    let h = harness().await;
    let s = h.session("s").await;
    h.call(&s, "shell", json!({"command": "true"})).await;
    let mut row = h.row(&s).unwrap();
    row.code_language = Some("javascript".into());
    row.code_context = Some("ctx-old".into());
    h.tmp.open().update_sandbox(&row).unwrap();

    h.call(&s, "run_code", json!({"language": "python", "code": "1"}))
        .await;
    let row = h.row(&s).unwrap();
    assert_eq!(row.code_language.as_deref(), Some("python"));
    assert_ne!(row.code_context.as_deref(), Some("ctx-old"));
}

#[tokio::test]
async fn files_are_written_and_read_inside_the_sandbox_only() {
    let h = harness().await;
    let s = h.session("s").await;
    let content = "line 1\nit's \"quoted\" $(not run)\n";
    let out = h
        .call(
            &s,
            "write_file",
            json!({"path": "/workspace/a.txt", "content": content}),
        )
        .await;
    assert_eq!(
        out,
        format!("wrote {} bytes to /workspace/a.txt", content.len())
    );
    assert_eq!(h.fake.file("/workspace/a.txt").unwrap(), content.as_bytes());

    let out = h
        .call(&s, "read_file", json!({"path": "/workspace/a.txt"}))
        .await;
    assert_eq!(out, content);
    // Never the host's files: the path went to the sandbox.
    let download = &h.fake.requests_to("GET", "/files/download")[0];
    assert_eq!(download.query, "path=%2Fworkspace%2Fa.txt");
    assert_eq!(download.headers["range"], format!("bytes=0-{READ_LIMIT}"));

    // A file longer than the limit is cut, and says so.
    h.fake.put_file("/big", &vec![b'a'; READ_LIMIT + 10]);
    let out = h.call(&s, "read_file", json!({"path": "/big"})).await;
    assert!(out.ends_with(&format!("[file truncated after {READ_LIMIT} bytes]")));
    assert_eq!(
        out.len() - out.find('\n').unwrap(),
        format!("\n[file truncated after {READ_LIMIT} bytes]").len()
    );

    // An empty file, and one exactly at the limit, are whole.
    h.fake.put_file("/empty", b"");
    assert_eq!(h.call(&s, "read_file", json!({"path": "/empty"})).await, "");
    h.fake.put_file("/exact", &vec![b'b'; READ_LIMIT]);
    assert_eq!(
        h.call(&s, "read_file", json!({"path": "/exact"}))
            .await
            .len(),
        READ_LIMIT
    );

    let out = h.call(&s, "read_file", json!({"path": "/nope"})).await;
    assert!(out.contains("HTTP 404"), "{out}");
}

/// The last message the model was sent, as JSON.
fn last_message(request: &rig_core::completion::CompletionRequest) -> Value {
    serde_json::to_value(request.chat_history.last().unwrap()).unwrap()
}

#[tokio::test]
async fn an_image_the_model_looks_at_is_not_kept_in_the_transcript() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.put_file("/w/shot.png", SCREENSHOT);
    let request = h
        .turn(
            &s,
            "view_image",
            json!({"path": "/w/shot.png"}),
            "go".into(),
        )
        .await;

    let last = last_message(&request);
    let content = last["content"].as_array().unwrap();
    // The tool result stays text, so it can be sent as the tool's reply.
    assert_eq!(content[0]["content"][0]["text"], "image: /w/shot.png");
    assert_eq!(content[0]["content"].as_array().unwrap().len(), 1);
    // Then the image itself, as the user's.
    assert_eq!(content[1]["text"], "Image from view_image:");
    assert_eq!(content[2]["type"], "image");
    assert_eq!(content[2]["data"]["value"], media::base64(SCREENSHOT));
    assert_eq!(content[2]["media_type"], "png");
    assert_eq!(content.len(), 3);

    // The transcript keeps the note but not the image's bytes.
    let stored = raw_rows(&h.tmp.raw(), &s).join("\n");
    assert!(stored.contains("image: /w/shot.png"));
    assert!(!stored.contains(&media::base64(SCREENSHOT)));
    assert!(stored.contains(media::NOT_KEPT));
}

#[tokio::test]
async fn view_image_shows_images_and_says_why_it_shows_nothing_else() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.put_file("/w/chart.png", SCREENSHOT);
    let request = h
        .turn(
            &s,
            "view_image",
            json!({"path": "/w/chart.png"}),
            "go".into(),
        )
        .await;
    let last = last_message(&request);
    assert_eq!(
        last["content"][0]["content"][0]["text"],
        "image: /w/chart.png"
    );
    assert_eq!(last["content"][1]["text"], "Image from view_image:");
    assert_eq!(
        last["content"][2]["data"]["value"],
        media::base64(SCREENSHOT)
    );

    h.fake.put_file("/w/notes.txt", b"just text");
    let out = h
        .call(&s, "view_image", json!({"path": "/w/notes.txt"}))
        .await;
    assert_eq!(
        out,
        "image: /w/notes.txt\n[not shown: not a PNG, JPEG, GIF or WebP image]"
    );

    let mut big = SCREENSHOT.to_vec();
    big.resize(media::MAX_IMAGE_BYTES + 1, 0);
    h.fake.put_file("/w/big.png", &big);
    let out = h
        .call(&s, "view_image", json!({"path": "/w/big.png"}))
        .await;
    assert!(out.ends_with("bytes the model can be shown]"), "{out}");

    let out = h
        .call(&s, "view_image", json!({"path": "/w/none.png"}))
        .await;
    assert!(out.starts_with("image: /w/none.png\n[not shown: "), "{out}");
    assert!(out.contains("HTTP 404"), "{out}");
}

/// A turn in which the model sends `path` with `tool`, with an outbox.
async fn sent(h: &Harness, s: &str, tool: &str, args: Value) -> (String, Outbox) {
    let outbox = Outbox::default();
    let request = Request {
        text: "go".into(),
        outbox: Some(outbox.clone()),
        ..Request::default()
    };
    let out = tool_result(&h.turn(s, tool, args, request).await);
    (out, outbox)
}

#[tokio::test]
async fn sent_files_are_queued_for_the_transport_with_a_safe_name() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.put_file("/w/out/shot.png", SCREENSHOT);
    h.fake.put_file("/w/report.csv", b"a,b\n1,2\n");

    let (out, outbox) = sent(
        &h,
        &s,
        "send_photo",
        json!({"path": "/w/out/shot.png", "caption": "the page"}),
    )
    .await;
    assert_eq!(
        out,
        "shot.png (23 bytes) will be sent to the user with your reply"
    );
    let queued = outbox.take();
    assert_eq!(queued.len(), 1);
    assert_eq!(
        (
            queued[0].name.as_str(),
            queued[0].kind,
            queued[0].caption.as_deref()
        ),
        ("shot.png", Kind::Photo, Some("the page"))
    );
    assert_eq!(queued[0].bytes, SCREENSHOT);

    let (out, outbox) = sent(&h, &s, "send_file", json!({"path": "/w/report.csv"})).await;
    assert!(out.starts_with("report.csv (8 bytes)"), "{out}");
    let queued = outbox.take();
    assert_eq!(
        (queued[0].kind, queued[0].caption.clone()),
        (Kind::Document, None)
    );
}

#[tokio::test]
async fn files_are_not_sent_where_they_cannot_arrive_or_should_not() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.put_file("/w/report.csv", b"a,b");
    let mut big = SCREENSHOT.to_vec();
    big.resize(PHOTO_LIMIT + 1, 0);
    h.fake.put_file("/w/huge.png", &big);

    // No outbox: the HTTP API and the CLI.
    let out = h
        .call(&s, "send_file", json!({"path": "/w/report.csv"}))
        .await;
    assert!(out.contains("cannot receive files"), "{out}");

    for (tool, args, why) in [
        (
            "send_photo",
            json!({"path": "/w/report.csv"}),
            "send it with send_file",
        ),
        ("send_photo", json!({"path": "/w/huge.png"}), "is over the"),
        (
            "send_file",
            json!({"path": "/w/report.csv", "caption": "c".repeat(CAPTION_LIMIT + 1)}),
            "caption is over",
        ),
        ("send_file", json!({"path": "/w/missing"}), "HTTP 404"),
    ] {
        let (out, outbox) = sent(&h, &s, tool, args).await;
        assert!(out.contains(why), "{tool}: {out}");
        assert!(outbox.take().is_empty(), "{tool}");
    }
}

#[tokio::test]
async fn a_users_file_is_staged_in_the_sessions_inbox() {
    let h = harness().await;
    let s = h.session("s").await;
    let path = h
        .sandboxes
        .stage(&s, "a b.pdf", b"%PDF".to_vec())
        .await
        .unwrap();
    let name = path.strip_prefix(&format!("{INBOX_DIR}/")).unwrap();
    assert_eq!(name.len(), "12345678-a b.pdf".len(), "{path}");
    assert!(name.ends_with("-a b.pdf"), "{path}");
    assert_eq!(h.fake.file(&path).unwrap(), b"%PDF");
    let mkdir = &h.fake.requests_to("POST", "/command")[0];
    assert_eq!(mkdir.body["command"], "'mkdir' '-p' '/tmp/athena-inbox'");

    let failed =
        json!({"type": "error", "error": {"ename": "CommandExecError", "evalue": "exit status 1"}});
    h.fake.reply_next(format!("{failed}\n\n"));
    let err = h.sandboxes.stage(&s, "x", vec![]).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "sandbox: creating /tmp/athena-inbox: CommandExecError: exit status 1"
    );
    h.fake.fail_next("/command", 500, "down");
    let err = h.sandboxes.stage(&s, "x", vec![]).await.unwrap_err();
    assert!(err.to_string().contains("HTTP 500"), "{err}");
}

#[tokio::test]
async fn agent_browser_runs_the_cli_with_exactly_the_arguments_given() {
    let h = harness().await;
    let s = h.session("s").await;
    let prefix = format!("'agent-browser' '--session' '{s}' '--content-boundaries'");
    for (args, tail) in [
        (json!(["--help"]), "'--help'"),
        (json!(["skills", "get", "core"]), "'skills' 'get' 'core'"),
        (
            json!(["eval", "document.title", "--stdin"]),
            "'eval' 'document.title' '--stdin'",
        ),
        (
            json!(["fill", "@e3", "it's `id` $(rm -rf /)\n"]),
            "'fill' '@e3' 'it'\\''s `id` $(rm -rf /)\n'",
        ),
    ] {
        let expected = format!("{prefix} {tail}");
        assert_eq!(
            h.call(&s, "agent_browser", json!({ "args": args })).await,
            format!("ran: {expected}")
        );
        let sent = h.fake.requests_to("POST", "/command").pop().unwrap();
        assert_eq!(sent.body["command"], expected);
        assert_eq!(sent.body["timeout"], 90_000);
    }
}

#[tokio::test]
async fn agent_browser_refuses_no_arguments_and_nul_bytes_before_anything_runs() {
    let h = harness().await;
    let s = h.session("s").await;
    let out = h.call(&s, "agent_browser", json!({"args": []})).await;
    assert!(out.contains("args is empty"), "{out}");
    let out = h
        .call(
            &s,
            "agent_browser",
            json!({"args": ["fill", "@e1", "nul\u{0}"]}),
        )
        .await;
    assert!(out.contains("NUL byte"), "{out}");
    assert!(h.fake.requests_to("POST", "/command").is_empty());
}

#[tokio::test]
async fn agent_browser_tells_the_model_to_read_the_guide_and_help_first() {
    let h = harness().await;
    let s = h.session("s").await;
    let (agent, model) = h.agent(vec![MockTurn::text("hi")]);
    h.service.send(&agent, &h.user, &s, "hi").await.unwrap();
    let tool = model.requests()[0]
        .tools
        .iter()
        .find(|t| t.name == "agent_browser")
        .unwrap()
        .clone();
    for needed in [
        "\"skills\", \"get\", \"core\"",
        "\"--help\"",
        "Before your first use",
    ] {
        assert!(tool.description.contains(needed), "{needed}");
    }
    // The guide path the model is sent to is where the image installs it.
    let guide = "/usr/local/share/agent-browser/skill-data";
    assert!(tool.description.contains(&format!("{guide}/core/SKILL.md")));
    assert!(include_str!("../deploy/sandbox-image/Dockerfile").contains(guide));
    assert_eq!(tool.parameters["properties"]["args"]["type"], "array");
    assert_eq!(tool.parameters["required"], json!(["args"]));
}

#[tokio::test]
async fn the_api_key_is_sent_to_the_server_and_its_proxy() {
    let h = harness_with(|c| c.api_key = Some(HeaderValue::from_static("k-123"))).await;
    let s = h.session("s").await;
    h.call(&s, "shell", json!({"command": "true"})).await;
    for request in h.fake.requests() {
        assert_eq!(
            request.headers["open-sandbox-api-key"], "k-123",
            "{}",
            request.path
        );
    }
}

#[tokio::test]
async fn a_sandbox_that_is_still_starting_is_waited_for() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.start_as(&["Pending", "Pending", "Running"]);
    assert_eq!(
        h.call(&s, "shell", json!({"command": "true"})).await,
        "ran: true"
    );
    assert_eq!(h.fake.requests_to("GET", "/v1/sandboxes/sbx-1").len(), 2);
}

#[tokio::test]
async fn a_sandbox_that_fails_to_start_is_deleted_and_reported() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.start_as(&["Pending", "Failed"]);
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(
        out.contains("sandbox sbx-1 is Failed instead of starting"),
        "{out}"
    );
    assert!(h.fake.live().is_empty());
    assert!(h.row(&s).is_none());
}

#[tokio::test]
async fn a_sandbox_that_never_starts_times_out() {
    let h = harness_with(|c| c.startup_timeout = Duration::from_millis(20)).await;
    let s = h.session("s").await;
    h.fake.start_as(&["Pending"]);
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(out.contains("did not start within"), "{out}");
    assert!(h.fake.live().is_empty());
}

#[tokio::test]
async fn a_sandbox_that_vanishes_while_starting_is_reported() {
    let h = harness().await;
    let s = h.session("s").await;
    h.fake.start_as(&["Pending"]);
    h.fake.fail_next("/v1/sandboxes/sbx-1", 404, "{}");
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(
        out.contains("sandbox sbx-1 is gone instead of starting"),
        "{out}"
    );
}

#[tokio::test]
async fn when_another_process_records_a_sandbox_first_that_one_is_used() {
    let h = harness().await;
    let s = h.session("s").await;
    let (store, session) = (h.tmp.open(), s.clone());
    // While our create is in flight, another process records its own.
    h.fake.on_create(move || {
        let theirs = SandboxRow {
            session_id: session,
            sandbox_id: "sbx-theirs".into(),
            bash_session: None,
            code_language: None,
            code_context: None,
            created_at: 0,
            expires_at: 0,
        };
        assert!(store.insert_sandbox(&theirs).unwrap());
    });
    // The fake does not know `sbx-theirs`, so running in it fails; what
    // matters is which sandbox was chosen and that ours was dropped.
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(out.contains("sandbox server"), "{out}");
    assert_eq!(h.row(&s).unwrap().sandbox_id, "sbx-theirs");
    assert!(h.fake.live().is_empty());
    assert_eq!(h.fake.requests_to("DELETE", "/v1/sandboxes/sbx-1").len(), 1);
}

#[tokio::test]
async fn server_errors_are_reported_to_the_model() {
    let h = harness().await;
    let s = h.session("s").await;

    h.fake.fail_next(
        "/v1/sandboxes",
        503,
        "{\"code\":\"BUSY\",\"message\":\"no capacity\"}",
    );
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(
        out.contains("refused to create sandbox: HTTP 503") && out.contains("no capacity"),
        "{out}"
    );

    h.fake.fail_next("/v1/sandboxes", 200, "not json");
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(out.contains("create sandbox: unexpected response"), "{out}");

    // A sandbox exists from here on.
    h.call(&s, "shell", json!({"command": "true"})).await;
    h.fake.fail_next("/renew-expiration", 409, "clock skew");
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(
        out.contains("refused to renew sandbox: HTTP 409: clock skew"),
        "{out}"
    );

    h.fake.reply_next("<html>proxy error</html>\n\n");
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(out.contains("unreadable execd event"), "{out}");

    h.fake.fail_next("/files/upload", 500, "disk full");
    let out = h
        .call(&s, "write_file", json!({"path": "/x", "content": "y"}))
        .await;
    assert!(
        out.contains("refused to upload file: HTTP 500: disk full"),
        "{out}"
    );

    h.fake.fail_next("/session", 500, "no shells");
    h.fake.restart_execd();
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(out.contains("refused to create bash session"), "{out}");
}

#[tokio::test]
async fn execd_endpoints_are_used_as_the_server_gives_them() {
    let h = harness().await;
    let s = h.session("s").await;
    h.call(&s, "shell", json!({"command": "true"})).await;
    let host = h.fake.url.trim_start_matches("http://").to_string();

    // With a scheme, and with headers execd needs.
    h.fake.endpoint_answers(json!({
        "endpoint": format!("{}/v1/sandboxes/sbx-1/proxy/44772", h.fake.url),
        "headers": {"x-execd-token": "t"}
    }));
    h.call(&s, "shell", json!({"command": "true"})).await;
    let run = h.fake.requests_to("POST", "/run").pop().unwrap();
    assert_eq!(run.headers["x-execd-token"], "t");

    for (answer, why) in [
        (json!({"endpoint": "http://[bad"}), "execd endpoint"),
        (
            json!({"endpoint": host, "headers": {"bad header": "x"}}),
            "is not valid",
        ),
        (
            json!({"nothing": true}),
            "get execd endpoint: unexpected response",
        ),
    ] {
        h.fake.endpoint_answers(answer);
        let out = h.call(&s, "shell", json!({"command": "true"})).await;
        assert!(out.contains(why), "{out}");
    }
}

#[tokio::test]
async fn an_unreachable_server_is_reported_not_raised() {
    let h = harness_with(|c| c.url = url::Url::parse("http://127.0.0.1:1").unwrap()).await;
    let s = h.session("s").await;
    let out = h.call(&s, "shell", json!({"command": "true"})).await;
    assert!(out.contains("sandbox server unreachable"), "{out}");
}

#[tokio::test]
async fn a_session_that_does_not_exist_gets_no_sandbox() {
    let h = harness().await;
    let err = h
        .sandboxes
        .shell("no-such-session", "true", Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("no-such-session")),
        "{err}"
    );
    assert!(h.fake.requests().is_empty());
}

#[tokio::test]
async fn release_deletes_the_sandbox_and_the_next_call_starts_fresh() {
    let h = harness().await;
    let s = h.session("s").await;
    // Nothing to release yet.
    h.sandboxes.release(&s).await.unwrap();
    assert!(h.fake.requests().is_empty());

    h.call(&s, "shell", json!({"command": "true"})).await;
    h.sandboxes.release(&s).await.unwrap();
    assert!(h.fake.live().is_empty());
    assert!(h.row(&s).is_none());

    // Already gone on the server counts as released.
    h.call(&s, "shell", json!({"command": "true"})).await;
    h.fake.kill("sbx-2");
    h.sandboxes.release(&s).await.unwrap();
    assert!(h.row(&s).is_none());

    h.call(&s, "shell", json!({"command": "true"})).await;
    h.fake.fail_next("/v1/sandboxes/sbx-3", 500, "stuck");
    let err = h.sandboxes.release(&s).await.unwrap_err();
    assert!(
        err.to_string().contains("refused to delete sandbox"),
        "{err}"
    );
    assert!(h.row(&s).is_some());
}

#[tokio::test]
async fn the_model_is_offered_every_sandbox_tool_only_with_a_server() {
    let h = harness().await;
    let s = h.session("s").await;
    let (agent, model) = h.agent(vec![MockTurn::text("hi")]);
    h.service.send(&agent, &h.user, &s, "hi").await.unwrap();
    let mut tools: Vec<String> = model.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    tools.sort();
    assert_eq!(
        tools,
        [
            "add",
            "agent_browser",
            "read_file",
            "run_code",
            "send_file",
            "send_photo",
            "shell",
            "view_image",
            "write_file"
        ]
    );
    // Every tool describes itself and declares an object schema.
    for tool in &model.requests()[0].tools {
        assert!(!tool.description.is_empty(), "{}", tool.name);
        assert_eq!(tool.parameters["type"], "object", "{}", tool.name);
    }
}

#[tokio::test]
async fn a_streamed_turn_reaches_the_same_sandbox() {
    let h = harness().await;
    let s = h.session("s").await;
    let service = Arc::new(h.service);
    let (agent, _) = {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "call_1",
                    "shell",
                    json!({"command": "echo streamed"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response(usage(1, 1)),
            ],
            streamed_text(&["ok"], usage(1, 1)),
        ]);
        let builder = AgentBuilder::new(model.clone()).memory(service.memory());
        (
            agent::configure_with(builder, Some(h.sandboxes.clone())),
            model,
        )
    };
    let mut turn = service
        .send_stream(Arc::new(agent), &h.user, &s, "go")
        .await
        .unwrap();
    while turn.next().await.is_some() {}
    assert_eq!(
        h.fake.requests_to("POST", "/run")[0].body["command"],
        "echo streamed"
    );
    assert_eq!(h.fake.requests()[0].body["metadata"]["session"], s);
}

// ---------------- tool policy ----------------

/// More calls than the old budget of 40, to show there is no budget.
const MANY_CALLS: usize = 100;

#[tokio::test]
async fn every_tool_call_runs_however_many_a_run_makes() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let user = cli_user(&service).await;
    let s = session(&service, &user, "s").await;
    let calls: Vec<AssistantContent> = (0..MANY_CALLS)
        .map(|i| AssistantContent::tool_call(format!("call_{i}"), "add", json!({"a": 1, "b": i})))
        .collect();
    let (agent, model) = mock_agent(
        &service,
        [MockTurn::from_contents(calls), MockTurn::text("done")],
    );
    service
        .send(&agent, &user, &s.id, "add lots")
        .await
        .unwrap();

    let results = serde_json::to_string(model.requests()[1].chat_history.last().unwrap()).unwrap();
    assert_eq!(results.matches("\"json\"").count(), MANY_CALLS, "{results}");
    assert!(!results.contains("Not run"), "{results}");
}

#[tokio::test]
async fn a_tool_call_with_oversized_arguments_is_skipped() {
    let h = harness().await;
    let s = h.session("s").await;
    let huge = "x".repeat(athena::policy::MAX_ARGUMENT_BYTES);
    let out = h
        .call(&s, "write_file", json!({"path": "/big", "content": huge}))
        .await;
    assert!(out.contains("over the limit"), "{out}");
    // Never reached the sandbox.
    assert!(h.fake.requests().is_empty());
}
