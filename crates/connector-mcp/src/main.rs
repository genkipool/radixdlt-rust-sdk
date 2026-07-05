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

mod gateway;
mod qr;
mod rpc;
mod store;
mod tools;

use std::rc::Rc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::rpc::App;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to start the Tokio runtime");
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&runtime, run());
    std::process::exit(code);
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
    let mut stdout = tokio::io::stdout();

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
        // A single input message never produces more than one output line, and
        // notifications produce none.
        if let Some(response) = rpc::handle_line(&app, &line).await {
            if stdout.write_all(response.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    }
    0
}
