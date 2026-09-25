# Athena production template: plan

Status: draft v3. It went through adversarial review, then was revised with the
owner's answers. Nothing is implemented yet.

**Goal.** Fork the template, work through a checklist of about 45 minutes, and end up
with a production-ready agent. That means:
- CI/CD, with staging and prod environments;
- a sandbox and a browser for the agent;
- OTel traces, evals, and analytics.

**Constraints.**
- Open-source tools only. GHCR is used as the release store, via ORAS; no Docker on the VM.
- One Linux VM on a Tailscale tailnet, deployed from GitHub Actions.

**Owner decisions.**

| Area | Decision |
|---|---|
| Repo | `QueryPlanner/athena` is **public** |
| VM | Hetzner **x86_64**, 2 vCPU / 4 GB |
| Environments | **staging + prod only**, both on the VM |
| Sandbox | OpenSandbox on the owner's laptop (24 GB RAM, on the tailnet). It currently has no API key and allows egress. |
| Browser | agent-browser |
| Dependencies | obvious dependencies may be added once the plan is approved |
| HTTP auth | undecided (section 10) |

---

## 1. Topology

```
                         Tailscale tailnet (ACLs = the perimeter)
┌───────────────────────────── athena-vm  tag:athena-server ─────────────────────────────┐
│ systemd (no Docker):                                                                   │
│   athena-serve@prod      <tailnet-ip>:18080   (listens on the tailnet IP only)        │
│   athena-telegram@prod                                                                 │
│   athena-serve@staging   <tailnet-ip>:18081                                           │
│   otelcol-contrib (.deb)  → JSONL files → DuckDB   (+ OpenObserve in M3)                │
│ /opt/athena/releases/<digest>/      (cache of the last 3; GHCR holds all releases)     │
│ /opt/athena/bin/deploy-gate         (forced SSH command; the only thing CI can run)    │
└────────────────────────────────────────────────────────────────────────────────────────┘
      ▲ ssh :22 (forced cmd)                                 │ :9090 (execd via server proxy)
      │                                                      ▼
 GitHub Actions runner (ephemeral, tag:ci)       owner's laptop  tag:sandbox
                                                 OpenSandbox + Docker (gVisor if available)
 other apps on the tailnet (tag:athena-client)   per-session image: athena-sandbox
   ──► :18080                                    (tools + agent-browser + Chrome)
```

### What the Tailscale tags mean

A **Tailscale tag** is an identity for a *machine* rather than a *person*.
- A device joined with `--advertise-tags=tag:x` stops belonging to your user account
  and becomes "a `tag:x` machine".
- Access rules (ACLs) then say which tags may talk to which, on which ports.
- `tagOwners` says who may create devices with a tag; here that is only you.
- Tagged devices don't expire their node key, which you want for servers.

These are unrelated to **git release tags** (`v1.2.0`) in section 2.

| Tag | Which machine | Why it exists | Can reach |
|---|---|---|---|
| `tag:athena-server` | the Hetzner VM | runs staging and prod | the sandbox server, `:9090` |
| `tag:ci` | a GitHub Actions runner for the few minutes of a deploy job. It joins with `tailscale/github-action`, is **ephemeral** (removed when the job ends) and authenticates by OIDC, so no Tailscale secret is stored in GitHub. | lets CI reach the VM without opening a public port | the VM, `:22` only, where the SSH key can only run `deploy-gate` |
| `tag:sandbox` | your laptop running OpenSandbox | code and browser execution | nothing; it only answers requests |
| `tag:athena-client` | future apps that call Athena's HTTP API | the auth "stage 0" gate (section 10) | the VM, `:18080` (prod) |
| (your user, `autogroup:admin`) | your laptop and phone as a person | admin | everything |

The policy file is `deploy/tailscale-policy.hujson`, pasted into the admin console
once:

| From | To | Ports |
|---|---|---|
| `tag:ci` | `tag:athena-server` | `22` |
| `tag:athena-server` | `tag:sandbox` | `9090` |
| `tag:athena-client` | `tag:athena-server` | `18080` |
| `autogroup:admin` | `*` | `*` |

- Everything not listed is denied. In particular, a sandbox cannot reach the VM.
- **Existing machines don't need re-tagging.** On a VM that is already on the tailnet
  as your user (like the owner's), the policy can name it through a `hosts` alias
  (`"athena-vm": "100.124.202.79"`) instead of `tag:athena-server`. Re-tagging an
  existing node changes its identity and could break access your other apps rely
  on. Tags are the default only for fresh machines; `setup-host.sh` detects which
  case applies.
- CI cannot call the staging API directly. Smoke and eval run on the VM through
  `deploy-gate`.

## 2. CI/CD: the pipeline diagram plus release-tag promotion (agent-foundation pattern)

This mirrors `doughayden/agent-foundation`'s `ci-cd.yml`:
- PR runs checks;
- merge builds and deploys to stage, then smoke-tests;
- **pushing a `v*` tag** confirms that the tagged commit already passed stage, and
  promotes the **same artifact** to prod behind a `prod` environment with required
  reviewers.

Required reviewers work on public repos on every GitHub plan.

A single `.github/workflows/ci-cd.yml` holds the job graph. Reusable workflows
(`workflow_call`) hold the steps.

| Trigger | Pipeline box | Jobs |
|---|---|---|
| `pull_request` | **CI Pipeline** | **Unit tests**: fmt, clippy `-D warnings`, `scripts/coverage.sh` (100% lines), `athena eval run --target replay` (cassettes, $0). **Integration tests**: `cargo build --release`, start the real binary with a temp DB and fake-telegram (+ fake-sandbox later). Exercise every path that doesn't call the model: `/health`, `/version`, sessions, usage, Host allowlist, graceful SIGTERM, `athena backup`, migration of a fixture DB. The model-calling paths are already covered in-process with rig's `MockCompletionModel`, so no fake model server is needed. |
| push to `main` | **CD Pipeline #1** | **Release build**: `cargo build --release --locked` for `x86_64-unknown-linux-gnu` on `ubuntu-22.04` (older glibc, so the binary also runs on newer Debian/Ubuntu), then **publish to GHCR** with ORAS: `oras push ghcr.io/<owner>/athena:sha-<sha>` gives back an immutable digest. **Deploy to staging**: tailnet join (`tag:ci`), then `ssh deploy@athena-vm deploy staging sha256:<digest>`. Only the digest string crosses SSH; the VM **pulls** the binary itself. **Load tests (staging)**: `ssh … loadcheck staging` runs a small, spend-capped real-model profile on the VM (for example 3 concurrent sessions × 2 min with the cheap model) and checks p95 latency and error rate. **Smoke + eval report**: `ssh … smoke staging`, then `ssh … eval staging` (advisory until the judge is calibrated, section 9). |
| push tag `v*` | **CD Pipeline #2** | **require-stage-success**: the tagged SHA's `main` run must show green deploy, load and smoke jobs, otherwise stop. **Deploy to prod**: runs under `environment: prod` with required reviewers (you), so it **waits for approval**. Then `oras tag …@<digest> v1.2.0` (same bytes, now also a release name) and `ssh … promote prod sha256:<digest>`. The VM re-points prod at the release staging validated, pulling it by digest if it isn't in the local cache. **Nothing is rebuilt.** Then a prod smoke test. |

`★ Releases live in GHCR; the VM keeps a small cache`
- **ORAS** (CNCF, Apache-2.0) pushes a plain file (the Rust binary) to an OCI
  registry, so GHCR stores releases without Docker.
  - `sha-<sha>` names every build from `main`.
  - `v1.2.0` names releases, attached to the same digest.
  - The repo is public, so the package is public. Anyone can
    `oras pull ghcr.io/queryplanner/athena:v1.2.0` with no build and no login. That
    makes the "anyone with a VM" path easy.
- The VM pulls **by digest**, which is content-addressed, so what staging tested is
  byte-for-byte what prod runs.
- The VM keeps only the last `ATHENA_KEEP_RELEASES` (default 3) under
  `/opt/athena/releases/<digest>/` for instant rollback. GHCR holds the full
  history.
- **Tested 2026-09-25** with `ghcr.io/queryplanner/athena-oras-probe`:
  - push of an 18.7 MB binary with `--artifact-type application/vnd.athena.release.v1`
    worked;
  - `oras tag` added `v0.0.0-probe` to the same digest without re-uploading;
  - a pull by digest returned identical bytes (sha256 matched).
- **A new package starts private**, even when the
  `org.opencontainers.image.source` annotation links it to the public repo. An
  anonymous pull failed with `unauthorized`.
  - One-time human step in `SETUP.md`: Package settings → Change visibility →
    Public.
  - The alternative is a read-only `read:packages` token on the VM, which is one
    more secret.

**Load testing uses the real model only** (owner decision: fake-model load tests add
nothing). An agent's latency and failures come from the model provider and the tools,
and a fake model measures neither.

The diagram's **Load tests** box is `loadcheck staging`:
- it runs on the VM, so there is no tailnet or DERP noise;
- a few concurrent sessions for a couple of minutes, on the cheap model with the
  spend-capped staging key;
- thresholds on p95 turn latency, error rate and tokens per turn.

It is sized so it doesn't starve prod or the other apps on 2 vCPUs. The concurrency
and duration are variables, so a fork can tune the cost.

**GitHub settings** (set by `scripts/init-github.sh`)

| Setting | Configuration |
|---|---|
| Environment `staging` | deployment branches: `main` only |
| Environment `prod` | deployment tags: `v*` only; **required reviewer** = owner; "prevent self-review" off, so a solo owner can approve |
| Ruleset | protects `v*` tags: only admins create them, no deletion or force-push |
| Branch protection on `main` | PR required, CI green |

**Public-repo hardening**
- Never use `pull_request_target`.
- PRs from forks get no secrets and no `id-token`, so they cannot join the tailnet.
  PR jobs never need it.
- Every action is pinned to a commit SHA.
- `permissions:` minimal per job. `id-token: write` is granted only on deploy jobs;
  `actions: read` on require-stage-success.
- Concurrency group `deploy-${env}`, with `cancel-in-progress: false`.
  `deploy-gate` also takes a `flock`.

**Secrets**

| Where | What |
|---|---|
| GitHub environments | `DEPLOY_SSH_KEY`, one per environment |
| Repo variables | `TS_OAUTH_CLIENT_ID`, `TS_AUDIENCE`, `VM_HOST` |
| VM only (`/etc/athena/<env>.env`, 0600, root) | OpenRouter keys (staging gets its own spend-capped key), the **existing** Telegram bot token (prod only), the sandbox API key once one is set |

## 3. Deployment: systemd, no Docker on the VM

**Why no Docker for Athena.** A Rust release build is a single file.
- `rusqlite` is `bundled`, and TLS is rustls, with no OpenSSL. The binary needs only
  glibc.
- Docker's usual value is packaging a runtime and its dependencies. Here there is
  nothing to package.

| | systemd + binary (chosen) | Docker |
|---|---|---|
| Registry | GHCR via ORAS (a plain binary, no image) | GHCR, holding a container image |
| Isolation | systemd sandboxing: `DynamicUser`, `ProtectSystem=strict`, `ProtectHome`, `ProtectProc=invisible`, `PrivateTmp`, `NoNewPrivileges`, `RestrictAddressFamilies`, `SystemCallFilter=@system-service`, `MemoryMax`, `CPUQuota` | namespaces and cgroups; similar strength, but the `deploy` user in the docker group is root-equivalent |
| RAM | none extra | dockerd + containerd, roughly 100–150 MB |
| Logs | journald, native | docker logs driver |
| Rollback | flip a symlink (local cache), or pull any older digest from GHCR | redeploy the previous digest |
| Obs stack | otelcol-contrib and OpenObserve both ship official `.deb`/single-binary releases with systemd units | compose |
| Downside | no identical local "container" environment | heavier; dockerd plus images on a 4 GB VM |

Docker stays on the **sandbox laptop only**, because OpenSandbox needs it to create
sandboxes.

**Units** (`deploy/systemd/`)
- `athena-serve@.service` (staging and prod) and `athena-telegram@.service` (**prod
  only**), with `%i` = `staging` or `prod`.
  - **No second Telegram bot** (owner decision). Two processes polling one bot token
    conflict (409), so staging runs HTTP only.
  - The Telegram code path is tested in CI against the existing fake Telegram API
    (`tests/telegram/fake_api.rs`), and staging's smoke, load and eval checks go
    over HTTP.

  Each unit uses:
  - `EnvironmentFile=/etc/athena/%i.env`;
  - `ExecStart=/opt/athena/%i/current/athena serve` (or `telegram`);
  - `StateDirectory=athena/%i`, so the DB lives at `/var/lib/athena/%i/agent.db`;
  - `ATHENA_DB` absolute, and `WorkingDirectory=/var/lib/athena/%i`;
  - `TimeoutStopSec=120` (graceful drain), `Restart=on-failure`, plus the hardening
    options above.
- `athena@.target` groups both units, so `systemctl restart athena@staging.target`
  moves them together.
- **Athena listens on the VM's tailnet IP** (`ATHENA_ADDR=<tailnet-ip>:18080`, found
  by `setup-host.sh` with `tailscale ip -4`), the same way `blacki-agent-1` already
  does. It never listens on `0.0.0.0`, so it can't be reached from the public
  internet. No `tailscale serve` is needed:
  - the machine is already on the tailnet;
  - WireGuard already encrypts the traffic, so TLS adds nothing inside the tailnet;
  - the ACL decides who can connect.
  - `tailscale serve` (a reverse proxy in tailscaled that adds a
    `https://<machine>.<tailnet>.ts.net` URL and Tailscale-user identity headers)
    only becomes useful at auth stage 2 (section 10). It is deferred until then.
- Code change (PR 3): today a non-loopback address prints the UNAUTHENTICATED banner
  and accepts any `Host`. **`ATHENA_ALLOWED_HOSTS`** (for example
  `100.124.202.79:18080,athena-vm:18080`) restores the DNS-rebinding protection for
  the tailnet address. The banner is printed only when no allowlist is set.
- Units use `After=tailscaled.service` and `Restart=on-failure` with `RestartSec=5`,
  because binding to the tailnet IP fails until `tailscale0` is up at boot.

**`deploy-gate` is a small Rust binary** in this repo (`src/bin/deploy-gate.rs` plus
a `gate` module), installed as the forced command. It is Rust rather than bash
because it is root-adjacent production code:
- it parses input from CI;
- it drives `systemctl` and backups;
- it deletes old releases.

Being in the repo means `cargo test` and the 100% coverage gate apply to it with no
new test tool. `systemctl`, `oras` and the clock sit behind a small trait, so tests
use fakes. The tests cover:
- injection attempts, and a key reaching the other env;
- pruning never deleting the current release;
- backup before switch;
- rollback when the health check fails;
- the low-disk refusal.

It is installed and updated by `setup-host.sh`, which pulls a **pinned** gate version
from GHCR, separately from app releases. That way a bad app release can never replace
the tool that rolls it back. It validates `SSH_ORIGINAL_COMMAND` against a whitelist
before running anything; the env is fixed by which key was used.

| Key | Allowed commands |
|---|---|
| staging key | `deploy staging <digest>`, `loadcheck staging`, `smoke staging`, `eval staging` |
| prod key | `promote prod <digest>`, `smoke prod` |

Restore is **not** reachable from CI. An admin runs `sudo deploy-gate restore <env>
<backup>` over Tailscale SSH.

`deploy staging <digest>` runs in this order (the digest must match `^sha256:[0-9a-f]{64}$`):
1. Take the `flock`.
2. `oras pull ghcr.io/<owner>/athena@<digest>` into
   `/opt/athena/releases/<digest>/`. The package is public, so no credential is
   stored on the VM. ORAS verifies the content against the digest. `chmod 0755`.
3. Check that the new binary runs: `athena --version`.
4. `systemctl stop athena@staging.target`. SIGTERM lets in-flight turns drain.
5. Back up the DB. The **currently running** binary runs `athena backup
   /var/lib/athena/staging/backups/<ts>.db`, which opens the DB raw and read-only
   with rusqlite's online backup and never calls `Store::open`, since that migrates
   (`src/store.rs:470`). Keep the last 10.
6. Flip `/opt/athena/staging/current` to the new release.
7. Start serve (it migrates), then poll `/health` and `/version` (which must report
   the expected git sha).
8. Start telegram.

`promote prod <digest>` runs steps 2 (skipped when the release is already cached)
and 4–8 for prod. It refuses if `<digest>` isn't the one staging currently runs, or
was never deployed to staging.

If the schema version changed, `deploy-gate` prints a warning banner: rolling back
the binary would require a restore, and data written since the backup would be lost.

**What happens to data on a redeploy.** Nothing stored is lost. The DB lives in
`/var/lib/athena/<env>/agent.db`, outside the release directories, so a new binary
opens the same file.

| Data | Across a redeploy |
|---|---|
| Users, sessions, full message history, runs/usage, each user's selected session | **kept**. Conversations continue with full context, because `SqliteMemory` reloads a session's history from the DB. |
| Schema changes | the new binary migrates forward on start (versioned migrations, `src/store.rs`). `tests/upgrade.rs` asserts that "nothing stored is lost when the schema moves forward", using a fixture for every earlier schema. |
| Turns in progress at stop time | SIGTERM → serve and telegram **finish in-flight turns and reply**, then exit (up to `TimeoutStopSec=120`). |
| Open SSE streams (HTTP clients) | disconnected when serve stops; the client reconnects. The turn itself finished and was stored. |
| Telegram messages sent during the ~seconds of downtime | queued by Telegram and delivered when the bot polls again (Telegram keeps pending updates for up to 24 h) |
| Sandboxes (M2) | live on the laptop, with their ids in the DB. After restart, Athena reuses a session's sandbox if it hasn't expired, otherwise creates a new one. Files in an expired sandbox are gone, by design. |
| Staging vs prod | separate DB files. Staging data never touches prod. |
| Safety net | a DB backup is taken before every switch (last 10 kept). |

The one-way door: after a schema migration, an older binary refuses the newer DB. So
a binary rollback across a migration means restoring the pre-deploy backup, which
loses what was written since. `deploy-gate` warns when a deploy changes the schema
version.

**Rollback**
1. `deploy-gate` keeps the last 3 releases locally, and GHCR keeps all of them.
2. To roll back, push a new tag on the previous good commit, as agent-foundation's
   "hotfix + tag" strategy does. If the release is cached, promotion is instant;
   otherwise it is pulled from GHCR.
3. If the schema moved forward, an admin restores the backup first.

**Where everything lives on the VM.** Everything Athena owns sits under three roots,
so it is easy to find, back up, or remove.

| Path | Contents | Cap |
|---|---|---|
| `/opt/athena/releases/<digest>/athena` | local release cache (~15–19 MB each, measured on a local arm64 build; Linux x86_64 unverified) | the last `ATHENA_KEEP_RELEASES` (default 3, ≈60 MB). A release that staging or prod currently points at is never deleted. GHCR keeps the full history. |
| `/opt/athena/{staging,prod}/current` | symlink to a release | – |
| `/opt/athena/bin/deploy-gate` | deploy script | – |
| `/etc/athena/{staging,prod}.env` | secrets, 0600 root | – |
| `/var/lib/athena/{staging,prod}/agent.db` | SQLite (WAL) | – |
| `/var/lib/athena/{staging,prod}/backups/` | pre-deploy DB backups | the last 10 per env. `deploy-gate` refuses to deploy when free disk is under 1 GB. |
| `/var/lib/athena/otel/` | collector JSONL, rotated at 50 MB | 30 days |

`setup-host.sh --uninstall` stops the units and removes the three roots. It removes
the DB only with `--purge`.

**Host setup: safe on an existing VM.** `scripts/setup-host.sh` is idempotent. It
assumes the VM may already run other things, as the owner's does: 7 Docker apps,
Tailscale, and about 1.9 GB of free RAM.
- **Checks first, changes second.** `scripts/preflight.sh` is read-only and prints a
  JSON report:
  - OS and architecture, systemd, free RAM and disk;
  - whether Tailscale is installed and logged in, and whether the node is tagged or
    user-owned;
  - the tailnet IP, and whether ports 18080/18081 are free on it;
  - whether Docker is present, reported but never touched.

  `setup-host.sh` refuses to continue on a conflict and prints the fix, for example
  `--port-prod 18090`.
- **It never touches what it didn't create.**
  - No Docker changes.
  - No ufw or sshd changes by default. Changing either on a shared VM can cut off
    other apps. `--harden` opts in for fresh single-purpose VMs.
  - Tailscale is reused if present. It only installs Tailscale when missing, and
    only re-tags with `--tag`.
  - No Tailscale config changes at all: no `serve`, no tags (unless `--tag`).
- **What it creates:**
  - the `deploy` user (no docker group; one narrow sudoers rule so `deploy-gate` can
    run `systemctl` on `athena@*` only);
  - the three roots above;
  - the units and `deploy-gate`;
  - the two `authorized_keys` entries with `restrict,command=…`;
  - otelcol-contrib (`.deb`), with `MemoryMax=256M`.
- Hetzner's cloud firewall and Docker-published ports are outside Athena's scope.
  `preflight.sh` still **warns** when it sees services published on `0.0.0.0`.

## 4. Sandbox: OpenSandbox over REST

**Decision:** call OpenSandbox's REST API directly. Not MCP, not microsandbox.
- MCP would hand the LLM `sandbox_create` and `sandbox_kill`, and it adds a Python hop
  (`/mcp` is not served by your server). rig 0.42 *does* support MCP via the
  `rig-agent` feature `rmcp`, so this is a choice.
- microsandbox is embedded: it runs libkrun microVMs on Athena's host and needs KVM.

**Design**
- Remove the host `read_file` tool (`src/agent.rs:16`). It can read
  `/proc/self/environ`, including the API key.
- Add `src/sandbox.rs`, a small reqwest client for:
  - lifecycle: `POST/GET/DELETE /v1/sandboxes`, `renew-expiration`,
    `GET /v1/sandboxes/{id}/endpoints/44772`;
  - execd: `/command`, `/files/*`.
- Configuration: `OPEN_SANDBOX_URL`, `OPEN_SANDBOX_API_KEY` (sent as the
  `OPEN-SANDBOX-API-KEY` header), `ATHENA_SANDBOX_IMAGE`.
- Tools map onto execd's three execution modes (checked in `specs/execd-api.yaml`).
  Each one streams SSE events (`stdout`, `stderr`, `result`, `error`,
  `execution_complete`) and authenticates with `X-EXECD-ACCESS-TOKEN`.

  | Tool | execd endpoint | Keeps state? |
  |---|---|---|
  | `shell(command)` | `POST /session` once per Athena session, then `POST /session/{id}/run` with `{command, timeout}` | Yes: persistent bash, so `cd`, env vars and venv activation carry over between calls |
  | `run_code(language, code)` | `POST /code/context {language}` once, then `POST /code {context, code}` | Yes: Jupyter kernel, so Python variables persist and results come back as MIME data |
  | internal only (browser tools, helpers) | `POST /command {argv: [...], timeout}` | No: one-shot, with native argv and **no shell expansion**, so no injection. **Caveat:** `argv` is in the spec at `release-1.1.0` (added 2026-09-08) but *not* in the newest execd image tag seen, `docker/execd/v1.1.0` (2026-08-26). The fallback is `command` with every argument single-quote escaped by a tested Rust helper. |

  - The file tools (`read_file`, `write_file`) use `/files/*` inside the sandbox.
  - There is **no PTY or WebSocket** API, so interactive programs (vim, top,
    password prompts) won't work. Tool descriptions tell the model to run
    non-interactive commands only (`-y`, `--no-input`). Every call sets a
    `timeout`.
  - Background jobs (`background: true`, then `/command/status/{id}` and
    `/command/{id}/logs`) are deferred to a later PR.
- **One sandbox per (user, session)**, created lazily and recorded in a new
  `sandboxes` table (migration plus schema fixture). Every create sets:
  - `metadata {app, env, user, session}`;
  - a finite `timeout`, renewed on use (never null, since null means it never
    expires);
  - `resourceLimits`;
  - a `networkPolicy`.

  The sandbox is deleted with its session.
- Output is truncated to a byte cap before it reaches the model. Athena never passes
  its own secrets in.
- Tests use a fake OpenSandbox built on axum, in the style of
  `tests/telegram/fake_api.rs`.

**Sandbox-host hardening: deferred** (owner decision, 2026-09-25: "the current
OpenSandbox is fine for now; we'll enhance it later").
- Athena sends `OPEN-SANDBOX-API-KEY` only when `OPEN_SANDBOX_API_KEY` is set, so it
  works with today's open server and with a keyed one later.
- **Accepted risk until then**, as shown by the probe:
  - any tailnet device can create sandboxes;
  - sandboxes run as root;
  - sandboxes can reach the Athena VM over the tailnet.
- The steps below are the later enhancement. Nothing in M1 or M2 waits on them.
- The safety eval cases that assert "sandbox can't reach the tailnet" are tagged
  `pending-hardening` and are reported, not gated, until the enhancement lands.

On the owner's host (later):
1. **Set `server.api_key`.** Today `GET /v1/sandboxes` returns 200 with no key.
2. ACL: only `tag:athena-server` → `tag:sandbox`.
3. **Egress: allow the internet, block the tailnet and private networks** with
   `DOCKER-USER` rules for:
   - IPv4: `100.64.0.0/10` (including MagicDNS `100.100.100.100`), `10/8`,
     `172.16/12`, `192.168/16`, `169.254/16` (cloud metadata);
   - IPv6: `fd7a:115c:a1e0::/48` (the tailnet), `fc00::/7`, `fe80::/10`.

   A probe sandbox checks this and is part of the safety eval suite.
4. gVisor (`runsc`) as the runtime, if supported.

Residual risk, documented: with internet egress, a prompt injection can exfiltrate
conversation content the model has seen. The mitigation is keeping secrets out of the
context. A future `networkPolicy` allowlist mode per agent is the real fix.

## 5. Browser: agent-browser inside the sandbox

Both options were compared. **agent-browser** (vercel-labs, Apache-2.0, Rust, v0.38.x)
wins over **browser-use** (MIT, Python):
- browser-use is itself an LLM agent loop with its own model key. Putting it inside
  Athena means two agent loops, double tokens, and behaviour that is hard to observe.
- agent-browser is a deterministic CLI: `open`, `snapshot -i` (accessibility tree
  with `@eN` refs), `click`, `fill`, `get text`, `screenshot`, `read <url>`. It needs
  no Python or Node at runtime.

Design:
- **It runs inside the per-session sandbox, not on the 4 GB VM.** The image is
  `deploy/sandbox-image/Dockerfile`: based on OpenSandbox's `examples/chrome`
  (debian-slim + chromium), plus a pinned `agent-browser` release and Chrome for
  Testing. No hosted registry is used: `scripts/build-sandbox-image.sh` builds it
  on the sandbox laptop (`docker build -t athena-sandbox:<version>`), and
  `ATHENA_SANDBOX_IMAGE` names that local tag. OpenSandbox pulls nothing for it.
  Unverified: whether OpenSandbox's Docker runtime uses a local-only image without
  trying to pull it; to be checked on the laptop.
- The browser gets the sandbox's isolation and egress policy for free. Budget roughly
  1–1.5 GB per browser sandbox on the sandbox host; this is unmeasured. Set a large
  `/dev/shm` or pass `--disable-dev-shm-usage`.
- **Typed tools**: `browser_open(url)`, `browser_snapshot()`, `browser_click(ref)`,
  `browser_fill(ref, text)`, `browser_read(url)`, `browser_screenshot()`. Each one
  runs `agent-browser --session <session-id> …` via execd.
  - Arguments are validated in Rust (URL scheme http/https only, ref matches
    `^@e\d+$`). Where the execd image supports it, they are sent as a `/command`
    `argv` array, which is never shell-expanded. Otherwise they go through the
    single-quote escaping fallback (see the sandbox tools table).
- Output is bounded by always passing `snapshot -i -c -d <depth>`, `--max-output`, and
  `--content-boundaries`, which wrap untrusted page text, plus Athena's byte cap.
- Screenshots are returned as sandbox file paths, never inline base64.
- `--confirm-actions eval,download` is on. `--allowed-domains` is available as an
  optional per-agent allowlist.
- Pin the agent-browser version: it moves fast, and the image has no official
  runtime build.

## 6. Hooks (rig already has them)

rig-agent 0.42 has the `AgentHook` trait (`rig-agent/src/agent/hook.rs:1206`). It
attaches with `AgentBuilder::add_hook`, or per prompt, stream or runner request.

| Hook | When | What it can do |
|---|---|---|
| `on_completion_call` | before the model | patch the request, or stop |
| `on_completion_response` / `on_stream_response_finish` | after the model | observe |
| `on_tool_call` | before a tool | `Run`, `Rewrite`, `Skip(msg)`, `Stop` |
| `on_tool_result` | after a tool | keep, rewrite, stop |
| `on_model_turn_finished` | end of a model turn | continue, retry, stop |
| `on_invalid_tool_call` | bad tool call | repair, retry, skip |
| delta hooks | streaming | observe deltas |

There is no `on_error`.

Athena uses one hook in v1: **`ToolPolicy`** (`on_tool_call`). It enforces a per-run
tool-call budget and argument size limits, and returns `Skip` with a reason. Output
truncation stays in the tool code. Tracing uses rig's spans, not hooks.

## 7. Observability (OTel standard practice)

**rig already emits GenAI-semconv `tracing` spans:** `invoke_agent`, `chat`, and
`execute_tool`, carrying `gen_ai.*` attributes. The sources are
`rig-core/src/telemetry/mod.rs:441` and `rig-agent/src/agent/runner.rs:504,791`.
Content is recorded only with `record_content_telemetry(true)`. The semantic
conventions are still at "Development" status.

Athena adds:
- **Crates** (pinned together): `tracing`, `tracing-subscriber` (json, env-filter),
  `opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp` 0.33,
  `tracing-opentelemetry` 0.34, `opentelemetry-appender-tracing` 0.33, `tower-http`.
  If the versions mismatch, export silently does nothing.
- **`src/telemetry.rs`.** It builds layers from an **injected exporter or config**, so
  tests can use an in-memory exporter; the global subscriber can be installed only
  once per process. It is enabled only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set,
  and that branch gets its own test.
  - Resource attributes: `service.name`, `service.version` (from `ATHENA_VERSION`,
    written into `/etc/athena/<env>.env` by `deploy-gate`, not `option_env!`, which would leave a branch
    uncovered) and `deployment.environment.name`.
  - Flush happens on shutdown, after drain.
- **An Athena-owned `invoke_agent` span** per turn, which rig adopts. It carries:
  - `gen_ai.conversation.id=<session>`;
  - `athena.run_id`, the join key to SQLite `runs`;
  - `athena.transport`;
  - a hashed user id.
- Child spans `sandbox.exec` and `browser.action`.
- W3C `traceparent` in and out on HTTP.
- **Content capture.** Off in prod. Staging turns it on only while the staging bot has
  no real users.
  - The collector strips `gen_ai.input/output.messages` and tool arguments/results
    as a backstop.
  - The `redaction` processor also covers the **logs** pipeline.
  - Log statements never include prompt text.
- `RUNS_STORE_RAW=0` in prod.

**The stack, phased for the 4 GB VM.** Each component is a systemd service installed
from its official `.deb` or release binary, with `MemoryMax=`. There is no Docker.

| Phase | Components | Why |
|---|---|---|
| M2 | otelcol-contrib (Apache-2.0, `MemoryMax=256M`) → rotating JSONL files → DuckDB | Traces and logs are captured and queryable with almost no RAM. |
| M3 | **OpenObserve** (AGPL-3.0, Rust, single binary): traces, logs and metrics, with a UI and dashboards, and native OTLP ingest. It replaces Tempo + Prometheus + Grafana. | Live monitoring from **one** process instead of three, **on the VM**, with `MemoryMax=512M` to start. The collector exports OTLP to it and keeps writing JSONL for DuckDB. Its footprint at Athena's volume is unmeasured; PR 19 measures it with `systemd-cgtop` before relying on it. If it doesn't fit, the fallback is running it on another tailnet host (the collector's `file_storage` queue buffers while that host sleeps). It listens only on the tailnet IP, and the admin password is set at setup. |

**Why M3 had moved off the VM, and why it can come back.**
- The earlier design (Tempo + Prometheus + Grafana) was estimated at about 1.1 GB.
  The owner's VM has about 1.9 GB free next to 7 Docker apps, which is too tight.
- Lighter options, all open source and checked on GitHub 2026-09-25:

| Option | License | What you get | Fit |
|---|---|---|---|
| DuckDB only (M2 as is) | MIT | SQL over traces and logs | **lightest**: no daemon at all, but no UI |
| **OpenObserve** | AGPL-3.0, Rust | one binary for traces, logs and metrics, with a UI | **recommended for M3**: one process instead of three |
| Jaeger v2 (Badger storage) | Apache-2.0 | trace UI only | light, but no metrics or logs |
| VictoriaTraces + VictoriaLogs (+ VictoriaMetrics) | Apache-2.0 | very efficient stores with built-in UIs | light, but VictoriaTraces is young (~480 stars) and it is three services |
| Tempo + Prometheus + Grafana | AGPL / Apache | the fullest stack | too heavy for this VM |

Excluded:
- Arize Phoenix: ELv2, not OSI open source.
- Langfuse and SigNoz: ClickHouse is too heavy here.
- `grafana/otel-lgtm`: labelled dev/demo only.
- Loki: optional profile for a bigger VM.

**Alerts: none for now** (owner decision). Nothing posts to Telegram, and no extra bot
is created. Monitoring means looking at traces and logs and running analytics
queries. Alerts can be added later on top of the same data.

## 8. Analytics (DuckDB)

- **Trace and log data.** The collector JSONL is read directly with DuckDB's `otlp`
  community extension (MIT). It caps input at 100 MB per file, so files rotate at
  50 MB.
- **Athena's own data.** `ATTACH '/var/lib/athena/prod/agent.db' (TYPE sqlite, READ_ONLY)`.
  No ETL is needed.
- **Eval results.** Read directly with `read_json_auto('results/*.jsonl')`.
- **Canned queries** in `analytics/queries/*.sql`:
  - cost per model, session and day (joined with `prices.csv`);
  - p95 latency and error rate per tool;
  - sandbox and browser failures;
  - eval pass rate per tag across git SHAs.
- **Running them.** `scripts/analytics.sh <query>` runs the DuckDB CLI on the VM over
  Tailscale SSH.
- **Deferred:** Parquet compaction and the Grafana DuckDB plugin (which is unsigned).

## 9. Evaluation methodology and gate

**Approach.** A native `athena eval` subcommand, with no Python or Node. rig 0.42 has
no eval module (checked).

**Dataset.** `evals/cases/*.yaml`, with field names close to agents-cli's `EvalCase`:
- `eval_case_id`
- `kind`: single, multi, trajectory, safety, or regression
- `tags`
- `turns`
- `expect.{trajectory, output, rubric}`
- `thresholds`
- `cassette`

**Graders (v1, deterministic).**
- Tool trajectory, rebuilt from the run's `messages` rows, with subset, ordered, or
  exact matching and `forbid` rules on tool plus arguments.
- Argument regex or JSON schema.
- Output contains or regex.
- Turn and token budgets.
- `finish_reason != length`.

**Safety suite (p0).**
- Sandbox tries to reach the tailnet, private networks, or the metadata service.
- Attempts to read secrets.
- Prompt injection through tool output and through web page content.

**Where the gate sits.**

| Stage | What runs | Model | Effect |
|---|---|---|---|
| PR | all cases replayed through `ReplayModel`, a rig `CompletionModel` that serves committed cassettes | none | **Blocks.** A stale cassette fails loudly as "trajectory drift". |
| Staging, M2 | live suite, k=3, deterministic graders | cheap real model | **Report** attached to the run and read by the approver. The safety suite's pass^k is highlighted. |
| Later | an LLM judge (a different model family, N=3, majority vote) that gates only after calibration against about 50 hand labels, plus `athena eval compare` against the last prod baseline | cheap real model | Becomes a blocking gate once it is stable. |

- Results are written as JSONL rows:
  `eval_run_id, git_sha, env, case_id, sample, pass, scores, run_id, trace_id, tokens, latency_ms`.
- `AGENT_SPEC.md` holds tools, constraints and success criteria. Each success
  criterion maps to at least one tagged case.

## 10. HTTP auth and "Telegram as a plugin"

**The framing.** Athena already keys users as `(transport, external_id)`, and Telegram
is just one transport. Generalise that to **`(client, external_id)`**.

| Stage | When | How |
|---|---|---|
| 0, ship now | only you, CI and your own apps | The tailnet ACL is the gate: only `tag:athena-client` and admins reach `:18080`. `X-Athena-User` is still trusted. |
| 1 | a second app appears on the tailnet | Each app gets an API key, hashed in a `clients` table. `Caller` maps the key to a client, and the user becomes `(client, X-Athena-User)`. An app can impersonate its own users, never another app's. |
| 2 | people call the API directly | `tailscale serve` strips spoofed `Tailscale-User-*` headers and injects `Tailscale-User-Login`. Tagged devices get no login header, so they keep using stage 1 keys. |

- Only `Caller` (`src/http.rs:176`) changes between stages.
- Telegram and HTTP can become Cargo features later, so a fork can drop a transport.
  Both of these are recorded as a design note, not built in v1.

## 10b. Public ingress for a future frontend: Tailscale and Caddy, each with its own job

They solve different problems, so the template uses both.

| | Tailscale | Caddy (Apache-2.0) |
|---|---|---|
| Job | **private plane**: who inside your network can reach what | **public plane**: your domain on the internet, automatic HTTPS (Let's Encrypt) |
| Carries | CI deploys, admin SSH, Athena API ↔ your own apps, Athena → sandbox, observability | browser traffic from anyone on the internet |
| Identity | device/user identity in the tailnet | none by itself; the app behind it must authenticate |

**Tailscale Funnel** (public exposure through Tailscale) can't serve your own domain.
Tailscale's docs say: "Funnel can only use DNS names in your tailnet's domain
(`tailnet-name.ts.net`)".
- It listens only on ports 443, 8443 and 10000.
- Traffic has "non-configurable bandwidth limits".
- A Cloudflare CNAME to a Funnel host fails the TLS handshake (tailscale/tailscale#16478).
- Custom-domain support is an open feature request (tailscale/tailscale#11563).

Source: https://tailscale.com/kb/1223/funnel
The owner already runs **Caddy** on the VM (`/usr/bin/caddy`, listening publicly on
`:80`/`:443`), so that is the public ingress.

**The rule: the browser never talks to Athena directly.**

```
browser ──https://app.<your-domain>──► Caddy (VM, public :443)
                                         └─► frontend + its backend (BFF)
                                               │  authenticates end users (sessions/OAuth)
                                               └─► Athena API over the tailnet (or loopback)
                                                    Authorization: Bearer <client key>   (auth stage 1)
                                                    X-Athena-User: <that app's user id>
```

- Athena stays on its tailnet IP and is never published. Caddy proxies only to the
  frontend and its backend-for-frontend (BFF).
- The BFF is a stage-1 client (section 10). It holds one API key and can only act
  as its own users, so a browser can never choose `X-Athena-User`. That header is
  set server-side.
- Streaming: the BFF relays Athena's SSE to the browser. Caddy's `reverse_proxy`
  flushes `text/event-stream` responses immediately, so no buffering config is
  needed (from Caddy's docs; to be verified with the real frontend).

**If a public Athena API is ever required** (for example for third-party
integrations), it goes behind Caddy only after all of these are in place:
- auth stage 1 (API keys);
- rate limiting;
- CORS;
- request size limits.

The template ships `deploy/caddy/athena.caddy` as an **example snippet** to
`import`. `setup-host.sh` never edits an existing Caddyfile, because the owner's
Caddy already serves other sites.

**Ordering:** the frontend is future work. It needs auth stage 1 (the `clients`
table and API keys) first. Nothing in M1 or M2 depends on it.

## 11. Setup UX: "give this prompt to your coding agent"

Developer-friendliness is the default. Setup has two front doors onto **the same
scripts**: a coding agent, or a person following the same runbook. The agent never
improvises infrastructure. It runs the repo's idempotent scripts in order, reads their
JSON output, and stops at human checkpoints. This borrows agents-cli's idea of shipping
skills for coding agents, applied to Athena's own setup.

**What's in the repo**

| File | Purpose |
|---|---|
| `SETUP.md` | The runbook, written for an agent to follow. Each step lists: a command, how to tell it succeeded, what to do on failure, and a ⏸ checkpoint where the human must act or approve. |
| `.claude/skills/athena-setup/SKILL.md` + `AGENTS.md` | Point Claude Code, Codex, Gemini CLI and others at `SETUP.md` (agents-cli-style skills). |
| `scripts/preflight.sh` | Read-only. Prints a JSON report for the local machine, the VM (over SSH), the tailnet and GitHub. It is always step 1, and safe to run anywhere. |
| `scripts/setup-host.sh` | Idempotent and brownfield-safe (section 3). Accepts `--dry-run`, which prints every change without making it. |
| `scripts/init-github.sh` | Environments, `v*` tag ruleset, secrets, variables. Accepts `--dry-run`. |
| `scripts/doctor.sh` | Verifies the finished setup end to end. JSON plus a human summary. |

**The prompt in the README**

> Set up Athena for me. My VM is reachable over Tailscale at `<ip-or-name>` as
> `<user>`. Follow `SETUP.md` exactly: run preflight first, show me the plan and wait
> for my OK before changing anything, and never ask me to paste secrets into this chat.

**How the agent works through `SETUP.md`**
1. **Preflight (read-only).** It runs `preflight.sh` and summarises the result. For
   the owner's VM, that would read: Ubuntu 24.04 x86_64, ~1.9 GB free, Docker with 7
   apps, Tailscale present and user-owned, ports 18080/18081 free. It proposes
   options, for example "use a hosts alias, not a tag" and "obs backend on another
   host".
2. ⏸ **Plan approval.** It shows `setup-host.sh --dry-run` and
   `init-github.sh --dry-run`, then waits for an explicit "yes".
3. ⏸ **Human-only steps, never delegated:**
   - Tailscale admin console: paste the generated policy, and create the CI OIDC
     credential;
   - the existing Telegram bot token (prod only);
   - OpenRouter keys: the human types them into
     `ssh -t <vm> sudoedit /etc/athena/<env>.env` themselves.

   **Secrets never pass through the agent's context.** The scripts only report
   "present / missing / wrong mode".
4. **Apply.** It runs `setup-host.sh`, then `init-github.sh`. Both are idempotent, so
   a failed step is safe to re-run.
5. **First deploy.** Merging to `main` (or `gh workflow run`) deploys staging. It then
   runs `doctor.sh`.
6. ⏸ **First release.** The human pushes the `v0.1.0` tag and approves prod in
   GitHub.

**The human path** is the same list, run by hand:

```bash
./scripts/preflight.sh --vm <user>@<vm>        # read-only report
./scripts/setup-host.sh --vm <user>@<vm> --dry-run   # review, then run without --dry-run
ssh -t <vm> 'sudoedit /etc/athena/staging.env /etc/athena/prod.env'
./scripts/init-github.sh --dry-run             # review, then run for real
./scripts/doctor.sh
git tag -a v0.1.0 -m "first release" && git push origin v0.1.0   # after the first green staging run
```

**What still can't be automated:**
- Tailscale's admin console (policy and OIDC credential). `preflight.sh` generates
  the exact policy JSON to paste.
- Pasting the existing Telegram bot token.
- OpenRouter keys.
- The sandbox host.

These are the checkpoints.

**Why scripts plus a runbook, not "let the agent figure it out":**
- The scripts are testable. `deploy-gate` is Rust with full coverage. For the bash scripts, CI runs `shellcheck`, plus `--dry-run` against a
  container with a fake systemd and Tailscale.
- The same path works with no agent at all.
- Every change is visible in a dry run before it happens. This matters on a shared
  VM running other apps.

`doctor.sh` checks:
- tailnet reachability;
- that the ACLs hold (CI can reach only `:22`; the sandbox cannot reach the VM);
- env files present with mode 0600;
- sandbox auth (reported as a warning while hardening is deferred);
- that staging and prod `/version` match the expected SHAs;
- the prod environment has a reviewer;
- the `v*` tag ruleset exists;
- free disk above 1 GB.

## 12. Implementation order (one feature per PR)

| # | PR | Needs |
|---|---|---|
| **M1: deployable** | | |
| 1 | `athena backup` (raw read-only online backup) + require an absolute `ATHENA_DB` for serve/telegram | – |
| 2 | `athena --version` + `/version` endpoint, from runtime `ATHENA_VERSION` | – |
| 3 | `ATHENA_ALLOWED_HOSTS` extending the loopback Host rule | – |
| 5 | PR pipeline: integration job (real release binary, temp DB, fake-telegram; no model calls) | 1, 2 |
| 6 | systemd units + `athena@.target` + hardening, verified with `systemd-analyze security` | 1–3 |
| 7a | `preflight.sh` (read-only JSON report, brownfield detection, generated Tailscale policy) | – |
| 7b | `deploy-gate` Rust binary: command whitelist, pull by digest, backup, switch, health check, rollback, pruning; fakes behind a trait; 100% covered | 1, 2 |
| 7c | `setup-host.sh` (brownfield-safe, `--dry-run`, `--uninstall`; installs a pinned `deploy-gate` from GHCR) | 6, 7a, 7b |
| 8 | `ci-cd.yml`: merge → build → staging → loadcheck → smoke; tag → require-stage-success → prod (approval) → smoke; + `init-github.sh --dry-run` | 5, 7c |
| 8b | `SETUP.md` agent runbook + `.claude/skills/athena-setup` + `AGENTS.md` + `doctor.sh` + README prompt | 7b, 8 |
| **M2: capable, observable, gated** | | |
| 9 | Sandbox client + `shell` / `read_file` / `write_file` tools + `sandboxes` table; remove host `read_file` (works with today's unauthenticated server; API key optional) | – |
| 10 | `run_code` tool (execd `/code` contexts) | 9 |
| 11 | Sandbox image with agent-browser, built on the laptop by a script | 9 |
| 12 | Browser tools | 11 |
| 13 | `ToolPolicy` hook | 9 |
| 14 | Telemetry: tracing + OTLP + `invoke_agent` span + traceparent | – |
| 15 | otelcol-contrib unit + config: collector → JSONL, with redaction/strip processors | 14 |
| 16 | `athena eval` v1: dataset, deterministic graders, `ReplayModel`, PR gate | – |
| 17 | Live eval target + staging eval report via `deploy-gate eval` | 8, 16 |
| 18 | DuckDB analytics scripts + canned queries | 15, 16 |
| **M3: polish** | | |
| 19 | OpenObserve (single binary, `MemoryMax`) + saved dashboards, after measuring memory (no alerts) | 15 |
| 20 | LLM judge + calibration + `eval compare` → blocking gate | 17 |
| 21 | `init.sh` (rename for forks) + a CI job that exercises `SETUP.md`'s scripts in `--dry-run` against a fake host | all |

## 13. Owner decisions

| # | Question | Answer | Effect on the plan |
|---|---|---|---|
| 1 | Repo visibility | **Public** | Environment required reviewers are free. Prod deploys wait for approval on a `v*` tag. Public-repo hardening in section 2. |
| 2 | Registry | **GHCR, via ORAS** (revised from "not hosted") | Releases are pushed as OCI artifacts. The VM pulls by digest and caches 3. The public package makes installs easy for anyone. The sandbox image is still built locally on the laptop. |
| 3 | VM architecture | **x86_64** | Build `x86_64-unknown-linux-gnu` on `ubuntu-22.04` runners. |
| 4 | Dependencies | **Approved** once the plan is approved | Add the obvious ones: `reqwest`, the tracing/opentelemetry crates, `tower-http`, a YAML parser. |
| 5 | Sandbox host | **Owner's laptop**, 24 GB, on the tailnet | See the laptop notes below. |

**The owner's VM** (`appuser@100.124.202.79`, inspected read-only on 2026-09-25):

| Item | Found |
|---|---|
| OS | Ubuntu 24.04.4 LTS, x86_64 |
| CPU and memory | 2 vCPU; 3.8 GB RAM, about 1.9 GB available |
| Disk | 12 GB free of 38 GB |
| Docker | 7 app containers, published on ports 8000, 8080 (on the tailnet IP), 8081, 8082, 8180 and 8181 |
| Tailscale | installed, user-owned node, no `serve` config |
| Other | `/opt/ajenti` present |

Consequences:
- Setup takes the brownfield path: hosts alias, no firewall changes, no Docker
  changes.
- The M3 observability backend goes on another host.
- **Observation, outside Athena's scope:** `compare-mf-web` (8081) and
  `latex-resume-ai-backend` (8000) are published on `0.0.0.0`. Docker-published
  ports bypass ufw, so they are publicly reachable unless Hetzner's cloud firewall
  blocks them.
- **Question:** is `blacki-agent-1` (tailnet IP, port 8080) the predecessor that
  Athena replaces?

Decided since v3:
- No fake-model load tests. `loadcheck` on staging uses the real cheap model.
- `blacki-agent-1` stays until Athena is good enough to replace it.
- No alerts, and no additional Telegram bots. Staging is HTTP only.
- Sandbox hardening is deferred. Today's open OpenSandbox is used as is.

**Laptop as the sandbox host**
- Later (deferred): tag it `tag:sandbox`, which strips your user identity from the
  device, and apply the section 4 hardening:
  - API key;
  - egress block for private and tailnet ranges, IPv4 and IPv6;
  - gVisor.
- **Don't run it under your personal account** if the laptop holds personal files.
  A sandbox escape reaches whatever the Docker host can reach. Use a dedicated OS
  user, or better, a clean OS install.
- Keep it awake with the lid closed: `HandleLidSwitch=ignore` in logind, and disable
  suspend. Otherwise Athena's tools fail whenever the laptop sleeps. Sandbox
  failures surface as tool errors in traces.
- Capacity: with about 1–1.5 GB per browser sandbox (unmeasured), 24 GB gives
  roughly 10–12 concurrent browser sessions, and many more shell-only sessions. Set
  OpenSandbox resource limits so one session can't take the whole host.

## Sandbox probe results (2026-09-25)

Method: one throwaway sandbox (`debian:13-slim`, 0.5 CPU, 256 MiB, 120 s timeout),
deleted afterwards (HTTP 204).

| Check | Result |
|---|---|
| Create without an API key | worked. **The server is open to the whole tailnet.** |
| execd via `GET …/endpoints/44772?use_server_proxy=true` | returns `100.118.54.67:9090/v1/sandboxes/{id}/proxy/44772` with **no extra headers**, so Athena needs only port 9090 and no execd token |
| Output stream format | JSON objects separated by blank lines (`init`, `ping`, `stdout`, `stderr`, `execution_complete`), **not** `data:`-prefixed SSE. The Rust parser must handle this. |
| `argv` on `/command` | **rejected** (`'Command' … required`), so this execd image predates argv. Use the escaping fallback, or upgrade the execd image in the server config. |
| Persistent bash (`/session`) | works: `cd` and `export` persist across calls |
| User inside the sandbox | root (`uid=0`), so gVisor is strongly advised |
| Sandbox → Athena VM `100.124.202.79:8080` | **reachable.** The private/tailnet egress block (section 4) is mandatory. |
| Sandbox → sandbox host `100.118.54.67:9090` | blocked or closed |
| Sandbox → internet `1.1.1.1:443` | reachable (egress allowed, as configured) |

## Unverified (checked during implementation)

- Memory use of the services on the 4 GB VM, and Chromium memory per sandbox.
- Whether OpenSandbox uses a locally built image without pulling it.
- The DuckDB `otlp` extension's column names.
- The collector OTTL syntax.

## Sources

- google/agents-cli: `docs/src/guide/{cicd,evaluation,lifecycle}.md`,
  `skills/google-agents-cli-eval/references/*`
- OpenSandbox: https://github.com/opensandbox-group/OpenSandbox (`specs/`,
  `server/opensandbox_server/middleware/auth.py`, `examples/chrome`,
  `docs/examples/chrome.md`)
- agent-browser: https://github.com/vercel-labs/agent-browser
- browser-use: https://github.com/browser-use/browser-use
- microsandbox: https://github.com/superradcompany/microsandbox
- rig 0.42 source: `rig-agent-0.42.0/src/agent/{hook.rs,runner.rs,builder.rs}`,
  `src/tool/rmcp.rs`, `rig-core-0.42.0/src/telemetry/mod.rs`
- OTel GenAI semconv: https://github.com/open-telemetry/semantic-conventions-genai
- Collector file exporter: https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/exporter/fileexporter/README.md
- DuckDB otlp extension: https://duckdb.org/community_extensions/extensions/otlp
- GitHub environments and plans: https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments
- GitHub secure use: https://docs.github.com/en/actions/reference/security/secure-use
- Tailscale: https://github.com/tailscale/github-action,
  https://tailscale.com/kb/1312/serve, https://tailscale.com/kb/1337/policy-syntax
- agent-foundation (release-tag promotion): https://github.com/doughayden/agent-foundation (`.github/workflows/ci-cd.yml`, `require-stage-success.yml`, `docs/infrastructure.md`)
- systemd sandboxing: https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html
- ORAS: https://github.com/oras-project/oras
- Caddy reverse_proxy: https://caddyserver.com/docs/caddyfile/directives/reverse_proxy
