#!/usr/bin/env bash
# Coverage, per crate rather than one average.
#
# These crates do different jobs and deserve different bars. `rola` and `address` decide
# whether a signature proves ownership and which address a key controls — untested lines there
# are the ones that matter most. `gateway-tx` is mostly HTTP against a live Gateway, and its
# value is proven by talking to one, not by unit tests. A single average would let the first
# slide behind the second.
#
# Every number is a RATCHET: raise it when the code earns it, never lower it to pass.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

# crate path : minimum line coverage
declare -A MIN=(
    ["rola/src/lib.rs"]=74           # signature verification
    ["address/src/lib.rs"]=67        # address derivation
    ["connect-types/src/lib.rs"]=85  # the wire schema, pure parsing
    ["keystore/src/lib.rs"]=65       # key custody
    ["i18n/src/lib.rs"]=65
)
GLOBAL_MIN=65

OUT=target/qa/coverage
mkdir -p "$OUT"
echo "== measuring =="
cargo llvm-cov --workspace --summary-only --json --output-path "$OUT/coverage.json" >/dev/null 2>&1 \
    || { echo "coverage run failed" >&2; exit 1; }
cargo llvm-cov report --html --output-dir "$OUT/html" >/dev/null 2>&1 || true

pct_of() {
    python3 - "$OUT/coverage.json" "$1" <<'PYEOF'
import json, sys
with open(sys.argv[1]) as fh:
    data = json.load(fh)
for f in data["data"][0]["files"]:
    if f["filename"].endswith(sys.argv[2]):
        print(f"{f['summary']['lines']['percent']:.2f}")
        break
PYEOF
}

fail=0
echo
echo "== per crate =="
for f in "${!MIN[@]}"; do
    p=$(pct_of "$f"); min=${MIN[$f]}
    if [ -z "$p" ]; then
        printf '  %-30s MISSING from the report\n' "$f" >&2; fail=1; continue
    fi
    if awk -v p="$p" -v m="$min" 'BEGIN{exit !(p+0 < m+0)}'; then
        printf '  %-30s %6s%%  BELOW %s%%\n' "$f" "$p" "$min" >&2; fail=1
    else
        printf '  %-30s %6s%%  ok (min %s%%)\n' "$f" "$p" "$min"
    fi
done

total=$(python3 -c "
import json
d=json.load(open('$OUT/coverage.json'))
print(f\"{d['data'][0]['totals']['lines']['percent']:.2f}\")")
echo
if awk -v p="$total" -v m="$GLOBAL_MIN" 'BEGIN{exit !(p+0 < m+0)}'; then
    echo "  TOTAL ${total}%  BELOW ${GLOBAL_MIN}%" >&2; fail=1
else
    echo "  TOTAL ${total}%  ok (min ${GLOBAL_MIN}%)"
fi

echo
[ "$fail" = 0 ] && echo "Coverage OK." || echo "Coverage FAILED. Add tests — do not lower the thresholds." >&2
exit $fail
