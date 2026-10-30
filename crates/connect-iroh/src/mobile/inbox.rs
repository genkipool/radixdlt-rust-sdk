//! Matching the wallet's messages with the requests waiting for them.
//!
//! The session confirms a message to the wallet as soon as the link's reader takes it, and the
//! reader takes every message at once (it never blocks on a slow consumer). So an answer that
//! arrives while nobody waits for it — the request timed out here and the person approved it
//! later — is already confirmed: dropping it would lose it for good, the wallet will not send it
//! again. It is kept instead, bounded in number, size and age, for
//! [`await_response`](super::MobileWalletConnector::await_response) (or a repeated
//! [`exchange`](super::MobileWalletConnector::exchange) of the same interaction) to collect.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// Unclaimed answers kept per link, at most.
pub(super) const UNCLAIMED_MAX: usize = 32;
/// Bytes of unclaimed answers kept per link, at most (the default maximum message size).
pub(super) const UNCLAIMED_MAX_BYTES: usize = 8 * 1024 * 1024;
/// How long an unclaimed answer is kept.
pub(super) const UNCLAIMED_TTL: Duration = Duration::from_secs(30 * 60);
/// Interactions remembered as asked for on a link, to tell a late answer from an unknown one.
const ASKED_MAX: usize = 256;

/// The `interactionId` a message answers, if it carries a non-empty one.
pub(super) fn interaction_id(message: &Value) -> Option<&str> {
    message
        .get("interactionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// What registering for an answer gave.
#[derive(Debug)]
pub(super) enum Registration {
    /// The answer had already arrived.
    Answered(Value),
    /// The answer will arrive on `answer`. `token` identifies this registration;
    /// `others_in_flight` is how many other interactions wait on the link.
    Waiting {
        token: u64,
        answer: oneshot::Receiver<Value>,
        others_in_flight: usize,
    },
}

/// Why an answer nobody waited for was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kept {
    /// Its request was asked for here and stopped waiting (timed out, dropped).
    Late,
    /// No request on this link asked for it (since the session started).
    Unknown,
    /// An answer to the same interaction was already kept; this one replaces it.
    Duplicate,
}

impl Kept {
    pub(super) fn label(self) -> &'static str {
        match self {
            Kept::Late => "late",
            Kept::Unknown => "unknown",
            Kept::Duplicate => "duplicate",
        }
    }
}

/// Why a registration was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refused {
    /// Someone already waits for this interaction on this link.
    AlreadyWaiting,
    /// The link has ended.
    Closed,
}

#[derive(Debug)]
struct Unclaimed {
    id: String,
    answer: Value,
    size: usize,
    at: Instant,
}

/// One link's waiters and unclaimed answers.
#[derive(Debug, Default)]
pub(super) struct Inbox {
    waiting: HashMap<String, (u64, oneshot::Sender<Value>)>,
    unclaimed: VecDeque<Unclaimed>,
    unclaimed_bytes: usize,
    /// Interactions asked for on this link, newest last (bounded).
    asked: VecDeque<String>,
    next_token: u64,
    closed: bool,
}

impl Inbox {
    /// Waits for the answer to interaction `id`, or takes it if it already arrived.
    pub(super) fn register(&mut self, id: &str, now: Instant) -> Result<Registration, Refused> {
        if self.closed {
            return Err(Refused::Closed);
        }
        self.expire(now);
        if let Some(answer) = self.take_unclaimed(id) {
            return Ok(Registration::Answered(answer));
        }
        if self
            .waiting
            .get(id)
            .is_some_and(|(_, waiter)| !waiter.is_closed())
        {
            return Err(Refused::AlreadyWaiting);
        }
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        let (sender, answer) = oneshot::channel();
        self.waiting.insert(id.to_owned(), (token, sender));
        if !self.asked.iter().any(|asked| asked == id) {
            self.asked.push_back(id.to_owned());
            if self.asked.len() > ASKED_MAX {
                self.asked.pop_front();
            }
        }
        Ok(Registration::Waiting {
            token,
            answer,
            others_in_flight: self
                .waiting
                .values()
                .filter(|(_, waiter)| !waiter.is_closed())
                .count()
                .saturating_sub(1),
        })
    }

    /// Stops waiting. Only the registration `token` is removed: a later one for the same id
    /// is left alone.
    pub(super) fn unregister(&mut self, id: &str, token: u64) {
        if self.waiting.get(id).is_some_and(|(current, _)| *current == token) {
            self.waiting.remove(id);
        }
    }

    /// Hands `message` (of `size` bytes on the wire) to whoever waits for it. Returns it when
    /// nobody did — for the link's subscribers — keeping a copy if it answers an interaction, and
    /// why it was kept.
    pub(super) fn deliver(
        &mut self,
        message: Value,
        size: usize,
        now: Instant,
    ) -> (Option<Value>, Option<Kept>) {
        let Some(id) = interaction_id(&message).map(str::to_owned) else {
            return (Some(message), None);
        };
        let message = match self.waiting.remove(&id) {
            Some((_, waiter)) => match waiter.send(message) {
                Ok(()) => return (None, None),
                // The waiter gave up a moment ago: keep the answer for the next one.
                Err(message) => message,
            },
            None => message,
        };
        let kept = if self.unclaimed.iter().any(|kept| kept.id == id) {
            Kept::Duplicate
        } else if self.asked.iter().any(|asked| *asked == id) {
            Kept::Late
        } else {
            Kept::Unknown
        };
        self.keep(id, message.clone(), size, now);
        (Some(message), Some(kept))
    }

    /// The link ended: every waiter is woken (their receivers fail) and nothing more is kept.
    pub(super) fn close(&mut self) {
        self.closed = true;
        self.waiting.clear();
        self.unclaimed.clear();
        self.unclaimed_bytes = 0;
    }

    /// Unclaimed answers currently kept.
    #[cfg(test)]
    fn kept(&self) -> usize {
        self.unclaimed.len()
    }

    fn keep(&mut self, id: String, answer: Value, size: usize, now: Instant) {
        self.expire(now);
        // A second answer to the same interaction replaces the first.
        self.take_unclaimed(&id);
        if size > UNCLAIMED_MAX_BYTES {
            return;
        }
        self.unclaimed_bytes += size;
        self.unclaimed.push_back(Unclaimed {
            id,
            answer,
            size,
            at: now,
        });
        while self.unclaimed.len() > UNCLAIMED_MAX || self.unclaimed_bytes > UNCLAIMED_MAX_BYTES {
            match self.unclaimed.pop_front() {
                Some(oldest) => self.unclaimed_bytes -= oldest.size,
                None => break,
            }
        }
    }

    fn take_unclaimed(&mut self, id: &str) -> Option<Value> {
        let position = self.unclaimed.iter().position(|kept| kept.id == id)?;
        let kept = self.unclaimed.remove(position)?;
        self.unclaimed_bytes -= kept.size;
        Some(kept.answer)
    }

    fn expire(&mut self, now: Instant) {
        while let Some(oldest) = self.unclaimed.front() {
            if now.saturating_duration_since(oldest.at) < UNCLAIMED_TTL {
                break;
            }
            self.unclaimed_bytes -= oldest.size;
            self.unclaimed.pop_front();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer(id: &str) -> Value {
        json!({ "discriminator": "success", "interactionId": id })
    }

    fn waiting(registration: Result<Registration, Refused>) -> (u64, oneshot::Receiver<Value>) {
        match registration {
            Ok(Registration::Waiting { token, answer, .. }) => (token, answer),
            other => panic!("expected to wait, got {other:?}"),
        }
    }

    #[test]
    fn an_answer_goes_to_the_request_it_answers_and_only_to_it() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        let (_, mut a) = waiting(inbox.register("a", now));
        let (_, mut b) = waiting(inbox.register("b", now));
        assert_eq!(inbox.deliver(answer("b"), 10, now), (None, None));
        assert_eq!(b.try_recv().unwrap(), answer("b"));
        assert!(a.try_recv().is_err(), "a must still be waiting");
        assert_eq!(inbox.kept(), 0);
    }

    #[test]
    fn messages_that_answer_nothing_go_to_the_subscribers_only() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        let accounts = json!({ "discriminator": "accountList", "accounts": [] });
        assert_eq!(inbox.deliver(accounts.clone(), 10, now), (Some(accounts), None));
        assert_eq!(inbox.kept(), 0, "only answers are kept");
    }

    #[test]
    fn a_late_answer_is_kept_for_whoever_asks_next() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        assert_eq!(
            inbox.deliver(answer("late"), 10, now),
            (Some(answer("late")), Some(Kept::Unknown))
        );
        match inbox.register("late", now).unwrap() {
            Registration::Answered(value) => assert_eq!(value, answer("late")),
            other => panic!("{other:?}"),
        }
        // Taken once: the next registration waits.
        waiting(inbox.register("late", now));
    }

    #[test]
    fn an_answer_whose_waiter_just_left_is_kept() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        let (_, receiver) = waiting(inbox.register("x", now));
        drop(receiver);
        assert_eq!(inbox.deliver(answer("x"), 10, now).1, Some(Kept::Late));
        assert_eq!(inbox.kept(), 1);
    }

    /// Why an answer nobody waits for is kept tells a wallet answering late from one answering
    /// what this link never asked, or answering twice.
    #[test]
    fn an_unclaimed_answer_says_whether_it_is_late_unknown_or_a_duplicate() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        let (_, first) = waiting(inbox.register("asked", now));
        let (_, second) = waiting(inbox.register("other", now));
        drop((first, second));
        assert_eq!(inbox.deliver(answer("asked"), 10, now).1, Some(Kept::Late));
        assert_eq!(inbox.deliver(answer("asked"), 10, now).1, Some(Kept::Duplicate));
        assert_eq!(inbox.deliver(answer("never"), 10, now).1, Some(Kept::Unknown));
        assert_eq!(inbox.kept(), 2, "the duplicate replaced the first");

        let (_, _a) = waiting(inbox.register("a", now));
        match inbox.register("b", now) {
            Ok(Registration::Waiting { others_in_flight, .. }) => assert_eq!(others_in_flight, 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_waiter_per_interaction() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        let (first, receiver) = waiting(inbox.register("x", now));
        assert_eq!(inbox.register("x", now).unwrap_err(), Refused::AlreadyWaiting);
        // A stale unregister of an older registration does not remove the current one.
        inbox.unregister("x", first.wrapping_add(7));
        assert_eq!(inbox.register("x", now).unwrap_err(), Refused::AlreadyWaiting);
        inbox.unregister("x", first);
        drop(receiver);
        waiting(inbox.register("x", now));
    }

    #[test]
    fn what_is_kept_is_bounded_in_number_size_and_age() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        for n in 0..UNCLAIMED_MAX + 5 {
            inbox.deliver(answer(&n.to_string()), 10, now);
        }
        assert_eq!(inbox.kept(), UNCLAIMED_MAX);
        // The oldest went first.
        waiting(inbox.register("0", now));

        inbox.deliver(answer("big"), UNCLAIMED_MAX_BYTES, now);
        assert_eq!(inbox.kept(), 1, "a big answer pushes the others out");
        inbox.deliver(answer("huge"), UNCLAIMED_MAX_BYTES + 1, now);
        assert!(matches!(
            inbox.register("huge", now),
            Ok(Registration::Waiting { .. })
        ));

        let later = now + UNCLAIMED_TTL;
        match inbox.register("big", later) {
            Ok(Registration::Waiting { .. }) => {}
            other => panic!("an expired answer must be gone: {other:?}"),
        }
    }

    #[test]
    fn closing_wakes_every_waiter_and_refuses_new_ones() {
        let now = Instant::now();
        let mut inbox = Inbox::default();
        let (_, mut receiver) = waiting(inbox.register("x", now));
        inbox.close();
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert_eq!(inbox.register("y", now).unwrap_err(), Refused::Closed);
    }
}
