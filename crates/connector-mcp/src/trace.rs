//! The libraries' own events, in `connector.log`.
//!
//! The iroh protocol library (`radix_connect_iroh`) and the SDK's mobile connector
//! (`radixdlt_connect_iroh`) log through `tracing`. This layer writes their INFO, WARN and ERROR
//! events to the connector's log, beside its own, so a problem in the field can be followed from
//! one file (`connector_log`):
//!
//! - `link_collision`: another holder of the link's key took this end's place — at the relay or
//!   with the wallet. Two connectors on one link (the same `connector.json` on two machines, a
//!   copied state directory) show up as this. The diagnostics of every link this process holds are
//!   attached (`links`).
//! - `wallet_connection_replaced`: the wallet's connection was replaced by a newer one (the wallet
//!   reconnected before the old connection was seen to end), also with the links' diagnostics.
//! - `iroh_protocol` / `iroh_sdk`: everything else, with its level, message and fields. The SDK's
//!   `link` is the connector's link id; its `interaction` becomes `interaction_id`, so
//!   `connector_log { interaction_id }` shows both sides of a request.
//!
//! Neither library logs secrets or message contents. Fields that may hold a network address are
//! dropped here, and [`Log`] blanks IP addresses in whatever is left. The same line repeated within
//! [`REPEAT_WINDOW`] is counted instead of written (`suppressed` on the next one written).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

use crate::diag::Log;
use crate::iroh::{held_diagnostics, HeldLinks};

/// A line repeated within this window is counted, not written.
const REPEAT_WINDOW: Duration = Duration::from_secs(5);

/// Words in a library message that mean another holder of the link's key took this end's place.
const COLLISION_WORDS: &[&str] = &["displac", "another holder", "took over", "collision"];

/// Fields that may carry a network address: never logged.
const ADDRESS_FIELDS: &[&str] = &["addr", "remote_addr", "address", "ip", "socket", "local_addr"];

/// The `tracing` layer that writes the libraries' events to the connector's log.
pub struct Bridge {
    log: Log,
    links: HeldLinks,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// When each line was last written, and how many were left out since.
    written: HashMap<String, (Instant, u64)>,
    /// Whether a connection from each peer is being served (a second one replaces the first).
    serving: HashMap<String, bool>,
}

impl Bridge {
    pub fn new(log: Log, links: HeldLinks) -> Self {
        Bridge {
            log,
            links,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a connection from `peer` replaces one already served, keeping track as the
    /// protocol reports them.
    fn replaces(&self, message: &str, peer: Option<&str>) -> bool {
        let Some(peer) = peer else {
            return false;
        };
        let mut state = self.state();
        match message {
            // A new session serves nothing yet.
            "radix connect iroh session started" => {
                state.serving.insert(peer.to_string(), false);
                false
            }
            "connected to the peer" => state.serving.insert(peer.to_string(), true) == Some(true),
            _ => false,
        }
    }

    /// `Some(left out since the last one written)` when the line `key` is written now.
    fn due(&self, key: String) -> Option<u64> {
        let mut state = self.state();
        let now = Instant::now();
        match state.written.get_mut(&key) {
            Some((at, left_out)) if now.saturating_duration_since(*at) < REPEAT_WINDOW => {
                *left_out += 1;
                None
            }
            Some((at, left_out)) => {
                let suppressed = *left_out;
                *at = now;
                *left_out = 0;
                Some(suppressed)
            }
            None => {
                state.written.insert(key, (now, 0));
                Some(0)
            }
        }
    }

    fn forward(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut fields = Fields::default();
        event.record(&mut fields);
        let protocol = metadata.target().starts_with("radix_connect_iroh");
        let lower = fields.message.to_lowercase();
        let peer = fields.map.get("peer").and_then(Value::as_str);
        let name = if COLLISION_WORDS.iter().any(|word| lower.contains(word)) {
            "link_collision"
        } else if lower.contains("replaced") || (protocol && self.replaces(&fields.message, peer)) {
            "wallet_connection_replaced"
        } else if protocol {
            "iroh_protocol"
        } else {
            "iroh_sdk"
        };
        let key = format!(
            "{name}|{}|{}|{}",
            fields.message,
            fields.map.get("link").map(Value::to_string).unwrap_or_default(),
            fields
                .map
                .get("interaction_id")
                .map(Value::to_string)
                .unwrap_or_default(),
        );
        let Some(suppressed) = self.due(key) else {
            return;
        };
        let mut line = Map::new();
        line.insert("level".into(), metadata.level().as_str().to_lowercase().into());
        line.insert("target".into(), metadata.target().into());
        line.insert("message".into(), fields.message.into());
        line.extend(fields.map);
        if suppressed > 0 {
            line.insert("suppressed".into(), suppressed.into());
        }
        if matches!(name, "link_collision" | "wallet_connection_replaced") {
            line.insert("links".into(), held_diagnostics(&self.links).join(" | ").into());
        }
        self.log.event(name, Value::Object(line));
    }
}

/// Whether an event is one the log takes: the two libraries', INFO and above.
fn wanted(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    (target.starts_with("radix_connect_iroh") || target.starts_with("radixdlt_connect_iroh"))
        && *metadata.level() <= Level::INFO
}

impl<S: Subscriber> Layer<S> for Bridge {
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        wanted(metadata)
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if wanted(event.metadata()) {
            self.forward(event);
        }
    }
}

/// An event's message and fields, as JSON.
#[derive(Default)]
struct Fields {
    message: String,
    map: Map<String, Value>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: Value) {
        let name = match field.name() {
            "message" => {
                self.message = match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                };
                return;
            }
            name if ADDRESS_FIELDS.contains(&name) => return,
            "interaction" => "interaction_id",
            name => name,
        };
        self.map.insert(name.to_string(), value);
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, format!("{value:?}").into());
    }
}

/// Writes the libraries' events to `log` for the rest of the process.
pub fn install(log: Log, links: HeldLinks) {
    let subscriber = tracing_subscriber::registry().with(Bridge::new(log, links));
    // Only fails if one is installed already, which then keeps logging.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// [`install`], for this thread only, until the guard is dropped.
///
/// Best effort where other threads log at the same time (parallel tests): `tracing` caches, per
/// call site and for the whole process, whether anyone listens, and a call site first reached on
/// another thread may keep saying no. The process-wide [`install`] has no such gap.
#[cfg(test)]
pub fn scoped(log: Log, links: HeldLinks) -> tracing::subscriber::DefaultGuard {
    let guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(Bridge::new(log, links)));
    tracing::callsite::rebuild_interest_cache();
    guard
}
