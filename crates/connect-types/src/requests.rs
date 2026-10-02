//! The rest of the Radix Connect request vocabulary, as one builder.
//!
//! The functions in the crate root each build ONE request a caller used to need: an account proof,
//! a login, a transaction. The wallet protocol can say more than that, and every piece of it is
//! here, in the shape the wallet reads (the same schema `@radixdlt/radix-dapp-toolkit` sends):
//!
//!   * **proof of ownership** of EXACT accounts and/or a persona — nothing to pick, only to confirm;
//!   * **`usePersona`** — act as a persona already logged in, without logging in again;
//!   * **`loginWithoutChallenge`** — log in without a proof, when only the persona's name is wanted;
//!   * **ongoing** accounts and persona data — what the wallet REMEMBERS having shared with this dApp,
//!     and does not ask again;
//!   * **reset** — forget what was shared;
//!   * **exact quantities** — `exactly N` as well as `at least N`;
//!   * **phone numbers** in persona data, beside the name and the email addresses.
//!
//! A proof of ownership only exists inside an AUTHORIZED request, which is the protocol's rule and
//! not this crate's: the wallet proves ownership on behalf of a persona the dApp already knows.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::{check_failure, metadata, DappContext, WalletInteractionError};

/// How many of something to ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quantity {
    /// At least this many: the person may share more.
    AtLeast(u16),
    /// Exactly this many.
    Exactly(u16),
}

impl Quantity {
    fn to_json(self) -> Value {
        match self {
            Quantity::AtLeast(n) => json!({ "quantifier": "atLeast", "quantity": n }),
            Quantity::Exactly(n) => json!({ "quantifier": "exactly", "quantity": n }),
        }
    }
}

/// Accounts to ask for, with a ROLA proof of each when a challenge is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountsWanted {
    /// How many.
    pub quantity: Quantity,
    /// The challenge each account signs, hex; `None` for addresses without proof.
    pub challenge: Option<String>,
}

impl AccountsWanted {
    fn to_json(&self) -> Value {
        let mut item = json!({ "numberOfAccounts": self.quantity.to_json() });
        if let Some(challenge) = &self.challenge {
            item["challenge"] = json!(challenge);
        }
        item
    }
}

/// Persona data to ask for. A one-time request is MANDATORY for whoever answers it: a wallet will
/// not let the person approve until they give all of it, so ask only for what is needed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PersonaDataWanted {
    /// The person's name.
    pub name: bool,
    /// Email addresses, and how many.
    pub emails: Option<Quantity>,
    /// Phone numbers, and how many.
    pub phones: Option<Quantity>,
}

impl PersonaDataWanted {
    fn to_json(self) -> Value {
        let mut item = json!({ "isRequestingName": self.name });
        if let Some(emails) = self.emails {
            item["numberOfRequestedEmailAddresses"] = emails.to_json();
        }
        if let Some(phones) = self.phones {
            item["numberOfRequestedPhoneNumbers"] = phones.to_json();
        }
        item
    }
}

/// How an authorized request says WHICH persona it is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    /// Log in, with the persona signing this challenge (hex).
    LoginWithChallenge(String),
    /// Log in without a proof: the persona is named, not proven.
    LoginWithoutChallenge,
    /// Act as this persona (`identity_…`), already logged in to this dApp — no new login.
    UsePersona(String),
}

impl Auth {
    fn to_json(&self) -> Value {
        match self {
            Auth::LoginWithChallenge(challenge) => {
                json!({ "discriminator": "loginWithChallenge", "challenge": challenge })
            }
            Auth::LoginWithoutChallenge => json!({ "discriminator": "loginWithoutChallenge" }),
            Auth::UsePersona(identity) => {
                json!({ "discriminator": "usePersona", "identityAddress": identity })
            }
        }
    }
}

/// A proof of ownership: the wallet signs `challenge` with EXACTLY these accounts and/or this
/// persona, with nothing for the person to pick — only to confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnershipWanted {
    /// The challenge every proof signs, hex.
    pub challenge: String,
    /// The accounts to prove, by address.
    pub accounts: Vec<String>,
    /// The persona to prove too, by identity address.
    pub identity: Option<String>,
}

/// An AUTHORIZED request: a persona, and whatever else is asked of the wallet in the same approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedRequest {
    /// Which persona.
    pub auth: Auth,
    /// Forget previously shared accounts / persona data before answering: `(accounts, personaData)`.
    pub reset: Option<(bool, bool)>,
    /// Prove ownership of exact accounts and/or the persona.
    pub proof_of_ownership: Option<OwnershipWanted>,
    /// Accounts shared for THIS request only.
    pub one_time_accounts: Option<AccountsWanted>,
    /// Accounts shared ONGOING: the wallet remembers them for this dApp and does not ask again.
    pub ongoing_accounts: Option<AccountsWanted>,
    /// Persona data for this request only.
    pub one_time_persona_data: Option<PersonaDataWanted>,
    /// Persona data shared ongoing, remembered like the accounts.
    pub ongoing_persona_data: Option<PersonaDataWanted>,
}

impl AuthorizedRequest {
    /// A request for `auth` and nothing else; add the rest with struct update syntax.
    #[must_use]
    pub fn new(auth: Auth) -> Self {
        AuthorizedRequest {
            auth,
            reset: None,
            proof_of_ownership: None,
            one_time_accounts: None,
            ongoing_accounts: None,
            one_time_persona_data: None,
            ongoing_persona_data: None,
        }
    }
}

/// Builds an authorized request in the wallet's shape.
#[must_use]
pub fn authorized_request(request: &AuthorizedRequest, ctx: &DappContext) -> Value {
    let mut items = json!({ "discriminator": "authorizedRequest", "auth": request.auth.to_json() });
    if let Some((accounts, persona_data)) = request.reset {
        items["reset"] = json!({ "accounts": accounts, "personaData": persona_data });
    }
    if let Some(own) = &request.proof_of_ownership {
        let mut item = json!({ "challenge": own.challenge });
        if !own.accounts.is_empty() {
            item["accountAddresses"] = json!(own.accounts);
        }
        if let Some(identity) = &own.identity {
            item["identityAddress"] = json!(identity);
        }
        items["proofOfOwnership"] = item;
    }
    if let Some(accounts) = &request.one_time_accounts {
        items["oneTimeAccounts"] = accounts.to_json();
    }
    if let Some(accounts) = &request.ongoing_accounts {
        items["ongoingAccounts"] = accounts.to_json();
    }
    if let Some(data) = request.one_time_persona_data {
        items["oneTimePersonaData"] = data.to_json();
    }
    if let Some(data) = request.ongoing_persona_data {
        items["ongoingPersonaData"] = data.to_json();
    }
    json!({ "interactionId": Uuid::new_v4().to_string(), "metadata": metadata(ctx), "items": items })
}

/// An UNAUTHORIZED request (no persona): one-time accounts and/or persona data, with exact or
/// minimum quantities and phone numbers — the general form of the crate root's account requests.
#[must_use]
pub fn unauthorized_request(
    accounts: Option<&AccountsWanted>,
    persona_data: Option<PersonaDataWanted>,
    ctx: &DappContext,
) -> Value {
    let mut items = json!({ "discriminator": "unauthorizedRequest" });
    if let Some(accounts) = accounts {
        items["oneTimeAccounts"] = accounts.to_json();
    }
    if let Some(data) = persona_data {
        items["oneTimePersonaData"] = data.to_json();
    }
    json!({ "interactionId": Uuid::new_v4().to_string(), "metadata": metadata(ctx), "items": items })
}

/// Proves EXACT accounts (and the persona, when `prove_persona`) as a persona already logged in to
/// this dApp: `usePersona` plus a proof of ownership, all signing `challenge_hex`. What a service
/// asks once it knows who somebody is — one confirmation on the phone, nothing to pick.
#[must_use]
pub fn ownership_request(
    challenge_hex: &str,
    identity: &str,
    accounts: &[String],
    prove_persona: bool,
    ctx: &DappContext,
) -> Value {
    let own = OwnershipWanted {
        challenge: challenge_hex.to_string(),
        accounts: accounts.to_vec(),
        identity: prove_persona.then(|| identity.to_string()),
    };
    let request = AuthorizedRequest {
        proof_of_ownership: Some(own),
        ..AuthorizedRequest::new(Auth::UsePersona(identity.to_string()))
    };
    authorized_request(&request, ctx)
}

/// One proof from a proof-of-ownership answer, in the shape a ROLA verifier wants:
/// `{ challenge, address, type: "account" | "persona", proof }`.
///
/// # Errors
/// As [`check_failure`], plus [`WalletInteractionError::Protocol`] when the answer has no proof
/// of ownership — what a wallet answers to a request that did not ask for one.
pub fn extract_ownership_proofs(response: &Value) -> Result<Vec<Value>, WalletInteractionError> {
    check_failure(response)?;
    let own = response
        .get("items")
        .and_then(|i| i.get("proofOfOwnership"))
        .ok_or_else(|| WalletInteractionError::Protocol("response without proofOfOwnership".into()))?;
    let challenge = own.get("challenge").and_then(Value::as_str).unwrap_or_default();
    let proofs = own
        .get("proofs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(proofs
        .iter()
        .filter_map(|entry| {
            let proof = entry.get("proof")?.clone();
            let (address, kind) = match (entry.get("accountAddress"), entry.get("identityAddress")) {
                (Some(account), _) => (account.as_str()?, "account"),
                (None, Some(identity)) => (identity.as_str()?, "persona"),
                (None, None) => return None,
            };
            Some(json!({ "challenge": challenge, "address": address, "type": kind, "proof": proof }))
        })
        .collect())
}

/// The accounts shared ONGOING, from an answer that asked for them.
///
/// # Errors
/// As [`check_failure`].
pub fn extract_ongoing_accounts(
    response: &Value,
) -> Result<Vec<(String, Option<String>)>, WalletInteractionError> {
    check_failure(response)?;
    let accounts = response
        .get("items")
        .and_then(|i| i.get("ongoingAccounts"))
        .and_then(|o| o.get("accounts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(accounts
        .iter()
        .filter_map(|account| {
            let address = account.get("address")?.as_str()?.to_string();
            let label = account.get("label").and_then(Value::as_str).map(str::to_string);
            Some((address, label))
        })
        .collect())
}

/// The phone numbers shared, one-time or ongoing, in the order the wallet gave them.
#[must_use]
pub fn extract_persona_phones(response: &Value) -> Vec<String> {
    let items = response.get("items");
    ["oneTimePersonaData", "ongoingPersonaData"]
        .iter()
        .filter_map(|key| items?.get(*key)?.get("phoneNumbers")?.as_array().cloned())
        .flatten()
        .filter_map(|entry| match entry {
            Value::String(s) => Some(s),
            other => other.get("value")?.as_str().map(str::to_string),
        })
        .filter(|phone| !phone.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> DappContext {
        DappContext::new(2, "account_tdx_2_dapp", "https://example.com")
    }

    /// The request the wallet reads for «prove THIS account (and this persona)»: `usePersona`
    /// names who, `proofOfOwnership` names what — and nothing asks the person to choose.
    #[test]
    fn a_proof_of_ownership_names_the_persona_and_the_exact_accounts() {
        let accounts = vec!["account_tdx_2_1a".to_string()];
        let r = ownership_request("ab", "identity_tdx_2_1p", &accounts, true, &ctx());
        let items = &r["items"];
        assert_eq!(items["discriminator"], "authorizedRequest");
        assert_eq!(
            items["auth"],
            json!({ "discriminator": "usePersona", "identityAddress": "identity_tdx_2_1p" })
        );
        assert_eq!(items["proofOfOwnership"]["challenge"], "ab");
        assert_eq!(
            items["proofOfOwnership"]["accountAddresses"],
            json!(["account_tdx_2_1a"])
        );
        assert_eq!(items["proofOfOwnership"]["identityAddress"], "identity_tdx_2_1p");
        // Nothing to pick: no account or persona-data request beside it.
        assert!(items.get("oneTimeAccounts").is_none() && items.get("oneTimePersonaData").is_none());
        // Accounts only: the persona is named but not proven.
        let bare = ownership_request("ab", "identity_tdx_2_1p", &accounts, false, &ctx());
        assert!(bare["items"]["proofOfOwnership"].get("identityAddress").is_none());
    }

    /// Each proof comes back typed, so an account proof is never verified as a persona's.
    #[test]
    fn ownership_proofs_come_back_typed_for_rola() {
        let answer = json!({ "discriminator": "success", "items": {
            "discriminator": "authorizedRequest",
            "auth": { "discriminator": "usePersona", "persona": { "identityAddress": "identity_tdx_2_1p", "label": "L" } },
            "proofOfOwnership": { "challenge": "ab", "proofs": [
                { "accountAddress": "account_tdx_2_1a", "proof": { "publicKey": "k1", "signature": "s1", "curve": "curve25519" } },
                { "identityAddress": "identity_tdx_2_1p", "proof": { "publicKey": "k2", "signature": "s2", "curve": "curve25519" } }
            ] }
        } });
        let proofs = extract_ownership_proofs(&answer).expect("proofs");
        assert_eq!(proofs.len(), 2);
        assert_eq!(proofs[0]["type"], "account");
        assert_eq!(proofs[0]["address"], "account_tdx_2_1a");
        assert_eq!(proofs[0]["challenge"], "ab");
        assert_eq!(proofs[1]["type"], "persona");
        assert_eq!(proofs[1]["proof"]["signature"], "s2");
        assert!(extract_ownership_proofs(&json!({ "items": {} })).is_err());
        assert!(extract_ownership_proofs(&crate::failure_response("i", "rejectedByUser")).is_err());
    }

    /// Every piece of an authorized request, in the wallet's names.
    #[test]
    fn an_authorized_request_carries_every_item_it_was_given() {
        let request = AuthorizedRequest {
            reset: Some((true, false)),
            ongoing_accounts: Some(AccountsWanted {
                quantity: Quantity::Exactly(2),
                challenge: None,
            }),
            one_time_accounts: Some(AccountsWanted {
                quantity: Quantity::AtLeast(1),
                challenge: Some("cd".into()),
            }),
            ongoing_persona_data: Some(PersonaDataWanted {
                name: true,
                emails: Some(Quantity::Exactly(1)),
                phones: None,
            }),
            one_time_persona_data: Some(PersonaDataWanted {
                name: false,
                emails: None,
                phones: Some(Quantity::AtLeast(1)),
            }),
            ..AuthorizedRequest::new(Auth::LoginWithoutChallenge)
        };
        let items = &authorized_request(&request, &ctx())["items"];
        assert_eq!(items["auth"], json!({ "discriminator": "loginWithoutChallenge" }));
        assert_eq!(items["reset"], json!({ "accounts": true, "personaData": false }));
        assert_eq!(
            items["ongoingAccounts"],
            json!({ "numberOfAccounts": { "quantifier": "exactly", "quantity": 2 } })
        );
        assert_eq!(items["oneTimeAccounts"]["challenge"], "cd");
        assert_eq!(
            items["ongoingPersonaData"]["numberOfRequestedEmailAddresses"]["quantifier"],
            "exactly"
        );
        assert_eq!(
            items["oneTimePersonaData"]["numberOfRequestedPhoneNumbers"]["quantity"],
            1
        );
        assert!(items.get("proofOfOwnership").is_none());
        // `loginWithChallenge` carries its challenge.
        let login = authorized_request(
            &AuthorizedRequest::new(Auth::LoginWithChallenge("ef".into())),
            &ctx(),
        );
        assert_eq!(login["items"]["auth"]["challenge"], "ef");
    }

    /// The unauthorized form, with an exact number of accounts and phone numbers.
    #[test]
    fn an_unauthorized_request_can_ask_for_exactly_n_and_for_phones() {
        let accounts = AccountsWanted {
            quantity: Quantity::Exactly(1),
            challenge: Some("ab".into()),
        };
        let data = PersonaDataWanted {
            name: true,
            emails: None,
            phones: Some(Quantity::Exactly(1)),
        };
        let r = unauthorized_request(Some(&accounts), Some(data), &ctx());
        assert_eq!(r["items"]["discriminator"], "unauthorizedRequest");
        assert_eq!(
            r["items"]["oneTimeAccounts"]["numberOfAccounts"]["quantifier"],
            "exactly"
        );
        assert_eq!(
            r["items"]["oneTimePersonaData"]["numberOfRequestedPhoneNumbers"]["quantifier"],
            "exactly"
        );
        // And it is the same account-proof request the wallet side already parses.
        let parsed = crate::parse_account_proof_request(&r).expect("parsed");
        assert_eq!(parsed.challenge_hex, "ab");
    }

    #[test]
    fn ongoing_accounts_and_phones_are_read_back() {
        let answer = json!({ "discriminator": "success", "items": {
            "ongoingAccounts": { "accounts": [ { "address": "account_tdx_2_1a", "label": "Main" }, { "address": "account_tdx_2_1b" } ] },
            "oneTimePersonaData": { "phoneNumbers": ["+34 600"] },
            "ongoingPersonaData": { "phoneNumbers": [{ "value": "+34 700" }, ""] }
        } });
        assert_eq!(
            extract_ongoing_accounts(&answer).expect("accounts"),
            vec![
                ("account_tdx_2_1a".to_string(), Some("Main".to_string())),
                ("account_tdx_2_1b".to_string(), None)
            ]
        );
        assert_eq!(
            extract_persona_phones(&answer),
            vec!["+34 600".to_string(), "+34 700".to_string()]
        );
        assert!(extract_ongoing_accounts(&json!({})).expect("none").is_empty());
    }
}
