---
name: athena-setup
description: Set up, deploy, verify or tear down Athena's production environment (a Tailscale VM running staging and prod under systemd, deployed by GitHub Actions). Use when asked to "set up Athena", "deploy Athena to my VM", "configure CI/CD for Athena", "check my Athena setup", or to run preflight, setup-host, init-github or doctor.
---

# Athena production setup

The runbook is `SETUP.md` at the repository root. Read it in full and follow
it step by step; this skill only restates the rules that must not be broken.

1. Always start with the read-only preflight:
   `./scripts/preflight.sh --vm USER@HOST | jq .` Summarise it for the user.
2. Before any change, show the dry runs of `scripts/setup-host.sh` and
   `scripts/init-github.sh` and wait for an explicit "yes".
3. Stop at every ⏸ checkpoint in `SETUP.md` (Tailscale admin console, OAuth
   client, VM secrets, making GHCR packages public, pushing a release tag,
   approving prod). Tell the user exactly what to do and wait.
4. Secrets never pass through you. Do not ask for them, read them, echo them
   or put them in files. The user types them into `sudoedit` on the VM or
   `gh secret set` in their own terminal.
5. Never modify Docker, ufw, sshd, Caddy or Tailscale configuration on the VM.
   The scripts are designed not to; do not work around them with ad-hoc
   commands. If a script fails, report its output and the fix it prints.
6. Finish with `./scripts/doctor.sh --vm USER@HOST` and report every
   non-passing check with its suggested fix.
