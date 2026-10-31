//! One error for both transports, saying how far the request got — which is what a caller decides
//! on: whether anything reached the phone, whether the person answered, whether to try again.

use std::fmt;

use radixdlt_connect_iroh::mobile::MobileError;
use radixdlt_connect_types::{Transport, WalletInteractionError};
use radixdlt_connect_webrtc::ConnectError;
use radixdlt_i18n::{tr, Lang};

/// Why a call to the wallet did not return its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The link's password is not usable (not hex, or not 32 bytes).
    InvalidLink(String),
    /// This end reached no relay nor signaling server: it has no network. Nothing was sent.
    NoNetwork {
        /// Why, over each transport tried.
        reasons: Vec<(Transport, String)>,
    },
    /// The wallet did not connect over any transport in time: its app is closed, the phone has no
    /// network, or it is not linked to this connector any more. Nothing was sent.
    WalletUnreachable {
        /// Why, over each transport tried.
        reasons: Vec<(Transport, String)>,
    },
    /// The request was sent, and the wallet never confirmed receiving it: it most likely did not.
    NotConfirmed {
        /// The transport it went over.
        transport: Transport,
        /// What that transport said.
        detail: String,
    },
    /// The wallet received the request and nobody answered in time: it may still be on the phone,
    /// so it must not be sent again.
    NoAnswer {
        /// The transport it went over.
        transport: Transport,
    },
    /// The wallet answered with a failure: the person rejected it, or the wallet cannot do what
    /// was asked (see [`Error::rejected_by_person`]).
    WalletRejected(String),
    /// The wallet removed this link (the person removed the connector in the wallet): forget it;
    /// pairing again makes a new one.
    LinkRemoved,
    /// Another conversation on this link did not end in time.
    LinkBusy,
    /// Pairing failed over every transport: why, over each.
    PairingFailed {
        /// Why, over each transport tried.
        reasons: Vec<(Transport, String)>,
    },
    /// Anything else, over this transport.
    Failed {
        /// The transport.
        transport: Transport,
        /// What it said.
        detail: String,
    },
}

/// What a transport's failure says about the phone, before the race combines them.
#[derive(Debug)]
pub(crate) enum Reach {
    /// This end has no network for that transport.
    NoNetwork(String),
    /// The wallet was not there.
    Absent(String),
    /// Anything that is already a final answer.
    Final(Error),
}

impl Error {
    /// Whether the PERSON refused, rather than the wallet being unable to answer what was asked.
    #[must_use]
    pub fn rejected_by_person(&self) -> bool {
        matches!(self, Error::WalletRejected(reason) if radixdlt_connect_types::is_rejection_by_person(reason))
    }

    /// Whether the request may be on the phone: it reached the wallet (or may have), so sending
    /// it again could ask the person twice.
    #[must_use]
    pub fn may_be_on_the_phone(&self) -> bool {
        matches!(self, Error::NoAnswer { .. })
    }

    /// The failures of the transports that never reached the wallet, combined.
    pub(crate) fn unreachable(reaches: Vec<(Transport, Reach)>) -> Self {
        let mut no_network = true;
        let mut reasons = Vec::new();
        for (transport, reach) in reaches {
            match reach {
                Reach::Final(error) => return error,
                Reach::NoNetwork(reason) => reasons.push((transport, reason)),
                Reach::Absent(reason) => {
                    no_network = false;
                    reasons.push((transport, reason));
                }
            }
        }
        if no_network && !reasons.is_empty() {
            Error::NoNetwork { reasons }
        } else {
            Error::WalletUnreachable { reasons }
        }
    }

    /// A WebRTC failure, as a final error once the request went over WebRTC.
    pub(crate) fn from_webrtc(error: ConnectError) -> Self {
        match Self::reach_webrtc(error) {
            Reach::Final(error) => error,
            reach @ (Reach::NoNetwork(_) | Reach::Absent(_)) => {
                Self::unreachable(vec![(Transport::Webrtc, reach)])
            }
        }
    }

    /// An iroh failure, as a final error once the request went over iroh.
    pub(crate) fn from_iroh(error: MobileError) -> Self {
        match Self::reach_iroh(error) {
            Reach::Final(error) => error,
            reach @ (Reach::NoNetwork(_) | Reach::Absent(_)) => {
                Self::unreachable(vec![(Transport::Iroh, reach)])
            }
        }
    }

    pub(crate) fn reach_webrtc(error: ConnectError) -> Reach {
        let transport = Transport::Webrtc;
        match error {
            ConnectError::Signaling(_) | ConnectError::SignalingClosed => Reach::NoNetwork(error.to_string()),
            ConnectError::ChannelTimeout => Reach::Absent(error.to_string()),
            ConnectError::ConfirmationTimeout => Reach::Final(Error::NotConfirmed {
                transport,
                detail: error.to_string(),
            }),
            ConnectError::ResponseTimeout => Reach::Final(Error::NoAnswer { transport }),
            ConnectError::WalletRejected(reason) => Reach::Final(Error::WalletRejected(reason)),
            ConnectError::LinkRemoved => Reach::Final(Error::LinkRemoved),
            ConnectError::LinkBusy => Reach::Final(Error::LinkBusy),
            ConnectError::WebRtc(_) | ConnectError::Crypto(_) | ConnectError::Protocol(_) => {
                Reach::Final(Error::Failed {
                    transport,
                    detail: error.to_string(),
                })
            }
        }
    }

    pub(crate) fn reach_iroh(error: MobileError) -> Reach {
        let transport = Transport::Iroh;
        match error {
            MobileError::WalletUnreachable { .. } => Reach::Absent(error.to_string()),
            // The endpoint could not start or reach its relay: this end's network.
            MobileError::Session(_) | MobileError::InvalidRelayUrl(_) => Reach::NoNetwork(error.to_string()),
            MobileError::ConfirmationTimeout { .. } => Reach::Final(Error::NotConfirmed {
                transport,
                detail: error.to_string(),
            }),
            MobileError::ResponseTimeout { .. } => Reach::Final(Error::NoAnswer { transport }),
            MobileError::WalletRejected(reason) => Reach::Final(Error::WalletRejected(reason)),
            MobileError::LinkRemoved => Reach::Final(Error::LinkRemoved),
            MobileError::InvalidPassword => Reach::Final(Error::InvalidLink(error.to_string())),
            other => Reach::Final(Error::Failed {
                transport,
                detail: other.to_string(),
            }),
        }
    }
}

impl From<WalletInteractionError> for Error {
    fn from(error: WalletInteractionError) -> Self {
        match error {
            WalletInteractionError::WalletRejected(reason) => Error::WalletRejected(reason),
            WalletInteractionError::Protocol(detail) => Error::Failed {
                transport: Transport::Webrtc,
                detail,
            },
        }
    }
}

fn each(reasons: &[(Transport, String)]) -> String {
    reasons
        .iter()
        .map(|(transport, reason)| format!("{transport}: {reason}"))
        .collect::<Vec<_>>()
        .join("; ")
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lang = Lang::detect();
        let text = match self {
            Error::InvalidLink(detail) => tr!(
                lang,
                format!("the link is not usable: {detail}"),
                format!("el enlace no se puede usar: {detail}")
            ),
            Error::NoNetwork { reasons } => tr!(
                lang,
                format!("this computer could not reach the network ({})", each(reasons)),
                format!("este equipo no pudo conectar con la red ({})", each(reasons))
            ),
            Error::WalletUnreachable { reasons } => tr!(
                lang,
                format!(
                    "the wallet did not connect: is the Radix Wallet open? ({})",
                    each(reasons)
                ),
                format!(
                    "la wallet no se conectó: ¿está abierta la Radix Wallet? ({})",
                    each(reasons)
                )
            ),
            Error::NotConfirmed { transport, detail } => tr!(
                lang,
                format!("the wallet never confirmed receiving it over {transport}: {detail}"),
                format!("la wallet no confirmó haberlo recibido por {transport}: {detail}")
            ),
            Error::NoAnswer { transport } => tr!(
                lang,
                format!("the wallet received it over {transport}, and nobody answered in time"),
                format!("la wallet lo recibió por {transport} y nadie respondió a tiempo")
            ),
            Error::WalletRejected(reason) => tr!(
                lang,
                format!("the wallet rejected the request: {reason}"),
                format!("la wallet rechazó la solicitud: {reason}")
            ),
            Error::LinkRemoved => tr!(
                lang,
                "the wallet removed this connector: pair it again to use it".to_string(),
                "la wallet eliminó este conector: vuelve a emparejarla para usarla".to_string()
            ),
            Error::LinkBusy => tr!(
                lang,
                "another request is still in flight on this link".to_string(),
                "ya hay otra petición en curso en este enlace".to_string()
            ),
            Error::PairingFailed { reasons } => tr!(
                lang,
                format!("the wallet did not link ({})", each(reasons)),
                format!("la wallet no se enlazó ({})", each(reasons))
            ),
            Error::Failed { transport, detail } => tr!(
                lang,
                format!("{transport}: {detail}"),
                format!("{transport}: {detail}")
            ),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_reached_means_no_network_only_when_no_transport_got_out() {
        let absent = Error::unreachable(vec![
            (Transport::Iroh, Reach::Absent("no wallet".into())),
            (Transport::Webrtc, Reach::NoNetwork("no signaling".into())),
        ]);
        assert!(
            matches!(absent, Error::WalletUnreachable { .. }),
            "one got out: the wallet was missing"
        );
        let offline = Error::unreachable(vec![
            (Transport::Iroh, Reach::NoNetwork("no relay".into())),
            (Transport::Webrtc, Reach::NoNetwork("no signaling".into())),
        ]);
        assert!(matches!(offline, Error::NoNetwork { .. }));
    }

    #[test]
    fn each_transport_says_how_far_it_got_the_same_way() {
        assert_eq!(
            Error::from_webrtc(ConnectError::ResponseTimeout),
            Error::NoAnswer {
                transport: Transport::Webrtc
            }
        );
        assert_eq!(Error::from_webrtc(ConnectError::LinkRemoved), Error::LinkRemoved);
        assert_eq!(Error::from_iroh(MobileError::LinkRemoved), Error::LinkRemoved);
        assert!(
            Error::from_webrtc(ConnectError::WalletRejected("rejectedByUser".into())).rejected_by_person()
        );
        assert!(!Error::from_iroh(MobileError::WalletRejected("invalidPersona".into())).rejected_by_person());
        assert!(Error::NoAnswer {
            transport: Transport::Iroh
        }
        .may_be_on_the_phone());
    }
}
