# Testing

Three layers. Every change runs the first two. Run the third before merging
anything that touches the agent loop, storage or a transport.

| Layer | Command | Network | What it proves |
|---|---|---|---|
| Unit | `cargo test --lib` | none | Each function's rules, including the error paths |
| Integration | `cargo test --tests` | none | The real Rig agent loop and real tools against a scripted model and a real SQLite file. The CLI in-process and as the built binary. The Telegram bot against a fake Bot API. Upgrades from every earlier schema. |
| End-to-end | `scripts/e2e.sh` | OpenRouter | The real provider, several processes, one database |

## Before opening a PR

```
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
./scripts/coverage.sh
```

CI runs the same three commands on every pull request. `coverage.sh` also runs
the unit and integration tests.

## Coverage

Every source line must be executed by at least one test. `scripts/coverage.sh`
enforces this with no exclusions. It reads LCOV rather than using
`cargo llvm-cov --fail-under-lines`, and the script says why.

If a line cannot be reached from a test, change the design rather than
excluding the line. The existing code shows the usual fixes:

- Environment variables are read by a one-line function that passes the
  value to a pure function you can test: `agent::model_or_default`,
  `store::path_or_default`, `runner::keep_raw`, `ops::version_or_dev`,
  `ops::absolute_db`, `http::allowed_hosts`.
- The provider is kept apart from the agent's definition, so tests build
  the production agent around a mock model: `agent::configure`.
- The CLI takes its input, output and agent factory as parameters:
  `cli::run`.

Coverage is a floor, not the goal. A test that runs a line without checking
what it did does not count.

## Integration tests

Tests that run the real binary must run it in an empty directory, as
`athena_in` in `tests/cli.rs` does. The binary loads `.env` from its working
directory, and in the repository that file holds a developer's real keys:
a "no API key" test would find one and call the provider.

`tests/common/mod.rs` has the helpers:

- `TempDb`: a real database file that is deleted on drop. `tmp.service()`
  opens a fresh `Store` and `Service` on it, as a new process would; drop it
  and call again to simulate a restart. `tmp.raw()` is a plain connection
  for inspecting what was stored.
- `mock_agent(&service, turns)`: the production agent from
  `agent::configure`, with the service's conversation memory, in front of
  Rig's `MockCompletionModel`. It returns the model too, so
  `model.requests()` shows exactly what the agent sent.
- `add_turns()`: the `add 21 and 21` exchange, a tool call and then the
  answer, with the token counts OpenRouter reported for it.
- `cli_user`, `session` and `session_id`: the `cli:local` user, a session by
  name, and a session's id looked up in the database.
- `raw_rows` and `runs`: the stored transcript and telemetry, by session id.
- `mock_agent_with_memory` and `mock_stream_agent`: the production agent
  around any memory (a wrapper that parks a turn, say), in front of a
  scripted blocking or streaming model. One mock model scripts blocking
  turns or streaming turns, never both, so a test of both endpoints builds
  two agents on the same memory. `streamed_text` and `streamed_add_turns`
  script streaming turns. Every scripted stream must end with
  `MockStreamEvent::final_response`: without it Rig treats the turn as
  truncated and fails it.

`tests/http.rs` drives the HTTP API in-process through the router
(`tower::ServiceExt::oneshot`), the server over real loopback TCP, and
`athena serve` as the built binary, stopped with SIGINT so its coverage is
written. A client that disconnects is simulated by dropping the response
body or the request future, which is what hyper does when a socket closes.
Turns are parked inside a wrapping memory (`ParkedAppend`), never timed.

`tests/telegram.rs` drives the Telegram transport through
`tests/telegram/fake_api.rs`, a fake Bot API server on a local port. Point
a `teloxide::Bot` at it with `set_api_url`, or the binary with
`TELEGRAM_API_URL`. `api.push(text_from(user, text))` queues an update;
`api.wait_for(...)` and `api.messages_to(chat, n)` wait on a condition over
the recorded calls; `api.fail_next(method, error)` makes the next call to a
method fail. The decision logic in `src/telegram.rs` is unit-tested through
the `Chat` trait with a recording chat, so most cases need no server.

Concurrency tests must be deterministic. Park a turn inside a wrapping
`ConversationMemory` (see `Gated` in `src/service.rs`) and wait on a
condition, never on a sleep. A timeout is acceptable only as a guard that
turns a deadlock into a failure. Check that a concurrency test fails when
the lock it tests is removed.

Script the model with `MockTurn::text`, `MockTurn::tool_call` and
`MockTurn::error`. Assert on what reached the database and what reached the
model, not only on the return value.

One Rig 0.42 detail: the preamble reaches the model as a leading `System`
message in `chat_history`, not in `CompletionRequest::preamble`, and it is
never stored.

## Schema changes without losing data

`Store::open` guarantees the following, and `tests/upgrade.rs` and the unit
tests in `src/store.rs` test each point:

- Migrations run in one `IMMEDIATE` transaction, so concurrent first opens
  cannot both migrate.
- A database from a newer build is refused, not written into.
- A table whose columns differ from `EXPECTED_COLUMNS` is refused, and the
  database is left exactly as it was found.
- A row that references a missing user or session is refused the same way.
- Switching to WAL retries while another process holds the file.

Foreign keys are enforced per connection, and only `Store` turns them on. A
test that writes through `tmp.raw()` or the `sqlite3` shell is not checked.

To change the schema:

1. Append a migration to `MIGRATIONS` in `src/store.rs`. Never edit or
   reorder one that has shipped. Only migrations 1 and 2 use
   `IF NOT EXISTS`, because they existed before versioning did. New ones are
   plain DDL.
2. Before writing it, save a fixture of the schema it upgrades from as
   `tests/fixtures/v<N>_<name>.sql`. Include realistic rows: real Rig message
   JSON with a tool call and a tool result. The best source is the previous
   build's binary run against a copy of the previous fixture, then
   `sqlite3 .dump` (which omits `user_version`; append it). Redact account
   identifiers and say so in the header. Never edit a fixture afterwards.
3. Add an upgrade test to `tests/upgrade.rs` for that fixture. It opens the
   fixture, checks that every earlier row of every table is unchanged (the
   `dump` helper compares every column and rowid), runs a turn, and checks
   again. Transcripts are append-only now, so the second check catches a
   turn that rewrites history or a migration that loses an owner.
4. Update `EXPECTED_COLUMNS`. Then run the tests and replace
   `tests/fixtures/schema.txt` with the text the snapshot test prints. It
   lists columns, foreign keys, every index (the automatic ones behind
   `UNIQUE` included) and triggers. The diff to that file is what review
   sees.
5. If the change affects what a user sees, add an upgrade section to
   `scripts/e2e.sh` that loads the new fixture through the real binary.

## End-to-end, for an AI agent

The CLI is the test interface. Everything a transport does must be reachable
from it, so an agent can drive and check the real system without a browser
or a chat app. One exception: the session a Telegram user has selected is
not shown by the CLI yet. Read it from the `selected_sessions` table.

### Running it

```
./scripts/e2e.sh         # key from .env, or export OPENROUTER_API_KEY
```

It builds the binary, points `ATHENA_DB` at a throwaway file, and prints one
`PASS` or `FAIL` line per check. It exits non-zero on the first failure. If
the binary itself errors, its stderr is printed. On failure the database is
kept and its path printed, so you can inspect it:

```
sqlite3 <path> "SELECT s.name, u.transport || ':' || u.external_id, s.id FROM sessions s JOIN users u ON u.id = s.user_id"
sqlite3 <path> "SELECT seq, substr(json, 1, 120) FROM messages WHERE session_id='<id>' ORDER BY seq"
sqlite3 <path> "SELECT first_seq, last_seq, model_calls, status, error FROM runs ORDER BY started_at"
```

Sessions created by this build have uuid ids; sessions migrated from an
older database keep their old name as their id. The script's `sid NAME
[TRANSPORT ID]` helper looks one up.

Checks read the database, not the model's wording. Two checks depend on the
model's choices: that it called the tool, and that it repeated a code word.
Each retries once. If one fails twice, look at the stored transcript before
assuming the code is wrong. The model may have refused or rephrased.

### What it covers today

1. A tool-using turn: the tool call and result are stored, one run is
   recorded with at least two model calls and non-zero tokens, and seq
   numbers are contiguous.
2. A second process continues the session: it recalls a code word, earlier
   rows are untouched, and each run starts where the previous one ended.
3. Sessions are isolated: a new session starts at seq 0, the other session
   is untouched, and `sessions` and `usage` list both.
4. Upgrade in place: `tests/fixtures/v0_main.sql` is loaded and a turn is
   run through the real binary. The schema migrates, the 8 old rows are
   byte for byte unchanged, the session belongs to `cli:local` under its old
   id, and the new turn starts at seq 8.
5. Users and sessions: `sessions new` prints the stored id, refuses a
   duplicate and writes no second row, and chatting by that name uses it.
   `--user telegram:e2e` starts with no sessions, gets a different session
   for the same name, and each user's `sessions` and `usage` list only their
   own. No session lacks an owner and no message lacks a session.
6. Upgrade from schema 2: `tests/fixtures/v2_run_telemetry.sql` is loaded
   and a turn is run. Every old message and run is unchanged, all three old
   sessions (one with only a failed run) belong to `cli:local`, and the new
   turn starts at seq 12.
7. The HTTP API: `athena serve` on a free loopback port, driven with
   curl. Health needs no user; a missing `X-Athena-User` is 400 and a
   foreign `Host` 403, and neither creates a user. Sessions are created
   (201, the stored id) and refused when duplicated (409). A blocking
   message makes a tool call and returns the stored run's id; the
   transcript endpoint returns every stored row. A streamed message sends
   deltas and ends with exactly one `done`, every `event:` line has a
   `data:` line, and `done` names the stored run, which counted tokens
   and starts where the previous run ended. A second user gets the same
   404 for the first user's session as for a missing one, on every
   endpoint, and lists nothing. A client that hangs up mid-stream still
   gets an ok run and a whole transcript. SIGTERM, as `docker stop` sends,
   while a stream is running lets it end with `done`, saves its run, and
   exits 0.

8. The Telegram bot: `scripts/fake_telegram.py`, a fake Bot API server,
   and the real `athena telegram` binary against it through
   `TELEGRAM_API_URL`. Checks that need no model run first: the command
   menu is registered; `/new` creates and selects a session for that
   Telegram user; `/sessions` marks it; `/switch` moves and stores the
   selection, refuses a missing session and creates nothing; another user
   cannot switch into it. SIGTERM exits with status 0, and after a restart
   `/sessions` still marks the selection and old updates are not handled
   again. Then real turns: a new Telegram user's prompt creates the user
   and their `default` session, stores one ok run, shows typing, and the
   model's reply comes back through the fake API. Prompts go to the
   selected session; `/switch` back resumes a conversation (a code word,
   retried once), and it never reaches the other user's sessions. The bot
   token never appears in the bot's log. Without a valid key the model-free
   checks still pass and the first turn check fails.

### Extending it

Every feature PR adds a numbered section for what it introduces, such as the
HTTP API or Telegram. A transport's users are reachable from the CLI with
`--user TRANSPORT:ID`, so check what a transport stored with the same
commands. Follow these rules:

- Use `ATHENA_DB`. Never point a test at the repository's `agent.db`.
- Assert on database state, or on a distinctive token like `ZEBRA-7391`
  matched case-insensitively. Never assert on the model's exact phrasing.
- Retry only checks that depend on the model's choices, and only once.
- A section that starts a background process appends its pid to
  `BACKGROUND`, so every exit, failures included, kills it.
- A new invariant belongs in both places: a hermetic integration test
  proves the code, and an e2e check proves it holds against the real
  provider.
- Start background processes with `&`, append their pid to `BG_PIDS`, and
  wait on a condition (a file, a count from the fake server), never a fixed
  sleep. The exit trap stops everything in `BG_PIDS`, on failure too.

## Telegram, live (manual)

A bot cannot message itself, so no script can talk to the real Telegram.
Once you have a token from @BotFather, a person runs this checklist. Use a
throwaway database.

1. `export TELEGRAM_BOT_TOKEN=...` and `OPENROUTER_API_KEY=...`, then
   `ATHENA_DB=/tmp/tg.db cargo run -- telegram`. It prints
   `telegram: polling for messages`.
2. Open the bot in Telegram. Type `/` and check the menu lists `new`,
   `sessions`, `switch`, `usage` and `help`.
3. Send `/start`: it says you are in session `default`.
4. Send `Remember the code word LYNX-2211.` "typing..." shows, then a reply.
5. Send `/new side`, then `Say hi.` Then `/sessions`: `* side` is marked and
   both sessions show 2 messages.
6. Send `/switch default`, then `What code word did I give you?`: the reply
   has LYNX-2211.
7. Send two messages quickly: the second is answered "Still working on your
   last message".
8. Ask for something long (`Write 6000 words about rivers.`): it arrives in
   several messages, none cut mid-word unless a word is longer than a
   message.
9. Press Ctrl-C, start the bot again, send `/sessions`: `default` is still
   marked. `/usage` lists both sessions.
10. Add the bot to a group and send a message there: the bot says nothing,
    and its stderr logs that it ignored the chat.
11. `sqlite3 /tmp/tg.db "SELECT transport, external_id FROM users"` shows
    `telegram` and your Telegram user id.
