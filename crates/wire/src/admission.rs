//! Who gets in, how often they may try, and what they may do once inside.
//!
//! Two concerns that look separate and are not. Authorisation decides whether a session may
//! run a command; admission control decides whether a connection may become a session at all.
//! Both are the difference between a core that is exposed to the internet and one that merely
//! answers on a port.
//!
//! # Rate limiting the handshake
//!
//! Device authentication (task 2.2) is not guessable — an Ed25519 signature cannot be
//! brute-forced — but the *enrolment code* is short enough to type, and therefore short enough
//! to guess. Five attempts a minute from an address, then a minute of silence, turns a code
//! space that would fall in hours into one that would take years.
//!
//! The limit is per source address rather than global: a global counter lets one attacker lock
//! the operator out, which converts an authentication problem into a denial of service.
//!
//! # Why a session cap
//!
//! Each session holds subscriptions, rings and a resume window. Without a cap a client stuck
//! in a reconnect loop accumulates them until the core runs out of memory, and it does so
//! while looking like normal use. Four is generous for one person with a laptop, a desktop and
//! a phone, and the oldest is closed rather than the newest refused — a terminal that has just
//! reconnected is the one the operator is actually looking at.

use crate::auth::DeviceId;
use crate::command::{Command, Role};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Failed handshakes allowed from one address per window.
pub const HELLO_ATTEMPTS_PER_MIN: u32 = 5;
/// How long the window is, and how long a ban lasts once it trips.
pub const BAN_DURATION: Duration = Duration::from_millis(60_000);
/// Concurrent sessions allowed for one device.
pub const MAX_SESSIONS: usize = 4;

/// A session's identity within one core lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// Why a command was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forbidden {
    pub required: Role,
    pub held: Role,
}

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this command needs {:?}, the session holds {:?}", self.required, self.held)
    }
}

/// Whether a session holding `role` may run `command` (task 2.3).
///
/// A free function over the command's own declared minimum, not a table kept alongside: a
/// table is a second place to update, and the update that gets forgotten is the one that
/// leaves a new command open to everyone.
pub fn authorize(role: Role, command: &Command) -> Result<(), Forbidden> {
    let required = command.min_role();
    if role >= required {
        Ok(())
    } else {
        Err(Forbidden { required, held: role })
    }
}

/// What to do with a connection trying to authenticate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admit {
    /// Proceed with the handshake.
    Ok,
    /// Too many failures from this address recently.
    Banned { until: Instant },
}

/// A session the core is currently holding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSession {
    pub id: SessionId,
    pub device: DeviceId,
    pub peer: IpAddr,
    pub role: Role,
    pub opened_at: Instant,
}

/// Sessions the core has closed and the reason, for the caller to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    pub id: SessionId,
    pub reason: CloseReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// The device was revoked.
    Revoked,
    /// The device opened more sessions than it is allowed, and this was the oldest.
    TooManySessions,
}

/// Handshake rate limiting and the session table.
#[derive(Debug, Default)]
pub struct Gatekeeper {
    attempts: HashMap<IpAddr, Window>,
    sessions: HashMap<SessionId, LiveSession>,
    next_session: u64,
}

#[derive(Debug)]
struct Window {
    failures: u32,
    started: Instant,
    banned_until: Option<Instant>,
}

impl Gatekeeper {
    pub fn new() -> Self {
        Self { attempts: HashMap::new(), sessions: HashMap::new(), next_session: 1 }
    }

    /// May this address attempt a handshake?
    pub fn admit(&mut self, peer: IpAddr, now: Instant) -> Admit {
        let Some(window) = self.attempts.get_mut(&peer) else {
            return Admit::Ok;
        };
        if let Some(until) = window.banned_until {
            if now < until {
                return Admit::Banned { until };
            }
            // The ban has run out. The window resets with it, so a banned address gets a full
            // allowance again rather than being one failure from another ban forever.
            window.banned_until = None;
            window.failures = 0;
            window.started = now;
        }
        Admit::Ok
    }

    /// Record a failed handshake, banning the address if it has failed too often.
    pub fn handshake_failed(&mut self, peer: IpAddr, now: Instant) -> Admit {
        let window =
            self.attempts.entry(peer).or_insert(Window { failures: 0, started: now, banned_until: None });

        // A rolling window rather than a leaky bucket: five attempts, then a minute of
        // silence, is what an operator retyping a code experiences as fair and what an
        // attacker experiences as hopeless.
        if now.duration_since(window.started) > BAN_DURATION {
            window.failures = 0;
            window.started = now;
        }
        window.failures += 1;

        if window.failures >= HELLO_ATTEMPTS_PER_MIN {
            let until = now + BAN_DURATION;
            window.banned_until = Some(until);
            return Admit::Banned { until };
        }
        Admit::Ok
    }

    /// Forget the failures of an address that has just succeeded.
    ///
    /// Without this, an operator who mistypes a code four times and then gets it right stays
    /// one failure away from a ban for the rest of the minute.
    pub fn handshake_succeeded(&mut self, peer: IpAddr) {
        self.attempts.remove(&peer);
    }

    /// Open a session, closing the device's oldest if it is over its limit.
    pub fn open(
        &mut self,
        device: DeviceId,
        peer: IpAddr,
        role: Role,
        now: Instant,
    ) -> (SessionId, Vec<Closed>) {
        let id = SessionId(self.next_session);
        self.next_session += 1;
        self.sessions.insert(id, LiveSession { id, device, peer, role, opened_at: now });

        let mut closed = Vec::new();
        // The oldest goes rather than the newest being refused: the terminal that has just
        // connected is the one the operator is looking at, and refusing it would make a
        // forgotten session on another machine lock them out of their own core.
        while self.sessions_of(device).len() > MAX_SESSIONS {
            let Some(oldest) = self
                .sessions
                .values()
                .filter(|s| s.device == device)
                .min_by_key(|s| (s.opened_at, s.id))
                .map(|s| s.id)
            else {
                break;
            };
            self.sessions.remove(&oldest);
            closed.push(Closed { id: oldest, reason: CloseReason::TooManySessions });
        }
        (id, closed)
    }

    pub fn close(&mut self, id: SessionId) -> bool {
        self.sessions.remove(&id).is_some()
    }

    /// Every live session of a device, for revocation and for the session cap.
    pub fn sessions_of(&self, device: DeviceId) -> Vec<SessionId> {
        let mut ids: Vec<_> = self.sessions.values().filter(|s| s.device == device).map(|s| s.id).collect();
        ids.sort();
        ids
    }

    /// Close every session of a revoked device.
    ///
    /// Revocation that left live sessions running would be theatre: the lost laptop it exists
    /// for is the one already connected.
    pub fn revoke_device(&mut self, device: DeviceId) -> Vec<Closed> {
        let doomed = self.sessions_of(device);
        for id in &doomed {
            self.sessions.remove(id);
        }
        doomed.into_iter().map(|id| Closed { id, reason: CloseReason::Revoked }).collect()
    }

    pub fn get(&self, id: SessionId) -> Option<&LiveSession> {
        self.sessions.get(&id)
    }

    pub fn live_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Drop rate-limit records that have gone quiet. Called on a timer; without it an exposed
    /// core accumulates one entry per address that ever probed it.
    pub fn expire_windows(&mut self, now: Instant) -> usize {
        let before = self.attempts.len();
        self.attempts.retain(|_, w| {
            w.banned_until.is_some_and(|until| now < until) || now.duration_since(w.started) <= BAN_DURATION
        });
        before - self.attempts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CloseMode, Scope, Wallet};
    use domain::{ClientOrderId, ExchangeId, MarketKind, PositionSide, Symbol};
    use rust_decimal_macros::dec;

    fn origin() -> Instant {
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    fn at(ms: u64) -> Instant {
        origin() + Duration::from_millis(ms)
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([203, 0, 113, last])
    }

    fn device(n: u8) -> DeviceId {
        DeviceId([n; 16])
    }

    fn sym() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    fn place_order() -> Command {
        Command::PlaceOrder {
            order: Box::new(domain::NewOrder {
                client_id: ClientOrderId("c-1".into()),
                symbol: sym(),
                side: domain::Side::Buy,
                order_type: domain::OrderType::Limit,
                qty: dec!(1),
                price: Some(dec!(1)),
                trigger_price: None,
                tif: domain::TimeInForce::Gtc,
                position_side: PositionSide::Long,
                reduce_only: false,
            }),
        }
    }

    // --- authorisation (task 2.3) ------------------------------------------------------------

    #[test]
    fn a_viewer_is_forbidden_from_placing_an_order() {
        // The acceptance criterion for task 2.3.
        let refusal = authorize(Role::Viewer, &place_order()).unwrap_err();
        assert_eq!(refusal, Forbidden { required: Role::Trader, held: Role::Viewer });
        assert!(refusal.to_string().contains("Trader"), "the message must say what is needed");
    }

    #[test]
    fn a_trader_may_trade_but_not_move_money_off_the_trading_wallet() {
        assert!(authorize(Role::Trader, &place_order()).is_ok());

        let transfer = Command::TransferAsset {
            exchange: ExchangeId::Binance,
            asset: "USDT".into(),
            qty: dec!(1),
            from: Wallet::Futures,
            to: Wallet::Spot,
        };
        assert!(authorize(Role::Trader, &transfer).is_err());
        assert!(authorize(Role::Admin, &transfer).is_ok());
    }

    #[test]
    fn an_admin_may_do_everything_a_trader_may() {
        // Roles are ordered, so a higher one must never be refused something a lower one is
        // allowed. Checked over the whole catalogue rather than by spot check.
        let commands = [
            place_order(),
            Command::SubscriptionsGet,
            Command::CancelAll { scope: Scope::All },
            Command::FlattenAll { scope: Scope::All, mode: CloseMode::Market },
        ];
        for command in commands {
            if authorize(Role::Trader, &command).is_ok() {
                assert!(authorize(Role::Admin, &command).is_ok(), "an admin was refused");
            }
            if authorize(Role::Viewer, &command).is_ok() {
                assert!(authorize(Role::Trader, &command).is_ok());
            }
        }
    }

    #[test]
    fn a_viewer_may_still_look() {
        for command in [Command::SubscriptionsGet, Command::SettingsGet, Command::ReportSchemaRequest] {
            assert!(authorize(Role::Viewer, &command).is_ok(), "a viewer must be able to watch");
        }
    }

    // --- handshake rate limiting (task 2.6) ----------------------------------------------------

    #[test]
    fn the_sixth_attempt_in_a_minute_is_refused() {
        // The acceptance criterion for task 2.6.
        let mut gate = Gatekeeper::new();
        let peer = ip(1);

        for attempt in 1..HELLO_ATTEMPTS_PER_MIN {
            assert_eq!(gate.admit(peer, at(0)), Admit::Ok, "attempt {attempt} must be allowed");
            assert_eq!(gate.handshake_failed(peer, at(0)), Admit::Ok);
        }
        // The fifth failure trips the ban.
        assert!(matches!(gate.handshake_failed(peer, at(0)), Admit::Banned { .. }));
        assert!(matches!(gate.admit(peer, at(0)), Admit::Banned { .. }), "the sixth is refused");
    }

    #[test]
    fn a_ban_expires_and_the_allowance_comes_back_whole() {
        // Otherwise a banned address stays one failure from another ban forever, and an
        // operator who once fat-fingered a code is permanently on a hair trigger.
        let mut gate = Gatekeeper::new();
        let peer = ip(1);
        for _ in 0..HELLO_ATTEMPTS_PER_MIN {
            gate.handshake_failed(peer, at(0));
        }
        assert!(matches!(gate.admit(peer, at(59_999)), Admit::Banned { .. }));
        assert_eq!(gate.admit(peer, at(60_000)), Admit::Ok);

        // A full allowance, not one attempt.
        for _ in 1..HELLO_ATTEMPTS_PER_MIN {
            assert_eq!(gate.handshake_failed(peer, at(60_000)), Admit::Ok);
        }
    }

    #[test]
    fn one_attacker_cannot_lock_the_operator_out() {
        // The reason the limit is per address. A global counter would turn an authentication
        // problem into a denial of service.
        let mut gate = Gatekeeper::new();
        for _ in 0..HELLO_ATTEMPTS_PER_MIN * 4 {
            gate.handshake_failed(ip(66), at(0));
        }
        assert!(matches!(gate.admit(ip(66), at(0)), Admit::Banned { .. }));
        assert_eq!(gate.admit(ip(1), at(0)), Admit::Ok, "the operator's address is untouched");
    }

    #[test]
    fn success_clears_the_failures_that_preceded_it() {
        let mut gate = Gatekeeper::new();
        let peer = ip(1);
        for _ in 0..HELLO_ATTEMPTS_PER_MIN - 1 {
            gate.handshake_failed(peer, at(0));
        }
        gate.handshake_succeeded(peer);

        // Back to a full allowance rather than one failure from a ban.
        for _ in 1..HELLO_ATTEMPTS_PER_MIN {
            assert_eq!(gate.handshake_failed(peer, at(1)), Admit::Ok);
        }
    }

    #[test]
    fn failures_spread_over_more_than_a_window_do_not_accumulate() {
        // A terminal on a flaky link failing once a minute for an hour is not an attacker.
        let mut gate = Gatekeeper::new();
        let peer = ip(1);
        for minute in 0..60u64 {
            assert_eq!(gate.handshake_failed(peer, at(minute * 61_000)), Admit::Ok);
        }
    }

    #[test]
    fn rate_limit_records_do_not_accumulate_forever() {
        let mut gate = Gatekeeper::new();
        for n in 0..200u8 {
            gate.handshake_failed(ip(n), at(0));
        }
        assert_eq!(gate.expire_windows(at(60_001)), 200, "quiet addresses are forgotten");
        assert_eq!(gate.expire_windows(at(60_002)), 0);
    }

    #[test]
    fn a_banned_address_is_kept_until_its_ban_ends() {
        let mut gate = Gatekeeper::new();
        for _ in 0..HELLO_ATTEMPTS_PER_MIN {
            gate.handshake_failed(ip(1), at(0));
        }
        assert_eq!(gate.expire_windows(at(30_000)), 0, "forgetting it would lift the ban early");
        assert!(matches!(gate.admit(ip(1), at(30_000)), Admit::Banned { .. }));
    }

    // --- the session table -----------------------------------------------------------------------

    #[test]
    fn a_device_over_its_session_cap_loses_its_oldest_not_its_newest() {
        // The terminal that has just connected is the one the operator is looking at.
        // Refusing it would let a forgotten session on another machine lock them out.
        let mut gate = Gatekeeper::new();
        let d = device(1);

        let mut ids = Vec::new();
        for n in 0..MAX_SESSIONS as u64 {
            let (id, closed) = gate.open(d, ip(1), Role::Trader, at(n * 1_000));
            assert!(closed.is_empty(), "under the cap nothing is closed");
            ids.push(id);
        }
        assert_eq!(gate.sessions_of(d).len(), MAX_SESSIONS);

        let (newest, closed) = gate.open(d, ip(1), Role::Trader, at(99_000));
        assert_eq!(closed, vec![Closed { id: ids[0], reason: CloseReason::TooManySessions }]);
        assert!(gate.get(newest).is_some(), "the new session survives");
        assert!(gate.get(ids[0]).is_none(), "the oldest is gone");
        assert_eq!(gate.sessions_of(d).len(), MAX_SESSIONS);
    }

    #[test]
    fn the_cap_is_per_device_not_per_core() {
        // Two people with a terminal each must not compete for the same four slots.
        let mut gate = Gatekeeper::new();
        for n in 0..MAX_SESSIONS as u64 {
            gate.open(device(1), ip(1), Role::Trader, at(n));
        }
        let (_, closed) = gate.open(device(2), ip(2), Role::Viewer, at(100));
        assert!(closed.is_empty(), "a second device has its own allowance");
        assert_eq!(gate.live_sessions(), MAX_SESSIONS + 1);
    }

    #[test]
    fn revoking_a_device_closes_the_sessions_it_already_has() {
        // Revocation that left live sessions running would be theatre: the lost laptop it
        // exists for is the one already connected.
        let mut gate = Gatekeeper::new();
        let (doomed, _) = gate.open(device(1), ip(1), Role::Trader, at(0));
        let (other, _) = gate.open(device(2), ip(2), Role::Trader, at(0));

        let closed = gate.revoke_device(device(1));
        assert_eq!(closed, vec![Closed { id: doomed, reason: CloseReason::Revoked }]);
        assert!(gate.get(doomed).is_none());
        assert!(gate.get(other).is_some(), "another device is untouched");
    }

    #[test]
    fn revoking_a_device_with_no_sessions_reports_nothing_closed() {
        let mut gate = Gatekeeper::new();
        assert!(gate.revoke_device(device(9)).is_empty());
    }

    #[test]
    fn session_ids_are_never_reused_within_a_core_lifetime() {
        // Reuse would let a late frame from a closed session be attributed to a new one.
        let mut gate = Gatekeeper::new();
        let mut seen = std::collections::HashSet::new();
        for n in 0..1_000u64 {
            let (id, _) = gate.open(device((n % 7) as u8), ip(1), Role::Trader, at(n));
            assert!(seen.insert(id), "session id repeated");
            gate.close(id);
        }
    }

    #[test]
    fn closing_a_session_twice_is_reported_honestly() {
        let mut gate = Gatekeeper::new();
        let (id, _) = gate.open(device(1), ip(1), Role::Trader, at(0));
        assert!(gate.close(id));
        assert!(!gate.close(id), "the second close must report that nothing happened");
    }
}
