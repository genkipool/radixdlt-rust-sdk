# radixdlt-sdk

*[English](README.md) · **Español***

Crate paraguas del **RadixDLT Rust SDK**. Reexporta los crates individuales tras
*flags* de features para que dependas de un solo crate y actives exactamente lo que
necesitas.

```toml
# Verificar pruebas ROLA (por defecto):
radixdlt-sdk = "0.1"

# Construir/enviar transacciones + gestionar claves:
radixdlt-sdk = { version = "0.1", features = ["full"] }
```

## Features

| Feature | Reexporta | Qué te da |
|---|---|---|
| `address` | `radixdlt-address` | Derivación de direcciones de cuenta |
| `rola` *(por defecto)* | `radixdlt-rola` | Autenticación off-ledger ROLA (implica `address`) |
| `keystore` | `radixdlt-keystore` | Keystore Ed25519 cifrado (implica `address`) |
| `gateway` | `radixdlt-gateway-tx` | Cliente del Gateway + firma local de transacciones |
| `full` | todo lo anterior | — |

El módulo `i18n` (detección del idioma del sistema) siempre está disponible. Todos los
mensajes de error visibles se localizan al idioma del sistema (inglés/español).

## Wallet / transporte

Radix Connect (emparejamiento de wallet) **no** se reexporta aquí porque su árbol de
dependencias de transporte no se puede resolver junto al árbol `radix-engine` de la
feature `gateway`. Añade el transporte directamente:

- [`radixdlt-connect`](https://crates.io/crates/radixdlt-connect) — WebRTC, habla con la wallet móvil de Radix.
- [`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh) — QUIC, para peers SDK-a-SDK en Rust puro.

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
