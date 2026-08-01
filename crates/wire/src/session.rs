//! Per-connection state, as pure logic.
//!
//! No sockets here on purpose: the rules that matter — who may receive private data, when a
//! connection must be dropped — are exactly the rules that are hard to test through a
//! network, so they are kept where a unit test can reach them.

use crate::{ClientMsg, ErrorCode, ServerMsg, PROTOCOL_VERSION};
use domain::Event;
use exchange::Subscription;

/// The core's shared secret, as exported to a terminal.
#[derive(Clone)]
pub struct Auth {
    token: String,
}

impl std::fmt::Debug for Auth {
    /// Hand-written so a stray `{:?}` cannot put the token in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth").field("token", &"<redacted>").finish()
    }
}

impl Auth {
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into() }
    }

    /// Compare in time independent of how many bytes match.
    ///
    /// A short-circuiting `==` lets a caller recover the token one byte at a time by timing
    /// the reply. The length is allowed to leak; the content is not.
    pub fn verify(&self, candidate: &str) -> bool {
        let (a, b) = (self.token.as_bytes(), candidate.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b) {
            diff |= x ^ y;
        }
        diff == 0
    }
}

/// What the caller should do with a connection after handling a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub reply: ServerMsg,
    /// Close the connection after sending the reply.
    pub close: bool,
}

/// One terminal's connection.
#[derive(Debug, Default)]
pub struct Session {
    authenticated: bool,
    subscriptions: Vec<Subscription>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    pub fn subscriptions(&self) -> &[Subscription] {
        &self.subscriptions
    }

    /// Whether this session may be sent `event`.
    ///
    /// Account data — orders, fills, balances, positions — is the entire reason the core
    /// holds keys and the terminal does not. An unauthenticated socket sees market data or
    /// nothing.
    pub fn may_receive(&self, event: &Event) -> bool {
        self.authenticated || !event.is_private()
    }

    /// Whether this session asked for `event`.
    ///
    /// Privacy is checked first, then subscription: a terminal watching one instrument
    /// should not be paying to decode the tape of forty others. Events that do not belong
    /// to a single instrument — connection state, instrument lists — always pass.
    pub fn wants(&self, event: &Event) -> bool {
        use domain::Payload;

        if !self.may_receive(event) {
            return false;
        }
        let Payload::Market(market) = &event.payload else { return true };
        let Some(symbol) = market.symbol() else { return true };
        self.subscriptions.iter().any(|s| s.symbol() == symbol)
    }

    /// Handle one decoded message.
    pub fn handle(&mut self, msg: ClientMsg, auth: &Auth) -> Reaction {
        match msg {
            ClientMsg::Hello { protocol, token } => {
                if protocol != PROTOCOL_VERSION {
                    return fail(
                        ErrorCode::ProtocolMismatch,
                        format!("core speaks protocol {PROTOCOL_VERSION}, terminal speaks {protocol}"),
                    );
                }
                if !auth.verify(&token) {
                    // Deliberately vague: a message distinguishing "no such token" from
                    // "wrong token" is a probing aid.
                    return fail(ErrorCode::Unauthorized, "rejected".into());
                }
                self.authenticated = true;
                Reaction { reply: ServerMsg::Welcome { protocol: PROTOCOL_VERSION }, close: false }
            }

            _ if !self.authenticated => fail(ErrorCode::NotAuthenticated, "send Hello first".into()),

            ClientMsg::Subscribe(subs) => {
                for sub in subs {
                    if !self.subscriptions.contains(&sub) {
                        self.subscriptions.push(sub);
                    }
                }
                Reaction { reply: ServerMsg::Pong(0), close: false }
            }

            ClientMsg::Unsubscribe(subs) => {
                self.subscriptions.retain(|held| !subs.contains(held));
                Reaction { reply: ServerMsg::Pong(0), close: false }
            }

            ClientMsg::Ping(nonce) => Reaction { reply: ServerMsg::Pong(nonce), close: false },
        }
    }
}

fn fail(code: ErrorCode, message: String) -> Reaction {
    Reaction { reply: ServerMsg::Failed { code, message }, close: code.is_fatal() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{
        AccountEvent, ConnectionEvent, Event, ExchangeId, MarketEvent, MarketKind, OrderBook, Symbol,
        Timestamp,
    };

    fn auth() -> Auth {
        Auth::new("correct-horse")
    }

    fn hello(token: &str) -> ClientMsg {
        ClientMsg::Hello { protocol: PROTOCOL_VERSION, token: token.into() }
    }

    fn book_event() -> Event {
        Event::market(Timestamp::from_millis(1), MarketEvent::BookSnapshot(OrderBook::default()))
    }

    fn balance_event() -> Event {
        Event::account(Timestamp::from_millis(1), AccountEvent::Balances(vec![]))
    }

    #[test]
    fn a_correct_token_authenticates() {
        let mut s = Session::new();
        let out = s.handle(hello("correct-horse"), &auth());

        assert_eq!(out.reply, ServerMsg::Welcome { protocol: PROTOCOL_VERSION });
        assert!(!out.close);
        assert!(s.is_authenticated());
    }

    #[test]
    fn a_wrong_token_is_rejected_and_the_connection_closed() {
        let mut s = Session::new();
        let out = s.handle(hello("wrong"), &auth());

        assert!(matches!(out.reply, ServerMsg::Failed { code: ErrorCode::Unauthorized, .. }));
        assert!(out.close, "a rejected peer must not stay connected to retry");
        assert!(!s.is_authenticated());
    }

    #[test]
    fn the_rejection_message_does_not_help_a_guesser() {
        let mut s = Session::new();
        let ServerMsg::Failed { message, .. } = s.handle(hello("wrong"), &auth()).reply else {
            panic!("expected a failure");
        };
        assert!(!message.contains("correct-horse"));
        assert!(!message.to_lowercase().contains("token"));
    }

    #[test]
    fn a_version_mismatch_is_refused_at_hello() {
        // Better a clear refusal than two peers misparsing each other's frames later.
        let mut s = Session::new();
        let out = s.handle(
            ClientMsg::Hello { protocol: PROTOCOL_VERSION + 1, token: "correct-horse".into() },
            &auth(),
        );

        assert!(matches!(out.reply, ServerMsg::Failed { code: ErrorCode::ProtocolMismatch, .. }));
        assert!(out.close);
        assert!(!s.is_authenticated());
    }

    #[test]
    fn nothing_works_before_hello() {
        let mut s = Session::new();
        let out = s.handle(ClientMsg::Ping(1), &auth());

        assert!(matches!(out.reply, ServerMsg::Failed { code: ErrorCode::NotAuthenticated, .. }));
        assert!(out.close);
        assert!(s.subscriptions().is_empty());
    }

    #[test]
    fn an_unauthenticated_session_never_receives_account_data() {
        // The invariant the whole key-custody split rests on.
        let s = Session::new();
        assert!(s.may_receive(&book_event()));
        assert!(!s.may_receive(&balance_event()));
        assert!(s.may_receive(&Event::connection(Timestamp::from_millis(1), ConnectionEvent::Ready)));
    }

    #[test]
    fn an_authenticated_session_receives_everything() {
        let mut s = Session::new();
        s.handle(hello("correct-horse"), &auth());
        assert!(s.may_receive(&balance_event()));
    }

    #[test]
    fn token_comparison_rejects_prefixes_and_wrong_lengths() {
        let a = auth();
        assert!(a.verify("correct-horse"));
        assert!(!a.verify("correct-hors"));
        assert!(!a.verify("correct-horsee"));
        assert!(!a.verify(""));
        assert!(!a.verify("Correct-horse"));
    }

    #[test]
    fn subscriptions_accumulate_without_duplicates_and_can_be_dropped() {
        let symbol = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT");
        let mut s = Session::new();
        s.handle(hello("correct-horse"), &auth());

        s.handle(ClientMsg::Subscribe(vec![Subscription::Book(symbol.clone())]), &auth());
        s.handle(ClientMsg::Subscribe(vec![Subscription::Book(symbol.clone())]), &auth());
        assert_eq!(s.subscriptions().len(), 1, "re-subscribing must not duplicate");

        s.handle(ClientMsg::Subscribe(vec![Subscription::Trades(symbol.clone())]), &auth());
        assert_eq!(s.subscriptions().len(), 2);

        s.handle(ClientMsg::Unsubscribe(vec![Subscription::Book(symbol.clone())]), &auth());
        assert_eq!(s.subscriptions(), &[Subscription::Trades(symbol)]);
    }

    #[test]
    fn ping_echoes_the_nonce_so_a_terminal_can_match_replies() {
        let mut s = Session::new();
        s.handle(hello("correct-horse"), &auth());
        assert_eq!(s.handle(ClientMsg::Ping(42), &auth()).reply, ServerMsg::Pong(42));
    }

    #[test]
    fn auth_never_renders_its_token() {
        assert!(!format!("{:?}", auth()).contains("correct-horse"));
    }

    #[test]
    fn a_session_only_receives_the_instruments_it_asked_for() {
        let btc = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT");
        let eth = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "ETHUSDT");

        let mut s = Session::new();
        s.handle(hello("correct-horse"), &auth());
        s.handle(ClientMsg::Subscribe(vec![Subscription::Book(btc.clone())]), &auth());

        let snapshot = |symbol: &Symbol| {
            Event::market(
                Timestamp::from_millis(1),
                MarketEvent::BookSnapshot(OrderBook { symbol: Some(symbol.clone()), ..Default::default() }),
            )
        };
        assert!(s.wants(&snapshot(&btc)));
        assert!(!s.wants(&snapshot(&eth)));

        // Connection state is not about one instrument, so it is never filtered out.
        assert!(s.wants(&Event::connection(Timestamp::from_millis(1), ConnectionEvent::Ready)));
    }

    #[test]
    fn the_same_ticker_on_another_market_is_a_different_subscription() {
        // Spot BTCUSDT and perp BTCUSDT must not leak into each other's streams.
        let spot = Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT");
        let perp = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT");

        let mut s = Session::new();
        s.handle(hello("correct-horse"), &auth());
        s.handle(ClientMsg::Subscribe(vec![Subscription::Book(spot.clone())]), &auth());

        let snapshot = |symbol: &Symbol| {
            Event::market(
                Timestamp::from_millis(1),
                MarketEvent::BookSnapshot(OrderBook { symbol: Some(symbol.clone()), ..Default::default() }),
            )
        };
        assert!(s.wants(&snapshot(&spot)));
        assert!(!s.wants(&snapshot(&perp)));
    }

    #[test]
    fn private_events_are_filtered_before_subscriptions_are_even_consulted() {
        // An unauthenticated peer must not learn anything from what it does or does not get.
        let s = Session::new();
        assert!(!s.wants(&balance_event()));
    }
}
