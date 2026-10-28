use std::collections::{BTreeSet, HashMap};

use redb::WriteTransaction;

use super::membership::txn_delete_account;
use super::{
    BOARDS, BOARDS_BY_OWNER, Db, DbResult, PRUNE_STATUS_KEY, SETTINGS, USERS, read_one, rows_in,
    txn_instance_settings, txn_mark_unrecorded_boards_used, txn_owner_boards, txn_read_one,
    txn_remove_board, write_one,
};
use crate::models::{
    AccountId, AccountRole, AccountStatus, BoardRow, InstanceMode, InstanceSettings, PruneNotice,
    PrunePolicy, PruneStatus, SECONDS_PER_DAY, UserRow,
};

impl Db {
    /// Most boards or accounts one write transaction of the cleanup deletes, so it never holds
    /// the writer for long.
    const PRUNE_BATCH_SIZE: usize = 50;

    /// Returns the outcome of the latest run of the unused-board cleanup, if there was one.
    pub fn prune_status(&self) -> DbResult<Option<PruneStatus>> {
        read_one(&self.db, SETTINGS, PRUNE_STATUS_KEY)
    }

    /// Deletes every board of an active member that went unused for longer than the prune
    /// settings allow at `now`, then every account the run left without a board unless it is the
    /// admin or pending, and stores and returns the run's outcome.
    ///
    /// Returns [`None`] and does nothing in single mode, while pruning is off, and within a day
    /// of the previous run. Boards without a recorded use count as used at `now`.
    pub fn prune_unused_boards(&self, now: i64) -> DbResult<Option<PruneStatus>> {
        let settings = self.instance_settings()?;
        if settings.mode != InstanceMode::Multi || !settings.prune.enabled {
            return Ok(None);
        }
        if self
            .prune_status()?
            .is_some_and(|last| now - last.ran_at < SECONDS_PER_DAY)
        {
            return Ok(None);
        }
        self.mark_unrecorded_boards_used(now)?;
        let (boards_deleted, mut owners) = self.prune_boards(&settings, now)?;
        owners.extend(self.boardless_members()?);
        let accounts_deleted = self.prune_empty_accounts(&owners, now)?;
        let status = PruneStatus {
            ran_at: now,
            boards_deleted,
            accounts_deleted,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, SETTINGS, PRUNE_STATUS_KEY, &status)?;
        txn.commit()?;
        Ok(Some(status))
    }

    /// Records every board without a recorded use as used at `now`.
    fn mark_unrecorded_boards_used(&self, now: i64) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_mark_unrecorded_boards_used(&txn, now)?;
        txn.commit()?;
        Ok(())
    }

    /// Deletes the boards unused at `now` under `settings` in batches, and returns how many it
    /// deleted and the accounts that owned them.
    fn prune_boards(
        &self,
        settings: &InstanceSettings,
        now: i64,
    ) -> DbResult<(u64, BTreeSet<String>)> {
        let candidates = self.unused_boards(settings, now)?;
        let mut deleted = 0;
        let mut owners = BTreeSet::new();
        for batch in candidates.chunks(Self::PRUNE_BATCH_SIZE) {
            let txn = self.db.begin_write()?;
            let mut removed = Vec::new();
            for id in batch {
                if let Some(board) = self.unused_board_in(&txn, id, now)? {
                    txn_remove_board(&txn, &board)?;
                    owners.insert(board.owner_id);
                    removed.push(id);
                }
            }
            txn.commit()?;
            for id in removed {
                self.forget_board_activity(id);
                deleted += 1;
            }
        }
        Ok((deleted, owners))
    }

    /// Returns board `id` when, read inside an open write transaction, its owner's policy deletes
    /// it at `now`.
    fn unused_board_in(
        &self,
        txn: &WriteTransaction,
        id: &str,
        now: i64,
    ) -> DbResult<Option<BoardRow>> {
        let Some(board) = txn_read_one::<BoardRow>(txn, BOARDS, id)? else {
            return Ok(None);
        };
        let Some(owner) = txn_read_one::<UserRow>(txn, USERS, &board.owner_id)? else {
            return Ok(None);
        };
        let Some(policy) = txn_instance_settings(txn)?.prune_policy(owner.role, owner.status)
        else {
            return Ok(None);
        };
        let due = self
            .last_used_in(txn, id)?
            .is_some_and(|at| policy.is_due(at, now));
        Ok(due.then_some(board))
    }

    /// Returns the ids of the boards that the policy of their owner deletes at `now`, as last
    /// read outside a write transaction.
    fn unused_boards(&self, settings: &InstanceSettings, now: i64) -> DbResult<Vec<String>> {
        let used = self.boards_last_used()?;
        let txn = self.db.begin_read()?;
        let policies: HashMap<String, PrunePolicy> =
            rows_in::<UserRow, _>(&txn.open_table(USERS)?)?
                .into_iter()
                .filter_map(|user| {
                    let policy = settings.prune_policy(user.role, user.status)?;
                    Some((user.id, policy))
                })
                .collect();
        Ok(rows_in::<BoardRow, _>(&txn.open_table(BOARDS)?)?
            .into_iter()
            .filter(|board| {
                policies
                    .get(&board.owner_id)
                    .zip(used.get(&board.id))
                    .is_some_and(|(policy, at)| policy.is_due(*at, now))
            })
            .map(|board| board.id)
            .collect())
    }

    /// Returns the ids of the active members that own no board.
    fn boardless_members(&self) -> DbResult<Vec<String>> {
        let txn = self.db.begin_read()?;
        let by_owner = txn.open_multimap_table(BOARDS_BY_OWNER)?;
        let mut members = Vec::new();
        for user in rows_in::<UserRow, _>(&txn.open_table(USERS)?)? {
            let member = user.role != AccountRole::Admin && user.status == AccountStatus::Active;
            if member && by_owner.get(user.id.as_str())?.is_empty() {
                members.push(user.id);
            }
        }
        Ok(members)
    }

    /// Deletes each account of `owners` that holds no board and that its owner's policy, applied
    /// to when it was last seen, also deletes at `now`, in batches, and returns how many it
    /// deleted. The admin and pending accounts stay, and so does an account still in use without
    /// a board, such as one that only runs the timer.
    fn prune_empty_accounts(&self, owners: &BTreeSet<String>, now: i64) -> DbResult<u64> {
        let owners: Vec<&String> = owners.iter().collect();
        let mut deleted = 0;
        for batch in owners.chunks(Self::PRUNE_BATCH_SIZE) {
            let txn = self.db.begin_write()?;
            let mut removed = Vec::new();
            for id in batch {
                let Some(user) = txn_read_one::<UserRow>(&txn, USERS, id)? else {
                    continue;
                };
                let seen = self.last_seen_in(&txn, id)?.unwrap_or(user.created_at);
                let idle = txn_instance_settings(&txn)?
                    .prune_policy(user.role, user.status)
                    .is_some_and(|policy| policy.is_due(seen, now));
                let kept = user.role == AccountRole::Admin
                    || user.status == AccountStatus::Pending
                    || !idle
                    || !txn_owner_boards(&txn, id)?.is_empty();
                if !kept {
                    txn_delete_account(&txn, &user)?;
                    removed.push(user.account_id());
                }
            }
            txn.commit()?;
            for account in removed {
                self.forget_activity(&account);
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Returns the warning each board of `owner` in `boards` shows at `now`, keyed by board id.
    pub(super) fn prune_notices(
        &self,
        owner: &AccountId,
        boards: &[String],
        now: i64,
    ) -> DbResult<HashMap<String, PruneNotice>> {
        let Some(user) = self.user(owner.as_str())? else {
            return Ok(HashMap::new());
        };
        let Some(policy) = self
            .instance_settings()?
            .prune_policy(user.role, user.status)
        else {
            return Ok(HashMap::new());
        };
        Ok(self
            .last_used_of(boards)?
            .into_iter()
            .filter_map(|(board, at)| policy.notice(at, now).map(|notice| (board, notice)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::db::AccountAction;
    use crate::models::{Actor, PruneSettings};

    /// Days past the default inactivity limit at which a board is pruned.
    const PRUNED_AFTER_DAYS: i64 = PruneSettings::DEFAULT_INACTIVE_DAYS as i64 + 1;
    /// Days a board went unused before a suspension was lifted.
    const SUSPENDED_DAYS: i64 = 200;
    /// Days pruning stayed off before the admin turned it on.
    const DAYS_OFF: i64 = 400;
    /// Seconds past a day boundary at which a run checks a limit it just crossed.
    const PAST_LIMIT_SECONDS: i64 = 1;

    /// An open database with its admin and a member.
    struct Fixture {
        /// Keeps the database directory alive.
        _dir: TempDir,
        /// The database.
        db: Db,
        /// The admin.
        admin: AccountId,
        /// The member.
        member: AccountId,
        /// Unix seconds right after the accounts were created, before any test board.
        start: i64,
    }

    fn fixture(mode: InstanceMode) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("prune.redb").to_str().expect("utf8")).expect("db");
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        db.apply_mode(mode).expect("mode");
        Fixture {
            start: Utc::now().timestamp(),
            _dir: dir,
            db,
            admin,
            member,
        }
    }

    fn days(n: i64) -> i64 {
        n * SECONDS_PER_DAY
    }

    fn boards_of(db: &Db, account: &AccountId) -> Vec<String> {
        db.owner_boards(account)
            .expect("boards")
            .into_iter()
            .map(|board| board.id)
            .collect()
    }

    fn exists(db: &Db, account: &AccountId) -> bool {
        db.user(account.as_str()).expect("read").is_some()
    }

    #[test]
    fn prune_should_delete_inactive_boards_then_empty_accounts() {
        let fx = fixture(InstanceMode::Multi);
        let used = fx.db.create_user("carol").expect("member");
        let newcomer = fx.db.create_user("dave").expect("member");
        fx.db.touch_account(&newcomer, fx.start);
        let idle = fx.db.create_board(&fx.member, "Idle").expect("board");
        let busy = fx.db.create_board(&used, "Busy").expect("board");
        let stale = fx.db.create_board(&used, "Stale").expect("board");

        let day_n = fx.start + days(PruneSettings::DEFAULT_INACTIVE_DAYS.into());
        assert_eq!(
            fx.db.prune_unused_boards(day_n).expect("prune"),
            Some(PruneStatus {
                ran_at: day_n,
                boards_deleted: 0,
                accounts_deleted: 0,
            }),
            "a board unused for exactly the limit stays"
        );

        fx.db.touch_board(&busy, day_n);
        let day_after = day_n + days(1) + PAST_LIMIT_SECONDS;
        let status = fx.db.prune_unused_boards(day_after).expect("prune");
        assert_eq!(
            status,
            Some(PruneStatus {
                ran_at: day_after,
                boards_deleted: 2,
                accounts_deleted: 2,
            })
        );
        assert!(fx.db.board(&idle).expect("read").is_none());
        assert!(fx.db.board(&stale).expect("read").is_none());
        assert!(fx.db.board(&busy).expect("read").is_some());
        assert!(!exists(&fx.db, &fx.member));
        assert!(exists(&fx.db, &used));
        assert!(
            !exists(&fx.db, &newcomer),
            "a member without a board goes once it is not seen past the limit"
        );
        assert_eq!(fx.db.prune_status().expect("status"), status);
        assert_eq!(
            fx.db
                .prune_unused_boards(day_after + PAST_LIMIT_SECONDS)
                .expect("prune"),
            None,
            "the cleanup runs at most once a day"
        );
    }

    #[test]
    fn prune_should_skip_single_mode() {
        let fx = fixture(InstanceMode::Single);
        let board = fx.db.create_board(&fx.member, "Member").expect("board");
        let admin_boards = boards_of(&fx.db, &fx.admin);
        let later = fx.start + days(PRUNED_AFTER_DAYS);
        assert_eq!(fx.db.prune_unused_boards(later).expect("prune"), None);
        assert_eq!(boards_of(&fx.db, &fx.admin), admin_boards);
        assert!(fx.db.board(&board).expect("read").is_some());
        assert_eq!(fx.db.prune_status().expect("status"), None);
    }

    #[test]
    fn prune_should_skip_admin() {
        let fx = fixture(InstanceMode::Multi);
        let kept = fx.db.create_board(&fx.admin, "Admin").expect("board");
        let gone = fx.db.create_board(&fx.member, "Member").expect("board");
        let admin_boards = boards_of(&fx.db, &fx.admin);
        assert!(admin_boards.contains(&kept));
        let later = fx.start + days(PRUNED_AFTER_DAYS);
        let status = fx.db.prune_unused_boards(later).expect("prune");
        assert_eq!(status.map(|s| s.boards_deleted), Some(1));
        assert_eq!(boards_of(&fx.db, &fx.admin), admin_boards);
        assert!(fx.db.board(&gone).expect("read").is_none());
        assert!(exists(&fx.db, &fx.admin));
    }

    #[test]
    fn prune_should_skip_suspended_accounts() {
        let fx = fixture(InstanceMode::Multi);
        let board = fx.db.create_board(&fx.member, "Kept").expect("board");
        fx.db
            .apply_account_action(fx.member.as_str(), AccountAction::Suspend)
            .expect("suspend");
        let later = fx.start + days(PRUNED_AFTER_DAYS);
        let status = fx.db.prune_unused_boards(later).expect("prune");
        assert_eq!(status.map(|s| s.boards_deleted), Some(0));
        assert!(fx.db.board(&board).expect("read").is_some());
        assert!(exists(&fx.db, &fx.member));
    }

    #[test]
    fn unsuspend_should_count_as_a_use_of_every_board() {
        let fx = fixture(InstanceMode::Multi);
        let board = fx.db.create_board(&fx.member, "Kept").expect("board");
        fx.db
            .apply_account_action(fx.member.as_str(), AccountAction::Suspend)
            .expect("suspend");
        fx.db
            .set_board_last_used(&board, fx.start - days(SUSPENDED_DAYS));
        fx.db
            .apply_account_action(fx.member.as_str(), AccountAction::Unsuspend)
            .expect("unsuspend");
        let next_day = Utc::now().timestamp() + days(1);
        let status = fx.db.prune_unused_boards(next_day).expect("prune");
        assert_eq!(status.map(|s| s.boards_deleted), Some(0));
        assert!(fx.db.board(&board).expect("read").is_some());
    }

    #[test]
    fn prune_should_skip_when_disabled() {
        let fx = fixture(InstanceMode::Multi);
        let board = fx.db.create_board(&fx.member, "Member").expect("board");
        fx.db
            .update_instance_settings(|settings| settings.prune.enabled = false)
            .expect("settings");
        let later = fx.start + days(PRUNED_AFTER_DAYS);
        assert_eq!(fx.db.prune_unused_boards(later).expect("prune"), None);
        assert!(fx.db.board(&board).expect("read").is_some());
        assert!(exists(&fx.db, &fx.member));
    }

    /// Asserts that `board`, unused long past every limit, is warned about but kept until a full
    /// warning period after `changed_at`, and deleted after it.
    fn assert_warning_period_after_change(fx: &Fixture, board: &str, changed_at: i64) {
        let warning = days(PruneSettings::DEFAULT_WARNING_DAYS.into());
        let boards = [board.to_string()];
        let notices = fx
            .db
            .prune_notices(&fx.member, &boards, changed_at)
            .expect("notices");
        assert!(
            notices.contains_key(board),
            "the banner shows from the change"
        );
        let within = changed_at + warning;
        let status = fx.db.prune_unused_boards(within).expect("prune");
        assert_eq!(status.map(|s| s.boards_deleted), Some(0));
        assert!(fx.db.board(board).expect("read").is_some());
        let after = within + days(1) + PAST_LIMIT_SECONDS;
        let status = fx.db.prune_unused_boards(after).expect("prune");
        assert_eq!(status.map(|s| s.boards_deleted), Some(1));
    }

    #[test]
    fn enabling_pruning_should_wait_a_warning_period() {
        let fx = fixture(InstanceMode::Multi);
        let board = fx.db.create_board(&fx.member, "Old").expect("board");
        fx.db
            .update_instance_settings(|settings| settings.prune.enabled = false)
            .expect("settings");
        let enabled_at = fx.start + days(DAYS_OFF);
        fx.db
            .update_instance_settings(|settings| {
                let prune = PruneSettings {
                    enabled: true,
                    ..settings.prune
                };
                settings.set_prune(prune, enabled_at);
            })
            .expect("settings");
        assert_warning_period_after_change(&fx, &board, enabled_at);
    }

    #[test]
    fn lowering_the_limit_should_wait_a_warning_period() {
        let fx = fixture(InstanceMode::Multi);
        let board = fx.db.create_board(&fx.member, "Old").expect("board");
        let lowered_at = fx.start + days(PruneSettings::DEFAULT_INACTIVE_DAYS.into()) - days(1);
        fx.db
            .update_instance_settings(|settings| {
                let prune = PruneSettings {
                    inactive_days: PruneSettings::MIN_INACTIVE_DAYS,
                    ..settings.prune
                };
                settings.set_prune(prune, lowered_at);
            })
            .expect("settings");
        assert_warning_period_after_change(&fx, &board, lowered_at);
    }

    #[test]
    fn prune_should_keep_an_account_that_still_has_a_used_board() {
        let fx = fixture(InstanceMode::Multi);
        let idle = fx.db.create_board(&fx.member, "Idle").expect("board");
        let later = fx.start + days(PRUNED_AFTER_DAYS);
        let used = fx.db.create_board(&fx.member, "Used").expect("board");
        fx.db.touch_board(&used, later);
        fx.db.prune_unused_boards(later).expect("prune");
        assert!(fx.db.board(&idle).expect("read").is_none());
        assert_eq!(boards_of(&fx.db, &fx.member), [used]);
    }

    #[test]
    fn prune_should_keep_an_account_seen_since_its_boards_went_unused() {
        let fx = fixture(InstanceMode::Multi);
        let idle = fx.db.create_board(&fx.member, "Idle").expect("board");
        let later = fx.start + days(PRUNED_AFTER_DAYS);
        fx.db.touch_account(&fx.member, later - days(1));
        let status = fx.db.prune_unused_boards(later).expect("prune");
        assert_eq!(
            status.map(|s| (s.boards_deleted, s.accounts_deleted)),
            Some((1, 0))
        );
        assert!(fx.db.board(&idle).expect("read").is_none());
        assert!(
            exists(&fx.db, &fx.member),
            "a member that only runs the timer keeps its account"
        );

        let soon = later + days(PRUNED_AFTER_DAYS) - days(2);
        let status = fx.db.prune_unused_boards(soon).expect("prune");
        assert_eq!(status.map(|s| s.accounts_deleted), Some(0));
        assert!(exists(&fx.db, &fx.member), "seen within the limit");

        let much_later = later + days(PRUNED_AFTER_DAYS);
        let status = fx.db.prune_unused_boards(much_later).expect("prune");
        assert_eq!(status.map(|s| s.accounts_deleted), Some(1));
        assert!(
            !exists(&fx.db, &fx.member),
            "a board-less member that stopped being seen goes in a later run"
        );
    }

    #[test]
    fn prune_should_remove_attachment_bytes() {
        let fx = fixture(InstanceMode::Multi);
        let board = fx.db.create_board(&fx.member, "Files").expect("board");
        let column = fx.db.create_column(&board, "Todo", None).expect("column");
        let task = fx
            .db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        let attachment = fx
            .db
            .add_attachment(&task, "a.txt", "text/plain", b"secret", Actor::SYSTEM)
            .expect("attach")
            .expect("task")
            .id;
        assert!(
            fx.db
                .read_attachment_data(&attachment)
                .expect("read")
                .is_some()
        );
        fx.db
            .prune_unused_boards(fx.start + days(PRUNED_AFTER_DAYS))
            .expect("prune");
        assert_eq!(fx.db.read_attachment_data(&attachment).expect("read"), None);
        assert_eq!(fx.db.attachment_data_count(), 0);
    }

    #[test]
    fn existing_boards_should_not_be_pruned_right_after_upgrade() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("upgrade.redb");
        let path = path.to_str().expect("utf8");
        let board = {
            let db = Db::connect(path).expect("db");
            db.create_user("admin").expect("admin");
            let member = db.create_user("bob").expect("member");
            db.apply_mode(InstanceMode::Multi).expect("mode");
            let board = db.create_board(&member, "Old").expect("board");
            db.rewind_to_schema_13();
            board
        };
        let next_day = Utc::now().timestamp() + days(1);
        let db = Db::connect(path).expect("upgrade from version 13");
        let status = db.prune_unused_boards(next_day).expect("prune");
        assert_eq!(status.map(|s| s.boards_deleted), Some(0));
        assert!(db.board(&board).expect("read").is_some());

        let past_limit = next_day + days(PRUNED_AFTER_DAYS);
        let status = db.prune_unused_boards(past_limit).expect("prune");
        assert_eq!(status.map(|s| s.boards_deleted), Some(1));
    }
}
