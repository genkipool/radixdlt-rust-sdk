//! Errors of the mobile-wallet connector. `Display` is localized to the system language.

use radix_connect_iroh::Diagnostics;
use radixdlt_connect_types::linking::LinkingError;
use radixdlt_connect_types::WalletInteractionError;
use radixdlt_i18n::{tr, Lang};

/// Errors pairing with, or talking to, the Radix mobile wallet over iroh.
///
/// What each failure says about the REQUEST matters more than its wording, because it decides
/// whether sending it again is safe:
///
/// | Variant | The request… |
/// |---|---|
/// | [`WalletUnreachable`](Self::WalletUnreachable), [`MessageTooLarge`](Self::MessageTooLarge), [`MessageRefused`](Self::MessageRefused), [`InvalidPassword`](Self::InvalidPassword), [`InvalidRelayUrl`](Self::InvalidRelayUrl), [`Session`](Self::Session) | never reached the wallet: sending it again is safe |
/// | [`ConfirmationTimeout`](Self::ConfirmationTimeout) | may or may not be on the phone: wait with [`await_response`](super::MobileWalletConnector::await_response) rather than send it again |
/// | [`ResponseTimeout`](Self::ResponseTimeout) | is on the phone, unanswered: its answer can still be collected with `await_response` |
/// | [`WalletRejected`](Self::WalletRejected) | was answered: with a refusal, or that the wallet cannot do it |
///
/// The timeouts carry a snapshot of the link's connectivity at the moment they happened
/// ([`diagnostics`](Self::diagnostics)), shown after the message: `relay … (NOT connected)` says
/// this server cannot reach its relay, a wallet that never connected with the relay up says the
/// phone is not there (app closed, iroh off, another relay choice), and `last_error` says what failed
/// last. It holds no secrets: endpoint ids are public keys, and no message contents.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MobileError {
    /// The link password is not 32 bytes.
    InvalidPassword,
    /// The relay URL is not usable (see [`radix_connect_iroh::SessionConfig::new`]).
    InvalidRelayUrl(String),
    /// The link's iroh endpoint could not be started.
    Session(String),
    /// The linking cryptography failed: an unusable connector identity, or a wallet whose
    /// linking signature does not verify.
    Crypto(String),
    /// A message did not have the expected shape.
    Protocol(String),
    /// The wallet did not connect in time: its app is closed, iroh is not enabled for this
    /// link, it made another relay choice (public vs. a custom relay, or another custom one), or
    /// it is not linked to this connector. Nothing was sent.
    WalletUnreachable {
        /// The link's connectivity when the wait ended.
        diagnostics: Box<Diagnostics>,
    },
    /// The wallet took the request but did not answer in time (the person has not decided yet).
    ResponseTimeout {
        /// The link's connectivity when the wait ended.
        diagnostics: Box<Diagnostics>,
    },
    /// The wallet never confirmed the request, and it may or may not have received it.
    ConfirmationTimeout {
        /// The link's connectivity when the wait ended.
        diagnostics: Box<Diagnostics>,
    },
    /// The wallet answered with a failure: the person rejected the request, or the wallet
    /// cannot do what was asked (see [`rejected_by_person`](Self::rejected_by_person)).
    WalletRejected(String),
    /// The request is larger than the session's maximum message size. Nothing was sent.
    MessageTooLarge {
        /// Size of the request in bytes.
        size: usize,
        /// The configured maximum in bytes.
        max: usize,
    },
    /// The wallet refused the message (too large or malformed for it). It was not delivered.
    MessageRefused,
    /// The link was closed ([`close_link`](super::MobileWalletConnector::close_link) or
    /// [`close`](super::MobileWalletConnector::close)) while the call was in progress.
    Closed,
    /// The wallet removed this link (the person removed the connector in the wallet): it will
    /// not answer over it again. Forget the link; pairing again makes a new one.
    LinkRemoved,
}

impl MobileError {
    /// Whether the wallet answered that the PERSON refused, rather than that it cannot answer
    /// what was asked (see [`radixdlt_connect_types::is_rejection_by_person`]).
    #[must_use]
    pub fn rejected_by_person(&self) -> bool {
        matches!(self, MobileError::WalletRejected(reason) if radixdlt_connect_types::is_rejection_by_person(reason))
    }

    /// The link's connectivity when this error happened, for the errors that carry it (the
    /// timeouts). Its `Display` is one line for logs.
    #[must_use]
    pub fn diagnostics(&self) -> Option<&Diagnostics> {
        match self {
            MobileError::WalletUnreachable { diagnostics }
            | MobileError::ResponseTimeout { diagnostics }
            | MobileError::ConfirmationTimeout { diagnostics } => Some(diagnostics),
            _ => None,
        }
    }
}

impl std::fmt::Display for MobileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lang = Lang::detect();
        let msg = match self {
            MobileError::InvalidPassword => tr!(
                lang,
                "the link password must be 32 bytes".to_string(),
                "la contraseña del enlace debe tener 32 bytes".to_string()
            ),
            MobileError::InvalidRelayUrl(e) => tr!(
                lang,
                format!("invalid iroh relay URL: {e}"),
                format!("URL de relé iroh no válida: {e}")
            ),
            MobileError::Session(e) => tr!(
                lang,
                format!("could not start the iroh session: {e}"),
                format!("no se pudo iniciar la sesión iroh: {e}")
            ),
            MobileError::Crypto(e) => tr!(
                lang,
                format!("cryptography error: {e}"),
                format!("error de criptografía: {e}")
            ),
            MobileError::Protocol(e) => tr!(
                lang,
                format!("protocol error: {e}"),
                format!("error de protocolo: {e}")
            ),
            MobileError::WalletUnreachable { diagnostics } => tr!(
                lang,
                format!(
                    "the wallet did not connect over iroh in time (is the app open, with iroh enabled \
                     and the same relays?) — diagnostics: {diagnostics}"
                ),
                format!(
                    "la wallet no se conectó por iroh a tiempo (¿está la app abierta, con iroh activado \
                     y los mismos relés?) — diagnóstico: {diagnostics}"
                )
            ),
            MobileError::ResponseTimeout { diagnostics } => tr!(
                lang,
                format!("timed out waiting for the wallet's response — diagnostics: {diagnostics}"),
                format!(
                    "se agotó el tiempo esperando la respuesta de la wallet — diagnóstico: {diagnostics}"
                )
            ),
            MobileError::ConfirmationTimeout { diagnostics } => tr!(
                lang,
                format!(
                    "the wallet never confirmed the request; it may or may not have received it \
                     — diagnostics: {diagnostics}"
                ),
                format!(
                    "la wallet nunca confirmó la solicitud; puede que la haya recibido o no \
                     — diagnóstico: {diagnostics}"
                )
            ),
            MobileError::WalletRejected(e) => tr!(
                lang,
                format!("the wallet rejected/cancelled the request: {e}"),
                format!("la wallet rechazó/canceló la solicitud: {e}")
            ),
            MobileError::MessageTooLarge { size, max } => tr!(
                lang,
                format!("the request is {size} bytes, the maximum is {max}"),
                format!("la solicitud ocupa {size} bytes y el máximo es {max}")
            ),
            MobileError::MessageRefused => tr!(
                lang,
                "the wallet refused the message".to_string(),
                "la wallet rechazó el mensaje".to_string()
            ),
            MobileError::Closed => tr!(
                lang,
                "the link was closed".to_string(),
                "el enlace se cerró".to_string()
            ),
            MobileError::LinkRemoved => tr!(
                lang,
                "the wallet removed this connector: pair it again to use it".to_string(),
                "la wallet eliminó este conector: vuelve a emparejarla para usarla".to_string()
            ),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for MobileError {}

impl From<WalletInteractionError> for MobileError {
    fn from(e: WalletInteractionError) -> Self {
        match e {
            WalletInteractionError::WalletRejected(s) => MobileError::WalletRejected(s),
            WalletInteractionError::Protocol(s) => MobileError::Protocol(s),
        }
    }
}

impl From<LinkingError> for MobileError {
    fn from(e: LinkingError) -> Self {
        match e {
            LinkingError::InvalidPassword => MobileError::InvalidPassword,
            LinkingError::Malformed(_) => MobileError::Protocol(e.to_string()),
            other => MobileError::Crypto(other.to_string()),
        }
    }
}

impl From<radix_connect_iroh::Error> for MobileError {
    fn from(e: radix_connect_iroh::Error) -> Self {
        match e {
            radix_connect_iroh::Error::InvalidPassword => MobileError::InvalidPassword,
            radix_connect_iroh::Error::InvalidRelayUrl(reason) => MobileError::InvalidRelayUrl(reason),
            other => MobileError::Session(other.to_string()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use radix_connect_iroh::{ConnectionPassword, ConnectionState, Role};

    fn snapshot(password: &ConnectionPassword) -> Box<Diagnostics> {
        Box::new(Diagnostics {
            role: Role::Connector,
            local_id: password.endpoint_id(Role::Connector),
            peer_id: password.endpoint_id(Role::Wallet),
            relay_url: Some("http://127.0.0.1:3340".parse().unwrap()),
            public_relays: false,
            relay_connected: false,
            relay_error: Some("connection refused".to_string()),
            state: ConnectionState::Disconnected,
            path: None,
            rtt: None,
            failed_attempts: 2,
            last_error: Some("incoming handshake timed out".to_string()),
            connected_for: None,
            key_collision: false,
        })
    }

    /// Someone reading "timed out" must also read WHY it may have: the relay was down, the
    /// wallet never came, what failed last. And never anything that opens the link.
    #[test]
    fn timeouts_say_what_the_link_looked_like_and_nothing_secret() {
        let password_bytes = [0x5a_u8; 32];
        let password = ConnectionPassword::from_bytes(password_bytes);
        for error in [
            MobileError::WalletUnreachable {
                diagnostics: snapshot(&password),
            },
            MobileError::ResponseTimeout {
                diagnostics: snapshot(&password),
            },
            MobileError::ConfirmationTimeout {
                diagnostics: snapshot(&password),
            },
        ] {
            let shown = error.to_string();
            let line = error.diagnostics().unwrap().to_string();
            assert!(shown.ends_with(&line), "{shown}");
            // The protocol library never shows IP addresses, not even a relay's (0.1.1).
            assert!(
                shown.contains("relay=http://<ip>:3340/ (NOT connected)"),
                "{shown}"
            );
            assert!(!shown.contains("127.0.0.1"), "{shown}");
            assert!(shown.contains("failed_attempts=2"), "{shown}");
            for text in [shown, format!("{error:?}")] {
                assert!(!text.contains(&hex::encode(password_bytes)), "{text}");
            }
        }
        assert!(MobileError::Closed.diagnostics().is_none());
    }

    #[test]
    fn only_the_person_rejecting_is_a_refusal() {
        assert!(MobileError::WalletRejected("rejectedByUser".into()).rejected_by_person());
        assert!(!MobileError::WalletRejected("invalidPersona".into()).rejected_by_person());
        assert!(!MobileError::Closed.rejected_by_person());
    }

    #[test]
    fn library_errors_keep_their_meaning() {
        assert_eq!(
            MobileError::from(radix_connect_iroh::Error::InvalidPassword),
            MobileError::InvalidPassword
        );
        assert!(matches!(
            MobileError::from(radix_connect_iroh::Error::InvalidRelayUrl("x".into())),
            MobileError::InvalidRelayUrl(_)
        ));
        assert!(matches!(
            MobileError::from(LinkingError::InvalidSignature),
            MobileError::Crypto(_)
        ));
        assert!(matches!(
            MobileError::from(WalletInteractionError::WalletRejected("rejectedByUser".into())),
            MobileError::WalletRejected(_)
        ));
    }
}
