//! Removing a wallet here removes this connector from the wallet too, and the other way round.
//!
//! The SDK does it (`radixdlt_connect_auto`, a connector given this connector's state file): it
//! retires the link and tells the wallet, at once when it is reachable and otherwise whenever it
//! next connects while this connector runs, gives a notice up after 30 days, and forgets a link
//! its wallet removed. What stays here is this process's own bookkeeping: the iroh sessions it
//! holds (`iroh`), let go of when their link goes.

use std::rc::Rc;
use std::time::Duration;

use radixdlt_connect_auto::{Removal, Transports, WalletConnector};
use serde_json::json;

use crate::iroh::{self, Setting};
use crate::rpc::App;
use crate::store::Store;

/// How long `remove_wallet` waits for the wallet before leaving the notice to
/// [`deliver_notices`]: a wallet whose app is open right now gets it in a second or two.
pub const AT_ONCE: Duration = Duration::from_secs(10);

/// The SDK's connector, over this process's own transports and its state file.
fn connector(app: &App) -> Result<WalletConnector, String> {
    // This process's own iroh connector, so a link has one session here whoever opens it: the
    // SDK's notice and this process's requests never compete for it. A link that recorded no
    // relay is reached on the one this connector is set to, as every request here does.
    let mut connector = WalletConnector::new()
        .with_webrtc(app.webrtc_connector())
        .with_iroh_shared(app.iroh.connector(&app.iroh.relay)?)
        // As for requests: the setting `iroh` reaches every link over iroh, whatever it recorded.
        .with_transports(match app.iroh.setting {
            Setting::Iroh => Transports::IrohOnly,
            Setting::Auto | Setting::Webrtc => Transports::Auto,
        })
        .with_state(app.config_path());
    if let Some(url) = &app.iroh.relay {
        connector = connector.with_iroh_on_relay(url, app.iroh.connector(&app.iroh.relay)?);
    }
    Ok(connector)
}

/// Unpairs the wallet `wallet_public_key` and tells it, waiting [`AT_ONCE`] at most. `Ok(true)`:
/// the wallet was told; `Ok(false)`: it was not reachable, and [`deliver_notices`] tells it.
pub async fn remove(app: &Rc<App>, wallet_public_key: &str) -> Result<bool, String> {
    let password = Store::load(app.config_path())
        .ok()
        .and_then(|state| state.password_bytes_for(wallet_public_key).ok())
        .ok_or_else(|| format!("no paired wallet with public key {wallet_public_key}."))?;
    // This process lets go of its session first, so the SDK tells the wallet over a fresh one.
    iroh::forget(app, &password, "removed here").await;
    let link = iroh::short_id(&iroh::connection_id(&password)).to_string();
    let removal = connector(app)?
        .remove_link(wallet_public_key, AT_ONCE)
        .await
        .map_err(|e| e.to_string())?;
    app.log.event(
        "wallet_retired",
        json!({ "link": link, "told": removal == Removal::Told }),
    );
    Ok(removal == Removal::Told)
}

/// The wallet removed this link: forgets it here (no notice: the wallet knows) and lets go of its
/// session. A request over iroh has the SDK forget it already; over WebRTC alone this does.
/// Returns whether a paired wallet had that password.
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

/// Delivers the notices of the wallets removed here, for as long as this connector runs (the
/// SDK's `deliver_removal_notices`).
pub async fn deliver_notices(app: Rc<App>) {
    match connector(&app) {
        Ok(connector) => connector.deliver_removal_notices().await,
        Err(error) => app
            .log
            .event("removal_notices_unavailable", json!({ "error": error })),
    }
}
