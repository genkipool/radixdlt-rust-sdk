# radixdlt-address

Native derivation of a Radix **virtual-account address** from an Ed25519 public key,
using `radix-common` — no Node, no RET-via-JS.

***English** · [Español](README.es.md)*

```toml
[dependencies]
radixdlt-address = "0.1"
```

```rust
use radixdlt_address::virtual_account_address;

// network_id: 1 = mainnet, 2 = stokenet
let address = virtual_account_address(public_key_hex, 2)?;
```

Error messages are localized to the system language. Part of the
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## License

Licensed under either of MIT or Apache-2.0 at your option.
