//! Failures an agent can act on, and the connector's diagnostic log.
//!
//! Every failure carries a stable `code` (what happened), a `stage` (how far the request got),
//! whether sending it again is safe (`retry_safe`), and a hint written as the next thing to do.
//! The distinction that matters most is the stage: a request that never reached the wallet can be
//! sent again; one the wallet RECEIVED is in its queue on the phone until somebody approves or
//! rejects it, and sending it again only stacks a second copy behind the first.
//!
//! The log is JSON lines (`connector.log`, next to `connector.json`): one line per step of every
//! request, so a failure can be traced after the fact with `connector_log`. It never holds the
//! link password, and manifests appear only as a short summary.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use radixdlt_connect::ConnectError;
use radixdlt_connect_iroh::mobile::MobileError;
use serde_json::{json, Value};

/// The log file rolls over to `connector.log.1` beyond this size.
const LOG_MAX_BYTES: u64 = 1_000_000;

/// A failure, in the shape an agent can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Stable machine-readable code, e.g. `WALLET_UNREACHABLE`, `NO_ANSWER`, `WRONG_NETWORK`.
    pub code: &'static str,
    /// How far the request got: `input`, `preflight`, `queue`, `connect`, `deliver`,
    /// `awaiting_approval`, `wallet`, `verify`, `local`.
    pub stage: &'static str,
    /// Whether sending the same request again can neither duplicate it nor stack it in the wallet.
    pub retry_safe: bool,
    /// What went wrong, as the transport or the wallet said it.
    pub detail: String,
    /// What to do next.
    pub hint: String,
    /// The request this is about, when one was built.
    pub interaction_id: Option<String>,
}

impl Failure {
    pub fn new(
        code: &'static str,
        stage: &'static str,
        retry_safe: bool,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Failure {
            code,
            stage,
            retry_safe,
            detail: detail.into(),
            hint: hint.into(),
            interaction_id: None,
        }
    }

    /// A bad or missing argument: nothing was sent.
    pub fn input(detail: impl Into<String>) -> Self {
        Failure::new(
            "INVALID_ARGUMENT",
            "input",
            true,
            detail,
            "Fix the arguments and call again. Nothing was sent to the wallet.",
        )
    }

    /// A local problem (state file, disk): nothing was sent.
    pub fn local(detail: impl Into<String>) -> Self {
        Failure::new(
            "LOCAL_ERROR",
            "local",
            true,
            detail,
            "Nothing was sent to the wallet. Check the connector's config directory (permissions, disk).",
        )
    }

    pub fn with_interaction(mut self, interaction_id: &str) -> Self {
        if !interaction_id.is_empty() {
            self.interaction_id = Some(interaction_id.to_string());
        }
        self
    }

    /// Maps a transport error, knowing whether the wallet had already received the request.
    pub fn from_connect(error: &ConnectError, delivered: bool) -> Self {
        let detail = error.to_string();
        match error {
            ConnectError::WalletRejected(wallet) => Failure::from_wallet(wallet),
            ConnectError::LinkBusy => Failure::new(
                "LINK_BUSY",
                "queue",
                true,
                detail,
                "Another request is still running on this wallet link, or the wallet is still letting go of the previous channel (it needs a few seconds). Wait for it to finish (or cancel it with cancel_request), then call again.",
            ),
            ConnectError::Signaling(_) => Failure::new(
                "SIGNALING_UNREACHABLE",
                "connect",
                true,
                detail,
                "This machine could not reach the Radix Connect signaling server (internet, proxy or firewall). Nothing reached the wallet; retry once the network is back.",
            ),
            ConnectError::SignalingClosed if !delivered => Failure::new(
                "SIGNALING_CLOSED",
                "connect",
                true,
                detail,
                "The connection to the wallet closed before the request was sent. Nothing reached the wallet; retry.",
            ),
            ConnectError::ChannelTimeout => Failure::new(
                "WALLET_UNREACHABLE",
                "connect",
                true,
                detail,
                "The wallet never joined the channel, so the request was NOT delivered. Ask the person to open the Radix Wallet app and keep it in the foreground (it only listens while open), check the phone has internet, then retry. If it keeps failing, this connector may have been removed from the wallet (Settings › Linked Connectors): check with list_wallets and pair again.",
            ),
            ConnectError::WebRtc(_) | ConnectError::Crypto(_) if !delivered => Failure::new(
                "CONNECTION_FAILED",
                "connect",
                true,
                detail,
                "The channel to the wallet could not be set up. Nothing reached the wallet; retry. If it persists, check UDP/TURN is not blocked on this network.",
            ),
            ConnectError::ConfirmationTimeout => Failure::new(
                "NOT_DELIVERED",
                "deliver",
                true,
                format!("{detail} (after {} attempts, each on a fresh channel)", radixdlt_connect::SEND_ATTEMPTS),
                "The wallet app opened the channel but never confirmed receiving the request, so it is almost certainly NOT on the phone. Ask the person to bring the Radix Wallet to the foreground (or close and reopen it), check with check_wallet_connection, then send again. If the phone shows it after all, they answer it and await_response {interaction_id} collects the answer.",
            ),
            ConnectError::ResponseTimeout if delivered => Failure::new(
                "NO_ANSWER",
                "awaiting_approval",
                false,
                detail,
                "The wallet RECEIVED the request but nobody approved or rejected it in time. It is still in the wallet's queue: do NOT send it again. Ask the person to open the Radix Wallet. If the request shows, they approve or reject it and await_response {interaction_id} collects the answer. If nothing shows, the wallet's queue is stuck: force-close the app and open it again (that empties its queue), then cancel_request {interaction_id} here.",
            ),
            ConnectError::ResponseTimeout => Failure::new(
                "NO_ANSWER",
                "awaiting_approval",
                false,
                detail,
                "Nobody answered in time. Ask the person to look at the Radix Wallet, then call await_response again, or clear it with cancel_request.",
            ),
            _ if delivered => Failure::new(
                "CHANNEL_LOST",
                "awaiting_approval",
                false,
                detail,
                "The channel broke AFTER the wallet received the request, so it is still on the phone. Do not send it again: ask the person to answer it and collect the answer with await_response {interaction_id}, or clear it with cancel_request.",
            ),
            _ => Failure::new(
                "PROTOCOL_ERROR",
                "connect",
                true,
                detail,
                "Unexpected message from the wallet before delivery. Nothing reached the wallet; retry, and report it if it repeats.",
            ),
        }
    }

    /// Maps an iroh transport error, knowing whether the wallet had already received the request.
    ///
    /// The codes are the WebRTC ones where the meaning is the same, so an agent handles both
    /// transports alike. The timeouts' detail ends with the link's diagnostics line (relay
    /// reachable?, connection state, path, last error) — never a secret.
    pub fn from_mobile(error: &MobileError, delivered: bool) -> Self {
        let detail = error.to_string();
        match error {
            MobileError::WalletRejected(wallet) => Failure::from_wallet(wallet),
            MobileError::WalletUnreachable { .. } => Failure::new(
                "WALLET_UNREACHABLE",
                "connect",
                true,
                detail,
                "The wallet never connected over iroh, so the request was NOT delivered. Ask the person to open the Radix Wallet app (it only connects while open) with iroh on: Settings › Preferences › Radix Connect › Automatic or iroh, and the same iroh relay choice as this connector (both public by default). If the diagnostics say `relay … (NOT connected)`, this machine cannot reach its relay. Then retry; if it keeps failing, check with list_wallets that the link exists, and pair again.",
            ),
            MobileError::ConfirmationTimeout { .. } => Failure::new(
                "NOT_CONFIRMED",
                "deliver",
                false,
                detail,
                "The wallet never confirmed the request: it may or may not be on the phone, so it was NOT sent again. Ask the person to look at the Radix Wallet: if the request shows, they answer it and await_response {interaction_id} collects the answer; if nothing shows, clear it with cancel_request {interaction_id} and send it again.",
            ),
            MobileError::ResponseTimeout { .. } => Failure::new(
                "NO_ANSWER",
                "awaiting_approval",
                false,
                detail,
                "The wallet RECEIVED the request but nobody approved or rejected it in time. It is still in the wallet's queue: do NOT send it again. Ask the person to open the Radix Wallet; they approve or reject it and await_response {interaction_id} collects the answer (it is kept here even if it arrives before). If nothing shows, force-close and reopen the app, then cancel_request {interaction_id}.",
            ),
            MobileError::MessageTooLarge { .. } => Failure::new(
                "REQUEST_TOO_LARGE",
                "input",
                true,
                detail,
                "The request is larger than a Radix Connect message may be. Nothing was sent; make it smaller (blobs above all).",
            ),
            MobileError::MessageRefused => Failure::new(
                "NOT_DELIVERED",
                "deliver",
                true,
                detail,
                "The wallet refused the message itself, so it is not on the phone. Retry; report it if it repeats.",
            ),
            MobileError::Session(_) | MobileError::InvalidRelayUrl(_) => Failure::new(
                "IROH_UNAVAILABLE",
                "connect",
                true,
                detail,
                "This connector could not start its iroh endpoint. Nothing reached the wallet. Check the network, and the iroh relay setting (RADIX_CONNECTOR_IROH_RELAY / --iroh-relay), then retry.",
            ),
            MobileError::InvalidPassword => Failure::local(format!("{detail} (the stored link is damaged: pair again)")),
            _ if delivered => Failure::new(
                "CHANNEL_LOST",
                "awaiting_approval",
                false,
                detail,
                "The link closed AFTER the wallet received the request, so it is still on the phone. Do not send it again: ask the person to answer it and collect the answer with await_response {interaction_id}, or clear it with cancel_request.",
            ),
            _ => Failure::new(
                "CONNECTION_FAILED",
                "connect",
                true,
                detail,
                "The iroh link failed before the request was sent. Nothing reached the wallet; retry.",
            ),
        }
    }

    /// Maps a wallet `failure` answer (`"<errorType>"` or `"<errorType>: <message>"`).
    pub fn from_wallet(wallet: &str) -> Self {
        let error_type = wallet.split(':').next().unwrap_or(wallet).trim();
        let detail = format!("the wallet answered with a failure: {wallet}");
        let (code, retry_safe, hint): (&'static str, bool, &str) = match error_type {
            "rejectedByUser" => (
                "REJECTED_BY_USER",
                true,
                "The person rejected it on the phone. Do not send it again unless they ask for it.",
            ),
            "wrongNetwork" => (
                "WRONG_NETWORK",
                true,
                "The wallet is on another network. Ask the person to switch it (Radix Wallet › Settings › App Settings › Gateways) to the request's network, or send the request with the network the wallet is on.",
            ),
            "unknownWebsite" | "radixJsonNotFound" | "unknownDappDefinitionAddress" | "wrongAccountType"
            | "invalidOriginURL" | "invalidOrigin" => (
                "DAPP_NOT_VERIFIED",
                true,
                "The wallet could not verify the dApp: `dapp_definition` must be a 'dapp definition' account ON THIS NETWORK whose claimed_websites include `origin`, and {origin}/.well-known/radix.json must list it. Run check_dapp_identity to see which link is missing.",
            ),
            "invalidRequest" => (
                "INVALID_REQUEST",
                true,
                "The wallet refused the request itself. Usual causes: a manifest with lock_fee (the wallet adds its own), an empty or malformed dapp_definition, a malformed manifest or blob, or a request shape the wallet does not support. Validate the manifest (validate_transaction_manifest) and remove any lock_fee.",
            ),
            "failedToPrepareTransaction" | "failedToCompileTransaction" => (
                "MANIFEST_REJECTED",
                true,
                "The wallet could not build a transaction from the manifest. Validate and preview it (validate_transaction_manifest, preview_transaction) before sending again.",
            ),
            "failedToFindAccountWithEnoughFundsToLockFee" => (
                "NO_FEE_FUNDS",
                true,
                "No account in the wallet has enough XRD to pay the fee. Fund one (on Stokenet: build_faucet_manifest) and retry.",
            ),
            "failedToSignTransaction" | "failedToSignAuthChallenge" => (
                "SIGNING_FAILED",
                true,
                "The wallet could not sign (a Ledger device not connected, or a key it cannot use). Ask the person what the phone showed, then retry.",
            ),
            "failedToSubmitTransaction" => (
                "SUBMIT_FAILED",
                false,
                "The wallet signed but could not submit. Check transaction_status before sending anything again.",
            ),
            "failedToPollSubmittedTransaction" => (
                "SUBMITTED_STATUS_UNKNOWN",
                false,
                "The transaction WAS submitted but the wallet lost track of it. Do not send it again: check transaction_status.",
            ),
            "submittedTransactionWasDuplicate" => (
                "DUPLICATE_TRANSACTION",
                false,
                "The same transaction had already been submitted. Check transaction_status for the earlier one.",
            ),
            "submittedTransactionHasFailedTransactionStatus" => (
                "TRANSACTION_FAILED",
                false,
                "The transaction was committed but FAILED on-ledger (the fee was paid). Look at the receipt (get_transaction) before trying a corrected manifest.",
            ),
            "submittedTransactionHasRejectedTransactionStatus" => (
                "TRANSACTION_REJECTED",
                true,
                "The network rejected the transaction (nothing was committed). Preview the manifest to see why before retrying.",
            ),
            "invalidPersona" => (
                "INVALID_PERSONA",
                true,
                "The persona named is not one the wallet has logged in to this dApp. Log in first (request_login) and use the identity address it returns.",
            ),
            "invalidPersonaOrAccounts" => (
                "INVALID_PERSONA_OR_ACCOUNTS",
                true,
                "The proof of ownership named accounts or a persona this wallet does not hold. Use addresses the wallet shared (request_accounts / request_login).",
            ),
            "incompatibleVersion" => (
                "INCOMPATIBLE_VERSION",
                true,
                "The wallet and the connector speak different versions of the protocol. Update the Radix Wallet app and this connector.",
            ),
            "expiredSubintent" | "subintentExpirationTooClose" => (
                "SUBINTENT_EXPIRY",
                true,
                "The pre-authorization would expire too soon. Use a larger expire_after_seconds.",
            ),
            _ => (
                "WALLET_FAILURE",
                true,
                "The wallet answered with a failure. Read the detail; ask the person what the phone showed.",
            ),
        };
        Failure::new(code, "wallet", retry_safe, detail, hint)
    }

    /// The text block shown to the agent.
    pub fn to_text(&self, tool: &str, log_path: &Path) -> String {
        let mut out = format!(
            "✗ {code} — {tool} failed\n\
             stage:       {stage}\n\
             retry_safe:  {retry}\n",
            code = self.code,
            stage = self.stage,
            retry = if self.retry_safe {
                "yes (nothing is waiting in the wallet because of this call)"
            } else {
                "NO (the wallet may still hold this request — do not resend)"
            },
        );
        if let Some(id) = &self.interaction_id {
            out.push_str(&format!("interaction: {id}\n"));
        }
        let trace = match &self.interaction_id {
            Some(id) => format!("connector_log {{ \"interaction_id\": \"{id}\" }}"),
            None => "connector_log {}".to_string(),
        };
        out.push_str(&format!(
            "detail:      {detail}\n\nWhat to do: {hint}\n\nTrace: {trace} (log file: {log})",
            detail = self.detail,
            hint = self.hint,
            log = log_path.display(),
        ));
        out
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ok": false,
            "code": self.code,
            "stage": self.stage,
            "retry_safe": self.retry_safe,
            "detail": self.detail,
            "hint": self.hint,
            "interaction_id": self.interaction_id,
        })
    }
}

/* ─────────────────────────────────── log ─────────────────────────────────── */

/// Append-only JSON-lines log, mirrored to stderr.
pub struct Log {
    path: PathBuf,
}

impl Log {
    pub fn new(path: PathBuf) -> Self {
        Log { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records one event. Never fails the caller: a log that cannot be written is reported on
    /// stderr and otherwise ignored.
    pub fn event(&self, event: &str, fields: Value) {
        let mut line = json!({ "ts": iso_now(), "pid": std::process::id(), "event": event });
        if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
            line.extend(fields);
        }
        let text = line.to_string();
        eprintln!("radix-connector: {text}");
        if let Err(e) = self.append(&text) {
            eprintln!(
                "radix-connector: could not write the log {}: {e}",
                self.path.display()
            );
        }
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(file, "{line}")
    }

    /// The last `limit` events, oldest first, optionally only those about one interaction.
    pub fn tail(&self, limit: usize, interaction_id: Option<&str>) -> Vec<Value> {
        let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        let text = format!("{}{}", read(&self.path.with_extension("log.1")), read(&self.path));
        let mut events: Vec<Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| match interaction_id {
                Some(id) => event.get("interaction_id").and_then(Value::as_str) == Some(id),
                None => true,
            })
            .collect();
        let skip = events.len().saturating_sub(limit);
        events.drain(..skip);
        events
    }
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Now, as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
pub fn iso_now() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!(
        "{}.{:03}Z",
        iso_secs(now.as_secs()).trim_end_matches('Z'),
        now.subsec_millis()
    )
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC), without a date library.
pub fn iso_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates_are_utc_calendar_dates() {
        assert_eq!(iso_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_secs(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_secs(1_790_000_000), "2026-09-21T14:13:20Z");
    }

    /// The stage decides whether resending is safe: the same timeout means «nothing was sent»
    /// before delivery and «it is on the phone» after.
    #[test]
    fn delivery_decides_whether_a_retry_is_safe() {
        let before = Failure::from_connect(&ConnectError::ChannelTimeout, false);
        assert_eq!((before.code, before.retry_safe), ("WALLET_UNREACHABLE", true));
        let after = Failure::from_connect(&ConnectError::ResponseTimeout, true);
        assert_eq!((after.code, after.retry_safe), ("NO_ANSWER", false));
        let lost = Failure::from_connect(&ConnectError::SignalingClosed, true);
        assert_eq!((lost.code, lost.retry_safe), ("CHANNEL_LOST", false));
        let unconfirmed = Failure::from_connect(&ConnectError::ConfirmationTimeout, false);
        assert_eq!(unconfirmed.code, "NOT_DELIVERED");
    }

    /// Over iroh the same rule holds, with two differences: an unconfirmed request may be on the
    /// phone (never resent), and the timeouts carry the link's diagnostics — nothing secret.
    #[test]
    fn iroh_failures_say_how_far_the_request_got_and_what_the_link_looked_like() {
        use radix_connect_iroh::ConnectionPassword;
        use radixdlt_connect_iroh::mobile::{ConnectionState, Diagnostics, Role};

        let password = ConnectionPassword::from_bytes([0x5a; 32]);
        let diagnostics = Box::new(Diagnostics {
            role: Role::Connector,
            local_id: password.endpoint_id(Role::Connector),
            peer_id: password.endpoint_id(Role::Wallet),
            relay_url: None,
            public_relays: true,
            relay_connected: false,
            relay_error: None,
            state: ConnectionState::Disconnected,
            path: None,
            rtt: None,
            failed_attempts: 0,
            last_error: None,
            connected_for: None,
        });
        let line = diagnostics.to_string();

        let unreachable = Failure::from_mobile(
            &MobileError::WalletUnreachable {
                diagnostics: diagnostics.clone(),
            },
            false,
        );
        assert_eq!(
            (unreachable.code, unreachable.retry_safe),
            ("WALLET_UNREACHABLE", true)
        );
        assert!(unreachable.detail.ends_with(&line), "{}", unreachable.detail);
        assert!(!unreachable.detail.contains(&"5a".repeat(32)));

        let unconfirmed = Failure::from_mobile(
            &MobileError::ConfirmationTimeout {
                diagnostics: diagnostics.clone(),
            },
            false,
        );
        assert_eq!(
            (unconfirmed.code, unconfirmed.retry_safe),
            ("NOT_CONFIRMED", false)
        );
        let no_answer = Failure::from_mobile(&MobileError::ResponseTimeout { diagnostics }, true);
        assert_eq!((no_answer.code, no_answer.retry_safe), ("NO_ANSWER", false));
        assert_eq!(
            Failure::from_mobile(&MobileError::Closed, true).code,
            "CHANNEL_LOST"
        );
        assert_eq!(
            Failure::from_mobile(&MobileError::WalletRejected("rejectedByUser".into()), true).code,
            "REJECTED_BY_USER"
        );
    }

    #[test]
    fn wallet_failures_keep_their_type_and_message() {
        let f = Failure::from_wallet("wrongNetwork: wallet is on network 1");
        assert_eq!(f.code, "WRONG_NETWORK");
        assert!(f.detail.contains("wallet is on network 1"));
        assert_eq!(Failure::from_wallet("rejectedByUser").code, "REJECTED_BY_USER");
        assert_eq!(Failure::from_wallet("somethingNew").code, "WALLET_FAILURE");
        let via_connect = Failure::from_connect(&ConnectError::WalletRejected("invalidRequest".into()), true);
        assert_eq!(via_connect.code, "INVALID_REQUEST");
    }

    #[test]
    fn the_log_is_read_back_filtered_and_bounded() {
        let dir = std::env::temp_dir().join(format!("connector_mcp_log_{}", std::process::id()));
        let log = Log::new(dir.join("connector.log"));
        for i in 0..5 {
            log.event(
                "step",
                json!({ "interaction_id": if i % 2 == 0 { "a" } else { "b" }, "i": i }),
            );
        }
        assert_eq!(log.tail(10, Some("a")).len(), 3);
        let last_two = log.tail(2, None);
        assert_eq!(last_two.len(), 2);
        assert_eq!(last_two[1]["i"], 4);
        std::fs::remove_dir_all(&dir).ok();
    }
}
