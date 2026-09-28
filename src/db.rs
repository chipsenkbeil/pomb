//! redb-backed storage: table definitions, data-access methods, first-run seeding.
//!
//! Pure Rust, no C bindings: the whole database is a single `kanban.redb` file.
//! Records are JSON values keyed by id; parent -> child lookups use multimap
//! index tables maintained inside the same write transaction, so they stay
//! consistent.
//!
//! redb allows only one write transaction at a time (`begin_write` blocks while
//! another is open), so every method below opens its transaction, does its
//! work, and commits before returning -- no transaction is ever held across
//! `.await`, and callers can invoke these synchronously from async handlers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use redb::{
    Database, MultimapTableDefinition, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition, WriteTransaction,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use uuid::Uuid;

use crate::models::{
    ActiveTimer, BoardRow, ColumnRow, SessionRow, Settings, SwimlaneRow, TaskRow, TimeEntryRow,
    UserRow,
};

pub type DbResult<T> = Result<T, Box<dyn std::error::Error>>;

// ---- Tables: id -> JSON row ----

const USERS: TableDefinition<&str, &[u8]> = TableDefinition::new("users");
const USERNAMES: TableDefinition<&str, &str> = TableDefinition::new("usernames");
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
const BOARDS: TableDefinition<&str, &[u8]> = TableDefinition::new("boards");
const COLUMNS: TableDefinition<&str, &[u8]> = TableDefinition::new("columns");
const SWIMLANES: TableDefinition<&str, &[u8]> = TableDefinition::new("swimlanes");
const TASKS: TableDefinition<&str, &[u8]> = TableDefinition::new("tasks");
const TIME_ENTRIES: TableDefinition<&str, &[u8]> = TableDefinition::new("time_entries");
/// Single-row table (`"app"` -> JSON [`Settings`]).
const SETTINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("settings");
/// Single-row table (`"timer"` -> JSON [`ActiveTimer`]); absent when idle.
const ACTIVE_TIMER: TableDefinition<&str, &[u8]> = TableDefinition::new("active_timer");

// ---- Multimap indexes: parent id -> child id ----

const COLUMNS_BY_BOARD: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("columns_by_board");
const SWIMLANES_BY_BOARD: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("swimlanes_by_board");
const TASKS_BY_COLUMN: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("tasks_by_column");
const ENTRIES_BY_TASK: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("entries_by_task");

/// Thin wrapper around the redb database handle.
#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
}

/// Read one JSON row by id.
fn read_one<T: DeserializeOwned>(
    db: &Database,
    table: TableDefinition<&str, &[u8]>,
    key: &str,
) -> DbResult<Option<T>> {
    let txn = db.begin_read()?;
    let tbl = txn.open_table(table)?;
    match tbl.get(key)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

/// Write one JSON row inside an open write transaction.
fn write_one<T: Serialize>(
    txn: &WriteTransaction,
    table: TableDefinition<&str, &[u8]>,
    key: &str,
    value: &T,
) -> DbResult<()> {
    let bytes = serde_json::to_vec(value)?;
    let mut tbl = txn.open_table(table)?;
    tbl.insert(key, bytes.as_slice())?;
    Ok(())
}

/// Load a row, apply `f`, and store it back. Returns false when the id is unknown.
fn mutate<T, F>(
    db: &Database,
    table: TableDefinition<&str, &[u8]>,
    key: &str,
    f: F,
) -> DbResult<bool>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce(&mut T),
{
    let txn = db.begin_write()?;
    let found = {
        let mut tbl = txn.open_table(table)?;
        let current: Option<T> = tbl
            .get(key)?
            .map(|guard| serde_json::from_slice(guard.value()))
            .transpose()?;
        match current {
            Some(mut value) => {
                f(&mut value);
                let bytes = serde_json::to_vec(&value)?;
                tbl.insert(key, bytes.as_slice())?;
                true
            }
            None => false,
        }
    };
    txn.commit()?;
    Ok(found)
}

fn mmap_insert(
    txn: &WriteTransaction,
    table: MultimapTableDefinition<&str, &str>,
    key: &str,
    value: &str,
) -> DbResult<()> {
    let mut tbl = txn.open_multimap_table(table)?;
    tbl.insert(key, value)?;
    Ok(())
}

fn mmap_remove(
    txn: &WriteTransaction,
    table: MultimapTableDefinition<&str, &str>,
    key: &str,
    value: &str,
) -> DbResult<()> {
    let mut tbl = txn.open_multimap_table(table)?;
    tbl.remove(key, value)?;
    Ok(())
}

fn mmap_get(
    db: &Database,
    table: MultimapTableDefinition<&str, &str>,
    key: &str,
) -> DbResult<Vec<String>> {
    let txn = db.begin_read()?;
    let tbl = txn.open_multimap_table(table)?;
    let mut out = Vec::new();
    for guard in tbl.get(key)? {
        out.push(guard?.value().to_owned());
    }
    Ok(out)
}

impl Db {
    /// Open (creating when needed) the database file at `path`, create tables
    /// on first run, then seed the starter board. The admin account is NOT
    /// created here: the /setup page handles first-run account creation.
    pub fn connect(path: &str) -> DbResult<Self> {
        // redb won't create missing parent directories itself.
        if let Some(parent) = Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let db = Database::create(&path)?;
        let txn = db.begin_write()?;
        {
            txn.open_table(USERS)?;
            txn.open_table(USERNAMES)?;
            txn.open_table(SESSIONS)?;
            txn.open_table(BOARDS)?;
            txn.open_table(COLUMNS)?;
            txn.open_table(SWIMLANES)?;
            txn.open_table(TASKS)?;
            txn.open_table(TIME_ENTRIES)?;
            txn.open_table(SETTINGS)?;
            txn.open_table(ACTIVE_TIMER)?;
            txn.open_multimap_table(COLUMNS_BY_BOARD)?;
            txn.open_multimap_table(SWIMLANES_BY_BOARD)?;
            txn.open_multimap_table(TASKS_BY_COLUMN)?;
            txn.open_multimap_table(ENTRIES_BY_TASK)?;
        }
        txn.commit()?;

        let this = Self { db: Arc::new(db) };
        this.seed()?;
        Ok(this)
    }

    /// First-run seeding: the starter "General" board, created only when no
    /// boards exist, so restarting never duplicates anything.
    fn seed(&self) -> DbResult<()> {
        if self.table_len(BOARDS)? == 0 {
            let board_id = self.create_board("General")?;
            let columns: [(&str, Option<i64>, bool); 4] = [
                ("Work To-do", None, false),
                ("Do today", None, false),
                ("In progress", Some(3), false),
                ("Done", None, true),
            ];
            for (name, wip_limit, is_done) in columns {
                self.create_column_full(&board_id, name, wip_limit, is_done)?;
            }
            for name in ["PERSONAL TO-DO", "BACKLOG"] {
                self.create_swimlane(&board_id, name)?;
            }
        }
        Ok(())
    }

    fn table_len(&self, table: TableDefinition<&str, &[u8]>) -> DbResult<u64> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(table)?.len()?)
    }

    // ---- Users & sessions ----

    pub fn create_user(&self, username: &str, password_hash: &str) -> DbResult<String> {
        let id = Uuid::new_v4().to_string();
        let row = UserRow {
            id: id.clone(),
            username: username.to_string(),
            password_hash: password_hash.to_string(),
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, USERS, &id, &row)?;
        {
            let mut idx = txn.open_table(USERNAMES)?;
            idx.insert(username, id.as_str())?;
        }
        txn.commit()?;
        Ok(id)
    }

    pub fn user_by_username(&self, username: &str) -> DbResult<Option<UserRow>> {
        let id: Option<String> = {
            let txn = self.db.begin_read()?;
            let idx = txn.open_table(USERNAMES)?;
            idx.get(username)?.map(|guard| guard.value().to_owned())
        };
        match id {
            Some(id) => read_one(&self.db, USERS, &id),
            None => Ok(None),
        }
    }

    pub fn create_session(&self, user_id: &str) -> DbResult<String> {
        let token = Uuid::new_v4().to_string();
        let row = SessionRow {
            user_id: user_id.to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, SESSIONS, &token, &row)?;
        txn.commit()?;
        Ok(token)
    }

    /// Returns `(user id, username)` for a session token.
    pub fn user_for_token(&self, token: &str) -> DbResult<Option<(String, String)>> {
        let session: Option<SessionRow> = read_one(&self.db, SESSIONS, token)?;
        match session {
            Some(session) => {
                let user: Option<UserRow> = read_one(&self.db, USERS, &session.user_id)?;
                Ok(user.map(|user| (user.id, user.username)))
            }
            None => Ok(None),
        }
    }

    pub fn delete_session(&self, token: &str) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(SESSIONS)?;
            tbl.remove(token)?;
        }
        txn.commit()?;
        Ok(())
    }

    // ---- Boards ----

    pub fn create_board(&self, name: &str) -> DbResult<String> {
        let id = Uuid::new_v4().to_string();
        let row = BoardRow {
            id: id.clone(),
            name: name.to_string(),
            position: 0,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, BOARDS, &id, &row)?;
        txn.commit()?;
        Ok(id)
    }

    pub fn get_board(&self, id: &str) -> DbResult<Option<BoardRow>> {
        read_one(&self.db, BOARDS, id)
    }

    pub fn board_exists(&self, id: &str) -> DbResult<bool> {
        Ok(self.get_board(id)?.is_some())
    }

    pub fn first_board_id(&self) -> DbResult<Option<String>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(BOARDS)?;
        let mut best: Option<(i64, String)> = None;
        for item in tbl.iter()? {
            let (key, value) = item?;
            let row: BoardRow = serde_json::from_slice(value.value())?;
            let id = key.value().to_owned();
            if best
                .as_ref()
                .map(|(pos, _)| row.position < *pos)
                .unwrap_or(true)
            {
                best = Some((row.position, id));
            }
        }
        Ok(best.map(|(_, id)| id))
    }

    // ---- Columns ----

    pub fn get_column(&self, id: &str) -> DbResult<Option<ColumnRow>> {
        read_one(&self.db, COLUMNS, id)
    }

    /// Columns of a board, ordered by position (then id for stability).
    pub fn list_columns(&self, board_id: &str) -> DbResult<Vec<ColumnRow>> {
        let mut columns = Vec::new();
        for id in mmap_get(&self.db, COLUMNS_BY_BOARD, board_id)? {
            if let Some(column) = self.get_column(&id)? {
                columns.push(column);
            }
        }
        columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(columns)
    }

    pub fn create_column(
        &self,
        board_id: &str,
        name: &str,
        wip_limit: Option<i64>,
    ) -> DbResult<String> {
        self.create_column_full(board_id, name, wip_limit, false)
    }

    fn create_column_full(
        &self,
        board_id: &str,
        name: &str,
        wip_limit: Option<i64>,
        is_done: bool,
    ) -> DbResult<String> {
        let id = Uuid::new_v4().to_string();
        let position = self
            .list_columns(board_id)?
            .into_iter()
            .map(|column| column.position)
            .max()
            .map(|max| max + 1)
            .unwrap_or(0);
        let row = ColumnRow {
            id: id.clone(),
            board_id: board_id.to_string(),
            name: name.to_string(),
            position,
            wip_limit,
            is_done,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, COLUMNS, &id, &row)?;
        mmap_insert(&txn, COLUMNS_BY_BOARD, board_id, &id)?;
        txn.commit()?;
        Ok(id)
    }

    pub fn rename_column(&self, id: &str, name: &str) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.name = name.to_string();
        })
    }

    pub fn set_column_wip(&self, id: &str, wip_limit: Option<i64>) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.wip_limit = wip_limit;
        })
    }

    pub fn set_column_done(&self, id: &str, is_done: bool) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.is_done = is_done;
        })
    }

    /// Reorder a column within its board; positions are renumbered densely.
    /// Returns false when the column is unknown.
    pub fn move_column(&self, id: &str, position: i64) -> DbResult<bool> {
        let column = match self.get_column(id)? {
            Some(column) => column,
            None => return Ok(false),
        };
        let mut columns = self.list_columns(&column.board_id)?;
        columns.retain(|column| column.id != id);
        let at = position.clamp(0, columns.len() as i64) as usize;
        let mut order: Vec<String> = columns.iter().map(|column| column.id.clone()).collect();
        order.insert(at, id.to_string());

        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(COLUMNS)?;
            for (index, column_id) in order.iter().enumerate() {
                let current: Option<ColumnRow> = tbl
                    .get(column_id.as_str())?
                    .map(|guard| serde_json::from_slice(guard.value()))
                    .transpose()?;
                if let Some(mut row) = current {
                    row.position = index as i64;
                    let bytes = serde_json::to_vec(&row)?;
                    tbl.insert(column_id.as_str(), bytes.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// Delete a column. The caller checks it holds no tasks first.
    pub fn delete_column(&self, id: &str) -> DbResult<bool> {
        let column = match self.get_column(id)? {
            Some(column) => column,
            None => return Ok(false),
        };
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(COLUMNS)?;
            tbl.remove(id)?;
            mmap_remove(&txn, COLUMNS_BY_BOARD, &column.board_id, id)?;
        }
        txn.commit()?;
        Ok(true)
    }

    pub fn count_tasks_in_column(&self, column_id: &str) -> DbResult<usize> {
        Ok(mmap_get(&self.db, TASKS_BY_COLUMN, column_id)?.len())
    }

    // ---- Swimlanes ----

    pub fn get_swimlane(&self, id: &str) -> DbResult<Option<SwimlaneRow>> {
        read_one(&self.db, SWIMLANES, id)
    }

    /// Swimlanes of a board, ordered by position (then id for stability).
    pub fn list_swimlanes(&self, board_id: &str) -> DbResult<Vec<SwimlaneRow>> {
        let mut lanes = Vec::new();
        for id in mmap_get(&self.db, SWIMLANES_BY_BOARD, board_id)? {
            if let Some(lane) = self.get_swimlane(&id)? {
                lanes.push(lane);
            }
        }
        lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(lanes)
    }

    pub fn create_swimlane(&self, board_id: &str, name: &str) -> DbResult<String> {
        let id = Uuid::new_v4().to_string();
        let position = self
            .list_swimlanes(board_id)?
            .into_iter()
            .map(|lane| lane.position)
            .max()
            .map(|max| max + 1)
            .unwrap_or(0);
        let row = SwimlaneRow {
            id: id.clone(),
            board_id: board_id.to_string(),
            name: name.to_string(),
            position,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, SWIMLANES, &id, &row)?;
        mmap_insert(&txn, SWIMLANES_BY_BOARD, board_id, &id)?;
        txn.commit()?;
        Ok(id)
    }

    pub fn rename_swimlane(&self, id: &str, name: &str) -> DbResult<bool> {
        mutate(&self.db, SWIMLANES, id, |lane: &mut SwimlaneRow| {
            lane.name = name.to_string();
        })
    }

    /// Reorder a swimlane within its board; positions are renumbered densely.
    /// Returns false when the swimlane is unknown.
    pub fn move_swimlane(&self, id: &str, position: i64) -> DbResult<bool> {
        let lane = match self.get_swimlane(id)? {
            Some(lane) => lane,
            None => return Ok(false),
        };
        let mut lanes = self.list_swimlanes(&lane.board_id)?;
        lanes.retain(|lane| lane.id != id);
        let at = position.clamp(0, lanes.len() as i64) as usize;
        let mut order: Vec<String> = lanes.iter().map(|lane| lane.id.clone()).collect();
        order.insert(at, id.to_string());

        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(SWIMLANES)?;
            for (index, lane_id) in order.iter().enumerate() {
                let current: Option<SwimlaneRow> = tbl
                    .get(lane_id.as_str())?
                    .map(|guard| serde_json::from_slice(guard.value()))
                    .transpose()?;
                if let Some(mut row) = current {
                    row.position = index as i64;
                    let bytes = serde_json::to_vec(&row)?;
                    tbl.insert(lane_id.as_str(), bytes.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// Delete a swimlane. The caller checks it holds no tasks first.
    pub fn delete_swimlane(&self, id: &str) -> DbResult<bool> {
        let lane = match self.get_swimlane(id)? {
            Some(lane) => lane,
            None => return Ok(false),
        };
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(SWIMLANES)?;
            tbl.remove(id)?;
            mmap_remove(&txn, SWIMLANES_BY_BOARD, &lane.board_id, id)?;
        }
        txn.commit()?;
        Ok(true)
    }

    pub fn count_tasks_in_swimlane(&self, swimlane_id: &str) -> DbResult<usize> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TASKS)?;
        let mut count = 0;
        for item in tbl.iter()? {
            let (_, value) = item?;
            let task: TaskRow = serde_json::from_slice(value.value())?;
            if task.swimlane_id.as_deref() == Some(swimlane_id) {
                count += 1;
            }
        }
        Ok(count)
    }

    // ---- Tasks ----

    pub fn get_task(&self, id: &str) -> DbResult<Option<TaskRow>> {
        read_one(&self.db, TASKS, id)
    }

    /// All tasks (id + name), for the time-dialog task autocomplete.
    pub fn all_tasks(&self) -> DbResult<Vec<TaskRow>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TASKS)?;
        let mut out: Vec<TaskRow> = Vec::new();
        for item in tbl.iter()? {
            let (_, value) = item?;
            out.push(serde_json::from_slice(value.value())?);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn get_entry(&self, id: &str) -> DbResult<Option<TimeEntryRow>> {
        read_one(&self.db, TIME_ENTRIES, id)
    }

    pub fn task_total_minutes(&self, task_id: &str) -> DbResult<i64> {
        let mut total = 0;
        for entry_id in mmap_get(&self.db, ENTRIES_BY_TASK, task_id)? {
            if let Some(entry) = self.get_entry(&entry_id)? {
                total += entry.minutes;
            }
        }
        Ok(total)
    }

    /// One task with its total logged minutes filled in.
    pub fn get_task_with_minutes(&self, id: &str) -> DbResult<Option<TaskRow>> {
        match self.get_task(id)? {
            Some(mut task) => {
                task.total_minutes = self.task_total_minutes(id)?;
                Ok(Some(task))
            }
            None => Ok(None),
        }
    }

    /// Highest position in a column x swimlane cell, if the cell isn't empty.
    fn max_task_position(
        &self,
        column_id: &str,
        swimlane_id: Option<&str>,
    ) -> DbResult<Option<f64>> {
        let mut max: Option<f64> = None;
        for task_id in mmap_get(&self.db, TASKS_BY_COLUMN, column_id)? {
            if let Some(task) = self.get_task(&task_id)? {
                if task.swimlane_id.as_deref() == swimlane_id
                    && max.map(|current| task.position > current).unwrap_or(true)
                {
                    max = Some(task.position);
                }
            }
        }
        Ok(max)
    }

    /// Create a task appended at the end of its cell; returns its id.
    pub fn create_task(
        &self,
        column_id: &str,
        swimlane_id: Option<&str>,
        name: &str,
        size: i64,
    ) -> DbResult<String> {
        let id = Uuid::new_v4().to_string();
        let position = self
            .max_task_position(column_id, swimlane_id)?
            .map(|max| max + 1.0)
            .unwrap_or(0.0);
        let row = TaskRow {
            id: id.clone(),
            column_id: column_id.to_string(),
            swimlane_id: swimlane_id.map(str::to_string),
            name: name.to_string(),
            description: String::new(),
            size,
            position,
            created_at: Utc::now().to_rfc3339(),
            completed_at: None,
            total_minutes: 0,
            pomodori_completed: 0,
            interruptions: 0,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TASKS, &id, &row)?;
        mmap_insert(&txn, TASKS_BY_COLUMN, column_id, &id)?;
        txn.commit()?;
        Ok(id)
    }

    /// Patch a task's name/description/size. Returns false when unknown.
    pub fn update_task(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        size: Option<i64>,
    ) -> DbResult<bool> {
        mutate(&self.db, TASKS, id, |task: &mut TaskRow| {
            if let Some(name) = name {
                task.name = name.to_string();
            }
            if let Some(description) = description {
                task.description = description.to_string();
            }
            if let Some(size) = size {
                task.size = size;
            }
        })
    }

    /// Move a task to a new column/swimlane/position. Crossing into a done
    /// column stamps `completed_at`; crossing out clears it. Both affected
    /// cells are renumbered densely afterwards.
    pub fn move_task(
        &self,
        id: &str,
        column_id: &str,
        swimlane_id: Option<&str>,
        position: f64,
    ) -> DbResult<()> {
        let task = self
            .get_task(id)?
            .ok_or_else(|| format!("task not found: {id}"))?;
        let target = self
            .get_column(column_id)?
            .ok_or_else(|| format!("unknown column: {column_id}"))?;
        let old_column_id = task.column_id.clone();
        let old_swimlane_id = task.swimlane_id.clone();

        let was_done = self
            .get_column(&old_column_id)?
            .map(|column| column.is_done)
            .unwrap_or(false);
        let completed_at = if target.is_done && !was_done {
            Some(Utc::now().to_rfc3339())
        } else if !target.is_done && was_done {
            None
        } else {
            task.completed_at.clone()
        };

        // Keep the current swimlane when the caller doesn't name one.
        let new_swimlane_id = swimlane_id.map(str::to_string).or(old_swimlane_id.clone());

        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            let row = TaskRow {
                column_id: column_id.to_string(),
                swimlane_id: new_swimlane_id.clone(),
                position,
                completed_at,
                ..task
            };
            let bytes = serde_json::to_vec(&row)?;
            tbl.insert(id, bytes.as_slice())?;
            if column_id != old_column_id {
                mmap_remove(&txn, TASKS_BY_COLUMN, &old_column_id, id)?;
                mmap_insert(&txn, TASKS_BY_COLUMN, column_id, id)?;
            }
        }
        txn.commit()?;

        self.renumber_cell(column_id, new_swimlane_id.as_deref())?;
        if column_id != old_column_id || new_swimlane_id.as_deref() != old_swimlane_id.as_deref() {
            self.renumber_cell(&old_column_id, old_swimlane_id.as_deref())?;
        }
        Ok(())
    }

    /// Rewrite positions in one column x swimlane cell as dense 0..n ordering.
    pub fn renumber_cell(&self, column_id: &str, swimlane_id: Option<&str>) -> DbResult<()> {
        let mut tasks = Vec::new();
        for task_id in mmap_get(&self.db, TASKS_BY_COLUMN, column_id)? {
            if let Some(task) = self.get_task(&task_id)? {
                if task.swimlane_id.as_deref() == swimlane_id {
                    tasks.push(task);
                }
            }
        }
        tasks.sort_by(|a, b| {
            a.position
                .total_cmp(&b.position)
                .then_with(|| a.id.cmp(&b.id))
        });

        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            for (index, mut task) in tasks.into_iter().enumerate() {
                task.position = index as f64;
                let bytes = serde_json::to_vec(&task)?;
                tbl.insert(task.id.as_str(), bytes.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Delete a task and its time entries. Returns false when unknown.
    pub fn delete_task(&self, id: &str) -> DbResult<bool> {
        let task = match self.get_task(id)? {
            Some(task) => task,
            None => return Ok(false),
        };
        let entry_ids = mmap_get(&self.db, ENTRIES_BY_TASK, id)?;
        let txn = self.db.begin_write()?;
        {
            let mut tasks = txn.open_table(TASKS)?;
            tasks.remove(id)?;
            mmap_remove(&txn, TASKS_BY_COLUMN, &task.column_id, id)?;
            let mut entries = txn.open_table(TIME_ENTRIES)?;
            for entry_id in &entry_ids {
                entries.remove(entry_id.as_str())?;
                mmap_remove(&txn, ENTRIES_BY_TASK, id, entry_id)?;
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// Every task on a board with minutes filled in, ordered for rendering:
    /// column position, swimlane position (no swimlane sorts last), task
    /// position, task id.
    pub fn board_tasks(&self, board_id: &str) -> DbResult<Vec<TaskRow>> {
        let columns = self.list_columns(board_id)?;
        let lane_positions: HashMap<String, i64> = self
            .list_swimlanes(board_id)?
            .iter()
            .map(|lane| (lane.id.clone(), lane.position))
            .collect();
        let mut out = Vec::new();
        for column in &columns {
            let mut tasks = Vec::new();
            for task_id in mmap_get(&self.db, TASKS_BY_COLUMN, &column.id)? {
                if let Some(mut task) = self.get_task(&task_id)? {
                    task.total_minutes = self.task_total_minutes(&task_id)?;
                    tasks.push(task);
                }
            }
            tasks.sort_by(|a, b| {
                let lane_pos = |task: &TaskRow| {
                    task.swimlane_id
                        .as_deref()
                        .and_then(|sid| lane_positions.get(sid))
                        .copied()
                        .unwrap_or(9999)
                };
                lane_pos(a)
                    .cmp(&lane_pos(b))
                    .then_with(|| a.position.total_cmp(&b.position))
                    .then_with(|| a.id.cmp(&b.id))
            });
            out.extend(tasks);
        }
        Ok(out)
    }

    // ---- Time entries ----

    pub fn create_entry(&self, task_id: &str, minutes: i64, note: &str) -> DbResult<String> {
        // log_time verifies the task exists first, so this always returns Some.
        Ok(self
            .create_entry_full(Some(task_id), minutes, note, "manual", false, None)?
            .unwrap_or_default())
    }

    /// Time entries for a task, newest first.
    pub fn list_entries(&self, task_id: &str) -> DbResult<Vec<TimeEntryRow>> {
        let mut entries = Vec::new();
        for entry_id in mmap_get(&self.db, ENTRIES_BY_TASK, task_id)? {
            if let Some(entry) = self.get_entry(&entry_id)? {
                entries.push(entry);
            }
        }
        entries.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(entries)
    }

    // ---- Settings & timer ----

    pub fn user_count(&self) -> DbResult<u64> {
        self.table_len(USERS)
    }

    /// App settings; defaults when never saved.
    pub fn get_settings(&self) -> DbResult<Settings> {
        let stored: Option<Settings> = read_one(&self.db, SETTINGS, "app")?;
        Ok(stored.unwrap_or_default())
    }

    pub fn update_settings(&self, settings: &Settings) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        write_one(&txn, SETTINGS, "app", settings)?;
        txn.commit()?;
        Ok(())
    }

    pub fn get_active_timer(&self) -> DbResult<Option<ActiveTimer>> {
        read_one(&self.db, ACTIVE_TIMER, "timer")
    }

    pub fn set_active_timer(&self, timer: &ActiveTimer) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        write_one(&txn, ACTIVE_TIMER, "timer", timer)?;
        txn.commit()?;
        Ok(())
    }

    pub fn clear_active_timer(&self) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(ACTIVE_TIMER)?;
            tbl.remove("timer")?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Bump a task's completed-pomodori counter. No-op when unknown.
    pub fn record_pomodoro_complete(&self, task_id: &str) -> DbResult<()> {
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.pomodori_completed += 1;
        })?;
        Ok(())
    }

    /// Bump a task's interruption counter. No-op when unknown.
    pub fn record_interruption(&self, task_id: &str) -> DbResult<()> {
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.interruptions += 1;
        })?;
        Ok(())
    }

    /// Create a time entry with full timer metadata. Returns `None` (and
    /// writes nothing) when there is no task or the task is gone, so timer
    /// sessions never leave orphan entries behind.
    pub fn create_entry_full(
        &self,
        task_id: Option<&str>,
        minutes: i64,
        note: &str,
        kind: &str,
        interrupted: bool,
        interrupt_reason: Option<&str>,
    ) -> DbResult<Option<String>> {
        let task_id = match task_id {
            Some(id) if self.get_task(id)?.is_some() => id,
            _ => return Ok(None),
        };
        let id = Uuid::new_v4().to_string();
        let row = TimeEntryRow {
            id: id.clone(),
            task_id: task_id.to_string(),
            minutes,
            note: note.to_string(),
            started_at: Utc::now().to_rfc3339(),
            kind: kind.to_string(),
            interrupted,
            interrupt_reason: interrupt_reason.map(str::to_string),
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TIME_ENTRIES, &id, &row)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        txn.commit()?;
        Ok(Some(id))
    }

    /// Create a manual time entry with an explicit start time (RFC3339).
    /// Used by the "Add time manually" dialog. Returns None when the task
    /// does not exist.
    pub fn create_entry_at(
        &self,
        task_id: &str,
        minutes: i64,
        note: &str,
        started_at: &str,
    ) -> DbResult<Option<String>> {
        if self.get_task(task_id)?.is_none() {
            return Ok(None);
        }
        let id = Uuid::new_v4().to_string();
        let row = TimeEntryRow {
            id: id.clone(),
            task_id: task_id.to_string(),
            minutes,
            note: note.to_string(),
            started_at: started_at.to_string(),
            kind: "manual".to_string(),
            interrupted: false,
            interrupt_reason: None,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TIME_ENTRIES, &id, &row)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        txn.commit()?;
        Ok(Some(id))
    }

    /// Update a time entry (date/time, task reassignment, note).
    /// Returns false when the entry does not exist; the task must exist
    /// (checked by the caller).
    pub fn update_entry(
        &self,
        entry_id: &str,
        task_id: &str,
        minutes: i64,
        note: &str,
        started_at: &str,
    ) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let mut tbl = txn.open_table(TIME_ENTRIES)?;
        let existing: Option<TimeEntryRow> = tbl
            .get(entry_id)?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?;
        let mut row = match existing {
            Some(r) => r,
            None => return Ok(false),
        };
        let old_task = row.task_id.clone();
        row.task_id = task_id.to_string();
        row.minutes = minutes;
        row.note = note.to_string();
        row.started_at = started_at.to_string();
        // Insert via the already-open table (write_one would re-open it).
        let bytes = serde_json::to_vec(&row)?;
        tbl.insert(entry_id, bytes.as_slice())?;
        drop(tbl);
        if old_task != task_id {
            mmap_remove(&txn, ENTRIES_BY_TASK, &old_task, entry_id)?;
            mmap_insert(&txn, ENTRIES_BY_TASK, task_id, entry_id)?;
        }
        txn.commit()?;
        Ok(true)
    }

    /// All time entries (used by the timer log and entry lookup).
    pub fn all_entries(&self) -> DbResult<Vec<TimeEntryRow>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TIME_ENTRIES)?;
        let mut out: Vec<TimeEntryRow> = Vec::new();
        for item in tbl.iter()? {
            let (_, value) = item?;
            out.push(serde_json::from_slice(value.value())?);
        }
        Ok(out)
    }

    /// Today's time entries (UTC date), newest first, with their task names.
    pub fn entries_today(&self) -> DbResult<Vec<(TimeEntryRow, String)>> {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TIME_ENTRIES)?;
        let mut out = Vec::new();
        for item in tbl.iter()? {
            let (_, value) = item?;
            let entry: TimeEntryRow = serde_json::from_slice(value.value())?;
            if entry.started_at.get(..10) == Some(today.as_str()) {
                let name = self
                    .get_task(&entry.task_id)?
                    .map(|task| task.name)
                    .unwrap_or_else(|| "(deleted task)".to_string());
                out.push((entry, name));
            }
        }
        out.sort_by(|a, b| {
            b.0.started_at
                .cmp(&a.0.started_at)
                .then_with(|| b.0.id.cmp(&a.0.id))
        });
        Ok(out)
    }
}
