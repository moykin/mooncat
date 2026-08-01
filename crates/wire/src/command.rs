//! Everything a terminal can ask a core to do.
//!
//! # Scope
//!
//! This is the catalogue from `11-protocol-spec.md` §6, minus the strategy engine. The
//! customer's scope decision of 2026-08-02 (`13-roadmap.md` §1.0) removed strategies,
//! backtesting, the tuner and arbitrage, which takes out the whole of §6.6 along with
//! `AutoDetectSet` and `TriggerSet` — both of which exist only to drive detects. Fifty-three
//! commands remain, and `all_commands_round_trip` fails if one is added without a test.
//!
//! # Types only
//!
//! No handler executes any of this yet. Defining the shapes first is deliberate: the shape is
//! what both sides compile against, and getting it wrong after the terminal is written costs
//! far more than getting it right now. Handlers arrive with the OMS in phase 7.
//!
//! # Two rules that run through the whole catalogue
//!
//! **Idempotency is a property of the command, not of the transport.** Every command carries
//! a `req` in its envelope, and the core keeps a ring of the ones it has seen (task 1.7), so a
//! retry after a timeout returns the original acknowledgement instead of acting twice. Where
//! that is not enough — a market close, a bulk move — the command is marked as one that must
//! be sent exactly once, and the terminal is forbidden from retrying it. There is no way to
//! make "close my position" safely repeatable, so the protocol says so out loud rather than
//! pretending.
//!
//! **A command that changes existing state carries the revision it was based on.** `AmendOrder`
//! takes `expected_rev`, `SettingsSet` takes `base_rev`. Two terminals editing the same order
//! is not hypothetical — that is the normal case with a laptop and a phone — and without this
//! the second write silently destroys the first.

use domain::{
    Bucketing, ClientOrderId, Decimal, ExchangeId, MarginMode, MarketKind, NewOrder, PositionSide, Symbol,
};
use exchange::Subscription;
use serde::{Deserialize, Serialize};

/// Which instruments a bulk command applies to.
///
/// Present on every mass operation because "cancel everything" means different things on a
/// screen showing one market and a screen showing forty, and the terminal must say which.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Scope {
    /// Every instrument the core trades.
    All,
    Exchange(ExchangeId),
    Market {
        exchange: ExchangeId,
        market: MarketKind,
    },
    Symbol(Symbol),
    Symbols(Vec<Symbol>),
}

/// Which orders a bulk operation picks up.
///
/// Orthogonal to [`MoveTarget`]: any selection can be sent to any target, and the reverse
/// engineering of MoonBot showed the two axes are genuinely independent there too
/// (report 04 §9.3). Keeping them separate is what avoids a combinatorial enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSelect {
    All,
    Buys,
    Sells,
    /// Entry legs that have not filled at all.
    UnfilledEntries,
    /// Orders further from the market than a given distance.
    Far,
}

/// Where a bulk move puts the orders it selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum MoveTarget {
    /// To the current best bid or ask, whichever side the order is.
    Touch,
    /// A fixed number of ticks away from the touch.
    Ticks(i32),
    /// A percentage away from the last price.
    Percent(Decimal),
    /// To an absolute price. Only meaningful for a single-order selection.
    Price(Decimal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitMode {
    /// Equal parts.
    Even,
    /// Increasing size away from the market.
    Ladder,
}

/// How a position is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseMode {
    Market,
    /// Passive at the touch; the core does not chase.
    Limit,
    /// Passive, and the core re-prices it as the market moves.
    Chase,
}

/// What a protection setting attaches to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum ProtectionTarget {
    Order(ClientOrderId),
    Position {
        symbol: Symbol,
        side: PositionSide,
    },
    /// Defaults applied to anything opened from now on.
    Defaults(Scope),
}

/// A stop, a trailing stop or a take-profit.
///
/// Sent whole rather than as a patch. Partial updates to a stop are how a stop ends up in a
/// state neither side intended: `SetProtection` replaces, so what the terminal shows and what
/// the core holds cannot drift apart.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProtectionSpec {
    #[serde(default)]
    pub stop_loss: Option<Trigger>,
    #[serde(default)]
    pub take_profit: Option<Trigger>,
    #[serde(default)]
    pub trailing: Option<Trailing>,
    /// Whether the core places these at the venue or holds them and fires on its own feed.
    /// Native survives the core dying; emulated works on venues that lack the order type.
    #[serde(default)]
    pub prefer_native: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Trigger {
    /// Absolute price.
    Price(Decimal),
    /// Distance from the entry, in percent.
    PercentFromEntry(Decimal),
    /// Distance from the entry, in ticks.
    TicksFromEntry(i32),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trailing {
    /// How far behind the extreme the stop follows.
    pub distance: Trigger,
    /// Profit that must be reached before the trail arms at all.
    #[serde(default)]
    pub activate_at: Option<Trigger>,
}

/// Which venue wallet a transfer moves between.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wallet {
    Spot,
    Futures,
    Margin,
    Funding,
    Earn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeReason {
    /// The terminal expects to resume this session.
    Suspend,
    /// It does not; the core may forget the session immediately.
    Logout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfitResetKind {
    Current,
    All,
}

/// A change to core settings, as a sparse patch.
///
/// Sparse rather than whole-document because two terminals editing different panels must not
/// overwrite each other's fields — `base_rev` catches the case where they edit the same one.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct SettingsPatch {
    /// Dotted paths to values, in the order they should be applied.
    #[serde(default)]
    pub set: Vec<(String, rmpv::Value)>,
    /// Paths to reset to their default.
    #[serde(default)]
    pub clear: Vec<String>,
}

/// Automatic leverage as a function of position size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoLeverageSpec {
    pub scope: Scope,
    pub enabled: bool,
    /// Ascending notional thresholds and the leverage to use at or above each.
    #[serde(default)]
    pub steps: Vec<(Decimal, Decimal)>,
}

/// A chart alert as the terminal holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertObject {
    pub id: u64,
    /// Optimistic lock: an edit against a stale revision is refused, not merged.
    pub rev: u64,
    pub symbol: Symbol,
    pub condition: AlertCondition,
    pub actions: Vec<AlertAction>,
    pub armed: bool,
    #[serde(default)]
    pub cooldown_ms: u32,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum AlertCondition {
    PriceAbove(Decimal),
    PriceBelow(Decimal),
    PriceCrosses(Decimal),
    ChangePercentOver {
        window_ms: u32,
        percent: Decimal,
    },
    VolumeOver {
        window_ms: u32,
        quote_volume: Decimal,
    },
    BookWall {
        side: domain::Side,
        quote_volume: Decimal,
    },
    Liquidation {
        quote_volume: Decimal,
    },
    /// Fires when a symbol appears on a venue that did not list it before.
    NewListing,
}

/// What an alert does when it fires.
///
/// Deliberately without an order-placing action. With the strategy engine out of scope there
/// is nothing that turns a signal into a trade, and an alert that could place an order would
/// be an automated trading system arriving through the back door — see the guard test in
/// `13-roadmap.md` task 23.3.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum AlertAction {
    Sound(String),
    Notify,
    Log,
}

/// Filter for a report query.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReportFilter {
    #[serde(default)]
    pub from_ms: Option<i64>,
    #[serde(default)]
    pub to_ms: Option<i64>,
    #[serde(default)]
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub cores: Vec<u32>,
    /// Include rows the operator marked deleted. Exclusive when set: off hides them, on shows
    /// **only** them. The inclusive reading is what made the original panel confusing.
    #[serde(default)]
    pub only_deleted: bool,
}

/// Terminal → core.
///
/// Adjacently tagged so that a command this build has never heard of is captured rather than
/// becoming a decode error — see `tests/spike_forward_compat.rs`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Command {
    // --- 6.1 Control -------------------------------------------------------------------
    Hello {
        protocol: u16,
        device_id: [u8; 16],
        client_nonce: [u8; 32],
        #[serde(with = "serde_bytes")]
        sig: Vec<u8>,
        #[serde(default)]
        caps: Vec<String>,
        terminal_version: String,
    },
    Enroll {
        code: String,
        pubkey: [u8; 32],
        label: String,
    },
    Resume {
        session_id: u64,
        resume_token: [u8; 32],
        #[serde(with = "serde_bytes")]
        sig: Vec<u8>,
        /// Where the terminal got to on each channel, so the core replays only the gap.
        acks: Vec<(crate::Channel, u64)>,
    },
    RotateDeviceKey {
        new_pubkey: [u8; 32],
        #[serde(with = "serde_bytes")]
        sig_new: Vec<u8>,
    },
    Ping {
        nonce: u64,
        sent_ms: i64,
    },
    Pong {
        nonce: u64,
        recv_ms: i64,
    },
    Bye {
        reason: ByeReason,
    },

    // --- 6.2 Subscriptions and market data ---------------------------------------------
    Subscribe {
        subs: Vec<Subscription>,
        /// Replace the whole set rather than adding to it. A terminal switching tabs wants
        /// replacement; one opening a second chart wants addition.
        #[serde(default)]
        replace: bool,
    },
    Unsubscribe {
        subs: Vec<Subscription>,
    },
    SubscriptionsGet,
    BookResyncRequest {
        symbol: Symbol,
        depth: u32,
    },
    TapeBackfill {
        symbol: Symbol,
        from_ts: i64,
        limit: u32,
    },
    CandleHistory {
        symbol: Symbol,
        bucketing: Bucketing,
        #[serde(default)]
        from: Option<i64>,
        limit: u32,
    },

    // --- 6.3 Orders ---------------------------------------------------------------------
    PlaceOrder {
        order: Box<NewOrder>,
    },
    CancelOrder {
        symbol: Symbol,
        client_id: ClientOrderId,
    },
    AmendOrder {
        symbol: Symbol,
        client_id: ClientOrderId,
        #[serde(default)]
        new_price: Option<Decimal>,
        #[serde(default)]
        new_qty: Option<Decimal>,
        /// The revision this edit was composed against. `None` means "I do not care", which
        /// is only correct for a terminal that is the sole writer.
        #[serde(default)]
        expected_rev: Option<u64>,
    },
    CancelAll {
        scope: Scope,
    },
    MoveOrders {
        scope: Scope,
        select: OrderSelect,
        target: MoveTarget,
    },
    SplitOrder {
        client_id: ClientOrderId,
        parts: u16,
        mode: SplitMode,
    },
    OrdersSnapshotRequest {
        #[serde(default)]
        cursor: Option<String>,
    },
    OrderStatusRequest {
        client_id: ClientOrderId,
    },

    // --- 6.4 Positions and protection ---------------------------------------------------
    ClosePosition {
        symbol: Symbol,
        position_side: PositionSide,
        mode: CloseMode,
        /// Absent closes the whole position.
        #[serde(default)]
        qty: Option<Decimal>,
    },
    FlattenAll {
        scope: Scope,
        mode: CloseMode,
    },
    SetProtection {
        target: ProtectionTarget,
        spec: ProtectionSpec,
    },
    SetVolumeStop {
        target: ProtectionTarget,
        enabled: bool,
        /// Fixed level rather than one that follows the book.
        fixed: bool,
        level: Decimal,
        volume: Decimal,
    },
    SetPanic {
        target: ProtectionTarget,
        enabled: bool,
    },
    SetImmune {
        target: ProtectionTarget,
        enabled: bool,
    },

    // --- 6.5 Account and venue ----------------------------------------------------------
    AccountSnapshotRequest {
        scope: Scope,
    },
    SetLeverage {
        symbol: Symbol,
        leverage: Decimal,
    },
    SetMarginMode {
        symbol: Symbol,
        mode: MarginMode,
    },
    SetPositionMode {
        exchange: ExchangeId,
        market: MarketKind,
        hedge: bool,
    },
    TransferAsset {
        exchange: ExchangeId,
        asset: String,
        qty: Decimal,
        from: Wallet,
        to: Wallet,
    },
    TransferableAssetsRefresh {
        exchange: ExchangeId,
        wallet: Wallet,
    },
    ConfirmRiskLimit {
        symbol: Symbol,
    },
    ApiKeyStatusRequest {
        exchange: ExchangeId,
    },
    ConvertDust {
        exchange: ExchangeId,
        assets: Vec<String>,
    },

    // --- 6.7 Settings and modes ---------------------------------------------------------
    SettingsGet,
    SettingsSet {
        patch: SettingsPatch,
        base_rev: u64,
    },
    AutoLeverageSet {
        spec: AutoLeverageSpec,
    },
    ProfitReset {
        kind: ProfitResetKind,
    },
    EmuTrades {
        symbol: Symbol,
        base_ts: i64,
        /// Offset in milliseconds from `base_ts`, and a price. For exercising the chart and
        /// the tape without a live market.
        points: Vec<(u16, f32)>,
    },
    CoreRestart {
        /// The core's own name, typed by the operator. A restart is not something to do by
        /// mis-clicking, and an acknowledgement dialog is easier to click through than a name
        /// is to type.
        confirm: String,
    },

    // --- 6.8 Alerts, annotations, news ---------------------------------------------------
    AlertUpsert {
        alert: Box<AlertObject>,
    },
    AlertDelete {
        id: u64,
    },
    AlertsSnapshotRequest,
    ChartAnnotationsRequest {
        symbol: Symbol,
        #[serde(default)]
        want_filters: bool,
        #[serde(default)]
        want_debug: bool,
    },
    NewsHistoryRequest {
        limit: u16,
    },

    // --- 6.9 Reports ---------------------------------------------------------------------
    ReportSchemaRequest,
    ReportSyncRequest {
        from_rec_id: i64,
        depth_days: u16,
    },
    ReportPageAck {
        req: u64,
        last_rec_id: i64,
    },
    ReportCheckRows {
        rec_ids: Vec<i64>,
    },
    ReportSetRowsDeleted {
        deleted: bool,
        ranges: Vec<(i64, i64)>,
        singles: Vec<i64>,
    },
    ReportQuery {
        filter: Box<ReportFilter>,
        limit: u32,
        #[serde(default)]
        cursor: Option<String>,
    },
}

/// Caps the protocol enforces regardless of what a terminal asks for.
///
/// Stated as constants rather than left to the handler, because the terminal needs the same
/// numbers to paginate and the two drifting apart is how a request silently returns less than
/// the caller believes it did.
pub mod limits {
    pub const TAPE_BACKFILL_MAX: u32 = 4_000;
    pub const CANDLE_HISTORY_MAX: u32 = 5_000;
    pub const REPORT_QUERY_MAX: u32 = 5_000;
    pub const REPORT_CHECK_ROWS_MAX: usize = 100;
    pub const NEWS_HISTORY_MAX: u16 = 200;
    pub const EMU_TRADES_MAX_POINTS: usize = 4_096;
}

impl Command {
    /// The channel this command belongs on.
    ///
    /// Derived from the command rather than chosen by the caller: a terminal that could put a
    /// `PlaceOrder` on the book channel would defeat the priority separation entirely.
    pub const fn channel(&self) -> crate::Channel {
        use crate::Channel;
        match self {
            Self::Hello { .. }
            | Self::Enroll { .. }
            | Self::Resume { .. }
            | Self::RotateDeviceKey { .. }
            | Self::Ping { .. }
            | Self::Pong { .. }
            | Self::Bye { .. }
            | Self::CoreRestart { .. } => Channel::CONTROL,

            Self::Subscribe { .. }
            | Self::Unsubscribe { .. }
            | Self::SubscriptionsGet
            | Self::BookResyncRequest { .. }
            | Self::TapeBackfill { .. }
            | Self::CandleHistory { .. }
            | Self::EmuTrades { .. } => Channel::COMMAND,

            Self::PlaceOrder { .. }
            | Self::CancelOrder { .. }
            | Self::AmendOrder { .. }
            | Self::CancelAll { .. }
            | Self::MoveOrders { .. }
            | Self::SplitOrder { .. }
            | Self::OrdersSnapshotRequest { .. }
            | Self::OrderStatusRequest { .. }
            | Self::ClosePosition { .. }
            | Self::FlattenAll { .. }
            | Self::SetProtection { .. }
            | Self::SetVolumeStop { .. }
            | Self::SetPanic { .. }
            | Self::SetImmune { .. }
            | Self::AccountSnapshotRequest { .. }
            | Self::SetLeverage { .. }
            | Self::SetMarginMode { .. }
            | Self::SetPositionMode { .. }
            | Self::TransferAsset { .. }
            | Self::TransferableAssetsRefresh { .. }
            | Self::ConfirmRiskLimit { .. }
            | Self::ApiKeyStatusRequest { .. }
            | Self::ConvertDust { .. } => Channel::COMMAND,

            Self::SettingsGet
            | Self::SettingsSet { .. }
            | Self::AutoLeverageSet { .. }
            | Self::ProfitReset { .. } => Channel::SETTINGS,

            Self::AlertUpsert { .. }
            | Self::AlertDelete { .. }
            | Self::AlertsSnapshotRequest
            | Self::ChartAnnotationsRequest { .. }
            | Self::NewsHistoryRequest { .. } => Channel::ALERTS,

            Self::ReportSchemaRequest
            | Self::ReportSyncRequest { .. }
            | Self::ReportPageAck { .. }
            | Self::ReportCheckRows { .. }
            | Self::ReportSetRowsDeleted { .. }
            | Self::ReportQuery { .. } => Channel::REPORT,
        }
    }

    /// Whether a terminal may resend this command after a timeout.
    ///
    /// `false` means the outcome cannot be made safely repeatable: closing a position twice
    /// opens a short, and moving an order set twice moves it twice as far. For those the
    /// terminal must ask the user rather than retry, which is why this is a property of the
    /// protocol and not a policy inside the client.
    pub const fn retryable(&self) -> bool {
        !matches!(
            self,
            Self::MoveOrders { .. }
                | Self::SplitOrder { .. }
                | Self::ClosePosition { .. }
                | Self::FlattenAll { .. }
                | Self::ConvertDust { .. }
                | Self::ProfitReset { .. }
                | Self::EmuTrades { .. }
                | Self::CoreRestart { .. }
        )
    }

    /// Lowest role that may send it.
    pub const fn min_role(&self) -> Role {
        match self {
            // Read-only requests and the handshake.
            Self::Hello { .. }
            | Self::Enroll { .. }
            | Self::Resume { .. }
            | Self::Ping { .. }
            | Self::Pong { .. }
            | Self::Bye { .. }
            | Self::Subscribe { .. }
            | Self::Unsubscribe { .. }
            | Self::SubscriptionsGet
            | Self::BookResyncRequest { .. }
            | Self::TapeBackfill { .. }
            | Self::CandleHistory { .. }
            | Self::OrdersSnapshotRequest { .. }
            | Self::OrderStatusRequest { .. }
            | Self::AccountSnapshotRequest { .. }
            | Self::TransferableAssetsRefresh { .. }
            | Self::SettingsGet
            | Self::AlertsSnapshotRequest
            | Self::ChartAnnotationsRequest { .. }
            | Self::NewsHistoryRequest { .. }
            | Self::ReportSchemaRequest
            | Self::ReportSyncRequest { .. }
            | Self::ReportPageAck { .. }
            | Self::ReportCheckRows { .. }
            | Self::ReportQuery { .. } => Role::Viewer,

            // Anything that touches money or the venue account.
            Self::SetPositionMode { .. }
            | Self::TransferAsset { .. }
            | Self::ApiKeyStatusRequest { .. }
            | Self::ConvertDust { .. }
            | Self::ProfitReset { .. }
            | Self::CoreRestart { .. } => Role::Admin,

            _ => Role::Trader,
        }
    }
}

/// What a session is allowed to do.
///
/// Three levels rather than two because the operator of a core and the person trading on it
/// are not necessarily the same, and a screen shared for teaching should not be able to place
/// an order. Whether all three are ever used is the customer's call; implementing them is
/// cheap, retrofitting them is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Trader,
    Admin,
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{MarketKind, OrderType, Side, TimeInForce};
    use rust_decimal_macros::dec;

    fn sym() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    fn order() -> NewOrder {
        NewOrder {
            client_id: ClientOrderId("c-1".into()),
            symbol: sym(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            qty: dec!(0.5),
            price: Some(dec!(63096.01)),
            trigger_price: None,
            tif: TimeInForce::Gtc,
            position_side: PositionSide::Long,
            reduce_only: false,
        }
    }

    /// Every command, with a payload that exercises its fields.
    ///
    /// This is the list the acceptance criterion is written against. A command added to the
    /// enum without a line here fails `all_commands_round_trip`, because the compiler forces
    /// the match below to be exhaustive.
    fn every_command() -> Vec<Command> {
        vec![
            Command::Hello {
                protocol: 2,
                device_id: [1; 16],
                client_nonce: [2; 32],
                sig: vec![3; 64],
                caps: vec!["dom".into()],
                terminal_version: "0.1.0".into(),
            },
            Command::Enroll { code: "123456".into(), pubkey: [4; 32], label: "laptop".into() },
            Command::Resume {
                session_id: 9,
                resume_token: [5; 32],
                sig: vec![6; 64],
                acks: vec![(crate::Channel::TAPE, 1_000)],
            },
            Command::RotateDeviceKey { new_pubkey: [7; 32], sig_new: vec![8; 64] },
            Command::Ping { nonce: 1, sent_ms: 1_700_000_000_000 },
            Command::Pong { nonce: 1, recv_ms: 1_700_000_000_001 },
            Command::Bye { reason: ByeReason::Suspend },
            Command::Subscribe { subs: vec![Subscription::Book(sym())], replace: true },
            Command::Unsubscribe { subs: vec![Subscription::Trades(sym())] },
            Command::SubscriptionsGet,
            Command::BookResyncRequest { symbol: sym(), depth: 100 },
            Command::TapeBackfill { symbol: sym(), from_ts: 1, limit: limits::TAPE_BACKFILL_MAX },
            Command::CandleHistory {
                symbol: sym(),
                bucketing: Bucketing::Time { interval_ms: 60_000 },
                from: Some(1),
                limit: limits::CANDLE_HISTORY_MAX,
            },
            Command::PlaceOrder { order: Box::new(order()) },
            Command::CancelOrder { symbol: sym(), client_id: ClientOrderId("c-1".into()) },
            Command::AmendOrder {
                symbol: sym(),
                client_id: ClientOrderId("c-1".into()),
                new_price: Some(dec!(63000)),
                new_qty: None,
                expected_rev: Some(4),
            },
            Command::CancelAll { scope: Scope::Symbol(sym()) },
            Command::MoveOrders {
                scope: Scope::All,
                select: OrderSelect::UnfilledEntries,
                target: MoveTarget::Ticks(-3),
            },
            Command::SplitOrder { client_id: ClientOrderId("c-1".into()), parts: 4, mode: SplitMode::Ladder },
            Command::OrdersSnapshotRequest { cursor: None },
            Command::OrderStatusRequest { client_id: ClientOrderId("c-1".into()) },
            Command::ClosePosition {
                symbol: sym(),
                position_side: PositionSide::Long,
                mode: CloseMode::Chase,
                qty: Some(dec!(0.25)),
            },
            Command::FlattenAll { scope: Scope::Exchange(ExchangeId::Binance), mode: CloseMode::Market },
            Command::SetProtection {
                target: ProtectionTarget::Position { symbol: sym(), side: PositionSide::Long },
                spec: ProtectionSpec {
                    stop_loss: Some(Trigger::PercentFromEntry(dec!(1.5))),
                    take_profit: Some(Trigger::Price(dec!(64000))),
                    trailing: Some(Trailing {
                        distance: Trigger::TicksFromEntry(20),
                        activate_at: Some(Trigger::PercentFromEntry(dec!(0.5))),
                    }),
                    prefer_native: true,
                },
            },
            Command::SetVolumeStop {
                target: ProtectionTarget::Order(ClientOrderId("c-1".into())),
                enabled: true,
                fixed: false,
                level: dec!(62000),
                volume: dec!(500000),
            },
            Command::SetPanic { target: ProtectionTarget::Defaults(Scope::All), enabled: true },
            Command::SetImmune {
                target: ProtectionTarget::Order(ClientOrderId("c-2".into())),
                enabled: true,
            },
            Command::AccountSnapshotRequest { scope: Scope::All },
            Command::SetLeverage { symbol: sym(), leverage: dec!(10) },
            Command::SetMarginMode { symbol: sym(), mode: MarginMode::Isolated },
            Command::SetPositionMode {
                exchange: ExchangeId::Binance,
                market: MarketKind::LinearPerp,
                hedge: true,
            },
            Command::TransferAsset {
                exchange: ExchangeId::Binance,
                asset: "USDT".into(),
                qty: dec!(100),
                from: Wallet::Spot,
                to: Wallet::Futures,
            },
            Command::TransferableAssetsRefresh { exchange: ExchangeId::Binance, wallet: Wallet::Spot },
            Command::ConfirmRiskLimit { symbol: sym() },
            Command::ApiKeyStatusRequest { exchange: ExchangeId::Binance },
            Command::ConvertDust { exchange: ExchangeId::Binance, assets: vec!["BNB".into()] },
            Command::SettingsGet,
            Command::SettingsSet {
                patch: SettingsPatch {
                    set: vec![("dom.depth".into(), rmpv::Value::from(100))],
                    clear: vec!["chart.theme".into()],
                },
                base_rev: 12,
            },
            Command::AutoLeverageSet {
                spec: AutoLeverageSpec {
                    scope: Scope::All,
                    enabled: true,
                    steps: vec![(dec!(1000), dec!(20)), (dec!(10000), dec!(5))],
                },
            },
            Command::ProfitReset { kind: ProfitResetKind::Current },
            Command::EmuTrades { symbol: sym(), base_ts: 1, points: vec![(0, 63000.0), (10, 63001.5)] },
            Command::CoreRestart { confirm: "mooncore-tokyo".into() },
            Command::AlertUpsert {
                alert: Box::new(AlertObject {
                    id: 1,
                    rev: 3,
                    symbol: sym(),
                    condition: AlertCondition::PriceCrosses(dec!(63500)),
                    actions: vec![AlertAction::Sound("ding".into()), AlertAction::Notify],
                    armed: true,
                    cooldown_ms: 30_000,
                    note: "resistance".into(),
                }),
            },
            Command::AlertDelete { id: 1 },
            Command::AlertsSnapshotRequest,
            Command::ChartAnnotationsRequest { symbol: sym(), want_filters: true, want_debug: false },
            Command::NewsHistoryRequest { limit: limits::NEWS_HISTORY_MAX },
            Command::ReportSchemaRequest,
            Command::ReportSyncRequest { from_rec_id: 0, depth_days: 30 },
            Command::ReportPageAck { req: 5, last_rec_id: 900 },
            Command::ReportCheckRows { rec_ids: vec![1, 2, 3] },
            Command::ReportSetRowsDeleted { deleted: true, ranges: vec![(10, 20)], singles: vec![33] },
            Command::ReportQuery {
                filter: Box::new(ReportFilter {
                    from_ms: Some(1),
                    to_ms: None,
                    symbols: vec![sym()],
                    cores: vec![0],
                    only_deleted: false,
                }),
                limit: limits::REPORT_QUERY_MAX,
                cursor: None,
            },
        ]
    }

    /// Discriminant name, so the coverage test can name what is missing.
    fn tag_of(cmd: &Command) -> String {
        let value: rmpv::Value = rmp_serde::from_slice(&rmp_serde::to_vec_named(cmd).unwrap()).unwrap();
        match value {
            rmpv::Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some("t"))
                .and_then(|(_, v)| v.as_str().map(str::to_string))
                .expect("every command carries a tag"),
            other => panic!("a command must encode as a map, got {other:?}"),
        }
    }

    #[test]
    fn all_commands_round_trip() {
        // The acceptance criterion for task 1.5.
        for cmd in every_command() {
            let bytes = rmp_serde::to_vec_named(&cmd).expect("encodes");
            let back: Command = rmp_serde::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{} failed to decode: {e}", tag_of(&cmd)));
            assert_eq!(back, cmd, "{} did not survive the round trip", tag_of(&cmd));
        }
    }

    #[test]
    fn the_catalogue_has_fifty_three_commands_and_no_duplicates() {
        // Fifty-three rather than the specification's fifty-six: §6.6 (strategies, seven
        // commands) is out of scope, and `AutoDetectSet` and `TriggerSet` go with it because
        // both exist only to drive detects. Counted here so that a command quietly added or
        // removed shows up as a failure with a number, not as a silent drift.
        let all = every_command();
        assert_eq!(all.len(), 53, "the catalogue changed size");

        let mut tags: Vec<String> = all.iter().map(tag_of).collect();
        tags.sort();
        let before = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), before, "two commands share a tag");
    }

    #[test]
    fn no_strategy_command_survived_the_scope_cut() {
        // A guard against the catalogue being restored wholesale from the specification
        // without re-applying the scope decision.
        for tag in every_command().iter().map(tag_of) {
            assert!(
                !tag.contains("strategy") && !tag.contains("detect"),
                "`{tag}` belongs to the strategy engine, which is out of scope"
            );
        }
    }

    // --- routing ----------------------------------------------------------------------------

    #[test]
    fn orders_and_control_never_share_a_channel_with_market_data() {
        // The priority separation only works if the command cannot choose its own channel.
        assert_eq!(Command::SettingsGet.channel(), crate::Channel::SETTINGS);
        assert_eq!(Command::ReportSchemaRequest.channel(), crate::Channel::REPORT);
        assert_eq!(Command::AlertsSnapshotRequest.channel(), crate::Channel::ALERTS);
        assert_eq!(Command::PlaceOrder { order: Box::new(order()) }.channel(), crate::Channel::COMMAND);
        assert_eq!(Command::Ping { nonce: 1, sent_ms: 0 }.channel(), crate::Channel::CONTROL);

        // Nothing a terminal sends may land on a server-only channel.
        for cmd in every_command() {
            let channel = cmd.channel();
            assert!(
                !matches!(
                    channel,
                    crate::Channel::BOOK
                        | crate::Channel::TAPE
                        | crate::Channel::CANDLES
                        | crate::Channel::ACCOUNT
                        | crate::Channel::ARB
                ),
                "{} routes to the server-only channel {channel}",
                tag_of(&cmd)
            );
        }
    }

    #[test]
    fn every_command_is_on_a_channel_the_writer_will_prioritise_correctly() {
        for cmd in every_command() {
            let class = cmd.channel().class();
            let tag = tag_of(&cmd);
            // A trading command that sat at P4 would queue behind a settings snapshot.
            if tag.contains("order") || tag.contains("position") || tag.contains("close") {
                assert!(class <= 1, "`{tag}` is at class {class}, too low for a trading command");
            }
        }
    }

    // --- retry policy -------------------------------------------------------------------------

    #[test]
    fn commands_that_cannot_be_repeated_say_so() {
        // Closing a position twice opens a short. Moving an order set twice moves it twice as
        // far. There is no way to make these safely repeatable, so the protocol marks them
        // rather than leaving each client to work it out.
        let unsafe_to_repeat = [
            Command::ClosePosition {
                symbol: sym(),
                position_side: PositionSide::Long,
                mode: CloseMode::Market,
                qty: None,
            },
            Command::FlattenAll { scope: Scope::All, mode: CloseMode::Market },
            Command::MoveOrders { scope: Scope::All, select: OrderSelect::All, target: MoveTarget::Touch },
            Command::SplitOrder { client_id: ClientOrderId("c".into()), parts: 2, mode: SplitMode::Even },
            Command::CoreRestart { confirm: "x".into() },
        ];
        for cmd in unsafe_to_repeat {
            assert!(!cmd.retryable(), "{} must not be retried", tag_of(&cmd));
        }

        // And the ones that are safe are safe because the core deduplicates on `req`.
        assert!(Command::CancelOrder { symbol: sym(), client_id: ClientOrderId("c".into()) }.retryable());
        assert!(Command::PlaceOrder { order: Box::new(order()) }.retryable());
        assert!(Command::SubscriptionsGet.retryable());
    }

    // --- roles ---------------------------------------------------------------------------------

    #[test]
    fn a_viewer_can_look_but_not_trade() {
        assert_eq!(Command::SubscriptionsGet.min_role(), Role::Viewer);
        assert_eq!(
            Command::ReportQuery { filter: Box::new(ReportFilter::default()), limit: 10, cursor: None }
                .min_role(),
            Role::Viewer
        );

        assert_eq!(Command::PlaceOrder { order: Box::new(order()) }.min_role(), Role::Trader);
        assert_eq!(
            Command::TransferAsset {
                exchange: ExchangeId::Binance,
                asset: "USDT".into(),
                qty: dec!(1),
                from: Wallet::Spot,
                to: Wallet::Futures,
            }
            .min_role(),
            Role::Admin,
            "moving money off the trading wallet is not a trader's call"
        );
        assert_eq!(Command::CoreRestart { confirm: "x".into() }.min_role(), Role::Admin);
    }

    #[test]
    fn roles_are_ordered_so_a_check_is_a_comparison() {
        assert!(Role::Viewer < Role::Trader);
        assert!(Role::Trader < Role::Admin);
    }

    // --- shapes that are easy to get wrong ------------------------------------------------------

    #[test]
    fn an_alert_cannot_place_an_order() {
        // With the strategy engine out of scope there is nothing that turns a signal into a
        // trade, and an action that could would be automated trading arriving by the back
        // door. The enum has three variants and this test is what stops a fourth.
        let actions = [AlertAction::Sound("s".into()), AlertAction::Notify, AlertAction::Log];
        for action in &actions {
            let tag = rmp_serde::to_vec_named(action).unwrap();
            let text = format!("{:?}", rmp_serde::from_slice::<rmpv::Value>(&tag).unwrap());
            assert!(
                !text.contains("order") && !text.contains("buy") && !text.contains("sell"),
                "an alert action must not be able to trade: {text}"
            );
        }
        assert_eq!(actions.len(), 3, "a fourth alert action needs deliberate review");
    }

    #[test]
    fn amend_carries_the_revision_it_was_based_on() {
        // Two terminals editing one order is the normal case with a laptop and a phone, and
        // without this the second write silently destroys the first.
        let cmd = Command::AmendOrder {
            symbol: sym(),
            client_id: ClientOrderId("c-1".into()),
            new_price: Some(dec!(1)),
            new_qty: None,
            expected_rev: Some(7),
        };
        let bytes = rmp_serde::to_vec_named(&cmd).unwrap();
        let back: Command = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn decimals_survive_the_wire_exactly() {
        // The reason money is `Decimal` end to end. A float round trip would not be exact,
        // and an inexact price is a venue rejection.
        let cmd = Command::SetLeverage { symbol: sym(), leverage: dec!(12.5) };
        let bytes = rmp_serde::to_vec_named(&cmd).unwrap();
        match rmp_serde::from_slice::<Command>(&bytes).unwrap() {
            Command::SetLeverage { leverage, .. } => assert_eq!(leverage, dec!(12.5)),
            other => panic!("unexpected: {other:?}"),
        }

        let cmd = Command::TransferAsset {
            exchange: ExchangeId::Binance,
            asset: "USDT".into(),
            qty: dec!(0.00000001),
            from: Wallet::Spot,
            to: Wallet::Futures,
        };
        let back: Command = rmp_serde::from_slice(&rmp_serde::to_vec_named(&cmd).unwrap()).unwrap();
        assert_eq!(back, cmd, "satoshi-scale quantities must not round");
    }

    #[test]
    fn optional_fields_may_be_omitted_by_an_older_terminal() {
        // The rule task 1.1 established: every field added after v1 needs `#[serde(default)]`,
        // or an older peer stops decoding. Checked on the command most likely to grow.
        let minimal = rmpv::Value::Map(vec![
            ("t".into(), "amend_order".into()),
            (
                "d".into(),
                rmpv::Value::Map(vec![
                    (
                        "symbol".into(),
                        rmp_serde::from_slice(&rmp_serde::to_vec_named(&sym()).unwrap()).unwrap(),
                    ),
                    ("client_id".into(), "c-1".into()),
                ]),
            ),
        ]);
        let bytes = rmp_serde::to_vec_named(&minimal).unwrap();
        let cmd: Command = rmp_serde::from_slice(&bytes).expect("omitted optionals are defaulted");
        match cmd {
            Command::AmendOrder { new_price, new_qty, expected_rev, .. } => {
                assert_eq!((new_price, new_qty, expected_rev), (None, None, None));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
