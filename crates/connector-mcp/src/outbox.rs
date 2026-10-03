//! What this connector has sent to the wallet, and what became of it (`requests.json`, next to
//! `connector.json`).
//!
//! The Radix Wallet keeps incoming requests in a queue IN MEMORY and shows them one at a time. A
//! request leaves that queue only when the person approves or rejects it — or when the wallet app
//! is closed, which empties it. Nothing a dApp can send withdraws one. So a request that reached
//! the wallet but never showed blocks every request queued behind it, and sending more only makes
//! the queue longer. This record is how the connector knows something is already waiting on the
//! phone before it sends anything else, and how a late answer to an earlier request is matched
//! to what it answers.
//!
//! The file is shared by every connector process on this machine (an agent's MCP server and a
//! script driving another one), which is also what stops two of them talking to one wallet at once.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::diag::now_secs;

/// How many records are kept.
const KEEP: usize = 200;
/// A request delivered longer ago than this no longer blocks new ones: the wallet app has most
/// likely been closed since. Override with `RADIX_CONNECTOR_PENDING_TTL_SECONDS`.
const DEFAULT_PENDING_TTL: u64 = 15 * 60;
/// A request still `sending` after this long belongs to a process that died mid-request.
const SENDING_TTL: u64 = 16 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Being sent: the wallet has not confirmed receiving it yet.
    Sending,
    /// The wallet confirmed receiving it and nobody has answered: it is in the wallet's queue.
    Delivered,
    /// Sent, but the wallet never confirmed receipt: it MAY be in the wallet's queue.
    Unconfirmed,
    /// The wallet answered (approved or rejected — `outcome` says which).
    Answered,
    /// It never reached the wallet.
    NotDelivered,
    /// Cleared with `cancel_request`. If it had reached the wallet it may still be on the phone.
    Cancelled,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Sending => "sending",
            State::Delivered => "delivered — waiting in the wallet",
            State::Unconfirmed => "unconfirmed — may be waiting in the wallet",
            State::Answered => "answered",
            State::NotDelivered => "not delivered",
            State::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub interaction_id: String,
    /// The tool that sent it.
    pub tool: String,
    /// What was asked: `transaction`, `pre_authorization`, `account_proof`, `login`, …
    pub kind: String,
    pub network: String,
    /// Public key of the paired wallet it went to.
    pub wallet: String,
    /// One line saying what it was, for a person to recognise it on the phone.
    pub summary: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub state: State,
    /// The answer in one line, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The failure code, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub pid: u32,
}

impl Record {
    /// Whether this request may be sitting in the wallet's queue right now, so that sending
    /// another one would stack behind it.
    pub fn is_open(&self, now: u64, ttl: u64) -> bool {
        let age = now.saturating_sub(self.created_at);
        match self.state {
            State::Delivered | State::Unconfirmed => age < ttl,
            State::Sending => age < SENDING_TTL && process_alive(self.pid),
            _ => false,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    requests: Vec<Record>,
    /// When each wallet's last channel closed (unix milliseconds), so that ANOTHER connector
    /// process waits for the wallet to let go of it too (see `radixdlt_connect::WALLET_SETTLE`).
    #[serde(default)]
    links: std::collections::BTreeMap<String, u64>,
}

/// The persisted record of requests.
pub struct Outbox {
    path: PathBuf,
}

impl Outbox {
    pub fn new(path: PathBuf) -> Self {
        Outbox { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The age after which a delivered request stops blocking.
    pub fn pending_ttl() -> u64 {
        std::env::var("RADIX_CONNECTOR_PENDING_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PENDING_TTL)
    }

    /// Every record, oldest first. An unreadable file reads as empty: losing this history must
    /// never stop the connector from signing.
    pub fn all(&self) -> Vec<Record> {
        self.read().requests
    }

    fn read(&self) -> File {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<File>(&text).ok())
            .unwrap_or_default()
    }

    /// When `wallet`'s last channel closed, in unix milliseconds — by any connector process.
    pub fn link_closed_at(&self, wallet: &str) -> Option<u64> {
        self.read().links.get(wallet).copied()
    }

    /// Records that a channel to `wallet` just closed.
    pub fn mark_link_closed(&self, wallet: &str) -> Result<(), String> {
        let wallet = wallet.to_string();
        self.modify_file(|file| {
            file.links.insert(wallet, now_millis());
        })
    }

    pub fn get(&self, interaction_id: &str) -> Option<Record> {
        self.all()
            .into_iter()
            .find(|r| r.interaction_id == interaction_id)
    }

    /// Requests that may be waiting in `wallet`'s queue (every wallet with `None`), newest first.
    pub fn open(&self, wallet: Option<&str>) -> Vec<Record> {
        let (now, ttl) = (now_secs(), Outbox::pending_ttl());
        let mut open: Vec<Record> = self
            .all()
            .into_iter()
            .filter(|r| wallet.is_none_or(|w| r.wallet == w) && r.is_open(now, ttl))
            .collect();
        open.reverse();
        open
    }

    pub fn insert(&self, record: Record) -> Result<(), String> {
        self.modify(|records| {
            records.retain(|r| r.interaction_id != record.interaction_id);
            records.push(record);
        })
    }

    /// Moves a request to `state`, with an outcome/code when given. Returns whether it existed.
    pub fn set(
        &self,
        interaction_id: &str,
        state: State,
        outcome: Option<String>,
        code: Option<&str>,
    ) -> Result<bool, String> {
        let mut found = false;
        self.modify(|records| {
            if let Some(r) = records.iter_mut().find(|r| r.interaction_id == interaction_id) {
                found = true;
                r.state = state;
                r.updated_at = now_secs();
                if outcome.is_some() {
                    r.outcome = outcome;
                }
                if let Some(code) = code {
                    r.code = Some(code.to_string());
                }
            }
        })?;
        Ok(found)
    }

    fn modify(&self, change: impl FnOnce(&mut Vec<Record>)) -> Result<(), String> {
        self.modify_file(|file| change(&mut file.requests))
    }

    fn modify_file(&self, change: impl FnOnce(&mut File)) -> Result<(), String> {
        let mut file = self.read();
        change(&mut file);
        let skip = file.requests.len().saturating_sub(KEEP);
        file.requests.drain(..skip);
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        // Written beside and renamed over, so a reader in another process never sees half a file.
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        restrict(&tmp);
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

/// Unix milliseconds now.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Owner-only, like `connector.json`: the summaries say what the person signs.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Whether a process is still running. Where that cannot be told cheaply, assume it is: the
/// `SENDING_TTL` bounds how long a dead one can block.
fn process_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, state: State, age: u64) -> Record {
        let now = now_secs();
        Record {
            interaction_id: id.into(),
            tool: "send_transaction".into(),
            kind: "transaction".into(),
            network: "stokenet".into(),
            wallet: "pk".into(),
            summary: "CALL_METHOD".into(),
            created_at: now - age,
            updated_at: now - age,
            state,
            outcome: None,
            code: None,
            pid: std::process::id(),
        }
    }

    /// Only what may be on the phone blocks: delivered (or unconfirmed) and recent.
    #[test]
    fn only_recent_undelivered_answers_block() {
        let ttl = 900;
        let now = now_secs();
        assert!(record("a", State::Delivered, 10).is_open(now, ttl));
        assert!(record("b", State::Unconfirmed, 10).is_open(now, ttl));
        assert!(record("c", State::Sending, 10).is_open(now, ttl));
        assert!(!record("d", State::Delivered, 2_000).is_open(now, ttl));
        for state in [State::Answered, State::NotDelivered, State::Cancelled] {
            assert!(!record("e", state, 10).is_open(now, ttl));
        }
        let mut dead = record("f", State::Sending, 10);
        dead.pid = u32::MAX;
        if cfg!(target_os = "linux") {
            assert!(!dead.is_open(now, ttl), "a dead process's request must not block");
        }
    }

    #[test]
    fn records_round_trip_and_change_state() {
        let dir = std::env::temp_dir().join(format!("connector_mcp_outbox_{}", std::process::id()));
        let outbox = Outbox::new(dir.join("requests.json"));
        outbox.insert(record("x", State::Sending, 0)).unwrap();
        outbox.insert(record("y", State::Delivered, 0)).unwrap();
        assert_eq!(outbox.open(Some("pk")).len(), 2);
        assert_eq!(outbox.open(Some("other")).len(), 0);
        assert!(outbox
            .set("y", State::Answered, Some("txid_1".into()), None)
            .unwrap());
        assert_eq!(outbox.get("y").unwrap().outcome.as_deref(), Some("txid_1"));
        assert!(!outbox.set("missing", State::Answered, None, None).unwrap());
        assert_eq!(outbox.open(None).len(), 1);
        assert_eq!(outbox.link_closed_at("pk"), None);
        outbox.mark_link_closed("pk").unwrap();
        assert!(outbox.link_closed_at("pk").is_some_and(|at| at <= now_millis()));
        assert_eq!(outbox.all().len(), 2, "marking a link keeps the requests");
        std::fs::remove_dir_all(&dir).ok();
    }
}
