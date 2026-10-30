// Examples are demonstrations: `expect` is the idiomatic way to keep them short and to show the
// failure loudly. Library code keeps the deny.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! "Log in with Radix" from a Rust program, straight to the Radix mobile wallet over iroh — no
//! browser, no Connector Extension.
//!
//! ```text
//! cargo run --example mobile_login -- [--state <file>] [--relay <url>] [--timeout <seconds>]
//!                                      [--dapp <account>] [--origin <url>] [--network <id>] [--repair]
//! ```
//!
//! 1. On the phone: Settings → Preferences → Radix Connect → **Automatic** (or **iroh**), with the
//!    same relay as this program (`--relay`; default: the public development relay).
//! 2. The first run pairs: it draws a QR code here; scan it from Settings → Linked Connectors →
//!    Link New Connector. The link (its password and this connector's identity) is stored in the
//!    state file — created readable by you only — and is NEVER printed. Keep that file like a
//!    password: whoever has it can ask your wallet for signatures as this connector.
//! 3. Every run asks the wallet to log in (persona + accounts, over a fresh challenge), verifies
//!    the proofs with `radixdlt-rola` and prints the verified persona and accounts.
//!
//! Diagnostics: every connection change prints the link's connectivity line, and errors carry it.
//! For the full story, `RUST_LOG=radix_connect_iroh=debug,radixdlt_connect_iroh=debug`.
//!
//! The state file has the shape of `radixdlt-connect`'s `connector.json` (plus `irohRelayUrl`),
//! so a link paired here also works over WebRTC when the wallet uses Automatic.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use radixdlt_connect_iroh::mobile::{
    extract_login, extract_proofs, linking, login_request, DappContext, MobileWalletConnector,
    PersonaRequest, Progress, DEFAULT_RELAY_URL,
};
use radixdlt_rola::{verify_account_proof, verify_persona_proof, AccountProof};
use serde_json::{json, Value};

// Sample Stokenet dApp definition; use --dapp with your own.
const DAPP_DEFAULT: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
const ORIGIN_DEFAULT: &str = "https://localhost";
const PAIRING_TIMEOUT: Duration = Duration::from_secs(300);

struct Args {
    state: PathBuf,
    relay: Option<String>,
    timeout: Duration,
    dapp: String,
    origin: String,
    network: u8,
    repair: bool,
}

fn args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    Args {
        state: get("--state")
            .map(PathBuf::from)
            .unwrap_or_else(default_state_file),
        relay: get("--relay"),
        timeout: Duration::from_secs(get("--timeout").and_then(|t| t.parse().ok()).unwrap_or(120)),
        dapp: get("--dapp").unwrap_or_else(|| DAPP_DEFAULT.to_string()),
        origin: get("--origin").unwrap_or_else(|| ORIGIN_DEFAULT.to_string()),
        network: get("--network").and_then(|n| n.parse().ok()).unwrap_or(2),
        repair: args.iter().any(|a| a == "--repair"),
    }
}

/// `~/.config/radixdlt-connect-iroh/mobile-link.json` (`$XDG_CONFIG_HOME` first; `%APPDATA%` on
/// Windows).
fn default_state_file() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("radixdlt-connect-iroh")
        .join("mobile-link.json")
}

fn load(path: &Path) -> Value {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).expect("the state file is not valid JSON"),
        Err(_) => json!({ "version": 1 }),
    }
}

/// Writes the state readable by its owner only, replacing the old file in one step.
fn save(path: &Path, state: &Value) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("create the state directory");
    }
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary).expect("create the state file");
    std::io::Write::write_all(&mut file, serde_json::to_string_pretty(state).unwrap().as_bytes())
        .expect("write the state file");
    file.sync_all().expect("write the state file");
    std::fs::rename(&temporary, path).expect("replace the state file");
}

/// Prints a step of a call; connection changes come with the link's diagnostics line.
fn show(step: Progress) {
    match step {
        Progress::WaitingForWallet { waited, diagnostics } => {
            println!(
                "… waiting for the wallet ({}s): open the Radix Wallet app",
                waited.as_secs()
            );
            println!("  diagnostics: {diagnostics}");
        }
        Progress::Connected { diagnostics, .. } => println!("✓ wallet connected — {diagnostics}"),
        Progress::WalletDisconnected { diagnostics } => {
            println!("… the wallet disconnected; waiting for it to come back — {diagnostics}");
        }
        Progress::Delivered { .. } => println!(">>> APPROVE THE REQUEST ON YOUR PHONE <<<"),
        Progress::Resending { attempt, reason } => println!("… sending again (attempt {attempt}): {reason}"),
        Progress::Unconfirmed { reason } => {
            println!("… the wallet did not confirm the request ({reason}); still waiting for its answer");
        }
        Progress::OtherMessage(message) => {
            // Only the kind of message: its contents are the wallet's business.
            let kind = message["discriminator"].as_str().unwrap_or("?");
            println!("  (the wallet also sent: {kind})");
        }
        _ => {}
    }
}

/// The stored link, pairing first if there is none (or `--repair`).
async fn link(connector: &MobileWalletConnector, args: &Args, state: &mut Value) -> Vec<u8> {
    if !args.repair {
        if let Some(password) = state["links"][0]["password"].as_str() {
            println!("Using the link stored in {}", args.state.display());
            return hex::decode(password).expect("the stored link password is not hex");
        }
    }
    if state["identity"]["privateKey"].as_str().is_none() {
        let (private, public) = linking::generate_identity().expect("generate the connector identity");
        state["identity"] = json!({ "privateKey": private, "publicKey": public });
    }
    let private = state["identity"]["privateKey"].as_str().unwrap().to_string();
    let public = state["identity"]["publicKey"].as_str().unwrap().to_string();

    let on_qr = |qr: String| {
        // The QR carries the link password: it is drawn, never printed as text.
        let art = radixdlt_connect_types::qr::render_unicode(&qr).expect("draw the QR code");
        println!("\nIn the Radix Wallet: Settings → Linked Connectors → Link New Connector, and scan:\n");
        println!("{art}");
        println!("(If it does not scan, switch the terminal to a light theme or enlarge the window.)");
        println!(
            "Relay: {} — the wallet must use the same one.",
            connector.relay_url()
        );
    };
    let (wallet_public_key, password) = match connector
        .pair_with_progress(&private, &public, on_qr, PAIRING_TIMEOUT, &mut show)
        .await
    {
        Ok(linked) => linked,
        Err(error) => {
            eprintln!("pairing failed: {error}");
            std::process::exit(1);
        }
    };
    let linked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    state["links"] = json!([{
        "password": hex::encode(&password),
        "walletPublicKey": wallet_public_key,
        "linkedAt": linked_at,
        "irohRelayUrl": connector.relay_url(),
    }]);
    save(&args.state, state);
    println!(
        "✓ linked with wallet {wallet_public_key}; link stored in {}",
        args.state.display()
    );
    password
}

fn proof_of(address: &str, proof: &Value) -> AccountProof {
    AccountProof {
        address: address.to_string(),
        public_key_hex: proof["publicKey"].as_str().unwrap_or_default().to_string(),
        signature_hex: proof["signature"].as_str().unwrap_or_default().to_string(),
    }
}

/// Verifies the persona and every account of a login answer; prints them. `false` if any fails.
fn verify(answer: &Value, challenge: &str, ctx: &DappContext) -> bool {
    let (identity, label, persona) = match extract_login(answer) {
        Ok(login) => login,
        Err(error) => {
            eprintln!("the answer is not a login: {error}");
            return false;
        }
    };
    let mut all_ok = true;
    let persona = proof_of(&identity, &persona["proof"]);
    match verify_persona_proof(
        &persona,
        challenge,
        &ctx.dapp_definition,
        &ctx.origin,
        ctx.network_id,
    ) {
        Ok(()) => println!("✓ persona VERIFIED: {identity} ({label})"),
        Err(error) => {
            println!("✗ persona proof invalid for {identity}: {error}");
            all_ok = false;
        }
    }
    for (address, proof) in extract_proofs(answer).unwrap_or_default() {
        let account = proof_of(&address, &proof);
        match verify_account_proof(
            &account,
            challenge,
            &ctx.dapp_definition,
            &ctx.origin,
            ctx.network_id,
        ) {
            Ok(()) => println!("✓ account VERIFIED: {address}"),
            Err(error) => {
                println!("✗ account proof invalid for {address}: {error}");
                all_ok = false;
            }
        }
    }
    all_ok
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .init();

    let args = args();
    let mut state = load(&args.state);
    let relay = args
        .relay
        .clone()
        .or_else(|| state["links"][0]["irohRelayUrl"].as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());
    let connector = match MobileWalletConnector::new().with_relay_url(&relay) {
        Ok(connector) => connector,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    let password = link(&connector, &args, &mut state).await;
    // Listen at once, so the wallet can connect while the challenge is prepared.
    if let Err(error) = connector.open_link(&password).await {
        eprintln!("{error}");
        std::process::exit(2);
    }

    let mut challenge = [0u8; 32];
    getrandom::fill(&mut challenge).expect("system randomness");
    let challenge = hex::encode(challenge);
    let ctx = DappContext::new(args.network, &args.dapp, &args.origin);
    println!(
        "Asking the wallet to log in to {} (max {}s)…",
        args.origin,
        args.timeout.as_secs()
    );
    let interaction = login_request(&challenge, &ctx, PersonaRequest::NONE);
    let answer = match connector
        .exchange(&password, &interaction, args.timeout, &mut show)
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            eprintln!("the login failed: {error}");
            if let Some(diagnostics) = connector.diagnostics(&password) {
                eprintln!("diagnostics now: {diagnostics}");
            }
            connector.close().await;
            std::process::exit(3);
        }
    };
    let verified = verify(&answer, &challenge, &ctx);
    connector.close().await;
    std::process::exit(if verified { 0 } else { 4 });
}
