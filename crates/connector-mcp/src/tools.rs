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
use radixdlt_connect::state::{Link, LinkState};
use radixdlt_connect::{
    extract_accounts, extract_persona_name, extract_proofs, Connector, DappContext,
};
use radixdlt_rola::{verify_account_proof, AccountProof};

use crate::gateway;
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
    fn parse(s: &str) -> Result<Network, String> {
        match s {
            "mainnet" => Ok(Network::Mainnet),
            "stokenet" => Ok(Network::Stokenet),
            other => Err(format!(
                "invalid network \"{other}\" — use \"mainnet\" or \"stokenet\""
            )),
        }
    }

    fn id(self) -> u8 {
        match self {
            Network::Mainnet => 1,
            Network::Stokenet => 2,
        }
    }

    fn label(self) -> &'static str {
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
}

impl ToolResult {
    fn text(text: impl Into<String>) -> Self {
        ToolResult {
            content: vec![Content::Text(text.into())],
            is_error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        ToolResult {
            content: vec![Content::Text(text.into())],
            is_error: true,
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
        json!({ "content": content, "isError": self.is_error })
    }
}

/* ─────────────────────────────── registry ──────────────────────────────── */

const NETWORK_PROP: &str = "Radix network: \"mainnet\" (real funds) or \"stokenet\" (testnet). Required — there is no default, on purpose.";

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
            "Sends a transaction manifest to the paired wallet to sign AND submit. The user approves on their phone. Returns the transaction intent hash; confirm the commit with transaction_status. Build and preview the manifest with the radix-community HTTP MCP server first.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "manifest": { "type": "string", "description": "The transaction manifest (RTM text) to sign and submit." },
                    "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP },
                    "message": { "type": "string", "description": "Optional transaction message shown to the user in the wallet." },
                    "dapp_definition": { "type": "string", "description": "dApp definition address shown to the wallet (optional; falls back to the RADIX_DAPP_DEFINITION_MAINNET/STOKENET env var; if none, the request shows as unverified)." },
                    "origin": { "type": "string", "description": "Origin URL shown to the wallet (default: RADIX_DAPP_ORIGIN env var, else https://radix-community.genkipool.com)." },
                    "blobs": { "type": "array", "items": { "type": "string" }, "description": "Hex-encoded blobs referenced by the manifest via Blob(\"<hash>\") (optional)." },
                    "blob_files": { "type": "array", "items": { "type": "string" }, "description": "Paths to binary files read locally and attached as blobs — use for large payloads like package WASM (optional)." },
                    "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the first paired wallet)." },
                    "timeout_seconds": { "type": "integer", "description": "How long to wait for approval (default 300, max 900)." }
                },
                "required": ["manifest", "network"]
            }),
        ),
        tool(
            "deploy_package",
            "Deploy a Scrypto package",
            "Publishes a Scrypto package to the network. Reads the compiled .wasm from a LOCAL file path (it never travels through the agent), attaches it as a blob, and signs+submits via the paired wallet. Get `package_definition` by decoding the .rpd with the radix-community HTTP MCP server's build_deploy_package_manifest tool first.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "wasm_path": { "type": "string", "description": "Local filesystem path to the compiled package .wasm." },
                    "package_definition": { "type": "string", "description": "Package definition in manifest (SBOR) syntax — the decoded .rpd, from build_deploy_package_manifest." },
                    "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP },
                    "owner_role": { "type": "string", "description": "OwnerRole in manifest syntax (default \"None\" — no owner). Supply a richer value for badge-controlled packages." },
                    "dapp_definition": { "type": "string", "description": "dApp definition address (optional; falls back to the RADIX_DAPP_DEFINITION_MAINNET/STOKENET env var)." },
                    "origin": { "type": "string", "description": "Origin URL (default: RADIX_DAPP_ORIGIN env var, else https://radix-community.genkipool.com)." },
                    "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the first paired wallet)." },
                    "timeout_seconds": { "type": "integer", "description": "How long to wait for approval (default 300, max 900)." }
                },
                "required": ["wasm_path", "package_definition", "network"]
            }),
        ),
        tool(
            "request_pre_authorization",
            "Request a pre-authorization (subintent)",
            "Asks the wallet to sign a subintent (pre-authorization, transaction V2) WITHOUT submitting it. Returns the signed partial transaction as hex, to be combined into a larger transaction later.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "subintent_manifest": { "type": "string", "description": "The subintent manifest to pre-authorize." },
                    "expire_after_seconds": { "type": "integer", "description": "How long the pre-authorization stays valid, in seconds." },
                    "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP },
                    "message": { "type": "string", "description": "Optional message shown to the user in the wallet." },
                    "dapp_definition": { "type": "string", "description": "dApp definition address shown to the wallet (optional; falls back to the RADIX_DAPP_DEFINITION_MAINNET/STOKENET env var)." },
                    "origin": { "type": "string", "description": "Origin URL shown to the wallet (default: RADIX_DAPP_ORIGIN env var, else https://radix-community.genkipool.com)." },
                    "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the first paired wallet)." },
                    "timeout_seconds": { "type": "integer", "description": "How long to wait for approval (default 300, max 900)." }
                },
                "required": ["subintent_manifest", "expire_after_seconds", "network"]
            }),
        ),
        tool(
            "request_accounts",
            "Get the user's account address(es)",
            "Asks the wallet to SHARE its account address(es) WITHOUT a signature (lightweight, no ROLA proof). Use it to learn which account to fund, transfer from, or set as fee payer before building a manifest. The user approves the share on their phone.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP },
                    "dapp_definition": { "type": "string", "description": "dApp definition address shown to the wallet (optional; falls back to the RADIX_DAPP_DEFINITION_MAINNET/STOKENET env var)." },
                    "origin": { "type": "string", "description": "Origin URL shown to the wallet (default: RADIX_DAPP_ORIGIN env var, else https://radix-community.genkipool.com)." },
                    "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the first paired wallet)." },
                    "timeout_seconds": { "type": "integer", "description": "How long to wait for approval (default 300, max 900)." }
                },
                "required": ["network"]
            }),
        ),
        tool(
            "request_account_proof",
            "Request a ROLA account proof (log in with Radix)",
            "Asks the wallet to sign a ROLA challenge (\"log in with Radix\"). Returns the account address and whether the proof verified locally. `dapp_definition` and `origin` MUST match the values the verifier expects, because they are part of the signed message; pass them, or configure the RADIX_DAPP_DEFINITION_MAINNET/STOKENET and RADIX_DAPP_ORIGIN env vars.",
            false,
            json!({
                "type": "object",
                "properties": {
                    "challenge": { "type": "string", "description": "ROLA challenge as hex (32 bytes)." },
                    "network": { "type": "string", "enum": ["mainnet", "stokenet"], "description": NETWORK_PROP },
                    "dapp_definition": { "type": "string", "description": "dApp definition address (part of the signed ROLA message; falls back to the RADIX_DAPP_DEFINITION_MAINNET/STOKENET env var, and is required — cannot be empty)." },
                    "origin": { "type": "string", "description": "Origin URL (part of the signed ROLA message; falls back to RADIX_DAPP_ORIGIN env var, else https://radix-community.genkipool.com)." },
                    "request_persona": { "type": "boolean", "description": "Also ask for the persona name (default false)." },
                    "wallet_public_key": { "type": "string", "description": "Target a specific paired device (default: the first paired wallet)." },
                    "timeout_seconds": { "type": "integer", "description": "How long to wait for approval (default 300, max 900)." }
                },
                "required": ["challenge", "network"]
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
        "transaction_status" => transaction_status(&args).await,
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
            return ToolResult::error(
                "could not start pairing (the QR payload was not produced). Try again.",
            )
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
        Err(e) => {
            return ToolResult::error(format!("paired, but could not open the state file: {e}"))
        }
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
    let network = match req_network(args) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(e),
    };
    let ctx = match dapp_context(args, network) {
        Ok(ctx) => ctx,
        Err(e) => return ToolResult::error(e),
    };
    let password = match load_password(app, args) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    let timeout = signing_timeout(args);

    let connector = Connector::new();
    let response = match connector.request_accounts(&password, &ctx, timeout).await {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("account request failed: {e}")),
    };
    let accounts = match extract_accounts(&response) {
        Ok(a) => a,
        Err(e) => return ToolResult::error(format!("wallet returned no accounts: {e}")),
    };
    if accounts.is_empty() {
        return ToolResult::error("the wallet shared no accounts.".to_string());
    }

    let mut out = format!(
        "ACCOUNTS SHARED ✓ (network: {net})\n",
        net = network.label()
    );
    for (i, (address, label)) in accounts.iter().enumerate() {
        out.push_str(&format!(
            "{n}. {address}  [{label}]\n",
            n = i + 1,
            label = label.as_deref().unwrap_or("no label"),
        ));
    }
    ToolResult::text(out)
}

/// Signs + submits a manifest (with optional blobs) via the paired wallet.
/// Shared by `send_transaction` and `deploy_package`. Reads dApp context,
/// password and timeout from `args`.
async fn submit_transaction(
    app: &Rc<App>,
    args: &Value,
    network: Network,
    manifest: &str,
    message: &str,
    blobs: &[String],
) -> ToolResult {
    let ctx = match dapp_context(args, network) {
        Ok(ctx) => ctx,
        Err(e) => return ToolResult::error(e),
    };
    let password = match load_password(app, args) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    let timeout = signing_timeout(args);

    let connector = Connector::new();
    match connector
        .request_transaction(&password, manifest, message, blobs, &ctx, timeout)
        .await
    {
        Ok(txid) => ToolResult::text(format!(
            "TRANSACTION SUBMITTED ✓ (network: {net})\n\
             Intent hash: {txid}\n\n\
             The wallet signed and submitted it. Confirm the commit with:\n\
             transaction_status {{ \"intent_hash\": \"{txid}\", \"network\": \"{net}\" }}",
            net = network.label(),
            txid = txid,
        )),
        Err(e) => ToolResult::error(format!("transaction not signed: {e}")),
    }
}

async fn send_transaction(app: &Rc<App>, args: &Value) -> ToolResult {
    let manifest = match req_str(args, "manifest") {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };
    let network = match req_network(args) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(e),
    };
    let message = opt_str(args, "message").unwrap_or_default();
    let blobs = match resolve_blobs(args) {
        Ok(b) => b,
        Err(e) => return ToolResult::error(e),
    };
    submit_transaction(app, args, network, &manifest, &message, &blobs).await
}

async fn deploy_package(app: &Rc<App>, args: &Value) -> ToolResult {
    let wasm_path = match req_str(args, "wasm_path") {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };
    let package_definition = match req_str(args, "package_definition") {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };
    let network = match req_network(args) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(e),
    };
    // OwnerRole in manifest syntax; default "None" (no owner). The HTTP MCP can
    // supply a richer value (e.g. a badge rule) for advanced setups.
    let owner_role = opt_str(args, "owner_role").unwrap_or_else(|| "None".to_string());

    let wasm = match std::fs::read(&wasm_path) {
        Ok(bytes) => bytes,
        Err(e) => return ToolResult::error(format!("could not read wasm file '{wasm_path}': {e}")),
    };
    if wasm.is_empty() {
        return ToolResult::error(format!("wasm file '{wasm_path}' is empty"));
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
    if let Ok(outcome) = gateway::preview(network, &manifest, std::slice::from_ref(&wasm_hex)).await
    {
        if !outcome.success {
            return ToolResult::error(format!(
                "Deploy preview FAILED — not signing (a deploy costs the fee even when it fails):\n{}",
                outcome.message.unwrap_or_else(|| "the simulation did not succeed".to_string()),
            ));
        }
    }

    submit_transaction(app, args, network, &manifest, "", &[wasm_hex]).await
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
            let bytes = std::fs::read(path)
                .map_err(|e| format!("could not read blob file '{path}': {e}"))?;
            blobs.push(hex::encode(bytes));
        }
    }
    Ok(blobs)
}

async fn request_pre_authorization(app: &Rc<App>, args: &Value) -> ToolResult {
    let subintent = match req_str(args, "subintent_manifest") {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };
    let expire = match args.get("expire_after_seconds").and_then(Value::as_u64) {
        Some(v) => v,
        None => return ToolResult::error("missing required parameter 'expire_after_seconds'"),
    };
    let network = match req_network(args) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(e),
    };
    let message = opt_str(args, "message").unwrap_or_default();
    let ctx = match dapp_context(args, network) {
        Ok(ctx) => ctx,
        Err(e) => return ToolResult::error(e),
    };
    let password = match load_password(app, args) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    let timeout = signing_timeout(args);

    let connector = Connector::new();
    match connector
        .request_pre_authorization(&password, &subintent, &message, expire, &ctx, timeout)
        .await
    {
        Ok(signed_hex) => ToolResult::text(format!(
            "PRE-AUTHORIZATION SIGNED ✓ (network: {net})\n\
             Signed partial transaction (hex):\n{signed_hex}\n\n\
             It was NOT submitted. Combine it into a parent transaction to use it.",
            net = network.label(),
            signed_hex = signed_hex,
        )),
        Err(e) => ToolResult::error(format!("pre-authorization not signed: {e}")),
    }
}

async fn request_account_proof(app: &Rc<App>, args: &Value) -> ToolResult {
    let challenge = match req_str(args, "challenge") {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };
    let network = match req_network(args) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(e),
    };
    let dapp_definition = resolve_dapp_definition(args, network);
    if dapp_definition.is_empty() {
        return ToolResult::error(
            "missing 'dapp_definition' — pass it, or set the \
             RADIX_DAPP_DEFINITION_MAINNET / RADIX_DAPP_DEFINITION_STOKENET env var. \
             It is part of the signed ROLA message, so it cannot be empty."
                .to_string(),
        );
    }
    let origin = resolve_origin(args);
    let request_persona = opt_bool(args, "request_persona").unwrap_or(false);
    let password = match load_password(app, args) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    let timeout = signing_timeout(args);
    let ctx = DappContext::new(network.id(), dapp_definition.clone(), origin.clone());

    let connector = Connector::new();
    let response = match connector
        .request_account_proof(&password, &challenge, &ctx, request_persona, timeout)
        .await
    {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("account proof not signed: {e}")),
    };

    let proofs = match extract_proofs(&response) {
        Ok(proofs) => proofs,
        Err(e) => return ToolResult::error(format!("wallet returned no usable proof: {e}")),
    };
    let Some((address, proof)) = proofs.into_iter().next() else {
        return ToolResult::error("the wallet returned an empty proof set.");
    };

    let public_key_hex = proof
        .get("publicKey")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let signature_hex = proof
        .get("signature")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let ap = AccountProof {
        address: address.clone(),
        public_key_hex,
        signature_hex,
    };
    let verification =
        verify_account_proof(&ap, &challenge, &dapp_definition, &origin, network.id());
    let persona = extract_persona_name(&response);

    let (verdict, extra) = match verification {
        Ok(()) => ("VERIFIED ✓", String::new()),
        Err(e) => ("NOT VERIFIED ✗", format!("\nVerification error: {e}")),
    };

    ToolResult::text(format!(
        "ACCOUNT PROOF {verdict} (network: {net})\n\
         Address:    {address}\n\
         Public key: {pk}\n\
         Persona:    {persona}{extra}",
        verdict = verdict,
        net = network.label(),
        address = ap.address,
        pk = ap.public_key_hex,
        persona = persona.as_deref().unwrap_or("(not requested / not shared)"),
        extra = extra,
    ))
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

fn load_password(app: &Rc<App>, args: &Value) -> Result<Vec<u8>, String> {
    let state = Store::load(app.config_path()).map_err(|_| {
        "no paired wallet. Call pair_wallet first (needed once per device).".to_string()
    })?;
    password_for(&state, opt_str(args, "wallet_public_key").as_deref())
}

fn password_for(state: &LinkState, wallet_public_key: Option<&str>) -> Result<Vec<u8>, String> {
    match wallet_public_key {
        Some(pk) => state.password_bytes_for(pk).map_err(|e| e.to_string()),
        None => state.password_bytes().map_err(|_| {
            "no paired wallet. Call pair_wallet first (needed once per device).".to_string()
        }),
    }
}

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
/// → empty (which makes the wallet show the request as unverified).
fn resolve_dapp_definition(args: &Value, network: Network) -> String {
    opt_str(args, "dapp_definition")
        .or_else(|| env_var(dapp_definition_env(network)))
        .unwrap_or_default()
}

/// Resolves the origin with precedence: call arg → `RADIX_DAPP_ORIGIN` env var
/// → the built-in default.
fn resolve_origin(args: &Value) -> String {
    opt_str(args, "origin")
        .or_else(|| env_var("RADIX_DAPP_ORIGIN"))
        .unwrap_or_else(|| DEFAULT_ORIGIN.to_string())
}

/// Reads an env var, treating unset and empty as "not provided".
fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn dapp_context(args: &Value, network: Network) -> Result<DappContext, String> {
    let dapp_definition = resolve_dapp_definition(args, network);
    let origin = resolve_origin(args);
    Ok(DappContext::new(network.id(), dapp_definition, origin))
}

fn signing_timeout(args: &Value) -> Duration {
    Duration::from_secs(clamp_timeout(
        opt_u64(args, "timeout_seconds").unwrap_or(DEFAULT_SIGN_TIMEOUT),
    ))
}

fn clamp_timeout(seconds: u64) -> u64 {
    seconds.clamp(1, MAX_TIMEOUT)
}

fn req_network(args: &Value) -> Result<Network, String> {
    Network::parse(&req_str(args, "network")?)
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn req_str(args: &Value, key: &str) -> Result<String, String> {
    opt_str(args, key).ok_or_else(|| format!("missing required parameter '{key}'"))
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn opt_bool(args: &Value, key: &str) -> Option<bool> {
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
        assert_eq!(
            resolve_blobs(&inline).unwrap(),
            vec!["deadbeef".to_string()]
        );
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
