//! The trading core.
//!
//! Runs on the VPS next to the exchange: owns the connectors, owns the books, and will own
//! the API keys. Terminals connect over [`wire`] and receive events; they hold no secrets
//! and can disappear at any time without the core noticing.
//!
//! ```bash
//! export MOON_TOKEN=$(openssl rand -hex 24)
//! cargo run -p core --bin mooncore -- BTCUSDT ETHUSDT
//! ```

use domain::{ExchangeId, Symbol};
use exchange::{MarketDataSource, Subscription};
use std::sync::Arc;
use tokio::net::TcpListener;
use wire::Auth;

mod config;
mod server;
mod state;

use config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env(std::env::args().skip(1).collect())?;
    let auth = Arc::new(Auth::new(config.token.clone()));
    let fanout = server::fanout();
    let state = state::MarketState::default();

    // Connectors publish into a plain channel; one pump forwards into the broadcast so that
    // a terminal that disconnects mid-event cannot stall a venue reader.
    let (sink, mut events) = exchange::event_channel();
    let mut connectors = Vec::new();

    for market in &config.markets {
        let connector = binance::Binance::connect(*market, sink.clone())?;
        let subs: Vec<Subscription> = config
            .tickers
            .iter()
            .flat_map(|ticker| {
                let symbol = Symbol::new(ExchangeId::Binance, *market, ticker);
                [Subscription::Trades(symbol.clone()), Subscription::Book(symbol)]
            })
            .collect();

        connector.subscribe(&subs).await?;
        tracing::info!(market = %market, tickers = ?config.tickers, "streaming");
        connectors.push(connector);
    }
    keep_alive(connectors);

    let (forward, folding) = (fanout.clone(), state.clone());
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            // Fold before broadcasting, so a session subscribing at this instant is served
            // state that is at least as new as the events already queued for it.
            folding.apply(&event);
            // An error here only means no terminal is connected, which is normal.
            let _ = forward.send(event);
        }
        tracing::warn!("connector event stream ended — no further market data");
    });

    let listener = TcpListener::bind(config.bind).await?;
    tracing::info!(bind = %config.bind, "core is up");
    tokio::select! {
        _ = server::serve(listener, auth, fanout, state) => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}

/// Connectors stop their socket tasks when dropped, so the core must hold on to them for
/// as long as it runs.
fn keep_alive(connectors: Vec<binance::Binance>) {
    std::mem::forget(connectors);
}
