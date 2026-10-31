//! Ending a connector process without leaving its wallets waiting.
//!
//! A session that is only dropped tells its wallet nothing: the wallet believes it is connected
//! until QUIC's idle timeout (30 s), and the link's next session — the program's next run, the
//! next login — waits that long for it. Destructors cannot fix that: `std::process::exit` runs
//! none, and a runtime that is shutting down stops before the close is sent. So a program closes
//! its sessions on its way out, and these make that one line.

use std::future::Future;

use crate::WalletConnector;

/// Resolves when the process is asked to stop: SIGTERM (a service manager, `kill`, a client
/// ending the process) or Ctrl-C. Where a signal cannot be listened for, it never resolves for
/// it, rather than at once.
pub async fn stop_requested() {
    #[cfg(unix)]
    if let Ok(mut terminate) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        tokio::select! {
            _ = terminate.recv() => {}
            () = ctrl_c() => {}
        }
        return;
    }
    ctrl_c().await;
}

async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

impl WalletConnector {
    /// Runs `work` — the program's main loop: an MCP server reading its client, a daemon — until
    /// it ends or the process is asked to stop ([`stop_requested`]), then closes every session,
    /// telling each wallet, so that none is left waiting. `None`: it was asked to stop first.
    ///
    /// ```no_run
    /// # async fn serve(_: &radixdlt_connect_auto::WalletConnector) {}
    /// # async fn run() {
    /// let wallet = radixdlt_connect_auto::WalletConnector::new();
    /// wallet.run_until_stopped(serve(&wallet)).await;
    /// # }
    /// ```
    pub async fn run_until_stopped<F: Future>(&self, work: F) -> Option<F::Output> {
        // With a state file, its removal notices are delivered meanwhile (never ends by itself).
        let outcome = tokio::select! {
            output = work => Some(output),
            () = stop_requested() => None,
            () = self.deliver_removal_notices(), if self.state.is_some() => None,
        };
        self.close().await;
        outcome
    }
}
