# radixdlt-connect-types — Referencia del esquema de interacción con la wallet

*[English](SCHEMA.md) · **Español***

Estado: refleja los builders/parsers de `crates/connect-types/src/lib.rs`. Este
es el esquema de mensajes de Radix Connect **agnóstico del transporte**,
compartido por ambos transportes — [WebRTC](../../connect/docs/PROTOCOL.es.md) e
[Iroh](../../connect-iroh/docs/PROTOCOL.es.md) hablan exactamente este JSON.

Dos lados:

- **lado dApp** — construye *peticiones*, parsea *respuestas* (`*_request`,
  `extract_*`).
- **lado wallet** — parsea *peticiones*, construye *respuestas* (`parse_*_request`,
  `*_response`).

---

## 1. Envolturas

### Petición

```json
{
  "interactionId": "<uuid-v4>",
  "metadata": {
    "version": 2,
    "networkId": <u8>,
    "dAppDefinitionAddress": "account_...",
    "origin": "<cadena de origen>"
  },
  "items": { "discriminator": "<tipo>", ... }
}
```

El **tipo** es `items.discriminator`, leído por `interaction_discriminator`:

| `items.discriminator` | Interacción |
| --- | --- |
| `unauthorizedRequest` | prueba de cuenta (ROLA) **o** compartir cuenta |
| `transaction` | firmar + enviar un manifiesto |
| `preAuthorizationRequest` | firmar un subintent |

`metadata` se construye a partir de un `DappContext { network_id,
dapp_definition, origin }` y es idéntico en todas las peticiones.

### Respuesta

```json
{ "discriminator": "success" | "failure", "interactionId": "<eco>", "items": { ... } }
```

- `failure` → cadena `"error"` de nivel superior, sin `items`. Los parsers lo
  exponen como `WalletInteractionError::WalletRejected(error)`.
- `success` → `items` con la forma propia de la interacción (abajo).

---

## 2. Prueba de cuenta — ROLA (`account_proof_request` / `account_proof_response`)

**Petición** (`request_persona = true` añade `oneTimePersonaData`):

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

**Respuesta** (`persona_name` opcional):

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

Se lee con `extract_proofs` (→ pares `(address, proof)`) y
`extract_persona_name`. La prueba es una firma ROLA verificada por
[`radixdlt-rola`](../../rola).

---

## 3. Compartir cuenta — sin prueba (`account_request`)

La misma envoltura `unauthorizedRequest` **sin** `challenge` (por tanto sin firma
— solo aprender la(s) dirección(es) de cuenta):

```json
{
  "items": {
    "discriminator": "unauthorizedRequest",
    "oneTimeAccounts": { "numberOfAccounts": { "quantifier": "atLeast", "quantity": 1 } }
  }
}
```

Las cuentas de la respuesta se leen con `extract_accounts`.

---

## 4. Transacción — firmar + enviar (`transaction_request` / `transaction_response`)

**Petición** — los `blobs` son blobs de bytes en hex referenciados por el
manifiesto vía `Blob("<hash blake2b-256>")` (vacío para manifiestos normales):

```json
{
  "items": {
    "discriminator": "transaction",
    "send": { "version": 1, "transactionManifest": "<manifiesto>", "blobs": ["<hex>"], "message": "" }
  }
}
```

**Respuesta** — se lee con `extract_transaction_intent_hash`:

```json
{
  "discriminator": "success", "interactionId": "…",
  "items": { "discriminator": "transaction", "send": { "transactionIntentHash": "txid_…" } }
}
```

---

## 5. Pre-autorización — firmar un subintent (`pre_authorization_request` / `pre_authorization_response`)

**Petición**:

```json
{
  "items": {
    "discriminator": "preAuthorizationRequest",
    "request": {
      "discriminator": "subintent",
      "version": 1, "manifestVersion": 2,
      "subintentManifest": "<manifiesto>",
      "blobs": [], "message": "",
      "expiration": { "discriminator": "expireAfterDelay", "expireAfterSeconds": <u64> }
    }
  }
}
```

**Respuesta** — se lee con `extract_signed_partial_transaction`:

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

## 6. Fallo (`failure_response`)

Cualquier interacción puede fallar con:

```json
{ "discriminator": "failure", "interactionId": "…", "error": "<motivo>" }
```

Los parsers `extract_*` lo detectan y devuelven
`WalletInteractionError::WalletRejected(<motivo>)` (p. ej. `"rejectedByUser"`).

---

## 7. Mapa de builders / parsers

| Interacción | dApp construye | dApp lee | wallet parsea | wallet construye |
| --- | --- | --- | --- | --- |
| Prueba de cuenta | `account_proof_request` | `extract_proofs`, `extract_persona_name` | `parse_account_proof_request` | `account_proof_response` |
| Compartir cuenta | `account_request` | `extract_accounts` | — | — |
| Transacción | `transaction_request` | `extract_transaction_intent_hash` | `parse_transaction_request` | `transaction_response` |
| Pre-autorización | `pre_authorization_request` | `extract_signed_partial_transaction` | `parse_pre_authorization_request` | `pre_authorization_response` |
| fallo (cualquiera) | — | (vía `extract_*`) | — | `failure_response` |

Búsqueda del discriminador para el lado wallet:
`interaction_discriminator(request)` devuelve `items.discriminator`.
