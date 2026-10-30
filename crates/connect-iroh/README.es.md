# radixdlt-connect-iroh

Un transporte [iroh](https://iroh.computer) (QUIC) para el RadixDLT Rust SDK, alternativa
a WebRTC con dos usos:

- **La wallet móvil de Radix, directamente** — [`MobileWalletConnector`](src/mobile/mod.rs)
  habla *Radix Connect over iroh v1*, el protocolo que hablan la Radix Wallet y la
  Radix Connector Extension. Un servidor en Rust se empareja con el móvil mediante el
  QR de siempre y le pide inicios de sesión, pruebas de cuenta, transacciones y
  pre-autorizaciones — sin navegador ni extensión.
- **SDK-a-SDK** — dos peers que usan el SDK (un firmante de escritorio, un servidor, un
  dispositivo) intercambian mensajes JSON acotados y confirmados, y ejecutan flujos como
  "iniciar sesión con Radix" (ROLA) enteramente en Rust, con un protocolo propio
  (`IrohConnector`, `protocol`). La wallet móvil no habla ese.

*[English](README.md) · **Español***

```toml
[dependencies]
radixdlt-connect-iroh = "0.3"
```

Requiere Rust 1.91 (iroh 1.4).

## La wallet móvil de Radix sobre iroh

### En el móvil

1. **Settings → Preferences → Radix Connect → Automatic** (o **iroh**). En Automatic la
   wallet enlaza por iroh y WebRTC a la vez y mantiene iroh en los enlaces donde
   funciona; en iroh no usa nada más.
2. Su ajuste de **relé iroh** debe coincidir con la elección de tu servidor (ver
   [Relés](#relés)): por defecto ambos usan los relés públicos de n0, así que déjalo en su
   valor por defecto.
3. La wallet solo se conecta mientras su app está abierta: las peticiones esperan (dentro
   de su timeout) a que la persona la abra, e informan de que esperan.

### Emparejamiento

```rust
use std::time::Duration;
use radixdlt_connect_iroh::mobile::{linking, MobileWalletConnector};

// Relés públicos de n0: nada que ejecutar ni configurar, en ningún extremo.
let connector = MobileWalletConnector::new();
// …o un relé propio, con la misma URL configurada en la wallet:
// let connector = MobileWalletConnector::new().with_relay_url("https://relay.example.com")?;

// La identidad a largo plazo del conector: genérala una vez y consérvala (es el nombre
// del conector en la wallet).
let (identity_private, identity_public) = linking::generate_identity()?;

let (wallet_public_key, password) = connector
    .pair(&identity_private, &identity_public, |qr_json| {
        // Dibújalo (la feature `qr` de radixdlt-connect-types lo pinta en una terminal), y
        // nunca imprimas ni registres el JSON: lleva la contraseña del enlace.
        show_qr(&qr_json);
    }, Duration::from_secs(300))
    .await?;
// Guarda `password` como una credencial (ver Seguridad). La sesión iniciada para el
// emparejamiento se conserva: la wallet se reconecta con su sesión normal al mismo endpoint.
```

La persona escanea el QR desde **Settings → Linked Connectors → Link New
Connector**. El QR es el mismo JSON que en WebRTC; la firma del `linkClient` de la wallet se
verifica (y es obligatoria) antes de que `pair` vuelva. La wallet enlaza por una conexión
propia, que descarta cualquier otra cosa que se envíe por ella, así que `pair` solo vuelve
cuando la wallet la ha cerrado (como mucho 5 s después): una petición enviada justo después
del emparejamiento va por la conexión normal de la wallet. Un enlace emparejado aquí también
funciona con `radixdlt-connect` (WebRTC) cuando la wallet está en Automatic.

### Peticiones

La API refleja `radixdlt_connect::Connector` método a método — cambiar de transporte es
cambiar el tipo:

```rust
use radixdlt_connect_iroh::mobile::{extract_login, extract_proofs, DappContext, PersonaRequest};

connector.open_link(&password).await?;    // al arrancar, para cada enlace guardado (ver abajo)

let ctx = DappContext::new(2, dapp_definition, "https://my.app");
let answer = connector
    .request_login(&password, &challenge_hex, &ctx, PersonaRequest::NONE, Duration::from_secs(120))
    .await?;
let (identity, label, persona_proof) = extract_login(&answer)?;  // verifícalo con radixdlt-rola
let accounts = extract_proofs(&answer)?;                         // mismo desafío
```

También `request_account_proof(_sharing)`, `request_authorized`, `request_ownership`,
`request_unauthorized`, `request_accounts`, `request_transaction` (→ intent hash) y
`request_pre_authorization` (→ transacción parcial firmada), todos sobre
`radixdlt-connect-types`. `exchange` envía cualquier interacción e informa de cada paso
(`Progress`: esperando a la wallet, conectada, entregada, reenviando, sin confirmar,
desconectada, otros mensajes); `await_response` recoge la respuesta a una petición enviada
antes; `probe` comprueba que la wallet está accesible; `subscribe` entrega los mensajes que
ninguna petición reclama (el `accountList` de la wallet, respuestas tardías).

Cómo se comporta:

- **Una sesión por enlace**, iniciada en el primer uso y mantenida hasta `close_link`,
  `close` o drop. La wallet solo marca mientras una sesión escucha, y vuelve a marcar con
  una espera de hasta 15 s, así que llama a `open_link` para cada enlace guardado al
  arrancar el servidor.
- **Las respuestas se emparejan por `interactionId`**: puede haber varias peticiones en
  curso en un enlace, desde cualquier número de tareas (el conector es `Send + Sync`;
  compártelo en un `Arc`).
- **Nada que pueda haber llegado a la wallet se envía dos veces.** Una petición solo se
  reenvía cuando la sesión indica que no se envió nada (la wallet cortó la conexión justo
  antes). Una vez confirmada — o si su confirmación expiró o se perdió con la conexión — la
  llamada solo espera la respuesta, también entre reconexiones de la wallet.
- **Las respuestas tardías se guardan.** Una respuesta que llega después de que su petición
  expirase aquí (la persona aprobó más tarde) se guarda, acotada, para `await_response` — o
  para repetir el `exchange` de la misma interacción, que entonces la devuelve sin enviar
  nada.
- **Toda espera está acotada** por el timeout que pasa quien llama; los errores dicen hasta
  dónde llegó la petición (`MobileError` documenta cuáles se pueden reenviar sin riesgo).

### Relés

Un relé reenvía paquetes QUIC cifrados de extremo a extremo entre los dos dispositivos y les
ayuda a abrir un camino directo cuando la red lo permite. Hay dos opciones, y **los dos
extremos de un enlace deben elegir la misma** (`Relays`):

- **Relés públicos — la opción por defecto** (`MobileWalletConnector::new()`, y el ajuste
  por defecto de la wallet). Los relés públicos de n0 (Norteamérica, Europa, Asia-Pacífico):
  cada extremo se conecta al más cercano y pasa a otro si falla. Este conector publica qué
  relé usa — solo la URL del relé, nunca una dirección IP — en el servicio de búsqueda de
  direcciones de n0 (pkarr/DNS), bajo su id de endpoint; la wallet lo busca ahí. Nada que
  alojar ni configurar, en desarrollo ni en producción.
- **Un relé propio — opcional** (`with_relay_url(url)`, y la misma URL en el ajuste de relé
  iroh de la wallet). Normalmente tu propio [`iroh-relay`](https://crates.io/crates/iroh-relay)
  por HTTPS: controlas su disponibilidad y a quién sirve, y no se publica nada en ningún
  sitio. `http://` sin cifrar solo se acepta en direcciones de loopback, privadas y
  link-local (un relé en tu máquina o tu red local, para desarrollo).

Si un extremo usa los relés públicos y el otro uno propio (o dos relés propios distintos), la
wallet nunca se conecta: las peticiones terminan en `WalletUnreachable`, cuyo diagnóstico
muestra `public` solo en un lado.

### Diagnóstico y logs

Cada error de timeout lleva una instantánea de la conectividad del enlace
(`MobileError::diagnostics()`), y `connector.diagnostics(&password)` toma una en cualquier
momento; los avisos de estado de `Progress` también llevan una. Su `Display` es una línea,
p. ej.:

```text
connector efcc89cfbf → 4d99dc649e state=connected path=relay rtt=42ms relay=https://euc1-1.relay.n0.iroh.link./ (connected, public)
```

| Lo que ves | Lo que suele significar |
|---|---|
| `relay … (NOT connected)` o `relay=none yet` (más de unos segundos) | Este servidor no alcanza ningún relé: sin red, relé caído o bloqueado, o URL errónea. Nada puede conectar. |
| `state=disconnected`, relé conectado, `WalletUnreachable` | El móvil no está: app cerrada, iroh desactivado para el enlace, otra elección de relé, o no enlazado. |
| `last_error="… unauthorized"` | Los dos extremos tienen contraseñas distintas para el enlace: vuelve a emparejar. |
| `last_error="… replaced by a newer connection"` | Inofensivo: la wallet se reconectó (cambio de red, reinicio de la app). |
| `path=relay` | Normal tras NATs estrictos; sigue cifrado de extremo a extremo. |

El conector registra con [`tracing`](https://docs.rs/tracing) y campos estructurados:
`RUST_LOG=radix_connect_iroh=debug,radixdlt_connect_iroh=debug` muestra los pasos del
emparejamiento, cada cambio de conexión (con su línea de diagnóstico), envíos, reenvíos,
timeouts y las explicaciones del propio protocolo. Los enlaces aparecen como un hash corto
de la contraseña, los mensajes por su tamaño e id de interacción — nunca la contraseña ni
el contenido de un mensaje.

### Seguridad

- **La contraseña del enlace es el enlace.** Quien la tenga puede hablar con la wallet como
  este conector (y derivar las dos claves de endpoint iroh). Guárdala como una credencial —
  un fichero legible solo por la cuenta del servicio (`0600`), o un gestor de secretos — y
  nunca la registres. El conector nunca la registra, y ningún error ni línea de diagnóstico
  la contiene.
- **La clave privada de identidad** da nombre al conector en la wallet. Guárdala con el
  mismo cuidado, y reutilízala en cada emparejamiento.
- **Solo se admite a la wallet del enlace**: los dos ids de endpoint se derivan de la
  contraseña, y la sesión cierra cualquier otro peer. La firma del `linkClient` de la wallet
  sobre la contraseña es obligatoria y se verifica al emparejar.
- **Los relés no ven ni los mensajes ni la contraseña** — solo los dos ids de endpoint y el
  ritmo y tamaño de paquetes cifrados. Un relé malicioso puede descartar tráfico, no leerlo
  ni falsificarlo. Por HTTP sin cifrar (solo relés locales) un observador ve también los ids.
- **Qué publican los relés públicos.** Con la opción por defecto, este conector publica, bajo
  su id de endpoint, la URL del relé que usa — ninguna dirección IP, nada sobre la wallet. El
  id se deriva de la contraseña del enlace (distinto en cada enlace, imposible de relacionar,
  y no revela nada de la contraseña). Un relé propio no publica nada.
- **Verifica lo que devuelve la wallet.** Las pruebas ROLA se comprueban con
  `radixdlt-rola` contra un desafío nuevo, tu dApp definition y tu origen — el conector
  transporta las respuestas, no responde de ellas.

### Ejemplo

[`examples/mobile_login.rs`](examples/mobile_login.rs) empareja (QR en la terminal), guarda
el enlace en `~/.config/radixdlt-connect-iroh/mobile-link.json` (modo `0600`; la contraseña
nunca se imprime), y después pide a la wallet iniciar sesión y verifica las pruebas de
persona y de cuenta:

```bash
cargo run --example mobile_login                                       # relés públicos
cargo run --example mobile_login -- --relay https://relay.example.com  # un relé propio
RUST_LOG=radix_connect_iroh=debug,radixdlt_connect_iroh=debug cargo run --example mobile_login
```

Las pruebas de `tests/mobile_wallet_over_iroh.rs` ejecutan el conector contra una wallet
simulada (construida sobre la misma biblioteca de protocolo que la real) a través de un
relé dentro del proceso.

> **Nota de compilación.** La biblioteca del protocolo, `radix-connect-iroh`, es una
> dependencia por ruta a su repositorio hermano (`../../../../radix-connect-iroh`) mientras
> ambos se desarrollan juntos: clónalo junto a este repositorio para compilar. Pasará a ser
> una dependencia de crates.io (o git) antes de publicar esta crate.

## SDK-a-SDK (peers en Rust puro)

Dos peers que usan este crate intercambian mensajes JSON sobre iroh con un protocolo propio
(ALPN `radixdlt-connect-iroh/1`, especificado en [`docs/PROTOCOL.es.md`](docs/PROTOCOL.es.md)).

### El canal

```rust
use radixdlt_connect_iroh::IrohConnector;

let endpoint = IrohConnector::bind().await?;
let mut channel = endpoint.connect_to_ticket(&ticket).await?;
channel.send_message(&request).await?;    // vuelve cuando el peer lo ha RECIBIDO
let response = channel.recv_message().await?;
println!("hablando con {}", channel.remote_id()); // el EndpointId autenticado del peer
channel.close().await;                    // acotado
endpoint.close().await;                   // acotado
```

- **Cada mensaje viaja en un stream QUIC propio y el receptor lo confirma.** `send_message`
  devuelve `Ok` cuando el peer ha recibido el mensaje, así que cerrar justo después no pierde
  nada; ya no hace falta el baile de `finish`/`wait_closed`.
- **Acotado.** Un mensaje mayor que `Limits::max_message_size` (8 MiB por defecto) se rechaza
  — uno entrante, solo por su prefijo de longitud, antes de reservar nada — y el canal sigue
  usable. Una vez que el peer ha empezado un mensaje, este debe llegar entero en
  `Limits::message_timeout` (60 s): un peer que envía bytes con cuentagotas no puede retener una
  tarea para siempre. Esperar el SIGUIENTE mensaje no está acotado, así que un canal puede
  quedarse abierto e inactivo; envuelve `recv_message` en un timeout propio donde lo quieras.
- **Seguro ante cancelación.** `send_message` y `recv_message` se pueden envolver en
  `tokio::time::timeout` o `select!`: una recepción cancelada se reanuda donde se quedó; un envío
  cancelado se abandona entero (el peer nunca ve una parte). Ninguno puede desincronizar el
  canal.
- **Cierre.** `IrohChannel::close` e `IrohConnector::close` esperan como mucho
  `Limits::close_timeout` (5 s). Si se sueltan (drop) sin `close`, ambos se cierran igualmente
  en segundo plano cuando hay un runtime de tokio.

### Quién puede conectarse

`IrohChannel::remote_id()` es el `EndpointId` del peer, probado por el handshake QUIC (TLS 1.3):
marcar un id (o un ticket) llega a quien tiene esa clave o falla. Por defecto cualquiera puede
conectarse a un endpoint — a una aplicación que autentica a sus peers por su cuenta (como
PamAuthority, con ROLA) no le hace falta más. Para admitir solo peers conocidos antes de que la
aplicación los vea:

```rust
use radixdlt_connect_iroh::{IrohConnector, Limits, Relay};

let hub = IrohConnector::builder()
    .secret(hub_seed)                     // una clave solo para este endpoint (ver Identidad)
    .relay(Relay::Enabled)
    .allow_only([peer_a, peer_b])         // o .admit(|id| registro.contains(id))
    .limits(Limits::default().with_max_message_size(256 * 1024))
    .bind()
    .await?;
```

Un peer no admitido se rechaza justo después de su handshake; `Pending::ready` lo devuelve como
error.

### "Iniciar sesión con Radix" (ROLA) de alto nivel, emparejado por ticket

`protocol::Wallet` es un firmante en Rust puro: guarda la clave de una cuenta y responde a
interacciones de wallet. **No firma nada por su cuenta**: cada petición pasa por un `Approver`,
que ve quién pide (el `EndpointId` del peer, y la dApp y el origen que la petición *dice*) y
exactamente qué pide. Cada tipo de petición tiene su propio método, y cada uno rechaza salvo que
se implemente — un aprobador escrito para inicios de sesión no puede aprobar una transacción por
omisión. Las peticiones de otra red, los retos ROLA que no son de 32 bytes, los manifiestos que
no compilan y las pre-autorizaciones sin caducidad se rechazan antes de preguntar al aprobador.
Un rechazo le llega a la dApp como `rejectedByUser`.

```rust
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_connect_iroh::protocol::{
    request_account_proof, AccountProofRequest, Approver, DappContext, Requester, Wallet,
};

// Firmante: inicios de sesión en una dApp, del peer con el que se emparejó — nada más.
struct PairedLogins { peer: radixdlt_connect_iroh::EndpointId }
impl Approver for PairedLogins {
    async fn approve_account_proof(&self, from: &Requester, req: &AccountProofRequest) -> bool {
        from.peer == self.peer && req.dapp_definition == MI_DAPP && req.origin == MI_ORIGEN
    }
}
let wallet = Wallet::from_key_file(&key_file, passphrase)?;
let approver = PairedLogins { peer: dapp_id }; // el EndpointId de la dApp, del emparejamiento
let signer = IrohConnector::builder().allow_only([dapp_id]).bind().await?;
let ticket = signer.ticket();              // compártelo como QR / texto
let mut ch = signer.accept().await?;
wallet.answer(&mut ch, &approver).await?;

// Lado dApp:
let dapp = IrohConnector::bind().await?;
let mut ch = dapp.connect_to_ticket(&ticket).await?;
let proof = request_account_proof(&mut ch, &challenge, &ctx).await?; // enviada + verificada
```

El `Wallet` responde a tres interacciones, como en el flujo WebRTC:

- **prueba de cuenta** — ROLA "iniciar sesión con Radix" (`request_account_proof`).
- **transacción** — la wallet firma *y envía* un manifiesto, pagando sus comisiones
  (`request_transaction`). La dApp recibe el hash del intent solo cuando la transacción se ha
  confirmado con éxito en el ledger; un commit fallido, un rechazo o un resultado aún
  desconocido es un `failure` que nombra el estado y el hash del intent.
- **pre-autorización** — la wallet firma un subintent sin enviarlo
  (`request_pre_authorization`); quien lo tenga puede completarlo y enviarlo hasta que caduque.

`Wallet::new` envía por el Gateway público de su red (1 mainnet, 2 stokenet; cualquier otra
red es un error) — `with_gateway` lo cambia por otro de la misma red.

> **No expongas un `Wallet` a quien pueda alcanzarlo.** Cualquiera que abra un canal puede
> enviarle peticiones; solo el aprobador se interpone entre ellas y la clave. Empárejalo con
> peers conocidos (`allow_only`) y nunca respondas con un aprobador que lo aprueba todo.

### Tickets, identidad y alcance

| Localizador | Contenido | Alcanza |
| --- | --- | --- |
| `ticket()` | `EndpointId` + las direcciones que iroh conoce: las de las interfaces de la máquina (LAN), loopback y, con `Relay::Enabled`, su relé | el mismo host, la misma LAN; con `Relay::Enabled` en ambos extremos, internet |
| `id_ticket()` / `endpoint_id_string()` | solo el `EndpointId`, estable con una identidad persistente | necesita `Relay::Enabled` en ambos extremos: iroh busca el id en el pkarr/DNS de n0 |

`Relay::Disabled` (el de `bind()`) son conexiones directas solamente: sin relé, sin búsqueda de
direcciones y sin mapeo de puertos. `Relay::Enabled` usa los relés públicos y la búsqueda de
direcciones de n0 (la URL del relé de este endpoint se publica bajo su `EndpointId`), y el mapeo
de puertos (UPnP / NAT-PMP / PCP) puede pedir al router local que abra un puerto para conexiones
directas. Este crate no tiene mDNS: una aplicación que quiera descubrimiento en la LAN anuncia un
ticket por su cuenta.

Un ticket lista todas las direcciones de interfaz de la máquina (VPN y puentes de contenedores
incluidos): compártelo como cualquier otro dato de red.

**Identidad.** `bind()` usa una clave efímera; `Builder::secret(seed)` (o
`bind_with(Some(seed), …)`) una persistente, así que el `EndpointId` y el `id_ticket` sobreviven a
los reinicios, y `endpoint_id_from_seed(&seed)` deriva el id sin red. Usa una clave dedicada al
endpoint, no la de una cuenta de Radix: el `EndpointId` es público (lo ven los peers y los relés,
y con `Relay::Enabled` se publica junto al relé de este endpoint), así que reutilizar la clave de
una cuenta ata esa cuenta a la presencia en red de la máquina; y una misma clave firmaría en dos
protocolos (el handshake QUIC y las transacciones de Radix). PamAuthority hace a propósito que el
`EndpointId` de su *servidor* sea su clave de firma, para que las máquinas que lo marcan ya tengan
la clave que firma lo que les envía: es una identidad pública de servidor por diseño, no la cuenta
de una persona, y el compromiso está documentado allí.

Ver `examples/login.rs` (`cargo run --example login`) para el flujo completo, y `tests/` para las
pruebas del canal, del inicio de sesión y de transacción/pre-autorización. Los mensajes de error
salen en el idioma del sistema.

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
