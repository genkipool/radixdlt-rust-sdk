//! Persistent state of the wallet link, compatible with the `connector.json` file
//! written by the Radix Connect connector, so an existing pairing can be reused
//! without re-pairing.

use radixdlt_connect_types::link_removal::notice_expired;
pub use radixdlt_connect_types::Transport;
use serde::{Deserialize, Serialize};

use crate::error::ConnectError;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Identity {
    #[serde(rename = "privateKey")]
    pub private_key: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Link {
    pub password: String,
    #[serde(rename = "walletPublicKey")]
    pub wallet_public_key: String,
    #[serde(rename = "linkedAt", default)]
    pub linked_at: String,
    /// Optional human-readable label shown when listing links (e.g. "alice's phone").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The transport the link was paired over, if known: a link paired over iroh has a wallet that
    /// speaks iroh. Links paired before it was recorded have none, which means WebRTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// The custom iroh relay the link was paired on, if not iroh's public ones.
    #[serde(rename = "irohRelay", default, skip_serializing_if = "Option::is_none")]
    pub iroh_relay: Option<String>,
}

impl Link {
    /// The raw bytes of its password, to reach its wallet with.
    pub fn password_bytes(&self) -> Result<Vec<u8>, ConnectError> {
        hex::decode(&self.password).map_err(|e| ConnectError::Protocol(format!("invalid password hex: {e}")))
    }

    /// The transport it was paired over: WebRTC when it was not recorded.
    #[must_use]
    pub fn paired_over(&self) -> Transport {
        self.transport.unwrap_or(Transport::Webrtc)
    }
}

/// A link removed here whose wallet was not told yet: its password is kept to tell it (see
/// [`crate::link_removal`]), and nothing else is done with it. It is not a paired link: it is in
/// `retired_links`, never in `links`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RetiredLink {
    /// The link's password (hex), as it was paired: the only way to reach that wallet.
    pub password: String,
    /// The public key of the wallet it was paired to.
    #[serde(rename = "walletPublicKey")]
    pub wallet_public_key: String,
    /// The link's label, if it had one, to tell the person which phone it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The transport the link was paired over, to tell its wallet over it (see [`Link::transport`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// The custom iroh relay the link was paired on, if any.
    #[serde(rename = "irohRelay", default, skip_serializing_if = "Option::is_none")]
    pub iroh_relay: Option<String>,
    /// When it was removed, in seconds since the Unix epoch.
    #[serde(rename = "retiredAt")]
    pub retired_at: u64,
}

impl RetiredLink {
    /// The raw bytes of its password, to open the link with.
    pub fn password_bytes(&self) -> Result<Vec<u8>, ConnectError> {
        hex::decode(&self.password).map_err(|e| ConnectError::Protocol(format!("invalid password hex: {e}")))
    }

    /// Whether the notice is too old to keep: the link is then given up untold.
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        notice_expired(self.retired_at, now)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LinkState {
    #[serde(default = "default_version")]
    pub version: u32,
    pub identity: Identity,
    /// Legacy single-link field. Kept for backward compatibility with older
    /// `connector.json` files; on load it is migrated into `links` (see `load`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
    /// All paired wallets/desktops. Each link has its own password, hence its own
    /// `connectionId`, so the daemon can dispatch a challenge to one specific device.
    #[serde(default)]
    pub links: Vec<Link>,
    /// Links removed here whose wallets still have to be told. Older files have none, and older
    /// versions ignore the field.
    #[serde(rename = "retiredLinks", default, skip_serializing_if = "Vec::is_empty")]
    pub retired_links: Vec<RetiredLink>,
}

fn default_version() -> u32 {
    1
}

impl LinkState {
    pub fn load(path: &str) -> Result<Self, ConnectError> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| ConnectError::Protocol(format!("could not read {path}: {e}")))?;
        let mut state: Self = serde_json::from_str(&data)
            .map_err(|e| ConnectError::Protocol(format!("invalid connector.json: {e}")))?;
        // Migrate a legacy single `link` into the `links` vector so the rest of the
        // code only deals with `links`. After a save, the file carries only `links`
        // (the legacy `link` field is dropped).
        if let Some(legacy) = state.link.take() {
            if !state
                .links
                .iter()
                .any(|l| l.wallet_public_key == legacy.wallet_public_key)
            {
                state.links.insert(0, legacy);
            }
        }
        Ok(state)
    }

    /// All paired links (after migration of the legacy single link).
    pub fn all_links(&self) -> &[Link] {
        &self.links
    }

    /// Raw password bytes of the first paired link (backward-compatible helper used by
    /// the single-device standalone flow).
    pub fn password_bytes(&self) -> Result<Vec<u8>, ConnectError> {
        let link = self
            .links
            .first()
            .ok_or_else(|| ConnectError::Protocol("no paired link".into()))?;
        hex::decode(&link.password).map_err(|e| ConnectError::Protocol(format!("invalid password hex: {e}")))
    }

    /// Raw password bytes of the link paired to a specific wallet public key, so the
    /// caller can open a connector channel that reaches only that one device.
    pub fn password_bytes_for(&self, wallet_public_key: &str) -> Result<Vec<u8>, ConnectError> {
        let link = self
            .links
            .iter()
            .find(|l| l.wallet_public_key == wallet_public_key)
            .ok_or_else(|| ConnectError::Protocol(format!("no link for {wallet_public_key}")))?;
        hex::decode(&link.password).map_err(|e| ConnectError::Protocol(format!("invalid password hex: {e}")))
    }

    /// Adds a link, replacing any existing one with the same wallet public key (so
    /// re-pairing the same device refreshes it instead of duplicating). Pairing a new
    /// device appends without touching the others.
    pub fn add_or_replace_link(&mut self, link: Link) {
        self.link = None;
        if let Some(existing) = self
            .links
            .iter_mut()
            .find(|l| l.wallet_public_key == link.wallet_public_key)
        {
            *existing = link;
        } else {
            self.links.push(link);
        }
    }

    /// Removes the link paired to `wallet_public_key`. Returns true if one was removed.
    pub fn remove_link(&mut self, wallet_public_key: &str) -> bool {
        let before = self.links.len();
        self.links.retain(|l| l.wallet_public_key != wallet_public_key);
        self.link = None;
        self.links.len() != before
    }

    /// Removes the link paired to `wallet_public_key` and keeps a notice for its wallet, removed
    /// at `now` (seconds since the Unix epoch). Returns the notice, or `None` when no link is paired
    /// to that key. Pairing the same wallet again later is a new link, with a new password.
    pub fn retire_link(&mut self, wallet_public_key: &str, now: u64) -> Option<RetiredLink> {
        let link = self
            .links
            .iter()
            .find(|l| l.wallet_public_key == wallet_public_key)?
            .clone();
        self.remove_link(wallet_public_key);
        let retired = RetiredLink {
            password: link.password,
            wallet_public_key: link.wallet_public_key,
            label: link.label,
            transport: link.transport,
            iroh_relay: link.iroh_relay,
            retired_at: now,
        };
        self.retired_links.retain(|r| r.password != retired.password);
        self.retired_links.push(retired.clone());
        Some(retired)
    }

    /// Forgets the notice of the link with this password: its wallet was told, or removed the link
    /// too. Returns true if there was one.
    pub fn forget_retired(&mut self, password: &str) -> bool {
        let before = self.retired_links.len();
        self.retired_links.retain(|r| r.password != password);
        self.retired_links.len() != before
    }

    /// Forgets the notices that expired at `now`. Returns how many.
    pub fn prune_retired(&mut self, now: u64) -> usize {
        let before = self.retired_links.len();
        self.retired_links.retain(|r| !r.expired(now));
        before - self.retired_links.len()
    }

    /// Removes the link with this password because ITS WALLET removed it: no notice is kept, the
    /// wallet already knows. Returns the link removed, if one was paired with that password.
    pub fn remove_link_removed_by_wallet(&mut self, password: &[u8]) -> Option<Link> {
        let hex_password = hex::encode(password);
        let link = self
            .links
            .iter()
            .find(|l| l.password.eq_ignore_ascii_case(&hex_password))?
            .clone();
        self.remove_link(&link.wallet_public_key);
        Some(link)
    }

    pub fn save(&self, path: &str) -> Result<(), ConnectError> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| ConnectError::Protocol(format!("serialization: {e}")))?;
        std::fs::write(path, json + "\n")
            .map_err(|e| ConnectError::Protocol(format!("could not write {path}: {e}")))?;
        set_mode_600(path);
        Ok(())
    }
}

#[cfg(unix)]
fn set_mode_600(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_mode_600(_path: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(pk: &str, password: &str) -> Link {
        Link {
            password: password.into(),
            wallet_public_key: pk.into(),
            linked_at: String::new(),
            label: Some(format!("{pk}'s phone")),
            transport: Some(Transport::Iroh),
            iroh_relay: None,
        }
    }

    fn state() -> LinkState {
        LinkState {
            version: 1,
            identity: Identity {
                private_key: "00".into(),
                public_key: "11".into(),
            },
            link: None,
            links: vec![link("aa", "a1a1"), link("bb", "b2b2")],
            retired_links: Vec::new(),
        }
    }

    #[test]
    fn retiring_a_link_unpairs_it_and_keeps_one_notice_for_its_wallet() {
        let mut state = state();

        let retired = state.retire_link("aa", 100).unwrap();

        assert_eq!(retired.password, "a1a1");
        assert_eq!(retired.label.as_deref(), Some("aa's phone"));
        assert_eq!(
            retired.transport,
            Some(Transport::Iroh),
            "told over the transport it was paired over"
        );
        assert_eq!(state.all_links().len(), 1);
        assert_eq!(state.retired_links, vec![retired.clone()]);
        assert_eq!(retired.password_bytes().unwrap(), vec![0xa1, 0xa1]);
        assert!(state.retire_link("aa", 200).is_none(), "no longer paired");

        assert!(state.forget_retired("a1a1"));
        assert!(state.retired_links.is_empty());
    }

    #[test]
    fn expired_notices_are_dropped() {
        let mut state = state();
        state.retire_link("aa", 0);
        state.retire_link("bb", NOTICE_LIFETIME_SECS);

        assert_eq!(state.prune_retired(NOTICE_LIFETIME_SECS), 1);
        assert_eq!(state.retired_links.len(), 1);
        assert_eq!(state.retired_links[0].wallet_public_key, "bb");
    }

    #[test]
    fn a_removal_by_the_wallet_unpairs_without_a_notice() {
        let mut state = state();

        let removed = state.remove_link_removed_by_wallet(&[0xb2, 0xb2]).unwrap();

        assert_eq!(removed.wallet_public_key, "bb");
        assert_eq!(state.all_links().len(), 1);
        assert!(state.retired_links.is_empty());
        assert!(state.remove_link_removed_by_wallet(&[0xff]).is_none());
    }

    #[test]
    fn a_file_without_notices_loads_and_is_saved_without_them() {
        let json = r#"{"version":1,"identity":{"privateKey":"00","publicKey":"11"},"links":[]}"#;
        let state: LinkState = serde_json::from_str(json).unwrap();
        assert!(state.retired_links.is_empty());
        assert!(!serde_json::to_string(&state).unwrap().contains("retiredLinks"));
    }

    const NOTICE_LIFETIME_SECS: u64 = radixdlt_connect_types::link_removal::NOTICE_LIFETIME.as_secs();
}
