//! A connector process asked to stop (SIGTERM) closes its sessions first, so its wallet is told
//! at once instead of believing it is connected for 30 s. Its own test binary: the signal goes to
//! this whole process.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../connect-iroh/tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use radixdlt_connect_auto::{
    linking, Link, MobileWalletConnector, Transport, WalletConnector, WebrtcConnector,
};
use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_closes_every_session_and_tells_the_wallet_at_once() {
    let relay = LocalRelay::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wallet_connector = WalletConnector::new()
        .with_webrtc(
            WebrtcConnector::new()
                .with_ice_servers(Vec::new())
                .with_signaling_base(format!("ws://{}", listener.local_addr().unwrap())),
        )
        .with_iroh(MobileWalletConnector::new().with_relay_url(&relay.url).unwrap());
    let password = linking::generate_password().unwrap().to_vec();
    let link = Link {
        password: hex::encode(&password),
        wallet_public_key: "ab".repeat(32),
        linked_at: String::new(),
        label: None,
        transport: Some(Transport::Iroh),
        iroh_relay: None,
        wallet_relay: None,
    };
    wallet_connector.keep_listening(&link).await.unwrap();
    let wallet = Arc::new(SimWallet::default());
    let running = wallet.connect(&password, &relay.url, answers_rola()).await;
    running.wait_connected().await;

    // The program's main loop, which never ends by itself.
    let serving = {
        let wallet_connector = wallet_connector.clone();
        tokio::spawn(async move {
            wallet_connector
                .run_until_stopped(std::future::pending::<()>())
                .await
        })
    };
    // Once it listens for the signal, the process is asked to stop.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped_at = Instant::now();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let outcome = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("it stopped")
        .unwrap();
    assert_eq!(outcome, None, "stopped by the signal, not by its work");
    tokio::time::timeout(Duration::from_secs(5), async {
        while running.session.state() == radix_connect_iroh::ConnectionState::Connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the wallet was told at once");
    assert!(
        stopped_at.elapsed() < Duration::from_secs(5),
        "{:?}",
        stopped_at.elapsed()
    );
    running.close().await;
}
