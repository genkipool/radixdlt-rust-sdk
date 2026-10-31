//! The Radix **mobile wallet** over iroh: "Radix Connect over iroh v1", the protocol the Radix
//! Wallet and the Radix Connector Extension speak, from a Rust program — no browser involved.
//!
//! [`MobileWalletConnector`] plays the role the Connector Extension plays: it pairs with the
//! wallet through the same QR code, then sends it wallet interactions (ROLA logins and account
//! proofs, transactions, pre-authorizations) and returns its answers. Its API mirrors
//! `radixdlt_connect::Connector` (WebRTC) method for method, so switching transport is a matter
//! of changing the type: `pair`, `exchange` (with [`Progress`]), `await_response`, `probe` and the
//! `request_*` helpers, all built on [`radixdlt_connect_types`] — the same JSON either way.
//!
//! # How a link works over iroh
//!
//! A link is its 32-byte **password** (from pairing). Both iroh endpoint keys derive from it, so
//! only the holder of the password can reach this end, and the session admits only the link's
//! wallet. This connector keeps **one session per link**, started on first use and kept until
//! [`close_link`](MobileWalletConnector::close_link), [`close`](MobileWalletConnector::close) or
//! drop: it listens at its relay, and the wallet dials in whenever its app is open with iroh on
//! for the link. Both ends must make the **same relay choice** ([`Relays`]):
//!
//! - **Public** (the default, [`MobileWalletConnector::new`]): n0's public relays. Each end uses
//!   its closest one; this connector publishes which one — the relay URL only, never an IP
//!   address — through n0's address lookup, where the wallet finds it. Nothing to host or
//!   configure, in development or production. The wallet keeps its default relay setting.
//! - **Custom** ([`MobileWalletConnector::with_relay_url`]): one relay both ends are configured
//!   with, typically a self-hosted `iroh-relay`. Nothing is published anywhere.
//!
//! Requests are matched with answers by `interactionId`, so several can be in flight on one link.
//! Anything else the wallet sends — its `accountList` right after connecting, an answer that
//! arrived after its request timed out here — is reported to [`exchange`]'s observer as
//! [`Progress::OtherMessage`] and to [`subscribe`](MobileWalletConnector::subscribe)rs; late
//! answers are also kept (bounded) for [`await_response`](MobileWalletConnector::await_response).
//!
//! A request is sent again **only** when the session reports that nothing was sent (the wallet
//! dropped the connection just before). Once it may have reached the wallet — confirmed, or a
//! confirmation that timed out or was lost with the connection — it is never sent again: the
//! person could otherwise be asked twice, or sign a transaction twice.
//!
//! # Diagnostics
//!
//! Every timeout carries a snapshot of the link's connectivity ([`MobileError::diagnostics`]),
//! and the state reports in [`Progress`] carry one too; [`MobileWalletConnector::diagnostics`]
//! takes one at any time. Its `Display` is one log line, e.g.
//! `connector efcc89cfbf → 4d99dc649e state=connected path=relay rtt=42ms relay=https://… (connected, public)`. The
//! connector also logs through [`tracing`](https://docs.rs/tracing) (target
//! `radixdlt_connect_iroh`; the protocol's own events are under `radix_connect_iroh`): links are
//! named by a short hash of the password, messages by their size and interaction id — never the
//! password or a message's contents.
//!
//! [`exchange`]: MobileWalletConnector::exchange

#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::redundant_clone,
    clippy::todo
)]
#![deny(clippy::unwrap_used, clippy::dbg_macro, clippy::unimplemented, unsafe_code)]

mod error;
mod inbox;
mod link;
mod lock;
mod log;
mod requests;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use radix_connect_iroh::{ConnectionPassword, SendError, SessionConfig};
use radixdlt_connect_types::link_removal::{is_link_removed, link_removed_message, LINK_REMOVED};
use radixdlt_connect_types::linking::LinkClient;
use serde_json::Value;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;
use tokio::time::{sleep, sleep_until, timeout, timeout_at, Instant};
use zeroize::Zeroizing;

pub use error::MobileError;
pub use iroh::RelayUrl;
pub use radix_connect_iroh::{
    public_relay_urls, ConnectionState, Diagnostics, EndpointId, PathKind, Relays, Role,
};
/// The linking cryptography, shared with the WebRTC transport ([`radixdlt_connect_types::linking`]).
pub use radixdlt_connect_types::linking;
pub use radixdlt_connect_types::{
    account_proof_request, account_proof_request_sharing, account_request, authorized_request, check_failure,
    extract_accounts, extract_login, extract_ongoing_accounts, extract_ownership_proofs,
    extract_persona_email, extract_persona_name, extract_persona_phones, extract_proofs,
    extract_signed_partial_transaction, extract_transaction_intent_hash, is_rejection_by_person,
    login_request, ownership_request, pre_authorization_request, transaction_request, unauthorized_request,
    AccountsWanted, Auth, AuthorizedRequest, DappContext, OwnershipWanted, PersonaDataWanted, PersonaRequest,
    Quantity, WalletInteractionError,
};

use link::{Incoming, LinkId, Pending, Registry, Status, Waiter};

/// How long a call waits for the wallet before it first reports that it is waiting. Long enough
/// for a session that just started to reach its relay, so the first snapshot is not alarming.
const FIRST_WAITING_REPORT: Duration = Duration::from_secs(2);

/// How often a call still waiting for the wallet reports it, with a fresh diagnostics snapshot.
const WAITING_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Bound on starting a session for the calls that have no timeout of their own.
const SESSION_START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `pair` waits, once the wallet has linked, for it to close the connection it linked
/// over — as long as the Connector Extension waits (its `linkingGracePeriod`).
const LINKING_GRACE: Duration = Duration::from_secs(5);

/// Pause before sending again a request the session could not send at all.
const RESEND_PAUSE: Duration = Duration::from_millis(200);

/// One step in the life of a call, reported as it happens to the observer of
/// [`MobileWalletConnector::exchange`] (and of [`pair_with_progress`](MobileWalletConnector::pair_with_progress)).
///
/// As over WebRTC, the steps tell apart failures that look alike from here: a request that never
/// reached [`Progress::Delivered`] (nor [`Progress::Unconfirmed`]) is NOT on the phone, and
/// sending it again is safe.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Progress {
    /// Another process of this machine holds the link (one process per link: two would take the
    /// wallet from each other), and this call waits for it within its own time. Reported once.
    WaitingForLink {
        /// Who holds it: `process <pid> since <unix seconds>`.
        holder: String,
    },
    /// The other process let go of the link, and it is this one's now.
    LinkTaken {
        /// How long the call waited for it.
        waited: Duration,
    },
    /// The wallet is not connected: its app must be open, with iroh enabled for this link and the
    /// same relay. Reported two seconds into the wait and every ten seconds while it lasts.
    WaitingForWallet {
        /// How long the call has waited so far.
        waited: Duration,
        /// The link's connectivity now.
        diagnostics: Box<Diagnostics>,
    },
    /// The wallet is connected.
    Connected {
        /// Time since the call started.
        elapsed: Duration,
        /// The link's connectivity now (the path and its round-trip time).
        diagnostics: Box<Diagnostics>,
    },
    /// The request is being sent (`attempt` 1, or a resend after [`Progress::Resending`]).
    Sending {
        /// Which attempt this is.
        attempt: u32,
        /// Its size on the wire.
        bytes: usize,
        /// Time since the call started.
        elapsed: Duration,
    },
    /// The wallet confirmed it took the request. From here on it is on the phone.
    Delivered {
        /// Time since the call started.
        elapsed: Duration,
    },
    /// Nothing was sent — the wallet dropped the connection just before — so the request is sent
    /// again, with the same interaction id, once the wallet is back.
    Resending {
        /// Which attempt this is (2 = the first resend).
        attempt: u32,
        /// Why the previous attempt sent nothing.
        reason: String,
    },
    /// The wallet did not confirm the request, which may or may not have reached it. It is NOT
    /// sent again; the call keeps waiting for an answer until its timeout.
    Unconfirmed {
        /// What went wrong while sending.
        reason: String,
    },
    /// The wallet disconnected while the call waited for its answer. The request stays on the
    /// phone; its answer arrives when the wallet connects again.
    WalletDisconnected {
        /// The link's connectivity now.
        diagnostics: Box<Diagnostics>,
    },
    /// A message that answers no request in flight — the wallet's `accountList`, or the late
    /// answer to an earlier request (also kept for [`MobileWalletConnector::await_response`]).
    OtherMessage(Value),
}

/// The messages a link receives that no request claimed ([`MobileWalletConnector::subscribe`]).
#[derive(Debug)]
pub struct WalletMessages {
    link: LinkId,
    messages: broadcast::Receiver<Incoming>,
    status: watch::Receiver<Status>,
}

impl WalletMessages {
    /// The next message, or `None` once the link is closed. A subscriber that falls more than 64
    /// messages behind skips the oldest ones.
    pub async fn recv(&mut self) -> Option<Value> {
        loop {
            tokio::select! {
                biased;
                message = self.messages.recv() => match message {
                    Ok(incoming) => return Some(incoming.message),
                    Err(RecvError::Lagged(skipped)) => log::skipped(self.link, skipped),
                    Err(RecvError::Closed) => return None,
                },
                ended = self.status.wait_for(|status| *status == Status::Ended) => {
                    let _ = ended;
                    return None;
                }
            }
        }
    }
}

/// Talks to the Radix mobile wallet over "Radix Connect over iroh v1", playing the connector
/// role (see the [module documentation](self)).
///
/// Thread-safe: every method takes `&self`, so share one connector (in an `Arc`) between the
/// tasks of a server. Configure it before first use; a session already started keeps the
/// settings it started with.
pub struct MobileWalletConnector {
    relays: Relays,
    ack_timeout: Option<Duration>,
    max_message_size: Option<usize>,
    links: Registry,
    /// One process per link on this machine (`lock`).
    locks: lock::LinkLocks,
    /// The relay each link's wallet was last seen on, as the host remembers it: a session that
    /// starts knocks there, and a wallet that is up dials at once (see [`Self::set_wallet_relay`]).
    wallet_relays: std::sync::Mutex<std::collections::HashMap<LinkId, String>>,
}

impl fmt::Debug for MobileWalletConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MobileWalletConnector")
            .field("relays", &self.relays)
            .field("ack_timeout", &self.ack_timeout)
            .field("max_message_size", &self.max_message_size)
            .field("running_links", &self.links.running())
            .finish()
    }
}

impl Default for MobileWalletConnector {
    fn default() -> Self {
        MobileWalletConnector {
            relays: Relays::Public,
            ack_timeout: None,
            max_message_size: None,
            links: Registry::default(),
            locks: lock::LinkLocks::default(),
            wallet_relays: std::sync::Mutex::default(),
        }
    }
}

impl Drop for MobileWalletConnector {
    /// Closes every session, telling the wallet, in the background when a tokio runtime is
    /// running (otherwise they stop abruptly). Call [`close`](Self::close) to wait for it.
    fn drop(&mut self) {
        link::close_in_background(self.links.take_all());
    }
}

impl MobileWalletConnector {
    /// A connector on n0's public relays ([`Relays::Public`]): the wallet finds it through n0's
    /// address lookup, with no relay to run or configure on either end.
    #[must_use]
    pub fn new() -> Self {
        MobileWalletConnector::default()
    }

    /// Keeps the locks of this machine's links (one process per link) in `dir` instead of the
    /// user's runtime directory. Every process that may use the same links must use the same one.
    #[must_use]
    pub fn with_link_lock_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.locks = lock::LinkLocks::in_dir(dir);
        self
    }

    /// No lock per link: for a host that already keeps one process per link itself. Two
    /// processes using one link at once take the wallet from each other.
    #[must_use]
    pub fn without_link_locks(mut self) -> Self {
        self.locks = lock::LinkLocks::none();
        self
    }

    /// Uses ONE relay, at `relay_url` ([`Relays::Custom`]) — typically a self-hosted
    /// `iroh-relay`. The wallet must be configured with the same URL: both ends of a link make
    /// the same relay choice.
    ///
    /// # Errors
    /// [`MobileError::InvalidRelayUrl`] unless it is an absolute `https://` URL without
    /// credentials; plain `http://` is accepted only for loopback, private and link-local hosts
    /// (a development relay on this machine or the local network).
    pub fn with_relay_url(mut self, relay_url: &str) -> Result<Self, MobileError> {
        self.relays = SessionConfig::new(relay_url)?.relays().clone();
        Ok(self)
    }

    /// Uses `relays`: [`Relays::Public`] (the default) or [`Relays::Custom`].
    #[must_use]
    pub fn with_relays(mut self, relays: Relays) -> Self {
        self.relays = relays;
        self
    }

    /// How long the wallet has to confirm it took a message (default 10 s, as over WebRTC).
    #[must_use]
    pub fn with_ack_timeout(mut self, timeout: Duration) -> Self {
        self.ack_timeout = Some(timeout);
        self
    }

    /// The largest message sent or accepted (default 8 MiB).
    #[must_use]
    pub fn with_max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = Some(bytes);
        self
    }

    /// The relays this connector's sessions use.
    #[must_use]
    pub fn relays(&self) -> &Relays {
        &self.relays
    }

    #[cfg(test)]
    fn session_config(&self) -> Result<SessionConfig, MobileError> {
        self.session_config_for(None)
    }

    /// The settings of `link`'s session: with its wallet's relay, when known, to knock there.
    fn session_config_for(&self, link: Option<LinkId>) -> Result<SessionConfig, MobileError> {
        let mut config = match &self.relays {
            Relays::Public => SessionConfig::public(),
            Relays::Custom(url) => SessionConfig::new(url.as_str())?,
        };
        let hint = link.and_then(|id| {
            self.wallet_relays
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .cloned()
        });
        if let Some(relay) = hint {
            // A hint that is not a usable relay URL is no reason not to start.
            config = config.clone().with_peer_relay(&relay).unwrap_or(config);
        }
        if let Some(timeout) = self.ack_timeout {
            config = config.with_ack_timeout(timeout);
        }
        if let Some(bytes) = self.max_message_size {
            config = config.with_max_message_size(bytes);
        }
        Ok(config)
    }

    /// The running link for `password`, starting its session if needed (bounded by `deadline`).
    async fn link(&self, password: &[u8], deadline: Instant) -> Result<Arc<link::Link>, MobileError> {
        self.start_link(password, deadline, Knocks::Yes, &mut |_| {})
            .await
    }

    /// [`link`](Self::link), telling `on` when it waits for another process to let go of it.
    async fn link_observed(
        &self,
        password: &[u8],
        deadline: Instant,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Arc<link::Link>, MobileError> {
        self.start_link(password, deadline, Knocks::Yes, on).await
    }

    /// [`link`](Self::link), knocking or not when its session starts.
    async fn start_link(
        &self,
        password: &[u8],
        deadline: Instant,
        knocks: Knocks,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Arc<link::Link>, MobileError> {
        let secret = ConnectionPassword::from_slice(password)?;
        let id = LinkId::of(password);
        let limit_ms = log::ms(deadline.saturating_duration_since(Instant::now()));
        let config = || {
            let config = self.session_config_for(Some(id))?;
            Ok(match knocks {
                Knocks::Yes => config,
                Knocks::No => config.without_knocks(),
            })
        };
        match timeout_at(
            deadline,
            self.links.get_or_start(&secret, id, config, &self.locks, on),
        )
        .await
        {
            Ok(link) => link,
            Err(_) => {
                // Waiting for another process of this machine, not for the network: say so.
                if let Some(holder) = self.locks.holder(id) {
                    return Err(MobileError::LinkBusy { holder });
                }
                log::session_start_timed_out(id, &self.relays, limit_ms);
                Err(MobileError::Session(
                    "timed out starting the iroh endpoint".to_string(),
                ))
            }
        }
    }

    /// Pairing: shows a QR code (through `on_qr`, which receives the exact JSON to render), waits
    /// for the wallet that scans it to answer with its `linkClient`, verifies the wallet's
    /// signature and returns `(wallet_public_key_hex, password)` to persist the link.
    ///
    /// The QR is the same JSON the WebRTC transport shows ([`linking::pairing_qr`]), signed with
    /// the connector's long-term Ed25519 identity (`identity_*_key_hex`). The session started for
    /// the new password is KEPT: the wallet closes its linking connection and reconnects with its
    /// regular one, which this session accepts, so the first request needs no new session.
    ///
    /// Once the signature checks out, `pair` waits (up to 5 s) for the wallet to close the
    /// connection it linked over before returning: that connection drops whatever else arrives
    /// on it, so a request sent right after pairing must wait for the regular one.
    ///
    /// Store the password like a credential and never log it: whoever holds it can talk to the
    /// wallet as this connector.
    ///
    /// # Errors
    /// [`MobileError::Crypto`] for an unusable identity (before anything is shown) or a wallet
    /// whose signature does not verify (a missing one included);
    /// [`MobileError::WalletUnreachable`] when no wallet connected in time and
    /// [`MobileError::ResponseTimeout`] when one connected but did not link; plus
    /// [`MobileError::Session`] and [`MobileError::InvalidRelayUrl`]. On any error the session is
    /// closed and the password discarded.
    pub async fn pair<F: FnOnce(String)>(
        &self,
        identity_private_key_hex: &str,
        identity_public_key_hex: &str,
        on_qr: F,
        timeout: Duration,
    ) -> Result<(String, Vec<u8>), MobileError> {
        self.pair_with_progress(
            identity_private_key_hex,
            identity_public_key_hex,
            on_qr,
            timeout,
            &mut |_| {},
        )
        .await
    }

    /// [`pair`](Self::pair), reporting the wallet's connection to `on` ([`Progress::WaitingForWallet`],
    /// [`Progress::Connected`], [`Progress::WalletDisconnected`], each with a diagnostics
    /// snapshot) — pairing is where a relay the server cannot reach shows up first.
    ///
    /// # Errors
    /// As [`pair`](Self::pair).
    pub async fn pair_with_progress<F: FnOnce(String)>(
        &self,
        identity_private_key_hex: &str,
        identity_public_key_hex: &str,
        on_qr: F,
        timeout: Duration,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(String, Vec<u8>), MobileError> {
        let started = Instant::now();
        let password = Zeroizing::new(linking::generate_password()?);
        let qr = Zeroizing::new(linking::pairing_qr(
            password.as_slice(),
            identity_private_key_hex,
            identity_public_key_hex,
        )?);
        // The QR is shown once the session listens, so the wallet that scans it finds it there.
        let wallet_public_key = self
            .pair_on(
                password.as_slice(),
                started,
                started + timeout,
                move || on_qr(qr.as_str().to_owned()),
                on,
            )
            .await?;
        Ok((wallet_public_key, password.to_vec()))
    }

    /// Pairing over a password the CALLER made and showed: waits for the wallet that scanned its
    /// QR to answer with its `linkClient`, verifies the wallet's signature and returns its public
    /// key (hex). [`pair_with_progress`](Self::pair_with_progress) is this with a password of its
    /// own, reporting to `on` in the same way.
    ///
    /// For a program that shows ONE QR for several transports — e.g. this one and WebRTC
    /// (`radixdlt_connect::Connector::pair_with_password`), whichever the wallet links over: make
    /// the password with [`linking::generate_password`], its QR with [`linking::pairing_qr`],
    /// and pair over each transport with it. The QR may be shown before this is called: the
    /// session starts at once, and a wallet that scanned sooner dials again until it is there.
    ///
    /// As with [`pair`](Self::pair), the session is KEPT on success (the wallet's regular
    /// connection reaches it, so the first request needs no new one) and the call returns once
    /// the wallet closed the connection it linked over (up to 5 s). On any error — and when the
    /// future is dropped, e.g. because the wallet linked over the other transport — the link's
    /// session is closed. A `linkClient` without a valid signature is refused.
    ///
    /// # Errors
    /// [`MobileError::InvalidPassword`] for a password that is not 32 bytes;
    /// [`MobileError::Crypto`] for a wallet whose signature does not verify (a missing one
    /// included); [`MobileError::WalletUnreachable`] when no wallet connected in time and
    /// [`MobileError::ResponseTimeout`] when one connected but did not link; plus
    /// [`MobileError::Session`] and [`MobileError::InvalidRelayUrl`].
    pub async fn pair_with_password(
        &self,
        password: &[u8],
        timeout: Duration,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<String, MobileError> {
        let started = Instant::now();
        self.pair_on(password, started, started + timeout, || {}, on)
            .await
    }

    /// The pairing itself, over `password`'s session: `listening` runs once it is started.
    async fn pair_on(
        &self,
        password: &[u8],
        started: Instant,
        deadline: Instant,
        listening: impl FnOnce(),
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<String, MobileError> {
        let id = LinkId::of(password);
        // Nobody uses this link yet: if the caller stops waiting, its session goes.
        let mut abandoned = Abandoned {
            links: &self.links,
            id,
            armed: true,
        };
        // A pairing does not knock: the wallet does not know the link before it scans the QR.
        let link = self
            .start_link(password, deadline, Knocks::No, &mut |_| {})
            .await?;
        let mut messages = link.subscribe();
        log::pairing_started(
            id,
            &self.relays,
            log::ms(deadline.saturating_duration_since(started)),
        );
        listening();

        let linked = await_link_client(&link, &mut messages, started, deadline, on)
            .await
            .and_then(|(client, linked_at)| {
                client.verify(password)?;
                Ok((client.public_key, linked_at))
            });
        match linked {
            Ok((wallet_public_key, linked_at)) => {
                await_linking_closed(&link, linked_at).await;
                abandoned.armed = false;
                log::paired(id, started);
                Ok(wallet_public_key)
            }
            Err(error) => {
                abandoned.armed = false;
                log::pairing_failed(id, started, &error);
                self.links.close(id, "pairing failed").await;
                Err(error)
            }
        }
    }

    /// Sends ANY wallet interaction and returns the wallet's answer to it, reporting each step to
    /// `on` ([`Progress`]). Every `request_*` method is this with a built request.
    ///
    /// The session for `password` is started if needed, and the call waits — within
    /// `overall_timeout` — for the wallet to connect. The interaction must carry an
    /// `interactionId` (every builder sets one); its answer is the message with the same id. If
    /// that answer had already arrived (an earlier call timed out and the person approved later),
    /// it is returned without sending anything. Dropping the future stops waiting; an answer that
    /// arrives afterwards is kept for [`await_response`](Self::await_response).
    ///
    /// # Errors
    /// [`MobileError::WalletUnreachable`] (nothing sent), [`MobileError::ConfirmationTimeout`]
    /// (sent, never confirmed), [`MobileError::ResponseTimeout`] (on the phone, unanswered),
    /// [`MobileError::MessageTooLarge`] and [`MobileError::MessageRefused`] (not delivered),
    /// [`MobileError::Protocol`] for an interaction without an id or one already waiting on this
    /// link, [`MobileError::InvalidPassword`], [`MobileError::Session`] and
    /// [`MobileError::Closed`]. The answer may still be a wallet `failure`: read it with the
    /// `extract_*` functions or [`check_failure`].
    pub async fn exchange(
        &self,
        password: &[u8],
        interaction: &Value,
        overall_timeout: Duration,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Value, MobileError> {
        let started = Instant::now();
        let deadline = started + overall_timeout;
        let interaction_id = inbox::interaction_id(interaction)
            .ok_or_else(|| {
                MobileError::Protocol(
                    "the interaction has no interactionId to match its answer by".to_string(),
                )
            })?
            .to_owned();
        let text = serde_json::to_string(interaction).map_err(|e| MobileError::Protocol(e.to_string()))?;
        let link = self.link_observed(password, deadline, on).await?;
        if link.removed_by_wallet() {
            return Err(MobileError::LinkRemoved);
        }
        let mut waiter = match link.register(&interaction_id)? {
            Pending::Answered(answer) => {
                log::already_answered(link.id(), &interaction_id);
                return Ok(answer);
            }
            Pending::Waiting(waiter) => waiter,
        };
        let mut others = link.subscribe();
        let request = Request {
            link: &link,
            interaction_id: &interaction_id,
            started,
            deadline,
        };
        let delivered = request.deliver(&text, on).await?;
        request.listen(&mut waiter, &mut others, delivered, on).await
    }

    /// Waits for the answer to a request sent EARLIER (by `interaction_id`) without sending
    /// anything: it is already on the phone, and sending it again would ask the person twice.
    /// Returns at once if the answer already arrived (it is kept, bounded, when it arrives with
    /// nobody waiting).
    ///
    /// # Errors
    /// [`MobileError::ResponseTimeout`] when no answer came in time, plus
    /// [`MobileError::InvalidPassword`], [`MobileError::Session`], [`MobileError::Protocol`] (the
    /// interaction already has a waiter on this link) and [`MobileError::Closed`].
    pub async fn await_response(
        &self,
        password: &[u8],
        interaction_id: &str,
        overall_timeout: Duration,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Value, MobileError> {
        let started = Instant::now();
        let deadline = started + overall_timeout;
        let link = self.link(password, deadline).await?;
        if link.removed_by_wallet() {
            return Err(MobileError::LinkRemoved);
        }
        let mut waiter = match link.register(interaction_id)? {
            Pending::Answered(answer) => return Ok(answer),
            Pending::Waiting(waiter) => waiter,
        };
        let mut others = link.subscribe();
        let request = Request {
            link: &link,
            interaction_id,
            started,
            deadline,
        };
        request
            .listen(&mut waiter, &mut others, Delivery::Confirmed, on)
            .await
    }

    /// Waits until the wallet is connected and returns how long that took, sending nothing:
    /// whether the wallet app is reachable right now. Nothing appears on the phone.
    ///
    /// # Errors
    /// [`MobileError::WalletUnreachable`] when it did not connect in time, plus
    /// [`MobileError::InvalidPassword`], [`MobileError::Session`] and [`MobileError::Closed`].
    pub async fn probe(&self, password: &[u8], overall_timeout: Duration) -> Result<Duration, MobileError> {
        let started = Instant::now();
        let deadline = started + overall_timeout;
        let link = self.link(password, deadline).await?;
        Request {
            link: &link,
            interaction_id: "-",
            started,
            deadline,
        }
        .wait_for_wallet(&mut |_| {})
        .await?;
        Ok(started.elapsed())
    }

    /// Starts the link's session now, if it is not running, so the wallet can connect BEFORE the
    /// first request: the wallet dials only while a session listens, and re-dials with a backoff
    /// of up to 15 s, so a server should open every stored link when it starts. Idempotent;
    /// every call on a link opens it anyway when needed.
    ///
    /// # Errors
    /// [`MobileError::InvalidPassword`], [`MobileError::InvalidRelayUrl`] and
    /// [`MobileError::Session`].
    pub async fn open_link(&self, password: &[u8]) -> Result<(), MobileError> {
        self.link(password, Instant::now() + SESSION_START_TIMEOUT)
            .await?;
        Ok(())
    }

    /// [`open_link`](Self::open_link), waiting at most `wait` — for the session to start, and for
    /// another process of this machine to let go of the link ([`MobileError::LinkBusy`] after it).
    ///
    /// # Errors
    /// As [`open_link`](Self::open_link), and [`MobileError::LinkBusy`].
    pub async fn open_link_within(&self, password: &[u8], wait: Duration) -> Result<(), MobileError> {
        self.link(password, Instant::now() + wait).await?;
        Ok(())
    }

    /// Who holds the link of `password` on this machine besides this connector — `process <pid>
    /// since <unix seconds>` — or `None` when nobody else does (this connector holding it
    /// included).
    #[must_use]
    pub fn link_holder(&self, password: &[u8]) -> Option<String> {
        let id = LinkId::of(password);
        if self.links.get(id).is_some() {
            return None;
        }
        self.locks.holder(id)
    }

    /// The messages the link receives that no request claims — the wallet's `accountList` after
    /// each connection, late answers — from now on. Starts the link's session if needed.
    ///
    /// # Errors
    /// [`MobileError::InvalidPassword`] and [`MobileError::Session`].
    pub async fn subscribe(&self, password: &[u8]) -> Result<WalletMessages, MobileError> {
        let link = self
            .link(password, Instant::now() + SESSION_START_TIMEOUT)
            .await?;
        Ok(WalletMessages {
            link: link.id(),
            messages: link.subscribe(),
            status: link.status(),
        })
    }

    /// A snapshot of the link's connectivity, or `None` when its session is not running (or is
    /// being started right now). Its `Display` is one log line; it holds no secrets.
    #[must_use]
    pub fn diagnostics(&self, password: &[u8]) -> Option<Diagnostics> {
        self.links
            .get(LinkId::of(password))
            .map(|link| link.diagnostics())
    }

    /// The relay the wallet of this link was last seen on, as the host remembers it (kept with
    /// the link). The link's next session knocks there as it starts, so a wallet that is up
    /// dials at once instead of at its next retry — up to 30 s later once it has been waiting.
    /// A relay that is wrong or out of date only means the knock does not arrive.
    pub fn set_wallet_relay(&self, password: &[u8], relay_url: &str) {
        self.wallet_relays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(LinkId::of(password), relay_url.to_string());
    }

    /// The relay this link's wallet is on, while its session runs and the wallet connected:
    /// what to keep with the link for [`set_wallet_relay`](Self::set_wallet_relay).
    pub async fn wallet_relay(&self, password: &[u8]) -> Option<String> {
        let link = self.links.get(LinkId::of(password))?;
        link.peer_relay().await.map(|url| url.to_string())
    }

    /// Whether the wallet of this link is connected right now.
    #[must_use]
    pub fn is_connected(&self, password: &[u8]) -> bool {
        self.links
            .get(LinkId::of(password))
            .is_some_and(|link| link.is_connected())
    }

    /// Whether the wallet of this link said it removed the link (see [`MobileError::LinkRemoved`]).
    /// Only known while the link's session runs.
    #[must_use]
    pub fn removed_by_wallet(&self, password: &[u8]) -> bool {
        self.links
            .get(LinkId::of(password))
            .is_some_and(|link| link.removed_by_wallet())
    }

    /// Tells the wallet that this connector removed the link, so that it removes it too: waits
    /// for the wallet to connect, sends `linkRemoved` and returns once the wallet CONFIRMED
    /// receiving it (the wallet answers nothing). If the wallet removed the link too and said so,
    /// that is the same outcome. Close the link afterwards ([`close_link`](Self::close_link)).
    ///
    /// Keep the link's password until this succeeds: the wallet is only reachable while its app
    /// is open, which may be days later.
    ///
    /// # Errors
    /// [`MobileError::WalletUnreachable`] when the wallet did not connect in time,
    /// [`MobileError::ConfirmationTimeout`] when it may not have received it, plus
    /// [`MobileError::InvalidPassword`], [`MobileError::Session`] and [`MobileError::Closed`].
    pub async fn notify_link_removed(
        &self,
        password: &[u8],
        overall_timeout: Duration,
    ) -> Result<(), MobileError> {
        let started = Instant::now();
        let deadline = started + overall_timeout;
        let link = self.link(password, deadline).await?;
        if link.removed_by_wallet() {
            return Ok(());
        }
        let text = link_removed_message().to_string();
        let request = Request {
            link: &link,
            interaction_id: LINK_REMOVED,
            started,
            deadline,
        };
        // Unlike a request, the notice may arrive twice (removing a removed link changes nothing):
        // one the wallet never confirmed — a wallet app just opened, not reading yet — is sent
        // again while there is time.
        loop {
            if link.removed_by_wallet() {
                return Ok(());
            }
            match request.deliver(&text, &mut |_| {}).await? {
                Delivery::Confirmed => {
                    log::removal_notice_sent(link.id(), started);
                    return Ok(());
                }
                Delivery::Uncertain if Instant::now() + RESEND_PAUSE < deadline => sleep(RESEND_PAUSE).await,
                Delivery::Uncertain => {
                    return Err(MobileError::ConfirmationTimeout {
                        diagnostics: Box::new(link.diagnostics()),
                    })
                }
            }
        }
    }

    /// Closes the link's session, telling the wallet. Calls waiting on it fail with
    /// [`MobileError::Closed`]; the next call starts a new session.
    pub async fn close_link(&self, password: &[u8]) {
        self.links.close(LinkId::of(password), "close_link").await;
    }

    /// Closes every session, telling the wallets, and waits until they are closed.
    pub async fn close(&self) {
        self.links.close_all().await;
    }
}

/// Whether a link's session knocks for its wallet when it starts.
#[derive(Debug, Clone, Copy)]
enum Knocks {
    Yes,
    No,
}

/// Discards the session of a pairing whose caller stopped waiting (the future was dropped).
struct Abandoned<'a> {
    links: &'a Registry,
    id: LinkId,
    armed: bool,
}

impl Drop for Abandoned<'_> {
    fn drop(&mut self) {
        if self.armed && self.links.discard(self.id) {
            log::pairing_abandoned(self.id);
        }
    }
}

/// Waits for the `linkClient` of the wallet that scanned the QR.
async fn await_link_client(
    link: &link::Link,
    messages: &mut broadcast::Receiver<Incoming>,
    started: Instant,
    deadline: Instant,
    on: &mut (dyn FnMut(Progress) + Send),
) -> Result<(LinkClient, u64), MobileError> {
    let mut status = link.status();
    let mut next_report = Instant::now() + FIRST_WAITING_REPORT;
    loop {
        tokio::select! {
            biased;
            message = messages.recv() => match message {
                Ok(incoming) => {
                    if let Some(client) = link_client(link, &incoming.message)? {
                        return Ok((client, incoming.state_changes));
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return Err(MobileError::Closed),
            },
            changed = status.changed() => {
                let now = changed.map(|()| *status.borrow_and_update()).unwrap_or(Status::Ended);
                if !report_status(link, now, started, on) {
                    return Err(MobileError::Closed);
                }
            }
            () = sleep_until(next_report.min(deadline)) => {
                if Instant::now() >= deadline {
                    return Err(pairing_timed_out(link, started, deadline));
                }
                if !link.is_connected() {
                    report_waiting(link, "pairing", started, deadline, on);
                }
                next_report = Instant::now() + WAITING_REPORT_INTERVAL;
            }
        }
    }
}

/// Waits — at most [`LINKING_GRACE`] — for the wallet to close the connection it linked over.
///
/// The wallet links over a LINKING connection of its own, which takes (so confirms) and drops any
/// other message, and closes it once the link is saved; its regular connection follows. A request
/// sent before then is lost — confirmed, so never sent again, and never answered. `linked_at` is
/// the link's [`state_changes`](link::Link::state_changes) when the `linkClient` arrived: once it
/// moves on, that connection is gone.
async fn await_linking_closed(link: &link::Link, linked_at: u64) {
    let started = Instant::now();
    let mut status = link.status();
    let closed = timeout(LINKING_GRACE, async {
        while link.state_changes() == linked_at {
            if status.changed().await.is_err() || *status.borrow() == Status::Ended {
                return;
            }
        }
    })
    .await
    .is_ok();
    log::linking_connection_closed(link.id(), closed, started.elapsed(), &link.diagnostics());
}

/// The `linkClient` in `message`, if it is one.
fn link_client(link: &link::Link, message: &Value) -> Result<Option<LinkClient>, MobileError> {
    let client = linking::parse_link_client(message)?;
    log::pairing_message(link.id(), client.is_some());
    Ok(client)
}

/// The error of a pairing that ran out of time: no wallet came, or one came and did not link.
fn pairing_timed_out(link: &link::Link, started: Instant, deadline: Instant) -> MobileError {
    let diagnostics = Box::new(link.diagnostics());
    let limit_ms = log::ms(deadline.saturating_duration_since(started));
    log::pairing_timed_out(link.id(), link.seen_wallet(), started, limit_ms, &diagnostics);
    if link.seen_wallet() {
        MobileError::ResponseTimeout { diagnostics }
    } else {
        MobileError::WalletUnreachable { diagnostics }
    }
}

/// Reports a change of the link's status. `false` once the link has ended.
fn report_status(
    link: &link::Link,
    status: Status,
    started: Instant,
    on: &mut (dyn FnMut(Progress) + Send),
) -> bool {
    let diagnostics = Box::new(link.diagnostics());
    match status {
        Status::Connected => on(Progress::Connected {
            elapsed: started.elapsed(),
            diagnostics,
        }),
        Status::Waiting => on(Progress::WalletDisconnected { diagnostics }),
        Status::Ended => return false,
    }
    true
}

/// Reports that `what` is still waiting for the wallet, which it does until `deadline`.
fn report_waiting(
    link: &link::Link,
    what: &str,
    started: Instant,
    deadline: Instant,
    on: &mut (dyn FnMut(Progress) + Send),
) {
    let diagnostics = Box::new(link.diagnostics());
    let limit_ms = log::ms(deadline.saturating_duration_since(started));
    log::waiting(link.id(), what, started, limit_ms, &diagnostics);
    on(Progress::WaitingForWallet {
        waited: started.elapsed(),
        diagnostics,
    });
}

/// How far a request got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// The wallet confirmed it.
    Confirmed,
    /// It may or may not have reached the wallet.
    Uncertain,
}

/// What to do after a send the session refused.
#[derive(Debug, PartialEq, Eq)]
enum AfterSend {
    /// Nothing was sent: try again once the wallet is back.
    Retry,
    /// It may have reached the wallet: wait for an answer, never send it again.
    Uncertain,
    /// It was not delivered and cannot be: give up.
    Fail(MobileError),
}

impl AfterSend {
    /// The rule that keeps a person from being asked twice: only a send the session reports as
    /// NOT sent is retried. A timeout or a lost connection may have delivered it.
    fn of(error: &SendError) -> Self {
        match error {
            SendError::NotConnected => AfterSend::Retry,
            SendError::Closed => AfterSend::Fail(MobileError::Closed),
            SendError::MessageTooLarge { size, max } => AfterSend::Fail(MobileError::MessageTooLarge {
                size: *size,
                max: *max,
            }),
            SendError::Rejected => AfterSend::Fail(MobileError::MessageRefused),
            // Timeout, ConnectionLost and anything new: it may have reached the wallet.
            _ => AfterSend::Uncertain,
        }
    }
}

/// One request on one link, between its start and its deadline.
struct Request<'a> {
    link: &'a link::Link,
    interaction_id: &'a str,
    started: Instant,
    deadline: Instant,
}

impl Request<'_> {
    /// The call's time limit, in whole milliseconds, for the log.
    fn limit_ms(&self) -> u64 {
        log::ms(self.deadline.saturating_duration_since(self.started))
    }

    /// Waits, until the deadline, for the wallet to be connected.
    async fn wait_for_wallet(&self, on: &mut (dyn FnMut(Progress) + Send)) -> Result<(), MobileError> {
        let mut status = self.link.status();
        let mut next_report = Instant::now() + FIRST_WAITING_REPORT;
        loop {
            if self.link.is_connected() {
                on(Progress::Connected {
                    elapsed: self.started.elapsed(),
                    diagnostics: Box::new(self.link.diagnostics()),
                });
                return Ok(());
            }
            if !self.link.is_running() {
                return Err(MobileError::Closed);
            }
            if Instant::now() >= self.deadline {
                return Err(self.unreachable());
            }
            if Instant::now() >= next_report {
                report_waiting(self.link, self.interaction_id, self.started, self.deadline, on);
                next_report = Instant::now() + WAITING_REPORT_INTERVAL;
            }
            tokio::select! {
                changed = status.changed() => changed.map_err(|_| MobileError::Closed)?,
                () = sleep_until(next_report.min(self.deadline)) => {}
            }
        }
    }

    fn unreachable(&self) -> MobileError {
        let diagnostics = Box::new(self.link.diagnostics());
        log::unreachable(
            self.link.id(),
            self.interaction_id,
            self.started,
            self.limit_ms(),
            &diagnostics,
        );
        MobileError::WalletUnreachable { diagnostics }
    }

    /// Sends the request once the wallet is there. Sends again only what the session reports as
    /// NOT sent; anything that may have reached the wallet is never sent twice.
    async fn deliver(
        &self,
        text: &str,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Delivery, MobileError> {
        let mut attempt = 1u32;
        loop {
            self.wait_for_wallet(on).await?;
            log::sending(
                self.link.id(),
                self.interaction_id,
                text.len(),
                attempt,
                self.started,
            );
            on(Progress::Sending {
                attempt,
                bytes: text.len(),
                elapsed: self.started.elapsed(),
            });
            match timeout_at(self.deadline, self.link.send(text)).await {
                Ok(Ok(())) => {
                    log::delivered(self.link.id(), self.interaction_id, self.started);
                    on(Progress::Delivered {
                        elapsed: self.started.elapsed(),
                    });
                    return Ok(Delivery::Confirmed);
                }
                Ok(Err(error)) => match self.after_send(error, attempt + 1, on) {
                    AfterSend::Retry => {
                        attempt += 1;
                        sleep(RESEND_PAUSE.min(self.deadline.saturating_duration_since(Instant::now())))
                            .await;
                    }
                    AfterSend::Uncertain => return Ok(Delivery::Uncertain),
                    AfterSend::Fail(error) => return Err(error),
                },
                // Abandoning the send resets its stream, so the wallet drops a message it has
                // not taken yet; it may still have taken it a moment before.
                Err(_) => return Err(self.timed_out(Delivery::Uncertain)),
            }
        }
    }

    /// Classifies a send the session refused, reporting it.
    fn after_send(
        &self,
        error: SendError,
        next_attempt: u32,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> AfterSend {
        let reason = error.to_string();
        let next = AfterSend::of(&error);
        match &next {
            AfterSend::Retry => {
                log::resending(self.link.id(), self.interaction_id, next_attempt, &reason);
                on(Progress::Resending {
                    attempt: next_attempt,
                    reason,
                });
            }
            AfterSend::Uncertain => {
                log::unconfirmed(
                    self.link.id(),
                    self.interaction_id,
                    &reason,
                    &self.link.diagnostics(),
                );
                on(Progress::Unconfirmed { reason });
            }
            AfterSend::Fail(_) => {}
        }
        next
    }

    /// Waits, until the deadline, for the answer, reporting everything else that happens.
    async fn listen(
        &self,
        waiter: &mut Waiter,
        others: &mut broadcast::Receiver<Incoming>,
        delivery: Delivery,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Value, MobileError> {
        let mut status = self.link.status();
        let mut others_open = true;
        loop {
            tokio::select! {
                biased;
                answer = waiter.answer() => {
                    log::answered(self.link.id(), self.interaction_id, self.started, answer.is_some());
                    return answer.ok_or(MobileError::Closed);
                }
                message = others.recv(), if others_open => match message {
                    // The wallet removed the link: what was asked will not be answered.
                    Ok(incoming) if is_link_removed(&incoming.message) => return Err(MobileError::LinkRemoved),
                    Ok(incoming) => on(Progress::OtherMessage(incoming.message)),
                    Err(RecvError::Lagged(skipped)) => log::skipped(self.link.id(), skipped),
                    Err(RecvError::Closed) => others_open = false,
                },
                changed = status.changed() => {
                    let now = changed.map(|()| *status.borrow_and_update()).unwrap_or(Status::Ended);
                    log::changed_while_waiting(self.link.id(), self.interaction_id, now, &self.link.diagnostics());
                    // An ended link wakes the waiter, which reports it.
                    report_status(self.link, now, self.started, on);
                }
                () = sleep_until(self.deadline) => return Err(self.timed_out(delivery)),
            }
        }
    }

    /// The error of a request that ran out of time after it may have been sent.
    fn timed_out(&self, delivery: Delivery) -> MobileError {
        let diagnostics = Box::new(self.link.diagnostics());
        let confirmed = delivery == Delivery::Confirmed;
        log::timed_out(
            self.link.id(),
            self.interaction_id,
            confirmed,
            self.started,
            self.limit_ms(),
            &diagnostics,
        );
        if confirmed {
            MobileError::ResponseTimeout { diagnostics }
        } else {
            MobileError::ConfirmationTimeout { diagnostics }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn the_connector_can_be_shared_between_tasks() {
        assert_send_sync::<MobileWalletConnector>();
        assert_send_sync::<MobileError>();
        assert_send_sync::<WalletMessages>();
    }

    #[test]
    fn public_relays_by_default_and_a_custom_one_is_checked() {
        let connector = MobileWalletConnector::new();
        assert_eq!(connector.relays(), &Relays::Public);
        assert_eq!(connector.session_config().unwrap().relays(), &Relays::Public);
        let local = MobileWalletConnector::new()
            .with_relay_url(" http://127.0.0.1:3340 ")
            .unwrap();
        let Relays::Custom(url) = local.relays() else {
            panic!("{:?}", local.relays());
        };
        assert_eq!(url.as_str(), "http://127.0.0.1:3340/");
        assert_eq!(local.session_config().unwrap().relays(), local.relays());
        assert_eq!(local.with_relays(Relays::Public).relays(), &Relays::Public);
        for bad in [
            "http://relay.example.com",
            "wss://relay.example.com",
            "https://u:p@relay.example.com",
        ] {
            assert!(
                matches!(
                    MobileWalletConnector::new().with_relay_url(bad),
                    Err(MobileError::InvalidRelayUrl(_))
                ),
                "{bad}"
            );
        }
    }

    /// Only what certainly was NOT sent is sent again: anything that may have reached the
    /// wallet could ask the person twice, or sign a transaction twice.
    #[test]
    fn only_a_send_that_sent_nothing_is_retried() {
        assert_eq!(AfterSend::of(&SendError::NotConnected), AfterSend::Retry);
        assert_eq!(AfterSend::of(&SendError::Timeout), AfterSend::Uncertain);
        assert_eq!(
            AfterSend::of(&SendError::ConnectionLost("reset".into())),
            AfterSend::Uncertain
        );
        assert_eq!(
            AfterSend::of(&SendError::Rejected),
            AfterSend::Fail(MobileError::MessageRefused)
        );
        assert_eq!(
            AfterSend::of(&SendError::MessageTooLarge { size: 9, max: 8 }),
            AfterSend::Fail(MobileError::MessageTooLarge { size: 9, max: 8 })
        );
        assert_eq!(
            AfterSend::of(&SendError::Closed),
            AfterSend::Fail(MobileError::Closed)
        );
    }

    #[tokio::test]
    async fn a_password_of_the_wrong_length_is_refused_before_anything_starts() {
        let connector = MobileWalletConnector::new();
        let interaction = serde_json::json!({ "interactionId": "x" });
        let result = connector
            .exchange(&[1, 2, 3], &interaction, Duration::from_secs(1), &mut |_| {})
            .await;
        assert_eq!(result, Err(MobileError::InvalidPassword));
        assert!(connector.diagnostics(&[1, 2, 3]).is_none());
    }

    #[tokio::test]
    async fn an_interaction_without_an_id_is_refused() {
        let connector = MobileWalletConnector::new();
        let result = connector
            .exchange(
                &[7; 32],
                &serde_json::json!({ "items": {} }),
                Duration::from_secs(1),
                &mut |_| {},
            )
            .await;
        assert!(matches!(result, Err(MobileError::Protocol(_))), "{result:?}");
    }

    #[tokio::test]
    async fn an_unusable_identity_fails_pairing_before_the_qr_is_shown() {
        let connector = MobileWalletConnector::new();
        let mut shown = false;
        let result = connector
            .pair("00", "11", |_| shown = true, Duration::from_secs(1))
            .await;
        assert!(matches!(result, Err(MobileError::Crypto(_))), "{result:?}");
        assert!(!shown);
    }
}
