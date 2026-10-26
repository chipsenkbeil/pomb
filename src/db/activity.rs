use std::collections::HashMap;
use std::mem;
use std::sync::MutexGuard;

use redb::{ReadableDatabase, ReadableTable};

use super::{ACTIVITY, Db, DbResult, USERS};
use crate::models::AccountId;

/// When each account was last seen, recorded in memory at most once an hour per account and
/// written to the store in batches, so a request that only reads never writes.
#[derive(Debug, Default)]
pub(super) struct Activity {
    /// Account id -> Unix seconds of its last recorded touch.
    seen: HashMap<String, i64>,
    /// Recorded touches not written yet.
    unsaved: HashMap<String, i64>,
}

impl Activity {
    /// Seconds between two recorded touches of one account.
    const INTERVAL_SECONDS: i64 = 3600;

    /// Records a touch of `account` at `now` unless one was recorded within the interval.
    fn touch(&mut self, account: &str, now: i64) {
        if self
            .seen
            .get(account)
            .is_some_and(|last| (0..Self::INTERVAL_SECONDS).contains(&(now - last)))
        {
            return;
        }
        self.seen.insert(account.to_string(), now);
        self.unsaved.insert(account.to_string(), now);
    }

    /// Drops every touch of `account`.
    fn forget(&mut self, account: &str) {
        self.seen.remove(account);
        self.unsaved.remove(account);
    }
}

impl Db {
    /// Records that `account` made a request at `now`, at most once an hour. The ownerless id is
    /// never recorded.
    pub fn touch_account(&self, account: &AccountId, now: i64) {
        if !account.is_ownerless() {
            self.activity().touch(account.as_str(), now);
        }
    }

    /// Writes the recorded touches in one transaction, and forgets the touches of accounts
    /// deleted since they were recorded.
    pub fn flush_activity(&self) -> DbResult<()> {
        let unsaved = mem::take(&mut self.activity().unsaved);
        if unsaved.is_empty() {
            return Ok(());
        }
        let mut gone = Vec::new();
        let txn = self.db.begin_write()?;
        {
            let users = txn.open_table(USERS)?;
            let mut seen = txn.open_table(ACTIVITY)?;
            for (account, at) in &unsaved {
                if users.get(account.as_str())?.is_some() {
                    seen.insert(account.as_str(), *at)?;
                } else {
                    gone.push(account.as_str());
                }
            }
        }
        txn.commit()?;
        let mut activity = self.activity();
        for account in gone {
            activity.forget(account);
        }
        Ok(())
    }

    /// Forgets every touch of `account`, which no longer exists.
    pub(super) fn forget_activity(&self, account: &AccountId) {
        self.activity().forget(account.as_str());
    }

    /// Returns when each account was last seen: the later of its stored time and a touch recorded
    /// since.
    pub fn last_seen(&self) -> DbResult<HashMap<String, i64>> {
        let mut seen = HashMap::new();
        {
            let txn = self.db.begin_read()?;
            for item in txn.open_table(ACTIVITY)?.iter()? {
                let (account, at) = item?;
                seen.insert(account.value().to_string(), at.value());
            }
        }
        for (account, at) in &self.activity().seen {
            let stored = seen.entry(account.clone()).or_insert(*at);
            *stored = (*stored).max(*at);
        }
        Ok(seen)
    }

    /// Returns the recorded touches, recovering them from a poisoned lock.
    fn activity(&self) -> MutexGuard<'_, Activity> {
        self.activity.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn stored(db: &Db, account: &AccountId) -> Option<i64> {
        let txn = db.db.begin_read().expect("read");
        txn.open_table(ACTIVITY)
            .expect("table")
            .get(account.as_str())
            .expect("get")
            .map(|at| at.value())
    }

    #[test]
    fn last_seen_should_update_and_throttle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("seen.redb").to_str().expect("utf8")).expect("db");
        let account = db.create_user("admin").expect("user");

        db.touch_account(&account, NOW);
        assert_eq!(stored(&db, &account), None, "a touch alone never writes");
        assert_eq!(db.last_seen().expect("seen")[account.as_str()], NOW);
        db.flush_activity().expect("flush");
        assert_eq!(stored(&db, &account), Some(NOW));

        db.touch_account(&account, NOW + Activity::INTERVAL_SECONDS - 1);
        db.flush_activity().expect("flush");
        assert_eq!(
            stored(&db, &account),
            Some(NOW),
            "throttled within the hour"
        );
        assert_eq!(db.last_seen().expect("seen")[account.as_str()], NOW);

        let later = NOW + Activity::INTERVAL_SECONDS;
        db.touch_account(&account, later);
        assert_eq!(db.last_seen().expect("seen")[account.as_str()], later);
        db.flush_activity().expect("flush");
        assert_eq!(stored(&db, &account), Some(later));
    }

    #[test]
    fn interval_should_be_the_hour_the_docs_name() {
        assert_eq!(Activity::INTERVAL_SECONDS, 60 * 60);
    }

    #[test]
    fn deleted_account_should_be_forgotten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("gone.redb").to_str().expect("utf8")).expect("db");
        db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        db.touch_account(&member, NOW);
        db.delete_account(&member).expect("delete");
        assert!(!db.last_seen().expect("seen").contains_key(member.as_str()));
        db.flush_activity().expect("flush");
        assert_eq!(stored(&db, &member), None);

        let late = db.create_user("carol").expect("member");
        db.delete_account(&late).expect("delete");
        db.touch_account(&late, NOW);
        db.flush_activity().expect("flush");
        assert_eq!(stored(&db, &late), None);
        assert!(
            !db.last_seen().expect("seen").contains_key(late.as_str()),
            "a touch that lands after the delete is forgotten at the flush"
        );
    }
}
