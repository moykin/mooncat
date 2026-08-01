//! The operational surface: `/metrics` and `/health`.
//!
//! Served on its own listener, separate from the wire. The wire may face the network; this
//! should not have to, and giving it its own address means the choice is the operator's
//! rather than a consequence of how the code is arranged.
//!
//! HTTP is answered by hand rather than by a framework. What is needed is two routes, no
//! request body, no routing table and no middleware — against which a web stack is a
//! dependency shipped to the VPS, a build-time cost, and a supply chain, all to format
//! roughly forty lines of text.
//!
//! ## Why the drop counters matter more than anything else here
//!
//! There are two places where market data can be lost, and both were previously invisible:
//! [`exchange::EventSink::dropped`] counted events discarded because no consumer existed,
//! and nothing ever read it; `broadcast` lag drops events for a terminal that fell behind.
//! Silence in either case looks exactly like a quiet market. These are the numbers that say
//! otherwise.

use exchange::EventSink;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
    /// Held so the counter of events dropped for want of a consumer can be read out.
    sink: EventSink,
}

struct Inner {
    started: Instant,
    terminals_connected: AtomicI64,
    terminals_total: AtomicU64,
    events_broadcast: AtomicU64,
    events_lagged: AtomicU64,
}

impl Metrics {
    pub fn new(sink: EventSink) -> Self {
        Self {
            inner: Arc::new(Inner {
                started: Instant::now(),
                terminals_connected: AtomicI64::new(0),
                terminals_total: AtomicU64::new(0),
                events_broadcast: AtomicU64::new(0),
                events_lagged: AtomicU64::new(0),
            }),
            sink,
        }
    }

    /// A terminal finished its handshake. Returns a guard that decrements on drop, so a
    /// session ending by panic, error or disconnect cannot leak the gauge upward — which is
    /// exactly how a "connected terminals" number becomes fiction over a long uptime.
    pub fn terminal_connected(&self) -> TerminalGuard {
        self.inner.terminals_connected.fetch_add(1, Ordering::Relaxed);
        self.inner.terminals_total.fetch_add(1, Ordering::Relaxed);
        TerminalGuard(self.inner.clone())
    }

    pub fn event_broadcast(&self) {
        self.inner.events_broadcast.fetch_add(1, Ordering::Relaxed);
    }

    /// A terminal fell behind and the broadcast dropped `count` events for it.
    pub fn events_lagged(&self, count: u64) {
        self.inner.events_lagged.fetch_add(count, Ordering::Relaxed);
    }

    /// Prometheus text exposition. Every series carries HELP and TYPE: an unlabelled number
    /// on a dashboard at three in the morning is worse than no number.
    pub fn render(&self) -> String {
        let i = &self.inner;
        let mut out = String::with_capacity(1024);

        let metric = |out: &mut String, name: &str, help: &str, kind: &str, value: String| {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"));
        };

        metric(
            &mut out,
            "mooncore_uptime_seconds",
            "Seconds since the core started.",
            "gauge",
            i.started.elapsed().as_secs().to_string(),
        );
        metric(
            &mut out,
            "mooncore_terminals_connected",
            "Terminals with an open session right now.",
            "gauge",
            i.terminals_connected.load(Ordering::Relaxed).to_string(),
        );
        metric(
            &mut out,
            "mooncore_terminals_total",
            "Terminal sessions accepted since start.",
            "counter",
            i.terminals_total.load(Ordering::Relaxed).to_string(),
        );
        metric(
            &mut out,
            "mooncore_events_broadcast_total",
            "Market events published to the fanout.",
            "counter",
            i.events_broadcast.load(Ordering::Relaxed).to_string(),
        );
        metric(
            &mut out,
            "mooncore_events_dropped_total",
            "Events a connector produced that no consumer was there to receive. \
             Non-zero means the core lost market data.",
            "counter",
            self.sink.dropped().to_string(),
        );
        metric(
            &mut out,
            "mooncore_events_lagged_total",
            "Events dropped for terminals that could not keep up. \
             Non-zero means some terminal rendered a stale book until it resynced.",
            "counter",
            i.events_lagged.load(Ordering::Relaxed).to_string(),
        );
        out
    }

    /// Whether the core still has a path from the connectors to the terminals.
    ///
    /// A closed sink means every connector is writing into nothing: the process is alive, the
    /// port answers, and no market data can ever arrive again. Reporting that as healthy is
    /// the failure mode a health check exists to prevent.
    pub fn healthy(&self) -> bool {
        !self.sink.is_closed()
    }
}

/// Decrements the connected-terminals gauge when a session ends, however it ends.
pub struct TerminalGuard(Arc<Inner>);

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.0.terminals_connected.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Answer `/metrics` and `/health` until the listener dies.
pub async fn serve(listener: TcpListener, metrics: Metrics) {
    loop {
        let Ok((mut stream, _peer)) = listener.accept().await else {
            continue;
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let Ok(n) = stream.read(&mut buf).await else {
                return;
            };
            let response = respond(&buf[..n], &metrics);
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
    }
}

/// Route on the request line alone. Split out from the socket so it can be tested without one.
fn respond(request: &[u8], metrics: &Metrics) -> String {
    let head = String::from_utf8_lossy(request);
    let mut parts = head.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    // Only GET. A scanner POSTing at an ops port should get a flat refusal, not an attempt.
    if method != "GET" {
        return http(405, "text/plain; charset=utf-8", "method not allowed\n");
    }

    match path {
        "/metrics" => http(200, "text/plain; version=0.0.4; charset=utf-8", &metrics.render()),
        "/health" if metrics.healthy() => http(200, "text/plain; charset=utf-8", "ok\n"),
        "/health" => http(
            503,
            "text/plain; charset=utf-8",
            "connector event stream is closed — no market data can arrive\n",
        ),
        _ => http(404, "text/plain; charset=utf-8", "not found\n"),
    }
}

fn http(status: u16, content_type: &str, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ConnectionEvent, Event, Timestamp};

    fn fixture() -> (Metrics, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Metrics::new(EventSink::new(tx)), rx)
    }

    fn ready() -> Event {
        Event::connection(Timestamp::from_millis(1), ConnectionEvent::Ready)
    }

    fn body_of(response: &str) -> &str {
        response.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
    }

    /// Reads one `name value` series out of the exposition.
    fn series(text: &str, name: &str) -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name) && !l.starts_with('#'))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
    }

    #[test]
    fn dropped_counter_is_exported() {
        // The acceptance criterion for task 0.6: a drop in the sink must become visible.
        let (metrics, rx) = fixture();
        assert_eq!(series(&metrics.render(), "mooncore_events_dropped_total"), Some(0));

        // Losing the consumer is what makes the sink start counting.
        drop(rx);
        assert!(!metrics.sink.send(ready()));
        assert!(!metrics.sink.send(ready()));

        assert_eq!(series(&metrics.render(), "mooncore_events_dropped_total"), Some(2));
    }

    #[test]
    fn lagged_events_are_counted_separately_from_dropped() {
        // Two different losses with two different causes: a slow terminal is not a dead
        // connector, and one number for both would make neither actionable.
        let (metrics, _rx) = fixture();
        metrics.events_lagged(7);
        let text = metrics.render();
        assert_eq!(series(&text, "mooncore_events_lagged_total"), Some(7));
        assert_eq!(series(&text, "mooncore_events_dropped_total"), Some(0));
    }

    #[test]
    fn the_connected_gauge_returns_to_zero_when_a_session_ends() {
        let (metrics, _rx) = fixture();
        {
            let _a = metrics.terminal_connected();
            let _b = metrics.terminal_connected();
            assert_eq!(series(&metrics.render(), "mooncore_terminals_connected"), Some(2));
        }
        let text = metrics.render();
        assert_eq!(series(&text, "mooncore_terminals_connected"), Some(0));
        // The total is a counter and must not come back down with it.
        assert_eq!(series(&text, "mooncore_terminals_total"), Some(2));
    }

    #[test]
    fn every_series_declares_help_and_type() {
        let (metrics, _rx) = fixture();
        let text = metrics.render();
        let names: Vec<_> = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        assert!(!names.is_empty());
        for name in names {
            assert!(text.contains(&format!("# HELP {name} ")), "{name} has no HELP");
            assert!(text.contains(&format!("# TYPE {name} ")), "{name} has no TYPE");
        }
    }

    #[test]
    fn health_is_503_once_the_event_stream_is_gone() {
        let (metrics, rx) = fixture();
        assert!(respond(b"GET /health HTTP/1.1\r\n\r\n", &metrics).starts_with("HTTP/1.1 200"));

        // Every connector is now writing into nothing. The process is up and useless.
        drop(rx);
        let response = respond(b"GET /health HTTP/1.1\r\n\r\n", &metrics);
        assert!(response.starts_with("HTTP/1.1 503"), "got: {response}");
    }

    #[test]
    fn unknown_paths_and_methods_are_refused() {
        let (metrics, _rx) = fixture();
        assert!(respond(b"GET /admin HTTP/1.1\r\n\r\n", &metrics).starts_with("HTTP/1.1 404"));
        assert!(respond(b"POST /metrics HTTP/1.1\r\n\r\n", &metrics).starts_with("HTTP/1.1 405"));
        // A blank or malformed request must not panic the ops listener.
        assert!(respond(b"", &metrics).starts_with("HTTP/1.1 405"));
        assert!(respond(b"\x00\x01\x02", &metrics).starts_with("HTTP/1.1 405"));
    }

    #[test]
    fn the_content_length_matches_the_body() {
        // Prometheus is stricter than a browser about this, and a mismatch shows up as a
        // scrape that hangs rather than as an error.
        let (metrics, _rx) = fixture();
        let response = respond(b"GET /metrics HTTP/1.1\r\n\r\n", &metrics);
        let declared: usize = response
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .and_then(|v| v.trim().parse().ok())
            .expect("Content-Length is present");
        assert_eq!(declared, body_of(&response).len());
    }
}
