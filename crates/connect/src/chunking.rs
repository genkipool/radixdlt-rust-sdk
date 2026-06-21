//! Message chunking over the data channel (parity with `radix-connect-webrtc`).
//!
//! An application message is sent as one `metaData` package plus N `chunk`
//! packages. The receiver reassembles them, validates the blake2b hash and replies
//! with a `receiveMessageConfirmation`.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::crypto::blake2b_256;
use crate::error::ConnectError;

pub const CHUNK_SIZE: usize = 11_500;

// Field names mirror the on-the-wire Radix Connect JSON (camelCase) exactly.
#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "packageType")]
pub enum Package {
    #[serde(rename = "metaData")]
    MetaData {
        chunkCount: usize,
        messageByteCount: usize,
        hashOfMessage: String,
        messageId: String,
    },
    #[serde(rename = "chunk")]
    Chunk {
        chunkIndex: usize,
        chunkData: String,
        messageId: String,
    },
    #[serde(rename = "receiveMessageConfirmation")]
    ReceiveMessageConfirmation { messageId: String },
    #[serde(rename = "receiveMessageError")]
    ReceiveMessageError {
        messageId: String,
        #[serde(default)]
        error: String,
    },
}

/// Splits a message (JSON bytes) into metaData + chunks ready to send as JSON lines
/// over the data channel. Also returns the messageId.
pub fn message_to_chunks(message: &[u8]) -> (String, Vec<String>) {
    let message_id = Uuid::new_v4().to_string();
    let hash = hex::encode(blake2b_256(message));

    let mut chunk_jsons = Vec::new();
    let chunks: Vec<&[u8]> = message.chunks(CHUNK_SIZE).collect();
    let chunk_count = chunks.len();

    let meta = Package::MetaData {
        chunkCount: chunk_count,
        messageByteCount: message.len(),
        hashOfMessage: hash,
        messageId: message_id.clone(),
    };
    let mut out = vec![serde_json::to_string(&meta).unwrap()];

    for (i, c) in chunks.into_iter().enumerate() {
        let chunk = Package::Chunk {
            chunkIndex: i,
            chunkData: B64.encode(c),
            messageId: message_id.clone(),
        };
        chunk_jsons.push(serde_json::to_string(&chunk).unwrap());
    }
    out.extend(chunk_jsons);
    (message_id, out)
}

/// Reassembler for an incoming chunked message.
pub struct Reassembler {
    pub message_id: String,
    chunk_count: usize,
    hash: String,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
}

impl Reassembler {
    pub fn from_meta(meta: &Package) -> Option<Self> {
        if let Package::MetaData {
            chunkCount,
            hashOfMessage,
            messageId,
            ..
        } = meta
        {
            Some(Self {
                message_id: messageId.clone(),
                chunk_count: *chunkCount,
                hash: hashOfMessage.clone(),
                chunks: vec![None; *chunkCount],
                received: 0,
            })
        } else {
            None
        }
    }

    /// Adds a chunk; returns true once every chunk has arrived.
    pub fn add(&mut self, pkg: &Package) -> bool {
        if let Package::Chunk {
            chunkIndex,
            chunkData,
            messageId,
        } = pkg
        {
            if messageId != &self.message_id || *chunkIndex >= self.chunk_count {
                return self.received == self.chunk_count;
            }
            if self.chunks[*chunkIndex].is_none() {
                if let Ok(bytes) = B64.decode(chunkData) {
                    self.chunks[*chunkIndex] = Some(bytes);
                    self.received += 1;
                }
            }
        }
        self.received == self.chunk_count
    }

    /// Reassembles, validates the hash and parses the JSON.
    pub fn finish(&self) -> Result<Value, ConnectError> {
        if self.received != self.chunk_count {
            return Err(ConnectError::Protocol("missing chunks".into()));
        }
        let mut msg = Vec::new();
        for c in &self.chunks {
            msg.extend_from_slice(
                c.as_ref()
                    .ok_or_else(|| ConnectError::Protocol("absent chunk".into()))?,
            );
        }
        let got = hex::encode(blake2b_256(&msg));
        if got != self.hash {
            return Err(ConnectError::Protocol(format!(
                "hash mismatch (expected {}, got {got})",
                self.hash
            )));
        }
        serde_json::from_slice(&msg)
            .map_err(|e| ConnectError::Protocol(format!("invalid JSON: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_chunking() {
        let original = serde_json::json!({ "hello": "world", "n": 42, "arr": [1,2,3] });
        let bytes = serde_json::to_vec(&original).unwrap();
        let (mid, packages) = message_to_chunks(&bytes);

        let meta: Package = serde_json::from_str(&packages[0]).unwrap();
        let mut re = Reassembler::from_meta(&meta).unwrap();
        assert_eq!(re.message_id, mid);
        let mut done = false;
        for p in &packages[1..] {
            let pkg: Package = serde_json::from_str(p).unwrap();
            done = re.add(&pkg);
        }
        assert!(done);
        assert_eq!(re.finish().unwrap(), original);
    }

    #[test]
    fn splits_large_message_into_several_chunks() {
        let big = vec![b'x'; CHUNK_SIZE * 2 + 10];
        let (_mid, packages) = message_to_chunks(&big);
        // 1 metaData + 3 chunks
        assert_eq!(packages.len(), 1 + 3);
    }
}
