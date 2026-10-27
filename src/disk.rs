//! The disk guard: when the database holds more than its limit or the disk beside it runs low,
//! writes that grow the database stop until the admin makes room.
//!
//! [`DiskGuard`] measures through a [`DiskProbe`] that production points at the database and
//! tests replace with fixed numbers. A background task measures again every
//! [`DiskGuard::SAMPLE_EVERY`] on a blocking thread, and a write that finds the latest measurement
//! older than that starts one more there, so no request ever probes. A measurement older than
//! [`DiskGuard::STALE_AFTER`] stops the writes that grow the database. The limits come from the
//! instance's [`DiskLimits`], swapped live when the admin saves them.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};
use std::{fmt, fs, io};

use tokio::time::MissedTickBehavior;

use crate::db::{Db, DbResult};
use crate::models::{DiskLimits, InstanceMode};

/// Bytes in one MiB.
pub(crate) const MIB: u64 = 1024 * 1024;

/// What one probe measured.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DiskUsage {
    /// Bytes of keys, values and metadata the database holds, which deletes reduce even when the
    /// file keeps its size.
    pub database_bytes: u64,
    /// Bytes the database file takes, which deletes never reduce.
    pub file_bytes: u64,
    /// Bytes an unprivileged writer may still use on the database's file system.
    pub free_bytes: u64,
}

/// A probe that measures the bytes of keys, values and metadata a database holds.
pub type StoredBytes = Arc<dyn Fn() -> DbResult<u64> + Send + Sync>;

/// A probe that measures the bytes a database file takes.
pub type FileBytes = Arc<dyn Fn() -> io::Result<u64> + Send + Sync>;

/// A probe that measures the bytes an unprivileged writer may still use on the file system
/// holding a path.
pub type FreeBytes = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

/// A source of the current time.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The probes a [`DiskGuard`] measures through.
#[derive(Clone)]
pub struct DiskProbe {
    /// Measures the data the database holds.
    pub stored: StoredBytes,
    /// Measures the size of the database file.
    pub file: FileBytes,
    /// Measures the free space on the file system holding a path.
    pub free: FreeBytes,
}

impl DiskProbe {
    /// Returns the probe that asks `db` for the data it holds, its file for its size and the file
    /// system for its free space.
    #[must_use]
    pub fn of(db: &Db) -> Self {
        let path = db.path().to_path_buf();
        let db = db.clone();
        Self {
            stored: Arc::new(move || db.stored_bytes()),
            file: Arc::new(move || Ok(fs::metadata(&path)?.len())),
            free: Arc::new(free_bytes),
        }
    }
}

impl fmt::Debug for DiskProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DiskProbe(..)")
    }
}

/// Returns the bytes an unprivileged writer may still use on the file system holding `path`.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let stats = rustix::fs::statvfs(path)?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

/// Why the disk guard refuses a write or a backup.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DiskFull {
    /// The database holds its limit, in MiB.
    Database {
        /// The limit in MiB.
        max_mib: u32,
    },
    /// Less than the floor of free disk space is left, in MiB.
    FreeSpace {
        /// The floor in MiB.
        min_mib: u32,
    },
    /// A backup would leave less than the floor of free disk space, in MiB.
    BackupRoom {
        /// The floor in MiB.
        min_mib: u32,
    },
    /// No measurement succeeded within [`DiskGuard::STALE_AFTER`].
    Unmeasured,
}

impl DiskFull {
    /// Returns what is full, as a lowercase phrase.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Database { max_mib } => {
                format!("the database reached its limit of {max_mib} MiB")
            }
            Self::FreeSpace { min_mib } => {
                format!("less than {min_mib} MiB of disk space is free")
            }
            Self::BackupRoom { min_mib } => {
                format!("a backup would leave less than {min_mib} MiB of disk space free")
            }
            Self::Unmeasured => format!(
                "the disk was not measured in the last {} seconds",
                DiskGuard::STALE_AFTER.as_secs()
            ),
        }
    }
}

impl fmt::Display for DiskFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "storage is full: {}", self.detail())
    }
}

/// The disk guard of one server: its probe, its limits and its latest measurement.
#[derive(Clone)]
pub struct DiskGuard {
    /// What the guard measures through.
    probe: DiskProbe,
    /// The directory holding the database, whose file system's free space counts.
    dir: Arc<Path>,
    /// The limits every clone follows.
    limits: Arc<RwLock<DiskLimits>>,
    /// The latest successful measurement and when it was taken, absent until one succeeds.
    sample: Arc<Mutex<Option<(Instant, DiskUsage)>>>,
    /// Where the guard reads the time a measurement was taken and how old it is.
    clock: Clock,
    /// Whether a measurement a write started still runs, so at most one does.
    refreshing: Arc<AtomicBool>,
}

/// The latest measurement and whether it is older than [`DiskGuard::STALE_AFTER`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DiskSample {
    /// What the probe measured.
    pub usage: DiskUsage,
    /// Whether the measurement is too old to count.
    pub stale: bool,
}

impl DiskGuard {
    /// How often the background task measures again.
    pub const SAMPLE_EVERY: Duration = Duration::from_secs(10);
    /// Age past which a measurement no longer counts, so writes that grow the database stop.
    pub const STALE_AFTER: Duration = Duration::from_secs(60);

    /// Builds a guard with the default limits for the database in `dir`, and measures once.
    #[must_use]
    pub fn new(probe: DiskProbe, dir: &Path) -> Self {
        Self::with_clock(probe, dir, Arc::new(Instant::now))
    }

    /// Builds a guard like [`DiskGuard::new`] that reads the time from `clock`.
    #[must_use]
    pub fn with_clock(probe: DiskProbe, dir: &Path, clock: Clock) -> Self {
        let guard = Self {
            probe,
            dir: Arc::from(dir),
            limits: Arc::new(RwLock::new(DiskLimits::default())),
            sample: Arc::new(Mutex::new(None)),
            clock,
            refreshing: Arc::new(AtomicBool::new(false)),
        };
        guard.refresh();
        guard
    }

    /// Builds a guard with the default limits for `db`, and measures once.
    #[must_use]
    pub fn beside(db: &Db) -> Self {
        let dir = db
            .path()
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Self::new(DiskProbe::of(db), dir)
    }

    /// Returns the limits the guard follows.
    #[must_use]
    pub fn limits(&self) -> DiskLimits {
        *self.limits.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes every clone of this guard follow `limits` from the next check on.
    pub fn apply(&self, limits: &DiskLimits) {
        *self.limits.write().unwrap_or_else(PoisonError::into_inner) = *limits;
    }

    /// Measures the database and its disk again and keeps the result. A failed probe is logged
    /// and keeps the previous measurement.
    pub fn refresh(&self) {
        match self.measure() {
            Ok(usage) => *lock(&self.sample) = Some(((self.clock)(), usage)),
            Err(error) => eprintln!("pomb: could not measure the disk: {error}"),
        }
    }

    /// Runs every probe once.
    fn measure(&self) -> Result<DiskUsage, String> {
        let database_bytes = (self.probe.stored)().map_err(|error| error.to_string())?;
        let file_bytes = (self.probe.file)().map_err(|error| error.to_string())?;
        let free_bytes = (self.probe.free)(&self.dir).map_err(|error| error.to_string())?;
        Ok(DiskUsage {
            database_bytes,
            file_bytes,
            free_bytes,
        })
    }

    /// Measures again every [`Self::SAMPLE_EVERY`] on a blocking thread, until the runtime stops.
    pub async fn run_sampler(self) {
        let mut ticker = tokio::time::interval(Self::SAMPLE_EVERY);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let guard = self.clone();
            if let Err(error) = tokio::task::spawn_blocking(move || guard.refresh()).await {
                eprintln!("pomb: the disk measurement failed: {error}");
            }
        }
    }

    /// Returns why writes that grow the database stop, if they do, for an instance in `mode`, from
    /// the latest measurement, and starts one more on a blocking thread when that is older than
    /// [`DiskGuard::SAMPLE_EVERY`]. The database limit applies in multi mode only, to both the
    /// data the database holds and the size of its file. While no measurement is younger than
    /// [`DiskGuard::STALE_AFTER`], every write that grows the database stops.
    #[must_use]
    pub fn full(&self, mode: InstanceMode) -> Option<DiskFull> {
        let full = self.cached_full(mode);
        if self
            .sample()
            .is_none_or(|(at, _)| self.age(at) > Self::SAMPLE_EVERY)
        {
            self.refresh_in_background();
        }
        full
    }

    /// Returns why writes that grow the database stop, as [`DiskGuard::full`] does, from the
    /// latest measurement alone, so a page can say so without starting a measurement.
    #[must_use]
    pub fn cached_full(&self, mode: InstanceMode) -> Option<DiskFull> {
        let limits = self.limits();
        let Some(DiskSample {
            usage,
            stale: false,
        }) = self.latest()
        else {
            return Some(DiskFull::Unmeasured);
        };
        if let Some(max_mib) = limits.max_database_mib
            && mode == InstanceMode::Multi
            && usage.database_bytes.max(usage.file_bytes) >= u64::from(max_mib) * MIB
        {
            return Some(DiskFull::Database { max_mib });
        }
        Self::below(limits, usage)
    }

    /// Returns the latest measurement as it is, marked stale once it is older than
    /// [`DiskGuard::STALE_AFTER`]. Absent until a measurement succeeds.
    #[must_use]
    pub fn latest(&self) -> Option<DiskSample> {
        let (at, usage) = self.sample()?;
        Some(DiskSample {
            usage,
            stale: self.age(at) > Self::STALE_AFTER,
        })
    }

    /// Measures again on a blocking thread unless such a measurement already runs.
    fn refresh_in_background(&self) {
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let guard = self.clone();
        tokio::task::spawn_blocking(move || {
            let _done = Refreshing(guard.refreshing.clone());
            guard.refresh();
        });
    }

    /// Returns the latest measurement and when it was taken.
    fn sample(&self) -> Option<(Instant, DiskUsage)> {
        *lock(&self.sample)
    }

    /// Returns how long ago `at` was.
    fn age(&self, at: Instant) -> Duration {
        (self.clock)().saturating_duration_since(at)
    }

    /// Returns the refusal of a backup of `backup_bytes` into `dir` when it would leave less than
    /// the floor of free space there, measured now. A failed measurement is logged and refuses
    /// nothing.
    #[must_use]
    pub fn backup_room(&self, dir: &Path, backup_bytes: u64) -> Option<DiskFull> {
        let min_mib = self.limits().min_free_mib;
        let free = (self.probe.free)(dir)
            .inspect_err(|error| {
                eprintln!(
                    "pomb: could not measure the disk at {}: {error}",
                    dir.display()
                );
            })
            .ok()?;
        free.checked_sub(backup_bytes)
            .is_none_or(|left| left < u64::from(min_mib) * MIB)
            .then_some(DiskFull::BackupRoom { min_mib })
    }

    /// Returns the refusal when `usage` leaves less free space than the floor in `limits`.
    #[must_use]
    fn below(limits: DiskLimits, usage: DiskUsage) -> Option<DiskFull> {
        let min_mib = limits.min_free_mib;
        (usage.free_bytes < u64::from(min_mib) * MIB).then_some(DiskFull::FreeSpace { min_mib })
    }
}

impl fmt::Debug for DiskGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DiskGuard(..)")
    }
}

/// The flag of a running background measurement, cleared when the measurement ends, even by a
/// panic.
struct Refreshing(Arc<AtomicBool>);

impl Drop for Refreshing {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Locks `mutex`, taking the value over from a holder that panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;

    const DATABASE_MIB: u64 = 600;
    const ROOMY_MIB: u64 = 100_000;
    const LOW_MIB: u64 = 100;

    fn fixed(database_mib: u64, free_mib: u64) -> DiskProbe {
        DiskProbe {
            stored: Arc::new(move || Ok(database_mib * MIB)),
            file: Arc::new(move || Ok(database_mib * MIB)),
            free: Arc::new(move |_| Ok(free_mib * MIB)),
        }
    }

    /// A clock the test moves by hand.
    #[derive(Clone)]
    struct HandClock(Arc<Mutex<Instant>>);

    impl HandClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Instant::now())))
        }

        fn clock(&self) -> Clock {
            let now = self.0.clone();
            Arc::new(move || *lock(&now))
        }

        fn advance(&self, by: Duration) {
            *lock(&self.0) += by;
        }
    }

    #[tokio::test]
    async fn full_should_apply_the_database_limit_to_the_file_too() {
        let probe = DiskProbe {
            file: Arc::new(|| Ok(DATABASE_MIB * MIB)),
            ..fixed(1, ROOMY_MIB)
        };
        let guard = DiskGuard::new(probe, Path::new("."));
        assert_eq!(
            guard.full(InstanceMode::Multi),
            Some(DiskFull::Database {
                max_mib: DiskLimits::DEFAULT_MAX_DATABASE_MIB
            })
        );
        assert_eq!(guard.full(InstanceMode::Single), None);
    }

    /// Times [`until`] checks before it gives up.
    const POLL_LIMIT: u32 = 500;
    /// Milliseconds [`until`] waits between checks.
    const POLL_EVERY_MS: u64 = 10;

    /// Waits, in real time, until `done` holds.
    async fn until(mut done: impl FnMut() -> bool) {
        for _ in 0..POLL_LIMIT {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(POLL_EVERY_MS)).await;
        }
        panic!("the background measurement never ran");
    }

    /// Asks `guard` for a measurement until `probes` has counted `count` of them.
    async fn probed(guard: &DiskGuard, probes: &AtomicU64, count: u64) {
        until(|| {
            let _ = guard.full(InstanceMode::Multi);
            probes.load(Ordering::Relaxed) >= count
        })
        .await;
    }

    #[tokio::test]
    async fn full_should_measure_in_the_background_and_refuse_while_stale() {
        let probes = Arc::new(AtomicU64::new(0));
        let failing = Arc::new(AtomicBool::new(false));
        let (counted, fails) = (probes.clone(), failing.clone());
        let probe = DiskProbe {
            stored: Arc::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                if fails.load(Ordering::Relaxed) {
                    Err("probe failed".into())
                } else {
                    Ok(MIB)
                }
            }),
            ..fixed(1, ROOMY_MIB)
        };
        let time = HandClock::new();
        let guard = DiskGuard::with_clock(probe, Path::new("."), time.clock());
        assert_eq!(guard.full(InstanceMode::Single), None);
        assert_eq!(probes.load(Ordering::Relaxed), 1, "a fresh sample counts");

        time.advance(DiskGuard::SAMPLE_EVERY + Duration::from_secs(1));
        assert_eq!(
            guard.full(InstanceMode::Single),
            None,
            "a sample not yet stale"
        );
        probed(&guard, &probes, 2).await;

        failing.store(true, Ordering::Relaxed);
        time.advance(DiskGuard::STALE_AFTER + Duration::from_secs(1));
        assert_eq!(guard.full(InstanceMode::Multi), Some(DiskFull::Unmeasured));
        assert_eq!(
            guard.cached_full(InstanceMode::Multi),
            Some(DiskFull::Unmeasured)
        );
        probed(&guard, &probes, 3).await;
        assert!(guard.latest().is_some_and(|sample| sample.stale));

        failing.store(false, Ordering::Relaxed);
        assert_eq!(guard.full(InstanceMode::Multi), Some(DiskFull::Unmeasured));
        until(|| guard.full(InstanceMode::Multi).is_none()).await;
    }

    #[test]
    fn cached_full_should_never_measure() {
        let probes = Arc::new(AtomicU64::new(0));
        let counted = probes.clone();
        let probe = DiskProbe {
            stored: Arc::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(MIB)
            }),
            ..fixed(1, ROOMY_MIB)
        };
        let time = HandClock::new();
        let guard = DiskGuard::with_clock(probe, Path::new("."), time.clock());
        time.advance(DiskGuard::STALE_AFTER + Duration::from_secs(1));
        assert_eq!(
            guard.cached_full(InstanceMode::Single),
            Some(DiskFull::Unmeasured)
        );
        assert_eq!(probes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn full_should_refuse_before_any_measurement_succeeds() {
        let probe = DiskProbe {
            free: Arc::new(|_| Err(io::Error::from(io::ErrorKind::NotFound))),
            ..fixed(1, ROOMY_MIB)
        };
        let guard = DiskGuard::new(probe, Path::new("."));
        assert_eq!(guard.full(InstanceMode::Single), Some(DiskFull::Unmeasured));
    }

    #[tokio::test]
    async fn full_should_apply_the_database_limit_in_multi_mode_only() {
        let guard = DiskGuard::new(fixed(DATABASE_MIB, ROOMY_MIB), Path::new("."));
        assert_eq!(
            guard.full(InstanceMode::Multi),
            Some(DiskFull::Database {
                max_mib: DiskLimits::DEFAULT_MAX_DATABASE_MIB
            })
        );
        assert_eq!(guard.full(InstanceMode::Single), None);
        guard.apply(&DiskLimits {
            max_database_mib: None,
            ..DiskLimits::default()
        });
        assert_eq!(guard.full(InstanceMode::Multi), None);
    }

    #[tokio::test]
    async fn full_should_apply_the_free_space_floor_in_both_modes() {
        let guard = DiskGuard::new(fixed(1, LOW_MIB), Path::new("."));
        let floor = Some(DiskFull::FreeSpace {
            min_mib: DiskLimits::DEFAULT_MIN_FREE_MIB,
        });
        assert_eq!(guard.full(InstanceMode::Single), floor);
        assert_eq!(guard.full(InstanceMode::Multi), floor);
        guard.apply(&DiskLimits {
            min_free_mib: DiskLimits::FLOOR_OFF_MIB,
            ..DiskLimits::default()
        });
        assert_eq!(guard.full(InstanceMode::Multi), None);
    }

    #[tokio::test]
    async fn full_should_read_the_latest_measurement_without_probing() {
        let probes = Arc::new(AtomicU64::new(0));
        let counted = probes.clone();
        let probe = DiskProbe {
            stored: Arc::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(MIB)
            }),
            ..fixed(1, ROOMY_MIB)
        };
        let guard = DiskGuard::new(probe, Path::new("."));
        for _ in 0..50 {
            assert_eq!(guard.full(InstanceMode::Multi), None);
        }
        assert_eq!(probes.load(Ordering::Relaxed), 1);
        guard.refresh();
        assert_eq!(probes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn backup_room_should_count_the_backup_against_the_floor() {
        const SPARE_MIB: u64 = 10;
        let floor = u64::from(DiskLimits::DEFAULT_MIN_FREE_MIB);
        let guard = DiskGuard::new(fixed(1, floor + SPARE_MIB), Path::new("."));
        let refused = Some(DiskFull::BackupRoom {
            min_mib: DiskLimits::DEFAULT_MIN_FREE_MIB,
        });
        assert_eq!(guard.backup_room(Path::new("."), SPARE_MIB * MIB), None);
        assert_eq!(
            guard.backup_room(Path::new("."), (SPARE_MIB + 1) * MIB),
            refused
        );
    }

    #[test]
    fn free_bytes_should_measure_a_real_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(free_bytes(dir.path()).expect("statvfs") > 0);
    }
}
