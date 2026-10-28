use std::collections::HashMap;
use std::mem;
use std::sync::{Mutex, MutexGuard};

use redb::{ReadableTable, TableDefinition, WriteTransaction};

use super::{
    ACTIVITY, BOARD_ACTIVITY, BOARDS, BOARDS_BY_OWNER, Db, DbResult, Rows, USERS, txn_mmap_get,
};
use crate::models::AccountId;

/// When each account was last seen or each board last used, recorded in memory at most once an
/// hour per id and written to the store in batches, so a request that only reads never writes.
#[derive(Debug, Default)]
pub(super) struct Activity {
    /// Id -> Unix seconds of its last recorded touch.
    seen: HashMap<String, i64>,
    /// Recorded touches not written yet.
    unsaved: HashMap<String, i64>,
}

impl Activity {
    /// Seconds between two recorded touches of one id.
    const INTERVAL_SECONDS: i64 = 3600;

    /// Records a touch of `id` at `now` unless one was recorded within the interval.
    fn touch(&mut self, id: &str, now: i64) {
        if self
            .seen
            .get(id)
            .is_some_and(|last| (0..Self::INTERVAL_SECONDS).contains(&(now - last)))
        {
            return;
        }
        self.seen.insert(id.to_string(), now);
        self.unsaved.insert(id.to_string(), now);
    }

    /// Puts back `touches` that could not be written, keeping a later touch recorded since.
    fn keep_unsaved(&mut self, touches: HashMap<String, i64>) {
        for (id, at) in touches {
            let kept = self.unsaved.entry(id).or_insert(at);
            *kept = (*kept).max(at);
        }
    }

    /// Drops every touch of `id`.
    fn forget(&mut self, id: &str) {
        self.seen.remove(id);
        self.unsaved.remove(id);
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

    /// Records that a request used board `board` at `now`, at most once an hour.
    pub fn touch_board(&self, board: &str, now: i64) {
        lock(&self.board_activity).touch(board, now);
    }

    /// Writes the recorded touches of accounts and boards in one transaction, never moving a
    /// stored time backwards, and forgets the touches of accounts and boards deleted since they
    /// were recorded. A failed write keeps every touch for the next flush.
    pub fn flush_activity(&self) -> DbResult<()> {
        let accounts = mem::take(&mut self.activity().unsaved);
        let boards = mem::take(&mut lock(&self.board_activity).unsaved);
        if accounts.is_empty() && boards.is_empty() {
            return Ok(());
        }
        match self.write_activity(&accounts, &boards) {
            Ok((gone_accounts, gone_boards)) => {
                let mut activity = self.activity();
                for account in &gone_accounts {
                    activity.forget(account);
                }
                let mut board_activity = lock(&self.board_activity);
                for board in &gone_boards {
                    board_activity.forget(board);
                }
                Ok(())
            }
            Err(error) => {
                self.activity().keep_unsaved(accounts);
                lock(&self.board_activity).keep_unsaved(boards);
                Err(error)
            }
        }
    }

    /// Writes `accounts` and `boards` in one transaction, and returns the ids of the accounts and
    /// boards that no longer exist.
    fn write_activity(
        &self,
        accounts: &HashMap<String, i64>,
        boards: &HashMap<String, i64>,
    ) -> DbResult<(Vec<String>, Vec<String>)> {
        let txn = self.db.begin_write()?;
        let gone_accounts = write_touches(&txn, accounts, USERS, ACTIVITY)?;
        let gone_boards = write_touches(&txn, boards, BOARDS, BOARD_ACTIVITY)?;
        txn.commit()?;
        Ok((gone_accounts, gone_boards))
    }

    /// Forgets every touch of `account`, which no longer exists.
    pub(super) fn forget_activity(&self, account: &AccountId) {
        self.activity().forget(account.as_str());
    }

    /// Forgets every touch of `board`, which no longer exists.
    pub(super) fn forget_board_activity(&self, board: &str) {
        lock(&self.board_activity).forget(board);
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
        merge_touches(&mut seen, &self.activity());
        Ok(seen)
    }

    /// Returns when each board was last used: the later of its stored time and a touch recorded
    /// since. A board missing from the map has no recorded use.
    pub fn boards_last_used(&self) -> DbResult<HashMap<String, i64>> {
        let mut used = HashMap::new();
        {
            let txn = self.db.begin_read()?;
            for item in txn.open_table(BOARD_ACTIVITY)?.iter()? {
                let (board, at) = item?;
                used.insert(board.value().to_string(), at.value());
            }
        }
        merge_touches(&mut used, &lock(&self.board_activity));
        Ok(used)
    }

    /// Returns when each board of `boards` was last used: the later of its stored time and a
    /// touch recorded since.
    pub(super) fn last_used_of(&self, boards: &[String]) -> DbResult<HashMap<String, i64>> {
        let txn = self.db.begin_read()?;
        let stored = txn.open_table(BOARD_ACTIVITY)?;
        let recorded = lock(&self.board_activity);
        let mut used = HashMap::new();
        for board in boards {
            let stored = stored.get(board.as_str())?.map(|at| at.value());
            if let Some(at) = stored.max(recorded.seen.get(board).copied()) {
                used.insert(board.clone(), at);
            }
        }
        Ok(used)
    }

    /// Returns when board `board` was last used, read inside an open write transaction: the later
    /// of its stored time and a touch recorded since.
    pub(super) fn last_used_in(
        &self,
        txn: &WriteTransaction,
        board: &str,
    ) -> DbResult<Option<i64>> {
        let stored = txn
            .open_table(BOARD_ACTIVITY)?
            .get(board)?
            .map(|at| at.value());
        Ok(stored.max(lock(&self.board_activity).seen.get(board).copied()))
    }

    /// Returns when `account` was last seen, read inside an open write transaction: the later of
    /// its stored time and a touch recorded since.
    pub(super) fn last_seen_in(
        &self,
        txn: &WriteTransaction,
        account: &str,
    ) -> DbResult<Option<i64>> {
        let stored = txn.open_table(ACTIVITY)?.get(account)?.map(|at| at.value());
        Ok(stored.max(self.activity().seen.get(account).copied()))
    }

    /// Returns the recorded touches of accounts, recovering them from a poisoned lock.
    fn activity(&self) -> MutexGuard<'_, Activity> {
        lock(&self.activity)
    }
}

/// Returns the recorded touches behind `activity`, recovering them from a poisoned lock.
fn lock(activity: &Mutex<Activity>) -> MutexGuard<'_, Activity> {
    activity.lock().unwrap_or_else(|e| e.into_inner())
}

/// Records every board of `owner` as used at `now`, inside an open write transaction, never moving
/// a stored time backwards.
pub(super) fn txn_mark_boards_used(txn: &WriteTransaction, owner: &str, now: i64) -> DbResult<()> {
    let mut stored = txn.open_table(BOARD_ACTIVITY)?;
    for board in txn_mmap_get(txn, BOARDS_BY_OWNER, owner)? {
        let at = stored.get(board.as_str())?.map(|at| at.value());
        stored.insert(board.as_str(), at.map_or(now, |at| at.max(now)))?;
    }
    Ok(())
}

/// Raises each stored time in `stored` to the touch `activity` recorded since, when later.
fn merge_touches(stored: &mut HashMap<String, i64>, activity: &Activity) {
    for (key, at) in &activity.seen {
        let time = stored.entry(key.clone()).or_insert(*at);
        *time = (*time).max(*at);
    }
}

/// Writes `touches` into `store` for every key that still has a row in `rows`, inside an open
/// write transaction, keeping a later stored time, and returns the keys that no longer have one.
fn write_touches(
    txn: &WriteTransaction,
    touches: &HashMap<String, i64>,
    rows: Rows,
    store: TableDefinition<'_, &str, i64>,
) -> DbResult<Vec<String>> {
    let mut gone = Vec::new();
    let rows = txn.open_table(rows)?;
    let mut store = txn.open_table(store)?;
    for (key, at) in touches {
        if rows.get(key.as_str())?.is_none() {
            gone.push(key.clone());
            continue;
        }
        let stored = store.get(key.as_str())?.map(|stored| stored.value());
        store.insert(key.as_str(), stored.map_or(*at, |stored| stored.max(*at)))?;
    }
    Ok(gone)
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
    fn flush_should_never_move_a_board_back_in_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("used.redb").to_str().expect("utf8")).expect("db");
        let owner = db.create_user("admin").expect("user");
        let board = db.create_board(&owner, "Board").expect("board");
        db.set_board_last_used(&board, NOW);

        db.touch_board(&board, NOW - Activity::INTERVAL_SECONDS);
        db.flush_activity().expect("flush");
        assert_eq!(db.stored_board_last_used(&board), Some(NOW));

        let later = NOW + Activity::INTERVAL_SECONDS;
        db.touch_board(&board, later);
        db.flush_activity().expect("flush");
        assert_eq!(db.stored_board_last_used(&board), Some(later));
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
