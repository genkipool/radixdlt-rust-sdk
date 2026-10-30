//! High-level "log in with Radix" over iroh, using the `protocol` API and ticket
//! pairing — the flow an app like PamAuthority would use with a pure-Rust signer.

use radixdlt_connect_iroh::protocol::{
    request_account_proof, AccountProofRequest, Approver, DappContext, DenyAll, Requester, Wallet,
};
use radixdlt_connect_iroh::{EndpointId, IrohConnector, IrohError};
use radixdlt_keystore::KeyFile;

const DAPP: &str = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
const ORIGIN: &str = "iroh://radixdlt-connect-iroh";

/// Signs logins to this one dApp, from this one peer — what a signer paired by ticket approves.
struct LoginsFrom(EndpointId);

impl Approver for LoginsFrom {
    async fn approve_account_proof(&self, from: &Requester, request: &AccountProofRequest) -> bool {
        from.peer == self.0 && request.dapp_definition == DAPP && request.origin == ORIGIN
    }
}

#[tokio::test]
async fn login_with_ticket_pairing() {
    const NETWORK_ID: u8 = 2;

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

    let approver = LoginsFrom(dapp.endpoint_id());
    let signer_fut = async {
        let mut ch = signer.accept().await.unwrap();
        wallet.answer(&mut ch, &approver).await.unwrap();
        ch.close().await;
    };
    let dapp_fut = async {
        let mut ch = dapp.connect_to_ticket(&ticket).await.unwrap();
        let proof = request_account_proof(&mut ch, &challenge, &ctx).await.unwrap();
        ch.close().await;
        proof
    };

    let (_, proof) = tokio::join!(signer_fut, dapp_fut);

    // request_account_proof already verified the proof; check it is the signer's.
    assert_eq!(proof.address, expected_address);
}

#[tokio::test]
async fn a_signer_without_approval_signs_nothing() {
    let key = KeyFile::generate(2, "pw").unwrap();
    let wallet = Wallet::from_key_file(&key, "pw").unwrap();
    let signer = IrohConnector::bind().await.unwrap();
    let dapp = IrohConnector::bind().await.unwrap();
    let ticket = signer.ticket();
    let ctx = DappContext::new(2, DAPP, ORIGIN);

    let (_, refused) = tokio::join!(
        async {
            let mut ch = signer.accept().await.unwrap();
            wallet.answer(&mut ch, &DenyAll).await.unwrap();
        },
        async {
            let mut ch = dapp.connect_to_ticket(&ticket).await.unwrap();
            request_account_proof(&mut ch, &"cd".repeat(32), &ctx).await
        }
    );
    match refused {
        Err(IrohError::Rejected(reason)) => assert!(reason.starts_with("rejectedByUser"), "{reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}
