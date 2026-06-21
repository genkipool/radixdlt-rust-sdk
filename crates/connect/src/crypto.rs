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
use rand_core::{OsRng, RngCore};

use crate::error::ConnectError;

type Blake2b256 = Blake2b<U32>;

pub fn blake2b_256(data: &[u8]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    h.update(data);
    h.finalize().into()
}

/// connectionId (hex) derived from the link password.
pub fn connection_id_hex(password: &[u8]) -> String {
    hex::encode(blake2b_256(password))
}

/// Encrypts `plaintext` and returns `IV ‖ ciphertext‖tag` as hex (signaling format).
pub fn encrypt_payload(plaintext: &[u8], key: &[u8]) -> Result<String, ConnectError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut iv = [0u8; 12];
    OsRng.fill_bytes(&mut iv);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&iv),
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
    let raw =
        hex::decode(hex_data).map_err(|e| ConnectError::Crypto(format!("invalid hex: {e}")))?;
    if raw.len() < 12 + 16 {
        return Err(ConnectError::Crypto("payload too short".into()));
    }
    let (iv, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(iv), Payload { msg: ct, aad: b"" })
        .map_err(|_| ConnectError::Crypto("decryption failed (key/iv/tag)".into()))
}

/// Linking signature message: blake2b_256("L" ‖ password).
pub fn linking_message(password: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1 + password.len());
    buf.push(b'L');
    buf.extend_from_slice(password);
    blake2b_256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_aes_gcm() {
        let key = [7u8; 32];
        let pt = b"hello radix connect";
        let enc = encrypt_payload(pt, &key).unwrap();
        let dec = decrypt_payload(&enc, &key).unwrap();
        assert_eq!(dec, pt);
    }
}
