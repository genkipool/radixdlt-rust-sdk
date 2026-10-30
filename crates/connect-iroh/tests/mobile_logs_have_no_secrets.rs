//! What the mobile connector logs, at the most verbose level, through a pairing, a login and a
//! timeout: enough to follow what happened — and nothing that opens the link or tells what the
//! wallet said.
//!
//! A file of its own: it installs the process-wide `tracing` subscriber.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use radixdlt_connect_iroh::mobile::{linking, MobileError, MobileWalletConnector, PersonaRequest};
use serde_json::Value;
use support::*;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_logs_follow_a_link_without_its_secrets_or_its_messages() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "radixdlt_connect_iroh=trace,radix_connect_iroh=trace",
        ))
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let relay = LocalRelay::start().await;
    let connector = MobileWalletConnector::new().with_relay_url(&relay.url).unwrap();
    let (private, public) = linking::generate_identity().unwrap();
    let wallet = Arc::new(SimWallet::default());
    let (qr_tx, qr_rx) = tokio::sync::oneshot::channel::<String>();

    // Pairing, then the wallet's regular session.
    let pairing = connector.pair(&private, &public, |qr| qr_tx.send(qr).unwrap(), WAIT);
    let scanning = async {
        let qr = qr_rx.await.unwrap();
        let password = wallet.scan_and_link(&qr, &relay.url, LinkSignature::Valid).await;
        (qr, wallet.connect(&password, &relay.url, answers_rola()).await)
    };
    let (paired, (qr, running)) = tokio::join!(pairing, scanning);
    let (_, password) = paired.unwrap();

    // A login, then a request nobody answers.
    let challenge = challenge();
    let answer = connector
        .request_login(&password, &challenge, &ctx(), PersonaRequest::NONE, WAIT)
        .await
        .unwrap();
    running.close().await;
    let unanswered = connector
        .request_account_proof(&password, &challenge, &ctx(), false, Duration::from_secs(3))
        .await
        .unwrap_err();
    assert!(
        matches!(
            unanswered,
            MobileError::WalletUnreachable { .. } | MobileError::ConfirmationTimeout { .. }
        ),
        "{unanswered:?}"
    );
    connector.close().await;

    let logs = captured.text();
    // It can be followed: the link (by its hash), the pairing, the wallet, the timeout.
    let link = hex::encode(&linking::connection_id(&password)[..5]);
    for expected in [
        link.as_str(),
        "pairing: received the wallet's linkClient",
        "the wallet connected over iroh",
        "the wallet answered",
        "relay=",
        // Durations in ms, and the limit beside the timeout.
        "first=true",
        "since_start_ms=",
        "start_ms=",
        "elapsed_ms=",
        "limit_ms=3000",
        "sending the request to the wallet",
    ] {
        assert!(
            logs.contains(expected),
            "the logs should mention {expected:?}:\n{logs}"
        );
    }
    // And it does not hold anything that opens the link, nor what the wallet said.
    let qr: Value = serde_json::from_str(&qr).unwrap();
    let mut secrets = vec![
        ("the link password", hex::encode(&password)),
        ("the connector's private key", private.clone()),
        (
            "the QR's signature",
            qr["signature"].as_str().unwrap().to_string(),
        ),
        ("the challenge", challenge.clone()),
        ("the account address", wallet.account_address()),
        ("the persona address", wallet.identity_address()),
    ];
    if let Some(proof) = answer["items"]["auth"]["proof"]["signature"].as_str() {
        secrets.push(("the persona's signature", proof.to_string()));
    }
    for (what, secret) in secrets {
        assert!(!logs.contains(&secret), "{what} appears in the logs");
    }
}
