#!/usr/bin/env bash
# Run a canned DuckDB query from analytics/queries/ against Athena's data.
#
#   ./scripts/analytics.sh --list
#   ./scripts/analytics.sh cost_by_model_day                    # local files
#   ./scripts/analytics.sh --vm appuser@athena-vm --env prod tool_latency
#
# Queries are templates. Placeholders filled here:
#   {{ATHENA_DB}}     SQLite database, opened READ_ONLY (default /var/lib/athena/<env>/agent.db)
#   {{TELEMETRY_DIR}} directory of Athena's traces-*.jsonl files (default
#                     /var/lib/athena/*/telemetry: both environments)
#   {{EVAL_RESULTS}}  eval results JSONL glob (default results/*.jsonl)
#   {{PRICES}}        analytics/prices.csv
# A query marked "-- needs: spans.sql" gets analytics/queries/spans.sql first.
#
# With --vm the rendered SQL is copied to the VM and run there with
# `sudo duckdb` (the data is readable only by the athena user),
# so ssh asks for your sudo password on a terminal. Nothing is written.
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
QUERIES=$REPO_ROOT/analytics/queries
ENV_NAME=prod
VM=""
DB=""
TELEMETRY_DIR="/var/lib/athena/*/telemetry"
EVAL_RESULTS="results/*.jsonl"
MODE=box
QUERY=""

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
    echo
    echo "Options: --list  --env staging|prod  --vm USER@HOST  --db PATH  --telemetry-dir DIR"
    echo "         --eval-results GLOB  --json"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --list) for f in "$QUERIES"/*.sql; do
                    n=$(basename "$f" .sql); [ "$n" = spans ] && continue
                    printf '%-20s %s\n' "$n" "$(head -n1 "$f" | sed 's/^-- //')"
                done; exit 0 ;;
        --env) ENV_NAME="${2:?}"; shift ;;
        --vm) VM="${2:?}"; shift ;;
        --db) DB="${2:?}"; shift ;;
        --telemetry-dir) TELEMETRY_DIR="${2:?}"; shift ;;
        --eval-results) EVAL_RESULTS="${2:?}"; shift ;;
        --json) MODE=json ;;
        -h|--help) usage; exit 0 ;;
        -*) usage >&2; echo "analytics: unknown option: $1" >&2; exit 2 ;;
        *) QUERY=$1 ;;
    esac
    shift
done

[ -n "$QUERY" ] || { usage >&2; exit 2; }
[[ "$ENV_NAME" =~ ^(staging|prod)$ ]] || { echo "analytics: --env must be staging or prod" >&2; exit 2; }
if ! [[ "$QUERY" =~ ^[a-z0-9_]+$ && -f "$QUERIES/$QUERY.sql" ]]; then
    echo "analytics: no query '$QUERY'; see --list" >&2
    exit 2
fi
[ -n "$DB" ] || DB=/var/lib/athena/$ENV_NAME/agent.db

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Prices travel with the SQL, so the VM needs no checkout.
prices=$WORK/prices.csv
cp "$REPO_ROOT/analytics/prices.csv" "$prices"
[ -z "$VM" ] || prices=/tmp/athena-analytics/prices.csv

# Values are paths; refuse quotes so they cannot break out of SQL strings.
for v in "$DB" "$TELEMETRY_DIR" "$EVAL_RESULTS"; do
    [[ "$v" != *"'"* ]] || { echo "analytics: paths may not contain quotes: $v" >&2; exit 2; }
done

render() {
    sed -e "s#{{ATHENA_DB}}#$DB#g" -e "s#{{TELEMETRY_DIR}}#$TELEMETRY_DIR#g" \
        -e "s#{{EVAL_RESULTS}}#$EVAL_RESULTS#g" -e "s#{{PRICES}}#$prices#g" "$1"
}
{
    grep -q '^-- needs: spans.sql' "$QUERIES/$QUERY.sql" && render "$QUERIES/spans.sql"
    render "$QUERIES/$QUERY.sql"
} >"$WORK/query.sql"

if [ -z "$VM" ]; then
    command -v duckdb >/dev/null 2>&1 || { echo "analytics: duckdb CLI not found" >&2; exit 1; }
    duckdb "-$MODE" -f "$WORK/query.sql"
    exit 0
fi

tar -C "$WORK" -czf - query.sql prices.csv |
    ssh "$VM" 'rm -rf /tmp/athena-analytics && mkdir -m 0755 /tmp/athena-analytics && tar -xzf - -C /tmp/athena-analytics && chmod 0644 /tmp/athena-analytics/*'
status=0
# shellcheck disable=SC2029  # MODE is one of two fixed words
ssh -t "$VM" "sudo /usr/local/bin/duckdb -$MODE -f /tmp/athena-analytics/query.sql" || status=$?
ssh "$VM" 'rm -rf /tmp/athena-analytics' || true
exit "$status"
