//! One process per link on a machine: a second holder of the same link would take the wallet from
//! the first and lose what was in flight, so it waits for the link — and says who holds it when
//! it cannot wait any longer. Two connectors with one lock directory stand in for two processes
//! (the operating system's lock is per open file, not per process).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::{Duration, Instant};

use radixdlt_connect_iroh::mobile::{
    account_proof_request, linking, MobileError, MobileWalletConnector, Progress,
};
use support::*;

fn connector(relay: &LocalRelay, locks: &std::path::Path) -> MobileWalletConnector {
    MobileWalletConnector::new()
        .with_relay_url(&relay.url)
        .unwrap()
        .with_link_lock_dir(locks)
}

fn lock_dir() -> std::path::PathBuf {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    std::env::temp_dir().join(format!("radixdlt-connect-locks-{}", hex::encode(bytes)))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_holder_of_a_link_waits_for_it_and_says_who_holds_it() {
    let relay = LocalRelay::start().await;
    let locks = lock_dir();
    let first = connector(&relay, &locks);
    let second = connector(&relay, &locks);
    let password = linking::generate_password().unwrap().to_vec();
    first.open_link(&password).await.unwrap();

    let started = Instant::now();
    let mut steps = Vec::new();
    let error = second
        .exchange(
            &password,
            &account_proof_request(&challenge(), &ctx(), false),
            Duration::from_secs(2),
            &mut |step| steps.push(step),
        )
        .await
        .unwrap_err();
    // Said as it happens, so a program can show it: who it waits for.
    assert!(
        steps.iter().any(|step| matches!(step, Progress::WaitingForLink { holder } if holder.contains(&std::process::id().to_string()))),
        "{steps:?}"
    );
    match &error {
        MobileError::LinkBusy { holder } => {
            assert!(holder.contains(&std::process::id().to_string()), "{holder}");
        }
        other => panic!("expected LinkBusy, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );

    // Let go, and the other one has it at once.
    first.close_link(&password).await;
    let started = Instant::now();
    second.open_link(&password).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    second.close().await;
    let _ = std::fs::remove_dir_all(&locks);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_keeps_one_process_per_link_itself_can_do_without() {
    let relay = LocalRelay::start().await;
    let first = MobileWalletConnector::new()
        .with_relay_url(&relay.url)
        .unwrap()
        .without_link_locks();
    let second = MobileWalletConnector::new()
        .with_relay_url(&relay.url)
        .unwrap()
        .without_link_locks();
    let password = linking::generate_password().unwrap().to_vec();
    first.open_link(&password).await.unwrap();
    second.open_link(&password).await.unwrap();
    first.close().await;
    second.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_let_go_of_while_waiting_is_taken_and_said() {
    let relay = LocalRelay::start().await;
    let locks = lock_dir();
    let first = connector(&relay, &locks);
    let second = connector(&relay, &locks);
    let password = linking::generate_password().unwrap().to_vec();
    first.open_link(&password).await.unwrap();

    let letting_go = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        first.close_link(&password).await;
    };
    let mut steps = Vec::new();
    let request = account_proof_request(&challenge(), &ctx(), false);
    let mut record = |step| steps.push(step);
    let asking = second.exchange(&password, &request, Duration::from_secs(3), &mut record);
    let (asked, ()) = tokio::join!(asking, letting_go);
    // Taken once let go of; then there is no wallet to answer, which is another story.
    assert!(
        matches!(asked, Err(MobileError::WalletUnreachable { .. })),
        "{asked:?}"
    );
    let taken = steps
        .iter()
        .find_map(|step| match step {
            Progress::LinkTaken { waited } => Some(*waited),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{steps:?}"));
    assert!(taken >= Duration::from_millis(300), "{taken:?}");
    second.close().await;
    let _ = std::fs::remove_dir_all(&locks);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_left_locked_by_a_process_that_died_is_taken_over() {
    let relay = LocalRelay::start().await;
    let locks = lock_dir();
    let password = linking::generate_password().unwrap().to_vec();
    // What a holder that crashed leaves: its record (the operating system freed the lock itself).
    std::fs::create_dir_all(&locks).unwrap();
    let id = hex::encode(&linking::connection_id(&password)[..16]);
    let file = locks.join(format!("{id}.lock"));
    std::fs::write(&file, "process 999999 since 1700000000").unwrap();

    let started = Instant::now();
    let taking = connector(&relay, &locks);
    taking.open_link(&password).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let record = std::fs::read_to_string(&file).unwrap();
    assert!(
        record.starts_with(&format!("process {} since ", std::process::id())),
        "{record}"
    );
    taking.close().await;
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "",
        "let go of cleanly: no record left"
    );
    let _ = std::fs::remove_dir_all(&locks);
}
