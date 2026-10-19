# ROLA delegado v1 — Especificación

*[English](DELEGATED.md) · **Español***

Estado: refleja `crates/rola/src/{delegation,signed,canonical}.rs`. Vectores de prueba:
[`tests/vectors/delegated-v1.json`](../tests/vectors/delegated-v1.json), verificados por este crate y
por el verificador de TypeScript con los mismos veredictos.

Radix firma con Ed25519 (o secp256k1). El hardware gratuito que tiene cualquier ordenador o móvil
— TPM 2.0, Secure Enclave de Apple, passkeys (WebAuthn) — firma con **P-256** y no deja salir la
clave. El ROLA delegado une las dos cosas: las claves de Radix firman **una vez** que una clave
P-256 responde por sus direcciones, y se destruyen.

## 1. Flujo

1. El equipo pide al hardware una clave P-256 no exportable y recibe su clave pública.
2. Genera **en memoria** las claves Ed25519 de cuenta e identidad, firma con las dos el
   certificado y las borra (`zeroize`). El certificado es público; no se guarda nada secreto.
3. En cada inicio de sesión, el hardware firma `SHA-256(mensaje ROLA)` — tras huella o PIN
   (personas) o solo (agentes) — y se envía `{curve:"delegated", delegation, signature}`.
4. El verificador comprueba el certificado y la firma P-256 sobre **ese** mensaje.

## 2. El certificado de delegación

Campos: `v` (1), `network`, `account`, `identity`, `delegate {alg:"p256", key}` (punto SEC1
comprimido, 33 bytes en hex), `kind` (`tpm` | `secure-enclave` | `webauthn`), `rpId` (solo
webauthn), `subject` (`person` | `agent`), `purposes` (solo agentes), `label` (usuario del
sistema), `device`, `createdAt`, `attestation` (opcional) y `signatures {account, identity}`.

**Bytes firmados**

```
digest = blake2b_256( "pamauthority-delegation/v1\n" ‖ canónico(certificado − "signatures") )
```

Canónico: claves ordenadas por unidades UTF-16 (el `sort()` de JavaScript) a cualquier
profundidad, sin espacios, cadenas escapadas como `JSON.stringify`, solo enteros.

**Un certificado vale si y solo si**: tiene exactamente esos campos (se rechazan los
desconocidos), `v = 1` y `alg = "p256"`; `rpId` ⇔ `webauthn` y `purposes` ⇔ `agent`; la clave es
un punto P-256 válido; y las dos firmas Ed25519 verifican el `digest` con claves que derivan a
`account` e `identity` en esa red.

El prefijo de dominio impide confundir un certificado con un mensaje ROLA (empiezan por `R`). No
caduca: la raíz ya no existe para renovarlo; muere con sus direcciones (fuera de la lista,
insignia quemada).

## 3. La prueba delegada

Sustituye al `{publicKey, signature}` de la billetera en la `proof` de la cuenta y en la de la
persona (sirve el mismo objeto para las dos).

| `kind` | Prueba | Bytes firmados |
|---|---|---|
| `tpm`, `secure-enclave` | `{"curve":"delegated","delegation":{…},"signature":"<r‖s, 64 bytes>"}` | ECDSA-P256 sobre `SHA-256(mensaje ROLA)`; **S bajo** |
| `webauthn` | `{"curve":"delegated","delegation":{…},"webauthn":{authenticatorData, clientDataJSON, signature}}` | WebAuthn: `authenticatorData ‖ SHA-256(clientDataJSON)` |

Para WebAuthn además: `rpIdHash = SHA-256(rpId)`; banderas **UP y UV** (huella o PIN);
`type = "webauthn.get"`, `challenge = base64url(mensaje ROLA)`, `origin` el de la petición,
`crossOrigin ≠ true`. La bandera `BE` (passkey sincronizable) se informa; la política de la
empresa decide si vale.

## 4. Verificación: `verify_signed`

Sin `"curve":"delegated"` es una prueba de billetera y se verifica como ROLA. Si no: certificado
válido; misma red; su `account`/`identity` es la dirección; un agente solo firma para sus
`purposes` y nunca `web` ni `approve`; la firma P-256 verifica. Devuelve `Wallet` o
`Delegated{certificado, backup_eligible}`.

## 5. Seguridad

- Las Ed25519 solo viven mientras se firma el certificado. Nada secreto se escribe nunca.
- Repetición, otro sitio u otra dApp: los bloquea el propio mensaje ROLA.
- Quién puede pedir firmas al hardware lo impone el equipo (gesto, servicio, arranque medido,
  propósitos, ritmo), fuera de este formato.
- Una firma delegada solo vale donde se entiende el ROLA delegado; no firma transacciones Radix.
