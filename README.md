# RadixDLT Rust SDK

[![CI](https://github.com/genkipool/radixdlt-rust-sdk/actions/workflows/ci.yml/badge.svg)](https://github.com/genkipool/radixdlt-rust-sdk/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/radixdlt-sdk.svg)](https://crates.io/crates/radixdlt-sdk)
[![docs.rs](https://img.shields.io/docsrs/radixdlt-sdk)](https://docs.rs/radixdlt-sdk)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Native Rust building blocks for the [Radix](https://radixdlt.com) ledger — the
off-ledger primitives that, until now, only existed in JavaScript/TypeScript. Build
"log in with Radix" backends, transaction tools and wallet integrations in pure Rust.

*ES — Bloques nativos en Rust para el ledger de Radix: ROLA, derivación de direcciones, keystore, transacciones del Gateway y transportes (WebRTC/Iroh). Todo el texto visible sale en el idioma del sistema (inglés/español).*

## Crates

| Crate | What it does |
|---|---|
| [`radixdlt-sdk`](crates/sdk) | Umbrella crate; re-exports the below behind feature flags |
| [`radixdlt-rola`](crates/rola) | ROLA off-ledger authentication (drop-in for `@radixdlt/rola`) |
| [`radixdlt-address`](crates/address) | Virtual-account address derivation |
| [`radixdlt-keystore`](crates/keystore) | Encrypted Ed25519 keystore (scrypt + AES-256-GCM) |
| [`radixdlt-gateway-tx`](crates/gateway-tx) | Gateway client + local transaction signing |
| [`radixdlt-connect`](crates/connect) | Radix Connect over **WebRTC** (talks to the mobile wallet) |
| [`radixdlt-connect-iroh`](crates/connect-iroh) | Radix Connect over **Iroh/QUIC** (pure-Rust SDK-to-SDK) |
| [`radixdlt-i18n`](crates/i18n) | System-locale detection + bilingual text helpers |

## Quick start

```toml
# Verify ROLA proofs (log in with Radix):
radixdlt-sdk = "0.1"            # default feature: rola

# Build and send transactions + manage keys:
radixdlt-sdk = { version = "0.1", features = ["full"] }
```

## Design notes

- **Bilingual at runtime.** All user-facing error text is localized to the system
  language (`es*` → Spanish, otherwise English) via `radixdlt-i18n`.
- **Two transports, by design.** `webrtc` and the `radix-engine` tree (used by
  `gateway`) cannot be resolved in the same binary; neither can `webrtc` and `iroh`.
  So the transports are separate crates — pick the one your tool needs.
- **Workspaces.** `radixdlt-connect` and `radixdlt-connect-iroh` are isolated
  workspaces (heavy WebRTC / QUIC dependency trees); the rest share the main
  workspace.

## Author

Created and maintained by **Luis Alberto Reoyo Bolaños**
([genkipool](https://github.com/genkipool)). Contributions are welcome — see
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
