//! The SDK-to-SDK channel under the conditions that used to break it: messages over the size
//! limit, a peer that trickles a message forever, operations cancelled half way, peers that are
//! not admitted, and endpoints that have to close in bounded time.
//!
//! Some peers here are RAW iroh endpoints speaking the wire format by hand, so the tests can send
//! what a well-behaved `IrohChannel` never would.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use iroh::endpoint::{presets, Connection, ReadError, ReadToEndError, RecvStream, SendStream, VarInt};
use iroh::{Endpoint, EndpointAddr, RelayMode, TransportAddr};
use radixdlt_connect_iroh::{IrohChannel, IrohConnector, IrohError, Limits, ALPN};
use serde_json::{json, Value};
use tokio::time::timeout;

/// Stream error code of a message abandoned before it was complete (the wire constant).
const STREAM_ABANDONED: u32 = 2;

async fn raw_endpoint(alpns: Vec<Vec<u8>>) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(alpns)
        .bind()
        .await
        .unwrap()
}

/// A raw endpoint's address on this host.
fn loopback(endpoint: &Endpoint) -> EndpointAddr {
    let addrs = endpoint.bound_sockets().into_iter().map(|s| {
        let ip = if s.ip().is_unspecified() && s.is_ipv4() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            s.ip()
        };
        TransportAddr::Ip(SocketAddr::new(ip, s.port()))
    });
    EndpointAddr::from_parts(
        endpoint.id(),
        addrs.filter(|a| matches!(a, TransportAddr::Ip(s) if s.is_ipv4())),
    )
}

/// A raw client connected to `server`, and the server's channel. The server accepts while the
/// client connects (iroh runs a handshake only for a connection being accepted), and its `accept`
/// returns once the client's first bytes are in.
async fn raw_client_to(
    server: &IrohConnector,
    first_bytes: &[u8],
) -> (Endpoint, Connection, SendStream, RecvStream, IrohChannel) {
    let raw = raw_endpoint(Vec::new()).await;
    let client = async {
        let conn = raw.connect(server.local_addr(), ALPN).await.unwrap();
        let (mut send, recv) = conn.open_bi().await.unwrap();
        send.write_all(first_bytes).await.unwrap();
        (conn, send, recv)
    };
    let ((conn, send, recv), channel) = tokio::join!(client, async {
        timeout(Duration::from_secs(10), server.accept())
            .await
            .unwrap()
            .unwrap()
    });
    (raw, conn, send, recv, channel)
}

fn frame(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap();
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// What a raw sender reads back on a message stream: the receiver's status byte, or how the
/// stream ended.
async fn status(mut recv: RecvStream) -> Result<Vec<u8>, ReadToEndError> {
    timeout(Duration::from_secs(10), recv.read_to_end(8))
        .await
        .unwrap()
}

/// `from` sends `message` while `to` receives it.
async fn exchange(
    from: &mut IrohChannel,
    to: &mut IrohChannel,
    message: Value,
) -> (Result<(), IrohError>, Result<Value, IrohError>) {
    tokio::join!(from.send_message(&message), to.recv_message())
}

/// Two SDK peers: `(server, client, server's channel, client's channel)`. The client sends a
/// hello first so the server's `accept` returns.
async fn pair(
    server_limits: Limits,
    client_limits: Limits,
) -> (IrohConnector, IrohConnector, IrohChannel, IrohChannel) {
    let server = IrohConnector::builder()
        .limits(server_limits)
        .bind()
        .await
        .unwrap();
    let client = IrohConnector::builder()
        .limits(client_limits)
        .bind()
        .await
        .unwrap();
    let ticket = server.ticket();
    let (accepted, connected) = tokio::join!(
        async {
            let mut ch = server.accept().await.unwrap();
            assert_eq!(ch.recv_message().await.unwrap(), json!({ "hello": 1 }));
            ch
        },
        async {
            let mut ch = client.connect_to_ticket(&ticket).await.unwrap();
            ch.send_message(&json!({ "hello": 1 })).await.unwrap();
            ch
        }
    );
    (server, client, accepted, connected)
}

#[tokio::test]
async fn each_end_knows_who_the_other_is_and_sends_are_confirmed() {
    let (server, client, mut at_server, mut at_client) = pair(Limits::default(), Limits::default()).await;
    assert_eq!(at_server.remote_id(), client.endpoint_id());
    assert_eq!(at_client.remote_id(), server.endpoint_id());

    for n in 0..20 {
        let (sent, got) = exchange(&mut at_server, &mut at_client, json!({ "n": n })).await;
        sent.unwrap();
        assert_eq!(got.unwrap()["n"], n);
        let (sent, got) = exchange(&mut at_client, &mut at_server, json!({ "back": n })).await;
        sent.unwrap();
        assert_eq!(got.unwrap()["back"], n);
    }
    at_client.close().await;
    server.close().await;
    client.close().await;
}

#[tokio::test]
async fn a_message_over_the_peers_limit_is_refused_and_the_channel_goes_on() {
    let small = Limits::default().with_max_message_size(1024);
    let (_server, _client, mut at_server, mut at_client) = pair(small, Limits::default()).await;

    let big = json!({ "pad": "x".repeat(2048) });
    let (sent, got) = tokio::join!(at_client.send_message(&big), at_server.recv_message());
    assert_eq!(sent, Err(IrohError::Refused { too_large: true }));
    match got {
        Err(IrohError::TooLarge { size, max: 1024 }) => assert!(size > 2048, "{size}"),
        other => panic!("expected TooLarge, got {other:?}"),
    }

    // Same channel, next message.
    let (sent, got) = exchange(&mut at_client, &mut at_server, json!({ "ok": true })).await;
    sent.unwrap();
    assert_eq!(got.unwrap(), json!({ "ok": true }));
}

#[tokio::test]
async fn a_message_over_our_own_limit_is_not_sent_at_all() {
    let small = Limits::default().with_max_message_size(100);
    let (_server, _client, mut at_server, mut at_client) = pair(Limits::default(), small).await;

    let refused = at_client.send_message(&json!({ "pad": "x".repeat(200) })).await;
    assert!(
        matches!(refused, Err(IrohError::TooLarge { max: 100, .. })),
        "{refused:?}"
    );

    let (sent, got) = exchange(&mut at_client, &mut at_server, json!({ "next": 1 })).await;
    sent.unwrap();
    assert_eq!(
        got.unwrap(),
        json!({ "next": 1 }),
        "the refused message never reached the peer"
    );
}

#[tokio::test]
async fn a_trickled_message_times_out_but_waiting_for_the_next_one_does_not() {
    let limits = Limits::default().with_message_timeout(Duration::from_millis(500));
    let server = IrohConnector::builder().limits(limits).bind().await.unwrap();

    // Announces 100 bytes and then sends one every 100 ms: it would take 10 s to finish.
    let (_raw, conn, mut send, recv, mut channel) = raw_client_to(&server, &100u32.to_be_bytes()).await;
    let trickle = tokio::spawn(async move {
        for _ in 0..100 {
            if send.write_all(b" ").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let started = Instant::now();
    let got = channel.recv_message().await;
    assert_eq!(
        got,
        Err(IrohError::Timeout {
            sending: false,
            after: Duration::from_millis(500)
        })
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    // The trickler is told: its stream is reset, not confirmed.
    match status(recv).await {
        Err(ReadToEndError::Read(ReadError::Reset(code))) => {
            assert_eq!(code, VarInt::from_u32(STREAM_ABANDONED))
        }
        other => panic!("expected a reset, got {other:?}"),
    }
    trickle.abort();

    // Idle: no message has started, so waiting for one is not bounded by the 500 ms.
    assert!(
        timeout(Duration::from_millis(1500), channel.recv_message())
            .await
            .is_err(),
        "waiting for the next message must not time out by itself"
    );

    // And the channel still works.
    let (mut send, recv) = conn.open_bi().await.unwrap();
    send.write_all(&frame(&json!({ "after": "trickle" })))
        .await
        .unwrap();
    send.finish().unwrap();
    assert_eq!(
        channel.recv_message().await.unwrap(),
        json!({ "after": "trickle" })
    );
    assert_eq!(status(recv).await.unwrap(), [0x00]);
}

#[tokio::test]
async fn a_cancelled_receive_resumes_where_it_stopped() {
    let server = IrohConnector::bind().await.unwrap();
    let message = json!({ "whole": "message", "pad": "y".repeat(5000) });
    let bytes = frame(&message);
    let (first, rest) = bytes.split_at(2); // half a length prefix
    let (middle, last) = rest.split_at(rest.len() / 2);

    let (_raw, _conn, mut send, recv, mut channel) = raw_client_to(&server, first).await;
    // Cancelled with half a prefix in.
    assert!(timeout(Duration::from_millis(200), channel.recv_message())
        .await
        .is_err());
    send.write_all(middle).await.unwrap();
    // Cancelled again, now in the middle of the body.
    assert!(timeout(Duration::from_millis(200), channel.recv_message())
        .await
        .is_err());
    send.write_all(last).await.unwrap();
    send.finish().unwrap();

    assert_eq!(channel.recv_message().await.unwrap(), message);
    assert_eq!(
        status(recv).await.unwrap(),
        [0x00],
        "and it was confirmed once, whole"
    );
}

#[tokio::test]
async fn a_cancelled_send_is_abandoned_whole_and_the_next_one_is_intact() {
    // A raw receiver that does not read: the send stalls on flow control, half written.
    let raw = raw_endpoint(vec![ALPN.to_vec()]).await;
    let client = IrohConnector::bind().await.unwrap();
    let (accepted, connected) = tokio::join!(
        async { raw.accept().await.unwrap().await.unwrap() },
        client.connect(loopback(&raw))
    );
    let conn = accepted;
    let mut channel = connected.unwrap();

    let big = json!({ "pad": "z".repeat(4 * 1024 * 1024) });
    assert!(
        timeout(Duration::from_millis(300), channel.send_message(&big))
            .await
            .is_err(),
        "the send should still be writing when it is cancelled"
    );

    // What the receiver finds on that stream: part of a message, then a reset — never a FINISHED
    // stream it could take for a whole message.
    let (_answer, mut recv) = conn.accept_bi().await.unwrap();
    match timeout(Duration::from_secs(10), recv.read_to_end(8 * 1024 * 1024))
        .await
        .unwrap()
    {
        Err(ReadToEndError::Read(ReadError::Reset(code))) => {
            assert_eq!(code, VarInt::from_u32(STREAM_ABANDONED))
        }
        other => panic!(
            "expected the abandoned stream to be reset, got {:?}",
            other.map(|b| b.len())
        ),
    }

    // The next message starts on a stream of its own, with its own length prefix.
    let next = json!({ "next": "intact" });
    let (sent, read) = tokio::join!(channel.send_message(&next), async {
        let (mut answer, mut recv) = conn.accept_bi().await.unwrap();
        let bytes = recv.read_to_end(1024).await.unwrap();
        answer.write_all(&[0x00]).await.unwrap();
        answer.finish().unwrap();
        // Kept until the client has read the answer.
        tokio::time::sleep(Duration::from_millis(200)).await;
        bytes
    });
    sent.unwrap();
    assert_eq!(read, frame(&next));
}

#[tokio::test]
async fn between_two_sdk_peers_a_cancelled_send_is_skipped_by_the_receiver() {
    let (_server, _client, mut at_server, mut at_client) = pair(Limits::default(), Limits::default()).await;
    let big = json!({ "pad": "z".repeat(4 * 1024 * 1024) });
    // The server is not reading yet: the big send stalls and is cancelled.
    assert!(timeout(Duration::from_millis(300), at_client.send_message(&big))
        .await
        .is_err());
    let (sent, got) = exchange(&mut at_client, &mut at_server, json!({ "n": 2 })).await;
    sent.unwrap();
    assert_eq!(got.unwrap(), json!({ "n": 2 }));
}

#[tokio::test]
async fn an_unconfirmed_send_times_out_and_says_the_peer_may_still_take_it() {
    let quick = Limits::default().with_message_timeout(Duration::from_millis(300));
    let (_server, _client, mut at_server, mut at_client) = pair(Limits::default(), quick).await;
    // The server does not read for now.
    let sent = at_client.send_message(&json!({ "late": true })).await;
    assert_eq!(
        sent,
        Err(IrohError::Timeout {
            sending: true,
            after: Duration::from_millis(300)
        })
    );
    // It was written whole, so it is still there to be taken.
    assert_eq!(at_server.recv_message().await.unwrap(), json!({ "late": true }));
}

#[tokio::test]
async fn bytes_that_are_not_json_are_refused_and_the_channel_goes_on() {
    let server = IrohConnector::bind().await.unwrap();
    let mut garbage = 5u32.to_be_bytes().to_vec();
    garbage.extend_from_slice(b"{nope");
    let (_raw, conn, mut send, recv, mut channel) = raw_client_to(&server, &garbage).await;
    send.finish().unwrap();
    assert!(matches!(
        channel.recv_message().await,
        Err(IrohError::Protocol(_))
    ));
    assert_eq!(status(recv).await.unwrap(), [0x02]);

    // Longer than announced.
    let (mut send, recv) = conn.open_bi().await.unwrap();
    let mut long = 2u32.to_be_bytes().to_vec();
    long.extend_from_slice(b"{}{}");
    send.write_all(&long).await.unwrap();
    send.finish().unwrap();
    assert!(matches!(
        channel.recv_message().await,
        Err(IrohError::Protocol(_))
    ));
    assert_eq!(status(recv).await.unwrap(), [0x02]);

    let (mut send, recv) = conn.open_bi().await.unwrap();
    send.write_all(&frame(&json!([1, 2, 3]))).await.unwrap();
    send.finish().unwrap();
    assert_eq!(channel.recv_message().await.unwrap(), json!([1, 2, 3]));
    assert_eq!(status(recv).await.unwrap(), [0x00]);
}

/// A stranger dials `server` while it accepts: the stranger gets nowhere, and the server's
/// `ready` fails instead of handing it a channel.
async fn stranger_is_refused(server: &IrohConnector, stranger: &IrohConnector) {
    let ticket = server.ticket();
    let (at_server, at_stranger) = tokio::join!(
        async { server.accept_pending().await.unwrap().ready().await },
        async {
            let mut ch = stranger.connect_to_ticket(&ticket).await?;
            ch.send_message(&json!({ "who": "stranger" })).await
        }
    );
    assert!(matches!(at_server, Err(IrohError::Accept(_))), "{at_server:?}");
    assert!(at_stranger.is_err());
}

/// `friend` dials `server` and is served: the server's channel names it.
async fn friend_is_served(server: &IrohConnector, friend: &IrohConnector) {
    let ticket = server.ticket();
    let (accepted, sent) = tokio::join!(
        async {
            let mut ch = server.accept().await.unwrap();
            (ch.remote_id(), ch.recv_message().await.unwrap())
        },
        async {
            let mut ch = friend.connect_to_ticket(&ticket).await.unwrap();
            ch.send_message(&json!({ "who": "friend" })).await
        }
    );
    sent.unwrap();
    assert_eq!(accepted, (friend.endpoint_id(), json!({ "who": "friend" })));
}

#[tokio::test]
async fn only_admitted_peers_get_in() {
    let friend = IrohConnector::bind().await.unwrap();
    let stranger = IrohConnector::bind().await.unwrap();
    let server = IrohConnector::builder()
        .allow_only([friend.endpoint_id()])
        .bind()
        .await
        .unwrap();
    timeout(Duration::from_secs(20), stranger_is_refused(&server, &stranger))
        .await
        .unwrap();
    timeout(Duration::from_secs(20), friend_is_served(&server, &friend))
        .await
        .unwrap();
}

#[tokio::test]
async fn admission_can_be_decided_per_connection() {
    let friend = IrohConnector::bind().await.unwrap();
    let stranger = IrohConnector::bind().await.unwrap();
    let wanted = friend.endpoint_id();
    let server = IrohConnector::builder()
        .admit(move |peer| *peer == wanted)
        .bind()
        .await
        .unwrap();
    timeout(Duration::from_secs(20), stranger_is_refused(&server, &stranger))
        .await
        .unwrap();
    timeout(Duration::from_secs(20), friend_is_served(&server, &friend))
        .await
        .unwrap();
}

#[tokio::test]
async fn anyone_is_admitted_by_default() {
    let server = IrohConnector::bind().await.unwrap();
    let anyone = IrohConnector::bind().await.unwrap();
    timeout(Duration::from_secs(20), friend_is_served(&server, &anyone))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_peer_on_the_old_protocol_version_cannot_connect() {
    let server = IrohConnector::bind().await.unwrap();
    let old = raw_endpoint(Vec::new()).await;
    let (at_server, attempt) = tokio::join!(
        timeout(Duration::from_secs(10), async {
            server.accept_pending().await.unwrap().ready().await
        }),
        timeout(
            Duration::from_secs(10),
            old.connect(server.local_addr(), b"radixdlt-connect-iroh/0")
        )
    );
    assert!(attempt.unwrap().is_err(), "an old peer must not get a connection");
    assert!(!matches!(at_server, Ok(Ok(_))), "nor the server a channel");
}

#[tokio::test]
async fn closing_is_bounded() {
    let limits = Limits::default().with_close_timeout(Duration::from_millis(400));
    let (server, _client, at_server, mut at_client) = pair(limits, limits).await;

    // The peer never closes: waiting for it gives up after the bound.
    let waited = timeout(Duration::from_secs(2), at_server.wait_closed()).await;
    assert_eq!(waited, Ok(false), "wait_closed must give up by itself");

    // Closing the endpoint closes its channels, tells the peer, and stops accepting.
    let started = Instant::now();
    assert!(timeout(Duration::from_secs(3), server.close()).await.is_ok());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(matches!(server.accept_pending().await, Err(IrohError::Accept(_))));
    let got = timeout(Duration::from_secs(10), at_client.recv_message())
        .await
        .unwrap();
    assert!(matches!(got, Err(IrohError::Stream(_))), "{got:?}");
    drop(at_server);
}

#[tokio::test]
async fn a_channel_closed_right_after_its_last_message_still_confirms_it() {
    let (_server, _client, mut at_server, mut at_client) = pair(Limits::default(), Limits::default()).await;
    let last = json!({ "last": true });
    let (sent, closed) = tokio::join!(at_server.send_message(&last), async {
        let last = at_client.recv_message().await.unwrap();
        at_client.close().await;
        last
    });
    assert_eq!(closed, json!({ "last": true }));
    assert_eq!(sent, Ok(()), "the sender learns its last message was taken");
}

#[tokio::test]
async fn the_ticket_carries_this_hosts_lan_addresses_and_loopback() {
    let server = IrohConnector::bind().await.unwrap();
    let ticket: EndpointAddr = serde_json::from_slice(&hex::decode(server.ticket()).unwrap()).unwrap();
    assert_eq!(ticket.id, server.endpoint_id());
    let in_ticket: Vec<SocketAddr> = ticket.ip_addrs().copied().collect();
    // Same host.
    assert!(in_ticket.iter().any(|a| a.ip().is_loopback()), "{in_ticket:?}");
    // Every interface address iroh knows of (LAN), not loopback alone.
    for known in server.addr().ip_addrs() {
        assert!(in_ticket.contains(known), "{known} missing from {in_ticket:?}");
    }
}
