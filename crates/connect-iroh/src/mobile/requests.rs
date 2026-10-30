//! The `request_*` helpers: [`MobileWalletConnector::exchange`] with a request built by
//! [`radixdlt_connect_types`], method for method the same as `radixdlt_connect::Connector`'s.

use std::time::Duration;

use serde_json::Value;

use super::{
    account_proof_request_sharing, account_request, authorized_request, extract_signed_partial_transaction,
    extract_transaction_intent_hash, login_request, pre_authorization_request, transaction_request,
    unauthorized_request, AccountsWanted, Auth, AuthorizedRequest, DappContext, MobileError,
    MobileWalletConnector, OwnershipWanted, PersonaDataWanted, PersonaRequest,
};

impl MobileWalletConnector {
    async fn send_request(
        &self,
        password: &[u8],
        interaction: &Value,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        self.exchange(password, interaction, overall_timeout, &mut |_| {})
            .await
    }

    /// Asks the wallet to sign a ROLA account proof and returns its answer (with `proofs`, read
    /// with [`extract_proofs`](super::extract_proofs) and verified with `radixdlt-rola`).
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange).
    pub async fn request_account_proof(
        &self,
        password: &[u8],
        challenge_hex: &str,
        ctx: &DappContext,
        request_persona: bool,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        let share = if request_persona {
            PersonaRequest::NAME
        } else {
            PersonaRequest::NONE
        };
        self.request_account_proof_sharing(password, challenge_hex, ctx, share, overall_timeout)
            .await
    }

    /// The same, saying exactly what the person is asked to share about themselves. Nothing
    /// shared changes the proof.
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange).
    pub async fn request_account_proof_sharing(
        &self,
        password: &[u8],
        challenge_hex: &str,
        ctx: &DappContext,
        share: PersonaRequest,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        let interaction = account_proof_request_sharing(challenge_hex, ctx, share);
        self.send_request(password, &interaction, overall_timeout).await
    }

    /// Asks the wallet to LOG IN: the persona signs the challenge too, so the person is proven,
    /// not just an account. Read the answer with [`extract_login`](super::extract_login) (and the
    /// account proofs, over the same challenge, with [`extract_proofs`](super::extract_proofs)).
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange).
    pub async fn request_login(
        &self,
        password: &[u8],
        challenge_hex: &str,
        ctx: &DappContext,
        share: PersonaRequest,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        let interaction = login_request(challenge_hex, ctx, share);
        self.send_request(password, &interaction, overall_timeout).await
    }

    /// Sends any AUTHORIZED request ([`AuthorizedRequest`]) and returns the raw answer.
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange).
    pub async fn request_authorized(
        &self,
        password: &[u8],
        request: &AuthorizedRequest,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        let interaction = authorized_request(request, ctx);
        self.send_request(password, &interaction, overall_timeout).await
    }

    /// Asks the wallet to prove EXACT accounts (and the persona, with `own.identity`) as a persona
    /// already logged in to this dApp (`identity`). Read the proofs with
    /// [`extract_ownership_proofs`](super::extract_ownership_proofs).
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange).
    pub async fn request_ownership(
        &self,
        password: &[u8],
        identity: &str,
        own: &OwnershipWanted,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        let request = AuthorizedRequest {
            proof_of_ownership: Some(own.clone()),
            ..AuthorizedRequest::new(Auth::UsePersona(identity.to_string()))
        };
        self.request_authorized(password, &request, ctx, overall_timeout)
            .await
    }

    /// Sends any UNAUTHORIZED request: one-time accounts and/or persona data.
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange).
    pub async fn request_unauthorized(
        &self,
        password: &[u8],
        accounts: Option<&AccountsWanted>,
        persona_data: Option<PersonaDataWanted>,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        let interaction = unauthorized_request(accounts, persona_data, ctx);
        self.send_request(password, &interaction, overall_timeout).await
    }

    /// Asks the wallet to SHARE its account(s) without a proof. Read the addresses with
    /// [`extract_accounts`](super::extract_accounts).
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange).
    pub async fn request_accounts(
        &self,
        password: &[u8],
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<Value, MobileError> {
        let interaction = account_request(ctx);
        self.send_request(password, &interaction, overall_timeout).await
    }

    /// Sends a TRANSACTION MANIFEST for the person to sign and submit; returns the
    /// `transactionIntentHash`.
    ///
    /// # Errors
    /// As [`exchange`](Self::exchange), plus [`MobileError::WalletRejected`] when the person
    /// rejected it and [`MobileError::Protocol`] when the answer carries no intent hash.
    pub async fn request_transaction(
        &self,
        password: &[u8],
        manifest: &str,
        message: &str,
        blobs: &[String],
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<String, MobileError> {
        let interaction = transaction_request(manifest, message, blobs, ctx);
        let response = self.send_request(password, &interaction, overall_timeout).await?;
        Ok(extract_transaction_intent_hash(&response)?)
    }

    /// Asks for a PRE-AUTHORIZATION (subintent): the person approves a `subintentManifest` and the
    /// wallet returns the `signedPartialTransaction` (hex) WITHOUT submitting it.
    ///
    /// # Errors
    /// As [`request_transaction`](Self::request_transaction).
    pub async fn request_pre_authorization(
        &self,
        password: &[u8],
        subintent_manifest: &str,
        message: &str,
        expire_after_seconds: u64,
        ctx: &DappContext,
        overall_timeout: Duration,
    ) -> Result<String, MobileError> {
        let interaction = pre_authorization_request(subintent_manifest, message, expire_after_seconds, ctx);
        let response = self.send_request(password, &interaction, overall_timeout).await?;
        Ok(extract_signed_partial_transaction(&response)?)
    }
}
