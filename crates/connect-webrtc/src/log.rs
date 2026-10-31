//! Every log event of the WebRTC connector, in one place — the same rules as the iroh connector's.
//!
//! What may appear, and what never does:
//! - a link is a short hash of its password ([`link_tag`]), never the password;
//! - a message is its `interactionId` and its `discriminator`, never its contents, which carry
//!   account addresses, personas and transactions.
//!
//! Levels: `warn` for what fails, `info` for the steps of a request and for a channel opening and
//! closing. Durations are whole milliseconds (`*_ms`).
//!
//! A channel closes in the background when it is dropped (see `Channel`'s `Drop`): a `closing`
//! line with no `closed` after it is a close that never finished — the process ended first — and
//! the wallet then only notices the connection went when it stops hearing from it.

use std::time::Duration;

use blake2::{Blake2b, Digest};
use serde_json::Value;

use crate::{ConnectError, Progress};

/// The target of every event here: filter with `RUST_LOG=radixdlt_connect_webrtc=info`.
const TARGET: &str = "radixdlt_connect_webrtc";

/// A link as the log names it: eight hex digits of a hash of its password, stable across
/// processes, so the lines of two processes on the same link can be told apart from another's.
pub(crate) fn link_tag(password: &[u8]) -> String {
    let hash = Blake2b::<blake2::digest::consts::U4>::new_with_prefix(password).finalize();
    hex::encode(hash)
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn text<'a>(message: &'a Value, key: &str) -> &'a str {
    message.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A step of a request, as it is reported to the caller.
pub(crate) fn step(link: &str, progress: &Progress) {
    match progress {
        Progress::TurnTaken { waited } => {
            tracing::info!(target: TARGET, link, waited_ms = ms(*waited), "request: the link's turn taken");
        }
        Progress::Settling { wait } => tracing::info!(
            target: TARGET,
            link,
            wait_ms = ms(*wait),
            "request: waiting for the wallet to let go of the link's previous channel"
        ),
        Progress::ChannelOpen { elapsed } => {
            tracing::info!(target: TARGET, link, elapsed_ms = ms(*elapsed), "request: channel open");
        }
        Progress::Delivered { elapsed } => tracing::info!(
            target: TARGET,
            link,
            elapsed_ms = ms(*elapsed),
            "request: the wallet confirmed receiving it"
        ),
        Progress::Resending { attempt, reason } => tracing::warn!(
            target: TARGET,
            link,
            attempt,
            reason,
            "request: never confirmed, sent again on a new channel"
        ),
        Progress::Reconnecting { reason } => tracing::warn!(
            target: TARGET,
            link,
            reason,
            "request: channel lost after delivery, reopening to wait for the answer"
        ),
        Progress::OtherMessage(message) => tracing::info!(
            target: TARGET,
            link,
            interaction_id = text(message, "interactionId"),
            discriminator = text(message, "discriminator"),
            "request: a message for another request"
        ),
    }
}

/// The request's answer arrived.
pub(crate) fn answered(link: &str, interaction_id: &str) {
    tracing::info!(target: TARGET, link, interaction_id, "request: answered");
}

/// A channel could not be opened.
pub(crate) fn channel_failed(link: &str, error: &ConnectError) {
    tracing::warn!(target: TARGET, link, %error, "channel: could not open");
}

/// A dropped channel starts closing (in the background).
pub(crate) fn channel_closing(link: &str) {
    tracing::info!(target: TARGET, link, "channel: closing");
}

/// The background close finished: the wallet has been told.
pub(crate) fn channel_closed(link: &str, took: Duration) {
    tracing::info!(target: TARGET, link, took_ms = ms(took), "channel: closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link is named the same in every process, apart from another, and never by its password.
    #[test]
    fn a_link_is_named_by_a_short_hash_of_its_password() {
        let tag = link_tag(b"the link's password");
        assert_eq!(tag.len(), 8);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(tag, link_tag(b"the link's password"));
        assert_ne!(tag, link_tag(b"another link's password"));
        assert!(!tag.contains(&hex::encode(b"the l")));
    }
}
