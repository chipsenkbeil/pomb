use std::collections::HashMap;

use redb::{ReadableTable, WriteTransaction};

use super::outbox::{txn_queue_task_deleted, txn_queue_task_restored};
use super::usage::{OverQuota, txn_count_usage, txn_remove_entry, txn_store_entry, txn_write_task};
use super::{
    BIN, BIN_BY_BOARD, BOARDS, COLUMNS, COLUMNS_BY_BOARD, Commit, Db, DbResult, ENTRIES_BY_TASK,
    SETTINGS, SWIMLANES, SWIMLANES_BY_BOARD, TASK_COLORS, TASKS, TASKS_BY_COLUMN, TIME_ENTRIES,
    index_in, mmap_insert, mmap_remove, push_column_event, push_event, read_in, read_one,
    task_owner_in, txn_board_accounts, txn_clear_timer_if, txn_mmap_get, txn_read_one,
    txn_task_bytes, unlink_deleted_tasks, write_one,
};
use crate::models::{
    Actor, BIN_PERMANENT_DELETE_KEY, BinRow, BoardRow, ColorRow, ColumnRow, HistoryKind,
    SECONDS_PER_DAY, SwimlaneRow, TaskAttachment, TaskRow,
};

/// Key of the Unix second the cleanup of the recycle bin last ran, in [`SETTINGS`].
const BIN_PURGE_KEY: &str = "bin_purge";

/// What deleting a task from the recycle bin for good did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PurgeOutcome {
    /// The task, its time entries and its files are gone.
    Purged,
    /// No such task is in the bin.
    NotFound,
    /// The board does not allow deleting from its bin.
    Refused,
}

/// What restoring a task from the recycle bin did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The task is back in the column named, which is its old column or the board's first.
    Restored {
        column_id: String,
        column_name: String,
    },
    /// No such task is in the bin.
    NotFound,
    /// The task waited longer than the retention period, so only the cleanup can remove it.
    Expired,
}

/// Returns the bytes the account stores for `row`: its task row, its time entries and its
/// attachment files.
fn bin_bytes(row: &BinRow) -> DbResult<u64> {
    Ok(task_row_bytes(row)? + entries_bytes(row)? + files_bytes(row))
}

/// Returns the bytes of the task row of `row`.
fn task_row_bytes(row: &BinRow) -> DbResult<u64> {
    Ok(serde_json::to_vec(&row.task)?.len() as u64)
}

/// Returns the bytes of the time entries of `row`.
fn entries_bytes(row: &BinRow) -> DbResult<u64> {
    let mut bytes = 0;
    for entry in &row.entries {
        bytes += serde_json::to_vec(entry)?.len() as u64;
    }
    Ok(bytes)
}

/// Returns the bytes of the attachment files of `row`.
fn files_bytes(row: &BinRow) -> u64 {
    row.task
        .attachments
        .iter()
        .map(TaskAttachment::stored_bytes)
        .sum()
}

/// Removes the tasks in the recycle bin of `board_id` and returns their attachment ids and bytes.
pub(super) fn txn_remove_board_bin(
    txn: &WriteTransaction,
    board_id: &str,
) -> DbResult<(Vec<String>, u64)> {
    let mut attachment_ids = Vec::new();
    let mut freed = 0;
    for task_id in txn_mmap_get(txn, BIN_BY_BOARD, board_id)? {
        if let Some(row) = txn_read_one::<BinRow>(txn, BIN, &task_id)? {
            freed += bin_bytes(&row)?;
            attachment_ids.extend(attachment_ids_of(&row));
        }
        txn.open_table(BIN)?.remove(task_id.as_str())?;
    }
    txn.open_multimap_table(BIN_BY_BOARD)?
        .remove_all(board_id)?;
    Ok((attachment_ids, freed))
}

/// Adds the bytes of every task in the recycle bins to `usage`, by owning account.
pub(super) fn txn_bin_usage(
    txn: &WriteTransaction,
    usage: &mut HashMap<String, u64>,
) -> DbResult<()> {
    for item in txn.open_table(BIN)?.iter()? {
        let (_, raw) = item?;
        let row: BinRow = serde_json::from_slice(raw.value())?;
        *usage.entry(row.account_id.clone()).or_default() += bin_bytes(&row)?;
    }
    Ok(())
}

/// Returns the first column of `board_id` by position, read inside an open write transaction.
fn first_column(txn: &WriteTransaction, board_id: &str) -> DbResult<Option<ColumnRow>> {
    let mut first: Option<ColumnRow> = None;
    for id in txn_mmap_get(txn, COLUMNS_BY_BOARD, board_id)? {
        if let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &id)?
            && first
                .as_ref()
                .is_none_or(|held| column.position < held.position)
        {
            first = Some(column);
        }
    }
    Ok(first)
}

/// Returns the swimlane the restored task lands in: its old lane when it still exists, else the
/// board's first lane, else none.
fn restore_lane(
    txn: &WriteTransaction,
    board_id: &str,
    old: Option<&str>,
) -> DbResult<Option<String>> {
    let mut first: Option<SwimlaneRow> = None;
    for id in txn_mmap_get(txn, SWIMLANES_BY_BOARD, board_id)? {
        if let Some(lane) = read_in::<SwimlaneRow>(txn, SWIMLANES, &id)? {
            if old == Some(lane.id.as_str()) {
                return Ok(Some(lane.id));
            }
            if first
                .as_ref()
                .is_none_or(|held| lane.position < held.position)
            {
                first = Some(lane);
            }
        }
    }
    Ok(first.map(|lane| lane.id))
}

/// Moves the task `id` into the recycle bin in `txn` and returns its row, or [`None`] when the
/// task is unknown.
fn bin_task_in(txn: &WriteTransaction, id: &str, actor: Actor<'_>) -> DbResult<Option<BinRow>> {
    let Some(mut task) = txn_read_one::<TaskRow>(txn, TASKS, id)? else {
        return Ok(None);
    };
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &task.column_id)? else {
        return Ok(None);
    };
    let owner = task_owner_in(txn, id)?.unwrap_or_default();
    let entry_ids = txn_mmap_get(txn, ENTRIES_BY_TASK, id)?;
    let freed = txn_task_bytes(txn, &[id.to_string()])?;
    txn_queue_task_deleted(txn, &task, actor)?;
    unlink_deleted_tasks(txn, &[id.to_string()])?;
    txn_count_usage(txn, &owner, 0, freed)?;
    let mut entries = Vec::new();
    {
        txn.open_table(TASKS)?.remove(id)?;
        mmap_remove(txn, TASKS_BY_COLUMN, &task.column_id, id)?;
        let mut table = txn.open_table(TIME_ENTRIES)?;
        for entry_id in &entry_ids {
            entries.extend(txn_remove_entry(txn, &mut table, entry_id)?);
            mmap_remove(txn, ENTRIES_BY_TASK, id, entry_id)?;
        }
    }
    for account in txn_board_accounts(txn, &column.board_id)? {
        txn_clear_timer_if(txn, &account, |task_id| task_id == id)?;
    }
    task.links.clear();
    task.claim = None;
    let detail = format!("Task {} was deleted from column {}", task.name, column.name);
    push_event(&mut task, HistoryKind::Deleted, detail, actor);
    let row = BinRow {
        task,
        entries,
        board_id: column.board_id.clone(),
        account_id: owner.clone(),
        column_name: column.name,
        deleted_by: actor.name.to_string(),
        deleted_at: chrono::Utc::now().timestamp(),
    };
    txn_count_usage(txn, &owner, bin_bytes(&row)?, 0)?;
    write_one(txn, BIN, id, &row)?;
    mmap_insert(txn, BIN_BY_BOARD, &column.board_id, id)?;
    Ok(Some(row))
}

/// Takes `row` out of the recycle bin in `txn` and its bytes off its account.
fn unbin_in(txn: &WriteTransaction, row: &BinRow) -> DbResult<()> {
    txn_count_usage(txn, &row.account_id, 0, bin_bytes(row)?)?;
    txn.open_table(BIN)?.remove(row.task.id.as_str())?;
    mmap_remove(txn, BIN_BY_BOARD, &row.board_id, &row.task.id)?;
    Ok(())
}

impl Db {
    /// Moves a task, its time entries and its attachments into the recycle bin and takes it off
    /// the board, off every link to or from it and off the timer when it runs on it. Nothing is
    /// deleted: the task and its files stay until it is restored or [`crate::models::BIN_RETENTION_DAYS`] pass.
    /// Returns `false` when the task is unknown.
    pub fn delete_task(&self, id: &str, actor: Actor<'_>) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        if bin_task_in(&txn, id, actor)?.is_none() {
            return Ok(false);
        }
        txn.commit()?;
        Ok(true)
    }

    /// Moves each of `ids` into the recycle bin in one write transaction, as [`Db::delete_task`]
    /// does for one. A missing task leaves every task in place, and [`Commit::Discard`] ends the
    /// run without a commit. Returns `false` when a task is unknown.
    pub fn delete_tasks_logged(
        &self,
        ids: &[String],
        commit: Commit,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        for id in ids {
            if bin_task_in(&txn, id, actor)?.is_none() {
                return Ok(false);
            }
        }
        commit.finish(txn)?;
        Ok(true)
    }

    /// Removes a task its own creation takes back, with whatever reached it meanwhile, leaving
    /// nothing in the recycle bin. Returns `false` when the task is unknown.
    pub fn discard_task(&self, id: &str, actor: Actor<'_>) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(row) = bin_task_in(&txn, id, actor)? else {
            return Ok(false);
        };
        unbin_in(&txn, &row)?;
        txn.commit()?;
        self.files.remove(&attachment_ids_of(&row));
        Ok(true)
    }

    /// Returns the deleted tasks in the recycle bin of `board_id`, newest deletion first.
    pub fn binned_tasks(&self, board_id: &str) -> DbResult<Vec<BinRow>> {
        let txn = self.db.begin_read()?;
        let mut rows = Vec::new();
        for id in index_in(&txn, BIN_BY_BOARD, board_id)? {
            if let Some(row) = read_in::<BinRow>(&txn, BIN, &id)? {
                rows.push(row);
            }
        }
        rows.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then_with(|| a.task.id.cmp(&b.task.id))
        });
        Ok(rows)
    }

    /// Puts the deleted task `id` back, with its time entries, as of `now` in Unix seconds. It
    /// returns to its old column and swimlane, or to the board's first column and swimlane when
    /// they are gone, at the end of the cell.
    pub fn restore_task(&self, id: &str, now: i64, actor: Actor<'_>) -> DbResult<RestoreOutcome> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BinRow>(&txn, BIN, id)? else {
            return Ok(RestoreOutcome::NotFound);
        };
        if now >= row.expires_at() {
            return Ok(RestoreOutcome::Expired);
        }
        let old = read_in::<ColumnRow>(&txn, COLUMNS, &row.task.column_id)?
            .filter(|column| column.board_id == row.board_id);
        let column = match old {
            Some(column) => column,
            None => match first_column(&txn, &row.board_id)? {
                Some(column) => column,
                None => return Ok(RestoreOutcome::NotFound),
            },
        };
        let lane = restore_lane(&txn, &row.board_id, row.task.swimlane_id.as_deref())?;
        let color_gone = match row.task.color_id.as_deref() {
            Some(color) => read_in::<ColorRow>(&txn, TASK_COLORS, color)?
                .is_none_or(|found| found.board_id != row.board_id),
            None => false,
        };
        if color_gone {
            row.task.color_id = None;
        }
        let target_gone = match row.task.due_target_column_id.as_deref() {
            Some(target) => read_in::<ColumnRow>(&txn, COLUMNS, target)?
                .is_none_or(|found| found.board_id != row.board_id),
            None => false,
        };
        if target_gone {
            row.task.due_target_column_id = None;
        }
        let mut last: Option<f64> = None;
        for sibling_id in txn_mmap_get(&txn, TASKS_BY_COLUMN, &column.id)? {
            if let Some(sibling) = txn_read_one::<TaskRow>(&txn, TASKS, &sibling_id)?
                && sibling.swimlane_id == lane
            {
                last = Some(last.map_or(sibling.position, |held| held.max(sibling.position)));
            }
        }
        txn_count_usage(
            &txn,
            &row.account_id,
            0,
            task_row_bytes(&row)? + entries_bytes(&row)?,
        )?;
        let at = chrono::DateTime::from_timestamp(now, 0)
            .unwrap_or_default()
            .to_rfc3339();
        let detail = format!(
            "Task {} was restored to column {}",
            row.task.name, column.name
        );
        push_column_event(
            &mut row.task,
            HistoryKind::Restored,
            detail,
            actor,
            Some(&column.id),
        );
        let task = &mut row.task;
        task.column_id.clone_from(&column.id);
        task.swimlane_id = lane;
        task.position = last.map_or(0.0, |position| position + 1.0);
        task.column_entered_at = Some(at.clone());
        task.completed_at = column
            .is_done
            .then(|| task.completed_at.clone().unwrap_or_else(|| at.clone()));
        task.updated_at = Some(at);
        {
            let mut tasks = txn.open_table(TASKS)?;
            txn_write_task(&txn, &mut tasks, task, OverQuota::Allow, actor)?;
        }
        mmap_insert(&txn, TASKS_BY_COLUMN, &column.id, id)?;
        for entry in &row.entries {
            txn_store_entry(&txn, entry, OverQuota::Allow)?;
            mmap_insert(&txn, ENTRIES_BY_TASK, id, &entry.id)?;
        }
        txn.open_table(BIN)?.remove(id)?;
        mmap_remove(&txn, BIN_BY_BOARD, &row.board_id, id)?;
        txn_queue_task_restored(&txn, &row.task, actor)?;
        txn.commit()?;
        Ok(RestoreOutcome::Restored {
            column_id: column.id,
            column_name: column.name,
        })
    }

    /// Deletes the deleted task `id` for good when its board allows it, with its files, and takes
    /// its bytes off its account.
    pub fn purge_binned_task(&self, id: &str) -> DbResult<PurgeOutcome> {
        let purged = self.purge_bin_where(|txn, binned| {
            if binned.task.id != id {
                return Ok(false);
            }
            Ok(txn_read_one::<BoardRow>(txn, BOARDS, &binned.board_id)?
                .is_some_and(|board| board.config_bool(BIN_PERMANENT_DELETE_KEY)))
        })?;
        if purged > 0 {
            return Ok(PurgeOutcome::Purged);
        }
        let txn = self.db.begin_read()?;
        Ok(if read_in::<BinRow>(&txn, BIN, id)?.is_some() {
            PurgeOutcome::Refused
        } else {
            PurgeOutcome::NotFound
        })
    }

    /// Deletes the tasks of the recycle bin `pick` selects for good, their files with them, and
    /// takes their bytes off their accounts. Returns how many it deleted.
    fn purge_bin_where(
        &self,
        pick: impl Fn(&WriteTransaction, &BinRow) -> DbResult<bool>,
    ) -> DbResult<u64> {
        let txn = self.db.begin_write()?;
        let mut rows = Vec::new();
        for item in txn.open_table(BIN)?.iter()? {
            let (_, raw) = item?;
            let row: BinRow = serde_json::from_slice(raw.value())?;
            if pick(&txn, &row)? {
                rows.push(row);
            }
        }
        let mut attachment_ids = Vec::new();
        for row in &rows {
            attachment_ids.extend(attachment_ids_of(row));
            unbin_in(&txn, row)?;
        }
        txn.commit()?;
        self.files.remove(&attachment_ids);
        Ok(rows.len() as u64)
    }

    /// Deletes for good the deleted tasks that waited [`crate::models::BIN_RETENTION_DAYS`] in the bin, when
    /// the cleanup has not run for a day before `now`. Returns how many it deleted, [`None`]
    /// when it was not due.
    pub fn purge_bin(&self, now: i64) -> DbResult<Option<u64>> {
        let last: Option<i64> = read_one(&self.db, SETTINGS, BIN_PURGE_KEY)?;
        if last.is_some_and(|ran| now - ran < SECONDS_PER_DAY) {
            return Ok(None);
        }
        let purged = self.purge_bin_where(|_, row| Ok(now >= row.expires_at()))?;
        let txn = self.db.begin_write()?;
        write_one(&txn, SETTINGS, BIN_PURGE_KEY, &now)?;
        txn.commit()?;
        Ok(Some(purged))
    }
}

/// Returns the ids of the attachments of a binned task.
fn attachment_ids_of(row: &BinRow) -> Vec<String> {
    row.task
        .attachments
        .iter()
        .flat_map(|attachment| attachment.file_ids().map(str::to_string))
        .collect()
}
