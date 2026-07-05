//! Minimal Radix Gateway client — just the transaction-status read used to
//! confirm a commit after signing. Kept as a plain HTTP call (no `radix-engine`
//! dependency) so it coexists with the `webrtc` tree pulled in by
//! `radixdlt-connect`.

use serde_json::json;

use crate::tools::Network;

fn base_url(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "https://mainnet.radixdlt.com",
        Network::Stokenet => "https://stokenet.radixdlt.com",
    }
}

/// Fetches the current status of a transaction by its `txid_...` intent hash.
/// Returns the raw Gateway status string (e.g. `CommittedSuccess`, `Pending`).
pub async fn transaction_status(network: Network, intent_hash: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/transaction/status", base_url(network)))
        .json(&json!({ "intent_hash": intent_hash }))
        .send()
        .await
        .map_err(|e| format!("Gateway request failed: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("Gateway request failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("Gateway returned HTTP {status}: {text}"));
    }

    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("unexpected Gateway response: {e}"))?;
    Ok(body
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("Unknown")
        .to_string())
}
