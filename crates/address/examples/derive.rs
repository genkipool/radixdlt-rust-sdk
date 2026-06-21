//! Derives a Radix virtual-account address from an Ed25519 public key.
//!
//! Usage:
//!   cargo run -p radixdlt-address --example derive -- <pubkey_hex> [network_id]
//!
//! Error messages are printed in the system language (set RADIXDLT_LANG=es|en to force).

use radixdlt_address::virtual_account_address;

fn main() {
    let mut args = std::env::args().skip(1);
    let pubkey = args.next().unwrap_or_else(|| {
        // Sample Stokenet key when none is given.
        "fb92c06213fa5d789d90eafb919f2705fc2d665e918ffe69ceaf35a22531f32c".to_string()
    });
    let network_id: u8 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);

    match virtual_account_address(&pubkey, network_id) {
        Ok(addr) => println!("{addr}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
