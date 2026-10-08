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
  (`agent::reserved_tool_names`: `add`, `read_skill`, the calorie tools, the sandbox tools) or an earlier server (name order) has, that
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
| Browser state in each sandbox | `/tmp/athena-browser-state.json` (`sandbox::login::STATE_PATH`), agent-browser `state save` JSON | `sandbox::login` |
| Sign-in page screenshot | `/tmp/athena-viewer.png` (`sandbox::login::SCREEN_PATH`) | `Sandboxes::screen` |
| Sign-in tables | `browser_links(token, session_id, url, expires_at)`, links valid 1 h; `browser_states(user_id, state, saved_at)`, one per user | `store.rs` migration 6 |
| Bot API methods | `getFile`, file download `GET /file/bot<token>/<file_path>`, `sendPhoto`, `sendDocument` (multipart) | `telegram.rs` |
| Reply format | `sendMessage` with plain `text` and `entities` (UTF-16 offsets), no `parse_mode`; model Markdown rendered by `telegram::render::render`; a chunk answered "Bad Request" is resent as plain text without entities | `telegram/render.rs`, `telegram.rs` |
| Message limit | 4096 UTF-16 units after parsing (`telegram::MESSAGE_LIMIT`), at most 8 messages (`telegram::MAX_CHUNKS`) | Bot API |
| Download limit | 20 MB (`telegram::DOWNLOAD_LIMIT`) | Bot API |
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
