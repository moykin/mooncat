//! The desktop terminal.
//!
//! ```bash
//! MOON_TOKEN=… MOON_CORE=ws://127.0.0.1:8787 cargo run -p terminal --bin moonterm -- BTCUSDT
//! ```
//!
//! A window onto a core, nothing more: it holds no keys, keeps no venue connection, and can
//! be closed and reopened at will. The UI reads shared state on the frame tick and never
//! awaits — see [`feed`].

use domain::{ExchangeId, MarketKind, Symbol};
use exchange::Subscription;
use gpui::prelude::*;
use gpui::{
    div, px, rgba, size, App, Bounds, Context, SharedString, TitlebarOptions, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;
use moon_ui::{
    h_flex, v_flex, MoonBackgroundPolicy, MoonPalette, MoonStatusBar, MoonStatusIndicator, MoonStatusItem,
    MoonText, MoonTheme, MoonThemeConfig, MoonTone, Root,
};

mod feed;

use feed::{Feed, FeedState, Status};

const MARKETS: [MarketKind; 2] = [MarketKind::Spot, MarketKind::LinearPerp];

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();

    let token = match std::env::var("MOON_TOKEN") {
        Ok(t) => t,
        Err(_) => {
            eprintln!("set MOON_TOKEN to the core's token");
            std::process::exit(1);
        }
    };
    let url = std::env::var("MOON_CORE").unwrap_or_else(|_| "ws://127.0.0.1:8787".into());
    let tickers: Vec<String> = match std::env::args().skip(1).collect::<Vec<_>>() {
        empty if empty.is_empty() => vec!["BTCUSDT".into()],
        given => given.into_iter().map(|t| t.to_uppercase()).collect(),
    };

    let subs: Vec<Subscription> = tickers
        .iter()
        .flat_map(|ticker| {
            MARKETS.iter().flat_map(move |market| {
                let symbol = Symbol::new(ExchangeId::Binance, *market, ticker);
                [Subscription::Trades(symbol.clone()), Subscription::Book(symbol)]
            })
        })
        .collect();

    let feed = Feed::spawn(url.clone(), token, subs);

    application().with_assets(moon_ui::MoonAssets).run(move |cx: &mut App| {
        moon_ui::foundation::init(cx);
        MoonTheme::install_config(MoonThemeConfig::moon_terminal(), cx);

        let palette = MoonPalette::active(cx);
        let bounds = Bounds::centered(None, size(px(1100.0), px(680.0)), cx);

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
                let view = cx.new(|_| TerminalView { feed: feed.clone(), core: url.clone() });
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
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pull on the frame tick rather than waking the UI per event. A busy instrument
        // produces hundreds of updates a second and the screen refreshes sixty times.
        window.request_animation_frame();

        let state = self.feed.snapshot();
        let palette = MoonPalette::active(cx);

        v_flex()
            .size_full()
            .child(self.header(&state, cx))
            .child(
                v_flex()
                    .flex_1()
                    .p_4()
                    .gap_2()
                    .children(self.book_rows(&state, cx))
                    .child(self.note(&state, palette.text_muted)),
            )
            .child(self.status_bar(&state, cx))
    }
}

impl TerminalView {
    fn header(&self, state: &FeedState, cx: &App) -> impl IntoElement {
        let palette = MoonPalette::active(cx);
        h_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_3()
            .justify_between()
            .child(MoonText::new("moonterm").font_size(15.0).weight(700.0))
            .child(MoonText::new(self.core.clone()).font_size(12.0).color(palette.text_muted))
            .child(MoonText::new(state.status.label()).font_size(12.0).color(if state.status.is_live() {
                palette.green
            } else {
                palette.amber
            }))
    }

    /// One row per instrument. Books that have not attached yet are shown as such rather
    /// than omitted, so an empty screen is never ambiguous.
    fn book_rows(&self, state: &FeedState, cx: &App) -> Vec<gpui::AnyElement> {
        let palette = MoonPalette::active(cx);

        if state.books.is_empty() {
            return vec![MoonText::new(match state.status {
                Status::Live => "subscribed — waiting for the first book",
                _ => "not connected to a core",
            })
            .color(palette.text_muted)
            .into_any_element()];
        }

        state
            .books
            .iter()
            .map(|(key, book)| {
                let label = key.strip_prefix("binance:").unwrap_or(key).to_string();
                let trades = state.trades.get(key).copied().unwrap_or_default();

                let (bid, ask) = (book.best_bid(), book.best_ask());
                let cell = |text: String, color: u32| {
                    MoonText::new(text).font_size(13.0).color(color).mono(true).into_any_element()
                };

                h_flex()
                    .w_full()
                    .gap_6()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgba((palette.surface << 8) | 0xFF))
                    .child(div().w(px(190.0)).child(cell(label, palette.text)))
                    .child(div().w(px(170.0)).child(cell(
                        bid.map(|l| format!("{}  × {}", l.price, l.qty)).unwrap_or_else(|| "—".into()),
                        palette.green,
                    )))
                    .child(div().w(px(170.0)).child(cell(
                        ask.map(|l| format!("{}  × {}", l.price, l.qty)).unwrap_or_else(|| "—".into()),
                        palette.red,
                    )))
                    .child(div().w(px(110.0)).child(cell(
                        book.spread().map(|s| format!("spread {s}")).unwrap_or_default(),
                        palette.text_muted,
                    )))
                    .child(cell(format!("{trades} trades"), palette.text_muted))
                    .into_any_element()
            })
            .collect()
    }

    fn note(&self, state: &FeedState, color: u32) -> impl IntoElement {
        div()
            .pt_2()
            .children(state.core_note.clone().map(|note| MoonText::new(note).font_size(12.0).color(color)))
    }

    fn status_bar(&self, state: &FeedState, cx: &App) -> impl IntoElement {
        let palette = MoonPalette::active(cx);
        let (dot, tone) = match state.status {
            Status::Live => (palette.green, MoonTone::Positive),
            Status::Refused(_) => (palette.red, MoonTone::Negative),
            _ => (palette.amber, MoonTone::Info),
        };

        MoonStatusBar::new("moonterm-status")
            .indicator(MoonStatusIndicator::new(dot).glow(8.0, 0.28))
            .items([
                MoonStatusItem::new(state.status.label()).tone(tone),
                MoonStatusItem::separator(),
                MoonStatusItem::new(format!("{} books", state.books.len())).tone(MoonTone::Info),
            ])
            .right_item(MoonStatusItem::new("no keys on this machine").tone(MoonTone::Info))
    }
}
