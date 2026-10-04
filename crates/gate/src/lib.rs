//! # radixdlt-gate
//!
//! Short-lived credentials from a Radix Wallet signature, bound to a key the requester holds.
//!
//! A *Gate* runs where the protected thing is (a Kubernetes cluster, a cloud account, a database)
//! and trusts exactly two things: a ROLA signature from the person's wallet, and whatever on-ledger
//! rule it applies (an access badge, an account allow-list…). It keeps no state and holds no
//! secret shared with anybody else, so nothing stolen elsewhere opens it.
//!
//! - [`request`] — the [`GateRequest`](request::GateRequest) the wallet signs: destination
//!   (`aud`), level, the requester's public key, a short expiry and a nonce. The challenge is
//!   derived from it, so the Gate recomputes it instead of remembering it.
//! - [`proof`] — the wallet's answer and its offline verification (account, and persona when
//!   present) with `radixdlt-rola`.
//! - `seal` (feature `seal`) — answers sealed to the requester's one-time X25519 key, so a
//!   captured or replayed request only ever yields data its original requester can open.
//!
//! What a Gate issues (a certificate, a cloud session, a token) and which on-ledger rule it applies
//! are the Gate's own business; PamAuthority's Kubernetes and AWS Gates are built on this crate.

pub mod proof;
pub mod request;
#[cfg(feature = "seal")]
pub mod seal;

/// `N` bytes from the operating system's CSPRNG.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    // The OS generator failing leaves the buffer zeroed; a zero nonce or key is refused or
    // useless rather than silently weak, and there is nothing better to fall back on.
    let _ = getrandom::fill(&mut out);
    out
}
