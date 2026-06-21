//! Runnable "log in with Radix over iroh" demo: a pure-Rust wallet and a dApp,
//! paired by ticket, complete a ROLA login — no mobile phone, no WebRTC.
//!
//!   cargo run --example login

use radixdlt_connect_iroh::protocol::{request_account_proof, DappContext, Wallet};
use radixdlt_connect_iroh::IrohConnector;
use radixdlt_keystore::KeyFile;

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
    println!(
        "pairing ticket ({} chars) shared with the dApp",
        ticket.len()
    );

    let ctx = DappContext::new(network_id, dapp_def, origin);
    let challenge = "ab".repeat(32);

    let signer_fut = async {
        let mut ch = signer.accept().await.expect("accept");
        wallet.answer(&mut ch).await.expect("answer");
        ch.wait_closed().await;
    };
    let dapp_fut = async {
        let mut ch = dapp.connect_to_ticket(&ticket).await.expect("connect");
        let proof = request_account_proof(&mut ch, &challenge, &ctx)
            .await
            .expect("login");
        ch.close();
        proof
    };

    let (_, proof) = tokio::join!(signer_fut, dapp_fut);
    println!(
        "✓ ROLA login verified over iroh for account {}",
        proof.address
    );
}
