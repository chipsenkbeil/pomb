use chrono::Utc;
use redb::WriteTransaction;

use super::agents::txn_remove_grants_of_board;
use super::membership::handle_owner_in;
use super::usage::{OverQuota, txn_board_bytes, txn_count_usage, txn_refuse_over_bytes};
use super::{
    ACTIVE_TIMER, BOARD_MEMBERS, BOARDS, BOARDS_BY_MEMBER, BOARDS_BY_OWNER, COLUMNS, Db, DbResult,
    MEMBERS_BY_BOARD, Reads, RoleDenied, TASKS, USERNAMES, USERS, index_in, mmap_insert,
    mmap_remove, read_in, txn_clear_timer_if, txn_mmap_get, txn_read_one, txn_refuse_over_quota,
    txn_store_board, write_one,
};
use crate::models::{
    AccountId, AccountStatus, ActiveTimer, BoardMemberRow, BoardRole, BoardRow, ColumnRow, Quota,
    TaskHead, UserRow,
};

/// Returns the key of [`BOARD_MEMBERS`] that `account`'s role on `board_id` sits under.
#[must_use]
fn member_key(board_id: &str, account: &str) -> String {
    format!("{board_id}|{account}")
}

/// One account on a board with the role it holds there.
#[derive(Clone, Debug)]
pub struct BoardMember {
    /// The account.
    pub user: UserRow,
    /// The role the account holds on the board.
    pub role: BoardRole,
}

/// Typed outcome for [`Db::set_board_member`].
#[derive(Clone, Debug)]
pub enum MemberSet {
    /// The account joined the board with its role, or holds it now.
    Set(BoardMember),
    /// Refused: the board does not exist.
    NoBoard,
    /// Refused: no active account has the handle.
    NoAccount,
    /// Refused: the account owns the board, and its role does not change.
    IsOwner,
    /// Refused: only the board's owner holds the owner role.
    OwnerRole,
}

/// Typed outcome for [`Db::change_board_owner`].
#[derive(Clone, Debug)]
pub enum OwnerChange {
    /// The account owns the board now, and the previous owner is a member.
    Changed(BoardMember),
    /// Refused: the board does not exist.
    NoBoard,
    /// Refused: no active account has the handle.
    NoAccount,
    /// Refused: the account owns the board already.
    IsOwner,
    /// Refused: the account holds no role on the board, and only a member may take it over.
    NotMember,
}

/// Returns the role `account` holds on board `board_id`, through ownership or a member row, read
/// inside an open transaction. It is absent when the board is gone or the account holds none.
pub(crate) fn role_in(
    txn: &impl Reads,
    account: &AccountId,
    board_id: &str,
) -> DbResult<Option<BoardRole>> {
    let Some(board) = read_in::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(None);
    };
    if board.owner_id == account.as_str() {
        return Ok(Some(BoardRole::Owner));
    }
    let key = member_key(board_id, account.as_str());
    Ok(read_in::<BoardMemberRow>(txn, BOARD_MEMBERS, &key)?.map(|row| row.role))
}

/// Returns the role `account` holds on the board that task `task_id` sits on, read inside an open
/// transaction.
pub(crate) fn task_role_in(
    txn: &impl Reads,
    account: &AccountId,
    task_id: &str,
) -> DbResult<Option<BoardRole>> {
    let Some(head) = read_in::<TaskHead>(txn, TASKS, task_id)? else {
        return Ok(None);
    };
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &head.column_id)? else {
        return Ok(None);
    };
    role_in(txn, account, &column.board_id)
}

/// Returns the accounts that hold a role on board `board_id`: its owner, then its members, read
/// inside an open write transaction.
pub(crate) fn txn_board_accounts(txn: &WriteTransaction, board_id: &str) -> DbResult<Vec<String>> {
    let mut accounts: Vec<String> = txn_read_one::<BoardRow>(txn, BOARDS, board_id)?
        .map(|board| board.owner_id)
        .into_iter()
        .collect();
    accounts.extend(txn_mmap_get(txn, MEMBERS_BY_BOARD, board_id)?);
    Ok(accounts)
}

/// Deletes every member row of board `board_id` inside an open write transaction, with the active
/// timer of each member that runs on one of `task_ids`.
pub(crate) fn txn_remove_members_of_board(
    txn: &WriteTransaction,
    board_id: &str,
    task_ids: &[String],
) -> DbResult<()> {
    for account in txn_mmap_get(txn, MEMBERS_BY_BOARD, board_id)? {
        txn_clear_timer_if(txn, &account, |task_id| {
            task_ids.iter().any(|id| id == task_id)
        })?;
        txn_unjoin(txn, board_id, &account)?;
    }
    Ok(())
}

/// Deletes every member row of `account` inside an open write transaction.
pub(crate) fn txn_remove_memberships_of(txn: &WriteTransaction, account: &str) -> DbResult<()> {
    for board_id in txn_mmap_get(txn, BOARDS_BY_MEMBER, account)? {
        txn_unjoin(txn, &board_id, account)?;
    }
    Ok(())
}

/// Returns whether `account` holds a role on a board it does not own, read inside an open
/// write transaction.
pub(crate) fn txn_has_memberships(txn: &WriteTransaction, account: &str) -> DbResult<bool> {
    Ok(!txn_mmap_get(txn, BOARDS_BY_MEMBER, account)?.is_empty())
}

/// Deletes the member row of `account` on `board_id` and its index entries inside an open write
/// transaction.
fn txn_unjoin(txn: &WriteTransaction, board_id: &str, account: &str) -> DbResult<bool> {
    let removed = txn
        .open_table(BOARD_MEMBERS)?
        .remove(member_key(board_id, account).as_str())?
        .is_some();
    mmap_remove(txn, MEMBERS_BY_BOARD, board_id, account)?;
    mmap_remove(txn, BOARDS_BY_MEMBER, account, board_id)?;
    Ok(removed)
}

impl Db {
    /// Returns every account on board `board_id` with its role, the owner first and the rest by
    /// handle.
    pub fn board_members(&self, board_id: &str) -> DbResult<Vec<BoardMember>> {
        let txn = self.db.begin_read()?;
        let Some(board) = read_in::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(Vec::new());
        };
        let mut members = Vec::new();
        if let Some(user) = read_in::<UserRow>(&txn, USERS, &board.owner_id)? {
            members.push(BoardMember {
                user,
                role: BoardRole::Owner,
            });
        }
        let mut joined = Vec::new();
        for account in index_in(&txn, MEMBERS_BY_BOARD, board_id)? {
            let key = member_key(board_id, &account);
            let Some(row) = read_in::<BoardMemberRow>(&txn, BOARD_MEMBERS, &key)? else {
                continue;
            };
            if let Some(user) = read_in::<UserRow>(&txn, USERS, &account)? {
                joined.push(BoardMember {
                    user,
                    role: row.role,
                });
            }
        }
        joined.sort_by(|a, b| a.user.username.cmp(&b.user.username));
        members.extend(joined);
        Ok(members)
    }

    /// Returns every board `account` joined without owning, with its role there, ordered by name
    /// (then id for stability).
    pub fn joined_boards(&self, account: &AccountId) -> DbResult<Vec<(BoardRow, BoardRole)>> {
        let txn = self.db.begin_read()?;
        let mut out = Vec::new();
        for id in index_in(&txn, BOARDS_BY_MEMBER, account.as_str())? {
            let Some(board) = read_in::<BoardRow>(&txn, BOARDS, &id)? else {
                continue;
            };
            let key = member_key(&id, account.as_str());
            if let Some(row) = read_in::<BoardMemberRow>(&txn, BOARD_MEMBERS, &key)? {
                out.push((board, row.role));
            }
        }
        out.sort_by(|a, b| (&a.0.name, &a.0.id).cmp(&(&b.0.name, &b.0.id)));
        Ok(out)
    }

    /// Gives the active account with handle `handle` the role `role` on board `board_id`, keeping
    /// the time it first joined when it holds a role there already. A viewer cannot run a timer,
    /// so the account's timer on a task of the board stops.
    pub fn set_board_member(
        &self,
        board_id: &str,
        handle: &str,
        role: BoardRole,
    ) -> DbResult<MemberSet> {
        if role == BoardRole::Owner {
            return Ok(MemberSet::OwnerRole);
        }
        let txn = self.db.begin_write()?;
        let Some(board) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(MemberSet::NoBoard);
        };
        let account = handle_owner_in(&txn.open_table(USERNAMES)?, handle)?;
        let user = match account {
            Some(id) => txn_read_one::<UserRow>(&txn, USERS, &id)?,
            None => None,
        };
        let Some(user) = user.filter(|user| user.status == AccountStatus::Active) else {
            return Ok(MemberSet::NoAccount);
        };
        if user.id == board.owner_id {
            return Ok(MemberSet::IsOwner);
        }
        let key = member_key(board_id, &user.id);
        let added_at = txn_read_one::<BoardMemberRow>(&txn, BOARD_MEMBERS, &key)?
            .map_or_else(|| Utc::now().timestamp(), |row| row.added_at);
        let row = BoardMemberRow {
            board_id: board_id.to_string(),
            account_id: user.id.clone(),
            role,
            added_at,
        };
        write_one(&txn, BOARD_MEMBERS, &key, &row)?;
        mmap_insert(&txn, MEMBERS_BY_BOARD, board_id, &user.id)?;
        mmap_insert(&txn, BOARDS_BY_MEMBER, &user.id, board_id)?;
        if role == BoardRole::Viewer {
            txn_clear_timer_on_board(&txn, &user.id, board_id)?;
        }
        txn.commit()?;
        Ok(MemberSet::Set(BoardMember { user, role }))
    }

    /// Hands board `board_id`, owned by `from`, to its member with handle `handle`. The previous
    /// owner stays on the board as a member, and the board's bytes move from the previous
    /// owner's usage to the new owner's. A quota the board would break for the new owner refuses
    /// the change with [`QuotaExceeded`](super::QuotaExceeded).
    pub fn change_board_owner(
        &self,
        board_id: &str,
        from: &AccountId,
        handle: &str,
    ) -> DbResult<OwnerChange> {
        let txn = self.db.begin_write()?;
        let board = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)?;
        let Some(mut board) = board.filter(|board| board.owner_id == from.as_str()) else {
            return Ok(OwnerChange::NoBoard);
        };
        let account = handle_owner_in(&txn.open_table(USERNAMES)?, handle)?;
        let user = match account {
            Some(id) => txn_read_one::<UserRow>(&txn, USERS, &id)?,
            None => None,
        };
        let Some(user) = user.filter(|user| user.status == AccountStatus::Active) else {
            return Ok(OwnerChange::NoAccount);
        };
        if user.id == board.owner_id {
            return Ok(OwnerChange::IsOwner);
        }
        let key = member_key(board_id, &user.id);
        if txn_read_one::<BoardMemberRow>(&txn, BOARD_MEMBERS, &key)?.is_none() {
            return Ok(OwnerChange::NotMember);
        }
        board.config_json = board.config_with_feeds(&[]);
        txn_store_board(&txn, &board, OverQuota::Allow)?;
        let bytes = txn_board_bytes(&txn, board_id)?;
        let boards = txn_mmap_get(&txn, BOARDS_BY_OWNER, &user.id)?.len();
        txn_refuse_over_quota(&txn, &user.id, Quota::BoardsPerAccount, boards + 1)?;
        txn_refuse_over_bytes(&txn, &user.id, bytes, 0)?;
        let previous = std::mem::replace(&mut board.owner_id, user.id.clone());
        write_one(&txn, BOARDS, board_id, &board)?;
        mmap_remove(&txn, BOARDS_BY_OWNER, &previous, board_id)?;
        mmap_insert(&txn, BOARDS_BY_OWNER, &user.id, board_id)?;
        txn_count_usage(&txn, &previous, 0, bytes)?;
        txn_count_usage(&txn, &user.id, bytes, 0)?;
        txn_unjoin(&txn, board_id, &user.id)?;
        txn_remove_grants_of_board(&txn, board_id)?;
        let row = BoardMemberRow {
            board_id: board_id.to_string(),
            account_id: previous.clone(),
            role: BoardRole::Member,
            added_at: Utc::now().timestamp(),
        };
        write_one(&txn, BOARD_MEMBERS, &member_key(board_id, &previous), &row)?;
        mmap_insert(&txn, MEMBERS_BY_BOARD, board_id, &previous)?;
        mmap_insert(&txn, BOARDS_BY_MEMBER, &previous, board_id)?;
        txn.commit()?;
        Ok(OwnerChange::Changed(BoardMember {
            user,
            role: BoardRole::Owner,
        }))
    }

    /// Removes the member row of `account` on board `board_id`, with the account's active timer
    /// when it runs on a task of that board, and returns whether the account held one.
    pub fn remove_board_member(&self, board_id: &str, account: &AccountId) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let removed = txn_unjoin(&txn, board_id, account.as_str())?;
        if removed {
            txn_clear_timer_on_board(&txn, account.as_str(), board_id)?;
        }
        txn.commit()?;
        Ok(removed)
    }
}

/// Refuses with [`RoleDenied`] when `account` holds less than the member role on the board of task
/// `task_id`, inside an open write transaction, so a timer never starts on a board the account
/// only reads. A task that is gone is left to the caller.
pub(crate) fn txn_refuse_timer_below_member(
    txn: &WriteTransaction,
    account: &AccountId,
    task_id: &str,
) -> DbResult<()> {
    let Some(head) = read_in::<TaskHead>(txn, TASKS, task_id)? else {
        return Ok(());
    };
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &head.column_id)? else {
        return Ok(());
    };
    match role_in(txn, account, &column.board_id)? {
        Some(role) if role >= BoardRole::Member => Ok(()),
        _ => Err(Box::new(RoleDenied {
            board_id: column.board_id,
            needs: BoardRole::Member,
        })),
    }
}

/// Drops `account`'s active timer when it runs on a task of board `board_id`, inside an open write
/// transaction.
fn txn_clear_timer_on_board(txn: &WriteTransaction, account: &str, board_id: &str) -> DbResult<()> {
    let timer: Option<ActiveTimer> = txn_read_one(txn, ACTIVE_TIMER, account)?;
    let Some(task_id) = timer.and_then(|timer| timer.task_id) else {
        return Ok(());
    };
    let Some(head) = txn_read_one::<TaskHead>(txn, TASKS, &task_id)? else {
        return Ok(());
    };
    let column = txn_read_one::<ColumnRow>(txn, COLUMNS, &head.column_id)?;
    if column.is_some_and(|column| column.board_id == board_id) {
        txn.open_table(ACTIVE_TIMER)?.remove(account)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use redb::ReadableTableMetadata;

    use super::*;
    use crate::db::AdminActionOutcome;
    use crate::models::TimerMode;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Db,
        owner: AccountId,
        friend: AccountId,
        board: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sharing.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.create_user("alice").expect("alice");
        let friend = db.create_user("bob").expect("bob");
        let board = db.create_board(&owner, "Shared").expect("board");
        db.create_board(&owner, "Other").expect("board");
        Fixture {
            _dir: dir,
            db,
            owner,
            friend,
            board,
        }
    }

    fn rows_left(db: &Db) -> (u64, u64, u64) {
        let txn = db.db.begin_read().expect("read");
        let table = txn.open_table(BOARD_MEMBERS).expect("table");
        let by_board = txn.open_multimap_table(MEMBERS_BY_BOARD).expect("index");
        let by_member = txn.open_multimap_table(BOARDS_BY_MEMBER).expect("index");
        (
            ReadableTableMetadata::len(&table).expect("len"),
            ReadableTableMetadata::len(&by_board).expect("len"),
            ReadableTableMetadata::len(&by_member).expect("len"),
        )
    }

    fn row_of(fx: &Fixture) -> BoardMemberRow {
        let txn = fx.db.db.begin_read().expect("read");
        let key = member_key(&fx.board, fx.friend.as_str());
        read_in(&txn, BOARD_MEMBERS, &key)
            .expect("read")
            .expect("row")
    }

    #[test]
    fn set_board_member_should_store_the_role_and_list_the_board_as_joined() {
        let fx = fixture();
        let outcome = fx
            .db
            .set_board_member(&fx.board, "BOB", BoardRole::Member)
            .expect("set");
        assert!(matches!(outcome, MemberSet::Set(ref member) if member.role == BoardRole::Member));
        let joined = fx.db.joined_boards(&fx.friend).expect("joined");
        assert_eq!(joined.len(), 1);
        assert_eq!(
            (joined[0].0.id.as_str(), joined[0].1),
            (fx.board.as_str(), BoardRole::Member)
        );
        assert!(fx.db.joined_boards(&fx.owner).expect("joined").is_empty());
        let roles: Vec<_> = fx
            .db
            .board_members(&fx.board)
            .expect("members")
            .into_iter()
            .map(|member| (member.user.username, member.role))
            .collect();
        assert_eq!(
            roles,
            [
                ("alice".to_string(), BoardRole::Owner),
                ("bob".to_string(), BoardRole::Member)
            ]
        );
    }

    #[test]
    fn set_board_member_should_change_the_role_and_keep_the_join_time() {
        let fx = fixture();
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Member)
            .expect("set");
        let aged = BoardMemberRow {
            added_at: 0,
            ..row_of(&fx)
        };
        let txn = fx.db.db.begin_write().expect("write");
        let key = member_key(&fx.board, fx.friend.as_str());
        write_one(&txn, BOARD_MEMBERS, &key, &aged).expect("write");
        txn.commit().expect("commit");
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Viewer)
            .expect("set");
        let second = row_of(&fx);
        assert_eq!((second.role, second.added_at), (BoardRole::Viewer, 0));
        assert_eq!(rows_left(&fx.db), (1, 1, 1));
    }

    #[test]
    fn set_board_member_should_refuse_the_owner_an_unknown_handle_and_the_owner_role() {
        let fx = fixture();
        let set = |handle: &str, role| {
            fx.db
                .set_board_member(&fx.board, handle, role)
                .expect("set")
        };
        assert!(matches!(
            set("alice", BoardRole::Member),
            MemberSet::IsOwner
        ));
        assert!(matches!(
            set("nobody", BoardRole::Member),
            MemberSet::NoAccount
        ));
        assert!(matches!(set("bob", BoardRole::Owner), MemberSet::OwnerRole));
        let missing = fx
            .db
            .set_board_member("no-such-board", "bob", BoardRole::Member)
            .expect("set");
        assert!(matches!(missing, MemberSet::NoBoard));
        assert_eq!(rows_left(&fx.db), (0, 0, 0));
    }

    #[test]
    fn remove_board_member_should_drop_the_role_and_report_a_stranger() {
        let fx = fixture();
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Viewer)
            .expect("set");
        assert!(
            fx.db
                .remove_board_member(&fx.board, &fx.friend)
                .expect("remove")
        );
        assert!(
            !fx.db
                .remove_board_member(&fx.board, &fx.friend)
                .expect("remove")
        );
        assert_eq!(rows_left(&fx.db), (0, 0, 0));
    }

    #[test]
    fn deleting_a_board_should_remove_its_member_rows() {
        let fx = fixture();
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Member)
            .expect("set");
        fx.db.delete_board(&fx.board).expect("delete");
        assert_eq!(rows_left(&fx.db), (0, 0, 0));
        assert!(fx.db.joined_boards(&fx.friend).expect("joined").is_empty());
    }

    #[test]
    fn deleting_a_member_account_should_remove_its_member_rows() {
        let fx = fixture();
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Member)
            .expect("set");
        let outcome = fx.db.delete_member(fx.friend.as_str()).expect("delete");
        assert!(matches!(outcome, AdminActionOutcome::Done(_)));
        assert_eq!(rows_left(&fx.db), (0, 0, 0));
        assert_eq!(fx.db.board_members(&fx.board).expect("members").len(), 1);
    }

    fn run_timer_on(fx: &Fixture, board: &str) {
        let column = fx.db.create_column(board, "Todo", None).expect("column");
        let task = fx
            .db
            .create_task(&column, None, "Timed", 1, None)
            .expect("task");
        let timer = ActiveTimer {
            task_id: Some(task),
            mode: TimerMode::Stopwatch,
            started_at: 0,
            duration_secs: None,
        };
        fx.db.set_active_timer(&fx.friend, &timer).expect("timer");
    }

    #[test]
    fn remove_board_member_should_stop_the_timer_running_on_that_board() {
        let fx = fixture();
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Member)
            .expect("set");
        run_timer_on(&fx, &fx.board);
        fx.db
            .remove_board_member(&fx.board, &fx.friend)
            .expect("remove");
        assert!(fx.db.active_timer(&fx.friend).expect("timer").is_none());
    }

    #[test]
    fn remove_board_member_should_keep_a_timer_running_on_another_board() {
        let fx = fixture();
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Member)
            .expect("set");
        let other = fx.db.create_board(&fx.friend, "Mine").expect("board");
        run_timer_on(&fx, &other);
        fx.db
            .remove_board_member(&fx.board, &fx.friend)
            .expect("remove");
        assert!(fx.db.active_timer(&fx.friend).expect("timer").is_some());
    }

    #[test]
    fn deleting_a_board_should_stop_the_timer_of_its_members() {
        let fx = fixture();
        fx.db
            .set_board_member(&fx.board, "bob", BoardRole::Member)
            .expect("set");
        run_timer_on(&fx, &fx.board);
        fx.db.delete_board(&fx.board).expect("delete");
        assert!(fx.db.active_timer(&fx.friend).expect("timer").is_none());
    }

    #[test]
    fn deleting_the_owner_account_should_remove_the_member_rows_of_its_boards() {
        let fx = fixture();
        let other = fx.db.create_user("carol").expect("carol");
        let board = fx.db.create_board(&other, "Carol").expect("board");
        fx.db.create_board(&other, "Spare").expect("board");
        fx.db
            .set_board_member(&board, "bob", BoardRole::Viewer)
            .expect("set");
        let outcome = fx.db.delete_member(other.as_str()).expect("delete");
        assert!(matches!(outcome, AdminActionOutcome::Done(_)));
        assert_eq!(rows_left(&fx.db), (0, 0, 0));
        assert!(fx.db.joined_boards(&fx.friend).expect("joined").is_empty());
    }
}
