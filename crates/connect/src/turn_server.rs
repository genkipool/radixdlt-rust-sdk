//! A minimal TURN server over TCP (RFC 8656), for reaching a phone that has no network at all.
//!
//! The Radix Wallet on Android with Wi-Fi and mobile data off still runs WebRTC, but the only
//! network it sees is its own loopback: it offers `127.0.0.1` candidates and nothing else. With
//! `adb reverse tcp:3478 tcp:3478` the phone's `127.0.0.1:3478` IS this server, over the USB cable,
//! so a TURN allocation made there gives the phone an address the computer can reach — the relayed
//! address, a UDP socket on this machine. The computer's own WebRTC sends to it directly.
//!
//! Scope, on purpose: TCP control connections only (that is what crosses `adb reverse`), UDP
//! relays only, long-term credentials, and NO per-peer permissions — the server is meant to listen
//! on loopback for one person's own phone, and every relayed address is a loopback socket. It also
//! answers plain STUN Binding requests, so the same address works as the wallet's STUN server.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rtc::stun::attributes::{ATTR_NONCE, ATTR_REALM, ATTR_USERNAME};
use rtc::stun::error_code::{
    ErrorCode, ErrorCodeAttribute, CODE_ALLOC_MISMATCH, CODE_BAD_REQUEST, CODE_UNAUTHORIZED,
    CODE_UNSUPPORTED_TRANS_PROTO,
};
use rtc::stun::fingerprint::FINGERPRINT;
use rtc::stun::integrity::MessageIntegrity;
use rtc::stun::message::{
    Getter, Message, MessageType, Setter, TransactionId, CLASS_ERROR_RESPONSE, CLASS_INDICATION,
    CLASS_REQUEST, CLASS_SUCCESS_RESPONSE, METHOD_ALLOCATE, METHOD_BINDING, METHOD_CHANNEL_BIND,
    METHOD_CREATE_PERMISSION, METHOD_DATA, METHOD_REFRESH, METHOD_SEND,
};
use rtc::stun::textattrs::TextAttribute;
use rtc::stun::xoraddr::XorMappedAddress;
use rtc::turn::proto::channum::ChannelNumber;
use rtc::turn::proto::data::Data;
use rtc::turn::proto::lifetime::Lifetime;
use rtc::turn::proto::peeraddr::PeerAddress;
use rtc::turn::proto::relayaddr::RelayedAddress;
use rtc::turn::proto::reqtrans::RequestedTransport;
use rtc::turn::proto::{PROTO_TCP, PROTO_UDP};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

/// The port the TURN server listens on unless told otherwise (the standard STUN/TURN port).
pub const DEFAULT_TURN_PORT: u16 = 3478;
/// How long an allocation lives without a refresh.
const LIFETIME: Duration = Duration::from_secs(600);
/// Frames larger than this are refused (a TURN frame carries one datagram).
const MAX_FRAME: usize = 64 * 1024;

/// Who may allocate, and where relayed sockets are opened.
#[derive(Clone, Debug)]
pub struct TurnConfig {
    /// The realm sent in challenges.
    pub realm: String,
    /// The one username accepted.
    pub username: String,
    /// Its password.
    pub password: String,
    /// The address relayed sockets bind to (loopback for the cable setup).
    pub relay_ip: IpAddr,
}

impl TurnConfig {
    /// The cable setup: user `radix`, the given password, relays on 127.0.0.1.
    #[must_use]
    pub fn loopback(password: &str) -> Self {
        TurnConfig {
            realm: "radix-connect".into(),
            username: "radix".into(),
            password: password.into(),
            relay_ip: IpAddr::from([127, 0, 0, 1]),
        }
    }
}

/// Serves TURN over TCP on `listener` until accepting fails.
///
/// # Errors
/// Only when accepting connections fails outright.
pub async fn serve_turn(listener: TcpListener, config: TurnConfig) -> std::io::Result<()> {
    let config = Arc::new(config);
    loop {
        let (stream, client) = listener.accept().await?;
        let config = config.clone();
        tokio::spawn(async move {
            let _ = Session::run(stream, client, config).await;
        });
    }
}

/// Splits one complete frame off the front of `buf`: a STUN message or a ChannelData message
/// (padded to four bytes on a stream). `None` while it is incomplete.
fn take_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, &'static str> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let declared = u16::from_be_bytes([buf[0], buf[1]]);
    let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
    let total = if (0x4000..=0x7FFF).contains(&declared) {
        (4 + len).div_ceil(4) * 4
    } else {
        20 + len
    };
    if total > MAX_FRAME {
        return Err("frame too large");
    }
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some(buf.drain(..total).collect()))
}

/// A ChannelData message for a stream: number, length, data, padding to four bytes.
fn channel_data(number: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len() + 3);
    out.extend_from_slice(&number.to_be_bytes());
    out.extend_from_slice(&u16::try_from(data.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

struct Allocation {
    socket: Arc<UdpSocket>,
    channels: HashMap<u16, SocketAddr>,
    reader: tokio::task::JoinHandle<()>,
}

struct Session {
    client: SocketAddr,
    config: Arc<TurnConfig>,
    nonce: String,
    out: mpsc::UnboundedSender<Vec<u8>>,
    allocation: Option<Allocation>,
    /// Peer → channel, shared with the relay reader so it can frame what comes back.
    peers: Arc<std::sync::Mutex<HashMap<SocketAddr, u16>>>,
}

impl Session {
    async fn run(stream: TcpStream, client: SocketAddr, config: Arc<TurnConfig>) -> Result<(), String> {
        let (mut read, mut write) = stream.into_split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if write.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        let mut session = Session {
            client,
            config,
            nonce: hex::encode(nonce),
            out,
            allocation: None,
            peers: Arc::default(),
        };

        let mut buf = Vec::with_capacity(8 * 1024);
        let mut chunk = vec![0u8; 16 * 1024];
        'conn: loop {
            let n = read.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            loop {
                match take_frame(&mut buf) {
                    Ok(Some(frame)) => session.on_frame(&frame).await,
                    Ok(None) => break,
                    Err(_) => break 'conn,
                }
            }
        }
        if let Some(allocation) = session.allocation.take() {
            allocation.reader.abort();
        }
        writer.abort();
        Ok(())
    }

    async fn on_frame(&mut self, frame: &[u8]) {
        let first = u16::from_be_bytes([frame[0], frame[1]]);
        if (0x4000..=0x7FFF).contains(&first) {
            let len = usize::from(u16::from_be_bytes([frame[2], frame[3]]));
            let peer = self
                .allocation
                .as_ref()
                .and_then(|a| a.channels.get(&first).copied());
            if let (Some(peer), Some(allocation), Some(data)) =
                (peer, self.allocation.as_ref(), frame.get(4..4 + len))
            {
                let _ = allocation.socket.send_to(data, peer).await;
            }
            return;
        }
        let mut m = Message::new();
        m.raw = frame.to_vec();
        if m.decode().is_err() {
            return;
        }
        let (method, class) = (m.typ.method, m.typ.class);
        if class == CLASS_INDICATION && method == METHOD_SEND {
            self.on_send(&m).await;
            return;
        }
        if class != CLASS_REQUEST {
            return;
        }
        if method == METHOD_BINDING {
            let mapped = XorMappedAddress {
                ip: self.client.ip(),
                port: self.client.port(),
            };
            self.reply(&m, CLASS_SUCCESS_RESPONSE, vec![Box::new(mapped)], None);
            return;
        }
        let Some(integrity) = self.authenticate(&m) else {
            return;
        };
        match method {
            METHOD_ALLOCATE => self.on_allocate(&m, integrity).await,
            METHOD_REFRESH => self.on_refresh(&m, integrity),
            METHOD_CREATE_PERMISSION => {
                self.reply(&m, CLASS_SUCCESS_RESPONSE, vec![], Some(integrity));
            }
            METHOD_CHANNEL_BIND => self.on_channel_bind(&m, integrity),
            _ => self.error(&m, CODE_BAD_REQUEST, Some(integrity)),
        }
    }

    /// Long-term credentials: challenge a request without them, refuse a wrong one, and return
    /// the integrity to sign the response with.
    fn authenticate(&self, m: &Message) -> Option<MessageIntegrity> {
        let username = TextAttribute::get_from_as(m, ATTR_USERNAME).ok();
        let Some(username) = username.filter(|_| m.contains(rtc::stun::attributes::ATTR_MESSAGE_INTEGRITY))
        else {
            self.challenge(m);
            return None;
        };
        let nonce = TextAttribute::get_from_as(m, ATTR_NONCE)
            .map(|n| n.text)
            .unwrap_or_default();
        if username.text != self.config.username || nonce != self.nonce {
            self.challenge(m);
            return None;
        }
        let integrity = MessageIntegrity::new_long_term_integrity(
            self.config.username.clone(),
            self.config.realm.clone(),
            self.config.password.clone(),
        );
        let mut checked = m.clone();
        if integrity.check(&mut checked).is_err() {
            self.challenge(m);
            return None;
        }
        Some(integrity)
    }

    fn challenge(&self, m: &Message) {
        let attrs: Vec<Box<dyn Setter>> = vec![
            Box::new(ErrorCodeAttribute {
                code: CODE_UNAUTHORIZED,
                reason: b"Unauthorized".to_vec(),
            }),
            Box::new(TextAttribute::new(ATTR_REALM, self.config.realm.clone())),
            Box::new(TextAttribute::new(ATTR_NONCE, self.nonce.clone())),
        ];
        self.reply(m, CLASS_ERROR_RESPONSE, attrs, None);
    }

    fn error(&self, m: &Message, code: ErrorCode, integrity: Option<MessageIntegrity>) {
        let attrs: Vec<Box<dyn Setter>> = vec![Box::new(ErrorCodeAttribute {
            code,
            reason: b"Error".to_vec(),
        })];
        self.reply(m, CLASS_ERROR_RESPONSE, attrs, integrity);
    }

    fn reply(
        &self,
        m: &Message,
        class: rtc::stun::message::MessageClass,
        mut attrs: Vec<Box<dyn Setter>>,
        integrity: Option<MessageIntegrity>,
    ) {
        let mut setters: Vec<Box<dyn Setter>> = vec![
            Box::new(m.transaction_id),
            Box::new(MessageType::new(m.typ.method, class)),
        ];
        setters.append(&mut attrs);
        if let Some(integrity) = integrity {
            setters.push(Box::new(integrity));
        }
        setters.push(Box::new(FINGERPRINT));
        let mut out = Message::new();
        if out.build(&setters).is_ok() {
            let _ = self.out.send(out.raw);
        }
    }

    async fn on_allocate(&mut self, m: &Message, integrity: MessageIntegrity) {
        if self.allocation.is_some() {
            self.error(m, CODE_ALLOC_MISMATCH, Some(integrity));
            return;
        }
        // The relay is always UDP. libwebrtc asks for UDP; this crate's own TURN client (rtc-turn)
        // asks for TCP whenever its control connection is TCP, and public relays answer it with a
        // UDP relay anyway — so does this one, rather than refuse the client it ships with.
        let mut transport = RequestedTransport::default();
        if transport.get_from(m).is_err() || ![PROTO_UDP, PROTO_TCP].contains(&transport.protocol) {
            self.error(m, CODE_UNSUPPORTED_TRANS_PROTO, Some(integrity));
            return;
        }
        let socket = match UdpSocket::bind(SocketAddr::new(self.config.relay_ip, 0)).await {
            Ok(s) => Arc::new(s),
            Err(_) => {
                self.error(m, ErrorCode(508), Some(integrity));
                return;
            }
        };
        let Ok(relayed) = socket.local_addr() else {
            return;
        };
        let reader = {
            let (socket, out, peers) = (socket.clone(), self.out.clone(), self.peers.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; MAX_FRAME];
                while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                    let channel = peers.lock().ok().and_then(|p| p.get(&from).copied());
                    let frame = match channel {
                        Some(number) => channel_data(number, &buf[..n]),
                        None => {
                            let mut msg = Message::new();
                            let setters: Vec<Box<dyn Setter>> = vec![
                                Box::new(TransactionId::new()),
                                Box::new(MessageType::new(METHOD_DATA, CLASS_INDICATION)),
                                Box::new(PeerAddress {
                                    ip: from.ip(),
                                    port: from.port(),
                                }),
                                Box::new(Data(buf[..n].to_vec())),
                            ];
                            if msg.build(&setters).is_err() {
                                continue;
                            }
                            msg.raw
                        }
                    };
                    if out.send(frame).is_err() {
                        break;
                    }
                }
            })
        };
        self.allocation = Some(Allocation {
            socket,
            channels: HashMap::new(),
            reader,
        });
        let attrs: Vec<Box<dyn Setter>> = vec![
            Box::new(RelayedAddress {
                ip: relayed.ip(),
                port: relayed.port(),
            }),
            Box::new(Lifetime(LIFETIME)),
            Box::new(XorMappedAddress {
                ip: self.client.ip(),
                port: self.client.port(),
            }),
        ];
        self.reply(m, CLASS_SUCCESS_RESPONSE, attrs, Some(integrity));
    }

    fn on_refresh(&mut self, m: &Message, integrity: MessageIntegrity) {
        let mut lifetime = Lifetime(LIFETIME);
        let _ = lifetime.get_from(m);
        if lifetime.0.is_zero() {
            if let Some(allocation) = self.allocation.take() {
                allocation.reader.abort();
            }
        }
        let granted = if lifetime.0.is_zero() {
            Duration::ZERO
        } else {
            LIFETIME
        };
        self.reply(
            m,
            CLASS_SUCCESS_RESPONSE,
            vec![Box::new(Lifetime(granted))],
            Some(integrity),
        );
    }

    fn on_channel_bind(&mut self, m: &Message, integrity: MessageIntegrity) {
        let mut number = ChannelNumber::default();
        let mut peer = PeerAddress::default();
        let valid = number.get_from(m).is_ok() && peer.get_from(m).is_ok();
        let Some(allocation) = self.allocation.as_mut().filter(|_| valid) else {
            self.error(m, CODE_BAD_REQUEST, Some(integrity));
            return;
        };
        let peer = SocketAddr::new(peer.ip, peer.port);
        allocation.channels.insert(number.0, peer);
        if let Ok(mut peers) = self.peers.lock() {
            peers.insert(peer, number.0);
        }
        self.reply(m, CLASS_SUCCESS_RESPONSE, vec![], Some(integrity));
    }

    async fn on_send(&self, m: &Message) {
        let mut peer = PeerAddress::default();
        let mut data = Data::default();
        if peer.get_from(m).is_err() || data.get_from(m).is_err() {
            return;
        }
        if let Some(allocation) = &self.allocation {
            let _ = allocation
                .socket
                .send_to(&data.0, SocketAddr::new(peer.ip, peer.port))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_split_by_their_own_lengths() {
        // A STUN header declaring 4 bytes of attributes, then the start of the next frame.
        let mut buf = vec![0x00, 0x01, 0x00, 0x04];
        buf.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        buf.extend_from_slice(&[0x40, 0x00]);
        let frame = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(frame.len(), 24);
        assert_eq!(buf, vec![0x40, 0x00], "the rest stays for the next frame");
        assert_eq!(take_frame(&mut buf).unwrap(), None);

        // ChannelData of 5 bytes is padded to 4 + 8 on a stream.
        let mut cd = channel_data(0x4001, b"hello");
        assert_eq!(cd.len(), 12);
        cd.extend_from_slice(&[9]);
        assert_eq!(take_frame(&mut cd).unwrap().unwrap().len(), 12);
        assert_eq!(cd, vec![9]);
    }

    /// The library's own TURN-over-TCP client allocates on this server, and data relayed to the
    /// allocation reaches a plain UDP peer — the round trip the cable setup depends on.
    #[tokio::test]
    async fn the_librarys_turn_client_allocates_and_relays_through_it() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_turn(listener, TurnConfig::loopback("secret")));

        let server = crate::turn_tcp::TurnTcpServer::parse(
            &format!("turn:127.0.0.1:{port}?transport=tcp"),
            "radix",
            "secret",
        )
        .unwrap();
        let runtime = crate::turn_tcp::TurnTcpRuntime::connect(&server).await.unwrap();
        let relayed = runtime.relayed_addr();
        assert!(relayed.ip().is_loopback(), "{relayed}");

        // A wrong password is refused.
        let wrong = crate::turn_tcp::TurnTcpServer::parse(
            &format!("turn:127.0.0.1:{port}?transport=tcp"),
            "radix",
            "nope",
        )
        .unwrap();
        assert!(crate::turn_tcp::TurnTcpRuntime::connect(&wrong).await.is_err());
    }
}
