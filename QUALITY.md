# Quality gates

This is a **public library**. Its crates are published for other people to depend on, and the
connector binary is installed by AI agents on machines nobody here controls. A defect does not
break one deployment — it ships.

That difference decides which gates matter. Everything below runs in CI and locally with the
same scripts.

## Run everything

```bash
scripts/qa/all.sh
```

| Gate | Command | What it protects |
|---|---|---|
| Format | `scripts/qa/for-each-workspace.sh cargo fmt --all --check` | one shape |
| Lint | `scripts/qa/for-each-workspace.sh cargo clippy --all-targets` | correctness hazards |
| Tests | `scripts/qa/for-each-workspace.sh cargo test` | behaviour |
| **Public API** | `scripts/qa/semver.sh` | that consumers' builds keep working |
| Docs | `scripts/qa/docs.sh` | the interface people actually read |
| MSRV | `scripts/qa/msrv.sh` | the Rust version we promise |
| Coverage | `scripts/qa/coverage.sh` | that the crypto is tested |
| **Mutation** | `scripts/qa/mutants.sh` | that the tests would NOTICE a break |
| Supply chain | `cargo deny check` | advisories and licences |
| Release | `scripts/qa/verify-release.sh <tag>` | that published binaries actually run |

## There is more than one workspace

`connect`, `connect-iroh` and `connector-mcp` have workspaces of their own: the webrtc and
Scrypto trees pin `regex` to ranges that do not overlap, so they cannot resolve together.
`cargo <anything>` at the root does **not** see them, which is why every gate goes through
`scripts/qa/for-each-workspace.sh`.

## The two gates that only a library needs

### Public API compatibility

Removing a public function, narrowing a parameter, adding a field to a struct consumers
construct — none of it fails to compile here and no test catches it. It breaks at *their*
build, after release.

`cargo-semver-checks` compares against the previous commit rather than crates.io, because
nothing is published yet and waiting for a first release would mean the first breaking change
is found by a user. It has been verified to catch a real one: making
`interaction_discriminator` private compiles cleanly, passes every test, and is reported as
`pub fn removed or renamed: semver requires new major version`.

### Documentation

For a library the docs are the interface — consumers read docs.rs, not the source. `missing_docs`
is on, and rustdoc runs with `-D warnings` so a broken intra-doc link fails the build. It has
already caught doc links naming enum variants that do not exist.

## Coverage is per crate, and mutation is what it cannot say

Crates here do different jobs. `rola` and `address` decide whether a signature proves ownership
and which address a key controls; `gateway-tx` is mostly HTTP whose value is proven by talking
to a real Gateway. One average would let the first slide behind the second, so each has its own
floor and the total has one too.

Coverage still only says a line *ran*. `cargo mutants` says whether breaking it would be
noticed, and on its first run it found this:

```
MISSED  replace verify_account_proof -> Result<(), RolaError> with Ok(())
MISSED  replace != with == in verify_account_proof
```

The entire ROLA signature verification could be replaced with "always succeed" and the suite
stayed green — `rola` had one test, and it only checked a digest length. That is the finding
that justifies the whole exercise. Seven tests later, every mutant is caught.

`Display` impls for error enums are excluded: mutating them changes the *wording* of a message,
and pinning error text in tests makes translation fail the build for no gain in correctness.

## Open question before publishing to crates.io

The Radix Scrypto crates (`radix-common`, `radix-transactions`, `sbor`, …) publish with **no
`license` field** — verified against the crates.io API, it is upstream's omission. Their
repository carries **"Radix License v1.0"**, written by the Radix Foundation, with no SPDX
identifier. It is not Apache-2.0.

This SDK declares itself Apache-2.0 while depending on them. Whether that can be published and
redistributed as Apache-2.0 is a question for whoever owns the release, with the licence text in
front of them. `deny.toml` records the fact as `LicenseRef-Radix-1.0` so the gate keeps working
for every other dependency; recording it is not resolving it.

## For an AI agent working here

Run `scripts/qa/all.sh` before calling anything done. Do not silence a gate to get past it:

- no `#[allow(...)]` without a comment saying why the lint is wrong **here**;
- no lowering a coverage threshold or an MSRV;
- no deleting or weakening a test to make it green;
- if you change a public API, say so and bump the version the semver report asks for.

If a gate is genuinely wrong, argue it in the commit message and remove it. `must_use_candidate`
was dropped exactly that way: 27 hits, nearly all on plain getters where ignoring the result is
not a bug, and a rule that only fires falsely teaches people to stop reading the linter.
