#!/usr/bin/env bash
# The declared minimum Rust version must actually work.
#
# `rust-version` in Cargo.toml is a PROMISE to consumers: their build fails with a clear message
# instead of a wall of errors when their toolchain is too old. A promise nobody tests drifts the
# first time someone uses a newer language feature, and the first to find out is a user on an
# older toolchain.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

MSRV=$(grep -m1 '^rust-version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
[ -z "$MSRV" ] && { echo "no rust-version declared in Cargo.toml" >&2; exit 1; }

echo "Declared MSRV: $MSRV"
if ! rustup toolchain list | grep -q "^$MSRV"; then
    echo "Installing $MSRV to check against it…"
    rustup toolchain install "$MSRV" --profile minimal || exit 1
fi

# Check, not build: the point is that the code COMPILES on that version, and `check` says so in
# a fraction of the time.
"cargo" "+$MSRV" check --workspace --all-features
