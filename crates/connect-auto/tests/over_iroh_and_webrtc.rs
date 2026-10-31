//! `WalletConnector` against a simulated Radix Wallet over a real iroh relay running in this
//! process, with WebRTC's signaling server stood in for by a socket on 127.0.0.1 — one nobody
//! listens on, or one that takes connections and never answers — so nothing leaves this machine.
//!
//! What a real WebRTC wallet does is covered by `radixdlt-connect-webrtc` and by testing against
//! the Radix Wallet itself; here WebRTC is the transport that loses the race or fails.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "../../connect-iroh/tests/support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use radixdlt_connect_auto::{
    account_proof_request, linking, Error, Event, Link, MobileWalletConnector, RetiredLink, Transport,
    WalletConnector, WebrtcConnector,
};
use serde_json::json;
use support::*;
use tokio::io::AsyncReadExt;

/// A signaling server address nothing listens on: WebRTC fails at once.
async fn no_signaling() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("ws://{}", listener.local_addr().unwrap())
}

/// A signaling server that takes connections and never answers: WebRTC waits on it, as on the
/// real one for a wallet whose app is closed.
async fn silent_signaling() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                while matches!(stream.read(&mut buffer).await, Ok(n) if n > 0) {}
            });
        }
    });
    url
}

fn connector(relay: &LocalRelay, signaling: &str) -> WalletConnector {
    WalletConnector::new()
        .with_webrtc(
            WebrtcConnector::new()
                .with_ice_servers(Vec::new())
                .with_signaling_base(signaling.to_string()),
        )
        .with_iroh(MobileWalletConnector::new().with_relay_url(&relay.url).unwrap())
}

fn link_over(transport: Transport, password: &[u8]) -> Link {
    Link {
        password: hex::encode(password),
        wallet_public_key: "ab".repeat(32),
        linked_at: String::new(),
        label: None,
        transport: Some(transport),
        iroh_relay: None,
        wallet_relay: None,
    }
}

/// A ROLA account-proof request over `challenge`, which the simulated wallet answers.
fn ask(challenge: &str) -> serde_json::Value {
    account_proof_request(challenge, &ctx(), false)
}

fn password() -> Vec<u8> {
    linking::generate_password().unwrap().to_vec()
}

fn identity() -> (String, String) {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    (hex::encode(seed), hex::encode(key.verifying_key().to_bytes()))
}

/// Records the events of a call.
fn recorder() -> (Arc<Mutex<Vec<Event>>>, impl FnMut(Event) + Send) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    (events, move |event| sink.lock().unwrap().push(event))
}

fn carriers(events: &Mutex<Vec<Event>>) -> Vec<Transport> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Event::Carrier(t) => Some(*t),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_qr_pairs_over_iroh_while_webrtc_still_waits() {
    let relay = LocalRelay::start().await;
    let wallet_connector = connector(&relay, &silent_signaling().await);
    let (private_key, public_key) = identity();
    let (qr_tx, qr_rx) = tokio::sync::oneshot::channel();
    let wallet = Arc::new(SimWallet::default());

    let pairing = wallet_connector.pair(&private_key, &public_key, |qr| drop(qr_tx.send(qr)), WAIT);
    let scanning = async {
        let qr = qr_rx.await.unwrap();
        wallet.scan_and_link(&qr, &relay.url, LinkSignature::Valid).await
    };
    let (paired, scanned_password) = tokio::join!(pairing, scanning);

    let (password, paired) = paired.unwrap();
    assert_eq!(password, scanned_password);
    assert_eq!(paired.transport, Transport::Iroh, "iroh preferred, and it linked");
    assert_eq!(paired.wallet_public_key, wallet.public_key_hex());
    let link = paired.into_link(&password, Some("phone".into()), "now".into());
    assert_eq!(link.transport, Some(Transport::Iroh));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_goes_over_iroh_at_once_when_its_wallet_is_connected() {
    let relay = LocalRelay::start().await;
    let wallet_connector = connector(&relay, &silent_signaling().await);
    let password = password();
    let link = link_over(Transport::Iroh, &password);
    wallet_connector.keep_listening(&link).await.unwrap();
    let wallet = Arc::new(SimWallet::default());
    let running = wallet.connect(&password, &relay.url, answers_rola()).await;
    running.wait_connected().await;
    // The wallet's side knows first; this side, a moment later.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !wallet_connector.connected_over_iroh(&link) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connected over iroh on this side too");

    let (events, mut on) = recorder();
    let started = Instant::now();
    let interaction = ask(&challenge());
    let answer = wallet_connector
        .exchange_with(&link, &interaction, WAIT, &mut on)
        .await
        .unwrap();

    assert_eq!(answer["interactionId"], interaction["interactionId"]);
    assert_eq!(carriers(&events), vec![Transport::Iroh]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "no race, no wait: {:?}",
        started.elapsed()
    );
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_that_connects_over_iroh_mid_race_gets_it_over_iroh_and_only_there() {
    let relay = LocalRelay::start().await;
    // WebRTC waits for ever on its signaling server, as for a wallet whose app is closed.
    let wallet_connector = connector(&relay, &silent_signaling().await);
    let password = password();
    let link = link_over(Transport::Iroh, &password);
    let wallet = Arc::new(SimWallet::default());

    let (events, mut on) = recorder();
    let challenge = challenge();
    let asking = async {
        wallet_connector
            .exchange_with(&link, &ask(&challenge), WAIT, &mut on)
            .await
    };
    let opening = async {
        // The person opens the app a moment later.
        tokio::time::sleep(Duration::from_millis(800)).await;
        wallet.connect(&password, &relay.url, answers_rola()).await
    };
    let (answer, running) = tokio::join!(asking, opening);

    answer.unwrap();
    assert_eq!(carriers(&events), vec![Transport::Iroh]);
    assert_eq!(running.received().len(), 1, "sent once");
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_reachable_says_why_over_each_transport_and_never_waits_past_the_timeout() {
    let relay = LocalRelay::start().await;
    let wallet_connector = connector(&relay, &no_signaling().await);
    let link = link_over(Transport::Iroh, &password());

    let started = Instant::now();
    let error = wallet_connector
        .exchange(&link, &ask(&challenge()), Duration::from_secs(3))
        .await
        .unwrap_err();

    match &error {
        Error::WalletUnreachable { reasons } => {
            let over: Vec<Transport> = reasons.iter().map(|(t, _)| *t).collect();
            assert!(
                over.contains(&Transport::Iroh) && over.contains(&Transport::Webrtc),
                "{error}"
            );
        }
        other => panic!("expected WalletUnreachable, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_paired_over_webrtc_uses_webrtc_alone() {
    let relay = LocalRelay::start().await;
    let wallet_connector = connector(&relay, &no_signaling().await);
    let link = link_over(Transport::Webrtc, &password());
    let (events, mut on) = recorder();

    let error = wallet_connector
        .exchange_with(&link, &ask(&challenge()), Duration::from_secs(3), &mut on)
        .await
        .unwrap_err();

    assert!(matches!(error, Error::NoNetwork { .. }), "{error:?}");
    assert_eq!(
        carriers(&events),
        vec![Transport::Webrtc],
        "its wallet does not speak iroh"
    );
    assert!(!wallet_connector.connected_over_iroh(&link));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_that_removed_the_link_ends_the_request_at_once() {
    let relay = LocalRelay::start().await;
    let wallet_connector = connector(&relay, &silent_signaling().await);
    let password = password();
    let link = link_over(Transport::Iroh, &password);
    wallet_connector.keep_listening(&link).await.unwrap();
    let wallet = Arc::new(SimWallet::default());
    let removed: Handler = Arc::new(|_: &SimWallet, _: &serde_json::Value| {
        Reply::Now(json!({ "discriminator": "linkRemoved" }))
    });
    let running = wallet.connect(&password, &relay.url, removed).await;
    running.wait_connected().await;

    let error = wallet_connector
        .exchange(&link, &ask(&challenge()), WAIT)
        .await
        .unwrap_err();

    assert_eq!(error, Error::LinkRemoved);
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_link_is_told_over_iroh_while_webrtc_cannot() {
    let relay = LocalRelay::start().await;
    let wallet_connector = connector(&relay, &no_signaling().await);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    let notice = RetiredLink {
        password: hex::encode(&password),
        wallet_public_key: "ab".repeat(32),
        label: None,
        transport: Some(Transport::Iroh),
        iroh_relay: None,
        wallet_relay: None,
        retired_at: 0,
    };

    // The notice listens; the wallet connects once it does.
    let (told, running) = tokio::join!(
        wallet_connector.notify_link_removed(&notice, WAIT),
        wallet.connect(&password, &relay.url, never_answers()),
    );
    told.unwrap();

    assert!(running
        .wait_for_requests(1)
        .await
        .contains(&json!({ "discriminator": "linkRemoved" })));
    running.close().await;
}
