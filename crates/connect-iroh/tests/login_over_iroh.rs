//! High-level "log in with Radix" over iroh, using the `protocol` API and ticket
//! pairing — the flow an app like PamAuthority would use with a pure-Rust signer.

use radixdlt_connect_iroh::protocol::{request_account_proof, DappContext, Wallet};
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_keystore::KeyFile;

#[tokio::test]
async fn login_with_ticket_pairing() {
    const NETWORK_ID: u8 = 2;
    const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
    const ORIGIN: &str = "iroh://radixdlt-connect-iroh";

    // The signer (a pure-Rust "wallet") holds an encrypted key.
    let key = KeyFile::generate(NETWORK_ID, "pw").unwrap();
    let expected_address = key.address.clone();
    let wallet = Wallet::from_key_file(&key, "pw").unwrap();

    let signer = IrohConnector::bind().await.unwrap();
    let dapp = IrohConnector::bind().await.unwrap();

    // The signer shares a ticket (would be shown as a QR / pasted).
    let ticket = signer.ticket();

    let ctx = DappContext::new(NETWORK_ID, DAPP, ORIGIN);
    let challenge = "cd".repeat(32);

    let signer_fut = async {
        let mut ch = signer.accept().await.unwrap();
        wallet.answer(&mut ch).await.unwrap();
        ch.wait_closed().await;
    };
    let dapp_fut = async {
        let mut ch = dapp.connect_to_ticket(&ticket).await.unwrap();
        let proof = request_account_proof(&mut ch, &challenge, &ctx)
            .await
            .unwrap();
        ch.close();
        proof
    };

    let (_, proof) = tokio::join!(signer_fut, dapp_fut);

    // request_account_proof already verified the proof; check it is the signer's.
    assert_eq!(proof.address, expected_address);
}
