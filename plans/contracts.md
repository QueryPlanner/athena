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
| `OTEL_EXPORTER_OTLP_ENDPOINT` | all | e.g. `http://127.0.0.1:4318`. **Unset means telemetry is off**, with plain stderr logs. |
| `OTEL_SERVICE_NAME` | all | default `athena` |
| `ATHENA_RECORD_CONTENT` | all | `1` records prompt and response content on spans. Default off. |

## HTTP

`GET /version` returns `{"version": "<ATHENA_VERSION or pkg-dev>"}`. Like `/health`,
it needs no user header. Every other endpoint is unchanged.

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

The sandbox image is a separate Docker image:
- repository `ghcr.io/queryplanner/athena-sandbox`, tags `latest` and `sha-<sha>`;
- built from `deploy/sandbox-image/Dockerfile` by CI on push to `main`;
- includes execd-compatible tooling, `agent-browser` (pinned) and Chromium.

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
| `/var/lib/athena/otel/{traces,logs}.jsonl` | otelcol | collector file export, rotated |
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
| OpenObserve UI/API | 5080 |
| otelcol OTLP | 127.0.0.1:4318 (HTTP) and 127.0.0.1:4317 (gRPC) |

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
  **enabled for prod only**.
- `athena@.target`: `Wants=` both units, so `systemctl start athena@staging.target`
  works. For staging, only serve is enabled.
- `otelcol-contrib.service` comes from the `.deb`, with config at
  `/etc/otelcol-contrib/config.yaml` (from `deploy/otel/config.yaml`).
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
| staging | `deploy staging <digest>`, `smoke staging`, `bench staging`, `eval staging`, `status staging` |
| prod | `promote prod <digest>`, `smoke prod`, `status prod` |
| admin (run locally as root, no `--key-env`) | `restore <env> <backup-file>`, `install-gate <digest>` |

**Validation.** `<digest>` must match `^sha256:[0-9a-f]{64}$`. Anything else exits 2
with `rejected: ...` on stderr before any side effect.

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
8. Write `ATHENA_VERSION` into the env file, replacing the existing line.
9. `systemctl start athena-serve@staging`, then poll
   `http://<ATHENA_ADDR>/health` and `/version` for up to 60 s.
10. Start telegram if its unit is enabled for that env.
11. Write `state.json` (`/var/lib/athena/gate/<env>.state.json`), then prune releases.

On a failed health check it puts the previous `current` back, restarts, and exits 1.

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
| `TS_OAUTH_CLIENT_ID`, `TS_OAUTH_SECRET` | secret | repo (Tailscale OAuth client, tag `tag:ci`) |
| `DEPLOY_SSH_KEY` | secret | environment `staging`; another in environment `prod` |
| `VM_HOST` | variable | e.g. `100.124.202.79` |
| `VM_KNOWN_HOSTS` | variable | the VM's host key line |

**Jobs**
- **pull_request:** `unit` (fmt, clippy, coverage.sh, `athena eval run --target
  replay`), then `integration` (release build plus a tests-style smoke run of the real
  binary; no model calls).
- **push to main:** `build` (release build, `oras push` tag `sha-<sha>`, sandbox image
  build and push), then `deploy-staging` (environment `staging`), then
  `bench-staging`, then `smoke-staging` + `eval-staging` (advisory).
- **push tag `v*`:** `require-stage-success` (the tagged sha's main run must be
  green), then `deploy-prod` (environment `prod`, required reviewer; `oras tag` the
  digest with the version, `promote prod`), then `smoke prod`. The prod smoke runs
  as a step of `deploy-prod`: a separate job on the `prod` environment would ask
  the reviewer to approve a second time.

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

- OTLP over HTTP to `OTEL_EXPORTER_OTLP_ENDPOINT`.
- Each turn gets an Athena `invoke_agent` span carrying `gen_ai.conversation.id`,
  `athena.run_id`, `athena.transport`, and `enduser.pseudo.id` (a hash).
- rig's own spans nest under it.
- Logs go through `tracing`: plain text to stderr always (so the CLI REPL
  stays readable), plus OTLP when on.
- HTTP accepts W3C `traceparent` and, when OTel is on, answers with
  `x-trace-id` and `traceparent` response headers.
