# radixdlt-gateway-tx

*[English](README.md) · **Español***

Cliente del **Gateway** de Radix más construcción, firma, notarización y envío local de
transacciones — en Rust nativo. Lee el estado del ledger (época, saldos, estado,
entidades afectadas) y construye/firma/envía transacciones con una clave local.

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

> Nota: este crate arrastra el árbol de dependencias de `radix-engine`, que no se puede
> resolver junto al árbol de WebRTC de `radixdlt-connect`. Úsalos en binarios separados.

Los mensajes de error se localizan al idioma del sistema. Forma parte del
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
