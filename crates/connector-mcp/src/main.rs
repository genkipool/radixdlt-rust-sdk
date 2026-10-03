//! radix-connector-mcp — a local MCP (Model Context Protocol) server that gives
//! AI agents (Claude Code/Desktop, Antigravity, Cursor, …) the ability to pair a
//! Radix Wallet and get transactions **signed on the user's own machine**.
//!
//! Why local: signing a Radix transaction means keeping a live Radix Connect
//! (WebRTC) channel open to the phone during the whole approval. A stateless,
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
mod exchange;
mod gateway;
mod outbox;
mod qr;
mod requests;
mod rpc;
mod store;
mod tools;
mod update;

use std::rc::Rc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::rpc::App;

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => local.block_on(&runtime, run()),
        Some(_) => local.block_on(&runtime, command(&args)),
    };
    std::process::exit(code);
}

const USAGE: &str = "\
radix-connector-mcp — local MCP server that gets Radix transactions signed on the phone.

Usage:
  radix-connector-mcp                 run as an MCP server over stdio (what MCP clients launch)
  radix-connector-mcp version         print the installed version
  radix-connector-mcp check-update    tell whether a newer release is published
  radix-connector-mcp update [--tag connector-vX.Y.Z] [--force]
                                      download the newest (or the given) release and install it
                                      in place of this binary, keeping a backup beside it

Updating never touches connector.json: a paired wallet stays paired.";

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

/// Reads MCP messages line-by-line from stdin and writes one response line per
/// request to stdout. Returns the process exit code.
async fn run() -> i32 {
    let app = match App::new() {
        Ok(app) => Rc::new(app),
        Err(err) => {
            eprintln!("radix-connector-mcp: fatal: {err}");
            return 1;
        }
    };
    eprintln!(
        "radix-connector-mcp {} ready (config: {})",
        env!("CARGO_PKG_VERSION"),
        app.config_path().display()
    );

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
    drop(out_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    0
}
