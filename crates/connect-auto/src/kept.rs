//! The links a program keeps in its state file, looked after by the connector.
//!
//! Given the file ([`WalletConnector::with_state`]), the connector keeps it true without the
//! program doing anything: a link its wallet removed is forgotten, the relay a wallet is on is
//! noted for the next session's knock, a link removed here is told to its wallet — at once when
//! the wallet is reachable, else by [`WalletConnector::deliver_removal_notices`] whenever it
//! next is (which [`WalletConnector::run_until_stopped`] runs) — and a notice nobody could
//! deliver for 30 days is given up.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use radixdlt_connect_types::Transport;
use radixdlt_connect_webrtc::state::{Link, LinkState, RetiredLink};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::{Error, WalletConnector};

const TARGET: &str = "radixdlt_connect_auto";
/// How long one attempt listens for a removed link's wallet: long enough for a person to open the
/// app, short enough that a notice given up meanwhile is noticed.
const ATTEMPT: Duration = Duration::from_secs(5 * 60);
/// How often the pending notices are read again, to pick up the new ones.
const RESCAN: Duration = Duration::from_secs(20);

/// What became of a link removed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Removal {
    /// Its wallet was told, and confirmed it: nothing is left of the link.
    Told,
    /// Its wallet was not reachable: the notice is kept, and delivered when it is.
    Pending,
}

/// What one round of [`WalletConnector::deliver_pending_notices`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoticesDelivered {
    /// Wallets told, and their notices forgotten.
    pub told: usize,
    /// Notices still waiting for their wallet.
    pub pending: usize,
    /// Notices given up after 30 days.
    pub expired: usize,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn path_text(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| Error::State(format!("{} is not valid UTF-8", path.display())))
}

/// Loads, changes and saves the state file in one go; `change` says whether it changed.
fn update(path: &Path, change: impl FnOnce(&mut LinkState) -> bool) -> Result<bool, Error> {
    let text = path_text(path)?;
    let mut state = LinkState::load(text).map_err(|e| Error::State(e.to_string()))?;
    if !change(&mut state) {
        return Ok(false);
    }
    state.save(text).map_err(|e| Error::State(e.to_string()))?;
    Ok(true)
}

impl WalletConnector {
    /// Looks after the links of the program's state file at `path` (its `connector.json`): what a
    /// request finds out about a link is kept there, and a link removed here
    /// ([`remove_link`](Self::remove_link)) is told to its wallet. See the module.
    #[must_use]
    pub fn with_state(mut self, path: impl Into<PathBuf>) -> Self {
        self.state = Some(Arc::new(path.into()));
        self
    }

    /// The state file this connector looks after, if any.
    #[must_use]
    pub fn state_path(&self) -> Option<&Path> {
        self.state.as_deref().map(PathBuf::as_path)
    }

    /// What an answer over `link` says about it, kept: the wallet removed it (forgotten here, its
    /// session closed), or the relay its wallet is on now (noted for the next knock).
    pub(crate) async fn after_exchange(&self, link: &Link, result: &Result<Value, Error>) {
        let Some(path) = self.state_path() else {
            return;
        };
        match result {
            Err(Error::LinkRemoved) => {
                let Ok(password) = link.password_bytes() else {
                    return;
                };
                let forgotten = update(path, |state| {
                    state.remove_link_removed_by_wallet(&password).is_some()
                });
                tracing::info!(target: TARGET, forgotten = matches!(forgotten, Ok(true)), "the wallet removed a link: forgotten here");
                self.close_link(link).await;
            }
            Ok(_) if link.paired_over() == Transport::Iroh => {
                if let Some(relay) = self.wallet_relay(link).await {
                    let _ = update(path, |state| state.note_wallet_relay(&link.password, &relay));
                }
            }
            _ => {}
        }
    }

    /// Removes the link of the wallet `wallet_public_key` from the state file and tells the
    /// wallet, waiting `wait` at most for it: a wallet whose app is open gets it in a second or
    /// two. One not reachable gets it when it is ([`deliver_removal_notices`](Self::deliver_removal_notices)).
    ///
    /// # Errors
    /// [`Error::State`] without a state file or when it cannot be read or written, and
    /// [`Error::InvalidLink`] when no link has that wallet.
    pub async fn remove_link(&self, wallet_public_key: &str, wait: Duration) -> Result<Removal, Error> {
        let path = self
            .state_path()
            .ok_or_else(|| Error::State("no state file: call with_state".to_string()))?;
        let mut retired = None;
        update(path, |state| {
            retired = state.retire_link(wallet_public_key, now());
            retired.is_some()
        })?;
        let notice = retired
            .ok_or_else(|| Error::InvalidLink(format!("no link with the wallet {wallet_public_key}")))?;
        Ok(self.tell(path, &notice, wait).await)
    }

    /// One attempt to tell a notice's wallet, for `wait` at most: told, the notice is forgotten.
    async fn tell(&self, path: &Path, notice: &RetiredLink, wait: Duration) -> Removal {
        match self.notify_link_removed(notice, wait).await {
            Ok(()) => {
                let _ = update(path, |state| state.forget_retired(&notice.password));
                // Nothing more goes over it: its iroh session, if one ran, is let go of.
                if let (Ok(password), Ok(iroh)) = (
                    notice.password_bytes(),
                    self.iroh_on_relay(notice.iroh_relay.as_deref()),
                ) {
                    iroh.close_link(&password).await;
                }
                tracing::info!(target: TARGET, "a removed link was told to its wallet");
                Removal::Told
            }
            Err(error) => {
                tracing::debug!(target: TARGET, error = %error, "a removed link's wallet was not told yet: the notice is kept");
                Removal::Pending
            }
        }
    }

    /// One round: tells the wallets of the state file's pending notices, all at once and each for
    /// `wait` at most, forgets the told ones and gives up the expired ones. For a program that
    /// runs now and then (a daemon's timer); one that keeps running uses
    /// [`deliver_removal_notices`](Self::deliver_removal_notices).
    ///
    /// # Errors
    /// [`Error::State`] without a state file or when it cannot be read or written.
    pub async fn deliver_pending_notices(&self, wait: Duration) -> Result<NoticesDelivered, Error> {
        let path = self
            .state
            .clone()
            .ok_or_else(|| Error::State("no state file: call with_state".to_string()))?;
        let mut expired = 0;
        update(&path, |state| {
            expired = state.prune_retired(now());
            expired > 0
        })?;
        let notices = LinkState::load(path_text(&path)?)
            .map_err(|e| Error::State(e.to_string()))?
            .retired_links;
        let mut attempts = JoinSet::new();
        for notice in notices {
            let (connector, path) = (self.clone(), path.clone());
            attempts.spawn(async move { connector.tell(&path, &notice, wait).await });
        }
        let mut delivered = NoticesDelivered {
            expired,
            ..NoticesDelivered::default()
        };
        while let Some(done) = attempts.join_next().await {
            match done {
                Ok(Removal::Told) => delivered.told += 1,
                _ => delivered.pending += 1,
            }
        }
        Ok(delivered)
    }

    /// Notes, for each iroh link of the state file whose wallet is connected now, the relay it is
    /// on (for the next session's knock), even when no request went over it — a program that
    /// keeps its links listening calls it now and then. Requests note it by themselves.
    pub async fn remember_wallet_relays(&self) {
        let Some(path) = self.state_path() else {
            return;
        };
        let Some(links) = path_text(path)
            .ok()
            .and_then(|text| LinkState::load(text).ok())
            .map(|s| s.links)
        else {
            return;
        };
        let mut seen = Vec::new();
        for link in links.iter().filter(|link| link.paired_over() == Transport::Iroh) {
            if let Some(relay) = self.wallet_relay(link).await {
                seen.push((link.password.clone(), relay));
            }
        }
        if !seen.is_empty() {
            let _ = update(path, |state| {
                seen.iter().fold(false, |changed, (password, relay)| {
                    state.note_wallet_relay(password, relay) | changed
                })
            });
        }
    }

    /// Delivers the state file's pending notices for as long as it runs: each one until its
    /// wallet confirmed it, the expired ones given up. Never ends; [`run_until_stopped`] runs it
    /// beside the program's own work. Nothing to do without a state file.
    ///
    /// [`run_until_stopped`]: Self::run_until_stopped
    pub async fn deliver_removal_notices(&self) {
        let Some(path) = self.state.clone() else {
            return std::future::pending().await;
        };
        let mut attempts: JoinSet<String> = JoinSet::new();
        let mut running: Vec<String> = Vec::new();
        loop {
            for notice in pending(&path) {
                if running.contains(&notice.password) {
                    continue;
                }
                running.push(notice.password.clone());
                let connector = self.clone();
                let path = path.clone();
                attempts.spawn(async move {
                    connector.tell(&path, &notice, ATTEMPT).await;
                    notice.password
                });
            }
            tokio::select! {
                Some(Ok(done)) = attempts.join_next(), if !attempts.is_empty() => {
                    running.retain(|password| *password != done);
                }
                () = tokio::time::sleep(RESCAN) => {}
            }
        }
    }
}

/// The notices still to deliver, after giving up the expired ones.
fn pending(path: &Path) -> Vec<RetiredLink> {
    let now = now();
    let _ = update(path, |state| state.prune_retired(now) > 0);
    path_text(path)
        .ok()
        .and_then(|text| LinkState::load(text).ok())
        .map(|state| state.retired_links)
        .unwrap_or_default()
}
