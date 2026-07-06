# radix-connector-mcp — Arquitectura

*[English](ARCHITECTURE.md) · **Español***

Estado: refleja el código de `crates/connector-mcp` (`main.rs`, `rpc.rs`,
`tools.rs`, `store.rs`, `qr.rs`, `gateway.rs`). Este crate es un **servidor MCP
(Model Context Protocol) local** que permite a agentes de IA (Claude
Code/Desktop, Cursor, Antigravity, …) emparejar una Wallet Radix y obtener
transacciones **firmadas en el propio móvil del usuario** — la clave privada
nunca sale del dispositivo.

---

## 1. Por qué se ejecuta en local

Firmar una transacción Radix exige mantener un canal Radix Connect (WebRTC) vivo
con el móvil durante toda la aprobación, y los secretos del enlace nunca deben
salir de la máquina del usuario. Un backend serverless sin estado no puede
sostener ese canal, así que esta pieza corre en local y habla MCP por **stdio**
con el agente que la lanzó.

---

## 2. Componentes

```mermaid
flowchart LR
    Agent["Agente de IA<br/>(Claude Code/Desktop, Cursor, …)"]
    subgraph MCP["radix-connector-mcp (proceso local)"]
        direction TB
        M["main.rs<br/>transporte stdio:<br/>JSON-RPC 2.0 por líneas"]
        R["rpc.rs<br/>núcleo MCP:<br/>initialize / tools/list / tools/call"]
        T["tools.rs<br/>10 herramientas + dispatch"]
        ST["store.rs<br/>estado connector.json"]
        QR["qr.rs<br/>QR (unicode + PNG)"]
        GW["gateway.rs<br/>lectura HTTP de estado de tx"]
    end
    Conn["radixdlt-connect<br/>(WebRTC + signaling)"]
    Wallet["Wallet Radix (móvil)"]
    Ledger["Radix Gateway (HTTP)"]

    Agent -- "stdin/stdout" --> M --> R --> T
    T --> ST
    T --> QR
    T --> GW --> Ledger
    T --> Conn -- "Radix Connect" --> Wallet
```

- **stdout** transporta solo mensajes de protocolo; todos los logs legibles van a
  **stderr**.
- Todo el servidor corre en un **runtime Tokio monohilo dentro de un `LocalSet`**
  (un canal de wallet a la vez; mantiene locales los futures WebRTC no-`Send`
  mientras un emparejamiento lento corre en segundo plano).
- El estado compartido es un `Rc<App>` con mutabilidad interior `RefCell`; los
  handlers no deben mantener un borrow a través de un `.await`.

---

## 3. Transporte y núcleo MCP

- **Framing (`main.rs`):** lee stdin línea a línea; cada línea de petición
  produce como mucho una línea de respuesta en stdout; las notificaciones no
  producen ninguna; las líneas en blanco se ignoran.
- **MCP (`rpc.rs`):** JSON-RPC 2.0. Atiende `initialize` (negocia versión de
  protocolo — la más nueva `2025-06-18`, también acepta `2025-03-26` /
  `2024-11-05`), `ping`, `tools/list`, `tools/call`. `notifications/*` no
  reciben respuesta. Los errores usan códigos JSON-RPC (`-32700` parseo,
  `-32600` petición inválida, `-32601` método no encontrado).

```mermaid
sequenceDiagram
    autonumber
    participant A as Agente
    participant M as main.rs (stdio)
    participant R as rpc.rs
    participant T as tools.rs

    A->>M: { "method":"initialize", ... }\n
    M->>R: handle_line
    R-->>A: serverInfo + capabilities + instructions
    A-->>R: notifications/initialized  (sin respuesta)
    A->>R: tools/list
    R-->>A: [ pair_wallet, send_transaction, … ]
    A->>R: tools/call { name, arguments }
    R->>T: call(app, name, args)
    T-->>A: ToolResult (content o isError)
```

---

## 4. Conjunto de herramientas (`tools.rs`)

| Herramienta | Propósito |
| --- | --- |
| `pair_wallet` | Inicia el emparejamiento: devuelve un QR a escanear (ejecuta el handshake en segundo plano). |
| `pair_status` | Consulta el resultado del emparejamiento en curso. |
| `list_wallets` | Lista los dispositivos emparejados. |
| `remove_wallet` | Desempareja un dispositivo. |
| `request_accounts` | Pide a la wallet compartir dirección(es) de cuenta, sin prueba. |
| `request_account_proof` | Prueba ROLA "iniciar sesión con Radix". |
| `send_transaction` | Envía un manifiesto para que el usuario firme + envíe. |
| `deploy_package` | Publica un paquete (blobs WASM + RPD), con dry-run previo. |
| `request_pre_authorization` | Hace firmar un subintent (sin enviar). |
| `transaction_status` | Lee el estado de commit de una transacción desde el Gateway. |

El dispatch es un único `match` en `tools::call`; una herramienta desconocida
devuelve un resultado `isError` en vez de un error JSON-RPC, para que el agente
vea un fallo de herramienta.

---

## 5. Flujos clave

### 5.1 Emparejamiento (asíncrono, por sondeo)

`pair_wallet` devuelve el QR **inmediatamente** y ejecuta el handshake Radix
Connect (que bloquea hasta el escaneo) en una tarea de fondo `spawn_local`;
`pair_status` lee el slot de resultado compartido.

```mermaid
sequenceDiagram
    autonumber
    participant A as Agente
    participant T as tools.rs
    participant C as radixdlt-connect
    participant W as Wallet (móvil)

    A->>T: tools/call pair_wallet { label? }
    T->>T: carga/inicia connector.json (identity)
    T->>C: Connector::pair(...) [spawn_local, timeout 600 s]
    C-->>T: carga del QR (vía oneshot, antes de bloquear)
    T-->>A: QR (unicode + PNG) — mostrar al usuario
    W->>C: el usuario escanea el QR → linkClient
    C-->>T: PairOutcome { walletPublicKey, password } → slot de resultado
    A->>T: tools/call pair_status
    T->>T: persiste el Link en connector.json
    T-->>A: emparejado ✓ (walletPublicKey)
```

### 5.2 Firma de una transacción

```mermaid
sequenceDiagram
    autonumber
    participant A as Agente
    participant T as tools.rs
    participant S as store.rs
    participant C as radixdlt-connect
    participant W as Wallet (móvil)
    participant G as Gateway

    A->>T: tools/call send_transaction { manifest, network, wallet? }
    T->>S: password_bytes(_for) desde connector.json
    T->>C: Connector::request_transaction(password, manifest, ctx)
    C->>W: WalletInteraction (WebRTC) → el usuario aprueba + firma + envía
    W-->>C: transactionIntentHash
    C-->>T: hash del intent
    opt confirmar commit
        T->>G: transaction_status(intentHash)
        G-->>T: committedSuccess / pending / failure
    end
    T-->>A: hash del intent (+ estado)
```

`deploy_package` tiene la misma forma con un **dry-run previo al despliegue** y
blobs adjuntos; `request_pre_authorization` devuelve un
`signedPartialTransaction` y **no** envía.

---

## 6. Estado y configuración (`store.rs`)

El estado vive en un `connector.json` bajo el directorio de config del SO,
respetando `RADIX_CONNECTOR_HOME`:

- Linux: `~/.config/radix-connector/connector.json`
- macOS: `~/Library/Application Support/radix-connector/connector.json`
- Windows: `%APPDATA%\radix-connector\connector.json`

Reutiliza `LinkState` de [`radixdlt-connect`](../../connect/docs/PROTOCOL.es.md#7-estado-persistente-del-enlace-staters-connectorjson):
una identidad persistente del connector más un `Link` (password +
`walletPublicKey`) por dispositivo emparejado. `load_or_init` crea una identidad
nueva en el primer arranque.

---

## 7. Notas de seguridad

- **Higiene de stdout:** solo JSON de protocolo en stdout; logs en stderr — un
  print perdido corrompería el stream MCP.
- **Custodia de secretos:** el servidor solo guarda passwords de canal
  (`connector.json`, en `0600`); la clave de firma se queda en el móvil y el
  usuario aprueba cada firma ahí.
- **Red explícita:** cada herramienta de firma exige `mainnet` / `stokenet`
  explícito, para que un manifiesto no se firme contra la red equivocada por
  defecto.
- **Solo local:** la comunicación es stdio con el agente que lo lanza; no hay
  ningún listener de red.
