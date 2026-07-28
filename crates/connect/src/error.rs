//! Error type for the Radix Connect client. `Display` is localized to the system
//! language via `radixdlt-i18n`.

use radixdlt_connect_types::WalletInteractionError;
use radixdlt_i18n::{tr, Lang};

/// Errors raised while pairing with, or talking to, the Radix mobile wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// Signaling-server (WebSocket) transport error.
    Signaling(String),
    /// The signaling channel closed before the WebRTC channel could open.
    SignalingClosed,
    /// WebRTC negotiation/transport error.
    WebRtc(String),
    /// Cryptography error (encrypt/decrypt of the signaling payload).
    Crypto(String),
    /// The peer's message did not match the expected protocol shape.
    Protocol(String),
    /// Timed out waiting for the wallet to appear / open the data channel.
    ChannelTimeout,
    /// Timed out waiting for the wallet's response or approval.
    ResponseTimeout,
    /// Timed out waiting for the peer to confirm receipt of our message.
    ConfirmationTimeout,
    /// The wallet rejected or cancelled the request.
    WalletRejected(String),
    /// Another request is already in flight on the same paired link, and it did not
    /// finish within the timeout. A link carries ONE conversation at a time: requests
    /// that share it are queued, and this reports the queue never cleared in time.
    LinkBusy,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            ConnectError::Signaling(e) => tr!(
                lang,
                format!("could not connect to the signaling server: {e}"),
                format!("no se pudo conectar al servidor de señalización: {e}")
            ),
            ConnectError::SignalingClosed => tr!(
                lang,
                "the signaling channel closed before the WebRTC channel opened".to_string(),
                "la señalización se cerró antes de abrir el canal WebRTC".to_string()
            ),
            ConnectError::WebRtc(e) => tr!(
                lang,
                format!("WebRTC error: {e}"),
                format!("error de WebRTC: {e}")
            ),
            ConnectError::Crypto(e) => tr!(
                lang,
                format!("cryptography error: {e}"),
                format!("error de criptografía: {e}")
            ),
            ConnectError::Protocol(e) => tr!(
                lang,
                format!("protocol error: {e}"),
                format!("error de protocolo: {e}")
            ),
            ConnectError::ChannelTimeout => tr!(
                lang,
                "timed out establishing the WebRTC channel with the wallet".to_string(),
                "se agotó el tiempo estableciendo el canal WebRTC con la wallet".to_string()
            ),
            ConnectError::ResponseTimeout => tr!(
                lang,
                "timed out waiting for the wallet's response".to_string(),
                "se agotó el tiempo esperando la respuesta de la wallet".to_string()
            ),
            ConnectError::ConfirmationTimeout => tr!(
                lang,
                "timed out waiting for the peer to confirm receipt".to_string(),
                "se agotó el tiempo esperando la confirmación del peer".to_string()
            ),
            ConnectError::WalletRejected(e) => tr!(
                lang,
                format!("the wallet rejected/cancelled the request: {e}"),
                format!("la wallet rechazó/canceló la solicitud: {e}")
            ),
            ConnectError::LinkBusy => tr!(
                lang,
                "another request is still in flight on this link (one conversation at a time)".to_string(),
                "ya hay otra petición en curso en este enlace (una conversación a la vez)".to_string()
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for ConnectError {}

impl From<WalletInteractionError> for ConnectError {
    fn from(e: WalletInteractionError) -> Self {
        match e {
            WalletInteractionError::WalletRejected(s) => ConnectError::WalletRejected(s),
            WalletInteractionError::Protocol(s) => ConnectError::Protocol(s),
        }
    }
}
