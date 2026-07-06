//! High-level Radix Connect flows over the iroh transport, using the **shared
//! wallet-interaction schema** ([`radixdlt_connect_types`]) — the exact same JSON
//! the WebRTC transport uses. Both ends are pure Rust (SDK-to-SDK): the "wallet"
//! side is a [`Wallet`] that holds a key and answers interactions; the "dApp" side
//! sends requests over an [`IrohChannel`] and parses responses.
//!
//! Supported interactions: account proof (ROLA), transaction (sign + submit) and
//! pre-authorization (sign a subintent).

use ed25519_dalek::{Signer, SigningKey};
use radixdlt_address::virtual_account_address;
use radixdlt_connect_types::{
    account_proof_request, account_proof_response, extract_proofs,
    extract_signed_partial_transaction, extract_transaction_intent_hash, failure_response,
    interaction_discriminator, parse_account_proof_request, parse_pre_authorization_request,
    parse_transaction_request, pre_authorization_request, pre_authorization_response,
    transaction_request, transaction_response, AccountProofRequest, PreAuthorizationRequest,
    TransactionRequest,
};
use radixdlt_gateway_tx::{Ed25519PrivateKey, Gateway};
use radixdlt_keystore::KeyFile;
use radixdlt_rola::{signature_message, verify_account_proof, AccountProof};
use serde_json::Value;

use crate::{IrohChannel, IrohError};

pub use radixdlt_connect_types::DappContext;

// =============================== dApp side ===============================

/// Sends an account-proof request, receives the response and **verifies it
/// natively** against `ctx`. Returns the verified [`AccountProof`].
pub async fn request_account_proof(
    channel: &mut IrohChannel,
    challenge_hex: &str,
    ctx: &DappContext,
) -> Result<AccountProof, IrohError> {
    channel
        .send_message(&account_proof_request(challenge_hex, ctx, false))
        .await?;
    let response = channel.recv_message().await?;
    let proofs = extract_proofs(&response)?;
    let (address, proof) = proofs
        .into_iter()
        .next()
        .ok_or_else(|| IrohError::Protocol("wallet returned no proofs".into()))?;

    let ap = AccountProof {
        address,
        public_key_hex: proof
            .get("publicKey")
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_string(),
        signature_hex: proof
            .get("signature")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    verify_account_proof(
        &ap,
        challenge_hex,
        &ctx.dapp_definition,
        &ctx.origin,
        ctx.network_id,
    )
    .map_err(|e| IrohError::Protocol(format!("ROLA verification failed: {e}")))?;
    Ok(ap)
}

/// Sends a transaction manifest for the wallet to sign and submit. Returns the
/// transaction intent hash on success.
pub async fn request_transaction(
    channel: &mut IrohChannel,
    manifest: &str,
    ctx: &DappContext,
) -> Result<String, IrohError> {
    channel
        .send_message(&transaction_request(manifest, "", &[], ctx))
        .await?;
    let response = channel.recv_message().await?;
    Ok(extract_transaction_intent_hash(&response)?)
}

/// Sends a subintent for the wallet to pre-authorize (sign WITHOUT submitting).
/// Returns the signed partial transaction (hex).
pub async fn request_pre_authorization(
    channel: &mut IrohChannel,
    subintent_manifest: &str,
    expire_after_seconds: u64,
    ctx: &DappContext,
) -> Result<String, IrohError> {
    channel
        .send_message(&pre_authorization_request(
            subintent_manifest,
            "",
            expire_after_seconds,
            ctx,
        ))
        .await?;
    let response = channel.recv_message().await?;
    Ok(extract_signed_partial_transaction(&response)?)
}

// =============================== wallet side ===============================

/// A pure-Rust "wallet": holds an Ed25519 signing key and answers wallet
/// interactions over an iroh channel. This is the role the mobile wallet plays in
/// the WebRTC flow.
pub struct Wallet {
    signing_key: SigningKey,
    public_key_hex: String,
    address: String,
    gateway: Gateway,
}

impl Wallet {
    /// Builds a wallet from an Ed25519 signing key for the given network. The
    /// Gateway used for transaction submission defaults to the public one for the
    /// network (override with [`with_gateway`](Self::with_gateway)).
    pub fn new(signing_key: SigningKey, network_id: u8) -> Result<Self, IrohError> {
        let public_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let address = virtual_account_address(&public_key_hex, network_id)
            .map_err(|e| IrohError::Protocol(format!("address derivation: {e}")))?;
        let gateway = if network_id == 1 {
            Gateway::mainnet()
        } else {
            Gateway::stokenet()
        };
        Ok(Wallet {
            signing_key,
            public_key_hex,
            address,
            gateway,
        })
    }

    /// Builds a wallet from an encrypted key file (`radixdlt-keystore`).
    pub fn from_key_file(key_file: &KeyFile, passphrase: &str) -> Result<Self, IrohError> {
        let signing_key = key_file
            .signing_key(passphrase)
            .map_err(|e| IrohError::Protocol(format!("unlock key: {e}")))?;
        Wallet::new(signing_key, key_file.network_id)
    }

    /// Overrides the Gateway used for transaction submission.
    pub fn with_gateway(mut self, gateway: Gateway) -> Self {
        self.gateway = gateway;
        self
    }

    /// The account address this wallet proves.
    pub fn address(&self) -> &str {
        &self.address
    }

    fn radix_key(&self) -> Result<Ed25519PrivateKey, IrohError> {
        Ed25519PrivateKey::from_bytes(&self.signing_key.to_bytes())
            .map_err(|_| IrohError::Protocol("invalid signing key".into()))
    }

    /// Reads one wallet interaction from `channel` and answers it.
    pub async fn answer(&self, channel: &mut IrohChannel) -> Result<(), IrohError> {
        let request = channel.recv_message().await?;
        let response = match interaction_discriminator(&request) {
            Some("unauthorizedRequest") | Some("authorizedRequest") => {
                self.account_proof_response(&request)
            }
            Some("transaction") => self.transaction_response(&request).await,
            Some("preAuthorizationRequest") => self.pre_authorization_response(&request).await,
            other => failure_response(
                "",
                &format!("unsupported interaction: {}", other.unwrap_or("(none)")),
            ),
        };
        channel.send_message(&response).await
    }

    fn account_proof_response(&self, request: &Value) -> Value {
        let AccountProofRequest {
            interaction_id,
            challenge_hex,
            dapp_definition,
            origin,
            ..
        } = match parse_account_proof_request(request) {
            Some(r) => r,
            None => return failure_response("", "malformed account-proof request"),
        };
        let message = match signature_message(&challenge_hex, &dapp_definition, &origin) {
            Ok(m) => m,
            Err(e) => return failure_response(&interaction_id, &format!("bad request: {e}")),
        };
        let signature = hex::encode(self.signing_key.sign(&message).to_bytes());
        account_proof_response(
            &interaction_id,
            &self.address,
            &self.public_key_hex,
            &signature,
            None,
        )
    }

    async fn transaction_response(&self, request: &Value) -> Value {
        let TransactionRequest {
            interaction_id,
            manifest,
            ..
        } = match parse_transaction_request(request) {
            Some(r) => r,
            None => return failure_response("", "malformed transaction request"),
        };
        let key = match self.radix_key() {
            Ok(k) => k,
            Err(e) => return failure_response(&interaction_id, &e.to_string()),
        };
        let compiled = match self.gateway.compile_manifest_v1(&manifest) {
            Ok(m) => m,
            Err(e) => return failure_response(&interaction_id, &e.to_string()),
        };
        let tx = match self
            .gateway
            .build_notarized(compiled, &[&key], &key, false)
            .await
        {
            Ok(t) => t,
            Err(e) => return failure_response(&interaction_id, &e.to_string()),
        };
        match self.gateway.submit_and_wait(&tx).await {
            Ok(_) => transaction_response(&interaction_id, &tx.txid),
            // Submitted; return the hash even if the wait failed (e.g. timeout).
            Err(_) => transaction_response(&interaction_id, &tx.txid),
        }
    }

    async fn pre_authorization_response(&self, request: &Value) -> Value {
        let PreAuthorizationRequest {
            interaction_id,
            subintent_manifest,
            expire_after_seconds,
        } = match parse_pre_authorization_request(request) {
            Some(r) => r,
            None => return failure_response("", "malformed pre-authorization request"),
        };
        let key = match self.radix_key() {
            Ok(k) => k,
            Err(e) => return failure_response(&interaction_id, &e.to_string()),
        };
        let start = match self.gateway.current_epoch().await {
            Ok(e) => e,
            Err(e) => return failure_response(&interaction_id, &e.to_string()),
        };
        // Radix epochs are ~5 minutes; convert the requested expiry to an epoch window.
        let window = (expire_after_seconds / 300).max(1) + 1;
        match self
            .gateway
            .sign_subintent(&subintent_manifest, start, start + window, &key)
        {
            Ok(hex) => pre_authorization_response(&interaction_id, &hex),
            Err(e) => failure_response(&interaction_id, &e.to_string()),
        }
    }
}
