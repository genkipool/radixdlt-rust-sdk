# radixdlt-connector-mcp

A **local** MCP (Model Context Protocol) server that lets AI agents (Claude
Code/Desktop, Antigravity, Cursor, …) pair a Radix Wallet and get transactions
**signed on the user's own machine** — the wallet on the phone approves, and the
private key never leaves it.

***English** · [Español](README.es.md)*

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

## Update

```sh
radix-connector-mcp check-update   # exit code 10 when a newer release exists, 0 when up to date
radix-connector-mcp update         # download, verify (SHA-256 + it runs), back up, replace
radix-connector-mcp update --tag connector-v0.4.0   # a specific release (also to roll back)
```

An agent can do the same with the `check_update` / `update_connector` tools. **Updating never
requires pairing the phone again**: the pairing lives in `connector.json` in the config
directory, and an update only replaces the binary. Restart the MCP client afterwards so it
launches the new version: a running session keeps the binary it started with (on Linux
`/proc/<pid>/exe` ends in `(deleted)`), and that old copy keeps the old behaviour.

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
| `request_accounts` | Asks the wallet to **share its account address(es)** — no signature/proof. Use it to learn which account to fund or transfer from. |
| `send_transaction` | Sends a manifest to sign **and submit**; returns the intent hash. Supports `blobs` (inline hex) and `blob_files` (local paths). Simulates it on the Gateway first and does **not** reach the phone when the simulation fails (`PREVIEW_FAILED`); `preview_only: true` returns the simulation without sending. |
| `deploy_package` | Publishes a Scrypto package: reads the `.wasm` from a local path, **dry-runs it on the Gateway first** (aborts if it would fail), attaches it as a blob, signs and submits. |
| `request_pre_authorization` | Signs a subintent (V2 pre-authorization) without submitting. |
| `request_account_proof` | ROLA "log in with Radix" with an account; verifies the proof locally. |
| `request_login` | Logs in with a **persona** (authorized request): with a challenge the persona signs it and the proof is verified here; `without_challenge` only names it. Can ask for accounts (with proofs) and persona data in the same approval. |
| `request_ownership_proof` | Proves **exact** accounts (and the persona) as a persona already logged in — nothing to pick, one confirmation. |
| `request_authorized` | The whole authorized vocabulary: `login` / `login_without_challenge` / `use_persona`, `reset`, proof of ownership, accounts and persona data **one-time or ongoing**. |
| `request_data` | Unauthorized request: accounts with **exact or minimum** quantity (optionally with proofs) and persona data — name, emails, **phone numbers**. |
| `pending_requests` | What may still be waiting in the wallet's queue, and what to do about it. |
| `await_response` | Collects the answer to an earlier request **without sending it again**. |
| `cancel_request` | Stops a request here and stops it blocking new ones (the wallet itself has no remote cancel — the result says whether it is still on the phone). |
| `check_wallet_connection` | Opens and closes a channel, sending nothing: is the wallet app reachable right now? |
| `check_dapp_identity` | Runs the wallet's own dApp verification (dApp definition ↔ origin ↔ `radix.json`). |
| `connector_log` | The connector's trace of every request, step by step. |
| `transaction_status` | Reads a transaction's commit status from the Gateway. |

### With no internet: a local relay over the USB cable

`radix-connector-mcp relay --listen <addr> [--turn <addr> --turn-password <pw>]` runs a
Radix Connect signaling relay (and optionally a TURN server) on this computer. Add it as a
signaling server in the Radix Wallet (Settings › Preferences › Signaling Servers) and list it in
`RADIX_CONNECT_RELAYS` or `/etc/radix-connect/relays`: connectors try the public relay and it at
once. What was learned on a real phone:

- **The STUN list in the wallet must not be empty** — Android's WebRTC throws on it and the wallet
  crashes when a channel opens. TURN may be empty.
- **USB tethering alone is not enough**: the wallet reaches the relay, but its WebRTC only uses
  networks Android declares to apps, and a tethering interface never is one.
- **USB tethering + a WireGuard tunnel that is the phone's default route works** (the tunnel IS a
  declared network). PamAuthority automates this: `pamauthority wire`.
- **Or `adb reverse`** of the relay and TURN ports, with the TURN in the wallet's server.

### Talking to a phone: delivery, the queue, and failures

The Radix Wallet shows **one request at a time** from an in-memory queue, and nothing a dApp
sends can withdraw one: a request leaves the queue when the person approves or rejects it, or
when the app is closed. The connector is built around that:

- **It knows whether the wallet got it.** Every request is logged step by step (channel open →
  *delivered*, i.e. the wallet confirmed receipt → answered). A request the wallet never
  confirmed is sent again, with the same interaction id, on a fresh channel (up to 3 attempts);
  a confirmed one is never sent twice.
- **It does not flood the queue.** While a delivered request is unanswered, new requests to
  that wallet are refused with `PENDING_IN_WALLET` (pass `ignore_pending: true` to override).
  `await_response` collects a late answer, `cancel_request` clears one.
- **It waits for the wallet to let go of a channel.** The wallet keeps one channel per link and,
  ~5 s after a connection closes, tears down whatever channel the link has then — so a request
  sent right after another could reach the phone and lose its answer. The connector waits 8 s
  after a channel closes before opening the next one on the same link, and reopens a channel
  lost after delivery to keep waiting for the answer.
- **It takes turns with every other program on the machine (0.6.0).** Several processes often
  share one paired link — two AI sessions, a `sudo` prompt, a CLI — and before 0.6.0 the turn and
  the 8 s wait lived inside each process, so one process could open a channel over another's and
  the request vanished. The turn is now also an OS file lock and the last close is written to
  disk (`~/.config/radix-connect/links/`, named by a hash of the link — no secret in it). Verified
  with two processes asking at once on a real phone: the second waited its turn and the settle
  time, and both arrived. An **older** connector running beside them does not take part: update
  every program that uses the link.
- **It does not ring the phone for a transaction that would fail (0.6.0).** `send_transaction`
  previews the manifest on the Gateway (free credit, signatures assumed); a definitive failure
  stops it there (`PREVIEW_FAILED`, `retry_safe`: yes), a preview that could not run lets it
  through — the wallet previews it again before anybody signs.
- **It checks the dApp identity first.** When `{origin}/.well-known/radix.json` does not list the
  dApp definition, the wallet drops the request *without answering*; the connector refuses to
  send it (`DAPP_NOT_VERIFIED`) instead of waiting out the timeout.
- **Failures say what to do.** Each carries a `code` (`WALLET_UNREACHABLE`, `NOT_DELIVERED`,
  `NO_ANSWER`, `REJECTED_BY_USER`, `WRONG_NETWORK`, `INVALID_REQUEST`, …), a `stage`, whether
  resending is safe (`retry_safe`), a hint, and the interaction id — as text and as
  `structuredContent`. Late answers to earlier requests (e.g. a transaction approved after its
  call timed out) are reported, not dropped. `connector_log` traces any request.

Tool calls run concurrently, so `cancel_request` and `pending_requests` answer while another
call is waiting for the phone, and a client's `notifications/cancelled` stops the call it names.

Every signing tool requires an explicit `network` (`"mainnet"` or `"stokenet"`)
— there is no default, on purpose.

## dApp identity (environment variables)

When the wallet signs, it shows **which dApp** is asking. That identity is a pair
of values — the dApp definition address and the origin — that must match the
`claimed_websites` / dApp definition registered on-chain, and the origin's
`/.well-known/radix.json` must list it. Otherwise the wallet (without developer mode)
refuses the request — or, when `radix.json` loads but does not list the dApp, drops
it without answering. The connector checks all of this before sending
(`check_dapp_identity`; `skip_dapp_check: true` for a wallet in developer mode).

You can pass them per call (`dapp_definition`, `origin` on the signing tools), but
it is more robust to configure them **once** so the connector fills them in when a
call omits them. Precedence is **call argument → environment variable → built-in
default**.

| Variable | Used by | Default |
|---|---|---|
| `RADIX_DAPP_DEFINITION_MAINNET` | mainnet signing / ROLA | *(empty → refused: the wallet answers `invalidRequest`)* |
| `RADIX_DAPP_DEFINITION_STOKENET` | stokenet signing / ROLA | *(empty → refused: the wallet answers `invalidRequest`)* |
| `RADIX_DAPP_ORIGIN` | all signing / ROLA | `https://radix-community.genkipool.com` |
| `RADIX_CONNECTOR_PENDING_TTL_SECONDS` | the flood guard | `900` — after this a delivered, unanswered request stops blocking |

Notes:

- The dApp definition is **per network** (mainnet and stokenet are different
  accounts), hence the two separate variables.
- `request_account_proof` (ROLA) **requires** a non-empty dApp definition: if
  neither the call nor the env var provides one, the tool returns an error rather
  than signing a meaningless proof.
- Without a dApp definition the wallet answers `invalidRequest`: set one.

Example (`claude mcp add` with env, or your client's JSON config):

```sh
RADIX_DAPP_DEFINITION_MAINNET=account_rdx1... \
RADIX_DAPP_ORIGIN=https://radix-community.genkipool.com \
  radix-connector-mcp
```

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

## Architecture

Component overview, MCP/stdio transport, the tool set and the pairing/signing
sequence diagrams are in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
([Español](docs/ARCHITECTURE.es.md)).

## License

Licensed under either of MIT or Apache-2.0 at your option.
