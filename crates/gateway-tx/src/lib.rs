//! radixdlt-gateway-tx — Radix Gateway client + local transaction signing.
//!
//! Two responsibilities, both in native Rust (no Node, no RET-via-JS):
//!   * [`Gateway`]: read ledger state and submit/track transactions over the
//!     public Gateway HTTP API (epoch, balances, submit, status polling, affected
//!     entities).
//!   * Local building: turn a compiled manifest into a signed + notarized
//!     transaction ready to submit ([`Gateway::notarize_at_epoch`],
//!     [`Gateway::build_notarized`]).
//!
//! This is a pure library: it never reads files, never prints and never exits the
//! process. User-facing error text is localized to the system language.

use std::time::Duration;

use radix_common::prelude::*;
use radix_transactions::manifest::{compile_manifest, compile_manifest_v1, BlobProvider};
use radix_transactions::prelude::*;
use radixdlt_i18n::{tr, Lang};
use rand_core::{OsRng, RngCore};

/// Re-exported so callers can build a signing key without depending on
/// `radix-common` directly: `Ed25519PrivateKey::from_bytes(&secret_32)`.
pub use radix_common::crypto::Ed25519PrivateKey;

/// Default public Gateway base URL for Stokenet (test network).
pub const STOKENET_GATEWAY: &str = "https://stokenet.radixdlt.com";
/// Default public Gateway base URL for Mainnet.
pub const MAINNET_GATEWAY: &str = "https://mainnet.radixdlt.com";

/// Errors from the Gateway client / local building. `Display` is localized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// Network/transport error talking to the Gateway.
    Http(String),
    /// The Gateway replied with an unexpected or unparseable body.
    BadResponse(String),
    /// A transaction manifest failed to compile.
    ManifestCompile(String),
    /// Preparing/encoding the transaction failed.
    Encode(String),
    /// The Gateway rejected the submission.
    SubmitRejected(String),
    /// Timed out waiting for the transaction to be committed.
    Timeout,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            GatewayError::Http(e) => tr!(
                lang,
                format!("Gateway request failed: {e}"),
                format!("la petición al Gateway falló: {e}")
            ),
            GatewayError::BadResponse(e) => tr!(
                lang,
                format!("unexpected Gateway response: {e}"),
                format!("respuesta inesperada del Gateway: {e}")
            ),
            GatewayError::ManifestCompile(e) => tr!(
                lang,
                format!("invalid transaction manifest: {e}"),
                format!("manifiesto de transacción inválido: {e}")
            ),
            GatewayError::Encode(e) => tr!(
                lang,
                format!("could not encode the transaction: {e}"),
                format!("no se pudo codificar la transacción: {e}")
            ),
            GatewayError::SubmitRejected(e) => tr!(
                lang,
                format!("the Gateway rejected the submission: {e}"),
                format!("el Gateway rechazó el envío: {e}")
            ),
            GatewayError::Timeout => tr!(
                lang,
                "timed out waiting for the transaction to commit".to_string(),
                "se agotó el tiempo esperando el commit de la transacción".to_string()
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for GatewayError {}

/// Final or in-progress status of a transaction, as reported by the Gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStatus {
    /// Not yet committed (pending / unknown-but-not-final).
    Pending,
    /// Committed successfully.
    CommittedSuccess,
    /// Committed but the transaction failed.
    CommittedFailure,
    /// Permanently rejected.
    Rejected,
}

impl TxStatus {
    fn parse(s: &str) -> TxStatus {
        match s {
            "CommittedSuccess" => TxStatus::CommittedSuccess,
            "CommittedFailure" => TxStatus::CommittedFailure,
            "Rejected" => TxStatus::Rejected,
            _ => TxStatus::Pending,
        }
    }

    /// Whether the status is final (no further polling will change it).
    pub fn is_final(&self) -> bool {
        !matches!(self, TxStatus::Pending)
    }

    /// Whether the transaction committed successfully.
    pub fn is_success(&self) -> bool {
        matches!(self, TxStatus::CommittedSuccess)
    }
}

/// A locally built, signed and notarized transaction, ready to submit.
#[derive(Debug, Clone)]
pub struct NotarizedTx {
    /// Bech32m transaction intent hash (`txid_...`).
    pub txid: String,
    /// The notarized transaction encoded as hex (what `/transaction/submit` wants).
    pub notarized_hex: String,
}

/// A Radix Gateway client bound to one network.
pub struct Gateway {
    base_url: String,
    network: NetworkDefinition,
    client: reqwest::Client,
}

impl Gateway {
    /// Creates a client for an arbitrary Gateway URL and network.
    pub fn new(base_url: impl Into<String>, network: NetworkDefinition) -> Self {
        Gateway {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            network,
            client: reqwest::Client::new(),
        }
    }

    /// Client for the default public Stokenet Gateway.
    pub fn stokenet() -> Self {
        Gateway::new(STOKENET_GATEWAY, NetworkDefinition::stokenet())
    }

    /// Client for the default public Mainnet Gateway.
    pub fn mainnet() -> Self {
        Gateway::new(MAINNET_GATEWAY, NetworkDefinition::mainnet())
    }

    /// The network this client is bound to.
    pub fn network(&self) -> &NetworkDefinition {
        &self.network
    }

    // ------------------------------- reads -------------------------------

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, GatewayError> {
        let resp = self
            .client
            .post(format!(
                "{}/{}",
                self.base_url,
                path.trim_start_matches('/')
            ))
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| GatewayError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(GatewayError::BadResponse(format!("HTTP {status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| GatewayError::BadResponse(e.to_string()))
    }

    /// Current ledger epoch.
    pub async fn current_epoch(&self) -> Result<u64, GatewayError> {
        let v = self
            .post("status/gateway-status", serde_json::json!({}))
            .await?;
        v["ledger_state"]["epoch"]
            .as_u64()
            .ok_or_else(|| GatewayError::BadResponse("missing ledger_state.epoch".into()))
    }

    /// Balance of a fungible `resource` held by `account` (0 if none).
    pub async fn fungible_balance(
        &self,
        account: &str,
        resource: &str,
    ) -> Result<Decimal, GatewayError> {
        let v = self
            .post(
                "state/entity/page/fungibles/",
                serde_json::json!({ "address": account }),
            )
            .await?;
        let amount = v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|it| it["resource_address"].as_str() == Some(resource))
            .and_then(|it| it["amount"].as_str())
            .and_then(|s| Decimal::try_from(s).ok())
            .unwrap_or(Decimal::ZERO);
        Ok(amount)
    }

    /// XRD balance of `account` (0 if none).
    pub async fn xrd_balance(&self, account: &str) -> Result<Decimal, GatewayError> {
        let xrd = AddressBech32Encoder::new(&self.network)
            .encode(XRD.as_bytes())
            .map_err(|e| GatewayError::Encode(format!("{e:?}")))?;
        self.fungible_balance(account, &xrd).await
    }

    /// Current status of a transaction by its `txid_...` intent hash.
    pub async fn transaction_status(&self, txid: &str) -> Result<TxStatus, GatewayError> {
        let v = self
            .post(
                "transaction/status",
                serde_json::json!({ "intent_hash": txid }),
            )
            .await?;
        Ok(TxStatus::parse(v["status"].as_str().unwrap_or("Unknown")))
    }

    /// Global entities affected by a committed transaction (e.g. newly created
    /// components/resources).
    pub async fn committed_entities(&self, txid: &str) -> Result<Vec<String>, GatewayError> {
        let v = self
            .post(
                "transaction/committed-details",
                serde_json::json!({ "intent_hash": txid, "opt_ins": { "affected_global_entities": true } }),
            )
            .await?;
        Ok(v["transaction"]["affected_global_entities"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Polls until the transaction reaches a final status or `attempts` run out.
    pub async fn wait_for_commit(
        &self,
        txid: &str,
        attempts: u32,
        interval: Duration,
    ) -> Result<TxStatus, GatewayError> {
        for _ in 0..attempts {
            tokio::time::sleep(interval).await;
            let status = self.transaction_status(txid).await?;
            if status.is_final() {
                return Ok(status);
            }
        }
        Err(GatewayError::Timeout)
    }

    // ------------------------------- submit ------------------------------

    /// Submits a notarized transaction (does not wait for the commit).
    pub async fn submit(&self, tx: &NotarizedTx) -> Result<(), GatewayError> {
        let v = self
            .post(
                "transaction/submit",
                serde_json::json!({ "notarized_transaction_hex": tx.notarized_hex }),
            )
            .await
            .map_err(|e| match e {
                GatewayError::BadResponse(b) => GatewayError::SubmitRejected(b),
                other => other,
            })?;
        let _ = v;
        Ok(())
    }

    /// Convenience: submit then poll for the commit (40 attempts, 2 s apart).
    pub async fn submit_and_wait(&self, tx: &NotarizedTx) -> Result<TxStatus, GatewayError> {
        self.submit(tx).await?;
        self.wait_for_commit(&tx.txid, 40, Duration::from_secs(2))
            .await
    }

    // ----------------------------- local build ---------------------------

    /// Compiles a v1 transaction manifest from its text form.
    pub fn compile_manifest_v1(&self, text: &str) -> Result<TransactionManifestV1, GatewayError> {
        compile_manifest_v1(text, &self.network, BlobProvider::new())
            .map_err(|e| GatewayError::ManifestCompile(format!("{e:?}")))
    }

    /// Builds, signs and notarizes a manifest for an explicit epoch window. Pure:
    /// no network access. `epoch_window` is how many epochs the transaction stays
    /// valid (e.g. 10).
    pub fn notarize_at_epoch(
        &self,
        manifest: TransactionManifestV1,
        start_epoch: u64,
        epoch_window: u64,
        signers: &[&Ed25519PrivateKey],
        notary: &Ed25519PrivateKey,
        notary_is_signatory: bool,
    ) -> Result<NotarizedTx, GatewayError> {
        let header = TransactionHeaderV1 {
            network_id: self.network.id,
            start_epoch_inclusive: Epoch::of(start_epoch),
            end_epoch_exclusive: Epoch::of(start_epoch + epoch_window),
            nonce: rand_nonce(),
            notary_public_key: notary.public_key().into(),
            notary_is_signatory,
            tip_percentage: 0,
        };
        let mut builder = TransactionBuilder::new().header(header).manifest(manifest);
        for signer in signers {
            builder = builder.sign(*signer);
        }
        let notarized = builder.notarize(notary).build();
        let prepared = notarized
            .prepare(PreparationSettings::latest_ref())
            .map_err(|e| GatewayError::Encode(format!("{e:?}")))?;
        let txid = TransactionHashBech32Encoder::new(&self.network)
            .encode(&prepared.transaction_intent_hash())
            .map_err(|e| GatewayError::Encode(format!("{e:?}")))?;
        let raw = notarized
            .to_raw()
            .map_err(|e| GatewayError::Encode(format!("{e:?}")))?;
        Ok(NotarizedTx {
            txid,
            notarized_hex: hex::encode(raw.as_slice()),
        })
    }

    /// Builds, signs and notarizes a manifest using the current epoch (fetched from
    /// the Gateway). The transaction stays valid for 10 epochs.
    pub async fn build_notarized(
        &self,
        manifest: TransactionManifestV1,
        signers: &[&Ed25519PrivateKey],
        notary: &Ed25519PrivateKey,
        notary_is_signatory: bool,
    ) -> Result<NotarizedTx, GatewayError> {
        let epoch = self.current_epoch().await?;
        self.notarize_at_epoch(manifest, epoch, 10, signers, notary, notary_is_signatory)
    }

    /// Signs a SUBINTENT (V2 pre-authorization): compiles `subintent_manifest`, builds
    /// a partial transaction valid in `[start_epoch, end_epoch)` and signs it with
    /// `signer`. Returns the signed partial transaction as hex (NOT submitted). Pure:
    /// no network access.
    pub fn sign_subintent(
        &self,
        subintent_manifest: &str,
        start_epoch: u64,
        end_epoch: u64,
        signer: &Ed25519PrivateKey,
    ) -> Result<String, GatewayError> {
        let compiled = compile_manifest::<SubintentManifestV2>(
            subintent_manifest,
            &self.network,
            BlobProvider::new(),
        )
        .map_err(|e| GatewayError::ManifestCompile(format!("{e:?}")))?;
        let raw = PartialTransactionV2Builder::new()
            .intent_header(IntentHeaderV2 {
                network_id: self.network.id,
                start_epoch_inclusive: Epoch::of(start_epoch),
                end_epoch_exclusive: Epoch::of(end_epoch),
                min_proposer_timestamp_inclusive: None,
                max_proposer_timestamp_exclusive: None,
                intent_discriminator: rand_nonce() as u64,
            })
            .manifest(compiled)
            .sign(signer)
            .build_minimal()
            .to_raw()
            .map_err(|e| GatewayError::Encode(format!("{e:?}")))?;
        Ok(hex::encode(raw.as_slice()))
    }
}

fn rand_nonce() -> u32 {
    let mut b = [0u8; 4];
    OsRng.fill_bytes(&mut b);
    u32::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_status_parsing_and_flags() {
        assert!(TxStatus::parse("CommittedSuccess").is_success());
        assert!(TxStatus::parse("CommittedSuccess").is_final());
        assert!(TxStatus::parse("Rejected").is_final());
        assert!(!TxStatus::parse("Pending").is_final());
        assert!(!TxStatus::parse("Whatever").is_success());
    }

    #[test]
    fn notarize_offline_produces_txid_and_hex() {
        // Fully offline: build + sign + notarize an empty manifest with a local key.
        let gw = Gateway::stokenet();
        let key = Ed25519PrivateKey::from_bytes(&[1u8; 32]).unwrap();
        let manifest = ManifestBuilder::new().build();
        let tx = gw
            .notarize_at_epoch(manifest, 100, 10, &[&key], &key, true)
            .expect("notarize");
        assert!(tx.txid.starts_with("txid_tdx_2_"), "txid was {}", tx.txid);
        assert!(!tx.notarized_hex.is_empty());
    }

    #[test]
    fn sign_subintent_offline_produces_hex() {
        // Fully offline: sign a minimal subintent with a local key.
        let gw = Gateway::stokenet();
        let key = Ed25519PrivateKey::from_bytes(&[3u8; 32]).unwrap();
        let hex = gw
            .sign_subintent("YIELD_TO_PARENT;", 100, 110, &key)
            .expect("sign subintent");
        assert!(!hex.is_empty());
    }
}
