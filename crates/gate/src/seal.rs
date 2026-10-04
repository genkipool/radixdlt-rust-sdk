//! Sealing a Gate's answer to the one-time key the client named in its request.
//!
//! A Gate may answer with live credentials (a cloud session, a token). They must be useless to anybody who sees the answer
//! — a proxy, a log, someone who replays the signed request — so the client puts a fresh X25519
//! public key in `GateRequest::key`, and the Gate seals the credentials to it:
//!
//! X25519(gate one-time secret, client key) → HKDF-SHA256 → ChaCha20-Poly1305, with the request's
//! challenge as associated data, so a sealed answer only opens for the request it answers.
//!
//! The client's secret never leaves its memory and is dropped after one use. Replaying a captured
//! request makes the Gate seal new credentials to a key whose secret only the original client had.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

const INFO: &[u8] = b"pamauthority-gate/v1 seal";

/// A sealed answer: the Gate's one-time public key, the nonce and the ciphertext, all hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    /// The Gate's one-time X25519 public key.
    pub epk: String,
    /// The ChaCha20-Poly1305 nonce.
    pub nonce: String,
    /// The ciphertext with its tag.
    pub ct: String,
}

/// Why a sealed answer could not be made or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// Not a 32-byte hex key.
    BadKey,
    /// Malformed fields, or the answer was not sealed to this key for this request.
    Unopenable,
}

/// A one-time key pair for the client: the secret stays in memory, the public half goes in the request.
pub struct OneTimeKey {
    secret: StaticSecret,
}

/// Shows the public half only: a secret in a debug line is a secret in a log.
impl std::fmt::Debug for OneTimeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneTimeKey")
            .field("public", &self.public_hex())
            .finish_non_exhaustive()
    }
}

impl OneTimeKey {
    /// A fresh key from the operating system's generator.
    pub fn generate() -> Self {
        Self {
            secret: StaticSecret::from(crate::random::<32>()),
        }
    }

    /// The hex public key to put in `GateRequest::key`.
    pub fn public_hex(&self) -> String {
        hex::encode(PublicKey::from(&self.secret).as_bytes())
    }

    /// Opens an answer sealed to this key for the request whose challenge is `challenge`.
    ///
    /// # Errors
    /// [`SealError::Unopenable`] for anything that does not decrypt and authenticate.
    pub fn open(&self, sealed: &Sealed, challenge: &str) -> Result<Vec<u8>, SealError> {
        let epk = key_from_hex(&sealed.epk)?;
        let shared = self.secret.diffie_hellman(&epk);
        let cipher = cipher(shared.as_bytes(), &epk, &PublicKey::from(&self.secret))?;
        let nonce: [u8; 12] = hex::decode(&sealed.nonce)
            .ok()
            .and_then(|n| n.try_into().ok())
            .ok_or(SealError::Unopenable)?;
        let ct = hex::decode(&sealed.ct).map_err(|_| SealError::Unopenable)?;
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ct,
                    aad: challenge.as_bytes(),
                },
            )
            .map_err(|_| SealError::Unopenable)
    }
}

fn key_from_hex(hex_key: &str) -> Result<PublicKey, SealError> {
    let bytes: [u8; 32] = hex::decode(hex_key)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(SealError::BadKey)?;
    Ok(PublicKey::from(bytes))
}

/// The AEAD for one exchange, bound to both public keys.
fn cipher(shared: &[u8; 32], gate: &PublicKey, client: &PublicKey) -> Result<ChaCha20Poly1305, SealError> {
    // An all-zero shared secret means a low-order key was sent: refuse rather than seal to nobody.
    if shared.iter().all(|b| *b == 0) {
        return Err(SealError::BadKey);
    }
    let mut salt = Vec::with_capacity(64);
    salt.extend_from_slice(gate.as_bytes());
    salt.extend_from_slice(client.as_bytes());
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), shared)
        .expand(INFO, &mut key)
        .map_err(|_| SealError::BadKey)?;
    Ok(ChaCha20Poly1305::new(Key::from_slice(&key)))
}

/// Seals `plaintext` to the client key `client_hex` for the request whose challenge is `challenge`.
///
/// # Errors
/// [`SealError::BadKey`] when the client key is not a usable X25519 public key.
pub fn seal(plaintext: &[u8], client_hex: &str, challenge: &str) -> Result<Sealed, SealError> {
    let client = key_from_hex(client_hex)?;
    let secret = StaticSecret::from(crate::random::<32>());
    let epk = PublicKey::from(&secret);
    let shared = secret.diffie_hellman(&client);
    let cipher = cipher(shared.as_bytes(), &epk, &client)?;
    let nonce = crate::random::<12>();
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: challenge.as_bytes(),
            },
        )
        .map_err(|_| SealError::BadKey)?;
    Ok(Sealed {
        epk: hex::encode(epk.as_bytes()),
        nonce: hex::encode(nonce),
        ct: hex::encode(ct),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_key_in_the_request_opens_the_answer() -> Result<(), SealError> {
        let client = OneTimeKey::generate();
        let sealed = seal(b"credentials", &client.public_hex(), "challenge-1")?;
        assert_eq!(client.open(&sealed, "challenge-1")?, b"credentials");
        // Somebody else's key, or the same answer replayed against another request, opens nothing.
        assert_eq!(
            OneTimeKey::generate().open(&sealed, "challenge-1"),
            Err(SealError::Unopenable)
        );
        assert_eq!(client.open(&sealed, "challenge-2"), Err(SealError::Unopenable));
        Ok(())
    }

    #[test]
    fn a_tampered_answer_or_a_bad_key_is_refused() -> Result<(), SealError> {
        let client = OneTimeKey::generate();
        let mut sealed = seal(b"x", &client.public_hex(), "c")?;
        sealed
            .ct
            .replace_range(0..2, if sealed.ct.starts_with("00") { "01" } else { "00" });
        assert_eq!(client.open(&sealed, "c"), Err(SealError::Unopenable));
        assert_eq!(seal(b"x", "zz", "c"), Err(SealError::BadKey));
        // The all-zero point is low order: sealing to it would seal to anybody.
        assert_eq!(seal(b"x", &"00".repeat(32), "c"), Err(SealError::BadKey));
        Ok(())
    }
}
