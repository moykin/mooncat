//! Picking a session back up after a break.
//!
//! # What a reconnect costs without this
//!
//! A laptop closed for thirty seconds, a train going through a tunnel, a Wi-Fi handover. On
//! reconnect the naive answer is to send the whole starting state again: every book, every
//! instrument, the tape, the account. That is megabytes and a second or two of a blank screen,
//! for a gap in which almost nothing happened.
//!
//! So the core keeps a short history per channel. The terminal says where it got to, and gets
//! back only what it missed.
//!
//! # Why the epoch comes first
//!
//! Sequence numbers and [`SymbolId`](crate::SymbolId)s only mean anything within one core
//! lifetime. After a restart the core assigns them afresh, and a terminal resuming against the
//! old ones would apply deltas to the wrong instruments — silently, and with the book looking
//! entirely plausible. `core_epoch` makes that impossible to miss: a mismatch is refused
//! outright and the terminal starts clean.
//!
//! # Why some channels cannot resume at all
//!
//! Replaying book deltas to rebuild a book costs more than one snapshot and is wrong if any
//! delta was dropped, so [`Channel::resumable`](crate::Channel::resumable) says no and the
//! terminal is told to resync instead. Control has nothing to replay — a session either
//! resumed or it did not. Reports carry their own cursor, so a replay would duplicate rows the
//! terminal has already committed.

use crate::envelope::Channel;
use crate::event::ServerEvent;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// How far back a resume can reach.
///
/// Two minutes covers a tunnel, a lid closed over lunch is not covered, and that asymmetry is
/// deliberate: past a couple of minutes a fresh snapshot is both cheaper and more certainly
/// correct than replaying a long tail of deltas.
pub const RESUME_WINDOW: Duration = Duration::from_secs(120);

/// Events retained per channel, from `11-protocol-spec.md` §5.1.
///
/// Sized by rate rather than by importance: the tape produces the most and is the cheapest to
/// keep, the account produces few and each one matters.
pub const fn ring_capacity(channel: Channel) -> usize {
    match channel {
        Channel::ACCOUNT => 8_192,
        Channel::COMMAND => 2_048,
        Channel::TAPE => 16_384,
        Channel::CANDLES => 4_096,
        Channel::ALERTS => 512,
        Channel::REFERENCE | Channel::SETTINGS => 256,
        _ => 0,
    }
}

/// Why a resume could not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Impossible {
    /// The core restarted; nothing from the previous lifetime can be trusted.
    EpochChanged { theirs: u64, ours: u64 },
    /// The break was longer than the window.
    WindowExceeded,
    /// The session was forgotten — evicted, revoked, or never existed.
    UnknownSession,
}

/// What the terminal gets back.
#[derive(Clone, Debug, PartialEq)]
pub struct Resumed {
    /// Per channel, the events it missed, oldest first.
    pub replay: Vec<(Channel, Vec<ServerEvent>)>,
    /// Channels that could not be replayed. The terminal must ask for a fresh snapshot of
    /// each — being told which is what stops it from throwing away the ones that survived.
    pub lost: Vec<Channel>,
}

#[derive(Debug)]
struct Ring {
    capacity: usize,
    /// Sequence, when it was published, and the event itself.
    entries: VecDeque<(u64, Instant, ServerEvent)>,
    next_seq: u64,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self { capacity, entries: VecDeque::new(), next_seq: 1 }
    }

    fn push(&mut self, event: ServerEvent, now: Instant) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.capacity > 0 {
            self.entries.push_back((seq, now, event));
            while self.entries.len() > self.capacity {
                self.entries.pop_front();
            }
        }
        seq
    }

    /// Everything after `after_seq`, or `None` if the history no longer reaches that far.
    fn since(&self, after_seq: u64, now: Instant) -> Option<Vec<ServerEvent>> {
        // Already up to date. Checked before the coverage test so that a terminal which
        // missed nothing resumes even on a channel that has been idle longer than the window.
        if after_seq + 1 == self.next_seq {
            return Some(Vec::new());
        }
        if after_seq >= self.next_seq {
            // Ahead of the core: it saw something this core never sent, which only happens
            // across an epoch boundary that should already have been caught.
            return None;
        }

        let oldest = self.entries.front()?;
        if oldest.0 > after_seq + 1 {
            return None; // The gap starts before anything still held.
        }
        if now.duration_since(oldest.1) > RESUME_WINDOW {
            return None; // Held, but too old to be trusted.
        }

        Some(
            self.entries
                .iter()
                .filter(|(seq, _, _)| *seq > after_seq)
                .map(|(_, _, event)| event.clone())
                .collect(),
        )
    }
}

/// The replayable history of one session.
#[derive(Debug)]
pub struct ResumeLog {
    epoch: u64,
    rings: HashMap<Channel, Ring>,
}

impl ResumeLog {
    /// `epoch` identifies this core lifetime. It must change on every restart, or a terminal
    /// will resume against sequence numbers that mean something else now.
    pub fn new(epoch: u64) -> Self {
        Self { epoch, rings: HashMap::new() }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Record an outgoing event and return the sequence it was given.
    ///
    /// Every event on every channel gets a sequence, including channels that cannot resume:
    /// the number is what makes a gap detectable, and detecting one on the book channel is
    /// precisely what triggers a resync.
    pub fn record(&mut self, event: ServerEvent, now: Instant) -> u64 {
        let channel = event.channel();
        self.rings.entry(channel).or_insert_with(|| Ring::new(ring_capacity(channel))).push(event, now)
    }

    /// Next sequence a channel will use, for a `SyncComplete`.
    pub fn next_seq(&self, channel: Channel) -> u64 {
        self.rings.get(&channel).map_or(1, |r| r.next_seq)
    }

    /// Answer a resume request.
    ///
    /// `acks` is what the terminal says it has: the last sequence it saw on each channel.
    pub fn resume(
        &self,
        their_epoch: u64,
        acks: &[(Channel, u64)],
        now: Instant,
    ) -> Result<Resumed, Impossible> {
        // Before anything else. A mismatch means every sequence and every symbol id the
        // terminal holds refers to a core that no longer exists.
        if their_epoch != self.epoch {
            return Err(Impossible::EpochChanged { theirs: their_epoch, ours: self.epoch });
        }

        let (mut replay, mut lost) = (Vec::new(), Vec::new());
        for (channel, last_seen) in acks {
            if !channel.resumable() {
                lost.push(*channel);
                continue;
            }
            match self.rings.get(channel).and_then(|r| r.since(*last_seen, now)) {
                Some(events) if events.is_empty() => {}
                Some(events) => replay.push((*channel, events)),
                None => lost.push(*channel),
            }
        }
        replay.sort_by_key(|(c, _)| c.0);
        lost.sort_by_key(|c| c.0);
        Ok(Resumed { replay, lost })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::SymbolId;
    use crate::event::{OrderCause, ResyncReason};
    use domain::{ClientOrderId, ExchangeId, MarketKind, Order, OrderStatus, OrderType};
    use domain::{PositionSide, Side, Symbol, TimeInForce, Timestamp};
    use rust_decimal_macros::dec;

    fn origin() -> Instant {
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    fn at(secs: u64) -> Instant {
        origin() + Duration::from_secs(secs)
    }

    fn an_order() -> Order {
        Order {
            client_id: ClientOrderId("c-1".into()),
            venue_id: None,
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT"),
            side: Side::Buy,
            order_type: OrderType::Limit,
            status: OrderStatus::New,
            qty: dec!(1),
            filled_qty: dec!(0),
            price: Some(dec!(1)),
            trigger_price: None,
            avg_price: dec!(0),
            tif: TimeInForce::Gtc,
            position_side: PositionSide::Long,
            reduce_only: false,
            created_at: Timestamp::from_millis(1),
            updated_at: Timestamp::from_millis(1),
        }
    }

    /// An account event, which is on a resumable channel.
    fn account(rev: u64) -> ServerEvent {
        ServerEvent::OrderUpdate { order: Box::new(an_order()), rev, cause: OrderCause::Placed }
    }

    /// A book event, which is not.
    fn book() -> ServerEvent {
        ServerEvent::BookResync { sid: SymbolId(1), reason: ResyncReason::Gap }
    }

    // --- the three acceptance criteria (doc 11 §12.5, 4-6) ---------------------------------

    #[test]
    fn a_short_break_delivers_only_what_was_missed() {
        // Criterion 4. The whole point: a five-second gap must not cost a full resend.
        let mut log = ResumeLog::new(7);
        for rev in 0..10 {
            log.record(account(rev), at(0));
        }
        // The terminal saw the first four and then went away.
        let resumed = log.resume(7, &[(Channel::ACCOUNT, 4)], at(5)).unwrap();

        assert!(resumed.lost.is_empty());
        let (channel, events) = &resumed.replay[0];
        assert_eq!(*channel, Channel::ACCOUNT);
        assert_eq!(events.len(), 6, "exactly the six it missed, not all ten");
    }

    #[test]
    fn a_long_break_cannot_be_resumed() {
        // Criterion 5. Past the window a fresh snapshot is both cheaper and more certainly
        // correct than replaying a long tail.
        let mut log = ResumeLog::new(7);
        log.record(account(0), at(0));
        log.record(account(1), at(0));

        let resumed = log.resume(7, &[(Channel::ACCOUNT, 0)], at(180)).unwrap();
        assert_eq!(resumed.lost, vec![Channel::ACCOUNT]);
        assert!(resumed.replay.is_empty());
    }

    #[test]
    fn a_restarted_core_refuses_the_resume_outright() {
        // Criterion 6, and the most dangerous case in the file. Sequences and symbol ids are
        // reassigned on restart; resuming against the old ones would apply deltas to the wrong
        // instruments, silently, with a book that looks entirely plausible.
        let mut log = ResumeLog::new(8);
        log.record(account(0), at(0));

        assert_eq!(
            log.resume(7, &[(Channel::ACCOUNT, 0)], at(1)),
            Err(Impossible::EpochChanged { theirs: 7, ours: 8 })
        );
    }

    #[test]
    fn the_epoch_is_checked_before_anything_else() {
        // Even a resume that would otherwise be perfectly satisfiable must be refused.
        let mut log = ResumeLog::new(2);
        log.record(account(0), at(0));
        assert!(log.resume(1, &[], at(0)).is_err(), "an empty ack list is no excuse");
    }

    // --- what can and cannot be replayed ------------------------------------------------------

    #[test]
    fn the_book_is_always_reported_lost_and_never_replayed() {
        // Replaying deltas to rebuild a book costs more than one snapshot and is wrong if any
        // were dropped.
        let mut log = ResumeLog::new(1);
        log.record(book(), at(0));

        let resumed = log.resume(1, &[(Channel::BOOK, 0)], at(1)).unwrap();
        assert_eq!(resumed.lost, vec![Channel::BOOK]);
        assert!(resumed.replay.is_empty());
    }

    #[test]
    fn losing_one_channel_does_not_cost_the_others() {
        // The terminal is told exactly which channels to resync, so it keeps the state that
        // survived instead of starting over.
        let mut log = ResumeLog::new(1);
        for rev in 0..5 {
            log.record(account(rev), at(0));
        }
        log.record(book(), at(0));

        let resumed = log.resume(1, &[(Channel::ACCOUNT, 2), (Channel::BOOK, 0)], at(1)).unwrap();
        assert_eq!(resumed.lost, vec![Channel::BOOK]);
        assert_eq!(resumed.replay.len(), 1);
        assert_eq!(resumed.replay[0].0, Channel::ACCOUNT);
    }

    #[test]
    fn a_terminal_that_missed_nothing_gets_nothing() {
        let mut log = ResumeLog::new(1);
        for rev in 0..3 {
            log.record(account(rev), at(0));
        }
        let resumed = log.resume(1, &[(Channel::ACCOUNT, 3)], at(1)).unwrap();
        assert!(resumed.replay.is_empty() && resumed.lost.is_empty());
    }

    #[test]
    fn an_idle_channel_still_resumes_even_after_the_window() {
        // A terminal that missed nothing must not be told to resync merely because nothing has
        // happened for two minutes. This is the case a naive age check gets wrong.
        let mut log = ResumeLog::new(1);
        log.record(account(0), at(0));

        let resumed = log.resume(1, &[(Channel::ACCOUNT, 1)], at(1_000)).unwrap();
        assert!(resumed.lost.is_empty(), "nothing was missed, so nothing is lost");
    }

    #[test]
    fn a_channel_the_terminal_never_saw_replays_from_the_start() {
        let mut log = ResumeLog::new(1);
        for rev in 0..3 {
            log.record(account(rev), at(0));
        }
        let resumed = log.resume(1, &[(Channel::ACCOUNT, 0)], at(1)).unwrap();
        assert_eq!(resumed.replay[0].1.len(), 3);
    }

    // --- ring behaviour --------------------------------------------------------------------------

    #[test]
    fn a_gap_older_than_the_ring_is_reported_lost_not_silently_shortened() {
        // The failure that would be worst: handing back a partial replay, which the terminal
        // would apply as if it were complete.
        let mut log = ResumeLog::new(1);
        let capacity = ring_capacity(Channel::ALERTS);
        for id in 0..capacity as u64 + 100 {
            log.record(ServerEvent::AlertDeleted { id }, at(0));
        }
        let resumed = log.resume(1, &[(Channel::ALERTS, 1)], at(1)).unwrap();
        assert_eq!(resumed.lost, vec![Channel::ALERTS], "a partial replay must never be offered");
    }

    #[test]
    fn each_channel_is_sized_for_what_it_produces() {
        // The tape produces the most and is cheapest to keep; the account produces few and
        // each one matters.
        assert!(ring_capacity(Channel::TAPE) > ring_capacity(Channel::ACCOUNT));
        assert!(ring_capacity(Channel::ACCOUNT) > ring_capacity(Channel::ALERTS));
        for channel in [Channel::BOOK, Channel::CONTROL, Channel::REPORT, Channel::ARB] {
            assert_eq!(ring_capacity(channel), 0, "{channel} does not resume, so it keeps nothing");
        }
    }

    #[test]
    fn a_non_resumable_channel_costs_no_memory() {
        let mut log = ResumeLog::new(1);
        for _ in 0..100_000 {
            log.record(book(), at(0));
        }
        assert_eq!(log.rings[&Channel::BOOK].entries.len(), 0, "nothing is retained");
        assert_eq!(log.next_seq(Channel::BOOK), 100_001, "but sequences still advance");
    }

    #[test]
    fn sequences_are_per_channel_and_start_at_one() {
        // A single global sequence would make every channel's gap look like every other's.
        let mut log = ResumeLog::new(1);
        assert_eq!(log.record(account(0), at(0)), 1);
        assert_eq!(log.record(account(1), at(0)), 2);
        assert_eq!(log.record(book(), at(0)), 1, "the book channel counts separately");
        assert_eq!(log.next_seq(Channel::ACCOUNT), 3);
        assert_eq!(log.next_seq(Channel::CANDLES), 1, "an untouched channel starts at one");
    }

    #[test]
    fn a_terminal_ahead_of_the_core_is_refused_rather_than_believed() {
        // It claims to have seen something this core never sent. Across an epoch boundary that
        // is caught earlier; reaching here means something is wrong, and replaying from a
        // negative gap would be worse than resyncing.
        let mut log = ResumeLog::new(1);
        log.record(account(0), at(0));
        let resumed = log.resume(1, &[(Channel::ACCOUNT, 99)], at(1)).unwrap();
        assert_eq!(resumed.lost, vec![Channel::ACCOUNT]);
    }

    #[test]
    fn replayed_events_come_back_in_order_and_unchanged() {
        // A replay applied out of order is worse than no replay: the terminal would end up
        // with an older state than it started with.
        let mut log = ResumeLog::new(1);
        let sent: Vec<_> = (0..5).map(account).collect();
        for event in &sent {
            log.record(event.clone(), at(0));
        }

        let resumed = log.resume(1, &[(Channel::ACCOUNT, 0)], at(1)).unwrap();
        assert_eq!(resumed.replay[0].1, sent, "order and content must both survive");
    }

    #[test]
    fn the_window_is_measured_from_the_oldest_event_not_from_the_disconnect() {
        // The core does not know when the terminal went away; it knows how old its own history
        // is, and that is the honest thing to measure.
        let mut log = ResumeLog::new(1);
        log.record(account(0), at(0));
        log.record(account(1), at(119));

        assert!(log.resume(1, &[(Channel::ACCOUNT, 0)], at(119)).unwrap().lost.is_empty());
        assert_eq!(
            log.resume(1, &[(Channel::ACCOUNT, 0)], at(121)).unwrap().lost,
            vec![Channel::ACCOUNT],
            "past the window the oldest entry can no longer be trusted"
        );
    }
}
