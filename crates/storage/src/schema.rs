//! The tables, and how they change over time.
//!
//! # Migrations are forward-only and numbered
//!
//! Each one is applied once, in order, and `PRAGMA user_version` records how far the file has
//! got. There is no down-migration: reversing a schema change on a database that has been
//! written to since is a data-loss operation dressed as a convenience, and the honest way back
//! is the backup taken before the upgrade. [`Migrator::migrate`] refuses to run against a file
//! from a *newer* build for the same reason — an older binary cannot know what the newer
//! schema means.
//!
//! # Why the constraints are in the database and not in the code
//!
//! A rule enforced only in Rust holds until someone writes a row from a shell. The ones here
//! are the ones whose violation is silent and expensive:
//!
//! * **A closed deal must have a closing time.** MoonBot allowed undated closed deals, and the
//!   consequence was that they fell out of every period report without appearing anywhere as
//!   missing — the numbers were simply wrong and nothing said so (`10-target-architecture.md`
//!   difference 12). Here it is a `CHECK`, so the row cannot exist.
//! * **A fill belongs to an order.** A foreign key, so an execution cannot be recorded against
//!   something that was never placed.
//! * **Money is text.** A `REAL` column would undo every decision made upstream about decimals
//!   at the last possible moment.

use crate::store::{Store, StoreError, Value, Write};

/// One step. Numbered from one; the version in the file is the highest applied.
struct Migration {
    version: i64,
    what: &'static str,
    sql: &'static str,
}

/// Every migration this build knows, in order.
///
/// Append only. Editing one that has shipped means two databases with the same
/// `user_version` and different shapes, which is worse than any schema mistake it could fix.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    what: "initial schema",
    sql: r#"
    -- A trade from open to close: the thing a report row describes. Distinct from an order,
    -- because one deal is usually several orders — an entry, a stop, a take-profit — and the
    -- number a trader cares about belongs to the cycle rather than to any one of them.
    CREATE TABLE deals (
        deal_id       INTEGER PRIMARY KEY,
        core_uid      INTEGER NOT NULL,
        exchange      TEXT    NOT NULL,
        market        TEXT    NOT NULL,
        symbol        TEXT    NOT NULL,
        side          TEXT    NOT NULL CHECK (side IN ('buy','sell')),
        phase         TEXT    NOT NULL,
        qty           TEXT    NOT NULL,
        entry_price   TEXT,
        exit_price    TEXT,
        realized      TEXT,
        fee           TEXT    NOT NULL DEFAULT '0',
        close_reason  TEXT,
        opened_at     INTEGER NOT NULL,
        -- The constraint that difference 12 exists for: a closed deal with no closing time
        -- silently vanishes from every period report.
        closed_at     INTEGER,
        CHECK (phase <> 'closed' OR closed_at IS NOT NULL),
        CHECK (phase <> 'closed' OR realized IS NOT NULL)
    );

    -- What was actually sent to the venue.
    CREATE TABLE orders (
        client_id     TEXT    PRIMARY KEY,
        deal_id       INTEGER REFERENCES deals(deal_id),
        venue_id      TEXT,
        core_uid      INTEGER NOT NULL,
        exchange      TEXT    NOT NULL,
        market        TEXT    NOT NULL,
        symbol        TEXT    NOT NULL,
        side          TEXT    NOT NULL CHECK (side IN ('buy','sell')),
        order_type    TEXT    NOT NULL,
        status        TEXT    NOT NULL,
        qty           TEXT    NOT NULL,
        filled_qty    TEXT    NOT NULL DEFAULT '0',
        price         TEXT,
        trigger_price TEXT,
        avg_price     TEXT    NOT NULL DEFAULT '0',
        reduce_only   INTEGER NOT NULL DEFAULT 0,
        -- Written before the order is sent, so a crash in between leaves it findable.
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL,
        -- Optimistic lock. Two terminals editing one order is the normal case.
        rev           INTEGER NOT NULL DEFAULT 0
    );

    -- Executions. A separate table because one order fills many times and the average price
    -- has to be reconstructible rather than trusted.
    CREATE TABLE fills (
        trade_id      TEXT    NOT NULL,
        client_id     TEXT    NOT NULL REFERENCES orders(client_id),
        symbol        TEXT    NOT NULL,
        side          TEXT    NOT NULL CHECK (side IN ('buy','sell')),
        price         TEXT    NOT NULL,
        qty           TEXT    NOT NULL,
        fee           TEXT    NOT NULL,
        fee_asset     TEXT    NOT NULL,
        is_maker      INTEGER NOT NULL,
        ts            INTEGER NOT NULL,
        -- The venue replays executions after a reconnect; this is what makes that harmless.
        PRIMARY KEY (client_id, trade_id)
    );

    CREATE TABLE candles (
        exchange      TEXT    NOT NULL,
        market        TEXT    NOT NULL,
        symbol        TEXT    NOT NULL,
        tf_ms         INTEGER NOT NULL,
        open_time     INTEGER NOT NULL,
        open          TEXT    NOT NULL,
        high          TEXT    NOT NULL,
        low           TEXT    NOT NULL,
        close         TEXT    NOT NULL,
        volume        TEXT    NOT NULL,
        trades        INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (exchange, market, symbol, tf_ms, open_time)
    );

    -- Who did what, from where. Append-only by construction: nothing updates it.
    CREATE TABLE audit (
        rec_id        INTEGER PRIMARY KEY AUTOINCREMENT,
        at_ms         INTEGER NOT NULL,
        session_id    INTEGER NOT NULL,
        device_id     TEXT    NOT NULL,
        peer          TEXT    NOT NULL,
        role          TEXT    NOT NULL,
        req           INTEGER NOT NULL,
        command       TEXT    NOT NULL,
        body          BLOB    NOT NULL,
        outcome       TEXT    NOT NULL,
        detail        TEXT
    );

    -- Indexes for the queries that actually run. Report periods are always bounded by close
    -- time within one core, and a chart always asks for one series over a time range.
    CREATE INDEX deals_by_close  ON deals (core_uid, closed_at);
    CREATE INDEX deals_by_symbol ON deals (symbol, closed_at);
    CREATE INDEX orders_by_deal  ON orders (deal_id);
    CREATE INDEX orders_open     ON orders (status, updated_at);
    CREATE INDEX fills_by_time   ON fills (ts);
    CREATE INDEX audit_by_time   ON audit (at_ms);
    "#,
}];

/// The schema version this build expects.
pub fn expected_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("database is at version {found}, this build only knows up to {known} — it was written by a newer core")]
    FromTheFuture { found: i64, known: i64 },
    #[error("migration {version} ({what}) failed: {why}")]
    Failed { version: i64, what: &'static str, why: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Bring a database up to the schema this build expects.
pub struct Migrator;

impl Migrator {
    /// Apply whatever is missing. Idempotent: running it twice does nothing the second time.
    pub fn migrate(store: &Store) -> Result<i64, MigrationError> {
        let current = Self::version(store)?;
        let known = expected_version();

        // An older binary cannot know what a newer schema means, and guessing would corrupt
        // rather than fail. Refusing is the only safe answer.
        if current > known {
            return Err(MigrationError::FromTheFuture { found: current, known });
        }

        for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
            // The migration and the version bump go in one transaction: a crash between them
            // would leave a file whose shape and recorded version disagree, and every later
            // run would then do the wrong thing.
            let sql = format!("{}\nPRAGMA user_version = {};", migration.sql, migration.version);
            store.write_durable(vec![Write::owned(sql, vec![])]).map_err(|e| MigrationError::Failed {
                version: migration.version,
                what: migration.what,
                why: e.to_string(),
            })?;
            tracing::info!(version = migration.version, what = migration.what, "schema migrated");
        }
        Ok(known)
    }

    /// Version recorded in the file. Zero for an empty one.
    pub fn version(store: &Store) -> Result<i64, StoreError> {
        let connection = store.reader()?;
        Ok(connection.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }
}

/// Statements the rest of the crate builds on, so the column lists live next to the schema
/// rather than being repeated at every call site.
pub mod sql {
    pub const INSERT_DEAL: &str = "INSERT INTO deals \
        (deal_id, core_uid, exchange, market, symbol, side, phase, qty, entry_price, exit_price, \
         realized, fee, close_reason, opened_at, closed_at) \
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)";

    pub const INSERT_ORDER: &str = "INSERT INTO orders \
        (client_id, deal_id, venue_id, core_uid, exchange, market, symbol, side, order_type, \
         status, qty, filled_qty, price, trigger_price, avg_price, reduce_only, created_at, \
         updated_at, rev) \
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)";

    /// Idempotent by primary key: a venue replaying executions after a reconnect must not
    /// double-count them.
    pub const INSERT_FILL: &str = "INSERT OR IGNORE INTO fills \
        (trade_id, client_id, symbol, side, price, qty, fee, fee_asset, is_maker, ts) \
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)";

    /// A candle is rewritten as it closes, so the last write wins.
    pub const UPSERT_CANDLE: &str = "INSERT INTO candles \
        (exchange, market, symbol, tf_ms, open_time, open, high, low, close, volume, trades) \
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) \
        ON CONFLICT (exchange, market, symbol, tf_ms, open_time) DO UPDATE SET \
        high=excluded.high, low=excluded.low, close=excluded.close, \
        volume=excluded.volume, trades=excluded.trades";

    pub const INSERT_AUDIT: &str = "INSERT INTO audit \
        (at_ms, session_id, device_id, peer, role, req, command, body, outcome, detail) \
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)";
}

/// Convenience for the common shape of a text parameter.
pub fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-schema-{}-{n}-{tag}", std::process::id()));
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

    fn migrated(tag: &str) -> (TempDir, Store) {
        let dir = TempDir::new(tag);
        let store = Store::open(dir.db()).expect("opens");
        Migrator::migrate(&store).expect("migrates");
        (dir, store)
    }

    fn deal(phase: &str, closed_at: Value, realized: Value) -> Vec<Write> {
        vec![Write::new(
            sql::INSERT_DEAL,
            vec![
                Value::Int(1),
                Value::Int(0),
                text("binance"),
                text("linear_perp"),
                text("BTCUSDT"),
                text("buy"),
                text(phase),
                text("0.5"),
                text("63000.00"),
                text("63100.00"),
                realized,
                text("0.01"),
                Value::Null,
                Value::Int(1_700_000_000_000),
                closed_at,
            ],
        )]
    }

    fn tables(store: &Store) -> Vec<String> {
        let connection = store.reader().unwrap();
        let mut stmt =
            connection.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").unwrap();
        let rows: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        rows
    }

    // --- the acceptance criterion --------------------------------------------------------

    #[test]
    fn a_closed_deal_without_a_closing_time_cannot_exist() {
        // Difference 12: MoonBot allowed these, and the consequence was that such deals fell
        // out of every period report without appearing anywhere as missing. The numbers were
        // simply wrong and nothing said so. Enforced by the database, not by Rust, because a
        // rule that lives only in code holds until someone writes a row from a shell.
        let (_dir, store) = migrated("closed_at");

        let refused = store.write_durable(deal("closed", Value::Null, text("100")));
        assert!(refused.is_err(), "a closed deal with no closing time must be impossible");

        let accepted = store.write_durable(deal("closed", Value::Int(1_700_000_001_000), text("100")));
        assert!(accepted.is_ok(), "with a closing time it is a normal row");
    }

    #[test]
    fn a_closed_deal_must_also_have_a_realised_figure() {
        // The other half of the same failure: a closed deal with no profit recorded is counted
        // in the trade count and not in the total, so the two disagree and neither is wrong
        // enough to notice.
        let (_dir, store) = migrated("realized");
        assert!(store.write_durable(deal("closed", Value::Int(1), Value::Null)).is_err());
    }

    #[test]
    fn an_open_deal_may_have_neither() {
        // The constraint must not make the ordinary case impossible: a deal that is open has
        // not closed and has realised nothing.
        let (_dir, store) = migrated("open");
        assert!(store.write_durable(deal("open", Value::Null, Value::Null)).is_ok());
    }

    // --- migrations ---------------------------------------------------------------------------

    #[test]
    fn migrating_a_fresh_file_creates_everything_and_records_the_version() {
        let (_dir, store) = migrated("fresh");
        assert_eq!(Migrator::version(&store).unwrap(), expected_version());

        let found = tables(&store);
        for expected in ["deals", "orders", "fills", "candles", "audit"] {
            assert!(found.contains(&expected.to_string()), "{expected} is missing from {found:?}");
        }
    }

    #[test]
    fn migrating_twice_does_nothing_the_second_time() {
        // It runs on every start, so it has to be idempotent — and a second run that tried to
        // create the tables again would fail rather than being a no-op.
        let (_dir, store) = migrated("twice");
        let before = Migrator::version(&store).unwrap();
        assert_eq!(Migrator::migrate(&store).unwrap(), before);
        assert_eq!(Migrator::version(&store).unwrap(), before);
    }

    #[test]
    fn a_file_from_a_newer_build_is_refused_rather_than_guessed_at() {
        // An older binary cannot know what a newer schema means, and guessing corrupts where
        // refusing merely stops. The message names both versions, because the operator's next
        // action is to work out which binary to run.
        let dir = TempDir::new("future");
        let store = Store::open(dir.db()).unwrap();
        store.write_durable(vec![Write::new("PRAGMA user_version = 999", vec![])]).unwrap();

        let err = Migrator::migrate(&store).unwrap_err();
        assert!(matches!(err, MigrationError::FromTheFuture { found: 999, .. }), "got {err:?}");
        assert!(err.to_string().contains("999"), "the message must name the version found");
    }

    #[test]
    fn the_version_and_the_shape_move_together() {
        // Applied in one transaction, so a crash between them cannot leave a file whose
        // recorded version disagrees with its tables — every later run would then do the
        // wrong thing based on a number it had no reason to distrust.
        let (_dir, store) = migrated("atomic");
        assert_eq!(Migrator::version(&store).unwrap(), 1);
        assert!(tables(&store).contains(&"deals".to_string()));
    }

    #[test]
    fn a_fresh_database_reports_version_zero() {
        let dir = TempDir::new("zero");
        let store = Store::open(dir.db()).unwrap();
        assert_eq!(Migrator::version(&store).unwrap(), 0);
    }

    // --- the other constraints -------------------------------------------------------------------

    #[test]
    fn a_fill_cannot_belong_to_an_order_that_was_never_placed() {
        // An execution recorded against nothing is a position with no explanation, and it is
        // exactly what a race between the venue's stream and our own bookkeeping produces.
        let (_dir, store) = migrated("fk");
        let orphan = vec![Write::new(
            sql::INSERT_FILL,
            vec![
                text("t-1"),
                text("no-such-order"),
                text("BTCUSDT"),
                text("buy"),
                text("63000"),
                text("0.1"),
                text("0.01"),
                text("USDT"),
                Value::Int(0),
                Value::Int(1),
            ],
        )];
        assert!(store.write_durable(orphan).is_err(), "foreign keys must be enforced");
    }

    #[test]
    fn a_replayed_execution_is_recorded_once() {
        // Venues resend executions after a reconnect. Without the primary key every reconnect
        // would double the recorded volume.
        let (_dir, store) = migrated("replay");
        store.write_durable(vec![Write::new(sql::INSERT_ORDER, order_params("c-1"))]).unwrap();

        let fill = || {
            Write::new(
                sql::INSERT_FILL,
                vec![
                    text("t-1"),
                    text("c-1"),
                    text("BTCUSDT"),
                    text("buy"),
                    text("63000"),
                    text("0.1"),
                    text("0.01"),
                    text("USDT"),
                    Value::Int(0),
                    Value::Int(1),
                ],
            )
        };
        store.write_durable(vec![fill()]).unwrap();
        store.write_durable(vec![fill()]).unwrap();

        let count: i64 =
            store.reader().unwrap().query_row("SELECT COUNT(*) FROM fills", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "a replayed execution must not be counted twice");
    }

    #[test]
    fn a_candle_is_rewritten_as_it_closes() {
        // A live candle is updated many times before it is final, and each update replaces the
        // previous rather than adding a row.
        let (_dir, store) = migrated("candle");
        let candle = |close: &str| {
            Write::new(
                sql::UPSERT_CANDLE,
                vec![
                    text("binance"),
                    text("linear_perp"),
                    text("BTCUSDT"),
                    Value::Int(60_000),
                    Value::Int(1_700_000_000_000),
                    text("63000"),
                    text("63100"),
                    text("62900"),
                    text(close),
                    text("12.5"),
                    Value::Int(400),
                ],
            )
        };
        store.write_durable(vec![candle("63050")]).unwrap();
        store.write_durable(vec![candle("63075")]).unwrap();

        let connection = store.reader().unwrap();
        let (rows, last): (i64, String) = (
            connection.query_row("SELECT COUNT(*) FROM candles", [], |r| r.get(0)).unwrap(),
            connection.query_row("SELECT close FROM candles", [], |r| r.get(0)).unwrap(),
        );
        assert_eq!((rows, last.as_str()), (1, "63075"));
    }

    #[test]
    fn a_side_outside_the_two_that_exist_is_refused() {
        // Cheap, and it catches the class of bug where an enum is stringified through the
        // wrong formatter and everything downstream quietly filters the rows out.
        let (_dir, store) = migrated("side");
        let mut params = order_params("c-2");
        params[7] = text("Buy");
        assert!(store.write_durable(vec![Write::new(sql::INSERT_ORDER, params)]).is_err());
    }

    #[test]
    fn money_columns_hold_text_so_precision_is_not_lost() {
        // The last place the point of `Decimal` could be thrown away. A REAL column would turn
        // 63096.01 into something that is nearly it.
        let (_dir, store) = migrated("money");
        store.write_durable(vec![Write::new(sql::INSERT_ORDER, order_params("c-3"))]).unwrap();

        let price: String = store
            .reader()
            .unwrap()
            .query_row("SELECT price FROM orders WHERE client_id='c-3'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(price, "63096.01000000", "the exact decimal string must survive");

        let kind: String = store
            .reader()
            .unwrap()
            .query_row("SELECT type FROM pragma_table_info('orders') WHERE name='price'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "TEXT", "a REAL column here would undo every decision upstream");
    }

    #[test]
    fn the_indexes_the_report_queries_need_are_present() {
        // Report periods are always bounded by close time within one core; without the index
        // the query walks the whole table, which report 05 measured at seconds rather than
        // milliseconds.
        // Both halves bound: `migrated(..).1` would drop the TempDir at the end of the
        // statement and delete the database out from under the connection.
        let (_dir, store) = migrated("indexes");
        let connection = store.reader().unwrap();
        let mut stmt = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'")
            .unwrap();
        let found: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();

        for expected in ["deals_by_close", "orders_open", "fills_by_time", "audit_by_time"] {
            assert!(found.contains(&expected.to_string()), "{expected} is missing from {found:?}");
        }
    }

    fn order_params(client_id: &str) -> Vec<Value> {
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
            text("new"),
            text("0.5"),
            text("0"),
            text("63096.01000000"),
            Value::Null,
            text("0"),
            Value::Int(0),
            Value::Int(1_700_000_000_000),
            Value::Int(1_700_000_000_000),
            Value::Int(0),
        ]
    }
}
