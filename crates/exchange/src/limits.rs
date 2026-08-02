//! Staying inside what the venue will accept.
//!
//! # Why not a leaky bucket
//!
//! A leaky bucket smooths a rate. Binance does not limit a rate — it counts **weight inside a
//! wall-clock window** and resets the counter when the window turns over. The two behave
//! differently in exactly the case that matters: a burst of forty requests at the top of a
//! minute is fine under Binance's rules and is throttled by a bucket, while a bucket tuned to
//! allow it will happily let the same burst through at the fifty-ninth second, which is a ban.
//!
//! The reverse-engineering report on the original notes it uses weighted windows too
//! (report 08 §6.4); modelling anything else means being wrong at the edges of every minute.
//!
//! # Why the check happens before the request, not after the rejection
//!
//! A `429` is not free. Binance escalates repeats into a `418` and then an IP ban measured in
//! minutes to days, and a ban during a position is not an inconvenience — it is being unable
//! to close. So the limiter refuses locally and says how long to wait; nothing is sent that
//! was going to be rejected.
//!
//! # Why the venue's own count wins
//!
//! Our counter is an estimate. Another process on the same address, a request whose weight we
//! guessed wrong, a retry we did not account for — all make it drift low, and drifting low is
//! the dangerous direction. Binance reports the truth in `X-MBX-USED-WEIGHT-1M` on every
//! response, so [`RateLimiter::observe_used`] adopts it. Only upward: taking a lower number
//! from a response that raced with our own increment would undo an increment we still owe.

use std::time::{Duration, Instant};

/// One counted window.
#[derive(Clone, Debug)]
pub struct Window {
    limit: u32,
    span: Duration,
    used: u32,
    /// When the current window began. Windows turn over rather than sliding, because that is
    /// what the venue does.
    started: Instant,
}

impl Window {
    pub fn new(limit: u32, span: Duration, now: Instant) -> Self {
        Self { limit, span, used: 0, started: now }
    }

    fn roll(&mut self, now: Instant) {
        if now.duration_since(self.started) >= self.span {
            // Jump whole spans rather than resetting to `now`: a limiter idle for ten minutes
            // must land on the venue's window boundary, not invent its own.
            let elapsed = now.duration_since(self.started).as_nanos();
            let spans = elapsed / self.span.as_nanos().max(1);
            self.started += self.span * spans as u32;
            self.used = 0;
        }
    }

    /// How long until `cost` would fit, or zero if it fits now.
    fn wait_for(&self, cost: u32, now: Instant) -> Duration {
        if self.used + cost <= self.limit {
            return Duration::ZERO;
        }
        (self.started + self.span).saturating_duration_since(now)
    }

    fn charge(&mut self, cost: u32) {
        self.used = self.used.saturating_add(cost);
    }

    pub fn used(&self) -> u32 {
        self.used
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used)
    }
}

/// What kind of budget a request draws on.
///
/// Order placement counts against both the shared weight budget and its own counters, and the
/// order counters are the tighter of the two — which is why a burst of cancels can be refused
/// while a burst of book snapshots is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Anything that is not an order: market data, account queries.
    Request,
    /// Placing, amending or cancelling.
    Order,
}

/// The venue's published limits.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Weight allowed per minute against the source address.
    pub weight_per_minute: u32,
    /// Orders per ten seconds.
    pub orders_per_10s: u32,
    /// Orders per minute.
    pub orders_per_minute: u32,
    /// Orders per day.
    pub orders_per_day: u32,
}

impl Profile {
    /// Binance spot, from its published limits.
    pub const BINANCE_SPOT: Self = Self {
        weight_per_minute: 6_000,
        orders_per_10s: 100,
        orders_per_minute: 1_200,
        orders_per_day: 200_000,
    };

    /// Binance USD-margined futures, which counts differently from spot.
    pub const BINANCE_USDM: Self = Self {
        weight_per_minute: 2_400,
        orders_per_10s: 300,
        orders_per_minute: 1_200,
        orders_per_day: 200_000,
    };
}

/// Whether to send now, and if not, when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Inside every budget. The cost has been charged.
    Send,
    /// Would exceed one. Nothing was charged and nothing should be sent.
    Wait { for_: Duration, budget: &'static str },
}

impl Decision {
    pub fn is_send(self) -> bool {
        matches!(self, Self::Send)
    }
}

/// One venue's budgets, for one set of credentials on one address.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    weight: Window,
    orders_10s: Window,
    orders_1m: Window,
    orders_1d: Window,
    /// How much of the budget to leave unused as a margin for error.
    ///
    /// Our accounting can be wrong — a weight we guessed, another process on the same address,
    /// a response still in flight. Running to exactly the limit means any of those is a `429`.
    headroom_permille: u32,
    deferred: u64,
}

/// Five per cent held back. Small enough not to matter at normal rates, large enough to absorb
/// one request whose weight was guessed wrong.
const DEFAULT_HEADROOM_PERMILLE: u32 = 50;

impl RateLimiter {
    pub fn new(profile: &Profile, now: Instant) -> Self {
        Self {
            weight: Window::new(profile.weight_per_minute, Duration::from_secs(60), now),
            orders_10s: Window::new(profile.orders_per_10s, Duration::from_secs(10), now),
            orders_1m: Window::new(profile.orders_per_minute, Duration::from_secs(60), now),
            orders_1d: Window::new(profile.orders_per_day, Duration::from_secs(86_400), now),
            headroom_permille: DEFAULT_HEADROOM_PERMILLE,
            deferred: 0,
        }
    }

    /// Ask whether a request may go out, charging it if so.
    ///
    /// Charging here rather than on success is deliberate: a request that is sent and then
    /// fails still consumed weight at the venue, so a limiter that only counted successes
    /// would drift low exactly while things were going wrong.
    pub fn check(&mut self, kind: Kind, weight: u32, now: Instant) -> Decision {
        self.roll(now);

        let effective = self.with_headroom(weight);
        let mut budgets: Vec<(&'static str, Duration)> =
            vec![("weight", self.weight.wait_for(effective, now))];

        if kind == Kind::Order {
            budgets.push(("orders_10s", self.orders_10s.wait_for(1, now)));
            budgets.push(("orders_1m", self.orders_1m.wait_for(1, now)));
            budgets.push(("orders_1d", self.orders_1d.wait_for(1, now)));
        }

        // The longest wait wins: satisfying one budget while another is exhausted would send a
        // request that gets rejected anyway.
        if let Some((budget, wait)) =
            budgets.into_iter().filter(|(_, w)| !w.is_zero()).max_by_key(|(_, w)| *w)
        {
            self.deferred += 1;
            return Decision::Wait { for_: wait, budget };
        }

        self.weight.charge(weight);
        if kind == Kind::Order {
            self.orders_10s.charge(1);
            self.orders_1m.charge(1);
            self.orders_1d.charge(1);
        }
        Decision::Send
    }

    /// Adopt the venue's own count of the weight used this minute.
    ///
    /// Only upward. A lower number from a response that raced with our own increment would
    /// undo an increment we still owe, and undercounting is the direction that ends in a ban.
    pub fn observe_used(&mut self, venue_used: u32, now: Instant) {
        self.roll(now);
        if venue_used > self.weight.used {
            self.weight.used = venue_used;
        }
    }

    /// The venue rejected us for rate limiting anyway. Treat its own retry hint as authoritative
    /// and stop sending until it passes.
    ///
    /// Reaching here means the local accounting was wrong, so the response is to give up the
    /// whole window rather than to shave a little off: at this point our estimate has already
    /// been proven untrustworthy.
    pub fn observe_rejection(&mut self, retry_after: Option<Duration>, now: Instant) {
        self.weight.used = self.weight.limit;
        if let Some(after) = retry_after {
            // Push the window's end out to where the venue says it is.
            let ends_at = now + after;
            self.weight.started = ends_at.checked_sub(self.weight.span).unwrap_or(now);
        }
    }

    /// Requests refused locally rather than sent into a rejection. Exported as a metric: a
    /// number that climbs steadily means the limits are too tight for what is being asked.
    pub fn deferred(&self) -> u64 {
        self.deferred
    }

    pub fn weight_window(&self) -> &Window {
        &self.weight
    }

    fn roll(&mut self, now: Instant) {
        self.weight.roll(now);
        self.orders_10s.roll(now);
        self.orders_1m.roll(now);
        self.orders_1d.roll(now);
    }

    /// Inflate a cost by the headroom, so the budget is treated as slightly smaller than it is.
    fn with_headroom(&self, weight: u32) -> u32 {
        weight + (self.weight.limit * self.headroom_permille / 1_000).max(1)
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

    fn limiter() -> RateLimiter {
        RateLimiter::new(&Profile::BINANCE_SPOT, origin())
    }

    // --- the acceptance criterion --------------------------------------------------------

    #[test]
    fn a_storm_of_a_thousand_requests_a_second_produces_no_rejection() {
        // The acceptance criterion for task 3.5. What is asserted is not that everything got
        // through — most of it should not — but that nothing was *sent* that the venue would
        // have refused. A 429 escalates to a 418 and then to an IP ban, and a ban during an
        // open position means being unable to close it.
        let mut rl = limiter();
        let profile = Profile::BINANCE_SPOT;

        let (mut sent, mut deferred) = (0u32, 0u32);
        let mut weight_this_minute = 0u32;
        let mut minute = 0u64;

        for ms in 0..60_000u64 {
            // A thousand a second, weight 5 each — a book snapshot.
            if ms % 60_000 < 60_000 && ms / 60_000 != minute {
                minute = ms / 60_000;
                weight_this_minute = 0;
            }
            for _ in 0..1 {
                match rl.check(Kind::Request, 5, at(ms)) {
                    Decision::Send => {
                        sent += 1;
                        weight_this_minute += 5;
                    }
                    Decision::Wait { .. } => deferred += 1,
                }
            }
        }

        println!("{sent} sent, {deferred} deferred, {weight_this_minute} weight used in the minute");
        assert!(
            weight_this_minute <= profile.weight_per_minute,
            "sent {weight_this_minute} weight against a limit of {}",
            profile.weight_per_minute
        );
        assert!(deferred > 0, "the storm must actually have been throttled");
        assert_eq!(rl.deferred(), deferred as u64);
    }

    #[test]
    fn an_exhausted_budget_defers_rather_than_sending() {
        // The whole point: nothing goes out that was going to come back rejected.
        let mut rl = limiter();
        while rl.check(Kind::Request, 100, at(0)).is_send() {}

        match rl.check(Kind::Request, 100, at(0)) {
            Decision::Wait { for_, budget } => {
                assert_eq!(budget, "weight");
                assert!(for_ > Duration::ZERO && for_ <= Duration::from_secs(60));
            }
            Decision::Send => panic!("an exhausted budget must not send"),
        }
    }

    #[test]
    fn a_deferred_request_costs_nothing() {
        // A refusal that still charged would ratchet the counter up on a request that never
        // went out, and the limiter would strangle itself.
        let mut rl = limiter();
        while rl.check(Kind::Request, 100, at(0)).is_send() {}
        let used = rl.weight_window().used();

        for _ in 0..100 {
            rl.check(Kind::Request, 100, at(0));
        }
        assert_eq!(rl.weight_window().used(), used, "deferrals must not accumulate weight");
    }

    // --- windows turn over, they do not slide ------------------------------------------------

    #[test]
    fn the_budget_returns_when_the_window_turns_over() {
        let mut rl = limiter();
        while rl.check(Kind::Request, 100, at(0)).is_send() {}
        assert!(!rl.check(Kind::Request, 100, at(59_999)).is_send());
        assert!(rl.check(Kind::Request, 100, at(60_000)).is_send(), "a new minute is a new budget");
    }

    #[test]
    fn a_long_idle_period_lands_on_a_window_boundary_not_on_now() {
        // A limiter that reset to `now` would drift out of step with the venue's own minute,
        // and the drift is what puts a burst on the wrong side of a boundary.
        let mut rl = limiter();
        rl.check(Kind::Request, 10, at(0));
        rl.check(Kind::Request, 10, at(600_500));

        let window = rl.weight_window();
        let offset = window.started.duration_since(origin()).as_millis() % 60_000;
        assert_eq!(offset, 0, "the window drifted by {offset} ms");
    }

    #[test]
    fn a_burst_at_the_top_of_a_minute_is_allowed() {
        // The case a leaky bucket gets wrong. Binance counts per window, so forty requests at
        // once is fine; a bucket would smooth them out for no reason.
        let mut rl = limiter();
        for i in 0..40 {
            assert!(rl.check(Kind::Request, 5, at(0)).is_send(), "request {i} was throttled");
        }
    }

    // --- order budgets are separate and tighter ------------------------------------------------

    #[test]
    fn orders_are_limited_by_their_own_counters_before_the_weight_runs_out() {
        // A hundred orders in ten seconds is far below the weight budget, so a limiter that
        // only counted weight would sail past the order limit and get rejected.
        let mut rl = limiter();
        let mut placed = 0;
        while rl.check(Kind::Order, 1, at(0)).is_send() {
            placed += 1;
            assert!(placed <= Profile::BINANCE_SPOT.orders_per_10s, "the 10-second cap was ignored");
        }
        assert_eq!(placed, Profile::BINANCE_SPOT.orders_per_10s);
    }

    #[test]
    fn a_request_is_not_charged_against_the_order_counters() {
        // Otherwise fetching a book would eat the budget for placing orders, and a busy chart
        // would make trading impossible.
        let mut rl = limiter();
        for _ in 0..500 {
            rl.check(Kind::Request, 1, at(0));
        }
        assert!(rl.check(Kind::Order, 1, at(0)).is_send(), "queries must not consume order budget");
    }

    #[test]
    fn the_longest_wait_wins_when_several_budgets_are_exhausted() {
        // Satisfying one while another is exhausted would send something rejected anyway.
        let mut rl = limiter();
        while rl.check(Kind::Order, 1, at(0)).is_send() {}

        match rl.check(Kind::Order, 1, at(0)) {
            Decision::Wait { budget, for_ } => {
                assert_eq!(budget, "orders_10s", "the tightest budget is the ten-second one");
                assert!(for_ <= Duration::from_secs(10));
            }
            Decision::Send => panic!("expected a deferral"),
        }
    }

    #[test]
    fn the_ten_second_budget_returns_before_the_daily_one() {
        let mut rl = limiter();
        while rl.check(Kind::Order, 1, at(0)).is_send() {}
        assert!(rl.check(Kind::Order, 1, at(10_000)).is_send(), "ten seconds is a new sub-window");
    }

    // --- the venue's own count wins -----------------------------------------------------------------

    #[test]
    fn the_venue_header_corrects_an_undercount() {
        // Our number is an estimate: another process on the same address, a weight we guessed
        // wrong, a retry we did not account for. Drifting low is the direction that ends in a
        // ban, so the venue's figure is adopted.
        let mut rl = limiter();
        rl.check(Kind::Request, 10, at(0));
        assert_eq!(rl.weight_window().used(), 10);

        rl.observe_used(5_900, at(0));
        assert_eq!(rl.weight_window().used(), 5_900);
        assert!(!rl.check(Kind::Request, 200, at(0)).is_send(), "and it takes effect at once");
    }

    #[test]
    fn the_venue_header_never_lowers_our_count() {
        // A response can race with our own increment. Taking its lower number would undo an
        // increment we still owe.
        let mut rl = limiter();
        rl.check(Kind::Request, 500, at(0));
        rl.observe_used(100, at(0));
        assert_eq!(rl.weight_window().used(), 500, "a stale header must not lower the count");
    }

    #[test]
    fn an_actual_rejection_gives_up_the_whole_window() {
        // Reaching here means the local accounting was already wrong, so shaving a little off
        // would be trusting an estimate that has just been disproved.
        let mut rl = limiter();
        assert!(rl.check(Kind::Request, 1, at(0)).is_send());

        rl.observe_rejection(Some(Duration::from_secs(30)), at(1_000));
        assert!(!rl.check(Kind::Request, 1, at(1_000)).is_send());
        assert!(!rl.check(Kind::Request, 1, at(30_000)).is_send(), "still inside the venue's hint");
        assert!(rl.check(Kind::Request, 1, at(31_001)).is_send(), "and released after it");
    }

    #[test]
    fn a_rejection_without_a_hint_still_stops_the_window() {
        let mut rl = limiter();
        rl.observe_rejection(None, at(0));
        assert!(!rl.check(Kind::Request, 1, at(0)).is_send());
    }

    // --- headroom -----------------------------------------------------------------------------------

    #[test]
    fn the_budget_is_never_run_to_exactly_the_limit() {
        // Our accounting can be wrong by one request. Running to the edge means any of those
        // errors is a rejection, and the margin costs five per cent of a budget that is rarely
        // the binding constraint anyway.
        let mut rl = limiter();
        while rl.check(Kind::Request, 1, at(0)).is_send() {}

        let used = rl.weight_window().used();
        let limit = rl.weight_window().limit();
        assert!(used < limit, "ran to {used} of {limit} with no margin left");
        assert!(limit - used >= limit / 100, "the margin is too thin to absorb one mistake");
    }

    #[test]
    fn futures_and_spot_have_different_budgets() {
        // They genuinely differ, and a single shared limiter would be wrong for one of them.
        assert_ne!(Profile::BINANCE_SPOT.weight_per_minute, Profile::BINANCE_USDM.weight_per_minute);
        assert_ne!(Profile::BINANCE_SPOT.orders_per_10s, Profile::BINANCE_USDM.orders_per_10s);

        let mut usdm = RateLimiter::new(&Profile::BINANCE_USDM, origin());
        let mut sent = 0;
        while usdm.check(Kind::Request, 1, at(0)).is_send() {
            sent += 1;
        }
        assert!(sent < Profile::BINANCE_SPOT.weight_per_minute, "futures are tighter than spot");
    }

    #[test]
    fn a_request_heavier_than_the_whole_budget_is_refused_rather_than_looping() {
        // Nothing sends it, and it must not be reported as sendable after a window turnover
        // either — that would be an infinite retry.
        let mut rl = limiter();
        let huge = Profile::BINANCE_SPOT.weight_per_minute + 1;
        assert!(!rl.check(Kind::Request, huge, at(0)).is_send());
        assert!(!rl.check(Kind::Request, huge, at(120_000)).is_send());
    }
}
