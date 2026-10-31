//! The decisions of a race between the two transports, as plain state machines: no network, no
//! clock, no task. [`crate::WalletConnector`] feeds them what happens and does what they say, and
//! the tests here drive them through every order of events.

use radixdlt_connect_types::Transport;

/// What a pairing over several transports at once does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairStep {
    /// Keep waiting; `start_grace` when WebRTC linked and iroh gets its grace period from now.
    Wait { start_grace: bool },
    /// The link keeps this transport: the wallet with this public key linked over it.
    Linked(Transport, String),
    /// Every transport failed: why, in the order they failed.
    Failed(Vec<(Transport, String)>),
}

/// Which transport a pairing keeps, iroh preferred:
///
/// - iroh links: iroh, at once;
/// - WebRTC links while iroh is still trying: iroh gets a grace period — linking within it, iroh
///   is kept; once it is over (or iroh failed), WebRTC;
/// - a transport that fails leaves the others; when all failed, each one's reason.
///
/// One QR serves both (one password): the wallet links over iroh when it has iroh on, otherwise
/// over WebRTC. Preferring iroh is preferring what the wallet can do best.
#[derive(Debug)]
pub(crate) struct PairingRace {
    trying: Vec<Transport>,
    failures: Vec<(Transport, String)>,
    /// The wallet that linked over WebRTC, waiting for iroh's grace period.
    over_webrtc: Option<String>,
}

impl PairingRace {
    pub(crate) fn new(transports: &[Transport]) -> Self {
        Self {
            trying: transports.to_vec(),
            failures: Vec::new(),
            over_webrtc: None,
        }
    }

    pub(crate) fn linked(&mut self, transport: Transport, wallet_public_key: String) -> PairStep {
        self.trying.retain(|t| *t != transport);
        match transport {
            Transport::Iroh => PairStep::Linked(Transport::Iroh, wallet_public_key),
            Transport::Webrtc if self.trying.contains(&Transport::Iroh) => {
                self.over_webrtc = Some(wallet_public_key);
                PairStep::Wait { start_grace: true }
            }
            Transport::Webrtc => PairStep::Linked(Transport::Webrtc, wallet_public_key),
        }
    }

    pub(crate) fn failed(&mut self, transport: Transport, reason: String) -> PairStep {
        self.trying.retain(|t| *t != transport);
        self.failures.push((transport, reason));
        if !self.trying.contains(&Transport::Iroh) {
            if let Some(wallet_public_key) = self.over_webrtc.take() {
                return PairStep::Linked(Transport::Webrtc, wallet_public_key);
            }
        }
        if self.trying.is_empty() {
            PairStep::Failed(std::mem::take(&mut self.failures))
        } else {
            PairStep::Wait { start_grace: false }
        }
    }

    pub(crate) fn grace_over(&mut self) -> PairStep {
        match self.over_webrtc.take() {
            Some(wallet_public_key) => PairStep::Linked(Transport::Webrtc, wallet_public_key),
            None => PairStep::Wait { start_grace: false },
        }
    }
}

/// Which transport carries one request.
///
/// The first transport READY to carry it — iroh connected to the wallet, or a WebRTC channel
/// opened to it — carries it, and only that one: the other is dropped before it sent anything, so
/// the wallet never gets the request twice. iroh is asked first when both are ready at once. A
/// transport that fails before being chosen leaves the other; when none is left, the request
/// failed over each.
#[derive(Debug, Default)]
pub(crate) struct CarrierRace {
    carrier: Option<Transport>,
    failed: Vec<Transport>,
}

impl CarrierRace {
    /// `transport` is ready: whether it carries the request (it is the first).
    pub(crate) fn ready(&mut self, transport: Transport) -> bool {
        if self.carrier.is_some() || self.failed.contains(&transport) {
            return false;
        }
        self.carrier = Some(transport);
        true
    }

    /// `transport` failed before carrying anything: whether any other is still trying.
    pub(crate) fn failed(&mut self, transport: Transport, racing: &[Transport]) -> bool {
        if !self.failed.contains(&transport) {
            self.failed.push(transport);
        }
        racing.iter().any(|t| !self.failed.contains(t))
    }

    pub(crate) fn carrier(&self) -> Option<Transport> {
        self.carrier
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Transport::{Iroh, Webrtc};

    const BOTH: [Transport; 2] = [Iroh, Webrtc];

    #[test]
    fn iroh_linking_first_is_kept_at_once() {
        let mut race = PairingRace::new(&BOTH);
        assert_eq!(race.linked(Iroh, "w".into()), PairStep::Linked(Iroh, "w".into()));
    }

    #[test]
    fn webrtc_linking_first_waits_for_irohs_grace_and_iroh_within_it_wins() {
        let mut race = PairingRace::new(&BOTH);
        assert_eq!(
            race.linked(Webrtc, "w".into()),
            PairStep::Wait { start_grace: true }
        );
        assert_eq!(race.linked(Iroh, "w".into()), PairStep::Linked(Iroh, "w".into()));
    }

    #[test]
    fn webrtc_is_kept_once_the_grace_is_over_or_iroh_failed() {
        let mut race = PairingRace::new(&BOTH);
        race.linked(Webrtc, "w".into());
        assert_eq!(race.grace_over(), PairStep::Linked(Webrtc, "w".into()));

        let mut race = PairingRace::new(&BOTH);
        race.linked(Webrtc, "w".into());
        assert_eq!(
            race.failed(Iroh, "no iroh".into()),
            PairStep::Linked(Webrtc, "w".into())
        );
    }

    #[test]
    fn a_failure_leaves_the_other_and_all_failing_names_each() {
        let mut race = PairingRace::new(&BOTH);
        assert_eq!(
            race.failed(Iroh, "a".into()),
            PairStep::Wait { start_grace: false }
        );
        assert_eq!(
            race.failed(Webrtc, "b".into()),
            PairStep::Failed(vec![(Iroh, "a".into()), (Webrtc, "b".into())])
        );
        // A grace period with nothing waiting for it decides nothing.
        assert_eq!(
            PairingRace::new(&BOTH).grace_over(),
            PairStep::Wait { start_grace: false }
        );
    }

    #[test]
    fn one_transport_alone_is_kept_when_it_links() {
        let mut race = PairingRace::new(&[Webrtc]);
        assert_eq!(
            race.linked(Webrtc, "w".into()),
            PairStep::Linked(Webrtc, "w".into())
        );
    }

    #[test]
    fn the_first_ready_transport_carries_the_request_and_no_other_ever_does() {
        let mut race = CarrierRace::default();
        assert!(race.ready(Webrtc));
        assert!(
            !race.ready(Iroh),
            "never a second carrier: the wallet must not get it twice"
        );
        assert_eq!(race.carrier(), Some(Webrtc));
    }

    #[test]
    fn a_transport_that_failed_cannot_carry_and_the_last_failure_ends_it() {
        let mut race = CarrierRace::default();
        assert!(race.failed(Iroh, &BOTH), "webrtc still trying");
        assert!(!race.ready(Iroh));
        assert!(!race.failed(Webrtc, &BOTH), "none left");
        assert_eq!(race.carrier(), None);
    }
}
