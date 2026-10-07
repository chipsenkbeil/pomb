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
    SessionRow, Settings, Subtask, SwimlaneRow, TaskAttachment, TaskComment, TaskEvent, TaskRow,
    TimeEntryRow, UserRow,
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
/// attachment id -> raw file bytes (KanbanFlow parity: attachments).
const ATTACHMENT_DATA: TableDefinition<&str, &[u8]> = TableDefinition::new("attachment_data");

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
        ("yellow", "Yellow", true, true, 1),
        ("green", "Green", true, false, 2),
        ("blue", "Blue", true, false, 3),
        ("red", "Red", true, false, 4),
        // KF-151: KanbanFlow enables its standard palette by default, so all
        // remaining standard colors are enabled too (not just the 4 Pomodoro
        // scheme colors).
        ("orange", "Orange", true, false, 5),
        ("purple", "Purple", true, false, 6),
        ("magenta", "Magenta", true, false, 7),
        ("cyan", "Cyan", true, false, 8),
        ("brown", "Brown", true, false, 9),
        ("white", "White", true, false, 10),
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
        // KF-307: starter tasks demonstrating the pomodoro color scheme
        // (tracker-documented expectation: e.g. "Pomodoro 1"; GM-141).
        "tasks": [
            {
                "name": "Pomodoro 1",
                "description": "",
                "size": 1,
                "color_value": "yellow",
                "column": "Work To-do",
                "swimlane": "PERSONAL TO-DO",
                "position": 0.0,
                "due_at": null,
                "due_repeat": null,
                "estimate_hours": null,
                "subtasks": [],
                "labels": []
            },
            {
                "name": "Pomodoro 2",
                "description": "",
                "size": 2,
                "color_value": "green",
                "column": "Do today",
                "swimlane": "PERSONAL TO-DO",
                "position": 0.0,
                "due_at": null,
                "due_repeat": null,
                "estimate_hours": null,
                "subtasks": [],
                "labels": []
            },
            {
                "name": "Pomodoro 3",
                "description": "",
                "size": 3,
                "color_value": "blue",
                "column": "In progress",
                "swimlane": "PERSONAL TO-DO",
                "position": 0.0,
                "due_at": null,
                "due_repeat": null,
                "estimate_hours": null,
                "subtasks": [],
                "labels": []
            },
        ],
    })
}

/// The built-in "Kanban basics" template snapshot: the standard 10-color
/// palette with standard labels, the classic To-do / In progress / Done
/// column layout, and a single "Default" swimlane.
fn kanban_basics_template_snapshot() -> serde_json::Value {
    let colors: Vec<serde_json::Value> = crate::models::STANDARD_COLORS
        .iter()
        .enumerate()
        .map(|(i, (value, _, _, _, standard_label))| {
            let (bg, border, light, _) = standard_color(value).expect("known standard color");
            serde_json::json!({
                "value": value,
                "label": standard_label,
                "description": "",
                "background_hex": bg,
                "border_hex": border,
                "light_hex": light,
                "enabled": true,
                "is_default": *value == "yellow",
                "sort_order": (i as i64) + 1,
            })
        })
        .collect();
    serde_json::json!({
        "colors": colors,
        "columns": [
            {"name": "To-do", "wip_limit": null, "is_done": false, "position": 0},
            {"name": "In progress", "wip_limit": null, "is_done": false, "position": 1},
            {"name": "Done", "wip_limit": null, "is_done": true, "position": 2},
        ],
        "swimlanes": [
            {"name": "Default", "position": 0},
        ],
    })
}

/// One built-in board template: (name, description, snapshot builder).
type BuiltinTemplateSpec = (&'static str, &'static str, fn() -> serde_json::Value);

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
            txn.open_table(ATTACHMENT_DATA)?;
            txn.open_multimap_table(COLORS_BY_BOARD)?;
        }
        txn.commit()?;

        let this = Self { db: Arc::new(db) };
        // KF-222: backfill before seed() so existing boards keep their
        // current rendering (legend ON) while a fresh install's starter
        // board defaults to OFF (KanbanFlow parity).
        this.migrate_legend_default()?;
        this.migrate_pomodoro_plural_labels()?;
        this.seed()?;
        Ok(this)
    }

    /// KF-222: one-time backfill for the per-board "Color legend" toggle.
    /// Boards that predate the toggle keep their current rendering by
    /// defaulting `legend_visible` to true; boards created afterwards read
    /// the key as absent and default to false (KanbanFlow shows no legend
    /// by default). Only fills the key where absent, so an explicit user
    /// choice — including false — is never overwritten. The
    /// `legend_backfill_done` marker makes the pass run exactly once: a
    /// fresh install's second boot must not backfill the starter board that
    /// `seed()` created. Like `ensure_board_colors`, this is a lazy
    /// backfill, not a schema migration.
    fn migrate_legend_default(&self) -> DbResult<()> {
        let done: bool = read_one(&self.db, SETTINGS, "legend_backfill_done")?.unwrap_or(false);
        if done {
            return Ok(());
        }
        let ids: Vec<String> = {
            let txn = self.db.begin_read()?;
            let tbl = txn.open_table(BOARDS)?;
            let mut ids = Vec::new();
            for item in tbl.iter()? {
                let (key, _) = item?;
                ids.push(key.value().to_string());
            }
            ids
        };
        for id in ids {
            let missing = self
                .get_board(&id)?
                .map(|board| {
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                        &board.config_json,
                    )
                    .map(|cfg| !cfg.contains_key("legend_visible"))
                    .unwrap_or(true)
                })
                .unwrap_or(false);
            if missing {
                let mut updates = serde_json::Map::new();
                updates.insert("legend_visible".to_string(), serde_json::Value::Bool(true));
                self.set_board_config(&id, &updates)?;
            }
        }
        let txn = self.db.begin_write()?;
        write_one(&txn, SETTINGS, "legend_backfill_done", &true)?;
        txn.commit()?;
        Ok(())
    }

    /// KF-296: one-time backfill renaming stored pomodoro-count color labels
    /// from the Italian plural to KanbanFlow's "Pomodoros" ("2 Pomodori" →
    /// "2 Pomodoros", etc.). Boards whose colors were backfilled before
    /// Round-2 carry the old labels in their color rows; only exact matches
    /// are rewritten, so a user-customized label is never touched ("1
    /// Pomodoro" is unchanged by design). The `pomodoro_plural_backfill_done`
    /// marker makes the pass run exactly once. Like `migrate_legend_default`,
    /// this is a lazy backfill, not a schema migration.
    fn migrate_pomodoro_plural_labels(&self) -> DbResult<()> {
        let done: bool =
            read_one(&self.db, SETTINGS, "pomodoro_plural_backfill_done")?.unwrap_or(false);
        if done {
            return Ok(());
        }
        const RENAMES: [(&str, &str); 3] = [
            ("2 Pomodori", "2 Pomodoros"),
            ("3 Pomodori", "3 Pomodoros"),
            (">3 Pomodori", ">3 Pomodoros"),
        ];
        let txn = self.db.begin_write()?;
        let updates: Vec<(String, ColorRow)> = {
            let tbl = txn.open_table(TASK_COLORS)?;
            let mut updates = Vec::new();
            for item in tbl.iter()? {
                let (key, guard) = item?;
                let mut row: ColorRow = serde_json::from_slice(guard.value())?;
                if let Some(&(_, new_label)) = RENAMES.iter().find(|(old, _)| row.label == *old) {
                    row.label = new_label.to_string();
                    updates.push((key.value().to_string(), row));
                }
            }
            updates
        };
        for (id, row) in &updates {
            write_one(&txn, TASK_COLORS, id.as_str(), row)?;
        }
        write_one(&txn, SETTINGS, "pomodoro_plural_backfill_done", &true)?;
        txn.commit()?;
        Ok(())
    }

    /// First-run seeding: the built-in board templates (inserted exactly
    /// once each), then the starter "General" board built by applying the
    /// "Pomodoro board" template — only when no boards exist, so restarting
    /// never duplicates anything.
    fn seed(&self) -> DbResult<()> {
        let template_id = self.ensure_builtin_templates(Self::BUILTIN_TEMPLATE_NAME)?;
        if self.table_len(BOARDS)? == 0 {
            self.apply_template(&template_id, "General")?;
        }
        Ok(())
    }

    /// The name of the shipped built-in board template used for the starter
    /// "General" board.
    pub const BUILTIN_TEMPLATE_NAME: &'static str = "Pomodoro board";

    /// The name of the shipped built-in "Kanban basics" board template.
    pub const KANBAN_BASICS_TEMPLATE_NAME: &'static str = "Kanban basics";

    /// Insert every built-in board template that has no built-in row with
    /// its name yet; returns the id of the requested template (idempotent).
    fn ensure_builtin_templates(&self, want: &str) -> DbResult<String> {
        let builtins: Vec<BuiltinTemplateSpec> = vec![
            (
                Self::BUILTIN_TEMPLATE_NAME,
                "Pomodoro board: task colors for 1/2/3/>3 pomodori plus a ready-to-use column and swimlane layout.",
                pomodoro_template_snapshot as fn() -> serde_json::Value,
            ),
            (
                Self::KANBAN_BASICS_TEMPLATE_NAME,
                "Kanban basics: the standard color palette with the classic To-do / In progress / Done column layout.",
                kanban_basics_template_snapshot as fn() -> serde_json::Value,
            ),
        ];
        let mut wanted_id = None;
        for (name, description, snapshot_fn) in &builtins {
            let mut existing: Option<String> = None;
            for template in self.list_templates()? {
                if template.built_in && template.name == *name {
                    existing = Some(template.id);
                    break;
                }
            }
            let id = match existing {
                Some(id) => id,
                None => {
                    let row = BoardTemplateRow {
                        id: Uuid::new_v4().to_string(),
                        name: name.to_string(),
                        description: description.to_string(),
                        built_in: true,
                        snapshot: snapshot_fn(),
                    };
                    let id = row.id.clone();
                    let txn = self.db.begin_write()?;
                    write_one(&txn, TEMPLATES, &id, &row)?;
                    txn.commit()?;
                    id
                }
            };
            if *name == want {
                wanted_id = Some(id);
            }
        }
        wanted_id.ok_or_else(|| format!("unknown built-in template: {want}").into())
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
            config_json: "{}".to_string(),
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

    /// Rename a board. Errors on a blank or over-long name; returns false
    /// when the board is unknown.
    pub fn rename_board(&self, id: &str, name: &str) -> DbResult<bool> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err("board name must be 1-100 characters".into());
        }
        let txn = self.db.begin_write()?;
        let updated = {
            let current: Option<BoardRow> = txn
                .open_table(BOARDS)?
                .get(id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            match current {
                None => false,
                Some(mut row) => {
                    row.name = name.to_string();
                    write_one(&txn, BOARDS, id, &row)?;
                    true
                }
            }
        };
        txn.commit()?;
        Ok(updated)
    }

    /// Merge key/value pairs into a board's opaque config bag (KF-183: e.g.
    /// `legend_visible`). Returns false when the board is unknown.
    pub fn set_board_config(
        &self,
        id: &str,
        updates: &serde_json::Map<String, serde_json::Value>,
    ) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let updated = {
            let current: Option<BoardRow> = txn
                .open_table(BOARDS)?
                .get(id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            match current {
                None => false,
                Some(mut row) => {
                    let mut cfg: serde_json::Map<String, serde_json::Value> =
                        serde_json::from_str(&row.config_json).unwrap_or_default();
                    for (k, v) in updates {
                        cfg.insert(k.clone(), v.clone());
                    }
                    row.config_json = serde_json::to_string(&cfg)?;
                    write_one(&txn, BOARDS, id, &row)?;
                    true
                }
            }
        };
        txn.commit()?;
        Ok(updated)
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

    /// Delete a board and everything in it: columns, swimlanes, tasks,
    /// time entries, and task colors. Returns false when the board id is
    /// unknown. Refuses to delete the last remaining board.
    pub fn delete_board(&self, id: &str) -> DbResult<()> {
        if self.get_board(id)?.is_none() {
            return Err("board not found".into());
        }
        if self.list_boards()?.len() <= 1 {
            return Err("cannot delete the last board".into());
        }
        let column_ids = mmap_get(&self.db, COLUMNS_BY_BOARD, id)?;
        let swimlane_ids = mmap_get(&self.db, SWIMLANES_BY_BOARD, id)?;
        let color_ids = mmap_get(&self.db, COLORS_BY_BOARD, id)?;
        // Collect task ids (and their entry ids) before opening the write txn.
        let mut task_ids: Vec<String> = Vec::new();
        let mut entry_ids: Vec<String> = Vec::new();
        for column_id in &column_ids {
            for task_id in mmap_get(&self.db, TASKS_BY_COLUMN, column_id)? {
                entry_ids.extend(mmap_get(&self.db, ENTRIES_BY_TASK, &task_id)?);
                task_ids.push(task_id);
            }
        }
        let txn = self.db.begin_write()?;
        {
            let mut boards = txn.open_table(BOARDS)?;
            boards.remove(id)?;
            let mut columns = txn.open_table(COLUMNS)?;
            for column_id in &column_ids {
                columns.remove(column_id.as_str())?;
                mmap_remove(&txn, COLUMNS_BY_BOARD, id, column_id)?;
            }
            let mut swimlanes = txn.open_table(SWIMLANES)?;
            for swimlane_id in &swimlane_ids {
                swimlanes.remove(swimlane_id.as_str())?;
                mmap_remove(&txn, SWIMLANES_BY_BOARD, id, swimlane_id)?;
            }
            let mut colors = txn.open_table(TASK_COLORS)?;
            for color_id in &color_ids {
                colors.remove(color_id.as_str())?;
                mmap_remove(&txn, COLORS_BY_BOARD, id, color_id)?;
            }
            let mut tasks = txn.open_table(TASKS)?;
            let mut entries = txn.open_table(TIME_ENTRIES)?;
            for task_id in &task_ids {
                tasks.remove(task_id.as_str())?;
            }
            for entry_id in &entry_ids {
                entries.remove(entry_id.as_str())?;
            }
            // Drop the multimap index entries for tasks/entries.
            for column_id in &column_ids {
                let mut idx = txn.open_multimap_table(TASKS_BY_COLUMN)?;
                idx.remove_all(column_id.as_str())?;
            }
            for task_id in &task_ids {
                let mut idx = txn.open_multimap_table(ENTRIES_BY_TASK)?;
                idx.remove_all(task_id.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
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

    /// Replace a board's whole color palette with a copy of another board's
    /// (KanbanFlow parity: Colors tab "Copy from board"). Old rows are
    /// removed and re-inserted under new ids, preserving each source row's
    /// value, label, description, enabled flag, default flag, and order.
    /// Tasks on the target board keep their color by standard value
    /// (old color id -> new color id for the same value).
    /// Returns the number of colors copied.
    pub fn copy_colors(&self, target_board_id: &str, source_board_id: &str) -> DbResult<usize> {
        if self.get_board(source_board_id)?.is_none() {
            return Err(format!("unknown source board: {source_board_id}").into());
        }
        let source = self.list_colors(source_board_id)?;
        let old_target: Vec<ColorRow> = {
            let mut rows = Vec::new();
            for id in mmap_get(&self.db, COLORS_BY_BOARD, target_board_id)? {
                if let Some(color) = self.get_color(&id)? {
                    rows.push(color);
                }
            }
            rows
        };
        let old_value_by_id: HashMap<String, String> = old_target
            .iter()
            .map(|color| (color.id.clone(), color.value.clone()))
            .collect();

        let mut new_id_by_value: HashMap<String, String> = HashMap::new();
        let mut copies: Vec<ColorRow> = Vec::with_capacity(source.len());
        for color in &source {
            let (background_hex, border_hex, light_hex, _) = standard_color(&color.value)
                .ok_or_else(|| format!("unknown color value: {}", color.value))?;
            let id = Uuid::new_v4().to_string();
            new_id_by_value.insert(color.value.clone(), id.clone());
            copies.push(ColorRow {
                id,
                board_id: target_board_id.to_string(),
                value: color.value.clone(),
                label: color.label.clone(),
                description: color.description.clone(),
                background_hex: background_hex.to_string(),
                border_hex: border_hex.to_string(),
                light_hex: light_hex.to_string(),
                enabled: color.enabled,
                is_default: color.is_default,
                sort_order: color.sort_order,
            });
        }

        let tasks = self.board_tasks(target_board_id)?;
        let txn = self.db.begin_write()?;
        {
            let mut colors_tbl = txn.open_table(TASK_COLORS)?;
            let mut by_board = txn.open_multimap_table(COLORS_BY_BOARD)?;
            for color in &old_target {
                colors_tbl.remove(color.id.as_str())?;
                by_board.remove(target_board_id, color.id.as_str())?;
            }
            for color in &copies {
                let bytes = serde_json::to_vec(color)?;
                colors_tbl.insert(color.id.as_str(), bytes.as_slice())?;
                by_board.insert(target_board_id, color.id.as_str())?;
            }
            let mut tasks_tbl = txn.open_table(TASKS)?;
            for task in &tasks {
                if let Some(color_id) = task.color_id.as_deref() {
                    if let Some(value) = old_value_by_id.get(color_id) {
                        if let Some(new_id) = new_id_by_value.get(value) {
                            let mut updated = task.clone();
                            updated.color_id = Some(new_id.clone());
                            let bytes = serde_json::to_vec(&updated)?;
                            tasks_tbl.insert(task.id.as_str(), bytes.as_slice())?;
                        }
                    }
                }
            }
        }
        txn.commit()?;
        Ok(copies.len())
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
    /// colors with the Pomodoro defaults (all 10 standard colors enabled,
    /// Chip's labels, yellow default). This backfills boards created before
    /// no migration step needed, and old DB files pick colors up lazily.
    pub fn ensure_board_colors(&self, board_id: &str) -> DbResult<()> {
        if !mmap_get(&self.db, COLORS_BY_BOARD, board_id)?.is_empty() {
            return Ok(());
        }
        // Don't backfill colors for a board that doesn't exist (e.g. after
        // deletion); list_colors on a deleted board must stay empty.
        if self.get_board(board_id)?.is_none() {
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

    /// KF-149: every board needs at least one swimlane. The board's add-task
    /// form clones into the first swimlane row's cell, so a board with zero
    /// swimlanes (e.g. created without a template before this fix) leaves the
    /// column "+" buttons silently dead. Backfills a "Default" lane when the
    /// board has none.
    pub fn ensure_default_swimlane(&self, board_id: &str) -> DbResult<()> {
        if !self.list_swimlanes(board_id)?.is_empty() {
            return Ok(());
        }
        // Don't backfill for a board that doesn't exist (e.g. after
        // deletion); list_swimlanes on a deleted board must stay empty.
        if self.get_board(board_id)?.is_none() {
            return Ok(());
        }
        self.create_swimlane(board_id, "Default")?;
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

    /// Capture a board's colors (all, with their config), columns,
    /// swimlanes, and tasks as a reusable template. Returns None when the
    /// board is unknown.
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
        // Tasks are captured with their content and layout metadata (column
        // and swimlane by name, color by value) so instantiation on a fresh
        // board reproduces them. Ephemeral per-instance state — history,
        // comments, attachments, member assignments, timer stats — is not
        // part of a template.
        let column_names: HashMap<String, String> = self
            .list_columns(board_id)?
            .into_iter()
            .map(|column| (column.id, column.name))
            .collect();
        let lane_names: HashMap<String, String> = self
            .list_swimlanes(board_id)?
            .into_iter()
            .map(|lane| (lane.id, lane.name))
            .collect();
        let color_values: HashMap<String, String> = self
            .list_colors(board_id)?
            .into_iter()
            .map(|color| (color.id, color.value))
            .collect();
        let tasks: Vec<serde_json::Value> = self
            .board_tasks(board_id)?
            .iter()
            .map(|task| {
                serde_json::json!({
                    "name": task.name,
                    "description": task.description,
                    "size": task.size,
                    "color_value": task
                        .color_id
                        .as_deref()
                        .and_then(|id| color_values.get(id)),
                    "column": column_names.get(&task.column_id),
                    "swimlane": task
                        .swimlane_id
                        .as_deref()
                        .and_then(|id| lane_names.get(id)),
                    "position": task.position,
                    "due_at": task.due_at,
                    "due_repeat": task.due_repeat,
                    "estimate_hours": task.estimate_hours,
                    "subtasks": task.subtasks,
                    "labels": task.labels,
                })
            })
            .collect();
        let snapshot = serde_json::json!({
            "colors": colors,
            "columns": columns,
            "swimlanes": swimlanes,
            "tasks": tasks,
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
    /// columns, swimlanes, and captured tasks. Returns the new board id, or
    /// None when the template is unknown.
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
        self.apply_template_tasks(&board_id, snapshot)?;
        Ok(Some(board_id))
    }

    /// Restore tasks captured in a template snapshot onto a freshly built
    /// board. Columns, swimlanes, and colors are resolved by name/value
    /// (ids differ per board); unmapped values fall back gracefully.
    /// Timer stats and completion state are not part of a template — new
    /// tasks start fresh.
    fn apply_template_tasks(&self, board_id: &str, snapshot: &serde_json::Value) -> DbResult<()> {
        let tasks = match snapshot.get("tasks").and_then(|value| value.as_array()) {
            Some(tasks) if !tasks.is_empty() => tasks,
            _ => return Ok(()),
        };
        let column_ids: HashMap<String, String> = self
            .list_columns(board_id)?
            .into_iter()
            .map(|column| (column.name, column.id))
            .collect();
        let fallback_column: Option<String> = self
            .list_columns(board_id)?
            .first()
            .map(|column| column.id.clone());
        let lane_ids: HashMap<String, String> = self
            .list_swimlanes(board_id)?
            .into_iter()
            .map(|lane| (lane.name, lane.id))
            .collect();
        let color_ids: HashMap<String, String> = self
            .list_colors(board_id)?
            .into_iter()
            .map(|color| (color.value, color.id))
            .collect();
        for task in tasks {
            let column_id = task
                .get("column")
                .and_then(|value| value.as_str())
                .and_then(|name| column_ids.get(name).cloned())
                .or_else(|| fallback_column.clone());
            let column_id = match column_id {
                Some(id) => id,
                None => continue,
            };
            let swimlane_id = task
                .get("swimlane")
                .and_then(|value| value.as_str())
                .and_then(|name| lane_ids.get(name).cloned());
            let color_id = task
                .get("color_value")
                .and_then(|value| value.as_str())
                .and_then(|value| color_ids.get(value).cloned());
            let id = Uuid::new_v4().to_string();
            let now = Utc::now().to_rfc3339();
            let row = TaskRow {
                id: id.clone(),
                column_id: column_id.clone(),
                swimlane_id,
                name: task
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("Task")
                    .to_string(),
                description: task
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
                size: task
                    .get("size")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(1),
                position: task
                    .get("position")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0),
                created_at: now.clone(),
                completed_at: None,
                total_minutes: 0,
                pomodori_completed: 0,
                interruptions: 0,
                color_id,
                subtasks: task
                    .get("subtasks")
                    .and_then(|value| serde_json::from_value::<Vec<Subtask>>(value.clone()).ok())
                    .unwrap_or_default(),
                member_ids: Vec::new(),
                grouping_date: None,
                watched: false,
                labels: task
                    .get("labels")
                    .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
                    .unwrap_or_default(),
                due_at: task
                    .get("due_at")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                due_repeat: task
                    .get("due_repeat")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                due_done: false,
                estimate_hours: task.get("estimate_hours").and_then(|v| v.as_f64()),
                column_added_at: Some(now),
                comments: Vec::new(),
                attachments: Vec::new(),
                history: Vec::new(),
            };
            let txn = self.db.begin_write()?;
            write_one(&txn, TASKS, &id, &row)?;
            mmap_insert(&txn, TASKS_BY_COLUMN, &column_id, &id)?;
            txn.commit()?;
        }
        Ok(())
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
            subtasks: Vec::new(),
            member_ids: Vec::new(),
            grouping_date: None,
            watched: false,
            labels: Vec::new(),
            due_at: None,
            due_repeat: None,
            due_done: false,
            estimate_hours: None,
            column_added_at: Some(Utc::now().to_rfc3339()),
            comments: Vec::new(),
            attachments: Vec::new(),
            history: Vec::new(),
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

    /// All registered users, ordered by username. In the single-admin
    /// model this is the board-member roster (KanbanFlow parity).
    pub fn list_users(&self) -> DbResult<Vec<UserRow>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(USERS)?;
        let mut out: Vec<UserRow> = Vec::new();
        for item in tbl.iter()? {
            let (_, value) = item?;
            out.push(serde_json::from_slice(value.value())?);
        }
        out.sort_by(|a, b| a.username.cmp(&b.username));
        Ok(out)
    }

    /// Append a subtask to a task. Returns the new subtask, or None when
    /// the task does not exist.
    pub fn add_subtask(&self, task_id: &str, name: &str) -> DbResult<Option<Subtask>> {
        let sub = Subtask {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            done: false,
        };
        let added = sub.clone();
        let found = mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.subtasks.push(sub.clone());
        })?;
        Ok(found.then_some(added))
    }

    /// Update a subtask's name and/or done flag. Returns false when the
    /// task or subtask does not exist.
    pub fn update_subtask(
        &self,
        task_id: &str,
        sub_id: &str,
        name: Option<&str>,
        done: Option<bool>,
    ) -> DbResult<bool> {
        let mut touched = false;
        let found = mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            if let Some(sub) = task.subtasks.iter_mut().find(|s| s.id == sub_id) {
                if let Some(name) = name {
                    sub.name = name.to_string();
                }
                if let Some(done) = done {
                    sub.done = done;
                }
                touched = true;
            }
        })?;
        Ok(found && touched)
    }

    /// Remove a subtask. Returns false when the task or subtask does not exist.
    pub fn remove_subtask(&self, task_id: &str, sub_id: &str) -> DbResult<bool> {
        let mut removed = false;
        let found = mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            let before = task.subtasks.len();
            task.subtasks.retain(|s| s.id != sub_id);
            removed = task.subtasks.len() != before;
        })?;
        Ok(found && removed)
    }

    /// Reorder a task's subtasks to the given id sequence. Ids in `order`
    /// that exist take the given positions; any current subtasks not
    /// mentioned are appended in their original relative order so a partial
    /// list can never silently drop subtasks. Returns false when the task
    /// does not exist.
    pub fn reorder_subtasks(&self, task_id: &str, order: &[String]) -> DbResult<bool> {
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            let mut ordered: Vec<Subtask> = Vec::with_capacity(task.subtasks.len());
            for id in order {
                if let Some(pos) = task.subtasks.iter().position(|s| &s.id == id) {
                    ordered.push(task.subtasks[pos].clone());
                }
            }
            for sub in task.subtasks.iter() {
                if !ordered.iter().any(|s| s.id == sub.id) {
                    ordered.push(sub.clone());
                }
            }
            task.subtasks = ordered;
        })
    }

    /// Replace a task's assigned member user ids.
    pub fn set_task_members(&self, task_id: &str, member_ids: &[String]) -> DbResult<bool> {
        let ids: Vec<String> = member_ids.to_vec();
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.member_ids = ids.clone();
        })
    }

    /// Set (Some) or clear (None) a task's grouping-date override.
    pub fn set_grouping_date(&self, task_id: &str, date: Option<&str>) -> DbResult<bool> {
        let date = date.map(str::to_string);
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.grouping_date = date.clone();
        })
    }

    /// Set a task's watch flag (KanbanFlow parity: "Watch" in the task
    /// More menu). Returns false when the task does not exist.
    pub fn set_task_watched(&self, task_id: &str, watched: bool) -> DbResult<bool> {
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.watched = watched;
        })
    }

    /// Replace a task's labels (KanbanFlow parity). Labels are trimmed,
    /// empties dropped, duplicates removed, order kept. Returns false when
    /// the task does not exist.
    pub fn set_task_labels(&self, task_id: &str, labels: &[String]) -> DbResult<bool> {
        let mut clean: Vec<String> = Vec::new();
        for label in labels {
            let label = label.trim().to_string();
            if !label.is_empty() && !clean.iter().any(|l| l == &label) {
                clean.push(label);
            }
        }
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.labels = clean.clone();
        })
    }

    /// Set (Some) or clear (None) a task's due date (RFC3339) and repeat
    /// text (KanbanFlow parity: "Add due date"). Returns false when the
    /// task does not exist.
    pub fn set_task_due(
        &self,
        task_id: &str,
        due_at: Option<&str>,
        repeat: Option<&str>,
    ) -> DbResult<bool> {
        let due_at = due_at.map(str::to_string);
        let repeat = repeat
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.due_at = due_at.clone();
            task.due_repeat = repeat.clone();
            // Clearing the due date also clears its done flag — there is
            // no date left to be done (KF-224 parenthetical rule).
            if due_at.is_none() {
                task.due_done = false;
            }
        })
    }

    /// Mark a task's due date done/undone (KanbanFlow parity: checking the
    /// due-date item; the card renders "(Done)"). No-op for tasks without
    /// a due date.
    pub fn set_task_due_done(&self, task_id: &str, done: bool) -> DbResult<bool> {
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            if task.due_at.is_some() {
                task.due_done = done;
            }
        })
    }

    /// Set (or clear, with None) a task's hour-based time estimate
    /// (KanbanFlow parity, KF-216). Hours must already be validated by the
    /// caller (`normalize_estimate_hours`); values are rounded to two
    /// decimals to avoid float noise.
    pub fn set_task_estimate(&self, task_id: &str, hours: Option<f64>) -> DbResult<bool> {
        let hours = hours.map(|h| (h * 100.0).round() / 100.0);
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.estimate_hours = hours;
        })
    }

    /// Append a comment to a task. Returns the new comment, or None when
    /// the task does not exist.
    pub fn add_comment(
        &self,
        task_id: &str,
        author: &str,
        body: &str,
    ) -> DbResult<Option<TaskComment>> {
        let comment = TaskComment {
            id: Uuid::new_v4().to_string(),
            author: author.to_string(),
            body: body.to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        let added = comment.clone();
        let found = mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.comments.push(comment.clone());
        })?;
        Ok(found.then_some(added))
    }

    /// Delete a comment from a task. Returns false when the task or the
    /// comment does not exist.
    pub fn delete_comment(&self, task_id: &str, comment_id: &str) -> DbResult<bool> {
        let mut removed = false;
        let found = mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            let before = task.comments.len();
            task.comments.retain(|c| c.id != comment_id);
            removed = task.comments.len() != before;
        })?;
        Ok(found && removed)
    }

    /// Maximum stored attachment size: 10 MiB.
    pub const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

    /// Attach a file's bytes to a task. The metadata is appended to the
    /// task row and the bytes go into the `attachment_data` table, both in
    /// one write transaction. Returns None when the task does not exist.
    pub fn add_attachment(
        &self,
        task_id: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
        uploaded_by: &str,
    ) -> DbResult<Option<TaskAttachment>> {
        let meta = TaskAttachment {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            mime: mime.to_string(),
            size: bytes.len() as u64,
            uploaded_by: uploaded_by.to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        let added = meta.clone();
        let txn = self.db.begin_write()?;
        let found = {
            let mut tbl = txn.open_table(TASKS)?;
            let current: Option<TaskRow> = tbl
                .get(task_id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            match current {
                Some(mut task) => {
                    task.attachments.push(meta);
                    let raw = serde_json::to_vec(&task)?;
                    tbl.insert(task_id, raw.as_slice())?;
                    let mut data = txn.open_table(ATTACHMENT_DATA)?;
                    data.insert(added.id.as_str(), bytes)?;
                    true
                }
                None => false,
            }
        };
        txn.commit()?;
        Ok(found.then_some(added))
    }

    /// Read an attachment's bytes. Returns None when unknown.
    pub fn read_attachment_data(&self, attachment_id: &str) -> DbResult<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(ATTACHMENT_DATA)?;
        Ok(tbl.get(attachment_id)?.map(|v| v.value().to_vec()))
    }

    /// Delete an attachment: removes the metadata from the task row and
    /// the bytes from the data table. Returns false when the task or the
    /// attachment does not exist.
    pub fn delete_attachment(&self, task_id: &str, attachment_id: &str) -> DbResult<bool> {
        let mut removed = false;
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            let current: Option<TaskRow> = tbl
                .get(task_id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            if let Some(mut task) = current {
                let before = task.attachments.len();
                task.attachments.retain(|a| a.id != attachment_id);
                removed = task.attachments.len() != before;
                if removed {
                    let raw = serde_json::to_vec(&task)?;
                    tbl.insert(task_id, raw.as_slice())?;
                    let mut data = txn.open_table(ATTACHMENT_DATA)?;
                    data.remove(attachment_id)?;
                }
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Maximum history events kept per task; older ones are dropped from
    /// the front so the trail stays bounded (KanbanFlow parity: History).
    pub const MAX_HISTORY_EVENTS: usize = 500;

    /// Append an activity-trail event to a task (KanbanFlow parity:
    /// History report). No-op (Ok(false)) when the task does not exist.
    pub fn log_event(
        &self,
        task_id: &str,
        kind: &str,
        detail: &str,
        actor: &str,
    ) -> DbResult<bool> {
        let event = TaskEvent {
            id: Uuid::new_v4().to_string(),
            kind: kind.to_string(),
            detail: detail.to_string(),
            actor: actor.to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        mutate(&self.db, TASKS, task_id, |task: &mut TaskRow| {
            task.history.push(event.clone());
            let len = task.history.len();
            if len > Self::MAX_HISTORY_EVENTS {
                task.history.drain(..len - Self::MAX_HISTORY_EVENTS);
            }
        })
    }

    /// Distinct labels used anywhere on a board: task labels plus time
    /// entry labels, sorted. Powers the "Add labels..." suggestions
    /// (KanbanFlow parity).
    pub fn board_labels(&self, board_id: &str) -> DbResult<Vec<String>> {
        let tasks = self.board_tasks(board_id)?;
        let mut labels: Vec<String> = Vec::new();
        for task in &tasks {
            for label in &task.labels {
                if !labels.iter().any(|l| l == label) {
                    labels.push(label.clone());
                }
            }
        }
        for task in &tasks {
            for entry in self.list_entries(&task.id)? {
                for label in &entry.labels {
                    if !labels.iter().any(|l| l == label) {
                        labels.push(label.clone());
                    }
                }
            }
        }
        labels.sort();
        Ok(labels)
    }

    /// Delete a time entry. Returns false when the entry does not exist.
    pub fn delete_entry(&self, entry_id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let mut removed = false;
        {
            let mut tbl = txn.open_table(TIME_ENTRIES)?;
            let entry: Option<TimeEntryRow> = tbl
                .get(entry_id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            if let Some(entry) = entry {
                tbl.remove(entry_id)?;
                mmap_remove(&txn, ENTRIES_BY_TASK, &entry.task_id, entry_id)?;
                removed = true;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Move a task to a new column/swimlane/position. Crossing into a done
    /// column stamps `completed_at`; crossing out clears it. The moved task
    /// is inserted at the client-sent index with its siblings renumbered
    /// densely around it; the old cell is renumbered when it differs.
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

        // Explicit index insertion (KF-120): the target cell's other tasks
        // keep their relative order and the moved task lands exactly at the
        // client-sent index. Writing the raw position onto the row and then
        // renumbering with id tie-breaking let a mid-list drop collide with
        // the displaced task's position, persisting the card one slot off.
        let mut siblings: Vec<TaskRow> = Vec::new();
        for task_id in mmap_get(&self.db, TASKS_BY_COLUMN, column_id)? {
            if task_id == id {
                continue;
            }
            if let Some(sibling) = self.get_task(&task_id)? {
                if sibling.swimlane_id.as_deref() == new_swimlane_id.as_deref() {
                    siblings.push(sibling);
                }
            }
        }
        siblings.sort_by(|a, b| {
            a.position
                .total_cmp(&b.position)
                .then_with(|| a.id.cmp(&b.id))
        });
        let at = (position.round() as i64).clamp(0, siblings.len() as i64) as usize;
        let moved = TaskRow {
            column_id: column_id.to_string(),
            swimlane_id: new_swimlane_id.clone(),
            position: at as f64,
            completed_at,
            // KF-053: the "Added to column" date follows the task across
            // columns; same-column moves keep the existing stamp.
            column_added_at: if column_id != old_column_id {
                Some(Utc::now().to_rfc3339())
            } else {
                task.column_added_at.clone()
            },
            ..task
        };

        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            if column_id != old_column_id {
                mmap_remove(&txn, TASKS_BY_COLUMN, &old_column_id, id)?;
                mmap_insert(&txn, TASKS_BY_COLUMN, column_id, id)?;
            }
            let bytes = serde_json::to_vec(&moved)?;
            tbl.insert(id, bytes.as_slice())?;
            for (index, mut sibling) in siblings.into_iter().enumerate() {
                let dense = if index >= at { index + 1 } else { index };
                sibling.position = dense as f64;
                let bytes = serde_json::to_vec(&sibling)?;
                tbl.insert(sibling.id.as_str(), bytes.as_slice())?;
            }
        }
        txn.commit()?;

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
            .create_entry_full(
                Some(task_id),
                minutes,
                minutes * 60,
                note,
                "manual",
                false,
                None,
                &[],
                "",
            )?
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
        let mut settings = stored.unwrap_or_default();
        // KF-005: backfill KanbanFlow's verbatim interruption reasons for
        // databases whose settings predate them (empty list would render
        // an empty "Why did you stop?" menu).
        if settings.interrupt_reasons.is_empty() {
            settings.interrupt_reasons = Settings::default().interrupt_reasons;
        }
        Ok(settings)
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
    #[allow(clippy::too_many_arguments)]
    pub fn create_entry_full(
        &self,
        task_id: Option<&str>,
        minutes: i64,
        seconds: i64,
        note: &str,
        kind: &str,
        interrupted: bool,
        interrupt_reason: Option<&str>,
        labels: &[String],
        created_by: &str,
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
            seconds,
            note: note.to_string(),
            started_at: Utc::now().to_rfc3339(),
            kind: kind.to_string(),
            interrupted,
            interrupt_reason: interrupt_reason.map(str::to_string),
            labels: labels.to_vec(),
            created_by: created_by.to_string(),
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
        labels: &[String],
        created_by: &str,
    ) -> DbResult<Option<String>> {
        if self.get_task(task_id)?.is_none() {
            return Ok(None);
        }
        let id = Uuid::new_v4().to_string();
        let row = TimeEntryRow {
            id: id.clone(),
            task_id: task_id.to_string(),
            minutes,
            seconds: minutes * 60,
            note: note.to_string(),
            started_at: started_at.to_string(),
            kind: "manual".to_string(),
            interrupted: false,
            interrupt_reason: None,
            labels: labels.to_vec(),
            created_by: created_by.to_string(),
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TIME_ENTRIES, &id, &row)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        txn.commit()?;
        Ok(Some(id))
    }

    /// Update a time entry (date/time, task reassignment, note, labels).
    /// Returns false when the entry does not exist; the task must exist
    /// (checked by the caller).
    pub fn update_entry(
        &self,
        entry_id: &str,
        task_id: &str,
        minutes: i64,
        note: &str,
        started_at: &str,
        labels: &[String],
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
        row.labels = labels.to_vec();
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

#[cfg(test)]
mod legend_migration_tests {
    //! KF-222: the one-time `legend_visible` backfill must grant legacy
    //! boards the legend (missing key -> true), run exactly once, and never
    //! overwrite an explicit user choice. New boards keep the key absent so
    //! the footer stays off (KanbanFlow default).

    use super::*;

    fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        (dir, db)
    }

    /// Drop the one-time marker to simulate a pre-upgrade startup state.
    fn clear_backfill_marker(db: &Db) {
        let txn = db.db.begin_write().expect("write txn");
        txn.open_table(SETTINGS)
            .expect("settings table")
            .remove("legend_backfill_done")
            .expect("remove marker");
        txn.commit().expect("commit");
    }

    fn set_legend(db: &Db, board_id: &str, value: bool) {
        let mut updates = serde_json::Map::new();
        updates.insert("legend_visible".to_string(), serde_json::Value::Bool(value));
        db.set_board_config(board_id, &updates).expect("set config");
    }

    fn legend_of(db: &Db, board_id: &str) -> bool {
        db.get_board(board_id)
            .expect("get board")
            .expect("board exists")
            .config_bool("legend_visible")
    }

    #[test]
    fn backfill_grants_legacy_boards_true_exactly_once() {
        let (_dir, db) = test_db();
        // Fresh install: seeded starter board has no key -> legend OFF.
        let general = db.list_boards().expect("list boards")[0].id.clone();
        assert!(!legend_of(&db, &general));

        let legacy = db.create_board("Legacy").expect("create board");
        assert!(!legend_of(&db, &legacy));

        // Simulate the upgrade boot: marker absent, legacy board keyless.
        clear_backfill_marker(&db);
        db.migrate_legend_default().expect("backfill");
        assert!(legend_of(&db, &legacy), "legacy board keeps the legend");

        // Re-running must not touch anything (marker short-circuits).
        set_legend(&db, &legacy, false);
        db.migrate_legend_default().expect("second backfill");
        assert!(
            !legend_of(&db, &legacy),
            "explicit false survives re-run when marker is set"
        );

        // Explicit false also survives a fresh backfill pass (KF-222 req 4).
        clear_backfill_marker(&db);
        db.migrate_legend_default().expect("backfill again");
        assert!(
            !legend_of(&db, &legacy),
            "explicit user choice is never overwritten"
        );

        // Boards created after the toggle default OFF.
        let fresh = db.create_board("Fresh").expect("create board");
        assert!(!legend_of(&db, &fresh));
    }
}

#[cfg(test)]
mod pomodoro_plural_migration_tests {
    //! KF-296: the one-time pomodoro-count color label backfill must rename
    //! stored "2 Pomodori" / "3 Pomodori" / ">3 Pomodori" labels to
    //! KanbanFlow's "Pomodoros", run exactly once, and never touch
    //! user-customized labels ("1 Pomodoro" is unchanged by design).

    use super::*;

    fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        (dir, db)
    }

    /// Drop the one-time marker to simulate a pre-upgrade startup state.
    fn clear_backfill_marker(db: &Db) {
        let txn = db.db.begin_write().expect("write txn");
        txn.open_table(SETTINGS)
            .expect("settings table")
            .remove("pomodoro_plural_backfill_done")
            .expect("remove marker");
        txn.commit().expect("commit");
    }

    fn label_of(db: &Db, color_id: &str) -> String {
        db.get_color(color_id)
            .expect("get color")
            .expect("color exists")
            .label
    }

    #[test]
    fn backfill_renames_old_labels_exactly_once() {
        let (_dir, db) = test_db();
        let board = db.create_board("Legacy").expect("create board");
        let colors = db.list_colors(&board).expect("list colors");
        // Simulate a pre-Round-2 board: old Italian-plural labels, plus one
        // user-customized label that must survive the backfill.
        let ids: Vec<String> = colors.iter().take(5).map(|c| c.id.clone()).collect();
        db.update_color(&ids[0], Some("1 Pomodoro"), None, None, None)
            .expect("old singular");
        db.update_color(&ids[1], Some("2 Pomodori"), None, None, None)
            .expect("old two");
        db.update_color(&ids[2], Some("3 Pomodori"), None, None, None)
            .expect("old three");
        db.update_color(&ids[3], Some(">3 Pomodori"), None, None, None)
            .expect("old many");
        db.update_color(&ids[4], Some("Deep work"), None, None, None)
            .expect("custom");

        clear_backfill_marker(&db);
        db.migrate_pomodoro_plural_labels().expect("backfill");

        assert_eq!(label_of(&db, &ids[0]), "1 Pomodoro", "singular unchanged");
        assert_eq!(label_of(&db, &ids[1]), "2 Pomodoros");
        assert_eq!(label_of(&db, &ids[2]), "3 Pomodoros");
        assert_eq!(label_of(&db, &ids[3]), ">3 Pomodoros");
        assert_eq!(
            label_of(&db, &ids[4]),
            "Deep work",
            "user-customized label preserved"
        );

        // Re-running must be a no-op (marker short-circuits the pass).
        db.update_color(&ids[1], Some("2 Pomodori"), None, None, None)
            .expect("revert");
        db.migrate_pomodoro_plural_labels()
            .expect("second backfill");
        assert_eq!(
            label_of(&db, &ids[1]),
            "2 Pomodori",
            "marker prevents a second run"
        );
    }
}
