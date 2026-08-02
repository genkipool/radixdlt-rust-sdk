//! WebRTC connector: establishes the data channel with the mobile wallet and
//! carries application messages (WalletInteraction / linkClient).
//!
//! We are always the *initiator* (we create the offer); the wallet is the answerer.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCIceTransportPolicy, RTCPeerConnectionIceEvent,
    RTCSessionDescription,
};
use webrtc::runtime::{Runtime, TokioRuntime};

use crate::chunking::{message_to_chunks, Package, Reassembler};
use crate::error::ConnectError;
use crate::signaling::{SignalEvent, Signaling};
use crate::turn_tcp::{TurnTcpRuntime, TurnTcpServer};

/// An ICE (STUN/TURN) server. A STUN server leaves `username`/`credential` empty.
#[derive(Debug, Clone)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl IceServer {
    /// A STUN server (no credentials).
    pub fn stun(url: impl Into<String>) -> Self {
        IceServer {
            urls: vec![url.into()],
            username: String::new(),
            credential: String::new(),
        }
    }

    /// A TURN server with username/password credentials.
    pub fn turn(url: impl Into<String>, username: impl Into<String>, credential: impl Into<String>) -> Self {
        IceServer {
            urls: vec![url.into()],
            username: username.into(),
            credential: credential.into(),
        }
    }
}

/// The default ICE set used by the public Radix Connect connector: Google STUN
/// plus the public Radix TURN relay (works out of the box; override it to use your
/// own relay).
pub fn radix_default_ice_servers() -> Vec<IceServer> {
    vec![
        IceServer::stun("stun:stun.l.google.com:19302"),
        IceServer::stun("stun:stun1.l.google.com:19302"),
        IceServer::turn(
            "turn:standard.relay.metered.ca:80",
            "51253affa7c2960189ce8cb6",
            "3HWkp3Wgg2cujD2g",
        ),
        IceServer::turn(
            "turns:standard.relay.metered.ca:443?transport=tcp",
            "51253affa7c2960189ce8cb6",
            "3HWkp3Wgg2cujD2g",
        ),
    ]
}

fn to_rtc(servers: &[IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .map(|s| RTCIceServer {
            urls: s.urls.clone(),
            username: s.username.clone(),
            credential: s.credential.clone(),
        })
        .collect()
}

/// Rewrites the address in an SDP `a=candidate` line.
///
/// The line is space-separated and positional (RFC 5245 §15.1): foundation, component,
/// transport, priority, then the connection address at index 4. Anything shorter is not a
/// candidate line and is left exactly as it came.
fn candidate_at(line: &str, addr: std::net::IpAddr) -> String {
    let mut fields: Vec<&str> = line.split(' ').collect();
    let shown = addr.to_string();
    match fields.get_mut(4) {
        Some(slot) => {
            *slot = &shown;
            fields.join(" ")
        }
        None => line.to_string(),
    }
}

/// Forwards locally gathered ICE candidates to the negotiation loop.
///
/// The peer-connection API reports events through this trait rather than through closures,
/// so the sender lives here and the loop owns the matching receiver.
///
/// `advertise_as` is set when a TCP relay carries the traffic. The peer connection builds
/// candidates from the socket it bound, which is this machine, and no setting will talk it
/// out of that: `SettingEngine::set_nat_1to1_ips` exists but nothing reads it. So the
/// address is corrected here, on the way out. This is not a fiction — it names where our
/// traffic genuinely arrives, which is the relay, and it is the only address that could be
/// of any use to the peer.
#[derive(Clone)]
struct IceCandidateForwarder {
    local_candidates: mpsc::UnboundedSender<Value>,
    advertise_as: Option<std::net::IpAddr>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for IceCandidateForwarder {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let candidate = match self.advertise_as {
                Some(addr) => candidate_at(&init.candidate, addr),
                None => init.candidate,
            };
            let _ = self.local_candidates.send(json!({
                "candidate": candidate,
                "sdpMid": init.sdp_mid,
                "sdpMLineIndex": init.sdp_mline_index,
            }));
        }
    }
}

/// Result of establishing the channel: the data channel plus a receiver of incoming
/// application messages already reassembled (JSON Value).
pub struct Channel {
    pub dc: Arc<dyn DataChannel>,
    incoming: mpsc::UnboundedReceiver<Value>,
    confirmations: mpsc::UnboundedReceiver<String>, // messageIds confirmed by the peer
    _pc: Arc<dyn PeerConnection>,
}

/// Gathers ICE candidates through a TCP relay and reports them, talking to no wallet.
///
/// A relay that authenticates but advertises the wrong address fails exactly like a relay
/// that is unreachable — both end as a channel that never opens — so this separates the two.
/// Every candidate returned should carry the relay's own address; anything else means the
/// peer would be told to reach somewhere it cannot.
pub async fn probe_relay_candidates(
    server: &TurnTcpServer,
    wait: Duration,
) -> Result<(std::net::SocketAddr, Vec<String>), ConnectError> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let rt = TurnTcpRuntime::connect(server).await?;
    let relayed = rt.relayed_addr();

    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_handler(Arc::new(IceCandidateForwarder {
            local_candidates: tx,
            advertise_as: Some(relayed.ip()),
        }))
        .with_runtime(rt)
        .with_udp_addrs(vec![format!("0.0.0.0:{}", relayed.port())])
        .build()
        .await
        .map_err(|e| ConnectError::WebRtc(format!("peer connection: {e}")))?;

    // Gathering only starts once there is something to gather for.
    let dc_init = RTCDataChannelInit {
        negotiated: Some(0),
        ordered: true,
        ..Default::default()
    };
    pc.create_data_channel("data", Some(dc_init))
        .await
        .map_err(|e| ConnectError::WebRtc(format!("data channel: {e}")))?;
    let offer = pc
        .create_offer(None)
        .await
        .map_err(|e| ConnectError::WebRtc(format!("create_offer: {e}")))?;
    pc.set_local_description(offer)
        .await
        .map_err(|e| ConnectError::WebRtc(format!("set_local: {e}")))?;

    let mut found = Vec::new();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            cand = rx.recv() => match cand {
                Some(c) => {
                    if let Some(s) = c.get("candidate").and_then(|s| s.as_str()) {
                        found.push(s.to_string());
                    }
                }
                None => break,
            },
        }
    }
    Ok((relayed, found))
}

/// Establishes the WebRTC connection with the wallet using the link password.
/// Resolves once the data channel is open.
///
/// `relay_only` restricts ICE to relay candidates: no host or server-reflexive candidate is
/// gathered, so the only path out is the TURN allocation. Paired with a TURN server reached
/// over TCP it makes the whole connection outbound-TCP, which is what hosts that forbid UDP
/// require.
pub async fn establish(
    ice_servers: &[IceServer],
    signaling_base: &str,
    password: &[u8],
    open_timeout: Duration,
    relay_only: bool,
    turn_tcp: Option<&TurnTcpServer>,
) -> Result<Channel, ConnectError> {
    let mut signaling = Signaling::connect(password, signaling_base).await?;

    // Local ICE candidates → send over signaling. The handler must exist before the peer
    // connection is built, because gathering starts as soon as it does.
    let (local_cand_tx, mut local_cand_rx) = mpsc::unbounded_channel::<Value>();

    // With a TCP relay the allocation sits BELOW the peer connection: it is handed a
    // "socket" that is already the relay. It therefore gets no ICE servers of its own --
    // there is nothing left for it to gather -- and not the relay-only policy, which would
    // discard the one candidate that works.
    //
    // Getting the ADDRESS advertised is the subtle part. The candidates are built from the
    // std socket the builder binds, not from what `wrap_udp_socket` hands back, so left
    // alone it would offer this machine's own address and the wallet would have nowhere to
    // go. `set_nat_1to1_ips` is the documented cure -- it is how a cloud instance advertises
    // its public address instead of its private one -- but it rewrites the IP and keeps the
    // port, so the port has to match: hence binding locally to the very port the relay
    // allocated. That socket is never read or written; only its number matters.
    let (runtime, servers, policy, udp_addr, advertise_as): (
        Arc<dyn Runtime>,
        Vec<RTCIceServer>,
        RTCIceTransportPolicy,
        String,
        Option<std::net::IpAddr>,
    ) = match turn_tcp {
        Some(server) => {
            let rt = TurnTcpRuntime::connect(server).await?;
            let relayed = rt.relayed_addr();
            (
                rt,
                Vec::new(),
                RTCIceTransportPolicy::All,
                format!("0.0.0.0:{}", relayed.port()),
                Some(relayed.ip()),
            )
        }
        None => (
            Arc::new(TokioRuntime),
            to_rtc(ice_servers),
            if relay_only {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::All
            },
            "0.0.0.0:0".to_string(),
            None,
        ),
    };

    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(servers)
        .with_ice_transport_policy(policy)
        .build();

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_handler(Arc::new(IceCandidateForwarder {
            local_candidates: local_cand_tx,
            advertise_as,
        }))
        .with_runtime(runtime)
        .with_udp_addrs(vec![udp_addr])
        .build()
        .await
        .map_err(|e| ConnectError::WebRtc(format!("peer connection: {e}")))?;
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);

    // Negotiated data channel (id 0), same as the browser extension.
    let dc_init = RTCDataChannelInit {
        negotiated: Some(0),
        ordered: true,
        ..Default::default()
    };
    let dc = pc
        .create_data_channel("data", Some(dc_init))
        .await
        .map_err(|e| ConnectError::WebRtc(format!("data channel: {e}")))?;

    // Internal channels back to the caller.
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Value>();
    let (conf_tx, conf_rx) = mpsc::unbounded_channel::<String>();
    let (open_tx, mut open_rx) = mpsc::unbounded_channel::<()>();

    // Data-channel events are pulled rather than pushed, so one task drains them for the
    // life of the channel: it reassembles chunked application messages, confirms each one
    // to the peer, and reports the peer's confirmations of ours. It ends when the channel
    // does, which is what stops `poll` yielding.
    {
        let dc_for_msg = dc.clone();
        let open_tx = open_tx.clone();
        tokio::spawn(async move {
            // Reassembler for incoming application messages.
            let reassembler: Arc<Mutex<Option<Reassembler>>> = Arc::new(Mutex::new(None));
            while let Some(event) = dc_for_msg.poll().await {
                match event {
                    DataChannelEvent::OnOpen => {
                        let _ = open_tx.send(());
                    }
                    DataChannelEvent::OnClose => break,
                    DataChannelEvent::OnMessage(msg) => {
                        let text = String::from_utf8_lossy(&msg.data).to_string();
                        let pkg: Package = match serde_json::from_str(&text) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        match &pkg {
                            Package::MetaData { .. } => {
                                *reassembler.lock().await = Reassembler::from_meta(&pkg);
                            }
                            Package::Chunk { .. } => {
                                let mut guard = reassembler.lock().await;
                                if let Some(re) = guard.as_mut() {
                                    if re.add(&pkg) {
                                        match re.finish() {
                                            Ok(value) => {
                                                // Confirm receipt to the peer.
                                                let conf = json!({
                                                    "packageType": "receiveMessageConfirmation",
                                                    "messageId": re.message_id,
                                                });
                                                let _ = dc_for_msg.send_text(&conf.to_string()).await;
                                                let _ = incoming_tx.send(value);
                                            }
                                            Err(_) => {
                                                let err = json!({
                                                    "packageType": "receiveMessageError",
                                                    "messageId": re.message_id,
                                                    "error": "messageHashesMismatch",
                                                });
                                                let _ = dc_for_msg.send_text(&err.to_string()).await;
                                            }
                                        }
                                        *guard = None;
                                    }
                                }
                            }
                            Package::ReceiveMessageConfirmation { messageId } => {
                                let _ = conf_tx.send(messageId.clone());
                            }
                            Package::ReceiveMessageError { messageId, .. } => {
                                let _ = conf_tx.send(format!("ERROR:{messageId}"));
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // Negotiation loop: consume signaling events and local candidates.
    let pc_neg = pc.clone();
    let deadline = tokio::time::Instant::now() + open_timeout;
    let mut remote_description_set = false;
    let mut pending_remote_candidates: Vec<Value> = Vec::new();

    loop {
        tokio::select! {
            _ = open_rx.recv() => {
                // Channel open: stop forwarding candidates and return.
                drop(local_cand_rx);
                return Ok(Channel { dc, incoming: incoming_rx, confirmations: conf_rx, _pc: pc });
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err(ConnectError::ChannelTimeout);
            }
            Some(cand) = local_cand_rx.recv() => {
                let _ = signaling.send("iceCandidate", cand);
            }
            ev = signaling.events.recv() => {
                match ev {
                    Some(SignalEvent::RemoteClientConnected(id)) => {
                        signaling.set_target(id);
                        // Create and send the offer.
                        let offer = pc_neg.create_offer(None).await
                            .map_err(|e| ConnectError::WebRtc(format!("create_offer: {e}")))?;
                        pc_neg.set_local_description(offer).await
                            .map_err(|e| ConnectError::WebRtc(format!("set_local: {e}")))?;
                        if let Some(local) = pc_neg.local_description().await {
                            let _ = signaling.send("offer", json!({ "sdp": local.sdp }));
                        }
                    }
                    Some(SignalEvent::Answer(payload)) => {
                        if let Some(sdp) = payload.get("sdp").and_then(|s| s.as_str()) {
                            let answer = RTCSessionDescription::answer(sdp.to_string())
                                .map_err(|e| ConnectError::WebRtc(format!("answer sdp: {e}")))?;
                            pc_neg.set_remote_description(answer).await
                                .map_err(|e| ConnectError::WebRtc(format!("set_remote: {e}")))?;
                            remote_description_set = true;
                            for c in pending_remote_candidates.drain(..) {
                                add_remote_candidate(&pc_neg, c).await;
                            }
                        }
                    }
                    Some(SignalEvent::IceCandidate(payload)) => {
                        if remote_description_set {
                            add_remote_candidate(&pc_neg, payload).await;
                        } else {
                            pending_remote_candidates.push(payload);
                        }
                    }
                    Some(SignalEvent::RemoteClientDisconnected) => {
                        // The wallet isn't here yet; keep waiting until the timeout.
                    }
                    Some(SignalEvent::Offer(_)) | Some(SignalEvent::Confirmation(_)) => {}
                    None => return Err(ConnectError::SignalingClosed),
                }
            }
        }
    }
}

async fn add_remote_candidate(pc: &Arc<dyn PeerConnection>, payload: Value) {
    let candidate = payload
        .get("candidate")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let sdp_mid = payload
        .get("sdpMid")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let sdp_mline_index = payload
        .get("sdpMLineIndex")
        .and_then(|c| c.as_u64())
        .map(|n| n as u16);
    let init = RTCIceCandidateInit {
        candidate,
        sdp_mid,
        sdp_mline_index,
        username_fragment: None,
        // Only meaningful for LOCAL srflx/relay candidates; a remote one carries no server.
        url: None,
    };
    let _ = pc.add_ice_candidate(init).await;
}

impl Channel {
    /// Sends an application message (JSON Value), chunked, and waits for the peer's
    /// confirmation.
    pub async fn send_message(
        &mut self,
        message: &Value,
        confirm_timeout: Duration,
    ) -> Result<(), ConnectError> {
        let bytes =
            serde_json::to_vec(message).map_err(|e| ConnectError::Protocol(format!("serialization: {e}")))?;
        let (message_id, packages) = message_to_chunks(&bytes);
        for p in &packages {
            self.dc
                .send_text(p)
                .await
                .map_err(|e| ConnectError::WebRtc(format!("send chunk: {e}")))?;
        }
        // Wait for a receiveMessageConfirmation matching our messageId.
        timeout(confirm_timeout, async {
            while let Some(id) = self.confirmations.recv().await {
                if id == message_id {
                    return Ok(());
                }
                if id == format!("ERROR:{message_id}") {
                    return Err(ConnectError::Protocol("peer reported a hash error".into()));
                }
            }
            Err(ConnectError::SignalingClosed)
        })
        .await
        .map_err(|_| ConnectError::ConfirmationTimeout)?
    }

    /// Waits for the next incoming application message (reassembled).
    pub async fn recv_message(&mut self, wait: Duration) -> Result<Value, ConnectError> {
        timeout(wait, self.incoming.recv())
            .await
            .map_err(|_| ConnectError::ResponseTimeout)?
            .ok_or(ConnectError::SignalingClosed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address sits at index 4 and nothing else may move: the peer parses by position.
    #[test]
    fn the_candidate_address_is_replaced_in_place() {
        let line = "candidate:291967603 1 udp 2130706431 0.0.0.0 43378 typ host";
        let out = candidate_at(line, "37.27.44.221".parse().expect("ip"));
        assert_eq!(
            out,
            "candidate:291967603 1 udp 2130706431 37.27.44.221 43378 typ host"
        );
    }

    /// Trailing attributes (raddr, rport, generation…) must survive untouched.
    #[test]
    fn everything_after_the_address_is_preserved() {
        let line = "candidate:1 1 udp 41885439 10.0.0.1 5000 typ relay raddr 0.0.0.0 rport 0";
        let out = candidate_at(line, "203.0.113.7".parse().expect("ip"));
        assert!(out.ends_with("typ relay raddr 0.0.0.0 rport 0"));
        assert!(out.contains(" 203.0.113.7 5000 "));
    }

    /// Something too short to be a candidate line is not mangled into one.
    #[test]
    fn a_line_that_is_not_a_candidate_is_left_alone() {
        assert_eq!(
            candidate_at("nonsense", "1.2.3.4".parse().expect("ip")),
            "nonsense"
        );
    }
}
