# Notes for coding agents

This file is read by Codex, Gemini CLI, Claude Code and other agents.

## Setting up production

If you are asked to deploy or set up Athena on a VM, follow
[`SETUP.md`](SETUP.md) exactly:

- run `scripts/preflight.sh` first, and summarise its JSON;
- show the `--dry-run` output of `scripts/setup-host.sh` and
  `scripts/init-github.sh`, and wait for an explicit "yes" before changing
  anything;
- stop at every ⏸ checkpoint and let the human act;
- never ask for, read, or print a secret. Humans type secrets into `sudoedit`
  and `gh secret set` themselves;
- never change Docker, ufw, sshd, Caddy or Tailscale settings on the VM.

## Changing code

- Edit `src/agent.rs` to change the agent (README, "Make a new agent").
- Before a PR: `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
  `./scripts/coverage.sh` (100% line coverage, no exclusions). See `TESTING.md`.
- Shell scripts must pass `shellcheck scripts/*.sh`; workflows must pass `actionlint`.
- Names, paths, ports and protocols shared between components are in
  `plans/contracts.md`. Change that file in the same PR as the code.
