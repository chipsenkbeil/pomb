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
mod agents;
mod avatar;
mod bin;
mod dispatch;
mod entries;
mod events;
mod files;
mod import;
mod inbound;
mod instance;
mod limit_groups;
mod membership;
mod notifications;
mod outbox;
mod prune;
mod public;
mod sharing;
mod snapshot;
mod store;
#[cfg(test)]
mod support;
mod usage;
mod webhooks;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fs::File;
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
    ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle, UntypedTableHandle,
    WriteTransaction,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use uuid::Uuid;
use webauthn_rs::prelude::Passkey;

pub use self::account::{
    AccountDb, BoardDenied, ChildRef, LinkReads, OwnedBoard, OwnedColumn, OwnedTask, RoleDenied,
    UmbrellaPart, UmbrellaRef,
};
use self::activity::Activity;
pub use self::agents::{AgentRoleNameTaken, StandardRoles};
use self::agents::{txn_remove_grants_of_board, txn_seed_standard_roles};
use self::bin::txn_remove_board_bin;
pub use self::bin::{PurgeOutcome, RestoreOutcome};
pub use self::dispatch::Delivery;
use self::entries::txn_reindex_entries;
use self::events::txn_remove_events;
pub use self::events::{EventRead, MAX_EVENTS_PER_BOARD};
pub use self::files::{AttachmentFiles, Incoming};
pub use self::import::{BoardImport, FileImport, TaskImport};
pub use self::inbound::{InboundColumnMissing, IssuedInbound, MAX_PREFIX_CHARS, NewInbound};
use self::inbound::{txn_drop_inbound, txn_remove_inbound};
pub use self::instance::InstanceDb;
pub use self::limit_groups::{ColumnMove, GroupWrite};
use self::limit_groups::{
    txn_group_over_limit, txn_leave_limit_groups, txn_order_splits_group, txn_sort_groups,
};
pub use self::membership::{
    AccountAction, AccountDeleteOutcome, AccountSummary, AdminActionOutcome, ModeSwitch,
    NewAccount, RecoverOutcome, RegisterOutcome, Revoked,
};
pub use self::notifications::{Change, NotificationPage};
use self::outbox::txn_remove_webhook_outbox;
pub use self::public::PublicDb;
pub use self::sharing::{BoardMember, MemberSet, OwnerChange};
use self::sharing::{
    role_in, task_role_in, txn_board_accounts, txn_refuse_timer_below_member,
    txn_remove_members_of_board, txn_remove_memberships_of,
};
pub use self::snapshot::{BoardSnapshot, OfflineDb};
pub use self::store::{Store, WriteTxn};
pub use self::usage::LargestUse;
use self::usage::{
    OverQuota, mutate_owned, read_refuse_over_bytes, refuse_row_past, txn_column_owner,
    txn_count_usage, txn_recount_usage, txn_refuse_over_bytes, txn_remove_entry, txn_remove_owned,
    txn_rows_bytes, txn_store_entry, txn_store_owned, txn_store_task, txn_task_bytes,
};
pub use self::webhooks::{NewWebhook, WebhookColumnMissing, WebhookPatch};
use crate::api_error::{ErrorCode, coded_response};
use crate::auth::{
    REAUTH_WINDOW_SECS, SESSION_MAX_AGE_SECONDS, SignIn, api_token_digest, api_token_last4,
    constant_time_eq, hash_feed_token, hash_session_token, verify_api_token,
};
use crate::backup::Backups;
use crate::disk::{DiskGuard, MIB};
use crate::labels::{CONFIG_KEY, LabelSettings};
use crate::layouts::{LAYOUTS, Layout};
use crate::limit::{Limits, TrustProxy};
use crate::models::{
    AccountId, AccountRole, AccountStatus, ActiveTimer, Actor, ActorKind, AgentRoleRow,
    ApiTokenRecord, AttachmentVersion, AuthScope, BLOCKED_COLUMN_KEY, BLOCKED_KIND, BackupSettings,
    BackupStatus, BinRow, BoardAccess, BoardFeed, BoardRole, BoardRow, BoardTemplateRow, ColorRow,
    ColumnRow, ColumnStage, CommentQuestion, CommentReaction, CoverChange, DeviceAuthRow,
    DeviceAuthStatus, EntryKind, EscalationTier, EventChange, FieldDefRow, HistoryKind,
    InboundAddressRow, InstanceMode, InstanceSettings, LaneLimit, LinkKind, MAX_BOARD_FEEDS,
    MAX_CUSTOM_COLORS, MAX_NUMBERING_NAME_CHARS, MAX_NUMBERING_NUMBER, MAX_NUMBERING_PREFIX_CHARS,
    MAX_REPORT_BOOKMARKS, MIN_TEXT_CONTRAST, NEEDS_HUMAN_LABEL, NUMBERING_KEY, Numbering, Owner,
    OwnerHolder, PasskeyRow, QUESTION_KIND, QuestionAnswer, Quota, Quotas, RecoveryCodeRow,
    ReportBookmark, Responsible, SECONDS_PER_HOUR, Seat, SeatSlot, SessionRow, Settings, Subtask,
    SwimlaneRow, TaskAttachment, TaskClaim, TaskComment, TaskEvent, TaskHead, TaskKind, TaskLink,
    TaskRow, TextCap, TimeEntryRow, UnnamedFiles, UserRow, WebhookRow, custom_color_hexes,
    custom_color_value, format_minutes, is_custom_color, lighter_hex, normalize_hex,
    standard_color, text_contrast,
};
use crate::permissions::{Permission, STANDARD_ROLES};
use crate::rules;
use crate::webhook::{Prober, is_public_address};

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
    /// Refused: a custom color can be disabled but not deleted.
    RefusedCustom,
}

/// Typed outcome for [`Db::delete_template`]: built-in templates are
/// protected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TemplateDeleteOutcome {
    Deleted,
    NotFound,
    RefusedBuiltIn,
}

/// What a board copy takes of the source board's tasks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CopyTasks {
    /// No tasks: the copy starts empty.
    None,
    /// The first [`BOARD_COPY_TASK_CAP`] tasks, with their members and responsible actor.
    Keep,
    /// The first [`BOARD_COPY_TASK_CAP`] tasks, with no members and no responsible actor.
    ClearMembers,
}

/// Most tasks a board copy takes from its source.
pub const BOARD_COPY_TASK_CAP: usize = 200;

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
    /// What the task is.
    #[serde(default)]
    kind: TaskKind,
    /// Whether someone closed the epic.
    #[serde(default)]
    closed: bool,
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
    /// Refused: the two tasks already have a relation of another type.
    Duplicate,
    /// Refused: the child already has an umbrella.
    HasParent,
    /// Refused: the umbrella sits under the child, so the link would close a parent cycle.
    ParentCycle,
    /// Refused: the umbrella is not an epic.
    NotEpic,
}

/// The link changes [`Db::change_links_logged`] makes on each task.
#[derive(Copy, Clone, Debug)]
pub struct LinkEdit<'a> {
    /// The relations to remove, each as the kind the task shows and the other task.
    pub remove: &'a [(LinkKind, String)],
    /// The relations to add, each as the kind the task shows and the other task.
    pub add: &'a [(LinkKind, String)],
    /// The API token an added link records, or none for the browser.
    pub token_id: Option<&'a str>,
    /// The API token a removal is limited to, or none to remove any link.
    pub owner: Option<&'a str>,
}

/// Why [`Db::change_links_logged`] refused a change.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LinkRefusal {
    /// The `blocked_by` chain would loop back to the task.
    Cycle,
    /// The two tasks already have a relation of another type.
    Duplicate,
    /// The child already has an umbrella.
    HasParent,
    /// The umbrella sits under the child.
    ParentCycle,
    /// The umbrella is not an epic.
    NotEpic,
    /// A link to remove was not created by the API token the removal is limited to.
    NotOwner,
}

/// Typed outcome for [`Db::delete_task_link`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LinkDeleteOutcome {
    Deleted,
    /// The task or the link does not exist.
    NotFound,
    /// Refused: the link was not created by the API token the delete is limited to.
    NotOwner,
}

/// Returns whether `link` may go for `owner`: always without one, and with one only when that
/// API token created it.
fn link_is_by(link: &TaskLink, owner: Option<&str>) -> bool {
    owner.is_none_or(|id| !id.is_empty() && link.token_id.as_deref() == Some(id))
}

/// Typed outcome for moving a task under another umbrella.
#[derive(Clone, Debug)]
pub enum UmbrellaChange {
    /// The task's `parent` link now names the target, or already did.
    Changed(TaskLink),
    NotFound,
    /// Refused: the task has no umbrella to replace.
    NoUmbrella,
    /// Refused: the new umbrella sits under the task, so the change would close a parent cycle.
    ParentCycle,
    /// Refused: the new umbrella is not an epic.
    NotEpic,
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
    /// The id of the token that claims, which the owner records.
    pub token_id: Option<String>,
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
    /// Refused: another holder's live owner lease holds the task.
    OwnerHeld(Owner),
    /// Refused: the fence does not match a live claim.
    FenceMismatch,
}

/// Fields of an owner grab.
#[derive(Clone, Debug)]
pub struct GrabRequest {
    /// Who grabs the task.
    pub holder: OwnerHolder,
    /// Lease length in seconds, or none for a lease that never ends.
    pub ttl_seconds: Option<i64>,
    /// Whether a live lease of another holder is replaced.
    pub take_over: bool,
}

/// Typed outcome for the owner operations on [`Db`].
#[derive(Clone, Debug)]
pub enum OwnerOutcome {
    /// The owner as stored after the call, with the fence of its grab.
    Granted(Owner, u64),
    /// The lease ended, or had already ended.
    Released,
    /// The task does not exist.
    NotFound,
    /// Refused: another holder's live lease holds the task.
    Held(Owner),
    /// Refused: the fence is not the task's latest, or the lease lapsed.
    StaleFence,
}

/// Typed outcome of [`Db::set_epic_closed`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EpicCloseOutcome {
    /// The flag changed and the history event was written.
    Changed,
    /// The epic already had the requested state, so nothing was written.
    Unchanged,
    /// The task is not an epic.
    NotEpic,
    /// The task does not exist.
    NotFound,
}

/// The new values of an edited time entry. See [`Db::update_entry`].
#[derive(Copy, Clone, Debug)]
pub struct EntryEdit<'a> {
    /// The new length in seconds.
    pub seconds: i64,
    /// Free-text note.
    pub note: &'a str,
    /// RFC3339 start of the session.
    pub started_at: &'a str,
    /// Labels of the entry.
    pub labels: &'a [String],
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
    /// Role the author logged the time in.
    pub role: Option<&'a str>,
    /// Name of the API token that logged the entry.
    pub token_name: Option<&'a str>,
    /// Id of the agent role of the token that logged the entry.
    pub role_id: Option<&'a str>,
    /// The role's name when the token was issued.
    pub role_name: Option<&'a str>,
}

impl NewTimeEntry<'_> {
    /// Returns the stored row of this entry.
    fn into_row(self, id: String, task_id: &str, account_id: String) -> TimeEntryRow {
        TimeEntryRow {
            id,
            task_id: task_id.to_string(),
            minutes: self.minutes,
            seconds: self.seconds,
            note: self.note.to_string(),
            started_at: self.started_at.to_string(),
            kind: self.kind.as_str().to_string(),
            interrupted: self.interrupted,
            interrupt_reason: self.interrupt_reason.map(str::to_string),
            labels: self.labels.to_vec(),
            created_by: self.created_by.to_string(),
            activity_id: self.activity_id.map(str::to_string),
            account_id,
            role: self.role.map(str::to_string),
            token_name: self.token_name.map(str::to_string),
            role_id: self.role_id.map(str::to_string),
            role_name: self.role_name.map(str::to_string),
        }
    }
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
    /// Detail of a `moved` history event to append.
    pub event: Option<&'a str>,
    /// An `If-Claim-Fence` value; a live claim holding another fence
    /// refuses the move.
    pub fence: Option<u64>,
    /// Refuse a move into another column that already holds its
    /// `wip_limit` of tasks.
    pub enforce_wip: bool,
    /// Tasks of the same batch still waiting to move, which the target cell does not count as siblings.
    pub batch: &'a [String],
    /// Whether to skip the refusal of an epic with an open child moving into a WIP-limited or
    /// Done column, and to leave the epic's closed flag alone.
    pub skip_epic_guard: bool,
}

/// What a bulk write does with its transaction once every task changed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Commit {
    /// Commit the write.
    Write,
    /// End the transaction without a commit, so the run only checks.
    Discard,
}

impl Commit {
    /// Ends `txn` the way this mode says.
    fn finish(self, txn: WriteTxn<'_>) -> DbResult<()> {
        match self {
            Self::Write => txn.commit()?,
            Self::Discard => txn.abort()?,
        }
        Ok(())
    }
}

/// Typed outcome for [`Db::apply_tasks_change`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeOutcome<E> {
    /// Every task changed and the write committed.
    Changed,
    /// The change refused a task with this error, and no task changed.
    Refused(E),
    /// A task is missing, and no task changed.
    NotFound,
}

/// Typed outcome for [`Db::move_task_logged`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved,
    NotFound,
    StaleFence,
    /// Refused: the epic holds `count` children that are not Done, and the target column has a
    /// WIP limit or is a Done column.
    OpenChildren {
        count: usize,
    },
    /// Refused: the target column holds `count` tasks against `limit`.
    WipLimit {
        count: usize,
        limit: i64,
    },
    /// Refused: the limit group of the target column holds `count` tasks against `limit`.
    GroupLimit {
        count: usize,
        limit: i64,
    },
}

/// Where a question's write moves the task.
#[derive(Copy, Clone, Debug)]
pub struct QuestionMove<'a> {
    /// Id of the destination column.
    pub column_id: &'a str,
    /// Index in the target cell.
    pub position: f64,
    /// An `If-Claim-Fence` value, where a live claim holding another fence refuses the write.
    pub fence: Option<u64>,
}

/// Typed outcome for [`Db::post_question`] and [`Db::answer_question`].
#[derive(Clone, Debug)]
pub enum QuestionWrite {
    /// Written, carrying the question comment as stored.
    Done(Box<TaskComment>),
    /// Refused: the task or the comment does not exist.
    NotFound,
    /// Refused: a live claim holds another fence.
    StaleFence,
    /// Refused: the epic holds `count` children that are not Done, and the target column has a
    /// WIP limit or is a Done column.
    OpenChildren {
        /// Children that are not Done.
        count: usize,
    },
    /// Refused: the target column holds `count` tasks against `limit`.
    WipLimit {
        /// Tasks the column holds.
        count: usize,
        /// The column's limit.
        limit: i64,
    },
    /// Refused: the limit group of the target column holds `count` tasks against `limit`.
    GroupLimit {
        /// Tasks the group holds.
        count: usize,
        /// The group's limit.
        limit: i64,
    },
    /// Refused: the comment is not a question.
    NoQuestion,
    /// Refused: the question has its answer already.
    AlreadyAnswered,
    /// Refused: the question has no choice with that number.
    BadChoice,
}

impl QuestionWrite {
    /// Commits `txn` and returns [`QuestionWrite::Done`] with `comment` when the move happened,
    /// and drops `txn` with the move's refusal otherwise.
    fn commit_if_moved(
        txn: WriteTxn<'_>,
        moved: MoveOutcome,
        comment: TaskComment,
    ) -> DbResult<Self> {
        let refusal = match moved {
            MoveOutcome::Moved => {
                txn.commit()?;
                return Ok(Self::Done(Box::new(comment)));
            }
            MoveOutcome::NotFound => Self::NotFound,
            MoveOutcome::StaleFence => Self::StaleFence,
            MoveOutcome::OpenChildren { count } => Self::OpenChildren { count },
            MoveOutcome::WipLimit { count, limit } => Self::WipLimit { count, limit },
            MoveOutcome::GroupLimit { count, limit } => Self::GroupLimit { count, limit },
        };
        Ok(refusal)
    }
}

/// Adds the [`NEEDS_HUMAN_LABEL`] label to `task` with a labels event, unless it has it.
fn add_needs_human(task: &mut TaskRow, actor: Actor<'_>) {
    if task.labels.iter().any(|label| label == NEEDS_HUMAN_LABEL) {
        return;
    }
    let before = task.labels.join(", ");
    task.labels.push(NEEDS_HUMAN_LABEL.to_string());
    let change = EventChange {
        from: (!before.is_empty()).then_some(before),
        to: Some(task.labels.join(", ")),
    };
    let detail = format!("Set labels: {}", task.labels.join(", "));
    push_change_event(task, HistoryKind::LabelsUpdated, detail, actor, change);
}

/// Removes the [`NEEDS_HUMAN_LABEL`] label from `task` with a labels event, if it has it.
fn remove_needs_human(task: &mut TaskRow, actor: Actor<'_>) {
    if !task.labels.iter().any(|label| label == NEEDS_HUMAN_LABEL) {
        return;
    }
    let before = task.labels.join(", ");
    task.labels.retain(|label| label != NEEDS_HUMAN_LABEL);
    let detail = if task.labels.is_empty() {
        "Cleared the labels".to_string()
    } else {
        format!("Set labels: {}", task.labels.join(", "))
    };
    let change = EventChange {
        from: Some(before),
        to: (!task.labels.is_empty()).then(|| task.labels.join(", ")),
    };
    push_change_event(task, HistoryKind::LabelsUpdated, detail, actor, change);
}

/// Adds the label of `tier` to `task` and removes the label of the other tier, with one labels
/// event, unless the labels already read that way.
fn set_tier_label(task: &mut TaskRow, tier: EscalationTier, actor: Actor<'_>) {
    let before = task.labels.join(", ");
    let dropped = tier.other().label();
    task.labels.retain(|label| label != dropped);
    if !task.labels.iter().any(|label| label == tier.label()) {
        task.labels.push(tier.label().to_string());
    }
    let after = task.labels.join(", ");
    if after == before {
        return;
    }
    let change = EventChange {
        from: (!before.is_empty()).then_some(before),
        to: Some(after.clone()),
    };
    let detail = format!("Set labels: {after}");
    push_change_event(task, HistoryKind::LabelsUpdated, detail, actor, change);
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

/// What the owner grants when approving a device request: permissions never above the requested
/// ones, the boards the token reaches and when it expires.
#[derive(Clone, Debug)]
pub struct DeviceGrant {
    /// The scope that labels the granted permissions.
    pub scope: AuthScope,
    /// The permissions the token gets.
    pub permissions: BTreeSet<Permission>,
    /// The boards the token reaches.
    pub boards: BoardAccess,
    /// Unix seconds, or [`None`] for a token that never expires.
    pub expires_at: Option<i64>,
    /// The agent role the token is issued as, or [`None`] for a token issued as no role.
    pub role_id: Option<String>,
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
/// Task id -> JSON [`BinRow`], a deleted task whole with its time entries.
const BIN: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("recycle_bin");
const TIME_ENTRIES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("time_entries");
/// Single-row table (`"app"` -> JSON [`Settings`]).
const SETTINGS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("settings");
/// The uploaded avatar image of each account, by account id.
const AVATARS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("avatars");
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
/// Attachment bytes keyed by id, in a table that only versions before 32 write.
const LEGACY_ATTACHMENT_DATA: TableDefinition<'_, &str, &[u8]> =
    TableDefinition::new("attachment_data");
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
/// Notifications keyed by account id and sequence number, each a JSON
/// [`crate::models::NotificationRow`]. The account's oldest go past
/// [`notifications::MAX_NOTIFICATIONS_PER_ACCOUNT`], and all of them go with the account.
const NOTIFICATIONS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("notifications");
/// Schema stamp table; never rename it, or older binaries read the file as unstamped.
const META: TableDefinition<'_, &str, u64> = TableDefinition::new("meta");
/// Key of the stamp in [`META`]; never rename it, or older binaries read the file as unstamped.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// The settings key that marks the pass filling Responsible from the first member as done.
const RESPONSIBLE_FILL_DONE: &str = "responsible_fill_done";

/// Schema version this binary reads and writes. Bump it whenever a stored
/// struct or table gains a field or table, so older binaries refuse the
/// newer file instead of dropping the new data on their next save.
pub const SCHEMA_VERSION: u64 = 83;

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
    "2c24806bc2804c2a70523b32e6ce0184e2a00fd07609d1bfbbfd3ca19e23d54b",
    "d24ee0a3a5270e2bd0d35bc122b5cc8410ed3258da47375ee03509798292c92d",
    "8c8f6d4640a2561d2736d02e1bb11eb9f17afac94d732994c71a75b041f759e3",
    "27c6bb5bb24a9ab1990cee91cb195d87697407cc38d4432ec5bc591d732fa132",
    "ff42dc77168394e594bbb56be3a8b9f0866d45f96cc2df534aa2cc1a769bcaf2",
    "2406b717f11428346211c06fcb75193ebfe974ff849599a998d151a0c0a5571f",
    "6df26d15958012140f6aef71d712de0f708da23645f4f8c885b1c41e24002a5a",
    "6a95a4a237d206cccbd679c9ad4de55d4cb487f54196f25ebf09024f0c294931",
    "e7ed3099870242a84d93bbf8c643a80555aebc83b9ad4c1940f24086f3c75dcc",
    "b7f0e74fd81f7a0ab874d5612b13f96e6b44a869fcd97fac49fc1be0d7762569",
    "e365c169a7fff1afa3ef1a9912ded12fcfcb4b640a4766ef8b680fa24deb0e64",
    "009dcd092ac83416d6c1650cae5ea35befb7f8dadcc5661b4599653fc2b55f88",
    "170895c26321c6d9805d8292d04fed137c914e9041294693a70137d92ed0cd7f",
    "f75a9c9915d6d30c0b9c993ac692a4fda73791567e2f9aa5e6141148e0f0bf58",
    "4e1f16f2b36fd538dafa78b67abdd1d5c12604fe05197c96ab704aa012546e60",
    "84022fd6ae049ea9d06a5553e35138162da957d9f3bd8d9eced680b2b4fe8a43",
    "22060e6823907660475897626e746d3db173b03a1ae289189d0bac80dba7f518",
    "9d9039c7758077a04f2148449d03a240efc9e1e3d633d36616162e75d5bba321",
    "0b5be92bc9fe59e49d4d1ca853b70d0d0202facc703ccd2c1320cbabc8697ad8",
    "2ce540a842bd9249b7b1db25e17f46db4c5a5105727824f30ac420364e872b75",
    "757ce6aa7b7c347045a6218e97b4cbd8fd5249c56ddcd3c4cd1944eb33aead37",
    "8cba50eda37aff485e68c7212080d4b38b49bea36208334927f3fd9af718c036",
    "4e1a40189f58839e0e92f8a51bc0115a07240857c74b8bb5d9dffef5af426f49",
    "e2ad6d69293caa30bf33daed53023dc102e27126a581e713d256da0250ebdc22",
    "730d168836090d108d212985fb215b2a4b2db3e52aced650b3c6ff8914565d19",
    "9de708db88e49ffbb1da2c7b25f39f73c69f5c1401dd7604e73699f6b03c87d3",
    "2cb1661e822c7d059b59e6018ad072dee1e5b03236574714b1aa1bbdf7289133",
    "41ef38f21ae22ac033cebb5bdefa2240fd783b10c642a8d1187bcb53962d7937",
    "2e95a79f7700532bed2bf98ad082756690715b2aa950c21bf59c0e1f3520b8cd",
    "dfd39048866caf54730ca8c1bd13904d404f57314bd3cd4e4ac5d764a59bf2b1",
    "26879f4069c114a3dac70192954b9e787d42d63b504f1e1acdca47e1839868cf",
    "e7a53b9d817be858118e1204c0350d7ec3c1de04c6e251227c9f289704cdc4b2",
    "3e6c4600a02a4c61aa6e04a9f2eff3501e8ce6fe084b3560b780cfc87b5af6d7",
    "5a3e907a0bb7f4d27fe1fb58d297b01392e3ce684e5990b93072a616be4a4f97",
    "b7b02fe0140b962c9f01b8e8ee5638102ca0d36d573db17dd75638df5df87e71",
    "4bed9374b31b1c8fcac7f409b776c34eaf93b6757f6795c077ee05570d615f60",
    "36a870e0a7b7c046a30ee5a9b470d7b8bb0817173e2368a62d80711757e92abb",
    "e0b911ec0186f26f2948e7fa29bcadb8dc26cc3f5e190321ca1321f2c5947669",
    "043aa18d87c1fbed7a5e8a135ced106f018c97063e0c551c5aaf1b6b2036a922",
    "aaaf5ff2b9576404c34b111a8ae33bff2eba289b4f2734c014c791f71cfffe36",
    "4f54b1b8694a065153cb08819ff7a974a9090b5974807113d37f1e188d06255d",
    "bcb728397c770bad9fed1da00e67973b338a43e78744ffd93faf21828277eb5e",
    "a3b189a2e52ffed93957495b282579a2df966d0f356324742c99512c4c841158",
    "a3b189a2e52ffed93957495b282579a2df966d0f356324742c99512c4c841158",
    "a3b189a2e52ffed93957495b282579a2df966d0f356324742c99512c4c841158",
    "a3b189a2e52ffed93957495b282579a2df966d0f356324742c99512c4c841158",
    "a32b88150a45da953d15152e7fda8261d01b3d6acdde4fc865ea6e3f6dd7ee74",
    "a32b88150a45da953d15152e7fda8261d01b3d6acdde4fc865ea6e3f6dd7ee74",
    "2f1061ac24004e1c00ef04eff0c1d8fecf32cfbd9f9cd51c098e8a9f4b3d7c1d",
    "2f1061ac24004e1c00ef04eff0c1d8fecf32cfbd9f9cd51c098e8a9f4b3d7c1d",
    "332b9168cd35300e66a5e0a6bed3e8b755406698a9995661fcceaf615e746428",
    "2ede0a454c015ba8cb29d4d975460326645220f6c4751abfa01773ea7d97af62",
    "79781e79136dc13b6cbe973f282c2f0b95c0406748922d77a924bb29f7a41770",
    "e1da24113ea1128afeaace95b5d5cd2e4876bd2b5d04f387dd1119807f4330e5",
    "ddef6d5beb8718c6a99f0f42a1c37ba81b4eec45ec86ffd797cccfb89066835c",
    "8f0a76c629391994f2e71f44847877943e826288031ba9748875ef75810bba01",
    "735ee709d084a3663625557b7e7ba31e2bff2c9e72847f21a133ac4071b36fe8",
    "00c82eb31b386bf84813988f8daf4462e6d9ad3d8ded1250c8ea0a6b801c613b",
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
    if from.is_none_or(|version| version < 27) {
        grant_scope_permissions(txn)?;
    }
    if from.is_none_or(|version| version < 62) {
        split_write_permissions(txn)?;
    }
    let mut added = Vec::new();
    if from.is_none_or(|version| version < 41) {
        added.extend([
            Permission::TasksBin,
            Permission::LabelsManage,
            Permission::EpicsManage,
            Permission::InboundManage,
        ]);
    } else if from.is_some_and(|version| version < 51) {
        added.extend([
            Permission::LabelsManage,
            Permission::EpicsManage,
            Permission::InboundManage,
        ]);
    } else if from.is_some_and(|version| version < 58) {
        added.extend([Permission::EpicsManage, Permission::InboundManage]);
    } else if from.is_some_and(|version| version < 59) {
        added.push(Permission::InboundManage);
    }
    if from.is_none_or(|version| version < 70) {
        added.push(Permission::SubtasksDelete);
    }
    if from.is_none_or(|version| version < 78) {
        added.push(Permission::SeatsManage);
    }
    if from.is_none_or(|version| version < 80) {
        added.push(Permission::SeatsTake);
    }
    if !added.is_empty() {
        grant_added_permissions(txn, &added)?;
    }
    if from.is_none_or(|version| version < 58) {
        mark_parents_as_epics(txn)?;
        txn_recount_usage(txn)?;
    }
    if from.is_none_or(|version| version < 67) {
        txn_reindex_role_tokens(txn)?;
    }
    if from.is_none_or(|version| version < 69) {
        grant_standard_roles_boards_read(txn)?;
    }
    if from.is_none_or(|version| version < 71) {
        fit_lead_role(txn)?;
    }
    if from.is_none_or(|version| version < 73) {
        widen_lead_role(txn)?;
    }
    if from.is_none_or(|version| version < 74) {
        assign_task_owners(txn)?;
    }
    if from.is_none_or(|version| version < 75) {
        seed_standard_roles(txn)?;
    }
    if from.is_none_or(|version| version < 78) {
        grant_lead_role_seats_manage(txn)?;
    }
    if from.is_none_or(|version| version < 79) {
        assign_column_stages(txn)?;
        txn_recount_usage(txn)?;
    }
    if from.is_none_or(|version| version < 80) {
        grant_standard_roles_seats_take(txn)?;
    }
    if from.is_none_or(|version| version < 81) {
        assign_role_keys(txn)?;
    }
    Ok(())
}

/// Sets each stored role's standard key from its current name, where the name is a standard
/// role's.
fn assign_role_keys(txn: &WriteTransaction) -> DbResult<()> {
    rewrite_rows(txn, AGENT_ROLES, |_, role| {
        let key = role
            .get("name")
            .and_then(serde_json::Value::as_str)
            .and_then(|name| STANDARD_ROLES.iter().find(|standard| standard.name == name))
            .map(|standard| standard.key);
        let Some(key) = key else {
            return false;
        };
        role.insert("standard".to_string(), serde_json::json!(key));
        true
    })
}

/// Sets each stored column's stage from its current name.
fn assign_column_stages(txn: &WriteTransaction) -> DbResult<()> {
    rewrite_rows(txn, COLUMNS, |_, column| {
        let stage = column
            .get("name")
            .and_then(serde_json::Value::as_str)
            .and_then(ColumnStage::from_legacy_name);
        let Some(stage) = stage else {
            return false;
        };
        column.insert("stage".to_string(), serde_json::json!(stage));
        true
    })
}

/// Adds `seats.manage` to each stored role named `lead` whose permissions are exactly the lead
/// standard role without it and without `seats.take`, and leaves any other lead role as the owner
/// set it.
fn grant_lead_role_seats_manage(txn: &WriteTransaction) -> DbResult<()> {
    let added = Permission::SeatsManage.name();
    let old: BTreeSet<&str> = STANDARD_ROLES
        .iter()
        .filter(|standard| standard.name == "lead")
        .flat_map(|standard| standard.permissions())
        .map(Permission::name)
        .filter(|name| *name != added && *name != Permission::SeatsTake.name())
        .collect();
    rewrite_rows(txn, AGENT_ROLES, |_, role| {
        if role.get("name").and_then(serde_json::Value::as_str) != Some("lead") {
            return false;
        }
        let Some(held) = role
            .get_mut("permissions")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return false;
        };
        let names: BTreeSet<&str> = held.iter().filter_map(serde_json::Value::as_str).collect();
        if names != old || names.len() != held.len() {
            return false;
        }
        held.push(serde_json::Value::from(added));
        true
    })
}

/// Adds `seats.take` to each stored role named `reviewer`, `verifier` or `lead` whose permissions
/// are exactly its standard role without it, and leaves any other role as the owner set it.
fn grant_standard_roles_seats_take(txn: &WriteTransaction) -> DbResult<()> {
    let added = Permission::SeatsTake.name();
    let old: Vec<(&str, BTreeSet<&str>)> = STANDARD_ROLES
        .iter()
        .filter(|standard| standard.extra.contains(&Permission::SeatsTake))
        .map(|standard| {
            let names = standard
                .permissions()
                .into_iter()
                .map(Permission::name)
                .filter(|name| *name != added)
                .collect();
            (standard.name, names)
        })
        .collect();
    rewrite_rows(txn, AGENT_ROLES, |_, role| {
        let Some(name) = role.get("name").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some((_, expected)) = old.iter().find(|(standard, _)| *standard == name) else {
            return false;
        };
        let Some(held) = role
            .get_mut("permissions")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return false;
        };
        let names: BTreeSet<&str> = held.iter().filter_map(serde_json::Value::as_str).collect();
        if names != *expected || names.len() != held.len() {
            return false;
        }
        held.push(serde_json::Value::from(added));
        true
    })
}

/// Gives each account that holds no agent role the standard roles.
fn seed_standard_roles(txn: &WriteTransaction) -> DbResult<()> {
    let users: Vec<UserRow> = rows_in(&txn.open_table(USERS)?)?;
    for user in users {
        txn_seed_standard_roles(txn, &user.id)?;
    }
    Ok(())
}

/// The permissions the lead standard role held in version 72.
const LEAD_ROLE_V72: [&str; 17] = [
    "boards.read",
    "tasks.read",
    "comments.read",
    "attachments.read",
    "comments.write",
    "tasks.create",
    "tasks.edit",
    "tasks.move",
    "tasks.claim",
    "tasks.bin",
    "subtasks.edit",
    "subtasks.delete",
    "links.write",
    "columns.edit",
    "settings.read",
    "tokens.issue",
    "roles.grant",
];

/// The permissions the lead standard role gained in version 73.
const LEAD_ROLE_V73_ADDED: [Permission; 4] = [
    Permission::SubtasksCheck,
    Permission::LabelsManage,
    Permission::AttachmentsWrite,
    Permission::TimeWrite,
];

/// Adds [`LEAD_ROLE_V73_ADDED`] to each stored role named `lead` whose permissions are exactly
/// [`LEAD_ROLE_V72`], and leaves any other lead role as the owner set it.
fn widen_lead_role(txn: &WriteTransaction) -> DbResult<()> {
    let old = BTreeSet::from(LEAD_ROLE_V72);
    rewrite_rows(txn, AGENT_ROLES, |_, role| {
        if role.get("name").and_then(serde_json::Value::as_str) != Some("lead") {
            return false;
        }
        let Some(held) = role
            .get_mut("permissions")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return false;
        };
        let names: BTreeSet<&str> = held.iter().filter_map(serde_json::Value::as_str).collect();
        if names != old || names.len() != held.len() {
            return false;
        }
        held.extend(
            LEAD_ROLE_V73_ADDED
                .iter()
                .map(|permission| serde_json::Value::from(permission.name())),
        );
        true
    })
}

/// Sets the stored role named `lead` to exactly the permissions of the lead standard role.
fn fit_lead_role(txn: &WriteTransaction) -> DbResult<()> {
    let fitted: Vec<serde_json::Value> = STANDARD_ROLES
        .iter()
        .filter(|standard| standard.name == "lead")
        .flat_map(|standard| standard.permissions())
        .map(|permission| serde_json::Value::from(permission.name()))
        .collect();
    rewrite_rows(txn, AGENT_ROLES, |_, role| {
        if role.get("name").and_then(serde_json::Value::as_str) != Some("lead") {
            return false;
        }
        let Some(held) = role.get_mut("permissions") else {
            return false;
        };
        let fitted = serde_json::Value::Array(fitted.clone());
        if *held == fitted {
            return false;
        }
        *held = fitted;
        true
    })
}

/// Gives every stored role that bears a standard name `boards.read`, which the base of the standard
/// roles gained.
fn grant_standard_roles_boards_read(txn: &WriteTransaction) -> DbResult<()> {
    let added = serde_json::Value::from(Permission::BoardsRead.name());
    rewrite_rows(txn, AGENT_ROLES, |_, role| {
        let standard = role
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| STANDARD_ROLES.iter().any(|standard| standard.name == name));
        let Some(held) = role
            .get_mut("permissions")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return false;
        };
        if !standard || held.contains(&added) {
            return false;
        }
        held.push(added.clone());
        true
    })
}

/// The permissions a stored `tasks.write` and `subtasks.write` split into.
const SPLIT_WRITES: [(&str, &[Permission]); 2] = [
    (
        "tasks.write",
        &[
            Permission::TasksCreate,
            Permission::TasksEdit,
            Permission::TasksMove,
            Permission::TasksClaim,
        ],
    ),
    (
        "subtasks.write",
        &[Permission::SubtasksEdit, Permission::SubtasksCheck],
    ),
];

/// Replaces `tasks.write` and `subtasks.write` in every token and device request with the
/// permissions they split into, which keeps every ability each had.
fn split_write_permissions(txn: &WriteTransaction) -> DbResult<()> {
    for table in [API_TOKENS, SUSPENDED_TOKENS, DEVICE_AUTHS] {
        rewrite_rows(txn, table, |_, row| split_writes_in(row))?;
    }
    Ok(())
}

/// Swaps each split permission in `row` for its parts, and reports whether it changed the row.
fn split_writes_in(row: &mut JsonObject) -> bool {
    let Some(held) = row
        .get_mut("permissions")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for (old, parts) in SPLIT_WRITES {
        let Some(at) = held.iter().position(|name| *name == old) else {
            continue;
        };
        held.remove(at);
        for part in parts {
            let name = serde_json::Value::from(part.name());
            if !held.contains(&name) {
                held.push(name);
            }
        }
        changed = true;
    }
    changed
}

/// Sets the kind of every task that is the target of a parent link to epic.
fn mark_parents_as_epics(txn: &WriteTransaction) -> DbResult<()> {
    let mut parents = HashSet::new();
    rewrite_rows(txn, TASKS, |_, task| {
        let links = task.get("links").and_then(serde_json::Value::as_array);
        for link in links.into_iter().flatten() {
            if link.get("kind").and_then(serde_json::Value::as_str) == Some("parent")
                && let Some(target) = link.get("target_id").and_then(serde_json::Value::as_str)
            {
                parents.insert(target.to_string());
            }
        }
        false
    })?;
    rewrite_rows(txn, TASKS, |id, task| {
        if !parents.contains(id) {
            return false;
        }
        task.insert("kind".to_string(), serde_json::Value::from("epic"));
        true
    })
}

/// Gives each stored task, binned ones included, the owner its live claim, responsible actor or
/// first member names.
fn assign_task_owners(txn: &WriteTransaction) -> DbResult<()> {
    let now = Utc::now();
    rewrite_rows(txn, TASKS, |_, task| set_owner_of(task, now))?;
    rewrite_rows(txn, BIN, |_, bin| {
        bin.get_mut("task")
            .and_then(serde_json::Value::as_object_mut)
            .is_some_and(|task| set_owner_of(task, now))
    })
}

/// Sets `owner` on a stored task from its claim, responsible actor and members, and returns
/// whether it changed the row.
fn set_owner_of(task: &mut JsonObject, now: DateTime<Utc>) -> bool {
    let field = |name: &str| task.get(name).cloned().unwrap_or_default();
    let claim: Option<TaskClaim> = serde_json::from_value(field("claim")).ok().flatten();
    let responsible: Option<Responsible> =
        serde_json::from_value(field("responsible")).ok().flatten();
    let members: Vec<String> = serde_json::from_value(field("member_ids")).unwrap_or_default();
    let Some(owner) = Owner::from_holders(claim.as_ref(), responsible.as_ref(), &members, now)
    else {
        return false;
    };
    let Ok(owner) = serde_json::to_value(owner) else {
        return false;
    };
    task.insert("owner".to_string(), owner);
    true
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

/// Gives every token and device request all boards and the permissions of the preset its scope
/// stands for, except `tokens.issue`, which no upgrade grants.
fn grant_scope_permissions(txn: &WriteTransaction) -> DbResult<()> {
    grant_permissions_in(txn, API_TOKENS, "api_tokens")?;
    grant_permissions_in(txn, SUSPENDED_TOKENS, "suspended_tokens")?;
    grant_permissions_in(txn, DEVICE_AUTHS, "device_auths")
}

/// The permissions of a preset that no upgrade grants to a stored row.
const UNGRANTED: [Permission; 2] = [Permission::TokensIssue, Permission::RolesGrant];

/// Gives `added` to every token and device request that holds the whole lead or admin preset of
/// its scope as it stood before they existed.
fn grant_added_permissions(txn: &WriteTransaction, added: &[Permission]) -> DbResult<()> {
    for table in [API_TOKENS, SUSPENDED_TOKENS, DEVICE_AUTHS] {
        rewrite_rows(txn, table, |_, row| grant_to_preset(row, added))?;
    }
    Ok(())
}

/// Gives `row` the permissions in `added` that the preset of its scope holds, and reports whether
/// it did. Only a row short of its preset by nothing but `added`, `tokens.issue` and `roles.grant`,
/// which no upgrade grants, gains them, so a custom set and a full preset stay as they are.
fn grant_to_preset(row: &mut JsonObject, added: &[Permission]) -> bool {
    let Some(scope) = row
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .and_then(AuthScope::parse)
    else {
        return false;
    };
    let preset = scope.permissions();
    let Some(held) = row
        .get("permissions")
        .cloned()
        .and_then(|value| serde_json::from_value::<BTreeSet<Permission>>(value).ok())
    else {
        return false;
    };
    let short_only_of_added = preset
        .difference(&held)
        .all(|permission| added.contains(permission) || UNGRANTED.contains(permission));
    if !held.is_subset(&preset) || !short_only_of_added {
        return false;
    }
    let granted: BTreeSet<Permission> = held
        .iter()
        .chain(
            added
                .iter()
                .filter(|permission| preset.contains(permission)),
        )
        .copied()
        .collect();
    if granted == held {
        return false;
    }
    let Ok(granted) = serde_json::to_value(granted) else {
        return false;
    };
    row.insert("permissions".to_string(), granted);
    true
}

/// Sets `permissions` and `boards` on every row of `table` from its scope, and fails on a row whose
/// scope is not one of the four, so no row is granted by guesswork.
fn grant_permissions_in(txn: &WriteTransaction, table: Rows, name: &str) -> DbResult<()> {
    let mut tbl = txn.open_table(table)?;
    let mut rewritten = Vec::new();
    for item in tbl.iter()? {
        let (key, value) = item?;
        let mut row: serde_json::Value = serde_json::from_slice(value.value())?;
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        let Some(scope) = object
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .and_then(AuthScope::parse)
        else {
            return Err(format!("a row in {name} holds no valid scope").into());
        };
        let mut granted = scope.permissions();
        granted.remove(&Permission::TokensIssue);
        granted.remove(&Permission::RolesGrant);
        object.insert("permissions".to_string(), serde_json::to_value(granted)?);
        object.insert(
            "boards".to_string(),
            serde_json::to_value(BoardAccess::All)?,
        );
        rewritten.push((key.value().to_owned(), serde_json::to_vec(&row)?));
    }
    for (key, raw) in rewritten {
        tbl.insert(key.as_str(), raw.as_slice())?;
    }
    Ok(())
}

/// Returns whether `tables` includes the legacy `attachment_data` table.
fn has_legacy_attachment_table(mut tables: impl Iterator<Item = UntypedTableHandle>) -> bool {
    tables.any(|table| table.name() == LEGACY_ATTACHMENT_DATA.name())
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
/// webhook id -> `WebhookRow`.
const WEBHOOKS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("webhooks");
/// inbound address id -> `InboundAddressRow`.
const INBOUND_ADDRESSES: TableDefinition<'_, &str, &[u8]> =
    TableDefinition::new("inbound_addresses");
/// SHA-256 of each inbound address -> its id.
const INBOUND_BY_HASH: TableDefinition<'_, &str, &str> = TableDefinition::new("inbound_by_hash");
/// delivery key -> [`OutboxRow`].
const WEBHOOK_OUTBOX: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("webhook_outbox");
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
/// board id -> webhook id for `WebhookRow` rows.
const WEBHOOKS_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("webhooks_by_board");
/// board id -> inbound address id for `InboundAddressRow` rows.
const INBOUND_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("inbound_by_board");
/// webhook id -> delivery key for [`OutboxRow`] rows.
const WEBHOOK_OUTBOX_BY_HOOK: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("webhook_outbox_by_hook");
/// board id -> id of each deleted task in its recycle bin.
const BIN_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("recycle_bin_by_board");
/// task id -> id of each task holding a [`TaskLink`] that targets it.
const LINKS_BY_TARGET: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("task_links_by_target");
/// account id -> id of each board it owns, with ownerless boards under the empty id.
const BOARDS_BY_OWNER: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("boards_by_owner");
/// `<board id>|<account id>` -> [`BoardMemberRow`] for each account that joined a board it does
/// not own.
const BOARD_MEMBERS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("board_members");
/// board id -> id of each account that joined it.
const MEMBERS_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("members_by_board");
/// account id -> id of each board it joined.
const BOARDS_BY_MEMBER: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("boards_by_member");
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

/// What adding a calendar feed to a board did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedAdd {
    /// The board holds the feed now.
    Added(BoardFeed),
    /// The board already holds [`MAX_BOARD_FEEDS`].
    Full,
    /// No board has that id.
    NoBoard,
}

/// What saving a report bookmark on a board did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookmarkAdd {
    /// The board holds the bookmark now.
    Added(ReportBookmark),
    /// The board already holds [`MAX_REPORT_BOOKMARKS`].
    Full,
    /// No board has that id.
    NoBoard,
}

/// The fields of a report bookmark a change replaces, where a field left `None` stays.
#[derive(Clone, Debug, Default)]
pub struct BookmarkPatch {
    /// The new name.
    pub name: Option<String>,
    /// Whether every member of the board sees it.
    pub shared: Option<bool>,
    /// Whether the Reports menu lists it.
    pub shortcut: Option<bool>,
    /// The new filter values, a JSON object.
    pub filters: Option<String>,
}
/// base64url credential id -> id of the passkey that holds it.
const PASSKEYS_BY_CREDENTIAL: TableDefinition<'_, &str, &str> =
    TableDefinition::new("passkeys_by_credential");
/// account id -> id of each API token it holds, in [`API_TOKENS`] or [`SUSPENDED_TOKENS`].
const TOKENS_BY_ACCOUNT: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("tokens_by_account");
/// `role id/board id` -> id of each token issued as that role that reaches that board, in
/// [`API_TOKENS`] or [`SUSPENDED_TOKENS`]. A token that reaches every board sits under `role id/`.
const TOKENS_BY_ROLE_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("tokens_by_role_board");
/// Agent roles by id.
const AGENT_ROLES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("agent_roles");
/// account id -> id of each agent role it holds.
const AGENT_ROLES_BY_ACCOUNT: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("agent_roles_by_account");
/// `role id/board id` -> the grant of that role on that board.
const AGENT_ROLE_GRANTS: TableDefinition<'_, &str, &[u8]> =
    TableDefinition::new("agent_role_grants");
/// board id -> id of each agent role granted on it.
const GRANTS_BY_BOARD: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("agent_role_grants_by_board");
/// agent role id -> id of each board it is granted on.
const GRANTS_BY_ROLE: MultimapTableDefinition<'_, &str, &str> =
    MultimapTableDefinition::new("agent_role_grants_by_role");
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
    /// Where the attachment bytes live, beside the database file.
    files: AttachmentFiles,
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
    /// Runs the host lookups and test requests of the webhook settings.
    pub prober: Prober,
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
    /// Attachment uploads that may run at once, each streaming its file to disk.
    pub const UPLOADS_AT_ONCE: usize = 8;
    /// Attachment uploads to one account's tasks that may run at once.
    pub const UPLOADS_PER_ACCOUNT_AT_ONCE: usize = 4;
    /// Waits for board events one account may hold open at once, holding no thread and no
    /// transaction.
    pub const WAITS_PER_ACCOUNT_AT_ONCE: usize = 32;
    /// Waits for board events one API token may hold open at once.
    pub const WAITS_PER_TOKEN_AT_ONCE: usize = 16;

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
            prober: Prober::new(is_public_address),
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

    /// Replaces the prober, to reach a local server or another resolver.
    #[cfg(test)]
    #[must_use]
    pub fn with_prober(mut self, prober: Prober) -> Self {
        self.prober = prober;
        self
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

    /// Returns the most bytes the admin lets one attachment file hold.
    #[must_use]
    pub fn max_attachment_bytes(&self) -> u64 {
        self.disk.limits().max_attachment_bytes()
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

/// Returns the stored settings, or the defaults when none were saved.
fn settings_or_default(stored: Option<Settings>) -> Settings {
    let mut settings = stored.unwrap_or_default();

    // An empty reasons list renders an empty "Why did you stop?" menu, so restore the defaults
    if settings.interrupt_reasons.is_empty() {
        settings.interrupt_reasons = Settings::default().interrupt_reasons;
    }
    settings
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
/// false when the id is unknown. The change is the server's own and leaves the column, name,
/// size, labels and comments as they are.
fn mutate_task<F>(db: &Store, key: &str, f: F) -> DbResult<bool>
where
    F: FnOnce(&mut TaskRow),
{
    mutate_task_by(db, key, Actor::SYSTEM, f)
}

/// [`mutate_task`] for a change `actor` makes, whatever it touches.
fn mutate_task_by<F>(db: &Store, key: &str, actor: Actor<'_>, f: F) -> DbResult<bool>
where
    F: FnOnce(&mut TaskRow),
{
    mutate_task_as(db, key, OverQuota::Refuse, actor, f)
}

/// [`mutate_task_by`] for a change that `over` says whether a quota refuses.
fn mutate_task_as<F>(
    db: &Store,
    key: &str,
    over: OverQuota,
    actor: Actor<'_>,
    f: F,
) -> DbResult<bool>
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
        txn_store_task(&txn, &mut tasks, &task, over, actor)?;
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

/// Returns the id of the board the task `id` sits on, read inside an open transaction.
fn task_board_in(txn: &impl Reads, id: &str) -> DbResult<Option<String>> {
    let Some(task) = read_in::<TaskRow>(txn, TASKS, id)? else {
        return Ok(None);
    };
    Ok(read_in::<ColumnRow>(txn, COLUMNS, &task.column_id)?.map(|column| column.board_id))
}

/// What one board holds in task rows and attachment files, its recycle bin included.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardUse {
    /// Bytes of the board's task rows and their attachment files.
    pub bytes: u64,
    /// Bytes of the board's largest attachment file.
    pub largest_file: u64,
}

impl BoardUse {
    /// Adds the bytes of `task` and its attachment files.
    fn add_task(&mut self, task: &TaskRow) -> DbResult<()> {
        self.bytes += serde_json::to_vec(task)?.len() as u64;
        for attachment in &task.attachments {
            self.bytes += attachment.stored_bytes();
            self.largest_file = self.largest_file.max(attachment.size);
        }
        Ok(())
    }
}

/// Returns what board `board_id` holds in task rows, attachment files and its recycle bin, read
/// inside an open transaction.
///
/// Each file counts at the size its row records.
fn board_use_in(txn: &impl Reads, board_id: &str) -> DbResult<BoardUse> {
    let mut held = BoardUse::default();
    for column_id in index_in(txn, COLUMNS_BY_BOARD, board_id)? {
        for task_id in index_in(txn, TASKS_BY_COLUMN, &column_id)? {
            if let Some(task) = read_in::<TaskRow>(txn, TASKS, &task_id)? {
                held.add_task(&task)?;
            }
        }
    }
    for task_id in index_in(txn, BIN_BY_BOARD, board_id)? {
        if let Some(binned) = read_in::<BinRow>(txn, BIN, &task_id)? {
            held.add_task(&binned.task)?;
        }
    }
    Ok(held)
}

/// Refuses, with [`QuotaExceeded`], a write that adds `added` bytes to board `board_id` when that
/// leaves it past its owner's stored-bytes quota for one board, inside that transaction. Single
/// mode, the admin and the ownerless id have no quotas.
fn txn_refuse_board_over_bytes(txn: &impl Reads, board_id: &str, added: u64) -> DbResult<()> {
    let Some(board) = read_in::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(());
    };
    let Some(quotas) = txn_member_quotas(txn, &board.owner_id)? else {
        return Ok(());
    };
    let limit = quotas.limit(Quota::StoredMibPerBoard);
    let after = board_use_in(txn, board_id)?.bytes.saturating_add(added);
    if after > u64::from(limit) * MIB {
        return Err(Box::new(QuotaExceeded::of(Quota::StoredMibPerBoard, limit)));
    }
    Ok(())
}

/// What a count of a board's tasks by number reads of a task.
#[derive(Deserialize)]
struct TaskNumber {
    /// The task's number, absent on a task created without numbering.
    #[serde(default)]
    number: Option<String>,
}

/// Returns the highest number under `prefix` that a task of board `board_id` holds, in the board or
/// in its recycle bin, so a restored task never meets a number handed out again.
fn highest_number_in(txn: &impl Reads, board_id: &str, prefix: &str) -> DbResult<u64> {
    let highest = Cell::new(0);
    let note = |label: Option<&str>| {
        if let Some(number) = label.and_then(|label| Numbering::number_of(label, prefix)) {
            highest.set(highest.get().max(number));
        }
    };
    board_task_count_in(txn, board_id, |task: &TaskNumber| {
        note(task.number.as_deref());
        false
    })?;
    for task_id in index_in(txn, BIN_BY_BOARD, board_id)? {
        if let Some(binned) = read_in::<BinRow>(txn, BIN, &task_id)? {
            note(binned.task.number.as_deref());
        }
    }
    Ok(highest.get())
}

/// Stores `numbering` in the config bag of `board`, or removes it with none.
fn txn_store_numbering(
    txn: &WriteTransaction,
    board: &mut BoardRow,
    numbering: Option<&Numbering>,
) -> DbResult<()> {
    let mut config: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&board.config_json).unwrap_or_default();
    match numbering {
        Some(numbering) => {
            config.insert(NUMBERING_KEY.to_string(), serde_json::to_value(numbering)?);
        }
        None => {
            config.remove(NUMBERING_KEY);
        }
    }
    board.config_json = serde_json::to_string(&config)?;
    txn_store_board(txn, board, OverQuota::Allow)
}

/// Takes the next number of board `board_id`'s numbering inside an open write transaction and
/// returns its label, or none when the board has no numbering.
fn txn_take_task_number(txn: &WriteTransaction, board_id: &str) -> DbResult<Option<String>> {
    let Some(mut board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(None);
    };
    let Some(mut numbering) = board.numbering() else {
        return Ok(None);
    };
    if numbering.next > MAX_NUMBERING_NUMBER {
        return Err(Box::new(NumberingExhausted));
    }
    let label = Numbering::label(&numbering.prefix, numbering.next);
    numbering.next += 1;
    txn_store_numbering(txn, &mut board, Some(&numbering))?;
    Ok(Some(label))
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
    /// The ids of the tasks that hold a parent link to this one, read for an epic and empty for
    /// any other task.
    pub child_ids: Vec<String>,
}

/// Whether a task change also removes the parent link of each child of the task.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChildLinks {
    /// Leave the children linked.
    Keep,
    /// Delete each child's parent link to the task in the same write.
    Remove,
}

/// Returns the ids of the tasks that hold a parent link to `epic_id`.
fn child_ids_in(
    txn: &WriteTransaction,
    tasks: &impl ReadableTable<&'static str, &'static [u8]>,
    epic_id: &str,
) -> DbResult<Vec<String>> {
    let mut children = Vec::new();
    for holder_id in txn_mmap_get(txn, LINKS_BY_TARGET, epic_id)? {
        let Some(holder) = task_in(tasks, &holder_id)? else {
            continue;
        };
        if holder
            .links
            .iter()
            .any(|link| link.kind == LinkKind::Parent && link.target_id == epic_id)
        {
            children.push(holder_id);
        }
    }
    Ok(children)
}

/// Returns how many children of `epic_id` sit in a column that is not a Done column, leaving out
/// the children in `finishing`, which are about to move into a Done column.
fn txn_open_children(
    txn: &WriteTransaction,
    epic_id: &str,
    finishing: &[String],
) -> DbResult<usize> {
    let tasks = txn.open_table(TASKS)?;
    let mut open = 0;
    for child_id in child_ids_in(txn, &tasks, epic_id)? {
        if finishing.contains(&child_id) {
            continue;
        }
        let Some(child) = task_in(&tasks, &child_id)? else {
            continue;
        };
        let column: Option<ColumnRow> = txn_read_one(txn, COLUMNS, &child.column_id)?;
        if !column.is_some_and(|column| column.is_done) {
            open += 1;
        }
    }
    Ok(open)
}

/// Removes each child's parent link to `epic_id` and records a `link_removed` event on the child.
fn txn_unlink_children(
    txn: &WriteTransaction,
    tasks: &mut redb::Table<'_, &'static str, &'static [u8]>,
    epic_id: &str,
    actor: Actor<'_>,
) -> DbResult<()> {
    let children = child_ids_in(txn, tasks, epic_id)?;
    let epic_name = task_in(tasks, epic_id)?.map(|epic| epic.name);
    let mut by_target = txn.open_multimap_table(LINKS_BY_TARGET)?;
    for child_id in children {
        let Some(mut child) = task_in(tasks, &child_id)? else {
            continue;
        };
        let (removed, kept): (Vec<_>, Vec<_>) = child
            .links
            .into_iter()
            .partition(|link| link.kind == LinkKind::Parent && link.target_id == epic_id);
        child.links = kept;
        for link in &removed {
            let detail = link_event_detail(link, epic_name.as_deref(), |detail| {
                format!("Removed link {detail}")
            });
            push_event(&mut child, HistoryKind::LinkRemoved, detail, actor);
        }
        child.updated_at = Some(Utc::now().to_rfc3339());
        txn_store_task(txn, tasks, &child, OverQuota::Allow, actor)?;
        if !child.links.iter().any(|link| link.target_id == epic_id) {
            by_target.remove(epic_id, child_id.as_str())?;
        }
    }
    Ok(())
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
        child_ids: Vec::new(),
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
                    push_event(&mut holder, HistoryKind::LinkRemoved, detail, Actor::SYSTEM);
                }
                holder.updated_at = Some(Utc::now().to_rfc3339());
                txn_store_task(txn, &mut tasks, &holder, OverQuota::Allow, Actor::SYSTEM)?;
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

/// Returns true when a `blocked_by` link among `links` names a task that is still open, read inside
/// an open transaction. A task is open when it sits in a column that is not done, unless it is a
/// closed epic. `open` remembers, by task id, whether a named task is open, so each is read once.
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
                    Some(target) if target.kind == TaskKind::Epic && target.closed => None,
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
            txn_store_task(txn, &mut tasks, &task, OverQuota::Allow, Actor::SYSTEM)?;
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
        if !task.is_work() || task.completed_at.is_some() == is_done {
            continue;
        }
        task.completed_at = is_done.then(|| Utc::now().to_rfc3339());
        txn_store_task(txn, &mut tasks, &task, OverQuota::Allow, Actor::SYSTEM)?;
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

/// Returns whether `to` is `from` or one of the umbrellas above it, following `parent` links.
fn parent_chain_reaches<T>(tbl: &T, from: &str, to: &str) -> DbResult<bool>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    let mut seen = std::collections::HashSet::new();
    let mut current = Some(from.to_string());
    while let Some(id) = current {
        if id == to {
            return Ok(true);
        }
        if !seen.insert(id.clone()) {
            break;
        }
        current = task_in(tbl, &id)?.and_then(|task| {
            task.links
                .iter()
                .find(|link| link.kind == LinkKind::Parent)
                .map(|link| link.target_id.clone())
        });
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

/// Returns the minutes `account` logged on a task inside `txn`.
fn txn_account_minutes(txn: &WriteTransaction, account: &str, task_id: &str) -> DbResult<i64> {
    let mut total = 0;
    for entry_id in txn_mmap_get(txn, ENTRIES_BY_TASK, task_id)? {
        if let Some(held) = txn_read_one::<TimeEntryRow>(txn, TIME_ENTRIES, &entry_id)?
            && held.account_id == account
        {
            total += held.minutes;
        }
    }
    Ok(total)
}

/// Appends a history event that changed a value to the task `task_id` inside `txn`, storing the
/// task under `over`. Returns false, writing nothing, when the task does not exist.
fn txn_push_task_event(
    txn: &WriteTransaction,
    task_id: &str,
    kind: HistoryKind,
    detail: &str,
    actor: Actor<'_>,
    change: EventChange,
    over: OverQuota,
) -> DbResult<bool> {
    let mut tasks = txn.open_table(TASKS)?;
    let Some(mut task) = task_in(&tasks, task_id)? else {
        return Ok(false);
    };
    push_change_event(&mut task, kind, detail.to_string(), actor, change);
    task.updated_at = Some(Utc::now().to_rfc3339());
    txn_store_task(txn, &mut tasks, &task, over, actor)?;
    Ok(true)
}

/// [`push_event`] for an event that changed a value, recording `change` on it.
pub fn push_change_event(
    task: &mut TaskRow,
    kind: HistoryKind,
    detail: String,
    actor: Actor<'_>,
    change: EventChange,
) {
    push_event(task, kind, detail, actor);
    record_change(task, change);
}

/// Records `change` on the newest history event of `task`, if it has one.
fn record_change(task: &mut TaskRow, change: EventChange) {
    if let Some(event) = task.history.last_mut() {
        event.from = change.from.as_deref().map(history_text);
        event.to = change.to.as_deref().map(history_text);
    }
}

/// Append a history event, dropping the oldest past the cap.
pub fn push_event(task: &mut TaskRow, kind: HistoryKind, detail: String, actor: Actor<'_>) {
    push_column_event(task, kind, detail, actor, None);
}

/// [`push_event`] for an event that puts the task in `column_id`, so reports follow the column
/// through renames. Every text the event records is cut to [`MAX_HISTORY_TEXT_CHARS`].
pub fn push_column_event(
    task: &mut TaskRow,
    kind: HistoryKind,
    detail: String,
    actor: Actor<'_>,
    column_id: Option<&str>,
) {
    task.history.push(TaskEvent {
        id: Uuid::new_v4().to_string(),
        kind,
        detail: history_text(&detail),
        actor: history_text(actor.name),
        created_at: Utc::now().to_rfc3339(),
        column_id: column_id.map(str::to_string),
        token_name: actor.token_name.map(history_text),
        from: None,
        to: None,
        parent_token: actor.parent_token.cloned(),
        role_id: actor.role_id.map(history_text),
        role_name: actor.role_name.map(history_text),
        label: actor.label.map(history_text),
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
pub(crate) fn carry_fields(
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
        LinkKind::Parent => "parent",
        LinkKind::Child => "child",
    };
    let id = &link.target_id;
    let Some(name) = target_name else {
        return wrap(&format!("{kind} {id}"));
    };
    let frame = wrap(&format!("{kind} \"\" ({id})")).chars().count();
    let name = cut_text(name, MAX_HISTORY_TEXT_CHARS.saturating_sub(frame));
    wrap(&format!("{kind} \"{name}\" ({id})"))
}

/// Returns whether an event of `kind` puts the task in a column.
fn puts_in_column(kind: HistoryKind) -> bool {
    matches!(
        kind,
        HistoryKind::Created | HistoryKind::Moved | HistoryKind::Restored
    )
}

/// Returns `event`, which put the task in a column on a board outside the token's set, without
/// the names and ids of that board.
fn redact_placed_outside(mut event: TaskEvent) -> TaskEvent {
    event.detail = match event.kind {
        HistoryKind::Created => "Created on another board",
        HistoryKind::Restored => "Restored on another board",
        _ => "Moved to another board",
    }
    .to_string();
    event.column_id = None;
    event.from = None;
    event.to = None;
    event
}

/// Returns the `moved` event `event`, which took the task from a board outside the token's set,
/// without the names of that board.
fn redact_left_outside(mut event: TaskEvent) -> TaskEvent {
    event.detail = "Moved from another board".to_string();
    event.from = None;
    event
}

/// Returns the `deleted` event `event`, which took the task off a board outside the token's set,
/// without the names of that board.
fn redact_deleted_outside(mut event: TaskEvent) -> TaskEvent {
    event.detail = "Deleted from another board".to_string();
    event
}

/// Returns the events of `history` that `shows` lets through, with the names and ids of a board
/// outside the token's set taken out of the events that put the task in a column on it or took it
/// from it. `outside` tells whether a column sits on such a board.
fn redacted_history(
    history: impl IntoIterator<Item = TaskEvent>,
    mut shows: impl FnMut(&TaskEvent) -> DbResult<bool>,
    mut outside: impl FnMut(&str) -> DbResult<bool>,
) -> DbResult<Vec<TaskEvent>> {
    let mut visible = Vec::new();
    let mut left_outside = false;
    for event in history {
        if !shows(&event)? {
            continue;
        }
        let column = event
            .column_id
            .clone()
            .filter(|_| puts_in_column(event.kind));
        let Some(column) = column else {
            visible.push(if left_outside && event.kind == HistoryKind::Deleted {
                redact_deleted_outside(event)
            } else {
                event
            });
            continue;
        };
        let here = outside(&column)?;
        visible.push(if here {
            redact_placed_outside(event)
        } else if left_outside && event.kind == HistoryKind::Moved {
            redact_left_outside(event)
        } else {
            event
        });
        left_outside = here;
    }
    Ok(visible)
}

/// Returns the last word of a link event's detail: the id of the task it names, or a word no id
/// matches when the other task is gone. Any other event names none.
fn link_event_target(event: &TaskEvent) -> Option<&str> {
    matches!(
        event.kind,
        HistoryKind::LinkAdded | HistoryKind::LinkRemoved
    )
    .then(|| {
        event
            .detail
            .rsplit([' ', '(', ')'])
            .find(|word| !word.is_empty())
    })
    .flatten()
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
            push_event(task, HistoryKind::ClaimExpired, detail, Actor::SYSTEM);
            true
        }
        _ => false,
    }
}

/// Ends the owner's lease at `now`, which keeps its name.
fn end_lease(task: &mut TaskRow, now: chrono::DateTime<Utc>) {
    if let Some(owner) = task.owner.as_mut() {
        owner.lease_expires_at = Some(now.to_rfc3339());
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
/// tasks, time entries, task colors, fields and webhooks with their queued events, and its
/// owner's active timer when it runs on one of its tasks. Returns the ids of its attachments,
/// whose files the caller deletes once the transaction commits.
fn txn_remove_board(txn: &WriteTransaction, board: &BoardRow) -> DbResult<Vec<String>> {
    let id = board.id.as_str();
    let column_ids = txn_mmap_get(txn, COLUMNS_BY_BOARD, id)?;
    let swimlane_ids = txn_mmap_get(txn, SWIMLANES_BY_BOARD, id)?;
    let color_ids = txn_mmap_get(txn, COLORS_BY_BOARD, id)?;
    let field_ids = txn_mmap_get(txn, FIELDS_BY_BOARD, id)?;
    let webhook_ids = txn_mmap_get(txn, WEBHOOKS_BY_BOARD, id)?;
    let inbound_ids = txn_mmap_get(txn, INBOUND_BY_BOARD, id)?;
    for webhook_id in &webhook_ids {
        txn_remove_webhook_outbox(txn, webhook_id)?;
    }
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
        + txn_rows_bytes(txn, FIELD_DEFS, &field_ids)?
        + txn_rows_bytes(txn, WEBHOOKS, &webhook_ids)?
        + txn_rows_bytes(txn, INBOUND_ADDRESSES, &inbound_ids)?;
    let mut attachment_ids = txn_attachment_ids(txn, &task_ids)?;
    unlink_deleted_tasks(txn, &task_ids)?;
    txn_prune_token_sets(txn, &board.owner_id, id)?;
    let (binned_ids, binned_bytes) = txn_remove_board_bin(txn, id)?;
    attachment_ids.extend(binned_ids);
    txn_count_usage(txn, &board.owner_id, 0, freed + binned_bytes)?;
    {
        let mut boards = txn.open_table(BOARDS)?;
        boards.remove(id)?;
        mmap_remove(txn, BOARDS_BY_OWNER, &board.owner_id, id)?;
        txn.open_table(BOARD_ACTIVITY)?.remove(id)?;
        let mut feed_tokens = txn.open_table(FEED_TOKENS)?;
        for feed in board.feeds() {
            feed_tokens.remove(feed_token_key(&feed.token).as_str())?;
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
        let mut webhooks = txn.open_table(WEBHOOKS)?;
        for webhook_id in &webhook_ids {
            webhooks.remove(webhook_id.as_str())?;
            mmap_remove(txn, WEBHOOKS_BY_BOARD, id, webhook_id)?;
        }
        for inbound_id in &inbound_ids {
            txn_drop_inbound(txn, inbound_id)?;
            mmap_remove(txn, INBOUND_BY_BOARD, id, inbound_id)?;
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
    txn_remove_grants_of_board(txn, id)?;
    txn_remove_members_of_board(txn, id, &task_ids)?;
    txn_clear_timer_if(txn, &board.owner_id, |task_id| {
        task_ids.iter().any(|id| id == task_id)
    })?;
    Ok(attachment_ids)
}

/// Returns the ids of the attachments on the tasks `task_ids`, read inside an open write
/// transaction.
fn txn_attachment_ids(txn: &WriteTransaction, task_ids: &[String]) -> DbResult<Vec<String>> {
    let mut attachment_ids = Vec::new();
    for task_id in task_ids {
        if let Some(task) = txn_read_one::<TaskRow>(txn, TASKS, task_id)? {
            attachment_ids.extend(
                task.attachments
                    .iter()
                    .flat_map(|attachment| attachment.file_ids().map(str::to_string)),
            );
        }
    }
    Ok(attachment_ids)
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

/// Returns the ids of the attachments the task rows of an open table and the deleted tasks of
/// an open recycle bin name.
fn named_in(
    tasks: &impl ReadableTable<&'static str, &'static [u8]>,
    bin: &impl ReadableTable<&'static str, &'static [u8]>,
) -> DbResult<HashSet<String>> {
    let mut named = HashSet::new();
    for task in rows_in::<TaskRow, _>(tasks)? {
        named.extend(
            task.attachments
                .iter()
                .flat_map(|attachment| attachment.file_ids().map(str::to_string)),
        );
    }
    for row in rows_in::<BinRow, _>(bin)? {
        named.extend(
            row.task
                .attachments
                .iter()
                .flat_map(|attachment| attachment.file_ids().map(str::to_string)),
        );
    }
    Ok(named)
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
    txn_seed_standard_roles(txn, &user.id)?;
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

/// Rebuilds [`TOKENS_BY_ROLE_BOARD`] from the live and suspended tokens that name a role, inside an
/// open write transaction.
fn txn_reindex_role_tokens(txn: &WriteTransaction) -> DbResult<()> {
    txn.delete_multimap_table(TOKENS_BY_ROLE_BOARD)?;
    for table in [API_TOKENS, SUSPENDED_TOKENS] {
        for token in rows_in::<ApiTokenRecord, _>(&txn.open_table(table)?)? {
            txn_index_role_token(txn, &token)?;
        }
    }
    Ok(())
}

/// Returns the key of [`TOKENS_BY_ROLE_BOARD`] for `role_id` on `board_id`.
fn role_board_key(role_id: &str, board_id: &str) -> String {
    format!("{role_id}/{board_id}")
}

/// Returns the keys of [`TOKENS_BY_ROLE_BOARD`] that `token` sits under, none for a token issued
/// as no role.
fn role_board_keys(token: &ApiTokenRecord) -> Vec<String> {
    let Some(role_id) = token.role_id.as_deref() else {
        return Vec::new();
    };
    match &token.boards {
        BoardAccess::All => vec![format!("{role_id}/")],
        BoardAccess::Only(ids) => ids.iter().map(|id| role_board_key(role_id, id)).collect(),
    }
}

/// Adds `token` under every key [`role_board_keys`] lists, inside an open write transaction.
fn txn_index_role_token(txn: &WriteTransaction, token: &ApiTokenRecord) -> DbResult<()> {
    for key in role_board_keys(token) {
        mmap_insert(txn, TOKENS_BY_ROLE_BOARD, &key, &token.id)?;
    }
    Ok(())
}

/// Removes `token` from every key [`role_board_keys`] lists, inside an open write transaction.
fn txn_unindex_role_token(txn: &WriteTransaction, token: &ApiTokenRecord) -> DbResult<()> {
    for key in role_board_keys(token) {
        mmap_remove(txn, TOKENS_BY_ROLE_BOARD, &key, &token.id)?;
    }
    Ok(())
}

/// Returns the ids of the tokens issued as `role_id` that reach `board_id`, or every token issued
/// as it when `board_id` is [`None`], read inside an open write transaction.
fn txn_role_token_ids(
    txn: &WriteTransaction,
    role_id: &str,
    board_id: Option<&str>,
) -> DbResult<BTreeSet<String>> {
    let tbl = txn.open_multimap_table(TOKENS_BY_ROLE_BOARD)?;
    let every = format!("{role_id}/");
    let mut keys = Vec::new();
    let mut ids = BTreeSet::new();
    match board_id {
        Some(board_id) => keys.extend([every.clone(), role_board_key(role_id, board_id)]),
        None => {
            let end = format!("{role_id}0");
            for entry in tbl.range(every.as_str()..end.as_str())? {
                for guard in entry?.1 {
                    ids.insert(guard?.value().to_owned());
                }
            }
        }
    }
    for key in keys {
        for guard in tbl.get(key.as_str())? {
            ids.insert(guard?.value().to_owned());
        }
    }
    Ok(ids)
}

/// Stamps `record` as issued as agent role `role_id` of its account, with the role's current name and
/// the next `agent-N` label, inside an open write transaction. Returns `false`, writing nothing,
/// when the account holds no such role.
fn txn_issue_as_role(
    txn: &WriteTransaction,
    record: &mut ApiTokenRecord,
    role_id: &str,
) -> DbResult<bool> {
    let role = txn_read_one::<AgentRoleRow>(txn, AGENT_ROLES, role_id)?
        .filter(|role| role.account_id == record.account_id);
    let Some(mut role) = role else {
        return Ok(false);
    };
    role.issued += 1;
    record.role_id = Some(role.id.clone());
    record.role_name = Some(role.name.clone());
    record.label = Some(format!("agent-{}", role.issued));
    write_one(txn, AGENT_ROLES, role_id, &role)?;
    Ok(true)
}

/// Stores a new `token` when its account is active and below the per-account token cap, inside an
/// open write transaction.
fn txn_create_token(txn: &WriteTransaction, token: &ApiTokenRecord) -> DbResult<()> {
    let account = AccountId::new(token.account_id.as_str());
    txn_refuse_stranded(txn, &account)?;
    if !txn_account_active(txn, &account)? {
        return Err(ACCOUNT_NOT_ACTIVE.into());
    }
    if token.parent_id.is_none()
        && let Some(refusal) = txn_token_cap_reached(txn, &token.account_id)?
    {
        return Err(Box::new(refusal));
    }
    txn_store_token(txn, token)
}

/// Runs the checks every child token passes inside an open write transaction, and sets the expiry
/// of `record`, which names its parent. Returns the refusal, with nothing written beyond the
/// deletion of the account's expired issued tokens, when the parent is gone, is itself issued and
/// the child would hold `tokens.issue`, allows its children fewer than `hours`, or another token
/// of the account has the name.
fn txn_check_child_token(
    txn: &WriteTransaction,
    record: &mut ApiTokenRecord,
    hours: i64,
    now: i64,
) -> DbResult<Result<(), ChildTokenRefusal>> {
    let Some(parent_id) = record.parent_id.as_deref() else {
        return Err("a child token must name its parent".into());
    };
    let parent = txn_read_one::<ApiTokenRecord>(txn, API_TOKENS, parent_id)?;
    let Some(parent) = parent.filter(|parent| parent.account_id == record.account_id) else {
        return Ok(Err(ChildTokenRefusal::ParentGone));
    };
    if parent.parent_id.is_some() && record.permissions.contains(&Permission::TokensIssue) {
        return Ok(Err(ChildTokenRefusal::IssuerIsIssued));
    }
    if hours > parent.child_lifetime_hours {
        return Ok(Err(ChildTokenRefusal::LifetimeTooLong {
            allowed_hours: parent.child_lifetime_hours,
        }));
    }
    record.expires_at =
        Some((now + hours * SECONDS_PER_HOUR).min(parent.expires_at.unwrap_or(i64::MAX)));
    txn_remove_expired_issued_tokens(txn, &record.account_id, now)?;
    if txn_token_name_taken(txn, &record.account_id, &record.name, None)? {
        return Ok(Err(ChildTokenRefusal::NameTaken));
    }
    Ok(Ok(()))
}

/// Returns whether a token of `account` other than `except` has `name`, whatever its letter case.
fn txn_token_name_taken(
    txn: &WriteTransaction,
    account: &str,
    name: &str,
    except: Option<&str>,
) -> DbResult<bool> {
    let name = name.to_lowercase();
    for id in txn_mmap_get(txn, TOKENS_BY_ACCOUNT, account)? {
        if except == Some(id.as_str()) {
            continue;
        }
        for table in [API_TOKENS, SUSPENDED_TOKENS] {
            let Some(token) = txn_read_one::<ApiTokenRecord>(txn, table, &id)? else {
                continue;
            };
            if token.name.to_lowercase() == name {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Deletes the expired issued tokens of `account`, live or suspended, with the tokens they
/// issued, inside an open write transaction, and returns how many live ones it deleted.
fn txn_remove_expired_issued_tokens(
    txn: &WriteTransaction,
    account: &str,
    now: i64,
) -> DbResult<u64> {
    let mut removed = 0;
    for id in txn_mmap_get(txn, TOKENS_BY_ACCOUNT, account)? {
        for table in [API_TOKENS, SUSPENDED_TOKENS] {
            let expired = txn_read_one::<ApiTokenRecord>(txn, table, &id)?
                .is_some_and(|token| token.parent_id.is_some() && token.is_expired(now));
            if expired {
                removed += txn_remove_token(txn, account, &id)?;
            }
        }
    }
    Ok(removed)
}

/// Stores `token` and indexes it under its account and its key, inside an open write
/// transaction.
fn txn_store_token(txn: &WriteTransaction, token: &ApiTokenRecord) -> DbResult<()> {
    write_one(txn, API_TOKENS, &token.id, token)?;
    mmap_insert(txn, TOKENS_BY_ACCOUNT, &token.account_id, &token.id)?;
    txn_index_role_token(txn, token)?;
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
/// entries, inside an open write transaction, together with the children it issued, and returns
/// how many live tokens it deleted.
fn txn_remove_token(txn: &WriteTransaction, account: &str, id: &str) -> DbResult<u64> {
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
        txn_unindex_role_token(txn, token)?;
    }
    mmap_remove(txn, TOKENS_BY_ACCOUNT, account, id)?;
    let mut removed = u64::from(live.is_some());
    for child in txn_child_tokens(txn, account, id)? {
        removed += txn_remove_token(txn, account, &child)?;
    }
    Ok(removed)
}

/// Returns the ids of the tokens of `account`, live or suspended, that token `parent` issued,
/// inside an open write transaction.
fn txn_child_tokens(txn: &WriteTransaction, account: &str, parent: &str) -> DbResult<Vec<String>> {
    let mut children = Vec::new();
    for id in txn_mmap_get(txn, TOKENS_BY_ACCOUNT, account)? {
        for table in [API_TOKENS, SUSPENDED_TOKENS] {
            let token = txn_read_one::<ApiTokenRecord>(txn, table, &id)?;
            if token.is_some_and(|token| token.parent_id.as_deref() == Some(parent)) {
                children.push(id.clone());
            }
        }
    }
    Ok(children)
}

/// Deletes every token of `account`, live or suspended, from [`API_TOKENS`], [`SUSPENDED_TOKENS`]
/// and the index, inside an open write transaction, and returns how many live ones it deleted.
fn txn_remove_tokens_of(txn: &WriteTransaction, account: &str) -> DbResult<u64> {
    let mut removed = 0;
    for id in txn_mmap_get(txn, TOKENS_BY_ACCOUNT, account)? {
        removed += txn_remove_token(txn, account, &id)?;
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
/// with its calendar-feed tokens, and counts the change in its size against its owner.
fn txn_store_board(txn: &WriteTransaction, board: &BoardRow, over: OverQuota) -> DbResult<()> {
    let held: Vec<String> = read_in::<BoardRow>(txn, BOARDS, &board.id)?
        .map(|held| held.feeds().into_iter().map(|feed| feed.token).collect())
        .unwrap_or_default();
    txn_store_owned(txn, BOARDS, &board.id, board, over)?;
    let tokens: Vec<String> = board.feeds().into_iter().map(|feed| feed.token).collect();
    if held == tokens {
        return Ok(());
    }
    let mut index = txn.open_table(FEED_TOKENS)?;
    for token in held.iter().filter(|token| !tokens.contains(token)) {
        index.remove(feed_token_key(token).as_str())?;
    }
    for token in tokens.iter().filter(|token| !held.contains(token)) {
        index.insert(feed_token_key(token).as_str(), board.id.as_str())?;
    }
    Ok(())
}

/// Rebuilds [`FEED_TOKENS`] from the boards, inside an open write transaction.
fn txn_reindex_feed_tokens(txn: &WriteTransaction) -> DbResult<()> {
    let boards: Vec<BoardRow> = rows_in(&txn.open_table(BOARDS)?)?;
    let mut tokens = txn.open_table(FEED_TOKENS)?;
    tokens.retain(|_, _| false)?;
    for board in &boards {
        for feed in board.feeds() {
            tokens.insert(feed_token_key(&feed.token).as_str(), board.id.as_str())?;
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
/// none are stored, read inside an open transaction.
fn txn_instance_settings(txn: &impl Reads) -> DbResult<InstanceSettings> {
    let stored: Option<InstanceSettings> = read_in(txn, SETTINGS, INSTANCE_SETTINGS_KEY)?;
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
    /// The webhooks one board may hold, [`MAX_WEBHOOKS_PER_BOARD`].
    WebhooksPerBoard,
    /// The inbound addresses one board may hold, [`MAX_INBOUND_PER_BOARD`].
    InboundPerBoard,
    /// The agent roles one account may hold, [`MAX_AGENT_ROLES_PER_ACCOUNT`].
    AgentRoles,
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
            Self::Quota(Quota::StoredMibPerBoard) => "MiB stored per board",
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
            Self::WebhooksPerBoard => "webhooks per board",
            Self::InboundPerBoard => "inbound addresses per board",
            Self::AgentRoles => "agent roles per account",
        }
    }
}

/// A file placed in the attachment store whose row is not yet written.
struct StoredFile {
    id: String,
    size: u64,
    sha256: Option<String>,
    url: Option<String>,
    edited_at: Option<String>,
}

/// Refusal of an upload to a task that already holds an attachment with the same name or the same
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateAttachment {
    /// The id of the attachment the task already holds.
    pub existing_id: String,
    /// The name of the attachment the task already holds.
    pub name: String,
}

impl DuplicateAttachment {
    /// Returns the refusal of a file called `name` whose bytes hash to `sha256`, when `held`, the
    /// attachments of its task, has one with that name or that hash. An attachment stored before
    /// hashes has none, and matches by name alone.
    fn among(held: &[TaskAttachment], name: &str, sha256: Option<&str>) -> Option<Self> {
        held.iter()
            .find(|held| {
                held.name == name || (sha256.is_some() && held.sha256.as_deref() == sha256)
            })
            .map(|held| Self {
                existing_id: held.id.clone(),
                name: held.name.clone(),
            })
    }
}

impl fmt::Display for DuplicateAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the task already holds this file as attachment {} named {}",
            self.existing_id, self.name
        )
    }
}

impl Error for DuplicateAttachment {}

/// Refusal of a palette copy that would strand or recolor a task of the target board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteStrandsColor {
    /// The internal id of the custom color a task of the target board uses, such as `custom1`.
    pub value: String,
}

impl fmt::Display for PaletteStrandsColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a task of this board uses its custom color {}, and the source board has no custom color {} with the same hex",
            self.value, self.value
        )
    }
}

impl Error for PaletteStrandsColor {}

/// Refusal of a task create or cross-board move into a board whose numbering has no number left.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NumberingExhausted;

impl fmt::Display for NumberingExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the board's numbering has no number left; remove the numbering and activate one under another prefix"
        )
    }
}

impl Error for NumberingExhausted {}

/// Refusal of a token name that another token of the account holds, whatever its letter case.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TokenNameTaken;

impl fmt::Display for TokenNameTaken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("another token of the account has that name")
    }
}

impl Error for TokenNameTaken {}

/// The field of a token an edit may change. A name left as [`None`] stays as it is.
#[derive(Debug)]
pub struct TokenEdit {
    /// The new name.
    pub name: Option<String>,
}

/// Why a child token was not stored.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChildTokenRefusal {
    /// The issuing token was revoked, or never existed on the account.
    ParentGone,
    /// Another token of the account has the name, whatever its letter case.
    NameTaken,
    /// The issuing token was itself issued, so its child may not hold `tokens.issue`.
    IssuerIsIssued,
    /// The child would live longer than the issuing token allows its children.
    LifetimeTooLong {
        /// Most hours a child of the issuing token may live.
        allowed_hours: i64,
    },
}

/// Why a token was not stored as a child issued as an agent role.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RoleTokenRefusal {
    /// The account holds no such role.
    UnknownRole,
    /// A check every child token passes refused it.
    Child(ChildTokenRefusal),
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
        match self.bound {
            Bound::Quota(Quota::StoredMibPerAccount) => {
                write!(f, "this account has used its {} MiB of storage", self.limit)
            }
            bound => write!(
                f,
                "limit reached: at most {} {}",
                self.limit,
                bound.describe()
            ),
        }
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

/// A session cookie value, which hashes to the key its session is stored under.
#[derive(Copy, Clone)]
pub struct SessionToken<'a>(&'a str);

impl<'a> SessionToken<'a> {
    /// Wraps the cookie value `raw`.
    pub fn new(raw: &'a str) -> Self {
        Self(raw)
    }

    /// Returns the key the session is stored under.
    fn key(self) -> String {
        hash_session_token(self.0)
    }
}

impl fmt::Debug for SessionToken<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(..)")
    }
}

/// Longest device label a session keeps, in characters.
const MAX_SESSION_DEVICE_CHARS: usize = 100;

/// Characters of a session's key that name it in the sessions list.
const SESSION_PUBLIC_ID_CHARS: usize = 16;

/// One session in the sessions list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListing {
    /// The name that ends this session, a prefix of the session's key.
    pub id: String,
    /// The browser and system the sign-in came from, empty when unknown.
    pub device: String,
    /// RFC 3339 time of the sign-in.
    pub created_at: String,
    /// Whether the request asking is this session.
    pub current: bool,
}

/// What ending one listed session did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionEndOutcome {
    /// The session ended.
    Ended,
    /// The account has no such session.
    NotFound,
    /// It is the session asking, which stays.
    Current,
}

/// Returns the part of a session's key that names it in the sessions list.
fn session_public_id(key: &str) -> &str {
    key.get(..SESSION_PUBLIC_ID_CHARS).unwrap_or(key)
}

/// Most API tokens one account may hold, the admin's included.
pub const MAX_API_TOKENS_PER_ACCOUNT: usize = 50;

/// Most passkeys one account may hold, the admin's included.
pub const MAX_PASSKEYS_PER_ACCOUNT: usize = 10;

/// Most webhooks one board may hold.
pub const MAX_WEBHOOKS_PER_BOARD: usize = 20;

/// Most inbound addresses one board may hold.
pub const MAX_INBOUND_PER_BOARD: usize = 20;

/// Most agent roles one account may hold.
pub const MAX_AGENT_ROLES_PER_ACCOUNT: usize = 50;

/// Most bytes one task's stored row may take, so reading or rewriting a task stays cheap.
pub const MAX_TASK_ROW_BYTES: usize = 1_048_576;

/// Most bytes one field definition's stored row may take, 256 KiB.
pub const MAX_FIELD_DEF_ROW_BYTES: usize = 262_144;

/// Most bytes one saved template's stored row may take, 8 MiB.
pub const MAX_TEMPLATE_ROW_BYTES: usize = 8_388_608;

/// Returns the refusal of a token for `account` while it holds [`MAX_API_TOKENS_PER_ACCOUNT`]
/// tokens made by hand, read inside that write's transaction. A token another token issued does
/// not count.
fn txn_token_cap_reached(txn: &WriteTransaction, account: &str) -> DbResult<Option<QuotaExceeded>> {
    let mut held = 0;
    for id in txn_mmap_get(txn, TOKENS_BY_ACCOUNT, account)? {
        for table in [API_TOKENS, SUSPENDED_TOKENS] {
            let token = txn_read_one::<ApiTokenRecord>(txn, table, &id)?;
            held += usize::from(token.is_some_and(|token| token.parent_id.is_none()));
        }
    }
    Ok(
        (held >= MAX_API_TOKENS_PER_ACCOUNT).then_some(QuotaExceeded {
            bound: Bound::ApiTokens,
            limit: MAX_API_TOKENS_PER_ACCOUNT as u64,
        }),
    )
}

/// Returns the quotas `owner` is held to, read inside an open transaction. Single mode, the
/// admin and the ownerless id have none.
fn txn_member_quotas(txn: &impl Reads, owner: &str) -> DbResult<Option<Quotas>> {
    let settings = txn_instance_settings(txn)?;
    if settings.mode != InstanceMode::Multi {
        return Ok(None);
    }
    let member =
        read_in::<UserRow>(txn, USERS, owner)?.is_some_and(|user| user.role != AccountRole::Admin);
    Ok(member.then_some(settings.quotas))
}

/// Refuses, with [`QuotaExceeded`], a write that leaves `owner` holding `after` items of `quota`,
/// inside that transaction. Single mode, the admin and the ownerless id have no quotas.
fn txn_refuse_over_quota(
    txn: &impl Reads,
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
        txn_store_task(txn, &mut tbl, &task, OverQuota::Allow, Actor::SYSTEM)?;
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
    let (background_hex, border_hex, light_hex) = match color.hex {
        Some(hex) if is_custom_color(color.value) => {
            custom_color_hexes(hex).ok_or_else(|| format!("not a hex color: {hex}"))?
        }
        _ => {
            let (background, border, light, _) = standard_color(color.value)
                .ok_or_else(|| format!("unknown color value: {}", color.value))?;
            (
                background.to_string(),
                border.to_string(),
                light.to_string(),
            )
        }
    };
    let id = Uuid::new_v4().to_string();
    let row = ColorRow {
        id: id.clone(),
        board_id: board_id.to_string(),
        value: color.value.to_string(),
        label: color.label.to_string(),
        description: color.description.to_string(),
        background_hex,
        border_hex,
        light_hex,
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
                hex: None,
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
    stage: Option<ColumnStage>,
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
        watchers: Vec::new(),
        stage,
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

/// Points a board's `blocked_column_id` at `column_id`, or removes the key when `blocked` is false
/// and it names `column_id`, inside an open write transaction.
fn txn_set_blocked_column(
    txn: &WriteTransaction,
    board_id: &str,
    column_id: &str,
    blocked: bool,
) -> DbResult<()> {
    let Some(mut board) = txn_read_one::<BoardRow>(txn, BOARDS, board_id)? else {
        return Ok(());
    };
    let mut cfg: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&board.config_json).unwrap_or_default();
    if blocked {
        cfg.insert(BLOCKED_COLUMN_KEY.to_string(), column_id.into());
    } else if board.is_blocked_column(column_id) {
        cfg.remove(BLOCKED_COLUMN_KEY);
    } else {
        return Ok(());
    }
    board.config_json = serde_json::to_string(&cfg)?;
    txn_store_board(txn, &board, OverQuota::Allow)
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

/// Returns the id of the first unarchived board of `boards`, or of the first when every board is
/// archived.
fn first_board(boards: &[BoardRow]) -> Option<String> {
    let first = boards
        .iter()
        .find(|board| !board.config_bool("archived"))
        .or_else(|| boards.first());
    first.map(|board| board.id.clone())
}

/// Inserts a bare board row for `owner` inside an open write transaction, joining the set of the
/// API token `joining_token`, and returns its id.
fn txn_insert_board(
    txn: &WriteTransaction,
    owner: &AccountId,
    name: &str,
    joining_token: &str,
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
        report_bookmarks: Vec::new(),
        seats: Vec::new(),
        limit_groups: Vec::new(),
    };
    txn_store_owned(txn, BOARDS, &id, &row, over)?;
    mmap_insert(txn, BOARDS_BY_OWNER, owner.as_str(), &id)?;
    txn.open_table(BOARD_ACTIVITY)?
        .insert(id.as_str(), Utc::now().timestamp())?;
    txn_join_token_set(txn, joining_token, &id)?;
    Ok(id)
}

/// Adds board `board_id` to the set of API token `token_id`, when that token has one.
fn txn_join_token_set(txn: &WriteTransaction, token_id: &str, board_id: &str) -> DbResult<()> {
    let Some(mut token) = txn_read_one::<ApiTokenRecord>(txn, API_TOKENS, token_id)? else {
        return Ok(());
    };
    if let BoardAccess::Only(ids) = &mut token.boards
        && ids.insert(board_id.to_string())
    {
        write_one(txn, API_TOKENS, token_id, &token)?;
        if let Some(role_id) = token.role_id.as_deref() {
            mmap_insert(
                txn,
                TOKENS_BY_ROLE_BOARD,
                &role_board_key(role_id, board_id),
                token_id,
            )?;
        }
    }
    Ok(())
}

/// Removes board `board_id` from the set of every token and device request of `owner`.
fn txn_prune_token_sets(txn: &WriteTransaction, owner: &str, board_id: &str) -> DbResult<()> {
    for token_id in txn_mmap_get(txn, TOKENS_BY_ACCOUNT, owner)? {
        for table in [API_TOKENS, SUSPENDED_TOKENS] {
            let Some(mut token) = txn_read_one::<ApiTokenRecord>(txn, table, &token_id)? else {
                continue;
            };
            if token.boards.forget(board_id) {
                write_one(txn, table, &token_id, &token)?;
                if let Some(role_id) = token.role_id.as_deref() {
                    mmap_remove(
                        txn,
                        TOKENS_BY_ROLE_BOARD,
                        &role_board_key(role_id, board_id),
                        &token_id,
                    )?;
                }
            }
        }
    }
    for key in txn_mmap_get(txn, DEVICES_BY_ACCOUNT, owner)? {
        let Some(mut request) = txn_read_one::<DeviceAuthRow>(txn, DEVICE_AUTHS, &key)? else {
            continue;
        };
        if request.boards.forget(board_id) {
            write_one(txn, DEVICE_AUTHS, &key, &request)?;
        }
    }
    Ok(())
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

/// Returns the snapshot of a built-in layout: the simple board's palette and lane with the columns
/// of `layout`.
fn layout_snapshot(layout: &Layout) -> serde_json::Value {
    let last = layout.columns.len() - 1;
    let columns: Vec<serde_json::Value> = layout
        .columns
        .iter()
        .enumerate()
        .map(|(position, name)| {
            serde_json::json!({
                "name": name,
                "wip_limit": null,
                "is_done": position == last,
                "position": position,
            })
        })
        .collect();
    let mut snapshot = simple_task_board_snapshot();
    snapshot["columns"] = serde_json::Value::Array(columns);
    snapshot
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
    /// Starts or stops one account's watch of the column.
    pub watch: Option<ColumnWatch<'a>>,
    /// `Some(None)` clears the WIP limit.
    pub wip_limit: Option<Option<i64>>,
    pub is_done: Option<bool>,
    /// `Some(None)` clears the stage.
    pub stage: Option<Option<ColumnStage>>,
    /// Whether the column is its board's Blocked column, where `false` clears the board's Blocked
    /// column only when it is this one.
    pub is_blocked: Option<bool>,
    pub description: Option<&'a str>,
    pub collapsed: Option<bool>,
    pub config_json: Option<&'a str>,
}

/// A change to whether one account watches a column.
#[derive(Copy, Clone, Debug)]
pub struct ColumnWatch<'a> {
    /// The id of the account.
    pub account: &'a str,
    /// Whether the account watches the column afterwards.
    pub watched: bool,
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

/// One built-in board template: (name, description, snapshot).
type BuiltinTemplateSpec = (&'static str, &'static str, serde_json::Value);

/// Parameters for inserting a [`ColorRow`] (keeps `txn_insert_color`
/// under clippy's argument-count lint).
struct NewColor<'a> {
    value: &'a str,
    /// The background of a custom color, which has no fixed palette entry.
    hex: Option<&'a str>,
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
    /// Maximum stored attachment size: 50 MiB.
    pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
    /// Maximum allowed color label length (KanbanFlow renames cap at 50).
    pub const MAX_COLOR_LABEL_LEN: usize = 50;
    /// Maximum history events kept per task; older ones are dropped from
    /// the front so the trail stays bounded (KanbanFlow parity: History).
    pub const MAX_HISTORY_EVENTS: usize = 500;
    /// Maximum attachment size sent as base64 in a JSON body: 5 MiB.
    pub const MAX_JSON_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
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
            txn.open_multimap_table(COLORS_BY_BOARD)?;
            txn.open_multimap_table(LINKS_BY_TARGET)?;
            txn.open_table(BIN)?;
            txn.open_multimap_table(BIN_BY_BOARD)?;
            txn.open_table(FIELD_DEFS)?;
            txn.open_table(DEVICE_AUTHS)?;
            txn.open_table(PASSKEYS)?;
            txn.open_multimap_table(FIELDS_BY_BOARD)?;
            txn.open_table(WEBHOOKS)?;
            txn.open_multimap_table(WEBHOOKS_BY_BOARD)?;
            txn.open_table(INBOUND_ADDRESSES)?;
            txn.open_table(INBOUND_BY_HASH)?;
            txn.open_multimap_table(INBOUND_BY_BOARD)?;
            txn.open_table(AGENT_ROLES)?;
            txn.open_multimap_table(AGENT_ROLES_BY_ACCOUNT)?;
            txn.open_table(AGENT_ROLE_GRANTS)?;
            txn.open_multimap_table(GRANTS_BY_BOARD)?;
            txn.open_multimap_table(GRANTS_BY_ROLE)?;
            txn.open_table(WEBHOOK_OUTBOX)?;
            txn.open_multimap_table(WEBHOOK_OUTBOX_BY_HOOK)?;
            txn.open_multimap_table(BOARDS_BY_OWNER)?;
            txn.open_table(ACCOUNT_SETTINGS)?;
            txn.open_table(AVATARS)?;
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
            txn.open_multimap_table(TOKENS_BY_ROLE_BOARD)?;
            txn.open_multimap_table(SESSIONS_BY_USER)?;
            txn.open_multimap_table(DEVICES_BY_ACCOUNT)?;
            txn.open_table(TOKEN_IDS_BY_HASH)?;
            txn.open_multimap_table(SALTED_TOKENS_BY_LAST4)?;
            txn.open_multimap_table(DEVICES_BY_USER_CODE)?;
            txn.open_multimap_table(DEVICES_BY_EXPIRY)?;
            txn.open_table(BOARD_EVENTS)?;
            txn.open_table(EVENT_WAKEUPS)?;
            txn.open_table(BOARD_MEMBERS)?;
            txn.open_multimap_table(MEMBERS_BY_BOARD)?;
            txn.open_multimap_table(BOARDS_BY_MEMBER)?;
            txn.open_table(NOTIFICATIONS)?;
        }
        txn.commit()?;
        compact(&mut db, path);

        let this = Self {
            db: Arc::new(Store::new(db)),
            path: Arc::from(Path::new(path)),
            files: AttachmentFiles::open(Path::new(path))?,
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
        this.fill_responsible()?;
        this.seed()?;
        this.move_legacy_attachments()?;
        this.check_attachment_files()?;
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
                txn_store_task(&txn, &mut tasks, task, OverQuota::Allow, Actor::SYSTEM)?;
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
                txn_store_task(&txn, &mut tasks, task, OverQuota::Allow, Actor::SYSTEM)?;
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

    /// Makes the first member of each task responsible when the task has no responsible actor,
    /// once per database.
    ///
    /// A responsible cleared later stays cleared.
    fn fill_responsible(&self) -> DbResult<()> {
        let done: bool = read_one(&self.db, SETTINGS, RESPONSIBLE_FILL_DONE)?.unwrap_or(false);
        if done {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        let filled: Vec<TaskRow> = {
            let tbl = txn.open_table(TASKS)?;
            let mut filled = Vec::new();
            for item in tbl.iter()? {
                let (_, value) = item?;
                let mut task: TaskRow = serde_json::from_slice(value.value())?;
                let changed_by_hand = task
                    .history
                    .iter()
                    .any(|event| event.kind == HistoryKind::ResponsibleUpdated);
                if task.responsible.is_none()
                    && !changed_by_hand
                    && let Some(id) = task.member_ids.first()
                {
                    task.responsible = Some(Responsible::Member { id: id.clone() });
                    filled.push(task);
                }
            }
            filled
        };
        {
            let mut tasks = txn.open_table(TASKS)?;
            for task in &filled {
                txn_store_task(&txn, &mut tasks, task, OverQuota::Allow, Actor::SYSTEM)?;
            }
        }
        write_one(&txn, SETTINGS, RESPONSIBLE_FILL_DONE, &true)?;
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
                pomodoro_template_snapshot(),
            ),
            (
                Self::KANBAN_BASICS_TEMPLATE_NAME,
                "Kanban basics: the standard color palette with the classic To-do / In progress / Done column layout.",
                kanban_basics_template_snapshot(),
            ),
        ]
        .into_iter()
        .chain(LAYOUTS.iter().map(|layout| {
            (
                layout.name,
                layout.description,
                layout_snapshot(layout),
            )
        }))
        .collect();
        let mut wanted_id = None;
        for (name, description, snapshot) in &builtins {
            let snapshot = serde_json::to_vec(snapshot)?;
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

    /// Lists the sessions of `user`, newest first, marking the one of the cookie `current`.
    pub fn list_sessions(
        &self,
        user: &AccountId,
        current: Option<SessionToken<'_>>,
    ) -> DbResult<Vec<SessionListing>> {
        let current = current.map(SessionToken::key);
        let mut listing = Vec::new();
        for key in mmap_get(&self.db, SESSIONS_BY_USER, user.as_str())? {
            if let Some(row) = read_one::<SessionRow>(&self.db, SESSIONS, &key)? {
                listing.push(SessionListing {
                    id: session_public_id(&key).to_string(),
                    device: row.device,
                    created_at: row.created_at,
                    current: current.as_deref() == Some(key.as_str()),
                });
            }
        }
        listing.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(listing)
    }

    /// Ends the session of `user` that the listing names `id`, unless it is the one of the cookie
    /// `current`.
    pub fn end_session(
        &self,
        user: &AccountId,
        id: &str,
        current: Option<SessionToken<'_>>,
    ) -> DbResult<SessionEndOutcome> {
        let current = current.map(SessionToken::key);
        let txn = self.db.begin_write()?;
        let found = txn_mmap_get(&txn, SESSIONS_BY_USER, user.as_str())?
            .into_iter()
            .find(|key| session_public_id(key) == id);
        let outcome = match found {
            None => SessionEndOutcome::NotFound,
            Some(key) if current.as_deref() == Some(key.as_str()) => SessionEndOutcome::Current,
            Some(key) => {
                txn.open_table(SESSIONS)?.remove(key.as_str())?;
                mmap_remove(&txn, SESSIONS_BY_USER, user.as_str(), &key)?;
                SessionEndOutcome::Ended
            }
        };
        txn.commit()?;
        Ok(outcome)
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
    pub fn create_session(&self, user: &AccountId, device: &str) -> DbResult<String> {
        let token = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let row = SessionRow {
            user_id: user.as_str().to_string(),
            created_at: now.clone(),
            verified_at: Some(now),
            device: device.chars().take(MAX_SESSION_DEVICE_CHARS).collect(),
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
        txn_create_token(&txn, record)?;
        txn.commit()?;
        Ok(())
    }

    /// Stores `record` as a child of the token it names in `parent_id`, living `hours` from `now`
    /// and never past that token, and returns the stored record. Returns the refusal, storing
    /// nothing, when that token no longer exists on the same account, is itself issued and the
    /// child would hold `tokens.issue`, allows its children fewer than `hours`, or another token of
    /// the account has the same name. It first deletes the account's expired issued tokens, so they
    /// hold no name. A revoke cannot leave an orphan.
    pub fn create_child_api_token(
        &self,
        mut record: ApiTokenRecord,
        hours: i64,
        now: i64,
    ) -> DbResult<Result<ApiTokenRecord, ChildTokenRefusal>> {
        let txn = self.db.begin_write()?;
        if let Err(refusal) = txn_check_child_token(&txn, &mut record, hours, now)? {
            return Ok(Err(refusal));
        }
        txn_create_token(&txn, &record)?;
        txn.commit()?;
        Ok(Ok(record))
    }

    /// Deletes every issued token that expired by `now`, on every account, and returns how many
    /// live ones it deleted.
    pub fn prune_expired_issued_tokens(&self, now: i64) -> DbResult<u64> {
        let txn = self.db.begin_write()?;
        let mut accounts = BTreeSet::new();
        for table in [API_TOKENS, SUSPENDED_TOKENS] {
            for item in txn.open_table(table)?.iter()? {
                let (_, raw) = item?;
                let token: ApiTokenRecord = serde_json::from_slice(raw.value())?;
                if token.parent_id.is_some() && token.is_expired(now) {
                    accounts.insert(token.account_id);
                }
            }
        }
        let mut removed = 0;
        for account in accounts {
            removed += txn_remove_expired_issued_tokens(&txn, &account, now)?;
        }
        txn.commit()?;
        Ok(removed)
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

    /// Returns the name of the agent role token `id` of `account` was issued as, or none for a
    /// token issued as no role or one that is gone.
    pub fn api_token_role_name(&self, account: &AccountId, id: &str) -> DbResult<Option<String>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(API_TOKENS)?;
        Ok(row_in::<ApiTokenRecord, _>(&tbl, id)?
            .filter(|token| token.account_id == account.as_str())
            .and_then(|token| token.role_name))
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
        let removed = held && txn_remove_token(&txn, account.as_str(), id)? > 0;
        txn.commit()?;
        Ok(removed)
    }

    /// Deletes the tokens that token `parent` of `account` issued, or only the one named by `only`
    /// when it is one of them, and returns how many live tokens it deleted.
    pub fn delete_child_api_tokens(
        &self,
        account: &AccountId,
        parent: &str,
        only: Option<&str>,
    ) -> DbResult<u64> {
        let txn = self.db.begin_write()?;
        let mut removed = 0;
        for child in txn_child_tokens(&txn, account.as_str(), parent)? {
            if only.is_none_or(|id| id == child) {
                removed += txn_remove_token(&txn, account.as_str(), &child)?;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Applies `edit` to `account`'s token `id`, keeping its secret, permissions, boards and
    /// expiry. Returns the stored token, or [`None`] when that account holds no such token. A name
    /// another token of the account holds is refused with [`TokenNameTaken`].
    pub fn edit_api_token(
        &self,
        account: &AccountId,
        id: &str,
        edit: TokenEdit,
    ) -> DbResult<Option<ApiTokenRecord>> {
        let txn = self.db.begin_write()?;
        let held = txn_mmap_get(&txn, TOKENS_BY_ACCOUNT, account.as_str())?
            .iter()
            .any(|token| token == id);
        let stored = if held {
            row_in::<ApiTokenRecord, _>(&txn.open_table(API_TOKENS)?, id)?
        } else {
            None
        };
        let Some(mut record) = stored else {
            return Ok(None);
        };
        if let Some(name) = edit.name.filter(|name| *name != record.name) {
            if txn_token_name_taken(&txn, account.as_str(), &name, Some(id))? {
                return Err(Box::new(TokenNameTaken));
            }
            record.name = name;
        }
        write_one(&txn, API_TOKENS, id, &record)?;
        txn.commit()?;
        Ok(Some(record))
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

    /// Returns the bytes of keys, values and metadata the tables hold plus the bytes of the
    /// attachment files, which deletes reduce even when the file keeps its size. It reads, so it
    /// never waits for a writer.
    ///
    /// The count leaves out the trees that index the tables and redb's own system trees, so it
    /// stays a little under the file's live bytes.
    pub fn stored_bytes(&self) -> DbResult<u64> {
        let txn = self.db.begin_read()?;
        let mut bytes = self.files.bytes()?;
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

    /// Creates a board for `owner` with the standard color palette, Brown and White off, which
    /// joins the set of the API token `joining_token`, or of none when the string is empty.
    pub fn create_board_by(
        &self,
        owner: &AccountId,
        name: &str,
        joining_token: &str,
    ) -> DbResult<String> {
        let txn = self.db.begin_write()?;
        let id = txn_insert_board(&txn, owner, name, joining_token, OverQuota::Refuse)?;
        txn_insert_color_specs(&txn, &id, simple_board_color_specs())?;
        txn.commit()?;
        Ok(id)
    }

    /// Creates a board for `owner` as a person gets one without picking a template, shaped by
    /// [`simple_task_board_snapshot`], which joins the set of the API token `joining_token`, or of
    /// none when the string is empty.
    pub fn create_simple_board_by(
        &self,
        owner: &AccountId,
        name: &str,
        joining_token: &str,
        actor: Actor<'_>,
    ) -> DbResult<String> {
        let txn = self.db.begin_write()?;
        let id = Self::build_from_template(
            &txn,
            owner,
            &simple_task_board_snapshot(),
            name,
            joining_token,
            OverQuota::Refuse,
            actor,
        )?;
        txn.commit()?;
        Ok(id)
    }

    pub fn board(&self, id: &str) -> DbResult<Option<BoardRow>> {
        read_one(&self.db, BOARDS, id)
    }

    /// Finds the board one of whose calendar feeds has the bearer token `raw`. The tokens live in
    /// the board's opaque config bag, which is never serialized to clients.
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
                .feeds()
                .iter()
                .any(|feed| constant_time_eq(feed.token.as_bytes(), raw.as_bytes()))
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

    /// Adds a calendar feed with `label` and the bearer token `token` to a board, in one
    /// transaction, unless the board already holds [`MAX_BOARD_FEEDS`].
    pub fn add_board_feed(&self, id: &str, label: &str, token: &str) -> DbResult<FeedAdd> {
        TextCap::FEED_LABEL.check(label)?;
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Ok(FeedAdd::NoBoard);
        };
        let mut feeds = row.feeds();
        if feeds.len() >= MAX_BOARD_FEEDS {
            return Ok(FeedAdd::Full);
        }
        let feed = BoardFeed {
            id: uuid::Uuid::new_v4().to_string(),
            label: label.to_string(),
            token: token.to_string(),
        };
        feeds.push(feed.clone());
        row.config_json = row.config_with_feeds(&feeds);
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(FeedAdd::Added(feed))
    }

    /// Replaces the seats a board offers, which tasks created later copy, in one transaction.
    /// Returns `false` when the board is unknown.
    pub fn set_board_seats(&self, id: &str, seats: Vec<SeatSlot>) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Ok(false);
        };
        row.seats = seats;
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(true)
    }

    /// Checks a numbering name and prefix and returns both trimmed.
    pub fn validate_numbering(name: &str, prefix: &str) -> Result<(String, String), String> {
        let (name, prefix) = (name.trim(), prefix.trim());
        if name.is_empty() || name.chars().count() > MAX_NUMBERING_NAME_CHARS {
            return Err(format!(
                "the numbering name must be 1 to {MAX_NUMBERING_NAME_CHARS} characters"
            ));
        }
        if prefix.is_empty()
            || prefix.chars().count() > MAX_NUMBERING_PREFIX_CHARS
            || !prefix.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Err(format!(
                "the numbering prefix must be 1 to {MAX_NUMBERING_PREFIX_CHARS} letters or digits"
            ));
        }
        Ok((name.to_string(), prefix.to_string()))
    }

    /// Gives board `board_id` a task numbering that starts at `start`, or at the number after the
    /// highest one the board's tasks already hold under `prefix`, whichever is later, so a number
    /// is never reused. Returns none when the board is unknown, and an error when it is numbered.
    pub fn activate_numbering(
        &self,
        board_id: &str,
        name: &str,
        prefix: &str,
        start: u64,
    ) -> DbResult<Option<Numbering>> {
        let (name, prefix) = Self::validate_numbering(name, prefix)?;
        let txn = self.db.begin_write()?;
        let Some(mut board) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(None);
        };
        if board.numbering().is_some() {
            return Err("the board is numbered already".into());
        }
        let highest = highest_number_in(&txn, board_id, &prefix)?;
        let numbering = Numbering {
            name,
            prefix,
            next: Numbering::resume_at(start, highest)?,
        };
        txn_store_numbering(&txn, &mut board, Some(&numbering))?;
        txn.commit()?;
        Ok(Some(numbering))
    }

    /// Renames the numbering of board `board_id` or changes its prefix, which only new numbers
    /// follow and which never reuse a number the new prefix holds. Returns none when the board has
    /// no numbering.
    pub fn update_numbering(
        &self,
        board_id: &str,
        name: Option<&str>,
        prefix: Option<&str>,
    ) -> DbResult<Option<Numbering>> {
        let txn = self.db.begin_write()?;
        let Some(mut board) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(None);
        };
        let Some(mut numbering) = board.numbering() else {
            return Ok(None);
        };
        let (name, prefix) = Self::validate_numbering(
            name.unwrap_or(&numbering.name),
            prefix.unwrap_or(&numbering.prefix),
        )?;
        if prefix != numbering.prefix {
            let highest = highest_number_in(&txn, board_id, &prefix)?;
            numbering.next = Numbering::resume_at(numbering.next, highest)?;
        }
        numbering.name = name;
        numbering.prefix = prefix;
        txn_store_numbering(&txn, &mut board, Some(&numbering))?;
        txn.commit()?;
        Ok(Some(numbering))
    }

    /// Returns the highest number under `prefix` that a task of board `board_id` holds, in the
    /// board or in its recycle bin, and 0 when it holds none.
    pub fn highest_task_number(&self, board_id: &str, prefix: &str) -> DbResult<u64> {
        highest_number_in(&self.read()?, board_id, prefix)
    }

    /// Removes the numbering of board `board_id` and leaves the numbers its tasks hold. Returns
    /// false when the board has none.
    pub fn delete_numbering(&self, board_id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(mut board) = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)? else {
            return Ok(false);
        };
        if board.numbering().is_none() {
            return Ok(false);
        }
        txn_store_numbering(&txn, &mut board, None)?;
        txn.commit()?;
        Ok(true)
    }

    /// Removes the calendar feed `feed_id` from a board, so its URL stops serving at once, in one
    /// transaction. Returns false when the board has no such feed.
    pub fn remove_board_feed(&self, id: &str, feed_id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Ok(false);
        };
        let mut feeds = row.feeds();
        let before = feeds.len();
        feeds.retain(|feed| feed.id != feed_id);
        if feeds.len() == before {
            return Ok(false);
        }
        row.config_json = row.config_with_feeds(&feeds);
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(true)
    }

    /// Adds a report bookmark to a board in one transaction, unless the board already holds
    /// [`MAX_REPORT_BOOKMARKS`].
    pub fn add_report_bookmark(&self, id: &str, bookmark: ReportBookmark) -> DbResult<BookmarkAdd> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Ok(BookmarkAdd::NoBoard);
        };
        if row.report_bookmarks.len() >= MAX_REPORT_BOOKMARKS {
            return Ok(BookmarkAdd::Full);
        }
        row.report_bookmarks.push(bookmark.clone());
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(BookmarkAdd::Added(bookmark))
    }

    /// Changes the bookmark `bookmark_id` of a board, which `account` saved, in one transaction.
    /// Returns none when the board has no such bookmark of that account.
    pub fn update_report_bookmark(
        &self,
        id: &str,
        bookmark_id: &str,
        account: &str,
        patch: BookmarkPatch,
    ) -> DbResult<Option<ReportBookmark>> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Ok(None);
        };
        let Some(held) = row
            .report_bookmarks
            .iter_mut()
            .find(|held| held.id == bookmark_id && held.owner_id == account)
        else {
            return Ok(None);
        };
        if let Some(name) = patch.name {
            held.name = name;
        }
        if let Some(shared) = patch.shared {
            held.shared = shared;
        }
        if let Some(shortcut) = patch.shortcut {
            held.shortcut = shortcut;
        }
        if let Some(filters) = patch.filters {
            held.filters = filters;
        }
        let updated = held.clone();
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(Some(updated))
    }

    /// Removes the bookmark `bookmark_id` of a board, which `account` saved, in one transaction.
    /// Returns false when the board has no such bookmark of that account.
    pub fn remove_report_bookmark(
        &self,
        id: &str,
        bookmark_id: &str,
        account: &str,
    ) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<BoardRow>(&txn, BOARDS, id)? else {
            return Ok(false);
        };
        let before = row.report_bookmarks.len();
        row.report_bookmarks
            .retain(|held| !(held.id == bookmark_id && held.owner_id == account));
        if row.report_bookmarks.len() == before {
            return Ok(false);
        }
        txn_store_board(&txn, &row, OverQuota::Allow)?;
        txn.commit()?;
        Ok(true)
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
        Ok(first_board(&self.owner_boards(owner)?))
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
        let attachment_ids = txn_remove_board(&txn, &board)?;
        txn.commit()?;
        self.files.remove(&attachment_ids);
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
        let id = txn_insert_column(
            &txn,
            board_id,
            name,
            wip_limit,
            is_done,
            None,
            OverQuota::Refuse,
        )?;
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
            if let Some(stage) = patch.stage {
                column.stage = stage;
            }
            if let Some(description) = patch.description {
                TextCap::DESCRIPTION.check(description)?;
                column.description = description.to_string();
            }
            if let Some(collapsed) = patch.collapsed {
                column.collapsed = collapsed;
            }
            if let Some(watch) = patch.watch {
                column.watchers.retain(|watcher| watcher != watch.account);
                if watch.watched {
                    column.watchers.push(watch.account.to_string());
                    TextCap::COLUMN_WATCHERS.check_count(&column.watchers)?;
                }
            }
            if let Some(config_json) = patch.config_json {
                TextCap::CONFIG.check(config_json)?;
                column.config_json = config_json.to_string();
            }
            txn_store_owned(&txn, COLUMNS, id, &column, OverQuota::Refuse)?;
            if let Some(is_blocked) = patch.is_blocked {
                txn_set_blocked_column(&txn, &column.board_id, id, is_blocked)?;
            }
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

    /// Reorders a column within its board and renumbers the positions densely. A move that would
    /// split a limit group's columns apart is refused.
    pub fn move_column(&self, id: &str, position: i64) -> DbResult<ColumnMove> {
        let txn = self.db.begin_write()?;
        let Some(column) = txn_read_one::<ColumnRow>(&txn, COLUMNS, id)? else {
            return Ok(ColumnMove::NotFound);
        };
        let mut columns: Vec<ColumnRow> =
            txn_indexed_rows(&txn, COLUMNS_BY_BOARD, &column.board_id, COLUMNS)?;
        columns.retain(|column| column.id != id);
        columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        let at = position.clamp(0, columns.len() as i64) as usize;
        columns.insert(at, column);
        let order: Vec<&str> = columns.iter().map(|column| column.id.as_str()).collect();
        if txn_order_splits_group(&txn, &columns[at].board_id, &order)? {
            return Ok(ColumnMove::BreaksGroup);
        }
        txn_sort_groups(&txn, &columns[at].board_id, &order)?;
        for (index, mut row) in columns.into_iter().enumerate() {
            row.position = index as i64;
            txn_store_owned(&txn, COLUMNS, &row.id, &row, OverQuota::Allow)?;
        }
        txn.commit()?;
        Ok(ColumnMove::Moved)
    }

    /// Replace a board's whole color palette with a copy of another board's
    /// (KanbanFlow parity: Colors tab "Copy from board"). Old rows are
    /// removed and re-inserted under new ids, preserving each source row's
    /// value, label, description, enabled flag, default flag, and order.
    /// Tasks on the target board keep their color by value (old color id -> new color id for the
    /// same value). The copy is refused with [`PaletteStrandsColor`] while a task uses a custom
    /// color the source lacks by value and hex, so no task is stranded or recolored.
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
            let (background_hex, border_hex, light_hex) = if is_custom_color(&color.value) {
                (
                    color.background_hex.clone(),
                    color.border_hex.clone(),
                    color.light_hex.clone(),
                )
            } else {
                let (background, border, light, _) = standard_color(&color.value)
                    .ok_or_else(|| format!("unknown color value: {}", color.value))?;
                (
                    background.to_string(),
                    border.to_string(),
                    light.to_string(),
                )
            };
            let id = Uuid::new_v4().to_string();
            new_id_by_value.insert(color.value.clone(), id.clone());
            copies.push(ColorRow {
                id,
                board_id: target_board_id.to_string(),
                value: color.value.clone(),
                label: color.label.clone(),
                description: color.description.clone(),
                background_hex,
                border_hex,
                light_hex,
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
        for task in &tasks {
            let used = task
                .color_id
                .as_deref()
                .and_then(|color_id| old_target.iter().find(|color| color.id == color_id))
                .filter(|color| is_custom_color(&color.value));
            if let Some(used) = used
                && !copies.iter().any(|copy| {
                    copy.value == used.value && copy.background_hex == used.background_hex
                })
            {
                return Err(Box::new(PaletteStrandsColor {
                    value: used.value.clone(),
                }));
            }
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
                    txn_store_task(&txn, &mut tasks_tbl, &task, OverQuota::Allow, Actor::SYSTEM)?;
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
        let held: Vec<TaskRow> = txn_indexed_rows(&txn, TASKS_BY_COLUMN, id, TASKS)?;
        let work = held.iter().filter(|task| task.is_work()).count();
        if work > 0 {
            return Ok(ColumnDeleteOutcome::RefusedHasTasks(work));
        }
        if !held.is_empty() {
            let mut siblings: Vec<ColumnRow> =
                txn_indexed_rows(&txn, COLUMNS_BY_BOARD, &column.board_id, COLUMNS)?;
            siblings.retain(|sibling| sibling.id != id);
            siblings.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
            let first = siblings
                .first()
                .ok_or_else(|| format!("no column left beside: {id}"))?;
            let detail = format!("Moved from {} to {}", column.name, first.name);
            for epic in &held {
                let guards = MoveGuards {
                    event: Some(&detail),
                    skip_epic_guard: true,
                    ..MoveGuards::default()
                };
                let outcome = Self::move_in(
                    &txn,
                    &epic.id,
                    &first.id,
                    epic.swimlane_id.as_deref(),
                    f64::MAX,
                    guards,
                    Actor::SYSTEM,
                )?;
                if outcome != MoveOutcome::Moved {
                    return Err(format!("epic did not move: {}", epic.id).into());
                }
            }
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
                    txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow, Actor::SYSTEM)?;
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
        for mut hook in
            txn_indexed_rows::<WebhookRow>(&txn, WEBHOOKS_BY_BOARD, &column.board_id, WEBHOOKS)?
        {
            if hook.column_id.as_deref() == Some(id) {
                hook.column_id = None;
                hook.enabled = false;
                txn_store_owned(&txn, WEBHOOKS, &hook.id, &hook, OverQuota::Allow)?;
            }
        }
        for address in txn_indexed_rows::<InboundAddressRow>(
            &txn,
            INBOUND_BY_BOARD,
            &column.board_id,
            INBOUND_ADDRESSES,
        )? {
            if address.column_id == id {
                txn_remove_inbound(&txn, &address.id)?;
                mmap_remove(&txn, INBOUND_BY_BOARD, &column.board_id, &address.id)?;
            }
        }
        txn_leave_limit_groups(&txn, &column.board_id, id)?;
        txn_remove_owned::<ColumnRow>(&txn, COLUMNS, id)?;
        mmap_remove(&txn, COLUMNS_BY_BOARD, &column.board_id, id)?;
        txn.commit()?;
        Ok(ColumnDeleteOutcome::Deleted)
    }

    pub fn count_tasks_in_column(&self, column_id: &str) -> DbResult<usize> {
        let mut work = 0;
        for id in mmap_get(&self.db, TASKS_BY_COLUMN, column_id)? {
            let task: Option<TaskRow> = read_one(&self.db, TASKS, &id)?;
            work += usize::from(task.is_some_and(|task| task.is_work()));
        }
        Ok(work)
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
                push_event(
                    &mut task,
                    HistoryKind::SwimlaneMerged,
                    detail.clone(),
                    actor,
                );
                txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow, actor)?;
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
                txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow, Actor::SYSTEM)?;
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
            .map_err(|message| -> Box<dyn Error> { message.into() })?;
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
                hex: None,
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

    /// Checks the hex of a custom color and returns it as lowercase `#rrggbb`.
    ///
    /// A hex with a low contrast to black and gray text is refused with the message the board shows,
    /// which ends with a lighter hex to use instead.
    pub fn validate_custom_hex(hex: &str) -> Result<String, String> {
        let hex = normalize_hex(hex).ok_or_else(|| format!("not a hex color: {hex}"))?;
        let contrast = text_contrast(&hex).unwrap_or(f64::MAX);
        if contrast >= MIN_TEXT_CONTRAST {
            return Ok(hex);
        }
        let lighter = lighter_hex(&hex).unwrap_or_default();
        Err(format!(
            "The color {} has a low contrast to black and gray text. Use the lighter {lighter}",
            hex.to_uppercase()
        ))
    }

    /// Adds a custom color with background `hex` to a board under the lowest free internal id from
    /// `custom1` to `custom10`, and returns its id.
    ///
    /// A custom color is never deleted, so its internal id is never free again and the tenth
    /// color is the last.
    pub fn create_custom_color(
        &self,
        board_id: &str,
        hex: &str,
        label: &str,
        description: Option<&str>,
    ) -> DbResult<String> {
        let hex = Self::validate_custom_hex(hex)?;
        Self::validate_color_label(label)
            .map_err(|message| -> Box<dyn Error> { message.into() })?;
        TextCap::COLOR_DESCRIPTION.check(description.unwrap_or_default())?;
        let txn = self.db.begin_write()?;
        let colors = txn_board_colors(&txn, board_id)?;
        let value = (1..=MAX_CUSTOM_COLORS)
            .map(custom_color_value)
            .find(|value| colors.iter().all(|color| &color.value != value))
            .ok_or_else(|| format!("a board holds at most {MAX_CUSTOM_COLORS} custom colors"))?;
        let id = txn_insert_color(
            &txn,
            board_id,
            NewColor {
                value: &value,
                hex: Some(&hex),
                label: label.trim(),
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

    /// Changes the background of custom color `id` to `hex`. Returns false when the color is
    /// unknown or is not a custom one.
    pub fn set_custom_color_hex(&self, id: &str, hex: &str) -> DbResult<bool> {
        let hex = Self::validate_custom_hex(hex)?;
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<ColorRow>(&txn, TASK_COLORS, id)? else {
            return Ok(false);
        };
        if !is_custom_color(&row.value) {
            return Ok(false);
        }
        let (background, border, light) =
            custom_color_hexes(&hex).ok_or_else(|| format!("not a hex color: {hex}"))?;
        row.background_hex = background;
        row.border_hex = border;
        row.light_hex = light;
        txn_store_owned(&txn, TASK_COLORS, id, &row, OverQuota::Refuse)?;
        txn.commit()?;
        Ok(true)
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
                .map_err(|message| -> Box<dyn Error> { message.into() })?;
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
        if is_custom_color(&color.value) {
            return Ok(ColorDeleteOutcome::RefusedCustom);
        }
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

    /// Returns the column names of the snapshot of template `id`, left to right.
    pub fn template_columns(&self, id: &str) -> DbResult<Vec<String>> {
        let txn = self.read()?;
        let snapshot = txn_template_snapshot(&txn, id)?;
        let mut columns: Vec<(i64, String)> = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot["columns"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|column| {
                let name = column["name"].as_str()?;
                Some((column["position"].as_i64().unwrap_or(0), name.to_owned()))
            })
            .collect();
        columns.sort_by_key(|(position, _)| *position);
        Ok(columns.into_iter().map(|(_, name)| name).collect())
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
        let snapshot = self.write_template_snapshot(
            board_id,
            &self.board_task_ids(board_id)?,
            CopyTasks::ClearMembers,
        )?;
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
    /// order, skipping any that are gone, with their members and responsible actor when `tasks`
    /// is [`CopyTasks::Keep`]. The snapshot is written one task at a time and refused with [`QuotaExceeded`] once
    /// it passes [`MAX_TEMPLATE_ROW_BYTES`].
    fn write_template_snapshot(
        &self,
        board_id: &str,
        task_ids: &[String],
        tasks: CopyTasks,
    ) -> DbResult<Vec<u8>> {
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
                    "stage": column.stage,
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
        let board = self.board(board_id)?;
        let due_time = board
            .as_ref()
            .map(|board| board.due_time().format("%H:%M").to_string());
        let label_settings = board
            .as_ref()
            .map(|board| LabelSettings::from_config(&board.config_json).to_config_value());
        let seats = board
            .as_ref()
            .map(|board| board.seats.clone())
            .unwrap_or_default();

        // The snapshot is written one task at a time and refused once it passes the cap
        let mut snapshot = Vec::new();
        snapshot.extend_from_slice(b"{\"due_time\":");
        serde_json::to_writer(&mut snapshot, &due_time)?;
        snapshot.extend_from_slice(format!(",\"{}\":", CONFIG_KEY).as_bytes());
        serde_json::to_writer(&mut snapshot, &label_settings)?;
        for (key, value) in [
            ("colors", colors),
            ("columns", columns),
            ("swimlanes", swimlanes),
        ] {
            snapshot.extend_from_slice(format!(",\"{key}\":").as_bytes());
            serde_json::to_writer(&mut snapshot, &value)?;
        }
        snapshot.extend_from_slice(b",\"seats\":");
        serde_json::to_writer(&mut snapshot, &seats)?;
        snapshot.extend_from_slice(b",\"tasks\":[");
        let mut wrote_task = false;
        for task_id in task_ids {
            let Some(task) = self.task(task_id)? else {
                continue;
            };
            let mut subtasks = task.subtasks.clone();
            if tasks != CopyTasks::Keep {
                for subtask in &mut subtasks {
                    subtask.member_ids.clear();
                }
            }
            let mut entry = serde_json::json!({
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
                "subtasks": subtasks,
                "labels": task.labels,
                "kind": task.kind,
                "closed": task.closed,
            });
            if tasks == CopyTasks::Keep {
                entry["member_ids"] = serde_json::json!(task.member_ids);
                entry["responsible"] = serde_json::json!(task.responsible);
            }
            if wrote_task {
                snapshot.push(b',');
            }
            serde_json::to_writer(&mut snapshot, &entry)?;
            wrote_task = true;
            refuse_template_past(&snapshot)?;
        }
        snapshot.extend_from_slice(b"]}");
        refuse_template_past(&snapshot)?;
        Ok(snapshot)
    }

    /// Builds a new board named `name` for `owner` that copies board `source_id`: its colors,
    /// columns with their limits, swimlanes and due time, and the tasks `tasks` names, all in one
    /// write transaction. Returns `None` when the source is unknown.
    pub fn copy_board_by(
        &self,
        owner: &AccountId,
        source_id: &str,
        name: &str,
        tasks: CopyTasks,
        joining_token: &str,
        actor: Actor<'_>,
    ) -> DbResult<Option<String>> {
        if !self.board_exists(source_id)? {
            return Ok(None);
        }
        let mut task_ids = match tasks {
            CopyTasks::None => Vec::new(),
            CopyTasks::Keep | CopyTasks::ClearMembers => self.board_task_ids(source_id)?,
        };
        task_ids.truncate(BOARD_COPY_TASK_CAP);
        let snapshot = self.write_template_snapshot(source_id, &task_ids, tasks)?;
        let snapshot: serde_json::Value = serde_json::from_slice(&snapshot)?;
        let txn = self.db.begin_write()?;
        let board_id = Self::build_from_template(
            &txn,
            owner,
            &snapshot,
            name,
            joining_token,
            OverQuota::Refuse,
            actor,
        )?;
        let held = txn_board_task_count(&txn, &board_id)?;
        txn_refuse_board_over_quota(&txn, &board_id, Quota::TasksPerBoard, held)?;
        txn.commit()?;
        Ok(Some(board_id))
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
        self.apply_template_by(owner, template_id, new_board_name, "", Actor::SYSTEM)
    }

    /// [`Db::apply_template`], with the new board joining the set of the API token
    /// `joining_token`, or of none when the string is empty.
    pub fn apply_template_by(
        &self,
        owner: &AccountId,
        template_id: &str,
        new_board_name: &str,
        joining_token: &str,
        actor: Actor<'_>,
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
            joining_token,
            OverQuota::Refuse,
            actor,
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
        joining_token: &str,
        over: OverQuota,
        actor: Actor<'_>,
    ) -> DbResult<String> {
        let board_id = txn_insert_board(txn, owner, name, joining_token, over)?;

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

        if let Some(settings) = snapshot
            .get(CONFIG_KEY)
            .and_then(|value| serde_json::from_value::<LabelSettings>(value.clone()).ok())
        {
            let mut updates = serde_json::Map::new();
            updates.insert(CONFIG_KEY.to_string(), settings.to_config_value());
            txn_merge_board_config(txn, &board_id, updates)?;
        }

        if let Some(seats) = snapshot
            .get("seats")
            .and_then(|value| serde_json::from_value::<Vec<SeatSlot>>(value.clone()).ok())
            .and_then(|seats| SeatSlot::normalize_all(seats).ok())
            && let Some(mut board) = txn_read_one::<BoardRow>(txn, BOARDS, &board_id)?
        {
            board.seats = seats;
            txn_store_board(txn, &board, over)?;
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
                let hex = color
                    .get("background_hex")
                    .and_then(|value| value.as_str())
                    .filter(|hex| is_custom_color(value) && normalize_hex(hex).is_some());
                if hex.is_none() && standard_color(value).is_none() {
                    continue;
                }
                txn_insert_color(
                    txn,
                    &board_id,
                    NewColor {
                        value,
                        hex,
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
                let name = column
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("To-do");
                let stage = match column.get("stage") {
                    Some(stored) => serde_json::from_value(stored.clone()).ok().flatten(),
                    None => ColumnStage::from_legacy_name(name),
                };
                txn_insert_column(
                    txn,
                    &board_id,
                    name,
                    column.get("wip_limit").and_then(|value| value.as_i64()),
                    column
                        .get("is_done")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                    stage,
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
        Self::apply_template_tasks(txn, &board_id, snapshot, over, actor)?;
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
        actor: Actor<'_>,
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
        let seats: Vec<Seat> = txn_read_one::<BoardRow>(txn, BOARDS, board_id)?
            .map(|board| board.seats.iter().map(Seat::open).collect())
            .unwrap_or_default();
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
                member_ids: task
                    .get("member_ids")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_default(),
                responsible: task
                    .get("responsible")
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
                grouping_date: None,
                timeline_start: None,
                timeline_end: None,
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
                points: None,
                number: None,
                column_entered_at: Some(now.clone()),
                comments: Vec::new(),
                attachments: Vec::new(),
                cover_id: None,
                kind: task
                    .get("kind")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_default(),
                closed: task
                    .get("closed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_default(),
                history: Vec::new(),
                updated_at: Some(now),
                metadata: None,
                links: Vec::new(),
                claim: None,
                claim_fence: 0,
                owner: None,
                seats: seats.clone(),
                fields: serde_json::Map::new(),
            };
            txn_store_task(txn, &mut txn.open_table(TASKS)?, &row, over, actor)?;
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

    /// Returns the minutes `account` logged on a task.
    pub fn task_minutes_of(&self, account: &AccountId, task_id: &str) -> DbResult<i64> {
        let mut total = 0;
        for entry_id in mmap_get(&self.db, ENTRIES_BY_TASK, task_id)? {
            if let Some(entry) = self.entry(&entry_id)?
                && entry.account_id == account.as_str()
            {
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
            responsible: None,
            grouping_date: None,
            timeline_start: None,
            timeline_end: None,
            watched: false,
            labels: Vec::new(),
            due_at: None,
            due_repeat: None,
            due_done: false,
            due_target_column_id: None,
            estimate_hours: None,
            points: None,
            number: None,
            column_entered_at: Some(now.clone()),
            comments: Vec::new(),
            attachments: Vec::new(),
            cover_id: None,
            kind: TaskKind::Task,
            closed: false,
            history: Vec::new(),
            updated_at: Some(now),
            metadata: None,
            links: Vec::new(),
            claim: None,
            claim_fence: 0,
            owner: None,
            seats: Vec::new(),
            fields: serde_json::Map::new(),
        }
    }

    /// Store a prepared task row at the top or bottom of its cell in one
    /// write, as the server's own change; returns its id. A `color_id` deleted since fails with
    /// [`UNKNOWN_COLOR`]. Also backfills the board's color palette for
    /// boards created before colors existed.
    pub fn insert_task(&self, row: TaskRow, placement: NewTaskPlacement) -> DbResult<String> {
        Ok(self
            .insert_task_checked(row, placement, Actor::SYSTEM)?
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
        actor: Actor<'_>,
    ) -> DbResult<Result<String, String>> {
        let txn = self.db.begin_write()?;
        if let Some(column) = txn_read_one::<ColumnRow>(&txn, COLUMNS, &row.column_id)? {
            let held = txn_board_task_count(&txn, &column.board_id)?;
            txn_refuse_board_over_quota(&txn, &column.board_id, Quota::TasksPerBoard, held + 1)?;
            txn_ensure_board_colors(&txn, &column.board_id)?;
            if row.seats.is_empty()
                && let Some(board) = txn_read_one::<BoardRow>(&txn, BOARDS, &column.board_id)?
            {
                row.seats = board.seats.iter().map(Seat::open).collect();
            }
            if row.number.is_none() {
                row.number = txn_take_task_number(&txn, &column.board_id)?;
            }

            // A task created in a done column is complete from the start
            if column.is_done && row.is_work() && row.completed_at.is_none() {
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
        txn_store_task(
            &txn,
            &mut txn.open_table(TASKS)?,
            &row,
            OverQuota::Refuse,
            actor,
        )?;
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
        mutate_task_by(&self.db, id, Actor::SYSTEM, |task| {
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
            token_id: None,
            member_ids: Vec::new(),
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
        let touched = self.apply_task_change(task_id, Actor::SYSTEM, |task| {
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
        let removed = self.apply_task_change(task_id, Actor::SYSTEM, |task| {
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
            task.set_members(ids.clone());
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
        mutate_task_by(&self.db, task_id, Actor::SYSTEM, |task| {
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
            reactions: Vec::new(),
            question: None,
            parent_token: actor.parent_token.cloned(),
            role_id: actor.role_id.map(str::to_string),
            role_name: actor.role_name.map(str::to_string),
            label: actor.label.map(str::to_string),
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
            push_event(&mut task, HistoryKind::CommentAdded, detail, actor);
            task.updated_at = Some(Utc::now().to_rfc3339());
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, actor)?;
        }
        txn.commit()?;
        Ok(FencedWrite::Written(comment))
    }

    /// Appends a question comment, adds the [`NEEDS_HUMAN_LABEL`] label and moves the task to the
    /// `blocked` column, with the comment, label and move events, in one write. The move applies
    /// the fence, the open-children refusal for an epic and the target's WIP limit as any move
    /// does, and a refusal writes nothing.
    pub fn post_question(
        &self,
        task_id: &str,
        new: NewComment<'_>,
        question: CommentQuestion,
        to: QuestionMove<'_>,
        actor: Actor<'_>,
    ) -> DbResult<QuestionWrite> {
        let comment = TaskComment {
            id: Uuid::new_v4().to_string(),
            author: new.author.to_string(),
            body: new.body.to_string(),
            created_at: Utc::now().to_rfc3339(),
            actor_kind: new.actor_kind,
            token_name: new.token_name.map(str::to_string),
            token_id: new.token_id.filter(|id| !id.is_empty()).map(str::to_string),
            edited_at: None,
            kind: Some(QUESTION_KIND.to_string()),
            reactions: Vec::new(),
            parent_token: actor.parent_token.cloned(),
            role_id: actor.role_id.map(str::to_string),
            role_name: actor.role_name.map(str::to_string),
            label: actor.label.map(str::to_string),
            question: Some(question),
        };
        let txn = self.db.begin_write()?;
        let owner = task_owner_in(&txn, task_id)?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tbl, task_id)? else {
                return Ok(QuestionWrite::NotFound);
            };
            if !task.fence_allows(to.fence) {
                return Ok(QuestionWrite::StaleFence);
            }
            if let Some(owner) = &owner {
                let after = task.comments.len() + 1;
                txn_refuse_over_quota(&txn, owner, Quota::CommentsPerTask, after)?;
            }
            task.comments.push(comment.clone());
            let detail = format!("{} asked a question", new.author);
            push_event(&mut task, HistoryKind::CommentAdded, detail, actor);
            add_needs_human(&mut task, actor);
            task.updated_at = Some(Utc::now().to_rfc3339());
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, actor)?;
        }
        let moved = Self::move_for_question(&txn, task_id, to, actor)?;
        QuestionWrite::commit_if_moved(txn, moved, comment)
    }

    /// Appends a `blocked` comment holding the reason, adds the label of `tier` and removes the
    /// label of the other tier, with the comment and label events, in one write. The task's
    /// column stays. With a `fence`, a live claim holding another fence refuses the write.
    pub fn escalate_task(
        &self,
        task_id: &str,
        new: NewComment<'_>,
        tier: EscalationTier,
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
            kind: Some(BLOCKED_KIND.to_string()),
            reactions: Vec::new(),
            question: None,
            parent_token: actor.parent_token.cloned(),
            role_id: actor.role_id.map(str::to_string),
            role_name: actor.role_name.map(str::to_string),
            label: actor.label.map(str::to_string),
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
            let detail = format!("{} escalated the card", new.author);
            push_event(&mut task, HistoryKind::CommentAdded, detail, actor);
            set_tier_label(&mut task, tier, actor);
            task.updated_at = Some(Utc::now().to_rfc3339());
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, actor)?;
        }
        txn.commit()?;
        Ok(FencedWrite::Written(comment))
    }

    /// Records the answer to question comment `comment_id`, removes the [`NEEDS_HUMAN_LABEL`]
    /// label and moves the task to the destination of the choice `answer` names, with the answer,
    /// label and move events, in one write. While another question on the task is still open, the
    /// answer is recorded and the label and the task's column stay as they are. Only an open
    /// question takes an answer, and a refused move writes nothing.
    pub fn answer_question(
        &self,
        task_id: &str,
        comment_id: &str,
        answer: QuestionAnswer,
        position: f64,
        actor: Actor<'_>,
    ) -> DbResult<QuestionWrite> {
        let choice = answer.choice;
        let txn = self.db.begin_write()?;
        let destination;
        let answered;
        {
            let mut tbl = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tbl, task_id)? else {
                return Ok(QuestionWrite::NotFound);
            };
            let Some(comment) = task.comments.iter_mut().find(|c| c.id == comment_id) else {
                return Ok(QuestionWrite::NotFound);
            };
            let Some(question) = comment.question.as_mut() else {
                return Ok(QuestionWrite::NoQuestion);
            };
            if question.answer.is_some() {
                return Ok(QuestionWrite::AlreadyAnswered);
            }
            let Some(picked) = question.choice(choice) else {
                return Ok(QuestionWrite::BadChoice);
            };
            let picked = picked.column_id.clone();
            let detail = format!("{} answered choice {choice}", answer.answered_by);
            question.answer = Some(answer);
            answered = comment.clone();
            let others_open = task
                .comments
                .iter()
                .any(|c| c.question.as_ref().is_some_and(|q| q.answer.is_none()));
            destination = (!others_open).then_some(picked);
            push_event(&mut task, HistoryKind::CommentAdded, detail, actor);
            if !others_open {
                remove_needs_human(&mut task, actor);
            }
            task.updated_at = Some(Utc::now().to_rfc3339());
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, actor)?;
        }
        let Some(column_id) = destination else {
            txn.commit()?;
            return Ok(QuestionWrite::Done(Box::new(answered)));
        };
        let to = QuestionMove {
            column_id: &column_id,
            position,
            fence: None,
        };
        let moved = Self::move_for_question(&txn, task_id, to, actor)?;
        QuestionWrite::commit_if_moved(txn, moved, answered)
    }

    /// Moves a task into `column_id` inside the caller's transaction with the history event a
    /// move logs and the target's WIP limit enforced.
    fn move_for_question(
        txn: &WriteTransaction,
        task_id: &str,
        to: QuestionMove<'_>,
        actor: Actor<'_>,
    ) -> DbResult<MoveOutcome> {
        let QuestionMove {
            column_id,
            position,
            fence,
        } = to;
        let Some(task) = txn_read_one::<TaskRow>(txn, TASKS, task_id)? else {
            return Ok(MoveOutcome::NotFound);
        };
        let target: ColumnRow = txn_read_one(txn, COLUMNS, column_id)?
            .ok_or_else(|| format!("unknown column: {column_id}"))?;
        let source: Option<ColumnRow> = txn_read_one(txn, COLUMNS, &task.column_id)?;
        let from = source
            .as_ref()
            .map_or("a column", |column| column.name.as_str());
        let detail = format!("Moved from {from} to {}", target.name);
        let guards = MoveGuards {
            event: (task.column_id != column_id).then_some(detail.as_str()),
            fence,
            enforce_wip: true,
            batch: &[],
            skip_epic_guard: false,
        };
        Self::move_in(txn, task_id, column_id, None, position, guards, actor)
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
        let outcome = self.apply_task_change(task_id, actor, |task| {
            let Some(comment) = task.comments.iter().find(|c| c.id == comment_id) else {
                return (CommentWrite::NotFound, false);
            };
            if !comment_is_by(comment, owner) {
                return (CommentWrite::NotOwner, false);
            }
            task.comments.retain(|c| c.id != comment_id);
            push_event(
                task,
                HistoryKind::CommentDeleted,
                "Deleted a comment".into(),
                actor,
            );
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
        let outcome = self.apply_task_change(task_id, actor, |task| {
            let Some(comment) = task.comments.iter_mut().find(|c| c.id == comment_id) else {
                return (CommentWrite::NotFound, false);
            };
            if !comment_is_by(comment, owner) {
                return (CommentWrite::NotOwner, false);
            }
            comment.body = body.to_string();
            comment.edited_at = Some(Utc::now().to_rfc3339());
            let edited = comment.clone();
            push_event(
                task,
                HistoryKind::CommentEdited,
                "Edited a comment".into(),
                actor,
            );
            (CommentWrite::Done(edited), true)
        })?;
        Ok(outcome.unwrap_or(CommentWrite::NotFound))
    }

    /// Adds the `handle` account's `emoji` reaction to a comment, or takes it back when the
    /// account already gave it, and returns the comment.
    pub fn toggle_comment_reaction(
        &self,
        task_id: &str,
        comment_id: &str,
        emoji: &str,
        handle: &str,
        actor: Actor<'_>,
    ) -> DbResult<CommentWrite<TaskComment>> {
        let outcome = self.apply_task_change(task_id, actor, |task| {
            let Some(comment) = task.comments.iter_mut().find(|c| c.id == comment_id) else {
                return (CommentWrite::NotFound, false);
            };
            let before = comment.reactions.len();
            comment
                .reactions
                .retain(|r| !(r.emoji == emoji && r.handle == handle));
            if comment.reactions.len() == before {
                comment.reactions.push(CommentReaction {
                    emoji: emoji.to_string(),
                    handle: handle.to_string(),
                });
            }
            (CommentWrite::Done(comment.clone()), true)
        })?;
        Ok(outcome.unwrap_or(CommentWrite::NotFound))
    }

    /// Link `task_id` to `target_id` in one transaction. A `blocks` link is stored as `blocked_by`
    /// on the target, and a `child` link as `parent` on the target; asking for a link that already
    /// exists returns it unchanged. A new link records the API `token_id` that created it.
    pub fn add_task_link(
        &self,
        task_id: &str,
        kind: LinkKind,
        target_id: &str,
        token_id: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<LinkAddOutcome> {
        let txn = self.db.begin_write()?;
        let outcome = Self::add_link_in(&txn, task_id, kind, target_id, token_id, actor)?;
        if matches!(outcome, LinkAddOutcome::Added(_)) {
            txn.commit()?;
        }
        Ok(outcome)
    }

    /// Links `task_id` to `target_id` inside the caller's write transaction, which the caller
    /// commits when the outcome is [`LinkAddOutcome::Added`] and drops otherwise.
    fn add_link_in(
        txn: &WriteTransaction,
        task_id: &str,
        kind: LinkKind,
        target_id: &str,
        token_id: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<LinkAddOutcome> {
        let (holder_id, stored_kind, stored_target) = match kind {
            LinkKind::Blocks => (target_id, LinkKind::BlockedBy, task_id),
            LinkKind::Child => (target_id, LinkKind::Parent, task_id),
            other => (task_id, other, target_id),
        };
        let as_requested = |link: &TaskLink| TaskLink {
            kind,
            target_id: target_id.to_string(),
            ..link.clone()
        };
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
            let is_relation =
                |kind: LinkKind| matches!(kind, LinkKind::BlockedBy | LinkKind::Related);
            let relates = |task: &TaskRow, target: &str| {
                task.links
                    .iter()
                    .any(|link| is_relation(link.kind) && link.target_id == target)
            };
            if is_relation(stored_kind)
                && (relates(&holder, stored_target) || relates(&other, holder_id))
            {
                return Ok(LinkAddOutcome::Duplicate);
            }
            if stored_kind == LinkKind::BlockedBy
                && blocked_by_reaches(&tbl, stored_target, holder_id)?
            {
                return Ok(LinkAddOutcome::Cycle);
            }
            if stored_kind == LinkKind::Parent {
                let has_parent =
                    |task: &TaskRow| task.links.iter().any(|link| link.kind == LinkKind::Parent);
                if has_parent(&holder) {
                    return Ok(LinkAddOutcome::HasParent);
                }
                if parent_chain_reaches(&tbl, stored_target, holder_id)? {
                    return Ok(LinkAddOutcome::ParentCycle);
                }
                if other.kind != TaskKind::Epic {
                    return Ok(LinkAddOutcome::NotEpic);
                }
            }
            let now = Utc::now().to_rfc3339();
            let link = TaskLink {
                id: Uuid::new_v4().to_string(),
                kind: stored_kind,
                target_id: stored_target.to_string(),
                created_at: now.clone(),
                board_id: None,
                token_id: token_id.map(str::to_string),
            };
            holder.links.push(link.clone());
            let detail = link_event_detail(&link, Some(&other.name), |detail| {
                format!("Added link {detail}")
            });
            push_event(&mut holder, HistoryKind::LinkAdded, detail, actor);
            holder.updated_at = Some(now);
            txn_store_task(txn, &mut tbl, &holder, OverQuota::Refuse, actor)?;
            link
        };
        mmap_insert(txn, LINKS_BY_TARGET, stored_target, holder_id)?;
        Ok(LinkAddOutcome::Added(as_requested(&link)))
    }

    /// Replace the `parent` link of `task_id` with one to `umbrella_id` in one transaction, so a
    /// failure leaves the old umbrella in place. Naming the current umbrella returns its link
    /// unchanged.
    pub fn change_umbrella(
        &self,
        task_id: &str,
        umbrella_id: &str,
        actor: Actor<'_>,
    ) -> DbResult<UmbrellaChange> {
        let txn = self.db.begin_write()?;
        let link = {
            let mut tbl = txn.open_table(TASKS)?;
            let (Some(mut task), Some(umbrella)) =
                (task_in(&tbl, task_id)?, task_in(&tbl, umbrella_id)?)
            else {
                return Ok(UmbrellaChange::NotFound);
            };
            let Some(index) = task
                .links
                .iter()
                .position(|link| link.kind == LinkKind::Parent)
            else {
                return Ok(UmbrellaChange::NoUmbrella);
            };
            if task.links[index].target_id == umbrella_id {
                return Ok(UmbrellaChange::Changed(task.links[index].clone()));
            }
            if parent_chain_reaches(&tbl, umbrella_id, task_id)? {
                return Ok(UmbrellaChange::ParentCycle);
            }
            if umbrella.kind != TaskKind::Epic {
                return Ok(UmbrellaChange::NotEpic);
            }
            let old = task.links.remove(index);
            let old_name = task_in(&tbl, &old.target_id)?.map(|old_umbrella| old_umbrella.name);
            let removed = link_event_detail(&old, old_name.as_deref(), |detail| {
                format!("Removed link {detail}")
            });
            push_event(&mut task, HistoryKind::LinkRemoved, removed, actor);
            let now = Utc::now().to_rfc3339();
            let link = TaskLink {
                id: Uuid::new_v4().to_string(),
                kind: LinkKind::Parent,
                target_id: umbrella_id.to_string(),
                created_at: now.clone(),
                board_id: None,
                token_id: None,
            };
            task.links.push(link.clone());
            let added = link_event_detail(&link, Some(&umbrella.name), |detail| {
                format!("Added link {detail}")
            });
            push_event(&mut task, HistoryKind::LinkAdded, added, actor);
            task.updated_at = Some(now);
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, actor)?;
            if !task
                .links
                .iter()
                .any(|other| other.target_id == old.target_id)
            {
                mmap_remove(&txn, LINKS_BY_TARGET, &old.target_id, task_id)?;
            }
            link
        };
        mmap_insert(&txn, LINKS_BY_TARGET, umbrella_id, task_id)?;
        txn.commit()?;
        Ok(UmbrellaChange::Changed(link))
    }

    /// A task's links as seen from that task: the ones it holds, plus a
    /// `blocks`, `related` or `child` entry for each link another task holds on it.
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
                    kind: link.kind.seen_by_target(),
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

    /// Deletes a link listed on `task_id`, whichever task holds it. With an `owner`, only a link
    /// that API token created may go.
    pub fn delete_task_link(
        &self,
        task_id: &str,
        link_id: &str,
        owner: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<LinkDeleteOutcome> {
        let txn = self.db.begin_write()?;
        let outcome = Self::delete_link_in(&txn, task_id, link_id, owner, actor)?;
        if outcome == LinkDeleteOutcome::Deleted {
            txn.commit()?;
        }
        Ok(outcome)
    }

    /// Edits the links of each of `ids` in one write transaction: first it removes the link each
    /// `remove` pair names where the task holds it, then it adds each `add` pair, as
    /// [`Db::add_task_link`] does. A refused change or a missing task leaves every task unchanged,
    /// and [`Commit::Discard`] ends the run without a commit. With an `owner`, only a link that API
    /// token created may go.
    pub fn change_links_logged(
        &self,
        ids: &[String],
        edit: &LinkEdit<'_>,
        commit: Commit,
        actor: Actor<'_>,
    ) -> DbResult<ChangeOutcome<LinkRefusal>> {
        let txn = self.db.begin_write()?;
        for id in ids {
            if txn_read_one::<TaskRow>(&txn, TASKS, id)?.is_none() {
                return Ok(ChangeOutcome::NotFound);
            }
            for (kind, target_id) in edit.remove {
                let Some(link_id) = Self::held_link_in(&txn, id, *kind, target_id)? else {
                    continue;
                };
                match Self::delete_link_in(&txn, id, &link_id, edit.owner, actor)? {
                    LinkDeleteOutcome::Deleted => {}
                    LinkDeleteOutcome::NotFound => return Ok(ChangeOutcome::NotFound),
                    LinkDeleteOutcome::NotOwner => {
                        return Ok(ChangeOutcome::Refused(LinkRefusal::NotOwner));
                    }
                }
            }
            for (kind, target_id) in edit.add {
                match Self::add_link_in(&txn, id, *kind, target_id, edit.token_id, actor)? {
                    LinkAddOutcome::Added(_) => {}
                    LinkAddOutcome::NotFound => return Ok(ChangeOutcome::NotFound),
                    LinkAddOutcome::Cycle => {
                        return Ok(ChangeOutcome::Refused(LinkRefusal::Cycle));
                    }
                    LinkAddOutcome::Duplicate => {
                        return Ok(ChangeOutcome::Refused(LinkRefusal::Duplicate));
                    }
                    LinkAddOutcome::HasParent => {
                        return Ok(ChangeOutcome::Refused(LinkRefusal::HasParent));
                    }
                    LinkAddOutcome::ParentCycle => {
                        return Ok(ChangeOutcome::Refused(LinkRefusal::ParentCycle));
                    }
                    LinkAddOutcome::NotEpic => {
                        return Ok(ChangeOutcome::Refused(LinkRefusal::NotEpic));
                    }
                }
            }
        }
        commit.finish(txn)?;
        Ok(ChangeOutcome::Changed)
    }

    /// Returns the id of the link `task_id` shows with `target_id` as `kind`, held by either end.
    fn held_link_in(
        txn: &WriteTransaction,
        task_id: &str,
        kind: LinkKind,
        target_id: &str,
    ) -> DbResult<Option<String>> {
        let (holder_id, stored_kind, stored_target) = match kind {
            LinkKind::Blocks => (target_id, LinkKind::BlockedBy, task_id),
            LinkKind::Child => (target_id, LinkKind::Parent, task_id),
            other => (task_id, other, target_id),
        };
        let mut holders = vec![(holder_id, stored_target)];
        if kind == LinkKind::Related {
            holders.push((target_id, task_id));
        }
        for (holder_id, target) in holders {
            let Some(holder) = txn_read_one::<TaskRow>(txn, TASKS, holder_id)? else {
                continue;
            };
            if let Some(link) = holder
                .links
                .iter()
                .find(|link| link.kind == stored_kind && link.target_id == target)
            {
                return Ok(Some(link.id.clone()));
            }
        }
        Ok(None)
    }

    /// Deletes a link listed on `task_id` inside the caller's write transaction, which the caller
    /// commits when the outcome is [`LinkDeleteOutcome::Deleted`] and drops otherwise.
    fn delete_link_in(
        txn: &WriteTransaction,
        task_id: &str,
        link_id: &str,
        owner: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<LinkDeleteOutcome> {
        let mut holders = vec![task_id.to_string()];
        holders.extend(txn_mmap_get(txn, LINKS_BY_TARGET, task_id)?);
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
                if !link_is_by(&holder.links[index], owner) {
                    return Ok(LinkDeleteOutcome::NotOwner);
                }
                let link = holder.links.remove(index);
                let target_name = task_in(&tbl, &link.target_id)?.map(|target| target.name);
                let detail = link_event_detail(&link, target_name.as_deref(), |detail| {
                    format!("Removed link {detail}")
                });
                push_event(&mut holder, HistoryKind::LinkRemoved, detail, actor);
                let still_linked = holder
                    .links
                    .iter()
                    .any(|other| other.target_id == link.target_id);
                holder.updated_at = Some(Utc::now().to_rfc3339());
                txn_store_task(txn, &mut tbl, &holder, OverQuota::Allow, actor)?;
                if !still_linked {
                    by_target.remove(link.target_id.as_str(), holder_id.as_str())?;
                }
                found = true;
                break;
            }
        }
        if !found {
            return Ok(LinkDeleteOutcome::NotFound);
        }
        Ok(LinkDeleteOutcome::Deleted)
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
                    txn_store_task(&txn, &mut table, task, OverQuota::Refuse, Actor::SYSTEM)?;
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
                    txn_store_task(&txn, &mut tasks, &task, OverQuota::Allow, Actor::SYSTEM)?;
                }
            }
        }
        mmap_remove(&txn, FIELDS_BY_BOARD, &field.board_id, id)?;
        let board: Option<BoardRow> = txn
            .open_table(BOARDS)?
            .get(field.board_id.as_str())?
            .map(|guard| serde_json::from_slice(guard.value()))
            .transpose()?;
        if let Some(mut board) = board
            && let Some(config) = rules::config_without_field(&board.config_json, &field.key)
        {
            board.config_json = config;
            txn_store_board(&txn, &board, OverQuota::Allow)?;
        }
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
                            row.permissions = grant.permissions.clone();
                            row.boards = grant.boards.clone();
                            row.token_expires_at = grant.expires_at;
                            row.role_id = grant.role_id.clone();
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
                        let mut record = mint(&row);
                        if let Some(refusal) = txn_token_cap_reached(&txn, &record.account_id)? {
                            DevicePoll::TooManyTokens(refusal)
                        } else if let Some(role_id) = row.role_id.as_deref()
                            && !txn_issue_as_role(&txn, &mut record, role_id)?
                        {
                            row.status = DeviceAuthStatus::Denied;
                            DevicePoll::Denied
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

    /// Attaches a file's bytes to a task. Returns [`None`] when the task does not exist.
    pub fn add_attachment(
        &self,
        task_id: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        TextCap::ATTACHMENT_MIME.check(mime)?;
        let id = Uuid::new_v4().to_string();
        let _held = self.files.hold(&id);
        self.files.write(&id, bytes)?;
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let file = StoredFile {
            id,
            size: bytes.len() as u64,
            sha256: Some(sha256),
            url: None,
            edited_at: None,
        };
        self.attach_stored(task_id, file, name, mime, uploaded_by)
    }

    /// Attaches the link `url` called `name` to a task. Returns [`None`] when the task does not
    /// exist.
    pub fn add_link(
        &self,
        task_id: &str,
        name: &str,
        url: &str,
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        let id = Uuid::new_v4().to_string();
        let _held = self.files.hold(&id);
        self.files.write(&id, url.as_bytes())?;
        let file = StoredFile {
            id,
            size: url.len() as u64,
            sha256: Some(format!("{:x}", Sha256::digest(url.as_bytes()))),
            url: Some(url.to_string()),
            edited_at: None,
        };
        self.attach_stored(task_id, file, name, TaskAttachment::LINK_MIME, uploaded_by)
    }

    /// Attaches a received file to a task and places it, which a task that does not exist or a
    /// refusal undoes. Returns [`None`] when the task does not exist.
    pub fn add_attachment_file(
        &self,
        task_id: &str,
        name: &str,
        mime: &str,
        upload: &Incoming,
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        TextCap::ATTACHMENT_MIME.check(mime)?;
        let _held = self.files.hold(upload.id());
        self.files.place_incoming(upload)?;
        let file = StoredFile {
            id: upload.id().to_string(),
            size: upload.size(),
            sha256: Some(upload.sha256()),
            url: None,
            edited_at: None,
        };
        self.attach_stored(task_id, file, name, mime, uploaded_by)
    }

    /// Attaches a text document called `name` holding `text`, with `.txt` appended to the name
    /// unless it ends with it. Returns [`None`] when the task does not exist.
    pub fn add_document(
        &self,
        task_id: &str,
        name: &str,
        text: &str,
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        let name = if name.ends_with(".txt") {
            name.to_string()
        } else {
            format!("{name}.txt")
        };
        TextCap::ATTACHMENT_NAME.check(&name)?;
        let id = Uuid::new_v4().to_string();
        let _held = self.files.hold(&id);
        self.files.write(&id, text.as_bytes())?;
        let file = StoredFile {
            id,
            size: text.len() as u64,
            sha256: None,
            url: None,
            edited_at: Some(Utc::now().to_rfc3339()),
        };
        self.attach_stored(
            task_id,
            file,
            &name,
            TaskAttachment::DOCUMENT_MIME,
            uploaded_by,
        )
    }

    /// Saves `text` as the content of a task's text document, keeping the earlier content as a
    /// version and renaming the document to `name` when one is given. Returns [`None`] and writes
    /// nothing when the task holds no such document.
    pub fn save_document(
        &self,
        task_id: &str,
        attachment_id: &str,
        name: Option<&str>,
        text: &str,
        actor: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        let size = text.len() as u64;
        let version_id = Uuid::new_v4().to_string();
        let _held = self.files.hold(&version_id);
        let txn = self.db.begin_write()?;
        let owner = task_owner_in(&txn, task_id)?;
        let board = task_board_in(&txn, task_id)?;
        let Some(mut task) = read_in::<TaskRow>(&txn, TASKS, task_id)? else {
            return Ok(None);
        };
        let held = serde_json::to_vec(&task)?.len() as u64;
        let Some(current_name) = task
            .attachments
            .iter()
            .find(|a| a.id == attachment_id && a.is_document())
            .map(|a| a.name.clone())
        else {
            return Ok(None);
        };
        let renamed = name
            .map(|name| TaskAttachment::keep_extension(&current_name, name))
            .filter(|renamed| *renamed != current_name);
        if let Some(renamed) = &renamed {
            let others: Vec<TaskAttachment> = task
                .attachments
                .iter()
                .filter(|a| a.id != attachment_id)
                .cloned()
                .collect();
            if let Some(duplicate) = DuplicateAttachment::among(&others, renamed, None) {
                return Err(Box::new(duplicate));
            }
        }
        let Some(document) = task.attachments.iter_mut().find(|a| a.id == attachment_id) else {
            return Ok(None);
        };
        if let Some(board) = &board {
            txn_refuse_board_over_bytes(&txn, board, size)?;
        }
        if let Some(owner) = &owner {
            txn_refuse_over_bytes(&txn, owner, size, 0)?;
        }
        let old = self.files.read(attachment_id)?.unwrap_or_default();
        let now = Utc::now().to_rfc3339();
        document.versions.push(AttachmentVersion {
            id: version_id.clone(),
            size: old.len() as u64,
            saved_at: document
                .edited_at
                .clone()
                .unwrap_or_else(|| document.created_at.clone()),
        });
        let excess = document
            .versions
            .len()
            .saturating_sub(TaskAttachment::MAX_VERSIONS);
        let dropped: Vec<AttachmentVersion> = document.versions.drain(..excess).collect();
        let freed: u64 = dropped.iter().map(|version| version.size).sum();
        document.size = size;
        document.sha256 = None;
        document.edited_at = Some(now.clone());
        let mut details = vec![format!("Saved \"{}\"", document.name)];
        if let Some(renamed) = renamed {
            TextCap::ATTACHMENT_NAME.check(&renamed)?;
            details.push(format!("Renamed \"{}\" to \"{renamed}\"", document.name));
            document.name = renamed;
        }
        let saved = document.clone();
        push_event(
            &mut task,
            HistoryKind::AttachmentEdited,
            details.remove(0),
            actor,
        );
        for detail in details {
            push_event(&mut task, HistoryKind::AttachmentRenamed, detail, actor);
        }
        task.updated_at = Some(now);
        let bytes = serde_json::to_vec(&task)?.len() as u64;
        refuse_row_past(Bound::TaskRowBytes, MAX_TASK_ROW_BYTES, bytes, held)?;
        {
            let mut table = txn.open_table(TASKS)?;
            txn_store_task(&txn, &mut table, &task, OverQuota::Allow, actor)?;
        }
        if let Some(owner) = &owner {
            txn_count_usage(&txn, owner, size, freed)?;
        }
        self.files.write(&version_id, &old)?;
        if let Err(error) = self.files.write(attachment_id, text.as_bytes()) {
            let _ = self.files.write(attachment_id, &old);
            self.files.remove(&[version_id]);
            return Err(error);
        }
        if let Err(error) = txn.commit() {
            let _ = self.files.write(attachment_id, &old);
            self.files.remove(&[version_id]);
            return Err(error);
        }
        let gone: Vec<String> = dropped.into_iter().map(|version| version.id).collect();
        self.files.remove(&gone);
        Ok(Some(saved))
    }

    /// Refuses, with [`QuotaExceeded`], an upload of `bytes` to task `task_id` that would leave
    /// the task past its attachment count or its owner past the stored-bytes quota, before any
    /// byte is received. It reads only, so a concurrent write can still be refused after the upload.
    pub fn check_attachment_room(&self, task_id: &str, name: &str, bytes: u64) -> DbResult<()> {
        let txn = self.db.begin_read()?;
        let Some(owner) = task_owner_in(&txn, task_id)? else {
            return Ok(());
        };
        let held = read_in::<TaskRow>(&txn, TASKS, task_id)?
            .map(|task| task.attachments)
            .unwrap_or_default();
        if let Some(duplicate) = DuplicateAttachment::among(&held, name, None) {
            return Err(Box::new(duplicate));
        }
        txn_refuse_over_quota(&txn, &owner, Quota::AttachmentsPerTask, held.len() + 1)?;
        if let Some(board) = task_board_in(&txn, task_id)? {
            txn_refuse_board_over_bytes(&txn, &board, bytes)?;
        }
        read_refuse_over_bytes(&txn, &owner, bytes)
    }

    /// Starts receiving an attachment's bytes into a file beside the database.
    pub fn receive_attachment(&self) -> DbResult<Incoming> {
        self.files.receive()
    }

    /// Opens the file of an attachment and returns it with its length, or [`None`] when no file holds
    /// it. Returns an error for an id the server did not generate.
    pub fn open_attachment(&self, attachment_id: &str) -> DbResult<Option<(File, u64)>> {
        self.files.open_file(attachment_id)
    }

    /// Stores the row of the attachment whose file `id` is in place, and removes the file when the
    /// task does not exist or the row is refused.
    fn attach_stored(
        &self,
        task_id: &str,
        file: StoredFile,
        name: &str,
        mime: &str,
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        let StoredFile {
            id,
            size,
            sha256,
            url,
            edited_at,
        } = file;
        let meta = TaskAttachment {
            id,
            name: name.to_string(),
            mime: mime.to_string(),
            size,
            uploaded_by: uploaded_by.name.to_string(),
            token_name: uploaded_by.token_name.map(str::to_string),
            role_id: uploaded_by.role_id.map(str::to_string),
            role_name: uploaded_by.role_name.map(str::to_string),
            label: uploaded_by.label.map(str::to_string),
            created_at: Utc::now().to_rfc3339(),
            sha256,
            url,
            edited_at,
            versions: Vec::new(),
        };
        let added = meta.clone();
        let stored = self.store_attachment(task_id, meta, name, size, uploaded_by);
        if !matches!(stored, Ok(true)) {
            self.files.remove(std::slice::from_ref(&added.id));
        }
        Ok(stored?.then_some(added))
    }

    /// Appends `meta` to the row of `task_id` and logs the upload, returning whether the task
    /// exists.
    fn store_attachment(
        &self,
        task_id: &str,
        meta: TaskAttachment,
        name: &str,
        file_bytes: u64,
        uploaded_by: Actor<'_>,
    ) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let owner = task_owner_in(&txn, task_id)?;
        let current = read_in::<TaskRow>(&txn, TASKS, task_id)?;
        if let Some(task) = &current {
            let sha256 = meta.sha256.as_deref();
            if let Some(duplicate) = DuplicateAttachment::among(&task.attachments, name, sha256) {
                return Err(Box::new(duplicate));
            }
        }
        if let Some(board) = task_board_in(&txn, task_id)? {
            txn_refuse_board_over_bytes(&txn, &board, file_bytes)?;
        }
        let found = {
            let mut tbl = txn.open_table(TASKS)?;
            match current {
                Some(mut task) => {
                    if let Some(owner) = &owner {
                        let after = task.attachments.len() + 1;
                        txn_refuse_over_quota(&txn, owner, Quota::AttachmentsPerTask, after)?;
                        txn_refuse_over_bytes(&txn, owner, file_bytes, 0)?;
                        txn_count_usage(&txn, owner, file_bytes, 0)?;
                    }
                    task.attachments.push(meta);
                    let detail = format!("Attached \"{name}\"");
                    push_event(&mut task, HistoryKind::AttachmentAdded, detail, uploaded_by);
                    task.updated_at = Some(Utc::now().to_rfc3339());
                    txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, uploaded_by)?;
                    true
                }
                None => false,
            }
        };
        txn.commit()?;
        Ok(found)
    }

    /// Moves the rows of the legacy `attachment_data` table into attachment files and drops it.
    fn move_legacy_attachments(&self) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        if !has_legacy_attachment_table(txn.list_tables()?) {
            return Ok(());
        }
        let mut named = HashSet::new();
        for task in rows_in::<TaskRow, _>(&txn.open_table(TASKS)?)? {
            named.extend(task.attachments.into_iter().map(|attachment| attachment.id));
        }
        let (mut moved, mut dropped) = (0_usize, 0_usize);
        {
            let table = txn.open_table(LEGACY_ATTACHMENT_DATA)?;
            for row in table.iter()? {
                let (id, bytes) = row?;
                let id = id.value();
                self.files.check_id(id)?;
                if named.contains(id) {
                    self.files.write(id, bytes.value())?;
                    moved += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        txn.delete_table(LEGACY_ATTACHMENT_DATA)?;
        txn.commit()?;
        println!(
            "pomb: moved {moved} attachments from the database into files; dropped {dropped} \
             that no task names"
        );
        Ok(())
    }

    /// Deletes the `.part` files a crash left, which happens before the first write of a run, then
    /// reports the complete files no task row names, which it keeps because a restore of an older
    /// backup leaves the files of newer attachments there, and the attachments whose file is
    /// missing.
    fn check_attachment_files(&self) -> DbResult<()> {
        let named = self.named_attachments()?;
        let swept = self.files.sweep_parts()?;
        if swept > 0 {
            println!("pomb: removed {swept} unfinished attachment files");
        }
        let unnamed = self.files.unnamed(&named)?;
        if !unnamed.is_empty() {
            println!(
                "pomb: {} attachment files in {} belong to no task and were kept. Remove them \
                 from the Backups section of the settings page once you have checked them",
                unnamed.len(),
                self.files.dir().display()
            );
        }
        let missing = self.missing_attachment_files()?;
        if let Some(first) = missing.first() {
            eprintln!(
                "pomb: {} of {} attachments have no file in {}, first {first}. Restore the \
                 attachments directory that belongs to this database, for example from the \
                 backup copy beside the backup that was restored",
                missing.len(),
                named.len(),
                self.files.dir().display()
            );
        }
        Ok(())
    }

    /// Returns the ids of the attachments the task rows name.
    fn named_attachments(&self) -> DbResult<HashSet<String>> {
        let txn = self.db.begin_read()?;
        named_in(&txn.open_table(TASKS)?, &txn.open_table(BIN)?)
    }

    /// Returns how many attachment files no task names and their bytes.
    pub fn unnamed_attachment_files(&self) -> DbResult<UnnamedFiles> {
        let files = self.files.unnamed(&self.named_attachments()?)?;
        Ok(UnnamedFiles {
            count: files.len(),
            bytes: files.iter().map(|(_, size)| size).sum(),
        })
    }

    /// Deletes the attachment files no task names and returns how many it deleted and their
    /// bytes. A file an upload stores meanwhile is never deleted.
    pub fn remove_unnamed_attachment_files(&self) -> DbResult<UnnamedFiles> {
        let txn = self.db.begin_write()?;
        let named = named_in(&txn.open_table(TASKS)?, &txn.open_table(BIN)?)?;
        self.files.remove_unnamed(&named)
    }

    /// Returns the ids of the attachments whose file is missing.
    pub fn missing_attachment_files(&self) -> DbResult<Vec<String>> {
        self.files.missing(&self.named_attachments()?)
    }

    /// Returns the bytes of an attachment, [`None`] when no file holds them, and an error for an
    /// id the server did not generate.
    pub fn read_attachment_data(&self, attachment_id: &str) -> DbResult<Option<Vec<u8>>> {
        self.files.read(attachment_id)
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
            let renamed = if attachment.url.is_some() {
                name.to_string()
            } else {
                TaskAttachment::keep_extension(&attachment.name, name)
            };
            TextCap::ATTACHMENT_NAME.check(&renamed)?;
            let detail = format!("Renamed \"{}\" to \"{renamed}\"", attachment.name);
            attachment.name = renamed;
            let updated = attachment.clone();
            push_event(&mut task, HistoryKind::AttachmentRenamed, detail, actor);
            task.updated_at = Some(Utc::now().to_rfc3339());
            let bytes = serde_json::to_vec(&task)?.len() as u64;
            refuse_row_past(Bound::TaskRowBytes, MAX_TASK_ROW_BYTES, bytes, held)?;
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow, actor)?;
            updated
        };
        txn.commit()?;
        Ok(Some(renamed))
    }

    /// Deletes an attachment's row, logs it and removes its file once that write commits. Returns
    /// false, writing nothing, when the task or the attachment does not exist.
    pub fn delete_attachment(
        &self,
        task_id: &str,
        attachment_id: &str,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        let mut removed = Vec::new();
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            let current: Option<TaskRow> = tbl
                .get(task_id)?
                .map(|guard| serde_json::from_slice(guard.value()))
                .transpose()?;
            if let Some(mut task) = current {
                let found = task
                    .attachments
                    .iter()
                    .find(|a| a.id == attachment_id)
                    .map(|a| {
                        let files: Vec<String> = a.file_ids().map(str::to_string).collect();
                        (a.name.clone(), a.stored_bytes(), files)
                    });
                if let Some((name, freed, files)) = found {
                    task.attachments.retain(|a| a.id != attachment_id);
                    removed = files;
                    let detail = format!("Deleted \"{name}\"");
                    push_event(&mut task, HistoryKind::AttachmentDeleted, detail, actor);
                    if task.cover_id.as_deref() == Some(attachment_id) {
                        task.cover_id = None;
                        let detail = format!("Removed \"{name}\" as the cover image");
                        push_event(&mut task, HistoryKind::CoverRemoved, detail, actor);
                    }
                    task.updated_at = Some(Utc::now().to_rfc3339());
                    txn_store_task(&txn, &mut tbl, &task, OverQuota::Allow, actor)?;
                    if let Some(owner) = txn_column_owner(&txn, &task.column_id)? {
                        txn_count_usage(&txn, &owner, 0, freed)?;
                    }
                }
            }
        }
        txn.commit()?;
        self.files.remove(&removed);
        Ok(!removed.is_empty())
    }

    /// Makes an image attachment the task's cover, or stops it being the cover, and logs the
    /// change. Returns [`None`] when the task does not exist and `false` when it holds no such
    /// image attachment.
    pub fn set_cover(
        &self,
        task_id: &str,
        attachment_id: &str,
        cover: CoverChange,
        actor: Actor<'_>,
    ) -> DbResult<Option<bool>> {
        self.apply_task_change(task_id, actor, |task| {
            let Some(name) = task
                .attachments
                .iter()
                .find(|a| a.id == attachment_id && a.is_image())
                .map(|a| a.name.clone())
            else {
                return (false, false);
            };
            if (task.cover_id.as_deref() == Some(attachment_id)) == (cover == CoverChange::Set) {
                return (true, false);
            }
            if cover == CoverChange::Set {
                task.cover_id = Some(attachment_id.to_string());
                let detail = format!("Set \"{name}\" as the cover image");
                push_event(task, HistoryKind::CoverSet, detail, actor);
            } else {
                task.cover_id = None;
                let detail = format!("Removed \"{name}\" as the cover image");
                push_event(task, HistoryKind::CoverRemoved, detail, actor);
            }
            (true, true)
        })
    }

    /// Run `f` on one task inside a single write transaction, storing the
    /// row (with `updated_at` stamped) when `f` reports a change. When it
    /// reports none, nothing is written. Returns None when the task does
    /// not exist.
    pub fn apply_task_change<R, F>(
        &self,
        task_id: &str,
        actor: Actor<'_>,
        f: F,
    ) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow) -> (R, bool),
    {
        self.change_task(task_id, false, ChildLinks::Keep, actor, |task, _| {
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
    pub fn apply_task_change_in_board<R, F>(
        &self,
        task_id: &str,
        children: ChildLinks,
        actor: Actor<'_>,
        f: F,
    ) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow, &TaskContext) -> (R, bool),
    {
        self.change_task(task_id, true, children, actor, |task, context| {
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

    /// Runs `f` on each of `ids` in one write transaction and stores every row with `updated_at`
    /// stamped. The first task `f` refuses ends the run with its error and writes nothing, and a
    /// missing task does the same. With `Commit::Discard` the run ends without a commit even when every
    /// task changed, so it checks and writes nothing.
    pub fn apply_tasks_change<E, F>(
        &self,
        ids: &[String],
        commit: Commit,
        actor: Actor<'_>,
        mut f: F,
    ) -> DbResult<ChangeOutcome<E>>
    where
        F: FnMut(&mut TaskRow, &TaskContext) -> Result<(), E>,
    {
        let txn = self.db.begin_write()?;
        for id in ids {
            let mut tbl = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tbl, id)? else {
                return Ok(ChangeOutcome::NotFound);
            };
            let context = task_context_in(&txn, &task)?;
            if let Err(error) = f(&mut task, &context) {
                return Ok(ChangeOutcome::Refused(error));
            }
            task.updated_at = Some(Utc::now().to_rfc3339());
            txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, actor)?;
        }
        commit.finish(txn)?;
        Ok(ChangeOutcome::Changed)
    }

    fn change_task<R, F>(
        &self,
        task_id: &str,
        with_context: bool,
        children: ChildLinks,
        actor: Actor<'_>,
        f: F,
    ) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow, &TaskContext) -> (R, RowWrite),
    {
        let txn = self.db.begin_write()?;
        let result = {
            let mut tbl = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tbl, task_id)? else {
                return Ok(None);
            };
            let mut context = if with_context {
                task_context_in(&txn, &task)?
            } else {
                TaskContext::default()
            };
            if with_context && task.kind == TaskKind::Epic {
                context.child_ids = child_ids_in(&txn, &tbl, task_id)?;
            }
            let (result, write) = f(&mut task, &context);
            if matches!(write, RowWrite::Stamp) {
                task.updated_at = Some(Utc::now().to_rfc3339());
            }
            if !matches!(write, RowWrite::Skip) {
                txn_store_task(&txn, &mut tbl, &task, OverQuota::Refuse, actor)?;
                if children == ChildLinks::Remove {
                    txn_unlink_children(&txn, &mut tbl, task_id, actor)?;
                }
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
        let outcome = self.change_task(task_id, false, ChildLinks::Keep, actor, |task, _| {
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
                if let Some(owner) = task.owner.as_mut() {
                    owner.lease_expires_at = Some(live.expires_at.clone());
                }

                // A renewal changes only the lease, like a heartbeat
                return (ClaimOutcome::Granted(live.clone()), RowWrite::Keep);
            }
            let holds = |owner: &Owner| match &owner.holder {
                OwnerHolder::Agent { name, .. } => *name == request.claimant,
                OwnerHolder::Human { .. } => false,
            };
            if let Some(owner) = task.live_owner(now).filter(|owner| !holds(owner)) {
                let write = if expired {
                    RowWrite::Stamp
                } else {
                    RowWrite::Skip
                };
                return (ClaimOutcome::OwnerHeld(owner.clone()), write);
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
            push_event(task, HistoryKind::Claimed, detail, actor);
            task.owner = Some(Owner {
                holder: OwnerHolder::Agent {
                    token_id: request.token_id.clone(),
                    name: claim.claimant.clone(),
                },
                lease_expires_at: Some(claim.expires_at.clone()),
            });
            task.claim = Some(claim.clone());
            (ClaimOutcome::Granted(claim), RowWrite::Stamp)
        })?;
        Ok(outcome.unwrap_or(ClaimOutcome::NotFound))
    }

    /// Sets whether an epic is closed, with a `closed` or `reopened` history event, in one write
    /// transaction. It leaves the column, `completed_at` and the children alone, and stamps
    /// `updated_at` on every dependent whose `blocked` value flips.
    pub fn set_epic_closed(
        &self,
        task_id: &str,
        closed: bool,
        actor: Actor<'_>,
    ) -> DbResult<EpicCloseOutcome> {
        let txn = self.db.begin_write()?;
        let before = blocked_states(&txn, &blocked_dependents(&txn, &[task_id.to_string()])?)?;
        let outcome = {
            let mut tasks = txn.open_table(TASKS)?;
            let Some(mut task) = task_in(&tasks, task_id)? else {
                return Ok(EpicCloseOutcome::NotFound);
            };
            if task.kind != TaskKind::Epic {
                return Ok(EpicCloseOutcome::NotEpic);
            }
            if task.closed == closed {
                return Ok(EpicCloseOutcome::Unchanged);
            }
            task.closed = closed;
            let (kind, detail) = if closed {
                (HistoryKind::Closed, "Closed the epic")
            } else {
                (HistoryKind::Reopened, "Reopened the epic")
            };
            push_event(&mut task, kind, detail.to_string(), actor);
            task.updated_at = Some(Utc::now().to_rfc3339());
            txn_store_task(&txn, &mut tasks, &task, OverQuota::Refuse, actor)?;
            EpicCloseOutcome::Changed
        };
        bump_flipped(&txn, &before)?;
        txn.commit()?;
        Ok(outcome)
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
        let outcome = self.change_task(
            task_id,
            false,
            ChildLinks::Keep,
            Actor::SYSTEM,
            |task, _| {
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
                if let Some(owner) = task.owner.as_mut() {
                    owner.lease_expires_at = Some(live.expires_at.clone());
                }

                // A heartbeat is not a change to the task, so `?since=` pollers
                // do not see every claimed task on every tick.
                (ClaimOutcome::Granted(live.clone()), RowWrite::Keep)
            },
        )?;
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
        let outcome = self.apply_task_change(task_id, actor, |task| {
            let now = Utc::now();
            let expired = expire_claim(task, now);
            match &task.claim {
                Some(live) if live.fence == fence => {
                    let detail = format!("Claim by {} (fence {}) released", live.claimant, fence);
                    task.claim = None;
                    end_lease(task, now);
                    push_event(task, HistoryKind::ClaimReleased, detail, actor);
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

    /// Makes `request.holder` the owner of a task in one write transaction. A task with no live
    /// lease, or with the holder's own, is granted. A live lease of another holder is returned as
    /// [`OwnerOutcome::Held`] unless `request.take_over` is set. A new owner gets the next fence
    /// and ends any live claim. The holder's own live lease keeps its fence and only changes its
    /// end.
    pub fn grab_owner(
        &self,
        task_id: &str,
        request: &GrabRequest,
        actor: Actor<'_>,
    ) -> DbResult<OwnerOutcome> {
        let outcome = self.change_task(task_id, false, ChildLinks::Keep, actor, |task, _| {
            let now = Utc::now();
            let expired = expire_claim(task, now);
            let lease = request.ttl_seconds.map(|ttl| lease_end(now, ttl));
            if let Some(live) = task.live_holder(now) {
                if live.holder.is_same_as(&request.holder) {
                    let renewed = Owner {
                        holder: request.holder.clone(),
                        lease_expires_at: lease,
                    };
                    if let (Some(claim), Some(end)) =
                        (task.claim.as_mut(), renewed.lease_expires_at.as_ref())
                    {
                        claim.expires_at.clone_from(end);
                    }
                    task.owner = Some(renewed.clone());
                    return (
                        OwnerOutcome::Granted(renewed, task.claim_fence),
                        RowWrite::Keep,
                    );
                }
                if !request.take_over {
                    let write = if expired {
                        RowWrite::Stamp
                    } else {
                        RowWrite::Skip
                    };
                    return (OwnerOutcome::Held(live), write);
                }
            }
            task.claim_fence += 1;
            task.claim = None;
            let owner = Owner {
                holder: request.holder.clone(),
                lease_expires_at: lease,
            };
            let detail = format!(
                "Owner grabbed by {} (fence {}){}",
                owner.holder.label(),
                task.claim_fence,
                owner
                    .lease_expires_at
                    .as_ref()
                    .map(|end| format!(" until {end}"))
                    .unwrap_or_default()
            );
            push_event(task, HistoryKind::Claimed, detail, actor);
            task.owner = Some(owner.clone());
            (
                OwnerOutcome::Granted(owner, task.claim_fence),
                RowWrite::Stamp,
            )
        })?;
        Ok(outcome.unwrap_or(OwnerOutcome::NotFound))
    }

    /// Extends the live lease of `holder` to `ttl_seconds` from now, when `fence` is the task's
    /// latest. A lapsed lease is [`OwnerOutcome::StaleFence`], and the lease of another holder is
    /// [`OwnerOutcome::Held`].
    pub fn renew_owner(
        &self,
        task_id: &str,
        holder: &OwnerHolder,
        fence: u64,
        ttl_seconds: i64,
    ) -> DbResult<OwnerOutcome> {
        let outcome = self.change_task(
            task_id,
            false,
            ChildLinks::Keep,
            Actor::SYSTEM,
            |task, _| {
                let now = Utc::now();
                let expired = expire_claim(task, now);
                let stale = if expired {
                    RowWrite::Stamp
                } else {
                    RowWrite::Skip
                };
                let Some(owner) = task.owner.as_mut().filter(|owner| owner.is_live(now)) else {
                    return (OwnerOutcome::StaleFence, stale);
                };
                if fence != task.claim_fence {
                    return (OwnerOutcome::StaleFence, stale);
                }
                if owner.holder != *holder {
                    return (OwnerOutcome::Held(owner.clone()), stale);
                }
                owner.lease_expires_at = Some(lease_end(now, ttl_seconds));
                let renewed = owner.clone();
                if let Some(claim) = task.claim.as_mut() {
                    claim.heartbeat_at = now.to_rfc3339();
                    claim.expires_at = lease_end(now, ttl_seconds);
                }
                (OwnerOutcome::Granted(renewed, fence), RowWrite::Keep)
            },
        )?;
        Ok(outcome.unwrap_or(OwnerOutcome::NotFound))
    }

    /// Ends the live lease of `holder`, which keeps the name, when `fence` is the task's latest.
    /// With no live lease, any fence the task has issued counts as already released.
    pub fn release_owner(
        &self,
        task_id: &str,
        holder: &OwnerHolder,
        fence: u64,
        actor: Actor<'_>,
    ) -> DbResult<OwnerOutcome> {
        let outcome = self.apply_task_change(task_id, actor, |task| {
            let now = Utc::now();
            let expired = expire_claim(task, now);
            let Some(live) = task.owner.clone().filter(|owner| owner.is_live(now)) else {
                let outcome = if (1..=task.claim_fence).contains(&fence) {
                    OwnerOutcome::Released
                } else {
                    OwnerOutcome::StaleFence
                };
                return (outcome, expired);
            };
            if fence != task.claim_fence {
                return (OwnerOutcome::StaleFence, expired);
            }
            if live.holder != *holder {
                return (OwnerOutcome::Held(live), expired);
            }
            task.claim = None;
            end_lease(task, now);
            let detail = format!("Owner {} (fence {fence}) released", live.holder.label());
            push_event(task, HistoryKind::ClaimReleased, detail, actor);
            (OwnerOutcome::Released, true)
        })?;
        Ok(outcome.unwrap_or(OwnerOutcome::NotFound))
    }

    /// Distinct labels of a board: task labels, time entry labels, and the labels its settings
    /// highlight or keep, sorted. Powers the "Add labels..." suggestions (KanbanFlow parity) and
    /// the filter.
    pub fn board_labels(&self, board_id: &str) -> DbResult<Vec<String>> {
        let mut labels: BTreeSet<String> = self.board_label_counts(board_id)?.into_keys().collect();
        self.visit_board_tasks(board_id, |task: TaskLabels| {
            for entry in self.list_entries(&task.id)? {
                labels.extend(entry.labels);
            }
            Ok(())
        })?;
        Ok(labels.into_iter().collect())
    }

    /// Returns each label of board `board_id` with the number of tasks carrying it: the labels on
    /// its tasks, plus the ones its settings highlight or keep, which may count 0.
    pub fn board_label_counts(&self, board_id: &str) -> DbResult<BTreeMap<String, usize>> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        self.visit_board_tasks(board_id, |task: TaskLabels| {
            for label in task.labels {
                *counts.entry(label).or_default() += 1;
            }
            Ok(())
        })?;
        if let Some(board) = self.board(board_id)? {
            let settings = LabelSettings::from_config(&board.config_json);
            let kept = settings
                .highlights
                .into_iter()
                .map(|highlight| highlight.label)
                .chain(settings.added);
            for label in kept {
                counts.entry(label).or_default();
            }
        }
        Ok(counts)
    }

    /// Deletes a time entry and writes a `time_deleted` event on its task, if the task exists, in
    /// one write, past the stored-bytes quota so a full account can still free space. Returns false when the entry does not exist.
    pub fn delete_entry(&self, entry_id: &str, actor: Actor<'_>) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(entry) = txn_read_one::<TimeEntryRow>(&txn, TIME_ENTRIES, entry_id)? else {
            return Ok(false);
        };
        let before = txn_account_minutes(&txn, &entry.account_id, &entry.task_id)?;
        {
            let mut tbl = txn.open_table(TIME_ENTRIES)?;
            txn_remove_entry(&txn, &mut tbl, entry_id)?;
        }
        mmap_remove(&txn, ENTRIES_BY_TASK, &entry.task_id, entry_id)?;
        if !entry.task_id.is_empty() {
            let change = EventChange {
                from: Some(format_minutes(before)),
                to: Some(format_minutes((before - entry.minutes).max(0))),
            };
            let detail = format!("Deleted a {}m time entry", entry.minutes);
            txn_push_task_event(
                &txn,
                &entry.task_id,
                HistoryKind::TimeDeleted,
                &detail,
                actor,
                change,
                OverQuota::Allow,
            )?;
        }
        txn.commit()?;
        Ok(true)
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
        let guards = MoveGuards::default();
        match self.move_task_logged(id, column_id, swimlane_id, position, guards, Actor::SYSTEM)? {
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
        actor: Actor<'_>,
    ) -> DbResult<MoveOutcome> {
        let txn = self.db.begin_write()?;
        let outcome = Self::move_in(&txn, id, column_id, swimlane_id, position, guards, actor)?;
        if outcome == MoveOutcome::Moved {
            txn.commit()?;
        }
        Ok(outcome)
    }

    /// Moves every task in `ids` into one cell, the first at `position` and each next one right
    /// after the one before, so the tasks keep the order given. The moves share one write
    /// transaction: when any task is missing, none moves. A move to another column logs the same
    /// `moved` event a single move does, and the target column's WIP limit does not block. With
    /// `Commit::Discard` every move runs and the transaction ends without a commit.
    pub fn move_tasks_logged(
        &self,
        ids: &[String],
        column_id: &str,
        swimlane_id: Option<&str>,
        position: f64,
        commit: Commit,
        actor: Actor<'_>,
    ) -> DbResult<MoveOutcome> {
        let txn = self.db.begin_write()?;
        for (offset, id) in ids.iter().enumerate() {
            let Some(task) = txn_read_one::<TaskRow>(&txn, TASKS, id)? else {
                return Ok(MoveOutcome::NotFound);
            };
            let target: ColumnRow = txn_read_one(&txn, COLUMNS, column_id)?
                .ok_or_else(|| format!("unknown column: {column_id}"))?;
            let source: Option<ColumnRow> = txn_read_one(&txn, COLUMNS, &task.column_id)?;
            let detail = (task.column_id != column_id).then(|| {
                let from = source
                    .as_ref()
                    .map_or("a column", |column| column.name.as_str());
                format!("Moved from {from} to {}", target.name)
            });
            let guards = MoveGuards {
                event: detail.as_deref(),
                batch: &ids[offset + 1..],
                ..MoveGuards::default()
            };
            let at = position + offset as f64;
            let outcome = Self::move_in(&txn, id, column_id, swimlane_id, at, guards, actor)?;
            if outcome != MoveOutcome::Moved {
                return Ok(outcome);
            }
        }
        commit.finish(txn)?;
        Ok(MoveOutcome::Moved)
    }

    /// Moves one task inside the caller's write transaction, which the caller commits when the
    /// outcome is [`MoveOutcome::Moved`] and drops otherwise.
    fn move_in(
        txn: &WriteTransaction,
        id: &str,
        column_id: &str,
        swimlane_id: Option<&str>,
        position: f64,
        guards: MoveGuards<'_>,
        actor: Actor<'_>,
    ) -> DbResult<MoveOutcome> {
        let Some(task) = txn_read_one::<TaskRow>(txn, TASKS, id)? else {
            return Ok(MoveOutcome::NotFound);
        };
        let target: ColumnRow = txn_read_one(txn, COLUMNS, column_id)?
            .ok_or_else(|| format!("unknown column: {column_id}"))?;
        if task.kind == TaskKind::Epic
            && !guards.skip_epic_guard
            && task.column_id != column_id
            && (target.wip_limit.is_some() || target.is_done)
        {
            let finishing = if target.is_done { guards.batch } else { &[] };
            let count = txn_open_children(txn, id, finishing)?;
            if count > 0 {
                return Ok(MoveOutcome::OpenChildren { count });
            }
        }
        if !task.fence_allows(guards.fence) {
            return Ok(MoveOutcome::StaleFence);
        }
        let old_column_id = task.column_id.clone();
        let old_swimlane_id = task.swimlane_id.clone();

        // A move within its own column never adds to that column
        if let (true, Some(limit), true, true) = (
            guards.enforce_wip,
            target.wip_limit,
            old_column_id != column_id,
            task.is_work(),
        ) {
            if target.wip_limit_in_hours() {
                let minutes =
                    |task: &TaskRow| (task.estimate_hours.unwrap_or(0.0) * 60.0).round() as i64;
                let cell: Vec<TaskRow> = txn_indexed_rows(txn, TASKS_BY_COLUMN, column_id, TASKS)?;
                let held: i64 = cell
                    .iter()
                    .filter(|other| other.id != id && other.is_work())
                    .map(minutes)
                    .sum();
                if minutes(&task) > 0 && held + minutes(&task) > limit * 60 {
                    return Ok(MoveOutcome::WipLimit {
                        count: (held as usize).div_ceil(60),
                        limit,
                    });
                }
            } else {
                let count = txn_indexed_rows::<TaskRow>(txn, TASKS_BY_COLUMN, column_id, TASKS)?
                    .iter()
                    .filter(|other| other.id != id && other.is_work())
                    .count();
                if count as i64 >= limit {
                    return Ok(MoveOutcome::WipLimit { count, limit });
                }
            }
        }
        if guards.enforce_wip
            && old_column_id != column_id
            && task.is_work()
            && let Some((count, limit)) =
                txn_group_over_limit(txn, &target.board_id, id, &old_column_id, column_id)?
        {
            return Ok(MoveOutcome::GroupLimit { count, limit });
        }

        let old_column: Option<ColumnRow> = txn_read_one(txn, COLUMNS, &old_column_id)?;
        let was_done = old_column
            .as_ref()
            .map(|column| column.is_done)
            .unwrap_or(false);
        let completed_at = match (target.is_done, was_done) {
            _ if !task.is_work() => task.completed_at.clone(),
            (true, false) => Some(Utc::now().to_rfc3339()),
            (true, true) => task.completed_at.clone(),
            (false, _) => None,
        };

        // Keep the current swimlane when the caller doesn't name one. Lanes are per-board, so a
        // cross-board move lands in the target board's first lane instead.
        let crosses_boards = old_column
            .as_ref()
            .is_some_and(|column| column.board_id != target.board_id);
        if let Some(old_column) = old_column.as_ref().filter(|_| crosses_boards) {
            for account in txn_board_accounts(txn, &old_column.board_id)? {
                let kept = role_in(txn, &AccountId::new(account.clone()), &target.board_id)?;
                if !matches!(kept, Some(role) if role >= BoardRole::Member) {
                    txn_clear_timer_if(txn, &account, |task_id| task_id == id)?;
                }
            }
        }
        if crosses_boards {
            let held = txn_board_task_count(txn, &target.board_id)?;
            txn_refuse_board_over_quota(txn, &target.board_id, Quota::TasksPerBoard, held + 1)?;
        }
        let new_swimlane_id = match swimlane_id {
            Some(lane) => Some(lane.to_string()),
            None if crosses_boards => {
                let mut lanes: Vec<SwimlaneRow> =
                    txn_indexed_rows(txn, SWIMLANES_BY_BOARD, &target.board_id, SWIMLANES)?;
                lanes.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
                lanes.into_iter().next().map(|lane| lane.id)
            }
            None => old_swimlane_id.clone(),
        };
        let cell_changed =
            column_id != old_column_id || new_swimlane_id.as_deref() != old_swimlane_id.as_deref();
        let before = blocked_states(txn, &blocked_dependents(txn, &[id.to_string()])?)?;

        // Custom field values only travel to a board that defines the same
        // key and accepts the value.
        let target_fields = if crosses_boards {
            board_fields_in(txn, &target.board_id)?
        } else {
            Vec::new()
        };

        let number = match task.number {
            Some(_) if crosses_boards => {
                txn_take_task_number(txn, &target.board_id)?.or_else(|| task.number.clone())
            }
            _ => task.number.clone(),
        };

        // Colors are per board, so a cross-board move takes the target board's color of the same
        // value, else its default.
        let color_id = match task.color_id.as_deref() {
            Some(color_id) if crosses_boards => {
                let value =
                    txn_read_one::<ColorRow>(txn, TASK_COLORS, color_id)?.map(|color| color.value);
                let colors = txn_board_colors(txn, &target.board_id)?;
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
        let mut siblings = txn_cell_tasks(txn, column_id, new_swimlane_id.as_deref())?;
        siblings.retain(|sibling| sibling.id != id && !guards.batch.contains(&sibling.id));
        let work = siblings.iter().filter(|sibling| sibling.is_work()).count();
        let work_at = (position.round() as i64).clamp(0, work as i64) as usize;
        let at = siblings
            .iter()
            .enumerate()
            .filter(|(_, sibling)| sibling.is_work())
            .nth(work_at)
            .map_or(siblings.len(), |(index, _)| index);
        let closed = match (target.is_done, was_done) {
            _ if guards.skip_epic_guard => task.closed,
            (true, false) if task.kind == TaskKind::Epic => true,
            (false, true) => false,
            _ => task.closed,
        };
        let closed_changed = closed != task.closed;
        let mut moved = TaskRow {
            closed,
            column_id: column_id.to_string(),
            swimlane_id: new_swimlane_id.clone(),
            position: at as f64,
            completed_at,
            color_id,
            number,

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
        if closed_changed {
            let (kind, detail) = if closed {
                (HistoryKind::Closed, "Closed the epic")
            } else {
                (HistoryKind::Reopened, "Reopened the epic")
            };
            push_event(&mut moved, kind, detail.to_string(), actor);
        }
        let logged = match guards.event {
            Some(detail) => {
                let detail = format!("{detail}{dropped_note}");
                push_column_event(
                    &mut moved,
                    HistoryKind::Moved,
                    detail,
                    actor,
                    Some(column_id),
                );
                true
            }
            None if !dropped_note.is_empty() => {
                let detail = format!("Moved to another board{dropped_note}");
                push_column_event(
                    &mut moved,
                    HistoryKind::Moved,
                    detail,
                    actor,
                    Some(column_id),
                );
                true
            }
            None => false,
        };
        if logged && column_id != old_column_id {
            let change = EventChange {
                from: old_column.as_ref().map(|column| column.name.clone()),
                to: Some(target.name.clone()),
            };
            record_change(&mut moved, change);
        }
        if crosses_boards {
            let board_name = |board_id: Option<&str>| -> DbResult<Option<String>> {
                let Some(board_id) = board_id else {
                    return Ok(None);
                };
                Ok(txn_read_one::<BoardRow>(txn, BOARDS, board_id)?.map(|board| board.name))
            };
            let change = EventChange {
                from: board_name(old_column.as_ref().map(|column| column.board_id.as_str()))?,
                to: board_name(Some(target.board_id.as_str()))?,
            };
            let detail = format!(
                "Moved to board {}",
                change.to.as_deref().unwrap_or("(none)")
            );
            push_change_event(&mut moved, HistoryKind::BoardUpdated, detail, actor, change);
        }
        if new_swimlane_id != old_swimlane_id {
            let lane_name = |lane_id: Option<&str>| -> DbResult<Option<String>> {
                let Some(lane_id) = lane_id else {
                    return Ok(None);
                };
                Ok(txn_read_one::<SwimlaneRow>(txn, SWIMLANES, lane_id)?.map(|lane| lane.name))
            };
            let change = EventChange {
                from: lane_name(old_swimlane_id.as_deref())?,
                to: lane_name(new_swimlane_id.as_deref())?,
            };
            let detail = format!(
                "Moved to swimlane {}",
                change.to.as_deref().unwrap_or("(none)")
            );
            push_change_event(
                &mut moved,
                HistoryKind::SwimlaneUpdated,
                detail,
                actor,
                change,
            );
        }

        if column_id != old_column_id {
            mmap_remove(txn, TASKS_BY_COLUMN, &old_column_id, id)?;
            mmap_insert(txn, TASKS_BY_COLUMN, column_id, id)?;
        }
        {
            let mut tbl = txn.open_table(TASKS)?;
            txn_store_task(txn, &mut tbl, &moved, OverQuota::Refuse, actor)?;
            for (index, mut sibling) in siblings.into_iter().enumerate() {
                let dense = (if index >= at { index + 1 } else { index }) as f64;
                if sibling.position == dense {
                    continue;
                }
                sibling.position = dense;
                sibling.updated_at = Some(Utc::now().to_rfc3339());
                txn_store_task(txn, &mut tbl, &sibling, OverQuota::Allow, actor)?;
            }
        }
        if cell_changed {
            txn_renumber_cell(txn, &old_column_id, old_swimlane_id.as_deref())?;
        }
        bump_flipped(txn, &before)?;
        Ok(MoveOutcome::Moved)
    }

    /// Rewrite positions in one column x swimlane cell as dense 0..n ordering.
    pub fn renumber_cell(&self, column_id: &str, swimlane_id: Option<&str>) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_renumber_cell(&txn, column_id, swimlane_id)?;
        txn.commit()?;
        Ok(())
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
                    role: None,
                    token_name: None,
                    role_id: None,
                    role_name: None,
                },
            )?
            .unwrap_or_default())
    }

    /// Logs `entry` for `account` on its task and its `time_logged` history event, reading
    /// `detail`, in one write, so a failed history write stores no entry. A manual entry is
    /// refused at the stored-bytes quota, and a timer session's is stored past it as far as
    /// [`OverQuota::TimerSession`] allows. Returns the entry id, or [`None`] with nothing written
    /// when the entry names no task, the task does not exist or `account` holds no role on its
    /// board.
    pub fn log_task_time(
        &self,
        account: &AccountId,
        entry: NewTimeEntry<'_>,
        detail: &str,
        actor: Actor<'_>,
    ) -> DbResult<Option<String>> {
        let Some(task_id) = entry.task_id else {
            return Ok(None);
        };
        let over = if entry.kind == EntryKind::Manual {
            OverQuota::Refuse
        } else {
            OverQuota::TimerSession
        };
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        if task_role_in(&txn, account, task_id)?.is_none() {
            return Ok(None);
        }
        let entry = entry.into_row(
            Uuid::new_v4().to_string(),
            task_id,
            account.as_str().to_string(),
        );
        let before = txn_account_minutes(&txn, &entry.account_id, task_id)?;
        let change = EventChange {
            from: Some(format_minutes(before)),
            to: Some(format_minutes(before + entry.minutes)),
        };
        if !txn_push_task_event(
            &txn,
            task_id,
            HistoryKind::TimeLogged,
            detail,
            actor,
            change,
            over,
        )? {
            return Ok(None);
        }
        txn_store_entry(&txn, &entry, over)?;
        mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &entry.id)?;
        txn.commit()?;
        Ok(Some(entry.id))
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
        Ok(settings_or_default(stored))
    }

    /// Edits `account`'s settings with `edit` in one write, holding the same caps as
    /// [`Db::update_settings`], and returns what `edit` returns.
    pub fn update_settings_with<R>(
        &self,
        account: &AccountId,
        edit: impl FnOnce(&mut Settings) -> R,
    ) -> DbResult<R> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        let stored = txn_read_one::<Settings>(&txn, ACCOUNT_SETTINGS, account.as_str())?;
        let mut settings = settings_or_default(stored.clone());
        let edited = edit(&mut settings);
        settings.check_caps(stored.as_ref())?;
        write_one(&txn, ACCOUNT_SETTINGS, account.as_str(), &settings)?;
        txn.commit()?;
        Ok(edited)
    }

    /// Stores `account`'s settings.
    pub fn update_settings(&self, account: &AccountId, settings: &Settings) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        let stored = txn_read_one::<Settings>(&txn, ACCOUNT_SETTINGS, account.as_str())?;
        let mut settings = settings.clone();
        if let Some(stored) = &stored {
            settings.avatar = stored.avatar.clone();
        }
        settings.check_caps(stored.as_ref())?;
        write_one(&txn, ACCOUNT_SETTINGS, account.as_str(), &settings)?;
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
    /// and flushes it to disk, with a copy of the attachment files in the directory `files`.
    ///
    /// Writers wait until both copies are written, and readers are not blocked. The copy carries
    /// redb's "not closed cleanly" mark, so its first open runs redb's crash repair. Every file a
    /// task row of the copy names is in `files`, along with any file whose attachment a delete has
    /// already removed from the rows.
    pub fn snapshot(&self, part: &Path, files: &Path) -> DbResult<()> {
        self.with_writes_held(|| {
            let mut source = std::fs::File::open(&*self.path)?;
            let mut copy = std::fs::File::create(part)?;
            std::io::copy(&mut source, &mut copy)?;
            copy.sync_all()?;
            self.files.copy_to(files).map(drop)
        })
    }

    /// Returns the directory beside the database file that holds the attachment files.
    pub fn attachments_dir(&self) -> &Path {
        self.files.dir()
    }

    /// Returns the bytes the attachment files hold.
    pub fn attachment_file_bytes(&self) -> DbResult<u64> {
        self.files.bytes()
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
        if let Some(task_id) = &timer.task_id {
            txn_refuse_timer_below_member(&txn, account, task_id)?;
        }
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
        mutate_task_as(&self.db, task_id, OverQuota::Allow, Actor::SYSTEM, |task| {
            task.pomodori_completed += 1;
        })?;
        Ok(())
    }

    /// Bump a task's interruption counter, even past the stored-bytes quota. No-op when unknown.
    pub fn record_interruption(&self, task_id: &str) -> DbResult<()> {
        mutate_task_as(&self.db, task_id, OverQuota::Allow, Actor::SYSTEM, |task| {
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
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        let task_id = match entry.task_id {
            Some(id) if task_role_in(&txn, account, id)?.is_some() => id,
            Some(_) => return Ok(None),
            None => "",
        };
        let id = Uuid::new_v4().to_string();
        let over = if entry.kind == EntryKind::Manual {
            OverQuota::Refuse
        } else {
            OverQuota::TimerSession
        };
        let row = entry.into_row(id.clone(), task_id, account.as_str().to_string());
        txn_store_entry(&txn, &row, over)?;
        if !task_id.is_empty() {
            mmap_insert(&txn, ENTRIES_BY_TASK, task_id, &id)?;
        }
        txn.commit()?;
        Ok(Some(id))
    }

    /// Updates a time entry (date/time, task reassignment, note, labels) and writes a
    /// `time_edited` event on its task, if it has one, in one write.
    /// An empty `task_id` makes it task-less. Returns false when the entry does not exist; the task
    /// must exist (checked by the caller).
    pub fn update_entry(
        &self,
        entry_id: &str,
        task_id: &str,
        edit: EntryEdit<'_>,
        actor: Actor<'_>,
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
        row.seconds = edit.seconds;
        row.minutes = edit.seconds / 60;
        row.note = edit.note.to_string();
        row.started_at = edit.started_at.to_string();
        row.labels = edit.labels.to_vec();

        drop(tbl);
        let before = txn_account_minutes(&txn, &row.account_id, task_id)?;
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
        if !task_id.is_empty() {
            let after = txn_account_minutes(&txn, &row.account_id, task_id)?;
            let change = EventChange {
                from: Some(format_minutes(before)),
                to: Some(format_minutes(after)),
            };
            let detail = format!("Edited a time entry ({}m)", row.minutes);
            txn_push_task_event(
                &txn,
                task_id,
                HistoryKind::TimeEdited,
                &detail,
                actor,
                change,
                OverQuota::Refuse,
            )?;
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
            .write_template_snapshot(&board, &["gone".to_string(), kept], CopyTasks::ClearMembers)
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

    #[test]
    fn template_snapshot_should_keep_an_epic_and_read_an_old_task_as_open_plain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let board = db
            .create_board(&AccountId::ownerless(), "Board")
            .expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let epic = db
            .create_task(&column, None, "Epic", 1, None)
            .expect("task");
        db.apply_task_change(&epic, Actor::SYSTEM, |row| {
            row.kind = TaskKind::Epic;
            row.closed = true;
            ((), true)
        })
        .expect("epic");
        let snapshot = db
            .write_template_snapshot(&board, &[epic], CopyTasks::ClearMembers)
            .expect("snapshot");
        let mut snapshot: serde_json::Value =
            serde_json::from_slice(&snapshot).expect("valid JSON");
        let tasks = snapshot["tasks"].as_array_mut().expect("tasks");
        let mut old = tasks[0].clone();
        let fields = old.as_object_mut().expect("object");
        fields.remove("kind");
        fields.remove("closed");
        fields.insert("name".to_string(), serde_json::json!("Old"));
        tasks.push(old);
        let mut row = BoardTemplateRow {
            id: "epics".to_string(),
            name: "Epics".to_string(),
            description: String::new(),
            built_in: false,
            owner_id: Some(String::new()),
            snapshot_bytes: 0,
        };
        let snapshot = serde_json::to_vec(&snapshot).expect("json");
        let txn = db.db.begin_write().expect("write");
        txn_store_template(&txn, &mut row, &snapshot).expect("template");
        txn.commit().expect("commit");

        let copy = db
            .apply_template(&AccountId::ownerless(), "epics", "Copy")
            .expect("apply")
            .expect("board");

        let tasks = db.board_tasks(&copy).expect("tasks");
        let state_of = |name: &str| {
            let task = tasks.iter().find(|task| task.name == name).expect("task");
            (task.kind, task.closed)
        };
        assert_eq!(state_of("Epic"), (TaskKind::Epic, true));
        assert_eq!(state_of("Old"), (TaskKind::Task, false));
    }

    fn seat_slots(pairs: &[(&str, &str)]) -> Vec<SeatSlot> {
        pairs
            .iter()
            .map(|(name, role)| SeatSlot {
                name: (*name).to_string(),
                role: (*role).to_string(),
            })
            .collect()
    }

    fn store_template_snapshot(db: &Db, id: &str, snapshot: &serde_json::Value) {
        let mut row = BoardTemplateRow {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            built_in: false,
            owner_id: Some(String::new()),
            snapshot_bytes: 0,
        };
        let snapshot = serde_json::to_vec(snapshot).expect("json");
        let txn = db.db.begin_write().expect("write");
        txn_store_template(&txn, &mut row, &snapshot).expect("template");
        txn.commit().expect("commit");
    }

    #[test]
    fn saved_template_should_keep_seats_and_give_them_to_its_tasks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let owner = AccountId::ownerless();
        let board = db.create_board(&owner, "Source").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        db.create_task(&column, None, "Work", 1, None)
            .expect("task");
        let slots = seat_slots(&[("code", "implementer"), ("verify", "verifier")]);
        db.set_board_seats(&board, slots.clone()).expect("seats");
        let template = db
            .save_board_as_template(&owner, &board, "Seated", "")
            .expect("save")
            .expect("template");

        let copy = db
            .apply_template(&owner, &template, "Copy")
            .expect("apply")
            .expect("board");

        let copied = db.board(&copy).expect("board").expect("row");
        assert_eq!(copied.seats, slots);
        let tasks = db.board_tasks(&copy).expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].seats,
            slots.iter().map(Seat::open).collect::<Vec<_>>()
        );
    }

    #[test]
    fn applying_a_saved_agent_template_should_give_the_board_code_style_and_verify_seats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let slots = seat_slots(&[
            ("code", "implementer"),
            ("style", "reviewer"),
            ("verify", "verifier"),
        ]);
        store_template_snapshot(
            &db,
            "agent",
            &serde_json::json!({ "seats": slots, "columns": [{ "name": "Todo" }] }),
        );

        let copy = db
            .apply_template(&AccountId::ownerless(), "agent", "Agents")
            .expect("apply")
            .expect("board");

        let copied = db.board(&copy).expect("board").expect("row");
        let names: Vec<&str> = copied.seats.iter().map(|seat| seat.name.as_str()).collect();
        assert_eq!(names, vec!["code", "style", "verify"]);
        assert_eq!(copied.seats, slots);
    }

    #[test]
    fn applying_a_saved_template_without_seats_should_give_no_seats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        store_template_snapshot(
            &db,
            "old",
            &serde_json::json!({
                "columns": [{ "name": "Todo" }],
                "tasks": [{ "name": "Old", "column": "Todo" }],
            }),
        );

        let copy = db
            .apply_template(&AccountId::ownerless(), "old", "Copy")
            .expect("apply")
            .expect("board");

        assert!(
            db.board(&copy)
                .expect("board")
                .expect("row")
                .seats
                .is_empty()
        );
        let tasks = db.board_tasks(&copy).expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].seats.is_empty());
    }

    #[test]
    fn an_epic_created_in_a_done_column_should_not_be_completed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let board = db
            .create_board(&AccountId::ownerless(), "Board")
            .expect("board");
        let done = db.create_column(&board, "Done", None).expect("column");
        db.set_column_done(&done, true).expect("done flag");
        let mut row = Db::new_task_row(&done, None, "Epic", 1, None);
        row.kind = TaskKind::Epic;
        let epic = db.insert_task(row, NewTaskPlacement::Bottom).expect("epic");
        let plain = db.create_task(&done, None, "Plain", 1, None).expect("task");

        let completed = |id: &str| db.task(id).expect("read").expect("task").completed_at;
        assert_eq!(completed(&epic), None);
        assert!(completed(&plain).is_some());
    }

    #[test]
    fn marking_a_column_done_should_complete_its_cards_but_not_its_epics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let board = db
            .create_board(&AccountId::ownerless(), "Board")
            .expect("board");
        let column = db.create_column(&board, "Review", None).expect("column");
        let epic = db
            .create_task(&column, None, "Epic", 1, None)
            .expect("epic");
        let plain = db
            .create_task(&column, None, "Plain", 1, None)
            .expect("task");
        db.apply_task_change(&epic, Actor::SYSTEM, |row| {
            row.kind = TaskKind::Epic;
            ((), true)
        })
        .expect("epic kind");

        db.set_column_done(&column, true).expect("done flag");

        let completed = |id: &str| db.task(id).expect("read").expect("task").completed_at;
        assert_eq!(completed(&epic), None);
        assert!(completed(&plain).is_some());
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

    fn event_kinds(board: &Board, task: &str) -> Vec<HistoryKind> {
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
        assert!(!event_kinds(&board, &own).contains(&HistoryKind::SwimlaneMerged));
        assert!(event_kinds(&board, &moved).contains(&HistoryKind::SwimlaneMerged));
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
                .feeds()
                .is_empty()
        );
        assert!(db.board_by_feed_token("nope").expect("lookup").is_none());

        set_feed_token(&db, &board_id, "tok123");
        let board = db
            .board(&board_id)
            .expect("get board")
            .expect("board exists");
        assert_eq!(board.feeds()[0].token, "tok123");

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
    fn feeds_should_resolve_by_their_own_token_until_removed() {
        let (_dir, db) = test_db();
        let board_id = db
            .create_board(&AccountId::ownerless(), "Work")
            .expect("create board");
        let FeedAdd::Added(phone) = db.add_board_feed(&board_id, "Phone", "tok-a").expect("add")
        else {
            panic!("the feed was not added");
        };
        let FeedAdd::Added(laptop) = db
            .add_board_feed(&board_id, "Laptop", "tok-b")
            .expect("add")
        else {
            panic!("the feed was not added");
        };
        assert_ne!(phone.id, laptop.id);
        for token in ["tok-a", "tok-b"] {
            let found = db.board_by_feed_token(token).expect("lookup");
            assert_eq!(
                found.map(|board| board.id),
                Some(board_id.clone()),
                "{token}"
            );
        }
        let labels: Vec<String> = db
            .board(&board_id)
            .expect("get")
            .expect("board")
            .feeds()
            .into_iter()
            .map(|feed| feed.label)
            .collect();
        assert_eq!(labels, ["Phone", "Laptop"]);

        assert!(db.remove_board_feed(&board_id, &phone.id).expect("remove"));
        assert!(!db.remove_board_feed(&board_id, &phone.id).expect("again"));
        assert!(db.board_by_feed_token("tok-a").expect("lookup").is_none());
        assert!(db.board_by_feed_token("tok-b").expect("lookup").is_some());
    }

    #[test]
    fn adding_a_feed_should_stop_at_the_board_limit() {
        let (_dir, db) = test_db();
        let board_id = db
            .create_board(&AccountId::ownerless(), "Work")
            .expect("create board");
        for n in 0..MAX_BOARD_FEEDS {
            let added = db
                .add_board_feed(&board_id, "Feed", &format!("tok-{n}"))
                .expect("add");
            assert!(matches!(added, FeedAdd::Added(_)), "feed {n}");
        }
        let full = db
            .add_board_feed(&board_id, "Feed", "tok-over")
            .expect("add");
        assert_eq!(full, FeedAdd::Full);
        assert!(
            db.board_by_feed_token("tok-over")
                .expect("lookup")
                .is_none()
        );
        let missing = db.add_board_feed("nope", "Feed", "tok-x").expect("add");
        assert_eq!(missing, FeedAdd::NoBoard);
    }

    #[test]
    fn deleting_a_board_should_revoke_all_its_feeds() {
        let (_dir, db) = test_db();
        let board_id = db
            .create_board(&AccountId::ownerless(), "Work")
            .expect("create board");
        db.create_board(&AccountId::ownerless(), "Other")
            .expect("create board");
        db.add_board_feed(&board_id, "Phone", "tok-a").expect("add");
        db.add_board_feed(&board_id, "Laptop", "tok-b")
            .expect("add");
        db.delete_board(&board_id).expect("delete board");
        for token in ["tok-a", "tok-b"] {
            assert!(
                db.board_by_feed_token(token).expect("lookup").is_none(),
                "{token}"
            );
        }
    }

    fn bookmark(owner: &str, name: &str, shared: bool) -> ReportBookmark {
        ReportBookmark {
            id: uuid::Uuid::new_v4().to_string(),
            owner_id: owner.to_string(),
            report: "burndown".to_string(),
            name: name.to_string(),
            shared,
            shortcut: false,
            filters: "{}".to_string(),
        }
    }

    #[test]
    fn report_bookmarks_should_change_only_for_the_account_that_saved_them() {
        let (_dir, db) = test_db();
        let board_id = db
            .create_board(&AccountId::ownerless(), "Work")
            .expect("create board");
        let BookmarkAdd::Added(mine) = db
            .add_report_bookmark(&board_id, bookmark("a", "Mine", true))
            .expect("add")
        else {
            panic!("not added");
        };
        let patch = || BookmarkPatch {
            name: Some("Taken".to_string()),
            ..BookmarkPatch::default()
        };
        assert!(
            db.update_report_bookmark(&board_id, &mine.id, "b", patch())
                .expect("update")
                .is_none()
        );
        assert!(
            !db.remove_report_bookmark(&board_id, &mine.id, "b")
                .expect("remove")
        );
        let row = db.board(&board_id).expect("board").expect("row");
        assert_eq!(row.report_bookmarks, vec![mine.clone()]);

        let renamed = db
            .update_report_bookmark(&board_id, &mine.id, "a", patch())
            .expect("update")
            .expect("own bookmark");
        assert_eq!(renamed.name, "Taken");
        assert!(
            db.remove_report_bookmark(&board_id, &mine.id, "a")
                .expect("remove")
        );
        let row = db.board(&board_id).expect("board").expect("row");
        assert!(row.report_bookmarks.is_empty());
    }

    #[test]
    fn report_bookmarks_should_show_a_private_one_only_to_its_owner() {
        let private = bookmark("a", "Private", false);
        let shared = bookmark("a", "Shared", true);
        assert!(private.visible_to("a"));
        assert!(!private.visible_to("b"));
        assert!(shared.visible_to("b"));
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

    use std::collections::BTreeMap;

    use redb::TableHandle;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::auth::new_api_token_record;
    use crate::models::{AgentRoleRow, FieldKind, FieldOwner};
    use crate::permissions::Preset;

    fn lead_role_permissions() -> BTreeSet<Permission> {
        STANDARD_ROLES
            .iter()
            .find(|standard| standard.name == "lead")
            .expect("lead")
            .permissions()
    }

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
    fn version_68_file_should_give_standard_roles_boards_read_and_no_other_role() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let held = BTreeSet::from([Permission::TasksRead, Permission::CommentsWrite]);
        let (planner, custom, covered) = {
            let db = Db::connect(&path).expect("connect");
            let planner = db
                .create_agent_role(account.as_str(), "planner", held.clone())
                .expect("planner");
            let custom = db
                .create_agent_role(account.as_str(), "scribe", held.clone())
                .expect("custom");
            let with_read = BTreeSet::from([Permission::BoardsRead, Permission::TasksRead]);
            let covered = db
                .create_agent_role(account.as_str(), "verifier", with_read)
                .expect("verifier");
            (planner.id, custom.id, covered.id)
        };
        stamp(&path, 68);

        let db = Db::connect(&path).expect("open version 68");
        let role = |id: &str| -> AgentRoleRow {
            read_one(&db.db, AGENT_ROLES, id)
                .expect("read")
                .expect("row")
        };
        let mut expected = held.clone();
        expected.insert(Permission::BoardsRead);
        assert_eq!(role(&planner).permissions, expected);
        assert_eq!(role(&custom).permissions, held);
        assert_eq!(
            role(&covered).permissions,
            BTreeSet::from([Permission::BoardsRead, Permission::TasksRead])
        );
        drop(db);
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    /// A version-62 file opens with its tokens' permissions unchanged, an issuing token's child
    /// lifetime read as one hour, and empty agent role tables.
    #[test]
    fn version_62_file_should_open_with_one_hour_children_and_no_agent_roles() {
        let (_dir, path) = temp_path();
        let (record, _) =
            new_api_token_record(&AccountId::ownerless(), "lead", AuthScope::Work, None);
        {
            let db = Db::connect(&path).expect("connect");
            db.create_api_token(&record).expect("token");
        }
        edit_raw(&path, API_TOKENS, &record.id, |object| {
            object.remove("child_lifetime_hours");
        });
        stamp(&path, 62);
        let db = Db::connect(&path).expect("open version 62");
        let held = db.list_api_tokens().expect("list");
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].child_lifetime_hours, 1);
        assert_eq!(held[0].permissions, record.permissions);
        drop(db);
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        assert!(rows_of(&path, AGENT_ROLES).is_empty());
        assert!(rows_of(&path, AGENT_ROLE_GRANTS).is_empty());
    }

    /// Every key and token id of [`TOKENS_BY_ROLE_BOARD`], as `role id/board id` and token id.
    fn role_index(db: &Db) -> BTreeSet<(String, String)> {
        let txn = db.db.begin_read().expect("read txn");
        let tbl = txn
            .open_multimap_table(TOKENS_BY_ROLE_BOARD)
            .expect("table");
        let mut out = BTreeSet::new();
        for entry in tbl.iter().expect("iter") {
            let (key, values) = entry.expect("entry");
            for value in values {
                out.insert((
                    key.value().to_owned(),
                    value.expect("id").value().to_owned(),
                ));
            }
        }
        out
    }

    /// Issues a token as role `role_id` that reaches `boards`, under a fresh issuer.
    fn role_token(
        db: &Db,
        account: &AccountId,
        role_id: &str,
        name: &str,
        boards: &[&str],
        hours: i64,
    ) -> ApiTokenRecord {
        let issuer = {
            let (record, _) =
                new_api_token_record(account, &format!("lead-{name}"), AuthScope::Provision, None);
            db.create_api_token(&record).expect("issuer");
            record
        };
        let (mut record, _) = new_api_token_record(account, name, AuthScope::Work, None);
        record.parent_id = Some(issuer.id);
        record.boards = BoardAccess::Only(boards.iter().map(|id| (*id).to_string()).collect());
        db.create_api_token_as_role(role_id, record, hours, Utc::now().timestamp())
            .expect("issue")
            .expect("role")
    }

    /// The role index follows a token through issue, revoke, expiry and the board's deletion.
    #[test]
    fn role_index_should_follow_each_way_a_token_ends() {
        let (_dir, path) = temp_path();
        let db = Db::connect(&path).expect("connect");
        let account = db.create_user("alice").expect("account");
        drop_seeded_roles(&db, &account);
        let a = db.create_board(&account, "A").expect("board");
        let b = db.create_board(&account, "B").expect("board");
        let role = db
            .create_agent_role(account.as_str(), "planner", BTreeSet::new())
            .expect("role");

        let both = role_token(&db, &account, &role.id, "both", &[&a, &b], 1);
        let only_a = role_token(&db, &account, &role.id, "only-a", &[&a], 1);
        let expiring = role_token(&db, &account, &role.id, "expiring", &[&b], 0);
        let key = |board: &str, token: &ApiTokenRecord| {
            (format!("{}/{board}", role.id), token.id.clone())
        };
        assert_eq!(
            role_index(&db),
            BTreeSet::from([
                key(&a, &both),
                key(&b, &both),
                key(&a, &only_a),
                key(&b, &expiring),
            ])
        );

        let created = db
            .create_board_by(&account, "C", &both.id)
            .expect("board by token");
        assert!(role_index(&db).contains(&key(&created, &both)));
        assert_eq!(
            db.revoke_role_tokens(account.as_str(), &role.id, Some(&created))
                .expect("revoke"),
            Some(1)
        );
        assert!(role_index(&db).iter().all(|(_, id)| *id != both.id));
        let both = role_token(&db, &account, &role.id, "both-again", &[&a, &b], 1);
        assert!(db.delete_api_token(&account, &only_a.id).expect("revoke"));
        db.prune_expired_issued_tokens(expiring.expires_at.expect("expiry") + 1)
            .expect("prune");
        assert_eq!(
            role_index(&db),
            BTreeSet::from([key(&a, &both), key(&b, &both)])
        );

        db.delete_board(&b).expect("delete board");
        assert_eq!(role_index(&db), BTreeSet::from([key(&a, &both)]));
        assert!(db.delete_api_token(&account, &both.id).expect("revoke"));
        assert!(role_index(&db).is_empty());
    }

    /// Deleting a role leaves no role, grant, index or token row of it, and keeps another role's.
    #[test]
    fn delete_agent_role_should_leave_no_row_of_the_role() {
        let (_dir, path) = temp_path();
        let (account, kept_role, kept_token) = {
            let db = Db::connect(&path).expect("connect");
            let account = db.create_user("alice").expect("account");
            drop_seeded_roles(&db, &account);
            let a = db.create_board(&account, "A").expect("board");
            let b = db.create_board(&account, "B").expect("board");
            let role = db
                .create_agent_role(account.as_str(), "planner", BTreeSet::new())
                .expect("role");
            let kept = db
                .create_agent_role(account.as_str(), "keeper", BTreeSet::new())
                .expect("role");
            for board in [&a, &b] {
                for id in [&role.id, &kept.id] {
                    assert!(
                        db.grant_agent_role(account.as_str(), id, board)
                            .expect("grant")
                    );
                }
            }
            role_token(&db, &account, &role.id, "one", &[&a], 1);
            role_token(&db, &account, &role.id, "two", &[&a, &b], 1);
            let kept_token = role_token(&db, &account, &kept.id, "kept", &[&b], 1);
            assert_eq!(
                db.delete_agent_role("someone-else", &role.id)
                    .expect("foreign"),
                None
            );
            assert_eq!(
                db.delete_agent_role(account.as_str(), &role.id)
                    .expect("delete"),
                Some(2)
            );
            assert_eq!(
                db.delete_agent_role(account.as_str(), &role.id)
                    .expect("again"),
                None
            );
            assert!(
                role_index(&db)
                    .iter()
                    .all(|(key, _)| key.starts_with(&kept.id))
            );
            (account, kept.id, kept_token.id)
        };
        let roles = rows_of(&path, AGENT_ROLES);
        assert_eq!(roles.keys().collect::<Vec<_>>(), [&kept_role]);
        let grants = rows_of(&path, AGENT_ROLE_GRANTS);
        assert!(grants.keys().all(|key| key.starts_with(&kept_role)));
        assert_eq!(grants.len(), 2);
        let db = Db::connect(&path).expect("reopen");
        let tokens = db.list_account_api_tokens(&account).expect("tokens");
        assert!(tokens.iter().any(|token| token.id == kept_token));
        assert_eq!(
            db.list_agent_roles(account.as_str()).expect("roles").len(),
            1
        );
    }

    fn drop_seeded_roles(db: &Db, account: &AccountId) {
        for role in db.list_agent_roles(account.as_str()).expect("roles") {
            db.delete_agent_role(account.as_str(), &role.id)
                .expect("delete");
        }
    }

    fn role_names(db: &Db, account: &AccountId) -> Vec<String> {
        let mut names: Vec<String> = db
            .list_agent_roles(account.as_str())
            .expect("roles")
            .into_iter()
            .map(|role| role.name)
            .collect();
        names.sort();
        names
    }

    fn standard_names() -> Vec<String> {
        let mut names: Vec<String> = STANDARD_ROLES
            .iter()
            .map(|standard| standard.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// A new account holds the standard roles, and a version-74 account with none gets them while
    /// an account that holds a role keeps only that one.
    #[test]
    fn standard_roles_added_once_on_upgrade_and_account_creation() {
        let (_dir, path) = temp_path();
        let (bare, custom) = {
            let db = Db::connect(&path).expect("connect");
            let bare = db.create_user("bare").expect("bare");
            assert_eq!(role_names(&db, &bare), standard_names());
            let custom = db.create_user("custom").expect("custom");
            drop_seeded_roles(&db, &bare);
            for role in db.list_agent_roles(custom.as_str()).expect("roles") {
                if role.name != "planner" {
                    db.delete_agent_role(custom.as_str(), &role.id)
                        .expect("delete");
                }
            }
            (bare, custom)
        };
        stamp(&path, 74);
        let db = Db::connect(&path).expect("open version 74");
        assert_eq!(role_names(&db, &bare), standard_names());
        assert_eq!(role_names(&db, &custom), ["planner"]);
        drop(db);
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    /// A standard role the owner deleted or edited stays that way after a later start.
    #[test]
    fn deleted_standard_role_is_not_re_added() {
        let (_dir, path) = temp_path();
        let (account, edited) = {
            let db = Db::connect(&path).expect("connect");
            let account = db.create_user("owner").expect("account");
            let roles = db.list_agent_roles(account.as_str()).expect("roles");
            let find = |name: &str| roles.iter().find(|role| role.name == name).expect(name);
            db.delete_agent_role(account.as_str(), &find("planner").id)
                .expect("delete");
            let held = BTreeSet::from([Permission::TasksRead]);
            db.update_agent_role(account.as_str(), &find("lead").id, None, Some(held))
                .expect("edit");
            (account, find("lead").id.clone())
        };
        let db = Db::connect(&path).expect("reopen");
        let names = role_names(&db, &account);
        assert!(!names.contains(&"planner".to_string()));
        assert_eq!(names.len(), STANDARD_ROLES.len() - 1);
        let lead = db
            .agent_role(account.as_str(), &edited)
            .expect("read")
            .expect("lead");
        assert_eq!(lead.permissions, BTreeSet::from([Permission::TasksRead]));
    }

    /// A version-66 file opens with the role index filled from its role tokens.
    #[test]
    fn version_66_file_should_open_with_its_role_tokens_indexed() {
        let (_dir, path) = temp_path();
        let expected = {
            let db = Db::connect(&path).expect("connect");
            let account = db.create_user("alice").expect("account");
            drop_seeded_roles(&db, &account);
            let board = db.create_board(&account, "A").expect("board");
            let role = db
                .create_agent_role(account.as_str(), "planner", BTreeSet::new())
                .expect("role");
            let token = role_token(&db, &account, &role.id, "agent", &[&board], 1);
            let txn = db.db.begin_write().expect("write txn");
            txn.delete_multimap_table(TOKENS_BY_ROLE_BOARD)
                .expect("drop");
            txn.commit().expect("commit");
            BTreeSet::from([(format!("{}/{board}", role.id), token.id)])
        };
        stamp(&path, 66);
        let db = Db::connect(&path).expect("open version 66");
        assert_eq!(role_index(&db), expected);
        drop(db);
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
    }

    /// Applies `edit` to the row `key` of `table` as stored.
    fn edit_raw(
        path: &str,
        table: TableDefinition<'_, &str, &[u8]>,
        key: &str,
        edit: impl FnOnce(&mut JsonObject),
    ) {
        let db = Database::create(path).expect("raw open");
        let txn = db.begin_write().expect("write txn");
        {
            let mut tbl = txn.open_table(table).expect("table");
            let mut row: serde_json::Value = {
                let guard = tbl.get(key).expect("get").expect("row");
                serde_json::from_slice(guard.value()).expect("json")
            };
            edit(row.as_object_mut().expect("object"));
            let raw = serde_json::to_vec(&row).expect("json");
            tbl.insert(key, raw.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");
    }

    /// Removes the fields a version-26 file did not store from the row `key` of `table`.
    fn strip_grants(path: &str, table: TableDefinition<'_, &str, &[u8]>, key: &str) {
        edit_raw(path, table, key, |object| {
            object.remove("permissions");
            object.remove("boards");
        });
    }

    /// Takes `tasks.bin` off the row `key` of `table`, as a version-40 file stored it.
    fn strip_bin(path: &str, table: TableDefinition<'_, &str, &[u8]>, key: &str) {
        edit_raw(path, table, key, |object| {
            object["permissions"]
                .as_array_mut()
                .expect("permissions")
                .retain(|name| {
                    name != "tasks.bin" && name != "labels.manage" && name != "epics.manage"
                });
        });
    }

    /// Every row of `table` as JSON, keyed by id.
    fn rows_of(
        path: &str,
        table: TableDefinition<'_, &str, &[u8]>,
    ) -> BTreeMap<String, serde_json::Value> {
        let db = redb::ReadOnlyDatabase::open(path).expect("read-only open");
        let txn = db.begin_read().expect("read txn");
        let tbl = txn.open_table(table).expect("table");
        tbl.iter()
            .expect("iter")
            .map(|item| {
                let (key, value) = item.expect("row");
                let row = serde_json::from_slice(value.value()).expect("json");
                (key.value().to_owned(), row)
            })
            .collect()
    }

    /// A version-40 file's tokens and device requests at the lead or admin preset gain
    /// `tasks.bin`, and a custom set and the lower presets keep what they hold.
    #[test]
    fn version_40_file_should_grant_the_bin_to_lead_and_admin_presets_and_no_custom_set() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let (presets, custom, suspended_id) = {
            let db = Db::connect(&path).expect("connect");
            let mut presets = Vec::new();
            for scope in AuthScope::ALL {
                let (record, _) = new_api_token_record(&account, scope.as_str(), scope, None);
                db.create_api_token(&record).expect("token");
                presets.push((scope, record.id));
            }
            let mut custom = Vec::new();
            for (scope, extra) in [
                (AuthScope::Provision, Permission::BoardsDelete),
                (AuthScope::Work, Permission::TasksDelete),
            ] {
                let (mut record, _) = new_api_token_record(&account, "custom", scope, None);
                record.permissions.insert(extra);
                db.create_api_token(&record).expect("token");
                custom.push(record.id);
            }
            let (record, _) =
                new_api_token_record(&account, "suspended", AuthScope::Provision, None);
            db.create_api_token(&record).expect("token");
            let txn = db.db.begin_write().expect("write txn");
            move_row_to(&txn, API_TOKENS, &record.id, SUSPENDED_TOKENS, &record.id)
                .expect("suspend");
            txn.commit().expect("commit");
            (presets, custom, record.id)
        };
        let device = DeviceAuthRow {
            device_code_hash: "hash".to_string(),
            user_code: "CODE0000".to_string(),
            client_name: "device".to_string(),
            scope: AuthScope::Provision,
            permissions: AuthScope::Provision.permissions(),
            boards: BoardAccess::All,
            status: DeviceAuthStatus::Pending,
            created_at: 1,
            expires_at: i64::MAX,
            last_poll_at: None,
            token_expires_at: None,
            account_id: None,
            client_network: None,
            role_id: None,
            requested_role: None,
            requested_boards: Vec::new(),
            requested_hours: None,
            reason: None,
        };
        let value = serde_json::to_value(&device).expect("json");
        put_raw(&path, DEVICE_AUTHS, &device.device_code_hash, &value);
        for (table, key) in presets
            .iter()
            .map(|(_, id)| (API_TOKENS, id))
            .chain(custom.iter().map(|id| (API_TOKENS, id)))
            .chain([
                (SUSPENDED_TOKENS, &suspended_id),
                (DEVICE_AUTHS, &device.device_code_hash),
            ])
        {
            strip_bin(&path, table, key);
        }
        stamp(&path, 40);

        let db = Db::connect(&path).expect("open version 40");
        let holds_bin = |table, id: &str| {
            let row: ApiTokenRecord = read_one(&db.db, table, id).expect("read").expect("row");
            row.permissions.contains(&Permission::TasksBin)
        };
        for (scope, id) in &presets {
            let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, id)
                .expect("read")
                .expect("row");
            assert!(token.permissions == scope.permissions(), "{scope:?}");
        }
        for id in &custom {
            assert!(!holds_bin(API_TOKENS, id), "a custom set gained tasks.bin");
        }
        assert!(holds_bin(SUSPENDED_TOKENS, &suspended_id));
        let request: DeviceAuthRow = read_one(&db.db, DEVICE_AUTHS, &device.device_code_hash)
            .expect("read")
            .expect("row");
        assert!(request.permissions.contains(&Permission::TasksBin));
    }

    /// A version-50 file's tokens at the lead or admin preset gain `labels.manage` and keep
    /// `tasks.bin`, and a custom set and the lower presets keep what they hold.
    #[test]
    fn version_50_file_should_grant_label_management_to_lead_and_admin_presets_and_no_custom_set() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let (presets, custom) = {
            let db = Db::connect(&path).expect("connect");
            let mut presets = Vec::new();
            for scope in AuthScope::ALL {
                let (record, _) = new_api_token_record(&account, scope.as_str(), scope, None);
                db.create_api_token(&record).expect("token");
                presets.push((scope, record.id));
            }
            let (mut record, _) = new_api_token_record(&account, "custom", AuthScope::Work, None);
            record.permissions.insert(Permission::TasksBin);
            db.create_api_token(&record).expect("token");
            (presets, record.id)
        };
        let strip = |object: &mut serde_json::Map<String, serde_json::Value>| {
            object["permissions"]
                .as_array_mut()
                .expect("permissions")
                .retain(|name| name != "labels.manage" && name != "epics.manage");
        };
        for key in presets.iter().map(|(_, id)| id).chain([&custom]) {
            edit_raw(&path, API_TOKENS, key, strip);
        }
        stamp(&path, 50);

        let db = Db::connect(&path).expect("open version 50");
        for (scope, id) in &presets {
            let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, id)
                .expect("read")
                .expect("row");
            assert!(token.permissions == scope.permissions(), "{scope:?}");
        }
        let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, &custom)
            .expect("read")
            .expect("row");
        assert!(!token.permissions.contains(&Permission::LabelsManage));
        assert!(token.permissions.contains(&Permission::TasksBin));
    }

    /// A version-57 file's tokens at the lead or admin preset gain `epics.manage`, and a custom set
    /// and the lower presets keep what they hold.
    #[test]
    fn version_57_file_should_grant_epic_management_to_lead_and_admin_presets_and_no_custom_set() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let (presets, custom) = {
            let db = Db::connect(&path).expect("connect");
            let mut presets = Vec::new();
            for scope in AuthScope::ALL {
                let (record, _) = new_api_token_record(&account, scope.as_str(), scope, None);
                db.create_api_token(&record).expect("token");
                presets.push((scope, record.id));
            }
            let (mut record, _) = new_api_token_record(&account, "custom", AuthScope::Work, None);
            record.permissions.insert(Permission::TasksBin);
            db.create_api_token(&record).expect("token");
            (presets, record.id)
        };
        let strip = |object: &mut serde_json::Map<String, serde_json::Value>| {
            object["permissions"]
                .as_array_mut()
                .expect("permissions")
                .retain(|name| name != "epics.manage");
        };
        for key in presets.iter().map(|(_, id)| id).chain([&custom]) {
            edit_raw(&path, API_TOKENS, key, strip);
        }
        stamp(&path, 57);

        let db = Db::connect(&path).expect("open version 57");
        for (scope, id) in &presets {
            let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, id)
                .expect("read")
                .expect("row");
            assert!(token.permissions == scope.permissions(), "{scope:?}");
        }
        let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, &custom)
            .expect("read")
            .expect("row");
        assert!(!token.permissions.contains(&Permission::EpicsManage));
        assert!(token.permissions.contains(&Permission::TasksBin));
    }

    /// A version-57 file's tokens, which hold neither `epics.manage` nor `inbound.manage`, gain
    /// both at the admin preset and `epics.manage` alone at the lead preset.
    #[test]
    fn version_57_file_should_grant_inbound_management_to_admin_presets_beside_epics() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let presets = {
            let db = Db::connect(&path).expect("connect");
            let mut presets = Vec::new();
            for scope in AuthScope::ALL {
                let (record, _) = new_api_token_record(&account, scope.as_str(), scope, None);
                db.create_api_token(&record).expect("token");
                presets.push((scope, record.id));
            }
            presets
        };
        let strip = |object: &mut serde_json::Map<String, serde_json::Value>| {
            object["permissions"]
                .as_array_mut()
                .expect("permissions")
                .retain(|name| name != "epics.manage" && name != "inbound.manage");
        };
        for (_, id) in &presets {
            edit_raw(&path, API_TOKENS, id, strip);
        }
        stamp(&path, 57);

        let db = Db::connect(&path).expect("open version 57");
        for (scope, id) in &presets {
            let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, id)
                .expect("read")
                .expect("row");
            assert!(token.permissions == scope.permissions(), "{scope:?}");
        }
    }

    /// A version-58 file's tokens at the admin preset gain `inbound.manage`, and a custom set and
    /// the lower presets keep what they hold.
    #[test]
    fn version_58_file_should_grant_inbound_management_to_admin_presets_and_no_custom_set() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let (presets, custom) = {
            let db = Db::connect(&path).expect("connect");
            let mut presets = Vec::new();
            for scope in AuthScope::ALL {
                let (record, _) = new_api_token_record(&account, scope.as_str(), scope, None);
                db.create_api_token(&record).expect("token");
                presets.push((scope, record.id));
            }
            let (mut record, _) = new_api_token_record(&account, "custom", AuthScope::Work, None);
            record.permissions.insert(Permission::TasksBin);
            db.create_api_token(&record).expect("token");
            (presets, record.id)
        };
        let strip = |object: &mut serde_json::Map<String, serde_json::Value>| {
            object["permissions"]
                .as_array_mut()
                .expect("permissions")
                .retain(|name| name != "inbound.manage");
        };
        for key in presets.iter().map(|(_, id)| id).chain([&custom]) {
            edit_raw(&path, API_TOKENS, key, strip);
        }
        stamp(&path, 58);

        let db = Db::connect(&path).expect("open version 58");
        for (scope, id) in &presets {
            let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, id)
                .expect("read")
                .expect("row");
            assert!(token.permissions == scope.permissions(), "{scope:?}");
        }
        let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, &custom)
            .expect("read")
            .expect("row");
        assert!(!token.permissions.contains(&Permission::InboundManage));
        assert!(token.permissions.contains(&Permission::TasksBin));
    }

    /// A version-58 admin token without `inbound.manage` or `feeds.manage` keeps its trimmed set
    /// through the upgrade.
    #[test]
    fn version_58_file_should_leave_a_trimmed_admin_set_without_inbound_or_feeds_management() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let trimmed = {
            let db = Db::connect(&path).expect("connect");
            let (record, _) = new_api_token_record(&account, "trimmed", AuthScope::Admin, None);
            db.create_api_token(&record).expect("token");
            record.id
        };
        edit_raw(&path, API_TOKENS, &trimmed, |object| {
            object["permissions"]
                .as_array_mut()
                .expect("permissions")
                .retain(|name| name != "inbound.manage" && name != "feeds.manage");
        });
        stamp(&path, 58);

        let db = Db::connect(&path).expect("open version 58");
        let token: ApiTokenRecord = read_one(&db.db, API_TOKENS, &trimmed)
            .expect("read")
            .expect("row");
        assert!(!token.permissions.contains(&Permission::InboundManage));
        assert!(!token.permissions.contains(&Permission::FeedsManage));
    }

    fn upgraded_preset_token(scope: AuthScope, version: u64, newer: &[&str]) -> ApiTokenRecord {
        let (_dir, path) = temp_path();
        let id = {
            let db = Db::connect(&path).expect("connect");
            let (record, _) =
                new_api_token_record(&AccountId::ownerless(), scope.as_str(), scope, None);
            db.create_api_token(&record).expect("token");
            record.id
        };
        let reset = [
            "tokens.issue",
            "roles.grant",
            "tasks.create",
            "tasks.edit",
            "tasks.move",
            "tasks.claim",
            "subtasks.edit",
            "subtasks.check",
        ];
        edit_raw(&path, API_TOKENS, &id, |object| {
            let held = object["permissions"].as_array_mut().expect("permissions");
            held.retain(|name| !reset.iter().chain(newer).any(|dropped| name == dropped));
            held.push(serde_json::json!("tasks.write"));
            held.push(serde_json::json!("subtasks.write"));
        });
        stamp(&path, version);

        let db = Db::connect(&path).expect("open older version");
        read_one(&db.db, API_TOKENS, &id)
            .expect("read")
            .expect("row")
    }

    #[test]
    fn version_58_admin_token_should_split_its_writes_and_gain_inbound_management_but_not_issuing()
    {
        let token = upgraded_preset_token(AuthScope::Admin, 58, &["inbound.manage"]);
        let mut expected = AuthScope::Admin.permissions();
        expected.remove(&Permission::TokensIssue);
        expected.remove(&Permission::RolesGrant);
        assert_eq!(token.permissions, expected);
    }

    #[test]
    fn version_40_lead_token_should_gain_later_lead_permissions_but_not_issuing() {
        let later = ["tasks.bin", "labels.manage", "epics.manage"];
        let token = upgraded_preset_token(AuthScope::Provision, 40, &later);
        let mut expected = AuthScope::Provision.permissions();
        expected.remove(&Permission::TokensIssue);
        expected.remove(&Permission::RolesGrant);
        assert_eq!(token.permissions, expected);
    }

    #[test]
    fn version_69_lead_token_should_gain_subtasks_delete_and_a_custom_set_should_not() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let (lead, work, custom) = {
            let db = Db::connect(&path).expect("connect");
            let mut ids = Vec::new();
            for scope in [AuthScope::Provision, AuthScope::Work, AuthScope::Provision] {
                let (record, _) = new_api_token_record(&account, scope.as_str(), scope, None);
                db.create_api_token(&record).expect("token");
                ids.push(record.id);
            }
            (ids[0].clone(), ids[1].clone(), ids[2].clone())
        };
        let without = |object: &mut serde_json::Map<String, serde_json::Value>| {
            let held = object["permissions"].as_array_mut().expect("permissions");
            held.retain(|name| name != "subtasks.delete");
        };
        edit_raw(&path, API_TOKENS, &lead, without);
        edit_raw(&path, API_TOKENS, &work, without);
        edit_raw(&path, API_TOKENS, &custom, |object| {
            without(object);
            let held = object["permissions"].as_array_mut().expect("permissions");
            held.retain(|name| name != "tasks.bin");
        });
        stamp(&path, 69);

        let db = Db::connect(&path).expect("open version 69");
        let token = |id: &str| -> ApiTokenRecord {
            read_one(&db.db, API_TOKENS, id)
                .expect("read")
                .expect("row")
        };
        assert!(
            token(&lead)
                .permissions
                .contains(&Permission::SubtasksDelete)
        );
        assert!(
            !token(&work)
                .permissions
                .contains(&Permission::SubtasksDelete)
        );
        assert!(
            !token(&custom)
                .permissions
                .contains(&Permission::SubtasksDelete)
        );
    }

    #[test]
    fn version_70_file_should_fit_the_lead_role_and_leave_other_roles_and_tokens() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let held = BTreeSet::from([Permission::TasksRead, Permission::TimeWrite]);
        let (lead, planner, token) = {
            let db = Db::connect(&path).expect("connect");
            let lead = db
                .create_agent_role(account.as_str(), "lead", held.clone())
                .expect("lead");
            let planner = db
                .create_agent_role(account.as_str(), "planner", held.clone())
                .expect("planner");
            let (record, _) = new_api_token_record(&account, "issued", AuthScope::Provision, None);
            db.create_api_token(&record).expect("token");
            (lead.id, planner.id, record)
        };
        stamp(&path, 70);

        let db = Db::connect(&path).expect("open version 70");
        let role = |id: &str| -> AgentRoleRow {
            read_one(&db.db, AGENT_ROLES, id)
                .expect("read")
                .expect("row")
        };
        let fitted = lead_role_permissions();
        assert_eq!(role(&lead).permissions, fitted);
        assert_eq!(role(&planner).permissions, held);
        let kept: ApiTokenRecord = read_one(&db.db, API_TOKENS, &token.id)
            .expect("read")
            .expect("row");
        assert_eq!(kept.permissions, token.permissions);
    }

    #[test]
    fn version_72_file_should_widen_an_untouched_lead_role_and_keep_an_edited_one() {
        let held = |extra: &[Permission]| -> BTreeSet<Permission> {
            let mut set: BTreeSet<Permission> = lead_role_permissions()
                .difference(&BTreeSet::from(LEAD_ROLE_V73_ADDED))
                .copied()
                .filter(|permission| {
                    !matches!(
                        permission,
                        Permission::SeatsManage | Permission::SeatsTake | Permission::LinksDelete
                    )
                })
                .collect();
            set.extend(extra.iter().copied());
            set
        };
        let edited = held(&[Permission::TimeRead]);
        let (_dir, path) = temp_path();
        let (untouched, owned, other) = {
            let db = Db::connect(&path).expect("connect");
            let untouched = db.create_agent_role("a", "lead", held(&[])).expect("lead");
            let owned = db
                .create_agent_role("b", "lead", edited.clone())
                .expect("lead");
            let other = db
                .create_agent_role("a", "planner", held(&[]))
                .expect("planner");
            (untouched.id, owned.id, other.id)
        };
        stamp(&path, 72);

        let db = Db::connect(&path).expect("open version 72");
        let role = |id: &str| -> AgentRoleRow {
            read_one(&db.db, AGENT_ROLES, id)
                .expect("read")
                .expect("row")
        };
        let mut widened = held(&[]);
        widened.extend(LEAD_ROLE_V73_ADDED);
        assert_eq!(role(&untouched).permissions, widened);
        assert_eq!(role(&owned).permissions, edited);
        assert_eq!(role(&other).permissions, held(&[]));
    }

    mod grant_standard_roles_seats_take {
        use super::*;

        #[test]
        fn should_give_a_version_77_lead_role_both_seats_permissions() {
            let (_dir, path) = temp_path();
            let id = {
                let db = Db::connect(&path).expect("connect");
                let mut held = lead_role_permissions();
                held.remove(&Permission::SeatsManage);
                held.remove(&Permission::SeatsTake);
                db.create_agent_role("a", "lead", held).expect("lead").id
            };
            stamp(&path, 77);

            let db = Db::connect(&path).expect("open version 77");
            let role: AgentRoleRow = read_one(&db.db, AGENT_ROLES, &id)
                .expect("read")
                .expect("row");
            assert_eq!(role.permissions, lead_role_permissions());
        }

        fn without_take(role: &str) -> BTreeSet<Permission> {
            let mut held = STANDARD_ROLES
                .iter()
                .find(|standard| standard.name == role)
                .expect("standard role")
                .permissions();
            held.remove(&Permission::SeatsTake);
            held
        }

        fn drop_take(object: &mut JsonObject) {
            let held = object["permissions"].as_array_mut().expect("permissions");
            held.retain(|name| name != "seats.take");
        }

        #[test]
        fn should_grant_seats_take_to_preset_tokens_and_matching_stored_roles_once_at_upgrade() {
            let (_dir, path) = temp_path();
            let account = AccountId::ownerless();
            let edited = {
                let mut held = without_take("reviewer");
                held.insert(Permission::TimeRead);
                held
            };
            let (work, lead, custom, reviewer, verifier, lead_role, edited_role, planner) = {
                let db = Db::connect(&path).expect("connect");
                let mut tokens = Vec::new();
                for scope in [AuthScope::Work, AuthScope::Provision, AuthScope::Work] {
                    let (record, _) = new_api_token_record(&account, "old", scope, None);
                    db.create_api_token(&record).expect("token");
                    tokens.push(record.id);
                }
                let role = |name: &str, held: BTreeSet<Permission>| {
                    db.create_agent_role(account.as_str(), name, held)
                        .expect("role")
                        .id
                };
                (
                    tokens[0].clone(),
                    tokens[1].clone(),
                    tokens[2].clone(),
                    role("reviewer", without_take("reviewer")),
                    role("verifier", without_take("verifier")),
                    role("lead", without_take("lead")),
                    role("tester", edited.clone()),
                    role("planner", without_take("planner")),
                )
            };
            for id in [&work, &lead, &custom] {
                edit_raw(&path, API_TOKENS, id, drop_take);
            }
            edit_raw(&path, API_TOKENS, &custom, |object| {
                let held = object["permissions"].as_array_mut().expect("permissions");
                held.retain(|name| name != "tasks.move");
            });
            stamp(&path, 79);

            let db = Db::connect(&path).expect("open version 79");
            let token = |id: &str| -> ApiTokenRecord {
                read_one(&db.db, API_TOKENS, id)
                    .expect("read")
                    .expect("row")
            };
            let role = |id: &str| -> AgentRoleRow {
                read_one(&db.db, AGENT_ROLES, id)
                    .expect("read")
                    .expect("row")
            };
            assert!(token(&work).permissions.contains(&Permission::SeatsTake));
            assert!(token(&lead).permissions.contains(&Permission::SeatsTake));
            assert!(!token(&custom).permissions.contains(&Permission::SeatsTake));
            for (id, name) in [
                (&reviewer, "reviewer"),
                (&verifier, "verifier"),
                (&lead_role, "lead"),
            ] {
                let standard = STANDARD_ROLES
                    .iter()
                    .find(|standard| standard.name == name)
                    .expect("standard role");
                assert_eq!(role(id).permissions, standard.permissions(), "{name}");
            }
            assert_eq!(role(&edited_role).permissions, edited);
            assert_eq!(role(&planner).permissions, without_take("planner"));

            drop(db);
            edit_raw(&path, AGENT_ROLES, &reviewer, drop_take);
            edit_raw(&path, API_TOKENS, &work, drop_take);
            let db = Db::connect(&path).expect("reopen at version 80");
            let kept: AgentRoleRow = read_one(&db.db, AGENT_ROLES, &reviewer)
                .expect("read")
                .expect("row");
            assert_eq!(kept.permissions, without_take("reviewer"));
            let kept: ApiTokenRecord = read_one(&db.db, API_TOKENS, &work)
                .expect("read")
                .expect("row");
            assert!(!kept.permissions.contains(&Permission::SeatsTake));
        }
    }

    fn upgraded_token_holding(held: &[&str]) -> ApiTokenRecord {
        let (_dir, path) = temp_path();
        let id = {
            let db = Db::connect(&path).expect("connect");
            let (record, _) =
                new_api_token_record(&AccountId::ownerless(), "old", AuthScope::Read, None);
            db.create_api_token(&record).expect("token");
            record.id
        };
        let rewrite = |object: &mut serde_json::Map<String, serde_json::Value>| {
            object["permissions"] = serde_json::json!(held);
        };
        edit_raw(&path, API_TOKENS, &id, rewrite);
        stamp(&path, 61);

        let db = Db::connect(&path).expect("open version 61");
        read_one(&db.db, API_TOKENS, &id)
            .expect("read")
            .expect("row")
    }

    #[test]
    fn stored_tasks_write_should_migrate_to_all_parts() {
        let token = upgraded_token_holding(&["tasks.read", "tasks.write", "comments.write"]);
        assert_eq!(
            token.permissions,
            BTreeSet::from([
                Permission::TasksRead,
                Permission::TasksCreate,
                Permission::TasksEdit,
                Permission::TasksMove,
                Permission::TasksClaim,
                Permission::CommentsWrite,
            ])
        );
    }

    #[test]
    fn stored_subtasks_write_should_migrate_to_both_parts() {
        let token = upgraded_token_holding(&["tasks.read", "subtasks.write"]);
        assert_eq!(
            token.permissions,
            BTreeSet::from([
                Permission::TasksRead,
                Permission::SubtasksEdit,
                Permission::SubtasksCheck,
            ])
        );
    }

    #[test]
    fn stored_write_permissions_should_migrate_in_suspended_tokens_and_device_requests() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let suspended = {
            let db = Db::connect(&path).expect("connect");
            let (record, _) = new_api_token_record(&account, "old", AuthScope::Read, None);
            db.create_api_token(&record).expect("token");
            let txn = db.db.begin_write().expect("write txn");
            move_row_to(&txn, API_TOKENS, &record.id, SUSPENDED_TOKENS, &record.id)
                .expect("suspend");
            txn.commit().expect("commit");
            record.id
        };
        let request = DeviceAuthRow {
            device_code_hash: "hash".to_string(),
            user_code: "CODE0001".to_string(),
            client_name: "device".to_string(),
            scope: AuthScope::Read,
            permissions: BTreeSet::new(),
            boards: BoardAccess::All,
            status: DeviceAuthStatus::Pending,
            created_at: 1,
            expires_at: i64::MAX,
            last_poll_at: None,
            token_expires_at: None,
            account_id: None,
            client_network: None,
            role_id: None,
            requested_role: None,
            requested_boards: Vec::new(),
            requested_hours: None,
            reason: None,
        };
        put_raw(
            &path,
            DEVICE_AUTHS,
            "hash",
            &serde_json::to_value(&request).expect("json"),
        );
        let held = serde_json::json!(["tasks.read", "tasks.write", "subtasks.write"]);
        let rewrite = |object: &mut serde_json::Map<String, serde_json::Value>| {
            object["permissions"] = held.clone();
        };
        edit_raw(&path, SUSPENDED_TOKENS, &suspended, rewrite);
        edit_raw(&path, DEVICE_AUTHS, "hash", rewrite);
        stamp(&path, 61);

        let db = Db::connect(&path).expect("open version 61");
        let expected = BTreeSet::from([
            Permission::TasksRead,
            Permission::TasksCreate,
            Permission::TasksEdit,
            Permission::TasksMove,
            Permission::TasksClaim,
            Permission::SubtasksEdit,
            Permission::SubtasksCheck,
        ]);
        let token: ApiTokenRecord = read_one(&db.db, SUSPENDED_TOKENS, &suspended)
            .expect("read")
            .expect("row");
        assert_eq!(token.permissions, expected);
        let request: DeviceAuthRow = read_one(&db.db, DEVICE_AUTHS, "hash")
            .expect("read")
            .expect("row");
        assert_eq!(request.permissions, expected);
    }

    /// A version-26 file's tokens and device requests gain the permissions of their scope's
    /// preset and all boards, and keep every other field and their lookups.
    #[test]
    fn version_26_file_should_gain_the_preset_of_each_scope_and_all_boards() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let mut live = Vec::new();
        let (suspended_id, suspended_raw) = {
            let db = Db::connect(&path).expect("connect");
            for scope in AuthScope::ALL {
                let (record, raw) = new_api_token_record(&account, scope.as_str(), scope, None);
                db.create_api_token(&record).expect("token");
                live.push((record.id, raw));
            }
            let (record, raw) =
                new_api_token_record(&account, "suspended", AuthScope::Provision, Some(i64::MAX));
            db.create_api_token(&record).expect("token");
            let txn = db.db.begin_write().expect("write txn");
            move_row_to(&txn, API_TOKENS, &record.id, SUSPENDED_TOKENS, &record.id)
                .expect("suspend");
            txn.commit().expect("commit");
            (record.id, raw)
        };
        for (n, scope) in AuthScope::ALL.into_iter().enumerate() {
            let approved = n % 2 == 1;
            let row = DeviceAuthRow {
                device_code_hash: format!("hash-{n}"),
                user_code: format!("CODE{n:04}"),
                client_name: format!("device-{n}"),
                scope,
                permissions: scope.permissions(),
                boards: BoardAccess::All,
                status: if approved {
                    DeviceAuthStatus::Approved
                } else {
                    DeviceAuthStatus::Pending
                },
                created_at: 1,
                expires_at: i64::MAX,
                last_poll_at: None,
                token_expires_at: approved.then_some(99),
                account_id: approved.then(|| account.as_str().to_string()),
                client_network: Some("net".to_string()),
                role_id: None,
                requested_role: None,
                requested_boards: Vec::new(),
                requested_hours: None,
                reason: None,
            };
            let value = serde_json::to_value(&row).expect("json");
            put_raw(&path, DEVICE_AUTHS, &row.device_code_hash, &value);
        }
        let tables = [
            (API_TOKENS, "api_tokens"),
            (SUSPENDED_TOKENS, "suspended_tokens"),
            (DEVICE_AUTHS, "device_auths"),
        ];
        for (table, _) in &tables {
            for key in rows_of(&path, *table).keys() {
                strip_grants(&path, *table, key);
            }
        }
        stamp(&path, 26);
        let before: Vec<_> = tables
            .iter()
            .map(|(table, _)| rows_of(&path, *table))
            .collect();
        assert_eq!(before[0].len(), 4);
        assert_eq!(before[1].len(), 1);
        assert_eq!(before[2].len(), 4);

        let db = Db::connect(&path).expect("open version 26");
        let upgraded = |mut preset: BTreeSet<Permission>| {
            preset.remove(&Permission::TokensIssue);
            preset.remove(&Permission::RolesGrant);
            preset
        };
        let lead = upgraded(Preset::Lead.permissions());
        let suspended: ApiTokenRecord = read_one(&db.db, SUSPENDED_TOKENS, &suspended_id)
            .expect("read")
            .expect("suspended token");
        assert!(suspended.scope == AuthScope::Provision);
        assert!(suspended.permissions == lead);
        assert!(suspended.boards == BoardAccess::All);
        assert!(db.is_suspended_token(&suspended_raw).expect("suspended"));
        for (id, raw) in &live {
            let found = db.find_api_token(raw).expect("find").expect("token");
            assert!(found.id == *id, "a token no longer finds its own row");
            assert!(found.permissions == upgraded(found.scope.preset().permissions()));
            assert!(found.boards == BoardAccess::All);
            assert!(found.scope != AuthScope::Provision || found.permissions == lead);
        }
        drop(db);
        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));

        for ((table, name), rows) in tables.iter().zip(&before) {
            let after = rows_of(&path, *table);
            assert!(after.keys().eq(rows.keys()), "{name} keys changed");
            for (key, old) in rows {
                let scope = old["scope"].as_str().and_then(AuthScope::parse);
                let scope = scope.expect("scope");
                let mut new = after[key].clone();
                let object = new.as_object_mut().expect("object");
                let permissions = serde_json::json!(upgraded(scope.permissions()));
                assert!(
                    object.remove("permissions") == Some(permissions),
                    "{name} row holds the wrong permissions"
                );
                assert!(
                    object.remove("boards") == Some(serde_json::json!("all")),
                    "{name} row holds the wrong boards"
                );
                assert!(new == *old, "{name} row changed beyond the new fields");
            }
        }
    }

    /// A row with a scope outside the four fails the upgrade, which leaves the file stamped and
    /// its rows as they were.
    #[test]
    fn version_26_file_with_an_unknown_scope_should_fail_and_stay_untouched() {
        let (_dir, path) = temp_path();
        let account = AccountId::ownerless();
        let mut ids = Vec::new();
        {
            let db = Db::connect(&path).expect("connect");
            for name in ["good", "odd"] {
                let (record, _) = new_api_token_record(&account, name, AuthScope::Read, None);
                db.create_api_token(&record).expect("token");
                ids.push(record.id);
            }
        }
        for id in &ids {
            strip_grants(&path, API_TOKENS, id);
        }
        edit_raw(&path, API_TOKENS, &ids[1], |object| {
            object.insert("scope".to_string(), serde_json::json!("root"));
        });
        stamp(&path, 26);
        let before = rows_of(&path, API_TOKENS);

        assert!(Db::connect(&path).is_err());
        assert_eq!(stamp_of(&path), Some(26));
        assert!(
            rows_of(&path, API_TOKENS) == before,
            "the failed upgrade wrote rows"
        );
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
                        kind: ActorKind::User,
                        parent_token: None,
                        role_id: None,
                        role_name: None,
                        label: None,
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
        let (moved, untouched, stamped) = {
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
                    (HistoryKind::Moved, "2026-01-02T03:04:05+00:00"),
                    (HistoryKind::Comment, "2026-02-01T00:00:00+00:00"),
                    (HistoryKind::Moved, "2026-01-09T10:00:00+00:00"),
                    (HistoryKind::Comment, "2026-02-02T00:00:00+00:00"),
                ] {
                    row.history.push(TaskEvent {
                        id: at.to_string(),
                        kind,
                        detail: String::new(),
                        actor: "a".to_string(),
                        created_at: at.to_string(),
                        column_id: None,
                        token_name: None,
                        from: None,
                        to: None,
                        parent_token: None,
                        role_id: None,
                        role_name: None,
                        label: None,
                    });
                }
            })
            .expect("add history");
            mutate(&db.db, TASKS, &stamped, |row: &mut TaskRow| {
                row.column_entered_at = Some("2026-03-03T00:00:00+00:00".to_string());
            })
            .expect("set stamp");
            (moved, untouched, stamped)
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

    fn strip_task_fields(path: &str, fields: &[&str]) {
        let raw = Database::create(path).expect("raw open");
        let txn = raw.begin_write().expect("write txn");
        {
            let mut tbl = txn.open_table(TASKS).expect("tasks");
            let rows: Vec<(String, Vec<u8>)> = tbl
                .iter()
                .expect("iter")
                .map(|item| {
                    let (key, value) = item.expect("row");
                    (key.value().to_string(), value.value().to_vec())
                })
                .collect();
            for (id, bytes) in rows {
                let mut row: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
                let object = row.as_object_mut().expect("object");
                for field in fields {
                    assert!(object.remove(*field).is_some(), "{field} was stored");
                }
                let bytes = serde_json::to_vec(&row).expect("bytes");
                tbl.insert(id.as_str(), bytes.as_slice()).expect("insert");
            }
        }
        txn.commit().expect("commit");
    }

    fn kind_of(db: &Db, id: &str) -> TaskKind {
        db.task(id).expect("read").expect("task kept").kind
    }

    #[test]
    fn version_fifty_seven_file_should_make_epics_of_exactly_the_parents_once() {
        let (_dir, path) = temp_path();
        let (parent, grandparent, child, sibling, blocker, blocked, lone) = {
            let db = Db::connect(&path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Old")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let make = |name: &str| db.create_task(&column, None, name, 1, None).expect("task");
            let tasks = (
                make("Parent"),
                make("Grandparent"),
                make("Child"),
                make("Sibling"),
                make("Blocker"),
                make("Blocked"),
                make("Lone"),
            );
            let link = |holder: &str, kind: LinkKind, target: &str| {
                let outcome = db
                    .add_task_link(holder, kind, target, None, Actor::SYSTEM)
                    .expect("link");
                assert!(matches!(outcome, LinkAddOutcome::Added(_)));
            };
            for id in [&tasks.0, &tasks.1] {
                db.apply_task_change(id, Actor::SYSTEM, |row| {
                    row.kind = TaskKind::Epic;
                    ((), true)
                })
                .expect("epic");
            }
            link(&tasks.2, LinkKind::Parent, &tasks.0);
            link(&tasks.3, LinkKind::Parent, &tasks.0);
            link(&tasks.0, LinkKind::Parent, &tasks.1);
            link(&tasks.5, LinkKind::BlockedBy, &tasks.4);
            tasks
        };
        strip_task_fields(&path, &["kind", "closed"]);
        stamp(&path, 57);

        drop(Db::connect(&path).expect("open version-57 file"));

        assert_eq!(stamp_of(&path), Some(SCHEMA_VERSION));
        let db = Db::connect(&path).expect("reopen");
        let mut kept = db.stored_bytes_by_account().expect("usage");
        kept.retain(|_, bytes| *bytes > 0);
        let recount = db.db.begin_write().expect("write");
        txn_recount_usage(&recount).expect("recount");
        let counted: HashMap<String, u64> = recount
            .open_table(ACCOUNT_USAGE)
            .expect("table")
            .iter()
            .expect("iter")
            .map(|item| {
                let (account, bytes) = item.expect("row");
                (account.value().to_string(), bytes.value())
            })
            .collect();
        recount.abort().expect("abort");
        assert_eq!(kept, counted, "usage was recounted");
        for id in [&parent, &grandparent] {
            assert_eq!(kind_of(&db, id), TaskKind::Epic);
        }
        for id in [&child, &sibling, &blocker, &blocked, &lone] {
            assert_eq!(kind_of(&db, id), TaskKind::Task);
        }
        for id in [
            &parent,
            &grandparent,
            &child,
            &sibling,
            &blocker,
            &blocked,
            &lone,
        ] {
            assert!(!db.task(id).expect("read").expect("task kept").closed);
        }

        db.apply_task_change(&parent, Actor::SYSTEM, |row| {
            row.kind = TaskKind::Task;
            ((), true)
        })
        .expect("demote");
        drop(db);
        drop(Db::connect(&path).expect("open version-58 file"));
        let db = Db::connect(&path).expect("reopen");
        assert_eq!(kind_of(&db, &parent), TaskKind::Task, "no second upgrade");
        assert_eq!(kind_of(&db, &grandparent), TaskKind::Epic);
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

    use redb::{Key, MultimapTableHandle, Value};
    use serde::de::value::{Error as ShapeError, StrDeserializer};
    use serde::de::{
        DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Visitor,
    };
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::models::{
        AgentRoleGrantRow, AgentRoleRow, Avatar, AvatarKind, BoardEventRow, BoardMemberRow,
        BoardRole, BreakActivity, DiskLimits, FieldColumn, InviteRow, LimitGroup, NotificationRow,
        OutboxRow, OwnerHolder, ParentToken, PruneSettings, PruneStatus, QuestionChoice, Quotas,
        RateLimits, RegistrationMode, Responsible, SeatVerdict, SubtaskTemplate, TimeFormat,
        TimerMode, WebhookEvent, WeekStart,
    };
    use crate::permissions::StandardKey;

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
            shape::<AgentRoleGrantRow>("AgentRoleGrantRow"),
            shape::<AgentRoleRow>("AgentRoleRow"),
            shape::<AttachmentVersion>("AttachmentVersion"),
            shape::<ApiTokenRecord>("ApiTokenRecord"),
            shape::<BackupSettings>("BackupSettings"),
            shape::<BackupStatus>("BackupStatus"),
            shape::<BinRow>("BinRow"),
            shape::<BoardAccess>("BoardAccess"),
            shape::<BoardEventRow>("BoardEventRow"),
            shape::<BoardMemberRow>("BoardMemberRow"),
            shape::<BoardRole>("BoardRole"),
            shape::<BoardFeed>("BoardFeed"),
            shape::<BoardRow>("BoardRow"),
            shape::<BoardTemplateRow>("BoardTemplateRow"),
            shape::<BreakActivity>("BreakActivity"),
            shape::<ColorRow>("ColorRow"),
            shape::<ColumnRow>("ColumnRow"),
            shape::<ColumnStage>("ColumnStage"),
            shape::<StandardKey>("StandardKey"),
            shape::<DeviceAuthRow>("DeviceAuthRow"),
            shape::<DiskLimits>("DiskLimits"),
            shape::<FieldColumn>("FieldColumn"),
            shape::<FieldDefRow>("FieldDefRow"),
            shape::<AccountStatus>("AccountStatus"),
            shape::<InboundAddressRow>("InboundAddressRow"),
            shape::<InstanceSettings>("InstanceSettings"),
            shape::<InviteRow>("InviteRow"),
            shape::<LimitGroup>("LimitGroup"),
            shape::<NotificationRow>("NotificationRow"),
            shape::<ParentToken>("ParentToken"),
            shape::<PasskeyRow>("PasskeyRow"),
            shape::<PruneSettings>("PruneSettings"),
            shape::<PruneStatus>("PruneStatus"),
            shape::<Quotas>("Quotas"),
            shape::<RateLimits>("RateLimits"),
            shape::<RecoveryCodeRow>("RecoveryCodeRow"),
            shape::<ReportBookmark>("ReportBookmark"),
            shape::<RegistrationMode>("RegistrationMode"),
            shape::<OutboxRow>("OutboxRow"),
            shape::<Owner>("Owner"),
            shape::<OwnerHolder>("OwnerHolder"),
            shape::<Responsible>("Responsible"),
            shape::<Seat>("Seat"),
            shape::<SeatSlot>("SeatSlot"),
            shape::<SeatVerdict>("SeatVerdict"),
            shape::<SessionRow>("SessionRow"),
            shape::<Settings>("Settings"),
            shape::<Avatar>("Avatar"),
            shape::<TimeFormat>("TimeFormat"),
            shape::<WeekStart>("WeekStart"),
            shape::<AvatarKind>("AvatarKind"),
            shape::<LaneLimit>("LaneLimit"),
            shape::<Subtask>("Subtask"),
            shape::<SubtaskTemplate>("SubtaskTemplate"),
            shape::<SwimlaneRow>("SwimlaneRow"),
            shape::<TaskAttachment>("TaskAttachment"),
            shape::<TaskComment>("TaskComment"),
            shape::<CommentQuestion>("CommentQuestion"),
            shape::<CommentReaction>("CommentReaction"),
            shape::<QuestionAnswer>("QuestionAnswer"),
            shape::<QuestionChoice>("QuestionChoice"),
            shape::<TaskEvent>("TaskEvent"),
            shape::<TaskKind>("TaskKind"),
            shape::<TaskRow>("TaskRow"),
            shape::<TimeEntryRow>("TimeEntryRow"),
            shape::<TimerMode>("TimerMode"),
            shape::<UserRow>("UserRow"),
            shape::<WebhookEvent>("WebhookEvent"),
            shape::<WebhookRow>("WebhookRow"),
        ];

        let mut tables = vec![
            table(USERS),
            table(USERNAMES),
            table(SESSIONS),
            table(BOARDS),
            table(COLUMNS),
            table(SWIMLANES),
            table(TASKS),
            table(BIN),
            table(TIME_ENTRIES),
            table(SETTINGS),
            table(ACTIVE_TIMER),
            table(API_TOKENS),
            table(TASK_COLORS),
            table(TEMPLATES),
            table(TEMPLATE_SNAPSHOTS),
            table(META),
            table(FIELD_DEFS),
            table(WEBHOOKS),
            table(INBOUND_ADDRESSES),
            table(INBOUND_BY_HASH),
            table(AGENT_ROLES),
            table(AGENT_ROLE_GRANTS),
            table(WEBHOOK_OUTBOX),
            table(DEVICE_AUTHS),
            table(PASSKEYS),
            table(ACCOUNT_SETTINGS),
            table(AVATARS),
            table(RECOVERY_CODES),
            table(INVITES),
            table(ACTIVITY),
            table(SUSPENDED_TOKENS),
            table(BOARD_ACTIVITY),
            table(ACCOUNT_USAGE),
            table(BOARD_EVENTS),
            table(EVENT_WAKEUPS),
            table(NOTIFICATIONS),
            table(FEED_TOKENS),
            table(PASSKEYS_BY_CREDENTIAL),
            multimap(TOKENS_BY_ACCOUNT),
            multimap(TOKENS_BY_ROLE_BOARD),
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
            multimap(WEBHOOKS_BY_BOARD),
            multimap(INBOUND_BY_BOARD),
            multimap(AGENT_ROLES_BY_ACCOUNT),
            multimap(GRANTS_BY_BOARD),
            multimap(GRANTS_BY_ROLE),
            multimap(WEBHOOK_OUTBOX_BY_HOOK),
            multimap(LINKS_BY_TARGET),
            table(BOARD_MEMBERS),
            multimap(MEMBERS_BY_BOARD),
            multimap(BOARDS_BY_MEMBER),
            multimap(BIN_BY_BOARD),
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
                report_bookmarks: Vec::new(),
                seats: Vec::new(),
                limit_groups: Vec::new(),
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
                    role: None,
                    token_name: None,
                    role_id: None,
                    role_name: None,
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
            device: String::new(),
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
        let fresh = db.create_session(&account, "").expect("session");
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
        let current = db.create_session(&account, "").expect("session");
        let elsewhere = db.create_session(&account, "").expect("session");
        let theirs = db.create_session(&other, "").expect("session");
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
    fn list_sessions_should_name_each_device_and_mark_only_the_current_one() {
        let (_dir, db, account) = open();
        let other = db.create_user("bob").expect("member");
        let phone = db
            .create_session(&account, "Safari on iOS")
            .expect("session");
        let desk = db
            .create_session(&account, "Firefox on Linux")
            .expect("session");
        db.create_session(&other, "Chrome on Windows")
            .expect("session");
        let long = "x".repeat(MAX_SESSION_DEVICE_CHARS + 50);
        let capped = db.create_session(&account, &long).expect("session");

        let listed = db
            .list_sessions(&account, Some(SessionToken::new(&desk)))
            .expect("list");
        assert_eq!(listed.len(), 3);
        let current: Vec<&SessionListing> = listed.iter().filter(|row| row.current).collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].device, "Firefox on Linux");
        assert!(listed.iter().any(|row| row.device == "Safari on iOS"));
        assert!(listed.iter().all(|row| row.device != "Chrome on Windows"));
        assert!(
            listed
                .iter()
                .any(|row| row.device.chars().count() == MAX_SESSION_DEVICE_CHARS)
        );
        let ids = listed.iter().map(|row| row.id.as_str());
        for token in [&phone, &desk, &capped] {
            assert!(ids.clone().all(|id| !token.starts_with(id)));
            assert!(ids.clone().all(|id| id.len() == SESSION_PUBLIC_ID_CHARS));
        }
        assert!(
            db.list_sessions(&account, None)
                .expect("list")
                .iter()
                .all(|row| !row.current)
        );
    }

    #[test]
    fn end_session_should_end_only_the_named_session_of_that_account() {
        let (_dir, db, account) = open();
        let other = db.create_user("bob").expect("member");
        let current = db
            .create_session(&account, "Firefox on Linux")
            .expect("session");
        let elsewhere = db
            .create_session(&account, "Safari on iOS")
            .expect("session");
        let theirs = db
            .create_session(&other, "Chrome on Windows")
            .expect("session");
        let id_of = |owner: &AccountId, device: &str| {
            db.list_sessions(owner, None)
                .expect("list")
                .into_iter()
                .find(|row| row.device == device)
                .expect("row")
                .id
        };
        let elsewhere_id = id_of(&account, "Safari on iOS");
        let current_id = id_of(&account, "Firefox on Linux");
        let theirs_id = id_of(&other, "Chrome on Windows");

        assert_eq!(
            db.end_session(&account, &theirs_id, Some(SessionToken::new(&current)))
                .expect("end"),
            SessionEndOutcome::NotFound
        );
        assert_eq!(
            db.end_session(&account, &current_id, Some(SessionToken::new(&current)))
                .expect("end"),
            SessionEndOutcome::Current
        );
        assert_eq!(
            db.end_session(&account, &elsewhere_id, Some(SessionToken::new(&current)))
                .expect("end"),
            SessionEndOutcome::Ended
        );
        assert_eq!(
            db.end_session(&account, &elsewhere_id, Some(SessionToken::new(&current)))
                .expect("end"),
            SessionEndOutcome::NotFound
        );
        assert_eq!(
            db.end_session(&account, "", Some(SessionToken::new(&current)))
                .expect("end"),
            SessionEndOutcome::NotFound
        );
        assert!(stored(&db, &current));
        assert!(!stored(&db, &elsewhere));
        assert!(stored(&db, &theirs));
        assert_eq!(indexed(&db, &account), [hash_session_token(&current)]);
    }

    #[test]
    fn session_is_fresh_should_hold_for_the_window_after_a_passkey_check() {
        let (_dir, db, account) = open();
        let created = db.create_session(&account, "").expect("session");
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
            db.create_session(&account, "").expect("session"),
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
        let ended = db.create_session(&account, "").expect("session");
        let kept = db.create_session(&account, "").expect("session");
        db.delete_session(&ended).expect("delete");
        assert!(!stored(&db, &ended));
        assert!(stored(&db, &kept));
    }

    #[test]
    fn create_session_should_refuse_a_suspended_account() {
        let (_dir, db, _admin) = open();
        let member = db.create_user("bob").expect("member");
        let before = db.create_session(&member, "").expect("session");
        let outcome = db
            .apply_account_action(member.as_str(), AccountAction::Suspend)
            .expect("suspend");
        assert!(matches!(outcome, AdminActionOutcome::Done(_)));
        assert!(!stored(&db, &before));

        let refusal = db
            .create_session(&member, "")
            .expect_err("suspended account");
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
            .create_session(&member, "")
            .expect("a pending account signs in");
        assert!(stored(&db, &token));
    }

    #[test]
    fn create_session_should_refuse_an_absent_account() {
        let (_dir, db, _admin) = open();
        let ghost = AccountId::new("ghost");
        let refusal = db.create_session(&ghost, "").expect_err("absent account");
        assert!(refusal.downcast_ref::<SessionRefused>().is_some());
        assert!(session_rows(&db).is_empty());
    }

    #[test]
    fn create_session_should_stay_gone_after_a_suspension_and_unsuspend() {
        let (_dir, db, _admin) = open();
        let member = db.create_user("bob").expect("member");
        let before = db.create_session(&member, "").expect("session");
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
                    let _ = db.create_session(&member, "");
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
            db.create_session(&member, "")
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
            let session = db.create_session(&account, "").expect("session");
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
    use crate::models::BoardAccess;

    /// Returns a pending request from `network`, numbered `n` for its codes.
    fn request(n: usize, network: &str, now: i64) -> DeviceAuthRow {
        DeviceAuthRow {
            device_code_hash: format!("hash-{n}"),
            user_code: format!("CODE{n:04}"),
            client_name: "agent".to_string(),
            scope: AuthScope::Read,
            permissions: AuthScope::Read.permissions(),
            boards: BoardAccess::All,
            status: DeviceAuthStatus::Pending,
            created_at: now,
            expires_at: now + Db::DEVICE_CODE_TTL_SECONDS,
            last_poll_at: None,
            token_expires_at: None,
            account_id: None,
            client_network: Some(network.to_string()),
            role_id: None,
            requested_role: None,
            requested_boards: Vec::new(),
            requested_hours: None,
            reason: None,
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
    fn decide_device_auth_should_store_the_permissions_of_the_granted_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Db::connect(dir.path().join("devices.redb").to_str().expect("utf8")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let now = Utc::now().timestamp();
        let asked = DeviceAuthRow {
            scope: AuthScope::Admin,
            permissions: AuthScope::Admin.permissions(),
            ..request(0, "a network", now)
        };
        assert!(matches!(
            db.create_device_auth(&asked, now).expect("create"),
            DeviceAuthCreateOutcome::Created
        ));
        let grant = DeviceGrant {
            scope: AuthScope::Read,
            permissions: AuthScope::Read.permissions(),
            boards: BoardAccess::All,
            expires_at: None,
            role_id: None,
        };
        assert!(
            db.decide_device_auth(&admin, &asked.user_code, Some(&grant), now)
                .expect("approve")
        );
        let stored: DeviceAuthRow = read_one(&db.db, DEVICE_AUTHS, &asked.device_code_hash)
            .expect("read")
            .expect("request");
        assert!(stored.scope == AuthScope::Read);
        assert!(stored.permissions == AuthScope::Read.permissions());
        assert!(stored.boards == BoardAccess::All);
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
            permissions: AuthScope::Read.permissions(),
            boards: BoardAccess::All,
            expires_at: None,
            role_id: None,
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
    use crate::models::BoardAccess;

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
            permissions: AuthScope::Read.permissions(),
            boards: BoardAccess::All,
            status: DeviceAuthStatus::Pending,
            created_at: now,
            expires_at: now + Db::DEVICE_CODE_TTL_SECONDS,
            last_poll_at: None,
            token_expires_at: None,
            account_id: None,
            client_network: None,
            role_id: None,
            requested_role: None,
            requested_boards: Vec::new(),
            requested_hours: None,
            reason: None,
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
            permissions: AuthScope::Read.permissions(),
            boards: BoardAccess::All,
            expires_at: None,
            role_id: None,
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
            let block = vec![7u8; 4 * MIB as usize];
            for fill in [true, false] {
                let txn = db.db.begin_write().expect("write");
                {
                    let mut table = txn.open_table(SETTINGS).expect("table");
                    for key in ["bulk0", "bulk1", "bulk2", "bulk3", "bulk4"] {
                        if fill {
                            table.insert(key, block.as_slice()).expect("insert");
                        } else {
                            table.remove(key).expect("remove");
                        }
                    }
                }
                txn.commit().expect("commit");
            }
            assert!(file_mib() >= u64::from(LIMIT_MIB), "{} MiB", file_mib());
            column
        };

        let db = Db::connect(path).expect("reopen");
        assert!(file_mib() < u64::from(LIMIT_MIB), "{} MiB", file_mib());
        let guard = DiskGuard::beside(&db);
        guard.apply(&DiskLimits {
            max_database_mib: Some(LIMIT_MIB),
            min_free_mib: DiskLimits::FLOOR_OFF_MIB,
            ..DiskLimits::default()
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
            role: None,
            token_name: None,
            role_id: None,
            role_name: None,
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

#[cfg(test)]
mod responsible_fill_tests {
    use super::*;

    fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        (dir, db)
    }

    fn older_task(db: &Db, responsible: Option<Responsible>) -> String {
        let board = db
            .create_board(&AccountId::ownerless(), "Plans")
            .expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let task = db.create_task(&column, None, "Own", 1, None).expect("task");
        let members = vec!["a".to_string(), "b".to_string()];
        mutate_task(&db.db, &task, |row| {
            row.member_ids = members.clone();
            row.responsible = responsible.clone();
        })
        .expect("mutate");
        task
    }

    fn clear_marker(db: &Db) {
        let txn = db.db.begin_write().expect("write txn");
        txn.open_table(SETTINGS)
            .expect("settings table")
            .remove(RESPONSIBLE_FILL_DONE)
            .expect("remove marker");
        txn.commit().expect("commit");
    }

    fn responsible_of(db: &Db, id: &str) -> Option<Responsible> {
        db.task(id)
            .expect("read")
            .expect("task")
            .responsible
            .clone()
    }

    #[test]
    fn fill_responsible_should_make_the_first_member_of_an_older_task_responsible() {
        let (_dir, db) = test_db();
        let id = older_task(&db, None);
        clear_marker(&db);

        db.fill_responsible().expect("fill");

        assert_eq!(
            responsible_of(&db, &id),
            Some(Responsible::Member { id: "a".into() })
        );
    }

    #[test]
    fn fill_responsible_should_never_replace_a_responsible_a_person_set() {
        let (_dir, db) = test_db();
        let session = Responsible::Session {
            name: "coordinator".into(),
        };
        let id = older_task(&db, Some(session.clone()));
        clear_marker(&db);

        db.fill_responsible().expect("fill");

        assert_eq!(responsible_of(&db, &id), Some(session));
    }

    #[test]
    fn fill_responsible_should_change_nothing_when_it_runs_twice() {
        let (_dir, db) = test_db();
        let id = older_task(&db, None);
        clear_marker(&db);
        db.fill_responsible().expect("fill");
        mutate_task(&db.db, &id, |row| row.responsible = None).expect("clear");

        db.fill_responsible().expect("second fill");

        assert_eq!(responsible_of(&db, &id), None);
    }

    #[test]
    fn fill_responsible_should_leave_a_task_with_a_logged_responsible_change_alone() {
        let (_dir, db) = test_db();
        let id = older_task(&db, None);
        mutate_task(&db.db, &id, |row| {
            row.history.push(TaskEvent {
                id: "e1".to_string(),
                kind: HistoryKind::ResponsibleUpdated,
                detail: "Cleared the responsible".to_string(),
                actor: "admin".to_string(),
                created_at: "2026-10-31T10:00:00+00:00".to_string(),
                column_id: None,
                token_name: None,
                from: None,
                to: None,
                parent_token: None,
                role_id: None,
                role_name: None,
                label: None,
            });
        })
        .expect("log");
        clear_marker(&db);

        db.fill_responsible().expect("fill");

        assert_eq!(responsible_of(&db, &id), None);
    }

    #[test]
    fn fill_responsible_should_leave_a_task_without_members_alone() {
        let (_dir, db) = test_db();
        let id = older_task(&db, None);
        mutate_task(&db.db, &id, |row| row.member_ids.clear()).expect("clear members");
        clear_marker(&db);

        db.fill_responsible().expect("fill");

        assert_eq!(responsible_of(&db, &id), None);
    }
}

#[cfg(test)]
mod owner_migration_tests {
    use super::*;
    use crate::models::OwnerHolder;

    fn claim(claimant: &str, expires_at: DateTime<Utc>) -> TaskClaim {
        TaskClaim {
            claimant: claimant.to_string(),
            role: None,
            harness: None,
            model: None,
            jj_workspace: None,
            jj_change_id: None,
            session_id: None,
            fence: 1,
            started_at: Utc::now().to_rfc3339(),
            heartbeat_at: Utc::now().to_rfc3339(),
            expires_at: expires_at.to_rfc3339(),
        }
    }

    fn members(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn owner_migration_prefers_live_claim_then_responsible_then_first_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let soon = Utc::now() + chrono::Duration::hours(1);
        let past = Utc::now() - chrono::Duration::hours(1);
        let human = |id: &str| OwnerHolder::Human { id: id.to_string() };
        let ids = {
            let db = Db::connect(path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Plans")
                .expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            let mut ids = Vec::new();
            for name in ["claimed", "expired", "session", "member", "none"] {
                ids.push(db.create_task(&column, None, name, 1, None).expect("task"));
            }
            let session = Responsible::Session {
                name: "coordinator".to_string(),
            };
            let rows: [(Option<TaskClaim>, Option<Responsible>, Vec<String>); 5] = [
                (
                    Some(claim("agent-a", soon)),
                    Some(Responsible::Member { id: "x".into() }),
                    members(&["m1"]),
                ),
                (
                    Some(claim("agent-b", past)),
                    Some(Responsible::Member { id: "x".into() }),
                    members(&["m1"]),
                ),
                (None, Some(session), members(&["m1"])),
                (None, None, members(&["m1", "m2"])),
                (None, None, Vec::new()),
            ];
            for (id, (claim, responsible, member_ids)) in ids.iter().zip(rows) {
                mutate_task(&db.db, id, |row| {
                    row.claim = claim.clone();
                    row.responsible = responsible.clone();
                    row.member_ids = member_ids.clone();
                    row.owner = None;
                })
                .expect("mutate");
            }
            let txn = db.db.begin_write().expect("write txn");
            txn.open_table(META)
                .expect("meta table")
                .insert(SCHEMA_VERSION_KEY, 73)
                .expect("stamp");
            txn.commit().expect("commit");
            ids
        };

        let db = Db::connect(path).expect("reconnect");

        let owners: Vec<Option<Owner>> = ids
            .iter()
            .map(|id| db.task(id).expect("read").expect("task").owner.clone())
            .collect();
        assert_eq!(
            owners[0],
            Some(Owner {
                holder: OwnerHolder::Agent {
                    token_id: None,
                    name: "agent-a".to_string(),
                },
                lease_expires_at: Some(soon.to_rfc3339()),
            })
        );
        let unleased = |holder| {
            Some(Owner {
                holder,
                lease_expires_at: None,
            })
        };
        assert_eq!(owners[1], unleased(human("x")));
        assert_eq!(
            owners[2],
            unleased(OwnerHolder::Agent {
                token_id: None,
                name: "coordinator".to_string(),
            })
        );
        assert_eq!(owners[3], unleased(human("m1")));
        assert_eq!(owners[4], None);
    }
}

#[cfg(test)]
mod role_key_upgrade_tests {
    use super::*;
    use crate::permissions::StandardKey;

    fn stamp(path: &str, version: u64) {
        let db = Database::create(path).expect("raw open");
        let txn = db.begin_write().expect("write txn");
        txn.open_table(META)
            .expect("meta table")
            .insert(SCHEMA_VERSION_KEY, version)
            .expect("stamp");
        txn.commit().expect("commit");
    }

    fn key_of(db: &Db, role: &str) -> Option<StandardKey> {
        db.agent_role(AccountId::ownerless().as_str(), role)
            .expect("read")
            .expect("role")
            .standard
    }

    #[test]
    fn should_stamp_standard_role_keys_once_at_upgrade() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let account = AccountId::ownerless();
        let (reviewer, scribe) = {
            let db = Db::connect(path).expect("connect");
            let held = BTreeSet::from([Permission::TasksRead]);
            let reviewer = db
                .create_agent_role(account.as_str(), "reviewer", held.clone())
                .expect("reviewer");
            let scribe = db
                .create_agent_role(account.as_str(), "scribe", held)
                .expect("scribe");
            assert_eq!(reviewer.standard, None);
            (reviewer.id, scribe.id)
        };
        stamp(path, 79);
        let db = Db::connect(path).expect("upgrade");
        assert_eq!(key_of(&db, &reviewer), Some(StandardKey::Reviewer));
        assert_eq!(key_of(&db, &scribe), None);
        db.update_agent_role(account.as_str(), &reviewer, Some("checker"), None)
            .expect("rename");
        let held = BTreeSet::from([Permission::TasksRead]);
        let custom = db
            .create_agent_role(account.as_str(), "reviewer", held)
            .expect("custom");
        drop(db);
        let db = Db::connect(path).expect("reopen");
        assert_eq!(key_of(&db, &reviewer), Some(StandardKey::Reviewer));
        assert_eq!(key_of(&db, &custom.id), None);
    }
}

#[cfg(test)]
mod stage_upgrade_tests {
    use super::*;

    fn stamp(path: &str, version: u64) {
        let db = Database::create(path).expect("raw open");
        let txn = db.begin_write().expect("write txn");
        txn.open_table(META)
            .expect("meta table")
            .insert(SCHEMA_VERSION_KEY, version)
            .expect("stamp");
        txn.commit().expect("commit");
    }

    fn stage_of(db: &Db, column: &str) -> Option<ColumnStage> {
        db.column(column).expect("read").expect("column").stage
    }

    #[test]
    fn should_assign_stages_from_names_once_at_upgrade() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let path = path.to_str().expect("utf8 path");
        let (working, doing, review) = {
            let db = Db::connect(path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Legacy")
                .expect("board");
            let working = db
                .create_column(&board, "In Progress", None)
                .expect("working");
            let doing = db.create_column(&board, "Doing", None).expect("doing");
            let review = db.create_column(&board, "In Review", None).expect("review");
            assert_eq!(stage_of(&db, &working), None);
            (working, doing, review)
        };
        stamp(path, 78);
        let db = Db::connect(path).expect("upgrade");
        assert_eq!(stage_of(&db, &working), Some(ColumnStage::InProgress));
        assert_eq!(stage_of(&db, &doing), None);
        assert_eq!(stage_of(&db, &review), Some(ColumnStage::Review));
        db.rename_column(&working, "Building").expect("rename");
        drop(db);
        let db = Db::connect(path).expect("reopen");
        assert_eq!(stage_of(&db, &working), Some(ColumnStage::InProgress));
    }

    #[test]
    fn should_carry_stages_through_a_template() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = AccountId::ownerless();
        let board = db.create_board(&owner, "Source").expect("board");
        let doing = db.create_column(&board, "Doing", None).expect("doing");
        let verify = db.create_column(&board, "Verify", None).expect("verify");
        let patch = ColumnPatch {
            stage: Some(Some(ColumnStage::Review)),
            ..ColumnPatch::default()
        };
        db.update_column(&doing, &patch).expect("stage");
        let template = db
            .save_board_as_template(&owner, &board, "Staged", "")
            .expect("save")
            .expect("template");
        let copy = db
            .apply_template(&owner, &template, "Copy")
            .expect("apply")
            .expect("board");
        let stages: Vec<(String, Option<ColumnStage>)> = db
            .list_columns(&copy)
            .expect("columns")
            .into_iter()
            .map(|column| (column.name, column.stage))
            .collect();
        assert_eq!(
            stages,
            vec![
                ("Doing".to_string(), Some(ColumnStage::Review)),
                ("Verify".to_string(), None),
            ]
        );
        assert_eq!(stage_of(&db, &verify), None);
    }
}
