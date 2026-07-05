//! On-disk state: the connector identity and the paired wallets, kept in a
//! `connector.json` under the OS config directory. Reuses [`LinkState`] from
//! `radixdlt-connect` (same file format the browser/Node connector writes, with
//! `0600` permissions on Unix).
//!
//! Cross-platform config location (via the `dirs` crate):
//!   * Linux:   `~/.config/radix-connector/connector.json`
//!   * macOS:   `~/Library/Application Support/radix-connector/connector.json`
//!   * Windows: `%APPDATA%\radix-connector\connector.json`
//!
//! `RADIX_CONNECTOR_HOME` overrides the directory (handy for tests / multiple
//! profiles).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use radixdlt_connect::state::{Identity, LinkState};
use rand_core::OsRng;

/// Namespace for connector-state helpers (no instance state of its own).
pub struct Store;

impl Store {
    /// Resolves the `connector.json` path, honouring `RADIX_CONNECTOR_HOME`.
    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(dir) = std::env::var("RADIX_CONNECTOR_HOME") {
            if !dir.trim().is_empty() {
                return Ok(PathBuf::from(dir).join("connector.json"));
            }
        }
        let base = dirs::config_dir()
            .or_else(dirs::home_dir)
            .ok_or_else(|| "could not determine a config directory for this OS".to_string())?;
        Ok(base.join("radix-connector").join("connector.json"))
    }

    /// Loads the state, creating a fresh one (with a new connector identity) on
    /// first run and persisting it before returning.
    pub fn load_or_init(path: &Path) -> Result<LinkState, String> {
        let path_str = path_str(path);
        if path.exists() {
            return LinkState::load(&path_str).map_err(|e| e.to_string());
        }
        let state = LinkState {
            version: 1,
            identity: new_identity(),
            link: None,
            links: Vec::new(),
        };
        state.save(&path_str).map_err(|e| e.to_string())?;
        Ok(state)
    }

    /// Loads existing state, erroring if it has not been initialised yet.
    pub fn load(path: &Path) -> Result<LinkState, String> {
        LinkState::load(&path_str(path)).map_err(|e| e.to_string())
    }

    /// Persists state to disk (`0600` on Unix).
    pub fn save(path: &Path, state: &LinkState) -> Result<(), String> {
        state.save(&path_str(path)).map_err(|e| e.to_string())
    }
}

/// String form of a path for the `LinkState` API (which takes `&str`).
pub fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Current time as Unix seconds, for the `linkedAt` field.
pub fn now_unix_seconds() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

/// Generates a brand-new Ed25519 connector identity (hex-encoded).
fn new_identity() -> Identity {
    let signing = SigningKey::generate(&mut OsRng);
    Identity {
        private_key: hex::encode(signing.to_bytes()),
        public_key: hex::encode(signing.verifying_key().to_bytes()),
    }
}
