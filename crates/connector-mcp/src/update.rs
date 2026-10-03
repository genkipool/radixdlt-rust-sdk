//! Knowing when there is a newer connector, and installing it.
//!
//! Releases are the GitHub releases tagged `connector-vX.Y.Z`, one binary per platform named
//! `radix-connector-mcp-<target>` (`.exe` on Windows), each with a `<binary>.sha256` beside it
//! from 0.4.0 on — the same assets `scripts/install-connector.sh` downloads.
//!
//! Updating replaces ONLY the binary. The pairing with the phone lives in `connector.json` (and
//! the request record in `requests.json`) in the config directory, which an update never touches:
//! a paired wallet stays paired.

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

const REPO: &str = "genkipool/radixdlt-rust-sdk";
const TAG_PREFIX: &str = "connector-v";

/// The version this binary is.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The release asset name for the platform this binary was built for.
pub fn asset_name() -> Option<String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    Some(format!("radix-connector-mcp-{target}{ext}"))
}

/// `X.Y.Z` as numbers; anything after a `-` (a pre-release) makes it not a release.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches(TAG_PREFIX).trim_start_matches('v');
    if v.contains('-') {
        return None;
    }
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// The newest published release among `releases` (the GitHub API's list).
fn newest(releases: &[Value]) -> Option<(String, (u64, u64, u64))> {
    releases
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(true) && !r["prerelease"].as_bool().unwrap_or(true))
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            tag.starts_with(TAG_PREFIX)
                .then(|| parse_version(tag).map(|v| (tag.to_string(), v)))?
        })
        .max_by_key(|(_, v)| *v)
}

/// What a check found.
pub struct Check {
    pub current: String,
    pub latest_tag: String,
    pub latest: String,
    pub newer: bool,
}

impl Check {
    pub fn describe(&self) -> String {
        if self.newer {
            format!(
                "UPDATE AVAILABLE: radix-connector-mcp {latest} (installed: {current}).\n\
                 Install it with:  radix-connector-mcp update\n\
                 (or the update_connector tool). The pairing with the phone is kept: it lives in \
                 connector.json, which an update never touches.",
                latest = self.latest,
                current = self.current,
            )
        } else {
            format!(
                "UP TO DATE: radix-connector-mcp {current} (latest release: {latest}).",
                current = self.current,
                latest = self.latest,
            )
        }
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("radix-connector-mcp/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))
}

/// Asks GitHub for the newest connector release.
pub async fn check() -> Result<Check, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=50");
    let resp = client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("could not reach GitHub ({url}): {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub answered HTTP {} for {url}", resp.status()));
    }
    let releases: Vec<Value> = resp
        .json()
        .await
        .map_err(|e| format!("unexpected GitHub answer: {e}"))?;
    let (tag, latest) = newest(&releases).ok_or("no connector release found on GitHub")?;
    let current = parse_version(CURRENT).ok_or("this binary's own version is not X.Y.Z")?;
    Ok(Check {
        current: CURRENT.to_string(),
        latest_tag: tag,
        latest: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        newer: latest > current,
    })
}

/// Downloads `tag` (default: the newest), checks it, and puts it in place of this binary.
/// Returns what happened, for a person to read.
pub async fn update(tag: Option<&str>, force: bool) -> Result<String, String> {
    let (tag, version) = match tag {
        Some(tag) => {
            let tag = if tag.starts_with(TAG_PREFIX) {
                tag.to_string()
            } else {
                format!("{TAG_PREFIX}{}", tag.trim_start_matches('v'))
            };
            let v = parse_version(&tag).ok_or(format!("'{tag}' is not a connector release tag"))?;
            (tag, format!("{}.{}.{}", v.0, v.1, v.2))
        }
        None => {
            let found = check().await?;
            if !found.newer && !force {
                return Ok(found.describe());
            }
            (found.latest_tag, found.latest)
        }
    };
    let asset =
        asset_name().ok_or("no prebuilt binary for this platform: build it with cargo install --git")?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);

    let base = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    let http = client()?;
    let bytes = http
        .get(&base)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("download failed ({base}): {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download failed ({base}): {e}"))?;
    if bytes.is_empty() {
        return Err(format!("the download from {base} is empty"));
    }

    // The checksum published beside the binary (from 0.4.0 on). Older releases have none.
    let digest = hex::encode(Sha256::digest(&bytes));
    let checksum = match http.get(format!("{base}.sha256")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let text = resp.text().await.unwrap_or_default();
            let expected = text.split_whitespace().next().unwrap_or_default().to_lowercase();
            if expected != digest {
                return Err(format!(
                    "the download does NOT match its published SHA-256 (expected {expected}, got {digest}); nothing was changed"
                ));
            }
            "SHA-256 verified"
        }
        _ => "no published SHA-256 for this release (older than 0.4.0); downloaded over HTTPS from GitHub",
    };

    let new_path = sibling(&exe, "new");
    std::fs::write(&new_path, &bytes).map_err(|e| format!("could not write {}: {e}", new_path.display()))?;
    make_executable(&new_path)?;

    // It must RUN before it replaces anything: a binary for the wrong platform, or a broken one,
    // would otherwise take the connector away until somebody reinstalls it by hand.
    let ran = std::process::Command::new(&new_path).arg("version").output();
    match ran {
        // From 0.4.0 `version` prints it; older releases have no subcommands, start as a server,
        // find stdin closed and exit — after announcing their version on stderr.
        Ok(out)
            if out.status.success()
                && (String::from_utf8_lossy(&out.stdout).contains(&version)
                    || String::from_utf8_lossy(&out.stderr).contains(&version)) => {}
        Ok(out) => {
            let _ = std::fs::remove_file(&new_path);
            return Err(format!(
                "the downloaded binary did not report version {version} (it said: {}); nothing was changed",
                String::from_utf8_lossy(&out.stdout).trim()
            ));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&new_path);
            return Err(format!(
                "the downloaded binary does not run here ({e}); nothing was changed"
            ));
        }
    }

    let backup = sibling(&exe, &format!("{CURRENT}.bak"));
    std::fs::copy(&exe, &backup).map_err(|e| format!("could not back up {}: {e}", exe.display()))?;
    replace(&new_path, &exe)?;
    Ok(format!(
        "UPDATED: radix-connector-mcp {CURRENT} → {version} ({checksum}).\n\
         Binary:  {exe}\n\
         Backup:  {backup}\n\
         The pairing with the phone is kept (connector.json is not touched): no need to pair again.\n\
         Restart the MCP client (or its session) so it launches the new version.",
        exe = exe.display(),
        backup = backup.display(),
    ))
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{suffix}"));
    exe.with_file_name(name)
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("could not make {} executable: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Puts `new` where `exe` is. On Unix a rename over a running binary is fine (the running
/// process keeps its inode); Windows refuses to overwrite a running .exe but lets it be renamed.
fn replace(new: &Path, exe: &Path) -> Result<(), String> {
    if cfg!(windows) {
        let old = sibling(exe, "old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).map_err(|e| format!("could not move the running binary aside: {e}"))?;
    }
    std::fs::rename(new, exe).map_err(|e| format!("could not install {}: {e}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_as_numbers_and_skip_pre_releases() {
        assert_eq!(parse_version("connector-v0.10.2"), Some((0, 10, 2)));
        assert_eq!(parse_version("0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_version("connector-v0.5.0-rc1"), None);
        assert_eq!(parse_version("connector-v1.2"), None);
        assert!(parse_version("connector-v0.10.0") > parse_version("connector-v0.9.9"));
    }

    #[test]
    fn the_newest_connector_release_wins_regardless_of_order() {
        let releases = vec![
            json!({ "tag_name": "connector-v0.3.2", "draft": false, "prerelease": false }),
            json!({ "tag_name": "connector-v0.10.0", "draft": false, "prerelease": false }),
            json!({ "tag_name": "connector-v0.11.0", "draft": true, "prerelease": false }),
            json!({ "tag_name": "connector-v0.12.0", "draft": false, "prerelease": true }),
            json!({ "tag_name": "sdk-v9.0.0", "draft": false, "prerelease": false }),
        ];
        assert_eq!(
            newest(&releases),
            Some(("connector-v0.10.0".to_string(), (0, 10, 0)))
        );
        assert_eq!(newest(&[]), None);
    }

    #[test]
    fn this_platform_has_an_asset_name_and_backups_sit_beside_the_binary() {
        if let Some(name) = asset_name() {
            assert!(name.starts_with("radix-connector-mcp-"));
        }
        let exe = Path::new("/opt/bin/radix-connector-mcp");
        assert_eq!(
            sibling(exe, "0.4.0.bak"),
            Path::new("/opt/bin/radix-connector-mcp.0.4.0.bak")
        );
    }
}
