#!/usr/bin/env bash
# The published documentation must BUILD, and its links must resolve.
#
# For a library the docs are the interface: a consumer reads docs.rs, not the source. A broken
# intra-doc link there is a dead end at the exact moment someone is trying to understand the
# API, and it is invisible from `cargo build`.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

# `-D warnings` covers broken_intra_doc_links, private_intra_doc_links and the rest of the
# rustdoc lint set. `--no-deps` because we are answerable for our own docs, not our dependencies'.
RUSTDOCFLAGS="-D warnings" ./scripts/qa/for-each-workspace.sh cargo doc --no-deps --all-features
