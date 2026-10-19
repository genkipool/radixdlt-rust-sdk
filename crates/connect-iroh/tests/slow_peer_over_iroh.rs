//! A server that loops on `accept_pending` serves the next peer while an earlier one is still
//! stuck in its handshake: one peer that connects and never opens its stream must not keep the
//! others at the door.

use radixdlt_connect_iroh::IrohConnector;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn a_stalled_peer_does_not_hold_the_next_one() {
    let server = IrohConnector::bind().await.expect("bind server");
    let ticket = server.ticket();

    // Connects and says nothing: the server never gets a stream from it.
    let stalled = IrohConnector::bind().await.expect("bind stalled");
    let quiet_ticket = ticket.clone();
    let quiet = tokio::spawn(async move {
        let channel = stalled.connect_to_ticket(&quiet_ticket).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(channel);
    });
    // Talks.
    let good = IrohConnector::bind().await.expect("bind good");
    let talk = tokio::spawn(async move {
        let mut channel = good.connect_to_ticket(&ticket).await.expect("good connects");
        channel
            .send_message(&json!({ "hello": 1 }))
            .await
            .expect("good sends");
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(channel);
    });

    // Both arrive at once — neither waits for the other's handshake — and each is finished in a
    // task of its own: the one that talks is served while the silent one is still waited on.
    let (tx, mut served) = tokio::sync::mpsc::channel(2);
    for _ in 0..2 {
        let pending = tokio::time::timeout(Duration::from_secs(10), server.accept_pending())
            .await
            .expect("a peer arrives")
            .expect("pending");
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Ok(mut channel) = pending.ready().await {
                let _ = tx.send(channel.recv_message().await.ok()).await;
            }
        });
    }
    let first = tokio::time::timeout(Duration::from_secs(10), served.recv())
        .await
        .expect("the talking peer is served while the silent one still waits")
        .flatten()
        .expect("its message");
    assert_eq!(first["hello"], 1);
    quiet.abort();
    talk.abort();
}
