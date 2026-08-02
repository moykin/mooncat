//! Throwing away what is no longer worth keeping.
//!
//! # Why anything is deleted at all
//!
//! A minute candle for three hundred instruments is four hundred thousand rows a day. Kept
//! forever that is a database nobody can back up and a `VACUUM` nobody can afford to run. The
//! coarse series are cheap and are what a chart older than a week actually shows, so the fine
//! ones age out and the coarse ones do not.
//!
//! # Why it happens in small batches
//!
//! A single `DELETE` of several million rows holds the write lock for as long as it takes, and
//! for as long as it takes nothing else can be recorded — including a fill. Retention running
//! at three in the morning must not be able to stall an order at three in the morning. So it
//! deletes a few thousand rows at a time and yields between batches, and the longest it can
//! hold the lock is one batch rather than one sweep.

use crate::store::{Store, StoreError, Value, Write};
use rusqlite::Connection;
use std::time::Duration;

/// Rows removed per transaction.
///
/// Small enough that one batch is milliseconds even on a slow disk, large enough that the
/// per-transaction cost does not dominate. The point is the ceiling on lock time, not speed.
pub const BATCH: i64 = 5_000;

/// How long to stand aside between batches, so a writer that is waiting gets the lock.
pub const YIELD_BETWEEN_BATCHES: Duration = Duration::from_millis(5);

/// How long each series is kept.
///
/// Minute bars answer "what happened in the last few days" and nothing else — nobody charts a
/// minute bar from two years ago. Hourly and above are what history is actually read from, and
/// they are small enough that ten years costs little.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub minute_days: i64,
    pub five_minute_days: i64,
    pub hourly_and_above_days: i64,
    pub audit_days: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Self { minute_days: 30, five_minute_days: 15, hourly_and_above_days: 3_650, audit_days: 365 }
    }
}

impl Policy {
    /// Oldest timestamp to keep for a timeframe, given the current time.
    pub fn keep_candles_from(&self, tf_ms: i64, now_ms: i64) -> i64 {
        let days = match tf_ms {
            ..=60_000 => self.minute_days,
            60_001..=3_599_999 => self.five_minute_days,
            _ => self.hourly_and_above_days,
        };
        now_ms - days * 86_400_000
    }

    pub fn keep_audit_from(&self, now_ms: i64) -> i64 {
        now_ms - self.audit_days * 86_400_000
    }
}

/// What one sweep removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub candles: i64,
    pub audit: i64,
    pub batches: i64,
}

/// Delete everything past its retention, a batch at a time.
///
/// `now_ms` is passed in rather than read from the clock so a test can age a database without
/// waiting a month, and so a sweep is reproducible.
pub fn sweep(store: &Store, policy: &Policy, now_ms: i64) -> Result<Swept, StoreError> {
    let mut swept = Swept::default();

    // Timeframes are swept separately because each has its own horizon, and one `DELETE` with
    // a `CASE` over them would be a longer statement holding the lock for longer.
    let timeframes = distinct_timeframes(store)?;
    for tf_ms in timeframes {
        let cutoff = policy.keep_candles_from(tf_ms, now_ms);
        loop {
            let removed = delete_batch(
                store,
                "DELETE FROM candles WHERE rowid IN \
                 (SELECT rowid FROM candles WHERE tf_ms = ?1 AND open_time < ?2 LIMIT ?3)",
                vec![Value::Int(tf_ms), Value::Int(cutoff), Value::Int(BATCH)],
            )?;
            swept.candles += removed;
            swept.batches += 1;
            if removed < BATCH {
                break;
            }
            std::thread::sleep(YIELD_BETWEEN_BATCHES);
        }
    }

    let audit_cutoff = policy.keep_audit_from(now_ms);
    loop {
        let removed = delete_batch(
            store,
            "DELETE FROM audit WHERE rec_id IN \
             (SELECT rec_id FROM audit WHERE at_ms < ?1 LIMIT ?2)",
            vec![Value::Int(audit_cutoff), Value::Int(BATCH)],
        )?;
        swept.audit += removed;
        swept.batches += 1;
        if removed < BATCH {
            break;
        }
        std::thread::sleep(YIELD_BETWEEN_BATCHES);
    }

    Ok(swept)
}

/// One batch, and how many rows it removed.
///
/// Counted by reading the table rather than trusting a return value, because the write goes
/// through the queue and the writer does not report row counts back.
fn delete_batch(store: &Store, sql: &'static str, params: Vec<Value>) -> Result<i64, StoreError> {
    let before = total_rows(store, sql)?;
    store.write_durable(vec![Write::new(sql, params)])?;
    let after = total_rows(store, sql)?;
    Ok((before - after).max(0))
}

fn total_rows(store: &Store, sql: &str) -> Result<i64, StoreError> {
    let table = if sql.contains("FROM candles") { "candles" } else { "audit" };
    let connection = store.reader()?;
    Ok(connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
}

fn distinct_timeframes(store: &Store) -> Result<Vec<i64>, StoreError> {
    let connection = store.reader()?;
    let mut stmt = connection.prepare("SELECT DISTINCT tf_ms FROM candles ORDER BY tf_ms")?;
    let rows: Result<Vec<i64>, _> = stmt.query_map([], |r| r.get(0))?.collect();
    Ok(rows?)
}

/// Rows that would be removed by a sweep, without removing them.
///
/// For a dry run before enabling retention on a database that has been accumulating for a
/// year: deleting a lot of history is not something to discover after the fact.
pub fn would_sweep(connection: &Connection, policy: &Policy, now_ms: i64) -> Result<Swept, rusqlite::Error> {
    let mut swept = Swept::default();

    let mut stmt = connection.prepare("SELECT DISTINCT tf_ms FROM candles")?;
    let timeframes: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for tf_ms in timeframes {
        let cutoff = policy.keep_candles_from(tf_ms, now_ms);
        swept.candles += connection.query_row(
            "SELECT COUNT(*) FROM candles WHERE tf_ms = ?1 AND open_time < ?2",
            [tf_ms, cutoff],
            |r| r.get::<_, i64>(0),
        )?;
    }
    swept.audit = connection.query_row(
        "SELECT COUNT(*) FROM audit WHERE at_ms < ?1",
        [policy.keep_audit_from(now_ms)],
        |r| r.get::<_, i64>(0),
    )?;
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{sql, text, Migrator};
    use std::path::PathBuf;
    use std::time::Instant;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_700_000_000_000;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-ret-{}-{n}-{tag}", std::process::id()));
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

    fn candles(store: &Store, tf_ms: i64, count: i64, oldest_ms: i64, step_ms: i64) {
        let writes: Vec<Write> = (0..count)
            .map(|i| {
                Write::new(
                    sql::UPSERT_CANDLE,
                    vec![
                        text("binance"),
                        text("linear_perp"),
                        text("BTCUSDT"),
                        Value::Int(tf_ms),
                        Value::Int(oldest_ms + i * step_ms),
                        text("1"),
                        text("2"),
                        text("0.5"),
                        text("1.5"),
                        text("10"),
                        Value::Int(1),
                    ],
                )
            })
            .collect();
        for chunk in writes.chunks(500) {
            store.write_durable(chunk.iter().map(clone_write).collect()).unwrap();
        }
    }

    fn clone_write(w: &Write) -> Write {
        Write { sql: w.sql.clone(), params: w.params.clone() }
    }

    fn audit_rows(store: &Store, count: i64, oldest_ms: i64, step_ms: i64) {
        for chunk_start in (0..count).step_by(500) {
            let writes: Vec<Write> = (chunk_start..(chunk_start + 500).min(count))
                .map(|i| {
                    Write::new(
                        sql::INSERT_AUDIT,
                        vec![
                            Value::Int(oldest_ms + i * step_ms),
                            Value::Int(1),
                            text("device"),
                            text("127.0.0.1"),
                            text("trader"),
                            Value::Int(i),
                            text("place_order"),
                            Value::Blob(vec![1]),
                            text("succeeded"),
                            Value::Null,
                        ],
                    )
                })
                .collect();
            store.write_durable(writes).unwrap();
        }
    }

    fn count(store: &Store, table: &str) -> i64 {
        store.reader().unwrap().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    // --- the policy ---------------------------------------------------------------------------

    #[test]
    fn each_timeframe_has_its_own_horizon() {
        // Nobody charts a minute bar from two years ago, and hourly bars are small enough that
        // ten years costs little. Getting this the wrong way round would throw away the data
        // that is actually read and keep the data that is not.
        let policy = Policy::default();
        assert_eq!(policy.keep_candles_from(60_000, NOW), NOW - 30 * DAY);
        assert_eq!(policy.keep_candles_from(300_000, NOW), NOW - 15 * DAY);
        assert_eq!(policy.keep_candles_from(3_600_000, NOW), NOW - 3_650 * DAY);
        assert_eq!(policy.keep_candles_from(86_400_000, NOW), NOW - 3_650 * DAY);
    }

    #[test]
    fn a_second_bar_is_treated_as_finely_as_a_minute_bar() {
        // Anything finer than a minute is at least as expensive to keep, so it must not fall
        // through to the ten-year branch.
        assert_eq!(Policy::default().keep_candles_from(1_000, NOW), NOW - 30 * DAY);
    }

    // --- sweeping ------------------------------------------------------------------------------

    #[test]
    fn old_minute_candles_go_and_recent_ones_stay() {
        // Sixty daily-spaced bars ending at now, so the horizon falls exactly halfway.
        let (_dir, store) = ready("minutes");
        candles(&store, 60_000, 60, NOW - 60 * DAY, DAY);

        let swept = sweep(&store, &Policy::default(), NOW).unwrap();
        assert_eq!(swept.candles, 30, "everything older than thirty days goes");
        assert_eq!(count(&store, "candles"), 30, "and everything newer stays");
    }

    #[test]
    fn hourly_candles_are_not_touched_by_a_sweep_that_removes_minute_ones() {
        // The two live in the same table, and a sweep that ignored the timeframe would take
        // the history a chart older than a month depends on.
        let (_dir, store) = ready("mixed");
        candles(&store, 60_000, 60, NOW - 60 * DAY, DAY);
        candles(&store, 3_600_000, 60, NOW - 60 * DAY, DAY);

        sweep(&store, &Policy::default(), NOW).unwrap();
        let hourly: i64 = store
            .reader()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM candles WHERE tf_ms = 3600000", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hourly, 60, "hourly history must survive");
    }

    #[test]
    fn the_audit_trail_is_kept_for_a_year() {
        let (_dir, store) = ready("audit");
        audit_rows(&store, 800, NOW - 800 * DAY, DAY);

        sweep(&store, &Policy::default(), NOW).unwrap();
        assert_eq!(count(&store, "audit"), 365, "a year of it stays");
    }

    #[test]
    fn a_sweep_with_nothing_to_do_is_cheap_and_reports_nothing() {
        // It runs on a timer, and almost every run has nothing to remove.
        let (_dir, store) = ready("noop");
        candles(&store, 60_000, 10, NOW - DAY, 60_000);

        let swept = sweep(&store, &Policy::default(), NOW).unwrap();
        assert_eq!(swept.candles, 0);
        assert_eq!(count(&store, "candles"), 10);
    }

    // --- the lock-time property ---------------------------------------------------------------------

    #[test]
    fn deleting_a_lot_never_holds_the_lock_for_a_whole_sweep() {
        // The acceptance property for task 5.3. Retention running at three in the morning must
        // not be able to stall a fill at three in the morning, so what is bounded is the time
        // of one batch rather than the time of a sweep.
        let (_dir, store) = ready("batched");
        candles(&store, 60_000, 12_000, NOW - 400 * DAY, 60_000);

        let started = Instant::now();
        let swept = sweep(&store, &Policy::default(), NOW).unwrap();
        let elapsed = started.elapsed();

        assert!(swept.candles >= 12_000, "everything past the horizon must go");
        assert!(swept.batches >= 3, "12 000 rows must take several batches, got {}", swept.batches);
        println!("{} rows in {} batches, {elapsed:?}", swept.candles, swept.batches);
    }

    #[test]
    fn a_write_can_get_through_between_batches() {
        // The reason for the pause: a producer waiting on the lock has to be able to take it.
        let (_dir, store) = ready("interleaved");
        candles(&store, 60_000, 11_000, NOW - 400 * DAY, 60_000);

        let writer = std::thread::spawn({
            // Cannot share `Store` across threads by reference here, so a second handle to the
            // same file is used — which is also what the real writer contends with.
            let path = _dir.db();
            move || {
                let other = Store::open(path).expect("second handle");
                let started = Instant::now();
                other
                    .write_durable(vec![Write::new(
                        sql::INSERT_AUDIT,
                        vec![
                            Value::Int(NOW),
                            Value::Int(1),
                            text("d"),
                            text("ip"),
                            text("trader"),
                            Value::Int(1),
                            text("place_order"),
                            Value::Blob(vec![1]),
                            text("succeeded"),
                            Value::Null,
                        ],
                    )])
                    .expect("the write must get through");
                started.elapsed()
            }
        });

        sweep(&store, &Policy::default(), NOW).unwrap();
        let waited = writer.join().expect("the writer thread must not panic");
        println!("a competing write waited {waited:?}");
        assert!(waited < Duration::from_secs(3), "a write waited {waited:?} behind retention");
    }

    // --- the dry run ---------------------------------------------------------------------------------

    #[test]
    fn a_dry_run_counts_without_deleting() {
        // Enabling retention on a database that has been accumulating for a year should not be
        // the first time anyone learns how much it will remove.
        let (_dir, store) = ready("dry");
        candles(&store, 60_000, 100, NOW - 60 * DAY, DAY);
        let before = count(&store, "candles");

        let connection = store.reader().unwrap();
        let would = would_sweep(&connection, &Policy::default(), NOW).unwrap();
        assert!(would.candles > 0);
        assert_eq!(count(&store, "candles"), before, "a dry run must remove nothing");

        let actual = sweep(&store, &Policy::default(), NOW).unwrap();
        assert_eq!(actual.candles, would.candles, "the estimate must match what happens");
    }

    #[test]
    fn a_custom_policy_is_respected() {
        // An operator with a small disk should be able to keep less without editing the code.
        let (_dir, store) = ready("custom");
        candles(&store, 60_000, 60, NOW - 60 * DAY, DAY);

        let strict = Policy { minute_days: 5, ..Policy::default() };
        sweep(&store, &strict, NOW).unwrap();
        assert_eq!(count(&store, "candles"), 5, "only the last five days survive");
    }
}
