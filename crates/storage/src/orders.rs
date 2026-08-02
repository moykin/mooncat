//! Recording an order before it exists anywhere else.
//!
//! # The window this closes
//!
//! Sending an order is irreversible and takes an unknown amount of time. Between the decision
//! and the venue's acknowledgement there is a window in which the process can die, and what is
//! on disk when it comes back decides what happens next.
//!
//! Record after sending, and a crash inside the window leaves an order at the venue that the
//! core has never heard of. It will not appear in reconciliation, because reconciliation
//! compares what the venue has against what we recorded — and we recorded nothing. The
//! position it opens is managed by nobody, has no stop, and is discovered when someone looks.
//!
//! Record **before** sending, and the worst case is an order marked `Pending` that the venue
//! never received. Reconciliation finds it, asks the venue, gets nothing, and marks it dead.
//! One is a stray row, the other is an unmanaged position.
//!
//! # Why the write is waited on
//!
//! [`record_intent`] blocks until the row has committed. Queuing it and carrying on would
//! reintroduce the same window, only narrower — and a narrower race is harder to find, not
//! less real.

use crate::schema::sql;
use crate::store::{Store, StoreError, Value, Write};
use domain::{NewOrder, Order, OrderStatus};

/// State an order is written in before anyone has heard of it.
///
/// The same [`OrderStatus::Pending`] the domain already defines — and it documents itself with
/// the same reasoning, which is a good sign the shape is right: "an order that dies here may
/// still be live on the venue and must be reconciled at startup". This constant is the name it
/// is stored under, kept next to the code that writes it.
pub const PENDING: &str = "pending";

/// Write an order as `pending` and wait for the commit.
///
/// Must be called **before** the request goes to the venue. The return is the point at which
/// it is safe to send: the row exists, so a crash from here on is recoverable.
pub fn record_intent(store: &Store, order: &NewOrder, core_uid: i64, now_ms: i64) -> Result<(), StoreError> {
    store.write_durable(vec![Write::new(
        sql::INSERT_ORDER,
        vec![
            Value::Text(order.client_id.0.clone()),
            Value::Null,
            Value::Null,
            Value::Int(core_uid),
            Value::Text(order.symbol.exchange.to_string()),
            Value::Text(order.symbol.market.to_string()),
            Value::Text(order.symbol.raw.clone()),
            Value::Text(side_of(order.side).into()),
            Value::Text(format!("{:?}", order.order_type).to_lowercase()),
            Value::Text(PENDING.into()),
            Value::Text(order.qty.to_string()),
            Value::Text("0".into()),
            order.price.map_or(Value::Null, |p| Value::Text(p.to_string())),
            order.trigger_price.map_or(Value::Null, |p| Value::Text(p.to_string())),
            Value::Text("0".into()),
            Value::Int(i64::from(order.reduce_only)),
            Value::Int(now_ms),
            Value::Int(now_ms),
            Value::Int(0),
        ],
    )])
}

/// Move an order on once the venue has answered.
///
/// The revision is bumped in the same statement as the status, so a reader can never see a
/// status from one update and a revision from another.
pub fn record_update(store: &Store, order: &Order, now_ms: i64) -> Result<(), StoreError> {
    store.write_durable(vec![Write::new(
        "UPDATE orders SET status = ?2, venue_id = ?3, filled_qty = ?4, avg_price = ?5, \
         updated_at = ?6, rev = rev + 1 WHERE client_id = ?1",
        vec![
            Value::Text(order.client_id.0.clone()),
            Value::Text(status_of(order.status).into()),
            order.venue_id.clone().map_or(Value::Null, Value::Text),
            Value::Text(order.filled_qty.to_string()),
            Value::Text(order.avg_price.to_string()),
            Value::Int(now_ms),
        ],
    )])
}

/// Mark an order the venue turned out never to have received.
///
/// The other end of the window: reconciliation asked, the venue has nothing, and the row must
/// stop looking like something that needs managing.
pub fn record_never_placed(store: &Store, client_id: &str, now_ms: i64) -> Result<(), StoreError> {
    store.write_durable(vec![Write::new(
        "UPDATE orders SET status = 'rejected', updated_at = ?2, rev = rev + 1 \
         WHERE client_id = ?1 AND status = 'pending'",
        vec![Value::Text(client_id.into()), Value::Int(now_ms)],
    )])
}

fn side_of(side: domain::Side) -> &'static str {
    match side {
        domain::Side::Buy => "buy",
        domain::Side::Sell => "sell",
    }
}

/// Lowercase names matching the `status` values the schema and the reconciliation query use.
///
/// Written out rather than derived from `Debug`, because a rename of a variant would silently
/// change what is on disk and make every historical row unmatchable.
fn status_of(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Pending => PENDING,
        OrderStatus::New => "new",
        OrderStatus::PartiallyFilled => "partially_filled",
        OrderStatus::Filled => "filled",
        OrderStatus::Canceled => "canceled",
        OrderStatus::Rejected => "rejected",
        OrderStatus::Expired => "expired",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::{unresolved_orders, Reader};
    use crate::schema::Migrator;
    use domain::{ClientOrderId, ExchangeId, MarketKind, OrderType, PositionSide, Side, Symbol};
    use domain::{TimeInForce, Timestamp};
    use rust_decimal_macros::dec;
    use std::path::PathBuf;

    const NOW: i64 = 1_700_000_000_000;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-ord-{}-{n}-{tag}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn db(&self) -> PathBuf {
            self.0.join("moon.sqlite")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ready(tag: &str) -> (TempDir, Store) {
        let dir = TempDir::new(tag);
        let store = Store::open(dir.db()).expect("opens");
        Migrator::migrate(&store).expect("migrates");
        (dir, store)
    }

    fn new_order(client_id: &str) -> NewOrder {
        NewOrder {
            client_id: ClientOrderId(client_id.into()),
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT"),
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

    fn placed(client_id: &str, status: OrderStatus) -> Order {
        let n = new_order(client_id);
        Order {
            client_id: n.client_id,
            venue_id: Some("v-1".into()),
            symbol: n.symbol,
            side: n.side,
            order_type: n.order_type,
            status,
            qty: n.qty,
            filled_qty: dec!(0.25),
            price: n.price,
            trigger_price: None,
            avg_price: dec!(63096.01),
            tif: n.tif,
            position_side: n.position_side,
            reduce_only: false,
            created_at: Timestamp::from_millis(NOW),
            updated_at: Timestamp::from_millis(NOW + 1),
        }
    }

    fn status(store: &Store, client_id: &str) -> String {
        store
            .reader()
            .unwrap()
            .query_row("SELECT status FROM orders WHERE client_id = ?1", [client_id], |r| r.get(0))
            .unwrap()
    }

    // --- the acceptance criterion -------------------------------------------------------------

    #[test]
    fn an_order_killed_between_the_write_and_the_send_is_found_at_startup() {
        // The acceptance criterion for task 5.6, and the reason the whole storage layer exists.
        //
        // The process is simulated as dying immediately after `record_intent` and before
        // anything reaches the venue. What matters is what the next start finds.
        let dir = TempDir::new("crash");
        {
            let store = Store::open(dir.db()).unwrap();
            Migrator::migrate(&store).unwrap();
            record_intent(&store, &new_order("c-1"), 0, NOW).expect("recorded before sending");
            // …and here the process dies. Nothing was sent.
        }

        let store = Store::open(dir.db()).unwrap();
        let mut reader = Reader::open(&store).unwrap();
        let found = reader.snapshot(unresolved_orders).unwrap();

        assert_eq!(found, vec!["c-1"], "the order must be waiting for reconciliation");
        assert_eq!(status(&store, "c-1"), PENDING, "and must say it was never confirmed");
    }

    #[test]
    fn the_write_has_committed_before_record_intent_returns() {
        // Queuing it and carrying on would reintroduce the same window, only narrower — and a
        // narrower race is harder to find, not less real.
        let (dir, store) = ready("durable");
        record_intent(&store, &new_order("c-1"), 0, NOW).unwrap();

        // An independent connection, which knows nothing of the writer's in-memory state.
        let independent = rusqlite::Connection::open(dir.db()).unwrap();
        let count: i64 = independent
            .query_row("SELECT COUNT(*) FROM orders WHERE client_id='c-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "the row must be on disk when the call returns");
    }

    #[test]
    fn pending_is_not_a_status_the_venue_can_report() {
        // It describes the one state where the venue believes nothing, so it must not collide
        // with anything the venue can say — otherwise a real venue status would be mistaken
        // for an unsent order and reconciled away.
        // `Pending` is the exception: it maps to the same name on purpose.
        for status in [
            OrderStatus::New,
            OrderStatus::PartiallyFilled,
            OrderStatus::Filled,
            OrderStatus::Canceled,
            OrderStatus::Rejected,
            OrderStatus::Expired,
        ] {
            assert_ne!(status_of(status), PENDING);
        }
    }

    // --- the lifecycle ----------------------------------------------------------------------------

    #[test]
    fn an_order_moves_out_of_pending_once_the_venue_answers() {
        let (_dir, store) = ready("confirm");
        record_intent(&store, &new_order("c-1"), 0, NOW).unwrap();
        assert_eq!(status(&store, "c-1"), PENDING);

        record_update(&store, &placed("c-1", OrderStatus::New), NOW + 10).unwrap();
        assert_eq!(status(&store, "c-1"), "new");

        let mut reader = Reader::open(&store).unwrap();
        assert_eq!(
            reader.snapshot(unresolved_orders).unwrap(),
            vec!["c-1"],
            "still unresolved: a live order is exactly what reconciliation is for"
        );
    }

    #[test]
    fn a_filled_order_leaves_the_reconciliation_list() {
        let (_dir, store) = ready("filled");
        record_intent(&store, &new_order("c-1"), 0, NOW).unwrap();
        record_update(&store, &placed("c-1", OrderStatus::Filled), NOW + 10).unwrap();

        let mut reader = Reader::open(&store).unwrap();
        assert!(reader.snapshot(unresolved_orders).unwrap().is_empty());
    }

    #[test]
    fn an_order_the_venue_never_received_is_closed_out() {
        // The other end of the window: reconciliation asked, the venue has nothing, and the row
        // must stop looking like something that needs managing.
        let (_dir, store) = ready("never");
        record_intent(&store, &new_order("c-1"), 0, NOW).unwrap();
        record_never_placed(&store, "c-1", NOW + 10).unwrap();

        assert_eq!(status(&store, "c-1"), "rejected");
        let mut reader = Reader::open(&store).unwrap();
        assert!(reader.snapshot(unresolved_orders).unwrap().is_empty());
    }

    #[test]
    fn closing_out_only_touches_orders_still_pending() {
        // A late reconciliation must not overwrite an order that has since been confirmed —
        // that would turn a live order into a rejected row while it sits at the venue.
        let (_dir, store) = ready("late");
        record_intent(&store, &new_order("c-1"), 0, NOW).unwrap();
        record_update(&store, &placed("c-1", OrderStatus::New), NOW + 10).unwrap();

        record_never_placed(&store, "c-1", NOW + 20).unwrap();
        assert_eq!(status(&store, "c-1"), "new", "a confirmed order must not be closed out");
    }

    #[test]
    fn the_revision_moves_with_every_update() {
        // It is the optimistic lock two terminals editing one order depend on, and it is bumped
        // in the same statement as the status so a reader cannot see one without the other.
        let (_dir, store) = ready("rev");
        record_intent(&store, &new_order("c-1"), 0, NOW).unwrap();

        let rev = |s: &Store| -> i64 {
            s.reader()
                .unwrap()
                .query_row("SELECT rev FROM orders WHERE client_id='c-1'", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(rev(&store), 0);

        record_update(&store, &placed("c-1", OrderStatus::New), NOW + 1).unwrap();
        assert_eq!(rev(&store), 1);
        record_update(&store, &placed("c-1", OrderStatus::PartiallyFilled), NOW + 2).unwrap();
        assert_eq!(rev(&store), 2);
    }

    #[test]
    fn the_recorded_order_carries_what_reconciliation_needs_to_ask_the_venue() {
        // Symbol, side and quantity, because the query to the venue is by client id and the
        // answer has to be checked against what was intended.
        let (_dir, store) = ready("fields");
        record_intent(&store, &new_order("c-1"), 7, NOW).unwrap();

        let (symbol, side, qty, price, core_uid): (String, String, String, String, i64) = store
            .reader()
            .unwrap()
            .query_row(
                "SELECT symbol, side, qty, price, core_uid FROM orders WHERE client_id='c-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();

        assert_eq!(symbol, "BTCUSDT");
        assert_eq!(side, "buy");
        assert_eq!(qty, "0.5");
        assert_eq!(price, "63096.01", "the exact decimal, not a rounded one");
        assert_eq!(core_uid, 7);
    }

    #[test]
    fn two_orders_with_the_same_id_cannot_both_exist() {
        // The client id is what reconciliation matches on, so a duplicate would make two
        // orders indistinguishable at exactly the moment it matters.
        let (_dir, store) = ready("dup");
        record_intent(&store, &new_order("c-1"), 0, NOW).unwrap();
        assert!(record_intent(&store, &new_order("c-1"), 0, NOW).is_err());
    }

    #[test]
    fn status_names_are_written_out_rather_than_derived() {
        // Deriving them from `Debug` would mean a variant rename silently changes what is on
        // disk, and every historical row becomes unmatchable.
        assert_eq!(status_of(OrderStatus::PartiallyFilled), "partially_filled");
        assert_eq!(status_of(OrderStatus::Filled), "filled");
        assert_eq!(side_of(Side::Sell), "sell");
    }
}
