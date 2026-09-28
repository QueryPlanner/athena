# Many agents on one VM, and `athena-cli` to make them

Status: in progress (one PR, by the owner's choice).

## Goal

Keep today's setup (one VM, staging and prod per agent, CI deploys through
`deploy-gate`) and make it host any number of agents. A new agent is one
command on the laptop, `athena-cli new <name>`, which writes a small repo that
depends on the shared runtime. The owner still edits tools, instructions and
policy per agent.

## 1. Code: a Cargo workspace

| Package | Path | What |
|---|---|---|
| `athena-core` (lib) | `crates/athena-core` | everything that is the same for every agent: runner, service, store, HTTP, CLI REPL, telegram, eval, bench, ops, telemetry, sandbox client and tools, tool policy, the gate library, and `run(spec)` (today's `main.rs`) |
| `athena` (lib + bins `athena`, `deploy-gate`) | repo root | the Athena agent: `src/agent.rs` (its `AgentSpec`) and a two-line `main.rs`. Integration tests stay in `tests/`, because they need `CARGO_BIN_EXE_athena` |
| `athena-cli` (bin) | `crates/athena-cli` | the scaffolder and VM helper; templates and scripts are embedded with `include_str!` |

### `AgentSpec`

```rust
pub struct AgentSpec {
    pub name: &'static str,          // [a-z][a-z0-9]{0,23}; gen_ai.agent.name, OTEL service name
    pub version: &'static str,       // the agent crate's env!("CARGO_PKG_VERSION")
    pub preamble: &'static str,
    pub default_model: &'static str, // AGENT_MODEL still overrides
    pub max_turns: usize,
    pub policy: ToolPolicy,
    pub sandbox_tools: SandboxTools, // All | None | Only(&'static [&'static str])
    pub tools: fn(AgentBuilder<WithBuilderTools>) -> AgentBuilder<WithBuilderTools>,
}
```

The core builds `name/preamble/record_content_telemetry/hook/max_turns`, turns
the builder into `WithBuilderTools` with `dynamic_tools(vec![])`, applies
`spec.tools`, then the selected sandbox tools. Every place that called
`crate::agent::*` takes `&AgentSpec` instead (service span name, telegram,
eval target, main). Core unit tests use `athena_core::testing::SPEC`.

Provider stays OpenRouter in the core for now.

## 2. VM layout per agent

`<a>` is the agent name, `<env>` is `staging` or `prod`.

**Athena keeps today's layout** (paths, units `athena-serve@<env>`, user
`athena`, keys without `--agent`). Nothing on the live VM moves: the new gate
treats a missing `--agent` as `athena` on the old layout, so it can be
installed over the old one at any time and CI keeps working. A new agent can
not be called `athena`.

Every other agent:

| Path | Contents |
|---|---|
| `/opt/athena/bin/deploy-gate` | shared, one install (from Athena's release artifact) |
| `/opt/athena/agents/<a>/releases/<hex>/athena` | release cache per agent |
| `/opt/athena/agents/<a>/<env>/current` | symlink |
| `/etc/athena/gate.env` | `ORAS`, `ATHENA_KEEP_RELEASES`, `ATHENA_REPO` (Athena's, also the gate's own) |
| `/etc/athena/agents/<a>/agent.env` | root 0644: `ATHENA_REPO`, `PORT_STAGING`, `PORT_PROD`. Its existence is what makes `<a>` a known agent |
| `/etc/athena/agents/<a>/<env>.env` | root:athena-<a> 0640 |
| `/var/lib/athena/agents/<a>/<env>/{agent.db,backups,telemetry}` | athena-<a>'s |
| `/var/lib/athena/gate/<a>/<env>.state.json`, `/var/lib/athena/gate/<a>/.lock` | root |

- **User** `athena-<a>` per agent: its services and every binary the gate runs
  for it run as that user, so one agent's release cannot write another's data.
- **Units** are rendered per agent from `deploy/systemd/agent-*.in`:
  `athena-<a>-serve@.service`, `athena-<a>-telegram@.service`,
  `athena-<a>@.target`; the instance is the env, as for Athena.
- Every release artifact holds a binary named `athena` (the runtime).

## 3. deploy-gate

- `authorized_keys`: one line per agent per env,
  `restrict,command="sudo /opt/athena/bin/deploy-gate --agent <a> --key-env <env>" … ci-<a>-<env>`.
  No `--agent` means `athena`. Any other agent must have an `agent.env`. So a
  key reaches one agent and one env. CI's commands do not change.
- One lock per agent; `install-gate` takes every lock's parent, the old global
  `/var/lib/athena/.gate.lock`, which Athena's deploys also take.
- `/health` returns the agent name; when present, the gate checks it matches.
- Admin commands: `restore [--agent <a>] <env> <file>`, `install-gate
  <digest>`, `list` (every agent with its ports and state).
- The commands the gate runs in a release (`--version`, `backup`, `bench`,
  `eval run`, `/health`, `/version`, `current/evals`) are a versioned runtime
  contract in `plans/contracts.md`, because agents pin different core
  revisions.

## 4. Scripts

- `setup-host.sh --agent <a>` (default `athena`, which does exactly what it
  does today): host steps as today, then that agent's user, dirs,
  `agent.env`, env files (with `OTEL_SERVICE_NAME=<a>`), rendered units, its
  key lines (other agents' lines are kept; a key already used by another line
  is refused), and enabling its units. Without `--port-*`, a new agent gets the
  next free pair from 18082 up, skipping every registered agent's ports,
  Athena's, and anything listening.
- `doctor.sh`, `preflight.sh`, `analytics.sh`: take `--agent`.
- `init-github.sh`: already takes `--repo`; unchanged.

## 5. `athena-cli`

| Command | Does |
|---|---|
| `athena-cli new <name> [--dir D] [--owner O] [--core-rev R]` | writes a repo: `Cargo.toml` (git dep on `athena-core`), `src/main.rs`, `src/agent.rs`, `prompts/system.md`, `evals/cases/`, `.github/workflows/ci-cd.yml`, `AGENTS.md`, `.agents/skills/athena-agent/SKILL.md`, `scripts/coverage.sh`, `.gitignore`; runs `git init` |
| `athena-cli vm add <name> --vm USER@HOST [--dry-run] …` | runs the embedded `setup-host.sh --agent <name> --vm …` |
| `athena-cli vm list --vm USER@HOST` | `sudo deploy-gate list` over ssh |
| `athena-cli vm doctor <name> --vm USER@HOST` | embedded `doctor.sh --agent` |
| `athena-cli github init <name> …` | embedded `init-github.sh` |

All run on the laptop. The VM only ever pulls artifacts that CI built.

## 6. Rollout on the existing VM

1. Merge. CI deploys Athena's staging as today.
2. ⏸ Human: `setup-host.sh --gate-digest <new>` dry run, then real: installs
   the new gate. Athena is untouched otherwise.
3. `athena-cli new scratch`, push, ⏸ make its GHCR package public,
   `athena-cli github init`, `athena-cli vm add scratch` (dry run, ⏸, real),
   merge a change in `scratch`, watch it reach staging next to Athena.

## Known limits

- Athena keeps the legacy layout; moving it into `agents/athena` is a later,
  separate step.
- All agents share one OpenSandbox server and API key.
- Tailscale ACLs for new ports are a human edit (setup never touches Tailscale).
