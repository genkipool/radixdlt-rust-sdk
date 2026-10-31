//! [`WalletConnector`]: pairing, requests and link removal over iroh and WebRTC, the transport
//! chosen per link and per request.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use radixdlt_connect_iroh::mobile::{MobileWalletConnector, Progress as IrohProgress};
use radixdlt_connect_types::{linking, Transport};
use radixdlt_connect_webrtc::state::{Link, RetiredLink};
use radixdlt_connect_webrtc::{Connector, Progress as WebrtcProgress};
use serde_json::Value;
use tokio::time::{sleep, sleep_until, Instant};

use crate::error::{Error, Reach};
use crate::race::{CarrierRace, PairStep, PairingRace};

/// How long iroh keeps the chance to link after WebRTC linked first, when pairing over both: a
/// wallet with iroh on links over both, and iroh is the better of the two.
pub const IROH_GRACE: Duration = Duration::from_secs(5);

const TARGET: &str = "radixdlt_connect_auto";

/// One transport's part of a race, until it ends (or is dropped, when the other won).
type Running<'a, T> = Option<Pin<Box<dyn Future<Output = T> + Send + 'a>>>;

/// Which transports a connector uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transports {
    /// Both: pairing races them, preferring iroh; a link paired over iroh races both for each
    /// request, a link paired over WebRTC uses WebRTC (its wallet does not speak iroh).
    #[default]
    Auto,
    /// WebRTC only.
    WebrtcOnly,
    /// iroh only.
    IrohOnly,
}

impl Transports {
    /// The transports a pairing tries.
    fn for_pairing(self) -> &'static [Transport] {
        match self {
            Transports::Auto => &[Transport::Iroh, Transport::Webrtc],
            Transports::WebrtcOnly => &[Transport::Webrtc],
            Transports::IrohOnly => &[Transport::Iroh],
        }
    }

    /// The transports a request over a link paired over `paired_over` races.
    fn for_link(self, paired_over: Transport) -> &'static [Transport] {
        match (self, paired_over) {
            (Transports::Auto, Transport::Iroh) => &[Transport::Iroh, Transport::Webrtc],
            (Transports::Auto, Transport::Webrtc) | (Transports::WebrtcOnly, _) => &[Transport::Webrtc],
            (Transports::IrohOnly, _) => &[Transport::Iroh],
        }
    }
}

/// What happens during a call, for whoever shows or logs it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// The request goes over this transport (the first one ready).
    Carrier(Transport),
    /// The wallet confirmed receiving the request.
    Delivered(Transport),
    /// A message that is not this request's answer (the wallet's account list, a late answer).
    OtherMessage(Value),
    /// A pairing attempt over a transport ended: the wallet that linked over it, or why not. The
    /// pairing keeps the one the race decides; this is for whoever logs each attempt.
    PairingAttempt {
        /// The transport.
        transport: Transport,
        /// The wallet's public key, or why it did not link.
        linked: Result<String, String>,
    },
    /// Every step iroh reports (waiting for the wallet, connected, sending, resending…), as it
    /// reports it: for logs and diagnostics that want the detail.
    IrohStep(IrohProgress),
    /// Every step WebRTC reports, as it reports it.
    WebrtcStep(WebrtcProgress),
}

/// A wallet that linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    /// The wallet's public key.
    pub wallet_public_key: String,
    /// The transport it linked over (and the link keeps).
    pub transport: Transport,
    /// The custom iroh relay, when it linked over iroh on one.
    pub iroh_relay: Option<String>,
    /// Why the pairing kept that transport, for logs (`iroh first`, `webrtc: no iroh within grace`…).
    pub why: &'static str,
}

impl Paired {
    /// The link to store: with `password` (the one the pairing used), a label and when.
    #[must_use]
    pub fn into_link(self, password: &[u8], label: Option<String>, linked_at: String) -> Link {
        Link {
            password: to_hex(password),
            wallet_public_key: self.wallet_public_key,
            linked_at,
            label,
            transport: Some(self.transport),
            iroh_relay: self.iroh_relay,
        }
    }
}

/// Talks to the Radix Wallet over iroh or WebRTC, with nothing to choose.
///
/// - [`pair`](Self::pair) shows ONE QR that links over both: over iroh when the wallet has it on,
///   otherwise over WebRTC. The [`Link`] records which.
/// - Each request ([`exchange`](Self::exchange) and the `request_*` helpers) over a link paired
///   over iroh goes over iroh at once when its connection is up, and otherwise RACES both: the
///   first transport ready to carry it carries it, and only that one — the wallet never gets it
///   twice. Its answer is awaited over both, as the wallet answers over whichever it is connected
///   on. So a request is never slower than WebRTC, and as fast as iroh when iroh is there.
/// - [`keep_listening`](Self::keep_listening) keeps a link's iroh session open, so that the wallet
///   connects as soon as its app opens and the next request goes at once.
/// - [`notify_link_removed`](Self::notify_link_removed) tells a wallet its link was removed, over
///   every transport it may be on.
///
/// Cheap to clone: clones share the transports, their sessions and their per-link turns.
#[derive(Clone)]
pub struct WalletConnector {
    webrtc: Arc<Connector>,
    iroh: Arc<MobileWalletConnector>,
    /// The iroh connector of each custom relay a link was paired on.
    iroh_by_relay: Arc<Mutex<HashMap<String, Arc<MobileWalletConnector>>>>,
    /// The custom iroh relay new pairings use, if not iroh's public ones.
    iroh_relay: Option<String>,
    transports: Transports,
    iroh_grace: Duration,
}

impl std::fmt::Debug for WalletConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletConnector")
            .field("transports", &self.transports)
            .field("iroh_relay", &self.iroh_relay)
            .finish_non_exhaustive()
    }
}

impl Default for WalletConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl WalletConnector {
    /// Both transports, on their public servers: Radix's signaling server for WebRTC, iroh's
    /// public relays.
    ///
    /// TLS: when the application has not installed a process-wide `rustls` crypto provider, this
    /// installs `ring` (WebRTC's signaling and iroh together enable two, and rustls would refuse to
    /// choose). One installed before is kept.
    #[must_use]
    pub fn new() -> Self {
        static TLS: std::sync::Once = std::sync::Once::new();
        TLS.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        Self {
            webrtc: Arc::new(Connector::new()),
            iroh: Arc::new(MobileWalletConnector::new()),
            iroh_by_relay: Arc::default(),
            iroh_relay: None,
            transports: Transports::Auto,
            iroh_grace: IROH_GRACE,
        }
    }

    /// Which transports to use (both by default).
    #[must_use]
    pub fn with_transports(mut self, transports: Transports) -> Self {
        self.transports = transports;
        self
    }

    /// The WebRTC connector to use (its signaling servers, ICE servers, TURN).
    #[must_use]
    pub fn with_webrtc(mut self, webrtc: Connector) -> Self {
        self.webrtc = Arc::new(webrtc);
        self
    }

    /// The iroh connector for links on iroh's public relays (and its settings).
    #[must_use]
    pub fn with_iroh(mut self, iroh: MobileWalletConnector) -> Self {
        self.iroh = Arc::new(iroh);
        self
    }

    /// The same, shared with its owner: whoever holds other sessions on it (or its turns) keeps
    /// them, and this uses them.
    #[must_use]
    pub fn with_iroh_shared(mut self, iroh: Arc<MobileWalletConnector>) -> Self {
        self.iroh = iroh;
        self
    }

    /// A custom iroh relay for new pairings, instead of iroh's public relays.
    ///
    /// # Errors
    /// [`Error::Failed`] when the URL is not a usable relay URL.
    pub fn with_iroh_relay(mut self, relay_url: &str) -> Result<Self, Error> {
        self.iroh_on(Some(relay_url))?;
        self.iroh_relay = Some(relay_url.to_string());
        Ok(self)
    }

    /// How long iroh may still link after WebRTC did, when pairing ([`IROH_GRACE`] by default).
    #[must_use]
    pub fn with_iroh_grace(mut self, grace: Duration) -> Self {
        self.iroh_grace = grace;
        self
    }

    /// The iroh connector of a relay (`None`: the public relays).
    fn iroh_on(&self, relay: Option<&str>) -> Result<Arc<MobileWalletConnector>, Error> {
        let Some(url) = relay else {
            return Ok(self.iroh.clone());
        };
        let mut by_relay = self.iroh_by_relay.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(connector) = by_relay.get(url) {
            return Ok(connector.clone());
        }
        let connector =
            Arc::new(
                MobileWalletConnector::new()
                    .with_relay_url(url)
                    .map_err(|e| Error::Failed {
                        transport: Transport::Iroh,
                        detail: e.to_string(),
                    })?,
            );
        by_relay.insert(url.to_string(), connector.clone());
        Ok(connector)
    }

    /* ─────────────────────────────── pairing ─────────────────────────────── */

    /// Pairs a wallet: shows one QR (through `on_qr`, which gets the exact text to render) that
    /// links over iroh or WebRTC, and returns the link to store once the wallet linked.
    ///
    /// `identity_private_key_hex` / `identity_public_key_hex` are the connector's identity (as
    /// in `connector.json`), which signs the QR.
    ///
    /// # Errors
    /// [`Error::PairingFailed`] with why over each transport, [`Error::InvalidLink`] for an
    /// unusable identity.
    pub async fn pair(
        &self,
        identity_private_key_hex: &str,
        identity_public_key_hex: &str,
        on_qr: impl FnOnce(String),
        timeout: Duration,
    ) -> Result<(Vec<u8>, Paired), Error> {
        let password = linking::generate_password().map_err(|e| Error::InvalidLink(e.to_string()))?;
        let qr = linking::pairing_qr(&password, identity_private_key_hex, identity_public_key_hex)
            .map_err(|e| Error::InvalidLink(e.to_string()))?;
        on_qr(qr);
        let paired = self.pair_with_password(&password, timeout).await?;
        Ok((password.to_vec(), paired))
    }

    /// Pairs over `password`, whose QR the caller shows (see [`linking::pairing_qr`]): over both
    /// transports at once, iroh preferred (it gets [`IROH_GRACE`] after WebRTC links).
    ///
    /// # Errors
    /// [`Error::PairingFailed`] with why over each transport.
    pub async fn pair_with_password(&self, password: &[u8], timeout: Duration) -> Result<Paired, Error> {
        Box::pin(self.pair_with_password_observed(password, timeout, &mut |_| {})).await
    }

    /// [`pair_with_password`](Self::pair_with_password), reporting each attempt and each iroh step
    /// through `on`.
    ///
    /// # Errors
    /// [`Error::PairingFailed`] with why over each transport.
    pub async fn pair_with_password_observed(
        &self,
        password: &[u8],
        timeout: Duration,
        on: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Paired, Error> {
        let password: &[u8; linking::PASSWORD_LENGTH] = password
            .try_into()
            .map_err(|_| Error::InvalidLink("the password must be 32 bytes".into()))?;
        let plan = self.transports.for_pairing();
        let mut race = PairingRace::new(plan);
        let iroh = self.iroh_on(self.iroh_relay.as_deref())?;
        let (steps, mut stepped) = tokio::sync::mpsc::unbounded_channel();
        let mut forward = move |step: IrohProgress| {
            let _ = steps.send(step);
        };
        let mut over_iroh: Running<'_, Result<String, Error>> = plan.contains(&Transport::Iroh).then(|| {
            let iroh = iroh.clone();
            let forward = &mut forward;
            Box::pin(async move {
                iroh.pair_with_password(password, timeout, forward)
                    .await
                    .map_err(Error::from_iroh)
            }) as Pin<Box<dyn Future<Output = _> + Send + '_>>
        });
        let mut over_webrtc: Running<'_, Result<String, Error>> =
            plan.contains(&Transport::Webrtc).then(|| {
                Box::pin(async move {
                    self.webrtc
                        .pair_with_password(password, timeout)
                        .await
                        .map_err(Error::from_webrtc)
                }) as Pin<Box<dyn Future<Output = _> + Send + '_>>
            });
        let mut grace: Option<Pin<Box<tokio::time::Sleep>>> = None;
        tracing::info!(target: TARGET, transports = ?plan, "pairing: waiting for the wallet");
        loop {
            let step = tokio::select! {
                biased;
                Some(step) = stepped.recv() => {
                    on(Event::IrohStep(step));
                    continue;
                }
                linked = async { over_iroh.as_mut().expect("guarded").await }, if over_iroh.is_some() => {
                    over_iroh = None;
                    let linked = linked.map_err(|e| e.to_string());
                    on(Event::PairingAttempt { transport: Transport::Iroh, linked: linked.clone() });
                    match linked {
                        Ok(wallet) => race.linked(Transport::Iroh, wallet),
                        Err(reason) => race.failed(Transport::Iroh, reason),
                    }
                }
                linked = async { over_webrtc.as_mut().expect("guarded").await }, if over_webrtc.is_some() => {
                    over_webrtc = None;
                    let linked = linked.map_err(|e| e.to_string());
                    on(Event::PairingAttempt { transport: Transport::Webrtc, linked: linked.clone() });
                    match linked {
                        Ok(wallet) => race.linked(Transport::Webrtc, wallet),
                        Err(reason) => race.failed(Transport::Webrtc, reason),
                    }
                }
                () = async { grace.as_mut().expect("guarded").await }, if grace.is_some() => {
                    grace = None;
                    race.grace_over()
                }
            };
            match step {
                PairStep::Wait { start_grace } => {
                    if start_grace {
                        grace = Some(Box::pin(sleep(self.iroh_grace)));
                    }
                }
                // The attempt not kept is dropped as this returns: its session or signaling closes.
                PairStep::Linked(transport, wallet_public_key, why) => {
                    tracing::info!(target: TARGET, %transport, why, "pairing: the wallet linked");
                    return Ok(Paired {
                        wallet_public_key,
                        transport,
                        why,
                        iroh_relay: (transport == Transport::Iroh)
                            .then(|| self.iroh_relay.clone())
                            .flatten(),
                    });
                }
                PairStep::Failed(reasons) => return Err(Error::PairingFailed { reasons }),
            }
        }
    }

    /* ─────────────────────────────── requests ─────────────────────────────── */

    /// Sends `interaction` (any wallet interaction: the `*_request` builders) over `link` and
    /// returns the wallet's answer, which may still be a wallet `failure`: read it with the
    /// `extract_*` functions or [`crate::check_failure`].
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn exchange(
        &self,
        link: &Link,
        interaction: &Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.exchange_with(link, interaction, timeout, &mut |_| {}).await
    }

    /// [`exchange`](Self::exchange), reporting what happens through `on`.
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn exchange_with(
        &self,
        link: &Link,
        interaction: &Value,
        timeout: Duration,
        on: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Value, Error> {
        let password = link
            .password_bytes()
            .map_err(|e| Error::InvalidLink(e.to_string()))?;
        // Each on the heap: a race's state is large, and a caller that nests this deep in its own
        // futures (a tool call in a server, on a thread with a small stack) must not carry it.
        match self.transports.for_link(link.paired_over()) {
            [Transport::Webrtc] => {
                on(Event::Carrier(Transport::Webrtc));
                Box::pin(self.over_webrtc(&password, interaction, timeout, on)).await
            }
            [Transport::Iroh] => {
                let iroh = self.iroh_on(link.iroh_relay.as_deref())?;
                on(Event::Carrier(Transport::Iroh));
                Box::pin(over_iroh(&iroh, &password, interaction, timeout, on)).await
            }
            racing => {
                let iroh = self.iroh_on(link.iroh_relay.as_deref())?;
                Box::pin(self.race(&iroh, racing, &password, interaction, timeout, on)).await
            }
        }
    }

    async fn over_webrtc(
        &self,
        password: &[u8],
        interaction: &Value,
        timeout: Duration,
        on: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Value, Error> {
        let (steps, mut stepped) = tokio::sync::mpsc::unbounded_channel();
        let mut forward = move |step: WebrtcProgress| {
            let _ = steps.send(step);
        };
        let exchange = self.webrtc.exchange(password, interaction, timeout, &mut forward);
        tokio::pin!(exchange);
        loop {
            tokio::select! {
                result = &mut exchange => {
                    while let Ok(step) = stepped.try_recv() {
                        report_webrtc(step, on);
                    }
                    return result.map_err(Error::from_webrtc);
                }
                Some(step) = stepped.recv() => report_webrtc(step, on),
            }
        }
    }

    /// One request over the transports `racing` (iroh and WebRTC): the first ready carries it.
    #[allow(clippy::too_many_lines)]
    async fn race(
        &self,
        iroh: &MobileWalletConnector,
        racing: &[Transport],
        password: &[u8],
        interaction: &Value,
        timeout: Duration,
        on: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Value, Error> {
        let started = Instant::now();
        let deadline = started + timeout;
        // Its connection is up (a link kept listening, or a request a moment ago): no race.
        if iroh.is_connected(password) {
            tracing::debug!(target: TARGET, "request: iroh already connected, sent over it");
            on(Event::Carrier(Transport::Iroh));
            return Box::pin(over_iroh(iroh, password, interaction, timeout, on)).await;
        }
        let interaction_id = interaction
            .get("interactionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut race = CarrierRace::default();
        let mut reaches: Vec<(Transport, Reach)> = Vec::new();

        let (steps, mut stepped) = tokio::sync::mpsc::unbounded_channel();
        let mut forward = move |step: WebrtcProgress| {
            let _ = steps.send(step);
        };
        // WebRTC opens its channel and, unless it is dropped first, sends the request on it a moment
        // later (`ChannelOpen`, then the send): dropped before its channel opened, it sent nothing.
        let mut webrtc = Some(Box::pin(self.webrtc.exchange(
            password,
            interaction,
            timeout,
            &mut forward,
        )));
        // iroh: until the wallet is connected, sending nothing.
        let mut iroh_ready = Some(Box::pin(iroh.probe(password, timeout)));
        // Once WebRTC carries it: the answer, should the wallet send it over iroh.
        let mut iroh_answer: Running<'_, Result<Value, radixdlt_connect_iroh::mobile::MobileError>> = None;

        loop {
            tokio::select! {
                biased;
                ready = async { iroh_ready.as_mut().expect("guarded").await }, if iroh_ready.is_some() => {
                    iroh_ready = None;
                    match ready {
                        Ok(_) if race.ready(Transport::Iroh) => {
                            tracing::info!(target: TARGET, elapsed_ms = ms(started), "request: iroh ready first, sent over it");
                            drop(webrtc.take());
                            on(Event::Carrier(Transport::Iroh));
                            return over_iroh(iroh, password, interaction, deadline.saturating_duration_since(Instant::now()), on).await;
                        }
                        // WebRTC carries it: listen over iroh too.
                        Ok(_) => {
                            let left = deadline.saturating_duration_since(Instant::now());
                            let interaction_id = &interaction_id;
                            iroh_answer = Some(Box::pin(async move {
                                let mut ignore = |_: IrohProgress| {};
                                iroh.await_response(password, interaction_id, left, &mut ignore).await
                            }));
                        }
                        Err(error) => {
                            let reach = Error::reach_iroh(error);
                            if let Reach::Final(Error::LinkRemoved) = reach {
                                drop(webrtc.take());
                                return Err(Error::LinkRemoved);
                            }
                            if race.carrier().is_none() {
                                reaches.push((Transport::Iroh, reach));
                                if !race.failed(Transport::Iroh, racing) {
                                    return Err(Error::unreachable(reaches));
                                }
                            }
                        }
                    }
                }
                answer = async { iroh_answer.as_mut().expect("guarded").await }, if iroh_answer.is_some() => {
                    iroh_answer = None;
                    if let Ok(answer) = answer {
                        tracing::info!(target: TARGET, elapsed_ms = ms(started), "request: answered over iroh");
                        return Ok(answer);
                    }
                }
                Some(step) = stepped.recv() => {
                    if matches!(step, WebrtcProgress::ChannelOpen { .. }) && race.ready(Transport::Webrtc) {
                        tracing::info!(target: TARGET, elapsed_ms = ms(started), "request: webrtc ready first, sent over it");
                        on(Event::Carrier(Transport::Webrtc));
                    }
                    report_webrtc(step, on);
                }
                result = async { webrtc.as_mut().expect("guarded").await }, if webrtc.is_some() => {
                    webrtc = None;
                    while let Ok(step) = stepped.try_recv() {
                        report_webrtc(step, on);
                    }
                    match result {
                        Ok(answer) => return Ok(answer),
                        // It carried the request: its outcome is the request's.
                        Err(error) if race.carrier() == Some(Transport::Webrtc) => {
                            // Unless the answer is coming over iroh.
                            if let Some(answer) = iroh_answer.take() {
                                if let Ok(Ok(answer)) = tokio::time::timeout_at(deadline, answer).await {
                                    return Ok(answer);
                                }
                            }
                            return Err(Error::from_webrtc(error));
                        }
                        Err(error) => {
                            let reach = Error::reach_webrtc(error);
                            if let Reach::Final(Error::LinkRemoved) = reach {
                                return Err(Error::LinkRemoved);
                            }
                            reaches.push((Transport::Webrtc, reach));
                            if !race.failed(Transport::Webrtc, racing) {
                                return Err(Error::unreachable(reaches));
                            }
                        }
                    }
                }
                () = sleep_until(deadline) => {
                    // A transport still trying when the time is up got out to the network and found
                    // no wallet: never "no network", whichever finished first.
                    let still_trying = [
                        (Transport::Iroh, iroh_ready.is_some()),
                        (Transport::Webrtc, webrtc.is_some()),
                    ];
                    for (transport, trying) in still_trying {
                        if trying && !reaches.iter().any(|(t, _)| *t == transport) {
                            reaches.push((transport, Reach::Absent("the wallet did not connect in time".into())));
                        }
                    }
                    return Err(Error::unreachable(reaches));
                }
            }
        }
    }

    /* ────────────────────────────── link removal ───────────────────────────── */

    /// Tells the wallet of a removed link that it was removed, over every transport it may be on
    /// (it may arrive twice: removing a removed link changes nothing). Returns once the wallet
    /// confirmed it over one. Keep the [`RetiredLink`] until then.
    ///
    /// # Errors
    /// The failures over each transport when none got it through in time.
    pub async fn notify_link_removed(&self, link: &RetiredLink, timeout: Duration) -> Result<(), Error> {
        let password = link
            .password_bytes()
            .map_err(|e| Error::InvalidLink(e.to_string()))?;
        let paired_over = link.transport.unwrap_or(Transport::Webrtc);
        let plan = self.transports.for_link(paired_over);
        let iroh = self.iroh_on(link.iroh_relay.as_deref())?;
        let over_iroh = async {
            if plan.contains(&Transport::Iroh) {
                iroh.notify_link_removed(&password, timeout)
                    .await
                    .map_err(|e| (Transport::Iroh, Error::reach_iroh(e)))
            } else {
                Err((Transport::Iroh, Reach::Absent("not tried".into())))
            }
        };
        let over_webrtc = async {
            if plan.contains(&Transport::Webrtc) {
                self.webrtc
                    .notify_link_removed(&password, timeout)
                    .await
                    .map_err(|e| (Transport::Webrtc, Error::reach_webrtc(e)))
            } else {
                Err((Transport::Webrtc, Reach::Absent("not tried".into())))
            }
        };
        tokio::pin!(over_iroh, over_webrtc);
        let (mut iroh_done, mut webrtc_done) = (false, false);
        let mut reaches = Vec::new();
        loop {
            tokio::select! {
                told = &mut over_iroh, if !iroh_done => {
                    iroh_done = true;
                    match told {
                        Ok(()) => return Ok(()),
                        Err(failure) if plan.contains(&Transport::Iroh) => reaches.push(failure),
                        Err(_) => {}
                    }
                }
                told = &mut over_webrtc, if !webrtc_done => {
                    webrtc_done = true;
                    match told {
                        Ok(()) => return Ok(()),
                        Err(failure) if plan.contains(&Transport::Webrtc) => reaches.push(failure),
                        Err(_) => {}
                    }
                }
            }
            if iroh_done && webrtc_done {
                return Err(Error::unreachable(reaches));
            }
        }
    }

    /* ───────────────────────────── staying ready ───────────────────────────── */

    /// Keeps `link`'s iroh session open from now on, so that its wallet connects as soon as its
    /// app opens and the next request goes at once, with no race. For a process that stays up (a
    /// daemon, an agent); a link paired over WebRTC has nothing to keep open.
    ///
    /// # Errors
    /// [`Error::InvalidLink`], or why the iroh session could not start.
    pub async fn keep_listening(&self, link: &Link) -> Result<(), Error> {
        if !self
            .transports
            .for_link(link.paired_over())
            .contains(&Transport::Iroh)
        {
            return Ok(());
        }
        let password = link
            .password_bytes()
            .map_err(|e| Error::InvalidLink(e.to_string()))?;
        self.iroh_on(link.iroh_relay.as_deref())?
            .open_link(&password)
            .await
            .map_err(Error::from_iroh)
    }

    /// Stops listening on `link` over iroh (its wallet is told). The next request opens it again.
    pub async fn close_link(&self, link: &Link) {
        if let (Ok(password), Ok(iroh)) = (link.password_bytes(), self.iroh_on(link.iroh_relay.as_deref())) {
            iroh.close_link(&password).await;
        }
    }

    /// Whether `link`'s wallet is connected over iroh right now.
    #[must_use]
    pub fn connected_over_iroh(&self, link: &Link) -> bool {
        match (link.password_bytes(), self.iroh_on(link.iroh_relay.as_deref())) {
            (Ok(password), Ok(iroh)) => iroh.is_connected(&password),
            _ => false,
        }
    }

    /// The WebRTC connector it uses.
    #[must_use]
    pub fn webrtc(&self) -> &Connector {
        &self.webrtc
    }
}

async fn over_iroh(
    iroh: &MobileWalletConnector,
    password: &[u8],
    interaction: &Value,
    timeout: Duration,
    on: &mut (dyn FnMut(Event) + Send),
) -> Result<Value, Error> {
    let (steps, mut stepped) = tokio::sync::mpsc::unbounded_channel();
    let mut forward = move |step: IrohProgress| {
        let _ = steps.send(step);
    };
    let exchange = iroh.exchange(password, interaction, timeout, &mut forward);
    tokio::pin!(exchange);
    loop {
        tokio::select! {
            result = &mut exchange => {
                while let Ok(step) = stepped.try_recv() {
                    report_iroh(step, on);
                }
                return result.map_err(Error::from_iroh);
            }
            Some(step) = stepped.recv() => report_iroh(step, on),
        }
    }
}

fn report_webrtc(step: WebrtcProgress, on: &mut (dyn FnMut(Event) + Send)) {
    on(Event::WebrtcStep(step.clone()));
    match step {
        WebrtcProgress::Delivered { .. } => on(Event::Delivered(Transport::Webrtc)),
        WebrtcProgress::OtherMessage(message) => on(Event::OtherMessage(message)),
        _ => {}
    }
}

fn report_iroh(step: IrohProgress, on: &mut (dyn FnMut(Event) + Send)) {
    on(Event::IrohStep(step.clone()));
    match step {
        IrohProgress::Delivered { .. } => on(Event::Delivered(Transport::Iroh)),
        IrohProgress::OtherMessage(message) => on(Event::OtherMessage(message)),
        _ => {}
    }
}

fn ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}
