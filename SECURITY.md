# Security Policy

## Reporting a vulnerability

Please **do not** open a public issue for security vulnerabilities.

Instead, report them privately via GitHub's
[security advisories](https://github.com/genkipool/radixdlt-rust-sdk/security/advisories/new),
or by email to the maintainer. Include:

- the affected crate(s) and version(s),
- a description of the issue and its impact,
- steps to reproduce, if possible.

You can expect an initial response within a few days. Once a fix is ready it will be
released as a new patch version and the vulnerable versions will be yanked.

## Scope

These crates handle cryptographic material (Ed25519 keys, ROLA proofs) and network
transports. Particularly relevant areas: `radixdlt-keystore` (key encryption),
`radixdlt-rola` (proof verification), and the transports `radixdlt-connect` /
`radixdlt-connect-iroh`.

## Supported versions

While the project is in `0.x`, only the latest published minor version of each crate
receives security fixes.
