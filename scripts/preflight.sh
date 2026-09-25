#!/usr/bin/env bash
# Read-only report on whether a machine can host Athena. Changes nothing.
#
#   ./scripts/preflight.sh                      # this machine
#   ./scripts/preflight.sh --vm appuser@host    # the VM, over ssh, plus local tools
#
# Prints one JSON object on stdout. "conflicts" are blockers that
# setup-host.sh would refuse on; "warnings" are worth reading but not fatal.
# Exit status is 0 even with conflicts: the report is the result.
set -euo pipefail

VM=""
PORTS=(18080 18081 5080 4317 4318)
while [ $# -gt 0 ]; do
    case "$1" in
        --vm) VM="${2:?--vm needs USER@HOST}"; shift ;;
        -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "preflight: unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# JSON string, escaped.
js() {
    local s=$1
    s=${s//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\n'/\\n}; s=${s//$'\t'/\\t}; s=${s//$'\r'/}
    printf '"%s"' "$s"
}
# JSON string, or null when empty.
jsn() { if [ -n "$1" ]; then js "$1"; else printf 'null'; fi; }
jbool() { if [ "$1" = 1 ]; then printf 'true'; else printf 'false'; fi; }
has() { command -v "$1" >/dev/null 2>&1; }

if [ -n "$VM" ]; then
    remote=$(ssh -o BatchMode=yes -o ConnectTimeout=15 "$VM" 'bash -s' <"$0") || {
        printf '{"vm":null,"error":%s}\n' "$(js "ssh $VM failed; check the address and that your key is loaded")"
        exit 0
    }
    gh_state="missing"
    if has gh; then
        gh_user=$(gh api user --jq .login 2>/dev/null || true)
        gh_state=${gh_user:+"authenticated as $gh_user"}
        gh_state=${gh_state:-"installed, not authenticated (gh auth login)"}
    fi
    printf '{"vm":%s,"local":{"gh":%s,"oras":%s,"ssh_to_vm":true}}\n' "$remote" "$(js "$gh_state")" \
        "$(jsn "$(oras version 2>/dev/null | sed -n 's/^Version: *//p' || true)")"
    exit 0
fi

WARNINGS=()
CONFLICTS=()

# ---- OS ----
os_id=""; os_version=""
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    os_id=$(. /etc/os-release && echo "${ID:-}")
    # shellcheck disable=SC1091
    os_version=$(. /etc/os-release && echo "${VERSION_ID:-}")
fi
arch=$(uname -m)
kernel=$(uname -s)
[ "$kernel" = Linux ] || CONFLICTS+=("not Linux ($kernel)")
[ "$arch" = x86_64 ] || CONFLICTS+=("architecture $arch; the release artifact is x86_64 only")
systemd=0; [ -d /run/systemd/system ] && systemd=1
[ "$systemd" = 1 ] || CONFLICTS+=("systemd is not running")

# ---- memory and disk ----
mem_total_mb=""; mem_avail_mb=""
if [ -r /proc/meminfo ]; then
    mem_total_mb=$(awk '/^MemTotal:/ {print int($2/1024)}' /proc/meminfo)
    mem_avail_mb=$(awk '/^MemAvailable:/ {print int($2/1024)}' /proc/meminfo)
    # MemoryMax caps: athena-serve x2 + athena-telegram@prod + OpenObserve at
    # 512M each, otelcol-contrib at 256M.
    [ "${mem_avail_mb:-0}" -ge 2304 ] ||
        WARNINGS+=("only ${mem_avail_mb} MB available; Athena's services can use up to 2304 MB at their MemoryMax caps (serve x2, telegram and OpenObserve at 512M, otelcol at 256M); if the host runs out, the kernel OOM-kills its largest process, which may be another app")
fi
disk_free_mb=$(df -Pm / 2>/dev/null | awk 'NR==2 {print $4}')
[ "${disk_free_mb:-0}" -ge 2048 ] || WARNINGS+=("under 2 GB free on /; deploy-gate refuses deploys under 1 GB")

# ---- Tailscale ----
ts_installed=0; ts_running=0; ts_ip=""; ts_tags=""; ts_tagged="null"
if has tailscale; then
    ts_installed=1
    ts_ip=$(tailscale ip -4 2>/dev/null | head -n1 || true)
    [ -n "$ts_ip" ] && ts_running=1
    if [ "$ts_running" = 1 ] && has python3; then
        ts_tags=$(tailscale status --json 2>/dev/null | python3 -c \
            'import json,sys; print(",".join(json.load(sys.stdin).get("Self",{}).get("Tags") or []))' 2>/dev/null || true)
        if [ -n "$ts_tags" ]; then ts_tagged=true; else ts_tagged=false; fi
    fi
fi
[ "$ts_installed" = 1 ] || CONFLICTS+=("Tailscale is not installed; install it and log in (setup-host.sh never changes Tailscale)")
[ "$ts_installed" = 0 ] || [ "$ts_running" = 1 ] || CONFLICTS+=("Tailscale is installed but has no IPv4; run 'tailscale up'")
[ "$ts_tagged" = false ] &&
    WARNINGS+=("node is user-owned: in deploy/tailscale-policy.hujson use the hosts alias \"athena-vm\": \"$ts_ip\"; do not re-tag it")

# ---- ports ----
ports_json=""
for port in "${PORTS[@]}"; do
    state="free"
    if has ss; then
        line=$(ss -Hltn "sport = :$port" 2>/dev/null | awk '{print $4}' | paste -sd, - || true)
        if [ -n "$line" ]; then
            owner=$(ss -Hltnp "sport = :$port" 2>/dev/null | grep -o 'users:(("[^"]*"' | head -n1 | cut -d'"' -f2 || true)
            state="in use on $line${owner:+ by $owner}"
            case "$owner" in
                athena|openobserve|otelcol-contrib) state="$state (Athena's own)" ;;
                *) CONFLICTS+=("port $port is $state") ;;
            esac
        fi
    else
        state="unknown (no ss)"
    fi
    ports_json+="${ports_json:+,}$(js "$port"):$(js "$state")"
done

# ---- Docker (reported, never touched) ----
docker_installed=0; docker_public="null"
if has docker; then
    docker_installed=1
    if published=$(docker ps --format '{{.Names}} {{.Ports}}' 2>/dev/null); then
        public=$(grep -E '0\.0\.0\.0:|\[::\]:' <<<"$published" | awk '{print $1}' | paste -sd, - || true)
        docker_public=$(jsn "$public")
        [ -n "$public" ] &&
            WARNINGS+=("Docker containers publish ports on all interfaces ($public); outside Athena's scope, but check your cloud firewall")
    else
        docker_public=$(js "unknown: no permission to query Docker")
    fi
fi

# ---- Caddy ----
caddy_installed=0; caddy_active=0
has caddy && caddy_installed=1
systemctl is-active --quiet caddy 2>/dev/null && caddy_active=1

# ---- existing Athena and tools ----
opt_athena=0; [ -d /opt/athena ] && opt_athena=1
gate=0; [ -x /opt/athena/bin/deploy-gate ] && gate=1
units=0; [ -f /etc/systemd/system/athena-serve@.service ] && units=1
env_state() {
    local f=/etc/athena/$1.env
    if [ -e "$f" ]; then stat -c 'present %a %U:%G' "$f" 2>/dev/null || echo present; else echo missing; fi
}
oras_v=$( (oras version 2>/dev/null || /usr/local/bin/oras version 2>/dev/null) | sed -n 's/^Version: *//p' | head -n1 || true)
otel_v=$(dpkg-query -W -f='${Version}' otelcol-contrib 2>/dev/null || true)
o2_v=$( (/usr/local/bin/openobserve --version 2>/dev/null || true) | head -n1)
duck_v=$( (duckdb --version 2>/dev/null || /usr/local/bin/duckdb --version 2>/dev/null || true) | awk '{print $1}' | head -n1)

list_json() {
    local out="" item
    for item in "$@"; do out+="${out:+,}$(js "$item")"; done
    printf '[%s]' "$out"
}

ok=true; [ ${#CONFLICTS[@]} -eq 0 ] || ok=false
cat <<EOF
{"ok":$ok,
 "host":$(js "$(hostname 2>/dev/null || echo unknown)"),
 "os":{"id":$(jsn "$os_id"),"version":$(jsn "$os_version"),"kernel":$(js "$kernel"),"arch":$(js "$arch")},
 "systemd":$(jbool "$systemd"),
 "memory_mb":{"total":${mem_total_mb:-null},"available":${mem_avail_mb:-null}},
 "disk_free_mb":${disk_free_mb:-null},
 "tailscale":{"installed":$(jbool "$ts_installed"),"running":$(jbool "$ts_running"),"ip":$(jsn "$ts_ip"),"tagged":$ts_tagged,"tags":$(jsn "$ts_tags")},
 "ports":{$ports_json},
 "docker":{"installed":$(jbool "$docker_installed"),"publicly_published":$docker_public},
 "caddy":{"installed":$(jbool "$caddy_installed"),"active":$(jbool "$caddy_active")},
 "athena":{"opt_athena":$(jbool "$opt_athena"),"units_installed":$(jbool "$units"),"deploy_gate":$(jbool "$gate"),"staging_env":$(js "$(env_state staging)"),"prod_env":$(js "$(env_state prod)")},
 "tools":{"oras":$(jsn "$oras_v"),"otelcol_contrib":$(jsn "$otel_v"),"openobserve":$(jsn "$o2_v"),"duckdb":$(jsn "$duck_v")},
 "conflicts":$(list_json ${CONFLICTS[@]+"${CONFLICTS[@]}"}),
 "warnings":$(list_json ${WARNINGS[@]+"${WARNINGS[@]}"})}
EOF
