# Live eval cases

Cases here need a sandbox server, so they cannot replay: the `replay` target
builds the agent without sandbox tools, and `eval run` reads `evals/cases`
by default. Run them against a running `athena serve` that has
`OPEN_SANDBOX_URL` set, over the tailnet:

    athena eval run --cases evals/live --target http://<vm-tailnet-ip>:18081

They spend model credit and create an `eval` user's sessions and sandboxes
on that server. `gate` is `false`: they report and never fail a deploy.
