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

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ed25519_dalek::{Signature, VerifyingKey};
use radixdlt_address::{virtual_account_address, AddressError};
use radixdlt_i18n::{tr, Lang};

type Blake2b256 = Blake2b<U32>;

/// Proof of ownership of an account (Ed25519/Curve25519 curve).
#[derive(Debug, Clone)]
pub struct AccountProof {
    /// The account address being claimed. Verification re-derives it from the public key and
    /// refuses a mismatch: without that step a proof would say "I hold SOME key", not "I hold
    /// the key to THIS account".
    pub address: String,
    /// Ed25519 public key, hex-encoded.
    pub public_key_hex: String,
    /// Signature over the ROLA message, hex-encoded.
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
    AddressMismatch {
        /// Address the presented public key actually derives to.
        derived: String,
        /// Address the proof claimed.
        claimed: String,
    },
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
///
/// # Errors
/// [`RolaError::InvalidChallengeHex`] when the challenge is not hex, and
/// [`RolaError::DappDefinitionTooLong`] beyond 255 bytes — the length travels as a single
/// prefix byte, so a longer value could not be encoded unambiguously.
pub fn signature_message(
    challenge_hex: &str,
    dapp_definition: &str,
    origin: &str,
) -> Result<Vec<u8>, RolaError> {
    let challenge = hex::decode(challenge_hex).map_err(|e| RolaError::InvalidChallengeHex(e.to_string()))?;
    // One length-prefixed byte, so the guard and the conversion are the SAME statement. Written
    // as a check followed by `as u8`, a later edit could move them apart and the prefix would
    // silently wrap — which changes the signed message, and therefore what the signature means.
    let dapp_len = u8::try_from(dapp_definition.len()).map_err(|_| RolaError::DappDefinitionTooLong)?;
    let mut msg = Vec::with_capacity(1 + challenge.len() + 1 + dapp_len as usize + origin.len());
    msg.push(b'R');
    msg.extend_from_slice(&challenge);
    msg.push(dapp_len);
    msg.extend_from_slice(dapp_definition.as_bytes());
    msg.extend_from_slice(origin.as_bytes());

    let mut hasher = Blake2b256::new();
    hasher.update(&msg);
    Ok(hasher.finalize().to_vec())
}

/// Verifies a ROLA account proof: returns `Ok(())` when the signature is valid and
/// the public key derives to the claimed (virtual) account address.
///
/// # Errors
/// [`RolaError::SignatureMismatch`] when the signature does not verify, and
/// [`RolaError::AddressMismatch`] when it does but the key derives to a DIFFERENT account
/// than the one claimed — a valid signature over the right message still proves nothing
/// about an account its key does not control.
pub fn verify_account_proof(
    proof: &AccountProof,
    challenge_hex: &str,
    dapp_definition: &str,
    origin: &str,
    network_id: u8,
) -> Result<(), RolaError> {
    let message = signature_message(challenge_hex, dapp_definition, origin)?;

    // 1) Signature valid for the public key.
    let pk_bytes =
        hex::decode(&proof.public_key_hex).map_err(|e| RolaError::InvalidPublicKeyHex(e.to_string()))?;
    let pk_arr: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| RolaError::InvalidPublicKey)?;
    let verifying_key = VerifyingKey::from_bytes(&pk_arr).map_err(|_| RolaError::InvalidPublicKey)?;

    let sig_bytes =
        hex::decode(&proof.signature_hex).map_err(|e| RolaError::InvalidSignatureHex(e.to_string()))?;
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
        let msg = signature_message(&"00".repeat(32), "account_tdx_2_abc", "http://localhost:8080").unwrap();
        assert_eq!(msg.len(), 32);
    }

    use ed25519_dalek::{Signer, SigningKey};

    const CHALLENGE: &str = "aa";
    const DAPP: &str = "account_tdx_2_12yf9gd53yfep7a669fv2t3wm7nz9zeezwd04n02a433ker8vza6rhe";
    const ORIGIN: &str = "https://example.test";
    const NETWORK: u8 = 2;

    /// A key, and a genuine proof from it, so the tests below can bend ONE thing at a time.
    fn signed_proof() -> (SigningKey, AccountProof) {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk_hex = hex::encode(sk.verifying_key().to_bytes());
        let address = virtual_account_address(&pk_hex, NETWORK).unwrap();
        let msg = signature_message(&CHALLENGE.repeat(32), DAPP, ORIGIN).unwrap();
        let proof = AccountProof {
            address,
            public_key_hex: pk_hex,
            signature_hex: hex::encode(sk.sign(&msg).to_bytes()),
        };
        (sk, proof)
    }

    /// The baseline: a real signature over the real message, for the address the key controls.
    /// Everything below is this, with one thing wrong.
    #[test]
    fn a_genuine_proof_verifies() {
        let (_, proof) = signed_proof();
        assert!(verify_account_proof(&proof, &CHALLENGE.repeat(32), DAPP, ORIGIN, NETWORK).is_ok());
    }

    /// The one that matters most. If verification ever became a no-op — a refactor gone wrong, a
    /// mutation, an early `Ok(())` — this is the test that would notice, because a signature
    /// that signs nothing must not be accepted.
    #[test]
    fn a_forged_signature_is_rejected() {
        let (_, mut proof) = signed_proof();
        proof.signature_hex = hex::encode([0u8; 64]);
        assert_eq!(
            verify_account_proof(&proof, &CHALLENGE.repeat(32), DAPP, ORIGIN, NETWORK),
            Err(RolaError::SignatureMismatch)
        );
    }

    /// A signature is only meaningful for the CHALLENGE it signed. Accepting another one is
    /// what a replay is: a proof captured once, presented again for a different login.
    #[test]
    fn a_signature_over_another_challenge_is_rejected() {
        let (_, proof) = signed_proof();
        let other = "bb".repeat(32);
        assert_eq!(
            verify_account_proof(&proof, &other, DAPP, ORIGIN, NETWORK),
            Err(RolaError::SignatureMismatch)
        );
    }

    /// The message binds the dApp and the origin. A proof obtained by one site must not be
    /// presentable by another, or any dApp could harvest proofs for every other.
    #[test]
    fn a_proof_does_not_transfer_to_another_dapp_or_origin() {
        let (_, proof) = signed_proof();
        let other_dapp = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
        assert!(verify_account_proof(&proof, &CHALLENGE.repeat(32), other_dapp, ORIGIN, NETWORK).is_err());
        assert!(
            verify_account_proof(&proof, &CHALLENGE.repeat(32), DAPP, "https://evil.test", NETWORK).is_err()
        );
    }

    /// A valid signature proves ownership of a KEY. Claiming an account that key does not
    /// control has to fail here, or a proof would authenticate anyone as anybody.
    #[test]
    fn a_valid_signature_for_the_wrong_account_is_rejected() {
        let (_, mut proof) = signed_proof();
        proof.address = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk".to_string();
        match verify_account_proof(&proof, &CHALLENGE.repeat(32), DAPP, ORIGIN, NETWORK) {
            Err(RolaError::AddressMismatch { .. }) => {}
            other => panic!("expected AddressMismatch, got {other:?}"),
        }
    }

    /// The same key derives a DIFFERENT address on each network, so verifying against the wrong
    /// one must fail rather than quietly accept a mainnet claim on stokenet.
    #[test]
    fn the_network_is_part_of_the_identity() {
        let (_, proof) = signed_proof();
        assert!(verify_account_proof(&proof, &CHALLENGE.repeat(32), DAPP, ORIGIN, 1).is_err());
    }

    /// Malformed input must be reported, not panic: these values arrive from a peer.
    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        let (_, base) = signed_proof();
        let mut p = base.clone();
        p.public_key_hex = "zz".to_string();
        assert!(matches!(
            verify_account_proof(&p, &CHALLENGE.repeat(32), DAPP, ORIGIN, NETWORK),
            Err(RolaError::InvalidPublicKeyHex(_))
        ));

        let mut p = base.clone();
        p.signature_hex = hex::encode([0u8; 10]);
        assert_eq!(
            verify_account_proof(&p, &CHALLENGE.repeat(32), DAPP, ORIGIN, NETWORK),
            Err(RolaError::InvalidSignatureLength)
        );

        assert!(verify_account_proof(&base, "not hex", DAPP, ORIGIN, NETWORK).is_err());
    }
}
