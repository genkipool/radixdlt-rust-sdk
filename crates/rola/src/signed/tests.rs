//! Each test starts from a genuine proof and bends ONE thing.

use super::*;
use crate::delegation::tests::{certificate, person, NETWORK};
use ed25519_dalek::Signer as _;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::SigningKey as P256Secret;
use serde_json::json;

const CHALLENGE: &str = "5c3a0f0e7d1b2a39485766554433221100ffeeddccbbaa998877665544332211";
const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
const ORIGIN: &str = "https://pam.example";

fn request(purpose: &str) -> Request<'_> {
    Request {
        challenge_hex: CHALLENGE,
        dapp_definition: DAPP,
        origin: ORIGIN,
        network_id: NETWORK,
        purpose,
    }
}

fn rola_message() -> Vec<u8> {
    signature_message(CHALLENGE, DAPP, ORIGIN).unwrap()
}

/// What a TPM answers: low-S `r ‖ s` over SHA-256 of the ROLA message.
fn raw_proof(hardware: &P256Secret, cert: &Value) -> Value {
    let signature: P256Signature = hardware.sign(&rola_message());
    let signature = signature.normalize_s().unwrap_or(signature);
    json!({"curve": "delegated", "delegation": cert, "signature": hex::encode(signature.to_bytes())})
}

fn addresses(cert: &Value) -> (String, String) {
    (
        cert["account"].as_str().unwrap().to_string(),
        cert["identity"].as_str().unwrap().to_string(),
    )
}

fn check(address: &str, holder: Holder, proof: &Value, purpose: &str) -> Result<Method, SignedError> {
    verify_signed(
        address,
        holder,
        &SignedProof::from_json(proof)?,
        &request(purpose),
    )
}

#[test]
fn a_wallet_proof_still_verifies_for_its_account_and_its_persona() {
    let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let pk = hex::encode(key.verifying_key().to_bytes());
    let proof = json!({"publicKey": pk, "signature": hex::encode(key.sign(&rola_message()).to_bytes())});
    let account = radixdlt_address::virtual_account_address(&pk, NETWORK).unwrap();
    let identity = radixdlt_address::virtual_identity_address(&pk, NETWORK).unwrap();
    assert_eq!(
        check(&account, Holder::Account, &proof, "login"),
        Ok(Method::Wallet)
    );
    assert_eq!(
        check(&identity, Holder::Identity, &proof, "login"),
        Ok(Method::Wallet)
    );
    assert!(matches!(
        check(&identity, Holder::Account, &proof, "login"),
        Err(SignedError::Rola(RolaError::AddressMismatch { .. }))
    ));
}

#[test]
fn a_delegated_proof_answers_for_both_addresses_and_says_who_signed() {
    let (hardware, cert) = person();
    let proof = raw_proof(&hardware, &cert);
    let (account, identity) = addresses(&cert);
    for (address, holder) in [(account, Holder::Account), (identity, Holder::Identity)] {
        match check(&address, holder, &proof, "login") {
            Ok(Method::Delegated {
                delegation,
                backup_eligible,
            }) => {
                assert_eq!(delegation.label, "luis");
                assert!(!backup_eligible);
            }
            other => panic!("expected a delegated signer, got {other:?}"),
        }
    }
}

/// The delegated signature is bound to the challenge, the dApp and the origin, like ROLA's.
#[test]
fn a_delegated_signature_proves_nothing_about_another_login() {
    let (hardware, cert) = person();
    let proof = raw_proof(&hardware, &cert);
    let (account, _) = addresses(&cert);
    let parsed = SignedProof::from_json(&proof).unwrap();
    let other_challenge = "00".repeat(32);
    for bent in [
        Request {
            challenge_hex: &other_challenge,
            ..request("login")
        },
        Request {
            origin: "https://evil.example",
            ..request("login")
        },
        Request {
            dapp_definition: "account_tdx_2_other",
            ..request("login")
        },
    ] {
        assert_eq!(
            verify_signed(&account, Holder::Account, &parsed, &bent),
            Err(SignedError::DelegateSignature)
        );
    }
}

/// Another hardware key, holding no certificate, cannot sign for the addresses.
#[test]
fn only_the_named_key_signs() {
    let (_, cert) = person();
    let intruder = P256Secret::from_slice(&[9; 32]).unwrap();
    let (account, _) = addresses(&cert);
    assert_eq!(
        check(&account, Holder::Account, &raw_proof(&intruder, &cert), "login"),
        Err(SignedError::DelegateSignature)
    );
}

#[test]
fn a_high_s_twin_is_refused() {
    let (hardware, cert) = person();
    let signature: P256Signature = hardware.sign(&rola_message());
    let low = signature.normalize_s().unwrap_or(signature);
    let (r, s) = low.split_scalars();
    let high = P256Signature::from_scalars(r, -*s).unwrap();
    let proof = json!({"curve": "delegated", "delegation": cert, "signature": hex::encode(high.to_bytes())});
    let (account, _) = addresses(&cert);
    assert_eq!(
        check(&account, Holder::Account, &proof, "login"),
        Err(SignedError::DelegateSignature)
    );
    let good = json!({"curve": "delegated", "delegation": cert, "signature": hex::encode(low.to_bytes())});
    assert!(check(&account, Holder::Account, &good, "login").is_ok());
}

#[test]
fn a_certificate_answers_only_for_its_own_addresses_and_network() {
    let (hardware, cert) = person();
    let proof = raw_proof(&hardware, &cert);
    let (account, identity) = addresses(&cert);
    assert_eq!(
        check(&identity, Holder::Account, &proof, "login"),
        Err(SignedError::WrongAddress)
    );
    assert_eq!(
        check(&account, Holder::Identity, &proof, "login"),
        Err(SignedError::WrongAddress)
    );
    let mainnet = Request {
        network_id: 1,
        ..request("login")
    };
    assert_eq!(
        verify_signed(
            &account,
            Holder::Account,
            &SignedProof::from_json(&proof).unwrap(),
            &mainnet
        ),
        Err(SignedError::WrongAddress)
    );
}

#[test]
fn an_agent_signs_only_for_its_purposes() {
    let (hardware, cert) = certificate(Some(&["login"]), "tpm", None);
    let proof = raw_proof(&hardware, &cert);
    let (account, _) = addresses(&cert);
    assert!(check(&account, Holder::Account, &proof, "login").is_ok());
    assert_eq!(
        check(&account, Holder::Account, &proof, "kube"),
        Err(SignedError::PurposeNotAllowed("kube".into()))
    );
    assert!(matches!(
        check(&account, Holder::Account, &proof, "web"),
        Err(SignedError::PurposeNotAllowed(_))
    ));
}

#[test]
fn a_bent_certificate_inside_a_proof_is_refused() {
    let (hardware, cert) = person();
    let mut bent = cert.clone();
    bent["label"] = json!("admin");
    let (account, _) = addresses(&cert);
    assert_eq!(
        check(&account, Holder::Account, &raw_proof(&hardware, &bent), "login"),
        Err(SignedError::RootSignature)
    );
}

#[test]
fn malformed_proofs_are_errors_not_panics() {
    let (_, cert) = person();
    let (account, _) = addresses(&cert);
    for proof in [
        json!({"curve": "delegated", "signature": "00"}),
        json!({"curve": "delegated", "delegation": cert, "signature": "zz"}),
        json!({"curve": "delegated", "delegation": cert, "signature": "00".repeat(63)}),
        json!({"curve": "delegated", "delegation": cert, "webauthn": {"authenticatorData": "!"}}),
        json!({"curve": "delegated", "delegation": 7, "signature": "00".repeat(64)}),
    ] {
        assert!(
            check(&account, Holder::Account, &proof, "login").is_err(),
            "{proof}"
        );
    }
    assert!(matches!(
        check(&account, Holder::Account, &json!({}), "login"),
        Err(SignedError::Rola(_))
    ));
}

// ── WebAuthn (Windows Hello, passkeys) ──────────────────────────────────────────────────────

const RP: &str = "pam.example";

struct Assertion {
    flags: u8,
    kind: &'static str,
    origin: &'static str,
    rp: &'static str,
    cross_origin: bool,
}

const GOOD: Assertion = Assertion {
    flags: UP | UV,
    kind: "webauthn.get",
    origin: ORIGIN,
    rp: RP,
    cross_origin: false,
};

/// What a browser hands back for `navigator.credentials.get` with the ROLA message as challenge.
fn webauthn_proof(hardware: &P256Secret, cert: &Value, a: &Assertion) -> Value {
    let mut authenticator_data = Sha256::digest(a.rp.as_bytes()).to_vec();
    authenticator_data.push(a.flags);
    authenticator_data.extend_from_slice(&[0, 0, 0, 1]);
    let client_data = json!({
        "type": a.kind,
        "challenge": URL_SAFE_NO_PAD.encode(rola_message()),
        "origin": a.origin,
        "crossOrigin": a.cross_origin,
    })
    .to_string();
    let mut signed = authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(client_data.as_bytes()));
    let signature: P256Signature = hardware.sign(&signed);
    json!({"curve": "delegated", "delegation": cert, "webauthn": {
        "authenticatorData": URL_SAFE_NO_PAD.encode(&authenticator_data),
        "clientDataJSON": URL_SAFE_NO_PAD.encode(client_data.as_bytes()),
        "signature": URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes()),
    }})
}

fn passkey() -> (P256Secret, Value, String) {
    let (hardware, cert) = certificate(None, "webauthn", Some(RP));
    let account = cert["account"].as_str().unwrap().to_string();
    (hardware, cert, account)
}

#[test]
fn a_passkey_assertion_verifies_and_reports_whether_it_can_sync() {
    let (hardware, cert, account) = passkey();
    let bound = check(
        &account,
        Holder::Account,
        &webauthn_proof(&hardware, &cert, &GOOD),
        "web",
    );
    assert!(matches!(
        bound,
        Ok(Method::Delegated {
            backup_eligible: false,
            ..
        })
    ));
    let synced = Assertion {
        flags: UP | UV | BE,
        ..GOOD
    };
    let synced = check(
        &account,
        Holder::Account,
        &webauthn_proof(&hardware, &cert, &synced),
        "web",
    );
    assert!(matches!(
        synced,
        Ok(Method::Delegated {
            backup_eligible: true,
            ..
        })
    ));
}

#[test]
fn a_passkey_must_verify_the_person() {
    let (hardware, cert, account) = passkey();
    for flags in [UP, UV, 0] {
        let a = Assertion { flags, ..GOOD };
        assert_eq!(
            check(
                &account,
                Holder::Account,
                &webauthn_proof(&hardware, &cert, &a),
                "web"
            ),
            Err(SignedError::UserNotVerified),
            "flags {flags:#x}"
        );
    }
}

#[test]
fn a_passkey_assertion_is_bound_to_its_site_and_this_login() {
    let (hardware, cert, account) = passkey();
    for bent in [
        Assertion {
            origin: "https://evil.example",
            ..GOOD
        },
        Assertion {
            rp: "evil.example",
            ..GOOD
        },
        Assertion {
            kind: "webauthn.create",
            ..GOOD
        },
        Assertion {
            cross_origin: true,
            ..GOOD
        },
    ] {
        assert_eq!(
            check(
                &account,
                Holder::Account,
                &webauthn_proof(&hardware, &cert, &bent),
                "web"
            ),
            Err(SignedError::DelegateSignature)
        );
    }
}

#[test]
fn a_signature_of_the_wrong_kind_is_refused() {
    let (hardware, cert, account) = passkey();
    assert!(matches!(
        check(&account, Holder::Account, &raw_proof(&hardware, &cert), "web"),
        Err(SignedError::Malformed(_))
    ));
    let (tpm, tpm_cert) = person();
    let (tpm_account, _) = addresses(&tpm_cert);
    assert!(matches!(
        check(
            &tpm_account,
            Holder::Account,
            &webauthn_proof(&tpm, &tpm_cert, &GOOD),
            "web"
        ),
        Err(SignedError::Malformed(_))
    ));
}

#[test]
fn errors_read_in_both_languages() {
    for e in [
        SignedError::Malformed("x".into()),
        SignedError::RootSignature,
        SignedError::WrongAddress,
        SignedError::PurposeNotAllowed("kube".into()),
        SignedError::DelegateSignature,
        SignedError::UserNotVerified,
        SignedError::Rola(RolaError::SignatureMismatch),
    ] {
        assert!(!e.to_string().is_empty());
    }
}
