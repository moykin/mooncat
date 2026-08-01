//! Chart geometry.
//!
//! Everything here maps candles onto a unit square — x and y both run 0..1 — and the
//! renderer multiplies by whatever pixel size the pane happens to have. That keeps the
//! arithmetic testable without a window, and keeps the drawing code free of scale maths.
//!
//! y grows downward, matching the screen: a high price sits near 0.

use domain::{Candle, Decimal};

/// Fraction of the price range left blank above and below, so the extremes are not drawn
/// flush against the edges of the pane.
const PADDING: f64 = 0.08;

/// Price gridlines aimed for. The tick chooser lands near this, not exactly on it.
const TARGET_TICKS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub open_time: i64,
    /// Centre of the bar, 0..1 from the left.
    pub x: f32,
    /// Half the slot width, 0..1 — the body spans `x ± half_width`.
    pub half_width: f32,
    pub open_y: f32,
    pub high_y: f32,
    pub low_y: f32,
    pub close_y: f32,
    /// Close at or above open. Doji count as rising, as everywhere else.
    pub rising: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PriceTick {
    pub price: Decimal,
    /// 0..1 from the top.
    pub y: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChartView {
    pub bars: Vec<Bar>,
    pub price_ticks: Vec<PriceTick>,
    /// Top and bottom of the drawn range, after padding.
    pub high: Decimal,
    pub low: Decimal,
    /// Latest close, for the price line.
    pub last: Option<Decimal>,
    /// Where the last close sits, 0..1 from the top.
    pub last_y: Option<f32>,
}

/// Lay out the most recent `visible` candles.
pub fn chart_view(candles: &[Candle], visible: usize) -> ChartView {
    let shown: Vec<&Candle> = candles.iter().rev().take(visible.max(1)).rev().collect();
    if shown.is_empty() {
        return ChartView::default();
    }

    let (raw_high, raw_low) = extremes(&shown);
    let (high, low) = pad(raw_high, raw_low);
    let span = to_f64(high - low);

    // Slots are sized against the requested width, not the number of candles present, so a
    // chart that is still filling up grows from the left instead of stretching each bar.
    let slots = visible.max(shown.len()).max(1) as f32;
    let slot = 1.0 / slots;
    let first = slots as usize - shown.len();

    let y_of = |price: Decimal| -> f32 {
        if span <= 0.0 {
            return 0.5;
        }
        (to_f64(high - price) / span) as f32
    };

    let bars = shown
        .iter()
        .enumerate()
        .map(|(index, candle)| Bar {
            open_time: candle.open_time.millis(),
            x: (first + index) as f32 * slot + slot / 2.0,
            // A sliver of gap between bars; below three pixels of slot it stops mattering.
            half_width: slot * 0.36,
            open_y: y_of(candle.open),
            high_y: y_of(candle.high),
            low_y: y_of(candle.low),
            close_y: y_of(candle.close),
            rising: candle.close >= candle.open,
        })
        .collect();

    let last = shown.last().map(|c| c.close);

    ChartView {
        bars,
        price_ticks: ticks(high, low).into_iter().map(|p| PriceTick { price: p, y: y_of(p) }).collect(),
        high,
        low,
        last,
        last_y: last.map(y_of),
    }
}

fn extremes(candles: &[&Candle]) -> (Decimal, Decimal) {
    let mut high = candles[0].high;
    let mut low = candles[0].low;
    for candle in candles {
        high = high.max(candle.high);
        low = low.min(candle.low);
    }
    (high, low)
}

/// Widen the range for breathing room, and rescue a range with no height at all.
fn pad(high: Decimal, low: Decimal) -> (Decimal, Decimal) {
    let range = high - low;
    if range.is_zero() {
        // Every candle at one price. Without this the whole chart divides by zero and
        // collapses onto a single line.
        let nudge = if high.is_zero() { Decimal::ONE } else { high.abs() * dec_from(0.001) };
        return (high + nudge, low - nudge);
    }
    let margin = range * dec_from(PADDING);
    (high + margin, low - margin)
}

/// Round gridline prices inside the range.
fn ticks(high: Decimal, low: Decimal) -> Vec<Decimal> {
    let span = to_f64(high - low);
    if span <= 0.0 {
        return vec![low];
    }

    let step = nice_step(span / TARGET_TICKS as f64);
    if step <= 0.0 {
        return vec![low];
    }

    let start = (to_f64(low) / step).ceil() * step;
    let mut out = Vec::new();
    let mut value = start;
    // Bounded so a pathological step cannot spin here.
    while value <= to_f64(high) && out.len() < 32 {
        out.push(dec_from(value));
        value += step;
    }
    out
}

/// The nearest 1 / 2 / 2.5 / 5 × 10ⁿ at or below `rough`.
///
/// Gridlines a trader can read at a glance are round numbers; the exact spacing matters far
/// less than the labels being 62950 rather than 62947.3163.
fn nice_step(rough: f64) -> f64 {
    if rough <= 0.0 || !rough.is_finite() {
        return 0.0;
    }
    let magnitude = 10f64.powf(rough.log10().floor());
    let normalised = rough / magnitude;

    let factor = if normalised >= 5.0 {
        5.0
    } else if normalised >= 2.5 {
        2.5
    } else if normalised >= 2.0 {
        2.0
    } else {
        1.0
    };
    factor * magnitude
}

fn to_f64(value: Decimal) -> f64 {
    value.try_into().unwrap_or(0.0)
}

fn dec_from(value: f64) -> Decimal {
    Decimal::try_from(value).unwrap_or(Decimal::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ExchangeId, MarketKind, Symbol, Timestamp};
    use rust_decimal_macros::dec;

    fn candle(open_time: i64, open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> Candle {
        Candle {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT"),
            open_time: Timestamp::from_millis(open_time),
            interval_ms: 1_000,
            open,
            high,
            low,
            close,
            volume: dec!(1),
            closed: true,
        }
    }

    fn series() -> Vec<Candle> {
        vec![
            candle(1_000, dec!(100), dec!(105), dec!(99), dec!(104)),
            candle(2_000, dec!(104), dec!(108), dec!(103), dec!(106)),
            candle(3_000, dec!(106), dec!(107), dec!(100), dec!(101)),
        ]
    }

    #[test]
    fn a_high_price_sits_near_the_top() {
        // y grows downward like the screen; getting this backwards flips the whole chart.
        let view = chart_view(&series(), 3);
        let highest = view.bars.iter().map(|b| b.high_y).fold(f32::MAX, f32::min);
        let lowest = view.bars.iter().map(|b| b.low_y).fold(f32::MIN, f32::max);

        assert!(highest < lowest);
        assert!((0.0..=1.0).contains(&highest));
        assert!((0.0..=1.0).contains(&lowest));
    }

    #[test]
    fn bars_are_ordered_left_to_right_by_time() {
        let view = chart_view(&series(), 3);
        let xs: Vec<f32> = view.bars.iter().map(|b| b.x).collect();

        assert_eq!(view.bars.iter().map(|b| b.open_time).collect::<Vec<_>>(), vec![1_000, 2_000, 3_000]);
        assert!(xs.windows(2).all(|w| w[0] < w[1]));
        assert!(xs.iter().all(|x| (0.0..=1.0).contains(x)));
    }

    #[test]
    fn a_partly_filled_chart_keeps_its_slot_width_and_grows_from_the_left() {
        // Otherwise three candles stretch across the whole pane and shrink as more arrive,
        // which makes the chart appear to zoom out on its own.
        let full = chart_view(&series(), 3);
        let partial = chart_view(&series(), 10);

        assert!(partial.bars[0].x > 0.5, "three of ten sit at the right-hand end");
        assert!(partial.bars[0].half_width < full.bars[0].half_width);
        assert_eq!(partial.bars.len(), 3);
    }

    #[test]
    fn only_the_most_recent_candles_are_shown() {
        let view = chart_view(&series(), 2);
        assert_eq!(view.bars.iter().map(|b| b.open_time).collect::<Vec<_>>(), vec![2_000, 3_000]);
    }

    #[test]
    fn direction_comes_from_open_against_close() {
        let view = chart_view(&series(), 3);
        assert!(view.bars[0].rising, "100 -> 104");
        assert!(!view.bars[2].rising, "106 -> 101");
    }

    #[test]
    fn a_doji_counts_as_rising_rather_than_falling() {
        let flat = vec![candle(1_000, dec!(100), dec!(101), dec!(99), dec!(100))];
        assert!(chart_view(&flat, 1).bars[0].rising);
    }

    #[test]
    fn a_flat_series_does_not_divide_by_zero() {
        // Happens on an illiquid instrument that printed once and stopped.
        let flat = vec![candle(1_000, dec!(100), dec!(100), dec!(100), dec!(100))];
        let view = chart_view(&flat, 5);

        assert!(view.high > view.low, "the range is nudged apart");
        assert!(view.bars[0].close_y.is_finite());
        assert!((0.0..=1.0).contains(&view.bars[0].close_y));
    }

    #[test]
    fn an_empty_series_yields_an_empty_chart_rather_than_a_panic() {
        let view = chart_view(&[], 10);
        assert!(view.bars.is_empty());
        assert!(view.price_ticks.is_empty());
        assert_eq!(view.last, None);
    }

    #[test]
    fn the_drawn_range_is_padded_beyond_the_extremes() {
        let view = chart_view(&series(), 3);
        assert!(view.high > dec!(108), "the top of the wick is not flush with the edge");
        assert!(view.low < dec!(99));
    }

    #[test]
    fn gridlines_land_on_round_numbers_inside_the_range() {
        let view = chart_view(&series(), 3);
        assert!(!view.price_ticks.is_empty());

        for tick in &view.price_ticks {
            assert!(tick.price >= view.low && tick.price <= view.high);
            assert!((0.0..=1.0).contains(&tick.y));
        }
        // Readable labels, not 62947.3163.
        assert!(view.price_ticks.iter().any(|t| t.price.normalize().scale() <= 1));
    }

    #[test]
    fn nice_steps_are_the_ones_a_person_would_pick() {
        assert_eq!(nice_step(1.0), 1.0);
        assert_eq!(nice_step(2.2), 2.0);
        assert_eq!(nice_step(3.0), 2.5);
        assert_eq!(nice_step(7.0), 5.0);
        assert_eq!(nice_step(12.0), 10.0);
        assert_eq!(nice_step(0.03), 0.025, "2.5 × 10⁻² is in the family too");
    }

    #[test]
    fn a_degenerate_step_request_does_not_hang() {
        assert_eq!(nice_step(0.0), 0.0);
        assert_eq!(nice_step(-1.0), 0.0);
        assert_eq!(nice_step(f64::NAN), 0.0);
    }

    #[test]
    fn the_last_close_is_reported_with_its_position() {
        let view = chart_view(&series(), 3);
        assert_eq!(view.last, Some(dec!(101)));
        assert_eq!(view.last_y, Some(view.bars[2].close_y));
    }
}
