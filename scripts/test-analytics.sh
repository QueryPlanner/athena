#!/usr/bin/env bash
# Run every canned analytics query against fixtures and check the numbers.
# Needs duckdb and sqlite3. CI runs this; so can you.
#   - the database is tests/fixtures/v3_users_sessions.sql (real runs rows)
#   - spans are analytics/testdata/traces-*.jsonl, laid out as on the VM
#     (/var/lib/athena/<env>/telemetry/) so the default glob is exercised
#   - eval results are analytics/testdata/results.jsonl
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
sqlite3 "$WORK/agent.db" <"$REPO_ROOT/tests/fixtures/v3_users_sessions.sql"
mkdir -p "$WORK/lib/prod/telemetry" "$WORK/lib/staging/telemetry"
cp "$REPO_ROOT/analytics/testdata/traces-serve-20260921.jsonl" "$WORK/lib/prod/telemetry/"
cp "$REPO_ROOT/analytics/testdata/traces-telegram-20260922.jsonl" "$WORK/lib/staging/telemetry/"

q() { # query name -> JSON rows
    "$REPO_ROOT/scripts/analytics.sh" --json --db "$WORK/agent.db" \
        --telemetry-dir "$WORK/lib/*/telemetry" \
        --eval-results "$REPO_ROOT/analytics/testdata/results.jsonl" "$1"
}
fail() { echo "FAIL  $1" >&2; exit 1; }
pass() { echo "PASS  $1"; }

runs=$(sqlite3 "$WORK/agent.db" 'SELECT count(*) FROM runs')
got=$(q cost_by_model_day | jq -s 'flatten | map(.runs) | add')
[ "$got" = "$runs" ] || fail "cost_by_model_day counts $got runs, the table has $runs"
pass "cost_by_model_day covers all $runs runs"

tool=$(q tool_latency | jq -c 'flatten | map(select(.tool == "add"))[0]')
[ "$(jq .calls <<<"$tool")" = 3 ] || fail "tool_latency: $tool"
[ "$(jq '.error_rate * 3 | round' <<<"$tool")" = 1 ] || fail "tool_latency error rate: $tool"
[ "$(jq '.p50_ms | round' <<<"$tool")" = 300 ] || fail "tool_latency p50: $tool"
pass "tool_latency: add has 3 calls, p50 300 ms, 1 in 3 failed"

sb=$(q sandbox_failures | jq -c 'flatten | map({key: .span_name, value: .failures}) | from_entries')
[ "$sb" = '{"sandbox.exec":1,"browser.action":1}' ] || [ "$sb" = '{"browser.action":1,"sandbox.exec":1}' ] ||
    fail "sandbox_failures: $sb"
pass "sandbox_failures finds one sandbox and one browser failure"

rate=$(q eval_pass_rate | jq -sc 'flatten | map(select(.git_sha == "bbb"))[0].pass_rate')
[ "$rate" = 0.75 ] || fail "eval_pass_rate for bbb is $rate, want 0.75"
pass "eval_pass_rate: bbb passes 3 of 4 samples"
