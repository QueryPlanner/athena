#!/usr/bin/env bash
# Live end-to-end test: the real binary, the real provider, a throwaway database.
#
# Every check reads the database, not the model's wording, so a model that
# phrases things differently does not fail it. The two checks that do depend
# on the model (it chose the tool; it repeated a code word) retry once.
#
# Needs OPENROUTER_API_KEY, sqlite3, curl and python3 (section 8's fake
# Telegram server). Costs a few cents. Not run in CI.
# See TESTING.md for what each check proves and how to extend it.
set -euo pipefail

: "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY not set}"
export AGENT_MODEL="${AGENT_MODEL:-openai/gpt-5.6-luna}"
command -v sqlite3 >/dev/null || { echo "sqlite3 is required"; exit 1; }
command -v curl >/dev/null || { echo "curl is required"; exit 1; }
python3 -c 'import http.server' 2>/dev/null || { echo "python3 is required"; exit 1; }

cd "$(dirname "$0")/.."
cargo build --quiet --locked
BIN="$PWD/target/debug/athena"

WORK=$(mktemp -d)
# Background processes (section 8) are always stopped, even on failure.
BG_PIDS=()
stop_bg() { for pid in "${BG_PIDS[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done; }
trap 'stop_bg; rm -rf "$WORK"' EXIT
export ATHENA_DB="$WORK/e2e.db"

q() { sqlite3 "$ATHENA_DB" "$1"; }
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; echo "      database kept at $ATHENA_DB"; trap stop_bg EXIT; exit 1; }
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

# The id of a session by name, owned by cli:local unless a user is given.
sid() { # name, [transport], [external id]
    q "SELECT s.id FROM sessions s JOIN users u ON u.id = s.user_id
       WHERE s.name = '$1' AND u.transport = '${2:-cli}' AND u.external_id = '${3:-local}'"
}

# Rows in a session whose content includes a part of the given type.
count_parts() { # session id, part type
    q "SELECT COUNT(*) FROM messages, json_each(messages.json, '\$.content') AS part
       WHERE session_id = '$1' AND json_extract(part.value, '\$.type') = '$2'"
}

# Seq numbers run 0..n-1 with no gaps or repeats.
contiguous() { # session id
    q "SELECT COUNT(*) = COUNT(DISTINCT seq) AND MIN(seq) = 0 AND MAX(seq) = COUNT(*) - 1
       FROM messages WHERE session_id = '$1'"
}

# Every run starts right after the previous one ended, and the last run ends
# on the last stored message.
runs_tile_transcript() { # session id
    q "WITH r AS (SELECT first_seq, last_seq,
                         LAG(last_seq) OVER (ORDER BY started_at, rowid) AS prev
                  FROM runs WHERE session_id = '$1' AND status = 'ok')
       SELECT (SELECT COUNT(*) FROM r WHERE prev IS NOT NULL AND first_seq != prev + 1) = 0
          AND (SELECT MIN(first_seq) FROM r) = 0
          AND (SELECT MAX(last_seq) FROM r) =
              (SELECT MAX(seq) FROM messages WHERE session_id = '$1')"
}

messages_in() { # session id
    q "SELECT COUNT(*) FROM messages WHERE session_id = '$1'"
}

echo
echo "-- 1. a tool-using turn --"
for attempt in 1 2; do
    rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
    athena e2e "Use the add tool to add 21 and 21. Reply with just the number." >/dev/null
    E2E=$(sid e2e)
    [ "$(count_parts "$E2E" toolcall)" -ge 1 ] && break
    echo "      model did not call the tool on attempt $attempt"
done
[ "$(count_parts "$E2E" toolcall)" -ge 1 ] || fail "model called the add tool"
pass "model called the add tool"
expect "the tool's result was stored" "$(count_parts "$E2E" toolresult)" "1"
expect "one run recorded, status ok" "$(q "SELECT COUNT(*) || ' ' || status FROM runs WHERE session_id='$E2E'")" "1 ok"
expect "a tool turn takes at least two model calls" \
    "$(q "SELECT model_calls >= 2 FROM runs WHERE session_id='$E2E'")" "1"
expect "token usage recorded" \
    "$(q "SELECT input_tokens > 0 AND output_tokens > 0 FROM runs WHERE session_id='$E2E'")" "1"
expect "seq numbers contiguous" "$(contiguous "$E2E")" "1"
expect "the run covers exactly the rows it appended" "$(runs_tile_transcript "$E2E")" "1"

echo
echo "-- 2. a second process continues the same session --"
before=$(messages_in "$E2E")
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
    "$(q "SELECT COUNT(*) FROM messages WHERE session_id='$E2E' AND seq < $before")" "$before"
expect "seq numbers still contiguous" "$(contiguous "$E2E")" "1"
expect "each run starts where the previous one ended" "$(runs_tile_transcript "$E2E")" "1"

echo
echo "-- 3. sessions are isolated --"
e2e_rows=$(messages_in "$E2E")
athena other "Say hi in one word." >/dev/null
expect "a new session starts at seq 0" "$(q "SELECT first_seq FROM runs WHERE session_id='$(sid other)'")" "0"
expect "the other session was not touched" "$(messages_in "$E2E")" "$e2e_rows"
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
expect "the old session belongs to cli:local under its old id" "$(sid testsess)" "testsess"
expect "the new turn appended at seq 8" "$(q "SELECT first_seq FROM runs WHERE session_id='testsess'")" "8"
expect "seq numbers contiguous" "$(contiguous testsess)" "1"

echo
echo "-- 5. users and sessions --"
rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
created=$(athena sessions new notes)
NOTES=$(sid notes)
expect "sessions new prints the name and the stored id" "$created" "$(printf 'notes\t%s' "$NOTES")"
expect "a new session is empty and listed" "$(athena sessions)" "$(printf 'notes\t0 messages')"
if "$BIN" sessions new notes >/dev/null 2>&1; then fail "a duplicate name is refused"; fi
pass "a duplicate name is refused"
expect "and no second session was stored" "$(q "SELECT COUNT(*) FROM sessions WHERE name='notes'")" "1"
athena notes "Say OK in one word." >/dev/null
expect "chatting by name uses the created session" "$(messages_in "$NOTES")" "2"
expect "its run starts at seq 0" "$(q "SELECT first_seq FROM runs WHERE session_id='$NOTES'")" "0"

expect "another user starts with no sessions" "$(athena --user telegram:e2e sessions)" ""
athena --user telegram:e2e notes "Say hi in one word." >/dev/null
THEIRS=$(sid notes telegram e2e)
[ -n "$THEIRS" ] && [ "$THEIRS" != "$NOTES" ] || fail "the same name is a different session for another user"
pass "the same name is a different session for another user"
expect "their session holds only their own turn" "$(messages_in "$THEIRS")" "2"
expect "the cli user's session was not touched" "$(messages_in "$NOTES")" "2"
expect "each user lists only their own" \
    "$(athena sessions | cut -f1) / $(athena --user telegram:e2e sessions)" \
    "notes / $(printf 'notes\t2 messages')"
expect "usage is per user too" "$(athena --user telegram:e2e usage | tail -n +2 | cut -f1,2)" \
    "$(printf 'notes\t1')"
expect "every session has exactly one owner" \
    "$(q "SELECT COUNT(*) FROM sessions s LEFT JOIN users u ON u.id = s.user_id WHERE u.id IS NULL")" "0"
expect "no message belongs to a session that does not exist" \
    "$(q "SELECT COUNT(*) FROM messages WHERE session_id NOT IN (SELECT id FROM sessions)")" "0"

echo
echo "-- 6. a database at schema version 2 gains owners in place --"
rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
sqlite3 "$ATHENA_DB" < tests/fixtures/v2_run_telemetry.sql
q "SELECT session_id, seq, json FROM messages ORDER BY session_id, seq" > "$WORK/before.txt"
q "SELECT * FROM runs ORDER BY rowid" > "$WORK/runs_before.txt"
athena testsess "What was the result of adding 2 and 3? Reply with just the number." >/dev/null
q "SELECT session_id, seq, json FROM messages WHERE NOT (session_id='testsess' AND seq >= 12)
   ORDER BY session_id, seq" > "$WORK/after.txt"
q "SELECT * FROM runs WHERE rowid <= 3 ORDER BY rowid" > "$WORK/runs_after.txt"
expect "schema migrated to the version a fresh database gets" "$(q "PRAGMA user_version")" "$latest"
cmp -s "$WORK/before.txt" "$WORK/after.txt" || fail "every old message unchanged byte for byte"
pass "every old message unchanged byte for byte"
cmp -s "$WORK/runs_before.txt" "$WORK/runs_after.txt" || fail "every old run unchanged"
pass "every old run unchanged"
expect "every old session belongs to cli:local under its old id" \
    "$(sid broken) $(sid research) $(sid testsess)" "broken research testsess"
expect "the new turn appended at seq 12" \
    "$(q "SELECT first_seq FROM runs WHERE session_id='testsess' AND rowid > 3")" "12"
expect "seq numbers contiguous" "$(contiguous testsess)" "1"

echo
echo "-- 8. the Telegram bot, against a fake Bot API --"
# The real `athena telegram` binary polls scripts/fake_telegram.py through
# TELEGRAM_API_URL. Checks without the model come first, then real turns.
rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
python3 scripts/fake_telegram.py "$WORK/tg.port" &
BG_PIDS+=("$!")
for _ in $(seq 1 100); do [ -s "$WORK/tg.port" ] && break; sleep 0.1; done
[ -s "$WORK/tg.port" ] || fail "the fake Bot API started"
TG="http://127.0.0.1:$(cat "$WORK/tg.port")"
# Token-shaped, so teloxide redacts it, but built here: never a real secret.
TG_SECRET="not_a_real_secret_$(printf 'x%.0s' $(seq 1 18))"

BOT_PID=""
start_bot() {
    TELEGRAM_BOT_TOKEN="123456789:$TG_SECRET" TELEGRAM_API_URL="$TG" \
        "$BIN" telegram 2>>"$WORK/bot.log" &
    BOT_PID=$!
    BG_PIDS+=("$BOT_PID")
}
stop_bot() { # description
    kill -INT "$BOT_PID"
    if wait "$BOT_PID"; then pass "$1"; else sed 's/^/      /' "$WORK/bot.log"; fail "$1"; fi
}
tg_count() { curl -sf "$TG/control/count?method=$1&chat=${2:-0}"; } # method, [chat]
tg_text() { curl -sf "$TG/control/text?chat=$1&n=$2"; }              # chat, n (1-based)
tg_send() { curl -sf --data-urlencode "user=$1" --data-urlencode "text=$2" "$TG/control/message" >/dev/null; }
tg_wait() { # method, chat, n, description: wait until the fake saw n such calls
    for _ in $(seq 1 1200); do
        [ "$(tg_count "$1" "$2")" -ge "$3" ] && return 0
        kill -0 "$BOT_PID" 2>/dev/null || { sed 's/^/      /' "$WORK/bot.log"; fail "$4 (the bot exited)"; }
        sleep 0.1
    done
    sed 's/^/      /' "$WORK/bot.log"
    fail "$4 (timed out)"
}
tg_say() { # user, text: send it, wait for the next reply to that user, print it
    local n=$(($(tg_count sendMessage "$1") + 1))
    tg_send "$1" "$2"
    tg_wait sendMessage "$1" "$n" "a reply to telegram user $1 for '$2'"
    tg_text "$1" "$n"
}
tg_selected() { # telegram user id: name of their selected session
    q "SELECT s.name FROM selected_sessions c JOIN sessions s ON s.id = c.session_id
       JOIN users u ON u.id = c.user_id
       WHERE u.transport = 'telegram' AND u.external_id = '$1'"
}
tg_sessions() { # telegram user id: their sessions, as the CLI lists them
    athena --user "telegram:$1" sessions | tr '\t\n' ': '
}

start_bot
tg_wait getUpdates 0 1 "the bot started polling"
expect "the command menu was registered" "$(tg_count setMyCommands)" "1"

reply=$(tg_say 9001 "/new side")
expect "/new answers" "$reply" "Started session \`side\`. Your messages go to it now."
expect "/new created the session for that telegram user" "$(tg_sessions 9001)" "side:0 messages "
expect "/new stored the selection" "$(tg_selected 9001)" "side"
expect "/sessions marks it" "$(tg_say 9001 /sessions)" "$(printf 'Your sessions:\n* side (0 messages)')"
expect "/switch default answers" "$(tg_say 9001 "/switch default")" "Switched to \`default\` (0 messages)."
expect "/switch stored the selection" "$(tg_selected 9001)" "default"
reply=$(tg_say 9001 "/switch nope")
case "$reply" in "You have no session named \`nope\`"*) pass "/switch refuses a missing session";;
    *) fail "/switch refuses a missing session (got '$reply')";; esac
expect "and does not create it" "$(tg_sessions 9001)" "default:0 messages side:0 messages "
reply=$(tg_say 9003 "/switch side")
case "$reply" in "You have no session named \`side\`"*) pass "another user cannot switch to it";;
    *) fail "another user cannot switch to it (got '$reply')";; esac
expect "the other user has no sessions" "$(tg_sessions 9003)" ""

stop_bot "Ctrl-C stops the bot cleanly"
polls=$(tg_count getUpdates)
start_bot
tg_wait getUpdates 0 $((polls + 1)) "the restarted bot started polling"
expect "after a restart the selection is still current" \
    "$(tg_say 9001 /sessions)" "$(printf 'Your sessions:\n* default (0 messages)\n  side (0 messages)')"
expect "the old process's messages were not handled again" "$(tg_count sendMessage 9001)" "5"

# From here on, real turns against OpenRouter.
reply=$(tg_say 9002 "Remember this code word: OTTER-5150. Reply with just OK.")
case "$reply" in
    "" | "The model failed"* | "Something went wrong"*)
        sed 's/^/      /' "$WORK/bot.log"
        fail "a prompt gets a model reply through the Bot API (got '$reply')";;
esac
pass "a prompt gets a model reply through the Bot API"
expect "a first message creates the user and their default session" \
    "$(tg_sessions 9002)" "default:2 messages "
DEFAULT_9002=$(sid default telegram 9002)
expect "one run recorded, status ok" \
    "$(q "SELECT COUNT(*) || ' ' || status FROM runs WHERE session_id='$DEFAULT_9002'")" "1 ok"
[ "$(tg_count sendChatAction 9002)" -ge 1 ] || fail "the bot showed typing before replying"
pass "the bot showed typing before replying"

tg_say 9001 "Say hi in one word." >/dev/null
expect "a prompt goes to the selected session" "$(tg_sessions 9001)" "default:2 messages side:0 messages "
tg_say 9001 "/switch side" >/dev/null
tg_say 9001 "Say hello in one word." >/dev/null
expect "after /switch, prompts go to the new selection" \
    "$(tg_sessions 9001)" "default:2 messages side:2 messages "

tg_say 9002 "/new other" >/dev/null
tg_say 9002 "/switch default" >/dev/null
recalled=""
for attempt in 1 2; do
    reply=$(tg_say 9002 "What code word did I give you? Reply with just the code word.")
    if echo "$reply" | grep -qi 'otter-5150'; then recalled=yes; break; fi
    echo "      reply on attempt $attempt: $reply"
done
[ -n "$recalled" ] || fail "switching back resumes the conversation"
pass "switching back resumes the conversation"
expect "the code word never reached the other telegram user's sessions" \
    "$(q "SELECT COUNT(*) FROM messages m JOIN sessions s ON s.id = m.session_id
          JOIN users u ON u.id = s.user_id
          WHERE u.external_id = '9001' AND m.json LIKE '%OTTER%'")" "0"
expect "/usage answers from the database" "$(tg_say 9002 /usage | cut -d: -f1)" "default"

stop_bot "the restarted bot stops cleanly"
expect "no session lacks an owner" \
    "$(q "SELECT COUNT(*) FROM sessions s LEFT JOIN users u ON u.id = s.user_id WHERE u.id IS NULL")" "0"
if grep -q "$TG_SECRET" "$WORK/bot.log"; then fail "the bot token never reaches the log"; fi
pass "the bot token never reaches the log"

echo
echo "All end-to-end checks passed."
