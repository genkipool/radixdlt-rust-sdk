//! radixdlt-rola — Native ROLA (Radix Off-Ledger Authentication) verification.
//!
//! A drop-in Rust replacement for `@radixdlt/rola`:
//!   message = blake2b_256( "R" ‖ challenge ‖ len(dAppDef) ‖ dAppDef ‖ origin )
//!   a proof is valid  ⇔  ed25519_verify(pubKey, message, signature)
//!                    AND derive_virtual_account(pubKey, network) == claimed address
//!
//! (Virtual accounts; accounts with rotated owner keys additionally require a
//! Gateway read — a later phase.)
//!
//! User-facing error text is localized to the system language via `radixdlt-i18n`.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ed25519_dalek::{Signature, VerifyingKey};
use radixdlt_address::{virtual_account_address, AddressError};
use radixdlt_i18n::{tr, Lang};

type Blake2b256 = Blake2b<U32>;

/// Proof of ownership of an account (Ed25519/Curve25519 curve).
#[derive(Debug, Clone)]
pub struct AccountProof {
    pub address: String,
    pub public_key_hex: String,
    pub signature_hex: String,
}

/// ROLA verification errors. Their `Display` text is localized to the system language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolaError {
    /// The challenge hex is invalid.
    InvalidChallengeHex(String),
    /// The dApp definition address is too long (length must fit in one byte).
    DappDefinitionTooLong,
    /// The public key hex is invalid.
    InvalidPublicKeyHex(String),
    /// The public key is not 32 bytes / is otherwise invalid.
    InvalidPublicKey,
    /// The signature hex is invalid.
    InvalidSignatureHex(String),
    /// The signature is not 64 bytes.
    InvalidSignatureLength,
    /// The signature does not verify against the public key and message.
    SignatureMismatch,
    /// The public key does not derive to the claimed address.
    AddressMismatch { derived: String, claimed: String },
    /// Address derivation failed.
    Address(AddressError),
}

impl std::fmt::Display for RolaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            RolaError::InvalidChallengeHex(e) => tr!(
                lang,
                format!("invalid challenge hex: {e}"),
                format!("challenge en hex inválido: {e}")
            ),
            RolaError::DappDefinitionTooLong => tr!(
                lang,
                "dAppDefinitionAddress is too long".to_string(),
                "dAppDefinitionAddress demasiado largo".to_string()
            ),
            RolaError::InvalidPublicKeyHex(e) => tr!(
                lang,
                format!("invalid public key hex: {e}"),
                format!("clave pública en hex inválida: {e}")
            ),
            RolaError::InvalidPublicKey => tr!(
                lang,
                "invalid public key".to_string(),
                "clave pública inválida".to_string()
            ),
            RolaError::InvalidSignatureHex(e) => tr!(
                lang,
                format!("invalid signature hex: {e}"),
                format!("firma en hex inválida: {e}")
            ),
            RolaError::InvalidSignatureLength => tr!(
                lang,
                "signature is not 64 bytes".to_string(),
                "la firma no es de 64 bytes".to_string()
            ),
            RolaError::SignatureMismatch => tr!(
                lang,
                "invalid signature".to_string(),
                "firma inválida".to_string()
            ),
            RolaError::AddressMismatch { derived, claimed } => tr!(
                lang,
                format!("public key does not derive to the claimed address (derived={derived}, claimed={claimed})"),
                format!("la clave no deriva a la dirección reclamada (derivada={derived}, reclamada={claimed})")
            ),
            RolaError::Address(e) => return std::fmt::Display::fmt(e, f),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for RolaError {}

impl From<AddressError> for RolaError {
    fn from(e: AddressError) -> Self {
        RolaError::Address(e)
    }
}

/// Builds the ROLA message (the bytes that are signed/verified), returned raw.
pub fn signature_message(
    challenge_hex: &str,
    dapp_definition: &str,
    origin: &str,
) -> Result<Vec<u8>, RolaError> {
    let challenge =
        hex::decode(challenge_hex).map_err(|e| RolaError::InvalidChallengeHex(e.to_string()))?;
    let dapp_len = dapp_definition.len();
    if dapp_len > 0xff {
        return Err(RolaError::DappDefinitionTooLong);
    }
    let mut msg = Vec::with_capacity(1 + challenge.len() + 1 + dapp_len + origin.len());
    msg.push(b'R');
    msg.extend_from_slice(&challenge);
    msg.push(dapp_len as u8);
    msg.extend_from_slice(dapp_definition.as_bytes());
    msg.extend_from_slice(origin.as_bytes());

    let mut hasher = Blake2b256::new();
    hasher.update(&msg);
    Ok(hasher.finalize().to_vec())
}

/// Verifies a ROLA account proof: returns `Ok(())` when the signature is valid and
/// the public key derives to the claimed (virtual) account address.
pub fn verify_account_proof(
    proof: &AccountProof,
    challenge_hex: &str,
    dapp_definition: &str,
    origin: &str,
    network_id: u8,
) -> Result<(), RolaError> {
    let message = signature_message(challenge_hex, dapp_definition, origin)?;

    // 1) Signature valid for the public key.
    let pk_bytes = hex::decode(&proof.public_key_hex)
        .map_err(|e| RolaError::InvalidPublicKeyHex(e.to_string()))?;
    let pk_arr: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| RolaError::InvalidPublicKey)?;
    let verifying_key =
        VerifyingKey::from_bytes(&pk_arr).map_err(|_| RolaError::InvalidPublicKey)?;

    let sig_bytes = hex::decode(&proof.signature_hex)
        .map_err(|e| RolaError::InvalidSignatureHex(e.to_string()))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| RolaError::InvalidSignatureLength)?;
    let signature = Signature::from_bytes(&sig_arr);

    verifying_key
        .verify_strict(&message, &signature)
        .map_err(|_| RolaError::SignatureMismatch)?;

    // 2) The public key derives to the claimed address (virtual account).
    let derived = virtual_account_address(&proof.public_key_hex, network_id)?;
    if derived != proof.address {
        return Err(RolaError::AddressMismatch {
            derived,
            claimed: proof.address.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rola_message_is_blake2b_256() {
        // 32-byte zero challenge, sample dApp/origin: the message is a blake2b-256 digest.
        let msg = signature_message(
            &"00".repeat(32),
            "account_tdx_2_abc",
            "http://localhost:8080",
        )
        .unwrap();
        assert_eq!(msg.len(), 32);
    }
}
