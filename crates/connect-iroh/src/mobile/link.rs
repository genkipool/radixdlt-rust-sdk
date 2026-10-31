//! One link's iroh session: started on demand, read by exactly one task, shared by every request
//! on the link.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use radix_connect_iroh::{
    ConnectionPassword, ConnectionState, Diagnostics, Role, SendError, Session, SessionConfig, SessionEvent,
};
use radixdlt_connect_types::link_removal::is_link_removed;
use radixdlt_connect_types::linking;
use serde_json::Value;
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::inbox::{self, Inbox, Refused, Registration};
use super::{log, MobileError};

/// Messages no request claimed, buffered per subscriber. One that falls further behind skips
/// the oldest.
const SUBSCRIBER_BUFFER: usize = 64;

/// A link, named by a hash of its password ([`linking::connection_id`]) — never the password.
///
/// It is the same value the wallet names the link by and that WebRTC sends to its signaling
/// server: public, and enough to tell links apart in a log.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LinkId([u8; 32]);

impl LinkId {
    pub(super) fn of(password: &[u8]) -> Self {
        LinkId(linking::connection_id(password))
    }

    /// The name of its lock file: half the hash, plenty to tell a machine's links apart.
    pub(super) fn file_name(&self) -> String {
        hex::encode(self.0.get(..16).unwrap_or_default())
    }
}

impl fmt::Display for LinkId {
    /// The first five bytes, like iroh's short endpoint ids.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0.get(..5).unwrap_or_default()))
    }
}

impl fmt::Debug for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkId({self})")
    }
}

/// Where the link stands, as its reader saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    /// The session listens at its relay; the wallet is not connected.
    Waiting,
    /// The wallet is connected.
    Connected,
    /// The session is closed: nothing will arrive any more.
    Ended,
}

/// A message no request claimed, as the link's subscribers receive it.
#[derive(Debug, Clone)]
pub(super) struct Incoming {
    pub(super) message: Value,
    /// How many times the wallet's connection state had changed when the message arrived
    /// ([`Link::state_changes`]): the connection it came on is gone once that number moves on.
    pub(super) state_changes: u64,
}

/// What the link's reader shares with the requests.
#[derive(Debug)]
struct Shared {
    id: LinkId,
    /// When the session started: the latency to the wallet's first connection is measured from it.
    started: Instant,
    inbox: Mutex<Inbox>,
    status: watch::Sender<Status>,
    state_changes: AtomicU64,
    unclaimed: broadcast::Sender<Incoming>,
    seen_wallet: AtomicBool,
    /// The wallet said it removed the link (`linkRemoved`): nothing asked over it is answered.
    removed_by_wallet: AtomicBool,
}

impl Shared {
    fn inbox(&self) -> MutexGuard<'_, Inbox> {
        // A poisoned lock only means another thread panicked while holding it; the inbox's
        // invariants hold between its methods, so carry on rather than spread the panic.
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn on_state(&self, state: ConnectionState, session: &Session) {
        let connected = state == ConnectionState::Connected;
        let first = connected && !self.seen_wallet.swap(true, Ordering::Relaxed);
        // Before the status is published, so whoever wakes on it reads the new count.
        self.state_changes.fetch_add(1, Ordering::SeqCst);
        self.status.send_replace(if connected {
            Status::Connected
        } else {
            Status::Waiting
        });
        log::state_changed(self.id, connected, first, self.started, &session.diagnostics());
    }

    fn on_message(&self, text: &str) {
        let message: Value = match serde_json::from_str(text) {
            Ok(message @ Value::Object(_)) => message,
            _ => return log::not_json(self.id, text.len()),
        };
        log::message(
            self.id,
            text.len(),
            inbox::interaction_id(&message),
            message.get("discriminator").and_then(Value::as_str),
        );
        if is_link_removed(&message) && !self.removed_by_wallet.swap(true, Ordering::SeqCst) {
            log::removed_by_wallet(self.id);
        }
        let (unclaimed, kept) = self.inbox().deliver(message, text.len(), Instant::now());
        if let Some(message) = unclaimed {
            if let (Some(id), Some(kept)) = (inbox::interaction_id(&message), kept) {
                log::kept_unclaimed(self.id, id, kept.label());
            }
            // No subscriber is not an error: most messages (the account list) interest nobody.
            let _ = self.unclaimed.send(Incoming {
                message,
                state_changes: self.state_changes.load(Ordering::SeqCst),
            });
        }
    }

    fn end(&self) {
        self.inbox().close();
        self.status.send_replace(Status::Ended);
        log::session_ended(self.id, self.started, self.state_changes.load(Ordering::SeqCst));
    }
}

/// The one task that takes the session's events, for the session's whole life.
///
/// Taking a message confirms it to the wallet, so this loop never waits on anybody: answers go
/// to their waiters (or are kept), everything else to the subscribers.
async fn read_events(session: Arc<Session>, shared: Arc<Shared>) {
    while let Some(event) = session.next_event().await {
        match event {
            SessionEvent::StateChanged(state) => shared.on_state(state, &session),
            SessionEvent::Message(text) => shared.on_message(&text),
        }
    }
    shared.end();
}

/// A started link: its session and the task reading it.
#[derive(Debug)]
pub(super) struct Link {
    session: Arc<Session>,
    shared: Arc<Shared>,
    reader: JoinHandle<()>,
    /// This process's hold on the link (`lock`), shared with the registry: let go once the link is
    /// closed there and every session that used it has ended.
    _lock: Option<Arc<super::lock::LinkLock>>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // The reader holds the session; stopping it lets the session (and its endpoint) go.
        self.reader.abort();
    }
}

impl Link {
    async fn start(
        id: LinkId,
        password: &ConnectionPassword,
        config: SessionConfig,
        lock: Option<Arc<super::lock::LinkLock>>,
    ) -> Result<Self, MobileError> {
        let relays = config.relays().clone();
        let started = Instant::now();
        let session = Arc::new(
            Session::start(password, Role::Connector, config)
                .await
                .map_err(|error| {
                    log::session_failed(id, &relays, &error);
                    MobileError::from(error)
                })?,
        );
        let (unclaimed, _) = broadcast::channel(SUBSCRIBER_BUFFER);
        let shared = Arc::new(Shared {
            id,
            started,
            inbox: Mutex::new(Inbox::default()),
            status: watch::Sender::new(Status::Waiting),
            state_changes: AtomicU64::new(0),
            unclaimed,
            seen_wallet: AtomicBool::new(false),
            removed_by_wallet: AtomicBool::new(false),
        });
        log::session_started(id, &relays, &session.diagnostics(), log::ms(started.elapsed()));
        let reader = tokio::spawn(read_events(session.clone(), shared.clone()));
        Ok(Link {
            session,
            shared,
            reader,
            _lock: lock,
        })
    }

    pub(super) fn id(&self) -> LinkId {
        self.shared.id
    }

    pub(super) fn is_running(&self) -> bool {
        *self.shared.status.borrow() != Status::Ended
    }

    /// Whether the wallet is connected right now (the session's own state, not the reader's).
    pub(super) fn is_connected(&self) -> bool {
        self.session.state() == ConnectionState::Connected
    }

    /// Whether the wallet has connected at least once since the session started.
    pub(super) fn seen_wallet(&self) -> bool {
        self.shared.seen_wallet.load(Ordering::Relaxed)
    }

    /// Whether the wallet said it removed the link.
    pub(super) fn removed_by_wallet(&self) -> bool {
        self.shared.removed_by_wallet.load(Ordering::SeqCst)
    }

    /// The relay the wallet is on, as this end knows it.
    pub(super) async fn peer_relay(&self) -> Option<iroh::RelayUrl> {
        self.session.peer_relay().await
    }

    pub(super) fn diagnostics(&self) -> Diagnostics {
        self.session.diagnostics()
    }

    pub(super) fn status(&self) -> watch::Receiver<Status> {
        self.shared.status.subscribe()
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<Incoming> {
        self.shared.unclaimed.subscribe()
    }

    /// How many times the wallet's connection state has changed since the session started. It
    /// only grows: every connection that ends moves it on.
    pub(super) fn state_changes(&self) -> u64 {
        self.shared.state_changes.load(Ordering::SeqCst)
    }

    /// Starts waiting for the answer to `interaction_id` — or takes it, if it already arrived.
    pub(super) fn register(&self, interaction_id: &str) -> Result<Pending, MobileError> {
        let registration = self
            .shared
            .inbox()
            .register(interaction_id, Instant::now())
            .map_err(|refused| match refused {
                Refused::AlreadyWaiting => MobileError::Protocol(format!(
                    "interaction {interaction_id} is already waiting for its answer on this link"
                )),
                Refused::Closed => MobileError::Closed,
            })?;
        Ok(match registration {
            Registration::Answered(answer) => Pending::Answered(answer),
            Registration::Waiting {
                token,
                answer,
                others_in_flight,
            } => {
                log::registered(self.shared.id, interaction_id, others_in_flight);
                Pending::Waiting(Waiter {
                    shared: self.shared.clone(),
                    id: interaction_id.to_owned(),
                    token,
                    answer,
                })
            }
        })
    }

    pub(super) async fn send(&self, message: &str) -> Result<(), SendError> {
        self.session.send(message).await
    }

    /// Closes the session, telling the wallet. The reader then ends and wakes every waiter.
    pub(super) async fn close(&self) {
        self.session.close().await;
    }
}

/// An answer already there, or one being waited for.
#[derive(Debug)]
pub(super) enum Pending {
    Answered(Value),
    Waiting(Waiter),
}

/// Waiting for one interaction's answer. Dropping it stops waiting; an answer that arrives
/// afterwards is kept for the next request for it.
#[derive(Debug)]
pub(super) struct Waiter {
    shared: Arc<Shared>,
    id: String,
    token: u64,
    answer: oneshot::Receiver<Value>,
}

impl Waiter {
    /// The answer, or `None` once the link has ended.
    pub(super) async fn answer(&mut self) -> Option<Value> {
        (&mut self.answer).await.ok()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.shared.inbox().unregister(&self.id, self.token);
    }
}

/// Every link this connector has started, by [`LinkId`].
///
/// Each link has its own slot and its own async lock, held while its session starts: two
/// requests on one link never start two sessions (which would share an endpoint key and compete
/// at the relay), and a slow start on one link never holds up another.
#[derive(Debug, Default)]
pub(super) struct Registry {
    slots: Mutex<HashMap<LinkId, Arc<Slot>>>,
}

#[derive(Debug, Default)]
struct Slot {
    link: tokio::sync::Mutex<Option<Arc<Link>>>,
    /// This process's hold on the link, taken by its first session and kept for the next ones —
    /// a session started again must not wait for the one it replaces — until the link is closed.
    lock: std::sync::Mutex<Option<Arc<super::lock::LinkLock>>>,
}

impl Slot {
    fn held_lock(&self) -> Option<Arc<super::lock::LinkLock>> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn keep_lock(&self, lock: Option<Arc<super::lock::LinkLock>>) {
        *self.lock.lock().unwrap_or_else(PoisonError::into_inner) = lock;
    }
}

impl Registry {
    fn slot(&self, id: LinkId) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.entry(id).or_default().clone()
    }

    fn existing_slot(&self, id: LinkId) -> Option<Arc<Slot>> {
        let slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.get(&id).cloned()
    }

    /// The running link for `password`, starting its session if there is none.
    pub(super) async fn get_or_start(
        &self,
        password: &ConnectionPassword,
        id: LinkId,
        config: impl FnOnce() -> Result<SessionConfig, MobileError>,
        locks: &super::lock::LinkLocks,
        on: &mut (dyn FnMut(super::Progress) + Send),
    ) -> Result<Arc<Link>, MobileError> {
        let slot = self.slot(id);
        let mut current = slot.link.lock().await;
        if let Some(link) = current.as_ref().filter(|link| link.is_running()) {
            log::session_reused(id);
            return Ok(link.clone());
        }
        // A session that ended still owns its endpoint key until it is fully closed.
        if let Some(stale) = current.take() {
            log::session_restarting(id);
            stale.close().await;
        }
        let config = config()?;
        // One process per link on this machine: another one holding it is waited for — this one,
        // holding it already, is not.
        let lock = match slot.held_lock() {
            Some(lock) => Some(lock),
            None => locks.take(id, on).await.map(Arc::new),
        };
        slot.keep_lock(lock.clone());
        let link = match Link::start(id, password, config, lock).await {
            Ok(link) => Arc::new(link),
            Err(error) => {
                // No session: the link is let go of, for another process to have.
                slot.keep_lock(None);
                return Err(error);
            }
        };
        *current = Some(link.clone());
        Ok(link)
    }

    /// The link for `id`, if one is running and not being started right now.
    pub(super) fn get(&self, id: LinkId) -> Option<Arc<Link>> {
        let slot = self.existing_slot(id)?;
        let current = slot.link.try_lock().ok()?;
        current.as_ref().filter(|link| link.is_running()).cloned()
    }

    /// Closes the link for `id`, if there is one; `why` is for the log.
    pub(super) async fn close(&self, id: LinkId, why: &'static str) {
        let Some(slot) = self.existing_slot(id) else {
            return;
        };
        let mut current = slot.link.lock().await;
        if let Some(link) = current.take() {
            log::closing(id, why);
            link.close().await;
        }
        // The link is let go of (once the session that held it ends, which it just did).
        slot.keep_lock(None);
    }

    /// Closes every link.
    pub(super) async fn close_all(&self) {
        let ids: Vec<LinkId> = {
            let slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            slots.keys().copied().collect()
        };
        for id in ids {
            self.close(id, "closing every link").await;
        }
    }

    /// Takes the link for `id` out and closes it in the background (for `Drop`, which cannot
    /// wait). Without a tokio runtime it stops abruptly.
    /// `false` when there was none.
    pub(super) fn discard(&self, id: LinkId) -> bool {
        let slot = self.existing_slot(id);
        let link = slot
            .as_ref()
            .and_then(|slot| slot.link.try_lock().ok().and_then(|mut link| link.take()));
        // The closing session keeps its own hold until it has ended.
        if let Some(slot) = &slot {
            slot.keep_lock(None);
        }
        let found = link.is_some();
        close_in_background(link.into_iter().collect());
        found
    }

    /// Takes every link out, without waiting (for `Drop`).
    pub(super) fn take_all(&self) -> Vec<Arc<Link>> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots
            .drain()
            .filter_map(|(_, slot)| slot.link.try_lock().ok().and_then(|mut link| link.take()))
            .collect()
    }

    /// How many links are running.
    pub(super) fn running(&self) -> usize {
        let slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots
            .values()
            .filter(|slot| {
                slot.link
                    .try_lock()
                    .is_ok_and(|link| link.as_ref().is_some_and(|link| link.is_running()))
            })
            .count()
    }
}

/// Closes `links` (telling their wallets) on the current tokio runtime, if there is one;
/// otherwise they stop abruptly when dropped.
pub(super) fn close_in_background(links: Vec<Arc<Link>>) {
    if links.is_empty() {
        return;
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        for link in links {
            runtime.spawn(async move { link.close().await });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_named_by_a_hash_never_by_its_password() {
        let password = [0xab_u8; 32];
        let id = LinkId::of(&password);
        let shown = format!("{id} {id:?}");
        assert!(!shown.contains(&hex::encode(password)));
        assert!(!shown.contains(&hex::encode(&password[..5])));
        assert_eq!(
            id.to_string(),
            hex::encode(&linking::connection_id(&password)[..5])
        );
        assert_ne!(LinkId::of(&[0xac; 32]), id);
    }
}
