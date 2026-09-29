//! Domain types shared by the DB layer, routes, and templates.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Pomodoro size of a task. `Many` (stored as 4) means ">3 pomodori".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Human label, e.g. "2 Pomodori".
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardRow {
    pub id: String,
    pub name: String,
    pub position: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

fn default_column_config() -> String {
    "{}".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwimlaneRow {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Per-board task color (a [`ColorRow`] id), or None for rows written
    /// before colors existed — those render via the legacy size mapping.
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
    /// ("Edit grouping date"). None means use the created/completed date.
    #[serde(default)]
    pub grouping_date: Option<String>,
}

/// One checklist item on a task (KanbanFlow parity: Subtasks section).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Subtask {
    pub id: String,
    pub name: String,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeEntryRow {
    pub id: String,
    pub task_id: String,
    pub minutes: i64,
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
}

fn default_entry_kind() -> String {
    "manual".to_string()
}

// ---- Per-board task colors (KanbanFlow parity) ----

/// One color slot on a board's palette. The hex values are fixed per
/// standard color (see [`STANDARD_COLORS`]); the board config only
/// enables/disables, renames, describes, reorders, and picks the default.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// One standard KanbanFlow color: (value, background, border, light,
/// default label). Hex values are fixed and never user-editable.
pub const STANDARD_COLORS: &[(&str, &str, &str, &str, &str)] = &[
    ("yellow", "#ffffe0", "#f5cc00", "#ffffe0", "Yellow"),
    ("green", "#dbffc2", "#59d600", "#e4ffd1", "Green"),
    ("blue", "#cce3ff", "#70b0ff", "#d6e9ff", "Blue"),
    ("red", "#ffccd0", "#ff858f", "#ffe0e3", "Red"),
    ("orange", "#ffeac2", "#faa200", "#ffeac2", "Orange"),
    ("purple", "#eddbff", "#c994ff", "#eddbff", "Purple"),
    ("magenta", "#ffe0ff", "#ff85ff", "#ffe0ff", "Magenta"),
    ("cyan", "#dbffff", "#00d6d6", "#dbffff", "Cyan"),
    ("brown", "#f6ddcb", "#e49b67", "#f6ddcb", "Brown"),
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardTemplateRow {
    pub id: String,
    pub name: String,
    pub description: String,
    /// True for the shipped built-in templates (cannot be deleted).
    pub built_in: bool,
    pub snapshot: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub user_id: String,
    pub created_at: String,
}

/// App settings, edited on the /settings page and stored as one JSON row.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Settings {
    pub pomodoro_minutes: u32,
    pub short_break_minutes: u32,
    pub long_break_minutes: u32,
    /// Take a long break every N completed pomodori.
    pub long_break_every: u32,
    /// Play a ding when a pomodoro or break ends.
    /// Superseded by `sounds_enabled` + `alarm_sound`; kept for DB compat.
    pub ding_enabled: bool,
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
            long_break_minutes: 15,
            long_break_every: 4,
            ding_enabled: true,
            notifications_enabled: true,
            ticking_mode: default_ticking_mode(),
            alarm_sound: default_alarm_sound(),
            alarm_volume: default_volume(),
            points_volume: default_volume(),
            sounds_enabled: true,
            pip_enabled: true,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Entry kind recorded when a session of this mode is logged.
    pub fn entry_kind(self) -> &'static str {
        match self {
            TimerMode::Pomodoro => "pomodoro",
            TimerMode::Stopwatch => "stopwatch",
            TimerMode::ShortBreak => "short_break",
            TimerMode::LongBreak => "long_break",
        }
    }
}

/// The single currently-running timer, if any. `duration_secs` is `None`
/// for the stopwatch, which counts up without a target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTimer {
    pub task_id: Option<String>,
    pub mode: TimerMode,
    pub started_at: i64,
    pub duration_secs: Option<u64>,
}

/// A named API token for agent/script access. The raw token is shown once
/// at creation and never stored; only the salted hash is persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Scopes: subset of ["read", "write"]. "write" implies "read".
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    /// Unix timestamp, or None for no expiry.
    pub expires_at: Option<i64>,
}

impl ApiTokenRecord {
    /// True when this token grants the named scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        if scope == "read" {
            self.scopes.iter().any(|s| s == "read" || s == "write")
        } else {
            self.scopes.iter().any(|s| s == scope)
        }
    }

    /// True when the token is past its expiry (None = never expires).
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.map(|exp| now >= exp).unwrap_or(false)
    }
}
