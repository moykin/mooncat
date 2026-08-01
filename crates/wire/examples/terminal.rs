//! A terminal, minus the pixels.
//!
//! Connects to a core, authenticates, subscribes and renders the book as text. Stage 4
//! replaces the `println!` with GPUI; everything below it stays as it is.
//!
//! ```bash
//! MOON_TOKEN=… cargo run -p wire --example terminal -- BTCUSDT
//! ```

use domain::{ApplyOutcome, ExchangeId, MarketEvent, MarketKind, OrderBook, Payload, Symbol};
use exchange::Subscription;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio_tungstenite::tungstenite::Message;
use wire::{ClientMsg, ServerMsg, PROTOCOL_VERSION};

const MARKETS: [MarketKind; 2] = [MarketKind::Spot, MarketKind::LinearPerp];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("MOON_TOKEN").map_err(|_| "set MOON_TOKEN to the core's token")?;
    let url = std::env::var("MOON_CORE").unwrap_or_else(|_| "ws://127.0.0.1:8787".into());
    let ticker = std::env::args().nth(1).unwrap_or_else(|| "BTCUSDT".into()).to_uppercase();

    let (socket, _) = tokio_tungstenite::connect_async(&url).await?;
    let (mut tx, mut rx) = socket.split();
    println!("connected to {url}");

    send(&mut tx, &ClientMsg::Hello { protocol: PROTOCOL_VERSION, token }).await?;

    let subs: Vec<Subscription> = MARKETS
        .iter()
        .flat_map(|market| {
            let symbol = Symbol::new(ExchangeId::Binance, *market, &ticker);
            [Subscription::Trades(symbol.clone()), Subscription::Book(symbol)]
        })
        .collect();

    let mut books: HashMap<String, OrderBook> = HashMap::new();
    let mut trades: HashMap<String, u64> = HashMap::new();
    let mut kinds: HashMap<&'static str, u64> = HashMap::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = tick.tick() => print_books(&books, &trades, &kinds),
            incoming = rx.next() => match incoming {
                None => break,
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(Message::Binary(bytes))) => match wire::decode_server(&bytes)? {
                    ServerMsg::Welcome { protocol } => {
                        println!("authenticated, protocol {protocol}\n");
                        send(&mut tx, &ClientMsg::Subscribe(subs.clone())).await?;
                    }
                    ServerMsg::Failed { code, message } => {
                        return Err(format!("core refused us: {code:?} — {message}").into());
                    }
                    ServerMsg::Pong(_) => {}
                    ServerMsg::Event(event) => match event.payload {
                        Payload::Market(MarketEvent::BookSnapshot(book)) => {
                            *kinds.entry("snapshot").or_default() += 1;
                            if let Some(symbol) = book.symbol.clone() {
                                books.insert(symbol.key(), book);
                            }
                        }
                        Payload::Market(MarketEvent::BookDelta(delta)) => {
                            *kinds.entry("delta").or_default() += 1;
                            if let Some(book) = books.get_mut(&delta.symbol.key()) {
                                if let ApplyOutcome::Gap { .. } = book.apply(&delta) {
                                    eprintln!("{}: delta did not chain", delta.symbol);
                                }
                            }
                        }
                        Payload::Market(MarketEvent::Trade(t)) => {
                            *kinds.entry("trade").or_default() += 1;
                            *trades.entry(t.symbol.key()).or_default() += 1;
                        }
                        Payload::Connection(state) => println!("· {state:?}"),
                        _ => {}
                    },
                },
                Some(Ok(_)) => {}
            },
        }
    }
    Ok(())
}

async fn send<S>(tx: &mut S, msg: &ClientMsg) -> Result<(), Box<dyn std::error::Error>>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + 'static,
{
    tx.send(Message::Binary(wire::encode(msg)?.into())).await?;
    Ok(())
}

fn print_books(
    books: &HashMap<String, OrderBook>,
    trades: &HashMap<String, u64>,
    kinds: &HashMap<&'static str, u64>,
) {
    let mut keys: Vec<&String> = books.keys().collect();
    keys.sort();
    if keys.is_empty() {
        let mut seen: Vec<String> = kinds.iter().map(|(k, v)| format!("{k}={v}")).collect();
        seen.sort();
        println!("waiting for books… received: [{}]", seen.join(" "));
        return;
    }
    for key in keys {
        let book = &books[key];
        let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) else { continue };
        println!(
            "{:<28} bid {:>12} × {:<10} ask {:>12} × {:<10} spread {:<10} trades {}",
            key.strip_prefix("binance:").unwrap_or(key),
            bid.price,
            bid.qty,
            ask.price,
            ask.qty,
            book.spread().unwrap_or_default(),
            trades.get(key).copied().unwrap_or_default(),
        );
    }
    println!();
}
