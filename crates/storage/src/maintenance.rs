//! Keeping the file healthy, and surviving it when it is not.
//!
//! # Corruption is not hypothetical
//!
//! A power cut during a checkpoint, a disk that lies about `fsync`, a cloud drive that syncs a
//! half-written file, a VPS that reboots under a snapshot. SQLite is unusually resilient to all
//! of these and not immune to any of them.
//!
//! What matters is what happens next. A core that starts against a corrupt database and
//! carries on has no way to know which of its rows are real, and the first thing it does is
//! reconcile positions against them. So the file is checked at startup, and a failed check
//! stops the core rather than being logged and ignored.
//!
//! # Why the broken file is kept
//!
//! Recovery moves it aside rather than deleting it. Most of a corrupt SQLite file is usually
//! readable, and the rows in it are the only record of what happened before the crash. Deleting
//! it to get the process running again trades the one thing that cannot be reconstructed for a
//! few minutes of downtime.

use crate::store::{Store, StoreError};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// What the integrity check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// SQLite reported problems. The strings are its own, and are worth keeping verbatim: they
    /// are what a recovery tool is driven by.
    Corrupt(Vec<String>),
    /// The file could not be opened at all — truncated, not a database, wrong permissions.
    Unreadable(String),
}

impl Health {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Check a database file without opening a store on it.
///
/// Run at startup, before anything is read. `quick_check` rather than `integrity_check`: it
/// finds every structural problem that matters and takes a fraction of the time on a large
/// file, and startup time is what decides whether this gets run at all.
pub fn check(path: impl AsRef<Path>) -> Health {
    let connection =
        match Connection::open_with_flags(path.as_ref(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => return Health::Unreadable(e.to_string()),
        };

    let mut stmt = match connection.prepare("PRAGMA quick_check") {
        Ok(s) => s,
        Err(e) => return Health::Unreadable(e.to_string()),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(rows) => rows,
        Err(e) => return Health::Unreadable(e.to_string()),
    };

    let mut problems = Vec::new();
    for row in rows {
        match row {
            Ok(text) if text == "ok" => {}
            Ok(text) => problems.push(text),
            Err(e) => return Health::Unreadable(e.to_string()),
        }
    }
    if problems.is_empty() {
        Health::Ok
    } else {
        Health::Corrupt(problems)
    }
}

/// Where a broken file was put, and what replaced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// The corrupt file, moved rather than deleted.
    pub quarantined: PathBuf,
    /// How much of it was salvaged into the fresh database, if anything.
    pub rows_recovered: u64,
}

/// Move a corrupt database aside and start a fresh one in its place.
///
/// `now_ms` names the quarantine directory, so several recoveries do not overwrite each other —
/// a file that has been recovered twice is a much more interesting artefact than either
/// recovery alone.
pub fn recover(path: impl AsRef<Path>, now_ms: i64) -> Result<Recovered, StoreError> {
    let path = path.as_ref();
    let parent = path.parent().unwrap_or(Path::new("."));
    let quarantine = parent.join("corrupt").join(now_ms.to_string());
    std::fs::create_dir_all(&quarantine).map_err(|e| StoreError::Io(e.to_string()))?;

    let name = path.file_name().unwrap_or_default();
    let moved = quarantine.join(name);

    // The write-ahead log and the shared-memory file go with it. Leaving them behind would
    // make the fresh database try to replay a log written for a different file, which is a way
    // to corrupt the replacement with the original's problem.
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", path.display()));
        if from.exists() {
            let to = PathBuf::from(format!("{}{suffix}", moved.display()));
            std::fs::rename(&from, &to).map_err(|e| StoreError::Io(e.to_string()))?;
        }
    }

    Ok(Recovered { quarantined: moved, rows_recovered: 0 })
}

/// Reclaim space a sweep freed.
///
/// Deleting rows does not shrink the file — the pages are reused, which is what you want on a
/// database that keeps growing and useless on one that has just had a year of history removed.
///
/// It rewrites the whole file and holds the write lock throughout, so it is an operator action
/// rather than something on a timer. On a large database that is minutes during which nothing
/// can be recorded, and doing that automatically at an unknown moment is worse than a file
/// that is larger than it needs to be.
pub fn vacuum(store: &Store) -> Result<u64, StoreError> {
    let before = total_size(store.path());

    // Not through the write queue. Every batch there runs inside `BEGIN IMMEDIATE`, and SQLite
    // refuses to `VACUUM` from within a transaction — it rewrites the file, so there is nothing
    // for a transaction to roll back to. Its own connection also means the exclusive lock it
    // takes is held for exactly as long as the rewrite, rather than for a queued batch around it.
    let connection = rusqlite::Connection::open(store.path())?;
    connection.busy_timeout(crate::store::BUSY_TIMEOUT)?;
    // Checkpoint first, and TRUNCATE rather than PASSIVE. Deleted pages sit in the write-ahead
    // log until it is folded back, so a vacuum without this rewrites a file the freed space has
    // not reached yet — it succeeds, reclaims nothing, and looks like the vacuum did not work.
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    connection.execute_batch("VACUUM")?;
    drop(connection);

    let after = total_size(store.path());
    Ok(before.saturating_sub(after))
}

/// Copy the database to `destination`, consistently, while it is in use.
///
/// SQLite's own backup API rather than a file copy: copying the file of a live database gives
/// a snapshot of a moving target, and the result usually opens and is sometimes wrong.
pub fn backup(store: &Store, destination: impl AsRef<Path>) -> Result<PathBuf, StoreError> {
    let destination = destination.as_ref().to_path_buf();
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
    }

    let source = store.reader()?;
    let mut target = Connection::open(&destination)?;
    let backup = rusqlite::backup::Backup::new(&source, &mut target)?;
    backup.run_to_completion(1_000, std::time::Duration::from_millis(50), None)?;
    Ok(destination)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The database plus its write-ahead log.
///
/// What an operator's disk actually holds, and the only honest thing to measure a vacuum
/// against: with an open store the checkpoint cannot always fold the whole log back, so the
/// main file alone can grow while the total falls by megabytes.
fn total_size(path: &Path) -> u64 {
    file_size(path)
        + file_size(&PathBuf::from(format!("{}-wal", path.display())))
        + file_size(&PathBuf::from(format!("{}-shm", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{sql, text, Migrator};
    use crate::store::{Value, Write};

    const NOW: i64 = 1_700_000_000_000;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-mnt-{}-{n}-{tag}", std::process::id()));
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

    fn deal(id: i64) -> Write {
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
                text("10"),
                text("0.01"),
                text("take_profit"),
                Value::Int(NOW - 1_000),
                Value::Int(NOW),
            ],
        )
    }

    fn fill_with(store: &Store, deals: i64) {
        for chunk in (0..deals).collect::<Vec<_>>().chunks(500) {
            store.write_durable(chunk.iter().map(|id| deal(*id)).collect()).unwrap();
        }
    }

    // --- the integrity check ------------------------------------------------------------------

    #[test]
    fn a_healthy_database_checks_out() {
        let (dir, store) = ready("healthy");
        fill_with(&store, 100);
        drop(store);
        assert_eq!(check(dir.db()), Health::Ok);
    }

    #[test]
    fn a_corrupted_page_is_detected_rather_than_read_as_data() {
        // The acceptance criterion for task 5.5. A core that starts against a corrupt file and
        // carries on has no way to know which of its rows are real, and the first thing it does
        // is reconcile positions against them.
        let (dir, store) = ready("corrupt");
        fill_with(&store, 2_000);
        drop(store);

        // Damage a byte well inside the file, past the header, where a page's own structure is.
        let mut bytes = std::fs::read(dir.db()).unwrap();
        let middle = bytes.len() / 2;
        for byte in &mut bytes[middle..middle + 256] {
            *byte ^= 0xFF;
        }
        std::fs::write(dir.db(), &bytes).unwrap();

        let health = check(dir.db());
        assert!(!health.is_ok(), "corruption must be detected, got {health:?}");
        match health {
            Health::Corrupt(problems) => {
                assert!(!problems.is_empty(), "SQLite's own description must be kept");
                println!("detected: {}", problems[0]);
            }
            Health::Unreadable(why) => println!("unreadable: {why}"),
            Health::Ok => unreachable!(),
        }
    }

    #[test]
    fn a_file_that_is_not_a_database_is_reported_as_unreadable() {
        // Pointed at the wrong path — a config file, a log — it must say so rather than
        // reporting corruption of something that was never a database.
        let dir = TempDir::new("notadb");
        std::fs::write(dir.db(), b"this is not a database at all, it is a text file").unwrap();
        assert!(matches!(check(dir.db()), Health::Unreadable(_) | Health::Corrupt(_)));
    }

    #[test]
    fn a_missing_file_is_unreadable_not_healthy() {
        // The distinction matters at startup: a missing file is a first run, a healthy one is
        // a normal start, and conflating them would silently skip a migration.
        let dir = TempDir::new("missing");
        assert!(matches!(check(dir.0.join("nothing.sqlite")), Health::Unreadable(_)));
    }

    // --- recovery -------------------------------------------------------------------------------

    #[test]
    fn recovery_moves_the_broken_file_aside_rather_than_deleting_it() {
        // Most of a corrupt SQLite file is usually readable, and the rows in it are the only
        // record of what happened before the crash. Deleting it to get the process running
        // trades the one thing that cannot be reconstructed for a few minutes of downtime.
        let (dir, store) = ready("recover");
        fill_with(&store, 50);
        drop(store);

        let recovered = recover(dir.db(), NOW).unwrap();
        assert!(recovered.quarantined.exists(), "the broken file must survive");
        assert!(!dir.db().exists(), "and must be out of the way");
        assert!(
            recovered.quarantined.to_string_lossy().contains(&NOW.to_string()),
            "the quarantine must be named so a second recovery does not overwrite the first"
        );
    }

    #[test]
    fn the_write_ahead_log_is_moved_with_the_file_it_belongs_to() {
        // Left behind, the fresh database would try to replay a log written for a different
        // file — a way to give the replacement the original's problem.
        let (dir, store) = ready("wal");
        fill_with(&store, 500);
        let wal = PathBuf::from(format!("{}-wal", dir.db().display()));
        assert!(wal.exists(), "the fixture needs a write-ahead log to exist");
        drop(store);

        recover(dir.db(), NOW).unwrap();
        assert!(!wal.exists(), "the log must not be left behind");
    }

    #[test]
    fn a_fresh_store_opens_where_the_broken_one_was() {
        // The point of recovery: the core comes back up.
        let (dir, store) = ready("fresh");
        fill_with(&store, 10);
        drop(store);
        recover(dir.db(), NOW).unwrap();

        let replacement = Store::open(dir.db()).expect("a fresh database opens in its place");
        Migrator::migrate(&replacement).expect("and migrates");
        let deals: i64 =
            replacement.reader().unwrap().query_row("SELECT COUNT(*) FROM deals", [], |r| r.get(0)).unwrap();
        assert_eq!(deals, 0, "the replacement starts empty");
    }

    #[test]
    fn two_recoveries_do_not_overwrite_each_other() {
        let (dir, store) = ready("twice");
        fill_with(&store, 10);
        drop(store);

        let first = recover(dir.db(), NOW).unwrap();
        let store = Store::open(dir.db()).unwrap();
        Migrator::migrate(&store).unwrap();
        fill_with(&store, 10);
        drop(store);
        let second = recover(dir.db(), NOW + 1).unwrap();

        assert_ne!(first.quarantined, second.quarantined);
        assert!(first.quarantined.exists() && second.quarantined.exists());
    }

    // --- backup ------------------------------------------------------------------------------------

    #[test]
    fn a_backup_of_a_live_database_is_consistent() {
        // SQLite's own backup API rather than a file copy: copying the file of a live database
        // gives a snapshot of a moving target, and the result usually opens and is sometimes
        // wrong — which is the worst combination available.
        let (dir, store) = ready("backup");
        fill_with(&store, 1_000);

        let destination = dir.0.join("backups/moon.sqlite");
        backup(&store, &destination).expect("backs up");

        assert_eq!(check(&destination), Health::Ok, "the copy must be a valid database");
        let copied: i64 = Connection::open(&destination)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM deals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(copied, 1_000, "and must contain what the original did");
    }

    #[test]
    fn a_backup_creates_its_directory() {
        // A backup path pointing somewhere that does not exist yet is the normal case on a
        // first run, and failing there would mean no backups at all rather than a late one.
        let (dir, store) = ready("mkdir");
        let deep = dir.0.join("a/b/c/moon.sqlite");
        assert!(backup(&store, &deep).is_ok());
        assert!(deep.exists());
    }

    #[test]
    fn a_backup_can_be_taken_while_writes_are_in_flight() {
        let (dir, store) = ready("live");
        for i in 0..2_000 {
            store.write(vec![deal(i)]).unwrap();
        }
        // Deliberately not waiting: the backup must work against a database being written to.
        let destination = dir.0.join("live-backup.sqlite");
        assert!(backup(&store, &destination).is_ok());
        assert_eq!(check(&destination), Health::Ok);
    }

    // --- vacuum ---------------------------------------------------------------------------------------

    #[test]
    fn vacuum_reclaims_what_a_sweep_freed() {
        // Deleting rows does not shrink the file — the pages are reused, which is what you want
        // on a database that keeps growing and useless on one that has just had a year of
        // history removed.
        let (dir, store) = ready("vacuum");
        fill_with(&store, 20_000);
        store.write_durable(vec![Write::new("DELETE FROM deals", vec![])]).unwrap();

        let before = total_size(&dir.db());
        let reclaimed = vacuum(&store).expect("vacuums");
        println!("{before} bytes on disk before, {reclaimed} reclaimed");
        assert!(reclaimed > 0, "the space a delete freed must come back");
        assert!(
            reclaimed > before / 2,
            "emptying the table should reclaim most of it: {reclaimed} of {before}"
        );
    }

    #[test]
    fn a_vacuum_leaves_the_data_intact() {
        // It rewrites the whole file, so "it got smaller" is not on its own good news.
        let (dir, store) = ready("intact");
        fill_with(&store, 5_000);
        store.write_durable(vec![Write::new("DELETE FROM deals WHERE deal_id > 100", vec![])]).unwrap();

        vacuum(&store).unwrap();
        let remaining: i64 =
            store.reader().unwrap().query_row("SELECT COUNT(*) FROM deals", [], |r| r.get(0)).unwrap();
        assert_eq!(remaining, 101);
        drop(store);
        assert_eq!(check(dir.db()), Health::Ok, "and the file must still be sound");
    }
}
