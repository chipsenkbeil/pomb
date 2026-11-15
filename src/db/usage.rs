use std::collections::{BTreeMap, HashMap};

use redb::{ReadTransaction, ReadableTable, Table, WriteTransaction};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::bin::txn_bin_usage;
use super::entries::{txn_index_entry, txn_refuse_entry, txn_unindex_entry};
use super::events::txn_record_events;
use super::notifications::txn_record_task_notifications;
use super::outbox::txn_queue_events;
use super::{
    ACCOUNT_USAGE, AVATARS, BOARDS, Bound, COLORS_BY_BOARD, COLUMNS, COLUMNS_BY_BOARD, Db,
    DbResult, FIELD_DEFS, FIELDS_BY_BOARD, INBOUND_ADDRESSES, MAX_FIELD_DEF_ROW_BYTES,
    MAX_TASK_ROW_BYTES, QuotaExceeded, Rows, SWIMLANES, SWIMLANES_BY_BOARD, Store, TASK_COLORS,
    TASKS, TASKS_BY_COLUMN, TEMPLATES, TIME_ENTRIES, WEBHOOKS, board_use_in, read_in, rows_in,
    txn_member_quotas, txn_mmap_get, txn_refuse_over_quota,
};
use crate::disk::MIB;
use crate::models::{
    Actor, BoardRow, BoardTemplateRow, ColorRow, ColumnRow, FieldDefRow, InboundAddressRow, Quota,
    Quotas, SwimlaneRow, TaskAttachment, TaskRow, TimeEntryRow, WebhookRow,
};

/// Whether a write that takes its account past one of its quotas is refused. Writes the server
/// makes on its own, such as the starter board and the changes a delete makes to other tasks,
/// are allowed and still counted.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum OverQuota {
    /// The write is the account's own, so a quota refuses it.
    Refuse,
    /// The write is the server's, so it is counted and never refused.
    Allow,
    /// The write records a timer session that already ran, so it is refused only once the
    /// account is [`MAX_TIMER_OVERAGE_BYTES`] past its stored-bytes quota.
    TimerSession,
}

impl OverQuota {
    /// Returns the bytes past its stored-bytes quota the write may take its account, or [`None`]
    /// for a write no quota refuses.
    fn allowance(self) -> Option<u64> {
        match self {
            Self::Refuse => Some(0),
            Self::Allow => None,
            Self::TimerSession => Some(MAX_TIMER_OVERAGE_BYTES),
        }
    }
}

/// Bytes past its stored-bytes quota an account may still record finished timer sessions in.
const MAX_TIMER_OVERAGE_BYTES: u64 = 1_048_576;

/// Returns the account that owns the board column `column_id` sits on, read inside an open write
/// transaction.
pub(crate) fn txn_column_owner(
    txn: &WriteTransaction,
    column_id: &str,
) -> DbResult<Option<String>> {
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, column_id)? else {
        return Ok(None);
    };
    txn_board_owner(txn, &column.board_id)
}

/// Returns the account that owns the board `board_id`, read inside an open write transaction.
fn txn_board_owner(txn: &WriteTransaction, board_id: &str) -> DbResult<Option<String>> {
    Ok(read_in::<BoardRow>(txn, BOARDS, board_id)?.map(|board| board.owner_id))
}

/// A stored row that counts against the account owning its board.
pub(crate) trait Counted: Serialize + DeserializeOwned {
    /// The most bytes one stored row may take and the bound a write past it breaks, absent when
    /// any size is stored.
    const MAX_ROW: Option<(Bound, usize)> = None;

    /// Returns the account that owns the row, read inside an open write transaction.
    fn owner(&self, txn: &WriteTransaction) -> DbResult<Option<String>>;
}

impl Counted for BoardRow {
    fn owner(&self, _: &WriteTransaction) -> DbResult<Option<String>> {
        Ok(Some(self.owner_id.clone()))
    }
}

/// Implements [`Counted`] for rows that belong to the board their `board_id` names.
macro_rules! counted_by_board {
    ($($row:ty),*) => {$(
        impl Counted for $row {
            fn owner(&self, txn: &WriteTransaction) -> DbResult<Option<String>> {
                txn_board_owner(txn, &self.board_id)
            }
        }
    )*};
}

counted_by_board!(
    ColumnRow,
    SwimlaneRow,
    ColorRow,
    WebhookRow,
    InboundAddressRow
);

impl Counted for FieldDefRow {
    const MAX_ROW: Option<(Bound, usize)> =
        Some((Bound::FieldDefRowBytes, MAX_FIELD_DEF_ROW_BYTES));

    fn owner(&self, txn: &WriteTransaction) -> DbResult<Option<String>> {
        txn_board_owner(txn, &self.board_id)
    }
}

/// Refuses, with [`QuotaExceeded`], a row of `bytes` past the `max` of `bound` that grows on the
/// `held` bytes it replaces, so a write that shrinks such a row still goes through.
pub(crate) fn refuse_row_past(bound: Bound, max: usize, bytes: u64, held: u64) -> DbResult<()> {
    if bytes > max as u64 && bytes > held {
        return Err(Box::new(QuotaExceeded {
            bound,
            limit: max as u64,
        }));
    }
    Ok(())
}

/// Stores `row` under `key` in `table`, inside an open write transaction, and counts the change
/// in its size against its owner. Unless `over` is [`OverQuota::Allow`], a write that grows the
/// row past its [`Counted::MAX_ROW`] or its owner past the stored-bytes quota is refused.
pub(crate) fn txn_store_owned<T: Counted>(
    txn: &WriteTransaction,
    table: Rows,
    key: &str,
    row: &T,
    over: OverQuota,
) -> DbResult<()> {
    let raw = serde_json::to_vec(row)?;
    let bytes = raw.len() as u64;
    let held = txn
        .open_table(table)?
        .get(key)?
        .map_or(0, |guard| guard.value().len() as u64);
    let allowance = over.allowance();
    if let (Some((bound, max)), Some(_)) = (T::MAX_ROW, allowance) {
        refuse_row_past(bound, max, bytes, held)?;
    }
    if let Some(owner) = row.owner(txn)? {
        if let Some(allowance) = allowance {
            txn_refuse_past_bytes(txn, &owner, bytes.saturating_sub(held), allowance)?;
        }
        txn_count_usage(txn, &owner, bytes, held)?;
    }
    txn.open_table(table)?.insert(key, raw.as_slice())?;
    Ok(())
}

/// Removes `key` from `table`, inside an open write transaction, takes its bytes off its owner,
/// and returns the removed row.
pub(crate) fn txn_remove_owned<T: Counted>(
    txn: &WriteTransaction,
    table: Rows,
    key: &str,
) -> DbResult<Option<T>> {
    let Some(raw) = txn
        .open_table(table)?
        .remove(key)?
        .map(|guard| guard.value().to_vec())
    else {
        return Ok(None);
    };
    let row: T = serde_json::from_slice(&raw)?;
    if let Some(owner) = row.owner(txn)? {
        txn_count_usage(txn, &owner, 0, raw.len() as u64)?;
    }
    Ok(Some(row))
}

/// Loads row `key` of `table`, applies `f` and stores it back through [`txn_store_owned`] under
/// [`OverQuota::Refuse`]. Returns false when the key is unknown.
pub(crate) fn mutate_owned<T: Counted>(
    db: &Store,
    table: Rows,
    key: &str,
    f: impl FnOnce(&mut T),
) -> DbResult<bool> {
    let txn = db.begin_write()?;
    let Some(mut row) = read_in::<T>(&txn, table, key)? else {
        return Ok(false);
    };
    f(&mut row);
    txn_store_owned(&txn, table, key, &row, OverQuota::Refuse)?;
    txn.commit()?;
    Ok(true)
}

/// Returns the bytes the stored rows `keys` of `table` take, read inside an open write
/// transaction.
pub(crate) fn txn_rows_bytes(
    txn: &WriteTransaction,
    table: Rows,
    keys: &[String],
) -> DbResult<u64> {
    let tbl = txn.open_table(table)?;
    let mut bytes = 0;
    for key in keys {
        if let Some(guard) = tbl.get(key.as_str())? {
            bytes += guard.value().len() as u64;
        }
    }
    Ok(bytes)
}

/// Returns the bytes `owner` stores, read inside an open write transaction.
fn txn_usage(txn: &WriteTransaction, owner: &str) -> DbResult<u64> {
    Ok(txn
        .open_table(ACCOUNT_USAGE)?
        .get(owner)?
        .map_or(0, |bytes| bytes.value()))
}

/// Adds `added` bytes to what `owner` stores and takes `removed` away, inside an open write
/// transaction. The count never drops below zero.
pub(crate) fn txn_count_usage(
    txn: &WriteTransaction,
    owner: &str,
    added: u64,
    removed: u64,
) -> DbResult<()> {
    if added == removed {
        return Ok(());
    }
    let held = txn_usage(txn, owner)?;
    let bytes = held.saturating_add(added).saturating_sub(removed);
    txn.open_table(ACCOUNT_USAGE)?.insert(owner, bytes)?;
    Ok(())
}

/// Refuses, with [`QuotaExceeded`], a write that adds `added` bytes and removes `removed` from
/// what `owner` stores when that leaves it past its stored-bytes quota, inside that write's
/// transaction. A write that grows nothing is never refused.
pub(crate) fn txn_refuse_over_bytes(
    txn: &WriteTransaction,
    owner: &str,
    added: u64,
    removed: u64,
) -> DbResult<()> {
    txn_refuse_past_bytes(txn, owner, added.saturating_sub(removed), 0)
}

/// Refuses, with [`QuotaExceeded`], a write that grows what `owner` stores by `grown` bytes when
/// that leaves it more than `allowance` bytes past its stored-bytes quota, inside that write's
/// transaction.
fn txn_refuse_past_bytes(
    txn: &WriteTransaction,
    owner: &str,
    grown: u64,
    allowance: u64,
) -> DbResult<()> {
    if grown == 0 {
        return Ok(());
    }
    let Some(quotas) = txn_member_quotas(txn, owner)? else {
        return Ok(());
    };
    refuse_past_quota(
        &quotas,
        txn_usage(txn, owner)?.saturating_add(grown),
        allowance,
    )
}

/// Refuses, with [`QuotaExceeded`], an upload that adds `added` bytes to what `owner` stores when
/// that leaves it past its stored-bytes quota, read inside a read transaction.
pub(crate) fn read_refuse_over_bytes(
    txn: &ReadTransaction,
    owner: &str,
    added: u64,
) -> DbResult<()> {
    if added == 0 {
        return Ok(());
    }
    let Some(quotas) = txn_member_quotas(txn, owner)? else {
        return Ok(());
    };
    let held = txn
        .open_table(ACCOUNT_USAGE)?
        .get(owner)?
        .map_or(0, |bytes| bytes.value());
    refuse_past_quota(&quotas, held.saturating_add(added), 0)
}

/// Refuses, with [`QuotaExceeded`], `after` stored bytes when they are more than `allowance`
/// bytes past the stored-bytes quota of `quotas`.
fn refuse_past_quota(quotas: &Quotas, after: u64, allowance: u64) -> DbResult<()> {
    let limit = quotas.limit(Quota::StoredMibPerAccount);
    if after > u64::from(limit) * MIB + allowance {
        return Err(Box::new(QuotaExceeded::of(
            Quota::StoredMibPerAccount,
            limit,
        )));
    }
    Ok(())
}

/// Stores `task` through the open tasks table `tasks`, inside an open write transaction, counts
/// the change in the row's size against its owner, records the board events and the notifications
/// the change makes, and queues the webhook events it makes.
///
/// Unless `over` is [`OverQuota::Allow`], the write is refused when text that differs from the
/// stored row is past its cap, when it adds subtasks past the subtask quota, when it grows the row
/// past [`MAX_TASK_ROW_BYTES`], or when it grows its owner past the stored-bytes quota. A write
/// under [`OverQuota::Allow`] changes no user text and never moves a task to another account's
/// board, so it is only counted.
pub(crate) fn txn_store_task(
    txn: &WriteTransaction,
    tasks: &mut Table<'_, &'static str, &'static [u8]>,
    task: &TaskRow,
    over: OverQuota,
    actor: Actor<'_>,
) -> DbResult<()> {
    let before = txn_write_task(txn, tasks, task, over, actor)?;
    txn_queue_events(txn, before.as_ref(), task, actor)
}

/// Stores `task` like [`txn_store_task`] but queues no webhook event, and returns the row it
/// replaced.
pub(crate) fn txn_write_task(
    txn: &WriteTransaction,
    tasks: &mut Table<'_, &'static str, &'static [u8]>,
    task: &TaskRow,
    over: OverQuota,
    actor: Actor<'_>,
) -> DbResult<Option<TaskRow>> {
    let raw = serde_json::to_vec(task)?;
    let bytes = raw.len() as u64;
    let (held_bytes, before) = match tasks.get(task.id.as_str())? {
        Some(guard) => {
            let held = guard.value();
            (
                held.len() as u64,
                Some(serde_json::from_slice::<TaskRow>(held)?),
            )
        }
        None => (0, None),
    };
    match over.allowance() {
        Some(allowance) => {
            txn_check_task(txn, task, before.as_ref(), bytes, held_bytes, allowance)?;
        }
        None => {
            let owner = txn_column_owner(txn, &task.column_id)?;
            let held = match before.as_ref() {
                Some(before) => row_shares(before, held_bytes)?,
                None => Shares::default(),
            };
            txn_charge_task(
                txn,
                owner.as_deref(),
                &row_shares(task, bytes)?,
                owner.as_deref(),
                &held,
                None,
            )?;
        }
    }
    tasks.insert(task.id.as_str(), raw.as_slice())?;
    txn_record_events(txn, before.as_ref(), task, actor)?;
    txn_record_task_notifications(txn, before.as_ref(), task, actor)?;
    Ok(before)
}

/// Refuses a write of `task`, which takes `bytes` stored and replaces the stored row `before`
/// of `held_bytes`, past a text cap, the subtask quota, the row cap or `allowance` bytes past the
/// stored-bytes quota, and counts it against the owners of its old and new column.
fn txn_check_task(
    txn: &WriteTransaction,
    task: &TaskRow,
    before: Option<&TaskRow>,
    bytes: u64,
    held_bytes: u64,
    allowance: u64,
) -> DbResult<()> {
    task.check_caps(before)?;
    refuse_row_past(Bound::TaskRowBytes, MAX_TASK_ROW_BYTES, bytes, held_bytes)?;
    let owner = txn_column_owner(txn, &task.column_id)?;
    let held_owner = match before {
        Some(before) if before.column_id != task.column_id => {
            txn_column_owner(txn, &before.column_id)?
        }
        Some(_) => owner.clone(),
        None => None,
    };
    if let Some(owner) = owner.as_deref() {
        let held_subtasks = before.map_or(0, |row| row.subtasks.len());
        if task.subtasks.len() > held_subtasks {
            txn_refuse_over_quota(txn, owner, Quota::SubtasksPerTask, task.subtasks.len())?;
        }
    }
    let held = match before {
        Some(before) => row_shares(before, held_bytes)?,
        None => Shares::default(),
    };
    txn_charge_task(
        txn,
        owner.as_deref(),
        &row_shares(task, bytes)?,
        held_owner.as_deref(),
        &held,
        Some(allowance),
    )
}

/// What a stored task takes, split by the account that holds each part.
#[derive(Debug, Default)]
pub(crate) struct Shares {
    /// Bytes counted against the board owner: the row without the history events other accounts
    /// paid for, plus its attachment files where they are counted.
    pub(crate) owner: u64,
    /// Bytes of the history events each paying account holds.
    pub(crate) payers: BTreeMap<String, u64>,
}

impl Shares {
    /// Takes every payer's share off its account, inside an open write transaction.
    pub(crate) fn release_payers(&self, txn: &WriteTransaction) -> DbResult<()> {
        for (payer, bytes) in &self.payers {
            txn_count_usage(txn, payer, 0, *bytes)?;
        }
        Ok(())
    }

    /// Adds `other` to these shares.
    fn add(&mut self, other: Self) {
        self.owner += other.owner;
        for (payer, bytes) in other.payers {
            *self.payers.entry(payer).or_default() += bytes;
        }
    }
}

/// Splits the `len` bytes of the stored row of `task`, without its attachment files, between its
/// board owner and the accounts its history events name in `paid_by`.
fn row_shares(task: &TaskRow, len: u64) -> DbResult<Shares> {
    let mut payers: BTreeMap<String, u64> = BTreeMap::new();
    for event in &task.history {
        if let Some(payer) = &event.paid_by {
            let bytes = serde_json::to_vec(event)?.len() as u64 + 1;
            *payers.entry(payer.clone()).or_default() += bytes;
        }
    }
    let paid: u64 = payers.values().sum();
    Ok(Shares {
        owner: len.saturating_sub(paid),
        payers,
    })
}

/// Counts the change from `held`, which `held_owner` held, to `after`, which `owner` holds, on
/// every account that holds a part, inside an open write transaction. With an `allowance`, the
/// write is first refused when it grows an account past its stored-bytes quota.
fn txn_charge_task(
    txn: &WriteTransaction,
    owner: Option<&str>,
    after: &Shares,
    held_owner: Option<&str>,
    held: &Shares,
    allowance: Option<u64>,
) -> DbResult<()> {
    // Collect what each account adds and removes
    let mut changes: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    if let Some(owner) = owner {
        changes.entry(owner).or_default().0 += after.owner;
    }
    if let Some(held_owner) = held_owner {
        changes.entry(held_owner).or_default().1 += held.owner;
    }
    for (payer, bytes) in &after.payers {
        changes.entry(payer).or_default().0 += bytes;
    }
    for (payer, bytes) in &held.payers {
        changes.entry(payer).or_default().1 += bytes;
    }

    // Refuse a write that grows an account past its quota
    if let Some(allowance) = allowance {
        for (account, (added, removed)) in &changes {
            txn_refuse_past_bytes(txn, account, added.saturating_sub(*removed), allowance)?;
        }
    }

    // Count the change on every account
    for (account, (added, removed)) in changes {
        txn_count_usage(txn, account, added, removed)?;
    }
    Ok(())
}

/// Returns the shares a stored task row `raw` and its attachment files take, the files by the
/// sizes the row records, and counts the files against the board owner.
fn task_shares(raw: &[u8]) -> DbResult<Shares> {
    let task: TaskRow = serde_json::from_slice(raw)?;
    let files: u64 = task
        .attachments
        .iter()
        .map(TaskAttachment::stored_bytes)
        .sum();
    let mut shares = row_shares(&task, raw.len() as u64)?;
    shares.owner += files;
    Ok(shares)
}

/// Returns the bytes the board `board_id` counts against its owner: its tasks and layout rows,
/// read inside an open write transaction. Recycle bin rows count against the account that
/// deleted them.
pub(crate) fn txn_board_bytes(txn: &WriteTransaction, board_id: &str) -> DbResult<u64> {
    let column_ids = txn_mmap_get(txn, COLUMNS_BY_BOARD, board_id)?;
    let swimlane_ids = txn_mmap_get(txn, SWIMLANES_BY_BOARD, board_id)?;
    let color_ids = txn_mmap_get(txn, COLORS_BY_BOARD, board_id)?;
    let field_ids = txn_mmap_get(txn, FIELDS_BY_BOARD, board_id)?;
    let mut task_ids = Vec::new();
    for column_id in &column_ids {
        task_ids.extend(txn_mmap_get(txn, TASKS_BY_COLUMN, column_id)?);
    }
    Ok(txn_task_bytes(txn, &task_ids)?.owner
        + txn_rows_bytes(txn, BOARDS, &[board_id.to_string()])?
        + txn_rows_bytes(txn, COLUMNS, &column_ids)?
        + txn_rows_bytes(txn, SWIMLANES, &swimlane_ids)?
        + txn_rows_bytes(txn, TASK_COLORS, &color_ids)?
        + txn_rows_bytes(txn, FIELD_DEFS, &field_ids)?)
}

/// Returns the shares the stored rows of `task_ids` and their attachment files take, read inside
/// an open write transaction, for a delete that removes them all.
pub(crate) fn txn_task_bytes(txn: &WriteTransaction, task_ids: &[String]) -> DbResult<Shares> {
    let tasks = txn.open_table(TASKS)?;
    let mut shares = Shares::default();
    for id in task_ids {
        if let Some(guard) = tasks.get(id.as_str())? {
            shares.add(task_shares(guard.value())?);
        }
    }
    Ok(shares)
}

/// Stores `entry`, inside an open write transaction, indexes it by its account and day, and
/// counts the change in its size against its account. The write is refused when text that
/// differs from the stored entry is past its cap, and unless `over` is [`OverQuota::Allow`] when
/// it takes the account past its time entry quota or its day past its most entries, or grows the
/// account past its stored-bytes quota by more than [`OverQuota::allowance`].
pub(crate) fn txn_store_entry(
    txn: &WriteTransaction,
    entry: &TimeEntryRow,
    over: OverQuota,
) -> DbResult<()> {
    let mut entries = txn.open_table(TIME_ENTRIES)?;
    let (held, before) = match entries.get(entry.id.as_str())? {
        Some(guard) => (
            guard.value().len() as u64,
            Some(serde_json::from_slice::<TimeEntryRow>(guard.value())?),
        ),
        None => (0, None),
    };
    entry.check_caps(before.as_ref())?;
    txn_refuse_entry(txn, entry, before.as_ref(), over)?;
    let raw = serde_json::to_vec(entry)?;
    let bytes = raw.len() as u64;
    let grown = bytes.saturating_sub(held);
    if let Some(allowance) = over.allowance() {
        txn_refuse_past_bytes(txn, &entry.account_id, grown, allowance)?;
    }
    txn_count_usage(txn, &entry.account_id, bytes, held)?;
    entries.insert(entry.id.as_str(), raw.as_slice())?;
    txn_index_entry(txn, entry, before.as_ref())
}

/// Removes the time entry `id` through the open table `entries`, inside an open write
/// transaction, drops it from the indexes of its account and day, and takes its bytes off its
/// account. Returns the removed entry.
pub(crate) fn txn_remove_entry(
    txn: &WriteTransaction,
    entries: &mut Table<'_, &'static str, &'static [u8]>,
    id: &str,
) -> DbResult<Option<TimeEntryRow>> {
    let Some(guard) = entries.remove(id)? else {
        return Ok(None);
    };
    let entry: TimeEntryRow = serde_json::from_slice(guard.value())?;
    txn_count_usage(txn, &entry.account_id, 0, guard.value().len() as u64)?;
    txn_unindex_entry(txn, &entry)?;
    Ok(Some(entry))
}

/// Adds the bytes every task row and its attachment files take to the usage of the task's owner,
/// read inside an open write transaction.
fn count_tasks(txn: &WriteTransaction, usage: &mut HashMap<String, u64>) -> DbResult<()> {
    let mut owners: HashMap<String, Option<String>> = HashMap::new();
    let tasks = txn.open_table(TASKS)?;
    for item in tasks.iter()? {
        let (_, raw) = item?;
        let task: TaskRow = serde_json::from_slice(raw.value())?;
        let owner = match owners.get(&task.column_id) {
            Some(owner) => owner.clone(),
            None => {
                let owner = txn_column_owner(txn, &task.column_id)?;
                owners.insert(task.column_id.clone(), owner.clone());
                owner
            }
        };
        let shares = task_shares(raw.value())?;
        if let Some(owner) = owner {
            *usage.entry(owner).or_default() += shares.owner;
        }
        for (payer, bytes) in shares.payers {
            *usage.entry(payer).or_default() += bytes;
        }
    }
    Ok(())
}

/// Adds the bytes of every row of `table` to its owner in `usage`, inside an open write
/// transaction.
fn count_owned<T: Counted>(
    txn: &WriteTransaction,
    table: Rows,
    usage: &mut HashMap<String, u64>,
) -> DbResult<()> {
    for item in txn.open_table(table)?.iter()? {
        let (_, raw) = item?;
        let row: T = serde_json::from_slice(raw.value())?;
        if let Some(owner) = row.owner(txn)? {
            *usage.entry(owner).or_default() += raw.value().len() as u64;
        }
    }
    Ok(())
}

/// Rebuilds what every account stores from its boards with their columns, swimlanes, colors,
/// field definitions and webhooks, its task rows, time entries, saved templates and attachment
/// files, inside an open write transaction.
pub(crate) fn txn_recount_usage(txn: &WriteTransaction) -> DbResult<()> {
    let mut usage: HashMap<String, u64> = HashMap::new();
    count_tasks(txn, &mut usage)?;
    txn_bin_usage(txn, &mut usage)?;
    for item in txn.open_table(TIME_ENTRIES)?.iter()? {
        let (_, raw) = item?;
        let entry: TimeEntryRow = serde_json::from_slice(raw.value())?;
        *usage.entry(entry.account_id).or_default() += raw.value().len() as u64;
    }
    for item in txn.open_table(TEMPLATES)?.iter()? {
        let (_, raw) = item?;
        let template: BoardTemplateRow = serde_json::from_slice(raw.value())?;
        if let Some(owner) = template.owner_id {
            *usage.entry(owner).or_default() += raw.value().len() as u64 + template.snapshot_bytes;
        }
    }
    count_owned::<BoardRow>(txn, BOARDS, &mut usage)?;
    count_owned::<ColumnRow>(txn, COLUMNS, &mut usage)?;
    count_owned::<SwimlaneRow>(txn, SWIMLANES, &mut usage)?;
    count_owned::<ColorRow>(txn, TASK_COLORS, &mut usage)?;
    count_owned::<FieldDefRow>(txn, FIELD_DEFS, &mut usage)?;
    count_owned::<WebhookRow>(txn, WEBHOOKS, &mut usage)?;
    for item in txn.open_table(AVATARS)?.iter()? {
        let (owner, image) = item?;
        *usage.entry(owner.value().to_string()).or_default() += image.value().len() as u64;
    }
    count_owned::<InboundAddressRow>(txn, INBOUND_ADDRESSES, &mut usage)?;
    let mut table = txn.open_table(ACCOUNT_USAGE)?;
    table.retain(|_, _| false)?;
    for (owner, bytes) in usage {
        table.insert(owner.as_str(), bytes)?;
    }
    Ok(())
}

/// The most that one account, one board and one attachment file hold.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LargestUse {
    /// Bytes the account that stores the most holds.
    pub account_bytes: u64,
    /// Bytes of task rows and attachment files the fullest board holds, its recycle bin included.
    pub board_bytes: u64,
    /// Bytes of the largest attachment file.
    pub file_bytes: u64,
}

impl Db {
    /// Returns the most that one account, one board and one attachment file hold, which reads
    /// every board.
    pub fn largest_use(&self) -> DbResult<LargestUse> {
        let txn = self.db.begin_read()?;
        let mut largest = LargestUse::default();
        for item in txn.open_table(ACCOUNT_USAGE)?.iter()? {
            largest.account_bytes = largest.account_bytes.max(item?.1.value());
        }
        let boards: Vec<BoardRow> = rows_in(&txn.open_table(BOARDS)?)?;
        for board in boards {
            let held = board_use_in(&txn, &board.id)?;
            largest.board_bytes = largest.board_bytes.max(held.bytes);
            largest.file_bytes = largest.file_bytes.max(held.largest_file);
        }
        Ok(largest)
    }

    /// Returns the bytes each account stores in boards and their layout, task rows, time entries,
    /// saved templates and attachment files, keyed by account id. An account missing from the
    /// map stores nothing.
    pub fn stored_bytes_by_account(&self) -> DbResult<HashMap<String, u64>> {
        let txn = self.db.begin_read()?;
        let mut usage = HashMap::new();
        for item in txn.open_table(ACCOUNT_USAGE)?.iter()? {
            let (account, bytes) = item?;
            usage.insert(account.value().to_string(), bytes.value());
        }
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::db::{
        ColumnDeleteOutcome, ColumnPatch, EntryEdit, FieldCreateOutcome, META, NewWebhook,
        OwnerChange, SCHEMA_VERSION_KEY, SwimlanePatch, WebhookPatch, WipCheck, mutate_task_as,
        push_column_event,
    };
    use crate::models::{
        AccountId, Actor, BIN_RETENTION_DAYS, BoardRole, FieldColumn, FieldKind, FieldOwner,
        HistoryKind, InstanceMode, LinkKind, SECONDS_PER_DAY, WebhookEvent,
    };

    /// An open database with its admin and a member with one column.
    struct Fixture {
        /// Keeps the database directory alive.
        _dir: tempfile::TempDir,
        /// The database.
        db: Db,
        /// The member.
        member: AccountId,
        /// The member's board.
        board: String,
        /// The column of the member's board.
        column: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("usage.redb").to_str().expect("utf8")).expect("db");
        db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let board = db.create_board(&member, "Bob").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        Fixture {
            _dir: dir,
            db,
            member,
            board,
            column,
        }
    }

    /// Returns what every account stores as counted from scratch, leaving the kept counts as
    /// they were.
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

    /// Returns the bytes `account` stores, as kept.
    fn stored(db: &Db, account: &AccountId) -> u64 {
        db.stored_bytes_by_account()
            .expect("usage")
            .get(account.as_str())
            .copied()
            .unwrap_or_default()
    }

    /// Asserts that the kept counts match a count from scratch, ignoring accounts at zero.
    fn assert_counts_match(db: &Db) {
        let mut kept = db.stored_bytes_by_account().expect("usage");
        kept.retain(|_, bytes| *bytes > 0);
        assert_eq!(kept, recounted(db));
    }

    #[test]
    fn usage_should_follow_every_write_that_changes_what_an_account_stores() {
        let fx = fixture();
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        assert_counts_match(&fx.db);
        fx.db
            .update_task(&task, Some("Renamed"), Some("A description"), None, None)
            .expect("update");
        fx.db
            .add_attachment(
                &task,
                "a.bin",
                "application/octet-stream",
                &[7; 4096],
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
        assert_counts_match(&fx.db);
        let held = stored(&fx.db, &fx.member);
        assert!(held > 4096, "{held}");

        let template = fx
            .db
            .save_board_as_template(&fx.member, &fx.board, "Mine", "")
            .expect("save")
            .expect("board");
        assert_counts_match(&fx.db);
        fx.db.delete_template(&template).expect("delete");
        assert_counts_match(&fx.db);

        let second = fx
            .db
            .create_task(&fx.column, None, "Second", 1, None)
            .expect("task");
        let entry = fx.db.create_entry(&second, 5, "A note").expect("entry");
        assert_counts_match(&fx.db);
        let longer = "a longer note ".repeat(20);
        let started = Utc::now().to_rfc3339();
        assert!(
            fx.db
                .update_entry(
                    &entry,
                    &second,
                    EntryEdit {
                        seconds: 600,
                        note: &longer,
                        started_at: &started,
                        labels: &[],
                    },
                    Actor::SYSTEM,
                )
                .expect("update")
        );
        assert_counts_match(&fx.db);
        let other = fx.db.create_entry(&second, 3, "Other").expect("entry");
        assert!(fx.db.delete_entry(&other, Actor::SYSTEM).expect("delete"));
        assert_counts_match(&fx.db);
        assert!(fx.db.delete_task(&task, Actor::SYSTEM).expect("delete"));
        assert_counts_match(&fx.db);
        let after = Utc::now().timestamp() + (BIN_RETENTION_DAYS + 1) * SECONDS_PER_DAY;
        assert_eq!(fx.db.purge_bin(after).expect("purge"), Some(1));
        assert_counts_match(&fx.db);
        assert!(stored(&fx.db, &fx.member) < held);

        let other = fx.db.create_board(&fx.member, "Other").expect("board");
        fx.db.delete_board(&fx.board).expect("delete");
        assert!(fx.db.task(&second).expect("read").is_none());
        assert_counts_match(&fx.db);
        fx.db
            .delete_board(&other)
            .expect_err("the last board stays");
    }

    #[test]
    fn usage_should_follow_document_saves_dropped_versions_and_the_bin_purge() {
        let fx = fixture();
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        let document = fx
            .db
            .add_document(&task, "notes", "first", Actor::SYSTEM)
            .expect("add")
            .expect("task");
        assert_counts_match(&fx.db);
        for n in 0..=TaskAttachment::MAX_VERSIONS {
            fx.db
                .save_document(
                    &task,
                    &document.id,
                    None,
                    &format!("text {n}"),
                    Actor::SYSTEM,
                )
                .expect("save")
                .expect("document");
            assert_counts_match(&fx.db);
        }
        let files = || {
            std::fs::read_dir(fx.db.attachments_dir())
                .expect("dir")
                .count()
        };
        assert_eq!(files(), TaskAttachment::MAX_VERSIONS + 1);
        assert!(fx.db.delete_task(&task, Actor::SYSTEM).expect("delete"));
        let after = Utc::now().timestamp() + (BIN_RETENTION_DAYS + 1) * SECONDS_PER_DAY;
        assert_eq!(fx.db.purge_bin(after).expect("purge"), Some(1));
        assert_counts_match(&fx.db);
        assert_eq!(files(), 0);
    }

    #[test]
    fn usage_should_stay_in_step_as_a_task_enters_leaves_and_is_purged_from_the_bin() {
        let fx = fixture();
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        fx.db.create_entry(&task, 5, "A note").expect("entry");
        fx.db
            .add_attachment(
                &task,
                "a.bin",
                "application/octet-stream",
                &[7; 2048],
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
        assert_counts_match(&fx.db);
        let held = stored(&fx.db, &fx.member);

        assert!(fx.db.delete_task(&task, Actor::SYSTEM).expect("delete"));
        assert_counts_match(&fx.db);
        let now = Utc::now().timestamp();
        fx.db
            .restore_task(&task, now, WipCheck::Force, Actor::SYSTEM)
            .expect("restore");
        assert_counts_match(&fx.db);
        assert!(stored(&fx.db, &fx.member) >= held);

        assert!(fx.db.delete_task(&task, Actor::SYSTEM).expect("delete"));
        fx.db
            .purge_bin(now + (BIN_RETENTION_DAYS + 1) * SECONDS_PER_DAY)
            .expect("purge");
        assert_counts_match(&fx.db);
        assert!(stored(&fx.db, &fx.member) < held);
    }

    #[test]
    fn discarding_a_task_should_leave_no_bin_row_link_file_or_usage() {
        let fx = fixture();
        let parent = fx
            .db
            .create_task(&fx.column, None, "Parent", 1, None)
            .expect("parent");
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        fx.db
            .add_task_link(&parent, LinkKind::BlockedBy, &task, None, Actor::SYSTEM)
            .expect("link");
        let file = fx
            .db
            .add_attachment(
                &task,
                "a.bin",
                "application/octet-stream",
                &[7; 2048],
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task")
            .id;
        let held = stored(&fx.db, &fx.member);

        assert!(fx.db.discard_task(&task, Actor::SYSTEM).expect("discard"));

        assert_counts_match(&fx.db);
        assert!(stored(&fx.db, &fx.member) < held);
        assert!(fx.db.task(&task).expect("read").is_none());
        assert!(fx.db.binned_tasks(&fx.board).expect("bin").is_empty());
        assert_eq!(fx.db.read_attachment_data(&file).expect("read"), None);
        let kept = fx.db.task(&parent).expect("read").expect("parent");
        assert!(kept.links.is_empty(), "{:?}", kept.links);
    }

    #[test]
    fn usage_should_follow_every_edit_of_a_board_and_its_layout() {
        let fx = fixture();
        let before = stored(&fx.db, &fx.member);
        assert!(before > 0, "the board and its column are counted");
        assert_counts_match(&fx.db);

        let mut config = serde_json::Map::new();
        config.insert("note".to_string(), "n".repeat(500).into());
        fx.db.set_board_config(&fx.board, &config).expect("config");
        fx.db
            .rename_board(&fx.board, "Renamed board")
            .expect("rename");
        let patch = ColumnPatch {
            name: Some("Doing"),
            description: Some("A column description"),
            config_json: Some(r#"{"sorting":"due"}"#),
            ..ColumnPatch::default()
        };
        fx.db.update_column(&fx.column, &patch).expect("column");
        let second = fx
            .db
            .create_column(&fx.board, "Done", None)
            .expect("column");
        fx.db.move_column(&second, 0).expect("move");
        assert_counts_match(&fx.db);

        let lane = fx.db.create_swimlane(&fx.board, "Lane").expect("lane");
        let other = fx.db.create_swimlane(&fx.board, "Other").expect("lane");
        fx.db
            .update_swimlane(
                &lane,
                &SwimlanePatch {
                    name: Some("A longer lane name"),
                    ..SwimlanePatch::default()
                },
            )
            .expect("rename");
        fx.db.move_swimlane(&other, 0).expect("move");
        fx.db.delete_swimlane(&lane).expect("delete");
        assert_counts_match(&fx.db);

        let color = fx.db.list_colors(&fx.board).expect("colors")[0].id.clone();
        fx.db
            .update_color(&color, None, Some("What red means"), None, Some(true))
            .expect("color");
        let source = fx.db.create_board(&fx.member, "Source").expect("board");
        fx.db.copy_colors(&fx.board, &source).expect("copy");
        assert_counts_match(&fx.db);

        let field = FieldDefRow {
            id: String::new(),
            board_id: fx.board.clone(),
            key: "size".to_string(),
            label: "Size".to_string(),
            kind: FieldKind::Select,
            options: vec!["S".to_string(), "M".to_string()],
            columns: Vec::new(),
            default: None,
            required: false,
            owner: FieldOwner::User,
            position: 0,
            created_at: String::new(),
        };
        let FieldCreateOutcome::Created(field) = fx.db.create_field(field).expect("field") else {
            panic!("the key is new");
        };
        fx.db
            .apply_field_change(&field.id, |field| {
                field.options.push("L".to_string());
                ((), true)
            })
            .expect("change");
        assert_counts_match(&fx.db);
        fx.db.delete_field(&field.id).expect("delete");
        assert!(matches!(
            fx.db.delete_column(&second).expect("delete"),
            ColumnDeleteOutcome::Deleted
        ));
        assert_counts_match(&fx.db);

        fx.db.delete_board(&source).expect("delete");
        assert_counts_match(&fx.db);
    }

    #[test]
    fn field_definitions_should_stop_growing_past_their_row_cap() {
        const OPTION_CHARS: usize = 200;
        const OPTIONS: usize = 200;
        let fx = fixture();
        let column = |n: usize| FieldColumn {
            key: format!("c{n}"),
            label: format!("C{n}"),
            kind: FieldKind::Select,
            options: (0..OPTIONS)
                .map(|o| format!("{o:0>OPTION_CHARS$}"))
                .collect(),
            required: false,
        };
        let field = |columns: Vec<FieldColumn>| FieldDefRow {
            id: String::new(),
            board_id: fx.board.clone(),
            key: "grid".to_string(),
            label: "Grid".to_string(),
            kind: FieldKind::Table,
            options: Vec::new(),
            columns,
            default: None,
            required: false,
            owner: FieldOwner::User,
            position: 0,
            created_at: String::new(),
        };
        let over = MAX_FIELD_DEF_ROW_BYTES / (OPTIONS * OPTION_CHARS) + 1;
        let refused = fx
            .db
            .create_field(field((0..over).map(column).collect()))
            .expect_err("past the row cap");
        let refusal = refused
            .downcast_ref::<QuotaExceeded>()
            .expect("a quota refusal");
        assert_eq!(refusal.bound, Bound::FieldDefRowBytes);

        let FieldCreateOutcome::Created(made) = fx
            .db
            .create_field(field((0..over - 1).map(column).collect()))
            .expect("under the row cap")
        else {
            panic!("the key is new");
        };
        let grown = fx.db.apply_field_change(&made.id, |field| {
            field.columns.push(column(over));
            ((), true)
        });
        let refusal = grown.expect_err("past the row cap");
        assert_eq!(
            refusal
                .downcast_ref::<QuotaExceeded>()
                .map(|refusal| refusal.bound),
            Some(Bound::FieldDefRowBytes)
        );
        fx.db
            .apply_field_change(&made.id, |field| {
                field.columns.pop();
                ((), true)
            })
            .expect("a shrinking write goes through");
        assert_counts_match(&fx.db);
    }

    #[test]
    fn webhooks_should_count_against_stored_bytes_until_they_or_their_board_go() {
        let fx = fixture();
        let before = fx.db.stored_bytes_by_account().expect("usage")[fx.member.as_str()];
        let hook = |name: &str| NewWebhook {
            board_id: fx.board.clone(),
            name: name.to_string(),
            url: "https://example.com/hook".to_string(),
            events: vec![WebhookEvent::TaskCreated],
            column_id: None,
            enabled: true,
        };
        let first = fx.db.create_webhook(hook("First")).expect("webhook");
        let second = fx.db.create_webhook(hook("Second")).expect("webhook");
        let held = fx.db.stored_bytes_by_account().expect("usage")[fx.member.as_str()];
        assert!(held > before);
        assert_counts_match(&fx.db);

        fx.db
            .update_webhook(
                &first.id,
                WebhookPatch {
                    name: Some("A much longer webhook name".to_string()),
                    enabled: Some(false),
                    ..WebhookPatch::default()
                },
            )
            .expect("update");
        assert_counts_match(&fx.db);

        assert!(fx.db.delete_webhook(&first.id).expect("delete"));
        assert_counts_match(&fx.db);

        fx.db.create_board(&fx.member, "Spare").expect("board");
        fx.db.delete_board(&fx.board).expect("delete board");
        assert!(fx.db.webhook(&second.id).expect("read").is_none());
        assert_counts_match(&fx.db);
    }

    #[test]
    fn queued_webhook_events_should_not_count_against_stored_bytes() {
        let fx = fixture();
        let hook = |name: &str| NewWebhook {
            board_id: fx.board.clone(),
            name: name.to_string(),
            url: "https://example.com/hook".to_string(),
            events: vec![WebhookEvent::TaskCreated],
            column_id: None,
            enabled: true,
        };
        let first = fx.db.create_webhook(hook("First")).expect("webhook");
        let second = fx.db.create_webhook(hook("Second")).expect("webhook");
        fx.db
            .create_task(&fx.column, None, "Queued", 1, None)
            .expect("task");
        assert_eq!(fx.db.webhook_outbox(&first.id).expect("outbox").len(), 1);
        let held = stored(&fx.db, &fx.member);

        let txn = fx.db.db.begin_write().expect("write");
        for index in 0..1005 {
            let row = Db::new_task_row(&fx.column, None, &format!("Task {index}"), 0, None);
            txn_queue_events(&txn, None, &row, Actor::SYSTEM).expect("queue");
        }
        txn.commit().expect("commit");
        assert_eq!(fx.db.webhook_outbox(&first.id).expect("outbox").len(), 1000);
        assert_eq!(stored(&fx.db, &fx.member), held);
        assert_counts_match(&fx.db);

        assert!(fx.db.delete_webhook(&first.id).expect("delete"));
        fx.db.create_board(&fx.member, "Spare").expect("board");
        fx.db.delete_board(&fx.board).expect("delete board");
        assert!(fx.db.webhook_outbox(&second.id).expect("outbox").is_empty());
        assert_counts_match(&fx.db);
    }

    #[test]
    fn usage_should_be_counted_when_a_version_14_file_is_upgraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("upgrade.redb");
        let path = path.to_str().expect("utf8");
        let (member, layout_only) = {
            let db = Db::connect(path).expect("db");
            db.create_user("admin").expect("admin");
            let member = db.create_user("bob").expect("member");
            let layout_only = db.create_user("carol").expect("member");
            let empty = db.create_board(&layout_only, "Carol").expect("board");
            db.create_column(&empty, "Todo", None).expect("column");
            let board = db.create_board(&member, "Bob").expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let task = db
                .create_task(&column, None, "Task", 1, None)
                .expect("task");
            db.add_attachment(
                &task,
                "a.bin",
                "application/octet-stream",
                &[1; 512],
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
            let txn = db.db.begin_write().expect("write");
            txn.delete_table(ACCOUNT_USAGE).expect("drop table");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 14)
                .expect("stamp");
            txn.commit().expect("commit");
            (member, layout_only)
        };
        let db = Db::connect(path).expect("upgrade from version 14");
        assert!(stored(&db, &member) > 512);
        assert!(
            stored(&db, &layout_only) > 0,
            "a board and its column count"
        );
        assert_counts_match(&db);
    }

    #[test]
    fn changing_the_board_owner_should_move_the_boards_bytes_and_keep_the_counts_true() {
        let fx = fixture();
        let carol = fx.db.create_user("carol").expect("carol");
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        fx.db
            .add_attachment(
                &task,
                "a.bin",
                "application/octet-stream",
                &[3; 2048],
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
        let binned = fx
            .db
            .create_task(&fx.column, None, "Binned", 1, None)
            .expect("task");
        assert!(fx.db.delete_task(&binned, Actor::SYSTEM).expect("delete"));
        fx.db
            .add_board_feed(&fx.board, "Calendar", "feed-secret")
            .expect("feed");
        let held = stored(&fx.db, &fx.member);
        assert!(held > 2048);
        fx.db
            .set_board_member(&fx.board, "carol", BoardRole::Member)
            .expect("join");
        let outcome = fx
            .db
            .change_board_owner(&fx.board, &fx.member, "carol")
            .expect("change");
        assert!(matches!(outcome, OwnerChange::Changed(_)));
        let (kept, moved) = (stored(&fx.db, &fx.member), stored(&fx.db, &carol));
        assert!(kept + moved < held, "the dropped feed frees its bytes");
        assert!(
            moved > 2048 && kept > 0,
            "the bin row stays with its deleter"
        );
        assert_counts_match(&fx.db);
        assert!(
            fx.db
                .board_by_feed_token("feed-secret")
                .expect("feed")
                .is_none(),
            "the previous owner's calendar feed stops working"
        );
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
                ("carol".to_string(), BoardRole::Owner),
                ("bob".to_string(), BoardRole::Member)
            ]
        );
        assert_eq!(fx.db.joined_boards(&fx.member).expect("joined").len(), 1);
        assert!(fx.db.joined_boards(&carol).expect("joined").is_empty());
        let again = fx
            .db
            .change_board_owner(&fx.board, &fx.member, "carol")
            .expect("change");
        assert!(matches!(again, OwnerChange::NoBoard));
    }

    #[test]
    fn changing_the_board_owner_should_refuse_a_stranger_the_owner_and_a_full_account() {
        let fx = fixture();
        let carol = fx.db.create_user("carol").expect("carol");
        let change = |handle: &str| {
            fx.db
                .change_board_owner(&fx.board, &fx.member, handle)
                .expect("change")
        };
        assert!(matches!(change("carol"), OwnerChange::NotMember));
        assert!(matches!(change("nobody"), OwnerChange::NoAccount));
        assert!(matches!(change("bob"), OwnerChange::IsOwner));
        fx.db
            .set_board_member(&fx.board, "carol", BoardRole::Viewer)
            .expect("join");
        fx.db
            .update_instance_settings(|settings| settings.quotas.boards_per_account = 1)
            .expect("quotas");
        fx.db.create_board(&carol, "Held").expect("board");
        let refused = fx
            .db
            .change_board_owner(&fx.board, &fx.member, "carol")
            .expect_err("quota");
        assert!(refused.downcast_ref::<QuotaExceeded>().is_some());
        assert_counts_match(&fx.db);
        fx.db
            .update_instance_settings(|settings| {
                settings.quotas.boards_per_account = 10;
                settings.quotas.stored_mib_per_account = 0;
            })
            .expect("quotas");
        let full = fx
            .db
            .change_board_owner(&fx.board, &fx.member, "carol")
            .expect_err("bytes");
        assert!(full.downcast_ref::<QuotaExceeded>().is_some());
        assert_counts_match(&fx.db);
        let members = fx.db.board_members(&fx.board).expect("members");
        assert_eq!(
            members
                .iter()
                .map(|member| member.user.username.as_str())
                .collect::<Vec<_>>(),
            ["bob", "carol"]
        );
    }

    /// Logs `count` history events on `task` that `payer` pays for, under `over`.
    fn pay(
        fx: &Fixture,
        task: &str,
        payer: &AccountId,
        over: OverQuota,
        count: usize,
    ) -> DbResult<bool> {
        mutate_task_as(&fx.db.db, task, over, Actor::SYSTEM, |row| {
            for _ in 0..count {
                push_column_event(
                    row,
                    HistoryKind::TimeLogged,
                    "Logged 5m".to_string(),
                    Actor::SYSTEM,
                    None,
                );
                if let Some(event) = row.history.last_mut() {
                    event.paid_by = Some(payer.as_str().to_string());
                }
            }
        })
    }

    /// Returns the bytes of the stored row of `task`.
    #[must_use]
    fn row_len(db: &Db, task: &str) -> u64 {
        let row = db.task(task).expect("read").expect("task");
        serde_json::to_vec(&row).expect("json").len() as u64
    }

    #[test]
    fn usage_should_charge_a_paid_history_event_to_its_payer() {
        let fx = fixture();
        let carol = fx.db.create_user("carol").expect("carol");
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        let (owner_before, row_before) = (stored(&fx.db, &fx.member), row_len(&fx.db, &task));
        assert_eq!(stored(&fx.db, &carol), 0);

        assert!(pay(&fx, &task, &carol, OverQuota::Refuse, 1).expect("pay"));

        let row = fx.db.task(&task).expect("read").expect("task");
        let event = row.history.last().expect("event");
        assert_eq!(event.paid_by.as_deref(), Some(carol.as_str()));
        let paid = serde_json::to_vec(event).expect("json").len() as u64 + 1;
        assert_eq!(stored(&fx.db, &carol), paid);
        let grown = row_len(&fx.db, &task) - row_before;
        assert_eq!(stored(&fx.db, &fx.member), owner_before + grown - paid);
        assert_counts_match(&fx.db);
    }

    #[test]
    fn usage_should_stay_true_when_a_paid_task_is_trimmed_deleted_or_purged() {
        let fx = fixture();
        let carol = fx.db.create_user("carol").expect("carol");
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        let full = Db::MAX_HISTORY_EVENTS;
        assert!(pay(&fx, &task, &carol, OverQuota::Allow, full).expect("pay"));
        assert_counts_match(&fx.db);
        assert!(stored(&fx.db, &carol) > 0);

        assert!(pay(&fx, &task, &carol, OverQuota::Allow, 3).expect("pay"));
        let row = fx.db.task(&task).expect("read").expect("task");
        assert_eq!(row.history.len(), full);
        assert_counts_match(&fx.db);

        assert!(fx.db.delete_task(&task, Actor::SYSTEM).expect("delete"));
        assert_eq!(stored(&fx.db, &carol), 0);
        assert_counts_match(&fx.db);

        let after = Utc::now().timestamp() + (BIN_RETENTION_DAYS + 1) * SECONDS_PER_DAY;
        assert_eq!(fx.db.purge_bin(after).expect("purge"), Some(1));
        assert_eq!(stored(&fx.db, &carol), 0);
        assert_counts_match(&fx.db);
    }

    #[test]
    fn usage_should_release_a_payers_share_when_its_board_goes_or_its_task_returns() {
        let fx = fixture();
        let carol = fx.db.create_user("carol").expect("carol");
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        assert!(pay(&fx, &task, &carol, OverQuota::Refuse, 2).expect("pay"));
        let paid = stored(&fx.db, &carol);
        assert!(paid > 0);

        assert!(fx.db.delete_task(&task, Actor::SYSTEM).expect("delete"));
        assert_eq!(stored(&fx.db, &carol), 0);
        let now = Utc::now().timestamp();
        fx.db
            .restore_task(&task, now, WipCheck::Force, Actor::SYSTEM)
            .expect("restore");
        assert!(stored(&fx.db, &carol) > 0);
        assert_counts_match(&fx.db);

        fx.db.create_board(&fx.member, "Other").expect("board");
        fx.db.delete_board(&fx.board).expect("delete");
        assert_eq!(stored(&fx.db, &carol), 0);
        assert_counts_match(&fx.db);
    }

    #[test]
    fn changing_the_board_owner_should_leave_a_payers_share_where_it_was() {
        let fx = fixture();
        let carol = fx.db.create_user("carol").expect("carol");
        let dave = fx.db.create_user("dave").expect("dave");
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        assert!(pay(&fx, &task, &dave, OverQuota::Refuse, 2).expect("pay"));
        let paid = stored(&fx.db, &dave);
        assert!(paid > 0);
        fx.db
            .set_board_member(&fx.board, "carol", BoardRole::Member)
            .expect("join");
        let outcome = fx
            .db
            .change_board_owner(&fx.board, &fx.member, "carol")
            .expect("change");
        assert!(matches!(outcome, OwnerChange::Changed(_)));
        assert_eq!(stored(&fx.db, &dave), paid);
        assert!(stored(&fx.db, &carol) > 0);
        assert_counts_match(&fx.db);
    }

    #[test]
    fn txn_store_task_should_refuse_a_payer_past_its_quota() {
        let fx = fixture();
        let carol = fx.db.create_user("carol").expect("carol");
        let task = fx
            .db
            .create_task(&fx.column, None, "Task", 1, None)
            .expect("task");
        fx.db
            .update_instance_settings(|settings| settings.quotas.stored_mib_per_account = 1)
            .expect("quotas");
        let txn = fx.db.db.begin_write().expect("write");
        txn_count_usage(&txn, carol.as_str(), MIB - 8, 0).expect("fill");
        txn.commit().expect("commit");
        let held = stored(&fx.db, &carol);
        let history = fx
            .db
            .task(&task)
            .expect("read")
            .expect("task")
            .history
            .len();

        let refused = pay(&fx, &task, &carol, OverQuota::Refuse, 1).expect_err("past the quota");
        assert!(refused.downcast_ref::<QuotaExceeded>().is_some());
        assert_eq!(stored(&fx.db, &carol), held);
        let row = fx.db.task(&task).expect("read").expect("task");
        assert_eq!(row.history.len(), history);

        assert!(pay(&fx, &task, &carol, OverQuota::Allow, 1).expect("server write"));
        assert!(stored(&fx.db, &carol) > held);
    }
}
