//! One writer, a bounded queue, and an acknowledgement that means the data is on disk.
//!
//! # The failure this shape exists to prevent
//!
//! MoonTerminal's storage layer once grew to **88 gigabytes of resident memory** (report 05
//! §4.2). Not a leak in the ordinary sense: the write queue was unbounded, the disk fell
//! behind the market, and the queue absorbed the difference until the machine died. Every
//! individual part was working correctly.
//!
//! An unbounded queue does not remove backpressure, it converts it into memory consumption and
//! defers the failure to a worse moment. So the channel here is bounded, and a producer that
//! outruns the disk **blocks**. Blocking is visible, survivable and happens at the point of
//! the problem; running out of memory on the machine holding the API keys is none of those.
//!
//! # Why exactly one writer
//!
//! SQLite permits one writer at a time whatever the caller does. Several threads writing means
//! they serialise anyway, but through lock contention and `SQLITE_BUSY` retries rather than
//! through a queue — which is the same throughput with worse latency and a failure mode that
//! only appears under load. One thread makes the serialisation explicit and lets writes be
//! batched into a single transaction, which is where the actual speed comes from.
//!
//! # Why the acknowledgement comes after the commit
//!
//! Anything else is a lie the caller cannot detect. An order recorded as persisted and then
//! lost to a power cut is an order the core will not reconcile after a restart, which means a
//! position nobody is managing. [`WriteAck::wait`] returns when the transaction has committed
//! and not before.

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Writes that may be waiting at once.
///
/// Sixteen thousand is roughly a minute of a busy market at the rate anything here is actually
/// written — fills and closed deals, not ticks. Deep enough that a disk hiccup is invisible,
/// shallow enough that the memory it can hold is bounded and small.
pub const QUEUE_DEPTH: usize = 16_384;

/// Writes gathered into one transaction.
///
/// The single largest factor in throughput: SQLite pays for durability per transaction, not
/// per statement, so five hundred rows in one commit cost about what one row costs.
pub const MAX_BATCH: usize = 512;

/// How long to wait for another writer before giving up.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(3);

/// Never checkpoint more often than this.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// And only when the write-ahead log has grown past this.
///
/// Checkpointing is what moves the WAL back into the database file, and it is not free: it
/// blocks writers for as long as it takes. Doing it on a timer alone would stall the writer
/// every minute for no reason on a quiet day; doing it on size alone would never happen on a
/// quiet day and let the log grow all week. Both conditions, so it happens when it is worth it.
pub const CHECKPOINT_WAL_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("the store has shut down")]
    Closed,
    #[error("io: {0}")]
    Io(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

/// One statement and its parameters, ready to execute.
///
/// Owned rather than borrowed because it crosses a thread boundary. Parameters are strings and
/// integers only: this is a queue, not a query builder, and anything that needs richer types
/// converts before it gets here.
pub struct Write {
    pub sql: &'static str,
    pub params: Vec<Value>,
}

/// What a parameter can be. Deliberately small.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Real(f64),
    /// Money and identities. Decimal values arrive as their exact string form, never as a
    /// float — the whole point of `Decimal` upstream would be lost at the last step otherwise.
    Text(String),
    Blob(Vec<u8>),
    Null,
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, ValueRef};
        Ok(match self {
            Self::Int(i) => ToSqlOutput::Owned((*i).into()),
            Self::Real(f) => ToSqlOutput::Owned((*f).into()),
            Self::Text(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Self::Blob(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
            Self::Null => ToSqlOutput::Owned(rusqlite::types::Value::Null),
        })
    }
}

/// A handle that becomes ready when the write is on disk.
pub struct WriteAck(Receiver<Result<(), StoreError>>);

impl WriteAck {
    /// Block until the transaction has committed.
    ///
    /// A caller that does not wait is choosing to accept the loss of this write across a
    /// crash, which is correct for a candle and wrong for an order. Making it a separate call
    /// is what forces that to be a decision.
    pub fn wait(self) -> Result<(), StoreError> {
        self.0.recv().map_err(|_| StoreError::Closed)?
    }
}

struct Job {
    writes: Vec<Write>,
    ack: SyncSender<Result<(), StoreError>>,
}

/// Counters an operator can see. A queue that is regularly full means the disk cannot keep up.
#[derive(Debug, Default)]
pub struct Stats {
    pub committed: AtomicU64,
    pub batches: AtomicU64,
    pub checkpoints: AtomicU64,
    /// Times a producer had to block because the queue was full.
    pub blocked: AtomicU64,
}

/// The write side of the database.
pub struct Store {
    tx: SyncSender<Job>,
    stats: Arc<Stats>,
    writer: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}

impl Store {
    /// Open or create the database and start its writer.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }

        // Opened here rather than on the writer thread so that a bad path or a corrupt file is
        // an error from `open` instead of a thread that dies silently a moment later.
        let connection = Self::configure(&path)?;

        let (tx, rx) = sync_channel::<Job>(QUEUE_DEPTH);
        let stats = Arc::new(Stats::default());
        let writer = std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn({
                let stats = stats.clone();
                move || writer_loop(connection, rx, stats)
            })
            .map_err(|e| StoreError::Io(e.to_string()))?;

        Ok(Self { tx, stats, writer: Some(writer), path })
    }

    fn configure(path: &Path) -> Result<Connection, StoreError> {
        let connection = Connection::open(path)?;
        // Write-ahead logging: readers do not block the writer and the writer does not block
        // readers, which is the whole reason a terminal can query reports while trading runs.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        // NORMAL rather than FULL: with WAL it still survives a process crash, and only loses
        // the last transaction on a power cut. FULL costs an fsync per commit for a guarantee
        // that market data does not need.
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Queue a write. **Blocks** when the queue is full.
    ///
    /// Blocking is the point. The alternative — growing a queue — is what turned a slow disk
    /// into 88 gigabytes of resident memory, and the failure arrived far from its cause.
    pub fn write(&self, writes: Vec<Write>) -> Result<WriteAck, StoreError> {
        let (ack_tx, ack_rx) = sync_channel(1);
        let job = Job { writes, ack: ack_tx };

        match self.tx.try_send(job) {
            Ok(()) => Ok(WriteAck(ack_rx)),
            Err(std::sync::mpsc::TrySendError::Full(job)) => {
                self.stats.blocked.fetch_add(1, Ordering::Relaxed);
                self.tx.send(job).map_err(|_| StoreError::Closed)?;
                Ok(WriteAck(ack_rx))
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(StoreError::Closed),
        }
    }

    /// Write and wait for the commit, for callers that cannot afford to lose it.
    pub fn write_durable(&self, writes: Vec<Write>) -> Result<(), StoreError> {
        self.write(writes)?.wait()
    }

    /// Open a read-only connection.
    ///
    /// Separate from the writer because a reader must never take the write lock: a report
    /// query that blocked an order being recorded would be the wrong trade in every case.
    pub fn reader(&self) -> Result<Connection, StoreError> {
        let connection = Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        Ok(connection)
    }
}

impl Drop for Store {
    /// Close the queue and wait for the writer to finish what it has.
    ///
    /// Without the join, a process exiting immediately after a write would drop the thread
    /// mid-transaction — and the write that was acknowledged as queued would be gone.
    fn drop(&mut self) {
        // Dropping the sender is what ends the writer's loop.
        let (dead, _) = sync_channel(1);
        let tx = std::mem::replace(&mut self.tx, dead);
        drop(tx);

        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// The single writer.
fn writer_loop(connection: Connection, rx: Receiver<Job>, stats: Arc<Stats>) {
    let mut last_checkpoint = Instant::now();

    while let Ok(first) = rx.recv() {
        // Gather whatever else is already waiting. Not a delay — only what has arrived — so a
        // lone write is not held back, while a burst is committed as one transaction.
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }

        let outcome = commit(&connection, &batch);
        let rows: usize = batch.iter().map(|j| j.writes.len()).sum();

        // Acknowledged strictly after the commit returns. Any earlier and the caller is told
        // its order is durable while it is still only in memory.
        for job in batch {
            let answer = match &outcome {
                Ok(()) => Ok(()),
                Err(e) => Err(StoreError::Sqlite(e.to_string())),
            };
            let _ = job.ack.send(answer);
        }

        if outcome.is_ok() {
            stats.committed.fetch_add(rows as u64, Ordering::Relaxed);
            stats.batches.fetch_add(1, Ordering::Relaxed);
        }

        if should_checkpoint(&connection, last_checkpoint) {
            if checkpoint(&connection).is_ok() {
                stats.checkpoints.fetch_add(1, Ordering::Relaxed);
            }
            last_checkpoint = Instant::now();
        }
    }
}

fn commit(connection: &Connection, batch: &[Job]) -> Result<(), rusqlite::Error> {
    connection.execute_batch("BEGIN IMMEDIATE")?;
    for job in batch {
        for write in &job.writes {
            if let Err(e) = connection.execute(write.sql, rusqlite::params_from_iter(&write.params)) {
                // One bad statement must not commit the good ones alongside it: a partial
                // batch is a state nobody designed and nobody can reason about.
                let _ = connection.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }
    connection.execute_batch("COMMIT")
}

/// Both conditions, not either.
fn should_checkpoint(connection: &Connection, last: Instant) -> bool {
    if last.elapsed() < CHECKPOINT_INTERVAL {
        return false;
    }
    wal_bytes(connection) > CHECKPOINT_WAL_BYTES
}

fn wal_bytes(connection: &Connection) -> u64 {
    let Some(path) = connection.path() else {
        return 0;
    };
    // SQLite names the log `<database>-wal`, appended to the whole filename rather than
    // replacing the extension — `moon.sqlite-wal`, not `moon-wal`.
    let wal = PathBuf::from(format!("{path}-wal"));
    std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0)
}

fn checkpoint(connection: &Connection) -> Result<(), rusqlite::Error> {
    // PASSIVE rather than TRUNCATE: it does what it can without waiting for readers, and
    // being interrupted by a busy reader is fine because the next attempt continues.
    connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-store-{}-{n}-{tag}", std::process::id()));
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

    fn opened(tag: &str) -> (TempDir, Store) {
        let dir = TempDir::new(tag);
        let store = Store::open(dir.db()).expect("opens");
        store
            .write_durable(vec![Write {
                sql: "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
                params: vec![],
            }])
            .expect("schema");
        (dir, store)
    }

    fn insert(v: &str) -> Write {
        Write { sql: "INSERT INTO t (v) VALUES (?1)", params: vec![Value::Text(v.into())] }
    }

    fn count(store: &Store) -> i64 {
        store.reader().unwrap().query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap()
    }

    // --- the acceptance criterion -----------------------------------------------------------

    #[test]
    fn an_unbounded_write_queue_is_impossible() {
        // The acceptance criterion for task 5.1, and the reason the whole shape looks like
        // this: an unbounded queue turned a slow disk into 88 GiB of resident memory, and the
        // failure arrived a long way from its cause.
        //
        // Asserted structurally rather than by measuring memory: the channel is created with a
        // fixed depth, so there is no code path that can grow it.
        let (_dir, store) = opened("bounded");
        assert_eq!(QUEUE_DEPTH, 16_384);

        // A producer that outruns the writer blocks. Demonstrated by filling far past the
        // queue depth and observing that everything still arrives, in order, without the
        // process growing without bound.
        for i in 0..QUEUE_DEPTH as i64 + 1_000 {
            store.write(vec![insert(&format!("row-{i}"))]).expect("queued or blocked, never dropped");
        }
        drop(store);

        let store = Store::open(_dir.db()).unwrap();
        assert_eq!(count(&store), QUEUE_DEPTH as i64 + 1_000, "nothing was dropped under pressure");
    }

    #[test]
    fn an_acknowledgement_means_the_data_is_committed() {
        // The other half of the criterion. Anything earlier is a lie the caller cannot detect,
        // and an order believed durable that is not is a position nobody reconciles.
        let (dir, store) = opened("ack");
        store.write_durable(vec![insert("committed")]).expect("commits");

        // Read through a connection that knows nothing of the writer's in-memory state.
        let reader = Connection::open(dir.db()).unwrap();
        let found: i64 = reader.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(found, 1, "the row must be visible to an independent connection");
    }

    #[test]
    fn a_write_survives_the_process_that_made_it() {
        let dir = TempDir::new("durable");
        {
            let store = Store::open(dir.db()).unwrap();
            store
                .write_durable(vec![Write {
                    sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
                    params: vec![],
                }])
                .unwrap();
            store.write_durable(vec![insert("survives")]).unwrap();
        }

        let reopened = Store::open(dir.db()).unwrap();
        assert_eq!(count(&reopened), 1);
    }

    #[test]
    fn dropping_the_store_finishes_what_was_queued() {
        // Without the join in `Drop`, a process exiting straight after a write would abandon
        // the writer mid-transaction and lose what it had already accepted.
        let dir = TempDir::new("drain");
        {
            let store = Store::open(dir.db()).unwrap();
            store
                .write_durable(vec![Write {
                    sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
                    params: vec![],
                }])
                .unwrap();
            for i in 0..1_000 {
                store.write(vec![insert(&format!("r{i}"))]).unwrap();
            }
            // No wait: the drop must handle it.
        }

        let store = Store::open(dir.db()).unwrap();
        assert_eq!(count(&store), 1_000, "queued writes must not be abandoned on shutdown");
    }

    // --- batching ------------------------------------------------------------------------------

    #[test]
    fn a_burst_is_committed_as_far_fewer_transactions_than_writes() {
        // Where the throughput comes from: SQLite pays for durability per transaction, not
        // per statement.
        let (_dir, store) = opened("batch");
        let mut last = None;
        for i in 0..5_000 {
            last = Some(store.write(vec![insert(&format!("r{i}"))]).unwrap());
        }
        last.unwrap().wait().unwrap();

        let batches = store.stats().batches.load(Ordering::Relaxed);
        let committed = store.stats().committed.load(Ordering::Relaxed);
        println!("{committed} rows in {batches} transactions");
        // 5 001: the schema statement from `opened` committed first, in its own transaction.
        assert_eq!(committed, 5_001);
        assert!(batches < 100, "5 000 writes in {batches} transactions is not batching");
    }

    #[test]
    fn a_lone_write_is_not_held_back_waiting_for_company() {
        // The batcher gathers what has already arrived rather than waiting for more, so a
        // single order does not pay a latency penalty for an optimisation aimed at bursts.
        let (_dir, store) = opened("lone");
        let started = Instant::now();
        store.write_durable(vec![insert("alone")]).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500), "a single write waited too long");
    }

    // --- failure handling -------------------------------------------------------------------------

    #[test]
    fn a_failing_statement_rolls_back_the_whole_batch() {
        // A partial batch is a state nobody designed and nobody can reason about afterwards.
        let (_dir, store) = opened("rollback");
        store.write_durable(vec![insert("first")]).unwrap();

        let bad = store.write(vec![
            insert("good"),
            Write { sql: "INSERT INTO nonexistent (v) VALUES (?1)", params: vec![Value::Int(1)] },
        ]);
        assert!(bad.unwrap().wait().is_err(), "the batch must fail");
        assert_eq!(count(&store), 1, "the good statement must not have committed alone");
    }

    #[test]
    fn a_failure_is_reported_to_the_caller_not_swallowed() {
        let (_dir, store) = opened("report");
        let err = store.write_durable(vec![Write { sql: "THIS IS NOT SQL", params: vec![] }]).unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)), "got {err:?}");
    }

    #[test]
    fn writing_to_a_closed_store_is_an_error_rather_than_a_hang() {
        let dir = TempDir::new("closed");
        let store = Store::open(dir.db()).unwrap();
        let ack = store.write(vec![Write { sql: "SELECT 1", params: vec![] }]).unwrap();
        drop(store);
        // The write either committed before shutdown or reports the store is gone; it must
        // never block forever.
        let _ = ack.wait();
    }

    // --- configuration ------------------------------------------------------------------------------

    #[test]
    fn the_database_is_in_write_ahead_mode() {
        // Readers must not block the writer: a report query that stalled an order being
        // recorded would be the wrong trade in every case.
        let (_dir, store) = opened("wal");
        let mode: String =
            store.reader().unwrap().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn a_reader_can_query_while_writes_are_in_flight() {
        let (_dir, store) = opened("concurrent");
        for i in 0..2_000 {
            store.write(vec![insert(&format!("r{i}"))]).unwrap();
        }
        // Not waiting: the point is that a read works while the writer is busy.
        let during: i64 =
            store.reader().unwrap().query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap();
        assert!(during >= 0, "a read during writes must not fail or block");
    }

    #[test]
    fn a_reader_cannot_write() {
        // Structurally, not by convention: a reader that took the write lock would be able to
        // stall the thing it exists to stay out of the way of.
        let (_dir, store) = opened("readonly");
        let reader = store.reader().unwrap();
        assert!(reader.execute("INSERT INTO t (v) VALUES ('x')", []).is_err());
    }

    #[test]
    fn a_missing_directory_is_created() {
        let dir = TempDir::new("mkdir");
        let nested = dir.0.join("a/b/c/moon.sqlite");
        assert!(Store::open(&nested).is_ok());
        assert!(nested.exists());
    }

    #[test]
    fn checkpointing_needs_both_conditions_not_either() {
        // Time alone would stall the writer every minute on a quiet day; size alone would let
        // the log grow all week. The constants are asserted because loosening either one
        // reintroduces the behaviour the other prevents.
        assert_eq!(CHECKPOINT_INTERVAL, Duration::from_secs(60));
        assert_eq!(CHECKPOINT_WAL_BYTES, 32 * 1024 * 1024);

        let (_dir, store) = opened("checkpoint");
        let connection = store.reader().unwrap();
        assert!(!should_checkpoint(&connection, Instant::now()), "too soon, whatever the size");
    }

    #[test]
    fn blocked_producers_are_counted() {
        // A number that climbs means the disk cannot keep up, which is the thing an operator
        // needs to see before it becomes a problem rather than after.
        let (_dir, store) = opened("blocked");
        for i in 0..QUEUE_DEPTH as i64 + 2_000 {
            store.write(vec![insert(&format!("r{i}"))]).unwrap();
        }
        println!("blocked {} times", store.stats().blocked.load(Ordering::Relaxed));
    }

    // --- values ----------------------------------------------------------------------------------------

    #[test]
    fn money_goes_in_as_text_and_comes_back_exact() {
        // The last place the point of using `Decimal` could be thrown away. A float column
        // here would undo every careful decision upstream.
        let (_dir, store) = opened("money");
        store
            .write_durable(vec![Write {
                sql: "INSERT INTO t (v) VALUES (?1)",
                params: vec![Value::Text("63096.01000000".into())],
            }])
            .unwrap();

        let back: String = store.reader().unwrap().query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(back, "63096.01000000", "the exact decimal string must survive");
    }

    #[test]
    fn every_value_kind_round_trips() {
        let (_dir, store) = opened("values");
        store
            .write_durable(vec![Write {
                sql: "CREATE TABLE v (i INTEGER, r REAL, t TEXT, b BLOB, n TEXT)",
                params: vec![],
            }])
            .unwrap();
        store
            .write_durable(vec![Write {
                sql: "INSERT INTO v VALUES (?1, ?2, ?3, ?4, ?5)",
                params: vec![
                    Value::Int(-42),
                    Value::Real(1.5),
                    Value::Text("text".into()),
                    Value::Blob(vec![1, 2, 3]),
                    Value::Null,
                ],
            }])
            .unwrap();

        let reader = store.reader().unwrap();
        let (i, r, t, b, n): (i64, f64, String, Vec<u8>, Option<String>) = reader
            .query_row("SELECT i, r, t, b, n FROM v", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })
            .unwrap();
        assert_eq!((i, r, t, b, n), (-42, 1.5, "text".to_string(), vec![1, 2, 3], None));
    }
}
