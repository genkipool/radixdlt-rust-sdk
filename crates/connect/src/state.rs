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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LinkState {
    #[serde(default = "default_version")]
    pub version: u32,
    pub identity: Identity,
    #[serde(default)]
    pub link: Option<Link>,
}

fn default_version() -> u32 {
    1
}

impl LinkState {
    pub fn load(path: &str) -> Result<Self, ConnectError> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| ConnectError::Protocol(format!("could not read {path}: {e}")))?;
        serde_json::from_str(&data)
            .map_err(|e| ConnectError::Protocol(format!("invalid connector.json: {e}")))
    }

    pub fn password_bytes(&self) -> Result<Vec<u8>, ConnectError> {
        let link = self
            .link
            .as_ref()
            .ok_or_else(|| ConnectError::Protocol("no paired link".into()))?;
        hex::decode(&link.password)
            .map_err(|e| ConnectError::Protocol(format!("invalid password hex: {e}")))
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
