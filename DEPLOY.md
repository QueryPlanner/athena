# deploy-gate

`deploy-gate` is the only program CI can run on the VM. It deploys a release
artifact to staging, promotes the same artifact to prod, and runs the checks
CI needs. The source is `src/bin/deploy-gate.rs` (wiring) and `src/gate/`
(everything else). The interface is fixed in `plans/contracts.md`, section
"deploy-gate protocol".

## How CI reaches it

The `deploy` user's `authorized_keys` has one key per environment. Each key
can only run the gate, and the gate knows which key was used:

```
restrict,command="sudo /opt/athena/bin/deploy-gate --key-env staging" ssh-ed25519 AAAA... ci-staging
restrict,command="sudo /opt/athena/bin/deploy-gate --key-env prod" ssh-ed25519 AAAA... ci-prod
```

sudo drops `SSH_ORIGINAL_COMMAND` by default, so sudoers needs:

```
deploy ALL=(root) NOPASSWD: /opt/athena/bin/deploy-gate *
Defaults!/opt/athena/bin/deploy-gate env_keep += "SSH_ORIGINAL_COMMAND"
```

CI sends the command as the SSH command line, for example
`ssh deploy@vm deploy staging sha256:...`. The gate reads it from
`SSH_ORIGINAL_COMMAND`, splits it on whitespace (no shell is involved) and
accepts it only if it matches one of these, word for word:

| Key | Commands |
|---|---|
| staging | `deploy staging <digest>`, `smoke staging`, `bench staging`, `eval staging`, `status staging` |
| prod | `promote prod <digest>`, `smoke prod`, `status prod` |

`<digest>` must be `sha256:` followed by 64 lowercase hex digits. Anything
else, including extra words, shell syntax or the other environment's
commands, exits 2 with `rejected: ...` before anything happens.

An admin with root runs two more commands directly, never through a key:

```
sudo /opt/athena/bin/deploy-gate restore <staging|prod> <backup-file>
sudo /opt/athena/bin/deploy-gate install-gate <digest>
deploy-gate --version
```

## Output

Progress lines go to stderr. The last thing on stdout is one JSON line:

```
{"ok":true,"command":"deploy","env":"staging","digest":"sha256:...","version":"<git sha>","previous":"<hex>","backup":"/var/lib/athena/staging/backups/20260925T120000Z.db","pruned":[]}
{"ok":false,"command":"deploy","error":"not healthy after 60s: ...; rolled back to <hex>. ..."}
{"ok":false,"rejected":"\"deploy prod; rm -rf /\" is not allowed for the staging key"}
```

| Exit | Meaning |
|---|---|
| 0 | done |
| 1 | the operation failed; the message says what state it left |
| 2 | the input was rejected (bad command, bad digest, or a promote of a digest staging does not run); nothing changed |

## What `deploy staging <digest>` does

1. Takes an exclusive lock on `/var/lib/athena/.gate.lock`. If another gate
   holds it, it fails at once instead of waiting.
2. Reads `/etc/athena/staging.env` (it needs `ATHENA_ADDR=<ip>:<port>`) and
   `/etc/athena/gate.env`.
3. Checks that `/opt/athena/releases` and `/var/lib/athena` each have at
   least 1 GiB free.
4. Pulls the artifact unless it is cached:
   `$ORAS pull $ATHENA_REPO@<digest> -o /opt/athena/releases/<hex>.tmp`,
   `chmod 0755`, then renames it to `/opt/athena/releases/<hex>`. The version
   is read from the manifest's `org.opencontainers.image.revision`
   annotation. Without one it is `sha256:<first 12 hex digits>`.
5. Runs `<new>/athena --version` as user `athena`. It must print `athena ...`.
6. Stops `athena@staging.target`, `athena-serve@staging.service` and
   `athena-telegram@staging.service`.
7. If there is a database and a previous release, runs `<old>/athena backup
   /var/lib/athena/staging/backups/<utc>.db` as `athena`, with the env
   file's variables. It keeps the newest 10 backups it named. If the backup
   fails, the old release is started again and nothing else changes.
8. Points `/opt/athena/staging/current` at the new release (a new symlink,
   renamed over the old one).
9. Sets `ATHENA_VERSION` in the env file, keeping the file's mode and owner.
10. Starts `athena-serve@staging`, then polls `/health` and `/version` once a
    second for up to 60 s. `/version` must report the new version.
11. Starts `athena-telegram@staging` if `systemctl is-enabled` says it is
    enabled.
12. Writes `/var/lib/athena/gate/staging.state.json`
    (`{"digest","version","deployed_at"}`), then prunes the release cache to
    the `ATHENA_KEEP_RELEASES` most recently used releases. A release that
    staging or prod points at is never removed.

If step 8, 9, 10 or 11 fails, the gate stops the units, points `current`
back at the previous release, restores its `ATHENA_VERSION`, starts it,
waits for it to be healthy, and exits 1. If the new release had already
migrated the database, the old binary may refuse it: restore the backup
from step 7. On a first deploy there is nothing to roll back to, so the
units are left stopped.

`promote prod <digest>` does the same for prod, and first refuses (exit 2)
unless `<digest>` is the digest in staging's `state.json`. The release is
normally already cached from staging, so nothing is pulled.

## The other commands

| Command | Does |
|---|---|
| `smoke <env>` | `GET /health`, `GET /version` (must match `state.json`), then as user `smoke`: `POST /sessions` and one real message. Fails on any error. |
| `bench staging` | `<current>/athena bench --url http://<ATHENA_ADDR>` as `athena`. Exit 1 if it fails its thresholds; the JSON line carries its summary. |
| `eval staging` | `<current>/athena eval run --target http://<ATHENA_ADDR> --cases <current>/evals` as `athena`, when the release has an `evals` directory; otherwise skipped. Advisory: exit 1 only if the run itself errors. |
| `status <env>` | prints `state.json` and the release `current` points at |
| `restore <env> <file>` | admin only. `<file>` is a name in the env's `backups/` directory, not a path. Backs up the current database first, replaces `agent.db` (removing its `-wal` and `-shm`), restarts, and waits for health. |
| `install-gate <digest>` | admin only. Pulls the artifact, runs its `deploy-gate --version` as `athena`, and installs it as `/opt/athena/bin/deploy-gate`. CI cannot do this, so a bad app release can never replace the tool that rolls it back. |

## Configuration

`/etc/athena/gate.env` (root, 0644). Every line is optional:

```
ATHENA_REPO=ghcr.io/queryplanner/athena
ATHENA_KEEP_RELEASES=3
ORAS=/usr/local/bin/oras
```

Env files use systemd's `EnvironmentFile=` syntax. As in systemd, `$VAR` is
not expanded.

## Safety properties

- No shell anywhere. Every process is an argv with an empty environment plus
  a fixed `PATH` (and, for `athena` commands, the env file's variables).
- Binaries from a release run only as `athena`, never as root.
- Temporary files that replace an env file are created with its mode, so a
  secret is never world-readable, even briefly.
- Backup file names cannot contain `/`, so `restore` cannot read outside the
  backups directory.
- `state.json` lives in `/var/lib/athena/gate/`, which only root can write.
  The service can write its own data directory, and `promote` trusts
  staging's state, so a compromised staging service must not be able to
  forge it.
- Progress writes to stderr never panic, and SIGHUP is ignored, so a CI
  connection that drops mid-deploy does not leave the units stopped.
- `ATHENA_ALLOWED_HOSTS`, when set, must include `ATHENA_ADDR`: the gate
  sends that as `Host`. HTTP responses are read up to 1 MiB.
- HTTP goes only to the `ATHENA_ADDR` socket address from the env file,
  which must parse as `ip:port`.

## Testing

Every side effect (processes, HTTP, the clock, free disk, stderr) goes
through the `System` trait, and every path is under a root directory. The
unit tests in `src/gate/` run the real logic against a temporary root and a
fake VM (`src/gate/testing.rs`) that behaves like systemd, ORAS, `athena`
and `athena serve`. `tests/deploy_gate.rs` runs the built binary, but only
with input it must reject, because the binary is rooted at `/`.
