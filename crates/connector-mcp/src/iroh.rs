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
//! compete at the relay and pass the wallet back and forth. So a process takes an OS file lock
//! per link before starting its session, keeps it while it uses the link and for a while after
//! (the wallet stays connected between requests), and releases it after thirty idle minutes
//! (`RADIX_CONNECTOR_IROH_IDLE_SECONDS`), closing the session first — long, because a wallet
//! takes up to ~15 s to find a session started again. Another process waits for the lock within
//! its own timeout.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use radixdlt_connect_iroh::mobile::{linking, Diagnostics, MobileWalletConnector};
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::Zeroizing;

use crate::diag::Failure;
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
/// How often a process waiting for another one to release a link checks again.
const TURN_POLL: Duration = Duration::from_millis(250);

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

/// A link whose iroh session this process holds.
struct Held {
    /// The link's OS lock; released when dropped, after the session is closed.
    _lock: File,
    connector: Rc<MobileWalletConnector>,
    /// Needed to close the session; memory only, wiped when dropped.
    password: Zeroizing<Vec<u8>>,
    /// Calls using the link right now.
    users: u32,
    /// Moves on with every use, so a pending idle release knows it is stale.
    generation: u64,
}

/// The iroh side of the connector: its setting and the sessions it holds.
pub struct Iroh {
    pub setting: Setting,
    /// The relay for new pairings and for links forced onto iroh; `None`: public relays.
    pub relay: Option<String>,
    pub records: Records,
    lock_dir: PathBuf,
    idle: Duration,
    connectors: RefCell<HashMap<Option<String>, Rc<MobileWalletConnector>>>,
    held: RefCell<HashMap<String, Held>>,
}

/// One call's use of a link's iroh session. Dropping it lets the link go idle.
pub struct LinkTurn {
    app: Rc<App>,
    key: String,
    pub connector: Rc<MobileWalletConnector>,
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
            held: RefCell::new(HashMap::new()),
        }
    }

    /// How requests on the link of `password` reach the wallet.
    pub fn route(&self, password: &[u8]) -> Route {
        let record = self.records.get(password);
        match self.setting.for_link(record.as_ref().map(|r| r.transport)) {
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
        }
    }

    /// The connector for one relay choice (one per choice, shared by its links).
    pub fn connector(&self, relay: &Option<String>) -> Result<Rc<MobileWalletConnector>, String> {
        if let Some(connector) = self.connectors.borrow().get(relay) {
            return Ok(connector.clone());
        }
        let connector = match relay {
            None => MobileWalletConnector::new(),
            Some(url) => MobileWalletConnector::new()
                .with_relay_url(url)
                .map_err(|e| e.to_string())?,
        };
        let connector = Rc::new(connector);
        self.connectors
            .borrow_mut()
            .insert(relay.clone(), connector.clone());
        Ok(connector)
    }

    /// The live connectivity of a link this process holds.
    pub fn diagnostics(&self, password: &[u8]) -> Option<Diagnostics> {
        let held = self.held.borrow();
        let held = held.get(&connection_id(password))?;
        held.connector.diagnostics(password)
    }

    fn lock_path(&self, key: &str) -> PathBuf {
        // Half the hash is plenty to tell a machine's links apart.
        self.lock_dir
            .join(format!("{}.lock", key.get(..32).unwrap_or(key)))
    }
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
    let connector = iroh.connector(relay).map_err(|e| unavailable(&e))?;
    let mut lock: Option<File> = None;
    let mut announced = false;
    loop {
        if let Some(held) = iroh.held.borrow_mut().get_mut(&key) {
            held.users += 1;
            held.generation += 1;
            return Ok(LinkTurn {
                app: app.clone(),
                key,
                connector: held.connector.clone(),
            });
        }
        let file = match lock.take() {
            Some(file) => file,
            None => open_lock(&iroh.lock_path(&key)).map_err(Failure::local)?,
        };
        if matches!(FileExt::try_lock_exclusive(&file), Ok(true)) {
            lock = Some(file);
            break;
        }
        if Instant::now() >= deadline {
            let holder = holder_pid(&file);
            app.log.event(
                "iroh_link_busy",
                json!({ "link": short_id(&key), "holder_pid": holder }),
            );
            return Err(link_busy(holder, iroh.idle));
        }
        if !announced {
            announced = true;
            app.log.event(
                "iroh_link_wait",
                json!({ "link": short_id(&key), "holder_pid": holder_pid(&file) }),
            );
        }
        lock = Some(file);
        tokio::time::sleep(TURN_POLL).await;
    }
    let Some(mut file) = lock else {
        return Err(Failure::local("the link's lock was lost"));
    };
    write_holder(&mut file);
    if let Err(error) = connector.open_link(password).await {
        return Err(Failure::from_mobile(&error, false));
    }
    app.log.event(
        "iroh_link_open",
        json!({ "link": short_id(&key), "relays": relay_label(relay) }),
    );
    // Another call may have taken the link while this one opened it: join it.
    let mut held = iroh.held.borrow_mut();
    let entry = held.entry(key.clone()).or_insert_with(|| Held {
        _lock: file,
        connector: connector.clone(),
        password: Zeroizing::new(password.to_vec()),
        users: 0,
        generation: 0,
    });
    entry.users += 1;
    entry.generation += 1;
    Ok(LinkTurn {
        app: app.clone(),
        key,
        connector: entry.connector.clone(),
    })
}

/// Takes over the session a pairing over iroh just started (nobody else knows its password, so
/// its lock is free), and lets it go idle like any other.
pub fn adopt(app: &Rc<App>, password: &[u8], relay: &Option<String>) {
    let key = connection_id(password);
    let lock = open_lock(&app.iroh.lock_path(&key))
        .ok()
        .filter(|file| matches!(FileExt::try_lock_exclusive(file), Ok(true)));
    let (Some(mut lock), Ok(connector)) = (lock, app.iroh.connector(relay)) else {
        return;
    };
    write_holder(&mut lock);
    app.iroh.held.borrow_mut().insert(
        key.clone(),
        Held {
            _lock: lock,
            connector,
            password: Zeroizing::new(password.to_vec()),
            users: 1,
            generation: 0,
        },
    );
    release(app, &key);
}

/// Closes the link's session (if this process holds it) and forgets how it was paired.
pub async fn forget(app: &Rc<App>, password: &[u8]) {
    let _ = app.iroh.records.remove(password);
    let held = app.iroh.held.borrow_mut().remove(&connection_id(password));
    if let Some(held) = held {
        held.connector.close_link(password).await;
    }
}

/// One call is done with the link: when nobody uses it any more, it is closed after the idle
/// time unless something uses it again first.
fn release(app: &Rc<App>, key: &str) {
    let generation = {
        let mut held = app.iroh.held.borrow_mut();
        let Some(entry) = held.get_mut(key) else {
            return;
        };
        entry.users = entry.users.saturating_sub(1);
        if entry.users > 0 {
            return;
        }
        entry.generation += 1;
        entry.generation
    };
    let app = app.clone();
    let key = key.to_string();
    let idle = app.iroh.idle;
    tokio::task::spawn_local(async move {
        tokio::time::sleep(idle).await;
        let expired = {
            let mut held = app.iroh.held.borrow_mut();
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
            // Closed BEFORE the lock goes, so no other process starts a session for the link
            // while this one still holds its endpoint.
            held.connector.close_link(&held.password).await;
            app.log
                .event("iroh_link_released", json!({ "link": short_id(&key) }));
        }
    });
}

fn open_lock(path: &Path) -> Result<File, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Records this process as the link's holder, for the message another process shows.
fn write_holder(file: &mut File) {
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = write!(file, "{}", std::process::id());
}

fn holder_pid(file: &File) -> Option<u32> {
    let mut text = String::new();
    let mut file = file;
    let _ = file.rewind();
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
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

fn link_busy(holder: Option<u32>, idle: Duration) -> Failure {
    let who = holder.map_or_else(
        || "another radix-connector-mcp process".to_string(),
        |pid| format!("another radix-connector-mcp process (pid {pid})"),
    );
    Failure::new(
        "LINK_BUSY",
        "queue",
        true,
        format!("{who} holds this wallet's iroh link"),
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
