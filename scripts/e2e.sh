#!/usr/bin/env bash
# Live end-to-end test: the real binary, the real provider, a throwaway database.
#
# Every check reads the database, not the model's wording, so a model that
# phrases things differently does not fail it. The two checks that do depend
# on the model (it chose the tool; it repeated a code word) retry once.
#
# Needs OPENROUTER_API_KEY, in the shell or in .env, plus sqlite3, curl and
# python3 (section 8's fake Telegram server). Costs a few cents. Not run in
# CI. See TESTING.md for what each check proves and how to extend it.
set -euo pipefail

cd "$(dirname "$0")/.."
# The binary runs from here, so it loads ./.env itself. This only checks that
# a key is available one way or the other; the script never reads it.
if [ -z "${OPENROUTER_API_KEY:-}" ] && ! grep -q '^OPENROUTER_API_KEY=..*' .env 2>/dev/null; then
    echo "OPENROUTER_API_KEY not set: export it or add it to .env"
    exit 1
fi
export AGENT_MODEL="${AGENT_MODEL:-openai/gpt-5.6-luna}"
command -v sqlite3 >/dev/null || { echo "sqlite3 is required"; exit 1; }
command -v curl >/dev/null || { echo "curl is required"; exit 1; }
python3 -c 'import http.server' 2>/dev/null || { echo "python3 is required"; exit 1; }

cargo build --quiet --locked
BIN="$PWD/target/debug/athena"

WORK=$(mktemp -d)
# Processes a section starts in the background, killed on every exit.
BACKGROUND=""
stop_background() { for pid in $BACKGROUND; do kill "$pid" 2>/dev/null || true; done; }
trap 'stop_background; rm -rf "$WORK"' EXIT
export ATHENA_DB="$WORK/e2e.db"

q() { sqlite3 "$ATHENA_DB" "$1"; }
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; echo "      database kept at $ATHENA_DB"; stop_background; trap - EXIT; exit 1; }
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
echo "-- 7. the HTTP API --"
rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
SERVE_LOG="$WORK/serve.log"
echo "      server log: $SERVE_LOG (kept on failure)"

# Port 0: the OS picks a free port, and the server prints the one it got.
"$BIN" serve --addr 127.0.0.1:0 2>"$SERVE_LOG" &
SERVE_PID=$!
BACKGROUND="$BACKGROUND $SERVE_PID"
for _ in $(seq 1 100); do grep -q '^listening on ' "$SERVE_LOG" && break; sleep 0.1; done
URL=$(sed -n 's/^listening on //p' "$SERVE_LOG")
[ -n "$URL" ] || fail "athena serve started and printed its address"
pass "athena serve started on $URL"

# A field of a JSON document, read with sqlite3's JSON functions.
jget() { # json, path
    sqlite3 :memory: "SELECT json_extract('${1//\'/\'\'}', '$2')"
}
jlen() { # json, path: the length of the array there
    sqlite3 :memory: "SELECT json_array_length('${1//\'/\'\'}', '$2')"
}
# One request. Prints the body, then the status code on its own line.
api() { # method, path, user ('' for none), [json body]
    local args=(-sS -X "$1" -w '\n%{http_code}')
    [ -n "$3" ] && args+=(-H "X-Athena-User: $3")
    [ -n "${4:-}" ] && args+=(-d "$4")
    curl "${args[@]}" "$URL$2"
}
body() { sed '$d' <<<"$1"; }
code() { tail -n 1 <<<"$1"; }
# Stream one message; the raw SSE response goes to a file.
stream_to() { # file, user, session id, text
    curl -sS -N -o "$1" -w '%{http_code}' -H "X-Athena-User: $2" \
        -d "{\"text\":\"$4\"}" "$URL/sessions/$3/messages/stream"
}
last_run() { # session id: the newest run's id
    q "SELECT run_id FROM runs WHERE session_id='$1' ORDER BY started_at DESC, rowid DESC LIMIT 1"
}

r=$(api GET /health '')
expect "health answers without a user" "$(code "$r") $(jget "$(body "$r")" '$.status')" "200 ok"
r=$(api GET /sessions '')
expect "a request without X-Athena-User is refused" \
    "$(code "$r") $(jget "$(body "$r")" '$.error.code')" "400 invalid"
r=$(curl -sS -w '\n%{http_code}' -H 'Host: attacker.example' -H 'X-Athena-User: alice' "$URL/sessions")
expect "a request addressed to another host is refused" "$(code "$r")" "403"
expect "and created no user" "$(q "SELECT COUNT(*) FROM users WHERE transport='http'")" "0"

r=$(api POST /sessions alice '{"name":"web"}')
WEB=$(jget "$(body "$r")" '$.id')
expect "creating a session returns 201 and the stored id" "$(code "$r") $WEB" "201 $(sid web http alice)"
r=$(api POST /sessions alice '{"name":"web"}')
expect "a duplicate name is 409" "$(code "$r") $(jget "$(body "$r")" '$.error.code')" "409 already_exists"

for attempt in 1 2; do
    r=$(api POST "/sessions/$WEB/messages" alice \
        '{"text":"Use the add tool to add 21 and 21. Reply with just the number."}')
    [ "$(code "$r")" = 200 ] || fail "a message returns 200 (got $(code "$r"): $(body "$r"))"
    [ "$(count_parts "$WEB" toolcall)" -ge 1 ] && break
    echo "      model did not call the tool on attempt $attempt"
done
[ "$(count_parts "$WEB" toolcall)" -ge 1 ] || fail "over HTTP, the model called the add tool"
pass "over HTTP, the model called the add tool"
reply=$(body "$r")
expect "the reply names the run that was stored" "$(jget "$reply" '$.run.run_id')" "$(last_run "$WEB")"
expect "that run is ok, with tokens and several model calls" \
    "$(jget "$reply" '$.run.status') $(q "SELECT status, model_calls >= 2, input_tokens > 0 FROM runs WHERE run_id='$(last_run "$WEB")'")" \
    "ok ok|1|1"
expect "the run's range ends on the last stored row" \
    "$(jget "$reply" '$.run.last_seq')" "$(q "SELECT MAX(seq) FROM messages WHERE session_id='$WEB'")"
r=$(api GET "/sessions/$WEB/messages" alice)
expect "the transcript endpoint returns every stored row" \
    "$(code "$r") $(jlen "$(body "$r")" '$.messages')" "200 $(messages_in "$WEB")"

rows=$(messages_in "$WEB")
status=$(stream_to "$WORK/stream.txt" alice "$WEB" "Count from 1 to 5, one number per line.")
events=$(grep '^event: ' "$WORK/stream.txt" | sed 's/^event: //')
expect "a stream answers 200" "$status" "200"
[ "$(grep -c '^delta$' <<<"$events")" -ge 1 ] || fail "a stream sends text deltas ($events)"
pass "a stream sends text deltas"
expect "it ends with exactly one done and no error" \
    "$(tail -n 1 <<<"$events") $(grep -c '^done$' <<<"$events") $(grep -c '^error$' <<<"$events")" "done 1 0"
expect "every event line is followed by a data line" \
    "$(grep -A1 '^event: ' "$WORK/stream.txt" | grep -c '^data: ')" "$(wc -l <<<"$events" | tr -d ' ')"
done_data=$(grep -A1 '^event: done' "$WORK/stream.txt" | sed -n 's/^data: //p')
expect "done names the run that was stored" "$(jget "$done_data" '$.run.run_id')" "$(last_run "$WEB")"
expect "the streamed run is ok and counted tokens" \
    "$(q "SELECT status, input_tokens > 0, output_tokens > 0 FROM runs WHERE run_id='$(last_run "$WEB")'")" "ok|1|1"
expect "the streamed turn was appended after the earlier rows" \
    "$(q "SELECT first_seq FROM runs WHERE run_id='$(last_run "$WEB")'")" "$rows"
expect "seq numbers contiguous" "$(contiguous "$WEB")" "1"
expect "each run starts where the previous one ended" "$(runs_tile_transcript "$WEB")" "1"

rows=$(messages_in "$WEB")
missing=$(api GET /sessions/no-such-session/messages bob)
expect "bob gets 404 for an id that does not exist" "$(code "$missing")" "404"
expect "bob reading alice's session gets the same answer" "$(api GET "/sessions/$WEB/messages" bob)" "$missing"
expect "bob sending to it gets the same answer" "$(api POST "/sessions/$WEB/messages" bob '{"text":"hi"}')" "$missing"
expect "bob streaming to it gets 404 before any stream" \
    "$(stream_to "$WORK/bob.txt" bob "$WEB" "hi") $(grep -c '^event:' "$WORK/bob.txt")" "404 0"
expect "bob lists no sessions" "$(body "$(api GET /sessions bob)")" '{"sessions":[]}'
expect "bob has no usage" "$(body "$(api GET /usage bob)")" '{"usage":[]}'
expect "alice's session was not touched" "$(messages_in "$WEB")" "$rows"
r=$(api GET /usage alice)
expect "alice's usage counts her runs" "$(jget "$(body "$r")" '$.usage[0].runs')" \
    "$(q "SELECT COUNT(*) FROM runs WHERE session_id='$WEB'")"

# A client that hangs up mid-stream: the turn still finishes and is saved.
runs_before=$(q "SELECT COUNT(*) FROM runs WHERE session_id='$WEB'")
curl -sS -N -o "$WORK/cut.txt" -H "X-Athena-User: alice" \
    -d '{"text":"Write twenty numbered sentences about the sea."}' \
    "$URL/sessions/$WEB/messages/stream" &
CURL_PID=$!
for _ in $(seq 1 300); do grep -q '^event: delta' "$WORK/cut.txt" 2>/dev/null && break; sleep 0.1; done
kill "$CURL_PID" 2>/dev/null || true
wait "$CURL_PID" 2>/dev/null || true
grep -q '^event: done' "$WORK/cut.txt" && echo "      note: the turn finished before the client hung up"
for _ in $(seq 1 600); do
    [ "$(q "SELECT COUNT(*) FROM runs WHERE session_id='$WEB'")" -gt "$runs_before" ] && break
    sleep 0.1
done
expect "a client that hung up mid-stream still got an ok run" \
    "$(q "SELECT status FROM runs WHERE run_id='$(last_run "$WEB")'")" "ok"
expect "and its whole turn was saved" "$(runs_tile_transcript "$WEB")" "1"

# SIGTERM (docker stop, systemd) while a stream is running: it finishes,
# then the server exits.
runs_before=$(q "SELECT COUNT(*) FROM runs WHERE session_id='$WEB'")
stream_to "$WORK/drain.txt" alice "$WEB" "Write ten numbered sentences about mountains." >/dev/null &
CURL_PID=$!
for _ in $(seq 1 300); do grep -q '^event: delta' "$WORK/drain.txt" 2>/dev/null && break; sleep 0.1; done
kill -TERM "$SERVE_PID"
wait "$CURL_PID" || true
expect "a stream in flight at SIGTERM still ends with done" \
    "$(grep '^event: ' "$WORK/drain.txt" | tail -n 1)" "event: done"
serve_status=0
wait "$SERVE_PID" || serve_status=$?
expect "athena serve exits cleanly after SIGTERM" "$serve_status" "0"
grep -q 'shutting down' "$SERVE_LOG" || fail "athena serve said it was shutting down"
pass "athena serve said it was shutting down"
expect "the turn in flight was saved" \
    "$(q "SELECT COUNT(*) FROM runs WHERE session_id='$WEB' AND status='ok'")" "$((runs_before + 1))"
if curl -s -o /dev/null "$URL/health"; then fail "the server no longer answers"; fi
pass "the server no longer answers"
echo "-- 8. the Telegram bot, against a fake Bot API --"
# The real `athena telegram` binary polls scripts/fake_telegram.py through
# TELEGRAM_API_URL. Checks without the model come first, then real turns.
rm -f "$ATHENA_DB" "$ATHENA_DB-wal" "$ATHENA_DB-shm"
python3 scripts/fake_telegram.py "$WORK/tg.port" &
BACKGROUND="$BACKGROUND $!"
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
    BACKGROUND="$BACKGROUND $BOT_PID"
}
stop_bot() { # signal (INT is Ctrl-C, TERM is docker stop), description
    kill "-$1" "$BOT_PID"
    shift
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

stop_bot TERM "SIGTERM stops the bot cleanly"
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

stop_bot INT "Ctrl-C stops the restarted bot cleanly"
expect "no session lacks an owner" \
    "$(q "SELECT COUNT(*) FROM sessions s LEFT JOIN users u ON u.id = s.user_id WHERE u.id IS NULL")" "0"
if grep -q "$TG_SECRET" "$WORK/bot.log"; then fail "the bot token never reaches the log"; fi
pass "the bot token never reaches the log"

echo
echo "All end-to-end checks passed."
