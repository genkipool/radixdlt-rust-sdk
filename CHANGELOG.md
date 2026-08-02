# Changelog

All notable changes to the RadixDLT Rust SDK are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates
follow [Semantic Versioning](https://semver.org/). While the crates are in `0.x`,
minor versions may contain breaking changes.

## [Unreleased]

### Added

- `radixdlt-connector-mcp` — local MCP server (stdio) that pairs a Radix Wallet
  over Radix Connect and gets transactions signed on the user's machine (pairing
  QR, `send_transaction`, pre-authorization, ROLA account proof, transaction
  status). Installs from GitHub (`cargo install --git …` or `scripts/install-connector.{sh,ps1}`).
- `radixdlt-i18n` — labelled `tr!` arms (`tr!(lang, en, Es: …, Fr: …)`) with an
  English fallback; `Lang` is now `#[non_exhaustive]`, so adding a language is
  non-breaking.

### Changed

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
