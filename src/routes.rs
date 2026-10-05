//! HTTP routes: pages, the task JSON/form API, time tracking, and auth.
//!
//! API handlers accept both `application/json` (used by `app.js` fetch calls)
//! and form-encoded bodies (used by htmx), so either client works.

use std::collections::HashMap;

use askama::Template;
use axum::{
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::{
        header::ACCEPT, header::CONTENT_DISPOSITION, header::CONTENT_TYPE, HeaderMap, StatusCode,
    },
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, patch, post, put},
    Form, Json, Router,
};
use base64::Engine as _;
use chrono::{DateTime, Duration, Local};
#[cfg(not(debug_assertions))]
use rust_embed::RustEmbed;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{IntoParams, Modify, OpenApi, ToSchema};

use crate::auth::{self, AuthUser};
use crate::db::Db;
use crate::models::*;
use crate::AppState;

/// Build the full router. The auth middleware layer is added *before*
/// `/login` and `/setup` are registered, so those two stay public.
pub fn router(state: AppState) -> Router {
    let router = Router::new()
        .route("/", get(root))
        .route("/logout", post(logout))
        .route("/settings", get(settings_page).post(settings_submit))
        .route("/b/:board_id", get(board_page))
        .route("/api/tasks", post(create_task).get(list_tasks))
        .route(
            "/api/tasks/:id",
            get(get_task).patch(update_task).delete(delete_task),
        )
        .route("/api/tasks/:id/card", get(task_card))
        .route("/api/tasks/:id/modal", get(task_modal))
        .route("/api/tasks/:id/move", post(move_task))
        .route("/api/tasks/:id/watch", post(watch_task))
        .route("/api/tasks/:id/time", post(log_time).get(get_task_time))
        .route("/api/tasks/:id/subtasks", post(create_subtask))
        .route("/api/tasks/:id/subtasks/order", put(reorder_subtasks))
        .route(
            "/api/tasks/:id/subtasks/:sub_id",
            patch(update_subtask).delete(delete_subtask),
        )
        // Task extras: labels, comments, attachments, history (KF-061,
        // KF-064, KF-100, KF-101).
        .route("/api/boards/:id/labels", get(list_board_labels))
        .route(
            "/api/tasks/:id/comments",
            post(create_comment).get(list_comments),
        )
        .route(
            "/api/tasks/:id/comments/:comment_id",
            delete(delete_comment),
        )
        .route("/api/tasks/:id/attachments", post(upload_attachment))
        .route(
            "/api/tasks/:id/attachments/:attachment_id/file",
            get(download_attachment),
        )
        .route(
            "/api/tasks/:id/attachments/:attachment_id",
            delete(delete_attachment),
        )
        .route("/api/tasks/:id/time-log-view", get(time_log_view))
        .route("/api/tasks/:id/history-view", get(history_view))
        .route(
            "/api/time/entries/:id",
            get(get_time_entry)
                .put(update_time_entry)
                .delete(delete_time_entry),
        )
        .route("/api/members", get(list_members))
        .route("/api/time/manual", post(create_manual_time))
        .route("/api/timer/start", post(timer_start))
        .route("/api/timer/status", get(timer_status))
        .route("/api/timer/stop", post(timer_stop))
        .route("/api/timer/retarget", post(timer_retarget))
        .route("/api/timer/today", get(timer_today))
        .route("/api/timer/settings", get(api_settings))
        .route("/timer/log", get(timer_log_page))
        .route("/timer/statistics", get(timer_statistics_page))
        .route("/api/timer/log", get(api_timer_log))
        .route("/api/timer/time-spent", get(api_time_spent))
        .route("/api/timer/statistics", get(api_timer_statistics))
        .route("/api/settings", get(api_settings).put(api_update_settings))
        .route("/api/columns", post(create_column))
        .route(
            "/api/columns/:id",
            patch(update_column).delete(delete_column),
        )
        .route("/api/columns/:id/move", post(move_column))
        .route("/api/swimlanes", post(create_swimlane))
        .route(
            "/api/swimlanes/:id",
            patch(update_swimlane).delete(delete_swimlane),
        )
        .route("/api/swimlanes/:id/move", post(move_swimlane))
        // Boards, board templates, and per-board task colors.
        .route("/api/boards", get(list_boards).post(create_board))
        .route("/api/boards/:id", delete(delete_board_api))
        .route("/api/boards/:id/config", put(update_board_config))
        .route("/api/boards/:id/columns", get(list_board_columns))
        .route("/b/:board_id/settings/delete", get(board_delete_page))
        .route("/boards/new", get(new_board_page))
        .route("/boards/archived", get(archived_boards_page))
        .route(
            "/api/boards/:id/save-as-template",
            post(save_board_as_template),
        )
        .route("/api/templates", get(list_templates))
        .route("/api/templates/:id", delete(delete_template))
        .route("/b/:board_id/settings/colors", get(board_colors_page))
        // Board settings shell (KF-080): General, Layout, Colors, Task
        // settings, Advanced, API & Webhooks, Add task from email.
        .route(
            "/b/:board_id/settings",
            get(board_settings_page).post(board_settings_submit),
        )
        .route(
            "/b/:board_id/settings/layout",
            get(board_settings_layout_page),
        )
        .route(
            "/b/:board_id/settings/task-settings",
            get(board_settings_task_page),
        )
        .route(
            "/b/:board_id/settings/advanced",
            get(board_settings_advanced_page),
        )
        .route(
            "/b/:board_id/settings/api-webhooks",
            get(board_settings_api_page),
        )
        .route(
            "/b/:board_id/settings/add-from-email",
            get(board_settings_email_page),
        )
        .route(
            "/api/boards/:id/colors",
            get(list_board_colors).post(create_board_color),
        )
        .route(
            "/api/boards/:id/colors/:color_id",
            patch(update_board_color).delete(delete_board_color),
        )
        // Copy another board's whole palette onto this board (KF-083).
        .route("/api/boards/:id/colors/copy-from", post(copy_board_colors))
        // Agent API tokens. Creation/listing/revocation require the
        // browser session cookie — a Bearer token can never mint tokens.
        .route(
            "/api/v1/auth/tokens",
            post(create_api_token).get(list_api_tokens),
        )
        .route("/api/v1/auth/tokens/:id", delete(revoke_api_token))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/login", get(login_page).post(login_submit))
        // Agent discovery endpoints: public documentation, no secrets.
        // Registered after the auth layer so agents can fetch them with
        // just the base URL.
        .route("/agents.md", get(agents_md))
        .route("/agents/skill.md", get(agents_skill_md))
        .route("/.well-known/agents.json", get(agents_json))
        .route("/api/v1/openapi.json", get(openapi_json))
        .route("/api/v1/version", get(version_info))
        .route("/setup", get(setup_page).post(setup_submit));
    // Static assets (`static/app.js`, `static/style.css`): served from disk
    // in debug builds so edits show up without a rebuild; embedded in the
    // binary in release builds so `cargo install` produces a fully
    // self-contained binary with no `./static` directory needed next to it.
    #[cfg(debug_assertions)]
    let router = router.nest_service("/static", ServeDir::new("static"));
    #[cfg(not(debug_assertions))]
    let router = router.route("/static/*path", get(serve_embedded_static));
    router.with_state(state)
}

/// Static assets embedded in release builds so the installed binary is
/// self-contained (see `router`).
#[cfg(not(debug_assertions))]
#[derive(RustEmbed)]
#[folder = "static/"]
struct EmbeddedStatic;

#[cfg(not(debug_assertions))]
async fn serve_embedded_static(Path(path): Path<String>) -> impl IntoResponse {
    match EmbeddedStatic::get(&path) {
        Some(file) => {
            let content_type = if path.ends_with(".js") {
                "text/javascript"
            } else if path.ends_with(".css") {
                "text/css"
            } else {
                "application/octet-stream"
            };
            ([(CONTENT_TYPE, content_type)], file.data).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---- Errors ----

/// Simple error type that renders as an HTTP response.
pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

impl From<Box<dyn std::error::Error>> for AppError {
    fn from(error: Box<dyn std::error::Error>) -> Self {
        Self::internal(error.to_string())
    }
}

/// Parse a request body as JSON or as a form, based on Content-Type.
async fn parse_body<T: DeserializeOwned>(headers: &HeaderMap, body: Bytes) -> Result<T, AppError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type.contains("application/json") {
        serde_json::from_slice(&body)
            .map_err(|error| AppError::bad_request(format!("invalid JSON: {error}")))
    } else {
        serde_urlencoded::from_bytes(&body)
            .map_err(|error| AppError::bad_request(format!("invalid form body: {error}")))
    }
}

/// True when the client explicitly asked for JSON (`Accept:
/// application/json`). Default `*/*` (curl, htmx, browsers) keeps the
/// legacy HTML responses.
fn accepts_json(headers: &HeaderMap) -> bool {
    headers.get_all(ACCEPT).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(',').any(|part| {
                part.split(';')
                    .next()
                    .is_some_and(|media| media.trim() == "application/json")
            })
        })
    })
}

// ---- API view models ----
//
// These structs are the JSON shapes returned by the API handlers. They are
// the single source of truth for both the runtime responses and the
// generated OpenAPI spec (via `ToSchema`), so the spec cannot drift from
// the implementation.

/// `{ "id", "name" }` pair, e.g. one task in `GET /api/tasks`.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TaskNameItem {
    id: String,
    name: String,
}

/// 201 JSON body for `POST /api/tasks` when the client sends
/// `Accept: application/json` (KF-221a): a clean JSON way to get the
/// created task's id without parsing the HTML card fragment.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct CreateTaskResponse {
    id: String,
    name: String,
    column_id: String,
    swimlane_id: Option<String>,
}

/// `{ "id" }` — returned when creating columns, swimlanes, etc.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct IdResult {
    id: String,
}

/// `{ "id", "minutes" }` — returned when creating/updating time entries.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct IdMinutesResult {
    id: String,
    minutes: i64,
}

/// Single time entry as JSON (powers the edit-entry dialog).
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TimeEntryDetail {
    id: String,
    task_id: String,
    task_name: String,
    minutes: i64,
    note: String,
    started_at: String,
    /// Labels attached in the manual/edit dialogs (KanbanFlow parity).
    labels: Vec<String>,
}

/// One row of the paginated timer log.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TimerLogEntry {
    id: String,
    task_id: String,
    task_name: String,
    board_id: Option<String>,
    board_name: Option<String>,
    minutes: i64,
    /// KF-019: "31s" for sub-minute durations, "Nm" otherwise.
    duration_display: String,
    kind: String,
    badge_code: String,
    badge_title: String,
    interrupted: bool,
    interrupt_reason: Option<String>,
    note: String,
    /// YYYY-MM-DD day bucket for client-side day grouping.
    day_key: String,
    date: String,
    time_range: String,
    end: String,
    /// Labels attached in the manual-time / edit-entry dialogs
    /// (KanbanFlow parity, KF-102).
    labels: Vec<String>,
    /// Username that created the entry (avatar in the time log, KF-115).
    created_by: String,
}

/// Paginated timer log page.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TimerLogPage {
    entries: Vec<TimerLogEntry>,
    has_more: bool,
    total: usize,
}

/// One task's time within a day for the Time spent report.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TaskTime {
    task_id: String,
    task_name: String,
    minutes: i64,
    color_id: Option<String>,
}

/// One day's total for the Time spent report, with per-task breakdown.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct DayTotal {
    date: String,
    label: String,
    minutes: i64,
    /// Minutes from task entries (pomodoro/stopwatch/manual), excluding breaks.
    task_minutes: i64,
    /// Minutes from break entries (short_break/long_break).
    break_minutes: i64,
    tasks: Vec<TaskTime>,
}

/// Daily time totals for the Time spent report.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TimeSpentReport {
    days: Vec<DayTotal>,
    total_minutes: i64,
    /// Total task minutes across all days (excluding breaks).
    total_task_minutes: i64,
    /// Total break minutes across all days.
    total_break_minutes: i64,
}

/// Interruption count for one "Why did you stop?" reason.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct ReasonCount {
    reason: String,
    count: i64,
}

/// One day's pomodoro count for the statistics bar chart.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct DayPomodori {
    date: String,
    label: String,
    pomodori: i64,
}

/// Pomodoro statistics report.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TimerStatisticsReport {
    total_pomodori: i64,
    total_minutes: i64,
    avg_minutes: i64,
    interruptions: i64,
    by_reason: Vec<ReasonCount>,
    daily: Vec<DayPomodori>,
    /// Best single day in the requested range (Highscores tab).
    best_day: Option<DayPomodori>,
    /// Longest run of consecutive days with at least one pomodoro.
    longest_streak: i64,
}

/// Result of stopping the timer.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TimerStopResult {
    /// True when the session was under 20s and discarded.
    discarded: bool,
    /// Logged minutes, or None when discarded.
    minutes: Option<i64>,
    completed: bool,
}

/// Result of revoking a token.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct RevokeResult {
    revoked: String,
}

// ---- View models for templates ----

/// Task shaped for rendering: colors/labels resolved, dates formatted.
#[derive(Debug, Clone)]
struct TaskView {
    id: String,
    column_id: String,
    name: String,
    description: String,
    size_label: &'static str,
    /// Hour-based estimate label, e.g. "4h" (KanbanFlow parity, KF-216).
    /// None when no hour estimate is set; the modal then shows `size_label`.
    estimate_label: Option<String>,
    /// Resolved per-board color value, e.g. "yellow".
    color_value: String,
    /// Resolved per-board color label, e.g. "1 Pomodoro".
    color_label: String,
    /// True when the task's resolved color is not the board's default color
    /// (KF-257: the modal shows the color name text only in this case;
    /// tasks on the default color render the dot alone, per the contract's
    /// "no name shown for the no-color/default case").
    show_color_name: bool,
    color_bg: String,
    color_border: String,
    color_light: String,
    total_minutes: i64,
    /// Plain duration label for the modal's Time spent cell, KanbanFlow-style:
    /// "2h 30m" (space between hours and minutes), "43m" sub-hour, "0m" for
    /// zero (KF-258) — no pomodori count, no emoji, no Time log link.
    time_spent_label: String,
    completed_at: Option<String>,
    /// "Sep 28" style rendering of `completed_at`, for cards.
    completed_display: Option<String>,
    /// "Sep 28, 2026" style rendering of `created_at`, for the modal.
    created_display: String,
    /// "Sep 28" style rendering of `created_at`, for the card footer.
    created_day: String,
    /// Checklist subtasks (KanbanFlow parity).
    subtasks: Vec<Subtask>,
    /// Done subtask count, for the card footer ("2/5").
    subtasks_done: usize,
    /// Assigned member user ids (KanbanFlow parity).
    member_ids: Vec<String>,
    /// Assigned members resolved to username + initial, for card avatars.
    member_chips: Vec<MemberChip>,
    /// Labels (KanbanFlow parity), for the card footer.
    labels: Vec<String>,
    /// Labels serialized as a JSON array, rendered as the card's
    /// `data-labels` attribute so client-side label filtering (KF-165)
    /// can match cards even when the column hides the label chips.
    labels_json: String,
    /// "Sep 28" rendering of `due_at`, honoring the column's due-dates
    /// mode ("active_7d" hides far-future dues). None when hidden or unset.
    due_display: Option<String>,
    /// "Sep 28" rendering of `column_added_at` (falls back to created_at),
    /// for the card footer.
    added_display: String,
    /// Which task properties this task's column shows on cards (KF-043's
    /// "Task properties to display on board" config).
    display: TaskCardDisplay,
    /// Grouping-date override, if set ("Edit grouping date").
    grouping_date: Option<String>,
    /// Watch flag: task More menu "Watch" (KanbanFlow parity).
    watched: bool,
    /// Due date rendered for the modal and the card's full-date tooltip,
    /// e.g. "Sep 28, 2026 5:00 PM". Distinct from `due_display`, the
    /// short mode-honoring card rendering.
    due_full: Option<String>,
    /// KanbanFlow-style modal due-date rendering (KF-259), e.g.
    /// "Friday 5:00 PM" / "Friday 5:00 PM (Done)" / "30 October 5:00 PM".
    /// Same ±7-day weekday rule as `due_card`, with the " (Done)" marker
    /// when the due date was marked done. None when unset or unparseable.
    due_modal: Option<String>,
    /// True when the due date is in the past and the task is not done.
    due_overdue: bool,
    /// Due-date repeat text, e.g. "every week".
    due_repeat: Option<String>,
    /// Whether the due date was marked done (KanbanFlow parity: the card
    /// renders "(Done)" after the date).
    due_done: bool,
    /// KanbanFlow-style card due-date rendering, e.g. "Friday 5:00 PM"
    /// (near-term) or "30 October 5:00 PM" (farther out). None when unset
    /// or unparseable.
    due_card: Option<String>,
    /// KanbanFlow-style card time readout, e.g. "2h 30m / 8h" (spent /
    /// estimate), "0h / 4h", or "45m" (spent only). None when nothing to
    /// show.
    time_kf: Option<String>,
    /// Done count among the subtasks past the card's inline head (for
    /// the "N hidden (M done)" summary).
    subtasks_hidden_done: usize,
    /// Subtasks ordered for the card's inline list: undone first
    /// (KanbanFlow shows unchecked items in the visible head).
    subtasks_card: Vec<Subtask>,
    /// Raw RFC3339 due date, if set. Rendered only as `data-due-at` on the
    /// card so client-side date filters can evaluate it (KF-159).
    due_at: Option<String>,
    /// Task comments (KanbanFlow parity), oldest first.
    comments: Vec<CommentView>,
}

/// One comment shaped for templates.
#[derive(Debug, Clone)]
struct CommentView {
    id: String,
    author: String,
    body: String,
    /// "Today 3:03 PM" relative rendering of `created_at` (KF-254).
    created_display: String,
    /// Two-letter uppercase initials for the comment avatar circle (KF-254);
    /// KanbanFlow renders comment avatars uppercase ("CS", GM-070).
    author_initials: String,
}

/// One assigned member as shown on a card: username + avatar initial.
#[derive(Debug, Clone)]
struct MemberChip {
    username: String,
    initial: String,
}

/// Which task properties a column shows on its cards: KF-043's "Task
/// properties to display on board" config, stored as `prop_*` keys in the
/// column's config_json bag. Defaults mirror the dialog (all hide except
/// Due dates, which defaults to "Show active due in 7 days").
#[derive(Debug, Clone, Default)]
struct TaskCardDisplay {
    description: bool,
    labels: bool,
    subtasks: bool,
    due_dates: bool,
    /// Some(7) when the mode is "active_7d" (only near dues shown);
    /// None for plain "show".
    due_within_days: Option<i64>,
    created: bool,
    added: bool,
}

impl TaskCardDisplay {
    fn from_config_json(raw: &str) -> Self {
        let v: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
        let get = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
        let show = |key: &str| matches!(get(key), "show");
        let due_mode = get("prop_due_dates");
        Self {
            description: show("prop_description"),
            labels: show("prop_labels"),
            subtasks: show("prop_subtasks"),
            // Any non-"hide" value shows due dates; the default (empty) is
            // KanbanFlow's "Show active due in 7 days".
            due_dates: due_mode != "hide",
            due_within_days: if due_mode == "show" { None } else { Some(7) },
            created: show("prop_created"),
            added: show("prop_added"),
        }
    }
}

/// Resolved task color fields (per-board config or legacy fallback).
#[derive(Debug, Clone)]
struct ColorFields {
    value: String,
    label: String,
    bg: String,
    border: String,
    light: String,
}

impl From<&ColorRow> for ColorFields {
    fn from(color: &ColorRow) -> Self {
        Self {
            value: color.value.clone(),
            label: color.label.clone(),
            bg: color.background_hex.clone(),
            border: color.border_hex.clone(),
            light: color.light_hex.clone(),
        }
    }
}

/// Resolve a task's color fields. Explicit `color_id` wins; otherwise the
/// legacy size mapping applies, resolved through the board's configured
/// colors when present (so old rows pick up the board's exact palette).
/// The returned flag is true when the resolved color is NOT the board's
/// default color — only then does the modal show the color name (KF-257).
fn task_color_view(
    db: &Db,
    board_id: &str,
    task: &TaskRow,
) -> Result<(ColorFields, bool), AppError> {
    if let Some(color_id) = task.color_id.as_deref() {
        if let Some(color) = db.get_color(color_id).map_err(AppError::from)? {
            return Ok((ColorFields::from(&color), !color.is_default));
        }
    }
    let value = size_to_color_value(task.size);
    let size = Size::from_i64(task.size);
    if let Some(color) = db
        .list_colors(board_id)
        .map_err(AppError::from)?
        .into_iter()
        .find(|color| color.value == value)
    {
        return Ok((ColorFields::from(&color), !color.is_default));
    }
    // No color rows (shouldn't happen — list_colors backfills): fall back
    // to the fixed standard hex values with the legacy size label.
    let (bg, border, light, _) = standard_color(value).expect("known standard color");
    Ok((
        ColorFields {
            value: value.to_string(),
            label: size.label().to_string(),
            bg: bg.to_string(),
            border: border.to_string(),
            light: light.to_string(),
        },
        // No board colors at all: the size label is never a color name.
        false,
    ))
}

impl TaskView {
    fn from_row(row: &TaskRow) -> Self {
        let size = Size::from_i64(row.size);
        let value = size_to_color_value(row.size);
        let (bg, border, light, _) = standard_color(value).expect("known standard color");
        let done = row.completed_at.is_some();
        Self {
            id: row.id.clone(),
            column_id: row.column_id.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            size_label: size.label(),
            estimate_label: row.estimate_hours.map(format_estimate_hours),
            color_value: value.to_string(),
            color_label: size.label().to_string(),
            show_color_name: false,
            color_bg: bg.to_string(),
            color_border: border.to_string(),
            color_light: light.to_string(),
            total_minutes: row.total_minutes,
            time_spent_label: format_estimate_hours(row.total_minutes as f64 / 60.0),
            completed_at: row.completed_at.clone(),
            completed_display: row.completed_at.as_deref().map(format_day),
            created_display: DateTime::parse_from_rfc3339(&row.created_at)
                .map(|dt| dt.with_timezone(&Local).format("%b %d, %Y").to_string())
                .unwrap_or_else(|_| row.created_at.clone()),
            created_day: format_day(&row.created_at),
            subtasks: row.subtasks.clone(),
            subtasks_done: row.subtasks.iter().filter(|s| s.done).count(),
            subtasks_hidden_done: {
                let mut ordered: Vec<Subtask> = row.subtasks.clone();
                ordered.sort_by_key(|s| s.done);
                ordered.iter().skip(2).filter(|s| s.done).count()
            },
            subtasks_card: {
                let mut ordered: Vec<Subtask> = row.subtasks.clone();
                ordered.sort_by_key(|s| s.done);
                ordered
            },
            member_ids: row.member_ids.clone(),
            member_chips: Vec::new(),
            labels: row.labels.clone(),
            labels_json: serde_json::to_string(&row.labels).unwrap_or_else(|_| "[]".to_string()),
            due_display: None,
            added_display: format_added(row),
            display: TaskCardDisplay::default(),
            grouping_date: row.grouping_date.clone(),
            watched: row.watched,
            due_full: row.due_at.as_deref().map(format_datetime),
            due_modal: row
                .due_at
                .as_deref()
                .and_then(|d| format_due_modal(d, row.due_done)),
            due_overdue: is_overdue(row.due_at.as_deref(), done),
            due_repeat: row.due_repeat.clone(),
            due_done: row.due_done,
            due_card: row.due_at.as_deref().and_then(format_due_card),
            time_kf: format_time_kf(row.total_minutes, row.estimate_hours),
            due_at: row.due_at.clone(),
            // Column card-property config is resolved in `from_row_in_board`;
            // standalone rows keep the `TaskCardDisplay` defaults.
            comments: row
                .comments
                .iter()
                .map(|c| CommentView {
                    id: c.id.clone(),
                    author: c.author.clone(),
                    body: c.body.clone(),
                    created_display: format_comment_datetime(&c.created_at),
                    author_initials: user_initials(&c.author),
                })
                .collect(),
        }
    }

    /// Like `from_row`, but resolves the color fields through the board's
    /// color configuration (`task.color_id` first, legacy size mapping
    /// otherwise). Also resolves the column's per-property display flags
    /// (due dates / labels on cards, from the column config bag).
    fn from_row_in_board(db: &Db, board_id: &str, row: &TaskRow) -> Result<Self, AppError> {
        let mut view = Self::from_row(row);
        let (fields, show_name) = task_color_view(db, board_id, row)?;
        view.color_value = fields.value;
        view.color_label = fields.label;
        view.color_bg = fields.bg;
        view.color_border = fields.border;
        view.color_light = fields.light;
        view.show_color_name = show_name;
        // KF-053: per-column card property config + due-date mode come from
        // the task's column; member avatars resolve through the user list.
        if let Ok(Some(col)) = db.get_column(&row.column_id) {
            view.display = TaskCardDisplay::from_config_json(&col.config_json);
            if view.display.due_dates {
                view.due_display = row
                    .due_at
                    .as_deref()
                    .and_then(|d| format_due(d, view.display.due_within_days));
            }
        }
        if !row.member_ids.is_empty() {
            if let Ok(users) = db.list_users() {
                view.member_chips = users
                    .into_iter()
                    .filter(|u| row.member_ids.iter().any(|m| m == &u.id))
                    .map(|u| {
                        let initial = u
                            .username
                            .chars()
                            .next()
                            .map(|c| c.to_uppercase().to_string())
                            .unwrap_or_else(|| "?".to_string());
                        MemberChip {
                            username: u.username,
                            initial,
                        }
                    })
                    .collect();
            }
        }
        Ok(view)
    }
}

/// "Sep 28, 2026 1:25 PM" rendering of a stored RFC3339 timestamp (12h
/// clock, KanbanFlow parity for time ranges and history).
fn format_datetime(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%b %d, %Y %-I:%M %p")
                .to_string()
        })
        .unwrap_or_else(|_| rfc3339.to_string())
}

/// KF-254: "Today 3:03 PM" rendering of a stored RFC3339 timestamp for
/// comment timestamps (KanbanFlow parity): relative day label (Today /
/// Yesterday / "Sep 27") plus 12-hour time.
fn format_comment_datetime(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => {
            let local = dt.with_timezone(&Local);
            format!(
                "{} {}",
                day_label(local.date_naive()),
                local.format("%-I:%M %p")
            )
        }
        Err(_) => rfc3339.to_string(),
    }
}

/// "Sep 28" rendering of a stored RFC3339 timestamp.
fn format_day(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.with_timezone(&Local).format("%b %d").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

/// KF-255: the task-modal subline renders "Created: Today" when the task was
/// created on the same local calendar day (KanbanFlow GM-071); any other day
/// keeps the absolute `format_day` rendering. Scoped to the subline only —
/// `format_day` serves other surfaces (KF-053 card metadata, completed dates)
/// and stays absolute.
fn format_relative_day(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => {
            let local = dt.with_timezone(&Local);
            if local.date_naive() == Local::now().date_naive() {
                "Today".to_string()
            } else {
                local.format("%b %d").to_string()
            }
        }
        Err(_) => rfc3339.to_string(),
    }
}

/// KF-019: "31s" for sub-minute durations, "Nm" otherwise. `seconds` is the
/// precise elapsed seconds (0 for rows written before the field existed, in
/// which case we fall back to `minutes`).
fn format_entry_duration(seconds: i64, minutes: i64) -> String {
    if seconds > 0 && seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m")
    }
}

/// KF-029: two-letter uppercase initials for the avatar circle ("CS").
/// First letters of the first two whitespace-separated words, falling back
/// to the first two characters.
fn user_initials(username: &str) -> String {
    let mut words = username.split_whitespace();
    match (words.next(), words.next()) {
        (Some(first), Some(second)) => format!(
            "{}{}",
            first.chars().next().unwrap_or_default(),
            second.chars().next().unwrap_or_default()
        )
        .to_uppercase(),
        (Some(single), None) => single.chars().take(2).collect::<String>().to_uppercase(),
        _ => String::new(),
    }
}

/// KF-053: "Sep 28" rendering of when the task entered its current column,
/// falling back to the creation date for rows written before the field
/// existed.
fn format_added(row: &TaskRow) -> String {
    let stamp = row.column_added_at.as_deref().unwrap_or(&row.created_at);
    format_day(stamp)
}

/// KF-053: "Sep 28" rendering of a due date, honoring the column's
/// due-dates mode: `within_days` (Some(7) for KanbanFlow's "Show active due
/// in 7 days") hides dues more than that far in the future; overdue dues
/// stay visible. None when unparseable or out of range.
fn format_due(rfc3339: &str, within_days: Option<i64>) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(rfc3339)
        .ok()?
        .with_timezone(&Local);
    if let Some(days) = within_days {
        if dt > Local::now() + Duration::days(days) {
            return None;
        }
    }
    Some(dt.format("%b %d").to_string())
}

/// KF-224: KanbanFlow-style due-date rendering for the card's due line.
/// Near-term dates (within 7 days either way) render as a weekday —
/// "Friday 5:00 PM"; farther dates render absolute — "30 October 5:00 PM".
/// None when unparseable.
fn format_due_card(rfc3339: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(rfc3339)
        .ok()?
        .with_timezone(&Local);
    let now = Local::now();
    if dt > now - Duration::days(7) && dt < now + Duration::days(7) {
        Some(dt.format("%A %-I:%M %p").to_string())
    } else {
        Some(dt.format("%-d %B %-I:%M %p").to_string())
    }
}

/// KF-259: KanbanFlow-style due-date rendering for the task modal's
/// DUE DATE row: the same ±7-day weekday rule as `format_due_card`, with
/// " (Done)" appended verbatim when the due date was marked done.
/// None when unparseable (shared `format_datetime` untouched).
fn format_due_modal(rfc3339: &str, due_done: bool) -> Option<String> {
    let mut text = format_due_card(rfc3339)?;
    if due_done {
        text.push_str(" (Done)");
    }
    Some(text)
}

/// KF-224: KanbanFlow-style card time readout — "{spent} / {estimate}"
/// (e.g. "2h 30m / 8h", "0h / 4h"), or just "{spent}" (e.g. "45m") when no
/// estimate is set. None when there is no logged time and no estimate.
fn format_time_kf(total_minutes: i64, estimate_hours: Option<f64>) -> Option<String> {
    let estimate = estimate_hours.map(format_estimate_hours);
    let spent = if total_minutes <= 0 {
        "0h".to_string()
    } else {
        format_estimate_hours(total_minutes as f64 / 60.0)
    };
    match (total_minutes > 0, estimate) {
        (false, None) => None,
        (_, Some(e)) => Some(format!("{spent} / {e}")),
        (true, None) => Some(spent),
    }
}

/// Grouping label for completed tasks: Today / Yesterday / "Friday, 10 July".
fn done_group_label(rfc3339: &str) -> String {
    let today = Local::now().date_naive();
    match DateTime::parse_from_rfc3339(rfc3339).map(|dt| dt.with_timezone(&Local).date_naive()) {
        Ok(date) if date == today => "Today".to_string(),
        Ok(date) if date == today - Duration::days(1) => "Yesterday".to_string(),
        Ok(date) => date.format("%A, %-d %B").to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

/// One column x swimlane cell on the board.
#[derive(Debug, Clone)]
struct CellView {
    column_id: String,
    swimlane_id: String,
    is_done: bool,
    /// Tasks for normal columns.
    tasks: Vec<TaskView>,
    /// Tasks for the Done column, grouped under date headers.
    done_groups: Vec<DoneGroup>,
}

#[derive(Debug, Clone)]
struct DoneGroup {
    label: String,
    tasks: Vec<TaskView>,
}

/// One swimlane band: its header plus one cell per column.
#[derive(Debug, Clone)]
struct BandView {
    id: String,
    name: String,
    task_count: usize,
    cells: Vec<CellView>,
}

/// Column header data: id, name, task count, optional WIP limit, done flag.
#[derive(Debug, Clone)]
struct ColumnHead {
    id: String,
    name: String,
    description: String,
    collapsed: bool,
    wip_limit: Option<i64>,
    count: usize,
    over_limit: bool,
    /// KF-223: tasks in this column whose due timestamp has passed and that
    /// are not completed (same rule as TaskView::due_overdue), for the
    /// collapsed-strip "N overdue task(s)" indicator.
    overdue_count: usize,
    is_done: bool,
    /// Opaque column-dialog settings bag, for the Edit column dialog.
    config_json: String,
}

/// One color slot as JSON (also used by the color-admin page template).
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct ColorView {
    id: String,
    /// Standard color value, e.g. "yellow".
    value: String,
    /// Standard color display name, e.g. "Yellow" (KF-087: shown inside the swatch).
    standard_name: String,
    label: String,
    description: String,
    enabled: bool,
    is_default: bool,
    sort_order: i64,
    bg: String,
    border: String,
    light: String,
}

impl ColorView {
    /// KF-217 / KF-222: the color's display name — the board-configured
    /// custom name when set, falling back to the fixed standard palette
    /// name ("Yellow", "Green", …) when the custom label is empty.
    /// KanbanFlow's filter Color section lists the palette's fixed names
    /// unless renamed on the board, so the filter dropdown and the legend
    /// footer share this.
    fn display_label(&self) -> &str {
        if self.label.is_empty() {
            &self.standard_name
        } else {
            &self.label
        }
    }

    /// KF-335: whether this color is one of the 4 pomodoro-count legend
    /// segments (KanbanFlow parity). The legend shows only yellow/green/
    /// blue/red, not all board colors.
    fn is_pomodoro_legend_color(&self) -> bool {
        matches!(self.value.as_str(), "yellow" | "green" | "blue" | "red")
    }

    /// KF-335: the background color of this color's legend segment.
    /// Measured from GM-139 golden master (exact KanbanFlow values).
    fn legend_bg(&self) -> &str {
        match self.value.as_str() {
            "yellow" => "#ffffe2",
            "green" => "#e1fec7",
            "blue" => "#d1e1fd",
            "red" => "#f7cdd1",
            _ => self.bg.as_str(),
        }
    }

    /// KF-249 / KF-335: the 1px top edge color of this color's color-legend
    /// segment. GM-139 shows a uniform gray (#cbcbcb) top edge across all
    /// four pomodoro segments (not saturated per-color edges).
    fn legend_edge(&self) -> &str {
        "#cbcbcb"
    }
}

impl From<&ColorRow> for ColorView {
    fn from(color: &ColorRow) -> Self {
        Self {
            id: color.id.clone(),
            value: color.value.clone(),
            standard_name: crate::models::STANDARD_COLORS
                .iter()
                .find(|(v, _, _, _, _)| *v == color.value)
                .map(|(_, _, _, _, label)| label.to_string())
                .unwrap_or_else(|| color.value.clone()),
            label: color.label.clone(),
            description: color.description.clone(),
            enabled: color.enabled,
            is_default: color.is_default,
            sort_order: color.sort_order,
            bg: color.background_hex.clone(),
            border: color.border_hex.clone(),
            light: color.light_hex.clone(),
        }
    }
}

/// `{ "id", "name", "archived" }` — one board in `GET /api/boards` and the new-board page.
/// KF-305: `archived` indicates the board is archived (hidden from default lists).
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct BoardListItem {
    id: String,
    name: String,
    /// Whether the board is archived (KF-305). Archived boards are excluded
    /// from default listings; pass `?include_archived=true` to include them.
    archived: bool,
}

/// One board column in `GET /api/boards/:id/columns`:
/// `{ "id", "name", "wip_limit", "is_done" }`
/// (Move-task dialog board switcher, KF-070; KF-221c).
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct ColumnListItem {
    id: String,
    name: String,
    /// WIP limit; `None` when unset (compatible extension — new field).
    wip_limit: Option<i64>,
    /// Whether tasks moved here count as Done.
    is_done: bool,
}

/// `{ "id", "name", "description", "built_in" }` — one board template.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TemplateListItem {
    id: String,
    name: String,
    description: String,
    built_in: bool,
}

#[derive(Template)]
#[template(path = "board.html")]
struct BoardTemplate {
    board_id: String,
    board_name: String,
    username: String,
    /// Two-letter uppercase initials — avatar circles (KF-029).
    username_initial: String,
    columns: Vec<ColumnHead>,
    bands: Vec<BandView>,
    /// KF-152: true when the board has exactly one swimlane named "Default".
    /// The swimlane still exists in the DB (KF-149 needs it so column "+"
    /// buttons work); only its header row is hidden, matching KanbanFlow's
    /// free tier which renders no swimlane row.
    hide_swimlane_header: bool,
    /// Enabled colors, ordered for the task color picker / legend.
    /// Also drives the filter panel's Color section (KF-134).
    colors: Vec<ColorView>,
    /// KF-183: whether the color legend bar is shown. KanbanFlow shows no
    /// legend by default; the Menu toggles it per board (persisted in the
    /// board's config bag).
    legend_visible: bool,
    /// Standard value of the board's default color, e.g. "yellow".
    default_color_value: String,
    /// All boards for the Boards dropdown menu (KF-088, KF-330).
    boards: Vec<BoardListItem>,
}

#[derive(Template)]
#[template(path = "new_board.html")]
struct NewBoardTemplate {
    boards: Vec<BoardListItem>,
    templates: Vec<TemplateListItem>,
}

/// KF-305: archived boards list page.
#[derive(Template)]
#[template(path = "archived_boards.html")]
struct ArchivedBoardsTemplate {
    boards: Vec<BoardListItem>,
}

#[derive(Template)]
#[template(path = "task_card.html")]
struct TaskCardTemplate {
    task: TaskView,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: bool,
}

#[derive(Debug, Clone)]
struct TimeEntryView {
    id: String,
    minutes: i64,
    /// KF-019: "31s" for sub-minute durations, "Nm" otherwise.
    duration_display: String,
    note: String,
    started_display: String,
    /// "P" | "M" | "S" badge (v2-00835).
    badge_code: String,
    /// Tooltip: "Pomodoro" / "Stopped with reason 'X'" / "Manually added" / "Stopwatch".
    badge_title: String,
    interrupted: bool,
    interrupt_reason: Option<String>,
    /// "1:25 PM - 1:55 PM" From–To range in 12h (KanbanFlow parity,
    /// KF-115).
    range_display: String,
    /// Username that logged the entry (avatar + name in the log view).
    member_name: String,
    /// First letter of `member_name`, for the avatar dot.
    member_initial: String,
    /// Labels attached in the manual/edit dialogs (KanbanFlow parity).
    labels: Vec<String>,
}

/// One day group in the dedicated time-log sub-view: "Today 2m",
/// "Yesterday 40m", or "Sep 27" style (KanbanFlow parity, KF-101).
#[derive(Debug, Clone)]
struct DayGroup {
    label: String,
    total_minutes: i64,
    entries: Vec<TimeEntryView>,
}

/// "Today" / "Yesterday" / "Sep 27" label for an entry's local date.
fn day_label(date: chrono::NaiveDate) -> String {
    let today = Local::now().date_naive();
    if date == today {
        "Today".to_string()
    } else if date == today - chrono::Duration::days(1) {
        "Yesterday".to_string()
    } else {
        date.format("%b %-d").to_string()
    }
}

/// Group newest-first entry views into day buckets (newest day first),
/// each carrying its summed minutes for the "Today 2m" header.
fn group_entries_by_day(
    entries: Vec<TimeEntryView>,
    dates: Vec<chrono::NaiveDate>,
) -> Vec<DayGroup> {
    let mut groups: Vec<DayGroup> = Vec::new();
    for (entry, date) in entries.into_iter().zip(dates) {
        let label = day_label(date);
        match groups.last_mut() {
            Some(group) if group.label == label => {
                group.total_minutes += entry.minutes;
                group.entries.push(entry);
            }
            _ => groups.push(DayGroup {
                label,
                total_minutes: entry.minutes,
                entries: vec![entry],
            }),
        }
    }
    groups
}

#[derive(Template)]
#[template(path = "time_entries.html")]
struct TimeEntriesTemplate {
    entries: Vec<TimeEntryView>,
}

#[derive(Template)]
#[template(path = "timer_log.html")]
struct TimerLogTemplate {}

#[derive(Template)]
#[template(path = "timer_statistics.html")]
struct TimerStatisticsTemplate {
    boards: Vec<serde_json::Value>,
}

#[derive(Template)]
#[template(path = "modal.html")]
struct ModalTemplate {
    task: TaskView,
    is_done: bool,
    /// Members currently assigned to the task, for the modal body row.
    assigned_members: Vec<MemberView>,
    /// Name of the task's current column (for the modal subtitle, KF-057).
    column_name: String,
    /// "Jun 23" rendering of the task's created date (KF-057).
    created_short: String,
    /// Two-letter uppercase initials of the signed-in user, for the
    /// comment composer avatar circle (KF-254).
    composer_initials: String,
}

/// One history event shaped for the History sub-view (KanbanFlow parity,
/// KF-100). Newest first.
#[derive(Debug, Clone)]
struct HistoryEventView {
    kind: String,
    detail: String,
    actor: String,
    /// "Sep 28, 2026 1:25 PM" rendering of `created_at`.
    created_display: String,
}

#[derive(Template)]
#[template(path = "history_view.html")]
struct HistoryViewTemplate {
    task_id: String,
    task_name: String,
    events: Vec<HistoryEventView>,
}

#[derive(Template)]
#[template(path = "time_log_view.html")]
struct TimeLogViewTemplate {
    task_id: String,
    task_name: String,
    total_minutes: i64,
    groups: Vec<DayGroup>,
}

/// One board member as shown in the modal body row.
#[derive(Debug, Clone)]
struct MemberView {
    username: String,
    initials: String,
}

// ---- Small DB helpers ----

/// Fetch one task with its total logged minutes, shaped for templates.
/// The color fields resolve through the task's board color configuration.
fn fetch_task_view(db: &Db, id: &str) -> Result<Option<TaskView>, AppError> {
    let row = db.get_task_with_minutes(id).map_err(AppError::from)?;
    match row {
        Some(row) => {
            let board_id = db
                .get_column(&row.column_id)
                .map_err(AppError::from)?
                .map(|column| column.board_id);
            match board_id {
                Some(board_id) => Ok(Some(TaskView::from_row_in_board(db, &board_id, &row)?)),
                None => Ok(Some(TaskView::from_row(&row))),
            }
        }
        None => Ok(None),
    }
}

/// Time entries for a task, newest first, with display-ready timestamps.
/// `fallback_user` names the entry creator when the row predates
/// `created_by` recording.
fn fetch_entries(
    db: &Db,
    task_id: &str,
    fallback_user: &str,
) -> Result<Vec<TimeEntryView>, AppError> {
    let rows = db.list_entries(task_id).map_err(AppError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            // KF-015/KF-016: P = "Pomodoro" (or "Stopped with reason 'X'"),
            // M = "Manually added" (v3-01594; frame review).
            let badge_code = match row.kind.as_str() {
                "pomodoro" => "P",
                "stopwatch" => "S",
                _ => "M",
            };
            let badge_title = match row.kind.as_str() {
                "pomodoro" => match &row.interrupt_reason {
                    Some(reason) => format!("Stopped with reason '{reason}'"),
                    None => "Pomodoro".to_string(),
                },
                "stopwatch" => "Stopwatch".to_string(),
                _ => "Manually added".to_string(),
            };
            let local_start =
                DateTime::parse_from_rfc3339(&row.started_at).map(|dt| dt.with_timezone(&Local));
            let started_display = local_start
                .as_ref()
                .map(|dt| dt.format("%b %d, %Y %H:%M").to_string())
                .unwrap_or_else(|_| row.started_at.clone());
            // KF-115: the From–To range in 12h, derived server-side from
            // started_at + minutes.
            let range_display = local_start
                .as_ref()
                .map(|start| {
                    let end = *start + chrono::Duration::minutes(row.minutes);
                    format!(
                        "{} - {}",
                        start.format("%-I:%M %p"),
                        end.format("%-I:%M %p")
                    )
                })
                .unwrap_or_default();
            let member_name = if row.created_by.is_empty() {
                fallback_user.to_string()
            } else {
                row.created_by.clone()
            };
            let member_initial = member_name.chars().next().unwrap_or('?').to_string();
            TimeEntryView {
                id: row.id.clone(),
                minutes: row.minutes,
                duration_display: format_entry_duration(row.seconds, row.minutes),
                note: row.note.clone(),
                started_display,
                badge_code: badge_code.to_string(),
                badge_title: badge_title.to_string(),
                interrupted: row.interrupted,
                interrupt_reason: row.interrupt_reason.clone(),
                range_display,
                member_name,
                member_initial,
                labels: row.labels.clone(),
            }
        })
        .collect())
}

/// Local dates (parallel to a [`fetch_entries`] result) for day grouping.
fn entry_dates(db: &Db, task_id: &str) -> Result<Vec<chrono::NaiveDate>, AppError> {
    let rows = db.list_entries(task_id).map_err(AppError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            DateTime::parse_from_rfc3339(&row.started_at)
                .map(|dt| dt.with_timezone(&Local).date_naive())
                .unwrap_or_else(|_| Local::now().date_naive())
        })
        .collect())
}

/// First registered username, used as the entry-creator fallback for rows
/// that predate `created_by` recording.
fn fallback_username(db: &Db) -> String {
    db.list_users()
        .ok()
        .and_then(|users| users.into_iter().next())
        .map(|u| u.username)
        .unwrap_or_else(|| "admin".to_string())
}

/// Append an activity-trail event (KanbanFlow parity: History, KF-100).
/// History must never break the operation it annotates, so failures are
/// swallowed here.
fn log_history(db: &Db, task_id: &str, kind: &str, detail: &str, actor: &str) {
    let _ = db.log_event(task_id, kind, detail, actor);
}

// ---- Page handlers ----

/// Redirect to the first board.
async fn root(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Response, AppError> {
    let id: Option<String> = state.db.first_board_id().map_err(AppError::from)?;
    match id {
        Some(id) => Ok(Redirect::to(&format!("/b/{id}")).into_response()),
        None => Err(AppError::internal("no boards found")),
    }
}

/// Full board page: columns with counts/WIP usage, swimlane bands, task cells.
async fn board_page(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<BoardTemplate, AppError> {
    let db = &state.db;

    let board = db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;

    let columns = db.list_columns(&board_id).map_err(AppError::from)?;

    let swimlanes = db.list_swimlanes(&board_id).map_err(AppError::from)?;

    // One query for every task on the board, ordered for rendering.
    let tasks = db.board_tasks(&board_id).map_err(AppError::from)?;

    let column_heads: Vec<ColumnHead> = columns
        .iter()
        .map(|col| {
            let count = tasks.iter().filter(|t| t.column_id == col.id).count();
            // KF-035: the warning is a *violation* — it fires only when the
            // count exceeds the limit, not when it merely reaches it.
            let over_limit = col
                .wip_limit
                .map(|limit| count as i64 > limit)
                .unwrap_or(false);
            // KF-223: overdue tasks in this column (same rule as
            // TaskView::due_overdue) for the collapsed-strip indicator.
            let now = Local::now();
            let overdue_count = tasks
                .iter()
                .filter(|t| {
                    t.column_id == col.id
                        && t.completed_at.is_none()
                        && t.due_at
                            .as_deref()
                            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                            .map(|dt| dt.with_timezone(&Local) < now)
                            .unwrap_or(false)
                })
                .count();
            ColumnHead {
                id: col.id.clone(),
                name: col.name.clone(),
                description: col.description.clone(),
                collapsed: col.collapsed,
                wip_limit: col.wip_limit,
                count,
                over_limit,
                overdue_count,
                is_done: col.is_done,
                config_json: col.config_json.clone(),
            }
        })
        .collect();

    let mut bands = Vec::new();
    for lane in &swimlanes {
        let mut cells = Vec::new();
        for col in &columns {
            let is_done = col.is_done;
            let cell_tasks: Vec<TaskView> = tasks
                .iter()
                .filter(|t| {
                    t.column_id == col.id && t.swimlane_id.as_deref() == Some(lane.id.as_str())
                })
                .map(|task| TaskView::from_row_in_board(db, &board_id, task))
                .collect::<Result<Vec<_>, _>>()?;

            // Done-column tasks group under Today / Yesterday / date headers.
            let mut done_groups: Vec<DoneGroup> = Vec::new();
            if is_done {
                for task in &cell_tasks {
                    // Ensure we get a proper label, even with edge cases
                    let label = match &task.completed_at {
                        Some(completed_at) => {
                            // Use done_group_label but ensure it returns a meaningful label
                            let result = done_group_label(completed_at);
                            if result == *completed_at {
                                // If the result is just the raw timestamp, fallback to "Completed"
                                "Completed".to_string()
                            } else {
                                result
                            }
                        }
                        None => "Completed".to_string(),
                    };
                    match done_groups.last_mut() {
                        Some(group) if group.label == label => group.tasks.push(task.clone()),
                        _ => done_groups.push(DoneGroup {
                            label,
                            tasks: vec![task.clone()],
                        }),
                    }
                }
            }

            cells.push(CellView {
                column_id: col.id.clone(),
                swimlane_id: lane.id.clone(),
                is_done,
                tasks: if is_done { Vec::new() } else { cell_tasks },
                done_groups,
            });
        }
        let lane_task_count = tasks
            .iter()
            .filter(|t| t.swimlane_id.as_deref() == Some(lane.id.as_str()))
            .count();
        bands.push(BandView {
            id: lane.id.clone(),
            name: lane.name.clone(),
            task_count: lane_task_count,
            cells,
        });
    }

    // KF-152: hide the swimlane header row when the board has only the
    // structural "Default" swimlane (it must still exist for KF-149).
    let hide_swimlane_header = bands.len() == 1 && bands[0].name == "Default";

    // KF-183: the color legend is opt-in per board (KanbanFlow shows no
    // legend by default); read before `board.name` is moved below.
    let legend_visible = board.config_bool("legend_visible");

    Ok(BoardTemplate {
        board_id: board.id.clone(),
        board_name: board.name,
        username_initial: user_initials(&user.username),
        username: user.username,
        columns: column_heads,
        bands,
        hide_swimlane_header,
        legend_visible,
        colors: db
            .list_colors(&board_id)
            .map_err(AppError::from)?
            .iter()
            .filter(|color| color.enabled)
            .map(ColorView::from)
            .collect(),
        default_color_value: db
            .default_color(&board_id)
            .map_err(AppError::from)?
            .map(|color| color.value)
            .unwrap_or_default(),
        boards: db
            .list_boards()
            .map_err(AppError::from)?
            .into_iter()
            .filter(|b| !b.config_bool("archived"))
            .map(|b| BoardListItem {
                id: b.id.clone(),
                name: b.name.clone(),
                archived: false,
            })
            .collect(),
    })
}

// ---- Auth pages ----

async fn login_page(Query(query): Query<HashMap<String, String>>) -> impl IntoResponse {
    LoginTemplate {
        error: query.contains_key("error"),
    }
}

#[derive(Deserialize)]
struct LoginForm {
    user: String,
    pass: String,
}

async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    let row = state
        .db
        .user_by_username(form.user.trim())
        .map_err(AppError::from)?;

    let authenticated = row
        .as_ref()
        .map(|user| auth::verify_password(&user.password_hash, &form.pass))
        .unwrap_or(false);
    if !authenticated {
        return Ok(Redirect::to("/login?error=1").into_response());
    }

    let token =
        auth::create_session(&state.db, &row.expect("checked above").id).map_err(AppError::from)?;
    // KF-221d: `Secure` only when the request actually arrived over HTTPS;
    // a `Secure` cookie sent over plain HTTP is dropped by the browser.
    let secure = auth::request_is_https(&headers);
    Ok(auth::redirect_with_cookie(
        "/",
        &auth::session_cookie_secure(&token, secure),
    ))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = auth::session_token_from_headers(&headers) {
        let _ = state.db.delete_session(&token);
    }
    auth::redirect_with_cookie("/login", &auth::clear_session_cookie())
}

// ---- Task API ----

#[derive(Deserialize, ToSchema)]
struct CreateTaskInput {
    column_id: String,
    swimlane_id: Option<String>,
    name: String,
    size: Option<i64>,
    /// Color slot id. When absent: `size` (when given) keeps the legacy
    /// size-based coloring; otherwise the task gets the board's default
    /// color.
    color_id: Option<String>,
    /// Hour-based time estimate, e.g. 4.0 for "4h" (KanbanFlow parity,
    /// KF-216). Positive values set the estimate; the pomodoro `size` is
    /// derived from it unless `size` is also given.
    estimate_hours: Option<f64>,
    /// Due date/time as RFC3339 (or "YYYY-MM-DD HH:MM" in the server's
    /// local timezone; a bare "YYYY-MM-DD" means end of that day).
    /// Absent, null, or empty means no due date (KF-226).
    due_at: Option<String>,
    /// Due-date repeat text, e.g. "every week" (KF-226).
    due_repeat: Option<String>,
}

/// All tasks as id/name pairs.
/// Task name list for the manual-time / edit-entry task autocomplete.
#[utoipa::path(
    get,
    path = "/api/tasks",
    tag = "Tasks",
    responses(
        (status = 200, description = "All tasks as id/name pairs", body = Vec<TaskNameItem>),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_tasks(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Vec<TaskNameItem>>, AppError> {
    let tasks = state
        .db
        .all_tasks()
        .map_err(AppError::from)?
        .into_iter()
        .map(|t| TaskNameItem {
            id: t.id,
            name: t.name,
        })
        .collect();
    Ok(Json(tasks))
}

/// Create a task. With `Accept: application/json` this returns a 201 JSON
/// body carrying the created task's id (KF-221a); without it, the
/// rendered card fragment (for htmx appends).
#[utoipa::path(
    post,
    path = "/api/tasks",
    tag = "Tasks",
    request_body = CreateTaskInput,
    responses(
        (status = 201, description = "Created task (JSON request)", body = CreateTaskResponse),
        (status = 200, description = "Rendered task card HTML fragment", content_type = "text/html"),
        (status = 400, description = "Invalid input: empty name or unknown column"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: CreateTaskInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("task name is required"));
    }

    let column = db
        .get_column(&input.column_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::bad_request("unknown column"))?;

    // Default to the board's first swimlane when none is given.
    let swimlane_id = match input.swimlane_id.filter(|s| !s.is_empty()) {
        Some(id) => Some(id),
        None => db
            .list_swimlanes(&column.board_id)
            .map_err(AppError::from)?
            .into_iter()
            .next()
            .map(|lane| lane.id),
    };

    let size = Size::from_i64(input.size.unwrap_or(1));
    // Color: explicit color_id wins (validated against the board);
    // explicit size keeps the legacy size-based coloring; otherwise the
    // task gets the board's default color.
    let (size, color_id) = match input.color_id.filter(|s| !s.is_empty()) {
        Some(color_id) => {
            let color = db
                .get_color(&color_id)
                .map_err(AppError::from)?
                .filter(|color| color.board_id == column.board_id)
                .ok_or_else(|| AppError::bad_request("unknown color"))?;
            (color_value_to_size(&color.value), Some(color.id))
        }
        None => match input.size {
            Some(_) => (size as i64, None),
            None => match db.default_color(&column.board_id).map_err(AppError::from)? {
                Some(color) => (color_value_to_size(&color.value), Some(color.id)),
                None => (1, None),
            },
        },
    };
    let id = db
        .create_task(
            &column.id,
            swimlane_id.as_deref(),
            &name,
            size,
            color_id.as_deref(),
        )
        .map_err(AppError::from)?;

    // KF-216: hour-based estimate. When given without an explicit size,
    // derive the pomodoro size from it (the color above already resolved).
    if let Some(raw_hours) = input.estimate_hours {
        let hours = normalize_estimate_hours(raw_hours).map_err(AppError::bad_request)?;
        db.set_task_estimate(&id, hours).map_err(AppError::from)?;
        if input.size.is_none() {
            if let Some(h) = hours {
                let pomodoro_minutes = db.get_settings().map(|s| s.pomodoro_minutes).unwrap_or(25);
                let derived = estimate_hours_to_size(h, pomodoro_minutes);
                db.update_task(&id, None, None, Some(derived), None)
                    .map_err(AppError::from)?;
            }
        }
    }

    // KF-226: optional due date at creation (same format rules as the
    // PATCH path; absent, null, or blank means no due date).
    if input.due_at.is_some() || input.due_repeat.is_some() {
        let due_at = input
            .due_at
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(normalize_due_input);
        let due_repeat = input
            .due_repeat
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        db.set_task_due(&id, due_at.as_deref(), due_repeat)
            .map_err(AppError::from)?;
    }

    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::internal("task vanished after create"))?;
    log_history(
        db,
        &id,
        "created",
        &format!("Task created in {}", column.name),
        &user.username,
    );
    // KF-221a: API callers that ask for JSON get a clean 201 body with the
    // id; the HTML card fragment path stays for the htmx UI.
    if accepts_json(&headers) {
        let response = CreateTaskResponse {
            id: task.id.clone(),
            name: task.name.clone(),
            column_id: task.column_id.clone(),
            swimlane_id: task.swimlane_id.clone(),
        };
        Ok((StatusCode::CREATED, Json(response)).into_response())
    } else {
        Ok(TaskCardTemplate {
            task: TaskView::from_row_in_board(db, &column.board_id, &task)?,
        }
        .into_response())
    }
}

/// Serde helper for `Option<Option<T>>` PATCH fields: stock serde
/// collapses an explicit JSON `null` into the same `None` as a missing
/// field, so without this, `null` can never mean "clear". Missing →
/// `None` (outer), `null` → `Some(None)`, a value → `Some(Some(v))`
/// (KF-227, KF-228).
fn de_opt_opt<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

#[derive(Deserialize, ToSchema)]
struct UpdateTaskInput {
    name: Option<String>,
    description: Option<String>,
    size: Option<i64>,
    /// Color slot id; empty string clears the assignment (back to the
    /// legacy size-based coloring). Absent leaves it unchanged.
    color_id: Option<String>,
    /// Replace the assigned member user ids. Absent leaves them unchanged.
    member_ids: Option<Vec<String>>,
    /// Grouping-date override ("Edit grouping date"); empty string clears
    /// it. Absent leaves it unchanged.
    grouping_date: Option<String>,
    /// Replace the task's labels (KanbanFlow parity). Absent leaves them
    /// unchanged.
    labels: Option<Vec<String>>,
    /// Due date/time as RFC3339 (or "YYYY-MM-DD HH:MM"). Explicit JSON
    /// null (`Some(None)`) clears it, and an empty string clears it too;
    /// absent leaves it unchanged (KF-227).
    #[serde(default, deserialize_with = "de_opt_opt")]
    due_at: Option<Option<String>>,
    /// Due-date repeat text, e.g. "every week". Explicit JSON null
    /// clears it, and an empty string clears it too; absent leaves it
    /// unchanged (KF-227).
    #[serde(default, deserialize_with = "de_opt_opt")]
    due_repeat: Option<Option<String>>,
    /// Mark the due date done/undone (KanbanFlow parity: checking the
    /// due-date item; the card renders "(Done)"). Absent leaves it
    /// unchanged.
    due_done: Option<bool>,
    /// Hour-based time estimate in hours (KanbanFlow parity, KF-216).
    /// Positive values set it (deriving `size` unless `size` is also
    /// given); exactly 0 clears it. Absent leaves it unchanged.
    estimate_hours: Option<f64>,
}

/// Patch name/description/size; returns the refreshed card fragment.
#[utoipa::path(
    patch,
    path = "/api/tasks/{id}",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    request_body = UpdateTaskInput,
    responses(
        (status = 200, description = "Refreshed task card HTML fragment", content_type = "text/html"),
        (status = 400, description = "Invalid input"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<TaskCardTemplate, AppError> {
    let input: UpdateTaskInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let existing = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;

    if let Some(name) = input.name.as_deref() {
        if name.trim().is_empty() {
            return Err(AppError::bad_request("task name is required"));
        }
    }

    // Color assignment: validated against the task's board. Setting a
    // color also derives `size` from it unless `size` is passed explicitly.
    let board_id = db
        .get_column(&existing.column_id)
        .map_err(AppError::from)?
        .map(|column| column.board_id);
    let mut color_id: Option<Option<String>> = None;
    let mut size = input.size.map(|size| Size::from_i64(size) as i64);
    if let Some(requested) = input.color_id.as_deref() {
        if requested.is_empty() {
            color_id = Some(None);
        } else {
            let color = db
                .get_color(requested)
                .map_err(AppError::from)?
                .filter(|color| Some(color.board_id.as_str()) == board_id.as_deref())
                .ok_or_else(|| AppError::bad_request("unknown color"))?;
            if size.is_none() {
                size = Some(color_value_to_size(&color.value));
            }
            color_id = Some(Some(color.id));
        }
    }
    db.update_task(
        &id,
        input.name.as_deref().map(str::trim),
        input.description.as_deref(),
        size,
        color_id.as_ref().map(|option| option.as_deref()),
    )
    .map_err(AppError::from)?;

    // KF-216: hour-based estimate. Setting it also derives the pomodoro
    // size unless `size` was given explicitly in the same request.
    if let Some(raw_hours) = input.estimate_hours {
        let hours = normalize_estimate_hours(raw_hours).map_err(AppError::bad_request)?;
        db.set_task_estimate(&id, hours).map_err(AppError::from)?;
        if size.is_none() {
            if let Some(h) = hours {
                let pomodoro_minutes = db.get_settings().map(|s| s.pomodoro_minutes).unwrap_or(25);
                let derived = estimate_hours_to_size(h, pomodoro_minutes);
                db.update_task(&id, None, None, Some(derived), None)
                    .map_err(AppError::from)?;
            }
        }
    }

    if let Some(member_ids) = input.member_ids.as_deref() {
        db.set_task_members(&id, member_ids)
            .map_err(AppError::from)?;
    }
    if let Some(grouping_date) = input.grouping_date.as_deref() {
        let date = grouping_date.trim();
        db.set_grouping_date(&id, if date.is_empty() { None } else { Some(date) })
            .map_err(AppError::from)?;
    }
    if let Some(labels) = input.labels.as_deref() {
        db.set_task_labels(&id, labels).map_err(AppError::from)?;
    }
    if input.due_at.is_some() || input.due_repeat.is_some() {
        // Explicit JSON null clears, empty strings still clear (legacy),
        // and absent leaves the field unchanged (KF-227).
        let due_at = match input.due_at.as_ref().map(|inner| inner.as_deref()) {
            Some(Some(s)) if s.trim().is_empty() => None,
            Some(Some(s)) => Some(normalize_due_input(s)),
            Some(None) => None,
            None => existing.due_at.clone(),
        };
        let due_repeat = match input.due_repeat.as_ref().map(|inner| inner.as_deref()) {
            Some(Some(s)) if s.trim().is_empty() => None,
            Some(Some(s)) => Some(s.trim().to_string()),
            Some(None) => None,
            None => existing.due_repeat.clone(),
        };
        db.set_task_due(&id, due_at.as_deref(), due_repeat.as_deref())
            .map_err(AppError::from)?;
        // KF-224: a changed due date is not done — the done flag belongs
        // to the old date (KanbanFlow's checked due-date item).
        if due_at != existing.due_at {
            db.set_task_due_done(&id, false).map_err(AppError::from)?;
        }
    }
    // KF-224: explicit due-date done flag. Applied after the due block so
    // an explicit value wins over the change-reset above.
    if let Some(due_done) = input.due_done {
        db.set_task_due_done(&id, due_done)
            .map_err(AppError::from)?;
    }

    // History (KF-100): one event per changed field, diffed against the
    // pre-update snapshot.
    let updated = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if let Some(name) = input.name.as_deref() {
        let name = name.trim();
        if name != existing.name {
            log_history(
                db,
                &id,
                "renamed",
                &format!("Renamed to \"{name}\""),
                &user.username,
            );
        }
    }
    if let Some(description) = input.description.as_deref() {
        if description != existing.description {
            let detail = if existing.description.is_empty() {
                "Added a description"
            } else if description.is_empty() {
                "Cleared the description"
            } else {
                "Updated the description"
            };
            log_history(db, &id, "description_updated", detail, &user.username);
        }
    }
    if updated.estimate_hours != existing.estimate_hours {
        let detail = match updated.estimate_hours {
            Some(h) => format!("Changed the time estimate to {}", format_estimate_hours(h)),
            None => "Cleared the time estimate".to_string(),
        };
        log_history(db, &id, "estimate_updated", &detail, &user.username);
    } else if updated.size != existing.size {
        log_history(
            db,
            &id,
            "estimate_updated",
            &format!(
                "Changed the time estimate to {}",
                Size::from_i64(updated.size).label()
            ),
            &user.username,
        );
    }
    if updated.color_id != existing.color_id {
        log_history(
            db,
            &id,
            "color_updated",
            "Changed the task color",
            &user.username,
        );
    }
    if updated.labels != existing.labels {
        let detail = if updated.labels.is_empty() {
            "Cleared the labels".to_string()
        } else {
            format!("Set labels: {}", updated.labels.join(", "))
        };
        log_history(db, &id, "labels_updated", &detail, &user.username);
    }
    if updated.due_at != existing.due_at || updated.due_repeat != existing.due_repeat {
        let detail = match updated.due_at.as_deref() {
            Some(due) => {
                let mut s = format!("Set the due date to {}", format_datetime(due));
                if let Some(repeat) = updated.due_repeat.as_deref() {
                    s.push_str(&format!(" (repeats {repeat})"));
                }
                s
            }
            None => "Cleared the due date".to_string(),
        };
        log_history(db, &id, "due_updated", &detail, &user.username);
    }
    // KF-224: due-date done flag flipped.
    if updated.due_done != existing.due_done {
        let detail = if updated.due_done {
            "Marked the due date done"
        } else {
            "Marked the due date not done"
        };
        log_history(db, &id, "due_done_updated", detail, &user.username);
    }
    if updated.member_ids != existing.member_ids {
        let users = db.list_users().map_err(AppError::from)?;
        let names: Vec<String> = updated
            .member_ids
            .iter()
            .filter_map(|mid| {
                users
                    .iter()
                    .find(|u| &u.id == mid)
                    .map(|u| u.username.clone())
            })
            .collect();
        let detail = if names.is_empty() {
            "Removed all members".to_string()
        } else {
            format!("Assigned members: {}", names.join(", "))
        };
        log_history(db, &id, "members_updated", &detail, &user.username);
    }
    if updated.grouping_date != existing.grouping_date {
        log_history(
            db,
            &id,
            "grouping_date_updated",
            "Changed the grouping date",
            &user.username,
        );
    }

    let task = fetch_task_view(db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(TaskCardTemplate { task })
}

/// Normalize a due-date input into RFC3339: accept a full RFC3339
/// timestamp or "YYYY-MM-DD HH:MM" / "YYYY-MM-DDTHH:MM" in the server's
/// local timezone; pass anything else through unchanged.
///
/// KF-219: a bare "YYYY-MM-DD" means "due that day", so it normalizes to
/// end of day (23:59:59), not midnight — otherwise a task due today would
/// be flagged overdue for the entire due day, while KanbanFlow only
/// counts it overdue once the day has passed.
fn normalize_due_input(raw: &str) -> String {
    let raw = raw.trim();
    if DateTime::parse_from_rfc3339(raw).is_ok() {
        return raw.to_string();
    }
    // A bare date (no time component) is due at the end of that day.
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        if let Some(end_of_day) = date.and_hms_opt(23, 59, 59) {
            if let Some(local) = end_of_day.and_local_timezone(Local).single() {
                return local.to_rfc3339();
            }
        }
    }
    for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, fmt) {
            if let Some(local) = naive.and_local_timezone(Local).single() {
                return local.to_rfc3339();
            }
        }
    }
    raw.to_string()
}

/// KF-219: overdue means the FULL due timestamp is in the past (and the
/// task is not done) — never the due date alone. A task due later today
/// is not overdue, matching KanbanFlow.
fn is_overdue(due_at: Option<&str>, done: bool) -> bool {
    due_at
        .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
        .map(|dt| dt.with_timezone(&Local) < Local::now() && !done)
        .unwrap_or(false)
}

/// Normalize an hour-based time estimate from the API (KF-216):
/// positive values set the estimate, exactly 0 clears it, anything else
/// (negative, NaN, infinite) is invalid.
fn normalize_estimate_hours(raw: f64) -> Result<Option<f64>, &'static str> {
    if raw == 0.0 {
        Ok(None)
    } else if raw.is_finite() && raw > 0.0 {
        Ok(Some(raw))
    } else {
        Err("estimate_hours must be a positive number of hours (0 clears the estimate)")
    }
}

/// Derive the pomodoro size (1..=4) from an hour estimate using the
/// configured pomodoro length (KF-216). Hours stay the canonical
/// estimate; `size` remains the derived/display concept for the pomodoro
/// legend and legacy size-based coloring.
fn estimate_hours_to_size(hours: f64, pomodoro_minutes: u32) -> i64 {
    let pomodoro_minutes = pomodoro_minutes.max(1) as f64;
    ((hours * 60.0 / pomodoro_minutes).round() as i64).clamp(1, 4)
}

/// Render an hour estimate KanbanFlow-style: "4h", "1h 30m", "30m".
fn format_estimate_hours(hours: f64) -> String {
    let minutes = (hours * 60.0).round() as i64;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[derive(Deserialize, ToSchema)]
struct MoveTaskInput {
    column_id: String,
    swimlane_id: Option<String>,
    position: f64,
}

/// Move a task to a new column/swimlane/position. Crossing into the Done
/// column stamps `completed_at`; crossing out clears it.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/move",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    request_body = MoveTaskInput,
    responses(
        (status = 200, description = "Task moved"),
        (status = 400, description = "Unknown column"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn move_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: MoveTaskInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let before = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let from_name = db
        .get_column(&before.column_id)
        .map_err(AppError::from)?
        .map(|c| c.name)
        .unwrap_or_else(|| "a column".to_string());
    let to_name = db
        .get_column(&input.column_id)
        .map_err(AppError::from)?
        .map(|c| c.name)
        .ok_or_else(|| AppError::bad_request("unknown column"))?;

    // Keep the current swimlane when the client doesn't name one.
    let swimlane_id = input.swimlane_id.filter(|s| !s.is_empty());

    // Crossing into the Done column stamps `completed_at`; crossing out
    // clears it. Both affected cells are renumbered densely inside.
    db.move_task(
        &id,
        &input.column_id,
        swimlane_id.as_deref(),
        input.position,
    )
    .map_err(AppError::from)?;

    // Return the refreshed card so the Done stamp (or its removal) appears
    // without a reload. Done-column date grouping is handled client-side
    // by reloading when the move crosses the Done boundary.
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if before.column_id != task.column_id {
        log_history(
            db,
            &id,
            "moved",
            &format!("Moved from {from_name} to {to_name}"),
            &user.username,
        );
    }
    let board_id = db
        .get_column(&task.column_id)
        .map_err(AppError::from)?
        .map(|column| column.board_id);
    let view = match board_id {
        Some(board_id) => TaskView::from_row_in_board(db, &board_id, &task)?,
        None => TaskView::from_row(&task),
    };
    Ok(TaskCardTemplate { task: view })
}

#[derive(Deserialize, ToSchema)]
struct WatchTaskInput {
    watched: bool,
}

/// Set a task's watch flag (KanbanFlow parity: "Watch" in the task More
/// menu). Persisted per task; returns the new flag.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/watch",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    request_body = WatchTaskInput,
    responses(
        (status = 200, description = "Watch flag updated", body = WatchTaskResponse),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn watch_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<WatchTaskResponse>, AppError> {
    let input: WatchTaskInput = parse_body(&headers, body).await?;
    let updated = state
        .db
        .set_task_watched(&id, input.watched)
        .map_err(AppError::from)?;
    if !updated {
        return Err(AppError::not_found("task not found"));
    }
    log_history(
        &state.db,
        &id,
        if input.watched {
            "watched"
        } else {
            "unwatched"
        },
        if input.watched {
            "Started watching this task"
        } else {
            "Stopped watching this task"
        },
        &user.username,
    );
    Ok(Json(WatchTaskResponse {
        watched: input.watched,
    }))
}

#[derive(serde::Serialize, ToSchema)]
struct WatchTaskResponse {
    watched: bool,
}

/// Delete a task and its time entries.
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "Task deleted"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_task(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let deleted = state.db.delete_task(&id).map_err(AppError::from)?;
    if !deleted {
        return Err(AppError::not_found("task not found"));
    }
    Ok(StatusCode::OK)
}

#[derive(serde::Serialize, ToSchema)]
struct TaskDetail {
    id: String,
    member_ids: Vec<String>,
    grouping_date: Option<String>,
    /// Watch flag: task More menu "Watch" (KanbanFlow parity).
    watched: bool,
    subtasks: Vec<SubtaskDetail>,
    /// Task labels (KanbanFlow parity).
    labels: Vec<String>,
    /// Due date/time as RFC3339, if set (KanbanFlow parity).
    due_at: Option<String>,
    /// Due-date repeat text, if set.
    due_repeat: Option<String>,
    /// Whether the due date was marked done (KanbanFlow parity: the card
    /// renders "(Done)").
    due_done: bool,
}

/// Fetch one task's assignment, grouping-date, and subtask state.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "Task detail", body = TaskDetail),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn get_task(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<TaskDetail>, AppError> {
    let task = state
        .db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(Json(TaskDetail {
        id: task.id.clone(),
        member_ids: task.member_ids,
        grouping_date: task.grouping_date,
        watched: task.watched,
        subtasks: task.subtasks.iter().map(SubtaskDetail::from).collect(),
        labels: task.labels,
        due_at: task.due_at,
        due_repeat: task.due_repeat,
        due_done: task.due_done,
    }))
}

#[derive(Deserialize, ToSchema)]
struct CreateSubtaskInput {
    name: String,
}

#[derive(serde::Serialize, ToSchema)]
struct SubtaskDetail {
    id: String,
    name: String,
    done: bool,
}

impl From<&Subtask> for SubtaskDetail {
    fn from(sub: &Subtask) -> Self {
        Self {
            id: sub.id.clone(),
            name: sub.name.clone(),
            done: sub.done,
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct UpdateSubtaskInput {
    name: Option<String>,
    done: Option<bool>,
}

/// Append a subtask to a task's checklist (KanbanFlow parity).
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/subtasks",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    request_body = CreateSubtaskInput,
    responses(
        (status = 200, description = "The created subtask", body = SubtaskDetail),
        (status = 400, description = "Subtask name is required"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_subtask(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SubtaskDetail>, AppError> {
    let input: CreateSubtaskInput = parse_body(&headers, body).await?;
    if input.name.trim().is_empty() {
        return Err(AppError::bad_request("subtask name is required"));
    }
    let sub = state
        .db
        .add_subtask(&id, input.name.trim())
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    log_history(
        &state.db,
        &id,
        "subtask_added",
        &format!("Added subtask \"{}\"", sub.name),
        &user.username,
    );
    Ok(Json(SubtaskDetail::from(&sub)))
}

#[derive(Deserialize, ToSchema)]
struct ReorderSubtasksInput {
    /// Subtask ids in the desired display order (full list; ids not
    /// mentioned keep their relative order after the listed ones).
    order: Vec<String>,
}

/// Reorder a task's subtasks to the given id sequence (KanbanFlow parity:
/// Cmd+Up/Down reorders the focused subtask; order is persisted).
#[utoipa::path(
    put,
    path = "/api/tasks/{id}/subtasks/order",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    request_body = ReorderSubtasksInput,
    responses(
        (status = 200, description = "The reordered subtask ids", body = Vec<String>),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn reorder_subtasks(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Vec<String>>, AppError> {
    let input: ReorderSubtasksInput = parse_body(&headers, body).await?;
    let db = &state.db;
    let ok = db
        .reorder_subtasks(&id, &input.order)
        .map_err(AppError::from)?;
    if !ok {
        return Err(AppError::not_found("task not found"));
    }
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(Json(task.subtasks.iter().map(|s| s.id.clone()).collect()))
}

/// Rename a subtask or toggle its done flag.
#[utoipa::path(
    patch,
    path = "/api/tasks/{id}/subtasks/{sub_id}",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("sub_id" = String, Path, description = "Subtask id"),
    ),
    request_body = UpdateSubtaskInput,
    responses(
        (status = 200, description = "The updated subtask", body = SubtaskDetail),
        (status = 404, description = "Task or subtask not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_subtask(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path((id, sub_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SubtaskDetail>, AppError> {
    let input: UpdateSubtaskInput = parse_body(&headers, body).await?;
    if let Some(name) = input.name.as_deref() {
        if name.trim().is_empty() {
            return Err(AppError::bad_request("subtask name is required"));
        }
    }
    let db = &state.db;
    let updated = db
        .update_subtask(
            &id,
            &sub_id,
            input.name.as_deref().map(str::trim),
            input.done,
        )
        .map_err(AppError::from)?;
    if !updated {
        return Err(AppError::not_found("task or subtask not found"));
    }
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let sub = task
        .subtasks
        .iter()
        .find(|s| s.id == sub_id)
        .ok_or_else(|| AppError::not_found("subtask not found"))?;
    if let Some(done) = input.done {
        log_history(
            db,
            &id,
            if done {
                "subtask_done"
            } else {
                "subtask_reopened"
            },
            &format!(
                "{} subtask \"{}\"",
                if done { "Completed" } else { "Reopened" },
                sub.name
            ),
            &user.username,
        );
    } else if input.name.is_some() {
        log_history(
            db,
            &id,
            "subtask_renamed",
            &format!("Renamed subtask to \"{}\"", sub.name),
            &user.username,
        );
    }
    Ok(Json(SubtaskDetail::from(sub)))
}

/// Remove a subtask from a task's checklist.
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}/subtasks/{sub_id}",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("sub_id" = String, Path, description = "Subtask id"),
    ),
    responses(
        (status = 200, description = "Subtask removed"),
        (status = 404, description = "Task or subtask not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_subtask(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path((id, sub_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let name = state
        .db
        .get_task(&id)
        .map_err(AppError::from)?
        .and_then(|t| t.subtasks.into_iter().find(|s| s.id == sub_id))
        .map(|s| s.name);
    let removed = state
        .db
        .remove_subtask(&id, &sub_id)
        .map_err(AppError::from)?;
    if !removed {
        return Err(AppError::not_found("task or subtask not found"));
    }
    if let Some(name) = name {
        log_history(
            &state.db,
            &id,
            "subtask_deleted",
            &format!("Deleted subtask \"{name}\""),
            &user.username,
        );
    }
    Ok(StatusCode::OK)
}

#[derive(serde::Serialize, ToSchema)]
struct MemberDetail {
    id: String,
    username: String,
}

// ---- Task extras: labels, due dates, comments, attachments, history ----
// (KanbanFlow parity: KF-061, KF-062, KF-064, KF-100, KF-101)

/// Distinct labels used on a board — task labels plus time-entry labels,
/// sorted. Powers the "Add labels..." suggestions in the Labels sub-dialog
/// and in the manual/edit time dialogs (KanbanFlow parity, KF-061/KF-102).
#[utoipa::path(
    get,
    path = "/api/boards/{id}/labels",
    tag = "Tasks",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Distinct labels, sorted", body = Vec<String>),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_board_labels(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<String>>, AppError> {
    if !state.db.board_exists(&id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }
    Ok(Json(state.db.board_labels(&id).map_err(AppError::from)?))
}

#[derive(Deserialize, ToSchema)]
struct CreateCommentInput {
    body: String,
    /// Optional display name to attribute the comment to. Defaults to the
    /// authenticated username (`api-token` for API-token requests, KF-221e).
    author: Option<String>,
}

/// List a task's comments as JSON, oldest first (KF-221b). API consumers
/// no longer need to scrape the task modal's HTML.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/comments",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "Task comments, oldest first", body = Vec<TaskComment>),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_comments(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<TaskComment>>, AppError> {
    let task = state
        .db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(Json(task.comments))
}

/// Add a comment to a task (KanbanFlow parity, KF-064). Returns the
/// created comment.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/comments",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    request_body = CreateCommentInput,
    responses(
        (status = 200, description = "The created comment", body = TaskComment),
        (status = 400, description = "Comment body is required"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_comment(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TaskComment>, AppError> {
    let input: CreateCommentInput = parse_body(&headers, body).await?;
    let text = input.body.trim();
    if text.is_empty() {
        return Err(AppError::bad_request("comment body is required"));
    }
    if text.len() > 5000 {
        return Err(AppError::bad_request(
            "comment is too long (max 5000 chars)",
        ));
    }
    // KF-221e: API callers can attribute the comment to a display name;
    // blank falls back to the authenticated username.
    let author: &str = input
        .author
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&user.username);
    let comment = state
        .db
        .add_comment(&id, author, text)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    log_history(
        &state.db,
        &id,
        "comment_added",
        &format!("{author} added a comment"),
        &user.username,
    );
    Ok(Json(comment))
}

/// Delete a comment from a task (KanbanFlow parity, KF-064).
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}/comments/{comment_id}",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("comment_id" = String, Path, description = "Comment id"),
    ),
    responses(
        (status = 200, description = "Comment deleted"),
        (status = 404, description = "Task or comment not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_comment(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path((id, comment_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let deleted = state
        .db
        .delete_comment(&id, &comment_id)
        .map_err(AppError::from)?;
    if !deleted {
        return Err(AppError::not_found("task or comment not found"));
    }
    log_history(
        &state.db,
        &id,
        "comment_deleted",
        "Deleted a comment",
        &user.username,
    );
    Ok(StatusCode::OK)
}

#[derive(Deserialize, ToSchema)]
struct UploadAttachmentInput {
    /// File name, e.g. "screenshot.png".
    name: String,
    /// MIME type, e.g. "image/png". Defaults to application/octet-stream.
    mime: Option<String>,
    /// Base64-encoded file bytes.
    data: String,
}

/// Upload a file attachment to a task (KanbanFlow parity, KF-064). The
/// bytes are stored in the database (10 MiB cap); download them via the
/// file route. Returns the stored attachment metadata.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/attachments",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    request_body = UploadAttachmentInput,
    responses(
        (status = 200, description = "The stored attachment metadata", body = TaskAttachment),
        (status = 400, description = "Invalid file name, data, or size"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn upload_attachment(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TaskAttachment>, AppError> {
    let input: UploadAttachmentInput = parse_body(&headers, body).await?;
    let name = input.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("file name is required"));
    }
    if name.len() > 255 {
        return Err(AppError::bad_request("file name is too long"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(input.data.trim())
        .map_err(|_| AppError::bad_request("invalid base64 data"))?;
    if bytes.len() as u64 > Db::MAX_ATTACHMENT_BYTES {
        return Err(AppError::bad_request("attachment exceeds the 10 MiB limit"));
    }
    let mime = input
        .mime
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    let attachment = state
        .db
        .add_attachment(&id, name, &mime, &bytes, &user.username)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    log_history(
        &state.db,
        &id,
        "attachment_added",
        &format!("Attached \"{name}\""),
        &user.username,
    );
    Ok(Json(attachment))
}

/// Download an attachment's bytes (KanbanFlow parity, KF-064).
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/attachments/{attachment_id}/file",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("attachment_id" = String, Path, description = "Attachment id"),
    ),
    responses(
        (status = 200, description = "The file bytes", content_type = "application/octet-stream"),
        (status = 404, description = "Task or attachment not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn download_attachment(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path((id, attachment_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let db = &state.db;
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let meta = task
        .attachments
        .iter()
        .find(|a| a.id == attachment_id)
        .ok_or_else(|| AppError::not_found("attachment not found"))?;
    let bytes = db
        .read_attachment_data(&attachment_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("attachment data missing"))?;
    // Sanitize the filename for the Content-Disposition header.
    let safe_name: String = meta
        .name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, meta.mime.clone()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{safe_name}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// Delete an attachment from a task (KanbanFlow parity, KF-064).
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}/attachments/{attachment_id}",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("attachment_id" = String, Path, description = "Attachment id"),
    ),
    responses(
        (status = 200, description = "Attachment deleted"),
        (status = 404, description = "Task or attachment not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_attachment(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path((id, attachment_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let deleted = state
        .db
        .delete_attachment(&id, &attachment_id)
        .map_err(AppError::from)?;
    if !deleted {
        return Err(AppError::not_found("task or attachment not found"));
    }
    log_history(
        &state.db,
        &id,
        "attachment_deleted",
        "Deleted an attachment",
        &user.username,
    );
    Ok(StatusCode::OK)
}

/// Dedicated in-modal time-log sub-view (KanbanFlow parity, KF-101):
/// "← {task name}" back header, "Time log" title, "+ ADD ENTRY", and
/// day-grouped entries — each with avatar + member name + duration +
/// 12h From–To range + red trash icon.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/time-log-view",
    tag = "Time",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "Time-log sub-view HTML fragment", content_type = "text/html"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn time_log_view(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<TimeLogViewTemplate, AppError> {
    let db = &state.db;
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let fallback = fallback_username(db);
    let entries = fetch_entries(db, &id, &fallback)?;
    let dates = entry_dates(db, &id)?;
    let total_minutes: i64 = entries.iter().map(|e| e.minutes).sum();
    Ok(TimeLogViewTemplate {
        task_id: id,
        task_name: task.name,
        total_minutes,
        groups: group_entries_by_day(entries, dates),
    })
}

/// Dedicated in-modal History sub-view (KanbanFlow parity, KF-100): the
/// task's activity trail (created, moves, edits, timer sessions,
/// comments, attachments), newest first.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/history-view",
    tag = "Tasks",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "History sub-view HTML fragment", content_type = "text/html"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn history_view(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<HistoryViewTemplate, AppError> {
    let db = &state.db;
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let events: Vec<HistoryEventView> = task
        .history
        .iter()
        .rev()
        .map(|e| HistoryEventView {
            kind: e.kind.clone(),
            detail: e.detail.clone(),
            actor: e.actor.clone(),
            created_display: format_datetime(&e.created_at),
        })
        .collect();
    Ok(HistoryViewTemplate {
        task_id: id,
        task_name: task.name,
        events,
    })
}

/// Delete a time entry (powers the red trash icon in the time-log
/// sub-view, KF-101).
#[utoipa::path(
    delete,
    path = "/api/time/entries/{id}",
    tag = "Time",
    params(("id" = String, Path, description = "Time entry id")),
    responses(
        (status = 200, description = "Entry deleted"),
        (status = 404, description = "Entry not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_time_entry(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = &state.db;
    let entry = db
        .all_entries()
        .map_err(AppError::from)?
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| AppError::not_found("entry not found"))?;
    if !db.delete_entry(&id).map_err(AppError::from)? {
        return Err(AppError::not_found("entry not found"));
    }
    log_history(
        db,
        &entry.task_id,
        "time_deleted",
        &format!("Deleted a {}m time entry", entry.minutes),
        &user.username,
    );
    Ok(StatusCode::OK)
}

/// Board-member roster (KanbanFlow parity). In the single-admin model
/// this is the registered users list.
#[utoipa::path(
    get,
    path = "/api/members",
    tag = "Tasks",
    responses(
        (status = 200, description = "Board members", body = Vec<MemberDetail>),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_members(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Vec<MemberDetail>>, AppError> {
    let members = state
        .db
        .list_users()
        .map_err(AppError::from)?
        .into_iter()
        .map(|user| MemberDetail {
            id: user.id,
            username: user.username,
        })
        .collect();
    Ok(Json(members))
}

/// Render one task's card fragment (used by htmx refreshes).
async fn task_card(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<TaskCardTemplate, AppError> {
    let task =
        fetch_task_view(&state.db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(TaskCardTemplate { task })
}

/// Render the task-detail modal: editable fields, timer, time entries.
async fn task_modal(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<ModalTemplate, AppError> {
    let db = &state.db;
    let task = fetch_task_view(db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;
    let column = db.get_column(&task.column_id).map_err(AppError::from)?;
    let is_done = column.as_ref().map(|c| c.is_done).unwrap_or(false);
    let column_name = column
        .map(|c| c.name)
        .unwrap_or_else(|| "Unknown column".to_string());
    let row = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let created_short = format_relative_day(&row.created_at);
    let assigned_members: Vec<MemberView> = db
        .list_users()
        .map_err(AppError::from)?
        .into_iter()
        .filter(|user| task.member_ids.iter().any(|m| m == &user.id))
        .map(|user| MemberView {
            username: user.username.clone(),
            initials: user_initials(&user.username),
        })
        .collect();
    Ok(ModalTemplate {
        task,
        is_done,
        assigned_members,
        column_name,
        created_short,
        composer_initials: user_initials(&user.username),
    })
}

#[derive(Deserialize, ToSchema)]
struct LogTimeInput {
    minutes: i64,
    note: Option<String>,
}

/// Log minutes on a task; returns the refreshed time-entries fragment.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/time",
    tag = "Time",
    params(("id" = String, Path, description = "Task id")),
    request_body = LogTimeInput,
    responses(
        (status = 200, description = "Refreshed time-entries HTML fragment", content_type = "text/html"),
        (status = 400, description = "Minutes must be between 1 and 1440"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn log_time(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<TimeEntriesTemplate, AppError> {
    let input: LogTimeInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if !(1..=24 * 60).contains(&input.minutes) {
        return Err(AppError::bad_request("minutes must be between 1 and 1440"));
    }
    if db.get_task(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("task not found"));
    }

    db.create_entry(&id, input.minutes, &input.note.unwrap_or_default())
        .map_err(AppError::from)?;

    log_history(
        db,
        &id,
        "time_logged",
        &format!("Logged {}m", input.minutes),
        &user.username,
    );

    Ok(TimeEntriesTemplate {
        entries: fetch_entries(db, &id, &fallback_username(db))?,
    })
}

/// Time-log HTML fragment for a task (used to refresh the modal log).
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/time",
    tag = "Time",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "Time-entries HTML fragment", content_type = "text/html"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn get_task_time(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<TimeEntriesTemplate, AppError> {
    let db = &state.db;
    if db.get_task(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("task not found"));
    }
    Ok(TimeEntriesTemplate {
        entries: fetch_entries(db, &id, &fallback_username(db))?,
    })
}

/// Single time entry as JSON (powers the edit-entry dialog).
#[utoipa::path(
    get,
    path = "/api/time/entries/{id}",
    tag = "Time",
    params(("id" = String, Path, description = "Time entry id")),
    responses(
        (status = 200, description = "Single time entry", body = TimeEntryDetail),
        (status = 404, description = "Entry not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn get_time_entry(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<TimeEntryDetail>, AppError> {
    let db = &state.db;
    let entry = db
        .all_entries()
        .map_err(AppError::from)?
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| AppError::not_found("entry not found"))?;
    let task_name = db
        .get_task(&entry.task_id)
        .map_err(AppError::from)?
        .map(|t| t.name)
        .unwrap_or_default();
    Ok(Json(TimeEntryDetail {
        id: entry.id,
        task_id: entry.task_id,
        task_name,
        minutes: entry.minutes,
        note: entry.note,
        started_at: entry.started_at,
        labels: entry.labels,
    }))
}

/// Standalone Time spent report page (KF-286). Title "Time spent"; top-left
/// Filter / Print / Export buttons; close × back to the board. The timer log
/// itself is a modal on the board page, reached from the timer popup's Log.
async fn timer_log_page(
    State(_state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<TimerLogTemplate, AppError> {
    Ok(TimerLogTemplate {})
}

/// Pomodoro Statistics page (v3-02080).
async fn timer_statistics_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<TimerStatisticsTemplate, AppError> {
    let boards = state
        .db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .map(|b| serde_json::json!({ "id": b.id, "name": b.name }))
        .collect();
    Ok(TimerStatisticsTemplate { boards })
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct TimerLogQuery {
    task_id: Option<String>,
    /// Filter to one board.
    board_id: Option<String>,
    /// Filter by entry kind: pomodoro | stopwatch | manual.
    entry_type: Option<String>,
    /// YYYY-MM-DD, start of the day-group range.
    from: Option<String>,
    /// YYYY-MM-DD, end of the day-group range.
    to: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Paginated timer log entries, newest first (v2-00838 "Load more").
#[utoipa::path(
    get,
    path = "/api/timer/log",
    tag = "Timer",
    params(TimerLogQuery),
    responses(
        (status = 200, description = "Paginated timer log, newest first", body = TimerLogPage),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn api_timer_log(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Query(q): Query<TimerLogQuery>,
) -> Result<Json<TimerLogPage>, AppError> {
    let db = &state.db;
    let limit = q.limit.unwrap_or(50).min(200);
    let offset = q.offset.unwrap_or(0);

    let mut entries: Vec<TimeEntryRow> = db.all_entries().map_err(AppError::from)?;
    if let Some(tid) = q.task_id.filter(|s| !s.is_empty()) {
        entries.retain(|e| e.task_id == tid);
    }
    if let Some(kind) = q.entry_type.filter(|s| !s.is_empty()) {
        entries.retain(|e| e.kind == kind);
    }
    if let Some(from) = q.from.filter(|s| !s.is_empty()) {
        entries.retain(|e| e.started_at.get(..10).is_some_and(|d| d >= from.as_str()));
    }
    if let Some(to) = q.to.filter(|s| !s.is_empty()) {
        entries.retain(|e| e.started_at.get(..10).is_some_and(|d| d <= to.as_str()));
    }
    // Resolve each entry's board up front so board filtering and the
    // board_name display don't require a second pass.
    let mut board_of: HashMap<String, (String, String)> = HashMap::new();
    for e in &entries {
        if !board_of.contains_key(&e.task_id) {
            let pair = db
                .get_task(&e.task_id)
                .ok()
                .flatten()
                .and_then(|t| db.get_column(&t.column_id).ok().flatten())
                .and_then(|c| {
                    db.get_board(&c.board_id)
                        .ok()
                        .flatten()
                        .map(|b| (b.id, b.name))
                });
            if let Some(pair) = pair {
                board_of.insert(e.task_id.clone(), pair);
            }
        }
    }
    if let Some(bid) = q.board_id.filter(|s| !s.is_empty()) {
        entries.retain(|e| board_of.get(&e.task_id).is_some_and(|(id, _)| id == &bid));
    }
    entries.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.id.cmp(&a.id))
    });

    let total = entries.len();
    let page: Vec<TimerLogEntry> = entries
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|e| {
            let task_name = db
                .get_task(&e.task_id)
                .ok()
                .flatten()
                .map(|t| t.name)
                .unwrap_or_else(|| "(deleted task)".to_string());
            let (board_id, board_name) = match board_of.get(&e.task_id) {
                Some((id, name)) => (Some(id.clone()), Some(name.clone())),
                None => (None, None),
            };
            let (start_display, end_display, range) =
                match DateTime::parse_from_rfc3339(&e.started_at) {
                    Ok(dt) => {
                        let local = dt.with_timezone(&Local);
                        let end = local + Duration::minutes(e.minutes);
                        (
                            local.format("%b %d, %Y").to_string(),
                            end.format("%-I:%M %p").to_string(),
                            format!(
                                "{} – {}",
                                local.format("%-I:%M %p"),
                                end.format("%-I:%M %p")
                            ),
                        )
                    }
                    Err(_) => (e.started_at.clone(), String::new(), String::new()),
                };
            // KF-015/KF-016: P = "Pomodoro" (or "Stopped with reason 'X'"),
            // M = "Manually added" (v3-01594; frame review).
            let (badge_code, badge_title): (&str, String) = match e.kind.as_str() {
                "pomodoro" => (
                    "P",
                    match &e.interrupt_reason {
                        Some(reason) => format!("Stopped with reason '{reason}'"),
                        None => "Pomodoro".to_string(),
                    },
                ),
                "stopwatch" => ("S", "Stopwatch".to_string()),
                _ => ("M", "Manually added".to_string()),
            };
            TimerLogEntry {
                id: e.id,
                task_id: e.task_id,
                task_name,
                board_id,
                board_name,
                minutes: e.minutes,
                duration_display: format_entry_duration(e.seconds, e.minutes),
                kind: e.kind,
                badge_code: badge_code.to_string(),
                badge_title: badge_title.to_string(),
                interrupted: e.interrupted,
                interrupt_reason: e.interrupt_reason,
                note: e.note,
                day_key: e.started_at.get(..10).unwrap_or("").to_string(),
                date: start_display,
                time_range: range,
                end: end_display,
                labels: e.labels,
                created_by: e.created_by,
            }
        })
        .collect();

    Ok(Json(TimerLogPage {
        entries: page,
        has_more: offset + limit < total,
        total,
    }))
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct TimeSpentQuery {
    /// YYYY-MM-DD, defaults to 30 days ago.
    from: Option<String>,
    /// YYYY-MM-DD, defaults to today.
    to: Option<String>,
    /// Filter to one board.
    board_id: Option<String>,
    /// Filter to one task color id.
    color_id: Option<String>,
    /// Filter by entry kind: tasks | breaks | all. "tasks" = pomodoro/stopwatch/manual,
    /// "breaks" = short_break/long_break.
    entry_type: Option<String>,
}

/// Daily time totals for the Time spent report (v2-01026).
#[utoipa::path(
    get,
    path = "/api/timer/time-spent",
    tag = "Timer",
    params(TimeSpentQuery),
    responses(
        (status = 200, description = "Daily time totals", body = TimeSpentReport),
        (status = 400, description = "Invalid date"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn api_time_spent(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Query(q): Query<TimeSpentQuery>,
) -> Result<Json<TimeSpentReport>, AppError> {
    use chrono::NaiveDate;
    let today = Local::now().date_naive();
    let bad = || AppError::bad_request("invalid date");
    let from = match q.from {
        Some(s) => NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| bad())?,
        None => today - Duration::days(29),
    };
    let to = match q.to {
        Some(s) => NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| bad())?,
        None => today,
    };
    if to < from {
        return Err(AppError::bad_request("end date is before start date"));
    }
    let from_s = from.format("%Y-%m-%d").to_string();
    let to_s = to.format("%Y-%m-%d").to_string();
    let board_id = q.board_id.filter(|s| !s.is_empty());
    let color_id = q.color_id.filter(|s| !s.is_empty());
    let entry_type = q.entry_type.filter(|s| !s.is_empty());

    let db = &state.db;
    // (date, task_id) -> minutes, plus task metadata for the detailed view.
    let mut totals: HashMap<String, i64> = HashMap::new();
    let mut task_totals: HashMap<String, i64> = HashMap::new();
    let mut break_totals: HashMap<String, i64> = HashMap::new();
    let mut per_task: HashMap<(String, String), i64> = HashMap::new();
    let mut task_meta: HashMap<String, (String, Option<String>)> = HashMap::new();
    for e in db.all_entries().map_err(AppError::from)? {
        if let Some(date) = e.started_at.get(..10) {
            if date < from_s.as_str() || date > to_s.as_str() {
                continue;
            }
            // KF-292: entry-type filter. "tasks" = pomodoro/stopwatch/manual;
            // "breaks" = short_break/long_break; anything else (or absent) = all.
            let is_break = e.kind == "short_break" || e.kind == "long_break";
            if let Some(ref et) = entry_type {
                if et == "tasks" && is_break {
                    continue;
                }
                if et == "breaks" && !is_break {
                    continue;
                }
            }
            let task = db.get_task(&e.task_id).ok().flatten();
            if let Some(ref bid) = board_id {
                let task_board = task
                    .as_ref()
                    .and_then(|t| db.get_column(&t.column_id).ok().flatten())
                    .map(|c| c.board_id);
                if task_board.as_deref() != Some(bid.as_str()) {
                    continue;
                }
            }
            if let Some(ref cid) = color_id {
                if task.as_ref().and_then(|t| t.color_id.as_deref()) != Some(cid.as_str()) {
                    continue;
                }
            }
            *totals.entry(date.to_string()).or_insert(0) += e.minutes;
            if is_break {
                *break_totals.entry(date.to_string()).or_insert(0) += e.minutes;
            } else {
                *task_totals.entry(date.to_string()).or_insert(0) += e.minutes;
            }
            *per_task
                .entry((date.to_string(), e.task_id.clone()))
                .or_insert(0) += e.minutes;
            if let Some(t) = task {
                task_meta
                    .entry(e.task_id.clone())
                    .or_insert((t.name.clone(), t.color_id.clone()));
            }
        }
    }

    let mut days = Vec::new();
    let mut d = from;
    while d <= to {
        let key = d.format("%Y-%m-%d").to_string();
        let mut tasks: Vec<TaskTime> = per_task
            .iter()
            .filter(|((date, _), _)| date == &key)
            .map(|((_, task_id), minutes)| {
                let (name, color) = task_meta
                    .get(task_id)
                    .cloned()
                    .unwrap_or_else(|| ("(deleted task)".to_string(), None));
                TaskTime {
                    task_id: task_id.clone(),
                    task_name: name,
                    minutes: *minutes,
                    color_id: color,
                }
            })
            .collect();
        tasks.sort_by(|a, b| {
            b.minutes
                .cmp(&a.minutes)
                .then_with(|| a.task_name.cmp(&b.task_name))
        });
        days.push(DayTotal {
            date: key.clone(),
            label: d.format("%b %d").to_string(),
            minutes: totals.get(&key).copied().unwrap_or(0),
            task_minutes: task_totals.get(&key).copied().unwrap_or(0),
            break_minutes: break_totals.get(&key).copied().unwrap_or(0),
            tasks,
        });
        d += Duration::days(1);
    }
    let total_minutes: i64 = days.iter().map(|d| d.minutes).sum();
    let total_task_minutes: i64 = days.iter().map(|d| d.task_minutes).sum();
    let total_break_minutes: i64 = days.iter().map(|d| d.break_minutes).sum();

    Ok(Json(TimeSpentReport {
        days,
        total_minutes,
        total_task_minutes,
        total_break_minutes,
    }))
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct TimerStatisticsQuery {
    /// YYYY-MM-DD, defaults to 30 days ago.
    from: Option<String>,
    /// YYYY-MM-DD, defaults to today.
    to: Option<String>,
    /// Filter to one board.
    board_id: Option<String>,
}

/// Pomodoro statistics (v3-02080, v3-02085, v3-02091, v3-02092).
#[utoipa::path(
    get,
    path = "/api/timer/statistics",
    tag = "Timer",
    params(TimerStatisticsQuery),
    responses(
        (status = 200, description = "Pomodoro statistics", body = TimerStatisticsReport),
        (status = 400, description = "Invalid date"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn api_timer_statistics(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Query(q): Query<TimerStatisticsQuery>,
) -> Result<Json<TimerStatisticsReport>, AppError> {
    use chrono::NaiveDate;
    let db = &state.db;
    let today = Local::now().date_naive();
    let bad = || AppError::bad_request("invalid date");
    let from = match q.from {
        Some(s) => NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| bad())?,
        None => today - Duration::days(29),
    };
    let to = match q.to {
        Some(s) => NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| bad())?,
        None => today,
    };
    if to < from {
        return Err(AppError::bad_request("end date is before start date"));
    }
    let from_s = from.format("%Y-%m-%d").to_string();
    let to_s = to.format("%Y-%m-%d").to_string();
    let board_id = q.board_id.filter(|s| !s.is_empty());

    let in_scope = |e: &TimeEntryRow| -> bool {
        let Some(date) = e.started_at.get(..10) else {
            return false;
        };
        if date < from_s.as_str() || date > to_s.as_str() {
            return false;
        }
        if let Some(ref bid) = board_id {
            let task_board = db
                .get_task(&e.task_id)
                .ok()
                .flatten()
                .and_then(|t| db.get_column(&t.column_id).ok().flatten())
                .map(|c| c.board_id);
            if task_board.as_deref() != Some(bid.as_str()) {
                return false;
            }
        }
        true
    };

    let entries = db.all_entries().map_err(AppError::from)?;
    let pomodori: Vec<&TimeEntryRow> = entries
        .iter()
        .filter(|e| e.kind == "pomodoro" && in_scope(e))
        .collect();
    let total_pomodori = pomodori.len() as i64;
    let total_minutes: i64 = pomodori.iter().map(|e| e.minutes).sum();
    let avg_minutes = if total_pomodori > 0 {
        total_minutes / total_pomodori
    } else {
        0
    };
    let interruptions = pomodori.iter().filter(|e| e.interrupted).count() as i64;

    // Interrupt counts by reason (v3-02091), green/red coloring client-side.
    let mut by_reason: HashMap<String, i64> = HashMap::new();
    for e in pomodori.iter().filter(|e| e.interrupted) {
        let reason = e
            .interrupt_reason
            .clone()
            .unwrap_or_else(|| "No reason".to_string());
        *by_reason.entry(reason).or_insert(0) += 1;
    }
    let mut by_reason: Vec<ReasonCount> = by_reason
        .into_iter()
        .map(|(reason, count)| ReasonCount { reason, count })
        .collect();
    by_reason.sort_by_key(|r| std::cmp::Reverse(r.count));

    // Daily pomodori for the bar chart over the requested range.
    let mut daily_counts: HashMap<String, i64> = HashMap::new();
    for e in &pomodori {
        if let Some(date) = e.started_at.get(..10) {
            *daily_counts.entry(date.to_string()).or_insert(0) += 1;
        }
    }
    let mut days = Vec::new();
    let mut d = from;
    while d <= to {
        let key = d.format("%Y-%m-%d").to_string();
        days.push(DayPomodori {
            date: key.clone(),
            label: d.format("%b %d").to_string(),
            pomodori: daily_counts.get(&key).copied().unwrap_or(0),
        });
        d += Duration::days(1);
    }

    // Highscores: best day and longest streak within the range.
    let best_day = days.iter().max_by_key(|day| day.pomodori).cloned();
    let mut longest_streak = 0i64;
    let mut run = 0i64;
    for day in &days {
        if day.pomodori > 0 {
            run += 1;
            longest_streak = longest_streak.max(run);
        } else {
            run = 0;
        }
    }

    Ok(Json(TimerStatisticsReport {
        total_pomodori,
        total_minutes,
        avg_minutes,
        interruptions,
        by_reason,
        daily: days,
        best_day,
        longest_streak,
    }))
}

#[derive(Deserialize, ToSchema)]
struct ManualTimeInput {
    task_id: String,
    /// YYYY-MM-DD
    date: String,
    /// HH:MM or HH:MM:SS (24h)
    from: String,
    /// HH:MM or HH:MM:SS (24h)
    to: String,
    note: Option<String>,
    /// Labels for the entry (KanbanFlow parity). Absent = none.
    labels: Option<Vec<String>>,
}

/// "Add time manually" dialog (v3-00001): explicit date + from/to.
/// Duration is auto-computed; future times are rejected with KanbanFlow's
/// exact message and the dialog keeps its state (client-side).
#[utoipa::path(
    post,
    path = "/api/time/manual",
    tag = "Time",
    request_body = ManualTimeInput,
    responses(
        (status = 200, description = "Created entry id and minutes", body = IdMinutesResult),
        (status = 400, description = "Invalid date/time range or time in the future"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_manual_time(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IdMinutesResult>, AppError> {
    let input: ManualTimeInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let (started_at, minutes) = parse_manual_range(&input.date, &input.from, &input.to)?;
    let now = Local::now();
    if started_at > now {
        return Err(AppError::bad_request(
            "You can not enter a time in the future",
        ));
    }
    if minutes < 1 {
        return Err(AppError::bad_request("End time must be after start time"));
    }

    let id = db
        .create_entry_at(
            &input.task_id,
            minutes,
            &input.note.unwrap_or_default(),
            &started_at.to_rfc3339(),
            input.labels.as_deref().unwrap_or(&[]),
            &user.username,
        )
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;

    log_history(
        db,
        &input.task_id,
        "time_logged",
        &format!("Manually logged {minutes}m"),
        &user.username,
    );

    Ok(Json(IdMinutesResult { id, minutes }))
}

/// Parse a manual date + from/to range into a start DateTime and duration.
/// Times are interpreted in the server's local timezone. Seconds are
/// optional (the edit dialog's time inputs carry step="1").
fn parse_manual_range(
    date: &str,
    from: &str,
    to: &str,
) -> Result<(DateTime<Local>, i64), AppError> {
    use chrono::NaiveDateTime;
    let bad = || AppError::bad_request("invalid date or time");
    let parse = |s: &str| {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
    };
    let start = parse(&format!("{date} {from}")).map_err(|_| bad())?;
    let end = parse(&format!("{date} {to}")).map_err(|_| bad())?;
    let start = start.and_local_timezone(Local).single().ok_or_else(bad)?;
    let end = end.and_local_timezone(Local).single().ok_or_else(bad)?;
    let minutes = (end - start).num_minutes();
    Ok((start, minutes))
}

#[derive(Deserialize, ToSchema)]
struct UpdateTimeEntryInput {
    task_id: String,
    /// YYYY-MM-DD
    date: String,
    /// HH:MM or HH:MM:SS (24h)
    from: String,
    /// HH:MM or HH:MM:SS (24h)
    to: String,
    note: Option<String>,
    /// Labels for the entry (KanbanFlow parity). Absent = none.
    labels: Option<Vec<String>>,
}

/// Edit a time entry (v3-01841): date/time, task reassignment, note.
/// Date edits re-bucket the entry via its new started_at.
#[utoipa::path(
    put,
    path = "/api/time/entries/{id}",
    tag = "Time",
    params(("id" = String, Path, description = "Time entry id")),
    request_body = UpdateTimeEntryInput,
    responses(
        (status = 200, description = "Updated entry id and minutes", body = IdMinutesResult),
        (status = 400, description = "Invalid date/time range"),
        (status = 404, description = "Entry or task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_time_entry(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IdMinutesResult>, AppError> {
    let input: UpdateTimeEntryInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let (started_at, minutes) = parse_manual_range(&input.date, &input.from, &input.to)?;
    if started_at > Local::now() {
        return Err(AppError::bad_request(
            "You can not enter a time in the future",
        ));
    }
    if minutes < 1 {
        return Err(AppError::bad_request("End time must be after start time"));
    }
    if db
        .get_task(&input.task_id)
        .map_err(AppError::from)?
        .is_none()
    {
        return Err(AppError::not_found("task not found"));
    }

    let updated = db
        .update_entry(
            &id,
            &input.task_id,
            minutes,
            &input.note.unwrap_or_default(),
            &started_at.to_rfc3339(),
            input.labels.as_deref().unwrap_or(&[]),
        )
        .map_err(AppError::from)?;
    if !updated {
        return Err(AppError::not_found("entry not found"));
    }

    log_history(
        db,
        &input.task_id,
        "time_edited",
        &format!("Edited a time entry ({minutes}m)"),
        &user.username,
    );

    Ok(Json(IdMinutesResult { id, minutes }))
}

// ---- Column & swimlane management ----

/// Resolve the board for a create call: an explicit id when given,
/// otherwise the first board (mirrors the `/` redirect).
fn resolve_board(db: &Db, board_id: Option<String>) -> Result<String, AppError> {
    if let Some(id) = board_id.filter(|s| !s.is_empty()) {
        return match db.board_exists(&id).map_err(AppError::from)? {
            true => Ok(id),
            false => Err(AppError::bad_request("unknown board")),
        };
    }
    db.first_board_id()
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::internal("no boards found"))
}

#[derive(Deserialize, ToSchema)]
struct CreateColumnInput {
    board_id: Option<String>,
    name: String,
    wip_limit: Option<i64>,
}

/// Create a column at the end of the board's column order; returns its id.
#[utoipa::path(
    post,
    path = "/api/columns",
    tag = "Columns",
    request_body = CreateColumnInput,
    responses(
        (status = 200, description = "Created column id", body = IdResult),
        (status = 400, description = "Invalid input"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_column(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IdResult>, AppError> {
    let input: CreateColumnInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("column name is required"));
    }
    if let Some(limit) = input.wip_limit {
        if limit < 1 {
            return Err(AppError::bad_request("WIP limit must be at least 1"));
        }
    }

    let board_id = resolve_board(db, input.board_id)?;
    let id = db
        .create_column(&board_id, &name, input.wip_limit)
        .map_err(AppError::from)?;
    Ok(Json(IdResult { id }))
}

#[derive(Deserialize, ToSchema)]
struct UpdateColumnInput {
    name: Option<String>,
    /// WIP limit. Explicit JSON null clears the limit, a number sets it;
    /// absent leaves it unchanged (KF-228).
    #[serde(default, deserialize_with = "de_opt_opt")]
    wip_limit: Option<Option<i64>>,
    is_done: Option<bool>,
    /// Column description (empty string clears it).
    description: Option<String>,
    /// Collapse/expand the column on the board.
    collapsed: Option<bool>,
    /// Opaque settings bag for the column dialog (must be valid JSON).
    config_json: Option<String>,
}

/// Rename a column, set/clear its WIP limit, flip its done flag, and
/// update its description, collapsed state, and settings bag.
#[utoipa::path(
    patch,
    path = "/api/columns/{id}",
    tag = "Columns",
    params(("id" = String, Path, description = "Column id")),
    request_body = UpdateColumnInput,
    responses(
        (status = 200, description = "Column updated"),
        (status = 400, description = "Invalid input"),
        (status = 404, description = "Column not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_column(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: UpdateColumnInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if db.get_column(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("column not found"));
    }

    if let Some(name) = input.name.as_deref() {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::bad_request("column name is required"));
        }
        db.rename_column(&id, name).map_err(AppError::from)?;
    }
    if let Some(wip_limit) = input.wip_limit {
        if let Some(limit) = wip_limit {
            if limit < 1 {
                return Err(AppError::bad_request("WIP limit must be at least 1"));
            }
        }
        db.set_column_wip(&id, wip_limit).map_err(AppError::from)?;
    }
    if let Some(is_done) = input.is_done {
        db.set_column_done(&id, is_done).map_err(AppError::from)?;
    }
    if let Some(description) = input.description.as_deref() {
        db.set_column_description(&id, description.trim())
            .map_err(AppError::from)?;
    }
    if let Some(collapsed) = input.collapsed {
        db.set_column_collapsed(&id, collapsed)
            .map_err(AppError::from)?;
    }
    if let Some(config_json) = input.config_json.as_deref() {
        serde_json::from_str::<serde_json::Value>(config_json)
            .map_err(|_| AppError::bad_request("config_json must be valid JSON"))?;
        db.set_column_config(&id, config_json)
            .map_err(AppError::from)?;
    }
    Ok(StatusCode::OK)
}

#[derive(Deserialize, ToSchema)]
struct MoveColumnInput {
    position: i64,
}

/// Reorder a column within its board; positions are renumbered densely.
#[utoipa::path(
    post,
    path = "/api/columns/{id}/move",
    tag = "Columns",
    params(("id" = String, Path, description = "Column id")),
    request_body = MoveColumnInput,
    responses(
        (status = 200, description = "Column moved"),
        (status = 404, description = "Column not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn move_column(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: MoveColumnInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if !db
        .move_column(&id, input.position)
        .map_err(AppError::from)?
    {
        return Err(AppError::not_found("column not found"));
    }
    Ok(StatusCode::OK)
}

/// Delete a column. Refuses with 400 while it still holds tasks.
#[utoipa::path(
    delete,
    path = "/api/columns/{id}",
    tag = "Columns",
    params(("id" = String, Path, description = "Column id")),
    responses(
        (status = 200, description = "Column deleted"),
        (status = 400, description = "Column still holds tasks"),
        (status = 404, description = "Column not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_column(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = &state.db;

    if db.get_column(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("column not found"));
    }
    let task_count = db.count_tasks_in_column(&id).map_err(AppError::from)?;
    if task_count > 0 {
        return Err(AppError::bad_request(format!(
            "cannot delete column with {task_count} task(s); move or delete them first"
        )));
    }
    db.delete_column(&id).map_err(AppError::from)?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize, ToSchema)]
struct CreateSwimlaneInput {
    board_id: Option<String>,
    name: String,
}

/// Create a swimlane at the end of the board's swimlane order; returns its id.
#[utoipa::path(
    post,
    path = "/api/swimlanes",
    tag = "Swimlanes",
    request_body = CreateSwimlaneInput,
    responses(
        (status = 200, description = "Created swimlane id", body = IdResult),
        (status = 400, description = "Invalid input"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_swimlane(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IdResult>, AppError> {
    let input: CreateSwimlaneInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("swimlane name is required"));
    }

    let board_id = resolve_board(db, input.board_id)?;
    let id = db
        .create_swimlane(&board_id, &name)
        .map_err(AppError::from)?;
    Ok(Json(IdResult { id }))
}

#[derive(Deserialize, ToSchema)]
struct UpdateSwimlaneInput {
    name: Option<String>,
}

/// Rename a swimlane.
#[utoipa::path(
    patch,
    path = "/api/swimlanes/{id}",
    tag = "Swimlanes",
    params(("id" = String, Path, description = "Swimlane id")),
    request_body = UpdateSwimlaneInput,
    responses(
        (status = 200, description = "Swimlane renamed"),
        (status = 400, description = "Invalid input"),
        (status = 404, description = "Swimlane not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_swimlane(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: UpdateSwimlaneInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let name = input
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::bad_request("swimlane name is required"))?;
    if !db.rename_swimlane(&id, &name).map_err(AppError::from)? {
        return Err(AppError::not_found("swimlane not found"));
    }
    Ok(StatusCode::OK)
}

#[derive(Deserialize, ToSchema)]
struct MoveSwimlaneInput {
    position: i64,
}

/// Reorder a swimlane within its board; positions are renumbered densely.
#[utoipa::path(
    post,
    path = "/api/swimlanes/{id}/move",
    tag = "Swimlanes",
    params(("id" = String, Path, description = "Swimlane id")),
    request_body = MoveSwimlaneInput,
    responses(
        (status = 200, description = "Swimlane moved"),
        (status = 404, description = "Swimlane not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn move_swimlane(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: MoveSwimlaneInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if !db
        .move_swimlane(&id, input.position)
        .map_err(AppError::from)?
    {
        return Err(AppError::not_found("swimlane not found"));
    }
    Ok(StatusCode::OK)
}

/// Delete a swimlane. Refuses with 400 while it still holds tasks.
#[utoipa::path(
    delete,
    path = "/api/swimlanes/{id}",
    tag = "Swimlanes",
    params(("id" = String, Path, description = "Swimlane id")),
    responses(
        (status = 200, description = "Swimlane deleted"),
        (status = 400, description = "Swimlane still holds tasks"),
        (status = 404, description = "Swimlane not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_swimlane(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = &state.db;

    let lane = db
        .get_swimlane(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("swimlane not found"))?;
    let task_count = db.count_tasks_in_swimlane(&id).map_err(AppError::from)?;
    if task_count > 0 {
        return Err(AppError::bad_request(format!(
            "cannot delete swimlane with {task_count} task(s); move or delete them first"
        )));
    }
    // KF-149: the board must keep at least one swimlane — with zero, the
    // column "+" add-task buttons silently do nothing.
    if db
        .list_swimlanes(&lane.board_id)
        .map_err(AppError::from)?
        .len()
        <= 1
    {
        return Err(AppError::bad_request(
            "cannot delete the last swimlane on a board",
        ));
    }
    db.delete_swimlane(&id).map_err(AppError::from)?;
    Ok(StatusCode::OK)
}

// ---- Boards, board templates, task colors ----

/// All boards as id/name pairs. Archived boards are excluded unless
/// `?include_archived=true` is passed (KF-305).
#[utoipa::path(
    get,
    path = "/api/boards",
    tag = "Boards",
    params(("include_archived" = Option<bool>, Query, description = "Include archived boards (KF-305)")),
    responses(
        (status = 200, description = "All boards as id/name pairs", body = Vec<BoardListItem>),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_boards(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<BoardListItem>>, AppError> {
    let include_archived = query
        .get("include_archived")
        .is_some_and(|v| v == "true" || v == "1");
    let boards = state
        .db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .filter(|board| include_archived || !board.config_bool("archived"))
        .map(|board| BoardListItem {
            id: board.id.clone(),
            name: board.name.clone(),
            archived: board.config_bool("archived"),
        })
        .collect();
    Ok(Json(boards))
}

/// List a board's columns (id, name, WIP limit, Done flag) for the
/// Move-task dialog's board switcher (KF-070; KF-221c).
#[utoipa::path(
    get,
    path = "/api/boards/{id}/columns",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Board columns", body = Vec<ColumnListItem>),
        (status = 404, description = "Board not found")
    )
)]
async fn list_board_columns(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ColumnListItem>>, AppError> {
    // 404 on unknown board so the dialog doesn't offer a dead target.
    state
        .db
        .get_board(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    let columns = state
        .db
        .list_columns(&id)
        .map_err(AppError::from)?
        .into_iter()
        .map(|c| ColumnListItem {
            id: c.id,
            name: c.name,
            wip_limit: c.wip_limit,
            is_done: c.is_done,
        })
        .collect();
    Ok(Json(columns))
}

#[derive(Deserialize, ToSchema)]
struct CreateBoardInput {
    name: String,
    /// Optional template id. Without one the board starts blank: the
    /// standard color palette plus a single "To-do" column.
    template_id: Option<String>,
}

/// Create a board, optionally from a template; returns the board id.
/// A blank board gets the standard color palette and one "To-do" column.
#[utoipa::path(
    post,
    path = "/api/boards",
    tag = "Boards",
    request_body = CreateBoardInput,
    responses(
        (status = 200, description = "Created board id", body = IdResult),
        (status = 400, description = "Invalid input: empty name"),
        (status = 404, description = "Template not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_board(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IdResult>, AppError> {
    let input: CreateBoardInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("board name is required"));
    }

    let id = match input.template_id.filter(|s| !s.is_empty()) {
        Some(template_id) => db
            .apply_template(&template_id, &name)
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::not_found("template not found"))?,
        None => {
            let id = db.create_board(&name).map_err(AppError::from)?;
            db.ensure_board_colors(&id).map_err(AppError::from)?;
            db.create_column(&id, "To-do", None)
                .map_err(AppError::from)?;
            // KF-149: a board with zero swimlanes leaves the column "+"
            // add-task buttons silently dead (the form clones into the first
            // swimlane row's cell).
            db.ensure_default_swimlane(&id).map_err(AppError::from)?;
            id
        }
    };
    Ok(Json(IdResult { id }))
}

/// Delete a board and everything in it. Refuses when it is the last board.
#[utoipa::path(
    delete,
    path = "/api/boards/{id}",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Board deleted"),
        (status = 400, description = "Cannot delete the last board"),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_board_api(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.db.delete_board(&id).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("not found") {
            AppError::not_found(msg)
        } else if msg.contains("last board") {
            AppError::bad_request(msg)
        } else {
            AppError::from(e)
        }
    })?;
    Ok(Json(serde_json::json!({ "deleted": id })))
}

#[derive(Template)]
#[template(path = "board_delete.html")]
struct BoardDeleteTemplate {
    board_id: String,
    board_name: String,
    task_count: usize,
}

#[derive(Deserialize, ToSchema)]
struct BoardConfigInput {
    /// KF-183: the Menu's Color legend toggle. When present, it is merged
    /// into the board's opaque config bag.
    legend_visible: Option<bool>,
    /// KF-305: archive/unarchive the board. When present, it is merged
    /// into the board's opaque config bag.
    archived: Option<bool>,
}

#[derive(Serialize, ToSchema)]
struct BoardConfigView {
    /// KF-183: whether the color legend bar is shown on this board.
    legend_visible: bool,
    /// KF-305: whether the board is archived.
    archived: bool,
}

/// Merge per-board UI settings into the board's opaque config bag
/// (KF-183: the Color legend Menu toggle persists `legend_visible` here,
/// per board).
#[utoipa::path(
    put,
    path = "/api/boards/{id}/config",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    request_body = BoardConfigInput,
    responses(
        (status = 200, description = "Current board config", body = BoardConfigView),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_board_config(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<BoardConfigView>, AppError> {
    let input: BoardConfigInput = parse_body(&headers, body).await?;
    let db = &state.db;
    if db.get_board(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("board not found"));
    }
    let mut updates = serde_json::Map::new();
    if let Some(legend_visible) = input.legend_visible {
        updates.insert(
            "legend_visible".to_string(),
            serde_json::Value::Bool(legend_visible),
        );
    }
    if let Some(archived) = input.archived {
        updates.insert("archived".to_string(), serde_json::Value::Bool(archived));
    }
    db.set_board_config(&id, &updates).map_err(AppError::from)?;
    let board = db
        .get_board(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(Json(BoardConfigView {
        legend_visible: board.config_bool("legend_visible"),
        archived: board.config_bool("archived"),
    }))
}

/// Red "Delete board" confirmation page (KanbanFlow parity: Settings →
/// Delete board → red confirmation page → final "cannot be undone" dialog).
#[utoipa::path(
    get,
    path = "/b/{board_id}/settings/delete",
    tag = "Boards",
    params(("board_id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Delete-board confirmation HTML page", content_type = "text/html"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn board_delete_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<BoardDeleteTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    let task_count = state
        .db
        .board_tasks(&board_id)
        .map_err(AppError::from)?
        .len();
    Ok(BoardDeleteTemplate {
        board_id: board.id,
        board_name: board.name,
        task_count,
    })
}

/// "New board" page: existing boards plus the template picker.
#[utoipa::path(
    get,
    path = "/boards/new",
    tag = "Boards",
    responses(
        (status = 200, description = "New-board HTML page", content_type = "text/html"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn new_board_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<NewBoardTemplate, AppError> {
    let db = &state.db;
    let boards = db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .filter(|board| !board.config_bool("archived"))
        .map(|board| BoardListItem {
            id: board.id.clone(),
            name: board.name.clone(),
            archived: false,
        })
        .collect();
    let templates = db
        .list_templates()
        .map_err(AppError::from)?
        .into_iter()
        .map(|template| TemplateListItem {
            id: template.id,
            name: template.name,
            description: template.description,
            built_in: template.built_in,
        })
        .collect();
    Ok(NewBoardTemplate { boards, templates })
}

/// KF-305: list archived boards with unarchive actions.
#[utoipa::path(
    get,
    path = "/boards/archived",
    tag = "Boards",
    responses(
        (status = 200, description = "Archived-boards HTML page", content_type = "text/html"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn archived_boards_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<ArchivedBoardsTemplate, AppError> {
    let db = &state.db;
    let boards = db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .filter(|board| board.config_bool("archived"))
        .map(|board| BoardListItem {
            id: board.id.clone(),
            name: board.name.clone(),
            archived: true,
        })
        .collect();
    Ok(ArchivedBoardsTemplate { boards })
}

#[derive(Deserialize, ToSchema)]
struct SaveTemplateInput {
    name: String,
    description: Option<String>,
}

/// Capture a board as a reusable template: its colors (all, with their
/// config), columns, swimlanes, and tasks. Returns the template id.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/save-as-template",
    tag = "Templates",
    params(("id" = String, Path, description = "Board id")),
    request_body = SaveTemplateInput,
    responses(
        (status = 200, description = "Created template id", body = IdResult),
        (status = 400, description = "Invalid input: empty name"),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn save_board_as_template(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IdResult>, AppError> {
    let input: SaveTemplateInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("template name is required"));
    }
    let description = input.description.unwrap_or_default();

    let id = db
        .save_board_as_template(&id, &name, &description)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(Json(IdResult { id }))
}

/// All board templates: built-ins first, then by name.
#[utoipa::path(
    get,
    path = "/api/templates",
    tag = "Templates",
    responses(
        (status = 200, description = "Board templates", body = Vec<TemplateListItem>),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_templates(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Vec<TemplateListItem>>, AppError> {
    let templates = state
        .db
        .list_templates()
        .map_err(AppError::from)?
        .into_iter()
        .map(|template| TemplateListItem {
            id: template.id,
            name: template.name,
            description: template.description,
            built_in: template.built_in,
        })
        .collect();
    Ok(Json(templates))
}

/// Delete a template. Built-in templates are protected (400).
#[utoipa::path(
    delete,
    path = "/api/templates/{id}",
    tag = "Templates",
    params(("id" = String, Path, description = "Template id")),
    responses(
        (status = 200, description = "Template deleted"),
        (status = 400, description = "Built-in templates cannot be deleted"),
        (status = 404, description = "Template not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_template(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    use crate::db::TemplateDeleteOutcome;
    match state.db.delete_template(&id).map_err(AppError::from)? {
        TemplateDeleteOutcome::Deleted => Ok(StatusCode::OK),
        TemplateDeleteOutcome::NotFound => Err(AppError::not_found("template not found")),
        TemplateDeleteOutcome::RefusedBuiltIn => Err(AppError::bad_request(
            "built-in templates cannot be deleted",
        )),
    }
}

// ---- Board settings shell (KF-080) ----

/// One board-settings tab rendered inside the shared shell template.
#[derive(Template)]
#[template(path = "board_settings.html")]
struct BoardSettingsTemplate {
    board_id: String,
    board_name: String,
    /// "general" | "layout" | "colors" | "task" | "advanced" | "api" | "email"
    tab: String,
    colors: Vec<ColorView>,
    /// Other boards, for the Colors tab's "Copy from board" dialog.
    boards: Vec<BoardListItem>,
    columns: Vec<SettingsColumnView>,
    swimlanes: Vec<SettingsLaneView>,
    default_color_label: String,
    done_column_name: String,
    board_position: i64,
    /// True right after the General tab's rename form saved (?saved=1).
    saved: bool,
}

#[derive(Debug, Clone)]
struct SettingsColumnView {
    name: String,
    wip_limit: Option<i64>,
    is_done: bool,
    collapsed: bool,
}

#[derive(Debug, Clone)]
struct SettingsLaneView {
    name: String,
}

fn board_settings_context(
    db: &Db,
    board_id: &str,
    tab: &str,
    saved: bool,
) -> Result<BoardSettingsTemplate, AppError> {
    let board = db
        .get_board(board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    let colors: Vec<ColorView> = db
        .list_colors(board_id)
        .map_err(AppError::from)?
        .iter()
        .map(ColorView::from)
        .collect();
    let boards: Vec<BoardListItem> = db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .filter(|b| b.id != board_id)
        .filter(|b| !b.config_bool("archived"))
        .map(|b| BoardListItem {
            id: b.id.clone(),
            name: b.name.clone(),
            archived: false,
        })
        .collect();
    let columns: Vec<SettingsColumnView> = db
        .list_columns(board_id)
        .map_err(AppError::from)?
        .into_iter()
        .map(|c| SettingsColumnView {
            name: c.name,
            wip_limit: c.wip_limit,
            is_done: c.is_done,
            collapsed: c.collapsed,
        })
        .collect();
    let swimlanes: Vec<SettingsLaneView> = db
        .list_swimlanes(board_id)
        .map_err(AppError::from)?
        .into_iter()
        .map(|s| SettingsLaneView { name: s.name })
        .collect();
    let default_color_label = db
        .default_color(board_id)
        .map_err(AppError::from)?
        .map(|c| c.label)
        .unwrap_or_default();
    let done_column_name = columns
        .iter()
        .find(|c| c.is_done)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    Ok(BoardSettingsTemplate {
        board_id: board.id,
        board_name: board.name,
        tab: tab.to_string(),
        colors,
        boards,
        columns,
        swimlanes,
        default_color_label,
        done_column_name,
        board_position: board.position,
        saved,
    })
}

macro_rules! board_settings_tab {
    ($name:ident, $tab:literal, $path:literal) => {
        #[utoipa::path(
            get,
            path = $path,
            tag = "Board settings",
            params(("board_id" = String, Path, description = "Board id")),
            responses(
                (status = 200, description = "Board settings HTML page", content_type = "text/html"),
                (status = 404, description = "Board not found"),
                (status = 401, description = "Missing or invalid credentials"),
            ),
        )]
        async fn $name(
            State(state): State<AppState>,
            Extension(_user): Extension<AuthUser>,
            Path(board_id): Path<String>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Result<BoardSettingsTemplate, AppError> {
            board_settings_context(&state.db, &board_id, $tab, query.contains_key("saved"))
        }
    };
}

board_settings_tab!(board_settings_page, "general", "/b/{board_id}/settings");
board_settings_tab!(
    board_settings_layout_page,
    "layout",
    "/b/{board_id}/settings/layout"
);
board_settings_tab!(
    board_settings_task_page,
    "task",
    "/b/{board_id}/settings/task-settings"
);
board_settings_tab!(
    board_settings_advanced_page,
    "advanced",
    "/b/{board_id}/settings/advanced"
);
board_settings_tab!(
    board_settings_api_page,
    "api",
    "/b/{board_id}/settings/api-webhooks"
);
board_settings_tab!(
    board_settings_email_page,
    "email",
    "/b/{board_id}/settings/add-from-email"
);

#[derive(Deserialize)]
struct BoardSettingsForm {
    name: String,
}

/// Rename the board from the Board Settings → General tab.
async fn board_settings_submit(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
    Form(form): Form<BoardSettingsForm>,
) -> Result<Response, AppError> {
    if !state.db.board_exists(&board_id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }
    match state
        .db
        .rename_board(&board_id, &form.name)
        .map_err(AppError::from)?
    {
        true => Ok(Redirect::to(&format!("/b/{board_id}/settings?saved=1")).into_response()),
        false => Err(AppError::not_found("board not found")),
    }
}

/// Board color-admin page (Board Settings → Colors).
#[utoipa::path(
    get,
    path = "/b/{board_id}/settings/colors",
    tag = "Colors",
    params(("board_id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Color-admin HTML page", content_type = "text/html"),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn board_colors_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<BoardSettingsTemplate, AppError> {
    board_settings_context(&state.db, &board_id, "colors", false)
}

/// Copy another board's whole color palette onto this board (KF-083).
/// Replaces every color on the target board; tasks keep their color by
/// standard value.
#[derive(Debug, Deserialize, ToSchema)]
struct CopyColorsInput {
    /// Board to copy the palette from.
    source_board_id: String,
}

#[derive(Debug, serde::Serialize, ToSchema)]
struct CountResult {
    count: usize,
}

#[utoipa::path(
    post,
    path = "/api/boards/{id}/colors/copy-from",
    tag = "Colors",
    params(("id" = String, Path, description = "Target board id")),
    request_body = CopyColorsInput,
    responses(
        (status = 200, description = "Number of colors copied", body = CountResult),
        (status = 400, description = "Invalid input: unknown board or self-copy"),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn copy_board_colors(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CountResult>, AppError> {
    let input: CopyColorsInput = parse_body(&headers, body).await?;
    let db = &state.db;
    if !db.board_exists(&id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }
    if !db
        .board_exists(&input.source_board_id)
        .map_err(AppError::from)?
    {
        return Err(AppError::bad_request("unknown source board"));
    }
    if input.source_board_id == id {
        return Err(AppError::bad_request(
            "cannot copy a board's palette onto itself",
        ));
    }
    let count = db
        .copy_colors(&id, &input.source_board_id)
        .map_err(AppError::from)?;
    Ok(Json(CountResult { count }))
}

/// A board's color palette, ordered by sort_order. Backfills the standard
/// palette on boards that predate colors.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/colors",
    tag = "Colors",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Board color palette", body = Vec<ColorView>),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_board_colors(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ColorView>>, AppError> {
    let db = &state.db;
    if !db.board_exists(&id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }
    let colors = db
        .list_colors(&id)
        .map_err(AppError::from)?
        .iter()
        .map(ColorView::from)
        .collect();
    Ok(Json(colors))
}

#[derive(Deserialize, ToSchema)]
struct CreateColorInput {
    /// Standard color value: yellow | green | blue | red | orange |
    /// purple | magenta | cyan | brown | white. Hex values are fixed per
    /// value and never user-editable.
    value: String,
    /// Defaults to the capitalized color name when empty.
    label: Option<String>,
    /// Legend tooltip text.
    description: Option<String>,
}

/// Add a color slot for a standard value. Errors with 400 when the value
/// is unknown or the board already has a slot for it.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/colors",
    tag = "Colors",
    params(("id" = String, Path, description = "Board id")),
    request_body = CreateColorInput,
    responses(
        (status = 200, description = "Created color id", body = IdResult),
        (status = 400, description = "Invalid input: unknown color value, duplicate, or bad label"),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_board_color(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IdResult>, AppError> {
    let input: CreateColorInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if !db.board_exists(&id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }
    if standard_color(&input.value).is_none() {
        return Err(AppError::bad_request(format!(
            "unknown color value: {}",
            input.value
        )));
    }
    if let Some(label) = input.label.as_deref() {
        if !label.trim().is_empty() {
            Db::validate_color_label(label).map_err(AppError::bad_request)?;
        }
    }

    let id = db
        .create_color(
            &id,
            &input.value,
            input.label.as_deref(),
            input.description.as_deref(),
        )
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    Ok(Json(IdResult { id }))
}

#[derive(Deserialize, ToSchema)]
struct UpdateColorInput {
    /// New label, max 50 chars (KanbanFlow parity).
    label: Option<String>,
    /// Legend tooltip text; empty string clears it.
    description: Option<String>,
    /// Whether the task color picker offers this color.
    enabled: Option<bool>,
    /// True makes this the board's default color.
    is_default: Option<bool>,
    /// Desired position; the board's colors are renumbered densely.
    sort_order: Option<i64>,
}

/// Rename/describe/enable a color, change the default, or reorder it.
#[utoipa::path(
    patch,
    path = "/api/boards/{id}/colors/{color_id}",
    tag = "Colors",
    params(
        ("id" = String, Path, description = "Board id"),
        ("color_id" = String, Path, description = "Color id"),
    ),
    request_body = UpdateColorInput,
    responses(
        (status = 200, description = "Color updated"),
        (status = 400, description = "Invalid input: label too long"),
        (status = 404, description = "Board or color not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_board_color(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path((id, color_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: UpdateColorInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if !db.board_exists(&id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }
    if db
        .get_color(&color_id)
        .map_err(AppError::from)?
        .filter(|color| color.board_id == id)
        .is_none()
    {
        return Err(AppError::not_found("color not found"));
    }

    if let Some(label) = input.label.as_deref() {
        Db::validate_color_label(label).map_err(AppError::bad_request)?;
    }
    if !db
        .update_color(
            &color_id,
            input.label.as_deref(),
            input.description.as_deref(),
            input.enabled,
            input.is_default,
        )
        .map_err(AppError::from)?
    {
        return Err(AppError::not_found("color not found"));
    }
    if let Some(sort_order) = input.sort_order {
        db.move_color(&color_id, sort_order)
            .map_err(AppError::from)?;
    }
    Ok(StatusCode::OK)
}

/// Delete a color slot. Refuses with 400 when it's the board default or
/// tasks still use it.
#[utoipa::path(
    delete,
    path = "/api/boards/{id}/colors/{color_id}",
    tag = "Colors",
    params(
        ("id" = String, Path, description = "Board id"),
        ("color_id" = String, Path, description = "Color id"),
    ),
    responses(
        (status = 200, description = "Color deleted"),
        (status = 400, description = "Cannot delete the default color or a color tasks use"),
        (status = 404, description = "Board or color not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_board_color(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path((id, color_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    use crate::db::ColorDeleteOutcome;
    let db = &state.db;

    if !db.board_exists(&id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }
    if db
        .get_color(&color_id)
        .map_err(AppError::from)?
        .filter(|color| color.board_id == id)
        .is_none()
    {
        return Err(AppError::not_found("color not found"));
    }

    match db.delete_color(&color_id).map_err(AppError::from)? {
        ColorDeleteOutcome::Deleted => Ok(StatusCode::OK),
        ColorDeleteOutcome::NotFound => Err(AppError::not_found("color not found")),
        ColorDeleteOutcome::RefusedDefault => Err(AppError::bad_request(
            "cannot delete the board's default color; pick another default first",
        )),
        ColorDeleteOutcome::RefusedInUse(count) => Err(AppError::bad_request(format!(
            "cannot delete color with {count} task(s) using it; recolor them first"
        ))),
    }
}

// ---- First-run setup ----

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    error: Option<String>,
}

/// First-run admin account creation. Unreachable once a user exists
/// (the middleware redirects everything to /setup only while the user
/// table is empty).
async fn setup_page(State(state): State<AppState>) -> Result<Response, AppError> {
    let count = state.db.user_count().map_err(AppError::from)?;
    if count > 0 {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(SetupTemplate { error: None }.into_response())
}

#[derive(Deserialize)]
struct SetupForm {
    username: String,
    password: String,
    confirm: String,
}

async fn setup_submit(
    State(state): State<AppState>,
    Form(form): Form<SetupForm>,
) -> Result<Response, AppError> {
    let db = &state.db;
    if db.user_count().map_err(AppError::from)? > 0 {
        return Ok(Redirect::to("/").into_response());
    }

    let username = form.username.trim().to_string();
    let error = if username.is_empty() {
        Some("Choose a username.".to_string())
    } else if form.password.len() < 8 {
        Some("Password must be at least 8 characters.".to_string())
    } else if form.password != form.confirm {
        Some("Passwords do not match.".to_string())
    } else {
        None
    };
    if let Some(error) = error {
        return Ok(SetupTemplate { error: Some(error) }.into_response());
    }

    let hash = auth::hash_password(&form.password).map_err(AppError::from)?;
    db.create_user(&username, &hash).map_err(AppError::from)?;
    Ok(Redirect::to("/login").into_response())
}

// ---- Settings ----

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    settings: Settings,
    saved: bool,
    tokens: Vec<ApiTokenListItem>,
}

/// Token row for the settings page, with human-readable dates.
struct ApiTokenListItem {
    id: String,
    name: String,
    prefix: String,
    last4: String,
    scopes: String,
    created: String,
    last_used: String,
    expires: String,
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "—".to_string())
}

async fn settings_page(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<SettingsTemplate, AppError> {
    let settings = state.db.get_settings().map_err(AppError::from)?;
    let tokens = state
        .db
        .list_api_tokens()
        .map_err(AppError::from)?
        .iter()
        .map(|t| ApiTokenListItem {
            id: t.id.clone(),
            name: t.name.clone(),
            prefix: t.prefix.clone(),
            last4: t.last4.clone(),
            scopes: t.scopes.join(", "),
            created: fmt_ts(t.created_at),
            last_used: t
                .last_used_at
                .map(fmt_ts)
                .unwrap_or_else(|| "—".to_string()),
            expires: t
                .expires_at
                .map(fmt_ts)
                .unwrap_or_else(|| "never".to_string()),
        })
        .collect();
    let _ = user;
    Ok(SettingsTemplate {
        settings,
        saved: query.contains_key("saved"),
        tokens,
    })
}

#[derive(Deserialize)]
struct SettingsForm {
    /// Optional: absent when the /settings page no longer renders the
    /// timer fields (they live in the Timer settings modal now).
    pomodoro_minutes: Option<u32>,
    short_break_minutes: Option<u32>,
    long_break_minutes: Option<u32>,
    long_break_every: Option<u32>,
    ding_enabled: Option<String>,
    notifications_enabled: Option<String>,
    interrupt_reasons: Option<String>,
}

async fn settings_submit(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Form(form): Form<SettingsForm>,
) -> Result<Response, AppError> {
    let clamp_minutes = |value: u32, fallback: u32| {
        if (1..=180).contains(&value) {
            value
        } else {
            fallback
        }
    };
    let defaults = Settings::default();
    let current = state.db.get_settings().map_err(AppError::from)?;
    let reasons: Vec<String> = form
        .interrupt_reasons
        .as_deref()
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let settings = Settings {
        pomodoro_minutes: form
            .pomodoro_minutes
            .map(|v| clamp_minutes(v, defaults.pomodoro_minutes))
            .unwrap_or(current.pomodoro_minutes),
        short_break_minutes: form
            .short_break_minutes
            .map(|v| clamp_minutes(v, defaults.short_break_minutes))
            .unwrap_or(current.short_break_minutes),
        long_break_minutes: form
            .long_break_minutes
            .map(|v| clamp_minutes(v, defaults.long_break_minutes))
            .unwrap_or(current.long_break_minutes),
        long_break_every: form
            .long_break_every
            .map(|v| v.clamp(1, 12))
            .unwrap_or(current.long_break_every),
        ding_enabled: form.ding_enabled.as_deref() == Some("on"),
        notifications_enabled: form.notifications_enabled.as_deref() == Some("on"),
        interrupt_reasons: if reasons.is_empty() {
            current.interrupt_reasons
        } else {
            reasons
        },
        // Preserved: edited via the Timer settings modal (PUT /api/settings).
        ticking_mode: current.ticking_mode,
        alarm_sound: current.alarm_sound,
        alarm_volume: current.alarm_volume,
        points_volume: current.points_volume,
        sounds_enabled: current.sounds_enabled,
        pip_enabled: current.pip_enabled,
        break_activities: current.break_activities,
        favorite_boards: current.favorite_boards,
    };
    state
        .db
        .update_settings(&settings)
        .map_err(AppError::from)?;
    Ok(Redirect::to("/settings?saved=1").into_response())
}

/// Settings as JSON for the timer JavaScript.
#[utoipa::path(
    get,
    path = "/api/settings",
    tag = "Settings",
    responses(
        (status = 200, description = "App settings", body = Settings),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn api_settings(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Settings>, AppError> {
    Ok(Json(state.db.get_settings().map_err(AppError::from)?))
}

/// Partial timer-settings update from the Timer settings modal.
/// Only the fields present in the JSON body are changed.
#[derive(Debug, Deserialize, ToSchema)]
struct SettingsUpdate {
    pomodoro_minutes: Option<u32>,
    short_break_minutes: Option<u32>,
    long_break_minutes: Option<u32>,
    long_break_every: Option<u32>,
    ticking_mode: Option<String>,
    alarm_sound: Option<String>,
    alarm_volume: Option<u32>,
    points_volume: Option<u32>,
    sounds_enabled: Option<bool>,
    pip_enabled: Option<bool>,
    notifications_enabled: Option<bool>,
    interrupt_reasons: Option<Vec<String>>,
    /// Break activities for the Timer settings modal (KanbanFlow parity).
    /// Replaces the whole list; entries are cleaned (blank names dropped,
    /// daily_goal clamped to 1..=100, empty ids regenerated).
    break_activities: Option<Vec<BreakActivity>>,
    /// Board ids pinned in the Boards sidebar Favorites (KF-088).
    /// Replaces the whole list; unknown board ids are dropped.
    favorite_boards: Option<Vec<String>>,
}

#[utoipa::path(
    put,
    path = "/api/settings",
    tag = "Settings",
    request_body = SettingsUpdate,
    responses(
        (status = 200, description = "Updated app settings", body = Settings),
        (status = 400, description = "Invalid value"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn api_update_settings(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Json(patch): Json<SettingsUpdate>,
) -> Result<Json<Settings>, AppError> {
    let bad = |msg: &str| AppError::bad_request(msg);
    let mut settings = state.db.get_settings().map_err(AppError::from)?;
    let clamp_minutes = |v: u32| v.clamp(1, 180);
    if let Some(v) = patch.pomodoro_minutes {
        settings.pomodoro_minutes = clamp_minutes(v);
    }
    if let Some(v) = patch.short_break_minutes {
        settings.short_break_minutes = clamp_minutes(v);
    }
    if let Some(v) = patch.long_break_minutes {
        settings.long_break_minutes = clamp_minutes(v);
    }
    if let Some(v) = patch.long_break_every {
        settings.long_break_every = v.clamp(1, 12);
    }
    if let Some(ref v) = patch.ticking_mode {
        match v.as_str() {
            "always" | "timer_start" | "never" => settings.ticking_mode = v.clone(),
            _ => return Err(bad("invalid ticking_mode")),
        }
    }
    if let Some(ref v) = patch.alarm_sound {
        match v.as_str() {
            "bell" | "chime" | "beeps" | "blip" | "glass" | "microwave" | "egg_timer"
            | "grandpa_clock" | "melodic" => settings.alarm_sound = v.clone(),
            _ => return Err(bad("invalid alarm_sound")),
        }
    }
    if let Some(v) = patch.alarm_volume {
        settings.alarm_volume = v.min(100);
    }
    if let Some(v) = patch.points_volume {
        settings.points_volume = v.min(100);
    }
    if let Some(v) = patch.sounds_enabled {
        settings.sounds_enabled = v;
    }
    if let Some(v) = patch.pip_enabled {
        settings.pip_enabled = v;
    }
    if let Some(v) = patch.notifications_enabled {
        settings.notifications_enabled = v;
    }
    if let Some(reasons) = patch.interrupt_reasons {
        let cleaned: Vec<String> = reasons
            .into_iter()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect();
        if !cleaned.is_empty() {
            settings.interrupt_reasons = cleaned;
        }
    }
    if let Some(activities) = patch.break_activities {
        settings.break_activities = activities
            .into_iter()
            .map(|mut activity| {
                activity.id = activity.id.trim().to_string();
                if activity.id.is_empty() {
                    activity.id = uuid::Uuid::new_v4().to_string();
                }
                activity.name = activity.name.trim().to_string();
                activity.description = activity.description.trim().to_string();
                activity.daily_goal = activity.daily_goal.clamp(1, 100);
                activity
            })
            .filter(|activity| !activity.name.is_empty())
            .collect();
    }
    if let Some(favorites) = patch.favorite_boards {
        let mut seen = std::collections::HashSet::new();
        let mut kept = Vec::new();
        for id in favorites {
            let id = id.trim().to_string();
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            if state.db.board_exists(&id).map_err(AppError::from)? {
                kept.push(id);
            }
        }
        settings.favorite_boards = kept;
    }
    state
        .db
        .update_settings(&settings)
        .map_err(AppError::from)?;
    Ok(Json(settings))
}

// ---- Agent API tokens ----

/// Public view of a token: everything except the secret and its hash.
#[derive(serde::Serialize, ToSchema)]
struct ApiTokenView {
    id: String,
    name: String,
    prefix: String,
    last4: String,
    scopes: Vec<String>,
    created_at: i64,
    last_used_at: Option<i64>,
    expires_at: Option<i64>,
}

impl From<&ApiTokenRecord> for ApiTokenView {
    fn from(record: &ApiTokenRecord) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
            prefix: record.prefix.clone(),
            last4: record.last4.clone(),
            scopes: record.scopes.clone(),
            created_at: record.created_at,
            last_used_at: record.last_used_at,
            expires_at: record.expires_at,
        }
    }
}

#[derive(serde::Serialize, ToSchema)]
struct CreateTokenResponse {
    #[serde(flatten)]
    token: ApiTokenView,
    /// The raw token, shown exactly once. Never stored server-side.
    raw_token: String,
}

#[derive(Deserialize, ToSchema)]
struct CreateTokenInput {
    name: String,
    scopes: Vec<String>,
    /// Days until expiry; None or <= 0 means no expiry.
    expires_in_days: Option<i64>,
}

/// A token must never be able to mint new tokens: creation, listing, and
/// revocation require the browser session cookie.
fn require_cookie_auth(user: &AuthUser) -> Result<(), AppError> {
    if user.via_api_token {
        Err(AppError::forbidden(
            "token management requires the browser session; API tokens cannot mint new tokens",
        ))
    } else {
        Ok(())
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/tokens",
    tag = "Tokens",
    request_body = CreateTokenInput,
    responses(
        (status = 200, description = "Created token (raw_token shown once)", body = CreateTokenResponse),
        (status = 400, description = "Invalid name or scopes"),
        (status = 403, description = "Requires the browser session cookie; API tokens cannot mint tokens"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_api_token(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CreateTokenResponse>, AppError> {
    require_cookie_auth(&user)?;
    let input: CreateTokenInput = parse_body(&headers, body).await?;
    let name = input.name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err(AppError::bad_request("token name must be 1-80 characters"));
    }
    if input.scopes.is_empty() || !input.scopes.iter().all(|s| s == "read" || s == "write") {
        return Err(AppError::bad_request(
            "scopes must be a non-empty subset of [\"read\", \"write\"]",
        ));
    }
    let mut scopes = input.scopes.clone();
    scopes.sort();
    scopes.dedup();
    let expires_at = match input.expires_in_days {
        Some(days) if days > 0 => Some(chrono::Utc::now().timestamp() + days.min(3650) * 86400),
        _ => None,
    };
    let (record, raw) = auth::new_api_token_record(name, scopes, expires_at);
    state.db.create_api_token(&record).map_err(AppError::from)?;
    Ok(Json(CreateTokenResponse {
        token: ApiTokenView::from(&record),
        raw_token: raw,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/tokens",
    tag = "Tokens",
    responses(
        (status = 200, description = "Active tokens (no secrets)", body = Vec<ApiTokenView>),
        (status = 403, description = "Requires the browser session cookie"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_api_tokens(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<Vec<ApiTokenView>>, AppError> {
    require_cookie_auth(&user)?;
    let tokens = state.db.list_api_tokens().map_err(AppError::from)?;
    Ok(Json(tokens.iter().map(ApiTokenView::from).collect()))
}

#[utoipa::path(
    delete,
    path = "/api/v1/auth/tokens/{id}",
    tag = "Tokens",
    params(("id" = String, Path, description = "Token id")),
    responses(
        (status = 200, description = "Revoked token id", body = RevokeResult),
        (status = 403, description = "Requires the browser session cookie"),
        (status = 404, description = "No such token"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn revoke_api_token(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<RevokeResult>, AppError> {
    require_cookie_auth(&user)?;
    let removed = state.db.delete_api_token(&id).map_err(AppError::from)?;
    if !removed {
        return Err(AppError::not_found("no such token"));
    }
    Ok(Json(RevokeResult { revoked: id }))
}

// ---- Agent discovery ----
//
// Curated documentation served from files at the repo root so humans
// browsing the repo see the same content agents fetch over HTTP.

/// `GET /agents.md` — the primary agent guide: auth, base URL, worked
/// examples, error model. Served as Markdown, no auth required.
#[utoipa::path(
    get,
    path = "/agents.md",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "Agent guide (Markdown)", content_type = "text/markdown"),
    ),
)]
async fn agents_md() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/markdown; charset=utf-8")],
        include_str!("../agents.md"),
    )
}

/// `GET /agents/skill.md` — the same guide in Agent Skills format
/// (YAML frontmatter + Markdown) for installable cross-agent skills.
#[utoipa::path(
    get,
    path = "/agents/skill.md",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "Agent Skills guide (Markdown)", content_type = "text/markdown"),
    ),
)]
async fn agents_skill_md() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/markdown; charset=utf-8")],
        include_str!("../agents/skill.md"),
    )
}

/// `GET /.well-known/agents.json` — static pointer to the discovery docs.
#[utoipa::path(
    get,
    path = "/.well-known/agents.json",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "Pointer to the discovery documents", content_type = "application/json"),
    ),
)]
async fn agents_json() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/json")],
        include_str!("../agents.json"),
    )
}

/// `GET /api/v1/openapi.json` — machine-readable contract for the JSON API,
/// generated from the `#[utoipa::path]` annotations on the handlers.
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "This document (generated OpenAPI)", content_type = "application/json"),
    ),
)]
async fn openapi_json() -> Result<impl IntoResponse, AppError> {
    let spec = ApiDoc::openapi()
        .to_json()
        .map_err(|error| AppError::internal(format!("failed to serialize OpenAPI: {error}")))?;
    Ok(([(CONTENT_TYPE, "application/json")], spec))
}

/// `GET /api/v1/version` — public build identity for agents and health
/// checks: the crate version and the git SHA baked in at build time
/// (`CHIPFLOW_BUILD_SHA`, or `"unknown"` when the build didn't set it).
#[derive(serde::Serialize, ToSchema)]
struct VersionInfo {
    version: String,
    build_sha: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "Build version and commit SHA", body = VersionInfo),
    ),
)]
async fn version_info() -> Json<VersionInfo> {
    Json(VersionInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_sha: option_env!("CHIPFLOW_BUILD_SHA")
            .unwrap_or("unknown")
            .to_string(),
    })
}

// ---- Timer ----

/// Timer status for the header pill and popup, with everything the
/// JavaScript needs to render without another round trip.
#[derive(serde::Serialize, ToSchema)]
struct TimerStatusView {
    active: bool,
    /// 'idle' when no timer is running, otherwise the mode string (e.g. 'pomodoro', 'stopwatch', 'short_break').
    phase: String,
    mode: Option<String>,
    mode_title: Option<String>,
    task_id: Option<String>,
    task_name: Option<String>,
    /// URL to the task, if a task is attached.
    task_url: Option<String>,
    started_at: Option<i64>,
    duration_secs: Option<u64>,
    /// Seconds remaining (computed from started_at + duration_secs).
    remaining_seconds: Option<i64>,
    /// Total duration in seconds (= duration_secs).
    total_seconds: Option<u64>,
    /// Number of pomodoros completed today.
    pomodoro_count: u32,
}

fn timer_status_view(db: &Db, timer: Option<ActiveTimer>) -> Result<TimerStatusView, AppError> {
    // Count today's completed pomodoros for the pill display.
    let pomodoro_count = db
        .entries_today()
        .map(|entries| {
            entries
                .iter()
                .filter(|(entry, _)| entry.kind == "pomodoro")
                .count() as u32
        })
        .unwrap_or(0);

    match timer {
        None => Ok(TimerStatusView {
            active: false,
            phase: "idle".to_string(),
            mode: None,
            mode_title: None,
            task_id: None,
            task_name: None,
            task_url: None,
            started_at: None,
            duration_secs: None,
            remaining_seconds: None,
            total_seconds: None,
            pomodoro_count,
        }),
        Some(timer) => {
            let task_name = match timer.task_id.as_deref() {
                Some(id) => db
                    .get_task(id)
                    .map_err(AppError::from)?
                    .map(|task| task.name),
                None => None,
            };
            // If the task was deleted mid-session, drop the timer quietly.
            if timer.task_id.is_some() && task_name.is_none() {
                db.clear_active_timer().map_err(AppError::from)?;
                return Ok(TimerStatusView {
                    active: false,
                    phase: "idle".to_string(),
                    mode: None,
                    mode_title: None,
                    task_id: None,
                    task_name: None,
                    task_url: None,
                    started_at: None,
                    duration_secs: None,
                    remaining_seconds: None,
                    total_seconds: None,
                    pomodoro_count,
                });
            }
            let now = chrono::Utc::now().timestamp();
            let remaining_seconds = timer.duration_secs.map(|dur| {
                let elapsed = now - timer.started_at;
                (dur as i64 - elapsed).max(0)
            });
            let task_url = timer
                .task_id
                .as_deref()
                .map(|id| format!("/api/tasks/{id}"));
            let phase = timer.mode.as_str().to_string();
            Ok(TimerStatusView {
                active: true,
                phase,
                mode: Some(timer.mode.as_str().to_string()),
                mode_title: Some(timer.mode.title().to_string()),
                task_id: timer.task_id,
                task_name,
                task_url,
                started_at: Some(timer.started_at),
                duration_secs: timer.duration_secs,
                remaining_seconds,
                total_seconds: timer.duration_secs,
                pomodoro_count,
            })
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/timer/status",
    tag = "Timer",
    responses(
        (status = 200, description = "Current timer status", body = TimerStatusView),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn timer_status(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<TimerStatusView>, AppError> {
    let timer = state.db.get_active_timer().map_err(AppError::from)?;
    Ok(Json(timer_status_view(&state.db, timer)?))
}

#[derive(Deserialize, ToSchema)]
struct TimerStartInput {
    task_id: Option<String>,
    mode: String,
}

/// Start a timer, replacing any active one. A replaced session of 20+
/// seconds is logged as interrupted ("Switched task") so no work time
/// silently vanishes; shorter ones are discarded.
#[utoipa::path(
    post,
    path = "/api/timer/start",
    tag = "Timer",
    request_body = TimerStartInput,
    responses(
        (status = 200, description = "Timer status after starting", body = TimerStatusView),
        (status = 400, description = "Unknown timer mode"),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn timer_start(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TimerStatusView>, AppError> {
    let input: TimerStartInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let mode: TimerMode = input.mode.parse().map_err(AppError::bad_request)?;
    if let Some(task_id) = input.task_id.as_deref() {
        if db.get_task(task_id).map_err(AppError::from)?.is_none() {
            return Err(AppError::not_found("task not found"));
        }
    }

    // Retire any running timer first, with the same logging rules as stop.
    if let Some(old) = db.get_active_timer().map_err(AppError::from)? {
        log_timer_session(db, &old, false, Some("Switched task"), &user.username)?;
    }

    let settings = db.get_settings().map_err(AppError::from)?;
    let duration_secs = match mode {
        TimerMode::Pomodoro => Some(u64::from(settings.pomodoro_minutes) * 60),
        TimerMode::ShortBreak => Some(u64::from(settings.short_break_minutes) * 60),
        TimerMode::LongBreak => Some(u64::from(settings.long_break_minutes) * 60),
        TimerMode::Stopwatch => None,
    };
    let timer = ActiveTimer {
        task_id: input.task_id,
        mode,
        started_at: chrono::Utc::now().timestamp(),
        duration_secs,
    };
    db.set_active_timer(&timer).map_err(AppError::from)?;
    Ok(Json(timer_status_view(db, Some(timer))?))
}

#[derive(Deserialize, ToSchema)]
struct TimerStopInput {
    /// True when the timer ran to zero on its own. Defaults to false for manual stops.
    #[serde(default)]
    completed: bool,
    /// Why a pomodoro was stopped early ("Why did you stop?").
    reason: Option<String>,
}

/// Stop the active timer and log the session. Sessions under 20 seconds
/// are discarded (KanbanFlow does the same). Durations are minute-truncated,
/// not rounded. A finished pomodoro bumps the task's pomodori counter; an
/// early stop logs its time and records an interruption but does not count
/// as a Pomodoro (KF-200).
#[utoipa::path(
    post,
    path = "/api/timer/stop",
    tag = "Timer",
    request_body = TimerStopInput,
    responses(
        (status = 200, description = "Stop result with logged minutes", body = TimerStopResult),
        (status = 404, description = "No active timer"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn timer_stop(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TimerStopResult>, AppError> {
    let input: TimerStopInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let timer = db
        .get_active_timer()
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("no active timer"))?;
    let logged = log_timer_session(
        db,
        &timer,
        input.completed,
        input.reason.as_deref(),
        &user.username,
    )?;

    // KF-106: Do NOT auto-insert custom "why did you stop?" reasons here.
    // KanbanFlow's verified path is the Interruptions tab's "Add reason"
    // button; automatic addition on timer stop was never verified.
    // "Task done" is never a configured reason (KF-011/KF-077).

    Ok(Json(TimerStopResult {
        discarded: logged.is_none(),
        minutes: logged,
        completed: input.completed,
    }))
}

/// Shared stop logic for timer_stop and timer_start's replacement path.
/// Returns the logged minutes, or None when the session was discarded.
fn log_timer_session(
    db: &Db,
    timer: &ActiveTimer,
    completed: bool,
    reason: Option<&str>,
    actor: &str,
) -> Result<Option<i64>, AppError> {
    let elapsed = (chrono::Utc::now().timestamp() - timer.started_at).max(0);
    db.clear_active_timer().map_err(AppError::from)?;
    if elapsed < 20 {
        return Ok(None);
    }
    // KF-019: keep second precision for sub-minute durations ("31s", not a 1m floor).
    let seconds = elapsed;
    let minutes = elapsed / 60;
    let interrupted =
        !completed && matches!(timer.mode, TimerMode::Pomodoro | TimerMode::Stopwatch);

    if let Some(task_id) = timer.task_id.as_deref() {
        db.create_entry_full(
            Some(task_id),
            minutes,
            seconds,
            "",
            timer.mode.entry_kind(),
            interrupted,
            reason,
            &[],
            actor,
        )
        .map_err(AppError::from)?;
        let kind_label = match timer.mode {
            TimerMode::Pomodoro => "Pomodoro",
            TimerMode::Stopwatch => "Stopwatch",
            TimerMode::ShortBreak => "Short break",
            TimerMode::LongBreak => "Long break",
        };
        log_history(
            db,
            task_id,
            "time_logged",
            &format!("{kind_label} session logged ({minutes}m)"),
            actor,
        );
        match timer.mode {
            TimerMode::Pomodoro if completed => {
                db.record_pomodoro_complete(task_id)
                    .map_err(AppError::from)?;
            }
            TimerMode::Pomodoro => {
                // KF-200: only a pomodoro that ran to zero counts as a
                // Pomodoro (card tomato). A stopped/interrupted session logs
                // its time and records an interruption, but must not bump the
                // pomodori counter.
                db.record_interruption(task_id).map_err(AppError::from)?;
            }
            _ => {}
        }
    }
    Ok(Some(minutes))
}

#[derive(Deserialize, ToSchema)]
struct TimerRetargetInput {
    task_id: Option<String>,
}

/// Point the active timer at a different task ("Change task").
#[utoipa::path(
    post,
    path = "/api/timer/retarget",
    tag = "Timer",
    request_body = TimerRetargetInput,
    responses(
        (status = 200, description = "Timer status after retargeting", body = TimerStatusView),
        (status = 404, description = "No active timer or task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn timer_retarget(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TimerStatusView>, AppError> {
    let input: TimerRetargetInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if let Some(task_id) = input.task_id.as_deref() {
        if db.get_task(task_id).map_err(AppError::from)?.is_none() {
            return Err(AppError::not_found("task not found"));
        }
    }
    let mut timer = db
        .get_active_timer()
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("no active timer"))?;
    timer.task_id = input.task_id;
    db.set_active_timer(&timer).map_err(AppError::from)?;
    Ok(Json(timer_status_view(db, Some(timer))?))
}

#[derive(serde::Serialize, ToSchema)]
struct TodayEntryView {
    id: String,
    task_name: String,
    minutes: i64,
    kind: String,
    kind_label: String,
    started_display: String,
    ended_display: String,
    interrupted: bool,
    interrupt_reason: Option<String>,
}

/// Today's logged sessions for the timer popup's "Today" list.
#[utoipa::path(
    get,
    path = "/api/timer/today",
    tag = "Timer",
    responses(
        (status = 200, description = "Today's logged sessions", body = Vec<TodayEntryView>),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn timer_today(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Vec<TodayEntryView>>, AppError> {
    let rows = state.db.entries_today().map_err(AppError::from)?;
    let entries = rows
        .into_iter()
        .map(|(entry, task_name)| {
            let kind_label = match entry.kind.as_str() {
                "pomodoro" => "Pomodoro",
                "stopwatch" => "Stopwatch",
                "short_break" => "Short break",
                "long_break" => "Long break",
                _ => "Time",
            }
            .to_string();
            let started_display = DateTime::parse_from_rfc3339(&entry.started_at)
                .map(|dt| dt.with_timezone(&Local).format("%-I:%M %p").to_string())
                .unwrap_or(entry.started_at.clone());
            let ended_display = DateTime::parse_from_rfc3339(&entry.started_at)
                .map(|dt| {
                    (dt + chrono::Duration::minutes(entry.minutes))
                        .with_timezone(&Local)
                        .format("%-I:%M %p")
                        .to_string()
                })
                .unwrap_or_default();
            TodayEntryView {
                id: entry.id,
                task_name,
                minutes: entry.minutes,
                kind: entry.kind,
                kind_label,
                started_display,
                ended_display,
                interrupted: entry.interrupted,
                interrupt_reason: entry.interrupt_reason,
            }
        })
        .collect();
    Ok(Json(entries))
}

// ---- Generated OpenAPI ----

/// Adds the `bearerAuth` HTTP bearer security scheme, matching the
/// `cf_...` API tokens described in `/agents.md`.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("cf_...")
                    .build(),
            ),
        );
    }
}

/// Code-generated OpenAPI served at `GET /api/v1/openapi.json`. The
/// `#[utoipa::path]` annotations on the handlers above are the source of
/// truth — the spec tracks the implementation by construction.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "ChipFlow API",
        version = "1.0.0",
        description = "JSON API for the ChipFlow self-hosted kanban + pomodoro app. Full agent guide at /agents.md. Errors are plain-text bodies with 4xx/5xx statuses (no envelope). Timestamps are Unix seconds (UTC). Durations truncate to whole minutes."
    ),
    paths(
        list_tasks,
        create_task,
        get_task,
        update_task,
        delete_task,
        move_task,
        watch_task,
        create_subtask,
        reorder_subtasks,
        update_subtask,
        delete_subtask,
        list_board_labels,
        create_comment,
        list_comments,
        delete_comment,
        upload_attachment,
        download_attachment,
        delete_attachment,
        time_log_view,
        history_view,
        delete_time_entry,
        list_members,
        log_time,
        get_task_time,
        get_time_entry,
        update_time_entry,
        create_manual_time,
        api_timer_log,
        api_time_spent,
        api_timer_statistics,
        timer_start,
        timer_status,
        timer_stop,
        timer_retarget,
        timer_today,
        api_settings,
        api_update_settings,
        delete_board_api,
        update_board_config,
        board_delete_page,
        create_column,
        update_column,
        move_column,
        delete_column,
        create_swimlane,
        update_swimlane,
        move_swimlane,
        delete_swimlane,
        list_boards,
        list_board_columns,
        create_board,
        new_board_page,
        archived_boards_page,
        save_board_as_template,
        list_templates,
        delete_template,
        board_colors_page,
        board_settings_page,
        board_settings_layout_page,
        board_settings_task_page,
        board_settings_advanced_page,
        board_settings_api_page,
        board_settings_email_page,
        list_board_colors,
        create_board_color,
        update_board_color,
        delete_board_color,
        copy_board_colors,
        create_api_token,
        list_api_tokens,
        revoke_api_token,
        agents_md,
        agents_skill_md,
        agents_json,
        openapi_json,
        version_info,
    ),
    components(
        schemas(
            TaskNameItem,
            IdResult,
            IdMinutesResult,
            TimeEntryDetail,
            TimerLogEntry,
            TimerLogPage,
            DayTotal,
            TaskTime,
            TimeSpentReport,
            SettingsUpdate,
            ReasonCount,
            DayPomodori,
            TimerStatisticsReport,
            TimerStopResult,
            TodayEntryView,
            TimerStatusView,
            ApiTokenView,
            CreateTokenResponse,
            RevokeResult,
            Settings,
            CreateTaskInput,
            CreateTaskResponse,
            UpdateTaskInput,
            MoveTaskInput,
            WatchTaskInput,
            WatchTaskResponse,
            CreateSubtaskInput,
            ReorderSubtasksInput,
            UpdateSubtaskInput,
            SubtaskDetail,
            CreateCommentInput,
            UploadAttachmentInput,
            TaskComment,
            TaskAttachment,
            MemberDetail,
            TaskDetail,
            LogTimeInput,
            ManualTimeInput,
            UpdateTimeEntryInput,
            CreateColumnInput,
            UpdateColumnInput,
            MoveColumnInput,
            CreateSwimlaneInput,
            UpdateSwimlaneInput,
            CopyColorsInput,
            CountResult,
            BreakActivity,
            MoveSwimlaneInput,
            TimerStartInput,
            TimerStopInput,
            TimerRetargetInput,
            TimerLogQuery,
            TimeSpentQuery,
            CreateTokenInput,
            BoardListItem,
            ColumnListItem,
            VersionInfo,
            TemplateListItem,
            ColorView,
            CreateBoardInput,
            SaveTemplateInput,
            CreateColorInput,
            BoardConfigInput,
            BoardConfigView,
            UpdateColorInput,
        )
    ),
    modifiers(&SecurityAddon),
    security(("bearerAuth" = [])),
    tags(
        (name = "Tasks", description = "Kanban tasks"),
        (name = "Time", description = "Time entries"),
        (name = "Timer", description = "Pomodoro / stopwatch timer"),
        (name = "Boards", description = "Boards"),
        (name = "Templates", description = "Board templates"),
        (name = "Colors", description = "Per-board task colors"),
        (name = "Columns", description = "Board columns"),
        (name = "Swimlanes", description = "Board swimlanes"),
        (name = "Settings", description = "App settings"),
        (name = "Tokens", description = "Agent API tokens"),
        (name = "Discovery", description = "Agent discovery documents"),
    ),
)]
struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::ApiDoc;
    use super::{
        estimate_hours_to_size, format_added, format_due, format_due_card, format_due_modal,
        format_estimate_hours, format_relative_day, format_time_kf, is_overdue,
        normalize_due_input, normalize_estimate_hours, ColorView, TaskCardDisplay,
        UpdateColumnInput, UpdateTaskInput,
    };
    use crate::models::TaskRow;
    use chrono::{DateTime, Duration, Local, Timelike};
    use utoipa::OpenApi;

    /// KF-053: the card-property config defaults to KanbanFlow's (all hide
    /// except due dates, which defaults to "Show active due in 7 days").
    #[test]
    fn card_display_defaults_match_kanbanflow() {
        let d = TaskCardDisplay::from_config_json("{}");
        assert!(!d.description);
        assert!(!d.labels);
        assert!(!d.subtasks);
        assert!(d.due_dates);
        assert_eq!(d.due_within_days, Some(7));
        assert!(!d.created);
        assert!(!d.added);
    }

    #[test]
    fn card_display_parses_show_values() {
        let d = TaskCardDisplay::from_config_json(
            r#"{"prop_description":"show","prop_labels":"show","prop_subtasks":"show","prop_due_dates":"show","prop_created":"show","prop_added":"show"}"#,
        );
        assert!(d.description);
        assert!(d.labels);
        assert!(d.subtasks);
        assert!(d.due_dates);
        assert_eq!(d.due_within_days, None);
        assert!(d.created);
        assert!(d.added);
    }

    #[test]
    fn card_display_hide_due_dates_disables_them() {
        let d = TaskCardDisplay::from_config_json(r#"{"prop_due_dates":"hide"}"#);
        assert!(!d.due_dates);
    }

    /// KF-053: "active_7d" shows overdue and near-future dues, hides
    /// far-future ones; "show" shows everything; garbage is None.
    #[test]
    fn format_due_honors_seven_day_window() {
        // Noon UTC stays Jan 01 in every timezone (avoids midnight edge cases).
        let past = "2020-01-01T12:00:00Z";
        assert_eq!(format_due(past, Some(7)).as_deref(), Some("Jan 01"));
        assert_eq!(format_due(past, None).as_deref(), Some("Jan 01"));

        let far = "2999-01-01T12:00:00Z";
        assert_eq!(format_due(far, Some(7)), None);
        assert_eq!(format_due(far, None).as_deref(), Some("Jan 01"));

        assert_eq!(format_due("not-a-date", Some(7)), None);
    }

    /// KF-224: near-term dues render as weekday + time, farther ones as
    /// absolute day + month + time; unparseable is None.
    #[test]
    fn format_due_card_weekday_near_absolute_far() {
        // 2 days out -> weekday form ("Friday 5:00 PM" style).
        let near = (Local::now() + Duration::days(2))
            .format("%Y-%m-%dT17:00:00")
            .to_string();
        let rendered = format_due_card(&format!("{near}Z")).expect("parses");
        assert!(rendered.contains("5:00 PM"), "got {rendered}");
        assert!(
            !rendered.chars().next().unwrap().is_ascii_digit(),
            "weekday-first, got {rendered}"
        );

        // 30 days out -> absolute form ("30 October 5:00 PM" style).
        let far = (Local::now() + Duration::days(30))
            .format("%Y-%m-%dT17:00:00")
            .to_string();
        let rendered = format_due_card(&format!("{far}Z")).expect("parses");
        assert!(rendered.contains("5:00 PM"), "got {rendered}");
        assert!(
            rendered.chars().next().unwrap().is_ascii_digit(),
            "day-first, got {rendered}"
        );

        assert_eq!(format_due_card("not-a-date"), None);
    }

    /// KF-259: modal DUE DATE row — same ±7-day weekday rule as the card's
    /// due line, " (Done)" appended only when due_done.
    #[test]
    fn format_due_modal_relative_and_done_marker() {
        let rfc3339 = |days: i64| {
            format!(
                "{}Z",
                (Local::now() + Duration::days(days)).format("%Y-%m-%dT17:00:00")
            )
        };

        // Within 7 days, not done: weekday form, no marker.
        let near = format_due_modal(&rfc3339(1), false).expect("parses");
        assert!(near.contains("5:00 PM"), "got {near}");
        assert!(
            !near.chars().next().unwrap().is_ascii_digit(),
            "weekday-first, got {near}"
        );
        assert!(
            !near.contains("(Done)"),
            "no marker when not done, got {near}"
        );

        // Within 7 days, done: weekday form + " (Done)".
        let near_done = format_due_modal(&rfc3339(1), true).expect("parses");
        assert!(near_done.ends_with("5:00 PM (Done)"), "got {near_done}");
        assert!(
            !near_done.chars().next().unwrap().is_ascii_digit(),
            "weekday-first, got {near_done}"
        );

        // 30 days out, done: absolute form + " (Done)".
        let far_done = format_due_modal(&rfc3339(30), true).expect("parses");
        assert!(far_done.contains("5:00 PM"), "got {far_done}");
        assert!(
            far_done.chars().next().unwrap().is_ascii_digit(),
            "day-first, got {far_done}"
        );
        assert!(far_done.ends_with("(Done)"), "got {far_done}");

        // The 7-day boundary is strict (same as format_due_card): strictly
        // inside 7 days renders the weekday, strictly outside absolute.
        // Offset-aware instants keep this deterministic regardless of the
        // local UTC offset.
        let exact = |delta: Duration| {
            (Local::now() + delta)
                .format("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        };
        let inside = format_due_modal(&exact(Duration::days(7) - Duration::hours(1)), false)
            .expect("parses");
        assert!(
            !inside.chars().next().unwrap().is_ascii_digit(),
            "inside 7 days renders weekday, got {inside}"
        );
        let outside = format_due_modal(&exact(Duration::days(7) + Duration::hours(1)), false)
            .expect("parses");
        assert!(
            outside.chars().next().unwrap().is_ascii_digit(),
            "outside 7 days renders absolute, got {outside}"
        );

        // Unparseable input: None (shared format_datetime untouched).
        assert_eq!(format_due_modal("not-a-date", false), None);
        assert_eq!(format_due_modal("not-a-date", true), None);
    }

    /// KF-224: "{spent} / {estimate}", spent-only, or None.
    #[test]
    fn format_time_kf_spent_and_estimate() {
        assert_eq!(
            format_time_kf(150, Some(8.0)).as_deref(),
            Some("2h 30m / 8h")
        );
        assert_eq!(format_time_kf(0, Some(4.0)).as_deref(), Some("0h / 4h"));
        assert_eq!(format_time_kf(45, None).as_deref(), Some("45m"));
        assert_eq!(format_time_kf(0, None), None);
    }

    /// KF-053: `column_added_at` falls back to the creation date for rows
    /// written before the field existed.
    #[test]
    fn format_added_falls_back_to_created_at() {
        let row = TaskRow {
            id: "t".into(),
            column_id: "c".into(),
            swimlane_id: None,
            name: "n".into(),
            description: String::new(),
            size: 1,
            color_id: None,
            position: 0.0,
            pomodori_completed: 0,
            interruptions: 0,
            total_minutes: 0,
            created_at: "2026-09-20T10:00:00Z".into(),
            completed_at: None,
            due_at: None,
            due_repeat: None,
            due_done: false,
            estimate_hours: None,
            labels: Vec::new(),
            subtasks: Vec::new(),
            member_ids: Vec::new(),
            watched: false,
            grouping_date: None,
            column_added_at: None,
            comments: Vec::new(),
            attachments: Vec::new(),
            history: Vec::new(),
        };
        assert_eq!(format_added(&row), "Sep 20");

        let row2 = TaskRow {
            column_added_at: Some("2026-09-29T10:00:00Z".into()),
            ..row
        };
        assert_eq!(format_added(&row2), "Sep 29");
    }

    /// The generated OpenAPI spec must expose every data-layer endpoint.
    #[test]
    fn openapi_includes_color_template_and_board_paths() {
        let spec = ApiDoc::openapi();
        let json = spec.to_json().expect("spec serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let paths = value
            .get("paths")
            .and_then(|paths| paths.as_object())
            .expect("paths object");
        for path in [
            "/api/boards",
            "/boards/new",
            "/api/boards/{id}/save-as-template",
            "/api/templates",
            "/api/templates/{id}",
            "/b/{board_id}/settings/colors",
            "/api/boards/{id}/colors",
            "/api/boards/{id}/colors/{color_id}",
        ] {
            assert!(paths.contains_key(path), "openapi missing path {path}");
        }
        // Spot-check methods on the color endpoints.
        let colors = &paths["/api/boards/{id}/colors"];
        assert!(colors.get("get").is_some());
        assert!(colors.get("post").is_some());
        let color = &paths["/api/boards/{id}/colors/{color_id}"];
        assert!(color.get("patch").is_some());
        assert!(color.get("delete").is_some());
        // Version endpoint is public discovery too.
        let version = &paths["/api/v1/version"];
        assert!(version.get("get").is_some());
    }

    /// The task-modal parity batch endpoints must appear in the generated
    /// OpenAPI spec with the right methods.
    #[test]
    fn openapi_includes_task_extras_paths() {
        let spec = ApiDoc::openapi();
        let json = spec.to_json().expect("spec serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let paths = value
            .get("paths")
            .and_then(|paths| paths.as_object())
            .expect("paths object");
        for path in [
            "/api/boards/{id}/labels",
            "/api/tasks/{id}/comments",
            "/api/tasks/{id}/comments/{comment_id}",
            "/api/tasks/{id}/attachments",
            "/api/tasks/{id}/attachments/{attachment_id}/file",
            "/api/tasks/{id}/attachments/{attachment_id}",
            "/api/tasks/{id}/time-log-view",
            "/api/tasks/{id}/history-view",
            "/api/time/entries/{id}",
        ] {
            assert!(paths.contains_key(path), "openapi missing path {path}");
        }
        // Spot-check methods.
        assert!(paths["/api/boards/{id}/labels"].get("get").is_some());
        assert!(paths["/api/tasks/{id}/comments"].get("post").is_some());
        // KF-221b: comments list endpoint must appear alongside POST.
        assert!(paths["/api/tasks/{id}/comments"].get("get").is_some());
        assert!(paths["/api/tasks/{id}/comments/{comment_id}"]
            .get("delete")
            .is_some());
        assert!(paths["/api/tasks/{id}/attachments"].get("post").is_some());
        assert!(paths["/api/tasks/{id}/attachments/{attachment_id}/file"]
            .get("get")
            .is_some());
        assert!(paths["/api/tasks/{id}/attachments/{attachment_id}"]
            .get("delete")
            .is_some());
        assert!(paths["/api/tasks/{id}/time-log-view"].get("get").is_some());
        assert!(paths["/api/tasks/{id}/history-view"].get("get").is_some());
        let entry = &paths["/api/time/entries/{id}"];
        assert!(entry.get("put").is_some());
        assert!(entry.get("delete").is_some());
        // KF-221a: POST /api/tasks advertises the 201 JSON create response
        // (Accept: application/json) next to the 200 HTML fragment.
        let create_task = &paths["/api/tasks"];
        let post = create_task.get("post").expect("POST /api/tasks in spec");
        let responses = post.get("responses").expect("responses");
        assert!(responses.get("201").is_some());
        assert!(responses.get("200").is_some());
    }

    /// `GET /api/v1/version` is public and reports the crate version plus
    /// the build SHA (or "unknown" when the build didn't bake one in).
    #[tokio::test]
    async fn version_endpoint_reports_build_identity() {
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::connect(
            dir.path()
                .join("version-test.redb")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("connect");
        let app = super::router(crate::AppState { db });

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/version")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        let sha = value["build_sha"].as_str().expect("build_sha string");
        assert!(!sha.is_empty(), "build_sha present");
    }

    /// Shared fixture for the KF-221 handler regression tests: a fresh
    /// temp-DB app, an admin user with a session cookie, and a board
    /// with a WIP-limited column plus a Done column.
    struct Kf221Fixture {
        app: axum::Router,
        cookie: String,
        board_id: String,
        col_wip: String,
        col_done: String,
    }

    fn kf221_fixture() -> (tempfile::TempDir, Kf221Fixture) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::connect(
            dir.path()
                .join("kf221-test.redb")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("connect");
        let user_id = db
            .create_user(
                "admin",
                &crate::auth::hash_password("s3cret").expect("hash password"),
            )
            .expect("create user");
        let token = crate::auth::create_session(&db, &user_id).expect("session");
        let board_id = db.create_board("KF-221").expect("board");
        let col_wip = db.create_column(&board_id, "Wip", Some(3)).expect("column");
        let col_done = db.create_column(&board_id, "Done", None).expect("column");
        db.set_column_done(&col_done, true).expect("set done");
        let app = super::router(crate::AppState { db });
        let fixture = Kf221Fixture {
            app,
            cookie: format!("session={token}"),
            board_id,
            col_wip,
            col_done,
        };
        (dir, fixture)
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&body).expect("json")
    }

    /// KF-221c: the board columns list exposes `wip_limit` and `is_done`.
    #[tokio::test]
    async fn columns_list_includes_wip_limit_and_is_done() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        let response = fx
            .app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/boards/{}/columns", fx.board_id))
                    .header("cookie", &fx.cookie)
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let columns = json_body(response).await.as_array().expect("array").clone();
        assert_eq!(columns.len(), 2);
        let wip = &columns[0];
        assert_eq!(wip["id"], fx.col_wip);
        assert_eq!(wip["name"], "Wip");
        assert_eq!(wip["wip_limit"], 3);
        assert_eq!(wip["is_done"], false);
        let done = &columns[1];
        assert_eq!(done["id"], fx.col_done);
        assert_eq!(done["name"], "Done");
        assert_eq!(done["wip_limit"], serde_json::Value::Null);
        assert_eq!(done["is_done"], true);
    }

    /// KF-221a: `Accept: application/json` on POST /api/tasks returns a
    /// 201 JSON body with the created task's id.
    #[tokio::test]
    async fn create_task_json_accept_returns_201_with_id() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        let payload = serde_json::json!({"name": "Json task", "column_id": fx.col_wip});
        let response = fx
            .app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/tasks")
                    .header("cookie", &fx.cookie)
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .body(axum::body::Body::from(payload.to_string()))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        let value = json_body(response).await;
        let id = value["id"].as_str().expect("id string");
        assert!(!id.is_empty());
        assert_eq!(value["name"], "Json task");
        assert_eq!(value["column_id"], fx.col_wip);
    }

    /// KF-221a: the default POST /api/tasks response stays the HTML card
    /// fragment the htmx UI appends.
    #[tokio::test]
    async fn create_task_default_stays_html_fragment() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        let payload = serde_json::json!({"name": "Html task", "column_id": fx.col_wip});
        let response = fx
            .app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/tasks")
                    .header("cookie", &fx.cookie)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(payload.to_string()))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            html.contains("data-task-id"),
            "card fragment keeps data-task-id"
        );
    }

    /// KF-221b/e: comments round-trip as JSON; the `author` field
    /// overrides the default username attribution.
    #[tokio::test]
    async fn comments_json_list_and_author_override() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        // Create a task via the API (JSON path hands us the id).
        let payload = serde_json::json!({"name": "Comment task", "column_id": fx.col_wip});
        let response = fx
            .app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/tasks")
                    .header("cookie", &fx.cookie)
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .body(axum::body::Body::from(payload.to_string()))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        let task_id = json_body(response).await["id"]
            .as_str()
            .expect("id")
            .to_string();

        let post_comment = |body: serde_json::Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/tasks/{task_id}/comments"))
                .header("cookie", &fx.cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request")
        };
        // No author: attributed to the authenticated username.
        let response = fx
            .app
            .clone()
            .oneshot(post_comment(serde_json::json!({"body": "First"})))
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(json_body(response).await["author"], "admin");
        // With author: the display name wins.
        let response = fx
            .app
            .clone()
            .oneshot(post_comment(
                serde_json::json!({"body": "Second", "author": "Chip"}),
            ))
            .await
            .expect("oneshot");
        assert_eq!(json_body(response).await["author"], "Chip");

        // KF-221b: GET returns the JSON comment list, oldest first.
        let response = fx
            .app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/tasks/{task_id}/comments"))
                    .header("cookie", &fx.cookie)
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let comments = json_body(response).await.as_array().expect("array").clone();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["body"], "First");
        assert_eq!(comments[1]["body"], "Second");
        assert_eq!(comments[1]["author"], "Chip");
    }

    /// KF-221d: the login Set-Cookie carries `Secure` only when the
    /// request arrived over HTTPS; plain-HTTP logins keep a cookie the
    /// browser will actually store.
    #[tokio::test]
    async fn login_cookie_secure_only_over_https() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        let login_request = |proto: Option<&str>| {
            let mut builder = axum::http::Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded");
            if let Some(proto) = proto {
                builder = builder.header("x-forwarded-proto", proto);
            }
            builder
                .body(axum::body::Body::from("user=admin&pass=s3cret"))
                .expect("request")
        };
        let plain = fx
            .app
            .clone()
            .oneshot(login_request(None))
            .await
            .expect("oneshot");
        assert_eq!(plain.status(), axum::http::StatusCode::SEE_OTHER);
        let plain_cookie = plain.headers()["set-cookie"].to_str().expect("set-cookie");
        assert!(
            !plain_cookie.contains("Secure"),
            "no Secure over plain HTTP: {plain_cookie}"
        );
        let https = fx
            .app
            .oneshot(login_request(Some("https")))
            .await
            .expect("oneshot");
        assert_eq!(https.status(), axum::http::StatusCode::SEE_OTHER);
        let https_cookie = https.headers()["set-cookie"].to_str().expect("set-cookie");
        assert!(
            https_cookie.contains("Secure"),
            "Secure over HTTPS: {https_cookie}"
        );
    }

    /// KF-216: hour-based estimates validate; 0 clears; negatives and
    /// non-finite values are rejected.
    #[test]
    fn normalize_estimate_hours_accepts_positive_clears_on_zero() {
        assert_eq!(normalize_estimate_hours(4.0).unwrap(), Some(4.0));
        assert_eq!(normalize_estimate_hours(0.5).unwrap(), Some(0.5));
        assert_eq!(normalize_estimate_hours(0.0).unwrap(), None);
        assert!(normalize_estimate_hours(-1.0).is_err());
        assert!(normalize_estimate_hours(f64::NAN).is_err());
        assert!(normalize_estimate_hours(f64::INFINITY).is_err());
    }

    /// KF-216: the pomodoro size derives from hours via the configured
    /// pomodoro length, clamped to 1..=4 so the legend keeps working.
    #[test]
    fn estimate_hours_to_size_derives_pomodori() {
        assert_eq!(estimate_hours_to_size(4.0, 25), 4); // 9.6 rounds/clamps to 4
        assert_eq!(estimate_hours_to_size(2.0, 25), 4); // 4.8 -> 5 -> clamp 4
        assert_eq!(estimate_hours_to_size(1.0, 25), 2); // 2.4 -> 2
        assert_eq!(estimate_hours_to_size(0.5, 25), 1); // 1.2 -> 1
        assert_eq!(estimate_hours_to_size(0.1, 25), 1); // never 0
        assert_eq!(estimate_hours_to_size(1.0, 60), 1); // honors pomodoro length
    }

    /// KF-216: estimate labels render KanbanFlow-style.
    #[test]
    fn format_estimate_hours_renders_compact_labels() {
        assert_eq!(format_estimate_hours(4.0), "4h");
        assert_eq!(format_estimate_hours(1.5), "1h 30m");
        assert_eq!(format_estimate_hours(0.5), "30m");
        assert_eq!(format_estimate_hours(2.25), "2h 15m");
    }

    /// KF-258: the modal Time spent cell renders the plain duration label
    /// ("Nh Nm", space-separated): "2h 30m", "43m", "0m" for zero.
    #[test]
    fn time_spent_label_renders_kf258_plain_duration() {
        let label = |mins: i64| format_estimate_hours(mins as f64 / 60.0);
        assert_eq!(label(150), "2h 30m");
        assert_eq!(label(43), "43m");
        assert_eq!(label(0), "0m");
        assert_eq!(label(60), "1h");
        assert_eq!(label(120), "2h");
    }

    /// KF-219: a bare date means "due that day" — it normalizes to end of
    /// day (23:59:59), not midnight, so a task due today is not flagged
    /// overdue for the entire due day.
    #[test]
    fn normalize_due_input_date_only_means_end_of_day() {
        let norm = normalize_due_input("2026-09-30");
        let dt = DateTime::parse_from_rfc3339(&norm)
            .expect("normalizes to RFC3339")
            .with_timezone(&Local);
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2026-09-30");
        assert_eq!((dt.hour(), dt.minute(), dt.second()), (23, 59, 59));
        // Full timestamps pass through untouched.
        assert_eq!(
            normalize_due_input("2026-09-30T17:00:00+00:00"),
            "2026-09-30T17:00:00+00:00"
        );
        assert!(normalize_due_input("2026-09-30 17:00").contains('T'));
    }

    /// KF-227: explicit JSON null on due_at/due_repeat deserializes to
    /// `Some(None)` (clear), distinct from a missing field (`None`,
    /// unchanged) — stock serde collapses both to `None` without the
    /// `de_opt_opt` helper.
    #[test]
    fn due_fields_distinguish_null_from_missing() {
        let input: UpdateTaskInput =
            serde_json::from_str(r#"{"due_at":null,"due_repeat":null}"#).unwrap();
        assert_eq!(input.due_at, Some(None));
        assert_eq!(input.due_repeat, Some(None));
        let input: UpdateTaskInput = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(input.due_at, None);
        assert_eq!(input.due_repeat, None);
        let input: UpdateTaskInput =
            serde_json::from_str(r#"{"due_at":"2026-10-05 17:00","due_repeat":"every week"}"#)
                .unwrap();
        assert_eq!(input.due_at, Some(Some("2026-10-05 17:00".to_string())));
        assert_eq!(input.due_repeat, Some(Some("every week".to_string())));
        let input: UpdateTaskInput = serde_json::from_str(r#"{"due_at":""}"#).unwrap();
        assert_eq!(input.due_at, Some(Some(String::new())));
    }

    /// KF-228: explicit JSON null on wip_limit deserializes to `Some(None)`
    /// (clear), distinct from a missing field (`None`, unchanged) and a
    /// numeric value (`Some(Some(n))`, set).
    #[test]
    fn wip_limit_distinguishes_null_from_missing() {
        let input: UpdateColumnInput = serde_json::from_str(r#"{"wip_limit":null}"#).unwrap();
        assert_eq!(input.wip_limit, Some(None));
        let input: UpdateColumnInput = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(input.wip_limit, None);
        let input: UpdateColumnInput = serde_json::from_str(r#"{"wip_limit":5}"#).unwrap();
        assert_eq!(input.wip_limit, Some(Some(5)));
    }

    /// KF-219: overdue compares the full due timestamp, never the date —
    /// a card due in 2 hours is not overdue; a card due yesterday is.
    #[test]
    fn is_overdue_compares_full_timestamp_not_date() {
        let future = (Local::now() + chrono::Duration::hours(2)).to_rfc3339();
        assert!(
            !is_overdue(Some(&future), false),
            "due in 2h is not overdue"
        );
        let past = (Local::now() - chrono::Duration::hours(26)).to_rfc3339();
        assert!(is_overdue(Some(&past), false), "due yesterday is overdue");
        assert!(
            !is_overdue(Some(&past), true),
            "done tasks are never overdue"
        );
        assert!(!is_overdue(None, false), "no due date is never overdue");
        // A date-only due normalized today is not overdue until the day ends.
        let today_end = normalize_due_input(&Local::now().format("%Y-%m-%d").to_string());
        assert!(
            !is_overdue(Some(&today_end), false),
            "due-today (end of day) is not overdue"
        );
    }

    /// KF-255: the task-modal subline shows "Today" for a task created on the
    /// same local calendar day, and the absolute "%b %d" rendering otherwise.
    #[test]
    fn format_relative_day_is_today_for_same_calendar_day() {
        let now = Local::now().to_rfc3339();
        assert_eq!(format_relative_day(&now), "Today");
        // Earlier the same day, just after midnight.
        let midnight = Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 1)
            .expect("midnight")
            .and_local_timezone(Local)
            .unwrap()
            .to_rfc3339();
        assert_eq!(format_relative_day(&midnight), "Today");
        // Yesterday keeps the absolute rendering.
        let yesterday = (Local::now() - chrono::Duration::days(1)).to_rfc3339();
        let expected = (Local::now() - chrono::Duration::days(1))
            .format("%b %d")
            .to_string();
        assert_eq!(format_relative_day(&yesterday), expected);
        assert_eq!(format_relative_day("not-a-date"), "not-a-date");
    }

    /// KF-217: the filter Color dropdown honors the board's custom color
    /// renames, falling back to the fixed standard palette name ("Purple",
    /// …) when a color has no custom label — never a blank option.
    #[test]
    fn color_display_label_honors_rename_with_standard_fallback() {
        let renamed = ColorView {
            id: "c1".into(),
            value: "purple".into(),
            standard_name: "Purple".into(),
            label: "Deep work".into(),
            description: String::new(),
            enabled: true,
            is_default: false,
            sort_order: 6,
            bg: "#fff".into(),
            border: "#000".into(),
            light: "#eee".into(),
        };
        assert_eq!(renamed.display_label(), "Deep work");
        let mut unlabeled = renamed.clone();
        unlabeled.label.clear();
        assert_eq!(unlabeled.display_label(), "Purple");
    }
}
