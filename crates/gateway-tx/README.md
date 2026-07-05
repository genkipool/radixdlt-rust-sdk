# radixdlt-gateway-tx

Radix **Gateway** client plus local transaction building, signing, notarization and
submission — in native Rust. Read ledger state (epoch, balances, status, affected
entities) and build/sign/submit transactions with a local key.

***English** · [Español](README.es.md)*

```toml
[dependencies]
radixdlt-gateway-tx = "0.1"
```

```rust
use radixdlt_gateway_tx::Gateway;

let gw = Gateway::stokenet();
let tx = gw.build_notarized(manifest, &[&key], &key, false).await?;
let status = gw.submit_and_wait(&tx).await?;
```

> Note: this crate pulls the `radix-engine` dependency tree, which cannot be resolved
> together with the WebRTC tree of `radixdlt-connect`. Use them in separate binaries.

Error messages are localized to the system language. Part of the
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## License

Licensed under either of MIT or Apache-2.0 at your option.
