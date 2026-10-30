//! The SDK-to-SDK message channel: every message on a QUIC stream of its own, confirmed by the
//! receiver.
//!
//! For each message (ALPN [`crate::ALPN`]):
//!
//! 1. the sender opens a bidirectional stream, writes a 4-byte big-endian length followed by the
//!    UTF-8 JSON body, and finishes its half;
//! 2. the receiver reads exactly that many bytes and the end of the stream — refusing, before it
//!    allocates anything, a length over its limit — and answers on the same stream with ONE
//!    status byte: `0x00` taken, `0x01` over its size limit, `0x02` not one JSON document of
//!    the announced length;
//! 3. the sender's [`IrohChannel::send_message`] returns once that byte arrives.
//!
//! Why a stream per message: a stream abandoned half way — a send cancelled or timed out — is
//! RESET, never finished, so the receiver cannot take a truncated message for a whole one, and the
//! next message starts on a fresh stream. Nothing a cancelled operation leaves behind can ever be
//! read as a length prefix. On a single long-lived stream, the bytes after an interrupted frame are
//! the middle of a message, and the next read takes JSON for a length.

use std::time::Duration;

use iroh::endpoint::{Connection, ReadError, ReadToEndError, RecvStream, SendStream, VarInt, WriteError};
use iroh::EndpointId;
use serde_json::Value;
use tokio::time;

use crate::IrohError;

/// Size of the big-endian length prefix of a message.
const LENGTH_PREFIX: usize = 4;

/// Status byte: the receiver took the message.
pub(crate) const STATUS_TAKEN: u8 = 0x00;
/// Status byte: the message is over the receiver's size limit; it was not read.
pub(crate) const STATUS_TOO_LARGE: u8 = 0x01;
/// Status byte: the message is not one JSON document of the length it announced.
pub(crate) const STATUS_MALFORMED: u8 = 0x02;

/// Stream error code: the receiver will not read the rest of a message over its limit.
pub(crate) const STREAM_TOO_LARGE: u32 = 1;
/// Stream error code: a message abandoned before it was complete (cancelled, timed out).
pub(crate) const STREAM_ABANDONED: u32 = 2;
/// Stream error code: the receiver will not read the rest of a message longer than it announced.
pub(crate) const STREAM_MALFORMED: u32 = 3;

/// QUIC close code of a channel closed by its owner.
const CLOSE_DONE: u32 = 0;

/// Default [`Limits::max_message_size`]: 8 MiB.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 8 * 1024 * 1024;
/// Default [`Limits::message_timeout`]: 60 s.
pub const DEFAULT_MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);
/// Default [`Limits::close_timeout`]: 5 s.
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The bounds every channel of an [`crate::IrohConnector`] applies. Set them with
/// [`crate::Builder::limits`]; the defaults suit JSON requests and answers.
///
/// What is NOT bounded, on purpose: waiting for the peer's NEXT message. A channel may stay open
/// and idle for as long as both ends want (a server keeps its machines' channels open); wrap
/// [`IrohChannel::recv_message`] in a timeout of your own where you want one — it is cancel-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Largest JSON body sent or accepted, in bytes. A peer announcing more is refused before
    /// anything is allocated for it, and the channel stays usable. Default 8 MiB.
    pub max_message_size: usize,
    /// Once a peer has STARTED a message, how long the whole of it may take to arrive; and how
    /// long one of ours may take to be delivered and confirmed. Default 60 s.
    pub message_timeout: Duration,
    /// How long [`IrohChannel::close`], [`IrohChannel::wait_closed`] and
    /// [`crate::IrohConnector::close`] may wait. Default 5 s.
    pub close_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            message_timeout: DEFAULT_MESSAGE_TIMEOUT,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
        }
    }
}

impl Limits {
    /// Sets [`Limits::max_message_size`]. Capped at `u32::MAX`, what the length prefix can say.
    #[must_use]
    pub fn with_max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = bytes.min(usize::try_from(u32::MAX).unwrap_or(usize::MAX));
        self
    }

    /// Sets [`Limits::message_timeout`].
    #[must_use]
    pub fn with_message_timeout(mut self, timeout: Duration) -> Self {
        self.message_timeout = timeout;
        self
    }

    /// Sets [`Limits::close_timeout`].
    #[must_use]
    pub fn with_close_timeout(mut self, timeout: Duration) -> Self {
        self.close_timeout = timeout;
        self
    }
}

/// A send stream that is RESET, not finished, if it is dropped before [`Guarded::finish`].
///
/// Dropping a QUIC send stream finishes it, which would hand the peer whatever was written so far
/// as if it were complete: a send cancelled half way must abort its stream instead. The same guard
/// holds the receiver's answer, so a receiver that goes away without answering resets it too, and
/// the sender never reads that silence as a confirmation.
#[derive(Debug)]
struct Guarded(Option<SendStream>);

impl Guarded {
    fn stream(&mut self) -> Result<&mut SendStream, WriteError> {
        self.0.as_mut().ok_or(WriteError::ClosedStream)
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.stream()?.write_all(bytes).await
    }

    /// Writes one byte. Cancel-safe: if this does not complete, nothing was written.
    async fn write_byte(&mut self, byte: u8) -> Result<(), WriteError> {
        self.stream()?.write(&[byte]).await.map(|_| ())
    }

    /// Finishes the stream and disarms the guard. Returns it so the caller can wait for the peer
    /// to acknowledge what was written.
    fn finish(mut self) -> Option<SendStream> {
        let mut stream = self.0.take()?;
        stream.finish().ok()?;
        Some(stream)
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        if let Some(mut stream) = self.0.take() {
            let _ = stream.reset(VarInt::from_u32(STREAM_ABANDONED));
        }
    }
}

/// What the receiver decided about a message it read whole (or refused), before its answer to the
/// sender is written.
#[derive(Debug)]
enum Verdict {
    Taken(Value),
    Refused(IrohError),
}

/// How reading one stream ended.
#[derive(Debug)]
enum Read {
    /// The status byte is written (or the sender is gone): hand the verdict to the caller.
    Answered,
    /// The sender abandoned the message before it was complete: nothing to deliver or answer.
    Abandoned,
}

/// A message the peer is sending, as far as it has arrived. Kept in the channel across cancelled
/// [`IrohChannel::recv_message`] calls, so a cancelled receive loses nothing and resumes exactly
/// where it stopped.
#[derive(Debug)]
struct Inbound {
    answer: Guarded,
    recv: RecvStream,
    /// The length prefix, then the body, as they arrive. Grows with the data, never from the
    /// announced length: a peer announcing 4 GiB and sending 10 bytes costs 14 bytes.
    bytes: Vec<u8>,
    /// The body's announced length, once the prefix is in (and within the limit).
    length: Option<usize>,
    /// Decided; waiting for the status byte to be written.
    verdict: Option<(u8, Verdict)>,
}

impl Inbound {
    fn new(answer: SendStream, recv: RecvStream) -> Self {
        Inbound {
            answer: Guarded(Some(answer)),
            recv,
            bytes: Vec::new(),
            length: None,
            verdict: None,
        }
    }

    /// Reads until the message is decided and answered. Cancel-safe: every await is, and all
    /// progress is kept in `self`.
    async fn read(&mut self, max_message_size: usize) -> Result<Read, IrohError> {
        loop {
            if let Some((status, _)) = &self.verdict {
                // A sender that gave up waiting no longer needs the answer: a failed write changes
                // nothing about what was received.
                let _ = self.answer.write_byte(*status).await;
                return Ok(Read::Answered);
            }
            let wanted = match self.length {
                None => LENGTH_PREFIX.saturating_sub(self.bytes.len()),
                // The body, then one more read that must find the end of the stream.
                Some(length) => (LENGTH_PREFIX + length).saturating_sub(self.bytes.len()).max(1),
            };
            let chunk = match self.recv.read_chunk(wanted).await {
                Ok(chunk) => chunk,
                // The sender abandoned it (a reset): the next message is on another stream.
                Err(ReadError::Reset(_)) => return Ok(Read::Abandoned),
                Err(error) => return Err(lost(error)),
            };
            let complete = self
                .length
                .is_some_and(|length| self.bytes.len() == LENGTH_PREFIX + length);
            match (chunk, complete) {
                // The end of the stream right after the body: the message is whole.
                (None, true) => self.decide_body(),
                // Ended before it was complete: the sender did not follow the protocol, and it
                // will learn so from the reset answer. Nothing to deliver.
                (None, false) => return Ok(Read::Abandoned),
                // More than it announced.
                (Some(_), true) => {
                    let _ = self.recv.stop(VarInt::from_u32(STREAM_MALFORMED));
                    self.verdict = Some((
                        STATUS_MALFORMED,
                        Verdict::Refused(IrohError::Protocol(
                            "the message is longer than its announced length".into(),
                        )),
                    ));
                }
                (Some(chunk), false) => {
                    self.bytes.extend_from_slice(&chunk);
                    if self.length.is_none() && self.bytes.len() == LENGTH_PREFIX {
                        self.decide_length(max_message_size);
                    }
                }
            }
        }
    }

    fn decide_length(&mut self, max_message_size: usize) {
        let Some(prefix) = self
            .bytes
            .get(..LENGTH_PREFIX)
            .and_then(|prefix| <[u8; LENGTH_PREFIX]>::try_from(prefix).ok())
        else {
            return;
        };
        let announced = u32::from_be_bytes(prefix);
        match usize::try_from(announced) {
            Ok(length) if length <= max_message_size => self.length = Some(length),
            // Refused from the prefix alone: nothing is allocated for it, and the rest is not read.
            _ => {
                let _ = self.recv.stop(VarInt::from_u32(STREAM_TOO_LARGE));
                self.verdict = Some((
                    STATUS_TOO_LARGE,
                    Verdict::Refused(IrohError::TooLarge {
                        size: u64::from(announced),
                        max: max_message_size,
                    }),
                ));
            }
        }
    }

    fn decide_body(&mut self) {
        let body = self.bytes.get(LENGTH_PREFIX..).unwrap_or_default();
        self.verdict = Some(match serde_json::from_slice(body) {
            Ok(message) => (STATUS_TAKEN, Verdict::Taken(message)),
            Err(e) => (
                STATUS_MALFORMED,
                Verdict::Refused(IrohError::Protocol(e.to_string())),
            ),
        });
        self.bytes = Vec::new();
    }

    /// Finishes the answer stream (returned so `close` can let it reach the peer) and hands over
    /// the verdict.
    fn into_verdict(mut self) -> (Option<SendStream>, Option<Verdict>) {
        let verdict = self.verdict.take().map(|(_, verdict)| verdict);
        let answer = std::mem::replace(&mut self.answer, Guarded(None)).finish();
        (answer, verdict)
    }
}

/// A bidirectional JSON message channel with one peer, over an iroh (QUIC) connection.
///
/// Every message travels on a stream of its own and is confirmed by the receiver: when
/// [`send_message`](Self::send_message) returns `Ok`, the peer has taken the message. Both
/// operations are cancel-safe — wrap them in `tokio::time::timeout` or `select!` freely: a
/// cancelled receive resumes where it stopped, and a cancelled send is abandoned as a whole (the
/// peer never sees part of it). Neither can leave the channel out of step.
#[derive(Debug)]
pub struct IrohChannel {
    conn: Connection,
    limits: Limits,
    inbound: Option<Inbound>,
    /// Our answer to the last message received, so [`close`](Self::close) can let it reach the
    /// peer before the connection goes.
    last_answer: Option<SendStream>,
}

impl IrohChannel {
    pub(crate) fn new(conn: Connection, limits: Limits) -> Self {
        IrohChannel {
            conn,
            limits,
            inbound: None,
            last_answer: None,
        }
    }

    /// A channel whose peer has already opened its first message (the accepting side).
    pub(crate) fn with_first(conn: Connection, limits: Limits, first: (SendStream, RecvStream)) -> Self {
        let (answer, recv) = first;
        let mut channel = IrohChannel::new(conn, limits);
        channel.inbound = Some(Inbound::new(answer, recv));
        channel
    }

    /// The peer's identity: its `EndpointId`, authenticated by the QUIC (TLS 1.3) handshake. For a
    /// channel opened with [`crate::IrohConnector::connect`] it is the id that was dialled.
    pub fn remote_id(&self) -> EndpointId {
        self.conn.remote_id()
    }

    /// The bounds this channel applies.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Sends a JSON message and waits until the peer has taken it.
    ///
    /// Cancel-safe: cancelled before the whole message was written, the message is abandoned
    /// (its stream is reset) and the peer never sees it; cancelled after, the peer may still take
    /// it. Either way the channel stays usable.
    ///
    /// # Errors
    /// - [`IrohError::TooLarge`]: over this side's [`Limits::max_message_size`]; nothing was sent.
    /// - [`IrohError::Refused`]: the peer refused it (over ITS limit, or not JSON).
    /// - [`IrohError::Timeout`]: not delivered and confirmed within [`Limits::message_timeout`].
    ///   The peer may still take it if it was written whole.
    /// - [`IrohError::Stream`]: the connection was lost.
    /// - [`IrohError::Protocol`]: `message` could not be serialized.
    pub async fn send_message(&mut self, message: &Value) -> Result<(), IrohError> {
        let body = serde_json::to_vec(message).map_err(|e| IrohError::Protocol(e.to_string()))?;
        let max = self.limits.max_message_size;
        let too_large = || IrohError::TooLarge {
            size: u64::try_from(body.len()).unwrap_or(u64::MAX),
            max,
        };
        if body.len() > max {
            return Err(too_large());
        }
        let length = u32::try_from(body.len()).map_err(|_| too_large())?;
        let mut frame = Vec::with_capacity(LENGTH_PREFIX + body.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&body);
        let after = self.limits.message_timeout;
        time::timeout(after, deliver(&self.conn, &frame))
            .await
            .unwrap_or(Err(IrohError::Timeout { sending: true, after }))
    }

    /// Receives the next JSON message, confirming it to the peer.
    ///
    /// Waiting for the peer to START a message is not bounded (a channel may stay idle as long as
    /// both ends want); once it has started, the whole of it must arrive within
    /// [`Limits::message_timeout`]. Cancel-safe: a cancelled call keeps what has arrived, and the
    /// next call resumes it. A message the peer abandons half way is skipped.
    ///
    /// After an error about one message ([`IrohError::TooLarge`], [`IrohError::Timeout`],
    /// [`IrohError::Protocol`]) the channel stays usable: the next call reads the next message.
    ///
    /// # Errors
    /// - [`IrohError::TooLarge`]: the peer announced more than [`Limits::max_message_size`]. It
    ///   was refused from its length alone: nothing was allocated for it.
    /// - [`IrohError::Timeout`]: started but not complete within [`Limits::message_timeout`].
    /// - [`IrohError::Protocol`]: not one JSON document, or longer than it announced.
    /// - [`IrohError::Stream`]: the connection was closed or lost.
    pub async fn recv_message(&mut self) -> Result<Value, IrohError> {
        loop {
            if self.inbound.is_none() {
                // Not bounded: waiting for the peer's next message is what an idle channel does.
                let (answer, recv) = self.conn.accept_bi().await.map_err(lost)?;
                self.inbound = Some(Inbound::new(answer, recv));
            }
            let Some(inbound) = self.inbound.as_mut() else {
                continue;
            };
            let after = self.limits.message_timeout;
            let read = time::timeout(after, inbound.read(self.limits.max_message_size)).await;
            // Past this point the message is over, one way or another: its stream is let go.
            let Some(inbound) = self.inbound.take() else {
                continue;
            };
            match read {
                Ok(Ok(Read::Answered)) => {
                    let (answer, verdict) = inbound.into_verdict();
                    if answer.is_some() {
                        self.last_answer = answer;
                    }
                    match verdict {
                        Some(Verdict::Taken(message)) => return Ok(message),
                        Some(Verdict::Refused(error)) => return Err(error),
                        None => continue,
                    }
                }
                Ok(Ok(Read::Abandoned)) => continue,
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    // Dropped unanswered: its answer is reset, so the sender learns it failed.
                    let mut inbound = inbound;
                    let _ = inbound.recv.stop(VarInt::from_u32(STREAM_ABANDONED));
                    return Err(IrohError::Timeout {
                        sending: false,
                        after,
                    });
                }
            }
        }
    }

    /// Closes the channel: lets our confirmation of the last message received reach the peer (at
    /// most [`Limits::close_timeout`]), then closes the connection. Messages sent with
    /// [`send_message`](Self::send_message) are already confirmed, so nothing is lost by closing
    /// right after one.
    ///
    /// Dropping a channel does the same in the background when a tokio runtime is there to run
    /// it; `close` is the way to know it is done.
    pub async fn close(mut self) {
        if let Some(answer) = self.last_answer.take() {
            let _ = time::timeout(self.limits.close_timeout, answer.stopped()).await;
        }
        self.conn.close(VarInt::from_u32(CLOSE_DONE), b"done");
    }

    /// Waits until the peer closes the connection, at most [`Limits::close_timeout`]. Returns
    /// whether it did.
    pub async fn wait_closed(&self) -> bool {
        time::timeout(self.limits.close_timeout, self.conn.closed())
            .await
            .is_ok()
    }
}

impl Drop for IrohChannel {
    fn drop(&mut self) {
        // Closed at once, the connection would discard our confirmation of the last message if
        // it is still in flight, and the peer would report a message it delivered as lost. So it
        // is let through first (bounded), and only then closed.
        let Some(answer) = self.last_answer.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let conn = self.conn.clone();
            let within = self.limits.close_timeout;
            runtime.spawn(async move {
                let _ = time::timeout(within, answer.stopped()).await;
                conn.close(VarInt::from_u32(CLOSE_DONE), b"done");
            });
        }
    }
}

/// Opens a stream, writes the frame, and waits for the peer's status byte.
async fn deliver(conn: &Connection, frame: &[u8]) -> Result<(), IrohError> {
    let (send, mut recv) = conn.open_bi().await.map_err(lost)?;
    let mut send = Guarded(Some(send));
    send.write_all(frame).await.map_err(write_failed)?;
    // Finished, not reset: from here on the message is whole and the peer may take it. Kept until
    // the answer arrives, so dropping it here cannot be mistaken for anything.
    let _sent = send.finish();
    match recv.read_to_end(1).await {
        Ok(status) => match status.as_slice() {
            [STATUS_TAKEN] => Ok(()),
            [STATUS_TOO_LARGE] => Err(IrohError::Refused { too_large: true }),
            [STATUS_MALFORMED] => Err(IrohError::Refused { too_large: false }),
            _ => Err(IrohError::Stream("the peer did not confirm the message".into())),
        },
        Err(ReadToEndError::TooLong) => Err(IrohError::Protocol(
            "the peer's confirmation is longer than one byte".into(),
        )),
        Err(ReadToEndError::Read(error)) => Err(lost(error)),
    }
}

fn write_failed(error: WriteError) -> IrohError {
    match error {
        WriteError::Stopped(code) if code == VarInt::from_u32(STREAM_TOO_LARGE) => {
            IrohError::Refused { too_large: true }
        }
        WriteError::Stopped(code) if code == VarInt::from_u32(STREAM_MALFORMED) => {
            IrohError::Refused { too_large: false }
        }
        other => lost(other),
    }
}

fn lost(error: impl std::fmt::Display) -> IrohError {
    IrohError::Stream(error.to_string())
}
