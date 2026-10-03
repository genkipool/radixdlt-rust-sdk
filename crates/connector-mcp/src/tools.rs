//! The MCP tools this server exposes, and their handlers. Each signing tool maps
//! to a `radixdlt-connect` call that opens a Radix Connect channel to the paired
//! phone; the user approves there. Pairing is split into `pair_wallet` (returns
//! the QR immediately, starts the handshake in the background) and `pair_status`
//! (completes it) because a single blocking call could never show the QR before
//! it needs to be scanned.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::oneshot;

use radixdlt_connect::crypto::blake2b_256;
use radixdlt_connect::state::Link;
use radixdlt_connect::{
    account_proof_request_sharing, account_request, authorized_request, extract_accounts,
    extract_persona_email, extract_persona_name, extract_proofs, extract_signed_partial_transaction,
    extract_transaction_intent_hash, pre_authorization_request, transaction_request, unauthorized_request,
    Auth, AuthorizedRequest, Connector, OwnershipWanted, PersonaRequest,
};
use radixdlt_rola::{verify_account_proof, AccountProof};

use crate::diag::Failure;
use crate::exchange::{self, failed, interact, manifest_summary, target, Ask, Target};
use crate::gateway;
use crate::requests::{
    check_challenge, parse_accounts, parse_persona_data, random_challenge, render_answer, Verifier,
};
use crate::rpc::{App, PairOutcome, Pending};
use crate::store::{now_unix_seconds, Store};

/// Origin advertised to the wallet when neither the call nor the
/// `RADIX_DAPP_ORIGIN` env var set one. Must match the `claimed_websites`
/// metadata of the dApp definition on-chain, or the wallet shows the request
/// as unverified (and ROLA verification fails).
const DEFAULT_ORIGIN: &str = "https://radix-community.genkipool.com";
/// Default and maximum wallet-approval timeouts (seconds).
const DEFAULT_SIGN_TIMEOUT: u64 = 300;
const MAX_TIMEOUT: u64 = 900;
/// How long to wait for the background pairing task to hand us the QR string.
const QR_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// The two Radix networks this connector talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Stokenet,
}

impl Network {
    pub fn parse(s: &str) -> Result<Network, String> {
        match s {
            "mainnet" => Ok(Network::Mainnet),
            "stokenet" => Ok(Network::Stokenet),
            other => Err(format!(
                "invalid network \"{other}\" — use \"mainnet\" or \"stokenet\""
            )),
        }
    }

    pub fn id(self) -> u8 {
        match self {
            Network::Mainnet => 1,
            Network::Stokenet => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Stokenet => "stokenet",
        }
    }
}

/* ───────────────────────────── result plumbing ─────────────────────────── */

enum Content {
    Text(String),
    Image { data: String, mime: String },
}

/// The MCP `tools/call` result for one tool invocation.
pub struct ToolResult {
    content: Vec<Content>,
    is_error: bool,
    /// Machine-readable form of a failure (`code`, `stage`, `retry_safe`, `hint`, …).
    structured: Option<Value>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        ToolResult {
            content: vec![Content::Text(text.into())],
            is_error: false,
            structured: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolResult {
            content: vec![Content::Text(text.into())],
            is_error: true,
            structured: None,
        }
    }

    /// A failure with its machine-readable form.
    pub fn failure(text: impl Into<String>, structured: Value) -> Self {
        ToolResult {
            content: vec![Content::Text(text.into())],
            is_error: true,
            structured: Some(structured),
        }
    }

    /// Appends to the first text block.
    pub fn push_text(&mut self, more: impl AsRef<str>) {
        if let Some(Content::Text(text)) = self.content.first_mut() {
            text.push_str(more.as_ref());
        } else {
            self.content.push(Content::Text(more.as_ref().to_string()));
        }
    }

    fn with_image(mut self, data: String, mime: impl Into<String>) -> Self {
        self.content.push(Content::Image {
            data,
            mime: mime.into(),
        });
        self
    }

    fn to_json(&self) -> Value {
        let content: Vec<Value> = self
            .content
            .iter()
            .map(|block| match block {
                Content::Text(text) => json!({ "type": "text", "text": text }),
                Content::Image { data, mime } => {
                    json!({ "type": "image", "data": data, "mimeType": mime })
                }
            })
            .collect();
        let mut result = json!({ "content": content, "isError": self.is_error });
        if let Some(structured) = &self.structured {
            result["structuredContent"] = structured.clone();
        }
        result
    }
}

/* ─────────────────────────────── registry ──────────────────────────────── */

const NETWORK_PROP: &str = "Radix network: \"mainnet\" (real funds) or \"stokenet\" (testnet). Required — there is no default, on purpose.";

/// Properties every request to the wallet takes.
fn wallet_request_props() -> Value {
    json!({
        "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP },
        "dapp_definition": { "type": "string", "description": "dApp definition address shown to the wallet (falls back to the RADIX_DAPP_DEFINITION_MAINNET/STOKENET env var). The wallet REFUSES a request without one, and DROPS one whose origin does not vouch for it — checked before sending." },
        "origin": { "type": "string", "description": "Origin URL shown to the wallet (default: RADIX_DAPP_ORIGIN env var, else https://radix-community.genkipool.com). Must be claimed by the dApp definition and list it in /.well-known/radix.json." },
        "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the first paired wallet)." },
        "timeout_seconds": { "type": "integer", "description": "How long to wait for approval (default 300, max 900)." },
        "ignore_pending": { "type": "boolean", "description": "Send even though an earlier request may still be waiting in the wallet (default false). The wallet shows one request at a time: only use it when you are sure nothing is waiting." },
        "skip_dapp_check": { "type": "boolean", "description": "Skip checking the dApp identity before sending (default false) — e.g. when the wallet has developer mode on." }
    })
}

/// A schema of `own` properties plus the common wallet-request ones.
fn request_schema(own: Value, required: &[&str]) -> Value {
    let mut properties = wallet_request_props();
    if let (Some(all), Value::Object(own)) = (properties.as_object_mut(), own) {
        all.extend(own);
    }
    let mut required: Vec<&str> = required.to_vec();
    required.push("network");
    json!({ "type": "object", "properties": properties, "required": required })
}

const ACCOUNTS_PROP: &str = "Accounts to ask for: a number (at least N), or { \"quantity\": N, \"exactly\": true|false, \"with_proof\": true|false }. With with_proof each account signs the challenge (ROLA), verified here.";
const PERSONA_DATA_PROP: &str = "Persona data to ask for: { \"name\": true, \"emails\": N | {quantity, exactly}, \"phones\": N | {quantity, exactly} }. One-time data is MANDATORY for whoever answers: the wallet will not let them approve without it, so ask only for what is needed.";
const CHALLENGE_PROP: &str = "ROLA challenge, 32 bytes as hex. Omit it and the connector makes a fresh one and verifies the proofs itself; pass yours when a server will verify them.";

/// The `tools/list` payload, hand-built as JSON Schema so the whole binary stays
/// dependency-light.
pub fn list_json() -> Vec<Value> {
    vec![
        tool(
            "pair_wallet",
            "Pair a Radix Wallet",
            "Starts pairing with a Radix Wallet and returns a QR code (as a terminal drawing, a PNG image, and the raw payload). Show it to the user and ask them to scan it from the Radix Wallet app: Settings > Linked Connectors > Link New Connector. Then call pair_status. Only needed once per device.",
            true,
            json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "Optional human label for this device, e.g. \"my phone\"." }
                }
            }),
        ),
        tool(
            "pair_status",
            "Finish/inspect pairing",
            "Completes a pairing started by pair_wallet: waits up to `wait_seconds` for the user to scan the QR and approve on their phone, then saves the link. Call it after showing the QR; call again if it reports it is still waiting.",
            true,
            json!({
                "type": "object",
                "properties": {
                    "wait_seconds": { "type": "integer", "description": "How long to wait for the scan before returning (default 120, max 900)." }
                }
            }),
        ),
        tool(
            "list_wallets",
            "List paired wallets",
            "Lists the wallets currently paired with this connector (label, public key, when linked). Use the public key as `wallet_public_key` in the signing tools to target a specific device.",
            true,
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "remove_wallet",
            "Remove a paired wallet",
            "Removes a paired wallet by its public key (from list_wallets). The user can re-pair later with pair_wallet.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "wallet_public_key": { "type": "string", "description": "Public key of the wallet to remove (see list_wallets)." }
                },
                "required": ["wallet_public_key"]
            }),
        ),
        tool(
            "send_transaction",
            "Send a transaction to sign",
            "Sends a transaction manifest to the paired wallet to sign AND submit. The user approves on their phone. Returns the transaction intent hash; confirm the commit with transaction_status. Build and preview the manifest with the radix-community HTTP MCP server first. Refuses to send while an earlier request may still be waiting in the wallet (see pending_requests).",
            false,
            request_schema(
                json!({
                    "manifest": { "type": "string", "description": "The transaction manifest (RTM text) to sign and submit. Never include lock_fee: the wallet adds its own." },
                    "message": { "type": "string", "description": "Optional transaction message shown to the user in the wallet." },
                    "blobs": { "type": "array", "items": { "type": "string" }, "description": "Hex-encoded blobs referenced by the manifest via Blob(\"<hash>\") (optional)." },
                    "blob_files": { "type": "array", "items": { "type": "string" }, "description": "Paths to binary files read locally and attached as blobs — use for large payloads like package WASM (optional)." }
                }),
                &["manifest"],
            ),
        ),
        tool(
            "deploy_package",
            "Deploy a Scrypto package",
            "Publishes a Scrypto package to the network. Reads the compiled .wasm from a LOCAL file path (it never travels through the agent), dry-runs it on the Gateway, attaches it as a blob, and signs+submits via the paired wallet. Get `package_definition` by decoding the .rpd with the radix-community HTTP MCP server's build_deploy_package_manifest tool first.",
            false,
            request_schema(
                json!({
                    "wasm_path": { "type": "string", "description": "Local filesystem path to the compiled package .wasm." },
                    "package_definition": { "type": "string", "description": "Package definition in manifest (SBOR) syntax — the decoded .rpd, from build_deploy_package_manifest." },
                    "owner_role": { "type": "string", "description": "OwnerRole in manifest syntax (default \"None\" — no owner). Supply a richer value for badge-controlled packages." }
                }),
                &["wasm_path", "package_definition"],
            ),
        ),
        tool(
            "request_pre_authorization",
            "Request a pre-authorization (subintent)",
            "Asks the wallet to sign a subintent (pre-authorization, transaction V2) WITHOUT submitting it. Returns the signed partial transaction as hex, to be combined into a larger transaction later.",
            false,
            request_schema(
                json!({
                    "subintent_manifest": { "type": "string", "description": "The subintent manifest to pre-authorize." },
                    "expire_after_seconds": { "type": "integer", "description": "How long the pre-authorization stays valid, in seconds." },
                    "message": { "type": "string", "description": "Optional message shown to the user in the wallet." }
                }),
                &["subintent_manifest", "expire_after_seconds"],
            ),
        ),
        tool(
            "request_accounts",
            "Get the user's account address(es)",
            "Asks the wallet to SHARE its account address(es) WITHOUT a signature (lightweight, no ROLA proof). Use it to learn which account to fund, transfer from, or set as fee payer before building a manifest. The user approves the share on their phone. For exact quantities, proofs or persona data use request_data.",
            false,
            request_schema(json!({}), &[]),
        ),
        tool(
            "request_account_proof",
            "Request a ROLA account proof (log in with Radix)",
            "Asks the wallet to sign a ROLA challenge with an account. Returns the account address and whether the proof verified locally. `dapp_definition` and `origin` MUST match the values the verifier expects, because they are part of the signed message. To prove WHO the person is (their persona), use request_login.",
            false,
            request_schema(
                json!({
                    "challenge": { "type": "string", "description": "ROLA challenge as hex (32 bytes)." },
                    "request_persona": { "type": "boolean", "description": "Also ask for the persona name (default false)." },
                    "request_email": { "type": "boolean", "description": "Also ask for the persona name and email address (default false). The wallet will not let the person approve until they provide what is asked for, so ask only when the answer needs it." }
                }),
                &["challenge"],
            ),
        ),
        tool(
            "request_login",
            "Log in with a persona",
            "Asks the wallet to LOG IN to the dApp with a persona (authorized request). With a challenge (default) the persona signs it, so the person is PROVEN — the answer carries the identity address and a proof, verified here; with without_challenge: true the persona is only named. Can ask in the same approval for accounts (optionally with proofs) and persona data (name, emails, phones). Use the identity it returns with request_ownership_proof / request_authorized (use_persona).",
            false,
            request_schema(
                json!({
                    "challenge": { "type": "string", "description": CHALLENGE_PROP },
                    "without_challenge": { "type": "boolean", "description": "Log in without a proof: the persona is named, not proven (default false)." },
                    "accounts": { "description": ACCOUNTS_PROP },
                    "persona_data": { "type": "object", "description": PERSONA_DATA_PROP }
                }),
                &[],
            ),
        ),
        tool(
            "request_ownership_proof",
            "Prove exact accounts / persona",
            "Asks the wallet to prove ownership of EXACT accounts (and optionally the persona) as a persona already logged in to this dApp: one confirmation, nothing for the person to pick. Each proof signs the challenge and is verified here. Requires the persona's identity address (from request_login).",
            false,
            request_schema(
                json!({
                    "identity_address": { "type": "string", "description": "The persona (identity_…) already logged in to this dApp." },
                    "accounts": { "type": "array", "items": { "type": "string" }, "description": "Account addresses to prove (may be empty when prove_persona is true)." },
                    "prove_persona": { "type": "boolean", "description": "Also prove the persona itself (default false)." },
                    "challenge": { "type": "string", "description": CHALLENGE_PROP }
                }),
                &["identity_address"],
            ),
        ),
        tool(
            "request_authorized",
            "Authorized request (full)",
            "The whole authorized-request vocabulary in one approval: auth = \"login\" (persona signs the challenge), \"login_without_challenge\", or \"use_persona\" (act as identity_address, already logged in — no new login); reset of what was shared before; proof of ownership of exact accounts / the persona; accounts and persona data ONE-TIME (this request only) or ONGOING (the wallet remembers them for this dApp and stops asking). Every proof is verified here.",
            false,
            request_schema(
                json!({
                    "auth": { "type": "string", "enum": ["login", "login_without_challenge", "use_persona"], "description": "Which persona: log in (with proof), log in without proof, or use one already logged in (needs identity_address)." },
                    "identity_address": { "type": "string", "description": "The persona for auth = use_persona (and for prove_persona)." },
                    "challenge": { "type": "string", "description": CHALLENGE_PROP },
                    "reset_accounts": { "type": "boolean", "description": "Forget the accounts shared ongoing before, so the person picks again (default false)." },
                    "reset_persona_data": { "type": "boolean", "description": "Forget the persona data shared ongoing before (default false)." },
                    "prove_accounts": { "type": "array", "items": { "type": "string" }, "description": "Exact accounts to prove ownership of." },
                    "prove_persona": { "type": "boolean", "description": "Prove the persona too (needs auth = use_persona)." },
                    "one_time_accounts": { "description": ACCOUNTS_PROP },
                    "ongoing_accounts": { "description": ACCOUNTS_PROP },
                    "one_time_persona_data": { "type": "object", "description": PERSONA_DATA_PROP },
                    "ongoing_persona_data": { "type": "object", "description": "Persona data shared ongoing, same shape as one_time_persona_data." }
                }),
                &["auth"],
            ),
        ),
        tool(
            "request_data",
            "Ask for accounts / persona data (no login)",
            "An unauthorized request (no persona login): accounts with an exact or minimum quantity, optionally each proving ownership (ROLA, verified here), and/or one-time persona data — name, email addresses, PHONE NUMBERS.",
            false,
            request_schema(
                json!({
                    "accounts": { "description": ACCOUNTS_PROP },
                    "persona_data": { "type": "object", "description": PERSONA_DATA_PROP },
                    "challenge": { "type": "string", "description": CHALLENGE_PROP }
                }),
                &[],
            ),
        ),
        tool(
            "pending_requests",
            "Requests waiting in the wallet",
            "Lists the requests this connector sent that may still be waiting in the wallet's queue (delivered, nobody answered), with what each was and what to do. The wallet shows ONE request at a time and nothing withdraws one remotely, so check this before sending more; history: true also lists recent answered ones.",
            true,
            json!({
                "type": "object",
                "properties": {
                    "wallet_public_key": { "type": "string", "description": "Only this wallet (default: all)." },
                    "history": { "type": "boolean", "description": "Also list recent requests and their outcomes (default false)." },
                    "limit": { "type": "integer", "description": "How many recent requests with history (default 10)." }
                }
            }),
        ),
        tool(
            "await_response",
            "Collect a late answer",
            "Waits for the answer to a request sent EARLIER (one that timed out with NO_ANSWER, or was delivered and is still pending), WITHOUT sending it again — re-sending would queue a second copy. Ask the person to answer it on the phone, then call this. Default: the most recent pending request.",
            true,
            json!({
                "type": "object",
                "properties": {
                    "interaction_id": { "type": "string", "description": "The request to wait for (from the failure or pending_requests)." },
                    "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the request's)." },
                    "timeout_seconds": { "type": "integer", "description": "How long to wait (default 120, max 900)." }
                }
            }),
        ),
        tool(
            "cancel_request",
            "Cancel a request",
            "Stops a request: if its call is still waiting here it is stopped (closing the channel), and it no longer blocks new requests. The wallet offers NO remote cancel — a request it already received stays on the phone until the person rejects it (or force-closes the app); the result says which case applies.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "interaction_id": { "type": "string", "description": "The request to cancel." },
                    "all": { "type": "boolean", "description": "Cancel every pending request (of wallet_public_key, or all wallets)." },
                    "wallet_public_key": { "type": "string", "description": "With all: only this wallet." }
                }
            }),
        ),
        tool(
            "check_wallet_connection",
            "Is the wallet reachable?",
            "Opens a channel to the paired wallet and closes it, sending nothing (nothing appears on the phone): says whether the Radix Wallet app is reachable right now and how long the channel took. Use it to tell «phone offline / app closed» apart from «request not answered».",
            true,
            json!({
                "type": "object",
                "properties": {
                    "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the first paired wallet)." },
                    "timeout_seconds": { "type": "integer", "description": "How long to wait for the wallet (default 30)." }
                }
            }),
        ),
        tool(
            "check_dapp_identity",
            "Check the dApp identity",
            "Runs the check the wallet runs on every request: the dApp definition is a 'dapp definition' account on this network, claims the origin, and {origin}/.well-known/radix.json lists it. When the last link is missing the wallet DROPS requests without answering — they look stuck. Sending tools run this automatically.",
            true,
            json!({
                "type": "object",
                "properties": {
                    "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP },
                    "dapp_definition": { "type": "string", "description": "dApp definition address (default: the env var for the network)." },
                    "origin": { "type": "string", "description": "Origin URL (default: RADIX_DAPP_ORIGIN, else https://radix-community.genkipool.com)." }
                },
                "required": ["network"]
            }),
        ),
        tool(
            "connector_log",
            "Read the connector log",
            "The connector's own trace: every step of every request (sent, delivered, answered, failed with code/stage, late answers, cancellations), oldest first. Filter by interaction_id to follow one request end to end.",
            true,
            json!({
                "type": "object",
                "properties": {
                    "interaction_id": { "type": "string", "description": "Only events about this request." },
                    "limit": { "type": "integer", "description": "How many of the latest events (default 40, max 500)." }
                }
            }),
        ),
        tool(
            "check_update",
            "Is there a newer connector?",
            "Checks GitHub for the newest radix-connector-mcp release and compares it with this one. Read-only. Updating never requires pairing the phone again.",
            true,
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "update_connector",
            "Update the connector",
            "Downloads the newest radix-connector-mcp release for this platform, verifies its SHA-256 and that it runs, keeps a backup, and installs it in place of this binary. The pairing with the phone is kept. The MCP client must be restarted to launch the new version.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "tag": { "type": "string", "description": "A specific release (connector-vX.Y.Z) instead of the newest." },
                    "force": { "type": "boolean", "description": "Reinstall even when already up to date (default false)." }
                }
            }),
        ),
        tool(
            "transaction_status",
            "Check a transaction status",
            "Reads the current status of a transaction from the Radix Gateway by its intent hash (txid_...). Read-only; no signing. Use it after send_transaction to confirm the commit.",
            true,
            json!({
                "type": "object",
                "properties": {
                    "intent_hash": { "type": "string", "description": "Transaction intent hash (txid_...)." },
                    "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP }
                },
                "required": ["intent_hash", "network"]
            }),
        ),
    ]
}

fn tool(name: &str, title: &str, description: &str, read_only: bool, schema: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": schema,
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": !read_only,
            "openWorldHint": true,
        }
    })
}

/* ─────────────────────────────── dispatch ──────────────────────────────── */

/// Runs one tool. Never panics — failures come back as `isError` results.
pub async fn call(app: &Rc<App>, name: &str, args: Value) -> Value {
    let result = match name {
        "pair_wallet" => pair_wallet(app, &args).await,
        "pair_status" => pair_status(app, &args).await,
        "list_wallets" => list_wallets(app),
        "remove_wallet" => remove_wallet(app, &args),
        "request_accounts" => request_accounts(app, &args).await,
        "send_transaction" => send_transaction(app, &args).await,
        "deploy_package" => deploy_package(app, &args).await,
        "request_pre_authorization" => request_pre_authorization(app, &args).await,
        "request_account_proof" => request_account_proof(app, &args).await,
        "request_login" => request_login(app, &args).await,
        "request_ownership_proof" => request_ownership_proof(app, &args).await,
        "request_authorized" => request_authorized_tool(app, &args).await,
        "request_data" => request_data(app, &args).await,
        "pending_requests" => exchange::pending_requests(app, &args),
        "await_response" => exchange::await_response(app, &args).await,
        "cancel_request" => exchange::cancel_request(app, &args),
        "check_wallet_connection" => exchange::check_wallet_connection(app, &args).await,
        "check_dapp_identity" => exchange::check_dapp_identity(&args).await,
        "connector_log" => exchange::connector_log(app, &args),
        "transaction_status" => transaction_status(&args).await,
        "check_update" => match crate::update::check().await {
            Ok(found) => ToolResult::text(found.describe()),
            Err(e) => ToolResult::error(format!("could not check for updates: {e}")),
        },
        "update_connector" => match crate::update::update(
            opt_str(&args, "tag").as_deref(),
            opt_bool(&args, "force").unwrap_or(false),
        )
        .await
        {
            Ok(report) => ToolResult::text(report),
            Err(e) => ToolResult::error(format!("update failed: {e}")),
        },
        other => ToolResult::error(format!(
            "Unknown tool \"{other}\". Call tools/list to see the available tools."
        )),
    };
    result.to_json()
}

/* ──────────────────────────────── handlers ─────────────────────────────── */

async fn pair_wallet(app: &Rc<App>, args: &Value) -> ToolResult {
    let label = opt_str(args, "label");

    let state = match Store::load_or_init(app.config_path()) {
        Ok(state) => state,
        Err(e) => return ToolResult::error(format!("could not open the connector state: {e}")),
    };
    let priv_hex = state.identity.private_key.clone();
    let pub_hex = state.identity.public_key.clone();

    let (qr_tx, qr_rx) = oneshot::channel::<String>();
    let result_slot: Rc<RefCell<Option<Result<PairOutcome, String>>>> = Rc::new(RefCell::new(None));
    let task_slot = result_slot.clone();

    // Run the (blocking-until-scanned) Radix Connect handshake in the background.
    // `pair` invokes the callback with the QR payload BEFORE it starts waiting, so
    // we get the QR back immediately over the oneshot channel.
    tokio::task::spawn_local(async move {
        let connector = Connector::new();
        let outcome = connector
            .pair(
                &priv_hex,
                &pub_hex,
                move |qr| {
                    let _ = qr_tx.send(qr);
                },
                Duration::from_secs(600),
            )
            .await;
        *task_slot.borrow_mut() = Some(
            outcome
                .map(|(wallet_public_key, password)| PairOutcome {
                    wallet_public_key,
                    password,
                })
                .map_err(|e| e.to_string()),
        );
    });

    let payload = match tokio::time::timeout(QR_READY_TIMEOUT, qr_rx).await {
        Ok(Ok(payload)) => payload,
        _ => {
            return ToolResult::error("could not start pairing (the QR payload was not produced). Try again.")
        }
    };

    let rendered = match crate::qr::render(&payload) {
        Ok(rendered) => rendered,
        Err(e) => return ToolResult::error(e),
    };

    *app.pairing.borrow_mut() = Some(Pending {
        result: result_slot,
        label,
    });

    let text = format!(
        "PAIR A RADIX WALLET\n\
         Show this QR to the user and ask them to scan it from the Radix Wallet app:\n\
         Settings > Linked Connectors > Link New Connector.\n\
         Then call `pair_status` to finish (it waits for the scan + approval).\n\n\
         {unicode}\n\
         If the terminal QR does not scan (dark themes can invert it), use the PNG image\n\
         in this result, or paste the raw payload below into a LOCAL QR generator:\n\n\
         ```json\n{payload}\n```",
        unicode = rendered.unicode,
        payload = payload,
    );

    ToolResult::text(text).with_image(rendered.png_base64, "image/png")
}

async fn pair_status(app: &Rc<App>, args: &Value) -> ToolResult {
    let wait_seconds = clamp_timeout(opt_u64(args, "wait_seconds").unwrap_or(120));

    // Grab the shared slot + label without holding the borrow across awaits.
    let (slot, label) = {
        let pending = app.pairing.borrow();
        match pending.as_ref() {
            Some(p) => (p.result.clone(), p.label.clone()),
            None => {
                return ToolResult::error(
                    "no pairing in progress. Call pair_wallet first, show the QR, then call pair_status.",
                )
            }
        }
    };

    let deadline = Instant::now() + Duration::from_secs(wait_seconds);
    loop {
        if let Some(outcome) = slot.borrow_mut().take() {
            *app.pairing.borrow_mut() = None; // pairing finished, clear it
            return match outcome {
                Ok(outcome) => finish_pairing(app, outcome, label),
                Err(e) => ToolResult::error(format!(
                    "pairing failed: {e}\nMake sure the Radix Wallet app is open and try pair_wallet again."
                )),
            };
        }
        if Instant::now() >= deadline {
            return ToolResult::text(
                "Still waiting for the wallet to scan the QR and approve. Show the QR to the user (from pair_wallet) and call pair_status again.",
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn finish_pairing(app: &Rc<App>, outcome: PairOutcome, label: Option<String>) -> ToolResult {
    let mut state = match Store::load_or_init(app.config_path()) {
        Ok(state) => state,
        Err(e) => return ToolResult::error(format!("paired, but could not open the state file: {e}")),
    };
    state.add_or_replace_link(Link {
        password: hex::encode(&outcome.password),
        wallet_public_key: outcome.wallet_public_key.clone(),
        linked_at: now_unix_seconds(),
        label: label.clone(),
    });
    if let Err(e) = Store::save(app.config_path(), &state) {
        return ToolResult::error(format!("paired, but could not save the link: {e}"));
    }
    ToolResult::text(format!(
        "WALLET PAIRED ✓\n\
         Label:      {label}\n\
         Public key: {pk}\n\
         Saved to:   {path}\n\n\
         You can now sign with send_transaction / request_pre_authorization / request_account_proof.",
        label = label.as_deref().unwrap_or("(none)"),
        pk = outcome.wallet_public_key,
        path = app.config_path().display(),
    ))
}

fn list_wallets(app: &Rc<App>) -> ToolResult {
    let state = match Store::load_or_init(app.config_path()) {
        Ok(state) => state,
        Err(e) => return ToolResult::error(format!("could not open the connector state: {e}")),
    };
    let links = state.all_links();
    if links.is_empty() {
        return ToolResult::text(
            "No wallets paired yet. Call pair_wallet to link one (needed once per device).",
        );
    }
    let mut out = String::from("PAIRED WALLETS\n");
    for (i, link) in links.iter().enumerate() {
        out.push_str(&format!(
            "{n}. {label}\n   public key: {pk}\n   linked at:  {at} (unix seconds)\n",
            n = i + 1,
            label = link.label.as_deref().unwrap_or("(no label)"),
            pk = link.wallet_public_key,
            at = link.linked_at,
        ));
    }
    ToolResult::text(out)
}

fn remove_wallet(app: &Rc<App>, args: &Value) -> ToolResult {
    let pk = match req_str(args, "wallet_public_key") {
        Ok(pk) => pk,
        Err(e) => return ToolResult::error(e),
    };
    let mut state = match Store::load_or_init(app.config_path()) {
        Ok(state) => state,
        Err(e) => return ToolResult::error(format!("could not open the connector state: {e}")),
    };
    if !state.remove_link(&pk) {
        return ToolResult::error(format!("no paired wallet with public key {pk}."));
    }
    if let Err(e) = Store::save(app.config_path(), &state) {
        return ToolResult::error(format!("could not save the state: {e}"));
    }
    ToolResult::text(format!("Removed the paired wallet {pk}."))
}

async fn request_accounts(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "request_accounts";
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let ask = Ask {
        tool: TOOL,
        kind: "accounts",
        summary: "share account address(es), no proof".to_string(),
        interaction: account_request(&target.ctx),
    };
    let (reply, result) = interact(app, args, &target, ask).await;
    let response = match result {
        Ok(v) => v,
        Err(f) => return failed(app, TOOL, &f, Some(&reply)),
    };
    let accounts = match extract_accounts(&response) {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => {
            return failed(
                app,
                TOOL,
                &no_content("the wallet shared no accounts", &reply),
                Some(&reply),
            )
        }
        Err(e) => return failed(app, TOOL, &no_content(&e.to_string(), &reply), Some(&reply)),
    };

    let mut out = format!(
        "ACCOUNTS SHARED ✓ (network: {net})\n",
        net = target.network.label()
    );
    for (i, (address, label)) in accounts.iter().enumerate() {
        out.push_str(&format!(
            "{n}. {address}  [{label}]\n",
            n = i + 1,
            label = label.as_deref().unwrap_or("no label"),
        ));
    }
    out.push_str(&format!("Interaction: {}", reply.interaction_id));
    reply.annotate(ToolResult::text(out))
}

/// An answer that came back without what was asked for.
fn no_content(detail: &str, reply: &exchange::Reply) -> Failure {
    Failure::new(
        "UNEXPECTED_ANSWER",
        "wallet",
        true,
        detail.to_string(),
        "The wallet answered, but not with what was asked. Read the detail; trace it with connector_log.",
    )
    .with_interaction(&reply.interaction_id)
}

/// Signs + submits a manifest (with optional blobs) via the paired wallet.
/// Shared by `send_transaction` and `deploy_package`.
async fn submit_transaction(
    app: &Rc<App>,
    tool_name: &str,
    args: &Value,
    target: &Target,
    manifest: &str,
    message: &str,
    blobs: &[String],
) -> ToolResult {
    let ask = Ask {
        tool: tool_name,
        kind: "transaction",
        summary: manifest_summary(manifest, message),
        interaction: transaction_request(manifest, message, blobs, &target.ctx),
    };
    let (reply, result) = interact(app, args, target, ask).await;
    let response = match result {
        Ok(v) => v,
        Err(f) => return failed(app, tool_name, &f, Some(&reply)),
    };
    match extract_transaction_intent_hash(&response) {
        Ok(txid) => reply.annotate(ToolResult::text(format!(
            "TRANSACTION SUBMITTED ✓ (network: {net})\n\
             Intent hash: {txid}\n\
             Interaction: {id}\n\n\
             The wallet signed and submitted it. Confirm the commit with:\n\
             transaction_status {{ \"intent_hash\": \"{txid}\", \"network\": \"{net}\" }}",
            net = target.network.label(),
            id = reply.interaction_id,
        ))),
        Err(e) => failed(app, tool_name, &no_content(&e.to_string(), &reply), Some(&reply)),
    }
}

async fn send_transaction(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "send_transaction";
    let manifest = match req_str(args, "manifest") {
        Ok(v) => v,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    if manifest.contains("\"lock_fee\"") || manifest.contains("\"lock_contingent_fee\"") {
        return failed(
            app,
            TOOL,
            &Failure::input("the manifest locks a fee (lock_fee / lock_contingent_fee): the wallet adds its own fee lock and answers invalidRequest to a manifest that already has one. Remove it."),
            None,
        );
    }
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let message = opt_str(args, "message").unwrap_or_default();
    let blobs = match resolve_blobs(args) {
        Ok(b) => b,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    submit_transaction(app, TOOL, args, &target, &manifest, &message, &blobs).await
}

async fn deploy_package(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "deploy_package";
    let wasm_path = match req_str(args, "wasm_path") {
        Ok(v) => v,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    let package_definition = match req_str(args, "package_definition") {
        Ok(v) => v,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    // OwnerRole in manifest syntax; default "None" (no owner). The HTTP MCP can
    // supply a richer value (e.g. a badge rule) for advanced setups.
    let owner_role = opt_str(args, "owner_role").unwrap_or_else(|| "None".to_string());

    let wasm = match std::fs::read(&wasm_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return failed(
                app,
                TOOL,
                &Failure::input(format!("could not read wasm file '{wasm_path}': {e}")),
                None,
            )
        }
    };
    if wasm.is_empty() {
        return failed(
            app,
            TOOL,
            &Failure::input(format!("wasm file '{wasm_path}' is empty")),
            None,
        );
    }
    let wasm_hex = hex::encode(&wasm);
    let blob_hash = hex::encode(blake2b_256(&wasm));

    let manifest = format!(
        "PUBLISH_PACKAGE_ADVANCED\n    \
         {owner_role}\n    \
         {package_definition}\n    \
         Blob(\"{blob_hash}\")\n    \
         Map<String, Tuple>()\n    \
         None\n;\n"
    );

    // Dry-run on the Gateway (with the WASM blob) before asking the user to
    // approve — a package deploy is costly, so never sign one that would fail.
    // Only a definitive simulated failure blocks; a preview infra error does not.
    if let Ok(outcome) = gateway::preview(target.network, &manifest, std::slice::from_ref(&wasm_hex)).await {
        if !outcome.success {
            let failure = Failure::new(
                "PREVIEW_FAILED",
                "preflight",
                true,
                outcome
                    .message
                    .unwrap_or_else(|| "the simulation did not succeed".to_string()),
                "Not signed: a deploy costs the fee even when it fails. Fix the package or the owner role, and preview again.",
            );
            return failed(app, TOOL, &failure, None);
        }
    }

    submit_transaction(app, TOOL, args, &target, &manifest, "", &[wasm_hex]).await
}

/// Collects transaction blobs from `blobs` (inline hex strings) and `blob_files`
/// (paths to binary files the connector reads and hex-encodes locally — for
/// large payloads such as package WASM that must never travel through the agent).
fn resolve_blobs(args: &Value) -> Result<Vec<String>, String> {
    let mut blobs = Vec::new();
    if let Some(arr) = args.get("blobs").and_then(Value::as_array) {
        for entry in arr {
            let hex_str = entry
                .as_str()
                .ok_or("each entry in 'blobs' must be a hex string")?;
            if hex_str.is_empty() {
                continue;
            }
            if hex_str.len() % 2 != 0 || !hex_str.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("each entry in 'blobs' must be hex-encoded".to_string());
            }
            blobs.push(hex_str.to_string());
        }
    }
    if let Some(arr) = args.get("blob_files").and_then(Value::as_array) {
        for entry in arr {
            let path = entry
                .as_str()
                .ok_or("each entry in 'blob_files' must be a file path")?;
            let bytes = std::fs::read(path).map_err(|e| format!("could not read blob file '{path}': {e}"))?;
            blobs.push(hex::encode(bytes));
        }
    }
    Ok(blobs)
}

async fn request_pre_authorization(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "request_pre_authorization";
    let subintent = match req_str(args, "subintent_manifest") {
        Ok(v) => v,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    let Some(expire) = opt_u64(args, "expire_after_seconds") else {
        return failed(
            app,
            TOOL,
            &Failure::input("missing required parameter 'expire_after_seconds'"),
            None,
        );
    };
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let message = opt_str(args, "message").unwrap_or_default();
    let ask = Ask {
        tool: TOOL,
        kind: "pre_authorization",
        summary: format!(
            "{} (expires after {expire} s)",
            manifest_summary(&subintent, &message)
        ),
        interaction: pre_authorization_request(&subintent, &message, expire, &target.ctx),
    };
    let (reply, result) = interact(app, args, &target, ask).await;
    let response = match result {
        Ok(v) => v,
        Err(f) => return failed(app, TOOL, &f, Some(&reply)),
    };
    match extract_signed_partial_transaction(&response) {
        Ok(signed_hex) => reply.annotate(ToolResult::text(format!(
            "PRE-AUTHORIZATION SIGNED ✓ (network: {net})\n\
             Interaction: {id}\n\
             Signed partial transaction (hex):\n{signed_hex}\n\n\
             It was NOT submitted. Combine it into a parent transaction to use it.",
            net = target.network.label(),
            id = reply.interaction_id,
        ))),
        Err(e) => failed(app, TOOL, &no_content(&e.to_string(), &reply), Some(&reply)),
    }
}

/// A dApp definition is part of every signed ROLA message: without one a proof proves nothing.
fn require_dapp(target: &Target) -> Result<(), Failure> {
    if target.dapp_definition().is_empty() {
        return Err(Failure::input(
            "missing 'dapp_definition' — pass it, or set the RADIX_DAPP_DEFINITION_MAINNET / \
             RADIX_DAPP_DEFINITION_STOKENET env var. It is part of the signed ROLA message, so it cannot be empty.",
        ));
    }
    Ok(())
}

async fn request_account_proof(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "request_account_proof";
    let challenge = match req_str(args, "challenge") {
        Ok(v) => v,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    if let Err(f) = require_dapp(&target) {
        return failed(app, TOOL, &f, None);
    }
    // What the person is asked to share besides the signature. Nothing by default: a one-time
    // data request is one the wallet will not let them skip, so asking for an email they do not
    // have turns "log in" into "first write one down".
    let share = PersonaRequest {
        name: opt_bool(args, "request_persona").unwrap_or(false)
            || opt_bool(args, "request_email").unwrap_or(false),
        email: opt_bool(args, "request_email").unwrap_or(false),
    };
    let ask = Ask {
        tool: TOOL,
        kind: "account_proof",
        summary: "sign a ROLA challenge with an account".to_string(),
        interaction: account_proof_request_sharing(&challenge, &target.ctx, share),
    };
    let (reply, result) = interact(app, args, &target, ask).await;
    let response = match result {
        Ok(v) => v,
        Err(f) => return failed(app, TOOL, &f, Some(&reply)),
    };

    let proofs = match extract_proofs(&response) {
        Ok(proofs) => proofs,
        Err(e) => return failed(app, TOOL, &no_content(&e.to_string(), &reply), Some(&reply)),
    };
    let Some((address, proof)) = proofs.into_iter().next() else {
        return failed(
            app,
            TOOL,
            &no_content("the wallet returned an empty proof set", &reply),
            Some(&reply),
        );
    };

    let ap = AccountProof {
        address: address.clone(),
        public_key_hex: proof
            .get("publicKey")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        signature_hex: proof
            .get("signature")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    let verification = verify_account_proof(
        &ap,
        &challenge,
        target.dapp_definition(),
        target.origin(),
        target.network.id(),
    );
    let persona = extract_persona_name(&response);
    let email = extract_persona_email(&response);

    let (verdict, extra) = match verification {
        Ok(()) => ("VERIFIED ✓", String::new()),
        Err(e) => ("NOT VERIFIED ✗", format!("\nVerification error: {e}")),
    };

    reply.annotate(ToolResult::text(format!(
        "ACCOUNT PROOF {verdict} (network: {net})\n\
         Address:     {address}\n\
         Public key:  {pk}\n\
         Persona:     {persona}\n\
         Email:       {email}\n\
         Interaction: {id}{extra}",
        net = target.network.label(),
        address = ap.address,
        pk = ap.public_key_hex,
        persona = persona.as_deref().unwrap_or("(not requested / not shared)"),
        email = email.as_deref().unwrap_or("(not requested / not shared)"),
        id = reply.interaction_id,
    )))
}

/// The challenge a request signs: the caller's (checked), or a fresh one.
fn challenge_arg(args: &Value) -> Result<String, Failure> {
    match opt_str(args, "challenge") {
        Some(challenge) => {
            check_challenge(&challenge).map_err(Failure::input)?;
            Ok(challenge)
        }
        None => random_challenge().map_err(Failure::local),
    }
}

/// Sends an authorized/unauthorized request and lays out the answer with every proof verified.
async fn persona_request(
    app: &Rc<App>,
    args: &Value,
    target: &Target,
    tool_name: &str,
    kind: &str,
    summary: String,
    interaction: Value,
    challenge: &str,
) -> ToolResult {
    let ask = Ask {
        tool: tool_name,
        kind,
        summary,
        interaction,
    };
    let (reply, result) = interact(app, args, target, ask).await;
    let response = match result {
        Ok(v) => v,
        Err(f) => return failed(app, tool_name, &f, Some(&reply)),
    };
    let verifier = Verifier {
        challenge,
        dapp_definition: target.dapp_definition(),
        origin: target.origin(),
        network_id: target.network.id(),
    };
    let body = render_answer(&response, Some(&verifier));
    let warning = if body.contains("✗ NOT VERIFIED") {
        "\n⚠ At least one proof did NOT verify: do not trust what it claims.\n"
    } else {
        ""
    };
    reply.annotate(ToolResult::text(format!(
        "{title} ✓ (network: {net})\n{body}{warning}\nChallenge:   {challenge}\nInteraction: {id}\n\n\
         Raw answer:\n```json\n{raw}\n```",
        title = tool_name.to_uppercase().replace('_', " "),
        net = target.network.label(),
        id = reply.interaction_id,
        raw = serde_json::to_string_pretty(&response).unwrap_or_default(),
    )))
}

async fn request_login(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "request_login";
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let without = opt_bool(args, "without_challenge").unwrap_or(false);
    let challenge = match challenge_arg(args) {
        Ok(c) => c,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let built = (|| -> Result<AuthorizedRequest, String> {
        Ok(AuthorizedRequest {
            reset: Some((false, false)),
            one_time_accounts: parse_accounts(args.get("accounts"), "accounts", &challenge)?,
            one_time_persona_data: parse_persona_data(args.get("persona_data"), "persona_data")?,
            ..AuthorizedRequest::new(if without {
                Auth::LoginWithoutChallenge
            } else {
                Auth::LoginWithChallenge(challenge.clone())
            })
        })
    })();
    let request = match built {
        Ok(r) => r,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    if let Err(f) = require_dapp(&target) {
        return failed(app, TOOL, &f, None);
    }
    let summary = format!(
        "log in{}{}",
        if without {
            " (no proof)"
        } else {
            " with a persona proof"
        },
        what_else(&request)
    );
    let interaction = authorized_request(&request, &target.ctx);
    persona_request(
        app,
        args,
        &target,
        TOOL,
        "login",
        summary,
        interaction,
        &challenge,
    )
    .await
}

async fn request_ownership_proof(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "request_ownership_proof";
    let identity = match req_str(args, "identity_address") {
        Ok(v) => v,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    let accounts = string_list(args, "accounts");
    let prove_persona = opt_bool(args, "prove_persona").unwrap_or(false);
    if accounts.is_empty() && !prove_persona {
        return failed(
            app,
            TOOL,
            &Failure::input("nothing to prove: pass accounts and/or prove_persona: true"),
            None,
        );
    }
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    if let Err(f) = require_dapp(&target) {
        return failed(app, TOOL, &f, None);
    }
    let challenge = match challenge_arg(args) {
        Ok(c) => c,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let request = AuthorizedRequest {
        proof_of_ownership: Some(OwnershipWanted {
            challenge: challenge.clone(),
            accounts: accounts.clone(),
            identity: prove_persona.then(|| identity.clone()),
        }),
        ..AuthorizedRequest::new(Auth::UsePersona(identity.clone()))
    };
    let summary = format!(
        "prove ownership of {n} account(s){p} as {identity}",
        n = accounts.len(),
        p = if prove_persona { " and the persona" } else { "" },
    );
    let interaction = authorized_request(&request, &target.ctx);
    persona_request(
        app,
        args,
        &target,
        TOOL,
        "ownership_proof",
        summary,
        interaction,
        &challenge,
    )
    .await
}

async fn request_authorized_tool(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "request_authorized";
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let challenge = match challenge_arg(args) {
        Ok(c) => c,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let identity = opt_str(args, "identity_address");
    let built = (|| -> Result<AuthorizedRequest, String> {
        let auth = match opt_str(args, "auth").as_deref() {
            Some("login") => Auth::LoginWithChallenge(challenge.clone()),
            Some("login_without_challenge") => Auth::LoginWithoutChallenge,
            Some("use_persona") => Auth::UsePersona(
                identity
                    .clone()
                    .ok_or("auth = use_persona needs identity_address (from request_login)")?,
            ),
            Some(other) => {
                return Err(format!(
                    "unknown auth \"{other}\" — use login, login_without_challenge or use_persona"
                ))
            }
            None => return Err("missing required parameter 'auth'".to_string()),
        };
        let prove_accounts = string_list(args, "prove_accounts");
        let prove_persona = opt_bool(args, "prove_persona").unwrap_or(false);
        if prove_persona && !matches!(auth, Auth::UsePersona(_)) {
            return Err("prove_persona needs auth = use_persona with identity_address".to_string());
        }
        let proof_of_ownership = (!prove_accounts.is_empty() || prove_persona).then(|| OwnershipWanted {
            challenge: challenge.clone(),
            accounts: prove_accounts,
            identity: if prove_persona { identity.clone() } else { None },
        });
        Ok(AuthorizedRequest {
            auth,
            reset: Some((
                opt_bool(args, "reset_accounts").unwrap_or(false),
                opt_bool(args, "reset_persona_data").unwrap_or(false),
            )),
            proof_of_ownership,
            one_time_accounts: parse_accounts(
                args.get("one_time_accounts"),
                "one_time_accounts",
                &challenge,
            )?,
            ongoing_accounts: parse_accounts(args.get("ongoing_accounts"), "ongoing_accounts", &challenge)?,
            one_time_persona_data: parse_persona_data(
                args.get("one_time_persona_data"),
                "one_time_persona_data",
            )?,
            ongoing_persona_data: parse_persona_data(
                args.get("ongoing_persona_data"),
                "ongoing_persona_data",
            )?,
        })
    })();
    let request = match built {
        Ok(r) => r,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    if let Err(f) = require_dapp(&target) {
        return failed(app, TOOL, &f, None);
    }
    let how = match &request.auth {
        Auth::LoginWithChallenge(_) => "log in with a persona proof".to_string(),
        Auth::LoginWithoutChallenge => "log in (no proof)".to_string(),
        Auth::UsePersona(identity) => format!("as {identity}"),
    };
    let summary = format!("{how}{}", what_else(&request));
    let interaction = authorized_request(&request, &target.ctx);
    persona_request(
        app,
        args,
        &target,
        TOOL,
        "authorized",
        summary,
        interaction,
        &challenge,
    )
    .await
}

async fn request_data(app: &Rc<App>, args: &Value) -> ToolResult {
    const TOOL: &str = "request_data";
    let target = match target(app, args, DEFAULT_SIGN_TIMEOUT) {
        Ok(t) => t,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let challenge = match challenge_arg(args) {
        Ok(c) => c,
        Err(f) => return failed(app, TOOL, &f, None),
    };
    let accounts = match parse_accounts(args.get("accounts"), "accounts", &challenge) {
        Ok(a) => a,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    let persona_data = match parse_persona_data(args.get("persona_data"), "persona_data") {
        Ok(d) => d,
        Err(e) => return failed(app, TOOL, &Failure::input(e), None),
    };
    if accounts.is_none() && persona_data.is_none() {
        return failed(
            app,
            TOOL,
            &Failure::input("nothing asked: pass accounts and/or persona_data"),
            None,
        );
    }
    if accounts.as_ref().is_some_and(|a| a.challenge.is_some()) {
        if let Err(f) = require_dapp(&target) {
            return failed(app, TOOL, &f, None);
        }
    }
    let probe = AuthorizedRequest {
        one_time_accounts: accounts.clone(),
        one_time_persona_data: persona_data,
        ..AuthorizedRequest::new(Auth::LoginWithoutChallenge)
    };
    let summary = format!("share{}", what_else(&probe));
    let interaction = unauthorized_request(accounts.as_ref(), persona_data, &target.ctx);
    persona_request(app, args, &target, TOOL, "data", summary, interaction, &challenge).await
}

/// «, 1+ account(s) with proof, ongoing persona data (name, phones)» — what else a request asks.
fn what_else(request: &AuthorizedRequest) -> String {
    let quantity = |q: radixdlt_connect::Quantity| match q {
        radixdlt_connect::Quantity::AtLeast(n) => format!("{n}+"),
        radixdlt_connect::Quantity::Exactly(n) => format!("exactly {n}"),
    };
    let accounts = |a: &radixdlt_connect::AccountsWanted, when: &str| {
        format!(
            "{when}{q} account(s){p}",
            q = quantity(a.quantity),
            p = if a.challenge.is_some() { " with proof" } else { "" }
        )
    };
    let data = |d: &radixdlt_connect::PersonaDataWanted, when: &str| {
        let mut fields = Vec::new();
        if d.name {
            fields.push("name".to_string());
        }
        if let Some(q) = d.emails {
            fields.push(format!("{} email(s)", quantity(q)));
        }
        if let Some(q) = d.phones {
            fields.push(format!("{} phone(s)", quantity(q)));
        }
        format!("{when}persona data ({})", fields.join(", "))
    };
    let mut parts = Vec::new();
    if let Some((a, d)) = request.reset {
        if a || d {
            parts.push(format!(
                "reset {}",
                [(a, "accounts"), (d, "persona data")]
                    .iter()
                    .filter(|(on, _)| *on)
                    .map(|(_, what)| *what)
                    .collect::<Vec<_>>()
                    .join(" + ")
            ));
        }
    }
    if let Some(own) = &request.proof_of_ownership {
        parts.push(format!(
            "prove {} account(s){}",
            own.accounts.len(),
            if own.identity.is_some() { " + persona" } else { "" }
        ));
    }
    if let Some(a) = &request.one_time_accounts {
        parts.push(accounts(a, ""));
    }
    if let Some(a) = &request.ongoing_accounts {
        parts.push(accounts(a, "ongoing "));
    }
    if let Some(d) = &request.one_time_persona_data {
        parts.push(data(d, ""));
    }
    if let Some(d) = &request.ongoing_persona_data {
        parts.push(data(d, "ongoing "));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(", {}", parts.join(", "))
    }
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn transaction_status(args: &Value) -> ToolResult {
    let intent_hash = match req_str(args, "intent_hash") {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };
    let network = match req_network(args) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(e),
    };
    match gateway::transaction_status(network, &intent_hash).await {
        Ok(status) => {
            let note = match status.as_str() {
                "CommittedSuccess" => "The transaction committed successfully.",
                "CommittedFailure" => "The transaction committed but FAILED on-ledger.",
                "Rejected" => "The transaction was permanently rejected.",
                "Pending" | "Unknown" => "Not final yet — check again shortly.",
                _ => "",
            };
            ToolResult::text(format!(
                "TRANSACTION STATUS (network: {net})\n\
                 Intent hash: {hash}\n\
                 Status:      {status}\n{note}",
                net = network.label(),
                hash = intent_hash,
                status = status,
                note = note,
            ))
        }
        Err(e) => ToolResult::error(e),
    }
}

/* ──────────────────────────────── helpers ──────────────────────────────── */

/// Env var holding the default dApp definition for a network, so the operator
/// can configure the connector's identity once instead of relying on the agent
/// to pass `dapp_definition` on every call.
fn dapp_definition_env(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "RADIX_DAPP_DEFINITION_MAINNET",
        Network::Stokenet => "RADIX_DAPP_DEFINITION_STOKENET",
    }
}

/// Resolves the dApp definition with precedence: call arg → per-network env var
/// → empty (which the wallet refuses).
pub fn resolve_dapp_definition(args: &Value, network: Network) -> String {
    opt_str(args, "dapp_definition")
        .or_else(|| env_var(dapp_definition_env(network)))
        .unwrap_or_default()
}

/// Resolves the origin with precedence: call arg → `RADIX_DAPP_ORIGIN` env var
/// → the built-in default.
pub fn resolve_origin(args: &Value) -> String {
    opt_str(args, "origin")
        .or_else(|| env_var("RADIX_DAPP_ORIGIN"))
        .unwrap_or_else(|| DEFAULT_ORIGIN.to_string())
}

/// Reads an env var, treating unset and empty as "not provided".
fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

pub fn clamp_timeout(seconds: u64) -> u64 {
    seconds.clamp(1, MAX_TIMEOUT)
}

pub fn req_network(args: &Value) -> Result<Network, String> {
    Network::parse(&req_str(args, "network")?)
}

pub fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn req_str(args: &Value, key: &str) -> Result<String, String> {
    opt_str(args, key).ok_or_else(|| format!("missing required parameter '{key}'"))
}

pub fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

pub fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_parsing_and_ids() {
        assert_eq!(Network::parse("mainnet").unwrap().id(), 1);
        assert_eq!(Network::parse("stokenet").unwrap().id(), 2);
        assert!(Network::parse("devnet").is_err());
    }

    #[test]
    fn every_tool_has_a_schema() {
        for tool in list_json() {
            assert!(tool.get("name").and_then(Value::as_str).is_some());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn timeouts_are_clamped() {
        assert_eq!(clamp_timeout(0), 1);
        assert_eq!(clamp_timeout(10_000), MAX_TIMEOUT);
        assert_eq!(clamp_timeout(300), 300);
    }

    #[test]
    fn dapp_definition_env_is_per_network() {
        assert_eq!(
            dapp_definition_env(Network::Mainnet),
            "RADIX_DAPP_DEFINITION_MAINNET"
        );
        assert_eq!(
            dapp_definition_env(Network::Stokenet),
            "RADIX_DAPP_DEFINITION_STOKENET"
        );
    }

    #[test]
    fn call_arg_takes_precedence_over_env_and_default() {
        // An explicit arg is always honoured regardless of env/default.
        let args = json!({ "dapp_definition": "account_rdx_arg", "origin": "https://arg.example" });
        assert_eq!(
            resolve_dapp_definition(&args, Network::Mainnet),
            "account_rdx_arg"
        );
        assert_eq!(resolve_origin(&args), "https://arg.example");
    }

    #[test]
    fn blake2b_256_matches_standard_vector() {
        // The package-deploy blob hash must equal the standard BLAKE2b-256 the
        // wallet/gateway recompute, and the TS side (blakejs). "abc" is a fixed
        // cross-checked vector (b2sum -l 256).
        assert_eq!(
            hex::encode(blake2b_256(b"abc")),
            "bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d52319"
        );
    }

    #[test]
    fn resolve_blobs_reads_inline_hex_and_files() {
        // Inline hex is validated and passed through; odd-length/non-hex is rejected.
        let inline = json!({ "blobs": ["deadbeef", ""] });
        assert_eq!(resolve_blobs(&inline).unwrap(), vec!["deadbeef".to_string()]);
        assert!(resolve_blobs(&json!({ "blobs": ["xyz"] })).is_err());
        assert!(resolve_blobs(&json!({ "blobs": ["abc"] })).is_err());

        // blob_files are read from disk and hex-encoded.
        let mut path = std::env::temp_dir();
        path.push(format!("connector_mcp_blob_{}.bin", std::process::id()));
        std::fs::write(&path, [0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        let files = json!({ "blob_files": [path.to_str().unwrap()] });
        assert_eq!(resolve_blobs(&files).unwrap(), vec!["deadbeef".to_string()]);
        std::fs::remove_file(&path).ok();

        // No blob keys → empty.
        assert!(resolve_blobs(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn origin_falls_back_to_default_when_unset() {
        // With no arg and (in the test env) no RADIX_DAPP_ORIGIN, origin is the default
        // and the dApp definition is empty.
        let args = json!({});
        assert_eq!(resolve_origin(&args), DEFAULT_ORIGIN);
        assert!(resolve_dapp_definition(&args, Network::Stokenet).is_empty());
    }
}
