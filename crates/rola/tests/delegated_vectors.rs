// In tests a panic IS the failure.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The delegated-ROLA test vectors (`tests/vectors/delegated-v1.json`), shared with the
//! TypeScript verifier (`@pam/core`). Both sides must accept every `valid` case and refuse every
//! `invalid` one; this test also checks that the file is what the keys below produce, so the
//! vectors cannot drift from the code. `UPDATE_VECTORS=1 cargo test` rewrites the file.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::SigningKey;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey as P256Secret};
use radixdlt_rola::{issue, signature_message, verify_signed, Holder, Issue, Request, SignedProof};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/delegated-v1.json");
const CHALLENGE: &str = "5c3a0f0e7d1b2a39485766554433221100ffeeddccbbaa998877665544332211";
const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
const ORIGIN: &str = "https://pam.example";
const RP: &str = "pam.example";
const NETWORK: u8 = 2;

fn hardware(seed: u8) -> (P256Secret, [u8; 33]) {
    let key = P256Secret::from_slice(&[seed; 32]).unwrap();
    let point: [u8; 33] = key
        .verifying_key()
        .to_encoded_point(true)
        .as_bytes()
        .try_into()
        .unwrap();
    (key, point)
}

fn certificate(point: &[u8; 33], kind: &str, rp_id: Option<&str>, purposes: Option<&[&str]>) -> Value {
    let spec = Issue {
        network: NETWORK,
        delegate_key: point,
        kind,
        rp_id,
        agent_purposes: purposes,
        label: "luis",
        device: "pc-de-luis",
        created_at: 1_760_000_000,
    };
    issue(
        &spec,
        &SigningKey::from_bytes(&[1; 32]),
        &SigningKey::from_bytes(&[2; 32]),
    )
    .unwrap()
}

fn message() -> Vec<u8> {
    signature_message(CHALLENGE, DAPP, ORIGIN).unwrap()
}

fn raw(key: &P256Secret, cert: &Value) -> Value {
    let s: Signature = key.sign(&message());
    let s = s.normalize_s().unwrap_or(s);
    json!({"curve": "delegated", "delegation": cert, "signature": hex::encode(s.to_bytes())})
}

fn webauthn(key: &P256Secret, cert: &Value, flags: u8) -> Value {
    let mut data = Sha256::digest(RP.as_bytes()).to_vec();
    data.push(flags);
    data.extend_from_slice(&[0, 0, 0, 7]);
    let client = format!(
        r#"{{"type":"webauthn.get","challenge":"{}","origin":"{ORIGIN}","crossOrigin":false}}"#,
        URL_SAFE_NO_PAD.encode(message())
    );
    let mut signed = data.clone();
    signed.extend_from_slice(&Sha256::digest(client.as_bytes()));
    let s: Signature = key.sign(&signed);
    json!({"curve": "delegated", "delegation": cert, "webauthn": {
        "authenticatorData": URL_SAFE_NO_PAD.encode(&data),
        "clientDataJSON": URL_SAFE_NO_PAD.encode(client.as_bytes()),
        "signature": URL_SAFE_NO_PAD.encode(s.to_der().as_bytes()),
    }})
}

fn case(name: &str, proof: &Value, holder: &str, purpose: &str, valid: bool) -> Value {
    let address = match holder {
        "account" => &proof["delegation"]["account"],
        _ => &proof["delegation"]["identity"],
    };
    json!({"name": name, "address": address, "holder": holder, "purpose": purpose, "proof": proof, "valid": valid})
}

fn vectors() -> Value {
    let (tpm, tpm_point) = hardware(5);
    let person = certificate(&tpm_point, "tpm", None, None);
    let (agent_key, agent_point) = hardware(6);
    let agent = certificate(&agent_point, "tpm", None, Some(&["login", "kube"]));
    let (passkey, passkey_point) = hardware(8);
    let web = certificate(&passkey_point, "webauthn", Some(RP), None);
    let mut bent = person.clone();
    bent["label"] = json!("admin");
    let (intruder, _) = hardware(9);
    json!({
        "spec": "pamauthority delegated ROLA v1 — crates/rola/docs/DELEGATED.md",
        "challenge": CHALLENGE, "dAppDefinition": DAPP, "origin": ORIGIN, "networkId": NETWORK,
        "cases": [
            case("tpm person, account", &raw(&tpm, &person), "account", "login", true),
            case("tpm person, identity", &raw(&tpm, &person), "identity", "web", true),
            case("agent, listed purpose", &raw(&agent_key, &agent), "account", "kube", true),
            case("agent, unlisted purpose", &raw(&agent_key, &agent), "account", "aws", false),
            case("agent, people only", &raw(&agent_key, &agent), "account", "web", false),
            case("passkey, user verified", &webauthn(&passkey, &web, 0x05), "identity", "web", true),
            case("passkey, synced (BE)", &webauthn(&passkey, &web, 0x0d), "identity", "web", true),
            case("passkey, not user verified", &webauthn(&passkey, &web, 0x01), "identity", "web", false),
            case("certificate changed", &raw(&tpm, &bent), "account", "login", false),
            case("another hardware key", &raw(&intruder, &person), "account", "login", false),
        ],
    })
}

fn verdict(vectors: &Value, case: &Value) -> bool {
    let request = Request {
        challenge_hex: vectors["challenge"].as_str().unwrap(),
        dapp_definition: vectors["dAppDefinition"].as_str().unwrap(),
        origin: vectors["origin"].as_str().unwrap(),
        network_id: NETWORK,
        purpose: case["purpose"].as_str().unwrap(),
    };
    let holder = if case["holder"] == "account" {
        Holder::Account
    } else {
        Holder::Identity
    };
    SignedProof::from_json(&case["proof"])
        .and_then(|p| verify_signed(case["address"].as_str().unwrap(), holder, &p, &request))
        .is_ok()
}

#[test]
fn the_vectors_file_is_what_the_code_produces() {
    let fresh = serde_json::to_string_pretty(&vectors()).unwrap() + "\n";
    if std::env::var_os("UPDATE_VECTORS").is_some() {
        std::fs::write(FILE, &fresh).unwrap();
    }
    let on_disk = std::fs::read_to_string(FILE).expect("run once with UPDATE_VECTORS=1");
    assert_eq!(
        on_disk, fresh,
        "the vectors are stale: UPDATE_VECTORS=1 cargo test"
    );
}

#[test]
fn every_vector_gets_its_verdict() {
    // The cases as the code makes them; the test above holds the file to the same bytes.
    let vectors = vectors();
    for case in vectors["cases"].as_array().unwrap() {
        assert_eq!(
            verdict(&vectors, case),
            case["valid"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
    }
}
