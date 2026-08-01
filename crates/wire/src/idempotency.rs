//! Making a retry safe: the `req` ring.
//!
//! # The problem it solves
//!
//! A terminal sends `PlaceOrder` and hears nothing. It cannot tell whether the command was
//! lost on the way out, executed and the acknowledgement was lost on the way back, or is
//! simply slow. Every option looks identical from where it is sitting. Without help it must
//! choose between never retrying — and leaving the operator staring at a screen that may or
//! may not reflect reality — or retrying and risking a second order.
//!
//! So the core remembers. Every command carries a `req` in its envelope; the core keeps the
//! last few thousand it has seen along with the answer it gave, and a repeat gets that answer
//! back **without the command running again**. That is what makes the retry policy in
//! `command.rs` sound, and it is why `AckCode::VenueTimeout` can be marked retryable at all:
//! the command may well have executed at the venue, and only the ring makes asking again a
//! no-op rather than a second execution.
//!
//! # Why a ring and not a set
//!
//! A set that never forgets is a memory leak on a process meant to run for months. Four
//! thousand and ninety-six entries is roughly an hour of continuous manual trading at one
//! command a second, and far longer in practice — comfortably more than any retry a human or
//! a reconnect will produce.
//!
//! # The case that has no good answer
//!
//! A `req` older than everything the ring remembers cannot be judged: it may be a duplicate
//! whose record was evicted, or it may be new. [`Admission::TooOld`] refuses it rather than
//! guessing, because the two errors are not symmetric — refusing a legitimate command shows
//! the operator an error they can act on, while executing a duplicate silently doubles a
//! position.

use crate::envelope::ReqId;
use crate::event::ServerEvent;
use std::collections::{HashMap, VecDeque};

/// How many requests a session remembers.
pub const SEEN_REQ_CAPACITY: usize = 4_096;

/// What to do with an arriving command.
#[derive(Clone, Debug, PartialEq)]
pub enum Admission {
    /// Not seen before. Execute it.
    Fresh,
    /// Seen, and finished. Send this acknowledgement again and do **not** execute.
    Replay(Box<ServerEvent>),
    /// Seen, and still running. Do not execute; the original will answer when it finishes.
    InFlight,
    /// Older than anything remembered, so it cannot be proven not to be a duplicate.
    TooOld { oldest_remembered: ReqId },
}

#[derive(Clone, Debug)]
enum Record {
    InFlight,
    /// The final acknowledgement, kept verbatim so a replay is byte-identical to the original.
    Answered(Box<ServerEvent>),
}

/// One session's memory of what it has been asked.
#[derive(Debug, Default)]
pub struct ReqRing {
    /// Insertion order, for eviction. Holds only keys; the answers live in `records`.
    order: VecDeque<ReqId>,
    records: HashMap<ReqId, Record>,
    /// Highest `req` ever admitted. `req` is monotonic per session, so this is what makes
    /// "older than the ring" decidable at all.
    high_water: Option<ReqId>,
    evicted_below: Option<ReqId>,
}

impl ReqRing {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Decide what to do with an arriving `req`, recording it if it is new.
    ///
    /// Recording happens here rather than after execution on purpose: two frames carrying the
    /// same `req` can be in flight at once — that is exactly what a retry after a timeout
    /// looks like — and a check that did not claim the id would let both through.
    pub fn admit(&mut self, req: ReqId) -> Admission {
        if let Some(record) = self.records.get(&req) {
            return match record {
                Record::InFlight => Admission::InFlight,
                Record::Answered(ack) => Admission::Replay(ack.clone()),
            };
        }

        // Below the eviction floor: the answer, if there ever was one, is gone.
        if let Some(floor) = self.evicted_below {
            if req < floor {
                return Admission::TooOld { oldest_remembered: floor };
            }
        }

        self.records.insert(req, Record::InFlight);
        self.order.push_back(req);
        self.high_water = Some(self.high_water.map_or(req, |h| h.max(req)));
        self.evict_if_full();
        Admission::Fresh
    }

    /// Record the final acknowledgement for a request.
    ///
    /// Non-final statuses are ignored: `Accepted` means the work is still running, and caching
    /// it would make a retry return "accepted" forever while the original quietly finished.
    pub fn complete(&mut self, req: ReqId, ack: ServerEvent) {
        if let ServerEvent::CommandAck { status, .. } = &ack {
            if !status.is_final() {
                return;
            }
        }
        // Only for a request being tracked. A `complete` for an evicted or unknown id would
        // otherwise resurrect it and defeat the eviction floor.
        if let Some(slot) = self.records.get_mut(&req) {
            *slot = Record::Answered(Box::new(ack));
        }
    }

    /// Highest request id admitted so far.
    pub fn high_water(&self) -> Option<ReqId> {
        self.high_water
    }

    fn evict_if_full(&mut self) {
        while self.order.len() > SEEN_REQ_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.records.remove(&oldest);
                // The floor is the id *after* the one just dropped: everything up to and
                // including it is now unjudgeable.
                self.evicted_below = Some(oldest + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AckCode, AckDetail, AckStatus};

    fn ack(req: ReqId, status: AckStatus) -> ServerEvent {
        ServerEvent::CommandAck { req, status, detail: None }
    }

    fn done(req: ReqId, count: u32) -> ServerEvent {
        ServerEvent::CommandAck {
            req,
            status: AckStatus::Done,
            detail: Some(AckDetail { count: Some(count), ..Default::default() }),
        }
    }

    /// A stand-in for the side effect a command would have.
    #[derive(Default)]
    struct Venue {
        orders_placed: u32,
    }

    impl Venue {
        /// The shape every handler will have: admit, act only if fresh, record the answer.
        fn handle(&mut self, ring: &mut ReqRing, req: ReqId) -> Option<ServerEvent> {
            match ring.admit(req) {
                Admission::Fresh => {
                    self.orders_placed += 1;
                    let answer = done(req, 1);
                    ring.complete(req, answer.clone());
                    Some(answer)
                }
                Admission::Replay(previous) => Some(*previous),
                Admission::InFlight => None,
                Admission::TooOld { .. } => Some(ServerEvent::CommandAck {
                    req,
                    status: AckStatus::Rejected,
                    detail: Some(AckDetail {
                        code: Some(AckCode::Invalid),
                        message: "request id is older than the core remembers".into(),
                        retryable: false,
                        ..Default::default()
                    }),
                }),
            }
        }
    }

    // --- the acceptance invariant (test 8 from 11-protocol-spec.md §10.5) ------------------

    #[test]
    fn a_repeated_req_returns_the_cached_ack_and_does_not_execute_twice() {
        let (mut ring, mut venue) = (ReqRing::new(), Venue::default());

        let first = venue.handle(&mut ring, 1).expect("the first attempt is executed");
        assert_eq!(venue.orders_placed, 1);

        // The terminal timed out and asked again with the same id.
        let second = venue.handle(&mut ring, 1).expect("a retry is answered");
        assert_eq!(venue.orders_placed, 1, "the venue must not see a second order");
        assert_eq!(second, first, "the retry must get the original answer, byte for byte");

        // And a third time, and a tenth.
        for _ in 0..10 {
            assert_eq!(venue.handle(&mut ring, 1), Some(first.clone()));
        }
        assert_eq!(venue.orders_placed, 1);
    }

    #[test]
    fn a_different_req_is_a_different_command() {
        // The other half: deduplication must not swallow genuine repeats of the same action.
        // Buying twice on purpose is a normal thing to do.
        let (mut ring, mut venue) = (ReqRing::new(), Venue::default());
        venue.handle(&mut ring, 1);
        venue.handle(&mut ring, 2);
        assert_eq!(venue.orders_placed, 2);
    }

    // --- in-flight ---------------------------------------------------------------------------

    #[test]
    fn a_retry_arriving_while_the_original_is_running_is_held_not_executed() {
        // Two frames with the same `req` can genuinely be in flight at once — that is what a
        // retry after a timeout looks like from the core's side.
        let mut ring = ReqRing::new();
        assert_eq!(ring.admit(7), Admission::Fresh);
        assert_eq!(ring.admit(7), Admission::InFlight, "the second must not execute");

        ring.complete(7, done(7, 3));
        match ring.admit(7) {
            Admission::Replay(ack) => assert_eq!(*ack, done(7, 3)),
            other => panic!("expected a replay once finished, got {other:?}"),
        }
    }

    #[test]
    fn a_non_final_status_is_not_cached() {
        // Caching `Accepted` would make every later retry answer "accepted" forever, even
        // after the original had finished and failed.
        let mut ring = ReqRing::new();
        ring.admit(1);
        ring.complete(1, ack(1, AckStatus::Accepted));
        assert_eq!(ring.admit(1), Admission::InFlight, "still running, not answered");

        ring.complete(1, ack(1, AckStatus::Failed));
        assert!(matches!(ring.admit(1), Admission::Replay(_)), "a final status is cached");
    }

    #[test]
    fn every_final_status_is_cached_including_the_failures() {
        // A failure is an answer. Replaying it is what stops a terminal retrying into the
        // same rejection forever.
        for status in [AckStatus::Done, AckStatus::Rejected, AckStatus::Failed] {
            let mut ring = ReqRing::new();
            ring.admit(1);
            ring.complete(1, ack(1, status));
            assert!(matches!(ring.admit(1), Admission::Replay(_)), "{status:?} must be replayable");
        }
    }

    // --- eviction -----------------------------------------------------------------------------

    #[test]
    fn the_ring_holds_exactly_its_capacity() {
        let mut ring = ReqRing::new();
        for req in 0..SEEN_REQ_CAPACITY as u64 * 2 {
            ring.admit(req);
            ring.complete(req, done(req, 1));
        }
        assert_eq!(ring.len(), SEEN_REQ_CAPACITY, "memory must not grow without bound");
    }

    #[test]
    fn recent_requests_survive_and_ancient_ones_are_refused_not_re_executed() {
        let (mut ring, mut venue) = (ReqRing::new(), Venue::default());

        // Fill past capacity, so the earliest ids fall out.
        let total = SEEN_REQ_CAPACITY as u64 + 100;
        for req in 0..total {
            venue.handle(&mut ring, req);
        }
        assert_eq!(venue.orders_placed, total as u32);

        // A recent one still replays.
        let recent = total - 1;
        venue.handle(&mut ring, recent);
        assert_eq!(venue.orders_placed, total as u32, "a remembered retry must not execute");

        // An evicted one is refused rather than executed. That asymmetry is the point: an
        // error the operator can see beats a position quietly doubling.
        match ring.admit(0) {
            Admission::TooOld { oldest_remembered } => assert!(oldest_remembered > 0),
            other => panic!("an evicted id must not be treated as fresh, got {other:?}"),
        }
        assert_eq!(venue.orders_placed, total as u32);
    }

    #[test]
    fn an_evicted_request_is_refused_through_the_handler_too() {
        let (mut ring, mut venue) = (ReqRing::new(), Venue::default());
        for req in 1..=SEEN_REQ_CAPACITY as u64 + 10 {
            venue.handle(&mut ring, req);
        }
        let before = venue.orders_placed;

        let answer = venue.handle(&mut ring, 1).expect("refused, but answered");
        match answer {
            ServerEvent::CommandAck { status, detail, .. } => {
                assert_eq!(status, AckStatus::Rejected);
                assert!(!detail.expect("a reason").retryable, "retrying will not help");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(venue.orders_placed, before, "an unjudgeable id must not execute");
    }

    #[test]
    fn completing_an_evicted_request_does_not_resurrect_it() {
        // Otherwise a late handler finishing after eviction would put the id back and defeat
        // the floor — and worse, leave the ring holding more than its capacity.
        let mut ring = ReqRing::new();
        for req in 0..SEEN_REQ_CAPACITY as u64 + 5 {
            ring.admit(req);
        }
        let len_before = ring.len();

        ring.complete(0, done(0, 1));
        assert_eq!(ring.len(), len_before);
        assert!(matches!(ring.admit(0), Admission::TooOld { .. }));
    }

    // --- ordering ------------------------------------------------------------------------------

    #[test]
    fn ids_arriving_out_of_order_are_still_judged_correctly() {
        // `req` is monotonic by contract, but frames are not guaranteed to arrive in the order
        // they were sent once a retry is in play.
        let mut ring = ReqRing::new();
        assert_eq!(ring.admit(10), Admission::Fresh);
        assert_eq!(ring.admit(5), Admission::Fresh, "a lower id is still new");
        assert_eq!(ring.admit(10), Admission::InFlight);
        assert_eq!(ring.high_water(), Some(10), "the high-water mark tracks the maximum");
    }

    #[test]
    fn a_fresh_session_remembers_nothing() {
        // The ring is per session. A reconnect starts clean, which is correct: the terminal
        // starts a fresh `req` sequence with it.
        let mut ring = ReqRing::new();
        assert!(ring.is_empty());
        assert_eq!(ring.high_water(), None);
        assert_eq!(ring.admit(1), Admission::Fresh);
    }

    #[test]
    fn the_replayed_answer_is_identical_and_not_merely_equivalent() {
        // A terminal correlating on `req` and comparing payloads must not see a difference
        // between the original and the replay, or it will treat one command as two outcomes.
        let mut ring = ReqRing::new();
        let original = done(42, 17);
        ring.admit(42);
        ring.complete(42, original.clone());

        match ring.admit(42) {
            Admission::Replay(replayed) => {
                assert_eq!(*replayed, original);
                assert_eq!(
                    rmp_serde::to_vec_named(&*replayed).unwrap(),
                    rmp_serde::to_vec_named(&original).unwrap(),
                    "the replay must encode to the same bytes"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
