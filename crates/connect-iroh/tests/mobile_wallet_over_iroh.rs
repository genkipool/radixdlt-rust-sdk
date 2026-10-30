//! The mobile-wallet connector against a simulated Radix Wallet, over a real iroh relay running
//! in this process (plain HTTP on 127.0.0.1): pairing, requests and answers, a wallet that comes
//! and goes, what is never sent twice, and the timeouts — each with the link's diagnostics.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use radixdlt_connect_iroh::mobile::{
    account_proof_request, extract_login, extract_proofs, linking, ConnectionState, MobileError,
    MobileWalletConnector, PersonaRequest, Progress,
};
use radixdlt_rola::{verify_account_proof, verify_persona_proof, AccountProof};
use serde_json::{json, Value};
use support::*;

fn connector(relay: &LocalRelay) -> MobileWalletConnector {
    MobileWalletConnector::new().with_relay_url(&relay.url).unwrap()
}

fn password() -> Vec<u8> {
    linking::generate_password().unwrap().to_vec()
}

/// The account proof in `answer`, verified against `challenge`; returns the account.
fn verified_account(answer: &Value, challenge: &str) -> String {
    let (address, proof) = extract_proofs(answer).unwrap().into_iter().next().unwrap();
    let proof = AccountProof {
        address: address.clone(),
        public_key_hex: proof["publicKey"].as_str().unwrap().to_string(),
        signature_hex: proof["signature"].as_str().unwrap().to_string(),
    };
    verify_account_proof(&proof, challenge, DAPP, ORIGIN, NETWORK_ID).expect("the proof verifies");
    address
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_links_the_wallet_and_its_regular_session_serves_requests() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let (private, public) = linking::generate_identity().unwrap();
    let wallet = Arc::new(SimWallet::default());
    let (qr_tx, qr_rx) = tokio::sync::oneshot::channel::<String>();

    let pairing = connector.pair(&private, &public, |qr| qr_tx.send(qr).unwrap(), WAIT);
    let scanning = async {
        let qr = qr_rx.await.unwrap();
        let qr_json: Value = serde_json::from_str(&qr).unwrap();
        assert_eq!(qr_json["publicKey"], public);
        let password = wallet.scan_and_link(&qr, &relay.url, LinkSignature::Valid).await;
        // The wallet then reconnects with its regular session, to the same endpoint.
        (
            password.clone(),
            wallet.connect(&password, &relay.url, answers_rola()).await,
        )
    };
    let (paired, (scanned_password, running)) = tokio::join!(pairing, scanning);
    let (wallet_public_key, password) = paired.unwrap();
    assert_eq!(wallet_public_key, wallet.public_key_hex());
    assert_eq!(password, scanned_password);

    // The session started for pairing is the one that serves the wallet now.
    let challenge = challenge();
    let answer = connector
        .request_account_proof(&password, &challenge, &ctx(), false, WAIT)
        .await
        .unwrap();
    assert_eq!(verified_account(&answer, &challenge), wallet.account_address());

    // A login proves the persona too.
    let challenge = support::challenge();
    let answer = connector
        .request_login(&password, &challenge, &ctx(), PersonaRequest::NONE, WAIT)
        .await
        .unwrap();
    let (identity, _label, proof) = extract_login(&answer).unwrap();
    assert_eq!(identity, wallet.identity_address());
    let persona = AccountProof {
        address: identity,
        public_key_hex: proof["proof"]["publicKey"].as_str().unwrap().to_string(),
        signature_hex: proof["proof"]["signature"].as_str().unwrap().to_string(),
    };
    verify_persona_proof(&persona, &challenge, DAPP, ORIGIN, NETWORK_ID).unwrap();

    assert!(connector.is_connected(&password));
    let diagnostics = connector.diagnostics(&password).unwrap();
    assert_eq!(diagnostics.state, ConnectionState::Connected);
    assert!(diagnostics.relay_connected, "{diagnostics}");
    running.close().await;
    connector.close().await;
}

/// Regression (seen with a real phone): a request sent right after `pair` returned went over the
/// wallet's LINKING connection, which takes and drops every message — so it was confirmed, never
/// resent, and never answered. `pair` now returns once that connection has gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_right_after_pairing_goes_over_the_regular_connection() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let (private, public) = linking::generate_identity().unwrap();
    let wallet = Arc::new(SimWallet::default());
    let (qr_tx, qr_rx) = tokio::sync::oneshot::channel::<String>();

    let server = async {
        let (_, password) = connector
            .pair(&private, &public, |qr| qr_tx.send(qr).unwrap(), WAIT)
            .await
            .unwrap();
        // At once, as a server does once the person has linked.
        let challenge = challenge();
        let answer = connector
            .request_account_proof(&password, &challenge, &ctx(), false, Duration::from_secs(20))
            .await;
        (challenge, answer)
    };
    let phone = async {
        let qr = qr_rx.await.unwrap();
        let password = wallet.scan_and_link(&qr, &relay.url, LinkSignature::Valid).await;
        wallet.connect(&password, &relay.url, answers_rola()).await
    };
    let ((challenge, answer), running) = tokio::join!(server, phone);
    let ignored = wallet.ignored_while_linking.lock().unwrap().clone();
    assert!(ignored.is_empty(), "the linking session swallowed {ignored:?}");
    assert_eq!(
        verified_account(&answer.unwrap(), &challenge),
        wallet.account_address()
    );
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_linking_signature_fails_pairing_and_closes_its_session() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let (private, public) = linking::generate_identity().unwrap();
    let wallet = SimWallet::default();
    let (qr_tx, qr_rx) = tokio::sync::oneshot::channel::<String>();

    let pairing = connector.pair(&private, &public, |qr| qr_tx.send(qr).unwrap(), WAIT);
    let scanning = async {
        let qr = qr_rx.await.unwrap();
        wallet.scan_and_link(&qr, &relay.url, LinkSignature::Forged).await
    };
    let (paired, password) = tokio::join!(pairing, scanning);
    assert!(matches!(paired, Err(MobileError::Crypto(_))), "{paired:?}");
    assert!(
        connector.diagnostics(&password).is_none(),
        "a failed pairing must not keep a session for its password"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_nobody_scans_times_out_with_the_links_diagnostics() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let (private, public) = linking::generate_identity().unwrap();
    let steps = Steps::default();
    let mut observer = steps.observer();
    let mut shown = String::new();

    let started = Instant::now();
    let result = connector
        .pair_with_progress(
            &private,
            &public,
            |qr| shown = qr,
            Duration::from_secs(3),
            &mut observer,
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    let error = result.unwrap_err();
    let diagnostics = error
        .diagnostics()
        .expect("a timeout carries diagnostics")
        .clone();
    assert!(
        matches!(error, MobileError::WalletUnreachable { .. }),
        "{error:?}"
    );
    assert_eq!(diagnostics.state, ConnectionState::Disconnected);
    let relay_in_use = diagnostics
        .relay_url
        .as_ref()
        .expect("a custom relay is always known");
    assert!(relay_in_use.as_str().starts_with(&relay.url), "{diagnostics}");
    assert!(!diagnostics.public_relays, "{diagnostics}");

    // The message says what the link looked like, and nothing that opens it.
    let message = error.to_string();
    assert!(message.contains(&diagnostics.to_string()), "{message}");
    let qr: Value = serde_json::from_str(&shown).unwrap();
    for secret in [qr["password"].as_str().unwrap(), private.as_str()] {
        assert!(!message.contains(secret) && !format!("{error:?}").contains(secret));
    }
    // While it waited, it said so, with a snapshot.
    assert!(
        steps.count(|step| matches!(step, Progress::WaitingForWallet { .. })) >= 1,
        "{:?}",
        steps.all()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_its_caller_abandons_leaves_no_session_behind() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let (private, public) = linking::generate_identity().unwrap();
    let shown = std::sync::Mutex::new(None::<String>);
    let pairing = connector.pair(&private, &public, |qr| *shown.lock().unwrap() = Some(qr), WAIT);
    // The caller gives up (a cancelled task, a `select!`) while the QR is on screen.
    assert!(tokio::time::timeout(Duration::from_secs(1), pairing)
        .await
        .is_err());
    let qr: Value = serde_json::from_str(&shown.lock().unwrap().take().unwrap()).unwrap();
    let password = hex::decode(qr["password"].as_str().unwrap()).unwrap();
    assert!(connector.diagnostics(&password).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_on_one_link_each_get_their_own_answer() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    // The first request is answered last: answers come back in the reverse order.
    let order = Arc::new(AtomicUsize::new(0));
    let handler: Handler = Arc::new(move |wallet: &SimWallet, request: &Value| {
        let n = order.fetch_add(1, Ordering::SeqCst) as u64;
        Reply::After(
            Duration::from_millis(1500u64.saturating_sub(n * 400)),
            wallet.rola_answer(request),
        )
    });
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, handler).await;

    // From four tasks sharing one connector: it is meant to be shared by a server's handlers.
    let connector = Arc::new(connector);
    let challenges: Vec<String> = (0..4).map(|_| challenge()).collect();
    let tasks: Vec<_> = challenges
        .iter()
        .map(|challenge| {
            let (connector, password, challenge) = (connector.clone(), password.clone(), challenge.clone());
            tokio::spawn(async move {
                connector
                    .request_account_proof(&password, &challenge, &ctx(), false, WAIT)
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut answers = Vec::new();
    for task in tasks {
        answers.push(task.await.unwrap());
    }
    // Each proof verifies against ITS challenge: an answer handed to the wrong request would not.
    for (answer, challenge) in answers.iter().zip(&challenges) {
        assert_eq!(verified_account(answer, challenge), wallet.account_address());
    }
    assert_eq!(running.received().len(), 4);
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_waits_for_the_wallet_to_open_its_app() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    let steps = Steps::default();
    let mut observer = steps.observer();
    let challenge = challenge();
    let interaction = account_proof_request(&challenge, &ctx(), false);

    let asking = connector.exchange(&password, &interaction, WAIT, &mut observer);
    let opening = async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        wallet.connect(&password, &relay.url, answers_rola()).await
    };
    let (answer, running) = tokio::join!(asking, opening);
    assert_eq!(
        verified_account(&answer.unwrap(), &challenge),
        wallet.account_address()
    );

    let steps = steps.all();
    let position = |wanted: fn(&Progress) -> bool| steps.iter().position(wanted);
    let waiting = position(|s| matches!(s, Progress::WaitingForWallet { .. })).expect("it reported waiting");
    let connected = position(|s| matches!(s, Progress::Connected { .. })).expect("it reported the wallet");
    let delivered = position(|s| matches!(s, Progress::Delivered { .. })).expect("it reported delivery");
    assert!(waiting < connected && connected < delivered, "{steps:?}");
    assert_eq!(running.count(interaction["interactionId"].as_str().unwrap()), 1);
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_that_reconnects_mid_flight_still_answers_and_nothing_is_sent_twice() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let first = wallet.connect(&password, &relay.url, never_answers()).await;
    let steps = Steps::default();
    let mut observer = steps.observer();
    let challenge = challenge();
    let interaction = account_proof_request(&challenge, &ctx(), false);
    let id = interaction["interactionId"].as_str().unwrap().to_string();

    let asking = connector.exchange(&password, &interaction, WAIT, &mut observer);
    let restarting = async {
        let request = first.wait_for_requests(1).await.remove(0);
        // The app is killed with the request on screen, and opened again a moment later.
        first.close().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let second = wallet.connect(&password, &relay.url, never_answers()).await;
        // The person approves: the answer goes out on the new connection.
        second.send(&wallet.rola_answer(&request)).await;
        second
    };
    let (answer, second) = tokio::join!(asking, restarting);
    assert_eq!(
        verified_account(&answer.unwrap(), &challenge),
        wallet.account_address()
    );

    assert_eq!(second.count(&id), 0, "a delivered request is never sent again");
    let steps = steps.all();
    assert_eq!(
        steps
            .iter()
            .filter(|s| matches!(s, Progress::Delivered { .. }))
            .count(),
        1
    );
    assert!(
        steps
            .iter()
            .any(|s| matches!(s, Progress::WalletDisconnected { .. })),
        "{steps:?}"
    );
    assert!(
        !steps.iter().any(|s| matches!(s, Progress::Resending { .. })),
        "{steps:?}"
    );
    second.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_the_wallet_never_confirms_is_not_sent_again() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay).with_ack_timeout(Duration::from_secs(1));
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, answers_rola()).await;
    // Let the account list through, then the wallet stops taking messages.
    tokio::time::sleep(Duration::from_millis(500)).await;
    running.set_reading(false);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let steps = Steps::default();
    let mut observer = steps.observer();
    let interaction = account_proof_request(&challenge(), &ctx(), false);
    let id = interaction["interactionId"].as_str().unwrap().to_string();
    let result = connector
        .exchange(&password, &interaction, Duration::from_secs(4), &mut observer)
        .await;
    let error = result.unwrap_err();
    assert!(
        matches!(error, MobileError::ConfirmationTimeout { .. }),
        "{error:?}"
    );
    assert!(error
        .to_string()
        .contains(&error.diagnostics().unwrap().to_string()));

    let steps = steps.all();
    assert_eq!(
        steps
            .iter()
            .filter(|s| matches!(s, Progress::Unconfirmed { .. }))
            .count(),
        1,
        "{steps:?}"
    );
    assert!(
        !steps.iter().any(|s| matches!(s, Progress::Resending { .. })),
        "{steps:?}"
    );

    // The wallet takes messages again: the abandoned request was withdrawn, and nothing resent.
    running.set_reading(true);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(running.count(&id), 0, "{:?}", running.received());
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_request_times_out_and_its_late_answer_is_kept() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, never_answers()).await;
    let challenge = challenge();
    let interaction = account_proof_request(&challenge, &ctx(), false);
    let id = interaction["interactionId"].as_str().unwrap().to_string();

    let error = connector
        .exchange(&password, &interaction, Duration::from_secs(2), &mut |_| {})
        .await
        .unwrap_err();
    let MobileError::ResponseTimeout { diagnostics } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(diagnostics.state, ConnectionState::Connected, "{diagnostics}");
    assert!(error.to_string().contains("state=connected"), "{error}");

    // The person approves after the request timed out here.
    let request = running.wait_for_requests(1).await.remove(0);
    running.send(&wallet.rola_answer(&request)).await;
    let answer = connector
        .await_response(&password, &id, WAIT, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(verified_account(&answer, &challenge), wallet.account_address());
    assert_eq!(running.count(&id), 1);
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_wallet_a_request_is_unreachable_and_says_why() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let started = Instant::now();
    // Long enough for this end to reach the local relay (about 3 s: iroh's first network report
    // waits on QUIC probes that a plain-HTTP test relay does not answer).
    let error = connector
        .request_account_proof(&password, &challenge(), &ctx(), false, Duration::from_secs(8))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );
    let MobileError::WalletUnreachable { diagnostics } = &error else {
        panic!("{error:?}");
    };
    // Our end is fine (it reached its relay): the phone is the one missing.
    assert!(diagnostics.relay_connected, "{diagnostics}");
    assert_eq!(diagnostics.state, ConnectionState::Disconnected);
    let message = error.to_string();
    assert!(
        message.contains("(connected)") && message.contains("state=disconnected"),
        "{message}"
    );
    assert!(
        !message.contains(&hex::encode(&password)) && !format!("{error:?}").contains(&hex::encode(&password))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejection_is_reported_as_the_persons() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    let handler: Handler = Arc::new(|_: &SimWallet, request: &Value| {
        Reply::Now(json!({
            "discriminator": "failure",
            "interactionId": request["interactionId"],
            "error": "rejectedByUser",
        }))
    });
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, handler).await;
    let error = connector
        .request_transaction(&password, "CALL_METHOD ...;", "", &[], &ctx(), WAIT)
        .await
        .unwrap_err();
    assert!(error.rejected_by_person(), "{error:?}");
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_account_list_reaches_subscribers_and_never_a_request() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let mut messages = connector.subscribe(&password).await.unwrap();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, answers_rola()).await;

    let first = tokio::time::timeout(WAIT, messages.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["discriminator"], "accountList");
    assert_eq!(first["accounts"][0]["address"], wallet.account_address());

    // A request in flight is not confused by it either way.
    let challenge = challenge();
    let answer = connector
        .request_account_proof(&password, &challenge, &ctx(), false, WAIT)
        .await
        .unwrap();
    verified_account(&answer, &challenge);
    running.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_link_ends_its_calls_and_the_next_call_starts_again() {
    let relay = LocalRelay::start().await;
    let connector = connector(&relay);
    let password = password();
    let wallet = Arc::new(SimWallet::default());
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, never_answers()).await;

    let (pending_challenge, context) = (challenge(), ctx());
    let asking = connector.request_account_proof(&password, &pending_challenge, &context, false, WAIT);
    let closing = async {
        running.wait_for_requests(1).await;
        connector.close_link(&password).await;
    };
    let (result, ()) = tokio::join!(asking, closing);
    assert_eq!(result.unwrap_err(), MobileError::Closed);
    assert!(connector.diagnostics(&password).is_none());
    running.close().await;

    // A new call starts a new session, and the wallet (which re-dials) is served again.
    connector.open_link(&password).await.unwrap();
    let running = wallet.connect(&password, &relay.url, answers_rola()).await;
    let challenge = challenge();
    let answer = connector
        .request_account_proof(&password, &challenge, &ctx(), false, WAIT)
        .await
        .unwrap();
    verified_account(&answer, &challenge);
    running.close().await;
}

/// The default — n0's public relays — end to end: neither end is given a relay URL; this end
/// publishes the one it uses through n0's address lookup and the wallet finds it there.
///
/// Opt-in, because it needs the internet and n0's live infrastructure:
/// `cargo test --test mobile_wallet_over_iroh -- --ignored public`. (The protocol library tests
/// this path offline with a local pkarr/DNS server, through a hook only its own tests can reach.)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the internet: n0's public relays and address lookup"]
async fn with_public_relays_the_wallet_finds_the_server_through_address_lookup() {
    let connector = MobileWalletConnector::new();
    let password = password();
    connector.open_link(&password).await.unwrap();
    let wallet = Arc::new(SimWallet::default());
    let running = tokio::time::timeout(
        Duration::from_secs(90),
        wallet.connect_with(
            &password,
            radix_connect_iroh::SessionConfig::public(),
            answers_rola(),
        ),
    )
    .await
    .expect("the wallet finds the server through address lookup");

    let challenge = challenge();
    let answer = connector
        .request_account_proof(&password, &challenge, &ctx(), false, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(verified_account(&answer, &challenge), wallet.account_address());
    let diagnostics = connector.diagnostics(&password).unwrap();
    assert!(diagnostics.public_relays, "{diagnostics}");
    assert!(diagnostics.relay_url.is_some(), "{diagnostics}");
    println!("{diagnostics}");
    running.close().await;
    connector.close().await;
}
