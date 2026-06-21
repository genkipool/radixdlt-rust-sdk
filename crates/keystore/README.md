# radixdlt-keystore

Encrypted Ed25519 **keystore** for the Radix ledger (scrypt KDF + AES-256-GCM),
compatible with the Radix `key.json` format. A pure library: it never prompts, never
prints and never exits the process.

*ES — Keystore Ed25519 cifrado (scrypt + AES-256-GCM), compatible con el formato `key.json` de Radix.*

```toml
[dependencies]
radixdlt-keystore = "0.1"
```

```rust
use radixdlt_keystore::KeyFile;

let kf = KeyFile::generate(2, passphrase)?; // new random key (stokenet)
kf.save("key.json")?;                        // 0600, parent dirs created
let signing_key = kf.signing_key(passphrase)?;
```

Error messages are localized to the system language. Part of the
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## License

Licensed under either of MIT or Apache-2.0 at your option.
