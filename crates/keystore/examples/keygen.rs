//! Generates an encrypted Ed25519 key, saves it, reloads it and unlocks it.
//!
//!   cargo run -p radixdlt-keystore --example keygen -- [path]

use radixdlt_keystore::KeyFile;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/radixdlt-key.json".to_string());
    let passphrase = "example-passphrase";

    // Create a fresh stokenet key and persist it (0600, parent dirs created).
    let kf = KeyFile::generate(2, passphrase).expect("generate");
    kf.save(&path).expect("save");
    println!("created {path} for account {}", kf.address);

    // Reload from disk and unlock with the passphrase.
    let loaded = KeyFile::load(&path).expect("load");
    let _signing_key = loaded.signing_key(passphrase).expect("unlock");
    println!("reloaded and unlocked; address still {}", loaded.address);
}
