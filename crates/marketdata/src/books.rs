//! Keeping a set of order books correct across snapshots, gaps and reconnects.
//!
//! The whole difficulty is that a REST snapshot and a delta stream are taken at different
//! moments and neither side tells you by how much. The naive loop — fetch snapshot, apply
//! deltas, refetch on mismatch — never converges, because each fresh snapshot lands ahead
//! of the stream again. Stage 1 proved that live: fifteen resyncs in three seconds and no
//! book at all.
//!
//! So deltas are **buffered** while a snapshot is in flight and replayed onto it once it
//! arrives. [`domain::OrderBook::apply`] then discards the overlap and joins on the delta
//! that spans the snapshot.
//!
//! This is venue-agnostic on purpose: it is the same problem on every exchange, and it is
//! testable without a network.

use domain::{ApplyOutcome, BookDelta, OrderBook, Symbol, Timestamp};
use std::collections::HashMap;

/// How long to wait before asking for another snapshot for the same symbol.
///
/// Without this a gap storm turns into a REST hammer and earns a rate-limit ban — the
/// second failure mode stage 1 walked into.
pub const DEFAULT_RESYNC_COOLDOWN_MS: i64 = 1_000;

/// Deltas held while a snapshot is in flight. Beyond this the snapshot is hopeless and we
/// start over rather than replaying minutes of stale updates.
const MAX_BUFFERED: usize = 4_096;

/// How long a snapshot request may be outstanding before another is allowed.
///
/// Not a nicety. A fetch that never reports back — a dropped connection, a task that died —
/// would otherwise leave `awaiting` latched on forever and the book would never rebuild.
/// One transient failure at startup must not cost an instrument its book for the whole
/// session, which is exactly what happened before this existed.
pub const REQUEST_DEADLINE_MS: i64 = 10_000;

/// What the caller must do after feeding an update in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// Nothing; the book absorbed it.
    Nothing,
    /// Fetch a REST snapshot and hand it to [`BookSet::install_snapshot`].
    Snapshot(Symbol),
}

/// The result of feeding one delta in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub need: Need,
    /// Whether to pass this delta on to consumers.
    ///
    /// Only updates that chained onto a live book qualify. Buffered, stale and gap-exposing
    /// updates must not be forwarded: a consumer applying them to its own copy would end up
    /// with a book that disagrees with ours, which is worse than having none.
    pub forward: bool,
    /// The book just became usable and consumers must be sent it in full.
    ///
    /// A snapshot does not always join the stream during replay — when the snapshot is ahead
    /// of the stream, the joining delta arrives later, through here. Publishing only from
    /// the replay path leaves such a book alive inside the connector and invisible outside,
    /// which is what kept spot books from ever reaching the terminal.
    pub attached: bool,
}

#[derive(Debug)]
struct Entry {
    symbol: Symbol,
    book: Option<OrderBook>,
    buffered: Vec<BookDelta>,
    awaiting: bool,
    /// When a snapshot was last asked for, for cooldown.
    requested_at: Option<Timestamp>,
    /// When the book last actually moved, for staleness detection.
    updated_at: Timestamp,
}

/// Every book a connector is maintaining.
#[derive(Debug)]
pub struct BookSet {
    entries: HashMap<String, Entry>,
    cooldown_ms: i64,
}

impl Default for BookSet {
    fn default() -> Self {
        Self::new(DEFAULT_RESYNC_COOLDOWN_MS)
    }
}

impl BookSet {
    pub fn new(cooldown_ms: i64) -> Self {
        Self { entries: HashMap::new(), cooldown_ms }
    }

    /// Feed one incremental update.
    pub fn on_delta(&mut self, delta: BookDelta, now: Timestamp) -> Outcome {
        let key = delta.symbol.key();
        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            symbol: delta.symbol.clone(),
            book: None,
            buffered: Vec::new(),
            awaiting: false,
            requested_at: None,
            updated_at: Timestamp::ZERO,
        });

        // No book yet, or one is being rebuilt: hold the update for replay.
        if entry.book.is_none() {
            entry.buffer(delta);
            let need = entry.request_snapshot(now, self.cooldown_ms);
            return Outcome { need, forward: false, attached: false };
        }

        let book = entry.book.as_mut().expect("checked above");
        let was_synced = book.synced;

        match book.apply(&delta) {
            ApplyOutcome::Applied => {
                entry.updated_at = now;
                // First delta to chain onto the snapshot: consumers have never seen this
                // book, so send the whole thing rather than an update to nothing.
                let attached = !was_synced;
                Outcome { need: Need::Nothing, forward: !attached, attached }
            }
            // Normal overlap right after a snapshot.
            ApplyOutcome::Stale => Outcome { need: Need::Nothing, forward: false, attached: false },
            ApplyOutcome::Gap { .. } => {
                // Anything rendered from here would be fiction. Drop the book, keep the
                // update that exposed the gap, and rebuild.
                entry.book = None;
                entry.buffer(delta);
                let need = entry.request_snapshot(now, self.cooldown_ms);
                Outcome { need, forward: false, attached: false }
            }
        }
    }

    /// Install a fetched snapshot and replay whatever arrived while it was in flight.
    ///
    /// Returns the book once it has joined the delta stream — that is the moment it is safe
    /// to publish. `None` means the snapshot was already too old and another is needed.
    pub fn install_snapshot(&mut self, mut snapshot: OrderBook, now: Timestamp) -> Option<OrderBook> {
        let key = snapshot.symbol.as_ref()?.key();
        let entry = self.entries.get_mut(&key)?;

        entry.awaiting = false;
        let buffered = std::mem::take(&mut entry.buffered);
        let span = buffered.first().map(|d| (d.prev_update_id, d.last_update_id));
        let last = buffered.last().map(|d| d.last_update_id);

        for delta in &buffered {
            if let ApplyOutcome::Gap { expected, got } = snapshot.apply(delta) {
                // The stream ran past this snapshot while it was in flight. Keep the
                // remaining updates and let the caller fetch a newer one.
                tracing::debug!(
                    snapshot = expected,
                    delta_prev = got,
                    buffered = buffered.len(),
                    "snapshot too old for the buffered updates"
                );
                entry.book = None;
                return None;
            }
        }
        tracing::debug!(
            snapshot = snapshot.last_update_id, ?span, buffered_last = ?last,
            buffered = buffered.len(), joined = snapshot.synced,
            "snapshot replayed"
        );

        entry.updated_at = now;
        let joined = snapshot.synced;
        entry.book = Some(snapshot);
        // Until a delta has chained onto it the book is a still photograph of a moment that
        // has already passed; publishing it would show a stale spread as if it were live.
        joined.then(|| entry.book.clone().expect("just set"))
    }

    pub fn book(&self, key: &str) -> Option<&OrderBook> {
        self.entries.get(key).and_then(|e| e.book.as_ref()).filter(|b| b.synced)
    }

    /// Symbols whose book has not moved for `max_age_ms`.
    ///
    /// A frozen book on a live socket is the failure that looks like health, so it gets
    /// reported rather than waited out.
    pub fn stale(&self, now: Timestamp, max_age_ms: i64) -> Vec<Symbol> {
        self.entries
            .values()
            .filter(|e| e.book.is_some() && e.updated_at.age_millis_from(now) > max_age_ms)
            .map(|e| e.symbol.clone())
            .collect()
    }

    /// Report that a snapshot fetch failed, so the next delta may ask again.
    ///
    /// The cooldown still applies — a venue refusing one request will likely refuse the
    /// next — but the request is no longer considered outstanding.
    pub fn snapshot_failed(&mut self, symbol: &Symbol) {
        if let Some(entry) = self.entries.get_mut(&symbol.key()) {
            entry.awaiting = false;
        }
    }

    /// Forget everything. Used on reconnect: the venue's sequence numbers do not survive it.
    pub fn reset(&mut self) {
        self.entries.clear();
    }
}

impl Entry {
    fn buffer(&mut self, delta: BookDelta) {
        if self.buffered.len() >= MAX_BUFFERED {
            // The in-flight snapshot will never catch up with this much backlog.
            self.buffered.clear();
            self.awaiting = false;
            self.requested_at = None;
        }
        self.buffered.push(delta);
    }

    fn request_snapshot(&mut self, now: Timestamp, cooldown_ms: i64) -> Need {
        let since_request = self.requested_at.map(|at| at.age_millis_from(now));

        // An outstanding request blocks another — but only until the deadline. Beyond it we
        // assume the fetch is never coming back rather than wait forever.
        if self.awaiting && since_request.is_some_and(|age| age < REQUEST_DEADLINE_MS) {
            return Need::Nothing;
        }
        if !self.awaiting && since_request.is_some_and(|age| age < cooldown_ms) {
            return Need::Nothing;
        }
        self.awaiting = true;
        self.requested_at = Some(now);
        Need::Snapshot(self.symbol.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookLevel, ExchangeId, MarketKind};
    use rust_decimal_macros::dec;

    fn sym() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    fn delta(prev: u64, last: u64, bid_qty: rust_decimal::Decimal) -> BookDelta {
        BookDelta {
            symbol: sym(),
            prev_update_id: prev,
            last_update_id: last,
            bids: vec![BookLevel { price: dec!(100), qty: bid_qty }],
            asks: vec![BookLevel { price: dec!(101), qty: dec!(1) }],
            ts: Timestamp::from_millis(1),
        }
    }

    fn snapshot(last_update_id: u64) -> OrderBook {
        OrderBook {
            symbol: Some(sym()),
            bids: vec![BookLevel { price: dec!(100), qty: dec!(1) }],
            asks: vec![BookLevel { price: dec!(101), qty: dec!(1) }],
            last_update_id,
            ts: Timestamp::from_millis(1),
            synced: false,
        }
    }

    fn at(ms: i64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    #[test]
    fn the_first_delta_asks_for_a_snapshot_and_is_kept_for_replay() {
        let mut books = BookSet::default();
        assert_eq!(books.on_delta(delta(990, 1_010, dec!(5)), at(0)).need, Need::Snapshot(sym()));

        // Nothing publishable yet — the book has not joined the stream.
        assert!(books.book(&sym().key()).is_none());
    }

    #[test]
    fn deltas_that_arrive_while_the_snapshot_is_in_flight_are_replayed_onto_it() {
        // This is the fix for the fifteen-resyncs-per-three-seconds failure.
        let mut books = BookSet::default();
        books.on_delta(delta(990, 1_010, dec!(5)), at(0));
        books.on_delta(delta(1_010, 1_020, dec!(7)), at(10));

        let published = books.install_snapshot(snapshot(1_000), at(20)).expect("joins the stream");

        assert_eq!(published.last_update_id, 1_020, "both buffered updates replayed");
        assert_eq!(published.best_bid().unwrap().qty, dec!(7), "the later update won");
        assert!(published.synced);
    }

    #[test]
    fn no_further_snapshot_is_requested_while_one_is_in_flight() {
        let mut books = BookSet::default();
        assert_eq!(books.on_delta(delta(990, 1_010, dec!(5)), at(0)).need, Need::Snapshot(sym()));

        // Every subsequent delta must NOT trigger another fetch, or we hammer the venue.
        assert_eq!(books.on_delta(delta(1_010, 1_020, dec!(6)), at(5)).need, Need::Nothing);
        assert_eq!(books.on_delta(delta(1_020, 1_030, dec!(7)), at(9)).need, Need::Nothing);
    }

    #[test]
    fn a_gap_rebuilds_the_book_and_stops_publishing_it() {
        let mut books = BookSet::default();
        books.on_delta(delta(990, 1_010, dec!(5)), at(0));
        books.install_snapshot(snapshot(1_000), at(10));
        assert!(books.book(&sym().key()).is_some());

        // The venue skipped updates: 1_010 -> 1_500 is not a chain.
        let out = books.on_delta(delta(1_500, 1_520, dec!(9)), at(2_000));

        assert_eq!(out.need, Need::Snapshot(sym()));
        assert!(!out.forward, "the update that exposed the gap must not reach consumers");
        assert!(books.book(&sym().key()).is_none(), "a torn book must not be served");
    }

    #[test]
    fn resync_requests_are_rate_limited_after_a_gap() {
        let mut books = BookSet::new(1_000);
        books.on_delta(delta(990, 1_010, dec!(5)), at(0));
        books.install_snapshot(snapshot(1_000), at(0));

        assert_eq!(books.on_delta(delta(1_500, 1_520, dec!(9)), at(1_000)).need, Need::Snapshot(sym()));
        books.install_snapshot(snapshot(1_400), at(1_010)); // too old, book stays absent

        // Inside the cooldown: hold off even though a book is still missing.
        assert_eq!(books.on_delta(delta(1_600, 1_620, dec!(9)), at(1_500)).need, Need::Nothing);
        // Past it: ask again.
        assert_eq!(books.on_delta(delta(1_620, 1_640, dec!(9)), at(2_200)).need, Need::Snapshot(sym()));
    }

    #[test]
    fn a_snapshot_the_stream_already_overran_is_rejected_not_installed() {
        let mut books = BookSet::default();
        books.on_delta(delta(5_000, 5_010, dec!(5)), at(0));

        // Snapshot from long before the buffered updates: they cannot chain onto it.
        assert!(books.install_snapshot(snapshot(1_000), at(10)).is_none());
        assert!(books.book(&sym().key()).is_none());
    }

    #[test]
    fn a_snapshot_with_no_delta_yet_is_held_back_until_it_joins() {
        let mut books = BookSet::default();
        books.on_delta(delta(990, 1_010, dec!(5)), at(0));

        // Snapshot ahead of the one buffered delta: nothing chains yet, so nothing is
        // published — serving it would show a spread from a moment that already passed.
        assert!(books.install_snapshot(snapshot(2_000), at(10)).is_none());
    }

    #[test]
    fn a_frozen_book_is_reported_stale() {
        let mut books = BookSet::default();
        books.on_delta(delta(990, 1_010, dec!(5)), at(0));
        books.install_snapshot(snapshot(1_000), at(1_000));

        assert!(books.stale(at(3_000), 5_000).is_empty());
        assert_eq!(books.stale(at(10_000), 5_000), vec![sym()]);
    }

    #[test]
    fn a_book_that_joins_later_is_still_announced_to_consumers() {
        // The bug: a snapshot ahead of the stream does not join during replay. The joining
        // delta arrives from the socket afterwards, and publishing only from the replay path
        // left such a book alive inside the connector and invisible outside — spot books
        // never reached the terminal at all.
        let mut books = BookSet::default();
        books.on_delta(delta(990, 1_000, dec!(1)), at(0));

        // Snapshot is ahead of everything buffered: nothing chains, nothing is published.
        assert!(books.install_snapshot(snapshot(2_000), at(10)).is_none());
        assert!(books.book(&sym().key()).is_none());

        // Stream is still behind: dropped as the expected overlap, still nothing to announce.
        let out = books.on_delta(delta(1_500, 1_900, dec!(2)), at(20));
        assert!(!out.attached);
        assert!(!out.forward);

        // And now the delta that spans the snapshot. This is the moment consumers must be
        // handed the whole book.
        let out = books.on_delta(delta(1_990, 2_010, dec!(3)), at(30));
        assert!(out.attached, "the join must be announced");
        assert!(!out.forward, "a delta is useless to a consumer with no book");
        assert_eq!(books.book(&sym().key()).unwrap().best_bid().unwrap().qty, dec!(3));

        // Afterwards it is an ordinary chained stream again.
        let out = books.on_delta(delta(2_010, 2_020, dec!(4)), at(40));
        assert!(!out.attached);
        assert!(out.forward);
    }

    #[test]
    fn a_failed_fetch_does_not_cost_the_instrument_its_book_forever() {
        // The bug this exists for: one transport error at startup latched `awaiting` on and
        // the book never rebuilt for the rest of the session.
        let mut books = BookSet::new(1_000);
        assert_eq!(books.on_delta(delta(990, 1_010, dec!(5)), at(0)).need, Need::Snapshot(sym()));

        // The fetch fails; nothing is ever installed.
        books.snapshot_failed(&sym());

        // Inside the cooldown, still quiet.
        assert_eq!(books.on_delta(delta(1_010, 1_020, dec!(5)), at(500)).need, Need::Nothing);
        // Past it, the book asks again instead of giving up.
        assert_eq!(books.on_delta(delta(1_020, 1_030, dec!(5)), at(1_200)).need, Need::Snapshot(sym()));
    }

    #[test]
    fn a_fetch_that_never_reports_back_stops_blocking_after_the_deadline() {
        // A task that dies without calling either install_snapshot or snapshot_failed must
        // not wedge the book. Nothing outside this crate can guarantee it reports.
        let mut books = BookSet::new(1_000);
        assert_eq!(books.on_delta(delta(990, 1_010, dec!(5)), at(0)).need, Need::Snapshot(sym()));

        let just_inside = REQUEST_DEADLINE_MS - 1;
        assert_eq!(books.on_delta(delta(1_010, 1_020, dec!(5)), at(just_inside)).need, Need::Nothing);
        assert_eq!(
            books.on_delta(delta(1_020, 1_030, dec!(5)), at(REQUEST_DEADLINE_MS + 1)).need,
            Need::Snapshot(sym())
        );
    }

    #[test]
    fn reset_forgets_everything_because_sequence_numbers_do_not_survive_a_reconnect() {
        let mut books = BookSet::default();
        books.on_delta(delta(990, 1_010, dec!(5)), at(0));
        books.install_snapshot(snapshot(1_000), at(10));
        assert!(books.book(&sym().key()).is_some());

        books.reset();
        assert!(books.book(&sym().key()).is_none());
        // And the next delta starts a clean rebuild immediately, no cooldown carried over.
        assert_eq!(books.on_delta(delta(1_010, 1_020, dec!(5)), at(20)).need, Need::Snapshot(sym()));
    }
}
