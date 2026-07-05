//! MCP JSON-RPC 2.0 core: negotiates `initialize`, answers `tools/list` and
//! `tools/call`, and holds the shared application state. Transport framing lives
//! in `main.rs`; this module only understands MCP messages.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{json, Value};

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
}

/// Successful pairing: the wallet's public key and the raw 32-byte link password.
pub struct PairOutcome {
    pub wallet_public_key: String,
    pub password: Vec<u8>,
}

/// Shared, single-threaded application state. Wrapped in `Rc` and passed to every
/// tool handler. Interior mutability is fine because the whole server runs on one
/// thread; handlers must not hold a `RefCell` borrow across an `.await`.
pub struct App {
    config_path: PathBuf,
    pub pairing: RefCell<Option<Pending>>,
}

impl App {
    pub fn new() -> Result<Self, String> {
        let config_path = Store::default_path()?;
        Ok(App {
            config_path,
            pairing: RefCell::new(None),
        })
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.as_path()
    }
}

/// Handles one raw input line. Returns the response JSON string to write back, or
/// `None` for notifications (which must not be answered) and unparseable input we
/// choose to drop.
pub async fn handle_line(app: &Rc<App>, line: &str) -> Option<String> {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            return Some(error_json(
                Value::Null,
                PARSE_ERROR,
                "Body is not valid JSON",
            ))
        }
    };

    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").cloned();
    let is_notification = message.get("id").is_none();

    let Some(method) = method else {
        if is_notification {
            return None;
        }
        return Some(error_json(
            id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            "Invalid JSON-RPC 2.0 request",
        ));
    };

    // Client-to-server notifications (initialized, cancelled, …) get no response.
    if method.starts_with("notifications/") {
        return None;
    }

    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let id = id.unwrap_or(Value::Null);

    match method {
        "initialize" => Some(result_json(id, initialize_result(&params))),
        "ping" => Some(result_json(id, json!({}))),
        "tools/list" => Some(result_json(id, json!({ "tools": tools::list_json() }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let result = tools::call(app, name, args).await;
            Some(result_json(id, result))
        }
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
    "After that, use send_transaction / request_pre_authorization / request_account_proof with a ",
    "manifest (build and preview manifests with the radix-community HTTP MCP server first). Every ",
    "signing tool requires an explicit network ('mainnet' or 'stokenet'). The user always approves ",
    "on their phone."
);

fn result_json(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_json(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}
