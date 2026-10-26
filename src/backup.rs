//! Database backups: consistent snapshots of the database file, rotation of the kept copies,
//! and the schedule that takes them.
//!
//! A backup is `pomb-<UTC YYYYMMDDTHHMMSSZ>.redb` in the backup directory. Rotation and cleanup
//! touch only files whose names match that pattern exactly, so anything else the owner keeps in
//! the directory is left alone.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use std::{fmt, fs, io};

use chrono::NaiveDateTime;
use redb::ReadableDatabase;
use serde::Serialize;
use tokio::sync::{Mutex, OwnedMutexGuard};
use utoipa::ToSchema;

use crate::db::Db;
use crate::models::{BackupSettings, BackupStatus};

/// Name of the backup directory beside the database file.
const DEFAULT_DIR_NAME: &str = "backups";

/// Start of a backup file name.
const FILE_PREFIX: &str = "pomb-";

/// End of a finished backup file name.
const FILE_SUFFIX: &str = ".redb";

/// End of a backup file name while its copy is still being written.
const PART_SUFFIX: &str = ".redb.part";

/// Format of the UTC time in a backup file name.
const STAMP_FORMAT: &str = "%Y%m%dT%H%M%SZ";

/// How often the schedule checks whether a backup is due.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Seconds the schedule waits before it retries a failed backup.
const RETRY_AFTER_FAILURE_SECONDS: i64 = 3600;

/// Proof that a backup is running, which keeps every other backup from starting until it drops.
pub type BackupLock = OwnedMutexGuard<()>;

/// A backup file in the backup directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct BackupFile {
    /// File name, which holds the UTC time of the backup.
    pub name: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Unix time the file was last written, in seconds.
    pub modified_at: i64,
}

/// Why a backup did not start.
#[derive(Debug, PartialEq, Eq)]
pub enum RunError {
    /// Another backup is still running.
    Busy,
    /// The stored settings or status could not be read.
    Settings(String),
    /// The task that runs the backup failed before it finished.
    Task(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("a backup is already running"),
            Self::Settings(error) => write!(f, "could not read the backup settings: {error}"),
            Self::Task(error) => write!(f, "the backup task failed: {error}"),
        }
    }
}

impl std::error::Error for RunError {}

/// The backup directory and the lock that lets one backup run at a time.
#[derive(Clone, Debug)]
pub struct Backups {
    /// Directory the backup files are written to.
    dir: Arc<Path>,
    /// Held by the backup that is running.
    lock: Arc<Mutex<()>>,
}

impl Backups {
    /// Creates backups that go to `dir`, which is created when the first backup runs.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir: Arc::from(dir),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Creates backups that go to a `backups` directory beside the database file at `database`.
    #[must_use]
    pub fn beside(database: &Path) -> Self {
        Self::new(default_dir(database))
    }

    /// Returns the backup directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Takes the lock that one running backup holds, without waiting for it.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Busy`] while another backup holds the lock.
    pub fn hold(&self) -> Result<BackupLock, RunError> {
        Arc::clone(&self.lock)
            .try_lock_owned()
            .map_err(|_| RunError::Busy)
    }

    /// Lists the kept backup files, newest first. A directory that does not exist yet is empty.
    pub fn files(&self) -> io::Result<Vec<BackupFile>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !entry.file_type()?.is_file() || !is_stamped(&name, FILE_SUFFIX) {
                continue;
            }
            let meta = entry.metadata()?;
            files.push(BackupFile {
                name,
                size_bytes: meta.len(),
                modified_at: modified_seconds(&meta),
            });
        }
        files.sort_unstable_by(|a, b| b.name.cmp(&a.name));
        Ok(files)
    }

    /// Takes one backup now, unless another is running.
    ///
    /// A failure to write the backup is not an error here. It comes back as a [`BackupStatus`]
    /// with `ok` false, which is also stored and logged, and no old backup is deleted.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Busy`] while another backup runs, [`RunError::Settings`] if the
    /// settings cannot be read, and [`RunError::Task`] if the backup task fails.
    pub async fn run(&self, db: &Db, now: i64) -> Result<BackupStatus, RunError> {
        self.launch(db, move |backups, db| {
            let (settings, _) = read_state(db)?;
            Ok(backups.write(db, &settings, now))
        })
        .await
    }

    /// Takes a backup if the schedule says one is due at `now`, and returns its outcome.
    ///
    /// The schedule is checked again once the lock is held, so a check that waited behind
    /// another backup does not take a second one.
    pub async fn tick(&self, db: &Db, now: i64) -> Option<BackupStatus> {
        let outcome = self
            .launch(db, move |backups, db| {
                let (settings, status) = read_state(db)?;
                Ok(is_due(&settings, status.as_ref(), now)
                    .then(|| backups.write(db, &settings, now)))
            })
            .await;
        match outcome {
            Ok(status) => status,
            Err(RunError::Busy) => None,
            Err(error) => {
                eprintln!("pomb: backup schedule failed: {error}");
                None
            }
        }
    }

    /// Runs `work` on a blocking thread while holding the lock.
    async fn launch<T: Send + 'static>(
        &self,
        db: &Db,
        work: impl FnOnce(&Self, &Db) -> Result<T, RunError> + Send + 'static,
    ) -> Result<T, RunError> {
        let held = self.hold()?;
        let backups = self.clone();
        let db = db.clone();
        tokio::task::spawn_blocking(move || {
            let _held = held;
            work(&backups, &db)
        })
        .await
        .map_err(|error| RunError::Task(error.to_string()))?
    }

    /// Takes a backup, rotates the kept files after a success, stores the outcome and returns it.
    fn write(&self, db: &Db, settings: &BackupSettings, now: i64) -> BackupStatus {
        self.remove_stale_parts(db.path(), settings, now);
        let previous = db.backup_status().ok().flatten();
        let status = match self.copy(db, now) {
            Ok((file, size)) => {
                println!("pomb: backup {file} written ({size} bytes)");
                self.rotate(db.path(), settings.keep, &file);
                BackupStatus {
                    at: now,
                    ok: true,
                    error: None,
                    file: Some(file),
                    size_bytes: Some(size),
                    last_ok_at: Some(now),
                }
            }
            Err(error) => {
                eprintln!("pomb: backup failed: {error}");
                BackupStatus {
                    at: now,
                    ok: false,
                    error: Some(error),
                    file: None,
                    size_bytes: None,
                    last_ok_at: previous.and_then(|status| status.last_ok_at),
                }
            }
        };
        if let Err(error) = db.record_backup_status(&status) {
            eprintln!("pomb: backup status could not be stored: {error}");
        }
        status
    }

    /// Writes one verified backup file named for `now`, and returns its name and size.
    fn copy(&self, db: &Db, now: i64) -> Result<(String, u64), String> {
        let stamp = chrono::DateTime::from_timestamp(now, 0)
            .ok_or_else(|| format!("time {now} is out of range"))?
            .format(STAMP_FORMAT);
        let file = format!("{FILE_PREFIX}{stamp}{FILE_SUFFIX}");
        let dest = self.dir.join(&file);
        let part = self.dir.join(format!("{FILE_PREFIX}{stamp}{PART_SUFFIX}"));
        fs::create_dir_all(&self.dir)
            .map_err(|error| format!("could not create {}: {error}", self.dir.display()))?;
        match self.publish(db, &part, &dest) {
            Ok(size) => Ok((file, size)),
            Err(error) => {
                let _ = fs::remove_file(&part);
                Err(error)
            }
        }
    }

    /// Copies the database to `part`, checks that the copy opens, and renames it to `dest`.
    fn publish(&self, db: &Db, part: &Path, dest: &Path) -> Result<u64, String> {
        db.snapshot(part)
            .map_err(|error| format!("could not write {}: {error}", part.display()))?;
        verify(part).map_err(|error| {
            format!("the copy of the database does not open, so it was discarded: {error}")
        })?;
        fs::rename(part, dest)
            .map_err(|error| format!("could not rename {}: {error}", part.display()))?;
        fs::File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| format!("could not sync {}: {error}", self.dir.display()))?;
        let meta = fs::metadata(dest)
            .map_err(|error| format!("could not read {}: {error}", dest.display()))?;
        Ok(meta.len())
    }

    /// Deletes the oldest backup files so that `keep` remain, and never deletes `written` or the
    /// live database file at `database`.
    fn rotate(&self, database: &Path, keep: u32, written: &str) {
        let files = match self.files() {
            Ok(files) => files,
            Err(error) => {
                eprintln!(
                    "pomb: backup rotation could not list {}: {error}",
                    self.dir.display()
                );
                return;
            }
        };
        let others = files.iter().filter(|file| {
            file.name != written && !is_same_file(database, &self.dir.join(&file.name))
        });
        for stale in others.skip((keep as usize).saturating_sub(1)) {
            if let Err(error) = fs::remove_file(self.dir.join(&stale.name)) {
                eprintln!(
                    "pomb: backup rotation could not delete {}: {error}",
                    stale.name
                );
            }
        }
    }

    /// Deletes partial backup files that are older than one interval, except the live database
    /// file at `database`.
    fn remove_stale_parts(&self, database: &Path, settings: &BackupSettings, now: i64) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let stale = meta.is_file()
                && is_stamped(&name, PART_SUFFIX)
                && !is_same_file(database, &entry.path())
                && now - modified_seconds(&meta) > settings.interval_secs();
            if stale && let Err(error) = fs::remove_file(entry.path()) {
                eprintln!("pomb: backup cleanup could not delete {name}: {error}");
            }
        }
    }
}

/// Says whether two paths name the same file, by canonical path or by device and inode.
fn is_same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize())
        && a == b
    {
        return true;
    }
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Returns the `backups` directory beside the database file at `database`.
#[must_use]
fn default_dir(database: &Path) -> PathBuf {
    database
        .parent()
        .unwrap_or(Path::new(""))
        .join(DEFAULT_DIR_NAME)
}

/// Reads the backup settings and the latest status.
fn read_state(db: &Db) -> Result<(BackupSettings, Option<BackupStatus>), RunError> {
    let settings = db
        .backup_settings()
        .map_err(|error| RunError::Settings(error.to_string()))?;
    let status = db
        .backup_status()
        .map_err(|error| RunError::Settings(error.to_string()))?;
    Ok((settings, status))
}

/// Opens the copy at `part`, which runs redb's crash repair on it, and flushes it to disk.
fn verify(part: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let copy = redb::Database::create(part)?;
    copy.begin_read()?;
    drop(copy);
    fs::File::open(part)?.sync_all()?;
    Ok(())
}

/// Says whether the schedule takes a backup at `now`.
///
/// A backup is due when the schedule is on and the latest successful one is at least the interval
/// old, or none exists. A success or failure time after `now` counts as old, since the clock
/// moved back. After a failed attempt the schedule waits [`RETRY_AFTER_FAILURE_SECONDS`] before
/// it tries again.
#[must_use]
fn is_due(settings: &BackupSettings, status: Option<&BackupStatus>, now: i64) -> bool {
    if !settings.enabled {
        return false;
    }
    let Some(status) = status else {
        return true;
    };
    if !status.ok && (0..RETRY_AFTER_FAILURE_SECONDS).contains(&(now - status.at)) {
        return false;
    }
    status
        .last_ok_at
        .is_none_or(|done| done > now || now - done >= settings.interval_secs())
}

/// Checks for a due backup once a minute, until the runtime stops.
pub async fn run_schedule(db: Db, backups: Backups) {
    let mut ticker = tokio::time::interval(CHECK_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        backups.tick(&db, chrono::Utc::now().timestamp()).await;
    }
}

/// Says whether `name` is `pomb-<UTC time><suffix>`, with the time in the exact file name format.
fn is_stamped(name: &str, suffix: &str) -> bool {
    name.strip_prefix(FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(suffix))
        .and_then(|stamp| {
            let time = NaiveDateTime::parse_from_str(stamp, STAMP_FORMAT).ok()?;
            (time.format(STAMP_FORMAT).to_string() == stamp).then_some(())
        })
        .is_some()
}

/// Returns when `meta` says the file was last written, in Unix seconds.
fn modified_seconds(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::SystemTime;

    use super::*;
    use crate::models::AccountId;

    const BACKUP_TIME: i64 = 1_800_000_000;
    const MINUTE: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const WRITER_WAIT: Duration = Duration::from_millis(300);

    fn open_db(dir: &Path) -> Db {
        let path = dir.join("pomb.redb");
        Db::connect(path.to_str().expect("utf8 path")).expect("connect")
    }

    fn enabled(interval_hours: u32, keep: u32) -> BackupSettings {
        BackupSettings {
            enabled: true,
            interval_hours,
            keep,
        }
    }

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), name).expect("write file");
    }

    fn names(backups: &Backups) -> Vec<String> {
        backups
            .files()
            .expect("list")
            .into_iter()
            .map(|file| file.name)
            .collect()
    }

    fn real_now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64
    }

    fn stamped(seconds: i64) -> String {
        let stamp = chrono::DateTime::from_timestamp(seconds, 0)
            .expect("time")
            .format(STAMP_FORMAT);
        format!("{FILE_PREFIX}{stamp}{FILE_SUFFIX}")
    }

    fn part_of(name: &str) -> String {
        name.replace(FILE_SUFFIX, PART_SUFFIX)
    }

    fn succeeded_at(at: i64) -> BackupStatus {
        BackupStatus {
            at,
            ok: true,
            last_ok_at: Some(at),
            ..BackupStatus::default()
        }
    }

    fn failed_at(at: i64, last_ok_at: Option<i64>) -> BackupStatus {
        BackupStatus {
            at,
            ok: false,
            error: Some("disk full".to_string()),
            last_ok_at,
            ..BackupStatus::default()
        }
    }

    #[tokio::test]
    async fn backup_should_open_with_redb_and_hold_the_data_at_that_moment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let board = db
            .create_board(&AccountId::ownerless(), "Kept")
            .expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        db.create_task(&column, None, "Before", 1, None)
            .expect("task");
        let backups = Backups::beside(db.path());

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");
        db.create_task(&column, None, "After", 1, None)
            .expect("later task");

        assert!(status.ok, "{status:?}");
        let file = status.file.expect("file name");
        assert_eq!(file, stamped(BACKUP_TIME));
        let copy = dir.path().join("backups").join(&file);
        assert_eq!(
            status.size_bytes,
            Some(fs::metadata(&copy).expect("meta").len())
        );
        assert_eq!(names(&backups), [file]);
        let restored = Db::connect(copy.to_str().expect("utf8 path")).expect("open the backup");
        let tasks: Vec<String> = restored
            .board_tasks(&board)
            .expect("tasks")
            .into_iter()
            .map(|task| task.name)
            .collect();
        assert_eq!(tasks, ["Before"]);
        assert_eq!(
            restored.board(&board).expect("board").expect("row").name,
            "Kept"
        );
        assert_eq!(db.board_tasks(&board).expect("tasks").len(), 2);
    }

    #[tokio::test]
    async fn backup_should_open_cleanly_without_a_repair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");

        let copy = backups.dir().join(status.file.expect("file name"));
        redb::ReadOnlyDatabase::open(&copy).expect("the verified copy needs no repair");
    }

    #[tokio::test]
    async fn backup_should_leave_no_part_file_and_store_its_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");

        let left: Vec<_> = fs::read_dir(backups.dir())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(db.backup_status().expect("status"), Some(status));
    }

    #[tokio::test]
    async fn unopenable_copy_should_fail_the_backup_and_rotate_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        db.update_backup_settings(&enabled(24, 1))
            .expect("settings");
        let backups = Backups::beside(db.path());
        fs::create_dir_all(backups.dir()).expect("backup dir");
        let old: Vec<String> = (1..=3)
            .map(|day| stamped(BACKUP_TIME - day * DAY))
            .collect();
        for name in &old {
            touch(backups.dir(), name);
        }
        fs::write(db.path(), vec![0xff; 1 << 16]).expect("garble the database file");

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");

        assert!(!status.ok);
        let error = status.error.as_deref().expect("error text");
        assert!(error.contains("does not open"), "{error}");
        assert_eq!(names(&backups), old);
        assert!(!backups.dir().join(part_of(&stamped(BACKUP_TIME))).exists());
        assert_eq!(db.backup_status().expect("status"), Some(status));
    }

    #[test]
    fn writer_should_wait_for_a_snapshot_and_stay_out_of_the_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        db.create_board(&AccountId::ownerless(), "Early")
            .expect("early board");
        let copy = dir.path().join("copy.redb");
        let written = AtomicBool::new(false);
        let (started, started_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let (db, written) = (&db, &written);
            let writer = scope.spawn(move || {
                started_rx.recv().expect("snapshot started");
                db.create_board(&AccountId::ownerless(), "Late")
                    .expect("late board");
                written.store(true, Ordering::SeqCst);
            });
            db.with_writes_held(|| {
                started.send(()).expect("writer running");
                std::thread::sleep(WRITER_WAIT);
                assert!(
                    !written.load(Ordering::SeqCst),
                    "the writer committed during the snapshot"
                );
                fs::copy(db.path(), &copy).map(|_| ())
            })
            .expect("snapshot");
            writer.join().expect("writer");
        });

        assert!(written.load(Ordering::SeqCst));
        let restored = Db::connect(copy.to_str().expect("utf8 path")).expect("open the copy");
        let boards: Vec<String> = restored
            .list_boards()
            .expect("boards")
            .into_iter()
            .map(|board| board.name)
            .collect();
        assert!(boards.contains(&"Early".to_string()), "{boards:?}");
        assert!(!boards.contains(&"Late".to_string()), "{boards:?}");
        assert!(
            db.list_boards()
                .expect("boards")
                .iter()
                .any(|board| board.name == "Late")
        );
    }

    #[tokio::test]
    async fn rotation_should_keep_exactly_n_and_touch_only_matching_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());
        let now = real_now();
        db.update_backup_settings(&enabled(24, 3))
            .expect("settings");
        fs::create_dir_all(backups.dir()).expect("backup dir");
        let old: Vec<String> = (1..=5).map(|day| stamped(now - day * DAY)).collect();
        for name in &old {
            touch(backups.dir(), name);
        }
        let others = [
            "notes.txt",
            "pomb-latest.redb",
            "pomb-20200101T000000Z.redb.bak",
            "pomb-20200101T000000Z.redb.part",
            "xpomb-20200101T000000Z.redb",
            "pomb-2020010T000000Z.redb",
            "pomb-20209901T000000Z.redb",
            "pomb-20200101T000000Z.REDB",
            "pomb.redb",
        ];
        for name in others {
            touch(backups.dir(), name);
        }
        let folder = backups.dir().join("pomb-20190101T000000Z.redb");
        fs::create_dir(&folder).expect("folder named like a backup");

        let status = backups.run(&db, now).await.expect("run");

        assert!(status.ok, "{status:?}");
        assert_eq!(
            names(&backups),
            [stamped(now), old[0].clone(), old[1].clone()]
        );
        for name in others {
            assert!(backups.dir().join(name).exists(), "{name} was touched");
        }
        assert!(folder.is_dir(), "a folder was deleted");
    }

    #[tokio::test]
    async fn rotation_should_never_delete_the_live_database_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().join(stamped(BACKUP_TIME - 5 * DAY));
        let db = Db::connect(live.to_str().expect("utf8 path")).expect("connect");
        db.update_backup_settings(&enabled(24, 1))
            .expect("settings");
        let backups = Backups::new(dir.path().to_path_buf());
        let old = stamped(BACKUP_TIME - 9 * DAY);
        touch(dir.path(), &old);

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");

        assert!(status.ok, "{status:?}");
        assert!(live.is_file(), "the live database file was deleted");
        assert!(
            !dir.path().join(&old).exists(),
            "the old backup was not rotated away"
        );
        assert!(dir.path().join(stamped(BACKUP_TIME)).is_file());
        assert!(
            db.create_board(&AccountId::ownerless(), "Still works")
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rotation_should_never_delete_the_file_it_just_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());
        db.update_backup_settings(&enabled(24, 2))
            .expect("settings");
        fs::create_dir_all(backups.dir()).expect("backup dir");
        let future: Vec<String> = (1..=3)
            .map(|day| stamped(BACKUP_TIME + day * DAY))
            .collect();
        for name in &future {
            touch(backups.dir(), name);
        }

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");

        assert!(status.ok, "{status:?}");
        assert_eq!(names(&backups), [future[2].clone(), stamped(BACKUP_TIME)]);
    }

    #[tokio::test]
    async fn failed_backup_should_delete_no_old_copy_and_record_the_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());
        db.update_backup_settings(&enabled(24, 1))
            .expect("settings");
        fs::create_dir_all(backups.dir()).expect("backup dir");
        let old: Vec<String> = (1..=3)
            .map(|day| stamped(BACKUP_TIME - day * DAY))
            .collect();
        for name in &old {
            touch(backups.dir(), name);
        }
        fs::create_dir(backups.dir().join(part_of(&stamped(BACKUP_TIME)))).expect("blocker");

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");

        assert!(!status.ok);
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|text| text.contains("could not write"))
        );
        assert_eq!((status.file.clone(), status.size_bytes), (None, None));
        assert_eq!(names(&backups), old);
        assert_eq!(db.backup_status().expect("status"), Some(status));
    }

    #[tokio::test]
    async fn unwritable_directory_should_be_recorded_and_leave_the_server_working() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let file = dir.path().join("occupied");
        fs::write(&file, "a file, not a directory").expect("file");
        let backups = Backups::new(file.join("backups"));

        let status = backups.run(&db, BACKUP_TIME).await.expect("run");

        assert!(!status.ok);
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|text| text.contains("could not create"))
        );
        assert_eq!(db.backup_status().expect("status"), Some(status));
        assert!(
            db.create_board(&AccountId::ownerless(), "Still works")
                .is_ok()
        );
        assert!(backups.files().is_err(), "the directory cannot be listed");
    }

    #[tokio::test]
    async fn failure_should_keep_the_time_of_the_last_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());
        backups.run(&db, BACKUP_TIME).await.expect("first");
        let later = BACKUP_TIME + MINUTE;
        fs::create_dir(backups.dir().join(part_of(&stamped(later)))).expect("blocker");

        let status = backups.run(&db, later).await.expect("second");

        assert!(!status.ok);
        assert_eq!(status.last_ok_at, Some(BACKUP_TIME));
    }

    #[tokio::test]
    async fn backup_should_be_refused_while_the_lock_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());

        let running = backups.hold().expect("lock");
        assert_eq!(
            backups.run(&db, BACKUP_TIME).await,
            Err(RunError::Busy),
            "back up now"
        );
        db.update_backup_settings(&enabled(1, 7)).expect("settings");
        assert_eq!(backups.tick(&db, BACKUP_TIME).await, None, "the schedule");
        assert_eq!(backups.hold().err(), Some(RunError::Busy));
        assert!(!backups.dir().exists(), "nothing was written");

        drop(running);
        assert!(
            backups.run(&db, BACKUP_TIME).await.is_ok(),
            "the lock reopens"
        );
    }

    #[tokio::test]
    async fn tick_should_check_the_schedule_again_once_it_holds_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        db.update_backup_settings(&enabled(24, 7))
            .expect("settings");
        let backups = Backups::beside(db.path());

        let manual = backups.run(&db, BACKUP_TIME).await.expect("back up now");
        let scheduled = backups.tick(&db, BACKUP_TIME + MINUTE).await;

        assert!(manual.ok);
        assert_eq!(scheduled, None);
        assert_eq!(names(&backups), [stamped(BACKUP_TIME)]);
    }

    #[tokio::test]
    async fn stale_part_files_should_be_removed_and_fresh_ones_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());
        fs::create_dir_all(backups.dir()).expect("backup dir");
        let now = real_now();
        let aged = |name: &str, hours: u64| {
            touch(backups.dir(), name);
            let file = fs::File::options()
                .write(true)
                .open(backups.dir().join(name))
                .expect("open");
            file.set_modified(SystemTime::now() - Duration::from_secs(hours * HOUR as u64))
                .expect("set mtime");
        };
        aged("pomb-20200101T000000Z.redb.part", 25);
        aged("pomb-20200102T000000Z.redb.part", 1);
        aged("notes.part", 100);
        aged("pomb-20200103T000000Z.redb", 100);

        backups.run(&db, now).await.expect("run");

        let mut left: Vec<String> = fs::read_dir(backups.dir())
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "notes.part".to_string(),
                "pomb-20200102T000000Z.redb.part".to_string(),
                "pomb-20200103T000000Z.redb".to_string(),
                stamped(now),
            ]
        );
    }

    #[test]
    fn is_due_should_follow_the_interval_since_the_last_success() {
        let settings = enabled(24, 7);
        let ok = succeeded_at(BACKUP_TIME);
        assert!(is_due(&settings, None, BACKUP_TIME), "never ran");
        assert!(!is_due(&settings, Some(&ok), BACKUP_TIME + DAY - 1));
        assert!(is_due(&settings, Some(&ok), BACKUP_TIME + DAY));
        assert!(
            !is_due(&BackupSettings::default(), None, BACKUP_TIME),
            "off by default"
        );
        assert!(!is_due(
            &BackupSettings::default(),
            Some(&ok),
            BACKUP_TIME + 10 * DAY
        ));
    }

    #[test]
    fn is_due_should_treat_a_success_in_the_future_as_old() {
        let settings = enabled(24, 7);
        assert!(is_due(
            &settings,
            Some(&succeeded_at(BACKUP_TIME + DAY)),
            BACKUP_TIME
        ));
    }

    #[test]
    fn is_due_should_wait_out_the_retry_delay_after_a_failure() {
        let failed = failed_at(BACKUP_TIME, Some(BACKUP_TIME - WEEK));
        let settings = enabled(24, 7);
        let after = |seconds| BACKUP_TIME + seconds;
        assert!(!is_due(
            &settings,
            Some(&failed),
            after(RETRY_AFTER_FAILURE_SECONDS - 1)
        ));
        assert!(is_due(
            &settings,
            Some(&failed),
            after(RETRY_AFTER_FAILURE_SECONDS)
        ));
        let never = failed_at(BACKUP_TIME, None);
        assert!(is_due(
            &settings,
            Some(&never),
            after(RETRY_AFTER_FAILURE_SECONDS)
        ));
    }

    #[test]
    fn is_due_should_treat_a_failure_in_the_future_as_old() {
        let failed = failed_at(BACKUP_TIME + DAY, Some(BACKUP_TIME - WEEK));
        assert!(is_due(&enabled(24, 7), Some(&failed), BACKUP_TIME));
    }

    #[tokio::test]
    async fn tick_should_back_up_only_when_the_schedule_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());

        assert_eq!(backups.tick(&db, BACKUP_TIME).await, None, "off by default");
        assert!(backups.files().expect("list").is_empty());

        db.update_backup_settings(&enabled(6, 7)).expect("settings");
        assert!(
            backups
                .tick(&db, BACKUP_TIME)
                .await
                .is_some_and(|status| status.ok)
        );
        assert_eq!(
            backups.tick(&db, BACKUP_TIME + 5 * HOUR).await,
            None,
            "interval not over"
        );
        assert!(
            backups
                .tick(&db, BACKUP_TIME + 6 * HOUR)
                .await
                .is_some_and(|status| status.ok)
        );
        assert_eq!(
            names(&backups),
            [stamped(BACKUP_TIME + 6 * HOUR), stamped(BACKUP_TIME)]
        );
    }

    #[tokio::test]
    async fn restart_should_not_back_up_again_before_the_interval() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let db = open_db(dir.path());
            db.update_backup_settings(&enabled(24, 7))
                .expect("settings");
            let backups = Backups::beside(db.path());
            assert!(backups.tick(&db, BACKUP_TIME).await.is_some());
        }
        let db = open_db(dir.path());
        let backups = Backups::beside(db.path());

        assert_eq!(backups.tick(&db, BACKUP_TIME + HOUR).await, None);
        assert!(backups.tick(&db, BACKUP_TIME + DAY).await.is_some());
    }

    #[tokio::test]
    async fn failing_schedule_should_retry_after_the_delay_not_every_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        db.update_backup_settings(&enabled(1, 7)).expect("settings");
        let file = dir.path().join("occupied");
        fs::write(&file, "x").expect("file");
        let backups = Backups::new(file.join("backups"));

        assert!(
            backups
                .tick(&db, BACKUP_TIME)
                .await
                .is_some_and(|status| !status.ok)
        );
        assert_eq!(backups.tick(&db, BACKUP_TIME + MINUTE).await, None);
        assert!(
            backups
                .tick(&db, BACKUP_TIME + RETRY_AFTER_FAILURE_SECONDS)
                .await
                .is_some_and(|status| !status.ok)
        );
    }

    #[test]
    fn default_dir_should_sit_beside_the_database_file() {
        assert_eq!(
            default_dir(Path::new("/var/lib/pomb/pomb.redb")),
            Path::new("/var/lib/pomb/backups")
        );
        assert_eq!(default_dir(Path::new("pomb.redb")), Path::new("backups"));
    }
}
