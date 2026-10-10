# Interface contracts for the production-template build

Every workstream builds against these names. Changing one means changing this file
in the same PR and telling the integrator. The design rationale is in
`plans/production-template.md`.

## Binaries

Both binaries live in one crate.

| Binary | Source | Notes |
|---|---|---|
| `athena` | `src/main.rs` | existing |
| `deploy-gate` | `src/bin/deploy-gate.rs` + `src/gate/` (lib module `athena::gate`) | the only program CI can run on the VM |

## `athena` CLI additions

| Command | Behaviour |
|---|---|
| `athena --version` | Prints `athena <version>`. `<version>` is `$ATHENA_VERSION` if set, else `<CARGO_PKG_VERSION>-dev`. Exit 0. No DB access. |
| `athena backup <dest-path>` | Online backup of `$ATHENA_DB` to `<dest-path>` via rusqlite's backup API (feature `backup`). Opens the source **read-only and raw**; never calls `Store::open` and never migrates. Creates parent dirs. Exit 0 on success, non-zero with a message on failure. |
| `athena eval run --target <replay\|URL> [--cases evals/cases] [--k N] [--out results.jsonl] [--user eval] [--judge]` | Runs eval cases (section "Evals"). |
| `athena eval compare <base.jsonl> <candidate.jsonl>` | Diff by case and tag. Exit non-zero on regression beyond thresholds. |
| `athena bench --url <base> [--concurrency 3] [--duration-secs 120] [--user bench] [--max-p95-ms 30000] [--max-error-rate 0.05]` | Load check against a running HTTP API. Prints a JSON summary on stdout. Exit non-zero when a threshold fails. |

Rules for `serve` and `telegram`:
- They **refuse to start** unless `ATHENA_DB` is set to an absolute path. The CLI
  REPL keeps today's default of `agent.db`.
- `serve` is the only process that runs migrations on deploy; `telegram` opens
  after it.

## Environment variables

| Var | Used by | Meaning |
|---|---|---|
| `ATHENA_DB` | all | SQLite path (absolute for serve/telegram) |
| `ATHENA_ADDR` | serve | listen address, e.g. `100.124.202.79:18080` |
| `ATHENA_ALLOWED_HOSTS` | serve | comma list of accepted `Host` values (`host` or `host:port`). When set, only these are accepted (plus loopback names when bound to loopback), and the UNAUTHENTICATED banner is not printed. |
| `ATHENA_VERSION` | all | version string, written by deploy-gate (`<git-sha>` or `v1.2.3+<sha>`) |
| `ATHENA_ENV` | all | `staging` \| `prod` \| unset (dev). Becomes OTel `deployment.environment.name`. |
| `OPENROUTER_API_KEY`, `AGENT_MODEL`, `RUNS_STORE_RAW`, `TELEGRAM_BOT_TOKEN` | existing | unchanged |
| `ATHENA_INSTRUCTIONS` | agent | optional path of a text file (at most 16 KiB, `custom::MAX_INSTRUCTIONS_BYTES`), appended to the base preamble after `## Custom instructions`. Unset or unusable means none (a warning is logged). |
| `ATHENA_SKILLS_DIR` | agent | optional path of a directory of Agent Skills (`<name>/SKILL.md`, https://agentskills.io/specification), loaded once at startup. Each valid skill is listed in the preamble after `## Skills`, and `read_skill(name)` is registered when at least one loads. Invalid skills are skipped with a warning. Limits: 64 KiB per `SKILL.md`, 64 skills, 16 KiB of listing, 256 directory entries of any kind examined, the scan stopping there; which ones past that is up to the file system (`custom::skills`). |
| `ATHENA_COMPACT_AT` | all | compact a session above this share of the model's context window; 0.3 to 0.95, default `0.8` |
| `ATHENA_COMPACT_MODEL` | all | model id that writes compaction summaries; default `AGENT_MODEL` |
| `ATHENA_CONTEXT_TOKENS` | all | the agent model's context window in tokens (8000 or more). Unset: OpenRouter's `context_length` for `AGENT_MODEL`, else 128000 |
| `OPEN_SANDBOX_URL` | agent | e.g. `http://100.118.54.67:9090`. **Unset means the sandbox tools are not registered** (dev and tests keep working). |
| `OPEN_SANDBOX_API_KEY` | agent | optional; sent as the `OPEN-SANDBOX-API-KEY` header when set |
| `ATHENA_SANDBOX_IMAGE` | agent | default `ghcr.io/queryplanner/athena-sandbox:latest` |
| `ATHENA_SANDBOX_TIMEOUT_SECS` | agent | default `1800`, minimum 60 |
| `ATHENA_PUBLIC_URL` | agent | base of sign-in links, e.g. `http://100.124.202.79:18080`; default `http://$ATHENA_ADDR` (so `serve` and `telegram` agree from the one env file). Must be http(s); required when `ATHENA_ADDR` is unspecified (`0.0.0.0`, `[::]`). |
| `CLOUDFLARE_ACCOUNT_ID` | telegram | Cloudflare account id (letters and digits). With `CLOUDFLARE_API_TOKEN`, voice notes and audio files are transcribed. **Both unset means voice notes are not prompts; only one set is a startup error.** |
| `CLOUDFLARE_API_TOKEN` | telegram | Cloudflare API token with Workers AI access; a secret, sent only as `Authorization: Bearer` to the endpoint below |
| `EXA_API_KEY` | agent | secret, optional. Exa API key, sent only to `https://api.exa.ai/search` as the `x-api-key` header (redirects not followed). **Unset or blank means the `web_search` tool is not registered**; its name stays reserved from MCP tools either way. Results are cut to `policy::MAX_RESULT_BYTES` by the tool itself and wrapped in nonce markers as untrusted web content. Typed into the env file by a human; not a deploy-gate setting. |
| `GOOGLE_HEALTH_CLIENT_ID` | agent, telegram | Google OAuth client id (Blacki's, unchanged). With the next two, Google Health is on. **All three unset (or blank) means it is off: the six tools exist and say it is not set up, the Telegram commands say so. Some but not all set is a startup error that names the missing variables, never a value.** |
| `GOOGLE_HEALTH_CLIENT_SECRET` | agent, telegram | secret. Sent only in the body of calls to `https://oauth2.googleapis.com/token`. Typed into the env file by a human. |
| `GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY` | agent, telegram | secret. URL-safe base64 of exactly 32 bytes (Blacki's Fernet key has this form); the 32 bytes are the XChaCha20-Poly1305 key for the stored refresh token. Anything else is a startup error. Changing it makes stored tokens unreadable (those connections are marked `revoked`; the user reconnects). |
| `GOOGLE_HEALTH_REDIRECT_URI` | agent, telegram | optional. The redirect URI registered with the Google OAuth client; default `http://127.0.0.1:8080/integrations/google-health/callback` (Blacki's). `https`, or `http` on `127.0.0.1` or `localhost`; no query or fragment. |
| `ATHENA_MCP_CONFIG` | agent | path of an `mcp.json` (section "MCP servers"). **Unset means no MCP tools**, and no default path is searched. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | all | OTLP/HTTP base URL; on the VM OpenObserve, `http://<tailnet-ip>:5080/api/default` (`/v1/traces` and `/v1/logs` are appended). **Unset means no OTLP export.** |
| `OTEL_EXPORTER_OTLP_HEADERS` | all | `Authorization=Basic%20<base64 of OpenObserve root email:password>`, written by `setup-host.sh` into the env file only |
| `ATHENA_TELEMETRY_DIR` | all | e.g. `/var/lib/athena/<env>/telemetry`: daily `traces-<role>-YYYYMMDD.jsonl` and `logs-<role>-YYYYMMDD.jsonl`, `<role>` being the process (`serve`, `telegram`, `cli`), so every file has one writer. **Unset means no files.** With neither this nor the endpoint, telemetry is off and logs go to stderr only. |
| `ATHENA_TELEMETRY_RETENTION_DAYS` | all | default `30`; files of older days are deleted |
| `OTEL_SERVICE_NAME` | all | default `athena` |

## MCP servers

`ATHENA_MCP_CONFIG` names a JSON file, `{"mcpServers": {NAME: ENTRY}}`, the shape
Claude Code and Claude Desktop use (`src/mcp.rs`).

| Entry field | Meaning |
|---|---|
| `command`, `args`, `env` | stdio server, a child process on the host (not in the sandbox) |
| `url`, `headers` | streamable-HTTP server (legacy SSE is refused) |
| `type` | optional `stdio`, `http` or `streamable-http`; must match the entry |
| `disabled` | `true` leaves the server out |
| `timeoutSecs` | per tool call, default 120 |
| `startupTimeoutSecs` | start, handshake and tool listing, default 60 |

- `${VAR}` and `${VAR:-default}` are expanded in `args`, `env` values and
  `headers` values from athena's environment, never in `url`; `$${` is a literal
  `${`. An unset variable without a default skips that server with a warning
  naming the variable. No value is logged, and warnings show a URL as scheme,
  host and port only.
- A stdio child's environment is its `env` plus `PATH` and `HOME` from athena.
- Servers connect at startup (`serve`, `telegram`) or on the CLI's first turn.
  A server that fails is a warning; its tools are missing.
- One connection per server serves every user and session of the process.
- A tool keeps its server's name. A name that a built-in tool
  (`agent::reserved_tool_names`: `add`, `read_skill`, `web_search`, the calorie tools, the workout tools, `now`, `timezone_set`, the reminder tools, the user skill tools, the sandbox tools) or an earlier server (name order) has, that
  is not 1 to 64 of `[A-Za-z0-9_-]`, or an input schema that is not of type
  object, is skipped with a warning, as are a server's tools past the first
  `MAX_TOOLS_PER_SERVER`. Descriptions are cut to `MAX_DESCRIPTION_BYTES`.
- `ToolPolicy` (`src/policy.rs`) applies by tool name: arguments over
  `MAX_ARGUMENT_BYTES` are not sent, MCP tools' results are cut to
  `MAX_RESULT_BYTES` in all (blocks kept in order, an image or empty block costs
  1 byte, the rest dropped, one notice appended), and at most
  `media::MAX_IMAGES_PER_REQUEST` images are kept, none over `media::MAX_IMAGE_BYTES`.
- Rust API: `agent::connect_mcp(warn) -> Mcp`, `agent::build(.., &Mcp)`,
  `agent::build_with(.., &Mcp)`, `agent::configure_with_mcp(.., &Mcp)`, then
  `Mcp::shutdown().await` when the process ends.

## HTTP

`X-Athena-User` remains trusted, without authentication. HTTP identity names
are trimmed, nonempty printable ASCII, at most 256 bytes. The same validator
applies to Telegram's `/link API_USER_ID` command.

Schema migration 8 adds `user_identities(transport, external_id, user_id,
created_at)`, keyed by `(transport, external_id)` and referencing `users.id`.
It backfills every existing user without changing owner IDs or existing rows.
Identity lookup and creation, and linking, use immediate transactions.

In private Telegram chats, `/link API_USER_ID` binds a never-used HTTP identity
to the caller's existing user ID. An existing binding to that same user succeeds;
a binding to any other user is refused, even if empty. No identities are moved
and no users are merged. The HTTP and Telegram processes must share `ATHENA_DB`.
Linked identities share sessions, usage, and user-owned saved browser state.
HTTP explicitly chooses a session; Telegram's selection remains unchanged.
Authentication, unlinking, and merging existing identities are not implemented.

`GET /version` returns `{"version": "<ATHENA_VERSION or pkg-dev>"}`. Like `/health`,
it needs no user header. Every other endpoint is unchanged.

Sign-in pages (`src/http/viewer.rs`) take no user header; the `{token}` from
`browser_links` is the credential, and the `Host` allowlist still applies.
Unknown or expired tokens are `404`.

| Route | Does |
|---|---|
| `GET /browser/{token}` | the HTML page |
| `POST /browser/{token}/start` | opens the link's `url` if the browser shows `about:blank` |
| `GET /browser/{token}/screen` | `image/png` of the page |
| `POST /browser/{token}/click` `{"x","y"}` | `mouse move x y`, `mouse down`, `mouse up` |
| `POST /browser/{token}/type` `{"text"}` | `keyboard type` (1 to 1000 chars) |
| `POST /browser/{token}/press` `{"key"}` | `press` |
| `POST /browser/{token}/scroll` `{"direction"}` | `scroll <dir> 400` |
| `POST /browser/{token}/open` `{"url"}` | `open` (http or https) |
| `GET /browser/{token}/controls` | `snapshot -i --json`, as `{"controls": [{"ref": "e4", "name", "kind"}]}` in page order; `kind` is `username`, `password`, `code`, `text` or `button` |
| `POST /browser/{token}/submit` `{"fills": [{"ref", "text"}], "click"}` | `fill @ref text` for each (at most 20, 1000 chars each), then `click @ref`, as one command |
| `POST /browser/{token}/done` | `state save`, stored for the session's owner; answers `{"saved_bytes"}` |

## Calorie persistence

Schema migration 9 adds `calorie_logs` in `ATHENA_DB`: `id` (monotonic
AUTOINCREMENT), `user_id` (foreign key to `users.id`), `request_key`, immutable
`request_hash`, `description`, `consumed_date`, nullable `calories`, `protein_g`,
`carbs_g`, `fat_g`, `meal_type`, `source`, `version`, `created_at`, `updated_at`,
and nullable `deleted_at`. `(user_id, request_key)` is unique. The
`calories_by_owner_date` index covers `(user_id, consumed_date, id)`.
Timestamps are Unix milliseconds; `consumed_date` is a supplied local date.

Native tools: `calorie_log(request_key, meal)`,
`calorie_history(start_date, end_date, limit?, before_id?)`,
`calorie_summary(start_date, end_date)`,
`calorie_update(id, expected_version, meal)`, and
`calorie_remove(id, expected_version)`. The host's `runner::Conversation`
resolves the session owner. Every query constrains that owner; the model cannot
choose another user. Tools are registered whenever a persistent agent is built,
and all five names are reserved against MCP collisions.

A meal requires description, a real `YYYY-MM-DD` local calendar date, and source
`user` or `estimated`. Nutrition uses kcal and grams, finite values in
`0..=1000000`, or null for unknown. Description is nonblank printable text of at
most 512 bytes, meal type at most 64 bytes, and retry key at most 128 bytes.
The typed original meal is hashed after signed-zero normalization. Matching
retry keys return the current record, including deletion state, without writing;
changed payloads conflict. Fresh occurrences require fresh keys, even for
identical food. No automatic retry-key generation or legacy database import.

History uses inclusive dates, logging-ID descending order, limit 20 by default
and at most 50, and returns `next_before_id` only when another page exists.
Summary returns `entry_count`, nullable nutrient `totals`, and nutrient `missing`
counts. SQL SUM preserves all-unknown totals as null. Deleted entries are
excluded from both operations.

Mutations commit before success. Corrections and soft deletion increment version
and require the prior version; missing, deleted, and stale entries share a
conflict response. Deletion retains its key and does not resurrect on log retry.
No new HTTP routes or authentication changes.

## User time zone

Schema migration 10 adds `user_settings(user_id, timezone, updated_at)`, keyed
by `user_id` (foreign key to `users.id`), one typed column per setting. A user
without a row uses `timezone::DEFAULT_TIMEZONE`, `Asia/Kolkata`. Only
`timezone_set` writes a row (an upsert); reads never do. The default is part of
this contract: changing it needs a migration that first writes every affected
user's zone explicitly, or their "today" moves. Linked identities share the
zone, as it belongs to the user.

Native tools, registered with the calorie tools and reserved the same way:
`now()` and `timezone_set(timezone)`. The owner comes from the host's
`runner::Conversation`, never from arguments.

| Field | `now` result |
|---|---|
| `datetime` | wall clock, RFC 3339 to the second with offset, e.g. `2026-10-10T01:30:00+05:30` |
| `date` | `YYYY-MM-DD` in the user's zone |
| `weekday` | English name, e.g. `Saturday` |
| `timezone` | IANA name |
| `utc_offset` | e.g. `+05:30` |

`timezone_set` returns `{"previous_timezone", "now"}`, `now` being the object
above in the new zone. A name is accepted when it is `UTC` or contains `/`, is
not under `posix/` or `right/`, is at most 64 printable ASCII bytes, and
resolves in jiff's time zone database (lookup ignores case; the stored name is
the database's spelling; aliases such as `Asia/Calcutta` keep their own name).
Fixed offsets (`+05:30`, `GMT+9`) and abbreviations (`IST`) are refused, with a
message asking for a city. An invalid name writes nothing.

The database is the host's (`/usr/share/zoneinfo`), else the copy compiled in
by jiff's `tzdb-bundle-always`. A stored name that no longer resolves is an
error naming it, not a silent fallback; `timezone_set` repairs it.

Rust API for later features (workouts, reminders): `Store::timezone(owner)`,
`Store::local_now(owner, at)` and `Store::today(owner, at)`, with
`at = jiff::Timestamp::now()` in production. "Today" is computed there, never
supplied by the model. Calorie dates stay model-supplied (`consumed_date`); the
preamble tells the agent to call `now` before reasoning about dates. No env var.

## Workout persistence

Schema migration 11 adds three tables in `ATHENA_DB`:

- `workout_sessions`: `id` (monotonic AUTOINCREMENT), `user_id` (foreign key
  to `users.id`), `request_key`, immutable `request_hash`, `session_date`
  (supplied local `YYYY-MM-DD`), `day_type`, nullable `notes`, `version`,
  `created_at`, `updated_at`, nullable `deleted_at`. `(user_id, request_key)`
  is unique; `workouts_by_owner_type_date` covers
  `(user_id, day_type, session_date, id)`. `day_type` has no SQL CHECK: Rust
  accepts `push`, `pull`, `legs`, `vo2` and `other`, so a new type needs no
  table rebuild.
- `workout_sets`: primary key `(session_id, exercise_index, set_index)`, both
  1-based; `exercise` as typed, `exercise_key` (lowercased, whitespace
  collapsed: how lifts match across sessions), `reps` `0..=1000`, `weight_kg`
  REAL `0..=1000` (kilograms only; 0 is bodyweight or no added load),
  nullable `target_reps` `1..=1000`, `is_warmup` 0 or 1, nullable `notes`.
  `workout_sets_by_exercise` covers `(exercise_key, session_id)`.
- `rowing_results`: `session_id` primary key (one piece per session),
  `distance_m` `100..=100000` (default 2000), `time_ms`. The 500 m split is
  derived (`round(time_ms * 500 / distance_m)`), never stored.

Sets and rowing pieces carry no `user_id`. Every read joins an active
`workout_sessions` row of the owner, and every write to them happens in the
IMMEDIATE transaction that first inserted or version-checked an owned
session, so a guessed id never reaches another user's sets. The migration's
foreign-key check cannot detect a set under the wrong user's session; this
access rule is what prevents one. No cascade: sessions are only soft-deleted,
and an update replaces its children explicitly.

Native tools, registered with the calorie tools and reserved the same way:
`workout_log(request_key, workout)`, `workout_last(day_type, before_date?)`,
`workout_next()`, `exercise_progress(kind?, exercise?, distance_m?, limit?)`,
`workout_history(start_date, end_date, limit?, before_id?)`,
`workout_update(id, expected_version, workout)` and
`workout_remove(id, expected_version)`. The owner comes from the host's
`runner::Conversation`, never from arguments.

A workout is `session_date`, `day_type`, optional `notes` (at most 512 bytes),
`exercises` (0 to 30 blocks of `name`, at most 64 bytes, and 1 to 20 sets of
`reps`, `weight_kg`, optional `target_reps`, `is_warmup` default false,
optional `notes` at most 256 bytes) and optional `rowing` (`distance_m`
default 2000, `time` as `M:SS`, `H:MM:SS`, either with up to 3 decimals). It
needs an exercise or a rowing piece; a lift may repeat in several blocks. A
piece whose split is outside 1:00 to 10:00 per 500 m is refused, which
catches a split entered as the total or minutes entered as hours. Weights are
rounded to 0.01 kg. Retry keys work as for meals; the hash is of the
normalized workout (rounded weights, `time_ms`, the default distance filled),
so `7:05` and `7:05.0` are one original.

"Today" is `Store::today(owner, jiff::Timestamp::now())`, never the model's.
`workout_last` returns `{"before_date", "session"}`: the newest active
session of that type dated before `before_date` (exclusive; default today),
or null. Each of its exercise blocks has `progression`: the top working set
(warm-ups and zero-rep sets excluded; ranked by Epley estimated 1RM, then
weight, then reps), `hit_target` (null without a target) and `suggested`:
2.5 kg more at the target reps when the top set reached its target and
weighed more than 0, otherwise one more rep at the same weight.
`workout_next` returns `today`, `next_day_type` (after the newest push, pull
or legs session dated before today: push → pull → legs → push; push if
none), `last_lifting`, `vo2` (`due` when no `vo2` session is dated in the
7 days ending today, `last_date`), `logged_today` (id, type, version of
today's sessions) and `previous` (as `workout_last` for the type due).
Sessions dated after today are ignored by both.

`exercise_progress` with `kind` `lift` (default) needs `exercise` and returns
the newest sessions with that lift (`limit` default 10, at most 50), each
with its top set and estimated 1RM (`w × (1 + reps/30)`, the weight itself
for one rep, to 0.1 kg), the all-time `best`, and
`estimated_1rm_change_kg` from the oldest to the newest in the window. No
match returns `known_exercises`. `kind` `rowing` takes `distance_m`
(default 2000) and returns `time`, `split_500m` (to the tenth) and
`time_change_s` the same way. History is as for meals but returns whole
sessions, limit 10 by default and at most 20. Corrections replace a session's
sets and piece; missing, deleted and stale sessions share one conflict
message; removal keeps the key and does not resurrect on retry. No env var,
route or authentication change.

## Scheduler and reminders

Schema migration 12 adds `jobs(id, user_id, kind, payload, next_run_at,
recurrence, status, lease_until, attempts, sent_at, last_error, created_at,
updated_at, confirm_code, confirm_session, confirm_expires_at)`, `user_id` a
foreign key to `users.id`, times in UTC milliseconds, with indexes
`jobs_due (status, next_run_at)` and `jobs_by_owner (user_id, status)`. `kind`
(`notify`, `agent_task`) and `status` (`pending`, `active`, `done`,
`cancelled`, `failed`) are checked in Rust, not
by a CHECK, so a later kind (system jobs) needs no table rebuild; a kind
this build does not know is marked failed when claimed. `recurrence` is NULL
(once), `daily@HH:MM` or `weekly:mon,wed@HH:MM`. `next_run_at` stays an
occurrence's due time while it is retried or deferred; `lease_until` is when
the row may be claimed again. `sent_at` is the last run that was delivered
or started, kept after the row ends.

Native tools, registered with the calorie and time tools and reserved the
same way; the owner comes from the host's `runner::Conversation`:

| Tool | Arguments | Result |
|---|---|---|
| `reminder_create` | `kind`, `text`, `repeat` (`once` default, `daily`, `weekly`); once: exactly one of `at` (local `YYYY-MM-DDTHH:MM`) or `in_minutes`; daily: `time` (`HH:MM`); weekly: `time` and `weekdays` (`mon`..`sun`) | the reminder: `id`, `kind`, `text`, `repeat`, `status`, `last_error`, `next_run {datetime, weekday, timezone}` on the user's clock; for an `agent_task` also `confirmation_code` and `next_step` |
| `reminder_confirm` | `id`, `code` | the task, now `active`, with `"confirmed": true` |
| `reminder_list` | none | `{"active": [...], "awaiting_confirmation": [...], "failed": [...]}`: active soonest first, tasks whose code still works, failed in the last 7 days |
| `reminder_cancel` | `id` | `{"cancelled": id}`, for an active or pending reminder; any other id is an error |

Confirmation: a `notify` is `active` at once. An `agent_task` runs a prompt
with the user's authority, so `reminder_create` stores it `pending` (never
claimed) with a fresh 8-character code (`A-Z` and `2-9` without `O`, `I`,
`0`, `1`), bound to the user, the session it was created in and that task's
id, expiring after 10 minutes (`reminders::CONFIRM_WINDOW`). It returns the
preview: the whole text, the schedule and the code. `reminder_confirm`
activates it only if the run's `runner::UserText`, the text the user sent
to start this turn, contains the code and the id as whole words (any case,
`#12` and `12` alike), in the same session, before expiry; the code then
stops working. A one-off whose time passed meanwhile is refused; a repeat
moves to its next run. `runner::tool_context(conversation, text, outbox)`
inserts `UserText`, from `Request::user_text()`, which is empty for a turn the
scheduler starts (`Request::scheduled`), so a scheduled task can never
confirm one. Only reminders use it; the per-user skills do not.

Times are the user's wall clock (`Store::timezone`). A one-off is stored as
the instant it names; a local time a daylight-saving change skips is
refused, one that happens twice is its first occurrence. A repeat's next run
is computed in the user's zone when the previous one ends, as the first
occurrence strictly after now (a skipped time runs an hour later, a doubled
one once); `timezone_set` recomputes the user's active repeats. Missed runs
are not made up: a run more than 5 minutes late is sent once with a
"(late)" note, then the repeat waits for its next time.

Limits per user (pending tasks whose code still works count as active): 50
active reminders, 10 of them `agent_task`; 100 created in
any 24 hours (cancelled ones count); text 1000 bytes (`notify`) or 2000
(`agent_task`), nonblank, no control characters but line breaks; first run in
the future and at most 366 days ahead. At most 20 `agent_task` runs start in
any 24 hours (counted from `sent_at`, whatever became of the reminder); past
that a run is skipped and the user told. The repeats on offer are daily at
the shortest, so one task runs at most once a day (23 hours apart on a
clock-change day). Only users with a Telegram identity can create reminders.

Delivery: only `athena telegram` runs the scheduler (`athena serve` and the
CLI only create rows), so **staging runs reminders only if `staging.env` has
its own `TELEGRAM_BOT_TOKEN`**. Every 30 s it leases up to 20 due rows for
10 minutes and runs each in its own task. The chat is the owner's Telegram
user id (`user_identities`, transport `telegram`, oldest first); a user
without one fails the job.

- `notify` sends `Reminder: <text>`; the run is recorded right after
  Telegram accepts it (at-least-once: a crash between the two repeats it).
- `agent_task` runs `<text>` as a turn in the user's currently selected
  session, through `Service` (session lock) and the bot's one-turn-per-user
  slot, at most 4 at once across users, and sends the reply as any turn's.
  If the user is mid-turn it waits a minute at a time, and is skipped once
  it is 30 minutes late. The run is recorded before the turn (at-most-once:
  a paid turn never runs twice; a crash mid-turn loses it). While it runs,
  the user's own messages get the usual "still working" reply.
- Telegram refusing for good (blocked, deactivated, chat not found, any
  `Forbidden`) fails the job, including a repeating task whose reply cannot
  be delivered. Other send failures retry after 1, 2, 4 and 8 minutes, then
  fail.
- SIGINT or SIGTERM stops claiming when it stops polling (or when polling
  ends for any other reason); jobs already started finish before exit.

## User skills

Schema migration 13 (after `jobs`, 12) adds three tables in `ATHENA_DB`, all
keyed to `users.id` (`ON DELETE CASCADE`):

- `user_skills`: `id` (AUTOINCREMENT), `user_id`, `name`, `description`,
  `body`, `origin` (`github` or `user`), `source_repo` (`owner/name`),
  `source_path` (empty for the root), `source_sha` (40 hex), `created_at`,
  `updated_at`, nullable `deleted_at`. `(user_id, name)` is unique; the three
  `source_*` columns are set exactly when `origin` is `github`. A confirmed
  skill of an existing name, removed or not, replaces every column and file.
- `user_skill_files`: `skill_id`, `path` (relative to the skill), `content`
  (UTF-8 text); key `(skill_id, path)`.
- `skill_previews`: `user_id`, `preview_id`, `session_id`, `payload` (the
  previewed `user_skills::SkillRecord` as JSON), `created_at`, `expires_at`;
  key `(user_id, preview_id)`.

Tools (registered with the calorie tools, whenever a persistent agent is built;
names always reserved): `skill_list()`, `skill_read(name, file?)`,
`skill_install(repo, path?, ref?)`, `skill_create(name, description, body?)`,
`skill_confirm(preview_id)`, `skill_remove(name)`. The owner is the session's
user (`runner::Conversation`); no tool takes a user. The preamble never lists
these skills: when the tools are registered it ends with
`user_skills::PREAMBLE`, which only points at them and tells the agent to show
the preview and call `skill_confirm` only after the user explicitly agrees in
the chat.

Two-step add: `skill_install` and `skill_create` save nothing. They stage the
record for `PREVIEW_TTL_MS` (10 minutes) under an opaque `preview_id` (`pv_`
and 16 random hex digits) and return a preview: name, description, source
repository and pinned commit, file list with sizes, the skipped files, the
body's first 1000 characters, whether it replaces a skill, and the id.
`skill_confirm(preview_id)` saves the staged record only if the id is the
user's, from the same session and unexpired; a preview is single use. At most
`MAX_PENDING` (5) staged per user, the oldest dropped; at most
`MAX_USER_SKILLS` (64) live skills per user.

Nothing in the code checks that the user agreed. The only thing between a
preview and a saved skill is the model asking: content that talks the model
into calling `skill_confirm` can add a skill (see `known-limits`). Containing
that is the job of the rest of the design: the preview step, the id's
expiry and single use, the session and user binding, the size limits, GitHub
text labelled untrusted, and skills only ever being read, never run.
(`runner::UserText` stays: reminders use it.)

`skill_read` returns the text between two lines of a delimiter made per call
(`=====skill-<uuid>=====`), after a label: GitHub skills are marked untrusted
third-party content with their repository and commit; created skills are the
user's notes. `skill_remove` sets `deleted_at`.

GitHub (`user_skills::github`): `https://api.github.com`, no credentials, so
public repositories only (60 requests an hour per IP). `repo` is `owner/name`
or `https://github.com/owner/name[.git]`; owner, name, ref and every path
segment are 1 to 100 of `[A-Za-z0-9._-]`, not `.` or `..`; a ref may not start
with `.` or `-` and defaults to `HEAD`. Requests: `GET
/repos/{o}/{r}/commits/{ref}` (`Accept: application/vnd.github.sha`) for the
SHA, then `GET /repos/{o}/{r}/contents/{path}?ref={sha}` for listings and,
with `Accept: application/vnd.github.raw`, files. No redirects, 20 s timeout,
`User-Agent: athena`. Only entries of type `file` and `dir` with safe names are
read, paths are rebuilt from the requested path and entry names. Limits:
`SKILL.md` and each file 64 KiB (a larger other file is skipped), 32 files
besides `SKILL.md`, 256 KiB in all, 8 directories, depth 3 (deeper ones
skipped); non-UTF-8 files are skipped. `SKILL.md` is parsed by
`custom::skills::parse`, and its `name` must equal the last path segment, or
the repository's name at the root. Files are stored, not staged into the
sandbox.

## Google Health

Read-only sleep, activity, heart-rate and body data from the Google Health API
(v4, the Fitbit successor), synced once a day per user into SQLite and read by
the model through six tools. Ported from Blacki (`src/blacki/health/`),
which keeps the same environment variable names.

**Schema** (migration 14, after `user_skills`, 13; `health_points` and `health_backfill` are migration 16, after the daily brief, 15). All keyed to `users.id`
(`ON DELETE CASCADE`):

- `health_connections`: `user_id` (primary key), `encrypted_refresh_token`
  BLOB (`Cipher`: byte `1`, a random 24-byte nonce, then the XChaCha20-Poly1305
  ciphertext and tag; the user id is authenticated, not stored), `scopes`
  (space separated, as Google granted them), `status` (`connected` or
  `revoked`), `connected_at`, `last_synced_at`, `last_attempt_at` (the last
  sync *started*, success or not: the daily schedule and the manual cooldown
  both claim it), `last_sync_error`, `updated_at`. Times are UTC milliseconds.
  A revoked row keeps its token.
- `health_oauth_states`: `state_hash` (SHA-256 hex of the `state`; primary
  key), `user_id`, `expires_at`, `created_at`. One live row per user.
- `health_daily`: `user_id`, `date` (`YYYY-MM-DD`, the user's local day),
  `metrics` (JSON), `updated_at`; key `(user_id, date)`. Rows survive
  `/disconnect_health`. Keys of `metrics`, present only when Google reported
  them. Only the recent 14-day window is aggregated; history is in
  `health_points`. Each day is at most 4 KiB of JSON (`health::normalize::
  MAX_DAY_BYTES`): over that, `hr_zones`, `activity_level_min`,
  `hr_zone_minutes`, `moods`, `symptoms`, `ovulation_tests`, `swim`,
  `workouts`, `sleep` are dropped in that order and named in `dropped`.
  Free text from Google is never stored in a day; enum-valued labels are kept
  only if `[A-Z0-9_]{1,40}`.
  - Sums of interval points: `steps`, `floors`, `distance_m`,
    `elevation_change_m` (net: Google's altitude value is a signed delta),
    `active_kcal`, `basal_kcal`, `active_min`, `zone_min` (active zone
    minutes), `sedentary_min`, `activity_level_min` (`{TYPE: minutes}`),
    `swim` (`{lengths, strokes}`), `hydration_ml`, `nutrition` (`{entries,
    kcal, carbs_g, fat_g, protein_g?, fiber_g?, sugar_g?, sodium_mg?}`),
    `hr_zone_minutes` (`{zone: minutes}`).
  - Samples, `{min, avg, max, n}`: `heart_rate`, `hrv_ms` (RMSSD),
    `spo2_pct`, `resp_rate_sleep`, `glucose_mgdl`, `core_temp_c`.
  - Latest of the day: `weight_kg`, `body_fat_pct`, `height_cm`, `vo2max`,
    `run_vo2max`.
  - Google's daily summaries as sent: `resting_hr`, `hrv_daily` (`{avg_ms,
    deep_rmssd_ms, non_rem_hr, entropy}`), `spo2_daily` (`{avg, min, max}`),
    `vo2max_daily` (`{value, level, estimated}`), `resp_rate_daily`,
    `sleep_temp` (`{nightly_c, baseline_c, rel_stddev_30d_c}`), `hr_zones`
    (`[{type, min_bpm, max_bpm}]`).
  - Sessions, capped per day (`workouts_omitted`, `sleeps_omitted` count the
    rest): `workouts` (at most 8: `[{type, minutes, kcal?, distance_m?,
    avg_hr?, zone_min?}]`), `sleep` (at most 4: `[{minutes, start, end,
    stages: [{type, minutes}], nap?}]`).
  - Counts and labels only, never a waveform or a heart-beat series: `ecg`
    (`{count, classes}`), `irn` (`{count}`), `moods` and `symptoms` (`{count,
    labels: {LABEL: n}}`, at most 12 labels), `ovulation_tests` (`{RESULT:
    n}`), `menstrual_period_started` (periods that began that day).
- `health_points` (migration 16): every data point as Google returned it.
  `id`, `user_id`, `data_type` (kebab case, as in the path), `point_key`
  (Google's `name`, else `sha256:` of type, start, source and value),
  `start_ms` and `end_ms` (UTC milliseconds; a sample has both the same; a
  daily summary is its date's local midnight), `civil_date` (the user's local
  day), `value` (the data point's JSON, whole), `source` (`PLATFORM:device`),
  `ingested_at`; unique `(user_id, data_type, point_key)`, index `(user_id,
  data_type, start_ms)`. Writes are upserts that change a row only when
  `value` differs, so re-reading is idempotent. Rows survive
  `/disconnect_health` and are in `athena backup` (the online backup copies
  the whole database). **Growth:** heart rate is about one point every few
  seconds to a minute: minute-level alone is about 525,000 rows a year, each
  a few hundred bytes with its index. Other minute types (steps, distance,
  energy, altitude, active minutes, SpO2) add a similar order each; ECG points
  carry their waveform (tens of KB each). Budget gigabytes for years of data
  per user. The model reads it only through `health_data_size`,
  `health_points` and `health_export` (below).
- `health_backfill` (migration 16): `user_id`, `data_type`, `oldest_date` (the
  earliest local day fetched so far), `done`, `empty_run`, `last_error`,
  `updated_at`; key `(user_id, data_type)`. Reconnecting deletes the user's
  rows here so history is looked for again.

**Connecting ("paste back").** Athena has no HTTPS callback and no new route:

1. `/connect_health` (private chat) stores `sha256(state)` for the user
   (`state` is 64 random hex digits, valid 10 minutes, one per user) and
   replies with `https://accounts.google.com/o/oauth2/v2/auth?...`:
   `response_type=code`, `access_type=offline`, `prompt=consent`,
   `include_granted_scopes=true`, the redirect URI, the `state`, a PKCE
   `code_challenge` (S256) and these scopes, all read-only:
   `googlehealth.activity_and_fitness.readonly`,
   `googlehealth.health_metrics_and_measurements.readonly`,
   `googlehealth.location.readonly`, `googlehealth.nutrition.readonly`,
   `googlehealth.sleep.readonly`, `googlehealth.reproductive_health.readonly`,
   `googlehealth.logged_symptoms.readonly`, `googlehealth.mindfulness.readonly`,
   `googlehealth.ecg.readonly`, `googlehealth.irn.readonly` (`health::SCOPES`;
   never a `writeonly` one). The user can untick any on Google's consent
   screen; a type whose scope was not granted answers 403 and is skipped. A
   connection made before these were added holds the first four only and must
   `/connect_health` again. The PKCE
   verifier is `sha256(secret, state)` in hex, derived and never stored.
2. The user approves; the browser goes to the redirect URI (it need not
   load) and the user pastes that URL into Telegram.
3. **The auth code never reaches the model.** `Telegram::respond` checks
   every message's text (a file's or voice note's caption too) before anything
   else with `health::find_callback`: the first whitespace-separated word,
   minus surrounding brackets and quotes, that holds the redirect URI's host
   and path, or both `code=` and `state=`, as it is or percent-encoded. That
   test runs whether or not Google Health is configured (unconfigured, only
   the `code=` and `state=` rule applies, and the reply is "not set up"). Such
   a message is never passed to `Service::send`, never stored in `messages`,
   never logged, and its parameters are not kept after step 4. A bare code
   without its URL cannot be recognised.
4. The `state` is consumed in one statement (`DELETE ... WHERE state_hash AND
   user_id AND expires_at > now`): single use, bound to the user, not spent by
   another user's attempt. Then `POST https://oauth2.googleapis.com/token`
   (form: client id and secret, `code`, `code_verifier`, `grant_type`,
   `redirect_uri`). A response without a refresh token fails. The token is
   sealed and stored, `status = connected`, `last_attempt_at` cleared, and the
   first sync runs at once.
   A declined, expired, spent or foreign link, a missing code, or Google
   refusing the code each get a plain reply and store nothing.

**Environment.** Section "Environment variables".

**HTTP.** One `reqwest` client: connect timeout 5 s, total 30 s, redirects not
followed. The access token travels only in an `Authorization` header marked
sensitive, the client secret and refresh token only in form bodies, never in a
URL. Token answers are read up to 64 KiB, a page of data points up to 2 MiB.
An error is a status and Google's short error code (a word of at most 64
characters) at most: never a URL, a body or a secret, and no type involved
prints one with `Debug`. Base URLs are constants (`health::TOKEN_URL`,
`REVOKE_URL`, `API_BASE`), not settings; tests inject a fake server through
`health::client::Endpoints`.

**Sync.** `GET https://health.googleapis.com/v4/users/me/dataTypes/<type>/dataPoints`
with a `pageSize`, a `filter` and `pageToken`, for the 40 data types of
`health::catalog::SPECS`, from the v4 `DataPoint` union
(<https://developers.google.com/health/reference/rest/v4/users.dataTypes.dataPoints>;
filters <https://developers.google.com/health/filters>, page sizes
<https://developers.google.com/health/reference/rest/v4/users.dataTypes.dataPoints/list>).
The filter names the type in snake case (`body_fat`; a hyphen is a 400), and
depends on the type's shape:

| Shape | Filter field | Types |
|---|---|---|
| Interval | `{t}.interval.start_time` (RFC 3339, `>=` and `<`) | steps, floors, distance, altitude, active-energy-burned, basal-energy-burned, active-minutes, active-zone-minutes, activity-level, sedentary-period, swim-lengths-data, time-in-heart-rate-zone, menstrual-period |
| Sample | `{t}.sample_time.physical_time` | vo2-max, run-vo2-max, heart-rate, heart-rate-variability, oxygen-saturation, respiratory-rate-sleep-summary, core-body-temperature, blood-glucose, weight, body-fat, height, ovulation-test, symptoms, moods |
| Daily | `{t}.date` (`YYYY-MM-DD`) | daily-vo2-max, daily-resting-heart-rate, daily-heart-rate-variability, daily-oxygen-saturation, daily-respiratory-rate, daily-sleep-temperature-derivations, daily-heart-rate-zones |
| Session | `{t}.interval.civil_start_time` (dates) | exercise, nutrition-log, hydration-log, irregular-rhythm-notification |
| Sleep | `sleep.interval.end_time` (a night belongs to the day it ended) | sleep |
| ECG | `electrocardiogram.interval.start_time >= ...` only (no end; docs) | electrocardiogram |

Scopes by type: `activity_and_fitness` (the activity, interval, VO2 max and
exercise types), `health_metrics_and_measurements` (heart rate, HRV, SpO2,
respiration, temperature, glucose, weight, body fat, height and the `daily-*`
summaries but `daily-vo2-max`), `sleep`, `nutrition` (`nutrition-log`,
`hydration-log`), `ecg`, `irn`, and, by their names, `reproductive_health`
(`menstrual-period`, `ovulation-test`), `logged_symptoms` (`symptoms`),
`mindfulness` (`moods`). `location` is requested for exercise GPS; no type
read today uses it. The docs do not tabulate scopes for `basal-energy-burned`
or the women's-health types; any scope not granted answers 403.

Page sizes: 25 for `exercise`, `sleep` and `irregular-rhythm-notification`
(the documented maximum for the first two); 10 for ECG (waveforms); 200 for
`nutrition-log`; otherwise 1000 (the documented maximum is 10,000, but a page
is read whole under 2 MiB). `dataSourceFamily` is never sent (not supported for
`sleep`, `food`, `food-measurement-unit`). Paging follows `nextPageToken`, at
most 60 pages per type (400 for heart rate). Left out: `food` and
`food-measurement-unit` (catalogues with no time, not intake; `nutrition-log`
is the intake) and `v4beta`-only types. The window is the user's last 14
local days ending today (whole days from local midnight, so a day with a
clock change is its real length). A point goes on its civil date, else on its
instant in the user's zone (`user_settings`); sleep goes on the day it ended;
ECG on its start instant in the user's zone (old ECGs carry no UTC offset).
Access tokens are kept in memory until a minute before they expire and never
stored; a rotated refresh token is sealed and stored.

Every page is written to `health_points` as it arrives (one transaction per
page, so memory holds one page) and, for the window, folded into the day
aggregates.

**Backfill.** On each *daily* pass (not on `health_sync_now`), after the
window is stored, for each type not `done`: up to 3 chunks
(`BACKFILL_CHUNKS_PER_PASS`) of 7 days (`BACKFILL_DAYS`) working backwards from
`health_backfill.oldest_date` (at first the start of the window), points
stored only (no aggregates). A type is `done` at 3 years back
(`BACKFILL_FLOOR_DAYS`; Google sets no retention limit), after 12 empty chunks
in a row, or when Google refuses it (403, or 400/404 for a type added after the
first release). ECG has no upper-bound filter, so its history is read in one
go back to the floor. A chunk that needs more pages than the cap keeps what it
read and moves on (`last_error: partial`). Any other failure leaves the
cursor and ends the pass; it never fails the sync. A user with history
therefore finishes in about 52 days (156 weeks / 3).

- A type Google answers 403 for is skipped and reported by name in
  `unavailable`; its days in the window are replaced by days without it. A
  type added after the first release that answers 400 or 404 is skipped and
  reported as `type (HTTP 400)`, so a filter Google rejects shows in the
  report instead of failing every user's sync.
- Any other failure (HTTP error, timeout, too many pages) writes no days:
  the old days stay, `last_sync_error` records why, the connection stays
  `connected`, and the next try is an hour later. Points already stored stay
  (they are the same points).
- On success the window's days are replaced in one transaction that also sets
  `last_synced_at` and clears the error, only if the connection still
  exists and is `connected` (a disconnect during a sync is not undone).
- Only Google's `invalid_grant` on a refresh, or a stored token that cannot be
  decrypted, sets `status = revoked`. The call that does it reports it; the
  daily task tells the user once ("send /connect_health").

**Daily sync.** `scheduler::Execute::system(now)` runs on every scheduler tick
(30 s) beside the jobs and is not a job: no `jobs` row, no lease, none of the
reminder limits. `Jobs` implements it in `athena telegram` only, so `athena
serve` never syncs. For each `connected` user (by id) it syncs when it is past
05:30 on their clock and `last_attempt_at` is before that instant, or when the
last attempt failed and an hour has passed. The attempt is claimed with a
compare-and-swap on `last_attempt_at`, and one process syncs a user once at a
time, so overlapping ticks and processes do not repeat a sync. A run that is
slow does not delay the next tick; shutdown waits for it (each request has its
own 30 s timeout).

**Tools** (names in `health::NAMES`: the first three here and the three raw
point tools below, always reserved and always registered with the other native
tools; the owner is the session's user, no tool takes one):

- `health_status()`: `configured`, `status` (`not_connected`, `connected`,
  `revoked`), `connected_at`, `last_synced_at` (local time), `last_sync_error`,
  and a `hint`.
- `health_summary(days?, metrics?)`: 1 to 30 local days (default 7) ending
  today, oldest first: the stored metrics with sleep as `sleep_min` and
  `sleep_stage_min`, `distance_km`, no `hr_zones`; `metrics` is a list of names
  from `health::tools::METRICS` (unknown name: an error listing them) and keeps
  only those (and `date`, `dropped`); `averages` over the days that have a
  value (`steps`, `active_min`, `zone_min`, `resting_hr`, `sleep_min`,
  `floors`, `active_kcal`, `hrv_ms`, `spo2_pct`, `resp_rate`,
  `heart_rate_avg`); `status`, `today`, `last_synced_at`. The result is at
  most `SUMMARY_BYTES` (56 KiB, under the 64 KiB tool-result policy): if the
  days do not fit, the oldest are left out and `truncated_days` counts them.
  A user with no connection
  row gets no days; a revoked one gets the old days with a hint. The daily
  sync fills 14 days, so a longer window fills over time.
- `health_sync_now()`: a sync now, refused within an hour of the last attempt
  of any kind (`status: cooldown`, `retry_after_minutes`).

What `health_summary` returns is personal health data: it goes to the model
provider and is stored in the session like any tool result.

**Raw point tools** (`health::data`, `health::export`; all three act for the
session's owner and filter every query by `user_id`; none takes a user).
`from` and `to` are inclusive civil dates `YYYY-MM-DD` with the same meaning as
`health_summary`'s days (the `civil_date` column); `from` after `to`, or a
malformed date, is an error. `types` are names from `health::catalog::SPECS`
(the 40 kebab-case data types, never taken from `SELECT DISTINCT`); an unknown
name is an error listing the valid ones; absent or empty means all. A date
window is turned into an indexed `start_ms` band in the owner's *current*
zone, widened by 2 days on each side (`store::health_data::BAND_DAYS`: the
zone can have changed since the point was synced), and `civil_date` then
decides exactly. Rows with a NULL `civil_date` appear only when no date is
given; rows with a NULL `start_ms` are found by `civil_date` alone.

- `health_data_size(types?, from?, to?)`: per type with points: `rows`,
  `first` and `last` (local time of the earliest and latest `start_ms`, from
  the index) with `first_date` and `last_date`, `approx_bytes` (rows times the
  mean stored value size over a sample of at most 500 points, never a SUM over
  the table), and `types_without_points`, `total_rows`, `approx_total_bytes`.
  One short store query at a time.
- `health_points(type, from?, to?, limit?, cursor?)`: `limit` 1 to 500
  (default 100). Order is `start_ms`, then `id`, points with no `start_ms`
  last (by `id`): the index order, so no sort. Each point is `{start, end,
  date, source, value}` with times in the owner's zone and `value` the *parsed*
  JSON of the type's own field (`catalog` `field`), without Google's `name` and
  `dataSource`. For `electrocardiogram` every array longer than 64 items is
  replaced by `{"samples_omitted": n}` and every string over 4096 bytes by
  `{"samples_omitted_bytes": n}` (JSON is never cut mid-string); any other
  value over 16 KiB is `{"omitted_bytes": n}`. The points JSON is at most 56
  KiB (`data::ROWS_BYTES`, under the 64 KiB tool-result policy): a page stops
  before the point that would pass it and returns `next_cursor`. One page
  reads at most 2 MiB of stored values. `next_cursor` is opaque (URL-safe
  base64 of `{t: type, f: from, to, s: last start_ms or null, i: last id}`);
  passing it with a different type, from or to, or a malformed one, is an
  error. The result says that points a sync changes between pages (an upsert
  keeps the row's `id` but can change `start_ms` and `civil_date`) can repeat
  or be skipped. When the byte limit stops a page the last point may be the
  last one, so a final page can come back empty with no cursor.
  The result is text: a header, the points JSON between `<<<HEALTH_DATA
  <nonce>>>>` and `<<<END_HEALTH_DATA <nonce>>>>` markers (fresh 32-hex nonce
  per result, `untrusted::nonce`), and after the end marker a line with
  `returned` and `next_cursor`. The header says the content is untrusted data
  (values include text the user typed: food names, notes, moods), as
  `web_search` does; both use `untrusted::fit` to stay within
  `policy::MAX_RESULT_BYTES`. Like `health_summary`, the result goes to the
  model provider and is stored in the session.
- `health_export(types?, from?, to?)`: builds a SQLite file in the session's
  sandbox. Refused (error, nothing read) when the turn's `UserText` is empty
  or absent (scheduled turns: the daily brief and `agent_task`; follows
  `reminder_confirm`), when there is no sandbox (`OPEN_SANDBOX_URL` unset: the
  tool is still registered), when no points match, and when the stored size
  estimate (`approx_bytes` summed) is over 128 MiB
  (`data::MAX_EXPORT_BYTES`; also checked against the bytes actually uploaded).
  Minute-level heart rate alone passes 128 MiB at about 10 months, so a full
  history needs narrower `types` or dates.
  - Transport: points are read in batches by `id` (`WHERE user_id = ? [AND
    data_type IN (...)] [window] AND id > ? ORDER BY id LIMIT n`, `NOT
    INDEXED` so the planner walks the table by `id` instead of sorting per
    batch) of at most 2000 points and 2 MiB of stored values
    (`export::BATCH_ROWS`, `BATCH_BYTES`), each batch one short store call,
    the connection free between batches. Each batch is projected as above and
    appended to NDJSON, `{"t": type, "k": point_key, "s": start_ms, "e":
    end_ms, "d": civil_date, "src": source, "v": projected value}` per line.
    At 4 MiB (`export::CHUNK_BYTES`) the chunk is uploaded with
    `Sandboxes::write_file` to `/tmp/athena-data/.export-<nonce>/chunk-NNNNN.ndjson`.
    `write_file` buffers the body and each upload has the sandbox client's 30 s
    request timeout, so 4 MiB needs at least 1.1 Mbit/s. Host memory is one
    chunk plus one batch; nothing touches the host's disk.
  - Before the chunks: `mkdir -p <dir> && chmod 700 /tmp/athena-data <dir>`
    (`Sandboxes::command`), then `load.py` (`src/health/export_loader.py`,
    `include_str!`, standard library only) and `meta.json` are uploaded to
    `<dir>`. After: `python3 <dir>/load.py <dir> /tmp/athena-data/health.sqlite`
    (`export::DB_PATH`, 15 minute limit). The loader builds
    `<dir>/health.sqlite.tmp` (inside the nonce directory, so two exports in
    one session cannot collide), indexes it, chmods it 0600, `os.replace`s it
    onto the destination (atomic), removes `<dir>`, and prints one JSON line
    `{"rows": {type: n}, "bytes": n}`. On failure it exits 1 with the reason
    on stderr, leaves the destination alone and removes `<dir>`; the host also
    `rm -rf`s `<dir>` when any step fails.
  - Schema: `points(data_type, point_key, start_ms, end_ms, civil_date,
    source, value TEXT)` with `UNIQUE (data_type, point_key)` written with
    `INSERT OR REPLACE`, indexes `(data_type, civil_date)` and `(data_type,
    start_ms)`; `meta(key, value)` with `exported_at`, `zone`, `types`,
    `from`, `to` and `rows`; views `heart_rate(start_ms, civil_date, source,
    bpm)`, `steps(.., count)`, `weight(.., kg)`, `sleep(start_ms, end_ms, ..,
    minutes)`, `hrv(.., rmssd_ms)` and `spo2(.., percent)` over `heart-rate`
    (`beatsPerMinute`), `steps` (`count`), `weight` (`weightGrams`/1000),
    `sleep` (`summary.minutesAsleep`, else the interval), `heart-rate-variability`
    (`rootMeanSquareOfSuccessiveDifferencesMilliseconds`) and
    `oxygen-saturation` (`percentage`), each cast to a number because Google
    sends 64-bit integers as strings. `export::schema()` is the copy of this
    that the result carries; a test compares it with the loader's real schema.
  - Result: never a value. `path`, `rows` per type, `total_rows`,
    `file_bytes`, `from`, `to`, `timezone`, `tables`, `views`, `columns`,
    `meta`, `how_to_query` (there is no `sqlite3` CLI in the image; use
    `python3 -c "import sqlite3 ..."` or `run_code`; loading all raw JSON into
    pandas can exhaust the sandbox's 1 GiB), `expires` (the file lives only in
    that session's sandbox, deleted after 30 idle minutes) and a `privacy`
    note.
  - Exfiltration note: `health_export` puts the user's whole health history
    in a sandbox that runs as root with internet egress and that any tailnet
    device able to reach OpenSandbox can read, until the sandbox expires.
    That is why it needs a typed user message, is refused on scheduled turns,
    and is described to the model as "only when the user asked". It does not
    stop prompt injection in a page the model reads in the same session from
    asking the model to send the file; the preamble and the sandbox's
    accepted-risk note (`sandbox.rs`) apply.

**Telegram.** `/connect_health` and `/disconnect_health` (private chat only,
like every command), both in the command menu. Disconnecting asks Google to
revoke the refresh token (`POST https://oauth2.googleapis.com/revoke`, token
in the body), then deletes the connection row, so the sealed token is gone,
and the user's pending links; it says whether Google confirmed. `health_daily`
and `health_points` rows stay. The preamble tells the model to call `health_summary` for recovery,
sleep and activity when advising on training.

## Daily brief

An opt-in morning message on Telegram: what is due today, last session's
numbers with progression, recent recovery and yesterday's calories. No new
crate; no confirmation code (see below).

**Schema** (migration 15, after Google Health, 14). One table, no existing
table changed: `daily_briefs(user_id PRIMARY KEY REFERENCES users(id) ON
DELETE CASCADE, local_time TEXT 'HH:MM', enabled INTEGER 0/1, last_sent_date
TEXT NULL, last_attempt_at INTEGER NULL, last_error TEXT NULL, created_at,
updated_at)`. `local_time` is on the user's wall clock; the zone is
`user_settings.timezone`, read each pass, so it follows `timezone_set` and
daylight saving. `last_sent_date` is the user's local date the brief was
last *claimed* for.

**Tools** (names in `brief::NAMES`, always reserved and registered with the
native tools; the owner is the session's user):

- `daily_brief_set(time)`: `HH:MM`, 24-hour, strictly; needs a Telegram
  identity. Upserts and switches on. If that time has already passed today,
  today is marked done, so the first brief is tomorrow's; if one was already
  claimed today, changing the time or switching off and on does not send
  another. Returns `time`, `timezone`, `next_brief`.
- `daily_brief_off()`: `{status: off, was_on}`.
- `daily_brief_status()`: `status`, `time`, `timezone`, `last_brief_date`,
  `last_error`, `next_brief`.

**Why no confirmation code.** An `agent_task` reminder runs a prompt the
model or user wrote, with the user's authority, so it needs a code only the
user can type. The brief's prompt is a constant in code (`brief::prompt`);
the only inputs are two dates the host computes. Nothing a user, a page or the
model wrote can change what the turn is told to do, and enabling it only
needs the user's own request for a time. The turn is `Request.scheduled`, so
its `UserText` is empty and it cannot confirm anything (a task, a skill).

**Delivery.** `Jobs::system(now)` (after the Google Health pass, in the same
call) runs `Telegram::daily_brief`, in `athena telegram` only. For each
enabled brief with a Telegram chat, `brief::phase(now, zone, time,
last_sent_date, health_pending)`:

- `Done`: `last_sent_date` is today's local date.
- `NotYet`: before today's `local_time` (a time a clock change skips moves
  later; computed per pass with jiff, so it is DST-correct).
- `WaitingForHealth`: Google Health is configured and the user's connection
  is `connected` but last synced on an earlier local date: wait until
  `local_time + 30 min`, then send without it. A revoked or absent
  connection never waits. The brief's prompt says to leave recovery out when
  health is unavailable, and that data may be old when `last_synced_at` is
  not today.
- `Send`: due.
- `Missed`: more than 4 hours past `local_time` (or past local midnight):
  skipped silently for that day, never made up.

A due brief takes a permit of the shared `TASKS` semaphore and the user's
`Busy` slot with `try_acquire`/`claim`: if either is not free nothing changes
and the next tick (30 s) tries again until the window closes; nothing waits
while holding the other. Only then is the day claimed, by `UPDATE ... SET
last_sent_date = today WHERE enabled = 1 AND last_sent_date IS NOT today`
(compare-and-set, so overlapping ticks and processes cannot both win). It
is claimed before the turn, so a crash or a failed send loses that day's
brief and never repeats it (as a reminder run). Then one scheduled turn runs
in the user's current session and the reply is sent as any turn's. A
Telegram refusal that `jobs::permanent` recognises (403, blocked, chat not
found) sets `enabled = 0` and records `last_error`; a failure to plan (an
unresolvable zone) records `last_error` and leaves it on.

**Cost.** At most one turn per user per local day, no retry after a send
failure. Known gaps, not fixed here: the agent has no per-turn model-call cap
(`default_max_turns(usize::MAX)`), and a scheduled turn still has the agent's
write tools; the fixed prompt tells the model to use three read tools and to
treat results as data, but that is a prompt, not a filter (the same exposure
as an `agent_task`). A zone change can shift a brief by up to a day (west:
two within 24 h; east: one skipped) since the claim is by local date.

## Compaction

| Name | Value | Owner |
|---|---|---|
| Table | `compactions(session_id, through_seq, summary, model, input_tokens, output_tokens, created_at)`, primary key `(session_id, through_seq)`; the summary stands in for every `messages` row of the session up to and including `through_seq`; the newest is the highest `through_seq`; rows are never deleted or edited | `store.rs` migration 7 |
| Loading | a turn loads the newest summary (one user message) then the rows after its `through_seq`; `Store::load` (history) returns every row | `store::SqliteMemory` |
| Checkpoint rule | `through_seq` must name a row that exists, and the row after it must not be a tool result (`Store::save_checkpoint` refuses otherwise); a checkpoint not newer than the newest is ignored | `store.rs` |
| Summary runs | a `runs` row per summary call: `model` is `ATHENA_COMPACT_MODEL`, `last_seq = first_seq - 1` (no messages; so is a turn that failed, so the discriminator is `calls_json`), `calls_json` is `[{"purpose": "compaction"}]`, status `error` when the call failed. They count in `runs` in `usage` and `GET /usage`. A reader that wants turns only filters `calls_json` or `last_seq >= first_seq AND model_calls > 0` | `compaction::SummaryRun` |
| Window source | `GET https://openrouter.ai/api/v1/models` (no key): `data[].id`, `data[].context_length` (the model's largest, not the smallest provider's); fetched on first use by a request of at least 6000 tokens, kept for the process; a failed lookup assumes 128000 for 10 minutes; 5 s timeout | `compaction::Compactor::window` |
| Request parameter | every request the agent sends to OpenRouter carries `plugins: [{"id": "context-compression", "enabled": false}]`; the summary call does not | `agent::openrouter_params` |
| Summary limits | summarizer sees each message part clipped to 8000 characters, tool results labelled untrusted; 120 s timeout; the call's `max_tokens` is a tenth of the window (256 to 4000) and a summary over that by a quarter (4 chars a token) is rejected like a failed one; a failed or useless attempt holds the session off until the request is a twentieth of the window bigger; kept verbatim: at least a fifth of the window | `compaction` |
| Summary message | one user message: a header saying these are notes and no instruction from the user, then the summary | `compaction::summary_message` |

## Release artifact (GHCR via ORAS)

- **Repository:** `ghcr.io/queryplanner/athena`
- **Artifact type:** `application/vnd.athena.release.v1`
- **Contents:** two files, `athena` and `deploy-gate` (pushed by bare name from
  the directory holding them, so `oras pull -o DIR` writes `DIR/athena`), both
  `x86_64-unknown-linux-gnu`, built on `ubuntu-22.04`, with media type
  `application/octet-stream`.
- **Tags:**
  - `sha-<full git sha>` on every push to `main`;
  - `v*` added to the same digest on release tags.
- **Annotation:** `org.opencontainers.image.source=https://github.com/QueryPlanner/athena`,
  so the package links to the repo.
- **Annotation:** `org.opencontainers.image.revision=<full git sha>`
  (`oras push --annotation` on the manifest). `deploy-gate` writes it into
  `ATHENA_VERSION`. Without it the version is `sha256:<first 12 hex digits>`.
- **Pull:** anonymous `oras pull ghcr.io/queryplanner/athena@sha256:...` works once
  the package is made public (one-time human step).

## Files between Telegram, the model and the sandbox

| Name | Value | Owner |
|---|---|---|
| Inbox in each sandbox | `/tmp/athena-inbox/<8 hex>-<safe name>` (`sandbox::INBOX_DIR`) | `Sandboxes::stage` |
| Health export in each sandbox | `/tmp/athena-data/health.sqlite` (`health::export::DB_PATH`), built through `/tmp/athena-data/.export-<nonce>/` (chunks, `load.py`, `meta.json`; removed when done) | `health_export` |
| Browser state in each sandbox | `/tmp/athena-browser-state.json` (`sandbox::login::STATE_PATH`), agent-browser `state save` JSON | `sandbox::login` |
| Sign-in page screenshot | `/tmp/athena-viewer.png` (`sandbox::login::SCREEN_PATH`) | `Sandboxes::screen` |
| Sign-in tables | `browser_links(token, session_id, url, expires_at)`, links valid 1 h; `browser_states(user_id, state, saved_at)`, one per user | `store.rs` migration 6 |
| Bot API methods | `getFile`, file download `GET /file/bot<token>/<file_path>`, `sendPhoto`, `sendDocument` (multipart) | `telegram.rs` |
| Reply format | `sendMessage` with plain `text` and `entities` (UTF-16 offsets), no `parse_mode`; model Markdown rendered by `telegram::render::render`; a chunk answered "Bad Request" is resent as plain text without entities | `telegram/render.rs`, `telegram.rs` |
| Message limit | 4096 UTF-16 units after parsing (`telegram::MESSAGE_LIMIT`), at most 8 messages (`telegram::MAX_CHUNKS`) | Bot API |
| Download limit | 20 MB (`telegram::DOWNLOAD_LIMIT`) | Bot API |
| Voice notes | `voice` and `audio` messages; refused before download over 2 MB (`telegram::voice::VOICE_LIMIT`) or 5 minutes (`VOICE_SECONDS`), and the download stops past 2 MB; one at a time (`telegram::VOICES`); audio never stored | `telegram.rs` |
| Transcription | `POST https://api.cloudflare.com/client/v4/accounts/<CLOUDFLARE_ACCOUNT_ID>/ai/run/@cf/openai/whisper-large-v3-turbo`, JSON `{"audio": <base64>, "task": "transcribe"}`; needs HTTP 2xx, `success: true` and a non-blank `result.text`; connect timeout 5 s, total 90 s, no redirects, answer read up to 256 KiB | `telegram/voice.rs` |
| Albums | collected by `(user, media_group_id)` until 2 s pass with no new item (`telegram::ALBUM_WAIT`), at most 10 items, one turn | `telegram.rs` |
| Upload limits | photo 10 MB, document 50 MB, caption 1024 chars, 10 files a turn | `sandbox::tools`, `media::MAX_ATTACHMENTS` |
| Image shown to the model | PNG, JPEG, GIF, WebP up to 3.75 MB; 4 a request | `media` |
| Stored transcripts | images replaced by `[image not kept in the transcript]` | `store::append` |
| Exported spans | base64 runs over 1024 chars replaced by `[N characters of base64 omitted]` | `telemetry::Redacted` |

The sandbox image is a separate Docker image:
- repository `ghcr.io/queryplanner/athena-sandbox`, tags `latest` and `sha-<sha>`;
- built from `deploy/sandbox-image/Dockerfile` by CI on push to `main`;
- includes execd-compatible tooling, `agent-browser` (pinned) and Chromium;
- ships agent-browser's usage guides in `/usr/local/share/agent-browser/skill-data`, found through `AGENT_BROWSER_SKILLS_DIR`, for `agent_browser skills get core`.

## VM layout

| Path | Owner / mode | Contents |
|---|---|---|
| `/opt/athena/bin/deploy-gate` | root, 0755 | installed by `setup-host.sh` from a pinned artifact digest |
| `/opt/athena/releases/<digest-hex>/{athena,deploy-gate}` | root | cache, pruned to `ATHENA_KEEP_RELEASES` (default 3); in-use releases are never pruned |
| `/opt/athena/<env>/current` | root | symlink to a releases dir |
| `/etc/athena/<env>.env` | root:athena, 0640 | secrets and settings per env |
| `/etc/athena/gate.env` | root, 0644 | `ATHENA_REPO=ghcr.io/queryplanner/athena`, `ATHENA_KEEP_RELEASES=3`, `ORAS=/usr/local/bin/oras`, `ATHENA_DEPLOY_MODE=staged` |
| `/var/lib/athena/<env>/agent.db` | athena | the database |
| `/var/lib/athena/<env>/backups/` | athena | last 10 per env |
| `/var/lib/athena/gate/<env>.state.json` | root, 0644 (dir root 0755) | written by deploy-gate, outside the athena-writable `<env>/` dir because `promote` trusts it: `{"digest":..., "version":..., "deployed_at":...}` |
| `/var/lib/athena/<env>/telemetry/{traces,logs}-<role>-YYYYMMDD.jsonl` | athena, dir 0750, files 0640 | Athena's own export (`ATHENA_TELEMETRY_DIR`), one file per signal per process role (`serve`, `telegram`, `cli`) per UTC day, pruned after `ATHENA_TELEMETRY_RETENTION_DAYS` |
| `/var/lib/openobserve/` | openobserve | OpenObserve data |

**Users**
- `athena` (system user) runs the services.
- `deploy` (system user with a shell) is used only through forced-command SSH keys.
- sudoers (file `/etc/sudoers.d/athena-deploy`): `deploy ALL=(root) NOPASSWD: /opt/athena/bin/deploy-gate *`,
  plus `Defaults!/opt/athena/bin/deploy-gate env_keep += "SSH_ORIGINAL_COMMAND"`
  (sudo's `env_reset` drops it otherwise, and every CI command is rejected).
- `/var/lib/athena` itself is root-owned 0755; `<env>/` under it is athena's.

**Ports** (all bound to the VM's tailnet IP unless noted)

| Service | Port |
|---|---|
| prod serve | 18080 |
| staging serve | 18081 |
| OpenObserve UI/API and OTLP/HTTP ingest | 5080 (one address: `ZO_HTTP_ADDR` takes a single IP) |

## systemd

- `athena-serve@.service`, where `%i` is `staging` or `prod`:
  - `User=athena`, `EnvironmentFile=/etc/athena/%i.env`;
  - `ExecStart=/opt/athena/%i/current/athena serve`;
  - `After=network-online.target tailscaled.service`;
  - `Restart=on-failure`, `RestartSec=5`, `TimeoutStopSec=120`;
  - hardening: `ProtectSystem=strict`, `ReadWritePaths=/var/lib/athena/%i`,
    `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`, `ProtectProc=invisible`,
    `MemoryMax=128M` (measured idle RSS of `athena serve`: ~15 MB).
- `athena-telegram@.service`: the same, with `ExecStart=… telegram`. It is
  **enabled for an env only when that env's file has a `TELEGRAM_BOT_TOKEN`**.
  Each env has its own bot, because one token can't be polled by two processes.
- `athena@.target`: `Wants=` both units, so `systemctl start athena@staging.target`
  works.
- `openobserve.service` comes from `deploy/systemd/openobserve.service` (binary in
  `/usr/local/bin/openobserve`).

## `deploy-gate` protocol

**Invocation.** `authorized_keys` for `deploy` holds one key per env:

```
restrict,command="sudo /opt/athena/bin/deploy-gate --key-env staging" ssh-ed25519 AAAA... ci-staging
restrict,command="sudo /opt/athena/bin/deploy-gate --key-env prod" ssh-ed25519 AAAA... ci-prod
```

The command comes from `SSH_ORIGINAL_COMMAND`, split on whitespace (no shell). It
must be exactly one of:

| Key | Allowed commands |
|---|---|
| staging | `deploy staging <digest> [KEY=VALUE ...]`, `smoke staging`, `bench staging`, `eval staging`, `status staging` |
| prod | `promote prod <digest> [KEY=VALUE ...]`, `deploy prod <digest> [KEY=VALUE ...]`, `smoke prod`, `status prod` |
| admin (run locally as root, no `--key-env`) | `restore <env> <backup-file>`, `install-gate <digest>` |

**Validation.** The line is at most 1024 bytes. `<digest>` must match
`^sha256:[0-9a-f]{64}$`. Anything else exits 2 with `rejected: ...` on stderr before
any side effect.

**Settings** (`src/gate/settings.rs`). The optional `KEY=VALUE` words after the
digest are non-secret settings written into that env's file:

- Keys: `OPEN_SANDBOX_URL`, `ATHENA_SANDBOX_IMAGE`, `ATHENA_SANDBOX_TIMEOUT_SECS`,
  `AGENT_MODEL`. Any other key is rejected, as is a key given twice.
- Values: 1 to 200 bytes of `[A-Za-z0-9._:/@+-]`. `OPEN_SANDBOX_URL` must be an
  http or https URL with no user or password; `ATHENA_SANDBOX_TIMEOUT_SECS` a
  number of at least 60, as `athena` reads them.
- A key given is set; a key not given is left alone. CI never removes a key: delete
  it with `sudoedit`.
- CI sends each GitHub environment's own variables (`vars.<KEY>` in `staging` or
  `prod`). Settings are not promoted from staging to prod.
- Old and new values are logged and returned in the JSON line (`settings`).

**`deploy staging <digest>`**
1. Take an exclusive lock on `/var/lib/athena/.gate.lock`.
2. Check free disk is at least 1 GiB.
3. `oras pull $ATHENA_REPO@<digest> -o /opt/athena/releases/<hex>.tmp`, then
   `chmod 0755` and rename to `/opt/athena/releases/<hex>`.
4. Run `<new>/athena --version`; it must succeed.
5. Stop the units: `systemctl stop athena@staging.target`.
6. If a DB exists and there is a previous release, run `<old>/athena backup
   /var/lib/athena/staging/backups/<utc-ts>.db` as user `athena`, with the env file
   loaded. Keep the last 10.
7. Point the `current` symlink at the new release (atomically: write a temp
   symlink, then rename).
8. Write the settings and `ATHENA_VERSION` into the env file in one atomic
   rename, keeping its mode and owner and every other line.
9. `systemctl start athena-serve@staging`, then poll
   `http://<ATHENA_ADDR>/health` and `/version` for up to 60 s.
10. Start telegram if its unit is enabled for that env.
11. Write `state.json` (`/var/lib/athena/gate/<env>.state.json`), then prune releases.

On a failed health check it puts the env file back exactly as it was before the
deploy, then the previous `current`, restarts, and exits 1. With no previous release
it restores the file and stops the env. The health check does not exercise the
settings (a wrong `OPEN_SANDBOX_URL` still passes it and shows only when a tool
runs), so a bad setting is rolled back only together with a bad release.

**`promote prod <digest>`**
- Refuses unless `<digest>` equals staging's `state.json` digest.
- Then runs the same steps (step 3 is skipped when the release is already cached).

**`deploy prod <digest>`**
- Refuses unless root-owned `/etc/athena/gate.env` sets `ATHENA_DEPLOY_MODE=direct`.
  Missing mode means `staged`; invalid or empty values fail configuration validation.
- Runs the same deployment, backup, health/version, rollback and pruning steps
  without requiring staging state. `promote prod` still requires staging in either mode.
- Both deployment paths hold the gate lock while checking policy and deploying.

**Other commands**

| Command | Does |
|---|---|
| `smoke <env>` | GET `/health` and `/version`; POST a session and one real message as user `smoke`; exit non-zero on failure |
| `bench staging` | runs `<current>/athena bench --url http://<addr> …` with default thresholds |
| `eval staging` | runs `<current>/athena eval run --target http://<addr> --cases /opt/athena/staging/current/evals` if present. Advisory: prints results and always exits 0 unless the run itself errors. |
| `status <env>` | prints `state.json` |

**Output.** Human-readable lines on stderr, and one final JSON line on stdout.

## CI/CD (`.github/workflows/ci-cd.yml`, replaces `ci.yml`)

**Secrets and variables**

| Name | Kind | Scope |
|---|---|---|
| `TS_AUTH_KEY` (or `TS_OAUTH_CLIENT_ID` + `TS_OAUTH_SECRET`) | secret | repo. Auth key: reusable, ephemeral, ideally pre-tagged `tag:ci`; expires, so rotate it. The OAuth client doesn't expire. |
| `DEPLOY_SSH_KEY` | secret | environment `staging`; another in environment `prod` |
| `VM_HOST` | variable | e.g. `100.124.202.79` |
| `VM_KNOWN_HOSTS` | variable | the VM's host key line |
| `ATHENA_DEPLOY_MODE` | variable, optional | repository only; `staged` by default or `direct`. Unknown values fail. Direct mode also requires the VM administrator's opt-in in `gate.env`. |
| `OPEN_SANDBOX_URL`, `ATHENA_SANDBOX_IMAGE`, `ATHENA_SANDBOX_TIMEOUT_SECS`, `AGENT_MODEL` | variable, optional | environment `staging` and/or `prod`; sent with that env's deploy (see Settings). Never a secret. |

**Jobs**
- **pull_request:** `unit` (fmt, clippy, coverage.sh, `athena eval run --target
  replay`), then `integration` (release build plus a tests-style smoke run of the real
  binary; no model calls).
- **all events:** `policy` validates the repository deployment mode and tests
  release admission. Its output fixes the mode for that workflow run.
- **push to main:** infrastructure, unit/coverage/replay, integration and sandbox
  checks must pass before `build` publishes `sha-<sha>`. In `staged` mode,
  `deploy-staging`, `bench-staging`, `smoke-staging` and advisory `eval-staging`
  follow. In `direct` mode these staging jobs are skipped; staging stays installed.
- **push tag `v*`:** `require-release-success` checks one successful main run for
  the exact tagged SHA. Paginated job evidence must show policy, infrastructure,
  unit, integration, build and sandbox-image success. Staged mode also requires
  deploy-staging, bench-staging and smoke-staging success; eval stays advisory.
  `deploy-prod` names the validated artifact by version, then calls `promote prod`
  in staged mode or `deploy prod` in direct mode, followed by `smoke prod`.
  Production remains restricted to release tags.
- The release decision is pushing the tag. The `release-tags` ruleset lets only
  repo admins create, move or delete `v*` tags.

**Rules**
- Actions are pinned to commit SHAs.
- Permissions are minimal per job.
- No `pull_request_target`.

## Evals

- **Cases:** `evals/cases/*.json`. The JSON schema is in `src/eval/` docs:
  `eval_case_id`, `kind`, `tags`, `turns[]`, `expect{trajectory, output, rubric}`,
  `thresholds`, `cassette`.
- **Cassettes:** `evals/cassettes/<case>.json`.
- **Results:** JSONL rows with the fields in plan section 9.

## Telemetry

Setup B: no collector. Athena exports itself, to two independent sinks.

- OTLP over HTTP (protobuf) to `OTEL_EXPORTER_OTLP_ENDPOINT`, with
  `OTEL_EXPORTER_OTLP_HEADERS`; on the VM that is OpenObserve.
- JSONL files under `ATHENA_TELEMETRY_DIR`. One object per line, flat
  schema (documented in `src/telemetry/jsonl.rs`). Spans: `trace_id`,
  `span_id`, `parent_span_id`, `name`, `kind`, `start_unix_nano`,
  `end_unix_nano`, `duration_ms`, `status` (`unset|ok|error`),
  `status_message`, `attributes` (object), `events`, `scope`, `resource`
  (object with `service.name`, `service.version`,
  `deployment.environment.name`). Logs: `time_unix_nano`, `severity`,
  `severity_number`, `target`, `body`, `trace_id`, `span_id`, `attributes`,
  `scope`, `resource`. `analytics/queries/spans.sql` reads this schema.
- Every environment exports everything, with no switch (PR #21): prompts,
  replies, the system prompt and tool arguments and results
  (`gen_ai.input.messages`, `gen_ai.output.messages`,
  `gen_ai.system_instructions`, `gen_ai.tool.call.arguments`,
  `gen_ai.tool.call.result`, `gen_ai.prompt`) stay on spans and log events.
  The one exception is on spans and span events: base64 runs over 1024
  characters are replaced (`telemetry::Redacted` wraps the span exporter
  only). Log records are exported as they are, so a long base64 value in a
  log field reaches OpenObserve and the JSONL log files whole.
- Staging and prod share one OpenObserve organization (`default`) and its
  default streams. What tells them apart is the resource attribute
  `deployment.environment.name`, from `ATHENA_ENV`. OpenObserve turns dots in
  attribute names into underscores: filter traces on
  `service_deployment_environment_name` and logs on
  `deployment_environment_name`.
- Each turn gets an Athena `invoke_agent` span carrying `gen_ai.conversation.id`,
  `athena.run_id`, `athena.transport`, and `enduser.pseudo.id` (a hash).
- rig's own spans nest under it.
- Logs go through `tracing`: plain text to stderr always (so the CLI REPL
  stays readable), plus each sink that is on.
- HTTP accepts W3C `traceparent` and, when OTel is on, answers with
  `x-trace-id` and `traceparent` response headers.
