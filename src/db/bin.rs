use std::collections::HashMap;

use redb::{ReadableTable, WriteTransaction};

use super::outbox::{txn_queue_task_deleted, txn_queue_task_restored};
use super::usage::{OverQuota, txn_count_usage, txn_remove_entry, txn_store_entry, txn_write_task};
use super::{
    BIN, BIN_BY_BOARD, BOARDS, COLUMNS, COLUMNS_BY_BOARD, Commit, Db, DbResult, ENTRIES_BY_TASK,
    LINKS_BY_TARGET, SETTINGS, SWIMLANES, SWIMLANES_BY_BOARD, TASK_COLORS, TASKS, TASKS_BY_COLUMN,
    TIME_ENTRIES, USERS, WipCheck, blocked_before, blocked_by_reaches, bump_flipped, index_in,
    link_event_detail, mmap_insert, mmap_remove, parent_chain_reaches, push_column_event,
    push_event, read_in, read_one, task_in, task_owner_in, txn_board_accounts,
    txn_board_task_count, txn_clear_timer_if, txn_mmap_get, txn_read_one,
    txn_refuse_board_over_quota, txn_task_bytes, txn_wip_refusal, unlink_deleted_tasks, write_one,
};
use crate::models::{
    Actor, BIN_PERMANENT_DELETE_KEY, BinLink, BinRow, BoardRow, ColorRow, ColumnRow, HistoryKind,
    LinkKind, Quota, SECONDS_PER_DAY, SwimlaneRow, TaskAttachment, TaskKind, TaskLink, TaskRow,
    UserRow,
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
pub(crate) fn txn_remove_board_bin(
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
pub(crate) fn txn_bin_usage(
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
    let links = links_around(txn, id, &task)?;
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
        links,
    };
    txn_count_usage(txn, &owner, bin_bytes(&row)?, 0)?;
    write_one(txn, BIN, id, &row)?;
    mmap_insert(txn, BIN_BY_BOARD, &column.board_id, id)?;
    Ok(Some(row))
}

/// Returns the links `task` holds and the links other tasks hold to it, read before they go.
fn links_around(txn: &WriteTransaction, id: &str, task: &TaskRow) -> DbResult<Vec<BinLink>> {
    let mut links: Vec<BinLink> = task
        .links
        .iter()
        .map(|link| BinLink {
            holder_id: id.to_string(),
            link: link.clone(),
        })
        .collect();
    for holder_id in txn_mmap_get(txn, LINKS_BY_TARGET, id)? {
        if holder_id == id {
            continue;
        }
        let Some(holder) = txn_read_one::<TaskRow>(txn, TASKS, &holder_id)? else {
            continue;
        };
        links.extend(
            holder
                .links
                .into_iter()
                .filter(|link| link.target_id == id)
                .map(|link| BinLink {
                    holder_id: holder_id.clone(),
                    link,
                }),
        );
    }
    Ok(links)
}

/// Hands `bin_link` to the deleted task `id` so its restore puts the link back, and drops it when
/// that task is gone.
fn defer_link(txn: &WriteTransaction, id: &str, bin_link: &BinLink) -> DbResult<()> {
    let Some(mut row) = txn_read_one::<BinRow>(txn, BIN, id)? else {
        return Ok(());
    };
    if row
        .links
        .iter()
        .any(|kept| kept.link.id == bin_link.link.id)
    {
        return Ok(());
    }
    row.links.push(bin_link.clone());
    write_one(txn, BIN, id, &row)
}

/// Puts back each of `links` whose holder and target are both on the board again and that
/// still fits the link rules, and skips the rest. A link whose other task is still in the bin
/// waits in that task's row.
fn restore_links_in(txn: &WriteTransaction, links: &[BinLink], actor: Actor<'_>) -> DbResult<()> {
    let mut tasks = txn.open_table(TASKS)?;
    for bin_link in links {
        let link = &bin_link.link;
        let holder_id = bin_link.holder_id.as_str();
        let holder = task_in(&tasks, holder_id)?;
        let target = task_in(&tasks, &link.target_id)?;
        let missing_id = if holder.is_none() {
            holder_id
        } else {
            link.target_id.as_str()
        };
        let (Some(mut holder), Some(target)) = (holder, target) else {
            defer_link(txn, missing_id, bin_link)?;
            continue;
        };
        let same = |held: &&TaskLink, target_id: &str| {
            held.kind == link.kind && held.target_id == target_id
        };
        let already_linked = holder.links.iter().any(|held| same(&held, &link.target_id))
            || (link.kind == LinkKind::Related
                && target.links.iter().any(|held| same(&held, holder_id)));
        let refused = match link.kind {
            LinkKind::BlockedBy => blocked_by_reaches(&tasks, &link.target_id, holder_id)?,
            LinkKind::Parent => {
                holder
                    .links
                    .iter()
                    .any(|held| held.kind == LinkKind::Parent)
                    || target.kind != TaskKind::Epic
                    || parent_chain_reaches(&tasks, &link.target_id, holder_id)?
            }
            _ => false,
        };
        if already_linked || refused {
            continue;
        }
        let detail = link_event_detail(link, Some(&target.name), |detail| {
            format!("Restored link {detail}")
        });
        holder.links.push(link.clone());
        push_event(&mut holder, HistoryKind::LinkAdded, detail, actor);
        holder.updated_at = Some(chrono::Utc::now().to_rfc3339());
        txn_write_task(txn, &mut tasks, &holder, OverQuota::Allow, actor)?;
        mmap_insert(txn, LINKS_BY_TARGET, &link.target_id, holder_id)?;
    }
    Ok(())
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
    /// they are gone, at the end of the cell. With `WipCheck::Enforce` a task whose column or
    /// limit group
    /// is at its limit is refused with a [`super::WipRefused`] error and stays in the bin.
    pub fn restore_task(
        &self,
        id: &str,
        now: i64,
        wip: WipCheck,
        actor: Actor<'_>,
    ) -> DbResult<RestoreOutcome> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BinRow>(&txn, BIN, id)? else {
            return Ok(RestoreOutcome::NotFound);
        };
        if now >= row.expires_at() {
            return Ok(RestoreOutcome::Expired);
        }
        let epics: Vec<String> = row
            .links
            .iter()
            .filter(|bin_link| bin_link.link.kind == LinkKind::Parent)
            .map(|bin_link| bin_link.link.target_id.clone())
            .collect();
        let before = blocked_before(&txn, &epics)?;
        let old = read_in::<ColumnRow>(&txn, COLUMNS, &row.task.column_id)?
            .filter(|column| column.board_id == row.board_id);
        let column = match old {
            Some(column) => column,
            None => match first_column(&txn, &row.board_id)? {
                Some(column) => column,
                None => return Ok(RestoreOutcome::NotFound),
            },
        };
        let held = txn_board_task_count(&txn, &row.board_id)?;
        txn_refuse_board_over_quota(&txn, &row.board_id, Quota::TasksPerBoard, held + 1)?;
        if wip == WipCheck::Enforce
            && let Some(refusal) = txn_wip_refusal(&txn, &row.task, &column, None)?
        {
            return Err(refusal.into());
        }
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
        restore_links_in(&txn, &row.links, actor)?;
        bump_flipped(&txn, &before)?;
        for entry in &row.entries {
            if read_in::<UserRow>(&txn, USERS, &entry.account_id)?.is_none() {
                continue;
            }
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

    /// Deletes every task in the recycle bin of board `board_id` for good, their files with them,
    /// whatever the board's `bin_permanent_delete` config says. Returns how many it deleted.
    pub fn purge_board_bin(&self, board_id: &str) -> DbResult<u64> {
        self.purge_bin_where(|_, row| Ok(row.board_id == board_id))
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::db::usage::txn_recount_usage;
    use crate::db::{ACCOUNT_USAGE, AccountDeleteOutcome, LinkAddOutcome, NewTimeEntry, mmap_get};
    use crate::models::{AccountId, BoardRole, EntryKind, InstanceMode};

    /// An open database with an admin who owns a task on a board and a member on that board.
    struct Fixture {
        /// Keeps the database directory alive.
        _dir: TempDir,
        /// The database.
        db: Db,
        /// The admin, who owns the board and the task.
        admin: AccountId,
        /// The member, who logs time on the task.
        member: AccountId,
        /// The task.
        task: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("bin.redb").to_str().expect("utf8")).expect("db");
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let board = db.create_board(&admin, "Board").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let task = db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        db.set_board_member(&board, "bob", BoardRole::Member)
            .expect("join");
        Fixture {
            _dir: dir,
            db,
            admin,
            member,
            task,
        }
    }

    /// Logs five minutes on `task` for `account` and returns the entry id.
    fn log(db: &Db, task: &str, account: &AccountId) -> String {
        db.create_entry_full(
            account,
            NewTimeEntry {
                task_id: Some(task),
                started_at: "2026-11-10T10:00:00+00:00",
                minutes: 5,
                seconds: 300,
                note: "",
                kind: EntryKind::Manual,
                interrupted: false,
                interrupt_reason: None,
                labels: &[],
                created_by: "",
                activity_id: None,
                role: None,
                token_name: None,
                role_id: None,
                role_name: None,
            },
        )
        .expect("entry")
        .expect("task")
    }

    /// Returns the bytes `account` stores, as kept.
    #[must_use]
    fn stored(db: &Db, account: &AccountId) -> u64 {
        db.stored_bytes_by_account()
            .expect("usage")
            .get(account.as_str())
            .copied()
            .unwrap_or_default()
    }

    /// Returns what every account stores as counted from scratch, leaving the kept counts as
    /// they were.
    #[must_use]
    fn recounted(db: &Db) -> HashMap<String, u64> {
        let txn = db.db.begin_write().expect("write");
        txn_recount_usage(&txn).expect("recount");
        let mut usage = HashMap::new();
        for item in txn
            .open_table(ACCOUNT_USAGE)
            .expect("table")
            .iter()
            .expect("iter")
        {
            let (account, bytes) = item.expect("row");
            usage.insert(account.value().to_string(), bytes.value());
        }
        txn.abort().expect("abort");
        usage
    }

    /// Asserts that the kept counts match a count from scratch, ignoring accounts at zero.
    fn assert_counts_match(db: &Db) {
        let mut kept = db.stored_bytes_by_account().expect("usage");
        kept.retain(|_, bytes| *bytes > 0);
        assert_eq!(kept, recounted(db));
    }

    /// Returns the sorted ids of the time entries on the task.
    #[must_use]
    fn entry_ids(db: &Db, task: &str) -> Vec<String> {
        let mut ids: Vec<String> = db
            .list_entries(task)
            .expect("entries")
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        ids.sort();
        ids
    }

    /// Restores the deleted task as of now.
    fn restore(fx: &Fixture) {
        let outcome = fx
            .db
            .restore_task(
                &fx.task,
                Utc::now().timestamp(),
                WipCheck::Force,
                Actor::SYSTEM,
            )
            .expect("restore");
        assert!(matches!(outcome, RestoreOutcome::Restored { .. }));
    }

    #[test]
    fn restoring_should_drop_the_entries_of_a_deleted_account() {
        let fx = fixture();
        let kept = log(&fx.db, &fx.task, &fx.admin);
        let dropped = log(&fx.db, &fx.task, &fx.member);
        assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
        assert!(matches!(
            fx.db.delete_account(&fx.member).expect("delete account"),
            AccountDeleteOutcome::Deleted { .. }
        ));
        restore(&fx);
        assert_eq!(entry_ids(&fx.db, &fx.task), vec![kept]);
        assert!(fx.db.entry(&dropped).expect("entry").is_none());
        assert_eq!(stored(&fx.db, &fx.member), 0);
        assert_counts_match(&fx.db);
    }

    #[test]
    fn restoring_should_keep_the_entries_of_live_accounts() {
        let fx = fixture();
        let admin_entry = log(&fx.db, &fx.task, &fx.admin);
        let member_entry = log(&fx.db, &fx.task, &fx.member);
        assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
        restore(&fx);
        let mut expected = vec![admin_entry, member_entry];
        expected.sort();
        assert_eq!(entry_ids(&fx.db, &fx.task), expected);
        assert!(stored(&fx.db, &fx.member) > 0);
        assert_counts_match(&fx.db);
    }

    /// Makes a second task in the column of the fixture's task and returns its id.
    fn other_task(fx: &Fixture, name: &str) -> String {
        let column = fx.db.task(&fx.task).expect("read").expect("task").column_id;
        fx.db
            .create_task(&column, None, name, 1, None)
            .expect("task")
    }

    /// Returns the sorted kind and target of every link `task` shows.
    #[must_use]
    fn links_of(db: &Db, task: &str) -> Vec<(LinkKind, String)> {
        let mut links: Vec<_> = db
            .list_task_links(task)
            .expect("links")
            .expect("task")
            .into_iter()
            .map(|link| (link.kind, link.target_id))
            .collect();
        links.sort_by(|a, b| a.1.cmp(&b.1));
        links
    }

    #[test]
    fn restoring_should_bring_back_the_links_in_both_directions() {
        let fx = fixture();
        let blocker = other_task(&fx, "Blocker");
        let blocked = other_task(&fx, "Blocked");
        let link = |task: &str, kind, target: &str| {
            fx.db
                .add_task_link(task, kind, target, None, Actor::SYSTEM)
                .expect("link")
        };
        link(&fx.task, LinkKind::BlockedBy, &blocker);
        link(&blocked, LinkKind::BlockedBy, &fx.task);
        let before = links_of(&fx.db, &fx.task);
        assert_eq!(before.len(), 2);
        assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
        restore(&fx);
        assert_eq!(links_of(&fx.db, &fx.task), before);
        assert_eq!(
            links_of(&fx.db, &blocked),
            vec![(LinkKind::BlockedBy, fx.task.clone())]
        );
        assert_eq!(
            links_of(&fx.db, &blocker),
            vec![(LinkKind::Blocks, fx.task.clone())]
        );
    }

    #[test]
    fn restoring_should_drop_a_link_to_a_purged_task() {
        let fx = fixture();
        let gone = other_task(&fx, "Gone");
        let kept = other_task(&fx, "Kept");
        for target in [&gone, &kept] {
            fx.db
                .add_task_link(&fx.task, LinkKind::Related, target, None, Actor::SYSTEM)
                .expect("link");
        }
        assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
        assert!(fx.db.delete_task(&gone, Actor::SYSTEM).expect("delete"));
        let purged = fx
            .db
            .purge_bin_where(|_, row| Ok(row.task.id == gone))
            .expect("purge");
        assert_eq!(purged, 1);
        restore(&fx);
        assert_eq!(
            links_of(&fx.db, &fx.task),
            vec![(LinkKind::Related, kept.clone())]
        );
        assert_eq!(
            links_of(&fx.db, &kept),
            vec![(LinkKind::Related, fx.task.clone())]
        );
    }

    #[test]
    fn restoring_should_keep_a_link_between_two_binned_tasks_in_either_order() {
        for first_is_holder in [true, false] {
            let fx = fixture();
            let other = other_task(&fx, "Other");
            fx.db
                .add_task_link(&fx.task, LinkKind::Related, &other, None, Actor::SYSTEM)
                .expect("link");
            assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
            assert!(fx.db.delete_task(&other, Actor::SYSTEM).expect("delete"));
            let order = if first_is_holder {
                [&fx.task, &other]
            } else {
                [&other, &fx.task]
            };
            for id in order {
                let outcome = fx
                    .db
                    .restore_task(id, Utc::now().timestamp(), WipCheck::Force, Actor::SYSTEM)
                    .expect("restore");
                assert!(matches!(outcome, RestoreOutcome::Restored { .. }));
            }
            assert_eq!(
                links_of(&fx.db, &fx.task),
                vec![(LinkKind::Related, other.clone())]
            );
            assert_eq!(
                links_of(&fx.db, &other),
                vec![(LinkKind::Related, fx.task.clone())]
            );
        }
    }

    #[test]
    fn restoring_should_drop_a_link_that_would_close_a_blocked_by_cycle() {
        let fx = fixture();
        let a = other_task(&fx, "A");
        let c = other_task(&fx, "C");
        let link = |task: &str, target: &str| {
            fx.db
                .add_task_link(task, LinkKind::BlockedBy, target, None, Actor::SYSTEM)
                .expect("link")
        };
        link(&fx.task, &c);
        link(&a, &fx.task);
        assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
        assert!(matches!(link(&c, &a), LinkAddOutcome::Added(_)));
        restore(&fx);
        assert_eq!(
            links_of(&fx.db, &fx.task),
            vec![(LinkKind::BlockedBy, c.clone())]
        );
        assert_eq!(links_of(&fx.db, &a), vec![(LinkKind::Blocks, c.clone())]);
    }

    #[test]
    fn restoring_should_drop_a_parent_link_to_a_task_that_is_no_longer_an_epic() {
        let fx = fixture();
        let umbrella = other_task(&fx, "Umbrella");
        let set_kind = |kind| {
            fx.db
                .apply_task_change(&umbrella, Actor::SYSTEM, |row| {
                    row.kind = kind;
                    ((), true)
                })
                .expect("kind");
        };
        set_kind(TaskKind::Epic);
        fx.db
            .add_task_link(&fx.task, LinkKind::Parent, &umbrella, None, Actor::SYSTEM)
            .expect("link");
        assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
        set_kind(TaskKind::Task);
        restore(&fx);
        assert!(links_of(&fx.db, &fx.task).is_empty());
        assert!(links_of(&fx.db, &umbrella).is_empty());
    }

    #[test]
    fn a_binned_tasks_links_should_block_nothing_while_it_is_in_the_bin() {
        let fx = fixture();
        let blocked = other_task(&fx, "Blocked");
        fx.db
            .add_task_link(&blocked, LinkKind::BlockedBy, &fx.task, None, Actor::SYSTEM)
            .expect("link");
        assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
        let row = fx.db.task(&blocked).expect("read").expect("task");
        assert!(row.links.is_empty());
        assert!(links_of(&fx.db, &blocked).is_empty());
        assert!(
            mmap_get(&fx.db.db, LINKS_BY_TARGET, &fx.task)
                .expect("index")
                .is_empty()
        );
    }
}
