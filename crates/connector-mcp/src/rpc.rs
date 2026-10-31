//! MCP JSON-RPC 2.0 core: negotiates `initialize`, answers `tools/list` and
//! `tools/call`, and holds the shared application state. Transport framing lives
//! in `main.rs`; this module only understands MCP messages.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use radixdlt_connect_webrtc::Connector;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use zeroize::Zeroizing;

use crate::diag::Log;
use crate::iroh::{Iroh, LinkRecord, Setting};
use crate::outbox::Outbox;
use crate::store::Store;
use crate::tools;

/// Newest protocol revision we implement, plus the older ones we accept if a
/// client asks for them (we echo back whatever version the client requested when
/// it is one we know).
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub const SERVER_NAME: &str = "radix-connector";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

/// A pairing started by `pair_wallet` and awaiting the phone scan. The background
/// task writes its outcome into `result`; `pair_status` reads it.
pub struct Pending {
    pub result: Rc<RefCell<Option<Result<PairOutcome, String>>>>,
    pub label: Option<String>,
    /// When its QR was shown.
    pub started: Instant,
    /// The transports it pairs over.
    pub transports: Vec<&'static str>,
    /// The background task: a newer `pair_wallet` stops it (and with it every transport it was
    /// pairing over), as its QR is no longer shown.
    pub task: AbortHandle,
}

/// Successful pairing: the wallet's public key, the raw 32-byte link password, and how the link
/// was paired (recorded with it).
pub struct PairOutcome {
    pub wallet_public_key: String,
    pub password: Zeroizing<Vec<u8>>,
    pub paired_over: LinkRecord,
    /// The iroh relay the wallet is on (linked over iroh), kept with the link to knock there.
    pub wallet_relay: Option<String>,
}

/// How this connector process reaches wallets: from `--transport` / `--iroh-relay` or their
/// environment variables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    /// Default: [`Setting::Auto`].
    pub transport: Setting,
    /// A custom iroh relay; `None`: n0's public relays.
    pub iroh_relay: Option<String>,
}

/// A wallet request running in this process: how to stop it, and which wallet it went to.
pub struct InFlight {
    pub wallet: String,
    pub stop: oneshot::Sender<()>,
}

/// Shared, single-threaded application state. Wrapped in `Rc` and passed to every
/// tool handler. Interior mutability is fine because the whole server runs on one
/// thread; handlers must not hold a `RefCell` borrow across an `.await`.
pub struct App {
    config_path: PathBuf,
    pub pairing: RefCell<Option<Pending>>,
    /// Requests sent to a wallet and what became of them, shared with other connector processes.
    pub outbox: Outbox,
    pub log: Log,
    /// Wallet requests running in this process, by interaction id (what `cancel_request` stops).
    pub inflight: RefCell<HashMap<String, InFlight>>,
    /// Tool calls running in this process, by JSON-RPC id (what `notifications/cancelled` stops).
    calls: RefCell<HashMap<String, AbortHandle>>,
    /// dApp identities already verified, by (network, dApp definition, origin), and when.
    pub dapp_checks: RefCell<HashMap<(u8, String, String), Instant>>,
    /// The transport setting, and the iroh sessions of the links that use iroh.
    pub iroh: Iroh,
    /// Makes the WebRTC connector of every pairing and request: `Connector::new` (the public
    /// signaling server and the machine's relays). Tests point it at servers on 127.0.0.1.
    webrtc: Box<dyn Fn() -> Connector>,
    /// A newer release found at start, said once: in the next tool result (`update::announce`).
    pub update_notice: RefCell<Option<String>>,
}

impl App {
    pub fn new(options: Options) -> Result<Self, String> {
        let config_path = Store::default_path()?;
        Ok(App::at(config_path, options))
    }

    /// State kept beside `config_path` (`connector.json`).
    pub fn at(config_path: PathBuf, options: Options) -> Self {
        let dir = config_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let log = Log::new(dir.join("connector.log"));
        App {
            iroh: Iroh::new(options.transport, options.iroh_relay, &dir),
            outbox: Outbox::new(dir.join("requests.json")).with_log(log.clone()),
            log,
            config_path,
            pairing: RefCell::new(None),
            inflight: RefCell::new(HashMap::new()),
            calls: RefCell::new(HashMap::new()),
            dapp_checks: RefCell::new(HashMap::new()),
            webrtc: Box::new(Connector::new),
            update_notice: RefCell::new(None),
        }
    }

    /// Makes WebRTC connectors with `make` instead of `Connector::new`.
    #[cfg(test)]
    pub fn with_webrtc(mut self, make: impl Fn() -> Connector + 'static) -> Self {
        self.webrtc = Box::new(make);
        self
    }

    /// A WebRTC connector, for one pairing or request.
    pub fn webrtc_connector(&self) -> Connector {
        (self.webrtc)()
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.as_path()
    }

    /// Whether any tool call is still running.
    pub fn busy(&self) -> bool {
        !self.calls.borrow().is_empty()
    }
}

/// Handles one raw input line, writing whatever answers it to `out`.
///
/// Tool calls run as their own tasks, so a call waiting minutes for the phone does not stop the
/// server answering others — above all `cancel_request`, `pending_requests`, and the client's
/// `notifications/cancelled`, which aborts the call it names (and with it the channel to the
/// wallet). Everything else is answered in line.
pub fn dispatch(app: &Rc<App>, line: &str, out: &mpsc::UnboundedSender<String>) {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            let _ = out.send(error_json(Value::Null, PARSE_ERROR, "Body is not valid JSON"));
            return;
        }
    };
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();

    if method == "notifications/cancelled" {
        let target = message
            .get("params")
            .and_then(|p| p.get("requestId"))
            .map(Value::to_string)
            .unwrap_or_default();
        if let Some(call) = app.calls.borrow_mut().remove(&target) {
            app.log
                .event("call_cancelled_by_client", json!({ "rpc_id": target }));
            call.abort();
        }
        return;
    }
    if method == "tools/call" {
        let Some(id) = id else { return };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let key = id.to_string();
        let (task_app, task_out, task_key) = (app.clone(), out.clone(), key.clone());
        let task = tokio::task::spawn_local(async move {
            let started = Instant::now();
            task_app
                .log
                .event("tool_call", json!({ "rpc_id": task_key, "tool": name }));
            let mut result = tools::call(&task_app, &name, args).await;
            if let Some(notice) = task_app.update_notice.borrow_mut().take() {
                crate::update::add_notice(&mut result, &notice);
            }
            task_app.log.event(
                "tool_done",
                json!({
                    "rpc_id": task_key, "tool": name, "elapsed_ms": crate::iroh::ms(started.elapsed()),
                    "is_error": result["isError"].as_bool().unwrap_or(false),
                    "code": result["structuredContent"]["code"],
                }),
            );
            task_app.calls.borrow_mut().remove(&task_key);
            let _ = task_out.send(result_json(id, result));
        });
        app.calls.borrow_mut().insert(key, task.abort_handle());
        return;
    }
    if let Some(response) = handle_inline(&message, method, id) {
        let _ = out.send(response);
    }
}

/// Everything but tool calls: answered at once. `None` for notifications, which get no answer.
fn handle_inline(message: &Value, method: &str, id: Option<Value>) -> Option<String> {
    let is_notification = id.is_none();
    if method.is_empty() {
        if is_notification {
            return None;
        }
        return Some(error_json(
            id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            "Invalid JSON-RPC 2.0 request",
        ));
    }

    // Client-to-server notifications (initialized, …) get no response.
    if method.starts_with("notifications/") {
        return None;
    }

    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let id = id.unwrap_or(Value::Null);

    match method {
        "initialize" => Some(result_json(id, initialize_result(&params))),
        "ping" => Some(result_json(id, json!({}))),
        "tools/list" => Some(result_json(id, json!({ "tools": tools::list_json() }))),
        other => {
            if is_notification {
                None
            } else {
                Some(error_json(
                    id,
                    METHOD_NOT_FOUND,
                    &format!("Method not supported: {other}"),
                ))
            }
        }
    }
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": SERVER_NAME,
            "title": "Radix Connector (local signing)",
            "version": SERVER_VERSION,
        },
        "instructions": SERVER_INSTRUCTIONS,
    })
}

const SERVER_INSTRUCTIONS: &str = concat!(
    "Local Radix Connect signer. It pairs with a Radix Wallet on the user's phone and gets ",
    "transactions signed there — the private key never leaves the phone; this server only holds ",
    "the channel password. First-time use: call pair_wallet, show the returned QR to the user, ask ",
    "them to scan it from the Radix Wallet app (Settings > Linked Connectors), then call pair_status. ",
    "Any Radix Wallet pairs, with nothing to set up on the phone: the one QR is listened for over ",
    "WebRTC and Radix Connect over iroh at once, and the wallet links over iroh when it has iroh on ",
    "(Settings > Preferences > Radix Connect: Automatic or iroh), over WebRTC otherwise. Each ",
    "wallet keeps the transport it linked over (see list_wallets). ",
    "After that, use send_transaction / request_pre_authorization / request_account_proof with a ",
    "manifest (build and preview manifests with the radix-community HTTP MCP server first), or ",
    "request_login / request_authorized / request_data / request_ownership_proof for personas, ",
    "accounts and persona data. Every signing tool requires an explicit network ('mainnet' or ",
    "'stokenet'). The user always approves on their phone. The wallet shows ONE request at a time ",
    "and cannot withdraw one remotely: never resend a request that failed with retry_safe = NO. ",
    "Check pending_requests, collect late answers with await_response, clear with cancel_request. ",
    "Failures carry a code, a stage and a hint; trace any request with connector_log."
);

fn result_json(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_json(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}
