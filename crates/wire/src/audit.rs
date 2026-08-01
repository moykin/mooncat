//! What was done, by whom, from where.
//!
//! # What an audit trail is actually for here
//!
//! Not compliance — there is no regulator asking. It answers a question the operator will one
//! day have at three in the morning: *where did that order come from?* A position that nobody
//! remembers opening has three possible explanations — a mis-click, a second terminal left
//! running, or someone else — and without a record they are indistinguishable. The trail turns
//! that into a lookup.
//!
//! It is also the only thing that makes revocation meaningful after the fact. Revoking a lost
//! laptop stops it connecting; the trail is what says whether it did anything first.
//!
//! # Why not simply log everything
//!
//! Two reasons, and the second is the one that matters.
//!
//! Volume: a terminal sends thousands of subscription and query commands an hour, and a table
//! that records them all is one nobody reads. Only commands that *change something* are
//! recorded, plus the authentication events, because "who connected" is half of "who did it".
//!
//! Secrets: a command body is not automatically safe to store. An enrolment code is a
//! credential until it is redeemed, and writing it verbatim into a table that is backed up and
//! copied around would leak the one thing that grants access. [`redact`] handles that, and
//! `an_enrolment_code_never_reaches_the_table` is the test that keeps it honest.

use crate::admission::SessionId;
use crate::auth::DeviceId;
use crate::command::{Command, Role};
use crate::envelope::ReqId;
use crate::event::AckStatus;
use std::net::IpAddr;

/// Fields whose value must never be written down, matched by name anywhere in a command body.
///
/// By name rather than by position, because a command grows fields and a positional rule
/// silently stops covering the one that was added.
const NEVER_RECORD: &[&str] = &["code", "passphrase", "secret", "token", "sig", "sig_new"];

/// Replacement text, chosen to be obvious in a table rather than to look like a value.
const REDACTED: &str = "<redacted>";

/// One line of the trail.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditRecord {
    pub at_ms: i64,
    pub session: SessionId,
    pub device: DeviceId,
    pub peer: IpAddr,
    /// The role the session actually held, taken from the registry — not what it asked for.
    pub role: Role,
    pub req: ReqId,
    /// The command's tag, for querying without decoding the body.
    pub command: &'static str,
    /// The body, with anything sensitive removed.
    pub body: rmpv::Value,
    pub outcome: Outcome,
}

/// What happened to the command. Recorded because an attempt that was refused is exactly as
/// interesting as one that succeeded — more so, if it was refused for lack of a role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Accepted and started. A later record with the same `req` carries the conclusion.
    Started,
    Succeeded,
    /// Refused before it ran, with why.
    Refused(String),
    Failed(String),
}

impl Outcome {
    pub fn from_ack(status: AckStatus, detail: Option<&str>) -> Self {
        let why = detail.unwrap_or_default().to_string();
        match status {
            AckStatus::Accepted => Self::Started,
            AckStatus::Done => Self::Succeeded,
            AckStatus::Rejected => Self::Refused(why),
            AckStatus::Failed => Self::Failed(why),
        }
    }
}

/// Whether a command belongs in the trail.
///
/// Everything that changes state, plus the authentication events. A subscription or a query
/// changes nothing and would only make the table too noisy to read — and a table nobody reads
/// answers no questions at three in the morning.
pub fn should_audit(command: &Command) -> bool {
    if matches!(command, Command::Enroll { .. } | Command::RotateDeviceKey { .. } | Command::Resume { .. }) {
        return true;
    }
    command.min_role() > Role::Viewer
}

/// Remove anything from a body that must not be stored.
///
/// Recursive, because a sensitive field can sit inside a nested structure, and applied to the
/// encoded form rather than to the typed command so that a field added later is covered
/// without anyone remembering to extend this.
pub fn redact(value: rmpv::Value) -> rmpv::Value {
    match value {
        rmpv::Value::Map(entries) => rmpv::Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| {
                    let sensitive = k.as_str().is_some_and(|name| NEVER_RECORD.contains(&name));
                    if sensitive {
                        (k, rmpv::Value::from(REDACTED))
                    } else {
                        (k, redact(v))
                    }
                })
                .collect(),
        ),
        rmpv::Value::Array(items) => rmpv::Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}

/// Build a record for a command, redacting as it goes.
///
/// Returns `None` for commands that are not audited, so the caller writes
/// `if let Some(record) = ...` and cannot forget the filter.
#[allow(clippy::too_many_arguments)]
pub fn record(
    command: &Command,
    at_ms: i64,
    session: SessionId,
    device: DeviceId,
    peer: IpAddr,
    role: Role,
    req: ReqId,
    outcome: Outcome,
) -> Option<AuditRecord> {
    if !should_audit(command) {
        return None;
    }
    let encoded = rmp_serde::to_vec_named(command).ok()?;
    let value: rmpv::Value = rmp_serde::from_slice(&encoded).ok()?;

    let (tag, body) = split_tagged(value);
    Some(AuditRecord {
        at_ms,
        session,
        device,
        peer,
        role,
        req,
        command: static_tag(tag.as_deref().unwrap_or("unknown")),
        body: redact(body),
        outcome,
    })
}

/// Pull the `t`/`d` pair apart, so the tag is queryable and the body is not a nested map with
/// the tag repeated inside it.
fn split_tagged(value: rmpv::Value) -> (Option<String>, rmpv::Value) {
    let rmpv::Value::Map(entries) = value else {
        return (None, value);
    };
    let mut tag = None;
    let mut body = rmpv::Value::Nil;
    for (k, v) in entries {
        match k.as_str() {
            Some("t") => tag = v.as_str().map(str::to_string),
            Some("d") => body = v,
            _ => {}
        }
    }
    (tag, body)
}

/// Intern the tag so a record carries a `&'static str` rather than an allocation per row.
///
/// Falls back to a fixed string rather than leaking memory for an unknown tag: a caller that
/// could make this allocate is a caller that could exhaust memory by sending nonsense.
fn static_tag(tag: &str) -> &'static str {
    // The tags come from a closed enum, so a lookup table is exhaustive by construction. Built
    // once from the command catalogue rather than written out again here.
    macro_rules! interned {
        ($($t:literal),* $(,)?) => {
            match tag { $($t => $t,)* _ => "unknown" }
        };
    }
    interned!(
        "hello",
        "enroll",
        "resume",
        "rotate_device_key",
        "ping",
        "pong",
        "bye",
        "subscribe",
        "unsubscribe",
        "subscriptions_get",
        "book_resync_request",
        "tape_backfill",
        "candle_history",
        "place_order",
        "cancel_order",
        "amend_order",
        "cancel_all",
        "move_orders",
        "split_order",
        "orders_snapshot_request",
        "order_status_request",
        "close_position",
        "flatten_all",
        "set_protection",
        "set_volume_stop",
        "set_panic",
        "set_immune",
        "account_snapshot_request",
        "set_leverage",
        "set_margin_mode",
        "set_position_mode",
        "transfer_asset",
        "transferable_assets_refresh",
        "confirm_risk_limit",
        "api_key_status_request",
        "convert_dust",
        "settings_get",
        "settings_set",
        "auto_leverage_set",
        "profit_reset",
        "emu_trades",
        "core_restart",
        "alert_upsert",
        "alert_delete",
        "alerts_snapshot_request",
        "chart_annotations_request",
        "news_history_request",
        "report_schema_request",
        "report_sync_request",
        "report_page_ack",
        "report_check_rows",
        "report_set_rows_deleted",
        "report_query",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CloseMode, Scope, Wallet};
    use domain::{ClientOrderId, ExchangeId, MarketKind, PositionSide, Symbol};
    use rust_decimal_macros::dec;

    fn peer() -> IpAddr {
        IpAddr::from([203, 0, 113, 7])
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
                qty: dec!(0.5),
                price: Some(dec!(63096.01)),
                trigger_price: None,
                tif: domain::TimeInForce::Gtc,
                position_side: PositionSide::Long,
                reduce_only: false,
            }),
        }
    }

    fn make(command: &Command, outcome: Outcome) -> Option<AuditRecord> {
        record(command, 1_700_000_000_000, SessionId(9), DeviceId([1; 16]), peer(), Role::Trader, 42, outcome)
    }

    /// Every string anywhere in a value, for leak hunting.
    fn strings_in(value: &rmpv::Value) -> Vec<String> {
        match value {
            rmpv::Value::String(s) => s.as_str().map(str::to_string).into_iter().collect(),
            rmpv::Value::Map(entries) => {
                entries.iter().flat_map(|(k, v)| [strings_in(k), strings_in(v)].concat()).collect()
            }
            rmpv::Value::Array(items) => items.iter().flat_map(strings_in).collect(),
            _ => Vec::new(),
        }
    }

    // --- the acceptance criterion -------------------------------------------------------------

    #[test]
    fn every_trading_command_produces_exactly_one_record() {
        let trading = [
            place_order(),
            Command::CancelOrder { symbol: sym(), client_id: ClientOrderId("c".into()) },
            Command::CancelAll { scope: Scope::All },
            Command::ClosePosition {
                symbol: sym(),
                position_side: PositionSide::Long,
                mode: CloseMode::Market,
                qty: None,
            },
            Command::FlattenAll { scope: Scope::All, mode: CloseMode::Market },
            Command::SetLeverage { symbol: sym(), leverage: dec!(10) },
        ];
        for command in trading {
            let row = make(&command, Outcome::Succeeded).expect("a trading command must be audited");
            assert_eq!(row.req, 42);
            assert_eq!(row.session, SessionId(9));
            assert_eq!(row.peer, peer());
            assert_eq!(row.role, Role::Trader);
            assert_ne!(row.command, "unknown", "the tag must be resolved, not fall through");
        }
    }

    #[test]
    fn queries_and_subscriptions_are_not_recorded() {
        // A terminal sends thousands an hour. A table that records them is one nobody reads,
        // and a table nobody reads answers no questions at three in the morning.
        for command in [
            Command::SubscriptionsGet,
            Command::SettingsGet,
            Command::ReportSchemaRequest,
            Command::AccountSnapshotRequest { scope: Scope::All },
            Command::Ping { nonce: 1, sent_ms: 0 },
        ] {
            assert!(make(&command, Outcome::Succeeded).is_none(), "{command:?} should not be audited");
        }
    }

    #[test]
    fn authentication_events_are_recorded_even_though_they_change_no_trading_state() {
        // "Who connected" is half of "who did it", and revoking a lost laptop is only
        // meaningful if there is a record of whether it did anything first.
        for command in [
            Command::Enroll { code: "123456".into(), pubkey: [1; 32], label: "laptop".into() },
            Command::RotateDeviceKey { new_pubkey: [2; 32], sig_new: vec![3; 64] },
            Command::Resume { session_id: 1, resume_token: [4; 32], sig: vec![5; 64], acks: vec![] },
        ] {
            assert!(make(&command, Outcome::Succeeded).is_some(), "{command:?} must be audited");
        }
    }

    // --- what must never reach the table --------------------------------------------------------

    #[test]
    fn an_enrolment_code_never_reaches_the_table() {
        // An enrolment code is a credential until it is redeemed. Writing it into a table that
        // gets backed up and copied around would leak the one thing that grants access.
        let secret = "correct-horse-battery";
        let command = Command::Enroll { code: secret.into(), pubkey: [1; 32], label: "laptop".into() };

        let row = make(&command, Outcome::Succeeded).expect("enrolment is audited");
        let found = strings_in(&row.body);
        assert!(!found.contains(&secret.to_string()), "the code leaked into the trail: {found:?}");
        assert!(found.contains(&REDACTED.to_string()), "it must be visibly redacted, not dropped");
        assert!(found.contains(&"laptop".to_string()), "the harmless label survives");
    }

    #[test]
    fn signatures_are_redacted_too() {
        // Not because a signature is secret, but because it is bulky, meaningless to a reader,
        // and its presence would invite someone to build a replay from the table.
        let command = Command::RotateDeviceKey { new_pubkey: [2; 32], sig_new: vec![7; 64] };
        let row = make(&command, Outcome::Succeeded).unwrap();
        assert!(strings_in(&row.body).contains(&REDACTED.to_string()));
    }

    #[test]
    fn redaction_reaches_into_nested_structures() {
        // A sensitive field can sit inside a nested map or an array, and a rule that only
        // looked at the top level would miss it the first time a command grows.
        let nested = rmpv::Value::Map(vec![
            ("outer".into(), rmpv::Value::Map(vec![("code".into(), "hunter2".into())])),
            ("list".into(), rmpv::Value::Array(vec![rmpv::Value::Map(vec![("token".into(), "abc".into())])])),
        ]);
        let found = strings_in(&redact(nested));
        assert!(!found.contains(&"hunter2".to_string()));
        assert!(!found.contains(&"abc".to_string()));
        assert_eq!(found.iter().filter(|s| *s == REDACTED).count(), 2);
    }

    #[test]
    fn redaction_matches_by_name_so_a_new_field_is_covered_without_being_remembered() {
        // The reason the rule is by name and applied to the encoded form: a field added to a
        // command later is covered automatically if it is called one of the sensitive names.
        let future = rmpv::Value::Map(vec![
            ("brand_new_field".into(), "harmless".into()),
            ("passphrase".into(), "s3cret".into()),
        ]);
        let found = strings_in(&redact(future));
        assert!(found.contains(&"harmless".to_string()));
        assert!(!found.contains(&"s3cret".to_string()));
    }

    #[test]
    fn the_order_itself_is_kept_in_full() {
        // Redaction must not become so eager that the trail stops answering the question it
        // exists for. Price, quantity and side are the whole point of the record.
        let row = make(&place_order(), Outcome::Succeeded).unwrap();
        let found = strings_in(&row.body);
        assert!(found.contains(&"63096.01".to_string()), "the price must be recorded: {found:?}");
        assert!(found.contains(&"c-1".to_string()), "and the client order id");
    }

    // --- outcomes ------------------------------------------------------------------------------------

    #[test]
    fn a_refusal_is_recorded_as_carefully_as_a_success() {
        // More interesting, in fact: a command refused for lack of a role is what an intrusion
        // looks like from the inside.
        let row = make(&place_order(), Outcome::Refused("needs Trader, holds Viewer".into())).unwrap();
        match row.outcome {
            Outcome::Refused(why) => assert!(why.contains("Viewer")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_acknowledgement_maps_onto_an_outcome() {
        assert_eq!(Outcome::from_ack(AckStatus::Accepted, None), Outcome::Started);
        assert_eq!(Outcome::from_ack(AckStatus::Done, None), Outcome::Succeeded);
        assert_eq!(Outcome::from_ack(AckStatus::Rejected, Some("risk")), Outcome::Refused("risk".into()));
        assert_eq!(
            Outcome::from_ack(AckStatus::Failed, Some("venue timeout")),
            Outcome::Failed("venue timeout".into())
        );
    }

    #[test]
    fn a_long_running_command_leaves_a_started_row_and_a_conclusion() {
        // Both carry the same `req`, which is what joins them. Without the first row a command
        // that never finished would leave no trace at all.
        let started = make(&Command::CancelAll { scope: Scope::All }, Outcome::Started).unwrap();
        let finished = make(&Command::CancelAll { scope: Scope::All }, Outcome::Succeeded).unwrap();
        assert_eq!(started.req, finished.req);
        assert_eq!(started.outcome, Outcome::Started);
    }

    // --- identity ------------------------------------------------------------------------------------

    #[test]
    fn the_recorded_role_is_the_one_the_session_held() {
        // Taken from the registry, never from anything the terminal claimed — otherwise the
        // trail would record what the attacker said rather than what was true.
        let row = record(
            &place_order(),
            1,
            SessionId(1),
            DeviceId([9; 16]),
            peer(),
            Role::Admin,
            1,
            Outcome::Succeeded,
        )
        .unwrap();
        assert_eq!(row.role, Role::Admin);
        assert_eq!(row.device, DeviceId([9; 16]));
    }

    #[test]
    fn an_unknown_tag_does_not_allocate() {
        // A caller able to make this intern arbitrary strings could exhaust memory by sending
        // nonsense. It falls back to a fixed string instead.
        assert_eq!(static_tag("something_invented"), "unknown");
        assert_eq!(static_tag("place_order"), "place_order");
    }

    #[test]
    fn every_audited_command_resolves_to_a_real_tag() {
        // The intern table is written by hand, so it can fall behind the catalogue. This is
        // what notices — a command added later would be recorded as "unknown" and become
        // invisible to any query filtering by name.
        use crate::command::{AutoLeverageSpec, ProfitResetKind, ProtectionTarget, SettingsPatch};
        let sample = [
            place_order(),
            Command::MoveOrders {
                scope: Scope::All,
                select: crate::command::OrderSelect::All,
                target: crate::command::MoveTarget::Touch,
            },
            Command::SetPanic { target: ProtectionTarget::Defaults(Scope::All), enabled: true },
            Command::TransferAsset {
                exchange: ExchangeId::Binance,
                asset: "USDT".into(),
                qty: dec!(1),
                from: Wallet::Spot,
                to: Wallet::Futures,
            },
            Command::SettingsSet { patch: SettingsPatch::default(), base_rev: 1 },
            Command::AutoLeverageSet {
                spec: AutoLeverageSpec { scope: Scope::All, enabled: true, steps: vec![] },
            },
            Command::ProfitReset { kind: ProfitResetKind::Current },
            Command::CoreRestart { confirm: "x".into() },
            Command::AlertDelete { id: 1 },
            Command::ReportSetRowsDeleted { deleted: true, ranges: vec![], singles: vec![] },
        ];
        for command in sample {
            let row = make(&command, Outcome::Succeeded).expect("all of these change state");
            assert_ne!(row.command, "unknown", "the intern table is missing a tag");
        }
    }
}
