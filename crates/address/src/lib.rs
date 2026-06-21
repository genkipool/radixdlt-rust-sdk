//! radixdlt-address — Native derivation of a Radix virtual-account address from an
//! Ed25519 public key, using `radix-common` (no Node, no RET-via-JS).

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
pub fn virtual_account_address(
    public_key_hex: &str,
    network_id: u8,
) -> Result<String, AddressError> {
    let bytes = hex::decode(public_key_hex).map_err(|e| AddressError::InvalidHex(e.to_string()))?;
    let pk =
        Ed25519PublicKey::try_from(bytes.as_slice()).map_err(|_| AddressError::InvalidKeyLength)?;
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
}
