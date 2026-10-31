//! radixdlt-connect-auto — talk to the Radix Wallet over iroh or WebRTC, whichever it can, with
//! nothing to choose.
//!
//! Radix Connect reaches the wallet on the phone over two transports: **WebRTC**, which every
//! Radix Wallet speaks, and **iroh** (QUIC), for wallets with it on. This crate uses both so that
//! the developer and the person using the app do not have to choose, and every request takes the
//! fastest way:
//!
//! - **Pairing** shows one QR that links over both at once. The wallet links over iroh when it has
//!   it on, otherwise over WebRTC; the [`Link`] records which.
//! - **Each request** over a link paired over iroh goes over iroh at once when its connection is
//!   up; otherwise it RACES both transports: the first one ready to carry it carries it, and only
//!   that one — the wallet never gets a request twice — and its answer is awaited over both. A
//!   request is never slower than WebRTC, and as fast as iroh when iroh is there.
//! - A process that stays up can [`keep_listening`](WalletConnector::keep_listening) on its links,
//!   so the wallet connects as soon as its app opens and requests go at once.
//! - **Removing a link** tells the wallet over every transport it may be on.
//!
//! The decisions of the race (which transport a pairing keeps, which one carries a request) are
//! plain state machines with no network, tested in every order of events.
//!
//! ```no_run
//! use std::time::Duration;
//! use radixdlt_connect_auto::{DappContext, PersonaRequest, WalletConnector};
//! # async fn demo(state: &radixdlt_connect_auto::LinkState) -> Result<(), radixdlt_connect_auto::Error> {
//! let wallet = WalletConnector::new();
//! let link = &state.all_links()[0];
//! let ctx = DappContext::new(2, "account_tdx_2_…", "https://example.org");
//! let answer = wallet
//!     .request_login(link, "<challenge hex>", &ctx, PersonaRequest::NAME, Duration::from_secs(120))
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! Only one transport? Use `radixdlt-connect-webrtc` or `radixdlt-connect-iroh` directly.

mod connector;
mod error;
mod race;
mod requests;

pub use connector::{Event, Paired, Transports, WalletConnector, IROH_GRACE};
pub use error::Error;

/// The transports, for settings the defaults do not cover.
pub use radixdlt_connect_iroh::mobile::{MobileError, MobileWalletConnector, Progress as IrohProgress};
pub use radixdlt_connect_types::{
    account_proof_request, account_proof_request_sharing, account_request, authorized_request, check_failure,
    extract_accounts, extract_login, extract_ongoing_accounts, extract_ownership_proofs,
    extract_persona_email, extract_persona_name, extract_persona_phones, extract_proofs,
    extract_signed_partial_transaction, extract_transaction_intent_hash, is_rejection_by_person,
    link_removal, linking, login_request, ownership_request, pre_authorization_request, transaction_request,
    unauthorized_request, AccountsWanted, Auth, AuthorizedRequest, DappContext, OwnershipWanted,
    PersonaDataWanted, PersonaRequest, Quantity, Transport, WalletInteractionError,
};
/// The links' state file (`connector.json`): the links, how each was paired, and the notices of
/// removed ones.
pub use radixdlt_connect_webrtc::state::{self, Identity, Link, LinkState, RetiredLink};
pub use radixdlt_connect_webrtc::{ConnectError, Connector as WebrtcConnector, Progress as WebrtcProgress};
