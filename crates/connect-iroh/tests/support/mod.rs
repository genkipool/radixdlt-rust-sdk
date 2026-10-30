//! What the mobile-wallet tests run against: a real iroh relay in this process, over plain HTTP
//! on 127.0.0.1, and a simulated Radix Wallet built on the same protocol library the real one
//! uses (`radix_connect_iroh::Session`, wallet role) that behaves like it:
//!
//! - it links by scanning the QR (checking the connector's signature, as the wallet does),
//!   sending a signed `linkClient` on a LINKING session that takes and drops any other message
//!   while the link is saved, and that it then closes before its regular session connects;
//! - its regular session sends an `accountList` each time it connects;
//! - it answers requests by `interactionId`, signing ROLA proofs with real keys.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use radix_connect_iroh::{ConnectionPassword, ConnectionState, Role, Session, SessionConfig, SessionEvent};
use radixdlt_address::{virtual_account_address, virtual_identity_address};
use radixdlt_connect_iroh::mobile::{linking, Progress};
use radixdlt_connect_types::{account_proof_response, parse_account_proof_request};
use radixdlt_rola::signature_message;
use serde_json::{json, Value};

pub const NETWORK_ID: u8 = 2;
pub const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
pub const ORIGIN: &str = "https://dapp.example.org";

/// Generous bound for anything that goes through the local relay.
pub const WAIT: Duration = Duration::from_secs(30);

/// How long the linking session stays open after its `linkClient` was confirmed, as the real
/// wallet saves the link and shows its confirmation before closing it.
pub const LINKING_SAVE_TIME: Duration = Duration::from_millis(1500);

/// An iroh relay on 127.0.0.1, plain HTTP. Dropping it stops it.
pub struct LocalRelay {
    _server: Server,
    pub url: String,
}

impl LocalRelay {
    pub async fn start() -> Self {
        let mut config = ServerConfig::default();
        config.relay = Some(RelayConfig::new(SocketAddr::from(([127, 0, 0, 1], 0))));
        let server = Server::spawn(config).await.expect("start the local relay");
        let url = format!("http://{}", server.http_addr().expect("the relay serves http"));
        LocalRelay { _server: server, url }
    }
}

pub fn ctx() -> radixdlt_connect_iroh::mobile::DappContext {
    radixdlt_connect_iroh::mobile::DappContext::new(NETWORK_ID, DAPP, ORIGIN)
}

pub fn challenge() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).unwrap();
    hex::encode(bytes)
}

/// How the simulated wallet signs its `linkClient`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSignature {
    /// As the real wallet: over `blake2b("L" ‖ password)`.
    Valid,
    /// Over another password's linking message.
    Forged,
}

/// What the simulated wallet does with a request.
pub enum Reply {
    /// Answer right away.
    Now(Value),
    /// Answer after a while (the person takes their time).
    After(Duration, Value),
    /// Record it and say nothing (the person has not decided).
    Nothing,
}

pub type Handler = Arc<dyn Fn(&SimWallet, &Value) -> Reply + Send + Sync>;

/// The keys of a simulated wallet: one to link, one account, one persona.
pub struct SimWallet {
    pub link_key: SigningKey,
    pub account_key: SigningKey,
    pub identity_key: SigningKey,
    /// What its linking sessions took and dropped: a request sent there is lost.
    pub ignored_while_linking: Arc<Mutex<Vec<Value>>>,
}

impl Default for SimWallet {
    fn default() -> Self {
        let key = || {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).unwrap();
            SigningKey::from_bytes(&seed)
        };
        SimWallet {
            link_key: key(),
            account_key: key(),
            identity_key: key(),
            ignored_while_linking: Arc::default(),
        }
    }
}

fn public_hex(key: &SigningKey) -> String {
    hex::encode(key.verifying_key().to_bytes())
}

impl SimWallet {
    pub fn public_key_hex(&self) -> String {
        public_hex(&self.link_key)
    }

    pub fn account_address(&self) -> String {
        virtual_account_address(&public_hex(&self.account_key), NETWORK_ID).unwrap()
    }

    pub fn identity_address(&self) -> String {
        virtual_identity_address(&public_hex(&self.identity_key), NETWORK_ID).unwrap()
    }

    fn rola_proof(&self, key: &SigningKey, request: &Value) -> Value {
        let parsed = parse_account_proof_request(request).expect("an account-proof request");
        let message =
            signature_message(&parsed.challenge_hex, &parsed.dapp_definition, &parsed.origin).unwrap();
        json!({
            "publicKey": public_hex(key),
            "signature": hex::encode(key.sign(&message).to_bytes()),
            "curve": "curve25519",
        })
    }

    /// The answer the real wallet gives to an account-proof or login request, with real ROLA
    /// signatures by this wallet's account (and persona, for a login).
    pub fn rola_answer(&self, request: &Value) -> Value {
        let id = request["interactionId"].as_str().unwrap();
        let address = self.account_address();
        let account_proof = self.rola_proof(&self.account_key, request);
        if request["items"]["auth"]["discriminator"] == "loginWithChallenge" {
            let challenge = request["items"]["auth"]["challenge"].clone();
            return json!({
                "discriminator": "success",
                "interactionId": id,
                "items": {
                    "discriminator": "authorizedRequest",
                    "auth": {
                        "discriminator": "loginWithChallenge",
                        "persona": { "identityAddress": self.identity_address(), "label": "Sim" },
                        "challenge": challenge,
                        "proof": self.rola_proof(&self.identity_key, request),
                    },
                    "oneTimeAccounts": {
                        "accounts": [ { "address": address } ],
                        "challenge": challenge,
                        "proofs": [ { "accountAddress": address, "proof": account_proof } ],
                    },
                },
            });
        }
        account_proof_response(
            id,
            &address,
            account_proof["publicKey"].as_str().unwrap(),
            account_proof["signature"].as_str().unwrap(),
            None,
        )
    }

    /// Scans the QR like the wallet does: checks the connector's signature, connects a LINKING
    /// session, sends a signed `linkClient`, keeps the session open while the link is saved —
    /// taking (so confirming) and dropping any message that arrives on it — and closes it.
    /// Returns the link password.
    pub async fn scan_and_link(&self, qr: &str, relay: &str, signature: LinkSignature) -> Vec<u8> {
        let qr: Value = serde_json::from_str(qr).unwrap();
        assert_eq!(qr["purpose"], "general");
        let password = hex::decode(qr["password"].as_str().unwrap()).unwrap();
        linking::verify_link_signature(
            &password,
            qr["publicKey"].as_str().unwrap(),
            qr["signature"].as_str().unwrap(),
        )
        .expect("the connector's QR signature verifies");

        let signed = match signature {
            LinkSignature::Valid => password.clone(),
            LinkSignature::Forged => vec![0x77; 32],
        };
        let link_client = json!({
            "discriminator": "linkClient",
            "publicKey": self.public_key_hex(),
            "signature": hex::encode(self.link_key.sign(&linking::linking_message(&signed)).to_bytes()),
        });

        let session = Arc::new(start_wallet_session(&password, SessionConfig::new(relay).unwrap()).await);
        let reader = {
            let session = session.clone();
            let ignored = self.ignored_while_linking.clone();
            tokio::spawn(async move {
                while let Some(event) = session.next_event().await {
                    if let SessionEvent::Message(text) = event {
                        ignored.lock().unwrap().push(serde_json::from_str(&text).unwrap());
                    }
                }
            })
        };
        wait_connected(&session).await;
        session
            .send(&link_client.to_string())
            .await
            .expect("linkClient confirmed");
        tokio::time::sleep(LINKING_SAVE_TIME).await;
        session.close().await;
        reader.abort();
        password
    }

    /// Connects the wallet's regular session for `password`, on the custom relay `relay`, and
    /// serves it with `handler`.
    pub async fn connect(self: &Arc<Self>, password: &[u8], relay: &str, handler: Handler) -> RunningWallet {
        self.connect_with(password, SessionConfig::new(relay).unwrap(), handler)
            .await
    }

    /// The same, with any session settings (the public relays, for one).
    pub async fn connect_with(
        self: &Arc<Self>,
        password: &[u8],
        config: SessionConfig,
        handler: Handler,
    ) -> RunningWallet {
        let session = Arc::new(start_wallet_session(password, config).await);
        let received = Arc::new(Mutex::new(Vec::new()));
        let reading = Arc::new(tokio::sync::watch::Sender::new(true));
        let connections = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(serve(
            self.clone(),
            session.clone(),
            handler,
            received.clone(),
            reading.clone(),
            connections.clone(),
        ));
        let running = RunningWallet {
            session,
            received,
            reading,
            connections,
            task,
        };
        running.wait_connected().await;
        running
    }
}

async fn start_wallet_session(password: &[u8], config: SessionConfig) -> Session {
    let password = ConnectionPassword::from_slice(password).unwrap();
    Session::start(&password, Role::Wallet, config).await.unwrap()
}

/// Waits for `session` to be connected (its state, polled: the events belong to its reader).
async fn wait_connected(session: &Session) {
    tokio::time::timeout(WAIT, async {
        while session.state() != ConnectionState::Connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the simulated wallet connects");
}

/// The simulated wallet's one reader of its session.
async fn serve(
    wallet: Arc<SimWallet>,
    session: Arc<Session>,
    handler: Handler,
    received: Arc<Mutex<Vec<Value>>>,
    reading: Arc<tokio::sync::watch::Sender<bool>>,
    connections: Arc<AtomicUsize>,
) {
    let mut reading = reading.subscribe();
    loop {
        // A wallet that is busy (or whose app is in the background) does not take messages.
        let _ = reading.wait_for(|reading| *reading).await;
        // `next_event` is cancel-safe: abandoning it neither loses nor confirms a message.
        let event = tokio::select! {
            biased;
            _ = reading.wait_for(|reading| !*reading) => continue,
            event = session.next_event() => event,
        };
        let Some(event) = event else {
            return;
        };
        match event {
            SessionEvent::StateChanged(ConnectionState::Connected) => {
                connections.fetch_add(1, Ordering::SeqCst);
                let accounts = json!({
                    "discriminator": "accountList",
                    "accounts": [ { "address": wallet.account_address(), "label": "Main", "appearanceId": 0 } ],
                });
                send_later(session.clone(), Duration::ZERO, accounts);
            }
            SessionEvent::StateChanged(_) => {}
            SessionEvent::Message(text) => {
                let request: Value = serde_json::from_str(&text).unwrap();
                received.lock().unwrap().push(request.clone());
                match handler(&wallet, &request) {
                    Reply::Now(answer) => send_later(session.clone(), Duration::ZERO, answer),
                    Reply::After(delay, answer) => send_later(session.clone(), delay, answer),
                    Reply::Nothing => {}
                }
            }
        }
    }
}

fn send_later(session: Arc<Session>, delay: Duration, message: Value) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        // A send that finds no connection is what a wallet does when the connector is gone.
        let _ = session.send(&message.to_string()).await;
    });
}

/// A connected simulated wallet.
pub struct RunningWallet {
    pub session: Arc<Session>,
    received: Arc<Mutex<Vec<Value>>>,
    reading: Arc<tokio::sync::watch::Sender<bool>>,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningWallet {
    pub async fn wait_connected(&self) {
        wait_connected(&self.session).await;
    }

    /// Every request this session received, in order.
    pub fn received(&self) -> Vec<Value> {
        self.received.lock().unwrap().clone()
    }

    /// How many times the request `interaction_id` arrived.
    pub fn count(&self, interaction_id: &str) -> usize {
        self.received()
            .iter()
            .filter(|request| request["interactionId"] == interaction_id)
            .count()
    }

    /// Waits until `n` requests have arrived; returns them.
    pub async fn wait_for_requests(&self, n: usize) -> Vec<Value> {
        tokio::time::timeout(WAIT, async {
            loop {
                let received = self.received();
                if received.len() >= n {
                    return received;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the requests reach the simulated wallet")
    }

    /// Stops (or resumes) taking messages from the session.
    pub fn set_reading(&self, reading: bool) {
        self.reading.send_replace(reading);
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Sends a message now, as the wallet does when the person decides.
    pub async fn send(&self, message: &Value) {
        self.session
            .send(&message.to_string())
            .await
            .expect("the connector takes it");
    }

    /// Closes the session, as when the app is killed.
    pub async fn close(self) {
        self.session.close().await;
        self.task.abort();
    }
}

/// A handler that answers ROLA requests at once.
pub fn answers_rola() -> Handler {
    Arc::new(|wallet: &SimWallet, request: &Value| Reply::Now(wallet.rola_answer(request)))
}

/// A handler that records requests and never answers.
pub fn never_answers() -> Handler {
    Arc::new(|_: &SimWallet, _: &Value| Reply::Nothing)
}

/// Collects the progress of a call, to check after it.
#[derive(Clone, Default)]
pub struct Steps(Arc<Mutex<Vec<Progress>>>);

impl Steps {
    pub fn observer(&self) -> impl FnMut(Progress) + Send + 'static {
        let steps = self.0.clone();
        move |step| steps.lock().unwrap().push(step)
    }

    pub fn all(&self) -> Vec<Progress> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, matches: impl Fn(&Progress) -> bool) -> usize {
        self.all().iter().filter(|step| matches(step)).count()
    }
}
