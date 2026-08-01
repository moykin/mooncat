//! The panel bodies.
//!
//! Kept apart from the shell so the chrome — frame, toolbar, dock, status bar — stays
//! readable as one piece of layout, and each panel stays a function of the feed state.

use crate::book_view::{depth_view, DepthView, Level};
use crate::chart::{chart_view, Bar, ChartView};
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
const AXIS_W: f32 = 74.0;
/// Width of the percent axis gutter.
const PCT_W: f32 = 58.0;
/// A body thinner than this is drawn as a line, so a doji stays visible.
const MIN_BODY: f32 = 0.004;

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

/// The price chart.
///
/// Bars are positioned in fractions of the pane rather than pixels, so the chart fills
/// whatever space the layout gives it without the geometry needing to know the size.
pub fn chart(state: &FeedState, key: &str, bars: usize, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    let series: Vec<domain::Candle> =
        state.candles.get(key).map(|s| s.iter().cloned().collect()).unwrap_or_default();

    if series.is_empty() {
        let text = if state.books.contains_key(key) {
            "no prints yet — the chart is built from the tape"
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

    let view = chart_view(&series, bars);

    h_flex()
        .size_full()
        .child(
            // The plot is a positioning context; every bar inside is placed against it.
            div()
                .relative()
                .flex_1()
                .h_full()
                .children(view.price_ticks.iter().map(|tick| gridline(tick.y, p.row_line)))
                // The anchor line, drawn last so it sits over the grid: it is the zero of
                // the percent axis and the only line worth picking out.
                .children(view.last_y.map(|y| gridline(y, p.accent)))
                .children(view.bars.iter().map(|bar| candle_marks(bar, cx)))
                .into_any_element(),
        )
        .child(price_axis(&view, cx))
        .child(percent_axis(&view, cx))
        .into_any_element()
}

/// Percent gutter, outboard of the price gutter.
///
/// Zero sits on the last traded price, so every label reads as "how far from where the
/// market is now" — the distance a scalper is actually sizing against.
fn percent_axis(view: &ChartView, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    let label = |y: f32, text: String, color: u32, weight: f32| {
        div()
            .absolute()
            .top(relative(y))
            .right_0()
            .child(
                MoonText::new(text).font_size(10.0).mono(true).uppercase(false).weight(weight).color(color),
            )
            .into_any_element()
    };

    div()
        .relative()
        .w(px(PCT_W))
        .h_full()
        .children(
            view.price_ticks
                .iter()
                // The anchor gets its own emphatic label below; skip the grid tick that
                // lands on top of it rather than printing two numbers in one place.
                .filter(|t| t.percent.abs() > rust_decimal::Decimal::new(5, 3))
                .map(|t| label(t.y, format!("{:+.2}%", t.percent), p.text_dim, 400.0)),
        )
        .children(view.last_y.map(|y| label(y, "0.00%".into(), p.accent, 700.0)))
        .into_any_element()
}

fn gridline(y: f32, color: u32) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(relative(y))
        .h(px(1.0))
        .bg(rgba((color << 8) | 0x66))
        .into_any_element()
}

/// One candle: the wick as a hairline, the body as a filled block.
fn candle_marks(bar: &Bar, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    let color = if bar.rising { p.green } else { p.red };

    let top = bar.open_y.min(bar.close_y);
    let height = (bar.open_y - bar.close_y).abs();

    let body = div()
        .absolute()
        .left(relative(bar.x - bar.half_width))
        .w(relative(bar.half_width * 2.0))
        .top(relative(top))
        .bg(rgba((color << 8) | 0xEE));

    // A candle that opened and closed at the same price has no body to draw; a hairline
    // keeps it on the chart instead of silently vanishing.
    let body = if height < MIN_BODY { body.h(px(1.5)) } else { body.h(relative(height)) };

    div()
        .absolute()
        .inset_0()
        .child(
            div()
                .absolute()
                .left(relative(bar.x))
                .w(px(1.0))
                .top(relative(bar.high_y))
                .h(relative(bar.low_y - bar.high_y))
                .bg(rgba((color << 8) | 0x99)),
        )
        .child(body)
        .into_any_element()
}

/// Price gutter down the right-hand edge, with the last price picked out.
fn price_axis(view: &ChartView, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);

    let label = |y: f32, text: String, color: u32| {
        div()
            .absolute()
            .top(relative(y))
            .right_0()
            .child(MoonText::new(text).font_size(10.0).mono(true).uppercase(false).color(color))
            .into_any_element()
    };

    div()
        .relative()
        .w(px(AXIS_W))
        .h_full()
        .children(view.price_ticks.iter().map(|t| label(t.y, show(t.price), p.text_dim)))
        .children(view.last.zip(view.last_y).map(|(price, y)| label(y, show(price), p.accent)))
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
