# radixdlt-connect-types

Transport-agnostic **Radix Connect** wallet-interaction message schema — the request
builders, response builders and parsers for account proofs, transactions and
pre-authorizations. Shared by the WebRTC transport
([`radixdlt-connect`](https://crates.io/crates/radixdlt-connect)) and the Iroh
transport ([`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh))
so both speak exactly the same JSON.

*ES — Esquema de mensajes de Radix Connect independiente del transporte (peticiones, respuestas y parseadores).*

```toml
[dependencies]
radixdlt-connect-types = "0.1"
```

- **dApp side** — build requests, parse responses: `account_proof_request`,
  `extract_proofs`, `extract_transaction_intent_hash`, …
- **Wallet side** — parse requests, build responses: `parse_account_proof_request`,
  `account_proof_response`, …

Error messages are localized to the system language. Part of the
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## License

Licensed under either of MIT or Apache-2.0 at your option.
