//! radixdlt-connect-iroh — An [iroh](https://iroh.computer) (QUIC) transport for the
//! RadixDLT Rust SDK, an alternative to `radixdlt-connect` (WebRTC). Two uses:
//!
//! * **The Radix mobile wallet** ([`mobile`], [`MobileWalletConnector`]): "Radix Connect over
//!   iroh v1", the protocol the Radix Wallet and the Radix Connector Extension speak. A Rust
//!   server pairs with the wallet through the usual QR code and sends it wallet interactions
//!   directly — no browser, no extension. Same API shape as `radixdlt_connect::Connector`.
//! * **SDK-to-SDK** ([`IrohConnector`], [`IrohChannel`], `protocol`): two peers that both use the
//!   SDK — a pure-Rust desktop signer, a server, an IoT device. Messages are JSON
//!   [`serde_json::Value`]s, each on a QUIC stream of its own and confirmed by the receiver
//!   (ALPN [`ALPN`]; see [`IrohChannel`] and `docs/PROTOCOL.md`). Every message is bounded in
//!   size ([`Limits`]), every wait for a started message is bounded in time, and sending and
//!   receiving are cancel-safe. This protocol is the SDK's own: the mobile wallet does not speak
//!   it.
//!
//! User-facing error text is localized to the system language.

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

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{
    presets, AfterHandshakeOutcome, Connection, EndpointHooks, Incoming, PortmapperConfig,
    QuicTransportConfig, Side, VarInt,
};
use iroh::{Endpoint, RelayMode, SecretKey, TransportAddr};
use radixdlt_i18n::{tr, Lang};
use tokio::time;
use zeroize::Zeroizing;

mod channel;
pub mod mobile;

pub use channel::{
    IrohChannel, Limits, DEFAULT_CLOSE_TIMEOUT, DEFAULT_MAX_MESSAGE_SIZE, DEFAULT_MESSAGE_TIMEOUT,
};
pub use iroh::{EndpointAddr, EndpointId};
pub use mobile::{MobileError, MobileWalletConnector};

/// Answering wallet requests. Behind the `wallet` feature because it needs the Scrypto
/// engine, whose exact `regex` pin cannot coexist with the mobile wallet's webrtc stack.
#[cfg(feature = "wallet")]
pub mod protocol;

/// Derives the `EndpointId` (as a string) from a 32-byte seed, without binding an
/// endpoint or touching the network. Useful to generate a hub locator (bootstrap
/// token) ahead of time; the same seed passed to [`IrohConnector::bind_with`]
/// yields the same id.
pub fn endpoint_id_from_seed(seed: &[u8; 32]) -> String {
    SecretKey::from_bytes(seed).public().to_string()
}

/// ALPN identifier for this transport. Version 1: a stream per message, with a confirmation.
/// Version 0 (one stream for the whole channel, no confirmations) is not spoken any more: a peer
/// still on it fails at the handshake instead of misreading the other's messages.
pub const ALPN: &[u8] = b"radixdlt-connect-iroh/1";

/// How many streams a peer may have open towards us at once. Messages are sequential, so one is
/// enough; the rest is headroom for sends the peer abandoned. Each stream may buffer up to QUIC's
/// stream window, so this is also what bounds what one peer can make us hold in memory.
const MAX_INCOMING_STREAMS: u32 = 8;

/// QUIC close code of a connection refused by [`Builder::allow_only`] / [`Builder::admit`].
const CLOSE_NOT_ADMITTED: u32 = 1;

/// Relay/discovery mode for [`IrohConnector::bind_with`] and [`Builder::relay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relay {
    /// Direct connections only (same host / LAN): no relay, no address lookup, and no port
    /// mapping on the router. Peers need a [`ticket`](IrohConnector::ticket) (it carries the
    /// addresses); an `EndpointId` alone cannot be resolved.
    Disabled,
    /// n0 public relays + address lookup (pkarr/DNS): peers behind NAT can reach this endpoint
    /// over the internet by `EndpointId` alone. Port mapping (UPnP / NAT-PMP / PCP) is on, so the
    /// local router may be asked to open a port for direct connections.
    Enabled,
}

/// Errors from the iroh transport. `Display` is localized to the system language.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IrohError {
    /// Could not bind the local endpoint.
    Bind(String),
    /// Could not connect to the remote peer.
    Connect(String),
    /// Error accepting an incoming connection.
    Accept(String),
    /// The connection was closed or lost, or a stream failed.
    Stream(String),
    /// The message could not be (de)serialized, or did not follow the protocol.
    Protocol(String),
    /// The remote peer (the "wallet") rejected the request.
    Rejected(String),
    /// A message over the size limit of THIS side ([`Limits::max_message_size`]): one we were
    /// asked to send (nothing was sent), or one the peer announced (nothing was allocated for it).
    TooLarge {
        /// The message's size in bytes (as announced, for an incoming one).
        size: u64,
        /// The limit it exceeds.
        max: usize,
    },
    /// The peer refused a message we sent.
    Refused {
        /// `true`: over the peer's size limit. `false`: not one JSON document of the length it
        /// announced.
        too_large: bool,
    },
    /// A message did not arrive whole, or was not confirmed, in time ([`Limits::message_timeout`]).
    Timeout {
        /// `true`: our message was not confirmed (the peer may still take it). `false`: the
        /// peer's message started but did not arrive whole.
        sending: bool,
        /// The bound that was exceeded.
        after: Duration,
    },
}

impl std::fmt::Display for IrohError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            IrohError::Bind(e) => tr!(
                lang,
                format!("could not bind the iroh endpoint: {e}"),
                format!("no se pudo enlazar el endpoint iroh: {e}")
            ),
            IrohError::Connect(e) => tr!(
                lang,
                format!("could not connect to the peer: {e}"),
                format!("no se pudo conectar con el peer: {e}")
            ),
            IrohError::Accept(e) => tr!(
                lang,
                format!("error accepting the connection: {e}"),
                format!("error aceptando la conexión: {e}")
            ),
            IrohError::Stream(e) => tr!(
                lang,
                format!("QUIC stream error: {e}"),
                format!("error de stream QUIC: {e}")
            ),
            IrohError::Protocol(e) => tr!(
                lang,
                format!("protocol error: {e}"),
                format!("error de protocolo: {e}")
            ),
            IrohError::Rejected(e) => tr!(
                lang,
                format!("the peer rejected the request: {e}"),
                format!("el peer rechazó la solicitud: {e}")
            ),
            IrohError::TooLarge { size, max } => tr!(
                lang,
                format!("a message of {size} bytes is over the limit of {max} bytes"),
                format!("un mensaje de {size} bytes supera el límite de {max} bytes")
            ),
            IrohError::Refused { too_large: true } => tr!(
                lang,
                "the peer refused the message: it is over the peer's size limit".to_string(),
                "el peer rechazó el mensaje: supera su tamaño máximo".to_string()
            ),
            IrohError::Refused { too_large: false } => tr!(
                lang,
                "the peer refused the message: it is not JSON of the length it announced".to_string(),
                "el peer rechazó el mensaje: no es un JSON de la longitud anunciada".to_string()
            ),
            IrohError::Timeout { sending: true, after } => tr!(
                lang,
                format!("the peer did not confirm the message within {after:?} (it may still take it)"),
                format!("el peer no confirmó el mensaje en {after:?} (aún podría recibirlo)")
            ),
            IrohError::Timeout {
                sending: false,
                after,
            } => tr!(
                lang,
                format!("the peer's message did not arrive whole within {after:?}"),
                format!("el mensaje del peer no llegó completo en {after:?}")
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for IrohError {}

impl From<radixdlt_connect_types::WalletInteractionError> for IrohError {
    fn from(e: radixdlt_connect_types::WalletInteractionError) -> Self {
        use radixdlt_connect_types::WalletInteractionError as W;
        match e {
            W::WalletRejected(s) => IrohError::Rejected(s),
            W::Protocol(s) => IrohError::Protocol(s),
        }
    }
}

/// Which peers may connect to an endpoint ([`Builder::allow_only`], [`Builder::admit`]).
type Admit = Arc<dyn Fn(&EndpointId) -> bool + Send + Sync>;

/// Refuses, right after the handshake, an incoming connection whose authenticated `EndpointId`
/// is not admitted: the application never sees it (not even as a [`Pending`]'s channel).
struct Admission(Admit);

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Admission")
    }
}

impl EndpointHooks for Admission {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        // Dialling out is not filtered: whoever dials chose the id, and the handshake proves it.
        if conn.side() == Side::Client || (self.0)(&conn.remote_id()) {
            AfterHandshakeOutcome::accept()
        } else {
            AfterHandshakeOutcome::Reject {
                error_code: VarInt::from_u32(CLOSE_NOT_ADMITTED),
                reason: b"not admitted".to_vec(),
            }
        }
    }
}

/// How to bind an [`IrohConnector`]: identity, relay mode, who may connect, and the channel
/// [`Limits`]. From [`IrohConnector::builder`].
pub struct Builder {
    secret: Option<Zeroizing<[u8; 32]>>,
    relay: Relay,
    admit: Option<Admit>,
    limits: Limits,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the seed: it IS the endpoint's private key.
        f.debug_struct("Builder")
            .field("persistent_identity", &self.secret.is_some())
            .field("relay", &self.relay)
            .field("admission", &self.admit.is_some())
            .field("limits", &self.limits)
            .finish()
    }
}

impl Builder {
    /// A FIXED identity from a 32-byte seed: a stable `EndpointId` (and a stable
    /// [`id_ticket`](IrohConnector::id_ticket)) across restarts. Without it the identity is
    /// ephemeral.
    ///
    /// Use a key dedicated to this endpoint — not a Radix account's key. The `EndpointId` is
    /// public: peers and relays see it, and with [`Relay::Enabled`] it is published in n0's
    /// address lookup together with this endpoint's relay. The same key as an account would tie
    /// that account to this machine's network presence, and would have one private key sign in
    /// two protocols (the QUIC handshake and Radix transactions). See the README's "Identity".
    #[must_use]
    pub fn secret(mut self, seed: [u8; 32]) -> Self {
        self.secret = Some(Zeroizing::new(seed));
        self
    }

    /// The relay mode ([`Relay::Disabled`] by default).
    #[must_use]
    pub fn relay(mut self, relay: Relay) -> Self {
        self.relay = relay;
        self
    }

    /// Only these peers may connect to this endpoint. Any other is refused right after the
    /// handshake (which authenticates its `EndpointId`), before [`IrohConnector::accept`] or
    /// [`Pending::ready`] return it. Dialling out is not restricted.
    #[must_use]
    pub fn allow_only(self, peers: impl IntoIterator<Item = EndpointId>) -> Self {
        let peers: HashSet<EndpointId> = peers.into_iter().collect();
        self.admit(move |peer| peers.contains(peer))
    }

    /// Decides, for each incoming connection, whether its peer may connect — as
    /// [`allow_only`](Self::allow_only), for a list that changes while the endpoint runs. Keep it
    /// fast: it runs inside iroh's handshake.
    #[must_use]
    pub fn admit(mut self, admit: impl Fn(&EndpointId) -> bool + Send + Sync + 'static) -> Self {
        self.admit = Some(Arc::new(admit));
        self
    }

    /// The bounds of every channel of this endpoint ([`Limits::default`] otherwise).
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Binds the endpoint.
    ///
    /// # Errors
    /// [`IrohError::Bind`] when the sockets cannot be bound.
    pub async fn bind(self) -> Result<IrohConnector, IrohError> {
        let transport = QuicTransportConfig::builder()
            .max_concurrent_bidi_streams(VarInt::from_u32(MAX_INCOMING_STREAMS))
            // The protocol has no use for them: none may be opened towards us.
            .max_concurrent_uni_streams(VarInt::from_u32(0))
            .build();
        let mut builder = match self.relay {
            Relay::Enabled => Endpoint::builder(presets::N0),
            // Same host / LAN: nothing to publish, and no reason to ask the router for a port.
            Relay::Disabled => Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .portmapper_config(PortmapperConfig::Disabled),
        }
        .alpns(vec![ALPN.to_vec()])
        .transport_config(transport);
        if let Some(seed) = &self.secret {
            builder = builder.secret_key(SecretKey::from_bytes(seed));
        }
        if let Some(admit) = &self.admit {
            builder = builder.hooks(Admission(Arc::clone(admit)));
        }
        let endpoint = builder.bind().await.map_err(|e| IrohError::Bind(e.to_string()))?;
        Ok(IrohConnector {
            endpoint,
            limits: self.limits,
        })
    }
}

/// A peer endpoint for the SDK-to-SDK transport.
///
/// Call [`close`](Self::close) when done with it. Dropped without it, the endpoint is closed in
/// the background when a tokio runtime is there to do it; otherwise iroh aborts it (logging an
/// error) and its peers only learn when their connections time out.
#[derive(Debug)]
pub struct IrohConnector {
    endpoint: Endpoint,
    limits: Limits,
}

impl IrohConnector {
    /// How to bind one: identity, relay mode, who may connect, limits.
    pub fn builder() -> Builder {
        Builder {
            secret: None,
            relay: Relay::Disabled,
            admit: None,
            limits: Limits::default(),
        }
    }

    /// Binds a local endpoint with the relay disabled (direct connections only), an ephemeral
    /// identity, anyone admitted and the default [`Limits`]. Equivalent to
    /// `bind_with(None, Relay::Disabled)`.
    ///
    /// # Errors
    /// [`IrohError::Bind`] when the sockets cannot be bound.
    pub async fn bind() -> Result<Self, IrohError> {
        Self::builder().bind().await
    }

    /// Binds with an optional FIXED identity and an explicit relay mode (see
    /// [`Builder::secret`] — a dedicated key, not an account's — and [`Relay`]). For an allowlist
    /// or other limits, use [`builder`](Self::builder).
    ///
    /// # Errors
    /// [`IrohError::Bind`] when the sockets cannot be bound.
    pub async fn bind_with(secret: Option<[u8; 32]>, relay: Relay) -> Result<Self, IrohError> {
        let builder = Self::builder().relay(relay);
        match secret {
            Some(seed) => builder.secret(seed).bind().await,
            None => builder.bind().await,
        }
    }

    /// This endpoint's identity (its public key).
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The bounds this endpoint's channels apply.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// This endpoint's address as iroh computes it (may require discovery/relay to
    /// be fully populated).
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// An address built from the bound sockets, with unspecified (wildcard) IPs mapped to
    /// loopback — for connecting two endpoints on the same host with no relay or discovery.
    pub fn local_addr(&self) -> EndpointAddr {
        EndpointAddr::from_parts(self.endpoint.id(), self.loopback_addrs())
    }

    fn loopback_addrs(&self) -> Vec<TransportAddr> {
        self.endpoint
            .bound_sockets()
            .into_iter()
            .map(|s| {
                let ip = match s.ip() {
                    IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    other => other,
                };
                TransportAddr::Ip(SocketAddr::new(ip, s.port()))
            })
            .collect()
    }

    /// A shareable connection ticket (hex of the JSON `EndpointAddr`) another peer can paste or
    /// scan to reach this endpoint: the `EndpointId`, the addresses iroh knows for it — this
    /// machine's interface addresses (LAN), and with [`Relay::Enabled`] its relay and public
    /// address once known — plus loopback, for a peer on the same host.
    ///
    /// The interface addresses appear as iroh learns them, shortly after binding. A peer on
    /// another network needs [`Relay::Enabled`] on both ends.
    pub fn ticket(&self) -> String {
        let known = self.endpoint.addr().addrs.into_iter();
        let addr = EndpointAddr::from_parts(self.endpoint.id(), known.chain(self.loopback_addrs()));
        hex::encode(serde_json::to_vec(&addr).unwrap_or_default())
    }

    /// A ticket carrying ONLY the `EndpointId` (no transport addrs). With a persistent
    /// identity it is **stable across restarts**; the peer reaches it via address lookup
    /// (requires `Relay::Enabled` on both ends). Use this for internet hubs.
    pub fn id_ticket(&self) -> String {
        let addr = EndpointAddr::from_parts(self.endpoint.id(), Vec::<TransportAddr>::new());
        hex::encode(serde_json::to_vec(&addr).unwrap_or_default())
    }

    /// Connects to a peer using a ticket produced by [`ticket`](Self::ticket) or
    /// [`id_ticket`](Self::id_ticket).
    ///
    /// # Errors
    /// [`IrohError::Connect`] for a malformed ticket or an unreachable peer (or one that refuses
    /// us).
    pub async fn connect_to_ticket(&self, ticket: &str) -> Result<IrohChannel, IrohError> {
        let bytes = hex::decode(ticket).map_err(|e| IrohError::Connect(format!("invalid ticket: {e}")))?;
        let addr: EndpointAddr =
            serde_json::from_slice(&bytes).map_err(|e| IrohError::Connect(format!("invalid ticket: {e}")))?;
        self.connect(addr).await
    }

    /// This endpoint's `EndpointId` as a string.
    pub fn endpoint_id_string(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Connects using ONLY the peer's `EndpointId` (string), letting iroh's address lookup
    /// resolve the rest: n0's pkarr/DNS, which needs [`Relay::Enabled`] on both ends. With
    /// [`Relay::Disabled`] there is no lookup — use a [`ticket`](Self::ticket) (an application
    /// may advertise one on its LAN, e.g. over mDNS of its own; this crate does not).
    ///
    /// # Errors
    /// [`IrohError::Connect`] for a malformed id or an unreachable peer.
    pub async fn connect_to_endpoint_id(&self, eid: &str) -> Result<IrohChannel, IrohError> {
        let id: EndpointId = eid
            .parse()
            .map_err(|e| IrohError::Connect(format!("invalid endpoint id: {e:?}")))?;
        let addr = EndpointAddr::from_parts(id, Vec::<TransportAddr>::new());
        self.connect(addr).await
    }

    /// Connects to a remote peer and opens a message channel. The connection is authenticated:
    /// the handshake fails unless the peer holds the key of `addr`'s `EndpointId`.
    ///
    /// # Errors
    /// [`IrohError::Connect`] when the peer cannot be reached, does not speak this protocol's
    /// version ([`ALPN`]), or refuses us.
    pub async fn connect(&self, addr: impl Into<EndpointAddr>) -> Result<IrohChannel, IrohError> {
        let conn = self
            .endpoint
            .connect(addr, ALPN)
            .await
            .map_err(|e| IrohError::Connect(e.to_string()))?;
        Ok(IrohChannel::new(conn, self.limits))
    }

    /// Accepts an incoming connection and opens a message channel. The peer sends
    /// the first message.
    ///
    /// # Errors
    /// As [`accept_pending`](Self::accept_pending) and [`Pending::ready`].
    pub async fn accept(&self) -> Result<IrohChannel, IrohError> {
        self.accept_pending().await?.ready().await
    }

    /// The next connection a peer opens, before its handshake: what a server that serves many
    /// peers loops on, finishing each one ([`Pending::ready`]) in a task of its own.
    ///
    /// [`IrohConnector::accept`] waits for the whole handshake AND for the peer's first message
    /// to start before it returns; looped on, one slow peer — or one that connects and never
    /// sends — holds every other peer at the door until it gives up.
    ///
    /// # Errors
    /// [`IrohError::Accept`] when the endpoint has been closed.
    pub async fn accept_pending(&self) -> Result<Pending, IrohError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| IrohError::Accept("endpoint closed".to_string()))?;
        Ok(Pending {
            incoming,
            limits: self.limits,
        })
    }

    /// Closes the endpoint: every connection it has (and so every channel) is closed and its
    /// peers are told; [`accept_pending`](Self::accept_pending) fails from then on. Waits at most
    /// [`Limits::close_timeout`] for the peers to acknowledge. Idempotent.
    pub async fn close(&self) {
        let _ = time::timeout(self.limits.close_timeout, self.endpoint.close()).await;
    }
}

impl Drop for IrohConnector {
    fn drop(&mut self) {
        if self.endpoint.is_closed() {
            return;
        }
        // Dropped without `close`: iroh would abort it, and its peers would only find out when
        // their connections time out. Closed in the background instead, when there is a runtime.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let endpoint = self.endpoint.clone();
            let within = self.limits.close_timeout;
            runtime.spawn(async move {
                let _ = time::timeout(within, endpoint.close()).await;
            });
        }
    }
}

/// A connection a peer has opened whose handshake is not finished yet
/// ([`IrohConnector::accept_pending`]). Its peer is not authenticated yet, so it has no
/// `remote_id`: the channel [`ready`](Self::ready) returns does ([`IrohChannel::remote_id`]), and
/// [`Builder::allow_only`] refuses unwanted peers before that.
#[derive(Debug)]
pub struct Pending {
    incoming: Incoming,
    limits: Limits,
}

impl Pending {
    /// Finishes the handshake and waits for the peer's first message to start. Bound it with a
    /// timeout: a peer that never sends would otherwise be waited on for as long as the
    /// connection lives.
    ///
    /// # Errors
    /// [`IrohError::Accept`] when the handshake fails or the peer is not admitted,
    /// [`IrohError::Stream`] when the connection ends before the peer sends anything.
    pub async fn ready(self) -> Result<IrohChannel, IrohError> {
        let conn = self
            .incoming
            .await
            .map_err(|e| IrohError::Accept(e.to_string()))?;
        let first = conn
            .accept_bi()
            .await
            .map_err(|e| IrohError::Stream(e.to_string()))?;
        Ok(IrohChannel::with_first(conn, self.limits, first))
    }
}
