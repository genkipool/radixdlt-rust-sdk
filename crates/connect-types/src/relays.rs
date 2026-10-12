//! Where extra signaling relays are configured on a machine, shared by the transports and by the
//! programs that add a relay of their own (a local relay reached over a USB cable, say).

/// The relays file every user of this machine shares: `/etc/radix-connect/relays`, or
/// `%ProgramData%\radix-connect\relays` on Windows. One URL per line, `#` for comments.
#[must_use]
pub fn system_relays_file() -> std::path::PathBuf {
    if cfg!(windows) {
        let data = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
        std::path::Path::new(&data).join("radix-connect").join("relays")
    } else {
        std::path::PathBuf::from("/etc/radix-connect/relays")
    }
}

/// The relays file of the person running this program: `~/.config/radix-connect/relays`
/// (`$XDG_CONFIG_HOME` first), or `%APPDATA%\radix-connect\relays` on Windows.
#[must_use]
pub fn user_relays_file() -> Option<std::path::PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")))
    };
    base.map(|dir| dir.join("radix-connect").join("relays"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_file_is_shared_by_every_user() {
        let file = system_relays_file();
        assert!(file.ends_with(std::path::Path::new("radix-connect").join("relays")));
        if !cfg!(windows) {
            assert_eq!(file, std::path::PathBuf::from("/etc/radix-connect/relays"));
        }
    }
}
