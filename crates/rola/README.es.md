# radixdlt-rola

*[English](README.md) · **Español***

Verificación nativa de **ROLA** (Radix Off-Ledger Authentication) en Rust — un
sustituto directo de `@radixdlt/rola`. Comprueba que una prueba de wallet firma el
desafío esperado y que la clave pública deriva a la cuenta reclamada.

```toml
[dependencies]
radixdlt-rola = "0.1"
```

```rust
use radixdlt_rola::{verify_account_proof, AccountProof};

verify_account_proof(&proof, challenge_hex, dapp_definition, origin, network_id)?;
```

Ideal para backends que hacen "iniciar sesión con Radix". Los mensajes de error se
localizan al idioma del sistema. Forma parte del
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
