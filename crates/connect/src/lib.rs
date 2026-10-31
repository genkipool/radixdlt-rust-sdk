//! radixdlt-connect — the former name of [`radixdlt-connect-webrtc`], which it re-exports whole:
//! code written against `radixdlt_connect::…` keeps building unchanged.
//!
//! New code depends on one of:
//! - `radixdlt-connect-auto` — talks to the Radix Wallet over iroh or WebRTC, whichever it can,
//!   with nothing to choose;
//! - `radixdlt-connect-webrtc` — WebRTC only (this crate's code);
//! - `radixdlt-connect-iroh` — iroh only.
//!
//! [`radixdlt-connect-webrtc`]: https://github.com/genkipool/radixdlt-rust-sdk/tree/main/crates/connect-webrtc

pub use radixdlt_connect_webrtc::*;
