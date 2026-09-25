# Setting up Athena in production

A runbook for a coding agent or a person. It takes one Linux VM on your
Tailscale tailnet and this GitHub repository to: staging and prod on the VM,
deployed by GitHub Actions, with traces, logs and analytics.

The design is in `plans/production-template.md`; names, paths and ports are
in `plans/contracts.md`.

## Rules for an agent following this file

- Run the steps in order. Each has a command, how to tell it worked, and what
  to do when it did not.
- **⏸ means stop.** Show the human what the step needs and wait for them to
  say it is done. Never do a ⏸ step for them.
- **Never ask for a secret in the chat, and never print one.** API keys, bot
  tokens and passwords are typed by the human into `sudoedit` or
  `gh secret set`. The scripts only report "present / missing / wrong mode".
- Never change anything on the VM or in GitHub before the human approves the
  dry runs in step 3.
- Do not improvise infrastructure. If a script fails, report its output and
  the fix it suggests; do not work around it with ad-hoc commands.
- Every script is idempotent: re-running one after a fix is safe.

## What you need

| What | Check |
|---|---|
| A Linux x86_64 VM with systemd (Ubuntu 22.04+), on your tailnet, with a user that has sudo | `./scripts/preflight.sh --vm USER@HOST` |
| This machine on the same tailnet, able to `ssh USER@HOST` | `ssh USER@HOST true` |
| `gh` logged in as an admin of the repository | `gh auth status` |
| `jq`, `ssh-keygen`, `curl` locally | `command -v jq ssh-keygen curl` |

Below, `VM=appuser@100.124.202.79` stands for your VM's login and tailnet IP.

## 1. Preflight (read-only)

```bash
./scripts/preflight.sh --vm "$VM" | jq .
```

Works when: it prints JSON with `"ok": true`. Summarise for the human: OS and
architecture, free RAM and disk, Tailscale IP and whether the node is tagged,
the five ports, Docker containers published on all interfaces (reported only),
Caddy, and any existing Athena install.

If `conflicts` is not empty, stop and show them. The usual fixes:
- a port in use: pick others and pass `--port-prod N --port-staging N` to
  `setup-host.sh` (and adjust `deploy/tailscale-policy.hujson`);
- Tailscale missing or logged out: the human installs it or runs
  `tailscale up`. Setup never changes Tailscale.

On a node that is **not** tagged (user-owned), keep it that way: the policy
names it through the `athena-vm` hosts alias. Re-tagging an existing machine
changes its identity and can break other apps' access.

## 2. CI deploy keys (local, no secrets leave this machine)

One key per environment. The key decides which environment CI may touch, so
they must differ.

```bash
mkdir -p ~/.config/athena && chmod 700 ~/.config/athena
ssh-keygen -t ed25519 -N '' -C ci-staging -f ~/.config/athena/ci-staging
ssh-keygen -t ed25519 -N '' -C ci-prod    -f ~/.config/athena/ci-prod
```

Skip a key that already exists. The `.pub` halves go to the VM, the private
halves to GitHub environment secrets. Neither is ever shown in the chat.

## 3. ⏸ Plan approval (dry runs)

```bash
./scripts/setup-host.sh --vm "$VM" --dry-run \
    --ci-staging-pubkey ~/.config/athena/ci-staging.pub \
    --ci-prod-pubkey ~/.config/athena/ci-prod.pub
./scripts/init-github.sh --dry-run --vm-host 100.124.202.79 \
    --staging-key ~/.config/athena/ci-staging --prod-key ~/.config/athena/ci-prod
```

Show both outputs. `setup-host.sh` lists every user, directory, file, unit and
package it would add; it never touches Docker, ufw, sshd, Caddy or Tailscale.
`init-github.sh` lists the environments, tag ruleset, variables and secrets.
**Wait for an explicit "yes".**

## 4. ⏸ Tailscale (admin console, human only)

1. Open the admin console, Access controls. Merge the sections of
   `deploy/tailscale-policy.hujson` into the policy: `tagOwners`, the
   `athena-vm` host (set it to the VM's tailnet IP) and the four ACL rules.
   Keep the rules other apps already rely on.
2. Settings, OAuth clients, Generate: scope **Auth Keys (write)**, tag
   **`tag:ci`**. Copy the client id and secret.
3. In a terminal (not the chat), type them into GitHub:
   ```bash
   gh secret set TS_OAUTH_CLIENT_ID   # paste the id at the prompt
   gh secret set TS_OAUTH_SECRET      # paste the secret at the prompt
   ```

Works when: `gh secret list` shows both names.

## 5. Apply on the VM

```bash
./scripts/setup-host.sh --vm "$VM" \
    --ci-staging-pubkey ~/.config/athena/ci-staging.pub \
    --ci-prod-pubkey ~/.config/athena/ci-prod.pub
```

It runs with sudo over `ssh -t`, so the human types their sudo password.

Works when: it ends with `Done:` and a "Still for a human to do" list. It
creates `/etc/athena/{staging,prod}.env` and the OpenObserve root password
(`/etc/openobserve/openobserve.env`, root only) the first time, and never
overwrites them later.

On failure: it stops before changing anything else and says why (port
conflict, no tailnet IP, not root, checksum mismatch). Fix and re-run.

## 6. ⏸ Secrets on the VM (human only)

The human types these; the agent never sees them.

```bash
ssh -t "$VM" 'sudoedit /etc/athena/staging.env'   # OPENROUTER_API_KEY (a spend-capped key)
ssh -t "$VM" 'sudoedit /etc/athena/prod.env'      # OPENROUTER_API_KEY, TELEGRAM_BOT_TOKEN
```

Optional in both: `OPEN_SANDBOX_URL` (and `OPEN_SANDBOX_API_KEY`) once the
sandbox host is ready. Staging has no Telegram bot, on purpose.

The OpenObserve login is `admin@athena.internal`; the human reads the password
on the VM with `sudo grep ZO_ROOT_USER_PASSWORD /etc/openobserve/openobserve.env`.
The UI is `http://<tailnet-ip>:5080`, reachable from the tailnet only.

## 7. Apply on GitHub

```bash
./scripts/init-github.sh --vm-host 100.124.202.79 \
    --staging-key ~/.config/athena/ci-staging --prod-key ~/.config/athena/ci-prod
```

`VM_KNOWN_HOSTS` comes from `ssh-keyscan`; the script prints the fingerprint.
Ask the human to compare it with
`ssh "$VM" ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`.

Works when: it ends with `Done:` and the only "Still to do" items, if any,
are the Tailscale secrets from step 4.

## 8. First build, then install deploy-gate

deploy-gate is installed from a pinned release artifact, separately from app
releases, so a bad release can never replace the tool that rolls it back. The
first artifact comes from the first push to `main`.

1. Merge to `main` (or `gh workflow run ci-cd.yml --ref main`). The `build`
   job pushes `ghcr.io/queryplanner/athena:sha-<sha>` and prints its digest in
   the job summary. `deploy-staging` fails on this first run: deploy-gate is
   not installed yet.
2. ⏸ **Make the packages public** (human only; new GHCR packages start
   private even in a public repo): github.com, your profile, Packages,
   `athena`, Package settings, Change visibility, Public. Repeat for
   `athena-sandbox` once it exists.
3. Install the gate from that digest:
   ```bash
   ./scripts/setup-host.sh --vm "$VM" --gate-digest sha256:<digest from the build summary>
   ```
4. Re-run the failed jobs: `gh run rerun <run-id> --failed`.

Works when: `deploy-staging`, `bench-staging` and `smoke-staging` are green.
`eval-staging` is advisory and may be yellow.

To upgrade deploy-gate later, re-run step 3 with a newer build's digest.

## 9. Verify

```bash
./scripts/doctor.sh --vm "$VM"
```

Works when: `"ok": true`. Every failing check names its fix. "skip" means the
check needs something missing (for example `sudo` without a password on the
VM to read the env files); it is never guessed.

## 10. ⏸ First release (human only)

After a green staging run:

```bash
git tag -a v0.1.0 -m "first release" && git push origin v0.1.0
```

The tag run checks that this commit passed staging, tags the same artifact
`v0.1.0` in GHCR, then waits for approval. The human approves `deploy-prod` in
the Actions tab; it promotes the exact bytes staging tested and smoke-tests
prod.

## Day two

| Task | How |
|---|---|
| Deploy to staging | merge to `main` |
| Release to prod | push a `v*` tag on a commit that passed staging, approve |
| Roll back | tag the previous good commit (for example `v0.1.1`) and approve; if the schema moved forward, restore a backup first |
| Restore a backup | on the VM: `sudo /opt/athena/bin/deploy-gate restore <env> <backup-file>` |
| Status | `ssh "$VM" sudo cat /var/lib/athena/prod/state.json` |
| Logs | `ssh "$VM" journalctl -u athena-serve@prod -f` |
| Traces and logs UI | `http://<tailnet-ip>:5080` (OpenObserve) |
| Analytics | `./scripts/analytics.sh --list`, then `./scripts/analytics.sh --vm "$VM" --env prod cost_by_model_day` |
| Remove Athena | `./scripts/setup-host.sh --vm "$VM" --uninstall` (keeps data); add `--purge` to delete databases and secrets too |
