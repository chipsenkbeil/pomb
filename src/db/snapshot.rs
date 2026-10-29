use std::fmt;

use redb::{ReadOnlyDatabase, ReadTransaction, ReadableDatabase};

use super::{
    ACCOUNT_SETTINGS, ATTACHMENT_DATA, BOARDS_BY_OWNER, COLORS_BY_BOARD, COLUMNS, COLUMNS_BY_BOARD,
    DbResult, ENTRIES_BY_TASK, FIELD_DEFS, FIELDS_BY_BOARD, SCHEMA_VERSION, SWIMLANES,
    SWIMLANES_BY_BOARD, TASK_COLORS, TASKS, TASKS_BY_COLUMN, TIME_ENTRIES, USERS, index_in,
    owned_board_in, read_in, read_schema_version, refuse_newer, rows_in,
};
use crate::models::{
    AccountId, BoardRow, BreakActivity, ColorRow, ColumnRow, FieldDefRow, Settings, SwimlaneRow,
    TaskHead, TaskRow, TimeEntryRow, UserRow,
};

/// A task a link names, as the board archive describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkTarget {
    /// The name of the task the link names.
    pub task_name: String,
    /// The name of the board that task sits on.
    pub board_name: String,
}

/// One board of one account, read through a single read transaction, so every read sees the
/// same committed state however long the archive takes to write.
pub struct BoardSnapshot {
    /// The read transaction every read goes through.
    txn: ReadTransaction,
    /// The account that owns the board.
    account: AccountId,
    /// The board as it was when the snapshot opened.
    board: BoardRow,
}

impl fmt::Debug for BoardSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoardSnapshot")
            .field("account", &self.account)
            .field("board", &self.board.id)
            .finish_non_exhaustive()
    }
}

impl BoardSnapshot {
    /// Returns the snapshot of board `board_id` read through `txn`, or [`None`] when `account`
    /// does not own it.
    pub(super) fn open(
        txn: ReadTransaction,
        account: &AccountId,
        board_id: &str,
    ) -> DbResult<Option<Self>> {
        let Some(board) = owned_board_in(&txn, account, board_id)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            txn,
            account: account.clone(),
            board,
        }))
    }

    /// Returns the board row.
    #[must_use]
    pub fn board(&self) -> &BoardRow {
        &self.board
    }

    /// Returns the account with `id`, if it still exists.
    pub fn user(&self, id: &str) -> DbResult<Option<UserRow>> {
        read_in::<UserRow>(&self.txn, USERS, id)
    }

    /// Returns the board's columns in board order.
    pub fn columns(&self) -> DbResult<Vec<ColumnRow>> {
        let mut columns = Vec::new();
        for id in index_in(&self.txn, COLUMNS_BY_BOARD, &self.board.id)? {
            columns.extend(read_in::<ColumnRow>(&self.txn, COLUMNS, &id)?);
        }
        columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(columns)
    }

    /// Returns the board's swimlanes in board order.
    pub fn swimlanes(&self) -> DbResult<Vec<SwimlaneRow>> {
        let mut lanes = Vec::new();
        for id in index_in(&self.txn, SWIMLANES_BY_BOARD, &self.board.id)? {
            lanes.extend(read_in::<SwimlaneRow>(&self.txn, SWIMLANES, &id)?);
        }
        lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(lanes)
    }

    /// Returns the board's task colors in palette order.
    pub fn colors(&self) -> DbResult<Vec<ColorRow>> {
        let mut colors = Vec::new();
        for id in index_in(&self.txn, COLORS_BY_BOARD, &self.board.id)? {
            let color = read_in::<ColorRow>(&self.txn, TASK_COLORS, &id)?;
            colors.extend(color.map(ColorRow::with_palette_hexes));
        }
        colors.sort_by(|a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(colors)
    }

    /// Returns the board's custom field definitions in board order.
    pub fn fields(&self) -> DbResult<Vec<FieldDefRow>> {
        let mut fields = Vec::new();
        for id in index_in(&self.txn, FIELDS_BY_BOARD, &self.board.id)? {
            fields.extend(read_in::<FieldDefRow>(&self.txn, FIELD_DEFS, &id)?);
        }
        fields.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        Ok(fields)
    }

    /// Returns the break activities of the owning account, which time entries name by id.
    pub fn break_activities(&self) -> DbResult<Vec<BreakActivity>> {
        let settings = read_in::<Settings>(&self.txn, ACCOUNT_SETTINGS, self.account.as_str())?;
        Ok(settings.map(|row| row.break_activities).unwrap_or_default())
    }

    /// Returns the ids of every task on the board, column by column in board order.
    pub fn task_ids(&self) -> DbResult<Vec<String>> {
        let mut ids = Vec::new();
        for column in self.columns()? {
            ids.extend(index_in(&self.txn, TASKS_BY_COLUMN, &column.id)?);
        }
        Ok(ids)
    }

    /// Returns task `id` as stored, or [`None`] when it is gone.
    pub fn task(&self, id: &str) -> DbResult<Option<TaskRow>> {
        read_in::<TaskRow>(&self.txn, TASKS, id)
    }

    /// Returns the time entries logged on task `task_id`, oldest first.
    pub fn time_entries(&self, task_id: &str) -> DbResult<Vec<TimeEntryRow>> {
        let mut entries = Vec::new();
        for id in index_in(&self.txn, ENTRIES_BY_TASK, task_id)? {
            entries.extend(read_in::<TimeEntryRow>(&self.txn, TIME_ENTRIES, &id)?);
        }
        entries.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(entries)
    }

    /// Returns the task and board names of task `id` when the account owns it.
    pub fn link_target(&self, id: &str) -> DbResult<Option<LinkTarget>> {
        let Some(head) = read_in::<TaskHead>(&self.txn, TASKS, id)? else {
            return Ok(None);
        };
        let Some(column) = read_in::<ColumnRow>(&self.txn, COLUMNS, &head.column_id)? else {
            return Ok(None);
        };
        let Some(board) = owned_board_in(&self.txn, &self.account, &column.board_id)? else {
            return Ok(None);
        };
        Ok(Some(LinkTarget {
            task_name: head.name,
            board_name: board.name,
        }))
    }

    /// Returns whether bytes are stored for attachment `id`.
    pub fn has_attachment(&self, id: &str) -> DbResult<bool> {
        let table = self.txn.open_table(ATTACHMENT_DATA)?;
        Ok(table.get(id)?.is_some())
    }

    /// Returns the bytes of attachment `id`, or [`None`] when none are stored.
    pub fn attachment(&self, id: &str) -> DbResult<Option<Vec<u8>>> {
        let table = self.txn.open_table(ATTACHMENT_DATA)?;
        Ok(table.get(id)?.map(|bytes| bytes.value().to_vec()))
    }
}

/// A database file opened read-only for offline reading, which never writes to it, repairs it or
/// upgrades it.
pub struct OfflineDb {
    /// The file, which redb locks against a writer for as long as it is open.
    database: ReadOnlyDatabase,
}

impl fmt::Debug for OfflineDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfflineDb").finish_non_exhaustive()
    }
}

impl OfflineDb {
    /// Opens the database file at `path` for reading.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaTooNew`](super::SchemaTooNew) for a file from a newer binary, and an error
    /// naming the way out for a file that a server holds open, one that was not closed cleanly,
    /// or one at an older schema version than this binary reads.
    pub fn open(path: &str) -> DbResult<Self> {
        let database = match ReadOnlyDatabase::open(path) {
            Ok(database) => database,
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(format!(
                    "database {path} is held open by a running pomb; stop it first, or download \
                     the archive from the board menu"
                )
                .into());
            }
            Err(redb::DatabaseError::RepairAborted) => {
                return Err(format!(
                    "database {path} was not closed cleanly; start pomb on it once so redb can \
                     repair it, stop pomb, then archive"
                )
                .into());
            }
            Err(error) => return Err(format!("database {path}: {error}").into()),
        };
        let found = read_schema_version(&database)?;
        refuse_newer(path, found, false)?;
        if found != Some(SCHEMA_VERSION) {
            let stamp = found.map_or_else(|| "none".to_string(), |version| version.to_string());
            return Err(format!(
                "database {path} has schema version {stamp}, but this pomb binary reads schema \
                 version {SCHEMA_VERSION}; start pomb on it once to upgrade it, stop pomb, then \
                 archive"
            )
            .into());
        }
        Ok(Self { database })
    }

    /// Returns every account, ordered by handle.
    pub fn accounts(&self) -> DbResult<Vec<UserRow>> {
        let txn = self.database.begin_read()?;
        let mut users: Vec<UserRow> = rows_in(&txn.open_table(USERS)?)?;
        users.sort_by(|a, b| a.username.cmp(&b.username).then_with(|| a.id.cmp(&b.id)));
        Ok(users)
    }

    /// Returns the boards `account` owns, in board order.
    pub fn boards(&self, account: &AccountId) -> DbResult<Vec<BoardRow>> {
        let txn = self.database.begin_read()?;
        let mut boards = Vec::new();
        for id in index_in(&txn, BOARDS_BY_OWNER, account.as_str())? {
            boards.extend(owned_board_in(&txn, account, &id)?);
        }
        boards.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(boards)
    }

    /// Returns the snapshot of board `board_id`, or [`None`] when `account` does not own it.
    pub fn snapshot(&self, account: &AccountId, board_id: &str) -> DbResult<Option<BoardSnapshot>> {
        BoardSnapshot::open(self.database.begin_read()?, account, board_id)
    }
}
