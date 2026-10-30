//! Linking a connector with the Radix Wallet: the cryptography every transport shares.
//!
//! Linking does not depend on how the two ends reach each other. The connector generates a
//! fresh 32-byte **link password**, shows it in a QR code signed with its long-term Ed25519
//! **identity** key, and the wallet that scans it answers — over whichever transport it reached
//! the connector on — with a `linkClient` message signed with ITS key:
//!
//! ```text
//! linking message = blake2b_256("L" ‖ password)
//! QR              = {"password": hex(P), "publicKey": hex(connector pk),
//!                    "signature": hex(sign(connector sk, linking message)), "purpose": "general"}
//! linkClient      = {"discriminator": "linkClient", "publicKey": hex(wallet pk),
//!                    "signature": hex(sign(wallet sk, linking message))}
//! ```
//!
//! Both signatures cover the same message, so each side proves it holds the key it claims AND
//! that it saw this very password. The password is the secret of the link from then on: whoever
//! holds it can talk to the other end (WebRTC derives its signaling id and encryption key from
//! it, iroh derives both endpoint keys from it), so it must be stored like a credential and
//! never logged.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use radixdlt_i18n::{tr, Lang};
use serde_json::{json, Value};

type Blake2b256 = Blake2b<U32>;

/// Length in bytes of a link password (and of every Ed25519 key involved).
pub const PASSWORD_LENGTH: usize = 32;

/// The `purpose` the QR declares. The wallet links a `general` connector as a regular one.
pub const QR_PURPOSE: &str = "general";

/// The `discriminator` of the wallet's answer to a scanned QR.
pub const LINK_CLIENT_DISCRIMINATOR: &str = "linkClient";

/// Errors building or checking the linking messages. `Display` is localized.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkingError {
    /// The link password is not 32 bytes.
    InvalidPassword,
    /// The connector's identity key is not a 32-byte, hex-encoded Ed25519 key.
    InvalidIdentityKey(String),
    /// The identity public key given does not belong to the identity private key: the QR would
    /// carry a signature the wallet cannot verify.
    IdentityMismatch,
    /// The wallet's public key is not a 32-byte, hex-encoded Ed25519 key.
    InvalidWalletKey(String),
    /// The signature is not 64 bytes of hex.
    MalformedSignature(String),
    /// The signature does not verify against the linking message of this password.
    InvalidSignature,
    /// The `linkClient` carries no signature.
    MissingSignature,
    /// A `linkClient` message without the fields it must carry.
    Malformed(String),
    /// The system's random number generator failed.
    Randomness(String),
}

impl std::fmt::Display for LinkingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            LinkingError::InvalidPassword => tr!(
                lang,
                format!("the link password must be {PASSWORD_LENGTH} bytes"),
                format!("la contraseña del enlace debe tener {PASSWORD_LENGTH} bytes")
            ),
            LinkingError::InvalidIdentityKey(e) => tr!(
                lang,
                format!("invalid connector identity key: {e}"),
                format!("clave de identidad del conector no válida: {e}")
            ),
            LinkingError::IdentityMismatch => tr!(
                lang,
                "the identity public key does not belong to the identity private key".to_string(),
                "la clave pública de identidad no corresponde a la clave privada".to_string()
            ),
            LinkingError::InvalidWalletKey(e) => tr!(
                lang,
                format!("invalid wallet public key: {e}"),
                format!("clave pública de la wallet no válida: {e}")
            ),
            LinkingError::MalformedSignature(e) => tr!(
                lang,
                format!("malformed linking signature: {e}"),
                format!("firma de enlace mal formada: {e}")
            ),
            LinkingError::InvalidSignature => tr!(
                lang,
                "INVALID wallet linking signature".to_string(),
                "firma de enlace de la wallet NO VÁLIDA".to_string()
            ),
            LinkingError::MissingSignature => tr!(
                lang,
                "the wallet's linkClient carries no signature".to_string(),
                "el linkClient de la wallet no lleva firma".to_string()
            ),
            LinkingError::Malformed(e) => tr!(
                lang,
                format!("malformed linkClient: {e}"),
                format!("linkClient mal formado: {e}")
            ),
            LinkingError::Randomness(e) => tr!(
                lang,
                format!("system randomness failed: {e}"),
                format!("falló la aleatoriedad del sistema: {e}")
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for LinkingError {}

fn blake2b_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// The message both linking signatures cover: `blake2b_256("L" ‖ password)`.
#[must_use]
pub fn linking_message(password: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b256::new();
    hasher.update(b"L");
    hasher.update(password);
    hasher.finalize().into()
}

/// The link's public identifier: `blake2b_256(password)`.
///
/// It is what the WebRTC transport sends to the signaling server as `connectionId`, and what the
/// wallet names a link by. It reveals nothing about the password (which has 256 bits of
/// entropy), so it is the value to key, log or display a link by — never the password itself.
#[must_use]
pub fn connection_id(password: &[u8]) -> [u8; 32] {
    blake2b_256(password)
}

/// A fresh link password from the system's random number generator.
///
/// # Errors
/// [`LinkingError::Randomness`] when the system's random number generator fails.
pub fn generate_password() -> Result<[u8; PASSWORD_LENGTH], LinkingError> {
    let mut password = [0u8; PASSWORD_LENGTH];
    getrandom::fill(&mut password).map_err(|e| LinkingError::Randomness(e.to_string()))?;
    Ok(password)
}

/// A fresh connector identity: `(private_key_hex, public_key_hex)` of a new Ed25519 key.
///
/// The identity is long-term: the wallet shows and remembers the connector by it, so generate
/// it once, store the private key like any other secret and reuse it for every pairing.
///
/// # Errors
/// [`LinkingError::Randomness`] when the system's random number generator fails.
pub fn generate_identity() -> Result<(String, String), LinkingError> {
    let mut seed = [0u8; PASSWORD_LENGTH];
    getrandom::fill(&mut seed).map_err(|e| LinkingError::Randomness(e.to_string()))?;
    let key = SigningKey::from_bytes(&seed);
    seed.fill(0);
    Ok((
        hex::encode(key.to_bytes()),
        hex::encode(key.verifying_key().to_bytes()),
    ))
}

fn hex_32(hex_str: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| e.to_string())?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("{} bytes instead of 32", bytes.len()))
}

/// The exact JSON to show in the linking QR code, signed with the connector's identity.
///
/// Fails BEFORE anything is shown when the identity is unusable — including a public key that
/// does not belong to the private key, which would otherwise produce a QR every wallet refuses.
///
/// # Errors
/// [`LinkingError::InvalidPassword`] for a password that is not 32 bytes,
/// [`LinkingError::InvalidIdentityKey`] for a key that is not 32 bytes of hex, and
/// [`LinkingError::IdentityMismatch`] when the two keys are not a pair.
pub fn pairing_qr(
    password: &[u8],
    identity_private_key_hex: &str,
    identity_public_key_hex: &str,
) -> Result<String, LinkingError> {
    if password.len() != PASSWORD_LENGTH {
        return Err(LinkingError::InvalidPassword);
    }
    let mut seed = hex_32(identity_private_key_hex).map_err(LinkingError::InvalidIdentityKey)?;
    let signing = SigningKey::from_bytes(&seed);
    seed.fill(0);
    let claimed = hex_32(identity_public_key_hex).map_err(LinkingError::InvalidIdentityKey)?;
    if signing.verifying_key().to_bytes() != claimed {
        return Err(LinkingError::IdentityMismatch);
    }
    let signature = signing.sign(&linking_message(password));
    Ok(json!({
        "password": hex::encode(password),
        "publicKey": hex::encode(claimed),
        "signature": hex::encode(signature.to_bytes()),
        "purpose": QR_PURPOSE,
    })
    .to_string())
}

/// The wallet's answer to a scanned QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkClient {
    /// The wallet's Ed25519 public key (hex), which identifies the wallet on this link.
    pub public_key: String,
    /// Its signature (hex) over [`linking_message`]. Every current wallet sends one.
    pub signature: Option<String>,
}

impl LinkClient {
    /// Verifies the wallet's signature over `password`'s linking message, requiring one.
    ///
    /// # Errors
    /// [`LinkingError::MissingSignature`] when the message carries none, plus the errors of
    /// [`verify_link_signature`].
    pub fn verify(&self, password: &[u8]) -> Result<(), LinkingError> {
        let signature = self.signature.as_deref().ok_or(LinkingError::MissingSignature)?;
        verify_link_signature(password, &self.public_key, signature)
    }
}

/// Reads a `linkClient` message. `Ok(None)` when the message is something else.
///
/// # Errors
/// [`LinkingError::Malformed`] for a `linkClient` without a `publicKey`.
pub fn parse_link_client(message: &Value) -> Result<Option<LinkClient>, LinkingError> {
    if message.get("discriminator").and_then(Value::as_str) != Some(LINK_CLIENT_DISCRIMINATOR) {
        return Ok(None);
    }
    let public_key = message
        .get("publicKey")
        .and_then(Value::as_str)
        .ok_or_else(|| LinkingError::Malformed("no publicKey".into()))?;
    Ok(Some(LinkClient {
        public_key: public_key.to_string(),
        signature: message
            .get("signature")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

/// Verifies a wallet's linking signature: `signature_hex` by `wallet_public_key_hex` over the
/// linking message of `password`, with Ed25519 `verify_strict` (no malleable or small-order
/// encodings).
///
/// # Errors
/// [`LinkingError::InvalidPassword`], [`LinkingError::InvalidWalletKey`] and
/// [`LinkingError::MalformedSignature`] for inputs of the wrong shape, and
/// [`LinkingError::InvalidSignature`] when the signature does not verify.
pub fn verify_link_signature(
    password: &[u8],
    wallet_public_key_hex: &str,
    signature_hex: &str,
) -> Result<(), LinkingError> {
    if password.len() != PASSWORD_LENGTH {
        return Err(LinkingError::InvalidPassword);
    }
    let key_bytes = hex_32(wallet_public_key_hex).map_err(LinkingError::InvalidWalletKey)?;
    let key =
        VerifyingKey::from_bytes(&key_bytes).map_err(|e| LinkingError::InvalidWalletKey(e.to_string()))?;
    let signature_bytes =
        hex::decode(signature_hex.trim()).map_err(|e| LinkingError::MalformedSignature(e.to_string()))?;
    let signature: [u8; 64] = signature_bytes.as_slice().try_into().map_err(|_| {
        LinkingError::MalformedSignature(format!("{} bytes instead of 64", signature_bytes.len()))
    })?;
    key.verify_strict(&linking_message(password), &Signature::from_bytes(&signature))
        .map_err(|_| LinkingError::InvalidSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two bytes `ca fe` sixteen times: the protocol documents' test password.
    const CAFE: [u8; 32] = [
        0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca,
        0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe, 0xca, 0xfe,
    ];

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    fn sign_link(key: &SigningKey, password: &[u8]) -> String {
        hex::encode(key.sign(&linking_message(password)).to_bytes())
    }

    /// Pinned, computed independently (Python `hashlib.blake2b(digest_size=32)`): the wallet
    /// verifies exactly these bytes, so a refactor that changes them breaks every pairing.
    #[test]
    fn linking_message_and_connection_id_are_pinned() {
        assert_eq!(
            hex::encode(linking_message(&CAFE)),
            "41b6c2265a248068a00cadf25617b8824b6be7690961eae5df2c6bbeb4a1f9e9"
        );
        assert_eq!(
            hex::encode(connection_id(&CAFE)),
            "98e140d9c01c069aa927797627b1bca4d25971a76549ca59df8ef9d8397afa97"
        );
    }

    #[test]
    fn the_qr_carries_the_password_and_a_signature_the_wallet_can_verify() {
        let identity = key(7);
        let qr = pairing_qr(
            &CAFE,
            &hex::encode(identity.to_bytes()),
            &hex::encode(identity.verifying_key().to_bytes()),
        )
        .unwrap();
        let qr: Value = serde_json::from_str(&qr).unwrap();
        let fields: Vec<&String> = qr.as_object().unwrap().keys().collect();
        assert_eq!(fields.len(), 4, "{qr}");
        assert_eq!(qr["password"], hex::encode(CAFE));
        assert_eq!(qr["purpose"], "general");
        assert_eq!(qr["publicKey"], hex::encode(identity.verifying_key().to_bytes()));
        // The wallet checks the connector's signature exactly like the connector checks the
        // wallet's: the same function verifies it.
        verify_link_signature(
            &CAFE,
            qr["publicKey"].as_str().unwrap(),
            qr["signature"].as_str().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn an_unusable_identity_is_refused_before_anything_is_shown() {
        let identity = key(7);
        let private = hex::encode(identity.to_bytes());
        let other = hex::encode(key(8).verifying_key().to_bytes());
        assert_eq!(
            pairing_qr(&CAFE, &private, &other),
            Err(LinkingError::IdentityMismatch)
        );
        assert!(matches!(
            pairing_qr(&CAFE, "zz", &other),
            Err(LinkingError::InvalidIdentityKey(_))
        ));
        assert!(matches!(
            pairing_qr(&CAFE, &private[..10], &other),
            Err(LinkingError::InvalidIdentityKey(_))
        ));
        assert_eq!(
            pairing_qr(&CAFE[..16], &private, &other),
            Err(LinkingError::InvalidPassword)
        );
    }

    #[test]
    fn a_wallet_signature_verifies_only_for_its_key_and_its_password() {
        let wallet = key(9);
        let public = hex::encode(wallet.verifying_key().to_bytes());
        let signature = sign_link(&wallet, &CAFE);
        assert_eq!(verify_link_signature(&CAFE, &public, &signature), Ok(()));

        let other_password = [1u8; 32];
        assert_eq!(
            verify_link_signature(&other_password, &public, &signature),
            Err(LinkingError::InvalidSignature)
        );
        let impostor = hex::encode(key(10).verifying_key().to_bytes());
        assert_eq!(
            verify_link_signature(&CAFE, &impostor, &signature),
            Err(LinkingError::InvalidSignature)
        );
        // A signature over the password itself, not over the linking message.
        let wrong_message = hex::encode(wallet.sign(&CAFE).to_bytes());
        assert_eq!(
            verify_link_signature(&CAFE, &public, &wrong_message),
            Err(LinkingError::InvalidSignature)
        );
        assert!(matches!(
            verify_link_signature(&CAFE, &public, &signature[..64]),
            Err(LinkingError::MalformedSignature(_))
        ));
        assert!(matches!(
            verify_link_signature(&CAFE, "nothex", &signature),
            Err(LinkingError::InvalidWalletKey(_))
        ));
    }

    #[test]
    fn link_client_messages_are_recognised_and_verified() {
        let wallet = key(11);
        let public = hex::encode(wallet.verifying_key().to_bytes());
        let message = json!({
            "discriminator": "linkClient",
            "publicKey": public,
            "signature": sign_link(&wallet, &CAFE),
        });
        let link = parse_link_client(&message).unwrap().unwrap();
        assert_eq!(link.public_key, public);
        assert_eq!(link.verify(&CAFE), Ok(()));

        let unsigned = parse_link_client(&json!({ "discriminator": "linkClient", "publicKey": public }))
            .unwrap()
            .unwrap();
        assert_eq!(unsigned.verify(&CAFE), Err(LinkingError::MissingSignature));

        assert_eq!(
            parse_link_client(&json!({ "discriminator": "accountList", "accounts": [] })),
            Ok(None)
        );
        assert!(matches!(
            parse_link_client(&json!({ "discriminator": "linkClient" })),
            Err(LinkingError::Malformed(_))
        ));
    }

    #[test]
    fn fresh_secrets_are_fresh() {
        assert_ne!(generate_password().unwrap(), generate_password().unwrap());
        let (private, public) = generate_identity().unwrap();
        // A generated identity is a usable one.
        let qr = pairing_qr(&CAFE, &private, &public).unwrap();
        assert!(qr.contains(&public));
    }
}
