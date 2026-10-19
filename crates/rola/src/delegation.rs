//! The delegation certificate of delegated ROLA (`docs/DELEGATED.md`, §2).
//!
//! An account key and an identity key, both Ed25519 and both used ONCE, sign that from now on
//! a P-256 key — one that lives in a TPM, a Secure Enclave or a passkey and cannot be copied —
//! answers for their two addresses. The two keys are then destroyed: the certificate is all
//! that is left of them, and it is public.

use std::collections::BTreeSet;

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use radixdlt_address::{virtual_account_address, virtual_identity_address};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::canonical::canonical;
use crate::signed::SignedError;

/// What every certificate is prefixed with before hashing, so a certificate can never be taken
/// for a ROLA message (those start with the byte `R`) nor for anything else that is signed.
const DOMAIN: &[u8] = b"pamauthority-delegation/v1\n";

/// What a person may never do when signing as an agent, whatever its certificate lists:
/// open the web panel and approve somebody else's request are for people.
const PEOPLE_ONLY: [&str; 2] = ["web", "approve"];

/// Where the delegated P-256 key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DelegateKind {
    /// A TPM 2.0 key (Linux, or Windows servers), signing a SHA-256 digest.
    Tpm,
    /// An Apple Secure Enclave key.
    SecureEnclave,
    /// A WebAuthn credential (Windows Hello, a passkey): signs the WebAuthn way.
    Webauthn,
}

/// Who the certificate speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Subject {
    /// A person, who signs with a gesture (fingerprint or PIN).
    Person,
    /// A program, which signs by itself and only for the purposes its certificate lists.
    Agent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegate {
    alg: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootSignature {
    #[serde(rename = "publicKey")]
    public_key: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootSignatures {
    account: RootSignature,
    identity: RootSignature,
}

/// The certificate exactly as it travels. Unknown fields are refused: everything in a
/// certificate is signed, and a field nobody reads is a field nobody checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Wire {
    v: u8,
    network: u8,
    account: String,
    identity: String,
    delegate: Delegate,
    kind: DelegateKind,
    #[serde(default)]
    rp_id: Option<String>,
    subject: Subject,
    #[serde(default)]
    purposes: Option<Vec<String>>,
    label: String,
    device: String,
    created_at: u64,
    #[serde(default)]
    attestation: Option<String>,
    signatures: RootSignatures,
}

/// A delegation certificate whose two signatures have been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    /// The network both addresses live on.
    pub network: u8,
    /// The virtual account it speaks for.
    pub account: String,
    /// The virtual identity (persona) it speaks for.
    pub identity: String,
    /// The delegated key: a compressed SEC1 P-256 point.
    pub delegate_key: [u8; 33],
    /// Where that key lives.
    pub kind: DelegateKind,
    /// The WebAuthn relying party, for `kind = webauthn`.
    pub rp_id: Option<String>,
    /// A person or an agent.
    pub subject: Subject,
    /// What an agent may sign for. Empty for a person, who is not limited by the certificate.
    pub purposes: BTreeSet<String>,
    /// The name it was created with: the operating-system user, or the invitation's.
    pub label: String,
    /// The machine it was created on.
    pub device: String,
    /// When it was created, in Unix seconds.
    pub created_at: u64,
    /// The hardware's own statement about the key (a TPM or WebAuthn attestation), when given.
    pub attestation: Option<String>,
}

impl Delegation {
    /// Checks a certificate and returns what it says.
    ///
    /// # Errors
    /// [`SignedError::Malformed`] when it does not read as a version-1 certificate, and
    /// [`SignedError::RootSignature`] when either signature does not verify or either key
    /// does not derive to the address it signs for.
    pub fn verify(value: &Value) -> Result<Delegation, SignedError> {
        let wire: Wire = serde_json::from_value(value.clone())
            .map_err(|e| SignedError::Malformed(format!("delegation: {e}")))?;
        if wire.v != 1 || wire.delegate.alg != "p256" {
            return Err(SignedError::Malformed("delegation: version or algorithm".into()));
        }
        if (wire.kind == DelegateKind::Webauthn) != wire.rp_id.is_some() {
            return Err(SignedError::Malformed(
                "delegation: rpId is for webauthn only".into(),
            ));
        }
        if (wire.subject == Subject::Agent) != wire.purposes.is_some() {
            return Err(SignedError::Malformed(
                "delegation: purposes are for agents only".into(),
            ));
        }
        let delegate_key = delegate_key(&wire.delegate.key)?;
        let digest = digest_of(value)?;
        check_root(
            &wire.signatures.account,
            &digest,
            &wire.account,
            wire.network,
            virtual_account_address,
        )?;
        check_root(
            &wire.signatures.identity,
            &digest,
            &wire.identity,
            wire.network,
            virtual_identity_address,
        )?;
        Ok(Delegation {
            network: wire.network,
            account: wire.account,
            identity: wire.identity,
            delegate_key,
            kind: wire.kind,
            rp_id: wire.rp_id,
            subject: wire.subject,
            purposes: wire.purposes.unwrap_or_default().into_iter().collect(),
            label: wire.label,
            device: wire.device,
            created_at: wire.created_at,
            attestation: wire.attestation,
        })
    }

    /// Whether this certificate may sign for `purpose`: a person for anything, an agent only
    /// for what it lists, and never for what is for people only.
    #[must_use]
    pub fn allows(&self, purpose: &str) -> bool {
        match self.subject {
            Subject::Person => true,
            Subject::Agent => !PEOPLE_ONLY.contains(&purpose) && self.purposes.contains(purpose),
        }
    }
}

/// What a new certificate says, before it is signed.
#[derive(Debug, Clone)]
pub struct Issue<'a> {
    /// Network of the two addresses.
    pub network: u8,
    /// The delegated key, compressed SEC1 (33 bytes).
    pub delegate_key: &'a [u8; 33],
    /// Where that key lives (`"tpm"`, `"secure-enclave"`, `"webauthn"`).
    pub kind: &'a str,
    /// The WebAuthn relying party, for `kind = webauthn` only.
    pub rp_id: Option<&'a str>,
    /// `None` for a person; the purposes an agent may sign for.
    pub agent_purposes: Option<&'a [&'a str]>,
    /// The operating-system user (or the invitation's name).
    pub label: &'a str,
    /// The machine.
    pub device: &'a str,
    /// Unix seconds.
    pub created_at: u64,
}

/// Signs a new certificate with the two one-time keys, which the caller then drops (both
/// zeroize themselves). The addresses are derived here, from those very keys.
///
/// # Errors
/// [`SignedError::Malformed`] when the network is unknown.
pub fn issue(
    spec: &Issue<'_>,
    account_key: &SigningKey,
    identity_key: &SigningKey,
) -> Result<Value, SignedError> {
    let account_pk = hex::encode(account_key.verifying_key().to_bytes());
    let identity_pk = hex::encode(identity_key.verifying_key().to_bytes());
    let unknown = |_| SignedError::Malformed("network".into());
    let mut body = json!({
        "v": 1,
        "network": spec.network,
        "account": virtual_account_address(&account_pk, spec.network).map_err(unknown)?,
        "identity": virtual_identity_address(&identity_pk, spec.network).map_err(unknown)?,
        "delegate": {"alg": "p256", "key": hex::encode(spec.delegate_key)},
        "kind": spec.kind,
        "subject": if spec.agent_purposes.is_some() { "agent" } else { "person" },
        "label": spec.label,
        "device": spec.device,
        "createdAt": spec.created_at,
    });
    if let Some(fields) = body.as_object_mut() {
        if let Some(rp_id) = spec.rp_id {
            fields.insert("rpId".into(), json!(rp_id));
        }
        if let Some(purposes) = spec.agent_purposes {
            fields.insert("purposes".into(), json!(purposes));
        }
    }
    let digest = digest_of(&body)?;
    let signed_by = |key: &SigningKey, pk: String| json!({"publicKey": pk, "signature": hex::encode(key.sign(&digest).to_bytes())});
    if let Some(fields) = body.as_object_mut() {
        fields.insert(
            "signatures".into(),
            json!({
                "account": signed_by(account_key, account_pk),
                "identity": signed_by(identity_key, identity_pk),
            }),
        );
    }
    Ok(body)
}

/// `blake2b_256(DOMAIN ‖ canonical(certificate without its signatures))`.
fn digest_of(certificate: &Value) -> Result<[u8; 32], SignedError> {
    let mut body: Map<String, Value> = certificate
        .as_object()
        .cloned()
        .ok_or_else(|| SignedError::Malformed("delegation: not an object".into()))?;
    body.remove("signatures");
    let text = canonical(&Value::Object(body))
        .ok_or_else(|| SignedError::Malformed("delegation: not canonical".into()))?;
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(DOMAIN);
    hasher.update(text.as_bytes());
    Ok(hasher.finalize().into())
}

fn delegate_key(hex_key: &str) -> Result<[u8; 33], SignedError> {
    let bytes: [u8; 33] = hex::decode(hex_key)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| SignedError::Malformed("delegation: key is not 33 bytes of hex".into()))?;
    p256::ecdsa::VerifyingKey::from_sec1_bytes(&bytes)
        .map_err(|_| SignedError::Malformed("delegation: key is not a P-256 point".into()))?;
    Ok(bytes)
}

fn check_root(
    root: &RootSignature,
    digest: &[u8; 32],
    address: &str,
    network: u8,
    derive: fn(&str, u8) -> Result<String, radixdlt_address::AddressError>,
) -> Result<(), SignedError> {
    let key: [u8; 32] = hex::decode(&root.public_key)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(SignedError::RootSignature)?;
    let signature: [u8; 64] = hex::decode(&root.signature)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(SignedError::RootSignature)?;
    VerifyingKey::from_bytes(&key)
        .map_err(|_| SignedError::RootSignature)?
        .verify_strict(digest, &Signature::from_bytes(&signature))
        .map_err(|_| SignedError::RootSignature)?;
    if derive(&root.public_key, network).ok().as_deref() != Some(address) {
        return Err(SignedError::RootSignature);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const NETWORK: u8 = 2;

    /// A P-256 key "in hardware", and a person's certificate delegating to it.
    pub(crate) fn person() -> (p256::ecdsa::SigningKey, Value) {
        certificate(None, "tpm", None)
    }

    pub(crate) fn certificate(
        purposes: Option<&[&str]>,
        kind: &str,
        rp_id: Option<&str>,
    ) -> (p256::ecdsa::SigningKey, Value) {
        let hardware = p256::ecdsa::SigningKey::from_slice(&[5; 32]).unwrap();
        let point = hardware.verifying_key().to_encoded_point(true);
        let delegate_key: [u8; 33] = point.as_bytes().try_into().unwrap();
        let spec = Issue {
            network: NETWORK,
            delegate_key: &delegate_key,
            kind,
            rp_id,
            agent_purposes: purposes,
            label: "luis",
            device: "pc-de-luis",
            created_at: 1_760_000_000,
        };
        let cert = issue(
            &spec,
            &SigningKey::from_bytes(&[1; 32]),
            &SigningKey::from_bytes(&[2; 32]),
        )
        .unwrap();
        (hardware, cert)
    }

    #[test]
    fn an_issued_certificate_verifies_and_says_what_it_was_issued_with() {
        let (hardware, cert) = person();
        let d = Delegation::verify(&cert).unwrap();
        assert_eq!(d.label, "luis");
        assert_eq!(d.device, "pc-de-luis");
        assert_eq!(d.subject, Subject::Person);
        assert_eq!(d.kind, DelegateKind::Tpm);
        assert_eq!(d.network, NETWORK);
        assert_eq!(d.created_at, 1_760_000_000);
        assert!(d.account.starts_with("account_tdx_2_"));
        assert!(d.identity.starts_with("identity_tdx_2_"));
        assert_eq!(
            &d.delegate_key[..],
            hardware.verifying_key().to_encoded_point(true).as_bytes()
        );
    }

    /// Each field is signed: changing any of them, even one nobody would think matters, breaks it.
    #[test]
    fn every_field_is_signed() {
        let (_, cert) = person();
        for (field, other) in [
            ("label", json!("ana")),
            ("device", json!("otro")),
            ("createdAt", json!(1)),
            ("network", json!(1)),
            ("kind", json!("secure-enclave")),
        ] {
            let mut bent = cert.clone();
            bent[field] = other;
            assert!(
                Delegation::verify(&bent).is_err(),
                "{field} is not covered by the signatures"
            );
        }
    }

    /// Somebody else's account cannot be put in a certificate: its key must derive to it.
    #[test]
    fn the_addresses_must_belong_to_the_signing_keys() {
        let (_, cert) = person();
        let (_, other) = certificate(Some(&["login"]), "tpm", None);
        let mut bent = cert.clone();
        bent["signatures"]["account"] = other["signatures"]["identity"].clone();
        assert_eq!(Delegation::verify(&bent), Err(SignedError::RootSignature));
        let mut swapped = cert;
        let identity = swapped["signatures"]["identity"].clone();
        swapped["signatures"]["identity"] = swapped["signatures"]["account"].clone();
        swapped["signatures"]["account"] = identity;
        assert_eq!(Delegation::verify(&swapped), Err(SignedError::RootSignature));
    }

    #[test]
    fn an_unknown_field_or_version_is_refused() {
        let (_, cert) = person();
        let mut extra = cert.clone();
        extra["extra"] = json!("x");
        assert!(matches!(
            Delegation::verify(&extra),
            Err(SignedError::Malformed(_))
        ));
        let mut v2 = cert.clone();
        v2["v"] = json!(2);
        assert!(matches!(Delegation::verify(&v2), Err(SignedError::Malformed(_))));
        let mut alg = cert;
        alg["delegate"]["alg"] = json!("ed25519");
        assert!(matches!(Delegation::verify(&alg), Err(SignedError::Malformed(_))));
    }

    #[test]
    fn the_delegated_key_must_be_a_curve_point() {
        let (_, cert) = person();
        let mut bent = cert;
        bent["delegate"]["key"] = json!(format!("02{}", "ff".repeat(32)));
        assert!(matches!(
            Delegation::verify(&bent),
            Err(SignedError::Malformed(_))
        ));
    }

    #[test]
    fn rp_id_goes_with_webauthn_and_purposes_with_agents() {
        let (_, cert) = person();
        let mut rp = cert.clone();
        rp["rpId"] = json!("example.com");
        assert!(matches!(Delegation::verify(&rp), Err(SignedError::Malformed(_))));
        let mut purposes = cert;
        purposes["purposes"] = json!(["login"]);
        assert!(matches!(
            Delegation::verify(&purposes),
            Err(SignedError::Malformed(_))
        ));
    }

    #[test]
    fn an_agent_signs_only_what_it_lists_and_never_for_people() {
        let (_, cert) = certificate(Some(&["login", "web", "approve"]), "tpm", None);
        let agent = Delegation::verify(&cert).unwrap();
        assert_eq!(agent.subject, Subject::Agent);
        assert!(agent.allows("login"));
        assert!(!agent.allows("kube"));
        assert!(!agent.allows("web"));
        assert!(!agent.allows("approve"));
        let person = Delegation::verify(&person().1).unwrap();
        assert!(person.allows("web") && person.allows("approve") && person.allows("kube"));
    }
}
