//! radixdlt-keystore — Encrypted Ed25519 keystore for the Radix ledger.
//!
//! Stores an Ed25519 private key encrypted with a passphrase (scrypt KDF +
//! AES-256-GCM), in the same `key.json` format used by the Radix SSH signer, so
//! existing key files keep working.
//!
//! This is a pure library: it never reads the terminal, never prompts for a
//! passphrase and never exits the process. The caller supplies the passphrase and
//! handles I/O policy. User-facing error text is localized to the system language.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use ed25519_dalek::SigningKey;
use radixdlt_address::{network_by_id, virtual_account_address, AddressError};
use radixdlt_i18n::{tr, Lang};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

/// scrypt cost parameter: log2(N). N = 2^15 = 32768 (matches the Node signer).
pub const SCRYPT_LOG_N: u8 = 15;
/// scrypt block-size parameter r.
pub const SCRYPT_R: u32 = 8;
/// scrypt parallelism parameter p.
pub const SCRYPT_P: u32 = 1;

/// Keystore errors. Their `Display` text is localized to the system language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeystoreError {
    /// A hex field of the keystore is corrupt (field name included).
    CorruptField(String),
    /// Wrong passphrase, or the key file has been tampered with.
    WrongPassphraseOrCorrupt,
    /// The decrypted private key does not have 32 bytes.
    UnexpectedKeyLength,
    /// Encryption failed unexpectedly.
    EncryptionFailed,
    /// Filesystem error while reading/writing the key file.
    Io(String),
    /// The key file is not valid JSON / has the wrong shape.
    Json(String),
    /// Address derivation failed.
    Address(AddressError),
}

impl std::fmt::Display for KeystoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            KeystoreError::CorruptField(field) => tr!(
                lang,
                format!("corrupt keystore field: {field}"),
                format!("campo del keystore corrupto: {field}")
            ),
            KeystoreError::WrongPassphraseOrCorrupt => tr!(
                lang,
                "wrong passphrase or corrupt key file".to_string(),
                "passphrase incorrecta o archivo de clave corrupto".to_string()
            ),
            KeystoreError::UnexpectedKeyLength => tr!(
                lang,
                "decrypted private key has an unexpected length".to_string(),
                "la clave privada descifrada tiene un tamaño inesperado".to_string()
            ),
            KeystoreError::EncryptionFailed => tr!(
                lang,
                "encryption failed".to_string(),
                "fallo al cifrar".to_string()
            ),
            KeystoreError::Io(e) => tr!(
                lang,
                format!("I/O error: {e}"),
                format!("error de E/S: {e}")
            ),
            KeystoreError::Json(e) => tr!(
                lang,
                format!("invalid key file: {e}"),
                format!("archivo de clave inválido: {e}")
            ),
            KeystoreError::Address(e) => return std::fmt::Display::fmt(e, f),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for KeystoreError {}

impl From<AddressError> for KeystoreError {
    fn from(e: AddressError) -> Self {
        KeystoreError::Address(e)
    }
}

/// Encrypted private-key blob (scrypt + AES-256-GCM), serialized as in `key.json`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CryptoBlob {
    pub kdf: String,
    pub salt: String,
    pub n: u32,
    pub r: u32,
    pub p: u32,
    pub iv: String,
    pub tag: String,
    pub ciphertext: String,
}

impl CryptoBlob {
    /// Encrypts a 32-byte Ed25519 private key with `passphrase`.
    pub fn encrypt(private_key: &[u8; 32], passphrase: &str) -> Result<CryptoBlob, KeystoreError> {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let mut iv = [0u8; 12];
        OsRng.fill_bytes(&mut iv);
        let key = scrypt_key(passphrase, &salt);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let mut combined = cipher
            .encrypt(
                Nonce::from_slice(&iv),
                Payload {
                    msg: private_key,
                    aad: b"",
                },
            )
            .map_err(|_| KeystoreError::EncryptionFailed)?;
        // aes-gcm returns ciphertext ‖ tag; split them for the key.json format.
        let tag = combined.split_off(combined.len() - 16);
        Ok(CryptoBlob {
            kdf: "scrypt".into(),
            salt: hex::encode(salt),
            n: 1u32 << SCRYPT_LOG_N,
            r: SCRYPT_R,
            p: SCRYPT_P,
            iv: hex::encode(iv),
            tag: hex::encode(tag),
            ciphertext: hex::encode(combined),
        })
    }

    /// Decrypts the blob into the 32-byte Ed25519 private key.
    pub fn decrypt(&self, passphrase: &str) -> Result<[u8; 32], KeystoreError> {
        let salt =
            hex::decode(&self.salt).map_err(|_| KeystoreError::CorruptField("salt".into()))?;
        let iv = hex::decode(&self.iv).map_err(|_| KeystoreError::CorruptField("iv".into()))?;
        let mut ciphertext = hex::decode(&self.ciphertext)
            .map_err(|_| KeystoreError::CorruptField("ciphertext".into()))?;
        let tag = hex::decode(&self.tag).map_err(|_| KeystoreError::CorruptField("tag".into()))?;
        ciphertext.extend_from_slice(&tag); // aes-gcm expects ciphertext ‖ tag

        let key = scrypt_key(passphrase, &salt);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&iv),
                Payload {
                    msg: &ciphertext,
                    aad: b"",
                },
            )
            .map_err(|_| KeystoreError::WrongPassphraseOrCorrupt)?;
        plaintext
            .as_slice()
            .try_into()
            .map_err(|_| KeystoreError::UnexpectedKeyLength)
    }
}

/// A Radix key file: public metadata plus the encrypted private key.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KeyFile {
    pub version: u32,
    pub network: String,
    #[serde(rename = "networkId")]
    pub network_id: u8,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    pub address: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub crypto: CryptoBlob,
}

impl KeyFile {
    /// Generates a brand-new random Ed25519 key for `network_id`, encrypted with
    /// `passphrase`.
    pub fn generate(network_id: u8, passphrase: &str) -> Result<KeyFile, KeystoreError> {
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        let kf = KeyFile::from_private_key(&secret, network_id, passphrase);
        secret.fill(0);
        kf
    }

    /// Builds a key file from an existing 32-byte private key.
    pub fn from_private_key(
        private_key: &[u8; 32],
        network_id: u8,
        passphrase: &str,
    ) -> Result<KeyFile, KeystoreError> {
        let network = network_by_id(network_id).ok_or(AddressError::UnknownNetwork(network_id))?;
        let signing = SigningKey::from_bytes(private_key);
        let public_key = hex::encode(signing.verifying_key().to_bytes());
        let address = virtual_account_address(&public_key, network_id)?;
        let crypto = CryptoBlob::encrypt(private_key, passphrase)?;
        Ok(KeyFile {
            version: 1,
            network: network.logical_name.to_string(),
            network_id,
            public_key,
            address,
            created_at: unix_seconds().to_string(),
            crypto,
        })
    }

    /// Reads a key file from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<KeyFile, KeystoreError> {
        let data = std::fs::read_to_string(path).map_err(|e| KeystoreError::Io(e.to_string()))?;
        serde_json::from_str(&data).map_err(|e| KeystoreError::Json(e.to_string()))
    }

    /// Writes the key file to disk as pretty JSON, creating parent directories and
    /// restricting permissions to `0600` (owner-only) on Unix.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), KeystoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| KeystoreError::Io(e.to_string()))?;
            }
        }
        let data =
            serde_json::to_string_pretty(self).map_err(|e| KeystoreError::Json(e.to_string()))?;
        std::fs::write(path, data + "\n").map_err(|e| KeystoreError::Io(e.to_string()))?;
        set_permissions_600(path);
        Ok(())
    }

    /// Decrypts and returns the 32-byte private key.
    pub fn private_key(&self, passphrase: &str) -> Result<[u8; 32], KeystoreError> {
        self.crypto.decrypt(passphrase)
    }

    /// Decrypts the key and returns a ready-to-use `SigningKey`.
    pub fn signing_key(&self, passphrase: &str) -> Result<SigningKey, KeystoreError> {
        Ok(SigningKey::from_bytes(&self.private_key(passphrase)?))
    }
}

fn scrypt_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, 32).expect("scrypt params");
    let mut out = [0u8; 32];
    scrypt::scrypt(passphrase.as_bytes(), salt, &params, &mut out).expect("scrypt");
    out
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn set_permissions_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_permissions_600(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = [7u8; 32];
        let blob = CryptoBlob::encrypt(&key, "correct horse").unwrap();
        assert_eq!(blob.decrypt("correct horse").unwrap(), key);
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        let blob = CryptoBlob::encrypt(&[1u8; 32], "right").unwrap();
        assert_eq!(
            blob.decrypt("wrong"),
            Err(KeystoreError::WrongPassphraseOrCorrupt)
        );
    }

    #[test]
    fn generate_then_unlock_matches_address() {
        let kf = KeyFile::generate(2, "pw").unwrap();
        assert!(kf.address.starts_with("account_tdx_2_"));
        // The stored address must match the one derived from the unlocked key.
        let sk = kf.signing_key("pw").unwrap();
        let derived =
            virtual_account_address(&hex::encode(sk.verifying_key().to_bytes()), 2).unwrap();
        assert_eq!(derived, kf.address);
    }
}
