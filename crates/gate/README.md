# radixdlt-gate

Short-lived credentials from a **Radix Wallet signature**, bound to a key the requester holds.

A *Gate* runs where the protected thing is — a Kubernetes cluster, a cloud account, a database —
and trusts only the wallet's ROLA signature plus whatever on-ledger rule it applies (an access
badge, an allow-list…). It keeps no state and shares no secret with anybody, so nothing stolen
elsewhere opens it. PamAuthority's Kubernetes and AWS Gates are built on this crate.

```toml
radixdlt-gate = { version = "0.2", features = ["seal"] }
```

## The three pieces

**1. The request** the wallet signs (`request::GateRequest`): destination (`aud`), level, the
requester's public key (`key`), a short expiry (`exp`, at most 5 minutes ahead) and a nonce. The
challenge is SHA-256 of `"pamauthority-gate/v1\n"` + the request as compact JSON, so the Gate
recomputes it rather than remembering it.

```rust
use radixdlt_gate::request::GateRequest;

let request = GateRequest::new("k8s:prod", "view", &my_key_hash, now, 120);
let challenge = request.challenge();          // the wallet signs this (ROLA)
// … the Gate, later:
request.check("k8s:prod", now)?;               // version, shape, destination, window
```

**2. The proof** (`proof::SignedProof`): the wallet's `{address, proof, personaProof?}`, verified
offline — the key must derive to the address and have signed THIS challenge for THIS dApp and
origin.

```rust
use radixdlt_gate::proof::{Binding, SignedProof};

let signer = proof.verify(Binding { challenge: &challenge, dapp_definition, origin, network_id: 2 })?;
signer.name(); // the persona when one signed, else the account
```

**3. Sealed answers** (`seal`, feature `seal`): when the Gate answers with live credentials it
seals them to the requester's one-time X25519 key (in `key`), with the challenge as associated
data. A captured or replayed request yields only data the original requester can open.

```rust
use radixdlt_gate::seal::{seal, OneTimeKey};

let mine = OneTimeKey::generate();             // requester: key.public_hex() goes in the request
let sealed = seal(credentials, &mine.public_hex(), &challenge)?; // Gate
let plain = mine.open(&sealed, &challenge)?;   // requester only
```

## What it does not decide

What a Gate issues (a certificate, a cloud session, a token), which on-ledger rule it applies and
for how long are the Gate's own business.

## License

Apache-2.0
