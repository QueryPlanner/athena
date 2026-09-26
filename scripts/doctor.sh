#!/usr/bin/env bash
# Check a finished Athena setup end to end. Read-only.
#
#   ./scripts/doctor.sh --vm appuser@100.124.202.79
#
# Prints a human summary on stderr and one JSON object on stdout:
#   {"ok": bool, "checks": [{"name", "status": pass|warn|fail|skip, "detail"}]}
# Exits 1 when any check fails. Checks that need something missing (ssh
# access, gh, sudo without a password) are reported as "skip", never guessed.
set -euo pipefail

VM=""
REPO=""
HOST=""
PORT_PROD=18080
PORT_STAGING=18081
PORT_O2=5080

while [ $# -gt 0 ]; do
    case "$1" in
        --vm) VM="${2:?}"; shift ;;
        --repo) REPO="${2:?}"; shift ;;
        --host) HOST="${2:?}"; shift ;;
        --port-prod) PORT_PROD="${2:?}"; shift ;;
        --port-staging) PORT_STAGING="${2:?}"; shift ;;
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
            echo "Options: --vm USER@HOST  --repo OWNER/NAME  --host TAILNET-HOST  --port-prod N  --port-staging N"
            exit 0 ;;
        *) echo "doctor: unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

has() { command -v "$1" >/dev/null 2>&1; }
js() {
    local s=$1
    s=${s//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\n'/ }; s=${s//$'\t'/ }; s=${s//$'\r'/}
    printf '"%s"' "$s"
}

CHECKS=""
FAILED=0
check() { # name status detail
    CHECKS+="${CHECKS:+,}{\"name\":$(js "$1"),\"status\":$(js "$2"),\"detail\":$(js "$3")}"
    [ "$2" = fail ] && FAILED=1
    printf '%-5s %-26s %s\n' "$(tr '[:lower:]' '[:upper:]' <<<"$2")" "$1" "$3" >&2
}

GH=0
if has gh && gh auth status >/dev/null 2>&1; then
    GH=1
    [ -n "$REPO" ] || REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)
    [ -n "$HOST" ] || HOST=$(gh variable get VM_HOST --repo "$REPO" 2>/dev/null || true)
fi
if [ -z "$HOST" ] && [ -n "$VM" ]; then HOST=${VM#*@}; fi

# ---- tailnet and HTTP ----
if [ -z "$HOST" ]; then
    check tailnet skip "no VM host: pass --host, --vm, or set the VM_HOST variable"
elif has tailscale; then
    if tailscale ping -c 1 --timeout 5s "$HOST" >/dev/null 2>&1; then
        check tailnet pass "tailscale ping $HOST answered"
    else
        check tailnet fail "tailscale ping $HOST got no answer; is this machine on the tailnet?"
    fi
else
    check tailnet skip "tailscale CLI not on this machine"
fi

expected_staging=""; expected_prod=""
if [ "$GH" = 1 ] && [ -n "$REPO" ]; then
    expected_staging=$(gh api "repos/$REPO/commits/main" --jq .sha 2>/dev/null || true)
    # deploy-gate reports the release's git commit, so compare prod with the
    # commit the newest v* tag points at (the tags API lists newest first).
    expected_prod=$(gh api "repos/$REPO/tags?per_page=100" --jq '[.[] | select(.name | startswith("v")) | .commit.sha] | first // empty' 2>/dev/null || true)
fi

http_env() { # env port expected-prefix
    local env=$1 port=$2 want=$3 body version
    if [ -z "$HOST" ]; then check "$env-version" skip "no VM host"; return; fi
    if ! curl -fsS --max-time 5 "http://$HOST:$port/health" >/dev/null 2>&1; then
        check "$env-health" fail "http://$HOST:$port/health did not answer"
        return
    fi
    check "$env-health" pass "http://$HOST:$port/health"
    body=$(curl -fsS --max-time 5 "http://$HOST:$port/version" 2>/dev/null || true)
    version=$(sed -n 's/.*"version" *: *"\([^"]*\)".*/\1/p' <<<"$body")
    if [ -z "$version" ]; then
        check "$env-version" fail "/version returned '$body'"
    elif [ -z "$want" ]; then
        check "$env-version" warn "running $version; nothing to compare with (no gh, or no release yet)"
    elif [[ "$version" == "$want"* ]]; then
        check "$env-version" pass "running $version"
    else
        check "$env-version" warn "running $version, expected $want (a deploy may still be in progress)"
    fi
}
http_env staging "$PORT_STAGING" "$expected_staging"
if [ -z "$expected_prod" ] && [ "$GH" = 1 ]; then
    check prod-version skip "no v* release tag yet"
else
    http_env prod "$PORT_PROD" "$expected_prod"
fi

if [ -n "$HOST" ]; then
    if curl -fsS --max-time 5 "http://$HOST:$PORT_O2/healthz" >/dev/null 2>&1; then
        check openobserve pass "http://$HOST:$PORT_O2 answers"
    else
        check openobserve warn "http://$HOST:$PORT_O2/healthz did not answer"
    fi
fi

# ---- the VM, over ssh ----
# Prints KEY=VALUE lines. Uses sudo only if it needs no password.
read -r -d '' REMOTE <<'EOF' || true
s() { if sudo -n true 2>/dev/null; then sudo -n "$@"; else return 99; fi; }
echo "disk_free_mb=$(df -Pm /var/lib/athena 2>/dev/null | awk 'NR==2 {print $4}')"
for u in athena-serve@staging athena-serve@prod athena-telegram@staging athena-telegram@prod openobserve; do
    echo "unit_$u=$(systemctl is-active "$u" 2>/dev/null || true)"
done
for e in staging prod; do
    echo "telegram_${e}_enabled=$(systemctl is-enabled athena-telegram@$e 2>/dev/null || true)"
done
for e in staging prod; do
    echo "env_$e=$(stat -c '%a %U:%G' /etc/athena/$e.env 2>/dev/null || echo missing)"
    for k in OPENROUTER_API_KEY TELEGRAM_BOT_TOKEN OPEN_SANDBOX_URL OPEN_SANDBOX_API_KEY \
        ATHENA_TELEMETRY_DIR OTEL_EXPORTER_OTLP_ENDPOINT OTEL_EXPORTER_OTLP_HEADERS; do
        v=$(s grep -c "^$k=..*" /etc/athena/$e.env 2>/dev/null); rc=$?
        [ "$rc" = 99 ] && v=nosudo
        echo "secret_${e}_$k=${v:-0}"
    done
    # Still pointed at the retired collector?
    v=$(s grep -c "^OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318$" /etc/athena/$e.env 2>/dev/null)
    echo "legacy_otlp_$e=${v:-0}"
    v=$(s sh -c "ls /var/lib/athena/$e/telemetry/traces-*.jsonl 2>/dev/null | wc -l"); rc=$?
    [ "$rc" = 99 ] && v=nosudo
    echo "telemetry_$e=${v:-0}"
done
echo "gate=$( [ -x /opt/athena/bin/deploy-gate ] && echo yes || echo no)"
echo "listen_public=$(ss -Hltn 2>/dev/null | awk '{print $4}' | grep -E '^(0\.0\.0\.0|\*|\[::\]):(18080|18081|5080)$' | paste -sd, -)"
EOF

if [ -z "$VM" ]; then
    check vm skip "pass --vm USER@HOST for disk, units, env files and listen checks"
elif ! out=$(ssh -o BatchMode=yes -o ConnectTimeout=15 "$VM" bash -s <<<"$REMOTE" 2>/dev/null); then
    check vm fail "ssh $VM failed"
else
    get() { sed -n "s/^$1=//p" <<<"$out" | head -n1; }
    disk=$(get disk_free_mb)
    if [ -z "$disk" ]; then check disk skip "/var/lib/athena not found"
    elif [ "$disk" -ge 1024 ]; then check disk pass "${disk} MB free under /var/lib/athena"
    else check disk fail "${disk} MB free; deploy-gate refuses deploys under 1 GB"; fi

    for u in athena-serve@staging athena-serve@prod openobserve; do
        state=$(get "unit_$u")
        if [ "$state" = active ]; then check "unit $u" pass active
        else check "unit $u" fail "${state:-unknown}"; fi
    done
    for env in staging prod; do
        if [ "$(get "telegram_${env}_enabled")" = enabled ]; then
            state=$(get "unit_athena-telegram@$env")
            if [ "$state" = active ]; then check "telegram $env" pass active
            else check "telegram $env" fail "enabled but ${state:-unknown}"; fi
        else check "telegram $env" pass "no bot for $env"; fi
    done

    if [ "$(get gate)" = yes ]; then check deploy-gate pass installed
    else check deploy-gate fail "/opt/athena/bin/deploy-gate missing: setup-host.sh --gate-digest"; fi

    for e in staging prod; do
        mode=$(get "env_$e")
        if [ "$mode" = "640 root:athena" ]; then check "$e.env mode" pass "$mode"
        else check "$e.env mode" fail "${mode:-missing} (want 640 root:athena)"; fi
        k=$(get "secret_${e}_OPENROUTER_API_KEY")
        case "$k" in
            nosudo) check "$e secrets" skip "reading /etc/athena/$e.env needs sudo with a password; run doctor from a shell where sudo -n works" ;;
            0) check "$e secrets" fail "OPENROUTER_API_KEY is empty: sudoedit /etc/athena/$e.env" ;;
            *) check "$e secrets" pass "OPENROUTER_API_KEY present (value not read)" ;;
        esac
        if [ "$e" = prod ] && [ "$k" != nosudo ]; then
            if [ "$(get secret_prod_TELEGRAM_BOT_TOKEN)" != 0 ]; then check "prod telegram token" pass present
            else check "prod telegram token" fail "TELEGRAM_BOT_TOKEN is empty in prod.env"; fi
        fi
        if [ "$k" != nosudo ] && [ "$(get "secret_${e}_OPEN_SANDBOX_URL")" != 0 ]; then
            if [ "$(get "secret_${e}_OPEN_SANDBOX_API_KEY")" != 0 ]; then check "$e sandbox auth" pass "API key set"
            else check "$e sandbox auth" warn "OPEN_SANDBOX_URL set without OPEN_SANDBOX_API_KEY (sandbox server is open to the tailnet)"; fi
        fi
        if [ "$k" != nosudo ]; then
            files=$(get "telemetry_$e")
            if [ "$(get "secret_${e}_ATHENA_TELEMETRY_DIR")" = 0 ]; then
                check "$e telemetry files" warn "ATHENA_TELEMETRY_DIR is not set: no JSONL for scripts/analytics.sh"
            elif [ "${files:-0}" = 0 ]; then
                check "$e telemetry files" warn "no traces-*.jsonl in /var/lib/athena/$e/telemetry yet (written once a request is served)"
            else
                check "$e telemetry files" pass "$files daily trace file(s)"
            fi
            if [ "$(get "legacy_otlp_$e")" != 0 ]; then
                check "$e OTLP" fail "OTEL_EXPORTER_OTLP_ENDPOINT still points at the retired collector (127.0.0.1:4318): re-run setup-host.sh"
            elif [ "$(get "secret_${e}_OTEL_EXPORTER_OTLP_ENDPOINT")" = 0 ]; then
                check "$e OTLP" skip "OTEL_EXPORTER_OTLP_ENDPOINT is not set: no export to OpenObserve"
            elif [ "$(get "secret_${e}_OTEL_EXPORTER_OTLP_HEADERS")" = 0 ]; then
                check "$e OTLP" warn "OTEL_EXPORTER_OTLP_HEADERS is not set: OpenObserve will refuse the export"
            else
                check "$e OTLP" pass "OpenObserve endpoint and auth header set (values not read)"
            fi
        fi
    done

    public=$(get listen_public)
    if [ -z "$public" ]; then check "listen addresses" pass "no Athena port on 0.0.0.0"
    else check "listen addresses" fail "listening on all interfaces: $public"; fi
fi

# ---- GitHub ----
if [ "$GH" = 0 ] || [ -z "$REPO" ]; then
    check github skip "gh is missing or not authenticated"
else
    # gh prints the error body on stdout, so only trust output on success.
    # Prod deploys from v* tags only; the tag ruleset decides who may tag.
    prod_tags=""
    prod_tags=$(gh api "repos/$REPO/environments/prod/deployment-branch-policies" \
        --jq '[.branch_policies[] | select(.type == "tag") | .name] | join(",")' 2>/dev/null || true)
    if [ "$prod_tags" = "v*" ]; then check "prod environment" pass "deploys from v* tags only"
    else check "prod environment" fail "environment prod missing or not limited to v* tags: init-github.sh"; fi
    if gh api "repos/$REPO/environments/staging" >/dev/null 2>&1; then check "staging environment" pass present
    else check "staging environment" fail "missing: init-github.sh"; fi
    ruleset=0
    if rs_json=$(gh api "repos/$REPO/rulesets" 2>/dev/null); then
        ruleset=$(jq '[.[] | select(.target == "tag")] | length' <<<"$rs_json")
    fi
    if [ "${ruleset:-0}" -gt 0 ]; then check "v* tag ruleset" pass "$ruleset tag ruleset(s)"
    else check "v* tag ruleset" fail "no tag ruleset: init-github.sh"; fi
    secrets=$(gh secret list --repo "$REPO" --json name --jq '.[].name' 2>/dev/null || true)
    if grep -qx TS_AUTH_KEY <<<"$secrets"; then check "tailscale credential" pass "TS_AUTH_KEY (expires; rotate)"
    elif grep -qx TS_OAUTH_CLIENT_ID <<<"$secrets" && grep -qx TS_OAUTH_SECRET <<<"$secrets"; then
        check "tailscale credential" pass "OAuth client"
    else check "tailscale credential" fail "missing: gh secret set TS_AUTH_KEY"; fi
    if gh variable get VM_KNOWN_HOSTS --repo "$REPO" >/dev/null 2>&1; then check "variable VM_KNOWN_HOSTS" pass set
    else check "variable VM_KNOWN_HOSTS" fail "missing: init-github.sh --vm-host"; fi

    # GHCR: can an anonymous client pull? (packages start private)
    pkg=$(tr '[:upper:]' '[:lower:]' <<<"$REPO")
    token=$(curl -fsS "https://ghcr.io/token?scope=repository:$pkg:pull" 2>/dev/null | sed -n 's/.*"token":"\([^"]*\)".*/\1/p' || true)
    code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${token:-none}" "https://ghcr.io/v2/$pkg/tags/list" || true)
    if [ "$code" = 200 ]; then check "ghcr package public" pass "ghcr.io/$pkg pulls anonymously"
    else check "ghcr package public" fail "anonymous pull of ghcr.io/$pkg got HTTP $code: make the package public (SETUP.md)"; fi
fi

# ---- things only the admin console can show ----
check "tailnet ACLs" skip "verify in the Tailscale admin console: tag:ci reaches only :22 on the VM; tag:sandbox reaches nothing"

ok=true; [ "$FAILED" = 0 ] || ok=false
printf '{"ok":%s,"checks":[%s]}\n' "$ok" "$CHECKS"
[ "$FAILED" = 0 ]
