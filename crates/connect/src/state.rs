//! Persistent state of the wallet link, compatible with the `connector.json` file
//! written by the Radix Connect connector, so an existing pairing can be reused
//! without re-pairing.

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
        hex::decode(&link.password)
            .map_err(|e| ConnectError::Protocol(format!("invalid password hex: {e}")))
    }

    /// Raw password bytes of the link paired to a specific wallet public key, so the
    /// caller can open a connector channel that reaches only that one device.
    pub fn password_bytes_for(&self, wallet_public_key: &str) -> Result<Vec<u8>, ConnectError> {
        let link = self
            .links
            .iter()
            .find(|l| l.wallet_public_key == wallet_public_key)
            .ok_or_else(|| ConnectError::Protocol(format!("no link for {wallet_public_key}")))?;
        hex::decode(&link.password)
            .map_err(|e| ConnectError::Protocol(format!("invalid password hex: {e}")))
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
        self.links
            .retain(|l| l.wallet_public_key != wallet_public_key);
        self.link = None;
        self.links.len() != before
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
