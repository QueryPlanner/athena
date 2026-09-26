#!/usr/bin/env bash
# Configure the GitHub side of Athena's CI/CD, idempotently, with `gh api`:
#   - environment `staging` (deploys from main only)
#   - environment `prod` (deploys from v* tags only; no reviewer: pushing the
#     tag is the release decision, and the ruleset limits tagging to admins)
#   - a ruleset protecting v* tags (only admins create; no delete/move)
#   - variables VM_HOST and VM_KNOWN_HOSTS
#   - the DEPLOY_SSH_KEY secret in each environment
# It checks, but never sets, TS_AUTH_KEY or TS_OAUTH_CLIENT_ID / TS_OAUTH_SECRET: the human
# types those into `gh secret set` (SETUP.md). Secret values are read from
# files and never printed.
#
#   ./scripts/init-github.sh --dry-run --vm-host 100.124.202.79 \
#       --staging-key ~/.config/athena/ci-staging --prod-key ~/.config/athena/ci-prod
set -euo pipefail

DRY_RUN=0
REPO=""
VM_HOST=""
KNOWN_HOSTS_FILE=""
STAGING_KEY=""
PROD_KEY=""
REVIEWER=""
RULESET_NAME="release-tags"

usage() {
    cat <<'EOF'
Usage: init-github.sh [options]

  --dry-run                 print every change without making it (reads still happen)
  --repo OWNER/NAME         default: the repository of the current directory
  --vm-host HOST            the VM's tailnet IP or name -> variable VM_HOST
  --known-hosts-file FILE   the VM's host key line(s) -> variable VM_KNOWN_HOSTS
                            (default: ssh-keyscan HOST, with the fingerprint printed
                            for you to compare)
  --staging-key FILE        private key CI uses for staging -> staging DEPLOY_SSH_KEY
  --prod-key FILE           private key CI uses for prod -> prod DEPLOY_SSH_KEY
  --reviewer LOGIN          optional: require LOGIN's approval before prod deploys
                            (default: none; the v* tag is the release decision)
  -h, --help                this help
EOF
}

die() { echo "init-github: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --repo) REPO="${2:?}"; shift ;;
        --vm-host) VM_HOST="${2:?}"; shift ;;
        --known-hosts-file) KNOWN_HOSTS_FILE="${2:?}"; shift ;;
        --staging-key) STAGING_KEY="${2:?}"; shift ;;
        --prod-key) PROD_KEY="${2:?}"; shift ;;
        --reviewer) REVIEWER="${2:?}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

command -v gh >/dev/null 2>&1 || die "gh (GitHub CLI) is required"
command -v jq >/dev/null 2>&1 || die "jq is required"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated: gh auth login"
if [ -z "$REPO" ]; then
    REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null) ||
        die "cannot tell which repository; pass --repo OWNER/NAME"
fi

CHANGED=()
TODO=()
changed() { CHANGED+=("$1"); echo "  + $1" >&2; }
step() { echo "== $1" >&2; }

# Write through the API, or show the call on a dry run. Bodies hold no secrets.
api_write() {
    local method=$1 path=$2 body=$3
    if [ "$DRY_RUN" = 1 ]; then
        echo "  would call: gh api -X $method $path" >&2
        jq -c . <<<"$body" | sed 's/^/      /' >&2
    else
        gh api -X "$method" "$path" --input - <<<"$body" >/dev/null
    fi
}

check_private_key() {
    local file=$1
    [ -f "$file" ] || die "no such key file: $file"
    ssh-keygen -y -P '' -f "$file" >/dev/null 2>&1 ||
        die "$file is not an unencrypted OpenSSH private key (CI cannot type a passphrase)"
}

echo "repository: $REPO" >&2
login=$(gh api user --jq .login)
admin=$(gh api "repos/$REPO" --jq .permissions.admin)
[ "$admin" = true ] || die "$login is not an admin of $REPO; environments and rulesets need admin"
# Optional: a required reviewer for prod. Off by default; the v* tag is the
# release decision. Required reviewers need a public repo or GitHub Enterprise.
reviewer_id=""
if [ -n "$REVIEWER" ]; then
    reviewer_id=$(gh api "users/$REVIEWER" --jq .id) || die "no GitHub user $REVIEWER"
fi

# Environment NAME with one deployment policy of TYPE (branch|tag) PATTERN.
ensure_environment() {
    local name=$1 type=$2 pattern=$3 extra=$4 body existing
    step "environment $name"
    body=$(jq -n --argjson extra "$extra" \
        '{deployment_branch_policy: {protected_branches: false, custom_branch_policies: true}} + $extra')
    # PUT is create-or-update, so it is idempotent by itself.
    api_write PUT "repos/$REPO/environments/$name" "$body"
    changed "environment $name ($type $pattern only)"
    existing=""
    if gh api "repos/$REPO/environments/$name" >/dev/null 2>&1; then
        existing=$(gh api "repos/$REPO/environments/$name/deployment-branch-policies" \
            --jq ".branch_policies[] | select(.name == \"$pattern\" and .type == \"$type\") | .id" 2>/dev/null || true)
    fi
    if [ -n "$existing" ]; then
        echo "  deployment policy $type $pattern already present" >&2
    else
        api_write POST "repos/$REPO/environments/$name/deployment-branch-policies" \
            "$(jq -n --arg n "$pattern" --arg t "$type" '{name: $n, type: $t}')"
        changed "environment $name: allow $type $pattern"
    fi
}

ensure_environment staging branch main '{}'
if [ -n "$reviewer_id" ]; then
    ensure_environment prod tag 'v*' \
        "$(jq -n --argjson id "$reviewer_id" '{reviewers: [{type: "User", id: $id}], prevent_self_review: false, wait_timer: 0}')"
    echo "  prod reviewer: $REVIEWER (self-review allowed, so a solo owner can approve)" >&2
else
    # PUT replaces the protection rules, so this also removes an old reviewer.
    ensure_environment prod tag 'v*' '{"reviewers": []}'
    echo "  prod: no reviewer; pushing a v* tag releases (only admins may tag)" >&2
fi

step "ruleset $RULESET_NAME (v* tags)"
ruleset=$(jq -n --arg name "$RULESET_NAME" '{
    name: $name,
    target: "tag",
    enforcement: "active",
    conditions: {ref_name: {include: ["refs/tags/v*"], exclude: []}},
    rules: [{type: "creation"}, {type: "update"}, {type: "deletion"}, {type: "non_fast_forward"}],
    bypass_actors: [{actor_id: 5, actor_type: "RepositoryRole", bypass_mode: "always"}]
}')
ruleset_id=$(gh api "repos/$REPO/rulesets" --jq ".[] | select(.name == \"$RULESET_NAME\") | .id" 2>/dev/null || true)
if [ -n "$ruleset_id" ]; then
    api_write PUT "repos/$REPO/rulesets/$ruleset_id" "$ruleset"
    changed "ruleset $RULESET_NAME updated (id $ruleset_id)"
else
    api_write POST "repos/$REPO/rulesets" "$ruleset"
    changed "ruleset $RULESET_NAME created: only admins create v* tags; none can be moved or deleted"
fi

step "variables"
if [ -n "$VM_HOST" ]; then
    if [ "$DRY_RUN" = 1 ]; then echo "  would set variable VM_HOST=$VM_HOST" >&2; else
        gh variable set VM_HOST --repo "$REPO" --body "$VM_HOST" >/dev/null
    fi
    changed "variable VM_HOST"
    kh=$(mktemp)
    trap 'rm -f "$kh"' EXIT
    if [ -n "$KNOWN_HOSTS_FILE" ]; then
        [ -f "$KNOWN_HOSTS_FILE" ] || die "no such file: $KNOWN_HOSTS_FILE"
        grep -v '^\s*#' "$KNOWN_HOSTS_FILE" | grep -v '^\s*$' >"$kh" || true
    else
        ssh-keyscan -T 10 -t ed25519 "$VM_HOST" 2>/dev/null >"$kh" || true
        [ -s "$kh" ] || die "ssh-keyscan $VM_HOST returned nothing; is the VM on your tailnet?"
        echo "  host key from ssh-keyscan (trust on first use). Compare with the VM:" >&2
        echo "    ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub   # on the VM" >&2
        ssh-keygen -lf "$kh" | sed 's/^/    scanned: /' >&2
    fi
    [ -s "$kh" ] || die "no host key lines to store in VM_KNOWN_HOSTS"
    ssh-keygen -lf "$kh" >/dev/null 2>&1 || die "VM_KNOWN_HOSTS content is not a known_hosts line"
    if [ "$DRY_RUN" = 1 ]; then echo "  would set variable VM_KNOWN_HOSTS ($(wc -l <"$kh" | tr -d ' ') line(s))" >&2; else
        gh variable set VM_KNOWN_HOSTS --repo "$REPO" <"$kh" >/dev/null
    fi
    changed "variable VM_KNOWN_HOSTS"
else
    TODO+=("VM_HOST / VM_KNOWN_HOSTS not set: re-run with --vm-host HOST")
fi

step "environment secrets"
for env in staging prod; do
    key=$STAGING_KEY
    [ "$env" = prod ] && key=$PROD_KEY
    if [ -z "$key" ]; then
        TODO+=("DEPLOY_SSH_KEY for $env not set: re-run with --$env-key FILE")
        continue
    fi
    check_private_key "$key"
    if [ "$DRY_RUN" = 1 ]; then echo "  would set secret DEPLOY_SSH_KEY in environment $env from $key" >&2; else
        gh secret set DEPLOY_SSH_KEY --repo "$REPO" --env "$env" <"$key" >/dev/null
    fi
    changed "secret DEPLOY_SSH_KEY in environment $env (value not shown)"
done
if [ -n "$STAGING_KEY" ] && [ -n "$PROD_KEY" ] &&
    [ "$(ssh-keygen -y -P '' -f "$STAGING_KEY")" = "$(ssh-keygen -y -P '' -f "$PROD_KEY")" ]; then
    die "staging and prod must use different keys: the key decides which environment CI may touch"
fi

step "repository secrets (checked, never set here)"
secrets=$(gh secret list --repo "$REPO" --json name --jq '.[].name' 2>/dev/null || true)
if grep -qx TS_AUTH_KEY <<<"$secrets"; then
    echo "  TS_AUTH_KEY present (auth key; rotate before it expires)" >&2
elif grep -qx TS_OAUTH_CLIENT_ID <<<"$secrets" && grep -qx TS_OAUTH_SECRET <<<"$secrets"; then
    echo "  TS_OAUTH_CLIENT_ID and TS_OAUTH_SECRET present" >&2
else
    TODO+=("no Tailscale credential for CI: gh secret set TS_AUTH_KEY --repo $REPO (or the OAuth pair TS_OAUTH_CLIENT_ID/TS_OAUTH_SECRET)")
fi

echo
if [ "$DRY_RUN" = 1 ]; then echo "Dry run: ${#CHANGED[@]} change(s) would be made to $REPO."; else
    echo "Done: ${#CHANGED[@]} setting(s) applied to $REPO (each is create-or-update)."
fi
for item in ${CHANGED[@]+"${CHANGED[@]}"}; do echo "  - $item"; done
if [ ${#TODO[@]} -gt 0 ]; then
    echo "Still to do:"
    for item in "${TODO[@]}"; do echo "  - $item"; done
fi
