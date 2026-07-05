# radixdlt-sdk

Umbrella crate for the **RadixDLT Rust SDK**. Re-exports the individual crates behind
feature flags so you can depend on one crate and opt into exactly what you need.

***English** · [Español](README.es.md)*

```toml
# Verify ROLA proofs (default):
radixdlt-sdk = "0.1"

# Build/send transactions + manage keys:
radixdlt-sdk = { version = "0.1", features = ["full"] }
```

## Features

| Feature | Re-exports | What it gives you |
|---|---|---|
| `address` | `radixdlt-address` | Account-address derivation |
| `rola` *(default)* | `radixdlt-rola` | ROLA off-ledger authentication (implies `address`) |
| `keystore` | `radixdlt-keystore` | Encrypted Ed25519 keystore (implies `address`) |
| `gateway` | `radixdlt-gateway-tx` | Gateway client + local transaction signing |
| `full` | all of the above | — |

The `i18n` module (system-language detection) is always available. All user-facing
error messages are localized to the system language (English/Spanish).

## Wallet / transport

Radix Connect (wallet pairing) is **not** re-exported here because its transport
dependency tree cannot be resolved together with the `gateway` feature's
`radix-engine` tree. Add the transport directly:

- [`radixdlt-connect`](https://crates.io/crates/radixdlt-connect) — WebRTC, talks to the Radix mobile wallet.
- [`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh) — QUIC, for pure-Rust SDK-to-SDK peers.

## License

Licensed under either of MIT or Apache-2.0 at your option.
