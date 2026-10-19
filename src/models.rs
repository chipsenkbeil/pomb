use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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

/// Seconds in a day, for expiry arithmetic.
pub const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardRow {
    pub id: String,
    pub name: String,
    pub position: i64,
    /// Opaque bag for board-level settings the UI manages itself
    /// (e.g. legend visibility). Defaults to "{}" for older rows.
    #[serde(default = "default_board_config")]
    pub config_json: String,
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

fn default_column_config() -> String {
    "{}".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwimlaneRow {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i64,
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
    /// on every column move. Null for rows written before the field
    /// existed — those fall back to the creation date on cards.
    #[serde(default)]
    pub column_added_at: Option<String>,
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
        }
    }

    /// True for the kinds whose values come from `options`.
    pub fn has_options(self) -> bool {
        matches!(self, Self::Select | Self::Multiselect)
    }
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
    pub default: Option<serde_json::Value>,
    pub required: bool,
    pub owner: FieldOwner,
    pub position: i64,
    /// RFC3339.
    pub created_at: String,
}

impl FieldDefRow {
    /// Checks a non-null value against the field's kind and options and
    /// returns it as stored (multiselect choices deduplicated). The error
    /// says what was expected, without the key.
    pub fn check_value(&self, value: &serde_json::Value) -> Result<serde_json::Value, String> {
        use serde_json::Value;
        let option = |choice: &str| {
            if self.options.iter().any(|o| o == choice) {
                Ok(())
            } else {
                Err(format!("\"{choice}\" is not an option"))
            }
        };
        match (self.kind, value) {
            (FieldKind::Text, Value::String(text)) if has_line_break(text) => {
                Err("text must be a single line; use a multiline field".into())
            }
            (FieldKind::Text | FieldKind::Multiline, Value::String(_)) => Ok(value.clone()),
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
        }
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
    /// Free-form tag such as "agent", "oracle", "review", or "system";
    /// null when untagged.
    #[serde(default)]
    pub kind: Option<String>,
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

/// A named, reusable board recipe. `snapshot` captures the board's colors
/// (all, with their config), columns, and swimlanes at save time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardTemplateRow {
    pub id: String,
    pub name: String,
    pub description: String,
    /// True for the shipped built-in templates (cannot be deleted).
    pub built_in: bool,
    pub snapshot: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub password_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRow {
    pub user_id: String,
    pub created_at: String,
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            pomodoro_minutes: 25,
            short_break_minutes: 5,

            // KanbanFlow's default long break (live ref 08).
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
}

/// A named API token for agent/script access. The raw token is shown once
/// at creation and never stored; only the salted hash is persisted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiTokenRecord {
    pub id: String,
    pub name: String,
    /// Token prefix shown in the UI (e.g. "cf_"), never the secret.
    pub prefix: String,
    /// Last 4 characters of the raw token, for visual identification.
    pub last4: String,
    /// Hex-encoded SHA-256(salt || token).
    pub token_hash: String,
    /// Hex-encoded random salt.
    pub salt: String,
    /// The access the token grants.
    pub scope: AuthScope,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    /// Unix timestamp, or None for no expiry.
    pub expires_at: Option<i64>,
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
    /// Permits everything else: deletes, board structure, settings and the calendar feed.
    Admin,
}

impl AuthScope {
    /// Every scope, lowest first.
    pub const ALL: [Self; 3] = [Self::Read, Self::Work, Self::Admin];

    /// Returns the scope's stored and wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Work => "work",
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
