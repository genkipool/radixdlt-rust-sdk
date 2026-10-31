//! The transports a Radix Connect link can travel over.
//!
//! A link is one password shared by a connector and a wallet; the same link can be reached over
//! WebRTC (a signaling server, then a peer-to-peer data channel) or over iroh (QUIC, through a
//! relay or directly). Which one a link was paired over is recorded with it, so that a connector
//! knows whether the wallet speaks iroh at all.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Radix Connect transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// WebRTC: every Radix Wallet speaks it.
    Webrtc,
    /// iroh (QUIC): wallets with Radix Connect over iroh on.
    Iroh,
}

impl Transport {
    /// Its name, as written in files and logs: `webrtc` or `iroh`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Transport::Webrtc => "webrtc",
            Transport::Iroh => "iroh",
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_written_by_its_name() {
        assert_eq!(serde_json::to_string(&Transport::Iroh).unwrap(), r#""iroh""#);
        assert_eq!(
            serde_json::from_str::<Transport>(r#""webrtc""#).unwrap(),
            Transport::Webrtc
        );
        assert_eq!(Transport::Iroh.to_string(), "iroh");
    }
}
