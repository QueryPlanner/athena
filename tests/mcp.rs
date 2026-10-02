//! Tools from MCP servers, through the real Rig agent loop: real client,
//! real child processes. The stdio server is `tests/mcp/fixture.rs`; the
//! HTTP one is a few lines of axum below. No network.

mod common;

use athena::agent;
use athena::mcp::Mcp;
use athena::media;
use athena::policy::MAX_RESULT_BYTES;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::*;
use rig_agent::agent::AgentBuilder;
use rig_core::completion::CompletionRequest;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIXTURE: &str = env!("CARGO_BIN_EXE_athena-mcp-fixture");

/// A config file in the temp dir, removed on drop.
struct ConfigFile(PathBuf);

impl ConfigFile {
    fn new(config: &Value) -> Self {
        let path = std::env::temp_dir().join(format!("athena-mcp-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, config.to_string()).unwrap();
        Self(path)
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A fixture server entry. `args` are the fixture's flags.
fn fx(args: &[&str]) -> Value {
    json!({"command": FIXTURE, "args": args})
}

/// A path for a fixture's `--pid-file`, deleted on drop.
struct PidFile(PathBuf);

impl PidFile {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("athena-pid-{}", uuid::Uuid::new_v4())))
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }

    /// The id the server wrote as it started. A loaded machine can be slow to
    /// start one, so this waits for it: the deadline only ends a hang.
    fn pid(&self) -> i32 {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&self.0)
                && let Ok(pid) = text.parse()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the server never wrote {}", self.path());
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Whether `pid` is a process, a zombie included: nothing reaped it.
fn exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait until `pid` is gone. The deadline only turns a hang into a failure.
async fn gone(pid: i32) {
    for _ in 0..200 {
        if !exists(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("process {pid} is still there");
}

/// Connect the servers of `config`. The environment Athena has is `vars`
/// plus a `PATH` and a `HOME`.
async fn start(config: Value, vars: &[(&str, &str)]) -> (Mcp, Vec<String>) {
    let file = ConfigFile::new(&config);
    let warnings = Mutex::new(Vec::new());
    let lookup = |name: &str| match name {
        "PATH" => Some("/usr/bin:/bin".to_string()),
        "HOME" => Some("/tmp/athena-test-home".to_string()),
        _ => vars
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string()),
    };
    let mcp = Mcp::start(
        Some(file.0.display().to_string()),
        &lookup,
        &agent::reserved_tool_names(),
        &|w| warnings.lock().unwrap().push(w.to_string()),
    )
    .await;
    (mcp, warnings.into_inner().unwrap())
}

/// One turn on the production agent with `mcp`'s tools, in front of a
/// scripted model that is wrapped like the real provider. Returns the
/// requests the model got.
async fn turn(mcp: &Mcp, turns: Vec<MockTurn>) -> Vec<CompletionRequest> {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let user = cli_user(&service).await;
    let s = session(&service, &user, "s").await;
    let model = MockCompletionModel::new(turns);
    let builder = AgentBuilder::new(media::Vision(model.clone())).memory(service.memory());
    let agent = agent::configure_with_mcp(builder, None, mcp);
    let reply = service.send(&agent, &user, &s.id, "go").await.unwrap();
    assert_eq!(reply.reply, "done");
    model.requests()
}

/// The turns of a model that calls `tool` once and then answers "done".
fn call(tool: &str, args: Value) -> Vec<MockTurn> {
    vec![
        MockTurn::tool_call("call_1", tool, args),
        MockTurn::text("done"),
    ]
}

/// What the tool answered, in the request the model got after the call.
fn answer(requests: &[CompletionRequest]) -> String {
    let last = serde_json::to_value(requests[1].chat_history.last().unwrap()).unwrap();
    assert_eq!(last["content"][0]["type"], "toolresult", "{last}");
    last["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Every block of what the tool answered, as text.
fn answer_blocks(requests: &[CompletionRequest]) -> Vec<String> {
    let last = serde_json::to_value(requests[1].chat_history.last().unwrap()).unwrap();
    last["content"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["text"].as_str().unwrap().to_string())
        .collect()
}

fn offered(request: &CompletionRequest) -> Vec<String> {
    let mut names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
    names.sort();
    names
}

// ---- the tools reach the model and run ----

#[tokio::test]
async fn the_model_calls_an_mcp_tool_and_gets_its_result() {
    let (mcp, warnings) = start(
        json!({"mcpServers": {"fx": fx(&["--only", "echo,image"])}}),
        &[],
    )
    .await;
    assert!(warnings.is_empty(), "{warnings:?}");

    let requests = turn(&mcp, call("echo", json!({"text": "hi"}))).await;

    assert_eq!(answer(&requests), "echo: hi");
    // Athena's own `add` and the server's two tools.
    assert_eq!(offered(&requests[0]), ["add", "echo", "image"]);
    mcp.shutdown().await;
}

#[tokio::test]
async fn an_mcp_tools_image_reaches_the_model_as_an_image() {
    let (mcp, _) = start(json!({"mcpServers": {"fx": fx(&["--only", "image"])}}), &[]).await;

    let requests = turn(&mcp, call("image", json!({}))).await;

    let last = serde_json::to_value(requests[1].chat_history.last().unwrap()).unwrap();
    let content = last["content"].as_array().unwrap();
    assert_eq!(content[0]["content"][0]["text"], "[image follows]");
    assert_eq!(content[1]["text"], "Image from image:");
    assert_eq!(content[2]["type"], "image");
    assert_eq!(content[2]["media_type"], "png");
    mcp.shutdown().await;
}

#[tokio::test]
async fn a_tool_that_reports_an_error_shows_the_model_the_error() {
    let (mcp, _) = start(json!({"mcpServers": {"fx": fx(&["--only", "fail"])}}), &[]).await;

    let requests = turn(&mcp, call("fail", json!({}))).await;

    assert!(answer(&requests).contains("boom"), "{}", answer(&requests));
    mcp.shutdown().await;
}

#[tokio::test]
async fn a_server_that_dies_mid_call_is_an_error_for_the_model_not_for_the_agent() {
    let (mcp, _) = start(json!({"mcpServers": {"fx": fx(&["--only", "die"])}}), &[]).await;

    let requests = turn(&mcp, call("die", json!({}))).await;

    let result = answer(&requests);
    assert!(result.contains("MCP tool 'die' request failed"), "{result}");
    assert!(!result.contains("timed out"), "{result}");
    mcp.shutdown().await;
}

// ---- startup ----

#[tokio::test]
async fn servers_that_fail_to_start_are_warnings_and_the_rest_still_work() {
    let hung = PidFile::new();
    let (mcp, warnings) = start(
        json!({"mcpServers": {
            "a_crash": fx(&["--exit"]),
            "b_missing": {"command": "athena-no-such-command"},
            "c_hang": {"command": FIXTURE, "args": ["--hang", "--pid-file", hung.path()],
                       "startupTimeoutSecs": 2},
            "d_init": fx(&["--init-error"]),
            "e_list": fx(&["--list-error"]),
            "f_good": fx(&["--only", "echo"]),
        }}),
        &[],
    )
    .await;

    assert_eq!(warnings.len(), 5, "{warnings:#?}");
    for (name, why) in [
        ("a_crash", "the handshake failed"),
        ("b_missing", "cannot start `athena-no-such-command`"),
        ("c_hang", "did not start within 2 s"),
        ("d_init", "the handshake failed"),
        ("e_list", "listing its tools failed"),
    ] {
        let w = warnings.iter().find(|w| w.contains(name)).unwrap();
        assert!(w.contains("skipped") && w.contains(why), "{w}");
    }
    assert_eq!(mcp.tool_names().into_iter().collect::<Vec<_>>(), ["echo"]);
    // The server that never answered was killed, not left running.
    gone(hung.pid()).await;

    let requests = turn(&mcp, call("echo", json!({"text": "still here"}))).await;
    assert_eq!(answer(&requests), "echo: still here");
    mcp.shutdown().await;
}

#[tokio::test]
async fn an_unset_variable_skips_its_server_before_anything_is_started() {
    let never = PidFile::new();
    let (mcp, warnings) = start(
        json!({"mcpServers": {"needs_token": {
            "command": FIXTURE,
            "args": ["--pid-file", never.path()],
            "env": {"TOKEN": "${ATHENA_TEST_TOKEN_THAT_IS_NOT_SET}"}
        }}}),
        &[],
    )
    .await;

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("`needs_token` skipped"),
        "{warnings:?}"
    );
    assert!(
        warnings[0].contains("unset environment variable(s): ATHENA_TEST_TOKEN_THAT_IS_NOT_SET"),
        "{warnings:?}"
    );
    assert!(mcp.tool_names().is_empty());
    assert!(!never.0.exists(), "the server was started");
    mcp.shutdown().await;
}

#[tokio::test]
async fn a_variable_reaches_the_server_and_no_log_line_shows_its_value() {
    let (mcp, warnings) = start(
        json!({"mcpServers": {
            "ok": {"command": FIXTURE, "args": ["--only", "getenv"],
                   "env": {"API_TOKEN": "${SECRET_FROM_ENV}"}},
            // Fails, and its warning must not repeat what it was given.
            "bad": {"command": "athena-no-such-command", "env": {"API_TOKEN": "${SECRET_FROM_ENV}"}},
        }}),
        &[("SECRET_FROM_ENV", "s3cret-value-123")],
    )
    .await;

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        !warnings.join("\n").contains("s3cret-value-123"),
        "{warnings:?}"
    );
    let requests = turn(&mcp, call("getenv", json!({"name": "API_TOKEN"}))).await;
    assert_eq!(answer(&requests), "s3cret-value-123");
    mcp.shutdown().await;
}

#[tokio::test]
async fn a_server_gets_its_declared_variables_plus_path_and_home_and_nothing_else() {
    let (mcp, _) = start(
        json!({"mcpServers": {"fx": {
            "command": FIXTURE,
            "args": ["--only", "env_names"],
            "env": {"DECLARED": "1"}
        }}}),
        // Athena itself has this one; the child must not.
        &[("OPENROUTER_API_KEY", "key-that-must-not-leak")],
    )
    .await;

    let requests = turn(&mcp, call("env_names", json!({}))).await;
    let names: Vec<String> = serde_json::from_str(&answer(&requests)).unwrap();
    assert_eq!(names, ["DECLARED", "HOME", "PATH"]);
    mcp.shutdown().await;
}

// ---- names ----

#[tokio::test]
async fn a_name_a_built_in_or_an_earlier_server_has_is_skipped_with_a_warning() {
    let (mcp, warnings) = start(
        json!({"mcpServers": {
            // `add` is Athena's own.
            "a": fx(&["--only", "echo,add,env_names"]),
            // `echo` is already server a's.
            "b": fx(&["--only", "echo,big"]),
        }}),
        &[],
    )
    .await;

    assert_eq!(warnings.len(), 2, "{warnings:#?}");
    assert!(
        warnings[0].contains("`a`: tool `add` skipped: the name belongs to a built-in tool"),
        "{warnings:?}"
    );
    assert!(
        warnings[1].contains("`b`: tool `echo` skipped: the name belongs to server `a`"),
        "{warnings:?}"
    );

    // Athena's `add` answers, not the server's.
    let requests = turn(&mcp, call("add", json!({"a": 1, "b": 2}))).await;
    let sent = serde_json::to_string(requests[1].chat_history.last().unwrap()).unwrap();
    assert!(
        sent.contains('3') && !sent.contains("fixture add"),
        "{sent}"
    );
    assert_eq!(
        offered(&requests[0]),
        ["add", "big", "echo", "env_names"],
        "each name once"
    );
    mcp.shutdown().await;
}

// ---- limits ----

#[tokio::test]
async fn tool_policy_covers_mcp_tools_arguments_and_results() {
    let (mcp, _) = start(
        json!({"mcpServers": {"fx": fx(&["--only", "echo,big"])}}),
        &[],
    )
    .await;

    // Arguments over the limit are not sent to the server.
    let text = "a".repeat(athena::policy::MAX_ARGUMENT_BYTES + 1);
    let requests = turn(&mcp, call("echo", json!({"text": text}))).await;
    assert!(
        answer(&requests).starts_with("Not run: the arguments are"),
        "{}",
        answer(&requests)
    );

    // A result over the limit is cut to the limit, and one notice follows.
    let requests = turn(&mcp, call("big", json!({"bytes": 3 * MAX_RESULT_BYTES}))).await;
    let blocks = answer_blocks(&requests);
    assert_eq!(blocks.len(), 2, "the text, then the notice");
    assert_eq!(blocks[0], "x".repeat(MAX_RESULT_BYTES));
    assert_eq!(blocks[1], "[cut: the result is over 65536 bytes]");

    // One within it is not touched.
    let requests = turn(&mcp, call("big", json!({"bytes": 1000}))).await;
    assert_eq!(answer(&requests), "x".repeat(1000));
    mcp.shutdown().await;
}

#[tokio::test]
async fn a_slow_tool_times_out_with_an_error_the_model_sees_and_shutdown_kills_the_server() {
    let pid = PidFile::new();
    let (mcp, _) = start(
        json!({"mcpServers": {"slow": {
            "command": FIXTURE,
            "args": ["--only", "slow", "--pid-file", pid.path()],
            "timeoutSecs": 1
        }}}),
        &[],
    )
    .await;

    let requests = turn(&mcp, call("slow", json!({}))).await;

    let result = answer(&requests);
    assert!(
        result.contains("MCP tool 'slow' timed out after 1s"),
        "{result}"
    );
    // The server is still inside the call. Shutdown ends it, and reaps it.
    assert!(exists(pid.pid()));
    mcp.shutdown().await;
    assert!(
        !exists(pid.pid()),
        "killed but not reaped, or still running"
    );
}

// ---- processes ----

#[tokio::test]
async fn shutdown_ends_every_server_and_leaves_no_zombie() {
    let pids = [PidFile::new(), PidFile::new()];
    let (mcp, _) = start(
        json!({"mcpServers": {
            "one": {"command": FIXTURE, "args": ["--only", "echo", "--pid-file", pids[0].path()]},
            "two": {"command": FIXTURE, "args": ["--only", "big", "--pid-file", pids[1].path()]},
        }}),
        &[],
    )
    .await;
    assert!(pids.iter().all(|p| exists(p.pid())));

    mcp.shutdown().await;

    for pid in &pids {
        assert!(!exists(pid.pid()), "still there, or a zombie");
    }
}

#[tokio::test]
async fn dropping_the_connections_kills_the_servers_too() {
    let pid = PidFile::new();
    let (mcp, _) = start(
        json!({"mcpServers": {"fx": {"command": FIXTURE, "args": ["--pid-file", pid.path()]}}}),
        &[],
    )
    .await;
    assert!(exists(pid.pid()));

    drop(mcp);

    gone(pid.pid()).await;
}

// ---- configuration ----

#[tokio::test]
async fn without_a_config_the_agent_is_what_it_was() {
    let (service_tmp, plain) = (TempDb::new(), agent::configure as fn(_) -> _);
    let (service, _) = service_tmp.service();
    let model = MockCompletionModel::new([MockTurn::text("done")]);
    let agent = plain(AgentBuilder::new(model.clone()).memory(service.memory()));
    let user = cli_user(&service).await;
    let s = session(&service, &user, "s").await;
    service.send(&agent, &user, &s.id, "go").await.unwrap();
    let unconfigured = offered(&model.requests()[0]);
    assert_eq!(unconfigured, ["add"]);

    // No path, an empty list of servers and no MCP at all are all the same.
    let lookup = |_: &str| None;
    let unset = Mcp::start(None, &lookup, &[], &|w| panic!("{w}")).await;
    let (empty, warnings) = start(json!({"mcpServers": {}}), &[]).await;
    assert!(warnings.is_empty());
    for mcp in [unset, empty, Mcp::none()] {
        let requests = turn_text_only(&mcp).await;
        assert_eq!(offered(&requests[0]), unconfigured);
        mcp.shutdown().await;
    }
}

async fn turn_text_only(mcp: &Mcp) -> Vec<CompletionRequest> {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let user = cli_user(&service).await;
    let s = session(&service, &user, "s").await;
    let model = MockCompletionModel::new([MockTurn::text("done")]);
    let builder = AgentBuilder::new(model.clone()).memory(service.memory());
    let agent = agent::configure_with_mcp(builder, None, mcp);
    service.send(&agent, &user, &s.id, "go").await.unwrap();
    model.requests()
}

#[tokio::test]
async fn the_cli_connects_servers_only_when_it_builds_an_agent() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let cell = tokio::sync::OnceCell::new();

    // No key: the client fails first, and no server is started.
    let missing = agent::build_on_demand(
        || anyhow::bail!("no key"),
        "m",
        service.memory(),
        &cell,
        &|_| (),
    )
    .await;
    assert_eq!(missing.err().unwrap().to_string(), "no key");
    assert!(!cell.initialized());

    let built = agent::build_on_demand(
        || Ok(agent::Client::new("unused-key")?),
        "m",
        service.memory(),
        &cell,
        &|_| (),
    )
    .await;
    assert!(built.is_ok());
    assert!(cell.initialized());

    agent::shutdown_on_demand(cell).await;
    agent::shutdown_on_demand(tokio::sync::OnceCell::new()).await;
}

// ---- HTTP ----

type Seen = Arc<Mutex<Vec<String>>>;

/// The fewest lines of a streamable-HTTP MCP server: JSON replies, one tool
/// `remote_echo`, and a record of the `Authorization` header of every request.
async fn http_server() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route(
            "/mcp",
            post(rpc).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

async fn rpc(State(seen): State<Seen>, headers: HeaderMap, Json(message): Json<Value>) -> Response {
    let auth = headers
        .get("authorization")
        .map_or("none", |v| v.to_str().unwrap());
    seen.lock().unwrap().push(auth.to_string());
    let id = &message["id"];
    let result = match message["method"].as_str().unwrap() {
        "initialize" => json!({
            "protocolVersion": message["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "http-fixture", "version": "0"}
        }),
        "tools/list" => json!({"tools": [{
            "name": "remote_echo",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
        }]}),
        "tools/call" => json!({"content": [{
            "type": "text",
            "text": format!("remote: {}", message["params"]["arguments"]["text"].as_str().unwrap())
        }]}),
        // A notification: acknowledged, nothing to say.
        _ => return StatusCode::ACCEPTED.into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

#[tokio::test]
async fn an_http_server_gets_its_headers_with_variables_expanded_and_its_tool_runs() {
    let (url, seen) = http_server().await;
    let (mcp, warnings) = start(
        json!({"mcpServers": {"remote": {
            "url": url,
            "headers": {"Authorization": "Bearer ${REMOTE_TOKEN}"}
        }}}),
        &[("REMOTE_TOKEN", "tok-123")],
    )
    .await;
    assert!(warnings.is_empty(), "{warnings:?}");

    let requests = turn(&mcp, call("remote_echo", json!({"text": "hi"}))).await;

    assert_eq!(answer(&requests), "remote: hi");
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 4,
        "initialize, initialized, list, call: {seen:?}"
    );
    assert!(seen.iter().all(|a| a == "Bearer tok-123"), "{seen:?}");
    mcp.shutdown().await;
}

#[tokio::test]
async fn an_http_server_that_is_down_or_misconfigured_is_a_warning() {
    // A port nothing listens on, and a key in the URL that must not be shown.
    let closed = "http://127.0.0.1:1/mcp/URLSECRET?key=QUERYSECRET";
    let (mcp, warnings) = start(
        json!({"mcpServers": {
            "down": {"url": closed, "startupTimeoutSecs": 10},
            "badheader": {"url": "http://127.0.0.1:1/mcp", "headers": {"bad name": "v"}},
        }}),
        &[],
    )
    .await;

    assert_eq!(warnings.len(), 2, "{warnings:#?}");
    assert!(
        warnings.iter().any(|w| w.contains("`down` skipped")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("`badheader` skipped") && w.contains("not a valid header name")),
        "{warnings:?}"
    );
    assert!(mcp.tool_names().is_empty());
    let all = warnings.join("\n");
    assert!(
        !all.contains("URLSECRET") && !all.contains("QUERYSECRET"),
        "{all}"
    );
    mcp.shutdown().await;
}
