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
    Database, MultimapTableDefinition, ReadableDatabase, ReadableMultimapTable, ReadableTable,
    ReadableTableMetadata, TableDefinition, WriteTransaction,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use uuid::Uuid;

use crate::models::{
    standard_color, ActiveTimer, ActorKind, ApiTokenRecord, BoardRow, BoardTemplateRow, ColorRow,
    ColumnRow, DeviceAuthRow, DeviceAuthStatus, FieldDefRow, LinkKind, SessionRow, Settings,
    Subtask, SwimlaneRow, TaskAttachment, TaskClaim, TaskComment, TaskEvent, TaskLink, TaskRow,
    TimeEntryRow, UserRow,
};

pub type DbResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Where a new task lands in its column x swimlane cell (KF-403).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTaskPlacement {
    Top,
    Bottom,
}

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

/// Display name for a time entry logged without a task (KF-006).
pub const NO_TASK_NAME: &str = "No task";

/// Returns the calendar day ("YYYY-MM-DD") in `tz` on which the RFC3339
/// `started_at` falls (KF-412).
pub fn day_in<Tz: chrono::TimeZone>(started_at: &str, tz: &Tz) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(started_at)
        .ok()
        .map(|dt| {
            dt.with_timezone(tz)
                .date_naive()
                .format("%Y-%m-%d")
                .to_string()
        })
}

/// Returns the server-local day a time entry started on; timer days, the
/// TODAY list and the long-break cycle all follow the server's zone.
pub fn local_day(started_at: &str) -> Option<String> {
    day_in(started_at, &chrono::Local)
}

/// Typed outcome for [`Db::add_task_link`].
#[derive(Debug, Clone)]
pub enum LinkAddOutcome {
    /// The new link, or the identical one that already existed, as seen
    /// from the requesting task.
    Added(TaskLink),
    NotFound,
    /// Refused: the `blocked_by` chain would loop back to the task.
    Cycle,
}

/// Typed outcome for a write guarded by an `If-Claim-Fence` value.
#[derive(Debug, Clone)]
pub enum FencedWrite<T> {
    Written(T),
    NotFound,
    /// Refused: a live claim holds a different fence; nothing written.
    StaleFence,
}

/// Fields of a claim request; see [`Db::claim_task`].
#[derive(Debug, Clone, Default)]
pub struct ClaimRequest {
    pub claimant: String,
    pub ttl_seconds: i64,
    pub role: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub jj_workspace: Option<String>,
    pub jj_change_id: Option<String>,
    pub session_id: Option<String>,
    pub fence: Option<u64>,
}

/// Typed outcome for the claim operations on [`Db`].
#[derive(Debug, Clone)]
pub enum ClaimOutcome {
    /// The claim as stored after the call.
    Granted(TaskClaim),
    /// Released, or already released by that fence.
    Released,
    NotFound,
    /// Refused: another live claim holds the task.
    Held(TaskClaim),
    /// Refused: the fence does not match a live claim.
    FenceMismatch,
}

/// The caller-supplied parts of a new comment; see [`Db::add_comment`].
#[derive(Debug, Clone, Copy)]
pub struct NewComment<'a> {
    pub author: &'a str,
    pub body: &'a str,
    pub actor_kind: ActorKind,
    pub token_name: Option<&'a str>,
    pub kind: Option<&'a str>,
}

/// Checks a move makes inside its write transaction.
#[derive(Debug, Clone, Copy, Default)]
pub struct MoveGuards<'a> {
    /// `(detail, actor)` of a `moved` history event to append.
    pub event: Option<(&'a str, &'a str)>,
    /// An `If-Claim-Fence` value; a live claim holding another fence
    /// refuses the move.
    pub fence: Option<u64>,
    /// Refuse a move into another column that already holds its
    /// `wip_limit` of tasks.
    pub enforce_wip: bool,
}

/// Typed outcome for [`Db::move_task_logged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved,
    NotFound,
    StaleFence,
    /// Refused: the target column holds `count` tasks against `limit`.
    WipLimit {
        count: usize,
        limit: i64,
    },
}

/// Typed outcome for [`Db::create_device_auth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAuthCreateOutcome {
    Created,
    /// Refused: too many requests are pending already.
    TooManyPending,
    /// Refused: a live request already uses this user code.
    UserCodeTaken,
}

/// Typed outcome for [`Db::poll_device_auth`].
#[derive(Debug, Clone)]
pub enum DevicePoll {
    /// Approved: this token record was stored and the code is spent.
    Issued(Box<ApiTokenRecord>),
    Pending,
    Denied,
    Expired,
    /// Polled sooner than the interval allows.
    SlowDown,
    /// Unknown, or already exchanged for a token.
    Invalid,
}

/// Typed outcome for [`Db::create_field`].
#[derive(Debug, Clone)]
pub enum FieldCreateOutcome {
    Created(FieldDefRow),
    /// Refused: the board already has a field with this key.
    KeyExists,
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
/// Schema stamp table; never rename it, or older binaries read the file as unstamped.
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Key of the stamp in [`META`]; never rename it, or older binaries read the file as unstamped.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Schema version this binary reads and writes. Bump it whenever a stored
/// struct or table gains a field or table, so older binaries refuse the
/// newer file instead of dropping the new data on their next save.
///
/// Version 2 adds, over version 1 (all additive, read from older rows
/// through serde defaults):
///
/// 1. `TaskRow`: `updated_at`, `metadata`, `links` (`TaskLink`, `LinkKind`),
///    `claim` (`TaskClaim`), `claim_fence`, `fields`.
/// 2. `TaskComment`: `actor_kind` (`ActorKind`), `token_name`, `kind`.
/// 3. Table `field_defs` holding `FieldDefRow` (`FieldKind`, `FieldOwner`),
///    and multimap `fields_by_board`.
/// 4. Multimap `task_links_by_target`.
/// 5. Table `device_auths` holding `DeviceAuthRow` (`DeviceAuthStatus`).
/// 6. `TaskRow`: `due_target_column_id` (KF-259); a board's opaque
///    `config_json` gains `due_time` (KF-404).
/// 7. `TaskEvent`: `column_id`, and `ColumnRow`: `former_names` (KF-350);
///    a column's opaque `config_json` stores `column_sum` as "count" or
///    "hours" and `sorting` as "due", "color" or "added" too (KF-371).
///
/// Until version 2 ships, a stream that adds stored data to the same
/// release appends its item to this list and replaces the last
/// [`SCHEMA_FINGERPRINTS`] entry instead of bumping again.
pub const SCHEMA_VERSION: u64 = 2;

/// Stored-schema fingerprint per version, oldest first; append one with each
/// bump and never edit a released entry (`stored_schema_matches_fingerprint`).
/// The version-2 entry stays editable until version 2 ships (see
/// [`SCHEMA_VERSION`]).
#[cfg(test)]
const SCHEMA_FINGERPRINTS: [&str; SCHEMA_VERSION as usize] = [
    "d9998454dc068986eed12c06c12d61ae715bac8ff7b5af745cc4ca4219b98ab7",
    "060770c2715c3136209d202e87d67a350eaf9287bdf41a8b76feeb17ffe994f1",
];

/// Refusal to open a file stamped by a newer binary.
#[derive(Debug)]
pub struct SchemaTooNew {
    pub path: String,
    pub found: u64,
    pub supported: u64,
    /// The file was left by a crash, so redb ran its crash repair on it.
    pub repaired: bool,
}

impl std::fmt::Display for SchemaTooNew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "database {} has schema version {}, but this chipflow binary supports up to \
             schema version {}; refusing to open it so its newer data is not dropped. \
             Restore a backup taken before the upgrade, or upgrade chipflow.",
            self.path, self.found, self.supported
        )?;
        if self.repaired {
            write!(
                f,
                " The file was not closed cleanly, so redb repaired its allocator state \
                 while opening it; no application data was written."
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for SchemaTooNew {}

/// Read the stamp; `None` for a file that predates the `meta` table.
fn read_schema_version(db: &impl ReadableDatabase) -> DbResult<Option<u64>> {
    let txn = db.begin_read()?;
    let tbl = match txn.open_table(META) {
        Ok(tbl) => tbl,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(tbl.get(SCHEMA_VERSION_KEY)?.map(|guard| guard.value()))
}

/// Fail with [`SchemaTooNew`] when `found` is newer than this binary.
fn refuse_newer(path: &str, found: Option<u64>, repaired: bool) -> DbResult<()> {
    match found {
        Some(found) if found > SCHEMA_VERSION => Err(Box::new(SchemaTooNew {
            path: path.to_string(),
            found,
            supported: SCHEMA_VERSION,
            repaired,
        })),
        _ => Ok(()),
    }
}

/// Refuse a cleanly closed newer file without opening it writable, so the
/// refusal leaves it byte-identical. Returns true when the file needs
/// redb's crash repair, which only a writable open performs.
fn preflight_schema_version(path: &str) -> DbResult<bool> {
    if !Path::new(path).exists() {
        return Ok(false);
    }
    match redb::ReadOnlyDatabase::open(path) {
        Ok(db) => refuse_newer(path, read_schema_version(&db)?, false).map(|()| false),
        Err(redb::DatabaseError::RepairAborted) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Bring a file stamped `from` (`None`: unstamped) up to [`SCHEMA_VERSION`].
/// The schema is additive with serde defaults, so no row needs rewriting;
/// a future non-additive change transforms rows here.
fn upgrade_schema(_txn: &WriteTransaction, _from: Option<u64>) -> DbResult<()> {
    Ok(())
}

/// Refuse a newer file before any write transaction, stamp an older or
/// unstamped one, and leave an equal one untouched.
fn ensure_schema_version(db: &Database, path: &str, repaired: bool) -> DbResult<()> {
    let found = read_schema_version(db)?;
    refuse_newer(path, found, repaired)?;
    if found == Some(SCHEMA_VERSION) {
        println!("chipflow: database {path} is at schema version {SCHEMA_VERSION}");
        return Ok(());
    }
    let txn = db.begin_write()?;
    upgrade_schema(&txn, found)?;
    txn.open_table(META)?
        .insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
    txn.commit()?;
    match found {
        Some(old) => println!(
            "chipflow: upgraded database {path} from schema version {old} to {SCHEMA_VERSION}"
        ),
        None => println!("chipflow: stamped database {path} with schema version {SCHEMA_VERSION}"),
    }
    Ok(())
}

/// device-code hash -> [`DeviceAuthRow`].
const DEVICE_AUTHS: TableDefinition<&str, &[u8]> = TableDefinition::new("device_auths");
/// field id -> [`FieldDefRow`].
const FIELD_DEFS: TableDefinition<&str, &[u8]> = TableDefinition::new("field_defs");

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
/// board id -> field id for [`FieldDefRow`] rows.
const FIELDS_BY_BOARD: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("fields_by_board");
/// task id -> id of each task holding a [`TaskLink`] that targets it.
const LINKS_BY_TARGET: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("task_links_by_target");

/// Thin wrapper around the redb database handle.
#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
}

/// Shared application state (database handle), used by the HTTP layer.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// Wrong device-pairing codes typed on this server instance.
    pub device_lockout: DeviceCodeLockout,
}

impl AppState {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            device_lockout: DeviceCodeLockout::default(),
        }
    }
}

/// Recent wrong device-pairing user codes, kept in memory per server
/// instance; past the cap, code entry and approval refuse until the window
/// moves on.
#[derive(Clone, Default)]
pub struct DeviceCodeLockout {
    failures: Arc<std::sync::Mutex<Vec<i64>>>,
}

impl DeviceCodeLockout {
    /// Seconds a wrong code counts against the cap.
    pub const WINDOW_SECONDS: i64 = 600;
    /// Wrong codes allowed within the window.
    pub const MAX_FAILURES: usize = 10;

    /// True while the window holds the maximum number of wrong codes.
    pub fn locked(&self, now: i64) -> bool {
        self.recent(now).len() >= Self::MAX_FAILURES
    }

    pub fn record_failure(&self, now: i64) {
        self.recent(now).push(now);
    }

    fn recent(&self, now: i64) -> std::sync::MutexGuard<'_, Vec<i64>> {
        let mut failures = self
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        failures.retain(|at| now - at < Self::WINDOW_SECONDS);
        failures
    }
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

/// Load a task, apply `f`, stamp `updated_at`, and store it back. Returns
/// false when the id is unknown.
fn mutate_task<F>(db: &Database, key: &str, f: F) -> DbResult<bool>
where
    F: FnOnce(&mut TaskRow),
{
    mutate(db, TASKS, key, |task: &mut TaskRow| {
        f(task);
        task.updated_at = Some(Utc::now().to_rfc3339());
    })
}

/// Read one task row through an open table.
fn task_in<T>(tbl: &T, id: &str) -> DbResult<Option<TaskRow>>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    Ok(tbl
        .get(id)?
        .map(|guard| serde_json::from_slice(guard.value()))
        .transpose()?)
}

/// The board a column belongs to, read inside a write transaction.
fn column_board_in(txn: &WriteTransaction, column_id: &str) -> DbResult<Option<String>> {
    let columns = txn.open_table(COLUMNS)?;
    let column: Option<ColumnRow> = columns
        .get(column_id)?
        .map(|guard| serde_json::from_slice(guard.value()))
        .transpose()?;
    Ok(column.map(|column| column.board_id))
}

/// A board's field definitions by position, read inside a write
/// transaction.
fn board_fields_in(txn: &WriteTransaction, board_id: &str) -> DbResult<Vec<FieldDefRow>> {
    let ids = txn_mmap_get(txn, FIELDS_BY_BOARD, board_id)?;
    let defs = txn.open_table(FIELD_DEFS)?;
    let mut fields = Vec::new();
    for id in ids {
        if let Some(guard) = defs.get(id.as_str())? {
            fields.push(serde_json::from_slice::<FieldDefRow>(guard.value())?);
        }
    }
    fields.sort_by(|a, b| {
        a.position
            .cmp(&b.position)
            .then_with(|| a.created_at.cmp(&b.created_at))
    });
    Ok(fields)
}

/// What a task change reads besides the row, from the same write
/// transaction: the task's board, its field definitions, and the
/// configured pomodoro length.
#[derive(Debug, Clone, Default)]
pub struct TaskContext {
    pub board_id: Option<String>,
    pub fields: Vec<FieldDefRow>,
    pub pomodoro_minutes: u32,
}

fn task_context_in(txn: &WriteTransaction, task: &TaskRow) -> DbResult<TaskContext> {
    let board_id = column_board_in(txn, &task.column_id)?;
    let fields = match &board_id {
        Some(board_id) => board_fields_in(txn, board_id)?,
        None => Vec::new(),
    };
    let settings: Option<Settings> = txn
        .open_table(SETTINGS)?
        .get("app")?
        .map(|guard| serde_json::from_slice(guard.value()))
        .transpose()?;
    Ok(TaskContext {
        board_id,
        fields,
        pomodoro_minutes: settings.unwrap_or_default().pomodoro_minutes,
    })
}

/// How a task change ends.
enum RowWrite {
    /// Nothing changed; nothing is written.
    Skip,
    /// Store the row with `updated_at` stamped.
    Stamp,
    /// Store the row as is: a heartbeat changes only the lease.
    Keep,
}

/// Remove every link to a task in `deleted` from the tasks that hold one,
/// and drop the deleted tasks' own index entries. Runs before the rows go.
fn unlink_deleted_tasks(txn: &WriteTransaction, deleted: &[String]) -> DbResult<()> {
    let mut tasks = txn.open_table(TASKS)?;
    let mut by_target = txn.open_multimap_table(LINKS_BY_TARGET)?;
    for id in deleted {
        let mut deleted_name = None;
        if let Some(task) = task_in(&tasks, id)? {
            for link in &task.links {
                by_target.remove(link.target_id.as_str(), id.as_str())?;
            }
            deleted_name = Some(task.name);
        }
        let mut holders = Vec::new();
        for guard in by_target.get(id.as_str())? {
            holders.push(guard?.value().to_owned());
        }
        for holder_id in holders.iter().filter(|h| !deleted.contains(h)) {
            if let Some(mut holder) = task_in(&tasks, holder_id)? {
                let (gone, kept) = std::mem::take(&mut holder.links)
                    .into_iter()
                    .partition(|link| link.target_id == *id);
                holder.links = kept;
                for link in gone {
                    let detail = format!(
                        "Removed link {} (task deleted)",
                        link_event_detail(&link, deleted_name.as_deref())
                    );
                    push_event(&mut holder, "link_removed", detail, "system");
                }
                holder.updated_at = Some(Utc::now().to_rfc3339());
                let raw = serde_json::to_vec(&holder)?;
                tasks.insert(holder_id.as_str(), raw.as_slice())?;
            }
        }
        by_target.remove_all(id.as_str())?;
    }
    Ok(())
}

/// Tasks holding a `blocked_by` link to any of `targets`, read inside a
/// write transaction.
fn blocked_dependents(txn: &WriteTransaction, targets: &[String]) -> DbResult<Vec<String>> {
    let mut holder_ids: Vec<String> = Vec::new();
    for target in targets {
        for holder_id in txn_mmap_get(txn, LINKS_BY_TARGET, target)? {
            if !holder_ids.contains(&holder_id) {
                holder_ids.push(holder_id);
            }
        }
    }
    let tasks = txn.open_table(TASKS)?;
    let mut dependents = Vec::new();
    for holder_id in holder_ids {
        let Some(holder) = task_in(&tasks, &holder_id)? else {
            continue;
        };
        let blocked_by_target = holder
            .links
            .iter()
            .any(|link| link.kind == LinkKind::BlockedBy && targets.contains(&link.target_id));
        if blocked_by_target {
            dependents.push(holder_id);
        }
    }
    Ok(dependents)
}

/// Each task's `blocked` value, read inside a write transaction.
fn blocked_states(txn: &WriteTransaction, ids: &[String]) -> DbResult<HashMap<String, bool>> {
    let tasks = txn.open_table(TASKS)?;
    let columns = txn.open_table(COLUMNS)?;
    let mut states = HashMap::new();
    for id in ids {
        let Some(task) = task_in(&tasks, id)? else {
            continue;
        };
        let mut blocked = false;
        for link in task.links.iter().filter(|l| l.kind == LinkKind::BlockedBy) {
            let Some(target) = task_in(&tasks, &link.target_id)? else {
                continue;
            };
            let column: Option<ColumnRow> = columns
                .get(target.column_id.as_str())?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            if column.is_some_and(|column| !column.is_done) {
                blocked = true;
                break;
            }
        }
        states.insert(id.clone(), blocked);
    }
    Ok(states)
}

/// Stamp `updated_at` on each task whose `blocked` value differs from
/// `before`, inside the same write transaction as the change.
fn bump_flipped(txn: &WriteTransaction, before: &HashMap<String, bool>) -> DbResult<()> {
    let ids: Vec<String> = before.keys().cloned().collect();
    let after = blocked_states(txn, &ids)?;
    let mut tasks = txn.open_table(TASKS)?;
    for (id, was_blocked) in before {
        if after.get(id) == Some(was_blocked) {
            continue;
        }
        if let Some(mut task) = task_in(&tasks, id)? {
            task.updated_at = Some(Utc::now().to_rfc3339());
            let raw = serde_json::to_vec(&task)?;
            tasks.insert(id.as_str(), raw.as_slice())?;
        }
    }
    Ok(())
}

/// True when following `blocked_by` links from `from` reaches `to`.
fn blocked_by_reaches<T>(tbl: &T, from: &str, to: &str) -> DbResult<bool>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    let mut pending = vec![from.to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = pending.pop() {
        if id == to {
            return Ok(true);
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(task) = task_in(tbl, &id)? {
            pending.extend(
                task.links
                    .iter()
                    .filter(|link| link.kind == LinkKind::BlockedBy)
                    .map(|link| link.target_id.clone()),
            );
        }
    }
    Ok(false)
}

/// Append a history event, dropping the oldest past the cap.
pub fn push_event(task: &mut TaskRow, kind: &str, detail: String, actor: &str) {
    push_column_event(task, kind, detail, actor, None);
}

/// [`push_event`] for an event that puts the task in `column_id`, so
/// reports follow the column through renames (KF-350).
pub fn push_column_event(
    task: &mut TaskRow,
    kind: &str,
    detail: String,
    actor: &str,
    column_id: Option<&str>,
) {
    task.history.push(TaskEvent {
        id: Uuid::new_v4().to_string(),
        kind: kind.to_string(),
        detail,
        actor: actor.to_string(),
        created_at: Utc::now().to_rfc3339(),
        column_id: column_id.map(str::to_string),
    });
    let len = task.history.len();
    if len > Db::MAX_HISTORY_EVENTS {
        task.history.drain(..len - Db::MAX_HISTORY_EVENTS);
    }
}

/// Labels trimmed, with empties and duplicates dropped, order kept.
pub fn clean_labels(labels: &[String]) -> Vec<String> {
    let mut clean: Vec<String> = Vec::new();
    for label in labels {
        let label = label.trim().to_string();
        if !label.is_empty() && !clean.contains(&label) {
            clean.push(label);
        }
    }
    clean
}

/// An hour estimate rounded to two decimals to avoid float noise.
pub fn round_estimate(hours: f64) -> f64 {
    (hours * 100.0).round() / 100.0
}

/// Merge `patch` into a task's metadata by top-level key: each key
/// replaces the stored one, a null value deletes it, and a None `patch`
/// clears everything; an empty result is stored as None. Returns false,
/// leaving the task untouched, when the merged object would exceed
/// [`Db::MAX_METADATA_BYTES`].
pub fn merge_metadata(
    task: &mut TaskRow,
    patch: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    let merged = patch.and_then(|patch| {
        let mut merged = task.metadata.clone().unwrap_or_default();
        for (key, value) in patch {
            if value.is_null() {
                merged.remove(key);
            } else {
                merged.insert(key.clone(), value.clone());
            }
        }
        Some(merged).filter(|merged| !merged.is_empty())
    });
    let size = merged.as_ref().map_or(0, |merged| {
        serde_json::to_vec(merged).map_or(usize::MAX, |raw| raw.len())
    });
    if size > Db::MAX_METADATA_BYTES {
        return false;
    }
    task.metadata = merged;
    true
}

/// Keep each field value whose key `defs` defines and whose value is valid
/// there, normalized; remove and return the rest.
fn carry_fields(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    defs: &[FieldDefRow],
) -> serde_json::Map<String, serde_json::Value> {
    let mut dropped = serde_json::Map::new();
    for (key, value) in std::mem::take(fields) {
        let carried = defs
            .iter()
            .find(|def| def.key == key)
            .and_then(|def| def.check_value(&value).ok());
        match carried {
            Some(value) => {
                fields.insert(key, value);
            }
            None => {
                dropped.insert(key, value);
            }
        }
    }
    dropped
}

/// History detail for a link event, e.g.
/// `blocked_by "Write docs" (<target id>)`.
fn link_event_detail(link: &TaskLink, target_name: Option<&str>) -> String {
    let kind = match link.kind {
        LinkKind::BlockedBy => "blocked_by",
        LinkKind::Blocks => "blocks",
        LinkKind::Related => "related",
    };
    match target_name {
        Some(name) => format!("{kind} \"{name}\" ({})", link.target_id),
        None => format!("{kind} {}", link.target_id),
    }
}

/// Clear a stored claim whose `expires_at` has passed, appending one
/// `claim_expired` event. Returns true when it did.
fn expire_claim(task: &mut TaskRow, now: chrono::DateTime<Utc>) -> bool {
    match &task.claim {
        Some(claim) if !claim.is_live(now) => {
            let detail = format!(
                "Claim by {} (fence {}) expired at {}",
                claim.claimant, claim.fence, claim.expires_at
            );
            task.claim = None;
            push_event(task, "claim_expired", detail, "system");
            true
        }
        _ => false,
    }
}

fn lease_end(now: chrono::DateTime<Utc>, ttl_seconds: i64) -> String {
    (now + chrono::Duration::seconds(ttl_seconds)).to_rfc3339()
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

// KF-421: read-modify-write paths read through their own write transaction,
// so no write committed by another caller in between is overwritten.

/// Read one JSON row by id inside an open write transaction.
fn txn_read_one<T: DeserializeOwned>(
    txn: &WriteTransaction,
    table: TableDefinition<&str, &[u8]>,
    key: &str,
) -> DbResult<Option<T>> {
    let tbl = txn.open_table(table)?;
    let row = tbl
        .get(key)?
        .map(|guard| serde_json::from_slice(guard.value()))
        .transpose()?;
    Ok(row)
}

/// Values stored under `key` in a multimap, read inside an open write
/// transaction.
fn txn_mmap_get(
    txn: &WriteTransaction,
    table: MultimapTableDefinition<&str, &str>,
    key: &str,
) -> DbResult<Vec<String>> {
    let tbl = txn.open_multimap_table(table)?;
    let mut out = Vec::new();
    for guard in tbl.get(key)? {
        out.push(guard?.value().to_owned());
    }
    Ok(out)
}

/// Rows that `index[key]` points at in `table`, read inside an open write
/// transaction. Dangling index entries are skipped.
fn txn_indexed_rows<T: DeserializeOwned>(
    txn: &WriteTransaction,
    index: MultimapTableDefinition<&str, &str>,
    key: &str,
    table: TableDefinition<&str, &[u8]>,
) -> DbResult<Vec<T>> {
    let mut rows = Vec::new();
    for id in txn_mmap_get(txn, index, key)? {
        if let Some(row) = txn_read_one(txn, table, &id)? {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Drop the active timer when it runs on a task `is_removed` says is being
/// deleted, inside that deletion's write transaction.
fn txn_clear_timer_if(txn: &WriteTransaction, is_removed: impl Fn(&str) -> bool) -> DbResult<()> {
    let timer: Option<ActiveTimer> = txn_read_one(txn, ACTIVE_TIMER, "timer")?;
    if timer
        .and_then(|timer| timer.task_id)
        .is_some_and(|id| is_removed(&id))
    {
        txn.open_table(ACTIVE_TIMER)?.remove("timer")?;
    }
    Ok(())
}

/// One column x swimlane cell's tasks in position order (id breaks ties),
/// read inside an open write transaction.
fn txn_cell_tasks(
    txn: &WriteTransaction,
    column_id: &str,
    swimlane_id: Option<&str>,
) -> DbResult<Vec<TaskRow>> {
    let mut tasks: Vec<TaskRow> = txn_indexed_rows(txn, TASKS_BY_COLUMN, column_id, TASKS)?;
    tasks.retain(|task| task.swimlane_id.as_deref() == swimlane_id);
    tasks.sort_by(|a, b| {
        a.position
            .total_cmp(&b.position)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(tasks)
}

/// Rewrite one cell's positions as dense 0..n inside an open write
/// transaction. Tasks whose position changes get `updated_at` stamped.
fn txn_renumber_cell(
    txn: &WriteTransaction,
    column_id: &str,
    swimlane_id: Option<&str>,
) -> DbResult<()> {
    let tasks = txn_cell_tasks(txn, column_id, swimlane_id)?;
    let mut tbl = txn.open_table(TASKS)?;
    for (index, mut task) in tasks.into_iter().enumerate() {
        if task.position == index as f64 {
            continue;
        }
        task.position = index as f64;
        task.updated_at = Some(Utc::now().to_rfc3339());
        let bytes = serde_json::to_vec(&task)?;
        tbl.insert(task.id.as_str(), bytes.as_slice())?;
    }
    Ok(())
}

// ---- Per-board color defaults (KanbanFlow parity) ----

/// Per-color defaults for a new board's standard palette:
/// (value, label, enabled, is_default, sort_order). Hex values are fixed
/// per standard color (see `STANDARD_COLORS` in models.rs).
fn standard_color_specs() -> Vec<(&'static str, &'static str, bool, bool, i64)> {
    vec![
        ("yellow", "Yellow", true, true, 1),
        ("green", "Green", true, false, 2),
        ("blue", "Blue", true, false, 3),
        ("red", "Red", true, false, 4),
        // KF-151: KanbanFlow enables its whole standard palette on a new
        // board.
        ("orange", "Orange", true, false, 5),
        ("purple", "Purple", true, false, 6),
        ("magenta", "Magenta", true, false, 7),
        ("cyan", "Cyan", true, false, 8),
        ("brown", "Brown", true, false, 9),
        ("white", "White", true, false, 10),
    ]
}

/// Per-color defaults for the Pomodoro board template, in the same tuple
/// shape as [`standard_color_specs`]. KF-372: the four pomodoro-count
/// colors are renamed and enabled, the other six disabled (KanbanFlow
/// reference: kf331-kanbanflow-settings-colors-recovered-20261016.png).
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
            {"name": "To-do", "wip_limit": null, "is_done": false, "position": 0},
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
                "column": "To-do",
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

/// Column fields one update may change; `None` leaves a field alone.
#[derive(Debug, Default)]
pub struct ColumnPatch<'a> {
    pub name: Option<&'a str>,
    /// `Some(None)` clears the WIP limit.
    pub wip_limit: Option<Option<i64>>,
    pub is_done: Option<bool>,
    pub description: Option<&'a str>,
    pub collapsed: Option<bool>,
    pub config_json: Option<&'a str>,
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
    /// Open (creating when needed) the database file at `path`, check its
    /// [`SCHEMA_VERSION`] stamp, create tables on first run, then seed the
    /// starter board. Fails with [`SchemaTooNew`] on a file from a newer
    /// binary. The admin account is NOT created here: the /setup page
    /// handles first-run account creation.
    pub fn connect(path: &str) -> DbResult<Self> {
        Self::open(path).map_err(|err| {
            if err.is::<SchemaTooNew>() {
                err
            } else {
                format!("database {path}: {err}").into()
            }
        })
    }

    fn open(path: &str) -> DbResult<Self> {
        // redb won't create missing parent directories itself.
        if let Some(parent) = Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let repaired = preflight_schema_version(path)?;
        let db = match Database::create(path) {
            Ok(db) => db,
            Err(err) if repaired => {
                return Err(format!(
                    "was not closed cleanly and needs redb's crash repair, which failed: {err}"
                )
                .into())
            }
            Err(err) => return Err(err.into()),
        };
        if repaired {
            println!("chipflow: database {path} was not closed cleanly; redb repaired it");
        }
        ensure_schema_version(&db, path, repaired)?;
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
            txn.open_multimap_table(LINKS_BY_TARGET)?;
            txn.open_table(FIELD_DEFS)?;
            txn.open_table(DEVICE_AUTHS)?;
            txn.open_multimap_table(FIELDS_BY_BOARD)?;
        }
        txn.commit()?;

        let this = Self { db: Arc::new(db) };
        // KF-222: backfill before seed() so existing boards keep their
        // current rendering (legend ON) while a fresh install's starter
        // board defaults to OFF (KanbanFlow parity).
        this.migrate_legend_default()?;
        this.restore_pomodori_labels()?;
        this.backfill_board_colors()?;
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

    /// Give every board without colors the standard palette, so boards from
    /// database files that predate colors need no backfill on a read.
    fn backfill_board_colors(&self) -> DbResult<()> {
        for board in self.list_boards()? {
            self.ensure_board_colors(&board.id)?;
        }
        Ok(())
    }

    /// KF-296: one-time backfill restoring KanbanFlow's "Pomodori" in stored
    /// pomodoro-count color labels. An earlier release renamed "2 Pomodori" to
    /// "2 Pomodoros" (and 3, >3); live KanbanFlow spells them "Pomodori".
    /// Only those exact strings are rewritten, so a user-customized label is
    /// never touched. The `pomodori_restore_done` marker makes the pass run
    /// exactly once. Like `migrate_legend_default`, this is a lazy backfill,
    /// not a schema migration.
    fn restore_pomodori_labels(&self) -> DbResult<()> {
        let done: bool = read_one(&self.db, SETTINGS, "pomodori_restore_done")?.unwrap_or(false);
        if done {
            return Ok(());
        }
        const RENAMES: [(&str, &str); 3] = [
            ("2 Pomodoros", "2 Pomodori"),
            ("3 Pomodoros", "3 Pomodori"),
            (">3 Pomodoros", ">3 Pomodori"),
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
        write_one(&txn, SETTINGS, "pomodori_restore_done", &true)?;
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
    /// its name yet, and rewrite a stored built-in whose snapshot differs
    /// from the shipped one; returns the id of the requested template
    /// (idempotent).
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
            let existing = self
                .list_templates()?
                .into_iter()
                .find(|template| template.built_in && template.name == *name);
            let id = match existing {
                Some(mut row) => {
                    let snapshot = snapshot_fn();
                    if row.snapshot != snapshot {
                        row.snapshot = snapshot;
                        let txn = self.db.begin_write()?;
                        write_one(&txn, TEMPLATES, &row.id, &row)?;
                        txn.commit()?;
                    }
                    row.id
                }
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

    /// Create a board with the standard color palette.
    pub fn create_board(&self, name: &str) -> DbResult<String> {
        let id = self.create_board_row(name)?;
        self.ensure_board_colors(&id)?;
        Ok(id)
    }

    /// Create a bare board row; the caller adds its colors.
    fn create_board_row(&self, name: &str) -> DbResult<String> {
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

    /// Find the board whose calendar-feed bearer token matches `raw`
    /// (KF-351). Linear scan: boards are few and feed clients poll
    /// infrequently, so no index is warranted. The token lives in the
    /// board's opaque config bag, which is never serialized to clients.
    pub fn get_board_by_feed_token(&self, raw: &str) -> DbResult<Option<BoardRow>> {
        for board in self.list_boards()? {
            if board.feed_token().as_deref() == Some(raw) {
                return Ok(Some(board));
            }
        }
        Ok(None)
    }

    pub fn board_exists(&self, id: &str) -> DbResult<bool> {
        Ok(self.get_board(id)?.is_some())
    }

    /// Trims a board name and checks it is 1-100 characters long.
    pub fn validate_board_name(name: &str) -> Result<&str, &'static str> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err("Board name must be 1-100 characters.");
        }
        Ok(name)
    }

    /// Rename a board. Errors on a blank or over-long name; returns false
    /// when the board is unknown.
    pub fn rename_board(&self, id: &str, name: &str) -> DbResult<bool> {
        let name = Self::validate_board_name(name)?;
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
    /// time entries, task colors, and the active timer when it runs on one
    /// of its tasks. Errors when the board id is unknown, and refuses to
    /// delete the last remaining board.
    pub fn delete_board(&self, id: &str) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        if txn_read_one::<BoardRow>(&txn, BOARDS, id)?.is_none() {
            return Err("board not found".into());
        }
        if txn.open_table(BOARDS)?.len()? <= 1 {
            return Err("cannot delete the last board".into());
        }
        let column_ids = txn_mmap_get(&txn, COLUMNS_BY_BOARD, id)?;
        let swimlane_ids = txn_mmap_get(&txn, SWIMLANES_BY_BOARD, id)?;
        let color_ids = txn_mmap_get(&txn, COLORS_BY_BOARD, id)?;
        let field_ids = txn_mmap_get(&txn, FIELDS_BY_BOARD, id)?;
        let mut task_ids: Vec<String> = Vec::new();
        let mut entry_ids: Vec<String> = Vec::new();
        for column_id in &column_ids {
            for task_id in txn_mmap_get(&txn, TASKS_BY_COLUMN, column_id)? {
                entry_ids.extend(txn_mmap_get(&txn, ENTRIES_BY_TASK, &task_id)?);
                task_ids.push(task_id);
            }
        }
        unlink_deleted_tasks(&txn, &task_ids)?;
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
            let mut fields = txn.open_table(FIELD_DEFS)?;
            for field_id in &field_ids {
                fields.remove(field_id.as_str())?;
                mmap_remove(&txn, FIELDS_BY_BOARD, id, field_id)?;
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
        txn_clear_timer_if(&txn, |task_id| task_ids.iter().any(|id| id == task_id))?;
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
            former_names: Vec::new(),
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, COLUMNS, &id, &row)?;
        mmap_insert(&txn, COLUMNS_BY_BOARD, board_id, &id)?;
        txn.commit()?;
        Ok(id)
    }

    /// Apply every field of `patch` in one transaction (KF-422). Toggling
    /// `is_done` stamps `updated_at`, in the same transaction, on every task
    /// whose `blocked` value flips. Returns false when the column is unknown.
    pub fn update_column(&self, id: &str, patch: &ColumnPatch<'_>) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let before = if patch.is_done.is_some() {
            let in_column = txn_mmap_get(&txn, TASKS_BY_COLUMN, id)?;
            blocked_states(&txn, &blocked_dependents(&txn, &in_column)?)?
        } else {
            HashMap::new()
        };
        {
            let mut columns = txn.open_table(COLUMNS)?;
            let stored: Option<ColumnRow> = columns
                .get(id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            let Some(mut column) = stored else {
                return Ok(false);
            };
            // KF-350: a rename remembers the old name in `former_names`.
            if let Some(name) = patch.name.filter(|name| *name != column.name) {
                let old = std::mem::replace(&mut column.name, name.to_string());
                column.former_names.retain(|former| former != &old);
                column.former_names.push(old);
            }
            if let Some(wip_limit) = patch.wip_limit {
                column.wip_limit = wip_limit;
            }
            if let Some(is_done) = patch.is_done {
                column.is_done = is_done;
            }
            if let Some(description) = patch.description {
                column.description = description.to_string();
            }
            if let Some(collapsed) = patch.collapsed {
                column.collapsed = collapsed;
            }
            if let Some(config_json) = patch.config_json {
                column.config_json = config_json.to_string();
            }
            let raw = serde_json::to_vec(&column)?;
            columns.insert(id, raw.as_slice())?;
        }
        bump_flipped(&txn, &before)?;
        txn.commit()?;
        Ok(true)
    }

    /// Rename a column, remembering the old name in `former_names`.
    pub fn rename_column(&self, id: &str, name: &str) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            if column.name != name {
                let old = std::mem::replace(&mut column.name, name.to_string());
                column.former_names.retain(|former| former != &old);
                column.former_names.push(old);
            }
        })
    }

    pub fn set_column_wip(&self, id: &str, wip_limit: Option<i64>) -> DbResult<bool> {
        mutate(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.wip_limit = wip_limit;
        })
    }

    /// Set a column's done flag. Tasks blocked by a task in the column
    /// whose `blocked` value flips get `updated_at` stamped in the same
    /// transaction.
    pub fn set_column_done(&self, id: &str, is_done: bool) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let in_column = txn_mmap_get(&txn, TASKS_BY_COLUMN, id)?;
        let before = blocked_states(&txn, &blocked_dependents(&txn, &in_column)?)?;
        {
            let mut columns = txn.open_table(COLUMNS)?;
            let column: Option<ColumnRow> = columns
                .get(id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            let Some(mut column) = column else {
                return Ok(false);
            };
            column.is_done = is_done;
            let raw = serde_json::to_vec(&column)?;
            columns.insert(id, raw.as_slice())?;
        }
        bump_flipped(&txn, &before)?;
        txn.commit()?;
        Ok(true)
    }

    /// Reorder a column within its board; positions are renumbered densely.
    /// Returns false when the column is unknown.
    pub fn move_column(&self, id: &str, position: i64) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(column) = txn_read_one::<ColumnRow>(&txn, COLUMNS, id)? else {
            return Ok(false);
        };
        let mut columns: Vec<ColumnRow> =
            txn_indexed_rows(&txn, COLUMNS_BY_BOARD, &column.board_id, COLUMNS)?;
        columns.retain(|column| column.id != id);
        columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        let at = position.clamp(0, columns.len() as i64) as usize;
        columns.insert(at, column);
        {
            let mut tbl = txn.open_table(COLUMNS)?;
            for (index, mut row) in columns.into_iter().enumerate() {
                row.position = index as i64;
                let bytes = serde_json::to_vec(&row)?;
                tbl.insert(row.id.as_str(), bytes.as_slice())?;
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

        let txn = self.db.begin_write()?;
        let old_target: Vec<ColorRow> =
            txn_indexed_rows(&txn, COLORS_BY_BOARD, target_board_id, TASK_COLORS)?;
        let old_value_by_id: HashMap<String, String> = old_target
            .iter()
            .map(|color| (color.id.clone(), color.value.clone()))
            .collect();
        let mut tasks: Vec<TaskRow> = Vec::new();
        for column_id in txn_mmap_get(&txn, COLUMNS_BY_BOARD, target_board_id)? {
            tasks.extend(txn_indexed_rows::<TaskRow>(
                &txn,
                TASKS_BY_COLUMN,
                &column_id,
                TASKS,
            )?);
        }
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
            for mut task in tasks {
                let new_id = task
                    .color_id
                    .as_deref()
                    .and_then(|color_id| old_value_by_id.get(color_id))
                    .and_then(|value| new_id_by_value.get(value));
                if let Some(new_id) = new_id {
                    task.color_id = Some(new_id.clone());
                    task.updated_at = Some(Utc::now().to_rfc3339());
                    let bytes = serde_json::to_vec(&task)?;
                    tasks_tbl.insert(task.id.as_str(), bytes.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(copies.len())
    }

    /// Delete a column. Errors while it still holds tasks; callers check
    /// first to answer with a readable 400.
    pub fn delete_column(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(column) = txn_read_one::<ColumnRow>(&txn, COLUMNS, id)? else {
            return Ok(false);
        };
        if !txn_mmap_get(&txn, TASKS_BY_COLUMN, id)?.is_empty() {
            return Err("column still holds tasks".into());
        }
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
        let txn = self.db.begin_write()?;
        let Some(lane) = txn_read_one::<SwimlaneRow>(&txn, SWIMLANES, id)? else {
            return Ok(false);
        };
        let mut lanes: Vec<SwimlaneRow> =
            txn_indexed_rows(&txn, SWIMLANES_BY_BOARD, &lane.board_id, SWIMLANES)?;
        lanes.retain(|lane| lane.id != id);
        lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        let at = position.clamp(0, lanes.len() as i64) as usize;
        lanes.insert(at, lane);
        {
            let mut tbl = txn.open_table(SWIMLANES)?;
            for (index, mut row) in lanes.into_iter().enumerate() {
                row.position = index as i64;
                let bytes = serde_json::to_vec(&row)?;
                tbl.insert(row.id.as_str(), bytes.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// Delete a swimlane. Errors while it still holds tasks; callers check
    /// first to answer with a readable 400.
    pub fn delete_swimlane(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(lane) = txn_read_one::<SwimlaneRow>(&txn, SWIMLANES, id)? else {
            return Ok(false);
        };
        let in_use = {
            let tbl = txn.open_table(TASKS)?;
            let mut in_use = false;
            for item in tbl.iter()? {
                let (_, value) = item?;
                let task: TaskRow = serde_json::from_slice(value.value())?;
                in_use |= task.swimlane_id.as_deref() == Some(id);
            }
            in_use
        };
        if in_use {
            return Err("swimlane still holds tasks".into());
        }
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

    /// Reads one color by id.
    ///
    /// A standard color reads back with the palette's current hex values, so
    /// rows stored before a palette correction (KF-245) render correctly.
    pub fn get_color(&self, id: &str) -> DbResult<Option<ColorRow>> {
        let color: Option<ColorRow> = read_one(&self.db, TASK_COLORS, id)?;
        Ok(color.map(|mut color| {
            if let Some((bg, border, light, _)) = standard_color(&color.value) {
                color.background_hex = bg.to_string();
                color.border_hex = border.to_string();
                color.light_hex = light.to_string();
            }
            color
        }))
    }

    /// Colors of a board, ordered by sort_order (then id for stability).
    /// Read-only (KF-420): boards that predate colors are backfilled at
    /// startup instead.
    pub fn list_colors(&self, board_id: &str) -> DbResult<Vec<ColorRow>> {
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
    /// colors (all enabled, standard names, yellow default).
    pub fn ensure_board_colors(&self, board_id: &str) -> DbResult<()> {
        if !mmap_get(&self.db, COLORS_BY_BOARD, board_id)?.is_empty() {
            return Ok(());
        }
        // Don't backfill colors for a board that doesn't exist (e.g. after
        // deletion); list_colors on a deleted board must stay empty.
        if self.get_board(board_id)?.is_none() {
            return Ok(());
        }
        for (value, label, enabled, is_default, sort_order) in standard_color_specs() {
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
        let txn = self.db.begin_write()?;
        let Some(color) = txn_read_one::<ColorRow>(&txn, TASK_COLORS, id)? else {
            return Ok(false);
        };
        let mut colors: Vec<ColorRow> =
            txn_indexed_rows(&txn, COLORS_BY_BOARD, &color.board_id, TASK_COLORS)?;
        colors.retain(|color| color.id != id);
        colors.sort_by(|a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| a.id.cmp(&b.id))
        });
        let at = position.clamp(0, colors.len() as i64) as usize;
        colors.insert(at, color);
        {
            let mut tbl = txn.open_table(TASK_COLORS)?;
            for (index, mut row) in colors.into_iter().enumerate() {
                row.sort_order = index as i64;
                let bytes = serde_json::to_vec(&row)?;
                tbl.insert(row.id.as_str(), bytes.as_slice())?;
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
        let board_id = self.create_board_row(new_board_name)?;
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
                due_target_column_id: None,
                estimate_hours: task.get("estimate_hours").and_then(|v| v.as_f64()),
                column_added_at: Some(now.clone()),
                comments: Vec::new(),
                attachments: Vec::new(),
                history: Vec::new(),
                updated_at: Some(now),
                metadata: None,
                links: Vec::new(),
                claim: None,
                claim_fence: 0,
                fields: serde_json::Map::new(),
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
        self.create_task_placed(
            column_id,
            swimlane_id,
            name,
            size,
            color_id,
            NewTaskPlacement::Bottom,
        )
    }

    /// Creates a task at the top or bottom of its cell; returns its id.
    ///
    /// Otherwise the same as [`Db::create_task`].
    pub fn create_task_placed(
        &self,
        column_id: &str,
        swimlane_id: Option<&str>,
        name: &str,
        size: i64,
        color_id: Option<&str>,
        placement: NewTaskPlacement,
    ) -> DbResult<String> {
        self.insert_task(
            Self::new_task_row(column_id, swimlane_id, name, size, color_id),
            placement,
        )
    }

    /// A new, unsaved task row for [`Db::insert_task`], stamped now.
    pub fn new_task_row(
        column_id: &str,
        swimlane_id: Option<&str>,
        name: &str,
        size: i64,
        color_id: Option<&str>,
    ) -> TaskRow {
        let now = Utc::now().to_rfc3339();
        TaskRow {
            id: Uuid::new_v4().to_string(),
            column_id: column_id.to_string(),
            swimlane_id: swimlane_id.map(str::to_string),
            name: name.to_string(),
            description: String::new(),
            size,
            position: 0.0,
            created_at: now.clone(),
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
            due_target_column_id: None,
            estimate_hours: None,
            column_added_at: Some(now.clone()),
            comments: Vec::new(),
            attachments: Vec::new(),
            history: Vec::new(),
            updated_at: Some(now),
            metadata: None,
            links: Vec::new(),
            claim: None,
            claim_fence: 0,
            fields: serde_json::Map::new(),
        }
    }

    /// Store a prepared task row at the top or bottom of its cell in one
    /// write; returns its id. Also backfills the board's color palette for
    /// boards created before colors existed.
    pub fn insert_task(&self, row: TaskRow, placement: NewTaskPlacement) -> DbResult<String> {
        Ok(self
            .insert_task_checked(row, placement)?
            .unwrap_or_else(|_| unreachable!("a row without fields always passes")))
    }

    /// [`Db::insert_task`], checking the row's field values against the
    /// board's field definitions as stored in the same transaction.
    /// Returns `Err` with a message naming the key, writing nothing, when
    /// one no longer fits.
    pub fn insert_task_checked(
        &self,
        mut row: TaskRow,
        placement: NewTaskPlacement,
    ) -> DbResult<Result<String, String>> {
        if let Some(column) = self.get_column(&row.column_id)? {
            self.ensure_board_colors(&column.board_id)?;
        }
        let txn = self.db.begin_write()?;
        {
            if !row.fields.is_empty() {
                let board_id = column_board_in(&txn, &row.column_id)?;
                let defs = match &board_id {
                    Some(board_id) => board_fields_in(&txn, board_id)?,
                    None => Vec::new(),
                };
                for (key, value) in &row.fields {
                    let Some(def) = defs.iter().find(|def| def.key == *key) else {
                        return Ok(Err(format!("unknown field \"{key}\"")));
                    };
                    if let Err(why) = def.check_value(value) {
                        return Ok(Err(format!("field \"{key}\": {why}")));
                    }
                }
            }
            // The cell is read inside the transaction so two creates
            // cannot take the same position.
            let cell = txn_mmap_get(&txn, TASKS_BY_COLUMN, &row.column_id)?;
            let tasks = txn.open_table(TASKS)?;
            let mut bounds: Option<(f64, f64)> = None;
            for task_id in &cell {
                if let Some(task) = task_in(&tasks, task_id)? {
                    if task.swimlane_id == row.swimlane_id {
                        let p = task.position;
                        bounds = Some(bounds.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))));
                    }
                }
            }
            row.position = match (bounds, placement) {
                (None, _) => 0.0,
                (Some((lo, _)), NewTaskPlacement::Top) => lo - 1.0,
                (Some((_, hi)), NewTaskPlacement::Bottom) => hi + 1.0,
            };
        }
        write_one(&txn, TASKS, &row.id, &row)?;
        mmap_insert(&txn, TASKS_BY_COLUMN, &row.column_id, &row.id)?;
        txn.commit()?;
        Ok(Ok(row.id))
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
        mutate_task(&self.db, id, |task| {
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
        let found = mutate_task(&self.db, task_id, |task| {
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
        // KF-423: an unknown subtask writes nothing.
        let touched = self.apply_task_change(task_id, |task| {
            let Some(sub) = task.subtasks.iter_mut().find(|s| s.id == sub_id) else {
                return (false, false);
            };
            if let Some(name) = name {
                sub.name = name.to_string();
            }
            if let Some(done) = done {
                sub.done = done;
            }
            (true, true)
        })?;
        Ok(touched == Some(true))
    }

    /// Remove a subtask. Returns false when the task or subtask does not exist.
    pub fn remove_subtask(&self, task_id: &str, sub_id: &str) -> DbResult<bool> {
        // KF-423: an unknown subtask writes nothing.
        let removed = self.apply_task_change(task_id, |task| {
            let before = task.subtasks.len();
            task.subtasks.retain(|s| s.id != sub_id);
            let removed = task.subtasks.len() != before;
            (removed, removed)
        })?;
        Ok(removed == Some(true))
    }

    /// Reorder a task's subtasks to the given id sequence. Ids in `order`
    /// that exist take the given positions; any current subtasks not
    /// mentioned are appended in their original relative order so a partial
    /// list can never silently drop subtasks. Returns false when the task
    /// does not exist.
    pub fn reorder_subtasks(&self, task_id: &str, order: &[String]) -> DbResult<bool> {
        mutate_task(&self.db, task_id, |task| {
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
        mutate_task(&self.db, task_id, |task| {
            task.member_ids = ids.clone();
        })
    }

    /// Set (Some) or clear (None) a task's grouping-date override.
    pub fn set_grouping_date(&self, task_id: &str, date: Option<&str>) -> DbResult<bool> {
        let date = date.map(str::to_string);
        mutate_task(&self.db, task_id, |task| {
            task.grouping_date = date.clone();
        })
    }

    /// Set a task's watch flag (KanbanFlow parity: "Watch" in the task
    /// More menu). Returns false when the task does not exist.
    pub fn set_task_watched(&self, task_id: &str, watched: bool) -> DbResult<bool> {
        mutate_task(&self.db, task_id, |task| {
            task.watched = watched;
        })
    }

    /// Replace a task's labels (KanbanFlow parity). Labels are trimmed,
    /// empties dropped, duplicates removed, order kept. Returns false when
    /// the task does not exist.
    pub fn set_task_labels(&self, task_id: &str, labels: &[String]) -> DbResult<bool> {
        let clean = clean_labels(labels);
        mutate_task(&self.db, task_id, |task| {
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
        mutate_task(&self.db, task_id, |task| {
            task.due_at = due_at.clone();
            task.due_repeat = repeat.clone();
            // Clearing the due date also clears its done flag and target
            // column; there is no date left to reach (KF-224, KF-259).
            if due_at.is_none() {
                task.due_done = false;
                task.due_target_column_id = None;
            }
        })
    }

    /// Mark a task's due date done/undone (KanbanFlow parity: checking the
    /// due-date item; the card renders "(Done)"). No-op for tasks without
    /// a due date.
    pub fn set_task_due_done(&self, task_id: &str, done: bool) -> DbResult<bool> {
        mutate_task(&self.db, task_id, |task| {
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
        let hours = hours.map(round_estimate);
        mutate_task(&self.db, task_id, |task| {
            task.estimate_hours = hours;
        })
    }

    /// Append a comment and its history event in one write. With a
    /// `fence`, a live claim holding another fence refuses the write.
    pub fn add_comment(
        &self,
        task_id: &str,
        new: NewComment,
        actor: &str,
        fence: Option<u64>,
    ) -> DbResult<FencedWrite<TaskComment>> {
        let comment = TaskComment {
            id: Uuid::new_v4().to_string(),
            author: new.author.to_string(),
            body: new.body.to_string(),
            created_at: Utc::now().to_rfc3339(),
            actor_kind: new.actor_kind,
            token_name: new.token_name.map(str::to_string),
            kind: new.kind.map(str::to_string),
        };
        let outcome = self.apply_task_change(task_id, |task| {
            if !task.fence_allows(fence) {
                return (FencedWrite::StaleFence, false);
            }
            task.comments.push(comment.clone());
            let detail = format!("{} added a comment", new.author);
            push_event(task, "comment_added", detail, actor);
            (FencedWrite::Written(comment), true)
        })?;
        Ok(outcome.unwrap_or(FencedWrite::NotFound))
    }

    /// Delete a comment from a task, logging the deletion in the same
    /// write. Returns false when the task or the comment does not exist.
    pub fn delete_comment(&self, task_id: &str, comment_id: &str, actor: &str) -> DbResult<bool> {
        let removed = self.apply_task_change(task_id, |task| {
            let before = task.comments.len();
            task.comments.retain(|c| c.id != comment_id);
            let removed = task.comments.len() != before;
            if removed {
                push_event(task, "comment_deleted", "Deleted a comment".into(), actor);
            }
            (removed, removed)
        })?;
        Ok(removed.unwrap_or(false))
    }

    /// Link `task_id` to `target_id` in one transaction. A `blocks` link is
    /// stored as `blocked_by` on the target; asking for a link that already
    /// exists returns it unchanged.
    pub fn add_task_link(
        &self,
        task_id: &str,
        kind: LinkKind,
        target_id: &str,
        actor: &str,
    ) -> DbResult<LinkAddOutcome> {
        let (holder_id, stored_kind, stored_target) = match kind {
            LinkKind::Blocks => (target_id, LinkKind::BlockedBy, task_id),
            other => (task_id, other, target_id),
        };
        let as_requested = |link: &TaskLink| TaskLink {
            kind,
            target_id: target_id.to_string(),
            ..link.clone()
        };
        let txn = self.db.begin_write()?;
        let link = {
            let mut tbl = txn.open_table(TASKS)?;
            let (Some(mut holder), Some(other)) =
                (task_in(&tbl, holder_id)?, task_in(&tbl, stored_target)?)
            else {
                return Ok(LinkAddOutcome::NotFound);
            };
            let same = |link: &&TaskLink, target: &str| {
                link.kind == stored_kind && link.target_id == target
            };
            let existing = holder
                .links
                .iter()
                .find(|link| same(link, stored_target))
                .or_else(|| match stored_kind {
                    LinkKind::Related => other.links.iter().find(|link| same(link, holder_id)),
                    _ => None,
                });
            if let Some(existing) = existing {
                return Ok(LinkAddOutcome::Added(as_requested(existing)));
            }
            if stored_kind == LinkKind::BlockedBy
                && blocked_by_reaches(&tbl, stored_target, holder_id)?
            {
                return Ok(LinkAddOutcome::Cycle);
            }
            let now = Utc::now().to_rfc3339();
            let link = TaskLink {
                id: Uuid::new_v4().to_string(),
                kind: stored_kind,
                target_id: stored_target.to_string(),
                created_at: now.clone(),
                board_id: None,
            };
            holder.links.push(link.clone());
            let detail = format!("Added link {}", link_event_detail(&link, Some(&other.name)));
            push_event(&mut holder, "link_added", detail, actor);
            holder.updated_at = Some(now);
            let raw = serde_json::to_vec(&holder)?;
            tbl.insert(holder_id, raw.as_slice())?;
            link
        };
        mmap_insert(&txn, LINKS_BY_TARGET, stored_target, holder_id)?;
        txn.commit()?;
        Ok(LinkAddOutcome::Added(as_requested(&link)))
    }

    /// A task's links as seen from that task: the ones it holds, plus a
    /// `blocks` or `related` entry for each link another task holds on it.
    /// Oldest first; None when the task does not exist.
    pub fn list_task_links(&self, task_id: &str) -> DbResult<Option<Vec<TaskLink>>> {
        let Some(task) = self.get_task(task_id)? else {
            return Ok(None);
        };
        let mut links = task.links;
        for holder_id in mmap_get(&self.db, LINKS_BY_TARGET, task_id)? {
            let Some(holder) = self.get_task(&holder_id)? else {
                continue;
            };
            for link in holder.links.iter().filter(|link| link.target_id == task_id) {
                links.push(TaskLink {
                    kind: match link.kind {
                        LinkKind::BlockedBy => LinkKind::Blocks,
                        other => other,
                    },
                    target_id: holder.id.clone(),
                    ..link.clone()
                });
            }
        }
        links.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for link in &mut links {
            link.board_id = self.task_board_id(&link.target_id)?;
        }
        Ok(Some(links))
    }

    /// The board a task sits on, through its column.
    pub fn task_board_id(&self, task_id: &str) -> DbResult<Option<String>> {
        let Some(task) = self.get_task(task_id)? else {
            return Ok(None);
        };
        Ok(self
            .get_column(&task.column_id)?
            .map(|column| column.board_id))
    }

    /// Delete a link listed on `task_id`, whichever task holds it. Returns
    /// false when the task or the link does not exist.
    pub fn delete_task_link(&self, task_id: &str, link_id: &str, actor: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let mut holders = vec![task_id.to_string()];
        holders.extend(txn_mmap_get(&txn, LINKS_BY_TARGET, task_id)?);
        let mut found = false;
        {
            let mut tbl = txn.open_table(TASKS)?;
            let mut by_target = txn.open_multimap_table(LINKS_BY_TARGET)?;
            for holder_id in &holders {
                let Some(mut holder) = task_in(&tbl, holder_id)? else {
                    continue;
                };
                let Some(index) = holder.links.iter().position(|link| {
                    link.id == link_id && (holder_id == task_id || link.target_id == task_id)
                }) else {
                    continue;
                };
                let link = holder.links.remove(index);
                let target_name = task_in(&tbl, &link.target_id)?.map(|target| target.name);
                let detail = format!(
                    "Removed link {}",
                    link_event_detail(&link, target_name.as_deref())
                );
                push_event(&mut holder, "link_removed", detail, actor);
                let still_linked = holder
                    .links
                    .iter()
                    .any(|other| other.target_id == link.target_id);
                holder.updated_at = Some(Utc::now().to_rfc3339());
                let raw = serde_json::to_vec(&holder)?;
                tbl.insert(holder_id.as_str(), raw.as_slice())?;
                if !still_linked {
                    by_target.remove(link.target_id.as_str(), holder_id.as_str())?;
                }
                found = true;
                break;
            }
        }
        if found {
            txn.commit()?;
        }
        Ok(found)
    }

    /// True when any `blocked_by` target of `task` sits in a column whose
    /// `is_done` is false.
    pub fn task_blocked(&self, task: &TaskRow) -> DbResult<bool> {
        for link in task.links.iter().filter(|l| l.kind == LinkKind::BlockedBy) {
            let Some(target) = self.get_task(&link.target_id)? else {
                continue;
            };
            if self
                .get_column(&target.column_id)?
                .is_some_and(|column| !column.is_done)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn get_field(&self, id: &str) -> DbResult<Option<FieldDefRow>> {
        read_one(&self.db, FIELD_DEFS, id)
    }

    /// A board's custom fields, ordered by position, then creation time.
    pub fn list_fields(&self, board_id: &str) -> DbResult<Vec<FieldDefRow>> {
        let mut fields = Vec::new();
        for id in mmap_get(&self.db, FIELDS_BY_BOARD, board_id)? {
            if let Some(field) = self.get_field(&id)? {
                fields.push(field);
            }
        }
        fields.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        Ok(fields)
    }

    /// Store a new field at the end of its board's list, refusing a key the
    /// board already has. The caller fills every field except `id`,
    /// `position` and `created_at`.
    pub fn create_field(&self, mut field: FieldDefRow) -> DbResult<FieldCreateOutcome> {
        let txn = self.db.begin_write()?;
        {
            let defs = txn.open_table(FIELD_DEFS)?;
            let mut last = -1;
            for id in txn_mmap_get(&txn, FIELDS_BY_BOARD, &field.board_id)? {
                let Some(guard) = defs.get(id.as_str())? else {
                    continue;
                };
                let existing: FieldDefRow = serde_json::from_slice(guard.value())?;
                if existing.key == field.key {
                    return Ok(FieldCreateOutcome::KeyExists);
                }
                last = last.max(existing.position);
            }
            field.id = Uuid::new_v4().to_string();
            field.position = last + 1;
            field.created_at = Utc::now().to_rfc3339();
        }
        write_one(&txn, FIELD_DEFS, &field.id, &field)?;
        mmap_insert(&txn, FIELDS_BY_BOARD, &field.board_id, &field.id)?;
        txn.commit()?;
        Ok(FieldCreateOutcome::Created(field))
    }

    /// Run `f` on a stored field definition inside one write transaction,
    /// storing it when `f` reports a change. Returns None when unknown.
    pub fn apply_field_change<R, F>(&self, id: &str, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut FieldDefRow) -> (R, bool),
    {
        let txn = self.db.begin_write()?;
        let result = {
            let mut defs = txn.open_table(FIELD_DEFS)?;
            let stored: Option<FieldDefRow> = defs
                .get(id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            let Some(mut field) = stored else {
                return Ok(None);
            };
            let (result, changed) = f(&mut field);
            if changed {
                let raw = serde_json::to_vec(&field)?;
                defs.insert(id, raw.as_slice())?;
            }
            (result, changed)
        };
        match result {
            (result, true) => {
                txn.commit()?;
                Ok(Some(result))
            }
            (result, false) => {
                txn.abort()?;
                Ok(Some(result))
            }
        }
    }

    /// Delete a field and, in the same transaction, its value on every task
    /// of the board. Returns false when unknown.
    pub fn delete_field(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let field = {
            let mut defs = txn.open_table(FIELD_DEFS)?;
            let stored: Option<FieldDefRow> = defs
                .get(id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            let Some(field) = stored else {
                return Ok(false);
            };
            defs.remove(id)?;
            field
        };
        let mut task_ids = Vec::new();
        for column_id in txn_mmap_get(&txn, COLUMNS_BY_BOARD, &field.board_id)? {
            task_ids.extend(txn_mmap_get(&txn, TASKS_BY_COLUMN, &column_id)?);
        }
        {
            let mut tasks = txn.open_table(TASKS)?;
            for task_id in &task_ids {
                let Some(mut task) = task_in(&tasks, task_id)? else {
                    continue;
                };
                if task.fields.remove(&field.key).is_some() {
                    task.updated_at = Some(Utc::now().to_rfc3339());
                    let raw = serde_json::to_vec(&task)?;
                    tasks.insert(task_id.as_str(), raw.as_slice())?;
                }
            }
        }
        mmap_remove(&txn, FIELDS_BY_BOARD, &field.board_id, id)?;
        txn.commit()?;
        Ok(true)
    }

    /// Seconds a device-pairing code stays valid.
    pub const DEVICE_CODE_TTL_SECONDS: i64 = 600;
    /// Minimum seconds between token polls for one device code.
    pub const DEVICE_POLL_INTERVAL_SECONDS: i64 = 5;
    /// Most device-pairing requests that may be pending at once.
    pub const MAX_PENDING_DEVICE_AUTHS: usize = 20;
    /// Seconds an expired or spent request is kept, so late polls still
    /// get a definite answer, before cleanup deletes it.
    const DEVICE_AUTH_RETENTION_SECONDS: i64 = 86_400;

    /// Store a new device-pairing request, first deleting requests past
    /// their retention. All in one transaction.
    pub fn create_device_auth(
        &self,
        row: &DeviceAuthRow,
        now: i64,
    ) -> DbResult<DeviceAuthCreateOutcome> {
        let txn = self.db.begin_write()?;
        let outcome = {
            let mut tbl = txn.open_table(DEVICE_AUTHS)?;
            let mut stale = Vec::new();
            let mut pending = 0;
            for item in tbl.iter()? {
                let (key, value) = item?;
                let existing: DeviceAuthRow = serde_json::from_slice(value.value())?;
                if existing.expires_at + Self::DEVICE_AUTH_RETENTION_SECONDS < now {
                    stale.push(key.value().to_owned());
                    continue;
                }
                let live = existing.expires_at >= now;
                if live && existing.user_code == row.user_code {
                    return Ok(DeviceAuthCreateOutcome::UserCodeTaken);
                }
                if live && existing.status == DeviceAuthStatus::Pending {
                    pending += 1;
                }
            }
            for key in &stale {
                tbl.remove(key.as_str())?;
            }
            if pending >= Self::MAX_PENDING_DEVICE_AUTHS {
                DeviceAuthCreateOutcome::TooManyPending
            } else {
                let raw = serde_json::to_vec(row)?;
                tbl.insert(row.device_code_hash.as_str(), raw.as_slice())?;
                DeviceAuthCreateOutcome::Created
            }
        };
        txn.commit()?;
        Ok(outcome)
    }

    /// The live, pending request with this canonical user code.
    pub fn pending_device_auth(
        &self,
        user_code: &str,
        now: i64,
    ) -> DbResult<Option<DeviceAuthRow>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(DEVICE_AUTHS)?;
        for item in tbl.iter()? {
            let (_, value) = item?;
            let row: DeviceAuthRow = serde_json::from_slice(value.value())?;
            if row.user_code == user_code
                && row.status == DeviceAuthStatus::Pending
                && row.expires_at >= now
            {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    /// True when this canonical user code belongs to a request that has not
    /// expired, decided or not. Only codes that never existed, or expired,
    /// count toward the wrong-code lockout.
    pub fn device_code_known(&self, user_code: &str, now: i64) -> DbResult<bool> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(DEVICE_AUTHS)?;
        for item in tbl.iter()? {
            let (_, value) = item?;
            let row: DeviceAuthRow = serde_json::from_slice(value.value())?;
            if row.user_code == user_code && row.expires_at >= now {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Approve or deny the live, pending request with this user code.
    /// Returns false when there is none.
    pub fn decide_device_auth(&self, user_code: &str, approve: bool, now: i64) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let decided = {
            let mut tbl = txn.open_table(DEVICE_AUTHS)?;
            let mut found = None;
            for item in tbl.iter()? {
                let (key, value) = item?;
                let row: DeviceAuthRow = serde_json::from_slice(value.value())?;
                if row.user_code == user_code
                    && row.status == DeviceAuthStatus::Pending
                    && row.expires_at >= now
                {
                    found = Some((key.value().to_owned(), row));
                    break;
                }
            }
            match found {
                Some((key, mut row)) => {
                    row.status = if approve {
                        DeviceAuthStatus::Approved
                    } else {
                        DeviceAuthStatus::Denied
                    };
                    let raw = serde_json::to_vec(&row)?;
                    tbl.insert(key.as_str(), raw.as_slice())?;
                    true
                }
                None => false,
            }
        };
        if decided {
            txn.commit()?;
        } else {
            txn.abort()?;
        }
        Ok(decided)
    }

    /// Answer one token poll in one transaction. An approved request has
    /// `mint` build its token, which is stored while the request is marked
    /// spent; every poll records its time for the interval check.
    pub fn poll_device_auth<F>(
        &self,
        device_code_hash: &str,
        now: i64,
        mint: F,
    ) -> DbResult<DevicePoll>
    where
        F: FnOnce(&DeviceAuthRow) -> ApiTokenRecord,
    {
        let txn = self.db.begin_write()?;
        let poll = {
            let mut tbl = txn.open_table(DEVICE_AUTHS)?;
            let row: Option<DeviceAuthRow> = tbl
                .get(device_code_hash)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            let Some(mut row) = row else {
                return Ok(DevicePoll::Invalid);
            };
            let too_soon = row
                .last_poll_at
                .is_some_and(|last| now - last < Self::DEVICE_POLL_INTERVAL_SECONDS);
            row.last_poll_at = Some(now);
            let poll = if row.status == DeviceAuthStatus::Issued {
                DevicePoll::Invalid
            } else if row.expires_at < now {
                DevicePoll::Expired
            } else if too_soon {
                DevicePoll::SlowDown
            } else {
                match row.status {
                    DeviceAuthStatus::Pending => DevicePoll::Pending,
                    DeviceAuthStatus::Denied => DevicePoll::Denied,
                    DeviceAuthStatus::Approved => {
                        let record = mint(&row);
                        write_one(&txn, API_TOKENS, &record.id, &record)?;
                        row.status = DeviceAuthStatus::Issued;
                        DevicePoll::Issued(Box::new(record))
                    }
                    DeviceAuthStatus::Issued => DevicePoll::Invalid,
                }
            };
            let raw = serde_json::to_vec(&row)?;
            tbl.insert(device_code_hash, raw.as_slice())?;
            poll
        };
        txn.commit()?;
        Ok(poll)
    }

    /// Maximum serialized size of a task's metadata object: 64 KiB.
    pub const MAX_METADATA_BYTES: usize = 64 * 1024;

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
                    let detail = format!("Attached \"{name}\"");
                    push_event(&mut task, "attachment_added", detail, uploaded_by);
                    task.updated_at = Some(Utc::now().to_rfc3339());
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

    /// Delete an attachment: removes the metadata from the task row, the
    /// bytes from the data table, and logs the deletion, in one write.
    /// Returns false, writing nothing, when the task or the attachment does
    /// not exist.
    pub fn delete_attachment(
        &self,
        task_id: &str,
        attachment_id: &str,
        actor: &str,
    ) -> DbResult<bool> {
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
                    let detail = "Deleted an attachment".to_string();
                    push_event(&mut task, "attachment_deleted", detail, actor);
                    task.updated_at = Some(Utc::now().to_rfc3339());
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
        mutate_task(&self.db, task_id, |task| {
            push_event(task, kind, detail.to_string(), actor);
        })
    }

    /// Run `f` on one task inside a single write transaction, storing the
    /// row (with `updated_at` stamped) when `f` reports a change. When it
    /// reports none, nothing is written. Returns None when the task does
    /// not exist.
    pub fn apply_task_change<R, F>(&self, task_id: &str, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow) -> (R, bool),
    {
        self.change_task(task_id, false, |task, _| {
            let (result, changed) = f(task);
            (
                result,
                if changed {
                    RowWrite::Stamp
                } else {
                    RowWrite::Skip
                },
            )
        })
    }

    /// [`Db::apply_task_change`] for changes that depend on the task's
    /// board: `f` also gets the [`TaskContext`], read in the same
    /// transaction.
    pub fn apply_task_change_in_board<R, F>(&self, task_id: &str, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow, &TaskContext) -> (R, bool),
    {
        self.change_task(task_id, true, |task, context| {
            let (result, changed) = f(task, context);
            (
                result,
                if changed {
                    RowWrite::Stamp
                } else {
                    RowWrite::Skip
                },
            )
        })
    }

    fn change_task<R, F>(&self, task_id: &str, with_context: bool, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow, &TaskContext) -> (R, RowWrite),
    {
        let txn = self.db.begin_write()?;
        let result = {
            let mut tbl = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tbl, task_id)? else {
                return Ok(None);
            };
            let context = if with_context {
                task_context_in(&txn, &task)?
            } else {
                TaskContext::default()
            };
            let (result, write) = f(&mut task, &context);
            if matches!(write, RowWrite::Stamp) {
                task.updated_at = Some(Utc::now().to_rfc3339());
            }
            if !matches!(write, RowWrite::Skip) {
                let raw = serde_json::to_vec(&task)?;
                tbl.insert(task_id, raw.as_slice())?;
            }
            (result, write)
        };
        // KF-423: a change that writes nothing commits nothing, so a
        // rejected request leaves the database file untouched.
        match result {
            (result, RowWrite::Skip) => {
                txn.abort()?;
                Ok(Some(result))
            }
            (result, _) => {
                txn.commit()?;
                Ok(Some(result))
            }
        }
    }

    /// Claim a task with a compare-and-set in one write transaction. With
    /// no live claim the caller gets a new claim under the next fence. A
    /// live claim by the same claimant with a matching `fence` is renewed;
    /// any other live claim is returned as [`ClaimOutcome::Held`].
    pub fn claim_task(
        &self,
        task_id: &str,
        request: &ClaimRequest,
        actor: &str,
    ) -> DbResult<ClaimOutcome> {
        let outcome = self.change_task(task_id, false, |task, _| {
            let now = Utc::now();
            let expired = expire_claim(task, now);
            if let Some(live) = task.claim.as_mut() {
                if live.claimant != request.claimant || request.fence != Some(live.fence) {
                    let write = if expired {
                        RowWrite::Stamp
                    } else {
                        RowWrite::Skip
                    };
                    return (ClaimOutcome::Held(live.clone()), write);
                }
                let fields = [
                    (&mut live.role, &request.role),
                    (&mut live.harness, &request.harness),
                    (&mut live.model, &request.model),
                    (&mut live.jj_workspace, &request.jj_workspace),
                    (&mut live.jj_change_id, &request.jj_change_id),
                    (&mut live.session_id, &request.session_id),
                ];
                for (stored, given) in fields {
                    if given.is_some() {
                        stored.clone_from(given);
                    }
                }
                live.heartbeat_at = now.to_rfc3339();
                live.expires_at = lease_end(now, request.ttl_seconds);
                // A renewal changes only the lease, like a heartbeat.
                return (ClaimOutcome::Granted(live.clone()), RowWrite::Keep);
            }
            task.claim_fence += 1;
            let claim = TaskClaim {
                claimant: request.claimant.clone(),
                role: request.role.clone(),
                harness: request.harness.clone(),
                model: request.model.clone(),
                jj_workspace: request.jj_workspace.clone(),
                jj_change_id: request.jj_change_id.clone(),
                session_id: request.session_id.clone(),
                fence: task.claim_fence,
                started_at: now.to_rfc3339(),
                heartbeat_at: now.to_rfc3339(),
                expires_at: lease_end(now, request.ttl_seconds),
            };
            let detail = format!(
                "Claimed by {} (fence {}) until {}",
                claim.claimant, claim.fence, claim.expires_at
            );
            push_event(task, "claimed", detail, actor);
            task.claim = Some(claim.clone());
            (ClaimOutcome::Granted(claim), RowWrite::Stamp)
        })?;
        Ok(outcome.unwrap_or(ClaimOutcome::NotFound))
    }

    /// Extend the live claim holding `fence`. Without `ttl_seconds` the
    /// lease keeps its previous length and `updated_at` is left alone. An
    /// expired claim is cleared and reported as a mismatch.
    pub fn heartbeat_claim(
        &self,
        task_id: &str,
        fence: u64,
        ttl_seconds: Option<i64>,
        session_id: Option<&str>,
    ) -> DbResult<ClaimOutcome> {
        let outcome = self.change_task(task_id, false, |task, _| {
            let now = Utc::now();
            let expired = expire_claim(task, now);
            let Some(live) = task.claim.as_mut().filter(|claim| claim.fence == fence) else {
                let write = if expired {
                    RowWrite::Stamp
                } else {
                    RowWrite::Skip
                };
                return (ClaimOutcome::FenceMismatch, write);
            };
            let previous_ttl = chrono::DateTime::parse_from_rfc3339(&live.expires_at)
                .ok()
                .zip(chrono::DateTime::parse_from_rfc3339(&live.heartbeat_at).ok())
                .map(|(end, beat)| (end - beat).num_seconds())
                .unwrap_or(0);
            live.heartbeat_at = now.to_rfc3339();
            live.expires_at = lease_end(now, ttl_seconds.unwrap_or(previous_ttl));
            if let Some(session_id) = session_id {
                live.session_id = Some(session_id.to_string());
            }
            // A heartbeat is not a change to the task, so `?since=` pollers
            // do not see every claimed task on every tick.
            (ClaimOutcome::Granted(live.clone()), RowWrite::Keep)
        })?;
        Ok(outcome.unwrap_or(ClaimOutcome::NotFound))
    }

    /// Release the live claim holding `fence`. With no live claim, any
    /// fence the task has issued counts as already released.
    pub fn release_claim(&self, task_id: &str, fence: u64, actor: &str) -> DbResult<ClaimOutcome> {
        let outcome = self.apply_task_change(task_id, |task| {
            let now = Utc::now();
            let expired = expire_claim(task, now);
            match &task.claim {
                Some(live) if live.fence == fence => {
                    let detail = format!("Claim by {} (fence {}) released", live.claimant, fence);
                    task.claim = None;
                    push_event(task, "claim_released", detail, actor);
                    (ClaimOutcome::Released, true)
                }
                None if (1..=task.claim_fence).contains(&fence) => {
                    (ClaimOutcome::Released, expired)
                }
                _ => (ClaimOutcome::FenceMismatch, expired),
            }
        })?;
        Ok(outcome.unwrap_or(ClaimOutcome::NotFound))
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
    /// densely around it; the old cell is renumbered when it differs. Every
    /// read and write happens in one write transaction (KF-421).
    pub fn move_task(
        &self,
        id: &str,
        column_id: &str,
        swimlane_id: Option<&str>,
        position: f64,
    ) -> DbResult<()> {
        match self.move_task_logged(id, column_id, swimlane_id, position, MoveGuards::default())? {
            MoveOutcome::NotFound => Err(format!("task not found: {id}").into()),
            _ => Ok(()),
        }
    }

    /// [`Db::move_task`] with the checks in `guards`. Every read the move
    /// depends on, the checks, the move, both cells' renumbering, the field
    /// carry, the readiness bump, and the history event share one write
    /// transaction (KF-421).
    pub fn move_task_logged(
        &self,
        id: &str,
        column_id: &str,
        swimlane_id: Option<&str>,
        position: f64,
        guards: MoveGuards,
    ) -> DbResult<MoveOutcome> {
        let txn = self.db.begin_write()?;
        let Some(task) = txn_read_one::<TaskRow>(&txn, TASKS, id)? else {
            return Ok(MoveOutcome::NotFound);
        };
        let target: ColumnRow = txn_read_one(&txn, COLUMNS, column_id)?
            .ok_or_else(|| format!("unknown column: {column_id}"))?;
        if !task.fence_allows(guards.fence) {
            return Ok(MoveOutcome::StaleFence);
        }
        let old_column_id = task.column_id.clone();
        let old_swimlane_id = task.swimlane_id.clone();
        // A move within its own column never adds to that column.
        if let (true, Some(limit), true) = (
            guards.enforce_wip,
            target.wip_limit,
            old_column_id != column_id,
        ) {
            let count = txn_mmap_get(&txn, TASKS_BY_COLUMN, column_id)?
                .iter()
                .filter(|task_id| task_id.as_str() != id)
                .count();
            if count as i64 >= limit {
                return Ok(MoveOutcome::WipLimit { count, limit });
            }
        }

        let old_column: Option<ColumnRow> = txn_read_one(&txn, COLUMNS, &old_column_id)?;
        let was_done = old_column
            .as_ref()
            .map(|column| column.is_done)
            .unwrap_or(false);
        let completed_at = if target.is_done && !was_done {
            Some(Utc::now().to_rfc3339())
        } else if !target.is_done && was_done {
            None
        } else {
            task.completed_at.clone()
        };

        // Keep the current swimlane when the caller doesn't name one. Lanes
        // are per-board, so a cross-board move lands in the target board's
        // first lane instead (KF-334).
        let crosses_boards = old_column
            .as_ref()
            .is_some_and(|column| column.board_id != target.board_id);
        let new_swimlane_id = match swimlane_id {
            Some(lane) => Some(lane.to_string()),
            None if crosses_boards => {
                let mut lanes: Vec<SwimlaneRow> =
                    txn_indexed_rows(&txn, SWIMLANES_BY_BOARD, &target.board_id, SWIMLANES)?;
                lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
                lanes.into_iter().next().map(|lane| lane.id)
            }
            None => old_swimlane_id.clone(),
        };
        let cell_changed =
            column_id != old_column_id || new_swimlane_id.as_deref() != old_swimlane_id.as_deref();
        let before = blocked_states(&txn, &blocked_dependents(&txn, &[id.to_string()])?)?;
        // Custom field values only travel to a board that defines the same
        // key and accepts the value.
        let target_fields = if crosses_boards {
            board_fields_in(&txn, &target.board_id)?
        } else {
            Vec::new()
        };

        // Explicit index insertion (KF-120): the target cell's other tasks
        // keep their relative order and the moved task lands exactly at the
        // client-sent index.
        let mut siblings = txn_cell_tasks(&txn, column_id, new_swimlane_id.as_deref())?;
        siblings.retain(|sibling| sibling.id != id);
        let at = (position.round() as i64).clamp(0, siblings.len() as i64) as usize;
        let mut moved = TaskRow {
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
            updated_at: Some(Utc::now().to_rfc3339()),
            ..task
        };
        let dropped = if crosses_boards {
            carry_fields(&mut moved.fields, &target_fields)
        } else {
            serde_json::Map::new()
        };
        let dropped_note = if dropped.is_empty() {
            String::new()
        } else {
            format!("; dropped fields: {}", serde_json::Value::Object(dropped))
        };
        match guards.event {
            Some((detail, actor)) => {
                let detail = format!("{detail}{dropped_note}");
                push_column_event(&mut moved, "moved", detail, actor, Some(column_id));
            }
            None if !dropped_note.is_empty() => {
                let detail = format!("Moved to another board{dropped_note}");
                push_column_event(&mut moved, "moved", detail, "system", Some(column_id));
            }
            None => {}
        }

        if column_id != old_column_id {
            mmap_remove(&txn, TASKS_BY_COLUMN, &old_column_id, id)?;
            mmap_insert(&txn, TASKS_BY_COLUMN, column_id, id)?;
        }
        {
            let mut tbl = txn.open_table(TASKS)?;
            let bytes = serde_json::to_vec(&moved)?;
            tbl.insert(id, bytes.as_slice())?;
            for (index, mut sibling) in siblings.into_iter().enumerate() {
                let dense = (if index >= at { index + 1 } else { index }) as f64;
                if sibling.position == dense {
                    continue;
                }
                sibling.position = dense;
                sibling.updated_at = Some(Utc::now().to_rfc3339());
                let bytes = serde_json::to_vec(&sibling)?;
                tbl.insert(sibling.id.as_str(), bytes.as_slice())?;
            }
        }
        if cell_changed {
            txn_renumber_cell(&txn, &old_column_id, old_swimlane_id.as_deref())?;
        }
        bump_flipped(&txn, &before)?;
        txn.commit()?;
        Ok(MoveOutcome::Moved)
    }

    /// Rewrite positions in one column x swimlane cell as dense 0..n ordering.
    pub fn renumber_cell(&self, column_id: &str, swimlane_id: Option<&str>) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_renumber_cell(&txn, column_id, swimlane_id)?;
        txn.commit()?;
        Ok(())
    }

    /// Delete a task, its time entries, and every link to or from it, and
    /// stop the timer when it runs on this task. Returns false when unknown.
    pub fn delete_task(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(task) = txn_read_one::<TaskRow>(&txn, TASKS, id)? else {
            return Ok(false);
        };
        let entry_ids = txn_mmap_get(&txn, ENTRIES_BY_TASK, id)?;
        unlink_deleted_tasks(&txn, &[id.to_string()])?;
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
        txn_clear_timer_if(&txn, |task_id| task_id == id)?;
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
                &Utc::now().to_rfc3339(),
                minutes,
                minutes * 60,
                note,
                "manual",
                false,
                None,
                &[],
                "",
                None,
            )?
            .unwrap_or_default())
    }

    /// Log manual minutes on a task and its `time_logged` history event in
    /// one write. Returns false, writing nothing, when the task does not
    /// exist.
    pub fn log_task_time(
        &self,
        task_id: &str,
        minutes: i64,
        note: &str,
        actor: &str,
    ) -> DbResult<bool> {
        let entry = TimeEntryRow {
            id: Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            minutes,
            seconds: minutes * 60,
            note: note.to_string(),
            started_at: Utc::now().to_rfc3339(),
            kind: "manual".to_string(),
            interrupted: false,
            interrupt_reason: None,
            labels: Vec::new(),
            created_by: String::new(),
            activity_id: None,
        };
        let txn = self.db.begin_write()?;
        {
            let mut tasks = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tasks, task_id)? else {
                return Ok(false);
            };
            push_event(
                &mut task,
                "time_logged",
                format!("Logged {minutes}m"),
                actor,
            );
            task.updated_at = Some(Utc::now().to_rfc3339());
            let raw = serde_json::to_vec(&task)?;
            tasks.insert(task_id, raw.as_slice())?;
        }
        write_one(&txn, TIME_ENTRIES, &entry.id, &entry)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &entry.id)?;
        txn.commit()?;
        Ok(true)
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
        mutate_task(&self.db, task_id, |task| {
            task.pomodori_completed += 1;
        })?;
        Ok(())
    }

    /// Bump a task's interruption counter. No-op when unknown.
    pub fn record_interruption(&self, task_id: &str) -> DbResult<()> {
        mutate_task(&self.db, task_id, |task| {
            task.interruptions += 1;
        })?;
        Ok(())
    }

    /// Create a time entry with full timer metadata, starting at the RFC3339
    /// `started_at`. A `task_id` of `None` writes a task-less entry (an
    /// empty `task_id`); returns `None` and writes nothing when the named
    /// task is gone.
    #[allow(clippy::too_many_arguments)]
    pub fn create_entry_full(
        &self,
        task_id: Option<&str>,
        started_at: &str,
        minutes: i64,
        seconds: i64,
        note: &str,
        kind: &str,
        interrupted: bool,
        interrupt_reason: Option<&str>,
        labels: &[String],
        created_by: &str,
        // KF-347: break-activity id attributed to the session (breaks only).
        activity_id: Option<&str>,
    ) -> DbResult<Option<String>> {
        let task_id = match task_id {
            Some(id) if self.get_task(id)?.is_some() => id,
            Some(_) => return Ok(None),
            None => "",
        };
        let id = Uuid::new_v4().to_string();
        let row = TimeEntryRow {
            id: id.clone(),
            task_id: task_id.to_string(),
            minutes,
            seconds,
            note: note.to_string(),
            started_at: started_at.to_string(),
            kind: kind.to_string(),
            interrupted,
            interrupt_reason: interrupt_reason.map(str::to_string),
            labels: labels.to_vec(),
            created_by: created_by.to_string(),
            activity_id: activity_id.map(str::to_string),
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TIME_ENTRIES, &id, &row)?;
        if !task_id.is_empty() {
            mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        }
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
            activity_id: None,
        };
        let txn = self.db.begin_write()?;
        write_one(&txn, TIME_ENTRIES, &id, &row)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        txn.commit()?;
        Ok(Some(id))
    }

    /// Update a time entry (date/time, task reassignment, note, labels);
    /// `seconds` is the new length. Returns false when the entry does not
    /// exist; the task must exist (checked by the caller).
    pub fn update_entry(
        &self,
        entry_id: &str,
        task_id: &str,
        seconds: i64,
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
        row.seconds = seconds;
        row.minutes = seconds / 60;
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
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TIME_ENTRIES)?;
        let mut out = Vec::new();
        for item in tbl.iter()? {
            let (_, value) = item?;
            let entry: TimeEntryRow = serde_json::from_slice(value.value())?;
            if local_day(&entry.started_at).as_deref() == Some(today.as_str()) {
                let name = match self.get_task(&entry.task_id)? {
                    Some(task) => task.name,
                    None if entry.task_id.is_empty() => NO_TASK_NAME.to_string(),
                    None => "(deleted task)".to_string(),
                };
                out.push((entry, name));
            }
        }
        // KF-279: newest start first. Timer rows are stored in UTC and edited
        // rows with a local offset, so compare instants, not strings.
        let instant = |entry: &TimeEntryRow| {
            chrono::DateTime::parse_from_rfc3339(&entry.started_at)
                .map(|dt| dt.timestamp())
                .unwrap_or(0)
        };
        out.sort_by(|a, b| {
            instant(&b.0)
                .cmp(&instant(&a.0))
                .then_with(|| b.0.id.cmp(&a.0.id))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod color_hex_tests {
    //! KF-245: stored standard colors read back with the shipped palette.

    use super::*;

    #[test]
    fn get_color_should_replace_stale_stored_hex_with_the_palette() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let board_id = db.list_boards().expect("boards")[0].id.clone();
        let mut blue = db
            .list_colors(&board_id)
            .expect("colors")
            .into_iter()
            .find(|c| c.value == "blue")
            .expect("blue");
        blue.border_hex = "#7faef9".to_string();
        let txn = db.db.begin_write().expect("write txn");
        write_one(&txn, TASK_COLORS, &blue.id, &blue).expect("write stale row");
        txn.commit().expect("commit");

        let read = db.get_color(&blue.id).expect("read").expect("row");
        assert_eq!(read.border_hex, "#70b0ff");
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
mod feed_token_tests {
    //! KF-351: the calendar-feed bearer token round-trips through the
    //! board's opaque config bag, and lookup by token resolves the board.

    use super::*;

    fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        (dir, db)
    }

    fn set_feed_token(db: &Db, board_id: &str, token: &str) {
        let mut updates = serde_json::Map::new();
        updates.insert(
            "calendar_feed_token".to_string(),
            serde_json::Value::String(token.to_string()),
        );
        db.set_board_config(board_id, &updates).expect("set config");
    }

    #[test]
    fn feed_token_round_trips_and_resolves() {
        let (_dir, db) = test_db();
        let board_id = db.create_board("Work").expect("create board");
        // No token until one is stored.
        assert!(db
            .get_board(&board_id)
            .expect("get board")
            .expect("board exists")
            .feed_token()
            .is_none());
        assert!(db
            .get_board_by_feed_token("nope")
            .expect("lookup")
            .is_none());

        set_feed_token(&db, &board_id, "tok123");
        let board = db
            .get_board(&board_id)
            .expect("get board")
            .expect("board exists");
        assert_eq!(board.feed_token().as_deref(), Some("tok123"));

        let found = db
            .get_board_by_feed_token("tok123")
            .expect("lookup")
            .expect("board resolves by token");
        assert_eq!(found.id, board_id);

        // Rotating the token invalidates the old one.
        set_feed_token(&db, &board_id, "tok456");
        assert!(db
            .get_board_by_feed_token("tok123")
            .expect("lookup")
            .is_none());
        let found = db
            .get_board_by_feed_token("tok456")
            .expect("lookup")
            .expect("new token resolves");
        assert_eq!(found.id, board_id);
    }
}

#[cfg(test)]
mod pomodori_restore_tests {
    //! KF-296: the one-time backfill must restore KanbanFlow's "Pomodori" in
    //! labels an earlier release rewrote to "Pomodoros", run exactly once,
    //! and never touch user-customized labels.

    use super::*;

    fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        (dir, db)
    }

    /// Drop the one-time marker to simulate a pre-upgrade startup state.
    fn clear_restore_marker(db: &Db) {
        let txn = db.db.begin_write().expect("write txn");
        txn.open_table(SETTINGS)
            .expect("settings table")
            .remove("pomodori_restore_done")
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
    fn restore_renames_pomodoros_labels_exactly_once() {
        let (_dir, db) = test_db();
        let board = db.create_board("Legacy").expect("create board");
        let colors = db.list_colors(&board).expect("list colors");
        // A board the earlier rename touched, plus a label of the user's own.
        let ids: Vec<String> = colors.iter().take(5).map(|c| c.id.clone()).collect();
        db.update_color(&ids[0], Some("1 Pomodoro"), None, None, None)
            .expect("singular");
        db.update_color(&ids[1], Some("2 Pomodoros"), None, None, None)
            .expect("two");
        db.update_color(&ids[2], Some("3 Pomodoros"), None, None, None)
            .expect("three");
        db.update_color(&ids[3], Some(">3 Pomodoros"), None, None, None)
            .expect("many");
        db.update_color(&ids[4], Some("Pomodoros galore"), None, None, None)
            .expect("custom");

        clear_restore_marker(&db);
        db.restore_pomodori_labels().expect("restore");

        assert_eq!(label_of(&db, &ids[0]), "1 Pomodoro", "singular unchanged");
        assert_eq!(label_of(&db, &ids[1]), "2 Pomodori");
        assert_eq!(label_of(&db, &ids[2]), "3 Pomodori");
        assert_eq!(label_of(&db, &ids[3]), ">3 Pomodori");
        assert_eq!(
            label_of(&db, &ids[4]),
            "Pomodoros galore",
            "user-customized label preserved"
        );

        // Re-running must be a no-op (marker short-circuits the pass).
        db.update_color(&ids[1], Some("2 Pomodoros"), None, None, None)
            .expect("re-set");
        db.restore_pomodori_labels().expect("second restore");
        assert_eq!(
            label_of(&db, &ids[1]),
            "2 Pomodoros",
            "marker prevents a second run"
        );
    }

    #[test]
    fn starter_board_keeps_pomodori_across_restarts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let board = {
            let db = Db::connect(path).expect("connect");
            db.list_boards().expect("boards")[0].id.clone()
        };
        let db = Db::connect(path).expect("reconnect");
        let labels: Vec<String> = db
            .list_colors(&board)
            .expect("colors")
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(labels.contains(&"2 Pomodori".to_string()), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l.contains("Pomodoros")),
            "{labels:?}"
        );
    }
}

#[cfg(test)]
mod builtin_template_refresh_tests {
    //! KF-372: a database that stored an older built-in template snapshot
    //! picks up the shipped one on the next start.

    use super::*;

    #[test]
    fn stale_builtin_snapshot_is_rewritten_on_connect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let template_id = {
            let db = Db::connect(path).expect("connect");
            let mut row = db
                .list_templates()
                .expect("templates")
                .into_iter()
                .find(|t| t.built_in && t.name == Db::BUILTIN_TEMPLATE_NAME)
                .expect("built-in template");
            row.snapshot = serde_json::json!({"colors": [], "columns": [], "swimlanes": []});
            let txn = db.db.begin_write().expect("write txn");
            write_one(&txn, TEMPLATES, &row.id, &row).expect("write stale row");
            txn.commit().expect("commit");
            row.id
        };
        let db = Db::connect(path).expect("reconnect");
        let row = db
            .list_templates()
            .expect("templates")
            .into_iter()
            .find(|t| t.id == template_id)
            .expect("same template id kept");
        assert_eq!(row.snapshot, pomodoro_template_snapshot());
    }
}

#[cfg(test)]
mod schema_version_tests {
    //! The `meta` stamp: fresh and older files are stamped with
    //! [`SCHEMA_VERSION`], a newer file is refused without application
    //! writes (byte-identical when it was closed cleanly).

    use redb::TableHandle;
    use sha2::{Digest, Sha256};

    use super::*;

    fn temp_path() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path").to_string();
        (dir, path)
    }

    fn file_hash(path: &str) -> Vec<u8> {
        Sha256::digest(std::fs::read(path).expect("read db file")).to_vec()
    }

    fn stamp(path: &str, version: u64) {
        let db = Database::create(path).expect("raw open");
        let txn = db.begin_write().expect("write txn");
        txn.open_table(META)
            .expect("meta table")
            .insert(SCHEMA_VERSION_KEY, version)
            .expect("stamp");
        txn.commit().expect("commit");
    }

    fn stamp_of(path: &str) -> Option<u64> {
        let db = redb::ReadOnlyDatabase::open(path).expect("read-only open");
        read_schema_version(&db).expect("read stamp")
    }

    #[test]
    fn fresh_db_is_stamped_with_current_version() {
        let (_dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    #[test]
    fn unstamped_db_opens_and_is_stamped() {
        let (_dir, path) = temp_path();
        let boards = {
            let db = Db::connect(&path).expect("connect");
            let txn = db.db.begin_write().expect("write txn");
            txn.delete_table(META).expect("drop meta");
            txn.commit().expect("commit");
            db.list_boards().expect("boards").len()
        };
        assert_eq!(stamp_of(&path), None, "file predates the meta table");

        let db = Db::connect(&path).expect("open unstamped file");
        assert_eq!(db.list_boards().expect("boards").len(), boards);
        drop(db);
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    #[test]
    fn older_stamp_is_raised_to_current_version() {
        let (_dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        stamp(&path, 0);
        drop(Db::connect(&path).expect("open older file"));
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    /// A file written at version 1 opens, is stamped 2, and keeps its rows,
    /// which read with the version-2 fields at their defaults.
    #[test]
    fn version_1_file_opens_stamped_2_with_its_rows() {
        let (_dir, path) = temp_path();
        let (task_id, comment_id) = {
            let db = Db::connect(&path).expect("connect");
            let board = db.create_board("Old").expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let task = db
                .create_task(&column, None, "Kept", 1, None)
                .expect("task");
            let comment = NewComment {
                author: "audit",
                body: "hi",
                actor_kind: ActorKind::User,
                token_name: None,
                kind: None,
            };
            let FencedWrite::Written(comment) = db
                .add_comment(&task, comment, "audit", None)
                .expect("comment")
            else {
                panic!("comment not written");
            };
            (task, comment.id)
        };
        // Strip the version-2 fields from the stored row, as version 1
        // wrote it, then stamp the file 1.
        {
            let db = Database::open(&path).expect("open");
            let txn = db.begin_write().expect("write");
            {
                let mut tasks = txn.open_table(TASKS).expect("tasks");
                let raw = tasks
                    .get(task_id.as_str())
                    .expect("get")
                    .expect("row")
                    .value()
                    .to_vec();
                let mut row: serde_json::Value = serde_json::from_slice(&raw).expect("json");
                let object = row.as_object_mut().expect("object");
                for field in [
                    "updated_at",
                    "metadata",
                    "links",
                    "claim",
                    "claim_fence",
                    "fields",
                ] {
                    object.remove(field);
                }
                for comment in object["comments"].as_array_mut().expect("comments") {
                    let comment = comment.as_object_mut().expect("comment");
                    for field in ["actor_kind", "token_name", "kind"] {
                        comment.remove(field);
                    }
                }
                let raw = serde_json::to_vec(&row).expect("bytes");
                tasks
                    .insert(task_id.as_str(), raw.as_slice())
                    .expect("insert");
            }
            txn.commit().expect("commit");
        }
        stamp(&path, 1);
        drop(Db::connect(&path).expect("open version-1 file"));
        assert_eq!(stamp_of(&path), Some(2));
        let db = Db::connect(&path).expect("reopen");
        let task = db.get_task(&task_id).expect("read").expect("task kept");
        assert_eq!(task.name, "Kept");
        assert_eq!(task.updated_at, None);
        assert_eq!(task.metadata, None);
        assert!(task.links.is_empty() && task.fields.is_empty() && task.claim.is_none());
        assert_eq!(task.comments[0].id, comment_id);
        assert_eq!(task.comments[0].actor_kind, ActorKind::User);
        assert!(db
            .list_fields("any")
            .expect("fields table exists")
            .is_empty());
    }

    #[test]
    fn newer_db_is_refused_and_left_byte_identical() {
        let (_dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        let newer = SCHEMA_VERSION + 1;
        stamp(&path, newer);
        let before = file_hash(&path);

        let err = match Db::connect(&path) {
            Ok(_) => panic!("a newer file must be refused"),
            Err(err) => err,
        };
        let refusal = err.downcast_ref::<SchemaTooNew>().expect("typed refusal");
        assert_eq!((refusal.found, refusal.supported), (newer, SCHEMA_VERSION));
        let text = err.to_string();
        for needle in [
            path.as_str(),
            &format!("schema version {newer}"),
            &format!("supports up to schema version {SCHEMA_VERSION}"),
            "Restore a backup",
            "upgrade chipflow",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from {text:?}");
        }
        assert_eq!(file_hash(&path), before, "refusal must not touch the file");
    }

    /// Copy `path` while a writer holds it open: the file a killed server
    /// leaves behind, which needs redb's crash repair.
    fn crash_left_copy(path: &str, copy: &str, version: u64) {
        let db = Database::create(path).expect("raw open");
        let txn = db.begin_write().expect("write txn");
        txn.open_table(META)
            .expect("meta table")
            .insert(SCHEMA_VERSION_KEY, version)
            .expect("stamp");
        txn.commit().expect("commit");
        std::fs::copy(path, copy).expect("copy open file");
        drop(db);
        assert!(matches!(
            redb::ReadOnlyDatabase::open(copy),
            Err(redb::DatabaseError::RepairAborted)
        ));
    }

    /// Every JSON row, keyed by table name and id.
    fn app_rows(path: &str) -> Vec<(String, String, Vec<u8>)> {
        let db = redb::ReadOnlyDatabase::open(path).expect("read-only open");
        let txn = db.begin_read().expect("read txn");
        let mut rows = Vec::new();
        for table in [
            USERS,
            SESSIONS,
            API_TOKENS,
            BOARDS,
            COLUMNS,
            SWIMLANES,
            TASKS,
            TIME_ENTRIES,
            SETTINGS,
            ACTIVE_TIMER,
            TASK_COLORS,
            TEMPLATES,
            ATTACHMENT_DATA,
        ] {
            let tbl = txn.open_table(table).expect("table");
            for item in tbl.iter().expect("iter") {
                let (key, value) = item.expect("row");
                rows.push((
                    table.name().to_string(),
                    key.value().to_string(),
                    value.value().to_vec(),
                ));
            }
        }
        rows
    }

    #[test]
    fn crash_left_newer_db_is_refused_without_application_writes() {
        let (dir, path) = temp_path();
        let db = Db::connect(&path).expect("connect");
        db.create_board("Kept").expect("create board");
        drop(db);
        let killed = dir.path().join("killed.redb");
        let killed = killed.to_str().expect("utf8 path").to_string();
        let newer = SCHEMA_VERSION + 1;
        crash_left_copy(&path, &killed, newer);

        let err = match Db::connect(&killed) {
            Ok(_) => panic!("a newer file must be refused"),
            Err(err) => err,
        };
        let refusal = err.downcast_ref::<SchemaTooNew>().expect("typed refusal");
        assert!(refusal.repaired);
        let text = err.to_string();
        for needle in [
            killed.as_str(),
            &format!("schema version {newer}"),
            "not closed cleanly",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from {text:?}");
        }
        assert_eq!(stamp_of(&killed), Some(newer));
        assert_eq!(app_rows(&killed), app_rows(&path), "rows must be untouched");
    }

    #[test]
    fn crash_left_current_db_is_repaired_and_opens() {
        let (dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        let killed = dir.path().join("killed.redb");
        let killed = killed.to_str().expect("utf8 path").to_string();
        crash_left_copy(&path, &killed, SCHEMA_VERSION);

        let db = Db::connect(&killed).expect("open crash-left file");
        assert_eq!(db.list_boards().expect("boards").len(), 1);
    }

    #[test]
    fn failed_crash_repair_is_reported_as_such() {
        let (dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        let killed = dir.path().join("killed.redb");
        let killed = killed.to_str().expect("utf8 path").to_string();
        crash_left_copy(&path, &killed, SCHEMA_VERSION);
        let mut perms = std::fs::metadata(&killed).expect("metadata").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&killed, perms).expect("make read-only");

        let err = match Db::connect(&killed) {
            Ok(_) => panic!("a read-only file cannot be repaired"),
            Err(err) => err,
        };
        let text = err.to_string();
        for needle in [
            killed.as_str(),
            "not closed cleanly",
            "crash repair, which failed",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from {text:?}");
        }
    }

    #[test]
    fn open_errors_name_the_database_path() {
        let (dir, _) = temp_path();
        let path = dir.path().to_str().expect("utf8 path").to_string();
        let err = match Db::connect(&path) {
            Ok(_) => panic!("a directory is not a database"),
            Err(err) => err,
        };
        assert!(err.to_string().contains(&path), "{err}");
    }

    #[test]
    fn equal_version_check_writes_nothing() {
        let (dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        let control = dir.path().join("control.redb");
        let control = control.to_str().expect("utf8 path").to_string();
        std::fs::copy(&path, &control).expect("copy db");

        // A writable open/close cycle rewrites redb's header on its own, so
        // compare against a control file that goes through the same cycle.
        drop(Database::create(&control).expect("raw open control"));
        let db = Database::create(&path).expect("raw open");
        ensure_schema_version(&db, &path, false).expect("equal version");
        drop(db);
        assert_eq!(
            file_hash(&path),
            file_hash(&control),
            "equal stamp must not be rewritten"
        );
    }
}

#[cfg(test)]
mod schema_fingerprint_tests {
    //! Enforces the bump rule: any change to a stored type's fields, field
    //! types or enum variants, or to a redb table's name, key or value type,
    //! must come with a new [`SCHEMA_VERSION`] and a new
    //! [`SCHEMA_FINGERPRINTS`] entry.

    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use redb::{Key, MultimapTableHandle, TableHandle, Value};
    use serde::de::value::{Error as ShapeError, StrDeserializer};
    use serde::de::{
        DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Visitor,
    };
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::models::{BreakActivity, TimerMode};

    /// Deserializer that feeds placeholder values to a derived `Deserialize`
    /// impl and records the shape it asks for: field names in sorted order,
    /// primitive kinds, options, sequences, nested structs and enum variants.
    #[derive(Clone, Copy)]
    struct Shape<'a>(&'a RefCell<String>);

    impl Shape<'_> {
        fn push(&self, text: &str) {
            self.0.borrow_mut().push_str(text);
        }
    }

    macro_rules! primitives {
        ($($method:ident => $kind:literal, $visit:ident($value:expr);)*) => {
            $(fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ShapeError> {
                self.push($kind);
                visitor.$visit($value)
            })*
        };
    }

    impl<'de> Deserializer<'de> for Shape<'_> {
        type Error = ShapeError;

        primitives! {
            deserialize_bool => "bool", visit_bool(false);
            deserialize_i8 => "i8", visit_i8(0);
            deserialize_i16 => "i16", visit_i16(0);
            deserialize_i32 => "i32", visit_i32(0);
            deserialize_i64 => "i64", visit_i64(0);
            deserialize_i128 => "i128", visit_i128(0);
            deserialize_u8 => "u8", visit_u8(0);
            deserialize_u16 => "u16", visit_u16(0);
            deserialize_u32 => "u32", visit_u32(0);
            deserialize_u64 => "u64", visit_u64(0);
            deserialize_u128 => "u128", visit_u128(0);
            deserialize_f32 => "f32", visit_f32(0.0);
            deserialize_f64 => "f64", visit_f64(0.0);
            deserialize_char => "char", visit_char('a');
            deserialize_str => "str", visit_str("");
            deserialize_string => "string", visit_string(String::new());
            deserialize_bytes => "bytes", visit_bytes(&[]);
            deserialize_byte_buf => "byte_buf", visit_byte_buf(Vec::new());
            deserialize_identifier => "identifier", visit_str("");
        }

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ShapeError> {
            self.push("any");
            visitor.visit_unit()
        }

        fn deserialize_ignored_any<V: Visitor<'de>>(
            self,
            visitor: V,
        ) -> Result<V::Value, ShapeError> {
            self.push("ignored");
            visitor.visit_unit()
        }

        fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ShapeError> {
            self.push("unit");
            visitor.visit_unit()
        }

        fn deserialize_unit_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            visitor: V,
        ) -> Result<V::Value, ShapeError> {
            self.push(name);
            visitor.visit_unit()
        }

        fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ShapeError> {
            self.push("option<");
            let value = visitor.visit_some(self)?;
            self.push(">");
            Ok(value)
        }

        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            visitor: V,
        ) -> Result<V::Value, ShapeError> {
            self.push(name);
            self.push("(");
            let value = visitor.visit_newtype_struct(self)?;
            self.push(")");
            Ok(value)
        }

        fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ShapeError> {
            self.push("seq<");
            let value = visitor.visit_seq(Elements {
                shape: self,
                left: 1,
            })?;
            self.push(">");
            Ok(value)
        }

        fn deserialize_tuple<V: Visitor<'de>>(
            self,
            len: usize,
            visitor: V,
        ) -> Result<V::Value, ShapeError> {
            self.push("tuple<");
            let value = visitor.visit_seq(Elements {
                shape: self,
                left: len,
            })?;
            self.push(">");
            Ok(value)
        }

        fn deserialize_tuple_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            len: usize,
            visitor: V,
        ) -> Result<V::Value, ShapeError> {
            self.push(name);
            self.deserialize_tuple(len, visitor)
        }

        fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ShapeError> {
            self.push("map<");
            let value = visitor.visit_map(OneEntry {
                shape: self,
                done: false,
            })?;
            self.push(">");
            Ok(value)
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, ShapeError> {
            let mut fields = fields.to_vec();
            fields.sort_unstable();
            self.push("{");
            let value = visitor.visit_map(Fields {
                shape: self,
                fields,
                next: 0,
            })?;
            self.push("}");
            Ok(value)
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, ShapeError> {
            let mut sorted = variants.to_vec();
            sorted.sort_unstable();
            self.push(&format!("enum({})", sorted.join("|")));
            let first: StrDeserializer<'_, ShapeError> = variants[0].into_deserializer();
            visitor.visit_enum(first)
        }
    }

    /// `left` placeholder elements, one shape each.
    struct Elements<'a> {
        shape: Shape<'a>,
        left: usize,
    }

    impl<'de> SeqAccess<'de> for Elements<'_> {
        type Error = ShapeError;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>, ShapeError> {
            if self.left == 0 {
                return Ok(None);
            }
            self.left -= 1;
            let value = seed.deserialize(self.shape)?;
            if self.left > 0 {
                self.shape.push(",");
            }
            Ok(Some(value))
        }
    }

    /// One placeholder map entry.
    struct OneEntry<'a> {
        shape: Shape<'a>,
        done: bool,
    }

    impl<'de> MapAccess<'de> for OneEntry<'_> {
        type Error = ShapeError;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, ShapeError> {
            if self.done {
                return Ok(None);
            }
            self.done = true;
            seed.deserialize(self.shape).map(Some)
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(
            &mut self,
            seed: V,
        ) -> Result<V::Value, ShapeError> {
            self.shape.push("=>");
            seed.deserialize(self.shape)
        }
    }

    /// Every field of a struct, in sorted order.
    struct Fields<'a> {
        shape: Shape<'a>,
        fields: Vec<&'static str>,
        next: usize,
    }

    impl<'de> MapAccess<'de> for Fields<'_> {
        type Error = ShapeError;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, ShapeError> {
            let Some(field) = self.fields.get(self.next) else {
                return Ok(None);
            };
            self.shape.push(field);
            self.shape.push(":");
            let key: StrDeserializer<'_, ShapeError> = field.into_deserializer();
            seed.deserialize(key).map(Some)
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(
            &mut self,
            seed: V,
        ) -> Result<V::Value, ShapeError> {
            let value = seed.deserialize(self.shape)?;
            self.next += 1;
            self.shape.push(",");
            Ok(value)
        }
    }

    fn shape<T: DeserializeOwned>(type_name: &str) -> String {
        let out = RefCell::new(String::new());
        if let Err(err) = T::deserialize(Shape(&out)) {
            panic!("{type_name}: cannot record its shape: {err}");
        }
        format!("{type_name}: {}", out.into_inner())
    }

    fn table<K: Key + 'static, V: Value + 'static>(def: TableDefinition<K, V>) -> (String, String) {
        let name = def.name().to_string();
        let line = format!(
            "table {name}: {} -> {}",
            K::type_name().name(),
            V::type_name().name()
        );
        (name, line)
    }

    fn multimap<K: Key + 'static, V: Key + 'static>(
        def: MultimapTableDefinition<K, V>,
    ) -> (String, String) {
        let name = def.name().to_string();
        let line = format!(
            "multimap {name}: {} -> {}",
            K::type_name().name(),
            V::type_name().name()
        );
        (name, line)
    }

    /// One line per stored type and per table. A new stored type must be
    /// added to the type list here; a table missing from the table list
    /// fails the test.
    fn stored_schema_listing() -> String {
        let mut lines = vec![
            shape::<ActiveTimer>("ActiveTimer"),
            shape::<ApiTokenRecord>("ApiTokenRecord"),
            shape::<BoardRow>("BoardRow"),
            shape::<BoardTemplateRow>("BoardTemplateRow"),
            shape::<BreakActivity>("BreakActivity"),
            shape::<ColorRow>("ColorRow"),
            shape::<ColumnRow>("ColumnRow"),
            shape::<DeviceAuthRow>("DeviceAuthRow"),
            shape::<FieldDefRow>("FieldDefRow"),
            shape::<SessionRow>("SessionRow"),
            shape::<Settings>("Settings"),
            shape::<Subtask>("Subtask"),
            shape::<SwimlaneRow>("SwimlaneRow"),
            shape::<TaskAttachment>("TaskAttachment"),
            shape::<TaskComment>("TaskComment"),
            shape::<TaskEvent>("TaskEvent"),
            shape::<TaskRow>("TaskRow"),
            shape::<TimeEntryRow>("TimeEntryRow"),
            shape::<TimerMode>("TimerMode"),
            shape::<UserRow>("UserRow"),
        ];

        let mut tables = vec![
            table(USERS),
            table(USERNAMES),
            table(SESSIONS),
            table(BOARDS),
            table(COLUMNS),
            table(SWIMLANES),
            table(TASKS),
            table(TIME_ENTRIES),
            table(SETTINGS),
            table(ACTIVE_TIMER),
            table(API_TOKENS),
            table(TASK_COLORS),
            table(TEMPLATES),
            table(ATTACHMENT_DATA),
            table(META),
            table(FIELD_DEFS),
            table(DEVICE_AUTHS),
            multimap(COLUMNS_BY_BOARD),
            multimap(SWIMLANES_BY_BOARD),
            multimap(TASKS_BY_COLUMN),
            multimap(ENTRIES_BY_TASK),
            multimap(COLORS_BY_BOARD),
            multimap(FIELDS_BY_BOARD),
            multimap(LINKS_BY_TARGET),
        ];
        tables.sort_unstable();

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let txn = db.db.begin_read().expect("read txn");
        let created: BTreeSet<String> = txn
            .list_tables()
            .expect("list tables")
            .map(|t| t.name().to_string())
            .chain(
                txn.list_multimap_tables()
                    .expect("list multimap tables")
                    .map(|t| t.name().to_string()),
            )
            .collect();
        let listed: BTreeSet<String> = tables.iter().map(|(name, _)| name.clone()).collect();
        assert_eq!(
            listed, created,
            "every table Db::connect creates must be in the fingerprint table list"
        );

        lines.extend(tables.into_iter().map(|(_, line)| line));
        lines.join("\n")
    }

    #[test]
    fn stored_schema_matches_fingerprint() {
        let listing = stored_schema_listing();
        let actual = format!("{:x}", Sha256::digest(listing.as_bytes()));
        let recorded = SCHEMA_FINGERPRINTS[SCHEMA_VERSION as usize - 1];
        assert!(
            recorded == actual,
            "The stored schema changed: a stored field, field type, enum variant or redb \
             table was added, removed, renamed or retyped.\n\
             Bump SCHEMA_VERSION in src/db.rs and append \"{actual}\" to \
             SCHEMA_FINGERPRINTS, so older binaries refuse the newer file instead of \
             dropping its new data on their next save.\n\
             Recorded fingerprint for schema version {SCHEMA_VERSION}: {recorded}\n\
             Current stored schema:\n{listing}"
        );
    }
}

#[cfg(test)]
mod color_backfill_tests {
    //! KF-420: boards that predate colors get the standard palette at
    //! startup; listing colors never writes.

    use super::*;

    #[test]
    fn colorless_boards_should_get_colors_at_startup_not_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let board = {
            let db = Db::connect(path).expect("connect");
            let board = db.create_board_row("Legacy").expect("board");
            let before = std::fs::read(path).expect("read db file");
            assert!(db.list_colors(&board).expect("list").is_empty());
            assert!(
                std::fs::read(path).expect("read db file") == before,
                "list_colors wrote to the database"
            );
            board
        };
        let db = Db::connect(path).expect("reconnect");
        assert_eq!(
            db.list_colors(&board).expect("list").len(),
            standard_color_specs().len()
        );
    }
}
