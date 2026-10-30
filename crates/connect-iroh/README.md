# radixdlt-connect-iroh

An [iroh](https://iroh.computer) (QUIC) transport for the RadixDLT Rust SDK, an
alternative to WebRTC with two uses:

- **The Radix mobile wallet, directly** — [`MobileWalletConnector`](src/mobile/mod.rs)
  speaks *Radix Connect over iroh v1*, the protocol the Radix Wallet and the Radix
  Connector Extension speak. A Rust server pairs with the phone through the usual QR
  code and asks it for logins, account proofs, transactions and pre-authorizations —
  no browser, no extension.
- **SDK-to-SDK** — two peers that both use the SDK (a desktop signer, a server, a
  device) exchange bounded, confirmed JSON messages, and run flows like ROLA "log in with
  Radix" entirely in Rust, with their own protocol (`IrohConnector`, `protocol`). The mobile
  wallet does not speak that one.

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

Two peers that both run this crate exchange JSON messages over iroh with their own protocol
(ALPN `radixdlt-connect-iroh/1`, specified in [`docs/PROTOCOL.md`](docs/PROTOCOL.md)).

### The channel

```rust
use radixdlt_connect_iroh::IrohConnector;

let endpoint = IrohConnector::bind().await?;
let mut channel = endpoint.connect_to_ticket(&ticket).await?;
channel.send_message(&request).await?;    // returns once the peer has TAKEN it
let response = channel.recv_message().await?;
println!("talking to {}", channel.remote_id()); // the peer's authenticated EndpointId
channel.close().await;                    // bounded
endpoint.close().await;                   // bounded
```

- **Every message travels on a QUIC stream of its own and is confirmed by the receiver.**
  `send_message` returns `Ok` once the peer has taken the message, so closing right after it
  loses nothing; there is no `finish`/`wait_closed` dance.
- **Bounded.** A message larger than `Limits::max_message_size` (8 MiB by default) is refused —
  an incoming one from its length prefix alone, before anything is allocated — and the channel
  stays usable. Once a peer has started a message, the whole of it must arrive within
  `Limits::message_timeout` (60 s): a peer trickling bytes cannot hold a task forever. Waiting
  for the NEXT message is not bounded, so a channel can stay open and idle; wrap
  `recv_message` in a timeout of your own where you want one.
- **Cancel-safe.** `send_message` and `recv_message` can be wrapped in `tokio::time::timeout` or
  `select!`: a cancelled receive resumes where it stopped; a cancelled send is abandoned whole
  (the peer never sees part of it). Neither can leave the channel out of step.
- **Closing.** `IrohChannel::close` and `IrohConnector::close` wait at most
  `Limits::close_timeout` (5 s). Dropped without `close`, both still close gracefully in the
  background when a tokio runtime is there.

### Who may connect

`IrohChannel::remote_id()` is the peer's `EndpointId`, proven by the QUIC (TLS 1.3) handshake:
dialling an id (or a ticket) reaches that key's holder or fails. By default anyone may connect
to an endpoint — an application that authenticates its peers itself (as PamAuthority does,
with ROLA) needs no more. To admit only known peers before the application sees them:

```rust
use radixdlt_connect_iroh::{IrohConnector, Limits, Relay};

let hub = IrohConnector::builder()
    .secret(hub_seed)                     // a key for this endpoint only (see Identity)
    .relay(Relay::Enabled)
    .allow_only([peer_a, peer_b])         // or .admit(|id| registry.contains(id))
    .limits(Limits::default().with_max_message_size(256 * 1024))
    .bind()
    .await?;
```

A peer not admitted is refused right after its handshake; `Pending::ready` reports it as an
error.

### High-level "log in with Radix" (ROLA), paired by ticket

`protocol::Wallet` is a pure-Rust signer: it holds an account key and answers wallet
interactions. **It signs nothing on its own**: every request goes to an `Approver`, which sees
who asks (the peer's `EndpointId`, plus the dApp definition and origin the request *claims*) and
exactly what is asked. Each kind of request has its own method, and each refuses unless
implemented — an approver written for logins cannot approve a transaction by omission.
Requests for another network, ROLA challenges that are not 32 bytes, manifests that do not
compile and pre-authorizations without expiry are refused before the approver is asked. A
refusal reaches the dApp as `rejectedByUser`.

```rust
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_connect_iroh::protocol::{
    request_account_proof, AccountProofRequest, Approver, DappContext, Requester, Wallet,
};

// Signer side: logins to one dApp, from the peer it paired with — nothing else.
struct PairedLogins { peer: radixdlt_connect_iroh::EndpointId }
impl Approver for PairedLogins {
    async fn approve_account_proof(&self, from: &Requester, req: &AccountProofRequest) -> bool {
        from.peer == self.peer && req.dapp_definition == MY_DAPP && req.origin == MY_ORIGIN
    }
}
let wallet = Wallet::from_key_file(&key_file, passphrase)?;
let approver = PairedLogins { peer: dapp_id }; // the dApp's EndpointId, learned when pairing
let signer = IrohConnector::builder().allow_only([dapp_id]).bind().await?;
let ticket = signer.ticket();              // share as a QR / string
let mut ch = signer.accept().await?;
wallet.answer(&mut ch, &approver).await?;

// dApp side:
let dapp = IrohConnector::bind().await?;
let mut ch = dapp.connect_to_ticket(&ticket).await?;
let proof = request_account_proof(&mut ch, &challenge, &ctx).await?; // sent + verified
```

The `Wallet` answers three interactions, matching the WebRTC flow:

- **account proof** — ROLA "log in with Radix" (`request_account_proof`).
- **transaction** — the wallet signs *and submits* a manifest, paying its fees
  (`request_transaction`). The dApp gets the intent hash only once the transaction has
  committed successfully; a committed failure, a rejection, or an outcome not known yet is a
  `failure` naming the status and the intent hash.
- **pre-authorization** — the wallet signs a subintent without submitting it
  (`request_pre_authorization`); whoever holds it can complete and submit it until it expires.

`Wallet::new` submits through the public Gateway of its network (1 mainnet, 2 stokenet; any
other network is an error) — `with_gateway` replaces it with one for the same network.

> **Do not expose a `Wallet` to whoever can reach it.** Anyone who can open a channel can send
> it requests; only the approver stands between them and the key. Pair it with known peers
> (`allow_only`), and never answer with an approver that approves everything.

### Tickets, identity and reachability

| Locator | Contents | Reaches |
| --- | --- | --- |
| `ticket()` | `EndpointId` + the addresses iroh knows: this machine's interface (LAN) addresses, loopback, and with `Relay::Enabled` its relay | same host, same LAN; with `Relay::Enabled` on both ends, the internet |
| `id_ticket()` / `endpoint_id_string()` | the `EndpointId` only, stable with a persistent identity | needs `Relay::Enabled` on both ends: iroh looks the id up in n0's pkarr/DNS |

`Relay::Disabled` (what `bind()` uses) is direct connections only: no relay, no address lookup,
and no port mapping. `Relay::Enabled` uses n0's public relays and address lookup (this endpoint's
relay URL is published under its `EndpointId`), and port mapping (UPnP / NAT-PMP / PCP) may ask
the local router to open a port for direct connections. There is no mDNS in this crate: an
application that wants LAN discovery advertises a ticket itself.

A ticket lists every interface address of the machine (VPN and container bridges included):
share it like any other network detail.

**Identity.** `bind()` uses an ephemeral key; `Builder::secret(seed)` (or
`bind_with(Some(seed), …)`) a persistent one, so the `EndpointId` and `id_ticket` survive
restarts, and `endpoint_id_from_seed(&seed)` derives the id offline. Use a key dedicated to the
endpoint, not a Radix account's key: the `EndpointId` is public (peers and relays see it, and
with `Relay::Enabled` it is published with this endpoint's relay), so reusing an account key ties
that account to the machine's network presence; and one key would sign in two protocols (the QUIC
handshake and Radix transactions). PamAuthority deliberately makes its *server's* `EndpointId` its
signing key, so machines that dial it already hold the key that signs what it sends them: that is
a public server identity by design, not a person's account, and the trade-off is documented
there.

See `examples/login.rs` (`cargo run --example login`) for the full flow, and `tests/` for the
channel, login, and transaction/pre-auth tests. Error messages are localized to the system
language.

## License

Licensed under either of MIT or Apache-2.0 at your option.
