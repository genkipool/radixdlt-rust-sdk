//! Removing a wallet here removes this connector from the wallet too, and the other way round
//! (see [`radixdlt_connect_webrtc::link_removal`]).
//!
//! - `remove_wallet` unpairs the wallet and keeps a notice ([`RetiredLink`], in `connector.json`)
//!   until the wallet confirmed it was told; while this connector runs, [`deliver_notices`] keeps
//!   each of those links listening until its wallet shows up — the wallet only connects while its
//!   app is open — and gives a notice up after its lifetime.
//! - A wallet that removed this connector says so when it next connects; the request then fails
//!   with `LINK_REMOVED`, and [`forget_removed_by_wallet`] unpairs it here, untold.

use std::rc::Rc;
use std::time::Duration;

use radixdlt_connect_webrtc::state::RetiredLink;
use serde_json::json;
use std::time::Instant;

use crate::diag::now_secs;
use crate::iroh::{self, Route};
use crate::rpc::App;
use crate::store::Store;

/// How long one attempt listens for the wallet before starting again: long enough for a person to
/// open the app, short enough that a notice given up meanwhile is noticed.
const ATTEMPT: Duration = Duration::from_secs(5 * 60);
/// The pause after an attempt that failed for another reason than the wallet not being there.
const PAUSE: Duration = Duration::from_secs(30);
/// How often the notices are read again, to pick up the ones `remove_wallet` adds.
const RESCAN: Duration = Duration::from_secs(20);
/// How long `remove_wallet` waits for the wallet before leaving the notice to [`deliver_notices`]:
/// a wallet whose app is open right now gets it in a second or two.
pub const AT_ONCE: Duration = Duration::from_secs(10);

/// Unpairs the wallet `wallet_public_key` and keeps a notice for it. Returns the notice, or why it
/// could not be done.
pub fn retire(app: &App, wallet_public_key: &str) -> Result<RetiredLink, String> {
    let mut state = Store::load_or_init(app.config_path())
        .map_err(|e| format!("could not open the connector state: {e}"))?;
    let retired = state
        .retire_link(wallet_public_key, now_secs())
        .ok_or_else(|| format!("no paired wallet with public key {wallet_public_key}."))?;
    Store::save(app.config_path(), &state).map_err(|e| format!("could not save the state: {e}"))?;
    app.log.event(
        "wallet_retired",
        json!({ "link": iroh::short_id(&iroh::connection_id(&retired.password_bytes().unwrap_or_default())) }),
    );
    Ok(retired)
}

/// The wallet removed this link: unpairs it here (no notice: the wallet knows) and closes its
/// session. Returns whether a paired wallet had that password.
pub async fn forget_removed_by_wallet(app: &Rc<App>, password: &[u8]) -> bool {
    let link = iroh::short_id(&iroh::connection_id(password)).to_string();
    let removed = match Store::load(app.config_path()) {
        Ok(mut state) => state
            .remove_link_removed_by_wallet(password)
            .filter(|_| Store::save(app.config_path(), &state).is_ok())
            .is_some(),
        Err(_) => false,
    };
    app.log.event(
        "wallet_removed_link",
        json!({ "link": link, "unpaired_here": removed }),
    );
    iroh::forget(app, password, "the wallet removed the link").await;
    removed
}

/// Delivers the notices of the wallets removed here, for as long as this connector runs: each
/// one in its own task, which ends when its wallet confirmed it, or when the notice expires or is
/// gone (another connector process delivered it).
pub async fn deliver_notices(app: Rc<App>) {
    let mut running: Vec<String> = Vec::new();
    loop {
        let notices = pending(&app).await;
        running.retain(|password| notices.iter().any(|n| &n.password == password));
        for notice in notices {
            if running.contains(&notice.password) {
                continue;
            }
            running.push(notice.password.clone());
            tokio::task::spawn_local(deliver(app.clone(), notice));
        }
        tokio::time::sleep(RESCAN).await;
    }
}

/// The notices still to deliver, after giving up the expired ones.
async fn pending(app: &Rc<App>) -> Vec<RetiredLink> {
    let Ok(mut state) = Store::load(app.config_path()) else {
        return Vec::new();
    };
    let now = now_secs();
    let expired: Vec<RetiredLink> = state
        .retired_links
        .iter()
        .filter(|n| n.expired(now))
        .cloned()
        .collect();
    if !expired.is_empty() && state.prune_retired(now) > 0 && Store::save(app.config_path(), &state).is_ok() {
        for notice in expired {
            if let Ok(password) = notice.password_bytes() {
                app.log.event(
                    "removal_notice_expired",
                    json!({ "link": iroh::short_id(&iroh::connection_id(&password)) }),
                );
                iroh::forget(app, &password, "removal notice expired").await;
            }
        }
    }
    state.retired_links
}

/// Tries to tell the wallet now, for `wait` at most. Returns whether it was told (the notice is
/// then gone); otherwise the notice stays for [`deliver_notices`].
pub async fn tell_now(app: &Rc<App>, notice: &RetiredLink, wait: Duration) -> bool {
    match notice.password_bytes() {
        Ok(password) => try_once(app, notice, &password, wait).await.is_ok(),
        Err(_) => false,
    }
}

/// Delivers one notice: attempt after attempt until the wallet confirmed it, then forgets it.
async fn deliver(app: Rc<App>, notice: RetiredLink) {
    let Ok(password) = notice.password_bytes() else {
        return;
    };
    while still_pending(&app, &notice) {
        match try_once(&app, &notice, &password, ATTEMPT).await {
            Ok(()) => return,
            // Not there yet: listen again at once.
            Err(Attempt::Absent) => {}
            Err(Attempt::Failed(_)) => tokio::time::sleep(PAUSE).await,
        }
    }
}

/// One attempt, for `wait` at most: on success the notice is forgotten and the link let go.
async fn try_once(
    app: &Rc<App>,
    notice: &RetiredLink,
    password: &[u8],
    wait: Duration,
) -> Result<(), Attempt> {
    let link = iroh::short_id(&iroh::connection_id(password)).to_string();
    let route = app.iroh.route(password);
    app.log.event(
        "removal_notice_waiting",
        json!({ "link": link, "transport": route.label(), "wait_s": wait.as_secs() }),
    );
    let result = attempt(app, password, &route, wait).await;
    match &result {
        Ok(()) => {
            if let Ok(mut state) = Store::load(app.config_path()) {
                if state.forget_retired(&notice.password) {
                    let _ = Store::save(app.config_path(), &state);
                }
            }
            app.log.event(
                "removal_notice_delivered",
                json!({ "link": link, "transport": route.label() }),
            );
            iroh::forget(app, password, "removal notice delivered").await;
        }
        Err(Attempt::Absent) => {}
        Err(Attempt::Failed(detail)) => {
            app.log
                .event("removal_notice_failed", json!({ "link": link, "detail": detail }));
        }
    }
    result
}

/// Why an attempt did not deliver.
enum Attempt {
    /// The wallet did not show up while it listened.
    Absent,
    /// Anything else.
    Failed(String),
}

async fn attempt(app: &Rc<App>, password: &[u8], route: &Route, wait: Duration) -> Result<(), Attempt> {
    use radixdlt_connect_webrtc::ConnectError;
    use radixdlt_connect_iroh::mobile::MobileError;
    match route {
        Route::Webrtc => match app.webrtc_connector().notify_link_removed(password, wait).await {
            Ok(()) => Ok(()),
            Err(ConnectError::ChannelTimeout | ConnectError::ConfirmationTimeout) => Err(Attempt::Absent),
            Err(error) => Err(Attempt::Failed(error.to_string())),
        },
        Route::Iroh { relay } => {
            let deadline = Instant::now() + wait;
            let turn = iroh::acquire(app, password, relay, deadline)
                .await
                .map_err(|failure| Attempt::Failed(failure.detail))?;
            let left = deadline.saturating_duration_since(Instant::now());
            match turn.connector.notify_link_removed(password, left).await {
                Ok(()) => Ok(()),
                Err(MobileError::WalletUnreachable { .. } | MobileError::ConfirmationTimeout { .. }) => {
                    Err(Attempt::Absent)
                }
                Err(error) => Err(Attempt::Failed(error.to_string())),
            }
        }
    }
}

/// Whether the notice is still kept (another process may have delivered it, or it expired).
fn still_pending(app: &App, notice: &RetiredLink) -> bool {
    Store::load(app.config_path()).is_ok_and(|state| {
        state
            .retired_links
            .iter()
            .any(|n| n.password == notice.password && !n.expired(now_secs()))
    })
}
