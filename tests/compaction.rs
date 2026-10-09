//! Compaction through the real provider code: Rig's OpenRouter client, the
//! production agent and the service, against a fake OpenRouter on a local
//! port. The unit tests in `src/compaction` drive the hook with Rig's mock
//! model; this checks what goes over the wire, and that the usage a real
//! (blocking or streamed) response carries is what compaction acts on.

mod common;

use athena::agent::{self, Client};
use athena::compaction::{Compactor, Settings};
use athena::mcp::Mcp;
use athena::policy::MAX_RESULT_BYTES;
use athena::service::{Service, TurnEvent};
use athena::store::Store;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::*;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const MAIN: &str = "test/main";
const SUMMARY_MODEL: &str = "test/summary";

/// What the agent's model says next: "OK", or a call to the `add` tool, and
/// the prompt size it reports for the request it answers.
#[derive(Clone, Copy)]
struct Reply {
    /// The tool it calls, and the JSON text of the arguments.
    tool_call: Option<(&'static str, &'static str)>,
    prompt_tokens: u64,
}

fn says_ok(prompt_tokens: &[u64]) -> Vec<Reply> {
    prompt_tokens
        .iter()
        .map(|&prompt_tokens| Reply {
            tool_call: None,
            prompt_tokens,
        })
        .collect()
}

fn calls_add(prompt_tokens: u64) -> Reply {
    calls("add", "{\"a\":1,\"b\":2}", prompt_tokens)
}

fn calls(tool: &'static str, arguments: &'static str, prompt_tokens: u64) -> Reply {
    Reply {
        tool_call: Some((tool, arguments)),
        prompt_tokens,
    }
}

/// A fake OpenRouter chat endpoint. The agent's model follows a script; the
/// summary model answers with a summary. Everything it was sent is kept.
struct Fake {
    bodies: Mutex<Vec<Value>>,
    script: Mutex<VecDeque<Reply>>,
}

impl Fake {
    async fn start(script: Vec<Reply>) -> (Arc<Self>, String) {
        let fake = Arc::new(Self {
            bodies: Mutex::default(),
            script: Mutex::new(script.into()),
        });
        let app = Router::new()
            .route("/chat/completions", post(chat))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (fake, url)
    }

    fn sent_to(&self, model: &str) -> Vec<Value> {
        let bodies = self.bodies.lock().unwrap();
        bodies
            .iter()
            .filter(|b| b["model"] == model)
            .cloned()
            .collect()
    }
}

/// One SSE message, as OpenRouter sends them.
fn sse(chunk: Value) -> String {
    format!("data: {chunk}\n\n")
}

async fn chat(State(fake): State<Arc<Fake>>, Json(body): Json<Value>) -> Response {
    let call = {
        let mut bodies = fake.bodies.lock().unwrap();
        bodies.push(body.clone());
        bodies.len()
    };
    let reply = if body["model"] == SUMMARY_MODEL {
        Reply {
            tool_call: None,
            prompt_tokens: 4_000,
        }
    } else {
        fake.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted reply")
    };
    let text = if body["model"] == SUMMARY_MODEL {
        "SUMMARY: the code word is LYNX-2211"
    } else {
        "OK"
    };
    let usage = json!({
        "prompt_tokens": reply.prompt_tokens, "completion_tokens": 5,
        "total_tokens": reply.prompt_tokens + 5
    });
    let tool_calls = json!([{
        "id": format!("call_{call}"), "type": "function",
        "function": {"name": reply.tool_call.map_or("", |c| c.0),
                     "arguments": reply.tool_call.map_or("", |c| c.1)}
    }]);
    let model = body["model"].clone();
    if body["stream"] == true {
        let chunk = |choices: Value, usage: Value| {
            let mut chunk = json!({
                "id": "gen-1", "object": "chat.completion.chunk", "created": 0,
                "model": model, "choices": choices,
            });
            if !usage.is_null() {
                chunk["usage"] = usage;
            }
            sse(chunk)
        };
        let (delta, finish) = if reply.tool_call.is_some() {
            let mut call = tool_calls[0].clone();
            call["index"] = json!(0);
            (
                json!({"role": "assistant", "tool_calls": [call]}),
                "tool_calls",
            )
        } else {
            (json!({"role": "assistant", "content": text}), "stop")
        };
        // As OpenRouter does: the content, the finish reason, then the usage
        // in a last message that has no choices.
        let body = chunk(
            json!([{"index": 0, "delta": delta, "finish_reason": null}]),
            Value::Null,
        ) + &chunk(
            json!([{"index": 0, "delta": {}, "finish_reason": finish}]),
            Value::Null,
        ) + &chunk(json!([]), usage)
            + "data: [DONE]\n\n";
        return ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response();
    }
    let message = if reply.tool_call.is_some() {
        json!({"role": "assistant", "content": null, "tool_calls": tool_calls})
    } else {
        json!({"role": "assistant", "content": text})
    };
    Json(json!({
        "id": "gen-1", "object": "chat.completion", "created": 0, "model": model,
        "choices": [{"index": 0, "message": message,
                     "finish_reason": if reply.tool_call.is_some() { "tool_calls" } else { "stop" }}],
        "usage": usage,
    }))
    .into_response()
}

fn client(base: &str) -> Client {
    Client::builder()
        .api_key("test-key")
        .base_url(base)
        .build()
        .unwrap()
}

/// About `tokens` tokens of text starting with `label`.
fn words(label: &str, tokens: usize) -> String {
    format!("{label} {}", "w".repeat(tokens * 4))
}

fn compaction_for(client: &Client, window: u64) -> Compactor {
    let settings = Settings {
        compact_at: 0.8,
        model: SUMMARY_MODEL.into(),
        context_tokens: Some(window),
    };
    Compactor::openrouter(client, settings, MAIN)
}

/// Whether any message of the request says `needle`.
fn mentions(body: &Value, needle: &str) -> bool {
    body["messages"].to_string().contains(needle)
}

/// Every tool message of the request answers a call of the assistant message
/// before it, and no tool call is left without its answer: what a provider
/// checks, on the wire's own shape.
fn assert_wire_pairs(body: &Value) {
    let messages = body["messages"].as_array().unwrap();
    let mut open: Vec<String> = Vec::new();
    for message in messages {
        match message["role"].as_str().unwrap() {
            "assistant" => {
                assert!(open.is_empty(), "calls without answers: {open:?}");
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    open.push(call["id"].as_str().unwrap().to_string());
                }
            }
            "tool" => {
                let id = message["tool_call_id"].as_str().unwrap();
                let at = open.iter().position(|open| open == id);
                assert!(at.is_some(), "a tool message with no call: {id}");
                open.remove(at.unwrap());
            }
            _ => assert!(open.is_empty(), "calls without answers: {open:?}"),
        }
    }
    assert!(open.is_empty(), "calls without answers: {open:?}");
}

/// A service on a temp database, compaction on, and its agent on the fake.
struct Setup {
    tmp: TempDb,
    service: Arc<Service>,
    agent: Arc<rig_agent::agent::Agent>,
    fake: Arc<Fake>,
}

async fn setup(script: Vec<Reply>) -> Setup {
    setup_with(script, 10_000, &Mcp::none()).await
}

/// [`setup`] with a window of `window` tokens and the tools of `mcp`, built
/// the way production builds its agent.
async fn setup_with(script: Vec<Reply>, window: u64, mcp: &Mcp) -> Setup {
    let (fake, base) = Fake::start(script).await;
    let client = client(&base);
    let tmp = TempDb::new();
    let store = Store::open(tmp.path()).unwrap();
    let service = Arc::new(
        Service::new(store, MAIN, |_| {}).with_compactor(Some(compaction_for(&client, window))),
    );
    let agent = Arc::new(agent::build_with(
        &client,
        MAIN,
        service.memory(),
        None,
        None,
        mcp,
    ));
    Setup {
        tmp,
        service,
        agent,
        fake,
    }
}

/// Every model request must switch OpenRouter's own context-compression
/// plugin off, whichever way it was sent. The summary request is a bare
/// completion and does not carry it.
fn assert_plugin_off(fake: &Fake) {
    let main = fake.sent_to(MAIN);
    assert!(!main.is_empty());
    for body in main {
        assert_eq!(
            body["plugins"],
            json!([{"id": "context-compression", "enabled": false}])
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_nears_the_window_is_summarized_over_the_blocking_wire() {
    // The third request reports 8 900 prompt tokens: over 80% of 10 000.
    let t = setup(says_ok(&[1_000, 2_500, 8_900, 3_000])).await;
    let user = cli_user(&t.service).await;
    let session = session(&t.service, &user, "chat").await;
    for n in 1..=3 {
        let text = words(&format!("U{n}"), 1_000);
        t.service
            .send(&*t.agent, &user, &session.id, text)
            .await
            .unwrap();
    }
    assert!(t.fake.sent_to(SUMMARY_MODEL).is_empty());

    let turn = t
        .service
        .send(&*t.agent, &user, &session.id, "What was the code word?")
        .await
        .unwrap();

    assert_eq!(turn.reply, "OK");
    // One summary request, to the summary model, of the oldest messages.
    let summaries = t.fake.sent_to(SUMMARY_MODEL);
    assert_eq!(summaries.len(), 1);
    assert!(mentions(&summaries[0], "User: U1 "));
    assert!(!mentions(&summaries[0], "U3"));
    // Capped at a tenth of the 10 000-token window, on the wire.
    assert_eq!(summaries[0]["max_tokens"], 1_000);
    // The agent's fourth request carries the summary instead of them.
    let fourth = t.fake.sent_to(MAIN).pop().unwrap();
    assert!(mentions(&fourth, "the code word is LYNX-2211"));
    assert!(!mentions(&fourth, "U1 "));
    assert!(mentions(&fourth, "U2 ") && mentions(&fourth, "U3 "));
    assert_plugin_off(&t.fake);
    // And it is a checkpoint, in a session that still has every row.
    let db = t.tmp.raw();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM compactions"), 1);
    // The summary covers U1 and its reply, rows 0 and 1; U2 on is kept.
    assert_eq!(count(&db, "SELECT through_seq FROM compactions"), 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), 8);
    assert_eq!(
        count(
            &db,
            "SELECT input_tokens FROM runs WHERE model = 'test/summary'"
        ),
        4_000
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_loop_that_outgrows_the_window_keeps_every_call_with_its_result_on_the_wire() {
    // Three big turns, then one that calls a tool twice. The second call's
    // request reports 8 900 tokens, so the third request is compacted.
    let mut script = says_ok(&[1_000, 2_500, 3_800]);
    script.extend([calls_add(4_000), calls_add(8_900)]);
    script.extend(says_ok(&[3_000]));
    let t = setup(script).await;
    let user = cli_user(&t.service).await;
    let session = session(&t.service, &user, "chat").await;
    for n in 1..=3 {
        let text = words(&format!("U{n}"), 1_000);
        t.service
            .send(&*t.agent, &user, &session.id, text)
            .await
            .unwrap();
    }

    let turn = t
        .service
        .send(&*t.agent, &user, &session.id, "go")
        .await
        .unwrap();

    assert_eq!(turn.reply, "OK");
    assert_eq!(t.fake.sent_to(SUMMARY_MODEL).len(), 1);
    let main = t.fake.sent_to(MAIN);
    for body in &main {
        assert_wire_pairs(body);
    }
    // The last request: the summary, then what was kept, ending with the
    // second call and its result.
    let roles: Vec<&str> = main[main.len() - 1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        [
            "system",
            "user",
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant",
            "tool",
            "assistant",
            "tool"
        ]
    );
    assert!(mentions(
        &main[main.len() - 1],
        "the code word is LYNX-2211"
    ));
    assert!(!mentions(&main[main.len() - 1], "U1 "));
    assert_plugin_off(&t.fake);
    assert_eq!(count(&t.tmp.raw(), "SELECT COUNT(*) FROM compactions"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_size_a_streamed_response_reports_is_what_compaction_acts_on() {
    let t = setup(says_ok(&[1_000, 2_500, 8_900, 3_000])).await;
    let user = cli_user(&t.service).await;
    let session = session(&t.service, &user, "chat").await;
    let turns = [
        words("U1", 1_000),
        words("U2", 1_000),
        words("U3", 1_000),
        "What was the code word?".to_string(),
    ];
    for text in &turns {
        let mut stream = t
            .service
            .send_stream(t.agent.clone(), &user, &session.id, text)
            .await
            .unwrap();
        let mut done = None;
        while let Some(event) = stream.next().await {
            if let TurnEvent::Done(outcome) = event {
                done = Some(outcome.unwrap());
            }
        }
        assert_eq!(done.unwrap().reply, "OK");
    }

    assert_eq!(t.fake.sent_to(SUMMARY_MODEL).len(), 1);
    let fourth = t.fake.sent_to(MAIN).pop().unwrap();
    assert_eq!(fourth["stream"], true);
    assert!(mentions(&fourth, "the code word is LYNX-2211"));
    assert!(!mentions(&fourth, "U1 "));
    assert_plugin_off(&t.fake);
    assert_eq!(count(&t.tmp.raw(), "SELECT COUNT(*) FROM compactions"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_tool_loop_is_compacted_with_its_calls_and_results_together() {
    let mut script = says_ok(&[1_000, 2_500, 3_800]);
    script.extend([calls_add(4_000), calls_add(8_900)]);
    script.extend(says_ok(&[3_000]));
    let t = setup(script).await;
    let user = cli_user(&t.service).await;
    let session = session(&t.service, &user, "chat").await;
    for text in [
        words("U1", 1_000),
        words("U2", 1_000),
        words("U3", 1_000),
        "go".to_string(),
    ] {
        let mut stream = t
            .service
            .send_stream(t.agent.clone(), &user, &session.id, &text)
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            if let TurnEvent::Done(outcome) = event {
                outcome.unwrap();
            }
        }
    }

    assert_eq!(t.fake.sent_to(SUMMARY_MODEL).len(), 1);
    let main = t.fake.sent_to(MAIN);
    for body in &main {
        assert_wire_pairs(body);
    }
    assert!(mentions(
        &main[main.len() - 1],
        "the code word is LYNX-2211"
    ));
    assert_eq!(count(&t.tmp.raw(), "SELECT COUNT(*) FROM compactions"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_stays_small_never_asks_for_a_summary() {
    let t = setup(says_ok(&[1_000, 1_100])).await;
    let user = cli_user(&t.service).await;
    let session = session(&t.service, &user, "chat").await;
    for text in ["hello", "hello again"] {
        t.service
            .send(&*t.agent, &user, &session.id, text)
            .await
            .unwrap();
    }

    assert!(t.fake.sent_to(SUMMARY_MODEL).is_empty());
    assert_plugin_off(&t.fake);
    assert_eq!(count(&t.tmp.raw(), "SELECT COUNT(*) FROM compactions"), 0);
}

/// The `big` tool of the MCP test server, on the production agent: its
/// results are cut by `ToolPolicy` to 64 KiB, and compaction carries on
/// around them.
#[tokio::test(flavor = "multi_thread")]
async fn large_truncated_mcp_results_are_clipped_for_the_summarizer_and_never_orphaned() {
    let config = std::env::temp_dir().join(format!("athena-mcp-{}.json", uuid::Uuid::new_v4()));
    let servers = json!({"mcpServers": {"fx": {
        "command": env!("CARGO_BIN_EXE_athena-mcp-fixture"), "args": ["--only", "big"]
    }}});
    std::fs::write(&config, servers.to_string()).unwrap();
    let mcp = Mcp::start(
        Some(config.display().to_string()),
        &|name| (name == "PATH").then(|| "/usr/bin:/bin".to_string()),
        &agent::reserved_tool_names(),
        &|w| panic!("unexpected warning: {w}"),
    )
    .await;
    std::fs::remove_file(&config).unwrap();
    // A 40 000-token window: compaction above 32 000, 8 000 kept word for
    // word. Each cut result is about 16 400 tokens. Three turns of 6 000
    // tokens, then one that calls `big` twice.
    let big = r#"{"bytes":300000}"#;
    let mut script = says_ok(&[6_000, 12_000, 18_000]);
    script.extend([
        // The second request of the turn is 34 000 by our count: compacted.
        calls("big", big, 18_100),
        // After that one the request is about 21 000; the next result makes it
        // 37 000: compacted again, and now the first result is summarized.
        calls("big", big, 21_000),
    ]);
    script.extend(says_ok(&[21_500]));
    let t = setup_with(script, 40_000, &mcp).await;
    let user = cli_user(&t.service).await;
    let session = session(&t.service, &user, "chat").await;
    for n in 1..=3 {
        let text = words(&format!("U{n}"), 6_000);
        t.service
            .send(&*t.agent, &user, &session.id, text)
            .await
            .unwrap();
    }

    let turn = t
        .service
        .send(&*t.agent, &user, &session.id, "go")
        .await
        .unwrap();
    mcp.shutdown().await;

    assert_eq!(turn.reply, "OK");
    let main = t.fake.sent_to(MAIN);
    assert_eq!(main.len(), 6);
    // Every request has the plugin switched off and the MCP tool offered,
    // and no tool message ever lacks its call.
    assert_plugin_off(&t.fake);
    for body in &main {
        assert_wire_pairs(body);
        let tools = body["tools"].to_string();
        assert!(
            tools.contains("\"big\"") && tools.contains("\"add\""),
            "{tools}"
        );
    }
    // The cut result, with the policy's notice, is what the model sees.
    let cut = "x".repeat(MAX_RESULT_BYTES);
    assert!(
        mentions(&main[4], &cut),
        "the first result reaches the model"
    );
    assert!(mentions(&main[4], "[cut: the result is over 65536 bytes]"));
    // Two summaries. The first was written from the three turns and "go", and
    // the first result is not in it; the second was written from the first
    // summary, the first call and its result, clipped to what a summarizer is
    // shown.
    let summaries = t.fake.sent_to(SUMMARY_MODEL);
    assert_eq!(summaries.len(), 2);
    assert!(mentions(&summaries[0], "User: U3 ") && !mentions(&summaries[0], "[called big"));
    let second = summaries[1]["messages"].to_string();
    assert!(second.contains("[called big"));
    assert!(second.contains("characters left out"));
    assert!(second.contains("untrusted tool output"));
    assert!(
        second.len() < 30_000,
        "a 65 536 byte result was clipped: {} bytes",
        second.len()
    );
    // The last request starts from the second summary and the second call.
    let roles: Vec<&str> = main[5]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    // Only the newest checkpoint is kept, and it covers the first result: the
    // rows are U1..A3 (0 to 5), go (6), call (7), result (8).
    let db = t.tmp.raw();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM compactions"), 1);
    assert_eq!(count(&db, "SELECT through_seq FROM compactions"), 8);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), 12);
}
