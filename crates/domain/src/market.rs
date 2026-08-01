//! Public market data: the tape, the book and candles.

use crate::{ids::Symbol, order::Side, time::Timestamp};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A trade printed on the public tape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTrade {
    pub symbol: Symbol,
    pub price: Decimal,
    pub qty: Decimal,
    /// Side of the aggressor, i.e. who crossed the spread.
    pub taker_side: Side,
    pub ts: Timestamp,
    /// Venue trade id, used to drop duplicates after a reconnect.
    pub id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub qty: Decimal,
}

/// A local order book plus the sequence bookkeeping needed to know it is still correct.
///
/// Venues publish an initial REST snapshot and then a stream of deltas. Miss one delta and
/// the book is silently wrong from then on — which is far worse than having no book at all.
/// `last_update_id` exists so [`OrderBook::apply`] can detect the gap and force a resync.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBook {
    pub symbol: Option<Symbol>,
    /// Best bid first (descending price).
    pub bids: Vec<BookLevel>,
    /// Best ask first (ascending price).
    pub asks: Vec<BookLevel>,
    /// Venue sequence number of the last applied update.
    pub last_update_id: u64,
    pub ts: Timestamp,
    /// False until a delta has been chained onto the snapshot.
    ///
    /// A REST snapshot is taken at a moment the delta stream has usually not reached yet,
    /// so the two do not line up. Until they do, a delta whose predecessor is older than
    /// the snapshot is the expected overlap, not a gap — see [`OrderBook::apply`].
    #[serde(default)]
    pub synced: bool,
}

/// One incremental book update as published by the venue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookDelta {
    pub symbol: Symbol,
    /// Sequence of the update immediately preceding this one.
    pub prev_update_id: u64,
    pub last_update_id: u64,
    /// Absolute levels, not deltas. A zero quantity removes the level.
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub ts: Timestamp,
}

/// Outcome of feeding a delta into the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Applied; the book is current.
    Applied,
    /// Older than what we hold. Normal right after a snapshot; drop it.
    Stale,
    /// A gap in the venue sequence. The book is now untrustworthy and must be resynced
    /// from a fresh REST snapshot.
    Gap { expected: u64, got: u64 },
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<BookLevel> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<BookLevel> {
        self.asks.first().copied()
    }

    pub fn mid(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Some((b.price + a.price) / Decimal::TWO),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()?.price - self.best_bid()?.price)
    }

    /// Apply an incremental update, reporting staleness and sequence gaps rather than
    /// silently corrupting the book.
    ///
    /// Two regimes, and conflating them is what makes a book resync forever:
    ///
    /// * **Attaching to a snapshot** (`synced == false`). The snapshot is normally *ahead*
    ///   of the live stream, so deltas arrive that the snapshot already contains. Those are
    ///   [`ApplyOutcome::Stale`]. The first delta that spans the snapshot — its predecessor
    ///   at or before the snapshot id, its own id past it — is the join, and applying it
    ///   makes the book synced.
    /// * **Chained** (`synced == true`). Every delta must name our current id as its
    ///   predecessor. Anything else means the venue skipped updates.
    pub fn apply(&mut self, delta: &BookDelta) -> ApplyOutcome {
        if delta.last_update_id <= self.last_update_id {
            return ApplyOutcome::Stale;
        }

        let continues = if self.synced {
            delta.prev_update_id == self.last_update_id
        } else {
            // The stream may still be behind the snapshot; it may not be ahead of it.
            delta.prev_update_id <= self.last_update_id
        };
        if !continues {
            return ApplyOutcome::Gap { expected: self.last_update_id, got: delta.prev_update_id };
        }

        apply_side(&mut self.bids, &delta.bids, true);
        apply_side(&mut self.asks, &delta.asks, false);
        self.last_update_id = delta.last_update_id;
        self.ts = delta.ts;
        self.synced = true;
        ApplyOutcome::Applied
    }
}

/// Merge absolute levels into one side, dropping levels the venue zeroed out.
fn apply_side(side: &mut Vec<BookLevel>, updates: &[BookLevel], descending: bool) {
    for upd in updates {
        match side.binary_search_by(|lvl| cmp_price(lvl.price, upd.price, descending)) {
            Ok(pos) => {
                if upd.qty.is_zero() {
                    side.remove(pos);
                } else {
                    side[pos].qty = upd.qty;
                }
            }
            Err(pos) if !upd.qty.is_zero() => side.insert(pos, *upd),
            Err(_) => {}
        }
    }
}

fn cmp_price(a: Decimal, b: Decimal, descending: bool) -> std::cmp::Ordering {
    if descending {
        b.cmp(&a)
    } else {
        a.cmp(&b)
    }
}

/// How a candle series is cut into bars.
///
/// Time and tick bars answer different questions and neither substitutes for the other. A
/// one-second bar says what happened in that second, including that nothing did; a
/// twenty-five-tick bar always contains twenty-five prints, so quiet stretches compress and
/// bursts expand. Scalping charts are usually tick-bucketed for exactly that reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bucketing {
    /// One bar per fixed slice of the clock.
    Time { interval_ms: i64 },
    /// One bar per fixed number of prints.
    Ticks { count: u32 },
}

impl Bucketing {
    pub fn label(&self) -> String {
        match self {
            Self::Time { interval_ms } if *interval_ms % 60_000 == 0 => {
                format!("{}m", interval_ms / 60_000)
            }
            Self::Time { interval_ms } if *interval_ms % 1_000 == 0 => {
                format!("{}s", interval_ms / 1_000)
            }
            Self::Time { interval_ms } => format!("{interval_ms}ms"),
            Self::Ticks { count } => format!("{count} ticks"),
        }
    }
}

/// A finished or forming candle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candle {
    pub symbol: Symbol,
    /// Time of the bar's first print. For tick bars this is where it began, not a slot.
    pub open_time: Timestamp,
    /// How this bar was cut.
    pub bucketing: Bucketing,
    /// Prints folded into this bar so far.
    pub trades: u32,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    /// False while the candle is still forming.
    pub closed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExchangeId, MarketKind};
    use rust_decimal_macros::dec;

    fn sym() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    fn delta(
        prev: u64,
        last: u64,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
    ) -> BookDelta {
        BookDelta {
            symbol: sym(),
            prev_update_id: prev,
            last_update_id: last,
            bids: bids.into_iter().map(|(price, qty)| BookLevel { price, qty }).collect(),
            asks: asks.into_iter().map(|(price, qty)| BookLevel { price, qty }).collect(),
            ts: Timestamp::from_millis(1),
        }
    }

    fn seeded() -> OrderBook {
        let mut book = OrderBook::default();
        book.apply(&delta(0, 10, vec![(dec!(100), dec!(1))], vec![(dec!(101), dec!(2))]));
        book
    }

    #[test]
    fn sides_stay_sorted_best_first() {
        let mut book = seeded();
        book.apply(&delta(
            10,
            11,
            vec![(dec!(99), dec!(5)), (dec!(100.5), dec!(3))],
            vec![(dec!(102), dec!(1))],
        ));

        assert_eq!(
            book.bids.iter().map(|l| l.price).collect::<Vec<_>>(),
            vec![dec!(100.5), dec!(100), dec!(99)]
        );
        assert_eq!(book.asks.iter().map(|l| l.price).collect::<Vec<_>>(), vec![dec!(101), dec!(102)]);
        assert_eq!(book.best_bid().unwrap().price, dec!(100.5));
        assert_eq!(book.best_ask().unwrap().price, dec!(101));
    }

    #[test]
    fn zero_quantity_removes_the_level() {
        let mut book = seeded();
        book.apply(&delta(10, 11, vec![(dec!(100), Decimal::ZERO)], vec![]));
        assert!(book.bids.is_empty());
        assert!(book.best_bid().is_none());
    }

    #[test]
    fn a_sequence_gap_is_reported_and_not_applied() {
        let mut book = seeded();
        // The venue jumped: this update follows 42, but we last applied 10.
        let out = book.apply(&delta(42, 43, vec![(dec!(100), dec!(9))], vec![]));

        assert_eq!(out, ApplyOutcome::Gap { expected: 10, got: 42 });
        // Crucially the book was left untouched, so a resync starts from a known state.
        assert_eq!(book.last_update_id, 10);
        assert_eq!(book.best_bid().unwrap().qty, dec!(1));
    }

    #[test]
    fn a_snapshot_ahead_of_the_stream_waits_for_the_delta_that_spans_it() {
        // The live failure this came from: the REST snapshot is taken later than the point
        // the delta stream has reached, so the first deltas name a predecessor older than
        // the snapshot. Treating those as gaps resyncs forever and never builds a book.
        let mut book = OrderBook { last_update_id: 1_000, ..Default::default() };
        assert!(!book.synced);

        // Already contained in the snapshot: dropped, no gap raised.
        assert_eq!(book.apply(&delta(940, 960, vec![(dec!(100), dec!(1))], vec![])), ApplyOutcome::Stale);
        assert_eq!(book.apply(&delta(960, 1_000, vec![], vec![])), ApplyOutcome::Stale);

        // Spans the snapshot: predecessor at or before 1000, own id past it. This is the join.
        assert_eq!(book.apply(&delta(990, 1_010, vec![(dec!(100), dec!(7))], vec![])), ApplyOutcome::Applied);
        assert!(book.synced);
        assert_eq!(book.last_update_id, 1_010);
        assert_eq!(book.best_bid().unwrap().qty, dec!(7));
    }

    #[test]
    fn once_synced_the_chain_is_strict_again() {
        let mut book = OrderBook { last_update_id: 1_000, ..Default::default() };
        book.apply(&delta(990, 1_010, vec![], vec![]));
        assert!(book.synced);

        // The tolerance used to attach to the snapshot must not survive the join, or a real
        // dropped update would be applied as if nothing were missing.
        assert_eq!(
            book.apply(&delta(1_005, 1_020, vec![], vec![])),
            ApplyOutcome::Gap { expected: 1_010, got: 1_005 }
        );
        assert_eq!(book.apply(&delta(1_010, 1_020, vec![], vec![])), ApplyOutcome::Applied);
    }

    #[test]
    fn a_stream_that_ran_past_the_snapshot_demands_a_newer_one() {
        // Opposite race: the snapshot is too old to attach to, so there is no join to wait
        // for and the caller must fetch again.
        let mut book = OrderBook { last_update_id: 1_000, ..Default::default() };
        assert_eq!(
            book.apply(&delta(1_500, 1_520, vec![], vec![])),
            ApplyOutcome::Gap { expected: 1_000, got: 1_500 }
        );
        assert!(!book.synced);
    }

    #[test]
    fn replayed_updates_are_dropped_as_stale() {
        let mut book = seeded();
        assert_eq!(book.apply(&delta(0, 10, vec![], vec![])), ApplyOutcome::Stale);
        assert_eq!(book.last_update_id, 10);
    }

    #[test]
    fn bucketing_labels_read_the_way_a_trader_would_say_them() {
        assert_eq!(Bucketing::Time { interval_ms: 1_000 }.label(), "1s");
        assert_eq!(Bucketing::Time { interval_ms: 300_000 }.label(), "5m");
        assert_eq!(Bucketing::Time { interval_ms: 250 }.label(), "250ms");
        assert_eq!(Bucketing::Ticks { count: 25 }.label(), "25 ticks");
    }

    #[test]
    fn mid_and_spread_need_both_sides() {
        let book = seeded();
        assert_eq!(book.mid(), Some(dec!(100.5)));
        assert_eq!(book.spread(), Some(dec!(1)));
        assert_eq!(OrderBook::default().mid(), None);
    }
}
