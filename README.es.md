# RadixDLT Rust SDK

[![CI](https://github.com/genkipool/radixdlt-rust-sdk/actions/workflows/ci.yml/badge.svg)](https://github.com/genkipool/radixdlt-rust-sdk/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/radixdlt-sdk.svg)](https://crates.io/crates/radixdlt-sdk)
[![docs.rs](https://img.shields.io/docsrs/radixdlt-sdk)](https://docs.rs/radixdlt-sdk)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#licencia)

*[English](README.md) · **Español***

Bloques de construcción nativos en Rust para el ledger de [Radix](https://radixdlt.com):
las primitivas *off-ledger* que hasta ahora solo existían en JavaScript/TypeScript.
Crea backends de "iniciar sesión con Radix", herramientas de transacciones e
integraciones de wallet en Rust puro.

## Crates

| Crate | Qué hace |
|---|---|
| [`radixdlt-sdk`](crates/sdk) | Crate paraguas; reexporta los de abajo tras *flags* de features |
| [`radixdlt-rola`](crates/rola) | Autenticación off-ledger ROLA (equivalente a `@radixdlt/rola`) |
| [`radixdlt-address`](crates/address) | Derivación de direcciones de cuenta virtual |
| [`radixdlt-keystore`](crates/keystore) | Keystore Ed25519 cifrado (scrypt + AES-256-GCM) |
| [`radixdlt-gateway-tx`](crates/gateway-tx) | Cliente del Gateway + firma local de transacciones |
| [`radixdlt-connect`](crates/connect) | Radix Connect sobre **WebRTC** (habla con la wallet del móvil) |
| [`radixdlt-connect-iroh`](crates/connect-iroh) | Radix Connect sobre **Iroh/QUIC** (SDK-a-SDK en Rust puro) |
| [`radixdlt-connector-mcp`](crates/connector-mcp) | **Servidor MCP** local (binario): permite a agentes de IA emparejar una wallet y firmar transacciones vía `radixdlt-connect` |
| [`radixdlt-i18n`](crates/i18n) | Detección del idioma del sistema + textos bilingües |

## Inicio rápido

```toml
# Verificar pruebas ROLA (iniciar sesión con Radix):
radixdlt-sdk = "0.1"            # feature por defecto: rola

# Construir y enviar transacciones + gestionar claves:
radixdlt-sdk = { version = "0.1", features = ["full"] }
```

## Notas de diseño

- **Bilingüe en tiempo de ejecución.** Todo el texto de error visible se localiza al
  idioma del sistema (`es*` → español; si no, inglés) mediante `radixdlt-i18n`.
- **Dos transportes, por diseño.** `webrtc` y el árbol de `radix-engine` (que usa
  `gateway`) no se pueden resolver en el mismo binario; tampoco `webrtc` e `iroh`.
  Por eso los transportes son crates separados: elige el que necesite tu herramienta.
- **Los agentes de IA pueden firmar.** `radixdlt-connector-mcp` es un servidor MCP
  local (stdio) que empareja una Radix Wallet y consigue que las transacciones se
  firmen en la máquina del usuario: el móvil aprueba y la clave privada nunca sale de
  él. Se instala desde GitHub (`cargo install --git …` o los scripts de
  [`scripts/`](scripts)); consulta su [README](crates/connector-mcp) para las
  herramientas y la configuración.
- **Workspaces.** `radixdlt-connect`, `radixdlt-connect-iroh` y `radixdlt-connector-mcp`
  son workspaces aislados (árboles de dependencias pesados de WebRTC / QUIC); el resto
  comparten el workspace principal.

## Autor

Creado y mantenido por **Luis Alberto Reoyo Bolaños**
([genkipool](https://github.com/genkipool)). Las contribuciones son bienvenidas —
consulta [CONTRIBUTING.md](CONTRIBUTING.md).

## Licencia

Publicado bajo [MIT](LICENSE-MIT) o [Apache-2.0](LICENSE-APACHE), a tu elección.
