//! A local Radix Connect signaling relay: the same protocol as `wss://signaling-server.radixdlt.com`,
//! served from this machine.
//!
//! Signaling only introduces the two ends: it carries their encrypted WebRTC offer, answer and ICE
//! candidates, and the conversation itself then flows peer to peer. The public relay is on the
//! internet, so with no internet the phone cannot be reached at all — even when it sits on a cable
//! next to the computer. Run this relay on the computer, add it to the Radix Wallet as a signaling
//! server (Settings › Preferences › Signaling Servers, e.g. `ws://172.20.10.10:8787/`), and the
//! introduction happens over whatever link joins the two: a USB cable (tethering / Personal
//! Hotspot), or a local Wi-Fi with no internet.
//!
//! It sees what the public relay sees and no more: the connection id (a hash of the link
//! password) and payloads encrypted with the link password. It keeps nothing on disk.
//!
//! Behaviour (parity with `radixdlt/signaling-server`): a client connects to
//! `/{connectionId}?source=<wallet|extension>&target=<the other>`; when its counterpart is
//! already there both are told (`remoteClientIsAlreadyConnected` / `remoteClientJustConnected`);
//! a message names a `targetClientId` and is delivered as `remoteData` (and confirmed with
//! `confirmation`), or answered with `missingRemoteClientError`; a client leaving is announced as
//! `remoteClientDisconnected`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

/// The port the relay listens on unless told otherwise.
pub const DEFAULT_RELAY_PORT: u16 = 8787;

type Outbox = mpsc::UnboundedSender<String>;

/// Who is connected: `"{connectionId}:{source}"` → websocket id → its outbox.
#[derive(Default)]
struct Rooms {
    clients: HashMap<String, HashMap<String, Outbox>>,
}

/// One connection, as its URL describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Peer {
    connection_id: String,
    source: String,
    target: String,
}

/// Reads `/{connectionId}?source=…&target=…`, refusing what the public relay refuses.
fn parse_path(path_and_query: &str) -> Result<Peer, &'static str> {
    let (path, query) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
    let connection_id = path.trim_start_matches('/').trim_end_matches('/');
    let connection_id = connection_id.rsplit('/').next().unwrap_or_default();
    if connection_id.len() != 64 || !connection_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("missing connectionId in path");
    }
    let mut source = None;
    let mut target = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("source", v)) => source = Some(v.to_string()),
            Some(("target", v)) => target = Some(v.to_string()),
            _ => {}
        }
    }
    let valid = |v: &Option<String>| matches!(v.as_deref(), Some("wallet" | "extension"));
    if !valid(&source) || !valid(&target) {
        return Err("invalid source or target");
    }
    let (source, target) = (source.unwrap_or_default(), target.unwrap_or_default());
    if source == target {
        return Err("source and target needs to be different");
    }
    Ok(Peer {
        connection_id: connection_id.to_lowercase(),
        source,
        target,
    })
}

/// Serves the relay on `listener` until it fails. Every connection is its own task.
///
/// # Errors
/// Only when accepting connections fails outright.
pub async fn serve_relay(listener: TcpListener) -> std::io::Result<()> {
    let rooms: Arc<Mutex<Rooms>> = Arc::default();
    loop {
        let (stream, _) = listener.accept().await?;
        let rooms = rooms.clone();
        tokio::spawn(async move {
            let _ = handle(stream, rooms).await;
        });
    }
}

/// Binds `addr` (e.g. `0.0.0.0:8787`) and serves the relay there.
///
/// # Errors
/// When the address cannot be bound (in use, no permission) or accepting fails.
pub async fn run_relay(addr: &str) -> std::io::Result<()> {
    serve_relay(TcpListener::bind(addr).await?).await
}

fn lock(rooms: &Mutex<Rooms>) -> std::sync::MutexGuard<'_, Rooms> {
    rooms.lock().unwrap_or_else(|e| e.into_inner())
}

// The handshake callback's error type is tungstenite's own HTTP response, which is large; it is
// never built here (every request is accepted, and refused afterwards with a close frame).
#[allow(clippy::result_large_err)]
async fn handle(stream: TcpStream, rooms: Arc<Mutex<Rooms>>) -> Result<(), ()> {
    let mut asked = String::new();
    let ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, resp: Response| {
        asked = req.uri().to_string();
        Ok(resp)
    })
    .await
    .map_err(|_| ())?;
    let (mut write, mut read) = ws.split();

    let peer = match parse_path(&asked) {
        Ok(peer) => peer,
        Err(why) => {
            let _ = write
                .send(WsMessage::Close(Some(
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: 1003.into(),
                        reason: why.into(),
                    },
                )))
                .await;
            return Err(());
        }
    };
    let id = Uuid::new_v4().to_string();
    let own_key = format!("{}:{}", peer.connection_id, peer.source);
    let target_key = format!("{}:{}", peer.connection_id, peer.target);
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Join, and introduce both ends when the other is already here.
    {
        let mut rooms = lock(&rooms);
        rooms
            .clients
            .entry(own_key.clone())
            .or_default()
            .insert(id.clone(), tx.clone());
        if let Some(targets) = rooms.clients.get(&target_key) {
            for (target_id, target_tx) in targets {
                let _ = tx.send(
                    json!({ "info": "remoteClientIsAlreadyConnected", "remoteClientId": target_id })
                        .to_string(),
                );
                let _ = target_tx
                    .send(json!({ "info": "remoteClientJustConnected", "remoteClientId": id }).to_string());
            }
        }
    }

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if write.send(WsMessage::Text(text)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = read.next().await {
        let text = match message {
            WsMessage::Text(t) => t,
            WsMessage::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            WsMessage::Close(_) => break,
            _ => continue,
        };
        let reply = route(&rooms, &id, &target_key, &text);
        if let Some(reply) = reply {
            let _ = tx.send(reply);
        }
    }

    // Leave, and tell whoever was waiting on the other end.
    {
        let mut rooms = lock(&rooms);
        if let Some(own) = rooms.clients.get_mut(&own_key) {
            own.remove(&id);
            if own.is_empty() {
                rooms.clients.remove(&own_key);
            }
        }
        if let Some(targets) = rooms.clients.get(&target_key) {
            for target_tx in targets.values() {
                let _ = target_tx
                    .send(json!({ "info": "remoteClientDisconnected", "remoteClientId": id }).to_string());
            }
        }
    }
    writer.abort();
    Ok(())
}

/// Delivers one message from `from` to the client it names, and says what to answer `from`.
fn route(rooms: &Mutex<Rooms>, from: &str, target_key: &str, text: &str) -> Option<String> {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return Some(
            json!({ "info": "invalidMessageError", "data": text, "error": "invalid message format, expected JSON" })
                .to_string(),
        );
    };
    let request_id = message.get("requestId").cloned().unwrap_or(Value::Null);
    let Some(target_id) = message.get("targetClientId").and_then(Value::as_str) else {
        return Some(
            json!({ "info": "validationError", "requestId": request_id, "error": "missing targetClientId" })
                .to_string(),
        );
    };
    let rooms = lock(rooms);
    match rooms.clients.get(target_key).and_then(|t| t.get(target_id)) {
        Some(target_tx) => {
            let _ = target_tx.send(
                json!({ "info": "remoteData", "data": message, "remoteClientId": from, "requestId": request_id })
                    .to_string(),
            );
            Some(json!({ "info": "confirmation", "requestId": request_id }).to_string())
        }
        None => Some(json!({ "info": "missingRemoteClientError", "requestId": request_id }).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CID: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

    #[test]
    fn the_path_names_the_room_and_both_ends() {
        let peer = parse_path(&format!("/{CID}?target=wallet&source=extension")).unwrap();
        assert_eq!(peer.connection_id, CID);
        assert_eq!(
            (peer.source.as_str(), peer.target.as_str()),
            ("extension", "wallet")
        );
        // A base URL with a path prefix still ends in the connection id.
        assert!(parse_path(&format!("/relay/{CID}?source=wallet&target=extension")).is_ok());
        assert!(parse_path("/short?source=wallet&target=extension").is_err());
        assert!(parse_path(&format!("/{CID}?source=wallet&target=wallet")).is_err());
        assert!(parse_path(&format!("/{CID}?source=phone&target=extension")).is_err());
    }

    async fn next<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        match ws.next().await {
            Some(Ok(WsMessage::Text(t))) => serde_json::from_str::<Value>(&t).unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// End to end over real sockets: both ends are introduced, a message goes through with its
    /// confirmation, and a leaving end is announced — what the Radix Wallet expects.
    #[tokio::test]
    async fn two_ends_are_introduced_and_relay_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_relay(listener));
        let url = |source: &str, target: &str| {
            format!("ws://127.0.0.1:{port}/{CID}?source={source}&target={target}")
        };

        let (mut wallet, _) = tokio_tungstenite::connect_async(url("wallet", "extension"))
            .await
            .unwrap();
        let (mut ext, _) = tokio_tungstenite::connect_async(url("extension", "wallet"))
            .await
            .unwrap();
        let hello = next(&mut ext).await;
        assert_eq!(hello["info"], "remoteClientIsAlreadyConnected");
        let wallet_id = hello["remoteClientId"].as_str().unwrap().to_string();
        let joined = next(&mut wallet).await;
        assert_eq!(joined["info"], "remoteClientJustConnected");

        let offer = json!({ "requestId": "r1", "targetClientId": wallet_id, "method": "offer",
                            "source": "extension", "connectionId": CID, "encryptedPayload": "00" });
        ext.send(WsMessage::Text(offer.to_string())).await.unwrap();
        assert_eq!(
            next(&mut ext).await,
            json!({ "info": "confirmation", "requestId": "r1" })
        );
        let got = next(&mut wallet).await;
        assert_eq!(got["info"], "remoteData");
        assert_eq!(got["data"]["method"], "offer");
        assert_eq!(got["remoteClientId"], joined["remoteClientId"]);

        let lost = json!({ "requestId": "r2", "targetClientId": "nobody", "method": "offer" });
        ext.send(WsMessage::Text(lost.to_string())).await.unwrap();
        assert_eq!(next(&mut ext).await["info"], "missingRemoteClientError");

        drop(wallet);
        assert_eq!(next(&mut ext).await["info"], "remoteClientDisconnected");
    }
}
