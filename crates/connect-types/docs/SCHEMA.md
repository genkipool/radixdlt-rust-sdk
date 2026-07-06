# radixdlt-connect-types — Wallet-Interaction Schema Reference

***English** · [Español](SCHEMA.es.md)*

Status: reflects the builders/parsers in `crates/connect-types/src/lib.rs`. This
is the **transport-agnostic** Radix Connect message schema shared by both
transports — [WebRTC](../../connect/docs/PROTOCOL.md) and
[Iroh](../../connect-iroh/docs/PROTOCOL.md) speak exactly this JSON.

Two sides:

- **dApp side** — builds *requests*, parses *responses* (`*_request`,
  `extract_*`).
- **wallet side** — parses *requests*, builds *responses* (`parse_*_request`,
  `*_response`).

---

## 1. Envelopes

### Request

```json
{
  "interactionId": "<uuid-v4>",
  "metadata": {
    "version": 2,
    "networkId": <u8>,
    "dAppDefinitionAddress": "account_...",
    "origin": "<origin string>"
  },
  "items": { "discriminator": "<kind>", ... }
}
```

The **kind** is `items.discriminator`, read by `interaction_discriminator`:

| `items.discriminator` | Interaction |
| --- | --- |
| `unauthorizedRequest` | account proof (ROLA) **or** account share |
| `transaction` | sign + submit a manifest |
| `preAuthorizationRequest` | sign a subintent |

`metadata` is built from a `DappContext { network_id, dapp_definition, origin }`
and is identical across all requests.

### Response

```json
{ "discriminator": "success" | "failure", "interactionId": "<echoed>", "items": { ... } }
```

- `failure` → top-level `"error"` string, no `items`. Parsers surface it as
  `WalletInteractionError::WalletRejected(error)`.
- `success` → `items` shaped per interaction (below).

---

## 2. Account proof — ROLA (`account_proof_request` / `account_proof_response`)

**Request** (`request_persona = true` adds `oneTimePersonaData`):

```json
{
  "interactionId": "…", "metadata": { … },
  "items": {
    "discriminator": "unauthorizedRequest",
    "oneTimeAccounts": {
      "challenge": "<hex>",
      "numberOfAccounts": { "quantifier": "atLeast", "quantity": 1 }
    },
    "oneTimePersonaData": { "isRequestingName": true }
  }
}
```

**Response** (`persona_name` optional):

```json
{
  "discriminator": "success", "interactionId": "…",
  "items": {
    "discriminator": "unauthorizedRequest",
    "oneTimeAccounts": {
      "accounts": [ { "address": "account_…" } ],
      "proofs": [ {
        "accountAddress": "account_…",
        "proof": { "publicKey": "<hex>", "signature": "<hex>", "curve": "curve25519" }
      } ]
    },
    "oneTimePersonaData": { "name": "…" }
  }
}
```

Read with `extract_proofs` (→ `(address, proof)` pairs) and
`extract_persona_name`. The proof is a ROLA signature verified by
[`radixdlt-rola`](../../rola).

---

## 3. Account share — no proof (`account_request`)

Same `unauthorizedRequest` envelope **without** a `challenge` (so no signature —
just learn the account address(es)):

```json
{
  "items": {
    "discriminator": "unauthorizedRequest",
    "oneTimeAccounts": { "numberOfAccounts": { "quantifier": "atLeast", "quantity": 1 } }
  }
}
```

Response accounts read with `extract_accounts`.

---

## 4. Transaction — sign + submit (`transaction_request` / `transaction_response`)

**Request** — `blobs` are hex byte blobs referenced by the manifest via
`Blob("<blake2b-256 hash>")` (empty for ordinary manifests):

```json
{
  "items": {
    "discriminator": "transaction",
    "send": { "version": 1, "transactionManifest": "<manifest>", "blobs": ["<hex>"], "message": "" }
  }
}
```

**Response** — read with `extract_transaction_intent_hash`:

```json
{
  "discriminator": "success", "interactionId": "…",
  "items": { "discriminator": "transaction", "send": { "transactionIntentHash": "txid_…" } }
}
```

---

## 5. Pre-authorization — sign a subintent (`pre_authorization_request` / `pre_authorization_response`)

**Request**:

```json
{
  "items": {
    "discriminator": "preAuthorizationRequest",
    "request": {
      "discriminator": "subintent",
      "version": 1, "manifestVersion": 2,
      "subintentManifest": "<manifest>",
      "blobs": [], "message": "",
      "expiration": { "discriminator": "expireAfterDelay", "expireAfterSeconds": <u64> }
    }
  }
}
```

**Response** — read with `extract_signed_partial_transaction`:

```json
{
  "discriminator": "success", "interactionId": "…",
  "items": {
    "discriminator": "preAuthorizationRequest",
    "response": { "signedPartialTransaction": "<hex>" }
  }
}
```

---

## 6. Failure (`failure_response`)

Any interaction can fail with:

```json
{ "discriminator": "failure", "interactionId": "…", "error": "<reason>" }
```

The `extract_*` parsers detect this and return
`WalletInteractionError::WalletRejected(<reason>)` (e.g. `"rejectedByUser"`).

---

## 7. Builder / parser map

| Interaction | dApp builds | dApp reads | wallet parses | wallet builds |
| --- | --- | --- | --- | --- |
| Account proof | `account_proof_request` | `extract_proofs`, `extract_persona_name` | `parse_account_proof_request` | `account_proof_response` |
| Account share | `account_request` | `extract_accounts` | — | — |
| Transaction | `transaction_request` | `extract_transaction_intent_hash` | `parse_transaction_request` | `transaction_response` |
| Pre-authorization | `pre_authorization_request` | `extract_signed_partial_transaction` | `parse_pre_authorization_request` | `pre_authorization_response` |
| (any) failure | — | (via `extract_*`) | — | `failure_response` |

Discriminator lookup for the wallet side: `interaction_discriminator(request)`
returns `items.discriminator`.
