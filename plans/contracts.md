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
| `OPEN_SANDBOX_URL` | agent | e.g. `http://100.118.54.67:9090`. **Unset means the sandbox tools are not registered** (dev and tests keep working). |
| `OPEN_SANDBOX_API_KEY` | agent | optional; sent as the `OPEN-SANDBOX-API-KEY` header when set |
| `ATHENA_SANDBOX_IMAGE` | agent | default `ghcr.io/queryplanner/athena-sandbox:latest` |
| `ATHENA_SANDBOX_TIMEOUT_SECS` | agent | default `1800`, minimum 60 |
| `ATHENA_PUBLIC_URL` | agent | base of sign-in links, e.g. `http://100.124.202.79:18080`; default `http://$ATHENA_ADDR` (so `serve` and `telegram` agree from the one env file). Must be http(s); required when `ATHENA_ADDR` is unspecified (`0.0.0.0`, `[::]`). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | all | OTLP/HTTP base URL; on the VM OpenObserve, `http://<tailnet-ip>:5080/api/default` (`/v1/traces` and `/v1/logs` are appended). **Unset means no OTLP export.** |
| `OTEL_EXPORTER_OTLP_HEADERS` | all | `Authorization=Basic%20<base64 of OpenObserve root email:password>`, written by `setup-host.sh` into the env file only |
| `ATHENA_TELEMETRY_DIR` | all | e.g. `/var/lib/athena/<env>/telemetry`: daily `traces-<role>-YYYYMMDD.jsonl` and `logs-<role>-YYYYMMDD.jsonl`, `<role>` being the process (`serve`, `telegram`, `cli`), so every file has one writer. **Unset means no files.** With neither this nor the endpoint, telemetry is off and logs go to stderr only. |
| `ATHENA_TELEMETRY_RETENTION_DAYS` | all | default `30`; files of older days are deleted |
| `OTEL_SERVICE_NAME` | all | default `athena` |

## HTTP

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
| `POST /browser/{token}/done` | `state save`, stored for the session's owner; answers `{"saved_bytes"}` |

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
| `/etc/athena/gate.env` | root, 0644 | `ATHENA_REPO=ghcr.io/queryplanner/athena`, `ATHENA_KEEP_RELEASES=3`, `ORAS=/usr/local/bin/oras` |
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
| prod | `promote prod <digest> [KEY=VALUE ...]`, `smoke prod`, `status prod` |
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
| `OPEN_SANDBOX_URL`, `ATHENA_SANDBOX_IMAGE`, `ATHENA_SANDBOX_TIMEOUT_SECS`, `AGENT_MODEL` | variable, optional | environment `staging` and/or `prod`; sent with that env's deploy (see Settings). Never a secret. |

**Jobs**
- **pull_request:** `unit` (fmt, clippy, coverage.sh, `athena eval run --target
  replay`), then `integration` (release build plus a tests-style smoke run of the real
  binary; no model calls).
- **push to main:** `build` (release build, `oras push` tag `sha-<sha>`, sandbox image
  build and push), then `deploy-staging` (environment `staging`), then
  `bench-staging`, then `smoke-staging` + `eval-staging` (advisory).
- **push tag `v*`:** `require-stage-success` (the tagged sha's main run must be
  green), then `deploy-prod` (environment `prod`, v* tags only, **no reviewer**;
  `oras tag` the digest with the version, `promote prod`), then `smoke prod` as a
  step of `deploy-prod`.
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
- For `ATHENA_ENV=prod`, `gen_ai.input.messages`, `gen_ai.output.messages`,
  `gen_ai.system_instructions`, `gen_ai.tool.call.arguments`,
  `gen_ai.tool.call.result`, `gen_ai.prompt` and `gen_ai.completion` are
  removed from spans and span events before either sink, and log events
  carrying them are not exported.
- Each turn gets an Athena `invoke_agent` span carrying `gen_ai.conversation.id`,
  `athena.run_id`, `athena.transport`, and `enduser.pseudo.id` (a hash).
- rig's own spans nest under it.
- Logs go through `tracing`: plain text to stderr always (so the CLI REPL
  stays readable), plus each sink that is on.
- HTTP accepts W3C `traceparent` and, when OTel is on, answers with
  `x-trace-id` and `traceparent` response headers.
