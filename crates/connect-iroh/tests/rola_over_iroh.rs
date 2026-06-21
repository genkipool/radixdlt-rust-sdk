//! Proves that PamAuthority's ROLA authentication flow works over an iroh (QUIC)
//! transport between two pure-Rust SDK peers — no mobile wallet, no WebRTC.
//!
//! Flow (the same challenge → sign → verify dance PamAuthority uses):
//!   1. The "verifier" (acting as the dApp/daemon) connects over iroh and sends a
//!      ROLA challenge.
//!   2. The "signer" (acting as a pure-Rust wallet, holding a `radixdlt-keystore`
//!      key) builds the ROLA message, signs it and returns the proof.
//!   3. The verifier checks the proof with `radixdlt-rola::verify_account_proof`.

use ed25519_dalek::Signer;
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_keystore::KeyFile;
use radixdlt_rola::{signature_message, verify_account_proof, AccountProof};
use serde_json::json;

#[tokio::test]
async fn rola_login_works_over_iroh() {
    const NETWORK_ID: u8 = 2; // Stokenet
    const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
    const ORIGIN: &str = "iroh://radixdlt-connect-iroh";
    const PASSPHRASE: &str = "test-passphrase";

    // The signer peer holds an encrypted Ed25519 key (a pure-Rust "wallet").
    let key = KeyFile::generate(NETWORK_ID, PASSPHRASE).expect("generate key");
    let expected_address = key.address.clone();

    // Bind both endpoints (relay disabled, loopback).
    let signer = IrohConnector::bind().await.expect("bind signer");
    let verifier = IrohConnector::bind().await.expect("bind verifier");
    let signer_addr = signer.local_addr();

    // A fixed challenge is fine for the test: the signer signs whatever is sent and
    // the verifier checks the very same challenge.
    let challenge = "ab".repeat(32); // 32 bytes hex

    // Signer side: accept, receive the challenge, sign it, return the proof.
    let signer_fut = async {
        let mut ch = signer.accept().await.expect("accept");
        let req = ch.recv_message().await.expect("recv challenge");
        let challenge = req["challenge"].as_str().unwrap().to_string();
        let dapp = req["dappDefinition"].as_str().unwrap().to_string();
        let origin = req["origin"].as_str().unwrap().to_string();

        let sk = key.signing_key(PASSPHRASE).expect("unlock");
        let message = signature_message(&challenge, &dapp, &origin).expect("rola message");
        let signature = sk.sign(&message);

        let proof = json!({
            "address": key.address,
            "publicKey": key.public_key,
            "signature": hex::encode(signature.to_bytes()),
        });
        ch.send_message(&proof).await.expect("send proof");
        ch.finish();
        // Keep the connection alive until the verifier has read and closed it, so
        // the proof is delivered before this side is dropped.
        ch.wait_closed().await;
    };

    // Verifier side: connect, send the challenge, receive and verify the proof.
    let verifier_fut = async {
        let mut ch = verifier.connect(signer_addr).await.expect("connect");
        ch.send_message(&json!({
            "challenge": challenge,
            "dappDefinition": DAPP,
            "origin": ORIGIN,
        }))
        .await
        .expect("send challenge");
        let proof = ch.recv_message().await.expect("recv proof");
        ch.close(); // signal the signer we are done
        proof
    };

    let (_, proof_msg) = tokio::join!(signer_fut, verifier_fut);

    let ap = AccountProof {
        address: proof_msg["address"].as_str().unwrap().to_string(),
        public_key_hex: proof_msg["publicKey"].as_str().unwrap().to_string(),
        signature_hex: proof_msg["signature"].as_str().unwrap().to_string(),
    };

    // The proof must verify natively, and resolve to the signer's account.
    verify_account_proof(&ap, &challenge, DAPP, ORIGIN, NETWORK_ID)
        .expect("ROLA proof must verify over the iroh transport");
    assert_eq!(ap.address, expected_address);
}
