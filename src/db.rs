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
    standard_color, ActiveTimer, ApiTokenRecord, BoardRow, BoardTemplateRow, ColorRow, ColumnRow,
    SessionRow, Settings, SwimlaneRow, TaskRow, TimeEntryRow, UserRow,
};

pub type DbResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Typed outcome for [`Db::delete_color`] so handlers can map refusals
/// to 400 without parsing error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDeleteOutcome {
    Deleted,
    NotFound,
    /// Refused: cannot delete the board's default color.
    RefusedDefault,
    /// Refused: this many tasks still use the color.
    RefusedInUse(usize),
}

/// Typed outcome for [`Db::delete_template`]: built-in templates are
/// protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateDeleteOutcome {
    Deleted,
    NotFound,
    RefusedBuiltIn,
}

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
const API_TOKENS: TableDefinition<&str, &[u8]> = TableDefinition::new("api_tokens");
/// id -> JSON [`ColorRow`]; per-board task colors (KanbanFlow parity).
const TASK_COLORS: TableDefinition<&str, &[u8]> = TableDefinition::new("task_colors");
/// id -> JSON [`BoardTemplateRow`]; reusable board recipes.
const TEMPLATES: TableDefinition<&str, &[u8]> = TableDefinition::new("templates");

// ---- Multimap indexes: parent id -> child id ----

const COLUMNS_BY_BOARD: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("columns_by_board");
const SWIMLANES_BY_BOARD: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("swimlanes_by_board");
const TASKS_BY_COLUMN: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("tasks_by_column");
const ENTRIES_BY_TASK: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("entries_by_task");
/// board id -> color id for [`ColorRow`] rows.
const COLORS_BY_BOARD: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("colors_by_board");

/// Thin wrapper around the redb database handle.
#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
}

/// Shared application state (database handle), used by the HTTP layer.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
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

// ---- Per-board color defaults (KanbanFlow parity) ----

/// Per-color defaults for the Pomodoro scheme:
/// (value, label, enabled, is_default, sort_order). Hex values are fixed
/// per standard color (see `STANDARD_COLORS` in models.rs).
fn pomodoro_color_specs() -> Vec<(&'static str, &'static str, bool, bool, i64)> {
    vec![
        ("yellow", "1 Pomodoro", true, true, 1),
        ("green", "2 Pomodori", true, false, 2),
        ("blue", "3 Pomodori", true, false, 3),
        ("red", ">3 Pomodori", true, false, 4),
        ("orange", "Orange", false, false, 5),
        ("purple", "Purple", false, false, 6),
        ("magenta", "Magenta", false, false, 7),
        ("cyan", "Cyan", false, false, 8),
        ("brown", "Brown", false, false, 9),
        ("white", "White", false, false, 10),
    ]
}

/// The built-in "Pomodoro board" template snapshot: the 10-color palette
/// with the Pomodoro scheme, four columns, and two swimlanes.
fn pomodoro_template_snapshot() -> serde_json::Value {
    let colors: Vec<serde_json::Value> = pomodoro_color_specs()
        .iter()
        .map(|(value, label, enabled, is_default, sort_order)| {
            let (bg, border, light, _) = standard_color(value).expect("known standard color");
            serde_json::json!({
                "value": value,
                "label": label,
                "description": "",
                "background_hex": bg,
                "border_hex": border,
                "light_hex": light,
                "enabled": enabled,
                "is_default": is_default,
                "sort_order": sort_order,
            })
        })
        .collect();
    serde_json::json!({
        "colors": colors,
        "columns": [
            {"name": "Work To-do", "wip_limit": null, "is_done": false, "position": 0},
            {"name": "Do today", "wip_limit": null, "is_done": false, "position": 1},
            {"name": "In progress", "wip_limit": 3, "is_done": false, "position": 2},
            {"name": "Done", "wip_limit": null, "is_done": true, "position": 3},
        ],
        "swimlanes": [
            {"name": "PERSONAL TO-DO", "position": 0},
            {"name": "BACKLOG", "position": 1},
        ],
    })
}

/// Parameters for inserting a [`ColorRow`] (keeps `insert_color_row`
/// under clippy's argument-count lint).
struct NewColor<'a> {
    value: &'a str,
    label: &'a str,
    description: &'a str,
    enabled: bool,
    is_default: bool,
    sort_order: i64,
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

        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        {
            txn.open_table(USERS)?;
            txn.open_table(USERNAMES)?;
            txn.open_table(SESSIONS)?;
            txn.open_table(API_TOKENS)?;
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
            txn.open_table(TASK_COLORS)?;
            txn.open_table(TEMPLATES)?;
            txn.open_multimap_table(COLORS_BY_BOARD)?;
        }
        txn.commit()?;

        let this = Self { db: Arc::new(db) };
        this.seed()?;
        Ok(this)
    }

    /// First-run seeding: the built-in "Pomodoro board" template (inserted
    /// exactly once), then the starter "General" board built by applying
    /// that template — only when no boards exist, so restarting never
    /// duplicates anything.
    fn seed(&self) -> DbResult<()> {
        let template_id = self.ensure_builtin_template()?;
        if self.table_len(BOARDS)? == 0 {
            self.apply_template(&template_id, "General")?;
        }
        Ok(())
    }

    /// The name of the shipped built-in board template.
    pub const BUILTIN_TEMPLATE_NAME: &'static str = "Pomodoro board";

    /// Insert the built-in "Pomodoro board" template when no built-in row
    /// with that name exists yet; returns its id either way (idempotent).
    fn ensure_builtin_template(&self) -> DbResult<String> {
        for template in self.list_templates()? {
            if template.built_in && template.name == Self::BUILTIN_TEMPLATE_NAME {
                return Ok(template.id);
            }
        }
        let snapshot = pomodoro_template_snapshot();
        let row = BoardTemplateRow {
            id: Uuid::new_v4().to_string(),
            name: Self::BUILTIN_TEMPLATE_NAME.to_string(),
            description: "Pomodoro board: task colors for 1/2/3/>3 pomodori plus a ready-to-use column and swimlane layout.".to_string(),
            built_in: true,
            snapshot,
        };
        let id = row.id.clone();
        let txn = self.db.begin_write()?;
        write_one(&txn, TEMPLATES, &id, &row)?;
        txn.commit()?;
        Ok(id)
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

    // ---- API tokens (agent access) ----

    pub fn create_api_token(&self, record: &ApiTokenRecord) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        write_one(&txn, API_TOKENS, &record.id, record)?;
        txn.commit()?;
        Ok(())
    }

    pub fn list_api_tokens(&self) -> DbResult<Vec<ApiTokenRecord>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(API_TOKENS)?;
        let mut out = Vec::new();
        for entry in tbl.iter()? {
            let (_, guard) = entry?;
            out.push(serde_json::from_slice(guard.value())?);
        }
        out.sort_by_key(|t: &ApiTokenRecord| t.created_at);
        Ok(out)
    }

    /// Find a token by verifying `raw` against every stored salted hash.
    /// Token counts are tiny (single admin), so a linear scan is fine and
    /// avoids a second index table.
    pub fn find_api_token(&self, raw: &str) -> DbResult<Option<ApiTokenRecord>> {
        for record in self.list_api_tokens()? {
            if crate::auth::verify_api_token(&record, raw) {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    pub fn delete_api_token(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let removed = {
            let mut tbl = txn.open_table(API_TOKENS)?;
            let removed = tbl.remove(id)?.is_some();
            removed
        };
        txn.commit()?;
        Ok(removed)
    }

    pub fn touch_api_token(&self, id: &str, now: i64) -> DbResult<()> {
        let _ = mutate(&self.db, API_TOKENS, id, |record: &mut ApiTokenRecord| {
            record.last_used_at = Some(now);
        });
        Ok(())
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

    /// All boards, ordered by position (then id for stability).
    pub fn list_boards(&self) -> DbResult<Vec<BoardRow>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(BOARDS)?;
        let mut out: Vec<BoardRow> = Vec::new();
        for item in tbl.iter()? {
            let (_, value) = item?;
            out.push(serde_json::from_slice(value.value())?);
        }
        out.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
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
            description: String::new(),
            collapsed: false,
            config_json: "{}".to_string(),
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

    /// Column description (shown in the column settings dialog).
    pub fn set_column_description(&self, id: &str, description: &str) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.description = description.to_string();
        })
    }

    /// Collapse/expand a column on the board.
    pub fn set_column_collapsed(&self, id: &str, collapsed: bool) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.collapsed = collapsed;
        })
    }

    /// Opaque settings bag for the column dialog (column sums, sorting,
    /// group-by-date, display options). Callers validate it is JSON.
    pub fn set_column_config(&self, id: &str, config_json: &str) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.config_json = config_json.to_string();
        })
    }

    // ---- Task colors (KanbanFlow parity) ----

    /// Maximum allowed color label length (KanbanFlow renames cap at 50).
    pub const MAX_COLOR_LABEL_LEN: usize = 50;

    /// Validate a color label for create/update: non-empty, at most 50
    /// chars. Handlers map the error to 400.
    pub fn validate_color_label(label: &str) -> Result<(), String> {
        let label = label.trim();
        if label.is_empty() {
            return Err("color label is required".to_string());
        }
        if label.chars().count() > Self::MAX_COLOR_LABEL_LEN {
            return Err(format!(
                "color label must be at most {} characters",
                Self::MAX_COLOR_LABEL_LEN
            ));
        }
        Ok(())
    }

    pub fn get_color(&self, id: &str) -> DbResult<Option<ColorRow>> {
        read_one(&self.db, TASK_COLORS, id)
    }

    /// Colors of a board, ordered by sort_order (then id for stability).
    /// Backfills the standard palette on boards that predate colors, so
    /// this is safe to call unconditionally.
    pub fn list_colors(&self, board_id: &str) -> DbResult<Vec<ColorRow>> {
        self.ensure_board_colors(board_id)?;
        let mut colors = Vec::new();
        for id in mmap_get(&self.db, COLORS_BY_BOARD, board_id)? {
            if let Some(color) = self.get_color(&id)? {
                colors.push(color);
            }
        }
        colors.sort_by(|a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(colors)
    }

    /// The board's default color (used for new tasks), if one is set.
    pub fn default_color(&self, board_id: &str) -> DbResult<Option<ColorRow>> {
        for color in self.list_colors(board_id)? {
            if color.is_default {
                return Ok(Some(color));
            }
        }
        Ok(None)
    }

    /// Idempotent: when a board has zero color rows, insert the 10 standard
    /// colors with the Pomodoro defaults (4 enabled, Chip's labels, yellow
    /// default). This backfills boards created before the color feature —
    /// no migration step needed, and old DB files pick colors up lazily.
    pub fn ensure_board_colors(&self, board_id: &str) -> DbResult<()> {
        if !mmap_get(&self.db, COLORS_BY_BOARD, board_id)?.is_empty() {
            return Ok(());
        }
        for (value, label, enabled, is_default, sort_order) in pomodoro_color_specs() {
            self.insert_color_row(
                board_id,
                NewColor {
                    value,
                    label,
                    description: "",
                    enabled,
                    is_default,
                    sort_order,
                },
            )?;
        }
        Ok(())
    }

    fn insert_color_row(&self, board_id: &str, color: NewColor<'_>) -> DbResult<String> {
        let (background_hex, border_hex, light_hex, _) = standard_color(color.value)
            .ok_or_else(|| format!("unknown color value: {}", color.value))?;
        let id = Uuid::new_v4().to_string();
        let row = ColorRow {
            id: id.clone(),
            board_id: board_id.to_string(),
            value: color.value.to_string(),
            label: color.label.to_string(),
            description: color.description.to_string(),
            background_hex: background_hex.to_string(),
            border_hex: border_hex.to_string(),
            light_hex: light_hex.to_string(),
            enabled: color.enabled,
            is_default: color.is_default,
            sort_order: color.sort_order,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TASK_COLORS, &id, &row)?;
        mmap_insert(&txn, COLORS_BY_BOARD, board_id, &id)?;
        txn.commit()?;
        Ok(id)
    }

    /// Create a color slot for a standard value on a board. Hex values are
    /// fixed per standard color; the caller picks label/description.
    /// Errors when the value is unknown or already has a slot on the board.
    pub fn create_color(
        &self,
        board_id: &str,
        value: &str,
        label: Option<&str>,
        description: Option<&str>,
    ) -> DbResult<String> {
        let default_label = crate::models::STANDARD_COLORS
            .iter()
            .find(|(value_name, _, _, _, _)| *value_name == value)
            .map(|(_, _, _, _, label)| *label)
            .ok_or_else(|| format!("unknown color value: {value}"))?;
        let label = label
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(default_label);
        Self::validate_color_label(label)
            .map_err(|message| -> Box<dyn std::error::Error> { message.into() })?;
        if self
            .list_colors(board_id)?
            .iter()
            .any(|color| color.value == value)
        {
            return Err(format!("color '{value}' already exists on this board").into());
        }
        let sort_order = self
            .list_colors(board_id)?
            .into_iter()
            .map(|color| color.sort_order)
            .max()
            .map(|max| max + 1)
            .unwrap_or(0);
        self.insert_color_row(
            board_id,
            NewColor {
                value,
                label,
                description: description.unwrap_or_default(),
                enabled: true,
                is_default: false,
                sort_order,
            },
        )
    }

    /// Rename/describe/enable a color and/or make it the board default.
    /// A given label must be 1..=50 chars after trimming; violations are an
    /// error. All requested changes apply atomically in one transaction.
    /// Returns false when the color is unknown.
    pub fn update_color(
        &self,
        id: &str,
        label: Option<&str>,
        description: Option<&str>,
        enabled: Option<bool>,
        is_default: Option<bool>,
    ) -> DbResult<bool> {
        if let Some(label) = label {
            Self::validate_color_label(label)
                .map_err(|message| -> Box<dyn std::error::Error> { message.into() })?;
        }
        let color = match self.get_color(id)? {
            Some(color) => color,
            None => return Ok(false),
        };
        let ids = mmap_get(&self.db, COLORS_BY_BOARD, color.board_id.as_str())?;
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASK_COLORS)?;
            if is_default == Some(true) {
                // Clear the default flag on the board's other colors first.
                for other_id in &ids {
                    if other_id == id {
                        continue;
                    }
                    let current: Option<ColorRow> = tbl
                        .get(other_id.as_str())?
                        .map(|guard| serde_json::from_slice(guard.value()))
                        .transpose()?;
                    if let Some(mut row) = current {
                        if row.is_default {
                            row.is_default = false;
                            let bytes = serde_json::to_vec(&row)?;
                            tbl.insert(other_id.as_str(), bytes.as_slice())?;
                        }
                    }
                }
            }
            let current: Option<ColorRow> = tbl
                .get(id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            if let Some(mut row) = current {
                if let Some(label) = label {
                    row.label = label.trim().to_string();
                }
                if let Some(description) = description {
                    row.description = description.to_string();
                }
                if let Some(enabled) = enabled {
                    row.enabled = enabled;
                }
                if let Some(is_default) = is_default {
                    row.is_default = is_default;
                }
                let bytes = serde_json::to_vec(&row)?;
                tbl.insert(id, bytes.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// Make `color_id` the board's default color, clearing the flag on the
    /// board's other colors. Returns false when the color is unknown or on
    /// a different board.
    pub fn set_board_default_color(&self, board_id: &str, color_id: &str) -> DbResult<bool> {
        let color = match self.get_color(color_id)? {
            Some(color) if color.board_id == board_id => color,
            _ => return Ok(false),
        };
        let _ = color;
        let ids = mmap_get(&self.db, COLORS_BY_BOARD, board_id)?;
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASK_COLORS)?;
            for cid in &ids {
                let current: Option<ColorRow> = tbl
                    .get(cid.as_str())?
                    .map(|guard| serde_json::from_slice(guard.value()))
                    .transpose()?;
                if let Some(mut row) = current {
                    row.is_default = cid == color_id;
                    let bytes = serde_json::to_vec(&row)?;
                    tbl.insert(cid.as_str(), bytes.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// Reorder a color within its board; positions are renumbered densely.
    /// Returns false when the color is unknown.
    pub fn move_color(&self, id: &str, position: i64) -> DbResult<bool> {
        let color = match self.get_color(id)? {
            Some(color) => color,
            None => return Ok(false),
        };
        let mut colors = self.list_colors(&color.board_id)?;
        colors.retain(|color| color.id != id);
        let at = position.clamp(0, colors.len() as i64) as usize;
        let mut order: Vec<String> = colors.iter().map(|color| color.id.clone()).collect();
        order.insert(at, id.to_string());

        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASK_COLORS)?;
            for (index, color_id) in order.iter().enumerate() {
                let current: Option<ColorRow> = tbl
                    .get(color_id.as_str())?
                    .map(|guard| serde_json::from_slice(guard.value()))
                    .transpose()?;
                if let Some(mut row) = current {
                    row.sort_order = index as i64;
                    let bytes = serde_json::to_vec(&row)?;
                    tbl.insert(color_id.as_str(), bytes.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// How many tasks currently carry this color.
    pub fn count_tasks_with_color(&self, color_id: &str) -> DbResult<usize> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TASKS)?;
        let mut count = 0;
        for item in tbl.iter()? {
            let (_, value) = item?;
            let task: TaskRow = serde_json::from_slice(value.value())?;
            if task.color_id.as_deref() == Some(color_id) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Typed outcome for color deletion so handlers can map refusals to 400.
    pub fn delete_color(&self, id: &str) -> DbResult<ColorDeleteOutcome> {
        let color = match self.get_color(id)? {
            Some(color) => color,
            None => return Ok(ColorDeleteOutcome::NotFound),
        };
        if color.is_default {
            return Ok(ColorDeleteOutcome::RefusedDefault);
        }
        let in_use = self.count_tasks_with_color(id)?;
        if in_use > 0 {
            return Ok(ColorDeleteOutcome::RefusedInUse(in_use));
        }
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASK_COLORS)?;
            tbl.remove(id)?;
            mmap_remove(&txn, COLORS_BY_BOARD, &color.board_id, id)?;
        }
        txn.commit()?;
        Ok(ColorDeleteOutcome::Deleted)
    }

    // ---- Board templates ----

    /// All templates: built-ins first, then by name.
    pub fn list_templates(&self) -> DbResult<Vec<BoardTemplateRow>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TEMPLATES)?;
        let mut out: Vec<BoardTemplateRow> = Vec::new();
        for item in tbl.iter()? {
            let (_, value) = item?;
            out.push(serde_json::from_slice(value.value())?);
        }
        out.sort_by(|a, b| {
            b.built_in
                .cmp(&a.built_in)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    pub fn get_template(&self, id: &str) -> DbResult<Option<BoardTemplateRow>> {
        read_one(&self.db, TEMPLATES, id)
    }

    /// Typed outcome: built-in templates cannot be deleted.
    pub fn delete_template(&self, id: &str) -> DbResult<TemplateDeleteOutcome> {
        let template = match self.get_template(id)? {
            Some(template) => template,
            None => return Ok(TemplateDeleteOutcome::NotFound),
        };
        if template.built_in {
            return Ok(TemplateDeleteOutcome::RefusedBuiltIn);
        }
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TEMPLATES)?;
            tbl.remove(id)?;
        }
        txn.commit()?;
        Ok(TemplateDeleteOutcome::Deleted)
    }

    /// Capture a board's colors (all, with their config), columns, and
    /// swimlanes as a reusable template. Returns None when the board is
    /// unknown.
    pub fn save_board_as_template(
        &self,
        board_id: &str,
        name: &str,
        description: &str,
    ) -> DbResult<Option<String>> {
        if !self.board_exists(board_id)? {
            return Ok(None);
        }
        let colors: Vec<serde_json::Value> = self
            .list_colors(board_id)?
            .iter()
            .map(|color| {
                serde_json::json!({
                    "value": color.value,
                    "label": color.label,
                    "description": color.description,
                    "background_hex": color.background_hex,
                    "border_hex": color.border_hex,
                    "light_hex": color.light_hex,
                    "enabled": color.enabled,
                    "is_default": color.is_default,
                    "sort_order": color.sort_order,
                })
            })
            .collect();
        let columns: Vec<serde_json::Value> = self
            .list_columns(board_id)?
            .iter()
            .map(|column| {
                serde_json::json!({
                    "name": column.name,
                    "wip_limit": column.wip_limit,
                    "is_done": column.is_done,
                    "position": column.position,
                })
            })
            .collect();
        let swimlanes: Vec<serde_json::Value> = self
            .list_swimlanes(board_id)?
            .iter()
            .map(|lane| {
                serde_json::json!({
                    "name": lane.name,
                    "position": lane.position,
                })
            })
            .collect();
        let snapshot = serde_json::json!({
            "colors": colors,
            "columns": columns,
            "swimlanes": swimlanes,
        });
        let id = Uuid::new_v4().to_string();
        let row = BoardTemplateRow {
            id: id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            built_in: false,
            snapshot,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TEMPLATES, &id, &row)?;
        txn.commit()?;
        Ok(Some(id))
    }

    /// Build a new board from a template snapshot: board row, its colors,
    /// columns, and swimlanes. Returns the new board id, or None when the
    /// template is unknown.
    pub fn apply_template(
        &self,
        template_id: &str,
        new_board_name: &str,
    ) -> DbResult<Option<String>> {
        let template = match self.get_template(template_id)? {
            Some(template) => template,
            None => return Ok(None),
        };
        let board_id = self.create_board(new_board_name)?;
        let snapshot = &template.snapshot;

        if let Some(colors) = snapshot.get("colors").and_then(|value| value.as_array()) {
            let mut colors: Vec<&serde_json::Value> = colors.iter().collect();
            colors.sort_by_key(|color| {
                color
                    .get("sort_order")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0)
            });
            for color in colors {
                let value = color
                    .get("value")
                    .and_then(|value| value.as_str())
                    .unwrap_or("yellow");
                if standard_color(value).is_none() {
                    continue;
                }
                self.insert_color_row(
                    &board_id,
                    NewColor {
                        value,
                        label: color
                            .get("label")
                            .and_then(|value| value.as_str())
                            .unwrap_or(value),
                        description: color
                            .get("description")
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        enabled: color
                            .get("enabled")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(true),
                        is_default: color
                            .get("is_default")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false),
                        sort_order: color
                            .get("sort_order")
                            .and_then(|value| value.as_i64())
                            .unwrap_or(0),
                    },
                )?;
            }
        }
        if let Some(columns) = snapshot.get("columns").and_then(|value| value.as_array()) {
            let mut columns: Vec<&serde_json::Value> = columns.iter().collect();
            columns.sort_by_key(|column| {
                column
                    .get("position")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0)
            });
            for column in columns {
                self.create_column_full(
                    &board_id,
                    column
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("To-do"),
                    column.get("wip_limit").and_then(|value| value.as_i64()),
                    column
                        .get("is_done")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                )?;
            }
        }
        if let Some(lanes) = snapshot.get("swimlanes").and_then(|value| value.as_array()) {
            let mut lanes: Vec<&serde_json::Value> = lanes.iter().collect();
            lanes.sort_by_key(|lane| {
                lane.get("position")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0)
            });
            for lane in lanes {
                self.create_swimlane(
                    &board_id,
                    lane.get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("Swimlane"),
                )?;
            }
        }

        // Snapshots that carry no colors (hand-edited templates) still get
        // the standard palette, and every board keeps exactly one default.
        self.ensure_board_colors(&board_id)?;
        self.ensure_single_default(&board_id)?;
        Ok(Some(board_id))
    }

    /// When a board has no default color, promote the first enabled color
    /// (falling back to the first color). Boards built from valid
    /// snapshots already have exactly one default; this only repairs the
    /// zero-default case.
    fn ensure_single_default(&self, board_id: &str) -> DbResult<()> {
        let colors = self.list_colors(board_id)?;
        if colors.iter().any(|color| color.is_default) {
            return Ok(());
        }
        if let Some(first) = colors
            .iter()
            .find(|color| color.enabled)
            .or_else(|| colors.first())
        {
            let id = first.id.clone();
            self.set_board_default_color(board_id, &id)?;
        }
        Ok(())
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
    /// `color_id` selects the task's color (validated by the caller);
    /// None keeps the legacy size-based coloring. Also backfills the
    /// board's color palette for boards created before colors existed.
    pub fn create_task(
        &self,
        column_id: &str,
        swimlane_id: Option<&str>,
        name: &str,
        size: i64,
        color_id: Option<&str>,
    ) -> DbResult<String> {
        let id = Uuid::new_v4().to_string();
        let position = self
            .max_task_position(column_id, swimlane_id)?
            .map(|max| max + 1.0)
            .unwrap_or(0.0);
        if let Some(column) = self.get_column(column_id)? {
            self.ensure_board_colors(&column.board_id)?;
        }
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
            color_id: color_id.map(str::to_string),
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TASKS, &id, &row)?;
        mmap_insert(&txn, TASKS_BY_COLUMN, column_id, &id)?;
        txn.commit()?;
        Ok(id)
    }

    /// Patch a task's name/description/size/color. `color_id` is
    /// double-optional: None leaves it alone, Some(None) clears it,
    /// Some(Some(id)) assigns it. Returns false when unknown.
    pub fn update_task(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        size: Option<i64>,
        color_id: Option<Option<&str>>,
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
            match color_id {
                Some(Some(color_id)) => task.color_id = Some(color_id.to_string()),
                Some(None) => task.color_id = None,
                None => {}
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
