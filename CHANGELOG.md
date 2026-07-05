# Changelog

All notable changes to the RadixDLT Rust SDK are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates
follow [Semantic Versioning](https://semver.org/). While the crates are in `0.x`,
minor versions may contain breaking changes.

## [Unreleased]

### Added

- `radixdlt-connector-mcp` — local MCP server (stdio) that pairs a Radix Wallet
  over Radix Connect and gets transactions signed on the user's machine (pairing
  QR, `send_transaction`, pre-authorization, ROLA account proof, transaction
  status). Installs from GitHub (`cargo install --git …` or `scripts/install-connector.{sh,ps1}`).
- `radixdlt-i18n` — labelled `tr!` arms (`tr!(lang, en, Es: …, Fr: …)`) with an
  English fallback; `Lang` is now `#[non_exhaustive]`, so adding a language is
  non-breaking.

### Changed

- `radixdlt-connect-iroh` — `IrohConnector::bind_with` takes a `Relay` enum
  instead of a `bool` flag (breaking).
- `radixdlt-connect` — all signing calls now correlate the wallet response by
  `interactionId`, discarding stale queued responses; `LinkState` documents the
  multi-device API in the README.

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
