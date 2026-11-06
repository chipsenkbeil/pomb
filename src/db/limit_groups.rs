use redb::WriteTransaction;
use uuid::Uuid;

use super::usage::OverQuota;
use super::{
    BOARDS, COLUMNS, COLUMNS_BY_BOARD, Db, DbResult, TASKS, TASKS_BY_COLUMN, txn_indexed_rows,
    txn_read_one, txn_store_board,
};
use crate::models::{BoardRow, ColumnRow, LimitGroup, MAX_LIMIT_GROUPS, TaskRow};

/// What a limit group write did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupWrite {
    /// Stored, carrying the group as stored.
    Saved(LimitGroup),
    /// The board does not exist.
    NoBoard,
    /// The board has no such group.
    NoGroup,
    /// Refused, with why.
    Refused(String),
}

/// What reordering a column did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColumnMove {
    /// The column moved and the columns are renumbered.
    Moved,
    /// The column does not exist.
    NotFound,
    /// Refused: the move would split a limit group's columns apart.
    BreaksGroup,
}

/// Returns the ids of a board's columns in board order.
pub(crate) fn txn_column_order(txn: &WriteTransaction, board_id: &str) -> DbResult<Vec<String>> {
    let mut columns: Vec<ColumnRow> = txn_indexed_rows(txn, COLUMNS_BY_BOARD, board_id, COLUMNS)?;
    columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
    Ok(columns.into_iter().map(|column| column.id).collect())
}

/// Returns the cards the group's columns hold and the group's limit when moving task `task_id`
/// from column `from` into column `to` would put the group over it. A move between two columns
/// of the same group adds nothing to it.
pub(crate) fn txn_group_over_limit(
    txn: &WriteTransaction,
    board_id: &str,
    task_id: &str,
    from: &str,
    to: &str,
) -> DbResult<Option<(usize, i64)>> {
    let Some(board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(None);
    };
    let Some(group) = board
        .limit_groups
        .iter()
        .find(|group| group.holds(to) && !group.holds(from))
    else {
        return Ok(None);
    };
    let mut count = 0;
    for column_id in &group.column_ids {
        let held: Vec<TaskRow> = txn_indexed_rows(txn, TASKS_BY_COLUMN, column_id, TASKS)?;
        count += held
            .iter()
            .filter(|other| other.id != task_id && other.is_work())
            .count();
    }
    Ok((count as i64 >= group.limit).then_some((count, group.limit)))
}

/// Takes column `column_id` out of its group, and drops the group when under two columns remain.
pub(crate) fn txn_leave_limit_groups(
    txn: &WriteTransaction,
    board_id: &str,
    column_id: &str,
) -> DbResult<()> {
    let Some(mut board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(());
    };
    if !board
        .limit_groups
        .iter()
        .any(|group| group.holds(column_id))
    {
        return Ok(());
    }
    for group in &mut board.limit_groups {
        group.column_ids.retain(|id| id != column_id);
    }
    board
        .limit_groups
        .retain(|group| group.column_ids.len() >= 2);
    txn_store_board(txn, &board, OverQuota::Allow)
}

/// Returns whether a board's columns in `order` leave a limit group split apart.
pub(crate) fn txn_order_splits_group(
    txn: &WriteTransaction,
    board_id: &str,
    order: &[&str],
) -> DbResult<bool> {
    let Some(board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(false);
    };
    Ok(board
        .limit_groups
        .iter()
        .any(|group| !group.is_adjacent_in(order)))
}

/// Puts the columns of each group of a board into the board order of `order`.
pub(crate) fn txn_sort_groups(
    txn: &WriteTransaction,
    board_id: &str,
    order: &[&str],
) -> DbResult<()> {
    let Some(mut board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(());
    };
    for group in &mut board.limit_groups {
        group
            .column_ids
            .sort_by_key(|id| order.iter().position(|column| column == id));
    }
    txn_store_board(txn, &board, OverQuota::Allow)
}

impl Db {
    /// Adds a limit group over `column_ids` with `limit` cards to a board, in one transaction,
    /// unless the board holds [`MAX_LIMIT_GROUPS`] or the columns are refused by
    /// [`LimitGroup::check`].
    pub fn add_limit_group(
        &self,
        board_id: &str,
        column_ids: Vec<String>,
        limit: i64,
    ) -> DbResult<GroupWrite> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(GroupWrite::NoBoard);
        };
        if row.limit_groups.len() >= MAX_LIMIT_GROUPS {
            return Ok(GroupWrite::Refused(format!(
                "a board holds at most {MAX_LIMIT_GROUPS} limit groups"
            )));
        }
        let order = txn_column_order(&txn, board_id)?;
        let ids: Vec<&str> = order.iter().map(String::as_str).collect();
        if let Err(why) = LimitGroup::check(&ids, &row.limit_groups, &column_ids, limit) {
            return Ok(GroupWrite::Refused(why));
        }
        let group = LimitGroup {
            id: Uuid::new_v4().to_string(),
            column_ids: in_board_order(column_ids, &ids),
            limit,
        };
        row.limit_groups.push(group.clone());
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(GroupWrite::Saved(group))
    }

    /// Changes the columns or the limit of group `group_id` of a board, in one transaction, and
    /// leaves what is `None` as it is.
    pub fn update_limit_group(
        &self,
        board_id: &str,
        group_id: &str,
        column_ids: Option<Vec<String>>,
        limit: Option<i64>,
    ) -> DbResult<GroupWrite> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(GroupWrite::NoBoard);
        };
        let Some(held) = row.limit_groups.iter().find(|group| group.id == group_id) else {
            return Ok(GroupWrite::NoGroup);
        };
        let column_ids = column_ids.unwrap_or_else(|| held.column_ids.clone());
        let limit = limit.unwrap_or(held.limit);
        let others: Vec<LimitGroup> = row
            .limit_groups
            .iter()
            .filter(|group| group.id != group_id)
            .cloned()
            .collect();
        let order = txn_column_order(&txn, board_id)?;
        let ids: Vec<&str> = order.iter().map(String::as_str).collect();
        if let Err(why) = LimitGroup::check(&ids, &others, &column_ids, limit) {
            return Ok(GroupWrite::Refused(why));
        }
        let changed = LimitGroup {
            id: group_id.to_string(),
            column_ids: in_board_order(column_ids, &ids),
            limit,
        };
        for group in &mut row.limit_groups {
            if group.id == group_id {
                *group = changed.clone();
            }
        }
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(GroupWrite::Saved(changed))
    }

    /// Unlinks group `group_id` of a board, which leaves its columns with their own limits.
    /// Returns `false` when the board has no such group.
    pub fn remove_limit_group(&self, board_id: &str, group_id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(false);
        };
        let before = row.limit_groups.len();
        row.limit_groups.retain(|group| group.id != group_id);
        if row.limit_groups.len() == before {
            return Ok(false);
        }
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(true)
    }

    /// Returns how many cards the columns of `group` hold together.
    pub fn limit_group_count(&self, group: &LimitGroup) -> DbResult<usize> {
        let mut count = 0;
        for column_id in &group.column_ids {
            count += self.count_tasks_in_column(column_id)?;
        }
        Ok(count)
    }
}

/// Returns `column_ids` sorted into the board order of `order`.
#[must_use]
fn in_board_order(mut column_ids: Vec<String>, order: &[&str]) -> Vec<String> {
    column_ids.sort_by_key(|id| order.iter().position(|column| column == id));
    column_ids
}
