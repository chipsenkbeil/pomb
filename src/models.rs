use std::fmt;

use chrono::{DateTime, Utc};
use serde::de::{self, Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use webauthn_rs::prelude::Passkey;

/// Pomodoro size of a task. `Many` (stored as 4) means ">3 pomodori".
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Size {
    One = 1,
    Two = 2,
    Three = 3,
    Many = 4,
}

impl Size {
    /// Convert a stored integer to a [`Size`], defaulting to [`Size::One`].
    pub fn from_i64(value: i64) -> Self {
        match value {
            2 => Size::Two,
            3 => Size::Three,
            4 => Size::Many,
            _ => Size::One,
        }
    }

    /// Human label, e.g. "2 Pomodori" (KanbanFlow's spelling).
    pub fn label(self) -> &'static str {
        match self {
            Size::One => "1 Pomodoro",
            Size::Two => "2 Pomodori",
            Size::Three => "3 Pomodori",
            Size::Many => ">3 Pomodori",
        }
    }
}

// ---- Row types: one per table, serialized as JSON values in redb ----

/// Board config key holding the main swimlane's id.
pub const MAIN_SWIMLANE_KEY: &str = "main_swimlane_id";

/// Board config key set while the board shows its swimlane names folded.
pub const LANE_NAMES_FOLDED_KEY: &str = "lane_names_folded";

/// Seconds in a day, for expiry arithmetic.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// Returns `count` followed by `noun`, which takes a plural `s` unless `count` is one.
#[must_use]
pub fn count_noun(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// How much of one kind of user input a write may store.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cap {
    /// Most characters in one text.
    Chars(usize),
    /// Most bytes in one text, as UTF-8.
    Bytes(usize),
    /// Most entries in one list.
    Items(usize),
}

impl fmt::Display for Cap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chars(max) => write!(f, "{max} characters"),
            Self::Bytes(max) => write!(f, "{max} bytes"),
            Self::Items(max) => write!(f, "{max} entries"),
        }
    }
}

/// Refusal of user input past its [`Cap`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TooLong {
    /// The input as the request names it, such as `name` or `labels`.
    pub field: &'static str,
    /// The most it may hold.
    pub cap: Cap,
}

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} must be at most {}", self.field, self.cap)
    }
}

impl std::error::Error for TooLong {}

/// One kind of user input and the most of it a write may store.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TextCap {
    /// The input as the request names it.
    pub field: &'static str,
    /// The most it may hold.
    pub cap: Cap,
}

impl TextCap {
    /// An attachment's MIME type.
    pub const ATTACHMENT_MIME: Self = Self::chars("mime", 200);
    /// An attachment's file name.
    pub const ATTACHMENT_NAME: Self = Self::bytes("name", 255);
    /// Most break activities, in the account settings.
    pub const BREAK_ACTIVITIES: Self = Self::items("break_activities", 50);
    /// A break activity's description, as long as the activity dialog allows.
    pub const BREAK_ACTIVITY_DESCRIPTION: Self = Self::chars("break_activities.description", 200);
    /// A break activity's id.
    pub const BREAK_ACTIVITY_ID: Self = Self::chars("break_activities.id", 100);
    /// A break activity's name, as long as the activity dialog allows.
    pub const BREAK_ACTIVITY_NAME: Self = Self::chars("break_activities.name", 80);
    /// Who holds a task claim.
    pub const CLAIMANT: Self = Self::chars("claimant", 200);
    /// The harness a task claim runs in.
    pub const CLAIM_HARNESS: Self = Self::chars("harness", 200);
    /// The jj change a task claim works on.
    pub const CLAIM_JJ_CHANGE_ID: Self = Self::chars("jj_change_id", 200);
    /// The jj workspace a task claim works in.
    pub const CLAIM_JJ_WORKSPACE: Self = Self::chars("jj_workspace", 200);
    /// The model a task claim runs.
    pub const CLAIM_MODEL: Self = Self::chars("model", 200);
    /// The role a task claim plays.
    pub const CLAIM_ROLE: Self = Self::chars("role", 200);
    /// The session a task claim belongs to.
    pub const CLAIM_SESSION_ID: Self = Self::chars("session_id", 200);
    /// A color's description.
    pub const COLOR_DESCRIPTION: Self = Self::chars("description", 500);
    /// A column's name.
    pub const COLUMN_NAME: Self = Self::chars("column name", 200);
    /// The settings bag of a board or a column, as JSON.
    pub const CONFIG: Self = Self::bytes("config", 16 * 1024);
    /// A task's or a column's description.
    pub const DESCRIPTION: Self = Self::bytes("description", 64 * 1024);
    /// A task's due-date repeat text.
    pub const DUE_REPEAT: Self = Self::chars("due_repeat", 200);
    /// A time entry's note.
    pub const ENTRY_NOTE: Self = Self::bytes("note", 4 * 1024);
    /// Most columns of one table field.
    pub const FIELD_COLUMNS: Self = Self::items("columns", 50);
    /// A table column's label.
    pub const FIELD_COLUMN_NAME: Self = Self::chars("columns.label", 200);
    /// A custom field's label, which names it in the dialog.
    pub const FIELD_NAME: Self = Self::chars("label", 200);
    /// One option of a select field or of a table column.
    pub const FIELD_OPTION: Self = Self::chars("options", 200);
    /// Most options of one select field or table column.
    pub const FIELD_OPTIONS: Self = Self::items("options", 200);
    /// A text value of a custom field, or one cell of a table field.
    pub const FIELD_VALUE: Self = Self::bytes("value", 4 * 1024);
    /// One preset reason a timer may stop early, in the account settings, as long as the Timer
    /// settings reason editor allows. A reason typed when stopping is a [`TextCap::STOP_REASON`].
    pub const INTERRUPT_REASON: Self = Self::chars("interrupt_reasons", 80);
    /// Most reasons a timer may stop early, in the account settings.
    pub const INTERRUPT_REASONS: Self = Self::items("interrupt_reasons", 50);
    /// One task label.
    pub const LABEL: Self = Self::chars("labels", 50);
    /// Most labels on one task.
    pub const LABELS: Self = Self::items("labels", 50);
    /// Most members on one task.
    pub const MEMBERS: Self = Self::items("member_ids", 50);
    /// Why a timer session stopped early.
    pub const STOP_REASON: Self = Self::bytes("reason", 1024);
    /// A subtask's name.
    pub const SUBTASK_NAME: Self = Self::chars("subtask name", 500);
    /// A swimlane's description.
    pub const SWIMLANE_DESCRIPTION: Self = Self::chars("swimlane description", 500);
    /// A swimlane's name.
    pub const SWIMLANE_NAME: Self = Self::chars("swimlane name", 200);
    /// A task's name.
    pub const TASK_NAME: Self = Self::chars("name", 500);
    /// A saved template's name.
    pub const TEMPLATE_NAME: Self = Self::chars("name", 200);

    /// Returns the cap of at most `max` entries in the list `field`.
    const fn items(field: &'static str, max: usize) -> Self {
        Self {
            field,
            cap: Cap::Items(max),
        }
    }

    /// Returns the cap of at most `max` characters in the text `field`.
    const fn chars(field: &'static str, max: usize) -> Self {
        Self {
            field,
            cap: Cap::Chars(max),
        }
    }

    /// Returns the cap of at most `max` UTF-8 bytes in the text `field`.
    const fn bytes(field: &'static str, max: usize) -> Self {
        Self {
            field,
            cap: Cap::Bytes(max),
        }
    }

    /// Returns the refusal of this input.
    fn refusal(self) -> TooLong {
        TooLong {
            field: self.field,
            cap: self.cap,
        }
    }

    /// Returns [`TooLong`] when `text` holds more than this cap allows. A list cap counts no text.
    pub fn check(self, text: &str) -> Result<(), TooLong> {
        let over = match self.cap {
            Cap::Chars(max) => text.len() > max && text.chars().count() > max,
            Cap::Bytes(max) => text.len() > max,
            Cap::Items(_) => false,
        };
        if over {
            return Err(self.refusal());
        }
        Ok(())
    }

    /// Returns [`TooLong`] when `items` holds more entries than this cap allows. A text cap counts
    /// no entries.
    pub fn check_count<T>(self, items: &[T]) -> Result<(), TooLong> {
        match self.cap {
            Cap::Items(max) if items.len() > max => Err(self.refusal()),
            _ => Ok(()),
        }
    }
}

/// Returns [`TooLong`] when `labels` adds labels past [`TextCap::LABELS`] or holds a label past
/// [`TextCap::LABEL`] that `held`, the labels as stored, does not.
fn check_labels(labels: &[String], held: Option<&[String]>) -> Result<(), TooLong> {
    if labels.len() > held.map_or(0, <[String]>::len) {
        TextCap::LABELS.check_count(labels)?;
    }
    for label in labels {
        if held.is_none_or(|held| !held.contains(label)) {
            TextCap::LABEL.check(label)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardRow {
    pub id: String,
    pub name: String,
    pub position: i64,
    /// The account the board belongs to, empty until the instance's first account claims it.
    #[serde(default)]
    pub owner_id: String,
    /// Opaque bag for board-level settings the UI manages itself
    /// (e.g. legend visibility). Defaults to "{}" for older rows.
    #[serde(default = "default_board_config")]
    pub config_json: String,
    /// The id of the API token that created the board, empty for a board a browser session
    /// created and for one that predates the field.
    #[serde(default)]
    pub created_by_token: String,
}

impl BoardRow {
    /// Reads a boolean flag from the board config bag.
    pub fn config_bool(&self, key: &str) -> bool {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&self.config_json)
            .ok()
            .and_then(|m| m.get(key).and_then(serde_json::Value::as_bool))
            .unwrap_or(false)
    }

    /// Returns the board's main swimlane among `lanes`, the lane the board shows without a label.
    ///
    /// A board with no stored id falls back to its first lane by position,
    /// without storing the choice.
    pub fn main_swimlane(&self, lanes: &[SwimlaneRow]) -> Option<String> {
        if let Some(id) = self.main_swimlane_id() {
            return lanes.iter().any(|lane| lane.id == id).then_some(id);
        }
        lanes
            .iter()
            .min_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)))
            .map(|lane| lane.id.clone())
    }

    /// Reports whether the board shows its swimlane names folded into a narrow rotated column.
    pub fn lane_names_folded(&self) -> bool {
        self.config_bool(LANE_NAMES_FOLDED_KEY)
    }

    /// Returns the id stored for the board's main swimlane.
    pub fn main_swimlane_id(&self) -> Option<String> {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&self.config_json)
            .ok()
            .and_then(|m| {
                m.get(MAIN_SWIMLANE_KEY)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
    }

    /// Calendar-feed bearer token, stored in the board's opaque config bag. The bag is never
    /// serialized to clients: export-data exposes only the board id/name and the config view only
    /// exposes legend_visible/archived/due_time, so the raw token only leaves the server via the
    /// authenticated feed-link endpoint.
    pub fn feed_token(&self) -> Option<String> {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&self.config_json)
            .ok()
            .and_then(|m| {
                m.get("calendar_feed_token")
                    .and_then(serde_json::Value::as_str)
                    .map(|s| s.to_string())
            })
    }

    /// The time a date-only due date gets on this board: the `due_time` "HH:MM" entry in the config
    /// bag, 5:00 PM when unset.
    pub fn due_time(&self) -> chrono::NaiveTime {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&self.config_json)
            .ok()
            .and_then(|m| {
                m.get("due_time")
                    .and_then(|v| v.as_str().and_then(parse_due_time))
            })
            .unwrap_or_else(default_due_time)
    }
}

/// KanbanFlow's default "Due date time" for a board: 5:00 PM.
pub fn default_due_time() -> chrono::NaiveTime {
    chrono::NaiveTime::from_hms_opt(17, 0, 0).expect("valid time")
}

/// Parses a board "Due date time" written as zero-padded 24-hour "HH:MM".
pub fn parse_due_time(raw: &str) -> Option<chrono::NaiveTime> {
    let time = chrono::NaiveTime::parse_from_str(raw, "%H:%M").ok()?;
    (time.format("%H:%M").to_string() == raw).then_some(time)
}

fn default_board_config() -> String {
    "{}".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColumnRow {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i64,
    pub wip_limit: Option<i64>,
    /// When true, moving a task into this column stamps `completed_at`;
    /// moving out clears it. (Replaces the old name-based "Done" check.)
    pub is_done: bool,
    /// Column description shown in the column dialog/settings.
    /// Defaults to "" for rows written before this field existed.
    #[serde(default)]
    pub description: String,
    /// Collapsed columns hide their task cells. Defaults to false.
    #[serde(default)]
    pub collapsed: bool,
    /// Opaque bag for column-dialog settings the UI manages itself
    /// (column sums, sorting, group-by-date, display options).
    /// Defaults to "{}" for older rows.
    #[serde(default = "default_column_config")]
    pub config_json: String,
    /// Names the column had before each rename, oldest first, so reports can map history recorded
    /// under an old name. Empty for rows written before the field existed.
    #[serde(default)]
    pub former_names: Vec<String>,
}

impl ColumnRow {
    /// Most former names a column remembers.
    pub const MAX_FORMER_NAMES: usize = 50;

    /// Renames the column to `name` and remembers the old name in `former_names`, forgetting the
    /// oldest past [`ColumnRow::MAX_FORMER_NAMES`].
    pub fn rename(&mut self, name: &str) {
        let old = std::mem::replace(&mut self.name, name.to_string());
        self.former_names.retain(|former| former != &old);
        self.former_names.push(old);
        let len = self.former_names.len();
        if len > Self::MAX_FORMER_NAMES {
            self.former_names.drain(..len - Self::MAX_FORMER_NAMES);
        }
    }
}

fn default_column_config() -> String {
    "{}".to_string()
}

/// A swimlane's cap on how many tasks one column may hold in that lane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LaneLimit {
    /// The column the limit applies to.
    pub column_id: String,
    /// Most tasks the lane's cell in that column may hold before the board flags it.
    pub max: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwimlaneRow {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i64,
    /// Shown as a tooltip from the lane's header. Defaults to "" for rows written before this
    /// field existed.
    #[serde(default)]
    pub description: String,
    /// One limit per column at most. Empty for rows written before this field existed.
    #[serde(default)]
    pub limits: Vec<LaneLimit>,
    /// A folded lane renders as one thin row with its counts. Defaults to false.
    #[serde(default)]
    pub collapsed: bool,
    /// The lane's height in pixels once dragged, or null for the default height.
    #[serde(default)]
    pub height: Option<i64>,
}

impl SwimlaneRow {
    /// Returns the limit this lane puts on `column_id`, if any.
    pub fn limit_for(&self, column_id: &str) -> Option<i64> {
        self.limits
            .iter()
            .find(|limit| limit.column_id == column_id)
            .map(|limit| limit.max)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct TaskRow {
    pub id: String,
    pub column_id: String,
    pub swimlane_id: Option<String>,
    pub name: String,
    pub description: String,
    pub size: i64,
    pub position: f64,
    pub created_at: String,
    pub completed_at: Option<String>,
    /// Aggregated by the DB layer on read; 0 for freshly created tasks.
    pub total_minutes: i64,
    /// Completed pomodoro sessions; defaults to 0 for rows written before
    /// this field existed.
    #[serde(default)]
    pub pomodori_completed: u32,
    /// Pomodoro sessions stopped early; defaults to 0 for older rows.
    #[serde(default)]
    pub interruptions: u32,
    /// Per-board task color id, or null for rows written before colors
    /// existed — those render via the legacy size mapping.
    #[serde(default)]
    pub color_id: Option<String>,
    /// Checklist subtasks (KanbanFlow parity). Empty for rows written
    /// before subtasks existed.
    #[serde(default)]
    pub subtasks: Vec<Subtask>,
    /// Assigned member user ids (KanbanFlow parity). Empty for rows
    /// written before members existed.
    #[serde(default)]
    pub member_ids: Vec<String>,
    /// Override date used when the column groups tasks by date
    /// ("Edit grouping date"). Null means use the created/completed date.
    #[serde(default)]
    pub grouping_date: Option<String>,
    /// Watch flag (KanbanFlow parity: task More menu "Watch"). False for
    /// rows written before watching existed.
    #[serde(default)]
    pub watched: bool,
    /// Labels attached via the Labels sub-dialog (KanbanFlow parity).
    /// Empty for rows written before labels existed.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Due date/time as RFC3339 (KanbanFlow parity: "Add due date").
    /// Null when the task has no due date.
    #[serde(default)]
    pub due_at: Option<String>,
    /// Hour-based time estimate: the canonical estimate, e.g. 4.0 for "4h". Null when no estimate
    /// is set. `size` (pomodori) is kept as the derived/display concept for the pomodoro legend and
    /// legacy coloring.
    #[serde(default)]
    pub estimate_hours: Option<f64>,
    /// Repeat cadence for the due date, e.g. "every week". Null means no
    /// repeat. Kept as free text mirroring the dialog's Repeat field.
    #[serde(default)]
    pub due_repeat: Option<String>,
    /// Whether the due date was marked done (KanbanFlow parity: the
    /// due-date item can be checked; the card then shows "(Done)").
    /// False for rows written before the field existed.
    #[serde(default)]
    pub due_done: bool,
    /// The column the task should reach by its due date (KanbanFlow's "Target column"). Null means
    /// "Any archived column", and it is null for rows written before the field existed.
    #[serde(default)]
    pub due_target_column_id: Option<String>,
    /// RFC3339 timestamp of when the task entered its current column
    /// (KanbanFlow parity: card "Added to column" date). Set on create and
    /// on every move to another column, and filled in for older rows from
    /// their history or creation time.
    #[serde(default)]
    pub column_entered_at: Option<String>,
    /// Task comments (KanbanFlow parity). Empty for older rows.
    #[serde(default)]
    pub comments: Vec<TaskComment>,
    /// Task attachments (KanbanFlow parity). Metadata lives here; the
    /// bytes live in the `attachment_data` table. Empty for older rows.
    #[serde(default)]
    pub attachments: Vec<TaskAttachment>,
    /// Activity trail (KanbanFlow parity: History report). Newest appended
    /// last; capped by the DB layer. Empty for older rows.
    #[serde(default)]
    pub history: Vec<TaskEvent>,
    /// RFC3339 timestamp of the last write to this row. Null for rows
    /// written before the field existed, which report `created_at`.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Caller-owned JSON object, merged by top-level key through the API.
    /// Null when unset, including rows written before the field existed.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// Links this task holds: its `blocked_by` and `related` links. Empty
    /// for rows written before links existed.
    #[serde(default)]
    pub links: Vec<TaskLink>,
    /// The latest claim, which may have expired. Null when never claimed
    /// or released.
    #[serde(default)]
    pub claim: Option<TaskClaim>,
    /// Fence of the latest successful claim; 0 when never claimed.
    #[serde(default)]
    pub claim_fence: u64,
    /// Custom field values by field key. Empty for rows written before
    /// fields existed.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

/// The parts of a stored task its board card and its calendar entry show. The description is
/// read as a preview, the comments and attachments as counts, and the history and metadata are
/// skipped, so a board renders without holding them.
#[derive(Clone, Debug, Deserialize)]
pub struct TaskCard {
    /// The task's id.
    pub id: String,
    /// The column the task sits in.
    pub column_id: String,
    /// The swimlane the task sits in, or null when it has none.
    pub swimlane_id: Option<String>,
    /// The task's name.
    pub name: String,
    /// The start of the description, at most [`TaskCard::DESCRIPTION_PREVIEW_CHARS`] characters
    /// and an ellipsis when it is longer.
    #[serde(deserialize_with = "description_preview")]
    pub description: String,
    /// Legacy size in pomodori, which colors a task without a color id.
    pub size: i64,
    /// Sort position within the task's cell.
    pub position: f64,
    /// RFC3339 time the task was created.
    pub created_at: String,
    /// RFC3339 time the task reached a done column, or null.
    pub completed_at: Option<String>,
    /// Minutes logged on the task, filled in from its time entries after it is read.
    #[serde(skip)]
    pub total_minutes: i64,
    /// Per-board task color id, or null for rows written before colors existed.
    #[serde(default)]
    pub color_id: Option<String>,
    /// Checklist subtasks (KanbanFlow parity).
    #[serde(default)]
    pub subtasks: Vec<Subtask>,
    /// Assigned member user ids (KanbanFlow parity).
    #[serde(default)]
    pub member_ids: Vec<String>,
    /// Override date used when the column groups tasks by date.
    #[serde(default)]
    pub grouping_date: Option<String>,
    /// Watch flag (KanbanFlow parity: task More menu "Watch").
    #[serde(default)]
    pub watched: bool,
    /// Labels attached via the Labels sub-dialog (KanbanFlow parity).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Due date/time as RFC3339, or null when the task has no due date.
    #[serde(default)]
    pub due_at: Option<String>,
    /// Hour-based time estimate, or null when no estimate is set.
    #[serde(default)]
    pub estimate_hours: Option<f64>,
    /// Repeat cadence for the due date, e.g. "every week", or null.
    #[serde(default)]
    pub due_repeat: Option<String>,
    /// Whether the due date was marked done.
    #[serde(default)]
    pub due_done: bool,
    /// The column the task should reach by its due date, or null.
    #[serde(default)]
    pub due_target_column_id: Option<String>,
    /// RFC3339 time the task entered its current column, or null.
    #[serde(default)]
    pub column_entered_at: Option<String>,
    /// How many comments the task holds.
    #[serde(default, deserialize_with = "count_items")]
    pub comments: usize,
    /// How many attachments the task holds.
    #[serde(default, deserialize_with = "count_items")]
    pub attachments: usize,
    /// Links this task holds: its `blocked_by` and `related` links.
    #[serde(default)]
    pub links: Vec<TaskLink>,
    /// The latest claim, which may have expired.
    #[serde(default)]
    pub claim: Option<TaskClaim>,
    /// Custom field values by field key.
    #[serde(default)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl TaskCard {
    /// Most characters of a description a card keeps.
    pub const DESCRIPTION_PREVIEW_CHARS: usize = 200;

    /// Returns the claim when its `expires_at` is not yet in the past.
    #[must_use]
    pub fn live_claim(&self) -> Option<&TaskClaim> {
        self.claim
            .as_ref()
            .filter(|claim| claim.is_live(Utc::now()))
    }
}

impl From<&TaskRow> for TaskCard {
    fn from(row: &TaskRow) -> Self {
        Self {
            id: row.id.clone(),
            column_id: row.column_id.clone(),
            swimlane_id: row.swimlane_id.clone(),
            name: row.name.clone(),
            description: preview(&row.description),
            size: row.size,
            position: row.position,
            created_at: row.created_at.clone(),
            completed_at: row.completed_at.clone(),
            total_minutes: row.total_minutes,
            color_id: row.color_id.clone(),
            subtasks: row.subtasks.clone(),
            member_ids: row.member_ids.clone(),
            grouping_date: row.grouping_date.clone(),
            watched: row.watched,
            labels: row.labels.clone(),
            due_at: row.due_at.clone(),
            estimate_hours: row.estimate_hours,
            due_repeat: row.due_repeat.clone(),
            due_done: row.due_done,
            due_target_column_id: row.due_target_column_id.clone(),
            column_entered_at: row.column_entered_at.clone(),
            comments: row.comments.len(),
            attachments: row.attachments.len(),
            links: row.links.clone(),
            claim: row.claim.clone(),
            fields: row.fields.clone(),
        }
    }
}

/// What a board's calendar feed reads of a task.
#[derive(Clone, Debug, Deserialize)]
pub struct FeedTask {
    /// The task's id.
    pub id: String,
    /// The task's name.
    pub name: String,
    /// The column the task sits in.
    pub column_id: String,
    /// Due date/time as RFC3339, or null when the task has no due date.
    #[serde(default)]
    pub due_at: Option<String>,
    /// Repeat cadence for the due date, e.g. "every week", or null.
    #[serde(default)]
    pub due_repeat: Option<String>,
}

/// What links and their checks read of the task a link names.
#[derive(Clone, Debug, Deserialize)]
pub struct TaskHead {
    /// The task's id.
    pub id: String,
    /// The task's name.
    pub name: String,
    /// The column the task sits in.
    pub column_id: String,
    /// Links this task holds: its `blocked_by` and `related` links.
    #[serde(default)]
    pub links: Vec<TaskLink>,
}

/// What the timer and its reports show of the task a time entry is logged on.
#[derive(Clone, Debug, Deserialize)]
pub struct TaskBrief {
    /// The task's name.
    pub name: String,
    /// The column the task sits in.
    pub column_id: String,
    /// Per-board task color id, or null for rows written before colors existed.
    #[serde(default)]
    pub color_id: Option<String>,
    /// The board the task sits on, filled in after it is read.
    #[serde(skip)]
    pub board_id: String,
}

/// Returns `text` cut to [`TaskCard::DESCRIPTION_PREVIEW_CHARS`] characters, with an ellipsis
/// when it was longer.
fn preview(text: &str) -> String {
    let mut chars = text.char_indices();
    match chars.nth(TaskCard::DESCRIPTION_PREVIEW_CHARS) {
        None => text.to_string(),
        Some((end, _)) => {
            let kept = &text[..end];
            format!("{kept}…")
        }
    }
}

/// Reads a description as its [`preview`], without keeping the rest.
fn description_preview<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    struct Preview;

    impl Visitor<'_> for Preview {
        type Value = String;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a description")
        }

        fn visit_str<E: de::Error>(self, text: &str) -> Result<String, E> {
            Ok(preview(text))
        }
    }

    deserializer.deserialize_str(Preview)
}

/// Reads a list as the number of entries it holds, without keeping them.
fn count_items<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    struct Count;

    impl<'de> Visitor<'de> for Count {
        type Value = usize;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a list")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<usize, A::Error> {
            let mut count = 0;
            while items.next_element::<IgnoredAny>()?.is_some() {
                count += 1;
            }
            Ok(count)
        }
    }

    deserializer.deserialize_seq(Count)
}

impl TaskRow {
    /// Returns the claim when its `expires_at` is not yet in the past.
    pub fn live_claim(&self) -> Option<&TaskClaim> {
        self.claim
            .as_ref()
            .filter(|claim| claim.is_live(Utc::now()))
    }

    /// False when `fence` is given and a live claim holds another fence.
    pub fn fence_allows(&self, fence: Option<u64>) -> bool {
        match (fence, self.live_claim()) {
            (Some(fence), Some(claim)) => claim.fence == fence,
            _ => true,
        }
    }

    /// Returns [`TooLong`] for the first text this row holds past its [`TextCap`] that differs
    /// from `before`, the row as stored. Text a row already held stays, so an older row past a
    /// cap that was added later can still be written.
    pub fn check_caps(&self, before: Option<&Self>) -> Result<(), TooLong> {
        let changed = |now: &str, then: Option<&str>| then != Some(now);
        if changed(&self.name, before.map(|row| row.name.as_str())) {
            TextCap::TASK_NAME.check(&self.name)?;
        }
        if changed(
            &self.description,
            before.map(|row| row.description.as_str()),
        ) {
            TextCap::DESCRIPTION.check(&self.description)?;
        }
        if let Some(repeat) = &self.due_repeat
            && changed(repeat, before.and_then(|row| row.due_repeat.as_deref()))
        {
            TextCap::DUE_REPEAT.check(repeat)?;
        }
        check_labels(&self.labels, before.map(|row| row.labels.as_slice()))?;
        for subtask in &self.subtasks {
            let held = before
                .and_then(|row| row.subtasks.iter().find(|held| held.id == subtask.id))
                .map(|held| held.name.as_str());
            if changed(&subtask.name, held) {
                TextCap::SUBTASK_NAME.check(&subtask.name)?;
            }
        }
        if self.member_ids.len() > before.map_or(0, |row| row.member_ids.len()) {
            TextCap::MEMBERS.check_count(&self.member_ids)?;
        }
        match (&self.claim, before.and_then(|row| row.claim.as_ref())) {
            (Some(claim), Some(held)) if claim.texts() == held.texts() => Ok(()),
            (Some(claim), _) => claim.check_caps(),
            (None, _) => Ok(()),
        }
    }
}

/// Value type of a custom field.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Multiline,
    Number,
    Bool,
    Select,
    Multiselect,
    Table,
}

impl FieldKind {
    /// Parses a wire name such as `multiselect`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "text" => Some(Self::Text),
            "multiline" => Some(Self::Multiline),
            "number" => Some(Self::Number),
            "bool" => Some(Self::Bool),
            "select" => Some(Self::Select),
            "multiselect" => Some(Self::Multiselect),
            "table" => Some(Self::Table),
            _ => None,
        }
    }

    /// Wire name, e.g. `multiselect`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Multiline => "multiline",
            Self::Number => "number",
            Self::Bool => "bool",
            Self::Select => "select",
            Self::Multiselect => "multiselect",
            Self::Table => "table",
        }
    }

    /// True for the kinds whose values come from `options`.
    pub fn has_options(self) -> bool {
        matches!(self, Self::Select | Self::Multiselect)
    }

    /// True for the kinds a table column may hold.
    pub fn is_column_kind(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Number | Self::Bool | Self::Select | Self::Multiselect
        )
    }
}

/// Most rows a table field value holds.
pub const MAX_TABLE_ROWS: usize = 200;

/// What a JSON list of columns or rows becomes when it cannot serialize.
const EMPTY_JSON_LIST: &str = "[]";

/// One typed column of a table field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FieldColumn {
    /// Dotted lowercase key with the same rules as a field key; unique within the table.
    pub key: String,
    pub label: String,
    /// One of `text`, `number`, `bool`, `select` or `multiselect`.
    pub kind: FieldKind,
    /// Choices for `select` and `multiselect`; empty otherwise.
    pub options: Vec<String>,
    /// Whether every row must hold a value for this column.
    pub required: bool,
}

/// Who edits a custom field: people in the task dialog, or programs only.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldOwner {
    User,
    System,
}

impl FieldOwner {
    /// Wire name: `user` or `system`.
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

/// Characters that break a line: `\n`, `\r`, NEL (U+0085), and the
/// Unicode line and paragraph separators (U+2028, U+2029).
pub const LINE_BREAKS: [char; 5] = ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'];

/// True when `text` contains any of [`LINE_BREAKS`].
pub fn has_line_break(text: &str) -> bool {
    text.contains(LINE_BREAKS)
}

/// Longest custom field key, in characters.
pub const MAX_FIELD_KEY_LEN: usize = 64;

/// True when `key` is dotted lowercase segments, each starting with a
/// letter (`^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`), at most 64 chars.
pub fn valid_field_key(key: &str) -> bool {
    key.len() <= MAX_FIELD_KEY_LEN
        && key.split('.').all(|segment| {
            let mut chars = segment.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// A custom field defined on one board.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldDefRow {
    pub id: String,
    pub board_id: String,
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    pub options: Vec<String>,
    /// Typed columns of a `table` field; empty for every other kind.
    #[serde(default)]
    pub columns: Vec<FieldColumn>,
    pub default: Option<serde_json::Value>,
    pub required: bool,
    pub owner: FieldOwner,
    pub position: i64,
    /// RFC3339.
    pub created_at: String,
}

impl FieldColumn {
    /// Returns the columns as JSON for the editors in the dialog and the settings page.
    pub fn list_json(columns: &[Self]) -> String {
        serde_json::to_string(columns).unwrap_or_else(|_| EMPTY_JSON_LIST.to_string())
    }

    /// Returns the rows of a table value as JSON, or `[]` for any other value.
    pub fn rows_json(value: Option<&Value>) -> String {
        match value {
            Some(rows @ Value::Array(_)) => rows.to_string(),
            _ => EMPTY_JSON_LIST.to_string(),
        }
    }
}

impl FieldDefRow {
    /// Checks a non-null value against the field's kind, options and columns
    /// and returns it as stored (multiselect choices deduplicated). The error
    /// says what was expected, without the key.
    pub fn check_value(&self, value: &Value) -> Result<Value, String> {
        check_kind_value(self.kind, &self.options, &self.columns, value)
    }

    /// Returns [`TooLong`] for the label, option or table column past its [`TextCap`].
    pub fn check_caps(&self) -> Result<(), TooLong> {
        TextCap::FIELD_NAME.check(&self.label)?;
        TextCap::FIELD_OPTIONS.check_count(&self.options)?;
        for option in &self.options {
            TextCap::FIELD_OPTION.check(option)?;
        }
        TextCap::FIELD_COLUMNS.check_count(&self.columns)?;
        for column in &self.columns {
            TextCap::FIELD_COLUMN_NAME.check(&column.label)?;
            TextCap::FIELD_OPTIONS.check_count(&column.options)?;
            for option in &column.options {
                TextCap::FIELD_OPTION.check(option)?;
            }
        }
        Ok(())
    }

    /// True when a required field counts as done: filled in, and valid for
    /// the field as defined now (a choice its options no longer list is
    /// not).
    pub fn is_complete(&self, value: Option<&serde_json::Value>) -> bool {
        Self::is_filled(value) && value.is_some_and(|value| self.check_value(value).is_ok())
    }

    /// True when a value counts as filled in for the `required` hint.
    pub fn is_filled(value: Option<&serde_json::Value>) -> bool {
        match value {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::String(text)) => !text.trim().is_empty(),
            Some(serde_json::Value::Array(items)) => !items.is_empty(),
            Some(_) => true,
        }
    }
}

/// Checks a non-null value against a kind with its options and columns, and returns it as stored.
fn check_kind_value(
    kind: FieldKind,
    options: &[String],
    columns: &[FieldColumn],
    value: &Value,
) -> Result<Value, String> {
    let option = |choice: &str| {
        if options.iter().any(|o| o == choice) {
            Ok(())
        } else {
            Err(format!("\"{choice}\" is not an option"))
        }
    };
    match (kind, value) {
        (FieldKind::Text, Value::String(text)) if has_line_break(text) => {
            Err("text must be a single line; use a multiline field".into())
        }
        (FieldKind::Text | FieldKind::Multiline, Value::String(text)) => TextCap::FIELD_VALUE
            .check(text)
            .map(|()| value.clone())
            .map_err(|refusal| refusal.to_string()),
        (FieldKind::Text | FieldKind::Multiline, _) => Err("expected a string".into()),
        (FieldKind::Number, Value::Number(_)) => Ok(value.clone()),
        (FieldKind::Number, _) => Err("expected a number".into()),
        (FieldKind::Bool, Value::Bool(_)) => Ok(value.clone()),
        (FieldKind::Bool, _) => Err("expected true or false".into()),
        (FieldKind::Select, Value::String(choice)) => option(choice).map(|_| value.clone()),
        (FieldKind::Select, _) => Err("expected one option as a string".into()),
        (FieldKind::Multiselect, Value::Array(items)) => {
            let mut chosen: Vec<Value> = Vec::new();
            for item in items {
                let Value::String(choice) = item else {
                    return Err("expected an array of options".into());
                };
                option(choice)?;
                if !chosen.contains(item) {
                    chosen.push(item.clone());
                }
            }
            Ok(Value::Array(chosen))
        }
        (FieldKind::Multiselect, _) => Err("expected an array of options".into()),
        (FieldKind::Table, Value::Array(rows)) => check_table_rows(columns, rows),
        (FieldKind::Table, _) => Err("expected an array of rows".into()),
    }
}

/// Checks the rows of a table value, keeping their order, and returns them as stored.
fn check_table_rows(columns: &[FieldColumn], rows: &[Value]) -> Result<Value, String> {
    if rows.len() > MAX_TABLE_ROWS {
        return Err(format!(
            "at most {MAX_TABLE_ROWS} rows are allowed, got {}",
            rows.len()
        ));
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| check_table_row(columns, index, row))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

/// Checks one row against the columns. The error names the row and column, e.g. `rows[1].on`.
fn check_table_row(columns: &[FieldColumn], index: usize, row: &Value) -> Result<Value, String> {
    let Value::Object(cells) = row else {
        return Err(format!("rows[{index}]: expected an object"));
    };
    if let Some(key) = cells
        .keys()
        .find(|key| !columns.iter().any(|column| column.key == **key))
    {
        return Err(format!("rows[{index}].{key}: unknown column"));
    }
    let mut stored = serde_json::Map::new();
    for column in columns {
        match cells.get(&column.key) {
            None | Some(Value::Null) if column.required => {
                return Err(format!("rows[{index}].{}: is required", column.key));
            }
            None | Some(Value::Null) => {}
            Some(cell) => {
                let cell = check_kind_value(column.kind, &column.options, &[], cell)
                    .map_err(|why| format!("rows[{index}].{}: {why}", column.key))?;
                stored.insert(column.key.clone(), cell);
            }
        }
    }
    Ok(Value::Object(stored))
}

/// A lease on a task: who works on it, until when, and under which fence.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct TaskClaim {
    pub claimant: String,
    pub role: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub jj_workspace: Option<String>,
    pub jj_change_id: Option<String>,
    pub session_id: Option<String>,
    /// Per-task counter that strictly increases on every successful claim.
    pub fence: u64,
    /// RFC3339.
    pub started_at: String,
    /// RFC3339.
    pub heartbeat_at: String,
    /// RFC3339.
    pub expires_at: String,
}

impl TaskClaim {
    /// Returns every text the claim holds, each beside its cap.
    #[must_use]
    pub fn texts(&self) -> [(TextCap, Option<&str>); 7] {
        [
            (TextCap::CLAIMANT, Some(self.claimant.as_str())),
            (TextCap::CLAIM_ROLE, self.role.as_deref()),
            (TextCap::CLAIM_HARNESS, self.harness.as_deref()),
            (TextCap::CLAIM_MODEL, self.model.as_deref()),
            (TextCap::CLAIM_JJ_WORKSPACE, self.jj_workspace.as_deref()),
            (TextCap::CLAIM_JJ_CHANGE_ID, self.jj_change_id.as_deref()),
            (TextCap::CLAIM_SESSION_ID, self.session_id.as_deref()),
        ]
    }

    /// Returns [`TooLong`] for the first text past its cap.
    pub fn check_caps(&self) -> Result<(), TooLong> {
        for (cap, text) in self.texts() {
            if let Some(text) = text {
                cap.check(text)?;
            }
        }
        Ok(())
    }

    /// True while `expires_at` is not in the past at `now`. An unparseable
    /// `expires_at` counts as expired.
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        DateTime::parse_from_rfc3339(&self.expires_at).is_ok_and(|at| at >= now)
    }
}

/// Relationship between two tasks. `blocks` is never stored: it is the
/// blocker's view of a `blocked_by` link held by the blocked task.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    BlockedBy,
    Blocks,
    Related,
}

impl LinkKind {
    /// Parses a wire name: `blocked_by`, `blocks`, or `related`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "blocked_by" => Some(Self::BlockedBy),
            "blocks" => Some(Self::Blocks),
            "related" => Some(Self::Related),
            _ => None,
        }
    }
}

/// One link between two tasks, as seen from the task it is listed on.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct TaskLink {
    pub id: String,
    pub kind: LinkKind,
    /// The other task.
    pub target_id: String,
    /// RFC3339.
    pub created_at: String,
    /// The other task's board, filled in when links are listed; never
    /// stored, since the other task can move between boards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<String>,
}

impl TaskRow {
    /// Returns `updated_at`, or `created_at` for rows never written since
    /// the field existed.
    pub fn last_updated(&self) -> &str {
        self.updated_at.as_deref().unwrap_or(&self.created_at)
    }

    /// Returns `column_entered_at`, or `created_at` for a row without the stamp.
    pub fn column_entered_at(&self) -> &str {
        self.column_entered_at
            .as_deref()
            .unwrap_or(&self.created_at)
    }
}

/// How a comment's author authenticated.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// The browser session; also every comment stored before the field
    /// existed.
    #[default]
    User,
    ApiToken,
}

/// One comment on a task (KanbanFlow parity).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct TaskComment {
    pub id: String,
    pub author: String,
    pub body: String,
    /// RFC3339.
    pub created_at: String,
    /// Filled in by the server.
    #[serde(default)]
    pub actor_kind: ActorKind,
    /// Name of the API token that wrote it, or null.
    #[serde(default)]
    pub token_name: Option<String>,
    /// Id of the API token that wrote it, or null. Only that token may edit or delete the
    /// comment without the `admin` scope.
    #[serde(default)]
    pub token_id: Option<String>,
    /// Free-form tag such as "agent", "oracle", "review", or "system";
    /// null when untagged.
    #[serde(default)]
    pub kind: Option<String>,
    /// RFC3339 time of the last edit of the body, or null when never edited.
    #[serde(default)]
    pub edited_at: Option<String>,
}

/// One file attachment on a task (KanbanFlow parity). The bytes are stored
/// in the separate `attachment_data` table keyed by `id`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct TaskAttachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub uploaded_by: String,
    /// RFC3339.
    pub created_at: String,
}

impl TaskAttachment {
    /// Returns the extension of the file name `name` with its dot (`.txt`), or an empty string if
    /// it has none.
    #[must_use]
    pub fn extension(name: &str) -> &str {
        match name.rfind('.') {
            Some(dot) if dot > 0 && !name[dot..].contains(char::is_whitespace) => &name[dot..],
            _ => "",
        }
    }

    /// Returns `name` with the extension of `current` appended, unless `name` already ends with
    /// it.
    #[must_use]
    pub fn keep_extension(current: &str, name: &str) -> String {
        let extension = Self::extension(current);
        if name.ends_with(extension) {
            name.to_string()
        } else {
            format!("{name}{extension}")
        }
    }
}

/// Who makes a change: the actor recorded on history events and, for an API token, its name.
#[derive(Copy, Clone, Debug)]
pub struct Actor<'a> {
    /// The username, `api-token` for an API token or `system` for the server itself.
    pub name: &'a str,
    /// Name of the API token behind the change, absent for the browser session, the bootstrap
    /// token and the server.
    pub token_name: Option<&'a str>,
}

impl Actor<'static> {
    /// The server acting on its own.
    pub const SYSTEM: Self = Self {
        name: "system",
        token_name: None,
    };
}

/// One activity-trail event on a task (KanbanFlow parity: History).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct TaskEvent {
    pub id: String,
    /// Machine-readable kind, e.g. "created", "moved", "time_logged".
    pub kind: String,
    /// Human-readable one-liner, e.g. "Moved to Done".
    pub detail: String,
    /// Who caused it (username or "API").
    pub actor: String,
    /// RFC3339.
    pub created_at: String,
    /// The column a "created" or "moved" event put the task in, so reports follow renames. Null for
    /// other kinds and for older events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_id: Option<String>,
    /// Name of the API token that made the change. Null for the browser session, for the
    /// bootstrap token, for the server itself and for older events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_name: Option<String>,
}

/// One checklist item on a task (KanbanFlow parity: Subtasks section).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Subtask {
    pub id: String,
    pub name: String,
    pub done: bool,
    /// Due date/time as RFC3339. Absent when the subtask has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeEntryRow {
    pub id: String,
    pub task_id: String,
    pub minutes: i64,
    /// Precise elapsed seconds. For rows written before this field existed, defaults to 0 and
    /// `minutes` is used for display.
    #[serde(default)]
    pub seconds: i64,
    pub note: String,
    pub started_at: String,
    /// "pomodoro" | "stopwatch" | "short_break" | "long_break" | "manual".
    /// Defaults to "manual" for rows written before this field existed.
    #[serde(default = "default_entry_kind")]
    pub kind: String,
    /// True when a pomodoro/stopwatch session was stopped before finishing.
    #[serde(default)]
    pub interrupted: bool,
    /// Why the session was stopped early, if a reason was given.
    #[serde(default)]
    pub interrupt_reason: Option<String>,
    /// Labels attached via the manual-time / edit-entry dialogs
    /// (KanbanFlow parity). Empty for rows written before labels existed.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Username that created the entry (for the time-log avatar/name).
    /// Empty for rows written before it was recorded.
    #[serde(default)]
    pub created_by: String,
    /// Break-activity id the session was attributed to. Only meaningful on short_break/long_break
    /// entries, and None when the user picked no activity or the session predates activity logging.
    #[serde(default)]
    pub activity_id: Option<String>,
    /// The account that logged the entry, which also owns an entry without a task.
    #[serde(default)]
    pub account_id: String,
}

impl TimeEntryRow {
    /// Returns the entry's length in seconds, falling back to `minutes` for
    /// rows written before `seconds` existed.
    pub fn duration_seconds(&self) -> i64 {
        if self.seconds > 0 {
            self.seconds
        } else {
            self.minutes * 60
        }
    }

    /// Returns [`TooLong`] for the first text this entry holds past its [`TextCap`] that differs
    /// from `before`, the entry as stored.
    pub fn check_caps(&self, before: Option<&Self>) -> Result<(), TooLong> {
        if before.is_none_or(|row| row.note != self.note) {
            TextCap::ENTRY_NOTE.check(&self.note)?;
        }
        if let Some(reason) = &self.interrupt_reason
            && before.is_none_or(|row| row.interrupt_reason.as_ref() != Some(reason))
        {
            TextCap::STOP_REASON.check(reason)?;
        }
        check_labels(&self.labels, before.map(|row| row.labels.as_slice()))
    }
}

fn default_entry_kind() -> String {
    EntryKind::Manual.as_str().to_string()
}

// ---- Per-board task colors (KanbanFlow parity) ----

/// One color slot on a board's palette. The hex values are fixed per
/// standard color (see [`STANDARD_COLORS`]); the board config only
/// enables/disables, renames, describes, reorders, and picks the default.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColorRow {
    pub id: String,
    pub board_id: String,
    /// Standard color value: "yellow" | "green" | "blue" | "red" |
    /// "orange" | "purple" | "magenta" | "cyan" | "brown" | "white".
    pub value: String,
    /// User-facing label (max 50 chars), e.g. "1 Pomodoro".
    pub label: String,
    /// Legend tooltip text; empty when none is set.
    pub description: String,
    pub background_hex: String,
    pub border_hex: String,
    pub light_hex: String,
    /// Whether the task color picker offers this color.
    pub enabled: bool,
    /// Whether new tasks get this color by default.
    pub is_default: bool,
    /// Picker/legend ordering, dense within the board.
    pub sort_order: i64,
}

impl ColorRow {
    /// Returns the row with its hexes taken from the fixed palette.
    ///
    /// Rows stored before a palette correction read back with the current values, and a value
    /// outside the palette keeps its own.
    #[must_use]
    pub fn with_palette_hexes(mut self) -> Self {
        if let Some((background, border, light, _)) = standard_color(&self.value) {
            self.background_hex = background.to_string();
            self.border_hex = border.to_string();
            self.light_hex = light.to_string();
        }
        self
    }
}

/// One standard KanbanFlow color: (value, background, border, light, default label). Hex values are
/// fixed and never user-editable, and borders follow KanbanFlow's taskBorderColor variables.
pub const STANDARD_COLORS: &[(&str, &str, &str, &str, &str)] = &[
    ("yellow", "#ffffe0", "#f5cc00", "#ffffe0", "Yellow"),
    ("green", "#dbffc2", "#59d600", "#e4ffd1", "Green"),
    ("blue", "#cce3ff", "#70b0ff", "#d6e9ff", "Blue"),
    ("red", "#ffccd0", "#ff858f", "#ffe0e3", "Red"),
    ("orange", "#ffeac2", "#faa200", "#ffeac2", "Orange"),
    ("purple", "#eddbff", "#c994ff", "#f2e6ff", "Purple"),
    ("magenta", "#ffe0ff", "#ff85ff", "#ffebff", "Magenta"),
    ("cyan", "#dbffff", "#00d6d6", "#e6ffff", "Cyan"),
    ("brown", "#f6ddcb", "#e49b67", "#f9e8dc", "Brown"),
    ("white", "#fbfbfb", "#d4d4d4", "#fbfbfb", "White"),
];

/// Look up a standard color's fixed hex values by value name.
pub fn standard_color(value: &str) -> Option<(&str, &str, &str, &str)> {
    STANDARD_COLORS
        .iter()
        .find_map(|(v, bg, border, light, _label)| {
            if *v == value {
                Some((*bg, *border, *light, *v))
            } else {
                None
            }
        })
}

/// Legacy size -> color value mapping (kept for backward compatibility).
pub fn size_to_color_value(size: i64) -> &'static str {
    match Size::from_i64(size) {
        Size::One => "yellow",
        Size::Two => "green",
        Size::Three => "blue",
        Size::Many => "red",
    }
}

/// Legacy color value -> size mapping (so a color assignment keeps the
/// integer `size` meaningful to old clients).
pub fn color_value_to_size(value: &str) -> i64 {
    match value {
        "yellow" => 1,
        "green" => 2,
        "blue" => 3,
        "red" => 4,
        _ => 1,
    }
}

// ---- Board templates ----

/// A named, reusable board recipe. Its snapshot of the board's colors (all, with their config),
/// columns, swimlanes and tasks is stored apart, so listing templates never reads one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardTemplateRow {
    pub id: String,
    pub name: String,
    pub description: String,
    /// True for the shipped built-in templates (cannot be deleted).
    pub built_in: bool,
    /// The account that saved the template, or [`None`] for a built-in one every account sees.
    #[serde(default)]
    pub owner_id: Option<String>,
    /// Bytes the stored snapshot takes.
    #[serde(default)]
    pub snapshot_bytes: u64,
}

/// The id of an account, or the ownerless id that holds a fresh instance's rows until its first
/// account claims them. It is stored as the bare id, and the ownerless id as an empty string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AccountId(String);

impl AccountId {
    /// Wraps a stored account id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the ownerless id.
    #[must_use]
    pub fn ownerless() -> Self {
        Self(String::new())
    }

    /// Returns whether this is the ownerless id.
    #[must_use]
    pub fn is_ownerless(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the id as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What an account may do beyond its own boards.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccountRole {
    /// Runs the instance: backups and instance settings.
    Admin,
    /// Uses only its own boards.
    Member,
}

impl AccountRole {
    /// Returns the role as stored and as the API names it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }
}

/// Whether an account may sign in.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccountStatus {
    /// Signs in and works normally.
    Active,
    /// Requested an account and waits for the admin's approval. It signs in to a page that says
    /// so, and reaches nothing else.
    Pending,
    /// Stopped by the admin: it cannot sign in, and its data stays.
    Suspended,
}

impl AccountStatus {
    /// Returns the status as stored and as the API names it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Suspended => "suspended",
        }
    }
}

/// How a person without an invite link gets an account, chosen by the admin.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationMode {
    /// Only an invite link registers an account.
    #[default]
    Invite,
    /// Anyone may request an account, which waits for the admin's approval. Invite links still
    /// register active accounts.
    Request,
}

/// An account: the user that owns boards, tokens and settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRow {
    /// A UUID, which doubles as the WebAuthn user handle.
    pub id: String,
    /// The account's handle.
    pub username: String,
    /// What the account may do beyond its own boards.
    pub role: AccountRole,
    /// Whether the account may sign in.
    pub status: AccountStatus,
    /// Unix seconds of the account's creation.
    pub created_at: i64,
}

impl UserRow {
    /// Returns the account's id.
    #[must_use]
    pub fn account_id(&self) -> AccountId {
        AccountId::new(self.id.as_str())
    }
}

/// How many accounts an instance serves, chosen with `--mode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum InstanceMode {
    /// One account, which is the admin.
    Single,
    /// The admin plus the members it invites, each with only its own boards.
    Multi,
}

impl fmt::Display for InstanceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Single => "single",
            Self::Multi => "multi",
        })
    }
}

/// Instance-wide settings, stored as the `instance` row of the settings table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSettings {
    /// How many accounts the instance serves.
    pub mode: InstanceMode,
    /// How a person without an invite link gets an account.
    #[serde(default)]
    pub registration: RegistrationMode,
    /// Most accounts that may wait for approval at once. A request to join past it is refused.
    #[serde(default = "InstanceSettings::default_pending_cap")]
    pub pending_cap: u32,
    /// How much each member account may hold, in multi mode.
    #[serde(default)]
    pub quotas: Quotas,
    /// How fast credentials, accounts and client addresses may send requests.
    #[serde(default)]
    pub limits: RateLimits,
    /// When the instance stops taking writes that grow the database.
    #[serde(default)]
    pub disk: DiskLimits,
    /// When member boards nobody uses are deleted.
    #[serde(default)]
    pub prune: PruneSettings,
    /// Unix seconds of the latest change to `prune`, absent while it holds its first settings.
    /// No board is pruned before a full warning period has passed since.
    #[serde(default)]
    pub prune_changed_at: Option<i64>,
}

impl InstanceSettings {
    /// Most pending accounts an instance takes unless the admin sets another cap.
    pub const DEFAULT_PENDING_CAP: u32 = 100;
    /// Largest pending-account cap the admin may set.
    pub const MAX_PENDING_CAP: u32 = 10_000;
    /// Smallest pending-account cap the admin may set.
    pub const MIN_PENDING_CAP: u32 = 1;

    /// Returns [`InstanceSettings::DEFAULT_PENDING_CAP`], which a stored row without a cap reads.
    fn default_pending_cap() -> u32 {
        Self::DEFAULT_PENDING_CAP
    }

    /// Replaces the prune settings with `prune`, and records `now` as the time they changed when
    /// they differ from the stored ones.
    pub fn set_prune(&mut self, prune: PruneSettings, now: i64) {
        if self.prune != prune {
            self.prune = prune;
            self.prune_changed_at = Some(now);
        }
    }

    /// Returns the pruning that applies to boards of an account with `role` and `status`: none
    /// in single mode, while pruning is off, and for the admin and a pending or suspended
    /// account.
    #[must_use]
    pub fn prune_policy(&self, role: AccountRole, status: AccountStatus) -> Option<PrunePolicy> {
        let applies = self.mode == InstanceMode::Multi
            && self.prune.enabled
            && role != AccountRole::Admin
            && status == AccountStatus::Active;
        applies.then_some(PrunePolicy {
            settings: self.prune,
            changed_at: self.prune_changed_at,
        })
    }
}

impl Default for InstanceSettings {
    /// Returns the settings of an instance that has stored none: single mode, invites only.
    fn default() -> Self {
        Self {
            mode: InstanceMode::Single,
            registration: RegistrationMode::Invite,
            pending_cap: Self::DEFAULT_PENDING_CAP,
            quotas: Quotas::default(),
            limits: RateLimits::default(),
            disk: DiskLimits::default(),
            prune: PruneSettings::default(),
            prune_changed_at: None,
        }
    }
}

/// Returns an error naming `name` when `value` lies outside `min..=max`.
fn check_bound<T: PartialOrd + fmt::Display>(
    name: &str,
    value: T,
    min: T,
    max: T,
) -> Result<(), String> {
    if value < min || value > max {
        return Err(format!("{name} must be from {min} to {max}"));
    }
    Ok(())
}

/// One of the per-account [`Quotas`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Quota {
    /// Boards one account owns.
    BoardsPerAccount,
    /// Tasks on one board.
    TasksPerBoard,
    /// Comments on one task.
    CommentsPerTask,
    /// Attachments on one task.
    AttachmentsPerTask,
    /// Subtasks on one task.
    SubtasksPerTask,
    /// Saved board templates one account owns.
    TemplatesPerAccount,
    /// MiB of task rows, time entries, saved templates and attachment files one account stores.
    StoredMibPerAccount,
    /// Columns on one board.
    ColumnsPerBoard,
    /// Swimlanes on one board.
    SwimlanesPerBoard,
    /// Custom field definitions on one board.
    FieldsPerBoard,
    /// Time entries one account logs.
    TimeEntriesPerAccount,
}

impl Quota {
    /// Every quota, in the order the settings list them.
    pub const ALL: [Self; 11] = [
        Self::BoardsPerAccount,
        Self::TasksPerBoard,
        Self::CommentsPerTask,
        Self::AttachmentsPerTask,
        Self::SubtasksPerTask,
        Self::TemplatesPerAccount,
        Self::StoredMibPerAccount,
        Self::ColumnsPerBoard,
        Self::SwimlanesPerBoard,
        Self::FieldsPerBoard,
        Self::TimeEntriesPerAccount,
    ];

    /// Returns the quota's name as the settings name it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BoardsPerAccount => "boards_per_account",
            Self::TasksPerBoard => "tasks_per_board",
            Self::CommentsPerTask => "comments_per_task",
            Self::AttachmentsPerTask => "attachments_per_task",
            Self::SubtasksPerTask => "subtasks_per_task",
            Self::TemplatesPerAccount => "templates_per_account",
            Self::StoredMibPerAccount => "stored_mib_per_account",
            Self::ColumnsPerBoard => "columns_per_board",
            Self::SwimlanesPerBoard => "swimlanes_per_board",
            Self::FieldsPerBoard => "fields_per_board",
            Self::TimeEntriesPerAccount => "time_entries_per_account",
        }
    }

    /// Returns the largest value the admin may give the quota.
    #[must_use]
    pub fn max(self) -> u32 {
        match self {
            Self::BoardsPerAccount => Quotas::MAX_BOARDS_PER_ACCOUNT,
            Self::TasksPerBoard => Quotas::MAX_TASKS_PER_BOARD,
            Self::CommentsPerTask => Quotas::MAX_COMMENTS_PER_TASK,
            Self::AttachmentsPerTask => Quotas::MAX_ATTACHMENTS_PER_TASK,
            Self::SubtasksPerTask => Quotas::MAX_SUBTASKS_PER_TASK,
            Self::TemplatesPerAccount => Quotas::MAX_TEMPLATES_PER_ACCOUNT,
            Self::StoredMibPerAccount => Quotas::MAX_STORED_MIB_PER_ACCOUNT,
            Self::ColumnsPerBoard | Self::SwimlanesPerBoard | Self::FieldsPerBoard => {
                Quotas::MAX_LAYOUT_PER_BOARD
            }
            Self::TimeEntriesPerAccount => Quotas::MAX_TIME_ENTRIES_PER_ACCOUNT,
        }
    }
}

/// How much each member account may hold in multi mode. The admin's account and single mode have
/// no quotas.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Quotas {
    /// Boards one account owns, from 1 to 1000.
    pub boards_per_account: u32,
    /// Tasks on one board, from 1 to 100000. The starter board a new account gets is exempt.
    pub tasks_per_board: u32,
    /// Comments on one task, from 1 to 10000.
    pub comments_per_task: u32,
    /// Attachments on one task, from 1 to 1000.
    pub attachments_per_task: u32,
    /// Subtasks on one task, from 1 to 1000. Absent means 200.
    #[serde(default = "Quotas::default_subtasks_per_task")]
    pub subtasks_per_task: u32,
    /// Saved board templates one account owns, from 1 to 1000. Absent means 50.
    #[serde(default = "Quotas::default_templates_per_account")]
    pub templates_per_account: u32,
    /// MiB one account stores, from 1 to 65536: its boards and their layout, its task rows, its
    /// time entries, its saved templates and its attachment files. Absent means 32.
    #[serde(default = "Quotas::default_stored_mib_per_account")]
    pub stored_mib_per_account: u32,
    /// Columns on one board, from 1 to 1000. Absent means 50.
    #[serde(default = "Quotas::default_columns_per_board")]
    pub columns_per_board: u32,
    /// Swimlanes on one board, from 1 to 1000. Absent means 20.
    #[serde(default = "Quotas::default_swimlanes_per_board")]
    pub swimlanes_per_board: u32,
    /// Custom field definitions on one board, from 1 to 1000. Absent means 50.
    #[serde(default = "Quotas::default_fields_per_board")]
    pub fields_per_board: u32,
    /// Time entries one account logs, from 1 to 100000. Absent means 20000.
    #[serde(default = "Quotas::default_time_entries_per_account")]
    pub time_entries_per_account: u32,
}

impl Quotas {
    /// Attachments per task until the admin changes it.
    pub const DEFAULT_ATTACHMENTS_PER_TASK: u32 = 20;
    /// Boards per account until the admin changes it.
    pub const DEFAULT_BOARDS_PER_ACCOUNT: u32 = 20;
    /// Columns per board until the admin changes it.
    pub const DEFAULT_COLUMNS_PER_BOARD: u32 = 50;
    /// Comments per task until the admin changes it.
    pub const DEFAULT_COMMENTS_PER_TASK: u32 = 500;
    /// Custom fields per board until the admin changes it.
    pub const DEFAULT_FIELDS_PER_BOARD: u32 = 50;
    /// MiB one account stores until the admin changes it.
    pub const DEFAULT_STORED_MIB_PER_ACCOUNT: u32 = 32;
    /// Subtasks per task until the admin changes it.
    pub const DEFAULT_SUBTASKS_PER_TASK: u32 = 200;
    /// Swimlanes per board until the admin changes it.
    pub const DEFAULT_SWIMLANES_PER_BOARD: u32 = 20;
    /// Tasks per board until the admin changes it.
    pub const DEFAULT_TASKS_PER_BOARD: u32 = 2000;
    /// Saved templates per account until the admin changes it.
    pub const DEFAULT_TEMPLATES_PER_ACCOUNT: u32 = 50;
    /// Time entries per account until the admin changes it.
    pub const DEFAULT_TIME_ENTRIES_PER_ACCOUNT: u32 = 20_000;
    /// Most attachments per task the admin may allow.
    pub const MAX_ATTACHMENTS_PER_TASK: u32 = 1000;
    /// Most boards per account the admin may allow.
    pub const MAX_BOARDS_PER_ACCOUNT: u32 = 1000;
    /// Most comments per task the admin may allow.
    pub const MAX_COMMENTS_PER_TASK: u32 = 10_000;
    /// Most columns, swimlanes or custom fields per board the admin may allow.
    pub const MAX_LAYOUT_PER_BOARD: u32 = 1000;
    /// Most MiB per account the admin may allow: 64 GiB.
    pub const MAX_STORED_MIB_PER_ACCOUNT: u32 = 65_536;
    /// Most subtasks per task the admin may allow.
    pub const MAX_SUBTASKS_PER_TASK: u32 = 1000;
    /// Most tasks per board the admin may allow.
    pub const MAX_TASKS_PER_BOARD: u32 = 100_000;
    /// Most saved templates per account the admin may allow.
    pub const MAX_TEMPLATES_PER_ACCOUNT: u32 = 1000;
    /// Most time entries per account the admin may allow.
    pub const MAX_TIME_ENTRIES_PER_ACCOUNT: u32 = 100_000;
    /// Fewest of anything a quota allows.
    pub const MIN: u32 = 1;

    /// Returns [`Quotas::DEFAULT_SUBTASKS_PER_TASK`], which a stored row without the quota reads.
    fn default_subtasks_per_task() -> u32 {
        Self::DEFAULT_SUBTASKS_PER_TASK
    }

    /// Returns [`Quotas::DEFAULT_TEMPLATES_PER_ACCOUNT`], which a stored row without the quota
    /// reads.
    fn default_templates_per_account() -> u32 {
        Self::DEFAULT_TEMPLATES_PER_ACCOUNT
    }

    /// Returns [`Quotas::DEFAULT_STORED_MIB_PER_ACCOUNT`], which a stored row without the quota
    /// reads.
    fn default_stored_mib_per_account() -> u32 {
        Self::DEFAULT_STORED_MIB_PER_ACCOUNT
    }

    /// Returns [`Quotas::DEFAULT_COLUMNS_PER_BOARD`], which a stored row without the quota reads.
    fn default_columns_per_board() -> u32 {
        Self::DEFAULT_COLUMNS_PER_BOARD
    }

    /// Returns [`Quotas::DEFAULT_SWIMLANES_PER_BOARD`], which a stored row without the quota
    /// reads.
    fn default_swimlanes_per_board() -> u32 {
        Self::DEFAULT_SWIMLANES_PER_BOARD
    }

    /// Returns [`Quotas::DEFAULT_FIELDS_PER_BOARD`], which a stored row without the quota reads.
    fn default_fields_per_board() -> u32 {
        Self::DEFAULT_FIELDS_PER_BOARD
    }

    /// Returns [`Quotas::DEFAULT_TIME_ENTRIES_PER_ACCOUNT`], which a stored row without the quota
    /// reads.
    fn default_time_entries_per_account() -> u32 {
        Self::DEFAULT_TIME_ENTRIES_PER_ACCOUNT
    }

    /// Returns how many of `quota`'s items one account, board or task may hold.
    #[must_use]
    pub fn limit(&self, quota: Quota) -> u32 {
        match quota {
            Quota::BoardsPerAccount => self.boards_per_account,
            Quota::TasksPerBoard => self.tasks_per_board,
            Quota::CommentsPerTask => self.comments_per_task,
            Quota::AttachmentsPerTask => self.attachments_per_task,
            Quota::SubtasksPerTask => self.subtasks_per_task,
            Quota::TemplatesPerAccount => self.templates_per_account,
            Quota::StoredMibPerAccount => self.stored_mib_per_account,
            Quota::ColumnsPerBoard => self.columns_per_board,
            Quota::SwimlanesPerBoard => self.swimlanes_per_board,
            Quota::FieldsPerBoard => self.fields_per_board,
            Quota::TimeEntriesPerAccount => self.time_entries_per_account,
        }
    }

    /// Returns the first bound these quotas break, worded for the admin.
    pub fn validate(&self) -> Result<(), String> {
        for quota in Quota::ALL {
            let name = format!("quotas.{}", quota.as_str());
            check_bound(&name, self.limit(quota), Self::MIN, quota.max())?;
        }
        Ok(())
    }
}

impl Default for Quotas {
    fn default() -> Self {
        Self {
            boards_per_account: Self::DEFAULT_BOARDS_PER_ACCOUNT,
            tasks_per_board: Self::DEFAULT_TASKS_PER_BOARD,
            comments_per_task: Self::DEFAULT_COMMENTS_PER_TASK,
            attachments_per_task: Self::DEFAULT_ATTACHMENTS_PER_TASK,
            subtasks_per_task: Self::DEFAULT_SUBTASKS_PER_TASK,
            templates_per_account: Self::DEFAULT_TEMPLATES_PER_ACCOUNT,
            stored_mib_per_account: Self::DEFAULT_STORED_MIB_PER_ACCOUNT,
            columns_per_board: Self::DEFAULT_COLUMNS_PER_BOARD,
            swimlanes_per_board: Self::DEFAULT_SWIMLANES_PER_BOARD,
            fields_per_board: Self::DEFAULT_FIELDS_PER_BOARD,
            time_entries_per_account: Self::DEFAULT_TIME_ENTRIES_PER_ACCOUNT,
        }
    }
}

/// One of the [`RateLimits`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RateLimit {
    /// Requests one API token can make at once.
    TokenBurst,
    /// Requests added back to an API token's bucket each second.
    TokenPerSecond,
    /// Requests one account's sessions and tokens together can make at once.
    AccountBurst,
    /// Requests added back to an account's bucket each second.
    AccountPerSecond,
    /// Requests one client address gets on each limited sign-in endpoint per minute.
    IpPerMinute,
}

impl RateLimit {
    /// Every rate limit, in the order the settings list them.
    pub const ALL: [Self; 5] = [
        Self::TokenBurst,
        Self::TokenPerSecond,
        Self::AccountBurst,
        Self::AccountPerSecond,
        Self::IpPerMinute,
    ];

    /// Returns the limit's name as the settings name it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TokenBurst => "token_burst",
            Self::TokenPerSecond => "token_per_second",
            Self::AccountBurst => "account_burst",
            Self::AccountPerSecond => "account_per_second",
            Self::IpPerMinute => "ip_per_minute",
        }
    }

    /// Returns the largest value the admin may give the limit.
    #[must_use]
    pub fn max(self) -> u32 {
        match self {
            Self::TokenBurst | Self::AccountBurst => RateLimits::MAX_BURST,
            Self::TokenPerSecond | Self::AccountPerSecond => RateLimits::MAX_PER_SECOND,
            Self::IpPerMinute => RateLimits::MAX_IP_PER_MINUTE,
        }
    }
}

/// How fast requests may come: a token bucket per API token, one per account shared by its
/// sessions and tokens, and a one-minute window per client address on sign-in, setup, device
/// pairing, registration and recovery.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimits {
    /// Requests one API token can make at once from a full bucket, from 1 to 10000.
    pub token_burst: u32,
    /// Requests added back to an API token's bucket each second, from 1 to 1000.
    pub token_per_second: u32,
    /// Requests one account's sessions and tokens together can make at once, from 1 to 10000.
    pub account_burst: u32,
    /// Requests added back to an account's bucket each second, from 1 to 1000.
    pub account_per_second: u32,
    /// Requests one client address gets on each limited sign-in endpoint per minute, from 1 to
    /// 1000.
    pub ip_per_minute: u32,
}

impl RateLimits {
    /// Burst of an account until the admin changes it.
    pub const DEFAULT_ACCOUNT_BURST: u32 = 500;
    /// Refill of an account until the admin changes it.
    pub const DEFAULT_ACCOUNT_PER_SECOND: u32 = 100;
    /// Requests per client address and minute until the admin changes it.
    pub const DEFAULT_IP_PER_MINUTE: u32 = 10;
    /// Burst of an API token until the admin changes it.
    pub const DEFAULT_TOKEN_BURST: u32 = 100;
    /// Refill of an API token until the admin changes it.
    pub const DEFAULT_TOKEN_PER_SECOND: u32 = 20;
    /// Largest burst the admin may allow.
    pub const MAX_BURST: u32 = 10_000;
    /// Most requests per client address and minute the admin may allow.
    pub const MAX_IP_PER_MINUTE: u32 = 1000;
    /// Largest refill per second the admin may allow.
    pub const MAX_PER_SECOND: u32 = 1000;
    /// Smallest value of any rate limit.
    pub const MIN: u32 = 1;

    /// Returns the value of `limit`.
    #[must_use]
    pub fn value(&self, limit: RateLimit) -> u32 {
        match limit {
            RateLimit::TokenBurst => self.token_burst,
            RateLimit::TokenPerSecond => self.token_per_second,
            RateLimit::AccountBurst => self.account_burst,
            RateLimit::AccountPerSecond => self.account_per_second,
            RateLimit::IpPerMinute => self.ip_per_minute,
        }
    }

    /// Returns the first bound these limits break, worded for the admin.
    pub fn validate(&self) -> Result<(), String> {
        for limit in RateLimit::ALL {
            let name = format!("limits.{}", limit.as_str());
            check_bound(&name, self.value(limit), Self::MIN, limit.max())?;
        }
        Ok(())
    }
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            token_burst: Self::DEFAULT_TOKEN_BURST,
            token_per_second: Self::DEFAULT_TOKEN_PER_SECOND,
            account_burst: Self::DEFAULT_ACCOUNT_BURST,
            account_per_second: Self::DEFAULT_ACCOUNT_PER_SECOND,
            ip_per_minute: Self::DEFAULT_IP_PER_MINUTE,
        }
    }
}

/// Reads an optional value whose field must be present, so a missing field is an error rather
/// than [`None`].
fn present_option<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer)
}

/// When the instance stops taking writes that grow the database. Deletes, sign-in, sign-out,
/// setup, recovery, account deletion and admin actions stay open.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiskLimits {
    /// Most data the database may hold in MiB, from 16 to 1048576, or null for no limit. The field
    /// is required. It applies in multi mode only, to both the data the database holds and the
    /// size of its file, which can stay larger after deletes.
    #[serde(deserialize_with = "present_option")]
    pub max_database_mib: Option<u32>,
    /// Free disk space in MiB below which writes stop, from 0 (no floor) to 1048576. It applies in
    /// both modes, and a backup must also leave this much free.
    pub min_free_mib: u32,
}

impl DiskLimits {
    /// Most data the database holds in MiB until the admin changes it.
    pub const DEFAULT_MAX_DATABASE_MIB: u32 = 512;
    /// Free disk space floor in MiB until the admin changes it.
    pub const DEFAULT_MIN_FREE_MIB: u32 = 256;
    /// Free space floor that turns the floor off.
    pub const FLOOR_OFF_MIB: u32 = 0;
    /// Largest database limit or free space floor in MiB the admin may set: 1 TiB.
    pub const MAX_MIB: u32 = 1_048_576;
    /// Smallest database limit in MiB the admin may set.
    pub const MIN_DATABASE_MIB: u32 = 16;

    /// Returns the first bound these settings break, worded for the admin.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(max) = self.max_database_mib {
            check_bound(
                "disk.max_database_mib",
                max,
                Self::MIN_DATABASE_MIB,
                Self::MAX_MIB,
            )?;
        }
        check_bound(
            "disk.min_free_mib",
            self.min_free_mib,
            Self::FLOOR_OFF_MIB,
            Self::MAX_MIB,
        )
    }
}

impl Default for DiskLimits {
    fn default() -> Self {
        Self {
            max_database_mib: Some(Self::DEFAULT_MAX_DATABASE_MIB),
            min_free_mib: Self::DEFAULT_MIN_FREE_MIB,
        }
    }
}

/// When member boards nobody uses are deleted, in multi mode. The boards of the admin and of
/// pending and suspended accounts are never pruned, and neither is anything in single mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PruneSettings {
    /// Whether unused boards are deleted at all.
    pub enabled: bool,
    /// Days without activity after which a board is deleted, from 30 to 3650.
    pub inactive_days: u32,
    /// Days before the deletion during which the board shows a warning, from 1 to 365 and fewer
    /// than `inactive_days`.
    pub warning_days: u32,
}

impl PruneSettings {
    /// Days without activity before deletion until the admin changes it.
    pub const DEFAULT_INACTIVE_DAYS: u32 = 180;
    /// Days of warning until the admin changes it.
    pub const DEFAULT_WARNING_DAYS: u32 = 14;
    /// Most days without activity the admin may allow.
    pub const MAX_INACTIVE_DAYS: u32 = 3650;
    /// Most days of warning the admin may set.
    pub const MAX_WARNING_DAYS: u32 = 365;
    /// Fewest days without activity the admin may allow.
    pub const MIN_INACTIVE_DAYS: u32 = 30;
    /// Fewest days of warning the admin may set.
    pub const MIN_WARNING_DAYS: u32 = 1;

    /// Returns the first bound these settings break, worded for the admin.
    pub fn validate(&self) -> Result<(), String> {
        check_bound(
            "prune.inactive_days",
            self.inactive_days,
            Self::MIN_INACTIVE_DAYS,
            Self::MAX_INACTIVE_DAYS,
        )?;
        check_bound(
            "prune.warning_days",
            self.warning_days,
            Self::MIN_WARNING_DAYS,
            Self::MAX_WARNING_DAYS,
        )?;
        if self.warning_days >= self.inactive_days {
            return Err("prune.warning_days must be fewer than prune.inactive_days".to_string());
        }
        Ok(())
    }
}

/// The pruning that applies to one account's boards: its settings and when they last changed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PrunePolicy {
    /// The prune settings in force.
    pub settings: PruneSettings,
    /// Unix seconds of the latest change to the settings, absent while they are the first ones.
    pub changed_at: Option<i64>,
}

impl PrunePolicy {
    /// Returns the Unix seconds at which a board last used at `last_used` is deleted. It is never
    /// earlier than a full warning period after the settings changed.
    #[must_use]
    pub fn deletes_at(&self, last_used: i64) -> i64 {
        let by_use = last_used + i64::from(self.settings.inactive_days) * SECONDS_PER_DAY;
        let warned = self
            .changed_at
            .map(|changed| changed + i64::from(self.settings.warning_days) * SECONDS_PER_DAY);
        warned.map_or(by_use, |warned| by_use.max(warned))
    }

    /// Returns whether a board last used at `last_used` is deleted at `now`.
    #[must_use]
    pub fn is_due(&self, last_used: i64, now: i64) -> bool {
        now > self.deletes_at(last_used)
    }

    /// Returns the warning a board last used at `last_used` shows at `now`, absent before the
    /// warning window opens.
    #[must_use]
    pub fn notice(&self, last_used: i64, now: i64) -> Option<PruneNotice> {
        let deletes_at = self.deletes_at(last_used);
        let opens_at = deletes_at - i64::from(self.settings.warning_days) * SECONDS_PER_DAY;
        (now >= opens_at).then(|| PruneNotice {
            idle_days: (now - last_used).max(0) / SECONDS_PER_DAY,
            deletes_at,
        })
    }
}

impl Default for PruneSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            inactive_days: Self::DEFAULT_INACTIVE_DAYS,
            warning_days: Self::DEFAULT_WARNING_DAYS,
        }
    }
}

/// The warning a board shows in the days before it is deleted for lack of use.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PruneNotice {
    /// Whole days since the board was last used.
    pub idle_days: i64,
    /// Unix seconds at which the board is deleted unless it is used.
    pub deletes_at: i64,
}

/// The outcome of the most recent run of the unused-board cleanup, stored as one JSON row.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneStatus {
    /// Unix seconds of the run.
    pub ran_at: i64,
    /// Boards the run deleted.
    pub boards_deleted: u64,
    /// Accounts the run deleted because it left them without a board.
    pub accounts_deleted: u64,
}

/// An account's recovery code, stored only as a salted hash and keyed by the account id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryCodeRow {
    /// Hex-encoded SHA-256(salt || code), over the code in canonical form.
    pub hash: String,
    /// Hex-encoded random salt.
    pub salt: String,
    /// Unix seconds of the code's issue.
    pub created_at: i64,
}

/// A single-use invite link, keyed by the SHA-256 of its token, which is never stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InviteRow {
    /// A UUID naming the invite in the admin's list.
    pub id: String,
    /// The account that minted the invite.
    pub created_by: String,
    /// Unix seconds of the invite's creation.
    pub created_at: i64,
    /// Unix seconds after which the invite no longer registers an account.
    pub expires_at: i64,
}

impl InviteRow {
    /// Returns whether the invite is past its expiry at `now`.
    #[must_use]
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// A passkey registered to a user.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PasskeyRow {
    /// A UUID naming the stored passkey, not the WebAuthn credential id.
    pub id: String,
    /// The [`UserRow`] the passkey signs in.
    pub user_id: String,
    /// The owner's label for the passkey, e.g. "Laptop".
    pub name: String,
    /// Unix seconds of the registration.
    pub created_at: i64,
    /// Unix seconds of the last sign-in with this passkey.
    pub last_used_at: Option<i64>,
    /// The webauthn-rs passkey as the JSON it serializes to: the public key and signature counter
    /// the relying party checks.
    pub credential: Value,
}

impl PasskeyRow {
    /// Returns the stored credential.
    pub fn passkey(&self) -> serde_json::Result<Passkey> {
        serde_json::from_value(self.credential.clone())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRow {
    pub user_id: String,
    pub created_at: String,
    /// RFC 3339 time the session last proved a passkey, absent for a session that never did.
    #[serde(default)]
    pub verified_at: Option<String>,
}

/// One Pomodoro break activity (KanbanFlow parity: Break activities tab).
/// Shown as break suggestions; a null `daily_limit` means "No limit".
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BreakActivity {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_daily_goal")]
    pub daily_goal: u32,
    #[serde(default)]
    pub daily_limit: Option<u32>,
}

fn default_daily_goal() -> u32 {
    1
}

/// App settings, edited on the /settings page and stored as one JSON row.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Settings {
    pub pomodoro_minutes: u32,
    pub short_break_minutes: u32,
    pub long_break_minutes: u32,
    /// Take a long break every N completed pomodori.
    pub long_break_every: u32,
    /// Show a browser notification when a pomodoro or break ends.
    pub notifications_enabled: bool,
    /// Preset answers for "Why did you stop?", editable on /settings.
    pub interrupt_reasons: Vec<String>,
    /// Ticking mode: "always" | "timer_start" | "never".
    #[serde(default = "default_ticking_mode")]
    pub ticking_mode: String,
    /// Alarm sound id: bell | chime | beeps | blip | glass | microwave |
    /// egg_timer | grandpa_clock | melodic.
    #[serde(default = "default_alarm_sound")]
    pub alarm_sound: String,
    /// Alarm volume 0-100.
    #[serde(default = "default_volume")]
    pub alarm_volume: u32,
    /// Points volume 0-100.
    #[serde(default = "default_volume")]
    pub points_volume: u32,
    /// Master sounds toggle.
    #[serde(default = "default_true")]
    pub sounds_enabled: bool,
    /// Picture-in-Picture toggle.
    #[serde(default = "default_true")]
    pub pip_enabled: bool,
    /// Break activities for the Timer settings modal's Break activities tab. Empty for rows written
    /// before the field existed.
    #[serde(default)]
    pub break_activities: Vec<BreakActivity>,
    /// Board ids pinned in the Boards sidebar's Favorites section. Empty for rows written before
    /// the field existed.
    #[serde(default)]
    pub favorite_boards: Vec<String>,
}

fn default_ticking_mode() -> String {
    "never".to_string()
}

fn default_alarm_sound() -> String {
    "bell".to_string()
}

fn default_volume() -> u32 {
    70
}

fn default_true() -> bool {
    true
}

impl Settings {
    /// Returns [`TooLong`] for the first interrupt reason or break activity text past its
    /// [`TextCap`] that `before`, the settings as stored, does not hold, or when either list
    /// grows past its most entries. Text the settings already held stays, so an older row past
    /// a cap that was added later can still be saved.
    pub fn check_caps(&self, before: Option<&Self>) -> Result<(), TooLong> {
        let held_reasons = before.map_or(&[][..], |held| held.interrupt_reasons.as_slice());
        if self.interrupt_reasons.len() > held_reasons.len() {
            TextCap::INTERRUPT_REASONS.check_count(&self.interrupt_reasons)?;
        }
        for reason in &self.interrupt_reasons {
            if !held_reasons.contains(reason) {
                TextCap::INTERRUPT_REASON.check(reason)?;
            }
        }
        let held_activities = before.map_or(&[][..], |held| held.break_activities.as_slice());
        if self.break_activities.len() > held_activities.len() {
            TextCap::BREAK_ACTIVITIES.check_count(&self.break_activities)?;
        }
        for activity in &self.break_activities {
            let held = held_activities.iter().find(|held| held.id == activity.id);
            let texts = [
                (
                    TextCap::BREAK_ACTIVITY_ID,
                    &activity.id,
                    held.map(|held| &held.id),
                ),
                (
                    TextCap::BREAK_ACTIVITY_NAME,
                    &activity.name,
                    held.map(|held| &held.name),
                ),
                (
                    TextCap::BREAK_ACTIVITY_DESCRIPTION,
                    &activity.description,
                    held.map(|held| &held.description),
                ),
            ];
            for (cap, text, stored) in texts {
                if stored != Some(text) {
                    cap.check(text)?;
                }
            }
        }
        Ok(())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            pomodoro_minutes: 25,
            short_break_minutes: 5,

            // KanbanFlow's default long break (live ref 08)
            long_break_minutes: 15,
            long_break_every: 4,
            notifications_enabled: true,
            ticking_mode: default_ticking_mode(),
            alarm_sound: default_alarm_sound(),
            alarm_volume: default_volume(),
            points_volume: default_volume(),
            sounds_enabled: true,
            pip_enabled: true,
            break_activities: Vec::new(),
            favorite_boards: Vec::new(),
            interrupt_reasons: vec![
                "Boss interrupted",
                "Colleague interrupted",
                "Context switch",
                "Dog",
                "Email",
                "Family",
                "Finished with no new task",
                "Food Delivery",
                "Meeting",
                "Other",
                "Phone call",
                "Restroom",
                "Sleep",
                "Web browsing",
                "Workchat",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }
}

/// How the scheduled database backups run, stored as one JSON row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct BackupSettings {
    /// Whether the schedule takes backups. Off until the owner turns it on.
    pub enabled: bool,
    /// Hours between scheduled backups, from 1 to 168.
    pub interval_hours: u32,
    /// Backup files kept after each successful backup, from 1 to 90. Older ones are deleted.
    pub keep: u32,
}

impl BackupSettings {
    /// Hours between scheduled backups until the owner changes it.
    pub const DEFAULT_INTERVAL_HOURS: u32 = 24;
    /// Backup files kept until the owner changes it.
    pub const DEFAULT_KEEP: u32 = 7;
    /// Most hours between scheduled backups: one week.
    pub const MAX_INTERVAL_HOURS: u32 = 168;
    /// Most backup files kept.
    pub const MAX_KEEP: u32 = 90;
    /// Fewest hours between scheduled backups.
    pub const MIN_INTERVAL_HOURS: u32 = 1;
    /// Fewest backup files kept.
    pub const MIN_KEEP: u32 = 1;
    /// Seconds in one hour.
    const SECONDS_PER_HOUR: i64 = 3600;

    /// Returns the interval between scheduled backups in seconds.
    #[must_use]
    pub fn interval_secs(&self) -> i64 {
        i64::from(self.interval_hours) * Self::SECONDS_PER_HOUR
    }

    /// Returns the first bound these settings break, worded for the owner.
    pub fn validate(&self) -> Result<(), String> {
        if !(Self::MIN_INTERVAL_HOURS..=Self::MAX_INTERVAL_HOURS).contains(&self.interval_hours) {
            return Err(format!(
                "interval_hours must be from {} to {}",
                Self::MIN_INTERVAL_HOURS,
                Self::MAX_INTERVAL_HOURS
            ));
        }
        if !(Self::MIN_KEEP..=Self::MAX_KEEP).contains(&self.keep) {
            return Err(format!(
                "keep must be from {} to {}",
                Self::MIN_KEEP,
                Self::MAX_KEEP
            ));
        }
        Ok(())
    }
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_hours: Self::DEFAULT_INTERVAL_HOURS,
            keep: Self::DEFAULT_KEEP,
        }
    }
}

/// The outcome of the most recent backup attempt, stored as one JSON row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct BackupStatus {
    /// Unix time of the attempt, in seconds.
    pub at: i64,
    /// Whether the attempt produced a backup file.
    pub ok: bool,
    /// What went wrong, when `ok` is false.
    pub error: Option<String>,
    /// Name of the file the attempt wrote, when `ok` is true.
    pub file: Option<String>,
    /// Size in bytes of the file the attempt wrote, when `ok` is true.
    pub size_bytes: Option<u64>,
    /// Unix time of the latest successful backup, in seconds. It survives later failures.
    pub last_ok_at: Option<i64>,
}

/// What the timer is currently doing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerMode {
    Pomodoro,
    Stopwatch,
    ShortBreak,
    LongBreak,
}

impl std::str::FromStr for TimerMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pomodoro" => Ok(TimerMode::Pomodoro),
            "stopwatch" => Ok(TimerMode::Stopwatch),
            "short_break" => Ok(TimerMode::ShortBreak),
            "long_break" => Ok(TimerMode::LongBreak),
            _ => Err(format!("unknown timer mode: {value}")),
        }
    }
}

impl TimerMode {
    pub fn title(self) -> &'static str {
        match self {
            TimerMode::Pomodoro => "Pomodoro",
            TimerMode::Stopwatch => "Stopwatch",
            TimerMode::ShortBreak => "Short break",
            TimerMode::LongBreak => "Long break",
        }
    }

    /// snake_case id used by the API and JavaScript.
    pub fn as_str(self) -> &'static str {
        match self {
            TimerMode::Pomodoro => "pomodoro",
            TimerMode::Stopwatch => "stopwatch",
            TimerMode::ShortBreak => "short_break",
            TimerMode::LongBreak => "long_break",
        }
    }

    /// Returns the entry kind recorded when a session of this mode is logged.
    pub fn entry_kind(self) -> EntryKind {
        match self {
            TimerMode::Pomodoro => EntryKind::Pomodoro,
            TimerMode::Stopwatch => EntryKind::Stopwatch,
            TimerMode::ShortBreak => EntryKind::ShortBreak,
            TimerMode::LongBreak => EntryKind::LongBreak,
        }
    }
}

/// What kind of session a time entry records.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A pomodoro work session.
    Pomodoro,
    /// A stopwatch session.
    Stopwatch,
    /// A short break.
    ShortBreak,
    /// A long break.
    LongBreak,
    /// Minutes logged by hand.
    Manual,
}

impl EntryKind {
    /// Returns the string stored in a time entry's `kind` field.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Pomodoro => "pomodoro",
            EntryKind::Stopwatch => "stopwatch",
            EntryKind::ShortBreak => "short_break",
            EntryKind::LongBreak => "long_break",
            EntryKind::Manual => "manual",
        }
    }

    /// Parses a stored `kind` string, returning `None` for a value no kind writes.
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "pomodoro" => Some(EntryKind::Pomodoro),
            "stopwatch" => Some(EntryKind::Stopwatch),
            "short_break" => Some(EntryKind::ShortBreak),
            "long_break" => Some(EntryKind::LongBreak),
            "manual" => Some(EntryKind::Manual),
            _ => None,
        }
    }

    /// Reports whether the kind is a pomodoro work session.
    pub fn is_pomodoro(self) -> bool {
        self == EntryKind::Pomodoro
    }

    /// Reports whether the kind is a short or long break.
    pub fn is_break(self) -> bool {
        matches!(self, EntryKind::ShortBreak | EntryKind::LongBreak)
    }
}

/// The single currently-running timer, if any. `duration_secs` is `None`
/// for the stopwatch, which counts up without a target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveTimer {
    pub task_id: Option<String>,
    pub mode: TimerMode,
    pub started_at: i64,
    pub duration_secs: Option<u64>,
}

/// Where a device-pairing request stands.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAuthStatus {
    Pending,
    Approved,
    Denied,
    /// The token was handed to the device; the code cannot be used again.
    Issued,
}

/// A device-pairing request (RFC 8628 device authorization grant). Keyed
/// by the SHA-256 of its device code; the code itself is never stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceAuthRow {
    pub device_code_hash: String,
    /// Canonical user code, e.g. "BCDFGHJK".
    pub user_code: String,
    pub client_name: String,
    /// The scope the client requested, lowered to the granted one when the owner approves.
    pub scope: AuthScope,
    pub status: DeviceAuthStatus,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub expires_at: i64,
    /// Unix seconds of the last token poll, for the polling interval.
    pub last_poll_at: Option<i64>,
    /// Unix seconds at which the token issued for this request expires, set when the owner
    /// approves it; [`None`] means the token never expires.
    pub token_expires_at: Option<i64>,
    /// The account that approved the request, which the issued token acts for.
    #[serde(default)]
    pub account_id: Option<String>,
    /// The client network the request came from, which caps its pending requests. Absent for a
    /// request stored before the cap counted per network.
    #[serde(default)]
    pub client_network: Option<String>,
}

/// A named API token for agent/script access. The raw token is shown once
/// at creation and never stored; only its SHA-256 is persisted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiTokenRecord {
    pub id: String,
    pub name: String,
    /// Token prefix shown in the UI (e.g. "pomb_"), never the secret.
    pub prefix: String,
    /// Last 4 characters of the raw token, for visual identification.
    pub last4: String,
    /// Hex-encoded SHA-256 of the token, or SHA-256(salt || token) while `salt` is set.
    pub token_hash: String,
    /// Hex-encoded random salt of a token that predates the keyed lookup, empty once the token is
    /// keyed by its SHA-256.
    pub salt: String,
    /// The access the token grants.
    pub scope: AuthScope,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    /// Unix timestamp, or None for no expiry.
    pub expires_at: Option<i64>,
    /// The account the token acts for.
    #[serde(default)]
    pub account_id: String,
}

/// Access a credential holds. The scopes are ordered, and each includes the ones before it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthScope {
    /// Permits every GET except the calendar-feed secret.
    Read,
    /// Adds the task workflow: create, update and move tasks, claims, comments, subtasks, links,
    /// attachments, time, the timer and the watch flag.
    Work,
    /// Adds creating boards, blank or from a template, adding columns, swimlanes and fields,
    /// adding options and table columns to a field, and converting a multiline field. On a board
    /// the same token created, it also changes a color's label, description and default and
    /// reorders columns.
    Provision,
    /// Permits everything else: deletes, board structure, settings and the calendar feed.
    Admin,
}

impl AuthScope {
    /// Every scope, lowest first.
    pub const ALL: [Self; 4] = [Self::Read, Self::Work, Self::Provision, Self::Admin];

    /// Returns the scope's stored and wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Work => "work",
            Self::Provision => "provision",
            Self::Admin => "admin",
        }
    }

    /// Parses a submitted scope string, returning [`None`] for an unknown one.
    #[must_use]
    pub fn parse(scope: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == scope)
    }

    /// Returns the scope names as a quoted list for messages, e.g. `["read", "work"]`.
    #[must_use]
    pub fn quoted_list() -> String {
        let quoted: Vec<String> = Self::ALL
            .into_iter()
            .map(|scope| format!("\"{}\"", scope.as_str()))
            .collect();
        format!("[{}]", quoted.join(", "))
    }

    /// Returns the scope names as prose for messages, e.g. `read, work or admin`.
    #[must_use]
    pub fn choice_list() -> String {
        let names: Vec<&str> = Self::ALL.into_iter().map(Self::as_str).collect();
        match names.split_last() {
            Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
            None => String::new(),
        }
    }

    /// Returns the highest of the submitted scope strings, or [`None`] when the list is empty or
    /// names an unknown scope.
    #[must_use]
    pub fn highest(scopes: &[String]) -> Option<Self> {
        let parsed: Option<Vec<Self>> = scopes.iter().map(|scope| Self::parse(scope)).collect();
        parsed?.into_iter().max()
    }
}

impl ApiTokenRecord {
    /// Returns whether this token grants `scope`, where each scope includes the ones below it.
    #[must_use]
    pub fn has_scope(&self, scope: AuthScope) -> bool {
        self.scope >= scope
    }

    /// True when the token is past its expiry (None = never expires).
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.map(|exp| now >= exp).unwrap_or(false)
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;

    fn stale_row(value: &str) -> ColorRow {
        ColorRow {
            id: "c".to_string(),
            board_id: "b".to_string(),
            value: value.to_string(),
            label: "Label".to_string(),
            description: String::new(),
            background_hex: "#000001".to_string(),
            border_hex: "#000002".to_string(),
            light_hex: "#000003".to_string(),
            enabled: true,
            is_default: false,
            sort_order: 1,
        }
    }

    /// One method maps a standard color to the palette's hexes.
    #[test]
    fn with_palette_hexes_should_use_the_palette_for_standard_colors_only() {
        let blue = stale_row("blue").with_palette_hexes();
        assert_eq!(blue.background_hex, "#cce3ff");
        assert_eq!(blue.border_hex, "#70b0ff");
        assert_eq!(blue.light_hex, "#d6e9ff");

        let custom = stale_row("teal").with_palette_hexes();
        assert_eq!(custom.border_hex, "#000002");
    }
}

#[cfg(test)]
mod entry_kind_tests {
    use super::*;

    #[test]
    fn parse_should_round_trip_every_kind_through_as_str() {
        for kind in [
            EntryKind::Pomodoro,
            EntryKind::Stopwatch,
            EntryKind::ShortBreak,
            EntryKind::LongBreak,
            EntryKind::Manual,
        ] {
            assert_eq!(EntryKind::parse(kind.as_str()), Some(kind));
        }
    }
}

#[cfg(test)]
mod backup_settings_tests {
    use super::*;

    fn with(interval_hours: u32, keep: u32) -> BackupSettings {
        BackupSettings {
            enabled: true,
            interval_hours,
            keep,
        }
    }

    #[test]
    fn default_should_be_off_daily_and_keep_seven() {
        let settings = BackupSettings::default();
        assert_eq!(
            (settings.enabled, settings.interval_hours, settings.keep),
            (
                false,
                BackupSettings::DEFAULT_INTERVAL_HOURS,
                BackupSettings::DEFAULT_KEEP
            )
        );
        assert_eq!(settings.validate(), Ok(()));
    }

    #[test]
    fn validate_should_accept_the_bounds() {
        for (hours, keep) in [
            (1, 1),
            (168, 90),
            (
                BackupSettings::DEFAULT_INTERVAL_HOURS,
                BackupSettings::DEFAULT_KEEP,
            ),
        ] {
            assert_eq!(with(hours, keep).validate(), Ok(()), "{hours} {keep}");
        }
    }

    #[test]
    fn validate_should_refuse_values_outside_the_bounds() {
        for (hours, keep) in [(0, 7), (169, 7), (24, 0), (24, 91)] {
            assert!(with(hours, keep).validate().is_err(), "{hours} {keep}");
        }
        assert!(
            with(0, 7)
                .validate()
                .unwrap_err()
                .contains("interval_hours")
        );
        assert!(with(24, 91).validate().unwrap_err().contains("keep"));
    }

    #[test]
    fn deserialize_should_fill_missing_fields_with_defaults() {
        let settings: BackupSettings = serde_json::from_str(r#"{"enabled":true}"#).expect("json");
        assert_eq!(
            settings,
            with(
                BackupSettings::DEFAULT_INTERVAL_HOURS,
                BackupSettings::DEFAULT_KEEP
            )
        );
    }
}

#[cfg(test)]
mod count_noun_tests {
    use super::*;

    #[test]
    fn count_noun_should_pluralize_every_count_but_one() {
        assert_eq!(count_noun(0, "board"), "0 boards");
        assert_eq!(count_noun(1, "board"), "1 board");
        assert_eq!(count_noun(3, "unused board"), "3 unused boards");
    }
}
