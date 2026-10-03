# radixdlt-connector-mcp

*[English](README.md) · **Español***

Un servidor MCP (Model Context Protocol) **local** que permite a los agentes de IA
(Claude Code/Desktop, Antigravity, Cursor, …) emparejar una Radix Wallet y conseguir
que las transacciones se **firmen en la propia máquina del usuario** — la wallet del
móvil aprueba y la clave privada nunca sale de él.

## Por qué un binario local (y no el MCP web)

Firmar una transacción de Radix implica mantener abierto un canal Radix Connect
(WebRTC) con el móvil durante toda la aprobación. Un backend serverless sin estado (el
portal web en Vercel) no puede hacerlo, y los secretos del enlace nunca deben tocar un
servidor. Por eso esta pieza corre en local y habla MCP por **stdio** con el agente que
la lanzó. El MCP HTTP del portal web sigue haciendo todo lo de solo lectura (docs,
ledger, construir y previsualizar manifiestos); este binario añade el paso de firma.

El comando instalado es `radix-connector-mcp`.

## Instalación (desde GitHub — sin crates.io / npm)

**Con Rust (cualquier SO):**

```sh
cargo install --git https://github.com/genkipool/radixdlt-rust-sdk radixdlt-connector-mcp
```

El binario queda en `~/.cargo/bin/radix-connector-mcp`.

**Binario precompilado, Linux/macOS:**

```sh
curl -fsSL https://raw.githubusercontent.com/genkipool/radixdlt-rust-sdk/main/scripts/install-connector.sh | sh
```

**Binario precompilado, Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/genkipool/radixdlt-rust-sdk/main/scripts/install-connector.ps1 | iex
```

## Actualizar

```sh
radix-connector-mcp check-update   # código de salida 10 si hay una versión nueva, 0 si está al día
radix-connector-mcp update         # descarga, verifica (SHA-256 y que arranca), guarda copia y sustituye
radix-connector-mcp update --tag connector-v0.4.0   # una versión concreta (también para volver atrás)
```

Un agente puede hacer lo mismo con las herramientas `check_update` / `update_connector`.
**Actualizar nunca obliga a emparejar otra vez el móvil**: el emparejamiento vive en
`connector.json` del directorio de configuración y una actualización solo sustituye el binario.
Reinicia después el cliente MCP para que arranque la versión nueva.

## Registrar en un cliente MCP

Claude Code:

```sh
claude mcp add radix-connector -- radix-connector-mcp
```

Configuración JSON genérica (Claude Desktop / Antigravity / Cursor):

```json
{
  "mcpServers": {
    "radix-connector": { "command": "radix-connector-mcp" }
  }
}
```

Si el binario no está en tu `PATH`, usa su ruta absoluta como `command`.

## Herramientas

| Herramienta | Qué hace |
|---|---|
| `pair_wallet` | Devuelve un QR (arte de terminal + PNG + payload crudo) para enlazar una wallet. Una vez por dispositivo. |
| `pair_status` | Espera el escaneo/aprobación y guarda el enlace. |
| `list_wallets` / `remove_wallet` | Gestiona los dispositivos emparejados. |
| `request_accounts` | Pide a la wallet que **comparta su(s) dirección(es) de cuenta** — sin firma/prueba. Útil para saber qué cuenta fondear o desde cuál transferir. |
| `send_transaction` | Envía un manifiesto para firmar **y enviar**; devuelve el intent hash. Admite `blobs` (hex en línea) y `blob_files` (rutas locales). |
| `deploy_package` | Publica un paquete Scrypto: lee el `.wasm` de una ruta local, **hace dry-run en el Gateway primero** (aborta si fallaría), lo adjunta como blob, firma y envía. |
| `request_pre_authorization` | Firma un subintent (pre-autorización V2) sin enviarlo. |
| `request_account_proof` | "Iniciar sesión con Radix" (ROLA) con una cuenta; verifica la prueba localmente. |
| `request_login` | Inicia sesión con una **persona** (petición autorizada): con reto, la persona lo firma y la prueba se verifica aquí; con `without_challenge` solo se nombra. Puede pedir cuentas (con prueba) y datos de la persona en la misma aprobación. |
| `request_ownership_proof` | Demuestra la propiedad de cuentas **exactas** (y de la persona) como una persona que ya inició sesión: nada que elegir, una confirmación. |
| `request_authorized` | Todo el vocabulario autorizado: `login` / `login_without_challenge` / `use_persona`, `reset`, prueba de propiedad, cuentas y datos **puntuales o continuos**. |
| `request_data` | Petición no autorizada: cuentas en cantidad **exacta o mínima** (con prueba opcional) y datos de la persona — nombre, correos, **teléfonos**. |
| `pending_requests` | Qué puede seguir esperando en la cola de la wallet, y qué hacer. |
| `await_response` | Recoge la respuesta a una petición anterior **sin volver a enviarla**. |
| `cancel_request` | Detiene una petición aquí y deja de bloquear las nuevas (la wallet no permite retirar una a distancia: el resultado dice si sigue en el móvil). |
| `check_wallet_connection` | Abre y cierra un canal sin enviar nada: ¿se llega ahora a la app? |
| `check_dapp_identity` | Hace la misma verificación de la dApp que la wallet (dApp definition ↔ origin ↔ `radix.json`). |
| `connector_log` | La traza del conector de cada petición, paso a paso. |
| `transaction_status` | Lee el estado de commit de una transacción desde el Gateway. |

### Hablar con un móvil: entrega, cola y fallos

La Radix Wallet muestra **una petición cada vez**, desde una cola en memoria, y nada que envíe
una dApp puede retirar una: sale de la cola cuando la persona aprueba o rechaza, o cuando se
cierra la app. El conector está hecho alrededor de eso:

- **Sabe si a la wallet le llegó.** Cada petición se registra paso a paso (canal abierto →
  *entregada*, es decir, la wallet confirmó la recepción → respondida). Una que la wallet no
  confirmó se vuelve a enviar, con el mismo id, por un canal nuevo (hasta 3 intentos); una
  confirmada nunca se envía dos veces.
- **No inunda la cola.** Mientras una petición entregada no tiene respuesta, las nuevas a esa
  wallet se rechazan con `PENDING_IN_WALLET` (`ignore_pending: true` para forzar).
  `await_response` recoge una respuesta tardía y `cancel_request` la despeja.
- **Espera a que la wallet suelte el canal.** La wallet guarda un canal por enlace y, unos 5 s
  después de cerrarse una conexión, cierra el canal que tenga el enlace en ese momento: una
  petición enviada justo después de otra podía llegar al móvil y perder su respuesta. El
  conector espera 8 s tras cerrar un canal antes de abrir otro en el mismo enlace (también
  entre procesos), y reabre un canal perdido tras la entrega para seguir esperando.
- **Comprueba antes la identidad de la dApp.** Si `{origin}/.well-known/radix.json` no lista la
  dApp definition, la wallet descarta la petición *sin responder*; el conector se niega a
  enviarla (`DAPP_NOT_VERIFIED`) en vez de agotar el tiempo.
- **Los fallos dicen qué hacer.** Cada uno lleva un `code` (`WALLET_UNREACHABLE`,
  `NOT_DELIVERED`, `NO_ANSWER`, `REJECTED_BY_USER`, `WRONG_NETWORK`, `INVALID_REQUEST`, …), una
  `stage`, si reenviar es seguro (`retry_safe`), una pista y el id de la interacción — en texto
  y como `structuredContent`. Las respuestas tardías a peticiones anteriores (p. ej. una
  transacción aprobada después de agotarse su llamada) se informan, no se pierden.
  `connector_log` traza cualquier petición.

Las llamadas a herramientas se atienden en paralelo: `cancel_request` y `pending_requests`
responden mientras otra llamada espera al móvil, y `notifications/cancelled` del cliente detiene
la llamada que nombra.

Cada herramienta de firma requiere una `network` explícita (`"mainnet"` o
`"stokenet"`) — no hay valor por defecto, a propósito.

## Identidad de la dApp (variables de entorno)

Cuando la wallet firma, muestra **qué dApp** lo está pidiendo. Esa identidad es un
par de valores — la dirección de la dApp definition y el origin — que deben
coincidir con el `claimed_websites` / la dApp definition registrada on-chain, y el
`/.well-known/radix.json` del origin debe listarla. Si no, la wallet (sin modo
desarrollador) rechaza la petición — o, si `radix.json` carga pero no la lista, la
descarta sin responder. El conector lo comprueba todo antes de enviar
(`check_dapp_identity`; `skip_dapp_check: true` para una wallet en modo desarrollador).

Puedes pasarlos por llamada (`dapp_definition`, `origin` en las herramientas de
firma), pero es más robusto configurarlos **una vez** para que el conector los
rellene cuando una llamada los omita. La precedencia es **argumento de la llamada
→ variable de entorno → valor por defecto integrado**.

| Variable | La usan | Valor por defecto |
|---|---|---|
| `RADIX_DAPP_DEFINITION_MAINNET` | firma / ROLA en mainnet | *(vacío → rechazada: la wallet responde `invalidRequest`)* |
| `RADIX_DAPP_DEFINITION_STOKENET` | firma / ROLA en stokenet | *(vacío → rechazada: la wallet responde `invalidRequest`)* |
| `RADIX_DAPP_ORIGIN` | toda firma / ROLA | `https://radix-community.genkipool.com` |
| `RADIX_CONNECTOR_PENDING_TTL_SECONDS` | el freno anti-inundación | `900` — después, una petición entregada sin respuesta deja de bloquear |

Notas:

- La dApp definition es **por red** (mainnet y stokenet son cuentas distintas), de
  ahí las dos variables separadas.
- `request_account_proof` (ROLA) **exige** una dApp definition no vacía: si ni la
  llamada ni la variable de entorno la aportan, la herramienta devuelve un error
  en vez de firmar una prueba sin sentido.
- Sin dApp definition la wallet responde `invalidRequest`: define una.

Ejemplo (`claude mcp add` con env, o la configuración JSON de tu cliente):

```sh
RADIX_DAPP_DEFINITION_MAINNET=account_rdx1... \
RADIX_DAPP_ORIGIN=https://radix-community.genkipool.com \
  radix-connector-mcp
```

## Flujo típico

1. Construye y previsualiza un manifiesto con el servidor MCP HTTP del portal web
   (`radix-community`).
2. `pair_wallet` → muestra el QR → el usuario lo escanea desde la app Radix Wallet
   (Ajustes → Conectores enlazados → Enlazar nuevo conector) → `pair_status`.
3. `send_transaction { manifest, network }` → el usuario aprueba en el móvil.
4. `transaction_status { intent_hash, network }` → confirma el commit.

## Estado y seguridad

- Las wallets emparejadas y la identidad del conector viven en `connector.json` dentro
  del directorio de configuración del SO (`~/.config/radix-connector/` en Linux; los
  equivalentes de cada plataforma en macOS/Windows), `0600` en Unix. Se puede
  sobreescribir con `RADIX_CONNECTOR_HOME`.
- La contraseña del enlace y la identidad nunca salen de la máquina; el QR se genera en
  local.
- El móvil es lo único que firma. Cada acción se aprueba ahí por una persona.

## Arquitectura

El resumen de componentes, el transporte MCP/stdio, el conjunto de herramientas y
los diagramas de secuencia de emparejamiento/firma están en
[`docs/ARCHITECTURE.es.md`](docs/ARCHITECTURE.es.md)
([English](docs/ARCHITECTURE.md)).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
