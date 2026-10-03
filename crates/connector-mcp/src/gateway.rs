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

/// What the wallet will find when it checks that `origin` and `dapp_definition` vouch for each
/// other — the check it runs on EVERY request before showing it.
///
/// Only some failures come back as an answer. When the dApp definition and the website disagree,
/// the wallet answers `unknownWebsite`; but when `{origin}/.well-known/radix.json` loads and does
/// not list the dApp definition, the Android wallet drops the request WITHOUT answering — the
/// sender waits out its whole timeout for a prompt that never appears, which looks exactly like a
/// stuck queue. Hence checking before sending.
#[derive(Debug, Default)]
pub struct DappCheck {
    /// Problems that make the wallet refuse or drop the request.
    pub problems: Vec<String>,
    /// Checks that could not be run (network errors): not proof of a problem.
    pub unchecked: Vec<String>,
}

impl DappCheck {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Runs the wallet's dApp verification from here.
pub async fn check_dapp_identity(network: Network, dapp_definition: &str, origin: &str) -> DappCheck {
    let mut check = DappCheck::default();
    if dapp_definition.is_empty() {
        check.problems.push(
            "no dapp_definition: the wallet cannot parse an empty address and answers invalidRequest. \
             Pass dapp_definition or set RADIX_DAPP_DEFINITION_MAINNET/STOKENET."
                .to_string(),
        );
        return check;
    }
    let prefix = match network {
        Network::Mainnet => "account_rdx1",
        Network::Stokenet => "account_tdx_2_1",
    };
    if !dapp_definition.starts_with(prefix) {
        check.problems.push(format!(
            "dapp_definition {dapp_definition} is not an account on {net} (expected {prefix}…): the wallet answers invalidRequest or unknownDappDefinitionAddress.",
            net = network.label(),
        ));
        return check;
    }
    if !origin.starts_with("https://") {
        check.problems.push(format!(
            "origin {origin} is not https: the wallet answers unknownWebsite (unless developer mode is on in the wallet)."
        ));
        return check;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            check
                .unchecked
                .push(format!("could not build an HTTP client: {e}"));
            return check;
        }
    };

    // 1) On-ledger: the account says it is a dApp definition and claims the website.
    let body = json!({
        "addresses": [dapp_definition],
        "opt_ins": { "explicit_metadata": ["account_type", "claimed_websites"] }
    });
    match client
        .post(format!("{}/state/entity/details", base_url(network)))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(v) => {
                let metadata = v
                    .pointer("/items/0/explicit_metadata/items")
                    .and_then(|m| m.as_array())
                    .cloned()
                    .unwrap_or_default();
                let typed = |key: &str| {
                    metadata
                        .iter()
                        .find(|m| m.get("key").and_then(|k| k.as_str()) == Some(key))
                        .and_then(|m| m.pointer("/value/typed").cloned())
                };
                let account_type = typed("account_type")
                    .and_then(|t| t.get("value").and_then(|v| v.as_str()).map(str::to_string));
                if account_type.as_deref() != Some("dapp definition") {
                    check.problems.push(format!(
                        "{dapp_definition} has no account_type = \"dapp definition\" metadata on {net}: the wallet answers wrongAccountType.",
                        net = network.label(),
                    ));
                }
                let claimed: Vec<String> = typed("claimed_websites")
                    .and_then(|t| t.get("values").and_then(|v| v.as_array()).cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim_end_matches('/').to_string()))
                    .collect();
                if !claimed.iter().any(|c| c == origin.trim_end_matches('/')) {
                    check.problems.push(format!(
                        "{dapp_definition} does not claim {origin} (claimed_websites: {claimed:?}): the wallet answers unknownWebsite."
                    ));
                }
            }
            Err(e) => check.unchecked.push(format!("Gateway answer unreadable: {e}")),
        },
        Ok(resp) => check
            .unchecked
            .push(format!("Gateway entity details: HTTP {}", resp.status())),
        Err(e) => check.unchecked.push(format!("Gateway unreachable: {e}")),
    }

    // 2) Off-ledger: the website lists the dApp definition.
    let url = format!("{}/.well-known/radix.json", origin.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(v) => {
                let listed = v.get("dApps").and_then(|d| d.as_array()).is_some_and(|dapps| {
                    dapps.iter().any(|d| {
                        d.get("dAppDefinitionAddress").and_then(|a| a.as_str()) == Some(dapp_definition)
                    })
                });
                if !listed {
                    check.problems.push(format!(
                        "{url} does not list {dapp_definition}: the wallet DROPS the request without answering (the call would wait out its whole timeout)."
                    ));
                }
            }
            Err(_) => check.problems.push(format!(
                "{url} is not valid JSON: the wallet answers radixJsonNotFound."
            )),
        },
        Ok(resp) => check.problems.push(format!(
            "{url} answered HTTP {}: the wallet answers radixJsonNotFound.",
            resp.status()
        )),
        Err(e) => check.unchecked.push(format!("{url} unreachable from here: {e}")),
    }
    check
}
