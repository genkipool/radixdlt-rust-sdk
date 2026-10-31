//! The wallet requests, as `radixdlt-connect-webrtc`'s `Connector` has them, over a [`Link`]: the
//! same builders, the same answers, over whichever transport the request races to.

use std::time::Duration;

use radixdlt_connect_types::{
    account_proof_request_sharing, account_request, authorized_request, extract_signed_partial_transaction,
    extract_transaction_intent_hash, login_request, pre_authorization_request, transaction_request,
    unauthorized_request, AccountsWanted, Auth, AuthorizedRequest, DappContext, OwnershipWanted,
    PersonaDataWanted, PersonaRequest,
};
use radixdlt_connect_webrtc::state::Link;
use serde_json::Value;

use crate::{Error, WalletConnector};

impl WalletConnector {
    /// Asks for a ROLA account proof over `challenge_hex`, sharing what `share` says of the persona.
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn request_account_proof_sharing(
        &self,
        link: &Link,
        challenge_hex: &str,
        ctx: &DappContext,
        share: PersonaRequest,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.exchange(
            link,
            &account_proof_request_sharing(challenge_hex, ctx, share),
            timeout,
        )
        .await
    }

    /// A login: the persona and an account, both proven over `challenge_hex`.
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn request_login(
        &self,
        link: &Link,
        challenge_hex: &str,
        ctx: &DappContext,
        share: PersonaRequest,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.exchange(link, &login_request(challenge_hex, ctx, share), timeout)
            .await
    }

    /// Any authorized request.
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn request_authorized(
        &self,
        link: &Link,
        request: &AuthorizedRequest,
        ctx: &DappContext,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.exchange(link, &authorized_request(request, ctx), timeout)
            .await
    }

    /// Proof that the persona `identity` owns what `own` says, the persona only confirming.
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn request_ownership(
        &self,
        link: &Link,
        identity: &str,
        own: &OwnershipWanted,
        ctx: &DappContext,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let request = AuthorizedRequest {
            proof_of_ownership: Some(own.clone()),
            ..AuthorizedRequest::new(Auth::UsePersona(identity.to_string()))
        };
        self.request_authorized(link, &request, ctx, timeout).await
    }

    /// Any unauthorized request.
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn request_unauthorized(
        &self,
        link: &Link,
        accounts: Option<&AccountsWanted>,
        persona_data: Option<PersonaDataWanted>,
        ctx: &DappContext,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.exchange(link, &unauthorized_request(accounts, persona_data, ctx), timeout)
            .await
    }

    /// The accounts the person shares, with no proof.
    ///
    /// # Errors
    /// How far it got (see [`Error`]).
    pub async fn request_accounts(
        &self,
        link: &Link,
        ctx: &DappContext,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.exchange(link, &account_request(ctx), timeout).await
    }

    /// A transaction to review, sign and submit: its intent hash once submitted.
    ///
    /// # Errors
    /// How far it got (see [`Error`]); [`Error::WalletRejected`] when it was not submitted.
    pub async fn request_transaction(
        &self,
        link: &Link,
        manifest: &str,
        message: &str,
        blobs: &[String],
        ctx: &DappContext,
        timeout: Duration,
    ) -> Result<String, Error> {
        let answer = self
            .exchange(link, &transaction_request(manifest, message, blobs, ctx), timeout)
            .await?;
        Ok(extract_transaction_intent_hash(&answer)?)
    }

    /// A pre-authorization (a subintent to sign, not submit): the signed partial transaction.
    ///
    /// # Errors
    /// How far it got (see [`Error`]); [`Error::WalletRejected`] when it was not signed.
    pub async fn request_pre_authorization(
        &self,
        link: &Link,
        subintent_manifest: &str,
        message: &str,
        expire_after_seconds: u64,
        ctx: &DappContext,
        timeout: Duration,
    ) -> Result<String, Error> {
        let interaction = pre_authorization_request(subintent_manifest, message, expire_after_seconds, ctx);
        let answer = self.exchange(link, &interaction, timeout).await?;
        Ok(extract_signed_partial_transaction(&answer)?)
    }
}
