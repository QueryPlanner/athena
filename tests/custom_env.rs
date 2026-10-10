//! `agent::build_with`, which production uses, reads `ATHENA_INSTRUCTIONS` and
//! `ATHENA_SKILLS_DIR` and puts them in the request the provider receives,
//! next to the tools of an MCP server (`ATHENA_MCP_CONFIG` shape) and the
//! parameter that switches OpenRouter's context compression off.
//!
//! This is the only test in its binary because it sets environment
//! variables, which is sound only while no other thread reads them.

mod common;

use athena::agent;
use athena::mcp::Mcp;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use common::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<Value>>>;

/// A stand-in for OpenRouter's chat completions that records each request
/// body and always answers "ok".
async fn fake_openrouter() -> (String, Seen) {
    async fn complete(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
        seen.lock().unwrap().push(body);
        Json(json!({
            "id": "gen-1",
            "object": "chat.completion",
            "created": 0,
            "model": "m",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
    }
    let seen = Seen::default();
    let app = Router::new()
        .route("/chat/completions", post(complete))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, seen)
}

#[test]
fn the_production_build_sends_the_environments_instructions_and_skills() {
    let dir = WorkDir::new();
    let skill_dir = dir.path().join("skills/house-style");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: house-style\ndescription: How we write.\n---\nBe terse.\n",
    )
    .unwrap();
    let instructions = dir.path().join("instructions.md");
    std::fs::write(&instructions, "Call the user Boss.").unwrap();
    // One MCP server, a test program that offers `echo`. Its tool is
    // `add` as well, which the agent's own `add` must keep.
    let mcp_config = dir.path().join("mcp.json");
    let fixture = env!("CARGO_BIN_EXE_athena-mcp-fixture");
    let servers =
        json!({"mcpServers": {"fx": {"command": fixture, "args": ["--only", "echo,add"]}}});
    std::fs::write(&mcp_config, servers.to_string()).unwrap();
    // SAFETY: this is the only test in the binary and nothing has started a
    // thread yet that could be reading the environment.
    unsafe {
        std::env::set_var("ATHENA_INSTRUCTIONS", &instructions);
        std::env::set_var("ATHENA_SKILLS_DIR", dir.path().join("skills"));
    }

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let seen = runtime.block_on(async {
        let (url, seen) = fake_openrouter().await;
        let client = agent::Client::builder()
            .api_key("unused-key")
            .base_url(&url)
            .build()
            .unwrap();
        let tmp = TempDb::new();
        let (service, _) = tmp.service();
        let user = cli_user(&service).await;
        let s = session(&service, &user, "s").await;
        let warnings = Mutex::new(Vec::<String>::new());
        let mcp = Mcp::start(
            Some(mcp_config.display().to_string()),
            &|name| std::env::var(name).ok(),
            &agent::reserved_tool_names(),
            &|w| warnings.lock().unwrap().push(w.to_string()),
        )
        .await;
        // Only the clash with the agent's own `add` is worth a warning.
        let said = warnings.lock().unwrap().clone();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("tool `add` skipped"), "{said:?}");
        let agent = agent::build_with(&client, "m", service.memory(), None, None, None, &mcp);
        service.send(&agent, &user, &s.id, "hi").await.unwrap();
        mcp.shutdown().await;
        seen
    });
    let body = seen.lock().unwrap()[0].clone();

    let system = &body["messages"][0];
    assert_eq!(system["role"], "system", "{body}");
    // Rig sends message content as a list of text parts.
    let parts = system["content"].as_array().unwrap();
    let prompt: String = parts.iter().map(|p| p["text"].as_str().unwrap()).collect();
    assert!(prompt.starts_with(agent::PREAMBLE));
    assert!(prompt.contains("## Custom instructions\n\nCall the user Boss."));
    // The owner's skills are listed; the user's own skills come after them,
    // only as a pointer to the skill tools.
    let owners = prompt
        .strip_suffix(athena::user_skills::PREAMBLE)
        .expect("the user-skills section ends the prompt");
    assert!(owners.ends_with("- house-style: How we write."), "{prompt}");
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    // Instructions, skills, native calorie, workout, time and reminder tools
    // and MCP tools coexist.
    let mut sorted = tools.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        [
            "add",
            "calorie_history",
            "calorie_log",
            "calorie_remove",
            "calorie_summary",
            "calorie_update",
            "echo",
            "exercise_progress",
            "health_status",
            "health_summary",
            "health_sync_now",
            "now",
            "read_skill",
            "reminder_cancel",
            "reminder_confirm",
            "reminder_create",
            "reminder_list",
            "skill_confirm",
            "skill_create",
            "skill_install",
            "skill_list",
            "skill_read",
            "skill_remove",
            "timezone_set",
            "workout_history",
            "workout_last",
            "workout_log",
            "workout_next",
            "workout_remove",
            "workout_update"
        ],
        "{tools:?}"
    );
    // And OpenRouter's own context compression is off, in the same request.
    assert_eq!(
        body["plugins"],
        json!([{"id": "context-compression", "enabled": false}]),
        "{body}"
    );
}
