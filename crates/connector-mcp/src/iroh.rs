//! Radix Connect over iroh in the connector: which transport each link uses, and the iroh
//! session of every link in use.
//!
//! **Which transport.** The setting (`--transport` / `RADIX_CONNECTOR_TRANSPORT`) says how NEW
//! links are paired; each link then keeps the transport it was paired over, recorded in
//! `transports.json` beside `connector.json` (whose format does not change, so older connectors
//! and the WebRTC library keep reading it). Links paired before that record existed were paired
//! over WebRTC. `iroh` additionally sends EVERY link's requests over iroh — for a wallet set to
//! "iroh" only, which reaches even its WebRTC-paired links over iroh.
//!
//! | setting | new pairings | links paired over iroh | links paired over WebRTC |
//! |---|---|---|---|
//! | `auto` (default) | WebRTC and iroh at once, one QR: the one the wallet links over, iroh preferred | iroh | WebRTC |
//! | `webrtc` | WebRTC | iroh | WebRTC |
//! | `iroh` | iroh | iroh | iroh |
//!
//! `auto` pairs with ANY Radix Wallet, as the wallet itself races its transports: one without
//! iroh (the official one) links over WebRTC; one with iroh on (Automatic or iroh) over iroh,
//! and then reaches that link over either transport, so the one recorded keeps working. iroh is
//! preferred: when WebRTC links first, iroh still has a few seconds (`tools::IROH_GRACE`).
//!
//! **One process per link.** A link's iroh session owns an endpoint key derived from the link
//! password; two connector processes (two MCP clients) running one for the same link would
//! compete at the relay and pass the wallet back and forth. So a process takes the link's lock
//! (the SDK's: `MobileWalletConnector` takes an OS file lock per link before starting its session,
//! here in `iroh-links` beside the state), keeps the session while it uses the link and for a while after
//! (the wallet stays connected between requests), and releases it after thirty idle minutes
//! (`RADIX_CONNECTOR_IROH_IDLE_SECONDS`), closing the session first — long, because a wallet
//! takes up to ~15 s to find a session started again. Another process waits for the lock within
//! its own timeout.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use radixdlt_connect_iroh::mobile::{linking, Diagnostics, MobileError, MobileWalletConnector};
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::Zeroizing;

use crate::diag::{iso_secs, Failure};
use crate::rpc::App;

/// Environment variable with the transport setting (`webrtc`, `iroh` or `auto`).
pub const TRANSPORT_ENV: &str = "RADIX_CONNECTOR_TRANSPORT";
/// Environment variable with a custom iroh relay URL (unset or `public`: n0's public relays).
pub const IROH_RELAY_ENV: &str = "RADIX_CONNECTOR_IROH_RELAY";
/// Environment variable overriding how long an unused link's iroh session is kept, in seconds.
pub const IDLE_ENV: &str = "RADIX_CONNECTOR_IROH_IDLE_SECONDS";

/// How long an unused link's iroh session is kept by default: reaching the wallet again after
/// a release costs it up to ~15 s (it re-dials with a back-off), so be generous.
const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);

/// How a link reaches the wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// WebRTC data channels through the Radix signaling server (`radixdlt-connect`).
    Webrtc,
    /// Radix Connect over iroh (`radixdlt-connect-iroh`).
    Iroh,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Transport::Webrtc => "webrtc",
            Transport::Iroh => "iroh",
        }
    }
}

/// The transport setting of this connector process (or of one `pair_wallet` call).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Setting {
    /// Pairs over both transports at once and keeps the one the wallet linked over.
    #[default]
    Auto,
    Webrtc,
    Iroh,
}

impl Setting {
    /// Unset or empty: [`Setting::Auto`].
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Setting::Auto),
            "webrtc" => Ok(Setting::Webrtc),
            "iroh" => Ok(Setting::Iroh),
            other => Err(format!(
                "unknown transport \"{other}\" — use \"auto\", \"webrtc\" or \"iroh\""
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Setting::Webrtc => "webrtc",
            Setting::Iroh => "iroh",
            Setting::Auto => "auto",
        }
    }

    /// The transports a new link is paired over, all at once with one QR; which one the link
    /// keeps is `tools::choose_link`'s decision (iroh preferred).
    pub fn pairing(self) -> &'static [Transport] {
        match self {
            Setting::Auto => &[Transport::Webrtc, Transport::Iroh],
            Setting::Webrtc => &[Transport::Webrtc],
            Setting::Iroh => &[Transport::Iroh],
        }
    }

    /// The transport a request uses on a link paired over `paired_with` (`None`: a link paired
    /// before transports were recorded, i.e. over WebRTC).
    pub fn for_link(self, paired_with: Option<Transport>) -> Transport {
        match (self, paired_with) {
            (Setting::Iroh, _) => Transport::Iroh,
            (_, Some(transport)) => transport,
            (_, None) => Transport::Webrtc,
        }
    }
}

/// The iroh relay choice from a setting: `None` (or `public`) is n0's public relays.
pub fn parse_relay(value: Option<&str>) -> Result<Option<String>, String> {
    match value.map(str::trim) {
        None | Some("") | Some("public") => Ok(None),
        Some(url) => {
            // Checked now, so a typo fails at startup rather than at the first request.
            MobileWalletConnector::new()
                .with_relay_url(url)
                .map_err(|e| e.to_string())?;
            Ok(Some(url.to_string()))
        }
    }
}

/// How one request reaches its wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Webrtc,
    /// Over iroh, on a custom relay or (`None`) n0's public relays.
    Iroh {
        relay: Option<String>,
    },
}

impl Route {
    pub fn label(&self) -> String {
        match self {
            Route::Webrtc => "webrtc".to_string(),
            Route::Iroh { relay: None } => "iroh (public relays)".to_string(),
            Route::Iroh { relay: Some(url) } => format!("iroh (relay {url})"),
        }
    }
}

/* ─────────────────────────── per-link records ─────────────────────────── */

/// How a link was paired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRecord {
    pub transport: Transport,
    /// The custom relay it was paired on; `None`: n0's public relays (or WebRTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh_relay: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecordsFile {
    version: u32,
    /// By connection id (`blake2b(password)`, hex): public, and never the password.
    #[serde(default)]
    links: BTreeMap<String, LinkRecord>,
}

/// `transports.json`: how each link was paired.
pub struct Records {
    path: PathBuf,
}

impl Records {
    pub fn new(path: PathBuf) -> Self {
        Records { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> RecordsFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, password: &[u8]) -> Option<LinkRecord> {
        self.read().links.get(&connection_id(password)).cloned()
    }

    pub fn set(&self, password: &[u8], record: LinkRecord) -> Result<(), String> {
        let mut file = self.read();
        file.version = 1;
        file.links.insert(connection_id(password), record);
        self.write(&file)
    }

    pub fn remove(&self, password: &[u8]) -> Result<(), String> {
        let mut file = self.read();
        if file.links.remove(&connection_id(password)).is_some() {
            self.write(&file)?;
        }
        Ok(())
    }

    /// Written whole to a temporary file and renamed over the old one, readable by its owner
    /// only: it names links (by a public hash), but there is no reason for others to read it.
    fn write(&self, file: &RecordsFile) -> Result<(), String> {
        let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let temporary = self.path.with_extension("json.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut out = options.open(&temporary).map_err(|e| e.to_string())?;
        out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        out.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, &self.path).map_err(|e| e.to_string())
    }
}

/// A link's public name: `blake2b(password)`, hex.
pub fn connection_id(password: &[u8]) -> String {
    hex::encode(linking::connection_id(password))
}

/* ─────────────────────────── the iroh sessions ─────────────────────────── */

/// How long, after opening a link's session, the connector watches for the wallet to reach it
/// (logged as `iroh_link_wallet_connected`, or `iroh_link_wallet_not_connected` with this limit).
const RECONNECT_WATCH: Duration = Duration::from_secs(120);
/// How often that watch looks.
const RECONNECT_POLL: Duration = Duration::from_millis(100);

/// A link whose iroh session this process holds.
pub struct Held {
    connector: Arc<MobileWalletConnector>,
    /// Needed to close the session; memory only, wiped when dropped.
    password: Zeroizing<Vec<u8>>,
    /// Calls using the link right now.
    users: u32,
    /// Moves on with every use, so a pending idle release knows it is stale.
    generation: u64,
    /// When this process opened (or adopted) its session.
    opened_at: Instant,
    /// Since when nobody uses it — a release is scheduled — if so.
    idle_since: Option<Instant>,
    /// Calls that used it.
    uses: u64,
}

/// The links this process holds, by connection id. Shared (behind a mutex) with the bridge that
/// writes the libraries' own events to the log, which attaches their diagnostics to a collision.
pub type HeldLinks = Arc<Mutex<HashMap<String, Held>>>;

/// One line per link held in `held`: `link <id>: <diagnostics>`. Skips (rather than waits) when
/// the table is busy, as it is called from inside logging.
pub fn held_diagnostics(held: &HeldLinks) -> Vec<String> {
    let Ok(held) = held.try_lock() else {
        return vec!["(links busy)".to_string()];
    };
    held.iter()
        .map(|(key, link)| {
            let now = link
                .connector
                .diagnostics(&link.password)
                .map_or_else(|| "no session".to_string(), |d| d.to_string());
            format!("link {}: {now}", short_id(key))
        })
        .collect()
}

/// The iroh side of the connector: its setting and the sessions it holds.
pub struct Iroh {
    pub setting: Setting,
    /// The relay for new pairings and for links forced onto iroh; `None`: public relays.
    pub relay: Option<String>,
    pub records: Records,
    lock_dir: PathBuf,
    idle: Duration,
    connectors: RefCell<HashMap<Option<String>, Arc<MobileWalletConnector>>>,
    held: HeldLinks,
    /// When this process last let go of each link, and why: the reason a session is reopened.
    released: RefCell<HashMap<String, (Instant, &'static str)>>,
}

/// One call's use of a link's iroh session. Dropping it lets the link go idle.
pub struct LinkTurn {
    app: Rc<App>,
    key: String,
    pub connector: Arc<MobileWalletConnector>,
}

impl Drop for LinkTurn {
    fn drop(&mut self) {
        release(&self.app, &self.key);
    }
}

impl Iroh {
    /// The iroh side for state kept in `dir`.
    pub fn new(setting: Setting, relay: Option<String>, dir: &Path) -> Self {
        let idle = std::env::var(IDLE_ENV)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map_or(DEFAULT_IDLE, Duration::from_secs);
        Iroh {
            setting,
            relay,
            records: Records::new(dir.join("transports.json")),
            lock_dir: dir.join("iroh-links"),
            idle,
            connectors: RefCell::new(HashMap::new()),
            held: HeldLinks::default(),
            released: RefCell::new(HashMap::new()),
        }
    }

    /// Lets go of an unused link after `idle` instead.
    #[cfg(test)]
    pub fn set_idle(&mut self, idle: Duration) {
        self.idle = idle;
    }

    /// The links this process holds (for the log bridge).
    pub fn held_links(&self) -> HeldLinks {
        self.held.clone()
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, Held>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How requests on the link of `password` reach the wallet.
    pub fn route(&self, password: &[u8]) -> Route {
        self.route_why(password).0
    }

    /// [`route`](Self::route), and why that one — for the log.
    pub fn route_why(&self, password: &[u8]) -> (Route, &'static str) {
        let record = self.records.get(password);
        let why = match (self.setting, &record) {
            (Setting::Iroh, _) => "setting iroh: every link over iroh",
            (
                _,
                Some(LinkRecord {
                    transport: Transport::Iroh,
                    ..
                }),
            ) => "paired over iroh (transports.json)",
            (_, Some(_)) => "paired over webrtc (transports.json)",
            (_, None) => "no record: paired before transports were recorded, so webrtc",
        };
        let route = match self.setting.for_link(record.as_ref().map(|r| r.transport)) {
            Transport::Webrtc => Route::Webrtc,
            Transport::Iroh => Route::Iroh {
                relay: match record {
                    Some(LinkRecord {
                        transport: Transport::Iroh,
                        iroh_relay,
                    }) => iroh_relay,
                    // A link paired over WebRTC, sent over iroh: the configured relays.
                    _ => self.relay.clone(),
                },
            },
        };
        (route, why)
    }

    /// The connector for one relay choice (one per choice, shared by its links).
    pub fn connector(&self, relay: &Option<String>) -> Result<Arc<MobileWalletConnector>, String> {
        if let Some(connector) = self.connectors.borrow().get(relay) {
            return Ok(connector.clone());
        }
        // The SDK keeps one process per link with its locks; here, beside the state, where every
        // connector process of this state looks.
        let connector = match relay {
            None => MobileWalletConnector::new(),
            Some(url) => MobileWalletConnector::new()
                .with_relay_url(url)
                .map_err(|e| e.to_string())?,
        }
        .with_link_lock_dir(self.lock_dir.clone());
        let connector = Arc::new(connector);
        self.connectors
            .borrow_mut()
            .insert(relay.clone(), connector.clone());
        Ok(connector)
    }

    /// The live connectivity of a link this process holds.
    pub fn diagnostics(&self, password: &[u8]) -> Option<Diagnostics> {
        let held = self.held();
        let held = held.get(&connection_id(password))?;
        held.connector.diagnostics(password)
    }

    /// Who holds the iroh link of `password` on this machine: this process, another one (by
    /// pid, and since when), or nobody.
    pub fn holder(&self, password: &[u8]) -> String {
        let key = connection_id(password);
        if let Some(held) = self.held().get(&key) {
            return format!(
                "this process (pid {}), for {} s, {} call(s) in use",
                std::process::id(),
                held.opened_at.elapsed().as_secs(),
                held.users
            );
        }
        match self.connector(&None).ok().and_then(|c| c.link_holder(password)) {
            Some(record) => format!("another process ({})", describe_holder(&record)),
            None => "nobody".to_string(),
        }
    }

    /// Why a session for `key` is being opened now.
    fn opening_reason(&self, key: &str) -> (&'static str, Option<u64>) {
        match self.released.borrow().get(key) {
            None => ("first use in this process", None),
            Some((at, "idle release")) => ("reopened after an idle release", Some(ms(at.elapsed()))),
            Some((at, _)) => ("reopened after the link was closed", Some(ms(at.elapsed()))),
        }
    }
}

/// Milliseconds, rounded to the nearest, for the log.
pub fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros().saturating_add(500) / 1000).unwrap_or(u64::MAX)
}

/// Takes the link of `password` for one call: its session, started (after taking the link's OS
/// lock) if this process does not hold it yet. Waits, until `deadline`, for another connector
/// process to release the link.
pub async fn acquire(
    app: &Rc<App>,
    password: &[u8],
    relay: &Option<String>,
    deadline: Instant,
) -> Result<LinkTurn, Failure> {
    let iroh = &app.iroh;
    let key = connection_id(password);
    let link = short_id(&key).to_string();
    let started = Instant::now();
    let limit_ms = ms(deadline.saturating_duration_since(started));
    let connector = iroh.connector(relay).map_err(|e| unavailable(&e))?;
    if let Some(turn) = join(app, &key) {
        return Ok(turn);
    }
    // Another process of this machine holding the link is waited for by the SDK, within this
    // call's own time; it logs the wait and the taking (`iroh_sdk` in this log).
    let (why, released_ago_ms) = iroh.opening_reason(&key);
    let opening = Instant::now();
    let opened = connector
        .open_link_within(password, deadline.saturating_duration_since(Instant::now()))
        .await;
    if let Err(MobileError::LinkBusy { holder }) = &opened {
        let waited_ms = ms(started.elapsed());
        app.log.event(
            "iroh_link_busy",
            json!({ "link": link, "holder": describe_holder(holder), "waited_ms": waited_ms, "limit_ms": limit_ms }),
        );
        return Err(link_busy(Some(holder), iroh.idle, waited_ms, limit_ms));
    }
    // Another call of this process may have opened it meanwhile: join it.
    if let Some(turn) = join(app, &key) {
        return Ok(turn);
    }
    if let Err(error) = opened {
        app.log.event(
            "iroh_link_open_failed",
            json!({ "link": link, "why": why, "error": error.to_string(), "elapsed_ms": ms(opening.elapsed()) }),
        );
        return Err(Failure::from_mobile(&error, false));
    }
    app.log.event(
        "iroh_link_open",
        json!({
            "link": link, "relays": relay_label(relay), "why": why, "released_ago_ms": released_ago_ms,
            "open_ms": ms(opening.elapsed()),
        }),
    );
    watch_wallet_connects(app, &key, password, &connector, why);
    // Another call may have taken the link while this one opened it: join it.
    let mut held = iroh.held();
    let entry = held.entry(key.clone()).or_insert_with(|| Held {
        connector: connector.clone(),
        password: Zeroizing::new(password.to_vec()),
        users: 0,
        generation: 0,
        opened_at: Instant::now(),
        idle_since: None,
        uses: 0,
    });
    entry.users += 1;
    entry.generation += 1;
    entry.uses += 1;
    Ok(LinkTurn {
        app: app.clone(),
        key,
        connector: entry.connector.clone(),
    })
}

/// A turn on a link this process already holds, if it does — cancelling a scheduled release.
fn join(app: &Rc<App>, key: &str) -> Option<LinkTurn> {
    let (connector, users, was_idle) = {
        let mut held = app.iroh.held();
        let entry = held.get_mut(key)?;
        entry.users += 1;
        entry.generation += 1;
        entry.uses += 1;
        let was_idle = entry.idle_since.take().map(|since| since.elapsed());
        (entry.connector.clone(), entry.users, was_idle)
    };
    match was_idle {
        Some(idle) => app.log.event(
            "iroh_link_release_cancelled",
            json!({ "link": short_id(key), "idle_ms": ms(idle), "release_after_ms": ms(app.iroh.idle) }),
        ),
        None => app.log.event(
            "iroh_link_reused",
            json!({ "link": short_id(key), "users": users }),
        ),
    }
    Some(LinkTurn {
        app: app.clone(),
        key: key.to_string(),
        connector,
    })
}

/// Watches, in the background, for the wallet to reach a session just opened, and logs how long
/// that took — what a person feels after an idle release: the wallet re-dials with a back-off.
fn watch_wallet_connects(
    app: &Rc<App>,
    key: &str,
    password: &[u8],
    connector: &Arc<MobileWalletConnector>,
    why: &'static str,
) {
    let (app, key, connector) = (app.clone(), key.to_string(), connector.clone());
    let password = Zeroizing::new(password.to_vec());
    let started = Instant::now();
    tokio::task::spawn_local(async move {
        let link = short_id(&key).to_string();
        while started.elapsed() < RECONNECT_WATCH {
            if connector.is_connected(&password) {
                let diagnostics = connector.diagnostics(&password).map(|d| d.to_string());
                app.log.event(
                    "iroh_link_wallet_connected",
                    json!({ "link": link, "after_ms": ms(started.elapsed()), "opened": why, "diagnostics": diagnostics }),
                );
                return;
            }
            // Stop once the link was let go of (or never held by this process).
            if !app.iroh.held().contains_key(&key) && started.elapsed() > Duration::from_secs(1) {
                return;
            }
            tokio::time::sleep(RECONNECT_POLL).await;
        }
        let diagnostics = connector.diagnostics(&password).map(|d| d.to_string());
        app.log.event(
            "iroh_link_wallet_not_connected",
            json!({
                "link": link, "waited_ms": ms(started.elapsed()), "limit_ms": ms(RECONNECT_WATCH),
                "opened": why, "diagnostics": diagnostics,
            }),
        );
    });
}

/// Takes over the session a pairing over iroh just started (nobody else knows its password, so
/// its lock is free), and lets it go idle like any other.
pub fn adopt(app: &Rc<App>, password: &[u8], relay: &Option<String>) {
    let key = connection_id(password);
    let link = short_id(&key).to_string();
    // Its lock is the SDK's, taken when the pairing started its session.
    let Ok(connector) = app.iroh.connector(relay) else {
        app.log.event(
            "iroh_link_adopt_failed",
            json!({ "link": link, "detail": "no connector for its relay; its session stays until it is used or closed" }),
        );
        return;
    };
    app.iroh.held().insert(
        key.clone(),
        Held {
            connector,
            password: Zeroizing::new(password.to_vec()),
            users: 1,
            generation: 0,
            opened_at: Instant::now(),
            idle_since: None,
            uses: 0,
        },
    );
    app.log.event(
        "iroh_link_adopted",
        json!({ "link": link, "relays": relay_label(relay), "why": "paired over iroh: its session is kept for the first requests" }),
    );
    release(app, &key);
}

/// Closes the link's session (if this process holds it) and forgets how it was paired.
pub async fn forget(app: &Rc<App>, password: &[u8], why: &'static str) {
    let _ = app.iroh.records.remove(password);
    let key = connection_id(password);
    let held = app.iroh.held().remove(&key);
    if let Some(held) = held {
        close(app, &key, held, why).await;
    }
}

/// Closes every link this process holds (the connector is exiting).
pub async fn close_all(app: &Rc<App>, why: &'static str) {
    let all: Vec<(String, Held)> = app.iroh.held().drain().collect();
    for (key, held) in all {
        close(app, &key, held, why).await;
    }
}

/// Closes one held link's session, then lets go of its lock (dropping `held`).
async fn close(app: &Rc<App>, key: &str, held: Held, why: &'static str) {
    // Closed BEFORE the lock goes, so no other process starts a session for the link while this
    // one still holds its endpoint.
    held.connector.close_link(&held.password).await;
    app.iroh
        .released
        .borrow_mut()
        .insert(key.to_string(), (Instant::now(), why));
    app.log.event(
        "iroh_link_closed",
        json!({ "link": short_id(key), "why": why, "lifetime_ms": ms(held.opened_at.elapsed()), "uses": held.uses }),
    );
}

/// One call is done with the link: when nobody uses it any more, it is closed after the idle
/// time unless something uses it again first.
fn release(app: &Rc<App>, key: &str) {
    let generation = {
        let mut held = app.iroh.held();
        let Some(entry) = held.get_mut(key) else {
            return;
        };
        entry.users = entry.users.saturating_sub(1);
        if entry.users > 0 {
            return;
        }
        entry.generation += 1;
        entry.idle_since = Some(Instant::now());
        entry.generation
    };
    let app = app.clone();
    let key = key.to_string();
    let idle = app.iroh.idle;
    app.log.event(
        "iroh_link_idle",
        json!({ "link": short_id(&key), "release_after_ms": ms(idle) }),
    );
    tokio::task::spawn_local(async move {
        tokio::time::sleep(idle).await;
        let expired = {
            let mut held = app.iroh.held();
            let still_idle = held
                .get(&key)
                .is_some_and(|h| h.users == 0 && h.generation == generation);
            if still_idle {
                held.remove(&key)
            } else {
                None
            }
        };
        if let Some(held) = expired {
            let (lifetime_ms, uses) = (ms(held.opened_at.elapsed()), held.uses);
            held.connector.close_link(&held.password).await;
            drop(held);
            app.iroh
                .released
                .borrow_mut()
                .insert(key.clone(), (Instant::now(), "idle release"));
            app.log.event(
                "iroh_link_released",
                json!({ "link": short_id(&key), "idle_ms": ms(idle), "lifetime_ms": lifetime_ms, "uses": uses }),
            );
        }
    });
}

/// What the SDK's lock records of a link's holder (`process <pid> since <unix seconds>`), for
/// people: `pid <pid>, holding it since <time>`.
fn describe_holder(record: &str) -> String {
    let mut words = record.split_whitespace();
    match (words.next(), words.next(), words.next(), words.next()) {
        (Some("process"), Some(pid), Some("since"), Some(since)) => match since.parse() {
            Ok(since) => format!("pid {pid}, holding it since {}", iso_secs(since)),
            Err(_) => format!("pid {pid}"),
        },
        _ => record.to_string(),
    }
}

/// A connection id shortened for the log, like the library's link ids.
pub fn short_id(key: &str) -> &str {
    key.get(..10).unwrap_or(key)
}

pub fn relay_label(relay: &Option<String>) -> String {
    relay.clone().unwrap_or_else(|| "public".to_string())
}

fn unavailable(detail: &str) -> Failure {
    Failure::new(
        "IROH_UNAVAILABLE",
        "connect",
        true,
        format!("could not set up iroh: {detail}"),
        "Nothing reached the wallet. Check the iroh relay setting (RADIX_CONNECTOR_IROH_RELAY / --iroh-relay).",
    )
}

fn link_busy(holder: Option<&str>, idle: Duration, waited_ms: u64, limit_ms: u64) -> Failure {
    let who = holder.map_or_else(
        || "another radix-connector-mcp process".to_string(),
        |holder| {
            format!(
                "another radix-connector-mcp process ({})",
                describe_holder(holder)
            )
        },
    );
    Failure::new(
        "LINK_BUSY",
        "queue",
        true,
        format!(
            "{who} holds this wallet's iroh link; waited {waited_ms} ms of {limit_ms} ms for it (no iroh session in this process)"
        ),
        format!(
            "Only one connector process at a time can talk to a wallet over iroh. Use the MCP client that runs it, close that client, or wait: it lets the link go after {} min without requests. Nothing was sent.",
            idle.as_secs().div_ceil(60)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_decides_new_pairings_and_only_iroh_overrides_a_links_record() {
        // Unset, the connector pairs over both transports.
        assert_eq!(Setting::parse("").unwrap(), Setting::Auto);
        assert_eq!(Setting::parse("  ").unwrap(), Setting::Auto);
        assert_eq!(Setting::default(), Setting::Auto);
        assert_eq!(Setting::parse(" Auto ").unwrap(), Setting::Auto);
        assert_eq!(Setting::parse("WebRTC").unwrap(), Setting::Webrtc);
        assert_eq!(Setting::parse("iroh").unwrap(), Setting::Iroh);
        assert!(Setting::parse("carrier-pigeon").is_err());

        assert_eq!(Setting::Auto.pairing(), [Transport::Webrtc, Transport::Iroh]);
        assert_eq!(Setting::Webrtc.pairing(), [Transport::Webrtc]);
        assert_eq!(Setting::Iroh.pairing(), [Transport::Iroh]);

        // Unchanged by default: every link paired before keeps WebRTC.
        assert_eq!(Setting::Webrtc.for_link(None), Transport::Webrtc);
        assert_eq!(Setting::Auto.for_link(None), Transport::Webrtc);
        // A link paired over iroh stays on iroh.
        assert_eq!(Setting::Webrtc.for_link(Some(Transport::Iroh)), Transport::Iroh);
        assert_eq!(Setting::Auto.for_link(Some(Transport::Webrtc)), Transport::Webrtc);
        // `iroh` reaches every link over iroh.
        assert_eq!(Setting::Iroh.for_link(Some(Transport::Webrtc)), Transport::Iroh);
        assert_eq!(Setting::Iroh.for_link(None), Transport::Iroh);
    }

    #[test]
    fn relays_are_public_unless_a_usable_url_is_given() {
        assert_eq!(parse_relay(None).unwrap(), None);
        assert_eq!(parse_relay(Some("public")).unwrap(), None);
        assert_eq!(
            parse_relay(Some("https://relay.example.com")).unwrap(),
            Some("https://relay.example.com".to_string())
        );
        assert!(parse_relay(Some("http://relay.example.com")).is_err());
    }

    #[test]
    fn records_name_links_by_hash_and_route_their_requests() {
        let dir = std::env::temp_dir().join(format!("connector_mcp_iroh_{}", std::process::id()));
        let iroh = Iroh::new(Setting::Auto, None, &dir);
        let (old, paired_over_iroh) = ([1u8; 32], [2u8; 32]);
        assert_eq!(iroh.route(&old), Route::Webrtc);
        iroh.records
            .set(
                &paired_over_iroh,
                LinkRecord {
                    transport: Transport::Iroh,
                    iroh_relay: Some("https://relay.example.com".into()),
                },
            )
            .unwrap();
        assert_eq!(
            iroh.route(&paired_over_iroh),
            Route::Iroh {
                relay: Some("https://relay.example.com".into())
            }
        );
        let text = std::fs::read_to_string(iroh.records.path()).unwrap();
        assert!(!text.contains(&hex::encode(paired_over_iroh)), "{text}");
        assert!(text.contains(&connection_id(&paired_over_iroh)));

        // Forced onto iroh, an old link uses the configured relays.
        let forced = Iroh::new(Setting::Iroh, None, &dir);
        assert_eq!(forced.route(&old), Route::Iroh { relay: None });

        iroh.records.remove(&paired_over_iroh).unwrap();
        assert_eq!(iroh.route(&paired_over_iroh), Route::Webrtc);
        std::fs::remove_dir_all(&dir).ok();
    }
}
