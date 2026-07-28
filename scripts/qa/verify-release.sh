#!/usr/bin/env bash
# Verifies a PUBLISHED release by using it, not by trusting that CI was green.
#
# CI proves the binaries built. It does not prove they run, speak MCP, or can reach the Gateway
# over TLS — and that last one has already changed underneath us once, when reqwest 0.13 moved
# root certificates from a bundle inside the binary to the operating system's trust store.
#
#   scripts/qa/verify-release.sh connector-v0.2.3
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

TAG="${1:-}"
[ -z "$TAG" ] && { echo "usage: $0 <tag>   e.g. connector-v0.2.3" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "== assets published for $TAG =="
gh release view "$TAG" --json assets -q '.assets[].name' | sed 's/^/  /' || exit 1

echo
echo "== downloading the linux build =="
gh release download "$TAG" -p '*x86_64-unknown-linux-gnu' -D "$WORK" --clobber || exit 1
BIN="$WORK/radix-connector-mcp-x86_64-unknown-linux-gnu"
chmod +x "$BIN"
echo "  $(stat -c%s "$BIN") bytes, needs $(strings -a "$BIN" | grep -oE 'GLIBC_2\.[0-9]+' | sort -V | tail -1)"

fail=0

echo
echo "== it answers an MCP handshake =="
init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"verify","version":"1"}}}'
if printf '%s\n' "$init" | timeout 20 "$BIN" 2>/dev/null | grep -q '"result"'; then
    echo "  ok"
else
    echo "  FAILED: no result from initialize" >&2; fail=1
fi

echo
echo "== it reaches the Gateway over HTTPS =="
# A deliberately invalid hash: a STRUCTURED rejection proves the TLS handshake and the round
# trip worked, which a success would prove no better and is harder to arrange.
call='{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"transaction_status","arguments":{"network":"stokenet","intent_hash":"txid_tdx_2_1invalid"}}}'
out=$(printf '%s\n%s\n' "$init" "$call" | timeout 40 "$BIN" 2>/dev/null)
if grep -qE 'Gateway returned|validation_errors|IntentHash' <<< "$out"; then
    echo "  ok — the Gateway answered, so TLS and the round trip work"
else
    echo "  FAILED: no answer from the Gateway (TLS roots?)" >&2
    echo "$out" | tail -3 >&2
    fail=1
fi

echo
[ "$fail" = 0 ] && echo "Release $TAG verified." || echo "Release $TAG FAILED verification." >&2
exit $fail
