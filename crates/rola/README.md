# radixdlt-rola

Native **ROLA** (Radix Off-Ledger Authentication) verification in Rust — a drop-in
replacement for `@radixdlt/rola`. Verifies that a wallet proof signs the expected
challenge and that the public key derives to the claimed account.

***English** · [Español](README.es.md)*

```toml
[dependencies]
radixdlt-rola = "0.1"
```

```rust
use radixdlt_rola::{verify_account_proof, AccountProof};

verify_account_proof(&proof, challenge_hex, dapp_definition, origin, network_id)?;
```

Ideal for backends doing "log in with Radix". Error messages are localized to the
system language. Part of the [RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## License

Licensed under either of MIT or Apache-2.0 at your option.
