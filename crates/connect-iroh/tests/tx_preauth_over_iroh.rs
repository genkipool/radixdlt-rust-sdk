//! Transaction signing and pre-authorization (subintent) over iroh, between a
//! pure-Rust wallet and a dApp, using the shared Radix Connect schema.
//!
//! Both hit live Stokenet (the wallet fetches the epoch / submits), so they are
//! ignored by default. Run with:
//!   cargo test --test tx_preauth_over_iroh -- --ignored

use radixdlt_connect_iroh::protocol::{
    request_pre_authorization, request_transaction, DappContext, Wallet,
};
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_keystore::KeyFile;

const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
const ORIGIN: &str = "iroh://radixdlt-connect-iroh";

#[tokio::test]
#[ignore = "hits live Stokenet (epoch read)"]
async fn pre_authorization_over_iroh() {
    let key = KeyFile::generate(2, "pw").unwrap();
    let wallet = Wallet::from_key_file(&key, "pw").unwrap();

    let signer = IrohConnector::bind().await.unwrap();
    let dapp = IrohConnector::bind().await.unwrap();
    let ticket = signer.ticket();
    let ctx = DappContext::new(2, DAPP, ORIGIN);

    let signer_fut = async {
        let mut ch = signer.accept().await.unwrap();
        wallet.answer(&mut ch).await.unwrap();
        ch.wait_closed().await;
    };
    let dapp_fut = async {
        let mut ch = dapp.connect_to_ticket(&ticket).await.unwrap();
        let spt = request_pre_authorization(&mut ch, "YIELD_TO_PARENT;", 600, &ctx)
            .await
            .unwrap();
        ch.close();
        spt
    };

    let (_, spt) = tokio::join!(signer_fut, dapp_fut);
    assert!(hex::decode(&spt).is_ok(), "signed partial tx must be hex");
}

#[tokio::test]
#[ignore = "hits live Stokenet (submits a transaction)"]
async fn transaction_over_iroh() {
    let key = KeyFile::generate(2, "pw").unwrap();
    let wallet = Wallet::from_key_file(&key, "pw").unwrap();
    let account = wallet.address().to_string();
    let faucet = "component_tdx_2_1cptxxxxxxxxxfaucetxxxxxxxxx000527798379xxxxxxxxxyulkzl";
    let manifest = format!(
        "CALL_METHOD Address(\"{faucet}\") \"lock_fee\" Decimal(\"100\");\n\
         CALL_METHOD Address(\"{faucet}\") \"free\";\n\
         CALL_METHOD Address(\"{account}\") \"try_deposit_batch_or_abort\" Expression(\"ENTIRE_WORKTOP\") Enum<0u8>();"
    );

    let signer = IrohConnector::bind().await.unwrap();
    let dapp = IrohConnector::bind().await.unwrap();
    let ticket = signer.ticket();
    let ctx = DappContext::new(2, DAPP, ORIGIN);

    let signer_fut = async {
        let mut ch = signer.accept().await.unwrap();
        wallet.answer(&mut ch).await.unwrap();
        ch.wait_closed().await;
    };
    let dapp_fut = async {
        let mut ch = dapp.connect_to_ticket(&ticket).await.unwrap();
        let txid = request_transaction(&mut ch, &manifest, &ctx).await.unwrap();
        ch.close();
        txid
    };

    let (_, txid) = tokio::join!(signer_fut, dapp_fut);
    assert!(txid.starts_with("txid_tdx_2_"), "txid: {txid}");
}
