#!/usr/bin/env bash
# Persistence test: two separate processes, one session.
set -euo pipefail
: "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY not set - source ~/.zshrc first}"
export OPENROUTER_API_KEY
export AGENT_MODEL="${AGENT_MODEL:-openai/gpt-5.6-luna}"

rm -f agent.db agent.db-wal agent.db-shm
echo "model: $AGENT_MODEL"
echo
echo "-- run 1: tool call, fresh session --"
cargo run --quiet -- testsess "Use the add tool to add 21 and 21. Reply with just the number."
echo
echo "-- run 2: separate process, must recall run 1 --"
cargo run --quiet -- testsess "What two numbers did I just ask you to add? Answer from our conversation, do not use a tool."
echo
echo "-- stored messages --"
sqlite3 agent.db "SELECT seq, substr(json,1,110) FROM messages WHERE session_id='testsess' ORDER BY seq;" \
  || echo "(sqlite3 not installed - skip)"

echo
echo "-- run telemetry --"
sqlite3 agent.db "SELECT model_calls, first_seq, last_seq, input_tokens, output_tokens, status
                  FROM runs ORDER BY started_at;" || echo "(sqlite3 not installed - skip)"
