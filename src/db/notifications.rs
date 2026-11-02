use chrono::Utc;
use redb::{ReadableTable, WriteTransaction};

use super::{
    ACCOUNT_SETTINGS, BOARDS, COLUMNS, Db, DbResult, MAX_HISTORY_TEXT_CHARS, NOTIFICATIONS,
    cut_text, read_in,
};
use crate::models::{
    AccountId, Actor, ActorKind, BoardRow, ColumnRow, NotificationKind, NotificationRow, Settings,
    TaskRow,
};

/// Most notifications an account keeps. Past it each new one drops the account's oldest.
pub const MAX_NOTIFICATIONS_PER_ACCOUNT: usize = 500;

/// Digits of a sequence number in a key, which sorts an account's notifications in order.
const SEQ_DIGITS: usize = 20;

/// A change that is not a task save, which notifies the account that owns what changed.
#[derive(Copy, Clone, Debug)]
pub enum Change<'a> {
    /// The task was deleted.
    TaskDeleted(&'a TaskRow),
    /// The column was renamed to `name`.
    ColumnRenamed {
        /// The column as it was stored before.
        column: &'a ColumnRow,
        /// The new name.
        name: &'a str,
    },
    /// The column was deleted.
    ColumnDeleted(&'a ColumnRow),
    /// The board was deleted.
    BoardDeleted(&'a BoardRow),
}

/// One page of an account's notifications, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct NotificationPage {
    /// Each notification with its sequence number.
    pub items: Vec<(u64, NotificationRow)>,
    /// The sequence number to pass as `before` for the next page, absent on the last page.
    pub next_before: Option<u64>,
    /// How many notifications the account has not read, across every page.
    pub unread: usize,
}

/// Returns the key of notification `seq` of `account`.
fn notification_key(account: &str, seq: u64) -> String {
    format!("{account}|{seq:0width$}", width = SEQ_DIGITS)
}

/// Returns the sequence number a key holds.
fn seq_of(key: &str) -> Option<u64> {
    key.rsplit_once('|')?.1.parse().ok()
}

/// Returns the first and last key an account's notifications can have.
fn account_keys(account: &str) -> (String, String) {
    (
        notification_key(account, 0),
        notification_key(account, u64::MAX),
    )
}

/// Returns whether a change by `kind` is one the account owner did not make in the browser, which
/// is the only change that notifies the owner.
fn is_another_party(kind: ActorKind) -> bool {
    kind == ActorKind::ApiToken
}

/// Returns whether `owner` follows `task`: a member of it, or watching it.
fn follows(owner: &str, task: &TaskRow) -> bool {
    task.watched || task.member_ids.iter().any(|member| member == owner)
}

/// Returns a notification of `kind` made by `actor` at `at`.
fn notification(
    kind: NotificationKind,
    board: &BoardRow,
    task: Option<&TaskRow>,
    detail: Option<&str>,
    actor: Actor<'_>,
    at: &str,
) -> NotificationRow {
    NotificationRow {
        kind,
        board_id: board.id.clone(),
        board_name: cut_text(&board.name, MAX_HISTORY_TEXT_CHARS),
        task_id: task.map(|task| task.id.clone()),
        task_name: task.map(|task| cut_text(&task.name, MAX_HISTORY_TEXT_CHARS)),
        detail: detail.map(|detail| cut_text(detail, MAX_HISTORY_TEXT_CHARS)),
        actor: actor.name.to_string(),
        actor_kind: actor.kind,
        token_name: actor.token_name.map(str::to_string),
        created_at: at.to_string(),
        read: false,
    }
}

/// Appends `rows` to `account`, inside an open write transaction, except the kinds the account
/// muted, and drops its oldest past [`MAX_NOTIFICATIONS_PER_ACCOUNT`].
fn txn_append(txn: &WriteTransaction, account: &str, rows: &[NotificationRow]) -> DbResult<()> {
    let muted = read_in::<Settings>(txn, ACCOUNT_SETTINGS, account)?
        .map(|settings| settings.muted_notifications)
        .unwrap_or_default();
    let rows: Vec<&NotificationRow> = rows
        .iter()
        .filter(|row| !muted.contains(&row.kind))
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let mut table = txn.open_table(NOTIFICATIONS)?;
    let (first, last) = account_keys(account);
    let mut seq = table
        .range(first.as_str()..=last.as_str())?
        .next_back()
        .transpose()?
        .and_then(|(key, _)| seq_of(key.value()))
        .unwrap_or(0);
    for row in rows {
        seq += 1;
        let raw = serde_json::to_vec(row)?;
        table.insert(notification_key(account, seq).as_str(), raw.as_slice())?;
        if let Some(dropped) = seq
            .checked_sub(MAX_NOTIFICATIONS_PER_ACCOUNT as u64)
            .filter(|dropped| *dropped > 0)
        {
            table.remove(notification_key(account, dropped).as_str())?;
        }
    }
    Ok(())
}

/// Writes the notifications the save of task `before` into `after` makes for the owner of its
/// board, inside the open write transaction that stores `after`. A task with no `before` is new.
pub(super) fn txn_record_task_notifications(
    txn: &WriteTransaction,
    before: Option<&TaskRow>,
    after: &TaskRow,
    actor: Actor<'_>,
) -> DbResult<()> {
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &after.column_id)? else {
        return Ok(());
    };
    let Some(board) = read_in::<BoardRow>(txn, BOARDS, &column.board_id)? else {
        return Ok(());
    };
    let owner = board.owner_id.as_str();
    if owner.is_empty() {
        return Ok(());
    }
    let at = Utc::now().to_rfc3339();
    let mut rows = Vec::new();
    if is_another_party(actor.kind) {
        let assigned = after.member_ids.iter().any(|member| member == owner)
            && !before.is_some_and(|before| before.member_ids.iter().any(|member| member == owner));
        if assigned {
            rows.push(notification(
                NotificationKind::TaskAssigned,
                &board,
                Some(after),
                None,
                actor,
                &at,
            ));
        }
        if follows(owner, after) && before.is_some_and(|before| before.column_id != after.column_id)
        {
            rows.push(notification(
                NotificationKind::TaskMoved,
                &board,
                Some(after),
                Some(&column.name),
                actor,
                &at,
            ));
        }
    }
    if before.is_some() && follows(owner, after) {
        for comment in after.comments.iter().filter(|comment| {
            is_another_party(comment.actor_kind)
                && !before.is_some_and(|before| before.comments.iter().any(|c| c.id == comment.id))
        }) {
            let author = Actor {
                name: &comment.author,
                token_name: comment.token_name.as_deref(),
                kind: comment.actor_kind,
            };
            rows.push(notification(
                NotificationKind::CommentAdded,
                &board,
                Some(after),
                None,
                author,
                &at,
            ));
        }
    }
    txn_append(txn, owner, &rows)
}

/// Removes every notification of `account`, inside an open write transaction.
pub(super) fn txn_remove_notifications(txn: &WriteTransaction, account: &str) -> DbResult<()> {
    let mut table = txn.open_table(NOTIFICATIONS)?;
    let (first, last) = account_keys(account);
    let keys = table
        .range(first.as_str()..=last.as_str())?
        .map(|entry| entry.map(|(key, _)| key.value().to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    for key in keys {
        table.remove(key.as_str())?;
    }
    Ok(())
}

/// Returns how many notifications of `account` are unread, inside an open write transaction.
fn txn_unread(txn: &WriteTransaction, account: &str) -> DbResult<usize> {
    let table = txn.open_table(NOTIFICATIONS)?;
    let (first, last) = account_keys(account);
    let mut unread = 0;
    for entry in table.range(first.as_str()..=last.as_str())? {
        let row: NotificationRow = serde_json::from_slice(entry?.1.value())?;
        unread += usize::from(!row.read);
    }
    Ok(unread)
}

impl Db {
    /// Records a notification for the board's owner when a token makes `change`. Nothing is
    /// recorded for a deleted task the owner does not follow.
    pub fn notify(&self, change: Change<'_>, actor: Actor<'_>) -> DbResult<()> {
        if !is_another_party(actor.kind) {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        let at = Utc::now().to_rfc3339();
        let (board, task, kind, detail) = match change {
            Change::TaskDeleted(task) => {
                let Some(column) = read_in::<ColumnRow>(&*txn, COLUMNS, &task.column_id)? else {
                    return Ok(());
                };
                let Some(board) = read_in::<BoardRow>(&*txn, BOARDS, &column.board_id)? else {
                    return Ok(());
                };
                if !follows(&board.owner_id, task) {
                    return Ok(());
                }
                (board, Some(task), NotificationKind::TaskDeleted, None)
            }
            Change::ColumnRenamed { column, name } => {
                let Some(board) = read_in::<BoardRow>(&*txn, BOARDS, &column.board_id)? else {
                    return Ok(());
                };
                (board, None, NotificationKind::ColumnRenamed, Some(name))
            }
            Change::ColumnDeleted(column) => {
                let Some(board) = read_in::<BoardRow>(&*txn, BOARDS, &column.board_id)? else {
                    return Ok(());
                };
                (
                    board,
                    None,
                    NotificationKind::ColumnDeleted,
                    Some(column.name.as_str()),
                )
            }
            Change::BoardDeleted(board) => {
                (board.clone(), None, NotificationKind::BoardDeleted, None)
            }
        };
        if board.owner_id.is_empty() {
            return Ok(());
        }
        let row = notification(kind, &board, task, detail, actor, &at);
        txn_append(&txn, &board.owner_id, &[row])?;
        txn.commit()
    }

    /// Returns up to `limit` notifications of `account`, newest first, older than the sequence
    /// number `before` when given.
    pub fn notifications(
        &self,
        account: &AccountId,
        before: Option<u64>,
        limit: usize,
    ) -> DbResult<NotificationPage> {
        let txn = self.read()?;
        let table = txn.open_table(NOTIFICATIONS)?;
        let (first, last) = account_keys(account.as_str());
        let mut items = Vec::new();
        let mut next_before = None;
        let mut unread = 0;
        for entry in table.range(first.as_str()..=last.as_str())?.rev() {
            let (key, value) = entry?;
            let Some(seq) = seq_of(key.value()) else {
                continue;
            };
            let row: NotificationRow = serde_json::from_slice(value.value())?;
            unread += usize::from(!row.read);
            if before.is_some_and(|before| seq >= before) {
                continue;
            }
            if items.len() < limit {
                items.push((seq, row));
            } else if next_before.is_none() {
                next_before = items.last().map(|(seq, _)| *seq);
            }
        }
        Ok(NotificationPage {
            items,
            next_before,
            unread,
        })
    }

    /// Marks notification `seq` of `account` read and returns how many remain unread, or [`None`]
    /// when the account has no such notification.
    pub fn mark_notification_read(&self, account: &AccountId, seq: u64) -> DbResult<Option<usize>> {
        let txn = self.db.begin_write()?;
        let key = notification_key(account.as_str(), seq);
        let row = {
            let table = txn.open_table(NOTIFICATIONS)?;
            let Some(guard) = table.get(key.as_str())? else {
                return Ok(None);
            };
            let mut row: NotificationRow = serde_json::from_slice(guard.value())?;
            row.read = true;
            row
        };
        txn.open_table(NOTIFICATIONS)?
            .insert(key.as_str(), serde_json::to_vec(&row)?.as_slice())?;
        let unread = txn_unread(&txn, account.as_str())?;
        txn.commit()?;
        Ok(Some(unread))
    }

    /// Marks every notification of `account` read.
    pub fn mark_all_notifications_read(&self, account: &AccountId) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(NOTIFICATIONS)?;
            let (first, last) = account_keys(account.as_str());
            let mut unread = Vec::new();
            for entry in table.range(first.as_str()..=last.as_str())? {
                let (key, value) = entry?;
                let mut row: NotificationRow = serde_json::from_slice(value.value())?;
                if !row.read {
                    row.read = true;
                    unread.push((key.value().to_string(), row));
                }
            }
            for (key, row) in unread {
                table.insert(key.as_str(), serde_json::to_vec(&row)?.as_slice())?;
            }
        }
        txn.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{MoveGuards, NewComment};
    use crate::models::{API_TOKEN_ACTOR, InstanceMode};

    const TOKEN: Actor<'static> = Actor {
        name: API_TOKEN_ACTOR,
        token_name: Some("ci"),
        kind: ActorKind::ApiToken,
    };

    const PERSON: Actor<'static> = Actor {
        name: "owner",
        token_name: None,
        kind: ActorKind::User,
    };

    /// An account with a board of a Todo and a Done column, and a second account.
    struct Fixture {
        _dir: tempfile::TempDir,
        db: Db,
        owner: AccountId,
        other: AccountId,
        board: String,
        todo: String,
        done: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let owner = db.create_user("owner").expect("owner");
        let other = db.create_user("other").expect("other");
        let board = db.create_board(&owner, "Work").expect("board");
        let todo = db.create_column(&board, "Todo", None).expect("column");
        let done = db.create_column(&board, "Done", None).expect("column");
        Fixture {
            _dir: dir,
            db,
            owner,
            other,
            board,
            todo,
            done,
        }
    }

    impl Fixture {
        fn followed(&self) -> String {
            let task = self
                .db
                .create_task(&self.todo, None, "Follow", 1, None)
                .expect("task");
            self.db
                .set_task_members(&task, &[self.owner.as_str().to_string()])
                .expect("members");
            task
        }

        fn comment(&self, task: &str, actor: Actor<'_>) {
            let new = NewComment {
                author: actor.name,
                body: "hello",
                actor_kind: actor.kind,
                token_name: actor.token_name,
                token_id: None,
                kind: None,
            };
            self.db
                .add_comment(task, new, actor, None)
                .expect("comment");
        }

        fn page(&self, before: Option<u64>, limit: usize) -> NotificationPage {
            self.db
                .notifications(&self.owner, before, limit)
                .expect("page")
        }

        fn board_row(&self) -> BoardRow {
            self.db.board(&self.board).expect("read").expect("board")
        }
    }

    fn seqs(page: &NotificationPage) -> Vec<u64> {
        page.items.iter().map(|(seq, _)| *seq).collect()
    }

    #[test]
    fn a_change_by_the_owner_in_the_browser_should_notify_no_one() {
        let fx = fixture();
        let task = fx.followed();
        fx.comment(&task, PERSON);
        fx.db
            .move_task_logged(&task, &fx.done, None, 0.0, MoveGuards::default(), PERSON)
            .expect("move");
        assert_eq!(fx.page(None, 10).items.len(), 0);
    }

    #[test]
    fn a_change_by_the_server_should_notify_no_one() {
        let fx = fixture();
        let task = fx.followed();
        fx.db
            .move_task_logged(
                &task,
                &fx.done,
                None,
                0.0,
                MoveGuards::default(),
                Actor::SYSTEM,
            )
            .expect("move");
        assert_eq!(fx.page(None, 10).items.len(), 0);
    }

    #[test]
    fn pages_should_run_newest_first_and_resume_from_the_cursor() {
        let fx = fixture();
        let task = fx.followed();
        for _ in 0..5 {
            fx.comment(&task, TOKEN);
        }
        let first = fx.page(None, 2);
        assert_eq!(seqs(&first), [5, 4]);
        assert_eq!((first.next_before, first.unread), (Some(4), 5));
        let second = fx.page(first.next_before, 2);
        assert_eq!(seqs(&second), [3, 2]);
        assert_eq!(second.next_before, Some(2));
        let last = fx.page(second.next_before, 2);
        assert_eq!(seqs(&last), [1]);
        assert_eq!(last.next_before, None);
    }

    #[test]
    fn marking_one_read_should_lower_the_unread_count_by_one() {
        let fx = fixture();
        let task = fx.followed();
        for _ in 0..3 {
            fx.comment(&task, TOKEN);
        }
        let unread = fx.db.mark_notification_read(&fx.owner, 2).expect("mark");
        assert_eq!(unread, Some(2));
        let page = fx.page(None, 10);
        let read: Vec<_> = page
            .items
            .iter()
            .map(|(seq, row)| (*seq, row.read))
            .collect();
        assert_eq!(read, [(3, false), (2, true), (1, false)]);
        assert_eq!(page.unread, 2);
        assert_eq!(
            fx.db.mark_notification_read(&fx.owner, 9).expect("mark"),
            None
        );
    }

    #[test]
    fn marking_all_read_should_clear_the_unread_count() {
        let fx = fixture();
        let task = fx.followed();
        for _ in 0..3 {
            fx.comment(&task, TOKEN);
        }
        fx.db
            .mark_all_notifications_read(&fx.owner)
            .expect("mark all");
        let page = fx.page(None, 10);
        assert_eq!(page.unread, 0);
        assert!(page.items.iter().all(|(_, row)| row.read));
    }

    #[test]
    fn another_account_should_see_and_mark_none_of_them() {
        let fx = fixture();
        let task = fx.followed();
        fx.comment(&task, TOKEN);
        let theirs = fx.db.notifications(&fx.other, None, 10).expect("page");
        assert_eq!((theirs.items.len(), theirs.unread), (0, 0));
        assert_eq!(
            fx.db.mark_notification_read(&fx.other, 1).expect("mark"),
            None
        );
        fx.db
            .mark_all_notifications_read(&fx.other)
            .expect("mark all");
        assert_eq!(fx.page(None, 10).unread, 1);
    }

    #[test]
    fn an_account_should_keep_only_its_newest_notifications() {
        let fx = fixture();
        let row = notification(
            NotificationKind::BoardDeleted,
            &fx.board_row(),
            None,
            None,
            TOKEN,
            "2026-01-01T00:00:00Z",
        );
        let rows = vec![row; MAX_NOTIFICATIONS_PER_ACCOUNT + 1];
        let txn = fx.db.db.begin_write().expect("write");
        txn_append(&txn, fx.owner.as_str(), &rows).expect("append");
        txn.commit().expect("commit");
        let page = fx.page(None, MAX_NOTIFICATIONS_PER_ACCOUNT + 1);
        assert_eq!(page.items.len(), MAX_NOTIFICATIONS_PER_ACCOUNT);
        assert_eq!(page.items.last().map(|(seq, _)| *seq), Some(2));
    }

    #[test]
    fn deleting_an_account_should_delete_its_notifications() {
        let fx = fixture();
        let board = fx.db.create_board(&fx.other, "Theirs").expect("board");
        let row = fx.db.board(&board).expect("read").expect("board");
        fx.db
            .notify(Change::BoardDeleted(&row), TOKEN)
            .expect("notify");
        let held = fx.db.notifications(&fx.other, None, 10).expect("page");
        assert_eq!(held.items.len(), 1);
        fx.db.delete_member(fx.other.as_str()).expect("delete");
        let left = fx.db.notifications(&fx.other, None, 10).expect("page");
        assert_eq!(left.items.len(), 0);
    }

    #[test]
    fn a_new_task_row_carrying_token_comments_should_record_no_comment_notification() {
        let fx = fixture();
        let task = fx.followed();
        fx.comment(&task, TOKEN);
        assert_eq!(fx.page(None, 10).items.len(), 1);
        let row = fx.db.task(&task).expect("read").expect("task");
        let txn = fx.db.db.begin_write().expect("write");
        txn_record_task_notifications(&txn, None, &row, TOKEN).expect("record");
        txn.commit().expect("commit");
        let comments = fx
            .page(None, 10)
            .items
            .iter()
            .filter(|(_, row)| row.kind == NotificationKind::CommentAdded)
            .count();
        assert_eq!(comments, 1);
    }

    #[test]
    fn a_muted_kind_should_not_be_recorded_for_that_account_only() {
        let fx = fixture();
        let task = fx.followed();
        let theirs = fx.db.create_board(&fx.other, "Theirs").expect("board");
        let theirs_row = fx.db.board(&theirs).expect("read").expect("board");
        let muted = Settings {
            muted_notifications: vec![
                NotificationKind::CommentAdded,
                NotificationKind::BoardDeleted,
            ],
            ..Settings::default()
        };
        fx.db.update_settings(&fx.owner, &muted).expect("settings");
        fx.comment(&task, TOKEN);
        fx.db
            .move_task_logged(&task, &fx.done, None, 0.0, MoveGuards::default(), TOKEN)
            .expect("move");
        fx.db
            .notify(Change::BoardDeleted(&theirs_row), TOKEN)
            .expect("notify");
        let kinds: Vec<NotificationKind> = fx
            .page(None, 10)
            .items
            .iter()
            .map(|(_, row)| row.kind)
            .collect();
        assert_eq!(kinds, vec![NotificationKind::TaskMoved]);
        let theirs_page = fx.db.notifications(&fx.other, None, 10).expect("page");
        assert_eq!(
            theirs_page.items.len(),
            1,
            "the other account muted nothing"
        );
    }
}
