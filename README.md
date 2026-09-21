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
    cargo run -- usage                        # per-session token totals
    ./test.sh                                 # persistence test, two processes

Same session name resumes the conversation, tool history included.

## Layout

    src/agent.rs    <- you edit this
    src/store.rs       open / load / save / save_run   (two tables)
    src/runner.rs      the Run trait, turn() and the REPL
    src/main.rs        arg parsing and wiring

`runner::turn()` is load -> chat -> save. That function is the entire service;
an HTTP handler would be a wrapper around it.

## What a run costs

Every turn writes one row to `runs` alongside the transcript: token usage
broken out by kind (input, output, cached input, reasoning), the number of
completion requests Rig issued, per-call finish reasons, and the provider's
raw response.

    cargo run -- usage
    session   runs  calls  in   out  cached
    live      1     2      256  26   0

Model calls are not message counts. `add 21 and 21` is one turn, two
completion requests and four transcript rows:

    call 0  finish_reason=tool_calls  in=111  out=21   <- model asks for add()
    call 1  finish_reason=stop        in=145  out=5    <- model answers "42"

Input grows between the two because the tool result re-enters context. That
ongoing cost is invisible in `messages` and is the reason `runs` exists.

`runs.calls_json` keeps the full `Vec<CompletionCall>` as opaque Rig JSON,
including each call's raw provider response. That payload carries detail Rig
does not model — for OpenRouter, upstream routing and cost — but it is an
unredacted copy of the response and it dominates row size (1737 bytes versus
263 in the run above). Set `RUNS_STORE_RAW=0` to drop it.

Telemetry is written after the transcript and its failure is warned about, not
fatal: losing a cost row must never cost you a reply.

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
- No timing beyond whole-turn wall clock. Rig reports no per-call latency;
  time-to-first-token and per-tool duration need Rig's hooks.
- `provider_request_id` is often empty — OpenRouter does not always report one.
- Concurrent turns on one session race: both load the same history and the
  last `save` wins. Single-process CLI use never hits this.
