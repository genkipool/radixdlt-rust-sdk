# radixdlt-connect-iroh

An [iroh](https://iroh.computer) (QUIC) transport for the RadixDLT Rust SDK, an
alternative to WebRTC with two uses:

- **The Radix mobile wallet, directly** — [`MobileWalletConnector`](src/mobile/mod.rs)
  speaks *Radix Connect over iroh v1*, the protocol the Radix Wallet and the Radix
  Connector Extension speak. A Rust server pairs with the phone through the usual QR
  code and asks it for logins, account proofs, transactions and pre-authorizations —
  no browser, no extension.
- **SDK-to-SDK** — two peers that both use the SDK (a desktop signer, a server, a
  device) run flows like ROLA "log in with Radix" entirely in Rust, with their own
  protocol (`IrohConnector`, `protocol`). The mobile wallet does not speak that one.

***English** · [Español](README.es.md)*

```toml
[dependencies]
radixdlt-connect-iroh = "0.3"
```

Requires Rust 1.91 (iroh 1.4).

## The Radix mobile wallet over iroh

### On the phone

1. **Settings → Preferences → Radix Connect → Automatic** (or **iroh**). In Automatic
   the wallet links over iroh and WebRTC at once and keeps iroh for the links where it
   works; in iroh it uses nothing else.
2. Its **iroh relay** setting must match your server's choice (see [Relays](#relays)): by
   default both use n0's public relays, so leave it at its default.
3. The wallet connects only while its app is open: requests wait (within their timeout)
   for the person to open it, and report that they are waiting.

### Pairing

```rust
use std::time::Duration;
use radixdlt_connect_iroh::mobile::{linking, MobileWalletConnector};

// n0's public relays: nothing to run or configure, on either end.
let connector = MobileWalletConnector::new();
// …or one relay of your own, set to the same URL in the wallet:
// let connector = MobileWalletConnector::new().with_relay_url("https://relay.example.com")?;

// The connector's long-term identity: generate it once and keep it (it names the
// connector in the wallet).
let (identity_private, identity_public) = linking::generate_identity()?;

let (wallet_public_key, password) = connector
    .pair(&identity_private, &identity_public, |qr_json| {
        // Draw it (radixdlt-connect-types' `qr` feature renders it for a terminal), and
        // never print or log the JSON: it carries the link password.
        show_qr(&qr_json);
    }, Duration::from_secs(300))
    .await?;
// Store `password` like a credential (see Security). The session started for pairing
// is kept: the wallet reconnects with its regular session to the same endpoint.
```

The person scans the QR from **Settings → Linked Connectors → Link New Connector**. The
QR is the same JSON as over WebRTC; the wallet's `linkClient` signature is verified (and
required) before `pair` returns. The wallet links over a connection of its own, which drops
anything else sent on it, so `pair` returns only once the wallet has closed it (at most 5 s
later): a request sent right after pairing goes over the wallet's regular connection. A link
paired here also works with
`radixdlt-connect` (WebRTC) when the wallet is on Automatic.

### Requests

The API mirrors `radixdlt_connect::Connector` method for method — switching transport is
changing the type:

```rust
use radixdlt_connect_iroh::mobile::{extract_login, extract_proofs, DappContext, PersonaRequest};

connector.open_link(&password).await?;    // at startup, for every stored link (see below)

let ctx = DappContext::new(2, dapp_definition, "https://my.app");
let answer = connector
    .request_login(&password, &challenge_hex, &ctx, PersonaRequest::NONE, Duration::from_secs(120))
    .await?;
let (identity, label, persona_proof) = extract_login(&answer)?;  // verify with radixdlt-rola
let accounts = extract_proofs(&answer)?;                         // same challenge
```

Also `request_account_proof(_sharing)`, `request_authorized`, `request_ownership`,
`request_unauthorized`, `request_accounts`, `request_transaction` (→ intent hash) and
`request_pre_authorization` (→ signed partial transaction), all built on
`radixdlt-connect-types`. `exchange` sends any interaction and reports each step
(`Progress`: waiting for the wallet, connected, delivered, resending, unconfirmed,
disconnected, other messages); `await_response` collects the answer to a request sent
earlier; `probe` checks the wallet is reachable; `subscribe` streams the messages no
request claims (the wallet's `accountList`, late answers).

How it behaves:

- **One session per link**, started on first use and kept until `close_link`, `close` or
  drop. The wallet dials only while a session listens and re-dials with a backoff of up
  to 15 s, so call `open_link` for every stored link when the server starts.
- **Answers are matched by `interactionId`**: several requests can be in flight on one
  link, from any number of tasks (the connector is `Send + Sync`; share it in an `Arc`).
- **Nothing that may have reached the wallet is sent twice.** A request is sent again
  only when the session reports that nothing was sent (the wallet dropped the connection
  just before). Once it was confirmed — or its confirmation timed out or was lost with
  the connection — the call only waits for the answer, across wallet reconnections.
- **Late answers are kept.** An answer that arrives after its request timed out here
  (the person approved later) is kept, bounded, for `await_response` — or for a repeat
  `exchange` of the same interaction, which then returns it without sending anything.
- **Every wait is bounded** by the timeout the caller passes; the errors say how far the
  request got (`MobileError` documents which ones are safe to resend).

### Relays

A relay forwards end-to-end encrypted QUIC packets between the two devices and helps them
punch a direct path when the network allows it. There are two choices, and **both ends of a
link must make the same one** (`Relays`):

- **Public relays — the default** (`MobileWalletConnector::new()`, and the wallet's default
  setting). n0's public relays (North America, Europe, Asia-Pacific): each end connects to its
  closest one and moves to another if it fails. This connector publishes which relay it uses
  — the relay URL only, never an IP address — through n0's address lookup (pkarr/DNS), under
  its endpoint id; the wallet looks it up there. Nothing to host or configure, in development
  or in production.
- **A custom relay — optional** (`with_relay_url(url)`, and the same URL in the wallet's iroh
  relay setting). Typically your own [`iroh-relay`](https://crates.io/crates/iroh-relay) over
  HTTPS: you control its availability and who it serves, and nothing is published anywhere.
  Plain `http://` is accepted only on loopback, private and link-local addresses (a relay on
  your machine or LAN, for development).

If one end uses the public relays and the other a custom one (or two different custom
relays), the wallet never connects: requests end in `WalletUnreachable`, whose diagnostics
show `public` on one side only.

### Diagnostics and logs

Every timeout error carries a connectivity snapshot of the link
(`MobileError::diagnostics()`), and `connector.diagnostics(&password)` takes one at any
time; the state reports in `Progress` carry one too. Its `Display` is one line, e.g.:

```text
connector efcc89cfbf → 4d99dc649e state=connected path=relay rtt=42ms relay=https://euc1-1.relay.n0.iroh.link./ (connected, public)
```

| What you see | What it usually means |
|---|---|
| `relay … (NOT connected)` or `relay=none yet` (for more than a few seconds) | This server cannot reach a relay: no network, the relay is down or blocked, or a wrong URL. Nothing can connect. |
| `state=disconnected`, relay connected, `WalletUnreachable` | The phone is not there: app closed, iroh off for the link, another relay choice, or not linked. |
| `last_error="… unauthorized"` | The two ends hold different passwords for the link: pair again. |
| `last_error="… replaced by a newer connection"` | Harmless: the wallet reconnected (network change, app restart). |
| `path=relay` | Normal behind strict NATs; still end-to-end encrypted. |

The connector logs through [`tracing`](https://docs.rs/tracing) with structured fields:
`RUST_LOG=radix_connect_iroh=debug,radixdlt_connect_iroh=debug` shows the pairing steps,
every connection change (with its diagnostics line), sends, resends, timeouts and the
protocol's own explanations. Links appear as a short hash of the password, messages as
their size and interaction id — never the password or a message's contents.

### Security

- **The link password is the link.** Whoever holds it can talk to the wallet as this
  connector (and derive both iroh endpoint keys). Store it like a credential — a file
  readable only by the service account (`0600`), or a secret manager — and never log it.
  The connector never logs it, and no error or diagnostics line contains it.
- **The identity private key** names the connector in the wallet. Keep it with the same
  care, and reuse it for every pairing.
- **Only the link's wallet is admitted**: both endpoint ids derive from the password,
  and the session closes any other peer. The wallet's `linkClient` signature over the
  password is required and verified at pairing.
- **Relays see neither messages nor the password** — only the two endpoint ids and the
  timing and size of encrypted packets. A malicious relay can drop traffic, not read or
  forge it. Over plain HTTP (local relays only) an observer also sees the endpoint ids.
- **What the public relays publish.** With the default, this connector publishes, under its
  endpoint id, the URL of the relay it uses — no IP address, nothing about the wallet. The
  id derives from the link password (different for every link, unlinkable, and revealing
  nothing about the password). A custom relay publishes nothing.
- **Verify what the wallet returns.** ROLA proofs are checked with `radixdlt-rola`
  against a fresh challenge, your dApp definition and origin — the connector carries
  answers, it does not vouch for them.

### Example

[`examples/mobile_login.rs`](examples/mobile_login.rs) pairs (QR in the terminal), stores
the link in `~/.config/radixdlt-connect-iroh/mobile-link.json` (mode `0600`; the password
is never printed), then asks the wallet to log in and verifies the persona and account
proofs:

```bash
cargo run --example mobile_login                                       # public relays
cargo run --example mobile_login -- --relay https://relay.example.com  # a relay of your own
RUST_LOG=radix_connect_iroh=debug,radixdlt_connect_iroh=debug cargo run --example mobile_login
```

The tests in `tests/mobile_wallet_over_iroh.rs` run the connector against a simulated
wallet (built on the same protocol library as the real one) over an in-process relay.

> **Build note.** The protocol library, `radix-connect-iroh`, is a path dependency on
> its sibling repository (`../../../../radix-connect-iroh`) while both are developed
> together: check it out next to this repository to build. It becomes a crates.io (or
> git) dependency before this crate is published.

## SDK-to-SDK (pure-Rust peers)

### Low-level transport

```rust
use radixdlt_connect_iroh::IrohConnector;

let endpoint = IrohConnector::bind().await?;
let mut channel = endpoint.connect(peer_addr).await?;
channel.send_message(&request).await?;
let response = channel.recv_message().await?;
```

### High-level "log in with Radix" (ROLA), paired by ticket

```rust
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_connect_iroh::protocol::{Wallet, request_account_proof, DappContext};

// Signer side (a pure-Rust "wallet"):
let wallet = Wallet::from_key_file(&key_file, passphrase)?;
let signer = IrohConnector::bind().await?;
let ticket = signer.ticket();             // share as a QR / string
let mut ch = signer.accept().await?;
wallet.answer(&mut ch).await?;            // signs the ROLA challenge

// dApp side:
let dapp = IrohConnector::bind().await?;
let mut ch = dapp.connect_to_ticket(&ticket).await?;
let proof = request_account_proof(&mut ch, &challenge, &ctx).await?; // sent + verified
```

The `Wallet` answers three interactions over iroh, matching the WebRTC flow:

- **account proof** — ROLA "log in with Radix" (`request_account_proof`).
- **transaction** — the wallet signs *and* submits a manifest, returns the intent
  hash (`request_transaction`).
- **pre-authorization** — the wallet signs a subintent without submitting, returns
  the signed partial transaction (`request_pre_authorization`).

### Persistent identity and internet peers (relay + discovery)

`bind()` uses an ephemeral identity and direct connections only (same host / LAN).
For peers that must be reachable across restarts or over the internet:

```rust
use radixdlt_connect_iroh::{endpoint_id_from_seed, IrohConnector, Relay};

// Hub: fixed 32-byte seed → stable EndpointId, n0 relays + discovery enabled.
let hub = IrohConnector::bind_with(Some(seed), Relay::Enabled).await?;
let ticket = hub.id_ticket();             // stable across restarts (id only, no addrs)

// Peer: also bind with Relay::Enabled, then dial by ticket or by raw EndpointId.
let peer = IrohConnector::bind_with(None, Relay::Enabled).await?;
let mut ch = peer.connect_to_ticket(&ticket).await?;
// …or, if the id was learned via mDNS/discovery:
// let mut ch = peer.connect_to_endpoint_id(&hub_id).await?;
```

`endpoint_id_from_seed(&seed)` derives the same `EndpointId` string offline (no
endpoint bound), useful to print or distribute a hub locator ahead of time.

See `examples/login.rs` (`cargo run --example login`) for the full flow, and
`tests/` for the low-level, login, and transaction/pre-auth tests. Error messages are
localized to the system language.

The wire protocol (transport, framing, pairing, message schema and per-interaction
sequence diagrams) is specified in [`docs/PROTOCOL.md`](docs/PROTOCOL.md)
([Español](docs/PROTOCOL.es.md)).

## License

Licensed under either of MIT or Apache-2.0 at your option.
