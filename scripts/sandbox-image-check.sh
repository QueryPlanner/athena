#!/usr/bin/env bash
# Checks that every tool the sandbox image promises actually runs in it.
#
#   scripts/sandbox-image-check.sh [IMAGE]    # default athena-sandbox:dev
#
# Build first: docker build -t athena-sandbox:dev deploy/sandbox-image
set -euo pipefail

image="${1:-athena-sandbox:dev}"

# One command per tool; each must exit 0.
checks=(
  "agent-browser --version"
  "agent-browser skills get core | grep -q 'name: core'"
  "chromium --version"
  "git --version"
  "curl --version"
  "python3 --version"
  "jupyter-server --version"
  "ffmpeg -version"
  "yt-dlp --version"
  "fd --version"
  "file --version"
  "jq --version"
  "rg --version"
  "tree --version"
  "unzip -v"
  "xz --version"
  "zip -v"
  "dig -v"
  "ssh -V"
  "rsync --version"
  "wget --version"
  "python3 -c 'import requests, pandas, PIL, bs4'"
)

failed=0
for check in "${checks[@]}"; do
  if docker run --rm --entrypoint /bin/sh "$image" -c "$check" >/dev/null 2>&1; then
    echo "ok    $check"
  else
    echo "FAIL  $check"
    failed=1
  fi
done

exit "$failed"
