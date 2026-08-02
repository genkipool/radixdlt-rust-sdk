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

### Redes que bloquean UDP (`turn_tcp`)

WebRTC quiere UDP, y hay muchos sitios que no lo dan: cortafuegos corporativos,
wifis de invitados y casi todas las plataformas serverless. Este crate puede llevar
la interacción entera por **TURN sobre TCP/TLS 443**, sin abrir un solo socket UDP:

```rust
use radixdlt_connect::{Connector, TurnTcpServer};

let relay = TurnTcpServer::parse(
    "turns:relay.example.com:443?transport=tcp",
    "usuario",
    "credencial",
)?;
let response = Connector::new()
    .with_turn_tcp(relay)
    .request_account_proof(&password, &challenge, &ctx, true, timeout)
    .await?;
```

Sustituye a `with_ice_servers` y `with_relay_only`: la asignación queda *por debajo*
de la conexión, haciendo de socket suyo, así que a ICE no le queda nada que recoger.
Cuenta con que sea más lento que un camino directo y con que cada byte pase por el
relay — úsalo cuando no haya UDP, no por defecto.

`probe_relay_candidates(&relay, wait)` hace la asignación e informa de los candidatos
que se ofrecerían, sin que intervenga ninguna wallet. Merece la pena contra un relay
nuevo: uno que autentica pero anuncia una dirección inalcanzable falla exactamente
igual que uno caído, como un canal que nunca abre.

> **Esto no existe en ningún otro sitio del ecosistema Rust.** `webrtc-ice` deja TCP
> y TURNS como un `TODO` sin implementar en `gather_candidates_relay` (sigue así en
> 0.17.2), y `webrtc` 0.20 descarta con un aviso cualquier URL `turns:` o que no sea
> UDP. O sea que una entrada `turns:…?transport=tcp` en una configuración ICE no hace
> absolutamente nada — incluida la del propio `radix_default_ice_servers` de este
> crate, que se conserva por compatibilidad. Si dependes de un relay TCP, usa
> `with_turn_tcp`.

Para conexiones peer-to-peer en Rust puro (sin wallet móvil), consulta el transporte
alternativo [`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh).
Ambos transportes comparten el esquema de interacción de
[`radixdlt-connect-types`](https://crates.io/crates/radixdlt-connect-types).

Los mensajes de error se localizan al idioma del sistema.

## Especificación del protocolo

El protocolo de cable completo de Radix Connect (signaling, cripto AES-256-GCM,
data channel WebRTC, chunking de mensajes, estado del enlace `connector.json`)
con diagramas de secuencia está en [`docs/PROTOCOL.es.md`](docs/PROTOCOL.es.md)
([English](docs/PROTOCOL.md)).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
