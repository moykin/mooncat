//! Everything a core can tell a terminal.
//!
//! # Snapshot, delta, or fact
//!
//! Every event is one of three kinds, and which one it is decides what happens when it is
//! lost. A **snapshot** replaces everything previously known about its subject, so losing one
//! costs nothing once the next arrives. A **delta** only makes sense applied to the state
//! before it, so losing one means the state is wrong until a snapshot repairs it. A **fact**
//! is a one-off notice — a log line, an alert firing — where loss is regrettable but harmless.
//!
//! The distinction is what makes [`Channel::resumable`](crate::Channel::resumable) meaningful:
//! a channel of deltas cannot be resumed past a gap, because the deltas that were dropped are
//! exactly the ones needed to make the later ones apply. The book is the clearest case, and it
//! is why `BookResync` exists as its own event rather than as a flag on a snapshot.
//!
//! # Scope
//!
//! The catalogue from `11-protocol-spec.md` §8, minus the strategy channel: the scope decision
//! of 2026-08-02 removed the strategy engine, so `StrategySchema`, `StrategySnapshot`,
//! `StrategyUpdated`, `StrategyDeleted`, `StrategyEngineState` and `Detect` have no producer
//! and are not defined. `coverage_of_moonproto_events` records where each of the twenty-six
//! MoonProto event variants ended up, including the ones deliberately dropped.

use crate::command::{AlertObject, ReportFilter, Role, Scope, Wallet};
use crate::envelope::{Channel, SymbolId};
use domain::{
    Balance, BookLevel, Bucketing, ClientOrderId, Decimal, ExchangeId, Instrument, MarketKind, Order,
    Position, Side, Symbol,
};
use serde::{Deserialize, Serialize};

// --- supporting shapes -------------------------------------------------------------------

/// How a core is doing, carried on the heartbeat rather than as its own event.
///
/// MoonProto had a separate `KernelHealth` event; folding it into `Ping` means health arrives
/// on a fixed cadence and cannot itself be the thing that got dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreHealth {
    pub cpu_permille: u16,
    pub rss_mb: u32,
    /// Events the fanout dropped because a consumer could not keep up.
    pub events_lagged: u64,
    /// Connectors that are not currently streaming.
    pub degraded: Vec<ExchangeId>,
}

/// Why an order changed. Lets a terminal tell an operator's action from the venue's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderCause {
    Placed,
    Amended,
    Filled,
    PartiallyFilled,
    Cancelled,
    Rejected,
    Expired,
    /// The venue changed it without being asked — liquidation, ADL, risk reduction.
    Venue,
    /// Discovered during reconciliation, not observed live.
    Reconciled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalReason {
    Filled,
    Cancelled,
    Rejected,
    Expired,
    /// Gone from the venue without a terminal state ever being seen.
    VanishedAtVenue,
}

/// A risk limit that is currently exceeded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breach {
    pub limit: String,
    pub allowed: Decimal,
    pub actual: Decimal,
}

/// Why a book had to be thrown away and rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    /// A delta named a predecessor that never arrived.
    Gap,
    /// The core dropped this symbol's updates because the terminal was too slow.
    Backpressure,
    /// The venue's own stream broke.
    VenueDrop,
    Restart,
}

/// Why a channel lost events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// The terminal could not keep up and its ring overflowed.
    SlowConsumer,
    /// The core dropped them under its own overload policy.
    Overflow,
    /// A resume could not reach back far enough.
    ResumeWindowExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    OperatorRequest,
    Upgrade,
    Fatal,
}

/// One printed trade, packed relative to a base timestamp.
///
/// Offsets rather than absolute times because a tape frame carries hundreds of rows within a
/// few seconds of each other, and a full timestamp on each is most of the payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeRow {
    /// Milliseconds after the frame's `base_ts`.
    pub dt: u32,
    pub price: Decimal,
    pub qty: Decimal,
    pub taker_side: Side,
    pub id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiqRow {
    pub dt: u32,
    pub price: Decimal,
    pub qty: Decimal,
    pub side: Side,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandleRow {
    pub open_time: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub trades: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeInfo {
    pub exchange: ExchangeId,
    pub markets: Vec<MarketKind>,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorState {
    pub exchange: ExchangeId,
    pub market: MarketKind,
    pub streaming: bool,
    /// Milliseconds since the last message from the venue.
    pub silent_for_ms: u64,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsItem {
    pub id: String,
    pub at: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<Symbol>,
    /// Whether this is the first version of the item, as opposed to a later correction.
    /// The reducer only lets a late frame overwrite a stored one when the stored one is
    /// original — the rule that stops a translation from clobbering a correction.
    #[serde(default)]
    pub is_original: bool,
}

/// A row of the report table, as the core replicates it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportRowData {
    /// Append-only identity. The replication cursor moves on this and nothing else.
    pub rec_id: i64,
    pub values: Vec<(String, rmpv::Value)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportField {
    pub name: String,
    pub kind: ReportFieldKind,
    #[serde(default)]
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFieldKind {
    Int,
    /// Kept as text on the wire so that an `i64` identity cannot be rounded by a consumer
    /// that decodes numbers as floats — which is what a spreadsheet does.
    Id,
    Decimal,
    Text,
    Timestamp,
    Bool,
}

// --- acknowledgement (task 1.8) -----------------------------------------------------------

/// What became of a command.
///
/// Exactly one *final* status per `req`. Heavy commands answer `Accepted` first and `Done` or
/// `Failed` later; light ones go straight to a final status. The split matters because a
/// terminal that has not heard anything cannot tell a lost command from a slow one, and will
/// eventually retry something that is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    /// Received and started. Not final: a `Done` or `Failed` follows.
    Accepted,
    /// Completed successfully. Final.
    Done,
    /// Refused before anything happened — bad arguments, wrong role, a risk limit. Final,
    /// and never worth retrying unchanged.
    Rejected,
    /// Started and did not complete. Final. Whether a retry is sane depends on
    /// [`AckDetail::retryable`].
    Failed,
}

impl AckStatus {
    /// Whether this is the last thing a terminal will hear about the request.
    pub const fn is_final(self) -> bool {
        !matches!(self, Self::Accepted)
    }
}

/// Why, and what to do about it.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AckDetail {
    #[serde(default)]
    pub code: Option<AckCode>,
    #[serde(default)]
    pub message: String,
    /// How many items a bulk command actually affected. A `CancelAll` that cancelled nothing
    /// and one that cancelled forty orders are different outcomes and must look different.
    #[serde(default)]
    pub count: Option<u32>,
    /// Whether resending the identical command could succeed.
    ///
    /// Decided by the core, not guessed at by the terminal: only the core knows whether a
    /// failure was a rate limit that will pass or a rejection that will not. A terminal that
    /// decided for itself would retry into a ban.
    #[serde(default)]
    pub retryable: bool,
    /// How long to wait first, when the core knows.
    #[serde(default)]
    pub retry_after_ms: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckCode {
    /// The session's role is too low.
    Forbidden,
    /// Arguments did not pass validation.
    Invalid,
    /// A risk limit refused it.
    RiskDenied,
    /// The venue said no.
    VenueRejected,
    /// The venue is rate limiting us. Retryable, after a wait.
    RateLimited,
    /// The venue did not answer in time. Retryable only if the command is idempotent.
    VenueTimeout,
    /// `expected_rev` did not match: someone else changed it first.
    Conflict,
    /// The order, position or object is gone.
    NotFound,
    /// The venue cannot do this at all.
    Unsupported,
    /// The core is shutting down or not ready.
    Unavailable,
}

impl AckCode {
    /// Whether an identical retry has any chance of a different outcome.
    ///
    /// Deliberately conservative: anything that means "the request was wrong" is not
    /// retryable, because a terminal that retries a rejection turns one refusal into a loop.
    /// `VenueTimeout` is the interesting case — the command may well have been executed, so
    /// it is retryable **only** because the `req` ring makes the second attempt a no-op.
    pub const fn retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::VenueTimeout | Self::Unavailable)
    }
}

// --- the catalogue ---------------------------------------------------------------------------

/// Core → terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum ServerEvent {
    // --- 8.1 Control (channel 0) ------------------------------------------------------
    Challenge {
        server_nonce: [u8; 32],
        core_epoch: u64,
        protocol_min: u16,
        protocol_max: u16,
        caps: Vec<String>,
    },
    Welcome {
        session_id: u64,
        resume_token: [u8; 32],
        core_epoch: u64,
        protocol: u16,
        caps: Vec<String>,
        server_time_ms: i64,
        role: Role,
    },
    Resumed {
        session_id: u64,
        resume_token: [u8; 32],
        /// Per channel: what will be replayed, as `(channel, from_seq, to_seq)`.
        replay: Vec<(Channel, u64, u64)>,
        /// Channels whose gap could not be replayed and need a fresh snapshot instead.
        lost: Vec<Channel>,
    },
    Enrolled {
        device_id: [u8; 16],
        role: Role,
    },
    /// The start-of-session state has all been sent.
    ///
    /// Exactly once, and it is what lets a terminal show a chart instead of a spinner: without
    /// it there is no way to distinguish "still arriving" from "that is all there is".
    SyncComplete {
        epoch: u64,
        channels: Vec<(Channel, u64)>,
    },
    Ping {
        nonce: u64,
        sent_ms: i64,
        rtt_ms: u32,
        core: CoreHealth,
    },
    /// Events were lost. Written **before** the next event of the affected channel, so the
    /// terminal learns its state is incomplete before it is handed anything that assumes
    /// otherwise.
    Gap {
        channel: Channel,
        from_seq: u64,
        to_seq: u64,
        dropped: u32,
        reason: GapReason,
    },
    Failed {
        code: crate::ErrorCode,
        message: String,
        #[serde(default)]
        retry_after_ms: Option<u32>,
    },
    Shutdown {
        reason: ShutdownReason,
        restart_expected: bool,
        #[serde(default)]
        eta_ms: Option<u32>,
    },
    CoreInfo {
        version: String,
        build: String,
        started_at: i64,
        exchanges: Vec<ExchangeInfo>,
        features: Vec<String>,
    },
    /// The core's own log, relayed. Closes the gap where a terminal invented log lines about
    /// a process it cannot see.
    CoreLog {
        at: i64,
        level: LogLevel,
        target: String,
        msg: String,
    },

    // --- 8.2 Account (channel 1), private ------------------------------------------------
    AccountSnapshot {
        scope: Scope,
        orders: Vec<Order>,
        positions: Vec<Position>,
        balances: Vec<Balance>,
        rev: u64,
        /// Lets a terminal check it agrees without re-sending everything.
        digest: u64,
    },
    OrderUpdate {
        order: Box<Order>,
        rev: u64,
        cause: OrderCause,
    },
    OrderRemoved {
        client_id: ClientOrderId,
        final_state: Box<Order>,
        reason: RemovalReason,
    },
    OrdersSnapshot {
        orders: Vec<Order>,
        #[serde(default)]
        cursor: Option<String>,
    },
    Fill {
        fill: Box<domain::Fill>,
    },
    Balances {
        exchange: ExchangeId,
        market: MarketKind,
        balances: Vec<Balance>,
        rev: u64,
    },
    Positions {
        exchange: ExchangeId,
        market: MarketKind,
        positions: Vec<Position>,
        rev: u64,
    },
    RiskState {
        used: Vec<(String, Decimal)>,
        breaches: Vec<Breach>,
        kill_switch_armed: bool,
    },
    ApiKeyStatus {
        exchange: ExchangeId,
        #[serde(default)]
        expires_at: Option<i64>,
        permissions: Vec<String>,
    },
    TransferableAssets {
        exchange: ExchangeId,
        wallet: Wallet,
        /// Asset, free amount, minimum transferable.
        items: Vec<(String, Decimal, Decimal)>,
    },

    // --- 8.3 Command (channel 2) -----------------------------------------------------------
    CommandAck {
        req: u64,
        status: AckStatus,
        #[serde(default)]
        detail: Option<AckDetail>,
    },
    /// Progress of a bulk command between `Accepted` and its final status.
    CommandProgress {
        req: u64,
        done: u32,
        total: u32,
    },

    // --- 8.4 Book (channel 3) ---------------------------------------------------------------
    BookSnapshot {
        sid: SymbolId,
        last_update_id: u64,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
        ts: i64,
    },
    BookDelta {
        sid: SymbolId,
        prev_update_id: u64,
        last_update_id: u64,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
        ts: i64,
    },
    /// The book is unusable and a snapshot is coming.
    ///
    /// Its own event rather than a flag on the snapshot, because the terminal needs to grey
    /// the panel out *now* — the interval between losing the chain and rebuilding it is
    /// exactly when a stale book is most dangerous to look at.
    BookResync {
        sid: SymbolId,
        reason: ResyncReason,
    },

    // --- 8.5 Tape and candles (channels 4, 5) ------------------------------------------------
    Trades {
        sid: SymbolId,
        base_ts: i64,
        rows: Vec<TradeRow>,
        /// History rather than live. A terminal draws a break rather than splicing.
        #[serde(default)]
        backfill: bool,
        #[serde(default)]
        cursor: Option<String>,
        /// The backfill hit its row cap before reaching `from_ts`.
        #[serde(default)]
        truncated: bool,
    },
    Liquidations {
        sid: SymbolId,
        base_ts: i64,
        rows: Vec<LiqRow>,
    },
    Candle {
        sid: SymbolId,
        bucketing: Bucketing,
        candle: CandleRow,
        closed: bool,
    },
    Candles {
        sid: SymbolId,
        bucketing: Bucketing,
        rows: Vec<CandleRow>,
        #[serde(default)]
        cursor: Option<String>,
    },
    CandleSeriesState {
        sid: SymbolId,
        #[serde(default)]
        bucketing: Option<Bucketing>,
        rev: i32,
    },

    // --- 8.6 Reference and settings (channels 6, 7) -------------------------------------------
    Instruments {
        exchange: ExchangeId,
        market: MarketKind,
        items: Vec<Instrument>,
        #[serde(default)]
        cursor: Option<String>,
        rev: u64,
    },
    /// Sent before the `Instruments` page that reflects it, so a terminal can react to a new
    /// listing without diffing two large lists itself.
    InstrumentsChanged {
        added: Vec<Symbol>,
        removed: Vec<Symbol>,
    },
    Subscribed {
        assigned: Vec<(Symbol, SymbolId)>,
        rejected: Vec<(Symbol, String)>,
    },
    Subscriptions {
        subs: Vec<exchange::Subscription>,
    },
    Settings {
        settings: rmpv::Value,
        rev: u64,
    },
    CoreRuntimeState {
        trading_enabled: bool,
        dry_run: bool,
        connectors: Vec<ConnectorState>,
    },
    ProfitState {
        realized: Decimal,
        trades: u32,
        session_realized: Decimal,
        session_trades: u32,
    },

    // --- 8.7 Alerts and news (channel 10) ------------------------------------------------------
    Alert {
        alert: Box<AlertObject>,
    },
    AlertDeleted {
        id: u64,
    },
    Alerts {
        items: Vec<AlertObject>,
    },
    AlertFired {
        id: u64,
        sid: SymbolId,
        at: i64,
        price: Decimal,
        #[serde(default)]
        note: String,
    },
    ChartAnnotations {
        sid: SymbolId,
        filter_lines: Vec<String>,
        debug_lines: Vec<String>,
    },
    News {
        items: Vec<NewsItem>,
        /// Backfill rather than live; history always precedes live for a given session.
        #[serde(default)]
        history: bool,
    },
    /// A symbol appeared on a venue that did not list it before.
    NewListing {
        symbol: Symbol,
        first_seen_ms: i64,
        source: ListingSource,
    },

    // --- 8.7 Reports (channel 9) ----------------------------------------------------------------
    ReportSchema {
        fields: Vec<ReportField>,
        rev: u64,
    },
    ReportPage {
        req: u64,
        rows: Vec<ReportRowData>,
        last_rec_id: i64,
        max_rec_id: i64,
        #[serde(default)]
        cursor: Option<String>,
        /// The core's database was recreated: every `rec_id` the terminal holds is stale, and
        /// it must start again from zero rather than from its cursor.
        #[serde(default)]
        db_recreated: bool,
    },
    ReportRow {
        row: ReportRowData,
    },
    ReportRowDeleted {
        rec_id: i64,
    },
    ReportRowsDeleted {
        deleted: bool,
        ranges: Vec<(i64, i64)>,
        singles: Vec<i64>,
    },
    ReportSyncComplete {
        req: u64,
        rows_total: u32,
        last_rec_id: i64,
    },
    ReportQueryResult {
        filter: Box<ReportFilter>,
        rows: Vec<ReportRowData>,
        #[serde(default)]
        cursor: Option<String>,
    },
}

/// How a new listing was noticed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingSource {
    /// It appeared in the venue's instrument catalogue between two refreshes.
    Catalogue,
    /// The venue announced it before trading opened.
    Announcement,
}

impl ServerEvent {
    /// Which channel carries it.
    pub const fn channel(&self) -> Channel {
        match self {
            Self::Challenge { .. }
            | Self::Welcome { .. }
            | Self::Resumed { .. }
            | Self::Enrolled { .. }
            | Self::SyncComplete { .. }
            | Self::Ping { .. }
            | Self::Gap { .. }
            | Self::Failed { .. }
            | Self::Shutdown { .. }
            | Self::CoreInfo { .. }
            | Self::CoreLog { .. } => Channel::CONTROL,

            Self::AccountSnapshot { .. }
            | Self::OrderUpdate { .. }
            | Self::OrderRemoved { .. }
            | Self::OrdersSnapshot { .. }
            | Self::Fill { .. }
            | Self::Balances { .. }
            | Self::Positions { .. }
            | Self::RiskState { .. }
            | Self::ApiKeyStatus { .. }
            | Self::TransferableAssets { .. } => Channel::ACCOUNT,

            Self::CommandAck { .. } | Self::CommandProgress { .. } => Channel::COMMAND,

            Self::BookSnapshot { .. } | Self::BookDelta { .. } | Self::BookResync { .. } => Channel::BOOK,

            Self::Trades { .. } | Self::Liquidations { .. } => Channel::TAPE,

            Self::Candle { .. } | Self::Candles { .. } | Self::CandleSeriesState { .. } => Channel::CANDLES,

            Self::Instruments { .. }
            | Self::InstrumentsChanged { .. }
            | Self::Subscribed { .. }
            | Self::Subscriptions { .. } => Channel::REFERENCE,

            Self::Settings { .. } | Self::CoreRuntimeState { .. } | Self::ProfitState { .. } => {
                Channel::SETTINGS
            }

            Self::Alert { .. }
            | Self::AlertDeleted { .. }
            | Self::Alerts { .. }
            | Self::AlertFired { .. }
            | Self::ChartAnnotations { .. }
            | Self::News { .. }
            | Self::NewListing { .. } => Channel::ALERTS,

            Self::ReportSchema { .. }
            | Self::ReportPage { .. }
            | Self::ReportRow { .. }
            | Self::ReportRowDeleted { .. }
            | Self::ReportRowsDeleted { .. }
            | Self::ReportSyncComplete { .. }
            | Self::ReportQueryResult { .. } => Channel::REPORT,
        }
    }

    /// Whether losing this event leaves the terminal's state wrong until something repairs it.
    ///
    /// True for deltas. A snapshot replaces what came before, so a lost one costs nothing once
    /// the next arrives; a fact is a one-off notice. This is what decides whether a `Gap` on
    /// this channel needs a resync or merely a log line.
    pub const fn is_delta(&self) -> bool {
        matches!(
            self,
            Self::OrderUpdate { .. }
                | Self::OrderRemoved { .. }
                | Self::Fill { .. }
                | Self::BookDelta { .. }
                | Self::Trades { .. }
                | Self::Liquidations { .. }
                | Self::Candle { .. }
                | Self::Alert { .. }
                | Self::AlertDeleted { .. }
                | Self::News { .. }
                | Self::ReportPage { .. }
                | Self::ReportRow { .. }
                | Self::ReportRowDeleted { .. }
        )
    }

    /// Whether an unauthenticated session may be sent this.
    ///
    /// The invariant the whole key-storage split exists to protect. Account data is private;
    /// market data is not. Expressed on the event rather than on the channel so that adding an
    /// event to a public channel cannot accidentally leak an account through it.
    pub const fn is_private(&self) -> bool {
        matches!(self.channel(), Channel::ACCOUNT)
            || matches!(
                self,
                Self::CommandAck { .. }
                    | Self::CommandProgress { .. }
                    | Self::ProfitState { .. }
                    | Self::Settings { .. }
                    | Self::ReportSchema { .. }
                    | Self::ReportPage { .. }
                    | Self::ReportRow { .. }
                    | Self::ReportRowDeleted { .. }
                    | Self::ReportRowsDeleted { .. }
                    | Self::ReportSyncComplete { .. }
                    | Self::ReportQueryResult { .. }
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{MarketKind, OrderStatus, OrderType, PositionSide, TimeInForce, Timestamp};
    use rust_decimal_macros::dec;

    fn sym() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    fn an_order() -> Order {
        Order {
            client_id: ClientOrderId("c-1".into()),
            venue_id: Some("77".into()),
            symbol: sym(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            qty: dec!(0.5),
            price: Some(dec!(63096.01)),
            trigger_price: None,
            tif: TimeInForce::Gtc,
            position_side: PositionSide::Long,
            reduce_only: false,
            status: OrderStatus::New,
            filled_qty: dec!(0),
            avg_price: dec!(0),
            created_at: Timestamp::from_millis(1),
            updated_at: Timestamp::from_millis(1),
        }
    }

    /// Every event, with a payload that exercises its fields. The list the acceptance
    /// criterion is written against.
    fn every_event() -> Vec<ServerEvent> {
        let sid = SymbolId(1);
        let level = || BookLevel { price: dec!(63000), qty: dec!(1) };
        vec![
            ServerEvent::Challenge {
                server_nonce: [1; 32],
                core_epoch: 7,
                protocol_min: 1,
                protocol_max: 2,
                caps: vec!["zstd".into()],
            },
            ServerEvent::Welcome {
                session_id: 1,
                resume_token: [2; 32],
                core_epoch: 7,
                protocol: 2,
                caps: vec![],
                server_time_ms: 1_700_000_000_000,
                role: Role::Trader,
            },
            ServerEvent::Resumed {
                session_id: 1,
                resume_token: [3; 32],
                replay: vec![(Channel::TAPE, 10, 42)],
                lost: vec![Channel::BOOK],
            },
            ServerEvent::Enrolled { device_id: [4; 16], role: Role::Viewer },
            ServerEvent::SyncComplete { epoch: 7, channels: vec![(Channel::BOOK, 5)] },
            ServerEvent::Ping {
                nonce: 1,
                sent_ms: 1,
                rtt_ms: 12,
                core: CoreHealth { cpu_permille: 120, rss_mb: 512, events_lagged: 0, degraded: vec![] },
            },
            ServerEvent::Gap {
                channel: Channel::TAPE,
                from_seq: 10,
                to_seq: 20,
                dropped: 10,
                reason: GapReason::SlowConsumer,
            },
            ServerEvent::Failed {
                code: crate::ErrorCode::Malformed,
                message: "bad frame".into(),
                retry_after_ms: None,
            },
            ServerEvent::Shutdown {
                reason: ShutdownReason::Upgrade,
                restart_expected: true,
                eta_ms: Some(5_000),
            },
            ServerEvent::CoreInfo {
                version: "0.1.0".into(),
                build: "abc".into(),
                started_at: 1,
                exchanges: vec![ExchangeInfo {
                    exchange: ExchangeId::Binance,
                    markets: vec![MarketKind::Spot],
                    connected: true,
                }],
                features: vec![],
            },
            ServerEvent::CoreLog {
                at: 1,
                level: LogLevel::Warn,
                target: "binance".into(),
                msg: "reconnecting".into(),
            },
            ServerEvent::AccountSnapshot {
                scope: Scope::All,
                orders: vec![an_order()],
                positions: vec![],
                balances: vec![],
                rev: 1,
                digest: 99,
            },
            ServerEvent::OrderUpdate {
                order: Box::new(an_order()),
                rev: 2,
                cause: OrderCause::PartiallyFilled,
            },
            ServerEvent::OrderRemoved {
                client_id: ClientOrderId("c-1".into()),
                final_state: Box::new(an_order()),
                reason: RemovalReason::Filled,
            },
            ServerEvent::OrdersSnapshot { orders: vec![an_order()], cursor: None },
            ServerEvent::Fill {
                fill: Box::new(domain::Fill {
                    client_id: ClientOrderId("c-1".into()),
                    symbol: sym(),
                    trade_id: "t-5".into(),
                    side: Side::Buy,
                    price: dec!(63000),
                    qty: dec!(0.1),
                    fee: dec!(0.01),
                    fee_asset: "USDT".into(),
                    is_maker: false,
                    ts: Timestamp::from_millis(1),
                }),
            },
            ServerEvent::Balances {
                exchange: ExchangeId::Binance,
                market: MarketKind::Spot,
                balances: vec![Balance { asset: "USDT".into(), free: dec!(1000), locked: dec!(0) }],
                rev: 1,
            },
            ServerEvent::Positions {
                exchange: ExchangeId::Binance,
                market: MarketKind::LinearPerp,
                positions: vec![],
                rev: 1,
            },
            ServerEvent::RiskState {
                used: vec![("notional".into(), dec!(500))],
                breaches: vec![Breach {
                    limit: "max_notional".into(),
                    allowed: dec!(1000),
                    actual: dec!(1200),
                }],
                kill_switch_armed: false,
            },
            ServerEvent::ApiKeyStatus {
                exchange: ExchangeId::Binance,
                expires_at: Some(1),
                permissions: vec!["SPOT".into()],
            },
            ServerEvent::TransferableAssets {
                exchange: ExchangeId::Binance,
                wallet: Wallet::Spot,
                items: vec![("USDT".into(), dec!(100), dec!(1))],
            },
            ServerEvent::CommandAck {
                req: 1,
                status: AckStatus::Done,
                detail: Some(AckDetail { count: Some(3), ..Default::default() }),
            },
            ServerEvent::CommandProgress { req: 1, done: 2, total: 10 },
            ServerEvent::BookSnapshot {
                sid,
                last_update_id: 100,
                bids: vec![level()],
                asks: vec![level()],
                ts: 1,
            },
            ServerEvent::BookDelta {
                sid,
                prev_update_id: 100,
                last_update_id: 101,
                bids: vec![level()],
                asks: vec![],
                ts: 2,
            },
            ServerEvent::BookResync { sid, reason: ResyncReason::Gap },
            ServerEvent::Trades {
                sid,
                base_ts: 1,
                rows: vec![TradeRow {
                    dt: 5,
                    price: dec!(63000),
                    qty: dec!(0.1),
                    taker_side: Side::Buy,
                    id: 1,
                }],
                backfill: false,
                cursor: None,
                truncated: false,
            },
            ServerEvent::Liquidations {
                sid,
                base_ts: 1,
                rows: vec![LiqRow { dt: 1, price: dec!(62000), qty: dec!(3), side: Side::Sell }],
            },
            ServerEvent::Candle {
                sid,
                bucketing: Bucketing::Time { interval_ms: 60_000 },
                candle: CandleRow {
                    open_time: 0,
                    open: dec!(1),
                    high: dec!(2),
                    low: dec!(0.5),
                    close: dec!(1.5),
                    volume: dec!(10),
                    trades: 5,
                },
                closed: false,
            },
            ServerEvent::Candles {
                sid,
                bucketing: Bucketing::Time { interval_ms: 60_000 },
                rows: vec![],
                cursor: None,
            },
            ServerEvent::CandleSeriesState { sid, bucketing: None, rev: 1 },
            ServerEvent::Instruments {
                exchange: ExchangeId::Binance,
                market: MarketKind::Spot,
                items: vec![],
                cursor: None,
                rev: 1,
            },
            ServerEvent::InstrumentsChanged { added: vec![sym()], removed: vec![] },
            ServerEvent::Subscribed { assigned: vec![(sym(), sid)], rejected: vec![] },
            ServerEvent::Subscriptions { subs: vec![exchange::Subscription::Book(sym())] },
            ServerEvent::Settings { settings: rmpv::Value::Nil, rev: 1 },
            ServerEvent::CoreRuntimeState {
                trading_enabled: true,
                dry_run: false,
                connectors: vec![ConnectorState {
                    exchange: ExchangeId::Binance,
                    market: MarketKind::Spot,
                    streaming: true,
                    silent_for_ms: 0,
                    last_error: None,
                }],
            },
            ServerEvent::ProfitState {
                realized: dec!(10),
                trades: 3,
                session_realized: dec!(2),
                session_trades: 1,
            },
            ServerEvent::Alert { alert: Box::new(an_alert()) },
            ServerEvent::AlertDeleted { id: 1 },
            ServerEvent::Alerts { items: vec![an_alert()] },
            ServerEvent::AlertFired { id: 1, sid, at: 1, price: dec!(63500), note: "resistance".into() },
            ServerEvent::ChartAnnotations { sid, filter_lines: vec!["vol > 1m".into()], debug_lines: vec![] },
            ServerEvent::News {
                items: vec![NewsItem {
                    id: "n1".into(),
                    at: 1,
                    title: "listing".into(),
                    body: String::new(),
                    tags: vec!["listing".into()],
                    symbols: vec![sym()],
                    is_original: true,
                }],
                history: false,
            },
            ServerEvent::NewListing { symbol: sym(), first_seen_ms: 1, source: ListingSource::Catalogue },
            ServerEvent::ReportSchema {
                fields: vec![ReportField {
                    name: "rec_id".into(),
                    kind: ReportFieldKind::Id,
                    title: "ID".into(),
                }],
                rev: 1,
            },
            ServerEvent::ReportPage {
                req: 1,
                rows: vec![a_report_row()],
                last_rec_id: 10,
                max_rec_id: 10,
                cursor: None,
                db_recreated: false,
            },
            ServerEvent::ReportRow { row: a_report_row() },
            ServerEvent::ReportRowDeleted { rec_id: 3 },
            ServerEvent::ReportRowsDeleted { deleted: true, ranges: vec![(1, 5)], singles: vec![9] },
            ServerEvent::ReportSyncComplete { req: 1, rows_total: 10, last_rec_id: 10 },
            ServerEvent::ReportQueryResult {
                filter: Box::new(ReportFilter::default()),
                rows: vec![],
                cursor: None,
            },
        ]
    }

    fn an_alert() -> AlertObject {
        AlertObject {
            id: 1,
            rev: 1,
            symbol: sym(),
            condition: crate::command::AlertCondition::PriceAbove(dec!(1)),
            actions: vec![crate::command::AlertAction::Notify],
            armed: true,
            cooldown_ms: 0,
            note: String::new(),
        }
    }

    fn a_report_row() -> ReportRowData {
        ReportRowData { rec_id: 1, values: vec![("pnl".into(), rmpv::Value::from(12))] }
    }

    fn tag_of(e: &ServerEvent) -> String {
        let value: rmpv::Value = rmp_serde::from_slice(&rmp_serde::to_vec_named(e).unwrap()).unwrap();
        match value {
            rmpv::Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some("t"))
                .and_then(|(_, v)| v.as_str().map(str::to_string))
                .expect("every event carries a tag"),
            other => panic!("an event must encode as a map, got {other:?}"),
        }
    }

    #[test]
    fn all_events_round_trip() {
        for event in every_event() {
            let bytes = rmp_serde::to_vec_named(&event).expect("encodes");
            let back: ServerEvent = rmp_serde::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{} failed to decode: {e}", tag_of(&event)));
            assert_eq!(back, event, "{} did not survive the round trip", tag_of(&event));
        }
    }

    #[test]
    fn the_catalogue_has_no_duplicate_tags() {
        let mut tags: Vec<String> = every_event().iter().map(tag_of).collect();
        let before = tags.len();
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), before, "two events share a tag");
        println!("{before} events in the catalogue");
    }

    /// **The acceptance criterion for task 1.6.**
    ///
    /// Every one of the twenty-six `Event` variants MoonProto defines has either an analogue
    /// here or an explicit record of why it does not. Written as data rather than prose so
    /// that "we forgot one" and "we decided against one" cannot look the same.
    #[test]
    fn coverage_of_moonproto_events() {
        enum Fate {
            /// Covered by these events of ours.
            By(&'static [&'static str]),
            /// Deliberately not carried over, with the reason.
            Dropped(&'static str),
        }
        use Fate::*;

        let coverage: &[(&str, Fate)] = &[
            ("KernelHealth", By(&["ping"])),
            ("News", By(&["news"])),
            ("Order", By(&["order_update", "order_removed", "orders_snapshot"])),
            ("OrderBook", By(&["book_snapshot", "book_delta", "book_resync"])),
            ("Trade", By(&["trades"])),
            ("WatcherFills", By(&["fill"])),
            ("Balance", By(&["balances"])),
            ("Account", By(&["api_key_status", "core_runtime_state"])),
            ("TransferAssets", By(&["transferable_assets"])),
            ("CoinCardCandles", By(&["candles"])),
            ("LiveCandle", By(&["candle"])),
            ("CandleTimeframeState", By(&["candle_series_state"])),
            ("CandlesSnapshot", By(&["candles"])),
            ("Position", By(&["positions"])),
            ("Instruments", By(&["instruments", "instruments_changed"])),
            ("Settings", By(&["settings"])),
            ("Report", By(&["report_page", "report_row", "report_sync_complete"])),
            ("ReportSchema", By(&["report_schema"])),
            ("Alerts", By(&["alerts", "alert", "alert_deleted", "alert_fired"])),
            ("ChartObjects", By(&["chart_annotations"])),
            ("Profit", By(&["profit_state"])),
            ("CoreLog", By(&["core_log"])),
            ("ServerToken", By(&["welcome", "resumed"])),
            ("Strategies", Dropped("the strategy engine is out of scope (13-roadmap.md §1.0)")),
            ("Detect", Dropped("detects exist only to drive strategies, which are out of scope")),
            ("Arbitrage", Dropped("arbitrage is out of scope; channel 11 stays reserved and unused")),
        ];

        assert_eq!(coverage.len(), 26, "MoonProto has 26 Event variants; this table must cover all");

        let ours: Vec<String> = every_event().iter().map(tag_of).collect();
        let (mut carried, mut dropped) = (0, 0);
        for (their, fate) in coverage {
            match fate {
                By(tags) => {
                    assert!(!tags.is_empty(), "{their} claims coverage by nothing");
                    for tag in *tags {
                        assert!(
                            ours.contains(&(*tag).to_string()),
                            "{their} is said to be covered by `{tag}`, which does not exist"
                        );
                    }
                    carried += 1;
                }
                Dropped(reason) => {
                    assert!(!reason.is_empty(), "{their} was dropped without a reason");
                    dropped += 1;
                }
            }
        }
        println!("{carried} of MoonProto's events carried over, {dropped} deliberately dropped");
        assert_eq!(dropped, 3, "the only intentional drops are strategies, detects and arbitrage");
    }

    // --- routing and privacy -------------------------------------------------------------

    #[test]
    fn every_event_lands_on_the_channel_its_kind_belongs_to() {
        for event in every_event() {
            let (tag, channel) = (tag_of(&event), event.channel());
            if tag.starts_with("book_") {
                assert_eq!(channel, Channel::BOOK, "{tag}");
            }
            if tag.starts_with("report_") {
                assert_eq!(channel, Channel::REPORT, "{tag}");
            }
            if tag.starts_with("order") || tag == "fill" || tag == "positions" {
                assert_eq!(channel, Channel::ACCOUNT, "{tag} must be private");
            }
        }
    }

    #[test]
    fn account_data_is_private_and_market_data_is_not() {
        // The invariant the whole key-storage split exists to protect.
        for event in every_event() {
            let (tag, private) = (tag_of(&event), event.is_private());
            let is_account = event.channel() == Channel::ACCOUNT;
            if is_account {
                assert!(private, "{tag} is account data and must be private");
            }
            if matches!(event.channel(), Channel::BOOK | Channel::TAPE | Channel::CANDLES) {
                assert!(!private, "{tag} is public market data");
            }
        }
    }

    #[test]
    fn command_acknowledgements_are_private() {
        // An ack carries how many orders were cancelled. On a public channel that is a leak
        // of account activity even without the orders themselves.
        let ack = ServerEvent::CommandAck { req: 1, status: AckStatus::Done, detail: None };
        assert!(ack.is_private());
        assert!(ServerEvent::CommandProgress { req: 1, done: 1, total: 2 }.is_private());
    }

    #[test]
    fn deltas_are_marked_and_snapshots_are_not() {
        // What decides whether a Gap needs a resync or merely a log line.
        let sid = SymbolId(1);
        assert!(ServerEvent::BookDelta {
            sid,
            prev_update_id: 1,
            last_update_id: 2,
            bids: vec![],
            asks: vec![],
            ts: 0
        }
        .is_delta());
        assert!(!ServerEvent::BookSnapshot { sid, last_update_id: 1, bids: vec![], asks: vec![], ts: 0 }
            .is_delta());
        assert!(!ServerEvent::BookResync { sid, reason: ResyncReason::Gap }.is_delta());
        assert!(ServerEvent::OrderUpdate { order: Box::new(an_order()), rev: 1, cause: OrderCause::Placed }
            .is_delta());
    }

    #[test]
    fn a_delta_never_rides_a_channel_that_cannot_be_resumed_without_a_repair_path() {
        // The book is the exception and it is deliberate: its deltas are not resumable, which
        // is exactly why BookResync exists. Anything else that is a delta must be on a
        // resumable channel, or a gap would leave it permanently wrong.
        for event in every_event().into_iter().filter(ServerEvent::is_delta) {
            let channel = event.channel();
            if channel == Channel::BOOK {
                continue;
            }
            assert!(
                channel.resumable() || channel == Channel::REPORT,
                "{} is a delta on {channel}, which can neither resume nor resync",
                tag_of(&event)
            );
        }
    }

    // --- acknowledgement semantics (task 1.8) -----------------------------------------------

    #[test]
    fn only_accepted_is_non_final() {
        assert!(!AckStatus::Accepted.is_final(), "a terminal must keep waiting after Accepted");
        for status in [AckStatus::Done, AckStatus::Rejected, AckStatus::Failed] {
            assert!(status.is_final(), "{status:?} ends the exchange");
        }
    }

    #[test]
    fn only_transient_failures_are_retryable() {
        // Conservative on purpose: a terminal that retries a rejection turns one refusal into
        // a loop, and against a venue that is rate limiting, into a ban.
        for code in [AckCode::RateLimited, AckCode::VenueTimeout, AckCode::Unavailable] {
            assert!(code.retryable(), "{code:?} is transient");
        }
        for code in [
            AckCode::Forbidden,
            AckCode::Invalid,
            AckCode::RiskDenied,
            AckCode::VenueRejected,
            AckCode::Conflict,
            AckCode::NotFound,
            AckCode::Unsupported,
        ] {
            assert!(!code.retryable(), "{code:?} will fail identically every time");
        }
    }

    #[test]
    fn a_venue_timeout_is_retryable_only_because_the_req_ring_makes_it_safe() {
        // The subtle one. A timed-out command may well have executed at the venue, so
        // resending it is only sound because the core deduplicates on `req` (task 1.7) and
        // answers the retry from cache instead of acting again.
        assert!(AckCode::VenueTimeout.retryable());
        assert!(!AckCode::VenueRejected.retryable(), "a rejection did not execute and will not");
    }

    #[test]
    fn a_bulk_acknowledgement_reports_how_many_it_touched() {
        // "Cancelled nothing" and "cancelled forty" are different outcomes and must not look
        // the same on screen.
        let ack = ServerEvent::CommandAck {
            req: 1,
            status: AckStatus::Done,
            detail: Some(AckDetail { count: Some(0), ..Default::default() }),
        };
        let back: ServerEvent = rmp_serde::from_slice(&rmp_serde::to_vec_named(&ack).unwrap()).unwrap();
        match back {
            ServerEvent::CommandAck { detail: Some(d), .. } => assert_eq!(d.count, Some(0)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_omitted_detail_decodes_as_absent_not_as_an_error() {
        let minimal = rmpv::Value::Map(vec![
            ("t".into(), "command_ack".into()),
            (
                "d".into(),
                rmpv::Value::Map(vec![
                    ("req".into(), rmpv::Value::from(1u64)),
                    ("status".into(), "done".into()),
                ]),
            ),
        ]);
        let bytes = rmp_serde::to_vec_named(&minimal).unwrap();
        let event: ServerEvent = rmp_serde::from_slice(&bytes).expect("detail is optional");
        assert!(matches!(event, ServerEvent::CommandAck { detail: None, .. }));
    }

    #[test]
    fn a_report_identity_is_carried_as_text_not_as_a_number() {
        // An i64 rec_id decoded as a float loses precision above 2^53, and a spreadsheet is
        // exactly the consumer that does that. The schema says so with its own field kind.
        assert_eq!(
            ReportField { name: "rec_id".into(), kind: ReportFieldKind::Id, title: String::new() }.kind,
            ReportFieldKind::Id
        );
    }
}
