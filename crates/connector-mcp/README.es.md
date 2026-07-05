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
| `send_transaction` | Envía un manifiesto para firmar **y enviar**; devuelve el intent hash. |
| `request_pre_authorization` | Firma un subintent (pre-autorización V2) sin enviarlo. |
| `request_account_proof` | "Iniciar sesión con Radix" (ROLA); verifica la prueba localmente. |
| `transaction_status` | Lee el estado de commit de una transacción desde el Gateway. |

Cada herramienta de firma requiere una `network` explícita (`"mainnet"` o
`"stokenet"`) — no hay valor por defecto, a propósito.

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

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
