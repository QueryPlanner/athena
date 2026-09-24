#!/usr/bin/env bash
# Live end-to-end test: the real binary, the real provider, a throwaway database.
#
# Every check reads the database, not the model's wording, so a model that
# phrases things differently does not fail it. The two checks that do depend
# on the model (it chose the tool; it repeated a code word) retry once.
#
# Needs OPENROUTER_API_KEY and sqlite3. Costs a few cents. Not run in CI.
# See TESTING.md for what each check proves and how to extend it.
set -euo pipefail

: "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY not set}"
export AGENT_MODEL="${AGENT_MODEL:-openai/gpt-5.6-luna}"
command -v sqlite3 >/dev/null || { echo "sqlite3 is required"; exit 1; }

cd "$(dirname "$0")/.."
cargo build --quiet --locked
BIN="$PWD/target/debug/athena"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
export ATHENA_DB="$WORK/e2e.db"

q() { sqlite3 "$ATHENA_DB" "$1"; }
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; echo "      database kept at $ATHENA_DB"; trap - EXIT; exit 1; }
expect() { # description, actual, expected
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (got '$2', want '$3')"; fi
}
athena() { # run the binary; on failure, show why and stop
    if ! "$BIN" "$@" 2>"$WORK/stderr.log"; then
        # stderr: callers often send stdout to /dev/null or capture it.
        echo "FAIL  athena $* exited non-zero:" >&2
        sed 's/^/      /' "$WORK/stderr.log" >&2
        exit 1
    fi
}

echo "model: $AGENT_MODEL"

# Rows in a session whose content includes a part of the given type.
count_parts() { # session, part type
    q "SELECT COUNT(*) FROM messages, json_each(messages.json, '\$.content') AS part
       WHERE session_id = '$1' AND json_extract(part.value, '\$.type') = '$2'"
}

# Seq numbers run 0..n-1 with no gaps or repeats.
contiguous() { # session
    q "SELECT COUNT(*) = COUNT(DISTINCT seq) AND MIN(seq) = 0 AND MAX(seq) = COUNT(*) - 1
       FROM messages WHERE session_id = '$1'"
}

# Every run starts right after the previous one ended, and the last run ends
# on the last stored message.
runs_tile_transcript() { # session
    q "WITH r AS (SELECT first_seq, last_seq,
                         LAG(last_seq) OVER (ORDER BY started_at, rowid) AS prev
                  FROM runs WHERE session_id = '$1' AND status = 'ok')
       SELECT (SELECT COUNT(*) FROM r WHERE prev IS NOT NULL AND first_seq != prev + 1) = 0
          AND (SELECT MIN(first_seq) FROM r) = 0
          AND (SELECT MAX(last_seq) FROM r) =
              (SELECT MAX(seq) FROM messages WHERE session_id = '$1')"
}

echo
echo "-- 1. a tool-using turn --"
for attempt in 1 2; do
    rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
    athena e2e "Use the add tool to add 21 and 21. Reply with just the number." >/dev/null
    [ "$(count_parts e2e toolcall)" -ge 1 ] && break
    echo "      model did not call the tool on attempt $attempt"
done
[ "$(count_parts e2e toolcall)" -ge 1 ] || fail "model called the add tool"
pass "model called the add tool"
expect "the tool's result was stored" "$(count_parts e2e toolresult)" "1"
expect "one run recorded, status ok" "$(q "SELECT COUNT(*) || ' ' || status FROM runs WHERE session_id='e2e'")" "1 ok"
expect "a tool turn takes at least two model calls" \
    "$(q "SELECT model_calls >= 2 FROM runs WHERE session_id='e2e'")" "1"
expect "token usage recorded" \
    "$(q "SELECT input_tokens > 0 AND output_tokens > 0 FROM runs WHERE session_id='e2e'")" "1"
expect "seq numbers contiguous" "$(contiguous e2e)" "1"
expect "the run covers exactly the rows it appended" "$(runs_tile_transcript e2e)" "1"

echo
echo "-- 2. a second process continues the same session --"
before=$(q "SELECT COUNT(*) FROM messages WHERE session_id='e2e'")
athena e2e "Remember this code word: ZEBRA-7391. Reply with just OK." >/dev/null
recalled=""
for attempt in 1 2; do
    reply=$(athena e2e "What code word did I give you? Reply with just the code word.")
    if echo "$reply" | grep -qi 'zebra-7391'; then recalled=yes; break; fi
    echo "      reply on attempt $attempt: $reply"
done
[ -n "$recalled" ] || fail "a new process recalled the previous turn"
pass "a new process recalled the previous turn"
expect "earlier rows untouched" \
    "$(q "SELECT COUNT(*) FROM messages WHERE session_id='e2e' AND seq < $before")" "$before"
expect "seq numbers still contiguous" "$(contiguous e2e)" "1"
expect "each run starts where the previous one ended" "$(runs_tile_transcript e2e)" "1"

echo
echo "-- 3. sessions are isolated --"
e2e_rows=$(q "SELECT COUNT(*) FROM messages WHERE session_id='e2e'")
athena other "Say hi in one word." >/dev/null
expect "a new session starts at seq 0" "$(q "SELECT first_seq FROM runs WHERE session_id='other'")" "0"
expect "the other session was not touched" \
    "$(q "SELECT COUNT(*) FROM messages WHERE session_id='e2e'")" "$e2e_rows"
expect "sessions lists both" "$(athena sessions | cut -f1 | sort | tr '\n' ' ')" "e2e other "
expect "usage lists both" "$(athena usage | tail -n +2 | cut -f1 | sort | tr '\n' ' ')" "e2e other "

echo
echo "-- 4. a database from before migrations upgrades in place --"
rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
sqlite3 "$ATHENA_DB" < tests/fixtures/v0_main.sql
q "SELECT json FROM messages WHERE session_id='testsess' ORDER BY seq" > "$WORK/before.txt"
athena testsess "What was the result of the addition? Reply with just the number." >/dev/null
q "SELECT json FROM messages WHERE session_id='testsess' AND seq < 8 ORDER BY seq" > "$WORK/after.txt"
latest=$(ATHENA_DB="$WORK/fresh.db" "$BIN" usage >/dev/null && sqlite3 "$WORK/fresh.db" "PRAGMA user_version")
expect "schema migrated to the version a fresh database gets" "$(q "PRAGMA user_version")" "$latest"
cmp -s "$WORK/before.txt" "$WORK/after.txt" || fail "old rows unchanged byte for byte"
pass "old rows unchanged byte for byte"
expect "the new turn appended at seq 8" "$(q "SELECT first_seq FROM runs WHERE session_id='testsess'")" "8"
expect "seq numbers contiguous" "$(contiguous testsess)" "1"

echo
echo "All end-to-end checks passed."
