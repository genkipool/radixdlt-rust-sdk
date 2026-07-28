#!/usr/bin/env bash
# Public API compatibility. THE gate a library needs and a binary does not.
#
# These crates are published for other people to depend on. Removing a public function,
# narrowing a parameter, adding a field to a struct they construct — none of that fails to
# compile HERE, and no test catches it. It breaks at THEIR build, after release.
#
# The baseline is a GIT REVISION, not crates.io: nothing is published yet, and waiting for a
# release to start checking would mean the first breaking change is found by a user. Comparing
# against the previous commit catches it while it is still a diff.
#
#   scripts/qa/semver.sh                 # against origin/main
#   scripts/qa/semver.sh v0.2.0          # against a tag
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

BASELINE="${1:-origin/main}"

if ! git rev-parse --verify --quiet "$BASELINE" >/dev/null; then
    echo "No baseline '$BASELINE' to compare against — nothing to check yet." 
    exit 0
fi

echo "Comparing the public API against $BASELINE"
echo
cargo semver-checks check-release --baseline-rev "$BASELINE" --workspace
status=$?

echo
if [ $status -eq 0 ]; then
    echo "Semver OK — the public API is compatible with $BASELINE."
else
    echo "The public API CHANGED incompatibly against $BASELINE." >&2
    echo "Either restore it, or bump the version the report asks for." >&2
fi
exit $status
