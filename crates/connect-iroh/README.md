# radixdlt-connect-iroh

An [iroh](https://iroh.computer) (QUIC) peer-to-peer transport for the RadixDLT Rust
SDK — an alternative to WebRTC for **pure-Rust SDK-to-SDK** connections.

*ES — Transporte P2P sobre iroh (QUIC), alternativa a WebRTC para peers puro-Rust.*

> This does **not** talk to the Radix mobile wallet (which only speaks Radix Connect
> over WebRTC; use [`radixdlt-connect`](https://crates.io/crates/radixdlt-connect)
> for that). It connects two peers that both use the SDK — e.g. a desktop signer, a
> server, or a device — so flows like ROLA "log in with Radix" can run entirely in
> Rust, no phone involved.

```toml
[dependencies]
radixdlt-connect-iroh = "0.1"
```

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

See `examples/login.rs` (`cargo run --example login`) for the full flow, and
`tests/` for the low-level, login, and transaction/pre-auth tests. Error messages are
localized to the system language.

## License

Licensed under either of MIT or Apache-2.0 at your option.
