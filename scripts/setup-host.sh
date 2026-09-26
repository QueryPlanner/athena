#!/usr/bin/env bash
# Prepare a VM to run Athena: users, directories, systemd units, the
# deploy-gate forced command, and the observability stack.
#
# Safe on a VM that already runs other things. It never touches Docker, ufw,
# sshd, Caddy or the Tailscale configuration, and it never overwrites a file
# that holds secrets. Every step checks before it changes, so re-running it is
# safe; --dry-run prints what would change and needs no root.
#
#   sudo ./scripts/setup-host.sh --dry-run \
#       --ci-staging-pubkey ci-staging.pub --ci-prod-pubkey ci-prod.pub \
#       --gate-digest sha256:<64 hex>
#   ./scripts/setup-host.sh --vm appuser@100.124.202.79 [same flags]
#   sudo ./scripts/setup-host.sh --uninstall [--purge]
#
# See SETUP.md for where this fits, and plans/contracts.md for the layout.
set -euo pipefail

# ---- pinned versions (bump deliberately, with the new checksum) ----
# sha256 values are from the GitHub release API "digest" field, or the
# vendor's .sha256sum file for OpenObserve (checked 2026-09-25).
ORAS_VERSION=1.3.4
ORAS_SHA256=f27adb935022d94df8dc77719c322dda592c78a0d57a6f7dcdd8d900b248c454
ORAS_URL="https://github.com/oras-project/oras/releases/download/v${ORAS_VERSION}/oras_${ORAS_VERSION}_linux_amd64.tar.gz"
OPENOBSERVE_VERSION=v1.0.4
OPENOBSERVE_SHA256=5c1b18bc072658c045ff32ca7dd0d2e3f22fac1209755d7a0ef5fd6448896e61
OPENOBSERVE_URL="https://downloads.openobserve.ai/releases/openobserve/${OPENOBSERVE_VERSION}/openobserve-${OPENOBSERVE_VERSION}-linux-amd64.tar.gz"
DUCKDB_VERSION=1.5.5
DUCKDB_SHA256=c61f21485e6e41d3a0c28ce9904ea18346309cf427b4cf9479bc3564348dc885
DUCKDB_URL="https://github.com/duckdb/duckdb/releases/download/v${DUCKDB_VERSION}/duckdb_cli-linux-amd64.gz"

# ---- defaults ----
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ATHENA_REPO=ghcr.io/queryplanner/athena
PORT_PROD=18080
PORT_STAGING=18081
PORT_O2=5080
HOST_ALIAS="athena-vm"
O2_EMAIL=admin@athena.internal
DRY_RUN=0
UNINSTALL=0
PURGE=0
ASSUME_YES=0
GATE_DIGEST=""
STAGING_PUBKEY=""
PROD_PUBKEY=""
TAILNET_IP=""
VM=""
SKIP_OBSERVABILITY=0
# Stamps recording what this script installed, so --uninstall removes only
# its own things. Kept outside /opt/athena so they survive a plain uninstall.
STAMPS=/var/lib/athena-setup

usage() {
    cat <<'EOF'
Usage: setup-host.sh [options]

  --dry-run                  print every change without making it (no root needed)
  --vm USER@HOST             copy this script and deploy/ to HOST and run it there
                             (with sudo, over ssh -t; the dry run runs without sudo)
  --gate-digest sha256:HEX   install deploy-gate from this release artifact digest
  --ci-staging-pubkey FILE   public key CI uses for staging (forced command)
  --ci-prod-pubkey FILE      public key CI uses for prod (forced command)
  --tailnet-ip IP            override `tailscale ip -4` (used by CI's dry run)
  --port-prod N              prod serve port (default 18080)
  --port-staging N           staging serve port (default 18081)
  --host-alias NAME          extra Host name accepted by Athena (default athena-vm)
  --o2-email EMAIL           OpenObserve root user (default admin@athena.internal)
  --repo REF                 release repository (default ghcr.io/queryplanner/athena)
  --skip-observability       do not install OpenObserve (Athena still writes JSONL)
  --uninstall                stop and remove Athena's units, binaries and rules;
                             keeps /etc/athena and the databases
  --purge                    with --uninstall: also delete /etc/athena,
                             /var/lib/athena and what this script installed
  --yes                      do not ask for confirmation on --uninstall
  -h, --help                 this help
EOF
}

die() { echo "setup-host: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --vm) VM="${2:?--vm needs USER@HOST}"; shift ;;
        --gate-digest) GATE_DIGEST="${2:?}"; shift ;;
        --ci-staging-pubkey) STAGING_PUBKEY="${2:?}"; shift ;;
        --ci-prod-pubkey) PROD_PUBKEY="${2:?}"; shift ;;
        --tailnet-ip) TAILNET_IP="${2:?}"; shift ;;
        --port-prod) PORT_PROD="${2:?}"; shift ;;
        --port-staging) PORT_STAGING="${2:?}"; shift ;;
        --host-alias) HOST_ALIAS="${2:?}"; shift ;;
        --o2-email) O2_EMAIL="${2:?}"; shift ;;
        --repo) ATHENA_REPO="${2:?}"; shift ;;
        --skip-observability) SKIP_OBSERVABILITY=1 ;;
        --uninstall) UNINSTALL=1 ;;
        --purge) PURGE=1 ;;
        --yes) ASSUME_YES=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

[ "$PURGE" = 1 ] && [ "$UNINSTALL" = 0 ] && die "--purge only makes sense with --uninstall"
if [ -n "$GATE_DIGEST" ] && ! [[ "$GATE_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    die "--gate-digest must look like sha256:<64 lowercase hex>, got '$GATE_DIGEST'"
fi
for p in "$PORT_PROD" "$PORT_STAGING"; do
    [[ "$p" =~ ^[0-9]+$ ]] || die "ports must be numbers, got '$p'"
done

# ---- remote mode: ship this script plus deploy/ and run it on the VM ----
if [ -n "$VM" ]; then
    stage=$(mktemp -d)
    trap 'rm -rf "$stage"' EXIT
    mkdir -p "$stage/scripts" "$stage/keys"
    cp "$REPO_ROOT/scripts/setup-host.sh" "$stage/scripts/"
    cp -R "$REPO_ROOT/deploy" "$stage/"
    fwd=()
    [ "$DRY_RUN" = 1 ] && fwd+=(--dry-run)
    [ "$UNINSTALL" = 1 ] && fwd+=(--uninstall)
    [ "$PURGE" = 1 ] && fwd+=(--purge)
    [ "$ASSUME_YES" = 1 ] && fwd+=(--yes)
    [ "$SKIP_OBSERVABILITY" = 1 ] && fwd+=(--skip-observability)
    [ -n "$GATE_DIGEST" ] && fwd+=(--gate-digest "$GATE_DIGEST")
    [ -n "$TAILNET_IP" ] && fwd+=(--tailnet-ip "$TAILNET_IP")
    fwd+=(--port-prod "$PORT_PROD" --port-staging "$PORT_STAGING" --host-alias "$HOST_ALIAS"
          --o2-email "$O2_EMAIL" --repo "$ATHENA_REPO")
    if [ -n "$STAGING_PUBKEY" ]; then
        cp "$STAGING_PUBKEY" "$stage/keys/ci-staging.pub"; fwd+=(--ci-staging-pubkey keys/ci-staging.pub)
    fi
    if [ -n "$PROD_PUBKEY" ]; then
        cp "$PROD_PUBKEY" "$stage/keys/ci-prod.pub"; fwd+=(--ci-prod-pubkey keys/ci-prod.pub)
    fi
    remote_dir=$(tar -C "$stage" -czf - . | ssh "$VM" 'd=$(mktemp -d) && tar -xzf - -C "$d" && echo "$d"')
    quoted=$(printf ' %q' "${fwd[@]}")
    sudo_cmd="sudo"
    [ "$DRY_RUN" = 1 ] && sudo_cmd=""
    status=0
    # shellcheck disable=SC2029  # expanded locally on purpose
    ssh -t "$VM" "cd $remote_dir && $sudo_cmd bash scripts/setup-host.sh$quoted" || status=$?
    # shellcheck disable=SC2029
    ssh "$VM" "rm -rf $remote_dir" || true
    exit "$status"
fi

# ---- bookkeeping ----
CHANGED=()
UNCHANGED=()
TODO=()
WARNINGS=()
changed() {
    local msg=$1
    [ "$DRY_RUN" = 1 ] && [[ "$msg" != would* ]] && msg="would: $msg"
    CHANGED+=("$msg"); echo "  + $msg" >&2
}
same() { UNCHANGED+=("$1"); }
todo() { TODO+=("$1"); }
warn() { WARNINGS+=("$1"); echo "  ! $1" >&2; }
step() { echo "== $1" >&2; }

# Run a command, or print it on a dry run.
run() {
    if [ "$DRY_RUN" = 1 ]; then
        printf '  would run:' >&2; printf ' %q' "$@" >&2; echo >&2
    else
        "$@"
    fi
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Install file SRC at DEST with MODE and OWNER:GROUP, if it differs.
# Returns 0 when it changed (or would change), 1 when already identical.
install_file() {
    local src=$1 dest=$2 mode=$3 owner=$4
    if [ -f "$dest" ] && cmp -s "$src" "$dest"; then
        fix_mode "$dest" "$mode" "$owner"
        same "$dest"
        return 1
    fi
    if [ "$DRY_RUN" = 1 ]; then
        changed "would write $dest ($mode $owner)"
    else
        install -D -m "$mode" -o "${owner%%:*}" -g "${owner##*:}" "$src" "$dest"
        changed "wrote $dest"
    fi
    return 0
}

# Enforce mode and owner on an existing path; never touches its content.
fix_mode() {
    local path=$1 mode=$2 owner=$3 have
    [ -e "$path" ] || return 0
    have=$(stat -c '%a %U:%G' "$path" 2>/dev/null || echo "?")
    [ "$have" = "${mode#0} $owner" ] && return 0
    if [ "$DRY_RUN" = 1 ]; then
        changed "would set $path to $mode $owner (is $have)"
    else
        chmod "$mode" "$path" && chown "$owner" "$path"
        changed "set $path to $mode $owner (was $have)"
    fi
}

ensure_dir() {
    local path=$1 mode=$2 owner=$3
    if [ -d "$path" ]; then
        fix_mode "$path" "$mode" "$owner"
        same "$path/"
    elif [ "$DRY_RUN" = 1 ]; then
        changed "would create $path/ ($mode $owner)"
    else
        install -d -m "$mode" -o "${owner%%:*}" -g "${owner##*:}" "$path"
        changed "created $path/"
    fi
}

# Owner for paths whose user may not exist yet on a dry run.
# On a dry run, report the owner a real run would set.
owner_or_root() {
    if id "$1" >/dev/null 2>&1 || [ "$DRY_RUN" = 1 ]; then echo "$1:$1"; else echo "root:root"; fi
}

stamp_get() { cat "$STAMPS/$1" 2>/dev/null || true; }
stamp_set() {
    [ "$DRY_RUN" = 1 ] && return 0
    install -d -m 0755 "$STAMPS"
    printf '%s\n' "$2" >"$STAMPS/$1"
}

# Download URL to DEST and check its sha256.
fetch() {
    local url=$1 sha=$2 dest=$3
    curl -fsSL --retry 3 -o "$dest" "$url"
    echo "$sha  $dest" | sha256sum -c --quiet - || die "checksum mismatch for $url"
}

# Replace @KEY@ placeholders in a template. Values may span lines.
render() {
    local content
    content=$(cat "$1"); shift
    while [ $# -gt 1 ]; do
        # Quoted: bash 5.2 would otherwise read "&" in a value as the match.
        content=${content//@$1@/"$2"}
        shift 2
    done
    printf '%s\n' "$content"
}

# 28 random base64 letters and digits, then "-Aa0": OpenObserve rejects a
# root password without a lowercase, uppercase, digit and special character.
random_secret() { printf '%s-Aa0' "$(head -c 48 /dev/urandom | base64 | tr -d '/+=\n' | head -c 28)"; }

confirm() {
    [ "$ASSUME_YES" = 1 ] && return 0
    [ "$DRY_RUN" = 1 ] && return 0
    printf '%s Type "yes" to continue: ' "$1" >&2
    local answer
    read -r answer
    [ "$answer" = "yes" ] || die "aborted"
}

# ---- preconditions ----
preconditions() {
    step "preconditions"
    if [ "$DRY_RUN" = 0 ]; then
        [ "$(id -u)" = 0 ] || die "run as root (sudo), or use --dry-run"
        [ "$(uname -s)" = Linux ] || die "Linux only"
        [ "$(uname -m)" = x86_64 ] || die "x86_64 only; the release artifact is x86_64-unknown-linux-gnu"
        [ -d /run/systemd/system ] || die "systemd is not running"
    else
        echo "  dry run: nothing will change" >&2
    fi
}

detect_tailnet_ip() {
    if [ -z "$TAILNET_IP" ] && command -v tailscale >/dev/null 2>&1; then
        TAILNET_IP=$(tailscale ip -4 2>/dev/null | head -n1 || true)
    fi
    if [ -z "$TAILNET_IP" ]; then
        [ "$DRY_RUN" = 1 ] || die "no tailnet IP: install and log in to Tailscale first (setup never changes Tailscale), or pass --tailnet-ip"
        TAILNET_IP="<tailnet-ip>"
        warn "no tailnet IP found; using the placeholder $TAILNET_IP for this dry run"
    fi
    echo "  tailnet IP: $TAILNET_IP" >&2
}

# Refuse when another program already listens on one of our ports.
check_ports() {
    step "ports"
    command -v ss >/dev/null 2>&1 || { warn "ss not found; port check skipped"; return 0; }
    local port line conflict=0
    for port in "$PORT_PROD" "$PORT_STAGING" "$PORT_O2"; do
        line=$(ss -Hltnp "sport = :$port" 2>/dev/null || true)
        [ -n "$line" ] || continue
        if grep -Eq '"(athena|openobserve)"' <<<"$line"; then
            same "port $port (already ours)"
        elif ! grep -q 'users:' <<<"$line"; then
            warn "port $port is in use; run as root to see by whom"
        else
            echo "  port $port is used by another program: $line" >&2
            conflict=1
        fi
    done
    if [ "$conflict" = 1 ]; then
        [ "$DRY_RUN" = 1 ] && { warn "port conflict; a real run would stop here"; return 0; }
        die "port conflict. Pick free ports, e.g. --port-prod 18090 --port-staging 18091"
    fi
}

ensure_users() {
    step "users"
    if id athena >/dev/null 2>&1; then same "user athena"; else
        run useradd --system --user-group --home-dir /var/lib/athena --no-create-home \
            --shell /usr/sbin/nologin athena
        changed "user athena (runs the services)"
    fi
    if id deploy >/dev/null 2>&1; then same "user deploy"; else
        run useradd --system --user-group --create-home --home-dir /home/deploy \
            --shell /bin/bash deploy
        changed "user deploy (forced-command SSH only)"
    fi
    if [ "$SKIP_OBSERVABILITY" = 0 ]; then
        if id openobserve >/dev/null 2>&1; then same "user openobserve"; else
            run useradd --system --user-group --home-dir /var/lib/openobserve --no-create-home \
                --shell /usr/sbin/nologin openobserve
            changed "user openobserve"
        fi
    fi
}

ensure_dirs() {
    step "directories"
    local athena env
    athena=$(owner_or_root athena)
    ensure_dir /opt/athena 0755 root:root
    ensure_dir /opt/athena/bin 0755 root:root
    ensure_dir /opt/athena/releases 0755 root:root
    ensure_dir /etc/athena 0755 root:root
    ensure_dir /var/lib/athena 0755 root:root
    for env in staging prod; do
        ensure_dir "/opt/athena/$env" 0755 root:root
        ensure_dir "/var/lib/athena/$env" 0750 "$athena"
        ensure_dir "/var/lib/athena/$env/backups" 0750 "$athena"
        # ATHENA_TELEMETRY_DIR: Athena's own JSONL spans and logs.
        ensure_dir "/var/lib/athena/$env/telemetry" 0750 "$athena"
    done
}

install_oras() {
    step "oras $ORAS_VERSION"
    if [ -x /usr/local/bin/oras ] && /usr/local/bin/oras version 2>/dev/null | grep -q "Version: *$ORAS_VERSION"; then
        same "oras $ORAS_VERSION"; return 0
    fi
    if [ "$DRY_RUN" = 1 ]; then changed "would install oras $ORAS_VERSION to /usr/local/bin/oras"; return 0; fi
    fetch "$ORAS_URL" "$ORAS_SHA256" "$WORK/oras.tgz"
    tar -xzf "$WORK/oras.tgz" -C "$WORK" oras
    # Stamp only what was not there before: --purge removes stamped binaries.
    [ -e /usr/local/bin/oras ] || stamp_set oras "$ORAS_VERSION"
    install -m 0755 -o root -g root "$WORK/oras" /usr/local/bin/oras
    changed "installed oras $ORAS_VERSION"
}

install_duckdb() {
    step "duckdb $DUCKDB_VERSION"
    if [ -x /usr/local/bin/duckdb ] && /usr/local/bin/duckdb --version 2>/dev/null | grep -q "v$DUCKDB_VERSION"; then
        same "duckdb $DUCKDB_VERSION"; return 0
    fi
    if [ "$DRY_RUN" = 1 ]; then changed "would install duckdb $DUCKDB_VERSION to /usr/local/bin/duckdb"; return 0; fi
    fetch "$DUCKDB_URL" "$DUCKDB_SHA256" "$WORK/duckdb.gz"
    gunzip -f "$WORK/duckdb.gz"
    [ -e /usr/local/bin/duckdb ] || stamp_set duckdb "$DUCKDB_VERSION"
    install -m 0755 -o root -g root "$WORK/duckdb" /usr/local/bin/duckdb
    changed "installed duckdb $DUCKDB_VERSION"
}

# Earlier versions of this script ran an OpenTelemetry Collector between
# Athena and OpenObserve. Athena now exports to both itself, so a collector
# this script set up is stopped, and purged if this script installed it.
# Its old JSONL files in /var/lib/athena/otel are left for a human.
retire_otelcol() {
    local dropin=/etc/systemd/system/otelcol-contrib.service.d/athena.conf
    [ -e "$dropin" ] || [ -n "$(stamp_get otelcol-contrib)" ] || return 0
    step "retire otelcol-contrib (replaced by Athena's own exporters)"
    run systemctl disable --now otelcol-contrib.service >/dev/null 2>&1 || true
    changed "stopped and disabled otelcol-contrib"
    if [ -e "$dropin" ]; then
        run rm -f "$dropin" /etc/otelcol-contrib/openobserve.env
        changed "removed the otelcol-contrib drop-in and its OpenObserve env file"
    fi
    if [ -n "$(stamp_get otelcol-contrib)" ]; then
        run dpkg --purge otelcol-contrib >/dev/null || warn "dpkg --purge otelcol-contrib failed; remove it by hand"
        run rm -f "$STAMPS/otelcol-contrib"
        changed "purged otelcol-contrib (this script installed it)"
    fi
    [ -d /var/lib/athena/otel ] &&
        todo "the collector's old JSONL files are still in /var/lib/athena/otel; delete them when no longer needed"
    return 0
}

# OpenObserve root credentials: generated once into a root-only file, never
# printed, never overwritten. Athena's env files get a derived basic-auth
# header (see otlp_block).
install_openobserve() {
    step "OpenObserve $OPENOBSERVE_VERSION"
    local restart=0
    if [ -x /usr/local/bin/openobserve ] && [ "$(stamp_get openobserve)" = "$OPENOBSERVE_VERSION" ]; then
        same "openobserve $OPENOBSERVE_VERSION"
    elif [ "$DRY_RUN" = 1 ]; then
        changed "would install openobserve $OPENOBSERVE_VERSION to /usr/local/bin/openobserve"
    else
        fetch "$OPENOBSERVE_URL" "$OPENOBSERVE_SHA256" "$WORK/o2.tgz"
        tar -xzf "$WORK/o2.tgz" -C "$WORK" openobserve
        install -m 0755 -o root -g root "$WORK/openobserve" /usr/local/bin/openobserve
        stamp_set openobserve "$OPENOBSERVE_VERSION"
        changed "installed openobserve $OPENOBSERVE_VERSION"
        restart=1
    fi
    ensure_dir /var/lib/openobserve 0750 "$(owner_or_root openobserve)"
    ensure_dir /etc/openobserve 0700 root:root

    local o2env=/etc/openobserve/openobserve.env
    if [ -f "$o2env" ]; then
        fix_mode "$o2env" 0600 root:root
        same "$o2env (kept; holds the root password)"
        if [ -r "$o2env" ] && ! grep -q "^ZO_HTTP_ADDR=$TAILNET_IP\$" "$o2env"; then
            warn "$o2env: ZO_HTTP_ADDR is not $TAILNET_IP; edit it with sudoedit"
        fi
    elif [ "$DRY_RUN" = 1 ]; then
        changed "would create $o2env (0600 root) with a random root password for $O2_EMAIL"
    else
        render "$REPO_ROOT/deploy/openobserve/openobserve.env.template" \
            ROOT_EMAIL "$O2_EMAIL" ROOT_PASSWORD "$(random_secret)" TAILNET_IP "$TAILNET_IP" \
            >"$WORK/o2.env"
        install -m 0600 -o root -g root "$WORK/o2.env" "$o2env"
        changed "created $o2env with a generated root password"
        todo "OpenObserve login: user $O2_EMAIL, password: sudo grep ZO_ROOT_USER_PASSWORD $o2env (UI: http://$TAILNET_IP:$PORT_O2)"
        restart=1
    fi

    install_file "$REPO_ROOT/deploy/systemd/openobserve.service" \
        /etc/systemd/system/openobserve.service 0644 root:root && restart=1
    O2_RESTART=$restart
}

install_units() {
    step "systemd units"
    local unit
    for unit in athena-serve@.service athena-telegram@.service athena@.target; do
        install_file "$REPO_ROOT/deploy/systemd/$unit" "/etc/systemd/system/$unit" 0644 root:root || true
    done
}

# The OTLP lines of an env file: OpenObserve's ingest endpoint and a basic
# auth header derived from its root credentials, or a note that there is no
# OTLP export. On a dry run the header is a placeholder, so the password is
# never printed.
otlp_block() {
    if [ "$SKIP_OBSERVABILITY" = 1 ]; then
        echo "# No OpenObserve (setup-host.sh --skip-observability), so no OTLP export."
        return 0
    fi
    local o2env=/etc/openobserve/openobserve.env auth
    if [ "$DRY_RUN" = 1 ]; then
        auth="<base64 of the OpenObserve root user:password, from $o2env>"
    else
        local email pass
        email=$(sed -n 's/^ZO_ROOT_USER_EMAIL=//p' "$o2env")
        pass=$(sed -n 's/^ZO_ROOT_USER_PASSWORD=//p' "$o2env")
        if [ -z "$email" ] || [ -z "$pass" ]; then die "$o2env has no root user or password"; fi
        auth=$(printf '%s:%s' "$email" "$pass" | base64 -w0)
    fi
    echo "# OpenObserve's OTLP/HTTP ingest (org \"default\"). The header is basic auth"
    echo "# for its root user, written by setup-host.sh from $o2env;"
    echo "# if that password changes, update it here too."
    echo "OTEL_EXPORTER_OTLP_ENDPOINT=http://$TAILNET_IP:$PORT_O2/api/default"
    echo "OTEL_EXPORTER_OTLP_HEADERS=Authorization=Basic%20$auth"
}

# An env file written for the collector points Athena at 127.0.0.1:4318,
# where nothing listens any more. Replace just that line (and rewrite the
# comments that described the collector) with the telemetry settings; nothing
# else in the file changes, and nothing is printed from it.
LEGACY_OTLP_LINE=OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
migrate_env_file() {
    local file=$1 env=$2
    if ! [ -r "$file" ] || ! grep -qx "$LEGACY_OTLP_LINE" "$file"; then return 0; fi
    if [ "$DRY_RUN" = 1 ]; then
        changed "would replace the collector endpoint in $file with ATHENA_TELEMETRY_DIR and OpenObserve's OTLP settings"
        return 0
    fi
    local block group
    block=$(otlp_block)
    group=$(stat -c %G "$file")
    {
        # Only if the file does not set them already.
        grep -q '^ATHENA_TELEMETRY_DIR=' "$file" ||
            echo "ATHENA_TELEMETRY_DIR=/var/lib/athena/$env/telemetry"
        grep -q '^ATHENA_TELEMETRY_RETENTION_DAYS=' "$file" ||
            echo "ATHENA_TELEMETRY_RETENTION_DAYS=30"
        printf '%s\n' "$block"
    } >"$WORK/otlp.block"
    # The block replaces the first legacy line; any repeat is dropped.
    awk -v block="$WORK/otlp.block" -v legacy="$LEGACY_OTLP_LINE" '
        $0 == legacy && !done { while ((getline line < block) > 0) print line; close(block); done = 1; next }
        $0 == legacy { next }
        $0 == "# Telemetry to the local collector. Remove the line to turn telemetry off." { next }
        $0 == "# 1 records prompts and responses on spans. Keep off in prod; the collector" {
            print "# 1 records prompts and responses on spans. Set 0 to stop."; next }
        $0 == "# strips them for prod anyway." { next }
        { print }' "$file" >"$WORK/migrated.env"
    # Same owner and mode: install copies the content, not the metadata.
    install -m 0640 -o root -g "$group" "$WORK/migrated.env" "$file"
    changed "replaced the collector endpoint in $file with ATHENA_TELEMETRY_DIR and OpenObserve's OTLP settings"
    todo "restart $env to use its new telemetry settings (or let the next deploy do it): sudo systemctl restart athena@$env.target. A release from before the collector was removed exports to OpenObserve with them but writes no JSONL files until the next deploy"
}

# /etc/athena/<env>.env: created once, then only its mode is enforced.
ensure_env_files() {
    step "environment files"
    local env port tg file hosts otlp
    local group
    group=$(owner_or_root athena); group=${group##*:}
    for env in staging prod; do
        file=/etc/athena/$env.env
        if [ "$env" = prod ]; then port=$PORT_PROD; else port=$PORT_STAGING; fi
        # Each env needs its own bot: one token cannot be polled by two
        # processes (Telegram answers 409). Empty means no bot for this env.
        tg=$'\n# This env\'s own bot token from @BotFather; leave empty for no bot.\nTELEGRAM_BOT_TOKEN='
        hosts="$TAILNET_IP:$port,$HOST_ALIAS:$port"
        if [ -f "$file" ]; then
            fix_mode "$file" 0640 "root:$group"
            same "$file (kept; may hold secrets)"
            if [ -r "$file" ] && ! grep -q "^ATHENA_ADDR=$TAILNET_IP:$port\$" "$file"; then
                warn "$file: ATHENA_ADDR is not $TAILNET_IP:$port; check it with sudoedit"
            fi
            migrate_env_file "$file" "$env"
            continue
        fi
        # A plain assignment, so a failure in otlp_block stops the script.
        otlp=$(otlp_block)
        render "$REPO_ROOT/deploy/athena.env.template" ENV "$env" TAILNET_IP "$TAILNET_IP" \
            PORT "$port" ALLOWED_HOSTS "$hosts" TELEGRAM_BLOCK "$tg" \
            OTLP_BLOCK "$otlp" >"$WORK/$env.env"
        if [ "$DRY_RUN" = 1 ]; then
            changed "would create $file (0640 root:athena):"
            sed 's/^/      /' "$WORK/$env.env" >&2
        else
            install -m 0640 -o root -g "$group" "$WORK/$env.env" "$file"
            changed "created $file"
        fi
        todo "type the secrets into $file yourself: sudoedit $file"
    done
    printf 'ATHENA_REPO=%s\nATHENA_KEEP_RELEASES=3\nORAS=/usr/local/bin/oras\n' "$ATHENA_REPO" >"$WORK/gate.env"
    install_file "$WORK/gate.env" /etc/athena/gate.env 0644 root:root || true
}

install_sudoers() {
    step "sudoers"
    cat >"$WORK/sudoers" <<'EOF'
# Managed by athena scripts/setup-host.sh. deploy may run deploy-gate as root
# and nothing else. SSH_ORIGINAL_COMMAND carries CI's request through sudo.
Defaults!/opt/athena/bin/deploy-gate env_keep += "SSH_ORIGINAL_COMMAND"
deploy ALL=(root) NOPASSWD: /opt/athena/bin/deploy-gate *
EOF
    if command -v visudo >/dev/null 2>&1; then
        visudo -cf "$WORK/sudoers" >/dev/null || die "generated sudoers rule failed visudo -c"
    else
        warn "visudo not found; sudoers rule not validated"
    fi
    install_file "$WORK/sudoers" /etc/sudoers.d/athena-deploy 0440 root:root || true
}

# One authorized_keys line per env. A key not given keeps its existing line.
ensure_authorized_keys() {
    step "deploy's authorized_keys"
    local file=/home/deploy/.ssh/authorized_keys env key line
    : >"$WORK/authorized_keys"
    for env in staging prod; do
        key=$STAGING_PUBKEY
        [ "$env" = prod ] && key=$PROD_PUBKEY
        if [ -n "$key" ]; then
            [ -f "$key" ] || die "no such public key file: $key"
            [ "$(grep -cv '^\s*$' "$key")" = 1 ] || die "$key must hold exactly one key"
            line=$(grep -v '^\s*$' "$key")
            [[ "$line" =~ ^(ssh-ed25519|ecdsa-sha2-nistp256|ssh-rsa)\ [A-Za-z0-9+/=]+ ]] ||
                die "$key is not an OpenSSH public key"
            ssh-keygen -lf "$key" >/dev/null 2>&1 || die "ssh-keygen rejects $key"
            [[ "$line" == *PRIVATE* ]] && die "$key looks like a private key"
            line=$(awk '{print $1" "$2}' <<<"$line")
            printf 'restrict,command="sudo /opt/athena/bin/deploy-gate --key-env %s" %s ci-%s\n' \
                "$env" "$line" "$env" >>"$WORK/authorized_keys"
        elif [ -r "$file" ] && grep -q -- "--key-env $env\"" "$file"; then
            grep -- "--key-env $env\"" "$file" >>"$WORK/authorized_keys"
        else
            todo "no CI key for $env yet: re-run with --ci-$env-pubkey FILE"
        fi
    done
    [ -s "$WORK/authorized_keys" ] || return 0
    local deploy
    deploy=$(owner_or_root deploy)
    ensure_dir /home/deploy/.ssh 0700 "$deploy"
    install_file "$WORK/authorized_keys" "$file" 0600 "$deploy" || true
}

install_gate() {
    step "deploy-gate"
    if [ -z "$GATE_DIGEST" ]; then
        if [ -x /opt/athena/bin/deploy-gate ]; then
            same "deploy-gate $(stamp_get deploy-gate)"
        else
            todo "install deploy-gate: re-run with --gate-digest sha256:... (the digest the first main build pushed)"
        fi
        return 0
    fi
    if [ -x /opt/athena/bin/deploy-gate ] && [ "$(stamp_get deploy-gate)" = "$GATE_DIGEST" ]; then
        same "deploy-gate $GATE_DIGEST"; return 0
    fi
    if [ "$DRY_RUN" = 1 ]; then
        changed "would pull $ATHENA_REPO@$GATE_DIGEST and install deploy-gate to /opt/athena/bin/"
        return 0
    fi
    /usr/local/bin/oras pull "$ATHENA_REPO@$GATE_DIGEST" -o "$WORK/gate" >/dev/null ||
        die "oras pull $ATHENA_REPO@$GATE_DIGEST failed (is the GHCR package public?)"
    [ -f "$WORK/gate/deploy-gate" ] || die "artifact $GATE_DIGEST has no deploy-gate file"
    install -m 0755 -o root -g root "$WORK/gate/deploy-gate" /opt/athena/bin/deploy-gate
    stamp_set deploy-gate "$GATE_DIGEST"
    changed "installed deploy-gate from $GATE_DIGEST"
}

enable_services() {
    step "enable services"
    run systemctl daemon-reload
    local target
    for target in athena@staging.target athena@prod.target athena-serve@staging.service \
        athena-serve@prod.service; do
        if systemctl is-enabled --quiet "$target" 2>/dev/null; then same "enabled $target"; else
            run systemctl enable --quiet "$target"
            changed "enabled $target"
        fi
    done
    # A bot runs in an env only when its env file has a token; without one
    # the unit would fail and restart forever. deploy-gate starts an enabled
    # bot on each deploy.
    local env unit
    for env in staging prod; do
        unit=athena-telegram@$env.service
        if [ -r "/etc/athena/$env.env" ] && grep -Eq '^TELEGRAM_BOT_TOKEN=.+' "/etc/athena/$env.env"; then
            if systemctl is-enabled --quiet "$unit" 2>/dev/null; then same "enabled $unit"; else
                run systemctl enable --quiet "$unit"
                changed "enabled $unit"
            fi
        elif systemctl is-enabled --quiet "$unit" 2>/dev/null; then
            run systemctl disable --quiet "$unit"
            changed "disabled $unit (no TELEGRAM_BOT_TOKEN in /etc/athena/$env.env)"
        else
            todo "no Telegram bot for $env: put its own token in /etc/athena/$env.env, then re-run setup-host.sh"
        fi
    done
    [ -e /opt/athena/staging/current ] ||
        todo "first deploy: merge to main; CI runs 'deploy staging <digest>' and starts the units"

    [ "$SKIP_OBSERVABILITY" = 1 ] && return 0
    if [ "${O2_RESTART:-0}" = 1 ] || ! systemctl is-active --quiet openobserve 2>/dev/null; then
        run systemctl enable --quiet openobserve.service
        run systemctl restart openobserve.service
        changed "(re)started openobserve"
    fi
}

report_tailscale() {
    command -v tailscale >/dev/null 2>&1 || return 0
    local tags
    tags=$(tailscale status --json 2>/dev/null |
        python3 -c 'import json,sys; print(",".join(json.load(sys.stdin).get("Self",{}).get("Tags") or []))' 2>/dev/null || true)
    if [ -z "$tags" ]; then
        todo "this node is user-owned (no tags): use the hosts alias \"$HOST_ALIAS\": \"$TAILNET_IP\" in deploy/tailscale-policy.hujson; do not re-tag it"
    else
        todo "this node is tagged ($tags): target those tags in deploy/tailscale-policy.hujson"
    fi
}

uninstall() {
    step "uninstall"
    if [ "$PURGE" = 1 ]; then
        confirm "This deletes Athena's databases, backups and /etc/athena secrets for good."
    else
        confirm "This stops Athena and removes its units, binaries and deploy access (databases and /etc/athena are kept)."
    fi
    local unit
    for unit in athena@staging.target athena@prod.target athena-serve@staging.service \
        athena-serve@prod.service athena-telegram@prod.service athena-telegram@staging.service; do
        run systemctl disable --now "$unit" >/dev/null 2>&1 || true
    done
    for unit in athena-serve@.service athena-telegram@.service athena@.target; do
        if [ -e "/etc/systemd/system/$unit" ]; then run rm -f "/etc/systemd/system/$unit"; changed "removed $unit"; fi
    done
    if [ -e /etc/sudoers.d/athena-deploy ]; then run rm -f /etc/sudoers.d/athena-deploy; changed "removed sudoers rule"; fi
    if [ -e /home/deploy/.ssh/authorized_keys ]; then run rm -f /home/deploy/.ssh/authorized_keys; changed "removed deploy's authorized_keys"; fi
    if [ -d /opt/athena ]; then run rm -rf /opt/athena; changed "removed /opt/athena"; fi
    if [ "$PURGE" = 1 ]; then
        [ -d /etc/athena ] && { run rm -rf /etc/athena; changed "removed /etc/athena"; }
        [ -d /var/lib/athena ] && { run rm -rf /var/lib/athena; changed "removed /var/lib/athena"; }
        if [ -e /etc/systemd/system/openobserve.service ]; then
            run systemctl disable --now openobserve.service >/dev/null 2>&1 || true
            run rm -f /etc/systemd/system/openobserve.service
            changed "removed openobserve.service"
        fi
        if [ -n "$(stamp_get openobserve)" ]; then
            run rm -rf /usr/local/bin/openobserve /var/lib/openobserve /etc/openobserve
            changed "removed OpenObserve binary, data and settings"
        fi
        retire_otelcol
        for bin in oras duckdb; do
            if [ -n "$(stamp_get "$bin")" ]; then run rm -f "/usr/local/bin/$bin"; changed "removed /usr/local/bin/$bin"; fi
        done
        local users=(deploy athena openobserve)
        # Only when an earlier version of this script created it for the
        # collector; the package never removes it.
        [ -n "$(stamp_get user-otelcol-contrib)" ] && users+=(otelcol-contrib)
        for u in "${users[@]}"; do
            if id "$u" >/dev/null 2>&1; then run userdel -r "$u" >/dev/null 2>&1 || run userdel "$u"; changed "removed user $u"; fi
        done
        [ -d "$STAMPS" ] && run rm -rf "$STAMPS"
    else
        todo "kept /etc/athena and /var/lib/athena; --uninstall --purge deletes them"
    fi
    run systemctl daemon-reload
}

summary() {
    echo
    if [ "$DRY_RUN" = 1 ]; then echo "Dry run: ${#CHANGED[@]} change(s) would be made."; else
        echo "Done: ${#CHANGED[@]} change(s), ${#UNCHANGED[@]} item(s) already in place."
    fi
    local item
    if [ ${#CHANGED[@]} -gt 0 ]; then echo "Changes:"; for item in "${CHANGED[@]}"; do echo "  - $item"; done; fi
    if [ ${#WARNINGS[@]} -gt 0 ]; then echo "Warnings:"; for item in "${WARNINGS[@]}"; do echo "  - $item"; done; fi
    if [ ${#TODO[@]} -gt 0 ]; then echo "Still for a human to do:"; for item in "${TODO[@]}"; do echo "  - $item"; done; fi
    echo "Untouched by design: Docker, ufw, sshd, Caddy, Tailscale configuration."
}

main() {
    preconditions
    if [ "$UNINSTALL" = 1 ]; then
        uninstall
        summary
        return 0
    fi
    detect_tailnet_ip
    check_ports
    ensure_users
    ensure_dirs
    install_oras
    install_duckdb
    O2_RESTART=0
    retire_otelcol
    [ "$SKIP_OBSERVABILITY" = 1 ] || install_openobserve
    install_units
    ensure_env_files
    install_sudoers
    ensure_authorized_keys
    install_gate
    enable_services
    report_tailscale
    summary
}

main
