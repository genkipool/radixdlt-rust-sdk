// Examples are demonstrations: `expect` is the idiomatic way to keep them short and to
// show the failure loudly. Library code keeps the deny.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Runnable "log in with Radix over iroh" demo: a pure-Rust wallet and a dApp,
//! paired by ticket, complete a ROLA login — no mobile phone, no WebRTC.
//!
//!   cargo run --example login

use radixdlt_connect_iroh::protocol::{
    request_account_proof, AccountProofRequest, Approver, DappContext, Requester, Wallet,
};
use radixdlt_connect_iroh::{EndpointId, IrohConnector};
use radixdlt_keystore::KeyFile;

/// What the signer agrees to: logins to ONE dApp and origin, from the ONE peer it paired with.
/// Transactions and pre-authorizations are not implemented here, so they are refused.
struct PairedLogins {
    peer: EndpointId,
    dapp_definition: String,
    origin: String,
}

impl Approver for PairedLogins {
    async fn approve_account_proof(&self, from: &Requester, request: &AccountProofRequest) -> bool {
        let ok = from.peer == self.peer
            && request.dapp_definition == self.dapp_definition
            && request.origin == self.origin;
        println!(
            "login to {} ({}) from {}: {}",
            request.origin,
            request.dapp_definition,
            from.peer.fmt_short(),
            if ok { "approved" } else { "refused" }
        );
        ok
    }
}

#[tokio::main]
async fn main() {
    let network_id = 2u8;
    let dapp_def = "account_tdx_2_129grv2vv4q3w7aqzzwesc5k0xp4lg5dj4p78q80ca79rj5rct8mujk";
    let origin = "iroh://radixdlt-connect-iroh";

    // The pure-Rust wallet holds a fresh key.
    let key = KeyFile::generate(network_id, "demo").expect("keygen");
    let wallet = Wallet::from_key_file(&key, "demo").expect("wallet");
    println!("wallet account: {}", wallet.address());

    let signer = IrohConnector::bind().await.expect("bind signer");
    let dapp = IrohConnector::bind().await.expect("bind dapp");
    let ticket = signer.ticket();
    println!("pairing ticket ({} chars) shared with the dApp", ticket.len());

    let ctx = DappContext::new(network_id, dapp_def, origin);
    let challenge = "ab".repeat(32);

    // In a real signer the person approves (or a policy decides); here, a fixed policy.
    let approver = PairedLogins {
        peer: dapp.endpoint_id(),
        dapp_definition: dapp_def.to_string(),
        origin: origin.to_string(),
    };
    let signer_fut = async {
        let mut ch = signer.accept().await.expect("accept");
        wallet.answer(&mut ch, &approver).await.expect("answer");
        ch.close().await;
    };
    let dapp_fut = async {
        let mut ch = dapp.connect_to_ticket(&ticket).await.expect("connect");
        let proof = request_account_proof(&mut ch, &challenge, &ctx)
            .await
            .expect("login");
        ch.close().await;
        proof
    };

    let (_, proof) = tokio::join!(signer_fut, dapp_fut);
    println!("✓ ROLA login verified over iroh for account {}", proof.address);
    signer.close().await;
    dapp.close().await;
}
