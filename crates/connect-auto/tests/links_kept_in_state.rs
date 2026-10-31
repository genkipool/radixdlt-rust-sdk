//! A connector given the program's state file looks after its links with nothing else to program:
//! a link removed here is told to its wallet (at once, or when the wallet is next there), one the
//! wallet removed is forgotten, and the relay a wallet is on is noted for the next knock. Against
//! a simulated Radix Wallet over a real iroh relay in this process, with a real `connector.json`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "../../connect-iroh/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use radixdlt_connect_auto::{
    account_proof_request, linking, Error, Identity, Link, LinkState, MobileWalletConnector, Removal,
    Transport, WalletConnector, WebrtcConnector,
};
use serde_json::json;
use support::*;

const WALLET: &str = "abababababababababababababababababababababababababababababababab";

/// A state file with one link over iroh, in a directory of its own.
fn state_with_link(password: &[u8]) -> PathBuf {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let dir = std::env::temp_dir().join(format!("radixdlt-connect-auto-state-{}", hex::encode(bytes)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("connector.json");
    let state = LinkState {
        version: 1,
        identity: Identity {
            private_key: "11".repeat(32),
            public_key: "22".repeat(32),
        },
        link: None,
        links: vec![Link {
            password: hex::encode(password),
            wallet_public_key: WALLET.into(),
            linked_at: String::new(),
            label: None,
            transport: Some(Transport::Iroh),
            iroh_relay: None,
            wallet_relay: None,
        }],
        retired_links: Vec::new(),
    };
    state.save(path.to_str().unwrap()).unwrap();
    path
}

fn read(path: &Path) -> LinkState {
    LinkState::load(path.to_str().unwrap()).unwrap()
}

async fn connector(relay: &LocalRelay, state: &Path) -> WalletConnector {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    WalletConnector::new()
        .with_webrtc(
            WebrtcConnector::new()
                .with_ice_servers(Vec::new())
                .with_signaling_base(format!("ws://{}", listener.local_addr().unwrap())),
        )
        .with_iroh(
            MobileWalletConnector::new()
                .with_relay_url(&relay.url)
                .unwrap()
                .with_link_lock_dir(state.parent().unwrap().join("links")),
        )
        .with_state(state)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_removed_here_is_told_at_once_to_a_wallet_that_is_there() {
    let relay = LocalRelay::start().await;
    let password = linking::generate_password().unwrap().to_vec();
    let state = state_with_link(&password);
    let wallet_connector = connector(&relay, &state).await;
    let link = read(&state).links[0].clone();
    wallet_connector.keep_listening(&link).await.unwrap();
    let wallet = Arc::new(SimWallet::default());
    let running = wallet.connect(&password, &relay.url, never_answers()).await;
    running.wait_connected().await;

    let removal = wallet_connector
        .remove_link(WALLET, Duration::from_secs(10))
        .await
        .unwrap();

    assert_eq!(removal, Removal::Told);
    let after = read(&state);
    assert!(
        after.links.is_empty() && after.retired_links.is_empty(),
        "nothing left of it"
    );
    assert!(running
        .wait_for_requests(1)
        .await
        .contains(&json!({ "discriminator": "linkRemoved" })));
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_removed_while_its_wallet_is_away_is_told_when_it_is_back() {
    let relay = LocalRelay::start().await;
    let password = linking::generate_password().unwrap().to_vec();
    let state = state_with_link(&password);
    let wallet_connector = connector(&relay, &state).await;

    let removal = wallet_connector
        .remove_link(WALLET, Duration::from_millis(500))
        .await
        .unwrap();
    assert_eq!(removal, Removal::Pending);
    assert_eq!(read(&state).retired_links.len(), 1, "the notice is kept");

    // The program goes on; the wallet's app opens later, and the notice reaches it.
    let delivering = wallet_connector.clone();
    let delivery = tokio::spawn(async move { delivering.deliver_removal_notices().await });
    let wallet = Arc::new(SimWallet::default());
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Told, the connector closes the connection at once: the wallet is started, not waited for.
    let running = wallet.start(&password, &relay.url, never_answers()).await;
    assert!(running
        .wait_for_requests(1)
        .await
        .contains(&json!({ "discriminator": "linkRemoved" })));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !read(&state).retired_links.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the delivered notice is forgotten");
    delivery.abort();
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_the_wallet_removed_is_forgotten_here_and_a_wallets_relay_is_noted() {
    let relay = LocalRelay::start().await;
    let password = linking::generate_password().unwrap().to_vec();
    let state = state_with_link(&password);
    let wallet_connector = connector(&relay, &state).await;
    let link = read(&state).links[0].clone();
    wallet_connector.keep_listening(&link).await.unwrap();
    let wallet = Arc::new(SimWallet::default());

    // A request answered over iroh: the relay its wallet is on is noted.
    let running = wallet.connect(&password, &relay.url, answers_rola()).await;
    running.wait_connected().await;
    wallet_connector
        .exchange(&link, &account_proof_request(&challenge(), &ctx(), false), WAIT)
        .await
        .unwrap();
    assert!(
        read(&state).links[0].wallet_relay.is_some(),
        "{:?}",
        read(&state).links[0]
    );
    running.close().await;

    // The wallet removes the link: the next request says so, and the link is gone here.
    let removed: Handler = Arc::new(|_: &SimWallet, _: &serde_json::Value| {
        Reply::Now(json!({ "discriminator": "linkRemoved" }))
    });
    let running = wallet.connect(&password, &relay.url, removed).await;
    running.wait_connected().await;
    let error = wallet_connector
        .exchange(&link, &account_proof_request(&challenge(), &ctx(), false), WAIT)
        .await
        .unwrap_err();
    assert_eq!(error, Error::LinkRemoved);
    assert!(read(&state).links.is_empty(), "forgotten here");
    assert!(
        read(&state).retired_links.is_empty(),
        "and nothing to tell: the wallet knows"
    );
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_round_of_notices_tells_the_wallets_that_are_there_and_keeps_the_rest() {
    let relay = LocalRelay::start().await;
    let password = linking::generate_password().unwrap().to_vec();
    let state = state_with_link(&password);
    let wallet_connector = connector(&relay, &state).await;
    assert_eq!(
        wallet_connector
            .remove_link(WALLET, Duration::from_millis(300))
            .await
            .unwrap(),
        Removal::Pending
    );

    let away = wallet_connector
        .deliver_pending_notices(Duration::from_millis(500))
        .await
        .unwrap();
    assert_eq!((away.told, away.pending, away.expired), (0, 1, 0));

    let wallet = Arc::new(SimWallet::default());
    let (round, running) = tokio::join!(
        wallet_connector.deliver_pending_notices(WAIT),
        wallet.start(&password, &relay.url, never_answers()),
    );
    let round = round.unwrap();
    assert_eq!((round.told, round.pending), (1, 0), "{round:?}");
    assert!(read(&state).retired_links.is_empty());
    running.close().await;
}
