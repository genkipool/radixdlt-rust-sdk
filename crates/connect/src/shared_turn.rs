//! The link's turn and its settle time, shared between PROCESSES.
//!
//! [`Connector::take_turn`](crate::Connector) already queues requests on one link inside a
//! process. But several processes use the same paired link — two AI sessions, each with its own
//! connector, `radix-pam-connect` on a `sudo`, the `pamauthority` CLI — and the wallet keeps ONE
//! channel per link: a channel opened by one process tears down the channel another just opened,
//! and the request on it never shows on the phone. Observed on 2026-10-04: three transactions sent
//! while other connector processes shared the link, none reached the phone.
//!
//! So the turn is also an OS file lock, and the moment a channel closed is also written to disk:
//! `~/.config/radix-connect/links/<key>.lock` and `<key>.closed`. `<key>` is a domain-separated
//! BLAKE2b hash of the link password — the files hold no secret, and the name is the same for
//! every binary (unlike `DefaultHasher`, whose output may change between compiler versions).
//! Without a usable directory everything still works, coordinated within the process only.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blake2::{Blake2b512, Digest};
use fs4::fs_std::FileExt;

/// How often a waiting process tries the lock again.
const POLL: Duration = Duration::from_millis(150);

/// The shared directory, created owner-only; `None` when there is no home to put it in.
fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let dir = base.join("radix-connect").join("links");
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

/// The same name for a link in every process and every build, and no secret in it.
pub(crate) fn key(password: &[u8]) -> String {
    let mut hash = Blake2b512::new();
    hash.update(b"radix-connect/shared-turn\n");
    hash.update(password);
    hex::encode(&hash.finalize()[..16])
}

/// Holding this is holding the link for every process on this machine. Released on drop.
#[derive(Debug)]
pub(crate) struct SharedTurn(Option<File>);

impl Drop for SharedTurn {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = FileExt::unlock(file);
        }
    }
}

/// Waits for the link's lock until `deadline`. `Ok(None)`-like behaviour (an empty turn) when
/// there is no shared directory: the process-wide turn still applies.
///
/// # Errors
/// `Err(())` when another process held the link past `deadline`.
pub(crate) async fn take(password: &[u8], deadline: Instant) -> Result<SharedTurn, ()> {
    let Some(path) = dir().map(|d| d.join(format!("{}.lock", key(password)))) else {
        return Ok(SharedTurn(None));
    };
    let Ok(file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    else {
        return Ok(SharedTurn(None));
    };
    loop {
        if matches!(FileExt::try_lock_exclusive(&file), Ok(true)) {
            return Ok(SharedTurn(Some(file)));
        }
        if Instant::now() + POLL >= deadline {
            return Err(());
        }
        tokio::time::sleep(POLL).await;
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Records, for every process, that a channel on this link just closed.
pub(crate) fn note_closed(key: &str) {
    if let Some(dir) = dir() {
        let _ = std::fs::write(dir.join(format!("{key}.closed")), now_millis().to_string());
    }
}

/// How long ago any process last closed a channel on this link.
pub(crate) fn since_closed(password: &[u8]) -> Option<Duration> {
    let text = std::fs::read_to_string(dir()?.join(format!("{}.closed", key(password)))).ok()?;
    let at: u128 = text.trim().parse().ok()?;
    let ago = now_millis().saturating_sub(at);
    Some(Duration::from_millis(u64::try_from(ago).unwrap_or(u64::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_stable_and_reveals_nothing() {
        let a = key(b"secret-link-password");
        assert_eq!(a, key(b"secret-link-password"));
        assert_ne!(a, key(b"another-link"));
        assert_eq!(a.len(), 32);
        assert!(!a.contains("secret"));
    }

    #[tokio::test]
    async fn a_second_holder_waits_and_a_closed_channel_is_seen_by_all() {
        let home = std::env::temp_dir().join(format!("shared-turn-test-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &home);
        let password = b"turn-test";
        let first = take(password, Instant::now() + Duration::from_secs(1)).await;
        assert!(first.is_ok());
        // Same file, other handle: what another process would see while the first holds it.
        let second = take(password, Instant::now() + Duration::from_millis(400)).await;
        assert!(second.is_err(), "the link is held");
        drop(first);
        assert!(take(password, Instant::now() + Duration::from_secs(1))
            .await
            .is_ok());
        note_closed(&key(password));
        assert!(since_closed(password).is_some_and(|ago| ago < Duration::from_secs(5)));
        let _ = std::fs::remove_dir_all(&home);
    }
}
