# radixdlt-connect

*[English](README.md) · **Español***

El protocolo **Radix Connect** en Rust nativo: señalización, WebRTC e interacciones de
wallet — un sustituto directo de `@radixdlt/radix-connect-webrtc` + `@roamhq/wrtc`.
Empareja con la **wallet móvil** de Radix, abre un canal WebRTC e intercambia
interacciones de wallet (pruebas de cuenta ROLA, transacciones, pre-autorizaciones).

```toml
[dependencies]
radixdlt-connect = "0.1"
```

### Emparejamiento (QR)

```rust
use radixdlt_connect::{Connector, LinkState};
use std::time::Duration;

let connector = Connector::new(); // conjunto ICE público de Radix; cámbialo con with_ice_servers()
let (wallet_pk, password) = connector
    .pair(&identity_priv_hex, &identity_pub_hex,
          |qr_json| render_qr(&qr_json),      // muestra este QR a la wallet móvil
          Duration::from_secs(120))
    .await?;
```

### Interacciones de wallet

Con una contraseña de enlace ya emparejada, el conector abre un canal WebRTC e
intercambia una interacción. Las respuestas se correlacionan por `interactionId`, así
que las respuestas obsoletas que hayan quedado en la cola de peticiones de la wallet de
intentos anteriores se descartan automáticamente.

```rust
use radixdlt_connect::DappContext;

let ctx = DappContext::new(network_id, dapp_definition, origin);

// Prueba de cuenta ROLA ("iniciar sesión con Radix"):
let response = connector
    .request_account_proof(&password, &challenge, &ctx, false, Duration::from_secs(120))
    .await?;

// Transacción: la wallet firma y envía, devuelve el intent hash:
let txid = connector
    .request_transaction(&password, &manifest, "", &ctx, Duration::from_secs(300))
    .await?;

// Pre-autorización: la wallet firma un subintent SIN enviarlo:
let signed_hex = connector
    .request_pre_authorization(&password, &subintent, "", 600, &ctx, Duration::from_secs(300))
    .await?;
```

### Persistencia del enlace y varios dispositivos (`state::LinkState`)

`LinkState` lee/escribe el mismo `connector.json` que usa el conector de Node, así que
un emparejamiento existente sigue funcionando. Admite **varias wallets emparejadas a la
vez** — cada enlace tiene su propia contraseña (y por tanto su propio `connectionId`),
de modo que un demonio puede alcanzar un dispositivo concreto:

```rust
use radixdlt_connect::state::{Link, LinkState};

let mut state = LinkState::load(path)?;         // migra el `link` único heredado
for link in state.all_links() { /* lista dispositivos; `link.label` es opcional */ }

let pw = state.password_bytes()?;               // primer enlace (flujos de un solo dispositivo)
let pw = state.password_bytes_for(&wallet_pk)?; // un dispositivo concreto

state.add_or_replace_link(Link { /* re-emparejar un dispositivo lo refresca */ .. });
state.remove_link(&wallet_pk);
state.save(path)?;                              // permisos 0600 en Unix
```

Para conexiones peer-to-peer en Rust puro (sin wallet móvil), consulta el transporte
alternativo [`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh).
Ambos transportes comparten el esquema de interacción de
[`radixdlt-connect-types`](https://crates.io/crates/radixdlt-connect-types).

Los mensajes de error se localizan al idioma del sistema.

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
