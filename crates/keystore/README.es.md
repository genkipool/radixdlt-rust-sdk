# radixdlt-keystore

*[English](README.md) · **Español***

**Keystore** Ed25519 cifrado para el ledger de Radix (KDF scrypt + AES-256-GCM),
compatible con el formato `key.json` de Radix. Una librería pura: nunca pregunta, nunca
imprime y nunca termina el proceso.

```toml
[dependencies]
radixdlt-keystore = "0.1"
```

```rust
use radixdlt_keystore::KeyFile;

let kf = KeyFile::generate(2, passphrase)?; // nueva clave aleatoria (stokenet)
kf.save("key.json")?;                        // 0600, crea los directorios padre
let signing_key = kf.signing_key(passphrase)?;
```

Los mensajes de error se localizan al idioma del sistema. Forma parte del
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## Formato del archivo

El formato en disco `key.json` (scrypt + AES-256-GCM) con el flujo de
cifrado/descifrado está especificado en [`docs/FORMAT.es.md`](docs/FORMAT.es.md)
([English](docs/FORMAT.md)).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
