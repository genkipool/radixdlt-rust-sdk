# radixdlt-connector-mcp

A **local** MCP (Model Context Protocol) server that lets AI agents (Claude
Code/Desktop, Antigravity, Cursor, …) pair a Radix Wallet and get transactions
**signed on the user's own machine** — the wallet on the phone approves, and the
private key never leaves it.

*ES — Servidor MCP local que permite a los agentes de IA emparejar una Radix
Wallet y firmar transacciones en la máquina del usuario; el móvil aprueba y la
clave privada nunca sale del teléfono.*

## Why a local binary (and not the web MCP)

Signing a Radix transaction means holding a live Radix Connect (WebRTC) channel
open to the phone for the whole approval. A stateless serverless backend (the web
portal on Vercel) cannot do that, and the link secrets must never touch a server.
So this piece runs locally and speaks MCP over **stdio** to the agent that
launched it. The web portal's HTTP MCP still does everything read-only (docs,
ledger, building and previewing manifests); this binary adds the signing step.

The installed command is `radix-connector-mcp`.

## Install (from GitHub — no crates.io / npm)

**With Rust (any OS):**

```sh
cargo install --git https://github.com/genkipool/radixdlt-rust-sdk radixdlt-connector-mcp
```

The binary lands in `~/.cargo/bin/radix-connector-mcp`.

**Prebuilt binary, Linux/macOS:**

```sh
curl -fsSL https://raw.githubusercontent.com/genkipool/radixdlt-rust-sdk/main/scripts/install-connector.sh | sh
```

**Prebuilt binary, Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/genkipool/radixdlt-rust-sdk/main/scripts/install-connector.ps1 | iex
```

## Register with an MCP client

Claude Code:

```sh
claude mcp add radix-connector -- radix-connector-mcp
```

Generic JSON config (Claude Desktop / Antigravity / Cursor):

```json
{
  "mcpServers": {
    "radix-connector": { "command": "radix-connector-mcp" }
  }
}
```

If the binary is not on your `PATH`, use its absolute path as `command`.

## Tools

| Tool | What it does |
|---|---|
| `pair_wallet` | Returns a QR (terminal art + PNG + raw payload) to link a wallet. Once per device. |
| `pair_status` | Waits for the scan/approval and saves the link. |
| `list_wallets` / `remove_wallet` | Manage paired devices. |
| `send_transaction` | Sends a manifest to sign **and submit**; returns the intent hash. |
| `request_pre_authorization` | Signs a subintent (V2 pre-authorization) without submitting. |
| `request_account_proof` | ROLA "log in with Radix"; verifies the proof locally. |
| `transaction_status` | Reads a transaction's commit status from the Gateway. |

Every signing tool requires an explicit `network` (`"mainnet"` or `"stokenet"`)
— there is no default, on purpose.

## Typical flow

1. Build and preview a manifest with the web portal's HTTP MCP server
   (`radix-community`).
2. `pair_wallet` → show the QR → user scans from the Radix Wallet app
   (Settings → Linked Connectors → Link New Connector) → `pair_status`.
3. `send_transaction { manifest, network }` → the user approves on the phone.
4. `transaction_status { intent_hash, network }` → confirm the commit.

## State & security

- Paired wallets and the connector identity live in `connector.json` under the OS
  config dir (`~/.config/radix-connector/` on Linux; the platform equivalents on
  macOS/Windows), `0600` on Unix. Override with `RADIX_CONNECTOR_HOME`.
- The link password and identity never leave the machine; the QR is generated
  locally.
- The phone is the only thing that signs. Every action is human-approved there.

## License

Licensed under either of MIT or Apache-2.0 at your option.
