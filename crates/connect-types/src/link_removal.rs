//! Removing a link on one side removes it on the other too.
//!
//! When the person removes a link — the wallet's "Linked Connectors", or a connector forgetting a
//! wallet — that side sends `{"discriminator": "linkRemoved"}` over the link itself, and the other
//! side forgets the link without answering it. The side that removed it keeps a notice (the link's
//! password) until the other side confirmed receiving it, for [`NOTICE_LIFETIME`] at most: the
//! other side may be offline for days, as a phone with the wallet closed usually is.
//!
//! Nothing but the holder of the link's password can send it (the link is encrypted with it), and
//! it only ever removes that one link. A side that does not know it simply ignores it.

use std::time::Duration;

use serde_json::{json, Value};

/// The discriminator of the message.
pub const LINK_REMOVED: &str = "linkRemoved";

/// How long a notice is kept before the link is given up untold: as long as the Radix Wallet and
/// the Connector extension keep theirs.
pub const NOTICE_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The message that tells the other side the link was removed.
#[must_use]
pub fn link_removed_message() -> Value {
    json!({ "discriminator": LINK_REMOVED })
}

/// Whether `message` says that the other side removed the link.
#[must_use]
pub fn is_link_removed(message: &Value) -> bool {
    message.get("discriminator").and_then(Value::as_str) == Some(LINK_REMOVED)
}

/// Whether a notice kept since `retired_at` (seconds since the Unix epoch) has expired at `now`.
#[must_use]
pub fn notice_expired(retired_at: u64, now: u64) -> bool {
    now.saturating_sub(retired_at) >= NOTICE_LIFETIME.as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_is_the_one_the_wallet_and_the_extension_send() {
        assert_eq!(
            serde_json::to_string(&link_removed_message()).unwrap(),
            r#"{"discriminator":"linkRemoved"}"#
        );
        assert!(is_link_removed(&json!({ "discriminator": "linkRemoved" })));
    }

    #[test]
    fn nothing_else_reads_as_a_removal() {
        for message in [
            json!({ "discriminator": "accountList", "accounts": [] }),
            json!({ "discriminator": "success", "interactionId": "linkRemoved" }),
            json!({ "linkRemoved": true }),
            json!("linkRemoved"),
            json!(null),
        ] {
            assert!(!is_link_removed(&message), "{message}");
        }
    }

    #[test]
    fn a_notice_lasts_thirty_days() {
        let lifetime = NOTICE_LIFETIME.as_secs();
        assert!(!notice_expired(100, 100 + lifetime - 1));
        assert!(notice_expired(100, 100 + lifetime));
        // A clock set back never makes a notice expire.
        assert!(!notice_expired(100, 50));
    }
}
