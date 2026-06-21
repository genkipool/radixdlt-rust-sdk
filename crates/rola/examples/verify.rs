//! End-to-end ROLA round-trip: a signer produces a proof for a challenge and a
//! verifier checks it — the "log in with Radix" handshake, in a few lines.
//!
//!   cargo run -p radixdlt-rola --example verify

use ed25519_dalek::{Signer, SigningKey};
use radixdlt_address::virtual_account_address;
use radixdlt_rola::{signature_message, verify_account_proof, AccountProof};

fn main() {
    let network_id = 2u8; // stokenet
    let dapp = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
    let origin = "https://example.com";
    let challenge = "00".repeat(32); // 32-byte challenge (hex); random in production

    // --- Signer side (e.g. a wallet) holds an Ed25519 key. ---
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let public_key_hex = hex::encode(signing.verifying_key().to_bytes());
    let address = virtual_account_address(&public_key_hex, network_id).expect("derive address");

    let message = signature_message(&challenge, dapp, origin).expect("rola message");
    let signature = signing.sign(&message);

    // --- Verifier side (e.g. a backend) checks the proof. ---
    let proof = AccountProof {
        address: address.clone(),
        public_key_hex,
        signature_hex: hex::encode(signature.to_bytes()),
    };

    match verify_account_proof(&proof, &challenge, dapp, origin, network_id) {
        Ok(()) => println!("ROLA proof VALID for {address}"),
        Err(e) => {
            eprintln!("ROLA proof INVALID: {e}");
            std::process::exit(1);
        }
    }
}
