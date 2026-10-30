# radixdlt-connect-types

*[English](README.md) · **Español***

Esquema de mensajes de interacción de wallet de **Radix Connect** independiente del
transporte — los constructores de peticiones, constructores de respuestas y
parseadores para pruebas de cuenta, transacciones y pre-autorizaciones. Compartido por
el transporte WebRTC ([`radixdlt-connect`](https://crates.io/crates/radixdlt-connect))
y el transporte Iroh
([`radixdlt-connect-iroh`](https://crates.io/crates/radixdlt-connect-iroh)) para que
ambos hablen exactamente el mismo JSON.

```toml
[dependencies]
radixdlt-connect-types = "0.1"
```

- **Lado dApp** — construye peticiones, parsea respuestas: `account_proof_request`,
  `extract_proofs`, `extract_transaction_intent_hash`, …
- **Lado wallet** — parsea peticiones, construye respuestas:
  `parse_account_proof_request`, `account_proof_response`, …
- **Enlace** (`linking`) — lo que firma y comprueba el emparejamiento de un conector con la
  wallet, igual en todos los transportes: `linking_message`, `pairing_qr` (el JSON del QR),
  `parse_link_client` / `verify_link_signature` (la respuesta de la wallet),
  `generate_password`, `generate_identity`, `connection_id`.
- **QR** (`qr`, feature `qr`) — el QR de emparejamiento como matriz (`QrMatrix`) y como
  dibujo para terminal (`render_unicode`).

Los mensajes de error se localizan al idioma del sistema. Forma parte del
[RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## Referencia del esquema

Cada envoltura JSON de petición/respuesta (por discriminador) está documentada en
[`docs/SCHEMA.es.md`](docs/SCHEMA.es.md) ([English](docs/SCHEMA.md)).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
