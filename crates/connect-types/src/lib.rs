//! radixdlt-connect-types — Transport-agnostic Radix Connect message schema.
//!
//! The Radix Connect `WalletInteraction` JSON (account proofs, transactions,
//! pre-authorizations) is independent of how it travels. This crate holds the
//! request builders, response builders and parsers for that schema, so both the
//! WebRTC transport (`radixdlt-connect`) and the Iroh transport
//! (`radixdlt-connect-iroh`) speak exactly the same thing.
//!
//! Two sides:
//!   * **dApp side** — builds *requests* and parses *responses*
//!     ([`account_proof_request`], [`extract_proofs`], …).
//!   * **wallet side** — parses *requests* and builds *responses*
//!     ([`parse_account_proof_request`], [`account_proof_response`], …).
//!
//! User-facing error text is localized to the system language.

use radixdlt_i18n::{tr, Lang};
use serde_json::{json, Value};
use uuid::Uuid;

/// The dApp context sent with every interaction (fixed per application).
#[derive(Debug, Clone)]
pub struct DappContext {
    pub network_id: u8,
    pub dapp_definition: String,
    pub origin: String,
}

impl DappContext {
    pub fn new(
        network_id: u8,
        dapp_definition: impl Into<String>,
        origin: impl Into<String>,
    ) -> Self {
        DappContext {
            network_id,
            dapp_definition: dapp_definition.into(),
            origin: origin.into(),
        }
    }
}

/// Errors parsing a wallet interaction or its response. `Display` is localized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletInteractionError {
    /// The wallet rejected or cancelled the request.
    WalletRejected(String),
    /// The message did not match the expected schema.
    Protocol(String),
}

impl std::fmt::Display for WalletInteractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            WalletInteractionError::WalletRejected(e) => tr!(
                lang,
                format!("the wallet rejected the request: {e}"),
                format!("la wallet rechazó la solicitud: {e}")
            ),
            WalletInteractionError::Protocol(e) => tr!(
                lang,
                format!("protocol error: {e}"),
                format!("error de protocolo: {e}")
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for WalletInteractionError {}

fn metadata(ctx: &DappContext) -> Value {
    json!({
        "version": 2,
        "networkId": ctx.network_id,
        "dAppDefinitionAddress": ctx.dapp_definition,
        "origin": ctx.origin,
    })
}

fn is_failure(response: &Value) -> bool {
    response.get("discriminator").and_then(|d| d.as_str()) == Some("failure")
}

/// Returns the `items.discriminator` of a request (e.g. `unauthorizedRequest`,
/// `transaction`, `preAuthorizationRequest`).
pub fn interaction_discriminator(request: &Value) -> Option<&str> {
    request.get("items")?.get("discriminator")?.as_str()
}

// =============================== dApp side ===============================

/// Builds an account-proof request (`oneTimeAccounts` with a ROLA challenge), with
/// an optional request for the person's name.
pub fn account_proof_request(
    challenge_hex: &str,
    ctx: &DappContext,
    request_persona: bool,
) -> Value {
    let mut items = json!({
        "discriminator": "unauthorizedRequest",
        "oneTimeAccounts": {
            "challenge": challenge_hex,
            "numberOfAccounts": { "quantifier": "atLeast", "quantity": 1 }
        }
    });
    if request_persona {
        items["oneTimePersonaData"] = json!({ "isRequestingName": true });
    }
    json!({ "interactionId": Uuid::new_v4().to_string(), "metadata": metadata(ctx), "items": items })
}

/// Builds a transaction request (a manifest for the wallet to sign and submit).
pub fn transaction_request(manifest: &str, message: &str, ctx: &DappContext) -> Value {
    json!({
        "interactionId": Uuid::new_v4().to_string(),
        "metadata": metadata(ctx),
        "items": {
            "discriminator": "transaction",
            "send": { "version": 1, "transactionManifest": manifest, "blobs": [], "message": message }
        }
    })
}

/// Builds a pre-authorization request (a subintent for the wallet to sign).
pub fn pre_authorization_request(
    subintent_manifest: &str,
    message: &str,
    expire_after_seconds: u64,
    ctx: &DappContext,
) -> Value {
    json!({
        "interactionId": Uuid::new_v4().to_string(),
        "metadata": metadata(ctx),
        "items": {
            "discriminator": "preAuthorizationRequest",
            "request": {
                "discriminator": "subintent",
                "version": 1,
                "manifestVersion": 2,
                "subintentManifest": subintent_manifest,
                "blobs": [],
                "message": message,
                "expiration": { "discriminator": "expireAfterDelay", "expireAfterSeconds": expire_after_seconds }
            }
        }
    })
}

/// Fails if the response is a wallet `failure` (rejection/cancellation).
pub fn check_failure(response: &Value) -> Result<(), WalletInteractionError> {
    if is_failure(response) {
        return Err(WalletInteractionError::WalletRejected(
            response
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown")
                .to_string(),
        ));
    }
    Ok(())
}

/// Extracts `(accountAddress, proof)` pairs from an account-proof response.
pub fn extract_proofs(response: &Value) -> Result<Vec<(String, Value)>, WalletInteractionError> {
    check_failure(response)?;
    let ota = response
        .get("items")
        .and_then(|i| i.get("oneTimeAccounts"))
        .ok_or_else(|| {
            WalletInteractionError::Protocol("response without oneTimeAccounts".into())
        })?;
    let proofs = ota
        .get("proofs")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for p in proofs {
        if let (Some(addr), Some(proof)) = (
            p.get("accountAddress").and_then(|a| a.as_str()),
            p.get("proof"),
        ) {
            out.push((addr.to_string(), proof.clone()));
        }
    }
    Ok(out)
}

/// Extracts the person's name from a response (if `oneTimePersonaData` is present).
/// Handles both a plain string and the `{ givenNames, familyName, nickname, variant }`
/// shape (order depends on `variant`).
pub fn extract_persona_name(response: &Value) -> Option<String> {
    let name = response
        .get("items")?
        .get("oneTimePersonaData")?
        .get("name")?;
    if let Some(s) = name.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    let given = name
        .get("givenNames")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let family = name
        .get("familyName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let nick = name
        .get("nickname")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let variant = name
        .get("variant")
        .and_then(|v| v.as_str())
        .unwrap_or("western");
    let full = if variant.eq_ignore_ascii_case("eastern") {
        format!("{family} {given}")
    } else {
        format!("{given} {family}")
    };
    let full = full.trim().to_string();
    if !full.is_empty() {
        Some(full)
    } else if !nick.is_empty() {
        Some(nick.to_string())
    } else {
        None
    }
}

/// Extracts the `transactionIntentHash` from a transaction response.
pub fn extract_transaction_intent_hash(response: &Value) -> Result<String, WalletInteractionError> {
    check_failure(response)?;
    response
        .get("items")
        .and_then(|i| i.get("send"))
        .and_then(|s| s.get("transactionIntentHash"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            WalletInteractionError::Protocol("response without transactionIntentHash".into())
        })
}

/// Extracts the `signedPartialTransaction` from a pre-authorization response
/// (searched flexibly, since the exact nesting varies by wallet version).
pub fn extract_signed_partial_transaction(
    response: &Value,
) -> Result<String, WalletInteractionError> {
    check_failure(response)?;
    fn find(v: &Value) -> Option<String> {
        if let Some(s) = v.get("signedPartialTransaction").and_then(|s| s.as_str()) {
            return Some(s.to_string());
        }
        match v {
            Value::Object(m) => m.values().find_map(find),
            Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    find(response).ok_or_else(|| {
        WalletInteractionError::Protocol("response without signedPartialTransaction".into())
    })
}

// =============================== wallet side ===============================

fn interaction_id(request: &Value) -> String {
    request
        .get("interactionId")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string()
}

/// A parsed account-proof request (the wallet must sign the ROLA challenge).
#[derive(Debug, Clone)]
pub struct AccountProofRequest {
    pub interaction_id: String,
    pub challenge_hex: String,
    pub network_id: u8,
    pub dapp_definition: String,
    pub origin: String,
}

/// Parses an account-proof request. Returns `None` if it is not one.
pub fn parse_account_proof_request(request: &Value) -> Option<AccountProofRequest> {
    let ota = request.get("items")?.get("oneTimeAccounts")?;
    let challenge_hex = ota.get("challenge")?.as_str()?.to_string();
    let md = request.get("metadata")?;
    Some(AccountProofRequest {
        interaction_id: interaction_id(request),
        challenge_hex,
        network_id: md.get("networkId").and_then(|n| n.as_u64()).unwrap_or(0) as u8,
        dapp_definition: md
            .get("dAppDefinitionAddress")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
        origin: md
            .get("origin")
            .and_then(|o| o.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// A parsed transaction request (the wallet must sign and submit the manifest).
#[derive(Debug, Clone)]
pub struct TransactionRequest {
    pub interaction_id: String,
    pub manifest: String,
    pub message: String,
}

/// Parses a transaction request. Returns `None` if it is not one.
pub fn parse_transaction_request(request: &Value) -> Option<TransactionRequest> {
    let send = request.get("items")?.get("send")?;
    Some(TransactionRequest {
        interaction_id: interaction_id(request),
        manifest: send.get("transactionManifest")?.as_str()?.to_string(),
        message: send
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// A parsed pre-authorization request (the wallet must sign the subintent).
#[derive(Debug, Clone)]
pub struct PreAuthorizationRequest {
    pub interaction_id: String,
    pub subintent_manifest: String,
    pub expire_after_seconds: u64,
}

/// Parses a pre-authorization request. Returns `None` if it is not one.
pub fn parse_pre_authorization_request(request: &Value) -> Option<PreAuthorizationRequest> {
    let req = request.get("items")?.get("request")?;
    Some(PreAuthorizationRequest {
        interaction_id: interaction_id(request),
        subintent_manifest: req.get("subintentManifest")?.as_str()?.to_string(),
        expire_after_seconds: req
            .get("expiration")
            .and_then(|e| e.get("expireAfterSeconds"))
            .and_then(|s| s.as_u64())
            .unwrap_or(0),
    })
}

/// Builds the wallet's account-proof response (read back with [`extract_proofs`] and
/// [`extract_persona_name`]).
pub fn account_proof_response(
    interaction_id: &str,
    address: &str,
    public_key_hex: &str,
    signature_hex: &str,
    persona_name: Option<&str>,
) -> Value {
    let mut items = json!({
        "discriminator": "unauthorizedRequest",
        "oneTimeAccounts": {
            "accounts": [ { "address": address } ],
            "proofs": [ {
                "accountAddress": address,
                "proof": { "publicKey": public_key_hex, "signature": signature_hex, "curve": "curve25519" }
            } ]
        }
    });
    if let Some(name) = persona_name {
        items["oneTimePersonaData"] = json!({ "name": name });
    }
    json!({ "discriminator": "success", "interactionId": interaction_id, "items": items })
}

/// Builds the wallet's transaction response (read back with
/// [`extract_transaction_intent_hash`]).
pub fn transaction_response(interaction_id: &str, transaction_intent_hash: &str) -> Value {
    json!({
        "discriminator": "success",
        "interactionId": interaction_id,
        "items": { "discriminator": "transaction", "send": { "transactionIntentHash": transaction_intent_hash } }
    })
}

/// Builds the wallet's pre-authorization response (read back with
/// [`extract_signed_partial_transaction`]).
pub fn pre_authorization_response(interaction_id: &str, signed_partial_transaction: &str) -> Value {
    json!({
        "discriminator": "success",
        "interactionId": interaction_id,
        "items": {
            "discriminator": "preAuthorizationRequest",
            "response": { "signedPartialTransaction": signed_partial_transaction }
        }
    })
}

/// Builds a wallet `failure` response (read back as [`WalletInteractionError`]).
pub fn failure_response(interaction_id: &str, error: &str) -> Value {
    json!({ "discriminator": "failure", "interactionId": interaction_id, "error": error })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> DappContext {
        DappContext::new(2, "account_tdx_2_dapp", "https://example.com")
    }

    #[test]
    fn account_proof_round_trips() {
        let req = account_proof_request("aa", &ctx(), true);
        let parsed = parse_account_proof_request(&req).unwrap();
        assert_eq!(parsed.challenge_hex, "aa");
        assert_eq!(parsed.network_id, 2);
        assert_eq!(parsed.dapp_definition, "account_tdx_2_dapp");

        let resp = account_proof_response(
            &parsed.interaction_id,
            "account_tdx_2_x",
            "pub",
            "sig",
            Some("Ada Lovelace"),
        );
        let proofs = extract_proofs(&resp).unwrap();
        assert_eq!(proofs.len(), 1);
        assert_eq!(proofs[0].0, "account_tdx_2_x");
        assert_eq!(proofs[0].1["publicKey"], "pub");
        assert_eq!(extract_persona_name(&resp).as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn transaction_round_trips() {
        let req = transaction_request("MANIFEST", "hi", &ctx());
        let parsed = parse_transaction_request(&req).unwrap();
        assert_eq!(parsed.manifest, "MANIFEST");
        let resp = transaction_response(&parsed.interaction_id, "txid_tdx_2_abc");
        assert_eq!(
            extract_transaction_intent_hash(&resp).unwrap(),
            "txid_tdx_2_abc"
        );
    }

    #[test]
    fn pre_authorization_round_trips() {
        let req = pre_authorization_request("YIELD_TO_PARENT;", "", 600, &ctx());
        let parsed = parse_pre_authorization_request(&req).unwrap();
        assert_eq!(parsed.subintent_manifest, "YIELD_TO_PARENT;");
        assert_eq!(parsed.expire_after_seconds, 600);
        let resp = pre_authorization_response(&parsed.interaction_id, "deadbeef");
        assert_eq!(
            extract_signed_partial_transaction(&resp).unwrap(),
            "deadbeef"
        );
    }

    #[test]
    fn failure_is_detected() {
        let resp = failure_response("id", "rejectedByUser");
        assert_eq!(
            extract_proofs(&resp),
            Err(WalletInteractionError::WalletRejected(
                "rejectedByUser".into()
            ))
        );
    }
}
