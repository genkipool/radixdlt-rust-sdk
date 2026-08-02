#!/usr/bin/env bash
# Every gate, in the order CI runs them. Green here means green there.
#
# The release check is NOT included: it needs a published tag. Run
# `scripts/qa/verify-release.sh <tag>` after publishing.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

step() { echo; echo "════════════════════════════════════════════════"; echo "  $1"; echo "════════════════════════════════════════════════"; shift; "$@"; }

fail=0
step "Formatting"   ./scripts/qa/for-each-workspace.sh cargo fmt --all --check || fail=1
step "Clippy"       ./scripts/qa/for-each-workspace.sh cargo clippy --all-targets --all-features || fail=1
step "Tests"        ./scripts/qa/for-each-workspace.sh cargo test --all-features || fail=1
step "Docs"         ./scripts/qa/docs.sh || fail=1
step "MSRV"         ./scripts/qa/msrv.sh || fail=1
step "Coverage"     ./scripts/qa/coverage.sh || fail=1
step "Supply chain" cargo deny check || fail=1
step "Public API"   ./scripts/qa/semver.sh || fail=1
step "Mutation"     ./scripts/qa/mutants.sh || fail=1

echo
if [ "$fail" = 0 ]; then echo "ALL GATES PASSED."; else echo "SOME GATES FAILED — fix the code, not the gate." >&2; fi
exit $fail
