//! Turning an order book into rows a person can read.
//!
//! Deliberately separate from the drawing code: cumulative depth, distance from mid and the
//! bar scale are arithmetic, and arithmetic should be testable without a window.

use domain::{Decimal, OrderBook};

/// One price level as displayed.
#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    pub price: Decimal,
    pub qty: Decimal,
    /// Size at this level and everything between it and the touch.
    pub cumulative: Decimal,
    /// Signed distance from mid in percent: negative below, positive above.
    pub from_mid_pct: Decimal,
    /// Cumulative size as a fraction of the deepest level shown, for the bar.
    pub fill: f32,
}

/// A book prepared for display, both sides already in top-to-bottom screen order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DepthView {
    /// Highest ask first, best ask last — so the touch sits next to the spread row.
    pub asks: Vec<Level>,
    /// Best bid first, descending.
    pub bids: Vec<Level>,
    pub mid: Option<Decimal>,
    pub spread: Option<Decimal>,
    pub spread_pct: Option<Decimal>,
}

const HUNDRED: Decimal = Decimal::ONE_HUNDRED;

/// Build the view for the `levels` best prices on each side.
pub fn depth_view(book: &OrderBook, levels: usize) -> DepthView {
    let mid = book.mid();

    // Both sides are scaled against one maximum, so an imbalance is visible as bar length
    // rather than having to be read off the numbers.
    let deepest =
        [total(&book.bids, levels), total(&book.asks, levels)].into_iter().max().unwrap_or(Decimal::ZERO);

    let mut asks = side(&book.asks, levels, mid, deepest);
    // Accumulated outward from the touch, but drawn with the far side at the top.
    asks.reverse();

    DepthView {
        asks,
        bids: side(&book.bids, levels, mid, deepest),
        mid,
        spread: book.spread(),
        spread_pct: match (book.spread(), mid) {
            (Some(spread), Some(mid)) if !mid.is_zero() => Some(spread / mid * HUNDRED),
            _ => None,
        },
    }
}

fn total(levels: &[domain::BookLevel], take: usize) -> Decimal {
    levels.iter().take(take).map(|l| l.qty).sum()
}

fn side(levels: &[domain::BookLevel], take: usize, mid: Option<Decimal>, deepest: Decimal) -> Vec<Level> {
    let mut cumulative = Decimal::ZERO;

    levels
        .iter()
        .take(take)
        .map(|level| {
            cumulative += level.qty;
            Level {
                price: level.price,
                qty: level.qty,
                cumulative,
                from_mid_pct: match mid {
                    Some(mid) if !mid.is_zero() => (level.price - mid) / mid * HUNDRED,
                    _ => Decimal::ZERO,
                },
                fill: ratio(cumulative, deepest),
            }
        })
        .collect()
}

/// Cumulative size as a 0..=1 fraction. Saturates rather than panicking on odd input.
fn ratio(part: Decimal, whole: Decimal) -> f32 {
    if whole <= Decimal::ZERO {
        return 0.0;
    }
    let fraction: f32 = (part / whole).try_into().unwrap_or(0.0);
    fraction.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookLevel, Timestamp};
    use rust_decimal_macros::dec;

    fn book(bids: &[(Decimal, Decimal)], asks: &[(Decimal, Decimal)]) -> OrderBook {
        let level = |(price, qty): &(Decimal, Decimal)| BookLevel { price: *price, qty: *qty };
        OrderBook {
            symbol: None,
            bids: bids.iter().map(level).collect(),
            asks: asks.iter().map(level).collect(),
            last_update_id: 1,
            ts: Timestamp::from_millis(1),
            synced: true,
        }
    }

    fn sample() -> OrderBook {
        book(
            &[(dec!(100), dec!(1)), (dec!(99), dec!(2)), (dec!(98), dec!(3))],
            &[(dec!(102), dec!(4)), (dec!(103), dec!(5)), (dec!(104), dec!(6))],
        )
    }

    #[test]
    fn cumulative_size_accumulates_outward_from_the_touch() {
        let view = depth_view(&sample(), 3);

        assert_eq!(
            view.bids.iter().map(|l| l.cumulative).collect::<Vec<_>>(),
            vec![dec!(1), dec!(3), dec!(6)]
        );
        // Asks are drawn far-to-near, so the largest cumulative is at the top.
        assert_eq!(
            view.asks.iter().map(|l| l.price).collect::<Vec<_>>(),
            vec![dec!(104), dec!(103), dec!(102)]
        );
        assert_eq!(
            view.asks.iter().map(|l| l.cumulative).collect::<Vec<_>>(),
            vec![dec!(15), dec!(9), dec!(4)]
        );
    }

    #[test]
    fn the_touch_sits_next_to_the_spread_row() {
        // Best ask must be the last ask row and best bid the first bid row, or the book
        // reads inside-out.
        let view = depth_view(&sample(), 3);
        assert_eq!(view.asks.last().unwrap().price, dec!(102));
        assert_eq!(view.bids.first().unwrap().price, dec!(100));
    }

    #[test]
    fn distance_from_mid_is_signed() {
        let view = depth_view(&sample(), 3);
        assert_eq!(view.mid, Some(dec!(101)));

        assert!(view.bids[0].from_mid_pct < Decimal::ZERO, "bids sit below mid");
        assert!(view.asks[0].from_mid_pct > Decimal::ZERO, "asks sit above mid");
        // 100 against a mid of 101 is a shade under one percent below.
        assert_eq!(view.bids[0].from_mid_pct.round_dp(4), dec!(-0.9901));
    }

    #[test]
    fn both_sides_share_one_bar_scale_so_imbalance_shows() {
        // Asks carry 15 against the bids' 6; if each side were scaled to itself, both would
        // end in a full bar and the imbalance would be invisible.
        let view = depth_view(&sample(), 3);

        assert_eq!(view.asks[0].fill, 1.0, "the deepest level anywhere fills the bar");
        assert!((view.bids[2].fill - 0.4).abs() < 1e-6, "6 of 15");
        assert!(view.bids.iter().all(|l| l.fill < 1.0));
    }

    #[test]
    fn spread_is_reported_in_price_and_in_percent() {
        let view = depth_view(&sample(), 3);
        assert_eq!(view.spread, Some(dec!(2)));
        assert_eq!(view.spread_pct.unwrap().round_dp(4), dec!(1.9802));
    }

    #[test]
    fn a_one_sided_book_does_not_produce_a_mid_or_a_percentage() {
        // Happens for a moment after a resync, and dividing by an absent mid would panic.
        let view = depth_view(&book(&[(dec!(100), dec!(1))], &[]), 3);

        assert_eq!(view.mid, None);
        assert_eq!(view.spread, None);
        assert_eq!(view.spread_pct, None);
        assert_eq!(view.bids[0].from_mid_pct, Decimal::ZERO);
    }

    #[test]
    fn an_empty_book_yields_empty_rows_rather_than_a_division_by_zero() {
        let view = depth_view(&OrderBook::default(), 10);
        assert!(view.asks.is_empty() && view.bids.is_empty());
        assert_eq!(view.mid, None);
    }

    #[test]
    fn asking_for_more_levels_than_exist_is_not_an_error() {
        let view = depth_view(&sample(), 50);
        assert_eq!(view.bids.len(), 3);
        assert_eq!(view.asks.len(), 3);
    }

    #[test]
    fn levels_beyond_the_requested_depth_do_not_affect_the_bar_scale() {
        // The bar answers "how big is this against what I can see", not against the whole
        // book — otherwise every visible bar collapses to nothing on a deep instrument.
        let deep = book(
            &[(dec!(100), dec!(1)), (dec!(99), dec!(1)), (dec!(98), dec!(1_000))],
            &[(dec!(102), dec!(1)), (dec!(103), dec!(1))],
        );
        let view = depth_view(&deep, 2);

        assert_eq!(view.bids[1].fill, 1.0, "2 of the 2 visible, not 2 of 1002");
    }
}
