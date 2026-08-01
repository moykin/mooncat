//! Stage 1 milestone: live spot **and** USD-M perpetual data for one ticker, in one
//! process, with no API keys and no licence.
//!
//! ```bash
//! cargo run -p binance --example live_feed            # BTCUSDT
//! cargo run -p binance --example live_feed -- ETHUSDT
//! ```
//!
//! It also exercises the part that is easy to get wrong and invisible when you do: the book
//! is rebuilt from a REST snapshot plus a delta stream, and a sequence gap forces a resync
//! instead of quietly diverging from the venue.

use domain::{
    ApplyOutcome, Event, ExchangeId, MarketEvent, MarketKind, OrderBook, Payload, PublicTrade, Symbol,
};
use exchange::{MarketDataSource, Subscription};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const MARKETS: [MarketKind; 2] = [MarketKind::Spot, MarketKind::LinearPerp];
const DEPTH: u32 = 20;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("warn,binance=info").init();

    let ticker = std::env::args().nth(1).unwrap_or_else(|| "BTCUSDT".into());
    let (sink, mut events) = exchange::event_channel();

    // One sink, two connectors. This is the whole point of the design: spot and futures are
    // two subscriptions on one bus, not two copies of a program.
    let mut connectors = HashMap::new();
    for market in MARKETS {
        let connector = Arc::new(binance::Binance::connect(market, sink.clone())?);
        let symbol = Symbol::new(ExchangeId::Binance, market, &ticker);

        let instruments = connector.instruments(market).await?;
        let listed = instruments.iter().find(|i| i.symbol == symbol);
        match listed {
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
        connectors.insert(symbol.key(), (connector, symbol));
    }

    if connectors.is_empty() {
        return Err(format!("{ticker} is listed on neither market").into());
    }
    println!("\nstreaming… ctrl-c to stop\n");

    let mut books: HashMap<String, OrderBook> = HashMap::new();
    let mut last_trade: HashMap<String, PublicTrade> = HashMap::new();
    let mut counts: HashMap<String, (u64, u64)> = HashMap::new(); // (trades, resyncs)
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let started = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = tick.tick() => print_row(&connectors, &books, &last_trade, &counts, started.elapsed()),
            Some(event) = events.recv() => {
                handle(event, &connectors, &mut books, &mut last_trade, &mut counts).await;
            }
            else => break,
        }
    }
    Ok(())
}

type Connectors = HashMap<String, (Arc<binance::Binance>, Symbol)>;

async fn handle(
    event: Event,
    connectors: &Connectors,
    books: &mut HashMap<String, OrderBook>,
    last_trade: &mut HashMap<String, PublicTrade>,
    counts: &mut HashMap<String, (u64, u64)>,
) {
    match event.payload {
        Payload::Market(MarketEvent::Trade(trade)) => {
            counts.entry(trade.symbol.key()).or_default().0 += 1;
            last_trade.insert(trade.symbol.key(), trade);
        }

        Payload::Market(MarketEvent::BookDelta(delta)) => {
            let key = delta.symbol.key();
            let Some((connector, symbol)) = connectors.get(&key) else { return };

            // First delta for this symbol: seed from a REST snapshot. Deltas older than the
            // snapshot are dropped by `apply` as stale, which is the normal overlap.
            if !books.contains_key(&key) {
                match connector.book_snapshot(symbol, DEPTH).await {
                    Ok(book) => {
                        books.insert(key.clone(), book);
                    }
                    Err(e) => {
                        eprintln!("{key}: snapshot failed: {e}");
                        return;
                    }
                }
            }

            let book = books.get_mut(&key).expect("seeded above");
            if let ApplyOutcome::Gap { expected, got } = book.apply(&delta) {
                // The venue skipped updates. Anything we render from here on would be a
                // fiction, so drop the book and rebuild it.
                eprintln!("{key}: sequence gap (expected {expected}, got {got}) — resyncing");
                counts.entry(key.clone()).or_default().1 += 1;
                books.remove(&key);
            }
        }

        Payload::Connection(state) => println!("· {state:?}"),
        _ => {}
    }
}

/// A subscribed stream that has never produced an event is the dangerous case: the socket
/// is up, the book is moving, and nothing looks wrong. Say so out loud instead.
const SILENT_STREAM_AFTER: Duration = Duration::from_secs(15);

fn print_row(
    connectors: &Connectors,
    books: &HashMap<String, OrderBook>,
    last_trade: &HashMap<String, PublicTrade>,
    counts: &HashMap<String, (u64, u64)>,
    elapsed: Duration,
) {
    let mut keys: Vec<&String> = connectors.keys().collect();
    keys.sort();

    for key in keys {
        let label = key.strip_prefix("binance:").unwrap_or(key);
        let (trades, resyncs) = counts.get(key).copied().unwrap_or_default();

        let book = match books.get(key) {
            Some(b) if b.best_bid().is_some() && b.best_ask().is_some() => b,
            _ => {
                println!("{label:<28} book: syncing…");
                continue;
            }
        };

        if trades == 0 && elapsed > SILENT_STREAM_AFTER {
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
            "{label:<28} bid {:>12} × {:<10} ask {:>12} × {:<10} spread {:<10} last {last:<16} trades {trades:<7} resyncs {resyncs}",
            bid.price,
            bid.qty,
            ask.price,
            ask.qty,
            book.spread().unwrap_or_default(),
        );
    }
    println!();
}
