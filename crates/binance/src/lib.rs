//! Binance connector.
//!
//! One implementation serves spot and USD-M perpetuals: the market is a field, not a fork.
//! Everything that actually differs between them — hosts, path prefixes, the book
//! sequencing scheme — is isolated in [`endpoints`] and [`wire`].
//!
//! Only [`MarketDataSource`] is implemented so far. Market data needs no API keys, so a
//! core can run the full feed with no credentials on disk at all.

use async_trait::async_trait;
use domain::{ConnectionEvent, Event, Instrument, MarketEvent, MarketKind, OrderBook, Symbol, Timestamp};
use exchange::{Error, EventSink, MarketDataSource, Result, Subscription};
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::sync::mpsc;

pub mod endpoints;
pub mod streams;
pub mod wire;

use endpoints::Endpoints;

/// Reconnect backoff bounds. The ceiling is low on purpose: a feed that is down is worse
/// than a few extra connection attempts.
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Commands sent from the public API to the socket task.
enum Command {
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
}

pub struct Binance {
    market: MarketKind,
    endpoints: &'static Endpoints,
    http: reqwest::Client,
    commands: mpsc::UnboundedSender<Command>,
}

impl Binance {
    /// Connect and start the socket task. Events land on `sink`.
    ///
    /// Returns as soon as the task is spawned; the first [`ConnectionEvent::Ready`] on the
    /// sink is what signals the stream is actually live.
    pub fn connect(market: MarketKind, sink: EventSink) -> Result<Self> {
        let endpoints = Endpoints::for_market(market)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(socket_task(market, endpoints, sink, rx));

        Ok(Self { market, endpoints, http, commands: tx })
    }

    pub fn market(&self) -> MarketKind {
        self.market
    }

    fn send(&self, cmd: Command) -> Result<()> {
        self.commands.send(cmd).map_err(|_| Error::Transport("socket task has stopped".into()))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.http.get(url).send().await.map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| Error::Transport(e.to_string()))?;

        if status == 429 || status == 418 {
            // 418 is Binance's "you ignored a 429 and are now banned" code.
            return Err(Error::RateLimited { retry_after_ms: None });
        }
        if !status.is_success() {
            return Err(Error::Rejected { code: status.as_u16().to_string(), message: body });
        }
        serde_json::from_str(&body).map_err(|e| Error::Malformed(format!("{e}: {}", truncate(&body, 200))))
    }
}

#[async_trait]
impl MarketDataSource for Binance {
    async fn instruments(&self, market: MarketKind) -> Result<Vec<Instrument>> {
        if market != self.market {
            return Err(Error::Unsupported { what: "instruments for another market", market });
        }
        let info: wire::ExchangeInfo = self.get_json(&self.endpoints.exchange_info_url()).await?;

        // One unparseable symbol must not cost us the whole list — Binance carries oddities
        // (leveraged tokens, index products) whose filter sets do not fit the normal shape.
        let mut out = Vec::with_capacity(info.symbols.len());
        for symbol in info.symbols {
            let name = symbol.symbol.clone();
            match symbol.into_domain(market) {
                Ok(instrument) => out.push(instrument),
                Err(e) => tracing::debug!(symbol = %name, error = %e, "skipping instrument"),
            }
        }
        Ok(out)
    }

    async fn subscribe(&self, subs: &[Subscription]) -> Result<()> {
        let names = subs.iter().map(streams::stream_name).collect::<Result<Vec<_>>>()?;
        self.send(Command::Subscribe(names))
    }

    async fn unsubscribe(&self, subs: &[Subscription]) -> Result<()> {
        let names = subs.iter().map(streams::stream_name).collect::<Result<Vec<_>>>()?;
        self.send(Command::Unsubscribe(names))
    }

    async fn book_snapshot(&self, symbol: &Symbol, depth: u32) -> Result<OrderBook> {
        let limit = endpoints::legal_depth_limit(depth);
        let snap: wire::DepthSnapshot = self.get_json(&self.endpoints.depth_url(&symbol.raw, limit)).await?;
        Ok(snap.into_domain(symbol.clone(), now()))
    }
}

// ------------------------------------------------------------------ socket task

/// Why the read loop returned.
enum Exit {
    /// The connector was dropped; stop for good.
    Shutdown,
    /// The socket died; reconnect and resubscribe.
    Reconnect(String),
}

async fn socket_task(
    market: MarketKind,
    endpoints: &'static Endpoints,
    sink: EventSink,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    // Survives reconnects: the venue forgets our subscriptions, we do not.
    let mut active: BTreeSet<String> = BTreeSet::new();
    let mut backoff = BACKOFF_START;

    loop {
        sink.send(Event::connection(now(), ConnectionEvent::Connecting));

        match tokio_tungstenite::connect_async(endpoints.ws).await {
            Ok((socket, _)) => {
                backoff = BACKOFF_START;
                match pump(socket, market, &sink, &mut commands, &mut active).await {
                    Exit::Shutdown => return,
                    Exit::Reconnect(reason) => {
                        sink.send(Event::connection(now(), ConnectionEvent::Disconnected { reason }));
                    }
                }
            }
            Err(e) => {
                sink.send(Event::connection(now(), ConnectionEvent::Disconnected { reason: e.to_string() }));
            }
        }

        if sink.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn pump(
    socket: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    market: MarketKind,
    sink: &EventSink,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    active: &mut BTreeSet<String>,
) -> Exit {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut tx, mut rx) = socket.split();
    let mut frame_id = 0u64;

    // Re-arm everything the caller asked for before this connection existed.
    if !active.is_empty() {
        frame_id += 1;
        let names: Vec<String> = active.iter().cloned().collect();
        if tx.send(Message::Text(streams::control_frame("SUBSCRIBE", &names, frame_id).into())).await.is_err()
        {
            return Exit::Reconnect("failed to resubscribe".into());
        }
    }
    sink.send(Event::connection(now(), ConnectionEvent::Ready));

    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None => return Exit::Shutdown,
                Some(Command::Subscribe(names)) => {
                    active.extend(names.iter().cloned());
                    frame_id += 1;
                    if tx.send(Message::Text(streams::control_frame("SUBSCRIBE", &names, frame_id).into())).await.is_err() {
                        return Exit::Reconnect("subscribe write failed".into());
                    }
                }
                Some(Command::Unsubscribe(names)) => {
                    for n in &names { active.remove(n); }
                    frame_id += 1;
                    if tx.send(Message::Text(streams::control_frame("UNSUBSCRIBE", &names, frame_id).into())).await.is_err() {
                        return Exit::Reconnect("unsubscribe write failed".into());
                    }
                }
            },

            msg = rx.next() => match msg {
                None => return Exit::Reconnect("stream ended".into()),
                Some(Err(e)) => return Exit::Reconnect(e.to_string()),
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = decode(&text, market) {
                        sink.send(event);
                    }
                }
                // Binance pings every few minutes and drops us if we do not pong.
                Some(Ok(Message::Ping(payload))) => {
                    if tx.send(Message::Pong(payload)).await.is_err() {
                        return Exit::Reconnect("pong write failed".into());
                    }
                }
                Some(Ok(Message::Close(_))) => return Exit::Reconnect("server closed".into()),
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Turn one stream frame into an event. Control acknowledgements and unknown stream types
/// yield `None` rather than an error — the venue adds fields freely.
fn decode(text: &str, market: MarketKind) -> Option<Event> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;

    match value.get("e")?.as_str()? {
        "aggTrade" => {
            let raw: wire::AggTrade = serde_json::from_value(value).ok()?;
            let symbol = Symbol::new(domain::ExchangeId::Binance, market, &raw.symbol);
            Some(Event::market(now(), MarketEvent::Trade(raw.into_domain(symbol))))
        }
        "depthUpdate" => {
            let raw: wire::DepthUpdate = serde_json::from_value(value).ok()?;
            let symbol = Symbol::new(domain::ExchangeId::Binance, market, &raw.symbol);
            Some(Event::market(now(), MarketEvent::BookDelta(raw.into_domain(symbol))))
        }
        _ => None,
    }
}

fn now() -> Timestamp {
    Timestamp::from_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    )
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_trade_frame_into_a_market_event() {
        let raw = r#"{"e":"aggTrade","E":1,"s":"BTCUSDT","a":99,"p":"64000.10","q":"0.235","f":1,"l":2,"T":1700000000000,"m":false}"#;
        let event = decode(raw, MarketKind::LinearPerp).expect("should decode");

        match event.payload {
            domain::Payload::Market(MarketEvent::Trade(t)) => {
                assert_eq!(t.symbol.market, MarketKind::LinearPerp);
                assert_eq!(t.taker_side, domain::Side::Buy);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn the_same_ticker_decodes_to_distinct_symbols_per_market() {
        // Running spot and futures in one process hinges on this not collapsing.
        let raw = r#"{"e":"aggTrade","E":1,"s":"BTCUSDT","a":1,"p":"1","q":"1","f":1,"l":1,"T":1,"m":false}"#;
        let spot = decode(raw, MarketKind::Spot).unwrap();
        let perp = decode(raw, MarketKind::LinearPerp).unwrap();

        let key = |e: &Event| match &e.payload {
            domain::Payload::Market(MarketEvent::Trade(t)) => t.symbol.key(),
            _ => unreachable!(),
        };
        assert_eq!(key(&spot), "binance:spot:BTCUSDT");
        assert_eq!(key(&perp), "binance:linear_perp:BTCUSDT");
        assert_ne!(key(&spot), key(&perp));
    }

    #[test]
    fn control_acks_and_unknown_streams_are_ignored_not_errors() {
        // Binance answers every SUBSCRIBE with this; it carries no "e" field.
        assert!(decode(r#"{"result":null,"id":1}"#, MarketKind::Spot).is_none());
        assert!(decode(r#"{"e":"markPriceUpdate","s":"BTCUSDT"}"#, MarketKind::Spot).is_none());
        assert!(decode("not json at all", MarketKind::Spot).is_none());
    }

    #[test]
    fn a_book_frame_decodes_with_its_sequence_numbers_intact() {
        let raw = r#"{"e":"depthUpdate","E":1700000000000,"T":1,"s":"BTCUSDT","U":157,"u":160,"pu":149,"b":[["64000.1","10"]],"a":[]}"#;
        let event = decode(raw, MarketKind::LinearPerp).unwrap();

        match event.payload {
            domain::Payload::Market(MarketEvent::BookDelta(d)) => {
                assert_eq!(d.prev_update_id, 149);
                assert_eq!(d.last_update_id, 160);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }
}
