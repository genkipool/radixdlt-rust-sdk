//! A request for several wallets of one person goes to the one the person opens first, and to
//! no other; the persona it answers as is kept with its link, so a request naming that persona
//! later goes to the wallets that have it. Against simulated Radix Wallets over a real iroh relay
//! in this process, with a real `connector.json`.

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
    linking, Identity, Link, LinkState, MobileWalletConnector, Transport, WalletConnector, WebrtcConnector,
};
use serde_json::{json, Value};
use support::*;

const PERSONA: &str = "identity_tdx_2_12xpersonaofthepersonwhoopenedthisphone000000000000000000";

/// A state file with one iroh link per password, in a directory of its own.
fn state_with_links(passwords: &[&[u8]]) -> PathBuf {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let dir = std::env::temp_dir().join(format!("radixdlt-connect-auto-first-{}", hex::encode(bytes)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("connector.json");
    let state = LinkState {
        version: 1,
        identity: Identity {
            private_key: "11".repeat(32),
            public_key: "22".repeat(32),
        },
        link: None,
        links: passwords
            .iter()
            .enumerate()
            .map(|(index, password)| Link {
                password: hex::encode(password),
                wallet_public_key: format!("{index:0>64}"),
                linked_at: String::new(),
                label: None,
                transport: Some(Transport::Iroh),
                iroh_relay: None,
                wallet_relay: None,
                personas: Vec::new(),
            })
            .collect(),
        retired_links: Vec::new(),
    };
    state.save(path.to_str().unwrap()).unwrap();
    path
}

fn read(path: &Path) -> LinkState {
    LinkState::load(path.to_str().unwrap()).unwrap()
}

async fn connector(relay: &LocalRelay, state: &Path) -> WalletConnector {
    // A signaling server nobody listens on: WebRTC fails at once, iroh carries everything.
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

/// A wallet that logs in at once, as `PERSONA`.
fn logs_in() -> Handler {
    Arc::new(|_: &SimWallet, request: &Value| {
        Reply::Now(json!({
            "discriminator": "success",
            "interactionId": request["interactionId"],
            "items": {
                "discriminator": "authorizedRequest",
                "auth": {
                    "discriminator": "loginWithoutChallenge",
                    "persona": { "identityAddress": PERSONA, "label": "Me" },
                },
            },
        }))
    })
}

fn login(id: &str) -> Value {
    json!({
        "interactionId": id,
        "items": { "discriminator": "authorizedRequest", "auth": { "discriminator": "loginWithoutChallenge" } },
        "metadata": { "version": 2, "networkId": 2, "origin": "https://pam.example", "dAppDefinitionAddress": "account_tdx_2_1x" },
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_goes_to_the_wallet_opened_first_and_to_no_other() {
    let relay = LocalRelay::start().await;
    let first = linking::generate_password().unwrap().to_vec();
    let second = linking::generate_password().unwrap().to_vec();
    let state = state_with_links(&[&first, &second]);
    let wallet_connector = connector(&relay, &state).await;
    let links = read(&state).links;

    // The person opens the SECOND phone; the first one a moment after.
    let opened = Arc::new(SimWallet::default());
    let other = Arc::new(SimWallet::default());
    let (relay_url, other_password) = (relay.url.clone(), first.clone());
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        other.start(&other_password, &relay_url, logs_in()).await
    });
    let running = opened.start(&second, &relay.url, logs_in()).await;

    let (chosen, answer) = wallet_connector
        .exchange_first(
            &links,
            &login("first-opened"),
            Duration::from_secs(20),
            &mut |_, _| {},
        )
        .await
        .unwrap();

    assert_eq!(chosen, 1, "the wallet opened first");
    assert_eq!(answer["items"]["auth"]["persona"]["identityAddress"], PERSONA);
    let late = late.await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        late.received().is_empty(),
        "the other wallet is never sent it: {:?}",
        late.received()
    );

    // The persona it answered as is kept with its link, and only there.
    assert_eq!(read(&state).links[1].personas, vec![PERSONA.to_string()]);
    assert!(read(&state).links[0].personas.is_empty());
    let with = wallet_connector.links_with_persona(PERSONA);
    assert_eq!(with.len(), 1);
    assert_eq!(with[0].wallet_public_key, links[1].wallet_public_key);
    assert!(wallet_connector
        .links_with_persona("identity_tdx_2_1nobody")
        .is_empty());

    running.close().await;
    late.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_wallet_opened_sends_nothing_and_says_so_in_time() {
    let relay = LocalRelay::start().await;
    let first = linking::generate_password().unwrap().to_vec();
    let second = linking::generate_password().unwrap().to_vec();
    let state = state_with_links(&[&first, &second]);
    let wallet_connector = connector(&relay, &state).await;
    let links = read(&state).links;

    let started = std::time::Instant::now();
    let result = wallet_connector
        .exchange_first(
            &links,
            &login("nobody-opened"),
            Duration::from_secs(3),
            &mut |_, _| {},
        )
        .await;

    assert!(result.is_err(), "nobody answered: {result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "within its time: {:?}",
        started.elapsed()
    );
}
