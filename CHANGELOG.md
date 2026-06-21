# Changelog

All notable changes to the RadixDLT Rust SDK are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates
follow [Semantic Versioning](https://semver.org/). While the crates are in `0.x`,
minor versions may contain breaking changes.

## [Unreleased]

## [0.1.0]

First release. All crates start at `0.1.0`.

### Added

- `radixdlt-i18n` — system-locale detection and bilingual (English/Spanish) text helpers.
- `radixdlt-address` — native Ed25519 virtual-account address derivation.
- `radixdlt-rola` — native ROLA (Radix Off-Ledger Authentication) verification.
- `radixdlt-keystore` — encrypted Ed25519 keystore (scrypt + AES-256-GCM), `key.json`-compatible.
- `radixdlt-gateway-tx` — Gateway client plus local transaction building, signing and submission.
- `radixdlt-connect` — Radix Connect over WebRTC (talks to the Radix mobile wallet).
- `radixdlt-connect-iroh` — Radix Connect over Iroh/QUIC for pure-Rust SDK-to-SDK peers.
- `radixdlt-sdk` — umbrella crate re-exporting the above behind feature flags.

[Unreleased]: https://github.com/genkipool/radixdlt-rust-sdk/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/genkipool/radixdlt-rust-sdk/releases/tag/v0.1.0
