//! [`WalletConnector::exchange_first`]: one request, to whichever of several wallets connects
//! first — the phone the person opens — over whichever transport reaches it first.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use radixdlt_connect_iroh::mobile::{MobileError, MobileWalletConnector, BACKGROUND_PATIENCE};
use radixdlt_connect_types::Transport;
use radixdlt_connect_webrtc::state::Link;
use radixdlt_connect_webrtc::ConnectError;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::connector::{over_iroh, Event, WalletConnector};
use crate::error::{Error, Reach};

/// One link over one transport, racing the others.
type Candidate = (usize, Transport);

/// What a candidate ended with.
enum Outcome {
    Answer(Value),
    /// Another was chosen; this one sent nothing (or stopped listening).
    Lost,
    Webrtc(ConnectError),
    Iroh(MobileError),
    /// The request went over iroh and failed there: the request's outcome.
    Carried(Error),
}

/// A link ready to race: its index, password, iroh connector (when it races over iroh) and the
/// transports it races over.
type Prepared = (
    usize,
    Vec<u8>,
    Option<Arc<MobileWalletConnector>>,
    &'static [Transport],
);

/// What [`WalletConnector::exchange_first`] returns: which link answered, and the answer.
pub type FirstAnswer<'a> = Pin<Box<dyn Future<Output = Result<(usize, Value), Error>> + Send + 'a>>;

type Running<'a> = Pin<Box<dyn Future<Output = (Candidate, Outcome)> + Send + 'a>>;

impl WalletConnector {
    /// Sends `interaction` to whichever of `links`' wallets connects FIRST — the phone the person
    /// opens — and to that one only: a wallet cannot withdraw a request it was shown, so the
    /// others are never sent it, and their connections are dropped as soon as one is chosen. A
    /// wallet already connected over iroh is the first. Each link races its transports as
    /// [`exchange`](Self::exchange) does, and the chosen one's answer is awaited over both (a
    /// wallet may answer over the other). Returns which link (its index in `links`) and the answer.
    ///
    /// For links of ONE person: whoever answers, answers as themselves. A request that names a
    /// persona goes to the links that have it ([`links_with_persona`](Self::links_with_persona)).
    ///
    /// # Errors
    /// The chosen link's error, once one was chosen; before that, how far each got
    /// ([`Error::WalletUnreachable`] when none connected in time, [`Error::NoNetwork`] without a network).
    ///
    /// The future is boxed: it holds every link's race, a type deep enough that a caller checking
    /// it is `Send` would run out of recursion.
    pub fn exchange_first<'a>(
        &'a self,
        links: &'a [Link],
        interaction: &'a Value,
        timeout: Duration,
        on: &'a mut (dyn FnMut(usize, Event) + Send),
    ) -> FirstAnswer<'a> {
        Box::pin(self.exchange_first_unboxed(links, interaction, timeout, on))
    }

    async fn exchange_first_unboxed(
        &self,
        links: &[Link],
        interaction: &Value,
        timeout: Duration,
        on: &mut (dyn FnMut(usize, Event) + Send),
    ) -> Result<(usize, Value), Error> {
        let (chosen, result) = match links {
            [] => return Err(Error::InvalidLink("no link to send the request over".to_string())),
            [only] => (
                0,
                self.exchange_over(only, interaction, timeout, &mut |event| on(0, event))
                    .await,
            ),
            // Connected over iroh and in the foreground: the phone in the person's hand.
            _ => match links
                .iter()
                .position(|link| self.connected_over_iroh(link) && !self.iroh_in_background(link))
            {
                Some(index) => {
                    let link = links.get(index).unwrap_or(&links[0]);
                    (
                        index,
                        self.exchange_over(link, interaction, timeout, &mut |event| on(index, event))
                            .await,
                    )
                }
                None => Box::pin(self.first_of(links, interaction, timeout, on)).await?,
            },
        };
        if let Some(link) = links.get(chosen) {
            self.after_exchange(link, &result).await;
        }
        result.map(|answer| (chosen, answer))
    }

    /// The race itself: every link over each of its transports, until one is chosen and answers.
    async fn first_of(
        &self,
        links: &[Link],
        interaction: &Value,
        timeout: Duration,
        on: &mut (dyn FnMut(usize, Event) + Send),
    ) -> Result<(usize, Result<Value, Error>), Error> {
        let deadline = Instant::now() + timeout;
        let interaction_id = interaction
            .get("interactionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut prepared: Vec<Prepared> = Vec::new();
        for (index, link) in links.iter().enumerate() {
            let password = link
                .password_bytes()
                .map_err(|e| Error::InvalidLink(e.to_string()))?;
            let transports = self.transports.for_link(link.paired_over());
            let iroh = if transports.contains(&Transport::Iroh) {
                Some(self.iroh_for(link, &password)?)
            } else {
                None
            };
            prepared.push((index, password, iroh, transports));
        }

        let (choice, _) = watch::channel(None::<Candidate>);
        let (events, mut evented) = mpsc::unbounded_channel::<(usize, Event)>();
        let mut running: FuturesUnordered<Running<'_>> = FuturesUnordered::new();
        for (index, password, iroh, transports) in &prepared {
            for transport in *transports {
                let candidate = (*index, *transport);
                let events = events.clone();
                let choice = &choice;
                running.push(match (transport, iroh) {
                    (Transport::Iroh, Some(iroh)) => Box::pin(self.iroh_candidate(
                        candidate,
                        iroh,
                        password,
                        interaction,
                        &interaction_id,
                        deadline,
                        choice,
                        events,
                    )),
                    _ => Box::pin(self.webrtc_candidate(
                        candidate,
                        password,
                        interaction,
                        timeout,
                        choice,
                        events,
                    )),
                });
            }
        }
        drop(events);

        let mut reaches: Vec<(Transport, Reach)> = Vec::new();
        let mut chosen_failed: Option<Error> = None;
        loop {
            tokio::select! {
                next = running.next() => {
                    let Some((candidate, outcome)) = next else { break };
                    let chosen = *choice.borrow();
                    let error = match outcome {
                        Outcome::Answer(answer) => return Ok((candidate.0, Ok(answer))),
                        Outcome::Lost => continue,
                        Outcome::Webrtc(error) if chosen == Some(candidate) => Error::from_webrtc(error),
                        Outcome::Iroh(error) if chosen == Some(candidate) => Error::from_iroh(error),
                        Outcome::Carried(error) => error,
                        Outcome::Webrtc(error) => {
                            reaches.push((Transport::Webrtc, Error::reach_webrtc(error)));
                            continue;
                        }
                        Outcome::Iroh(error) => {
                            reaches.push((Transport::Iroh, Error::reach_iroh(error)));
                            continue;
                        }
                    };
                    // The chosen one failed: unless its link's other transport is still
                    // listening for the answer, that is the request's outcome.
                    chosen_failed = Some(error);
                }
                Some((index, event)) = evented.recv() => on(index, event),
            }
        }
        // Every candidate is done (they borrow `choice`): what was chosen, read once.
        drop(running);
        let chosen = *choice.borrow();
        match (chosen, chosen_failed) {
            (Some((index, _)), Some(error)) => Ok((index, Err(error))),
            (Some((index, _)), None) => Ok((index, Err(Error::unreachable(reaches)))),
            (None, _) => Err(Error::unreachable(reaches)),
        }
    }

    /// WebRTC: opens the channel, and sends only if this candidate is chosen then. Stops at once
    /// when anything else is chosen (its link's iroh carries the request then, or another link).
    async fn webrtc_candidate(
        &self,
        candidate: Candidate,
        password: &[u8],
        interaction: &Value,
        timeout: Duration,
        choice: &watch::Sender<Option<Candidate>>,
        events: mpsc::UnboundedSender<(usize, Event)>,
    ) -> (Candidate, Outcome) {
        let mut lost = choice.subscribe();
        let index = candidate.0;
        let report = events.clone();
        let mut forward = move |step| {
            let _ = report.send((index, Event::WebrtcStep(step)));
        };
        let claim = || {
            let won = claim(choice, candidate);
            if won {
                let _ = events.send((index, Event::Carrier(Transport::Webrtc)));
            }
            won
        };
        let conversation = self
            .webrtc
            .exchange_if(password, interaction, timeout, &mut forward, &claim);
        let other = lost.wait_for(|chosen| chosen.is_some_and(|chosen| chosen != candidate));
        let outcome = tokio::select! {
            result = conversation => match result {
                Ok(Some(answer)) => Outcome::Answer(answer),
                Ok(None) => Outcome::Lost,
                Err(error) => Outcome::Webrtc(error),
            },
            _ = other => Outcome::Lost,
        };
        (candidate, outcome)
    }

    /// iroh: waits for the wallet to connect, sending nothing; chosen then, it carries the
    /// request. When its link's WebRTC carries it instead, it listens for the answer, which the
    /// wallet may send over iroh. Stops at once when another link is chosen.
    #[allow(clippy::too_many_arguments)]
    async fn iroh_candidate(
        &self,
        candidate: Candidate,
        iroh: &MobileWalletConnector,
        password: &[u8],
        interaction: &Value,
        interaction_id: &str,
        deadline: Instant,
        choice: &watch::Sender<Option<Candidate>>,
        events: mpsc::UnboundedSender<(usize, Event)>,
    ) -> (Candidate, Outcome) {
        let started = Instant::now();
        let mut lost = choice.subscribe();
        let index = candidate.0;
        let other_link = lost.wait_for(|chosen| chosen.is_some_and(|chosen| chosen.0 != index));
        let work = async {
            let left = || deadline.saturating_duration_since(Instant::now());
            if let Err(error) = iroh.probe(password, left()).await {
                return Outcome::Iroh(error);
            }
            // Connected with its app in the background (left a moment ago): another the person
            // opens goes first; this one only once it is back, or nobody opened another in time.
            let patience = started + BACKGROUND_PATIENCE;
            while iroh.in_background(password) && Instant::now() < patience {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if claim(choice, candidate) {
                let _ = events.send((index, Event::Carrier(Transport::Iroh)));
                let mut report = |event| {
                    let _ = events.send((index, event));
                };
                return match over_iroh(iroh, password, interaction, left(), &mut report).await {
                    Ok(answer) => Outcome::Answer(answer),
                    Err(error) => Outcome::Carried(error),
                };
            }
            // Its link's WebRTC carries it: the answer may come back over iroh.
            if *choice.borrow() == Some((index, Transport::Webrtc)) {
                let mut ignore = |_| {};
                if let Ok(answer) = iroh
                    .await_response(password, interaction_id, left(), &mut ignore)
                    .await
                {
                    return Outcome::Answer(answer);
                }
            }
            Outcome::Lost
        };
        let outcome = tokio::select! {
            outcome = work => outcome,
            _ = other_link => Outcome::Lost,
        };
        (candidate, outcome)
    }
}

/// Makes `candidate` the chosen one, unless another already is: whether it is. Atomic, so two
/// wallets connecting at the same moment cannot both be sent the request.
fn claim(choice: &watch::Sender<Option<Candidate>>, candidate: Candidate) -> bool {
    choice.send_if_modified(|chosen| {
        if chosen.is_none() {
            *chosen = Some(candidate);
            true
        } else {
            false
        }
    });
    *choice.borrow() == Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_candidate_is_chosen_and_its_link_s_other_transport_is_not() {
        let (choice, _) = watch::channel(None::<Candidate>);
        assert!(claim(&choice, (1, Transport::Iroh)));
        assert!(
            !claim(&choice, (1, Transport::Webrtc)),
            "the same wallet is not sent it twice"
        );
        assert!(!claim(&choice, (0, Transport::Iroh)));
        assert_eq!(*choice.borrow(), Some((1, Transport::Iroh)));
    }
}
