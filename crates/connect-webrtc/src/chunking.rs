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
        serde_json::from_slice(&msg).map_err(|e| ConnectError::Protocol(format!("invalid JSON: {e}")))
    }
}

/// Messages being reassembled at once, at most: a peer sends one at a time, and a few
/// more cover a send it gave up half way.
const MAX_IN_FLIGHT: usize = 8;

/// What a package completed or gave up on.
#[derive(Debug)]
pub enum Reassembled {
    /// Every chunk of the message arrived: the message, or why it is not valid.
    Message {
        message_id: String,
        result: Result<Value, ConnectError>,
    },
    /// An incomplete message dropped to make room for a newer one: the peer is told.
    Dropped { message_id: String },
}

/// The incoming messages of a data channel, each reassembled by its id.
///
/// One message at a time lost a message whenever another one's packages arrived before it
/// was complete (two sends at once on the other side): the newer `metaData` replaced it,
/// and its chunks were ignored. Each message now has its own reassembler; an incomplete
/// one is dropped, and the peer told, only to make room past [`MAX_IN_FLIGHT`].
#[derive(Default)]
pub struct Reassembly {
    in_flight: std::collections::VecDeque<Reassembler>,
}

impl Reassembly {
    /// Takes one package; returns what it completed or dropped.
    pub fn accept(&mut self, pkg: &Package) -> Vec<Reassembled> {
        match pkg {
            Package::MetaData { messageId, .. } => {
                let mut outcome = Vec::new();
                if self.in_flight.iter().any(|re| &re.message_id == messageId) {
                    return outcome;
                }
                if self.in_flight.len() >= MAX_IN_FLIGHT {
                    if let Some(oldest) = self.in_flight.pop_front() {
                        outcome.push(Reassembled::Dropped {
                            message_id: oldest.message_id,
                        });
                    }
                }
                if let Some(re) = Reassembler::from_meta(pkg) {
                    // A message of no chunks is complete at once.
                    if re.chunk_count == 0 {
                        outcome.push(Reassembled::Message {
                            result: re.finish(),
                            message_id: re.message_id,
                        });
                    } else {
                        self.in_flight.push_back(re);
                    }
                }
                outcome
            }
            Package::Chunk { messageId, .. } => {
                let Some(index) = self.in_flight.iter().position(|re| &re.message_id == messageId) else {
                    return Vec::new();
                };
                if !self.in_flight[index].add(pkg) {
                    return Vec::new();
                }
                let Some(re) = self.in_flight.remove(index) else {
                    return Vec::new();
                };
                vec![Reassembled::Message {
                    result: re.finish(),
                    message_id: re.message_id,
                }]
            }
            _ => Vec::new(),
        }
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

    fn packages_of(value: &Value) -> (String, Vec<Package>) {
        let (id, packages) = message_to_chunks(&serde_json::to_vec(value).unwrap());
        (
            id,
            packages
                .iter()
                .map(|p| serde_json::from_str(p).unwrap())
                .collect(),
        )
    }

    fn messages(outcomes: Vec<Reassembled>) -> Vec<Value> {
        outcomes
            .into_iter()
            .filter_map(|outcome| match outcome {
                Reassembled::Message { result, .. } => result.ok(),
                Reassembled::Dropped { .. } => None,
            })
            .collect()
    }

    #[test]
    fn two_messages_whose_packages_interleave_both_arrive() {
        let first = serde_json::json!({ "interactionId": "a", "big": "x".repeat(CHUNK_SIZE * 2) });
        let second = serde_json::json!({ "interactionId": "b", "big": "y".repeat(CHUNK_SIZE) });
        let (_, p) = packages_of(&first);
        let (_, r) = packages_of(&second);
        let mut reassembly = Reassembly::default();

        // meta P, meta R, then their chunks alternating.
        let mut got = Vec::new();
        for pkg in [&p[0], &r[0], &p[1], &r[1], &p[2], &r[2], &p[3]] {
            got.extend(messages(reassembly.accept(pkg)));
        }

        assert_eq!(got, vec![second, first]);
    }

    #[test]
    fn a_message_left_half_sent_does_not_block_the_next() {
        let half = serde_json::json!({ "big": "x".repeat(CHUNK_SIZE * 2) });
        let next = serde_json::json!({ "interactionId": "next" });
        let (_, h) = packages_of(&half);
        let (_, n) = packages_of(&next);
        let mut reassembly = Reassembly::default();

        reassembly.accept(&h[0]);
        reassembly.accept(&h[1]);
        let mut got = messages(reassembly.accept(&n[0]));
        got.extend(messages(reassembly.accept(&n[1])));

        assert_eq!(got, vec![next]);
    }

    #[test]
    fn past_the_limit_the_oldest_incomplete_message_is_dropped_and_said() {
        let mut reassembly = Reassembly::default();
        let mut ids = Vec::new();
        let mut dropped = Vec::new();
        for n in 0..=MAX_IN_FLIGHT {
            let (id, packages) =
                packages_of(&serde_json::json!({ "n": n, "big": "z".repeat(CHUNK_SIZE * 2) }));
            ids.push(id);
            for outcome in reassembly.accept(&packages[0]) {
                if let Reassembled::Dropped { message_id } = outcome {
                    dropped.push(message_id);
                }
            }
        }
        assert_eq!(dropped, vec![ids[0].clone()]);
    }

    #[test]
    fn splits_large_message_into_several_chunks() {
        let big = vec![b'x'; CHUNK_SIZE * 2 + 10];
        let (_mid, packages) = message_to_chunks(&big);
        // 1 metaData + 3 chunks
        assert_eq!(packages.len(), 1 + 3);
    }
}
