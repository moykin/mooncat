//! The panel bodies.
//!
//! Kept apart from the shell so the chrome — frame, toolbar, dock, status bar — stays
//! readable as one piece of layout, and each panel stays a function of the feed state.

use crate::book_view::{depth_view, DepthView, Level};
use crate::chart::{heatmap, tick_plot, HeatRow, Mark, Step, TickPlot};
use crate::clock;
use crate::feed::{FeedState, LogLevel, Status};
use domain::{Decimal, PublicTrade, Side};
use gpui::prelude::*;
use gpui::{div, px, relative, rgba, AnyElement, App};
use moon_ui::{h_flex, v_flex, MoonPalette, MoonText};

/// Price levels shown on each side of the book.
pub const DEPTH_ROWS: usize = 16;
/// Trades shown in the tape.
pub const TAPE_ROWS: usize = 40;
/// Log lines shown at once.
pub const LOG_ROWS: usize = 60;
/// Width of the price axis gutter.
const AXIS_W: f32 = 78.0;
/// Width of the percent axis gutter.
const PCT_W: f32 = 58.0;
/// Width of the book-depth strip beside the plot.
const HEAT_W: f32 = 96.0;
/// Height of the volume histogram.
const VOLUME_H: f32 = 46.0;
/// Print marks, smallest and largest.
const MARK_MIN: f32 = 3.0;
const MARK_MAX: f32 = 9.0;

const ROW_H: f32 = 17.0;
const BAR_W: f32 = 84.0;

// ------------------------------------------------------------------ order book

pub fn order_book(state: &FeedState, key: &str, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    let Some(book) = state.books.get(key) else {
        return waiting(state, cx);
    };
    let view = depth_view(book, DEPTH_ROWS);

    v_flex()
        .child(depth_header(cx))
        .children(view.asks.iter().map(|l| depth_row(l, p.red, p.red_soft_bd, cx)))
        .child(spread_row(&view, cx))
        .children(view.bids.iter().map(|l| depth_row(l, p.green, p.green_btn, cx)))
        .into_any_element()
}

fn depth_header(cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    h_flex()
        .h(px(ROW_H))
        .gap_2()
        .child(cell("price".into(), p.text_dim, 96.0))
        .child(cell("size".into(), p.text_dim, 76.0))
        .child(cell("total".into(), p.text_dim, 80.0))
        .child(cell("% mid".into(), p.text_dim, 60.0))
}

fn depth_row(level: &Level, text: u32, bar: u32, cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    h_flex()
        .h(px(ROW_H))
        .gap_2()
        .child(cell(show(level.price), text, 96.0))
        .child(cell(show(level.qty), p.text, 76.0))
        .child(cell(show(level.cumulative), p.text_muted, 80.0))
        .child(cell(format!("{:+.3}", level.from_mid_pct), p.text_muted, 60.0))
        .child(
            // The bar reads as depth at a glance; the numbers are for when it matters.
            div()
                .w(px(BAR_W))
                .h(px(ROW_H - 6.0))
                .child(div().h_full().w(px(BAR_W * level.fill)).rounded_sm().bg(rgba((bar << 8) | 0x55))),
        )
}

fn spread_row(view: &DepthView, cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    let text = match (view.mid, view.spread, view.spread_pct) {
        (Some(mid), Some(spread), Some(pct)) => {
            format!("{}   spread {}  ({:.3}%)", show(mid), show(spread), pct)
        }
        _ => "no two-sided market".into(),
    };

    h_flex()
        .h(px(ROW_H + 7.0))
        .w_full()
        .items_center()
        .child(MoonText::new(text).font_size(12.0).mono(true).uppercase(false).color(p.accent))
}

// ----------------------------------------------------------------------- tape

pub fn tape(state: &FeedState, key: &str, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    if state.tape.get(key).is_none_or(|t| t.is_empty()) {
        // Distinguish "nothing traded yet" from "this venue is not sending us trades" — the
        // second is a real condition on Binance USD-M and looks identical otherwise.
        let text = if state.books.contains_key(key) {
            "book is live but no trades have arrived"
        } else {
            "waiting for the instrument"
        };
        return note(text, p.text_muted);
    }

    v_flex()
        .child(
            h_flex()
                .h(px(ROW_H))
                .gap_2()
                .child(cell("time".into(), p.text_dim, 92.0))
                .child(cell("price".into(), p.text_dim, 96.0))
                .child(cell("size".into(), p.text_dim, 76.0))
                .child(cell("taker".into(), p.text_dim, 56.0)),
        )
        .children(state.tape.get(key).into_iter().flatten().take(TAPE_ROWS).map(|t| tape_row(t, cx)))
        .into_any_element()
}

fn tape_row(trade: &PublicTrade, cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    // Colour by aggressor: the side that crossed the spread is the information in a print.
    let color = match trade.taker_side {
        Side::Buy => p.green,
        Side::Sell => p.red,
    };

    h_flex()
        .h(px(ROW_H))
        .gap_2()
        .child(cell(clock::hms_millis(trade.ts.millis()), p.text_dim, 92.0))
        .child(cell(show(trade.price), color, 96.0))
        .child(cell(show(trade.qty), p.text, 76.0))
        .child(cell(
            match trade.taker_side {
                Side::Buy => "buy".into(),
                Side::Sell => "sell".into(),
            },
            color,
            56.0,
        ))
}

// ---------------------------------------------------------------- instruments

pub fn instruments(state: &FeedState, watching: &[String], cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    v_flex()
        .child(
            h_flex()
                .h(px(ROW_H))
                .gap_3()
                .child(cell("instrument".into(), p.text_dim, 190.0))
                .child(cell("bid".into(), p.text_dim, 150.0))
                .child(cell("ask".into(), p.text_dim, 150.0))
                .child(cell("spread".into(), p.text_dim, 100.0))
                .child(cell("trades".into(), p.text_dim, 80.0)),
        )
        .children(watching.iter().map(|key| {
            let label = key.strip_prefix("binance:").unwrap_or(key).to_string();
            let trades = state.trades.get(key).copied().unwrap_or_default();
            let book = state.books.get(key);
            let bid = book.and_then(|b| b.best_bid());
            let ask = book.and_then(|b| b.best_ask());

            h_flex()
                .h(px(ROW_H))
                .gap_3()
                // An instrument with no book stays listed and dim: absent is information.
                .child(cell(label, if book.is_some() { p.text } else { p.text_faint }, 190.0))
                .child(cell(level_text(bid), p.green, 150.0))
                .child(cell(level_text(ask), p.red, 150.0))
                .child(cell(
                    book.and_then(|b| b.spread()).map(show).unwrap_or_else(|| "—".into()),
                    p.text_muted,
                    100.0,
                ))
                .child(cell(trades.to_string(), p.text_muted, 80.0))
        }))
        .into_any_element()
}

fn level_text(level: Option<domain::BookLevel>) -> String {
    level.map(|l| format!("{}  × {}", show(l.price), show(l.qty))).unwrap_or_else(|| "—".into())
}

// ---------------------------------------------------------------- core status

pub fn core_status(state: &FeedState, core: &str, watching: &[String], cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    let synced = watching.iter().filter(|k| state.books.contains_key(*k)).count();

    let row = |label: &str, value: String, color: u32| {
        h_flex()
            .h(px(ROW_H))
            .gap_3()
            .child(cell(label.into(), p.text_dim, 150.0))
            .child(cell(value, color, 420.0))
    };

    v_flex()
        .child(row("core", core.to_string(), p.text))
        .child(row(
            "connection",
            state.status.label(),
            if state.status.is_live() { p.green } else { p.amber },
        ))
        .child(row("books", format!("{synced} of {} subscribed", watching.len()), p.text))
        .child(row("keys", "none on this machine — they live in the core".into(), p.text_muted))
        .child(row("last note", state.core_note.clone().unwrap_or_else(|| "—".into()), p.text_muted))
        .into_any_element()
}

// ------------------------------------------------------------------------ log

pub fn log(state: &FeedState, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    if state.log.is_empty() {
        return note("nothing logged yet", p.text_muted);
    }

    v_flex()
        .children(state.log.iter().take(LOG_ROWS).map(|line| {
            let color = match line.level {
                LogLevel::Info => p.text_muted,
                LogLevel::Warn => p.amber,
            };
            h_flex()
                .h(px(ROW_H))
                .gap_3()
                .child(cell(clock::hms_millis(line.at), p.text_dim, 96.0))
                .child(cell(line.text.clone(), color, 700.0))
        }))
        .into_any_element()
}

// ---------------------------------------------------------------------- chart

/// The tick plot: price against time, a mark per print, the book alongside it.
///
/// Laid out as three strips sharing one price scale — labels on the left, the plot, the
/// book heat on the right — plus a volume histogram and a time axis under all of them.
pub fn chart(state: &FeedState, key: &str, span_ms: i64, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    let prints: Vec<domain::PublicTrade> =
        state.tape.get(key).map(|t| t.iter().cloned().collect()).unwrap_or_default();

    if prints.is_empty() {
        let text = if state.books.contains_key(key) {
            "book is live but no prints have arrived — the plot is drawn from the tape"
        } else {
            "waiting for the instrument"
        };
        return v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(note(text, p.text_muted))
            .into_any_element();
    }

    let book = state.books.get(key);
    let plot = tick_plot(&prints, span_ms, book);
    let heat = book.map(|b| heatmap(b, plot.high, plot.low, 200)).unwrap_or_default();

    v_flex()
        .size_full()
        .child(
            h_flex()
                .w_full()
                .flex_1()
                .child(price_gutter(&plot, cx))
                .child(plot_area(&plot, cx))
                .child(heat_gutter(&heat, cx))
                .child(percent_gutter(&plot, cx)),
        )
        .child(
            h_flex()
                .w_full()
                .h(px(VOLUME_H))
                .child(div().w(px(AXIS_W)))
                .child(volume_area(&plot, cx))
                .child(div().w(px(HEAT_W + PCT_W))),
        )
        .child(
            h_flex()
                .w_full()
                .h(px(16.0))
                .child(div().w(px(AXIS_W)))
                .child(time_axis(&plot, cx))
                .child(div().w(px(HEAT_W + PCT_W))),
        )
        .into_any_element()
}

/// Price labels down the left, as on every trading chart.
fn price_gutter(plot: &TickPlot, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    div()
        .relative()
        .w(px(AXIS_W))
        .h_full()
        .children(plot.price_ticks.iter().map(|t| axis_label(t.y, show(t.price), p.text_dim, 400.0)))
        .children(plot.last_y.zip(plot.last).map(|(y, price)| axis_label(y, show(price), p.accent, 700.0)))
        .into_any_element()
}

/// Distance from the last price, outboard on the right.
fn percent_gutter(plot: &TickPlot, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    div()
        .relative()
        .w(px(PCT_W))
        .h_full()
        .children(
            plot.price_ticks
                .iter()
                // The tick that lands on the anchor is skipped: `0.00%` is printed there
                // instead, and two numbers in one place read worse than one.
                .filter(|t| t.percent.abs() > rust_decimal::Decimal::new(5, 3))
                .map(|t| axis_label(t.y, format!("{:+.2}%", t.percent), p.text_dim, 400.0)),
        )
        .children(plot.last_y.map(|y| axis_label(y, "0.00%".into(), p.accent, 700.0)))
        .into_any_element()
}

fn axis_label(y: f32, text: String, color: u32, weight: f32) -> AnyElement {
    div()
        .absolute()
        .top(relative(y))
        .right_0()
        .child(MoonText::new(text).font_size(10.0).mono(true).uppercase(false).weight(weight).color(color))
        .into_any_element()
}

fn plot_area(plot: &TickPlot, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    div()
        .relative()
        .flex_1()
        .h_full()
        .children(plot.price_ticks.iter().map(|t| gridline(t.y, p.row_line)))
        .children(plot.time_ticks.iter().map(|t| vertical_gridline(t.x, p.row_line)))
        // The anchor line last of the guides, so it sits over the grid rather than under it.
        .children(plot.last_y.map(|y| gridline(y, p.accent)))
        .children(plot.steps.iter().map(|s| step_segment(s, p.blue)))
        .children(plot.marks.iter().map(|m| print_mark(m, cx)))
        .into_any_element()
}

fn gridline(y: f32, color: u32) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(relative(y))
        .h(px(1.0))
        .bg(rgba((color << 8) | 0x55))
        .into_any_element()
}

fn vertical_gridline(x: f32, color: u32) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(relative(x))
        .w(px(1.0))
        .bg(rgba((color << 8) | 0x33))
        .into_any_element()
}

/// One flat run of the last-price line.
fn step_segment(step: &Step, color: u32) -> AnyElement {
    div()
        .absolute()
        .left(relative(step.x))
        .w(relative((step.to_x - step.x).max(0.0)))
        .top(relative(step.y))
        .h(px(1.5))
        .bg(rgba((color << 8) | 0xCC))
        .into_any_element()
}

/// One print. Size follows the fill, colour follows the aggressor.
fn print_mark(mark: &Mark, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    let color = if mark.buy { p.green } else { p.red };
    let size = MARK_MIN + (MARK_MAX - MARK_MIN) * mark.weight;

    div()
        .absolute()
        .left(relative(mark.x))
        .top(relative(mark.y))
        .w(px(size))
        .h(px(size))
        .rounded_sm()
        .bg(rgba((color << 8) | 0xDD))
        .into_any_element()
}

/// Depth alongside the plot, on the same price scale: asks above the touch, bids below.
fn heat_gutter(rows: &[HeatRow], cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    div()
        .relative()
        .w(px(HEAT_W))
        .h_full()
        .children(rows.iter().map(|row| {
            let color = if row.ask { p.red } else { p.green };
            div()
                .absolute()
                .left_0()
                .top(relative(row.y))
                .w(px(HEAT_W * row.fill))
                .h(px(2.0))
                .bg(rgba((color << 8) | 0xAA))
                .into_any_element()
        }))
        .into_any_element()
}

/// Traded size under the plot, split by which side crossed.
fn volume_area(plot: &TickPlot, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    div()
        .relative()
        .flex_1()
        .h_full()
        .children(plot.volume.iter().map(|bar| {
            let color = if bar.buy { p.green } else { p.red };
            div()
                .absolute()
                .left(relative(bar.x - bar.half_width))
                .w(relative(bar.half_width * 2.0))
                .bottom_0()
                .h(relative(bar.height))
                .bg(rgba((color << 8) | 0x99))
                .into_any_element()
        }))
        .into_any_element()
}

fn time_axis(plot: &TickPlot, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    div()
        .relative()
        .flex_1()
        .h_full()
        .children(plot.time_ticks.iter().map(|tick| {
            div()
                .absolute()
                .left(relative(tick.x))
                .child(
                    MoonText::new(clock::hms(tick.at_millis))
                        .font_size(10.0)
                        .mono(true)
                        .uppercase(false)
                        .color(p.text_dim),
                )
                .into_any_element()
        }))
        .into_any_element()
}

// --------------------------------------------------------------------- shared

fn waiting(state: &FeedState, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    note(
        match state.status {
            Status::Live => "subscribed — waiting for the book",
            _ => "not connected to a core",
        },
        p.text_muted,
    )
}

fn note(text: &str, color: u32) -> AnyElement {
    MoonText::new(text.to_string()).font_size(12.0).color(color).uppercase(false).into_any_element()
}

pub fn cell(text: String, color: u32, width: f32) -> AnyElement {
    div()
        .w(px(width))
        .child(MoonText::new(text).font_size(12.0).mono(true).uppercase(false).color(color))
        .into_any_element()
}

/// Venues pad prices to their full precision (`62958.64000000`); the trailing zeros are
/// noise on screen and make columns hard to compare at a glance.
pub fn show(value: Decimal) -> String {
    value.normalize().to_string()
}
