//! A tiny MCP server over stdio for `tests/mcp.rs`: newline-delimited
//! JSON-RPC, written by hand so that the tests check Athena's client against
//! the protocol and not against the library it is built on.
//!
//! Tools (all offered unless `--only a,b` narrows them):
//! - `echo {text}`: `echo: <text>`
//! - `add {a, b}`: the sum. Athena has a tool of the same name.
//! - `getenv {name}`: the value of one of this process's variables
//! - `env_names`: the names of all of them, as a JSON array
//! - `big {bytes}`: that many `x`
//! - `image`: a 1x1 PNG
//! - `fail`: an MCP tool error
//! - `slow`: sleeps a minute
//! - `die`: exits in the middle of the call
//!
//! Flags: `--exit` (exit before the handshake), `--hang` (never answer),
//! `--init-error`, `--list-error`,
//! `--pid-file PATH` (write this process's id there first).

use serde_json::{Value, json};
use std::io::{BufRead, Write};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn value_of<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).map(String::as_str)
}

fn send(message: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{message}").unwrap();
    out.flush().unwrap();
}

fn reply(id: &Value, result: Value) {
    send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn fail(id: &Value, message: &str) {
    send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}}));
}

fn text(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}]})
}

fn tool(name: &str, properties: Value) -> Value {
    json!({
        "name": name,
        "description": format!("the fixture's {name}"),
        "inputSchema": {"type": "object", "properties": properties}
    })
}

fn tools(args: &[String]) -> Vec<Value> {
    let only = value_of(args, "--only");
    [
        tool("echo", json!({"text": {"type": "string"}})),
        tool(
            "add",
            json!({"a": {"type": "number"}, "b": {"type": "number"}}),
        ),
        tool("getenv", json!({"name": {"type": "string"}})),
        tool("env_names", json!({})),
        tool("big", json!({"bytes": {"type": "integer"}})),
        tool("image", json!({})),
        tool("fail", json!({})),
        tool("slow", json!({})),
        tool("die", json!({})),
    ]
    .into_iter()
    .filter(|t| only.is_none_or(|only| only.split(',').any(|n| t["name"] == n)))
    .collect()
}

fn call(name: &str, arguments: &Value) -> Value {
    let number = |key: &str| arguments[key].as_f64().unwrap();
    match name {
        "echo" => text(format!("echo: {}", arguments["text"].as_str().unwrap())),
        "add" => text(format!("fixture add: {}", number("a") + number("b"))),
        "getenv" => {
            let name = arguments["name"].as_str().unwrap();
            text(std::env::var(name).unwrap_or_else(|_| "<unset>".into()))
        }
        "env_names" => {
            // Under coverage the profiler's runtime sets a variable of its
            // own in every process it instruments. That is not the parent's.
            let mut names: Vec<String> = std::env::vars()
                .map(|(name, _)| name)
                .filter(|name| !name.starts_with("__LLVM_PROFILE"))
                .collect();
            names.sort();
            text(json!(names).to_string())
        }
        "big" => text("x".repeat(arguments["bytes"].as_u64().unwrap() as usize)),
        "image" => json!({"content": [{"type": "image", "data": PNG, "mimeType": "image/png"}]}),
        "fail" => json!({"isError": true, "content": [{"type": "text", "text": "boom"}]}),
        "slow" => {
            std::thread::sleep(std::time::Duration::from_secs(60));
            text("late")
        }
        "die" => exit(1),
        other => panic!("no tool {other}"),
    }
}

/// Leave without running the exit handlers: under coverage they would drop a
/// profile file into the directory the tests ran from.
fn exit(code: i32) -> ! {
    // SAFETY: `_exit` only ends this process.
    unsafe { libc::_exit(code) }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(path) = value_of(&args, "--pid-file") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
    }
    if flag(&args, "--exit") {
        exit(3);
    }
    if flag(&args, "--hang") {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    for line in std::io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = &message["id"];
        match message["method"].as_str().unwrap_or_default() {
            "initialize" if flag(&args, "--init-error") => fail(id, "no thanks"),
            "initialize" => reply(
                id,
                json!({
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fixture", "version": "0"}
                }),
            ),
            "tools/list" if flag(&args, "--list-error") => fail(id, "cannot list"),
            "tools/list" => reply(id, json!({"tools": tools(&args)})),
            "tools/call" => {
                let params = &message["params"];
                reply(
                    id,
                    call(params["name"].as_str().unwrap(), &params["arguments"]),
                );
            }
            // Notifications (`initialized`, `cancelled`) get no answer.
            _ => {}
        }
    }
    exit(0);
}
