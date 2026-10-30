# radixdlt-connector-mcp

A **local** MCP (Model Context Protocol) server that lets AI agents (Claude
Code/Desktop, Antigravity, Cursor, …) pair a Radix Wallet and get transactions
**signed on the user's own machine** — the wallet on the phone approves, and the
private key never leaves it.

***English** · [Español](README.es.md)*

## Why a local binary (and not the web MCP)

Signing a Radix transaction means holding a live Radix Connect channel (WebRTC, or
iroh — see below) open to the phone for the whole approval. A stateless serverless backend (the web
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

## Two transports: WebRTC and iroh

**Any Radix Wallet pairs, with nothing to configure.** By default (`auto`) the connector shows
ONE QR and listens for it over **WebRTC** (the Radix signaling server, as the official wallet and
Connector Extension use) and over **Radix Connect over iroh** (QUIC through iroh relays — no
signaling server, and no UDP needed: the relay connection is HTTPS) at the same time. The wallet
links over whichever it uses, and that wallet keeps it:

- the official wallet (WebRTC only) links over **WebRTC**, as always;
- a wallet with iroh on — Settings → Preferences → Radix Connect → **Automatic** or **iroh** —
  links over **iroh**. iroh is preferred: when WebRTC links first, iroh still has 5 seconds to
  link (the wallet waits for iroh the same way), so both ends settle on iroh whenever it works.
  A wallet that linked over iroh reaches that link over either transport afterwards.

`connector_log` records the decision (`paired`, with the `transport` and `why`).

The setting decides how **new** wallets are paired; each wallet then keeps the transport it was
paired over (`list_wallets` shows it). Choose with `--transport` (or
`RADIX_CONNECTOR_TRANSPORT`); there is no need to, unless you want one transport only:

```sh
claude mcp add radix-connector -- radix-connector-mcp                    # auto
claude mcp add radix-connector -- radix-connector-mcp --transport webrtc # WebRTC only
```

| `--transport` | new pairings | wallets paired over iroh | wallets paired over WebRTC (and every earlier one) |
|---|---|---|---|
| `auto` (default) | WebRTC and iroh at once, one QR: the one the wallet links over (iroh preferred) | iroh | WebRTC |
| `webrtc` | WebRTC | iroh | WebRTC |
| `iroh` | iroh | iroh | iroh (for a wallet set to "iroh" only) |

`pair_wallet { "transport": "webrtc" | "iroh" | "auto" }` pairs one wallet that way whatever the
setting. The wallet only connects while its app is open.

**Relays.** By default both ends use n0's public iroh relays — nothing to host or configure; the
connector publishes which relay it uses (the URL only, never an IP address) through n0's address
lookup, where the wallet finds it. To use one relay of your own instead, start the connector
with `--iroh-relay https://relay.example.com` (or `RADIX_CONNECTOR_IROH_RELAY`) and set the same
URL as the wallet's iroh relay. Both ends must make the same choice.

**One connector process per wallet over iroh.** A wallet link over iroh is one endpoint; two
connector processes (two MCP clients) holding it at once would take the wallet from each other.
So a process locks the link while it uses it and for 30 minutes after
(`RADIX_CONNECTOR_IROH_IDLE_SECONDS`) — long, because once the session is released the wallet
takes up to ~15 s to find a new one; another process waits for it within its call's timeout and
then fails with `LINK_BUSY`, naming the holder's pid.

**Diagnostics.** Over iroh, `check_wallet_connection` and `list_wallets` show the link's
connection line, e.g.
`connector 55e138fbc3 → 0f63fb2de4 state=connected path=relay rtt=42ms relay=https://euc1-1.relay.n0.iroh.link./ (connected, public)`;
the failures `WALLET_UNREACHABLE`, `NOT_CONFIRMED` and `NO_ANSWER` end with it, and
`connector_log` records it on every connection change. `relay … (NOT connected)` for more than a
few seconds means this machine cannot reach its relay; `state=disconnected` with the relay
connected means the phone is not there (app closed, iroh off, another relay choice). It never
contains the link password. iroh-only codes: `NOT_CONFIRMED` (the wallet never confirmed the
request — it may be on the phone, so it is not resent), `LINK_BUSY`, `IROH_UNAVAILABLE`.

**The log tells the whole story.** `connector_log` (`connector.log`, JSON lines, each with a
`level`) records, besides every request's steps:

- **Requests:** which transport and why (`request_start` `route`: the link's recorded transport or
  the setting), other requests to the wallet in flight, each send, each change of its state
  (`request_state`), and every timeout with the time waited and its limit (`*_ms`, `limit_ms`).
  Answers nobody waits for are told apart: `late_answer`, `duplicate_answer`, `unknown_answer`.
- **Pairing:** each transport's progress and failure (`pairing_attempt_failed`, with a
  `category`), the time from the QR to the wallet, the decision (`paired`, `why`), and a pairing a
  newer `pair_wallet` replaced (`pairing_abandoned`).
- **iroh links:** opened and why (`iroh_link_open`: first use, reopened after an idle release…),
  reused, release scheduled / cancelled / done (`iroh_link_idle`, `iroh_link_release_cancelled`,
  `iroh_link_released`), closed and why, and how long the wallet took to come back
  (`iroh_link_wallet_connected`, `after_ms`) — what a person feels after an idle release.
- **Several connector processes:** waiting for another process's link (`iroh_link_wait`,
  `iroh_link_locked`), giving up (`iroh_link_busy` with the holder's pid), and the lock of a
  connector that crashed taken back (`iroh_link_lock_recovered`). `list_wallets` and
  `check_wallet_connection` show who holds each link, its last request and outcome, and its last
  error.
- **The iroh libraries' own events** (`iroh_protocol`, `iroh_sdk`), among them
  `link_collision` — another holder of the link's key took this end's place, e.g. the same
  `connector.json` on two machines — and `wallet_connection_replaced`, both with the links'
  diagnostics.

`connector_log { "link": "<link id>" }` follows one wallet link (`list_wallets` shows its id).
Nothing in it opens a link or names an address: no passwords, keys or message contents, and IP
addresses (other than loopback) are blanked out of every line. Repeated lines are counted, not
written (`suppressed`).

## Tools

| Tool | What it does |
|---|---|
| `pair_wallet` | Returns a QR (terminal art + PNG + raw payload) to link a wallet — any Radix Wallet, over WebRTC or iroh. Once per device. `transport`: `auto`, `webrtc` or `iroh` (default: `--transport`, i.e. `auto`). |
| `pair_status` | Waits for the scan/approval and saves the link. |
| `list_wallets` / `remove_wallet` | Manage paired devices (with each one's transport and, over iroh, its live connection). |
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
| `check_wallet_connection` | Opens and closes a channel (over iroh: waits for the wallet to connect), sending nothing: is the wallet app reachable right now? Over iroh it shows the connection's diagnostics. |
| `check_dapp_identity` | Runs the wallet's own dApp verification (dApp definition ↔ origin ↔ `radix.json`). |
| `connector_log` | The connector's trace: every request step by step, pairings, each iroh link's life, locks between connector processes, collisions. Filter by `interaction_id` or `link`. |
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
   (Settings → Linked Connectors → Link New Connector) → `pair_status`. Nothing to set up on
   the phone first: it links over iroh when the wallet has iroh on, over WebRTC otherwise.
3. `send_transaction { manifest, network }` → the user approves on the phone.
4. `transaction_status { intent_hash, network }` → confirm the commit.

## State & security

- Paired wallets and the connector identity live in `connector.json` under the OS
  config dir (`~/.config/radix-connector/` on Linux; the platform equivalents on
  macOS/Windows), `0600` on Unix. Override with `RADIX_CONNECTOR_HOME`. Beside it,
  `transports.json` records how each wallet was paired (by a hash of its link, no secrets) and
  `iroh-links/` holds the per-link locks.
- The link password and identity never leave the machine; the QR is generated
  locally.
- The phone is the only thing that signs. Every action is human-approved there.

## Architecture

Component overview, MCP/stdio transport, the tool set and the pairing/signing
sequence diagrams are in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
([Español](docs/ARCHITECTURE.es.md)).

## License

Licensed under either of MIT or Apache-2.0 at your option.
