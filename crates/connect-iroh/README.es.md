# radixdlt-connect-iroh

*[English](README.md) · **Español***

Un transporte peer-to-peer [iroh](https://iroh.computer) (QUIC) para el RadixDLT Rust
SDK — una alternativa a WebRTC para conexiones **SDK-a-SDK en Rust puro**.

> Esto **no** habla con la wallet móvil de Radix (que solo habla Radix Connect sobre
> WebRTC; usa [`radixdlt-connect`](https://crates.io/crates/radixdlt-connect) para eso).
> Conecta dos peers que ambos usan el SDK — p. ej. un firmante de escritorio, un
> servidor o un dispositivo — para que flujos como "iniciar sesión con Radix" (ROLA)
> puedan ejecutarse enteramente en Rust, sin móvil.

```toml
[dependencies]
radixdlt-connect-iroh = "0.1"
```

### Transporte de bajo nivel

```rust
use radixdlt_connect_iroh::IrohConnector;

let endpoint = IrohConnector::bind().await?;
let mut channel = endpoint.connect(peer_addr).await?;
channel.send_message(&request).await?;
let response = channel.recv_message().await?;
```

### "Iniciar sesión con Radix" (ROLA) de alto nivel, emparejado por ticket

```rust
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_connect_iroh::protocol::{Wallet, request_account_proof, DappContext};

// Lado firmante (una "wallet" en Rust puro):
let wallet = Wallet::from_key_file(&key_file, passphrase)?;
let signer = IrohConnector::bind().await?;
let ticket = signer.ticket();             // compártelo como QR / cadena
let mut ch = signer.accept().await?;
wallet.answer(&mut ch).await?;            // firma el desafío ROLA

// Lado dApp:
let dapp = IrohConnector::bind().await?;
let mut ch = dapp.connect_to_ticket(&ticket).await?;
let proof = request_account_proof(&mut ch, &challenge, &ctx).await?; // enviado + verificado
```

La `Wallet` responde a tres interacciones sobre iroh, igual que el flujo WebRTC:

- **prueba de cuenta** — "iniciar sesión con Radix" ROLA (`request_account_proof`).
- **transacción** — la wallet firma *y* envía un manifiesto, devuelve el intent hash
  (`request_transaction`).
- **pre-autorización** — la wallet firma un subintent sin enviarlo, devuelve la
  transacción parcial firmada (`request_pre_authorization`).

### Identidad persistente y peers por internet (relay + discovery)

`bind()` usa una identidad efímera y solo conexiones directas (mismo host / LAN). Para
peers que deban ser alcanzables entre reinicios o por internet:

```rust
use radixdlt_connect_iroh::{endpoint_id_from_seed, IrohConnector, Relay};

// Hub: semilla fija de 32 bytes → EndpointId estable, relays n0 + discovery activados.
let hub = IrohConnector::bind_with(Some(seed), Relay::Enabled).await?;
let ticket = hub.id_ticket();             // estable entre reinicios (solo id, sin addrs)

// Peer: enlaza también con Relay::Enabled, luego marca por ticket o por EndpointId crudo.
let peer = IrohConnector::bind_with(None, Relay::Enabled).await?;
let mut ch = peer.connect_to_ticket(&ticket).await?;
// …o, si el id se aprendió por mDNS/discovery:
// let mut ch = peer.connect_to_endpoint_id(&hub_id).await?;
```

`endpoint_id_from_seed(&seed)` deriva la misma cadena `EndpointId` sin conexión (sin
enlazar un endpoint), útil para imprimir o distribuir un localizador del hub por
adelantado.

Consulta `examples/login.rs` (`cargo run --example login`) para el flujo completo, y
`tests/` para las pruebas de bajo nivel, login y transacción/pre-auth. Los mensajes de
error se localizan al idioma del sistema.

El protocolo de cable (transporte, framing, emparejamiento, esquema de mensajes y
diagramas de secuencia por interacción) está especificado en
[`docs/PROTOCOL.es.md`](docs/PROTOCOL.es.md) ([English](docs/PROTOCOL.md)).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
