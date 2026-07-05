# radixdlt-connect

The **Radix Connect** protocol in native Rust: signaling, WebRTC and wallet
interactions — a drop-in for `@radixdlt/radix-connect-webrtc` + `@roamhq/wrtc`. Pair
with the Radix **mobile wallet**, open a WebRTC channel and exchange wallet
interactions (ROLA account proofs, transactions, pre-authorizations).

***English** · [Español](README.es.md)*

```toml
[dependencies]
radixdlt-connect = "0.1"
```

### Pairing (QR)

```rust
use radixdlt_connect::{Connector, LinkState};
use std::time::Duration;

let connector = Connector::new(); // public Radix ICE set; override with with_ice_servers()
let (wallet_pk, password) = connector
    .pair(&identity_priv_hex, &identity_pub_hex,
          |qr_json| render_qr(&qr_json),      // show this QR to the mobile wallet
          Duration::from_secs(120))
    .await?;
```

### Wallet interactions

With a paired link password, the connector opens a WebRTC channel and exchanges one
interaction. Responses are correlated by `interactionId`, so stale responses left in
the wallet's request queue from earlier attempts are discarded automatically.

```rust
use radixdlt_connect::DappContext;

let ctx = DappContext::new(network_id, dapp_definition, origin);

// ROLA account proof ("log in with Radix"):
let response = connector
    .request_account_proof(&password, &challenge, &ctx, false, Duration::from_secs(120))
    .await?;

// Transaction: the wallet signs and submits, returns the intent hash:
let txid = connector
    .request_transaction(&password, &manifest, "", &ctx, Duration::from_secs(300))
    .await?;

// Pre-authorization: the wallet signs a subintent WITHOUT submitting:
let signed_hex = connector
    .request_pre_authorization(&password, &subintent, "", 600, &ctx, Duration::from_secs(300))
    .await?;
```

### Link persistence and multiple devices (`state::LinkState`)

`LinkState` reads/writes the same `connector.json` the Node connector uses, so an
existing pairing keeps working. It supports **several paired wallets at once** —
each link has its own password (hence its own `connectionId`), so a daemon can
reach one specific device:

```rust
use radixdlt_connect::state::{Link, LinkState};

let mut state = LinkState::load(path)?;         // migrates the legacy single `link`
for link in state.all_links() { /* list devices; `link.label` is optional */ }

let pw = state.password_bytes()?;               // first link (single-device flows)
let pw = state.password_bytes_for(&wallet_pk)?; // one specific device

state.add_or_replace_link(Link { /* re-pairing a device refreshes it */ .. });
state.remove_link(&wallet_pk);
state.save(path)?;                              // 0600 permissions on Unix
```

For pure-Rust peer-to-peer connections (no mobile wallet), see the alternative
transport [`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh).
Both transports share the interaction schema in
[`radixdlt-connect-types`](https://crates.io/crates/radixdlt-connect-types).

Error messages are localized to the system language.

## License

Licensed under either of MIT or Apache-2.0 at your option.
