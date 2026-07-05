# radixdlt-address

*[English](README.md) · **Español***

Derivación nativa de la **dirección de cuenta virtual** de Radix a partir de una clave
pública Ed25519, usando `radix-common` — sin Node, sin RET-vía-JS.

```toml
[dependencies]
radixdlt-address = "0.1"
```

```rust
use radixdlt_address::virtual_account_address;

// network_id: 1 = mainnet, 2 = stokenet
let address = virtual_account_address(public_key_hex, 2)?;
```

Los mensajes de error se localizan al idioma del sistema. Forma parte del
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
