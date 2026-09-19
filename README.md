# athena

A template for persistent, tool-using agents in Rust.
rig-agent 0.42 + SQLite, talking to OpenRouter.

## Make a new agent

Edit `src/agent.rs`. That is the only file that changes.

    #[rig_tool(description = "...")]        // add your tools
    fn my_tool(arg: String) -> Result<String, rig::tool::ToolExecutionError> { ... }

    pub const PREAMBLE: &str = "...";       // set the system prompt
    pub const DEFAULT_MODEL: &str = "...";  // set the model

    pub fn build() -> Result<impl Chat> {
        ... .tool(MyTool) ...               // register them
    }

Everything else — session storage, the turn loop, the CLI — stays as is.

## Run

    source ~/.zshrc                          # OPENROUTER_API_KEY
    export AGENT_MODEL=openai/gpt-5.6-luna   # optional override

    cargo run                                # REPL, session "default"
    cargo run -- research                     # REPL, session "research"
    cargo run -- research "some prompt"       # one-shot, same session
    cargo run -- sessions                     # list sessions
    ./test.sh                                 # persistence test, two processes

Same session name resumes the conversation, tool history included.

## Layout

    src/agent.rs    <- you edit this
    src/store.rs       open / load / save            (one table)
    src/runner.rs      turn() and the REPL
    src/main.rs        arg parsing and wiring

`runner::turn()` is load -> chat -> save. That function is the entire service;
an HTTP handler would be a wrapper around it.

## How persistence works

`agent.chat(input, &mut history)` appends the user message, every tool call,
every tool result and the assistant reply into `history`. That vec is
serialised into one SQLite table, one row per message, stored as opaque Rig
JSON. There is no item taxonomy of our own because Rig's `Message` already
encodes tool calls and results — and keeping it opaque means a rig upgrade
can't break the database.

Inspect a session:

    sqlite3 agent.db "SELECT seq, json_extract(json,'\$.role'), substr(json,1,200)
                      FROM messages WHERE session_id='default' ORDER BY seq;"

## Known limits

- A crash mid-turn loses that turn. Persistence granularity is one save per
  `chat()` call, not per model call.
- `save` rewrites every row each turn. Fine until sessions get long.
- No streaming; a long tool chain is silent until it finishes.
- Context grows forever, and every turn re-sends the whole history.
- No network interface. `runner::turn()` is the seam to add one.
