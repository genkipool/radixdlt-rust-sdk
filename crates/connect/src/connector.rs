//! WebRTC connector: establishes the data channel with the mobile wallet and
//! carries application messages (WalletInteraction / linkClient).
//!
//! We are always the *initiator* (we create the offer); the wallet is the answerer.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::chunking::{message_to_chunks, Package, Reassembler};
use crate::error::ConnectError;
use crate::signaling::{SignalEvent, Signaling};

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
    pub fn turn(
        url: impl Into<String>,
        username: impl Into<String>,
        credential: impl Into<String>,
    ) -> Self {
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
        .map(|s| {
            if s.username.is_empty() {
                RTCIceServer {
                    urls: s.urls.clone(),
                    ..Default::default()
                }
            } else {
                RTCIceServer {
                    urls: s.urls.clone(),
                    username: s.username.clone(),
                    credential: s.credential.clone(),
                    credential_type: RTCIceCredentialType::Password,
                }
            }
        })
        .collect()
}

/// Result of establishing the channel: the data channel plus a receiver of incoming
/// application messages already reassembled (JSON Value).
pub struct Channel {
    pub dc: Arc<RTCDataChannel>,
    incoming: mpsc::UnboundedReceiver<Value>,
    confirmations: mpsc::UnboundedReceiver<String>, // messageIds confirmed by the peer
    _pc: Arc<RTCPeerConnection>,
}

/// Establishes the WebRTC connection with the wallet using the link password.
/// Resolves once the data channel is open.
pub async fn establish(
    ice_servers: &[IceServer],
    signaling_base: &str,
    password: &[u8],
    open_timeout: Duration,
) -> Result<Channel, ConnectError> {
    let mut signaling = Signaling::connect(password, signaling_base).await?;

    let api = APIBuilder::new().build();
    let config = RTCConfiguration {
        ice_servers: to_rtc(ice_servers),
        ..Default::default()
    };
    let pc = Arc::new(
        api.new_peer_connection(config)
            .await
            .map_err(|e| ConnectError::WebRtc(format!("peer connection: {e}")))?,
    );

    // Negotiated data channel (id 0), same as the browser extension.
    let dc_init = RTCDataChannelInit {
        negotiated: Some(0),
        ordered: Some(true),
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

    // Reassembler for incoming application messages.
    let reassembler: Arc<Mutex<Option<Reassembler>>> = Arc::new(Mutex::new(None));

    {
        let open_tx = open_tx.clone();
        dc.on_open(Box::new(move || {
            let _ = open_tx.send(());
            Box::pin(async {})
        }));
    }

    {
        let dc_for_msg = dc.clone();
        let reassembler = reassembler.clone();
        let incoming_tx = incoming_tx.clone();
        let conf_tx = conf_tx.clone();
        dc.on_message(Box::new(move |msg| {
            let text = String::from_utf8_lossy(&msg.data).to_string();
            let dc_for_msg = dc_for_msg.clone();
            let reassembler = reassembler.clone();
            let incoming_tx = incoming_tx.clone();
            let conf_tx = conf_tx.clone();
            Box::pin(async move {
                let pkg: Package = match serde_json::from_str(&text) {
                    Ok(p) => p,
                    Err(_) => return,
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
                                        let _ = dc_for_msg.send_text(conf.to_string()).await;
                                        let _ = incoming_tx.send(value);
                                    }
                                    Err(_) => {
                                        let err = json!({
                                            "packageType": "receiveMessageError",
                                            "messageId": re.message_id,
                                            "error": "messageHashesMismatch",
                                        });
                                        let _ = dc_for_msg.send_text(err.to_string()).await;
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
            })
        }));
    }

    // Local ICE candidates → send over signaling.
    let (local_cand_tx, mut local_cand_rx) = mpsc::unbounded_channel::<Value>();
    pc.on_ice_candidate(Box::new(move |cand| {
        let local_cand_tx = local_cand_tx.clone();
        Box::pin(async move {
            if let Some(c) = cand {
                if let Ok(init) = c.to_json() {
                    let payload = json!({
                        "candidate": init.candidate,
                        "sdpMid": init.sdp_mid,
                        "sdpMLineIndex": init.sdp_mline_index,
                    });
                    let _ = local_cand_tx.send(payload);
                }
            }
        })
    }));

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

async fn add_remote_candidate(pc: &Arc<RTCPeerConnection>, payload: Value) {
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
        let bytes = serde_json::to_vec(message)
            .map_err(|e| ConnectError::Protocol(format!("serialization: {e}")))?;
        let (message_id, packages) = message_to_chunks(&bytes);
        for p in &packages {
            self.dc
                .send_text(p.clone())
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
