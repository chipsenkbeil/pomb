//! Domain types shared by the DB layer, routes, and templates.

use serde::{Deserialize, Serialize};

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

    /// Card background color for this size (KanbanFlow-style pastels).
    pub fn color_hex(self) -> &'static str {
        match self {
            Size::One => "#FFF9C4",   // yellow
            Size::Two => "#C8E6C9",   // green
            Size::Three => "#BBDEFB", // blue
            Size::Many => "#FFCDD2",  // red
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub pomodoro_minutes: u32,
    pub short_break_minutes: u32,
    pub long_break_minutes: u32,
    /// Take a long break every N completed pomodori.
    pub long_break_every: u32,
    /// Play a ding when a pomodoro or break ends.
    pub ding_enabled: bool,
    /// Show a browser notification when a pomodoro or break ends.
    pub notifications_enabled: bool,
    /// Preset answers for "Why did you stop?", editable on /settings.
    pub interrupt_reasons: Vec<String>,
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
                "Task done",
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

impl TimerMode {
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pomodoro" => Some(TimerMode::Pomodoro),
            "stopwatch" => Some(TimerMode::Stopwatch),
            "short_break" => Some(TimerMode::ShortBreak),
            "long_break" => Some(TimerMode::LongBreak),
            _ => None,
        }
    }

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
