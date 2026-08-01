//! Live spot **and** USD-M perpetual data for one ticker, in one process, with no API keys
//! and no licence.
//!
//! ```bash
//! cargo run -p binance --example live_feed            # BTCUSDT
//! cargo run -p binance --example live_feed -- ETHUSDT
//! ```
//!
//! Note how little this does: it holds books and prints them. Snapshot fetching, gap
//! recovery and resubscription all live in the connector now, so every consumer — this
//! demo, the terminal, a strategy — gets a book that is either correct or absent.

use domain::{
    ApplyOutcome, Event, ExchangeId, MarketEvent, MarketKind, OrderBook, Payload, PublicTrade, Symbol,
};
use exchange::{MarketDataSource, Subscription};
use std::collections::HashMap;
use std::time::Duration;

const MARKETS: [MarketKind; 2] = [MarketKind::Spot, MarketKind::LinearPerp];

/// A subscribed stream that has never produced an event is the dangerous case: the socket
/// is up, the book is moving, and nothing looks wrong. Say so out loud instead.
const SILENT_STREAM_AFTER: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("warn,binance=info").init();

    let ticker = std::env::args().nth(1).unwrap_or_else(|| "BTCUSDT".into());
    let (sink, mut events) = exchange::event_channel();

    // One sink, two connectors. This is the whole point of the design: spot and futures are
    // two subscriptions on one bus, not two copies of a program.
    let mut watching: Vec<String> = Vec::new();
    let mut _connectors = Vec::new();

    for market in MARKETS {
        let connector = binance::Binance::connect(market, sink.clone())?;
        let symbol = Symbol::new(ExchangeId::Binance, market, &ticker);

        let instruments = connector.instruments(market).await?;
        match instruments.iter().find(|i| i.symbol == symbol) {
            Some(i) => println!(
                "{:<12} {} instruments · {ticker}: tick {} step {} min-notional {}",
                market.to_string(),
                instruments.len(),
                i.tick_size,
                i.step_size,
                i.min_notional
            ),
            None => {
                println!("{:<12} {ticker} is not listed, skipping", market.to_string());
                continue;
            }
        }

        connector
            .subscribe(&[Subscription::Trades(symbol.clone()), Subscription::Book(symbol.clone())])
            .await?;
        watching.push(symbol.key());
        _connectors.push(connector);
    }

    if watching.is_empty() {
        return Err(format!("{ticker} is listed on neither market").into());
    }
    watching.sort();
    println!("\nstreaming… ctrl-c to stop\n");

    let mut books: HashMap<String, OrderBook> = HashMap::new();
    let mut last_trade: HashMap<String, PublicTrade> = HashMap::new();
    let mut trades: HashMap<String, u64> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let started = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = tick.tick() => print_rows(&watching, &books, &last_trade, &trades, started.elapsed()),
            Some(event) = events.recv() => handle(event, &mut books, &mut last_trade, &mut trades),
            else => break,
        }
    }
    Ok(())
}

fn handle(
    event: Event,
    books: &mut HashMap<String, OrderBook>,
    last_trade: &mut HashMap<String, PublicTrade>,
    trades: &mut HashMap<String, u64>,
) {
    match event.payload {
        Payload::Market(MarketEvent::Trade(trade)) => {
            *trades.entry(trade.symbol.key()).or_default() += 1;
            last_trade.insert(trade.symbol.key(), trade);
        }

        // The connector publishes a snapshot only once it has joined the delta stream, so
        // this is always a safe reset point.
        Payload::Market(MarketEvent::BookSnapshot(book)) => {
            if let Some(symbol) = book.symbol.clone() {
                books.insert(symbol.key(), book);
            }
        }

        Payload::Market(MarketEvent::BookDelta(delta)) => {
            if let Some(book) = books.get_mut(&delta.symbol.key()) {
                // Only chainable updates are forwarded, so anything else means the
                // connector and this consumer have diverged — worth shouting about.
                if let ApplyOutcome::Gap { expected, got } = book.apply(&delta) {
                    eprintln!("{}: forwarded delta did not chain ({expected} vs {got})", delta.symbol);
                }
            }
        }

        Payload::Connection(state) => println!("· {state:?}"),
        _ => {}
    }
}

fn print_rows(
    watching: &[String],
    books: &HashMap<String, OrderBook>,
    last_trade: &HashMap<String, PublicTrade>,
    trades: &HashMap<String, u64>,
    elapsed: Duration,
) {
    for key in watching {
        let label = key.strip_prefix("binance:").unwrap_or(key);
        let count = trades.get(key).copied().unwrap_or_default();

        let Some(book) = books.get(key).filter(|b| b.best_bid().is_some() && b.best_ask().is_some()) else {
            println!("{label:<28} book: syncing…");
            continue;
        };
        if count == 0 && elapsed > SILENT_STREAM_AFTER {
            println!(
                "{label:<28} ⚠ book is live but the trade stream has sent nothing in {}s",
                elapsed.as_secs()
            );
        }

        let (bid, ask) = (book.best_bid().unwrap(), book.best_ask().unwrap());
        let last = last_trade
            .get(key)
            .map(|t| format!("{} {:?}", t.price, t.taker_side))
            .unwrap_or_else(|| "—".into());

        println!(
            "{label:<28} bid {:>12} × {:<10} ask {:>12} × {:<10} spread {:<10} last {last:<16} trades {count}",
            bid.price,
            bid.qty,
            ask.price,
            ask.qty,
            book.spread().unwrap_or_default(),
        );
    }
    println!();
}
