//! Radix Connect signaling client (WebSocket).
//!
//! Connects to `wss://…/{connectionId}?target=wallet&source=extension`, carries the
//! WebRTC negotiation (offer/answer/iceCandidate) encrypted with AES-256-GCM and
//! reports peer (wallet) presence. Parity with the JS `SignalingClient`.

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

use crate::crypto::{decrypt_payload, encrypt_payload};
use crate::error::ConnectError;

/// Default public Radix signaling server.
pub const SIGNALING_BASE: &str = "wss://signaling-server.radixdlt.com";

/// Message we SEND to the signaling server.
#[derive(Serialize)]
#[allow(non_snake_case)]
struct OutgoingMessage {
    requestId: String,
    connectionId: String,
    targetClientId: String,
    method: String, // offer | answer | iceCandidate
    source: String, // extension
    encryptedPayload: String,
}

/// Events the signaling client delivers to the WebRTC layer.
// Some variants carry protocol payloads we intentionally do not act on (we are the
// offerer, so an incoming Offer/Confirmation is ignored).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum SignalEvent {
    RemoteClientConnected(String), // remoteClientId
    RemoteClientDisconnected,
    Offer(Value),
    Answer(Value),
    IceCandidate(Value),
    Confirmation(String), // requestId
}

/// Manages the WSS connection. `events` emits SignalEvent; `out` receives messages
/// to send.
pub struct Signaling {
    pub events: mpsc::UnboundedReceiver<SignalEvent>,
    out: mpsc::UnboundedSender<OutgoingRequest>,
    target_client_id: Option<String>,
}

struct OutgoingRequest {
    method: String,
    payload: Value,
    target_client_id: String,
}

impl Signaling {
    /// Connects to the signaling server as the initiator (extension→wallet).
    pub async fn connect(password: &[u8], base: &str) -> Result<Self, ConnectError> {
        let connection_id = crate::crypto::connection_id_hex(password);
        let url = format!("{base}/{connection_id}?target=wallet&source=extension");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| ConnectError::Signaling(e.to_string()))?;
        let (mut write, mut read) = ws_stream.split();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutgoingRequest>();

        let key = password.to_vec();
        let cid = connection_id.clone();

        // Send task.
        let key_send = key.clone();
        tokio::spawn(async move {
            while let Some(req) = out_rx.recv().await {
                let payload_bytes = serde_json::to_vec(&req.payload).unwrap_or_default();
                let enc = match encrypt_payload(&payload_bytes, &key_send) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let msg = OutgoingMessage {
                    requestId: Uuid::new_v4().to_string(),
                    connectionId: cid.clone(),
                    targetClientId: req.target_client_id,
                    method: req.method,
                    source: "extension".into(),
                    encryptedPayload: enc,
                };
                let txt = serde_json::to_string(&msg).unwrap();
                if write.send(WsMessage::Text(txt)).await.is_err() {
                    break;
                }
            }
        });

        // Receive task.
        let key_recv = key.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                let text = match msg {
                    WsMessage::Text(t) => t,
                    WsMessage::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    WsMessage::Close(_) => break,
                    _ => continue,
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(ev) = parse_incoming(&v, &key_recv) {
                    if event_tx.send(ev).is_err() {
                        break;
                    }
                }
            }
            let _ = event_tx.send(SignalEvent::RemoteClientDisconnected);
        });

        Ok(Signaling {
            events: event_rx,
            out: out_tx,
            target_client_id: None,
        })
    }

    pub fn set_target(&mut self, id: String) {
        self.target_client_id = Some(id);
    }

    pub fn send(&self, method: &str, payload: Value) -> Result<(), ConnectError> {
        let target = self.target_client_id.clone().ok_or_else(|| {
            ConnectError::Protocol("no targetClientId yet (peer not announced)".into())
        })?;
        self.out
            .send(OutgoingRequest {
                method: method.into(),
                payload,
                target_client_id: target,
            })
            .map_err(|_| ConnectError::SignalingClosed)
    }
}

/// Turns a raw server message into a SignalEvent (decrypting where applicable).
fn parse_incoming(v: &Value, key: &[u8]) -> Option<SignalEvent> {
    let info = v.get("info").and_then(|x| x.as_str()).unwrap_or("");
    match info {
        "remoteClientJustConnected" | "remoteClientIsAlreadyConnected" => {
            let id = v.get("remoteClientId")?.as_str()?.to_string();
            Some(SignalEvent::RemoteClientConnected(id))
        }
        "remoteClientDisconnected" => Some(SignalEvent::RemoteClientDisconnected),
        "confirmation" => {
            let rid = v.get("requestId")?.as_str()?.to_string();
            Some(SignalEvent::Confirmation(rid))
        }
        "remoteData" => {
            let data = v.get("data")?;
            let method = data.get("method")?.as_str()?;
            let enc = data.get("encryptedPayload")?.as_str()?;
            let decrypted = decrypt_payload(enc, key).ok()?;
            let payload: Value = serde_json::from_slice(&decrypted).ok()?;
            match method {
                "offer" => Some(SignalEvent::Offer(payload)),
                "answer" => Some(SignalEvent::Answer(payload)),
                "iceCandidate" => Some(SignalEvent::IceCandidate(payload)),
                _ => None,
            }
        }
        _ => None,
    }
}
