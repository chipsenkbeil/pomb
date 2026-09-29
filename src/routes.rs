//! HTTP routes: pages, the task JSON/form API, time tracking, and auth.
//!
//! API handlers accept both `application/json` (used by `app.js` fetch calls)
//! and form-encoded bodies (used by htmx), so either client works.

use std::collections::HashMap;

use askama::Template;
use axum::{
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, patch, post},
    Form, Json, Router,
};
use chrono::{DateTime, Duration, Local};
#[cfg(not(debug_assertions))]
use rust_embed::RustEmbed;
use serde::de::DeserializeOwned;
use serde::Deserialize;
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
        .route("/api/tasks/:id/time", post(log_time).get(get_task_time))
        .route("/api/tasks/:id/subtasks", post(create_subtask))
        .route(
            "/api/tasks/:id/subtasks/:sub_id",
            patch(update_subtask).delete(delete_subtask),
        )
        .route("/api/members", get(list_members))
        .route("/api/time/manual", post(create_manual_time))
        .route(
            "/api/time/entries/:id",
            get(get_time_entry).put(update_time_entry),
        )
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
        .route("/b/:board_id/settings/delete", get(board_delete_page))
        .route("/boards/new", get(new_board_page))
        .route(
            "/api/boards/:id/save-as-template",
            post(save_board_as_template),
        )
        .route("/api/templates", get(list_templates))
        .route("/api/templates/:id", delete(delete_template))
        .route("/b/:board_id/settings/colors", get(board_colors_page))
        .route(
            "/api/boards/:id/colors",
            get(list_board_colors).post(create_board_color),
        )
        .route(
            "/api/boards/:id/colors/:color_id",
            patch(update_board_color).delete(delete_board_color),
        )
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
    tasks: Vec<TaskTime>,
}

/// Daily time totals for the Time spent report.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct TimeSpentReport {
    days: Vec<DayTotal>,
    total_minutes: i64,
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
    /// Resolved per-board color value, e.g. "yellow".
    color_value: String,
    /// Resolved per-board color label, e.g. "1 Pomodoro".
    color_label: String,
    color_bg: String,
    color_border: String,
    color_light: String,
    total_minutes: i64,
    completed_at: Option<String>,
    /// "Sep 28" style rendering of `completed_at`, for cards.
    completed_display: Option<String>,
    /// "Sep 28, 2026" style rendering of `created_at`, for the modal.
    created_display: String,
    pomodori_completed: u32,
    interruptions: u32,
    /// Checklist subtasks (KanbanFlow parity).
    subtasks: Vec<Subtask>,
    /// Assigned member user ids (KanbanFlow parity).
    member_ids: Vec<String>,
    /// Grouping-date override, if set ("Edit grouping date").
    grouping_date: Option<String>,
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
fn task_color_view(db: &Db, board_id: &str, task: &TaskRow) -> Result<ColorFields, AppError> {
    if let Some(color_id) = task.color_id.as_deref() {
        if let Some(color) = db.get_color(color_id).map_err(AppError::from)? {
            return Ok(ColorFields::from(&color));
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
        return Ok(ColorFields::from(&color));
    }
    // No color rows (shouldn't happen — list_colors backfills): fall back
    // to the fixed standard hex values with the legacy size label.
    let (bg, border, light, _) = standard_color(value).expect("known standard color");
    Ok(ColorFields {
        value: value.to_string(),
        label: size.label().to_string(),
        bg: bg.to_string(),
        border: border.to_string(),
        light: light.to_string(),
    })
}

impl TaskView {
    fn from_row(row: &TaskRow) -> Self {
        let size = Size::from_i64(row.size);
        let value = size_to_color_value(row.size);
        let (bg, border, light, _) = standard_color(value).expect("known standard color");
        Self {
            id: row.id.clone(),
            column_id: row.column_id.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            size_label: size.label(),
            color_value: value.to_string(),
            color_label: size.label().to_string(),
            color_bg: bg.to_string(),
            color_border: border.to_string(),
            color_light: light.to_string(),
            total_minutes: row.total_minutes,
            completed_at: row.completed_at.clone(),
            completed_display: row.completed_at.as_deref().map(format_day),
            created_display: DateTime::parse_from_rfc3339(&row.created_at)
                .map(|dt| dt.with_timezone(&Local).format("%b %d, %Y").to_string())
                .unwrap_or_else(|_| row.created_at.clone()),
            pomodori_completed: row.pomodori_completed,
            interruptions: row.interruptions,
            subtasks: row.subtasks.clone(),
            member_ids: row.member_ids.clone(),
            grouping_date: row.grouping_date.clone(),
        }
    }

    /// Like `from_row`, but resolves the color fields through the board's
    /// color configuration (`task.color_id` first, legacy size mapping
    /// otherwise).
    fn from_row_in_board(db: &Db, board_id: &str, row: &TaskRow) -> Result<Self, AppError> {
        let mut view = Self::from_row(row);
        let fields = task_color_view(db, board_id, row)?;
        view.color_value = fields.value;
        view.color_label = fields.label;
        view.color_bg = fields.bg;
        view.color_border = fields.border;
        view.color_light = fields.light;
        Ok(view)
    }
}

/// "Sep 28" rendering of a stored RFC3339 timestamp.
fn format_day(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.with_timezone(&Local).format("%b %d").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
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
    at_limit: bool,
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
    label: String,
    description: String,
    enabled: bool,
    is_default: bool,
    sort_order: i64,
    bg: String,
    border: String,
    light: String,
}

impl From<&ColorRow> for ColorView {
    fn from(color: &ColorRow) -> Self {
        Self {
            id: color.id.clone(),
            value: color.value.clone(),
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

/// `{ "id", "name" }` — one board in `GET /api/boards` and the new-board page.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
struct BoardListItem {
    id: String,
    name: String,
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
    /// First character of the username, uppercased — board-bar owner avatar.
    username_initial: String,
    columns: Vec<ColumnHead>,
    bands: Vec<BandView>,
    /// Enabled colors, ordered for the task color picker / legend.
    /// Also drives the filter panel's Color section (KF-134).
    colors: Vec<ColorView>,
    /// Standard value of the board's default color, e.g. "yellow".
    default_color_value: String,
}

#[derive(Template)]
#[template(path = "new_board.html")]
struct NewBoardTemplate {
    boards: Vec<BoardListItem>,
    templates: Vec<TemplateListItem>,
}

#[derive(Template)]
#[template(path = "board_colors.html")]
struct BoardColorsTemplate {
    board_id: String,
    board_name: String,
    colors: Vec<ColorView>,
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
    note: String,
    started_display: String,
    /// "P" | "M" | "S" badge (v2-00835).
    badge_code: String,
    /// Tooltip: "Pomodori" / "Manually added time" / "Stopwatch".
    badge_title: String,
    interrupted: bool,
    interrupt_reason: Option<String>,
}

#[derive(Template)]
#[template(path = "time_entries.html")]
struct TimeEntriesTemplate {
    entries: Vec<TimeEntryView>,
}

#[derive(Template)]
#[template(path = "timer_log.html")]
struct TimerLogTemplate {
    boards: Vec<serde_json::Value>,
    username: String,
}

#[derive(Template)]
#[template(path = "timer_statistics.html")]
struct TimerStatisticsTemplate {
    boards: Vec<serde_json::Value>,
}

#[derive(Template)]
#[template(path = "modal.html")]
struct ModalTemplate {
    task: TaskView,
    entries: Vec<TimeEntryView>,
    is_done: bool,
    /// Members currently assigned to the task, for the modal body row.
    assigned_members: Vec<MemberView>,
}

/// One board member as shown in the modal body row.
#[derive(Debug, Clone)]
struct MemberView {
    username: String,
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
fn fetch_entries(db: &Db, task_id: &str) -> Result<Vec<TimeEntryView>, AppError> {
    let rows = db.list_entries(task_id).map_err(AppError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            // P = Pomodori, M = Manually added time (v2-00961, v2-00962).
            let (badge_code, badge_title) = match row.kind.as_str() {
                "pomodoro" => ("P", "Pomodori"),
                "stopwatch" => ("S", "Stopwatch"),
                _ => ("M", "Manually added time"),
            };
            TimeEntryView {
                id: row.id.clone(),
                minutes: row.minutes,
                note: row.note.clone(),
                started_display: DateTime::parse_from_rfc3339(&row.started_at)
                    .map(|dt| {
                        dt.with_timezone(&Local)
                            .format("%b %d, %Y %H:%M")
                            .to_string()
                    })
                    .unwrap_or(row.started_at),
                badge_code: badge_code.to_string(),
                badge_title: badge_title.to_string(),
                interrupted: row.interrupted,
                interrupt_reason: row.interrupt_reason.clone(),
            }
        })
        .collect())
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
            let at_limit = col
                .wip_limit
                .map(|limit| count as i64 >= limit)
                .unwrap_or(false);
            ColumnHead {
                id: col.id.clone(),
                name: col.name.clone(),
                description: col.description.clone(),
                collapsed: col.collapsed,
                wip_limit: col.wip_limit,
                count,
                at_limit,
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
                    let label = task
                        .completed_at
                        .as_deref()
                        .map(done_group_label)
                        .unwrap_or_else(|| "Completed".to_string());
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

    Ok(BoardTemplate {
        board_id: board.id.clone(),
        board_name: board.name,
        username_initial: user
            .username
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_default(),
        username: user.username,
        columns: column_heads,
        bands,
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
    Ok(auth::redirect_with_cookie(
        "/",
        &auth::session_cookie(&token),
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

/// Create a task; returns the rendered card fragment (for htmx appends).

#[utoipa::path(
    post,
    path = "/api/tasks",
    tag = "Tasks",
    request_body = CreateTaskInput,
    responses(
        (status = 200, description = "Rendered task card HTML fragment", content_type = "text/html"),
        (status = 400, description = "Invalid input: empty name or unknown column"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_task(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<TaskCardTemplate, AppError> {
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

    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::internal("task vanished after create"))?;
    Ok(TaskCardTemplate {
        task: TaskView::from_row_in_board(db, &column.board_id, &task)?,
    })
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
    Extension(_user): Extension<AuthUser>,
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

    if let Some(member_ids) = input.member_ids.as_deref() {
        db.set_task_members(&id, member_ids)
            .map_err(AppError::from)?;
    }
    if let Some(grouping_date) = input.grouping_date.as_deref() {
        let date = grouping_date.trim();
        db.set_grouping_date(&id, if date.is_empty() { None } else { Some(date) })
            .map_err(AppError::from)?;
    }

    let task = fetch_task_view(db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(TaskCardTemplate { task })
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
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let input: MoveTaskInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if db.get_task(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("task not found"));
    }
    if db
        .get_column(&input.column_id)
        .map_err(AppError::from)?
        .is_none()
    {
        return Err(AppError::bad_request("unknown column"));
    }

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
    subtasks: Vec<SubtaskDetail>,
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
        subtasks: task.subtasks.iter().map(SubtaskDetail::from).collect(),
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
    Extension(_user): Extension<AuthUser>,
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
    Ok(Json(SubtaskDetail::from(&sub)))
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
    Extension(_user): Extension<AuthUser>,
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
    Extension(_user): Extension<AuthUser>,
    Path((id, sub_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let removed = state
        .db
        .remove_subtask(&id, &sub_id)
        .map_err(AppError::from)?;
    if !removed {
        return Err(AppError::not_found("task or subtask not found"));
    }
    Ok(StatusCode::OK)
}

#[derive(serde::Serialize, ToSchema)]
struct MemberDetail {
    id: String,
    username: String,
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
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<ModalTemplate, AppError> {
    let db = &state.db;
    let task = fetch_task_view(db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;
    let is_done = db
        .get_column(&task.column_id)
        .map_err(AppError::from)?
        .map(|column| column.is_done)
        .unwrap_or(false);
    let entries = fetch_entries(db, &id)?;
    let assigned_members: Vec<MemberView> = db
        .list_users()
        .map_err(AppError::from)?
        .into_iter()
        .filter(|user| task.member_ids.iter().any(|m| m == &user.id))
        .map(|user| MemberView {
            username: user.username,
        })
        .collect();
    Ok(ModalTemplate {
        task,
        entries,
        is_done,
        assigned_members,
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
    Extension(_user): Extension<AuthUser>,
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

    Ok(TimeEntriesTemplate {
        entries: fetch_entries(db, &id)?,
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
        entries: fetch_entries(db, &id)?,
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
    }))
}

/// Full-page Timer log (v2-00546): Time log / Time spent tabs.
async fn timer_log_page(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
) -> Result<TimerLogTemplate, AppError> {
    let boards = state
        .db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .map(|b| serde_json::json!({ "id": b.id, "name": b.name }))
        .collect();
    Ok(TimerLogTemplate {
        boards,
        username: user.username,
    })
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
            let (badge_code, badge_title) = match e.kind.as_str() {
                "pomodoro" => ("P", "Pomodori"),
                "stopwatch" => ("S", "Stopwatch"),
                _ => ("M", "Manually added time"),
            };
            TimerLogEntry {
                id: e.id,
                task_id: e.task_id,
                task_name,
                board_id,
                board_name,
                minutes: e.minutes,
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

    let db = &state.db;
    // (date, task_id) -> minutes, plus task metadata for the detailed view.
    let mut totals: HashMap<String, i64> = HashMap::new();
    let mut per_task: HashMap<(String, String), i64> = HashMap::new();
    let mut task_meta: HashMap<String, (String, Option<String>)> = HashMap::new();
    for e in db.all_entries().map_err(AppError::from)? {
        if let Some(date) = e.started_at.get(..10) {
            if date < from_s.as_str() || date > to_s.as_str() {
                continue;
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
            tasks,
        });
        d += Duration::days(1);
    }
    let total_minutes: i64 = days.iter().map(|d| d.minutes).sum();

    Ok(Json(TimeSpentReport {
        days,
        total_minutes,
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
    Extension(_user): Extension<AuthUser>,
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
        )
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;

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
    Extension(_user): Extension<AuthUser>,
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
        )
        .map_err(AppError::from)?;
    if !updated {
        return Err(AppError::not_found("entry not found"));
    }

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
    /// `Some(None)` (JSON null) clears the limit; absent leaves it alone.
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

    if db.get_swimlane(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("swimlane not found"));
    }
    let task_count = db.count_tasks_in_swimlane(&id).map_err(AppError::from)?;
    if task_count > 0 {
        return Err(AppError::bad_request(format!(
            "cannot delete swimlane with {task_count} task(s); move or delete them first"
        )));
    }
    db.delete_swimlane(&id).map_err(AppError::from)?;
    Ok(StatusCode::OK)
}

// ---- Boards, board templates, task colors ----

/// All boards as id/name pairs.
#[utoipa::path(
    get,
    path = "/api/boards",
    tag = "Boards",
    responses(
        (status = 200, description = "All boards as id/name pairs", body = Vec<BoardListItem>),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_boards(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Vec<BoardListItem>>, AppError> {
    let boards = state
        .db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .map(|board| BoardListItem {
            id: board.id,
            name: board.name,
        })
        .collect();
    Ok(Json(boards))
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
        .map(|board| BoardListItem {
            id: board.id,
            name: board.name,
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

#[derive(Deserialize, ToSchema)]
struct SaveTemplateInput {
    name: String,
    description: Option<String>,
}

/// Capture a board as a reusable template: its colors (all, with their
/// config), columns, and swimlanes. Returns the template id.
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
) -> Result<BoardColorsTemplate, AppError> {
    let db = &state.db;
    let board = db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    let colors = db
        .list_colors(&board_id)
        .map_err(AppError::from)?
        .iter()
        .map(ColorView::from)
        .collect();
    Ok(BoardColorsTemplate {
        board_id: board.id,
        board_name: board.name,
        colors,
    })
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
    pomodoro_minutes: u32,
    short_break_minutes: u32,
    long_break_minutes: u32,
    long_break_every: u32,
    ding_enabled: Option<String>,
    notifications_enabled: Option<String>,
    interrupt_reasons: String,
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
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let settings = Settings {
        pomodoro_minutes: clamp_minutes(form.pomodoro_minutes, defaults.pomodoro_minutes),
        short_break_minutes: clamp_minutes(form.short_break_minutes, defaults.short_break_minutes),
        long_break_minutes: clamp_minutes(form.long_break_minutes, defaults.long_break_minutes),
        long_break_every: form.long_break_every.clamp(1, 12),
        ding_enabled: form.ding_enabled.as_deref() == Some("on"),
        notifications_enabled: form.notifications_enabled.as_deref() == Some("on"),
        interrupt_reasons: if reasons.is_empty() {
            defaults.interrupt_reasons
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
    Extension(_user): Extension<AuthUser>,
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
        log_timer_session(db, &old, false, Some("Switched task"))?;
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
/// early stop bumps both its pomodori counter (stopped sessions count as
/// Pomodori, verified in KanbanFlow) and its interruptions.
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
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TimerStopResult>, AppError> {
    let input: TimerStopInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let timer = db
        .get_active_timer()
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("no active timer"))?;
    let logged = log_timer_session(db, &timer, input.completed, input.reason.as_deref())?;

    // Remember custom "why did you stop?" reasons for next time.
    // "Task done" is never remembered: it is always the final menu item,
    // not a configured reason (KF-011/KF-077).
    if let Some(reason) = input.reason.as_deref() {
        let reason = reason.trim();
        if !reason.is_empty() && !reason.eq_ignore_ascii_case("task done") {
            let mut settings = db.get_settings().map_err(AppError::from)?;
            if !settings
                .interrupt_reasons
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(reason))
            {
                // Keep the fixed list tidy: custom reasons slot in before
                // the trailing "Task done" on databases seeded with it
                // (new databases seed only the 15 defaults; "Task done"
                // is appended as the final menu item at render time).
                let at = settings
                    .interrupt_reasons
                    .iter()
                    .position(|existing| existing == "Task done")
                    .unwrap_or(settings.interrupt_reasons.len());
                settings.interrupt_reasons.insert(at, reason.to_string());
                db.update_settings(&settings).map_err(AppError::from)?;
            }
        }
    }

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
) -> Result<Option<i64>, AppError> {
    let elapsed = (chrono::Utc::now().timestamp() - timer.started_at).max(0);
    db.clear_active_timer().map_err(AppError::from)?;
    if elapsed < 20 {
        return Ok(None);
    }
    let minutes = (elapsed / 60).max(1);
    let interrupted =
        !completed && matches!(timer.mode, TimerMode::Pomodoro | TimerMode::Stopwatch);

    if let Some(task_id) = timer.task_id.as_deref() {
        db.create_entry_full(
            Some(task_id),
            minutes,
            "",
            timer.mode.entry_kind(),
            interrupted,
            reason,
        )
        .map_err(AppError::from)?;
        match timer.mode {
            TimerMode::Pomodoro if completed => {
                db.record_pomodoro_complete(task_id)
                    .map_err(AppError::from)?;
            }
            TimerMode::Pomodoro => {
                // Stopped sessions count as Pomodori (verified in KanbanFlow)
                // and also record an interruption.
                db.record_pomodoro_complete(task_id)
                    .map_err(AppError::from)?;
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
    task_name: String,
    minutes: i64,
    kind: String,
    kind_label: String,
    started_display: String,
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
            TodayEntryView {
                task_name,
                minutes: entry.minutes,
                kind: entry.kind,
                kind_label,
                started_display,
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
        create_subtask,
        update_subtask,
        delete_subtask,
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
        create_board,
        new_board_page,
        save_board_as_template,
        list_templates,
        delete_template,
        board_colors_page,
        list_board_colors,
        create_board_color,
        update_board_color,
        delete_board_color,
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
            UpdateTaskInput,
            MoveTaskInput,
            CreateSubtaskInput,
            UpdateSubtaskInput,
            SubtaskDetail,
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
            MoveSwimlaneInput,
            TimerStartInput,
            TimerStopInput,
            TimerRetargetInput,
            TimerLogQuery,
            TimeSpentQuery,
            CreateTokenInput,
            BoardListItem,
            VersionInfo,
            TemplateListItem,
            ColorView,
            CreateBoardInput,
            SaveTemplateInput,
            CreateColorInput,
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
    use utoipa::OpenApi;

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
}
