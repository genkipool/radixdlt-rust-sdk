# Changelog

All notable changes to the RadixDLT Rust SDK are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates
follow [Semantic Versioning](https://semver.org/). While the crates are in `0.x`,
minor versions may contain breaking changes.

## [Unreleased]

## [connector-v0.7.0] - 2026-10-31

`radixdlt-connector-mcp` 0.7.0: one QR for any Radix Wallet, over iroh or WebRTC with nothing to
choose, and over iroh a wallet that is open connects at once. The self-updater offers it;
`connector.json` files of 0.6 load as they are.

### Added

- **Pairing over iroh and WebRTC at once** (`radixdlt-connect-auto`): the QR works with any Radix
  Wallet; iroh is preferred when the wallet has it, WebRTC otherwise. The link records the
  transport it was paired over. `auto` is the default setting.
- **Each request races both transports** over a link paired over iroh: the first one ready carries
  it (only that one, so the wallet is never asked twice); a link whose iroh connection is up skips
  the race.
- **Knocks** (`radix-connect-iroh` 0.1.2): a session that starts tells its wallet, which dials at
  once instead of at its next retry (up to 30 s later). Each link keeps the relay its wallet was
  last seen on (`walletRelay`) to knock there. Measured with a phone: about 20 s down to 0.6 s.
- **Removing a link removes it on both sides**: the wallet is told when the link is forgotten here,
  and a link the wallet removed is forgotten here.

### Changed

- Built on `radixdlt-connect-auto` and `radixdlt-connect-webrtc` (the renamed `radixdlt-connect`).
- The iroh path explains itself in the logs (sessions, knocks, why a wallet is not reachable).


### Added — `radixdlt-connect-auto`: iroh or WebRTC, with nothing to choose

- **`WalletConnector`**: one QR pairs over iroh and WebRTC at once (iroh preferred, with a grace
  period after WebRTC links); the `Link` records the transport it was paired over.
- **Each request races both transports** over a link paired over iroh: the first one ready carries
  it — only that one, so the wallet never gets a request twice — and the answer is awaited over
  both. Never slower than WebRTC; as fast as iroh when iroh is there. A link whose iroh connection
  is up skips the race.
- `keep_listening` keeps a link's iroh session open (daemons, agents), `notify_link_removed` tells a
  wallet over every transport it may be on, and the `request_*` helpers match
  `radixdlt-connect-webrtc`'s, over a `Link`.
- One `Error` for both transports, saying how far a request got.
- `connect-types`: `Transport`; `state::Link` and `RetiredLink` record `transport` and `irohRelay`
  (optional: older `connector.json` files load as before, as WebRTC).

### Changed — `radixdlt-connect` is now `radixdlt-connect-webrtc`

- The WebRTC transport crate is renamed **`radixdlt-connect-webrtc`** (in `crates/connect-webrtc`),
  next to `radixdlt-connect-iroh`: each name says its transport. Nothing else changes in it.
- **`radixdlt-connect` stays**, as a crate that re-exports `radixdlt-connect-webrtc` whole, so code
  written against `radixdlt_connect::…` keeps building. New code depends on
  `radixdlt-connect-webrtc` (WebRTC only), `radixdlt-connect-iroh` (iroh only) or
  `radixdlt-connect-auto` (both, chosen by itself).
- `radixdlt-connector-mcp` depends on `radixdlt-connect-webrtc`. Its binary and package keep their
  names: installing and updating it does not change.

### Changed — `radixdlt-connector-mcp`: any Radix Wallet pairs, with one QR

- **`auto` is the default transport setting** (unset or empty `--transport` /
  `RADIX_CONNECTOR_TRANSPORT`), and it now pairs over **WebRTC and Radix Connect over iroh at
  once**, with ONE password and ONE QR: the official wallet (WebRTC only) links over WebRTC as
  before; a wallet with iroh on (Automatic or iroh) links over iroh. **iroh is preferred**: iroh
  linking first is kept at once; WebRTC linking first gives iroh 5 more seconds (`IROH_GRACE`; the
  wallet waits for iroh the same way, so both ends settle on iroh whenever it works), and only
  then is WebRTC kept. A transport that fails leaves the other waiting; when both fail, the error
  names both reasons. The one not kept is dropped right then (its signaling connection, or its
  iroh session, closed; nothing stays locked). The link is recorded with the transport it was
  paired over, as before, and `connector_log` records the decision (`paired` with `transport` and
  `why`; `pairing_attempt_failed` per transport). Requests on existing links keep their recorded
  transport (no record: WebRTC); `webrtc` and `iroh` keep their meaning.
- `pair_wallet`'s `transport` takes `auto` too. A new `pair_wallet` stops the pairing it replaces,
  over every transport, instead of leaving it waiting for its QR until the timeout.
- An unused iroh session is kept 30 minutes instead of 5 (`RADIX_CONNECTOR_IROH_IDLE_SECONDS`):
  once released, the wallet takes up to ~15 s to reach a new one.
- Tests (offline): the decision itself; end to end through the tools, a wallet linking over iroh
  while the WebRTC half waits at a signaling server on 127.0.0.1 (iroh recorded, the WebRTC
  connection closed, a ROLA request straight after, the link's lock released on
  `remove_wallet`); the grace both ways with the real iroh half; iroh unable to start (the pairing
  goes on over WebRTC) and both failing.

### Changed — diagnostics: the iroh path, from the logs alone

- `radixdlt-connect-iroh` (`mobile`): durations in whole milliseconds (`*_ms`) and the limit beside
  every timeout (`limit_ms`) — session start, waiting for the wallet, pairing, unconfirmed and
  unanswered requests. Sessions log how long they took to start, reuse and restart, why they
  close (`why`), how long they lived, and how long after (re)starting the wallet first reached
  them (`first`, `since_start_ms`). Each send is logged and reported (`Progress::Sending`, new).
  An answer nobody waits for says why it was kept: `late`, `unknown` or `duplicate`. Lines a
  misbehaving wallet could repeat without end are rate-limited per link.
- `radixdlt-connector-mcp`: `connector_log` covers what a field problem needs — which transport
  each request took and why, each change of a request's state, timeouts with their limits,
  late/duplicate/unknown answers; each pairing transport's progress and failure (with a
  category) and the time from the QR; each iroh link's life (opened and why, reused, idle release
  scheduled/cancelled/done, closed and why, how long the wallet took to come back); locks between
  connector processes (wait, taken, busy with the holder's pid, the lock of a crashed connector
  recovered); a re-paired wallet's old link closed (`link_replaced`); every tool call with its
  duration. The iroh libraries' own INFO+ events are bridged in (`iroh_protocol`, `iroh_sdk`),
  with `link_collision` (another holder of the link's key took this end's place) and
  `wallet_connection_replaced` carrying the held links' diagnostics. Every line has a `level`;
  IP addresses are blanked out of all of them; `connector_log` filters by `link` too.
  `list_wallets` and `check_wallet_connection` show who holds each iroh link, the last request
  and its outcome, and the last error. The links' sessions are closed when the connector exits.

### Changed — build

- `radixdlt-connect-iroh` and `radixdlt-connector-mcp` take the protocol library
  `radix-connect-iroh` from its published tag (`git`, `v0.1.0`) instead of a path to a sibling
  checkout, so a fresh clone builds (and CI can build them). `deny.toml` allows that one git
  source.

### Changed (breaking) — `radixdlt-connect-iroh`, SDK-to-SDK transport

- **Protocol version 1** (ALPN `radixdlt-connect-iroh/1`; a v0 peer now fails at the handshake
  instead of misreading). Every message travels on a QUIC stream of its own — the same 4-byte
  big-endian length and JSON body — and the receiver answers it with one status byte, so
  `IrohChannel::send_message` returns once the peer has *taken* the message. Both ends must be
  updated together; `docs/PROTOCOL.md` specifies it.
- **Bounded.** New `Limits` (per endpoint): `max_message_size` (8 MiB) — an incoming length over
  it is refused from the prefix alone, before anything is allocated (it used to allocate whatever
  the peer announced, up to 4 GiB, and to panic on 32-bit targets); `message_timeout` (60 s) — a
  message the peer has started must arrive whole in time (a peer trickling bytes held the task
  forever), while waiting for the NEXT message stays unbounded; `close_timeout` (5 s). A peer may
  have at most 8 streams open towards an endpoint and no unidirectional ones.
- **Cancel-safe.** A cancelled `recv_message` keeps what has arrived and the next call resumes it;
  a cancelled `send_message` resets its stream, so the peer never takes part of a message as a
  whole one. Before, a cancelled read or write left the stream mid-frame and the next read took
  JSON bytes for a length. After a refused, timed-out or malformed message the channel stays
  usable.
- **Peer identity and admission.** `IrohChannel::remote_id()` (the peer's `EndpointId`,
  authenticated by the handshake). `IrohConnector::builder()` with `secret`, `relay`, `limits`,
  and optionally `allow_only(ids)` / `admit(fn)`, which refuse other peers right after their
  handshake (anyone is admitted by default). `EndpointId` and `EndpointAddr` are re-exported.
- **Closing.** `IrohConnector::close()` (bounded; dropping one now closes it in the background
  instead of iroh aborting it and logging an error). `IrohChannel::close()` is now `async` and
  consumes the channel: it lets the last confirmation reach the peer, then closes (dropping does
  the same in the background). `IrohChannel::finish()` is gone (confirmations replace it);
  `wait_closed()` is bounded and returns whether the peer closed.
- `IrohError` is `#[non_exhaustive]`, with `TooLarge { size, max }`, `Refused { too_large }` and
  `Timeout { sending, after }`.
- `ticket()` carries the machine's interface (LAN) addresses as well as loopback (it had loopback
  only, although documented for LAN use). `Relay::Disabled` also turns port mapping off: a
  same-host/LAN endpoint no longer asks the router for a port. The docs no longer claim mDNS:
  `connect_to_endpoint_id` needs `Relay::Enabled` (n0's address lookup); LAN discovery is the
  application's. The docs recommend a key dedicated to the endpoint rather than a Radix
  account's, and explain why.
- **`protocol::Wallet` signs nothing without approval.** `Wallet::answer(channel, approver)`
  takes an `Approver` — one async method per kind of request (`approve_account_proof`,
  `approve_transaction`, `approve_pre_authorization`), each refusing unless implemented; `DenyAll`
  refuses everything. The approver sees a `Requester` (the peer's authenticated `EndpointId`, plus
  the dApp definition and origin the request claims) and the parsed request. Requests for another
  network (`wrongNetwork`), ROLA challenges that are not 32 bytes, dApp definitions that are not
  an account of the wallet's network, empty origins, manifests that do not compile and
  pre-authorizations without expiry (`invalidRequest`) are refused before the approver is asked;
  a refusal is `rejectedByUser`. It used to sign a ROLA proof for any dApp and origin, and sign and
  submit any manifest, for whoever connected.

### Fixed — `radixdlt-connect-iroh`

- `protocol::Wallet` reports a transaction that did not commit successfully as a `failure`
  naming its status (`submittedTransactionHasFailedTransactionStatus`,
  `submittedTransactionHasRejectedTransactionStatus`, `failedToPollSubmittedTransaction` when the
  outcome is not known yet, `failedToSubmitTransaction`) and its intent hash. It used to answer
  success with the hash whatever happened after the submission.
- `protocol::Wallet::new` maps network 1 to the mainnet Gateway and 2 to Stokenet, and refuses any
  other network (it sent every non-mainnet network to Stokenet); `with_gateway` refuses a Gateway
  of another network and now returns `Result`. New `protocol::public_gateway`.

### Added

- `radixdlt-connect` — `Connector::pair_with_password(password, timeout)` and
  `radixdlt-connect-iroh` — `MobileWalletConnector::pair_with_password(password, timeout, on)`:
  pairing over a password the caller made and showed (`linking::generate_password`,
  `linking::pairing_qr`), returning the wallet's public key — so one QR serves both transports.
  `pair` / `pair_with_progress` are now these with a password of their own, unchanged (the iroh
  session closed on failure or when the future is dropped, kept on success).

- `radixdlt-connector-mcp` — **Radix Connect over iroh as a second transport**, behind the same
  tools. `--transport webrtc|iroh|auto` (or `RADIX_CONNECTOR_TRANSPORT`; now `auto` by default,
  see above) says how new wallets are paired, and `pair_wallet` takes a
  `transport` per call; each wallet keeps the transport it was paired over (recorded in
  `transports.json`, by link hash — `connector.json` keeps its format), and `iroh` also sends
  every wallet's requests over iroh. n0's public relays by default, or `--iroh-relay <url>`. One
  connector process per wallet link over iroh (an OS lock, released after 30 idle minutes; others
  get `LINK_BUSY`). Failures keep the WebRTC codes where they mean the same, plus `NOT_CONFIRMED`,
  `LINK_BUSY` and `IROH_UNAVAILABLE`; the iroh timeouts, `check_wallet_connection`,
  `list_wallets` and `connector_log` carry the link's diagnostics line (no secrets). Built with
  `radixdlt-connect-iroh` without its `wallet` feature, so webrtc and iroh share one binary.
  End-to-end test: pairing and a ROLA request through the tools against a simulated wallet over
  an in-process relay.

- `radixdlt-connect-iroh` — **the Radix mobile wallet over iroh, from a server, no browser.**
  `mobile::MobileWalletConnector` speaks *Radix Connect over iroh v1* (the protocol of the Radix
  Wallet and the Connector Extension, through the `radix-connect-iroh` library) in the connector
  role, with `radixdlt_connect::Connector`'s API: `pair` (same QR, the wallet's `linkClient`
  signature required and verified; the pairing session is kept for the wallet's regular
  connection, and `pair` returns only once the wallet has closed the connection it linked over —
  which drops anything else sent on it — so a request right after pairing is not lost), `exchange` with `Progress`, `await_response`, `probe`, the `request_*` helpers,
  plus `open_link`, `subscribe`, `diagnostics`, `close_link`/`close`. One session per link,
  answers matched by `interactionId` (concurrent requests on one link), a request sent again only
  when nothing was sent, late answers kept for `await_response`, every wait bounded. Timeout
  errors carry the link's connectivity snapshot (relay reachable?, state, path, last error), and
  everything is logged through `tracing` without passwords or message contents. Example
  `mobile_login` (QR in the terminal, ROLA login verified); tests against a simulated wallet over
  an in-process relay. Relays: n0's public relays by default (`new()`; the wallet finds the server
  through n0's address lookup, nothing to run or configure), or one custom relay
  (`with_relay_url`); both ends make the same choice. `rust-version` is now 1.91 (iroh 1.4).
- `radixdlt-connect-types` — `linking`: the linking message, the pairing QR payload, `linkClient`
  parsing and signature verification, password and identity generation — one implementation for
  both transports. `qr` (feature `qr`): the QR matrix and its terminal rendering, moved from
  `radixdlt-connector-mcp`.

- `radixdlt-connect-types` / `radixdlt-connect` — **a refusal is not every failure.**
  `is_rejection_by_person(reason)`, `WalletInteractionError::rejected_by_person()` and
  `ConnectError::rejected_by_person()` tell the person pressing «reject» (`rejectedByUser`) apart from
  a request the wallet cannot answer (`invalidPersona` for a persona it does not hold, `wrongNetwork`,
  …). Callers that treated every wallet failure as a refusal gave up where they should have asked
  something else — PamAuthority asked a wallet for the last persona seen on a machine, got
  `invalidPersona`, and never went on to the full login.

- `radixdlt-gate` — **short-lived credentials from a wallet signature.** The generic layer of
  PamAuthority's Kubernetes and AWS Gates: `request::GateRequest` (destination, level, the
  requester's key, a short expiry and a nonce; the challenge is derived from it, so a Gate keeps no
  state), `proof::SignedProof::verify` (account and persona, offline, with `radixdlt-rola`) and,
  with the `seal` feature, answers sealed to the requester's one-time X25519 key
  (HKDF-SHA256 + ChaCha20-Poly1305, the challenge as associated data) so a replayed request only
  yields data its original requester can open. Wire-compatible with the Gates already deployed
  (pinned challenge vector). Also behind `radixdlt-sdk`'s `gate` feature.

- `radixdlt-connect` — **one turn per link across PROCESSES.** The link's turn and the
  `WALLET_SETTLE` wait used to live in each process, so two programs on one paired link (two AI
  sessions, a `sudo` prompt, a CLI) could open channels over each other and a request vanished
  without reaching the phone (seen on 2026-10-04: three transactions lost). The turn is now also an
  OS file lock (`fs4`) and the last close is shared on disk under
  `~/.config/radix-connect/links/`, named by a domain-separated BLAKE2b hash of the link password
  (no secret on disk, the same name for every build — unlike `DefaultHasher`). Verified with two
  processes asking at once on a real phone: the second waited its turn (6.2 s) and the settle
  time, and both were delivered and answered.
- `radixdlt-connector-mcp` 0.6.0 — `send_transaction` **simulates before ringing the phone**: a
  definitive preview failure is reported as `PREVIEW_FAILED` (retry_safe) and nothing is sent; a
  preview that cannot run lets the request through. `preview_only: true` returns the simulation
  without sending. Built on the turn above.

- `radixdlt-connect` — **reaching the phone with no internet.** A local signaling
  relay (`relay`, the public relay's protocol) and a minimal TURN-over-TCP server
  (`turn_server`); `Connector` tries the public relay and every configured one at
  once (`with_extra_signaling`, `RADIX_CONNECT_RELAYS`,
  `~/.config/radix-connect/relays`, `/etc/radix-connect/relays`) and keeps the one
  the wallet answers on. Verified on a real Android phone with no network: over
  USB tethering plus a WireGuard tunnel that is the phone's default route (the
  wallet's WebRTC ignores the tethering interface itself — Android never declares
  it to apps), and over `adb reverse` plus the local TURN. `RADIX_CONNECT_DEBUG=1`
  traces the negotiation.
- `radixdlt-connector-mcp` 0.5.0 — `relay [--listen] [--turn --turn-password]`.

- `radixdlt-connector-mcp` — `check-update` and `update` subcommands (and the
  `check_update` / `update_connector` tools): the newest `connector-v*` release is
  downloaded, checked against its published SHA-256 (released from 0.4.0 on), run
  once to prove it works, backed up and swapped in. The pairing is untouched.
- `radixdlt-connector-mcp` 0.4.0 — **the rest of the wallet protocol, and a
  connector that knows what reached the phone.** New tools: `request_login`
  (persona login, proof verified locally), `request_ownership_proof` (exact
  accounts / persona, nothing to pick), `request_authorized` (login / login without
  challenge / use persona, reset, proof of ownership, one-time and ONGOING accounts
  and persona data), `request_data` (exact or minimum quantities, phone numbers),
  plus `pending_requests`, `await_response`, `cancel_request`,
  `check_wallet_connection`, `check_dapp_identity` and `connector_log`. Every
  request is recorded (`requests.json`) and traced step by step
  (`connector.log`); a new request is refused while an earlier one is waiting in
  the wallet's queue (`PENDING_IN_WALLET`), because the wallet shows one at a time
  and cannot withdraw one. Failures carry a code, a stage, `retry_safe`, a hint and
  the interaction id, as text and `structuredContent`. Late answers to earlier
  requests are reported. Tool calls run concurrently and honour
  `notifications/cancelled`. The dApp identity is checked before sending, because
  a dApp its website's `radix.json` does not list is DROPPED by the wallet without
  an answer.
- `radixdlt-connect` — `Connector::exchange` / `await_response` / `probe` with a
  `Progress` observer (`Delivered` = the wallet confirmed receipt). Verified on a
  real phone, two wallet behaviours that lost requests are now handled: the wallet
  tears down the link's CURRENT channel ~5 s after an older connection closes (so
  the connector waits `WALLET_SETTLE` = 8 s between channels on a link), and it can
  miss a message sent the instant a channel opens (a 0.6 s grace, and a resend with
  the same interaction id when no receipt is confirmed, up to `SEND_ATTEMPTS`). A
  channel lost after delivery is reopened to keep waiting; channels close when
  dropped.
- `radixdlt-rola` / `radixdlt-address` — `verify_persona_proof` and
  `virtual_identity_address`: a persona's proof is checked against the identity its
  key derives to.

- `radixdlt-connect-types` / `radixdlt-connect` — **authorized LOGIN requests**, so the
  PERSON behind a wallet can be proven and not merely claimed: `login_request`,
  `extract_login` and `Connector::request_login`. An `unauthorizedRequest` can ask
  for a name, but a name is a string somebody typed — the answer never names the
  persona, so nothing that decides on *who* signed could be built on it. A login
  makes the identity key sign the same challenge the accounts sign, and the answer
  carries `identityAddress` beside a proof a ROLA verifier accepts as-is
  (`{challenge, address, type: "persona", proof}`).

  It also **asks for no persona data**, which matters more than it sounds: a
  one-time data request is mandatory for whoever answers it, so asking for a name
  and an email that a persona does not have does not yield an anonymous answer —
  the wallet closes. Verified against a real phone. The persona's own `label`
  comes back with every login for nothing, which is what a line of an audit trail
  should show when no name was configured.

- `radixdlt-connector-mcp` — local MCP server (stdio) that pairs a Radix Wallet
  over Radix Connect and gets transactions signed on the user's machine (pairing
  QR, `send_transaction`, pre-authorization, ROLA account proof, transaction
  status). Installs from GitHub (`cargo install --git …` or `scripts/install-connector.{sh,ps1}`).
- `radixdlt-i18n` — labelled `tr!` arms (`tr!(lang, en, Es: …, Fr: …)`) with an
  English fallback; `Lang` is now `#[non_exhaustive]`, so adding a language is
  non-breaking.

### Changed

- `radixdlt-connect` — `pair`, `await_link_client`, `crypto::linking_message` and
  `crypto::connection_id_hex` use `radixdlt-connect-types::linking` (same API). `pair` now refuses
  an identity whose public key does not belong to its private key before showing the QR (it used
  to show a QR every wallet rejects). New `From<LinkingError> for ConnectError`.
- `radixdlt-connector-mcp` — draws its QR with `radixdlt-connect-types::qr` (same output).
- `radixdlt-connect-iroh` — `protocol::Wallet` (and with it the `radixdlt-gateway-tx`
  dependency) is now behind a `wallet` feature, **on by default**, so nothing
  changes for existing users. It is separable because the Scrypto engine that
  crate pulls pins `regex` to exactly `1.9.3`, while `webrtc` requires `>=1.9.5`:
  a consumer that only wants the iroh transport could not share a dependency tree
  with the mobile wallet, over a dependency it never used. Take it with
  `default-features = false` to get the transport alone.

### Changed

- `reqwest` aligned to `0.13` across every crate and workspace (it was pinned to
  `0.12` while `iroh` already pulled `0.13`). Any binary combining the Gateway
  client with the iroh transport was linking two independent HTTP and TLS stacks;
  now there is one. HTTPS to the Gateway is covered by an ignored smoke test
  (`cargo test -p radixdlt-gateway-tx -- --ignored`), because a wrong TLS-roots
  feature fails at runtime rather than at compile time.

### Fixed

- `radixdlt-connect` — negotiation could hang past its timeout: the wallet's
  loopback candidates (offered when it has no network) made the ICE agent spin, a
  second wallet socket on the link (an app just closed) made it create a second
  offer that never returned, and connecting to a relay whose address was gone
  waited for the OS. Unusable remote candidates are dropped (a loopback RELAY
  candidate — a local TURN — is kept), a late wallet gets the same offer
  replayed, signaling connects are bounded, and the whole negotiation sits under
  one outer deadline.
- `radixdlt-connect-types` — a wallet `failure` keeps its `message` (which network
  it is on, which website it could not verify), not only the error type.
- `radixdlt-connect` — a data channel that closes is reported as such, not as
  «the signaling closed before the WebRTC channel opened».
- `radixdlt-connect` — requests sharing a paired link are now serialized. A link
  carries one conversation at a time, so two in flight at once raced on the same
  signaling rendezvous: the second failed within seconds and the wallet never
  prompted, which looked like an unresponsive phone. Requests queue on a turn keyed
  by the link password (process-wide, so callers that build a `Connector` per call
  are covered too); waiting is charged to the caller's timeout and a queue that does
  not clear reports the new `ConnectError::LinkBusy`.
- `radixdlt-connector-mcp` 0.2.1 — picks up the fix above; concurrent tool calls on
  the same paired wallet no longer fail silently.

### Changed

- `radixdlt-connect-iroh` — `IrohConnector::bind_with` takes a `Relay` enum
  instead of a `bool` flag (breaking).
- `radixdlt-connect` — all signing calls now correlate the wallet response by
  `interactionId`, discarding stale queued responses; `LinkState` documents the
  multi-device API in the README.

## [connector-v0.6.1] - 2026-10-25

`radixdlt-connector-mcp` 0.6.1: the same connector, rebuilt past the **ten** RustSec advisories
the 0.6.0 binary carried. Nothing changes for callers; the self-updater offers it.

### Security

- **Off the vulnerable `crypto-bigint 0.5`** ([RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387):
  operations meant to be constant-time could branch on secrets), and with it `ctutils` `0.4.5`
  ([RUSTSEC-2026-0385](https://rustsec.org/advisories/RUSTSEC-2026-0385)). No workspace locks it any more:
  - `radixdlt-rola` — `p256` `0.13` → `0.14` (`ecdsa 0.17`, `crypto-bigint 0.7.8`). The public API
    and the delegated vectors are unchanged; high-S raw signatures are still refused.
  - `radixdlt-connect` — `webrtc`/`rtc` `0.20` → `0.21`, whose DTLS no longer uses the RustCrypto
    curves (it takes `ring`, as rustls here already did). The TURN client and the test TURN server
    follow the new `rtc-turn`/`rtc-stun` API (explicit crypto provider, `now` on every call).
  - `rust-version` of `radixdlt-connect`, `radixdlt-connect-iroh` and `radixdlt-connector-mcp`
    raised to `1.87`, what their dependencies now require (as the rest of the SDK).
- TLS and certificates, all compatible updates:
  - `rustls` `0.23.46` ([RUSTSEC-2026-0346](https://rustsec.org/advisories/RUSTSEC-2026-0346),
    [0347](https://rustsec.org/advisories/RUSTSEC-2026-0347),
    [0348](https://rustsec.org/advisories/RUSTSEC-2026-0348),
    [0349](https://rustsec.org/advisories/RUSTSEC-2026-0349));
  - `rustls-webpki` `0.103.16` ([RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382),
    distrusted intermediates accepted);
  - `webpki-roots` and `webpki-root-certs` `1.0.11`
    ([RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381),
    [RUSTSEC-2026-0383](https://rustsec.org/advisories/RUSTSEC-2026-0383), distrusted emSign roots).
- `tokio` `1.54.1` ([RUSTSEC-2026-0386](https://rustsec.org/advisories/RUSTSEC-2026-0386), `AsyncFd`
  I/O safety); in `radixdlt-connect-iroh`'s lockfile, `h2` `0.4.20`
  ([RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258)).
- `radixdlt-connect`'s dev-dependency on `radixdlt-rola` names its version, as every other path
  dependency does (cargo-deny refused it as a wildcard).

## [connector-v0.3.2]

### Added

- `radixdlt-connect` — `Connector::request_account_proof_sharing`, which says exactly
  what the person is asked to share along with the signature (`PersonaRequest`: name,
  email, or nothing), and `extract_persona_email` to read the address back out of the
  wallet's answer. `request_account_proof` keeps its old meaning.
- `radixdlt-connector-mcp` — `request_account_proof` gained `request_email`: ask the
  wallet for the persona's name and email address, printed on their own lines. Nothing
  is asked for by default -- a data request is one-time and the wallet will not let the
  person approve until they provide what is asked for, so asking for an email they have
  not set up turns a login into a form.

## [connector-v0.3.1]

The same connector, a generation newer underneath. Nothing it exposes or stores
changes: paired wallets keep working untouched.

### Changed

- **The crypto stack moved up a generation**, and every crate with it: `ed25519-dalek`
  2 to 3, `aes-gcm` 0.10 to 0.11, `scrypt` 0.11 to 0.12, `base64` 0.22 to 0.23.
  `rand_core` is gone entirely -- `OsRng` no longer exists in either `rand_core`
  0.10 or `rand` 0.10, the ecosystem having settled on asking `getrandom` for OS
  entropy directly. Workspace version accordingly 0.1.0 to 0.2.0.

  Verified with a real wallet on both transports, because no test can cover it:
  a ROLA proof over the ordinary path (15.8 s) and one over the TCP relay with
  zero UDP sockets (14.8 s), each returned and verified natively.

  Key files written by the previous stack still open. scrypt 0.12 drops the
  output length from `Params::new` and takes it from the output buffer instead --
  the same 32 bytes, so the derivation is unchanged. That is now held down by a
  test carrying a real `key.json` produced by the OLD build, rather than by a
  re-encryption this build agrees with.
- `radixdlt-keystore` — `KeystoreError` is `#[non_exhaustive]`, so future error
  variants stop being breaking changes. Adding one is what forced this release to
  be breaking at all.

### Fixed

- `radixdlt-keystore`, `radixdlt-connect` — a key file or link password of the
  wrong length **panicked** instead of erroring. `Key::from_slice` and
  `Nonce::from_slice` panic on a length mismatch, and they sat in library code
  reading a `key.json` and a `connector.json` that a user can edit by hand: one
  stray hex character brought down the calling process. aes-gcm 0.11 deprecates
  both in favour of `TryFrom`, which is how they now report a corrupt field.
- Randomness failures are no longer silent. `fill_bytes` could not report one, so
  `CryptoBlob::encrypt` documented an error it was incapable of returning. Salt,
  nonce, connector identity and transaction nonce now all fail closed --
  `KeystoreError::RandomnessUnavailable` is the new variant. A predictable salt or
  nonce destroys the encryption that rests on it, and a transaction nonce the
  system could not randomise is one that can collide.

## [connector-v0.3.0]

The binary now works on networks that give it no UDP at all, and rides a WebRTC
stack that was rewritten underneath it.

### Added

- `radixdlt-connect` — **TURN over TCP/TLS**, so a wallet interaction can complete
  without opening a single UDP socket: for networks that block UDP (corporate
  firewalls, guest Wi-Fi) and hosts that do not offer it at all. Turn it on with
  `Connector::with_turn_tcp(TurnTcpServer::parse("turns:relay.example.com:443?transport=tcp", user, pass)?)`.

  This is not available anywhere else in the Rust ecosystem: `webrtc-ice` leaves
  TCP and TURNS as an unimplemented `TODO` in `gather_candidates_relay` (still so
  in 0.17.2, its newest release), and `webrtc` 0.20 discards any `turns:` or
  non-UDP URL with a warning. The consequence is worth knowing even if you never
  use this: a `turns:…?transport=tcp` entry in an ICE configuration — including
  the one in this crate's own `radix_default_ice_servers` — has never done
  anything, so a UDP-less network has had no fallback and the symptom is a wallet
  that appears not to respond.

  The Sans-I/O TURN client already spoke TCP; only the half that owns a stream was
  missing. `Runtime` and `AsyncUdpSocket` being public traits is what keeps this
  out of a fork: the peer connection is handed a runtime whose "UDP socket" is the
  allocation itself. Verified against a real wallet — proof returned and verified
  in 21.9 s with zero UDP sockets and two outbound TCP/443 connections.
- `radixdlt-connect` — `probe_relay_candidates`, which allocates on a TCP relay and
  reports the candidates that would be offered, with no wallet involved. A relay
  that authenticates but advertises an unreachable address fails exactly like one
  that is down — a channel that never opens — and this separates them.
- `radixdlt-connect` — `Connector::with_relay_only`, restricting ICE to relay
  candidates. Off by default.

### Changed

- `radixdlt-connect` — moved to `webrtc` 0.20 (from 0.11), which is a different
  library rather than a newer one: webrtc-rs split into a Sans-I/O core (`rtc`)
  with a thin async layer over it. Peer events arrive through a
  `PeerConnectionEventHandler` trait instead of closures, and data-channel events
  are pulled with `poll()` instead of pushed through `on_message`. None of that
  reaches this crate's own API — `Connector` is unchanged — but `Channel::dc` is
  now `Arc<dyn DataChannel>`.
- Every workspace refreshed to the latest compatible dependencies. The crypto
  majors (`ed25519-dalek` 2→3, `rand_core`, `aes-gcm`, `scrypt`, `base64`) are
  deliberately not part of this: they interlock and they sit under signature
  verification, so they get their own change with its own verification.

## [connector-v0.2.5]

### Fixed

- **Versions 0.2.2 to 0.2.4 could not talk to the wallet at all.** They panicked
  on rustls' crypto provider the moment the signalling connection opened, so no
  request ever reached the phone. The binary links two providers — aws-lc-rs
  arrives with `reqwest`, ring with `webrtc` — and rustls refuses to guess
  between them; one is now installed explicitly at startup.

  Introduced in 0.2.2 by the move to `reqwest` 0.13, and missed because the
  release check only exercised the MCP handshake and a Gateway call. Both are
  HTTP, both kept working, and neither touches the wallet path that is the whole
  point of the tool. `scripts/qa/verify-release.sh` now opens that path too.

  **If you are on 0.2.2, 0.2.3 or 0.2.4, upgrade.** 0.2.1 is unaffected.

## [connector-v0.2.4]

### Fixed

- `radixdlt-connect-types` — a `networkId` arriving from the peer was converted
  with `as u8`, which TRUNCATES instead of rejecting: a value of 258 was read as
  `2`, which is stokenet. Getting the network wrong in a wallet interaction means
  signing against the wrong ledger, so an out-of-range value is now refused
  rather than folded into a valid one.

### Changed

- Rebuilt with the SDK's new quality gates in place (workspace lints, documented
  public API, verified MSRV). No API change; the binaries differ only by the fix
  above and by documentation.

## [connector-v0.2.3]

### Fixed

- `radixdlt-connector-mcp` — messages now follow the SYSTEM's language when the
  process inherits no locale. An MCP server is started by an agent, not from a
  shell, so `LANG` is usually absent: every error it surfaced came out in English
  even on a machine configured in Spanish. `Lang::detect` now consults
  `/etc/locale.conf` and `/etc/default/locale` before falling back to English.

  Verified on Linux only — no macOS or Windows hardware here. Those systems do
  not use these files, so their behaviour is unchanged (English fallback), which
  is what 0.2.2 already did everywhere.

## [connector-v0.2.2]

### Changed

- `radixdlt-connector-mcp` — rebuilt on `reqwest` 0.13. The binary is otherwise
  unchanged: same MCP protocol, same commands, no API difference.

  TLS root certificates now come from the OPERATING SYSTEM's trust store
  (`rustls-platform-verifier`) instead of being compiled into the binary
  (`webpki-roots`), which is what `reqwest` 0.13's `rustls` feature selects.
  A corporate or self-managed CA installed on the machine is honoured from now
  on, and the three platforms no longer share one embedded root set.

  Verified against the live Stokenet Gateway on **Linux only** — the maintainers
  have no macOS or Windows hardware. The platform verifier exists precisely to
  handle those, and no failure is expected, but if HTTPS to the Gateway fails on
  macOS or Windows after upgrading, this change is where to look; 0.2.1 is the
  last release with embedded roots.

## [0.1.0]

First release. All crates start at `0.1.0`.

### Added

- `radixdlt-i18n` — system-locale detection and bilingual (English/Spanish) text helpers.
- `radixdlt-address` — native Ed25519 virtual-account address derivation.
- `radixdlt-rola` — native ROLA (Radix Off-Ledger Authentication) verification.
- `radixdlt-keystore` — encrypted Ed25519 keystore (scrypt + AES-256-GCM), `key.json`-compatible.
- `radixdlt-gateway-tx` — Gateway client plus local transaction building, signing and submission.
- `radixdlt-connect` — Radix Connect over WebRTC (talks to the Radix mobile wallet).
- `radixdlt-connect-iroh` — Radix Connect over Iroh/QUIC for pure-Rust SDK-to-SDK peers.
- `radixdlt-sdk` — umbrella crate re-exporting the above behind feature flags.

[Unreleased]: https://github.com/genkipool/radixdlt-rust-sdk/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/genkipool/radixdlt-rust-sdk/releases/tag/v0.1.0
