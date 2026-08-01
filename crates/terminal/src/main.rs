//! The desktop terminal.
//!
//! ```bash
//! MOON_TOKEN=… MOON_CORE=ws://127.0.0.1:8787 cargo run --bin moonterm -- BTCUSDT ETHUSDT
//! ```
//!
//! A window onto a core, nothing more: it holds no keys, keeps no venue connection, and can
//! be closed and reopened at will. The UI reads shared state on the frame tick and never
//! awaits — see [`feed`].
//!
//! The layout follows the shape a scalping terminal has settled into — instrument bar, size
//! presets, chart beside the book, a tabbed dock underneath, a status bar along the bottom.
//! That arrangement is a convention, not anyone's property; the code implementing it here is
//! ours, and the components it is drawn with are MoonUI under Apache-2.0.

use domain::{ExchangeId, MarketKind, Symbol};
use exchange::Subscription;
use gpui::prelude::*;
use gpui::{
    div, px, rgba, size, AnyElement, App, Bounds, Context, Entity, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use gpui_platform::application;
use moon_ui::{
    h_flex, v_flex, MoonBackgroundPolicy, MoonPalette, MoonStatusBar, MoonStatusIndicator, MoonStatusItem,
    MoonTabItem, MoonTabStrip, MoonText, MoonTheme, MoonThemeConfig, MoonTone, Root,
};
use std::time::Instant;

mod book_view;
mod chart;
mod clock;
mod feed;
mod panels;

use feed::{Feed, FeedState, Status};

const MARKETS: [MarketKind; 2] = [MarketKind::Spot, MarketKind::LinearPerp];

/// Order sizes offered on the toolbar, in quote currency.
const SIZE_PRESETS: [&str; 6] = ["50", "100", "250", "500", "1000", "2500"];

/// How much wall-clock time the plot covers. Zoom moves the window, not the data.
const SPANS: [(&str, i64); 5] =
    [("30s", 30_000), ("1m", 60_000), ("5m", 300_000), ("15m", 900_000), ("1h", 3_600_000)];

/// Dock tabs, in the order they appear.
const DOCK_TABS: [&str; 4] = ["Time & Sales", "Instruments", "Core Status", "Log"];

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
        let bounds = Bounds::centered(None, size(px(1320.0), px(860.0)), cx);

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
                    instrument: 0,
                    dock: 0,
                    size_preset: 1,
                    zoom: 1,
                    frames: 0,
                    fps: 0.0,
                    sampled_at: Instant::now(),
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
    /// Symbol keys in subscription order; the instrument bar indexes into this.
    watching: Vec<String>,
    instrument: usize,
    dock: usize,
    size_preset: usize,
    zoom: usize,
    frames: u32,
    fps: f32,
    sampled_at: Instant,
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pull on the frame tick rather than waking the UI per event. A busy instrument
        // produces hundreds of updates a second and the screen refreshes sixty times.
        window.request_animation_frame();
        self.sample_frame_rate();

        let state = self.feed.snapshot();
        let key = self.watching.get(self.instrument).cloned().unwrap_or_default();

        v_flex()
            .size_full()
            .child(self.top_bar(&state, &key, cx))
            .child(self.toolbar(&state, cx))
            .child(
                h_flex()
                    .flex_1()
                    .items_stretch()
                    .gap_2()
                    .px_2()
                    .child(self.chart_pane(&state, &key, cx))
                    .child(div().w(px(432.0)).h_full().child(pane(
                        panels::order_book(&state, &key, cx),
                        false,
                        cx,
                    ))),
            )
            .child(self.dock(&state, &key, cx))
            .child(self.status_bar(&state, cx))
    }
}

impl TerminalView {
    /// Frames per second, sampled once a second rather than smoothed per frame — a counter
    /// that jitters is worse than no counter.
    fn sample_frame_rate(&mut self) {
        self.frames += 1;
        let elapsed = self.sampled_at.elapsed();
        if elapsed.as_millis() >= 1_000 {
            self.fps = self.frames as f32 / elapsed.as_secs_f32();
            self.frames = 0;
            self.sampled_at = Instant::now();
        }
    }

    fn top_bar(&self, state: &FeedState, key: &str, cx: &App) -> impl IntoElement {
        let p = MoonPalette::active(cx);
        let mid = state.books.get(key).and_then(|b| b.mid());

        h_flex()
            .w_full()
            // The window's own controls are drawn over this row on macOS; start clear of them.
            .pl(px(84.0))
            .pr_4()
            .py_2()
            .gap_4()
            .items_center()
            .bg(rgba((p.chrome << 8) | 0xFF))
            .child(MoonText::new("moonterm").font_size(14.0).weight(700.0))
            .child(
                MoonText::new(self.core.clone())
                    .font_size(11.0)
                    .mono(true)
                    .uppercase(false)
                    .color(p.text_muted),
            )
            .child(div().flex_1())
            .child(
                MoonText::new(mid.map(panels::show).unwrap_or_else(|| "—".into()))
                    .font_size(13.0)
                    .mono(true)
                    .uppercase(false)
                    .color(p.text),
            )
            .child(
                MoonText::new(clock::hms(clock::now_millis()))
                    .font_size(13.0)
                    .mono(true)
                    .uppercase(false)
                    .color(p.text_muted),
            )
    }

    /// Instrument bar plus the order controls.
    ///
    /// The size, leverage and stop controls are live state but wired to nothing: order entry
    /// needs the OMS, which is not built. They are shown greyed rather than hidden so the
    /// layout is what it will be, and so nobody mistakes them for working buttons.
    fn toolbar(&self, state: &FeedState, cx: &mut Context<Self>) -> impl IntoElement {
        let p = MoonPalette::active(cx);

        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_4()
            .items_center()
            .flex_wrap()
            .bg(rgba((p.tabbar << 8) | 0xFF))
            .child(self.instrument_bar(state, cx))
            .child(div().flex_1())
            .child(MoonText::new("size, quote").font_size(10.0).color(p.text_dim).tracking(0.5))
            .child(self.size_presets(cx))
            .child(disabled_chip("lev ×1", p))
            .child(disabled_chip("sl —", p))
            .child(disabled_chip("tp —", p))
    }

    /// `MoonPresetStrip` is a display component with no click handling, so the strip is
    /// assembled from the same clickable chip the instrument bar uses.
    fn size_presets(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = MoonPalette::active(cx);

        h_flex().gap_1().children(SIZE_PRESETS.iter().enumerate().map(|(index, label)| {
            let selected = index == self.size_preset;

            div()
                .id(SharedString::from(format!("size-{label}")))
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgba(((if selected { p.accent } else { p.surface }) << 8) | 0xFF))
                .cursor_pointer()
                .child(
                    MoonText::new(label.to_string())
                        .font_size(11.0)
                        .mono(true)
                        .uppercase(false)
                        .color(if selected { p.accent_fg } else { p.text_muted }),
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.size_preset = index;
                    cx.notify();
                }))
        }))
    }

    fn instrument_bar(&self, state: &FeedState, cx: &mut Context<Self>) -> impl IntoElement {
        let p = MoonPalette::active(cx);

        h_flex().gap_2().flex_wrap().children(self.watching.iter().enumerate().map(|(index, key)| {
            let selected = index == self.instrument;
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
                        // Dim rather than hidden: a missing instrument must stay visible.
                        (false, true) => p.text,
                        (false, false) => p.text_faint,
                    },
                ))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.instrument = index;
                    cx.notify();
                }))
        }))
    }

    /// The plot, with its own span strip above it.
    ///
    /// The span moves the window over the tape; it does not change the data. Every print in
    /// range is drawn either way, so zooming out never silently aggregates anything away.
    fn chart_pane(&self, state: &FeedState, key: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let p = MoonPalette::active(cx);
        let (_, span_ms) = SPANS[self.zoom.min(SPANS.len() - 1)];
        let prints = state.tape.get(key).map_or(0, |t| t.len());

        div().flex_1().h_full().child(
            v_flex()
                .size_full()
                .p_3()
                .gap_2()
                .rounded_md()
                .bg(rgba((p.panel << 8) | 0xFF))
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(
                            MoonText::new(key.strip_prefix("binance:").unwrap_or(key).to_string())
                                .font_size(10.0)
                                .mono(true)
                                .uppercase(false)
                                .color(p.text_muted),
                        )
                        .child(
                            MoonText::new(format!("{prints} prints"))
                                .font_size(10.0)
                                .mono(true)
                                .uppercase(false)
                                .color(p.text_dim),
                        )
                        .child(div().flex_1())
                        .child(MoonText::new("span").font_size(10.0).color(p.text_dim).tracking(0.5))
                        .children(SPANS.iter().enumerate().map(|(index, (label, _))| {
                            let selected = index == self.zoom;
                            div()
                                .id(SharedString::from(format!("span-{label}")))
                                .px_2()
                                .rounded_sm()
                                .bg(rgba(((if selected { p.accent } else { p.surface }) << 8) | 0xFF))
                                .cursor_pointer()
                                .child(
                                    MoonText::new(label.to_string())
                                        .font_size(10.0)
                                        .mono(true)
                                        .uppercase(false)
                                        .color(if selected { p.accent_fg } else { p.text_muted }),
                                )
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.zoom = index;
                                    cx.notify();
                                }))
                        })),
                )
                .child(div().flex_1().w_full().child(panels::chart(state, key, span_ms, cx))),
        )
    }

    fn dock(&self, state: &FeedState, key: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let this: Entity<Self> = cx.entity();
        let selected = self.dock;

        let body: AnyElement = match self.dock {
            0 => panels::tape(state, key, cx),
            1 => panels::instruments(state, &self.watching, cx),
            2 => panels::core_status(state, &self.core, &self.watching, cx),
            _ => panels::log(state, cx),
        };

        v_flex()
            .h(px(268.0))
            .w_full()
            .px_2()
            .pb_2()
            .gap_1()
            .child(
                MoonTabStrip::new("moonterm-dock")
                    .items(DOCK_TABS.iter().enumerate().map(|(ix, title)| {
                        let item = MoonTabItem::new(*title).selected(ix == selected);
                        // A tab that carries a count says whether it is worth opening.
                        match ix {
                            0 => item.badge(state.tape.get(key).map_or(0, |t| t.len()).to_string()),
                            1 => item.badge(state.books.len().to_string()),
                            3 => item.badge(state.log.len().to_string()),
                            _ => item,
                        }
                    }))
                    .on_click(move |ix, _, _, app| {
                        this.update(app, |view, cx| {
                            view.dock = ix;
                            cx.notify();
                        });
                    })
                    .render(),
            )
            .child(pane(body, true, cx))
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
                MoonStatusItem::new(format!("books {}/{}", state.books.len(), self.watching.len()))
                    .tone(MoonTone::Info),
                MoonStatusItem::separator(),
                MoonStatusItem::new(format!("fps {:.0}", self.fps)).tone(MoonTone::Info),
            ])
            .right_item(MoonStatusItem::new("no keys on this machine").tone(MoonTone::Info))
    }
}

/// A framed content area. `grow` decides whether it takes the leftover space.
fn pane(body: AnyElement, grow: bool, cx: &App) -> AnyElement {
    let p = MoonPalette::active(cx);
    let panel = div().h_full().w_full().p_3().rounded_md().bg(rgba((p.panel << 8) | 0xFF)).child(body);

    if grow { div().flex_1().h_full().child(panel) } else { div().h_full().child(panel) }.into_any_element()
}

/// A control that is part of the layout but not yet wired to anything.
fn disabled_chip(label: &str, p: MoonPalette) -> impl IntoElement {
    div().px_3().py_1().rounded_md().bg(rgba((p.surface << 8) | 0x88)).child(
        MoonText::new(label.to_string()).font_size(11.0).mono(true).uppercase(false).color(p.text_faint),
    )
}
