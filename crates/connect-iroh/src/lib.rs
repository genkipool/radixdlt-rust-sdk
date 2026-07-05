//! radixdlt-connect-iroh — An [iroh](https://iroh.computer) (QUIC) peer-to-peer
//! transport for the RadixDLT Rust SDK.
//!
//! This is an alternative to `radixdlt-connect` (WebRTC). It does NOT talk to the
//! Radix mobile Wallet (which only speaks Radix Connect over WebRTC); instead it
//! connects two peers that both use the SDK — for example a pure-Rust desktop
//! signer, a server, or an IoT device. That makes it possible to run flows like
//! ROLA "log in with Radix" entirely in Rust, with no mobile wallet involved.
//!
//! Messages are JSON [`serde_json::Value`]s, length-prefixed over a single QUIC
//! bidirectional stream. The connecting side ([`IrohConnector::connect`]) sends
//! first; the accepting side ([`IrohConnector::accept`]) receives first.
//!
//! User-facing error text is localized to the system language.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr};
use radixdlt_i18n::{tr, Lang};
use serde_json::Value;

pub mod protocol;

/// Derives the `EndpointId` (as a string) from a 32-byte seed, without binding an
/// endpoint or touching the network. Useful to generate a hub locator (bootstrap
/// token) ahead of time; the same seed passed to [`IrohConnector::bind_with`]
/// yields the same id.
pub fn endpoint_id_from_seed(seed: &[u8; 32]) -> String {
    SecretKey::from_bytes(seed).public().to_string()
}

/// ALPN identifier for this transport.
pub const ALPN: &[u8] = b"radixdlt-connect-iroh/0";

/// Relay/discovery mode for [`IrohConnector::bind_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relay {
    /// Direct connections only (same host / LAN): no relay, no discovery.
    Disabled,
    /// n0 public relays + discovery: peers behind NAT can reach this endpoint
    /// over the internet by `EndpointId` alone.
    Enabled,
}

/// Errors from the iroh transport. `Display` is localized to the system language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrohError {
    /// Could not bind the local endpoint.
    Bind(String),
    /// Could not connect to the remote peer.
    Connect(String),
    /// Error accepting an incoming connection.
    Accept(String),
    /// QUIC stream read/write error.
    Stream(String),
    /// The message could not be (de)serialized.
    Protocol(String),
    /// The remote peer (the "wallet") rejected the request.
    Rejected(String),
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

/// A peer endpoint for the iroh transport.
pub struct IrohConnector {
    endpoint: Endpoint,
}

impl IrohConnector {
    /// Binds a local endpoint with the relay disabled (direct connections only) and an
    /// ephemeral identity. Equivalent to `bind_with(None, Relay::Disabled)`.
    pub async fn bind() -> Result<Self, IrohError> {
        Self::bind_with(None, Relay::Disabled).await
    }

    /// Binds with an optional FIXED identity and an explicit relay mode.
    ///
    /// - `secret` (32-byte seed): a persistent identity → a stable `EndpointId` (and a
    ///   stable [`id_ticket`](Self::id_ticket)) across restarts. `None` = ephemeral.
    ///   Use the same 32 bytes as a Radix account key to unify channel and ledger identity.
    /// - [`Relay::Enabled`]: n0 relays + discovery so peers can connect over the
    ///   internet (behind NAT) by `EndpointId` alone. [`Relay::Disabled`] = direct/LAN only.
    pub async fn bind_with(secret: Option<[u8; 32]>, relay: Relay) -> Result<Self, IrohError> {
        let bind_res = match relay {
            Relay::Enabled => {
                let mut b = Endpoint::builder(presets::N0).alpns(vec![ALPN.to_vec()]);
                if let Some(seed) = secret {
                    b = b.secret_key(SecretKey::from_bytes(&seed));
                }
                b.bind().await
            }
            Relay::Disabled => {
                let mut b = Endpoint::builder(presets::Minimal)
                    .alpns(vec![ALPN.to_vec()])
                    .relay_mode(RelayMode::Disabled);
                if let Some(seed) = secret {
                    b = b.secret_key(SecretKey::from_bytes(&seed));
                }
                b.bind().await
            }
        };
        let endpoint = bind_res.map_err(|e| IrohError::Bind(e.to_string()))?;
        Ok(IrohConnector { endpoint })
    }

    /// This endpoint's identity (its public key).
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// This endpoint's address as iroh computes it (may require discovery/relay to
    /// be fully populated).
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// An address built from the bound sockets, with unspecified (wildcard) IPs
    /// mapped to loopback — suitable for connecting two endpoints on the same host
    /// with no relay or discovery.
    pub fn local_addr(&self) -> EndpointAddr {
        let id = self.endpoint.id();
        let addrs: Vec<TransportAddr> = self
            .endpoint
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
            .collect();
        EndpointAddr::from_parts(id, addrs)
    }

    /// A shareable connection ticket (hex of the local address) that another peer
    /// can scan/paste to reach this endpoint. Suitable for same-host / LAN use; for
    /// internet peers behind NATs, enable an iroh relay and discovery.
    pub fn ticket(&self) -> String {
        let addr = self.local_addr();
        hex::encode(serde_json::to_vec(&addr).unwrap_or_default())
    }

    /// A ticket carrying ONLY the `EndpointId` (no transport addrs). With a persistent
    /// identity it is **stable across restarts**; the peer reaches it via discovery
    /// (requires `bind_with(_, Relay::Enabled)` on both ends). Use this for internet hubs.
    pub fn id_ticket(&self) -> String {
        let addr = EndpointAddr::from_parts(self.endpoint.id(), Vec::<TransportAddr>::new());
        hex::encode(serde_json::to_vec(&addr).unwrap_or_default())
    }

    /// Connects to a peer using a ticket produced by [`ticket`](Self::ticket).
    pub async fn connect_to_ticket(&self, ticket: &str) -> Result<IrohChannel, IrohError> {
        let bytes =
            hex::decode(ticket).map_err(|e| IrohError::Connect(format!("invalid ticket: {e}")))?;
        let addr: EndpointAddr = serde_json::from_slice(&bytes)
            .map_err(|e| IrohError::Connect(format!("invalid ticket: {e}")))?;
        self.connect(addr).await
    }

    /// This endpoint's `EndpointId` as a string (for advertising via mDNS/discovery).
    pub fn endpoint_id_string(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Connects using ONLY the peer's `EndpointId` (string), letting discovery resolve
    /// the rest. Used by LAN/mDNS auto-discovery: the node learns the hub's id (e.g. from
    /// an mDNS record) without a pasted ticket, then dials it.
    pub async fn connect_to_endpoint_id(&self, eid: &str) -> Result<IrohChannel, IrohError> {
        let id: EndpointId = eid
            .parse()
            .map_err(|e| IrohError::Connect(format!("invalid endpoint id: {e:?}")))?;
        let addr = EndpointAddr::from_parts(id, Vec::<TransportAddr>::new());
        self.connect(addr).await
    }

    /// Connects to a remote peer and opens a message channel. The caller sends the
    /// first message.
    pub async fn connect(&self, addr: impl Into<EndpointAddr>) -> Result<IrohChannel, IrohError> {
        let conn = self
            .endpoint
            .connect(addr, ALPN)
            .await
            .map_err(|e| IrohError::Connect(e.to_string()))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| IrohError::Stream(e.to_string()))?;
        Ok(IrohChannel { conn, send, recv })
    }

    /// Accepts an incoming connection and opens a message channel. The peer sends
    /// the first message.
    pub async fn accept(&self) -> Result<IrohChannel, IrohError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| IrohError::Accept("endpoint closed".to_string()))?;
        let conn = incoming
            .await
            .map_err(|e| IrohError::Accept(e.to_string()))?;
        let (send, recv) = conn
            .accept_bi()
            .await
            .map_err(|e| IrohError::Stream(e.to_string()))?;
        Ok(IrohChannel { conn, send, recv })
    }
}

/// A bidirectional JSON message channel over a QUIC stream.
pub struct IrohChannel {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

impl IrohChannel {
    /// Sends a JSON message (length-prefixed).
    pub async fn send_message(&mut self, message: &Value) -> Result<(), IrohError> {
        let bytes = serde_json::to_vec(message).map_err(|e| IrohError::Protocol(e.to_string()))?;
        let len = (bytes.len() as u32).to_be_bytes();
        self.send
            .write_all(&len)
            .await
            .map_err(|e| IrohError::Stream(e.to_string()))?;
        self.send
            .write_all(&bytes)
            .await
            .map_err(|e| IrohError::Stream(e.to_string()))?;
        Ok(())
    }

    /// Receives the next JSON message (length-prefixed).
    pub async fn recv_message(&mut self) -> Result<Value, IrohError> {
        let mut len = [0u8; 4];
        self.recv
            .read_exact(&mut len)
            .await
            .map_err(|e| IrohError::Stream(e.to_string()))?;
        let n = u32::from_be_bytes(len) as usize;
        let mut buf = vec![0u8; n];
        self.recv
            .read_exact(&mut buf)
            .await
            .map_err(|e| IrohError::Stream(e.to_string()))?;
        serde_json::from_slice(&buf).map_err(|e| IrohError::Protocol(e.to_string()))
    }

    /// Signals that no more messages will be sent on this channel.
    pub fn finish(&mut self) {
        let _ = self.send.finish();
    }

    /// Gracefully closes the connection (lets the peer know we are done).
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"done");
    }

    /// Waits until the peer closes the connection. Use this after sending a final
    /// message and calling [`finish`](Self::finish) so the data is delivered before
    /// the connection is dropped.
    pub async fn wait_closed(&self) {
        let _ = self.conn.closed().await;
    }
}
