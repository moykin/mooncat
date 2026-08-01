//! The panel bodies.
//!
//! Kept apart from the shell so the chrome — frame, toolbar, dock, status bar — stays
//! readable as one piece of layout, and each panel stays a function of the feed state.

use crate::book_view::{depth_view, DepthView, Level};
use crate::clock;
use crate::feed::{FeedState, LogLevel, Status};
use domain::{Decimal, PublicTrade, Side};
use gpui::prelude::*;
use gpui::{div, px, rgba, AnyElement, App};
use moon_ui::{h_flex, v_flex, MoonPalette, MoonText};

/// Price levels shown on each side of the book.
pub const DEPTH_ROWS: usize = 16;
/// Trades shown in the tape.
pub const TAPE_ROWS: usize = 40;
/// Log lines shown at once.
pub const LOG_ROWS: usize = 60;

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

/// Where the chart will go.
///
/// A labelled empty area rather than a blank one: an unbuilt feature and a broken one look
/// the same on screen, and only one of them deserves a bug report.
pub fn chart_placeholder(state: &FeedState, key: &str, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    let mid = state.books.get(key).and_then(|b| b.mid());

    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(
            MoonText::new(mid.map(show).unwrap_or_else(|| "—".into()))
                .font_size(34.0)
                .mono(true)
                .uppercase(false)
                .color(p.text),
        )
        .child(
            MoonText::new(key.strip_prefix("binance:").unwrap_or(key).to_string())
                .font_size(12.0)
                .mono(true)
                .uppercase(false)
                .color(p.text_muted),
        )
        .child(
            MoonText::new("tick chart is not built yet").font_size(11.0).uppercase(false).color(p.text_faint),
        )
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
