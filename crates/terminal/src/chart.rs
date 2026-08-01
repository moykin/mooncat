//! Chart geometry.
//!
//! The plot is a tick plot, not a candle chart: price against wall-clock time, one mark per
//! print, joined by the stepped line the last trade traces. That is what a scalper reads —
//! where each fill landed and which side crossed the spread — and it is information a candle
//! throws away by construction.
//!
//! Everything maps onto a unit square, x and y both 0..1, and the renderer multiplies by
//! whatever pixel size the pane has. The arithmetic stays testable without a window and the
//! drawing code stays free of scale maths. y grows downward, matching the screen.

use domain::{Decimal, OrderBook, PublicTrade, Side};

/// Fraction of the price range left blank above and below.
const PADDING: f64 = 0.08;
/// Price gridlines aimed for.
const TARGET_TICKS: usize = 6;
/// Time labels along the bottom.
const TIME_TICKS: usize = 6;
/// Buckets the volume histogram is summed into.
const VOLUME_BUCKETS: usize = 120;

/// One print.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    pub x: f32,
    pub y: f32,
    /// The aggressor crossed upward.
    pub buy: bool,
    /// Size relative to the largest print shown, 0..1 — a big fill should look big.
    pub weight: f32,
}

/// A step in the last-price line: hold `y` from `x` until `to_x`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub x: f32,
    pub to_x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PriceTick {
    pub price: Decimal,
    pub y: f32,
    /// Signed distance from the anchor, in percent.
    pub percent: Decimal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeTick {
    pub x: f32,
    pub at_millis: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolumeBar {
    pub x: f32,
    pub half_width: f32,
    /// 0..1 of the histogram's own height.
    pub height: f32,
    /// Buy volume outweighed sell volume in this bucket.
    pub buy: bool,
}

/// One price level of the book, drawn against the chart's own price scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeatRow {
    pub y: f32,
    /// Size relative to the largest level shown, 0..1.
    pub fill: f32,
    pub ask: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickPlot {
    pub marks: Vec<Mark>,
    pub steps: Vec<Step>,
    pub price_ticks: Vec<PriceTick>,
    pub time_ticks: Vec<TimeTick>,
    pub volume: Vec<VolumeBar>,
    pub high: Decimal,
    pub low: Decimal,
    pub last: Option<Decimal>,
    pub last_y: Option<f32>,
    /// Zero of the percent axis: the last traded price.
    pub anchor: Option<Decimal>,
    pub from_millis: i64,
    pub to_millis: i64,
}

/// Lay out the prints of the last `span_ms`.
///
/// `prints` may arrive in any order — replayed history and live trades reach the terminal
/// from opposite ends — so it is sorted here rather than trusted.
pub fn tick_plot(prints: &[PublicTrade], span_ms: i64, book: Option<&OrderBook>) -> TickPlot {
    let mut sorted: Vec<&PublicTrade> = prints.iter().collect();
    sorted.sort_by_key(|t| (t.ts.millis(), t.id));

    let Some(newest) = sorted.last().map(|t| t.ts.millis()) else {
        return TickPlot::default();
    };
    let span = span_ms.max(1);
    let from = newest - span;
    let shown: Vec<&PublicTrade> = sorted.into_iter().filter(|t| t.ts.millis() >= from).collect();
    if shown.is_empty() {
        return TickPlot::default();
    }

    let (high, low) = pad(price_extremes(&shown, book));
    let range = to_f64(high - low);
    let heaviest = shown.iter().map(|t| t.qty).max().unwrap_or(Decimal::ONE);

    let x_of = |millis: i64| ((millis - from) as f64 / span as f64).clamp(0.0, 1.0) as f32;
    let y_of = |price: Decimal| {
        if range <= 0.0 {
            0.5
        } else {
            (to_f64(high - price) / range).clamp(0.0, 1.0) as f32
        }
    };

    let marks = shown
        .iter()
        .map(|t| Mark {
            x: x_of(t.ts.millis()),
            y: y_of(t.price),
            buy: t.taker_side == Side::Buy,
            weight: ratio(t.qty, heaviest),
        })
        .collect();

    // The line is stepped rather than sloped: price does not drift between prints, it sits
    // where the last fill left it until someone trades again.
    let steps = shown
        .windows(2)
        .map(|pair| Step {
            x: x_of(pair[0].ts.millis()),
            to_x: x_of(pair[1].ts.millis()),
            y: y_of(pair[0].price),
        })
        .chain(shown.last().map(|t| Step { x: x_of(t.ts.millis()), to_x: 1.0, y: y_of(t.price) }))
        .collect();

    let last = shown.last().map(|t| t.price);

    TickPlot {
        marks,
        steps,
        price_ticks: price_ticks(high, low, last, &y_of),
        time_ticks: time_ticks(from, newest, span),
        volume: volume(&shown),
        high,
        low,
        last,
        last_y: last.map(&y_of),
        anchor: last,
        from_millis: from,
        to_millis: newest,
    }
}

/// The book drawn against a price scale the caller already fixed.
///
/// Levels outside the chart's range are dropped rather than clamped onto the edge, where
/// they would pile into a single misleading bar.
pub fn heatmap(book: &OrderBook, high: Decimal, low: Decimal, depth: usize) -> Vec<HeatRow> {
    let range = high - low;
    if range <= Decimal::ZERO {
        return Vec::new();
    }

    let levels: Vec<(Decimal, Decimal, bool)> = book
        .asks
        .iter()
        .take(depth)
        .map(|l| (l.price, l.qty, true))
        .chain(book.bids.iter().take(depth).map(|l| (l.price, l.qty, false)))
        .filter(|(price, _, _)| *price <= high && *price >= low)
        .collect();

    let heaviest = levels.iter().map(|(_, qty, _)| *qty).max().unwrap_or(Decimal::ONE);

    levels
        .into_iter()
        .map(|(price, qty, ask)| HeatRow {
            y: (to_f64(high - price) / to_f64(range)).clamp(0.0, 1.0) as f32,
            fill: ratio(qty, heaviest),
            ask,
        })
        .collect()
}

/// Signed distance from the anchor, in percent.
pub fn percent(price: Decimal, anchor: Option<Decimal>) -> Decimal {
    match anchor {
        Some(anchor) if !anchor.is_zero() => (price - anchor) / anchor * Decimal::ONE_HUNDRED,
        _ => Decimal::ZERO,
    }
}

// -------------------------------------------------------------------- internals

/// The vertical range, widened to include the visible book so the heatmap is not clipped.
fn price_extremes(prints: &[&PublicTrade], book: Option<&OrderBook>) -> (Decimal, Decimal) {
    let mut high = prints[0].price;
    let mut low = prints[0].price;
    for print in prints {
        high = high.max(print.price);
        low = low.min(print.price);
    }
    if let Some(book) = book {
        if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
            high = high.max(ask.price);
            low = low.min(bid.price);
        }
    }
    (high, low)
}

fn pad((high, low): (Decimal, Decimal)) -> (Decimal, Decimal) {
    let range = high - low;
    if range.is_zero() {
        // Every print at one price; without this the scale divides by zero.
        let nudge = if high.is_zero() { Decimal::ONE } else { high.abs() * dec_from(0.001) };
        return (high + nudge, low - nudge);
    }
    let margin = range * dec_from(PADDING);
    (high + margin, low - margin)
}

fn price_ticks(
    high: Decimal,
    low: Decimal,
    anchor: Option<Decimal>,
    y_of: &impl Fn(Decimal) -> f32,
) -> Vec<PriceTick> {
    let span = to_f64(high - low);
    let step = nice_step(span / TARGET_TICKS as f64);
    if step <= 0.0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut value = (to_f64(low) / step).ceil() * step;
    // Bounded so a pathological step cannot spin here.
    while value <= to_f64(high) && out.len() < 32 {
        let price = dec_from(value);
        out.push(PriceTick { price, y: y_of(price), percent: percent(price, anchor) });
        value += step;
    }
    out
}

fn time_ticks(from: i64, to: i64, span: i64) -> Vec<TimeTick> {
    (0..TIME_TICKS)
        .map(|index| {
            let fraction = index as f32 / (TIME_TICKS - 1).max(1) as f32;
            TimeTick { x: fraction, at_millis: from + (span as f64 * fraction as f64) as i64 }
        })
        .filter(|tick| tick.at_millis <= to)
        .collect()
}

fn volume(prints: &[&PublicTrade]) -> Vec<VolumeBar> {
    let slot = 1.0 / VOLUME_BUCKETS as f32;
    let mut buys = vec![Decimal::ZERO; VOLUME_BUCKETS];
    let mut sells = vec![Decimal::ZERO; VOLUME_BUCKETS];

    let (from, to) = (prints[0].ts.millis(), prints[prints.len() - 1].ts.millis());
    let span = (to - from).max(1) as f64;

    for print in prints {
        let fraction = (print.ts.millis() - from) as f64 / span;
        let index = ((fraction * VOLUME_BUCKETS as f64) as usize).min(VOLUME_BUCKETS - 1);
        if print.taker_side == Side::Buy {
            buys[index] += print.qty;
        } else {
            sells[index] += print.qty;
        }
    }

    let heaviest = buys.iter().zip(&sells).map(|(b, s)| *b + *s).max().unwrap_or(Decimal::ONE);

    (0..VOLUME_BUCKETS)
        .filter(|i| buys[*i] + sells[*i] > Decimal::ZERO)
        .map(|i| VolumeBar {
            x: i as f32 * slot + slot / 2.0,
            half_width: slot * 0.4,
            height: ratio(buys[i] + sells[i], heaviest),
            buy: buys[i] >= sells[i],
        })
        .collect()
}

/// The nearest 1 / 2 / 2.5 / 5 × 10ⁿ at or below `rough`.
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

fn ratio(part: Decimal, whole: Decimal) -> f32 {
    if whole <= Decimal::ZERO {
        return 0.0;
    }
    let fraction: f32 = (part / whole).try_into().unwrap_or(0.0);
    fraction.clamp(0.0, 1.0)
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
    use domain::{BookLevel, ExchangeId, MarketKind, Symbol, Timestamp};
    use rust_decimal_macros::dec;

    fn print(ts: i64, price: Decimal, qty: Decimal, buy: bool) -> PublicTrade {
        PublicTrade {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT"),
            price,
            qty,
            taker_side: if buy { Side::Buy } else { Side::Sell },
            ts: Timestamp::from_millis(ts),
            id: ts as u64,
        }
    }

    fn tape() -> Vec<PublicTrade> {
        vec![
            print(1_000, dec!(100), dec!(1), true),
            print(2_000, dec!(102), dec!(3), true),
            print(3_000, dec!(99), dec!(2), false),
            print(4_000, dec!(101), dec!(1), true),
        ]
    }

    #[test]
    fn every_print_becomes_a_mark() {
        // The defining difference from a candle chart: nothing is aggregated away.
        let plot = tick_plot(&tape(), 10_000, None);
        assert_eq!(plot.marks.len(), 4);
        assert_eq!(plot.marks.iter().filter(|m| m.buy).count(), 3);
    }

    #[test]
    fn time_runs_left_to_right_and_price_upward() {
        let plot = tick_plot(&tape(), 4_000, None);
        let xs: Vec<f32> = plot.marks.iter().map(|m| m.x).collect();
        assert!(xs.windows(2).all(|w| w[0] <= w[1]));

        // 102 is the highest price, so it must sit nearest the top.
        let highest = plot.marks.iter().min_by(|a, b| a.y.total_cmp(&b.y)).unwrap();
        assert!((highest.x - plot.marks[1].x).abs() < 1e-6);
    }

    #[test]
    fn prints_arriving_out_of_order_are_sorted_not_trusted() {
        // Replayed history and live prints reach the terminal from opposite ends.
        let mut shuffled = tape();
        shuffled.reverse();

        let plot = tick_plot(&shuffled, 10_000, None);
        let xs: Vec<f32> = plot.marks.iter().map(|m| m.x).collect();
        assert!(xs.windows(2).all(|w| w[0] <= w[1]), "order comes from the timestamp");
    }

    #[test]
    fn a_bigger_fill_gets_a_bigger_mark() {
        let plot = tick_plot(&tape(), 10_000, None);
        assert_eq!(plot.marks[1].weight, 1.0, "the 3-lot is the heaviest shown");
        assert!(plot.marks[0].weight < plot.marks[1].weight);
    }

    #[test]
    fn the_line_is_stepped_and_reaches_the_right_edge() {
        // Price holds where the last fill left it; a sloped line would imply drift that
        // never happened.
        let plot = tick_plot(&tape(), 4_000, None);
        assert_eq!(plot.steps.len(), 4);
        assert_eq!(plot.steps.last().unwrap().to_x, 1.0, "the last price runs to now");

        for step in &plot.steps {
            assert!(step.to_x >= step.x);
        }
    }

    #[test]
    fn only_prints_inside_the_window_are_drawn() {
        let plot = tick_plot(&tape(), 1_500, None);
        assert_eq!(plot.marks.len(), 2, "the window ends at the newest print");
        assert_eq!(plot.from_millis, 2_500);
        assert_eq!(plot.to_millis, 4_000);
    }

    #[test]
    fn the_book_widens_the_range_so_the_heatmap_is_not_clipped() {
        let book = OrderBook {
            bids: vec![BookLevel { price: dec!(90), qty: dec!(5) }],
            asks: vec![BookLevel { price: dec!(120), qty: dec!(5) }],
            ..Default::default()
        };
        let plot = tick_plot(&tape(), 10_000, Some(&book));

        assert!(plot.high > dec!(120));
        assert!(plot.low < dec!(90));
    }

    #[test]
    fn heatmap_rows_sit_where_their_price_sits() {
        let book = OrderBook {
            bids: vec![BookLevel { price: dec!(99), qty: dec!(2) }],
            asks: vec![BookLevel { price: dec!(101), qty: dec!(8) }],
            ..Default::default()
        };
        let rows = heatmap(&book, dec!(110), dec!(90), 10);

        assert_eq!(rows.len(), 2);
        let ask = rows.iter().find(|r| r.ask).unwrap();
        let bid = rows.iter().find(|r| !r.ask).unwrap();
        assert!(ask.y < bid.y, "asks sit above bids");
        assert_eq!(ask.fill, 1.0, "the heaviest level fills the row");
        assert!(bid.fill < 1.0);
    }

    #[test]
    fn levels_outside_the_range_are_dropped_not_clamped() {
        // Clamping would pile every far level onto the top row as one fat bar.
        let book = OrderBook {
            bids: vec![BookLevel { price: dec!(10), qty: dec!(500) }],
            asks: vec![BookLevel { price: dec!(101), qty: dec!(1) }],
            ..Default::default()
        };
        let rows = heatmap(&book, dec!(110), dec!(90), 10);

        assert_eq!(rows.len(), 1);
        assert!(rows[0].ask);
    }

    #[test]
    fn volume_is_split_by_aggressor() {
        let plot = tick_plot(&tape(), 10_000, None);
        assert!(!plot.volume.is_empty());
        assert!(plot.volume.iter().all(|b| (0.0..=1.0).contains(&b.height)));
        assert!(plot.volume.iter().any(|b| !b.buy), "the sell print must show as a sell bar");
    }

    #[test]
    fn time_labels_span_the_window() {
        let plot = tick_plot(&tape(), 4_000, None);
        assert!(!plot.time_ticks.is_empty());
        assert_eq!(plot.time_ticks.first().unwrap().at_millis, plot.from_millis);
        assert!(plot.time_ticks.iter().all(|t| (0.0..=1.0).contains(&t.x)));
        assert!(plot.time_ticks.iter().all(|t| t.at_millis <= plot.to_millis));
    }

    #[test]
    fn zero_percent_falls_on_the_last_print() {
        let plot = tick_plot(&tape(), 10_000, None);
        assert_eq!(plot.anchor, Some(dec!(101)));
        assert_eq!(percent(dec!(101), plot.anchor), Decimal::ZERO);
        assert!(percent(dec!(102), plot.anchor) > Decimal::ZERO);
    }

    #[test]
    fn percent_against_a_missing_or_zero_anchor_is_zero_not_infinity() {
        assert_eq!(percent(dec!(100), None), Decimal::ZERO);
        assert_eq!(percent(dec!(100), Some(Decimal::ZERO)), Decimal::ZERO);
    }

    #[test]
    fn a_single_print_does_not_divide_by_zero() {
        let plot = tick_plot(&[print(1_000, dec!(100), dec!(1), true)], 5_000, None);
        assert_eq!(plot.marks.len(), 1);
        assert!(plot.high > plot.low);
        assert!(plot.marks[0].y.is_finite());
    }

    #[test]
    fn an_empty_tape_yields_an_empty_plot_rather_than_a_panic() {
        let plot = tick_plot(&[], 5_000, None);
        assert!(plot.marks.is_empty() && plot.steps.is_empty() && plot.volume.is_empty());
        assert_eq!(plot.last, None);
    }

    #[test]
    fn gridlines_land_on_round_numbers_inside_the_range() {
        let plot = tick_plot(&tape(), 10_000, None);
        for tick in &plot.price_ticks {
            assert!(tick.price >= plot.low && tick.price <= plot.high);
            assert!((0.0..=1.0).contains(&tick.y));
        }
    }

    #[test]
    fn nice_steps_are_the_ones_a_person_would_pick() {
        assert_eq!(nice_step(1.0), 1.0);
        assert_eq!(nice_step(2.2), 2.0);
        assert_eq!(nice_step(3.0), 2.5);
        assert_eq!(nice_step(7.0), 5.0);
        assert_eq!(nice_step(12.0), 10.0);
    }

    #[test]
    fn a_degenerate_step_request_does_not_hang() {
        assert_eq!(nice_step(0.0), 0.0);
        assert_eq!(nice_step(-1.0), 0.0);
        assert_eq!(nice_step(f64::NAN), 0.0);
    }
}
