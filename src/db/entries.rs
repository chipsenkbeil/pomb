use chrono::{DateTime, Days, NaiveDate, TimeZone, Utc};
use redb::{ReadableMultimapTable, WriteTransaction};

use super::usage::OverQuota;
use super::{
    Bound, Db, DbResult, ENTRIES_BY_ACCOUNT, ENTRIES_BY_DAY, QuotaExceeded, TIME_ENTRIES, day_in,
    index_in, read_in, rows_in, txn_refuse_over_quota,
};
use crate::models::{AccountId, Quota, TimeEntryRow};

/// Most time entries one account may start on one UTC day, the admin's included.
pub const MAX_ENTRIES_PER_DAY: usize = 500;

/// Returns the key of [`ENTRIES_BY_DAY`] that `account`'s entries started on `day` sit under.
#[must_use]
fn day_key(account: &str, day: &str) -> String {
    format!("{account}|{day}")
}

/// Returns the key of [`ENTRIES_BY_DAY`] `entry` sits under, absent when its start does not
/// parse.
#[must_use]
fn entry_day_key(entry: &TimeEntryRow) -> Option<String> {
    day_in(&entry.started_at, &Utc).map(|day| day_key(&entry.account_id, &day))
}

/// Refuses, with [`QuotaExceeded`], storing `entry` over `before`, the entry it replaces, when a
/// new entry takes its account past the time entry quota, or when a new or moved entry takes its
/// day past [`MAX_ENTRIES_PER_DAY`], inside that write's transaction. A write under
/// [`OverQuota::Allow`] is never refused.
pub(super) fn txn_refuse_entry(
    txn: &WriteTransaction,
    entry: &TimeEntryRow,
    before: Option<&TimeEntryRow>,
    over: OverQuota,
) -> DbResult<()> {
    if over == OverQuota::Allow {
        return Ok(());
    }
    if before.is_none() {
        let held = txn
            .open_multimap_table(ENTRIES_BY_ACCOUNT)?
            .get(entry.account_id.as_str())?
            .len();
        txn_refuse_over_quota(
            txn,
            &entry.account_id,
            Quota::TimeEntriesPerAccount,
            held as usize + 1,
        )?;
    }
    let Some(key) = entry_day_key(entry) else {
        return Ok(());
    };
    if before.and_then(entry_day_key).as_ref() == Some(&key) {
        return Ok(());
    }
    let held = txn
        .open_multimap_table(ENTRIES_BY_DAY)?
        .get(key.as_str())?
        .len();
    if held as usize >= MAX_ENTRIES_PER_DAY {
        return Err(Box::new(QuotaExceeded {
            bound: Bound::EntriesPerDay,
            limit: MAX_ENTRIES_PER_DAY as u64,
        }));
    }
    Ok(())
}

/// Indexes `entry` by its account and day, dropping the day of `before`, the entry it replaces,
/// inside an open write transaction.
pub(super) fn txn_index_entry(
    txn: &WriteTransaction,
    entry: &TimeEntryRow,
    before: Option<&TimeEntryRow>,
) -> DbResult<()> {
    if let Some(before) = before {
        txn_unindex_entry(txn, before)?;
    }
    txn.open_multimap_table(ENTRIES_BY_ACCOUNT)?
        .insert(entry.account_id.as_str(), entry.id.as_str())?;
    if let Some(key) = entry_day_key(entry) {
        txn.open_multimap_table(ENTRIES_BY_DAY)?
            .insert(key.as_str(), entry.id.as_str())?;
    }
    Ok(())
}

/// Drops `entry` from the indexes of its account and day, inside an open write transaction.
pub(super) fn txn_unindex_entry(txn: &WriteTransaction, entry: &TimeEntryRow) -> DbResult<()> {
    txn.open_multimap_table(ENTRIES_BY_ACCOUNT)?
        .remove(entry.account_id.as_str(), entry.id.as_str())?;
    if let Some(key) = entry_day_key(entry) {
        txn.open_multimap_table(ENTRIES_BY_DAY)?
            .remove(key.as_str(), entry.id.as_str())?;
    }
    Ok(())
}

/// Rebuilds the indexes of every time entry by account and day, inside an open write transaction.
pub(super) fn txn_reindex_entries(txn: &WriteTransaction) -> DbResult<()> {
    for index in [ENTRIES_BY_ACCOUNT, ENTRIES_BY_DAY] {
        txn.delete_multimap_table(index)?;
        txn.open_multimap_table(index)?;
    }
    let entries: Vec<TimeEntryRow> = rows_in(&txn.open_table(TIME_ENTRIES)?)?;
    for entry in &entries {
        txn_index_entry(txn, entry, None)?;
    }
    Ok(())
}

/// Returns the instant `entry` started at in Unix seconds, zero when it does not parse.
#[must_use]
fn started_instant(entry: &TimeEntryRow) -> i64 {
    DateTime::parse_from_rfc3339(&entry.started_at).map_or(0, |dt| dt.timestamp())
}

impl Db {
    /// Returns every time entry `account` logged, in no set order.
    pub fn account_entries(&self, account: &AccountId) -> DbResult<Vec<TimeEntryRow>> {
        let txn = self.read()?;
        let mut out = Vec::new();
        for id in index_in(&txn, ENTRIES_BY_ACCOUNT, account.as_str())? {
            out.extend(read_in::<TimeEntryRow>(&txn, TIME_ENTRIES, &id)?);
        }
        Ok(out)
    }

    /// Returns `account`'s time entries that started today in `zone`, newest first.
    pub fn entries_today<Tz: TimeZone>(
        &self,
        account: &AccountId,
        zone: &Tz,
    ) -> DbResult<Vec<TimeEntryRow>> {
        self.entries_on(account, Utc::now().with_timezone(zone).date_naive(), zone)
    }

    /// Returns `account`'s time entries that started on `day` in `zone`, newest first.
    fn entries_on<Tz: TimeZone>(
        &self,
        account: &AccountId,
        day: NaiveDate,
        zone: &Tz,
    ) -> DbResult<Vec<TimeEntryRow>> {
        let wanted = day.format("%Y-%m-%d").to_string();
        let utc_days = [
            day.checked_sub_days(Days::new(1)),
            Some(day),
            day.checked_add_days(Days::new(1)),
        ];
        let txn = self.read()?;
        let mut out = Vec::new();
        for utc_day in utc_days.into_iter().flatten() {
            let key = day_key(account.as_str(), &utc_day.format("%Y-%m-%d").to_string());
            for id in index_in(&txn, ENTRIES_BY_DAY, &key)? {
                let entry = read_in::<TimeEntryRow>(&txn, TIME_ENTRIES, &id)?;
                out.extend(entry.filter(|entry| {
                    day_in(&entry.started_at, zone).as_deref() == Some(wanted.as_str())
                }));
            }
        }

        // Newest start first. Timer rows are stored in UTC and edited rows with a local offset, so
        // compare instants, not strings.
        out.sort_by(|a, b| {
            started_instant(b)
                .cmp(&started_instant(a))
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, Local};

    use super::*;
    use crate::db::{META, NewTimeEntry, SCHEMA_VERSION_KEY};
    use crate::models::{EntryKind, InstanceMode, Quotas};

    /// An open database with its admin and a member, in multi mode.
    struct Fixture {
        /// Keeps the database directory alive.
        _dir: tempfile::TempDir,
        /// The database.
        db: Db,
        /// The admin.
        admin: AccountId,
        /// The member.
        member: AccountId,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("entries.redb").to_str().expect("utf8")).expect("db");
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        Fixture {
            _dir: dir,
            db,
            admin,
            member,
        }
    }

    /// Logs a task-less manual entry for `account` started at `started_at`.
    fn log(db: &Db, account: &AccountId, started_at: &str) -> DbResult<Option<String>> {
        db.create_entry_full(
            account,
            NewTimeEntry {
                task_id: None,
                started_at,
                minutes: 1,
                seconds: 60,
                note: "",
                kind: EntryKind::Manual,
                interrupted: false,
                interrupt_reason: None,
                labels: &[],
                created_by: "",
                activity_id: None,
            },
        )
    }

    /// Returns the bound `result` was refused past.
    fn refused_bound(result: DbResult<Option<String>>) -> Bound {
        let error = result.expect_err("refused");
        error
            .downcast_ref::<QuotaExceeded>()
            .map(|refusal| refusal.bound)
            .unwrap_or_else(|| panic!("not a quota refusal: {error}"))
    }

    #[test]
    fn entries_today_should_read_only_the_account_and_its_days() {
        let fx = fixture();
        let now = Utc::now().to_rfc3339();
        let mine = log(&fx.db, &fx.member, &now).expect("log").expect("id");
        log(&fx.db, &fx.admin, &now).expect("log");
        log(&fx.db, &fx.member, "2020-01-02T10:00:00Z").expect("log");

        let today = fx.db.entries_today(&fx.member, &Local).expect("today");
        assert_eq!(
            today
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            [mine.as_str()]
        );
        assert_eq!(fx.db.account_entries(&fx.member).expect("all").len(), 2);
    }

    #[test]
    fn entries_on_should_find_a_local_day_that_spans_two_utc_days() {
        for (offset, hours) in [("-12:00", -12), ("+14:00", 14)] {
            let fx = fixture();
            let zone = FixedOffset::east_opt(hours * 3600).expect("offset");
            let start = |at: &str| format!("{at}{offset}");
            let mut wanted = Vec::new();
            for at in ["2020-06-15T00:05:00", "2020-06-15T23:55:00"] {
                let id = log(&fx.db, &fx.member, &start(at))
                    .expect("log")
                    .expect("id");
                wanted.push(id);
            }
            for at in ["2020-06-14T23:55:00", "2020-06-16T00:05:00"] {
                log(&fx.db, &fx.member, &start(at)).expect("log");
            }
            let day = NaiveDate::from_ymd_opt(2020, 6, 15).expect("date");
            let found = fx.db.entries_on(&fx.member, day, &zone).expect("day");
            let mut found: Vec<String> = found.into_iter().map(|entry| entry.id).collect();
            found.sort_unstable();
            wanted.sort_unstable();
            assert_eq!(found, wanted, "{offset}");
        }
    }

    #[test]
    fn entry_should_move_between_days_when_its_start_changes() {
        let fx = fixture();
        let id = log(&fx.db, &fx.member, "2020-01-02T10:00:00Z")
            .expect("log")
            .expect("id");
        let now = Utc::now().to_rfc3339();
        assert!(
            fx.db
                .update_entry(&id, "", 60, "", &now, &[])
                .expect("update")
        );
        assert_eq!(
            fx.db
                .entries_today(&fx.member, &Local)
                .expect("today")
                .len(),
            1
        );
        assert!(fx.db.delete_entry(&id).expect("delete"));
        assert!(
            fx.db
                .entries_today(&fx.member, &Local)
                .expect("today")
                .is_empty()
        );
        assert!(fx.db.account_entries(&fx.member).expect("all").is_empty());
        assert!(fx.db.entry(&id).expect("read").is_none());
    }

    #[test]
    fn day_cap_should_refuse_the_admin_too_past_its_entries_of_one_day() {
        let fx = fixture();
        let day = "2020-01-02T10:00:00Z";
        for _ in 0..MAX_ENTRIES_PER_DAY {
            log(&fx.db, &fx.admin, day).expect("under the cap");
        }
        assert_eq!(
            refused_bound(log(&fx.db, &fx.admin, day)),
            Bound::EntriesPerDay
        );
        log(&fx.db, &fx.member, day).expect("each account has its own days");
        let moved = log(&fx.db, &fx.admin, "2020-01-03T10:00:00Z")
            .expect("another day")
            .expect("id");
        let refused = fx.db.update_entry(&moved, "", 60, "", day, &[]);
        let refusal = refused.expect_err("a full day");
        assert_eq!(
            refusal
                .downcast_ref::<QuotaExceeded>()
                .map(|refusal| refusal.bound),
            Some(Bound::EntriesPerDay)
        );
    }

    #[test]
    fn quota_should_refuse_a_member_past_its_time_entries() {
        const LIMIT: u32 = 3;
        let fx = fixture();
        fx.db
            .update_instance_settings(|settings| {
                settings.quotas = Quotas {
                    time_entries_per_account: LIMIT,
                    ..Quotas::default()
                };
            })
            .expect("settings");
        let now = Utc::now().to_rfc3339();
        for _ in 0..LIMIT {
            log(&fx.db, &fx.member, &now).expect("under the quota");
        }
        assert_eq!(
            refused_bound(log(&fx.db, &fx.member, &now)),
            Bound::Quota(Quota::TimeEntriesPerAccount)
        );
        for _ in 0..=LIMIT {
            log(&fx.db, &fx.admin, &now).expect("the admin has no quota");
        }
    }

    #[test]
    fn entry_indexes_should_be_built_when_a_version_15_file_is_upgraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("upgrade.redb");
        let path = path.to_str().expect("utf8");
        let member = {
            let db = Db::connect(path).expect("db");
            db.create_user("admin").expect("admin");
            let member = db.create_user("bob").expect("member");
            log(&db, &member, &Utc::now().to_rfc3339()).expect("log");
            log(&db, &member, "2020-01-02T10:00:00Z").expect("log");
            let txn = db.db.begin_write().expect("write");
            txn.delete_multimap_table(ENTRIES_BY_ACCOUNT)
                .expect("drop index");
            txn.delete_multimap_table(ENTRIES_BY_DAY)
                .expect("drop index");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 15)
                .expect("stamp");
            txn.commit().expect("commit");
            member
        };
        let db = Db::connect(path).expect("upgrade from version 15");
        assert_eq!(db.account_entries(&member).expect("all").len(), 2);
        assert_eq!(db.entries_today(&member, &Local).expect("today").len(), 1);
    }
}
