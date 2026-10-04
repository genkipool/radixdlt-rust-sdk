//! radixdlt-connect — The Radix Connect protocol in native Rust (signaling +
//! WebRTC).
//!
//! A native replacement for the Node connector (`@radixdlt/radix-connect-webrtc` +
//! `@roamhq/wrtc`): pair with the mobile wallet, open a WebRTC channel and exchange
//! wallet interactions (ROLA account proofs, transactions, pre-authorizations).
//!
//! The entry point is [`Connector`], which carries the ICE/signaling configuration.
//! By default it uses the public Radix ICE set and signaling server; override the
//! ICE servers with [`Connector::with_ice_servers`] to use your own TURN relay.
//!
//! The wallet-interaction message schema is shared with the Iroh transport via
//! [`radixdlt_connect_types`] (re-exported here), so both transports speak exactly
//! the same JSON.
//!
//! This is a pure library: it never prints. User-facing error text is localized to
//! the system language.
//!
//! ## One conversation per link
//!
//! A paired link carries ONE conversation at a time: every request opens a fresh channel
//! through the signaling server keyed by the link password, so two in flight at once race on
//! the same rendezvous and the second fails within seconds without the wallet ever prompting.
//! The request methods therefore QUEUE on the link (keyed by the password, process-wide), and
//! waiting counts against the timeout the caller passed. A caller that will not wait gets
//! [`ConnectError::LinkBusy`] rather than a failure that looks like an unresponsive wallet.
//! Callers need no lock of their own — including those that build a [`Connector`] per call.

// In TESTS a panic IS the failure mechanism. Library code keeps the deny: a panic there is
// taken in the CONSUMER's process, which they neither chose nor can catch.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod chunking;
mod connector;
pub mod crypto;
mod error;
/// A local signaling relay, for reaching the phone with no internet (over a cable or a local Wi-Fi).
pub mod relay;
mod signaling;
pub mod state;
/// A minimal TURN-over-TCP server, for a phone whose only network is the USB cable (`adb reverse`).
pub mod turn_server;
/// A TURN allocation reached over TCP/TLS, presented to WebRTC as a UDP socket.
pub mod turn_tcp;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub use connector::{probe_relay_candidates, radix_default_ice_servers, Channel, IceServer};
pub use error::ConnectError;
pub use radixdlt_connect_types::{
    account_proof_request, account_proof_request_sharing, account_request, check_failure, extract_accounts,
    extract_login, extract_persona_email, extract_persona_name, extract_proofs,
    extract_signed_partial_transaction, extract_transaction_intent_hash, login_request,
    pre_authorization_request, transaction_request, DappContext, PersonaRequest, WalletInteractionError,
};
pub use radixdlt_connect_types::{
    authorized_request, extract_ongoing_accounts, extract_ownership_proofs, extract_persona_phones,
    ownership_request, unauthorized_request, AccountsWanted, Auth, AuthorizedRequest, OwnershipWanted,
    PersonaDataWanted, Quantity,
};
pub use signaling::SIGNALING_BASE;
pub use state::LinkState;
pub use turn_tcp::{TurnTcpRuntime, TurnTcpServer};

/// Waits for a `linkClient` message from the wallet (pairing) on an established
/// channel. Returns `(walletPublicKey, signatureHex)`.
pub async fn await_link_client(
    channel: &mut Channel,
    wait: Duration,
) -> Result<(String, Option<String>), ConnectError> {
    loop {
        let msg = channel.recv_message(wait).await?;
        if msg.get("discriminator").and_then(|d| d.as_str()) == Some("linkClient") {
            let pk = msg
                .get("publicKey")
                .and_then(|p| p.as_str())
                .ok_or_else(|| ConnectError::Protocol("linkClient without publicKey".into()))?;
            let sig = msg
                .get("signature")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            return Ok((pk.to_string(), sig));
        }
    }
}

/// One step in the life of a request, reported as it happens to [`Connector::exchange`]'s observer.
///
/// The steps tell apart failures that otherwise look identical from the sending side. A request
/// that never reached [`Progress::Delivered`] is NOT in the wallet: sending it again is safe. One
/// that did is in the wallet's queue until the person approves or rejects it (or the wallet app is
/// closed, which empties that queue), and sending more only stacks them behind it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Progress {
    /// The link's turn was taken, after waiting this long behind another request on it.
    TurnTaken {
        /// Time spent queued behind another request on the same link.
        waited: Duration,
    },
    /// The data channel to the wallet is open: the wallet app is running and reachable.
    ChannelOpen {
        /// Time since the turn was taken.
        elapsed: Duration,
    },
    /// The wallet confirmed it RECEIVED the request. From here on it sits in the wallet's queue.
    Delivered {
        /// Time since the turn was taken.
        elapsed: Duration,
    },
    /// A message that answers ANOTHER interaction — typically an earlier request that timed out
    /// here and that the person approved or rejected later on the phone.
    OtherMessage(Value),
    /// The channel was lost AFTER delivery, so the request is still on the phone: a new channel
    /// is being opened to keep waiting for its answer (nothing is sent again).
    Reconnecting {
        /// Why the previous channel ended.
        reason: String,
    },
    /// The wallet never confirmed receiving the request, so it is NOT on the phone: it is being
    /// sent again, with the same interaction id, on a fresh channel.
    Resending {
        /// Which attempt this is (2 = the first resend).
        attempt: u32,
        /// Why the previous attempt did not get through.
        reason: String,
    },
    /// Waiting for the wallet to finish tearing down the link's previous channel before opening
    /// a new one (see [`WALLET_SETTLE`]).
    Settling {
        /// How long.
        wait: Duration,
    },
}

/// How long the wallet takes to notice that a channel closed — and why a new one must wait.
///
/// The Radix Wallet keeps ONE data channel per paired link, keyed by the link, not by the
/// connection. When it notices that a previous connection went away (about five seconds after it
/// did), it tears down whatever channel the link has AT THAT MOMENT. A channel opened sooner than
/// that is the one torn down: the request on it reaches the phone, but its answer has nowhere to
/// go and is lost, and the sender sees a channel that died a few seconds in. Observed on a real
/// phone: a channel opened 0.6 s after the previous one closed died 5.4 s later.
pub const WALLET_SETTLE: Duration = Duration::from_secs(8);

/// How long after a channel opens before sending on it — see [`Connector::exchange`].
const SEND_GRACE: Duration = Duration::from_millis(600);
/// How long the wallet has to confirm it received a request before it is sent again.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(6);
/// How many times a request the wallet never confirmed is sent, in all.
pub const SEND_ATTEMPTS: u32 = 3;

/// Waits on `channel` for the message whose `interactionId` is `want_id`, until `deadline`.
///
/// The wallet's dAppRequestQueue can hold stale requests from earlier attempts;
/// their responses arrive first and would otherwise be mistaken for ours (e.g. a
/// "response without oneTimeAccounts" on an account-proof request). Requiring an
/// EXACT id match keeps us waiting for the user's actual approval. Those other answers
/// are not thrown away silently: each one goes to `on` as [`Progress::OtherMessage`],
/// because an earlier transaction approved late WAS submitted, and the caller has to know.
async fn listen(
    channel: &mut Channel,
    want_id: &str,
    deadline: Instant,
    on: &mut (dyn FnMut(Progress) + Send),
) -> Result<Value, ConnectError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let resp = channel.recv_message(remaining).await?;
        let got = resp.get("interactionId").and_then(|v| v.as_str()).unwrap_or("");
        if want_id.is_empty() || got == want_id {
            return Ok(resp);
        }
        on(Progress::OtherMessage(resp));
    }
}

mod shared_turn;

/// A channel that ended under us (the wallet closed it, or its transport failed), as opposed
/// to a timeout or an answer.
fn channel_lost(error: &ConnectError) -> bool {
    matches!(error, ConnectError::SignalingClosed | ConnectError::WebRtc(_))
}

/// When each link's last channel closed, keyed like [`link_turn`].
fn last_closed() -> &'static std::sync::Mutex<std::collections::HashMap<u64, Instant>> {
    static CLOSED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, Instant>>> =
        std::sync::OnceLock::new();
    CLOSED.get_or_init(Default::default)
}

/// Records, when dropped, that a channel on this link just closed — in this process and, through
/// the shared directory, for every other process using the link.
struct ClosesLink(u64, String);

impl ClosesLink {
    fn of(password: &[u8]) -> Self {
        Self(link_key(password), shared_turn::key(password))
    }
}

impl Drop for ClosesLink {
    fn drop(&mut self) {
        let mut closed = last_closed().lock().unwrap_or_else(|e| e.into_inner());
        closed.insert(self.0, Instant::now());
        shared_turn::note_closed(&self.1);
    }
}

/// How long to wait before opening a channel on this link: what is left of [`WALLET_SETTLE`]
/// since its last channel closed.
fn settle_wait(password: &[u8]) -> Duration {
    let here = {
        let closed = last_closed().lock().unwrap_or_else(|e| e.into_inner());
        closed
            .get(&link_key(password))
            .map(|at| WALLET_SETTLE.saturating_sub(at.elapsed()))
            .unwrap_or_default()
    };
    // Another process may have closed a channel on this link more recently.
    let elsewhere = shared_turn::since_closed(password)
        .map(|ago| WALLET_SETTLE.saturating_sub(ago))
        .unwrap_or_default();
    here.max(elsewhere)
}

fn link_key(password: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    password.hash(&mut hasher);
    hasher.finish()
}

/// A link's turn, held for one conversation: this process's and the machine's.
#[derive(Debug)]
struct Turn {
    _here: tokio::sync::OwnedMutexGuard<()>,
    _machine: shared_turn::SharedTurn,
}

/// The turn-taking mutex for one paired link, created on first use and shared process-wide.
///
/// Keyed by a HASH of the password rather than the password itself: the registry is a
/// `static` that lives for the whole process, and it has no business holding copies of link
/// secrets. A hash collision would merely make two unrelated links take turns — slower, never
/// wrong — which is the safe direction for the mistake to fall.
fn link_turn(password: &[u8]) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    static LINKS: OnceLock<Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let key = link_key(password);
    let links = LINKS.get_or_init(|| Mutex::new(HashMap::new()));
    // A poisoned registry only means some other thread panicked while holding it; the map
    // itself is still sound, so recover rather than propagate an unrelated panic.
    let mut links = links.lock().unwrap_or_else(|e| e.into_inner());
    links.entry(key).or_default().clone()
}

/// Where extra signaling relays are configured, besides [`Connector::with_extra_signaling`]:
/// the `RADIX_CONNECT_RELAYS` environment variable (URLs separated by commas or spaces), and
/// the files `~/.config/radix-connect/relays` and `/etc/radix-connect/relays` (one URL per
/// line, `#` for comments). Read when a [`Connector`] is created, so every program built on this
/// crate — and every user on the machine, through the `/etc` file — reaches a local relay with no
/// code of its own.
pub const RELAYS_ENV: &str = "RADIX_CONNECT_RELAYS";

/// The extra signaling relays configured on this machine (see [`RELAYS_ENV`]).
#[must_use]
pub fn configured_relays() -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut add = |text: &str| {
        for item in text
            .lines()
            .map(|line| line.split('#').next().unwrap_or_default())
            .flat_map(|line| line.split([',', ' ', '\t']))
            .map(str::trim)
            .filter(|item| item.starts_with("ws://") || item.starts_with("wss://"))
        {
            let base = normalize_base(item);
            if !found.contains(&base) {
                found.push(base);
            }
        }
    };
    if let Ok(env) = std::env::var(RELAYS_ENV) {
        add(&env);
    }
    let user_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")));
    let files = [
        user_config.map(|dir| dir.join("radix-connect").join("relays")),
        Some(std::path::PathBuf::from("/etc/radix-connect/relays")),
    ];
    for file in files.into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(file) {
            add(&text);
        }
    }
    found
}

/// The public Radix signaling server, then every configured relay.
fn default_signaling_bases() -> Vec<String> {
    let mut bases = vec![SIGNALING_BASE.to_string()];
    for relay in configured_relays() {
        if !bases.contains(&relay) {
            bases.push(relay);
        }
    }
    bases
}

/// A base URL without the trailing slash the wallet's settings want (`ws://h:8787/`): the
/// connection id is appended after a `/` of our own.
fn normalize_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_string()
}

/// Of the reasons every signaling server failed, the one that says most. A server that could
/// not be reached at all (no internet) explains nothing when another one WAS reached and the
/// wallet simply never came.
fn most_telling(errors: Vec<ConnectError>) -> ConnectError {
    let unreachable =
        |e: &ConnectError| matches!(e, ConnectError::Signaling(_) | ConnectError::SignalingClosed);
    let mut errors = errors.into_iter();
    let first = errors.next().unwrap_or(ConnectError::SignalingClosed);
    if !unreachable(&first) {
        return first;
    }
    errors.find(|e| !unreachable(e)).unwrap_or(first)
}

/// A Radix Connect client carrying the ICE/signaling configuration.
pub struct Connector {
    ice_servers: Vec<IceServer>,
    signaling_bases: Vec<String>,
    relay_only: bool,
    turn_tcp: Option<TurnTcpServer>,
}

impl Default for Connector {
    fn default() -> Self {
        Connector {
            ice_servers: radix_default_ice_servers(),
            signaling_bases: default_signaling_bases(),
            relay_only: false,
            turn_tcp: None,
        }
    }
}

impl Connector {
    /// A connector with the default public Radix ICE set and signaling server.
    pub fn new() -> Self {
        Connector::default()
    }

    /// Overrides the ICE (STUN/TURN) servers (e.g. to use your own TURN relay).
    pub fn with_ice_servers(mut self, servers: Vec<IceServer>) -> Self {
        self.ice_servers = servers;
        self
    }

    /// Uses ONLY this signaling server (instead of the public one and any configured relays).
    pub fn with_signaling_base(mut self, base: impl Into<String>) -> Self {
        self.signaling_bases = vec![normalize_base(&base.into())];
        self
    }

    /// Also tries this signaling server — e.g. a local [`relay`] the wallet reaches over a cable.
    /// Every channel is attempted through all of them at once, and the first the wallet answers
    /// on wins: the wallet only listens on the ONE server its current profile names, so the same
    /// connector works whether the phone is online or on a cable with no internet.
    pub fn with_extra_signaling(mut self, base: impl Into<String>) -> Self {
        let base = normalize_base(&base.into());
        if !self.signaling_bases.contains(&base) {
            self.signaling_bases.push(base);
        }
        self
    }

    /// The signaling servers this connector tries, in order.
    pub fn signaling_bases(&self) -> &[String] {
        &self.signaling_bases
    }

    /// Restricts ICE to relay candidates only.
    ///
    /// Host and server-reflexive candidates are not gathered, so the connection goes through
    /// the TURN allocation and nothing else. Combined with a TURN server reached over TCP
    /// (`turns:…?transport=tcp`) the process opens no UDP socket at all, which is what a host
    /// that forbids UDP requires. It costs latency and relay bandwidth, so it is off by
    /// default: the default ICE set already falls back to the relay when direct paths fail.
    pub fn with_relay_only(mut self, relay_only: bool) -> Self {
        self.relay_only = relay_only;
        self
    }

    /// Reaches the wallet through a TURN relay over TCP/TLS, opening no UDP socket at all.
    ///
    /// For hosts that do not offer UDP: corporate networks that block it, and most
    /// serverless platforms. The allocation is made before the peer connection exists and
    /// stands in for its socket, so ICE has a public address to offer and never discovers
    /// there is a relay beneath it. See [`turn_tcp`] for the shape of that.
    ///
    /// It supersedes both [`with_ice_servers`](Self::with_ice_servers) and
    /// [`with_relay_only`](Self::with_relay_only): with the relay underneath, there is
    /// nothing left for ICE to gather. Slower than a direct path and every byte crosses the
    /// relay, so reach for it when UDP is unavailable rather than by default.
    pub fn with_turn_tcp(mut self, server: TurnTcpServer) -> Self {
        self.turn_tcp = Some(server);
        self
    }

    async fn establish(&self, password: &[u8], open_timeout: Duration) -> Result<Channel, ConnectError> {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        let attempt = |base: &str| {
            let base = base.to_string();
            async move {
                connector::establish(
                    &self.ice_servers,
                    &base,
                    password,
                    open_timeout,
                    self.relay_only,
                    self.turn_tcp.as_ref(),
                )
                .await
            }
        };
        if let [only] = self.signaling_bases.as_slice() {
            return attempt(only).await;
        }
        let mut attempts: FuturesUnordered<_> = self.signaling_bases.iter().map(|b| attempt(b)).collect();
        let mut errors = Vec::new();
        while let Some(result) = attempts.next().await {
            match result {
                Ok(channel) => return Ok(channel),
                Err(error) => errors.push(error),
            }
        }
        Err(most_telling(errors))
    }

    /// Takes this link's turn, so only ONE conversation runs on it at a time.
    ///
    /// Every request opens a fresh channel through the signaling server keyed by the LINK
    /// PASSWORD. Two requests in flight on the same link therefore race on the same
    /// rendezvous: in practice the second one dies within seconds and the wallet never even
    /// shows a prompt, so the caller sees an unexplained failure. The constraint belongs to
    /// the link, not to a `Connector` value — callers routinely build one per call
    /// (`Connector::new().request_…()`) — so the turn is keyed by the password itself and
    /// shared process-wide.
    ///
    /// Waiting counts against `budget`; the remaining time is returned for the request
    /// itself, so a queued caller can never exceed the deadline it asked for. When the queue
    /// does not clear in time the caller gets [`ConnectError::LinkBusy`], which says what
    /// happened instead of looking like an unresponsive wallet.
    async fn take_turn(password: &[u8], budget: Duration) -> Result<(Turn, Duration), ConnectError> {
        let turn = link_turn(password);
        let started = Instant::now();
        let guard = tokio::time::timeout(budget, turn.lock_owned())
            .await
            .map_err(|_| ConnectError::LinkBusy)?;
        // Then the machine-wide turn, so another PROCESS on this link waits too.
        let shared = shared_turn::take(password, started + budget)
            .await
            .map_err(|()| ConnectError::LinkBusy)?;
        let left = budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(ConnectError::LinkBusy);
        }
        Ok((
            Turn {
                _here: guard,
                _machine: shared,
            },
            left,
        ))
    }

    /// Sends ANY wallet interaction and returns the wallet's answer to it, reporting each step to
    /// `on` as it happens ([`Progress`]). Every `request_*` method is this with a built request.
    ///
    /// Use it when the caller must know HOW FAR a request got — above all whether the wallet
    /// received it ([`Progress::Delivered`]) — or must not lose the late answers to earlier
    /// requests that arrive on the same channel ([`Progress::OtherMessage`]). Dropping the future
    /// (a cancelled task, a `select!`) closes the channel to the wallet.
    ///
    /// # Errors
    /// As [`request_account_proof_sharing`](Self::request_account_proof_sharing). The answer may
    /// still be a wallet `failure`: read it with the `extract_*` functions or
    /// [`check_failure`].
    pub async fn exchange(
        &self,
        password: &[u8],
        interaction: &Value,
        overall_timeout: Duration,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Value, ConnectError> {
        let want_id = interaction
            .get("interactionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.converse(password, Some(interaction), &want_id, overall_timeout, on)
            .await
    }

    /// Listens for the answer to a request sent EARLIER (by `interaction_id`) without sending
    /// anything: the request is already in the wallet's queue, and sending it again would only
    /// queue a second copy behind it. The wallet answers on whatever channel the link has open
    /// when the person decides, so this opens one and waits.
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange); [`ConnectError::ResponseTimeout`] when nobody answered.
    pub async fn await_response(
        &self,
        password: &[u8],
        interaction_id: &str,
        overall_timeout: Duration,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Value, ConnectError> {
        self.converse(password, None, interaction_id, overall_timeout, on)
            .await
    }

    /// One conversation on the link: take the turn, let the wallet settle, open a channel, send
    /// (unless only listening), and wait for the answer.
    ///
    /// Two things are retried, because both are the wallet's transport and not the person:
    /// a request the wallet never CONFIRMED receiving is sent again (same interaction id) on a
    /// fresh channel, up to [`SEND_ATTEMPTS`] times; and a channel lost AFTER delivery is reopened
    /// to keep listening, since the request is then on the phone and its answer will go to
    /// whatever channel the link has when the person decides. Nothing confirmed is ever resent.
    async fn converse(
        &self,
        password: &[u8],
        interaction: Option<&Value>,
        want_id: &str,
        overall_timeout: Duration,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Value, ConnectError> {
        let queued = Instant::now();
        let (_turn, budget) = Self::take_turn(password, overall_timeout).await?;
        on(Progress::TurnTaken {
            waited: queued.elapsed(),
        });
        let started = Instant::now();
        let deadline = started + budget;
        let _closes = ClosesLink::of(password);
        let mut delivered = interaction.is_none();
        let mut attempt = 1u32;
        loop {
            self.settle(password, deadline, on).await?;
            let mut channel = self
                .establish(password, deadline.saturating_duration_since(Instant::now()))
                .await?;
            on(Progress::ChannelOpen {
                elapsed: started.elapsed(),
            });
            if let (Some(interaction), false) = (interaction, delivered) {
                // The wallet starts reading a new channel only once it has seen the connection
                // come up on ITS side, a moment after ours: a message sent into that gap is lost
                // without a trace. A short grace closes the gap.
                tokio::time::sleep(SEND_GRACE).await;
                match channel.send_message(interaction, CONFIRM_TIMEOUT).await {
                    Ok(()) => {
                        delivered = true;
                        on(Progress::Delivered {
                            elapsed: started.elapsed(),
                        });
                    }
                    Err(error)
                        if (matches!(error, ConnectError::ConfirmationTimeout) || channel_lost(&error))
                            && attempt < SEND_ATTEMPTS
                            && deadline.saturating_duration_since(Instant::now())
                                > WALLET_SETTLE + Duration::from_secs(10) =>
                    {
                        attempt += 1;
                        on(Progress::Resending {
                            attempt,
                            reason: error.to_string(),
                        });
                        drop(channel);
                        drop(ClosesLink::of(password));
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            match listen(&mut channel, want_id, deadline, on).await {
                Err(error)
                    if channel_lost(&error)
                        && deadline.saturating_duration_since(Instant::now())
                            > WALLET_SETTLE + Duration::from_secs(10) =>
                {
                    on(Progress::Reconnecting {
                        reason: error.to_string(),
                    });
                    drop(channel);
                    drop(ClosesLink::of(password));
                }
                other => return other,
            }
        }
    }

    /// Waits out what is left of [`WALLET_SETTLE`] since the link's last channel closed.
    async fn settle(
        &self,
        password: &[u8],
        deadline: Instant,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(), ConnectError> {
        let wait = settle_wait(password);
        if wait.is_zero() {
            return Ok(());
        }
        if Instant::now() + wait >= deadline {
            return Err(ConnectError::LinkBusy);
        }
        on(Progress::Settling { wait });
        tokio::time::sleep(wait).await;
        Ok(())
    }

    /// Opens a channel to the wallet and closes it again, sending nothing: whether the wallet app
    /// is reachable right now, and how long the channel took. Nothing appears on the phone.
    ///
    /// # Errors
    /// [`ConnectError::ChannelTimeout`] when the wallet did not connect in time (app closed, no
    /// network, or this connector no longer linked), plus the transport errors of
    /// [`exchange`](Self::exchange).
    pub async fn probe(&self, password: &[u8], overall_timeout: Duration) -> Result<Duration, ConnectError> {
        let (_turn, budget) = Self::take_turn(password, overall_timeout).await?;
        let deadline = Instant::now() + budget;
        let _closes = ClosesLink::of(password);
        self.settle(password, deadline, &mut |_| {}).await?;
        let started = Instant::now();
        let channel = self
            .establish(password, deadline.saturating_duration_since(Instant::now()))
            .await?;
        let took = started.elapsed();
        drop(channel);
        Ok(took)
    }

    /// With an already-paired link password, asks the wallet to sign a ROLA account
    /// proof and returns the wallet's response (containing `proofs`).
    pub async fn request_account_proof(
        &self,
        password: &[u8],
        challenge_hex: &str,
        ctx: &DappContext,
        request_persona: bool,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let share = if request_persona {
            PersonaRequest::NAME
        } else {
            PersonaRequest::NONE
        };
        self.request_account_proof_sharing(password, challenge_hex, ctx, share, overall_timeout)
            .await
    }

    /// The same, saying exactly what the person is asked to share about themselves. Nothing
    /// shared changes the proof: the signature is what the caller verifies, and what the person
    /// declines to share simply is not in the answer.
    pub async fn request_account_proof_sharing(
        &self,
        password: &[u8],
        challenge_hex: &str,
        ctx: &DappContext,
        share: PersonaRequest,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let interaction = account_proof_request_sharing(challenge_hex, ctx, share);
        self.exchange(password, &interaction, overall_timeout, &mut |_| {})
            .await
    }

    /// Asks the wallet to LOG IN: the persona signs the challenge too, so the person is proven.
    ///
    /// [`request_account_proof_sharing`](Self::request_account_proof_sharing) proves an ACCOUNT and
    /// can ask for a name — but a name is a string somebody typed, and the answer never names the
    /// persona at all. This asks for an `authorizedRequest` with `loginWithChallenge`, whose answer
    /// carries the identity address and a proof over it. Read it with
    /// [`extract_login`]; the account proofs are in the same
    /// answer, over the same challenge, so one approval on the phone covers both.
    ///
    /// # Errors
    /// As [`request_account_proof_sharing`](Self::request_account_proof_sharing).
    pub async fn request_login(
        &self,
        password: &[u8],
        challenge_hex: &str,
        ctx: &DappContext,
        share: PersonaRequest,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let interaction = login_request(challenge_hex, ctx, share);
        self.exchange(password, &interaction, overall_timeout, &mut |_| {})
            .await
    }

    /// Sends any AUTHORIZED request ([`AuthorizedRequest`]): a persona — logging in, or one already
    /// logged in — and whatever else is asked in the same approval (proof of ownership, ongoing or
    /// one-time accounts and persona data, a reset). Returns the raw answer, read with the
    /// `extract_*` functions.
    ///
    /// # Errors
    /// As [`request_account_proof_sharing`](Self::request_account_proof_sharing).
    pub async fn request_authorized(
        &self,
        password: &[u8],
        request: &AuthorizedRequest,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let interaction = authorized_request(request, ctx);
        self.exchange(password, &interaction, overall_timeout, &mut |_| {})
            .await
    }

    /// Asks the wallet to prove EXACT accounts (and the persona, with `own.identity`) as a persona
    /// already logged in to this dApp (`identity`) — one confirmation, nothing to pick. Read the
    /// proofs with [`extract_ownership_proofs`].
    ///
    /// # Errors
    /// As [`request_account_proof_sharing`](Self::request_account_proof_sharing).
    pub async fn request_ownership(
        &self,
        password: &[u8],
        identity: &str,
        own: &OwnershipWanted,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let request = AuthorizedRequest {
            proof_of_ownership: Some(own.clone()),
            ..AuthorizedRequest::new(Auth::UsePersona(identity.to_string()))
        };
        self.request_authorized(password, &request, ctx, overall_timeout)
            .await
    }

    /// Sends any UNAUTHORIZED request: one-time accounts and/or persona data, with exact or
    /// minimum quantities and phone numbers.
    ///
    /// # Errors
    /// As [`request_account_proof_sharing`](Self::request_account_proof_sharing).
    pub async fn request_unauthorized(
        &self,
        password: &[u8],
        accounts: Option<&AccountsWanted>,
        persona_data: Option<PersonaDataWanted>,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let interaction = unauthorized_request(accounts, persona_data, ctx);
        self.exchange(password, &interaction, overall_timeout, &mut |_| {})
            .await
    }

    /// Asks the wallet to SHARE its account(s) without a ROLA proof (the
    /// lightweight account-discovery flow). Returns the raw response; read the
    /// addresses with [`extract_accounts`].
    pub async fn request_accounts(
        &self,
        password: &[u8],
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let interaction = account_request(ctx);
        self.exchange(password, &interaction, overall_timeout, &mut |_| {})
            .await
    }

    /// Sends a TRANSACTION MANIFEST to the wallet for the owner to sign and submit.
    /// Returns the `transactionIntentHash` on success.
    pub async fn request_transaction(
        &self,
        password: &[u8],
        manifest: &str,
        message: &str,
        blobs: &[String],
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<String, ConnectError> {
        let interaction = transaction_request(manifest, message, blobs, ctx);
        let response = self
            .exchange(password, &interaction, overall_timeout, &mut |_| {})
            .await?;
        Ok(extract_transaction_intent_hash(&response)?)
    }

    /// Asks the mobile wallet for a PRE-AUTHORIZATION (subintent V2): the user
    /// approves a `subintentManifest` and the wallet returns a
    /// `signedPartialTransaction` (hex) WITHOUT submitting it.
    pub async fn request_pre_authorization(
        &self,
        password: &[u8],
        subintent_manifest: &str,
        message: &str,
        expire_after_seconds: u64,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<String, ConnectError> {
        let interaction = pre_authorization_request(subintent_manifest, message, expire_after_seconds, ctx);
        let response = self
            .exchange(password, &interaction, overall_timeout, &mut |_| {})
            .await?;
        Ok(extract_signed_partial_transaction(&response)?)
    }

    /// Pairing: generates the QR payload (signed with the connector identity),
    /// establishes the channel with the scanning wallet, waits for its `linkClient`,
    /// verifies its signature and returns `(walletPublicKey, password_bytes)` to
    /// persist the link.
    ///
    /// `identity_private_key_hex` is the connector's persistent Ed25519 private key.
    /// `on_qr` receives the exact QR JSON string to render.
    pub async fn pair<F: FnOnce(String)>(
        &self,
        identity_private_key_hex: &str,
        identity_public_key_hex: &str,
        on_qr: F,
        timeout: Duration,
    ) -> Result<(String, Vec<u8>), ConnectError> {
        use ed25519_dalek::{Signer, SigningKey};

        // New 32-byte link password.
        let mut password = [0u8; 32];
        getrandom::fill(&mut password)
            .map_err(|e| ConnectError::Crypto(format!("system randomness: {e}")))?;

        // Connector identity signs blake2b("L"‖password).
        let sk_bytes = hex::decode(identity_private_key_hex)
            .map_err(|e| ConnectError::Crypto(format!("priv hex: {e}")))?;
        let sk_arr: [u8; 32] = sk_bytes
            .as_slice()
            .try_into()
            .map_err(|_| ConnectError::Crypto("private key is not 32 bytes".into()))?;
        let signing = SigningKey::from_bytes(&sk_arr);
        let link_msg = crypto::linking_message(&password);
        let signature = hex::encode(signing.sign(&link_msg).to_bytes());

        let qr = json!({
            "password": hex::encode(password),
            "publicKey": identity_public_key_hex,
            "signature": signature,
            "purpose": "general",
        });
        on_qr(qr.to_string());

        // Establish the channel with the scanning wallet and await its linkClient.
        let mut channel = self.establish(&password, timeout).await?;
        let (wallet_pk, sig) = await_link_client(&mut channel, timeout).await?;

        // Verify the wallet's linking signature.
        if let Some(sig_hex) = sig {
            use ed25519_dalek::{Signature, VerifyingKey};
            let pk_bytes =
                hex::decode(&wallet_pk).map_err(|e| ConnectError::Crypto(format!("wallet pk hex: {e}")))?;
            let pk_arr: [u8; 32] = pk_bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectError::Crypto("wallet pk is not 32 bytes".into()))?;
            let vk = VerifyingKey::from_bytes(&pk_arr)
                .map_err(|e| ConnectError::Crypto(format!("invalid wallet pk: {e}")))?;
            let sig_bytes =
                hex::decode(&sig_hex).map_err(|e| ConnectError::Crypto(format!("sig hex: {e}")))?;
            let sig_arr: [u8; 64] = sig_bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectError::Crypto("signature is not 64 bytes".into()))?;
            vk.verify_strict(&link_msg, &Signature::from_bytes(&sig_arr))
                .map_err(|_| ConnectError::Crypto("INVALID wallet linking signature".into()))?;
        }

        Ok((wallet_pk, password.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two requests sharing a link must take turns: the second one only starts once the
    /// first has finished. Without this they race on the same signaling rendezvous and the
    /// second dies within seconds, with the wallet never prompting.
    #[tokio::test]
    async fn requests_on_the_same_link_take_turns() {
        let password = b"same-link-password";
        let generous = Duration::from_secs(5);

        let (first, _) = Connector::take_turn(password, generous)
            .await
            .expect("first takes the turn");

        // While the first holds it, a second one cannot get through.
        let blocked = Connector::take_turn(password, Duration::from_millis(150)).await;
        assert_eq!(
            blocked.err(),
            Some(ConnectError::LinkBusy),
            "a busy link must report LinkBusy"
        );

        // Releasing it lets the queue move.
        drop(first);
        let (_second, left) = Connector::take_turn(password, generous)
            .await
            .expect("the queue moves on release");
        assert!(left <= generous, "the budget must never grow while queuing");
    }

    /// Different links are independent: one busy phone must not stall another.
    #[tokio::test]
    async fn different_links_do_not_block_each_other() {
        let (_a, _) = Connector::take_turn(b"link-a", Duration::from_secs(5))
            .await
            .unwrap();
        let b = Connector::take_turn(b"link-b", Duration::from_millis(200)).await;
        assert!(b.is_ok(), "an unrelated link must not wait");
    }

    /// Time spent queuing is charged to the caller's budget, so a queued request can never
    /// overrun the deadline it asked for.
    #[tokio::test]
    async fn waiting_is_charged_to_the_callers_budget() {
        let password = b"budget-link";
        let (held, _) = Connector::take_turn(password, Duration::from_secs(5))
            .await
            .unwrap();
        let waiter = tokio::spawn(async move {
            Connector::take_turn(password, Duration::from_secs(2))
                .await
                .map(|(_g, left)| left)
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(held);
        let left = waiter.await.unwrap().expect("it should get the turn");
        assert!(
            left < Duration::from_secs(2),
            "the wait must come out of the budget"
        );
    }

    /// A link whose channel just closed must wait for the wallet to let go of it; one that has
    /// never had a channel, or closed long ago, must not wait at all.
    #[test]
    fn a_new_channel_waits_for_the_wallet_to_settle() {
        assert_eq!(settle_wait(b"never-used-link"), Duration::ZERO);
        drop(ClosesLink::of(b"just-closed-link"));
        let wait = settle_wait(b"just-closed-link");
        assert!(
            wait > WALLET_SETTLE - Duration::from_secs(1) && wait <= WALLET_SETTLE,
            "{wait:?}"
        );
        assert_eq!(
            settle_wait(b"another-link"),
            Duration::ZERO,
            "links settle independently"
        );
    }

    /// A relay is added once, with or without the trailing slash the wallet's settings use, and
    /// the public server stays first.
    #[test]
    fn extra_signaling_servers_are_added_once() {
        let c = Connector::new()
            .with_signaling_base(SIGNALING_BASE)
            .with_extra_signaling("ws://172.20.10.10:8787/")
            .with_extra_signaling("ws://172.20.10.10:8787");
        assert_eq!(c.signaling_bases(), [SIGNALING_BASE, "ws://172.20.10.10:8787"]);
        let only = Connector::new().with_signaling_base("ws://127.0.0.1:8787/");
        assert_eq!(only.signaling_bases(), ["ws://127.0.0.1:8787"]);
    }

    /// When the public server is unreachable (no internet) but the relay answered, the failure
    /// to report is the relay's: that is where the wallet was expected.
    #[test]
    fn the_failure_that_says_most_is_reported() {
        let offline = ConnectError::Signaling("dns".into());
        assert_eq!(
            most_telling(vec![offline.clone(), ConnectError::ChannelTimeout]),
            ConnectError::ChannelTimeout
        );
        assert_eq!(most_telling(vec![offline.clone()]), offline);
        assert_eq!(
            most_telling(vec![ConnectError::ChannelTimeout, offline]),
            ConnectError::ChannelTimeout
        );
    }

    /// Only a channel that ENDED is worth reopening; a timeout or an answer is final.
    #[test]
    fn only_a_lost_channel_is_reopened() {
        assert!(channel_lost(&ConnectError::WebRtc("closed".into())));
        assert!(channel_lost(&ConnectError::SignalingClosed));
        assert!(!channel_lost(&ConnectError::ResponseTimeout));
        assert!(!channel_lost(&ConnectError::WalletRejected(
            "rejectedByUser".into()
        )));
    }

    #[test]
    fn default_ice_set_has_stun_and_turn() {
        let servers = radix_default_ice_servers();
        assert!(
            servers.iter().any(|s| s.username.is_empty()),
            "expected at least one STUN server"
        );
        assert!(
            servers.iter().any(|s| !s.username.is_empty()),
            "expected at least one TURN server"
        );
    }

    #[test]
    fn account_proof_interaction_shape() {
        let ctx = DappContext::new(2, "account_tdx_2_x", "http://localhost");
        let v = account_proof_request("aa", &ctx, true);
        assert_eq!(v["metadata"]["networkId"], 2);
        assert_eq!(v["items"]["oneTimeAccounts"]["challenge"], "aa");
        assert!(v["items"]["oneTimePersonaData"]["isRequestingName"]
            .as_bool()
            .unwrap());
    }

    #[test]
    fn extract_proofs_reports_wallet_failure() {
        let resp = json!({ "discriminator": "failure", "error": "rejectedByUser" });
        assert_eq!(
            extract_proofs(&resp),
            Err(WalletInteractionError::WalletRejected("rejectedByUser".into()))
        );
    }
}
