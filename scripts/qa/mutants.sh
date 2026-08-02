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

# cargo-mutants copies the whole tree -- `target` directories of four workspaces included --
# once per build. On a machine where /tmp is a tmpfs that copy is made in RAM, and it runs out
# long before the run finishes ("No space left on device") with plenty of disk free. Keep the
# scratch on real disk, beside the output it already writes there.
SCRATCH=target/qa/mutants-scratch
mkdir -p "$SCRATCH"
export TMPDIR="$PWD/$SCRATCH"

# `Display` impls for error enums are excluded: mutating them only changes the WORDING of a
# message, and pinning error text in tests makes translations and rewording fail the build for
# no gain in correctness.
#
# This comment lives ABOVE the command on purpose. Inside the backslash continuation it ended
# the command early -- bash joins the escaped newline, meets the `#`, and stops there -- so
# `--exclude-re` ran as a command of its own and `--timeout` was silently dropped. The script
# then read the resulting 127 as "mutants survived".
cargo mutants \
    --file 'crates/rola/src/lib.rs' \
    --file 'crates/address/src/lib.rs' \
    --file 'crates/keystore/src/lib.rs' \
    --file 'crates/connect-types/src/lib.rs' \
    --output "$OUT" \
    --exclude-re 'impl std::fmt::Display' \
    --timeout 120 \
    "$@"
status=$?

echo
# cargo-mutants distinguishes its outcomes by exit code: 2 is survivors, 3 is timeouts, 4 is a
# tree that would not build. Folding them together reports a broken run as "mutants survived"
# and sends you hunting for a missing test that does not exist -- which is exactly what a
# `No space left on device` did here.
case $status in
    0) echo "No surviving mutants: every change to signature verification is caught." ;;
    2) echo "Mutants SURVIVED — see $OUT/mutants.out/missed.txt." >&2
       echo "Each one is a change to the crypto that no test objects to." >&2 ;;
    3) echo "Mutants TIMED OUT — see $OUT/mutants.out/timeout.txt." >&2 ;;
    4) echo "The tree does not build, so nothing was mutated." >&2 ;;
    *) echo "cargo mutants could not run (exit $status). Nothing was measured." >&2 ;;
esac
exit $status
