//! One process per link on a machine.
//!
//! A link's session owns an endpoint key derived from the link's password. Two processes running
//! one for the same link (two logins at once, a helper and a resident daemon) compete for it at the
//! relay: each takes the wallet from the other, and an answer in flight — a signature just approved
//! — is lost with the connection it was on. So a session starts only once its process holds the
//! link's lock: an exclusive lock of the operating system on a file named after the link (a hash,
//! never the password), held for as long as the session runs and released when it closes — or by
//! the operating system when the process ends, however it ends. Another process waits for it,
//! within the time its call allows.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;

use super::link::LinkId;
use super::Progress;

/// How often a process waiting for a link looks again.
const POLL: Duration = Duration::from_millis(100);

/// Where the locks of this machine's links are, or none at all.
#[derive(Debug, Clone)]
pub(super) struct LinkLocks {
    dir: Option<PathBuf>,
}

impl Default for LinkLocks {
    /// The user's runtime directory (`$XDG_RUNTIME_DIR`), else their cache (`~/.cache`), else the
    /// system's temporary directory: what every process of this user on this machine sees alike.
    fn default() -> Self {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(std::env::temp_dir);
        LinkLocks {
            dir: Some(base.join("radixdlt-connect").join("links")),
        }
    }
}

impl LinkLocks {
    /// Locks in `dir`.
    pub(super) fn in_dir(dir: impl Into<PathBuf>) -> Self {
        LinkLocks {
            dir: Some(dir.into()),
        }
    }

    /// No locks: for a host that keeps one process per link itself.
    pub(super) fn none() -> Self {
        LinkLocks { dir: None }
    }

    fn path(&self, id: LinkId) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.lock", id.file_name())))
    }

    /// Takes the lock of link `id`, waiting while another process holds it (the caller bounds the
    /// wait). `None`: no locks here, or the directory cannot be used — the session then starts
    /// unlocked, as it did before there were locks, and the log says why.
    pub(super) async fn take(&self, id: LinkId, on: &mut (dyn FnMut(Progress) + Send)) -> Option<LinkLock> {
        let path = self.path(id)?;
        let file = match open(&path) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(link = %id, error = %error, "the link's lock cannot be used: starting without it");
                return None;
            }
        };
        let started = std::time::Instant::now();
        let mut waiting = false;
        loop {
            if matches!(FileExt::try_lock_exclusive(&file), Ok(true)) {
                return Some(taken(id, file, waiting.then(|| started.elapsed()), on));
            }
            if !waiting {
                waiting = true;
                waiting_for(id, &file, on);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Who holds link `id` on this machine right now: `None` when nobody does.
    pub(super) fn holder(&self, id: LinkId) -> Option<String> {
        let file = open(&self.path(id)?).ok()?;
        if matches!(FileExt::try_lock_exclusive(&file), Ok(true)) {
            let _ = FileExt::unlock(&file);
            return None;
        }
        Some(holder_of(&file).unwrap_or_else(|| "another process".to_string()))
    }
}

/// Says that another process holds link `id`, and who.
fn waiting_for(id: LinkId, file: &File, on: &mut (dyn FnMut(Progress) + Send)) {
    let holder = holder_of(file).unwrap_or_else(|| "another process".to_string());
    tracing::info!(link = %id, holder = %holder, "another process on this machine holds the link: waiting for it");
    on(Progress::WaitingForLink { holder });
}

/// The lock of link `id`, just taken (after `waited`, when it was waited for), recorded as this
/// process's.
fn taken(
    id: LinkId,
    file: File,
    waited: Option<Duration>,
    on: &mut (dyn FnMut(Progress) + Send),
) -> LinkLock {
    if let Some(waited) = waited {
        let waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        tracing::info!(link = %id, waited_ms, "the other process let go of the link: starting its session");
        on(Progress::LinkTaken { waited });
    }
    // A holder that lets go clears its record: one still there ended without letting go (it
    // crashed, or was killed), and the operating system freed its lock.
    if let Some(previous) = holder_of(&file) {
        tracing::warn!(
            link = %id,
            previous = %previous,
            "the link's previous holder ended without letting go of it (crashed or killed): taking it over"
        );
    }
    let mut lock = LinkLock { file };
    lock.record_holder();
    lock
}

fn open(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// What the holder wrote down: `process <pid> since <unix seconds>`.
fn holder_of(file: &File) -> Option<String> {
    let mut text = String::new();
    let mut reader = file;
    reader.seek(SeekFrom::Start(0)).ok()?;
    reader.read_to_string(&mut text).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A link's lock, held: released (and its record cleared) when dropped.
#[derive(Debug)]
pub(super) struct LinkLock {
    file: File,
}

impl LinkLock {
    fn record_holder(&mut self) {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        let _ = self.file.set_len(0);
        let _ = self.file.seek(SeekFrom::Start(0));
        let _ = write!(self.file, "process {} since {since}", std::process::id());
        let _ = self.file.flush();
    }
}

impl Drop for LinkLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = FileExt::unlock(&self.file);
    }
}
