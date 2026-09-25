#!/usr/bin/env bash
# Smoke-test a built athena binary the way production runs it: `serve` with an
# absolute ATHENA_DB, from an empty directory (so no .env is read), then
# SIGTERM. No model calls: the API key is a dummy and no turn is sent.
#
#   ./scripts/smoke-binary.sh target/release/athena
#
# CI's integration job runs this against the release build.
set -euo pipefail

BIN=$(cd "$(dirname "${1:?usage: smoke-binary.sh PATH/TO/athena}")" && pwd)/$(basename "$1")
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }
PORT=${SMOKE_PORT:-18089}
BASE="http://127.0.0.1:$PORT"

WORK=$(mktemp -d)
PID=""
cleanup() { [ -n "$PID" ] && kill "$PID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT
cd "$WORK"

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1" >&2; [ -f serve.log ] && sed 's/^/      /' serve.log >&2; exit 1; }

out=$(ATHENA_VERSION=smoke-1 "$BIN" --version) || fail "athena --version exited non-zero"
[ "$out" = "athena smoke-1" ] || fail "athena --version printed '$out', want 'athena smoke-1'"
pass "--version reports ATHENA_VERSION"

export ATHENA_DB="$WORK/smoke.db" ATHENA_VERSION=smoke-1 OPENROUTER_API_KEY=dummy-no-calls
"$BIN" serve --addr "127.0.0.1:$PORT" >serve.log 2>&1 &
PID=$!
for _ in $(seq 1 50); do
    curl -fsS "$BASE/health" >/dev/null 2>&1 && break
    kill -0 "$PID" 2>/dev/null || fail "serve exited during startup"
    sleep 0.2
done
curl -fsS "$BASE/health" >/dev/null || fail "/health did not answer within 10 s"
pass "/health answers"

version=$(curl -fsS "$BASE/version") || fail "/version failed"
[ "$(jq -r .version <<<"$version")" = "smoke-1" ] || fail "/version returned '$version'"
pass "/version reports ATHENA_VERSION"

created=$(curl -fsS -X POST -H 'X-Athena-User: smoke' -H 'Content-Type: application/json' \
    -d '{"name":"smoke"}' "$BASE/sessions") || fail "POST /sessions failed"
listed=$(curl -fsS -H 'X-Athena-User: smoke' "$BASE/sessions") || fail "GET /sessions failed"
grep -q '"smoke"' <<<"$listed" || fail "the new session is not listed: $listed (created: $created)"
pass "a session is created and listed"

code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: evil.example' -H 'X-Athena-User: smoke' "$BASE/sessions")
[ "$code" = 403 ] || fail "a foreign Host header got HTTP $code"
pass "foreign Host header is refused (HTTP $code)"

kill -TERM "$PID"
status=0
for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
kill -0 "$PID" 2>/dev/null && fail "serve still running 10 s after SIGTERM"
wait "$PID" || status=$?
PID=""
[ "$status" = 0 ] || fail "serve exited $status after SIGTERM, want 0"
pass "SIGTERM stops serve with exit 0"

"$BIN" backup "$WORK/backup/copy.db" >/dev/null 2>&1 || fail "athena backup failed"
[ "$(head -c 15 "$WORK/backup/copy.db")" = "SQLite format 3" ] || fail "backup is not a SQLite file"
pass "athena backup writes a SQLite copy"
