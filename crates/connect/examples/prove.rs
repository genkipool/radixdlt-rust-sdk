//! End-to-end check against a REAL wallet: reuse an existing pairing
//! (`~/.config/radix-pam/connector.json`), ask the phone for a ROLA proof over
//! WebRTC and verify it natively in Rust with `radixdlt-rola`.
//!
//!   cargo run --example prove -- [--state <path>] [--timeout <seconds>] [--dapp <addr>]
//!
//! Open the Radix Wallet on your phone and approve the request when it appears.
//! This is the flagship "log in with Radix" flow: radixdlt-connect (get the proof)
//! + radixdlt-rola (verify it), no Node involved.

use std::time::Duration;

use radixdlt_connect::{extract_proofs, Connector, DappContext, LinkState};
use radixdlt_rola::{verify_account_proof, AccountProof};

// Sample Stokenet dApp definition; override with --dapp for your own dApp.
const DAPP_DEFAULT: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
const ORIGIN: &str = "http://localhost:8080";
const NETWORK_ID: u8 = 2;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |name: &str, def: &str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| def.to_string())
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let state_path = get(
        "--state",
        &format!("{home}/.config/radix-pam/connector.json"),
    );
    let timeout_secs: u64 = get("--timeout", "90").parse().unwrap_or(90);
    let dapp = get("--dapp", DAPP_DEFAULT);

    let state = match LinkState::load(&state_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error loading the link: {e}");
            std::process::exit(1);
        }
    };
    let password = match state.password_bytes() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e} (paired?)");
            std::process::exit(1);
        }
    };

    // Random 32-byte challenge.
    let mut challenge = [0u8; 32];
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(&mut challenge);
    let challenge_hex = hex::encode(challenge);

    println!(">>> OPEN THE RADIX WALLET ON YOUR PHONE AND APPROVE THE REQUEST <<<");
    println!("Establishing the WebRTC channel and sending the request (max {timeout_secs}s)…");

    let ctx = DappContext::new(NETWORK_ID, &dapp, ORIGIN);
    let response = match Connector::new()
        .request_account_proof(
            &password,
            &challenge_hex,
            &ctx,
            true,
            Duration::from_secs(timeout_secs),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("connector failure: {e}");
            std::process::exit(2);
        }
    };

    let proofs = match extract_proofs(&response) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("response without proofs: {e}");
            std::process::exit(3);
        }
    };
    if proofs.is_empty() {
        eprintln!("the wallet returned no proofs.");
        std::process::exit(3);
    }

    let mut all_ok = true;
    for (address, proof) in &proofs {
        let ap = AccountProof {
            address: address.clone(),
            public_key_hex: proof["publicKey"].as_str().unwrap_or_default().to_string(),
            signature_hex: proof["signature"].as_str().unwrap_or_default().to_string(),
        };
        match verify_account_proof(&ap, &challenge_hex, &dapp, ORIGIN, NETWORK_ID) {
            Ok(()) => println!("✓ Proof VALID (verified natively in Rust): {address}"),
            Err(e) => {
                println!("✗ Invalid proof for {address}: {e}");
                all_ok = false;
            }
        }
    }

    std::process::exit(if all_ok { 0 } else { 4 });
}
