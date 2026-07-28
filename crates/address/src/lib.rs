//! radixdlt-address — Native derivation of a Radix virtual-account address from an
//! Ed25519 public key, using `radix-common` (no Node, no RET-via-JS).

// In TESTS a panic IS the failure mechanism. Library code keeps the deny: a panic there is
// taken in the CONSUMER's process, which they neither chose nor can catch.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use radix_common::address::AddressBech32Encoder;
use radix_common::crypto::Ed25519PublicKey;
use radix_common::network::NetworkDefinition;
use radix_common::types::ComponentAddress;
use radixdlt_i18n::{tr, Lang};

/// Address-derivation errors. Their `Display` text is localized to the system language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    /// The public key hex is invalid.
    InvalidHex(String),
    /// The public key is not 32 bytes.
    InvalidKeyLength,
    /// Unknown network id.
    UnknownNetwork(u8),
    /// Could not bech32m-encode the address.
    Encode(String),
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            AddressError::InvalidHex(e) => tr!(
                lang,
                format!("invalid public key hex: {e}"),
                format!("clave pública en hex inválida: {e}")
            ),
            AddressError::InvalidKeyLength => tr!(
                lang,
                "invalid Ed25519 public key (32 bytes expected)".to_string(),
                "clave pública Ed25519 inválida (se esperan 32 bytes)".to_string()
            ),
            AddressError::UnknownNetwork(id) => tr!(
                lang,
                format!("unknown network: {id}"),
                format!("red desconocida: {id}")
            ),
            AddressError::Encode(e) => tr!(
                lang,
                format!("could not encode the address: {e}"),
                format!("no se pudo codificar la dirección: {e}")
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for AddressError {}

/// Returns the Radix network by id (1 = mainnet, 2 = stokenet).
pub fn network_by_id(network_id: u8) -> Option<NetworkDefinition> {
    match network_id {
        1 => Some(NetworkDefinition::mainnet()),
        2 => Some(NetworkDefinition::stokenet()),
        _ => None,
    }
}

/// Derives the `account_...` (bech32m) address of an Ed25519 virtual account.
///
/// `public_key_hex` is the 32-byte public key in hex.
///
/// # Errors
/// [`AddressError::InvalidHex`] when the string is not hex, and
/// [`AddressError::InvalidKeyLength`] when it does not decode to exactly 32 bytes. Both are
/// refused rather than padded or truncated: a silently altered key derives a VALID-looking
/// address that nobody holds the key to.
pub fn virtual_account_address(public_key_hex: &str, network_id: u8) -> Result<String, AddressError> {
    let bytes = hex::decode(public_key_hex).map_err(|e| AddressError::InvalidHex(e.to_string()))?;
    let pk = Ed25519PublicKey::try_from(bytes.as_slice()).map_err(|_| AddressError::InvalidKeyLength)?;
    let network = network_by_id(network_id).ok_or(AddressError::UnknownNetwork(network_id))?;

    let account = ComponentAddress::preallocated_account_from_public_key(&pk);
    let encoder = AddressBech32Encoder::new(&network);
    encoder
        .encode(account.as_bytes())
        .map_err(|e| AddressError::Encode(format!("{e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_known_address() {
        // Real key/address (Stokenet).
        let pubkey = "fb92c06213fa5d789d90eafb919f2705fc2d665e918ffe69ceaf35a22531f32c";
        let expected = "account_tdx_2_129uh80n80uc4dxr3qt8gyj5tfdsm27dle2sapu5yn55j0e73megq4x";
        let derived = virtual_account_address(pubkey, 2).expect("derivation");
        assert_eq!(derived, expected);
    }

    #[test]
    fn unknown_network_errors() {
        let pubkey = "fb92c06213fa5d789d90eafb919f2705fc2d665e918ffe69ceaf35a22531f32c";
        assert_eq!(
            virtual_account_address(pubkey, 9),
            Err(AddressError::UnknownNetwork(9))
        );
    }

    /// Each id must map to ITS network and nothing else. The address prefix comes from here, so
    /// a swapped or dropped arm derives a perfectly valid-looking address on the wrong ledger —
    /// which is how funds go somewhere nobody holds the key to.
    #[test]
    fn every_network_id_maps_to_its_own_network() {
        assert_eq!(
            network_by_id(1).map(|n| n.hrp_suffix),
            Some(NetworkDefinition::mainnet().hrp_suffix)
        );
        assert_eq!(
            network_by_id(2).map(|n| n.hrp_suffix),
            Some(NetworkDefinition::stokenet().hrp_suffix)
        );
        assert_ne!(
            network_by_id(1).map(|n| n.hrp_suffix),
            network_by_id(2).map(|n| n.hrp_suffix),
            "mainnet and stokenet must not resolve to the same network"
        );
    }

    /// An unknown id is `None`, not a default. Falling back to a network would mean deriving an
    /// address for a chain the caller never asked about.
    #[test]
    fn an_unknown_network_id_has_no_network() {
        for id in [0u8, 3, 99, 255] {
            assert!(network_by_id(id).is_none(), "id {id} must not resolve");
        }
    }

    /// The same key on two networks is two different accounts. Verification depends on it.
    #[test]
    fn the_same_key_derives_a_different_address_per_network() {
        let pk = "b".repeat(64);
        let main = virtual_account_address(&pk, 1);
        let stoke = virtual_account_address(&pk, 2);
        assert!(main.is_ok() && stoke.is_ok());
        assert_ne!(main.unwrap(), stoke.unwrap());
    }
}
