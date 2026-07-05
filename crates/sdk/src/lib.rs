//! # radixdlt-sdk
//!
//! Umbrella crate for the RadixDLT Rust SDK. It re-exports the individual crates
//! behind feature flags so you can depend on a single crate and opt into exactly
//! what you need.
//!
//! ```toml
//! # Verify ROLA proofs (default):
//! radixdlt-sdk = "0.1"
//!
//! # Build and send transactions + manage keys:
//! radixdlt-sdk = { version = "0.1", features = ["full"] }
//! ```
//!
//! ## Features
//!
//! * `address` — virtual-account address derivation (module `address`).
//! * `rola` *(default)* — ROLA off-ledger authentication (module `rola`); implies `address`.
//! * `keystore` — encrypted Ed25519 keystore (module `keystore`); implies `address`.
//! * `gateway` — Gateway client + local transaction signing (module `gateway`).
//! * `connect-types` — transport-agnostic wallet-interaction schema (module `connect_types`).
//! * `full` — `rola` + `keystore` + `gateway` + `connect-types`.
//!
//! The [`i18n`] module (system-language detection) is always available.
//!
//! ## Wallet / WebRTC
//!
//! Radix Connect (pairing and WebRTC wallet interactions) lives in the separate
//! `radixdlt-connect` crate. It is **not** re-exported here because its `webrtc`
//! dependency tree cannot be resolved together with the `radix-engine` tree used by
//! the `gateway` feature. Add `radixdlt-connect` directly; it pairs naturally with
//! the `rola` feature for "log in with Radix" flows.
//!
//! All user-facing error messages across the SDK are localized to the system
//! language (English/Spanish).

/// System-language detection and bilingual text helpers (always available).
pub use radixdlt_i18n as i18n;

/// Virtual-account address derivation. Enabled by the `address` feature.
#[cfg(feature = "address")]
pub use radixdlt_address as address;

/// ROLA off-ledger authentication. Enabled by the `rola` feature.
#[cfg(feature = "rola")]
pub use radixdlt_rola as rola;

/// Encrypted Ed25519 keystore. Enabled by the `keystore` feature.
#[cfg(feature = "keystore")]
pub use radixdlt_keystore as keystore;

/// Gateway client + local transaction signing. Enabled by the `gateway` feature.
#[cfg(feature = "gateway")]
pub use radixdlt_gateway_tx as gateway;

/// Transport-agnostic Radix Connect message schema. Enabled by the `connect-types`
/// feature.
#[cfg(feature = "connect-types")]
pub use radixdlt_connect_types as connect_types;

/// Common imports. `use radixdlt_sdk::prelude::*;`
pub mod prelude {
    pub use crate::i18n::Lang;

    #[cfg(feature = "address")]
    pub use crate::address::{virtual_account_address, AddressError};
    #[cfg(feature = "gateway")]
    pub use crate::gateway::{Gateway, GatewayError, NotarizedTx, TxStatus};
    #[cfg(feature = "keystore")]
    pub use crate::keystore::{KeyFile, KeystoreError};
    #[cfg(feature = "rola")]
    pub use crate::rola::{verify_account_proof, AccountProof, RolaError};
}
