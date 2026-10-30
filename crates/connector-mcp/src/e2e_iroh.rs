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
use radixdlt_connect::Connector;
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

        // Removing the wallet closes its session and lets go of its link.
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
        assert!(removed.contains("Removed"), "{removed}");
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

        let outcome = tools::concluded(&app, password.as_slice(), chosen).unwrap();
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
        let outcome = tools::concluded(&app, password.as_slice(), chosen).unwrap();
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
