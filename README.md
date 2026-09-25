# athena

A template for persistent, tool-using agents in Rust.
rig-agent 0.42 + SQLite, talking to OpenRouter.

## Make a new agent

Edit `src/agent.rs`. That is the only file that changes.

    #[rig_tool(description = "...")]        // add your tools
    fn my_tool(arg: String) -> Result<String, rig::tool::ToolExecutionError> { ... }

    pub const PREAMBLE: &str = "...";       // set the system prompt
    pub const DEFAULT_MODEL: &str = "...";  // set the model

    pub fn configure(builder: AgentBuilder) -> Agent {
        builder ... .tool(MyTool) ...       // register them
    }

`configure` is the agent's whole definition. Production wraps it around the
OpenRouter client; the tests wrap it around Rig's mock model, so they run the
same preamble and tools you ship. Both give the builder the conversation
memory first (`builder.memory(service.memory())`), so `configure` never has
to know where conversations are stored.

Everything else — users, sessions, storage, the turn loop, the CLI — stays
as is.

## Run

    source ~/.zshrc                          # OPENROUTER_API_KEY
    export AGENT_MODEL=openai/gpt-5.6-luna   # optional override

    cargo run                                # REPL, session "default"
    cargo run -- research                    # REPL, session "research"
    cargo run -- research "some prompt"      # one-shot, same session
    cargo run -- sessions                    # list your sessions
    cargo run -- sessions new notes          # create an empty session
    cargo run -- usage                       # per-session token totals
    cargo run -- --user telegram:42 ...      # any of the above as another user
    ATHENA_DB=/tmp/x.db cargo run -- ...     # use another database file
    cargo run -- serve                       # HTTP API on 127.0.0.1:8080
    cargo run -- serve --addr 127.0.0.1:9000 # or ATHENA_ADDR=...

A session name is created on first use and resumes the conversation after
that, tool history included. `sessions new NAME` creates one explicitly and
prints `NAME<TAB>ID`; it fails if you already have one by that name.

The CLI acts as the user `cli:local` unless `--user TRANSPORT:ID` says
otherwise. That flag exists so an AI agent (or you) can drive exactly what an
HTTP client or a Telegram chat would see, from a shell. It is not an access
control boundary: anyone who can run the binary can read the database file.

## Users and sessions

A user is identified by `(transport, external_id)`: `("cli", "local")`
today, `("telegram", "<chat id>")` or `("http", "<account>")` once those
transports exist. A new transport adds a transport name, never a schema
change.

A session belongs to exactly one user. Its name is unique among that user's
sessions; its id (a uuid) is unique everywhere. Two users can both have a
session called `default` and never see each other's. A user asking for
another user's session id gets "no such session", the same answer as for an
id that does not exist.

## Layout

    src/agent.rs    <- you edit this
    src/service.rs     the core every transport calls: users, sessions, send
    src/store.rs       the database: migrations, Rig conversation memory, runs
    src/runner.rs      the Run trait and the run record
    src/cli.rs         the CLI transport: arguments, output, REPL
    src/http.rs        the HTTP transport: JSON API, SSE streaming, `serve`
    src/main.rs        wiring: real database, provider, stdin/stdout
    tests/             integration tests, upgrade fixtures, schema snapshot
    scripts/           coverage gate and live end-to-end test

## Test

    ./scripts/coverage.sh                     # all tests + 100% line coverage
    ./scripts/e2e.sh                          # live, against OpenRouter

[TESTING.md](TESTING.md) covers the three test layers, the rules for schema
changes, and how an AI agent runs and extends the end-to-end test.

## The core service

`service::Service` is the one API a transport calls. It returns values and
never prints; problems that do not fail a call (a lost telemetry row) go to
the `warn` callback given to `Service::new`.

    let service = Service::new(Store::open(path)?, model, warn);
    let agent   = agent::build(&client, model, service.memory());

    let user    = service.user("telegram", chat_id).await?;         // get or create
    let session = service.open_session(&user, "default").await?;    // get or create
    let session = service.create_session(&user, "notes").await?;    // AlreadyExists if taken
    let session = service.session(&user, &id).await?;               // NotFound unless theirs
    let list    = service.sessions(&user).await?;                   // name, id, message count
    let msgs    = service.history(&user, &id).await?;               // Vec<Message>
    let totals  = service.usage(&user).await?;
    let turn    = service.send(&agent, &user, &id, "hello").await?; // Turn { reply, run }

Errors are `service::Error`: `NotFound`, `AlreadyExists`, `Invalid`,
`Conflict`, `Model`, `Storage`, chosen to map straight onto HTTP statuses.

`send` guarantees:

- Turns on one session run one at a time. A second message that arrives
  while a turn is running waits, then loads the history the first one saved.
  Turns on different sessions run concurrently.
- The transcript is saved before `send` returns the reply. If it cannot be,
  `send` fails, even though the model answered; the run is still recorded
  with the tokens it spent.
- If another process appended to the session during the turn, the turn is
  refused with `Conflict` instead of interleaving two conversations that
  never saw each other.

The agent is a parameter of `send`, not part of the service, so listing and
creating sessions needs no API key and one service can serve any number of
tasks. Dropping `send`'s future cancels the turn. A transport whose client
can go away uses one of these instead (the service behind an `Arc`):

    let turn = service.send_detached(agent, &user, &id, "hi").await?;  // Turn
    let mut turn = service.send_stream(agent, &user, &id, "hi").await?;
    while let Some(event) = turn.next().await { ... }  // Text, ToolCall, then Done(Result<Turn>)
    service.idle().await;                             // on shutdown

Both check the request and wait for the session in the caller's future, so
a caller that gives up while queued behind another turn leaves nothing
behind. Once the turn holds the session it runs in its own task, with every
guarantee above, and finishes and is saved even if nobody is listening any
more. `idle` waits for those tasks. `send_stream` drives Rig's streaming
agent, which loads and appends through the same memory and receipt as the
blocking one; its `Done` carries exactly what `send` would have returned.

## HTTP API

`athena serve` listens on `--addr`, else `ATHENA_ADDR`, else
`127.0.0.1:8080`. It needs `OPENROUTER_API_KEY` at startup. It is
unauthenticated: read Known limits before listening anywhere but loopback.
On any other address it still starts, and prints a warning.

Every request except `/health` names its user in a header:

    X-Athena-User: alice          # the user ("http", "alice")

A missing, blank, non-ASCII or over-256-byte value is `400`. `src/http.rs`
turns the header into a user in one place, the `Caller` extractor, so
authentication replaces that and nothing else. While listening on loopback,
a request whose `Host` is not `localhost`, `127.0.0.1` or `[::1]` is `403`,
so a web page cannot reach the API by pointing its own domain at 127.0.0.1
(DNS rebinding).

| Method and path | Body | Success |
|---|---|---|
| `GET /health` | | `200 {"status":"ok"}` |
| `POST /sessions` | `{"name":"notes"}` | `201 {"id","name","created_at"}` |
| `GET /sessions` | | `200 {"sessions":[{"id","name","created_at","messages"}]}` |
| `GET /sessions/{id}/messages` | | `200 {"messages":[...]}` |
| `POST /sessions/{id}/messages` | `{"text":"hi"}` | `200 {"reply","run":{...}}` |
| `POST /sessions/{id}/messages/stream` | `{"text":"hi"}` | `200 text/event-stream` |
| `GET /usage` | | `200 {"usage":[{"session_id","name","runs","model_calls","input_tokens","output_tokens","cached_input_tokens"}]}` |

`messages` are Rig's own message JSON, exactly as stored, tool calls and
tool results included. That format belongs to Rig and can change with a Rig
upgrade.

`run` is the turn's `runs` row without `calls_json` (raw provider
responses): `run_id`, `session_id`, `status`, `error`, `model`,
`first_seq`, `last_seq`, `model_calls`, `input_tokens`, `output_tokens`,
`total_tokens`, `cached_input_tokens`, `reasoning_tokens`, `started_at`,
`ended_at`.

A streamed turn sends these events, then closes the stream:

    event: delta       data: {"text":"4"}                          0 or more
    event: tool_call   data: {"name":"add","arguments":{"a":21,"b":21}}  0 or more
    event: done        data: {"reply":"42","run":{...}}            last, on success
    event: error       data: {"error":{"code":"model","message":"..."}}  last, on failure

`done.reply` is the reply. Concatenated deltas can differ from it, because
text the model writes next to a tool call is streamed too. Keep-alive
comments (`:` lines) arrive every 15 s during long tool calls. A bad request
(bad body, empty text, someone else's session) is refused with a status
code before the stream starts.

Errors are `{"error":{"code","message"}}`:

| Status | `code` | When |
|---|---|---|
| 400 | `invalid` | bad or missing header, body not JSON, empty name or text |
| 403 | `forbidden_host` | see above |
| 404 | `not_found` | no such session **for this user**, including another user's |
| 409 | `already_exists` | session name taken |
| 409 | `conflict` | another process wrote to the session mid-turn; send again |
| 502 | `model` | the model failed; details in the server log and `runs.error` |
| 500 | `storage` | the database failed; details in the server log |
| (SSE) | `internal` | the turn crashed; details in the server log |

Another user's session id gets the same `404` as an id that does not exist.
`model` and `storage` messages are generic, because provider and database
errors can carry details an unauthenticated client should not see.

A turn that has started finishes and is saved even if its client
disconnects, streamed or not. Ctrl-C stops accepting requests, lets open
ones finish and waits for every turn still running, then exits. A second
Ctrl-C quits at once, and the turns in flight are lost as in a crash.

    curl -s localhost:8080/sessions -H 'X-Athena-User: alice' -d '{"name":"notes"}'
    curl -N localhost:8080/sessions/$ID/messages/stream -H 'X-Athena-User: alice' -d '{"text":"hi"}'

## How persistence works

Rig loads and saves the conversation itself, through
`store::SqliteMemory`, an implementation of Rig's `ConversationMemory` over
the `messages` table. The conversation id is the session id. Before a turn,
Rig loads the session's rows; after a successful turn it appends everything
the turn produced (the user message, each tool call and result, the reply) in
one call. The memory inserts those rows after the last one in a single
transaction and never rewrites an earlier row. Each row is opaque Rig JSON:
Rig's `Message` already encodes tool calls and results, and keeping it
opaque means a Rig upgrade does not need a schema change.

Rig treats a failed append as a warning and returns the reply anyway. The
memory records what each load and append did in a per-session receipt, and
`send` reads it after the run, which is how an unsaved transcript becomes an
error instead of a log line.

The database handle is one SQLite connection per process behind a mutex
(`store::Store`). SQL runs on tokio's blocking pool, never on an async worker,
and the mutex is never held across an `.await`, so a model call on one
session never holds up another. SQLite allows one writer at a time anyway,
so a pool or a dedicated writer task would add code without adding write
throughput. All SQL lives in `store.rs`, so changing the handle later
touches one file.

The schema is versioned with `PRAGMA user_version`. `Store::open` applies any
missing migrations in one transaction, refuses a database written by a newer
build, refuses a table whose columns it does not expect, and refuses one with
dangling references. Existing `agent.db` files upgrade in place: every
session they hold is given to `cli:local` under its old name, and no stored
message changes. The rules for adding a migration are in TESTING.md.

Foreign keys are enforced per connection. `Store` turns them on; a plain
`sqlite3` shell does not.

Inspect a session:

    sqlite3 agent.db "SELECT m.seq, json_extract(m.json,'\$.role'), substr(m.json,1,200)
                      FROM messages m JOIN sessions s ON s.id = m.session_id
                      WHERE s.name='default' ORDER BY m.seq;"

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

## Known limits

- **The HTTP API is unauthenticated. Do not expose it publicly.** Whoever
  can reach the port can act as any user by setting `X-Athena-User`, read
  every session, spend the OpenRouter credit, and use the agent's tools on
  the server's machine, including `read_file` on any path the process can
  read. Loopback is the default, and a loopback server refuses foreign
  `Host` headers, but every local process is still trusted. Authentication
  replaces `Caller` in `src/http.rs`; until then, put an authenticating
  proxy in front before listening anywhere else.
- Every distinct `X-Athena-User` value creates a `users` row, even for a
  read. There is no rate limit or cap beyond the 256-byte header limit.
- A crash mid-turn loses that turn, and so does a second Ctrl-C while
  `serve` drains. Persistence granularity is one append per turn, not per
  model call.
- Dropping `send`'s future cancels the turn. If that happens after the
  append but before the run row, the transcript has rows no run covers.
  `send_detached` and `send_stream` do not have this problem.
- A turn that panics (a bug) records no run. The session is unlocked, and
  a streamed client gets an `internal` error event.
- A streamed turn's `calls_json` holds what Rig's streaming driver
  reports for each call. With Rig's mock model that raw payload is the
  stream's terminal record, not a whole response; it has not yet been
  compared with a blocking call's payload from OpenRouter. The blocking
  HTTP endpoint and the CLI use the blocking driver.
- Deltas already streamed cannot be taken back. If the transcript then
  fails to save, the client has seen text that the `error` event says was
  not kept.
- Axum answers some malformed requests itself, not in this API's JSON
  error shape: unknown paths (empty `404`), wrong methods (`405`), and
  bodies over 2 MB (`413`).
- Only SIGINT (Ctrl-C) shuts `serve` down gracefully. SIGTERM, as sent by
  most process managers, stops it at once.
- Context grows forever, and every turn re-sends the whole history.
- Transports so far: the CLI and HTTP. They call `service::Service`.
- Turns on one session are serialized within a process. Across processes
  they are not: the second one to finish is refused with `Conflict` rather
  than interleaved.
- No timing beyond whole-turn wall clock. Rig reports no per-call latency;
  time-to-first-token and per-tool duration need Rig's hooks.
- `provider_request_id` is often empty — OpenRouter does not always report one.
- `runs` rows are ordered by `(started_at, rowid)`. `VACUUM` may renumber
  rowids, so two runs started in the same millisecond can swap after one.
