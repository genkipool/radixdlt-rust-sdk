//! radix-connector-mcp — a local MCP (Model Context Protocol) server that gives
//! AI agents (Claude Code/Desktop, Antigravity, Cursor, …) the ability to pair a
//! Radix Wallet and get transactions **signed on the user's own machine**.
//!
//! Why local: signing a Radix transaction means keeping a live Radix Connect
//! channel (WebRTC, or iroh — see the `iroh` module) open to the phone during the whole
//! approval. A stateless,
//! serverless backend (the web portal on Vercel) cannot hold that channel, and
//! the link secrets must never leave the user's machine. So this piece runs
//! locally and speaks MCP over **stdio** to whatever agent launched it.
//!
//! Transport: newline-delimited JSON-RPC 2.0 over stdin/stdout (the MCP stdio
//! transport). Everything human-readable (logs) goes to **stderr** — stdout is
//! reserved for protocol messages only.
//!
//! The whole server runs on a single-threaded Tokio runtime inside a `LocalSet`:
//! it is low-concurrency by nature (one wallet channel at a time) and this keeps
//! the WebRTC futures off the `Send` requirement while still letting a slow
//! pairing run in the background while other tool calls are served.

// In TESTS a panic IS the failure mechanism. Library code keeps the deny: a panic there is
// taken in the CONSUMER's process, which they neither chose nor can catch.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod diag;
#[cfg(test)]
mod e2e_iroh;
mod exchange;
mod gateway;
mod iroh;
mod link_removal;
mod outbox;
mod qr;
mod requests;
mod rpc;
mod store;
mod tools;
mod trace;
mod update;

use std::rc::Rc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::rpc::{App, Options};

fn main() {
    // Pick rustls' crypto provider explicitly, before anything opens a TLS connection.
    //
    // This binary links TWO providers — aws-lc-rs arrives with `reqwest`, ring with `webrtc` —
    // and rustls will not guess between them: it panics the moment a connection needs one.
    // That is what happened in 0.2.2 through 0.2.4, and it broke the tool's whole purpose,
    // because the panic lands on the WALLET path: the signalling connection dies and nothing
    // ever reaches the phone. The Gateway calls kept working, which is why a release check that
    // only exercised HTTP did not notice.
    //
    // The error is ignored on purpose: it only means a provider was already installed, which is
    // just as good an outcome as installing one.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to start the Tokio runtime");
    let local = tokio::task::LocalSet::new();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let options = match server_options(&mut args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("radix-connector-mcp: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let code = match args.first().map(String::as_str) {
        None => local.block_on(&runtime, run(options)),
        Some(_) => local.block_on(&runtime, command(&args)),
    };
    std::process::exit(code);
}

const USAGE: &str = "\
radix-connector-mcp — local MCP server that gets Radix transactions signed on the phone.

Usage:
  radix-connector-mcp [--transport auto|webrtc|iroh] [--iroh-relay <url>]
                                      run as an MCP server over stdio (what MCP clients launch).
                                      --transport (or RADIX_CONNECTOR_TRANSPORT) says how NEW
                                      wallets are paired: auto (default) shows one QR for WebRTC
                                      and Radix Connect over iroh at once, so any Radix Wallet
                                      links — over iroh when it has iroh on (Settings ›
                                      Preferences › Radix Connect › Automatic or iroh), over
                                      WebRTC otherwise; webrtc or iroh pair over that one only.
                                      Each wallet keeps the transport it was paired over; iroh
                                      also sends every wallet's requests over iroh. --iroh-relay
                                      (or RADIX_CONNECTOR_IROH_RELAY): one relay of your own
                                      instead of n0's public relays (the wallet must use the same).
  radix-connector-mcp version         print the installed version
  radix-connector-mcp check-update    tell whether a newer release is published
  radix-connector-mcp relay [--listen 127.0.0.1:8787] [--turn 127.0.0.1:3478 --turn-password <pw>]
                                      run a local signaling relay (and a TURN server), to reach the
                                      phone with no internet over the USB cable; see the README
  radix-connector-mcp update [--tag connector-vX.Y.Z] [--force]
                                      download the newest (or the given) release and install it
                                      in place of this binary, keeping a backup beside it

Updating never touches connector.json: a paired wallet stays paired.";

/// Takes the server options out of `args` (`--transport X`, `--iroh-relay X`, or `--name=X`);
/// their environment variables fill in what the command line does not say.
fn server_options(args: &mut Vec<String>) -> Result<Options, String> {
    let mut take = |name: &str| -> Option<String> {
        if let Some(i) = args.iter().position(|a| a == name) {
            let value = args.get(i + 1).cloned();
            let end = (i + 2).min(args.len());
            args.drain(i..end);
            return value;
        }
        let prefix = format!("{name}=");
        let i = args.iter().position(|a| a.starts_with(&prefix))?;
        Some(args.remove(i)[prefix.len()..].to_string())
    };
    let transport = take("--transport").or_else(|| std::env::var(iroh::TRANSPORT_ENV).ok());
    let relay = take("--iroh-relay").or_else(|| std::env::var(iroh::IROH_RELAY_ENV).ok());
    Ok(Options {
        transport: iroh::Setting::parse(transport.as_deref().unwrap_or_default())?,
        iroh_relay: iroh::parse_relay(relay.as_deref()).map_err(|e| format!("--iroh-relay: {e}"))?,
    })
}

/// The command-line subcommands. Returns the process exit code.
async fn command(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("version" | "--version" | "-V") => {
            println!("radix-connector-mcp {}", update::CURRENT);
            0
        }
        Some("check-update") => match update::check().await {
            Ok(found) => {
                println!("{}", found.describe());
                // 0 = up to date, 10 = an update is available: scripts can branch on it.
                if found.newer {
                    10
                } else {
                    0
                }
            }
            Err(e) => {
                eprintln!("radix-connector-mcp: check-update failed: {e}");
                1
            }
        },
        Some("update") => {
            let tag = args
                .iter()
                .position(|a| a == "--tag")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            let force = args.iter().any(|a| a == "--force");
            match update::update(tag, force).await {
                Ok(report) => {
                    println!("{report}");
                    0
                }
                Err(e) => {
                    eprintln!("radix-connector-mcp: update failed: {e}");
                    1
                }
            }
        }
        Some("relay") => {
            let listen = args
                .iter()
                .position(|a| a == "--listen")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_else(|| format!("0.0.0.0:{}", radixdlt_connect_webrtc::relay::DEFAULT_RELAY_PORT));
            let turn = args
                .iter()
                .position(|a| a == "--turn")
                .and_then(|i| args.get(i + 1))
                .cloned();
            let turn_password = args
                .iter()
                .position(|a| a == "--turn-password")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .or_else(|| std::env::var("RADIX_CONNECT_TURN_PASSWORD").ok());
            if let Some(turn) = turn {
                let Some(password) = turn_password else {
                    eprintln!(
                        "radix-connector-mcp: --turn needs --turn-password (or RADIX_CONNECT_TURN_PASSWORD)"
                    );
                    return 2;
                };
                let listener = match tokio::net::TcpListener::bind(&turn).await {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("radix-connector-mcp: cannot listen on {turn}: {e}");
                        return 1;
                    }
                };
                eprintln!("TURN (TCP) on {turn}: in the wallet's server add  stun:{turn}  and  turn:{turn}?transport=tcp  user radix");
                tokio::task::spawn_local(async move {
                    if let Err(e) = radixdlt_connect_webrtc::turn_server::serve_turn(
                        listener,
                        radixdlt_connect_webrtc::turn_server::TurnConfig::loopback(&password),
                    )
                    .await
                    {
                        eprintln!("radix-connector-mcp: TURN stopped: {e}");
                    }
                });
            }
            relay(&listen).await
        }
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("radix-connector-mcp: unknown command '{other}'\n\n{USAGE}");
            2
        }
        None => 2,
    }
}

/// Runs the local signaling relay in the foreground, saying where the wallet can reach it.
async fn relay(listen: &str) -> i32 {
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("radix-connector-mcp: cannot listen on {listen}: {e}");
            return 1;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
    eprintln!(
        "radix-connector-mcp {} — local signaling relay on {listen}",
        update::CURRENT
    );
    eprintln!("In the Radix Wallet: Settings › Preferences › Signaling Servers › add one with URL");
    for address in local_ipv4() {
        eprintln!("    ws://{address}:{port}/");
    }
    eprintln!("(the address of THIS computer on the cable or Wi-Fi the phone is on), and make it current.");
    eprintln!(
        "Connectors on this computer use it when it is listed in {} or ~/.config/radix-connect/relays",
        radixdlt_connect_webrtc::RELAYS_ENV
    );
    match radixdlt_connect_webrtc::relay::serve_relay(listener).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("radix-connector-mcp: relay stopped: {e}");
            1
        }
    }
}

/// This computer's IPv4 addresses, best effort (Linux/macOS `ip`/`ifconfig`), loopback excluded.
fn local_ipv4() -> Vec<String> {
    let output = std::process::Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .output()
        .or_else(|_| std::process::Command::new("ifconfig").output());
    let Ok(output) = output else {
        return vec!["<this computer's IP>".to_string()];
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found: Vec<String> = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|w| w[0] == "inet")
        .map(|w| w[1].split('/').next().unwrap_or_default().to_string())
        .filter(|ip| !ip.starts_with("127."))
        .collect();
    found.dedup();
    if found.is_empty() {
        found.push("<this computer's IP>".to_string());
    }
    found
}

/// Reads MCP messages line-by-line from stdin and writes one response line per
/// request to stdout. Returns the process exit code.
async fn run(options: Options) -> i32 {
    let app = match App::new(options) {
        Ok(app) => Rc::new(app),
        Err(err) => {
            eprintln!("radix-connector-mcp: fatal: {err}");
            return 1;
        }
    };
    eprintln!(
        "radix-connector-mcp {} ready (config: {}; transport: {}; iroh relays: {})",
        env!("CARGO_PKG_VERSION"),
        app.config_path().display(),
        app.iroh.setting.label(),
        iroh::relay_label(&app.iroh.relay),
    );
    // The iroh libraries' own events (collisions, replaced connections, relay state) go to the
    // connector's log too.
    trace::install(app.log.clone(), app.iroh.held_links());
    app.log.event(
        "connector_started",
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"), "transport": app.iroh.setting.label(),
            "iroh_relays": iroh::relay_label(&app.iroh.relay),
        }),
    );

    // Tells the wallets removed here, while this connector runs.
    tokio::task::spawn_local(link_removal::deliver_notices(app.clone()));

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();

    // One writer owns stdout, so answers from concurrent tool calls never interleave mid-line.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::task::spawn_local(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(response) = out_rx.recv().await {
            if stdout.write_all(response.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });

    loop {
        let line = match stdin.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break, // stdin closed: the client went away
            Err(err) => {
                eprintln!("radix-connector-mcp: stdin error: {err}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        rpc::dispatch(&app, &line, &out_tx);
    }
    // A client may send its calls and close stdin straight away (`echo … | radix-connector-mcp`):
    // the calls already started still finish — each is bounded by its own timeout — and answer.
    while app.busy() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // Telling each wallet, and letting go of the links for other connector processes.
    iroh::close_all(&app, "connector exiting").await;
    app.log.event("connector_exiting", serde_json::json!({}));
    drop(out_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn server_options_come_out_of_the_command_line_in_either_form() {
        let mut given = args(&["--transport", "auto", "--iroh-relay=https://relay.example.com"]);
        let options = server_options(&mut given).unwrap();
        assert_eq!(options.transport, iroh::Setting::Auto);
        assert_eq!(options.iroh_relay.as_deref(), Some("https://relay.example.com"));
        assert!(given.is_empty(), "{given:?}");

        // Subcommands keep their own arguments.
        let mut given = args(&["relay", "--listen", "127.0.0.1:8787"]);
        server_options(&mut given).unwrap();
        assert_eq!(given, args(&["relay", "--listen", "127.0.0.1:8787"]));

        let mut given = args(&["--transport=webrtc"]);
        assert_eq!(
            server_options(&mut given).unwrap().transport,
            iroh::Setting::Webrtc
        );

        // Neither flag nor variable: auto, both transports at once.
        assert_eq!(Options::default().transport, iroh::Setting::Auto);
        if std::env::var_os(iroh::TRANSPORT_ENV).is_none() {
            assert_eq!(
                server_options(&mut args(&[])).unwrap().transport,
                iroh::Setting::Auto
            );
        }

        assert!(server_options(&mut args(&["--transport", "pigeon"])).is_err());
        assert!(server_options(&mut args(&["--iroh-relay", "http://relay.example.com"])).is_err());
    }
}
