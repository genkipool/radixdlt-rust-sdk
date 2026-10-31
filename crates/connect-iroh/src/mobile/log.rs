//! Every log event of the mobile connector, in one place.
//!
//! What may appear, and what never does:
//! - a link is its [`LinkId`] — a short hash of the password — and never the password;
//! - a message is its size, its `interactionId` and its `discriminator` — never its contents,
//!   which carry account addresses, personas and transactions;
//! - connectivity is a [`Diagnostics`] line (endpoint ids are public keys).
//!
//! Levels: `warn` for what fails or may need a person to act, `info` for the steps of a pairing
//! and a request and for state changes, `debug` for every message.
//!
//! Durations are whole milliseconds (`*_ms`), and every timeout carries its limit (`limit_ms`).
//! Lines a misbehaving wallet could repeat without end are rate-limited per link (see [`every`]).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use radix_connect_iroh::{Diagnostics, Relays};
use tokio::time::Instant;

use super::link::{LinkId, Status};
use super::MobileError;

/// The target of every event here, whatever module emits it: filter with
/// `RUST_LOG=radixdlt_connect_iroh=debug` (and `radix_connect_iroh=debug` for the protocol's own).
const TARGET: &str = "radixdlt_connect_iroh::mobile";

/// The relay choice, for a log field: `public` or the custom relay's URL.
fn relays(relays: &Relays) -> String {
    match relays {
        Relays::Public => "public".to_string(),
        Relays::Custom(url) => url.to_string(),
    }
}

/// Whole milliseconds, for a log field.
pub(super) fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds since `started`.
fn since(started: Instant) -> u64 {
    ms(started.elapsed())
}

/// How often a line a wallet could repeat without end is written, per link.
const REPEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Whether a line of kind `what` on `link` is written now, and how many were left out since the
/// last one that was: the first is always written, then at most one per [`REPEAT_INTERVAL`].
fn every(what: &'static str, link: LinkId) -> Option<u64> {
    /// When each kind of line was last written per link, and how many were left out since.
    type Written = HashMap<(&'static str, LinkId), (Instant, u64)>;
    static LAST: OnceLock<Mutex<Written>> = OnceLock::new();
    let mut last = LAST
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    match last.get_mut(&(what, link)) {
        Some((at, skipped)) if now.saturating_duration_since(*at) < REPEAT_INTERVAL => {
            *skipped += 1;
            None
        }
        Some((at, skipped)) => {
            let left_out = *skipped;
            *at = now;
            *skipped = 0;
            Some(left_out)
        }
        None => {
            last.insert((what, link), (now, 0));
            Some(0)
        }
    }
}

pub(super) fn session_started(link: LinkId, relays_used: &Relays, diagnostics: &Diagnostics, start_ms: u64) {
    let relays = relays(relays_used);
    tracing::info!(target: TARGET, %link, relays, start_ms, %diagnostics, "iroh session started; waiting for the wallet");
}

pub(super) fn session_reused(link: LinkId) {
    tracing::debug!(target: TARGET, %link, "the link's iroh session is running; reusing it");
}

pub(super) fn session_restarting(link: LinkId) {
    tracing::info!(target: TARGET, %link, "the link's previous iroh session had ended; starting a new one");
}

pub(super) fn session_failed(link: LinkId, relays_used: &Relays, error: &dyn std::fmt::Display) {
    let relays = relays(relays_used);
    tracing::warn!(target: TARGET, %link, relays, %error, "could not start the iroh session");
}

pub(super) fn session_start_timed_out(link: LinkId, relays_used: &Relays, limit_ms: u64) {
    let relays = relays(relays_used);
    tracing::warn!(target: TARGET, %link, relays, limit_ms, "timed out starting the iroh session");
}

pub(super) fn session_ended(link: LinkId, started: Instant, state_changes: u64) {
    tracing::info!(target: TARGET, %link, lived_ms = since(started), state_changes, "the link's iroh session ended");
}

pub(super) fn removed_by_wallet(link: LinkId) {
    tracing::warn!(target: TARGET, %link, "the wallet removed this link: it will not answer over it again");
}

pub(super) fn removal_notice_sent(link: LinkId, started: Instant) {
    tracing::info!(target: TARGET, %link, elapsed_ms = since(started), "the wallet confirmed it was told the link was removed");
}

pub(super) fn closing(link: LinkId, why: &'static str) {
    tracing::info!(target: TARGET, %link, why, "closing the link's iroh session");
}

pub(super) fn state_changed(
    link: LinkId,
    connected: bool,
    first: bool,
    session_started: Instant,
    diagnostics: &Diagnostics,
) {
    let since_start_ms = since(session_started);
    if connected {
        // `first`: the time from (re)starting the session to the wallet reaching it.
        tracing::info!(target: TARGET, %link, first, since_start_ms, %diagnostics, "the wallet connected over iroh");
    } else {
        tracing::info!(target: TARGET, %link, since_start_ms, %diagnostics, "the wallet is not connected over iroh");
    }
}

pub(super) fn message(link: LinkId, bytes: usize, interaction: Option<&str>, discriminator: Option<&str>) {
    tracing::debug!(target: TARGET, %link,
        bytes,
        interaction = interaction.unwrap_or("-"),
        discriminator = discriminator.unwrap_or("-"),
        "message from the wallet"
    );
}

pub(super) fn not_json(link: LinkId, bytes: usize) {
    if let Some(left_out) = every("not_json", link) {
        tracing::warn!(target: TARGET, %link, bytes, left_out, "dropped a wallet message that is not a JSON object");
    }
}

/// `kind`: `late` (its request stopped waiting), `unknown` (no request on this link asked for it)
/// or `duplicate` (a second answer, replacing the one kept).
pub(super) fn kept_unclaimed(link: LinkId, interaction: &str, kind: &'static str) {
    if kind == "late" {
        tracing::info!(target: TARGET, %link, interaction, kind, "an answer arrived after its request stopped waiting; kept for await_response");
    } else {
        tracing::warn!(target: TARGET, %link, interaction, kind, "an answer arrived that no request expects; kept for await_response");
    }
}

pub(super) fn skipped(link: LinkId, skipped: u64) {
    if let Some(left_out) = every("skipped", link) {
        tracing::warn!(target: TARGET, %link, skipped, left_out, "fell behind and skipped wallet messages");
    }
}

pub(super) fn pairing_started(link: LinkId, relays_used: &Relays, limit_ms: u64) {
    let relays = relays(relays_used);
    tracing::info!(target: TARGET, %link, relays, limit_ms, "pairing: session listening, waiting for the wallet that scans the QR");
}

pub(super) fn pairing_message(link: LinkId, is_link_client: bool) {
    if is_link_client {
        tracing::info!(target: TARGET, %link, "pairing: received the wallet's linkClient");
    } else {
        tracing::debug!(target: TARGET, %link, "pairing: ignoring a message that is not a linkClient");
    }
}

pub(super) fn pairing_timed_out(
    link: LinkId,
    seen_wallet: bool,
    started: Instant,
    limit_ms: u64,
    diagnostics: &Diagnostics,
) {
    tracing::warn!(target: TARGET, %link, seen_wallet, waited_ms = since(started), limit_ms, %diagnostics, "pairing: timed out waiting for the wallet");
}

pub(super) fn linking_connection_closed(
    link: LinkId,
    closed: bool,
    waited: Duration,
    diagnostics: &Diagnostics,
) {
    let (waited_ms, limit_ms) = (ms(waited), ms(super::LINKING_GRACE));
    if closed {
        tracing::info!(target: TARGET, %link, waited_ms, %diagnostics, "pairing: the wallet closed its linking connection; its regular connection follows");
    } else {
        tracing::warn!(target: TARGET, %link, waited_ms, limit_ms, %diagnostics, "pairing: the wallet kept its linking connection open; a request sent over it now may be dropped");
    }
}

pub(super) fn paired(link: LinkId, started: Instant) {
    tracing::info!(target: TARGET, %link, elapsed_ms = since(started), "pairing: the wallet linked; its session is kept for the regular connection");
}

pub(super) fn pairing_abandoned(link: LinkId) {
    tracing::info!(target: TARGET, %link, "pairing: abandoned by the caller; closing its session");
}

pub(super) fn pairing_failed(link: LinkId, started: Instant, error: &MobileError) {
    tracing::warn!(target: TARGET, %link, elapsed_ms = since(started), %error, "pairing failed; closing its session");
}

pub(super) fn waiting(link: LinkId, what: &str, started: Instant, limit_ms: u64, diagnostics: &Diagnostics) {
    tracing::info!(target: TARGET, %link, interaction = what, waited_ms = since(started), limit_ms, %diagnostics, "waiting for the wallet to connect");
}

pub(super) fn unreachable(
    link: LinkId,
    interaction: &str,
    started: Instant,
    limit_ms: u64,
    diagnostics: &Diagnostics,
) {
    tracing::warn!(target: TARGET, %link, interaction, waited_ms = since(started), limit_ms, %diagnostics, "the wallet did not connect in time; nothing was sent");
}

pub(super) fn already_answered(link: LinkId, interaction: &str) {
    tracing::info!(target: TARGET, %link, interaction, "the answer had already arrived; nothing sent");
}

pub(super) fn registered(link: LinkId, interaction: &str, others_in_flight: usize) {
    tracing::debug!(target: TARGET, %link, interaction, others_in_flight, "waiting for the answer to this interaction");
}

pub(super) fn sending(link: LinkId, interaction: &str, bytes: usize, attempt: u32, started: Instant) {
    tracing::info!(target: TARGET, %link, interaction, bytes, attempt, elapsed_ms = since(started), "sending the request to the wallet");
}

pub(super) fn delivered(link: LinkId, interaction: &str, started: Instant) {
    tracing::info!(target: TARGET, %link, interaction, elapsed_ms = since(started), "the wallet took the request");
}

pub(super) fn resending(link: LinkId, interaction: &str, attempt: u32, reason: &str) {
    tracing::warn!(target: TARGET, %link, interaction, attempt, reason, "nothing was sent; sending again once the wallet is back");
}

pub(super) fn unconfirmed(link: LinkId, interaction: &str, reason: &str, diagnostics: &Diagnostics) {
    tracing::warn!(target: TARGET, %link, interaction, reason, %diagnostics, "the wallet did not confirm the request; it is not sent again");
}

pub(super) fn answered(link: LinkId, interaction: &str, started: Instant, answered: bool) {
    if answered {
        tracing::info!(target: TARGET, %link, interaction, elapsed_ms = since(started), "the wallet answered");
    } else {
        tracing::warn!(target: TARGET, %link, interaction, elapsed_ms = since(started), "the link closed while waiting for the wallet's answer");
    }
}

pub(super) fn changed_while_waiting(
    link: LinkId,
    interaction: &str,
    status: Status,
    diagnostics: &Diagnostics,
) {
    tracing::info!(target: TARGET, %link, interaction, ?status, %diagnostics, "the wallet's connection changed while waiting for its answer");
}

pub(super) fn timed_out(
    link: LinkId,
    interaction: &str,
    confirmed: bool,
    started: Instant,
    limit_ms: u64,
    diagnostics: &Diagnostics,
) {
    let waited_ms = since(started);
    if confirmed {
        tracing::warn!(target: TARGET, %link, interaction, waited_ms, limit_ms, %diagnostics, "timed out waiting for the wallet's answer (the request is on the phone)");
    } else {
        tracing::warn!(target: TARGET, %link, interaction, waited_ms, limit_ms, %diagnostics, "timed out: the wallet never confirmed the request, which may or may not have reached it");
    }
}
