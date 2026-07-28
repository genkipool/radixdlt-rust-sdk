#!/usr/bin/env bash
# Mutation testing on the code that decides whether a signature is real.
#
# Coverage says a line ran; it does not say that breaking it would fail anything. Here that
# distinction is the whole point: `rola` decides whether a signature proves ownership of an
# account, and `address` derives the address a key controls. A surviving mutant in either means
# the tests would not notice a verification that stopped verifying, or a key that started
# deriving somebody else's address.
#
# Scoped to those files on purpose. Mutating the transports would take hours and their bugs are
# found by talking to a real wallet, not by unit tests.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
OUT=target/qa/mutants
mkdir -p "$OUT"

cargo mutants \
    --file 'crates/rola/src/lib.rs' \
    --file 'crates/address/src/lib.rs' \
    --file 'crates/keystore/src/lib.rs' \
    --file 'crates/connect-types/src/lib.rs' \
    --output "$OUT" \
    # `Display` impls for error enums are excluded: mutating them only changes the WORDING of
    # a message, and pinning error text in tests makes translations and rewording fail the
    # build for no gain in correctness.
    --exclude-re 'impl std::fmt::Display' \
    --timeout 120 \
    "$@"
status=$?

echo
if [ $status -eq 0 ]; then
    echo "No surviving mutants: every change to signature verification is caught."
else
    echo "Mutants SURVIVED — see $OUT/mutants.out/missed.txt." >&2
    echo "Each one is a change to the crypto that no test objects to." >&2
fi
exit $status
