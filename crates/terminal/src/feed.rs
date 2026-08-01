//! The terminal's connection to a core.
//!
//! GPUI renders on its own thread and must never block; the wire client is async and wants a
//! tokio runtime. So the client runs on a thread of its own and publishes into shared state,
//! and the UI reads that state on the frame tick. Nothing in the render path ever awaits.

use domain::{ApplyOutcome, ConnectionEvent, MarketEvent, OrderBook, Payload};
use exchange::Subscription;
use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use wire::{ClientMsg, ServerMsg, PROTOCOL_VERSION};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Live,
    /// Terminal state: the core refused us. Retrying would not help.
    Refused(String),
    Lost(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting".into(),
            Self::Live => "live".into(),
            Self::Refused(why) => format!("refused: {why}"),
            Self::Lost(why) => format!("reconnecting: {why}"),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }
}

/// What the UI reads. Sorted maps so row order never jumps between frames.
#[derive(Clone, Debug)]
pub struct FeedState {
    pub status: Status,
    pub books: BTreeMap<String, OrderBook>,
    pub trades: BTreeMap<String, u64>,
    /// Last thing the core said about its own health.
    pub core_note: Option<String>,
}

impl Default for FeedState {
    fn default() -> Self {
        Self { status: Status::Connecting, books: BTreeMap::new(), trades: BTreeMap::new(), core_note: None }
    }
}

#[derive(Clone)]
pub struct Feed {
    state: Arc<RwLock<FeedState>>,
}

impl Feed {
    /// Start the client on its own thread. Returns immediately.
    pub fn spawn(url: String, token: String, subs: Vec<Subscription>) -> Self {
        let state = Arc::new(RwLock::new(FeedState::default()));
        let feed = Self { state: state.clone() };

        std::thread::Builder::new()
            .name("moon-feed".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        set_status(&state, Status::Refused(e.to_string()));
                        return;
                    }
                };
                runtime.block_on(run(state, url, token, subs));
            })
            .expect("spawn feed thread");

        feed
    }

    /// A consistent copy for one frame. Cloned rather than held under a lock, so a slow
    /// render can never stall the socket thread.
    pub fn snapshot(&self) -> FeedState {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn set_status(state: &Arc<RwLock<FeedState>>, status: Status) {
    state.write().unwrap_or_else(|e| e.into_inner()).status = status;
}

async fn run(state: Arc<RwLock<FeedState>>, url: String, token: String, subs: Vec<Subscription>) {
    loop {
        set_status(&state, Status::Connecting);

        match session(&state, &url, &token, &subs).await {
            // A rejected token will be rejected again; stop rather than hammer the core.
            Err(Refusal::Fatal(why)) => {
                set_status(&state, Status::Refused(why));
                return;
            }
            Err(Refusal::Transient(why)) => set_status(&state, Status::Lost(why)),
            Ok(()) => set_status(&state, Status::Lost("core closed the connection".into())),
        }

        // Books from the old connection describe a stream that no longer exists.
        {
            let mut guard = state.write().unwrap_or_else(|e| e.into_inner());
            guard.books.clear();
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

enum Refusal {
    Fatal(String),
    Transient(String),
}

async fn session(
    state: &Arc<RwLock<FeedState>>,
    url: &str,
    token: &str,
    subs: &[Subscription],
) -> Result<(), Refusal> {
    let (socket, _) =
        tokio_tungstenite::connect_async(url).await.map_err(|e| Refusal::Transient(e.to_string()))?;
    let (mut tx, mut rx) = socket.split();

    let hello = ClientMsg::Hello { protocol: PROTOCOL_VERSION, token: token.to_string() };
    send(&mut tx, &hello).await?;

    while let Some(frame) = rx.next().await {
        let bytes = match frame.map_err(|e| Refusal::Transient(e.to_string()))? {
            Message::Binary(bytes) => bytes,
            Message::Close(_) => return Ok(()),
            _ => continue,
        };

        // One undecodable frame is not worth dropping the connection over.
        let Ok(msg) = wire::decode_server(&bytes) else { continue };
        match msg {
            ServerMsg::Welcome { .. } => {
                send(&mut tx, &ClientMsg::Subscribe(subs.to_vec())).await?;
                set_status(state, Status::Live);
            }
            ServerMsg::Failed { code, message } => {
                let why = format!("{code:?}: {message}");
                return Err(if code.is_fatal() { Refusal::Fatal(why) } else { Refusal::Transient(why) });
            }
            ServerMsg::Pong(_) => {}
            ServerMsg::Event(event) => apply(state, *event),
        }
    }
    Ok(())
}

fn apply(state: &Arc<RwLock<FeedState>>, event: domain::Event) {
    let mut guard = state.write().unwrap_or_else(|e| e.into_inner());

    match event.payload {
        Payload::Market(MarketEvent::BookSnapshot(book)) => {
            if let Some(symbol) = book.symbol.clone() {
                guard.books.insert(symbol.key(), book);
            }
        }
        Payload::Market(MarketEvent::BookDelta(delta)) => {
            let key = delta.symbol.key();
            if let Some(book) = guard.books.get_mut(&key) {
                // The core forwards only chainable updates, so a gap means our copy has
                // drifted from the core's. Drop it and wait for the next snapshot rather
                // than draw a book nobody can trade against.
                if let ApplyOutcome::Gap { .. } = book.apply(&delta) {
                    guard.books.remove(&key);
                    guard.core_note = Some(format!("{} desynced, waiting for a snapshot", delta.symbol));
                }
            }
        }
        Payload::Market(MarketEvent::Trade(trade)) => {
            *guard.trades.entry(trade.symbol.key()).or_default() += 1;
        }
        Payload::Connection(ConnectionEvent::Resyncing { reason }) => guard.core_note = Some(reason),
        Payload::Connection(ConnectionEvent::Ready) => guard.core_note = None,
        Payload::Connection(ConnectionEvent::Disconnected { reason }) => {
            guard.core_note = Some(format!("core lost its venue feed: {reason}"));
        }
        _ => {}
    }
}

async fn send<S>(tx: &mut S, msg: &ClientMsg) -> Result<(), Refusal>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let bytes = wire::encode(msg).map_err(|e| Refusal::Fatal(e.to_string()))?;
    tx.send(Message::Binary(bytes.into())).await.map_err(|e| Refusal::Transient(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookDelta, BookLevel, Event, ExchangeId, MarketKind, Symbol, Timestamp};

    fn sym() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    fn state() -> Arc<RwLock<FeedState>> {
        Arc::new(RwLock::new(FeedState::default()))
    }

    fn snapshot_event(id: u64) -> Event {
        Event::market(
            Timestamp::from_millis(1),
            MarketEvent::BookSnapshot(OrderBook {
                symbol: Some(sym()),
                bids: vec![BookLevel { price: "100".parse().unwrap(), qty: "1".parse().unwrap() }],
                asks: vec![BookLevel { price: "101".parse().unwrap(), qty: "1".parse().unwrap() }],
                last_update_id: id,
                ts: Timestamp::from_millis(1),
                synced: true,
            }),
        )
    }

    fn delta_event(prev: u64, last: u64) -> Event {
        Event::market(
            Timestamp::from_millis(2),
            MarketEvent::BookDelta(BookDelta {
                symbol: sym(),
                prev_update_id: prev,
                last_update_id: last,
                bids: vec![BookLevel { price: "100".parse().unwrap(), qty: "9".parse().unwrap() }],
                asks: vec![],
                ts: Timestamp::from_millis(2),
            }),
        )
    }

    #[test]
    fn a_snapshot_then_deltas_build_the_book() {
        let state = state();
        apply(&state, snapshot_event(1_000));
        apply(&state, delta_event(1_000, 1_010));

        let books = &state.read().unwrap().books;
        assert_eq!(books[&sym().key()].best_bid().unwrap().qty, "9".parse().unwrap());
    }

    #[test]
    fn a_desync_drops_the_book_rather_than_drawing_a_stale_one() {
        let state = state();
        apply(&state, snapshot_event(1_000));
        apply(&state, delta_event(5_000, 5_010));

        let guard = state.read().unwrap();
        assert!(guard.books.is_empty(), "a book that disagrees with the core must not be shown");
        assert!(guard.core_note.as_ref().unwrap().contains("desynced"));
    }

    #[test]
    fn trades_are_counted_per_instrument() {
        let state = state();
        for _ in 0..3 {
            apply(
                &state,
                Event::market(
                    Timestamp::from_millis(1),
                    MarketEvent::Trade(domain::PublicTrade {
                        symbol: sym(),
                        price: "100".parse().unwrap(),
                        qty: "1".parse().unwrap(),
                        taker_side: domain::Side::Buy,
                        ts: Timestamp::from_millis(1),
                        id: 1,
                    }),
                ),
            );
        }
        assert_eq!(state.read().unwrap().trades[&sym().key()], 3);
    }

    #[test]
    fn core_health_notices_are_surfaced_and_cleared() {
        let state = state();
        apply(
            &state,
            Event::connection(
                Timestamp::from_millis(1),
                ConnectionEvent::Resyncing { reason: "rebuilding book".into() },
            ),
        );
        assert_eq!(state.read().unwrap().core_note.as_deref(), Some("rebuilding book"));

        apply(&state, Event::connection(Timestamp::from_millis(2), ConnectionEvent::Ready));
        assert!(state.read().unwrap().core_note.is_none());
    }

    #[test]
    fn a_refusal_reads_differently_from_a_dropped_connection() {
        // The UI must not invite the user to wait out something that will never resolve.
        assert!(Status::Refused("Unauthorized".into()).label().starts_with("refused"));
        assert!(Status::Lost("reset".into()).label().starts_with("reconnecting"));
        assert!(Status::Live.is_live());
        assert!(!Status::Connecting.is_live());
    }
}
