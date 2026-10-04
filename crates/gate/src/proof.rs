//! The wallet's answer and what it proves.
//!
//! The shape is `{address, proof:{publicKey, signature}}`, plus `personaProof` when a persona signed
//! too — what a login through Radix Connect returns. Both halves are verified with ROLA, offline:
//! the key must derive to the address it claims, and must have signed THIS challenge for THIS dApp
//! and origin. No network, no Gateway, nothing to be down. (An account whose key was rotated on
//! the ledger does not derive from its key; a Gate that must accept those reads its owner keys.)

use radixdlt_rola::{verify_account_proof, verify_persona_proof, AccountProof};
use serde::Deserialize;

/// The public key and signature of one proof.
#[derive(Debug, Clone, Deserialize)]
pub struct KeySignature {
    /// The Ed25519 public key, hex.
    #[serde(rename = "publicKey")]
    pub public_key: String,
    /// The signature over the ROLA message, hex.
    pub signature: String,
}

/// The persona half, as the wallet returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct PersonaProof {
    /// The persona's `identity_…` address.
    pub address: String,
    /// Its key and signature.
    pub proof: KeySignature,
}

/// What a signer prints.
#[derive(Debug, Clone, Deserialize)]
pub struct SignedProof {
    /// The `account_…` address that signed.
    pub address: String,
    /// Its key and signature.
    pub proof: KeySignature,
    /// The persona's proof, when a persona signed too.
    #[serde(rename = "personaProof")]
    pub persona_proof: Option<PersonaProof>,
}

/// Who a valid proof names: the account that holds badges, and the persona when it was proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    /// The account proven: the one that holds badges or funds.
    pub account: String,
    /// The persona proven, when there was one.
    pub identity: Option<String>,
}

impl Signer {
    /// The name a credential should carry: the persona (one person whatever account they sign
    /// with), or the account when only the account was proven.
    pub fn name(&self) -> &str {
        self.identity.as_deref().unwrap_or(&self.account)
    }
}

/// Where a proof must point.
#[derive(Debug, Clone, Copy)]
pub struct Binding<'a> {
    /// The challenge, hex — for a Gate, [`GateRequest::challenge`](crate::request::GateRequest::challenge).
    pub challenge: &'a str,
    /// The dApp definition address the wallet signed for.
    pub dapp_definition: &'a str,
    /// The origin the wallet signed for.
    pub origin: &'a str,
    /// 1 Mainnet, 2 Stokenet.
    pub network_id: u8,
}

impl SignedProof {
    /// The signer, when both halves (account always, persona when present) verify.
    ///
    /// # Errors
    /// `"account"` or `"persona"`: which half did not verify. Never the reason in detail — telling
    /// a prober why a signature failed only helps the prober.
    pub fn verify(&self, at: Binding<'_>) -> Result<Signer, &'static str> {
        let account = AccountProof {
            address: self.address.clone(),
            public_key_hex: self.proof.public_key.clone(),
            signature_hex: self.proof.signature.clone(),
        };
        verify_account_proof(
            &account,
            at.challenge,
            at.dapp_definition,
            at.origin,
            at.network_id,
        )
        .map_err(|_| "account")?;
        let identity = match &self.persona_proof {
            None => None,
            Some(persona) => {
                let proof = AccountProof {
                    address: persona.address.clone(),
                    public_key_hex: persona.proof.public_key.clone(),
                    signature_hex: persona.proof.signature.clone(),
                };
                verify_persona_proof(&proof, at.challenge, at.dapp_definition, at.origin, at.network_id)
                    .map_err(|_| "persona")?;
                Some(persona.address.clone())
            }
        };
        Ok(Signer {
            account: self.address.clone(),
            identity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_that_does_not_match_its_address_names_nobody() {
        let proof: SignedProof = serde_json::from_str(
            r#"{"address":"account_tdx_2_12xned60hq7pu6t83nvadgj0jx3wnjhgwlvxfsd7829v2p9fjxlqfxd",
                "proof":{"publicKey":"00","signature":"00"}}"#,
        )
        .unwrap_or_else(|_| SignedProof {
            address: String::new(),
            proof: KeySignature {
                public_key: String::new(),
                signature: String::new(),
            },
            persona_proof: None,
        });
        let at = Binding {
            challenge: &"ab".repeat(32),
            dapp_definition: "account_x",
            origin: "https://x.test",
            network_id: 2,
        };
        assert_eq!(proof.verify(at), Err("account"));
    }

    #[test]
    fn the_name_is_the_persona_when_there_is_one() {
        let signer = Signer {
            account: "account_a".into(),
            identity: Some("identity_b".into()),
        };
        assert_eq!(signer.name(), "identity_b");
        assert_eq!(
            Signer {
                account: "account_a".into(),
                identity: None
            }
            .name(),
            "account_a"
        );
    }
}
