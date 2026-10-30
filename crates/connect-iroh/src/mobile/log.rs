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

pub(super) fn session_started(link: LinkId, relays_used: &Relays, diagnostics: &Diagnostics) {
    let relays = relays(relays_used);
    tracing::info!(target: TARGET, %link, relays, %diagnostics, "iroh session started; waiting for the wallet");
}

pub(super) fn session_failed(link: LinkId, relays_used: &Relays, error: &dyn std::fmt::Display) {
    let relays = relays(relays_used);
    tracing::warn!(target: TARGET, %link, relays, %error, "could not start the iroh session");
}

pub(super) fn session_start_timed_out(link: LinkId, relays_used: &Relays) {
    let relays = relays(relays_used);
    tracing::warn!(target: TARGET, %link, relays, "timed out starting the iroh session");
}

pub(super) fn session_ended(link: LinkId) {
    tracing::info!(target: TARGET, %link, "the link's iroh session ended");
}

pub(super) fn closing(link: LinkId) {
    tracing::info!(target: TARGET, %link, "closing the link's iroh session");
}

pub(super) fn state_changed(link: LinkId, connected: bool, diagnostics: &Diagnostics) {
    if connected {
        tracing::info!(target: TARGET, %link, %diagnostics, "the wallet connected over iroh");
    } else {
        tracing::info!(target: TARGET, %link, %diagnostics, "the wallet is not connected over iroh");
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
    tracing::warn!(target: TARGET, %link, bytes, "dropped a wallet message that is not a JSON object");
}

pub(super) fn kept_unclaimed(link: LinkId, interaction: &str) {
    tracing::info!(target: TARGET, %link, interaction, "an answer arrived that no request waits for; kept for await_response");
}

pub(super) fn skipped(link: LinkId, skipped: u64) {
    tracing::warn!(target: TARGET, %link, skipped, "fell behind and skipped wallet messages");
}

pub(super) fn pairing_started(link: LinkId, relays_used: &Relays) {
    let relays = relays(relays_used);
    tracing::info!(target: TARGET, %link, relays, "pairing: QR shown, waiting for the wallet to scan it");
}

pub(super) fn pairing_message(link: LinkId, is_link_client: bool) {
    if is_link_client {
        tracing::info!(target: TARGET, %link, "pairing: received the wallet's linkClient");
    } else {
        tracing::debug!(target: TARGET, %link, "pairing: ignoring a message that is not a linkClient");
    }
}

pub(super) fn pairing_timed_out(link: LinkId, seen_wallet: bool, diagnostics: &Diagnostics) {
    tracing::warn!(target: TARGET, %link, seen_wallet, %diagnostics, "pairing: timed out waiting for the wallet");
}

pub(super) fn linking_connection_closed(link: LinkId, closed: bool, diagnostics: &Diagnostics) {
    if closed {
        tracing::info!(target: TARGET, %link, %diagnostics, "pairing: the wallet closed its linking connection; its regular connection follows");
    } else {
        tracing::warn!(target: TARGET, %link, %diagnostics, "pairing: the wallet kept its linking connection open; a request sent over it now may be dropped");
    }
}

pub(super) fn paired(link: LinkId, started: Instant) {
    tracing::info!(target: TARGET, %link, elapsed = ?started.elapsed(), "pairing: the wallet linked; its session is kept for the regular connection");
}

pub(super) fn pairing_abandoned(link: LinkId) {
    tracing::info!(target: TARGET, %link, "pairing: abandoned by the caller; closing its session");
}

pub(super) fn pairing_failed(link: LinkId, error: &MobileError) {
    tracing::warn!(target: TARGET, %link, %error, "pairing failed; closing its session");
}

pub(super) fn waiting(link: LinkId, what: &str, started: Instant, diagnostics: &Diagnostics) {
    tracing::info!(target: TARGET, %link, interaction = what, waited = ?started.elapsed(), %diagnostics, "waiting for the wallet to connect");
}

pub(super) fn unreachable(link: LinkId, interaction: &str, diagnostics: &Diagnostics) {
    tracing::warn!(target: TARGET, %link, interaction, %diagnostics, "the wallet did not connect in time; nothing was sent");
}

pub(super) fn already_answered(link: LinkId, interaction: &str) {
    tracing::info!(target: TARGET, %link, interaction, "the answer had already arrived; nothing sent");
}

pub(super) fn sending(link: LinkId, interaction: &str, bytes: usize, attempt: u32) {
    tracing::debug!(target: TARGET, %link, interaction, bytes, attempt, "sending the request to the wallet");
}

pub(super) fn delivered(link: LinkId, interaction: &str, started: Instant) {
    tracing::info!(target: TARGET, %link, interaction, elapsed = ?started.elapsed(), "the wallet took the request");
}

pub(super) fn resending(link: LinkId, interaction: &str, attempt: u32, reason: &str) {
    tracing::warn!(target: TARGET, %link, interaction, attempt, reason, "nothing was sent; sending again once the wallet is back");
}

pub(super) fn unconfirmed(link: LinkId, interaction: &str, reason: &str, diagnostics: &Diagnostics) {
    tracing::warn!(target: TARGET, %link, interaction, reason, %diagnostics, "the wallet did not confirm the request; it is not sent again");
}

pub(super) fn answered(link: LinkId, interaction: &str, started: Instant, answered: bool) {
    if answered {
        tracing::info!(target: TARGET, %link, interaction, elapsed = ?started.elapsed(), "the wallet answered");
    } else {
        tracing::warn!(target: TARGET, %link, interaction, "the link closed while waiting for the wallet's answer");
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

pub(super) fn timed_out(link: LinkId, interaction: &str, confirmed: bool, diagnostics: &Diagnostics) {
    if confirmed {
        tracing::warn!(target: TARGET, %link, interaction, %diagnostics, "timed out waiting for the wallet's answer (the request is on the phone)");
    } else {
        tracing::warn!(target: TARGET, %link, interaction, %diagnostics, "timed out: the wallet never confirmed the request, which may or may not have reached it");
    }
}
