# radix-connector-mcp — Architecture

***English** · [Español](ARCHITECTURE.es.md)*

Status: reflects the code in `crates/connector-mcp` (`main.rs`, `rpc.rs`,
`tools.rs`, `store.rs`, `qr.rs`, `gateway.rs`). This crate is a **local MCP
(Model Context Protocol) server** that lets AI agents (Claude Code/Desktop,
Cursor, Antigravity, …) pair a Radix Wallet and get transactions **signed on the
user's own phone** — the private key never leaves the device.

---

## 1. Why it runs locally

Signing a Radix transaction requires holding a live Radix Connect (WebRTC)
channel to the phone for the whole approval, and the link secrets must never
leave the user's machine. A stateless serverless backend cannot hold that
channel, so this piece runs locally and speaks MCP over **stdio** to the agent
that launched it.

---

## 2. Components

```mermaid
flowchart LR
    Agent["AI agent<br/>(Claude Code/Desktop, Cursor, …)"]
    subgraph MCP["radix-connector-mcp (local process)"]
        direction TB
        M["main.rs<br/>stdio transport:<br/>newline JSON-RPC 2.0"]
        R["rpc.rs<br/>MCP core:<br/>initialize / tools/list / tools/call"]
        T["tools.rs<br/>10 tools + dispatch"]
        ST["store.rs<br/>connector.json state"]
        QR["qr.rs<br/>QR (unicode + PNG)"]
        GW["gateway.rs<br/>tx-status HTTP read"]
    end
    Conn["radixdlt-connect<br/>(WebRTC + signaling)"]
    Wallet["Radix Wallet (phone)"]
    Ledger["Radix Gateway (HTTP)"]

    Agent -- "stdin/stdout" --> M --> R --> T
    T --> ST
    T --> QR
    T --> GW --> Ledger
    T --> Conn -- "Radix Connect" --> Wallet
```

- **stdout** carries protocol messages only; all human-readable logs go to
  **stderr**.
- The whole server runs on a **single-threaded Tokio runtime inside a
  `LocalSet`** (one wallet channel at a time; keeps the non-`Send` WebRTC futures
  local while a slow pairing runs in the background).
- Shared state is an `Rc<App>` with `RefCell` interior mutability; handlers must
  not hold a borrow across an `.await`.

---

## 3. Transport & MCP core

- **Framing (`main.rs`):** read stdin line by line; each request line yields at
  most one response line on stdout; notifications yield none; blank lines are
  skipped. One writer task owns stdout, so concurrent answers never interleave.
- **Concurrency (`rpc.rs`):** every `tools/call` runs as its own task, so a call
  waiting minutes for the phone does not block `pending_requests`,
  `cancel_request` or anything else. `notifications/cancelled` aborts the call it
  names (dropping its channel to the wallet). When stdin closes, calls already
  started still finish and answer.
- **MCP (`rpc.rs`):** JSON-RPC 2.0. Handles `initialize` (negotiates a protocol
  version — newest `2025-06-18`, also accepts `2025-03-26` / `2024-11-05`),
  `ping`, `tools/list`, `tools/call`. `notifications/*` get no response. Errors
  use JSON-RPC codes (`-32700` parse, `-32600` invalid request, `-32601` method
  not found).

```mermaid
sequenceDiagram
    autonumber
    participant A as Agent
    participant M as main.rs (stdio)
    participant R as rpc.rs
    participant T as tools.rs

    A->>M: { "method":"initialize", ... }\n
    M->>R: handle_line
    R-->>A: serverInfo + capabilities + instructions
    A-->>R: notifications/initialized  (no response)
    A->>R: tools/list
    R-->>A: [ pair_wallet, send_transaction, … ]
    A->>R: tools/call { name, arguments }
    R->>T: call(app, name, args)
    T-->>A: ToolResult (content or isError)
```

---

## 4. Tool set (`tools.rs`)

| Tool | Purpose |
| --- | --- |
| `pair_wallet` | Start pairing: return a QR to scan (runs the handshake in the background). |
| `pair_status` | Poll the outcome of the in-flight pairing. |
| `list_wallets` | List paired devices. |
| `remove_wallet` | Unpair a device. |
| `request_accounts` | Ask the wallet to share account address(es), no proof. |
| `request_account_proof` | ROLA "log in with Radix" proof. |
| `send_transaction` | Send a manifest for the user to sign + submit. |
| `deploy_package` | Publish a package (WASM + RPD blobs), with a pre-deploy dry-run. |
| `request_pre_authorization` | Have a subintent signed (no submit). |
| `request_login` | Log in with a persona (authorized request), persona proof verified locally. |
| `request_ownership_proof` | Prove exact accounts / the persona as a persona already logged in. |
| `request_authorized` | Any authorized request: auth, reset, proof of ownership, one-time / ongoing data. |
| `request_data` | Unauthorized request: exact/minimum accounts (with proofs), name, emails, phones. |
| `pending_requests` | Requests that may still be waiting in the wallet's queue. |
| `await_response` | Collect a late answer without resending. |
| `cancel_request` | Stop a request here and stop it blocking. |
| `check_wallet_connection` | Is the wallet reachable (opens a channel, sends nothing). |
| `check_dapp_identity` | The wallet's dApp verification, run locally. |
| `connector_log` | The step-by-step trace (`connector.log`). |
| `transaction_status` | Read a transaction's commit status from the Gateway. |

Every request to the phone goes through one road, `exchange::interact`:
flood guard (`outbox.rs`, `requests.json`) → dApp-identity preflight
(`gateway.rs`) → record + log (`diag.rs`, `connector.log`) → the library's
`Connector::exchange`, whose `Progress` steps (turn taken, settling, channel open,
**delivered**, resending, reconnecting, other message) update the record as they
happen. Failures are `diag::Failure`: code, stage, `retry_safe`, hint, interaction
id, returned as text and `structuredContent`.

Dispatch is a single `match` in `tools::call`; an unknown tool returns an
`isError` result rather than a JSON-RPC error, so the agent sees a tool failure.

---

## 5. Key flows

### 5.1 Pairing (async, poll-based)

`pair_wallet` returns the QR **immediately** and runs the blocking-until-scanned
Radix Connect handshake in a background `spawn_local` task; `pair_status` reads
the shared result slot.

```mermaid
sequenceDiagram
    autonumber
    participant A as Agent
    participant T as tools.rs
    participant C as radixdlt-connect
    participant W as Wallet (phone)

    A->>T: tools/call pair_wallet { label? }
    T->>T: load/init connector.json (identity)
    T->>C: Connector::pair(...) [spawn_local, 600 s timeout]
    C-->>T: QR payload (via oneshot, before it blocks)
    T-->>A: QR (unicode + PNG) — show to user
    W->>C: user scans QR → linkClient
    C-->>T: PairOutcome { walletPublicKey, password } → result slot
    A->>T: tools/call pair_status
    T->>T: persist Link into connector.json
    T-->>A: paired ✓ (walletPublicKey)
```

### 5.2 Signing a transaction

```mermaid
sequenceDiagram
    autonumber
    participant A as Agent
    participant T as tools.rs
    participant S as store.rs
    participant C as radixdlt-connect
    participant W as Wallet (phone)
    participant G as Gateway

    A->>T: tools/call send_transaction { manifest, network, wallet? }
    T->>S: password_bytes(_for) from connector.json
    T->>C: Connector::request_transaction(password, manifest, ctx)
    C->>W: WalletInteraction (WebRTC) → user approves + signs + submits
    W-->>C: transactionIntentHash
    C-->>T: intent hash
    opt confirm commit
        T->>G: transaction_status(intentHash)
        G-->>T: committedSuccess / pending / failure
    end
    T-->>A: intent hash (+ status)
```

`deploy_package` is the same shape with a **pre-deploy dry-run** first and blobs
attached; `request_pre_authorization` returns a `signedPartialTransaction` and
does **not** submit.

---

### 5.3 Delivery, the wallet's queue, and late answers

```mermaid
sequenceDiagram
    autonumber
    participant A as Agent
    participant C as connector-mcp
    participant W as Wallet (phone)

    A->>C: send_transaction
    C->>C: open request for this wallet? → PENDING_IN_WALLET (nothing sent)
    C->>C: dApp identity check → DAPP_NOT_VERIFIED (nothing sent)
    C->>C: last channel on this link closed < 8 s ago? wait (settle)
    C->>W: open channel, wait 0.6 s, send request
    alt no receipt confirmation in 6 s
        C->>W: fresh channel, same interaction id (up to 3 attempts) → else NOT_DELIVERED
    end
    W-->>C: receiveMessageConfirmation → state "delivered" (it is on the phone)
    alt nobody answers in time
        C-->>A: NO_ANSWER (retry_safe = NO); new requests refused
        A->>C: await_response → reopen (after settle), listen for the same id
    end
    W-->>C: answer (or an answer to an EARLIER request → reported as a late answer)
```

The wallet keeps one data channel per link and, about five seconds after a
connection closes, tears down whatever channel the link has then. Hence the
settle (in-process in the library, across processes via `requests.json`), and the
reconnect when a channel is lost after delivery.

## 6. State & config (`store.rs`)

State lives in a `connector.json` under the OS config dir, honouring
`RADIX_CONNECTOR_HOME`:

- Linux: `~/.config/radix-connector/connector.json`
- macOS: `~/Library/Application Support/radix-connector/connector.json`
- Windows: `%APPDATA%\radix-connector\connector.json`

It reuses `LinkState` from [`radixdlt-connect`](../../connect/docs/PROTOCOL.md#7-persistent-link-state-staters-connectorjson):
a persistent connector identity plus one `Link` (password + `walletPublicKey`)
per paired device. `load_or_init` creates a fresh identity on first run.

---

## 7. Security notes

- **stdout hygiene:** only protocol JSON on stdout; logs on stderr — a stray
  print would corrupt the MCP stream.
- **Secret custody:** the server holds only channel passwords
  (`connector.json`, stored `0600`); the signing key stays on the phone and the
  user approves every signature there.
- **Explicit network:** every signing tool requires an explicit `mainnet` /
  `stokenet`, so a manifest can't be signed against the wrong network by default.
- **Local-only:** communication is stdio to the launching agent; there is no
  network listener.
