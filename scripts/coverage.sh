#!/usr/bin/env bash
# The line-coverage gate: every source line must be executed by some test.
# CI runs this; run it locally before opening a PR.
#
# Why not `cargo llvm-cov --fail-under-lines 100`: that summary counts each
# compiled copy of a generic function separately. main.rs compiles its own
# copy of `cli::run` for stdin/stdout, and only a live provider can drive that
# copy past the API-key check, so it reports lines as missed that the tests
# do run through other copies. LCOV counts each source line once, covered if
# any test ran it. Nothing is excluded: an unexecuted line still fails here.
set -euo pipefail

lcov="${1:-target/lcov.info}"
cargo llvm-cov --all-targets --locked --lcov --output-path "$lcov"

missed=$(awk -F'[:,]' '/^SF:/ {file=$2} /^DA:/ && $3 == 0 {print file ":" $2}' "$lcov" | sort -u)
total=$(grep -c '^DA:' "$lcov")

if [ -n "$missed" ]; then
    echo "Lines no test executes:"
    echo "$missed"
    exit 1
fi
echo "Line coverage: 100% of $total lines"
