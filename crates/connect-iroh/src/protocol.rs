//! High-level Radix Connect flows over the iroh transport, using the **shared
//! wallet-interaction schema** ([`radixdlt_connect_types`]) — the exact same JSON
//! the WebRTC transport uses. Both ends are pure Rust (SDK-to-SDK): the "wallet"
//! side is a [`Wallet`](crate::protocol::Wallet) that holds a key and answers interactions; the "dApp" side
//! sends requests over an [`IrohChannel`] and parses responses.
//!
//! Supported interactions: account proof (ROLA), transaction (sign + submit) and
//! pre-authorization (sign a subintent).
//!
//! # Nothing is signed without approval
//!
//! A [`Wallet`](crate::protocol::Wallet) holds a private key and anyone who can open a channel to it can send it
//! requests. It therefore signs nothing on its own: every request goes to an [`Approver`](crate::protocol::Approver) — the
//! person at the keyboard, a policy, an allowlist of dApps — which sees who asks (the peer's
//! authenticated `EndpointId`, and what the request claims about its dApp and origin) and exactly
//! what is asked. Every [`Approver`](crate::protocol::Approver) method says no unless implemented, so an approver written
//! for logins can never approve a transaction by omission; [`DenyAll`](crate::protocol::DenyAll) refuses everything.
//! Requests for another network, or malformed ones, are refused before the approver is asked.
//!
//! A refusal reaches the dApp as `rejectedByUser`, the code the Radix Wallet uses.

use std::future::Future;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use radixdlt_address::virtual_account_address;
use radixdlt_connect_types::{
    account_proof_request, account_proof_response, extract_proofs, extract_signed_partial_transaction,
    extract_transaction_intent_hash, failure_response, interaction_discriminator,
    parse_account_proof_request, parse_pre_authorization_request, parse_transaction_request,
    pre_authorization_request, pre_authorization_response, transaction_request, transaction_response,
};
use radixdlt_gateway_tx::{Ed25519PrivateKey, Gateway, GatewayError, TxStatus};
use radixdlt_keystore::KeyFile;
use radixdlt_rola::{signature_message, verify_account_proof, AccountProof};
use serde_json::Value;

use crate::{EndpointId, IrohChannel, IrohError};

pub use radixdlt_connect_types::{
    AccountProofRequest, DappContext, PreAuthorizationRequest, TransactionRequest,
};

// =============================== dApp side ===============================

/// Sends an account-proof request, receives the response and **verifies it
/// natively** against `ctx`. Returns the verified [`AccountProof`].
///
/// # Errors
/// [`IrohError::Rejected`] when the wallet refuses (`rejectedByUser`, `wrongNetwork`, …),
/// [`IrohError::Protocol`] when the proof does not verify, and the channel's errors.
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
/// transaction intent hash once the transaction has COMMITTED SUCCESSFULLY.
///
/// # Errors
/// [`IrohError::Rejected`] when the wallet refuses, or when the transaction was submitted but
/// did not commit successfully — the reason names the status
/// (`submittedTransactionHasFailedTransactionStatus`, `…RejectedTransactionStatus`, or
/// `failedToPollSubmittedTransaction` when its outcome is not known yet) and the intent hash.
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
///
/// # Errors
/// [`IrohError::Rejected`] when the wallet refuses, and the channel's errors.
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

/// Who is asking, as an [`Approver`] sees it.
///
/// Only `peer` is authenticated — by the QUIC handshake, it is the key the other end holds. The
/// rest is what the request CLAIMS: a peer can write any dApp definition and origin it likes, so
/// trust them only as far as you trust `peer`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Requester {
    /// The peer's `EndpointId`, authenticated by the handshake.
    pub peer: EndpointId,
    /// The network the request is for (already checked to be the wallet's).
    pub network_id: u8,
    /// The dApp definition the request claims.
    pub dapp_definition: String,
    /// The origin the request claims.
    pub origin: String,
}

impl Requester {
    fn of(peer: EndpointId, request: &Value) -> Self {
        let metadata = request.get("metadata");
        let text = |key: &str| {
            metadata
                .and_then(|m| m.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Requester {
            peer,
            // Out of range is not folded into a valid id: it becomes 0, which no wallet is on.
            network_id: metadata
                .and_then(|m| m.get("networkId"))
                .and_then(Value::as_u64)
                .and_then(|n| u8::try_from(n).ok())
                .unwrap_or(0),
            dapp_definition: text("dAppDefinitionAddress"),
            origin: text("origin"),
        }
    }
}

/// Decides what a [`Wallet`] signs. One method per kind of request, each refusing unless
/// implemented: implement only what you mean to allow.
///
/// Methods are asynchronous, so an approver can ask a person and wait for the answer. The
/// request reaching the approver is already checked: it is for the wallet's network, a ROLA
/// challenge is 32 bytes, a transaction manifest compiles. What it does NOT know is whether the
/// request is wanted — that is the approver's call.
pub trait Approver: Send + Sync {
    /// Whether to sign a ROLA proof of the wallet's account for `request` — a login to
    /// `request.dapp_definition` at `request.origin`. Refuses unless implemented.
    fn approve_account_proof(
        &self,
        _from: &Requester,
        _request: &AccountProofRequest,
    ) -> impl Future<Output = bool> + Send {
        async { false }
    }

    /// Whether to sign AND SUBMIT `request.manifest`, paying its fees from the wallet's account.
    /// Refuses unless implemented.
    fn approve_transaction(
        &self,
        _from: &Requester,
        _request: &TransactionRequest,
    ) -> impl Future<Output = bool> + Send {
        async { false }
    }

    /// Whether to sign the subintent `request.subintent_manifest`, which the requester may then
    /// complete and submit at any time before it expires. Refuses unless implemented.
    fn approve_pre_authorization(
        &self,
        _from: &Requester,
        _request: &PreAuthorizationRequest,
    ) -> impl Future<Output = bool> + Send {
        async { false }
    }
}

/// An [`Approver`] that refuses everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DenyAll;

impl Approver for DenyAll {}

/// The public Gateway of a network the SDK knows: 1 mainnet, 2 stokenet.
///
/// # Errors
/// [`IrohError::Protocol`] for any other network: its Gateway must be given explicitly.
pub fn public_gateway(network_id: u8) -> Result<Gateway, IrohError> {
    match network_id {
        1 => Ok(Gateway::mainnet()),
        2 => Ok(Gateway::stokenet()),
        other => Err(IrohError::Protocol(format!(
            "no public Gateway is known for network {other}"
        ))),
    }
}

/// How long the wallet waits for a submitted transaction to commit (40 polls, 2 s apart).
const COMMIT_POLLS: u32 = 40;
const COMMIT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A pure-Rust "wallet": holds an Ed25519 signing key and answers wallet interactions over an
/// iroh channel — only those its [`Approver`] approves. This is the role the mobile wallet plays
/// in the WebRTC flow, where a person approves each request on the phone.
pub struct Wallet {
    signing_key: SigningKey,
    public_key_hex: String,
    address: String,
    network_id: u8,
    gateway: Gateway,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .field("network_id", &self.network_id)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    /// Builds a wallet from an Ed25519 signing key for the given network, submitting through the
    /// network's public Gateway ([`public_gateway`]; replace it with
    /// [`with_gateway`](Self::with_gateway)).
    ///
    /// # Errors
    /// [`IrohError::Protocol`] for a network other than 1 (mainnet) or 2 (stokenet).
    pub fn new(signing_key: SigningKey, network_id: u8) -> Result<Self, IrohError> {
        let gateway = public_gateway(network_id)?;
        let public_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let address = virtual_account_address(&public_key_hex, network_id)
            .map_err(|e| IrohError::Protocol(format!("address derivation: {e}")))?;
        Ok(Wallet {
            signing_key,
            public_key_hex,
            address,
            network_id,
            gateway,
        })
    }

    /// Builds a wallet from an encrypted key file (`radixdlt-keystore`).
    ///
    /// # Errors
    /// [`IrohError::Protocol`] when the key cannot be unlocked, or as [`Wallet::new`].
    pub fn from_key_file(key_file: &KeyFile, passphrase: &str) -> Result<Self, IrohError> {
        let signing_key = key_file
            .signing_key(passphrase)
            .map_err(|e| IrohError::Protocol(format!("unlock key: {e}")))?;
        Wallet::new(signing_key, key_file.network_id)
    }

    /// Replaces the Gateway used for transactions and pre-authorizations.
    ///
    /// # Errors
    /// [`IrohError::Protocol`] when the Gateway is for another network than the wallet's.
    pub fn with_gateway(mut self, gateway: Gateway) -> Result<Self, IrohError> {
        let gateway_network = gateway.network().id;
        if gateway_network != self.network_id {
            return Err(IrohError::Protocol(format!(
                "the Gateway is for network {gateway_network}, the wallet for network {}",
                self.network_id
            )));
        }
        self.gateway = gateway;
        Ok(self)
    }

    /// The account address this wallet proves.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The network this wallet is on.
    pub fn network_id(&self) -> u8 {
        self.network_id
    }

    fn radix_key(&self) -> Result<Ed25519PrivateKey, IrohError> {
        Ed25519PrivateKey::from_bytes(&self.signing_key.to_bytes())
            .map_err(|_| IrohError::Protocol("invalid signing key".into()))
    }

    /// Reads one wallet interaction from `channel` and answers it: signed only if `approver`
    /// approves it, a `failure` otherwise.
    ///
    /// # Errors
    /// The channel's errors. A refused request is not an error here: it is answered.
    pub async fn answer(&self, channel: &mut IrohChannel, approver: &impl Approver) -> Result<(), IrohError> {
        let request = channel.recv_message().await?;
        let from = Requester::of(channel.remote_id(), &request);
        let response = self.respond(&from, &request, approver).await;
        channel.send_message(&response).await
    }

    async fn respond(&self, from: &Requester, request: &Value, approver: &impl Approver) -> Value {
        let interaction_id = request
            .get("interactionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = interaction_discriminator(request);
        if kind.is_some() && from.network_id != self.network_id {
            return failure(
                interaction_id,
                "wrongNetwork",
                &format!(
                    "the request is for network {}, this wallet is on network {}",
                    from.network_id, self.network_id
                ),
            );
        }
        match kind {
            Some("unauthorizedRequest") | Some("authorizedRequest") => {
                self.account_proof_response(from, request, approver).await
            }
            Some("transaction") => self.transaction_response(from, request, approver).await,
            Some("preAuthorizationRequest") => self.pre_authorization_response(from, request, approver).await,
            other => failure(
                interaction_id,
                "invalidRequest",
                &format!("unsupported interaction: {}", other.unwrap_or("(none)")),
            ),
        }
    }

    async fn account_proof_response(
        &self,
        from: &Requester,
        request: &Value,
        approver: &impl Approver,
    ) -> Value {
        let Some(request) = parse_account_proof_request(request) else {
            return failure("", "invalidRequest", "malformed account-proof request");
        };
        if let Err(why) = self.check_account_proof(&request) {
            return failure(&request.interaction_id, "invalidRequest", &why);
        }
        if !approver.approve_account_proof(from, &request).await {
            return failure(&request.interaction_id, "rejectedByUser", "not approved");
        }
        let message =
            match signature_message(&request.challenge_hex, &request.dapp_definition, &request.origin) {
                Ok(m) => m,
                Err(e) => return failure(&request.interaction_id, "invalidRequest", &e.to_string()),
            };
        let signature = hex::encode(self.signing_key.sign(&message).to_bytes());
        account_proof_response(
            &request.interaction_id,
            &self.address,
            &self.public_key_hex,
            &signature,
            None,
        )
    }

    /// What must hold before a ROLA proof is even put to the approver.
    fn check_account_proof(&self, request: &AccountProofRequest) -> Result<(), String> {
        // ROLA challenges are 32 random bytes. Anything else is not a challenge this protocol
        // issues — and signing arbitrary bytes on request is how a signature gets repurposed.
        let challenge_ok = request.challenge_hex.len() == 64 && hex::decode(&request.challenge_hex).is_ok();
        if !challenge_ok {
            return Err("the challenge must be 32 bytes, hex".into());
        }
        // A dApp definition is an account on the wallet's own network.
        let prefix = format!("account_{}1", self.gateway.network().hrp_suffix);
        if !request.dapp_definition.starts_with(&prefix) || request.dapp_definition.len() > 255 {
            return Err(format!(
                "the dApp definition must be an account address ({prefix}…)"
            ));
        }
        if request.origin.is_empty() {
            return Err("the origin is missing".into());
        }
        Ok(())
    }

    async fn transaction_response(
        &self,
        from: &Requester,
        request: &Value,
        approver: &impl Approver,
    ) -> Value {
        let Some(request) = parse_transaction_request(request) else {
            return failure("", "invalidRequest", "malformed transaction request");
        };
        let id = request.interaction_id.as_str();
        // Compiled first, so the approver is never asked about a manifest that cannot run here.
        let compiled = match self.gateway.compile_manifest_v1(&request.manifest) {
            Ok(m) => m,
            Err(e) => return failure(id, "invalidRequest", &e.to_string()),
        };
        if !approver.approve_transaction(from, &request).await {
            return failure(id, "rejectedByUser", "not approved");
        }
        let key = match self.radix_key() {
            Ok(k) => k,
            Err(e) => return failure(id, "failedToSignTransaction", &e.to_string()),
        };
        let tx = match self.gateway.build_notarized(compiled, &[&key], &key, false).await {
            Ok(t) => t,
            Err(e) => return failure(id, "failedToPrepareTransaction", &e.to_string()),
        };
        if let Err(e) = self.gateway.submit(&tx).await {
            return failure(id, "failedToSubmitTransaction", &format!("{}: {e}", tx.txid));
        }
        let status = self
            .gateway
            .wait_for_commit(&tx.txid, COMMIT_POLLS, COMMIT_POLL_INTERVAL)
            .await;
        submission_outcome(id, &tx.txid, status)
    }

    async fn pre_authorization_response(
        &self,
        from: &Requester,
        request: &Value,
        approver: &impl Approver,
    ) -> Value {
        let Some(request) = parse_pre_authorization_request(request) else {
            return failure("", "invalidRequest", "malformed pre-authorization request");
        };
        let id = request.interaction_id.as_str();
        // Without an expiry a signed subintent never stops being usable.
        if request.expire_after_seconds == 0 {
            return failure(id, "invalidRequest", "a pre-authorization needs an expiration");
        }
        if !approver.approve_pre_authorization(from, &request).await {
            return failure(id, "rejectedByUser", "not approved");
        }
        let key = match self.radix_key() {
            Ok(k) => k,
            Err(e) => return failure(id, "failedToSignTransaction", &e.to_string()),
        };
        let start = match self.gateway.current_epoch().await {
            Ok(e) => e,
            Err(e) => return failure(id, "failedToPrepareTransaction", &e.to_string()),
        };
        let end = start.saturating_add(epoch_window(request.expire_after_seconds));
        match self
            .gateway
            .sign_subintent(&request.subintent_manifest, start, end, &key)
        {
            Ok(hex) => pre_authorization_response(id, &hex),
            Err(e) => failure(id, "failedToSignTransaction", &e.to_string()),
        }
    }
}

/// Radix epochs are ~5 minutes: the expiry as a window of epochs, at least two.
fn epoch_window(expire_after_seconds: u64) -> u64 {
    (expire_after_seconds / 300).max(1).saturating_add(1)
}

/// The answer to a submitted transaction, from what its submission came to. Only a successful
/// commit is a success: a transaction that committed as a failure, was rejected, or whose outcome
/// is not known yet is reported as such, with its intent hash so the dApp can look it up.
fn submission_outcome(interaction_id: &str, txid: &str, status: Result<TxStatus, GatewayError>) -> Value {
    match status {
        Ok(TxStatus::CommittedSuccess) => transaction_response(interaction_id, txid),
        Ok(TxStatus::CommittedFailure) => failure(
            interaction_id,
            "submittedTransactionHasFailedTransactionStatus",
            &format!("{txid} committed with a failure"),
        ),
        Ok(TxStatus::Rejected) => failure(
            interaction_id,
            "submittedTransactionHasRejectedTransactionStatus",
            &format!("{txid} was rejected"),
        ),
        // Submitted, outcome unknown: it may still commit, so it is neither a success nor a
        // failure the dApp could safely retry.
        Ok(TxStatus::Pending) => failure(
            interaction_id,
            "failedToPollSubmittedTransaction",
            &format!("{txid} was submitted and has not committed yet: check its status before retrying"),
        ),
        Err(e) => failure(
            interaction_id,
            "failedToPollSubmittedTransaction",
            &format!("{txid} was submitted; its outcome is unknown ({e}): check its status before retrying"),
        ),
    }
}

/// A `failure` answer with the wallet's error code and a `message` saying why.
fn failure(interaction_id: &str, error: &str, message: &str) -> Value {
    let mut response = failure_response(interaction_id, error);
    if let Some(fields) = response.as_object_mut() {
        fields.insert("message".into(), Value::String(message.into()));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use radixdlt_connect_types::{check_failure, WalletInteractionError};
    use std::sync::Mutex;

    const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
    const ORIGIN: &str = "iroh://radixdlt-connect-iroh";

    fn wallet() -> Wallet {
        Wallet::new(SigningKey::from_bytes(&[7; 32]), 2).unwrap()
    }

    fn peer() -> EndpointId {
        iroh::SecretKey::from_bytes(&[9; 32]).public()
    }

    fn proof_request(network_id: u8, challenge: &str, dapp: &str) -> Value {
        account_proof_request(challenge, &DappContext::new(network_id, dapp, ORIGIN), false)
    }

    async fn answer(wallet: &Wallet, request: &Value, approver: &impl Approver) -> Value {
        let from = Requester::of(peer(), request);
        wallet.respond(&from, request, approver).await
    }

    fn refusal(response: &Value) -> String {
        match check_failure(response) {
            Err(WalletInteractionError::WalletRejected(reason)) => reason,
            other => panic!("expected a failure, got {other:?}: {response}"),
        }
    }

    /// Approves logins only, and remembers who asked.
    #[derive(Default)]
    struct Logins(Mutex<Vec<Requester>>);

    impl Approver for Logins {
        async fn approve_account_proof(&self, from: &Requester, _: &AccountProofRequest) -> bool {
            self.0.lock().unwrap().push(from.clone());
            true
        }
    }

    #[tokio::test]
    async fn nothing_is_signed_without_approval() {
        let response = answer(&wallet(), &proof_request(2, &"ab".repeat(32), DAPP), &DenyAll).await;
        assert!(refusal(&response).starts_with("rejectedByUser"), "{response}");
        assert!(response.get("items").is_none());
    }

    #[tokio::test]
    async fn an_approved_login_is_signed_and_the_approver_sees_who_asked() {
        let wallet = wallet();
        let challenge = "ab".repeat(32);
        let approver = Logins::default();
        let response = answer(&wallet, &proof_request(2, &challenge, DAPP), &approver).await;
        let (address, proof) = extract_proofs(&response).unwrap().remove(0);
        let ap = AccountProof {
            address,
            public_key_hex: proof["publicKey"].as_str().unwrap().into(),
            signature_hex: proof["signature"].as_str().unwrap().into(),
        };
        verify_account_proof(&ap, &challenge, DAPP, ORIGIN, 2).unwrap();
        let asked = approver.0.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].peer, peer());
        assert_eq!(asked[0].dapp_definition, DAPP);
        assert_eq!(asked[0].origin, ORIGIN);
    }

    #[tokio::test]
    async fn an_approver_for_logins_does_not_approve_transactions_or_pre_authorizations() {
        let ctx = DappContext::new(2, DAPP, ORIGIN);
        let tx = transaction_request(
            &format!("CALL_METHOD Address(\"{DAPP}\") \"lock_fee\" Decimal(\"1\");"),
            "",
            &[],
            &ctx,
        );
        let response = answer(&wallet(), &tx, &Logins::default()).await;
        assert!(refusal(&response).starts_with("rejectedByUser"), "{response}");

        let pre = pre_authorization_request("YIELD_TO_PARENT;", "", 600, &ctx);
        let response = answer(&wallet(), &pre, &Logins::default()).await;
        assert!(refusal(&response).starts_with("rejectedByUser"), "{response}");
    }

    #[tokio::test]
    async fn bad_requests_are_refused_before_the_approver_is_asked() {
        let approver = Logins::default();
        let wallet = wallet();
        // Another network.
        let r = answer(&wallet, &proof_request(1, &"ab".repeat(32), DAPP), &approver).await;
        assert!(refusal(&r).starts_with("wrongNetwork"), "{r}");
        // Not a 32-byte challenge.
        let r = answer(&wallet, &proof_request(2, "abcd", DAPP), &approver).await;
        assert!(refusal(&r).starts_with("invalidRequest"), "{r}");
        // A dApp definition from another network.
        let mainnet_dapp = "account_rdx12xsvygvltz4uhsht6tdrfxktzpmnl77r0d40j8agmujgdj022sudkk";
        let r = answer(
            &wallet,
            &proof_request(2, &"ab".repeat(32), mainnet_dapp),
            &approver,
        )
        .await;
        assert!(refusal(&r).starts_with("invalidRequest"), "{r}");
        // A transaction manifest that does not compile.
        let ctx = DappContext::new(2, DAPP, ORIGIN);
        let r = answer(
            &wallet,
            &transaction_request("NOT A MANIFEST", "", &[], &ctx),
            &approver,
        )
        .await;
        assert!(refusal(&r).starts_with("invalidRequest"), "{r}");
        // A pre-authorization that never expires.
        let r = answer(
            &wallet,
            &pre_authorization_request("YIELD_TO_PARENT;", "", 0, &ctx),
            &approver,
        )
        .await;
        assert!(refusal(&r).starts_with("invalidRequest"), "{r}");
        assert!(approver.0.lock().unwrap().is_empty(), "the approver was asked");
    }

    #[test]
    fn only_a_successful_commit_is_a_success() {
        let ok = submission_outcome("i", "txid_x", Ok(TxStatus::CommittedSuccess));
        assert_eq!(extract_transaction_intent_hash(&ok).unwrap(), "txid_x");
        for (status, code) in [
            (
                Ok(TxStatus::CommittedFailure),
                "submittedTransactionHasFailedTransactionStatus",
            ),
            (
                Ok(TxStatus::Rejected),
                "submittedTransactionHasRejectedTransactionStatus",
            ),
            (Ok(TxStatus::Pending), "failedToPollSubmittedTransaction"),
            (Err(GatewayError::Timeout), "failedToPollSubmittedTransaction"),
            (
                Err(GatewayError::Http("down".into())),
                "failedToPollSubmittedTransaction",
            ),
        ] {
            let response = submission_outcome("i", "txid_x", status);
            let reason = refusal(&response);
            assert!(reason.starts_with(code), "{reason}");
            assert!(reason.contains("txid_x"), "the intent hash is reported: {reason}");
            assert!(extract_transaction_intent_hash(&response).is_err());
        }
    }

    #[test]
    fn each_network_gets_its_own_gateway() {
        assert_eq!(public_gateway(1).unwrap().network().id, 1);
        assert_eq!(public_gateway(2).unwrap().network().id, 2);
        assert!(public_gateway(240).is_err());
        assert!(public_gateway(0).is_err());

        let key = || SigningKey::from_bytes(&[7; 32]);
        assert_eq!(Wallet::new(key(), 1).unwrap().gateway.network().id, 1);
        assert_eq!(Wallet::new(key(), 2).unwrap().gateway.network().id, 2);
        assert!(Wallet::new(key(), 3).is_err());
        // A Gateway of another network is refused, not silently used.
        assert!(Wallet::new(key(), 1)
            .unwrap()
            .with_gateway(Gateway::stokenet())
            .is_err());
        assert!(Wallet::new(key(), 2)
            .unwrap()
            .with_gateway(Gateway::stokenet())
            .is_ok());
    }

    #[test]
    fn the_epoch_window_cannot_overflow() {
        assert_eq!(epoch_window(1), 2);
        assert_eq!(epoch_window(600), 3);
        assert_eq!(u64::MAX.saturating_add(epoch_window(u64::MAX)), u64::MAX);
    }
}
