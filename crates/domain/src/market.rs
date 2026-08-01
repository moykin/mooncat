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
    pub fn apply(&mut self, delta: &BookDelta) -> ApplyOutcome {
        if delta.last_update_id <= self.last_update_id {
            return ApplyOutcome::Stale;
        }
        if self.last_update_id != 0 && delta.prev_update_id != self.last_update_id {
            return ApplyOutcome::Gap { expected: self.last_update_id, got: delta.prev_update_id };
        }

        apply_side(&mut self.bids, &delta.bids, true);
        apply_side(&mut self.asks, &delta.asks, false);
        self.last_update_id = delta.last_update_id;
        self.ts = delta.ts;
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

/// A finished or forming candle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candle {
    pub symbol: Symbol,
    /// Candle open time.
    pub open_time: Timestamp,
    pub interval_ms: i64,
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
    fn replayed_updates_are_dropped_as_stale() {
        let mut book = seeded();
        assert_eq!(book.apply(&delta(0, 10, vec![], vec![])), ApplyOutcome::Stale);
        assert_eq!(book.last_update_id, 10);
    }

    #[test]
    fn mid_and_spread_need_both_sides() {
        let book = seeded();
        assert_eq!(book.mid(), Some(dec!(100.5)));
        assert_eq!(book.spread(), Some(dec!(1)));
        assert_eq!(OrderBook::default().mid(), None);
    }
}
