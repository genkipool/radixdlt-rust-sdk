# radixdlt-connect-auto

Talk to the **Radix Wallet** over **iroh** or **WebRTC**, whichever it can, with nothing to choose.

Radix Connect reaches the wallet on the phone over two transports: WebRTC, which every Radix Wallet
speaks, and iroh (QUIC), for wallets with it on. This crate uses both, so that neither you nor the
person using your app has to choose, and every request takes the fastest way:

- **One QR** pairs over both at once: the wallet links over iroh when it has it on, otherwise over
  WebRTC. The stored link records which.
- **Each request** over a link paired over iroh goes over iroh at once when its connection is up.
  Otherwise it **races both**: the first transport ready to carry it carries it — and only that one,
  so the wallet never gets a request twice — and the answer is awaited over both. A request is never
  slower than WebRTC, and as fast as iroh when iroh is there.
- **Staying ready:** a process that stays up (a daemon, an agent) can `keep_listening` on its links:
  the wallet connects as soon as its app opens, and the next request goes at once.
- **Removing a link** tells the wallet over every transport it may be on, so it removes its end too.

```toml
[dependencies]
radixdlt-connect-auto = "0.3"
```

```rust,no_run
use std::time::Duration;
use radixdlt_connect_auto::{DappContext, LinkState, PersonaRequest, WalletConnector};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let wallet = WalletConnector::new();

// Pairing: show the QR, store the link (it records the transport it was paired over).
let mut state = LinkState::load("connector.json")?;
let (password, paired) = wallet
    .pair(&state.identity.private_key, &state.identity.public_key, |qr| println!("{qr}"), Duration::from_secs(300))
    .await?;
state.add_or_replace_link(paired.into_link(&password, Some("my phone".into()), "now".into()));
state.save("connector.json")?;

// A request: over iroh or WebRTC, whichever is faster right now.
let ctx = DappContext::new(2, "account_tdx_2_…", "https://example.org");
let answer = wallet
    .request_login(&state.all_links()[0], "<challenge hex>", &ctx, PersonaRequest::NAME, Duration::from_secs(120))
    .await?;
# Ok(()) }
```

An `Error` says how far a request got — nothing sent (`WalletUnreachable`, `NoNetwork`), sent but
not confirmed (`NotConfirmed`), on the phone and unanswered (`NoAnswer`), refused (`WalletRejected`,
see `rejected_by_person`), or the wallet removed the link (`LinkRemoved`) — whichever transport
carried it.

**Only one transport?** Use [`radixdlt-connect-webrtc`](../connect-webrtc) or
[`radixdlt-connect-iroh`](../connect-iroh) directly.

## How the race is decided

The decisions — which transport a pairing keeps, which one carries a request — are plain state
machines with no network and no clock (`src/race.rs`), tested in every order of events; the
connector only feeds them what happens. The integration tests run against a simulated Radix Wallet
over a real iroh relay in the test process.

## License

MIT OR Apache-2.0.
