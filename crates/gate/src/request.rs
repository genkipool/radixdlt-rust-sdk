//! The request a client has its wallet sign to get a short-lived credential from a Gate.
//!
//! The protocol is named `pamauthority-gate/v1` ([`PROTOCOL`]) after the project that defined it;
//! the name is part of every challenge, so it stays as it is for compatibility.
//!
//! The client builds a [`GateRequest`], derives the challenge from it ([`GateRequest::challenge`]),
//! has the wallet sign that challenge, and sends request + proof. The Gate recomputes the challenge
//! from the request it received — so it keeps no state — and checks the window ([`GateRequest::check`]).
//!
//! What makes a captured request worthless: it names the client's own public key (`key`) and the
//! credential is bound to that key — a Kubernetes certificate needs the private key for mTLS, an
//! AWS answer is encrypted to it. Replaying a signature yields a credential only its signer can use.
//! It also names the destination (`aud`) and the level asked for (`level`), so it cannot be spent on
//! another cluster or account, nor to ask for more.
//!
//! The challenge is the SHA-256 of `"pamauthority-gate/v1\n"` followed by the request as compact
//! JSON with its fields in this exact order: `v, aud, level, key, exp, n`. Any other implementation
//! must produce the same bytes (see the test vector).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The protocol version a request carries.
pub const VERSION: u8 = 1;

/// How far ahead a request may expire. A wallet prompt takes seconds; anything longer is a request
/// prepared to be used later, which is exactly what a short window refuses.
pub const MAX_AHEAD_SECS: u64 = 300;

/// The protocol name, the first line of every challenge.
pub const PROTOCOL: &str = "pamauthority-gate/v1";

/// What the client asks for, and signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRequest {
    /// Protocol version: [`VERSION`].
    pub v: u8,
    /// The destination: `k8s:<cluster>` or `aws:<account>:<gate>`. A Gate refuses any other.
    pub aud: String,
    /// The level asked for within the Gate's target (`view`, `edit`, `admin`, `readonly`…).
    pub level: String,
    /// The client's public key the credential will be bound to (hex SHA-256 of its SPKI DER for
    /// Kubernetes, hex X25519 key for AWS).
    pub key: String,
    /// Unix seconds after which the request is refused.
    pub exp: u64,
    /// 16 random bytes, hex: two requests are never the same message.
    pub n: String,
}

/// Why a request was refused before anybody looked at a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRefusal {
    /// A version this Gate does not speak.
    Version,
    /// Signed for another destination.
    Audience,
    /// Past its `exp`.
    Expired,
    /// Expires further ahead than [`MAX_AHEAD_SECS`] — or the client's clock is far off.
    TooFarAhead,
    /// Missing or malformed fields.
    Malformed,
}

impl GateRefusal {
    /// A stable code for answers and logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Audience => "audience",
            Self::Expired => "expired",
            Self::TooFarAhead => "too_far_ahead",
            Self::Malformed => "malformed",
        }
    }
}

impl GateRequest {
    /// A fresh request for `aud`, `level` and `key`, valid for `secs` seconds from `now`.
    pub fn new(aud: &str, level: &str, key: &str, now: u64, secs: u64) -> Self {
        Self {
            v: VERSION,
            aud: aud.to_string(),
            level: level.to_string(),
            key: key.to_string(),
            exp: now + secs.min(MAX_AHEAD_SECS),
            n: hex::encode(crate::random::<16>()),
        }
    }

    /// The 32-byte challenge (hex) the wallet signs for this request.
    pub fn challenge(&self) -> String {
        // A struct of strings and integers always serialises; the fallback only exists so this
        // function cannot panic, and an empty body still yields a challenge nobody else asked for.
        let body = serde_json::to_string(self).unwrap_or_default();
        let mut hash = Sha256::new();
        hash.update(PROTOCOL.as_bytes());
        hash.update(b"\n");
        hash.update(body.as_bytes());
        hex::encode(hash.finalize())
    }

    /// Whether a Gate for `audience` may consider this request at `now`.
    ///
    /// # Errors
    /// The first rule the request breaks.
    pub fn check(&self, audience: &str, now: u64) -> Result<(), GateRefusal> {
        if self.v != VERSION {
            return Err(GateRefusal::Version);
        }
        let hex_ok = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
        if self.level.is_empty() || self.key.is_empty() || !hex_ok(&self.n, 32) {
            return Err(GateRefusal::Malformed);
        }
        if self.aud != audience {
            return Err(GateRefusal::Audience);
        }
        if self.exp <= now {
            return Err(GateRefusal::Expired);
        }
        if self.exp > now + MAX_AHEAD_SECS {
            return Err(GateRefusal::TooFarAhead);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> GateRequest {
        GateRequest {
            v: 1,
            aud: "k8s:prod".into(),
            level: "view".into(),
            key: "ab".repeat(32),
            exp: 1_800_000_100,
            n: "0f".repeat(16),
        }
    }

    /// The vector another implementation must reproduce byte for byte.
    #[test]
    fn the_challenge_is_stable() {
        let body = serde_json::to_string(&fixed()).unwrap_or_default();
        assert_eq!(
            body,
            format!(
                r#"{{"v":1,"aud":"k8s:prod","level":"view","key":"{}","exp":1800000100,"n":"{}"}}"#,
                "ab".repeat(32),
                "0f".repeat(16)
            )
        );
        // SHA-256 of "pamauthority-gate/v1\n" + that body, computed independently: every
        // implementation, and every Gate already deployed, must produce exactly this.
        assert_eq!(
            fixed().challenge(),
            "5417a9dc6fd1e5906d386332e8245b8ab97f877ce671a469e605d79876108781"
        );
    }

    /// Every field is part of what is signed: change any and the challenge changes.
    #[test]
    fn every_field_is_signed() {
        let base = fixed().challenge();
        let mut other = fixed();
        other.aud = "k8s:staging".into();
        assert_ne!(other.challenge(), base);
        let mut other = fixed();
        other.level = "admin".into();
        assert_ne!(other.challenge(), base);
        let mut other = fixed();
        other.key = "cd".repeat(32);
        assert_ne!(other.challenge(), base);
        let mut other = fixed();
        other.exp += 1;
        assert_ne!(other.challenge(), base);
    }

    #[test]
    fn the_window_is_short_and_the_destination_exact() {
        let now = 1_800_000_000;
        assert_eq!(fixed().check("k8s:prod", now), Ok(()));
        assert_eq!(fixed().check("k8s:staging", now), Err(GateRefusal::Audience));
        assert_eq!(
            fixed().check("k8s:prod", 1_800_000_100),
            Err(GateRefusal::Expired)
        );
        assert_eq!(
            fixed().check("k8s:prod", now - MAX_AHEAD_SECS),
            Err(GateRefusal::TooFarAhead)
        );
        let mut bad = fixed();
        bad.n = "zz".repeat(16);
        assert_eq!(bad.check("k8s:prod", now), Err(GateRefusal::Malformed));
        let mut old = fixed();
        old.v = 0;
        assert_eq!(old.check("k8s:prod", now), Err(GateRefusal::Version));
    }

    #[test]
    fn a_new_request_never_asks_for_more_time_than_allowed() {
        let request = GateRequest::new("k8s:prod", "view", "ab", 1000, 9999);
        assert_eq!(request.exp, 1000 + MAX_AHEAD_SECS);
        assert_ne!(request.n, GateRequest::new("k8s:prod", "view", "ab", 1000, 60).n);
    }
}
