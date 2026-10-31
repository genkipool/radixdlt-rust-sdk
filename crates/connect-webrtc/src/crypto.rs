//! Radix Connect cryptography (parity with `radix-connect-webrtc`).
//!
//!   connectionId  = blake2b_256(password)
//!   encryptionKey = password (32 bytes, raw AES-256-GCM key)
//!   encrypted signaling payload = IV(12) ‖ AES-256-GCM(key, IV, plaintext),
//!     transmitted as hex.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

use crate::error::ConnectError;

type Blake2b256 = Blake2b<U32>;

pub fn blake2b_256(data: &[u8]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    h.update(data);
    h.finalize().into()
}

/// connectionId (hex) derived from the link password
/// ([`radixdlt_connect_types::linking::connection_id`]).
pub fn connection_id_hex(password: &[u8]) -> String {
    hex::encode(radixdlt_connect_types::linking::connection_id(password))
}

/// The AES key from a link password, refusing anything that is not 32 bytes.
///
/// `Key::from_slice` would PANIC on a wrong length -- in library code, on input that arrives
/// from a file the user can edit. An error is the only acceptable answer.
fn aes_key(key: &[u8]) -> Result<Key<Aes256Gcm>, ConnectError> {
    Key::<Aes256Gcm>::try_from(key)
        .map_err(|_| ConnectError::Crypto("the link password is not a 32-byte key".into()))
}

/// Encrypts `plaintext` and returns `IV ‖ ciphertext‖tag` as hex (signaling format).
pub fn encrypt_payload(plaintext: &[u8], key: &[u8]) -> Result<String, ConnectError> {
    let cipher = Aes256Gcm::new(&aes_key(key)?);
    let mut iv = [0u8; 12];
    getrandom::fill(&mut iv).map_err(|e| ConnectError::Crypto(format!("system randomness: {e}")))?;
    let ct = cipher
        .encrypt(
            &Nonce::from(iv),
            Payload {
                msg: plaintext,
                aad: b"",
            },
        )
        .map_err(|_| ConnectError::Crypto("encryption failed".into()))?;
    let mut combined = Vec::with_capacity(12 + ct.len());
    combined.extend_from_slice(&iv);
    combined.extend_from_slice(&ct);
    Ok(hex::encode(combined))
}

/// Decrypts an `IV ‖ ciphertext‖tag` hex string.
pub fn decrypt_payload(hex_data: &str, key: &[u8]) -> Result<Vec<u8>, ConnectError> {
    let raw = hex::decode(hex_data).map_err(|e| ConnectError::Crypto(format!("invalid hex: {e}")))?;
    if raw.len() < 12 + 16 {
        return Err(ConnectError::Crypto("payload too short".into()));
    }
    let (iv, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new(&aes_key(key)?);
    // `split_at(12)` guarantees the length, so this conversion cannot fail.
    let nonce =
        Nonce::try_from(iv).map_err(|_| ConnectError::Crypto("payload nonce is not 12 bytes".into()))?;
    cipher
        .decrypt(&nonce, Payload { msg: ct, aad: b"" })
        .map_err(|_| ConnectError::Crypto("decryption failed (key/iv/tag)".into()))
}

/// Linking signature message: blake2b_256("L" ‖ password). One implementation for every
/// transport: [`radixdlt_connect_types::linking::linking_message`].
///
/// A function rather than a `pub use`, so tools that check this crate's public API alone (it
/// predates the shared module) still see it.
pub fn linking_message(password: &[u8]) -> [u8; 32] {
    radixdlt_connect_types::linking::linking_message(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The re-exported linking message is the one this crate always computed.
    #[test]
    fn linking_message_is_blake2b_of_l_and_the_password() {
        let password = [0x42u8; 32];
        let mut buf = vec![b'L'];
        buf.extend_from_slice(&password);
        assert_eq!(linking_message(&password), blake2b_256(&buf));
        assert_eq!(connection_id_hex(&password), hex::encode(blake2b_256(&password)));
    }

    #[test]
    fn round_trip_aes_gcm() {
        let key = [7u8; 32];
        let pt = b"hello radix connect";
        let enc = encrypt_payload(pt, &key).unwrap();
        let dec = decrypt_payload(&enc, &key).unwrap();
        assert_eq!(dec, pt);
    }
}
