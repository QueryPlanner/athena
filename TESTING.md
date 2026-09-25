# Testing

Three layers. Every change runs the first two. Run the third before merging
anything that touches the agent loop, storage or a transport.

| Layer | Command | Network | What it proves |
|---|---|---|---|
| Unit | `cargo test --lib` | none | Each function's rules, including the error paths |
| Integration | `cargo test --tests` | none | The real Rig agent loop and real tools against a scripted model and a real SQLite file. The CLI in-process and as the built binary. Upgrades from every earlier schema. |
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
  `store::path_or_default`, `runner::keep_raw`.
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
or a chat app.

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

### Extending it

Every feature PR adds a numbered section for what it introduces, such as the
HTTP API or Telegram. A transport's users are reachable from the CLI with
`--user TRANSPORT:ID`, so check what a transport stored with the same
commands. Follow these rules:

- Use `ATHENA_DB`. Never point a test at the repository's `agent.db`.
- Assert on database state, or on a distinctive token like `ZEBRA-7391`
  matched case-insensitively. Never assert on the model's exact phrasing.
- Retry only checks that depend on the model's choices, and only once.
- A new invariant belongs in both places: a hermetic integration test
  proves the code, and an e2e check proves it holds against the real
  provider.
