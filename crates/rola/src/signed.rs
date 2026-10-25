//! `verify_signed`: one check for every way an address can sign a ROLA challenge.
//!
//! A proof arrives either from a Radix Wallet (an Ed25519 key that derives to the address) or
//! from delegated ROLA (a certificate plus a P-256 signature by the key it names). Whoever
//! verifies does not choose between them: they hand over the JSON and get back WHO signed and
//! HOW, so the rules about which methods count (a company's signing policy) live in one place
//! above this one, and the cryptography lives here, once.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature as P256Signature, VerifyingKey as P256Key};
use radixdlt_i18n::{tr, Lang};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::delegation::{DelegateKind, Delegation};
use crate::{signature_message, verify_account_proof, verify_persona_proof, AccountProof, RolaError};

/// WebAuthn flags (authenticator data, byte 32): user present, user verified, backup eligible.
const UP: u8 = 0x01;
const UV: u8 = 0x04;
const BE: u8 = 0x08;

/// Why a signed proof was refused. `Display` is localized to the system language.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignedError {
    /// A wallet proof that ROLA refuses.
    Rola(RolaError),
    /// It does not read as a proof (the detail says which part).
    Malformed(String),
    /// A delegation certificate whose root signatures do not hold.
    RootSignature,
    /// A valid certificate, for another address or another network.
    WrongAddress,
    /// A valid agent certificate that does not list this purpose.
    PurposeNotAllowed(String),
    /// The delegated key did not sign this challenge, dApp and origin.
    DelegateSignature,
    /// A WebAuthn signature made without the person's fingerprint or PIN.
    UserNotVerified,
}

impl std::fmt::Display for SignedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            SignedError::Rola(e) => return std::fmt::Display::fmt(e, f),
            SignedError::Malformed(what) => tr!(
                lang,
                format!("malformed proof: {what}"),
                format!("prueba mal formada: {what}")
            ),
            SignedError::RootSignature => tr!(
                lang,
                "the delegation certificate is not signed by its addresses".to_string(),
                "el certificado de delegación no lo firman sus direcciones".to_string()
            ),
            SignedError::WrongAddress => tr!(
                lang,
                "the delegation is for another address or network".to_string(),
                "la delegación es de otra dirección o de otra red".to_string()
            ),
            SignedError::PurposeNotAllowed(p) => tr!(
                lang,
                format!("this agent may not sign for «{p}»"),
                format!("este agente no puede firmar para «{p}»")
            ),
            SignedError::DelegateSignature => tr!(
                lang,
                "invalid delegated signature".to_string(),
                "firma delegada inválida".to_string()
            ),
            SignedError::UserNotVerified => tr!(
                lang,
                "signed without fingerprint or PIN".to_string(),
                "firmado sin huella ni PIN".to_string()
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for SignedError {}

impl From<RolaError> for SignedError {
    fn from(e: RolaError) -> Self {
        SignedError::Rola(e)
    }
}

/// What is being signed in: the ROLA context, plus what the signature is FOR.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The one-time challenge, hex.
    pub challenge_hex: &'a str,
    /// The dApp definition address.
    pub dapp_definition: &'a str,
    /// The origin the challenge was issued for.
    pub origin: &'a str,
    /// The network the addresses live on.
    pub network_id: u8,
    /// What the signature opens: `"login"`, `"web"`, `"approve"`, `"network"`, `"kube"`, … An
    /// agent's certificate must list it; a wallet or a person is not limited by it.
    pub purpose: &'a str,
}

/// Which of the two addresses a proof claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// An `account_…` address.
    Account,
    /// An `identity_…` address (a persona).
    Identity,
}

/// How a proof was signed, once it has been verified.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    /// By the Radix Wallet (an Ed25519 key of the address itself).
    Wallet,
    /// By a delegated key, named by this certificate.
    Delegated {
        /// The verified certificate.
        delegation: Box<Delegation>,
        /// A WebAuthn credential that may be copied to other devices (a synced passkey).
        /// Always `false` for a TPM or a Secure Enclave.
        backup_eligible: bool,
    },
}

/// The signature a delegated proof carries.
#[derive(Debug, Clone)]
enum DelegateSignature {
    /// `r ‖ s` (64 bytes) over `SHA-256(ROLA message)`, low S.
    Raw([u8; 64]),
    /// A WebAuthn assertion whose challenge is the ROLA message.
    WebAuthn {
        authenticator_data: Vec<u8>,
        client_data_json: Vec<u8>,
        der: Vec<u8>,
    },
}

/// A proof as it arrives, before any check: a wallet's `{"publicKey", "signature"}`, or a
/// delegated `{"curve": "delegated", "delegation", "signature" | "webauthn"}`.
#[derive(Debug, Clone)]
pub struct SignedProof(Shape);

#[derive(Debug, Clone)]
enum Shape {
    Wallet {
        public_key_hex: String,
        signature_hex: String,
    },
    Delegated {
        delegation: Value,
        signature: DelegateSignature,
    },
}

impl SignedProof {
    /// Reads the `proof` object of a login answer.
    ///
    /// # Errors
    /// [`SignedError::Malformed`] when it is neither shape.
    pub fn from_json(proof: &Value) -> Result<SignedProof, SignedError> {
        let text = |key: &str| {
            proof
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        if proof.get("curve").and_then(Value::as_str) != Some("delegated") {
            return Ok(SignedProof(Shape::Wallet {
                public_key_hex: text("publicKey"),
                signature_hex: text("signature"),
            }));
        }
        let delegation = proof
            .get("delegation")
            .cloned()
            .ok_or_else(|| SignedError::Malformed("delegation missing".into()))?;
        let signature = match proof.get("webauthn") {
            Some(assertion) => webauthn_of(assertion)?,
            None => DelegateSignature::Raw(
                hex::decode(text("signature"))
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| SignedError::Malformed("signature is not 64 bytes of hex".into()))?,
            ),
        };
        Ok(SignedProof(Shape::Delegated {
            delegation,
            signature,
        }))
    }
}

fn webauthn_of(assertion: &Value) -> Result<DelegateSignature, SignedError> {
    let bytes = |key: &str| {
        assertion
            .get(key)
            .and_then(Value::as_str)
            .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
            .ok_or_else(|| SignedError::Malformed(format!("webauthn: {key}")))
    };
    Ok(DelegateSignature::WebAuthn {
        authenticator_data: bytes("authenticatorData")?,
        client_data_json: bytes("clientDataJSON")?,
        der: bytes("signature")?,
    })
}

/// Verifies that `proof` signs `request` for `address`, and says how it was signed.
///
/// # Errors
/// A [`SignedError`] naming the first thing that does not hold.
pub fn verify_signed(
    address: &str,
    holder: Holder,
    proof: &SignedProof,
    request: &Request<'_>,
) -> Result<Method, SignedError> {
    match &proof.0 {
        Shape::Wallet {
            public_key_hex,
            signature_hex,
        } => {
            let wallet = AccountProof {
                address: address.to_string(),
                public_key_hex: public_key_hex.clone(),
                signature_hex: signature_hex.clone(),
            };
            let verify = match holder {
                Holder::Account => verify_account_proof,
                Holder::Identity => verify_persona_proof,
            };
            verify(
                &wallet,
                request.challenge_hex,
                request.dapp_definition,
                request.origin,
                request.network_id,
            )?;
            Ok(Method::Wallet)
        }
        Shape::Delegated {
            delegation,
            signature,
        } => verify_delegated(address, holder, delegation, signature, request),
    }
}

fn verify_delegated(
    address: &str,
    holder: Holder,
    certificate: &Value,
    signature: &DelegateSignature,
    request: &Request<'_>,
) -> Result<Method, SignedError> {
    let delegation = Delegation::verify(certificate)?;
    let claimed = match holder {
        Holder::Account => &delegation.account,
        Holder::Identity => &delegation.identity,
    };
    if delegation.network != request.network_id || claimed != address {
        return Err(SignedError::WrongAddress);
    }
    if !delegation.allows(request.purpose) {
        return Err(SignedError::PurposeNotAllowed(request.purpose.to_string()));
    }
    let message = signature_message(request.challenge_hex, request.dapp_definition, request.origin)?;
    let key =
        P256Key::from_sec1_bytes(&delegation.delegate_key).map_err(|_| SignedError::DelegateSignature)?;
    let backup_eligible = match (delegation.kind, signature) {
        (DelegateKind::Tpm | DelegateKind::SecureEnclave, DelegateSignature::Raw(rs)) => {
            raw_verifies(&key, &message, rs)?;
            false
        }
        (DelegateKind::Webauthn, webauthn @ DelegateSignature::WebAuthn { .. }) => {
            let rp_id = delegation.rp_id.as_deref().unwrap_or_default();
            webauthn_verifies(&key, rp_id, &message, request.origin, webauthn)?
        }
        _ => {
            return Err(SignedError::Malformed(
                "signature does not match the key's kind".into(),
            ))
        }
    };
    Ok(Method::Delegated {
        delegation: Box::new(delegation),
        backup_eligible,
    })
}

/// A TPM or Secure Enclave signature: ECDSA P-256 over SHA-256 of the ROLA message, with a low
/// S (the high-S twin of a valid signature is refused, so a signature has one spelling).
fn raw_verifies(key: &P256Key, message: &[u8], rs: &[u8; 64]) -> Result<(), SignedError> {
    let signature = P256Signature::from_slice(rs).map_err(|_| SignedError::DelegateSignature)?;
    // A high S is the malleable twin of a valid signature: only the low form is accepted.
    if signature.normalize_s() != signature {
        return Err(SignedError::DelegateSignature);
    }
    key.verify(message, &signature)
        .map_err(|_| SignedError::DelegateSignature)
}

/// A WebAuthn assertion over the ROLA message: right relying party, the person verified
/// (fingerprint or PIN), a `webauthn.get` for this challenge and origin, and the signature over
/// `authenticatorData ‖ SHA-256(clientDataJSON)`. Returns whether the credential may be synced.
fn webauthn_verifies(
    key: &P256Key,
    rp_id: &str,
    message: &[u8],
    origin: &str,
    assertion: &DelegateSignature,
) -> Result<bool, SignedError> {
    let DelegateSignature::WebAuthn {
        authenticator_data,
        client_data_json,
        der,
    } = assertion
    else {
        return Err(SignedError::DelegateSignature);
    };
    let (rp_hash, rest) = authenticator_data
        .split_at_checked(32)
        .ok_or_else(|| SignedError::Malformed("webauthn: authenticator data".into()))?;
    let flags = *rest
        .first()
        .ok_or_else(|| SignedError::Malformed("webauthn: authenticator data".into()))?;
    if rp_hash != Sha256::digest(rp_id.as_bytes()).as_slice() {
        return Err(SignedError::DelegateSignature);
    }
    // Both flags, each on its own: present (a touch) AND verified (fingerprint or PIN).
    if flags & UP == 0 || flags & UV == 0 {
        return Err(SignedError::UserNotVerified);
    }
    let client: Value = serde_json::from_slice(client_data_json)
        .map_err(|_| SignedError::Malformed("webauthn: clientDataJSON".into()))?;
    let field = |key: &str| client.get(key).and_then(Value::as_str).unwrap_or_default();
    if field("type") != "webauthn.get"
        || field("challenge") != URL_SAFE_NO_PAD.encode(message)
        || field("origin") != origin
        || client.get("crossOrigin").and_then(Value::as_bool) == Some(true)
    {
        return Err(SignedError::DelegateSignature);
    }
    let signature = P256Signature::from_der(der).map_err(|_| SignedError::DelegateSignature)?;
    let signature = signature.normalize_s();
    let mut signed = authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(client_data_json));
    key.verify(&signed, &signature)
        .map_err(|_| SignedError::DelegateSignature)?;
    Ok(flags & BE != 0)
}

#[cfg(test)]
mod tests;
