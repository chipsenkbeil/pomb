//! redb-backed storage: table definitions, data-access methods, first-run seeding.
//!
//! Pure Rust, no C bindings: the whole database is a single `pomb.redb` file.
//! Records are JSON values keyed by id; parent -> child lookups use multimap
//! index tables maintained inside the same write transaction, so they stay
//! consistent.
//!
//! redb allows only one write transaction at a time (`begin_write` blocks while
//! another is open), so every method below opens its transaction, does its
//! work, and commits before returning -- no transaction is ever held across
//! `.await`, and callers can invoke these synchronously from async handlers.
//!
//! The HTTP layer never holds a [`Db`]. It reaches data through three capability handles:
//! [`AccountDb`], bound to one account, which only loads that account's rows; [`InstanceDb`],
//! for instance settings, with no board content at all; and [`PublicDb`], for the flows that run
//! before anyone is signed in.

mod account;
mod activity;
mod entries;
mod events;
mod instance;
mod membership;
mod prune;
mod public;
mod store;
#[cfg(test)]
mod support;
mod usage;

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::{cmp, fmt};

use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use redb::{
    Database, MultimapTableDefinition, ReadTransaction, ReadableDatabase, ReadableMultimapTable,
    ReadableTable, ReadableTableMetadata, TableDefinition, WriteTransaction,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use uuid::Uuid;
use webauthn_rs::prelude::Passkey;

pub use self::account::{AccountDb, LinkReads, OwnedBoard, OwnedTask};
use self::activity::Activity;
use self::entries::txn_reindex_entries;
use self::events::txn_remove_events;
pub use self::events::{EventRead, MAX_EVENTS_PER_BOARD};
pub use self::instance::InstanceDb;
pub use self::membership::{
    AccountAction, AccountDeleteOutcome, AccountSummary, AdminActionOutcome, ModeSwitch,
    NewAccount, RecoverOutcome, RegisterOutcome, Revoked,
};
pub use self::public::PublicDb;
pub use self::store::{Store, WriteTxn};
use self::usage::{
    OverQuota, mutate_owned, refuse_row_past, txn_column_owner, txn_count_usage, txn_recount_usage,
    txn_refuse_over_bytes, txn_remove_entry, txn_remove_owned, txn_rows_bytes, txn_store_entry,
    txn_store_owned, txn_store_task, txn_task_bytes,
};
use crate::api_error::{ErrorCode, coded_response};
use crate::auth::{
    REAUTH_WINDOW_SECS, SESSION_MAX_AGE_SECONDS, SignIn, api_token_digest, api_token_last4,
    constant_time_eq, hash_feed_token, hash_session_token, verify_api_token,
};
use crate::backup::Backups;
use crate::disk::{DiskGuard, MIB};
use crate::limit::{Limits, TrustProxy};
use crate::models::{
    AccountId, AccountRole, AccountStatus, ActiveTimer, Actor, ActorKind, ApiTokenRecord,
    AuthScope, BackupSettings, BackupStatus, BoardRow, BoardTemplateRow, ColorRow, ColumnRow,
    DeviceAuthRow, DeviceAuthStatus, EntryKind, FieldDefRow, InstanceMode, InstanceSettings,
    LaneLimit, LinkKind, PasskeyRow, Quota, Quotas, RecoveryCodeRow, SessionRow, Settings, Subtask,
    SwimlaneRow, TaskAttachment, TaskClaim, TaskComment, TaskEvent, TaskHead, TaskLink, TaskRow,
    TextCap, TimeEntryRow, UserRow, standard_color,
};

pub type DbResult<T> = Result<T, Box<dyn std::error::Error>>;

/// A table of JSON rows keyed by id.
type Rows = TableDefinition<'static, &'static str, &'static [u8]>;

/// A multimap index from a parent id to child ids.
type Index = MultimapTableDefinition<'static, &'static str, &'static str>;

/// Where a new task lands in its column x swimlane cell.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NewTaskPlacement {
    Top,
    Bottom,
}

/// Whether a new column counts as Done.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    Regular,
    Done,
}

/// Typed outcome for [`Db::delete_column`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColumnDeleteOutcome {
    Deleted,
    NotFound,
    /// Refused: a board keeps at least one column.
    RefusedLast,
    /// Refused: this many tasks still sit in the column.
    RefusedHasTasks(usize),
}

/// Typed outcome for [`Db::delete_swimlane`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SwimlaneDeleteOutcome {
    /// Deleted, with its tasks moved into a neighboring lane.
    Deleted,
    /// No swimlane has this id.
    NotFound,
    /// Refused: a board keeps at least one swimlane.
    RefusedLast,
}

/// Typed outcome for [`Db::update_swimlane`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SwimlaneUpdateOutcome {
    /// Updated, with every field set in the patch stored.
    Updated,
    /// No swimlane has this id.
    NotFound,
    /// Refused: a limit names a column that is not on the lane's board.
    UnknownColumn,
}

/// Typed outcome for [`Db::merge_swimlanes`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SwimlaneMergeOutcome {
    /// Merged: `swimlanes` lanes deleted and `tasks` tasks moved into the main lane.
    Merged { swimlanes: usize, tasks: usize },
    /// No board has this id.
    NotFound,
    /// Refused: the board has one swimlane, so there is nothing to merge.
    RefusedSingle,
}

/// Typed outcome for [`Db::delete_color`] so handlers can map refusals
/// to 400 without parsing error strings.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TemplateDeleteOutcome {
    Deleted,
    NotFound,
    RefusedBuiltIn,
}

/// Sort position of a task whose swimlane is missing, which puts it after every lane.
pub const NO_LANE_POSITION: i64 = 9999;

/// Where a task sits, which is all [`Db::board_task_ids`] reads of it.
#[derive(Deserialize)]
struct TaskPlace {
    id: String,
    column_id: String,
    swimlane_id: Option<String>,
    position: f64,
}

/// What a count of a board's tasks by color reads of a task.
#[derive(Deserialize)]
struct TaskColor {
    /// The task's color id, absent on rows written before colors existed.
    #[serde(default)]
    color_id: Option<String>,
}

/// What a check of a `blocked_by` link reads of the task it names.
#[derive(Deserialize)]
struct TaskColumn {
    /// The column the task sits in.
    column_id: String,
}

/// What [`Db::board_labels`] reads of a task.
#[derive(Deserialize)]
struct TaskLabels {
    id: String,
    #[serde(default)]
    labels: Vec<String>,
}

/// Where a task sorts within its column: by swimlane position, then position, then id.
#[derive(Clone, Debug)]
struct CellOrder {
    /// Position of the task's swimlane, [`NO_LANE_POSITION`] when it has none.
    lane: i64,
    /// The task's position within its cell.
    position: f64,
    /// The task's id, the final tie-break.
    id: String,
}

impl CellOrder {
    /// Returns where task `id` in `swimlane` at `position` sorts, given the swimlane positions
    /// `lanes` of its board.
    #[must_use]
    fn of(lanes: &HashMap<String, i64>, id: String, swimlane: Option<&str>, position: f64) -> Self {
        Self {
            lane: swimlane
                .and_then(|swimlane| lanes.get(swimlane))
                .copied()
                .unwrap_or(NO_LANE_POSITION),
            position,
            id,
        }
    }
}

impl Ord for CellOrder {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.lane
            .cmp(&other.lane)
            .then_with(|| self.position.total_cmp(&other.position))
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for CellOrder {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for CellOrder {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for CellOrder {}

/// Error text when a task names a color that no longer exists.
pub const UNKNOWN_COLOR: &str = "unknown color";

/// Refusal to delete the only board.
pub const LAST_BOARD: &str = "cannot delete the last board";

/// Refusal to delete the last unarchived board while archived ones remain.
pub const LAST_ACTIVE_BOARD: &str =
    "cannot delete the last active board; unarchive another board first";

/// Why deleting board `id` from `boards` is refused, if it is: it is the
/// only board, or the last unarchived one.
pub fn board_delete_refusal(boards: &[BoardRow], id: &str) -> Option<&'static str> {
    if boards.len() <= 1 {
        return Some(LAST_BOARD);
    }
    let active = |board: &&BoardRow| !board.config_bool("archived");
    let deleting_active = boards.iter().filter(active).any(|board| board.id == id);
    let other_active = boards.iter().filter(active).any(|board| board.id != id);
    (deleting_active && !other_active).then_some(LAST_ACTIVE_BOARD)
}

/// A stored grouping date in the strict zero-padded "YYYY-MM-DD" form: a valid unpadded date
/// ("2026-1-5") is padded, anything that is not a date gives None.
pub fn normalize_grouping_date(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.trim().split('-').collect();
    let [year, month, day] = parts.as_slice() else {
        return None;
    };
    let digits = |part: &str, max_len: usize| {
        (1..=max_len).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    if year.len() != 4 || !digits(year, 4) || !digits(month, 2) || !digits(day, 2) {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(
        year.parse().ok()?,
        month.parse().ok()?,
        day.parse().ok()?,
    )?;
    Some(date.format("%Y-%m-%d").to_string())
}

/// Display name for a time entry logged without a task.
pub const NO_TASK_NAME: &str = "No task";

/// Returns the calendar day ("YYYY-MM-DD") in `tz` on which the RFC3339 `started_at` falls.
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
#[derive(Clone, Debug)]
pub enum LinkAddOutcome {
    /// The new link, or the identical one that already existed, as seen
    /// from the requesting task.
    Added(TaskLink),
    NotFound,
    /// Refused: the `blocked_by` chain would loop back to the task.
    Cycle,
}

/// Typed outcome for a change to one comment.
#[derive(Clone, Debug)]
pub enum CommentWrite<T> {
    /// The comment was changed, carrying the result.
    Done(T),
    /// The task or the comment does not exist.
    NotFound,
    /// Refused: the comment was not written by the API token the change is limited to.
    NotOwner,
}

/// Returns whether `comment` may change for `owner`: always without one, and with one only when
/// that API token wrote it.
fn comment_is_by(comment: &TaskComment, owner: Option<&str>) -> bool {
    owner.is_none_or(|id| {
        !id.is_empty()
            && comment.actor_kind == ActorKind::ApiToken
            && comment.token_id.as_deref() == Some(id)
    })
}

/// Typed outcome for a write guarded by an `If-Claim-Fence` value.
#[derive(Clone, Debug)]
pub enum FencedWrite<T> {
    Written(T),
    NotFound,
    /// Refused: a live claim holds a different fence; nothing written.
    StaleFence,
}

/// Fields of a claim request; see [`Db::claim_task`].
#[derive(Clone, Debug, Default)]
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
#[derive(Clone, Debug)]
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

/// The caller-supplied parts of a new time entry.
/// See [`Db::create_entry_full`].
#[derive(Copy, Clone, Debug)]
pub struct NewTimeEntry<'a> {
    /// `None` writes a task-less entry (an empty `task_id`).
    pub task_id: Option<&'a str>,
    /// RFC3339 start of the session.
    pub started_at: &'a str,
    /// Whole minutes logged.
    pub minutes: i64,
    /// Seconds logged.
    pub seconds: i64,
    /// Free-text note.
    pub note: &'a str,
    /// What kind of session the entry records.
    pub kind: EntryKind,
    /// Whether the session was stopped before finishing.
    pub interrupted: bool,
    /// Why the session was stopped early, if a reason was given.
    pub interrupt_reason: Option<&'a str>,
    /// Labels copied onto the entry.
    pub labels: &'a [String],
    /// Who logged the entry.
    pub created_by: &'a str,
    /// Break-activity id attributed to the session (breaks only).
    pub activity_id: Option<&'a str>,
}

/// The caller-supplied parts of a new comment; see [`Db::add_comment`].
#[derive(Copy, Clone, Debug)]
pub struct NewComment<'a> {
    pub author: &'a str,
    pub body: &'a str,
    pub actor_kind: ActorKind,
    pub token_name: Option<&'a str>,
    /// Id of the API token writing the comment, or none.
    pub token_id: Option<&'a str>,
    pub kind: Option<&'a str>,
}

/// Checks a move makes inside its write transaction.
#[derive(Copy, Clone, Debug, Default)]
pub struct MoveGuards<'a> {
    /// `(detail, actor)` of a `moved` history event to append.
    pub event: Option<(&'a str, Actor<'a>)>,
    /// An `If-Claim-Fence` value; a live claim holding another fence
    /// refuses the move.
    pub fence: Option<u64>,
    /// Refuse a move into another column that already holds its
    /// `wip_limit` of tasks.
    pub enforce_wip: bool,
}

/// Typed outcome for [`Db::move_task_logged`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceAuthCreateOutcome {
    Created,
    /// Refused: too many requests are pending already.
    TooManyPending,
    /// Refused: a live request already uses this user code.
    UserCodeTaken,
}

/// Typed outcome for [`Db::poll_device_auth`].
#[derive(Clone, Debug)]
pub enum DevicePoll {
    /// Approved: this token record was stored and the code is spent.
    Issued(Box<ApiTokenRecord>),
    Pending,
    Denied,
    Expired,
    /// Polled sooner than the interval allows.
    SlowDown,
    /// Approved, but the approving account already holds its most API tokens, so no token was
    /// stored and the request stays approved for a later poll.
    TooManyTokens(QuotaExceeded),
    /// Unknown, or already exchanged for a token.
    Invalid,
}

/// What the owner grants when approving a device request: the scope, never above the requested
/// one, and when the issued token expires.
#[derive(Copy, Clone, Debug)]
pub struct DeviceGrant {
    /// The scope the token gets.
    pub scope: AuthScope,
    /// Unix seconds, or [`None`] for a token that never expires.
    pub expires_at: Option<i64>,
}

/// Typed outcome for [`Db::claim_first_passkey`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FirstPasskeyOutcome {
    /// The passkey is stored, and the user was created if it did not exist.
    Claimed,
    /// Refused: a passkey exists already, or another user owns the instance.
    Taken,
}

/// Typed outcome for [`Db::add_passkey`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PasskeyAddOutcome {
    /// The passkey is stored.
    Added,
    /// Refused: a passkey with this credential id is registered already.
    Duplicate,
}

/// Typed outcome for [`Db::delete_passkey`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PasskeyDeleteOutcome {
    /// The passkey is gone.
    Deleted,
    /// No passkey has that id.
    NotFound,
    /// Refused: the owner keeps at least one passkey.
    RefusedLast,
}

/// Typed outcome for [`Db::create_field`].
#[derive(Clone, Debug)]
pub enum FieldCreateOutcome {
    Created(Box<FieldDefRow>),
    /// Refused: the board already has a field with this key.
    KeyExists,
}

// ---- Tables: id -> JSON row ----

const USERS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("users");
const USERNAMES: TableDefinition<'_, &str, &str> = TableDefinition::new("usernames");
const SESSIONS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("sessions");
const BOARDS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("boards");
const COLUMNS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("columns");
const SWIMLANES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("swimlanes");
const TASKS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("tasks");
const TIME_ENTRIES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("time_entries");
/// Single-row table (`"app"` -> JSON [`Settings`]).
const SETTINGS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("settings");
/// Single-row table (`"timer"` -> JSON [`ActiveTimer`]); absent when idle.
const ACTIVE_TIMER: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("active_timer");
const API_TOKENS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("api_tokens");
/// id -> JSON [`ColorRow`]; per-board task colors (KanbanFlow parity).
const TASK_COLORS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("task_colors");
/// id -> JSON [`BoardTemplateRow`]; reusable board recipes, without their snapshots.
const TEMPLATES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("templates");
/// template id -> JSON snapshot of the template's board, read only when the template is applied.
const TEMPLATE_SNAPSHOTS: TableDefinition<'_, &str, &[u8]> =
    TableDefinition::new("template_snapshots");
/// attachment id -> raw file bytes (KanbanFlow parity: attachments).
const ATTACHMENT_DATA: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("attachment_data");
/// Name of the board every new account starts with, built from the built-in Pomodoro template.
const STARTER_BOARD_NAME: &str = "General";
/// Key of the [`BackupSettings`] row in [`SETTINGS`].
const BACKUP_SETTINGS_KEY: &str = "backups";
/// Key of the [`BackupStatus`] row in [`SETTINGS`].
const BACKUP_STATUS_KEY: &str = "backup_status";
/// Key of the [`InstanceSettings`] row in [`SETTINGS`].
const INSTANCE_SETTINGS_KEY: &str = "instance";
/// Key of the one [`Settings`] row in [`SETTINGS`] of a version 9 file.
const LEGACY_APP_SETTINGS_KEY: &str = "app";
/// Key of the one [`ActiveTimer`] in [`ACTIVE_TIMER`] of a version 9 file.
const LEGACY_TIMER_KEY: &str = "timer";
/// account id -> JSON [`Settings`]; absent until the account saves its settings.
const ACCOUNT_SETTINGS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("account_settings");
/// account id -> JSON [`RecoveryCodeRow`]; absent until the account gets a recovery code.
const RECOVERY_CODES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("recovery_codes");
/// invite-token hash -> JSON [`InviteRow`]; the row goes when its invite is used or revoked.
const INVITES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("invites");
/// account id -> Unix seconds the account was last seen; absent until its first flushed touch.
const ACTIVITY: TableDefinition<'_, &str, i64> = TableDefinition::new("activity");
/// board id -> Unix seconds the board was last used; written when the board is created.
const BOARD_ACTIVITY: TableDefinition<'_, &str, i64> = TableDefinition::new("board_activity");
/// Key of the `PruneStatus` row in [`SETTINGS`].
const PRUNE_STATUS_KEY: &str = "prune_status";
/// token id -> JSON [`ApiTokenRecord`] of a suspended account, which only names the account a
/// refused Bearer request belongs to and never authenticates.
const SUSPENDED_TOKENS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("suspended_tokens");
/// account id -> bytes of task rows, time entries, saved templates and attachment files the
/// account stores, kept in step by every write that changes them.
const ACCOUNT_USAGE: TableDefinition<'_, &str, u64> = TableDefinition::new("account_usage");
/// board id and sequence number -> JSON [`BoardEventRow`]; the board's oldest go past
/// [`MAX_EVENTS_PER_BOARD`], and all of them go with the board.
const BOARD_EVENTS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("board_events");
/// board id -> events written to it in the write transaction that is open, which its commit empties
/// and wakes the waiters of. No committed file holds a row.
const EVENT_WAKEUPS: TableDefinition<'_, &str, u64> = TableDefinition::new("event_wakeups");
/// Schema stamp table; never rename it, or older binaries read the file as unstamped.
const META: TableDefinition<'_, &str, u64> = TableDefinition::new("meta");
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
/// 6. `TaskRow`: `due_target_column_id`, and a board's opaque `config_json` gains `due_time`.
/// 7. `TaskEvent`: `column_id`, and `ColumnRow`: `former_names`. A column's opaque
///    `config_json` stores `column_sum` as "count" or "hours" and `sorting` as "due", "color" or
///    "added" too.
/// 8. A board's opaque `config_json` holds `main_swimlane_id`, the id of the lane that renders
///    without a label.
///
/// Version 2 has shipped.
///
/// Version 3 adds, over version 2 (additive, read from older rows through a
/// serde default):
///
/// 1. `Subtask`: `due_at`.
///
/// Version 3 has shipped.
///
/// Version 4 removes, over version 3 (older rows still holding the key load,
/// because serde ignores unknown keys):
///
/// 1. `Settings`: `ding_enabled`.
///
/// Version 4 has shipped.
///
/// Version 5 changes, over version 4:
///
/// 1. `ApiTokenRecord` and `DeviceAuthRow`: `scope` (`AuthScope`: `read`, `work` or `admin`)
///    replaces the `scopes` list. The upgrade maps a list holding `write` to `admin`, and a
///    list of only `read` to `read`.
/// 2. `DeviceAuthRow`: `token_expires_at`.
/// 3. `TaskEvent`: `token_name`.
///
/// Version 5 has shipped.
///
/// Version 6 changes, over version 5:
///
/// 1. `ApiTokenRecord` and `DeviceAuthRow`: the stored `scope` gains `provision`, between `work`
///    and `admin`. The upgrade rewrites no row, and the new stamp makes a version-5 binary refuse
///    a file that may hold a `provision` token.
///
/// Version 7 adds, over version 6 (additive, read from older rows through a serde default):
///
/// 1. `FieldDefRow`: `columns` (`FieldColumn`), the typed columns of a `table` field, and the
///    `table` value of `FieldKind`. The upgrade rewrites no row, and the new stamp makes a
///    version-6 binary refuse a file that may hold a `table` field, which it would read as an
///    unknown kind.
///
/// Version 7 has shipped.
///
/// Version 8 changes, over version 7:
///
/// 1. `UserRow`: `password_hash` is gone. The upgrade strips it from every stored user and deletes
///    every stored session, since each was opened with a password.
/// 2. Table `passkeys` holding `PasskeyRow`, the passkeys the owner signs in with.
///
/// Version 9 adds, over version 8 (additive, read from an absent row as its defaults):
///
/// 1. `BackupSettings` and `BackupStatus`, stored as the `backups` and `backup_status` rows of
///    the `settings` table. The upgrade writes no row, and the new stamp makes a version-8
///    binary refuse a file whose owner configured backups.
///
/// Version 9 has shipped.
///
/// Version 10 binds every row to the account that owns it:
///
/// 1. `BoardRow`: `owner_id`, indexed by the multimap `boards_by_owner`. `ApiTokenRecord`:
///    `account_id`. `DeviceAuthRow`: `account_id`, set on approval. `BoardTemplateRow`:
///    `owner_id`, [`None`] for a built-in template. `TimeEntryRow`: `account_id`.
/// 2. `UserRow`: `role` (`AccountRole`), `status` (`AccountStatus`) and `created_at`.
/// 3. Table `account_settings` holding each account's `Settings`, which leave the `app` row of
///    `settings`. The active timer is keyed by account id instead of `timer`.
/// 4. `InstanceSettings` (`InstanceMode`), stored as the `instance` row of `settings`.
///
/// The upgrade gives every row to the instance's one user, who becomes its admin. A file without
/// a user keeps its rows ownerless until the first account claims them.
///
/// Version 11 adds, over version 10:
///
/// 1. `InstanceMode`: `multi`, which `--mode multi` stores.
/// 2. Table `recovery_codes` holding `RecoveryCodeRow`, each account's recovery code as a salted
///    hash.
/// 3. Table `invites` holding `InviteRow`, the unused invite links by the hash of their token.
///
/// The upgrade rewrites no row, and the new stamp makes a version-10 binary refuse a file that
/// may hold a member account.
///
/// Version 12 adds, over version 11:
///
/// 1. `AccountStatus`: `pending`, an account that requested to join and waits for the admin, and
///    `suspended`, an account the admin stopped.
/// 2. `InstanceSettings`: `registration` (`RegistrationMode`), read as `invite` from an older row,
///    and `pending_cap`, read as the default cap.
/// 3. Table `activity`, the time each account was last seen.
/// 4. Table `suspended_tokens` holding the `ApiTokenRecord`s a suspension took from
///    `api_tokens`.
///
/// The upgrade rewrites no row, and the new stamp makes a version-11 binary refuse a file that
/// may hold a pending or suspended account.
///
/// Version 13 adds, over version 12:
///
/// 1. `InstanceSettings`: `quotas` (`Quotas`), `limits` (`RateLimits`) and `disk` (`DiskLimits`),
///    each read as its defaults from an older row.
///
/// The upgrade rewrites no row, and the new stamp makes a version-12 binary refuse a file whose
/// settings it would drop on its next write.
///
/// Version 14 adds, over version 13:
///
/// 1. Table `board_activity`, the time each board was last used.
/// 2. `InstanceSettings`: `prune` (`PruneSettings`), read as its defaults from an older row, and
///    `prune_changed_at`, read as absent.
/// 3. `PruneStatus`, stored as the `prune_status` row of the `settings` table.
///
/// The upgrade records every existing board as used at the upgrade, so no board is pruned for
/// the time before its use was tracked.
///
/// Version 14 has shipped.
///
/// Version 15 adds, over version 14:
///
/// 1. `Quotas`: `subtasks_per_task`, `templates_per_account`, `stored_mib_per_account`,
///    `columns_per_board`, `swimlanes_per_board` and `fields_per_board`, each read as its default
///    from an older row.
/// 2. Table `account_usage`, the bytes each account stores in task rows, time entries, saved
///    templates and attachment files.
/// 3. `DeviceAuthRow`: `client_network`, read as absent from an older row.
/// 4. `BoardTemplateRow`: `snapshot_bytes`, without its `snapshot`, which moves to table
///    `template_snapshots`. Table `templates_by_owner` indexes the templates by owner.
///
/// The upgrade moves every template's snapshot to its own table and counts what every account
/// stores. The new stamp makes a version-14 binary refuse a file whose usage counts it would leave
/// stale.
///
/// Version 15 has shipped.
///
/// Version 16 adds, over version 15:
///
/// 1. `Quotas`: `time_entries_per_account`, read as its default from an older row.
/// 2. Multimaps `entries_by_account` and `entries_by_day`, which index each account's time
///    entries, and `passkeys_by_user`, which indexes each account's passkeys.
/// 3. Table `feed_tokens`, the board each calendar-feed token names, by the token's hash.
///
/// The upgrade builds every new index from the stored rows. The new stamp makes a version-15
/// binary refuse a file whose indexes it would leave stale.
///
/// Version 16 has shipped.
///
/// Version 17 adds, over version 16:
///
/// 1. `SessionRow`: `verified_at`, read as absent from an older row, so a session from before the
///    upgrade has to prove a passkey again before a sensitive action.
///
/// The upgrade rewrites no row. The new stamp makes a version-16 binary refuse a file whose
/// sessions it would save without their proof.
///
/// Version 18 changes, over version 17:
///
/// 1. Table `sessions` is keyed by the hex SHA-256 of the session cookie value instead of the
///    value itself.
///
/// The upgrade deletes every stored session, since none is keyed by a hash, so everyone signs in
/// again. The stored types and tables are the same as in version 17, so the fingerprint is too.
/// The new stamp makes a version-17 binary refuse a file whose sessions it could not find.
///
/// Version 19 adds, over version 18:
///
/// 1. `BoardRow`: `created_by_token`, the id of the API token that created the board, read as
///    empty from an older row.
///
/// The upgrade also gives every task that has no `column_entered_at` the time of its latest `moved`
/// history event, else its creation time, and counts what every account stores again. The new
/// stamp makes a version-18 binary refuse a file whose boards it would save without their
/// creator.
///
/// Version 20 adds, over version 19:
///
/// 1. Table `passkeys_by_credential`, base64url credential id -> passkey id.
/// 2. Multimap `tokens_by_account`, account id -> id of each API token it holds, live or
///    suspended.
/// 3. Multimap `sessions_by_user`, account id -> key of each session it has.
/// 4. Multimap `devices_by_account`, account id -> key of each device request it approved.
///
/// The upgrade builds all four from the rows. The new stamp makes a version-19 binary refuse a
/// file whose rows it would change without keeping these in step.
///
/// Version 21 adds, over version 20 (additive, read from older rows through a serde default):
///
/// 1. `TaskComment`: `token_id`, the id of the API token that wrote the comment, and `edited_at`,
///    the time of its last edit.
///
/// The upgrade rewrites no row, so a comment from before the upgrade names no token and only an
/// admin can edit or delete it. The new stamp makes a version-20 binary refuse a file whose
/// comments it would save without their token or edit time.
///
/// Version 22 adds, over version 21:
///
/// 1. Table `token_ids_by_hash`, hex SHA-256 of an API token -> id of the token, live or
///    suspended. A new token keeps that SHA-256 as its `token_hash` and an empty `salt`.
/// 2. Multimap `salted_tokens_by_last4`, last four characters of a token -> id of each token
///    still under a salted hash.
/// 3. Multimap `devices_by_user_code`, canonical user code -> key of each device request that
///    carries it.
///
/// The upgrade indexes every stored token by its last four characters, because a salted hash
/// cannot be turned into a key without the raw token, and every device request by its user code.
/// A salted token moves to `token_ids_by_hash` the first time it authenticates. The new stamp
/// makes a version-21 binary refuse a file whose tokens and device requests it would change
/// without keeping these in step.
///
/// Version 23 changes, over version 22:
///
/// 1. `TaskRow`: `column_added_at` is now `column_entered_at`, the time the task entered its
///    column.
/// 2. Multimap `devices_by_expiry`, unix second -> key of each device request that expires then.
///
/// The upgrade moves each task's stamp to the new name and indexes every device request by its
/// expiry. A suspended account's tokens are no longer deleted when the suspension is lifted, which
/// changes no stored shape. The new stamp makes a version-22 binary refuse a file whose tasks it
/// would read without their stamp and whose device requests it would store without this index.
///
/// Version 24 adds, over version 23 (additive, read from older rows through a serde default):
///
/// 1. `SwimlaneRow`: `description`, the text shown as a tooltip on the lane's header, `limits`, the
///    WIP limits it puts on columns (`LaneLimit`: `column_id` and `max`), `collapsed`, whether the
///    lane is folded, and `height`, the lane's dragged height in pixels.
///
/// The upgrade rewrites no row, so a lane from before the upgrade reads with no description, no
/// limits, unfolded, at the default height. The new stamp makes a version-23 binary refuse a file
/// whose lanes it would save without those fields.
///
/// Version 25 adds, over version 24 (additive):
///
/// 1. Table `board_events` holding [`BoardEventRow`] ([`EventKind`]), keyed by board id and a
///    sequence number that counts up per board.
/// 2. Table `event_wakeups`, empty in every committed file.
///
/// The upgrade rewrites no row, so a board from before the upgrade starts with no events, and its
/// first cursor is 0. The new stamp makes a version-24 binary refuse a file whose events it would
/// neither keep in step with the tasks nor delete with their board.
///
/// The next change to stored data bumps the version again.
pub const SCHEMA_VERSION: u64 = 25;

/// Stored-schema fingerprint per version, oldest first; append one with each
/// bump and never edit a released entry. The schema fingerprint test fails until
/// a new entry is appended.
#[cfg(test)]
const SCHEMA_FINGERPRINTS: [&str; SCHEMA_VERSION as usize] = [
    "d9998454dc068986eed12c06c12d61ae715bac8ff7b5af745cc4ca4219b98ab7",
    "060770c2715c3136209d202e87d67a350eaf9287bdf41a8b76feeb17ffe994f1",
    "c2d6a2a8b3fb29dffb1695e7a5ac7d06541e7edebe8647077777bc7e08f2de78",
    "e11946eab5836edf41f3654e0e50fa920bc2727b5b2474f4fc1a4d3b6960b94e",
    "d719e02af9cafa00e1a908d97ebc652e5b40d7ec2005d75bdadac54276ba9b07",
    "ad1bf8d90656b521fa509f738a77405b0c3bc34df1fe84f88c510464a2b2203b",
    "f367d3069c76ca1bfafb68c18d5a3aa813d14e4f1e18240529ce1dbad7ceccbe",
    "e4d7a00eaf75c6361de746b7e6c2feaa6379088feb999ead65f8dd815352b66e",
    "01694a0326d3b3e140aecc6b2094fd28f078236e7ef915a0a02e1eacb45d429d",
    "eabcf33289e858e89131758230a6967850c2d29e0e20fbf807c13b2a2f36992c",
    "7be40c1ab7474df991632faa0d0785dea7d4ba0dd4b8213322cd0429379fad83",
    "782be9c82faec249c44282a9509aacffabdff8549a5fef3ff371d410e113c4e2",
    "4046c706868d928cf3fb4ec7ce17ee6c369d85e645620a86ab190850578aa435",
    "2a88e9514fb07500cd88c35828302680e45a59fb6631d812d07b70ba482155f6",
    "fe19b7bd59ec3a4cc715ca58876f88626c2a33ff7c3e5a3dc89dafdc5ee25a6b",
    "e44f723b5c0c160d8bc732fd8fee64bdea2c907cf7205f25e1eb796655cb025b",
    "efc976b0fd3a0e28b535f5eaab1a1e73221a7f3a298102611ccc86b193866264",
    "efc976b0fd3a0e28b535f5eaab1a1e73221a7f3a298102611ccc86b193866264",
    "59ef3740dc2610a8f370fe6311af7e100c11efaf95eac6391bb689f921a00fce",
    "1a0785c9ee3d458fffacd9a341aa2a65faa029879aa36305c728366df26aa836",
    "be37bb65b07e6b7fa103f23d56aa7fe635f41bf290c570c294a9fc08c0541997",
    "77cb1ab364361c38cf8ecc2a5ff252b369bcd49f3c7481e0dc54dccf8ad668f0",
    "5e1e939bf88dc6ab68db420648356f2a996be086d7f800fdc0fdeb14806a4634",
    "8c199cc5167624aa228c51abd45c752d25070a48e8755c2223f55318f82241db",
    "bba1a46e45b445b6886e6b52f04138c0af42fc456708f5f6a4665b029aef62cc",
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
            "database {} has schema version {}, but this pomb binary supports up to \
             schema version {}; refusing to open it so its newer data is not dropped. \
             Restore a backup taken before the upgrade, or upgrade pomb.",
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

/// Gives the free pages of the database file at `path` back to the file system, so a file that
/// grew past the size limit shrinks once data is deleted, and logs its size before and after. A
/// failed compaction is logged and leaves the file as it was.
fn compact(db: &mut Database, path: &str) {
    let size = || std::fs::metadata(path).map_or(0, |meta| meta.len()) as f64 / MIB as f64;
    let before = size();
    match db.compact() {
        Ok(_) => println!(
            "pomb: compacted database {path} from {before:.1} MiB to {:.1} MiB",
            size()
        ),
        Err(error) => eprintln!("pomb: could not compact database {path}: {error}"),
    }
}

/// Brings a file stamped `from` (`None`: unstamped) up to [`SCHEMA_VERSION`].
/// The schema is additive with serde defaults, so most changes need no row rewritten;
/// a non-additive change transforms rows here.
fn upgrade_schema(txn: &WriteTransaction, from: Option<u64>) -> DbResult<()> {
    if from.is_none_or(|version| version < 5) {
        rename_write_scope(txn)?;
    }
    if from.is_none_or(|version| version < 8) {
        drop_password_hashes(txn)?;
    }
    if from.is_none_or(|version| version < 10) {
        assign_rows_to_owner(txn, Utc::now().timestamp())?;
    }
    if from.is_none_or(|version| version < 14) {
        txn_mark_unrecorded_boards_used(txn, Utc::now().timestamp())?;
    }
    if from.is_none_or(|version| version < 15) {
        split_template_snapshots(txn)?;
        txn_recount_usage(txn)?;
    }
    if from.is_none_or(|version| version < 16) {
        txn_reindex_entries(txn)?;
        txn_reindex_passkeys(txn)?;
        txn_reindex_feed_tokens(txn)?;
    }
    if from.is_none_or(|version| version < 18) {
        txn.open_table(SESSIONS)?.retain(|_, _| false)?;
    }
    if from.is_none_or(|version| version < 23) {
        rename_column_stamp(txn)?;
        txn_reindex_device_expiry(txn)?;
    }
    if from.is_none_or(|version| version < 19) {
        stamp_column_entry(txn)?;
        txn_recount_usage(txn)?;
    }
    if from.is_none_or(|version| version < 20) {
        txn_reindex_credentials(txn)?;
        txn_reindex_tokens(txn)?;
        txn_reindex_sessions(txn)?;
        txn_reindex_devices(txn)?;
    }
    if from.is_none_or(|version| version < 22) {
        txn_reindex_token_keys(txn)?;
        txn_reindex_device_codes(txn)?;
    }
    Ok(())
}

/// Moves each task's `column_added_at` to `column_entered_at`, keeping a value the new name
/// already holds.
fn rename_column_stamp(txn: &WriteTransaction) -> DbResult<()> {
    rewrite_rows(txn, TASKS, |_, task| {
        let Some(stamp) = task.remove("column_added_at") else {
            return false;
        };
        if task
            .get("column_entered_at")
            .is_none_or(serde_json::Value::is_null)
        {
            task.insert("column_entered_at".to_string(), stamp);
        }
        true
    })
}

/// Gives every task without a `column_entered_at` the time of its latest `moved` history event, or
/// its creation time when it has none.
fn stamp_column_entry(txn: &WriteTransaction) -> DbResult<()> {
    rewrite_rows(txn, TASKS, |_, task| {
        if task
            .get("column_entered_at")
            .is_some_and(|stamp| !stamp.is_null())
        {
            return false;
        }
        let moved = task
            .get("history")
            .and_then(serde_json::Value::as_array)
            .and_then(|events| {
                events.iter().rfind(|event| {
                    event.get("kind").and_then(serde_json::Value::as_str) == Some("moved")
                })
            })
            .and_then(|event| event.get("created_at"));
        let Some(stamp) = moved.or_else(|| task.get("created_at")).cloned() else {
            return false;
        };
        task.insert("column_entered_at".to_string(), stamp);
        true
    })
}

/// Records every board without a recorded use as used at `now`.
fn txn_mark_unrecorded_boards_used(txn: &WriteTransaction, now: i64) -> DbResult<()> {
    let boards = txn.open_table(BOARDS)?;
    let mut used = txn.open_table(BOARD_ACTIVITY)?;
    for item in boards.iter()? {
        let (id, _) = item?;
        if used.get(id.value())?.is_none() {
            used.insert(id.value(), now)?;
        }
    }
    Ok(())
}

/// Rewrites every row of `table` that holds an object through `edit`, which reports whether it
/// changed the row.
fn rewrite_rows(
    txn: &WriteTransaction,
    table: Rows,
    mut edit: impl FnMut(&str, &mut JsonObject) -> bool,
) -> DbResult<()> {
    let mut tbl = txn.open_table(table)?;
    let mut rewritten = Vec::new();
    for item in tbl.iter()? {
        let (key, value) = item?;
        let mut row: serde_json::Value = serde_json::from_slice(value.value())?;
        if let Some(object) = row.as_object_mut()
            && edit(key.value(), object)
        {
            rewritten.push((key.value().to_owned(), serde_json::to_vec(&row)?));
        }
    }
    for (key, raw) in rewritten {
        tbl.insert(key.as_str(), raw.as_slice())?;
    }
    Ok(())
}

/// A stored row as a JSON object.
type JsonObject = serde_json::Map<String, serde_json::Value>;

/// Field naming the owning account on token and device rows and on time entries.
const ACCOUNT_ID_FIELD: &str = "account_id";

/// Field naming the owning account on boards and templates.
const OWNER_ID_FIELD: &str = "owner_id";

/// Sets `field` to `owner` on every row of `table`.
fn set_owner_field(
    txn: &WriteTransaction,
    table: Rows,
    field: &str,
    owner: &AccountId,
) -> DbResult<()> {
    rewrite_rows(txn, table, |_, row| {
        row.insert(field.to_string(), owner.as_str().into());
        true
    })
}

/// Moves the row `from` of `table` to the key `to`, when it exists.
fn move_row(txn: &WriteTransaction, table: Rows, from: &str, to: &str) -> DbResult<()> {
    let mut tbl = txn.open_table(table)?;
    let row = tbl.remove(from)?.map(|guard| guard.value().to_vec());
    if let Some(raw) = row {
        tbl.insert(to, raw.as_slice())?;
    }
    Ok(())
}

/// Moves the row `from` of `from_table` to the key `to` of `to_table`, when it exists.
fn move_row_to(
    txn: &WriteTransaction,
    from_table: Rows,
    from: &str,
    to_table: Rows,
    to: &str,
) -> DbResult<()> {
    let row = txn
        .open_table(from_table)?
        .remove(from)?
        .map(|guard| guard.value().to_vec());
    if let Some(raw) = row {
        txn.open_table(to_table)?.insert(to, raw.as_slice())?;
    }
    Ok(())
}

/// Runs the version 10 upgrade: gives every board, token, approved device request, saved template,
/// time entry, the settings and the active timer to the instance's one user, who becomes its
/// admin, and stores single mode. Without a user the rows stay ownerless.
fn assign_rows_to_owner(txn: &WriteTransaction, now: i64) -> DbResult<()> {
    let owner = txn
        .open_table(USERS)?
        .first()?
        .map_or_else(AccountId::ownerless, |(key, _)| AccountId::new(key.value()));
    make_users_admins(txn, now)?;
    assign_boards(txn, &owner)?;
    set_owner_field(txn, API_TOKENS, ACCOUNT_ID_FIELD, &owner)?;
    assign_decided_devices(txn, &owner)?;
    assign_saved_templates(txn, &owner)?;
    set_owner_field(txn, TIME_ENTRIES, ACCOUNT_ID_FIELD, &owner)?;
    move_row_to(
        txn,
        SETTINGS,
        LEGACY_APP_SETTINGS_KEY,
        ACCOUNT_SETTINGS,
        owner.as_str(),
    )?;
    move_row(txn, ACTIVE_TIMER, LEGACY_TIMER_KEY, owner.as_str())?;
    write_one(
        txn,
        SETTINGS,
        INSTANCE_SETTINGS_KEY,
        &InstanceSettings::default(),
    )
}

/// Makes every stored user an active admin created at `now`, keeping a stored creation time.
fn make_users_admins(txn: &WriteTransaction, now: i64) -> DbResult<()> {
    let role = serde_json::to_value(AccountRole::Admin)?;
    let status = serde_json::to_value(AccountStatus::Active)?;
    rewrite_rows(txn, USERS, |_, user| {
        user.insert("role".to_string(), role.clone());
        user.insert("status".to_string(), status.clone());
        user.entry("created_at").or_insert(now.into());
        true
    })
}

/// Gives every board to `owner` and indexes it under that owner.
fn assign_boards(txn: &WriteTransaction, owner: &AccountId) -> DbResult<()> {
    let mut boards = Vec::new();
    rewrite_rows(txn, BOARDS, |id, board| {
        boards.push(id.to_owned());
        board.insert(OWNER_ID_FIELD.to_string(), owner.as_str().into());
        true
    })?;
    let mut by_owner = txn.open_multimap_table(BOARDS_BY_OWNER)?;
    for id in &boards {
        by_owner.insert(owner.as_str(), id.as_str())?;
    }
    Ok(())
}

/// Gives every approved or issued device request to `owner`, whose token it mints.
fn assign_decided_devices(txn: &WriteTransaction, owner: &AccountId) -> DbResult<()> {
    let decided = [DeviceAuthStatus::Approved, DeviceAuthStatus::Issued]
        .map(serde_json::to_value)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    rewrite_rows(txn, DEVICE_AUTHS, |_, request| {
        let assign = request
            .get("status")
            .is_some_and(|status| decided.contains(status));
        if assign {
            request.insert(ACCOUNT_ID_FIELD.to_string(), owner.as_str().into());
        }
        assign
    })
}

/// Gives every saved template to `owner` and marks every built-in one as owned by none.
fn assign_saved_templates(txn: &WriteTransaction, owner: &AccountId) -> DbResult<()> {
    rewrite_rows(txn, TEMPLATES, |_, template| {
        let built_in = template.get("built_in") == Some(&serde_json::Value::Bool(true));
        let owner_id = if built_in {
            serde_json::Value::Null
        } else {
            owner.as_str().into()
        };
        template.insert(OWNER_ID_FIELD.to_string(), owner_id);
        true
    })
}

/// Gives every ownerless row to `account`, inside the transaction that stores the instance's
/// first user.
fn claim_ownerless_rows(txn: &WriteTransaction, account: &AccountId) -> DbResult<()> {
    let ownerless = AccountId::ownerless();
    claim_ownerless_boards(txn, account)?;
    for (table, field) in [
        (API_TOKENS, ACCOUNT_ID_FIELD),
        (TEMPLATES, OWNER_ID_FIELD),
        (TIME_ENTRIES, ACCOUNT_ID_FIELD),
    ] {
        rewrite_rows(txn, table, |_, row| {
            let claim =
                row.get(field).and_then(serde_json::Value::as_str) == Some(ownerless.as_str());
            if claim {
                row.insert(field.to_string(), account.as_str().into());
            }
            claim
        })?;
    }
    for table in [ACCOUNT_SETTINGS, ACTIVE_TIMER] {
        move_row(txn, table, ownerless.as_str(), account.as_str())?;
    }
    txn_reindex_templates(txn)?;
    txn_reindex_entries(txn)?;
    txn_reindex_tokens(txn)?;
    txn_recount_usage(txn)
}

/// Moves the snapshot of every template row that still holds one to [`TEMPLATE_SNAPSHOTS`], one
/// row at a time, and rebuilds [`TEMPLATES_BY_OWNER`].
fn split_template_snapshots(txn: &WriteTransaction) -> DbResult<()> {
    let ids: Vec<String> = {
        let tbl = txn.open_table(TEMPLATES)?;
        let mut ids = Vec::new();
        for item in tbl.iter()? {
            ids.push(item?.0.value().to_string());
        }
        ids
    };
    for id in ids {
        let raw = txn
            .open_table(TEMPLATES)?
            .get(id.as_str())?
            .map(|guard| guard.value().to_vec());
        let Some(raw) = raw else {
            continue;
        };
        let mut row: JsonObject = serde_json::from_slice(&raw)?;
        let Some(snapshot) = row.remove("snapshot") else {
            continue;
        };
        let snapshot = serde_json::to_vec(&snapshot)?;
        row.insert("snapshot_bytes".to_string(), snapshot.len().into());
        txn.open_table(TEMPLATE_SNAPSHOTS)?
            .insert(id.as_str(), snapshot.as_slice())?;
        txn.open_table(TEMPLATES)?
            .insert(id.as_str(), serde_json::to_vec(&row)?.as_slice())?;
    }
    txn_reindex_templates(txn)
}

/// Refuses, with [`QuotaExceeded`], a template `snapshot` past [`MAX_TEMPLATE_ROW_BYTES`].
fn refuse_template_past(snapshot: &[u8]) -> DbResult<()> {
    refuse_row_past(
        Bound::TemplateRowBytes,
        MAX_TEMPLATE_ROW_BYTES,
        snapshot.len() as u64,
        0,
    )
}

/// Returns the key `template` sits under in [`TEMPLATES_BY_OWNER`].
#[must_use]
fn template_owner_key(template: &BoardTemplateRow) -> &str {
    template.owner_id.as_deref().unwrap_or(BUILT_IN_TEMPLATES)
}

/// Rebuilds [`TEMPLATES_BY_OWNER`] from the template rows, inside an open write transaction.
fn txn_reindex_templates(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(TEMPLATES_BY_OWNER)?;
    let mut index = txn.open_multimap_table(TEMPLATES_BY_OWNER)?;
    for item in txn.open_table(TEMPLATES)?.iter()? {
        let (id, raw) = item?;
        let template: BoardTemplateRow = serde_json::from_slice(raw.value())?;
        index.insert(template_owner_key(&template), id.value())?;
    }
    Ok(())
}

/// Stores `template` with its `snapshot`, inside an open write transaction, and returns the bytes
/// both take. The row's `snapshot_bytes` is set from `snapshot`.
fn txn_store_template(
    txn: &WriteTransaction,
    template: &mut BoardTemplateRow,
    snapshot: &[u8],
) -> DbResult<u64> {
    template.snapshot_bytes = snapshot.len() as u64;
    let raw = serde_json::to_vec(&*template)?;
    txn.open_table(TEMPLATES)?
        .insert(template.id.as_str(), raw.as_slice())?;
    txn.open_table(TEMPLATE_SNAPSHOTS)?
        .insert(template.id.as_str(), snapshot)?;
    mmap_insert(
        txn,
        TEMPLATES_BY_OWNER,
        template_owner_key(template),
        &template.id,
    )?;
    Ok(raw.len() as u64 + template.snapshot_bytes)
}

/// Removes template `id` with its snapshot, inside an open write transaction, and returns it
/// with the bytes both took.
fn txn_remove_template(
    txn: &WriteTransaction,
    id: &str,
) -> DbResult<Option<(BoardTemplateRow, u64)>> {
    let raw = txn
        .open_table(TEMPLATES)?
        .remove(id)?
        .map(|guard| guard.value().to_vec());
    let Some(raw) = raw else {
        return Ok(None);
    };
    let template: BoardTemplateRow = serde_json::from_slice(&raw)?;
    txn.open_table(TEMPLATE_SNAPSHOTS)?.remove(id)?;
    mmap_remove(txn, TEMPLATES_BY_OWNER, template_owner_key(&template), id)?;
    let freed = raw.len() as u64 + template.snapshot_bytes;
    Ok(Some((template, freed)))
}

/// Returns the templates indexed under `owner` in [`TEMPLATES_BY_OWNER`], read inside `txn`.
fn txn_templates_of(txn: &impl Reads, owner: &str) -> DbResult<Vec<BoardTemplateRow>> {
    let mut out = Vec::new();
    for id in index_in(txn, TEMPLATES_BY_OWNER, owner)? {
        if let Some(template) = read_in::<BoardTemplateRow>(txn, TEMPLATES, &id)? {
            out.push(template);
        }
    }
    Ok(out)
}

/// Returns the snapshot of template `id`, read inside `txn`.
fn txn_template_snapshot(txn: &impl Reads, id: &str) -> DbResult<Option<serde_json::Value>> {
    read_in(txn, TEMPLATE_SNAPSHOTS, id)
}

/// Gives every ownerless board to `account` and moves it to that account's index.
fn claim_ownerless_boards(txn: &WriteTransaction, account: &AccountId) -> DbResult<()> {
    let ownerless = AccountId::ownerless();
    let boards = txn_mmap_get(txn, BOARDS_BY_OWNER, ownerless.as_str())?;
    let mut tbl = txn.open_table(BOARDS)?;
    let mut by_owner = txn.open_multimap_table(BOARDS_BY_OWNER)?;
    for id in &boards {
        if let Some(mut board) = row_in::<BoardRow, _>(&tbl, id)? {
            board.owner_id = account.as_str().to_string();
            tbl.insert(id.as_str(), serde_json::to_vec(&board)?.as_slice())?;
            by_owner.insert(account.as_str(), id.as_str())?;
        }
    }
    by_owner.remove_all(ownerless.as_str())?;
    Ok(())
}

/// Key of the argon2 hash a user row held before sign-in moved to passkeys.
const LEGACY_PASSWORD_HASH: &str = "password_hash";

/// Removes the stored password hash from every user row that still holds one.
fn drop_password_hashes(txn: &WriteTransaction) -> DbResult<()> {
    let mut tbl = txn.open_table(USERS)?;
    let mut rewritten = Vec::new();
    for item in tbl.iter()? {
        let (key, value) = item?;
        let mut row: serde_json::Value = serde_json::from_slice(value.value())?;
        let removed = row
            .as_object_mut()
            .and_then(|object| object.remove(LEGACY_PASSWORD_HASH));
        if removed.is_some() {
            rewritten.push((key.value().to_owned(), serde_json::to_vec(&row)?));
        }
    }
    for (key, raw) in rewritten {
        tbl.insert(key.as_str(), raw.as_slice())?;
    }
    Ok(())
}

/// Scope name a stored `scopes` list used for what `admin` now grants.
const LEGACY_WRITE_SCOPE: &str = "write";

/// Rewrites each stored `scopes` list in token and device rows to the single `scope` its highest
/// entry names. A stored `write` scope becomes `admin`, which has the same power.
fn rename_write_scope(txn: &WriteTransaction) -> DbResult<()> {
    rewrite_scopes_in(txn, API_TOKENS)?;
    rewrite_scopes_in(txn, DEVICE_AUTHS)
}

/// Replaces `scopes` with `scope` in every row of `table` that still holds the list.
fn rewrite_scopes_in(
    txn: &WriteTransaction,
    table: TableDefinition<'_, &str, &[u8]>,
) -> DbResult<()> {
    let mut tbl = txn.open_table(table)?;
    let mut rewritten = Vec::new();
    for item in tbl.iter()? {
        let (key, value) = item?;
        let mut row: serde_json::Value = serde_json::from_slice(value.value())?;
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        let Some(scopes) = object.remove("scopes") else {
            continue;
        };
        let listed = |name: &str| {
            scopes
                .as_array()
                .is_some_and(|list| list.iter().any(|scope| scope == name))
        };
        let scope = if listed(LEGACY_WRITE_SCOPE) || listed(AuthScope::Admin.as_str()) {
            AuthScope::Admin
        } else if listed(AuthScope::Work.as_str()) {
            AuthScope::Work
        } else {
            AuthScope::Read
        };
        object.insert("scope".to_string(), serde_json::to_value(scope)?);
        rewritten.push((key.value().to_owned(), serde_json::to_vec(&row)?));
    }
    for (key, raw) in rewritten {
        tbl.insert(key.as_str(), raw.as_slice())?;
    }
    Ok(())
}

/// Refuse a newer file before any write transaction, stamp an older or
/// unstamped one, and leave an equal one untouched.
fn ensure_schema_version(db: &Database, path: &str, repaired: bool) -> DbResult<()> {
    let found = read_schema_version(db)?;
    refuse_newer(path, found, repaired)?;
    if found == Some(SCHEMA_VERSION) {
        println!("pomb: database {path} is at schema version {SCHEMA_VERSION}");
        return Ok(());
    }
    let txn = db.begin_write()?;
    upgrade_schema(&txn, found)?;
    txn.open_table(META)?
        .insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
    txn.commit()?;
    match found {
        Some(old) => {
            println!("pomb: upgraded database {path} from schema version {old} to {SCHEMA_VERSION}")
        }
        None => println!("pomb: stamped database {path} with schema version {SCHEMA_VERSION}"),
    }
    Ok(())
}

/// device-code hash -> [`DeviceAuthRow`].
const DEVICE_AUTHS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("device_auths");
/// field id -> [`FieldDefRow`].
const FIELD_DEFS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("field_defs");
/// passkey id -> [`PasskeyRow`].
const PASSKEYS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("passkeys");

// ---- Multimap indexes: parent id -> child id ----

const COLUMNS_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("columns_by_board");
const SWIMLANES_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("swimlanes_by_board");
const TASKS_BY_COLUMN: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("tasks_by_column");
const ENTRIES_BY_TASK: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("entries_by_task");
/// board id -> color id for [`ColorRow`] rows.
const COLORS_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("colors_by_board");
/// board id -> field id for [`FieldDefRow`] rows.
const FIELDS_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("fields_by_board");
/// task id -> id of each task holding a [`TaskLink`] that targets it.
const LINKS_BY_TARGET: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("task_links_by_target");
/// account id -> id of each board it owns, with ownerless boards under the empty id.
const BOARDS_BY_OWNER: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("boards_by_owner");
/// account id -> id of each template it saved, with built-in templates under
/// [`BUILT_IN_TEMPLATES`] and ownerless ones under the empty id.
const TEMPLATES_BY_OWNER: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("templates_by_owner");
/// Key of the built-in templates in [`TEMPLATES_BY_OWNER`], which no account id can equal.
const BUILT_IN_TEMPLATES: &str = "built-in";
/// account id -> id of each time entry it logged.
const ENTRIES_BY_ACCOUNT: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("entries_by_account");
/// `<account id>|<UTC day YYYY-MM-DD>` -> id of each time entry the account started that day.
const ENTRIES_BY_DAY: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("entries_by_day");
/// account id -> id of each passkey it signs in with.
const PASSKEYS_BY_USER: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("passkeys_by_user");
/// hex SHA-256 of a calendar-feed token -> id of the board it names.
const FEED_TOKENS: TableDefinition<'_, &str, &str> = TableDefinition::new("feed_tokens");
/// base64url credential id -> id of the passkey that holds it.
const PASSKEYS_BY_CREDENTIAL: TableDefinition<'_, &str, &str> =
    TableDefinition::new("passkeys_by_credential");
/// account id -> id of each API token it holds, in [`API_TOKENS`] or [`SUSPENDED_TOKENS`].
const TOKENS_BY_ACCOUNT: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("tokens_by_account");
/// account id -> key in [`SESSIONS`] of each session it has.
const SESSIONS_BY_USER: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("sessions_by_user");
/// account id -> key in [`DEVICE_AUTHS`] of each device request it approved.
const DEVICES_BY_ACCOUNT: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("devices_by_account");
/// hex SHA-256 of an API token -> id of the token, in [`API_TOKENS`] or [`SUSPENDED_TOKENS`].
const TOKEN_IDS_BY_HASH: TableDefinition<'_, &str, &str> =
    TableDefinition::new("token_ids_by_hash");
/// Last four characters of an API token still under a salted hash -> id of each such token. A
/// token leaves it for [`TOKEN_IDS_BY_HASH`] the first time it authenticates.
const SALTED_TOKENS_BY_LAST4: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("salted_tokens_by_last4");
/// canonical user code -> key in [`DEVICE_AUTHS`] of each device request that carries it.
const DEVICES_BY_USER_CODE: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("devices_by_user_code");
/// Unix second a device request expires -> key in [`DEVICE_AUTHS`] of each request that expires
/// then, whatever its status.
const DEVICES_BY_EXPIRY: MultimapTableDefinition<'_, i64, &str> =
    MultimapTableDefinition::new("devices_by_expiry");

/// First session id of a process: microseconds since the epoch, so ids stay
/// unique across restarts and below 2^53 (JavaScript's integer limit).
fn session_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as u64)
}

/// Thin wrapper around the redb database handle.
#[derive(Clone, Debug)]
pub struct Db {
    db: Arc<Store>,
    /// Where the database file lives, which a backup copies.
    path: Arc<Path>,
    /// Source of in-memory timer session ids.
    session_seq: Arc<AtomicU64>,
    /// Id of each account's running timer session, absent after a restart.
    sessions: Arc<Mutex<HashMap<AccountId, u64>>>,
    /// Per account, serializes every read-modify-write of its active timer and session id.
    timer_gates: Arc<Mutex<HashMap<AccountId, Arc<Mutex<()>>>>>,
    /// When accounts were last seen, waiting for [`Db::flush_activity`].
    activity: Arc<Mutex<Activity>>,
    /// When boards were last used, recorded in memory until they are written in a batch.
    board_activity: Arc<Mutex<Activity>>,
}

/// Shared application state, used by the HTTP layer. Its database is reachable only through the
/// capability handles.
#[derive(Clone, Debug)]
pub struct AppState {
    db: Db,
    /// Wrong device-pairing codes typed on this server instance.
    pub device_lockout: DeviceCodeLockout,
    /// Headless bootstrap token from `POMB_BOOTSTRAP_TOKEN`, read once at
    /// startup.
    pub bootstrap_token: Option<BootstrapToken>,
    /// Externally visible base URL from `--public-url`, without a trailing slash.
    pub public_url: Option<String>,
    /// When each API token's `last_used_at` was last written.
    pub token_touch: TokenTouchThrottle,
    /// Per-credential and per-IP request limits.
    pub limits: Limits,
    /// Which proxy in front of the server names the client.
    pub trust_proxy: TrustProxy,
    /// How browsers sign in.
    pub sign_in: SignIn,
    /// How many accounts the instance serves.
    pub mode: InstanceMode,
    /// Where database backups are written and whether one is running.
    pub backups: Backups,
    /// When writes stop because the database or its disk is full.
    pub disk: DiskGuard,
    /// One permit per attachment upload that may run at once.
    pub uploads: Arc<Semaphore>,
    /// One permit per attachment download that may run at once.
    pub downloads: Arc<Semaphore>,
    /// One permit per request that may hold a whole board's tasks in memory at once.
    pub boards: Arc<Semaphore>,
    /// The permits of each account that asked for one, by kind and account id.
    account_slots: Arc<Mutex<AccountSlots>>,
    /// Held while settings are stored and applied, so saves run one at a time.
    settings_gate: Arc<Mutex<()>>,
    /// The OpenAPI document as JSON, with the public URL as its server, built on its first
    /// request.
    pub openapi: Arc<OnceLock<String>>,
}

/// The permits of each account that asked for one, by kind and account id.
type AccountSlots = HashMap<(AccountSlot, String), Arc<Semaphore>>;

/// The kinds of request each account may run only a few of at once, besides the server's limit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AccountSlot {
    /// An attachment upload to one of the account's tasks.
    Upload,
    /// An attachment download from one of the account's tasks.
    Download,
    /// A whole-board read the account makes, such as its board page.
    Board,
    /// A calendar feed of one of the account's boards, which anyone holding its token reads.
    Feed,
    /// An open wait for board events by the account, whatever credential it uses.
    Wait,
    /// An open wait for board events by one API token, counted under the token's id where the
    /// other kinds count under an account id.
    TokenWait,
}

impl AccountSlot {
    /// Returns how many requests of this kind one account may run at once.
    #[must_use]
    pub fn per_account(self) -> usize {
        match self {
            Self::Upload => AppState::UPLOADS_PER_ACCOUNT_AT_ONCE,
            Self::Download => AppState::DOWNLOADS_PER_ACCOUNT_AT_ONCE,
            Self::Board => AppState::BOARDS_PER_ACCOUNT_AT_ONCE,
            Self::Feed => AppState::FEEDS_PER_ACCOUNT_AT_ONCE,
            Self::Wait => AppState::WAITS_PER_ACCOUNT_AT_ONCE,
            Self::TokenWait => AppState::WAITS_PER_TOKEN_AT_ONCE,
        }
    }
}

impl AppState {
    /// Requests that may hold a whole board's tasks in memory at once, each up to an account's
    /// stored bytes.
    pub const BOARDS_AT_ONCE: usize = 3;
    /// Requests of one account that may hold a whole board's tasks in memory at once.
    pub const BOARDS_PER_ACCOUNT_AT_ONCE: usize = 1;
    /// Attachment downloads that may run at once, each holding its file in memory.
    pub const DOWNLOADS_AT_ONCE: usize = 8;
    /// Attachment downloads from one account's tasks that may run at once.
    pub const DOWNLOADS_PER_ACCOUNT_AT_ONCE: usize = 2;
    /// Calendar feeds of one account's boards that may be built at once, apart from its own
    /// whole-board reads.
    pub const FEEDS_PER_ACCOUNT_AT_ONCE: usize = 1;
    /// Attachment uploads that may run at once, each holding its file in memory.
    pub const UPLOADS_AT_ONCE: usize = 2;
    /// Attachment uploads to one account's tasks that may run at once.
    pub const UPLOADS_PER_ACCOUNT_AT_ONCE: usize = 1;
    /// Waits for board events one account may hold open at once, holding no thread and no
    /// transaction.
    pub const WAITS_PER_ACCOUNT_AT_ONCE: usize = 8;
    /// Waits for board events one API token may hold open at once.
    pub const WAITS_PER_TOKEN_AT_ONCE: usize = 4;

    /// Builds the state around a database, an optional bootstrap token and the sign-in mode.
    pub fn new(db: Db, bootstrap_token: Option<BootstrapToken>, sign_in: SignIn) -> Self {
        let settings = db.instance_settings().unwrap_or_else(|error| {
            eprintln!("pomb: could not read the instance settings, so defaults apply: {error}");
            InstanceSettings::default()
        });
        let limits = Limits::new();
        limits.apply(&settings.limits);
        let disk = DiskGuard::beside(&db);
        disk.apply(&settings.disk);
        Self {
            backups: Backups::beside(db.path()).with_disk(disk.clone()),
            disk,
            db,
            sign_in,
            mode: InstanceMode::Single,
            device_lockout: DeviceCodeLockout::default(),
            bootstrap_token,
            public_url: None,
            token_touch: TokenTouchThrottle::default(),
            limits,
            trust_proxy: TrustProxy::None,
            uploads: Arc::new(Semaphore::new(Self::UPLOADS_AT_ONCE)),
            downloads: Arc::new(Semaphore::new(Self::DOWNLOADS_AT_ONCE)),
            boards: Arc::new(Semaphore::new(Self::BOARDS_AT_ONCE)),
            account_slots: Arc::default(),
            settings_gate: Arc::new(Mutex::new(())),
            openapi: Arc::default(),
        }
    }

    /// Returns the permits `account` has of `kind`, which hold [`AccountSlot::per_account`].
    #[must_use]
    pub fn account_slot(&self, kind: AccountSlot, account: &str) -> Arc<Semaphore> {
        let mut held = self
            .account_slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            held.entry((kind, account.to_string()))
                .or_insert_with(|| Arc::new(Semaphore::new(kind.per_account()))),
        )
    }

    /// Changes the stored instance settings with `edit` through `instance`, then makes the live
    /// rate and disk limits follow them. Saves run one at a time, so the live limits always match
    /// the last stored ones.
    pub fn save_settings(
        &self,
        instance: &InstanceDb,
        edit: impl FnOnce(&mut InstanceSettings),
    ) -> DbResult<InstanceSettings> {
        let _held = self
            .settings_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = instance.update_settings(edit)?;
        self.limits.apply(&stored.limits);
        self.disk.apply(&stored.disk);
        Ok(stored)
    }

    /// Replaces the request limits, to run them on another clock. They follow the same numbers.
    #[cfg(test)]
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        limits.apply(&self.limits.settings());
        self.limits = limits;
        self
    }

    /// Replaces the disk guard, to measure through another probe. It follows the same limits and
    /// guards the backups too.
    #[cfg(test)]
    #[must_use]
    pub fn with_disk(mut self, disk: DiskGuard) -> Self {
        disk.apply(&self.disk.limits());
        self.backups = Backups::new(self.backups.dir().to_path_buf()).with_disk(disk.clone());
        self.disk = disk;
        self
    }

    /// Writes backups to `dir` instead of the `backups` directory beside the database file.
    #[must_use]
    pub fn with_backup_dir(mut self, dir: PathBuf) -> Self {
        self.backups = Backups::new(dir).with_disk(self.disk.clone());
        self
    }

    /// Serves the instance in `mode`, which the caller has stored and checked against the sign-in.
    #[must_use]
    pub fn with_mode(mut self, mode: InstanceMode) -> Self {
        self.mode = mode;
        self
    }

    /// Returns whether the instance serves the admin plus invited members.
    #[must_use]
    pub fn is_multi(&self) -> bool {
        self.mode == InstanceMode::Multi
    }

    /// Trusts `trust_proxy` to name the client of each request.
    #[must_use]
    pub fn with_trust_proxy(mut self, trust_proxy: TrustProxy) -> Self {
        self.trust_proxy = trust_proxy;
        self
    }

    /// Sets the externally visible base URL, already normalized by the caller.
    #[must_use]
    pub fn with_public_url(mut self, url: Option<String>) -> Self {
        self.public_url = url;
        self.openapi = Arc::default();
        self
    }

    /// Returns the handle for the flows that run before anyone is signed in.
    #[must_use]
    pub fn public(&self) -> PublicDb {
        PublicDb::new(self.db.clone())
    }
}

/// Returns the 500 a handle extractor answers when the request carries no signed-in account.
fn no_signed_in_account() -> Response {
    coded_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::InternalError,
        "no signed-in account",
    )
}

/// The trimmed, non-empty `POMB_BOOTSTRAP_TOKEN` value.
/// Its `Debug` output never shows the secret.
#[derive(Clone)]
pub struct BootstrapToken(String);

impl BootstrapToken {
    /// Wraps a raw value, or returns `None` when it is empty or whitespace.
    pub fn new(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_string()))
    }

    /// Reads `POMB_BOOTSTRAP_TOKEN`, or returns `None` when it is unset or blank.
    pub fn from_env() -> Option<Self> {
        std::env::var("POMB_BOOTSTRAP_TOKEN")
            .ok()
            .and_then(|raw| Self::new(&raw))
    }

    /// Compares a presented value with the token in constant time.
    pub fn matches(&self, raw: &[u8]) -> bool {
        constant_time_eq(raw, self.0.as_bytes())
    }
}

impl std::fmt::Debug for BootstrapToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BootstrapToken(..)")
    }
}

/// Recent wrong device-pairing user codes per client network, kept in memory per server
/// instance; past the cap, code entry and approval from that network refuse until the window
/// moves on.
#[derive(Clone, Debug, Default)]
pub struct DeviceCodeLockout {
    failures: Arc<Mutex<HashMap<IpAddr, Vec<i64>>>>,
}

impl DeviceCodeLockout {
    /// Wrong codes allowed from one client network within the window.
    pub const MAX_FAILURES: usize = 10;
    /// Seconds a wrong code counts against the cap.
    pub const WINDOW_SECONDS: i64 = 600;

    /// True while the window holds the maximum number of wrong codes from `network`.
    pub fn locked(&self, network: IpAddr, now: i64) -> bool {
        self.recent(now)
            .get(&network)
            .is_some_and(|failures| failures.len() >= Self::MAX_FAILURES)
    }

    /// Counts a wrong code from `network` at `now`.
    pub fn record_failure(&self, network: IpAddr, now: i64) {
        self.recent(now).entry(network).or_default().push(now);
    }

    /// Locks the failures after dropping those outside the window.
    fn recent(&self, now: i64) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Vec<i64>>> {
        let mut failures = self
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        failures.retain(|_, at| {
            at.retain(|at| now - at < Self::WINDOW_SECONDS);
            !at.is_empty()
        });
        failures
    }
}

/// Remembers, per API token, when `last_used_at` was last written, so a busy token causes one
/// token-table write per interval instead of one per request. In memory, so a restart allows one
/// extra write per token.
#[derive(Clone, Debug, Default)]
pub struct TokenTouchThrottle {
    written: Arc<Mutex<HashMap<String, i64>>>,
}

impl TokenTouchThrottle {
    /// Seconds between `last_used_at` writes for one token.
    pub const INTERVAL_SECONDS: i64 = 60;

    /// Claims the token's next `last_used_at` write: true (and the write is recorded at `now`)
    /// when none happened in the last interval, false when the caller should skip the write.
    pub fn claim_write(&self, token_id: &str, now: i64) -> bool {
        let mut written = self
            .written
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match written.get(token_id) {
            Some(last) if now - last < Self::INTERVAL_SECONDS => false,
            _ => {
                written.insert(token_id.to_string(), now);
                true
            }
        }
    }
}

/// Read one JSON row by id.
fn read_one<T: DeserializeOwned>(
    db: &Store,
    table: TableDefinition<'_, &str, &[u8]>,
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
    table: TableDefinition<'_, &str, &[u8]>,
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
    db: &Store,
    table: TableDefinition<'_, &str, &[u8]>,
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
fn mutate_task<F>(db: &Store, key: &str, f: F) -> DbResult<bool>
where
    F: FnOnce(&mut TaskRow),
{
    mutate_task_as(db, key, OverQuota::Refuse, f)
}

/// [`mutate_task`] for a change that `over` says whether a quota refuses.
fn mutate_task_as<F>(db: &Store, key: &str, over: OverQuota, f: F) -> DbResult<bool>
where
    F: FnOnce(&mut TaskRow),
{
    let txn = db.begin_write()?;
    {
        let mut tasks = txn.open_table(TASKS)?;
        let Some(mut task) = task_in(&tasks, key)? else {
            return Ok(false);
        };
        f(&mut task);
        task.updated_at = Some(Utc::now().to_rfc3339());
        txn_store_task(&txn, &mut tasks, &task, over)?;
    }
    txn.commit()?;
    Ok(true)
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

/// Reads one JSON row through an open table.
fn row_in<T, R>(tbl: &R, id: &str) -> DbResult<Option<T>>
where
    T: DeserializeOwned,
    R: ReadableTable<&'static str, &'static [u8]>,
{
    Ok(tbl
        .get(id)?
        .map(|guard| serde_json::from_slice(guard.value()))
        .transpose()?)
}

/// A transaction that reads tables: a read transaction, or a write transaction before it commits.
trait Reads {
    /// Opens a table of JSON rows.
    fn rows(&self, table: Rows) -> DbResult<impl ReadableTable<&'static str, &'static [u8]> + '_>;

    /// Opens a multimap index.
    fn index(
        &self,
        index: Index,
    ) -> DbResult<impl ReadableMultimapTable<&'static str, &'static str> + '_>;
}

impl Reads for ReadTransaction {
    fn rows(&self, table: Rows) -> DbResult<impl ReadableTable<&'static str, &'static [u8]> + '_> {
        Ok(self.open_table(table)?)
    }

    fn index(
        &self,
        index: Index,
    ) -> DbResult<impl ReadableMultimapTable<&'static str, &'static str> + '_> {
        Ok(self.open_multimap_table(index)?)
    }
}

impl Reads for WriteTransaction {
    fn rows(&self, table: Rows) -> DbResult<impl ReadableTable<&'static str, &'static [u8]> + '_> {
        Ok(self.open_table(table)?)
    }

    fn index(
        &self,
        index: Index,
    ) -> DbResult<impl ReadableMultimapTable<&'static str, &'static str> + '_> {
        Ok(self.open_multimap_table(index)?)
    }
}

impl Reads for WriteTxn<'_> {
    fn rows(&self, table: Rows) -> DbResult<impl ReadableTable<&'static str, &'static [u8]> + '_> {
        Ok(self.open_table(table)?)
    }

    fn index(
        &self,
        index: Index,
    ) -> DbResult<impl ReadableMultimapTable<&'static str, &'static str> + '_> {
        Ok(self.open_multimap_table(index)?)
    }
}

/// Reads one JSON row by id inside an open transaction.
fn read_in<T: DeserializeOwned>(txn: &impl Reads, table: Rows, id: &str) -> DbResult<Option<T>> {
    row_in(&txn.rows(table)?, id)
}

/// Values stored under `key` in a multimap, read inside an open transaction.
fn index_in(txn: &impl Reads, index: Index, key: &str) -> DbResult<Vec<String>> {
    let tbl = txn.index(index)?;
    let mut out = Vec::new();
    for guard in tbl.get(key)? {
        out.push(guard?.value().to_owned());
    }
    Ok(out)
}

/// Returns the board `id` when `account` owns it, read inside an open transaction.
fn owned_board_in(txn: &impl Reads, account: &AccountId, id: &str) -> DbResult<Option<BoardRow>> {
    Ok(read_in::<BoardRow>(txn, BOARDS, id)?.filter(|board| board.owner_id == account.as_str()))
}

/// Returns whether `account` owns board `id`, read inside an open transaction.
fn owns_board_in(txn: &impl Reads, account: &AccountId, id: &str) -> DbResult<bool> {
    Ok(owned_board_in(txn, account, id)?.is_some())
}

/// Returns the account that owns the board task `id` sits on, read inside an open transaction.
fn task_owner_in(txn: &impl Reads, id: &str) -> DbResult<Option<String>> {
    let Some(task) = read_in::<TaskRow>(txn, TASKS, id)? else {
        return Ok(None);
    };
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &task.column_id)? else {
        return Ok(None);
    };
    Ok(read_in::<BoardRow>(txn, BOARDS, &column.board_id)?.map(|board| board.owner_id))
}

/// Counts the tasks of board `board_id` that `counts` picks, each read as `T`, inside an open
/// transaction.
fn board_task_count_in<T: DeserializeOwned>(
    txn: &impl Reads,
    board_id: &str,
    counts: impl Fn(&T) -> bool,
) -> DbResult<usize> {
    let mut count = 0;
    for column_id in index_in(txn, COLUMNS_BY_BOARD, board_id)? {
        for task_id in index_in(txn, TASKS_BY_COLUMN, &column_id)? {
            if read_in::<T>(txn, TASKS, &task_id)?.is_some_and(|task| counts(&task)) {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Counts the tasks of board `board_id` that carry color `color_id`, read inside an open
/// transaction.
fn board_color_uses_in(txn: &impl Reads, board_id: &str, color_id: &str) -> DbResult<usize> {
    board_task_count_in(txn, board_id, |task: &TaskColor| {
        task.color_id.as_deref() == Some(color_id)
    })
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
/// transaction: the task's board, its field definitions and colors, and the
/// configured pomodoro length.
#[derive(Clone, Debug, Default)]
pub struct TaskContext {
    pub board_id: Option<String>,
    pub fields: Vec<FieldDefRow>,
    /// The board's colors as stored when the change runs.
    pub colors: Vec<ColorRow>,
    pub pomodoro_minutes: u32,
}

fn task_context_in(txn: &WriteTransaction, task: &TaskRow) -> DbResult<TaskContext> {
    let board_id = column_board_in(txn, &task.column_id)?;
    let fields = match &board_id {
        Some(board_id) => board_fields_in(txn, board_id)?,
        None => Vec::new(),
    };
    let colors = match &board_id {
        Some(board_id) => {
            txn_indexed_rows::<ColorRow>(txn, COLORS_BY_BOARD, board_id, TASK_COLORS)?
                .into_iter()
                .map(ColorRow::with_palette_hexes)
                .collect()
        }
        None => Vec::new(),
    };
    let owner = match &board_id {
        Some(board_id) => read_in::<BoardRow>(txn, BOARDS, board_id)?.map(|board| board.owner_id),
        None => None,
    };
    let settings: Option<Settings> = match owner {
        Some(owner) => txn_read_one(txn, ACCOUNT_SETTINGS, &owner)?,
        None => None,
    };
    Ok(TaskContext {
        board_id,
        fields,
        colors,
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
                    let detail = link_event_detail(&link, deleted_name.as_deref(), |detail| {
                        format!("Removed link {detail} (task deleted)")
                    });
                    push_event(&mut holder, "link_removed", detail, Actor::SYSTEM);
                }
                holder.updated_at = Some(Utc::now().to_rfc3339());
                txn_store_task(txn, &mut tasks, &holder, OverQuota::Allow)?;
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
    let mut dependents = Vec::new();
    for holder_id in holder_ids {
        let Some(holder) = read_in::<TaskHead>(txn, TASKS, &holder_id)? else {
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
    let mut open = HashMap::new();
    let mut states = HashMap::new();
    for id in ids {
        let Some(task) = read_in::<TaskHead>(txn, TASKS, id)? else {
            continue;
        };
        states.insert(id.clone(), links_blocked_in(txn, &task.links, &mut open)?);
    }
    Ok(states)
}

/// Returns true when a `blocked_by` link among `links` names a task in a column that is not done,
/// read inside an open transaction. `open` remembers, by task id, whether a named task sits in
/// such a column, so each is read once.
fn links_blocked_in(
    txn: &impl Reads,
    links: &[TaskLink],
    open: &mut HashMap<String, bool>,
) -> DbResult<bool> {
    for link in links.iter().filter(|link| link.kind == LinkKind::BlockedBy) {
        let is_open = match open.get(&link.target_id) {
            Some(is_open) => *is_open,
            None => {
                let column = match read_in::<TaskColumn>(txn, TASKS, &link.target_id)? {
                    Some(target) => read_in::<ColumnRow>(txn, COLUMNS, &target.column_id)?,
                    None => None,
                };
                let is_open = column.is_some_and(|column| !column.is_done);
                open.insert(link.target_id.clone(), is_open);
                is_open
            }
        };
        if is_open {
            return Ok(true);
        }
    }
    Ok(false)
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
            txn_store_task(txn, &mut tasks, &task, OverQuota::Allow)?;
        }
    }
    Ok(())
}

/// Makes `completed_at` agree with the column's done flag for every task in it: stamp those without
/// one when the column is done, clear it when the column is not. `updated_at` is left alone so the
/// ready feed still lists only blocked dependents.
fn restamp_column_tasks(txn: &WriteTransaction, column_id: &str, is_done: bool) -> DbResult<()> {
    let ids = txn_mmap_get(txn, TASKS_BY_COLUMN, column_id)?;
    let mut tasks = txn.open_table(TASKS)?;
    for id in ids {
        let Some(mut task) = task_in(&tasks, &id)? else {
            continue;
        };
        if task.completed_at.is_some() == is_done {
            continue;
        }
        task.completed_at = is_done.then(|| Utc::now().to_rfc3339());
        txn_store_task(txn, &mut tasks, &task, OverQuota::Allow)?;
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

/// Most characters of each text a history event stores: its detail, its actor and its token name.
pub const MAX_HISTORY_TEXT_CHARS: usize = 200;

/// Returns `text` as a history event stores it: whole when it holds at most
/// [`MAX_HISTORY_TEXT_CHARS`] characters, else cut to that many with an ellipsis as the last.
#[must_use]
fn history_text(text: &str) -> String {
    cut_text(text, MAX_HISTORY_TEXT_CHARS)
}

/// Returns `text` whole when it holds at most `max` characters, else cut to that many with an
/// ellipsis as the last. A `max` of zero leaves only the ellipsis.
#[must_use]
fn cut_text(text: &str, max: usize) -> String {
    if text.char_indices().nth(max).is_none() {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Append a history event, dropping the oldest past the cap.
pub fn push_event(task: &mut TaskRow, kind: &str, detail: String, actor: Actor<'_>) {
    push_column_event(task, kind, detail, actor, None);
}

/// [`push_event`] for an event that puts the task in `column_id`, so reports follow the column
/// through renames. Every text the event records is cut to [`MAX_HISTORY_TEXT_CHARS`].
pub fn push_column_event(
    task: &mut TaskRow,
    kind: &str,
    detail: String,
    actor: Actor<'_>,
    column_id: Option<&str>,
) {
    task.history.push(TaskEvent {
        id: Uuid::new_v4().to_string(),
        kind: kind.to_string(),
        detail: history_text(&detail),
        actor: history_text(actor.name),
        created_at: Utc::now().to_rfc3339(),
        column_id: column_id.map(str::to_string),
        token_name: actor.token_name.map(history_text),
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

/// Returns the history detail of a link event, `wrap` applied to the link's own detail such as
/// `blocked_by "Write docs" (<target id>)`. The target's name is cut so the whole detail fits in
/// [`MAX_HISTORY_TEXT_CHARS`] with its id intact.
fn link_event_detail(
    link: &TaskLink,
    target_name: Option<&str>,
    wrap: impl Fn(&str) -> String,
) -> String {
    let kind = match link.kind {
        LinkKind::BlockedBy => "blocked_by",
        LinkKind::Blocks => "blocks",
        LinkKind::Related => "related",
    };
    let id = &link.target_id;
    let Some(name) = target_name else {
        return wrap(&format!("{kind} {id}"));
    };
    let frame = wrap(&format!("{kind} \"\" ({id})")).chars().count();
    let name = cut_text(name, MAX_HISTORY_TEXT_CHARS.saturating_sub(frame));
    wrap(&format!("{kind} \"{name}\" ({id})"))
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
            push_event(task, "claim_expired", detail, Actor::SYSTEM);
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
    table: MultimapTableDefinition<'_, &str, &str>,
    key: &str,
    value: &str,
) -> DbResult<()> {
    let mut tbl = txn.open_multimap_table(table)?;
    tbl.insert(key, value)?;
    Ok(())
}

fn mmap_remove(
    txn: &WriteTransaction,
    table: MultimapTableDefinition<'_, &str, &str>,
    key: &str,
    value: &str,
) -> DbResult<()> {
    let mut tbl = txn.open_multimap_table(table)?;
    tbl.remove(key, value)?;
    Ok(())
}

fn mmap_get(
    db: &Store,
    table: MultimapTableDefinition<'_, &str, &str>,
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

// Read-modify-write paths read through their own write transaction, so no write committed by
// another caller in between is overwritten.

/// Returns whether `owner` has an unarchived board other than `except`, read inside an open write
/// transaction.
fn other_active_board_exists(txn: &WriteTransaction, owner: &str, except: &str) -> DbResult<bool> {
    for board in txn_owner_boards(txn, owner)? {
        if board.id != except && !board.config_bool("archived") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Removes `board` and everything in it inside an open write transaction: columns, swimlanes,
/// tasks with their attachment bytes, time entries, task colors and fields, and its owner's
/// active timer when it runs on one of its tasks.
fn txn_remove_board(txn: &WriteTransaction, board: &BoardRow) -> DbResult<()> {
    let id = board.id.as_str();
    let column_ids = txn_mmap_get(txn, COLUMNS_BY_BOARD, id)?;
    let swimlane_ids = txn_mmap_get(txn, SWIMLANES_BY_BOARD, id)?;
    let color_ids = txn_mmap_get(txn, COLORS_BY_BOARD, id)?;
    let field_ids = txn_mmap_get(txn, FIELDS_BY_BOARD, id)?;
    let mut task_ids: Vec<String> = Vec::new();
    let mut entry_ids: Vec<String> = Vec::new();
    for column_id in &column_ids {
        for task_id in txn_mmap_get(txn, TASKS_BY_COLUMN, column_id)? {
            entry_ids.extend(txn_mmap_get(txn, ENTRIES_BY_TASK, &task_id)?);
            task_ids.push(task_id);
        }
    }
    let freed = txn_task_bytes(txn, &task_ids)?
        + txn_rows_bytes(txn, BOARDS, &[id.to_string()])?
        + txn_rows_bytes(txn, COLUMNS, &column_ids)?
        + txn_rows_bytes(txn, SWIMLANES, &swimlane_ids)?
        + txn_rows_bytes(txn, TASK_COLORS, &color_ids)?
        + txn_rows_bytes(txn, FIELD_DEFS, &field_ids)?;
    txn_remove_attachment_data(txn, &task_ids)?;
    unlink_deleted_tasks(txn, &task_ids)?;
    txn_count_usage(txn, &board.owner_id, 0, freed)?;
    {
        let mut boards = txn.open_table(BOARDS)?;
        boards.remove(id)?;
        mmap_remove(txn, BOARDS_BY_OWNER, &board.owner_id, id)?;
        txn.open_table(BOARD_ACTIVITY)?.remove(id)?;
        if let Some(token) = board.feed_token() {
            txn.open_table(FEED_TOKENS)?
                .remove(feed_token_key(&token).as_str())?;
        }
        let mut columns = txn.open_table(COLUMNS)?;
        for column_id in &column_ids {
            columns.remove(column_id.as_str())?;
            mmap_remove(txn, COLUMNS_BY_BOARD, id, column_id)?;
        }
        let mut swimlanes = txn.open_table(SWIMLANES)?;
        for swimlane_id in &swimlane_ids {
            swimlanes.remove(swimlane_id.as_str())?;
            mmap_remove(txn, SWIMLANES_BY_BOARD, id, swimlane_id)?;
        }
        let mut colors = txn.open_table(TASK_COLORS)?;
        for color_id in &color_ids {
            colors.remove(color_id.as_str())?;
            mmap_remove(txn, COLORS_BY_BOARD, id, color_id)?;
        }
        let mut fields = txn.open_table(FIELD_DEFS)?;
        for field_id in &field_ids {
            fields.remove(field_id.as_str())?;
            mmap_remove(txn, FIELDS_BY_BOARD, id, field_id)?;
        }
        let mut tasks = txn.open_table(TASKS)?;
        let mut entries = txn.open_table(TIME_ENTRIES)?;
        for task_id in &task_ids {
            tasks.remove(task_id.as_str())?;
        }
        for entry_id in &entry_ids {
            txn_remove_entry(txn, &mut entries, entry_id)?;
        }

        // Drop the multimap index entries for tasks/entries
        for column_id in &column_ids {
            let mut idx = txn.open_multimap_table(TASKS_BY_COLUMN)?;
            idx.remove_all(column_id.as_str())?;
        }
        for task_id in &task_ids {
            let mut idx = txn.open_multimap_table(ENTRIES_BY_TASK)?;
            idx.remove_all(task_id.as_str())?;
        }
    }
    txn_remove_events(txn, id)?;
    txn_clear_timer_if(txn, &board.owner_id, |task_id| {
        task_ids.iter().any(|id| id == task_id)
    })
}

/// Removes the stored bytes of every attachment on the tasks `task_ids`, inside an open write
/// transaction.
fn txn_remove_attachment_data(txn: &WriteTransaction, task_ids: &[String]) -> DbResult<()> {
    let mut attachment_ids = Vec::new();
    for task_id in task_ids {
        if let Some(task) = txn_read_one::<TaskRow>(txn, TASKS, task_id)? {
            attachment_ids.extend(task.attachments.into_iter().map(|attachment| attachment.id));
        }
    }
    let mut data = txn.open_table(ATTACHMENT_DATA)?;
    for id in &attachment_ids {
        data.remove(id.as_str())?;
    }
    Ok(())
}

/// Returns every board `owner` owns, read inside an open write transaction.
fn txn_owner_boards(txn: &WriteTransaction, owner: &str) -> DbResult<Vec<BoardRow>> {
    txn_indexed_rows(txn, BOARDS_BY_OWNER, owner, BOARDS)
}

/// Read one JSON row by id inside an open write transaction.
fn txn_read_one<T: DeserializeOwned>(
    txn: &WriteTransaction,
    table: TableDefinition<'_, &str, &[u8]>,
    key: &str,
) -> DbResult<Option<T>> {
    let tbl = txn.open_table(table)?;
    let row = tbl
        .get(key)?
        .map(|guard| serde_json::from_slice(guard.value()))
        .transpose()?;
    Ok(row)
}

/// Returns every row of an open table, in key order.
fn rows_in<T, R>(tbl: &R) -> DbResult<Vec<T>>
where
    T: DeserializeOwned,
    R: ReadableTable<&'static str, &'static [u8]>,
{
    let mut rows = Vec::new();
    for item in tbl.iter()? {
        let (_, guard) = item?;
        rows.push(serde_json::from_slice(guard.value())?);
    }
    Ok(rows)
}

/// Returns the instance's owner from an open users table: its admin, or its first user when no
/// user is an admin.
fn owner_in<R>(tbl: &R) -> DbResult<Option<UserRow>>
where
    R: ReadableTable<&'static str, &'static [u8]>,
{
    let users: Vec<UserRow> = rows_in(tbl)?;
    Ok(users
        .iter()
        .find(|user| user.role == AccountRole::Admin)
        .or(users.first())
        .cloned())
}

/// Stores a user row and its username index entry. The instance's first user claims every
/// ownerless row in the same transaction.
fn txn_insert_user(txn: &WriteTransaction, user: &UserRow) -> DbResult<()> {
    let first = txn.open_table(USERS)?.is_empty()?;
    write_one(txn, USERS, &user.id, user)?;
    txn.open_table(USERNAMES)?
        .insert(user.username.as_str(), user.id.as_str())?;
    if first {
        claim_ownerless_rows(txn, &user.account_id())?;
    }
    Ok(())
}

/// Returns whether a stored passkey already holds the credential of `passkey`, read inside an
/// open write transaction.
fn txn_credential_registered(txn: &WriteTransaction, passkey: &PasskeyRow) -> DbResult<bool> {
    let key = credential_key(&passkey.passkey()?);
    Ok(txn
        .open_table(PASSKEYS_BY_CREDENTIAL)?
        .get(key.as_str())?
        .is_some())
}

/// Returns the key of [`PASSKEYS_BY_CREDENTIAL`] that `credential` sits under.
fn credential_key(credential: &Passkey) -> String {
    URL_SAFE_NO_PAD.encode(credential.cred_id().as_slice())
}

/// Stores `passkey` and indexes it under its user, inside an open write transaction. Refuses, with
/// [`QuotaExceeded`], a passkey past [`MAX_PASSKEYS_PER_ACCOUNT`].
fn txn_store_passkey(txn: &WriteTransaction, passkey: &PasskeyRow) -> DbResult<()> {
    let held = txn
        .open_multimap_table(PASSKEYS_BY_USER)?
        .get(passkey.user_id.as_str())?
        .len();
    if held as usize >= MAX_PASSKEYS_PER_ACCOUNT {
        return Err(Box::new(QuotaExceeded {
            bound: Bound::Passkeys,
            limit: MAX_PASSKEYS_PER_ACCOUNT as u64,
        }));
    }
    txn_insert_passkey(txn, passkey)
}

/// Stores `passkey` and indexes it under its user, inside an open write transaction, past
/// [`MAX_PASSKEYS_PER_ACCOUNT`], for a recovery that replaces lost passkeys.
fn txn_insert_passkey(txn: &WriteTransaction, passkey: &PasskeyRow) -> DbResult<()> {
    write_one(txn, PASSKEYS, &passkey.id, passkey)?;
    txn.open_table(PASSKEYS_BY_CREDENTIAL)?.insert(
        credential_key(&passkey.passkey()?).as_str(),
        passkey.id.as_str(),
    )?;
    mmap_insert(txn, PASSKEYS_BY_USER, &passkey.user_id, &passkey.id)
}

/// Removes the passkey `id` from [`PASSKEYS`] and [`PASSKEYS_BY_CREDENTIAL`], inside an open write
/// transaction. A row whose credential does not parse has no credential entry to remove.
fn txn_remove_passkey(txn: &WriteTransaction, id: &str) -> DbResult<()> {
    let credential = txn
        .open_table(PASSKEYS)?
        .remove(id)?
        .and_then(|guard| serde_json::from_slice::<PasskeyRow>(guard.value()).ok())
        .and_then(|row| row.passkey().ok());
    if let Some(credential) = credential {
        txn.open_table(PASSKEYS_BY_CREDENTIAL)?
            .remove(credential_key(&credential).as_str())?;
    }
    Ok(())
}

/// Removes every passkey of `user`, inside an open write transaction.
fn txn_remove_passkeys_of(txn: &WriteTransaction, user: &str) -> DbResult<()> {
    for id in txn_mmap_get(txn, PASSKEYS_BY_USER, user)? {
        txn_remove_passkey(txn, &id)?;
    }
    txn.open_multimap_table(PASSKEYS_BY_USER)?
        .remove_all(user)?;
    Ok(())
}

/// Rebuilds [`PASSKEYS_BY_USER`] from the passkey rows, inside an open write transaction.
fn txn_reindex_passkeys(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(PASSKEYS_BY_USER)?;
    let rows: Vec<PasskeyRow> = rows_in(&txn.open_table(PASSKEYS)?)?;
    let mut index = txn.open_multimap_table(PASSKEYS_BY_USER)?;
    for row in &rows {
        index.insert(row.user_id.as_str(), row.id.as_str())?;
    }
    Ok(())
}

/// Rebuilds [`PASSKEYS_BY_CREDENTIAL`] from the passkey rows, inside an open write transaction. A
/// row whose credential does not parse gets no entry.
fn txn_reindex_credentials(txn: &WriteTransaction) -> DbResult<()> {
    let rows: Vec<PasskeyRow> = rows_in(&txn.open_table(PASSKEYS)?)?;
    let mut index = txn.open_table(PASSKEYS_BY_CREDENTIAL)?;
    index.retain(|_, _| false)?;
    for row in &rows {
        if let Ok(credential) = row.passkey() {
            index.insert(credential_key(&credential).as_str(), row.id.as_str())?;
        }
    }
    Ok(())
}

/// Rebuilds [`TOKENS_BY_ACCOUNT`] from the live and suspended tokens, inside an open write
/// transaction.
fn txn_reindex_tokens(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(TOKENS_BY_ACCOUNT)?;
    let mut index = txn.open_multimap_table(TOKENS_BY_ACCOUNT)?;
    for table in [API_TOKENS, SUSPENDED_TOKENS] {
        for token in rows_in::<ApiTokenRecord, _>(&txn.open_table(table)?)? {
            index.insert(token.account_id.as_str(), token.id.as_str())?;
        }
    }
    Ok(())
}

/// Rebuilds [`TOKEN_IDS_BY_HASH`] and [`SALTED_TOKENS_BY_LAST4`] from the live and suspended
/// tokens, inside an open write transaction.
fn txn_reindex_token_keys(txn: &WriteTransaction) -> DbResult<()> {
    txn.open_table(TOKEN_IDS_BY_HASH)?.retain(|_, _| false)?;
    txn.delete_multimap_table(SALTED_TOKENS_BY_LAST4)?;
    for table in [API_TOKENS, SUSPENDED_TOKENS] {
        for token in rows_in::<ApiTokenRecord, _>(&txn.open_table(table)?)? {
            txn_index_token_key(txn, &token)?;
        }
    }
    Ok(())
}

/// Rebuilds [`DEVICES_BY_USER_CODE`] from the device requests, inside an open write transaction.
fn txn_reindex_device_codes(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(DEVICES_BY_USER_CODE)?;
    let mut index = txn.open_multimap_table(DEVICES_BY_USER_CODE)?;
    let tbl = txn.open_table(DEVICE_AUTHS)?;
    for item in tbl.iter()? {
        let (key, raw) = item?;
        let request: DeviceAuthRow = serde_json::from_slice(raw.value())?;
        index.insert(request.user_code.as_str(), key.value())?;
    }
    Ok(())
}

/// Rebuilds [`DEVICES_BY_EXPIRY`] from the device requests, inside an open write transaction.
fn txn_reindex_device_expiry(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(DEVICES_BY_EXPIRY)?;
    let mut index = txn.open_multimap_table(DEVICES_BY_EXPIRY)?;
    let tbl = txn.open_table(DEVICE_AUTHS)?;
    for item in tbl.iter()? {
        let (key, raw) = item?;
        let request: DeviceAuthRow = serde_json::from_slice(raw.value())?;
        index.insert(request.expires_at, key.value())?;
    }
    Ok(())
}

/// Rebuilds [`SESSIONS_BY_USER`] from the sessions, inside an open write transaction. A session
/// that does not parse is deleted.
fn txn_reindex_sessions(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(SESSIONS_BY_USER)?;
    let mut index = txn.open_multimap_table(SESSIONS_BY_USER)?;
    txn.open_table(SESSIONS)?.retain(|key, raw| {
        let Ok(session) = serde_json::from_slice::<SessionRow>(raw) else {
            return false;
        };
        index.insert(session.user_id.as_str(), key).is_ok()
    })?;
    Ok(())
}

/// Rebuilds [`DEVICES_BY_ACCOUNT`] from the device requests that name an account, inside an open
/// write transaction.
fn txn_reindex_devices(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(DEVICES_BY_ACCOUNT)?;
    let mut index = txn.open_multimap_table(DEVICES_BY_ACCOUNT)?;
    let tbl = txn.open_table(DEVICE_AUTHS)?;
    for item in tbl.iter()? {
        let (key, raw) = item?;
        let request: DeviceAuthRow = serde_json::from_slice(raw.value())?;
        if let Some(account) = request.account_id {
            index.insert(account.as_str(), key.value())?;
        }
    }
    Ok(())
}

/// Stores `token` and indexes it under its account and its key, inside an open write
/// transaction.
fn txn_store_token(txn: &WriteTransaction, token: &ApiTokenRecord) -> DbResult<()> {
    write_one(txn, API_TOKENS, &token.id, token)?;
    mmap_insert(txn, TOKENS_BY_ACCOUNT, &token.account_id, &token.id)?;
    txn_index_token_key(txn, token)
}

/// Indexes `token` by its SHA-256, or by its last four characters while it is under a salted
/// hash, inside an open write transaction.
fn txn_index_token_key(txn: &WriteTransaction, token: &ApiTokenRecord) -> DbResult<()> {
    if token.salt.is_empty() {
        txn.open_table(TOKEN_IDS_BY_HASH)?
            .insert(token.token_hash.as_str(), token.id.as_str())?;
        Ok(())
    } else {
        mmap_insert(txn, SALTED_TOKENS_BY_LAST4, &token.last4, &token.id)
    }
}

/// Removes the index entry [`txn_index_token_key`] made for `token`, inside an open write
/// transaction.
fn txn_unindex_token_key(txn: &WriteTransaction, token: &ApiTokenRecord) -> DbResult<()> {
    if token.salt.is_empty() {
        txn.open_table(TOKEN_IDS_BY_HASH)?
            .remove(token.token_hash.as_str())?;
        Ok(())
    } else {
        mmap_remove(txn, SALTED_TOKENS_BY_LAST4, &token.last4, &token.id)
    }
}

/// Deletes the token `id` of `account` from [`API_TOKENS`] and [`SUSPENDED_TOKENS`] and its index
/// entries, inside an open write transaction, and returns whether a live token was deleted.
fn txn_remove_token(txn: &WriteTransaction, account: &str, id: &str) -> DbResult<bool> {
    let live = txn
        .open_table(API_TOKENS)?
        .remove(id)?
        .map(|guard| serde_json::from_slice::<ApiTokenRecord>(guard.value()))
        .transpose()?;
    let suspended = txn
        .open_table(SUSPENDED_TOKENS)?
        .remove(id)?
        .map(|guard| serde_json::from_slice::<ApiTokenRecord>(guard.value()))
        .transpose()?;
    if let Some(token) = live.as_ref().or(suspended.as_ref()) {
        txn_unindex_token_key(txn, token)?;
    }
    mmap_remove(txn, TOKENS_BY_ACCOUNT, account, id)?;
    Ok(live.is_some())
}

/// Deletes every token of `account`, live or suspended, from [`API_TOKENS`], [`SUSPENDED_TOKENS`]
/// and the index, inside an open write transaction, and returns how many live ones it deleted.
fn txn_remove_tokens_of(txn: &WriteTransaction, account: &str) -> DbResult<u64> {
    let mut removed = 0;
    for id in txn_mmap_get(txn, TOKENS_BY_ACCOUNT, account)? {
        removed += u64::from(txn_remove_token(txn, account, &id)?);
    }
    Ok(removed)
}

/// Ends every session of `user` except the one under the key `keep`, inside an open write
/// transaction, and returns how many it ended.
fn txn_end_sessions_of(txn: &WriteTransaction, user: &str, keep: Option<&str>) -> DbResult<u64> {
    let mut ended = 0;
    for key in txn_mmap_get(txn, SESSIONS_BY_USER, user)? {
        if Some(key.as_str()) == keep {
            continue;
        }
        txn.open_table(SESSIONS)?.remove(key.as_str())?;
        mmap_remove(txn, SESSIONS_BY_USER, user, &key)?;
        ended += 1;
    }
    Ok(ended)
}

/// Returns `user`'s passkeys, oldest first, read inside an open transaction.
fn passkeys_of_in(txn: &impl Reads, user: &str) -> DbResult<Vec<PasskeyRow>> {
    let mut rows = Vec::new();
    for id in index_in(txn, PASSKEYS_BY_USER, user)? {
        rows.extend(read_in::<PasskeyRow>(txn, PASSKEYS, &id)?);
    }
    rows.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    Ok(rows)
}

/// Returns the key of [`FEED_TOKENS`] that the calendar-feed token `raw` sits under.
#[must_use]
fn feed_token_key(raw: &str) -> String {
    hash_feed_token(raw)
}

/// Stores `board` under its id, inside an open write transaction, keeping [`FEED_TOKENS`] in step
/// with its calendar-feed token, and counts the change in its size against its owner.
fn txn_store_board(txn: &WriteTransaction, board: &BoardRow, over: OverQuota) -> DbResult<()> {
    let held = read_in::<BoardRow>(txn, BOARDS, &board.id)?.and_then(|held| held.feed_token());
    txn_store_owned(txn, BOARDS, &board.id, board, over)?;
    let token = board.feed_token();
    if held == token {
        return Ok(());
    }
    let mut tokens = txn.open_table(FEED_TOKENS)?;
    if let Some(held) = held {
        tokens.remove(feed_token_key(&held).as_str())?;
    }
    if let Some(token) = token {
        tokens.insert(feed_token_key(&token).as_str(), board.id.as_str())?;
    }
    Ok(())
}

/// Rebuilds [`FEED_TOKENS`] from the boards, inside an open write transaction.
fn txn_reindex_feed_tokens(txn: &WriteTransaction) -> DbResult<()> {
    let boards: Vec<BoardRow> = rows_in(&txn.open_table(BOARDS)?)?;
    let mut tokens = txn.open_table(FEED_TOKENS)?;
    tokens.retain(|_, _| false)?;
    for board in &boards {
        if let Some(token) = board.feed_token() {
            tokens.insert(feed_token_key(&token).as_str(), board.id.as_str())?;
        }
    }
    Ok(())
}

/// Returns whether `session` is older than [`SESSION_MAX_AGE_SECONDS`] at `now`. A session whose
/// start does not parse counts as expired.
fn session_expired(session: &SessionRow, now: i64) -> bool {
    DateTime::parse_from_rfc3339(&session.created_at)
        .ok()
        .is_none_or(|created| now - created.timestamp() >= i64::from(SESSION_MAX_AGE_SECONDS))
}

/// Deletes every session older than [`SESSION_MAX_AGE_SECONDS`] at `now`, inside an open write
/// transaction, and returns how many it deleted. A row that does not parse is deleted too.
fn txn_sweep_sessions(txn: &WriteTransaction, now: i64) -> DbResult<u64> {
    let mut ended = Vec::new();
    txn.open_table(SESSIONS)?.retain(|token, raw| {
        let user = match serde_json::from_slice::<SessionRow>(raw) {
            Ok(row) if session_expired(&row, now) => Some(row.user_id),
            Ok(_) => return true,
            Err(_) => None,
        };
        ended.push((token.to_owned(), user));
        false
    })?;
    for (token, user) in &ended {
        if let Some(user) = user {
            mmap_remove(txn, SESSIONS_BY_USER, user, token)?;
        }
    }
    Ok(ended.len() as u64)
}

/// What the stored device-pairing requests say about a new one at some moment.
struct PendingCount {
    /// Keys of the requests past their retention, each with its row.
    stale: Vec<(String, DeviceAuthRow)>,
    /// Live requests that wait for approval.
    total: usize,
    /// Live requests that wait for approval from the new one's client network.
    here: usize,
    /// Whether a live request already holds the new one's user code.
    code_taken: bool,
}

/// Counts the requests in the open device table `tbl` that bear on storing `row` at `now`, finding
/// them through [`DEVICES_BY_EXPIRY`] instead of reading the whole table.
fn count_pending<T>(
    txn: &WriteTransaction,
    tbl: &T,
    row: &DeviceAuthRow,
    now: i64,
) -> DbResult<PendingCount>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    let mut count = PendingCount {
        stale: Vec::new(),
        total: 0,
        here: 0,
        code_taken: false,
    };
    let expiry = txn.open_multimap_table(DEVICES_BY_EXPIRY)?;
    for item in expiry.range(..now - Db::DEVICE_AUTH_RETENTION_SECONDS)? {
        for key in item?.1 {
            let key = key?.value().to_owned();
            if let Some(existing) = row_in::<DeviceAuthRow, _>(tbl, &key)? {
                count.stale.push((key, existing));
            }
        }
    }
    for item in expiry.range(now..)? {
        for key in item?.1 {
            let Some(existing) = row_in::<DeviceAuthRow, _>(tbl, key?.value())? else {
                continue;
            };
            count.code_taken |= existing.user_code == row.user_code;
            if existing.status == DeviceAuthStatus::Pending {
                count.total += 1;
                if existing.client_network.is_some()
                    && existing.client_network == row.client_network
                {
                    count.here += 1;
                }
            }
        }
    }
    Ok(count)
}

/// Adds the device request under `key` to [`DEVICES_BY_EXPIRY`] at `expires_at`, inside an open
/// write transaction.
fn txn_index_device_expiry(txn: &WriteTransaction, expires_at: i64, key: &str) -> DbResult<()> {
    txn.open_multimap_table(DEVICES_BY_EXPIRY)?
        .insert(expires_at, key)?;
    Ok(())
}

/// Removes the device request under `key` from [`DEVICES_BY_EXPIRY`], inside an open write
/// transaction.
fn txn_unindex_device_expiry(txn: &WriteTransaction, expires_at: i64, key: &str) -> DbResult<()> {
    txn.open_multimap_table(DEVICES_BY_EXPIRY)?
        .remove(expires_at, key)?;
    Ok(())
}

/// Error text when a write for the ownerless id arrives after the instance's first account
/// claimed every ownerless row.
pub const OWNER_EXISTS: &str = "this instance has an owner now; sign in as that account";

/// Error text when a write arrives for an account deleted since the request began.
pub const ACCOUNT_GONE: &str = "this account no longer exists";

/// Refuses a write that would store a row for the ownerless id once a user exists, or for an
/// account that no longer exists, inside that write's transaction, so no row is ever stranded.
fn txn_refuse_stranded(txn: &WriteTransaction, owner: &AccountId) -> DbResult<()> {
    let users = txn.open_table(USERS)?;
    if owner.is_ownerless() {
        if !users.is_empty()? {
            return Err(OWNER_EXISTS.into());
        }
    } else if users.get(owner.as_str())?.is_none() {
        return Err(ACCOUNT_GONE.into());
    }
    Ok(())
}

/// Error text when a token would be minted for a pending or suspended account.
const ACCOUNT_NOT_ACTIVE: &str = "this account is pending or suspended, so it holds no tokens";

/// Returns whether `account` exists and is active, read inside an open write transaction. The
/// ownerless id counts as active until the first account claims its rows.
fn txn_account_active(txn: &WriteTransaction, account: &AccountId) -> DbResult<bool> {
    if account.is_ownerless() {
        return Ok(txn.open_table(USERS)?.is_empty()?);
    }
    Ok(txn_read_one::<UserRow>(txn, USERS, account.as_str())?
        .is_some_and(|user| user.status == AccountStatus::Active))
}

/// Returns the stored instance settings, or those of a single-mode instance nobody has edited when
/// none are stored, read inside an open write transaction.
fn txn_instance_settings(txn: &WriteTransaction) -> DbResult<InstanceSettings> {
    let stored: Option<InstanceSettings> = txn_read_one(txn, SETTINGS, INSTANCE_SETTINGS_KEY)?;
    Ok(stored.unwrap_or_default())
}

/// A limit a write may run into: one of the admin's [`Quota`]s or a fixed bound.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    /// A quota the admin sets.
    Quota(Quota),
    /// The API tokens one account may hold, [`MAX_API_TOKENS_PER_ACCOUNT`].
    ApiTokens,
    /// The bytes one task's stored row may take, [`MAX_TASK_ROW_BYTES`].
    TaskRowBytes,
    /// The bytes one field definition's stored row may take, [`MAX_FIELD_DEF_ROW_BYTES`].
    FieldDefRowBytes,
    /// The bytes one saved template's stored row may take, [`MAX_TEMPLATE_ROW_BYTES`].
    TemplateRowBytes,
    /// The time entries one account may start on one UTC day, [`entries::MAX_ENTRIES_PER_DAY`].
    EntriesPerDay,
    /// The passkeys one account may hold, [`MAX_PASSKEYS_PER_ACCOUNT`].
    Passkeys,
}

impl Bound {
    /// Returns what the limit counts, as a refusal words it after the number.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Quota(Quota::BoardsPerAccount) => "boards per account",
            Self::Quota(Quota::TasksPerBoard) => "tasks per board",
            Self::Quota(Quota::CommentsPerTask) => "comments per task",
            Self::Quota(Quota::AttachmentsPerTask) => "attachments per task",
            Self::Quota(Quota::SubtasksPerTask) => "subtasks per task",
            Self::Quota(Quota::TemplatesPerAccount) => "saved templates per account",
            Self::Quota(Quota::StoredMibPerAccount) => "MiB stored per account",
            Self::Quota(Quota::ColumnsPerBoard) => "columns per board",
            Self::Quota(Quota::SwimlanesPerBoard) => "swimlanes per board",
            Self::Quota(Quota::FieldsPerBoard) => "custom fields per board",
            Self::Quota(Quota::TimeEntriesPerAccount) => "time entries per account",
            Self::ApiTokens => "API tokens per account",
            Self::TaskRowBytes => "bytes in one task",
            Self::FieldDefRowBytes => "bytes in one field definition",
            Self::TemplateRowBytes => "bytes in one saved template",
            Self::EntriesPerDay => "time entries started per day",
            Self::Passkeys => "passkeys per account",
        }
    }
}

/// Refusal of a write that would take an account past one of its quotas or a fixed bound.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct QuotaExceeded {
    /// The limit the write would break.
    pub bound: Bound,
    /// What the limit allows.
    pub limit: u64,
}

impl QuotaExceeded {
    /// Returns the refusal of a write past `quota`, which allows `limit`.
    #[must_use]
    pub fn of(quota: Quota, limit: u32) -> Self {
        Self {
            bound: Bound::Quota(quota),
            limit: u64::from(limit),
        }
    }
}

impl fmt::Display for QuotaExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "limit reached: at most {} {}",
            self.limit,
            self.bound.describe()
        )
    }
}

impl Error for QuotaExceeded {}

/// Refusal to open a session for an account the admin suspended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SessionRefused;

impl fmt::Display for SessionRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the admin suspended this account")
    }
}

impl Error for SessionRefused {}

/// Most API tokens one account may hold, the admin's included.
pub const MAX_API_TOKENS_PER_ACCOUNT: usize = 50;

/// Most passkeys one account may hold, the admin's included.
pub const MAX_PASSKEYS_PER_ACCOUNT: usize = 10;

/// Most bytes one task's stored row may take, so reading or rewriting a task stays cheap.
pub const MAX_TASK_ROW_BYTES: usize = 1_048_576;

/// Most bytes one field definition's stored row may take, 256 KiB.
pub const MAX_FIELD_DEF_ROW_BYTES: usize = 262_144;

/// Most bytes one saved template's stored row may take, 8 MiB.
pub const MAX_TEMPLATE_ROW_BYTES: usize = 8_388_608;

/// Returns the refusal of a token for `account` while it holds [`MAX_API_TOKENS_PER_ACCOUNT`],
/// read inside that write's transaction.
fn txn_token_cap_reached(txn: &WriteTransaction, account: &str) -> DbResult<Option<QuotaExceeded>> {
    let held = txn_mmap_get(txn, TOKENS_BY_ACCOUNT, account)?.len();
    Ok(
        (held >= MAX_API_TOKENS_PER_ACCOUNT).then_some(QuotaExceeded {
            bound: Bound::ApiTokens,
            limit: MAX_API_TOKENS_PER_ACCOUNT as u64,
        }),
    )
}

/// Returns the quotas `owner` is held to, read inside an open write transaction. Single mode, the
/// admin and the ownerless id have none.
fn txn_member_quotas(txn: &WriteTransaction, owner: &str) -> DbResult<Option<Quotas>> {
    let settings = txn_instance_settings(txn)?;
    if settings.mode != InstanceMode::Multi {
        return Ok(None);
    }
    let member = txn_read_one::<UserRow>(txn, USERS, owner)?
        .is_some_and(|user| user.role != AccountRole::Admin);
    Ok(member.then_some(settings.quotas))
}

/// Refuses, with [`QuotaExceeded`], a write that leaves `owner` holding `after` items of `quota`,
/// inside that write's transaction. Single mode, the admin and the ownerless id have no quotas.
fn txn_refuse_over_quota(
    txn: &WriteTransaction,
    owner: &str,
    quota: Quota,
    after: usize,
) -> DbResult<()> {
    let Some(quotas) = txn_member_quotas(txn, owner)? else {
        return Ok(());
    };
    let limit = quotas.limit(quota);
    if after > limit as usize {
        return Err(Box::new(QuotaExceeded::of(quota, limit)));
    }
    Ok(())
}

/// Refuses, with [`QuotaExceeded`], a write that leaves board `board_id` holding `after` items of
/// the per-board `quota`.
fn txn_refuse_board_over_quota(
    txn: &WriteTransaction,
    board_id: &str,
    quota: Quota,
    after: usize,
) -> DbResult<()> {
    let Some(board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(());
    };
    txn_refuse_over_quota(txn, &board.owner_id, quota, after)
}

/// Returns how many tasks board `board_id` holds, read inside an open write transaction.
fn txn_board_task_count(txn: &WriteTransaction, board_id: &str) -> DbResult<usize> {
    let mut count = 0;
    for column in txn_mmap_get(txn, COLUMNS_BY_BOARD, board_id)? {
        count += txn_mmap_get(txn, TASKS_BY_COLUMN, &column)?.len();
    }
    Ok(count)
}

/// Refuses a write into board `board_id` when the board is gone or its owner may not write, inside
/// that write's transaction.
fn txn_refuse_stranded_board(txn: &WriteTransaction, board_id: &str) -> DbResult<()> {
    let board: BoardRow = txn_read_one(txn, BOARDS, board_id)?
        .ok_or_else(|| format!("board {board_id} not found"))?;
    txn_refuse_stranded(txn, &AccountId::new(board.owner_id))
}

/// Returns a new active account named `username`.
fn new_user(username: &str, role: AccountRole) -> UserRow {
    UserRow {
        id: Uuid::new_v4().to_string(),
        username: username.to_string(),
        role,
        status: AccountStatus::Active,
        created_at: Utc::now().timestamp(),
    }
}

/// Values stored under `key` in a multimap, read inside an open write
/// transaction.
fn txn_mmap_get(
    txn: &WriteTransaction,
    table: MultimapTableDefinition<'_, &str, &str>,
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
    index: MultimapTableDefinition<'_, &str, &str>,
    key: &str,
    table: TableDefinition<'_, &str, &[u8]>,
) -> DbResult<Vec<T>> {
    let mut rows = Vec::new();
    for id in txn_mmap_get(txn, index, key)? {
        if let Some(row) = txn_read_one(txn, table, &id)? {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Drops `account`'s active timer when it runs on a task `is_removed` says is being deleted,
/// inside that deletion's write transaction.
fn txn_clear_timer_if(
    txn: &WriteTransaction,
    account: &str,
    is_removed: impl Fn(&str) -> bool,
) -> DbResult<()> {
    let timer: Option<ActiveTimer> = txn_read_one(txn, ACTIVE_TIMER, account)?;
    if timer
        .and_then(|timer| timer.task_id)
        .is_some_and(|id| is_removed(&id))
    {
        txn.open_table(ACTIVE_TIMER)?.remove(account)?;
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
        txn_store_task(txn, &mut tbl, &task, OverQuota::Allow)?;
    }
    Ok(())
}

/// Next free position after `positions`, or 0 when there are none.
fn next_position(positions: impl Iterator<Item = i64>) -> i64 {
    positions.max().map(|max| max + 1).unwrap_or(0)
}

/// Insert a color row for a standard value inside an open write
/// transaction; returns its id.
fn txn_insert_color(
    txn: &WriteTransaction,
    board_id: &str,
    color: NewColor<'_>,
    over: OverQuota,
) -> DbResult<String> {
    txn_refuse_stranded_board(txn, board_id)?;
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
    txn_store_owned(txn, TASK_COLORS, &id, &row, over)?;
    mmap_insert(txn, COLORS_BY_BOARD, board_id, &id)?;
    Ok(id)
}

/// A board's colors in sort order, read inside an open write transaction.
fn txn_board_colors(txn: &WriteTransaction, board_id: &str) -> DbResult<Vec<ColorRow>> {
    let mut colors: Vec<ColorRow> = txn_indexed_rows(txn, COLORS_BY_BOARD, board_id, TASK_COLORS)?;
    colors.sort_by(|a, b| {
        a.sort_order
            .cmp(&b.sort_order)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(colors)
}

/// Give an existing board without colors the standard palette inside an
/// open write transaction. Returns true when it wrote rows.
fn txn_ensure_board_colors(txn: &WriteTransaction, board_id: &str) -> DbResult<bool> {
    if !txn_mmap_get(txn, COLORS_BY_BOARD, board_id)?.is_empty()
        || txn_read_one::<BoardRow>(txn, BOARDS, board_id)?.is_none()
    {
        return Ok(false);
    }
    txn_insert_color_specs(txn, board_id, standard_color_specs())?;
    Ok(true)
}

/// Inserts one color row per spec into a board inside an open write transaction.
fn txn_insert_color_specs(
    txn: &WriteTransaction,
    board_id: &str,
    specs: Vec<(&'static str, &'static str, bool, bool, i64)>,
) -> DbResult<()> {
    for (value, label, enabled, is_default, sort_order) in specs {
        txn_insert_color(
            txn,
            board_id,
            NewColor {
                value,
                label,
                description: "",
                enabled,
                is_default,
                sort_order,
            },
            OverQuota::Allow,
        )?;
    }
    Ok(())
}

/// Promote the first enabled color (else the first color) to default when
/// a board has none, inside an open write transaction.
fn txn_ensure_single_default(txn: &WriteTransaction, board_id: &str) -> DbResult<()> {
    let colors = txn_board_colors(txn, board_id)?;
    if colors.iter().any(|color| color.is_default) {
        return Ok(());
    }
    if let Some(first) = colors
        .iter()
        .find(|color| color.enabled)
        .or_else(|| colors.first())
    {
        let promoted = ColorRow {
            is_default: true,
            ..first.clone()
        };
        txn_store_owned(txn, TASK_COLORS, &promoted.id, &promoted, OverQuota::Allow)?;
    }
    Ok(())
}

/// Append a column to a board inside an open write transaction; returns
/// its id.
fn txn_insert_column(
    txn: &WriteTransaction,
    board_id: &str,
    name: &str,
    wip_limit: Option<i64>,
    is_done: bool,
    over: OverQuota,
) -> DbResult<String> {
    TextCap::COLUMN_NAME.check(name)?;
    txn_refuse_stranded_board(txn, board_id)?;
    let columns: Vec<ColumnRow> = txn_indexed_rows(txn, COLUMNS_BY_BOARD, board_id, COLUMNS)?;
    txn_refuse_board_over_quota(txn, board_id, Quota::ColumnsPerBoard, columns.len() + 1)?;
    let id = Uuid::new_v4().to_string();
    let row = ColumnRow {
        id: id.clone(),
        board_id: board_id.to_string(),
        name: name.to_string(),
        position: next_position(columns.iter().map(|column| column.position)),
        wip_limit,
        is_done,
        description: String::new(),
        collapsed: false,
        config_json: "{}".to_string(),
        former_names: Vec::new(),
    };
    txn_store_owned(txn, COLUMNS, &id, &row, over)?;
    mmap_insert(txn, COLUMNS_BY_BOARD, board_id, &id)?;
    Ok(id)
}

/// Appends a swimlane to a board inside an open write transaction and returns its id. A board's
/// first lane becomes its main lane.
fn txn_insert_swimlane(
    txn: &WriteTransaction,
    board_id: &str,
    name: &str,
    over: OverQuota,
) -> DbResult<String> {
    TextCap::SWIMLANE_NAME.check(name)?;
    txn_refuse_stranded_board(txn, board_id)?;
    let lanes: Vec<SwimlaneRow> = txn_indexed_rows(txn, SWIMLANES_BY_BOARD, board_id, SWIMLANES)?;
    txn_refuse_board_over_quota(txn, board_id, Quota::SwimlanesPerBoard, lanes.len() + 1)?;
    let id = Uuid::new_v4().to_string();
    let row = SwimlaneRow {
        id: id.clone(),
        board_id: board_id.to_string(),
        name: name.to_string(),
        position: next_position(lanes.iter().map(|lane| lane.position)),
        description: String::new(),
        limits: Vec::new(),
        collapsed: false,
        height: None,
    };
    txn_store_owned(txn, SWIMLANES, &id, &row, over)?;
    mmap_insert(txn, SWIMLANES_BY_BOARD, board_id, &id)?;
    if lanes.is_empty() {
        txn_merge_board_config(txn, board_id, main_swimlane_entry(&id))?;
    }
    Ok(id)
}

/// Moves swimlane `id` to `position` in its board's order inside an open write transaction.
/// Positions are renumbered densely. Returns false when the swimlane is unknown.
fn txn_move_swimlane(txn: &WriteTransaction, id: &str, position: i64) -> DbResult<bool> {
    let Some(lane) = txn_read_one::<SwimlaneRow>(txn, SWIMLANES, id)? else {
        return Ok(false);
    };
    let mut lanes: Vec<SwimlaneRow> =
        txn_indexed_rows(txn, SWIMLANES_BY_BOARD, &lane.board_id, SWIMLANES)?;
    lanes.retain(|lane| lane.id != id);
    lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
    let at = position.clamp(0, lanes.len() as i64) as usize;
    lanes.insert(at, lane);
    for (index, mut row) in lanes.into_iter().enumerate() {
        row.position = index as i64;
        txn_store_owned(txn, SWIMLANES, &row.id, &row, OverQuota::Allow)?;
    }
    Ok(true)
}

/// Merge `updates` into a board's config bag inside an open write
/// transaction; an unknown board is left alone.
fn txn_merge_board_config(
    txn: &WriteTransaction,
    board_id: &str,
    updates: serde_json::Map<String, serde_json::Value>,
) -> DbResult<()> {
    let Some(mut board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(());
    };
    let mut cfg: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&board.config_json).unwrap_or_default();
    cfg.extend(updates);
    board.config_json = serde_json::to_string(&cfg)?;
    txn_store_board(txn, &board, OverQuota::Allow)
}

/// Inserts a bare board row for `owner` inside an open write transaction; returns its id.
fn txn_insert_board(
    txn: &WriteTransaction,
    owner: &AccountId,
    name: &str,
    created_by_token: &str,
    over: OverQuota,
) -> DbResult<String> {
    txn_refuse_stranded(txn, owner)?;
    let held = txn_mmap_get(txn, BOARDS_BY_OWNER, owner.as_str())?.len();
    txn_refuse_over_quota(txn, owner.as_str(), Quota::BoardsPerAccount, held + 1)?;
    let id = Uuid::new_v4().to_string();
    let row = BoardRow {
        id: id.clone(),
        name: name.to_string(),
        position: 0,
        owner_id: owner.as_str().to_string(),
        config_json: "{}".to_string(),
        created_by_token: created_by_token.to_string(),
    };
    txn_store_owned(txn, BOARDS, &id, &row, over)?;
    mmap_insert(txn, BOARDS_BY_OWNER, owner.as_str(), &id)?;
    txn.open_table(BOARD_ACTIVITY)?
        .insert(id.as_str(), Utc::now().timestamp())?;
    Ok(id)
}

// ---- Per-board color defaults (KanbanFlow parity) ----

/// Per-color defaults for a new board's standard palette:
/// (value, label, enabled, is_default, sort_order). Hex values are fixed
/// per standard color (see `STANDARD_COLORS` in models.rs).
fn standard_color_specs() -> Vec<(&'static str, &'static str, bool, bool, i64)> {
    let mut specs = vec![
        ("yellow", "Yellow", true, true, 1),
        ("green", "Green", true, false, 2),
        ("blue", "Blue", true, false, 3),
        ("red", "Red", true, false, 4),
    ];

    // KanbanFlow enables its whole standard palette on a new board
    specs.extend([
        ("orange", "Orange", true, false, 5),
        ("purple", "Purple", true, false, 6),
        ("magenta", "Magenta", true, false, 7),
        ("cyan", "Cyan", true, false, 8),
        ("brown", "Brown", true, false, 9),
        ("white", "White", true, false, 10),
    ]);
    specs
}

/// Per-color defaults for the Pomodoro board template, in the same tuple shape as
/// [`standard_color_specs`]. The four pomodoro-count colors are renamed and enabled, the other six
/// disabled (KanbanFlow reference: kf331-kanbanflow-settings-colors-recovered-20261016.png).
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
/// with the Pomodoro scheme, four columns, and the lone main lane.
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
            {"name": "Default", "position": 0},
        ],

        // Starter tasks demonstrating the pomodoro color scheme, e.g. "Pomodoro 1"
        "tasks": [
            {
                "name": "Pomodoro 1",
                "description": "",
                "size": 1,
                "color_value": "yellow",
                "column": "To-do",
                "swimlane": "Default",
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
                "swimlane": "Default",
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
                "swimlane": "Default",
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

const SIMPLE_BOARD_OFF_COLORS: [&str; 2] = ["brown", "white"];

/// The palette of a board made without a template: [`standard_color_specs`] with
/// [`SIMPLE_BOARD_OFF_COLORS`] disabled.
fn simple_board_color_specs() -> Vec<(&'static str, &'static str, bool, bool, i64)> {
    standard_color_specs()
        .into_iter()
        .map(|(value, label, enabled, is_default, sort_order)| {
            let enabled = enabled && !SIMPLE_BOARD_OFF_COLORS.contains(&value);
            (value, label, enabled, is_default, sort_order)
        })
        .collect()
}

/// The snapshot a board gets when a person creates one without picking a template, matching
/// KanbanFlow's "Simple task board": To-do, Doing and Done with no WIP limits, the standard palette
/// with Brown and White off, and the lone main lane the board page renders without a label.
fn simple_task_board_snapshot() -> serde_json::Value {
    let colors: Vec<serde_json::Value> = simple_board_color_specs()
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
            {"name": "Doing", "wip_limit": null, "is_done": false, "position": 1},
            {"name": "Done", "wip_limit": null, "is_done": true, "position": 2},
        ],
        "swimlanes": [
            {"name": "Default", "position": 0},
        ],
    })
}

/// Config-bag update that names `lane_id` as a board's main swimlane.
fn main_swimlane_entry(lane_id: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut entry = serde_json::Map::new();
    entry.insert(
        crate::models::MAIN_SWIMLANE_KEY.to_string(),
        serde_json::Value::String(lane_id.to_string()),
    );
    entry
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

/// Swimlane fields one update may change. `None` leaves a field alone.
#[derive(Debug, Default)]
pub struct SwimlanePatch<'a> {
    /// Renames the lane.
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    /// Replaces every limit of the lane.
    pub limits: Option<&'a [LaneLimit]>,
    /// Folds or unfolds the lane.
    pub collapsed: Option<bool>,
    /// `Some(None)` returns the lane to its default height.
    pub height: Option<Option<i64>>,
}

/// One built-in board template: (name, description, snapshot builder).
type BuiltinTemplateSpec = (&'static str, &'static str, fn() -> serde_json::Value);

/// Parameters for inserting a [`ColorRow`] (keeps `txn_insert_color`
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
    /// The name of the shipped built-in board template used for the starter
    /// "General" board.
    pub const BUILTIN_TEMPLATE_NAME: &'static str = "Pomodoro board";
    /// Bytes of the database file redb may cache in memory.
    const CACHE_BYTES: usize = 64 * 1024 * 1024;
    /// Seconds an expired or spent request is kept, so late polls still
    /// get a definite answer, before cleanup deletes it.
    const DEVICE_AUTH_RETENTION_SECONDS: i64 = 86_400;
    /// Seconds a device-pairing code stays valid.
    pub const DEVICE_CODE_TTL_SECONDS: i64 = 600;
    /// Minimum seconds between token polls for one device code.
    pub const DEVICE_POLL_INTERVAL_SECONDS: i64 = 5;
    /// The name of the shipped built-in "Kanban basics" board template.
    pub const KANBAN_BASICS_TEMPLATE_NAME: &'static str = "Kanban basics";
    /// Maximum stored attachment size: 5 MiB.
    pub const MAX_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
    /// Maximum allowed color label length (KanbanFlow renames cap at 50).
    pub const MAX_COLOR_LABEL_LEN: usize = 50;
    /// Maximum history events kept per task; older ones are dropped from
    /// the front so the trail stays bounded (KanbanFlow parity: History).
    pub const MAX_HISTORY_EVENTS: usize = 500;
    /// Maximum serialized size of a task's metadata object: 64 KiB.
    pub const MAX_METADATA_BYTES: usize = 64 * 1024;
    /// Most device-pairing requests that may be pending at once, across every client network.
    pub const MAX_PENDING_DEVICE_AUTHS: usize = 1000;
    /// Most device-pairing requests one client network may have pending at once.
    pub const MAX_PENDING_DEVICE_AUTHS_PER_NETWORK: usize = 5;

    /// Open (creating when needed) the database file at `path`, check its
    /// [`SCHEMA_VERSION`] stamp, create tables on first run, then seed the
    /// starter board. Fails with [`SchemaTooNew`] on a file from a newer
    /// binary. The owner is NOT created here: the /setup page
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
        // redb won't create missing parent directories itself
        if let Some(parent) = Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let repaired = preflight_schema_version(path)?;
        let mut db = match Database::builder()
            .set_cache_size(Self::CACHE_BYTES)
            .create(path)
        {
            Ok(db) => db,
            Err(err) if repaired => {
                return Err(format!(
                    "was not closed cleanly and needs redb's crash repair, which failed: {err}"
                )
                .into());
            }
            Err(err) => return Err(err.into()),
        };
        if repaired {
            println!("pomb: database {path} was not closed cleanly; redb repaired it");
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
            txn.open_table(TEMPLATE_SNAPSHOTS)?;
            txn.open_multimap_table(TEMPLATES_BY_OWNER)?;
            txn.open_table(ATTACHMENT_DATA)?;
            txn.open_multimap_table(COLORS_BY_BOARD)?;
            txn.open_multimap_table(LINKS_BY_TARGET)?;
            txn.open_table(FIELD_DEFS)?;
            txn.open_table(DEVICE_AUTHS)?;
            txn.open_table(PASSKEYS)?;
            txn.open_multimap_table(FIELDS_BY_BOARD)?;
            txn.open_multimap_table(BOARDS_BY_OWNER)?;
            txn.open_table(ACCOUNT_SETTINGS)?;
            txn.open_table(RECOVERY_CODES)?;
            txn.open_table(INVITES)?;
            txn.open_table(ACTIVITY)?;
            txn.open_table(SUSPENDED_TOKENS)?;
            txn.open_table(BOARD_ACTIVITY)?;
            txn.open_table(ACCOUNT_USAGE)?;
            txn.open_multimap_table(ENTRIES_BY_ACCOUNT)?;
            txn.open_multimap_table(ENTRIES_BY_DAY)?;
            txn.open_multimap_table(PASSKEYS_BY_USER)?;
            txn.open_table(FEED_TOKENS)?;
            txn.open_table(PASSKEYS_BY_CREDENTIAL)?;
            txn.open_multimap_table(TOKENS_BY_ACCOUNT)?;
            txn.open_multimap_table(SESSIONS_BY_USER)?;
            txn.open_multimap_table(DEVICES_BY_ACCOUNT)?;
            txn.open_table(TOKEN_IDS_BY_HASH)?;
            txn.open_multimap_table(SALTED_TOKENS_BY_LAST4)?;
            txn.open_multimap_table(DEVICES_BY_USER_CODE)?;
            txn.open_multimap_table(DEVICES_BY_EXPIRY)?;
            txn.open_table(BOARD_EVENTS)?;
            txn.open_table(EVENT_WAKEUPS)?;
        }
        txn.commit()?;
        compact(&mut db, path);

        let this = Self {
            db: Arc::new(Store::new(db)),
            path: Arc::from(Path::new(path)),
            session_seq: Arc::new(AtomicU64::new(session_seed())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            timer_gates: Arc::new(Mutex::new(HashMap::new())),
            activity: Arc::new(Mutex::new(Activity::default())),
            board_activity: Arc::new(Mutex::new(Activity::default())),
        };

        // Backfill before seed() so existing boards keep their current rendering (legend ON) while
        // a fresh install's starter board defaults to OFF (KanbanFlow parity).
        this.migrate_legend_default()?;
        this.restore_pomodori_labels()?;
        this.backfill_board_colors()?;
        this.assign_main_swimlanes()?;
        this.normalize_grouping_dates()?;
        this.clear_orphan_repeats()?;
        this.seed()?;
        Ok(this)
    }

    /// One-time backfill for the per-board "Color legend" toggle. Boards that predate the toggle
    /// keep their current rendering by defaulting `legend_visible` to true, and boards created
    /// afterwards read the key as absent and default to false (KanbanFlow shows no legend by
    /// default). Only fills the key where absent, so an explicit user choice, including false, is
    /// never overwritten. The `legend_backfill_done` marker makes the pass run exactly once,
    /// because a fresh install's second boot must not backfill the starter board that `seed()`
    /// created. Like `ensure_board_colors`, this is a lazy backfill and not a schema migration.
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
                .board(&id)?
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
                self.merge_board_config(&id, updates)?;
            }
        }
        let txn = self.db.begin_write()?;
        write_one(&txn, SETTINGS, "legend_backfill_done", &true)?;
        txn.commit()?;
        Ok(())
    }

    /// Rewrites grouping dates stored before the strict YYYY-MM-DD rule into that form, clearing
    /// ones that are not dates, and print each change so the admin sees what moved between Done
    /// groups.
    fn normalize_grouping_dates(&self) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        let changed: Vec<TaskRow> = {
            let tbl = txn.open_table(TASKS)?;
            let mut changed = Vec::new();
            for item in tbl.iter()? {
                let (_, value) = item?;
                let mut task: TaskRow = serde_json::from_slice(value.value())?;
                let Some(raw) = task.grouping_date.clone() else {
                    continue;
                };
                let normalized = normalize_grouping_date(&raw);
                if normalized.as_deref() == Some(raw.as_str()) {
                    continue;
                }
                match &normalized {
                    Some(date) => println!(
                        "pomb: task {}: grouping date \"{raw}\" rewritten as \"{date}\"",
                        task.id
                    ),
                    None => println!(
                        "pomb: task {}: grouping date \"{raw}\" is not a date; cleared it",
                        task.id
                    ),
                }
                task.grouping_date = normalized;
                changed.push(task);
            }
            changed
        };
        if changed.is_empty() {
            return Ok(());
        }
        {
            let mut tasks = txn.open_table(TASKS)?;
            for task in &changed {
                txn_store_task(&txn, &mut tasks, task, OverQuota::Allow)?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Clears the repeat on tasks without a due date, which older binaries left behind, and prints
    /// each change. Running it again changes nothing, and the rows keep their `updated_at`.
    fn clear_orphan_repeats(&self) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        let changed: Vec<TaskRow> = {
            let tbl = txn.open_table(TASKS)?;
            let mut changed = Vec::new();
            for item in tbl.iter()? {
                let (_, value) = item?;
                let mut task: TaskRow = serde_json::from_slice(value.value())?;
                if task.due_at.is_none() && task.due_repeat.is_some() {
                    println!(
                        "pomb: task {}: cleared a repeat that has no due date",
                        task.id
                    );
                    task.due_repeat = None;
                    changed.push(task);
                }
            }
            changed
        };
        if changed.is_empty() {
            return Ok(());
        }
        {
            let mut tasks = txn.open_table(TASKS)?;
            for task in &changed {
                txn_store_task(&txn, &mut tasks, task, OverQuota::Allow)?;
            }
        }
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

    /// One-time backfill restoring KanbanFlow's "Pomodori" in stored pomodoro-count color labels.
    /// An earlier release renamed "2 Pomodori" to "2 Pomodoros" (and 3, >3), and live KanbanFlow
    /// spells them "Pomodori". Only those exact strings are rewritten, so a user-customized label
    /// is never touched. The `pomodori_restore_done` marker makes the pass run exactly once. Like
    /// `migrate_legend_default`, this is a lazy backfill and not a schema migration.
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
            txn_store_owned(&txn, TASK_COLORS, id.as_str(), row, OverQuota::Allow)?;
        }
        write_one(&txn, SETTINGS, "pomodori_restore_done", &true)?;
        txn.commit()?;
        Ok(())
    }

    /// First-run seeding: the built-in board templates (inserted exactly
    /// once each), then the starter "General" board built by applying the
    /// "Pomodoro board" template — only when no boards exist, so restarting
    /// never duplicates anything. The board goes to the instance's owner, or
    /// stays ownerless for the first account to claim.
    fn seed(&self) -> DbResult<()> {
        let template_id = self.ensure_builtin_templates(Self::BUILTIN_TEMPLATE_NAME)?;
        if self.table_len(BOARDS)? == 0 {
            let owner = self
                .owner()?
                .map_or_else(AccountId::ownerless, |owner| owner.account_id());
            self.apply_template(&owner, &template_id, STARTER_BOARD_NAME)?;
        }
        Ok(())
    }

    /// Insert every built-in board template that has no built-in row with
    /// its name yet, and rewrite a stored built-in whose snapshot differs
    /// from the shipped one, refreshing its description too; returns the id of the requested template
    /// (idempotent).
    fn ensure_builtin_templates(&self, want: &str) -> DbResult<String> {
        let builtins: Vec<BuiltinTemplateSpec> = vec![
            (
                Self::BUILTIN_TEMPLATE_NAME,
                "Pomodoro board: task colors for 1/2/3/>3 pomodori plus a ready-to-use column layout.",
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
            let snapshot = serde_json::to_vec(&snapshot_fn())?;
            let (existing, current) = {
                let txn = self.read()?;
                let existing = txn_templates_of(&txn, BUILT_IN_TEMPLATES)?
                    .into_iter()
                    .find(|template| template.name == *name);
                let current = match &existing {
                    Some(row) => txn
                        .open_table(TEMPLATE_SNAPSHOTS)?
                        .get(row.id.as_str())?
                        .is_some_and(|stored| stored.value() == snapshot.as_slice()),
                    None => false,
                };
                (existing, current)
            };
            let mut row = existing.unwrap_or_else(|| BoardTemplateRow {
                id: Uuid::new_v4().to_string(),
                name: name.to_string(),
                description: description.to_string(),
                built_in: true,
                owner_id: None,
                snapshot_bytes: 0,
            });
            if !current {
                row.description = description.to_string();
                let txn = self.db.begin_write()?;
                txn_store_template(&txn, &mut row, &snapshot)?;
                txn.commit()?;
            }
            let id = row.id;
            if *name == want {
                wanted_id = Some(id);
            }
        }
        wanted_id.ok_or_else(|| format!("unknown built-in template: {want}").into())
    }

    /// Opens a read transaction.
    fn read(&self) -> DbResult<ReadTransaction> {
        Ok(self.db.begin_read()?)
    }

    fn table_len(&self, table: TableDefinition<'_, &str, &[u8]>) -> DbResult<u64> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(table)?.len()?)
    }

    // ---- Users & sessions ----

    /// Creates an account named `username` and returns its id. The instance's first account is
    /// its admin and claims every ownerless row, and any later one is a member.
    pub fn create_user(&self, username: &str) -> DbResult<AccountId> {
        let txn = self.db.begin_write()?;
        let role = if txn.open_table(USERS)?.is_empty()? {
            AccountRole::Admin
        } else {
            AccountRole::Member
        };
        let row = new_user(username, role);
        txn_insert_user(&txn, &row)?;
        txn.commit()?;
        Ok(row.account_id())
    }

    /// Returns the account with `id`.
    pub fn user(&self, id: &str) -> DbResult<Option<UserRow>> {
        read_one(&self.db, USERS, id)
    }

    /// Returns the instance's owner, its admin, if it exists yet.
    pub fn owner(&self) -> DbResult<Option<UserRow>> {
        let txn = self.db.begin_read()?;
        owner_in(&txn.open_table(USERS)?)
    }

    /// Returns the instance's owner, creating it as the admin `username` when there is none.
    pub fn ensure_owner(&self, username: &str) -> DbResult<UserRow> {
        if let Some(owner) = self.owner()? {
            return Ok(owner);
        }
        let txn = self.db.begin_write()?;
        let existing = owner_in(&txn.open_table(USERS)?)?;
        let owner = match existing {
            Some(owner) => owner,
            None => {
                let row = new_user(username, AccountRole::Admin);
                txn_insert_user(&txn, &row)?;
                row
            }
        };
        txn.commit()?;
        Ok(owner)
    }

    /// Returns `user`'s passkeys, oldest first.
    pub fn passkeys_of(&self, user: &AccountId) -> DbResult<Vec<PasskeyRow>> {
        passkeys_of_in(&self.read()?, user.as_str())
    }

    /// Stores the instance's first passkey for `user`, creating that user when no user exists,
    /// and its `recovery` code when one is given.
    ///
    /// Refuses when any passkey exists, or when a different user owns the instance, so the
    /// first-run claim succeeds once.
    pub fn claim_first_passkey(
        &self,
        user: &UserRow,
        passkey: &PasskeyRow,
        recovery: Option<&RecoveryCodeRow>,
    ) -> DbResult<FirstPasskeyOutcome> {
        let txn = self.db.begin_write()?;
        if txn.open_table(PASSKEYS)?.len()? > 0 {
            return Ok(FirstPasskeyOutcome::Taken);
        }
        let owner = owner_in(&txn.open_table(USERS)?)?;
        match owner {
            Some(owner) if owner.id != user.id => return Ok(FirstPasskeyOutcome::Taken),
            Some(_) => {}
            None => txn_insert_user(&txn, user)?,
        }
        txn_store_passkey(&txn, passkey)?;
        if let Some(recovery) = recovery {
            write_one(&txn, RECOVERY_CODES, &user.id, recovery)?;
        }
        txn.commit()?;
        Ok(FirstPasskeyOutcome::Claimed)
    }

    /// Stores another passkey, unless its credential is registered already. Refuses, with
    /// [`QuotaExceeded`], a passkey past [`MAX_PASSKEYS_PER_ACCOUNT`].
    pub fn add_passkey(&self, passkey: &PasskeyRow) -> DbResult<PasskeyAddOutcome> {
        let txn = self.db.begin_write()?;
        if txn_credential_registered(&txn, passkey)? {
            return Ok(PasskeyAddOutcome::Duplicate);
        }
        txn_store_passkey(&txn, passkey)?;
        txn.commit()?;
        Ok(PasskeyAddOutcome::Added)
    }

    /// Records a sign-in with passkey `id`, storing its updated `credential`.
    pub fn record_passkey_use(&self, id: &str, credential: &Passkey, now: i64) -> DbResult<bool> {
        let credential = serde_json::to_value(credential)?;
        mutate(&self.db, PASSKEYS, id, |row: &mut PasskeyRow| {
            row.credential = credential;
            row.last_used_at = Some(now);
        })
    }

    /// Removes `user`'s passkey `id`, refusing to remove that user's last one, and ends every
    /// session of the user except `keep` in the same transaction.
    pub fn delete_passkey(
        &self,
        user: &AccountId,
        id: &str,
        keep: Option<&str>,
    ) -> DbResult<PasskeyDeleteOutcome> {
        let txn = self.db.begin_write()?;
        let held = txn_mmap_get(&txn, PASSKEYS_BY_USER, user.as_str())?;
        if !held.iter().any(|held| held == id) {
            return Ok(PasskeyDeleteOutcome::NotFound);
        }
        if held.len() <= 1 {
            return Ok(PasskeyDeleteOutcome::RefusedLast);
        }
        txn_remove_passkey(&txn, id)?;
        mmap_remove(&txn, PASSKEYS_BY_USER, user.as_str(), id)?;
        let keep = keep.map(hash_session_token);
        txn_end_sessions_of(&txn, user.as_str(), keep.as_deref())?;
        txn.commit()?;
        Ok(PasskeyDeleteOutcome::Deleted)
    }

    /// Ends every session of `user` except `keep`, and returns how many it ended.
    pub fn end_other_sessions(&self, user: &AccountId, keep: Option<&str>) -> DbResult<u64> {
        let keep = keep.map(hash_session_token);
        let txn = self.db.begin_write()?;
        let ended = txn_end_sessions_of(&txn, user.as_str(), keep.as_deref())?;
        txn.commit()?;
        Ok(ended)
    }

    /// Ends every session older than [`SESSION_MAX_AGE_SECONDS`] at `now`, and returns how many
    /// it ended.
    pub fn prune_sessions(&self, now: i64) -> DbResult<u64> {
        let txn = self.db.begin_write()?;
        let ended = txn_sweep_sessions(&txn, now)?;
        txn.commit()?;
        Ok(ended)
    }

    /// Opens a session for `user` that has just proved a passkey, and returns its token. The
    /// session is stored under the hash of the token.
    ///
    /// Refuses, with [`SessionRefused`], an account that is absent or suspended when the write
    /// begins.
    pub fn create_session(&self, user: &AccountId) -> DbResult<String> {
        let token = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let row = SessionRow {
            user_id: user.as_str().to_string(),
            created_at: now.clone(),
            verified_at: Some(now),
        };
        let txn = self.db.begin_write()?;
        let refused = txn_read_one::<UserRow>(&txn, USERS, user.as_str())?
            .is_none_or(|account| account.status == AccountStatus::Suspended);
        if refused {
            return Err(Box::new(SessionRefused));
        }
        let key = hash_session_token(&token);
        write_one(&txn, SESSIONS, &key, &row)?;
        mmap_insert(&txn, SESSIONS_BY_USER, user.as_str(), &key)?;
        txn.commit()?;
        Ok(token)
    }

    /// Returns whether session `token` proved a passkey less than [`REAUTH_WINDOW_SECS`] before
    /// `now`.
    pub fn session_is_fresh(&self, token: &str, now: i64) -> DbResult<bool> {
        let Some(session) = read_one::<SessionRow>(&self.db, SESSIONS, &hash_session_token(token))?
        else {
            return Ok(false);
        };
        Ok(session
            .verified_at
            .and_then(|verified| DateTime::parse_from_rfc3339(&verified).ok())
            .is_some_and(|verified| now - verified.timestamp() < REAUTH_WINDOW_SECS))
    }

    /// Records that session `token` just proved a passkey, and returns whether the session
    /// exists.
    pub fn verify_session(&self, token: &str) -> DbResult<bool> {
        let verified = Utc::now().to_rfc3339();
        mutate(
            &self.db,
            SESSIONS,
            &hash_session_token(token),
            |row: &mut SessionRow| {
                row.verified_at = Some(verified);
            },
        )
    }

    /// Returns the account a session token signs in. A session older than
    /// [`SESSION_MAX_AGE_SECONDS`] signs in nobody and is deleted.
    pub fn user_for_token(&self, token: &str) -> DbResult<Option<UserRow>> {
        let user = {
            let txn = self.db.begin_read()?;
            let Some(session) = read_in::<SessionRow>(&txn, SESSIONS, &hash_session_token(token))?
            else {
                return Ok(None);
            };
            if !session_expired(&session, Utc::now().timestamp()) {
                return read_in(&txn, USERS, &session.user_id);
            }
            None
        };
        self.delete_session(token)?;
        Ok(user)
    }

    // ---- API tokens (agent access) ----

    pub fn create_api_token(&self, record: &ApiTokenRecord) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        let account = AccountId::new(record.account_id.as_str());
        txn_refuse_stranded(&txn, &account)?;
        if !txn_account_active(&txn, &account)? {
            return Err(ACCOUNT_NOT_ACTIVE.into());
        }
        if let Some(refusal) = txn_token_cap_reached(&txn, &record.account_id)? {
            return Err(Box::new(refusal));
        }
        txn_store_token(&txn, record)?;
        txn.commit()?;
        Ok(())
    }

    /// Returns the live API tokens of `account`, oldest first, reading only that account's.
    pub fn list_account_api_tokens(&self, account: &AccountId) -> DbResult<Vec<ApiTokenRecord>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(API_TOKENS)?;
        let mut out = Vec::new();
        for id in index_in(&txn, TOKENS_BY_ACCOUNT, account.as_str())? {
            out.extend(row_in::<ApiTokenRecord, _>(&tbl, &id)?);
        }
        out.sort_by_key(|t| t.created_at);
        Ok(out)
    }

    /// Finds the live token `raw` is through the index of token SHA-256s. A token still under a
    /// salted hash is found among those that end in the same four characters, and keyed by its
    /// SHA-256 from then on.
    pub fn find_api_token(&self, raw: &str) -> DbResult<Option<ApiTokenRecord>> {
        self.find_token_in(API_TOKENS, raw)
    }

    /// Finds the token `raw` is in `table`, which holds live or suspended tokens.
    fn find_token_in(&self, table: Rows, raw: &str) -> DbResult<Option<ApiTokenRecord>> {
        let digest = api_token_digest(raw);
        let found = {
            let txn = self.db.begin_read()?;
            let rows = txn.open_table(table)?;
            let keyed = txn
                .open_table(TOKEN_IDS_BY_HASH)?
                .get(digest.as_str())?
                .map(|id| id.value().to_owned());
            if let Some(id) = keyed {
                let record = row_in::<ApiTokenRecord, _>(&rows, &id)?;
                return Ok(record.filter(|record| verify_api_token(record, raw)));
            }
            let mut found = None;
            for id in index_in(&txn, SALTED_TOKENS_BY_LAST4, &api_token_last4(raw))? {
                let record = row_in::<ApiTokenRecord, _>(&rows, &id)?;
                if let Some(record) = record.filter(|record| verify_api_token(record, raw)) {
                    found = Some(record);
                    break;
                }
            }
            found
        };
        let Some(record) = found else {
            return Ok(None);
        };
        Ok(self.key_salted_token(table, record, &digest))
    }

    /// Moves a token found under its salted hash to its SHA-256 key, returning the record as
    /// stored, or `None` when the token was deleted in the meantime. A failed write leaves the
    /// token under its salted hash.
    fn key_salted_token(
        &self,
        table: Rows,
        record: ApiTokenRecord,
        digest: &str,
    ) -> Option<ApiTokenRecord> {
        let keyed = ApiTokenRecord {
            token_hash: digest.to_owned(),
            salt: String::new(),
            ..record.clone()
        };
        let moved = (|| -> DbResult<bool> {
            let txn = self.db.begin_write()?;
            let current = row_in::<ApiTokenRecord, _>(&txn.open_table(table)?, &record.id)?;
            let Some(current) = current else {
                return Ok(false);
            };
            if !current.salt.is_empty() {
                txn_unindex_token_key(&txn, &record)?;
                write_one(&txn, table, &record.id, &keyed)?;
                txn_index_token_key(&txn, &keyed)?;
            }
            txn.commit()?;
            Ok(true)
        })();
        match moved {
            Ok(true) => Some(keyed),
            Ok(false) => None,
            Err(_) => Some(record),
        }
    }

    /// Deletes `account`'s token `id`. Returns false when that account holds no such token.
    pub fn delete_api_token(&self, account: &AccountId, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let held = txn_mmap_get(&txn, TOKENS_BY_ACCOUNT, account.as_str())?
            .iter()
            .any(|token| token == id);
        let removed = held && txn_remove_token(&txn, account.as_str(), id)?;
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
        let key = hash_session_token(token);
        let removed = txn
            .open_table(SESSIONS)?
            .remove(key.as_str())?
            .map(|guard| serde_json::from_slice::<SessionRow>(guard.value()))
            .transpose()?;
        if let Some(session) = removed {
            mmap_remove(&txn, SESSIONS_BY_USER, &session.user_id, &key)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Returns the bytes of keys, values and metadata the tables hold, which deletes reduce even
    /// when the file keeps its size. It reads, so it never waits for a writer.
    ///
    /// The count leaves out the trees that index the tables and redb's own system trees, so it
    /// stays a little under the file's live bytes.
    pub fn stored_bytes(&self) -> DbResult<u64> {
        let txn = self.db.begin_read()?;
        let mut bytes = 0;
        for handle in txn.list_tables()? {
            let stats = txn.open_untyped_table(handle)?.stats()?;
            bytes += stats.stored_bytes() + stats.metadata_bytes();
        }
        for handle in txn.list_multimap_tables()? {
            let stats = txn.open_untyped_multimap_table(handle)?.stats()?;
            bytes += stats.stored_bytes() + stats.metadata_bytes();
        }
        Ok(bytes)
    }

    // ---- Boards ----

    /// Creates a board for `owner` with the standard color palette, Brown and White off.
    pub fn create_board(&self, owner: &AccountId, name: &str) -> DbResult<String> {
        self.create_board_by(owner, name, "")
    }

    /// Creates a board for `owner` with the standard color palette, Brown and White off, recording
    /// the API token `created_by_token` that made it, or none when the string is empty.
    pub fn create_board_by(
        &self,
        owner: &AccountId,
        name: &str,
        created_by_token: &str,
    ) -> DbResult<String> {
        let txn = self.db.begin_write()?;
        let id = txn_insert_board(&txn, owner, name, created_by_token, OverQuota::Refuse)?;
        txn_insert_color_specs(&txn, &id, simple_board_color_specs())?;
        txn.commit()?;
        Ok(id)
    }

    /// Creates a board for `owner` as a person gets one without picking a template, shaped by
    /// [`simple_task_board_snapshot`], recording the API token `created_by_token` that made it, or
    /// none when the string is empty.
    pub fn create_simple_board_by(
        &self,
        owner: &AccountId,
        name: &str,
        created_by_token: &str,
    ) -> DbResult<String> {
        let txn = self.db.begin_write()?;
        let id = Self::build_from_template(
            &txn,
            owner,
            &simple_task_board_snapshot(),
            name,
            created_by_token,
            OverQuota::Refuse,
        )?;
        txn.commit()?;
        Ok(id)
    }

    pub fn board(&self, id: &str) -> DbResult<Option<BoardRow>> {
        read_one(&self.db, BOARDS, id)
    }

    /// Finds the board whose calendar-feed bearer token is `raw`. The token lives in the board's
    /// opaque config bag, which is never serialized to clients.
    pub fn board_by_feed_token(&self, raw: &str) -> DbResult<Option<BoardRow>> {
        let txn = self.read()?;
        let Some(id) = txn
            .open_table(FEED_TOKENS)?
            .get(feed_token_key(raw).as_str())?
            .map(|guard| guard.value().to_string())
        else {
            return Ok(None);
        };
        let board = read_in::<BoardRow>(&txn, BOARDS, &id)?;
        Ok(board.filter(|board| {
            board
                .feed_token()
                .is_some_and(|token| constant_time_eq(token.as_bytes(), raw.as_bytes()))
        }))
    }

    pub fn board_exists(&self, id: &str) -> DbResult<bool> {
        Ok(self.board(id)?.is_some())
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
                    txn_store_board(&txn, &row, OverQuota::Refuse)?;
                    true
                }
            }
        };
        txn.commit()?;
        Ok(updated)
    }

    /// Merges key/value pairs into a board's opaque config bag (e.g. `legend_visible`). Returns
    /// false when the board is unknown. Errors with "cannot archive the last active board" when
    /// `archived: true` would leave no unarchived board.
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
                    let archives = updates.get("archived") == Some(&serde_json::Value::Bool(true))
                        && !row.config_bool("archived");
                    if archives && !other_active_board_exists(&txn, &row.owner_id, id)? {
                        return Err("cannot archive the last active board".into());
                    }
                    for (k, v) in updates {
                        cfg.insert(k.clone(), v.clone());
                    }
                    let config_json = serde_json::to_string(&cfg)?;
                    if config_json.len() > row.config_json.len() {
                        TextCap::CONFIG.check(&config_json)?;
                    }
                    row.config_json = config_json;
                    txn_store_board(&txn, &row, OverQuota::Refuse)?;
                    true
                }
            }
        };
        txn.commit()?;
        Ok(updated)
    }

    /// Merges `updates` into a board's config bag for the server, past any quota, in one
    /// transaction. An unknown board is left alone.
    fn merge_board_config(
        &self,
        id: &str,
        updates: serde_json::Map<String, serde_json::Value>,
    ) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_merge_board_config(&txn, id, updates)?;
        txn.commit()?;
        Ok(())
    }

    /// Stores `candidate` under `key` in a board's config bag unless the key already holds a
    /// string, in one transaction. Returns the stored string, or None when the board is unknown.
    pub fn ensure_board_config_string(
        &self,
        id: &str,
        key: &str,
        candidate: &str,
    ) -> DbResult<Option<String>> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Ok(None);
        };
        let mut cfg: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&row.config_json).unwrap_or_default();
        if let Some(existing) = cfg.get(key).and_then(serde_json::Value::as_str) {
            return Ok(Some(existing.to_string()));
        }
        cfg.insert(
            key.to_string(),
            serde_json::Value::String(candidate.to_string()),
        );
        row.config_json = serde_json::to_string(&cfg)?;
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(Some(candidate.to_string()))
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

    /// Returns every board `owner` owns, ordered by position (then id for stability).
    pub fn owner_boards(&self, owner: &AccountId) -> DbResult<Vec<BoardRow>> {
        let txn = self.db.begin_read()?;
        let mut out = Vec::new();
        for id in index_in(&txn, BOARDS_BY_OWNER, owner.as_str())? {
            out.extend(read_in::<BoardRow>(&txn, BOARDS, &id)?);
        }
        out.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    /// Returns the id of `owner`'s lowest-position unarchived board, or of its lowest-position
    /// archived one when every board is archived, or [`None`] when it has no board.
    pub fn first_board_id(&self, owner: &AccountId) -> DbResult<Option<String>> {
        let boards = self.owner_boards(owner)?;
        let first = boards
            .iter()
            .find(|board| !board.config_bool("archived"))
            .or_else(|| boards.first());
        Ok(first.map(|board| board.id.clone()))
    }

    /// Deletes a board and everything in it: columns, swimlanes, tasks with their attachments,
    /// time entries, task colors, and its owner's active timer when it runs on one
    /// of its tasks. Errors when the board id is unknown, and when it is its owner's only board or
    /// last unarchived one.
    pub fn delete_board(&self, id: &str) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        let Some(board) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Err("board not found".into());
        };
        let boards = txn_owner_boards(&txn, &board.owner_id)?;
        if let Some(refusal) = board_delete_refusal(&boards, id) {
            return Err(refusal.into());
        }
        txn_remove_board(&txn, &board)?;
        txn.commit()?;
        Ok(())
    }

    // ---- Columns ----

    pub fn column(&self, id: &str) -> DbResult<Option<ColumnRow>> {
        read_one(&self.db, COLUMNS, id)
    }

    /// Columns of a board, ordered by position (then id for stability).
    pub fn list_columns(&self, board_id: &str) -> DbResult<Vec<ColumnRow>> {
        let mut columns = Vec::new();
        for id in mmap_get(&self.db, COLUMNS_BY_BOARD, board_id)? {
            if let Some(column) = self.column(&id)? {
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
        self.create_column_as(board_id, name, wip_limit, ColumnKind::Regular)
    }

    pub fn create_column_as(
        &self,
        board_id: &str,
        name: &str,
        wip_limit: Option<i64>,
        kind: ColumnKind,
    ) -> DbResult<String> {
        let txn = self.db.begin_write()?;
        let is_done = kind == ColumnKind::Done;
        let id = txn_insert_column(&txn, board_id, name, wip_limit, is_done, OverQuota::Refuse)?;
        txn.commit()?;
        Ok(id)
    }

    /// Applies every field of `patch` in one transaction. Toggling `is_done` stamps `updated_at`,
    /// in the same transaction, on every task whose `blocked` value flips, and re-stamps
    /// `completed_at` on the column's tasks to match. Returns false when the column is unknown.
    pub fn update_column(&self, id: &str, patch: &ColumnPatch<'_>) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let before = if patch.is_done.is_some() {
            let in_column = txn_mmap_get(&txn, TASKS_BY_COLUMN, id)?;
            blocked_states(&txn, &blocked_dependents(&txn, &in_column)?)?
        } else {
            HashMap::new()
        };
        let mut restamp = None;
        {
            let Some(mut column) = txn_read_one::<ColumnRow>(&txn, COLUMNS, id)? else {
                return Ok(false);
            };

            // A rename remembers the old name in `former_names`
            if let Some(name) = patch.name.filter(|name| *name != column.name) {
                TextCap::COLUMN_NAME.check(name)?;
                column.rename(name);
            }
            if let Some(wip_limit) = patch.wip_limit {
                column.wip_limit = wip_limit;
            }
            if let Some(is_done) = patch.is_done {
                column.is_done = is_done;
                restamp = Some(is_done);
            }
            if let Some(description) = patch.description {
                TextCap::DESCRIPTION.check(description)?;
                column.description = description.to_string();
            }
            if let Some(collapsed) = patch.collapsed {
                column.collapsed = collapsed;
            }
            if let Some(config_json) = patch.config_json {
                TextCap::CONFIG.check(config_json)?;
                column.config_json = config_json.to_string();
            }
            txn_store_owned(&txn, COLUMNS, id, &column, OverQuota::Refuse)?;
        }
        if let Some(is_done) = restamp {
            restamp_column_tasks(&txn, id, is_done)?;
        }
        bump_flipped(&txn, &before)?;
        txn.commit()?;
        Ok(true)
    }

    /// Rename a column, remembering the old name in `former_names`.
    pub fn rename_column(&self, id: &str, name: &str) -> DbResult<bool> {
        TextCap::COLUMN_NAME.check(name)?;
        mutate_owned(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            if column.name != name {
                column.rename(name);
            }
        })
    }

    pub fn set_column_wip(&self, id: &str, wip_limit: Option<i64>) -> DbResult<bool> {
        mutate_owned(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.wip_limit = wip_limit;
        })
    }

    /// Sets a column's done flag and re-stamp `completed_at` on its tasks. Tasks blocked by a task
    /// in the column whose `blocked` value flips get `updated_at` stamped in the same transaction.
    pub fn set_column_done(&self, id: &str, is_done: bool) -> DbResult<bool> {
        let patch = ColumnPatch {
            is_done: Some(is_done),
            ..ColumnPatch::default()
        };
        self.update_column(id, &patch)
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
        for (index, mut row) in columns.into_iter().enumerate() {
            row.position = index as i64;
            txn_store_owned(&txn, COLUMNS, &row.id, &row, OverQuota::Allow)?;
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
        let txn = self.db.begin_write()?;
        if txn_read_one::<BoardRow>(&txn, BOARDS, source_board_id)?.is_none() {
            return Err(format!("unknown source board: {source_board_id}").into());
        }
        let source = txn_board_colors(&txn, source_board_id)?;
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
            for color in &old_target {
                txn_remove_owned::<ColorRow>(&txn, TASK_COLORS, &color.id)?;
                mmap_remove(&txn, COLORS_BY_BOARD, target_board_id, &color.id)?;
            }
            for color in &copies {
                txn_store_owned(&txn, TASK_COLORS, &color.id, color, OverQuota::Refuse)?;
                mmap_insert(&txn, COLORS_BY_BOARD, target_board_id, &color.id)?;
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
                    txn_store_task(&txn, &mut tasks_tbl, &task, OverQuota::Allow)?;
                }
            }
        }
        txn.commit()?;
        Ok(copies.len())
    }

    /// Deletes a column unless it is the board's last or still holds tasks. Both checks and the
    /// delete share one write transaction.
    pub fn delete_column(&self, id: &str) -> DbResult<ColumnDeleteOutcome> {
        let txn = self.db.begin_write()?;
        let Some(column) = txn_read_one::<ColumnRow>(&txn, COLUMNS, id)? else {
            return Ok(ColumnDeleteOutcome::NotFound);
        };
        if txn_mmap_get(&txn, COLUMNS_BY_BOARD, &column.board_id)?.len() <= 1 {
            return Ok(ColumnDeleteOutcome::RefusedLast);
        }
        let held = txn_mmap_get(&txn, TASKS_BY_COLUMN, id)?.len();
        if held > 0 {
            return Ok(ColumnDeleteOutcome::RefusedHasTasks(held));
        }

        // No task keeps a due target column that no longer exists
        for sibling in
            txn_indexed_rows::<ColumnRow>(&txn, COLUMNS_BY_BOARD, &column.board_id, COLUMNS)?
        {
            let tasks: Vec<TaskRow> = txn_indexed_rows(&txn, TASKS_BY_COLUMN, &sibling.id, TASKS)?;
            let mut tbl = txn.open_table(TASKS)?;
            for mut task in tasks {
                if task.due_target_column_id.as_deref() == Some(id) {
                    task.due_target_column_id = None;
                    task.updated_at = Some(Utc::now().to_rfc3339());
                    txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow)?;
                }
            }
        }

        for mut lane in
            txn_indexed_rows::<SwimlaneRow>(&txn, SWIMLANES_BY_BOARD, &column.board_id, SWIMLANES)?
        {
            if lane.limits.iter().any(|limit| limit.column_id == id) {
                lane.limits.retain(|limit| limit.column_id != id);
                txn_store_owned(&txn, SWIMLANES, &lane.id, &lane, OverQuota::Allow)?;
            }
        }
        txn_remove_owned::<ColumnRow>(&txn, COLUMNS, id)?;
        mmap_remove(&txn, COLUMNS_BY_BOARD, &column.board_id, id)?;
        txn.commit()?;
        Ok(ColumnDeleteOutcome::Deleted)
    }

    pub fn count_tasks_in_column(&self, column_id: &str) -> DbResult<usize> {
        Ok(mmap_get(&self.db, TASKS_BY_COLUMN, column_id)?.len())
    }

    // ---- Swimlanes ----

    pub fn swimlane(&self, id: &str) -> DbResult<Option<SwimlaneRow>> {
        read_one(&self.db, SWIMLANES, id)
    }

    /// Swimlanes of a board, ordered by position (then id for stability).
    pub fn list_swimlanes(&self, board_id: &str) -> DbResult<Vec<SwimlaneRow>> {
        let mut lanes = Vec::new();
        for id in mmap_get(&self.db, SWIMLANES_BY_BOARD, board_id)? {
            if let Some(lane) = self.swimlane(&id)? {
                lanes.push(lane);
            }
        }
        lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(lanes)
    }

    /// Creates a swimlane, and the first lane of a board becomes its main lane.
    pub fn create_swimlane(&self, board_id: &str, name: &str) -> DbResult<String> {
        self.create_swimlane_at(board_id, name, None)
    }

    /// Creates a swimlane at `position`, or at the end when none.
    pub fn create_swimlane_at(
        &self,
        board_id: &str,
        name: &str,
        position: Option<i64>,
    ) -> DbResult<String> {
        let txn = self.db.begin_write()?;
        let id = txn_insert_swimlane(&txn, board_id, name, OverQuota::Refuse)?;
        if let Some(position) = position {
            txn_move_swimlane(&txn, &id, position)?;
        }
        txn.commit()?;
        Ok(id)
    }

    /// Stores a main swimlane on every board that has none.
    ///
    /// Runs at startup and is idempotent, so reads never write.
    fn assign_main_swimlanes(&self) -> DbResult<()> {
        for board in self.list_boards()? {
            if board.main_swimlane_id().is_some() {
                continue;
            }
            let lanes = self.list_swimlanes(&board.id)?;
            if let Some(id) = board.main_swimlane(&lanes) {
                self.merge_board_config(&board.id, main_swimlane_entry(&id))?;
                println!("pomb: board {} got main swimlane {id} (KF-406)", board.id);
            }
        }
        Ok(())
    }

    /// Applies the fields set in `patch` to a swimlane, all in one write transaction. A limit
    /// that names a column of another board refuses the whole patch.
    pub fn update_swimlane(
        &self,
        id: &str,
        patch: &SwimlanePatch<'_>,
    ) -> DbResult<SwimlaneUpdateOutcome> {
        let txn = self.db.begin_write()?;
        let Some(mut lane) = txn_read_one::<SwimlaneRow>(&txn, SWIMLANES, id)? else {
            return Ok(SwimlaneUpdateOutcome::NotFound);
        };
        if let Some(name) = patch.name {
            TextCap::SWIMLANE_NAME.check(name)?;
            lane.name = name.to_string();
        }
        if let Some(description) = patch.description {
            TextCap::SWIMLANE_DESCRIPTION.check(description)?;
            lane.description = description.to_string();
        }
        if let Some(limits) = patch.limits {
            let columns = txn_mmap_get(&txn, COLUMNS_BY_BOARD, &lane.board_id)?;
            if limits
                .iter()
                .any(|limit| !columns.contains(&limit.column_id))
            {
                return Ok(SwimlaneUpdateOutcome::UnknownColumn);
            }
            lane.limits = limits.to_vec();
        }
        if let Some(collapsed) = patch.collapsed {
            lane.collapsed = collapsed;
        }
        if let Some(height) = patch.height {
            lane.height = height;
        }
        txn_store_owned(&txn, SWIMLANES, id, &lane, OverQuota::Refuse)?;
        txn.commit()?;
        Ok(SwimlaneUpdateOutcome::Updated)
    }

    /// Reorder a swimlane within its board; positions are renumbered densely.
    /// Returns false when the swimlane is unknown.
    pub fn move_swimlane(&self, id: &str, position: i64) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let moved = txn_move_swimlane(&txn, id, position)?;
        txn.commit()?;
        Ok(moved)
    }

    /// Merges every swimlane of a board into its main lane and deletes the others. Each moved
    /// task keeps its column, lands after the tasks already in that cell and gets a
    /// `swimlane_merged` history event. A board with one lane is left alone.
    pub fn merge_swimlanes(
        &self,
        board_id: &str,
        actor: Actor<'_>,
    ) -> DbResult<SwimlaneMergeOutcome> {
        let txn = self.db.begin_write()?;
        let Some(board) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(SwimlaneMergeOutcome::NotFound);
        };
        let mut lanes: Vec<SwimlaneRow> =
            txn_indexed_rows(&txn, SWIMLANES_BY_BOARD, board_id, SWIMLANES)?;
        lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        if lanes.len() < 2 {
            return Ok(SwimlaneMergeOutcome::RefusedSingle);
        }
        let Some(main_id) = board
            .main_swimlane(&lanes)
            .or_else(|| lanes.first().map(|lane| lane.id.clone()))
        else {
            return Ok(SwimlaneMergeOutcome::NotFound);
        };
        let Some(main) = lanes.iter().find(|lane| lane.id == main_id).cloned() else {
            return Ok(SwimlaneMergeOutcome::NotFound);
        };
        let detail = format!("Merged into swimlane {}", main.name);

        let mut moved = 0;
        for column_id in txn_mmap_get(&txn, COLUMNS_BY_BOARD, board_id)? {
            let mut arriving = Vec::new();
            for lane in lanes.iter().filter(|lane| lane.id != main_id) {
                arriving.extend(txn_cell_tasks(&txn, &column_id, Some(lane.id.as_str()))?);
            }
            if arriving.is_empty() {
                continue;
            }
            let next = txn_cell_tasks(&txn, &column_id, Some(main_id.as_str()))?
                .iter()
                .map(|task| task.position)
                .fold(-1.0, f64::max)
                + 1.0;
            let mut tbl = txn.open_table(TASKS)?;
            for (index, mut task) in arriving.into_iter().enumerate() {
                task.swimlane_id = Some(main_id.clone());
                task.position = next + index as f64;
                task.updated_at = Some(Utc::now().to_rfc3339());
                push_event(&mut task, "swimlane_merged", detail.clone(), actor);
                txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow)?;
                moved += 1;
            }
            drop(tbl);
            txn_renumber_cell(&txn, &column_id, Some(main_id.as_str()))?;
        }
        let removed = lanes.len() - 1;
        for lane in lanes.iter().filter(|lane| lane.id != main_id) {
            txn_remove_owned::<SwimlaneRow>(&txn, SWIMLANES, &lane.id)?;
            mmap_remove(&txn, SWIMLANES_BY_BOARD, board_id, &lane.id)?;
        }
        let mut main = main;
        main.collapsed = false;
        txn_store_owned(&txn, SWIMLANES, &main_id, &main, OverQuota::Allow)?;
        txn.commit()?;
        Ok(SwimlaneMergeOutcome::Merged {
            swimlanes: removed,
            tasks: moved,
        })
    }

    /// Deletes a swimlane unless it is the board's last, and moves its tasks into the lane above
    /// it, or into the next lane when it is the first. Each task keeps its column and lands after
    /// the tasks already in that cell. The refusal, the move and the delete share one write
    /// transaction.
    pub fn delete_swimlane(&self, id: &str) -> DbResult<SwimlaneDeleteOutcome> {
        let txn = self.db.begin_write()?;
        let Some(lane) = txn_read_one::<SwimlaneRow>(&txn, SWIMLANES, id)? else {
            return Ok(SwimlaneDeleteOutcome::NotFound);
        };
        let mut lanes: Vec<SwimlaneRow> =
            txn_indexed_rows(&txn, SWIMLANES_BY_BOARD, &lane.board_id, SWIMLANES)?;
        lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        let Some(at) = lanes.iter().position(|row| row.id == id) else {
            return Ok(SwimlaneDeleteOutcome::NotFound);
        };
        let neighbor = if at > 0 { at - 1 } else { at + 1 };
        let Some(target) = lanes.get(neighbor) else {
            return Ok(SwimlaneDeleteOutcome::RefusedLast);
        };

        for column_id in txn_mmap_get(&txn, COLUMNS_BY_BOARD, &lane.board_id)? {
            let leaving = txn_cell_tasks(&txn, &column_id, Some(id))?;
            if leaving.is_empty() {
                continue;
            }
            let next = txn_cell_tasks(&txn, &column_id, Some(target.id.as_str()))?
                .iter()
                .map(|task| task.position)
                .fold(-1.0, f64::max)
                + 1.0;
            let mut tbl = txn.open_table(TASKS)?;
            for (index, mut task) in leaving.into_iter().enumerate() {
                task.swimlane_id = Some(target.id.clone());
                task.position = next + index as f64;
                task.updated_at = Some(Utc::now().to_rfc3339());
                txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow)?;
            }
            drop(tbl);
            txn_renumber_cell(&txn, &column_id, Some(target.id.as_str()))?;
        }
        txn_remove_owned::<SwimlaneRow>(&txn, SWIMLANES, id)?;
        mmap_remove(&txn, SWIMLANES_BY_BOARD, &lane.board_id, id)?;

        // Deleting the main lane hands the role to the next lane
        let board = txn_read_one::<BoardRow>(&txn, BOARDS, &lane.board_id)?;
        if board.is_some_and(|board| board.main_swimlane_id().as_deref() == Some(id)) {
            txn_merge_board_config(&txn, &lane.board_id, main_swimlane_entry(&target.id))?;
        }
        txn.commit()?;
        Ok(SwimlaneDeleteOutcome::Deleted)
    }

    /// Column description (shown in the column settings dialog).
    pub fn set_column_description(&self, id: &str, description: &str) -> DbResult<bool> {
        TextCap::DESCRIPTION.check(description)?;
        mutate_owned(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.description = description.to_string();
        })
    }

    /// Collapse/expand a column on the board.
    pub fn set_column_collapsed(&self, id: &str, collapsed: bool) -> DbResult<bool> {
        mutate_owned(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.collapsed = collapsed;
        })
    }

    /// Opaque settings bag for the column dialog (column sums, sorting,
    /// group-by-date, display options). Callers validate it is JSON.
    pub fn set_column_config(&self, id: &str, config_json: &str) -> DbResult<bool> {
        TextCap::CONFIG.check(config_json)?;
        mutate_owned(&self.db, COLUMNS, id, |column: &mut ColumnRow| {
            column.config_json = config_json.to_string();
        })
    }

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
    /// A standard color reads back with the palette's current hex values, so rows stored before a
    /// palette correction render correctly.
    pub fn color(&self, id: &str) -> DbResult<Option<ColorRow>> {
        let color: Option<ColorRow> = read_one(&self.db, TASK_COLORS, id)?;
        Ok(color.map(ColorRow::with_palette_hexes))
    }

    /// Colors of a board, ordered by sort_order (then id for stability). Read-only: boards that
    /// predate colors are backfilled at startup instead.
    pub fn list_colors(&self, board_id: &str) -> DbResult<Vec<ColorRow>> {
        let mut colors = Vec::new();
        for id in mmap_get(&self.db, COLORS_BY_BOARD, board_id)? {
            if let Some(color) = self.color(&id)? {
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
    /// A board that doesn't exist (e.g. after deletion) gets none, so
    /// list_colors on a deleted board stays empty.
    pub fn ensure_board_colors(&self, board_id: &str) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        if txn_ensure_board_colors(&txn, board_id)? {
            txn.commit()?;
        }
        Ok(())
    }

    /// Every board needs at least one swimlane. The board's add-task form clones into the first
    /// swimlane row's cell, so a board with zero swimlanes (e.g. created without a template before
    /// this fix) leaves the column "+" buttons silently dead. Backfills a "Default" lane when the
    /// board has none. A board that doesn't exist (e.g. after deletion) gets none, so
    /// list_swimlanes on a deleted board stays empty.
    pub fn ensure_default_swimlane(&self, board_id: &str) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        let lanes: Vec<SwimlaneRow> =
            txn_indexed_rows(&txn, SWIMLANES_BY_BOARD, board_id, SWIMLANES)?;
        if !lanes.is_empty() || txn_read_one::<BoardRow>(&txn, BOARDS, board_id)?.is_none() {
            return Ok(());
        }
        txn_insert_swimlane(&txn, board_id, "Default", OverQuota::Allow)?;
        txn.commit()?;
        Ok(())
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
        TextCap::COLOR_DESCRIPTION.check(description.unwrap_or_default())?;
        let txn = self.db.begin_write()?;
        let colors = txn_board_colors(&txn, board_id)?;
        if colors.iter().any(|color| color.value == value) {
            return Err(format!("color '{value}' already exists on this board").into());
        }
        let id = txn_insert_color(
            &txn,
            board_id,
            NewColor {
                value,
                label,
                description: description.unwrap_or_default(),
                enabled: true,
                is_default: false,
                sort_order: next_position(colors.iter().map(|color| color.sort_order)),
            },
            OverQuota::Refuse,
        )?;
        txn.commit()?;
        Ok(id)
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
        if let Some(description) = description {
            TextCap::COLOR_DESCRIPTION.check(description)?;
        }
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<ColorRow>(&txn, TASK_COLORS, id)? else {
            return Ok(false);
        };
        if is_default == Some(true) {
            // Clear the default flag on the board's other colors first
            for mut other in txn_board_colors(&txn, &row.board_id)? {
                if other.id != id && other.is_default {
                    other.is_default = false;
                    txn_store_owned(&txn, TASK_COLORS, &other.id, &other, OverQuota::Allow)?;
                }
            }
        }
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
        txn_store_owned(&txn, TASK_COLORS, id, &row, OverQuota::Refuse)?;
        txn.commit()?;
        Ok(true)
    }

    /// Make `color_id` the board's default color, clearing the flag on the
    /// board's other colors. Returns false when the color is unknown or on
    /// a different board.
    pub fn set_board_default_color(&self, board_id: &str, color_id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        match txn_read_one::<ColorRow>(&txn, TASK_COLORS, color_id)? {
            Some(color) if color.board_id == board_id => {}
            _ => return Ok(false),
        }
        for mut row in txn_board_colors(&txn, board_id)? {
            row.is_default = row.id == color_id;
            txn_store_owned(&txn, TASK_COLORS, &row.id, &row, OverQuota::Allow)?;
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
        for (index, mut row) in colors.into_iter().enumerate() {
            row.sort_order = index as i64;
            txn_store_owned(&txn, TASK_COLORS, &row.id, &row, OverQuota::Allow)?;
        }
        txn.commit()?;
        Ok(true)
    }

    /// Typed outcome for color deletion so handlers can map refusals to 400.
    pub fn delete_color(&self, id: &str) -> DbResult<ColorDeleteOutcome> {
        // The checks and the removal share one write transaction, so a task cannot take the color
        // in between.
        let txn = self.db.begin_write()?;
        let color = match txn_read_one::<ColorRow>(&txn, TASK_COLORS, id)? {
            Some(color) => color,
            None => return Ok(ColorDeleteOutcome::NotFound),
        };
        if color.is_default {
            return Ok(ColorDeleteOutcome::RefusedDefault);
        }
        let in_use = board_color_uses_in(&txn, &color.board_id, id)?;
        if in_use > 0 {
            return Ok(ColorDeleteOutcome::RefusedInUse(in_use));
        }
        txn_remove_owned::<ColorRow>(&txn, TASK_COLORS, id)?;
        mmap_remove(&txn, COLORS_BY_BOARD, &color.board_id, id)?;
        txn.commit()?;
        Ok(ColorDeleteOutcome::Deleted)
    }

    // ---- Board templates ----

    /// Returns the built-in templates and those `owner` saved, built-ins first, then by name,
    /// without reading a snapshot.
    pub fn list_templates(&self, owner: &AccountId) -> DbResult<Vec<BoardTemplateRow>> {
        let txn = self.read()?;
        let mut out = txn_templates_of(&txn, BUILT_IN_TEMPLATES)?;
        out.extend(txn_templates_of(&txn, owner.as_str())?);
        out.sort_by(|a, b| {
            b.built_in
                .cmp(&a.built_in)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    pub fn template(&self, id: &str) -> DbResult<Option<BoardTemplateRow>> {
        read_one(&self.db, TEMPLATES, id)
    }

    /// Typed outcome: built-in templates cannot be deleted.
    pub fn delete_template(&self, id: &str) -> DbResult<TemplateDeleteOutcome> {
        let template = match self.template(id)? {
            Some(template) => template,
            None => return Ok(TemplateDeleteOutcome::NotFound),
        };
        if template.built_in {
            return Ok(TemplateDeleteOutcome::RefusedBuiltIn);
        }
        let txn = self.db.begin_write()?;
        if let Some((removed, freed)) = txn_remove_template(&txn, id)?
            && let Some(owner) = &removed.owner_id
        {
            txn_count_usage(&txn, owner, 0, freed)?;
        }
        txn.commit()?;
        Ok(TemplateDeleteOutcome::Deleted)
    }

    /// Capture a board's colors (all, with their config), columns,
    /// swimlanes, and tasks as a reusable template. Returns None when the
    /// board is unknown.
    pub fn save_board_as_template(
        &self,
        owner: &AccountId,
        board_id: &str,
        name: &str,
        description: &str,
    ) -> DbResult<Option<String>> {
        TextCap::TEMPLATE_NAME.check(name)?;
        TextCap::DESCRIPTION.check(description)?;
        if !self.board_exists(board_id)? {
            return Ok(None);
        }
        let snapshot = self.write_template_snapshot(board_id, &self.board_task_ids(board_id)?)?;
        let mut row = BoardTemplateRow {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: description.to_string(),
            built_in: false,
            owner_id: Some(owner.as_str().to_string()),
            snapshot_bytes: snapshot.len() as u64,
        };
        let bytes = serde_json::to_vec(&row)?.len() as u64 + row.snapshot_bytes;
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, owner)?;
        let held = index_in(&txn, TEMPLATES_BY_OWNER, owner.as_str())?.len();
        txn_refuse_over_quota(&txn, owner.as_str(), Quota::TemplatesPerAccount, held + 1)?;
        txn_refuse_over_bytes(&txn, owner.as_str(), bytes, 0)?;
        txn_count_usage(&txn, owner.as_str(), bytes, 0)?;
        txn_store_template(&txn, &mut row, &snapshot)?;
        txn.commit()?;
        Ok(Some(row.id))
    }

    /// Returns the template snapshot of board `board_id` with the tasks `task_ids` names, in that
    /// order, skipping any that are gone. The snapshot is written one task at a time and refused
    /// with [`QuotaExceeded`] once it passes [`MAX_TEMPLATE_ROW_BYTES`].
    fn write_template_snapshot(&self, board_id: &str, task_ids: &[String]) -> DbResult<Vec<u8>> {
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

        // A template keeps task content and layout (columns and swimlanes by name and position), never per-instance state like history or comments
        // Columns are also captured by position, since names can repeat
        let column_order: Vec<ColumnRow> = self.list_columns(board_id)?;
        let column_indexes: HashMap<&str, usize> = column_order
            .iter()
            .enumerate()
            .map(|(index, column)| (column.id.as_str(), index))
            .collect();
        let column_names: HashMap<String, String> = column_order
            .iter()
            .map(|column| (column.id.clone(), column.name.clone()))
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
        let due_time = self
            .board(board_id)?
            .map(|board| board.due_time().format("%H:%M").to_string());

        // The snapshot is written one task at a time and refused once it passes the cap
        let mut snapshot = Vec::new();
        snapshot.extend_from_slice(b"{\"due_time\":");
        serde_json::to_writer(&mut snapshot, &due_time)?;
        for (key, value) in [
            ("colors", colors),
            ("columns", columns),
            ("swimlanes", swimlanes),
        ] {
            snapshot.extend_from_slice(format!(",\"{key}\":").as_bytes());
            serde_json::to_writer(&mut snapshot, &value)?;
        }
        snapshot.extend_from_slice(b",\"tasks\":[");
        let mut wrote_task = false;
        for task_id in task_ids {
            let Some(task) = self.task(task_id)? else {
                continue;
            };
            let task = serde_json::json!({
                "name": task.name,
                "description": task.description,
                "size": task.size,
                "color_value": task
                    .color_id
                    .as_deref()
                    .and_then(|id| color_values.get(id)),
                "column": column_names.get(&task.column_id),
                "column_index": column_indexes.get(task.column_id.as_str()),
                "swimlane": task
                    .swimlane_id
                    .as_deref()
                    .and_then(|id| lane_names.get(id)),
                "position": task.position,
                "due_at": task.due_at,
                "due_repeat": task.due_repeat.as_ref().filter(|_| task.due_at.is_some()),
                "due_target": task
                    .due_target_column_id
                    .as_deref()
                    .and_then(|id| column_names.get(id)),
                "due_target_index": task
                    .due_target_column_id
                    .as_deref()
                    .and_then(|id| column_indexes.get(id)),
                "estimate_hours": task.estimate_hours,
                "subtasks": task.subtasks,
                "labels": task.labels,
            });
            if wrote_task {
                snapshot.push(b',');
            }
            serde_json::to_writer(&mut snapshot, &task)?;
            wrote_task = true;
            refuse_template_past(&snapshot)?;
        }
        snapshot.extend_from_slice(b"]}");
        refuse_template_past(&snapshot)?;
        Ok(snapshot)
    }

    /// Builds a new board for `owner` from a template snapshot: board row, its colors,
    /// columns, swimlanes, and captured tasks, all in one write transaction.
    /// Returns the new board id, or None when the template is unknown.
    pub fn apply_template(
        &self,
        owner: &AccountId,
        template_id: &str,
        new_board_name: &str,
    ) -> DbResult<Option<String>> {
        self.apply_template_by(owner, template_id, new_board_name, "")
    }

    /// [`Db::apply_template`], recording the API token `created_by_token` that asked for the
    /// board, or none when the string is empty.
    pub fn apply_template_by(
        &self,
        owner: &AccountId,
        template_id: &str,
        new_board_name: &str,
        created_by_token: &str,
    ) -> DbResult<Option<String>> {
        let txn = self.db.begin_write()?;
        let Some(snapshot) = txn_template_snapshot(&txn, template_id)? else {
            return Ok(None);
        };
        let board_id = Self::build_from_template(
            &txn,
            owner,
            &snapshot,
            new_board_name,
            created_by_token,
            OverQuota::Refuse,
        )?;
        let held = txn_board_task_count(&txn, &board_id)?;
        txn_refuse_board_over_quota(&txn, &board_id, Quota::TasksPerBoard, held)?;
        txn.commit()?;
        Ok(Some(board_id))
    }

    /// Builds a board named `name` for `owner` from a template's `snapshot` inside an open write
    /// transaction, and returns its id.
    fn build_from_template(
        txn: &WriteTransaction,
        owner: &AccountId,
        snapshot: &serde_json::Value,
        name: &str,
        created_by_token: &str,
        over: OverQuota,
    ) -> DbResult<String> {
        let board_id = txn_insert_board(txn, owner, name, created_by_token, over)?;

        // The board's "Due date time" travels with the template
        if let Some(due_time) = snapshot
            .get("due_time")
            .and_then(|value| value.as_str())
            .filter(|raw| crate::models::parse_due_time(raw).is_some())
        {
            let mut updates = serde_json::Map::new();
            updates.insert("due_time".to_string(), due_time.into());
            txn_merge_board_config(txn, &board_id, updates)?;
        }

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
                txn_insert_color(
                    txn,
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
                    over,
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
                txn_insert_column(
                    txn,
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
                    over,
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
                txn_insert_swimlane(
                    txn,
                    &board_id,
                    lane.get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("Swimlane"),
                    over,
                )?;
            }
        }

        // Snapshots that carry no colors (hand-edited templates) still get
        // the standard palette, and every board keeps exactly one default.
        txn_ensure_board_colors(txn, &board_id)?;
        txn_ensure_single_default(txn, &board_id)?;
        Self::apply_template_tasks(txn, &board_id, snapshot, over)?;
        Ok(board_id)
    }

    /// Restores tasks captured in a template snapshot onto a freshly built board. Columns,
    /// swimlanes, and colors are resolved by name/value (ids differ per board), and unmapped values
    /// fall back gracefully. Timer stats are not part of a template, so new tasks start fresh and
    /// those in a done column are complete as of now.
    fn apply_template_tasks(
        txn: &WriteTransaction,
        board_id: &str,
        snapshot: &serde_json::Value,
        over: OverQuota,
    ) -> DbResult<()> {
        let tasks = match snapshot.get("tasks").and_then(|value| value.as_array()) {
            Some(tasks) if !tasks.is_empty() => tasks,
            _ => return Ok(()),
        };
        let mut columns: Vec<ColumnRow> =
            txn_indexed_rows(txn, COLUMNS_BY_BOARD, board_id, COLUMNS)?;
        columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        let fallback_column: Option<String> = columns.first().map(|column| column.id.clone());
        let done_columns: Vec<String> = columns
            .iter()
            .filter(|column| column.is_done)
            .map(|column| column.id.clone())
            .collect();

        // The snapshot's column order is the new board's order. A template without indexes resolves
        // by name, first column winning.
        let ordered_ids: Vec<String> = columns.iter().map(|column| column.id.clone()).collect();
        let mut column_ids: HashMap<String, String> = HashMap::new();
        for column in columns {
            column_ids.entry(column.name).or_insert(column.id);
        }
        let resolve_column = |task: &serde_json::Value, name_key: &str, index_key: &str| {
            let by_index = task
                .get(index_key)
                .and_then(|value| value.as_u64())
                .and_then(|index| ordered_ids.get(index as usize));
            by_index.cloned().or_else(|| {
                task.get(name_key)
                    .and_then(|value| value.as_str())
                    .and_then(|name| column_ids.get(name).cloned())
            })
        };
        let mut lanes: Vec<SwimlaneRow> =
            txn_indexed_rows(txn, SWIMLANES_BY_BOARD, board_id, SWIMLANES)?;
        lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        let lane_ids: HashMap<String, String> =
            lanes.into_iter().map(|lane| (lane.name, lane.id)).collect();
        let color_ids: HashMap<String, String> = txn_board_colors(txn, board_id)?
            .into_iter()
            .map(|color| (color.value, color.id))
            .collect();
        for task in tasks {
            let column_id =
                resolve_column(task, "column", "column_index").or_else(|| fallback_column.clone());
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
            let due_at = task
                .get("due_at")
                .and_then(|value| value.as_str())
                .map(str::to_string);

            // The target column is resolved by name on the new board
            let due_target =
                resolve_column(task, "due_target", "due_target_index").filter(|_| due_at.is_some());

            // A repeat needs a due date and older snapshots may lack one
            let due_repeat = task
                .get("due_repeat")
                .and_then(|value| value.as_str())
                .filter(|_| due_at.is_some())
                .map(str::to_string);
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
                completed_at: done_columns.contains(&column_id).then(|| now.clone()),
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
                due_at,
                due_repeat,
                due_done: false,
                due_target_column_id: due_target,
                estimate_hours: task.get("estimate_hours").and_then(|v| v.as_f64()),
                column_entered_at: Some(now.clone()),
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
            txn_store_task(txn, &mut txn.open_table(TASKS)?, &row, over)?;
            mmap_insert(txn, TASKS_BY_COLUMN, &column_id, &id)?;
        }
        Ok(())
    }

    // ---- Tasks ----

    pub fn task(&self, id: &str) -> DbResult<Option<TaskRow>> {
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

    pub fn entry(&self, id: &str) -> DbResult<Option<TimeEntryRow>> {
        read_one(&self.db, TIME_ENTRIES, id)
    }

    pub fn task_total_minutes(&self, task_id: &str) -> DbResult<i64> {
        let mut total = 0;
        for entry_id in mmap_get(&self.db, ENTRIES_BY_TASK, task_id)? {
            if let Some(entry) = self.entry(&entry_id)? {
                total += entry.minutes;
            }
        }
        Ok(total)
    }

    /// One task with its total logged minutes filled in.
    pub fn task_with_minutes(&self, id: &str) -> DbResult<Option<TaskRow>> {
        match self.task(id)? {
            Some(mut task) => {
                task.total_minutes = self.task_total_minutes(id)?;
                Ok(Some(task))
            }
            None => Ok(None),
        }
    }

    /// Create a task appended at the end of its cell; returns its id.
    /// `color_id` selects the task's color (validated by the caller; one
    /// deleted since fails with [`UNKNOWN_COLOR`]); None keeps the legacy
    /// size-based coloring. Also backfills the board's color palette for
    /// boards created before colors existed.
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

    /// Returns a new, unsaved task row for [`Db::insert_task`], stamped now.
    #[must_use]
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
            column_entered_at: Some(now.clone()),
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
    /// write; returns its id. A `color_id` deleted since fails with
    /// [`UNKNOWN_COLOR`]. Also backfills the board's color palette for
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
        let txn = self.db.begin_write()?;
        if let Some(column) = txn_read_one::<ColumnRow>(&txn, COLUMNS, &row.column_id)? {
            let held = txn_board_task_count(&txn, &column.board_id)?;
            txn_refuse_board_over_quota(&txn, &column.board_id, Quota::TasksPerBoard, held + 1)?;
            txn_ensure_board_colors(&txn, &column.board_id)?;

            // A task created in a done column is complete from the start
            if column.is_done && row.completed_at.is_none() {
                let added = row.column_entered_at.clone();
                row.completed_at = Some(added.unwrap_or_else(|| Utc::now().to_rfc3339()));
            }
        }
        if let Some(color_id) = &row.color_id
            && txn_read_one::<ColorRow>(&txn, TASK_COLORS, color_id)?.is_none()
        {
            return Err(UNKNOWN_COLOR.into());
        }
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
                if let Some(task) = task_in(&tasks, task_id)?
                    && task.swimlane_id == row.swimlane_id
                {
                    let p = task.position;
                    bounds = Some(bounds.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))));
                }
            }
            row.position = match (bounds, placement) {
                (None, _) => 0.0,
                (Some((lo, _)), NewTaskPlacement::Top) => lo - 1.0,
                (Some((_, hi)), NewTaskPlacement::Bottom) => hi + 1.0,
            };
        }
        txn_store_task(&txn, &mut txn.open_table(TASKS)?, &row, OverQuota::Refuse)?;
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
            due_at: None,
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
        // An unknown subtask writes nothing
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
        // An unknown subtask writes nothing
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

    /// Sets (Some) or clears (None) a task's due date (RFC3339) and repeat text (KanbanFlow parity:
    /// "Add due date"). Without a due date the repeat is cleared too. Returns false when the task
    /// does not exist.
    pub fn set_task_due(
        &self,
        task_id: &str,
        due_at: Option<&str>,
        repeat: Option<&str>,
    ) -> DbResult<bool> {
        let due_at = due_at.map(str::to_string);
        let repeat = repeat
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty() && due_at.is_some());
        mutate_task(&self.db, task_id, |task| {
            task.due_at = due_at.clone();
            task.due_repeat = repeat.clone();

            // Clearing the due date also clears its done flag and target column, since no date is
            // left to reach
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

    /// Sets (or clears, with None) a task's hour-based time estimate. Hours must already be
    /// validated by the caller (`normalize_estimate_hours`), and values are rounded to two decimals
    /// to avoid float noise.
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
        new: NewComment<'_>,
        actor: Actor<'_>,
        fence: Option<u64>,
    ) -> DbResult<FencedWrite<TaskComment>> {
        let comment = TaskComment {
            id: Uuid::new_v4().to_string(),
            author: new.author.to_string(),
            body: new.body.to_string(),
            created_at: Utc::now().to_rfc3339(),
            actor_kind: new.actor_kind,
            token_name: new.token_name.map(str::to_string),
            token_id: new.token_id.filter(|id| !id.is_empty()).map(str::to_string),
            edited_at: None,
            kind: new.kind.map(str::to_string),
        };
        let txn = self.db.begin_write()?;
        let owner = task_owner_in(&txn, task_id)?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tbl, task_id)? else {
                return Ok(FencedWrite::NotFound);
            };
            if !task.fence_allows(fence) {
                return Ok(FencedWrite::StaleFence);
            }
            if let Some(owner) = &owner {
                let after = task.comments.len() + 1;
                txn_refuse_over_quota(&txn, owner, Quota::CommentsPerTask, after)?;
            }
            task.comments.push(comment.clone());
            let detail = format!("{} added a comment", new.author);
            push_event(&mut task, "comment_added", detail, actor);
            task.updated_at = Some(Utc::now().to_rfc3339());
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse)?;
        }
        txn.commit()?;
        Ok(FencedWrite::Written(comment))
    }

    /// Deletes a comment from a task, logging the deletion in the same
    /// write. With an `owner`, only a comment that API token wrote may go.
    pub fn delete_comment(
        &self,
        task_id: &str,
        comment_id: &str,
        owner: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<CommentWrite<()>> {
        let outcome = self.apply_task_change(task_id, |task| {
            let Some(comment) = task.comments.iter().find(|c| c.id == comment_id) else {
                return (CommentWrite::NotFound, false);
            };
            if !comment_is_by(comment, owner) {
                return (CommentWrite::NotOwner, false);
            }
            task.comments.retain(|c| c.id != comment_id);
            push_event(task, "comment_deleted", "Deleted a comment".into(), actor);
            (CommentWrite::Done(()), true)
        })?;
        Ok(outcome.unwrap_or(CommentWrite::NotFound))
    }

    /// Replaces the body of a comment and stamps `edited_at`, logging the edit in
    /// the same write. Every other field of the comment stays as written. With
    /// an `owner`, only a comment that API token wrote may change.
    pub fn edit_comment(
        &self,
        task_id: &str,
        comment_id: &str,
        body: &str,
        owner: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<CommentWrite<TaskComment>> {
        let outcome = self.apply_task_change(task_id, |task| {
            let Some(comment) = task.comments.iter_mut().find(|c| c.id == comment_id) else {
                return (CommentWrite::NotFound, false);
            };
            if !comment_is_by(comment, owner) {
                return (CommentWrite::NotOwner, false);
            }
            comment.body = body.to_string();
            comment.edited_at = Some(Utc::now().to_rfc3339());
            let edited = comment.clone();
            push_event(task, "comment_edited", "Edited a comment".into(), actor);
            (CommentWrite::Done(edited), true)
        })?;
        Ok(outcome.unwrap_or(CommentWrite::NotFound))
    }

    /// Link `task_id` to `target_id` in one transaction. A `blocks` link is
    /// stored as `blocked_by` on the target; asking for a link that already
    /// exists returns it unchanged.
    pub fn add_task_link(
        &self,
        task_id: &str,
        kind: LinkKind,
        target_id: &str,
        actor: Actor<'_>,
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
            let detail = link_event_detail(&link, Some(&other.name), |detail| {
                format!("Added link {detail}")
            });
            push_event(&mut holder, "link_added", detail, actor);
            holder.updated_at = Some(now);
            txn_store_task(&txn, &mut tbl, &holder, OverQuota::Refuse)?;
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
        let Some(task) = self.task_head(task_id)? else {
            return Ok(None);
        };
        let mut links = task.links;
        for holder_id in mmap_get(&self.db, LINKS_BY_TARGET, task_id)? {
            let Some(holder) = self.task_head(&holder_id)? else {
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
        let Some(task) = self.task_head(task_id)? else {
            return Ok(None);
        };
        Ok(self.column(&task.column_id)?.map(|column| column.board_id))
    }

    /// Delete a link listed on `task_id`, whichever task holds it. Returns
    /// false when the task or the link does not exist.
    pub fn delete_task_link(
        &self,
        task_id: &str,
        link_id: &str,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
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
                let detail = link_event_detail(&link, target_name.as_deref(), |detail| {
                    format!("Removed link {detail}")
                });
                push_event(&mut holder, "link_removed", detail, actor);
                let still_linked = holder
                    .links
                    .iter()
                    .any(|other| other.target_id == link.target_id);
                holder.updated_at = Some(Utc::now().to_rfc3339());
                txn_store_task(&txn, &mut tbl, &holder, OverQuota::Allow)?;
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
        self.links_blocked(&task.links)
    }

    /// Returns true when a `blocked_by` link among `links` targets a task in a column that is not
    /// done.
    pub fn links_blocked(&self, links: &[TaskLink]) -> DbResult<bool> {
        links_blocked_in(&self.read()?, links, &mut HashMap::new())
    }

    pub fn field(&self, id: &str) -> DbResult<Option<FieldDefRow>> {
        read_one(&self.db, FIELD_DEFS, id)
    }

    /// A board's custom fields, ordered by position, then creation time.
    pub fn list_fields(&self, board_id: &str) -> DbResult<Vec<FieldDefRow>> {
        let mut fields = Vec::new();
        for id in mmap_get(&self.db, FIELDS_BY_BOARD, board_id)? {
            if let Some(field) = self.field(&id)? {
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
        field.check_caps()?;
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
        let held = txn_mmap_get(&txn, FIELDS_BY_BOARD, &field.board_id)?.len();
        txn_refuse_board_over_quota(&txn, &field.board_id, Quota::FieldsPerBoard, held + 1)?;
        txn_store_owned(&txn, FIELD_DEFS, &field.id, &field, OverQuota::Refuse)?;
        mmap_insert(&txn, FIELDS_BY_BOARD, &field.board_id, &field.id)?;
        txn.commit()?;
        Ok(FieldCreateOutcome::Created(Box::new(field)))
    }

    /// Run `f` on a stored field definition inside one write transaction,
    /// storing it when `f` reports a change. Returns [`None`] when the field is unknown.
    pub fn apply_field_change<R, F>(&self, id: &str, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut FieldDefRow) -> (R, bool),
    {
        let txn = self.db.begin_write()?;
        let result = {
            let Some(mut field) = txn_read_one::<FieldDefRow>(&txn, FIELD_DEFS, id)? else {
                return Ok(None);
            };
            let (result, changed) = f(&mut field);
            if changed {
                field.check_caps()?;
                txn_store_owned(&txn, FIELD_DEFS, id, &field, OverQuota::Refuse)?;
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

    /// Runs `f` on a stored field definition and on every task of its board inside one write
    /// transaction. When `f` returns the new definition, it stores it together with each task
    /// whose `fields` it changed; an error from `f` writes nothing. The tasks come in board order,
    /// by column position and then task position. Returns None when unknown.
    pub fn convert_field<E, F>(&self, id: &str, f: F) -> DbResult<Option<Result<FieldDefRow, E>>>
    where
        F: FnOnce(&FieldDefRow, &mut [TaskRow]) -> Result<FieldDefRow, E>,
    {
        let txn = self.db.begin_write()?;
        let stored: Option<FieldDefRow> = txn
            .open_table(FIELD_DEFS)?
            .get(id)?
            .map(|guard| serde_json::from_slice(guard.value()))
            .transpose()?;
        let Some(field) = stored else {
            return Ok(None);
        };
        let mut columns = Vec::new();
        {
            let table = txn.open_table(COLUMNS)?;
            for column_id in txn_mmap_get(&txn, COLUMNS_BY_BOARD, &field.board_id)? {
                let column: Option<ColumnRow> = table
                    .get(column_id.as_str())?
                    .map(|guard| serde_json::from_slice(guard.value()))
                    .transpose()?;
                columns.extend(column);
            }
        }
        columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        let mut tasks = Vec::new();
        {
            let table = txn.open_table(TASKS)?;
            for column in &columns {
                let mut in_column = Vec::new();
                for task_id in txn_mmap_get(&txn, TASKS_BY_COLUMN, &column.id)? {
                    in_column.extend(task_in(&table, &task_id)?);
                }
                in_column.sort_by(|a, b| {
                    a.position
                        .total_cmp(&b.position)
                        .then_with(|| a.id.cmp(&b.id))
                });
                tasks.extend(in_column);
            }
        }
        let before: Vec<_> = tasks.iter().map(|task| task.fields.clone()).collect();
        let next = match f(&field, &mut tasks) {
            Ok(next) => next,
            Err(error) => {
                txn.abort()?;
                return Ok(Some(Err(error)));
            }
        };
        next.check_caps()?;
        txn_store_owned(&txn, FIELD_DEFS, id, &next, OverQuota::Refuse)?;
        {
            let mut table = txn.open_table(TASKS)?;
            let now = Utc::now().to_rfc3339();
            for (task, before) in tasks.iter_mut().zip(before) {
                if task.fields != before {
                    task.updated_at = Some(now.clone());
                    txn_store_task(&txn, &mut table, task, OverQuota::Refuse)?;
                }
            }
        }
        txn.commit()?;
        Ok(Some(Ok(next)))
    }

    /// Delete a field and, in the same transaction, its value on every task
    /// of the board. Returns false when unknown.
    pub fn delete_field(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(field) = txn_remove_owned::<FieldDefRow>(&txn, FIELD_DEFS, id)? else {
            return Ok(false);
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
                    txn_store_task(&txn, &mut tasks, &task, OverQuota::Allow)?;
                }
            }
        }
        mmap_remove(&txn, FIELDS_BY_BOARD, &field.board_id, id)?;
        txn.commit()?;
        Ok(true)
    }

    /// Store a new device-pairing request, first deleting requests past their retention. All in
    /// one transaction. The request is refused while its client network has
    /// [`Db::MAX_PENDING_DEVICE_AUTHS_PER_NETWORK`] requests pending, or every network together
    /// [`Db::MAX_PENDING_DEVICE_AUTHS`].
    pub fn create_device_auth(
        &self,
        row: &DeviceAuthRow,
        now: i64,
    ) -> DbResult<DeviceAuthCreateOutcome> {
        let txn = self.db.begin_write()?;
        let outcome = {
            let mut tbl = txn.open_table(DEVICE_AUTHS)?;
            let count = count_pending(&txn, &tbl, row, now)?;
            if count.code_taken {
                return Ok(DeviceAuthCreateOutcome::UserCodeTaken);
            }
            for (key, stale) in &count.stale {
                tbl.remove(key.as_str())?;
                if let Some(account) = &stale.account_id {
                    mmap_remove(&txn, DEVICES_BY_ACCOUNT, account, key)?;
                }
                mmap_remove(&txn, DEVICES_BY_USER_CODE, &stale.user_code, key)?;
                txn_unindex_device_expiry(&txn, stale.expires_at, key)?;
            }
            if count.total >= Self::MAX_PENDING_DEVICE_AUTHS
                || count.here >= Self::MAX_PENDING_DEVICE_AUTHS_PER_NETWORK
            {
                DeviceAuthCreateOutcome::TooManyPending
            } else {
                let raw = serde_json::to_vec(row)?;
                tbl.insert(row.device_code_hash.as_str(), raw.as_slice())?;
                mmap_insert(
                    &txn,
                    DEVICES_BY_USER_CODE,
                    &row.user_code,
                    &row.device_code_hash,
                )?;
                txn_index_device_expiry(&txn, row.expires_at, &row.device_code_hash)?;
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
        for key in index_in(&txn, DEVICES_BY_USER_CODE, user_code)? {
            if let Some(row) = row_in::<DeviceAuthRow, _>(&tbl, &key)?
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
        for key in index_in(&txn, DEVICES_BY_USER_CODE, user_code)? {
            if let Some(row) = row_in::<DeviceAuthRow, _>(&tbl, &key)?
                && row.expires_at >= now
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Approves the live, pending request with this user code for `account` on the terms of
    /// `grant`, or denies it when `grant` is [`None`]. Returns false when there is none.
    pub fn decide_device_auth(
        &self,
        account: &AccountId,
        user_code: &str,
        grant: Option<&DeviceGrant>,
        now: i64,
    ) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let decided = {
            let mut tbl = txn.open_table(DEVICE_AUTHS)?;
            let mut found = None;
            for key in txn_mmap_get(&txn, DEVICES_BY_USER_CODE, user_code)? {
                if let Some(row) = row_in::<DeviceAuthRow, _>(&tbl, &key)?
                    && row.status == DeviceAuthStatus::Pending
                    && row.expires_at >= now
                {
                    found = Some((key, row));
                    break;
                }
            }
            match found {
                Some((key, mut row)) => {
                    row.status = match grant {
                        Some(grant) => {
                            row.scope = grant.scope;
                            row.token_expires_at = grant.expires_at;
                            row.account_id = Some(account.as_str().to_string());
                            mmap_insert(&txn, DEVICES_BY_ACCOUNT, account.as_str(), &key)?;
                            DeviceAuthStatus::Approved
                        }
                        None => DeviceAuthStatus::Denied,
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
                let approver = AccountId::new(row.account_id.clone().unwrap_or_default());
                match row.status {
                    DeviceAuthStatus::Pending => DevicePoll::Pending,
                    DeviceAuthStatus::Denied => DevicePoll::Denied,
                    DeviceAuthStatus::Approved if !txn_account_active(&txn, &approver)? => {
                        row.status = DeviceAuthStatus::Denied;
                        DevicePoll::Denied
                    }
                    DeviceAuthStatus::Approved => {
                        let record = mint(&row);
                        if let Some(refusal) = txn_token_cap_reached(&txn, &record.account_id)? {
                            DevicePoll::TooManyTokens(refusal)
                        } else {
                            txn_store_token(&txn, &record)?;
                            row.status = DeviceAuthStatus::Issued;
                            DevicePoll::Issued(Box::new(record))
                        }
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

    /// Attach a file's bytes to a task. The metadata is appended to the
    /// task row and the bytes go into the `attachment_data` table, both in
    /// one write transaction. Returns None when the task does not exist.
    pub fn add_attachment(
        &self,
        task_id: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        TextCap::ATTACHMENT_MIME.check(mime)?;
        let meta = TaskAttachment {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            mime: mime.to_string(),
            size: bytes.len() as u64,
            uploaded_by: uploaded_by.name.to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        let added = meta.clone();
        let txn = self.db.begin_write()?;
        let owner = task_owner_in(&txn, task_id)?;
        let found = {
            let mut tbl = txn.open_table(TASKS)?;
            let current: Option<TaskRow> = tbl
                .get(task_id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            match current {
                Some(mut task) => {
                    if let Some(owner) = &owner {
                        let after = task.attachments.len() + 1;
                        txn_refuse_over_quota(&txn, owner, Quota::AttachmentsPerTask, after)?;
                        let file_bytes = bytes.len() as u64;
                        txn_refuse_over_bytes(&txn, owner, file_bytes, 0)?;
                        txn_count_usage(&txn, owner, file_bytes, 0)?;
                    }
                    task.attachments.push(meta);
                    let detail = format!("Attached \"{name}\"");
                    push_event(&mut task, "attachment_added", detail, uploaded_by);
                    task.updated_at = Some(Utc::now().to_rfc3339());
                    txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse)?;
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

    /// Renames an attachment, keeping the extension of its current name, and logs the rename.
    /// Returns [`None`] and writes nothing if the task or the attachment does not exist.
    pub fn rename_attachment(
        &self,
        task_id: &str,
        attachment_id: &str,
        name: &str,
        actor: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        let txn = self.db.begin_write()?;
        let renamed = {
            let mut tbl = txn.open_table(TASKS)?;
            let current: Option<(TaskRow, u64)> = tbl
                .get(task_id)?
                .map(|guard| {
                    let raw = guard.value();
                    serde_json::from_slice(raw).map(|row| (row, raw.len() as u64))
                })
                .transpose()?;
            let Some((mut task, held)) = current else {
                return Ok(None);
            };
            let Some(attachment) = task.attachments.iter_mut().find(|a| a.id == attachment_id)
            else {
                return Ok(None);
            };
            let renamed = TaskAttachment::keep_extension(&attachment.name, name);
            TextCap::ATTACHMENT_NAME.check(&renamed)?;
            let detail = format!("Renamed \"{}\" to \"{renamed}\"", attachment.name);
            attachment.name = renamed;
            let updated = attachment.clone();
            push_event(&mut task, "attachment_renamed", detail, actor);
            task.updated_at = Some(Utc::now().to_rfc3339());
            let bytes = serde_json::to_vec(&task)?.len() as u64;
            refuse_row_past(Bound::TaskRowBytes, MAX_TASK_ROW_BYTES, bytes, held)?;
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow)?;
            updated
        };
        txn.commit()?;
        Ok(Some(renamed))
    }

    /// Delete an attachment: removes the metadata from the task row, the
    /// bytes from the data table, and logs the deletion, in one write.
    /// Returns false, writing nothing, when the task or the attachment does
    /// not exist.
    pub fn delete_attachment(
        &self,
        task_id: &str,
        attachment_id: &str,
        actor: Actor<'_>,
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
                let name = task
                    .attachments
                    .iter()
                    .find(|a| a.id == attachment_id)
                    .map(|a| a.name.clone());
                if let Some(name) = name {
                    task.attachments.retain(|a| a.id != attachment_id);
                    removed = true;
                    let detail = format!("Deleted \"{name}\"");
                    push_event(&mut task, "attachment_deleted", detail, actor);
                    task.updated_at = Some(Utc::now().to_rfc3339());
                    txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow)?;
                    let freed = txn
                        .open_table(ATTACHMENT_DATA)?
                        .remove(attachment_id)?
                        .map_or(0, |file| file.value().len() as u64);
                    if let Some(owner) = txn_column_owner(&txn, &task.column_id)? {
                        txn_count_usage(&txn, &owner, 0, freed)?;
                    }
                }
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Append an activity-trail event to a task (KanbanFlow parity:
    /// History report). No-op (Ok(false)) when the task does not exist.
    pub fn log_event(
        &self,
        task_id: &str,
        kind: &str,
        detail: &str,
        actor: Actor<'_>,
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
                txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse)?;
            }
            (result, write)
        };

        // A change that writes nothing commits nothing, so a rejected request leaves the database
        // file untouched.
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
        actor: Actor<'_>,
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

                // A renewal changes only the lease, like a heartbeat
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
    pub fn release_claim(
        &self,
        task_id: &str,
        fence: u64,
        actor: Actor<'_>,
    ) -> DbResult<ClaimOutcome> {
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
        let mut labels = BTreeSet::new();
        self.visit_board_tasks(board_id, |task: TaskLabels| {
            for entry in self.list_entries(&task.id)? {
                labels.extend(entry.labels);
            }
            labels.extend(task.labels);
            Ok(())
        })?;
        Ok(labels.into_iter().collect())
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
                txn_remove_entry(&txn, &mut tbl, entry_id)?;
                mmap_remove(&txn, ENTRIES_BY_TASK, &entry.task_id, entry_id)?;
                removed = true;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Moves a task to a new column/swimlane/position. Crossing into a done column stamps
    /// `completed_at`, and landing in a column that is not done clears it. The moved task is
    /// inserted at the client-sent index with its siblings renumbered densely around it, and the
    /// old cell is renumbered when it differs. Every read and write happens in one write
    /// transaction.
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

    /// [`Db::move_task`] with the checks in `guards`. Every read the move depends on, the checks,
    /// the move, both cells' renumbering, the field carry, the readiness bump, and the history
    /// event share one write transaction.
    pub fn move_task_logged(
        &self,
        id: &str,
        column_id: &str,
        swimlane_id: Option<&str>,
        position: f64,
        guards: MoveGuards<'_>,
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

        // A move within its own column never adds to that column
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
        let completed_at = match (target.is_done, was_done) {
            (true, false) => Some(Utc::now().to_rfc3339()),
            (true, true) => task.completed_at.clone(),
            (false, _) => None,
        };

        // Keep the current swimlane when the caller doesn't name one. Lanes are per-board, so a
        // cross-board move lands in the target board's first lane instead.
        let crosses_boards = old_column
            .as_ref()
            .is_some_and(|column| column.board_id != target.board_id);
        if crosses_boards {
            let held = txn_board_task_count(&txn, &target.board_id)?;
            txn_refuse_board_over_quota(&txn, &target.board_id, Quota::TasksPerBoard, held + 1)?;
        }
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

        // Colors are per board, so a cross-board move takes the target board's color of the same
        // value, else its default.
        let color_id = match task.color_id.as_deref() {
            Some(color_id) if crosses_boards => {
                let value =
                    txn_read_one::<ColorRow>(&txn, TASK_COLORS, color_id)?.map(|color| color.value);
                let colors = txn_board_colors(&txn, &target.board_id)?;
                colors
                    .iter()
                    .find(|color| Some(&color.value) == value.as_ref())
                    .or_else(|| colors.iter().find(|color| color.is_default))
                    .map(|color| color.id.clone())
            }
            _ => task.color_id.clone(),
        };

        // Explicit index insertion: the target cell's other tasks keep their relative order and the
        // moved task lands exactly at the client-sent index.
        let mut siblings = txn_cell_tasks(&txn, column_id, new_swimlane_id.as_deref())?;
        siblings.retain(|sibling| sibling.id != id);
        let at = (position.round() as i64).clamp(0, siblings.len() as i64) as usize;
        let mut moved = TaskRow {
            column_id: column_id.to_string(),
            swimlane_id: new_swimlane_id.clone(),
            position: at as f64,
            completed_at,
            color_id,

            // The "Added to column" date follows the task across columns, and same-column moves
            // keep the existing stamp
            column_entered_at: if column_id != old_column_id {
                Some(Utc::now().to_rfc3339())
            } else {
                task.column_entered_at.clone()
            },
            updated_at: Some(Utc::now().to_rfc3339()),
            ..task
        };

        // A due target column belongs to the task's old board
        if crosses_boards {
            moved.due_target_column_id = None;
        }
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
                push_column_event(&mut moved, "moved", detail, Actor::SYSTEM, Some(column_id));
            }
            None => {}
        }

        if column_id != old_column_id {
            mmap_remove(&txn, TASKS_BY_COLUMN, &old_column_id, id)?;
            mmap_insert(&txn, TASKS_BY_COLUMN, column_id, id)?;
        }
        {
            let mut tbl = txn.open_table(TASKS)?;
            txn_store_task(&txn, &mut tbl, &moved, OverQuota::Refuse)?;
            for (index, mut sibling) in siblings.into_iter().enumerate() {
                let dense = (if index >= at { index + 1 } else { index }) as f64;
                if sibling.position == dense {
                    continue;
                }
                sibling.position = dense;
                sibling.updated_at = Some(Utc::now().to_rfc3339());
                txn_store_task(&txn, &mut tbl, &sibling, OverQuota::Allow)?;
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

    /// Deletes a task, its attachments, its time entries, and every link to or from it, and
    /// stop the timer when it runs on this task. Returns false when unknown.
    pub fn delete_task(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(task) = txn_read_one::<TaskRow>(&txn, TASKS, id)? else {
            return Ok(false);
        };
        let entry_ids = txn_mmap_get(&txn, ENTRIES_BY_TASK, id)?;
        let owner = task_owner_in(&txn, id)?.unwrap_or_default();
        let freed = txn_task_bytes(&txn, &[id.to_string()])?;
        txn_remove_attachment_data(&txn, &[id.to_string()])?;
        unlink_deleted_tasks(&txn, &[id.to_string()])?;
        txn_count_usage(&txn, &owner, 0, freed)?;
        {
            let mut tasks = txn.open_table(TASKS)?;
            tasks.remove(id)?;
            mmap_remove(&txn, TASKS_BY_COLUMN, &task.column_id, id)?;
            let mut entries = txn.open_table(TIME_ENTRIES)?;
            for entry_id in &entry_ids {
                txn_remove_entry(&txn, &mut entries, entry_id)?;
                mmap_remove(&txn, ENTRIES_BY_TASK, id, entry_id)?;
            }
        }
        txn_clear_timer_if(&txn, &owner, |task_id| task_id == id)?;
        txn.commit()?;
        Ok(true)
    }

    /// Every task on a board with minutes filled in, ordered for rendering:
    /// column position, swimlane position (no swimlane sorts last), task
    /// position, task id.
    pub fn board_tasks(&self, board_id: &str) -> DbResult<Vec<TaskRow>> {
        let lanes = self.lane_positions(board_id)?;
        let mut out = Vec::new();
        for column in &self.list_columns(board_id)? {
            let mut tasks = Vec::new();
            for task_id in mmap_get(&self.db, TASKS_BY_COLUMN, &column.id)? {
                if let Some(task) = self.board_task(&task_id)? {
                    tasks.push(task);
                }
            }
            tasks.sort_by_cached_key(|task| {
                CellOrder::of(
                    &lanes,
                    task.id.clone(),
                    task.swimlane_id.as_deref(),
                    task.position,
                )
            });
            out.extend(tasks);
        }
        Ok(out)
    }

    /// Returns the ids of a board's tasks in the order of [`Db::board_tasks`]. Each task is read
    /// and dropped in turn, so the board is never held whole.
    pub fn board_task_ids(&self, board_id: &str) -> DbResult<Vec<String>> {
        let lanes = self.lane_positions(board_id)?;
        let columns: HashMap<String, usize> = self
            .list_columns(board_id)?
            .into_iter()
            .enumerate()
            .map(|(index, column)| (column.id, index))
            .collect();
        let mut keys = Vec::new();
        self.visit_board_tasks(board_id, |task: TaskPlace| {
            let column = columns.get(&task.column_id).copied().unwrap_or(usize::MAX);
            let order = CellOrder::of(&lanes, task.id, task.swimlane_id.as_deref(), task.position);
            keys.push((column, order));
            Ok(())
        })?;
        keys.sort();
        Ok(keys.into_iter().map(|(_, key)| key.id).collect())
    }

    /// Hands every task on a board to `visit`, read as `T`, one at a time and in no set order, so
    /// the board is never held whole and a `T` naming only some fields reads only those.
    pub fn visit_board_tasks<T: DeserializeOwned>(
        &self,
        board_id: &str,
        mut visit: impl FnMut(T) -> DbResult<()>,
    ) -> DbResult<()> {
        for column in self.list_columns(board_id)? {
            for task_id in mmap_get(&self.db, TASKS_BY_COLUMN, &column.id)? {
                if let Some(task) = read_one::<T>(&self.db, TASKS, &task_id)? {
                    visit(task)?;
                }
            }
        }
        Ok(())
    }

    /// Returns what links read of task `id`.
    pub fn task_head(&self, id: &str) -> DbResult<Option<TaskHead>> {
        self.task_as(id)
    }

    /// Returns task `id` read as `T`, which reads only the fields `T` names.
    fn task_as<T: DeserializeOwned>(&self, id: &str) -> DbResult<Option<T>> {
        read_one(&self.db, TASKS, id)
    }

    /// Returns task `id` with its minutes filled in, as [`Db::board_tasks`] lists it.
    pub fn board_task(&self, id: &str) -> DbResult<Option<TaskRow>> {
        let Some(mut task) = self.task(id)? else {
            return Ok(None);
        };
        task.total_minutes = self.task_total_minutes(id)?;
        Ok(Some(task))
    }

    /// Returns the stored position of each of a board's swimlanes by id.
    fn lane_positions(&self, board_id: &str) -> DbResult<HashMap<String, i64>> {
        Ok(self
            .list_swimlanes(board_id)?
            .into_iter()
            .map(|lane| (lane.id, lane.position))
            .collect())
    }

    // ---- Time entries ----

    /// Logs `minutes` of manual time on a task for the account that owns it; returns the entry id,
    /// empty when the task does not exist.
    pub fn create_entry(&self, task_id: &str, minutes: i64, note: &str) -> DbResult<String> {
        let owner = {
            let txn = self.db.begin_read()?;
            task_owner_in(&txn, task_id)?.map_or_else(AccountId::ownerless, AccountId::new)
        };
        Ok(self
            .create_entry_full(
                &owner,
                NewTimeEntry {
                    task_id: Some(task_id),
                    started_at: &Utc::now().to_rfc3339(),
                    minutes,
                    seconds: minutes * 60,
                    note,
                    kind: EntryKind::Manual,
                    interrupted: false,
                    interrupt_reason: None,
                    labels: &[],
                    created_by: "",
                    activity_id: None,
                },
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
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        let entry = TimeEntryRow {
            id: Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            minutes,
            seconds: minutes * 60,
            note: note.to_string(),
            started_at: Utc::now().to_rfc3339(),
            kind: EntryKind::Manual.as_str().to_string(),
            interrupted: false,
            interrupt_reason: None,
            labels: Vec::new(),
            created_by: String::new(),
            activity_id: None,
            account_id: String::new(),
        };
        let txn = self.db.begin_write()?;
        let Some(account_id) = task_owner_in(&txn, task_id)? else {
            return Ok(false);
        };
        let entry = TimeEntryRow {
            account_id,
            ..entry
        };
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
            txn_store_task(&txn, &mut tasks, &task, OverQuota::Refuse)?;
        }
        txn_store_entry(&txn, &entry, OverQuota::Refuse)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &entry.id)?;
        txn.commit()?;
        Ok(true)
    }

    /// Time entries for a task, newest first.
    pub fn list_entries(&self, task_id: &str) -> DbResult<Vec<TimeEntryRow>> {
        let mut entries = Vec::new();
        for entry_id in mmap_get(&self.db, ENTRIES_BY_TASK, task_id)? {
            if let Some(entry) = self.entry(&entry_id)? {
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

    /// Returns how many passkeys are registered.
    pub fn passkey_count(&self) -> DbResult<u64> {
        self.table_len(PASSKEYS)
    }

    /// `account`'s settings; defaults when never saved.
    pub fn settings(&self, account: &AccountId) -> DbResult<Settings> {
        let stored: Option<Settings> = read_one(&self.db, ACCOUNT_SETTINGS, account.as_str())?;
        let mut settings = stored.unwrap_or_default();

        // Backfill KanbanFlow's verbatim interruption reasons for databases whose settings predate
        // them (empty list would render an empty "Why did you stop?" menu).
        if settings.interrupt_reasons.is_empty() {
            settings.interrupt_reasons = Settings::default().interrupt_reasons;
        }
        Ok(settings)
    }

    /// Stores `account`'s settings.
    pub fn update_settings(&self, account: &AccountId, settings: &Settings) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        let stored = txn_read_one::<Settings>(&txn, ACCOUNT_SETTINGS, account.as_str())?;
        settings.check_caps(stored.as_ref())?;
        write_one(&txn, ACCOUNT_SETTINGS, account.as_str(), settings)?;
        txn.commit()?;
        Ok(())
    }

    /// Returns the backup schedule settings, or their defaults when never saved.
    pub fn backup_settings(&self) -> DbResult<BackupSettings> {
        Ok(read_one(&self.db, SETTINGS, BACKUP_SETTINGS_KEY)?.unwrap_or_default())
    }

    /// Stores the backup schedule settings.
    pub fn update_backup_settings(&self, settings: &BackupSettings) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        write_one(&txn, SETTINGS, BACKUP_SETTINGS_KEY, settings)?;
        txn.commit()?;
        Ok(())
    }

    /// Returns the outcome of the latest backup attempt, if there was one.
    pub fn backup_status(&self) -> DbResult<Option<BackupStatus>> {
        read_one(&self.db, SETTINGS, BACKUP_STATUS_KEY)
    }

    /// Stores the outcome of a backup attempt.
    pub fn record_backup_status(&self, status: &BackupStatus) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        write_one(&txn, SETTINGS, BACKUP_STATUS_KEY, status)?;
        txn.commit()?;
        Ok(())
    }

    /// Where the database file lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes a copy of the database file to `part` that holds exactly the last committed state,
    /// and flushes it to disk.
    ///
    /// Writers wait until the copy is written, and readers are not blocked. The copy carries
    /// redb's "not closed cleanly" mark, so its first open runs redb's crash repair.
    pub fn snapshot(&self, part: &Path) -> DbResult<()> {
        self.with_writes_held(|| {
            let mut source = std::fs::File::open(&*self.path)?;
            let mut copy = std::fs::File::create(part)?;
            std::io::copy(&mut source, &mut copy)?;
            copy.sync_all()
        })
    }

    /// Runs `work` while no writer can commit, and returns its result.
    ///
    /// A writer that starts meanwhile waits until `work` returns, and nothing `work` leaves
    /// uncommitted is stored.
    pub fn with_writes_held<T>(&self, work: impl FnOnce() -> std::io::Result<T>) -> DbResult<T> {
        let txn = self.db.begin_write()?;
        let result = work();
        txn.abort()?;
        Ok(result?)
    }

    /// `account`'s running timer, if one runs.
    pub fn active_timer(&self, account: &AccountId) -> DbResult<Option<ActiveTimer>> {
        read_one(&self.db, ACTIVE_TIMER, account.as_str())
    }

    /// Begins a new timer session for `account` and returns its id. Ids live in memory only and
    /// increase with every start, so two sessions that start in the same second still differ.
    pub fn begin_session(&self, account: &AccountId) -> u64 {
        let id = self.session_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.clone(), id);
        id
    }

    /// `account`'s running session id; [`None`] when this process did not start it.
    pub fn current_session(&self, account: &AccountId) -> Option<u64> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .copied()
    }

    /// Returns the lock that serializes `account`'s timer reads and writes.
    fn timer_gate(&self, account: &AccountId) -> Arc<Mutex<()>> {
        let mut gates = self.timer_gates.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(gates.entry(account.clone()).or_default())
    }

    /// Stores `account`'s running timer.
    pub fn set_active_timer(&self, account: &AccountId, timer: &ActiveTimer) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        write_one(&txn, ACTIVE_TIMER, account.as_str(), timer)?;
        txn.commit()?;
        Ok(())
    }

    /// Stops `account`'s timer without logging it.
    pub fn clear_active_timer(&self, account: &AccountId) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(ACTIVE_TIMER)?;
            tbl.remove(account.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Bump a task's completed-pomodori counter, even past the stored-bytes quota. No-op when
    /// unknown.
    pub fn record_pomodoro_complete(&self, task_id: &str) -> DbResult<()> {
        mutate_task_as(&self.db, task_id, OverQuota::Allow, |task| {
            task.pomodori_completed += 1;
        })?;
        Ok(())
    }

    /// Bump a task's interruption counter, even past the stored-bytes quota. No-op when unknown.
    pub fn record_interruption(&self, task_id: &str) -> DbResult<()> {
        mutate_task_as(&self.db, task_id, OverQuota::Allow, |task| {
            task.interruptions += 1;
        })?;
        Ok(())
    }

    /// Creates `account`'s time entry with full timer metadata, starting at the RFC3339
    /// `started_at`. A `task_id` of `None` writes a task-less entry (an empty `task_id`); returns
    /// `None` and writes nothing when the named task is gone or belongs to another account. An
    /// entry a timer session records is stored past the stored-bytes quota as far as
    /// [`OverQuota::TimerSession`] allows, so a finished session is not lost to a full account; a
    /// manual one is refused at the quota.
    pub fn create_entry_full(
        &self,
        account: &AccountId,
        entry: NewTimeEntry<'_>,
    ) -> DbResult<Option<String>> {
        let NewTimeEntry {
            task_id,
            started_at,
            minutes,
            seconds,
            note,
            kind,
            interrupted,
            interrupt_reason,
            labels,
            created_by,
            activity_id,
        } = entry;
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        let task_id = match task_id {
            Some(id) if task_owner_in(&txn, id)?.as_deref() == Some(account.as_str()) => id,
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
            kind: kind.as_str().to_string(),
            interrupted,
            interrupt_reason: interrupt_reason.map(str::to_string),
            labels: labels.to_vec(),
            created_by: created_by.to_string(),
            activity_id: activity_id.map(str::to_string),
            account_id: account.as_str().to_string(),
        };
        let over = if kind == EntryKind::Manual {
            OverQuota::Refuse
        } else {
            OverQuota::TimerSession
        };
        txn_store_entry(&txn, &row, over)?;
        if !task_id.is_empty() {
            mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        }
        txn.commit()?;
        Ok(Some(id))
    }

    /// Creates a manual time entry with an explicit start time (RFC3339), for the account that owns
    /// the task. Returns [`None`] when the task does not exist.
    pub fn create_entry_at(
        &self,
        task_id: &str,
        minutes: i64,
        note: &str,
        started_at: &str,
        labels: &[String],
        created_by: &str,
    ) -> DbResult<Option<String>> {
        let txn = self.db.begin_write()?;
        let Some(account_id) = task_owner_in(&txn, task_id)? else {
            return Ok(None);
        };
        let id = Uuid::new_v4().to_string();
        let row = TimeEntryRow {
            id: id.clone(),
            task_id: task_id.to_string(),
            minutes,
            seconds: minutes * 60,
            note: note.to_string(),
            started_at: started_at.to_string(),
            kind: EntryKind::Manual.as_str().to_string(),
            interrupted: false,
            interrupt_reason: None,
            labels: labels.to_vec(),
            created_by: created_by.to_string(),
            activity_id: None,
            account_id,
        };
        txn_store_entry(&txn, &row, OverQuota::Refuse)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        txn.commit()?;
        Ok(Some(id))
    }

    /// Update a time entry (date/time, task reassignment, note, labels);
    /// `seconds` is the new length. An empty `task_id` makes it task-less.
    /// Returns false when the entry does not exist; the task must exist
    /// (checked by the caller).
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
        let tbl = txn.open_table(TIME_ENTRIES)?;
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

        drop(tbl);
        txn_store_entry(&txn, &row, OverQuota::Refuse)?;

        // Task-less entries (empty `task_id`) are not in the index
        if old_task != task_id {
            if !old_task.is_empty() {
                mmap_remove(&txn, ENTRIES_BY_TASK, &old_task, entry_id)?;
            }
            if !task_id.is_empty() {
                mmap_insert(&txn, ENTRIES_BY_TASK, task_id, entry_id)?;
            }
        }
        txn.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod template_repeat_tests {
    //! A template task keeps a repeat only together with a due date. The test
    //! applies a template whose task carries a repeat but no due date, and
    //! reads the created task back to see the repeat dropped. A template task
    //! with both a repeat and a due date is not covered here.

    use super::*;

    #[test]
    fn template_snapshot_should_stay_valid_json_when_its_first_task_is_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let board = db
            .create_board(&AccountId::ownerless(), "Board")
            .expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let kept = db
            .create_task(&column, None, "Kept", 1, None)
            .expect("task");
        let snapshot = db
            .write_template_snapshot(&board, &["gone".to_string(), kept])
            .expect("snapshot");
        let snapshot: serde_json::Value = serde_json::from_slice(&snapshot).expect("valid JSON");
        let tasks = snapshot["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["name"], "Kept");
    }

    #[test]
    fn apply_template_should_drop_a_repeat_without_a_due_date() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let snapshot = serde_json::json!({
            "columns": [{"name": "To-do"}],
            "tasks": [
                {"name": "orphan", "column": "To-do", "due_repeat": "every week"},
                {"name": "dated", "column": "To-do", "due_at": "2099-01-01 17:00",
                 "due_repeat": "every week"},
            ],
        });
        let mut row = BoardTemplateRow {
            id: "old".to_string(),
            name: "Old".to_string(),
            description: String::new(),
            built_in: false,
            owner_id: Some(String::new()),
            snapshot_bytes: 0,
        };
        let snapshot = serde_json::to_vec(&snapshot).expect("json");
        let txn = db.db.begin_write().expect("write");
        txn_store_template(&txn, &mut row, &snapshot).expect("template");
        txn.commit().expect("commit");

        let board = db
            .apply_template(&AccountId::ownerless(), "old", "Copy")
            .expect("apply")
            .expect("board");
        let tasks = db.board_tasks(&board).expect("tasks");
        let repeat_of = |name: &str| {
            let task = tasks.iter().find(|task| task.name == name).expect("task");
            (task.due_at.is_some(), task.due_repeat.clone())
        };
        assert_eq!(repeat_of("orphan"), (false, None));
        assert_eq!(repeat_of("dated"), (true, Some("every week".to_string())));
    }
}

#[cfg(test)]
mod main_swimlane_tests {
    //! The main swimlane is the board's stored lane, not a name. The tests
    //! rename the first lane, add a lane named default, delete the main lane,
    //! and open a legacy board, then check which lane is main and that the
    //! fallback to the first lane writes nothing.

    use super::*;

    fn test_db() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let board_id = db
            .create_board(&AccountId::ownerless(), "Lanes")
            .expect("board");
        db.ensure_default_swimlane(&board_id).expect("lane");
        (dir, db, board_id)
    }

    fn main_of(db: &Db, board_id: &str) -> Option<String> {
        let board = db.board(board_id).expect("read").expect("board");
        let lanes = db.list_swimlanes(board_id).expect("lanes");
        board.main_swimlane(&lanes)
    }

    #[test]
    fn main_swimlane_id_should_follow_the_first_lane_through_a_rename() {
        let (_dir, db, board_id) = test_db();
        let first = db.list_swimlanes(&board_id).expect("lanes")[0].id.clone();
        db.update_swimlane(
            &first,
            &SwimlanePatch {
                name: Some("Work"),
                ..SwimlanePatch::default()
            },
        )
        .expect("rename");
        assert_eq!(main_of(&db, &board_id), Some(first));
    }

    #[test]
    fn main_swimlane_id_should_ignore_a_later_lane_named_default() {
        let (_dir, db, board_id) = test_db();
        let first = db.list_swimlanes(&board_id).expect("lanes")[0].id.clone();
        db.update_swimlane(
            &first,
            &SwimlanePatch {
                name: Some("Work"),
                ..SwimlanePatch::default()
            },
        )
        .expect("rename");
        let later = db.create_swimlane(&board_id, "Default").expect("lane");
        assert_ne!(main_of(&db, &board_id), Some(later));
        assert_eq!(main_of(&db, &board_id), Some(first));
    }

    #[test]
    fn main_swimlane_id_should_pass_to_the_next_lane_when_the_main_lane_is_deleted() {
        let (_dir, db, board_id) = test_db();
        let first = db.list_swimlanes(&board_id).expect("lanes")[0].id.clone();
        let second = db.create_swimlane(&board_id, "Other").expect("lane");
        let third = db.create_swimlane(&board_id, "Third").expect("lane");
        db.delete_swimlane(&first).expect("delete");
        assert_eq!(main_of(&db, &board_id), Some(second.clone()));
        db.delete_swimlane(&third).expect("delete a non-main lane");
        assert_eq!(main_of(&db, &board_id), Some(second));
    }

    fn clear_main(db: &Db, board_id: &str) {
        let mut cleared = serde_json::Map::new();
        cleared.insert(
            crate::models::MAIN_SWIMLANE_KEY.to_string(),
            serde_json::Value::Null,
        );
        db.set_board_config(board_id, &cleared).expect("clear");
    }

    #[test]
    fn main_swimlane_should_fall_back_to_the_first_lane_without_writing() {
        let (_dir, db, board_id) = test_db();
        let first = db.list_swimlanes(&board_id).expect("lanes")[0].id.clone();
        clear_main(&db, &board_id);
        db.create_swimlane(&board_id, "Second").expect("lane");
        let before = db.board(&board_id).expect("read").expect("board");
        assert_eq!(main_of(&db, &board_id), Some(first));
        let after = db.board(&board_id).expect("read").expect("board");
        assert_eq!(before.config_json, after.config_json);
        assert_eq!(after.main_swimlane_id(), None);
    }

    #[test]
    fn open_should_assign_the_first_lane_as_main_whatever_the_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let (board_id, first) = {
            let db = Db::connect(path).expect("connect");
            let board_id = db
                .create_board(&AccountId::ownerless(), "Legacy")
                .expect("board");
            db.ensure_default_swimlane(&board_id).expect("lane");
            let first = db.list_swimlanes(&board_id).expect("lanes")[0].id.clone();
            db.update_swimlane(
                &first,
                &SwimlanePatch {
                    name: Some("Work"),
                    ..SwimlanePatch::default()
                },
            )
            .expect("rename");
            db.create_swimlane(&board_id, "Default").expect("lane");
            clear_main(&db, &board_id);
            (board_id, first)
        };
        let db = Db::connect(path).expect("reconnect");
        let board = db.board(&board_id).expect("read").expect("board");
        assert_eq!(board.main_swimlane_id(), Some(first));
        let stored = board.config_json.clone();
        drop(db);
        let db = Db::connect(path).expect("reconnect again");
        let board = db.board(&board_id).expect("read").expect("board");
        assert_eq!(board.config_json, stored);
    }
}

#[cfg(test)]
mod swimlane_tests {
    use super::*;

    struct Board {
        _dir: tempfile::TempDir,
        db: Db,
        id: String,
        columns: Vec<String>,
        lanes: Vec<String>,
    }

    fn board_with_lanes(extra: &[&str]) -> Board {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let id = db
            .create_board(&AccountId::ownerless(), "Lanes")
            .expect("board");
        db.ensure_default_swimlane(&id).expect("lane");
        let mut lanes = vec![db.list_swimlanes(&id).expect("lanes")[0].id.clone()];
        for name in extra {
            lanes.push(db.create_swimlane(&id, name).expect("lane"));
        }
        let columns = ["Todo", "Done"]
            .iter()
            .map(|name| db.create_column(&id, name, None).expect("column"))
            .collect();
        Board {
            _dir: dir,
            db,
            id,
            columns,
            lanes,
        }
    }

    fn add_task(board: &Board, column: usize, lane: usize, name: &str) -> String {
        board
            .db
            .create_task(
                &board.columns[column],
                Some(&board.lanes[lane]),
                name,
                0,
                None,
            )
            .expect("task")
    }

    fn place(board: &Board, task: &str) -> (String, Option<String>, f64) {
        let row = board.db.task(task).expect("read").expect("task");
        (row.column_id, row.swimlane_id, row.position)
    }

    #[test]
    fn should_move_tasks_into_the_lane_above_and_keep_their_columns() {
        let board = board_with_lanes(&["Two", "Three"]);
        let kept = add_task(&board, 0, 1, "kept");
        let todo = add_task(&board, 0, 2, "todo");
        let done = add_task(&board, 1, 2, "done");
        let outcome = board.db.delete_swimlane(&board.lanes[2]).expect("delete");
        assert_eq!(outcome, SwimlaneDeleteOutcome::Deleted);
        assert_eq!(
            place(&board, &todo),
            (board.columns[0].clone(), Some(board.lanes[1].clone()), 1.0)
        );
        assert_eq!(
            place(&board, &done),
            (board.columns[1].clone(), Some(board.lanes[1].clone()), 0.0)
        );
        assert_eq!(place(&board, &kept).2, 0.0);
    }

    #[test]
    fn should_move_tasks_of_the_first_lane_into_the_next_lane_and_pass_the_main_role() {
        let board = board_with_lanes(&["Two"]);
        let task = add_task(&board, 0, 0, "task");
        let outcome = board.db.delete_swimlane(&board.lanes[0]).expect("delete");
        assert_eq!(outcome, SwimlaneDeleteOutcome::Deleted);
        assert_eq!(place(&board, &task).1, Some(board.lanes[1].clone()));
        let row = board.db.board(&board.id).expect("read").expect("board");
        let lanes = board.db.list_swimlanes(&board.id).expect("lanes");
        assert_eq!(row.main_swimlane(&lanes), Some(board.lanes[1].clone()));
    }

    #[test]
    fn should_refuse_the_last_lane_and_keep_its_tasks() {
        let board = board_with_lanes(&["Two"]);
        let task = add_task(&board, 0, 0, "task");
        board.db.delete_swimlane(&board.lanes[1]).expect("delete");
        let outcome = board.db.delete_swimlane(&board.lanes[0]).expect("delete");
        assert_eq!(outcome, SwimlaneDeleteOutcome::RefusedLast);
        assert_eq!(place(&board, &task).1, Some(board.lanes[0].clone()));
    }

    fn event_kinds(board: &Board, task: &str) -> Vec<String> {
        let row = board.db.task(task).expect("read").expect("task");
        row.history.into_iter().map(|event| event.kind).collect()
    }

    #[test]
    fn merge_should_move_every_task_into_the_main_lane_and_keep_columns() {
        let board = board_with_lanes(&["Two", "Three"]);
        let own = add_task(&board, 0, 0, "own");
        let two = add_task(&board, 0, 1, "two");
        let three = add_task(&board, 0, 2, "three");
        let done = add_task(&board, 1, 2, "done");
        let outcome = board
            .db
            .merge_swimlanes(&board.id, Actor::SYSTEM)
            .expect("merge");
        assert_eq!(
            outcome,
            SwimlaneMergeOutcome::Merged {
                swimlanes: 2,
                tasks: 3
            }
        );
        let lanes = board.db.list_swimlanes(&board.id).expect("lanes");
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].id, board.lanes[0]);
        let mut positions = Vec::new();
        for (task, column) in [(&own, 0), (&two, 0), (&three, 0), (&done, 1)] {
            let (found_column, lane, position) = place(&board, task);
            assert_eq!(found_column, board.columns[column]);
            assert_eq!(lane, Some(board.lanes[0].clone()));
            if column == 0 {
                positions.push(position);
            }
        }
        positions.sort_by(f64::total_cmp);
        assert_eq!(positions, vec![0.0, 1.0, 2.0], "the cell stays dense");
        assert_eq!(
            place(&board, &own).2,
            0.0,
            "the main lane's own task stays first"
        );
    }

    #[test]
    fn merge_should_write_a_history_event_on_each_moved_task_only() {
        let board = board_with_lanes(&["Two"]);
        let own = add_task(&board, 0, 0, "own");
        let moved = add_task(&board, 0, 1, "moved");
        board
            .db
            .merge_swimlanes(&board.id, Actor::SYSTEM)
            .expect("merge");
        assert!(!event_kinds(&board, &own).contains(&"swimlane_merged".to_string()));
        assert!(event_kinds(&board, &moved).contains(&"swimlane_merged".to_string()));
    }

    #[test]
    fn merge_should_refuse_a_board_with_one_lane_and_an_unknown_board() {
        let board = board_with_lanes(&[]);
        let task = add_task(&board, 0, 0, "task");
        assert_eq!(
            board
                .db
                .merge_swimlanes(&board.id, Actor::SYSTEM)
                .expect("merge"),
            SwimlaneMergeOutcome::RefusedSingle
        );
        assert_eq!(
            board
                .db
                .merge_swimlanes("no-such-board", Actor::SYSTEM)
                .expect("merge"),
            SwimlaneMergeOutcome::NotFound
        );
        assert_eq!(place(&board, &task).1, Some(board.lanes[0].clone()));
    }

    #[test]
    fn merge_should_keep_the_main_lane_that_is_not_first() {
        let board = board_with_lanes(&["Two"]);
        let mut updates = serde_json::Map::new();
        updates.insert(
            crate::models::MAIN_SWIMLANE_KEY.to_string(),
            serde_json::Value::String(board.lanes[1].clone()),
        );
        board
            .db
            .set_board_config(&board.id, &updates)
            .expect("set main");
        let task = add_task(&board, 0, 0, "task");
        board
            .db
            .merge_swimlanes(&board.id, Actor::SYSTEM)
            .expect("merge");
        assert_eq!(place(&board, &task).1, Some(board.lanes[1].clone()));
    }

    #[test]
    fn merge_should_fall_back_to_the_first_lane_when_the_main_id_dangles() {
        let board = board_with_lanes(&["Two"]);
        let mut updates = serde_json::Map::new();
        updates.insert(
            crate::models::MAIN_SWIMLANE_KEY.to_string(),
            serde_json::Value::String("gone".to_string()),
        );
        board
            .db
            .set_board_config(&board.id, &updates)
            .expect("set main");
        let first = add_task(&board, 0, 0, "first");
        let second = add_task(&board, 1, 1, "second");
        let outcome = board
            .db
            .merge_swimlanes(&board.id, Actor::SYSTEM)
            .expect("merge");
        assert_eq!(
            outcome,
            SwimlaneMergeOutcome::Merged {
                swimlanes: 1,
                tasks: 1
            }
        );
        assert_eq!(place(&board, &first).1, Some(board.lanes[0].clone()));
        assert_eq!(place(&board, &second).1, Some(board.lanes[0].clone()));
        assert_eq!(board.db.list_swimlanes(&board.id).expect("lanes").len(), 1);
    }

    #[test]
    fn update_should_store_limits_and_refuse_a_column_of_another_board() {
        let board = board_with_lanes(&["Two"]);
        let other = board
            .db
            .create_board(&AccountId::ownerless(), "Other")
            .expect("board");
        let foreign = board
            .db
            .create_column(&other, "Foreign", None)
            .expect("column");
        let own = [LaneLimit {
            column_id: board.columns[0].clone(),
            max: 3,
        }];
        let patch = SwimlanePatch {
            description: Some("Front end"),
            limits: Some(&own),
            ..SwimlanePatch::default()
        };
        assert_eq!(
            board
                .db
                .update_swimlane(&board.lanes[1], &patch)
                .expect("update"),
            SwimlaneUpdateOutcome::Updated
        );
        let lane = board
            .db
            .swimlane(&board.lanes[1])
            .expect("read")
            .expect("lane");
        assert_eq!(lane.description, "Front end");
        assert_eq!(lane.limit_for(&board.columns[0]), Some(3));

        let bad = [LaneLimit {
            column_id: foreign,
            max: 3,
        }];
        let patch = SwimlanePatch {
            description: Some("changed"),
            limits: Some(&bad),
            ..SwimlanePatch::default()
        };
        assert_eq!(
            board
                .db
                .update_swimlane(&board.lanes[1], &patch)
                .expect("update"),
            SwimlaneUpdateOutcome::UnknownColumn
        );
        let lane = board
            .db
            .swimlane(&board.lanes[1])
            .expect("read")
            .expect("lane");
        assert_eq!(
            lane.description, "Front end",
            "a refused patch changes nothing"
        );
        assert_eq!(
            board
                .db
                .update_swimlane("no-such-lane", &SwimlanePatch::default())
                .expect("update"),
            SwimlaneUpdateOutcome::NotFound
        );
    }

    #[test]
    fn deleting_a_column_should_drop_the_lane_limits_on_it() {
        let board = board_with_lanes(&["Two"]);
        let limits = [
            LaneLimit {
                column_id: board.columns[0].clone(),
                max: 2,
            },
            LaneLimit {
                column_id: board.columns[1].clone(),
                max: 4,
            },
        ];
        let patch = SwimlanePatch {
            limits: Some(&limits),
            ..SwimlanePatch::default()
        };
        board
            .db
            .update_swimlane(&board.lanes[0], &patch)
            .expect("update");
        board.db.delete_column(&board.columns[0]).expect("delete");
        let lane = board
            .db
            .swimlane(&board.lanes[0])
            .expect("read")
            .expect("lane");
        assert_eq!(lane.limits, vec![limits[1].clone()]);
    }

    #[test]
    fn create_at_should_insert_at_the_position_and_clamp_past_the_end() {
        let board = board_with_lanes(&["Two"]);
        let top = board
            .db
            .create_swimlane_at(&board.id, "Top", Some(0))
            .expect("lane");
        let far = board
            .db
            .create_swimlane_at(&board.id, "Far", Some(99))
            .expect("lane");
        let order: Vec<String> = board
            .db
            .list_swimlanes(&board.id)
            .expect("lanes")
            .into_iter()
            .map(|lane| lane.id)
            .collect();
        assert_eq!(
            order,
            vec![top, board.lanes[0].clone(), board.lanes[1].clone(), far]
        );
    }

    #[test]
    fn upgrade_should_read_lanes_stored_before_limits_with_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("v23.redb");
        let path = path.to_str().expect("utf8");
        let (board_id, lane_id) = {
            let db = Db::connect(path).expect("connect");
            let board_id = db
                .create_board(&AccountId::ownerless(), "Legacy")
                .expect("board");
            let lane_id = "legacy-lane".to_string();
            let legacy = serde_json::json!({
                "id": lane_id,
                "board_id": board_id,
                "name": "Old lane",
                "position": 0,
            });
            let txn = db.db.begin_write().expect("write");
            txn.open_table(SWIMLANES)
                .expect("lanes")
                .insert(lane_id.as_str(), legacy.to_string().as_bytes())
                .expect("legacy row");
            mmap_insert(&txn, SWIMLANES_BY_BOARD, &board_id, &lane_id).expect("index");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 23)
                .expect("stamp");
            txn.commit().expect("commit");
            (board_id, lane_id)
        };
        let db = Db::connect(path).expect("upgrade from version 23");
        let lanes = db.list_swimlanes(&board_id).expect("lanes");
        let lane = lanes.iter().find(|lane| lane.id == lane_id).expect("kept");
        assert_eq!(lane.name, "Old lane");
        assert!(lane.description.is_empty());
        assert!(lane.limits.is_empty());
        assert!(!lane.collapsed);
        assert_eq!(lane.height, None);
        let patch = SwimlanePatch {
            collapsed: Some(true),
            ..SwimlanePatch::default()
        };
        db.update_swimlane(&lane_id, &patch).expect("update");
        assert!(
            db.swimlane(&lane_id)
                .expect("read")
                .expect("lane")
                .collapsed
        );
    }
}

#[cfg(test)]
mod color_hex_tests {
    //! Stored standard colors read back with the shipped palette. A color
    //! stored with a stale hex value is replaced by the palette hex when it is
    //! read, so a palette change reaches existing boards. The test stores a
    //! stale hex for a standard color and reads it back.

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

        let read = db.color(&blue.id).expect("read").expect("row");
        assert_eq!(read.border_hex, "#70b0ff");
    }
}

#[cfg(test)]
mod legend_migration_tests {
    //! The one-time `legend_visible` backfill must grant legacy boards the
    //! legend (missing key -> true), run exactly once, and never overwrite an
    //! explicit user choice. New boards keep the key absent so the footer stays
    //! off (KanbanFlow default).

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
        db.board(board_id)
            .expect("get board")
            .expect("board exists")
            .config_bool("legend_visible")
    }

    #[test]
    fn backfill_should_grant_legacy_boards_true_exactly_once() {
        let (_dir, db) = test_db();

        // Fresh install: seeded starter board has no key -> legend OFF
        let general = db.list_boards().expect("list boards")[0].id.clone();
        assert!(!legend_of(&db, &general));

        let legacy = db
            .create_board(&AccountId::ownerless(), "Legacy")
            .expect("create board");
        assert!(!legend_of(&db, &legacy));

        // Simulate the upgrade boot: marker absent, legacy board keyless
        clear_backfill_marker(&db);
        db.migrate_legend_default().expect("backfill");
        assert!(legend_of(&db, &legacy), "legacy board keeps the legend");

        // Re-running must not touch anything (marker short-circuits)
        set_legend(&db, &legacy, false);
        db.migrate_legend_default().expect("second backfill");
        assert!(
            !legend_of(&db, &legacy),
            "explicit false survives re-run when marker is set"
        );

        // Explicit false also survives a fresh backfill pass
        clear_backfill_marker(&db);
        db.migrate_legend_default().expect("backfill again");
        assert!(
            !legend_of(&db, &legacy),
            "explicit user choice is never overwritten"
        );

        // Boards created after the toggle default OFF
        let fresh = db
            .create_board(&AccountId::ownerless(), "Fresh")
            .expect("create board");
        assert!(!legend_of(&db, &fresh));
    }
}

#[cfg(test)]
mod feed_token_tests {
    //! The calendar-feed bearer token round-trips through the board's opaque
    //! config bag, and lookup by token resolves the board. The test stores a
    //! token on a board, then resolves the board from it, so the token never
    //! needs a column of its own.

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
    fn feed_token_should_round_trip_and_resolve() {
        let (_dir, db) = test_db();
        let board_id = db
            .create_board(&AccountId::ownerless(), "Work")
            .expect("create board");

        // No token until one is stored
        assert!(
            db.board(&board_id)
                .expect("get board")
                .expect("board exists")
                .feed_token()
                .is_none()
        );
        assert!(db.board_by_feed_token("nope").expect("lookup").is_none());

        set_feed_token(&db, &board_id, "tok123");
        let board = db
            .board(&board_id)
            .expect("get board")
            .expect("board exists");
        assert_eq!(board.feed_token().as_deref(), Some("tok123"));

        let found = db
            .board_by_feed_token("tok123")
            .expect("lookup")
            .expect("board resolves by token");
        assert_eq!(found.id, board_id);

        // Rotating the token invalidates the old one
        set_feed_token(&db, &board_id, "tok456");
        assert!(db.board_by_feed_token("tok123").expect("lookup").is_none());
        let found = db
            .board_by_feed_token("tok456")
            .expect("lookup")
            .expect("new token resolves");
        assert_eq!(found.id, board_id);

        let other = db
            .create_board(&AccountId::ownerless(), "Other")
            .expect("create board");
        set_feed_token(&db, &other, "tok789");
        db.delete_board(&other).expect("delete board");
        assert!(db.board_by_feed_token("tok789").expect("lookup").is_none());
        assert!(db.board_by_feed_token("tok456").expect("lookup").is_some());
    }

    #[test]
    fn feed_tokens_should_be_indexed_when_a_version_15_file_is_upgraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("upgrade.redb");
        let path = path.to_str().expect("utf8");
        let board_id = {
            let db = Db::connect(path).expect("db");
            let board_id = db
                .create_board(&AccountId::ownerless(), "Work")
                .expect("create board");
            set_feed_token(&db, &board_id, "tok123");
            let txn = db.db.begin_write().expect("write");
            txn.delete_table(FEED_TOKENS).expect("drop index");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 15)
                .expect("stamp");
            txn.commit().expect("commit");
            board_id
        };
        let db = Db::connect(path).expect("upgrade from version 15");
        let found = db
            .board_by_feed_token("tok123")
            .expect("lookup")
            .expect("the token resolves after the upgrade");
        assert_eq!(found.id, board_id);
    }
}

#[cfg(test)]
mod pomodori_restore_tests {
    //! The one-time backfill must restore KanbanFlow's "Pomodori" in labels an earlier release
    //! rewrote to "Pomodoros", run exactly once, and never touch user-customized labels. The tests
    //! clear the restore marker, rename labels, and reconnect to check the rename happens once and
    //! the starter board is left alone.

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
        db.color(color_id)
            .expect("get color")
            .expect("color exists")
            .label
    }

    #[test]
    fn restore_should_rename_pomodoros_labels_exactly_once() {
        let (_dir, db) = test_db();
        let board = db
            .create_board(&AccountId::ownerless(), "Legacy")
            .expect("create board");
        let colors = db.list_colors(&board).expect("list colors");

        // A board the earlier rename touched, plus a label of the user's own
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

        // Re-running must be a no-op (marker short-circuits the pass)
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
    fn starter_board_should_keep_pomodori_across_restarts() {
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
    //! A database that stored an older built-in template snapshot picks up the
    //! shipped one on the next start. The test plants a stale snapshot,
    //! reconnects, and reads the template back to confirm the shipped version
    //! replaced it.

    use super::*;

    #[test]
    fn stale_builtin_snapshot_should_be_rewritten_on_connect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let template_id = {
            let db = Db::connect(path).expect("connect");
            let mut row = db
                .list_templates(&AccountId::ownerless())
                .expect("templates")
                .into_iter()
                .find(|t| t.built_in && t.name == Db::BUILTIN_TEMPLATE_NAME)
                .expect("built-in template");
            let stale = serde_json::json!({"colors": [], "columns": [], "swimlanes": []});
            let stale = serde_json::to_vec(&stale).expect("json");
            let txn = db.db.begin_write().expect("write txn");
            txn_store_template(&txn, &mut row, &stale).expect("write stale row");
            txn.commit().expect("commit");
            row.id
        };
        let db = Db::connect(path).expect("reconnect");
        let row = db
            .list_templates(&AccountId::ownerless())
            .expect("templates")
            .into_iter()
            .find(|t| t.id == template_id)
            .expect("same template id kept");
        let snapshot = txn_template_snapshot(&db.read().expect("read"), &row.id)
            .expect("snapshot")
            .expect("stored");
        assert_eq!(snapshot, pomodoro_template_snapshot());
    }

    #[test]
    fn stale_builtin_snapshot_should_refresh_its_description_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let (pomodoro_id, other_id) = {
            let db = Db::connect(path).expect("connect");
            let rows = db
                .list_templates(&AccountId::ownerless())
                .expect("templates");
            let mut pomodoro = rows
                .iter()
                .find(|t| t.built_in && t.name == Db::BUILTIN_TEMPLATE_NAME)
                .expect("pomodoro built-in")
                .clone();
            let mut other = rows
                .iter()
                .find(|t| t.built_in && t.name == Db::KANBAN_BASICS_TEMPLATE_NAME)
                .expect("other built-in")
                .clone();
            let other_snapshot =
                serde_json::to_vec(&kanban_basics_template_snapshot()).expect("json");
            let stale = serde_json::to_vec(&serde_json::json!({"colors": []})).expect("json");
            pomodoro.description = "column and swimlane layout".to_string();
            other.description = "kept as stored".to_string();
            let txn = db.db.begin_write().expect("write txn");
            txn_store_template(&txn, &mut pomodoro, &stale).expect("write stale row");
            txn_store_template(&txn, &mut other, &other_snapshot).expect("write other row");
            txn.commit().expect("commit");
            (pomodoro.id, other.id)
        };
        let db = Db::connect(path).expect("reconnect");
        let rows = db
            .list_templates(&AccountId::ownerless())
            .expect("templates");
        let pomodoro = rows.iter().find(|t| t.id == pomodoro_id).expect("pomodoro");
        assert!(pomodoro.description.starts_with("Pomodoro board:"));
        let other = rows.iter().find(|t| t.id == other_id).expect("other");
        assert_eq!(other.description, "kept as stored");
    }
}

#[cfg(test)]
mod schema_version_tests {
    //! The `meta` stamp: fresh and older files are stamped with
    //! [`SCHEMA_VERSION`], a newer file is refused without application
    //! writes (byte-identical when it was closed cleanly).
    //! The tests cover version 1 and 2 files opening with their rows intact,
    //! newer files refused untouched, and crash-left copies.

    use redb::TableHandle;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::auth::new_api_token_record;
    use crate::models::{FieldKind, FieldOwner};

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

    /// A session id from before a restart never matches a session started after it.
    #[test]
    fn session_ids_should_differ_across_restarts() {
        let (_dir, path) = temp_path();
        let before = {
            let db = Db::connect(&path).expect("connect");
            db.begin_session(&AccountId::ownerless())
        };
        let db = Db::connect(&path).expect("reconnect");
        let after = db.begin_session(&AccountId::ownerless());
        assert!(after > before, "{after} should follow {before}");
        assert!(after < 1 << 53, "ids stay JavaScript-safe");
    }

    /// A repeat without a due date, left by an older binary, is cleared at
    /// startup and never reaches a template snapshot.
    #[test]
    fn orphan_repeats_should_be_cleared_at_startup_and_in_templates() {
        let (_dir, path) = temp_path();
        let (task_id, template_id) = {
            let db = Db::connect(&path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Old")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let task = db.create_task(&column, None, "T", 1, None).expect("task");
            mutate(&db.db, TASKS, &task, |row: &mut TaskRow| {
                row.due_repeat = Some("every week".to_string());
            })
            .expect("write legacy row");
            let template = db
                .save_board_as_template(&AccountId::ownerless(), &board, "Legacy", "")
                .expect("save")
                .expect("board exists");
            (task, template)
        };
        let db = Db::connect(&path).expect("reopen");
        let task = db.task(&task_id).expect("get").expect("task");
        assert_eq!(task.due_repeat, None);
        assert!(task.due_at.is_none());
        let snapshot = txn_template_snapshot(&db.read().expect("read"), &template_id)
            .expect("get")
            .expect("snapshot");
        assert_eq!(snapshot["tasks"][0]["due_repeat"], serde_json::Value::Null);
    }

    /// Writes `row` into `table` as a version-4 file held it, with a `scopes` list.
    fn put_raw(
        path: &str,
        table: TableDefinition<'_, &str, &[u8]>,
        key: &str,
        row: &serde_json::Value,
    ) {
        let db = Database::create(path).expect("raw open");
        let txn = db.begin_write().expect("write txn");
        {
            let mut tbl = txn.open_table(table).expect("table");
            let raw = serde_json::to_vec(row).expect("json");
            tbl.insert(key, raw.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");
    }

    /// A version-4 file's `scopes` lists become one `scope`: `write` turns into `admin` and
    /// `read` stays, for tokens and for pending device requests.
    #[test]
    fn version_4_file_should_rewrite_scope_lists_to_one_scope() {
        let (_dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        let mut tokens = Vec::new();
        for (name, scopes) in [
            ("reader", vec!["read"]),
            ("writer", vec!["write"]),
            ("both", vec!["read", "write"]),
        ] {
            let (record, _) =
                new_api_token_record(&AccountId::ownerless(), name, AuthScope::Read, None);
            let mut row = serde_json::to_value(&record).expect("json");
            let object = row.as_object_mut().expect("object");
            object.remove("scope");
            object.insert("scopes".to_string(), serde_json::json!(scopes));
            put_raw(&path, API_TOKENS, &record.id, &row);
            tokens.push(record.id);
        }
        let request = serde_json::json!({
            "device_code_hash": "hash",
            "user_code": "BCDFGHJK",
            "client_name": "waiting",
            "scopes": ["read", "write"],
            "status": "pending",
            "created_at": 0,
            "expires_at": i64::MAX,
            "last_poll_at": null,
        });
        put_raw(&path, DEVICE_AUTHS, "hash", &request);
        stamp(&path, 4);
        {
            let db = Db::connect(&path).expect("open version 4");
            let mut held: Vec<_> = db
                .list_api_tokens()
                .expect("list")
                .into_iter()
                .map(|record| (record.name, record.scope))
                .collect();
            held.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                held,
                [
                    ("both".to_string(), AuthScope::Admin),
                    ("reader".to_string(), AuthScope::Read),
                    ("writer".to_string(), AuthScope::Admin),
                ]
            );
            let waiting = db.pending_device_auth("BCDFGHJK", 1).expect("pending");
            assert_eq!(waiting.expect("request").scope, AuthScope::Admin);
        }
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    #[test]
    fn fresh_db_should_be_stamped_with_current_version() {
        let (_dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    #[test]
    fn unstamped_db_should_open_and_be_stamped() {
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
    fn version_5_file_should_upgrade_to_current_keeping_its_tokens() {
        let (_dir, path) = temp_path();
        let (record, _) =
            new_api_token_record(&AccountId::ownerless(), "kept", AuthScope::Work, None);
        {
            let db = Db::connect(&path).expect("connect");
            db.create_api_token(&record).expect("token");
        }
        stamp(&path, 5);
        let db = Db::connect(&path).expect("open version 5");
        let held = db.list_api_tokens().expect("list");
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].scope, AuthScope::Work);
        drop(db);
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    #[test]
    fn older_stamp_should_be_raised_to_current_version() {
        let (_dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        stamp(&path, 0);
        drop(Db::connect(&path).expect("open older file"));
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    /// A file written at version 1 opens, is stamped current, and keeps its
    /// rows, which read with the version-2 fields at their defaults.
    #[test]
    fn version_1_file_should_open_stamped_current_with_its_rows() {
        let (_dir, path) = temp_path();
        let (task_id, comment_id) = {
            let db = Db::connect(&path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Old")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let task = db
                .create_task(&column, None, "Kept", 1, None)
                .expect("task");
            let comment = NewComment {
                author: "audit",
                body: "hi",
                actor_kind: ActorKind::User,
                token_name: None,
                token_id: None,
                kind: None,
            };
            let FencedWrite::Written(comment) = db
                .add_comment(
                    &task,
                    comment,
                    Actor {
                        name: "audit",
                        token_name: None,
                    },
                    None,
                )
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
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        let db = Db::connect(&path).expect("reopen");
        let task = db.task(&task_id).expect("read").expect("task kept");
        assert_eq!(task.name, "Kept");
        assert_eq!(task.updated_at, None);
        assert_eq!(task.metadata, None);
        assert!(task.links.is_empty() && task.fields.is_empty() && task.claim.is_none());
        assert_eq!(task.comments[0].id, comment_id);
        assert_eq!(task.comments[0].actor_kind, ActorKind::User);
        assert!(
            db.list_fields("any")
                .expect("fields table exists")
                .is_empty()
        );
    }

    /// A file written at version 3 holds `ding_enabled` in its settings row. It opens, is stamped
    /// 4, and keeps every other setting.
    #[test]
    fn version_3_file_should_open_stamped_4_with_settings_intact() {
        let (_dir, path) = temp_path();
        let changed = Settings {
            pomodoro_minutes: 40,
            short_break_minutes: 7,
            long_break_minutes: 20,
            long_break_every: 3,
            notifications_enabled: false,
            interrupt_reasons: vec!["Dog".to_string(), "Meeting".to_string()],
            ticking_mode: "always".to_string(),
            alarm_sound: "chime".to_string(),
            alarm_volume: 33,
            points_volume: 44,
            sounds_enabled: false,
            pip_enabled: false,
            favorite_boards: vec!["board-1".to_string()],
            ..Settings::default()
        };
        drop(Db::connect(&path).expect("connect"));
        {
            let db = Database::open(&path).expect("open");
            let txn = db.begin_write().expect("write");
            {
                let mut settings = txn.open_table(SETTINGS).expect("settings");
                let mut row = serde_json::to_value(&changed).expect("json");
                assert!(
                    row.get("ding_enabled").is_none(),
                    "a version-4 row carries no ding_enabled"
                );
                row["ding_enabled"] = serde_json::Value::Bool(true);
                let raw = serde_json::to_vec(&row).expect("bytes");
                settings.insert("app", raw.as_slice()).expect("insert");
            }
            txn.commit().expect("commit");
        }
        stamp(&path, 3);
        drop(Db::connect(&path).expect("open version-3 file"));
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        let db = Db::connect(&path).expect("reopen");
        let loaded = db.settings(&AccountId::ownerless()).expect("settings");
        assert_eq!(
            serde_json::to_value(&loaded).expect("loaded json"),
            serde_json::to_value(&changed).expect("changed json")
        );
    }

    /// A file written at version 6 opens, is stamped current, and keeps every field definition
    /// byte for byte, reading it with no columns.
    #[test]
    fn version_6_file_should_open_with_its_fields_untouched_and_without_columns() {
        let (_dir, path) = temp_path();
        let (field_id, task_id) = {
            let db = Db::connect(&path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Old")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let task = db.create_task(&column, None, "T", 1, None).expect("task");
            let kinds = [
                (FieldKind::Select, vec!["a".to_string(), "b".to_string()]),
                (FieldKind::Multiline, Vec::new()),
            ];
            let mut first = String::new();
            for (n, (kind, options)) in kinds.into_iter().enumerate() {
                let created = db
                    .create_field(FieldDefRow {
                        id: String::new(),
                        board_id: board.clone(),
                        key: format!("f{n}"),
                        label: format!("F{n}"),
                        kind,
                        options,
                        columns: Vec::new(),
                        default: None,
                        required: n == 0,
                        owner: FieldOwner::User,
                        position: 0,
                        created_at: String::new(),
                    })
                    .expect("field");
                if let FieldCreateOutcome::Created(field) = created
                    && n == 0
                {
                    first = field.id;
                }
            }
            mutate(&db.db, TASKS, &task, |row: &mut TaskRow| {
                row.fields.insert("f0".to_string(), serde_json::json!("a"));
            })
            .expect("value");
            (first, task)
        };

        // A version-6 row has no `columns` key
        let raw_rows = |path: &str| {
            let db = Database::open(path).expect("open");
            let txn = db.begin_read().expect("read");
            let defs = txn.open_table(FIELD_DEFS).expect("field defs");
            defs.iter()
                .expect("iter")
                .map(|item| {
                    let (key, value) = item.expect("row");
                    (key.value().to_string(), value.value().to_vec())
                })
                .collect::<Vec<_>>()
        };
        {
            let db = Database::open(&path).expect("open");
            let txn = db.begin_write().expect("write");
            {
                let mut defs = txn.open_table(FIELD_DEFS).expect("field defs");
                let rows = raw_rows_in(&defs);
                for (key, raw) in rows {
                    let mut row: serde_json::Value = serde_json::from_slice(&raw).expect("json");
                    row.as_object_mut().expect("object").remove("columns");
                    let raw = serde_json::to_vec(&row).expect("bytes");
                    defs.insert(key.as_str(), raw.as_slice()).expect("insert");
                }
            }
            txn.commit().expect("commit");
        }
        let before = raw_rows(&path);
        assert!(
            before
                .iter()
                .all(|(_, raw)| !String::from_utf8_lossy(raw).contains("columns"))
        );
        stamp(&path, 6);
        drop(Db::connect(&path).expect("open version-6 file"));
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        assert_eq!(raw_rows(&path), before, "the upgrade rewrites no field row");

        let db = Db::connect(&path).expect("reopen");
        let field = db.field(&field_id).expect("read").expect("field kept");
        assert_eq!(
            (field.kind, field.options, field.required),
            (
                FieldKind::Select,
                vec!["a".to_string(), "b".to_string()],
                true
            )
        );
        assert!(field.columns.is_empty());
        let task = db.task(&task_id).expect("read").expect("task kept");
        assert_eq!(task.fields["f0"], serde_json::json!("a"));
    }

    fn raw_rows_in(defs: &redb::Table<'_, &str, &[u8]>) -> Vec<(String, Vec<u8>)> {
        defs.iter()
            .expect("iter")
            .map(|item| {
                let (key, value) = item.expect("row");
                (key.value().to_string(), value.value().to_vec())
            })
            .collect()
    }

    /// A file written at version 2 opens, is stamped current, and keeps every stored value. Its
    /// subtasks read with no due date until one is set.
    #[test]
    fn version_2_file_should_open_stamped_current_with_every_value_intact() {
        let (_dir, path) = temp_path();
        let (task_id, sub_id) = {
            let db = Db::connect(&path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Old")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let task = db
                .create_task(&column, None, "Kept", 1, None)
                .expect("task");
            let sub = db
                .add_subtask(&task, "Step")
                .expect("subtask")
                .expect("task exists");
            mutate(&db.db, TASKS, &task, |row: &mut TaskRow| {
                row.subtasks[0].done = true;
                row.due_at = Some("2026-10-20T17:00:00+00:00".to_string());
            })
            .expect("write");
            (task, sub.id)
        };

        // A version-2 row has no `due_at` key on its subtasks
        {
            let db = Database::open(&path).expect("open");
            let txn = db.begin_read().expect("read");
            let tasks = txn.open_table(TASKS).expect("tasks");
            let guard = tasks.get(task_id.as_str()).expect("get").expect("row");
            let row: serde_json::Value = serde_json::from_slice(guard.value()).expect("json");
            assert!(
                row["subtasks"][0].get("due_at").is_none(),
                "an unset subtask due date is not stored"
            );
        }
        stamp(&path, 2);
        drop(Db::connect(&path).expect("open version-2 file"));
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        let db = Db::connect(&path).expect("reopen");
        let task = db.task(&task_id).expect("read").expect("task kept");
        assert_eq!(task.name, "Kept");
        assert_eq!(task.due_at.as_deref(), Some("2026-10-20T17:00:00+00:00"));
        assert_eq!(task.subtasks.len(), 1);
        let sub = &task.subtasks[0];
        assert_eq!(
            (sub.id.as_str(), sub.name.as_str()),
            (sub_id.as_str(), "Step")
        );
        assert!(sub.done);
        assert_eq!(sub.due_at, None);
        mutate(&db.db, TASKS, &task_id, |row: &mut TaskRow| {
            row.subtasks[0].due_at = Some("2026-10-21T09:00:00+00:00".to_string());
        })
        .expect("set subtask due");
        let task = db.task(&task_id).expect("read").expect("task");
        assert_eq!(
            task.subtasks[0].due_at.as_deref(),
            Some("2026-10-21T09:00:00+00:00")
        );
    }

    #[test]
    fn version_eighteen_file_should_stamp_the_column_entry_of_every_task() {
        let (_dir, path) = temp_path();
        let (board, moved, untouched, stamped) = {
            let db = Db::connect(&path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Old")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let make = |name: &str| db.create_task(&column, None, name, 1, None).expect("task");
            let (moved, untouched, stamped) = (make("Moved"), make("Untouched"), make("Stamped"));
            for id in [&moved, &untouched] {
                mutate(&db.db, TASKS, id, |row: &mut TaskRow| {
                    row.column_entered_at = None;
                })
                .expect("clear stamp");
            }
            mutate(&db.db, TASKS, &moved, |row: &mut TaskRow| {
                for (kind, at) in [
                    ("moved", "2026-01-02T03:04:05+00:00"),
                    ("comment", "2026-02-01T00:00:00+00:00"),
                    ("moved", "2026-01-09T10:00:00+00:00"),
                    ("comment", "2026-02-02T00:00:00+00:00"),
                ] {
                    row.history.push(TaskEvent {
                        id: at.to_string(),
                        kind: kind.to_string(),
                        detail: String::new(),
                        actor: "a".to_string(),
                        created_at: at.to_string(),
                        column_id: None,
                        token_name: None,
                    });
                }
            })
            .expect("add history");
            mutate(&db.db, TASKS, &stamped, |row: &mut TaskRow| {
                row.column_entered_at = Some("2026-03-03T00:00:00+00:00".to_string());
            })
            .expect("set stamp");
            (board, moved, untouched, stamped)
        };
        stamp(&path, 18);

        drop(Db::connect(&path).expect("open version-18 file"));

        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        let db = Db::connect(&path).expect("reopen");
        let read = |id: &str| db.task(id).expect("read").expect("task kept");
        assert_eq!(
            read(&moved).column_entered_at.as_deref(),
            Some("2026-01-09T10:00:00+00:00"),
            "the latest move"
        );
        let untouched = read(&untouched);
        assert_eq!(
            untouched.column_entered_at.as_deref(),
            Some(untouched.created_at.as_str()),
            "no move, so the creation"
        );
        assert_eq!(
            read(&stamped).column_entered_at.as_deref(),
            Some("2026-03-03T00:00:00+00:00"),
            "a stamp already there stays"
        );
        assert_eq!(
            db.board(&board)
                .expect("read")
                .expect("board")
                .created_by_token,
            ""
        );
    }

    #[test]
    fn version_twenty_two_file_should_keep_the_column_stamp_under_its_new_name() {
        const STAMP: &str = "2026-04-05T06:07:08+00:00";
        let (_dir, path) = temp_path();
        let (kept, bare) = {
            let db = Db::connect(&path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Old")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let make = |name: &str| db.create_task(&column, None, name, 1, None).expect("task");
            (make("Kept"), make("Bare"))
        };
        {
            let raw = Database::create(&path).expect("raw open");
            let txn = raw.begin_write().expect("write txn");
            {
                let mut tbl = txn.open_table(TASKS).expect("tasks");
                for (id, stamp) in [(&kept, Some(STAMP)), (&bare, None)] {
                    let bytes = tbl
                        .get(id.as_str())
                        .expect("get")
                        .expect("row")
                        .value()
                        .to_vec();
                    let mut row: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
                    let object = row.as_object_mut().expect("object");
                    object.remove("column_entered_at");
                    if let Some(stamp) = stamp {
                        object.insert(
                            "column_added_at".to_string(),
                            serde_json::Value::from(stamp),
                        );
                    }
                    let bytes = serde_json::to_vec(&row).expect("bytes");
                    tbl.insert(id.as_str(), bytes.as_slice()).expect("insert");
                }
            }
            txn.commit().expect("commit");
        }
        stamp(&path, 22);

        drop(Db::connect(&path).expect("open version-22 file"));

        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        let db = Db::connect(&path).expect("reopen");
        let read = |id: &str| db.task(id).expect("read").expect("task kept");
        assert_eq!(read(&kept).column_entered_at.as_deref(), Some(STAMP));
        assert_eq!(read(&bare).column_entered_at, None);
    }

    #[test]
    fn version_eight_file_should_upgrade_to_default_backup_settings() {
        let (_dir, path) = temp_path();
        let board = {
            let db = Db::connect(&path).expect("connect");
            db.create_board(&AccountId::ownerless(), "Kept")
                .expect("board")
        };
        stamp(&path, 8);
        assert_eq!(stamp_of(&path), Some(8));

        drop(Db::connect(&path).expect("open version-8 file"));

        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        let db = Db::connect(&path).expect("reopen");
        assert_eq!(
            db.backup_settings().expect("settings"),
            BackupSettings::default()
        );
        assert_eq!(db.backup_status().expect("status"), None);
        assert_eq!(
            db.board(&board).expect("read").expect("board kept").name,
            "Kept"
        );
    }

    #[test]
    fn newer_db_should_be_refused_and_left_byte_identical() {
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
            "upgrade pomb",
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
            TEMPLATE_SNAPSHOTS,
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
    fn crash_left_newer_db_should_be_refused_without_application_writes() {
        let (dir, path) = temp_path();
        let db = Db::connect(&path).expect("connect");
        db.create_board(&AccountId::ownerless(), "Kept")
            .expect("create board");
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
    fn crash_left_current_db_should_be_repaired_and_open() {
        let (dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        let killed = dir.path().join("killed.redb");
        let killed = killed.to_str().expect("utf8 path").to_string();
        crash_left_copy(&path, &killed, SCHEMA_VERSION);

        let db = Db::connect(&killed).expect("open crash-left file");
        assert_eq!(db.list_boards().expect("boards").len(), 1);
    }

    #[test]
    fn failed_crash_repair_should_be_reported_as_such() {
        let (dir, path) = temp_path();
        drop(Db::connect(&path).expect("connect"));
        let killed = dir.path().join("killed.redb");
        let killed = killed.to_str().expect("utf8 path").to_string();
        crash_left_copy(&path, &killed, SCHEMA_VERSION);
        let mut perms = std::fs::metadata(&killed).expect("metadata").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&killed, perms).expect("make read-only");

        // Root (or CAP_DAC_OVERRIDE) writes through the read-only bit, so the repair cannot be made
        // to fail this way.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&killed)
            .is_ok()
        {
            eprintln!("skipped: this process can write to read-only files");
            return;
        }

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
    fn open_errors_should_name_the_database_path() {
        let (dir, _) = temp_path();
        let path = dir.path().to_str().expect("utf8 path").to_string();
        let err = match Db::connect(&path) {
            Ok(_) => panic!("a directory is not a database"),
            Err(err) => err,
        };
        assert!(err.to_string().contains(&path), "{err}");
    }

    #[test]
    fn equal_version_check_should_write_nothing() {
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
    use crate::models::{
        BoardEventRow, BreakActivity, DiskLimits, FieldColumn, InviteRow, PruneSettings,
        PruneStatus, Quotas, RateLimits, RegistrationMode, TimerMode,
    };

    /// Deserializer that feeds placeholder values to a derived `Deserialize`
    /// impl and records the shape it asks for: field names in sorted order,
    /// primitive kinds, options, sequences, nested structs and enum variants.
    #[derive(Copy, Clone)]
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

    fn table<K: Key + 'static, V: Value + 'static>(
        def: TableDefinition<'_, K, V>,
    ) -> (String, String) {
        let name = def.name().to_string();
        let line = format!(
            "table {name}: {} -> {}",
            K::type_name().name(),
            V::type_name().name()
        );
        (name, line)
    }

    fn multimap<K: Key + 'static, V: Key + 'static>(
        def: MultimapTableDefinition<'_, K, V>,
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
            shape::<BackupSettings>("BackupSettings"),
            shape::<BackupStatus>("BackupStatus"),
            shape::<BoardEventRow>("BoardEventRow"),
            shape::<BoardRow>("BoardRow"),
            shape::<BoardTemplateRow>("BoardTemplateRow"),
            shape::<BreakActivity>("BreakActivity"),
            shape::<ColorRow>("ColorRow"),
            shape::<ColumnRow>("ColumnRow"),
            shape::<DeviceAuthRow>("DeviceAuthRow"),
            shape::<DiskLimits>("DiskLimits"),
            shape::<FieldColumn>("FieldColumn"),
            shape::<FieldDefRow>("FieldDefRow"),
            shape::<AccountStatus>("AccountStatus"),
            shape::<InstanceSettings>("InstanceSettings"),
            shape::<InviteRow>("InviteRow"),
            shape::<PasskeyRow>("PasskeyRow"),
            shape::<PruneSettings>("PruneSettings"),
            shape::<PruneStatus>("PruneStatus"),
            shape::<Quotas>("Quotas"),
            shape::<RateLimits>("RateLimits"),
            shape::<RecoveryCodeRow>("RecoveryCodeRow"),
            shape::<RegistrationMode>("RegistrationMode"),
            shape::<SessionRow>("SessionRow"),
            shape::<Settings>("Settings"),
            shape::<LaneLimit>("LaneLimit"),
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
            table(TEMPLATE_SNAPSHOTS),
            table(ATTACHMENT_DATA),
            table(META),
            table(FIELD_DEFS),
            table(DEVICE_AUTHS),
            table(PASSKEYS),
            table(ACCOUNT_SETTINGS),
            table(RECOVERY_CODES),
            table(INVITES),
            table(ACTIVITY),
            table(SUSPENDED_TOKENS),
            table(BOARD_ACTIVITY),
            table(ACCOUNT_USAGE),
            table(BOARD_EVENTS),
            table(EVENT_WAKEUPS),
            table(FEED_TOKENS),
            table(PASSKEYS_BY_CREDENTIAL),
            multimap(TOKENS_BY_ACCOUNT),
            multimap(SESSIONS_BY_USER),
            multimap(DEVICES_BY_ACCOUNT),
            table(TOKEN_IDS_BY_HASH),
            multimap(SALTED_TOKENS_BY_LAST4),
            multimap(DEVICES_BY_USER_CODE),
            multimap(DEVICES_BY_EXPIRY),
            multimap(ENTRIES_BY_ACCOUNT),
            multimap(ENTRIES_BY_DAY),
            multimap(PASSKEYS_BY_USER),
            multimap(BOARDS_BY_OWNER),
            multimap(TEMPLATES_BY_OWNER),
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
    fn stored_schema_should_match_fingerprint() {
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
    //! Boards that predate colors get the standard palette at startup, and
    //! listing colors never writes. The test opens a database holding colorless
    //! boards, checks that the startup backfill added the palette, and checks
    //! that a later read wrote nothing.

    use super::*;

    #[test]
    fn colorless_boards_should_get_colors_at_startup_not_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let board = {
            let db = Db::connect(path).expect("connect");
            let txn = db.db.begin_write().expect("write");
            let board = txn_insert_board(
                &txn,
                &AccountId::ownerless(),
                "Legacy",
                "",
                OverQuota::Allow,
            )
            .expect("board");
            txn.commit().expect("commit");
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

/// The default board is the lowest-position unarchived board, and an archived one only when
/// every board is archived.
#[cfg(test)]
mod first_board_tests {
    use super::*;

    fn replace_boards(db: &Db, boards: &[(&str, i64, bool)]) {
        let txn = db.db.begin_write().expect("write txn");
        {
            let mut tbl = txn.open_table(BOARDS).expect("boards table");
            tbl.retain(|_, _| false).expect("clear boards");
            let mut by_owner = txn.open_multimap_table(BOARDS_BY_OWNER).expect("index");
            by_owner.remove_all("").expect("clear index");
        }
        for (id, position, archived) in boards {
            let row = BoardRow {
                id: (*id).to_string(),
                name: (*id).to_string(),
                position: *position,
                owner_id: String::new(),
                config_json: serde_json::json!({ "archived": archived }).to_string(),
                created_by_token: String::new(),
            };
            write_one(&txn, BOARDS, id, &row).expect("write board");
            mmap_insert(&txn, BOARDS_BY_OWNER, "", id).expect("index board");
        }
        txn.commit().expect("commit");
    }

    fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        (dir, db)
    }

    #[test]
    fn first_board_id_should_skip_archived_boards() {
        let (_dir, db) = test_db();
        replace_boards(&db, &[("old", 0, true), ("live", 1, false)]);
        assert_eq!(
            db.first_board_id(&AccountId::ownerless()).expect("first"),
            Some("live".to_string())
        );
    }

    #[test]
    fn first_board_id_should_fall_back_to_lowest_archived_board() {
        let (_dir, db) = test_db();
        replace_boards(&db, &[("b", 2, true), ("a", 1, true)]);
        assert_eq!(
            db.first_board_id(&AccountId::ownerless()).expect("first"),
            Some("a".to_string())
        );
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::auth::new_api_token_record;
    use crate::models::TimerMode;

    fn temp_path() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        (dir, path.to_str().expect("utf8 path").to_string())
    }

    /// Writes `row` as raw JSON, the way a version 9 binary stored it.
    fn put(txn: &WriteTransaction, table: Rows, key: &str, row: serde_json::Value) {
        let raw = serde_json::to_vec(&row).expect("json");
        txn.open_table(table)
            .expect("table")
            .insert(key, raw.as_slice())
            .expect("insert");
    }

    /// Reads a raw JSON row.
    fn get(db: &Db, table: Rows, key: &str) -> Option<serde_json::Value> {
        read_one(&db.db, table, key).expect("read")
    }

    #[test]
    fn migration_should_assign_every_row_to_the_owner() {
        let (_dir, path) = temp_path();
        {
            let db = Db::connect(&path).expect("connect");
            let txn = db.db.begin_write().expect("write");
            txn.open_table(BOARDS)
                .expect("boards")
                .retain(|_, _| false)
                .expect("clear boards");
            txn.open_multimap_table(BOARDS_BY_OWNER)
                .expect("index")
                .remove_all("")
                .expect("clear index");
            txn.open_table(SETTINGS)
                .expect("settings")
                .remove(INSTANCE_SETTINGS_KEY)
                .expect("clear instance");
            put(
                &txn,
                USERS,
                "u1",
                serde_json::json!({"id": "u1", "username": "chip"}),
            );
            txn.open_table(USERNAMES)
                .expect("usernames")
                .insert("chip", "u1")
                .expect("username");
            put(
                &txn,
                BOARDS,
                "b1",
                serde_json::json!({"id": "b1", "name": "Work", "position": 0, "config_json": "{}"}),
            );
            let (token, _) =
                new_api_token_record(&AccountId::ownerless(), "bot", AuthScope::Read, None);
            let mut token = serde_json::to_value(&token).expect("token");
            token.as_object_mut().expect("object").remove("account_id");
            put(&txn, API_TOKENS, "t1", token);
            put(
                &txn,
                TEMPLATES,
                "saved",
                serde_json::json!({
                    "id": "saved", "name": "Saved", "description": "", "built_in": false,
                    "snapshot": {},
                }),
            );
            put(
                &txn,
                TIME_ENTRIES,
                "e1",
                serde_json::json!({
                    "id": "e1", "task_id": "", "minutes": 5, "note": "",
                    "started_at": "2026-01-01T00:00:00Z",
                }),
            );
            put(
                &txn,
                DEVICE_AUTHS,
                "approved",
                serde_json::json!({
                    "device_code_hash": "approved", "user_code": "BCDFGHJK", "client_name": "cli",
                    "scope": "read", "status": "approved", "created_at": 0, "expires_at": 0,
                    "last_poll_at": null, "token_expires_at": null,
                }),
            );
            let settings = Settings {
                pomodoro_minutes: 50,
                ..Settings::default()
            };
            put(
                &txn,
                SETTINGS,
                LEGACY_APP_SETTINGS_KEY,
                serde_json::to_value(&settings).expect("settings"),
            );
            put(
                &txn,
                ACTIVE_TIMER,
                LEGACY_TIMER_KEY,
                serde_json::json!({"task_id": null, "mode": "stopwatch", "started_at": 1,
                    "duration_secs": null}),
            );
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 9)
                .expect("stamp");
            txn.commit().expect("commit");
        }

        let db = Db::connect(&path).expect("upgrade from version 9");
        let owner = db.user("u1").expect("read").expect("owner");
        assert_eq!(owner.role, AccountRole::Admin);
        assert_eq!(owner.status, AccountStatus::Active);
        assert!(owner.created_at > 0);
        let u1 = AccountId::new("u1");
        assert_eq!(db.board("b1").expect("read").expect("board").owner_id, "u1");
        assert_eq!(db.owner_boards(&u1).expect("boards").len(), 1);
        assert_eq!(db.list_api_tokens().expect("tokens")[0].account_id, "u1");
        let saved = db.template("saved").expect("read").expect("template");
        assert_eq!(saved.owner_id.as_deref(), Some("u1"));
        let snapshot = txn_template_snapshot(&db.read().expect("read"), "saved")
            .expect("snapshot")
            .expect("moved to its own table");
        assert_eq!(snapshot, serde_json::json!({}));
        assert_eq!(saved.snapshot_bytes, 2);
        assert!(
            db.list_templates(&u1)
                .expect("templates")
                .iter()
                .any(|template| template.id == "saved")
        );
        assert!(
            db.list_templates(&AccountId::new("someone else"))
                .expect("templates")
                .iter()
                .all(|template| template.id != "saved")
        );
        for builtin in db
            .list_templates(&u1)
            .expect("templates")
            .iter()
            .filter(|template| template.built_in)
        {
            assert_eq!(builtin.owner_id, None, "{}", builtin.name);
        }
        assert_eq!(
            db.entry("e1").expect("read").expect("entry").account_id,
            "u1"
        );
        assert_eq!(
            get(&db, DEVICE_AUTHS, "approved").expect("device")["account_id"],
            "u1"
        );
        assert_eq!(db.settings(&u1).expect("settings").pomodoro_minutes, 50);
        assert!(get(&db, SETTINGS, LEGACY_APP_SETTINGS_KEY).is_none());
        assert!(db.active_timer(&u1).expect("timer").is_some());
        assert!(get(&db, ACTIVE_TIMER, LEGACY_TIMER_KEY).is_none());
        assert_eq!(
            read_one::<InstanceSettings>(&db.db, SETTINGS, INSTANCE_SETTINGS_KEY).expect("read"),
            Some(InstanceSettings::default())
        );
    }

    #[test]
    fn first_account_should_claim_the_rows_made_before_it() {
        let (_dir, path) = temp_path();
        let db = Db::connect(&path).expect("connect");
        let starter = db
            .first_board_id(&AccountId::ownerless())
            .expect("first")
            .expect("starter board");
        let early = db
            .create_board(&AccountId::ownerless(), "Early")
            .expect("board");
        db.update_settings(
            &AccountId::ownerless(),
            &Settings {
                pomodoro_minutes: 40,
                ..Settings::default()
            },
        )
        .expect("settings");
        let (token, _) =
            new_api_token_record(&AccountId::ownerless(), "bot", AuthScope::Read, None);
        db.create_api_token(&token).expect("token");

        let owner = db.ensure_owner("owner").expect("owner");
        let owner_id = owner.account_id();
        assert_eq!(owner.role, AccountRole::Admin);
        for board in [&starter, &early] {
            assert_eq!(
                db.board(board).expect("read").expect("board").owner_id,
                owner.id
            );
        }
        assert!(
            db.owner_boards(&AccountId::ownerless())
                .expect("boards")
                .is_empty()
        );
        assert_eq!(
            db.settings(&owner_id).expect("settings").pomodoro_minutes,
            40
        );
        assert_eq!(
            db.list_api_tokens().expect("tokens")[0].account_id,
            owner.id
        );

        let member = db.create_user("member").expect("member");
        assert_eq!(
            db.user(member.as_str())
                .expect("read")
                .expect("member")
                .role,
            AccountRole::Member
        );
        assert_eq!(db.owner().expect("owner").expect("owner").id, owner.id);
    }

    #[test]
    fn ownerless_writes_should_be_refused_once_an_account_exists() {
        let (_dir, path) = temp_path();
        let db = Db::connect(&path).expect("connect");
        let ownerless = AccountId::ownerless();
        let board = db
            .first_board_id(&ownerless)
            .expect("first")
            .expect("starter board");
        db.ensure_owner("owner").expect("owner");

        let refused = |result: DbResult<()>| {
            assert_eq!(
                result.expect_err("ownerless write").to_string(),
                OWNER_EXISTS
            );
        };
        refused(db.create_board(&ownerless, "Late").map(drop));
        refused(db.update_settings(&ownerless, &Settings::default()));
        let timer = ActiveTimer {
            task_id: None,
            mode: TimerMode::Stopwatch,
            started_at: 1,
            duration_secs: None,
        };
        refused(db.set_active_timer(&ownerless, &timer));
        let (token, _) = new_api_token_record(&ownerless, "late", AuthScope::Read, None);
        refused(db.create_api_token(&token));
        refused(
            db.save_board_as_template(&ownerless, &board, "Late", "")
                .map(drop),
        );
        assert!(db.owner_boards(&ownerless).expect("boards").is_empty());
        assert!(db.list_api_tokens().expect("tokens").is_empty());
    }

    #[test]
    fn account_handle_should_load_only_its_own_rows() {
        let (_dir, path) = temp_path();
        let db = Db::connect(&path).expect("connect");
        let a = db.create_user("alice").expect("alice");
        let b = db.create_user("bob").expect("bob");
        let board = db.create_board(&a, "Alice's").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let task = db
            .create_task(&column, None, "Secret", 1, None)
            .expect("task");
        let entry = db.create_entry(&task, 5, "").expect("entry");

        let alice = db.account(&a);
        let bob = db.account(&b);
        assert!(alice.board(&board).expect("read").is_some());
        assert!(alice.task(&task).expect("read").is_some());
        assert!(alice.entry(&entry).expect("read").is_some());
        assert!(bob.board(&board).expect("read").is_none());
        assert!(bob.column(&column).expect("read").is_none());
        assert!(bob.task(&task).expect("read").is_none());
        assert!(bob.entry(&entry).expect("read").is_none());
        assert!(bob.list_columns(&board).expect("columns").is_empty());
        assert!(bob.board_tasks(&board).expect("tasks").is_empty());
        bob.visit_board_tasks(&board, |_: TaskRow| Err("another account's task".into()))
            .expect("no task of another account is visited");
        let listed: Vec<String> = alice
            .all_entries()
            .expect("entries")
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(listed, std::slice::from_ref(&entry));
        assert!(bob.all_entries().expect("entries").is_empty());
        assert!(
            bob.list_boards()
                .expect("boards")
                .iter()
                .all(|row| row.id != board)
        );
        assert_eq!(bob.list_users().expect("users").len(), 1);
        assert_eq!(
            db.account(&b)
                .create_entry_full(NewTimeEntry {
                    task_id: Some(&task),
                    started_at: "2026-01-01T00:00:00Z",
                    minutes: 1,
                    seconds: 60,
                    note: "",
                    kind: EntryKind::Manual,
                    interrupted: false,
                    interrupt_reason: None,
                    labels: &[],
                    created_by: "bob",
                    activity_id: None,
                })
                .expect("entry"),
            None,
            "an entry on another account's task is refused"
        );
    }

    #[test]
    fn last_board_refusal_should_count_only_the_owners_boards() {
        let (_dir, path) = temp_path();
        let db = Db::connect(&path).expect("connect");
        let a = db.create_user("alice").expect("alice");
        let b = db.create_user("bob").expect("bob");
        let only = db.create_board(&b, "Bob's only board").expect("board");
        assert!(db.owner_boards(&a).expect("boards").len() == 1);
        assert_eq!(
            db.delete_board(&only).expect_err("last board").to_string(),
            LAST_BOARD
        );
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    /// An open database with one account and its path's directory.
    fn open() -> (tempfile::TempDir, Db, AccountId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Db::connect(dir.path().join("sessions.redb").to_str().expect("utf8")).expect("connect");
        let account = db.create_user("admin").expect("admin");
        (dir, db, account)
    }

    /// Stores a session of `account` that started `age` seconds ago, returning its token.
    fn session_aged(db: &Db, account: &AccountId, age: i64) -> String {
        let token = Uuid::new_v4().to_string();
        let started = Utc::now() - chrono::Duration::seconds(age);
        let row = SessionRow {
            user_id: account.as_str().to_string(),
            created_at: started.to_rfc3339(),
            verified_at: None,
        };
        let txn = db.db.begin_write().expect("write");
        let key = hash_session_token(&token);
        write_one(&txn, SESSIONS, &key, &row).expect("session");
        mmap_insert(&txn, SESSIONS_BY_USER, account.as_str(), &key).expect("index");
        txn.commit().expect("commit");
        token
    }

    /// Returns every session key indexed under `account`.
    fn indexed(db: &Db, account: &AccountId) -> Vec<String> {
        let txn = db.db.begin_write().expect("write");
        txn_mmap_get(&txn, SESSIONS_BY_USER, account.as_str()).expect("index")
    }

    /// Returns whether the session `token` is still stored.
    fn stored(db: &Db, token: &str) -> bool {
        read_one::<SessionRow>(&db.db, SESSIONS, &hash_session_token(token))
            .expect("read")
            .is_some()
    }

    /// Returns every stored session as its key and its raw value.
    fn session_rows(db: &Db) -> Vec<(String, String)> {
        let txn = db.db.begin_read().expect("read");
        let tbl = txn.open_table(SESSIONS).expect("sessions");
        tbl.iter()
            .expect("iter")
            .map(|item| {
                let (key, value) = item.expect("row");
                (
                    key.value().to_string(),
                    String::from_utf8_lossy(value.value()).into_owned(),
                )
            })
            .collect()
    }

    const MAX_AGE: i64 = SESSION_MAX_AGE_SECONDS as i64;

    #[test]
    fn user_for_token_should_refuse_and_delete_a_session_past_its_age() {
        let (_dir, db, account) = open();
        let fresh = session_aged(&db, &account, MAX_AGE - 60);
        let expired = session_aged(&db, &account, MAX_AGE + 1);
        assert!(db.user_for_token(&fresh).expect("read").is_some());
        assert!(db.user_for_token(&expired).expect("read").is_none());
        assert!(!stored(&db, &expired));
        assert!(stored(&db, &fresh));
    }

    #[test]
    fn prune_sessions_should_end_only_the_expired_ones() {
        let (_dir, db, account) = open();
        let fresh = db.create_session(&account).expect("session");
        let expired = session_aged(&db, &account, MAX_AGE + 1);
        let now = Utc::now().timestamp();
        assert_eq!(db.prune_sessions(now).expect("prune"), 1);
        assert!(stored(&db, &fresh));
        assert!(!stored(&db, &expired));
        assert_eq!(indexed(&db, &account), [hash_session_token(&fresh)]);
        assert_eq!(db.prune_sessions(now).expect("prune"), 0);
    }

    #[test]
    fn end_other_sessions_should_keep_the_current_one_and_other_accounts() {
        let (_dir, db, account) = open();
        let other = db.create_user("bob").expect("member");
        let current = db.create_session(&account).expect("session");
        let elsewhere = db.create_session(&account).expect("session");
        let theirs = db.create_session(&other).expect("session");
        assert_eq!(
            db.end_other_sessions(&account, Some(&current))
                .expect("end"),
            1
        );
        assert!(stored(&db, &current));
        assert!(!stored(&db, &elsewhere));
        assert!(stored(&db, &theirs));
    }

    #[test]
    fn session_is_fresh_should_hold_for_the_window_after_a_passkey_check() {
        let (_dir, db, account) = open();
        let created = db.create_session(&account).expect("session");
        let unverified = session_aged(&db, &account, 0);
        let now = Utc::now().timestamp();
        assert!(db.session_is_fresh(&created, now).expect("read"));
        assert!(
            db.session_is_fresh(&created, now + REAUTH_WINDOW_SECS - 5)
                .expect("read")
        );
        assert!(
            !db.session_is_fresh(&created, now + REAUTH_WINDOW_SECS + 5)
                .expect("read")
        );
        assert!(!db.session_is_fresh(&unverified, now).expect("read"));
        assert!(!db.session_is_fresh("no such session", now).expect("read"));
    }

    #[test]
    fn sessions_table_should_hold_no_raw_cookie_value() {
        let (_dir, db, account) = open();
        let tokens = [
            db.create_session(&account).expect("session"),
            session_aged(&db, &account, 0),
        ];
        let rows = session_rows(&db);
        assert_eq!(rows.len(), tokens.len());
        for token in &tokens {
            for (key, value) in &rows {
                assert_ne!(key, token);
                assert!(!value.contains(token.as_str()), "{value}");
            }
            let key = hash_session_token(token);
            assert!(rows.iter().any(|(held, _)| *held == key));
            assert!(db.user_for_token(token).expect("read").is_some());
        }
        let hashed = hash_session_token(&tokens[0]);
        assert!(db.user_for_token(&hashed).expect("read").is_none());
    }

    #[test]
    fn delete_session_should_end_only_the_session_of_that_cookie() {
        let (_dir, db, account) = open();
        let ended = db.create_session(&account).expect("session");
        let kept = db.create_session(&account).expect("session");
        db.delete_session(&ended).expect("delete");
        assert!(!stored(&db, &ended));
        assert!(stored(&db, &kept));
    }

    #[test]
    fn create_session_should_refuse_a_suspended_account() {
        let (_dir, db, _admin) = open();
        let member = db.create_user("bob").expect("member");
        let before = db.create_session(&member).expect("session");
        let outcome = db
            .apply_account_action(member.as_str(), AccountAction::Suspend)
            .expect("suspend");
        assert!(matches!(outcome, AdminActionOutcome::Done(_)));
        assert!(!stored(&db, &before));

        let refusal = db.create_session(&member).expect_err("suspended account");
        assert!(refusal.downcast_ref::<SessionRefused>().is_some());
        assert!(session_rows(&db).is_empty());
    }

    #[test]
    fn create_session_should_allow_a_pending_account() {
        let (_dir, db, _admin) = open();
        let member = db.create_user("bob").expect("member");
        mutate(&db.db, USERS, member.as_str(), |row: &mut UserRow| {
            row.status = AccountStatus::Pending;
        })
        .expect("pend");
        let token = db
            .create_session(&member)
            .expect("a pending account signs in");
        assert!(stored(&db, &token));
    }

    #[test]
    fn create_session_should_refuse_an_absent_account() {
        let (_dir, db, _admin) = open();
        let ghost = AccountId::new("ghost");
        let refusal = db.create_session(&ghost).expect_err("absent account");
        assert!(refusal.downcast_ref::<SessionRefused>().is_some());
        assert!(session_rows(&db).is_empty());
    }

    #[test]
    fn create_session_should_stay_gone_after_a_suspension_and_unsuspend() {
        let (_dir, db, _admin) = open();
        let member = db.create_user("bob").expect("member");
        let before = db.create_session(&member).expect("session");
        for action in [AccountAction::Suspend, AccountAction::Unsuspend] {
            let outcome = db
                .apply_account_action(member.as_str(), action)
                .expect("status change");
            assert!(matches!(outcome, AdminActionOutcome::Done(_)));
            assert!(!stored(&db, &before));
            assert!(session_rows(&db).is_empty());
        }
    }

    #[test]
    fn no_session_should_survive_unsuspend_when_sign_in_races_a_suspension() {
        let (_dir, db, _admin) = open();
        let member = db.create_user("bob").expect("member");
        for _ in 0..25 {
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    let _ = db.create_session(&member);
                });
                barrier.wait();
                let outcome = db
                    .apply_account_action(member.as_str(), AccountAction::Suspend)
                    .expect("suspend");
                assert!(matches!(outcome, AdminActionOutcome::Done(_)));
            });
            assert!(session_rows(&db).is_empty());
            let outcome = db
                .apply_account_action(member.as_str(), AccountAction::Unsuspend)
                .expect("unsuspend");
            assert!(matches!(outcome, AdminActionOutcome::Done(_)));
            assert!(session_rows(&db).is_empty());
            db.create_session(&member)
                .expect("an active account signs in");
            db.end_other_sessions(&member, None).expect("end");
        }
    }

    #[test]
    fn upgrade_from_version_17_should_drop_sessions_and_keep_tokens_and_passkeys() {
        use crate::auth::new_api_token_record;
        use crate::models::AuthScope;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("v17.redb");
        let path = path.to_str().expect("utf8");
        let (account, session) = {
            let db = Db::connect(path).expect("connect");
            let account = db.create_user("admin").expect("admin");
            let session = db.create_session(&account).expect("session");
            let (record, _) = new_api_token_record(&account, "bot", AuthScope::Read, None);
            db.create_api_token(&record).expect("token");
            let passkey = PasskeyRow {
                id: Uuid::new_v4().to_string(),
                user_id: account.as_str().to_string(),
                name: "Laptop".to_string(),
                created_at: 0,
                last_used_at: None,
                credential: serde_json::json!({}),
            };
            let txn = db.db.begin_write().expect("write");
            write_one(&txn, PASSKEYS, &passkey.id, &passkey).expect("passkey");
            mmap_insert(&txn, PASSKEYS_BY_USER, &passkey.user_id, &passkey.id).expect("index");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 17)
                .expect("stamp");
            txn.commit().expect("commit");
            (account, session)
        };
        let db = Db::connect(path).expect("upgrade from version 17");
        assert!(!stored(&db, &session));
        assert!(session_rows(&db).is_empty());
        assert_eq!(db.list_api_tokens().expect("tokens").len(), 1);
        assert_eq!(db.passkeys_of(&account).expect("passkeys").len(), 1);
    }

    #[test]
    fn verify_session_should_stamp_only_that_session() {
        let (_dir, db, account) = open();
        let checked = session_aged(&db, &account, 0);
        let other = session_aged(&db, &account, 0);
        let now = Utc::now().timestamp();
        assert!(db.verify_session(&checked).expect("verify"));
        assert!(db.session_is_fresh(&checked, now).expect("read"));
        assert!(!db.session_is_fresh(&other, now).expect("read"));
        assert!(!db.verify_session("no such session").expect("verify"));
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;
    use crate::auth::new_api_token_record;

    /// Returns a pending request from `network`, numbered `n` for its codes.
    fn request(n: usize, network: &str, now: i64) -> DeviceAuthRow {
        DeviceAuthRow {
            device_code_hash: format!("hash-{n}"),
            user_code: format!("CODE{n:04}"),
            client_name: "agent".to_string(),
            scope: AuthScope::Read,
            status: DeviceAuthStatus::Pending,
            created_at: now,
            expires_at: now + Db::DEVICE_CODE_TTL_SECONDS,
            last_poll_at: None,
            token_expires_at: None,
            account_id: None,
            client_network: Some(network.to_string()),
        }
    }

    #[test]
    fn create_device_auth_should_cap_pending_requests_across_every_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Db::connect(dir.path().join("devices.redb").to_str().expect("utf8")).expect("connect");
        let now = Utc::now().timestamp();
        let per_network = Db::MAX_PENDING_DEVICE_AUTHS_PER_NETWORK;
        let last = Db::MAX_PENDING_DEVICE_AUTHS - 1;
        let txn = db.db.begin_write().expect("write");
        for n in 0..last {
            let row = request(n, &format!("network-{}", n / per_network), now);
            write_one(&txn, DEVICE_AUTHS, &row.device_code_hash, &row).expect("request");
            txn_index_device_expiry(&txn, row.expires_at, &row.device_code_hash).expect("index");
        }
        txn.commit().expect("commit");
        let created = db
            .create_device_auth(&request(last, "the last network", now), now)
            .expect("create");
        assert!(matches!(created, DeviceAuthCreateOutcome::Created));
        let fresh = request(Db::MAX_PENDING_DEVICE_AUTHS, "a fresh network", now);
        assert!(matches!(
            db.create_device_auth(&fresh, now).expect("create"),
            DeviceAuthCreateOutcome::TooManyPending
        ));
    }

    #[test]
    fn create_device_auth_should_not_count_decided_or_expired_requests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Db::connect(dir.path().join("devices.redb").to_str().expect("utf8")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let now = Utc::now().timestamp();
        let per_network = Db::MAX_PENDING_DEVICE_AUTHS_PER_NETWORK;
        for n in 0..per_network {
            let created = db
                .create_device_auth(&request(n, "a network", now), now)
                .expect("create");
            assert!(matches!(created, DeviceAuthCreateOutcome::Created));
        }
        let sixth = request(per_network, "a network", now);
        assert!(matches!(
            db.create_device_auth(&sixth, now).expect("create"),
            DeviceAuthCreateOutcome::TooManyPending
        ));

        assert!(
            db.decide_device_auth(&admin, &request(0, "", now).user_code, None, now)
                .expect("deny")
        );
        assert!(matches!(
            db.create_device_auth(&sixth, now).expect("create"),
            DeviceAuthCreateOutcome::Created
        ));

        let later = now + Db::DEVICE_CODE_TTL_SECONDS + 1;
        for n in 0..per_network {
            let created = db
                .create_device_auth(&request(100 + n, "a network", later), later)
                .expect("create");
            assert!(
                matches!(created, DeviceAuthCreateOutcome::Created),
                "request {n}: an expired request holds no place"
            );
        }
    }

    #[test]
    fn version_twenty_two_file_should_index_its_device_requests_by_expiry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.redb");
        let path = path.to_str().expect("utf8");
        let now = Utc::now().timestamp();
        let per_network = Db::MAX_PENDING_DEVICE_AUTHS_PER_NETWORK;
        {
            let db = Db::connect(path).expect("connect");
            let txn = db.db.begin_write().expect("write");
            for n in 0..per_network {
                let row = request(n, "a network", now);
                write_one(&txn, DEVICE_AUTHS, &row.device_code_hash, &row).expect("request");
            }
            txn.commit().expect("commit");
        }
        {
            let raw = Database::create(path).expect("raw open");
            let txn = raw.begin_write().expect("write");
            txn.delete_multimap_table(DEVICES_BY_EXPIRY).expect("drop");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 22u64)
                .expect("stamp");
            txn.commit().expect("commit");
        }

        let db = Db::connect(path).expect("open version-22 file");

        let refused = db
            .create_device_auth(&request(per_network, "a network", now), now)
            .expect("create");
        assert!(matches!(refused, DeviceAuthCreateOutcome::TooManyPending));
    }

    #[test]
    fn create_device_auth_should_drop_the_index_entry_of_a_request_past_its_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Db::connect(dir.path().join("devices.redb").to_str().expect("utf8")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let now = Utc::now().timestamp();
        let approved = request(0, "a network", now);
        assert!(matches!(
            db.create_device_auth(&approved, now).expect("create"),
            DeviceAuthCreateOutcome::Created
        ));
        let grant = DeviceGrant {
            scope: AuthScope::Read,
            expires_at: None,
        };
        assert!(
            db.decide_device_auth(&admin, &approved.user_code, Some(&grant), now)
                .expect("approve")
        );
        let indexed = txn_mmap_get(
            &db.db.begin_write().expect("write"),
            DEVICES_BY_ACCOUNT,
            admin.as_str(),
        )
        .expect("index");
        assert_eq!(indexed.len(), 1);

        let later = now + Db::DEVICE_CODE_TTL_SECONDS + Db::DEVICE_AUTH_RETENTION_SECONDS + 1;
        let next = request(1, "a network", later);
        assert!(matches!(
            db.create_device_auth(&next, later).expect("create"),
            DeviceAuthCreateOutcome::Created
        ));
        let indexed = txn_mmap_get(
            &db.db.begin_write().expect("write"),
            DEVICES_BY_ACCOUNT,
            admin.as_str(),
        )
        .expect("index");
        assert!(indexed.is_empty(), "{indexed:?}");
    }

    #[test]
    fn poll_device_auth_should_keep_an_approved_request_and_its_poll_time_at_the_token_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Db::connect(dir.path().join("devices.redb").to_str().expect("utf8")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        for n in 0..MAX_API_TOKENS_PER_ACCOUNT {
            let (record, _) = new_api_token_record(&admin, &format!("t{n}"), AuthScope::Read, None);
            db.create_api_token(&record).expect("token");
        }
        let now = Utc::now().timestamp();
        let approved = DeviceAuthRow {
            status: DeviceAuthStatus::Approved,
            account_id: Some(admin.as_str().to_string()),
            ..request(0, "a network", now)
        };
        let txn = db.db.begin_write().expect("write");
        write_one(&txn, DEVICE_AUTHS, &approved.device_code_hash, &approved).expect("request");
        txn.commit().expect("commit");
        let mint =
            |row: &DeviceAuthRow| new_api_token_record(&admin, &row.client_name, row.scope, None).0;

        let poll = db
            .poll_device_auth(&approved.device_code_hash, now, mint)
            .expect("poll");
        assert!(matches!(poll, DevicePoll::TooManyTokens(_)), "{poll:?}");
        let stored: DeviceAuthRow = read_one(&db.db, DEVICE_AUTHS, &approved.device_code_hash)
            .expect("read")
            .expect("request");
        assert_eq!(stored.status, DeviceAuthStatus::Approved);
        assert_eq!(stored.last_poll_at, Some(now));
        let again = db
            .poll_device_auth(&approved.device_code_hash, now, mint)
            .expect("poll");
        assert!(matches!(again, DevicePoll::SlowDown), "{again:?}");
    }
}

#[cfg(test)]
mod token_index_tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::auth::new_api_token_record;

    fn open() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Db::connect(dir.path().join("tokens.redb").to_str().expect("utf8")).expect("connect");
        (dir, db)
    }

    fn indexed_hashes(db: &Db) -> Vec<String> {
        let txn = db.db.begin_read().expect("read");
        let tbl = txn.open_table(TOKEN_IDS_BY_HASH).expect("table");
        tbl.iter()
            .expect("iter")
            .map(|item| item.expect("entry").0.value().to_owned())
            .collect()
    }

    fn legacy_record(account: &AccountId, raw: &str) -> ApiTokenRecord {
        let salt = [7u8; 16];
        let mut hasher = Sha256::new();
        hasher.update(salt);
        hasher.update(raw.as_bytes());
        let (keyed, _) = new_api_token_record(account, "old", AuthScope::Work, None);
        ApiTokenRecord {
            token_hash: format!("{:x}", hasher.finalize()),
            salt: "07".repeat(16),
            last4: api_token_last4(raw),
            ..keyed
        }
    }

    fn request(code: &str, key: &str, now: i64) -> DeviceAuthRow {
        DeviceAuthRow {
            device_code_hash: key.to_string(),
            user_code: code.to_string(),
            client_name: "agent".to_string(),
            scope: AuthScope::Read,
            status: DeviceAuthStatus::Pending,
            created_at: now,
            expires_at: now + Db::DEVICE_CODE_TTL_SECONDS,
            last_poll_at: None,
            token_expires_at: None,
            account_id: None,
            client_network: None,
        }
    }

    #[test]
    fn find_api_token_should_find_a_token_by_its_hash_and_reject_a_wrong_one() {
        let (_dir, db) = open();
        let admin = db.create_user("admin").expect("admin");
        let (record, raw) = new_api_token_record(&admin, "agent", AuthScope::Read, None);
        let (other, other_raw) = new_api_token_record(&admin, "other", AuthScope::Read, None);
        db.create_api_token(&record).expect("token");
        db.create_api_token(&other).expect("token");

        let found = db.find_api_token(&raw).expect("find").expect("token");
        assert_eq!(found.id, record.id);
        assert_eq!(
            db.find_api_token(&other_raw)
                .expect("find")
                .expect("token")
                .id,
            other.id
        );
        assert!(db.find_api_token("pomb_wrong").expect("find").is_none());
        let same_tail = format!("pomb_wrong{}", record.last4);
        assert!(db.find_api_token(&same_tail).expect("find").is_none());
        assert!(indexed_hashes(&db).contains(&api_token_digest(&raw)));
    }

    #[test]
    fn find_api_token_should_keep_an_old_format_token_working_after_the_upgrade() {
        let (dir, db) = open();
        let admin = db.create_user("admin").expect("admin");
        let raw = "pomb_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
        let old = legacy_record(&admin, raw);
        let path = dir.path().join("tokens.redb");
        drop(db);
        {
            let raw_db = Database::create(&path).expect("raw open");
            let txn = raw_db.begin_write().expect("write");
            write_one(&txn, API_TOKENS, &old.id, &old).expect("row");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 21)
                .expect("stamp");
            txn.commit().expect("commit");
        }

        let db = Db::connect(path.to_str().expect("utf8")).expect("upgrade");
        assert!(indexed_hashes(&db).is_empty());
        assert!(db.find_api_token("pomb_wrongDEFG").expect("find").is_none());
        let found = db.find_api_token(raw).expect("find").expect("old token");
        assert_eq!(found.id, old.id);
        assert!(found.salt.is_empty());
        assert_eq!(indexed_hashes(&db), [api_token_digest(raw)]);
        assert!(
            mmap_get(&db.db, SALTED_TOKENS_BY_LAST4, "DEFG")
                .expect("index")
                .is_empty()
        );
        let again = db.find_api_token(raw).expect("find").expect("keyed token");
        assert_eq!(again.id, old.id);
        assert_eq!(again.scope, AuthScope::Work);
        assert!(db.find_api_token("pomb_wrongDEFG").expect("find").is_none());
    }

    #[test]
    fn is_suspended_token_should_find_a_suspended_token_by_its_hash() {
        let (_dir, db) = open();
        db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let (record, raw) = new_api_token_record(&member, "agent", AuthScope::Read, None);
        db.create_api_token(&record).expect("token");
        assert!(!db.is_suspended_token(&raw).expect("read"));

        db.apply_account_action(member.as_str(), AccountAction::Suspend)
            .expect("suspend");
        assert!(db.is_suspended_token(&raw).expect("read"));
        assert!(!db.is_suspended_token("pomb_wrong").expect("read"));
        assert!(db.find_api_token(&raw).expect("find").is_none());
    }

    #[test]
    fn unsuspending_should_move_a_suspended_token_back_under_the_same_index_entry() {
        let (_dir, db) = open();
        db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let (record, raw) = new_api_token_record(&member, "agent", AuthScope::Read, None);
        db.create_api_token(&record).expect("token");

        db.apply_account_action(member.as_str(), AccountAction::Suspend)
            .expect("suspend");
        assert!(db.find_api_token(&raw).expect("find").is_none());
        assert_eq!(indexed_hashes(&db), [api_token_digest(&raw)]);

        db.apply_account_action(member.as_str(), AccountAction::Unsuspend)
            .expect("unsuspend");
        let found = db
            .find_api_token(&raw)
            .expect("find")
            .expect("token works again");
        assert_eq!(found.id, record.id);
        assert!(!db.is_suspended_token(&raw).expect("read"));
        assert_eq!(indexed_hashes(&db), [api_token_digest(&raw)]);
        assert_eq!(db.list_account_api_tokens(&member).expect("list").len(), 1);
    }

    #[test]
    fn is_suspended_token_should_rekey_a_suspended_old_format_token() {
        let (dir, db) = open();
        db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let raw = "pomb_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
        let old = legacy_record(&member, raw);
        let path = dir.path().join("tokens.redb");
        drop(db);
        {
            let raw_db = Database::create(&path).expect("raw open");
            let txn = raw_db.begin_write().expect("write");
            write_one(&txn, SUSPENDED_TOKENS, &old.id, &old).expect("row");
            mmap_insert(&txn, TOKENS_BY_ACCOUNT, &old.account_id, &old.id).expect("account");
            mmap_insert(&txn, SALTED_TOKENS_BY_LAST4, &old.last4, &old.id).expect("last4");
            txn.commit().expect("commit");
        }

        let db = Db::connect(path.to_str().expect("utf8")).expect("reopen");
        assert!(db.is_suspended_token(raw).expect("read"));
        assert!(
            mmap_get(&db.db, SALTED_TOKENS_BY_LAST4, "DEFG")
                .expect("index")
                .is_empty()
        );
        assert_eq!(indexed_hashes(&db), [api_token_digest(raw)]);
        assert!(db.is_suspended_token(raw).expect("read"));
        assert!(db.find_api_token(raw).expect("find").is_none());
    }

    #[test]
    fn list_account_api_tokens_should_leave_out_the_tokens_of_other_accounts() {
        let (_dir, db) = open();
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let (mine, _) = new_api_token_record(&admin, "mine", AuthScope::Read, None);
        let (theirs, _) = new_api_token_record(&member, "theirs", AuthScope::Read, None);
        db.create_api_token(&mine).expect("token");
        db.create_api_token(&theirs).expect("token");

        let listed = db.list_account_api_tokens(&admin).expect("list");
        assert_eq!(
            listed
                .iter()
                .map(|token| token.id.as_str())
                .collect::<Vec<_>>(),
            [mine.id.as_str()]
        );
        let listed = db.list_account_api_tokens(&member).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, theirs.id);
    }

    #[test]
    fn device_user_codes_should_be_looked_up_by_key() {
        let (_dir, db) = open();
        let admin = db.create_user("admin").expect("admin");
        let now = Utc::now().timestamp();
        for (code, key) in [("BCDFGHJK", "first"), ("LMNPQRST", "second")] {
            let created = db.create_device_auth(&request(code, key, now), now);
            assert!(matches!(created, Ok(DeviceAuthCreateOutcome::Created)));
        }
        assert_eq!(
            mmap_get(&db.db, DEVICES_BY_USER_CODE, "LMNPQRST").expect("index"),
            ["second"]
        );

        let pending = db.pending_device_auth("LMNPQRST", now).expect("find");
        assert_eq!(pending.expect("request").device_code_hash, "second");
        assert!(
            db.pending_device_auth("ZZZZZZZZ", now)
                .expect("find")
                .is_none()
        );
        assert!(db.device_code_known("BCDFGHJK", now).expect("known"));
        assert!(!db.device_code_known("ZZZZZZZZ", now).expect("known"));
        let expired = now + Db::DEVICE_CODE_TTL_SECONDS + 1;
        assert!(!db.device_code_known("BCDFGHJK", expired).expect("known"));
        assert!(
            db.pending_device_auth("BCDFGHJK", expired)
                .expect("find")
                .is_none()
        );

        let grant = DeviceGrant {
            scope: AuthScope::Read,
            expires_at: None,
        };
        assert!(
            db.decide_device_auth(&admin, "BCDFGHJK", Some(&grant), now)
                .expect("approve")
        );
        assert!(
            db.pending_device_auth("BCDFGHJK", now)
                .expect("find")
                .is_none()
        );
        assert!(db.device_code_known("BCDFGHJK", now).expect("known"));
        assert!(
            !db.decide_device_auth(&admin, "ZZZZZZZZ", None, now)
                .expect("decide")
        );
    }

    #[test]
    fn create_device_auth_should_drop_the_user_code_entry_of_a_stale_request() {
        let (_dir, db) = open();
        let now = Utc::now().timestamp();
        let created = db.create_device_auth(&request("BCDFGHJK", "old", now), now);
        assert!(matches!(created, Ok(DeviceAuthCreateOutcome::Created)));

        let later = now + Db::DEVICE_CODE_TTL_SECONDS + Db::DEVICE_AUTH_RETENTION_SECONDS + 1;
        let created = db.create_device_auth(&request("LMNPQRST", "new", later), later);
        assert!(matches!(created, Ok(DeviceAuthCreateOutcome::Created)));
        assert!(
            mmap_get(&db.db, DEVICES_BY_USER_CODE, "BCDFGHJK")
                .expect("index")
                .is_empty()
        );
        assert_eq!(
            mmap_get(&db.db, DEVICES_BY_USER_CODE, "LMNPQRST").expect("index"),
            ["new"]
        );
    }

    #[test]
    fn find_api_token_should_not_find_a_revoked_token() {
        let (_dir, db) = open();
        let admin = db.create_user("admin").expect("admin");
        let (record, raw) = new_api_token_record(&admin, "agent", AuthScope::Read, None);
        db.create_api_token(&record).expect("token");
        assert!(db.find_api_token(&raw).expect("find").is_some());

        assert!(db.delete_api_token(&admin, &record.id).expect("delete"));
        assert!(db.find_api_token(&raw).expect("find").is_none());
        assert!(indexed_hashes(&db).is_empty());
    }
}

#[cfg(test)]
mod storage_tests {
    use std::fs;

    use super::*;
    use crate::models::{BreakActivity, DiskLimits};

    #[test]
    fn reopening_should_shrink_a_file_over_the_limit_once_its_data_is_deleted() {
        const LIMIT_MIB: u32 = DiskLimits::MIN_DATABASE_MIB;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("compact.redb");
        let path = path.to_str().expect("utf8");
        let file_mib = || fs::metadata(path).expect("file").len() / MIB;
        let column = {
            let db = Db::connect(path).expect("db");
            db.create_user("admin").expect("admin");
            let member = db.create_user("bob").expect("member");
            let board = db.create_board(&member, "Bob").expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let task = db.create_task(&column, None, "Big", 1, None).expect("task");
            let kept = db
                .create_task(&column, None, "Kept", 1, None)
                .expect("task");
            let attach = |task: &str, bytes: usize| {
                db.add_attachment(
                    task,
                    "a.bin",
                    "application/octet-stream",
                    &vec![7; bytes],
                    Actor::SYSTEM,
                )
                .expect("attach")
                .expect("task");
            };
            for _ in 0..5 {
                attach(&task, 4 * MIB as usize);
            }
            attach(&kept, 1024);
            assert!(db.delete_task(&task).expect("delete"));
            assert!(file_mib() >= u64::from(LIMIT_MIB), "{} MiB", file_mib());
            column
        };

        let db = Db::connect(path).expect("reopen");
        assert!(file_mib() < u64::from(LIMIT_MIB), "{} MiB", file_mib());
        let guard = DiskGuard::beside(&db);
        guard.apply(&DiskLimits {
            max_database_mib: Some(LIMIT_MIB),
            min_free_mib: DiskLimits::FLOOR_OFF_MIB,
        });
        assert_eq!(guard.full(InstanceMode::Multi), None);
        db.create_task(&column, None, "After", 1, None)
            .expect("writes go on");
    }

    #[test]
    fn timer_stop_reason_should_be_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("reason.redb").to_str().expect("utf8")).expect("db");
        let account = db.create_user("admin").expect("admin");
        let reason = "r".repeat(1025);
        let entry = NewTimeEntry {
            task_id: None,
            started_at: "2020-01-02T10:00:00+00:00",
            minutes: 1,
            seconds: 60,
            note: "",
            kind: EntryKind::Pomodoro,
            interrupted: true,
            interrupt_reason: Some(&reason),
            labels: &[],
            created_by: "admin",
            activity_id: None,
        };
        let refused = db.create_entry_full(&account, entry).expect_err("too long");
        assert_eq!(refused.to_string(), "reason must be at most 1024 bytes");
    }

    #[test]
    fn update_settings_should_keep_stored_text_past_a_cap_and_refuse_new_text_past_it() {
        const OLD_DESCRIPTION_CHARS: usize = 300;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("settings.redb").to_str().expect("utf8")).expect("db");
        let account = db.create_user("admin").expect("admin");
        let activity = BreakActivity {
            id: "act-old".to_string(),
            name: "Walk".to_string(),
            description: "d".repeat(OLD_DESCRIPTION_CHARS),
            daily_goal: 1,
            daily_limit: None,
        };
        let stored = Settings {
            break_activities: vec![activity.clone()],
            ..Settings::default()
        };
        let txn = db.db.begin_write().expect("write");
        write_one(&txn, ACCOUNT_SETTINGS, account.as_str(), &stored).expect("settings");
        txn.commit().expect("commit");

        let mut edited = db.settings(&account).expect("settings");
        edited.alarm_volume = 10;
        db.update_settings(&account, &edited)
            .expect("an unrelated save goes through");

        let mut grown = edited.clone();
        grown.break_activities.push(BreakActivity {
            id: "act-new".to_string(),
            ..activity
        });
        let refused = db
            .update_settings(&account, &grown)
            .expect_err("new text past the cap");
        assert_eq!(
            refused.to_string(),
            "break_activities.description must be at most 200 characters"
        );
    }
}
