# radixdlt-connect

The **Radix Connect** protocol in native Rust: signaling, WebRTC and wallet
interactions — a drop-in for `@radixdlt/radix-connect-webrtc` + `@roamhq/wrtc`. Pair
with the Radix **mobile wallet**, open a WebRTC channel and exchange wallet
interactions (ROLA account proofs, transactions, pre-authorizations).

*ES — El protocolo Radix Connect en Rust (señalización + WebRTC) para hablar con la Radix Wallet del móvil.*

```toml
[dependencies]
radixdlt-connect = "0.1"
```

```rust
use radixdlt_connect::{Connector, DappContext};
use std::time::Duration;

let connector = Connector::new(); // public Radix ICE set; override with with_ice_servers()
let ctx = DappContext::new(network_id, dapp_definition, origin);
let response = connector
    .request_account_proof(&password, &challenge, &ctx, false, Duration::from_secs(120))
    .await?;
```

For pure-Rust peer-to-peer connections (no mobile wallet), see the alternative
transport [`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh).

Error messages are localized to the system language.

## License

Licensed under either of MIT or Apache-2.0 at your option.
