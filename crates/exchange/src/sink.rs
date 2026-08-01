//! Where connectors put their events.
//!
//! Unbounded on purpose. A bounded channel would make a slow consumer apply backpressure to
//! the venue reader, and a stalled reader means a stale book — the one failure mode that
//! must never happen quietly. Consumers that cannot keep up are dropped instead, and the
//! core resyncs them.

use domain::Event;
use tokio::sync::mpsc::UnboundedSender;

/// Cloneable handle a connector pushes events into.
#[derive(Clone)]
pub struct EventSink {
    tx: UnboundedSender<Event>,
    /// Events dropped because the receiver is gone. Surfaced in diagnostics rather than
    /// logged per event, which would itself become the bottleneck.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl EventSink {
    pub fn new(tx: UnboundedSender<Event>) -> Self {
        Self { tx, dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)) }
    }

    /// Publish an event.
    ///
    /// Returns false once the receiver is gone. Deliberately infallible from the caller's
    /// point of view: a connector must never abort its read loop because a consumer died.
    pub fn send(&self, event: Event) -> bool {
        if self.tx.send(event).is_ok() {
            return true;
        }
        self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        false
    }

    /// How many events were dropped because no receiver was listening.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ConnectionEvent, Timestamp};

    fn ready() -> Event {
        Event::connection(Timestamp::from_millis(1), ConnectionEvent::Ready)
    }

    #[tokio::test]
    async fn delivers_while_a_receiver_lives() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::new(tx);

        assert!(sink.send(ready()));
        assert_eq!(sink.dropped(), 0);
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn a_dead_consumer_is_counted_not_fatal() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        drop(rx);

        // The connector keeps running; the loss is recorded for diagnostics.
        assert!(!sink.send(ready()));
        assert!(!sink.send(ready()));
        assert_eq!(sink.dropped(), 2);
        assert!(sink.is_closed());
    }
}
