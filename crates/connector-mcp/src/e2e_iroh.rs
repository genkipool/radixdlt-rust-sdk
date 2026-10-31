//! End to end over iroh: the MCP tools themselves — `pair_wallet`, `pair_status`, a signing
//! request, `list_wallets`, `check_wallet_connection` — against a simulated Radix Wallet over a
//! real iroh relay running in this process (plain HTTP on 127.0.0.1).
//!
//! The simulated wallet and the relay are connect-iroh's test support, shared rather than copied:
//! the wallet is built on the same protocol library as the real one, links like it (a linking
//! session that drops anything else, closed before its regular session connects) and signs real
//! ROLA proofs.
//!
//! Pairing with the default setting (`auto`) races WebRTC too. Its signaling server is a socket on
//! 127.0.0.1 here — one that takes connections and never answers, or none at all — so nothing
//! leaves this machine.

use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fs4::fs_std::FileExt;
use radixdlt_connect_webrtc::Connector;
use radixdlt_connect_types::linking;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use zeroize::Zeroizing;

use crate::iroh::{LinkRecord, Setting, Transport};
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

/// WebRTC connectors that use ONE signaling server, `url`, and no STUN/TURN server.
fn local_webrtc(url: &str) -> impl Fn() -> Connector + 'static {
    let url = url.to_string();
    move || {
        Connector::new()
            .with_ice_servers(Vec::new())
            .with_signaling_base(url.clone())
    }
}

/// A signaling server address nothing listens on: WebRTC fails at once.
async fn no_signaling() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("ws://{}", listener.local_addr().unwrap())
}

/// A signaling server that takes connections and never answers: the WebRTC half of a pairing
/// waits on it, as on the real one for a wallet that links over iroh. It counts the connections
/// it took and those the connector closed.
struct SilentSignaling {
    url: String,
    taken: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl SilentSignaling {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (taken, closed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (counts_taken, counts_closed) = (taken.clone(), closed.clone());
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counts_taken.fetch_add(1, Ordering::SeqCst);
                let closed = counts_closed.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    while matches!(stream.read(&mut buffer).await, Ok(n) if n > 0) {}
                    closed.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        SilentSignaling {
            url,
            taken,
            closed,
            server,
        }
    }

    /// Waits until `what` holds of (taken, closed).
    async fn wait_for(&self, what: impl Fn(usize, usize) -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !what(
                self.taken.load(Ordering::SeqCst),
                self.closed.load(Ordering::SeqCst),
            ) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the signaling connections");
    }
}

impl Drop for SilentSignaling {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Whether the iroh lock of the link of `password` is free (no connector holds the link).
fn link_lock_is_free(dir: &std::path::Path, password: &[u8]) -> bool {
    let key = crate::iroh::connection_id(password);
    let path = dir.join("iroh-links").join(format!("{}.lock", &key[..32]));
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(path) else {
        return true; // never created
    };
    let free = matches!(FileExt::try_lock_exclusive(&file), Ok(true));
    if free {
        FileExt::unlock(&file).unwrap();
    }
    free
}

/// The events named `name` in the connector log of `dir`, oldest first.
fn events(dir: &std::path::Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join("connector.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event"] == name)
        .collect()
}

/// Waits (up to `limit`) until the log of `dir` holds `count` events named `name`; the last one.
async fn wait_for_event(dir: &std::path::Path, name: &str, count: usize, limit: Duration) -> Value {
    tokio::time::timeout(limit, async {
        loop {
            let found = events(dir, name);
            if found.len() >= count {
                return found.last().cloned().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {name} event in the log"))
}

/// Nothing in the connector log of `dir` opens the link, nor names a network address: no line
/// holds the link password, and blanking IP addresses changes nothing.
fn assert_log_clean(dir: &std::path::Path, password: &[u8]) {
    let log = std::fs::read_to_string(dir.join("connector.log")).unwrap_or_default();
    assert!(!log.is_empty(), "a log was written");
    assert!(
        !log.contains(&hex::encode(password)),
        "the link password is in the log"
    );
    assert_eq!(crate::diag::scrub_ips(&log), log, "an IP address is in the log");
}

/// A `connector.json` with one link, password `password`, to a wallet that is not there.
fn seed_link(dir: &std::path::Path, password: &[u8]) {
    std::fs::remove_dir_all(dir).ok();
    std::fs::create_dir_all(dir).unwrap();
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
        // Set to WebRTC: pairing over iroh is asked for by the call itself.
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

        // The wallet's transport, live connection and last request are visible.
        let wallets = text(&tools::call(&app, "list_wallets", json!({})).await);
        assert!(wallets.contains("transport:  iroh (relay"), "{wallets}");
        assert!(
            wallets.contains("paired over iroh (transports.json)"),
            "{wallets}"
        );
        assert!(wallets.contains("state=connected"), "{wallets}");
        assert!(
            wallets.contains("Last request: request_account_proof (account_proof)")
                && wallets.contains("answered"),
            "{wallets}"
        );
        assert!(wallets.contains("Iroh link held by: this process"), "{wallets}");
        let reachable =
            text(&tools::call(&app, "check_wallet_connection", json!({ "timeout_seconds": 20 })).await);
        assert!(reachable.contains("WALLET REACHABLE"), "{reachable}");
        assert!(reachable.contains("Connection: connector "), "{reachable}");
        assert!(reachable.contains("Last request: "), "{reachable}");

        // The trace follows the request over iroh — which transport and why, each step, each
        // change of its state — and holds no link password.
        let log = std::fs::read_to_string(dir.join("connector.log")).unwrap();
        assert!(log.contains("\"transport\":\"iroh (relay"), "{log}");
        assert!(log.contains("\"event\":\"delivered\""), "{log}");
        let start = &events(&dir, "request_start")[0];
        assert_eq!(start["route"], "paired over iroh (transports.json)", "{start}");
        assert_eq!(start["concurrent"], 0, "{start}");
        let id = start["interaction_id"].as_str().unwrap();
        let sending = &events(&dir, "sending")[0];
        assert_eq!(
            (sending["interaction_id"].as_str(), sending["attempt"].as_u64()),
            (Some(id), Some(1))
        );
        let states: Vec<(String, String)> = events(&dir, "request_state")
            .iter()
            .filter(|e| e["interaction_id"] == id)
            .map(|e| {
                (
                    e["from"].as_str().unwrap().to_string(),
                    e["to"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            states.first().map(|s| s.0.as_str()),
            Some("sending"),
            "{states:?}"
        );
        assert_eq!(
            states.last().map(|s| s.1.as_str()),
            Some("answered"),
            "{states:?}"
        );
        assert_eq!(
            events(&dir, "iroh_link_adopted").len(),
            1,
            "the pairing's session is kept"
        );
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("connector.json")).unwrap()).unwrap();
        let password = state["links"][0]["password"].as_str().unwrap().to_string();
        assert!(!log.contains(&password));
        let records = std::fs::read_to_string(dir.join("transports.json")).unwrap();
        assert!(
            records.contains("\"transport\": \"iroh\"") && !records.contains(&password),
            "{records}"
        );
        assert_log_clean(&dir, &hex::decode(&password).unwrap());

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

/// The default setting with a wallet that has iroh on: ONE QR, the wallet links over iroh while
/// the WebRTC half still waits at its signaling server; iroh is recorded, the WebRTC half is
/// dropped (its signaling connection closed), and the wallet signs over iroh straight after.
#[test]
fn by_default_a_wallet_with_iroh_links_over_iroh_and_webrtc_lets_go() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let signaling = SilentSignaling::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_auto_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let app = Rc::new(
            App::at(
                dir.join("connector.json"),
                Options {
                    iroh_relay: Some(relay.url.clone()),
                    ..Options::default()
                },
            )
            .with_webrtc(local_webrtc(&signaling.url)),
        );
        assert_eq!(app.iroh.setting, Setting::Auto, "auto is the default");

        let pairing = text(&tools::call(&app, "pair_wallet", json!({ "label": "sim" })).await);
        assert!(pairing.contains("transport: auto"), "{pairing}");
        assert!(pairing.contains("any Radix Wallet"), "{pairing}");
        let payload = qr_payload(&pairing);
        // The WebRTC half is at its signaling server, waiting for a wallet.
        signaling.wait_for(|taken, _| taken >= 1).await;

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
        // The WebRTC half was dropped with it: nothing is left at the signaling server.
        signaling
            .wait_for(|taken, closed| taken >= 1 && closed == taken)
            .await;
        let none = text(&tools::call(&app, "pair_status", json!({ "wait_seconds": 1 })).await);
        assert!(none.contains("no pairing in progress"), "{none}");

        // Recorded as paired over iroh, and logged so.
        let records = std::fs::read_to_string(dir.join("transports.json")).unwrap();
        assert!(records.contains("\"transport\": \"iroh\""), "{records}");
        let log = std::fs::read_to_string(dir.join("connector.log")).unwrap();
        assert!(
            log.contains("\"event\":\"pairing_started\"") && log.contains("[\"webrtc\",\"iroh\"]"),
            "{log}"
        );
        let paired = log
            .lines()
            .find(|line| line.contains("\"event\":\"paired\""))
            .unwrap_or_default();
        assert!(
            paired.contains("\"transport\":\"iroh\"") && paired.contains("\"why\":\"iroh first\""),
            "{log}"
        );

        // A request right after goes over iroh.
        let proof = tools::call(
            &app,
            "request_account_proof",
            json!({
                "network": "stokenet", "dapp_definition": DAPP, "origin": ORIGIN,
                "challenge": support::challenge(), "skip_dapp_check": true, "timeout_seconds": 60,
            }),
        )
        .await;
        let proof_text = text(&proof);
        assert!(proof_text.contains("ACCOUNT PROOF VERIFIED"), "{proof_text}");
        assert_eq!(running.received().len(), 1, "sent exactly once");

        // Removing the wallet tells it (it is connected), then closes its session and lets go of
        // its link.
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("connector.json")).unwrap()).unwrap();
        let password = hex::decode(state["links"][0]["password"].as_str().unwrap()).unwrap();
        assert!(!link_lock_is_free(&dir, &password), "held while in use");
        let removed = text(
            &tools::call(
                &app,
                "remove_wallet",
                json!({ "wallet_public_key": wallet.public_key_hex() }),
            )
            .await,
        );
        assert!(
            removed.contains("the wallet removed this connector too"),
            "{removed}"
        );
        assert_eq!(
            running.received().last().unwrap(),
            &json!({ "discriminator": "linkRemoved" })
        );
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("connector.json")).unwrap()).unwrap();
        assert!(state.get("retiredLinks").is_none(), "no notice left: {state}");
        assert!(link_lock_is_free(&dir, &password));
        assert!(app.iroh.diagnostics(&password).is_none());

        running.close().await;
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// A stand-in for the WebRTC half of a pairing (no WebRTC wallet runs here): it "links" wallet
/// `wallet` once `ready` holds. The iroh half, the decision and what follows it are the
/// connector's own.
fn webrtc_links_when<'a>(ready: impl Fn() -> bool + 'a, wallet: &'a str) -> tools::Attempt<'a> {
    Box::pin(async move {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(wallet.to_string())
    })
}

/// An app on the local relay, with the default setting.
fn auto_app(dir: &std::path::Path, relay: &LocalRelay) -> Rc<App> {
    std::fs::remove_dir_all(dir).ok();
    Rc::new(App::at(
        dir.join("connector.json"),
        Options {
            iroh_relay: Some(relay.url.clone()),
            ..Options::default()
        },
    ))
}

/// A wallet without iroh (the official one) links over WebRTC: iroh gets its grace, nothing
/// links over it, and WebRTC is kept — the iroh half, whose session was listening, is dropped:
/// no session, no lock.
#[test]
fn a_wallet_that_links_over_webrtc_only_is_kept_after_the_grace_and_iroh_goes() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_webrtc_kept_{}", std::process::id()));
        let app = auto_app(&dir, &relay);
        let password = Zeroizing::new(linking::generate_password().unwrap());
        let iroh = app.iroh.connector(&app.iroh.relay).unwrap();
        let wallet = "ab".repeat(32);
        // WebRTC links once the iroh session listens, so iroh is dropped while it runs.
        let over_webrtc = webrtc_links_when(|| iroh.diagnostics(password.as_slice()).is_some(), &wallet);

        let started = std::time::Instant::now();
        let chosen = tools::choose_link(
            vec![
                (Transport::Webrtc, over_webrtc),
                (Transport::Iroh, tools::attempt(&app, Transport::Iroh, &password)),
            ],
            tools::IROH_GRACE,
        )
        .await;
        assert!(started.elapsed() >= tools::IROH_GRACE, "{:?}", started.elapsed());
        let linked = chosen.clone().unwrap();
        assert_eq!(
            (linked.transport, linked.why),
            (Transport::Webrtc, "webrtc: no iroh within grace")
        );
        assert!(
            iroh.diagnostics(password.as_slice()).is_none(),
            "the iroh half's session is closed"
        );

        let outcome = tools::concluded(&app, password.as_slice(), chosen, std::time::Instant::now()).unwrap();
        assert_eq!(
            outcome.paired_over,
            LinkRecord {
                transport: Transport::Webrtc,
                iroh_relay: None
            }
        );
        assert!(app.iroh.diagnostics(password.as_slice()).is_none());
        assert!(link_lock_is_free(&dir, password.as_slice()));
        let log = std::fs::read_to_string(dir.join("connector.log")).unwrap();
        assert!(
            log.contains("\"transport\":\"webrtc\"")
                && log.contains("\"why\":\"webrtc: no iroh within grace\""),
            "{log}"
        );
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// iroh is preferred: when WebRTC links first and the wallet links over iroh within the grace,
/// iroh is kept (and its session, for the first requests).
#[test]
fn iroh_linking_within_the_grace_after_webrtc_is_kept() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_iroh_grace_{}", std::process::id()));
        let app = auto_app(&dir, &relay);
        let password = Zeroizing::new(linking::generate_password().unwrap());
        let (private, public) = linking::generate_identity().unwrap();
        let qr = linking::pairing_qr(password.as_slice(), &private, &public).unwrap();
        let iroh = app.iroh.connector(&app.iroh.relay).unwrap();
        let wallet = Arc::new(SimWallet::default());
        // WebRTC "links" the moment the wallet's iroh connection is up — before its linkClient.
        let over_webrtc = webrtc_links_when(|| iroh.is_connected(password.as_slice()), "webrtc-wallet");

        let deciding = tools::choose_link(
            vec![
                (Transport::Webrtc, over_webrtc),
                (Transport::Iroh, tools::attempt(&app, Transport::Iroh, &password)),
            ],
            tools::IROH_GRACE,
        );
        let phone = wallet.scan_and_link(&qr, &relay.url, LinkSignature::Valid);
        let (chosen, _) = tokio::join!(deciding, phone);
        assert_eq!(
            chosen,
            Ok(tools::Linked {
                transport: Transport::Iroh,
                wallet_public_key: wallet.public_key_hex(),
                why: "iroh within grace after webrtc",
            })
        );
        let outcome = tools::concluded(&app, password.as_slice(), chosen, std::time::Instant::now()).unwrap();
        assert_eq!(outcome.paired_over.transport, Transport::Iroh);
        assert!(
            app.iroh.diagnostics(password.as_slice()).is_some(),
            "kept for requests"
        );
        let log = std::fs::read_to_string(dir.join("connector.log")).unwrap();
        assert!(
            log.contains("\"why\":\"iroh within grace after webrtc\""),
            "{log}"
        );
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// When iroh cannot even start (a relay setting it cannot use), the pairing goes on over WebRTC
/// instead of failing; when WebRTC cannot either, it fails naming both reasons.
#[test]
fn a_transport_that_fails_leaves_the_other_and_both_failing_name_both() {
    in_local_set(async {
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_auto_fails_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        // Plain http to a public host: refused by the iroh library before anything is sent.
        let unusable_relay = Options {
            transport: Setting::Auto,
            iroh_relay: Some("http://relay.example.com".into()),
        };

        let signaling = SilentSignaling::start().await;
        let app = Rc::new(
            App::at(dir.join("connector.json"), unusable_relay.clone())
                .with_webrtc(local_webrtc(&signaling.url)),
        );
        let pairing = text(&tools::call(&app, "pair_wallet", json!({})).await);
        assert!(pairing.contains("```json"), "{pairing}");
        let waiting = text(&tools::call(&app, "pair_status", json!({ "wait_seconds": 2 })).await);
        assert!(waiting.contains("Still waiting"), "{waiting}");
        let log = std::fs::read_to_string(dir.join("connector.log")).unwrap();
        let failed = log
            .lines()
            .find(|line| line.contains("\"event\":\"pairing_attempt_failed\""))
            .unwrap_or_default();
        assert!(failed.contains("\"transport\":\"iroh\""), "{log}");
        signaling.wait_for(|taken, _| taken >= 1).await;

        // A new pairing stops the one before, its WebRTC half included; over iroh only, with that
        // relay setting, it fails — and says over what.
        text(&tools::call(&app, "pair_wallet", json!({ "transport": "iroh" })).await);
        signaling
            .wait_for(|taken, closed| taken >= 1 && closed == taken)
            .await;
        let result = tools::call(&app, "pair_status", json!({ "wait_seconds": 10 })).await;
        assert_eq!(result["isError"], true, "{result}");
        let failure = text(&result);
        assert!(failure.contains("pairing failed: over iroh: "), "{failure}");
        assert!(!failure.contains("webrtc"), "{failure}");

        let app = Rc::new(
            App::at(dir.join("connector.json"), unusable_relay)
                .with_webrtc(local_webrtc(&no_signaling().await)),
        );
        text(&tools::call(&app, "pair_wallet", json!({})).await);
        let result = tools::call(&app, "pair_status", json!({ "wait_seconds": 20 })).await;
        assert_eq!(result["isError"], true, "{result}");
        let failure = text(&result);
        assert!(failure.contains("pairing failed: over "), "{failure}");
        assert!(
            failure.contains("over webrtc: ") && failure.contains("over iroh: "),
            "{failure}"
        );
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// Two connector processes (two `App`s on one state directory) and one wallet link over iroh: the
/// one that holds it keeps it; the other reports LINK_BUSY naming the holder, how long it waited
/// and its limit — or, given the time, waits its turn and takes the link once it is let go of.
#[test]
fn two_connectors_on_one_link_one_holds_it_the_other_is_busy_or_waits_its_turn() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_two_{}", std::process::id()));
        let password = [0x51u8; 32];
        seed_link(&dir, &password);
        let options = Options {
            transport: Setting::Iroh,
            iroh_relay: Some(relay.url.clone()),
        };
        let mut first = App::at(dir.join("connector.json"), options.clone());
        first.iroh.set_idle(Duration::from_millis(300));
        let (first, second) = (
            Rc::new(first),
            Rc::new(App::at(dir.join("connector.json"), options)),
        );
        let relay_choice = Some(relay.url.clone());
        let soon = || std::time::Instant::now() + Duration::from_secs(15);

        let held = crate::iroh::acquire(&first, &password, &relay_choice, soon())
            .await
            .unwrap();
        let busy = tools::call(
            &second,
            "check_wallet_connection",
            json!({ "timeout_seconds": 1 }),
        )
        .await;
        let failure = &busy["structuredContent"];
        assert_eq!(failure["code"], "LINK_BUSY", "{busy}");
        let detail = failure["detail"].as_str().unwrap();
        assert!(
            detail.contains(&format!("pid {}", std::process::id())) && detail.contains("of 1000 ms"),
            "{detail}"
        );
        let wait = &events(&dir, "iroh_link_wait")[0];
        assert_eq!(wait["holder_pid"], std::process::id(), "{wait}");
        let busy_event = &events(&dir, "iroh_link_busy")[0];
        assert_eq!(busy_event["limit_ms"], 1000, "{busy_event}");
        assert!(busy_event["waited_ms"].as_u64().unwrap() >= 1000, "{busy_event}");
        let probe = &events(&dir, "probe_failed")[0];
        assert!(
            probe["holder"].as_str().unwrap().contains("another process"),
            "{probe}"
        );

        // Given the time, the second waits behind the first and takes the link once it goes.
        let letting_go = async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(held);
        };
        let (taken, ()) = tokio::join!(
            crate::iroh::acquire(&second, &password, &relay_choice, soon()),
            letting_go
        );
        let taken = taken.unwrap();
        let locked = &events(&dir, "iroh_link_locked")[0];
        assert!(locked["waited_ms"].as_u64().unwrap() >= 500, "{locked}");
        let released = &events(&dir, "iroh_link_released")[0];
        assert_eq!(released["uses"], 1, "{released}");
        assert!(
            events(&dir, "iroh_link_lock_recovered").is_empty(),
            "a link let go of cleanly is not a crashed holder"
        );
        drop(taken);
        assert_log_clean(&dir, &password);
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// A connector that crashed holding a link leaves its record in the link's lock file (the OS
/// frees the lock itself): the next one takes the link and says so.
#[test]
fn a_link_left_locked_by_a_crashed_connector_is_recovered_and_logged() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_stale_{}", std::process::id()));
        let password = [0x52u8; 32];
        seed_link(&dir, &password);
        let key = crate::iroh::connection_id(&password);
        std::fs::create_dir_all(dir.join("iroh-links")).unwrap();
        let lock = dir.join("iroh-links").join(format!("{}.lock", &key[..32]));
        std::fs::write(&lock, "999999 1700000000").unwrap();
        let app = Rc::new(App::at(
            dir.join("connector.json"),
            Options {
                transport: Setting::Iroh,
                iroh_relay: Some(relay.url.clone()),
            },
        ));

        let turn = crate::iroh::acquire(
            &app,
            &password,
            &Some(relay.url.clone()),
            std::time::Instant::now() + Duration::from_secs(15),
        )
        .await
        .unwrap();
        let recovered = &events(&dir, "iroh_link_lock_recovered")[0];
        assert_eq!(recovered["previous_holder_pid"], 999_999, "{recovered}");
        assert_eq!(
            recovered["previous_holder_since"], "2023-11-14T22:13:20Z",
            "{recovered}"
        );
        let record = std::fs::read_to_string(&lock).unwrap();
        assert!(
            record.starts_with(&format!("{} ", std::process::id())),
            "{record}"
        );
        assert!(app.iroh.holder(&password).starts_with("this process"));
        drop(turn);
        assert_log_clean(&dir, &password);
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// A link's session through its life: opened (first use), the wallet reaching it (timed), let go
/// of when idle — a release cancelled by a call, then one done — and reopened, timed again until
/// the wallet is back: what a person feels after an idle release.
#[test]
fn an_idle_link_is_released_and_reopening_it_is_timed_until_the_wallet_is_back() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_idle_{}", std::process::id()));
        let password = [0x53u8; 32];
        seed_link(&dir, &password);
        let mut app = App::at(
            dir.join("connector.json"),
            Options {
                transport: Setting::Iroh,
                iroh_relay: Some(relay.url.clone()),
            },
        );
        app.iroh.set_idle(Duration::from_millis(300));
        let app = Rc::new(app);
        let relay_choice = Some(relay.url.clone());
        let soon = || std::time::Instant::now() + Duration::from_secs(15);

        let turn = crate::iroh::acquire(&app, &password, &relay_choice, soon())
            .await
            .unwrap();
        let opened = &events(&dir, "iroh_link_open")[0];
        assert_eq!(opened["why"], "first use in this process", "{opened}");
        let wallet = Arc::new(SimWallet::default());
        let running = wallet.connect(&password, &relay.url, answers_rola()).await;
        let connected = wait_for_event(&dir, "iroh_link_wallet_connected", 1, Duration::from_secs(20)).await;
        assert_eq!(connected["opened"], "first use in this process", "{connected}");

        // Used again before the idle time is up: the release is cancelled.
        drop(turn);
        let again = crate::iroh::acquire(&app, &password, &relay_choice, soon())
            .await
            .unwrap();
        assert_eq!(events(&dir, "iroh_link_release_cancelled").len(), 1);
        drop(again);
        let released = wait_for_event(&dir, "iroh_link_released", 1, Duration::from_secs(10)).await;
        assert_eq!(released["idle_ms"], 300, "{released}");
        assert!(link_lock_is_free(&dir, &password));
        assert!(app.iroh.holder(&password) == "nobody");

        // Reopened: why, and how long until the wallet (re-dialing) is back.
        let reopened = crate::iroh::acquire(&app, &password, &relay_choice, soon())
            .await
            .unwrap();
        let open = events(&dir, "iroh_link_open").last().cloned().unwrap();
        assert_eq!(open["why"], "reopened after an idle release", "{open}");
        assert!(open["released_ago_ms"].is_u64(), "{open}");
        let back = wait_for_event(&dir, "iroh_link_wallet_connected", 2, Duration::from_secs(30)).await;
        assert_eq!(back["opened"], "reopened after an idle release", "{back}");
        assert!(
            back["after_ms"].is_u64() && back["diagnostics"].as_str().unwrap().contains("state=connected"),
            "{back}"
        );
        let idle = events(&dir, "iroh_link_idle");
        assert!(idle.len() >= 2 && idle[0]["release_after_ms"] == 300, "{idle:?}");

        drop(reopened);
        running.close().await;
        assert_log_clean(&dir, &password);
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// The libraries' own events reach the log: a collision (another holder of the link key took this
/// end's place) as `link_collision` with the held links' diagnostics, a replaced wallet connection,
/// the rest as `iroh_protocol` / `iroh_sdk`; repeats counted, addresses dropped.
#[test]
fn library_events_reach_the_log_and_a_collision_carries_the_links_diagnostics() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_trace_{}", std::process::id()));
        let password = [0x54u8; 32];
        seed_link(&dir, &password);
        let app = Rc::new(App::at(
            dir.join("connector.json"),
            Options {
                transport: Setting::Iroh,
                iroh_relay: Some(relay.url.clone()),
            },
        ));
        let _bridge = crate::trace::scoped(app.log.clone(), app.iroh.held_links());
        let turn = crate::iroh::acquire(
            &app,
            &password,
            &Some(relay.url.clone()),
            std::time::Instant::now() + Duration::from_secs(15),
        )
        .await
        .unwrap();
        let link = crate::iroh::short_id(&crate::iroh::connection_id(&password)).to_string();

        // The SDK's lines (named by the same link id as the connector's) and the protocol's.
        // Lines of this test's own: the libraries' may have been reached first on another test
        // thread, which `tracing` remembers process-wide (see `trace::scoped`).
        tracing::info!(target: "radixdlt_connect_iroh::mobile", link = link.as_str(), interaction = "i-1", "the wallet took the request");
        tracing::info!(target: "radix_connect_iroh::session", relay = "http://127.0.0.1:1/", "connected to the relay");
        let sdk = &events(&dir, "iroh_sdk")
            .into_iter()
            .find(|e| e["message"] == "the wallet took the request")
            .unwrap();
        assert_eq!(
            (sdk["link"].as_str(), sdk["interaction_id"].as_str()),
            (Some(link.as_str()), Some("i-1")),
            "{sdk}"
        );
        assert!(events(&dir, "iroh_protocol")
            .iter()
            .any(|e| e["message"] == "connected to the relay"));

        for _ in 0..3 {
            tracing::warn!(target: "radix_connect_iroh::session", local = "abc", "displaced at the relay by another holder of the link key");
        }
        let collisions = events(&dir, "link_collision");
        assert_eq!(
            collisions.len(),
            1,
            "repeats are counted, not written: {collisions:?}"
        );
        let links = collisions[0]["links"].as_str().unwrap();
        assert!(
            links.contains(&format!("link {link}: connector ")) && links.contains("relay="),
            "{links}"
        );

        tracing::info!(target: "radix_connect_iroh::session", peer = "f00d", "radix connect iroh session started");
        tracing::info!(target: "radix_connect_iroh::session", peer = "f00d", "connected to the peer");
        tracing::info!(target: "radix_connect_iroh::session", peer = "f00d", "connected to the peer");
        assert_eq!(events(&dir, "wallet_connection_replaced").len(), 1);

        tracing::warn!(target: "radix_connect_iroh::session", addr = "192.168.1.9:1234", "no route to 10.0.0.7");
        let log = std::fs::read_to_string(dir.join("connector.log")).unwrap();
        assert!(!log.contains("192.168.1.9") && !log.contains("10.0.0.7"), "{log}");

        drop(turn);
        assert_log_clean(&dir, &password);
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// An app on `dir`'s `connector.json`, every link over iroh at the local `relay`.
fn iroh_app(dir: &std::path::Path, relay: &LocalRelay) -> Rc<App> {
    Rc::new(App::at(
        dir.join("connector.json"),
        Options {
            transport: Setting::Iroh,
            iroh_relay: Some(relay.url.clone()),
        },
    ))
}

fn connector_state(dir: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("connector.json")).unwrap()).unwrap()
}

/// The person removed this connector in the wallet: the wallet says so when it next connects, the
/// request fails with LINK_REMOVED (not a timeout to retry), and the wallet is unpaired here.
#[test]
fn a_wallet_that_removed_this_connector_fails_the_request_and_is_unpaired_here() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!(
            "connector_mcp_e2e_removed_by_wallet_{}",
            std::process::id()
        ));
        let password = [0x51u8; 32];
        seed_link(&dir, &password);
        let app = iroh_app(&dir, &relay);
        let wallet = Arc::new(SimWallet::default());
        // As the Radix Wallet with this link retired: it takes the request, answers nothing to it,
        // and says the link was removed.
        let removed: support::Handler = Arc::new(|_: &SimWallet, _: &Value| {
            support::Reply::Now(json!({ "discriminator": "linkRemoved" }))
        });
        // The request opens the link; the wallet connects once it listens.
        let (result, running) = tokio::join!(
            tools::call(
                &app,
                "request_account_proof",
                json!({
                    "network": "stokenet", "dapp_definition": DAPP, "origin": ORIGIN,
                    "challenge": support::challenge(), "skip_dapp_check": true, "timeout_seconds": 20,
                }),
            ),
            wallet.connect(&password, &relay.url, removed),
        );

        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["code"], "LINK_REMOVED", "{result}");
        assert_eq!(
            result["structuredContent"]["retry_safe"], true,
            "nothing waits in the wallet"
        );
        let state = connector_state(&dir);
        assert_eq!(state["links"], json!([]), "{state}");
        assert!(
            state.get("retiredLinks").is_none(),
            "the wallet knows: no notice: {state}"
        );
        assert!(link_lock_is_free(&dir, &password));
        let listed = text(&tools::call(&app, "list_wallets", json!({})).await);
        assert!(listed.contains("No wallets paired"), "{listed}");
        running.close().await;
        std::fs::remove_dir_all(&dir).ok();
    });
}

/// Removing a wallet whose app is closed keeps a notice; once the wallet connects while this
/// connector runs, it is told, confirmed, and the notice is gone.
#[test]
fn a_wallet_removed_while_its_app_is_closed_is_told_when_it_connects() {
    in_local_set(async {
        let relay = LocalRelay::start().await;
        let dir = std::env::temp_dir().join(format!("connector_mcp_e2e_notice_later_{}", std::process::id()));
        let password = [0x52u8; 32];
        seed_link(&dir, &password);
        let app = iroh_app(&dir, &relay);

        let removed = text(
            &tools::call(
                &app,
                "remove_wallet",
                json!({ "wallet_public_key": "ab".repeat(32) }),
            )
            .await,
        );
        assert!(removed.contains("was not reachable now"), "{removed}");
        let state = connector_state(&dir);
        assert_eq!(state["links"], json!([]));
        assert_eq!(
            state["retiredLinks"][0]["password"],
            hex::encode(password),
            "{state}"
        );
        let listed = text(&tools::call(&app, "list_wallets", json!({})).await);
        assert!(listed.contains("REMOVED, NOT TOLD YET"), "{listed}");
        assert!(
            !listed.contains(&hex::encode(password)),
            "never the password: {listed}"
        );

        // The connector runs on; the person opens the wallet.
        tokio::task::spawn_local(crate::link_removal::deliver_notices(app.clone()));
        let wallet = Arc::new(SimWallet::default());
        let running = wallet.connect(&password, &relay.url, answers_rola()).await;
        let told = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if running
                    .received()
                    .contains(&json!({ "discriminator": "linkRemoved" }))
                    && connector_state(&dir).get("retiredLinks").is_none()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        assert!(
            told.is_ok(),
            "told, and the notice gone: {}",
            connector_state(&dir)
        );
        assert!(!events(&dir, "removal_notice_delivered").is_empty());
        assert_log_clean(&dir, &password);
        running.close().await;
        std::fs::remove_dir_all(&dir).ok();
    });
}
