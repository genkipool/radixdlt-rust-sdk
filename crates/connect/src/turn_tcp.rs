//! A TURN allocation reached over TCP/TLS, presented to WebRTC as a UDP socket.
//!
//! # Why this exists
//!
//! WebRTC wants UDP, and plenty of places will not give it: corporate firewalls, guest
//! networks, and most serverless hosts. TURN has always had an answer — RFC 5766 §2.1 lets a
//! client reach the relay over TCP, and RFC 6544 carries that into ICE — but the Rust stack
//! does not implement it. `webrtc`'s relayer skips any URL that is `turns:` or
//! `?transport=tcp` and hardcodes `TransportProtocol::UDP`, so a `turns:` entry in an ICE
//! configuration is silently discarded and a UDP-less network has no path at all.
//!
//! What is missing is only the I/O. The Sans-I/O TURN client already takes a
//! `transport_protocol` of UDP *or* TCP and tags what it emits accordingly; nobody had
//! written the half that owns a stream. This module is that half.
//!
//! # How it works
//!
//! `Runtime` and `AsyncUdpSocket` are public traits, so rather than patching the relayer we
//! hand WebRTC a runtime whose "UDP socket" is the allocation itself:
//!
//! * [`TurnTcpRuntime::connect`] opens TCP (TLS for `turns:`), allocates, and learns the
//!   relayed address the server assigned us.
//! * `local_addr()` reports that relayed address, so WebRTC gathers a **host** candidate
//!   pointing at the relay and never learns there is one underneath.
//! * `poll_send`/`poll_recv` hand datagrams to a driver task that wraps them in TURN
//!   framing and moves them over the stream.
//!
//! Because the relaying happens *below* WebRTC, a peer connection using this runtime should
//! be configured with **no ICE servers** — it has nothing left to gather — and the default
//! transport policy rather than `Relay`, which would discard the very candidate that works.
//! [`Connector::with_turn_tcp`](crate::Connector::with_turn_tcp) wires all of that up.
//!
//! # What the relay will not do for you
//!
//! A TURN server drops traffic to and from any peer it has no permission for, and a
//! permission takes a round trip. The peer's address is not known until ICE offers it, so
//! permissions are created lazily on the first datagram to a new address and that datagram
//! (with any that follow) waits in a small queue until the server confirms. The queue is
//! bounded: a peer that never gets a permission must not grow memory without limit.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use rtc::turn::client::{Client as TurnClient, ClientConfig as TurnClientConfig, Event as TurnEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use webrtc::runtime::{
    AsyncInterval, AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket, JoinHandle, RecvMeta, Runtime,
    TokioRuntime, Transmit,
};

use crate::error::ConnectError;

/// Datagrams held for a peer whose permission is still in flight. Sized to cover an ICE
/// connectivity-check burst; beyond that the peer is not answering and dropping is right.
const MAX_QUEUED_PER_PEER: usize = 32;

/// Cap on a single TURN frame, so a hostile or broken server cannot make us allocate
/// without bound while we wait for the rest of a length that never arrives.
const MAX_FRAME: usize = 64 * 1024;

/// How long to wait for the server to answer our Allocate before giving up.
const ALLOCATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a relay lives and how to authenticate to it.
#[derive(Debug, Clone)]
pub struct TurnTcpServer {
    /// Host and port, e.g. `standard.relay.metered.ca:443`.
    pub addr: String,
    /// Whether to wrap the TCP stream in TLS (`turns:` rather than `turn:`).
    pub tls: bool,
    /// Long-term credential username.
    pub username: String,
    /// Long-term credential password.
    pub password: String,
}

impl TurnTcpServer {
    /// Reads a `turn:`/`turns:` URL, which is how ICE configuration names a relay.
    ///
    /// Only TCP is accepted: a URL naming UDP would be answered by the ordinary WebRTC path,
    /// and silently treating it as TCP here would connect somewhere the operator did not ask
    /// for. `turns:` implies TCP, since TURN over TLS has no datagram form.
    pub fn parse(url: &str, username: &str, password: &str) -> Result<Self, ConnectError> {
        let (scheme, rest) = url
            .split_once(':')
            .ok_or_else(|| ConnectError::WebRtc(format!("not a TURN url: {url}")))?;
        let tls = match scheme {
            "turns" => true,
            "turn" => false,
            other => return Err(ConnectError::WebRtc(format!("not a TURN url: {other} scheme"))),
        };
        let (host_port, query) = match rest.split_once('?') {
            Some((h, q)) => (h, Some(q)),
            None => (rest, None),
        };
        let tcp = query
            .and_then(|q| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("transport="))
                    .map(|t| t.eq_ignore_ascii_case("tcp"))
            })
            // `turns:` is TLS, and there is no TLS over datagrams here, so TCP is implied.
            .unwrap_or(tls);
        if !tcp {
            return Err(ConnectError::WebRtc(format!(
                "{url} names UDP transport; this path is for TCP relays"
            )));
        }
        // A URL with no port takes the IANA default for its scheme.
        let addr = if host_port.contains(':') {
            host_port.to_string()
        } else if tls {
            format!("{host_port}:5349")
        } else {
            format!("{host_port}:3478")
        };
        Ok(TurnTcpServer {
            addr,
            tls,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    /// The host name, for TLS server-name verification.
    fn host(&self) -> &str {
        self.addr.rsplit_once(':').map_or(&self.addr, |(h, _)| h)
    }
}

/// A duplex stream to the relay, plain or TLS. Both are driven identically once open.
enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Stream {
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.write_all(buf).await,
            Stream::Tls(s) => s.write_all(buf).await,
        }
    }

    async fn read_buf(&mut self, buf: &mut BytesMut) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read_buf(buf).await,
            Stream::Tls(s) => s.read_buf(buf).await,
        }
    }
}

/// Length of the TURN frame at the front of `buf`, or `None` while it is still short.
///
/// Two framings share the stream. RFC 5389 §6 puts STUN's most significant two bits at zero
/// and its length (excluding the 20-byte header) at bytes 2..4; RFC 5766 §11 gives
/// ChannelData a number of 0x4000..=0x7FFF and a 4-byte header, and §11.5 requires the whole
/// thing be padded to a multiple of four when it travels over a stream.
fn frame_len(buf: &[u8]) -> Result<Option<usize>, ConnectError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let declared = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let total = if buf[0] & 0xC0 == 0 {
        20 + declared
    } else {
        let padded = declared.div_ceil(4) * 4;
        4 + padded
    };
    if total > MAX_FRAME {
        return Err(ConnectError::WebRtc(format!(
            "relay sent a {total}-byte frame, over the {MAX_FRAME} limit"
        )));
    }
    Ok((buf.len() >= total).then_some(total))
}

/// What the socket asks the driver to do.
enum Command {
    /// Send this payload to this peer through the allocation.
    Send(BytesMut, SocketAddr),
}

/// The TURN allocation, wearing the shape of a UDP socket.
struct TurnSocket {
    /// The address the relay allocated for us. This is what WebRTC advertises.
    relayed: SocketAddr,
    commands: mpsc::UnboundedSender<Command>,
    inbound: AsyncMutex<mpsc::UnboundedReceiver<(BytesMut, SocketAddr)>>,
}

impl fmt::Debug for TurnSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnSocket")
            .field("relayed", &self.relayed)
            .finish()
    }
}

impl AsyncUdpSocket for TurnSocket {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.relayed)
    }

    fn poll_send(&self, _cx: &mut Context<'_>, transmit: &Transmit<'_>) -> Poll<io::Result<usize>> {
        // The driver's queue is unbounded, so a send is always immediately ready. Segmented
        // sends (GSO) never reach us: `max_gso_segments` reports 1.
        let payload = BytesMut::from(transmit.contents);
        let len = payload.len();
        match self.commands.send(Command::Send(payload, transmit.destination)) {
            Ok(()) => Poll::Ready(Ok(len)),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the TURN relay connection is gone",
            ))),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() || meta.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // One datagram per call: the relay delivers them one frame at a time anyway, so
        // there is nothing for batching to coalesce.
        let mut guard = match Box::pin(self.inbound.lock()).as_mut().poll(cx) {
            Poll::Ready(g) => g,
            Poll::Pending => return Poll::Pending,
        };
        match guard.poll_recv(cx) {
            Poll::Ready(Some((data, from))) => {
                let n = data.len().min(bufs[0].len());
                bufs[0][..n].copy_from_slice(&data[..n]);
                // `RecvMeta` is #[non_exhaustive], so it can only be built by default and
                // then filled in -- a struct expression will not compile against it.
                let mut m = RecvMeta::default();
                m.addr = from;
                m.len = n;
                // Must be at least 1: callers divide by it to de-segment.
                m.stride = n.max(1);
                meta[0] = m;
                Poll::Ready(Ok(1))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the TURN relay connection is gone",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A [`Runtime`] that hands WebRTC a TURN-over-TCP allocation in place of a UDP socket.
///
/// Everything except socket creation is the ordinary Tokio runtime; only `wrap_udp_socket`
/// differs, and it ignores the socket it is given because the allocation already exists.
pub struct TurnTcpRuntime {
    inner: TokioRuntime,
    socket: Arc<TurnSocket>,
}

impl fmt::Debug for TurnTcpRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnTcpRuntime")
            .field("relayed", &self.socket.relayed)
            .finish()
    }
}

impl TurnTcpRuntime {
    /// Opens the relay connection and allocates, returning once the relayed address is known.
    ///
    /// The allocation is live from here on: the driver task refreshes it, so the caller only
    /// has to keep the runtime alive for as long as the peer connection.
    pub async fn connect(server: &TurnTcpServer) -> Result<Arc<Self>, ConnectError> {
        let stream = open_stream(server).await?;

        let peer = tokio::net::lookup_host(&server.addr)
            .await
            .map_err(|e| ConnectError::WebRtc(format!("resolving {}: {e}", server.addr)))?
            .next()
            .ok_or_else(|| ConnectError::WebRtc(format!("{} resolved to nothing", server.addr)))?;

        let mut client = TurnClient::new(TurnClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: peer.to_string(),
            // The stream's own local address is irrelevant to the protocol; what matters is
            // that everything is tagged TCP so the client frames it for a stream.
            local_addr: "0.0.0.0:0".parse().unwrap_or(peer),
            transport_protocol: TransportProtocol::TCP,
            username: server.username.clone(),
            password: server.password.clone(),
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
        })
        .map_err(|e| ConnectError::WebRtc(format!("TURN client: {e}")))?;

        client
            .allocate()
            .map_err(|e| ConnectError::WebRtc(format!("TURN allocate: {e}")))?;

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<(BytesMut, SocketAddr)>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<SocketAddr, String>>();

        tokio::spawn(drive(stream, client, peer, cmd_rx, in_tx, ready_tx));

        let relayed = match tokio::time::timeout(ALLOCATE_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(addr))) => addr,
            Ok(Ok(Err(e))) => return Err(ConnectError::WebRtc(format!("TURN allocate: {e}"))),
            Ok(Err(_)) => {
                return Err(ConnectError::WebRtc(
                    "the TURN driver stopped before allocating".into(),
                ))
            }
            Err(_) => return Err(ConnectError::WebRtc("TURN allocate timed out".into())),
        };

        Ok(Arc::new(TurnTcpRuntime {
            inner: TokioRuntime,
            socket: Arc::new(TurnSocket {
                relayed,
                commands: cmd_tx,
                inbound: AsyncMutex::new(in_rx),
            }),
        }))
    }

    /// The address the relay allocated, which is what peers will be told to reach.
    pub fn relayed_addr(&self) -> SocketAddr {
        self.socket.relayed
    }
}

impl Runtime for TurnTcpRuntime {
    fn spawn(&self, future: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>) -> Box<dyn JoinHandle> {
        self.inner.spawn(future)
    }

    /// Ignores `_socket`: the allocation is the socket, and it already exists.
    fn wrap_udp_socket(&self, _socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Ok(Arc::clone(&self.socket) as Arc<dyn AsyncUdpSocket>)
    }

    fn wrap_tcp_listener(&self, listener: std::net::TcpListener) -> io::Result<Arc<dyn AsyncTcpListener>> {
        self.inner.wrap_tcp_listener(listener)
    }

    fn connect_tcp<'a>(
        &'a self,
        remote_addr: SocketAddr,
    ) -> std::pin::Pin<Box<dyn Future<Output = io::Result<Arc<dyn AsyncTcpStream>>> + Send + 'a>> {
        self.inner.connect_tcp(remote_addr)
    }

    fn resolve_host<'a>(
        &'a self,
        host: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        self.inner.resolve_host(host)
    }

    fn sleep(&self, duration: Duration) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.inner.sleep(duration)
    }

    fn interval(&self, period: Duration) -> Box<dyn AsyncInterval> {
        self.inner.interval(period)
    }

    fn block_on(&self, future: std::pin::Pin<Box<dyn Future<Output = ()> + '_>>) {
        self.inner.block_on(future)
    }

    fn name(&self) -> &'static str {
        "turn-tcp"
    }
}

/// Opens the transport to the relay: TCP, wrapped in TLS when the URL said `turns:`.
async fn open_stream(server: &TurnTcpServer) -> Result<Stream, ConnectError> {
    let tcp = TcpStream::connect(&server.addr)
        .await
        .map_err(|e| ConnectError::WebRtc(format!("connecting to {}: {e}", server.addr)))?;
    // Relays answer request/response promptly and the frames are small, so waiting to
    // coalesce them only adds latency to every connectivity check.
    let _ = tcp.set_nodelay(true);

    if !server.tls {
        return Ok(Stream::Plain(tcp));
    }

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(server.host().to_string())
        .map_err(|e| ConnectError::WebRtc(format!("bad TLS server name: {e}")))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| ConnectError::WebRtc(format!("TLS to {}: {e}", server.addr)))?;
    Ok(Stream::Tls(Box::new(tls)))
}

/// Owns the stream and the TURN client for the life of the allocation.
///
/// Everything the protocol needs happens here: framing in both directions, permissions,
/// and the refresh timers that keep the allocation from expiring.
async fn drive(
    mut stream: Stream,
    mut client: TurnClient,
    server: SocketAddr,
    mut commands: mpsc::UnboundedReceiver<Command>,
    inbound: mpsc::UnboundedSender<(BytesMut, SocketAddr)>,
    ready: oneshot::Sender<Result<SocketAddr, String>>,
) {
    let mut read_buf = BytesMut::with_capacity(8 * 1024);
    let mut relayed: Option<SocketAddr> = None;
    let mut ready = Some(ready);
    let mut permitted: HashSet<SocketAddr> = HashSet::new();
    let mut requested: HashSet<SocketAddr> = HashSet::new();
    let mut queued: HashMap<SocketAddr, VecDeque<BytesMut>> = HashMap::new();

    loop {
        // Anything the client wants on the wire goes out first, so a response is never
        // waited on before its request has left.
        while let Some(out) = client.poll_write() {
            if stream.write_all(&out.message).await.is_err() {
                break;
            }
        }

        // Then whatever the client concluded from what has already arrived.
        while let Some(event) = client.poll_event() {
            match event {
                TurnEvent::AllocateResponse(_, addr) => {
                    relayed = Some(addr);
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Ok(addr));
                    }
                }
                TurnEvent::AllocateError(_, e) => {
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                }
                TurnEvent::CreatePermissionResponse(_, peer) => {
                    permitted.insert(peer);
                    // Whatever was waiting on this permission can go now.
                    if let (Some(addr), Some(mut waiting)) = (relayed, queued.remove(&peer)) {
                        if let Ok(mut relay) = client.relay(addr) {
                            for payload in waiting.drain(..) {
                                let _ = relay.send_to(&payload, peer);
                            }
                        }
                    }
                }
                TurnEvent::CreatePermissionError(_, _) => {
                    // Leave it out of `requested` so a later datagram retries rather than
                    // queueing against a permission that will never arrive.
                }
                TurnEvent::DataIndicationOrChannelData(_, peer, data) => {
                    if inbound.send((data, peer)).is_err() {
                        return; // The socket is gone; so is the reason to keep the stream.
                    }
                }
                _ => {}
            }
        }
        // Sending may have produced more to write, and a failed permission may have
        // produced an event; both are picked up on the next turn of the loop.
        while let Some(out) = client.poll_write() {
            if stream.write_all(&out.message).await.is_err() {
                return;
            }
        }

        let timeout = client.poll_timeout();
        let sleep = async {
            match timeout {
                Some(at) => {
                    let now = Instant::now();
                    tokio::time::sleep(at.saturating_duration_since(now)).await
                }
                // Nothing scheduled: wake occasionally rather than never, so a client that
                // only starts timers after its first response is still driven.
                None => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        };

        tokio::select! {
            read = stream.read_buf(&mut read_buf) => {
                match read {
                    Ok(0) | Err(_) => {
                        if let Some(tx) = ready.take() {
                            let _ = tx.send(Err("the relay closed the connection".into()));
                        }
                        return;
                    }
                    Ok(_) => loop {
                        match frame_len(&read_buf) {
                            Ok(Some(n)) => {
                                let frame = read_buf.split_to(n);
                                let msg = TaggedBytesMut {
                                    now: Instant::now(),
                                    transport: TransportContext {
                                        local_addr: "0.0.0.0:0".parse().unwrap_or(server),
                                        peer_addr: server,
                                        ecn: None,
                                        transport_protocol: TransportProtocol::TCP,
                                    },
                                    message: frame,
                                };
                                let _ = client.handle_read(msg);
                            }
                            Ok(None) => break,
                            Err(_) => return, // Oversized frame: the stream is not trustworthy.
                        }
                    },
                }
            }
            cmd = commands.recv() => {
                match cmd {
                    None => return, // The socket was dropped.
                    Some(Command::Send(payload, peer)) => {
                        let Some(addr) = relayed else { continue };
                        if permitted.contains(&peer) {
                            if let Ok(mut relay) = client.relay(addr) {
                                let _ = relay.send_to(&payload, peer);
                            }
                        } else {
                            // Hold it until the relay agrees to talk to this peer, and ask
                            // if we have not already.
                            let waiting = queued.entry(peer).or_default();
                            if waiting.len() == MAX_QUEUED_PER_PEER {
                                waiting.pop_front();
                            }
                            waiting.push_back(payload);
                            if requested.insert(peer) {
                                if let Ok(mut relay) = client.relay(addr) {
                                    if relay.create_permission(peer).is_err() {
                                        requested.remove(&peer);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            () = sleep => {
                let _ = client.handle_timeout(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turns_url_is_tls_and_tcp_without_being_told() {
        let s = TurnTcpServer::parse("turns:relay.example.com:443", "u", "p").expect("parsed");
        assert!(s.tls);
        assert_eq!(s.addr, "relay.example.com:443");
    }

    #[test]
    fn the_transport_query_is_honoured() {
        let s = TurnTcpServer::parse("turn:relay.example.com:80?transport=tcp", "u", "p").expect("parsed");
        assert!(!s.tls);
        assert_eq!(s.addr, "relay.example.com:80");
    }

    /// A UDP relay must be refused rather than quietly dialled over TCP: the operator named
    /// a transport, and the ordinary WebRTC path is what serves it.
    #[test]
    fn a_udp_relay_is_refused() {
        assert!(TurnTcpServer::parse("turn:relay.example.com:3478?transport=udp", "u", "p").is_err());
        assert!(TurnTcpServer::parse("turn:relay.example.com:3478", "u", "p").is_err());
    }

    #[test]
    fn a_url_without_a_port_takes_the_default_for_its_scheme() {
        assert_eq!(
            TurnTcpServer::parse("turns:relay.example.com", "u", "p")
                .expect("parsed")
                .addr,
            "relay.example.com:5349"
        );
    }

    #[test]
    fn something_that_is_not_a_turn_url_is_refused() {
        assert!(TurnTcpServer::parse("stun:stun.example.com:19302", "u", "p").is_err());
        assert!(TurnTcpServer::parse("relay.example.com", "u", "p").is_err());
    }

    #[test]
    fn the_host_is_taken_without_the_port_for_tls_naming() {
        let s = TurnTcpServer::parse("turns:relay.example.com:443", "u", "p").expect("parsed");
        assert_eq!(s.host(), "relay.example.com");
    }

    /// STUN carries its length excluding the 20-byte header (RFC 5389 §6).
    #[test]
    fn a_stun_frame_is_its_header_plus_the_declared_length() {
        let mut buf = vec![0x00, 0x01, 0x00, 0x08];
        buf.extend_from_slice(&[0u8; 24]);
        assert_eq!(frame_len(&buf).expect("sized"), Some(28));
    }

    /// ChannelData is padded to a multiple of four on a stream (RFC 5766 §11.5).
    #[test]
    fn channel_data_is_padded_to_four_bytes_on_a_stream() {
        let mut buf = vec![0x40, 0x00, 0x00, 0x05];
        buf.extend_from_slice(&[0u8; 12]);
        assert_eq!(frame_len(&buf).expect("sized"), Some(12));
    }

    #[test]
    fn an_incomplete_frame_reports_nothing_rather_than_guessing() {
        assert_eq!(frame_len(&[0x00, 0x01]).expect("short"), None);
        assert_eq!(frame_len(&[0x00, 0x01, 0x00, 0x08, 0x00]).expect("short"), None);
    }

    /// An absurd length must end the connection rather than reserve the memory it asks for.
    #[test]
    fn an_oversized_frame_is_an_error() {
        assert!(frame_len(&[0x00, 0x01, 0xFF, 0xFF]).is_err());
    }
}
