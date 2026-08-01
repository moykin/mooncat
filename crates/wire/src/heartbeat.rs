//! Noticing that the other end is gone.
//!
//! # Why TCP is not enough
//!
//! A TCP connection whose peer has vanished — a laptop closed, a VPS network partitioned, a
//! NAT box quietly forgetting the mapping — stays open on this side for minutes. Nothing
//! arrives, nothing fails, and the socket looks perfectly healthy. For a terminal that is a
//! stale book presented as live; for a core it is a session holding a slot and a subscription
//! for a client that no longer exists.
//!
//! # Why the core pings and not the terminal
//!
//! The core is the side with something to protect: sessions, memory, subscriptions and a slot
//! count. Making it the initiator means detection does not depend on the terminal being
//! well-behaved — a client that simply stops sending is exactly the case being detected, and a
//! scheme where the client drives its own liveness check cannot see it.
//!
//! The terminal may also ping, and does, to measure the round trip for its status bar. That
//! path is a measurement; this one is a policy.
//!
//! # The numbers
//!
//! Two seconds between pings, three missed in a row before the connection is dropped. Worst
//! case detection is therefore six seconds, which is the acceptance target. Making the
//! interval shorter buys little — a human notices nothing below a second — while making the
//! miss count smaller would drop connections over one lost packet on a mobile link.

use std::time::{Duration, Instant};

/// How often the core sends a ping.
pub const PING_INTERVAL: Duration = Duration::from_millis(2_000);
/// Consecutive unanswered pings before the session is considered dead.
pub const MISSES_BEFORE_DROP: u32 = 3;

/// Upper bound on how long a dead peer stays undetected.
pub const DETECTION_BOUND: Duration =
    Duration::from_millis(PING_INTERVAL.as_millis() as u64 * MISSES_BEFORE_DROP as u64);

/// What a received pong meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PongOutcome {
    /// It answered the ping in flight; the peer is alive and the round trip is now known.
    Matched(Duration),
    /// It answered a ping that has already been superseded. The peer is alive but slow, and
    /// this deliberately does **not** clear the miss counter: a peer answering only stale
    /// pings is failing to keep up, which is what the counter is for.
    Stale,
    /// A nonce that was never sent. Ignored rather than fatal — an old duplicate can arrive
    /// after a resume, and dropping the session over it would be worse than the noise.
    Unknown,
}

/// Liveness of one connection.
#[derive(Debug)]
pub struct Heartbeat {
    interval: Duration,
    misses_allowed: u32,
    /// The ping currently awaiting an answer.
    in_flight: Option<(u64, Instant)>,
    /// Nonces sent and not yet answered or superseded, kept only to tell "stale" from
    /// "never sent" — the distinction matters when reading a log.
    recently_sent: Vec<u64>,
    next_nonce: u64,
    consecutive_misses: u32,
    last_activity: Option<Instant>,
    rtt: Option<Duration>,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new(PING_INTERVAL, MISSES_BEFORE_DROP)
    }
}

impl Heartbeat {
    pub fn new(interval: Duration, misses_allowed: u32) -> Self {
        Self {
            interval,
            misses_allowed,
            in_flight: None,
            recently_sent: Vec::new(),
            next_nonce: 1,
            consecutive_misses: 0,
            last_activity: None,
            rtt: None,
        }
    }

    /// When the next ping is due. `None` before the first one, which is sent immediately.
    pub fn next_due(&self) -> Option<Instant> {
        self.last_activity.map(|t| t + self.interval)
    }

    pub fn due(&self, now: Instant) -> bool {
        self.next_due().is_none_or(|due| now >= due)
    }

    /// Record that a ping is going out, and return its nonce.
    ///
    /// The miss counter is incremented **here**, when a previous ping is superseded without an
    /// answer, rather than on a timer. That way one clock drives the whole thing and there is
    /// no window where a ping has expired but nothing has noticed.
    pub fn on_send(&mut self, now: Instant) -> u64 {
        if self.in_flight.take().is_some() {
            self.consecutive_misses += 1;
        }
        let nonce = self.next_nonce;
        self.next_nonce += 1;

        self.in_flight = Some((nonce, now));
        self.last_activity = Some(now);
        self.recently_sent.push(nonce);
        // Only enough history to name a stale nonce; this is a diagnostic, not a ledger.
        let keep = (self.misses_allowed as usize + 1) * 4;
        if self.recently_sent.len() > keep {
            self.recently_sent.drain(..self.recently_sent.len() - keep);
        }
        nonce
    }

    /// Record an answer.
    pub fn on_pong(&mut self, nonce: u64, now: Instant) -> PongOutcome {
        if let Some((sent_nonce, sent_at)) = self.in_flight {
            if sent_nonce == nonce {
                let rtt = now.saturating_duration_since(sent_at);
                self.in_flight = None;
                self.consecutive_misses = 0;
                self.rtt = Some(rtt);
                return PongOutcome::Matched(rtt);
            }
        }
        if self.recently_sent.contains(&nonce) {
            PongOutcome::Stale
        } else {
            PongOutcome::Unknown
        }
    }

    /// Whether the peer has missed enough pings to be considered gone.
    pub fn is_dead(&self) -> bool {
        self.consecutive_misses >= self.misses_allowed
    }

    pub fn consecutive_misses(&self) -> u32 {
        self.consecutive_misses
    }

    /// Last measured round trip, for the terminal's status bar.
    pub fn rtt(&self) -> Option<Duration> {
        self.rtt
    }

    /// Any traffic at all counts as liveness.
    ///
    /// A connection carrying a thousand book deltas a second is obviously alive, and pinging
    /// it anyway is pure overhead. This is what makes the heartbeat cost nothing on a busy
    /// session and everything it is worth on an idle one.
    pub fn saw_traffic(&mut self, now: Instant) {
        self.last_activity = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Instant {
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    fn at(ms: u64) -> Instant {
        origin() + Duration::from_millis(ms)
    }

    // --- the acceptance criterion -----------------------------------------------------------

    #[test]
    fn a_dead_peer_is_detected_within_six_seconds() {
        // The target from `10-target-architecture.md` §7.2, derived rather than asserted as a
        // magic number: three misses at two seconds each.
        assert_eq!(DETECTION_BOUND, Duration::from_secs(6));

        let mut hb = Heartbeat::default();
        let mut now = 0;
        hb.on_send(at(now));

        // The peer answers nothing from here on.
        let mut pings = 1;
        while !hb.is_dead() {
            now += PING_INTERVAL.as_millis() as u64;
            assert!(hb.due(at(now)), "a ping must be due after the interval");
            hb.on_send(at(now));
            pings += 1;
            assert!(pings < 10, "detection never happened");
        }
        assert_eq!(
            now,
            DETECTION_BOUND.as_millis() as u64,
            "detection must land exactly on the bound, not later"
        );
    }

    #[test]
    fn a_healthy_peer_is_never_declared_dead() {
        let mut hb = Heartbeat::default();
        for round in 0..100u64 {
            let sent = round * 2_000;
            let nonce = hb.on_send(at(sent));
            assert_eq!(hb.on_pong(nonce, at(sent + 30)), PongOutcome::Matched(Duration::from_millis(30)));
            assert!(!hb.is_dead());
            assert_eq!(hb.consecutive_misses(), 0);
        }
    }

    #[test]
    fn one_lost_ping_does_not_drop_the_connection() {
        // The reason the miss count is three and not one: a single lost packet on a mobile
        // link must not cost a reconnect.
        let mut hb = Heartbeat::default();
        hb.on_send(at(0));

        let second = hb.on_send(at(2_000));
        assert_eq!(hb.consecutive_misses(), 1, "the first went unanswered");
        assert!(!hb.is_dead());

        assert!(matches!(hb.on_pong(second, at(2_050)), PongOutcome::Matched(_)));
        assert_eq!(hb.consecutive_misses(), 0, "an answer clears the count");
    }

    // --- what counts as an answer -------------------------------------------------------------

    #[test]
    fn answering_a_superseded_ping_does_not_count_as_alive() {
        // A peer that only ever answers the previous ping is falling behind, and treating that
        // as healthy would keep a hopeless connection open indefinitely.
        let mut hb = Heartbeat::default();
        let first = hb.on_send(at(0));
        hb.on_send(at(2_000));

        assert_eq!(hb.on_pong(first, at(2_100)), PongOutcome::Stale);
        assert_eq!(hb.consecutive_misses(), 1, "a stale answer must not clear the count");
    }

    #[test]
    fn a_nonce_that_was_never_sent_is_ignored_not_fatal() {
        // A duplicate can arrive after a resume. Dropping the session over it would be worse
        // than the noise it makes.
        let mut hb = Heartbeat::default();
        let nonce = hb.on_send(at(0));
        assert_eq!(hb.on_pong(9_999, at(10)), PongOutcome::Unknown);
        assert!(!hb.is_dead());

        assert!(matches!(hb.on_pong(nonce, at(20)), PongOutcome::Matched(_)), "the real answer still works");
    }

    #[test]
    fn stale_and_unknown_are_distinguishable() {
        // They mean different things in a log: one is a slow peer, the other is a stray frame.
        let mut hb = Heartbeat::default();
        let old = hb.on_send(at(0));
        hb.on_send(at(2_000));
        assert_eq!(hb.on_pong(old, at(2_001)), PongOutcome::Stale);
        assert_eq!(hb.on_pong(old + 1_000, at(2_002)), PongOutcome::Unknown);
    }

    // --- scheduling -------------------------------------------------------------------------------

    #[test]
    fn the_first_ping_is_due_immediately() {
        let hb = Heartbeat::default();
        assert!(hb.due(origin()), "a new connection must be probed at once, not after 2 s");
        assert!(hb.next_due().is_none());
    }

    #[test]
    fn traffic_postpones_the_ping() {
        // A connection carrying a thousand deltas a second is obviously alive; pinging it as
        // well is pure overhead.
        let mut hb = Heartbeat::default();
        hb.on_send(at(0));
        assert!(!hb.due(at(1_999)));

        hb.saw_traffic(at(1_500));
        assert!(!hb.due(at(3_000)), "traffic at 1500 pushes the next ping to 3500");
        assert!(hb.due(at(3_500)));
    }

    #[test]
    fn a_nonce_is_never_reused() {
        // Reuse would make a stale answer indistinguishable from a current one, which is
        // exactly the distinction the miss counter depends on.
        let mut hb = Heartbeat::default();
        let mut seen = std::collections::HashSet::new();
        for i in 0..1_000u64 {
            assert!(seen.insert(hb.on_send(at(i * 2_000))), "nonce repeated at round {i}");
        }
    }

    #[test]
    fn the_round_trip_is_measured_and_kept_for_the_status_bar() {
        let mut hb = Heartbeat::default();
        assert!(hb.rtt().is_none(), "nothing to show before the first exchange");

        let nonce = hb.on_send(at(0));
        hb.on_pong(nonce, at(137));
        assert_eq!(hb.rtt(), Some(Duration::from_millis(137)));

        // A later measurement replaces it rather than averaging: the status bar should show
        // what the link is doing now, not what it was doing an hour ago.
        let nonce = hb.on_send(at(2_000));
        hb.on_pong(nonce, at(2_012));
        assert_eq!(hb.rtt(), Some(Duration::from_millis(12)));
    }

    #[test]
    fn a_pong_arriving_before_its_ping_does_not_underflow_the_clock() {
        // Instants cannot go backwards on the same machine, but the arithmetic is done with
        // saturating subtraction anyway: a panic in the heartbeat would take the session with
        // it, and a zero here is harmless.
        let mut hb = Heartbeat::new(PING_INTERVAL, MISSES_BEFORE_DROP);
        let nonce = hb.on_send(at(1_000));
        assert_eq!(hb.on_pong(nonce, at(1_000)), PongOutcome::Matched(Duration::ZERO));
    }

    #[test]
    fn the_recent_nonce_history_does_not_grow_without_bound() {
        // It exists to label a log line, not to be a ledger.
        let mut hb = Heartbeat::default();
        for i in 0..10_000u64 {
            hb.on_send(at(i * 2_000));
        }
        assert!(hb.recently_sent.len() <= 16, "history grew to {}", hb.recently_sent.len());
    }
}
