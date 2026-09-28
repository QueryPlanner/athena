# @NAME@

An Athena agent, made with `athena-cli new`. Its definition is
`src/agent.rs` and `prompts/system.md`; the runtime is `athena-core`.

## Run it locally

```bash
cp .env.example .env   # then put your OPENROUTER_API_KEY in it
cargo run              # a REPL
```

## Put it on the VM (once)

From your laptop, in this repo:

1. Push it to GitHub, then make its package public once the first build has
   pushed it (GitHub → Packages → @NAME@ → Package settings). The VM pulls
   releases anonymously.
2. `athena-cli vm add @NAME@ --vm USER@HOST --dry-run`, read it, then run it
   without `--dry-run`. It creates the agent's user, directories, env files,
   units and CI keys, and prints what is left for you (secrets, Tailscale ACL
   for its ports).
3. `athena-cli github init @NAME@ --vm-host HOST` to give this repo's CI its
   deploy keys and environments.
4. Merge to `main`. CI deploys staging. Tag `v0.1.0` on a green commit to
   release prod.

`athena-cli vm list --vm USER@HOST` shows every agent on the VM.
