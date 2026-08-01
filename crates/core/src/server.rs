//! The WebSocket server terminals connect to.

use crate::metrics::Metrics;
use crate::state::MarketState;
use domain::{ConnectionEvent, Event, MarketEvent, Timestamp};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use wire::{Auth, ServerMsg, Session};

/// Events buffered per terminal before it is considered too slow.
///
/// Deliberately generous but finite. An unbounded queue would let one stalled laptop grow
/// the core's memory without limit; dropping is recoverable, running out of memory on the
/// machine holding the API keys is not.
const FANOUT_CAPACITY: usize = 8_192;

pub type Fanout = broadcast::Sender<Event>;

pub fn fanout() -> Fanout {
    broadcast::channel(FANOUT_CAPACITY).0
}

/// Accept terminals until the listener dies.
///
/// `tls` is `None` only when bound to loopback — [`crate::config::Config::validate`] refuses
/// any other combination at startup, so this function never has to decide whether plaintext
/// is acceptable.
pub async fn serve(
    listener: TcpListener,
    auth: Arc<Auth>,
    fanout: Fanout,
    state: MarketState,
    tls: Option<TlsAcceptor>,
    metrics: Metrics,
) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        tracing::info!(%peer, "terminal connecting");

        let (auth, events, state) = (auth.clone(), fanout.subscribe(), state.clone());
        let (tls, metrics) = (tls.clone(), metrics.clone());
        tokio::spawn(async move {
            // The handshake happens on the connection's own task. Doing it in the accept loop
            // would let one client that opens a socket and then says nothing stall every other
            // terminal trying to connect.
            let outcome = match tls {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(stream) => session(stream, auth, events, state, metrics).await,
                    Err(e) => Err(format!("TLS handshake failed: {e}")),
                },
                None => session(stream, auth, events, state, metrics).await,
            };
            if let Err(e) = outcome {
                tracing::info!(%peer, reason = %e, "terminal disconnected");
            }
        });
    }
}

/// Generic over the transport so that one implementation serves both a plain `TcpStream` on
/// loopback and a TLS stream on a public interface. The protocol above the socket is the same
/// either way, and duplicating it to avoid a type parameter is how the two copies drift.
async fn session<S>(
    stream: S,
    auth: Arc<Auth>,
    mut events: broadcast::Receiver<Event>,
    state: MarketState,
    metrics: Metrics,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let socket = tokio_tungstenite::accept_async(stream).await.map_err(|e| e.to_string())?;
    // Counted from here — after the WebSocket handshake, so a port scanner opening and
    // closing a socket does not register as a terminal. The guard decrements on every exit
    // path out of this function.
    let _connected = metrics.terminal_connected();
    let (mut tx, mut rx) = socket.split();
    let mut session = Session::new();

    loop {
        tokio::select! {
            incoming = rx.next() => match incoming {
                None => return Ok(()),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(Message::Binary(bytes))) => {
                    let reaction = match wire::decode_client(&bytes) {
                        Ok(msg) => session.handle(msg, &auth),
                        Err(e) => wire::session::Reaction {
                            reply: ServerMsg::Failed {
                                code: wire::ErrorCode::Malformed,
                                message: e.to_string(),
                            },
                            close: false,
                        },
                    };
                    send(&mut tx, &reaction.reply).await?;
                    if reaction.close {
                        return Ok(());
                    }

                    // Hand over current state, then let the stream carry it forward. Without
                    // this a terminal joining after the books were built receives deltas for
                    // books it does not have and stays blank.
                    for trade in state.tape_for(session.subscriptions()) {
                        let event = Event::market(now(), MarketEvent::Trade(trade));
                        send(&mut tx, &ServerMsg::Event(Box::new(event))).await?;
                    }
                    for candle in state.candles_for(session.subscriptions()) {
                        let event = Event::market(now(), MarketEvent::Candle(candle));
                        send(&mut tx, &ServerMsg::Event(Box::new(event))).await?;
                    }
                    for book in state.snapshots_for(session.subscriptions()) {
                        let event = Event::market(now(), MarketEvent::BookSnapshot(book));
                        send(&mut tx, &ServerMsg::Event(Box::new(event))).await?;
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    tx.send(Message::Pong(payload)).await.map_err(|e| e.to_string())?;
                }
                Some(Ok(Message::Close(_))) => return Ok(()),
                // Text frames are not part of the protocol; ignoring them keeps a browser
                // poking at the port from costing us anything.
                Some(Ok(_)) => {}
            },

            event = events.recv() => match event {
                Ok(event) => {
                    if session.wants(&event) {
                        send(&mut tx, &ServerMsg::Event(Box::new(event))).await?;
                    }
                }
                // The terminal fell behind and lost events. Telling it is essential: its
                // book is now missing updates, and silence would leave it rendering a
                // stale book as if it were live.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    metrics.events_lagged(missed);
                    let notice = Event::connection(
                        now(),
                        ConnectionEvent::Resyncing {
                            reason: format!("terminal fell behind, {missed} events dropped"),
                        },
                    );
                    send(&mut tx, &ServerMsg::Event(Box::new(notice))).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

async fn send<S>(tx: &mut S, msg: &ServerMsg) -> Result<(), String>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let bytes = wire::encode(msg).map_err(|e| e.to_string())?;
    tx.send(Message::Binary(bytes.into())).await.map_err(|e| e.to_string())
}

fn now() -> Timestamp {
    Timestamp::from_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    //! Against a real socket on `127.0.0.1:0`.
    //!
    //! `wire::session` already tests the rules as pure logic, and those tests are the ones
    //! that pin down *what* is allowed. These pin down that the socket actually behaves that
    //! way: that a reply is written before the connection closes, that a rejected terminal is
    //! really disconnected rather than merely told no, and that the state handover happens in
    //! the right order. None of that is visible from a unit test of `Session`, and all of it
    //! was previously untested — 141 lines carrying the connection lifecycle.
    //!
    //! Port 0 lets the OS choose, so the suite can run in parallel with itself and with
    //! anything else on the machine.

    use super::*;
    use domain::{BookLevel, ExchangeId, MarketKind, OrderBook, PublicTrade, Side, Symbol};
    use exchange::{EventSink, Subscription};
    use rust_decimal_macros::dec;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
    use wire::{ClientMsg, ErrorCode, ServerMsg, PROTOCOL_VERSION};

    const TOKEN: &str = "0123456789abcdef";
    /// Long enough that a loaded CI machine does not fail the suite, short enough that a
    /// genuine hang is reported as a failure rather than as a hung job.
    const PATIENCE: Duration = Duration::from_secs(5);

    type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

    struct Harness {
        addr: SocketAddr,
        fanout: Fanout,
        state: MarketState,
        metrics: Metrics,
        /// The sink's receiver. Dropping it would close the sink and make `/health` unhealthy,
        /// so it is held for the lifetime of the harness.
        _events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    }

    async fn harness() -> Harness {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("port 0 binds");
        let addr = listener.local_addr().expect("a bound listener has an address");
        let (sink, events) = exchange::event_channel();
        let (fanout, state) = (fanout(), MarketState::default());
        let metrics = Metrics::new(sink);

        tokio::spawn(serve(
            listener,
            Arc::new(Auth::new(TOKEN)),
            fanout.clone(),
            state.clone(),
            None,
            metrics.clone(),
        ));

        Harness { addr, fanout, state, metrics, _events: events }
    }

    async fn connect(h: &Harness) -> Socket {
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{}", h.addr))
            .await
            .expect("the core accepts a WebSocket handshake");
        socket
    }

    async fn say(socket: &mut Socket, msg: &ClientMsg) {
        let bytes = wire::encode(msg).expect("client messages encode");
        socket.send(Message::Binary(bytes.into())).await.expect("the socket accepts a frame");
    }

    /// Next protocol message, skipping WebSocket-level frames the test does not care about.
    async fn hear(socket: &mut Socket) -> ServerMsg {
        let deadline = tokio::time::timeout(PATIENCE, async {
            loop {
                match socket.next().await {
                    Some(Ok(Message::Binary(bytes))) => {
                        return wire::decode_server(&bytes).expect("the core sends valid frames")
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => panic!("socket error while waiting for a message: {e}"),
                    None => panic!("the core closed the connection while a message was expected"),
                }
            }
        });
        deadline.await.expect("the core replied within the timeout")
    }

    /// Whether the core hung up, as opposed to going quiet.
    async fn closed(socket: &mut Socket) -> bool {
        let outcome = tokio::time::timeout(PATIENCE, async {
            loop {
                match socket.next().await {
                    None | Some(Ok(Message::Close(_))) => return true,
                    Some(Err(_)) => return true,
                    Some(Ok(_)) => continue,
                }
            }
        });
        outcome.await.unwrap_or(false)
    }

    async fn hello(socket: &mut Socket) -> ServerMsg {
        say(socket, &ClientMsg::Hello { protocol: PROTOCOL_VERSION, token: TOKEN.into() }).await;
        hear(socket).await
    }

    fn symbol() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    fn trade_event() -> Event {
        Event::market(
            Timestamp::from_millis(1),
            MarketEvent::Trade(PublicTrade {
                symbol: symbol(),
                price: dec!(63096.01),
                qty: dec!(0.5),
                taker_side: Side::Buy,
                ts: Timestamp::from_millis(1),
                id: 42,
            }),
        )
    }

    fn snapshot_event() -> Event {
        Event::market(
            Timestamp::from_millis(1),
            MarketEvent::BookSnapshot(OrderBook {
                symbol: Some(symbol()),
                bids: vec![BookLevel { price: dec!(100), qty: dec!(1) }],
                asks: vec![BookLevel { price: dec!(101), qty: dec!(1) }],
                last_update_id: 7,
                ts: Timestamp::from_millis(1),
                synced: true,
            }),
        )
    }

    // --- handshake -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_valid_hello_is_welcomed() {
        let h = harness().await;
        let mut socket = connect(&h).await;
        assert_eq!(hello(&mut socket).await, ServerMsg::Welcome { protocol: PROTOCOL_VERSION });
    }

    #[tokio::test]
    async fn a_wrong_token_is_told_no_and_then_disconnected() {
        // Both halves matter. Replying without closing leaves a socket to retry the token on;
        // closing without replying makes a wrong token indistinguishable from a network fault.
        let h = harness().await;
        let mut socket = connect(&h).await;

        say(&mut socket, &ClientMsg::Hello { protocol: PROTOCOL_VERSION, token: "wrong".into() }).await;
        match hear(&mut socket).await {
            ServerMsg::Failed { code, .. } => assert_eq!(code, ErrorCode::Unauthorized),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(closed(&mut socket).await, "a rejected terminal must be disconnected");
    }

    #[tokio::test]
    async fn a_protocol_mismatch_is_refused_before_anything_else() {
        let h = harness().await;
        let mut socket = connect(&h).await;

        // Correct token, wrong version: the version is checked first on purpose, so a peer
        // that cannot parse the reply is not also told its token was fine.
        say(&mut socket, &ClientMsg::Hello { protocol: PROTOCOL_VERSION + 1, token: TOKEN.into() }).await;
        match hear(&mut socket).await {
            ServerMsg::Failed { code, .. } => assert_eq!(code, ErrorCode::ProtocolMismatch),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(closed(&mut socket).await);
    }

    #[tokio::test]
    async fn a_command_before_hello_is_refused_and_the_session_ends() {
        let h = harness().await;
        let mut socket = connect(&h).await;

        say(&mut socket, &ClientMsg::Subscribe(vec![Subscription::Book(symbol())])).await;
        match hear(&mut socket).await {
            ServerMsg::Failed { code, .. } => assert_eq!(code, ErrorCode::NotAuthenticated),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(closed(&mut socket).await);
    }

    // --- framing -------------------------------------------------------------------------

    #[tokio::test]
    async fn a_malformed_frame_is_reported_without_costing_the_session() {
        // One corrupt frame is not an attack, and dropping the connection over it would turn
        // a transient glitch into a reconnect storm.
        let h = harness().await;
        let mut socket = connect(&h).await;
        assert_eq!(hello(&mut socket).await, ServerMsg::Welcome { protocol: PROTOCOL_VERSION });

        socket.send(Message::Binary(vec![0xc1, 0xff, 0x00].into())).await.unwrap();
        match hear(&mut socket).await {
            ServerMsg::Failed { code, .. } => assert_eq!(code, ErrorCode::Malformed),
            other => panic!("expected a decode failure, got {other:?}"),
        }

        // Still usable afterwards.
        say(&mut socket, &ClientMsg::Ping(99)).await;
        assert_eq!(hear(&mut socket).await, ServerMsg::Pong(99));
    }

    #[tokio::test]
    async fn text_frames_are_ignored_rather_than_answered() {
        // A browser or a scanner poking at the port must cost nothing and must not confuse
        // the protocol state machine.
        let h = harness().await;
        let mut socket = connect(&h).await;
        assert_eq!(hello(&mut socket).await, ServerMsg::Welcome { protocol: PROTOCOL_VERSION });

        socket.send(Message::Text("hello?".into())).await.unwrap();
        say(&mut socket, &ClientMsg::Ping(1)).await;
        assert_eq!(hear(&mut socket).await, ServerMsg::Pong(1), "the text frame must not have been answered");
    }

    #[tokio::test]
    async fn a_ping_is_answered_with_its_own_nonce() {
        let h = harness().await;
        let mut socket = connect(&h).await;
        hello(&mut socket).await;

        say(&mut socket, &ClientMsg::Ping(7)).await;
        assert_eq!(hear(&mut socket).await, ServerMsg::Pong(7));
        say(&mut socket, &ClientMsg::Ping(8)).await;
        assert_eq!(hear(&mut socket).await, ServerMsg::Pong(8), "nonces must not be transposed");
    }

    // --- the state handover --------------------------------------------------------------

    #[tokio::test]
    async fn subscribing_hands_over_the_state_that_already_exists() {
        // The bug this guards against cost a whole debugging session: a terminal connecting
        // after the books were built received deltas for a book it did not have and sat blank.
        let h = harness().await;
        h.state.apply(&trade_event());
        h.state.apply(&snapshot_event());

        let mut socket = connect(&h).await;
        hello(&mut socket).await;
        say(&mut socket, &ClientMsg::Subscribe(vec![Subscription::Book(symbol())])).await;

        // The ack first — `Subscribe` is acknowledged with `Pong(0)` — and only then the
        // state. The order matters: a terminal that receives a book before its subscribe was
        // acknowledged cannot tell whether the book belongs to the subscription it just made.
        assert_eq!(hear(&mut socket).await, ServerMsg::Pong(0), "subscribe must be acknowledged first");

        // Then the tape, then the book. Both were in the read-model before the terminal
        // existed, which is exactly the case that used to leave the panel blank.
        let (mut saw_trade, mut saw_snapshot) = (false, false);
        for _ in 0..8 {
            if saw_trade && saw_snapshot {
                break;
            }
            match hear(&mut socket).await {
                ServerMsg::Event(event) => match event.payload {
                    domain::Payload::Market(MarketEvent::Trade(_)) => saw_trade = true,
                    domain::Payload::Market(MarketEvent::BookSnapshot(_)) => saw_snapshot = true,
                    _ => {}
                },
                other => panic!("unexpected message during the handover: {other:?}"),
            }
        }
        assert!(saw_trade, "a subscribing terminal must be handed the tape it missed");
        assert!(saw_snapshot, "a subscribing terminal must be handed the current book");
    }

    #[tokio::test]
    async fn a_subscribed_terminal_receives_live_events() {
        let h = harness().await;
        let mut socket = connect(&h).await;
        hello(&mut socket).await;
        say(&mut socket, &ClientMsg::Subscribe(vec![Subscription::Trades(symbol())])).await;
        // Drain whatever the subscribe produced before publishing.
        let _ = tokio::time::timeout(Duration::from_millis(200), hear(&mut socket)).await;

        h.fanout.send(trade_event()).expect("a session is subscribed to the fanout");

        let mut saw_trade = false;
        for _ in 0..8 {
            if let ServerMsg::Event(event) = hear(&mut socket).await {
                if matches!(event.payload, domain::Payload::Market(MarketEvent::Trade(_))) {
                    saw_trade = true;
                    break;
                }
            }
        }
        assert!(saw_trade, "a live trade must reach a subscribed terminal");
    }

    // --- lifecycle and accounting --------------------------------------------------------

    #[tokio::test]
    async fn the_connected_gauge_rises_and_falls_with_real_sessions() {
        let h = harness().await;
        fn connected(m: &Metrics) -> i64 {
            m.render()
                .lines()
                .find(|l| l.starts_with("mooncore_terminals_connected "))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
                .expect("the gauge is exported")
        }
        assert_eq!(connected(&h.metrics), 0);

        let mut socket = connect(&h).await;
        hello(&mut socket).await;
        assert_eq!(connected(&h.metrics), 1);

        // Closing the socket must bring it back down: a gauge that only rises is worse than
        // no gauge, because it looks like a leak that is not there.
        drop(socket);
        for _ in 0..50 {
            if connected(&h.metrics) == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the gauge stayed at {} after the terminal went away", connected(&h.metrics));
    }

    #[tokio::test]
    async fn a_close_frame_ends_the_session_without_an_error() {
        let h = harness().await;
        let mut socket = connect(&h).await;
        hello(&mut socket).await;

        socket.close(None).await.expect("closing is graceful");
        assert!(closed(&mut socket).await);
    }

    #[tokio::test]
    async fn one_rejected_terminal_does_not_disturb_another() {
        // Sessions are independent tasks; a refusal on one must not take the listener with it.
        let h = harness().await;

        let mut bad = connect(&h).await;
        say(&mut bad, &ClientMsg::Hello { protocol: PROTOCOL_VERSION, token: "wrong".into() }).await;
        let _ = hear(&mut bad).await;

        let mut good = connect(&h).await;
        assert_eq!(hello(&mut good).await, ServerMsg::Welcome { protocol: PROTOCOL_VERSION });
    }

    #[tokio::test]
    async fn an_unauthenticated_socket_never_sees_private_data() {
        // The single invariant the whole split exists to protect. Checked here on the wire,
        // not only in `Session::may_receive`, because this is where a future refactor of the
        // send path could quietly bypass it.
        let h = harness().await;
        let sink = EventSink::new(tokio::sync::mpsc::unbounded_channel().0);
        let _ = sink;

        let mut socket = connect(&h).await;
        // No Hello. Publish a private event and give the core time to make a mistake.
        h.fanout.send(private_event()).ok();

        let quiet = tokio::time::timeout(Duration::from_millis(300), socket.next()).await;
        match quiet {
            Err(_) => {}
            Ok(None) => {}
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let msg = wire::decode_server(&bytes).expect("valid frame");
                assert!(
                    matches!(msg, ServerMsg::Failed { .. }),
                    "an unauthenticated socket received {msg:?}"
                );
            }
            Ok(Some(_)) => {}
        }
    }

    fn private_event() -> Event {
        use domain::{AccountEvent, Balance};
        Event::account(
            Timestamp::from_millis(1),
            AccountEvent::Balances(vec![Balance { asset: "USDT".into(), free: dec!(1000), locked: dec!(0) }]),
        )
    }
}
