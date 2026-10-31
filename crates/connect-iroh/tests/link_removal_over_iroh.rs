//! Removing a link over iroh, both ways, against a simulated Radix Wallet over a real iroh relay
//! running in this process: the wallet's `linkRemoved` ends what was asked over the link and every
//! later call, and the connector's own notice reaches the wallet, confirmed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use radixdlt_connect_iroh::mobile::{linking, MobileError, MobileWalletConnector};
use serde_json::json;
use support::*;

fn connector(relay: &LocalRelay) -> MobileWalletConnector {
    MobileWalletConnector::new().with_relay_url(&relay.url).unwrap()
}

fn password() -> Vec<u8> {
    linking::generate_password().unwrap().to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wallets_removal_ends_the_request_in_flight_and_every_later_call() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, never_answers()).await;
    running.wait_connected().await;

    let (challenge, ctx) = (challenge(), ctx());
    let asking = connector.request_account_proof(&password, &challenge, &ctx, false, WAIT);
    let removing = async {
        running.wait_for_requests(1).await;
        running.send(&json!({ "discriminator": "linkRemoved" })).await;
    };
    let (asked, ()) = tokio::join!(asking, removing);

    assert_eq!(asked.unwrap_err(), MobileError::LinkRemoved);
    assert!(connector.removed_by_wallet(&password));
    // Nothing more is sent over it: the next call fails at once.
    let again = connector
        .request_account_proof(&password, &challenge, &ctx, false, WAIT)
        .await;
    assert_eq!(again.unwrap_err(), MobileError::LinkRemoved);
    assert_eq!(running.received().len(), 1);
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_connectors_notice_reaches_the_wallet_confirmed() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, never_answers()).await;

    connector.notify_link_removed(&password, WAIT).await.unwrap();

    let received = running.wait_for_requests(1).await;
    assert_eq!(received, vec![json!({ "discriminator": "linkRemoved" })]);
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_waits_for_the_wallet_and_says_so_when_it_never_came() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);

    let error = connector
        .notify_link_removed(&password(), Duration::from_secs(2))
        .await
        .unwrap_err();

    assert!(
        matches!(error, MobileError::WalletUnreachable { .. }),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_the_wallet_did_not_read_yet_is_sent_again() {
    let relay = LocalRelay::start().await;
    // A short wait for each confirmation, so the unread notice is given up quickly.
    let connector = connector(&relay).with_ack_timeout(Duration::from_secs(1));
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, never_answers()).await;
    running.wait_connected().await;
    // As a wallet app just opened: connected, not reading yet.
    running.set_reading(false);

    let reading_later = async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        running.set_reading(true);
    };
    let (told, ()) = tokio::join!(connector.notify_link_removed(&password, WAIT), reading_later);

    told.unwrap();
    assert!(running
        .received()
        .contains(&json!({ "discriminator": "linkRemoved" })));
    running.close().await;
}
