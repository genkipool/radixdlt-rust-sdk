// Examples are demonstrations: `expect` is the idiomatic way to keep them short and to
// show the failure loudly. Library code keeps the deny.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Can a wallet interaction complete without a single UDP socket?
//!
//! The answer decides where the connector can be hosted. WebRTC normally wants UDP, and
//! serverless platforms tend not to offer it. But ICE can be restricted to relay candidates,
//! and a TURN server can be reached over TCP — in which case the only sockets this process
//! opens are outbound TCP/443: one to the signaling server, one to the relay.
//!
//!   cargo run --example relay_tcp -- [--mode relay-tcp|default] [--state <path>]
//!
//! `--mode default` is the control: the stock configuration, free to pick UDP. Run both.
//! Open the Radix Wallet on your phone and approve when the request appears.

use std::time::{Duration, Instant};

use radixdlt_connect::{extract_proofs, Connector, DappContext, IceServer, LinkState};
use radixdlt_rola::{verify_account_proof, AccountProof};

// Matches the daemon's defaults, so this measures the transport and nothing else.
const DAPP_DEFAULT: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
const ORIGIN: &str = "http://localhost:8080";
const NETWORK_ID: u8 = 2;

/// The relay the default ICE set already carries, kept to its TLS/TCP endpoint alone.
fn tcp_only_ice() -> Vec<IceServer> {
    vec![IceServer::turn(
        "turns:standard.relay.metered.ca:443?transport=tcp",
        "51253affa7c2960189ce8cb6",
        "3HWkp3Wgg2cujD2g",
    )]
}

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
    let state_path = get("--state", &format!("{home}/.config/radixdlt-pam/connector.json"));
    let timeout_secs: u64 = get("--timeout", "90").parse().unwrap_or(90);
    let dapp = get("--dapp", DAPP_DEFAULT);
    let relay_tcp = get("--mode", "relay-tcp") == "relay-tcp";

    let state = LinkState::load(&state_path).expect("loading the link state");
    let password = state.password_bytes().expect("no pairing in the state file");

    let mut challenge = [0u8; 32];
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(&mut challenge);
    let challenge_hex = hex::encode(challenge);

    let connector = if relay_tcp {
        println!("mode: RELAY-TCP — ICE restricted to relay, TURN over TLS/TCP 443 only");
        println!("      no host or server-reflexive candidate is gathered, so no UDP path exists");
        Connector::new()
            .with_ice_servers(tcp_only_ice())
            .with_relay_only(true)
    } else {
        println!("mode: DEFAULT (control) — stock ICE set, free to choose UDP");
        Connector::new()
    };

    println!(">>> OPEN THE RADIX WALLET AND APPROVE THE REQUEST <<<  (max {timeout_secs}s)");
    let started = Instant::now();

    let ctx = DappContext::new(NETWORK_ID, &dapp, ORIGIN);
    let response = match connector
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
            println!(
                "RESULT: FAILED after {:.1}s — {e}",
                started.elapsed().as_secs_f32()
            );
            std::process::exit(2);
        }
    };
    let elapsed = started.elapsed().as_secs_f32();

    let proofs = extract_proofs(&response).expect("response carries proofs");
    assert!(!proofs.is_empty(), "the wallet returned no proofs");

    let mut all_ok = true;
    for (address, proof) in &proofs {
        let ap = AccountProof {
            address: address.clone(),
            public_key_hex: proof["publicKey"].as_str().unwrap_or_default().to_string(),
            signature_hex: proof["signature"].as_str().unwrap_or_default().to_string(),
        };
        match verify_account_proof(&ap, &challenge_hex, &dapp, ORIGIN, NETWORK_ID) {
            Ok(()) => println!("  proof VALID: {address}"),
            Err(e) => {
                println!("  proof INVALID for {address}: {e}");
                all_ok = false;
            }
        }
    }
    println!(
        "RESULT: {} in {elapsed:.1}s",
        if all_ok { "SUCCESS" } else { "PROOF REJECTED" }
    );
    std::process::exit(if all_ok { 0 } else { 4 });
}
