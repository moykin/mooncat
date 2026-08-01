//! The desktop terminal.
//!
//! ```bash
//! MOON_TOKEN=… MOON_CORE=ws://127.0.0.1:8787 cargo run --bin moonterm -- BTCUSDT ETHUSDT
//! ```
//!
//! A window onto a core, nothing more: it holds no keys, keeps no venue connection, and can
//! be closed and reopened at will. The UI reads shared state on the frame tick and never
//! awaits — see [`feed`].

use domain::{Decimal, ExchangeId, MarketKind, PublicTrade, Side, Symbol};
use exchange::Subscription;
use gpui::prelude::*;
use gpui::{
    div, px, rgba, size, AnyElement, App, Bounds, Context, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use gpui_platform::application;
use moon_ui::{
    h_flex, v_flex, MoonBackgroundPolicy, MoonPalette, MoonStatusBar, MoonStatusIndicator, MoonStatusItem,
    MoonText, MoonTheme, MoonThemeConfig, MoonTone, Root,
};

mod book_view;
mod feed;

use book_view::{depth_view, DepthView, Level};
use feed::{Feed, FeedState, Status};

const MARKETS: [MarketKind; 2] = [MarketKind::Spot, MarketKind::LinearPerp];

/// Price levels shown on each side of the book.
const DEPTH_ROWS: usize = 14;
/// Trades shown in the tape.
const TAPE_ROWS: usize = 30;

const ROW_H: f32 = 18.0;
const BAR_W: f32 = 90.0;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();

    let Ok(token) = std::env::var("MOON_TOKEN") else {
        eprintln!("set MOON_TOKEN to the core's token");
        std::process::exit(1);
    };
    let url = std::env::var("MOON_CORE").unwrap_or_else(|_| "ws://127.0.0.1:8787".into());
    let tickers: Vec<String> = match std::env::args().skip(1).collect::<Vec<_>>() {
        empty if empty.is_empty() => vec!["BTCUSDT".into()],
        given => given.into_iter().map(|t| t.to_uppercase()).collect(),
    };

    let symbols: Vec<Symbol> = tickers
        .iter()
        .flat_map(|ticker| {
            MARKETS.iter().map(move |market| Symbol::new(ExchangeId::Binance, *market, ticker))
        })
        .collect();
    let watching: Vec<String> = symbols.iter().map(Symbol::key).collect();
    let subs: Vec<Subscription> = symbols
        .into_iter()
        .flat_map(|symbol| [Subscription::Trades(symbol.clone()), Subscription::Book(symbol)])
        .collect();

    let feed = Feed::spawn(url.clone(), token, subs);

    application().with_assets(moon_ui::MoonAssets).run(move |cx: &mut App| {
        moon_ui::foundation::init(cx);
        MoonTheme::install_config(MoonThemeConfig::moon_terminal(), cx);

        let palette = MoonPalette::active(cx);
        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("moonterm")),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                window_clear_color: Some(rgba((palette.shell << 8) | 0xFF)),
                app_id: Some("own.moon.terminal".to_string()),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|_| TerminalView {
                    feed: feed.clone(),
                    core: url.clone(),
                    watching: watching.clone(),
                    selected: 0,
                });
                cx.new(|cx| {
                    Root::new(view, window, cx)
                        .background_policy(MoonBackgroundPolicy::Opaque)
                        .background(MoonPalette::active(cx).shell)
                })
            },
        )
        .expect("open the terminal window");

        cx.activate(true);
    });
}

struct TerminalView {
    feed: Feed,
    core: String,
    /// Symbol keys in subscription order; the tab strip and selection index into this.
    watching: Vec<String>,
    selected: usize,
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pull on the frame tick rather than waking the UI per event. A busy instrument
        // produces hundreds of updates a second and the screen refreshes sixty times.
        window.request_animation_frame();

        let state = self.feed.snapshot();
        let key = self.watching.get(self.selected).cloned().unwrap_or_default();

        v_flex()
            .size_full()
            .child(self.header(&state, cx))
            .child(self.tabs(&state, cx))
            .child(
                h_flex()
                    .flex_1()
                    .items_start()
                    .gap_4()
                    .px_4()
                    .pb_2()
                    .child(self.depth_panel(&state, &key, cx))
                    .child(self.tape_panel(&state, &key, cx)),
            )
            .child(self.note(&state, cx))
            .child(self.status_bar(&state, cx))
    }
}

impl TerminalView {
    fn header(&self, state: &FeedState, cx: &App) -> impl IntoElement {
        let p = MoonPalette::active(cx);
        h_flex()
            .w_full()
            // The window's own controls are drawn over this row on macOS; start clear of them.
            .pl(px(84.0))
            .pr_4()
            .py_3()
            .gap_3()
            .justify_between()
            .child(MoonText::new("moonterm").font_size(15.0).weight(700.0))
            .child(MoonText::new(self.core.clone()).font_size(12.0).color(p.text_muted).uppercase(false))
            .child(MoonText::new(state.status.label()).font_size(12.0).color(if state.status.is_live() {
                p.green
            } else {
                p.amber
            }))
    }

    /// One tab per subscribed instrument. Tabs whose book has not arrived stay dim rather
    /// than vanishing, so a missing instrument is visible instead of merely absent.
    fn tabs(&self, state: &FeedState, cx: &mut Context<Self>) -> impl IntoElement {
        let p = MoonPalette::active(cx);

        h_flex().w_full().px_4().pb_2().gap_2().flex_wrap().children(self.watching.iter().enumerate().map(
            |(index, key)| {
                let selected = index == self.selected;
                let has_book = state.books.contains_key(key);
                let label = key.strip_prefix("binance:").unwrap_or(key).to_string();

                div()
                    .id(SharedString::from(key.clone()))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgba(((if selected { p.accent } else { p.surface }) << 8) | 0xFF))
                    .cursor_pointer()
                    .child(MoonText::new(label).font_size(12.0).mono(true).uppercase(false).color(
                        match (selected, has_book) {
                            (true, _) => p.accent_fg,
                            (false, true) => p.text,
                            (false, false) => p.text_faint,
                        },
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = index;
                        cx.notify();
                    }))
            },
        ))
    }

    fn depth_panel(&self, state: &FeedState, key: &str, cx: &App) -> AnyElement {
        let p = MoonPalette::active(cx);

        let Some(book) = state.books.get(key) else {
            return panel("order book", missing(state, p.text_muted), cx).into_any_element();
        };
        let view = depth_view(book, DEPTH_ROWS);

        let body = v_flex()
            .child(depth_header(cx))
            .children(view.asks.iter().map(|l| depth_row(l, p.red, p.red_soft_bd, cx)))
            .child(spread_row(&view, cx))
            .children(view.bids.iter().map(|l| depth_row(l, p.green, p.green_btn, cx)));

        panel("order book", body.into_any_element(), cx).into_any_element()
    }

    fn tape_panel(&self, state: &FeedState, key: &str, cx: &App) -> AnyElement {
        let p = MoonPalette::active(cx);

        if state.tape.get(key).is_none_or(|t| t.is_empty()) {
            // Distinguish "nothing traded yet" from "this venue is not sending us trades" —
            // the second is a real condition on Binance USD-M and looks identical otherwise.
            let text = if state.books.contains_key(key) {
                "book is live but no trades have arrived"
            } else {
                "waiting for the instrument"
            };
            let note = MoonText::new(text).font_size(12.0).color(p.text_muted).uppercase(false);
            return panel("time & sales", note.into_any_element(), cx).into_any_element();
        }

        let rows = state
            .tape
            .get(key)
            .into_iter()
            .flatten()
            .take(TAPE_ROWS)
            .map(|trade| tape_row(trade, cx))
            .collect::<Vec<_>>();

        panel("time & sales", v_flex().child(tape_header(cx)).children(rows).into_any_element(), cx)
            .into_any_element()
    }

    fn note(&self, state: &FeedState, cx: &App) -> impl IntoElement {
        let p = MoonPalette::active(cx);
        div().px_4().pb_1().children(
            state
                .core_note
                .clone()
                .map(|n| MoonText::new(n).font_size(11.0).color(p.text_muted).uppercase(false)),
        )
    }

    fn status_bar(&self, state: &FeedState, cx: &App) -> impl IntoElement {
        let p = MoonPalette::active(cx);
        let (dot, tone) = match state.status {
            Status::Live => (p.green, MoonTone::Positive),
            Status::Refused(_) => (p.red, MoonTone::Negative),
            _ => (p.amber, MoonTone::Info),
        };

        MoonStatusBar::new("moonterm-status")
            .indicator(MoonStatusIndicator::new(dot).glow(8.0, 0.28))
            .items([
                MoonStatusItem::new(state.status.label()).tone(tone),
                MoonStatusItem::separator(),
                MoonStatusItem::new(format!("{} of {} books", state.books.len(), self.watching.len()))
                    .tone(MoonTone::Info),
            ])
            .right_item(MoonStatusItem::new("no keys on this machine").tone(MoonTone::Info))
    }
}

// ------------------------------------------------------------------- pieces

fn panel(title: &str, body: AnyElement, cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    v_flex()
        .flex_1()
        .p_3()
        .gap_2()
        .rounded_md()
        .bg(rgba((p.panel << 8) | 0xFF))
        .child(MoonText::new(title.to_string()).font_size(11.0).color(p.text_dim).tracking(0.6))
        .child(body)
}

fn missing(state: &FeedState, color: u32) -> AnyElement {
    MoonText::new(match state.status {
        Status::Live => "subscribed — waiting for the book",
        _ => "not connected to a core",
    })
    .font_size(12.0)
    .color(color)
    .uppercase(false)
    .into_any_element()
}

fn cell(text: String, color: u32, width: f32) -> AnyElement {
    div()
        .w(px(width))
        .child(MoonText::new(text).font_size(12.0).mono(true).uppercase(false).color(color))
        .into_any_element()
}

fn depth_header(cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    h_flex()
        .h(px(ROW_H))
        .gap_2()
        .child(cell("price".into(), p.text_dim, 96.0))
        .child(cell("size".into(), p.text_dim, 78.0))
        .child(cell("total".into(), p.text_dim, 82.0))
        .child(cell("% mid".into(), p.text_dim, 62.0))
}

fn depth_row(level: &Level, text: u32, bar: u32, cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    h_flex()
        .h(px(ROW_H))
        .gap_2()
        .child(cell(show(level.price), text, 96.0))
        .child(cell(show(level.qty), p.text, 78.0))
        .child(cell(show(level.cumulative), p.text_muted, 82.0))
        .child(cell(format!("{:+.3}", level.from_mid_pct), p.text_muted, 62.0))
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
        .h(px(ROW_H + 6.0))
        .w_full()
        .items_center()
        .child(MoonText::new(text).font_size(12.0).mono(true).uppercase(false).color(p.accent))
}

fn tape_header(cx: &App) -> impl IntoElement {
    let p = MoonPalette::active(cx);
    h_flex()
        .h(px(ROW_H))
        .gap_2()
        .child(cell("price".into(), p.text_dim, 96.0))
        .child(cell("size".into(), p.text_dim, 78.0))
        .child(cell("taker".into(), p.text_dim, 56.0))
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
        .child(cell(show(trade.price), color, 96.0))
        .child(cell(show(trade.qty), p.text, 78.0))
        .child(cell(
            match trade.taker_side {
                Side::Buy => "buy".into(),
                Side::Sell => "sell".into(),
            },
            color,
            56.0,
        ))
}

/// Venues pad prices to their full precision (`62958.64000000`); the trailing zeros are
/// noise on screen and make columns hard to compare at a glance.
fn show(value: Decimal) -> String {
    value.normalize().to_string()
}
