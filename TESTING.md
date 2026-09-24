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

`tests/common/mod.rs` has the helpers:

- `TempDb`: a real database file that is deleted on drop. Close it and open
  it again to simulate a process restart.
- `mock_agent(turns)`: the production agent from `agent::configure` in front
  of Rig's `MockCompletionModel`. It returns the model too, so
  `model.requests()` shows exactly what the agent sent.
- `add_turns()`: the `add 21 and 21` exchange, a tool call and then the
  answer, with the token counts OpenRouter reported for it.
- `raw_rows` and `runs`: the stored transcript and telemetry, for assertions.

Script the model with `MockTurn::text`, `MockTurn::tool_call` and
`MockTurn::error`. Assert on what reached the database and what reached the
model, not only on the return value.

One Rig 0.42 detail: the preamble reaches the model as a leading `System`
message in `chat_history`, not in `CompletionRequest::preamble`, and it is
never stored.

## Schema changes without losing data

`store::open` guarantees the following, and `tests/upgrade.rs` tests each
point:

- Migrations run in one `IMMEDIATE` transaction, so concurrent first opens
  cannot both migrate.
- A database from a newer build is refused, not written into.
- A table whose columns differ from `EXPECTED_COLUMNS` is refused, and the
  database is left exactly as it was found.
- Switching to WAL retries while another process holds the file.

To change the schema:

1. Append a migration to `MIGRATIONS` in `src/store.rs`. Never edit or
   reorder one that has shipped. Only migrations 1 and 2 use
   `IF NOT EXISTS`, because they existed before versioning did. New ones are
   plain DDL.
2. Before writing it, save a fixture of the schema it upgrades from as
   `tests/fixtures/v<N>_<name>.sql`. Include realistic rows: real Rig message
   JSON with a tool call and a tool result. Never edit a fixture afterwards.
3. Add an upgrade test to `tests/upgrade.rs` for that fixture. It opens the
   fixture, checks that every earlier row is byte for byte unchanged, runs a
   turn, and checks again. `save` rewrites the whole session, so the second
   check is the one that catches a lossy Rig upgrade.
4. Update `EXPECTED_COLUMNS`. Then run the tests and replace
   `tests/fixtures/schema.txt` with the text the snapshot test prints. The
   diff to that file is what review sees.
5. If the change affects what a user sees, add it to step 4 of
   `scripts/e2e.sh`.

## End-to-end, for an AI agent

The CLI is the test interface. Everything a transport does must be reachable
from it, so an agent can drive and check the real system without a browser
or a chat app.

### Running it

```
source ~/.zshrc          # or export OPENROUTER_API_KEY
./scripts/e2e.sh
```

It builds the binary, points `ATHENA_DB` at a throwaway file, and prints one
`PASS` or `FAIL` line per check. It exits non-zero on the first failure. If
the binary itself errors, its stderr is printed. On failure the database is
kept and its path printed, so you can inspect it:

```
sqlite3 <path> "SELECT seq, substr(json, 1, 120) FROM messages WHERE session_id='e2e' ORDER BY seq"
sqlite3 <path> "SELECT first_seq, last_seq, model_calls, status, error FROM runs ORDER BY started_at"
```

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
   byte for byte unchanged, and the new turn starts at seq 8.

### Extending it

Every feature PR adds a numbered section for what it introduces, such as user
and session commands, the HTTP API, or Telegram. Follow these rules:

- Use `ATHENA_DB`. Never point a test at the repository's `agent.db`.
- Assert on database state, or on a distinctive token like `ZEBRA-7391`
  matched case-insensitively. Never assert on the model's exact phrasing.
- Retry only checks that depend on the model's choices, and only once.
- A new invariant belongs in both places: a hermetic integration test
  proves the code, and an e2e check proves it holds against the real
  provider.
