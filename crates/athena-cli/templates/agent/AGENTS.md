# Notes for coding agents

This is the **@NAME@** agent, made with `athena-cli new`. It runs on the
Athena runtime (`athena-core`), so it is small on purpose.

## Where things are

| To change | Edit |
|---|---|
| What the agent is told | `prompts/system.md` |
| Its own tools | `src/agent.rs`, a `#[rig_tool]` function added in `spec().tools` |
| Which runtime tools it gets (shell, code, files, browser) | `sandbox_tools` in `src/agent.rs` |
| Model, turn limit, tool-call limits | `default_model`, `max_turns`, `policy` in `src/agent.rs` |
| Everything else (HTTP API, store, telemetry, evals, deploys) | nothing here: it is `athena-core` |

The skill in `.agents/skills/athena-agent/` has worked examples.

## Rules

- Keep `NAME` as `@NAME@`. It names the agent's paths, units and user on the
  VM; changing it makes a different agent.
- Every tool gets a test in `src/agent.rs` or `tests/`. Tests use a scripted
  model (`rig_core::test_utils::MockCompletionModel`), never a real one.
- Before a PR: `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --locked`.
- Secrets live in `.env` (local) and `/etc/athena/agents/@NAME@/<env>.env`
  (VM). Never ask for, read, or print one.

## Deploying

Merge to `main`: CI builds the release and deploys staging. Push a `v*` tag
on a commit that passed staging: CI promotes it to prod. The VM is set up
once with `athena-cli vm add @NAME@`; see `README.md`.
