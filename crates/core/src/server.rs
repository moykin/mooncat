//! The WebSocket server terminals connect to.

use crate::state::MarketState;
use domain::{ConnectionEvent, Event, MarketEvent, Timestamp};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
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

pub async fn serve(listener: TcpListener, auth: Arc<Auth>, fanout: Fanout, state: MarketState) {
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
        tokio::spawn(async move {
            if let Err(e) = session(stream, auth, events, state).await {
                tracing::info!(%peer, reason = %e, "terminal disconnected");
            }
        });
    }
}

async fn session(
    stream: TcpStream,
    auth: Arc<Auth>,
    mut events: broadcast::Receiver<Event>,
    state: MarketState,
) -> Result<(), String> {
    let socket = tokio_tungstenite::accept_async(stream).await.map_err(|e| e.to_string())?;
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
