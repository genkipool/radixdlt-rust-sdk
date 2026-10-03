//! The persona/account side of the wallet protocol, as tool arguments in and readable answers out:
//! quantities («at least 2», «exactly 1»), persona data (name, emails, phones), and the answers —
//! personas, accounts shared one-time or ongoing, persona data, and proofs, each proof verified
//! here against the challenge THIS connector sent (never the one the answer echoes back).

use radixdlt_connect::{AccountsWanted, PersonaDataWanted, Quantity};
use radixdlt_rola::{verify_account_proof, verify_persona_proof, AccountProof};
use serde_json::{json, Value};

/* ─────────────────────────────── arguments ─────────────────────────────── */

/// A quantity: `2` means «at least 2»; `{ "quantity": 2, "exactly": true }` means «exactly 2».
/// `0`, `false` and absence mean «do not ask».
pub fn parse_quantity(v: Option<&Value>, what: &str) -> Result<Option<Quantity>, String> {
    let Some(v) = v else { return Ok(None) };
    let (n, exactly) = match v {
        Value::Null | Value::Bool(false) => return Ok(None),
        Value::Bool(true) => (1, false),
        Value::Number(n) => (
            n.as_u64().ok_or(format!("'{what}' must be a whole number"))?,
            false,
        ),
        Value::Object(o) => (
            o.get("quantity")
                .and_then(Value::as_u64)
                .ok_or(format!("'{what}.quantity' must be a whole number"))?,
            o.get("exactly").and_then(Value::as_bool).unwrap_or(false),
        ),
        _ => return Err(format!("'{what}' must be a number or {{ quantity, exactly }}")),
    };
    if n == 0 {
        return Ok(None);
    }
    let n = u16::try_from(n).map_err(|_| format!("'{what}' is too large"))?;
    Ok(Some(if exactly {
        Quantity::Exactly(n)
    } else {
        Quantity::AtLeast(n)
    }))
}

/// Accounts to ask for: `{ "quantity": 1, "exactly": false, "with_proof": true }` (or just a
/// number). With `with_proof` each account signs `challenge` (ROLA).
pub fn parse_accounts(
    v: Option<&Value>,
    what: &str,
    challenge: &str,
) -> Result<Option<AccountsWanted>, String> {
    let Some(quantity) = parse_quantity(v, what)? else {
        return Ok(None);
    };
    let with_proof = v
        .and_then(|v| v.get("with_proof"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Some(AccountsWanted {
        quantity,
        challenge: with_proof.then(|| challenge.to_string()),
    }))
}

/// Persona data to ask for: `{ "name": true, "emails": 1, "phones": { "quantity": 1, "exactly": true } }`.
pub fn parse_persona_data(v: Option<&Value>, what: &str) -> Result<Option<PersonaDataWanted>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if !v.is_object() {
        return Err(format!("'{what}' must be an object {{ name, emails, phones }}"));
    }
    let data = PersonaDataWanted {
        name: v.get("name").and_then(Value::as_bool).unwrap_or(false),
        emails: parse_quantity(v.get("emails"), &format!("{what}.emails"))?,
        phones: parse_quantity(v.get("phones"), &format!("{what}.phones"))?,
    };
    if !data.name && data.emails.is_none() && data.phones.is_none() {
        return Ok(None);
    }
    Ok(Some(data))
}

/// A fresh 32-byte challenge, hex — for a proof checked here rather than by a server.
pub fn random_challenge() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("system randomness: {e}"))?;
    Ok(hex::encode(bytes))
}

/// A challenge given by the caller: 32 bytes of hex.
pub fn check_challenge(challenge: &str) -> Result<(), String> {
    match hex::decode(challenge) {
        Ok(bytes) if bytes.len() == 32 => Ok(()),
        Ok(bytes) => Err(format!(
            "'challenge' must be 32 bytes of hex (got {} bytes)",
            bytes.len()
        )),
        Err(_) => Err("'challenge' must be hex".to_string()),
    }
}

/* ──────────────────────────────── answers ──────────────────────────────── */

/// What a proof is checked against: the challenge sent, and the dApp it was sent as.
pub struct Verifier<'a> {
    pub challenge: &'a str,
    pub dapp_definition: &'a str,
    pub origin: &'a str,
    pub network_id: u8,
}

impl Verifier<'_> {
    /// `✓ verified`, or why not, for one proof `{ publicKey, signature, curve }` of `address`.
    fn verdict(&self, address: &str, persona: bool, proof: &Value, echoed: Option<&str>) -> String {
        if echoed.is_some_and(|c| !c.is_empty() && c != self.challenge) {
            return "✗ NOT VERIFIED (it signs a different challenge than the one sent)".to_string();
        }
        let curve = proof.get("curve").and_then(Value::as_str).unwrap_or("curve25519");
        if curve != "curve25519" {
            return format!("– not checked here ({curve} key: only Ed25519 is verified locally)");
        }
        let ap = AccountProof {
            address: address.to_string(),
            public_key_hex: proof
                .get("publicKey")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            signature_hex: proof
                .get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        let check = if persona {
            verify_persona_proof
        } else {
            verify_account_proof
        };
        match check(
            &ap,
            self.challenge,
            self.dapp_definition,
            self.origin,
            self.network_id,
        ) {
            Ok(()) => "✓ verified".to_string(),
            Err(e) => format!("✗ NOT VERIFIED ({e})"),
        }
    }
}

/// The answer, laid out for a person to read (and an agent to quote).
pub fn render_answer(response: &Value, verifier: Option<&Verifier>) -> String {
    let empty = json!({});
    let items = response.get("items").unwrap_or(&empty);
    let mut out = String::new();

    if let Some(auth) = items.get("auth") {
        let persona = auth.get("persona").unwrap_or(&empty);
        let identity = persona
            .get("identityAddress")
            .and_then(Value::as_str)
            .unwrap_or("?");
        out.push_str(&format!(
            "Persona:  {label}  ({identity})\n",
            label = persona
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("(no label)"),
        ));
        match (auth.get("proof"), verifier) {
            (Some(proof), Some(v)) => out.push_str(&format!(
                "          login proof: {}\n",
                v.verdict(
                    identity,
                    true,
                    proof,
                    auth.get("challenge").and_then(Value::as_str)
                )
            )),
            (Some(_), None) => out.push_str("          login proof: present (not checked)\n"),
            (None, _) => {
                let how = auth.get("discriminator").and_then(Value::as_str).unwrap_or("");
                out.push_str(&format!(
                    "          no proof ({how}: the persona is named, not proven)\n"
                ));
            }
        }
    }

    for (key, title) in [
        ("oneTimeAccounts", "Accounts (one-time)"),
        ("ongoingAccounts", "Accounts (ongoing)"),
    ] {
        let Some(section) = items.get(key) else { continue };
        let accounts = section
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let proofs = section
            .get("proofs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let echoed = section.get("challenge").and_then(Value::as_str);
        out.push_str(&format!("{title}: {}\n", accounts.len()));
        for account in &accounts {
            let address = account.get("address").and_then(Value::as_str).unwrap_or("?");
            out.push_str(&format!(
                "  - {address}  [{label}]\n",
                label = account.get("label").and_then(Value::as_str).unwrap_or("no label"),
            ));
            let proof = proofs
                .iter()
                .find(|p| p.get("accountAddress").and_then(Value::as_str) == Some(address))
                .and_then(|p| p.get("proof"));
            match (proof, verifier) {
                (Some(proof), Some(v)) => {
                    out.push_str(&format!(
                        "      proof: {}\n",
                        v.verdict(address, false, proof, echoed)
                    ));
                }
                (Some(_), None) => out.push_str("      proof: present (not checked)\n"),
                (None, _) => {}
            }
        }
    }

    for (key, title) in [
        ("oneTimePersonaData", "Persona data (one-time)"),
        ("ongoingPersonaData", "Persona data (ongoing)"),
    ] {
        let Some(data) = items.get(key) else { continue };
        // The crate's readers look at `oneTimePersonaData`; present either section to them as one.
        let as_one_time = json!({ "items": { "oneTimePersonaData": data } });
        let emails = list_of(data.get("emailAddresses"));
        let phones = list_of(data.get("phoneNumbers"));
        out.push_str(&format!(
            "{title}:\n  name:   {name}\n  emails: {emails}\n  phones: {phones}\n",
            name = radixdlt_connect::extract_persona_name(&as_one_time).unwrap_or_else(|| "—".into()),
            emails = if emails.is_empty() {
                "—".into()
            } else {
                emails.join(", ")
            },
            phones = if phones.is_empty() {
                "—".into()
            } else {
                phones.join(", ")
            },
        ));
    }

    if let Some(own) = items.get("proofOfOwnership") {
        let proofs = own
            .get("proofs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let echoed = own.get("challenge").and_then(Value::as_str);
        out.push_str(&format!("Proof of ownership: {}\n", proofs.len()));
        for entry in &proofs {
            let (address, persona) = match (entry.get("accountAddress"), entry.get("identityAddress")) {
                (Some(a), _) => (a.as_str().unwrap_or("?"), false),
                (None, Some(i)) => (i.as_str().unwrap_or("?"), true),
                (None, None) => continue,
            };
            let verdict = match (entry.get("proof"), verifier) {
                (Some(proof), Some(v)) => v.verdict(address, persona, proof, echoed),
                _ => "present (not checked)".to_string(),
            };
            out.push_str(&format!(
                "  - {kind} {address}: {verdict}\n",
                kind = if persona { "persona" } else { "account" }
            ));
        }
    }

    if out.is_empty() {
        out.push_str("(the answer carries nothing to show)\n");
    }
    out
}

fn list_of(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| match entry {
                    Value::String(s) => Some(s.clone()),
                    other => other.get("value")?.as_str().map(str::to_string),
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// One line saying what an answer was — for the request record and for late answers.
pub fn describe_answer(response: &Value) -> String {
    if let Err(e) = radixdlt_connect::check_failure(response) {
        return format!("FAILURE — {e}");
    }
    let items = response.get("items").cloned().unwrap_or(Value::Null);
    if let Ok(txid) = radixdlt_connect::extract_transaction_intent_hash(response) {
        return format!("transaction SUBMITTED — {txid}");
    }
    if radixdlt_connect::extract_signed_partial_transaction(response).is_ok() {
        return "pre-authorization SIGNED (not submitted)".to_string();
    }
    let mut parts = Vec::new();
    if let Some(identity) = items
        .pointer("/auth/persona/identityAddress")
        .and_then(Value::as_str)
    {
        parts.push(format!("persona {identity}"));
    }
    for key in ["oneTimeAccounts", "ongoingAccounts"] {
        if let Some(n) = items
            .get(key)
            .and_then(|s| s.get("accounts"))
            .and_then(Value::as_array)
        {
            parts.push(format!("{} account(s)", n.len()));
        }
    }
    if items.get("proofOfOwnership").is_some() {
        parts.push("proof of ownership".to_string());
    }
    if items.get("oneTimePersonaData").is_some() || items.get("ongoingPersonaData").is_some() {
        parts.push("persona data".to_string());
    }
    if parts.is_empty() {
        "answered".to_string()
    } else {
        format!("shared: {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn quantities_read_as_at_least_unless_exact() {
        assert_eq!(
            parse_quantity(Some(&json!(2)), "q").unwrap(),
            Some(Quantity::AtLeast(2))
        );
        assert_eq!(
            parse_quantity(Some(&json!({ "quantity": 1, "exactly": true })), "q").unwrap(),
            Some(Quantity::Exactly(1))
        );
        assert_eq!(parse_quantity(Some(&json!(0)), "q").unwrap(), None);
        assert_eq!(parse_quantity(None, "q").unwrap(), None);
        assert!(parse_quantity(Some(&json!("two")), "q").is_err());
    }

    #[test]
    fn persona_data_asks_only_for_what_is_named() {
        let data = parse_persona_data(Some(&json!({ "name": true, "phones": 1 })), "d")
            .unwrap()
            .unwrap();
        assert!(data.name);
        assert_eq!(data.emails, None);
        assert_eq!(data.phones, Some(Quantity::AtLeast(1)));
        assert!(parse_persona_data(Some(&json!({})), "d").unwrap().is_none());
    }

    #[test]
    fn accounts_sign_the_challenge_only_when_asked() {
        let with = parse_accounts(Some(&json!({ "quantity": 1, "with_proof": true })), "a", "ab")
            .unwrap()
            .unwrap();
        assert_eq!(with.challenge.as_deref(), Some("ab"));
        let without = parse_accounts(Some(&json!(1)), "a", "ab").unwrap().unwrap();
        assert_eq!(without.challenge, None);
    }

    /// A proof is checked against the challenge SENT: one echoing another challenge fails even
    /// with a valid signature over it.
    #[test]
    fn proofs_are_verified_against_the_challenge_sent() {
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let pk = hex::encode(sk.verifying_key().to_bytes());
        let address = radixdlt_address::virtual_account_address(&pk, 2).unwrap();
        let challenge = "11".repeat(32);
        let (dapp, origin) = ("account_tdx_2_dapp", "https://example.test");
        let msg = radixdlt_rola::signature_message(&challenge, dapp, origin).unwrap();
        let proof = json!({ "publicKey": pk, "signature": hex::encode(sk.sign(&msg).to_bytes()), "curve": "curve25519" });
        let v = Verifier {
            challenge: &challenge,
            dapp_definition: dapp,
            origin,
            network_id: 2,
        };
        assert_eq!(v.verdict(&address, false, &proof, Some(&challenge)), "✓ verified");
        assert!(v.verdict(&address, false, &proof, Some("22")).starts_with("✗"));
        assert!(
            v.verdict(&address, true, &proof, None).starts_with("✗"),
            "an account key is not a persona"
        );

        let answer = json!({ "discriminator": "success", "interactionId": "i", "items": {
            "discriminator": "unauthorizedRequest",
            "oneTimeAccounts": { "challenge": challenge, "accounts": [{ "address": address, "label": "Main" }],
                "proofs": [{ "accountAddress": address, "proof": proof }] },
            "oneTimePersonaData": { "name": { "variant": "western", "givenNames": "Ana", "familyName": "Ruiz" },
                "phoneNumbers": ["+34 600"] }
        }});
        let text = render_answer(&answer, Some(&v));
        assert!(text.contains("[Main]") && text.contains("✓ verified"), "{text}");
        assert!(text.contains("Ana Ruiz") && text.contains("+34 600"), "{text}");
        assert_eq!(describe_answer(&answer), "shared: 1 account(s), persona data");
    }

    #[test]
    fn answers_say_in_one_line_what_they_were() {
        let tx = json!({ "discriminator": "success", "items": { "discriminator": "transaction", "send": { "transactionIntentHash": "txid_tdx_2_1x" } } });
        assert_eq!(describe_answer(&tx), "transaction SUBMITTED — txid_tdx_2_1x");
        let no = json!({ "discriminator": "failure", "error": "rejectedByUser" });
        assert!(describe_answer(&no).starts_with("FAILURE"));
    }
}
