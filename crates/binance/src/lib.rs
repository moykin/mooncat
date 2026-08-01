//! Binance connector.
//!
//! One implementation serves spot and USD-M perpetuals: the market is a field, not a fork.
//! Everything that actually differs between them — hosts, path prefixes, the book
//! sequencing scheme — is isolated in [`endpoints`] and [`wire`].
//!
//! The connector owns book correctness. Consumers receive a [`MarketEvent::BookSnapshot`]
//! whenever the book is (re)built and [`MarketEvent::BookDelta`] for every update that
//! chained onto it — never an update that would tear their copy. Snapshot fetching, gap
//! recovery and resubscription after a reconnect all happen underneath.
//!
//! Only [`MarketDataSource`] is implemented so far. Market data needs no API keys, so a
//! core can run the full feed with no credentials on disk at all.

use async_trait::async_trait;
use domain::{
    BookDelta, ConnectionEvent, Event, Instrument, MarketEvent, MarketKind, OrderBook, PublicTrade, Symbol,
    Timestamp,
};
use exchange::{Error, EventSink, MarketDataSource, Result, Subscription};
use marketdata::{BookSet, CandleSet, Need};
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

/// Depth requested when rebuilding a book.
const SNAPSHOT_DEPTH: u32 = 100;

/// How often to look for books that have stopped moving.
const STALENESS_CHECK: Duration = Duration::from_secs(5);

/// A book silent for this long on a live socket is reported, not waited out.
const STALE_AFTER_MS: i64 = 30_000;

/// Candle bucket. One second, because the chart this feeds is a scalping chart and the
/// venue's own klines start at a minute.
const CANDLE_INTERVAL_MS: i64 = 1_000;

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
        let http = http_client()?;

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(socket_task(market, endpoints, http.clone(), sink, rx));

        Ok(Self { market, endpoints, http, commands: tx })
    }

    pub fn market(&self) -> MarketKind {
        self.market
    }

    fn send(&self, cmd: Command) -> Result<()> {
        self.commands.send(cmd).map_err(|_| Error::Transport("socket task has stopped".into()))
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Transport(e.to_string()))
}

async fn get_json<T: serde::de::DeserializeOwned>(http: &reqwest::Client, url: &str) -> Result<T> {
    let resp = http.get(url).send().await.map_err(|e| Error::Transport(e.to_string()))?;
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

async fn fetch_snapshot(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    symbol: &Symbol,
    depth: u32,
) -> Result<OrderBook> {
    let limit = endpoints::legal_depth_limit(depth);
    let snap: wire::DepthSnapshot = get_json(http, &endpoints.depth_url(&symbol.raw, limit)).await?;
    Ok(snap.into_domain(symbol.clone(), now()))
}

#[async_trait]
impl MarketDataSource for Binance {
    async fn instruments(&self, market: MarketKind) -> Result<Vec<Instrument>> {
        if market != self.market {
            return Err(Error::Unsupported { what: "instruments for another market", market });
        }
        let info: wire::ExchangeInfo = get_json(&self.http, &self.endpoints.exchange_info_url()).await?;

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
        fetch_snapshot(&self.http, self.endpoints, symbol, depth).await
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
    http: reqwest::Client,
    sink: EventSink,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    // Survives reconnects: the venue forgets our subscriptions, we do not.
    let mut active: BTreeSet<String> = BTreeSet::new();
    let mut books = BookSet::default();
    let mut candles = CandleSet::new(CANDLE_INTERVAL_MS, marketdata::candles::DEFAULT_CAPACITY);
    let mut backoff = BACKOFF_START;

    loop {
        sink.send(Event::connection(now(), ConnectionEvent::Connecting));

        match tokio_tungstenite::connect_async(endpoints.ws).await {
            Ok((socket, _)) => {
                backoff = BACKOFF_START;
                let ctx = Ctx { market, endpoints, http: &http, sink: &sink };
                match pump(socket, &ctx, &mut commands, &mut active, &mut books, &mut candles).await {
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

        // Sequence numbers do not survive a reconnect, so neither may the books.
        books.reset();

        if sink.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

struct Ctx<'a> {
    market: MarketKind,
    endpoints: &'static Endpoints,
    http: &'a reqwest::Client,
    sink: &'a EventSink,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn pump(
    socket: Socket,
    ctx: &Ctx<'_>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    active: &mut BTreeSet<String>,
    books: &mut BookSet,
    candles: &mut CandleSet,
) -> Exit {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut tx, mut rx) = socket.split();
    let mut frame_id = 0u64;

    // Snapshot fetches run on their own tasks and report back here — success *and* failure.
    // Awaiting them inline would stall the read loop, and a stalled reader is exactly how a
    // book goes stale. Failures must be reported too: a fetch that reports nothing leaves
    // the book waiting for a snapshot that is never coming.
    let (snap_tx, mut snap_rx) = mpsc::unbounded_channel::<SnapshotResult>();
    let mut watchdog = tokio::time::interval(STALENESS_CHECK);
    watchdog.tick().await; // the first tick fires immediately

    // Re-arm everything the caller asked for before this connection existed.
    if !active.is_empty() {
        frame_id += 1;
        let names: Vec<String> = active.iter().cloned().collect();
        if tx.send(Message::Text(streams::control_frame("SUBSCRIBE", &names, frame_id).into())).await.is_err()
        {
            return Exit::Reconnect("failed to resubscribe".into());
        }
    }
    ctx.sink.send(Event::connection(now(), ConnectionEvent::Ready));

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
                Some(Ok(Message::Text(text))) => match decode(&text, ctx.market) {
                    Some(Decoded::Trade(trade)) => {
                        // Candles are folded from the same prints the tape shows, so the two
                        // can never disagree.
                        for candle in fold(candles, &trade) {
                            ctx.sink.send(Event::market(now(), MarketEvent::Candle(candle)));
                        }
                        ctx.sink.send(Event::market(now(), MarketEvent::Trade(trade)));
                    }
                    Some(Decoded::Delta(delta)) => {
                        on_delta(ctx, books, delta, &snap_tx);
                    }
                    None => {}
                },
                // Binance pings every few minutes and drops us if we do not pong.
                Some(Ok(Message::Ping(payload))) => {
                    if tx.send(Message::Pong(payload)).await.is_err() {
                        return Exit::Reconnect("pong write failed".into());
                    }
                }
                Some(Ok(Message::Close(_))) => return Exit::Reconnect("server closed".into()),
                Some(Ok(_)) => {}
            },

            Some(result) = snap_rx.recv() => match result {
                SnapshotResult::Fetched(snapshot) => {
                    let (sym, id) = (snapshot.symbol.clone(), snapshot.last_update_id);
                    match books.install_snapshot(snapshot, now()) {
                        Some(joined) => {
                            tracing::debug!(symbol = ?sym, id, "snapshot joined the stream");
                            ctx.sink.send(Event::market(now(), MarketEvent::BookSnapshot(joined)));
                        }
                        None => tracing::debug!(symbol = ?sym, id, "snapshot did not join"),
                    }
                }
                SnapshotResult::Failed(symbol) => books.snapshot_failed(&symbol),
            },

            _ = watchdog.tick() => {
                for symbol in books.stale(now(), STALE_AFTER_MS) {
                    // The socket is up and other streams may be fine — say which book froze.
                    ctx.sink.send(Event::connection(now(), ConnectionEvent::Resyncing {
                        reason: format!("{symbol} has not updated in {}s", STALE_AFTER_MS / 1000),
                    }));
                }
            }
        }
    }
}

/// Fold a print into its candle and return whatever consumers must redraw.
///
/// A boundary yields two: the bar that just became final, then the one now forming. Sending
/// only the new one would leave the previous bar drawn as still-forming forever.
fn fold(candles: &mut CandleSet, trade: &PublicTrade) -> Vec<domain::Candle> {
    match candles.on_trade(trade) {
        marketdata::Update::Formed(candle) => vec![candle],
        marketdata::Update::Closed { closed, opened } => vec![closed, opened],
        marketdata::Update::Ignored => Vec::new(),
    }
}

/// The outcome of one snapshot fetch, reported back to the socket task.
///
/// Failure is a variant rather than a log line: a fetch that reports nothing leaves the book
/// waiting for a snapshot that is never coming.
enum SnapshotResult {
    Fetched(OrderBook),
    Failed(Symbol),
}

/// Feed one book update through the maintainer, forwarding only what is safe to forward.
fn on_delta(
    ctx: &Ctx<'_>,
    books: &mut BookSet,
    delta: BookDelta,
    snap_tx: &mpsc::UnboundedSender<SnapshotResult>,
) {
    let key = delta.symbol.key();
    let outcome = books.on_delta(delta.clone(), now());

    if outcome.attached {
        // The book only just became usable; consumers have no copy to update.
        if let Some(book) = books.book(&key) {
            ctx.sink.send(Event::market(now(), MarketEvent::BookSnapshot(book.clone())));
        }
    } else if outcome.forward {
        ctx.sink.send(Event::market(now(), MarketEvent::BookDelta(delta)));
    }

    let Need::Snapshot(symbol) = outcome.need else { return };

    ctx.sink.send(Event::connection(
        now(),
        ConnectionEvent::Resyncing { reason: format!("rebuilding book for {symbol}") },
    ));

    let (http, endpoints, tx) = (ctx.http.clone(), ctx.endpoints, snap_tx.clone());
    tokio::spawn(async move {
        // A closed channel just means the socket moved on; the next delta re-requests.
        let outcome = match fetch_snapshot(&http, endpoints, &symbol, SNAPSHOT_DEPTH).await {
            Ok(book) => SnapshotResult::Fetched(book),
            Err(e) => {
                tracing::warn!(%symbol, error = %e, "book snapshot failed, will retry");
                SnapshotResult::Failed(symbol)
            }
        };
        let _ = tx.send(outcome);
    });
}

/// What one stream frame turned out to be.
enum Decoded {
    Trade(PublicTrade),
    Delta(BookDelta),
}

/// Turn one stream frame into something we recognise. Control acknowledgements and unknown
/// stream types yield `None` rather than an error — the venue adds fields freely.
fn decode(text: &str, market: MarketKind) -> Option<Decoded> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;

    match value.get("e")?.as_str()? {
        "aggTrade" => {
            let raw: wire::AggTrade = serde_json::from_value(value).ok()?;
            let symbol = Symbol::new(domain::ExchangeId::Binance, market, &raw.symbol);
            Some(Decoded::Trade(raw.into_domain(symbol)))
        }
        "depthUpdate" => {
            let raw: wire::DepthUpdate = serde_json::from_value(value).ok()?;
            let symbol = Symbol::new(domain::ExchangeId::Binance, market, &raw.symbol);
            Some(Decoded::Delta(raw.into_domain(symbol)))
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
    fn decodes_a_trade_frame() {
        let raw = r#"{"e":"aggTrade","E":1,"s":"BTCUSDT","a":99,"p":"64000.10","q":"0.235","f":1,"l":2,"T":1700000000000,"m":false}"#;
        match decode(raw, MarketKind::LinearPerp) {
            Some(Decoded::Trade(t)) => {
                assert_eq!(t.symbol.market, MarketKind::LinearPerp);
                assert_eq!(t.taker_side, domain::Side::Buy);
            }
            _ => panic!("expected a trade"),
        }
    }

    #[test]
    fn the_same_ticker_decodes_to_distinct_symbols_per_market() {
        // Running spot and futures in one process hinges on this not collapsing.
        let raw = r#"{"e":"aggTrade","E":1,"s":"BTCUSDT","a":1,"p":"1","q":"1","f":1,"l":1,"T":1,"m":false}"#;
        let key = |market| match decode(raw, market) {
            Some(Decoded::Trade(t)) => t.symbol.key(),
            _ => unreachable!(),
        };
        assert_eq!(key(MarketKind::Spot), "binance:spot:BTCUSDT");
        assert_eq!(key(MarketKind::LinearPerp), "binance:linear_perp:BTCUSDT");
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
        match decode(raw, MarketKind::LinearPerp) {
            Some(Decoded::Delta(d)) => {
                assert_eq!(d.prev_update_id, 149);
                assert_eq!(d.last_update_id, 160);
            }
            _ => panic!("expected a book delta"),
        }
    }
}
