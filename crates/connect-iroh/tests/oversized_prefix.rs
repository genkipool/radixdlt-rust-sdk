//! A peer announcing a 4 GiB message is refused from the length prefix alone: nothing of that
//! size is ever allocated (on a 32-bit target, even asking for it used to panic), and the refusal
//! is immediate instead of waiting for gigabytes that will never come.
//!
//! In a binary of its own: it watches EVERY allocation of the process for the largest one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use iroh::endpoint::{presets, VarInt, WriteError};
use iroh::{Endpoint, RelayMode};
use radixdlt_connect_iroh::{IrohConnector, IrohError, ALPN, DEFAULT_MAX_MESSAGE_SIZE};
use tokio::time::timeout;

/// The system allocator, recording the largest single allocation asked of it.
struct Largest;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded unchanged to `System`; only a size is recorded on the side.
unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Largest = Largest;

/// Stream error code of a message over the receiver's limit (the wire constant).
const STREAM_TOO_LARGE: u32 = 1;

#[tokio::test]
async fn a_four_gib_length_prefix_is_refused_without_allocating_it() {
    let server = IrohConnector::bind().await.unwrap();
    let raw = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    let target = server.local_addr();

    let peer = async {
        let conn = raw.connect(target, ALPN).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        send.write_all(b"{\"a\":1").await.unwrap();
        // The receiver's answer: refused as too large (status 0x01), or the stream stopped.
        let answer = timeout(Duration::from_secs(10), recv.read_to_end(8))
            .await
            .unwrap();
        (conn, send, answer)
    };
    let receiver = async {
        let mut channel = timeout(Duration::from_secs(10), server.accept())
            .await
            .unwrap()
            .unwrap();
        LARGEST.store(0, Ordering::Relaxed);
        let started = Instant::now();
        let got = timeout(Duration::from_secs(10), channel.recv_message()).await;
        let largest = LARGEST.load(Ordering::Relaxed);
        (got, started.elapsed(), largest, channel)
    };
    let ((_conn, mut send, answer), (got, took, largest, _channel)) = tokio::join!(peer, receiver);

    let got = got.expect("refused at once, not waiting for 4 GiB");
    assert_eq!(
        got,
        Err(IrohError::TooLarge {
            size: u64::from(u32::MAX),
            max: DEFAULT_MAX_MESSAGE_SIZE
        })
    );
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert!(
        largest < 1024 * 1024,
        "the largest allocation while refusing it was {largest} bytes"
    );
    assert_eq!(answer.unwrap(), [0x01], "the peer is told why");
    // And the rest of its message is not wanted.
    let more = send.write_all(&[b' '; 64 * 1024]).await;
    let stopped = match more {
        Err(WriteError::Stopped(code)) => code == VarInt::from_u32(STREAM_TOO_LARGE),
        // Small enough to be buffered before the stop arrives: the stop is checked on the next write.
        Ok(()) => matches!(
            timeout(Duration::from_secs(5), send.stopped()).await,
            Ok(Ok(Some(code))) if code == VarInt::from_u32(STREAM_TOO_LARGE)
        ),
        Err(_) => false,
    };
    assert!(stopped, "the sender is asked to stop sending the rest");
}
