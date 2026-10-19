//! The wallet's answer and what it proves.
//!
//! The shape is `{address, proof}`, plus `personaProof` when a persona signed too — each `proof`
//! a wallet's `{publicKey, signature}` or a delegated one (a TPM or a passkey speaking for the
//! address) — what a login through Radix Connect returns. Both halves are verified with ROLA, offline:
//! the key must derive to the address it claims, and must have signed THIS challenge for THIS dApp
//! and origin. No network, no Gateway, nothing to be down. (An account whose key was rotated on
//! the ledger does not derive from its key; a Gate that must accept those reads its owner keys.)

use radixdlt_rola::{verify_signed, Holder, Method, Request, SignedProof as Signed};
use serde::Deserialize;
use serde_json::Value;

/// The persona half, as the wallet returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct PersonaProof {
    /// The persona's `identity_…` address.
    pub address: String,
    /// Its proof: a wallet's `{publicKey, signature}`, or a delegated one.
    pub proof: Value,
}

/// What a signer prints.
#[derive(Debug, Clone, Deserialize)]
pub struct SignedProof {
    /// The `account_…` address that signed.
    pub address: String,
    /// Its proof: a wallet's `{publicKey, signature}`, or a delegated one (`docs/DELEGATED.md`
    /// of `radixdlt-rola`).
    pub proof: Value,
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
    /// How the account signed: the wallet, or a delegated key (and which).
    pub method: Method,
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
    /// What the credential is for (`"kube"`, `"aws"`): an agent's delegated key must list it.
    pub purpose: &'a str,
}

fn verified(address: &str, holder: Holder, proof: &Value, at: &Request<'_>) -> Option<Method> {
    let signed = Signed::from_json(proof).ok()?;
    verify_signed(address, holder, &signed, at).ok()
}

impl SignedProof {
    /// The signer, when both halves (account always, persona when present) verify.
    ///
    /// # Errors
    /// `"account"` or `"persona"`: which half did not verify. Never the reason in detail — telling
    /// a prober why a signature failed only helps the prober.
    pub fn verify(&self, at: Binding<'_>) -> Result<Signer, &'static str> {
        let request = Request {
            challenge_hex: at.challenge,
            dapp_definition: at.dapp_definition,
            origin: at.origin,
            network_id: at.network_id,
            purpose: at.purpose,
        };
        let method = verified(&self.address, Holder::Account, &self.proof, &request).ok_or("account")?;
        let identity = match &self.persona_proof {
            None => None,
            Some(persona) => {
                verified(&persona.address, Holder::Identity, &persona.proof, &request).ok_or("persona")?;
                Some(persona.address.clone())
            }
        };
        Ok(Signer {
            account: self.address.clone(),
            identity,
            method,
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
            proof: Value::Null,
            persona_proof: None,
        });
        let at = Binding {
            challenge: &"ab".repeat(32),
            dapp_definition: "account_x",
            origin: "https://x.test",
            network_id: 2,
            purpose: "kube",
        };
        assert_eq!(proof.verify(at), Err("account"));
    }

    #[test]
    fn the_name_is_the_persona_when_there_is_one() {
        let signer = Signer {
            account: "account_a".into(),
            identity: Some("identity_b".into()),
            method: Method::Wallet,
        };
        assert_eq!(signer.name(), "identity_b");
        assert_eq!(
            Signer {
                account: "account_a".into(),
                identity: None,
                method: Method::Wallet,
            }
            .name(),
            "account_a"
        );
    }

    /// A delegated proof (a TPM key speaking for the address) passes the Gate like a wallet's,
    /// and an agent's only for what its certificate lists.
    #[test]
    fn a_delegated_agent_opens_only_what_it_lists() {
        let text = include_str!("../../rola/tests/vectors/delegated-v1.json");
        let vectors: Value = serde_json::from_str(text).unwrap_or_default();
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let agent = vectors
            .get("cases")
            .and_then(Value::as_array)
            .and_then(|cases| {
                cases
                    .iter()
                    .find(|c| c.get("name").and_then(Value::as_str) == Some("agent, listed purpose"))
            })
            .cloned()
            .unwrap_or_default();
        let proof = SignedProof {
            address: text(&agent, "address"),
            proof: agent.get("proof").cloned().unwrap_or_default(),
            persona_proof: None,
        };
        let (challenge, dapp, origin) = (
            text(&vectors, "challenge"),
            text(&vectors, "dAppDefinition"),
            text(&vectors, "origin"),
        );
        let at = |purpose| Binding {
            challenge: &challenge,
            dapp_definition: &dapp,
            origin: &origin,
            network_id: 2,
            purpose,
        };
        let signer = proof.verify(at("kube"));
        assert!(matches!(
            signer,
            Ok(Signer {
                method: Method::Delegated { .. },
                ..
            })
        ));
        assert_eq!(proof.verify(at("aws")), Err("account"));
    }
}
