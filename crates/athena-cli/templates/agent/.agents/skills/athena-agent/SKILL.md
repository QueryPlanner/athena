---
name: athena-agent
description: Change this Athena agent — its instructions, its own tools, which runtime tools it gets, its model and limits — and test the change. Use when asked to add a tool, change what the agent says or does, or restrict what it can do.
---

# Changing this agent

Everything that makes this agent itself is `spec()` in `src/agent.rs`.
The runtime (`athena-core`) does the rest.

## Change its instructions

Edit `prompts/system.md`. It is compiled in with `include_str!`, so a rebuild
picks it up. Keep it short and specific; the model reads it on every turn.

## Add a tool

```rust
/// What the model sees is the description and the argument names.
#[rig_tool(description = "Look up a word in the dictionary")]
fn define(word: String) -> Result<String, rig::tool::ToolExecutionError> {
    if word.trim().is_empty() {
        return Err(rig::tool::ToolExecutionError::invalid_args("word is empty"));
    }
    Ok(format!("{word}: ..."))
}
```

Then register it: `tools: |b| b.tool(Add).tool(Define),`. The generated type
is the function name in CamelCase.

A tool that needs settings reads them from the environment when `spec()`
builds it (`std::env::var`), and the setting goes in `.env.example` and the
VM's env file. A tool never gets the database: that belongs to the runtime.

Test it twice: call the function directly, and run one turn through
`Service::send` with a `MockCompletionModel` that calls it (see
`tests/agent.rs`).

## Choose runtime tools

`sandbox_tools` decides which of the runtime's sandbox tools the agent has:

- `SandboxTools::All`: `shell`, `run_code`, `read_file`, `write_file` and the
  six `browser_*` tools;
- `SandboxTools::Only(&["read_file", "browser_open", "browser_read"])`;
- `SandboxTools::None`.

They only exist when `OPEN_SANDBOX_URL` is set, and they run in a sandbox,
never on the VM. An unknown name fails at startup (`spec().validate()`).

## Model and limits

`AgentSpec { default_model, max_turns, policy, .. }`. `AGENT_MODEL` in the
env file overrides the model without a release.

## Check

```bash
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
```

Try it locally with a key in `.env`: `cargo run` (a REPL), or
`ATHENA_DB=$PWD/agent.db cargo run -- serve` (the HTTP API).
