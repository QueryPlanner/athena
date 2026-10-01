# Live eval cases

Cases here need a sandbox server, so they cannot replay: the `replay` target
builds the agent without sandbox tools, and `eval run` reads `evals/cases`
by default. Run them against a running `athena serve` that has
`OPEN_SANDBOX_URL` set, over the tailnet:

    athena eval run --cases evals/live --target http://<vm-tailnet-ip>:18081

`skill_read_on_demand` also needs the server started with
`ATHENA_SKILLS_DIR` pointing at `evals/fixtures/skills` (the `house-style`
skill in it), so it fails on a server without that skill. It checks that the
model calls `read_skill` for a task the skill covers and follows what it
reads. The fixture is loaded by a unit test, so it cannot rot unnoticed.

They spend model credit and create an `eval` user's sessions and sandboxes
on that server. `gate` is `false`: they report and never fail a deploy.
