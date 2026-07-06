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

/// Outcome of a Gateway transaction dry-run.
pub struct PreviewOutcome {
    pub success: bool,
    pub message: Option<String>,
}

/// Dry-runs a manifest (with its blobs) on the Gateway with free credit and no
/// real signatures, so a deploy can be validated before the user approves and
/// pays. `Err` is an infra failure (couldn't run the preview); `Ok(outcome)`
/// carries the simulated result.
pub async fn preview(
    network: Network,
    manifest: &str,
    blobs_hex: &[String],
) -> Result<PreviewOutcome, String> {
    let client = reqwest::Client::new();
    let base = base_url(network);

    // 1) Current epoch, required by the preview request's validity window.
    let construction = client
        .post(format!("{base}/transaction/construction"))
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("gateway construction failed: {e}"))?;
    let construction_body: serde_json::Value = construction
        .json()
        .await
        .map_err(|e| format!("gateway construction parse failed: {e}"))?;
    let epoch = construction_body
        .get("ledger_state")
        .and_then(|l| l.get("epoch"))
        .and_then(|e| e.as_u64())
        .ok_or("gateway construction without epoch")?;

    // 2) Preview with free credit and assumed proofs (no real fee, no signature).
    // The validity window must be narrow — end_epoch_exclusive = epoch + 2.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let body = json!({
        "manifest": manifest,
        "start_epoch_inclusive": epoch,
        "end_epoch_exclusive": epoch + 2,
        "tip_percentage": 0,
        "nonce": nonce,
        "signer_public_keys": [],
        "flags": { "use_free_credit": true, "assume_all_signature_proofs": true, "skip_epoch_check": false },
        "blobs_hex": blobs_hex,
    });
    let resp = client
        .post(format!("{base}/transaction/preview"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("preview request failed: {e}"))?;
    let st = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("preview read failed: {e}"))?;
    if st.is_client_error() {
        // A 4xx means the transaction itself is invalid/unpreparable — a
        // definitive failure the caller should not sign, not a transient error.
        return Ok(PreviewOutcome {
            success: false,
            message: Some(format!("gateway rejected the transaction ({st}): {text}")),
        });
    }
    if !st.is_success() {
        return Err(format!("gateway preview HTTP {st}: {text}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("preview parse failed: {e}"))?;
    let receipt = v.get("receipt");
    let status_str = receipt
        .and_then(|r| r.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("Unknown");
    let success = status_str.eq_ignore_ascii_case("Succeeded");
    let message = receipt
        .and_then(|r| r.get("error_message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .or_else(|| (!success).then(|| status_str.to_string()));
    Ok(PreviewOutcome { success, message })
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
