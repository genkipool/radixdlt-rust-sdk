# Delegated ROLA v1 — Specification

***English** · [Español](DELEGATED.es.md)*

Status: reflects `crates/rola/src/{delegation,signed,canonical}.rs`. Test vectors:
[`tests/vectors/delegated-v1.json`](../tests/vectors/delegated-v1.json), verified by this crate and
by the TypeScript verifier with the same verdicts.

Radix signs with Ed25519 (or secp256k1). The hardware every computer and phone has for free —
TPM 2.0, Apple's Secure Enclave, passkeys (WebAuthn) — signs with **P-256** and never lets the key
out. Delegated ROLA joins the two: the Radix keys sign **once** that a P-256 key answers for their
addresses, and are destroyed.

---

## 1. Flow

```mermaid
sequenceDiagram
  participant D as Device (installer)
  participant H as Hardware (TPM / Enclave / passkey)
  participant V as Verifier
  D->>H: create a non-exportable P-256 key
  H-->>D: public key (SEC1, compressed)
  D->>D: generate account + identity Ed25519 keys IN MEMORY
  D->>D: sign the delegation certificate with both, then zeroize them
  Note over D: the certificate is public; nothing secret is stored
  V->>D: challenge (as ROLA)
  D->>H: sign SHA-256(ROLA message) — after fingerprint / PIN (people), or by itself (agents)
  D->>V: { curve: "delegated", delegation, signature }
  V->>V: certificate signed by keys that derive to the addresses? P-256 signature over THIS message?
```

## 2. The delegation certificate

```json
{
  "v": 1,
  "network": 2,
  "account": "account_tdx_2_…",
  "identity": "identity_tdx_2_…",
  "delegate": { "alg": "p256", "key": "<33-byte compressed SEC1 point, hex>" },
  "kind": "tpm" | "secure-enclave" | "webauthn",
  "rpId": "<only for webauthn>",
  "subject": "person" | "agent",
  "purposes": ["login", "kube"],          // only for agents
  "label": "eva",                       // the OS user name, or the invitation's
  "device": "pc-de-eva",
  "createdAt": 1760000000,
  "attestation": "<optional, opaque>",
  "signatures": {
    "account":  { "publicKey": "<ed25519 hex>", "signature": "<hex>" },
    "identity": { "publicKey": "<ed25519 hex>", "signature": "<hex>" }
  }
}
```

**Signed bytes**

```
digest = blake2b_256( "pamauthority-delegation/v1\n" ‖ canonical(certificate − "signatures") )
```

`canonical`: object keys sorted by UTF-16 code units (JavaScript's default sort) at every depth,
no whitespace, strings escaped as `JSON.stringify` does, integers only (a float makes the
certificate invalid).

**A certificate is valid iff**

1. it parses with exactly these fields (unknown fields are refused), `v = 1`, `alg = "p256"`;
2. `rpId` is present **iff** `kind = "webauthn"`; `purposes` is present **iff** `subject = "agent"`;
3. `delegate.key` is a valid P-256 point;
4. `Ed25519.verify_strict(signatures.account.publicKey, digest)` and
   `virtual_account_address(publicKey, network) = account`;
5. the same for `identity` with `virtual_identity_address`.

The domain prefix keeps a certificate from ever being mistaken for a ROLA message (those start
with the byte `R`). There is no expiry: the root keys no longer exist to renew it. A certificate
dies with its addresses (taken off a list, badge burnt).

## 3. The delegated proof

It takes the place of the wallet's `{publicKey, signature}` in the `proof` of an account and in
the `proof` of a persona (the same object can serve both).

| `kind` | Proof | Signed bytes |
|---|---|---|
| `tpm`, `secure-enclave` | `{"curve":"delegated","delegation":{…},"signature":"<r‖s hex, 64 bytes>"}` | ECDSA-P256 over `SHA-256(ROLA message)`; **S must be low** |
| `webauthn` | `{"curve":"delegated","delegation":{…},"webauthn":{"authenticatorData","clientDataJSON","signature"}}` (base64url, signature DER) | WebAuthn: `authenticatorData ‖ SHA-256(clientDataJSON)` |

`ROLA message` is exactly ROLA's: `blake2b_256("R" ‖ challenge ‖ len(dApp) ‖ dApp ‖ origin)`.

**For WebAuthn** the verifier also requires: `rpIdHash = SHA-256(rpId)`; flags **UP and UV**
(user present and verified: fingerprint or PIN); `clientDataJSON.type = "webauthn.get"`,
`challenge = base64url(ROLA message)`, `origin = ` the request's origin, `crossOrigin ≠ true`. The
`BE` flag (backup eligible: a passkey that syncs) is reported, not refused — whether a synced
passkey counts is the company's policy.

## 4. Verification: `verify_signed`

```text
verify_signed(address, holder: Account | Identity, proof, request{challenge, dApp, origin, network, purpose})
  → Wallet | Delegated{ certificate, backup_eligible }
```

1. A proof without `"curve":"delegated"` is a wallet proof and is verified as ROLA.
2. Otherwise: the certificate is valid (§2); its `network` is the request's; its
   `account`/`identity` (by `holder`) is `address`; an agent's `purposes` lists `purpose`, and
   `web` and `approve` are never an agent's; the P-256 signature verifies (§3).

`purpose` is what the signature opens: `login` (a machine: ssh, console, Windows), `web`, `approve`, `net` (routers), `kube`, `aws`, `mcp` — the door prefixes of PamAuthority

## 5. Security notes

- The two Ed25519 keys live only while the certificate is being signed (memory zeroized). Nothing
  secret is ever written: the certificate is public, the P-256 key cannot leave the hardware.
- Replay, cross-site and cross-dApp use are blocked by the ROLA message itself, as with ROLA.
- Who may ask the hardware to sign is enforced on the device (people: gesture; agents: only the
  service, measured boot, purposes, rate limit) — outside this format.
- A delegated signature is valid **only where delegated ROLA is understood**; it is not a Radix
  transaction signature.
