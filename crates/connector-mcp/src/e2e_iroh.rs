//! End to end over iroh: the MCP tools themselves — `pair_wallet`, `pair_status`, a signing
//! request, `list_wallets`, `check_wallet_connection` — against a simulated Radix Wallet over a
//! real iroh relay running in this process (plain HTTP on 127.0.0.1).
//!
//! The simulated wallet and the relay are connect-iroh's test support, shared rather than copied:
//! the wallet is built on the same protocol library as the real one, links like it (a linking
//! session that drops anything else, closed before its regular session connects) and signs real
//! ROLA proofs.

use std::rc::Rc;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::iroh::Setting;
use crate::rpc::{App, Options};
use crate::tools;

#[path = "../../connect-iroh/tests/support/mod.rs"]
mod support;

use support::{answers_rola, LinkSignature, LocalRelay, SimWallet, DAPP, ORIGIN};

/// The text of a tool result.
fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The raw QR payload `pair_wallet` prints for local QR generators.
fn qr_payload(pairing: &str) -> String {
    let start = pairing.find("```json\n").expect("the payload block") + "```json\n".len();
    let end = pairing[start..]
        .find("\n```")
        .expect("the end of the payload block")
        + start;
    pairing[start..end].to_string()
}

fn in_local_set(test: impl std::future::Future<Output = ()>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // As in `main`: one thread, a `LocalSet` for the tool calls' local tasks.
    tokio::task::LocalSet::new().block_on(&runtime, test);
}

#[test]
fn a_wallet_paired_over_iroh_signs_through_the_tools() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_iroh_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        // The default transport setting: pairing over iroh is asked for by the call itself.
        let app = Rc::new(App::at(
            dir.join("connector.json"),
            Options {
                transport: Setting::Webrtc,
                iroh_relay: Some(relay.url.clone()),
            },
        ));

        // Pairing: the QR, the phone scanning it, pair_status saving the link.
        let pairing = text(
            &tools::call(
                &app,
                "pair_wallet",
                json!({ "transport": "iroh", "label": "sim" }),
            )
            .await,
        );
        assert!(pairing.contains("transport: iroh"), "{pairing}");
        assert!(
            pairing.contains("Settings > Preferences > Radix Connect"),
            "{pairing}"
        );
        let payload = qr_payload(&pairing);
        let wallet = Arc::new(SimWallet::default());
        let phone = async {
            let password = wallet
                .scan_and_link(&payload, &relay.url, LinkSignature::Valid)
                .await;
            wallet.connect(&password, &relay.url, answers_rola()).await
        };
        let status = tools::call(&app, "pair_status", json!({ "wait_seconds": 60 }));
        let (running, status) = tokio::join!(phone, status);
        let status = text(&status);
        assert!(status.contains("WALLET PAIRED"), "{status}");
        assert!(
            status.contains("Transport:  iroh (relay http://127.0.0.1"),
            "{status}"
        );

        // A signing request (ROLA), straight after: it goes over iroh, not WebRTC.
        let challenge = support::challenge();
        let proof = tools::call(
            &app,
            "request_account_proof",
            json!({
                "network": "stokenet", "dapp_definition": DAPP, "origin": ORIGIN,
                "challenge": challenge, "skip_dapp_check": true, "timeout_seconds": 60,
            }),
        )
        .await;
        let proof_text = text(&proof);
        assert!(proof_text.contains("ACCOUNT PROOF VERIFIED"), "{proof_text}");
        assert!(proof_text.contains(&wallet.account_address()), "{proof_text}");
        assert_eq!(running.received().len(), 1, "sent exactly once");

        // The wallet's transport and live connection are visible.
        let wallets = text(&tools::call(&app, "list_wallets", json!({})).await);
        assert!(wallets.contains("transport:  iroh (relay"), "{wallets}");
        assert!(wallets.contains("state=connected"), "{wallets}");
        let reachable =
            text(&tools::call(&app, "check_wallet_connection", json!({ "timeout_seconds": 20 })).await);
        assert!(reachable.contains("WALLET REACHABLE"), "{reachable}");
        assert!(reachable.contains("Connection: connector "), "{reachable}");

        // The trace follows the request over iroh, and holds no link password.
        let log = std::fs::read_to_string(dir.join("connector.log")).unwrap();
        assert!(log.contains("\"transport\":\"iroh (relay"), "{log}");
        assert!(log.contains("\"event\":\"delivered\""), "{log}");
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("connector.json")).unwrap()).unwrap();
        let password = state["links"][0]["password"].as_str().unwrap().to_string();
        assert!(!log.contains(&password));
        let records = std::fs::read_to_string(dir.join("transports.json")).unwrap();
        assert!(
            records.contains("\"transport\": \"iroh\"") && !records.contains(&password),
            "{records}"
        );

        running.close().await;
        std::fs::remove_dir_all(&dir).ok();
    });
}

#[test]
fn a_wallet_that_never_connects_over_iroh_fails_with_the_links_diagnostics() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_iroh_down_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        // A link "paired" over iroh whose phone never shows up.
        let password = [0x42u8; 32];
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("connector.json"),
            json!({
                "version": 1,
                "identity": { "privateKey": "00".repeat(32), "publicKey": "11".repeat(32) },
                "links": [ { "password": hex::encode(password), "walletPublicKey": "ab".repeat(32), "linkedAt": "0" } ],
            })
            .to_string(),
        )
        .unwrap();
        let app = Rc::new(App::at(
            dir.join("connector.json"),
            Options {
                transport: Setting::Iroh,
                iroh_relay: Some(relay.url.clone()),
            },
        ));

        let result = tools::call(
            &app,
            "request_account_proof",
            json!({
                "network": "stokenet", "dapp_definition": DAPP, "origin": ORIGIN,
                "challenge": support::challenge(), "skip_dapp_check": true, "timeout_seconds": 6,
            }),
        )
        .await;
        assert_eq!(result["isError"], true);
        let failure = &result["structuredContent"];
        assert_eq!(failure["code"], "WALLET_UNREACHABLE", "{result}");
        assert_eq!(failure["retry_safe"], true);
        let detail = failure["detail"].as_str().unwrap();
        assert!(
            detail.contains("diagnostics: connector ") || detail.contains("diagnóstico: connector "),
            "{detail}"
        );
        assert!(detail.contains("state=disconnected"), "{detail}");
        let shown = result.to_string();
        assert!(!shown.contains(&hex::encode(password)), "{shown}");
        std::fs::remove_dir_all(&dir).ok();
    });
}
