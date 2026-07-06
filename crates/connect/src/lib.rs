//! radixdlt-connect — The Radix Connect protocol in native Rust (signaling +
//! WebRTC).
//!
//! A native replacement for the Node connector (`@radixdlt/radix-connect-webrtc` +
//! `@roamhq/wrtc`): pair with the mobile wallet, open a WebRTC channel and exchange
//! wallet interactions (ROLA account proofs, transactions, pre-authorizations).
//!
//! The entry point is [`Connector`], which carries the ICE/signaling configuration.
//! By default it uses the public Radix ICE set and signaling server; override the
//! ICE servers with [`Connector::with_ice_servers`] to use your own TURN relay.
//!
//! The wallet-interaction message schema is shared with the Iroh transport via
//! [`radixdlt_connect_types`] (re-exported here), so both transports speak exactly
//! the same JSON.
//!
//! This is a pure library: it never prints. User-facing error text is localized to
//! the system language.

pub mod chunking;
mod connector;
pub mod crypto;
mod error;
mod signaling;
pub mod state;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub use connector::{radix_default_ice_servers, Channel, IceServer};
pub use error::ConnectError;
pub use radixdlt_connect_types::{
    account_proof_request, account_request, extract_accounts, extract_persona_name, extract_proofs,
    extract_signed_partial_transaction, extract_transaction_intent_hash, pre_authorization_request,
    transaction_request, DappContext, WalletInteractionError,
};
pub use signaling::SIGNALING_BASE;
pub use state::LinkState;

/// Waits for a `linkClient` message from the wallet (pairing) on an established
/// channel. Returns `(walletPublicKey, signatureHex)`.
pub async fn await_link_client(
    channel: &mut Channel,
    wait: Duration,
) -> Result<(String, Option<String>), ConnectError> {
    loop {
        let msg = channel.recv_message(wait).await?;
        if msg.get("discriminator").and_then(|d| d.as_str()) == Some("linkClient") {
            let pk = msg
                .get("publicKey")
                .and_then(|p| p.as_str())
                .ok_or_else(|| ConnectError::Protocol("linkClient without publicKey".into()))?;
            let sig = msg
                .get("signature")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            return Ok((pk.to_string(), sig));
        }
    }
}

/// Sends a wallet interaction and waits for the response whose `interactionId`
/// matches it, discarding everything else until `overall_timeout` elapses.
///
/// The wallet's dAppRequestQueue can hold stale requests from earlier attempts;
/// their responses arrive first and would otherwise be mistaken for ours (e.g. a
/// "response without oneTimeAccounts" on an account-proof request). Requiring an
/// EXACT id match keeps us waiting for the user's actual approval. Only if our
/// request somehow had no id (should never happen) is the first message accepted.
async fn send_and_await_response(
    channel: &mut Channel,
    interaction: &Value,
    overall_timeout: Duration,
) -> Result<Value, ConnectError> {
    let want_id = interaction
        .get("interactionId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    channel
        .send_message(interaction, Duration::from_secs(15))
        .await?;
    let deadline = Instant::now() + overall_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let resp = channel.recv_message(remaining).await?;
        let got = resp
            .get("interactionId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if want_id.is_empty() || got == want_id {
            return Ok(resp);
        }
    }
}

/// A Radix Connect client carrying the ICE/signaling configuration.
pub struct Connector {
    ice_servers: Vec<IceServer>,
    signaling_base: String,
}

impl Default for Connector {
    fn default() -> Self {
        Connector {
            ice_servers: radix_default_ice_servers(),
            signaling_base: SIGNALING_BASE.to_string(),
        }
    }
}

impl Connector {
    /// A connector with the default public Radix ICE set and signaling server.
    pub fn new() -> Self {
        Connector::default()
    }

    /// Overrides the ICE (STUN/TURN) servers (e.g. to use your own TURN relay).
    pub fn with_ice_servers(mut self, servers: Vec<IceServer>) -> Self {
        self.ice_servers = servers;
        self
    }

    /// Overrides the signaling server base URL.
    pub fn with_signaling_base(mut self, base: impl Into<String>) -> Self {
        self.signaling_base = base.into();
        self
    }

    async fn establish(
        &self,
        password: &[u8],
        open_timeout: Duration,
    ) -> Result<Channel, ConnectError> {
        connector::establish(
            &self.ice_servers,
            &self.signaling_base,
            password,
            open_timeout,
        )
        .await
    }

    /// With an already-paired link password, asks the wallet to sign a ROLA account
    /// proof and returns the wallet's response (containing `proofs`).
    pub async fn request_account_proof(
        &self,
        password: &[u8],
        challenge_hex: &str,
        ctx: &DappContext,
        request_persona: bool,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let mut channel = self.establish(password, overall_timeout).await?;
        let interaction = account_proof_request(challenge_hex, ctx, request_persona);
        send_and_await_response(&mut channel, &interaction, overall_timeout).await
    }

    /// Asks the wallet to SHARE its account(s) without a ROLA proof (the
    /// lightweight account-discovery flow). Returns the raw response; read the
    /// addresses with [`extract_accounts`].
    pub async fn request_accounts(
        &self,
        password: &[u8],
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, ConnectError> {
        let mut channel = self.establish(password, overall_timeout).await?;
        let interaction = account_request(ctx);
        send_and_await_response(&mut channel, &interaction, overall_timeout).await
    }

    /// Sends a TRANSACTION MANIFEST to the wallet for the owner to sign and submit.
    /// Returns the `transactionIntentHash` on success.
    pub async fn request_transaction(
        &self,
        password: &[u8],
        manifest: &str,
        message: &str,
        blobs: &[String],
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<String, ConnectError> {
        let mut channel = self.establish(password, overall_timeout).await?;
        let interaction = transaction_request(manifest, message, blobs, ctx);
        let response = send_and_await_response(&mut channel, &interaction, overall_timeout).await?;
        Ok(extract_transaction_intent_hash(&response)?)
    }

    /// Asks the mobile wallet for a PRE-AUTHORIZATION (subintent V2): the user
    /// approves a `subintentManifest` and the wallet returns a
    /// `signedPartialTransaction` (hex) WITHOUT submitting it.
    pub async fn request_pre_authorization(
        &self,
        password: &[u8],
        subintent_manifest: &str,
        message: &str,
        expire_after_seconds: u64,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<String, ConnectError> {
        let mut channel = self.establish(password, overall_timeout).await?;
        let interaction =
            pre_authorization_request(subintent_manifest, message, expire_after_seconds, ctx);
        let response = send_and_await_response(&mut channel, &interaction, overall_timeout).await?;
        Ok(extract_signed_partial_transaction(&response)?)
    }

    /// Pairing: generates the QR payload (signed with the connector identity),
    /// establishes the channel with the scanning wallet, waits for its `linkClient`,
    /// verifies its signature and returns `(walletPublicKey, password_bytes)` to
    /// persist the link.
    ///
    /// `identity_private_key_hex` is the connector's persistent Ed25519 private key.
    /// `on_qr` receives the exact QR JSON string to render.
    pub async fn pair<F: FnOnce(String)>(
        &self,
        identity_private_key_hex: &str,
        identity_public_key_hex: &str,
        on_qr: F,
        timeout: Duration,
    ) -> Result<(String, Vec<u8>), ConnectError> {
        use ed25519_dalek::{Signer, SigningKey};
        use rand_core::RngCore;

        // New 32-byte link password.
        let mut password = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut password);

        // Connector identity signs blake2b("L"‖password).
        let sk_bytes = hex::decode(identity_private_key_hex)
            .map_err(|e| ConnectError::Crypto(format!("priv hex: {e}")))?;
        let sk_arr: [u8; 32] = sk_bytes
            .as_slice()
            .try_into()
            .map_err(|_| ConnectError::Crypto("private key is not 32 bytes".into()))?;
        let signing = SigningKey::from_bytes(&sk_arr);
        let link_msg = crypto::linking_message(&password);
        let signature = hex::encode(signing.sign(&link_msg).to_bytes());

        let qr = json!({
            "password": hex::encode(password),
            "publicKey": identity_public_key_hex,
            "signature": signature,
            "purpose": "general",
        });
        on_qr(qr.to_string());

        // Establish the channel with the scanning wallet and await its linkClient.
        let mut channel = self.establish(&password, timeout).await?;
        let (wallet_pk, sig) = await_link_client(&mut channel, timeout).await?;

        // Verify the wallet's linking signature.
        if let Some(sig_hex) = sig {
            use ed25519_dalek::{Signature, VerifyingKey};
            let pk_bytes = hex::decode(&wallet_pk)
                .map_err(|e| ConnectError::Crypto(format!("wallet pk hex: {e}")))?;
            let pk_arr: [u8; 32] = pk_bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectError::Crypto("wallet pk is not 32 bytes".into()))?;
            let vk = VerifyingKey::from_bytes(&pk_arr)
                .map_err(|e| ConnectError::Crypto(format!("invalid wallet pk: {e}")))?;
            let sig_bytes =
                hex::decode(&sig_hex).map_err(|e| ConnectError::Crypto(format!("sig hex: {e}")))?;
            let sig_arr: [u8; 64] = sig_bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectError::Crypto("signature is not 64 bytes".into()))?;
            vk.verify_strict(&link_msg, &Signature::from_bytes(&sig_arr))
                .map_err(|_| ConnectError::Crypto("INVALID wallet linking signature".into()))?;
        }

        Ok((wallet_pk, password.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ice_set_has_stun_and_turn() {
        let servers = radix_default_ice_servers();
        assert!(
            servers.iter().any(|s| s.username.is_empty()),
            "expected at least one STUN server"
        );
        assert!(
            servers.iter().any(|s| !s.username.is_empty()),
            "expected at least one TURN server"
        );
    }

    #[test]
    fn account_proof_interaction_shape() {
        let ctx = DappContext::new(2, "account_tdx_2_x", "http://localhost");
        let v = account_proof_request("aa", &ctx, true);
        assert_eq!(v["metadata"]["networkId"], 2);
        assert_eq!(v["items"]["oneTimeAccounts"]["challenge"], "aa");
        assert!(v["items"]["oneTimePersonaData"]["isRequestingName"]
            .as_bool()
            .unwrap());
    }

    #[test]
    fn extract_proofs_reports_wallet_failure() {
        let resp = json!({ "discriminator": "failure", "error": "rejectedByUser" });
        assert_eq!(
            extract_proofs(&resp),
            Err(WalletInteractionError::WalletRejected(
                "rejectedByUser".into()
            ))
        );
    }
}
