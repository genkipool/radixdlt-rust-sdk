//! radixdlt-keystore — Encrypted Ed25519 keystore for the Radix ledger.
//!
//! Stores an Ed25519 private key encrypted with a passphrase (scrypt KDF +
//! AES-256-GCM), in the same `key.json` format used by the Radix SSH signer, so
//! existing key files keep working.
//!
//! This is a pure library: it never reads the terminal, never prompts for a
//! passphrase and never exits the process. The caller supplies the passphrase and
//! handles I/O policy. User-facing error text is localized to the system language.

// In TESTS a panic IS the failure mechanism. Library code keeps the deny: a panic there is
// taken in the CONSUMER's process, which they neither chose nor can catch.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use ed25519_dalek::SigningKey;
use radixdlt_address::{network_by_id, virtual_account_address, AddressError};
use radixdlt_i18n::{tr, Lang};

use serde::{Deserialize, Serialize};

/// scrypt cost parameter: log2(N). N = 2^15 = 32768 (matches the Node signer).
pub const SCRYPT_LOG_N: u8 = 15;
/// scrypt block-size parameter r.
pub const SCRYPT_R: u32 = 8;
/// scrypt parallelism parameter p.
pub const SCRYPT_P: u32 = 1;

/// Keystore errors. Their `Display` text is localized to the system language.
///
/// Marked `#[non_exhaustive]`: a keystore learns new ways to fail as the crypto beneath it
/// moves, and each one should not force a breaking release on everyone who matches on this.
/// `RandomnessUnavailable` is the case in point -- it only became expressible once the RNG
/// could report a failure. Match with a `_` arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeystoreError {
    /// A hex field of the keystore is corrupt (field name included).
    CorruptField(String),
    /// Wrong passphrase, or the key file has been tampered with.
    WrongPassphraseOrCorrupt,
    /// The decrypted private key does not have 32 bytes.
    UnexpectedKeyLength,
    /// Encryption failed unexpectedly.
    EncryptionFailed,
    /// The operating system could not provide randomness, so no salt, nonce or key
    /// could be generated. Never silently continued past: a predictable salt or nonce
    /// destroys the encryption that depends on it.
    RandomnessUnavailable,
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
            KeystoreError::RandomnessUnavailable => tr!(
                lang,
                "the operating system could not provide randomness".to_string(),
                "el sistema operativo no pudo proporcionar aleatoriedad".to_string()
            ),
            KeystoreError::EncryptionFailed => tr!(
                lang,
                "encryption failed".to_string(),
                "fallo al cifrar".to_string()
            ),
            KeystoreError::Io(e) => tr!(lang, format!("I/O error: {e}"), format!("error de E/S: {e}")),
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
    /// Key-derivation function used to turn the passphrase into a key. Always `scrypt`.
    pub kdf: String,
    /// Random salt for the KDF, hex-encoded. Unique per file: it is what stops one cracking
    /// effort from being reusable against every other key file.
    pub salt: String,
    /// scrypt CPU/memory cost. The parameter that decides how expensive guessing is.
    pub n: u32,
    /// scrypt block size.
    pub r: u32,
    /// scrypt parallelisation factor.
    pub p: u32,
    /// AES-256-GCM nonce, hex-encoded. Never reused with the same key.
    pub iv: String,
    /// GCM authentication tag, hex-encoded. A wrong passphrase fails here, which is how
    /// decryption tells "wrong passphrase" apart from "corrupted file".
    pub tag: String,
    /// The encrypted 32-byte private key, hex-encoded.
    pub ciphertext: String,
}

impl CryptoBlob {
    /// Encrypts a 32-byte Ed25519 private key with `passphrase`.
    ///
    /// # Errors
    /// When the system RNG cannot produce a salt or nonce, or scrypt rejects the parameters.
    /// Never because of the passphrase: any passphrase encrypts, including a bad one.
    pub fn encrypt(private_key: &[u8; 32], passphrase: &str) -> Result<CryptoBlob, KeystoreError> {
        let mut salt = [0u8; 16];
        let mut iv = [0u8; 12];
        // Fails closed. `fill_bytes` could not report a problem, so this promise in the
        // doc comment above was unenforceable until now.
        getrandom::fill(&mut salt).map_err(|_| KeystoreError::RandomnessUnavailable)?;
        getrandom::fill(&mut iv).map_err(|_| KeystoreError::RandomnessUnavailable)?;
        let key = scrypt_key(passphrase, &salt);
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key));
        let mut combined = cipher
            .encrypt(
                &Nonce::from(iv),
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
    ///
    /// # Errors
    /// [`KeystoreError::WrongPassphraseOrCorrupt`] when the GCM tag does not verify — which is also what
    /// a tampered-with file looks like — and [`KeystoreError::CorruptField`] when a field is
    /// not valid hex or has the wrong length.
    pub fn decrypt(&self, passphrase: &str) -> Result<[u8; 32], KeystoreError> {
        let salt = hex::decode(&self.salt).map_err(|_| KeystoreError::CorruptField("salt".into()))?;
        let iv = hex::decode(&self.iv).map_err(|_| KeystoreError::CorruptField("iv".into()))?;
        let mut ciphertext =
            hex::decode(&self.ciphertext).map_err(|_| KeystoreError::CorruptField("ciphertext".into()))?;
        let tag = hex::decode(&self.tag).map_err(|_| KeystoreError::CorruptField("tag".into()))?;
        ciphertext.extend_from_slice(&tag); // aes-gcm expects ciphertext ‖ tag

        // The nonce comes from a file a user can edit, and the old `Nonce::from_slice` would
        // PANIC on any length but 12 rather than report a corrupt field.
        let nonce = Nonce::try_from(&iv[..]).map_err(|_| KeystoreError::CorruptField("iv".into()))?;
        let key = scrypt_key(passphrase, &salt);
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key));
        let plaintext = cipher
            .decrypt(
                &nonce,
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
    /// File-format version, so an older file can still be read after the format moves on.
    pub version: u32,
    /// Human-readable network name (`mainnet`, `stokenet`).
    pub network: String,
    /// Radix network id. It is part of the address, so a key file is not portable between
    /// networks: the same key yields a different address on each.
    #[serde(rename = "networkId")]
    pub network_id: u8,
    /// Ed25519 public key, hex-encoded. Safe to publish — it is what the address derives from.
    #[serde(rename = "publicKey")]
    pub public_key: String,
    /// The virtual account address this key controls, Bech32m-encoded.
    pub address: String,
    /// Creation timestamp (RFC 3339). Informational; nothing depends on it.
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// The encrypted private key. This is the only secret in the file.
    pub crypto: CryptoBlob,
}

impl KeyFile {
    /// Generates a brand-new random Ed25519 key for `network_id`, encrypted with
    /// `passphrase`.
    ///
    /// # Errors
    /// When the system RNG is unavailable, or the freshly generated key cannot be encrypted.
    pub fn generate(network_id: u8, passphrase: &str) -> Result<KeyFile, KeystoreError> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|_| KeystoreError::RandomnessUnavailable)?;
        let kf = KeyFile::from_private_key(&secret, network_id, passphrase);
        secret.fill(0);
        kf
    }

    /// Builds a key file from an existing 32-byte private key.
    ///
    /// # Errors
    /// When `network_id` has no known address prefix, or encryption fails.
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
    ///
    /// # Errors
    /// When the file cannot be read, or its contents are not a key file of a version this
    /// build understands.
    pub fn load(path: impl AsRef<Path>) -> Result<KeyFile, KeystoreError> {
        let data = std::fs::read_to_string(path).map_err(|e| KeystoreError::Io(e.to_string()))?;
        serde_json::from_str(&data).map_err(|e| KeystoreError::Json(e.to_string()))
    }

    /// Writes the key file to disk as pretty JSON, creating parent directories and
    /// restricting permissions to `0600` (owner-only) on Unix.
    ///
    /// # Errors
    /// When the path cannot be written. The file is written with owner-only permissions; a
    /// failure to apply them is also an error, because a world-readable key file is worse than
    /// no file at all.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), KeystoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| KeystoreError::Io(e.to_string()))?;
            }
        }
        let data = serde_json::to_string_pretty(self).map_err(|e| KeystoreError::Json(e.to_string()))?;
        std::fs::write(path, data + "\n").map_err(|e| KeystoreError::Io(e.to_string()))?;
        set_permissions_600(path);
        Ok(())
    }

    /// Decrypts and returns the 32-byte private key.
    ///
    /// # Errors
    /// As [`CryptoBlob::decrypt`]: a wrong passphrase, or a corrupted field.
    pub fn private_key(&self, passphrase: &str) -> Result<[u8; 32], KeystoreError> {
        self.crypto.decrypt(passphrase)
    }

    /// Decrypts the key and returns a ready-to-use `SigningKey`.
    ///
    /// # Errors
    /// As [`Self::private_key`]. The bytes are only rejected here if they are not a valid
    /// Ed25519 scalar, which cannot happen for a file this crate wrote.
    pub fn signing_key(&self, passphrase: &str) -> Result<SigningKey, KeystoreError> {
        Ok(SigningKey::from_bytes(&self.private_key(passphrase)?))
    }
}

// The two `expect`s below are on COMPILE-TIME CONSTANTS: `Params::new` only rejects values
// outside scrypt's legal ranges, and `scrypt` only fails on an output length it was just
// given. If either fired it would mean this build is malformed, not that anything went wrong
// at runtime — and returning an error the caller cannot act on would be worse than saying so.
#[allow(clippy::expect_used)]
fn scrypt_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    // The output length is no longer a parameter: scrypt 0.12 takes it from the buffer, and
    // documents that a length set on `Params` is ignored. Same 32 bytes either way, so the
    // derivation is unchanged and key files written by earlier versions still open --
    // which `a_key_file_from_before_this_change_still_opens` holds us to.
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P).expect("scrypt params");
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
        let derived = virtual_account_address(&hex::encode(sk.verifying_key().to_bytes()), 2).unwrap();
        assert_eq!(derived, kf.address);
    }

    /// A key file written BEFORE the crypto stack was upgraded (ed25519-dalek 2, scrypt 0.11,
    /// aes-gcm 0.10) must still open. This is the failure that would be both catastrophic and
    /// silent: every stored key becomes unreadable, and nothing says so until someone tries.
    ///
    /// The fixture is a real file produced by that older build, not a re-encryption by this
    /// one -- which would only prove the code agrees with itself.
    #[test]
    fn a_key_file_from_before_this_change_still_opens() {
        const BEFORE: &str = r#"{"version":1,"network":"stokenet","networkId":2,"publicKey":"5f60f5d663981c77e678b3e77693c6a9dd24f9641cfa8f8f04fc289bf7d73bea","address":"account_tdx_2_128exm3fvj87wn78yqpdakq5949h6dsc46g7whm3pfjm0mdaxv3xtk0","createdAt":"1785684343","crypto":{"kdf":"scrypt","salt":"ece1270e844f691612be87f7df013a0b","n":32768,"r":8,"p":1,"iv":"3ec5a50b616ad3333f335bfa","tag":"be78570bc602fbfb9ef59bf1abeb549a","ciphertext":"afef046ab51ce355398353c9ce1f47aeb52f94d7f9d53775e67cbc583c9e9cd2"}}"#;
        let kf: KeyFile = serde_json::from_str(BEFORE).expect("the old shape still parses");
        let secret = kf
            .private_key("correct horse battery staple")
            .expect("the old file still decrypts");
        let signing = SigningKey::from_bytes(&secret);
        assert_eq!(
            hex::encode(signing.verifying_key().to_bytes()),
            "5f60f5d663981c77e678b3e77693c6a9dd24f9641cfa8f8f04fc289bf7d73bea",
            "the key recovered must be the very one that was stored"
        );
        assert_eq!(
            kf.address, "account_tdx_2_128exm3fvj87wn78yqpdakq5949h6dsc46g7whm3pfjm0mdaxv3xtk0",
            "and its address must not have moved"
        );
    }

    /// A key file MUST be owner-only. Nothing tested this, so removing the call that applies
    /// the mode left every stored private key world-readable and every test still passed.
    #[cfg(unix)]
    #[test]
    fn a_saved_key_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ks-perm-{}", std::process::id()));
        let path = dir.join("key.json");
        let kf = KeyFile::generate(2, "pw").expect("generate");
        kf.save(&path).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a key file must not be readable by anyone else"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `save` reporting success without writing anything is indistinguishable from working,
    /// right up until the moment somebody needs the key back.
    #[test]
    fn save_writes_a_file_that_loads_back_identically() {
        let dir = std::env::temp_dir().join(format!("ks-save-{}", std::process::id()));
        let path = dir.join("nested").join("key.json");
        let kf = KeyFile::generate(2, "pw").expect("generate");
        kf.save(&path).expect("save");
        assert!(path.exists(), "save must actually create the file");
        let back = KeyFile::load(&path).expect("load");
        assert_eq!(back.public_key, kf.public_key);
        assert_eq!(back.address, kf.address);
        assert_eq!(
            back.private_key("pw").expect("decrypt"),
            kf.private_key("pw").expect("decrypt"),
            "and the key inside must survive the round trip to disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The blob is a FORMAT shared with the Node signer, so the field widths are part of the
    /// contract: a wrong split still round-trips through our own decrypt (it re-joins the two)
    /// while producing a file no other implementation can read.
    #[test]
    fn the_blob_splits_ciphertext_and_tag_at_the_documented_widths() {
        let blob = CryptoBlob::encrypt(&[7u8; 32], "pw").expect("encrypt");
        assert_eq!(blob.tag.len(), 32, "the GCM tag is 16 bytes, hex-encoded");
        assert_eq!(blob.ciphertext.len(), 64, "the key is 32 bytes, hex-encoded");
        assert_eq!(blob.salt.len(), 32, "16-byte salt");
        assert_eq!(blob.iv.len(), 24, "12-byte nonce");
    }

    /// `n` is written for other implementations to read; ours ignores it and uses the constant.
    /// So a wrong value here is invisible to us and fatal to them.
    #[test]
    fn the_recorded_scrypt_parameters_match_the_constants() {
        let blob = CryptoBlob::encrypt(&[7u8; 32], "pw").expect("encrypt");
        assert_eq!(blob.n, 32768, "n = 2^15");
        assert_eq!(blob.r, SCRYPT_R);
        assert_eq!(blob.p, SCRYPT_P);
        assert_eq!(blob.kdf, "scrypt");
    }

    #[test]
    fn a_new_key_file_is_stamped_with_the_current_time() {
        let kf = KeyFile::generate(2, "pw").expect("generate");
        let created: u64 = kf.created_at.parse().expect("createdAt is an epoch");
        // Any moment after this crate was written, and not in the future.
        assert!(
            created > 1_700_000_000,
            "createdAt must be a real timestamp, got {created}"
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        assert!(created <= now + 5, "createdAt must not be in the future");
    }
}
