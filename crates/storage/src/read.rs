//! Reading, without getting in the writer's way.
//!
//! # Why a snapshot rather than a series of queries
//!
//! An analytics panel asks several questions to fill one screen: the total for the period, the
//! best and worst trades, the daily breakdown. Run as separate queries they see different
//! states of the database, because trading continues between them. The numbers then disagree
//! with each other in ways nobody can reproduce — the total does not equal the sum of the
//! days, and both are individually correct.
//!
//! A read transaction fixes the view for its lifetime. With write-ahead logging that costs
//! nothing: readers do not block the writer and the writer does not block readers, so holding
//! a snapshot open for the duration of a screen is free.
//!
//! # Why the reader cannot write
//!
//! It is opened read-only at the connection level, not by discipline. A report query that took
//! the write lock would stall an order being recorded, and that trade-off is never worth
//! making — the report can wait, the order cannot.

use crate::store::{Store, StoreError};
use rusqlite::Connection;

/// A read-only view of the database.
pub struct Reader {
    connection: Connection,
}

impl Reader {
    pub fn open(store: &Store) -> Result<Self, StoreError> {
        Ok(Self { connection: store.reader()? })
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Run several queries against one fixed view of the data.
    ///
    /// Everything inside sees the database as it was when the closure started, however much
    /// trading happens meanwhile. That is what makes a screenful of numbers agree with itself.
    pub fn snapshot<T>(
        &mut self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, StoreError> {
        let tx = self.connection.unchecked_transaction()?;
        let out = f(&tx)?;
        // A read transaction has nothing to commit; ending it releases the snapshot.
        tx.finish()?;
        Ok(out)
    }
}

/// Aggregates a report screen asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeriodSummary {
    pub deals: i64,
    /// Summed as text through SQLite's decimal-free arithmetic would lose precision, so the
    /// total is accumulated in Rust from the exact strings instead.
    pub realized: String,
    pub wins: i64,
    pub losses: i64,
}

/// Everything closed within a period, for one core.
///
/// Bounded by `closed_at` and `core_uid` because that is what the index covers — report 05
/// measured the same query without it at seconds rather than milliseconds.
pub fn period_summary(
    connection: &Connection,
    core_uid: i64,
    from_ms: i64,
    to_ms: i64,
) -> Result<PeriodSummary, rusqlite::Error> {
    let mut stmt = connection.prepare(
        "SELECT realized FROM deals \
         WHERE core_uid = ?1 AND closed_at >= ?2 AND closed_at < ?3 AND phase = 'closed'",
    )?;
    let rows = stmt.query_map([core_uid, from_ms, to_ms], |r| r.get::<_, String>(0))?;

    let mut summary = PeriodSummary { realized: "0".into(), ..Default::default() };
    let mut total = rust_decimal::Decimal::ZERO;
    for row in rows {
        let text = row?;
        let value: rust_decimal::Decimal = text.parse().unwrap_or_default();
        summary.deals += 1;
        if value.is_sign_negative() && !value.is_zero() {
            summary.losses += 1;
        } else if !value.is_zero() {
            summary.wins += 1;
        }
        total += value;
    }
    summary.realized = total.to_string();
    Ok(summary)
}

/// Orders that were written but never reached a terminal state.
///
/// What the core asks for at startup: anything here is an order the venue may or may not know
/// about, and every one of them has to be reconciled before trading resumes.
pub fn unresolved_orders(connection: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = connection.prepare(
        "SELECT client_id FROM orders \
         WHERE status NOT IN ('filled','cancelled','rejected','expired') \
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{sql, text, Migrator};
    use crate::store::{Value, Write};
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-read-{}-{n}-{tag}", std::process::id()));
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

    fn closed_deal(id: i64, closed_at: i64, realized: &str) -> Write {
        Write::new(
            sql::INSERT_DEAL,
            vec![
                Value::Int(id),
                Value::Int(0),
                text("binance"),
                text("linear_perp"),
                text("BTCUSDT"),
                text("buy"),
                text("closed"),
                text("0.5"),
                text("63000"),
                text("63100"),
                text(realized),
                text("0.01"),
                text("take_profit"),
                Value::Int(closed_at - 1_000),
                Value::Int(closed_at),
            ],
        )
    }

    fn order(client_id: &str, status: &str, created_at: i64) -> Write {
        Write::new(
            sql::INSERT_ORDER,
            vec![
                text(client_id),
                Value::Null,
                Value::Null,
                Value::Int(0),
                text("binance"),
                text("linear_perp"),
                text("BTCUSDT"),
                text("buy"),
                text("limit"),
                text(status),
                text("0.5"),
                text("0"),
                text("63000"),
                Value::Null,
                text("0"),
                Value::Int(0),
                Value::Int(created_at),
                Value::Int(created_at),
                Value::Int(0),
            ],
        )
    }

    // --- the snapshot -----------------------------------------------------------------------

    #[test]
    fn a_snapshot_sees_one_state_however_much_is_written_meanwhile() {
        // The failure this prevents: a screenful of numbers that disagree with each other, all
        // individually correct, and nobody able to reproduce it.
        let (_dir, store) = ready("snapshot");
        for id in 0..10 {
            store.write_durable(vec![closed_deal(id, 1_000 + id, "10")]).unwrap();
        }

        let mut reader = Reader::open(&store).unwrap();
        let (first, second) = reader
            .snapshot(|c| {
                let before: i64 = c.query_row("SELECT COUNT(*) FROM deals", [], |r| r.get(0))?;

                // Trading continues while the screen is being filled.
                for id in 100..120 {
                    let _ = store.write_durable(vec![closed_deal(id, 2_000 + id, "5")]);
                }

                let after: i64 = c.query_row("SELECT COUNT(*) FROM deals", [], |r| r.get(0))?;
                Ok((before, after))
            })
            .unwrap();

        assert_eq!(first, second, "the view must not move under a snapshot");
        assert_eq!(first, 10);
    }

    #[test]
    fn a_new_snapshot_sees_what_was_written_since() {
        // The other half: it fixes the view for a screen, not forever.
        let (_dir, store) = ready("moves");
        store.write_durable(vec![closed_deal(1, 1_000, "10")]).unwrap();

        let mut reader = Reader::open(&store).unwrap();
        let before = reader
            .snapshot(|c| c.query_row("SELECT COUNT(*) FROM deals", [], |r| r.get::<_, i64>(0)))
            .unwrap();

        store.write_durable(vec![closed_deal(2, 2_000, "20")]).unwrap();
        let after = reader
            .snapshot(|c| c.query_row("SELECT COUNT(*) FROM deals", [], |r| r.get::<_, i64>(0)))
            .unwrap();

        assert_eq!((before, after), (1, 2));
    }

    #[test]
    fn a_reader_cannot_take_the_write_lock() {
        // Structural, not by convention: a report query that stalled an order being recorded
        // would be the wrong trade in every case.
        let (_dir, store) = ready("readonly");
        let reader = Reader::open(&store).unwrap();
        assert!(reader.connection().execute("DELETE FROM deals", []).is_err());
    }

    #[test]
    fn a_failing_query_inside_a_snapshot_releases_it() {
        // Otherwise one bad query holds a read transaction open for the life of the process,
        // and the write-ahead log can never be checkpointed again.
        let (_dir, store) = ready("release");
        let mut reader = Reader::open(&store).unwrap();

        assert!(reader.snapshot(|c| c.query_row("SELECT nope", [], |r| r.get::<_, i64>(0))).is_err());
        assert!(
            reader.snapshot(|c| c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))).is_ok(),
            "the reader must still work afterwards"
        );
    }

    // --- the queries the panels actually run ---------------------------------------------------

    #[test]
    fn a_period_summary_counts_only_what_closed_inside_it() {
        let (_dir, store) = ready("period");
        store.write_durable(vec![closed_deal(1, 500, "10")]).unwrap();
        store.write_durable(vec![closed_deal(2, 1_500, "20")]).unwrap();
        store.write_durable(vec![closed_deal(3, 2_500, "-5")]).unwrap();

        let mut reader = Reader::open(&store).unwrap();
        let summary = reader.snapshot(|c| period_summary(c, 0, 1_000, 3_000)).unwrap();

        assert_eq!(summary.deals, 2, "the one that closed before the period is excluded");
        assert_eq!(summary.wins, 1);
        assert_eq!(summary.losses, 1);
        assert_eq!(summary.realized, "15");
    }

    #[test]
    fn the_total_is_summed_exactly_and_not_through_a_float() {
        // SQLite has no decimal type, so `SUM()` over these columns would go through a double
        // and produce 0.30000000000000004 on the third trade. Summed in Rust from the exact
        // strings instead.
        let (_dir, store) = ready("exact");
        for (id, amount) in [(1, "0.1"), (2, "0.2"), (3, "0.00000001")] {
            store.write_durable(vec![closed_deal(id, 1_000 + id, amount)]).unwrap();
        }

        let mut reader = Reader::open(&store).unwrap();
        let summary = reader.snapshot(|c| period_summary(c, 0, 0, 9_999)).unwrap();
        assert_eq!(summary.realized, "0.30000001", "the total must be exact");
    }

    #[test]
    fn a_period_with_nothing_in_it_is_zero_rather_than_an_error() {
        // An empty week is a normal thing to look at, and it must not render as a blank panel.
        let (_dir, store) = ready("empty");
        let mut reader = Reader::open(&store).unwrap();
        let summary = reader.snapshot(|c| period_summary(c, 0, 0, 1)).unwrap();
        assert_eq!(summary, PeriodSummary { deals: 0, realized: "0".into(), wins: 0, losses: 0 });
    }

    #[test]
    fn a_break_even_deal_is_neither_a_win_nor_a_loss() {
        // Counting zero as a win inflates the win rate, which is the number a trader trusts
        // most and checks least.
        let (_dir, store) = ready("flat");
        store.write_durable(vec![closed_deal(1, 1_000, "0")]).unwrap();

        let mut reader = Reader::open(&store).unwrap();
        let summary = reader.snapshot(|c| period_summary(c, 0, 0, 9_999)).unwrap();
        assert_eq!((summary.deals, summary.wins, summary.losses), (1, 0, 0));
    }

    #[test]
    fn one_cores_deals_do_not_appear_in_anothers_report() {
        let (_dir, store) = ready("cores");
        store.write_durable(vec![closed_deal(1, 1_000, "10")]).unwrap();

        let mut reader = Reader::open(&store).unwrap();
        let other = reader.snapshot(|c| period_summary(c, 99, 0, 9_999)).unwrap();
        assert_eq!(other.deals, 0);
    }

    // --- what the core asks at startup ---------------------------------------------------------------

    #[test]
    fn unresolved_orders_are_what_reconciliation_starts_from() {
        // Anything here is an order the venue may or may not know about. Missing one means a
        // position nobody is managing.
        let (_dir, store) = ready("unresolved");
        store.write_durable(vec![order("c-1", "pending", 1)]).unwrap();
        store.write_durable(vec![order("c-2", "new", 2)]).unwrap();
        store.write_durable(vec![order("c-3", "filled", 3)]).unwrap();
        store.write_durable(vec![order("c-4", "cancelled", 4)]).unwrap();

        let mut reader = Reader::open(&store).unwrap();
        let found = reader.snapshot(unresolved_orders).unwrap();
        assert_eq!(found, vec!["c-1", "c-2"], "terminal states must not be reconciled again");
    }

    #[test]
    fn unresolved_orders_come_back_oldest_first() {
        // Reconciliation walks them in the order they were created, so a partial run leaves a
        // prefix done rather than an arbitrary subset.
        let (_dir, store) = ready("order");
        store.write_durable(vec![order("later", "new", 200)]).unwrap();
        store.write_durable(vec![order("earlier", "new", 100)]).unwrap();

        let mut reader = Reader::open(&store).unwrap();
        assert_eq!(reader.snapshot(unresolved_orders).unwrap(), vec!["earlier", "later"]);
    }

    #[test]
    fn a_clean_start_has_nothing_to_reconcile() {
        let (_dir, store) = ready("clean");
        let mut reader = Reader::open(&store).unwrap();
        assert!(reader.snapshot(unresolved_orders).unwrap().is_empty());
    }
}
