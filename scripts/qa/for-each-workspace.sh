#!/usr/bin/env bash
# Runs a cargo command in every workspace of this SDK.
#
# `connect`, `connect-iroh` and `connector-mcp` are workspaces of their own: the webrtc and
# Scrypto trees pin `regex` to ranges that do not overlap, so they cannot resolve together.
# `cargo <anything>` at the root therefore does NOT see them, and a gate that only runs at the
# root silently exempts three quarters of the crates that ship.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

WORKSPACES=(. crates/connect crates/connect-iroh crates/connector-mcp)

[ $# -eq 0 ] && { echo "usage: $0 <command> [args...]" >&2; exit 2; }

failed=()
for ws in "${WORKSPACES[@]}"; do
    echo "══ ${ws}: $* ══"
    (cd "$ws" && "$@") || failed+=("$ws")
done

if [ ${#failed[@]} -gt 0 ]; then
    printf '\nFAILED in %d workspace(s):\n' "${#failed[@]}" >&2
    printf '  - %s\n' "${failed[@]}" >&2
    exit 1
fi
echo
echo "OK in all ${#WORKSPACES[@]} workspaces."
