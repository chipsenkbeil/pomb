mod docs;

use std::collections::HashMap;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::header::{ACCEPT, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Form, Json, Router, middleware};
use base64::Engine as _;
use chrono::{DateTime, Datelike, Duration, Local};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use utoipa::openapi::path::Operation;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{ContentBuilder, OpenApi as OpenApiSpec, Ref, RefOr};
use utoipa::{IntoParams, Modify, OpenApi, ToSchema};

use crate::AppState;
use crate::api_error::{self, ErrorBody, ErrorCode, coded_response};
use crate::auth::{self, AuthUser, CredentialKind};
use crate::db::{
    ClaimOutcome, ClaimRequest, ColumnDeleteOutcome, Db, DeviceAuthCreateOutcome, DeviceGrant,
    DevicePoll, FencedWrite, FieldCreateOutcome, LinkAddOutcome, MoveGuards, MoveOutcome,
    NO_TASK_NAME, NewComment, NewTaskPlacement, NewTimeEntry, local_day,
};
use crate::limit::{Buckets, Limits};
use crate::models::*;

/// Build the full router. The auth middleware layer is added *before*
/// `/login` and `/setup` are registered, so those two stay public.
pub fn router(state: AppState) -> Router {
    #[cfg(debug_assertions)]
    use tower_http::services::ServeDir;

    let router = Router::new()
        .route("/", get(root))
        .route("/logout", post(logout))
        .route("/settings", get(settings_page).post(settings_submit))
        .route("/b/:board_id", get(board_page));

    // Board Export report page.
    let router = router.route("/b/:board_id/export", get(export_page));

    // Board Task count report page.
    let router = router.route("/b/:board_id/task-count", get(task_count_page));

    // Board Throughput report page.
    let router = router.route("/b/:board_id/throughput", get(throughput_page));

    // Board Burndown report page.
    let router = router.route("/b/:board_id/burndown", get(burndown_page));

    // Board Cumulative flow report page.
    let router = router.route("/b/:board_id/cumulative-flow", get(cumulative_flow_page));

    // Board Cycle & lead time report page.
    let router = router.route("/b/:board_id/cycle-lead", get(cycle_lead_page));

    // Board Due date performance report page.
    let router = router.route("/b/:board_id/due-date", get(due_date_page));

    // Board History report page.
    let router = router.route("/b/:board_id/board-history", get(board_history_page));

    // Board Calendar report page.
    let router = router.route("/b/:board_id/calendar", get(calendar_page));

    // Board Monte Carlo forecasting report page.
    let router = router.route("/b/:board_id/monte-carlo", get(monte_carlo_page));

    // Board Time estimate report page.
    let router = router
        .route("/b/:board_id/time-estimate", get(time_estimate_page))
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
        .route(
            "/api/tasks/:id/links",
            post(create_task_link).get(list_task_links),
        )
        .route("/api/tasks/:id/links/:link_id", delete(delete_task_link))
        .route("/api/tasks/:id/claim", post(claim_task))
        .route("/api/tasks/:id/claim/heartbeat", post(heartbeat_claim))
        .route("/api/tasks/:id/claim/release", post(release_claim))
        .route("/api/tasks/:id/subtasks/order", put(reorder_subtasks))
        .route(
            "/api/tasks/:id/subtasks/:sub_id",
            patch(update_subtask).delete(delete_subtask),
        );

    // Task extras: labels, comments, attachments, history.
    let router = router
        .route("/api/boards/:id/labels", get(list_board_labels))
        .route(
            "/api/tasks/:id/comments",
            post(create_comment).get(list_comments),
        )
        .route(
            "/api/tasks/:id/comments/:comment_id",
            delete(delete_comment),
        );

    // The base64 JSON body of a 10 MiB file exceeds axum's 2 MiB default body limit.
    let router = router
        .route(
            "/api/tasks/:id/attachments",
            post(upload_attachment).layer(DefaultBodyLimit::max(ATTACHMENT_BODY_LIMIT)),
        )
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
        .route("/api/timer/settings", get(api_timer_settings))
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
        .route("/api/swimlanes/:id/move", post(move_swimlane));

    // Boards, board templates, and per-board task colors.
    let router = router
        .route("/api/boards", get(list_boards).post(create_board))
        .route(
            "/api/boards/:id",
            get(get_board_detail).delete(delete_board_api),
        )
        .route("/api/boards/:id/config", put(update_board_config))
        .route("/api/boards/:id/columns", get(list_board_columns))
        .route("/api/boards/:id/tasks", get(list_board_tasks))
        .route(
            "/api/boards/:id/fields",
            post(create_board_field).get(list_board_fields),
        )
        .route(
            "/api/fields/:id",
            patch(update_board_field).delete(delete_board_field),
        );

    // Board export dataset for the Export report.
    let router = router.route("/api/boards/:id/export-data", get(board_export_data));

    // Per-board iCal feed link management (authenticated). The raw bearer token is only revealed
    // here.
    let router = router
        .route(
            "/api/boards/:id/calendar-feed",
            get(get_calendar_feed_link).post(create_calendar_feed_link),
        )
        .route(
            "/api/boards/:id/calendar-feed/reset",
            post(reset_calendar_feed),
        )
        .route("/b/:board_id/settings/delete", get(board_delete_page))
        .route("/boards/new", get(new_board_page))
        .route("/boards/archived", get(archived_boards_page))
        .route(
            "/api/boards/:id/save-as-template",
            post(save_board_as_template),
        )
        .route("/api/templates", get(list_templates))
        .route("/api/templates/:id", delete(delete_template))
        .route("/b/:board_id/settings/colors", get(board_colors_page));

    // Board settings shell: General, Layout, Colors, Task settings, Advanced, API & Webhooks, Add
    // task from email.
    let router = router
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
            "/b/:board_id/settings/fields",
            get(board_settings_fields_page),
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
        );

    // Copy another board's whole palette onto this board.
    let router = router.route("/api/boards/:id/colors/copy-from", post(copy_board_colors));

    // Agent API tokens. Creation/listing/revocation require the
    // browser session cookie — a Bearer token can never mint tokens.
    let router = router
        .route(
            "/api/v1/auth/tokens",
            post(create_api_token).get(list_api_tokens),
        )
        .route("/api/v1/auth/tokens/:id", delete(revoke_api_token))
        .route("/api/v1/auth/whoami", get(whoami));

    // Device pairing: the approval pages need the browser session.
    let router = router
        .route("/device", get(device_page).post(device_lookup))
        .route("/device/approve", post(device_approve))
        .route("/device/deny", post(device_deny))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/login", get(login_page).post(login_submit));

    // Device pairing for CLI login: the device side has no credentials.
    let router = router
        .route("/api/v1/auth/device", post(start_device_auth))
        .route("/api/v1/auth/device/token", post(poll_device_token));

    // Agent discovery endpoints: public documentation, no secrets.
    // Registered after the auth layer so agents can fetch them with
    // just the base URL.
    let router = router
        .route("/skill.md", get(skill_md))
        .route("/llms.txt", get(llms_txt))
        .route("/api/v1/openapi.json", get(openapi_json))
        .route("/docs/api.md", get(api_reference_md))
        .route("/api/v1/version", get(version_info));

    // Unauthenticated iCal feed (bearer token in the URL).
    let router = router
        .route("/api/calendar-feed/:token", get(serve_calendar_feed))
        .route("/setup", get(setup_page).post(setup_submit));

    // Static assets (`static/app.js`, `static/style.css`): served from disk
    // in debug builds so edits show up without a rebuild; embedded in the
    // binary in release builds so `cargo install` produces a fully
    // self-contained binary with no `./static` directory needed next to it.
    #[cfg(debug_assertions)]
    let router = router.nest_service("/static", ServeDir::new("static"));
    #[cfg(not(debug_assertions))]
    let router = router.route("/static/*path", get(serve_embedded_static));

    // Outermost layer, so it covers every route registered above.
    router
        .fallback(api_error::route_not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::limit::ip_limits,
        ))
        .layer(middleware::from_fn(crate::csrf::refuse_cross_origin_writes))
        .layer(middleware::from_fn(api_error::json_errors))
        .with_state(state)
}

/// Static assets embedded in release builds so the installed binary is
/// self-contained (see `router`).
#[cfg(not(debug_assertions))]
#[derive(rust_embed::RustEmbed)]
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
    code: ErrorCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self::with_status(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::with_status(StatusCode::NOT_FOUND, message)
    }

    fn with_status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: ErrorCode::for_status(status),
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::with_status(StatusCode::CONFLICT, message)
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self::with_status(StatusCode::FORBIDDEN, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::with_status(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// Names the stable code the JSON error body reports.
    fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = code;
        self
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        coded_response(self.status, self.code, self.message)
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

/// Returns `true` when the response should be JSON: the client asked for it, or authenticated with
/// a Bearer token rather than the browser session cookie.
fn wants_json(headers: &HeaderMap, user: &AuthUser) -> bool {
    user.kind != CredentialKind::Session || accepts_json(headers)
}

// ---- API view models ----
//
// These structs are the JSON shapes returned by the API handlers. They are
// the single source of truth for both the runtime responses and the
// generated OpenAPI spec (via `ToSchema`), so the spec cannot drift from
// the implementation.

/// One task in `GET /api/tasks`: id, name, and the board it sits on.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TaskNameItem {
    id: String,
    name: String,
    /// The board the task's column belongs to.
    board_id: String,
}

/// Filter for `GET /api/tasks`.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct TaskListQuery {
    /// Only tasks on this board; empty means no filter, an unknown board is
    /// a 404.
    board_id: Option<String>,
}

/// Response body `{ "id" }` returned when creating columns, swimlanes, etc.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct IdResult {
    id: String,
}

/// Response body `{ "id", "minutes" }` returned when creating or updating time entries.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct IdMinutesResult {
    id: String,
    minutes: i64,
}

/// Describes one time entry of a task.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TimeEntryItem {
    /// Entry id.
    id: String,
    /// Id of the task the entry belongs to.
    task_id: String,
    /// Length in whole minutes.
    minutes: i64,
    /// Exact length in seconds.
    seconds: i64,
    /// Free-text note.
    note: String,
    /// RFC3339 start time.
    started_at: String,
    /// One of `pomodoro`, `stopwatch`, `short_break`, `long_break` or `manual`.
    kind: String,
    /// Whether a timed session was stopped before it finished.
    interrupted: bool,
    /// Reason given when the session was stopped early.
    interrupt_reason: Option<String>,
    /// Username that logged the entry.
    created_by: String,
    /// Labels attached to the entry.
    labels: Vec<String>,
}

impl TimeEntryItem {
    fn from_row(row: TimeEntryRow, fallback_user: &str) -> Self {
        let seconds = row.duration_seconds();
        let created_by = if row.created_by.is_empty() {
            fallback_user.to_string()
        } else {
            row.created_by
        };
        Self {
            id: row.id,
            task_id: row.task_id,
            minutes: row.minutes,
            seconds,
            note: row.note,
            started_at: row.started_at,
            kind: row.kind,
            interrupted: row.interrupted,
            interrupt_reason: row.interrupt_reason,
            created_by,
            labels: row.labels,
        }
    }
}

/// Single time entry as JSON (powers the edit-entry dialog).
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TimeEntryDetail {
    id: String,
    task_id: String,
    task_name: String,
    minutes: i64,
    /// Exact length in seconds, so the edit dialog loads the true end time.
    seconds: i64,
    note: String,
    started_at: String,
    /// Labels attached in the manual/edit dialogs (KanbanFlow parity).
    labels: Vec<String>,
}

/// One row of the paginated timer log.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TimerLogEntry {
    id: String,
    task_id: String,
    task_name: String,
    board_id: Option<String>,
    board_name: Option<String>,
    minutes: i64,
    /// Exact length in seconds, for the day total ("Today 30s").
    seconds: i64,
    /// "31s" for sub-minute durations, "Nm" otherwise.
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
    /// Labels attached in the manual-time / edit-entry dialogs.
    labels: Vec<String>,
    /// Username that created the entry (avatar in the time log).
    created_by: String,
    /// The task colour's border shade for the row dot. Null for a task-less entry or a task without
    /// a colour.
    task_color: Option<String>,
}

/// Paginated timer log page.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TimerLogPage {
    entries: Vec<TimerLogEntry>,
    has_more: bool,
    total: usize,
}

/// One task's time within a day for the Time spent report.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TaskTime {
    task_id: String,
    task_name: String,
    minutes: i64,
    color_id: Option<String>,
}

/// One day's total for the Time spent report, with per-task breakdown.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
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
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TimeSpentReport {
    days: Vec<DayTotal>,
    total_minutes: i64,
    /// Total task minutes across all days (excluding breaks).
    total_task_minutes: i64,
    /// Total break minutes across all days.
    total_break_minutes: i64,
}

/// Interruption count for one "Why did you stop?" reason.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct ReasonCount {
    reason: String,
    count: i64,
}

/// One day's pomodoro count for the statistics bar chart.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct DayPomodori {
    date: String,
    label: String,
    pomodori: i64,
    minutes: i64,
}

/// One break activity's statistics for the Break activities tab: total logged time in the requested
/// range plus today's progress against the activity's daily goal.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct BreakActivityStat {
    id: String,
    name: String,
    minutes: i64,
    today_minutes: i64,
    daily_goal: u32,
    daily_limit: Option<u32>,
}

/// Pomodoro statistics report.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TimerStatisticsReport {
    total_pomodori: i64,
    total_minutes: i64,
    avg_minutes: i64,
    interruptions: i64,
    by_reason: Vec<ReasonCount>,
    daily: Vec<DayPomodori>,
    /// Best single day in the requested range (Highscores tab), null when no pomodoro was logged in
    /// it.
    best_day: Option<DayPomodori>,
    /// Longest run of consecutive days with at least one pomodoro.
    longest_streak: i64,
    /// Per-activity break statistics (Break activities tab).
    break_activities: Vec<BreakActivityStat>,
}

/// Result of stopping the timer.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TimerStopResult {
    /// True when the session was too short to log.
    discarded: bool,
    /// Logged minutes, or null when discarded.
    minutes: Option<i64>,
    /// The server's completion verdict, false when the session was discarded.
    completed: bool,
    /// True when the `started_at` given was no longer the active session, so nothing was stopped or
    /// logged.
    already_stopped: bool,
}

/// Result of revoking a token.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct RevokeResult {
    revoked: String,
}

// ---- View models for templates ----

/// Task shaped for rendering: colors/labels resolved, dates formatted.
#[derive(Clone, Debug)]
struct TaskView {
    id: String,
    column_id: String,
    name: String,
    description: String,
    /// Hour-based estimate label, e.g. "4h". None when no hour estimate is set, and the modal then
    /// omits Time estimate.
    estimate_label: Option<String>,
    /// Resolved per-board color value, e.g. "yellow".
    color_value: String,
    /// Resolved per-board color label, e.g. "1 Pomodoro".
    color_label: String,
    /// True when the task's resolved color is not the board's default color. The modal shows the
    /// color name text only in this case, and tasks on the default color render the dot alone.
    show_color_name: bool,
    color_bg: String,
    color_border: String,
    color_light: String,
    total_minutes: i64,
    /// Plain duration label for the modal's Time spent cell, KanbanFlow-style: "2h 30m" (space
    /// between hours and minutes), "43m" sub-hour, "0m" for zero. No emoji and no Time log link.
    time_spent_label: String,
    /// "1 Pomodoro" / "N Pomodori" beside Time spent, counted from the task's completed pomodoro
    /// entries by the modal. None at zero.
    pomodori_label: Option<String>,
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
    /// Labels serialized as a JSON array, rendered as the card's `data-labels` attribute so
    /// client-side label filtering can match cards even when the column hides the label chips.
    labels_json: String,
    /// The hour estimate in whole minutes (0 when unset), so the board can recompute an "Estimated
    /// hours" column sum client-side.
    estimate_minutes: i64,
    /// "Sep 28" rendering of `due_at`, honoring the column's due-dates
    /// mode ("active_7d" hides far-future dues). None when hidden or unset.
    due_display: Option<String>,
    /// "Sep 28" rendering of `column_added_at` (falls back to created_at),
    /// for the card footer.
    added_display: String,
    /// Which task properties this task's column shows on cards (the "Task properties to display on
    /// board" config).
    display: TaskCardDisplay,
    /// Grouping-date override, if set ("Edit grouping date").
    grouping_date: Option<String>,
    /// Raw RFC3339 time the task entered its column, for archiving groups.
    column_added_at: Option<String>,
    /// Whether the card sits past an archiving column's first page.
    archive: ArchivePlacement,
    /// Whether a `blocked_by` target keeps the card from being ready.
    readiness: Readiness,
    /// Watch flag: task More menu "Watch" (KanbanFlow parity).
    watched: bool,
    /// Due date rendered for the modal and the card's full-date tooltip,
    /// e.g. "Sep 28, 2026 5:00 PM". Distinct from `due_display`, the
    /// short mode-honoring card rendering.
    due_full: Option<String>,
    /// KanbanFlow-style modal due-date text, e.g. "Friday 5:00 PM" or "Oct 2 at 11:00 PM", and the
    /// template appends `due_suffix`. None when unset or unparseable.
    due_modal: Option<String>,
    /// Overdue and card-window state of the due date.
    due_state: DueState,
    /// Due-date repeat text, e.g. "every week".
    due_repeat: Option<String>,
    /// KanbanFlow-style card due-date rendering, e.g. "Friday 5:00 PM"
    /// (near-term) or "30 October 5:00 PM" (farther out). None when unset
    /// or unparseable.
    due_card: Option<String>,
    /// The due date's parenthetical, the target column name (or "Done" for a due date marked done
    /// without a target column).
    due_suffix: Option<String>,
    /// KanbanFlow-style card time readout, e.g. "2h 30m / 8h" (spent /
    /// estimate), "0h / 4h", or "45m" (spent only). None when nothing to
    /// show.
    time_kf: Option<String>,
    /// Raw RFC3339 due date, if set. Rendered only as `data-due-at` on the card so client-side date
    /// filters can evaluate it.
    due_at: Option<String>,
    /// Task comments (KanbanFlow parity), oldest first.
    comments: Vec<CommentView>,
    /// Pretty-printed metadata for the modal's read-only block; None when
    /// unset or empty.
    metadata_pretty: Option<String>,
    /// Number of links listed on the task, for the card's link chip.
    link_count: usize,
    /// Card link-chip tooltip: one "Blocked by: …" / "Blocks: …" /
    /// "Related: …" line per kind present.
    links_title: String,
    /// Holder of the live claim, for the card's claim chip.
    claimant: Option<String>,
    /// Claim-chip tooltip, e.g. "Claimed by builder-i until …".
    claim_title: String,
    /// True while a required "user" custom field is empty (card badge).
    fields_incomplete: bool,
}

/// Represents where an archiving column places a card.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ArchivePlacement {
    /// On the first page, or in a column that does not archive.
    Shown,
    /// Past the first page, rendered hidden until "Load more tasks".
    Hidden,
}

/// Represents whether a card's `blocked_by` targets are all in the Done column.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Readiness {
    /// Every `blocked_by` target is in the Done column, or there are none.
    Ready,
    /// A `blocked_by` target sits in a column that is not Done.
    Blocked,
}

/// How a task's due date renders on its card.
#[derive(Clone, Debug)]
struct DueState {
    /// True when the due date is in the past and the task is not done.
    overdue: bool,
    /// False when the column's due-dates mode hides this due date on the card ("Show active due in
    /// 7 days" and it is farther out).
    in_window: bool,
}

/// One custom field shaped for the task dialog.
#[derive(Clone, Debug)]
struct FieldInputView {
    key: String,
    label: String,
    /// Wire name of the kind, e.g. "multiselect".
    kind: &'static str,
    required: bool,
    /// Required and empty: the dialog marks the input.
    missing: bool,
    /// Text, multiline, and number values as text; empty when unset.
    text: String,
    /// Bool value.
    checked: bool,
    /// Select and multiselect choices with their selection state.
    options: Vec<FieldOptionView>,
    /// Read-only rendering for system fields, e.g. "a, b" or "Yes".
    display: String,
    /// Stored choices the field's options no longer list.
    stale: Vec<String>,
}

#[derive(Clone, Debug)]
struct FieldOptionView {
    value: String,
    selected: bool,
}

impl FieldInputView {
    fn new(def: &FieldDefRow, value: Option<&serde_json::Value>) -> Self {
        use serde_json::Value;
        let chosen = |option: &str| match value {
            Some(Value::String(choice)) => choice == option,
            Some(Value::Array(items)) => items.iter().any(|item| item.as_str() == Some(option)),
            _ => false,
        };
        let text = match value {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.to_string(),
            _ => String::new(),
        };
        let display = match value {
            Some(Value::Bool(true)) => "Yes".to_string(),
            Some(Value::Bool(false)) => "No".to_string(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            Some(Value::Null) | None => String::new(),
            Some(_) => text.clone(),
        };
        Self {
            key: def.key.clone(),
            label: def.label.clone(),
            kind: def.kind.name(),
            required: def.required,
            missing: def.required && !def.is_complete(value),
            text,
            checked: matches!(value, Some(Value::Bool(true))),
            options: def
                .options
                .iter()
                .map(|option| FieldOptionView {
                    value: option.clone(),
                    selected: chosen(option),
                })
                .collect(),
            display,
            stale: match value {
                Some(Value::String(choice)) => vec![choice.clone()],
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            }
            .into_iter()
            .filter(|choice| def.kind.has_options() && !def.options.contains(choice))
            .collect(),
        }
    }
}

/// Splits a board's fields into editable "user" and read-only "system"
/// views for one task.
fn field_views(
    defs: &[FieldDefRow],
    values: &serde_json::Map<String, serde_json::Value>,
) -> (Vec<FieldInputView>, Vec<FieldInputView>) {
    defs.iter()
        .map(|def| (def.owner, FieldInputView::new(def, values.get(&def.key))))
        .fold(
            (Vec::new(), Vec::new()),
            |(mut user, mut system), (owner, view)| {
                match owner {
                    FieldOwner::User => user.push(view),
                    FieldOwner::System => system.push(view),
                }
                (user, system)
            },
        )
}

/// One comment shaped for templates.
#[derive(Clone, Debug)]
struct CommentView {
    id: String,
    author: String,
    body: String,
    /// "Today 3:03 PM" relative rendering of `created_at`.
    created_display: String,
    /// Two-letter uppercase initials for the comment avatar circle. KanbanFlow renders comment
    /// avatars uppercase ("CS").
    author_initials: String,
    /// Tag shown beside the date, e.g. "agent".
    kind: Option<String>,
}

/// One assigned member as shown on a card: username + avatar initial.
#[derive(Clone, Debug)]
struct MemberChip {
    username: String,
    /// The same initials as the header avatar, e.g. "AU".
    initials: String,
}

/// Which task properties a column shows on its cards: the "Task properties to display on board"
/// config, stored as `prop_*` keys in the column's config_json bag. Defaults mirror the dialog (all
/// hide except Due dates, which defaults to "Show active due in 7 days").
#[derive(Clone, Debug, Default)]
struct TaskCardDisplay {
    content: CardContent,
    dates: CardDates,
    /// Some(7) when the mode is "active_7d" (only near dues shown);
    /// None for plain "show".
    due_within_days: Option<i64>,
}

/// Which content blocks a column shows on its cards.
#[derive(Clone, Debug, Default)]
struct CardContent {
    /// Whether the description shows.
    description: bool,
    /// Whether the labels show.
    labels: bool,
    /// Whether the subtasks show.
    subtasks: bool,
}

/// Which dates a column shows on its cards.
#[derive(Clone, Debug, Default)]
struct CardDates {
    /// Whether the due date shows.
    due: bool,
    /// Whether the creation date shows.
    created: bool,
    /// Whether the date the task was added to its column shows.
    added: bool,
}

impl TaskCardDisplay {
    fn from_config_json(raw: &str) -> Self {
        let v: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
        let get = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
        let show = |key: &str| matches!(get(key), "show");
        let due_mode = get("prop_due_dates");
        Self {
            content: CardContent {
                description: show("prop_description"),
                labels: show("prop_labels"),
                subtasks: show("prop_subtasks"),
            },
            dates: CardDates {
                // Any non-"hide" value shows due dates; the default (empty)
                // is KanbanFlow's "Show active due in 7 days".
                due: due_mode != "hide",
                created: show("prop_created"),
                added: show("prop_added"),
            },
            due_within_days: if due_mode == "show" { None } else { Some(7) },
        }
    }
}

/// Resolved task color fields (per-board config or legacy fallback).
#[derive(Clone, Debug)]
struct ColorFields {
    value: String,
    label: String,
    bg: String,
    border: String,
    light: String,
}

/// The border shade of a task's colour, KanbanFlow's dot beside a timed
/// entry; None for a missing task or a task without a colour.
fn task_dot_color(db: &Db, task_id: &str) -> Option<String> {
    db.get_task(task_id)
        .ok()
        .flatten()
        .and_then(|task| task.color_id)
        .and_then(|cid| db.get_color(&cid).ok().flatten())
        .map(|color| color.border_hex)
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

/// Resolves a task's color fields. Explicit `color_id` wins, otherwise the legacy size mapping
/// applies, resolved through the board's configured colors when present (so old rows pick up the
/// board's exact palette). The returned flag is true when the resolved color is NOT the board's
/// default color, and only then does the modal show the color name.
fn task_color_view(
    db: &Db,
    board_id: &str,
    task: &TaskRow,
) -> Result<(ColorFields, bool), AppError> {
    if let Some(color_id) = task.color_id.as_deref()
        && let Some(color) = db.get_color(color_id).map_err(AppError::from)?
    {
        return Ok((ColorFields::from(&color), !color.is_default));
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

    // No color rows (shouldn't happen — startup backfills them): fall back
    // to the fixed standard hex values with the legacy size label.
    let (bg, border, light, _) = standard_color(value).expect("known standard color");
    let fields = ColorFields {
        value: value.to_string(),
        label: size.label().to_string(),
        bg: bg.to_string(),
        border: border.to_string(),
        light: light.to_string(),
    };

    // No board colors at all: the size label is never a color name.
    Ok((fields, false))
}

/// Longest subtask list a card shows in full.
const CARD_SUBTASKS_SHOWN: usize = 3;

/// Reports whether a card hides its done subtasks behind "N hidden (N done)".
///
/// KanbanFlow lists every subtask of a short list; a longer one keeps only
/// the open items in view.
fn folds_done_subtasks(subtasks: &[Subtask]) -> bool {
    subtasks.len() > CARD_SUBTASKS_SHOWN && subtasks.iter().any(|s| s.done)
}

/// Tooltip for a card's link chip: one line per link kind, naming the
/// linked tasks, e.g. "Blocked by: Write docs, Review".
fn links_title(db: &Db, links: &[TaskLink], board_id: &str) -> Result<String, AppError> {
    let mut lines = Vec::new();
    for (kind, label) in [
        (LinkKind::BlockedBy, "Blocked by"),
        (LinkKind::Blocks, "Blocks"),
        (LinkKind::Related, "Related"),
    ] {
        let mut names = Vec::new();
        for link in links.iter().filter(|link| link.kind == kind) {
            let Some(target) = db.get_task(&link.target_id).map_err(AppError::from)? else {
                continue;
            };

            // A task on another board is named with its board.
            let other_board = match link.board_id.as_deref() {
                Some(other) if other != board_id => db.get_board(other).map_err(AppError::from)?,
                _ => None,
            };
            match other_board {
                Some(board) => names.push(format!("{} ({})", target.name, board.name)),
                None => names.push(target.name),
            }
        }
        if !names.is_empty() {
            lines.push(format!("{label}: {}", names.join(", ")));
        }
    }
    Ok(lines.join("\n"))
}

impl TaskView {
    /// Returns whether the card is past the first page of an archiving column.
    fn archive_hidden(&self) -> bool {
        self.archive == ArchivePlacement::Hidden
    }

    /// Returns whether a `blocked_by` target is not yet in the Done column.
    fn blocked(&self) -> bool {
        self.readiness == Readiness::Blocked
    }

    /// Whether the card folds its done subtasks under "N hidden
    /// (N done)", see [`folds_done_subtasks`].
    fn subtasks_fold(&self) -> bool {
        folds_done_subtasks(&self.subtasks)
    }

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
            estimate_label: row.estimate_hours.map(format_estimate_hours),
            color_value: value.to_string(),
            color_label: size.label().to_string(),
            show_color_name: false,
            color_bg: bg.to_string(),
            color_border: border.to_string(),
            color_light: light.to_string(),
            total_minutes: row.total_minutes,
            time_spent_label: format_estimate_hours(row.total_minutes as f64 / 60.0),
            pomodori_label: None,
            completed_at: row.completed_at.clone(),
            completed_display: row.completed_at.as_deref().map(format_day),
            created_display: DateTime::parse_from_rfc3339(&row.created_at)
                .map(|dt| dt.with_timezone(&Local).format("%b %d, %Y").to_string())
                .unwrap_or_else(|_| row.created_at.clone()),
            created_day: format_day(&row.created_at),
            subtasks: row.subtasks.clone(),
            subtasks_done: row.subtasks.iter().filter(|s| s.done).count(),
            member_ids: row.member_ids.clone(),
            member_chips: Vec::new(),
            labels: row.labels.clone(),
            labels_json: serde_json::to_string(&row.labels).unwrap_or_else(|_| "[]".to_string()),
            estimate_minutes: estimate_minutes(row.estimate_hours),
            due_display: None,
            added_display: format_added(row),
            display: TaskCardDisplay::default(),
            grouping_date: row.grouping_date.clone(),
            column_added_at: row.column_added_at.clone(),
            archive: ArchivePlacement::Shown,
            readiness: Readiness::Ready,
            watched: row.watched,
            due_full: row.due_at.as_deref().map(format_datetime),
            due_modal: row
                .due_at
                .as_deref()
                .and_then(|d| format_due_modal(d, None)),
            due_suffix: row.due_done.then(|| "Done".to_string()),
            due_state: DueState {
                overdue: is_overdue(row.due_at.as_deref(), done),
                in_window: true,
            },
            due_repeat: row.due_repeat.clone(),
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
                    kind: c.kind.clone(),
                })
                .collect(),
            metadata_pretty: row
                .metadata
                .as_ref()
                .filter(|metadata| !metadata.is_empty())
                .and_then(|metadata| serde_json::to_string_pretty(metadata).ok()),
            link_count: 0,
            links_title: String::new(),
            claimant: row.live_claim().map(|claim| claim.claimant.clone()),
            claim_title: row
                .live_claim()
                .map(|claim| {
                    format!(
                        "Claimed by {} until {}",
                        claim.claimant,
                        format_datetime(&claim.expires_at)
                    )
                })
                .unwrap_or_default(),
            fields_incomplete: false,
        }
    }

    /// Like `from_row`, but resolves the color fields through the board's
    /// color configuration (`task.color_id` first, legacy size mapping
    /// otherwise). Also resolves the column's per-property display flags
    /// (due dates / labels on cards, from the column config bag).
    fn from_row_in_board(db: &Db, board_id: &str, row: &TaskRow) -> Result<Self, AppError> {
        let mut view = Self::from_row(row);
        let links = db
            .list_task_links(&row.id)
            .map_err(AppError::from)?
            .unwrap_or_default();
        if !links.is_empty() {
            view.link_count = links.len();
            view.links_title = links_title(db, &links, board_id)?;
            if db.task_blocked(row).map_err(AppError::from)? {
                view.readiness = Readiness::Blocked;
            }
        }
        view.fields_incomplete = db
            .list_fields(board_id)
            .map_err(AppError::from)?
            .iter()
            .any(|def| {
                def.owner == FieldOwner::User
                    && def.required
                    && !def.is_complete(row.fields.get(&def.key))
            });
        let (fields, show_name) = task_color_view(db, board_id, row)?;
        view.color_value = fields.value;
        view.color_label = fields.label;
        view.color_bg = fields.bg;
        view.color_border = fields.border;
        view.color_light = fields.light;
        view.show_color_name = show_name;

        // Per-column card property config + due-date mode come from the task's column, and member
        // avatars resolve through the user list.
        if let Ok(Some(col)) = db.get_column(&row.column_id) {
            view.display = TaskCardDisplay::from_config_json(&col.config_json);
            if let (Some(due), Some(days)) = (row.due_at.as_deref(), view.display.due_within_days) {
                view.due_state.in_window = DateTime::parse_from_rfc3339(due)
                    .map_or(true, |due| due <= Local::now() + Duration::days(days));
            }
            if view.display.dates.due {
                view.due_display = row
                    .due_at
                    .as_deref()
                    .and_then(|d| format_due(d, view.display.due_within_days));
            }
        }

        // The parenthetical names the due date's target column.
        if let Some(target) = row.due_target_column_id.as_deref()
            && let Ok(Some(col)) = db.get_column(target)
            && col.board_id == board_id
        {
            view.due_suffix = Some(col.name);
        }
        if !row.member_ids.is_empty()
            && let Ok(users) = db.list_users()
        {
            view.member_chips = users
                .into_iter()
                .filter(|u| row.member_ids.iter().any(|m| m == &u.id))
                .map(|u| MemberChip {
                    initials: user_initials(&u.username),
                    username: u.username,
                })
                .collect();
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

/// "Today 3:03 PM" rendering of a stored RFC3339 timestamp for comment timestamps (KanbanFlow
/// parity): relative day label (Today / Yesterday / "Sep 27") plus 12-hour time.
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

/// The task-modal subline renders "Created: Today" when the task was created on the same local
/// calendar day, and any other day keeps the absolute `format_day` rendering. Scoped to the subline
/// only: `format_day` serves other surfaces (card metadata, completed dates) and stays absolute.
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

/// "31s" for sub-minute durations, "Nm" under an hour, "2h 30m" from an hour up. `seconds` is the
/// precise elapsed seconds (0 for rows written before the field existed, in which case we fall back
/// to `minutes`).
fn format_entry_duration(seconds: i64, minutes: i64) -> String {
    if seconds > 0 && seconds < 60 {
        format!("{seconds}s")
    } else {
        format_estimate_hours(minutes as f64 / 60.0)
    }
}

/// The task modal's pomodoro count, or None when there is none.
fn format_pomodori(count: u32) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 Pomodoro".to_string()),
        n => Some(format!("{n} Pomodori")),
    }
}

/// P/S/B/M badge code and tooltip for a time entry. A pomodoro reads "Pomodoro" unless it was
/// stopped early, and a break is never manual time.
fn entry_badge(kind: &str, interrupted: bool, reason: Option<&str>) -> (&'static str, String) {
    match EntryKind::parse(kind) {
        Some(EntryKind::Pomodoro) if interrupted => (
            "P",
            format!("Stopped with reason \"{}\"", reason.unwrap_or("No reason")),
        ),
        Some(EntryKind::Pomodoro) => ("P", "Pomodoro".to_string()),
        Some(EntryKind::Stopwatch) => ("S", "Stopwatch".to_string()),
        Some(EntryKind::ShortBreak) => ("B", "Short break".to_string()),
        Some(EntryKind::LongBreak) => ("B", "Long break".to_string()),
        Some(EntryKind::Manual) | None => ("M", "Manually added".to_string()),
    }
}

/// Two-letter uppercase initials for the avatar circle ("CS"). First letters of the first two
/// whitespace-separated words, falling back to the first two characters.
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

/// "Sep 28" rendering of when the task entered its current column, falling back to the creation
/// date for rows written before the field existed.
fn format_added(row: &TaskRow) -> String {
    let stamp = row.column_added_at.as_deref().unwrap_or(&row.created_at);
    format_day(stamp)
}

/// "Sep 28" rendering of a due date, honoring the column's due-dates mode. `within_days` (Some(7)
/// for KanbanFlow's "Show active due in 7 days") hides dues more than that far in the future, and
/// overdue dues stay visible. None when unparseable or out of range.
fn format_due(rfc3339: &str, within_days: Option<i64>) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(rfc3339)
        .ok()?
        .with_timezone(&Local);
    if let Some(days) = within_days
        && dt > Local::now() + Duration::days(days)
    {
        return None;
    }
    Some(dt.format("%b %d").to_string())
}

/// Where a due date falls relative to now, for KanbanFlow's due text.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DueDay {
    /// The same local day, earlier or later.
    Today,
    /// A later instant within the next six local days.
    ThisWeek,
    /// Any other past or future date.
    Other,
}

/// Parses `rfc3339` into local time and classifies it against `now`.
fn due_day(rfc3339: &str, now: DateTime<Local>) -> Option<(DateTime<Local>, DueDay)> {
    let due = DateTime::parse_from_rfc3339(rfc3339)
        .ok()?
        .with_timezone(&Local);
    let (day, today) = (due.date_naive(), now.date_naive());
    let kind = if day == today {
        DueDay::Today
    } else if due > now && day < today + Duration::days(7) {
        DueDay::ThisWeek
    } else {
        DueDay::Other
    };
    Some((due, kind))
}

/// KanbanFlow's card due text: "Today 5:00 PM", a weekday for a later date this week ("Friday 5:00
/// PM"), else "25 September 5:00 PM". None when unparseable.
fn format_due_card(rfc3339: &str) -> Option<String> {
    format_due_card_at(rfc3339, Local::now())
}

fn format_due_card_at(rfc3339: &str, now: DateTime<Local>) -> Option<String> {
    let (due, kind) = due_day(rfc3339, now)?;
    let format = match kind {
        DueDay::Today => "Today %-I:%M %p",
        DueDay::ThisWeek => "%A %-I:%M %p",
        DueDay::Other => "%-d %B %-I:%M %p",
    };
    Some(due.format(format).to_string())
}

/// KanbanFlow's modal due text: like the card, except other dates read "Oct 2 at 11:00 PM".
/// `suffix` (the target column name) follows in parentheses. None when unparseable.
fn format_due_modal(rfc3339: &str, suffix: Option<&str>) -> Option<String> {
    format_due_modal_at(rfc3339, suffix, Local::now())
}

fn format_due_modal_at(
    rfc3339: &str,
    suffix: Option<&str>,
    now: DateTime<Local>,
) -> Option<String> {
    let (due, kind) = due_day(rfc3339, now)?;
    let format = match kind {
        DueDay::Today => "Today %-I:%M %p",
        DueDay::ThisWeek => "%A %-I:%M %p",
        DueDay::Other => "%b %-d at %-I:%M %p",
    };
    let mut text = due.format(format).to_string();
    if let Some(suffix) = suffix {
        text.push_str(&format!(" ({suffix})"));
    }
    Some(text)
}

/// KanbanFlow-style card time readout: "{spent} / {estimate}" (e.g. "2h 30m / 8h", "0h / 4h"), or
/// just "{spent}" (e.g. "45m") when no estimate is set. None when there is no logged time and no
/// estimate.
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

/// Grouping label for completed tasks: Today / Yesterday / "Friday, 10 July", with the year added
/// outside `today`'s year.
fn done_group_label(date: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    if date == today {
        "Today".to_string()
    } else if date == today - Duration::days(1) {
        "Yesterday".to_string()
    } else if date.year() == today.year() {
        date.format("%A, %-d %B").to_string()
    } else {
        date.format("%A, %-d %B %Y").to_string()
    }
}

/// Groups Done-column tasks under day headers in [`done_display_key`] order.
///
/// A non-empty column starts with a "Today" header, empty when nothing was done today, and an empty
/// column has no header. Board order holds within a day, and undated tasks group last under
/// "Completed".
fn group_done_tasks(tasks: &[TaskView], today: chrono::NaiveDate) -> Vec<DoneGroup> {
    let mut dated: Vec<(Option<chrono::NaiveDate>, &TaskView)> = tasks
        .iter()
        .map(|task| {
            let date = done_group_date(
                task.grouping_date.as_deref(),
                task.completed_at.as_deref(),
                task.column_added_at.as_deref(),
            );
            (date, task)
        })
        .collect();
    if dated.is_empty() {
        return Vec::new();
    }
    dated.sort_by_key(|(date, _)| done_display_key(*date, today));
    let mut groups = vec![DoneGroup {
        label: done_group_label(today, today),
        hidden: false,
        tasks: Vec::new(),
    }];

    // Only the first ARCHIVE_PAGE_SIZE cards show, "Show more" reveals the rest, and a group whose
    // cards are all hidden hides too.
    for (shown, (date, task)) in dated.into_iter().enumerate() {
        let label = date.map_or_else(
            || "Completed".to_string(),
            |date| done_group_label(date, today),
        );
        let mut task = task.clone();
        task.archive = if shown >= ARCHIVE_PAGE_SIZE {
            ArchivePlacement::Hidden
        } else {
            ArchivePlacement::Shown
        };
        match groups.last_mut() {
            Some(group) if group.label == label => {
                group.hidden &= task.archive_hidden();
                group.tasks.push(task);
            }
            _ => groups.push(DoneGroup {
                label,
                hidden: task.archive_hidden(),
                tasks: vec![task],
            }),
        }
    }
    groups
}

/// Orders a Done group's date on screen: today first, then every other day
/// newest first (a future grouping date after today), undated last.
fn done_display_key(
    date: Option<chrono::NaiveDate>,
    today: chrono::NaiveDate,
) -> (bool, std::cmp::Reverse<Option<chrono::NaiveDate>>) {
    (date != Some(today), std::cmp::Reverse(date))
}

/// Parses a grouping date written exactly as zero-padded "YYYY-MM-DD".
fn parse_grouping_date(raw: &str) -> Option<chrono::NaiveDate> {
    let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    (date.format("%Y-%m-%d").to_string() == raw).then_some(date)
}

/// The day a Done-column task groups under: its grouping-date override when set, else the local day
/// it was completed.
fn done_group_date(
    grouping_date: Option<&str>,
    completed_at: Option<&str>,
    added_at: Option<&str>,
) -> Option<chrono::NaiveDate> {
    let local_day = |raw: Option<&str>| {
        let at = DateTime::parse_from_rfc3339(raw?).ok()?;
        Some(at.with_timezone(&Local).date_naive())
    };
    grouping_date
        .and_then(parse_grouping_date)
        .or_else(|| local_day(completed_at))
        .or_else(|| local_day(added_at))
}

/// Represents the Edit column "Task sorting" setting.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TaskSorting {
    /// Board order (manual).
    None,
    DueDate,
    Color,
    Name,
    Created,
    AddedToColumn,
}

/// Represents the Edit column "Column sum" setting, shown in the header badge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ColumnSum {
    /// The task count (the badge's long-standing content).
    None,
    TaskCount,
    EstimatedHours,
}

/// The Edit column settings that change how a column renders, read from its config bag.
#[derive(Copy, Clone, Debug)]
struct ColumnSettings {
    sorting: TaskSorting,
    sum: ColumnSum,
    /// "Archiving": group the column's tasks under date headers. Defaults to on for the Done column
    /// when never saved.
    archiving: bool,
}

impl ColumnSettings {
    fn from_column(column: &ColumnRow) -> Self {
        let bag: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&column.config_json).unwrap_or_default();
        let sorting = match bag.get("sorting").and_then(serde_json::Value::as_str) {
            Some("due") => TaskSorting::DueDate,
            Some("color") => TaskSorting::Color,
            Some("name") => TaskSorting::Name,
            Some("created") => TaskSorting::Created,
            Some("added") => TaskSorting::AddedToColumn,
            _ => TaskSorting::None,
        };
        let sum = match bag.get("column_sum") {
            Some(serde_json::Value::Bool(true)) => ColumnSum::TaskCount,
            Some(serde_json::Value::String(s)) if s == "count" => ColumnSum::TaskCount,
            Some(serde_json::Value::String(s)) if s == "hours" => ColumnSum::EstimatedHours,
            _ => ColumnSum::None,
        };
        let archiving = bag
            .get("group_by_date")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(column.is_done);
        Self {
            sorting,
            sum,
            archiving,
        }
    }
}

/// Orders one cell's tasks by the column's Task sorting. The sort is stable, so ties keep board
/// order. `color_rank` maps a color id to its palette position.
fn sort_cell_tasks(
    tasks: &mut [&TaskRow],
    sorting: TaskSorting,
    color_rank: &HashMap<String, i64>,
) {
    match sorting {
        TaskSorting::None => {}
        TaskSorting::DueDate => tasks.sort_by_key(|t| {
            let due = t
                .due_at
                .as_deref()
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok());
            (due.is_none(), due)
        }),
        TaskSorting::Color => tasks.sort_by_key(|t| {
            t.color_id
                .as_ref()
                .and_then(|id| color_rank.get(id).copied())
                .unwrap_or(i64::MAX)
        }),
        TaskSorting::Name => tasks.sort_by_key(|t| t.name.to_lowercase()),
        TaskSorting::Created => tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at)),
        TaskSorting::AddedToColumn => tasks.sort_by(|a, b| {
            let added = |t: &TaskRow| {
                t.column_added_at
                    .clone()
                    .unwrap_or_else(|| t.created_at.clone())
            };
            added(a).cmp(&added(b))
        }),
    }
}

/// Translates a drop index counted in a Done cell's on-screen order ([`done_display_key`]) into the
/// board-order index `Db::move_task` takes.
///
/// `siblings` holds the group dates of the cell's other tasks in board
/// order, `moved` the moved task's group date, and `current` its board
/// index when it already sits in this cell. The task lands next to the
/// on-screen neighbor from its own group; a drop beside no such neighbor
/// keeps `current`, or else goes to the top of its group.
fn done_drop_position(
    today: chrono::NaiveDate,
    siblings: &[Option<chrono::NaiveDate>],
    moved: Option<chrono::NaiveDate>,
    position: f64,
    current: Option<usize>,
) -> usize {
    let mut shown: Vec<usize> = (0..siblings.len()).collect();
    shown.sort_by_key(|&i| done_display_key(siblings[i], today));
    let at = (position.round() as i64).clamp(0, shown.len() as i64) as usize;
    let in_group = |i: &&usize| siblings[**i] == moved;
    if let Some(&prev) = at
        .checked_sub(1)
        .and_then(|i| shown.get(i))
        .filter(in_group)
    {
        return prev + 1;
    }
    if let Some(&next) = shown.get(at).filter(in_group) {
        return next;
    }
    current.unwrap_or_else(|| siblings.iter().position(|d| *d == moved).unwrap_or(0))
}

/// One column x swimlane cell on the board.
#[derive(Clone, Debug)]
struct CellView {
    column_id: String,
    swimlane_id: String,
    /// Whether the column groups its tasks by date ("Archiving").
    archiving: bool,
    /// Whether the column has a Task sorting, so manual drops within it snap back to the sort
    /// order.
    sorted: bool,
    /// Tasks past the archiving column's first page.
    archive_more: usize,
    /// Whether the column is collapsed, so the cell renders as the strip body carrying the vertical
    /// `column_name`.
    collapsed: bool,
    column_name: String,
    /// Tasks for normal columns.
    tasks: Vec<TaskView>,
    /// Tasks for the Done column, grouped under date headers.
    done_groups: Vec<DoneGroup>,
    /// The column's overdue count when this cell heads the column with the overdue strip, otherwise
    /// 0.
    overdue_count: usize,
    /// The column's total task count, for the overdue strip's tooltip.
    column_task_count: usize,
}

#[derive(Clone, Debug)]
struct DoneGroup {
    label: String,
    tasks: Vec<TaskView>,
    /// Every task in the group is past the first page, so the label stays hidden until "Load more
    /// tasks" reaches it.
    hidden: bool,
}

/// An archiving column shows its newest 20 tasks until "Load more tasks" reveals the next page
/// ("Loads the 20 most recent tasks").
const ARCHIVE_PAGE_SIZE: usize = 20;

/// One swimlane band: its header plus one cell per column.
#[derive(Clone, Debug)]
struct BandView {
    id: String,
    name: String,
    /// True for the main swimlane, whose row has no visible label.
    is_main: bool,
    task_count: usize,
    cells: Vec<CellView>,
}

/// Reports whether the board needs the swimlane label column.
///
/// Lanes without tasks fold into side strips, so the column exists only when
/// a lane other than the main one renders as a row.
fn needs_lane_label_column(bands: &[BandView]) -> bool {
    bands
        .iter()
        .any(|band| !band.is_main && band.task_count > 0)
}

/// Puts each column's overdue strip in the first band that renders as a row.
///
/// Lanes without tasks fold into side strips unless the main lane is alone,
/// matching the template's row rule.
fn place_overdue_strips(bands: &mut [BandView], heads: &[ColumnHead], hide_swimlane_header: bool) {
    let Some(band) = bands
        .iter_mut()
        .find(|band| hide_swimlane_header || band.task_count > 0)
    else {
        return;
    };
    for (cell, head) in band.cells.iter_mut().zip(heads) {
        cell.overdue_count = head.overdue_count;
        cell.column_task_count = head.count;
    }
}

/// Column header data: id, name, task count, optional WIP limit, done flag.
#[derive(Clone, Debug)]
struct ColumnHead {
    id: String,
    name: String,
    description: String,
    collapsed: bool,
    wip_limit: Option<i64>,
    count: usize,
    over_limit: bool,
    /// Tasks in this column whose due timestamp has passed and that are not completed (overdue
    /// means the due time has passed and the task is not completed), for the collapsed-strip "N
    /// overdue task(s)" indicator.
    overdue_count: usize,
    is_done: bool,
    /// Opaque column-dialog settings bag, for the Edit column dialog.
    config_json: String,
    /// The header badge, absent for Column sum "None" (KanbanFlow shows no count there), the task
    /// count for "Task count", or the column's estimate total for "Estimated hours", e.g. "12h".
    sum_label: Option<String>,
    /// "count" or "hours", so client-side count updates leave an hours badge alone.
    sum_kind: &'static str,
    /// The column's estimate total in minutes, the unfiltered base for client-side "Estimated
    /// hours" updates.
    estimate_minutes: i64,
}

/// One color slot as JSON (also used by the color-admin page template).
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct ColorView {
    id: String,
    /// Standard color value, e.g. "yellow".
    value: String,
    /// Standard color display name, e.g. "Yellow", shown inside the swatch.
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
    /// The color's display name: the board-configured custom name when set, falling back to the
    /// fixed standard palette name ("Yellow", "Green", …) when the custom label is empty.
    /// KanbanFlow's filter Color section lists the palette's fixed names unless renamed on the
    /// board, so the filter dropdown and the legend footer share this.
    fn display_label(&self) -> &str {
        if self.label.is_empty() {
            &self.standard_name
        } else {
            &self.label
        }
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

/// One board in `GET /api/boards` and the new-board page, shaped `{ "id", "name", "archived" }`.
/// `archived` indicates the board is archived (hidden from default lists).
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct BoardListItem {
    id: String,
    name: String,
    /// Whether the board is archived. Archived boards are excluded from default listings, and
    /// `?include_archived=true` includes them.
    archived: bool,
}

/// One board column in `GET /api/boards/:id/columns`: `{ "id", "name", "wip_limit", "is_done" }`
/// (Move-task dialog board switcher).
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct ColumnListItem {
    id: String,
    name: String,
    /// WIP limit; null when unset.
    wip_limit: Option<i64>,
    /// Whether tasks moved here count as Done.
    is_done: bool,
}

/// One board template, shaped `{ "id", "name", "description", "built_in" }`.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct TemplateListItem {
    id: String,
    name: String,
    description: String,
    built_in: bool,
}

/// How the board grid lays out its swimlane rows.
#[derive(Clone, Debug)]
struct BoardLayout {
    /// True when the board's only swimlane is its main one. The swimlane still exists in the DB
    /// (column "+" buttons work) and only its header row is hidden, matching KanbanFlow's free tier
    /// which renders no swimlane row.
    hide_swimlane_header: bool,
    /// Renders the corner cell and one label cell in every row.
    lane_label_column: bool,
}

#[derive(Template)]
#[template(path = "board.html")]
struct BoardTemplate {
    board_id: String,
    board_name: String,
    username: String,
    /// Two-letter uppercase initials for avatar circles.
    username_initial: String,
    columns: Vec<ColumnHead>,
    bands: Vec<BandView>,
    layout: BoardLayout,
    /// Whether the add-task dialog puts new cards at the top.
    new_tasks_at_top: bool,
    /// Enabled colors, ordered for the task color picker / legend. Also drives the filter panel's
    /// Color section.
    colors: Vec<ColorView>,
    /// Whether the color legend bar is shown. KanbanFlow shows no legend by default, and the Menu
    /// toggles it per board (persisted in the board's config bag).
    legend_visible: bool,
    /// All boards for the Boards dropdown menu.
    boards: Vec<BoardListItem>,
    /// The board's "Due date time" ("HH:MM"), the due dialog's time for a new due date.
    due_time: String,
    /// Registered users, one filter User option each.
    members: Vec<MemberDetail>,
}

impl BoardTemplate {
    /// Counts the columns that render at full width (board minimum).
    fn expanded_column_count(&self) -> usize {
        self.columns.len() - self.collapsed_column_count()
    }

    /// Counts the columns that render as 28px strips (board minimum).
    fn collapsed_column_count(&self) -> usize {
        self.columns.iter().filter(|col| col.collapsed).count()
    }
}

#[derive(Template)]
#[template(path = "new_board.html")]
struct NewBoardTemplate {
    boards: Vec<BoardListItem>,
    templates: Vec<TemplateListItem>,
}

/// Archived boards list page.
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

#[derive(Clone, Debug)]
struct TimeEntryView {
    id: String,
    minutes: i64,
    /// "31s" for sub-minute durations, "Nm" otherwise.
    duration_display: String,
    note: String,
    started_display: String,
    /// "P" | "M" | "S" | "B" badge.
    badge_code: String,
    /// Tooltip: "Pomodoro" / `Stopped with reason "X"` / "Manually added" / "Stopwatch" /
    /// "Short break" / "Long break".
    badge_title: String,
    interrupted: bool,
    interrupt_reason: Option<String>,
    /// "1:25 PM - 1:55 PM" From-To range in 12h (KanbanFlow parity).
    range_display: String,
    /// Username that logged the entry (avatar + name in the log view).
    member_name: String,
    /// First letter of `member_name`, for the avatar dot.
    member_initial: String,
    /// Labels attached in the manual/edit dialogs (KanbanFlow parity).
    labels: Vec<String>,
}

/// One day group in the dedicated time-log sub-view: "Today 2m", "Yesterday 40m", or "Sep 27"
/// style.
#[derive(Clone, Debug)]
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

/// Export report page: download the board's tasks as a file.
#[derive(Template)]
#[template(path = "export.html")]
struct ExportTemplate {
    board_id: String,
    board_name: String,
}

#[derive(Template)]
#[template(path = "modal.html")]
struct ModalTemplate {
    task: TaskView,
    is_done: bool,
    /// Members currently assigned to the task, for the modal body row.
    assigned_members: Vec<MemberView>,
    /// Name of the task's current column (for the modal subtitle).
    column_name: String,
    /// "Jun 23" rendering of the task's created date.
    created_short: String,
    /// Two-letter uppercase initials of the signed-in user, for the comment composer avatar circle.
    composer_initials: String,
    /// The board's "user" custom fields, editable in the dialog.
    user_fields: Vec<FieldInputView>,
    /// The board's "system" custom fields, shown read-only.
    system_fields: Vec<FieldInputView>,
}

/// One history event shaped for the History sub-view (KanbanFlow parity). Newest first.
#[derive(Clone, Debug)]
struct HistoryEventView {
    kind: String,
    detail: String,
    /// The actor, followed by the API token's name in parentheses when a token made the change.
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
    groups: Vec<DayGroup>,
}

/// One board member as shown in the modal body row.
#[derive(Clone, Debug)]
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
            let (badge_code, badge_title) =
                entry_badge(&row.kind, row.interrupted, row.interrupt_reason.as_deref());
            let local_start =
                DateTime::parse_from_rfc3339(&row.started_at).map(|dt| dt.with_timezone(&Local));
            let started_display = local_start
                .as_ref()
                .map(|dt| dt.format("%b %d, %Y %H:%M").to_string())
                .unwrap_or_else(|_| row.started_at.clone());

            // The From-To range in 12h, derived server-side from started_at + minutes
            let range_display = local_start
                .as_ref()
                .map(|start| {
                    let end = *start + chrono::Duration::seconds(row.duration_seconds());
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
                badge_title,
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

/// Appends an activity-trail event (KanbanFlow parity: History). History must never break the
/// operation it annotates, so failures are swallowed here.
fn log_history(db: &Db, task_id: &str, kind: &str, detail: &str, actor: Actor<'_>) {
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
    let main_lane_id = board.main_swimlane(&swimlanes);

    // One query for every task on the board, ordered for rendering.
    let tasks = db.board_tasks(&board_id).map_err(AppError::from)?;

    let column_heads: Vec<ColumnHead> = columns
        .iter()
        .map(|col| {
            let count = tasks.iter().filter(|t| t.column_id == col.id).count();

            // The warning is a *violation*, so it fires only when the count exceeds the limit and
            // not when it merely reaches it
            let over_limit = col
                .wip_limit
                .map(|limit| count as i64 > limit)
                .unwrap_or(false);

            // Overdue means the due time has passed and the task is not completed, and the count
            // feeds the collapsed-strip indicator
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
            let estimate_total: i64 = tasks
                .iter()
                .filter(|t| t.column_id == col.id)
                .map(|t| estimate_minutes(t.estimate_hours))
                .sum();
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
                sum_label: match ColumnSettings::from_column(col).sum {
                    ColumnSum::EstimatedHours => Some(format_column_hours(estimate_total)),
                    ColumnSum::TaskCount => Some(count.to_string()),
                    ColumnSum::None => None,
                },
                sum_kind: match ColumnSettings::from_column(col).sum {
                    ColumnSum::EstimatedHours => "hours",
                    ColumnSum::None | ColumnSum::TaskCount => "count",
                },
                estimate_minutes: estimate_total,
            }
        })
        .collect();

    // Palette positions for Task sorting by color.
    let color_rank: HashMap<String, i64> = db
        .list_colors(&board_id)
        .map_err(AppError::from)?
        .into_iter()
        .map(|color| (color.id, color.sort_order))
        .collect();

    let mut bands = Vec::new();
    for lane in &swimlanes {
        let mut cells = Vec::new();
        for col in &columns {
            let settings = ColumnSettings::from_column(col);
            let mut rows: Vec<&TaskRow> = tasks
                .iter()
                .filter(|t| {
                    t.column_id == col.id && t.swimlane_id.as_deref() == Some(lane.id.as_str())
                })
                .collect();
            sort_cell_tasks(&mut rows, settings.sorting, &color_rank);
            let cell_tasks: Vec<TaskView> = rows
                .into_iter()
                .map(|task| TaskView::from_row_in_board(db, &board_id, task))
                .collect::<Result<Vec<_>, _>>()?;

            // An archiving column (the Done column unless changed) groups its tasks under date
            // headers.
            let done_groups = if settings.archiving {
                group_done_tasks(&cell_tasks, Local::now().date_naive())
            } else {
                Vec::new()
            };

            cells.push(CellView {
                column_id: col.id.clone(),
                swimlane_id: lane.id.clone(),
                archiving: settings.archiving,
                sorted: settings.sorting != TaskSorting::None,
                archive_more: done_groups
                    .iter()
                    .flat_map(|group| &group.tasks)
                    .filter(|task| task.archive_hidden())
                    .count(),
                collapsed: col.collapsed,
                column_name: col.name.clone(),
                tasks: if settings.archiving {
                    Vec::new()
                } else {
                    cell_tasks
                },
                done_groups,
                overdue_count: 0,
                column_task_count: 0,
            });
        }
        let lane_task_count = tasks
            .iter()
            .filter(|t| t.swimlane_id.as_deref() == Some(lane.id.as_str()))
            .count();
        bands.push(BandView {
            id: lane.id.clone(),
            name: lane.name.clone(),
            is_main: main_lane_id.as_deref() == Some(lane.id.as_str()),
            task_count: lane_task_count,
            cells,
        });
    }

    // Hide the swimlane header row when the board has only the structural main swimlane (the lane
    // must still exist).
    let hide_swimlane_header = bands.len() == 1 && bands[0].is_main;
    let lane_label_column = needs_lane_label_column(&bands);
    place_overdue_strips(&mut bands, &column_heads, hide_swimlane_header);

    // The color legend is opt-in per board (KanbanFlow shows no legend by default), and it is read
    // before `board.name` is moved below.
    let legend_visible = board.config_bool("legend_visible");
    let new_tasks_at_top = board.config_bool("new_tasks_at_top");
    let due_time = board.due_time().format("%H:%M").to_string();

    Ok(BoardTemplate {
        board_id: board.id.clone(),
        board_name: board.name,
        username_initial: user_initials(&user.username),
        username: user.username,
        columns: column_heads,
        bands,
        layout: BoardLayout {
            hide_swimlane_header,
            lane_label_column,
        },
        new_tasks_at_top,
        legend_visible,
        due_time,
        colors: db
            .list_colors(&board_id)
            .map_err(AppError::from)?
            .iter()
            .filter(|color| color.enabled)
            .map(ColorView::from)
            .collect(),
        boards: dropdown_boards(db)?,
        members: db
            .list_users()
            .map_err(AppError::from)?
            .into_iter()
            .map(|user| MemberDetail {
                id: user.id,
                username: user.username,
            })
            .collect(),
    })
}

/// Unarchived boards for the top bar's Boards dropdown.
fn dropdown_boards(db: &Db) -> Result<Vec<BoardListItem>, AppError> {
    Ok(db
        .list_boards()
        .map_err(AppError::from)?
        .into_iter()
        .filter(|b| !b.config_bool("archived"))
        .map(|b| BoardListItem {
            id: b.id.clone(),
            name: b.name.clone(),
            archived: false,
        })
        .collect())
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

    // `Secure` only when the request actually arrived over HTTPS, since a `Secure` cookie sent over
    // plain HTTP is dropped by the browser
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
    /// Hour-based time estimate, e.g. 4.0 for "4h". Positive values set the estimate, and the
    /// pomodoro `size` is derived from it unless `size` is also given.
    estimate_hours: Option<f64>,
    /// Due date/time in one of the forms under Due dates in the Tasks section. Absent, null, empty
    /// or whitespace-only means no due date.
    due_at: Option<String>,
    /// Due-date repeat text, e.g. "every week", dropped without a due date.
    due_repeat: Option<String>,
    /// Task description.
    description: Option<String>,
    /// Labels; trimmed, empties and duplicates dropped.
    labels: Option<Vec<String>>,
    /// Custom field values by key, checked as Fields describes. A field with a default that is not
    /// given starts at its default.
    #[serde(default, deserialize_with = "de_opt_opt")]
    #[schema(value_type = Object, required = false)]
    fields: Option<Option<serde_json::Value>>,
}

/// Lists all tasks as id/name/board pairs, optionally for one board. Feeds the manual-time /
/// edit-entry task autocomplete.
#[utoipa::path(
    get,
    path = "/api/tasks",
    operation_id = "list_tasks",
    tag = "Tasks",
    params(TaskListQuery),
    responses(
        (status = 200, description = "Tasks as id/name/board_id items", body = Vec<TaskNameItem>),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_tasks(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<Vec<TaskNameItem>>, AppError> {
    let db = &state.db;
    let board_filter = query
        .board_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if let Some(board_id) = board_filter
        && db.get_board(board_id).map_err(AppError::from)?.is_none()
    {
        return Err(AppError::not_found("Board not found"));
    }
    let mut column_boards: HashMap<String, String> = HashMap::new();
    for board in db.list_boards().map_err(AppError::from)? {
        for column in db.list_columns(&board.id).map_err(AppError::from)? {
            column_boards.insert(column.id, board.id.clone());
        }
    }
    let tasks = db
        .all_tasks()
        .map_err(AppError::from)?
        .into_iter()
        .filter_map(|t| {
            let board_id = column_boards.get(&t.column_id)?.clone();
            Some(TaskNameItem {
                id: t.id,
                name: t.name,
                board_id,
            })
        })
        .filter(|item| board_filter.is_none_or(|id| id == item.board_id))
        .collect();
    Ok(Json(tasks))
}

/// Creates a task. Bearer callers, and any caller sending `Accept: application/json`, receive the
/// created task's detail as a 201 JSON body.
#[utoipa::path(
    post,
    path = "/api/tasks",
    operation_id = "create_task",
    tag = "Tasks",
    request_body = CreateTaskInput,
    responses(
        (status = 201, description = "Created task", body = TaskDetail),
        (status = 400, description = "Invalid input: empty name, unknown column, or unparseable due_at"),
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

    // An unparseable due date rejects the request before the task exists. Absent, null, or blank
    // means no due date, and a bare date takes the board's due time.
    let due_time = board_due_time(db, &column.id)?;
    let due_at = input
        .due_at
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|raw| normalize_due_input(raw, due_time))
        .transpose()
        .map_err(AppError::bad_request)?;

    let field_defs = db.list_fields(&column.board_id).map_err(AppError::from)?;
    let mut field_values = match &input.fields {
        Some(Some(patch)) => check_field_values(&field_defs, patch)?,
        Some(None) => return Err(AppError::bad_request("fields must be a JSON object")),
        None => serde_json::Map::new(),
    };
    for def in &field_defs {
        if let Some(default) = &def.default {
            field_values
                .entry(def.key.clone())
                .or_insert_with(|| default.clone());
        }
    }

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

    // The board's "Position of new tasks", bottom by default.
    let at_top = db
        .get_board(&column.board_id)
        .map_err(AppError::from)?
        .is_some_and(|board| board.config_bool("new_tasks_at_top"));
    let placement = if at_top {
        NewTaskPlacement::Top
    } else {
        NewTaskPlacement::Bottom
    };

    // Validate the estimate before anything is written.
    let estimate = input
        .estimate_hours
        .map(|raw| normalize_estimate_hours(raw).map_err(AppError::bad_request))
        .transpose()?;

    // Build the whole row, then store it in one write.
    let mut row = Db::new_task_row(
        &column.id,
        swimlane_id.as_deref(),
        &name,
        size,
        color_id.as_deref(),
    );

    // An estimate given without an explicit size derives the pomodoro size (the color above already
    // resolved).
    if let Some(hours) = estimate {
        row.estimate_hours = hours.map(crate::db::round_estimate);
        if let (None, Some(hours)) = (input.size, hours) {
            let pomodoro_minutes = db.get_settings().map(|s| s.pomodoro_minutes).unwrap_or(25);
            row.size = estimate_hours_to_size(hours, pomodoro_minutes);
        }
    }

    // Optional due date at creation (same format rules as the PATCH path). A repeat without a due
    // date is dropped.
    if due_at.is_some() {
        row.due_at = due_at;
        row.due_repeat = input
            .due_repeat
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    if let Some(description) = input.description {
        row.description = description;
    }
    if let Some(labels) = input.labels.as_deref() {
        row.labels = crate::db::clean_labels(labels);
    }
    for (key, value) in field_values {
        if !value.is_null() {
            row.fields.insert(key, value);
        }
    }
    let created = format!("Task created in {}", column.name);

    // The event names the column so reports follow renames.
    crate::db::push_column_event(&mut row, "created", created, user.actor(), Some(&column.id));

    // The field values are checked again against the definitions stored at
    // write time, in the same transaction as the insert.
    let id = db
        .insert_task_checked(row, placement)
        .map_err(|err| match err.to_string().as_str() {
            crate::db::UNKNOWN_COLOR => AppError::bad_request(crate::db::UNKNOWN_COLOR),
            _ => AppError::from(err),
        })?
        .map_err(AppError::bad_request)?;
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::internal("task vanished after create"))?;

    // API callers that ask for JSON get a 201 with the task detail, and the HTML card fragment path
    // stays for the htmx UI.
    if wants_json(&headers, &user) {
        Ok((StatusCode::CREATED, Json(TaskDetail::load(db, task)?)).into_response())
    } else {
        Ok(TaskCardTemplate {
            task: TaskView::from_row_in_board(db, &column.board_id, &task)?,
        }
        .into_response())
    }
}

/// Serde helper for `Option<Option<T>>` PATCH fields: stock serde collapses an explicit JSON `null`
/// into the same `None` as a missing field, so without this, `null` can never mean "clear". Missing
/// → `None` (outer), `null` → `Some(None)`, a value → `Some(Some(v))`.
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
    /// Grouping-date override ("Edit grouping date") as YYYY-MM-DD that decides the task's
    /// Done-column group. Null or an empty string clears it, and absent leaves it unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    grouping_date: Option<Option<String>>,
    /// Replace the task's labels (KanbanFlow parity). Absent leaves them
    /// unchanged.
    labels: Option<Vec<String>>,
    /// Due date/time in one of the forms under Due dates in the Tasks section. Null, an empty
    /// string or whitespace-only clears it, and absent leaves it unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    due_at: Option<Option<String>>,
    /// Due-date repeat text, e.g. "every week". Null or an empty string clears it, and absent
    /// leaves it unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    due_repeat: Option<Option<String>>,
    /// Mark the due date done/undone (KanbanFlow parity: checking the
    /// due-date item; the card renders "(Done)"). Absent leaves it
    /// unchanged.
    due_done: Option<bool>,
    /// The column the task should reach by its due date (KanbanFlow's "Target column"). Its name
    /// follows the due date in parentheses. Null or "" means "Any archived column", and absent
    /// leaves it unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    due_target_column_id: Option<Option<String>>,
    /// Hour-based time estimate in hours. Positive values set it (deriving `size` unless `size` is
    /// also given) and exactly 0 clears it. Absent leaves it unchanged.
    estimate_hours: Option<f64>,
    /// JSON object merged into the task's metadata (see Fields). Null clears all metadata, and
    /// absent leaves it unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    #[schema(value_type = Option<Object>)]
    metadata: Option<Option<serde_json::Value>>,
    /// Custom field values by key, merged as Fields describes.
    #[serde(default, deserialize_with = "de_opt_opt")]
    #[schema(value_type = Object, required = false)]
    fields: Option<Option<serde_json::Value>>,
}

/// History events for a task update: one per changed field, diffed between the stored row and the
/// patched one.
fn update_history(
    before: &TaskRow,
    after: &TaskRow,
    users: &[UserRow],
    target_name: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut events = Vec::new();
    if after.name != before.name {
        events.push(("renamed", format!("Renamed to \"{}\"", after.name)));
    }
    if after.description != before.description {
        let detail = if before.description.is_empty() {
            "Added a description"
        } else if after.description.is_empty() {
            "Cleared the description"
        } else {
            "Updated the description"
        };
        events.push(("description_updated", detail.to_string()));
    }
    if after.estimate_hours != before.estimate_hours {
        let detail = match after.estimate_hours {
            Some(h) => format!("Changed the time estimate to {}", format_estimate_hours(h)),
            None => "Cleared the time estimate".to_string(),
        };
        events.push(("estimate_updated", detail));
    } else if after.size != before.size {
        let label = Size::from_i64(after.size).label();
        events.push((
            "estimate_updated",
            format!("Changed the time estimate to {label}"),
        ));
    }
    if after.color_id != before.color_id {
        events.push(("color_updated", "Changed the task color".to_string()));
    }
    if after.labels != before.labels {
        let detail = if after.labels.is_empty() {
            "Cleared the labels".to_string()
        } else {
            format!("Set labels: {}", after.labels.join(", "))
        };
        events.push(("labels_updated", detail));
    }
    if after.due_at != before.due_at || after.due_repeat != before.due_repeat {
        let detail = match after.due_at.as_deref() {
            Some(due) => {
                let mut s = format!("Set the due date to {}", format_datetime(due));
                if let Some(repeat) = after.due_repeat.as_deref() {
                    s.push_str(&format!(" (repeats {repeat})"));
                }
                s
            }
            None => "Cleared the due date".to_string(),
        };
        events.push(("due_updated", detail));
    }
    let due_cleared = after.due_at.is_none() && after.due_at != before.due_at;
    if after.due_target_column_id != before.due_target_column_id && !due_cleared {
        // The target column changed (clearing the date logs itself).
        let detail = match target_name {
            Some(name) if after.due_target_column_id.is_some() => {
                format!("Set the due date target column to {name}")
            }
            _ => "Cleared the due date target column".to_string(),
        };
        events.push(("due_updated", detail));
    }

    // Due-date done flag flipped.
    if after.due_done != before.due_done {
        let detail = if after.due_done {
            "Marked the due date done"
        } else {
            "Marked the due date not done"
        };
        events.push(("due_done_updated", detail.to_string()));
    }
    if after.member_ids != before.member_ids {
        let names: Vec<&str> = after
            .member_ids
            .iter()
            .filter_map(|mid| users.iter().find(|u| &u.id == mid))
            .map(|u| u.username.as_str())
            .collect();
        let detail = if names.is_empty() {
            "Removed all members".to_string()
        } else {
            format!("Assigned members: {}", names.join(", "))
        };
        events.push(("members_updated", detail));
    }
    if after.grouping_date != before.grouping_date {
        events.push((
            "grouping_date_updated",
            "Changed the grouping date".to_string(),
        ));
    }
    events
}

/// Patches a task and returns the task detail. Browser-session callers get the HTML card unless
/// they send `Accept: application/json`.
#[utoipa::path(
    patch,
    path = "/api/tasks/{id}",
    operation_id = "update_task",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("If-Claim-Fence" = Option<u64>, Header, description = "Fence of the caller's claim (see Claims)"),
    ),
    request_body = UpdateTaskInput,
    responses(
        (status = 200, description = "Updated task detail", body = TaskDetail),
        (status = 400, description = "Invalid input"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "`stale_fence`: a live claim holds another fence"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let body_len = body.len();
    let input: UpdateTaskInput = parse_body(&headers, body).await?;
    let db = &state.db;
    let metadata = match &input.metadata {
        Some(Some(serde_json::Value::Object(patch))) => Some(Some(patch)),
        Some(Some(_)) => return Err(AppError::bad_request("metadata must be a JSON object")),
        Some(None) => Some(None),
        None => None,
    };
    if metadata.is_some() && body_len > Db::MAX_METADATA_BYTES {
        return Err(AppError::bad_request("request body exceeds 64 KiB"));
    }

    let fence = claim_fence_header(&headers)?;
    if let Some(name) = input.name.as_deref()
        && name.trim().is_empty()
    {
        return Err(AppError::bad_request("task name is required"));
    }

    // An unparseable due date rejects the request before any field is written. Explicit JSON null
    // clears, empty strings still clear (legacy), and absent leaves the field unchanged.
    let requested_due = match input.due_at.as_ref().map(|inner| inner.as_deref()) {
        Some(Some(s)) if s.trim().is_empty() => Some(None),
        Some(Some(s)) => {
            // A bare date takes the task's board's Due date time.
            let column_id = db
                .get_task(&id)
                .map_err(AppError::from)?
                .ok_or_else(|| AppError::not_found("task not found"))?
                .column_id;
            let due_time = board_due_time(db, &column_id)?;
            Some(Some(
                normalize_due_input(s, due_time).map_err(AppError::bad_request)?,
            ))
        }
        Some(None) => Some(None),
        None => None,
    };
    let requested_repeat = match input.due_repeat.as_ref().map(|inner| inner.as_deref()) {
        Some(Some(s)) if s.trim().is_empty() => Some(None),
        Some(Some(s)) => Some(Some(s.trim().to_string())),
        Some(None) => Some(None),
        None => None,
    };

    // The target column row is looked up here, and that it is on the task's board is checked inside
    // the write transaction
    let due_target = match input
        .due_target_column_id
        .as_ref()
        .map(|inner| inner.as_deref())
    {
        Some(Some(target)) if !target.trim().is_empty() => Some(Some(
            db.get_column(target.trim())
                .map_err(AppError::from)?
                .ok_or_else(|| AppError::bad_request("unknown target column"))?,
        )),
        Some(_) => Some(None),
        None => None,
    };
    let target_name = due_target
        .as_ref()
        .and_then(|target| target.as_ref())
        .map(|column| column.name.clone());

    // Validate the grouping date before any write, like due_at.
    let grouping_date = match input.grouping_date.as_ref().map(|inner| inner.as_deref()) {
        Some(Some(date)) if !date.trim().is_empty() => {
            let date = date.trim();
            parse_grouping_date(date).ok_or_else(|| {
                AppError::bad_request(format!("invalid grouping_date \"{date}\": use YYYY-MM-DD"))
            })?;
            Some(Some(date))
        }
        Some(_) => Some(None),
        None => None,
    };

    // That the color exists on the task's board is checked inside the write transaction.
    let requested_color = match input.color_id.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(requested) => Some(Some(requested)),
    };
    let fields_patch = match &input.fields {
        None => None,
        Some(Some(value @ serde_json::Value::Object(_))) => Some(value),
        Some(_) => return Err(AppError::bad_request("fields must be a JSON object")),
    };
    let estimate = input
        .estimate_hours
        .map(|raw| normalize_estimate_hours(raw).map_err(AppError::bad_request))
        .transpose()?;
    let users = db.list_users().map_err(AppError::from)?;
    let labels = input.labels.as_deref().map(crate::db::clean_labels);
    let actor = user.actor();

    // One write transaction. Every check that depends on stored state (the fence, the merged
    // metadata size, the color's board, the field definitions, the current due date) runs inside
    // it, and a failed check writes nothing.
    let applied = db
        .apply_task_change_in_board(&id, |task, context| {
            // A failed check returns before the row is stored.
            let mut apply = || -> Result<(), AppError> {
                if !task.fence_allows(fence) {
                    return Err(stale_fence());
                }
                let before = task.clone();
                let field_values = fields_patch
                    .map(|patch| check_field_values(&context.fields, patch))
                    .transpose()?;
                let mut size = input.size.map(|size| Size::from_i64(size) as i64);
                let color_id = match requested_color {
                    None => None,
                    Some(None) => Some(None),
                    Some(Some(requested)) => {
                        let color = context
                            .colors
                            .iter()
                            .find(|color| color.id == requested)
                            .ok_or_else(|| AppError::bad_request("unknown color"))?;
                        if size.is_none() {
                            size = Some(color_value_to_size(&color.value));
                        }
                        Some(Some(color.id.clone()))
                    }
                };
                if let Some(patch) = metadata
                    && !crate::db::merge_metadata(task, patch)
                {
                    return Err(AppError::bad_request("metadata exceeds 64 KiB"));
                }

                // Field values merge per key; they are never history.
                for (key, value) in field_values.into_iter().flatten() {
                    if value.is_null() {
                        task.fields.remove(&key);
                    } else {
                        task.fields.insert(key, value);
                    }
                }
                if let Some(name) = input.name.as_deref() {
                    task.name = name.trim().to_string();
                }
                if let Some(description) = input.description.as_deref() {
                    task.description = description.to_string();
                }
                if let Some(size) = size {
                    task.size = size;
                }
                if let Some(color_id) = color_id {
                    task.color_id = color_id;
                }

                // Setting the estimate derives the pomodoro size unless `size` was given explicitly
                // in the same request.
                if let Some(hours) = estimate {
                    task.estimate_hours = hours.map(crate::db::round_estimate);
                    if let (None, Some(hours)) = (size, hours) {
                        task.size = estimate_hours_to_size(hours, context.pomodoro_minutes);
                    }
                }
                if let Some(member_ids) = &input.member_ids {
                    task.member_ids = member_ids.clone();
                }
                if let Some(date) = grouping_date {
                    task.grouping_date = date.map(str::to_string);
                }
                if let Some(labels) = &labels {
                    task.labels = labels.clone();
                }
                if requested_due.is_some() || requested_repeat.is_some() {
                    let due_at = match &requested_due {
                        Some(due) => due.clone(),
                        None => before.due_at.clone(),
                    };

                    // The same instant in another format is not a change, so it keeps the stored
                    // string (and with it `due_done` and the history).
                    let due_at = if same_due_instant(due_at.as_deref(), before.due_at.as_deref()) {
                        before.due_at.clone()
                    } else {
                        due_at
                    };
                    task.due_repeat = match &requested_repeat {
                        Some(repeat) => repeat.clone(),
                        None => before.due_repeat.clone(),
                    };

                    // Clearing or changing the due date also clears its done flag, which belongs to
                    // the old date.
                    if due_at.is_none() || due_at != before.due_at {
                        task.due_done = false;
                    }

                    // No date left, no column to reach by it and no repeat.
                    if due_at.is_none() {
                        task.due_target_column_id = None;
                        task.due_repeat = None;
                    }
                    task.due_at = due_at;
                }

                // The target column must be on the task's board. A target without a due date is a
                // 400.
                match &due_target {
                    Some(Some(column)) => {
                        if context.board_id.as_deref() != Some(column.board_id.as_str()) {
                            return Err(AppError::bad_request("unknown target column"));
                        }
                        if task.due_at.is_none() {
                            return Err(AppError::bad_request(
                                "due_target_column_id needs a due date",
                            ));
                        }
                        task.due_target_column_id = Some(column.id.clone());
                    }
                    Some(None) => task.due_target_column_id = None,
                    None => {}
                }

                // An explicit done flag wins over the reset above.
                if let (Some(done), true) = (input.due_done, task.due_at.is_some()) {
                    task.due_done = done;
                }

                // History: one event per changed field.
                for (kind, detail) in update_history(&before, task, &users, target_name.as_deref())
                {
                    crate::db::push_event(task, kind, detail, actor);
                }
                Ok(())
            };
            match apply() {
                Ok(()) => (Ok(()), true),
                Err(error) => (Err(error), false),
            }
        })
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    applied?;

    if wants_json(&headers, &user) {
        let task = db
            .get_task(&id)
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::not_found("task not found"))?;
        return Ok(Json(TaskDetail::load(db, task)?).into_response());
    }
    let task = fetch_task_view(db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(TaskCardTemplate { task }.into_response())
}

/// Normalize a due-date input into RFC3339 in the server's timezone, with
/// a date-only input due at `due_time`; see [`normalize_due_input_at`].
fn normalize_due_input(raw: &str, due_time: chrono::NaiveTime) -> Result<String, String> {
    normalize_due_input_at(raw, &Local, due_time)
}

/// The board "Due date time" a task in `column_id` gets for a date-only due date, or KanbanFlow's
/// 5:00 PM when the column or board is missing.
fn board_due_time(db: &Db, column_id: &str) -> Result<chrono::NaiveTime, AppError> {
    let Some(column) = db.get_column(column_id).map_err(AppError::from)? else {
        return Ok(default_due_time());
    };
    Ok(db
        .get_board(&column.board_id)
        .map_err(AppError::from)?
        .map_or_else(default_due_time, |board| board.due_time()))
}

/// [`normalize_due_input_at`] with KanbanFlow's default 5:00 PM due time.
#[cfg(test)]
fn normalize_due_input_in<Tz>(raw: &str, tz: &Tz) -> Result<String, String>
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    normalize_due_input_at(raw, tz, default_due_time())
}

/// Normalizes a due-date input into RFC3339, reading offset-less input in `tz`. Errors on
/// anything the due-date renderer could not parse.
///
/// Accepted:
/// 1. RFC3339, kept verbatim.
/// 2. "YYYY-MM-DD HH:MM" or "YYYY-MM-DD HH:MM:SS", with a space or `T`.
/// 3. A bare "YYYY-MM-DD", due at `due_time` that day: the board's "Due date time".
///
/// A local time a DST fall-back repeats resolves to its first (earlier) occurrence, and a local
/// time a spring-forward gap skips is an error.
fn normalize_due_input_at<Tz>(
    raw: &str,
    tz: &Tz,
    due_time: chrono::NaiveTime,
) -> Result<String, String>
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    use chrono::Timelike as _;

    // chrono reads second 60 as a leap second (nanoseconds past 1e9);
    // browsers cannot parse it, so it is rejected in every form.
    let raw = raw.trim();
    let leap_second = || format!("invalid due_at \"{raw}\": seconds must be 00-59");
    if let Ok(due) = DateTime::parse_from_rfc3339(raw) {
        if due.nanosecond() >= 1_000_000_000 {
            return Err(leap_second());
        }
        return Ok(raw.to_string());
    }
    let date_only = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").is_ok();
    let local = if is_local_due_shape(raw) {
        match chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
            Ok(date) => Some(date.and_time(due_time)),
            Err(_) => [
                "%Y-%m-%d %H:%M",
                "%Y-%m-%dT%H:%M",
                "%Y-%m-%d %H:%M:%S",
                "%Y-%m-%dT%H:%M:%S",
            ]
            .iter()
            .find_map(|fmt| chrono::NaiveDateTime::parse_from_str(raw, fmt).ok()),
        }
    } else {
        None
    };
    let local = local.ok_or_else(|| {
        format!(
            "invalid due_at \"{raw}\": use RFC3339, \"YYYY-MM-DD HH:MM[:SS]\", or \"YYYY-MM-DD\""
        )
    })?;
    if local.nanosecond() >= 1_000_000_000 {
        return Err(leap_second());
    }
    resolve_local_time(local, tz)
        .map(|due| due.to_rfc3339())
        .ok_or_else(|| {
            if date_only {
                format!(
                    "invalid due_at \"{raw}\": the board's due date time ({}) falls in a \
                     daylight-saving gap on that date; send a time",
                    due_time.format("%H:%M")
                )
            } else {
                format!(
                    "invalid due_at \"{raw}\": that local time is skipped by a daylight-saving change"
                )
            }
        })
}

/// True when `raw` is laid out as "YYYY-M[M]-D[D]", optionally followed
/// by one space or `T` and "H[H]:M[M]" or "H[H]:M[M]:S[S]".
///
/// chrono's format parser alone also takes a short or signed year and any run of whitespace as the
/// separator.
fn is_local_due_shape(raw: &str) -> bool {
    let digits = |part: &str, min: usize, max: usize| {
        (min..=max).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit())
    };
    let (date, time) = match raw.find([' ', 'T']) {
        Some(at) => (&raw[..at], Some(&raw[at + 1..])),
        None => (raw, None),
    };
    let date: Vec<&str> = date.split('-').collect();
    let date_ok =
        date.len() == 3 && digits(date[0], 4, 4) && digits(date[1], 1, 2) && digits(date[2], 1, 2);
    let time_ok = time.is_none_or(|time| {
        let parts: Vec<&str> = time.split(':').collect();
        (2..=3).contains(&parts.len()) && parts.iter().all(|part| digits(part, 1, 2))
    });
    date_ok && time_ok
}

/// The earliest instant whose wall time in `tz` is exactly `local`, or
/// None when a DST gap skips it.
///
/// Candidates come from the offsets in force a day either side, and each
/// one must convert back to `local`: chrono's own local lookup counts the
/// transition second itself as both ambiguous and valid inside a gap.
fn resolve_local_time<Tz>(local: chrono::NaiveDateTime, tz: &Tz) -> Option<DateTime<Tz>>
where
    Tz: chrono::TimeZone,
{
    use chrono::Offset as _;

    [local - Duration::days(1), local, local + Duration::days(1)]
        .iter()
        .map(|probe| tz.offset_from_utc_datetime(probe).fix())
        .map(|offset| local - Duration::seconds(offset.local_minus_utc().into()))
        .map(|utc| tz.from_utc_datetime(&utc))
        .filter(|due| due.naive_local() == local)
        .min()
}

/// True when two stored due dates name the same instant. A value that is not RFC3339 only matches
/// the identical string.
fn same_due_instant(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            match (
                DateTime::parse_from_rfc3339(a),
                DateTime::parse_from_rfc3339(b),
            ) {
                (Ok(a), Ok(b)) => a == b,
                _ => a == b,
            }
        }
        (a, b) => a == b,
    }
}

/// Overdue means the FULL due timestamp is in the past (and the task is not done), never the due
/// date alone. A task due later today is not overdue, matching KanbanFlow.
fn is_overdue(due_at: Option<&str>, done: bool) -> bool {
    due_at
        .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
        .map(|dt| dt.with_timezone(&Local) < Local::now() && !done)
        .unwrap_or(false)
}

/// Normalizes an hour-based time estimate from the API: positive values set the estimate, exactly 0
/// clears it, anything else (negative, NaN, infinite) is invalid.
fn normalize_estimate_hours(raw: f64) -> Result<Option<f64>, &'static str> {
    if raw == 0.0 {
        Ok(None)
    } else if raw.is_finite() && raw > 0.0 {
        Ok(Some(raw))
    } else {
        Err("estimate_hours must be a positive number of hours (0 clears the estimate)")
    }
}

/// Derives the pomodoro size (1..=4) from an hour estimate using the configured pomodoro length.
/// Hours stay the canonical estimate, and `size` remains the derived/display concept for the
/// pomodoro legend and legacy size-based coloring.
fn estimate_hours_to_size(hours: f64, pomodoro_minutes: u32) -> i64 {
    let pomodoro_minutes = pomodoro_minutes.max(1) as f64;
    ((hours * 60.0 / pomodoro_minutes).round() as i64).clamp(1, 4)
}

/// Render an hour estimate KanbanFlow-style: "4h", "1h 30m", "30m".
/// Returns an hour estimate in whole minutes, 0 when unset.
fn estimate_minutes(hours: Option<f64>) -> i64 {
    hours.map_or(0, |h| (h * 60.0).round() as i64)
}

/// Formats an "Estimated hours" column sum: "0h" when empty, else as [`format_estimate_hours`].
/// `static/app.js` mirrors this in `formatColumnHours`.
fn format_column_hours(minutes: i64) -> String {
    if minutes > 0 {
        format_estimate_hours(minutes as f64 / 60.0)
    } else {
        "0h".to_string()
    }
}

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
    /// Index among the target cell's other tasks. In a Done column it counts them in on-screen
    /// order (date groups: Today first, then future and past days newest first, undated last) and
    /// the task lands beside a neighbor from its own date group.
    position: f64,
}

/// Moves a task to a new column/swimlane/position. Crossing into the Done column stamps
/// `completed_at` and crossing out clears it. Without a swimlane the task keeps its lane, or takes
/// the target board's first lane when the column is on another board. A move to another board also
/// gives the task that board's color of the same value, else its default.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/move",
    operation_id = "move_task",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("If-Claim-Fence" = Option<u64>, Header, description = "Fence of the caller's claim (see Claims)"),
        MoveTaskQuery,
    ),
    request_body = MoveTaskInput,
    responses(
        (status = 200, description = "Moved task detail", body = TaskDetail),
        (status = 400, description = "Unknown column"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "`wip_limit_reached`: column WIP limit reached (n/limit). `stale_fence`: a live claim holds another fence"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn move_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Query(query): Query<MoveTaskQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let input: MoveTaskInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let before = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let fence = claim_fence_header(&headers)?;
    let from_column = db.get_column(&before.column_id).map_err(AppError::from)?;
    let from_name = from_column
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "a column".to_string());
    let to_column = db
        .get_column(&input.column_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::bad_request("unknown column"))?;
    let to_name = to_column.name.clone();

    // Keep the current swimlane when the client doesn't name one.
    let swimlane_id = input.swimlane_id.filter(|s| !s.is_empty());

    // An archiving cell shows its tasks grouped by date, so its drop index counts on-screen cards
    // rather than board order.
    let position = if ColumnSettings::from_column(&to_column).archiving {
        // Same lane rule as `Db::move_task`, including the cross-board default to the target
        // board's first lane.
        let crosses_boards = from_column
            .as_ref()
            .is_some_and(|c| c.board_id != to_column.board_id);
        let lane = match swimlane_id.clone() {
            Some(lane) => Some(lane),
            None if crosses_boards => db
                .list_swimlanes(&to_column.board_id)
                .map_err(AppError::from)?
                .into_iter()
                .next()
                .map(|lane| lane.id),
            None => before.swimlane_id.clone(),
        };
        let cell: Vec<TaskRow> = db
            .board_tasks(&to_column.board_id)
            .map_err(AppError::from)?
            .into_iter()
            .filter(|t| t.column_id == to_column.id && t.swimlane_id == lane)
            .collect();
        let current = cell.iter().position(|t| t.id == id);
        let siblings: Vec<Option<chrono::NaiveDate>> = cell
            .iter()
            .filter(|t| t.id != id)
            .map(|t| {
                done_group_date(
                    t.grouping_date.as_deref(),
                    t.completed_at.as_deref(),
                    t.column_added_at.as_deref(),
                )
            })
            .collect();
        let was_done = from_column.as_ref().is_some_and(|c| c.is_done);
        let now = Some(chrono::Utc::now().to_rfc3339());
        let completed_at = match (to_column.is_done, was_done) {
            (true, false) => now.clone(),
            (true, true) => before.completed_at.clone(),
            (false, _) => None,
        };
        let added_at = if before.column_id == to_column.id {
            before.column_added_at.clone()
        } else {
            now
        };
        let moved = done_group_date(
            before.grouping_date.as_deref(),
            completed_at.as_deref(),
            added_at.as_deref(),
        );
        done_drop_position(
            Local::now().date_naive(),
            &siblings,
            moved,
            input.position,
            current,
        ) as f64
    } else {
        input.position
    };

    // Crossing into the Done column stamps `completed_at` and crossing out clears it. The move,
    // both cells' renumbering, and the history event are one write.
    let moved = format!("Moved from {from_name} to {to_name}");
    let event = (before.column_id != to_column.id).then_some((moved.as_str(), user.actor()));
    let guards = MoveGuards {
        event,
        fence,
        enforce_wip: !query.force.unwrap_or(false),
    };
    match db
        .move_task_logged(
            &id,
            &input.column_id,
            swimlane_id.as_deref(),
            position,
            guards,
        )
        .map_err(AppError::from)?
    {
        MoveOutcome::Moved => {}
        MoveOutcome::NotFound => return Err(AppError::not_found("task not found")),
        MoveOutcome::StaleFence => return Err(stale_fence()),
        MoveOutcome::WipLimit { count, limit } => {
            return Err(
                AppError::conflict(format!("column WIP limit reached ({count}/{limit})"))
                    .with_code(ErrorCode::WipLimitReached),
            );
        }
    }

    // Return the refreshed card so the Done stamp (or its removal) appears
    // without a reload. Done-column date grouping is handled client-side
    // by reloading when the move crosses the Done boundary.
    let task = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if wants_json(&headers, &user) {
        return Ok(Json(TaskDetail::load(db, task)?).into_response());
    }
    let board_id = db
        .get_column(&task.column_id)
        .map_err(AppError::from)?
        .map(|column| column.board_id);
    let view = match board_id {
        Some(board_id) => TaskView::from_row_in_board(db, &board_id, &task)?,
        None => TaskView::from_row(&task),
    };
    Ok(TaskCardTemplate { task: view }.into_response())
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct MoveTaskQuery {
    /// true moves into a column past its WIP limit (the board UI sends it).
    force: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
struct WatchTaskInput {
    watched: bool,
}

/// Sets a task's watch flag (KanbanFlow parity: "Watch" in the task More menu). Persisted per task
/// and returns the new flag.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/watch",
    operation_id = "watch_task",
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
    let (kind, detail) = if input.watched {
        ("watched", "Started watching this task")
    } else {
        ("unwatched", "Stopped watching this task")
    };

    // The flag and its history event are one write.
    state
        .db
        .apply_task_change(&id, |task| {
            task.watched = input.watched;
            crate::db::push_event(task, kind, detail.to_string(), user.actor());
            ((), true)
        })
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(Json(WatchTaskResponse {
        watched: input.watched,
    }))
}

#[derive(serde::Serialize, ToSchema)]
struct WatchTaskResponse {
    watched: bool,
}

/// Deletes a task and its time entries.
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}",
    operation_id = "delete_task",
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
    name: String,
    description: String,
    column_id: String,
    swimlane_id: Option<String>,
    /// The task's color row id, or null when it has none.
    color_id: Option<String>,
    /// RFC3339.
    created_at: String,
    /// RFC3339 time the task entered a Done column, null while it is not done.
    completed_at: Option<String>,
    /// RFC3339 time of the last change to the task.
    updated_at: String,
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
    /// The column the task should reach by its due date, where null means "Any archived column".
    due_target_column_id: Option<String>,
    /// Caller-owned JSON object, or null when unset.
    #[schema(value_type = Option<Object>)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// The task's links, as `GET /api/tasks/{id}/links` lists them.
    links: Vec<TaskLink>,
    /// True when a `blocked_by` target sits in a column that is not Done.
    blocked: bool,
    /// The live claim, or null when unclaimed, released, or expired.
    claim: Option<TaskClaim>,
    /// Custom field values by key.
    #[schema(value_type = Object)]
    fields: serde_json::Map<String, serde_json::Value>,
}

impl TaskDetail {
    /// Builds the detail, resolving links and readiness.
    fn load(db: &Db, task: TaskRow) -> Result<Self, AppError> {
        let blocked = db.task_blocked(&task).map_err(AppError::from)?;
        let links = db
            .list_task_links(&task.id)
            .map_err(AppError::from)?
            .unwrap_or_default();
        Ok(Self {
            claim: task.live_claim().cloned(),
            updated_at: task.last_updated().to_string(),
            id: task.id,
            name: task.name,
            description: task.description,
            column_id: task.column_id,
            swimlane_id: task.swimlane_id,
            color_id: task.color_id,
            created_at: task.created_at,
            completed_at: task.completed_at,
            member_ids: task.member_ids,
            grouping_date: task.grouping_date,
            watched: task.watched,
            subtasks: task.subtasks.iter().map(SubtaskDetail::from).collect(),
            labels: task.labels,
            due_at: task.due_at,
            due_repeat: task.due_repeat,
            due_done: task.due_done,
            due_target_column_id: task.due_target_column_id,
            metadata: task.metadata,
            links,
            blocked,
            fields: task.fields,
        })
    }
}

/// Returns one task: name, description, placement, timestamps, assignment, labels, due date,
/// subtasks, metadata, links, and readiness.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}",
    operation_id = "get_task",
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
    Ok(Json(TaskDetail::load(&state.db, task)?))
}

#[derive(Deserialize, ToSchema)]
struct CreateLinkInput {
    /// `blocked_by`, `blocks`, or `related`.
    #[schema(value_type = LinkKind)]
    kind: String,
    /// The other task.
    target_id: String,
}

/// Links a task to another task. Asking again for an existing link returns it unchanged.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/links",
    operation_id = "create_task_link",
    tag = "Links",
    params(("id" = String, Path, description = "Task id")),
    request_body = CreateLinkInput,
    responses(
        (status = 200, description = "The link, as seen from this task", body = TaskLink),
        (status = 400, description = "unknown link kind, target is the task itself, or link would create a cycle"),
        (status = 404, description = "Task or target not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_task_link(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TaskLink>, AppError> {
    let input: CreateLinkInput = parse_body(&headers, body).await?;
    let kind =
        LinkKind::parse(&input.kind).ok_or_else(|| AppError::bad_request("unknown link kind"))?;
    if input.target_id == id {
        return Err(AppError::bad_request("target is the task itself"));
    }
    match state
        .db
        .add_task_link(&id, kind, &input.target_id, user.actor())
        .map_err(AppError::from)?
    {
        LinkAddOutcome::Added(mut link) => {
            link.board_id = state
                .db
                .task_board_id(&link.target_id)
                .map_err(AppError::from)?;
            Ok(Json(link))
        }
        LinkAddOutcome::NotFound => Err(AppError::not_found("Task or target not found")),
        LinkAddOutcome::Cycle => Err(AppError::bad_request("link would create a cycle")),
    }
}

/// Lists a task's links: the ones it holds, plus a `blocks` or `related` entry for each link
/// another task holds on it. Oldest first.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/links",
    operation_id = "list_task_links",
    tag = "Links",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "The task's links", body = Vec<TaskLink>),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_task_links(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<TaskLink>>, AppError> {
    let links = state
        .db
        .list_task_links(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(Json(links))
}

/// Deletes a link listed on this task, whichever side holds it.
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}/links/{link_id}",
    operation_id = "delete_task_link",
    tag = "Links",
    params(
        ("id" = String, Path, description = "Task id"),
        ("link_id" = String, Path, description = "Link id"),
    ),
    responses(
        (status = 200, description = "Link deleted"),
        (status = 404, description = "Task or link not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_task_link(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path((id, link_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    if state
        .db
        .delete_task_link(&id, &link_id, user.actor())
        .map_err(AppError::from)?
    {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::not_found("link not found"))
    }
}

/// Optional header on move, update, and comment requests.
const IF_CLAIM_FENCE: &str = "if-claim-fence";

/// Parses the optional `If-Claim-Fence` header. The fence is checked
/// against the live claim inside the write transaction.
fn claim_fence_header(headers: &HeaderMap) -> Result<Option<u64>, AppError> {
    headers
        .get(IF_CLAIM_FENCE)
        .map(|raw| {
            raw.to_str()
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .ok_or_else(|| AppError::bad_request("invalid If-Claim-Fence: use an integer"))
        })
        .transpose()
}

fn stale_fence() -> AppError {
    AppError::conflict("stale claim fence").with_code(ErrorCode::StaleFence)
}

/// Longest lease a claim or heartbeat may ask for, in seconds.
const MAX_CLAIM_TTL_SECONDS: i64 = SECONDS_PER_DAY;

fn claim_ttl(ttl_seconds: Option<i64>) -> Result<Option<i64>, AppError> {
    match ttl_seconds {
        Some(ttl) if !(1..=MAX_CLAIM_TTL_SECONDS).contains(&ttl) => Err(AppError::bad_request(
            "ttl_seconds must be between 1 and 86400",
        )),
        ttl => Ok(ttl),
    }
}

#[derive(Deserialize, ToSchema)]
struct ClaimInput {
    /// Who holds the claim. Required.
    #[schema(value_type = String, required = true)]
    claimant: Option<String>,
    /// Lease length in seconds, 1 to 86400. Required.
    #[schema(value_type = i64, required = true)]
    ttl_seconds: Option<i64>,
    role: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    jj_workspace: Option<String>,
    jj_change_id: Option<String>,
    session_id: Option<String>,
    /// Fence of the caller's live claim, to renew it.
    fence: Option<u64>,
}

#[derive(Deserialize, ToSchema)]
struct HeartbeatInput {
    /// Fence of the live claim. Required.
    #[schema(value_type = u64, required = true)]
    fence: Option<u64>,
    /// New lease length in seconds, 1 to 86400.
    ttl_seconds: Option<i64>,
    session_id: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct ReleaseInput {
    /// Fence of the claim to release. Required.
    #[schema(value_type = u64, required = true)]
    fence: Option<u64>,
}

fn claim_conflict(claim: &TaskClaim) -> AppError {
    AppError::conflict(format!(
        "task claimed by {} until {}",
        claim.claimant, claim.expires_at
    ))
    .with_code(ErrorCode::ClaimHeld)
}

/// Claims a task, or renews the caller's own live claim.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/claim",
    operation_id = "claim_task",
    tag = "Claims",
    params(("id" = String, Path, description = "Task id")),
    request_body = ClaimInput,
    responses(
        (status = 200, description = "The claim", body = TaskClaim),
        (status = 400, description = "Missing claimant or ttl_seconds outside 1..86400"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "`claim_held`: task claimed by <claimant> until <expires_at>"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn claim_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TaskClaim>, AppError> {
    let input: ClaimInput = parse_body(&headers, body).await?;
    let claimant = input
        .claimant
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::bad_request("claimant is required"))?;
    let ttl_seconds = claim_ttl(input.ttl_seconds)?
        .ok_or_else(|| AppError::bad_request("ttl_seconds is required"))?;
    let request = ClaimRequest {
        claimant: claimant.to_string(),
        ttl_seconds,
        role: input.role,
        harness: input.harness,
        model: input.model,
        jj_workspace: input.jj_workspace,
        jj_change_id: input.jj_change_id,
        session_id: input.session_id,
        fence: input.fence,
    };
    match state
        .db
        .claim_task(&id, &request, user.actor())
        .map_err(AppError::from)?
    {
        ClaimOutcome::Granted(claim) => Ok(Json(claim)),
        ClaimOutcome::Held(claim) => Err(claim_conflict(&claim)),
        ClaimOutcome::NotFound => Err(AppError::not_found("task not found")),
        ClaimOutcome::Released | ClaimOutcome::FenceMismatch => {
            Err(AppError::internal("unexpected claim outcome"))
        }
    }
}

/// Extends the lease of the live claim holding `fence`.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/claim/heartbeat",
    operation_id = "heartbeat_task_claim",
    tag = "Claims",
    params(("id" = String, Path, description = "Task id")),
    request_body = HeartbeatInput,
    responses(
        (status = 200, description = "The renewed claim", body = TaskClaim),
        (status = 400, description = "Missing fence or ttl_seconds outside 1..86400"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "`stale_fence`: claim fence mismatch or expired"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn heartbeat_claim(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TaskClaim>, AppError> {
    let input: HeartbeatInput = parse_body(&headers, body).await?;
    let fence = input
        .fence
        .ok_or_else(|| AppError::bad_request("fence is required"))?;
    let ttl_seconds = claim_ttl(input.ttl_seconds)?;
    match state
        .db
        .heartbeat_claim(&id, fence, ttl_seconds, input.session_id.as_deref())
        .map_err(AppError::from)?
    {
        ClaimOutcome::Granted(claim) => Ok(Json(claim)),
        ClaimOutcome::NotFound => Err(AppError::not_found("task not found")),
        _ => {
            Err(AppError::conflict("claim fence mismatch or expired")
                .with_code(ErrorCode::StaleFence))
        }
    }
}

/// Releases the claim holding `fence`.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/claim/release",
    operation_id = "release_task_claim",
    tag = "Claims",
    params(("id" = String, Path, description = "Task id")),
    request_body = ReleaseInput,
    responses(
        (status = 200, description = "Released; the task's claim is null"),
        (status = 400, description = "Missing fence"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "`stale_fence`: claim fence mismatch"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn release_claim(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let input: ReleaseInput = parse_body(&headers, body).await?;
    let fence = input
        .fence
        .ok_or_else(|| AppError::bad_request("fence is required"))?;
    match state
        .db
        .release_claim(&id, fence, user.actor())
        .map_err(AppError::from)?
    {
        ClaimOutcome::Released => Ok(StatusCode::OK),
        ClaimOutcome::NotFound => Err(AppError::not_found("task not found")),
        _ => Err(AppError::conflict("claim fence mismatch").with_code(ErrorCode::StaleFence)),
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateSubtaskInput {
    name: String,
    /// Due date/time, same formats as a task's `due_at`. Absent or blank
    /// means none.
    due_at: Option<String>,
}

#[derive(serde::Serialize, ToSchema)]
struct SubtaskDetail {
    id: String,
    name: String,
    done: bool,
    /// Due date/time as RFC3339; null when the subtask has none.
    #[schema(required = true)]
    due_at: Option<String>,
}

impl From<&Subtask> for SubtaskDetail {
    fn from(sub: &Subtask) -> Self {
        Self {
            id: sub.id.clone(),
            name: sub.name.clone(),
            done: sub.done,
            due_at: sub.due_at.clone(),
        }
    }
}

impl Subtask {
    /// The due date as the modal shows it ("Oct 14, 9:00 AM"), if set.
    fn due_label(&self) -> Option<String> {
        self.due_at
            .as_deref()
            .and_then(|due| format_due_modal(due, None))
    }

    /// Whether the subtask is open and past its due date on a task that is
    /// not done (a done task shows no overdue chip of its own).
    fn due_overdue(&self, task_done: bool) -> bool {
        is_overdue(self.due_at.as_deref(), self.done || task_done)
    }
}

#[derive(Deserialize, ToSchema)]
struct UpdateSubtaskInput {
    name: Option<String>,
    done: Option<bool>,
    /// Due date/time, same formats as a task's `due_at`. Explicit JSON
    /// null or a blank string clears it; absent leaves it unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    due_at: Option<Option<String>>,
}

/// Validate a subtask's requested due date against the task's board (a bare
/// date takes the board's Due date time). Outer `None` leaves it unchanged.
fn resolve_subtask_due(
    db: &Db,
    task_id: &str,
    requested: Option<&Option<String>>,
) -> Result<Option<Option<String>>, AppError> {
    let raw = match requested {
        None => return Ok(None),
        Some(None) => return Ok(Some(None)),
        Some(Some(raw)) if raw.trim().is_empty() => return Ok(Some(None)),
        Some(Some(raw)) => raw,
    };
    let task = db
        .get_task(task_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let due_time = board_due_time(db, &task.column_id)?;
    let due = normalize_due_input(raw, due_time).map_err(AppError::bad_request)?;
    Ok(Some(Some(due)))
}

/// History kind and detail for a subtask's due date being set or cleared.
fn subtask_due_event(name: &str, due: Option<&str>) -> (&'static str, String) {
    match due {
        Some(at) => (
            "subtask_due_set",
            format!(
                "Set due date of subtask \"{name}\" to {}",
                format_datetime(at)
            ),
        ),
        None => (
            "subtask_due_cleared",
            format!("Cleared due date of subtask \"{name}\""),
        ),
    }
}

/// Appends a subtask to a task's checklist (KanbanFlow parity).
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/subtasks",
    operation_id = "create_subtask",
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
    let due_at = resolve_subtask_due(&state.db, &id, Some(&input.due_at))?.flatten();
    let sub = Subtask {
        id: uuid::Uuid::new_v4().to_string(),
        name: input.name.trim().to_string(),
        done: false,
        due_at,
    };

    // The subtask and its history event are one write.
    state
        .db
        .apply_task_change(&id, |task| {
            task.subtasks.push(sub.clone());
            let detail = format!("Added subtask \"{}\"", sub.name);
            crate::db::push_event(task, "subtask_added", detail, user.actor());
            if let Some(at) = sub.due_at.as_deref() {
                let (kind, detail) = subtask_due_event(&sub.name, Some(at));
                crate::db::push_event(task, kind, detail, user.actor());
            }
            ((), true)
        })
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(Json(SubtaskDetail::from(&sub)))
}

#[derive(Deserialize, ToSchema)]
struct ReorderSubtasksInput {
    /// Subtask ids in the desired display order (full list; ids not
    /// mentioned keep their relative order after the listed ones).
    order: Vec<String>,
}

/// Reorders a task's subtasks to the given id sequence (KanbanFlow parity: Cmd+Up/Down reorders the
/// focused subtask and the order is persisted).
#[utoipa::path(
    put,
    path = "/api/tasks/{id}/subtasks/order",
    operation_id = "reorder_subtasks",
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

/// Renames a subtask or toggles its done flag.
#[utoipa::path(
    patch,
    path = "/api/tasks/{id}/subtasks/{sub_id}",
    operation_id = "update_subtask",
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
    if let Some(name) = input.name.as_deref()
        && name.trim().is_empty()
    {
        return Err(AppError::bad_request("subtask name is required"));
    }
    let requested_due = resolve_subtask_due(&state.db, &id, input.due_at.as_ref())?;

    // The edit and its history event are one write, and an unknown subtask writes nothing
    let updated = state
        .db
        .apply_task_change(&id, |task| {
            let Some(sub) = task.subtasks.iter_mut().find(|s| s.id == sub_id) else {
                return (None, false);
            };
            if let Some(name) = input.name.as_deref() {
                sub.name = name.trim().to_string();
            }
            if let Some(done) = input.done {
                sub.done = done;
            }
            let due_event = requested_due
                .as_ref()
                .filter(|due| **due != sub.due_at)
                .map(|due| subtask_due_event(&sub.name, due.as_deref()));
            if let Some(due) = requested_due.clone() {
                sub.due_at = due;
            }
            let sub = sub.clone();
            let event = match input.done {
                Some(true) => Some((
                    "subtask_done",
                    format!("Completed subtask \"{}\"", sub.name),
                )),
                Some(false) => Some((
                    "subtask_reopened",
                    format!("Reopened subtask \"{}\"", sub.name),
                )),
                None if input.name.is_some() => Some((
                    "subtask_renamed",
                    format!("Renamed subtask to \"{}\"", sub.name),
                )),
                None => None,
            };
            for (kind, detail) in event.into_iter().chain(due_event) {
                crate::db::push_event(task, kind, detail, user.actor());
            }
            (Some(sub), true)
        })
        .map_err(AppError::from)?
        .flatten()
        .ok_or_else(|| AppError::not_found("task or subtask not found"))?;
    Ok(Json(SubtaskDetail::from(&updated)))
}

/// Removes a subtask from a task's checklist.
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}/subtasks/{sub_id}",
    operation_id = "delete_subtask",
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
    // The removal and its history event are one write, and an unknown subtask writes nothing
    let removed = state
        .db
        .apply_task_change(&id, |task| {
            let Some(index) = task.subtasks.iter().position(|s| s.id == sub_id) else {
                return (false, false);
            };
            let sub = task.subtasks.remove(index);
            let detail = format!("Deleted subtask \"{}\"", sub.name);
            crate::db::push_event(task, "subtask_deleted", detail, user.actor());
            (true, true)
        })
        .map_err(AppError::from)?
        .unwrap_or(false);
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

// ---- Task extras: labels, due dates, comments, attachments, history ---- (KanbanFlow parity)

/// Lists the distinct labels used on a board, task labels plus time-entry labels, sorted. Powers
/// the "Add labels..." suggestions in the Labels sub-dialog and in the manual/edit time dialogs.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/labels",
    operation_id = "list_board_labels",
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

/// Longest comment `kind` tag, in characters.
const MAX_COMMENT_KIND_CHARS: usize = 64;

#[derive(Deserialize, ToSchema)]
struct CreateCommentInput {
    body: String,
    /// Optional display name to attribute the comment to. Defaults to the authenticated username
    /// (`api-token` for API-token requests).
    author: Option<String>,
    /// Optional free-form tag, e.g. "agent", "oracle", "review", "system";
    /// at most 64 characters. Blank means untagged.
    kind: Option<String>,
}

/// Lists a task's comments as JSON, oldest first.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/comments",
    operation_id = "list_comments",
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

/// Adds a comment to a task. Returns the created comment.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/comments",
    operation_id = "create_comment",
    tag = "Tasks",
    params(
        ("id" = String, Path, description = "Task id"),
        ("If-Claim-Fence" = Option<u64>, Header, description = "Fence of the caller's claim (see Claims)"),
    ),
    request_body = CreateCommentInput,
    responses(
        (status = 200, description = "The created comment", body = TaskComment),
        (status = 400, description = "Comment body is required, too long, or kind over 64 characters"),
        (status = 404, description = "Task not found"),
        (status = 409, description = "`stale_fence`: a live claim holds another fence"),
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
    let fence = claim_fence_header(&headers)?;
    let text = input.body.trim();
    if text.is_empty() {
        return Err(AppError::bad_request("comment body is required"));
    }
    if text.len() > 5000 {
        return Err(AppError::bad_request(
            "comment is too long (max 5000 chars)",
        ));
    }
    let kind = input
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty());
    if kind.is_some_and(|kind| kind.chars().count() > MAX_COMMENT_KIND_CHARS) {
        return Err(AppError::bad_request(
            "comment kind must be at most 64 characters",
        ));
    }

    // API callers can attribute the comment to a display name, and blank falls back to the
    // authenticated username
    let author: &str = input
        .author
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&user.username);
    match state
        .db
        .add_comment(
            &id,
            NewComment {
                author,
                body: text,
                actor_kind: if user.kind == CredentialKind::Session {
                    ActorKind::User
                } else {
                    ActorKind::ApiToken
                },
                token_name: user.token_name.as_deref(),
                kind,
            },
            user.actor(),
            fence,
        )
        .map_err(AppError::from)?
    {
        FencedWrite::Written(comment) => Ok(Json(comment)),
        FencedWrite::NotFound => Err(AppError::not_found("task not found")),
        FencedWrite::StaleFence => Err(stale_fence()),
    }
}

/// Deletes a comment from a task.
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}/comments/{comment_id}",
    operation_id = "delete_comment",
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
        .delete_comment(&id, &comment_id, user.actor())
        .map_err(AppError::from)?;
    if !deleted {
        return Err(AppError::not_found("task or comment not found"));
    }
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

/// Request body limit for attachment uploads: the base64 encoding of a
/// `Db::MAX_ATTACHMENT_BYTES` file plus room for the JSON name and mime.
const ATTACHMENT_BODY_LIMIT: usize =
    (Db::MAX_ATTACHMENT_BYTES as usize).div_ceil(3) * 4 + 64 * 1024;

/// Uploads a file attachment to a task. The bytes are stored in the database (10 MiB cap) and
/// downloaded via the file route. Returns the stored attachment metadata.
///
/// Keep the returned `id`, since no operation lists a task's attachments.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/attachments",
    operation_id = "upload_attachment",
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
        .add_attachment(&id, name, &mime, &bytes, user.actor())
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(Json(attachment))
}

/// Downloads an attachment's bytes.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/attachments/{attachment_id}/file",
    operation_id = "download_attachment",
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

/// Deletes an attachment from a task.
#[utoipa::path(
    delete,
    path = "/api/tasks/{id}/attachments/{attachment_id}",
    operation_id = "delete_attachment",
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
        .delete_attachment(&id, &attachment_id, user.actor())
        .map_err(AppError::from)?;
    if !deleted {
        return Err(AppError::not_found("task or attachment not found"));
    }
    Ok(StatusCode::OK)
}

/// Dedicated in-modal time-log sub-view: "← {task name}" back header, "Time log" title, "+ ADD
/// ENTRY", and day-grouped entries, each with avatar + member name + duration + 12h From-To range +
/// red trash icon.
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
    Ok(TimeLogViewTemplate {
        task_id: id,
        task_name: task.name,
        groups: group_entries_by_day(entries, dates),
    })
}

/// Dedicated in-modal History sub-view: the task's activity trail (created, moves, edits, timer
/// sessions, comments, attachments), newest first.
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
            actor: match &e.token_name {
                Some(name) => format!("{} ({name})", e.actor),
                None => e.actor.clone(),
            },
            created_display: format_datetime(&e.created_at),
        })
        .collect();
    Ok(HistoryViewTemplate {
        task_id: id,
        task_name: task.name,
        events,
    })
}

/// Deletes a time entry (powers the red trash icon in the time-log sub-view).
#[utoipa::path(
    delete,
    path = "/api/time/entries/{id}",
    operation_id = "delete_time_entry",
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
        user.actor(),
    );
    Ok(StatusCode::OK)
}

/// Lists the board-member roster (KanbanFlow parity). In the single-admin model this is the
/// registered users list.
#[utoipa::path(
    get,
    path = "/api/members",
    operation_id = "list_members",
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
    let mut task =
        fetch_task_view(db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;

    // Count the entries, so deleting or editing one keeps it right.
    let completed = db
        .list_entries(&id)
        .map_err(AppError::from)?
        .iter()
        .filter(|entry| {
            EntryKind::parse(&entry.kind).is_some_and(EntryKind::is_pomodoro) && !entry.interrupted
        })
        .count();
    task.pomodori_label = format_pomodori(completed as u32);
    let column = db.get_column(&task.column_id).map_err(AppError::from)?;
    let is_done = column.as_ref().map(|c| c.is_done).unwrap_or(false);
    let column_name = column
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "Unknown column".to_string());
    let row = db
        .get_task(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    let created_short = format_relative_day(&row.created_at);
    let defs = match column.as_ref() {
        Some(column) => db.list_fields(&column.board_id).map_err(AppError::from)?,
        None => Vec::new(),
    };
    let (user_fields, system_fields) = field_views(&defs, &row.fields);
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
        user_fields,
        system_fields,
    })
}

#[derive(Deserialize, ToSchema)]
struct LogTimeInput {
    /// Minutes to log, 1 to 1440.
    minutes: i64,
    /// Free-text note stored with the entry.
    note: Option<String>,
}

/// Logs minutes on a task and returns the task's refreshed time entries, newest first. Browser-session
/// callers get the HTML fragment unless they send `Accept: application/json`.
#[utoipa::path(
    post,
    path = "/api/tasks/{id}/time",
    operation_id = "log_task_time",
    tag = "Time",
    params(("id" = String, Path, description = "Task id")),
    request_body = LogTimeInput,
    responses(
        (status = 200, description = "The task's time entries, newest first", body = Vec<TimeEntryItem>),
        (status = 400, description = "minutes outside 1 to 1440"),
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
) -> Result<Response, AppError> {
    let input: LogTimeInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if !(1..=24 * 60).contains(&input.minutes) {
        return Err(AppError::bad_request("minutes must be between 1 and 1440"));
    }

    // The entry and its history event are one write.
    let note = input.note.unwrap_or_default();
    if !db
        .log_task_time(&id, input.minutes, &note, user.actor())
        .map_err(AppError::from)?
    {
        return Err(AppError::not_found("task not found"));
    }

    time_entries_response(db, &id, TimeEntriesFormat::of(&headers, &user))
}

enum TimeEntriesFormat {
    Json,
    Html,
}

impl TimeEntriesFormat {
    fn of(headers: &HeaderMap, user: &AuthUser) -> Self {
        if wants_json(headers, user) {
            Self::Json
        } else {
            Self::Html
        }
    }
}

fn time_entries_response(
    db: &Db,
    task_id: &str,
    format: TimeEntriesFormat,
) -> Result<Response, AppError> {
    let fallback = fallback_username(db);
    if matches!(format, TimeEntriesFormat::Json) {
        let items: Vec<TimeEntryItem> = db
            .list_entries(task_id)
            .map_err(AppError::from)?
            .into_iter()
            .map(|row| TimeEntryItem::from_row(row, &fallback))
            .collect();
        return Ok(Json(items).into_response());
    }
    Ok(TimeEntriesTemplate {
        entries: fetch_entries(db, task_id, &fallback)?,
    }
    .into_response())
}

/// Lists a task's time entries, newest first. Browser-session callers get the HTML fragment used
/// to refresh the modal log unless they send `Accept: application/json`.
#[utoipa::path(
    get,
    path = "/api/tasks/{id}/time",
    operation_id = "get_task_time",
    tag = "Time",
    params(("id" = String, Path, description = "Task id")),
    responses(
        (status = 200, description = "The task's time entries, newest first", body = Vec<TimeEntryItem>),
        (status = 404, description = "Task not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn get_task_time(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let db = &state.db;
    if db.get_task(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("task not found"));
    }
    time_entries_response(db, &id, TimeEntriesFormat::of(&headers, &user))
}

/// Returns a single time entry as JSON (powers the edit-entry dialog).
#[utoipa::path(
    get,
    path = "/api/time/entries/{id}",
    operation_id = "get_time_entry",
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
    let seconds = entry.duration_seconds();
    Ok(Json(TimeEntryDetail {
        id: entry.id,
        task_id: entry.task_id,
        task_name,
        minutes: entry.minutes,
        seconds,
        note: entry.note,
        started_at: entry.started_at,
        labels: entry.labels,
    }))
}

/// Standalone Time spent report page. Title "Time spent", top-left Filter / Print / Export buttons,
/// and a close × back to the board. The timer log itself is a modal on the board page, reached
/// from the timer popup's Log.
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

/// Export report page: full-page report with format buttons that download the board's tasks (Excel,
/// CSV, XML and JSON, KanbanFlow's documented export formats).
async fn export_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<ExportTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(ExportTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

/// Task count report page: number of tasks in each column, broken down by color (KanbanFlow's "Time
/// estimate & Task count" report layout). Renders client-side from the shared export-data dataset.
#[derive(Template)]
#[template(path = "task_count.html")]
struct TaskCountTemplate {
    board_id: String,
    board_name: String,
}

async fn task_count_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<TaskCountTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(TaskCountTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

/// Throughput report page: tasks completed per week as a line chart (KanbanFlow's Throughput
/// chart). Renders client-side from the shared export-data dataset.
#[derive(Template)]
#[template(path = "throughput.html")]
struct ThroughputTemplate {
    board_id: String,
    board_name: String,
}

async fn throughput_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<ThroughputTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(ThroughputTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

/// Burndown report page: tasks remaining to be done per day as a line chart (KanbanFlow's Burndown
/// chart: "shows how many tasks remain to be done"). Renders client-side from the shared
/// export-data dataset.
#[derive(Template)]
#[template(path = "burndown.html")]
struct BurndownTemplate {
    board_id: String,
    board_name: String,
}

/// Cycle & lead time report page: average cycle/lead time per week as a line chart with a
/// moving-average trend (KanbanFlow's Cycle & lead time chart). Renders client-side from the shared
/// export-data dataset.
#[derive(Template)]
#[template(path = "cycle_lead.html")]
struct CycleLeadTemplate {
    board_id: String,
    board_name: String,
}

async fn burndown_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<BurndownTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(BurndownTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

async fn cycle_lead_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<CycleLeadTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(CycleLeadTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}
/// Cumulative flow report page: tasks in each column over time as a stacked area chart
/// (KanbanFlow's Cumulative Flow diagram: "shows how many tasks there are in different columns over
/// time"). Renders client-side from the shared export-data dataset.
#[derive(Template)]
#[template(path = "cumulative_flow.html")]
struct CumulativeFlowTemplate {
    board_id: String,
    board_name: String,
}

async fn cumulative_flow_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<CumulativeFlowTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(CumulativeFlowTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

/// Due date performance report page: how well tasks meet their due dates (on time / late / overdue
/// / upcoming). Renders client-side from the shared export-data dataset.
#[derive(Template)]
#[template(path = "due_date.html")]
struct DueDateTemplate {
    board_id: String,
    board_name: String,
}

async fn due_date_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<DueDateTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(DueDateTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

/// Board history report page: a detailed chronological log of changes made on the board
/// (KanbanFlow's Board history: filter the history log by user or type of change, and compare a
/// field's value side-by-side before and after the change). Renders client-side from the shared
/// export-data dataset.
#[derive(Template)]
#[template(path = "board_history.html")]
struct BoardHistoryTemplate {
    board_id: String,
    board_name: String,
}

async fn board_history_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<BoardHistoryTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(BoardHistoryTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

/// Calendar report page: tasks laid out by due date on a month grid (KanbanFlow's Calendar: "a
/// better overview of when your tasks are due", filterable by user, color, labels, swimlane).
/// Renders client-side from the shared export-data dataset.
#[derive(Template)]
#[template(path = "calendar.html")]
struct CalendarTemplate {
    board_id: String,
    board_name: String,
}

async fn calendar_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<CalendarTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(CalendarTemplate {
        board_id: board.id,
        board_name: board.name,
    })
}

/// Monte Carlo forecasting report page: predicts when tasks will be completed by running thousands
/// of simulations sampled from the board's historical throughput (KanbanFlow's Monte Carlo
/// forecasting: "predict when your tasks will be completed"). Renders client-side from the shared
/// export-data dataset.
#[derive(Template)]
#[template(path = "monte_carlo.html")]
struct MonteCarloTemplate {
    board_id: String,
    board_name: String,
    done_column_id: String,
}

async fn monte_carlo_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<MonteCarloTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;

    // Resolve the Done column from runtime configuration (never bolted to
    // a column name); the forecast's target column defaults to it.
    let done_column_id = state
        .db
        .list_columns(&board_id)
        .map_err(AppError::from)?
        .into_iter()
        .find(|c| c.is_done)
        .map(|c| c.id)
        .unwrap_or_default();
    Ok(MonteCarloTemplate {
        board_id: board.id,
        board_name: board.name,
        done_column_id,
    })
}

/// Time estimate report page: estimated workload by color and column, summed from each task's time
/// estimate (KanbanFlow's Time estimate report: "an overview of the estimated workload on your
/// board"). Renders client-side from the shared export-data dataset.
#[derive(Template)]
#[template(path = "time_estimate.html")]
struct TimeEstimateTemplate {
    board_id: String,
    board_name: String,
}

async fn time_estimate_page(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<TimeEstimateTemplate, AppError> {
    let board = state
        .db
        .get_board(&board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(TimeEstimateTemplate {
        board_id: board.id,
        board_name: board.name,
    })
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

/// Returns paginated timer log entries, newest first ("Load more").
#[utoipa::path(
    get,
    path = "/api/timer/log",
    operation_id = "get_timer_log",
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
        entries.retain(|e| local_day(&e.started_at).is_some_and(|d| d.as_str() >= from.as_str()));
    }
    if let Some(to) = q.to.filter(|s| !s.is_empty()) {
        entries.retain(|e| local_day(&e.started_at).is_some_and(|d| d.as_str() <= to.as_str()));
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
                .unwrap_or_else(|| {
                    if e.task_id.is_empty() {
                        NO_TASK_NAME
                    } else {
                        "(deleted task)"
                    }
                    .to_string()
                });
            let (board_id, board_name) = match board_of.get(&e.task_id) {
                Some((id, name)) => (Some(id.clone()), Some(name.clone())),
                None => (None, None),
            };
            let (start_display, end_display, range) =
                match DateTime::parse_from_rfc3339(&e.started_at) {
                    Ok(dt) => {
                        let local = dt.with_timezone(&Local);
                        let end = local + Duration::seconds(e.duration_seconds());
                        (
                            local.format("%b %d, %Y").to_string(),
                            end.format("%-I:%M %p").to_string(),
                            format!(
                                "{} - {}",
                                local.format("%-I:%M %p"),
                                end.format("%-I:%M %p")
                            ),
                        )
                    }
                    Err(_) => (e.started_at.clone(), String::new(), String::new()),
                };
            let (badge_code, badge_title) =
                entry_badge(&e.kind, e.interrupted, e.interrupt_reason.as_deref());
            let seconds = e.duration_seconds();
            let task_color = task_dot_color(db, &e.task_id);
            TimerLogEntry {
                id: e.id,
                task_id: e.task_id,
                task_name,
                board_id,
                board_name,
                minutes: e.minutes,
                seconds,
                duration_display: format_entry_duration(e.seconds, e.minutes),
                kind: e.kind,
                badge_code: badge_code.to_string(),
                badge_title,
                interrupted: e.interrupted,
                interrupt_reason: e.interrupt_reason,
                note: e.note,
                day_key: local_day(&e.started_at).unwrap_or_default(),
                date: start_display,
                time_range: range,
                end: end_display,
                labels: e.labels,
                task_color,
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

/// Returns daily time totals for the Time spent report.
#[utoipa::path(
    get,
    path = "/api/timer/time-spent",
    operation_id = "get_time_spent",
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
        if let Some(day) = local_day(&e.started_at) {
            let date = day.as_str();
            if date < from_s.as_str() || date > to_s.as_str() {
                continue;
            }

            // Entry-type filter. "tasks" = pomodoro/stopwatch/manual, "breaks" =
            // short_break/long_break, anything else (or absent) = all.
            let is_break = EntryKind::parse(&e.kind).is_some_and(EntryKind::is_break);
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
            if let Some(ref cid) = color_id
                && task.as_ref().and_then(|t| t.color_id.as_deref()) != Some(cid.as_str())
            {
                continue;
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
                let (name, color) = task_meta.get(task_id).cloned().unwrap_or_else(|| {
                    let name = if task_id.is_empty() {
                        NO_TASK_NAME
                    } else {
                        "(deleted task)"
                    };
                    (name.to_string(), None)
                });
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

/// Returns pomodoro statistics.
#[utoipa::path(
    get,
    path = "/api/timer/statistics",
    operation_id = "get_timer_statistics",
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
        let Some(day) = local_day(&e.started_at) else {
            return false;
        };
        let date = day.as_str();
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
        .filter(|e| EntryKind::parse(&e.kind).is_some_and(EntryKind::is_pomodoro) && in_scope(e))
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
    let mut daily_minutes: HashMap<String, i64> = HashMap::new();
    for e in &pomodori {
        if let Some(date) = local_day(&e.started_at) {
            *daily_counts.entry(date.clone()).or_insert(0) += 1;
            *daily_minutes.entry(date.to_string()).or_insert(0) += e.minutes;
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
            minutes: daily_minutes.get(&key).copied().unwrap_or(0),
        });
        d += Duration::days(1);
    }

    // Highscores: best day and longest streak within the range.
    let best_day = days
        .iter()
        .filter(|day| day.pomodori > 0)
        .max_by_key(|day| day.pomodori)
        .cloned();
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

    // Break-activity statistics: per defined activity, total logged break minutes in the range plus
    // today's minutes for the daily-goal progress shown on the Break activities tab.
    let settings = db.get_settings().map_err(AppError::from)?;
    let today_s = today.format("%Y-%m-%d").to_string();
    let mut activity_minutes: HashMap<String, i64> = HashMap::new();
    let mut activity_today: HashMap<String, i64> = HashMap::new();
    for e in entries.iter().filter(|e| {
        EntryKind::parse(&e.kind).is_some_and(EntryKind::is_break)
            && e.activity_id.is_some()
            && in_scope(e)
    }) {
        if let Some(aid) = e.activity_id.as_deref() {
            *activity_minutes.entry(aid.to_string()).or_insert(0) += e.minutes;
            if local_day(&e.started_at).as_deref() == Some(today_s.as_str()) {
                *activity_today.entry(aid.to_string()).or_insert(0) += e.minutes;
            }
        }
    }
    let break_activities: Vec<BreakActivityStat> = settings
        .break_activities
        .iter()
        .map(|a| BreakActivityStat {
            id: a.id.clone(),
            name: a.name.clone(),
            minutes: activity_minutes.get(&a.id).copied().unwrap_or(0),
            today_minutes: activity_today.get(&a.id).copied().unwrap_or(0),
            daily_goal: a.daily_goal,
            daily_limit: a.daily_limit,
        })
        .collect();

    Ok(Json(TimerStatisticsReport {
        total_pomodori,
        total_minutes,
        avg_minutes,
        interruptions,
        by_reason,
        daily: days,
        best_day,
        longest_streak,
        break_activities,
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

/// Creates a time entry from the "Add time manually" dialog with an explicit date and from/to.
/// Duration is auto-computed, and future times are rejected with KanbanFlow's exact message while
/// the dialog keeps its state (client-side).
#[utoipa::path(
    post,
    path = "/api/time/manual",
    operation_id = "create_manual_time_entry",
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

    let (started_at, seconds) = parse_manual_range(&input.date, &input.from, &input.to)?;
    let minutes = seconds / 60;
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
        user.actor(),
    );

    Ok(Json(IdMinutesResult { id, minutes }))
}

/// Parse a manual date + from/to range into a start DateTime and its length
/// in seconds.
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
    Ok((start, (end - start).num_seconds()))
}

#[derive(Deserialize, ToSchema)]
struct UpdateTimeEntryInput {
    /// The entry's task. Required for an entry that has a task, and a task-less entry stays
    /// task-less when this is absent or empty.
    task_id: Option<String>,
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

/// Edits a time entry: date/time, task reassignment, and note. Date edits re-bucket the entry via
/// its new started_at.
#[utoipa::path(
    put,
    path = "/api/time/entries/{id}",
    operation_id = "update_time_entry",
    tag = "Time",
    params(("id" = String, Path, description = "Time entry id")),
    request_body = UpdateTimeEntryInput,
    responses(
        (status = 200, description = "Updated entry id and minutes", body = IdMinutesResult),
        (status = 400, description = "Invalid date/time range, or no task_id for an entry that has a task"),
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

    let (started_at, seconds) = parse_manual_range(&input.date, &input.from, &input.to)?;
    let minutes = seconds / 60;
    if started_at > Local::now() {
        return Err(AppError::bad_request(
            "You can not enter a time in the future",
        ));
    }

    // Timer entries keep second precision, so a 24 s session saves.
    if seconds < 1 {
        return Err(AppError::bad_request("End time must be after start time"));
    }
    let existing = db
        .get_entry(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("entry not found"))?;
    let task_id = match input.task_id.as_deref().filter(|task| !task.is_empty()) {
        Some(task_id) => {
            if db.get_task(task_id).map_err(AppError::from)?.is_none() {
                return Err(AppError::not_found("task not found"));
            }
            task_id
        }
        None if existing.task_id.is_empty() => "",
        None => return Err(AppError::bad_request("task_id is required")),
    };

    let updated = db
        .update_entry(
            &id,
            task_id,
            seconds,
            &input.note.unwrap_or_default(),
            &started_at.to_rfc3339(),
            input.labels.as_deref().unwrap_or(&[]),
        )
        .map_err(AppError::from)?;
    if !updated {
        return Err(AppError::not_found("entry not found"));
    }

    if !task_id.is_empty() {
        log_history(
            db,
            task_id,
            "time_edited",
            &format!("Edited a time entry ({minutes}m)"),
            user.actor(),
        );
    }

    Ok(Json(IdMinutesResult { id, minutes }))
}

// ---- Column & swimlane management ----

/// Resolves the board a create call targets. The id is mandatory so a call never lands on a
/// board the caller did not name.
fn resolve_board(db: &Db, board_id: Option<String>) -> Result<String, AppError> {
    let id = board_id.filter(|s| !s.is_empty()).ok_or_else(|| {
        AppError::bad_request("board_id is required").with_code(ErrorCode::BoardIdRequired)
    })?;
    match db.board_exists(&id).map_err(AppError::from)? {
        true => Ok(id),
        false => Err(AppError::bad_request("unknown board_id")),
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateColumnInput {
    /// Board that receives the column (required).
    #[schema(value_type = String, required = true)]
    board_id: Option<String>,
    name: String,
    wip_limit: Option<i64>,
}

/// Creates a column at the end of the board's column order and returns its id.
#[utoipa::path(
    post,
    path = "/api/columns",
    operation_id = "create_column",
    tag = "Columns",
    request_body = CreateColumnInput,
    responses(
        (status = 200, description = "Created column id", body = IdResult),
        (status = 400, description = "`board_id_required`: no board_id, or `invalid_request`: unknown board_id or other invalid input"),
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
    if let Some(limit) = input.wip_limit
        && limit < 1
    {
        return Err(AppError::bad_request("WIP limit must be at least 1"));
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
    /// WIP limit. Explicit JSON null clears the limit and a number sets it. Absent leaves it
    /// unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    wip_limit: Option<Option<i64>>,
    is_done: Option<bool>,
    /// Column description (empty string clears it).
    description: Option<String>,
    /// Collapse/expand the column on the board.
    collapsed: Option<bool>,
    /// Settings bag for the column dialog, sent as a string of valid JSON. The keys with an effect
    /// are `sorting` (`none`, `due`, `color`, `name`, `created` or `added`) for the order of the
    /// column's tasks, `column_sum` (`none`, `count` or `hours`) for the header badge, and
    /// `group_by_date` (true or false, true by default for a Done column) for date headers.
    config_json: Option<String>,
}

/// Renames a column, sets or clears its WIP limit, flips its done flag, and updates its
/// description, collapsed state, and settings bag.
#[utoipa::path(
    patch,
    path = "/api/columns/{id}",
    operation_id = "update_column",
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

    // Validate every field before writing any, then write once.
    let name = input.name.as_deref().map(str::trim);
    if name == Some("") {
        return Err(AppError::bad_request("column name is required"));
    }
    if matches!(input.wip_limit, Some(Some(limit)) if limit < 1) {
        return Err(AppError::bad_request("WIP limit must be at least 1"));
    }
    if let Some(config_json) = input.config_json.as_deref() {
        serde_json::from_str::<serde_json::Value>(config_json)
            .map_err(|_| AppError::bad_request("config_json must be valid JSON"))?;
    }
    let patch = crate::db::ColumnPatch {
        name,
        wip_limit: input.wip_limit,
        is_done: input.is_done,
        description: input.description.as_deref().map(str::trim),
        collapsed: input.collapsed,
        config_json: input.config_json.as_deref(),
    };
    if !db.update_column(&id, &patch).map_err(AppError::from)? {
        return Err(AppError::not_found("column not found"));
    }
    Ok(StatusCode::OK)
}

#[derive(Deserialize, ToSchema)]
struct MoveColumnInput {
    position: i64,
}

/// Reorders a column within its board, and positions are renumbered densely.
#[utoipa::path(
    post,
    path = "/api/columns/{id}/move",
    operation_id = "move_column",
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

/// Deletes a column. Refuses with 400 while it still holds tasks.
#[utoipa::path(
    delete,
    path = "/api/columns/{id}",
    operation_id = "delete_column",
    tag = "Columns",
    params(("id" = String, Path, description = "Column id")),
    responses(
        (status = 200, description = "Column deleted"),
        (status = 400, description = "Column still holds tasks, or is the board's last column"),
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

    // The last-column and task checks run inside the delete.
    match db.delete_column(&id).map_err(AppError::from)? {
        ColumnDeleteOutcome::Deleted => Ok(StatusCode::OK),
        ColumnDeleteOutcome::NotFound => Err(AppError::not_found("column not found")),
        ColumnDeleteOutcome::RefusedLast => Err(AppError::bad_request(
            "cannot delete the board's last column",
        )),
        ColumnDeleteOutcome::RefusedHasTasks(count) => Err(AppError::bad_request(format!(
            "cannot delete column with {count} task(s); move or delete them first"
        ))),
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateSwimlaneInput {
    /// Board that receives the swimlane (required).
    #[schema(value_type = String, required = true)]
    board_id: Option<String>,
    name: String,
}

/// Creates a swimlane at the end of the board's swimlane order and returns its id.
#[utoipa::path(
    post,
    path = "/api/swimlanes",
    operation_id = "create_swimlane",
    tag = "Swimlanes",
    request_body = CreateSwimlaneInput,
    responses(
        (status = 200, description = "Created swimlane id", body = IdResult),
        (status = 400, description = "`board_id_required`: no board_id, or `invalid_request`: unknown board_id or other invalid input"),
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

/// Renames a swimlane.
#[utoipa::path(
    patch,
    path = "/api/swimlanes/{id}",
    operation_id = "update_swimlane",
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

/// Reorders a swimlane within its board, and positions are renumbered densely.
#[utoipa::path(
    post,
    path = "/api/swimlanes/{id}/move",
    operation_id = "move_swimlane",
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

/// Deletes a swimlane. Refuses with 400 while it still holds tasks.
#[utoipa::path(
    delete,
    path = "/api/swimlanes/{id}",
    operation_id = "delete_swimlane",
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

    // The board must keep at least one swimlane, because with zero the column "+" add-task buttons
    // silently do nothing.
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

/// Lists all boards as id/name pairs. Archived boards are excluded unless `?include_archived=true`
/// is passed.
#[utoipa::path(
    get,
    path = "/api/boards",
    operation_id = "list_boards",
    tag = "Boards",
    params(("include_archived" = Option<bool>, Query, description = "Include archived boards")),
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

/// One board's full dataset for the report views: the task rows plus the lookup tables. This is the
/// shared data shape the board reports (Export, Burndown, Cumulative flow, ...) consume, and each
/// report resolves display names client-side from the lookups.
#[derive(Clone, Debug, Serialize, ToSchema)]
struct BoardExportData {
    board: ExportBoardRef,
    columns: Vec<ExportColumnRef>,
    swimlanes: Vec<ExportNamedRef>,
    colors: Vec<ExportColorRef>,
    users: Vec<ExportUserRef>,
    tasks: Vec<TaskRow>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
struct ExportBoardRef {
    id: String,
    name: String,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
struct ExportNamedRef {
    id: String,
    name: String,
}

/// A column in the export dataset, with its names before renames.
#[derive(Clone, Debug, Serialize, ToSchema)]
struct ExportColumnRef {
    id: String,
    name: String,
    former_names: Vec<String>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
struct ExportColorRef {
    id: String,
    value: String,
    label: String,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
struct ExportUserRef {
    id: String,
    username: String,
}

/// Assemble the export dataset from its parts (pure; unit-tested).
fn build_board_export_data(
    board: &BoardRow,
    columns: &[ColumnRow],
    swimlanes: &[SwimlaneRow],
    colors: &[ColorRow],
    users: &[UserRow],
    tasks: Vec<TaskRow>,
) -> BoardExportData {
    BoardExportData {
        board: ExportBoardRef {
            id: board.id.clone(),
            name: board.name.clone(),
        },
        columns: columns
            .iter()
            .map(|c| ExportColumnRef {
                id: c.id.clone(),
                name: c.name.clone(),
                former_names: c.former_names.clone(),
            })
            .collect(),
        swimlanes: swimlanes
            .iter()
            .map(|s| ExportNamedRef {
                id: s.id.clone(),
                name: s.name.clone(),
            })
            .collect(),
        colors: colors
            .iter()
            .map(|c| ExportColorRef {
                id: c.id.clone(),
                value: c.value.clone(),
                label: c.label.clone(),
            })
            .collect(),
        users: users
            .iter()
            .map(|u| ExportUserRef {
                id: u.id.clone(),
                username: u.username.clone(),
            })
            .collect(),
        tasks,
    }
}

/// Returns the board export dataset: every task on the board plus the column/swimlane/color/user
/// lookups, as JSON. The Export report page downloads this and renders the file client-side.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/export-data",
    operation_id = "get_board_export_data",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Board export dataset", body = BoardExportData),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
    )
)]
async fn board_export_data(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<BoardExportData>, AppError> {
    let db = &state.db;
    let board = db
        .get_board(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    let columns = db.list_columns(&id).map_err(AppError::from)?;
    let swimlanes = db.list_swimlanes(&id).map_err(AppError::from)?;
    let colors = db.list_colors(&id).map_err(AppError::from)?;
    let users = db.list_users().map_err(AppError::from)?;
    let tasks = db.board_tasks(&id).map_err(AppError::from)?;
    Ok(Json(build_board_export_data(
        &board, &columns, &swimlanes, &colors, &users, tasks,
    )))
}

// ---- Board calendar iCal feed ----
//
// KanbanFlow parity: the calendar supports subscribing to an external iCal
// feed. Security design (the token is a bearer secret for the board's due
// dates):
// - Token: 32 CSPRNG bytes, base64url (43 chars) — unguessable, generated
//   server-side only.
// - Storage: the board's opaque config bag (`calendar_feed_token`). The bag
//   is never serialized to clients (export-data exposes only board id/name;
//   the board config view only exposes legend_visible/archived/due_time), so
//   the raw token leaves the server only via the authenticated link endpoints below,
//   on explicit user action — never in page source or other API responses.
//   Only POST creates it (a GET never writes).
// - Bearer endpoint `GET /api/calendar-feed/:token` is UNAUTHENTICATED by
//   design (calendar clients can't do cookie auth); an unknown token is a
//   404. `POST .../reset` rotates the token, invalidating the old URL at
//   once.
// - Feed payload is deliberately minimal: task name, due date/time, column
//   name, done state. No comments, history, members, user data, or internal
//   ids beyond the stable per-task UID.
// - Headers: `text/calendar`, `X-Robots-Tag: noindex`, private caching so
//   feed pollers don't hammer the DB.

/// Generates a fresh calendar-feed bearer token.
fn generate_feed_token() -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

/// Repeat cadence recognized for calendar expansion and iCal RRULE. ChipFlow stores `due_repeat` as
/// free text ("every week" from the dialog checkbox) and only these canonical cadences expand.
/// Anything else renders as a single due date, the behavior before repeat expansion.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RepeatCadence {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// Parse free-text `due_repeat` into a [`RepeatCadence`]. Never panics.
fn parse_repeat_cadence(raw: Option<&str>) -> Option<RepeatCadence> {
    let s = raw?.trim().to_lowercase();
    match s.as_str() {
        "every day" | "daily" => Some(RepeatCadence::Daily),
        "every week" | "weekly" => Some(RepeatCadence::Weekly),
        "every month" | "monthly" => Some(RepeatCadence::Monthly),
        "every year" | "yearly" | "annually" => Some(RepeatCadence::Yearly),
        _ => None,
    }
}

impl RepeatCadence {
    fn rrule_freq(self) -> &'static str {
        match self {
            RepeatCadence::Daily => "DAILY",
            RepeatCadence::Weekly => "WEEKLY",
            RepeatCadence::Monthly => "MONTHLY",
            RepeatCadence::Yearly => "YEARLY",
        }
    }
}

/// Escape iCalendar TEXT values (RFC 5545 §3.3.11).
fn ical_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Fold a content line at 75 octets with CRLF + single-space continuation
/// (RFC 5545 §3.1). Folding is byte-based and never splits a UTF-8 char.
fn ical_fold(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() <= 75 {
        return format!("{line}\r\n");
    }
    let mut out = String::new();
    let mut start = 0;
    let mut first = true;
    while start < bytes.len() {
        // RFC 5545 §3.1: no content line may exceed 75 octets excluding the
        // line break. Continuation lines carry a single-space prefix, so
        // their payload budget is 74 (1 + 74 = 75).
        let budget = if first { 75 } else { 74 };
        let mut end = (start + budget).min(bytes.len());
        while end < bytes.len() && !line.is_char_boundary(end) {
            end -= 1;
        }
        if !first {
            out.push_str("\r\n ");
        }
        out.push_str(&line[start..end]);
        start = end;
        first = false;
    }
    out.push_str("\r\n");
    out
}

/// Builds the iCalendar feed for a board. Only tasks with a parseable RFC3339 due date become
/// VEVENTs. Date-only dues (midnight) become all-day events and timed dues become UTC date-times
/// with a 1-hour default duration. Recognized `due_repeat` cadences become RRULEs.
fn build_ical_feed(
    board_name: &str,
    columns: &[ColumnRow],
    tasks: &[TaskRow],
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let col_name = |id: &str| {
        columns
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.as_str())
            .unwrap_or("")
    };
    let mut out = String::new();
    out.push_str(&ical_fold("BEGIN:VCALENDAR"));
    out.push_str(&ical_fold("VERSION:2.0"));
    out.push_str(&ical_fold("PRODID:-//ChipFlow//Board Calendar//EN"));
    out.push_str(&ical_fold(&format!(
        "X-WR-CALNAME:{}",
        ical_escape(board_name)
    )));
    let dtstamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    for task in tasks {
        let due_at = match task.due_at.as_deref() {
            Some(d) => d,
            None => continue,
        };
        let due = match chrono::DateTime::parse_from_rfc3339(due_at) {
            Ok(d) => d.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };

        // Stable UID so feed pollers update rather than duplicate events.
        out.push_str(&ical_fold("BEGIN:VEVENT"));
        out.push_str(&ical_fold(&format!("UID:{}@chipflow", task.id)));
        out.push_str(&ical_fold(&format!("DTSTAMP:{dtstamp}")));
        if due.time() == chrono::NaiveTime::MIN {
            // All-day event (RFC 5545: DTEND is exclusive).
            let start = due.format("%Y%m%d").to_string();
            let end = (due + chrono::Duration::days(1))
                .format("%Y%m%d")
                .to_string();
            out.push_str(&ical_fold(&format!("DTSTART;VALUE=DATE:{start}")));
            out.push_str(&ical_fold(&format!("DTEND;VALUE=DATE:{end}")));
        } else {
            let start = due.format("%Y%m%dT%H%M%SZ").to_string();
            let end = (due + chrono::Duration::hours(1))
                .format("%Y%m%dT%H%M%SZ")
                .to_string();
            out.push_str(&ical_fold(&format!("DTSTART:{start}")));
            out.push_str(&ical_fold(&format!("DTEND:{end}")));
        }
        out.push_str(&ical_fold(&format!("SUMMARY:{}", ical_escape(&task.name))));
        let column = col_name(&task.column_id);
        if !column.is_empty() {
            out.push_str(&ical_fold(&format!(
                "DESCRIPTION:{}",
                ical_escape(&format!("Column: {column}"))
            )));
        }

        // VEVENT statuses are TENTATIVE/CONFIRMED/CANCELLED only (RFC 5545
        // §3.8.1.11); COMPLETED is VTODO-only, so even done tasks are
        // CONFIRMED events.
        out.push_str(&ical_fold("STATUS:CONFIRMED"));
        if let Some(cadence) = parse_repeat_cadence(task.due_repeat.as_deref()) {
            out.push_str(&ical_fold(&format!("RRULE:FREQ={}", cadence.rrule_freq())));
        }
        out.push_str(&ical_fold("END:VEVENT"));
    }
    out.push_str(&ical_fold("END:VCALENDAR"));
    out
}

/// View model for the calendar-feed link endpoints. The client builds absolute `https://` /
/// `webcal://` URLs from `path` with `window.location.origin`, so the server never needs to know
/// its public host.
#[derive(Serialize, ToSchema)]
struct CalendarFeedLink {
    /// Null from GET when the board has no feed token yet.
    path: Option<String>,
}

fn calendar_feed_path(token: &str) -> String {
    format!("/api/calendar-feed/{token}.ics")
}

/// Ensures the board has a calendar-feed token, generating one on first call. Concurrent first
/// calls all get the same token.
fn ensure_feed_token(db: &Db, board_id: &str) -> Result<String, AppError> {
    db.ensure_board_config_string(board_id, "calendar_feed_token", &generate_feed_token())
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))
}

/// Returns the board's iCal feed link, or a null `path` when none has been created. Read-only,
/// since creating the token is a POST. Authenticated, and together with
/// `POST /api/boards/{id}/calendar-feed` the only endpoints that reveal the raw bearer token.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/calendar-feed",
    operation_id = "get_calendar_feed_link",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "iCal feed link (path null when none exists)", body = CalendarFeedLink),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
    ),
)]
async fn get_calendar_feed_link(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<CalendarFeedLink>, AppError> {
    let board = state
        .db
        .get_board(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(Json(CalendarFeedLink {
        path: board.feed_token().as_deref().map(calendar_feed_path),
    }))
}

/// Returns the board's iCal feed link, creating the token on first call. Authenticated.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/calendar-feed",
    operation_id = "create_calendar_feed_link",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "iCal feed link", body = CalendarFeedLink),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
    ),
)]
async fn create_calendar_feed_link(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<CalendarFeedLink>, AppError> {
    let token = ensure_feed_token(&state.db, &id)?;
    Ok(Json(CalendarFeedLink {
        path: Some(calendar_feed_path(&token)),
    }))
}

/// Rotates the board's iCal feed token. The old feed URL stops working immediately. Authenticated.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/calendar-feed/reset",
    operation_id = "reset_calendar_feed_link",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "New iCal feed link", body = CalendarFeedLink),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
    ),
)]
async fn reset_calendar_feed(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<CalendarFeedLink>, AppError> {
    if state.db.get_board(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("board not found"));
    }
    let token = generate_feed_token();
    let mut updates = serde_json::Map::new();
    updates.insert(
        "calendar_feed_token".to_string(),
        serde_json::Value::String(token.clone()),
    );
    state
        .db
        .set_board_config(&id, &updates)
        .map_err(AppError::from)?;
    Ok(Json(CalendarFeedLink {
        path: Some(calendar_feed_path(&token)),
    }))
}

/// Serves the iCal feed. PUBLIC by design: calendar clients cannot do cookie auth, so the
/// unguessable token in the URL is the credential. Unknown tokens are a 404 (no token-validity
/// oracle beyond the URL itself, and the 256-bit space is not enumerable).
#[utoipa::path(
    get,
    path = "/api/calendar-feed/{token}",
    operation_id = "get_calendar_feed",
    tag = "Boards",
    security(()),
    params(("token" = String, Path, description = "Board's calendar-feed bearer token")),
    responses(
        (status = 200, description = "iCalendar feed", content_type = "text/calendar"),
        (status = 404, description = "Unknown feed token")
    ),
)]
async fn serve_calendar_feed(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    // Accept the `.ics` suffix the link endpoint advertises.
    let token = token.strip_suffix(".ics").unwrap_or(&token);
    let board = state
        .db
        .get_board_by_feed_token(token)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("unknown feed"))?;
    let columns = state.db.list_columns(&board.id).map_err(AppError::from)?;
    let tasks = state.db.board_tasks(&board.id).map_err(AppError::from)?;
    let body = build_ical_feed(&board.name, &columns, &tasks, chrono::Utc::now());
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/calendar; charset=utf-8"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    headers.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    Ok((StatusCode::OK, headers, body).into_response())
}

/// Lists a board's columns (id, name, WIP limit, Done flag) for the Move-task dialog's board
/// switcher.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/columns",
    operation_id = "list_board_columns",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Board columns", body = Vec<ColumnListItem>),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
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
    Ok(Json(board_columns(&state.db, &id)?))
}

/// Returns a board's columns in board order as list items.
fn board_columns(db: &Db, board_id: &str) -> Result<Vec<ColumnListItem>, AppError> {
    let columns = db
        .list_columns(board_id)
        .map_err(AppError::from)?
        .into_iter()
        .map(ColumnListItem::from)
        .collect();
    Ok(columns)
}

impl From<ColumnRow> for ColumnListItem {
    fn from(column: ColumnRow) -> Self {
        Self {
            id: column.id,
            name: column.name,
            wip_limit: column.wip_limit,
            is_done: column.is_done,
        }
    }
}

/// One swimlane in `GET /api/boards/{id}`.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct SwimlaneListItem {
    /// Swimlane id.
    id: String,
    /// Swimlane name.
    name: String,
}

/// A board with its ordered columns and swimlanes.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct BoardDetail {
    /// Board id.
    id: String,
    /// Board name.
    name: String,
    /// Columns in board order.
    columns: Vec<ColumnListItem>,
    /// Swimlanes in board order.
    swimlanes: Vec<SwimlaneListItem>,
}

/// Returns a board's id, name, ordered columns and ordered swimlanes, the ids an agent needs to
/// create and move tasks.
#[utoipa::path(
    get,
    path = "/api/boards/{id}",
    operation_id = "get_board",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Board with its columns and swimlanes", body = BoardDetail),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
    ),
)]
async fn get_board_detail(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<BoardDetail>, AppError> {
    let db = &state.db;
    let board = db
        .get_board(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("Board not found"))?;
    let columns = board_columns(db, &id)?;
    let swimlanes = db
        .list_swimlanes(&id)
        .map_err(AppError::from)?
        .into_iter()
        .map(|lane| SwimlaneListItem {
            id: lane.id,
            name: lane.name,
        })
        .collect();
    Ok(Json(BoardDetail {
        id: board.id,
        name: board.name,
        columns,
        swimlanes,
    }))
}

/// One task in a board task list.
#[derive(serde::Serialize, ToSchema)]
struct TaskSummary {
    id: String,
    name: String,
    column_id: String,
    swimlane_id: Option<String>,
    /// Sort order within the task's column and swimlane cell.
    position: f64,
    labels: Vec<String>,
    member_ids: Vec<String>,
    /// RFC3339 time of the last change to the task.
    updated_at: String,
    /// Caller-owned JSON object, or null when unset.
    #[schema(value_type = Option<Object>)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// True when a `blocked_by` target sits in a column that is not Done.
    blocked: bool,
    /// The live claim, or null when unclaimed, released, or expired.
    claim: Option<TaskClaim>,
    /// Custom field values by key.
    #[schema(value_type = Object)]
    fields: serde_json::Map<String, serde_json::Value>,
}

impl TaskSummary {
    fn new(task: TaskRow, blocked: bool) -> Self {
        Self {
            claim: task.live_claim().cloned(),
            updated_at: task.last_updated().to_string(),
            id: task.id,
            name: task.name,
            column_id: task.column_id,
            swimlane_id: task.swimlane_id,
            position: task.position,
            labels: task.labels,
            member_ids: task.member_ids,
            metadata: task.metadata,
            blocked,
            fields: task.fields,
        }
    }
}

/// Filters for `GET /api/boards/{id}/tasks`; every given filter must match.
#[derive(Default, IntoParams)]
#[into_params(parameter_in = Query)]
struct BoardTasksQuery {
    /// Only tasks in this column.
    column_id: Option<String>,
    /// Only tasks in this swimlane.
    swimlane_id: Option<String>,
    /// Only tasks carrying this label. Repeat the parameter to require
    /// several labels.
    #[param(required = false)]
    label: Vec<String>,
    /// RFC3339; only tasks whose `updated_at` is after this instant.
    /// Percent-encode a `+` offset or use `Z`.
    #[param(value_type = Option<String>)]
    since: Option<DateTime<chrono::FixedOffset>>,
    /// true: only tasks that are not blocked; false: only blocked tasks.
    ready: Option<bool>,
    /// true: only tasks with a live claim; false: only unclaimed tasks.
    claimed: Option<bool>,
}

impl BoardTasksQuery {
    /// Parses raw query pairs, keeping every repeated `label`.
    fn from_pairs(pairs: Vec<(String, String)>) -> Result<Self, AppError> {
        let mut query = Self::default();
        for (key, value) in pairs {
            match key.as_str() {
                "column_id" => query.column_id = Some(value),
                "swimlane_id" => query.swimlane_id = Some(value),
                "label" => query.label.push(value),
                "since" => {
                    let since = DateTime::parse_from_rfc3339(value.trim()).map_err(|_| {
                        AppError::bad_request(format!("invalid since \"{value}\": use RFC3339"))
                    })?;
                    query.since = Some(since);
                }
                "ready" => {
                    let ready = value.parse().map_err(|_| {
                        AppError::bad_request(format!(
                            "invalid ready \"{value}\": use true or false"
                        ))
                    })?;
                    query.ready = Some(ready);
                }
                "claimed" => {
                    let claimed = value.parse().map_err(|_| {
                        AppError::bad_request(format!(
                            "invalid claimed \"{value}\": use true or false"
                        ))
                    })?;
                    query.claimed = Some(claimed);
                }
                _ => {}
            }
        }
        Ok(query)
    }

    fn matches(&self, task: &TaskRow, blocked: bool) -> bool {
        let updated_after = |since: &DateTime<chrono::FixedOffset>| {
            DateTime::parse_from_rfc3339(task.last_updated()).is_ok_and(|at| at > *since)
        };
        self.column_id
            .as_ref()
            .is_none_or(|id| *id == task.column_id)
            && self
                .swimlane_id
                .as_ref()
                .is_none_or(|id| task.swimlane_id.as_ref() == Some(id))
            && self.label.iter().all(|label| task.labels.contains(label))
            && self.since.as_ref().is_none_or(updated_after)
            && self.ready.is_none_or(|ready| ready != blocked)
            && self
                .claimed
                .is_none_or(|claimed| claimed == task.live_claim().is_some())
    }
}

/// Lists a board's tasks in board order (column, swimlane, position), optionally filtered.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/tasks",
    operation_id = "list_board_tasks",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id"), BoardTasksQuery),
    responses(
        (status = 200, description = "Matching tasks", body = Vec<TaskSummary>),
        (status = 400, description = "Unparseable since, ready, or claimed"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
    )
)]
async fn list_board_tasks(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<Vec<TaskSummary>>, AppError> {
    let db = &state.db;
    if db.get_board(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("Board not found"));
    }
    let query = BoardTasksQuery::from_pairs(pairs)?;
    let mut tasks = Vec::new();
    for task in db.board_tasks(&id).map_err(AppError::from)? {
        let blocked = db.task_blocked(&task).map_err(AppError::from)?;
        if query.matches(&task, blocked) {
            tasks.push(TaskSummary::new(task, blocked));
        }
    }
    Ok(Json(tasks))
}

/// A custom field defined on a board.
#[derive(Clone, Debug, serde::Serialize, ToSchema)]
struct FieldDef {
    id: String,
    /// Dotted lowercase key, e.g. `agentctl.checks`.
    key: String,
    label: String,
    kind: FieldKind,
    /// Choices for `select` and `multiselect`; empty otherwise.
    options: Vec<String>,
    /// Value new tasks start with, or null.
    default: Option<serde_json::Value>,
    /// A UI hint: the dialog marks the empty input and the card shows a
    /// "fields incomplete" badge. Never enforced by the API.
    required: bool,
    owner: FieldOwner,
    position: i64,
    /// RFC3339.
    created_at: String,
}

impl From<FieldDefRow> for FieldDef {
    fn from(row: FieldDefRow) -> Self {
        Self {
            id: row.id,
            key: row.key,
            label: row.label,
            kind: row.kind,
            options: row.options,
            default: row.default,
            required: row.required,
            owner: row.owner,
            position: row.position,
            created_at: row.created_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateFieldInput {
    /// Required; must match `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`, at
    /// most 64 characters.
    #[schema(value_type = String, required = true)]
    key: Option<String>,
    #[schema(value_type = String, required = true)]
    label: Option<String>,
    #[schema(value_type = FieldKind, required = true)]
    kind: Option<String>,
    /// Required and non-empty for `select` and `multiselect`; not allowed
    /// for other kinds.
    options: Option<Vec<String>>,
    default: Option<serde_json::Value>,
    required: Option<bool>,
    /// "user" (default) or "system".
    #[schema(value_type = Option<FieldOwner>)]
    owner: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateFieldInput {
    label: Option<String>,
    options: Option<Vec<String>>,
    /// Null clears the default; absent leaves it unchanged.
    #[serde(default, deserialize_with = "de_opt_opt")]
    default: Option<Option<serde_json::Value>>,
    required: Option<bool>,
    position: Option<i64>,
    /// Read only to answer 400. The key, kind and owner are fixed.
    #[schema(ignore)]
    #[serde(default, deserialize_with = "de_opt_opt")]
    key: Option<Option<serde_json::Value>>,
    #[schema(ignore)]
    #[serde(default, deserialize_with = "de_opt_opt")]
    kind: Option<Option<serde_json::Value>>,
    #[schema(ignore)]
    #[serde(default, deserialize_with = "de_opt_opt")]
    owner: Option<Option<serde_json::Value>>,
}

/// Checks a `fields` object against a board's definitions and returns the
/// values to merge (nulls kept, meaning "clear"). Every 400 names the key.
fn check_field_values(
    defs: &[FieldDefRow],
    patch: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, AppError> {
    let serde_json::Value::Object(patch) = patch else {
        return Err(AppError::bad_request("fields must be a JSON object"));
    };
    let mut values = serde_json::Map::new();
    for (key, value) in patch {
        let def = defs
            .iter()
            .find(|def| def.key == *key)
            .ok_or_else(|| AppError::bad_request(format!("unknown field \"{key}\"")))?;
        let value = if value.is_null() {
            serde_json::Value::Null
        } else {
            def.check_value(value)
                .map_err(|why| AppError::bad_request(format!("field \"{key}\": {why}")))?
        };
        values.insert(key.clone(), value);
    }
    Ok(values)
}

/// Normalizes `options` for a field kind: required, trimmed, non-empty and
/// unique for select kinds; absent or empty for the rest.
fn check_field_options(
    key: &str,
    kind: FieldKind,
    options: Option<Vec<String>>,
) -> Result<Vec<String>, AppError> {
    let options: Vec<String> = options
        .unwrap_or_default()
        .iter()
        .map(|option| option.trim().to_string())
        .collect();
    if !kind.has_options() {
        return match options.is_empty() {
            true => Ok(options),
            false => Err(AppError::bad_request(
                "options apply only to select and multiselect fields",
            )),
        };
    }
    let unique = options
        .iter()
        .enumerate()
        .all(|(i, option)| !option.is_empty() && !options[..i].contains(option));
    if options.is_empty() || !unique {
        return Err(AppError::bad_request(
            "select fields need options that are non-empty and unique",
        ));
    }

    // The settings page edits options one per line.
    if options.iter().any(|option| has_line_break(option)) {
        return Err(AppError::bad_request(format!(
            "field \"{key}\": options must be single lines"
        )));
    }
    Ok(options)
}

/// Checks a field's default against its own kind and options.
fn check_field_default(def: &FieldDefRow) -> Result<Option<serde_json::Value>, AppError> {
    match &def.default {
        None => Ok(None),
        Some(default) => def
            .check_value(default)
            .map(Some)
            .map_err(|why| AppError::bad_request(format!("field \"{}\": default: {why}", def.key))),
    }
}

/// Defines a custom field on a board.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/fields",
    operation_id = "create_board_field",
    tag = "Fields",
    params(("id" = String, Path, description = "Board id")),
    request_body = CreateFieldInput,
    responses(
        (status = 200, description = "The new field", body = FieldDef),
        (status = 400, description = "invalid field key, missing label or kind, bad options or default"),
        (status = 404, description = "Board not found"),
        (status = 409, description = "`field_key_exists`: field key exists"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn create_board_field(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<FieldDef>, AppError> {
    let input: CreateFieldInput = parse_body(&headers, body).await?;
    let db = &state.db;
    if !db.board_exists(&board_id).map_err(AppError::from)? {
        return Err(AppError::not_found("Board not found"));
    }
    let key = input
        .key
        .filter(|key| valid_field_key(key))
        .ok_or_else(|| AppError::bad_request("invalid field key"))?;
    let label = input
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .ok_or_else(|| AppError::bad_request("label is required"))?
        .to_string();
    let kind = input
        .kind
        .as_deref()
        .ok_or_else(|| AppError::bad_request("kind is required"))?;
    let kind = FieldKind::parse(kind).ok_or_else(|| AppError::bad_request("unknown field kind"))?;
    let owner = match input.owner.as_deref() {
        None | Some("user") => FieldOwner::User,
        Some("system") => FieldOwner::System,
        Some(_) => return Err(AppError::bad_request("unknown field owner")),
    };
    let options = check_field_options(&key, kind, input.options)?;
    let mut def = FieldDefRow {
        id: String::new(),
        board_id,
        key,
        label,
        kind,
        options,
        default: input.default.filter(|value| !value.is_null()),
        required: input.required.unwrap_or(false),
        owner,
        position: 0,
        created_at: String::new(),
    };
    def.default = check_field_default(&def)?;
    match db.create_field(def).map_err(AppError::from)? {
        FieldCreateOutcome::Created(def) => Ok(Json(FieldDef::from(def))),
        FieldCreateOutcome::KeyExists => {
            Err(AppError::conflict("field key exists").with_code(ErrorCode::FieldKeyExists))
        }
    }
}

/// Lists a board's custom fields in position order.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/fields",
    operation_id = "list_board_fields",
    tag = "Fields",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "The board's fields", body = Vec<FieldDef>),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn list_board_fields(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<Json<Vec<FieldDef>>, AppError> {
    let db = &state.db;
    if !db.board_exists(&board_id).map_err(AppError::from)? {
        return Err(AppError::not_found("Board not found"));
    }
    let fields = db
        .list_fields(&board_id)
        .map_err(AppError::from)?
        .into_iter()
        .map(FieldDef::from)
        .collect();
    Ok(Json(fields))
}

/// Changes a field's label, options, default, required hint, or position. The key, kind, and owner
/// are fixed.
#[utoipa::path(
    patch,
    path = "/api/fields/{id}",
    operation_id = "update_field",
    tag = "Fields",
    params(("id" = String, Path, description = "Field id")),
    request_body = UpdateFieldInput,
    responses(
        (status = 200, description = "The updated field", body = FieldDef),
        (status = 400, description = "Empty label, bad options, a default that no longer fits, or key, kind or owner sent"),
        (status = 404, description = "field not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn update_board_field(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<FieldDef>, AppError> {
    let input: UpdateFieldInput = parse_body(&headers, body).await?;
    for (name, sent) in [
        ("key", &input.key),
        ("kind", &input.kind),
        ("owner", &input.owner),
    ] {
        if sent.is_some() {
            return Err(AppError::bad_request(format!("{name} cannot be changed")));
        }
    }
    let label = match input.label.as_deref().map(str::trim) {
        Some("") => return Err(AppError::bad_request("label is required")),
        label => label,
    };

    // The checks that depend on the stored definition (its kind, and the
    // default against the options) run inside the write transaction.
    let updated = state
        .db
        .apply_field_change(&id, |def| {
            let check = || -> Result<FieldDefRow, AppError> {
                let mut next = def.clone();
                if let Some(label) = label {
                    next.label = label.to_string();
                }
                if let Some(options) = input.options.clone() {
                    next.options = check_field_options(&next.key, next.kind, Some(options))?;
                }
                if let Some(default) = input.default.clone() {
                    next.default = default.filter(|value| !value.is_null());
                }
                next.default = check_field_default(&next)?;
                if let Some(required) = input.required {
                    next.required = required;
                }
                if let Some(position) = input.position {
                    next.position = position;
                }
                Ok(next)
            };
            match check() {
                Ok(next) => {
                    *def = next.clone();
                    (Ok(next), true)
                }
                Err(error) => (Err(error), false),
            }
        })
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("field not found"))??;
    Ok(Json(FieldDef::from(updated)))
}

/// Deletes a field and its value on every task of the board.
#[utoipa::path(
    delete,
    path = "/api/fields/{id}",
    operation_id = "delete_field",
    tag = "Fields",
    params(("id" = String, Path, description = "Field id")),
    responses(
        (status = 200, description = "Field and its values deleted"),
        (status = 404, description = "field not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn delete_board_field(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    if state.db.delete_field(&id).map_err(AppError::from)? {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::not_found("field not found"))
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateBoardInput {
    name: String,
    /// Optional template id. Without one the board starts blank: the
    /// standard color palette plus a single "To-do" column.
    template_id: Option<String>,
}

/// Creates a board, optionally from a template, and returns the board id. A blank board gets the
/// standard color palette and one "To-do" column.
#[utoipa::path(
    post,
    path = "/api/boards",
    operation_id = "create_board",
    tag = "Boards",
    request_body = CreateBoardInput,
    responses(
        (status = 200, description = "Created board id", body = IdResult),
        (status = 400, description = "Invalid input: name blank or over 100 characters"),
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

    // The same 1-100 character rule as renaming.
    let name = Db::validate_board_name(&input.name)
        .map_err(AppError::bad_request)?
        .to_string();

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

            // A board with zero swimlanes leaves the column "+" add-task buttons silently dead (the
            // form clones into the first swimlane row's cell).
            db.ensure_default_swimlane(&id).map_err(AppError::from)?;
            id
        }
    };
    Ok(Json(IdResult { id }))
}

/// Deletes a board and everything in it. Refuses when it is the last board, or the last unarchived
/// one while archived boards remain.
#[utoipa::path(
    delete,
    path = "/api/boards/{id}",
    operation_id = "delete_board",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "Board deleted"),
        (status = 400, description = "Cannot delete the last board, or the last unarchived one"),
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
        } else if msg == crate::db::LAST_BOARD || msg == crate::db::LAST_ACTIVE_BOARD {
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
    /// Why the server refuses to delete this board, if it does.
    delete_refusal: Option<&'static str>,
    /// Global top bar data.
    boards: Vec<BoardListItem>,
    username: String,
    username_initial: String,
}

#[derive(Deserialize, ToSchema)]
struct BoardConfigInput {
    /// The Menu's Color legend toggle. When present, it is merged into the board's opaque config
    /// bag.
    legend_visible: Option<bool>,
    /// The "Due date time" a date-only due date gets, as 24-hour "HH:MM". When present, it is
    /// merged into the config bag.
    due_time: Option<String>,
    /// Archive/unarchive the board. When present, it is merged into the board's opaque config bag.
    archived: Option<bool>,
    /// "Position of new tasks": true puts new tasks at the top of their column, false (the default)
    /// at the bottom.
    new_tasks_at_top: Option<bool>,
}

#[derive(Serialize, ToSchema)]
struct BoardConfigView {
    /// Whether the color legend bar is shown on this board.
    legend_visible: bool,
    /// Whether the board is archived.
    archived: bool,
    /// Whether new tasks go to the top of their column.
    new_tasks_at_top: bool,
    /// The board's "Due date time" as 24-hour "HH:MM".
    due_time: String,
}

/// Merges per-board UI settings into the board's opaque config bag (the Color legend Menu toggle
/// persists `legend_visible` here, per board).
#[utoipa::path(
    put,
    path = "/api/boards/{id}/config",
    operation_id = "update_board_config",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    request_body = BoardConfigInput,
    responses(
        (status = 200, description = "Current board config", body = BoardConfigView),
        (status = 400, description = "Invalid due_time, or archiving the last unarchived board"),
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
    if let Some(at_top) = input.new_tasks_at_top {
        updates.insert(
            "new_tasks_at_top".to_string(),
            serde_json::Value::Bool(at_top),
        );
    }
    if let Some(raw) = input.due_time.as_deref() {
        let time = parse_due_time(raw.trim()).ok_or_else(|| {
            AppError::bad_request(format!("invalid due_time \"{raw}\": use 24-hour HH:MM"))
        })?;
        updates.insert(
            "due_time".to_string(),
            serde_json::Value::String(time.format("%H:%M").to_string()),
        );
    }
    db.set_board_config(&id, &updates).map_err(|e| {
        if e.to_string().contains("last active board") {
            AppError::bad_request(e.to_string())
        } else {
            AppError::from(e)
        }
    })?;
    let board = db
        .get_board(&id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    Ok(Json(BoardConfigView {
        legend_visible: board.config_bool("legend_visible"),
        archived: board.config_bool("archived"),
        new_tasks_at_top: board.config_bool("new_tasks_at_top"),
        due_time: board.due_time().format("%H:%M").to_string(),
    }))
}

/// Red "Delete board" confirmation page (KanbanFlow parity: Settings →
/// Delete board → red confirmation page → final "cannot be undone" dialog).
async fn board_delete_page(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
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
    let boards = state.db.list_boards().map_err(AppError::from)?;
    let delete_refusal = crate::db::board_delete_refusal(&boards, &board.id);
    Ok(BoardDeleteTemplate {
        board_id: board.id,
        board_name: board.name,
        task_count,
        delete_refusal,
        boards: dropdown_boards(&state.db)?,
        username_initial: user_initials(&user.username),
        username: user.username,
    })
}

/// "New board" page: existing boards plus the template picker.
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

/// Lists archived boards with unarchive actions.
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

/// Captures a board as a reusable template: its colors (all, with their config), columns,
/// swimlanes, and tasks. Returns the template id.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/save-as-template",
    operation_id = "save_board_as_template",
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

/// Lists all board templates: built-ins first, then by name.
#[utoipa::path(
    get,
    path = "/api/templates",
    operation_id = "list_templates",
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

/// Deletes a template. Built-in templates are protected (400).
#[utoipa::path(
    delete,
    path = "/api/templates/{id}",
    operation_id = "delete_template",
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

// ---- Board settings shell ----

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
    copy_boards: Vec<BoardListItem>,
    /// Global top bar data.
    boards: Vec<BoardListItem>,
    username: String,
    username_initial: String,
    columns: Vec<SettingsColumnView>,
    swimlanes: Vec<SettingsLaneView>,
    default_color_label: String,
    done_column_name: String,
    /// Whether new tasks go to the top of their column.
    new_tasks_at_top: bool,
    board_position: i64,
    /// True right after the General tab's rename form saved (?saved=1).
    saved: bool,
    /// Why the General tab's rename was rejected.
    error: Option<String>,
    /// The board's custom fields, for the Custom fields tab.
    fields: Vec<FieldDef>,
    /// The board's "Due date time" as "HH:MM" and as "5:00 PM".
    due_time: String,
    due_time_label: String,
}

#[derive(Clone, Debug)]
struct SettingsColumnView {
    name: String,
    wip_limit: Option<i64>,
    is_done: bool,
    collapsed: bool,
}

#[derive(Clone, Debug)]
struct SettingsLaneView {
    name: String,
}

fn board_settings_context(
    db: &Db,
    username: &str,
    board_id: &str,
    tab: &str,
    saved: bool,
) -> Result<BoardSettingsTemplate, AppError> {
    let board = db
        .get_board(board_id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("board not found"))?;
    let colors: Vec<ColorView> = {
        // KanbanFlow lists the Disabled colors section alphabetically by color name, and enabled
        // colors keep palette order.
        let mut enabled: Vec<ColorView> = Vec::new();
        let mut disabled: Vec<ColorView> = Vec::new();
        for c in db
            .list_colors(board_id)
            .map_err(AppError::from)?
            .iter()
            .map(ColorView::from)
        {
            if c.enabled {
                enabled.push(c);
            } else {
                disabled.push(c);
            }
        }
        disabled.sort_by(|a, b| {
            a.display_label()
                .to_lowercase()
                .cmp(&b.display_label().to_lowercase())
        });
        enabled.into_iter().chain(disabled).collect()
    };
    let copy_boards: Vec<BoardListItem> = db
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
    let new_tasks_at_top = board.config_bool("new_tasks_at_top");
    let due_time = board.due_time();
    Ok(BoardSettingsTemplate {
        due_time: due_time.format("%H:%M").to_string(),
        due_time_label: due_time.format("%-I:%M %p").to_string(),
        board_id: board.id,
        board_name: board.name,
        tab: tab.to_string(),
        colors,
        copy_boards,
        boards: dropdown_boards(db)?,
        username: username.to_string(),
        username_initial: user_initials(username),
        columns,
        swimlanes,
        default_color_label,
        done_column_name,
        new_tasks_at_top,
        board_position: board.position,
        saved,
        error: None,
        fields: db
            .list_fields(board_id)
            .map_err(AppError::from)?
            .into_iter()
            .map(FieldDef::from)
            .collect(),
    })
}

macro_rules! board_settings_tab {
    ($name:ident, $tab:literal) => {
        async fn $name(
            State(state): State<AppState>,
            Extension(user): Extension<AuthUser>,
            Path(board_id): Path<String>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Result<BoardSettingsTemplate, AppError> {
            board_settings_context(
                &state.db,
                &user.username,
                &board_id,
                $tab,
                query.contains_key("saved"),
            )
        }
    };
}

board_settings_tab!(board_settings_page, "general");
board_settings_tab!(board_settings_layout_page, "layout");
board_settings_tab!(board_settings_task_page, "task");
board_settings_tab!(board_settings_advanced_page, "advanced");
board_settings_tab!(board_settings_fields_page, "fields");
board_settings_tab!(board_settings_api_page, "api");
board_settings_tab!(board_settings_email_page, "email");

#[derive(Deserialize)]
struct BoardSettingsForm {
    name: String,
}

/// Rename the board from the Board Settings → General tab.
async fn board_settings_submit(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(board_id): Path<String>,
    Form(form): Form<BoardSettingsForm>,
) -> Result<Response, AppError> {
    if !state.db.board_exists(&board_id).map_err(AppError::from)? {
        return Err(AppError::not_found("board not found"));
    }

    // A rejected name re-renders the General tab with the reason.
    if let Err(message) = Db::validate_board_name(&form.name) {
        let mut page =
            board_settings_context(&state.db, &user.username, &board_id, "general", false)?;
        page.error = Some(message.to_string());
        return Ok((StatusCode::BAD_REQUEST, page).into_response());
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
async fn board_colors_page(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Path(board_id): Path<String>,
) -> Result<BoardSettingsTemplate, AppError> {
    board_settings_context(&state.db, &user.username, &board_id, "colors", false)
}

/// Names the board whose palette is copied.
#[derive(Debug, Deserialize, ToSchema)]
struct CopyColorsInput {
    /// Board to copy the palette from.
    source_board_id: String,
}

#[derive(Debug, serde::Serialize, ToSchema)]
struct CountResult {
    count: usize,
}

/// Copies another board's whole color palette onto this board.
///
/// Every color on the target board is replaced, and tasks keep their color by standard value.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/colors/copy-from",
    operation_id = "copy_board_colors",
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

/// Lists a board's color palette, ordered by sort_order. Read-only, and boards that predate colors
/// get the standard palette when the server starts.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/colors",
    operation_id = "list_board_colors",
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

/// Adds a color slot for a standard value. Errors with 400 when the value is unknown or the board
/// already has a slot for it.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/colors",
    operation_id = "create_board_color",
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
    if let Some(label) = input.label.as_deref()
        && !label.trim().is_empty()
    {
        Db::validate_color_label(label).map_err(AppError::bad_request)?;
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

/// Renames, describes or enables a color, changes the default, or reorders it.
#[utoipa::path(
    patch,
    path = "/api/boards/{id}/colors/{color_id}",
    operation_id = "update_board_color",
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

/// Deletes a color slot. Refuses with 400 when it's the board default or tasks still use it.
#[utoipa::path(
    delete,
    path = "/api/boards/{id}/colors/{color_id}",
    operation_id = "delete_board_color",
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
    /// The server's base URL, for the agent skill install line.
    base_url: String,
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
    headers: HeaderMap,
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
            scopes: t.scope.as_str().to_string(),
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
        base_url: base_url(&state, &headers),
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

/// Serves the settings as JSON for the timer JavaScript.
#[utoipa::path(
    get,
    path = "/api/settings",
    operation_id = "get_settings",
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

/// Serves the settings as JSON for the timer JavaScript. The body matches `GET /api/settings`, and
/// this stays its own spec entry.
#[utoipa::path(
    get,
    path = "/api/timer/settings",
    operation_id = "get_timer_settings",
    tag = "Timer",
    responses(
        (status = 200, description = "App settings (same as GET /api/settings)", body = Settings),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
async fn api_timer_settings(
    state: State<AppState>,
    user: Extension<AuthUser>,
) -> Result<Json<Settings>, AppError> {
    api_settings(state, user).await
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
    /// Board ids pinned in the Boards sidebar Favorites. Replaces the whole list, and unknown board
    /// ids are dropped.
    favorite_boards: Option<Vec<String>>,
}

/// Updates the app settings. Only the fields present change.
#[utoipa::path(
    put,
    path = "/api/settings",
    operation_id = "update_settings",
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
            scopes: vec![record.scope.as_str().to_string()],
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
    /// Days until expiry; null, absent, or <= 0 means no expiry.
    expires_in_days: Option<i64>,
}

/// Checks a submitted scope list and reduces it to the one highest scope, since each scope includes
/// the ones below it.
fn submitted_scope(scopes: &[String]) -> Result<AuthScope, AppError> {
    AuthScope::highest(scopes).ok_or_else(|| {
        AppError::bad_request(
            "scopes must be a non-empty subset of [\"read\", \"work\", \"admin\"]",
        )
    })
}

/// A token must never be able to mint new tokens: creation, listing, and
/// revocation require the browser session cookie.
#[derive(Deserialize, ToSchema)]
struct DeviceAuthInput {
    /// Shown to the person approving, and the issued token's name; 1-80
    /// characters. Required.
    #[schema(value_type = String, required = true)]
    client_name: Option<String>,
    /// Non-empty subset of ["read", "work", "admin"]; the highest one is requested. Required.
    #[schema(value_type = Vec<String>, required = true)]
    scopes: Option<Vec<String>>,
}

#[derive(serde::Serialize, ToSchema)]
struct DeviceAuthResponse {
    /// Secret for polling `/api/v1/auth/device/token`; usable once.
    device_code: String,
    /// Code the person types at `verification_url`, e.g. "BCDF-GHJK".
    user_code: String,
    verification_url: String,
    interval_seconds: i64,
    expires_in_seconds: i64,
}

#[derive(Deserialize, ToSchema)]
struct DeviceTokenInput {
    #[schema(value_type = String, required = true)]
    device_code: Option<String>,
}

#[derive(serde::Serialize, ToSchema)]
struct DeviceTokenResponse {
    /// The raw "cf_..." token, returned once.
    token: String,
    name: String,
    scopes: Vec<String>,
}

/// Starts device pairing (RFC 8628). No auth: the device shows `user_code` and the person approves
/// it at `verification_url` while logged in.
#[utoipa::path(
    post,
    path = "/api/v1/auth/device",
    operation_id = "start_device_auth",
    tag = "Tokens",
    request_body = DeviceAuthInput,
    security(()),
    responses(
        (status = 200, description = "Codes for the device", body = DeviceAuthResponse),
        (status = 400, description = "Bad client_name or scopes"),
        (status = 429, description = "`rate_limited`: too many pending device requests, or more than 10 starts a minute from one client IP"),
    ),
)]
async fn start_device_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<DeviceAuthResponse>, AppError> {
    let input: DeviceAuthInput = parse_body(&headers, body).await?;
    let client_name = input
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 80)
        .ok_or_else(|| AppError::bad_request("client_name must be 1-80 characters"))?
        .to_string();
    let scope = submitted_scope(&input.scopes.unwrap_or_default())?;
    let now = chrono::Utc::now().timestamp();
    let device_code = auth::generate_device_code();
    let mut row = DeviceAuthRow {
        device_code_hash: auth::hash_device_code(&device_code),
        user_code: auth::generate_user_code(),
        client_name,
        scope,
        status: DeviceAuthStatus::Pending,
        created_at: now,
        expires_at: now + Db::DEVICE_CODE_TTL_SECONDS,
        last_poll_at: None,
        token_expires_at: None,
    };

    // A user-code collision among live requests is astronomically rare;
    // a few fresh draws settle it.
    for _ in 0..3 {
        match state
            .db
            .create_device_auth(&row, now)
            .map_err(AppError::from)?
        {
            DeviceAuthCreateOutcome::Created => {
                let base = base_url(&state, &headers);
                return Ok(Json(DeviceAuthResponse {
                    device_code,
                    user_code: auth::display_user_code(&row.user_code),
                    verification_url: format!("{base}/device"),
                    interval_seconds: Db::DEVICE_POLL_INTERVAL_SECONDS,
                    expires_in_seconds: Db::DEVICE_CODE_TTL_SECONDS,
                }));
            }
            DeviceAuthCreateOutcome::TooManyPending => {
                return Err(AppError::with_status(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many pending device requests",
                ));
            }
            DeviceAuthCreateOutcome::UserCodeTaken => row.user_code = auth::generate_user_code(),
        }
    }
    Err(AppError::internal("could not draw a free user code"))
}

/// The server's externally visible base URL: `--public-url` when set, else built from the
/// request's scheme and `Host` header.
fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = &state.public_url {
        return url.clone();
    }
    let scheme = if auth::request_is_https(headers) {
        "https"
    } else {
        "http"
    };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

/// Polls for the token of a device-pairing request.
#[utoipa::path(
    post,
    path = "/api/v1/auth/device/token",
    operation_id = "poll_device_token",
    tag = "Tokens",
    request_body = DeviceTokenInput,
    security(()),
    responses(
        (status = 200, description = "Approved: the new API token, returned once", body = DeviceTokenResponse),
        (status = 400, description = "`invalid_request`: device_code is required, or `invalid_grant`: unknown or already used"),
        (status = 403, description = "`access_denied`: the person denied the request"),
        (status = 410, description = "`device_code_expired`: not approved within expires_in_seconds"),
        (status = 428, description = "`authorization_pending`: not approved yet, poll again later"),
        (status = 429, description = "`slow_down`: polled sooner than interval_seconds after the previous poll, so wait longer"),
    ),
)]
async fn poll_device_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<DeviceTokenResponse>, AppError> {
    let input: DeviceTokenInput = parse_body(&headers, body).await?;
    let device_code = input
        .device_code
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| AppError::bad_request("device_code is required"))?;
    let now = chrono::Utc::now().timestamp();
    let mut raw_token = String::new();
    let poll = state
        .db
        .poll_device_auth(&auth::hash_device_code(device_code.trim()), now, |row| {
            let (record, raw) =
                auth::new_api_token_record(&row.client_name, row.scope, row.token_expires_at);
            raw_token = raw;
            record
        })
        .map_err(AppError::from)?;
    match poll {
        DevicePoll::Issued(record) => Ok(Json(DeviceTokenResponse {
            token: raw_token,
            name: record.name,
            scopes: vec![record.scope.as_str().to_string()],
        })),
        DevicePoll::Pending => Err(AppError::with_status(
            StatusCode::PRECONDITION_REQUIRED,
            "authorization_pending",
        )
        .with_code(ErrorCode::AuthorizationPending)),
        DevicePoll::Denied => Err(AppError::forbidden("denied").with_code(ErrorCode::AccessDenied)),
        DevicePoll::Expired => Err(AppError::with_status(StatusCode::GONE, "expired")
            .with_code(ErrorCode::DeviceCodeExpired)),
        DevicePoll::SlowDown => Err(AppError::with_status(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
        )
        .with_code(ErrorCode::SlowDown)),
        DevicePoll::Invalid => {
            Err(AppError::bad_request("invalid_grant").with_code(ErrorCode::InvalidGrant))
        }
    }
}

/// The device-pairing page in one of its steps: "enter", "confirm",
/// "approved", or "denied".
#[derive(Template)]
#[template(path = "device.html")]
struct DeviceTemplate {
    step: &'static str,
    /// Display form, e.g. "BCDF-GHJK"; what the person typed on errors.
    user_code: String,
    client_name: String,
    /// The scope the client requested, as a list of one for the template.
    scopes: Vec<String>,
    /// Scopes the owner may approve, lowest first, ending with the requested one.
    choices: Vec<&'static str>,
    /// Whether the request is for `admin`, which the page warns about.
    requested_admin: bool,
    /// Expiries the owner can pick from.
    expiries: &'static [ExpiryChoice],
    /// Form value of the expiry selected first.
    default_expiry: &'static str,
    error: Option<String>,
}

/// One expiry the owner can give a device token.
struct ExpiryChoice {
    /// Form value.
    value: &'static str,
    /// Text shown in the expiry list.
    label: &'static str,
    /// Days until expiry, or [`None`] for a token that never expires.
    days: Option<i64>,
}

/// Expiry choices on the approval page, in display order.
const DEVICE_TOKEN_EXPIRIES: [ExpiryChoice; 4] = [
    ExpiryChoice {
        value: "30",
        label: "30 days",
        days: Some(30),
    },
    ExpiryChoice {
        value: "90",
        label: "90 days",
        days: Some(90),
    },
    ExpiryChoice {
        value: "365",
        label: "365 days",
        days: Some(365),
    },
    ExpiryChoice {
        value: "never",
        label: "Never",
        days: None,
    },
];

/// Form value of the expiry chosen when the owner picks none.
const DEVICE_TOKEN_DEFAULT_EXPIRY: &str = "90";

impl DeviceTemplate {
    fn enter(user_code: &str, error: Option<&str>) -> Self {
        Self {
            step: "enter",
            user_code: user_code.to_string(),
            client_name: String::new(),
            scopes: Vec::new(),
            choices: Vec::new(),
            requested_admin: false,
            expiries: &DEVICE_TOKEN_EXPIRIES,
            default_expiry: DEVICE_TOKEN_DEFAULT_EXPIRY,
            error: error.map(str::to_string),
        }
    }

    fn with(step: &'static str, row: &DeviceAuthRow) -> Self {
        let requested = row.scope;
        Self {
            step,
            user_code: auth::display_user_code(&row.user_code),
            client_name: row.client_name.clone(),
            scopes: vec![requested.as_str().to_string()],
            choices: AuthScope::ALL
                .into_iter()
                .filter(|scope| *scope <= requested)
                .map(AuthScope::as_str)
                .collect(),
            requested_admin: requested == AuthScope::Admin,
            expiries: &DEVICE_TOKEN_EXPIRIES,
            default_expiry: DEVICE_TOKEN_DEFAULT_EXPIRY,
            error: None,
        }
    }

    /// Returns the page with `error` shown above the form.
    fn with_error(mut self, error: &str) -> Self {
        self.error = Some(error.to_string());
        self
    }
}

/// Builds what the owner grants from the approval form, refusing a scope above the requested one
/// and an expiry outside the offered set. A missing field means the requested scope and 90 days.
fn device_grant(
    row: &DeviceAuthRow,
    form: &DeviceApproveForm,
    now: i64,
) -> Result<DeviceGrant, &'static str> {
    let requested = row.scope;
    let scope = match form.scope.as_deref() {
        None => requested,
        Some(text) => AuthScope::parse(text).ok_or("Choose read, work or admin as the scope.")?,
    };
    if scope > requested {
        return Err("The scope can be lowered but never raised above the request.");
    }
    let wanted = form
        .expires_in_days
        .as_deref()
        .unwrap_or(DEVICE_TOKEN_DEFAULT_EXPIRY);
    let expiry = DEVICE_TOKEN_EXPIRIES
        .iter()
        .find(|choice| choice.value == wanted)
        .ok_or("Choose 30, 90 or 365 days, or never.")?;
    Ok(DeviceGrant {
        scope,
        expires_at: expiry.days.map(|days| now + days * SECONDS_PER_DAY),
    })
}

/// The device page while too many wrong codes were typed recently.
fn device_locked_page() -> Response {
    let page = DeviceTemplate::enter("", Some("Too many wrong codes. Try again in 10 minutes."));
    (StatusCode::TOO_MANY_REQUESTS, page).into_response()
}

#[derive(Deserialize)]
struct DeviceCodeForm {
    user_code: String,
}

/// The fields of the device approval form.
#[derive(Deserialize)]
struct DeviceApproveForm {
    /// The code the person typed, in any case and with or without the dash.
    user_code: String,
    /// Scope to grant, `read`, `work` or `admin`; absent means the requested one.
    scope: Option<String>,
    /// `30`, `90`, `365` or `never`; absent means 90 days.
    expires_in_days: Option<String>,
}

async fn device_page(Extension(user): Extension<AuthUser>) -> Result<Response, AppError> {
    require_cookie_auth(&user)?;
    Ok(DeviceTemplate::enter("", None).into_response())
}

/// Look up a typed user code and ask the person to approve or deny it.
/// Wrong codes are counted; past the cap the page refuses with 429.
async fn device_lookup(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Form(form): Form<DeviceCodeForm>,
) -> Result<Response, AppError> {
    require_cookie_auth(&user)?;
    let now = chrono::Utc::now().timestamp();
    if state.device_lockout.locked(now) {
        return Ok(device_locked_page());
    }
    let code = auth::normalize_user_code(&form.user_code);
    match state
        .db
        .pending_device_auth(&code, now)
        .map_err(AppError::from)?
    {
        Some(row) => Ok(DeviceTemplate::with("confirm", &row).into_response()),
        None => {
            // An already-decided code is not a guess; only codes that
            // never existed, or expired, count toward the lockout.
            if !state
                .db
                .device_code_known(&code, now)
                .map_err(AppError::from)?
            {
                state.device_lockout.record_failure(now);
            }
            let page = DeviceTemplate::enter(
                &form.user_code,
                Some("That code is unknown or has expired."),
            );
            Ok((StatusCode::BAD_REQUEST, page).into_response())
        }
    }
}

async fn device_approve(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Form(form): Form<DeviceApproveForm>,
) -> Result<Response, AppError> {
    device_decide(&state, &user, &form.user_code, Some(&form))
}

async fn device_deny(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Form(form): Form<DeviceCodeForm>,
) -> Result<Response, AppError> {
    device_decide(&state, &user, &form.user_code, None)
}

/// Approve a pending request on the terms of `approval`, or deny it when that is None. Needs the
/// browser session: an API token can never approve a device, since that would mint a token.
fn device_decide(
    state: &AppState,
    user: &AuthUser,
    typed: &str,
    approval: Option<&DeviceApproveForm>,
) -> Result<Response, AppError> {
    require_cookie_auth(user)?;
    let db = &state.db;
    let now = chrono::Utc::now().timestamp();

    // The lockout covers approve and deny too, so codes cannot be guessed
    // here instead of on the code form.
    if state.device_lockout.locked(now) {
        return Ok(device_locked_page());
    }
    let code = auth::normalize_user_code(typed);
    let pending = db.pending_device_auth(&code, now).map_err(AppError::from)?;
    let grant = match (&pending, approval) {
        (Some(row), Some(form)) => match device_grant(row, form, now) {
            Ok(grant) => Some(grant),
            Err(message) => {
                let page = DeviceTemplate::with("confirm", row).with_error(message);
                return Ok((StatusCode::BAD_REQUEST, page).into_response());
            }
        },
        _ => None,
    };
    match pending {
        Some(row)
            if db
                .decide_device_auth(&code, grant.as_ref(), now)
                .map_err(AppError::from)? =>
        {
            let step = if grant.is_some() {
                "approved"
            } else {
                "denied"
            };
            Ok(DeviceTemplate::with(step, &row).into_response())
        }
        _ => {
            // A racer that lost to another decision, or a double submit,
            // names a real code and is not counted.
            if !db.device_code_known(&code, now).map_err(AppError::from)? {
                state.device_lockout.record_failure(now);
            }
            let page =
                DeviceTemplate::enter(typed, Some("That code is no longer waiting for approval."));
            Ok((StatusCode::BAD_REQUEST, page).into_response())
        }
    }
}

/// The credential behind the current request.
#[derive(serde::Serialize, ToSchema)]
struct Whoami {
    /// Credential kind: the browser session, a named API token, or the
    /// environment bootstrap token.
    kind: CredentialKind,
    /// The username for a session, the token's name for an API token, and
    /// `bootstrap` for the environment token.
    name: String,
    /// Highest scope held, where each scope includes the ones below it. Sessions and the bootstrap
    /// token hold `admin`.
    scope: AuthScope,
    /// Unix expiry in seconds, null when the credential does not expire (sessions never do).
    expires_at: Option<i64>,
}

/// Returns the kind, name, scope and expiry of the credential that made the
/// request. A revoked or expired token is a 401.
#[utoipa::path(
    get,
    path = "/api/v1/auth/whoami",
    operation_id = "get_whoami",
    tag = "Tokens",
    responses(
        (status = 200, description = "The calling credential", body = Whoami),
        (status = 401, description = "Missing, revoked or expired credentials"),
    ),
)]
async fn whoami(Extension(user): Extension<AuthUser>) -> Json<Whoami> {
    let name = match user.token_name {
        Some(token_name) => token_name,
        None if user.kind == CredentialKind::Bootstrap => "bootstrap".to_string(),
        None => user.username,
    };
    Json(Whoami {
        kind: user.kind,
        name,
        scope: user.scope,
        expires_at: user.expires_at,
    })
}

fn require_cookie_auth(user: &AuthUser) -> Result<(), AppError> {
    if user.kind != CredentialKind::Session {
        Err(AppError::forbidden(
            "token management requires the browser session; API tokens cannot mint new tokens",
        )
        .with_code(ErrorCode::SessionRequired))
    } else {
        Ok(())
    }
}

/// Creates an API token and returns its secret once.
#[utoipa::path(
    post,
    path = "/api/v1/auth/tokens",
    operation_id = "create_api_token",
    tag = "Tokens",
    request_body = CreateTokenInput,
    responses(
        (status = 200, description = "Created token (raw_token shown once)", body = CreateTokenResponse),
        (status = 400, description = "Invalid name or scopes"),
        (status = 403, description = "`session_required`: needs the browser session cookie, so API tokens cannot mint tokens"),
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
    let scope = submitted_scope(&input.scopes)?;
    let expires_at = match input.expires_in_days {
        Some(days) if days > 0 => {
            Some(chrono::Utc::now().timestamp() + days.min(3650) * SECONDS_PER_DAY)
        }
        _ => None,
    };
    let (record, raw) = auth::new_api_token_record(name, scope, expires_at);
    state.db.create_api_token(&record).map_err(AppError::from)?;
    Ok(Json(CreateTokenResponse {
        token: ApiTokenView::from(&record),
        raw_token: raw,
    }))
}

/// Lists the API tokens, without their secrets.
#[utoipa::path(
    get,
    path = "/api/v1/auth/tokens",
    operation_id = "list_api_tokens",
    tag = "Tokens",
    responses(
        (status = 200, description = "Active tokens (no secrets)", body = Vec<ApiTokenView>),
        (status = 403, description = "`session_required`: needs the browser session cookie"),
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

/// Revokes an API token.
#[utoipa::path(
    delete,
    path = "/api/v1/auth/tokens/{id}",
    operation_id = "revoke_api_token",
    tag = "Tokens",
    params(("id" = String, Path, description = "Token id")),
    responses(
        (status = 200, description = "Revoked token id", body = RevokeResult),
        (status = 403, description = "`session_required`: needs the browser session cookie"),
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

/// Serves the agent skill: the ChipFlow workflow in Agent Skills format.
#[utoipa::path(
    get,
    path = "/skill.md",
    operation_id = "get_skill",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "Agent skill (Markdown)", content_type = "text/markdown"),
    ),
)]
async fn skill_md() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/markdown; charset=utf-8")],
        docs::SKILL,
    )
}

/// Serves the index for an agent that knows only the base URL, with absolute links.
#[utoipa::path(
    get,
    path = "/llms.txt",
    operation_id = "get_llms_txt",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "Index of the agent documents", content_type = "text/plain"),
    ),
)]
async fn llms_txt(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        docs::render_llms_txt(&base_url(&state, &headers)),
    )
}

/// Serves the machine-readable contract for the JSON API, generated from the handlers' route
/// annotations.
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    operation_id = "get_openapi_spec",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "This document (generated OpenAPI)", content_type = "application/json"),
    ),
)]
async fn openapi_json(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let mut doc = ApiDoc::openapi();
    if let Some(url) = &state.public_url {
        doc.servers = Some(vec![utoipa::openapi::Server::new(url)]);
    }
    let spec = doc
        .to_json()
        .map_err(|error| AppError::internal(format!("failed to serialize OpenAPI: {error}")))?;
    Ok(([(CONTENT_TYPE, "application/json")], spec))
}

/// Serves the API reference as Markdown, rendered from the OpenAPI document on each request.
///
/// Each tag is a section that opens with its concept prose and lists its operations with their
/// parameters, request body, responses and error codes. The schemas follow at the end.
#[utoipa::path(
    get,
    path = "/docs/api.md",
    operation_id = "get_api_reference",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "API reference (Markdown)", content_type = "text/markdown"),
    ),
)]
async fn api_reference_md() -> Result<impl IntoResponse, AppError> {
    let spec = serde_json::to_value(ApiDoc::openapi())
        .map_err(|error| AppError::internal(format!("failed to serialize OpenAPI: {error}")))?;
    Ok((
        [(CONTENT_TYPE, "text/markdown; charset=utf-8")],
        docs::render_api_reference(&spec),
    ))
}

/// Public build identity for agents and health checks: the crate version and the git SHA baked in
/// at build time (`CHIPFLOW_BUILD_SHA`, or `"unknown"` when the build didn't set it).
#[derive(serde::Serialize, ToSchema)]
struct VersionInfo {
    version: String,
    build_sha: String,
    /// Version of the agent skill at `GET /skill.md`. An installed copy whose `metadata.version`
    /// differs is stale.
    skill_version: String,
}

/// Returns the build version, commit SHA and skill version.
#[utoipa::path(
    get,
    path = "/api/v1/version",
    operation_id = "get_version",
    tag = "Discovery",
    security(()),
    responses(
        (status = 200, description = "Build version, commit SHA and skill version", body = VersionInfo),
    ),
)]
async fn version_info() -> Json<VersionInfo> {
    Json(VersionInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_sha: option_env!("CHIPFLOW_BUILD_SHA")
            .unwrap_or("unknown")
            .to_string(),
        skill_version: docs::SKILL_VERSION.to_string(),
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
    /// Background hex of the task's color (the running popup's task row shows a color dot before
    /// the task name).
    task_color: Option<String>,
    /// URL to the task, if a task is attached.
    task_url: Option<String>,
    started_at: Option<i64>,
    /// In-memory id of the running session, distinct even for two sessions
    /// started in the same second. Pass it to `POST /api/timer/stop` as
    /// `session_id`. Null when idle, and null for a session that was running
    /// when the server restarted (stop then matches by `started_at`).
    session_id: Option<u64>,
    duration_secs: Option<u64>,
    /// Seconds remaining (computed from started_at + duration_secs).
    remaining_seconds: Option<i64>,
    /// Total duration in seconds (= duration_secs).
    total_seconds: Option<u64>,
    /// Seconds since the session started, by the server's clock (keeps
    /// counting past the duration). The web UI ticks from this, so a skewed
    /// browser clock never shortens or ends a session.
    elapsed_seconds: Option<i64>,
    /// Number of pomodoros completed today.
    pomodoro_count: u32,
}

fn timer_status_view(db: &Db, timer: Option<ActiveTimer>) -> Result<TimerStatusView, AppError> {
    // Today's completed pomodori pace the long-break cycle, and stopped ones do not count
    let pomodoro_count = db
        .entries_today()
        .map(|entries| {
            entries
                .iter()
                .filter(|(entry, _)| {
                    EntryKind::parse(&entry.kind).is_some_and(EntryKind::is_pomodoro)
                        && !entry.interrupted
                })
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
            task_color: None,
            task_url: None,
            started_at: None,
            duration_secs: None,
            remaining_seconds: None,
            total_seconds: None,
            elapsed_seconds: None,
            session_id: None,
            pomodoro_count,
        }),
        Some(timer) => {
            // Resolve the task's color for the popup's color dot.
            let (task_name, task_color) = match timer.task_id.as_deref() {
                Some(id) => match db.get_task(id).map_err(AppError::from)? {
                    Some(task) => {
                        let color = match task.color_id.as_deref() {
                            Some(cid) => db
                                .get_color(cid)
                                .map_err(AppError::from)?
                                .map(|c| c.background_hex),
                            None => None,
                        };
                        (Some(task.name), color)
                    }
                    None => (None, None),
                },
                None => (None, None),
            };

            // A timer whose task is gone reads as idle. Deleting a task clears its timer, so this
            // only covers older databases, and a status read never writes.
            if timer.task_id.is_some() && task_name.is_none() {
                return Ok(TimerStatusView {
                    active: false,
                    phase: "idle".to_string(),
                    mode: None,
                    mode_title: None,
                    task_id: None,
                    task_name: None,
                    task_color: None,
                    task_url: None,
                    started_at: None,
                    duration_secs: None,
                    remaining_seconds: None,
                    total_seconds: None,
                    elapsed_seconds: None,
                    session_id: None,
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
                task_color,
                task_url,
                started_at: Some(timer.started_at),
                session_id: db.current_session(),
                duration_secs: timer.duration_secs,
                remaining_seconds,
                total_seconds: timer.duration_secs,
                elapsed_seconds: Some((now - timer.started_at).max(0)),
                pomodoro_count,
            })
        }
    }
}

/// Returns the running timer, or an idle status when none runs.
#[utoipa::path(
    get,
    path = "/api/timer/status",
    operation_id = "get_timer_status",
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
    let _gate = state.db.lock_timer();
    let timer = state.db.get_active_timer().map_err(AppError::from)?;
    Ok(Json(timer_status_view(&state.db, timer)?))
}

#[derive(Deserialize, ToSchema)]
struct TimerStartInput {
    task_id: Option<String>,
    mode: String,
}

/// Starts a timer, replacing any active one. A replaced session of 20+ seconds is logged as
/// interrupted ("Switched task") so no work time silently vanishes, and shorter ones are discarded.
#[utoipa::path(
    post,
    path = "/api/timer/start",
    operation_id = "start_timer",
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
    if let Some(task_id) = input.task_id.as_deref()
        && db.get_task(task_id).map_err(AppError::from)?.is_none()
    {
        return Err(AppError::not_found("task not found"));
    }

    // One start at a time retires the old session, stores the new timer and begins its session id,
    // and answers with that id.
    let _gate = db.lock_timer();

    // Retire any running timer first, with the same logging rules as stop.
    if let Some(old) = db.get_active_timer().map_err(AppError::from)? {
        log_timer_session(db, &old, false, Some("Switched task"), None, user.actor())?;
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
    db.begin_session();
    Ok(Json(timer_status_view(db, Some(timer))?))
}

#[derive(Deserialize, ToSchema)]
struct TimerStopInput {
    /// Taken as sent for a stopwatch only. The server decides for a pomodoro or break.
    #[serde(default)]
    completed: bool,
    /// Why a pomodoro was stopped early ("Why did you stop?").
    reason: Option<String>,
    /// Break-activity id attributing a break session's time to a defined break activity. Ignored
    /// unless the id exists in settings.
    activity_id: Option<String>,
    /// The `started_at` of the session the caller means to stop. When it is no longer the active
    /// session (another client stopped or replaced it), the stop succeeds without touching the
    /// timer and reports `already_stopped`.
    started_at: Option<i64>,
    /// The `session_id` of the session the caller means to stop. Unlike `started_at` it tells apart
    /// two sessions started in the same second. A mismatch is reported as `already_stopped`.
    /// Ignored for a session the server restarted under (its id is gone).
    session_id: Option<u64>,
}

/// Stops the active timer and logs the session.
///
/// The server decides completion. A pomodoro or break counts as completed only once its elapsed
/// time reaches the duration, less a 2-second grace for the client's 1 s tick (1498 s of a 1500 s
/// pomodoro completes, 1497 s does not), whatever `completed` says. A stopwatch has no duration,
/// so for it the server takes `completed` as sent. The reply's `completed` is that verdict.
///
/// 1. A session under 20 seconds is discarded and logs nothing.
/// 2. A session logs at most one hour past its duration, and a stopwatch at most 12 hours, so a
///    timer left running across days does not log days of time.
/// 3. Only a completed pomodoro counts toward the task's pomodoro total. A pomodoro stopped
///    earlier logs its time and records an interruption, labelled with `reason` when one is sent.
///    A stopwatch or break entry is never interrupted.
/// 4. A session without a task is logged too, and a break with an `activity_id` naming a defined
///    break activity counts toward that activity.
///
/// Pass the status `started_at` or `session_id` to stop only that session. When it is no longer
/// active (another client stopped or replaced it) the stop is a 200 with `already_stopped` set and
/// changes nothing. A `session_id` tells apart two sessions started in the same second and never
/// repeats across a restart. A session running across a restart reports `session_id` null, and a
/// stop then matches by `started_at` alone. Without either, stopping an idle timer is a 404.
#[utoipa::path(
    post,
    path = "/api/timer/stop",
    operation_id = "stop_timer",
    tag = "Timer",
    request_body = TimerStopInput,
    responses(
        (status = 200, description = "Stop result with logged minutes", body = TimerStopResult),
        (status = 404, description = "No active timer, and neither started_at nor session_id given"),
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

    let _gate = db.lock_timer();
    let active = db.get_active_timer().map_err(AppError::from)?;
    let stale_session = input
        .session_id
        .is_some_and(|id| db.current_session().is_some_and(|current| current != id));
    if stale_session
        || (input.session_id.is_some() && active.is_none())
        || input.started_at.is_some_and(|started_at| {
            active.as_ref().map(|timer| timer.started_at) != Some(started_at)
        })
    {
        return Ok(Json(TimerStopResult {
            discarded: false,
            minutes: None,
            completed: false,
            already_stopped: true,
        }));
    }
    let timer = active.ok_or_else(|| AppError::not_found("no active timer"))?;
    let logged = log_timer_session(
        db,
        &timer,
        input.completed,
        input.reason.as_deref(),
        input.activity_id.as_deref(),
        user.actor(),
    )?;

    // Do NOT auto-insert custom "why did you stop?" reasons here. KanbanFlow's verified path is the
    // Interruptions tab's "Add reason" button, and automatic addition on timer stop was never
    // verified. "Task done" is never a configured reason.

    Ok(Json(TimerStopResult {
        discarded: logged.is_none(),
        minutes: logged.map(|(minutes, _)| minutes),
        completed: logged.is_some_and(|(_, completed)| completed),
        already_stopped: false,
    }))
}

/// Seconds a timed session may fall short of its duration and still count as completed, covering
/// the client's 1 s tick.
const COMPLETION_GRACE_SECS: i64 = 2;

/// The most a timed session logs past its duration while it waits for "Take break" or Start.
const MAX_OVERTIME_SECS: i64 = 3600;

/// The most one stopwatch session logs.
const MAX_STOPWATCH_SECS: i64 = 12 * 3600;

/// The seconds a session logs, so a timer left running across days cannot log days of work.
fn loggable_seconds(timer: &ActiveTimer, elapsed: i64) -> i64 {
    match timer.duration_secs {
        Some(duration) => elapsed.min(duration as i64 + MAX_OVERTIME_SECS),
        None => elapsed.min(MAX_STOPWATCH_SECS),
    }
}

/// Shared stop logic for timer_stop and timer_start's replacement path.
/// Returns the logged minutes and whether the session completed, or None
/// when it was discarded. `claimed_completed` counts only for a session
/// without a duration (a stopwatch); a timed one completes by the server's
/// own elapsed time.
fn log_timer_session(
    db: &Db,
    timer: &ActiveTimer,
    claimed_completed: bool,
    reason: Option<&str>,
    activity_id: Option<&str>,
    actor: Actor<'_>,
) -> Result<Option<(i64, bool)>, AppError> {
    let elapsed = (chrono::Utc::now().timestamp() - timer.started_at).max(0);
    db.clear_active_timer().map_err(AppError::from)?;
    if elapsed < 20 {
        return Ok(None);
    }

    // A timed session completed when the server's elapsed time reached its duration, even when it
    // is retired later (the pomodoro waits at 00:00 for "Take break"). An early `completed` claim
    // is ignored.
    let completed = match timer.duration_secs {
        Some(duration) => elapsed + COMPLETION_GRACE_SECS >= duration as i64,
        None => claimed_completed,
    };
    let started_at = DateTime::from_timestamp(timer.started_at, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();

    // Only attribute the session to a break activity when the id matches a defined activity, and
    // anything else is ignored (not stored)
    let activity_id = activity_id.filter(|id| {
        db.get_settings()
            .map(|s| s.break_activities.iter().any(|a| a.id == **id))
            .unwrap_or(false)
    });

    // Keep second precision for sub-minute durations ("31s", not a 1m floor).
    let seconds = loggable_seconds(timer, elapsed);
    let minutes = seconds / 60;

    // Only a pomodoro can be interrupted and a stopwatch just stops
    let interrupted = !completed && timer.mode == TimerMode::Pomodoro;

    // A completed session has no stop reason.
    let reason = reason.filter(|_| interrupted);

    let logged_on_task = match timer.task_id.as_deref() {
        Some(task_id) => db
            .create_entry_full(NewTimeEntry {
                task_id: Some(task_id),
                started_at: &started_at,
                minutes,
                seconds,
                note: "",
                kind: timer.mode.entry_kind(),
                interrupted,
                interrupt_reason: reason,
                labels: &[],
                created_by: actor.name,
                activity_id,
            })
            .map_err(AppError::from)?
            .map(|_| task_id),
        None => None,
    };
    let Some(task_id) = logged_on_task else {
        // Sessions without a task, breaks included, still reach the timer log and the
        // break-activity statistics, as does one whose task was deleted under the timer.
        db.create_entry_full(NewTimeEntry {
            task_id: None,
            started_at: &started_at,
            minutes,
            seconds,
            note: "",
            kind: timer.mode.entry_kind(),
            interrupted,
            interrupt_reason: reason,
            labels: &[],
            created_by: actor.name,
            activity_id,
        })
        .map_err(AppError::from)?;
        return Ok(Some((minutes, completed)));
    };
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
            // Only a pomodoro that ran to zero counts as a Pomodoro (card tomato). A
            // stopped/interrupted session logs its time and records an interruption, but must not
            // bump the pomodori counter.
            db.record_interruption(task_id).map_err(AppError::from)?;
        }
        _ => {}
    }
    Ok(Some((minutes, completed)))
}

#[derive(Deserialize, ToSchema)]
struct TimerRetargetInput {
    task_id: Option<String>,
}

/// Points the active timer at a different task ("Change task").
#[utoipa::path(
    post,
    path = "/api/timer/retarget",
    operation_id = "retarget_timer",
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

    if let Some(task_id) = input.task_id.as_deref()
        && db.get_task(task_id).map_err(AppError::from)?.is_none()
    {
        return Err(AppError::not_found("task not found"));
    }
    let _gate = db.lock_timer();
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
    /// The task color's border shade for the TODAY dot. Null for a task-less entry or a task
    /// without a color.
    task_color: Option<String>,
    minutes: i64,
    /// Exact length in seconds, for the TODAY day total.
    seconds: i64,
    kind: String,
    kind_label: String,
    started_display: String,
    ended_display: String,
    interrupted: bool,
    interrupt_reason: Option<String>,
}

/// Lists today's logged sessions for the timer popup's "Today" list.
#[utoipa::path(
    get,
    path = "/api/timer/today",
    operation_id = "get_timer_today",
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
    let db = &state.db;
    let rows = db.entries_today().map_err(AppError::from)?;
    let entries = rows
        .into_iter()
        .map(|(entry, task_name)| {
            let task_color = task_dot_color(db, &entry.task_id);
            let seconds = entry.duration_seconds();
            let kind_label = match EntryKind::parse(&entry.kind) {
                Some(EntryKind::Pomodoro) => "Pomodoro",
                Some(EntryKind::Stopwatch) => "Stopwatch",
                Some(EntryKind::ShortBreak) => "Short break",
                Some(EntryKind::LongBreak) => "Long break",
                Some(EntryKind::Manual) | None => "Time",
            }
            .to_string();
            let started_display = DateTime::parse_from_rfc3339(&entry.started_at)
                .map(|dt| dt.with_timezone(&Local).format("%-I:%M %p").to_string())
                .unwrap_or(entry.started_at.clone());
            let ended_display = DateTime::parse_from_rfc3339(&entry.started_at)
                .map(|dt| {
                    (dt + chrono::Duration::seconds(entry.duration_seconds()))
                        .with_timezone(&Local)
                        .format("%-I:%M %p")
                        .to_string()
                })
                .unwrap_or_default();
            TodayEntryView {
                id: entry.id,
                task_name,
                task_color,
                minutes: entry.minutes,
                seconds,
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

/// Adds the `bearerAuth` HTTP bearer security scheme for the `cf_...` API tokens, and gives each
/// operation that needs credentials the scope [`auth::required_scope`] demands of it.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut OpenApiSpec) {
        for (path, item) in &mut openapi.paths.paths {
            let methods = [
                (Method::GET, &mut item.get),
                (Method::POST, &mut item.post),
                (Method::PUT, &mut item.put),
                (Method::PATCH, &mut item.patch),
                (Method::DELETE, &mut item.delete),
            ];
            for (method, operation) in methods {
                let Some(operation) = operation else {
                    continue;
                };
                let public = operation.security.as_ref().is_some_and(|requirements| {
                    requirements
                        .iter()
                        .all(|r| *r == SecurityRequirement::default())
                });
                if !public {
                    let scope = auth::required_scope(&method, path);
                    operation.security = Some(vec![SecurityRequirement::new(
                        "bearerAuth",
                        [scope.as_str()],
                    )]);
                }
            }
        }
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

/// Appends the request limits to the API description, formatted from the limit constants.
struct RateLimitAddon;

impl Modify for RateLimitAddon {
    fn modify(&self, openapi: &mut OpenApiSpec) {
        let note = format!(
            "Rate limits. Each API token (and the bootstrap token) gets {} requests per second \
             with a burst of {}, and each client IP gets {} requests per minute on \
             `POST /api/v1/auth/device` and {} on `POST /login`. The browser session is not \
             limited. Over a limit any operation answers 429 `rate_limited` with a `Retry-After` \
             header in whole seconds (at least 1). Wait that long, then retry. The counters are \
             kept in memory and reset when the server restarts. Behind a reverse proxy on the \
             same host the client IP is the last `X-Forwarded-For` entry, and from any other \
             peer that header is ignored.",
            Buckets::PER_SECOND as u32,
            Buckets::BURST as u32,
            Limits::PER_IP_PER_MINUTE,
            Limits::PER_IP_PER_MINUTE,
        );
        let description = openapi.info.description.take().unwrap_or_default();
        openapi.info.description = Some(format!("{description}\n\n{note}"));
    }
}

/// Gives every 4xx and 5xx response the [`ErrorBody`] JSON schema that Bearer clients receive.
struct ErrorBodyAddon;

impl Modify for ErrorBodyAddon {
    fn modify(&self, openapi: &mut OpenApiSpec) {
        for item in openapi.paths.paths.values_mut() {
            let operations: [&mut Option<Operation>; 5] = [
                &mut item.get,
                &mut item.post,
                &mut item.put,
                &mut item.patch,
                &mut item.delete,
            ];
            for operation in operations.into_iter().flatten() {
                for (status, response) in &mut operation.responses.responses {
                    if !status.starts_with(['4', '5']) {
                        continue;
                    }
                    if let RefOr::T(response) = response {
                        let content = ContentBuilder::new()
                            .schema(Some(Ref::from_schema_name("ErrorBody")))
                            .build();
                        response
                            .content
                            .insert("application/json".to_string(), content);
                    }
                }
            }
        }
    }
}

/// API-wide conventions, the start of the spec's `info.description`.
const API_DOC: &str = r#"JSON API for ChipFlow, a self-hosted kanban board with a pomodoro timer and time tracking.
Paths are relative to the server's base URL, and `GET /docs/api.md` renders this document as
Markdown.

Authenticate with `Authorization: Bearer cf_...` (see Tokens) and send
`Content-Type: application/json`. A Bearer request gets JSON responses and JSON errors whatever
its `Accept` header says.

Errors sent to a Bearer token, and every 4xx sent to an `/api/*` request with neither a token nor
a cookie, are an `ErrorBody` with a `code` and a `message`. Match on `code`, a stable snake_case
`ErrorCode` value, and leave `message` to people. Each response below names the codes it can carry. A 401 on `/api/*` also carries
`WWW-Authenticate: Bearer`, `Link: </llms.txt>; rel="help"` and a `help` field. An unknown
`/api/*` path is a 404 `route_not_found` and a wrong method a 405 `method_not_allowed`, both
naming `/api/v1/openapi.json`. Requests that carry the session cookie keep plain-text error bodies.

Timestamps are RFC3339 strings, except Unix seconds (UTC) for the timer status `started_at` and
the API token `created_at`, `last_used_at` and `expires_at`. IDs are UUID strings. Durations are
truncated to whole minutes, never rounded up."#;

/// Concept prose for the `Tasks` tag.
const TASKS_DOC: &str = r#"Kanban tasks with their subtasks, labels, comments, attachments and watch flag. A task sits
in one column and one swimlane, and its board is its column's board.

Task writes are all-or-nothing. Creating, patching, moving, commenting on, linking, claiming and
watching a task, logging time on it, and adding, editing or deleting its subtasks and attachments
validate the whole request first and save it, history events included, in one transaction. A
request rejected with a 4xx changes nothing, except that a 409 heartbeat or release against an
expired claim records that expiry (see Claims).

`updated_at` changes on every write to the stored task, including a position shift caused by a
neighbor's move and each history event. It also changes when the task's `blocked` value flips
(see Links). Tasks stored before `updated_at` existed report `created_at` until their next write.

A column whose `is_done` is true is a Done column. Entering one stamps the task's `completed_at`
and leaving it clears it, and a task created in a Done column is done at once.

### Due dates

After surrounding whitespace is trimmed, `due_at` takes one of three forms:

1. RFC3339, stored exactly as sent, so the offset and the case of `T` and `Z` come back as
   written. As RFC3339 allows, a lowercase `t` or a space may replace the `T`, and a lowercase
   `z` the `Z`.
2. `YYYY-MM-DD HH:MM` or `YYYY-MM-DD HH:MM:SS` in the server's local timezone, with exactly one
   space or uppercase `T` between date and time.
3. A bare `YYYY-MM-DD`, due that day at the board's due date time (17:00 unless
   `PUT /api/boards/{id}/config` sets `due_time`) in the server's local timezone.

The year is always four digits with no sign. In forms 2 and 3 every other field may drop its
leading zero (`2026-1-5`, `2026-10-20 8:5:7`). Seconds run 00 to 59 in every form, so a `:60` leap
second is a 400. A local time that occurs twice at a daylight-saving fall-back resolves to the
earlier instant, and a local time that a spring-forward skips is a 400. Anything else is a 400 and
the request changes nothing. Null, an empty string or whitespace clears the due date. Re-sending
the same instant in another format is not a change, so the stored string and `due_done` stay,
while a new instant resets `due_done`. A task is overdue only once the full due timestamp has
passed, never merely on the due date. Subtask `due_at` takes the same forms.

`due_repeat` is free text such as `every week` and needs a due date: clearing `due_at` clears it,
and a repeat sent for a task with no due date is dropped. `due_target_column_id` names the column
the task should reach by its due date, a column on the task's board. Sending one for a task with
no due date is a 400, and clearing `due_at`, deleting that column or moving the task to another
board clears it. `due_done` marks the due date done and is ignored without a due date."#;

/// Concept prose for the `Claims` tag.
const CLAIMS_DOC: &str = r#"A claim is a lease on one task that tells other agents the task is taken. Claims are advisory:
any reader sees the claim and its fence, `claimant` is whatever the caller sends, and only writes
that carry `If-Claim-Fence` are checked against it.

Each new claim gets a `fence`, a per-task integer that goes up by one on every new claim, so a
stale worker's writes can be refused. The check and the write happen in one transaction, and of
two simultaneous claims exactly one gets 200.

1. Claim with `claimant` and `ttl_seconds` (1 to 86400). While another live claim holds the task
   the answer is 409 `claim_held`. The same claimant sending its live `fence` renews the lease
   (same fence and `started_at`, new `heartbeat_at` and `expires_at`, and any optional field it
   sends replaces the stored one). The same claimant without that fence gets the same 409.
2. Heartbeat with the live `fence` to extend the lease. Without `ttl_seconds` a heartbeat reuses
   the lease's previous length. A fence that is not live is a 409 `stale_fence`.
3. Release with the `fence`. With no live claim, any fence the task has issued releases with
   200, so retries are safe. A live claim holding another fence, or a fence never issued, is a
   409 `stale_fence`.

`If-Claim-Fence: <fence>` on `PATCH /api/tasks/{id}`, `POST /api/tasks/{id}/move` and
`POST /api/tasks/{id}/comments` refuses the write with 409 `stale_fence` when a live claim holds a
different fence. Without a live claim the header passes, and a value that is not an integer is a
400. People and the browser UI never send it.

Expiry is lazy, with no background job. Every read reports a claim whose `expires_at` has passed
as `"claim": null`, and reads never write. The history records `claimed` for a new claim
(renewals add nothing), `claim_released` for a release by the live fence, and `claim_expired`
once, written by the first claim, heartbeat or release call that finds the stored claim expired.
A task whose expired claim is never touched again has no `claim_expired` event.

A claim, a release and an expiry each change the task's `updated_at`. A heartbeat and a renewal
change only the lease, so `?since=` polling does not return every claimed task on every tick.
`GET /api/boards/{id}/export-data` shows the stored claim without lazy expiry."#;

/// Concept prose for the `Links` tag.
const LINKS_DOC: &str = r#"A link joins two tasks: `blocked_by`, its inverse `blocks`, or `related`. A `blocks` link is
stored as `blocked_by` on its target, so both forms of one dependency are one link with one id. A
link may target a task on another board, and its `board_id` is the other task's current board.

Readiness. A task is `blocked` while any of its `blocked_by` targets is in a column that is not
Done. A blocker in any Done column, whatever its name, no longer blocks.
`GET /api/boards/{id}/tasks?ready=true` lists the tasks that are not blocked. When a blocker
enters or leaves a Done column, a column's `is_done` is toggled or a blocker is deleted, every
dependent whose `blocked` value flipped gets a new `updated_at`, on any board, so
`?since=...&ready=true` finds newly ready tasks.

Each task lists its links from its own side: the blocked task sees `blocked_by`, the blocker sees
`blocks`, and both see `related`. Deleting a task, or its board, deletes every link to or from it.
Adding a link appends `link_added` and removing one appends `link_removed` to the history of the
task that holds the link, and a link removed with its other task logs `link_removed` with
` (task deleted)` appended."#;

/// Concept prose for the `Fields` tag.
const FIELDS_DOC: &str = r#"Custom fields are typed values that a board defines and its tasks carry in `fields`, a
`{key: value}` object on the task detail and on board task list entries (`{}` when none). Fields
are for people and agents alike, and `metadata` is the machine-only store.

A field definition has:

- `key`: required, dotted lowercase segments that each start with a letter, at most 64
  characters (`^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`). A key the board already has is a 409
  `field_key_exists`, so a client can create or adopt.
- `kind`: required, one of `text` (a single line), `multiline`, `number`, `bool`, `select` and
  `multiselect`.
- `options`: required, non-empty and unique for `select` and `multiselect`, and not allowed for
  the other kinds. Each option is a single line.
- `default`: checked like a value. New tasks start with it when they do not set the field.
- `required`: a UI hint only, never a 400. The task dialog marks the empty input and the card
  shows a "fields incomplete" badge.
- `owner`: `user` (the default, editable in the task dialog) or `system` (read-only there).

The key, kind and owner are fixed once the field exists.

Task values merge per key on `POST /api/tasks` and `PATCH /api/tasks/{id}`, and a null value
clears that key. Each value is checked against its field's kind and options, and every 400 names
the key, as in `field "points": expected a number`. `fields` must be an object, and anything else,
null included, is a 400. Values that a field's options no longer list stay stored until changed.
A move to another board keeps a value only when that board defines the same key and accepts the
value, and the dropped values are written into the move's `moved` history event. Field changes
are not recorded in the task history.

`metadata` is a JSON object the caller owns. The server checks that it is an object, never its
contents. The merge is per top-level key: each key sent replaces the stored one whole, and a key
set to null is deleted. Deleting the last key leaves `"metadata": null`, never `{}`, and
`"metadata": null` clears every key. Metadata changes are not recorded in the task history. A
request carrying `metadata` whose body is over 64 KiB is a 400, as is a merged object over
64 KiB."#;

/// Concept prose for the `Time` tag.
const TIME_DOC: &str = r#"Time entries record work on a task. Timer sessions create them when they stop (see Timer), and
the operations below add or edit them by hand. An entry carries `minutes` and `seconds`, since a
timer entry keeps second precision.

- `POST /api/tasks/{id}/time` logs 1 to 1440 minutes on a task, with an optional note.
- `POST /api/time/manual` logs a `date` (`YYYY-MM-DD`) and a `from` and `to` time (`HH:MM` or
  `HH:MM:SS`, 24-hour) in the server's local timezone. A time in the future or a range under one
  minute is a 400.
- `PUT /api/time/entries/{id}` replaces an entry's date, times, note and labels (an absent note
  or labels clears them) and keeps second precision. A `task_id` must name an existing task (404
  otherwise) and is required for an entry that has a task, and naming a task moves the entry to
  it.

Manual entries count toward time spent, never toward pomodori. Sessions run without a task,
breaks included, and sessions whose task was deleted while they ran are logged with `task_id` set
to `""` and show as "No task" in the timer log. A task-less entry stays task-less on edit when
`task_id` is absent or empty. The days of the timer log, time spent and statistics are calendar
days in the server's timezone."#;

/// Concept prose for the `Timer` tag.
const TIMER_DOC: &str = r#"One timer runs at a time, and the server keeps its state, so `started_at` from
`GET /api/timer/status` is authoritative. The modes are `pomodoro`, `stopwatch`, `short_break`
and `long_break`, with durations from settings (25, 5 and 15 minutes unless changed), and a
stopwatch has none. Starting a timer replaces the running one and logs the replaced session by
the stop rules with the reason "Switched task". `POST /api/timer/retarget` points a running timer
at another task without stopping it."#;

/// Concept prose for the `Tokens` tag.
const TOKENS_DOC: &str = r#"Every operation authenticates with `Authorization: Bearer cf_...` (the scheme name is
case-insensitive) unless it says "Auth: none". The owner creates tokens under Settings > API
tokens, or a client pairs through the device flow below. A token is shown once, at creation.

Tokens carry one of three ordered scopes, and each includes the ones before it. `read` allows
every GET except the calendar-feed secret. `work` adds the task workflow: create, update and move
tasks, claim, heartbeat and release, comments, subtasks, links and attachments, time and manual
time, timer start, stop and retarget, and watch. `admin` allows everything else: every delete,
boards, columns, swimlanes, fields, colors, templates, settings and the calendar feed. Each
operation below names the scope it needs. A request outside the token's scope gets 403
`insufficient_scope`, and a scope list sent to create a token or start a device request keeps only
its highest entry. An unknown or revoked token gets 401 `token_invalid`, and a token past its
expiry 401 `token_expired`. Creating, listing and revoking tokens needs the browser session (403
`session_required` for a Bearer token), so a token never mints tokens.

Device pairing (RFC 8628) gives a client its own token without pasting one:

1. `POST /api/v1/auth/device` with a `client_name` and `scopes` returns a `device_code`, a
   `user_code` and a `verification_url`.
2. The person opens `verification_url` while logged in, types `user_code` (case, spaces and the
   dash do not matter) and checks the client name and the requested scope. A request for `admin`
   is shown with a warning. The person approves with that scope or a lower one, never a higher
   one, and picks the token's expiry: 30, 90 (the default) or 365 days, or never. Or they deny.
3. The client polls `POST /api/v1/auth/device/token` with the `device_code`, waiting
   `interval_seconds` between polls, until it gets the token or a final error.

The device code is a single-use secret, stored only as a hash. Ten wrong user codes within ten
minutes lock the approval form for the rest of that window. Cookie-authenticated writes must be
same-origin, and Bearer requests skip that check.

A server started with `CHIPFLOW_BOOTSTRAP_TOKEN` set also accepts that value as a token with full
access, which works before the admin account exists, for headless setup. `whoami` reports it as
`kind` `bootstrap`."#;

/// Code-generated OpenAPI served at `GET /api/v1/openapi.json`. The
/// `#[utoipa::path]` annotations on the handlers above are the source of
/// truth — the spec tracks the implementation by construction.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "ChipFlow API",
        version = env!("CARGO_PKG_VERSION"),
        license(name = "Proprietary"),
        description = API_DOC
    ),
    paths(
        list_tasks,
        create_task,
        get_task,
        update_task,
        delete_task,
        move_task,
        get_task_time,
        log_time,
        watch_task,
        create_subtask,
        reorder_subtasks,
        create_task_link,
        list_task_links,
        delete_task_link,
        claim_task,
        heartbeat_claim,
        release_claim,
        update_subtask,
        delete_subtask,
        list_board_labels,
        create_comment,
        list_comments,
        delete_comment,
        upload_attachment,
        download_attachment,
        delete_attachment,
        delete_time_entry,
        list_members,
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
        api_timer_settings,
        api_update_settings,
        delete_board_api,
        update_board_config,
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
        get_board_detail,
        list_board_tasks,
        create_board_field,
        list_board_fields,
        update_board_field,
        delete_board_field,
        board_export_data,
        get_calendar_feed_link,
        create_calendar_feed_link,
        reset_calendar_feed,
        serve_calendar_feed,
        create_board,
        save_board_as_template,
        list_templates,
        delete_template,
        list_board_colors,
        create_board_color,
        update_board_color,
        delete_board_color,
        copy_board_colors,
        create_api_token,
        whoami,
        list_api_tokens,
        revoke_api_token,
        start_device_auth,
        poll_device_token,
        skill_md,
        llms_txt,
        openapi_json,
        api_reference_md,
        version_info,
    ),
    components(
        schemas(
            TaskNameItem,
            IdResult,
            IdMinutesResult,
            TimeEntryDetail,
            TimeEntryItem,
            ErrorBody,
            ErrorCode,
            LogTimeInput,
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
            WatchTaskInput,
            WatchTaskResponse,
            CreateSubtaskInput,
            ReorderSubtasksInput,
            UpdateSubtaskInput,
            SubtaskDetail,
            CreateCommentInput,
            UploadAttachmentInput,
            TaskComment,
            ActorKind,
            TaskAttachment,
            MemberDetail,
            TaskDetail,
            TaskSummary,
            TaskLink,
            LinkKind,
            CreateLinkInput,
            TaskClaim,
            FieldDef,
            FieldKind,
            FieldOwner,
            CreateFieldInput,
            UpdateFieldInput,
            ClaimInput,
            HeartbeatInput,
            ReleaseInput,
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
            DeviceAuthInput,
            DeviceAuthResponse,
            DeviceTokenInput,
            DeviceTokenResponse,
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
    modifiers(&SecurityAddon, &ErrorBodyAddon, &RateLimitAddon),
    security(("bearerAuth" = [])),
    tags(
        (name = "Tasks", description = TASKS_DOC),
        (name = "Claims", description = CLAIMS_DOC),
        (name = "Links", description = LINKS_DOC),
        (name = "Fields", description = FIELDS_DOC),
        (name = "Time", description = TIME_DOC),
        (name = "Timer", description = TIMER_DOC),
        (name = "Boards", description = "Boards"),
        (name = "Templates", description = "Board templates"),
        (name = "Colors", description = "Per-board task colors"),
        (name = "Columns", description = "Board columns"),
        (name = "Swimlanes", description = "Board swimlanes"),
        (name = "Settings", description = "App settings"),
        (name = "Tokens", description = TOKENS_DOC),
        (name = "Discovery", description = "Agent discovery documents"),
    ),
)]
struct ApiDoc;

#[cfg(test)]
mod agentctl_tests;

#[cfg(test)]
mod openapi;

#[cfg(test)]
mod server;

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, Local, Timelike, Utc};
    use utoipa::OpenApi;

    use super::docs::SKILL_VERSION;
    use super::{
        ApiDoc, COMPLETION_GRACE_SECS, ColorView, RepeatCadence, TaskCardDisplay,
        UpdateColumnInput, UpdateTaskInput, build_board_export_data, build_ical_feed, entry_badge,
        estimate_hours_to_size, format_added, format_due, format_due_card, format_entry_duration,
        format_estimate_hours, format_pomodori, format_relative_day, format_time_kf,
        generate_feed_token, ical_escape, ical_fold, is_overdue, normalize_due_input,
        normalize_estimate_hours, parse_manual_range, parse_repeat_cadence, router,
    };
    use crate::db::{BootstrapToken, NewTimeEntry};
    use crate::models::{
        Actor, ApiTokenRecord, AuthScope, ColumnRow, EntryKind, SECONDS_PER_DAY, TaskRow,
    };
    use crate::{AppState, auth};

    const SECONDS_PER_MINUTE: i64 = 60;

    fn test_actor() -> Actor<'static> {
        Actor {
            name: "a",
            token_name: None,
        }
    }

    /// A grouping date outside the current year carries its year.
    #[test]
    fn done_group_label_should_add_the_year_outside_the_current_year() {
        use chrono::NaiveDate;

        use super::done_group_label;

        let today = NaiveDate::from_ymd_opt(2026, 10, 17).expect("date");
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).expect("date");
        assert_eq!(done_group_label(day(2026, 10, 17), today), "Today");
        assert_eq!(done_group_label(day(2026, 10, 16), today), "Yesterday");
        assert_eq!(
            done_group_label(day(2026, 10, 14), today),
            "Wednesday, 14 October"
        );
        assert_eq!(
            done_group_label(day(2025, 10, 16), today),
            "Thursday, 16 October 2025"
        );
        assert_eq!(
            done_group_label(day(2027, 1, 2), today),
            "Saturday, 2 January 2027"
        );
    }

    /// OpenAPI text reads as API prose, not Rust.
    #[test]
    fn openapi_text_should_not_show_rust_syntax() {
        fn walk(value: &serde_json::Value, path: &str, hits: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        let here = format!("{path}/{key}");
                        if let (true, Some(text)) =
                            (key == "description" || key == "summary", child.as_str())
                        {
                            let rusty = ["Some(", "None", "Option<", "Vec<", "::", "&str", "[`"];
                            if rusty.iter().any(|needle| text.contains(needle)) {
                                hits.push(format!("{here}: {text}"));
                            }
                        }
                        walk(child, &here, hits);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        walk(child, &format!("{path}/{index}"), hits);
                    }
                }
                _ => {}
            }
        }
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("openapi json");
        let mut hits = Vec::new();
        walk(&doc, "", &mut hits);
        assert!(hits.is_empty(), "{}", hits.join("\n"));
    }

    /// The timer docs state the completion grace and the stopwatch case.
    #[test]
    fn timer_docs_should_state_the_grace_and_the_stopwatch_case() {
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("openapi json");
        let stop = doc["paths"]["/api/timer/stop"]["post"]["description"]
            .as_str()
            .expect("stop description")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let grace = format!("{}-second grace", COMPLETION_GRACE_SECS);
        assert!(stop.contains(&grace), "{stop}");
        assert!(stop.contains("A stopwatch has no duration"), "{stop}");
    }

    /// OpenAPI summaries describe the read-only GETs as they are.
    #[test]
    fn openapi_should_describe_read_only_gets_without_stale_text() {
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("openapi json");
        let text = |path: &str| {
            let get = &doc["paths"][path]["get"];
            format!("{} {}", get["summary"], get["description"])
        };
        let colors = text("/api/boards/{id}/colors");
        assert!(colors.contains("Read-only"), "{colors}");
        assert!(!colors.contains("Backfills"), "{colors}");
        let feed = text("/api/boards/{id}/calendar-feed");
        assert!(!feed.contains("below"), "{feed}");
        assert!(
            feed.contains("POST /api/boards/{id}/calendar-feed"),
            "{feed}"
        );
    }

    /// The delete page flags the last board so the UI refuses up front.
    #[test]
    fn board_delete_page_should_flag_only_the_last_board() {
        use askama::Template;

        use super::BoardDeleteTemplate;

        let render = |delete_refusal| {
            BoardDeleteTemplate {
                board_id: "b".into(),
                board_name: "Only".into(),
                task_count: 0,
                delete_refusal,
                boards: Vec::new(),
                username: "audit".into(),
                username_initial: "AU".into(),
            }
            .render()
            .expect("render delete page")
        };
        let last = render(Some(crate::db::LAST_ACTIVE_BOARD));
        assert!(
            last.contains(&format!(
                "data-last-board=\"{}\"",
                crate::db::LAST_ACTIVE_BOARD
            )),
            "last board flagged with the refusal"
        );
        assert!(
            last.contains("id=\"confirm-dialog\""),
            "styled confirm present"
        );
        assert!(
            !render(None).contains("data-last-board"),
            "other boards unflagged"
        );
    }

    /// Three subtasks show in full, and a longer list folds its done ones.
    #[test]
    fn folds_done_subtasks_should_follow_kanbanflow_truncation() {
        use super::folds_done_subtasks;
        use crate::models::Subtask;
        let sub = |done: bool| Subtask {
            id: String::new(),
            name: String::new(),
            done,
            due_at: None,
        };
        assert!(!folds_done_subtasks(&[sub(true), sub(false), sub(false)]));
        assert!(folds_done_subtasks(&[
            sub(true),
            sub(false),
            sub(true),
            sub(false)
        ]));
        assert!(!folds_done_subtasks(&[
            sub(false),
            sub(false),
            sub(false),
            sub(false)
        ]));
    }

    /// The card-property config defaults to KanbanFlow's (all hide except due dates, which defaults
    /// to "Show active due in 7 days").
    #[test]
    fn card_display_defaults_should_match_kanbanflow() {
        let d = TaskCardDisplay::from_config_json("{}");
        assert!(!d.content.description);
        assert!(!d.content.labels);
        assert!(!d.content.subtasks);
        assert!(d.dates.due);
        assert_eq!(d.due_within_days, Some(7));
        assert!(!d.dates.created);
        assert!(!d.dates.added);
    }

    #[test]
    fn card_display_should_parse_show_values() {
        let d = TaskCardDisplay::from_config_json(
            r#"{"prop_description":"show","prop_labels":"show","prop_subtasks":"show","prop_due_dates":"show","prop_created":"show","prop_added":"show"}"#,
        );
        assert!(d.content.description);
        assert!(d.content.labels);
        assert!(d.content.subtasks);
        assert!(d.dates.due);
        assert_eq!(d.due_within_days, None);
        assert!(d.dates.created);
        assert!(d.dates.added);
    }

    #[test]
    fn card_display_hide_due_dates_should_disable_them() {
        let d = TaskCardDisplay::from_config_json(r#"{"prop_due_dates":"hide"}"#);
        assert!(!d.dates.due);
    }

    /// "active_7d" shows overdue and near-future dues and hides far-future ones, "show" shows
    /// everything, and garbage is None.
    #[test]
    fn format_due_should_honor_seven_day_window() {
        // Local noon, so the day is Jan 01 in every host zone.
        let past = local_at(2020, 1, 1, 12);
        assert_eq!(format_due(&past, Some(7)).as_deref(), Some("Jan 01"));
        assert_eq!(format_due(&past, None).as_deref(), Some("Jan 01"));

        let far = local_at(2999, 1, 1, 12);
        assert_eq!(format_due(&far, Some(7)), None);
        assert_eq!(format_due(&far, None).as_deref(), Some("Jan 01"));

        assert_eq!(format_due("not-a-date", Some(7)), None);
    }

    /// `hour`:00 on the given day in the zone the formatters render in (the host's), as RFC3339, so
    /// expectations hold in any host zone.
    fn local_at(year: i32, month: u32, day: u32, hour: u32) -> String {
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day).expect("valid date");
        local_on(date, hour)
    }

    fn local_on(date: chrono::NaiveDate, hour: u32) -> String {
        use chrono::TimeZone as _;
        let at = date.and_hms_opt(hour, 0, 0).expect("valid time");
        Local
            .from_local_datetime(&at)
            .earliest()
            .expect("the hour exists locally")
            .to_rfc3339()
    }

    /// A fixed local "now" for due-text tests: Wednesday 2026-09-30 15:22.
    fn due_test_now() -> DateTime<Local> {
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 9, 30)
            .and_then(|d| d.and_hms_opt(15, 22, 0))
            .expect("now");
        naive
            .and_local_timezone(Local)
            .earliest()
            .expect("local now")
    }

    /// A local wall time `days` from `due_test_now()`'s date, as RFC3339.
    fn due_test_at(days: i64, hour: u32, minute: u32) -> String {
        let day = due_test_now().date_naive() + Duration::days(days);
        let naive = day.and_hms_opt(hour, minute, 0).expect("time");
        naive
            .and_local_timezone(Local)
            .earliest()
            .expect("local time")
            .to_rfc3339()
    }

    /// Card due text is "Today", a weekday only for later dates this week, and "D Month" otherwise
    /// (past dates included), matching KanbanFlow's 2026-09-30 captures.
    #[test]
    fn format_due_card_should_render_today_weekday_and_absolute() {
        use super::format_due_card_at;

        let now = due_test_now();
        let card = |days, hour, minute| format_due_card_at(&due_test_at(days, hour, minute), now);
        assert_eq!(card(0, 17, 0).as_deref(), Some("Today 5:00 PM"));
        assert_eq!(card(0, 9, 0).as_deref(), Some("Today 9:00 AM"));
        assert_eq!(card(2, 17, 0).as_deref(), Some("Friday 5:00 PM"));
        assert_eq!(card(6, 17, 0).as_deref(), Some("Tuesday 5:00 PM"));
        assert_eq!(card(7, 17, 0).as_deref(), Some("7 October 5:00 PM"));
        assert_eq!(card(-5, 17, 0).as_deref(), Some("25 September 5:00 PM"));
        assert_eq!(card(-1, 23, 0).as_deref(), Some("29 September 11:00 PM"));
        assert_eq!(card(30, 17, 0).as_deref(), Some("30 October 5:00 PM"));
        assert_eq!(format_due_card_at("not-a-date", now), None);
    }

    /// Modal due text matches the card for today and this week, reads "Oct 2 at 11:00 PM"
    /// otherwise, and ends with the target column name in parentheses.
    #[test]
    fn format_due_modal_should_render_relative_and_target_column() {
        use super::format_due_modal_at;

        let now = due_test_now();
        let modal =
            |days, hour, suffix| format_due_modal_at(&due_test_at(days, hour, 0), suffix, now);
        assert_eq!(modal(0, 17, None).as_deref(), Some("Today 5:00 PM"));
        assert_eq!(
            modal(2, 17, Some("Done")).as_deref(),
            Some("Friday 5:00 PM (Done)")
        );
        assert_eq!(
            modal(2, 23, Some("Done")).as_deref(),
            Some("Friday 11:00 PM (Done)")
        );
        assert_eq!(modal(-16, 9, None).as_deref(), Some("Sep 14 at 9:00 AM"));
        assert_eq!(
            modal(14, 9, Some("Review")).as_deref(),
            Some("Oct 14 at 9:00 AM (Review)")
        );
        assert_eq!(format_due_modal_at("not-a-date", Some("Done"), now), None);
    }

    /// A pomodoro retired after reaching its duration counts as completed, task-less work sessions
    /// are logged, and only completed pomodori feed the long-break cycle.
    #[test]
    fn log_timer_session_should_complete_overtime_and_log_taskless_work() {
        use crate::models::{ActiveTimer, TimerMode};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("timer.redb");
        let db = crate::db::Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let board = db.create_board("Board").expect("board");
        let col = db.create_column(&board, "Todo", None).expect("column");
        let task = db.create_task(&col, None, "Work", 1, None).expect("task");
        let now = chrono::Utc::now().timestamp();
        let pomodoro = |task_id: Option<&str>, elapsed: i64| ActiveTimer {
            task_id: task_id.map(str::to_string),
            mode: TimerMode::Pomodoro,
            started_at: now - elapsed,
            duration_secs: Some(60),
        };

        super::log_timer_session(
            &db,
            &pomodoro(Some(&task), 90),
            false,
            Some("Switched task"),
            None,
            test_actor(),
        )
        .ok()
        .flatten()
        .expect("logged overtime pomodoro");
        super::log_timer_session(
            &db,
            &pomodoro(None, 30),
            false,
            Some("Email"),
            None,
            test_actor(),
        )
        .ok()
        .flatten()
        .expect("logged task-less pomodoro");

        // An early "completed" claim does not complete a pomodoro.
        let early = super::log_timer_session(
            &db,
            &pomodoro(Some(&task), 40),
            true,
            None,
            None,
            test_actor(),
        )
        .ok()
        .flatten()
        .expect("logged early stop");
        assert!(!early.1, "40 s of a 60 s pomodoro is not completed");

        let entries = db.all_entries().expect("entries");
        let overtime = entries
            .iter()
            .find(|e| e.task_id == task && e.seconds >= 90)
            .expect("overtime entry");
        assert!(!overtime.interrupted);
        assert_eq!(overtime.interrupt_reason, None);
        let taskless = entries
            .iter()
            .find(|e| e.task_id.is_empty())
            .expect("task-less entry");
        assert!(taskless.interrupted);
        let started = DateTime::parse_from_rfc3339(&overtime.started_at).expect("rfc3339");
        assert_eq!(started.timestamp(), now - 90);
    }

    /// Entries, the TODAY list and the long-break cycle all bucket by the server-local day a
    /// session started, so a pomodoro that crosses midnight counts on the day it began.
    #[test]
    fn timer_status_should_count_a_pomodoro_on_the_day_it_started() {
        use crate::db::NewTimeEntry;
        use crate::models::EntryKind;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("timer.redb");
        let db = crate::db::Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let midnight = Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_local_timezone(Local)
            .unwrap();
        let log = |started_at: DateTime<Local>, seconds: i64| {
            let started_at = started_at.to_rfc3339();
            db.create_entry_full(NewTimeEntry {
                task_id: None,
                started_at: &started_at,
                minutes: seconds / 60,
                seconds,
                note: "",
                kind: EntryKind::Pomodoro,
                interrupted: false,
                interrupt_reason: None,
                labels: &[],
                created_by: "a",
                activity_id: None,
            })
            .expect("entry");
        };
        let count = || {
            let Ok(status) = super::timer_status_view(&db, None) else {
                panic!("status");
            };
            status.pomodoro_count
        };

        log(midnight - chrono::Duration::seconds(60), 1500);
        assert_eq!(count(), 0, "began yesterday, ended today");
        log(midnight, 1500);
        assert_eq!(count(), 1, "began today");
    }

    /// A 7:30 PM Chicago entry (00:30 UTC next day) belongs to the Chicago day, whichever offset
    /// the row was stored with.
    #[test]
    fn day_in_should_bucket_by_the_given_zone() {
        let chicago = chrono::FixedOffset::west_opt(5 * 3600).expect("offset");
        let utc = chrono::FixedOffset::east_opt(0).expect("offset");
        let day = |s: &str, tz: &chrono::FixedOffset| crate::db::day_in(s, tz);
        assert_eq!(
            day("2026-10-17T00:30:00+00:00", &chicago).as_deref(),
            Some("2026-10-16")
        );
        assert_eq!(
            day("2026-10-17T00:30:00+00:00", &utc).as_deref(),
            Some("2026-10-17")
        );
        assert_eq!(
            day("2026-10-16T19:30:00-05:00", &chicago).as_deref(),
            Some("2026-10-16")
        );
        assert_eq!(day("not a date", &utc), None);
    }

    /// Singular, plural, and hidden at zero.
    #[test]
    fn format_pomodori_should_hide_zero_and_pluralize() {
        assert_eq!(format_pomodori(0), None);
        assert_eq!(format_pomodori(1).as_deref(), Some("1 Pomodoro"));
        assert_eq!(format_pomodori(2).as_deref(), Some("2 Pomodori"));
    }

    /// Only a stopped pomodoro names its reason, and stopwatch entries never read as a pomodoro.
    #[test]
    fn entry_badge_should_name_the_reason_only_for_stopped_pomodori() {
        assert_eq!(
            entry_badge("pomodoro", false, None),
            ("P", "Pomodoro".to_string())
        );
        assert_eq!(
            entry_badge("pomodoro", true, Some("Email")),
            ("P", "Stopped with reason \"Email\"".to_string())
        );
        assert_eq!(
            entry_badge("stopwatch", true, Some("Other")),
            ("S", "Stopwatch".to_string())
        );
        assert_eq!(
            entry_badge("manual", false, None),
            ("M", "Manually added".to_string())
        );
    }

    /// Break entries carry their own badge, never "Manually added".
    #[test]
    fn entry_badge_should_name_breaks() {
        assert_eq!(
            entry_badge("short_break", false, None),
            ("B", "Short break".to_string())
        );
        assert_eq!(
            entry_badge("long_break", false, None),
            ("B", "Long break".to_string())
        );
    }

    /// A timer left running for two days logs at most an hour past its duration, or 12 hours for a
    /// stopwatch, and still completes.
    #[test]
    fn log_timer_session_should_cap_a_session_left_running_for_days() {
        use crate::models::{ActiveTimer, TimerMode};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("timer.redb");
        let db = crate::db::Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let board = db.create_board("Board").expect("board");
        let col = db.create_column(&board, "Todo", None).expect("column");
        let task = db.create_task(&col, None, "Work", 1, None).expect("task");
        let two_days_ago = chrono::Utc::now().timestamp() - 2 * 24 * 3600;
        let timer = |mode, duration_secs| ActiveTimer {
            task_id: Some(task.clone()),
            mode,
            started_at: two_days_ago,
            duration_secs,
        };

        let pomodoro = super::log_timer_session(
            &db,
            &timer(TimerMode::Pomodoro, Some(1500)),
            true,
            None,
            None,
            test_actor(),
        )
        .ok()
        .flatten()
        .expect("logged pomodoro");
        assert_eq!(pomodoro, (85, true));
        let stopwatch = super::log_timer_session(
            &db,
            &timer(TimerMode::Stopwatch, None),
            true,
            None,
            None,
            test_actor(),
        )
        .ok()
        .flatten()
        .expect("logged stopwatch");
        assert_eq!(stopwatch, (720, true));

        let mut seconds: Vec<i64> = db
            .all_entries()
            .expect("entries")
            .iter()
            .map(|e| e.seconds)
            .collect();
        seconds.sort_unstable();
        assert_eq!(seconds, vec![1500 + 3600, 12 * 3600]);
    }

    /// An edited range keeps its seconds.
    #[test]
    fn parse_manual_range_should_keep_seconds() {
        let seconds = |from, to| {
            parse_manual_range("2026-10-16", from, to)
                .map(|(_, s)| s)
                .unwrap_or(-1)
        };
        assert_eq!(seconds("16:06:05", "16:06:29"), 24);
        assert_eq!(seconds("08:00", "08:20"), 1200);
    }

    /// "{spent} / {estimate}", spent-only, or None.
    #[test]
    fn format_time_kf_should_render_spent_and_estimate() {
        assert_eq!(
            format_time_kf(150, Some(8.0)).as_deref(),
            Some("2h 30m / 8h")
        );
        assert_eq!(format_time_kf(0, Some(4.0)).as_deref(), Some("0h / 4h"));
        assert_eq!(format_time_kf(45, None).as_deref(), Some("45m"));
        assert_eq!(format_time_kf(0, None), None);
    }

    /// `column_added_at` falls back to the creation date for rows written before the field existed.
    #[test]
    fn format_added_should_fall_back_to_created_at() {
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
            created_at: local_at(2026, 9, 20, 10),
            completed_at: None,
            due_at: None,
            due_repeat: None,
            due_done: false,
            due_target_column_id: None,
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
            updated_at: None,
            metadata: None,
            links: Vec::new(),
            claim: None,
            claim_fence: 0,
            fields: serde_json::Map::new(),
        };
        assert_eq!(format_added(&row), "Sep 20");

        let row2 = TaskRow {
            column_added_at: Some(local_at(2026, 9, 29, 10)),
            ..row
        };
        assert_eq!(format_added(&row2), "Sep 29");
    }

    /// The generated OpenAPI spec must expose every data-layer endpoint.
    #[test]
    fn openapi_should_include_color_template_and_board_paths() {
        let spec = ApiDoc::openapi();
        let json = spec.to_json().expect("spec serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let paths = value
            .get("paths")
            .and_then(|paths| paths.as_object())
            .expect("paths object");
        for path in [
            "/api/boards",
            "/api/boards/{id}/save-as-template",
            "/api/templates",
            "/api/templates/{id}",
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
    fn openapi_should_include_task_extras_paths() {
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
            "/api/time/entries/{id}",
        ] {
            assert!(paths.contains_key(path), "openapi missing path {path}");
        }

        // Spot-check methods.
        assert!(paths["/api/boards/{id}/labels"].get("get").is_some());
        assert!(paths["/api/tasks/{id}/comments"].get("post").is_some());

        // Comments list endpoint must appear alongside POST.
        assert!(paths["/api/tasks/{id}/comments"].get("get").is_some());
        assert!(
            paths["/api/tasks/{id}/comments/{comment_id}"]
                .get("delete")
                .is_some()
        );
        assert!(paths["/api/tasks/{id}/attachments"].get("post").is_some());
        assert!(
            paths["/api/tasks/{id}/attachments/{attachment_id}/file"]
                .get("get")
                .is_some()
        );
        assert!(
            paths["/api/tasks/{id}/attachments/{attachment_id}"]
                .get("delete")
                .is_some()
        );

        // The timer settings alias
        assert!(paths["/api/timer/settings"].get("get").is_some());
        let entry = &paths["/api/time/entries/{id}"];
        assert!(entry.get("put").is_some());
        assert!(entry.get("delete").is_some());

        // POST /api/tasks advertises only the 201 JSON create response
        let create_task = &paths["/api/tasks"];
        let post = create_task.get("post").expect("POST /api/tasks in spec");
        let responses = post.get("responses").expect("responses");
        assert!(responses.get("201").is_some());
        assert!(responses.get("200").is_none());

        // UI page and fragment routes stay out of the spec
        for path in [
            "/api/tasks/{id}/card",
            "/boards/new",
            "/b/{board_id}/settings",
        ] {
            assert!(!paths.contains_key(path), "{path} is a UI route");
        }
    }

    /// `GET /api/v1/version` is public and reports the crate version plus
    /// the build SHA (or "unknown" when the build didn't bake one in).
    #[tokio::test]
    async fn version_endpoint_should_report_build_identity() {
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::connect(
            dir.path()
                .join("version-test.redb")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("connect");
        let app = super::router(crate::AppState::new(db, None));

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
        assert_eq!(value["skill_version"], SKILL_VERSION.to_string());
    }

    /// Shared fixture for the handler regression tests: a fresh temp-DB app, an admin user with a
    /// session cookie, and a board with a WIP-limited column plus a Done column.
    struct Kf221Fixture {
        db: crate::db::Db,
        app: axum::Router,
        cookie: String,
        board_id: String,
        col_wip: String,
        col_done: String,
    }

    impl Kf221Fixture {
        /// Returns the stored completion time of the task.
        fn completed_at(&self, task_id: &str) -> Option<String> {
            self.db
                .get_task(task_id)
                .expect("get")
                .expect("task")
                .completed_at
        }
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
        let app = super::router(
            crate::AppState::new(db.clone(), None).with_limits(super::server::unthrottled_limits()),
        );
        let fixture = Kf221Fixture {
            db,
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

    /// The board columns list exposes `wip_limit` and `is_done`.
    #[tokio::test]
    async fn columns_list_should_include_wip_limit_and_is_done() {
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

    /// `Accept: application/json` on POST /api/tasks returns a 201 JSON body with the created
    /// task's id.
    #[tokio::test]
    async fn create_task_json_accept_should_return_201_with_id() {
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

    /// The default POST /api/tasks response stays the HTML card fragment the htmx UI appends.
    #[tokio::test]
    async fn create_task_default_should_stay_html_fragment() {
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

    /// Comments round-trip as JSON, and the `author` field overrides the default username
    /// attribution.
    #[tokio::test]
    async fn comments_json_should_list_and_override_author() {
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

        // GET returns the JSON comment list, oldest first.
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

    /// The login Set-Cookie carries `Secure` only when the request arrived over HTTPS, so
    /// plain-HTTP logins keep a cookie the browser will actually store.
    #[tokio::test]
    async fn login_cookie_should_be_secure_only_over_https() {
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

    /// A cookie write the browser marks as cross-origin (another site, or the same site on another
    /// port or subdomain) is refused, while same-origin, header-less, and Bearer writes go through.
    #[tokio::test]
    async fn cross_origin_cookie_writes_should_be_refused() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        let mint = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/auth/tokens")
            .header("cookie", &fx.cookie)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"name":"kf420","scopes":["admin"]}"#,
            ))
            .expect("request");
        let minted = fx.app.clone().oneshot(mint).await.expect("oneshot");
        assert_eq!(minted.status(), axum::http::StatusCode::OK);
        let bearer = json_body(minted).await["raw_token"]
            .as_str()
            .expect("raw_token")
            .to_string();
        let rename = |credential: (&str, String), extra: &[(&str, &str)]| {
            let mut builder = axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/columns/{}", fx.col_wip))
                .header("host", "127.0.0.1:3357")
                .header("content-type", "application/json")
                .header(credential.0, credential.1);
            for (name, value) in extra {
                builder = builder.header(*name, *value);
            }
            builder
                .body(axum::body::Body::from(r#"{"name":"Renamed"}"#))
                .expect("request")
        };
        let cookie = || ("cookie", fx.cookie.clone());
        let send = |request| async { fx.app.clone().oneshot(request).await.expect("oneshot") };

        let allowed: [&[(&str, &str)]; 4] = [
            &[],
            &[
                ("sec-fetch-site", "same-origin"),
                ("origin", "http://127.0.0.1:3357"),
            ],
            &[("origin", "http://127.0.0.1:3357")],
            &[("sec-fetch-site", "none")],
        ];
        for extra in allowed {
            let response = send(rename(cookie(), extra)).await;
            assert_eq!(response.status(), axum::http::StatusCode::OK, "{extra:?}");
        }

        let refused: [&[(&str, &str)]; 6] = [
            &[
                ("sec-fetch-site", "cross-site"),
                ("origin", "https://evil.example"),
            ],
            &[("origin", "https://evil.example")],
            &[
                ("sec-fetch-site", "same-site"),
                ("origin", "http://127.0.0.1:3999"),
            ],
            &[("origin", "http://127.0.0.1:3999")],
            &[("origin", "http://wallabag.127.0.0.1:3357")],
            &[("origin", "null")],
        ];
        for extra in refused {
            let response = send(rename(cookie(), extra)).await;
            assert_eq!(
                response.status(),
                axum::http::StatusCode::FORBIDDEN,
                "{extra:?}"
            );
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .expect("body");
            assert_eq!(&body[..], crate::csrf::REFUSED_BODY.as_bytes());
        }

        // Bearer clients are not cookie-based: browser headers are ignored.
        let response = send(rename(
            ("authorization", format!("Bearer {bearer}")),
            &[("sec-fetch-site", "cross-site"), ("origin", "null")],
        ))
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // The login form gets the same check (login CSRF).
        let login = |extra: &[(&str, &str)]| {
            let mut builder = axum::http::Request::builder()
                .method("POST")
                .uri("/login")
                .header("host", "127.0.0.1:3357")
                .header("content-type", "application/x-www-form-urlencoded");
            for (name, value) in extra {
                builder = builder.header(*name, *value);
            }
            builder
                .body(axum::body::Body::from("user=admin&pass=s3cret"))
                .expect("request")
        };
        let response = send(login(&[("origin", "http://127.0.0.1:3999")])).await;
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        let response = send(login(&[("sec-fetch-site", "same-origin")])).await;
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    }

    /// An app over a database file the test can byte-compare, with an admin session cookie and one
    /// board.
    struct Kf420Fixture {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        db: crate::db::Db,
        app: axum::Router,
        cookie: String,
        board_id: String,
    }

    fn kf420_fixture() -> Kf420Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kf420-test.redb");
        let db = crate::db::Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let user_id = db
            .create_user("admin", &crate::auth::hash_password("pw").expect("hash"))
            .expect("create user");
        let cookie = format!(
            "session={}",
            crate::auth::create_session(&db, &user_id).expect("session")
        );
        let board_id = db.create_board("KF-420").expect("board");
        let app = super::router(crate::AppState::new(db.clone(), None));
        Kf420Fixture {
            _dir: dir,
            path,
            db,
            app,
            cookie,
            board_id,
        }
    }

    impl Kf420Fixture {
        async fn send(&self, method: &str, uri: &str, site: &str) -> axum::response::Response {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:3357")
                .header("cookie", &self.cookie)
                .header("sec-fetch-site", site)
                .header("accept", "application/json")
                .body(axum::body::Body::empty())
                .expect("request");
            self.app.clone().oneshot(request).await.expect("oneshot")
        }

        fn file(&self) -> Vec<u8> {
            std::fs::read(&self.path).expect("read db file")
        }

        async fn post_json(&self, uri: &str, body: &str) -> axum::response::Response {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("host", "127.0.0.1:3357")
                .header("cookie", &self.cookie)
                .header("sec-fetch-site", "same-origin")
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request");
            self.app.clone().oneshot(request).await.expect("oneshot")
        }
    }

    /// A stop naming only a `session_id`, with no active timer, is a 200 `already_stopped` instead
    /// of a 404.
    #[tokio::test]
    async fn timer_stop_should_accept_a_stale_session_id_without_an_active_timer() {
        let fx = kf420_fixture();
        let response = fx.post_json("/api/timer/stop", r#"{"session_id":7}"#).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(json_body(response).await["already_stopped"], true);
    }

    /// A stop naming a session that is no longer active succeeds without touching the timer, and
    /// without `started_at` it stays a 404.
    #[tokio::test]
    async fn timer_stop_should_treat_an_already_stopped_session_as_success() {
        use crate::models::{ActiveTimer, TimerMode};

        let fx = kf420_fixture();
        let response = fx.post_json("/api/timer/stop", "{}").await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        let response = fx
            .post_json("/api/timer/stop", r#"{"started_at":100}"#)
            .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(json_body(response).await["already_stopped"], true);

        let started_at = chrono::Utc::now().timestamp() - 30;
        let timer = ActiveTimer {
            task_id: None,
            mode: TimerMode::Stopwatch,
            started_at,
            duration_secs: None,
        };
        fx.db.set_active_timer(&timer).expect("timer");
        let stale = format!(r#"{{"started_at":{}}}"#, started_at - 600);
        let response = fx.post_json("/api/timer/stop", &stale).await;
        assert_eq!(json_body(response).await["already_stopped"], true);
        assert!(fx.db.get_active_timer().expect("timer").is_some());

        let current = format!(r#"{{"started_at":{started_at}}}"#);
        let response = fx.post_json("/api/timer/stop", &current).await;
        let body = json_body(response).await;
        assert_eq!(body["already_stopped"], false);
        assert_eq!(body["discarded"], false);
        assert!(fx.db.get_active_timer().expect("timer").is_none());
    }

    /// A log entry's time range joins its two times with a hyphen.
    #[tokio::test]
    async fn timer_log_should_join_time_ranges_with_a_hyphen() {
        use crate::models::{ActiveTimer, TimerMode};

        let fx = kf420_fixture();
        let timer = ActiveTimer {
            task_id: None,
            mode: TimerMode::Stopwatch,
            started_at: chrono::Utc::now().timestamp() - 90,
            duration_secs: None,
        };
        fx.db.set_active_timer(&timer).expect("timer");
        fx.post_json("/api/timer/stop", "{}").await;
        let response = fx.send("GET", "/api/timer/log", "same-origin").await;
        let body = json_body(response).await;
        let range = body["entries"][0]["time_range"].as_str().expect("range");
        let parts: Vec<&str> = range.split(" - ").collect();
        assert_eq!(parts.len(), 2, "{range}");
        assert!(range.chars().all(|c| c != '\u{2013}'), "{range}");
    }

    /// The standalone timer log page opens on "This + Last week".
    #[tokio::test]
    async fn timer_log_page_should_default_to_this_plus_last_week() {
        let fx = kf420_fixture();
        let response = fx.send("GET", "/timer/log", "same-origin").await;
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            html.contains(
                r#"<option value="this-plus-last-week" selected>This + Last week</option>"#
            ),
            "default period option missing"
        );
        assert!(!html.contains(r#"value="last-30" selected"#));
    }

    /// Reading the calendar feed link never writes, even cross-site, and only a same-origin POST
    /// creates the token.
    #[tokio::test]
    async fn calendar_feed_get_should_never_write() {
        let fx = kf420_fixture();
        let uri = format!("/api/boards/{}/calendar-feed", fx.board_id);
        let before = fx.file();
        for method in ["GET", "HEAD"] {
            let response = fx.send(method, &uri, "cross-site").await;
            assert_eq!(response.status(), axum::http::StatusCode::OK, "{method}");
        }
        let response = fx.send("GET", &uri, "same-origin").await;
        assert_eq!(json_body(response).await["path"], serde_json::Value::Null);
        let response = fx.send("POST", &uri, "cross-site").await;
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        assert!(fx.file() == before, "no request so far may write");
        let board = fx.db.get_board(&fx.board_id).expect("get").expect("board");
        assert!(board.feed_token().is_none());

        let response = fx.send("POST", &uri, "same-origin").await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let created = json_body(response).await["path"].clone();
        assert!(created.as_str().is_some_and(|path| path.ends_with(".ics")));
        let response = fx.send("POST", &uri, "same-origin").await;
        assert_eq!(json_body(response).await["path"], created, "POST reuses it");
        let after = fx.file();
        let response = fx.send("GET", &uri, "cross-site").await;
        assert_eq!(json_body(response).await["path"], created);
        assert!(fx.file() == after, "GET wrote to the database");
    }

    /// 12 concurrent first POSTs for a board's feed link all hand out the one token that is stored,
    /// and that feed serves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_feed_links_should_share_one_token() {
        let fx = std::sync::Arc::new(kf420_fixture());
        let uri = format!("/api/boards/{}/calendar-feed", fx.board_id);
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(12));
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let (fx, uri) = (std::sync::Arc::clone(&fx), uri.clone());
                let barrier = std::sync::Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    let response = fx.send("POST", &uri, "same-origin").await;
                    assert_eq!(response.status(), axum::http::StatusCode::OK);
                    json_body(response).await["path"]
                        .as_str()
                        .expect("path")
                        .to_string()
                })
            })
            .collect();
        let mut paths = std::collections::HashSet::new();
        for handle in handles {
            paths.insert(handle.await.expect("request task"));
        }
        assert_eq!(paths.len(), 1, "{paths:?}");
        let path = paths.into_iter().next().expect("one path");
        let response = fx.send("GET", &path, "cross-site").await;
        assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
    }

    /// Deleting a task (or its board) stops a timer running on it, and a status read never writes,
    /// even for a timer whose task is gone.
    #[tokio::test]
    async fn timer_should_stop_with_its_task_and_status_reads_should_never_write() {
        use crate::models::{ActiveTimer, TimerMode};

        let fx = kf420_fixture();
        let timer_on = |task_id: &str| ActiveTimer {
            task_id: Some(task_id.to_string()),
            mode: TimerMode::Pomodoro,
            started_at: chrono::Utc::now().timestamp(),
            duration_secs: Some(1500),
        };
        let column = fx
            .db
            .create_column(&fx.board_id, "A", None)
            .expect("column");
        let task = fx
            .db
            .create_task(&column, None, "t", 1, None)
            .expect("task");
        fx.db.set_active_timer(&timer_on(&task)).expect("timer");

        let before = fx.file();
        let response = fx.send("GET", "/api/timer/status", "cross-site").await;
        assert_eq!(json_body(response).await["active"], true);
        assert!(fx.file() == before, "status read wrote to the database");
        assert!(fx.db.delete_task(&task).expect("delete task"));
        assert!(fx.db.get_active_timer().expect("timer").is_none());

        fx.db.set_active_timer(&timer_on("gone")).expect("timer");
        let before = fx.file();
        let response = fx.send("GET", "/api/timer/status", "same-origin").await;
        assert_eq!(json_body(response).await["active"], false);
        assert!(fx.file() == before, "status read wrote to the database");
        assert!(fx.db.get_active_timer().expect("timer").is_some());

        let other = fx.db.create_board("Other").expect("board");
        let other_column = fx.db.create_column(&other, "B", None).expect("column");
        let other_task = fx
            .db
            .create_task(&other_column, None, "u", 1, None)
            .expect("task");
        fx.db
            .set_active_timer(&timer_on(&other_task))
            .expect("timer");
        fx.db.delete_board(&other).expect("delete board");
        assert!(fx.db.get_active_timer().expect("timer").is_none());
    }

    /// Stopping a timer whose task is gone logs the session task-less instead of reporting minutes
    /// it never logged.
    #[tokio::test]
    async fn stopping_an_orphaned_timer_should_log_the_session_task_less() {
        use crate::models::{ActiveTimer, TimerMode};

        let fx = kf420_fixture();
        fx.db
            .set_active_timer(&ActiveTimer {
                task_id: Some("gone".to_string()),
                mode: TimerMode::Pomodoro,
                started_at: chrono::Utc::now().timestamp() - 360,
                duration_secs: Some(1500),
            })
            .expect("timer");
        let response = fx.send("POST", "/api/timer/stop", "same-origin").await;
        let body = json_body(response).await;
        assert_eq!(body["discarded"], false, "{body}");
        assert_eq!(body["minutes"], 6, "{body}");
        let entries = fx.db.all_entries().expect("entries");
        assert_eq!(entries.len(), 1, "the session must be logged");
        assert_eq!(entries[0].task_id, "");
        assert_eq!(entries[0].minutes, 6);
        assert!(entries[0].interrupted);
    }

    /// A stopwatch entry is never logged as interrupted.
    #[tokio::test]
    async fn stopping_a_stopwatch_should_not_log_an_interruption() {
        use crate::models::{ActiveTimer, TimerMode};

        let fx = kf420_fixture();
        fx.db
            .set_active_timer(&ActiveTimer {
                task_id: None,
                mode: TimerMode::Stopwatch,
                started_at: chrono::Utc::now().timestamp() - 360,
                duration_secs: None,
            })
            .expect("timer");
        let response = fx.send("POST", "/api/timer/stop", "same-origin").await;
        let body = json_body(response).await;
        assert_eq!(body["minutes"], 6, "{body}");
        let entries = fx.db.all_entries().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "stopwatch");
        assert!(!entries[0].interrupted);
        assert_eq!(entries[0].interrupt_reason, None);
    }

    /// Stopping a break whose task is gone logs a task-less break entry, so the reported minutes
    /// match the log.
    #[tokio::test]
    async fn stopping_an_orphaned_break_should_log_a_task_less_entry() {
        use crate::models::{ActiveTimer, TimerMode};

        let fx = kf420_fixture();
        fx.db
            .set_active_timer(&ActiveTimer {
                task_id: Some("gone".to_string()),
                mode: TimerMode::ShortBreak,
                started_at: chrono::Utc::now().timestamp() - 360,
                duration_secs: Some(300),
            })
            .expect("timer");
        let response = fx.send("POST", "/api/timer/stop", "same-origin").await;
        let body = json_body(response).await;
        assert_eq!(body["discarded"], false, "{body}");
        assert_eq!(body["minutes"], 6, "{body}");
        let entries = fx.db.all_entries().expect("entries");
        assert_eq!(entries.len(), 1, "the break must be logged");
        assert_eq!(entries[0].task_id, "");
        assert_eq!(entries[0].minutes, 6);
        assert_eq!(entries[0].kind, "short_break");
    }

    /// A column PATCH that fails validation writes nothing, and a valid one applies every field.
    #[tokio::test]
    async fn update_column_should_write_nothing_when_any_field_is_invalid() {
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::connect(
            dir.path()
                .join("kf422-test.redb")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("connect");
        let user_id = db
            .create_user("admin", &crate::auth::hash_password("pw").expect("hash"))
            .expect("create user");
        let cookie = format!(
            "session={}",
            crate::auth::create_session(&db, &user_id).expect("session")
        );
        let board_id = db.create_board("KF-422").expect("board");
        let column_id = db.create_column(&board_id, "Wip", Some(3)).expect("column");
        let app = super::router(crate::AppState::new(db.clone(), None));
        let stored =
            || serde_json::to_value(db.get_column(&column_id).expect("get")).expect("json");
        let patch = |body: &'static str| {
            axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/columns/{column_id}"))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .expect("request")
        };

        let before = stored();
        for body in [
            r#"{"name":"RENAMED","is_done":true,"wip_limit":0}"#,
            r#"{"is_done":true,"config_json":"nope"}"#,
            r#"{"description":"changed","name":"   "}"#,
        ] {
            let response = app.clone().oneshot(patch(body)).await.expect("oneshot");
            assert_eq!(
                response.status(),
                axum::http::StatusCode::BAD_REQUEST,
                "{body}"
            );
            assert_eq!(stored(), before, "{body} must not write");
        }

        let response = app
            .clone()
            .oneshot(patch(
                r#"{"name":" Doing ","wip_limit":null,"is_done":true,"description":" d ","collapsed":true,"config_json":"{\"a\":1}"}"#,
            ))
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let after = db.get_column(&column_id).expect("get").expect("column");
        assert_eq!(after.name, "Doing");
        assert_eq!(after.wip_limit, None);
        assert!(after.is_done);
        assert_eq!(after.description, "d");
        assert!(after.collapsed);
        assert_eq!(after.config_json, r#"{"a":1}"#);
    }

    /// Hour-based estimates validate: 0 clears, and negatives and non-finite values are rejected.
    #[test]
    fn normalize_estimate_hours_should_accept_positive_and_clear_on_zero() {
        assert_eq!(normalize_estimate_hours(4.0).unwrap(), Some(4.0));
        assert_eq!(normalize_estimate_hours(0.5).unwrap(), Some(0.5));
        assert_eq!(normalize_estimate_hours(0.0).unwrap(), None);
        assert!(normalize_estimate_hours(-1.0).is_err());
        assert!(normalize_estimate_hours(f64::NAN).is_err());
        assert!(normalize_estimate_hours(f64::INFINITY).is_err());
    }

    /// The pomodoro size derives from hours via the configured pomodoro length, clamped to 1..=4 so
    /// the legend keeps working.
    #[test]
    fn estimate_hours_to_size_should_derive_pomodori() {
        assert_eq!(estimate_hours_to_size(4.0, 25), 4); // 9.6 rounds/clamps to 4
        assert_eq!(estimate_hours_to_size(2.0, 25), 4); // 4.8 -> 5 -> clamp 4
        assert_eq!(estimate_hours_to_size(1.0, 25), 2); // 2.4 -> 2
        assert_eq!(estimate_hours_to_size(0.5, 25), 1); // 1.2 -> 1
        assert_eq!(estimate_hours_to_size(0.1, 25), 1); // never 0
        assert_eq!(estimate_hours_to_size(1.0, 60), 1); // honors pomodoro length
    }

    /// Entry durations read "31s", "43m" and "2h 30m".
    #[test]
    fn format_entry_duration_should_use_hours_from_an_hour_up() {
        assert_eq!(format_entry_duration(31, 0), "31s");
        assert_eq!(format_entry_duration(2580, 43), "43m");
        assert_eq!(format_entry_duration(3600, 60), "1h");
        assert_eq!(format_entry_duration(9000, 150), "2h 30m");
    }

    /// Estimate labels render KanbanFlow-style.
    #[test]
    fn format_estimate_hours_should_render_compact_labels() {
        assert_eq!(format_estimate_hours(4.0), "4h");
        assert_eq!(format_estimate_hours(1.5), "1h 30m");
        assert_eq!(format_estimate_hours(0.5), "30m");
        assert_eq!(format_estimate_hours(2.25), "2h 15m");
    }

    /// The modal Time spent cell renders the plain duration label ("Nh Nm", space-separated): "2h
    /// 30m", "43m", "0m" for zero.
    #[test]
    fn time_spent_label_should_render_kf258_plain_duration() {
        let label = |mins: i64| format_estimate_hours(mins as f64 / 60.0);
        assert_eq!(label(150), "2h 30m");
        assert_eq!(label(43), "43m");
        assert_eq!(label(0), "0m");
        assert_eq!(label(60), "1h");
        assert_eq!(label(120), "2h");
    }

    /// A bare date is due at the board's Due date time that day (KanbanFlow's 5:00 PM by default,
    /// or whatever the board sets).
    #[test]
    fn normalize_due_input_date_only_should_take_the_due_time() {
        let due_time = crate::models::default_due_time();
        let norm = normalize_due_input("2026-09-30", due_time).expect("valid");
        let dt = DateTime::parse_from_rfc3339(&norm)
            .expect("normalizes to RFC3339")
            .with_timezone(&Local);
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2026-09-30");
        assert_eq!((dt.hour(), dt.minute(), dt.second()), (17, 0, 0));
        let morning = chrono::NaiveTime::from_hms_opt(8, 30, 0).expect("time");
        let norm = normalize_due_input("2026-09-30", morning).expect("valid");
        let dt = DateTime::parse_from_rfc3339(&norm)
            .expect("RFC3339")
            .with_timezone(&Local);
        assert_eq!((dt.hour(), dt.minute(), dt.second()), (8, 30, 0));

        // Full timestamps pass through untouched.
        assert_eq!(
            normalize_due_input("2026-09-30T17:00:00+00:00", due_time).as_deref(),
            Ok("2026-09-30T17:00:00+00:00")
        );
        assert!(
            normalize_due_input("2026-09-30 17:00", due_time)
                .expect("valid")
                .contains('T')
        );
    }

    /// A bare local datetime (no UTC offset), with or without seconds, normalizes to RFC3339
    /// server-local time instead of being stored verbatim (which `format_due_card` cannot parse).
    #[test]
    fn normalize_due_input_should_accept_bare_local_datetime_with_seconds() {
        for raw in ["2026-09-30T17:00:00", "2026-09-30 17:00:00"] {
            let norm = normalize_due_input(raw, crate::models::default_due_time()).expect("valid");
            let dt = DateTime::parse_from_rfc3339(&norm)
                .expect("normalizes to RFC3339")
                .with_timezone(&Local);
            assert_eq!(dt.format("%Y-%m-%d").to_string(), "2026-09-30");
            assert_eq!((dt.hour(), dt.minute(), dt.second()), (17, 0, 0));
            assert!(
                format_due_card(&norm).is_some(),
                "due renderer parses {raw}"
            );
        }
    }

    /// Explicit JSON null on due_at/due_repeat deserializes to `Some(None)` (clear), distinct from
    /// a missing field (`None`, unchanged). Stock serde collapses both to `None` without the
    /// `de_opt_opt` helper.
    #[test]
    fn due_fields_should_distinguish_null_from_missing() {
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

    /// Explicit JSON null on wip_limit deserializes to `Some(None)` (clear), distinct from a
    /// missing field (`None`, unchanged) and a numeric value (`Some(Some(n))`, set).
    #[test]
    fn wip_limit_should_distinguish_null_from_missing() {
        let input: UpdateColumnInput = serde_json::from_str(r#"{"wip_limit":null}"#).unwrap();
        assert_eq!(input.wip_limit, Some(None));
        let input: UpdateColumnInput = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(input.wip_limit, None);
        let input: UpdateColumnInput = serde_json::from_str(r#"{"wip_limit":5}"#).unwrap();
        assert_eq!(input.wip_limit, Some(Some(5)));
    }

    /// Overdue compares the full due timestamp, never the date. A card due in 2 hours is not
    /// overdue, and a card due yesterday is.
    #[test]
    fn is_overdue_should_compare_full_timestamp_not_date() {
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

        // A due at the end of today is not overdue until the day ends.
        let end_of_day = chrono::NaiveTime::from_hms_opt(23, 59, 59).expect("time");
        let today_end =
            normalize_due_input(&Local::now().format("%Y-%m-%d").to_string(), end_of_day)
                .expect("valid");
        assert!(
            !is_overdue(Some(&today_end), false),
            "due-today (end of day) is not overdue"
        );
    }

    /// The task-modal subline shows "Today" for a task created on the same local calendar day, and
    /// the absolute "%b %d" rendering otherwise.
    #[test]
    fn format_relative_day_should_be_today_for_same_calendar_day() {
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

    /// The filter Color dropdown honors the board's custom color renames, falling back to the fixed
    /// standard palette name ("Purple", …) when a color has no custom label, and never shows a
    /// blank option.
    #[test]
    fn color_display_label_should_honor_rename_with_standard_fallback() {
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

    /// The export dataset assembles the board ref, the lookup tables, and the task rows from the DB
    /// parts.
    #[test]
    fn build_board_export_data_should_assemble_lookups() {
        use crate::models::{BoardRow, ColorRow, ColumnRow, SwimlaneRow, UserRow};
        let board = BoardRow {
            id: "b1".into(),
            name: "Work".into(),
            position: 0,
            config_json: "{}".into(),
        };
        let columns = vec![ColumnRow {
            id: "c1".into(),
            board_id: "b1".into(),
            name: "To-do".into(),
            position: 0,
            wip_limit: None,
            is_done: false,
            description: String::new(),
            collapsed: false,
            config_json: "{}".into(),
            former_names: Vec::new(),
        }];
        let swimlanes = vec![SwimlaneRow {
            id: "s1".into(),
            board_id: "b1".into(),
            name: "Default".into(),
            position: 0,
        }];
        let colors = vec![ColorRow {
            id: "k1".into(),
            board_id: "b1".into(),
            value: "yellow".into(),
            label: "1 Pomodoro".into(),
            description: String::new(),
            background_hex: "#ffffe0".into(),
            border_hex: "#f5cc00".into(),
            light_hex: "#ffffe0".into(),
            enabled: true,
            is_default: true,
            sort_order: 0,
        }];
        let users = vec![UserRow {
            id: "u1".into(),
            username: "chip".into(),
            password_hash: String::new(),
        }];
        let data = build_board_export_data(&board, &columns, &swimlanes, &colors, &users, vec![]);
        assert_eq!(data.board.id, "b1");
        assert_eq!(data.board.name, "Work");
        assert_eq!(data.columns.len(), 1);
        assert_eq!(data.columns[0].name, "To-do");
        assert_eq!(data.swimlanes[0].name, "Default");
        assert_eq!(data.colors[0].value, "yellow");
        assert_eq!(data.colors[0].label, "1 Pomodoro");
        assert_eq!(data.users[0].username, "chip");
        assert!(data.tasks.is_empty());

        // The dataset serializes with the keys the Export page consumes.
        let v = serde_json::to_value(&data).unwrap();
        for key in ["board", "columns", "swimlanes", "colors", "users", "tasks"] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
    }

    /// The feed bearer token is unguessable (256 bits, base64url) and unique per generation.
    #[test]
    fn feed_token_should_be_unguessable_and_unique() {
        let a = generate_feed_token();
        let b = generate_feed_token();
        assert_eq!(a.len(), 43, "32 bytes as base64url");
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    /// Free-text due_repeat parses to the canonical cadences, and anything else means no expansion
    /// (single-date behavior).
    #[test]
    fn repeat_cadence_should_parse_canonical_text() {
        assert_eq!(
            parse_repeat_cadence(Some("every week")),
            Some(RepeatCadence::Weekly)
        );
        assert_eq!(
            parse_repeat_cadence(Some("Every Week")),
            Some(RepeatCadence::Weekly)
        );
        assert_eq!(
            parse_repeat_cadence(Some("  every day ")),
            Some(RepeatCadence::Daily)
        );
        assert_eq!(
            parse_repeat_cadence(Some("monthly")),
            Some(RepeatCadence::Monthly)
        );
        assert_eq!(
            parse_repeat_cadence(Some("annually")),
            Some(RepeatCadence::Yearly)
        );
        assert_eq!(parse_repeat_cadence(Some("every fortnight")), None);
        assert_eq!(parse_repeat_cadence(Some("")), None);
        assert_eq!(parse_repeat_cadence(None), None);
    }

    /// ICalendar TEXT escaping (RFC 5545 §3.3.11).
    #[test]
    fn ical_escape_should_cover_specials() {
        assert_eq!(ical_escape("a;b,c\\d\ne\rf"), "a\\;b\\,c\\\\d\\nef");
        assert_eq!(ical_escape("plain"), "plain");
    }

    /// Content lines fold at 75 octets with CRLF + space continuation, and short lines pass through
    /// unfolded. The 75-octet limit (RFC 5545 §3.1) includes the continuation space, so the
    /// measurement here is on the raw line, not the space-stripped body.
    #[test]
    fn ical_fold_should_wrap_long_lines() {
        assert_eq!(ical_fold("BEGIN:VEVENT"), "BEGIN:VEVENT\r\n");
        let folded = ical_fold(&("SUMMARY:".to_string() + &"x".repeat(100)));
        assert!(folded.ends_with("\r\n"));
        assert!(folded.contains("\r\n "));
        for segment in folded.trim_end_matches("\r\n").split("\r\n") {
            assert!(segment.len() <= 75, "folded segment too long");
        }
    }

    /// A >149-char SUMMARY forces a full continuation segment, and every raw line (continuation
    /// space included) must be <= 75 octets, byte-exact.
    #[test]
    fn ical_fold_continuation_should_stay_within_75_octets() {
        let line = "SUMMARY:".to_string() + &"y".repeat(200);
        let folded = ical_fold(&line);
        for segment in folded.trim_end_matches("\r\n").split("\r\n") {
            assert!(
                segment.len() <= 75,
                "segment {} octets: {segment:?}",
                segment.len()
            );
        }
    }

    fn feed_task(
        id: &str,
        name: &str,
        due_at: Option<&str>,
        repeat: Option<&str>,
        done: bool,
    ) -> TaskRow {
        TaskRow {
            id: id.into(),
            column_id: "c1".into(),
            swimlane_id: None,
            name: name.into(),
            description: String::new(),
            size: 1,
            color_id: None,
            position: 0.0,
            pomodori_completed: 0,
            interruptions: 0,
            total_minutes: 0,
            created_at: "2026-10-01T10:00:00Z".into(),
            completed_at: None,
            due_at: due_at.map(|s| s.into()),
            due_repeat: repeat.map(|s| s.into()),
            due_done: done,
            due_target_column_id: None,
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
            updated_at: None,
            metadata: None,
            links: Vec::new(),
            claim: None,
            claim_fence: 0,
            fields: serde_json::Map::new(),
        }
    }

    /// The feed exposes only tasks with due dates, with minimal fields (no
    /// members/comments/history), stable UIDs, all-day DATE for midnight dues, RRULEs for
    /// recognized repeats, and CRLF endings.
    #[test]
    fn ical_feed_should_be_minimal_and_stable() {
        let columns = vec![ColumnRow {
            id: "c1".into(),
            board_id: "b1".into(),
            name: "To-do".into(),
            position: 0,
            wip_limit: None,
            is_done: false,
            description: String::new(),
            collapsed: false,
            config_json: "{}".into(),
            former_names: Vec::new(),
        }];
        let tasks = vec![
            feed_task(
                "t1",
                "Ship it; now, please",
                Some("2026-10-15T14:30:00Z"),
                Some("every week"),
                false,
            ),
            feed_task("t2", "No due date", None, None, false),
            feed_task("t3", "All dayer", Some("2026-10-16T00:00:00Z"), None, false),
            feed_task("t4", "Done task", Some("2026-10-17T09:00:00Z"), None, true),
        ];
        let now = Utc::now();
        let feed = build_ical_feed("Work", &columns, &tasks, now);

        // Envelope.
        assert!(feed.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(feed.ends_with("END:VCALENDAR\r\n"));
        assert!(feed.contains("X-WR-CALNAME:Work\r\n"));

        // Only the three tasks with due dates become events.
        assert_eq!(feed.matches("BEGIN:VEVENT\r\n").count(), 3);
        assert!(!feed.contains("No due date"));

        // Stable UIDs across builds (feed pollers update, not duplicate).
        let again = build_ical_feed("Work", &columns, &tasks, now);
        for uid in ["UID:t1@chipflow", "UID:t3@chipflow", "UID:t4@chipflow"] {
            assert!(feed.contains(uid), "missing {uid}");
            assert!(again.contains(uid), "unstable {uid}");
        }

        // Timed vs all-day rendering.
        assert!(feed.contains("DTSTART:20261015T143000Z\r\n"));
        assert!(feed.contains("DTSTART;VALUE=DATE:20261016\r\n"));
        assert!(feed.contains("DTEND;VALUE=DATE:20261017\r\n"));

        // TEXT escaping in SUMMARY; minimal DESCRIPTION (column only).
        assert!(feed.contains("SUMMARY:Ship it\\; now\\, please\r\n"));
        assert!(feed.contains("DESCRIPTION:Column: To-do\r\n"));

        // Done state and repeat rule. VEVENT status is CONFIRMED even for
        // done tasks (RFC 5545 §3.8.1.11 — COMPLETED is VTODO-only).
        assert_eq!(feed.matches("STATUS:CONFIRMED\r\n").count(), 3);
        assert!(!feed.contains("STATUS:COMPLETED"));
        assert_eq!(feed.matches("RRULE:FREQ=WEEKLY\r\n").count(), 1);

        // No member, comment, history, or user data leaks into the feed.
        for leaked in ["member", "comment", "history", "password"] {
            assert!(!feed.to_lowercase().contains(leaked), "feed leaks {leaked}");
        }

        // Every content line is CRLF-terminated.
        assert!(!feed.replace("\r\n", "").contains('\n'));
    }

    /// Each legend segment takes its color's card face and border.
    #[test]
    fn legend_should_paint_segments_with_the_card_palette() {
        let (_, _, html) = board_grid(&[("Default", 0)]);
        assert!(
            html.contains("background-color: #cce3ff; border-top-color: #70b0ff\""),
            "blue segment uses the card face and border"
        );
    }

    /// The board table carries its column counts for the minimum width below which the board
    /// scrolls instead of shrinking.
    #[test]
    fn board_table_should_carry_expanded_and_collapsed_column_counts() {
        let (_, _, html) = board_grid(&[("Default", 0)]);
        assert!(html.contains("style=\"--expanded-columns: 5; --collapsed-columns: 0\""));
        let (_, _, html) = board_grid_collapsed(&[("Default", 0)], &[1, 3]);
        assert!(html.contains("style=\"--expanded-columns: 3; --collapsed-columns: 2\""));
    }

    /// Renders a board with five columns and the given `(lane name, task
    /// count)` bands, then returns the header cell count and each row's
    /// cell count.
    fn board_grid(lanes: &[(&str, usize)]) -> (usize, Vec<usize>, String) {
        board_grid_collapsed(lanes, &[])
    }

    /// Renders like [`board_grid`] with the columns at `collapsed` indexes
    /// collapsed.
    fn board_grid_collapsed(
        lanes: &[(&str, usize)],
        collapsed: &[usize],
    ) -> (usize, Vec<usize>, String) {
        use askama::Template;

        use super::{
            BandView, BoardLayout, BoardTemplate, CellView, ColorView, ColumnHead,
            needs_lane_label_column, place_overdue_strips,
        };

        let columns: Vec<ColumnHead> = ["Backlog", "To Do", "In Progress", "In Review", "Done"]
            .iter()
            .enumerate()
            .map(|(i, name)| ColumnHead {
                id: format!("col{i}"),
                name: (*name).into(),
                description: String::new(),
                collapsed: collapsed.contains(&i),
                wip_limit: None,
                count: 0,
                over_limit: false,
                overdue_count: usize::from(i == 1),
                is_done: i == 4,
                config_json: "{}".into(),
                sum_label: None,
                sum_kind: "count",
                estimate_minutes: 0,
            })
            .collect();
        let mut bands: Vec<BandView> = lanes
            .iter()
            .enumerate()
            .map(|(i, (name, task_count))| BandView {
                id: format!("lane{i}"),
                name: (*name).into(),
                is_main: *name == "Default",
                task_count: *task_count,
                cells: columns
                    .iter()
                    .map(|col| CellView {
                        column_id: col.id.clone(),
                        swimlane_id: format!("lane{i}"),
                        archiving: col.is_done,
                        sorted: false,
                        archive_more: 0,
                        collapsed: col.collapsed,
                        column_name: col.name.clone(),
                        tasks: Vec::new(),
                        done_groups: Vec::new(),
                        overdue_count: 0,
                        column_task_count: 0,
                    })
                    .collect(),
            })
            .collect();
        let hide_swimlane_header = bands.len() == 1 && bands[0].is_main;
        let lane_label_column = needs_lane_label_column(&bands);
        place_overdue_strips(&mut bands, &columns, hide_swimlane_header);
        let html = BoardTemplate {
            board_id: "b".into(),
            board_name: "Grid".into(),
            username: "audit".into(),
            username_initial: "AU".into(),
            columns,
            bands,
            layout: BoardLayout {
                hide_swimlane_header,
                lane_label_column,
            },
            new_tasks_at_top: false,
            colors: vec![ColorView {
                id: "c".into(),
                value: "blue".into(),
                standard_name: "Blue".into(),
                label: String::new(),
                description: String::new(),
                enabled: true,
                is_default: false,
                sort_order: 1,
                bg: "#cce3ff".into(),
                border: "#70b0ff".into(),
                light: "#d6e9ff".into(),
            }],
            legend_visible: true,
            boards: Vec::new(),
            due_time: "17:00".into(),
            members: Vec::new(),
        }
        .render()
        .expect("render board");

        let count_cells = |row: &str| row.matches("<th ").count() + row.matches("<td ").count();
        let head_start = html.find("id=\"column-headers-row\"").expect("header row");
        let head = &html[head_start..];
        let head_cells = count_cells(&head[..head.find("</tr>").expect("header end")]);
        let rows = html
            .split("<tr class=\"swimlane-row\">")
            .skip(1)
            .map(|row| count_cells(&row[..row.find("</tr>").expect("row end")]))
            .collect();
        (head_cells, rows, html)
    }

    /// The main lane's row keeps a cell under the corner cell.
    #[test]
    fn board_rows_should_align_with_headers_when_main_lane_has_company() {
        let (head, rows, html) = board_grid(&[("Default", 2), ("Lane Two", 1)]);
        assert_eq!(head, 6, "corner plus five column headers");
        assert_eq!(rows, vec![6, 6], "every row spans the header");
        assert!(
            html.contains("class=\"swimlane-gutter\""),
            "main lane gets a blank cell"
        );
        assert_eq!(
            html.matches("class=\"swimlane-label\"").count(),
            1,
            "only Lane Two is labeled"
        );
    }

    /// Folded lanes leave the main lane without a label column.
    #[test]
    fn board_should_drop_label_column_when_other_lanes_are_folded() {
        let (head, rows, html) = board_grid(&[("Default", 2), ("Lane Two", 0)]);
        assert_eq!(head, 5, "no corner cell");
        assert_eq!(rows, vec![5], "the main row spans the header");
        assert!(!html.contains("corner-cell"), "no corner cell markup");
    }

    /// A lone main lane renders no swimlane column at all.
    #[test]
    fn board_with_lone_main_lane_should_have_no_label_column() {
        let (head, rows, _) = board_grid(&[("Default", 0)]);
        assert_eq!(head, 5, "no corner cell");
        assert_eq!(rows, vec![5], "the main row still renders");
    }

    /// The overdue strip heads its column once, in the first row.
    #[test]
    fn board_should_show_overdue_strip_only_in_first_rendered_row() {
        let (_, rows, html) = board_grid(&[("Default", 0), ("Lane Two", 3), ("Lane Three", 1)]);
        assert_eq!(rows.len(), 2, "two rows render");
        assert_eq!(
            html.matches("class=\"columnOverdue\"").count(),
            1,
            "one strip"
        );
        let second_row = html
            .rfind("<tr class=\"swimlane-row\">")
            .expect("second row");
        assert!(
            html.find("class=\"columnOverdue\"").expect("strip") < second_row,
            "the strip sits in Lane Two's row"
        );
        assert!(html.contains(">1 overdue task</span>"), "singular label");
    }

    /// A non-main lane is labeled even when the main lane is folded.
    #[test]
    fn board_should_label_rows_when_main_lane_is_folded() {
        let (head, rows, html) = board_grid(&[("Default", 0), ("Lane Two", 3), ("Lane Three", 1)]);
        assert_eq!(head, 6, "corner plus five column headers");
        assert_eq!(rows, vec![6, 6], "both rows span the header");
        assert!(
            !html.contains("class=\"swimlane-gutter\""),
            "folded main lane has no row"
        );
    }

    /// Sends one cookie-authenticated request through a clone of the
    /// fixture app; `json` becomes an `application/json` body.
    async fn send(
        fx: &Kf221Fixture,
        method: &str,
        uri: &str,
        json: Option<serde_json::Value>,
    ) -> (axum::http::StatusCode, String) {
        use tower::ServiceExt;

        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", &fx.cookie)
            .header("accept", "application/json");
        let body = match json {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                axum::body::Body::from(value.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let response = fx
            .app
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Creates a task in the fixture's WIP column with `extra` merged into
    /// the JSON body; returns the status and the parsed response body.
    async fn create_task_json(
        fx: &Kf221Fixture,
        extra: serde_json::Value,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let mut body = serde_json::json!({"column_id": fx.col_wip, "name": "Task"});
        if let (Some(target), Some(fields)) = (body.as_object_mut(), extra.as_object()) {
            target.extend(fields.clone());
        }
        let (status, text) = send(fx, "POST", "/api/tasks", Some(body)).await;
        let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        (status, value)
    }

    /// New tasks land at the bottom of their column unless the board's "Position of new tasks" says
    /// top.
    #[tokio::test]
    async fn create_task_should_follow_the_new_task_position_setting() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        let (status, _) = send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        assert!(status.is_success(), "swimlane created");
        for name in ["Alpha", "Bravo"] {
            create_task_json(&fx, serde_json::json!({ "name": name })).await;
        }
        let config = format!("/api/boards/{}/config", fx.board_id);
        let at_top = serde_json::json!({ "new_tasks_at_top": true });
        let (status, body) = send(&fx, "PUT", &config, Some(at_top)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(
            body.contains("\"new_tasks_at_top\":true"),
            "config echoes the setting"
        );
        create_task_json(&fx, serde_json::json!({ "name": "Charlie" })).await;

        let (_, page) = send(&fx, "GET", &format!("/b/{}", fx.board_id), None).await;
        let at = |name: &str| page.find(&format!(">{name}<")).expect(name);
        assert!(at("Alpha") < at("Bravo"), "default appends at the bottom");
        assert!(at("Charlie") < at("Alpha"), "the top setting prepends");
    }

    /// Manual time lands on the task the request names, even when another board has a task of the
    /// same name.
    #[tokio::test]
    async fn manual_time_should_stay_on_the_named_task_across_boards() {
        let (_dir, fx) = kf221_fixture();
        let (_, a) = create_task_json(&fx, serde_json::json!({"name": "Same"})).await;
        let body = serde_json::json!({"name": "Other"});
        let (_, board) = send(&fx, "POST", "/api/boards", Some(body)).await;
        let board: serde_json::Value = serde_json::from_str(&board).expect("board json");
        let columns = get_json(
            &fx,
            &format!("/api/boards/{}/columns", board["id"].as_str().expect("id")),
        )
        .await;
        let extra = serde_json::json!({"name": "Same", "column_id": columns[0]["id"]});
        let (_, b) = create_task_json(&fx, extra).await;
        for (task, from, to) in [(&a, "08:00", "08:30"), (&b, "09:00", "09:45")] {
            let body = serde_json::json!({
                "task_id": task["id"], "date": "2026-10-16", "from": from, "to": to,
            });
            let (status, text) = send(&fx, "POST", "/api/time/manual", Some(body)).await;
            assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        }
        for (task, minutes) in [(&a, 30), (&b, 45)] {
            let id = task["id"].as_str().expect("id");
            let log = get_json(&fx, &format!("/api/timer/log?task_id={id}")).await;
            let entries = log["entries"].as_array().expect("entries");
            assert_eq!(entries.len(), 1, "{log}");
            assert_eq!(entries[0]["minutes"], minutes, "{log}");
        }
    }

    /// Uploads past axum's 2 MiB default body limit reach the handler, and a file just over 10 MiB
    /// gets the handler's 400.
    #[tokio::test]
    async fn attachment_upload_should_accept_files_up_to_ten_mib() {
        use base64::Engine as _;

        let (_dir, fx) = kf221_fixture();
        let (status, task) = create_task_json(&fx, serde_json::json!({})).await;
        assert_eq!(status, axum::http::StatusCode::CREATED);
        let uri = format!(
            "/api/tasks/{}/attachments",
            task["id"].as_str().expect("id")
        );
        let max = crate::db::Db::MAX_ATTACHMENT_BYTES as usize;
        for (size, expected) in [
            (3 * 1024 * 1024, axum::http::StatusCode::OK),
            (max, axum::http::StatusCode::OK),
            (max + 1, axum::http::StatusCode::BAD_REQUEST),
        ] {
            let data = base64::engine::general_purpose::STANDARD.encode(vec![7u8; size]);
            let body = serde_json::json!({"name": "big.bin", "data": data});
            let (status, text) = send(&fx, "POST", &uri, Some(body)).await;
            assert_eq!(status, expected, "{size} bytes: {text}");
        }
    }

    /// A due target for a task without a due date is a 400 naming the field, and with a due date,
    /// or sent together with one, it applies.
    #[tokio::test]
    async fn due_target_without_due_date_should_be_a_400() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let target = serde_json::json!({ "due_target_column_id": fx.col_done });
        let (status, text) = send(&fx, "PATCH", &uri, Some(target.clone())).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{text}");
        assert!(text.contains("due_target_column_id"), "{text}");
        assert_eq!(
            get_json(&fx, &uri).await["due_target_column_id"],
            serde_json::Value::Null
        );

        let both = serde_json::json!({
            "due_at": "2099-01-01 17:00",
            "due_target_column_id": fx.col_done,
        });
        let (status, text) = send(&fx, "PATCH", &uri, Some(both)).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        assert_eq!(
            get_json(&fx, &uri).await["due_target_column_id"],
            fx.col_done.as_str()
        );
    }

    /// Key, kind and owner on a field PATCH are a 400 naming the property and change nothing, and
    /// the other properties still apply.
    #[tokio::test]
    async fn field_patch_should_refuse_key_kind_and_owner() {
        let (_dir, fx) = kf221_fixture();
        let (status, text) = send(
            &fx,
            "POST",
            &format!("/api/boards/{}/fields", fx.board_id),
            Some(serde_json::json!({"key": "stage", "label": "Stage", "kind": "number"})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let field: serde_json::Value = serde_json::from_str(&text).expect("json");
        let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
        for (name, value) in [("kind", "text"), ("key", "other"), ("owner", "system")] {
            let patch = serde_json::json!({ "label": "Changed", name: value });
            let (status, text) = send(&fx, "PATCH", &uri, Some(patch)).await;
            assert_eq!(
                status,
                axum::http::StatusCode::BAD_REQUEST,
                "{name}: {text}"
            );
            assert!(text.contains(name), "{text}");
        }
        let fields = get_json(&fx, &format!("/api/boards/{}/fields", fx.board_id)).await;
        assert_eq!(fields[0]["label"], "Stage", "a refused PATCH wrote nothing");
        assert_eq!(fields[0]["kind"], "number");
        let (status, text) = send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({"label": "Renamed"})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
    }

    /// The new-board page limits the name to 100 characters, counted as the server counts them, not
    /// with a UTF-16 `maxlength`.
    #[tokio::test]
    async fn new_board_page_should_limit_the_name_to_100_characters() {
        let (_dir, fx) = kf221_fixture();
        let (status, html) = send(&fx, "GET", "/boards/new", None).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let input = html
            .split("<input")
            .find(|tag| tag.contains("id=\"nb-name\""))
            .expect("name input");
        let input = input.split('>').next().expect("tag");
        assert!(!input.contains("maxlength"), "{input}");
        assert!(html.contains("Array.from(name).length > 100"));
        assert!(html.contains("Board name must be 1-100 characters."));
    }

    /// A template resolves a task's column and due target by their position when names repeat.
    #[tokio::test]
    async fn template_should_keep_a_due_target_among_same_named_columns() {
        let (_dir, fx) = kf221_fixture();
        let board = fx.db.create_board("Twins").expect("board");
        let first = fx.db.create_column(&board, "Same", None).expect("first");
        let second = fx.db.create_column(&board, "Same", None).expect("second");
        for (name, column, target) in [("a", &second, &first), ("b", &first, &second)] {
            let (status, text) = send(
                &fx,
                "POST",
                "/api/tasks",
                Some(serde_json::json!({
                    "column_id": column, "name": name, "due_at": "2099-01-01 17:00",
                })),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::CREATED, "{text}");
            let created: serde_json::Value = serde_json::from_str(&text).expect("json");
            let uri = format!("/api/tasks/{}", created["id"].as_str().expect("id"));
            let patch = serde_json::json!({ "due_target_column_id": target });
            let (status, text) = send(&fx, "PATCH", &uri, Some(patch)).await;
            assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        }
        let (_, saved) = send(
            &fx,
            "POST",
            &format!("/api/boards/{board}/save-as-template"),
            Some(serde_json::json!({"name": "Twins"})),
        )
        .await;
        let saved: serde_json::Value = serde_json::from_str(&saved).expect("json");
        let (_, made) = send(
            &fx,
            "POST",
            "/api/boards",
            Some(serde_json::json!({"name": "Copy", "template_id": saved["id"]})),
        )
        .await;
        let made: serde_json::Value = serde_json::from_str(&made).expect("json");
        let copy = made["id"].as_str().expect("board id");
        let columns = get_json(&fx, &format!("/api/boards/{copy}/columns")).await;
        let ids: Vec<String> = columns
            .as_array()
            .expect("columns")
            .iter()
            .filter(|column| column["name"] == "Same")
            .map(|column| column["id"].as_str().expect("id").to_string())
            .collect();
        assert_eq!(ids.len(), 2);
        let tasks = get_json(&fx, &format!("/api/boards/{copy}/tasks")).await;
        for task in tasks.as_array().expect("tasks") {
            let detail = get_json(
                &fx,
                &format!("/api/tasks/{}", task["id"].as_str().expect("id")),
            )
            .await;
            let (own, target) = if task["name"] == "a" {
                (&ids[1], &ids[0])
            } else {
                (&ids[0], &ids[1])
            };
            assert_eq!(detail["column_id"], own.as_str(), "task {}", task["name"]);
            assert_eq!(
                detail["due_target_column_id"],
                target.as_str(),
                "task {}",
                task["name"]
            );
        }
    }

    /// Concurrent starts retire the running session once, and the timer status pairs each request's
    /// mode with its own session id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_timer_starts_should_pair_timer_and_session() {
        use tower::ServiceExt;

        use crate::models::{ActiveTimer, TimerMode};

        let (_dir, fx) = kf221_fixture();
        let start = |app: axum::Router, cookie: String, mode: &'static str| async move {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/api/timer/start")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "mode": mode }).to_string(),
                ))
                .expect("request");
            let response = app.oneshot(request).await.expect("oneshot");
            json_body(response).await
        };
        for round in 0..100 {
            fx.db
                .set_active_timer(&ActiveTimer {
                    task_id: None,
                    mode: TimerMode::Stopwatch,
                    started_at: chrono::Utc::now().timestamp() - 60,
                    duration_secs: None,
                })
                .expect("running timer");
            let logged = fx.db.all_entries().expect("entries").len();
            let a = tokio::spawn(start(fx.app.clone(), fx.cookie.clone(), "stopwatch"));
            let b = tokio::spawn(start(fx.app.clone(), fx.cookie.clone(), "short_break"));
            let (a, b) = (a.await.expect("a"), b.await.expect("b"));
            assert_eq!(
                fx.db.all_entries().expect("entries").len(),
                logged + 1,
                "round {round}: the running session is retired once"
            );
            let (_, status) = send(&fx, "GET", "/api/timer/status", None).await;
            let status: serde_json::Value = serde_json::from_str(&status).expect("json");
            let own = if status["mode"] == "stopwatch" {
                &a
            } else {
                &b
            };
            assert_eq!(status["mode"], own["mode"], "round {round}");
            assert_eq!(status["session_id"], own["session_id"], "round {round}");
            assert_eq!(status["started_at"], own["started_at"], "round {round}");
            assert_ne!(a["session_id"], b["session_id"], "round {round}");
        }
    }

    /// A blank board_id filter lists every board's tasks.
    #[tokio::test]
    async fn task_list_should_treat_a_blank_board_id_as_no_filter() {
        let (_dir, fx) = kf221_fixture();
        create_task_json(&fx, serde_json::json!({})).await;
        let (status, all) = send(&fx, "GET", "/api/tasks", None).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        for blank in ["%20", "", "%20%20"] {
            let (status, text) =
                send(&fx, "GET", &format!("/api/tasks?board_id={blank}"), None).await;
            assert_eq!(status, axum::http::StatusCode::OK, "{blank:?}: {text}");
            assert_eq!(text, all, "{blank:?}");
        }
        let (status, _) = send(&fx, "GET", "/api/tasks?board_id=nope", None).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    }

    /// A blank or over-long board name re-renders the General tab with a 400 and keeps the stored
    /// name, and a valid one still redirects.
    #[tokio::test]
    async fn board_rename_should_reject_blank_and_long_names_with_400() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        let post_name = |name: String| {
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/b/{}/settings", fx.board_id))
                .header("cookie", &fx.cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(axum::body::Body::from(
                    serde_urlencoded::to_string([("name", name)]).expect("form"),
                ))
                .expect("request")
        };
        for name in ["   ".to_string(), "x".repeat(101)] {
            let response = fx
                .app
                .clone()
                .oneshot(post_name(name))
                .await
                .expect("oneshot");
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("body");
            let html = String::from_utf8_lossy(&body);
            assert!(html.contains("Board name must be 1-100 characters."));
            assert!(html.contains(&format!("action=\"/b/{}/settings\"", fx.board_id)));
            assert!(html.contains("value=\"KF-221\""), "stored name kept");
        }
        let response = fx
            .app
            .clone()
            .oneshot(post_name(format!(" {} ", "y".repeat(100))))
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        let (_, boards) = send(&fx, "GET", "/api/boards", None).await;
        assert!(boards.contains(&format!("\"{}\"", "y".repeat(100))));
    }

    /// Every accepted format normalizes to RFC3339, and garbage and out-of-range values are errors.
    #[test]
    fn normalize_due_input_should_reject_unparseable() {
        use super::normalize_due_input_in;

        let zone = chrono_tz::Europe::London;
        let ok = |raw: &str| normalize_due_input_in(raw, &zone).expect(raw);
        assert_eq!(ok("2026-10-20T08:00:00Z"), "2026-10-20T08:00:00Z");
        assert_eq!(ok("2026-10-20T08:00:00.000Z"), "2026-10-20T08:00:00.000Z");
        assert_eq!(ok("2026-10-20 17:00"), "2026-10-20T17:00:00+01:00");
        assert_eq!(ok("2026-10-20T17:00"), "2026-10-20T17:00:00+01:00");
        assert_eq!(ok("2026-10-20 17:00:30"), "2026-10-20T17:00:30+01:00");
        assert_eq!(ok("2026-10-20T17:00:30"), "2026-10-20T17:00:30+01:00");
        assert_eq!(ok(" 2026-12-01 "), "2026-12-01T17:00:00+00:00");
        for bad in [
            "next tuesday",
            "garbage",
            "2026-13-45",
            "2026-02-30",
            "2026-10-20 25:00",
            "2026-10-20 17:00 +02:00",
        ] {
            assert!(
                normalize_due_input_in(bad, &zone).is_err(),
                "{bad} rejected"
            );
        }
    }

    /// Local times one second before, at, and one second after each DST
    /// transition, with the UTC instant each must resolve to (the earlier
    /// one when repeated), or None when the transition skips it.
    fn dst_transition_cases() -> Vec<(&'static str, chrono_tz::Tz, Option<&'static str>)> {
        use chrono_tz::America::New_York;
        use chrono_tz::Europe::London;

        let mut cases = Vec::new();

        // London fall-back: 02:00 BST becomes 01:00 GMT at 01:00Z.
        cases.extend([
            ("2026-10-25 00:59:59", London, Some("2026-10-24T23:59:59Z")),
            ("2026-10-25 01:00:00", London, Some("2026-10-25T00:00:00Z")),
            ("2026-10-25 01:00:01", London, Some("2026-10-25T00:00:01Z")),
            ("2026-10-25 01:59:59", London, Some("2026-10-25T00:59:59Z")),
            ("2026-10-25 02:00:00", London, Some("2026-10-25T02:00:00Z")),
            ("2026-10-25 02:00:01", London, Some("2026-10-25T02:00:01Z")),
        ]);

        // London spring-forward: 01:00 GMT becomes 02:00 BST at 01:00Z.
        cases.extend([
            ("2027-03-28 00:59:59", London, Some("2027-03-28T00:59:59Z")),
            ("2027-03-28 01:00:00", London, None),
            ("2027-03-28 01:00:01", London, None),
            ("2027-03-28 01:59:59", London, None),
            ("2027-03-28 02:00:00", London, Some("2027-03-28T01:00:00Z")),
            ("2027-03-28 02:00:01", London, Some("2027-03-28T01:00:01Z")),
        ]);

        // New York fall-back: 02:00 EDT becomes 01:00 EST at 06:00Z.
        cases.extend([
            (
                "2026-11-01 00:59:59",
                New_York,
                Some("2026-11-01T04:59:59Z"),
            ),
            (
                "2026-11-01 01:00:00",
                New_York,
                Some("2026-11-01T05:00:00Z"),
            ),
            (
                "2026-11-01 01:00:01",
                New_York,
                Some("2026-11-01T05:00:01Z"),
            ),
            (
                "2026-11-01 01:59:59",
                New_York,
                Some("2026-11-01T05:59:59Z"),
            ),
            (
                "2026-11-01 02:00:00",
                New_York,
                Some("2026-11-01T07:00:00Z"),
            ),
            (
                "2026-11-01 02:00:01",
                New_York,
                Some("2026-11-01T07:00:01Z"),
            ),
        ]);

        // New York spring-forward: 02:00 EST becomes 03:00 EDT at 07:00Z.
        cases.extend([
            (
                "2027-03-14 01:59:59",
                New_York,
                Some("2027-03-14T06:59:59Z"),
            ),
            ("2027-03-14 02:00:00", New_York, None),
            ("2027-03-14 02:00:01", New_York, None),
            ("2027-03-14 02:59:59", New_York, None),
            (
                "2027-03-14 03:00:00",
                New_York,
                Some("2027-03-14T07:00:00Z"),
            ),
            (
                "2027-03-14 03:00:01",
                New_York,
                Some("2027-03-14T07:00:01Z"),
            ),
        ]);

        cases
    }

    /// The UTC instant (whole seconds, `Z`) a due input normalized to.
    fn due_as_utc(due: Result<String, String>) -> Option<String> {
        due.ok().map(|due| {
            DateTime::parse_from_rfc3339(&due)
                .expect("RFC3339")
                .with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
    }

    /// DST transition seconds resolve correctly in explicit zones.
    #[test]
    fn normalize_due_input_should_resolve_dst_transition_seconds() {
        use super::normalize_due_input_in;

        for (raw, zone, expected) in dst_transition_cases() {
            let got = due_as_utc(normalize_due_input_in(raw, &zone));
            assert_eq!(got.as_deref(), expected, "{raw} in {zone}");
        }

        // The stored offset is the one in force at that instant.
        let london = chrono_tz::Europe::London;
        assert_eq!(
            normalize_due_input_in("2026-10-25 02:00", &london).as_deref(),
            Ok("2026-10-25T02:00:00+00:00")
        );
        assert_eq!(
            normalize_due_input_in("2026-10-25 01:30", &london).as_deref(),
            Ok("2026-10-25T01:30:00+01:00")
        );
    }

    /// Marker the child test prints when `Local` cannot load its zone.
    const LOCAL_ZONE_SKIP: &str = "SKIP local zone unavailable";

    /// Marker the child test prints with the number of cases it checked.
    const LOCAL_ZONE_CHECKED: &str = "checked local zone cases:";

    /// Resolves the same transition seconds through the server's `Local` zone, whose chrono lookup
    /// misreads them. Each zone runs the child half in a separate test process so TZ is set before
    /// chrono first reads it. Where TZ cannot load the zone (no zone database, as in the Nix build
    /// sandbox) the child reports a skip and this test passes with a note.
    #[test]
    fn normalize_due_input_should_resolve_dst_transition_seconds_in_local_zone() {
        for zone in ["Europe/London", "America/New_York"] {
            // chrono only searches fixed system directories, so honor
            // TZDIR the way libc does by passing the zone file's path.
            let tz = match std::env::var("TZDIR") {
                Ok(dir) if !dir.is_empty() => format!("{dir}/{zone}"),
                _ => zone.to_string(),
            };
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--ignored",
                    "--exact",
                    "--nocapture",
                    "routes::tests::dst_transition_should_resolve_cases_in_local_zone",
                ])
                .env("TZ", tz)
                .env("CHIPFLOW_DST_ZONE", zone)
                .output()
                .expect("child test process");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let report = format!(
                "{zone}: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{report}"
            );
            if stdout.contains(LOCAL_ZONE_SKIP) {
                println!("note: {zone} skipped, TZ does not load it here (no zone database)");
                continue;
            }
            let checked = stdout
                .lines()
                .find_map(|line| line.trim().strip_prefix(LOCAL_ZONE_CHECKED))
                .and_then(|count| count.trim().parse::<usize>().ok());
            assert!(checked.is_some_and(|n| n > 0), "no cases checked: {report}");
            println!("{zone}: {} local-zone cases checked", checked.unwrap_or(0));
        }
    }

    // ast-grep-ignore: test-ignore
    #[ignore = "run in a child process with TZ set by the parent test"]
    /// Checks the transition cases in the zone TZ was set to. A no-op unless
    /// `CHIPFLOW_DST_ZONE` names that zone.
    #[test]
    fn dst_transition_should_resolve_cases_in_local_zone() {
        use chrono::{Offset as _, TimeZone as _};

        let Ok(zone) = std::env::var("CHIPFLOW_DST_ZONE") else {
            return;
        };
        let zone: chrono_tz::Tz = zone.parse().expect("zone name");

        // `Local` falls back to UTC when TZ names a zone it cannot load, so
        // it must match chrono-tz in both summer and winter before the
        // cases mean anything.
        let loaded = ["2026-07-01 12:00:00", "2026-01-01 12:00:00"]
            .iter()
            .all(|at| {
                let utc = chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
                    .expect("probe instant");
                Local.offset_from_utc_datetime(&utc).fix()
                    == zone.offset_from_utc_datetime(&utc).fix()
            });
        if !loaded {
            println!("{LOCAL_ZONE_SKIP}: TZ={zone} does not match chrono-tz");
            return;
        }
        let mut checked = 0;
        for (raw, case_zone, expected) in dst_transition_cases() {
            if case_zone == zone {
                let got = due_as_utc(normalize_due_input(raw, crate::models::default_due_time()));
                assert_eq!(got.as_deref(), expected, "{raw} in Local {zone}");
                checked += 1;
            }
        }
        println!("{LOCAL_ZONE_CHECKED} {checked}");
    }

    /// POST and PATCH reject an unparseable due_at with 400, the POST creates no task, and the
    /// PATCH writes no other field.
    #[tokio::test]
    async fn task_api_should_reject_unparseable_due_at() {
        let (_dir, fx) = kf221_fixture();
        let (status, body) = create_task_json(
            &fx,
            serde_json::json!({"name": "Bad due", "due_at": "next tuesday"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
        let (_, tasks) = send(&fx, "GET", "/api/tasks", None).await;
        assert!(!tasks.contains("Bad due"), "rejected POST created no task");

        let (status, task) = create_task_json(
            &fx,
            serde_json::json!({"name": "Good due", "due_at": "2026-10-20 17:00"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::CREATED);
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let (_, detail) = send(&fx, "GET", &uri, None).await;
        let detail: serde_json::Value = serde_json::from_str(&detail).expect("json");
        let stored = detail["due_at"].as_str().expect("due stored").to_string();
        assert!(DateTime::parse_from_rfc3339(&stored).is_ok(), "{stored}");

        for bad in ["garbage", "2026-13-45"] {
            let patch = serde_json::json!({"name": "Renamed", "due_at": bad});
            let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{bad}");
        }
        let (_, detail) = send(&fx, "GET", &uri, None).await;
        let detail: serde_json::Value = serde_json::from_str(&detail).expect("json");
        assert_eq!(detail["due_at"], stored.as_str(), "due_at unchanged");
        let (_, tasks) = send(&fx, "GET", "/api/tasks", None).await;
        assert!(!tasks.contains("Renamed"), "rejected PATCH renamed nothing");
    }

    /// A subtask takes a due date on create and PATCH, validates it like a task's, clears it on
    /// null, records history, and the export dataset carries it.
    #[tokio::test]
    async fn subtask_due_date_should_round_trip_through_the_api() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({"name": "Host"})).await;
        let task_id = task["id"].as_str().expect("id").to_string();
        let base = format!("/api/tasks/{task_id}/subtasks");

        let (status, _) = send(
            &fx,
            "POST",
            &base,
            Some(serde_json::json!({"name": "Bad", "due_at": "garbage"})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

        let (status, text) = send(
            &fx,
            "POST",
            &base,
            Some(serde_json::json!({"name": "Step", "due_at": "2026-10-20 17:00"})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let sub: serde_json::Value = serde_json::from_str(&text).expect("json");
        let stored = sub["due_at"].as_str().expect("due stored").to_string();
        assert!(DateTime::parse_from_rfc3339(&stored).is_ok(), "{stored}");
        let uri = format!("{base}/{}", sub["id"].as_str().expect("sub id"));

        let data = get_json(&fx, &format!("/api/boards/{}/export-data", fx.board_id)).await;
        let created_history = data["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["id"] == task_id.as_str())
            .expect("task exported")["history"]
            .to_string();
        assert!(
            created_history.contains("subtask_due_set"),
            "{created_history}"
        );

        let (status, _) = send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({"due_at": "2026-13-45"})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        let (_, text) = send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({"name": "Renamed"})),
        )
        .await;
        let renamed: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(renamed["due_at"], stored.as_str(), "absent leaves it");

        let data = get_json(&fx, &format!("/api/boards/{}/export-data", fx.board_id)).await;
        let exported = data["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["id"] == task_id.as_str())
            .expect("task exported")["subtasks"][0]["due_at"]
            .clone();
        assert_eq!(exported, stored.as_str());

        let (_, text) = send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({"due_at": null})),
        )
        .await;
        let cleared: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert!(cleared["due_at"].is_null());
        let data = get_json(&fx, &format!("/api/boards/{}/export-data", fx.board_id)).await;
        let history = data["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["id"] == task_id.as_str())
            .expect("task exported")["history"]
            .to_string();
        assert!(history.contains("subtask_due_cleared"), "{history}");
    }

    /// SubtaskDetail always carries due_at, so the schema requires it.
    #[test]
    fn subtask_detail_schema_should_require_due_at() {
        use utoipa::OpenApi;
        let spec: serde_json::Value =
            serde_json::from_str(&ApiDoc::openapi().to_json().expect("json")).expect("spec");
        let required = &spec["components"]["schemas"]["SubtaskDetail"]["required"];
        assert!(
            required
                .as_array()
                .expect("required")
                .contains(&serde_json::json!("due_at")),
            "{required}"
        );
    }

    /// The subtask due history line shows the readable date, not RFC3339.
    #[tokio::test]
    async fn subtask_due_history_detail_should_be_readable() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({"name": "Host"})).await;
        let task_id = task["id"].as_str().expect("id").to_string();
        let base = format!("/api/tasks/{task_id}/subtasks");
        let (_, text) = send(
            &fx,
            "POST",
            &base,
            Some(serde_json::json!({"name": "Step"})),
        )
        .await;
        let sub: serde_json::Value = serde_json::from_str(&text).expect("json");
        let uri = format!("{base}/{}", sub["id"].as_str().expect("sub id"));
        let (_, text) = send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({"due_at": "2026-10-20T17:00:00Z"})),
        )
        .await;
        let sub: serde_json::Value = serde_json::from_str(&text).expect("json");
        let expected = super::format_datetime(sub["due_at"].as_str().expect("due"));
        let data = get_json(&fx, &format!("/api/boards/{}/export-data", fx.board_id)).await;
        let history = data["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["id"] == task_id.as_str())
            .expect("task exported")["history"]
            .clone();
        let detail = history
            .as_array()
            .expect("history")
            .iter()
            .find(|e| e["kind"] == "subtask_due_set")
            .expect("subtask_due_set event")["detail"]
            .as_str()
            .expect("detail")
            .to_string();
        assert_eq!(
            detail,
            format!("Set due date of subtask \"Step\" to {expected}")
        );
        assert!(expected.contains("Oct 20, 2026"), "{expected}");
        assert!(!detail.contains('T'), "{detail}");
    }

    /// GETs `uri` through the fixture app and parses the JSON body.
    async fn get_json(fx: &Kf221Fixture, uri: &str) -> serde_json::Value {
        let (status, text) = send(fx, "GET", uri, None).await;
        assert_eq!(status, axum::http::StatusCode::OK, "GET {uri}: {text}");
        serde_json::from_str(&text).expect("json")
    }

    /// POST /api/boards applies the 1-100 character board-name rule.
    #[tokio::test]
    async fn create_board_should_refuse_names_over_100_characters() {
        let (_dir, fx) = kf221_fixture();
        for template in [None, Some("builtin-pomodoro")] {
            let mut body = serde_json::json!({ "name": "x".repeat(101) });
            if let Some(template) = template {
                body["template_id"] = template.into();
            }
            let (status, text) = send(&fx, "POST", "/api/boards", Some(body)).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{text}");
            assert!(text.contains("1-100"), "{text}");
        }
        let (status, _) = send(
            &fx,
            "POST",
            "/api/boards",
            Some(serde_json::json!({ "name": "   " })),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        let name = format!("  {}  ", "y".repeat(100));
        let (status, text) = send(
            &fx,
            "POST",
            "/api/boards",
            Some(serde_json::json!({ "name": name })),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let boards = get_json(&fx, "/api/boards").await;
        let names: Vec<&str> = boards
            .as_array()
            .expect("boards")
            .iter()
            .filter_map(|board| board["name"].as_str())
            .collect();
        assert!(names.contains(&"y".repeat(100).as_str()), "{names:?}");
        assert!(!names.iter().any(|name| name.len() > 100), "{names:?}");
    }

    /// Clearing the due date by any means clears its repeat too.
    #[tokio::test]
    async fn clearing_the_due_date_should_clear_its_repeat() {
        let (_dir, fx) = kf221_fixture();
        for clear in [
            serde_json::json!({"due_at": "   "}),
            serde_json::json!({"due_at": ""}),
            serde_json::json!({"due_at": null}),
        ] {
            let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
            let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
            let set =
                serde_json::json!({"due_at": "2026-10-20T08:00:00Z", "due_repeat": "every week"});
            send(&fx, "PATCH", &uri, Some(set)).await;
            assert_eq!(get_json(&fx, &uri).await["due_repeat"], "every week");
            let (status, text) = send(&fx, "PATCH", &uri, Some(clear.clone())).await;
            assert_eq!(status, axum::http::StatusCode::OK, "{text}");
            let task = get_json(&fx, &uri).await;
            assert_eq!(task["due_at"], serde_json::Value::Null, "{clear}");
            assert_eq!(task["due_repeat"], serde_json::Value::Null, "{clear}");
            let repeat_only = serde_json::json!({"due_repeat": "every day"});
            send(&fx, "PATCH", &uri, Some(repeat_only)).await;
            assert_eq!(
                get_json(&fx, &uri).await["due_repeat"],
                serde_json::Value::Null
            );
        }
    }

    /// Re-saving the same due instant in another format keeps `due_done` and the stored string, and
    /// a real change still resets it.
    #[tokio::test]
    async fn resaving_same_due_instant_should_keep_due_done() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let set = serde_json::json!({"due_at": "2026-10-20T08:00:00Z", "due_done": true});
        send(&fx, "PATCH", &uri, Some(set)).await;
        for same in ["2026-10-20T08:00:00.000Z", "2026-10-20T10:00:00+02:00"] {
            let patch = serde_json::json!({"due_at": same});
            let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
            assert_eq!(status, axum::http::StatusCode::OK);
            let task = get_json(&fx, &uri).await;
            assert_eq!(task["due_done"], true, "{same} keeps due_done");
            assert_eq!(
                task["due_at"], "2026-10-20T08:00:00Z",
                "{same} keeps the string"
            );
        }
        let changed = serde_json::json!({"due_at": "2026-10-21T08:00:00Z"});
        send(&fx, "PATCH", &uri, Some(changed)).await;
        let task = get_json(&fx, &uri).await;
        assert_eq!(task["due_done"], false, "a new instant resets due_done");
        assert_eq!(task["due_at"], "2026-10-21T08:00:00Z");
    }

    /// Statistics report `best_day: null` when nothing was logged, and the day with the most
    /// pomodori once one was.
    #[tokio::test]
    async fn statistics_best_day_should_be_null_without_pomodori() {
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::connect(
            dir.path()
                .join("kf391-test.redb")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("connect");
        let user_id = db
            .create_user(
                "admin",
                &crate::auth::hash_password("s3cret").expect("hash"),
            )
            .expect("create user");
        let cookie = format!(
            "session={}",
            crate::auth::create_session(&db, &user_id).expect("session")
        );
        let board_id = db.create_board("KF-391").expect("board");
        let column_id = db.create_column(&board_id, "Todo", None).expect("column");
        let task_id = db
            .create_task(&column_id, None, "Task", 1, None)
            .expect("task");
        let app = super::router(crate::AppState::new(db.clone(), None));
        let statistics = |app: axum::Router| {
            let cookie = cookie.clone();
            async move {
                let response = app
                    .oneshot(
                        axum::http::Request::builder()
                            .uri("/api/timer/statistics")
                            .header("cookie", cookie)
                            .body(axum::body::Body::empty())
                            .expect("request"),
                    )
                    .await
                    .expect("oneshot");
                assert_eq!(response.status(), axum::http::StatusCode::OK);
                json_body(response).await
            }
        };

        let empty = statistics(app.clone()).await;
        assert_eq!(empty["best_day"], serde_json::Value::Null);
        assert_eq!(empty["total_pomodori"], 0);

        db.create_entry(&task_id, 30, "manual time is not a pomodoro")
            .expect("manual entry");
        let manual_only = statistics(app.clone()).await;
        assert_eq!(manual_only["best_day"], serde_json::Value::Null);

        db.create_entry_full(NewTimeEntry {
            task_id: Some(&task_id),
            started_at: &chrono::Utc::now().to_rfc3339(),
            minutes: 25,
            seconds: 25 * 60,
            note: "",
            kind: EntryKind::Pomodoro,
            interrupted: false,
            interrupt_reason: None,
            labels: &[],
            created_by: "admin",
            activity_id: None,
        })
        .expect("pomodoro entry");
        let logged = statistics(app).await;
        assert_eq!(logged["best_day"]["pomodori"], 1);
        assert_eq!(logged["longest_streak"], 1);
    }

    /// An empty "Today" header heads the Done column when nothing was done today.
    #[tokio::test]
    async fn done_column_should_always_start_with_a_today_header() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        let (status, _) = send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        assert!(status.is_success(), "swimlane created");
        let groups = |page: &str| -> Vec<String> {
            page.split("done-group-label\">")
                .skip(1)
                .filter_map(|rest| rest.split('<').next())
                .map(str::to_string)
                .collect()
        };
        let board = format!("/b/{}", fx.board_id);
        let (_, task) = create_task_json(&fx, serde_json::json!({ "name": "Older" })).await;
        let id = task["id"].as_str().expect("id").to_string();
        let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
        send(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(mv)).await;
        let yesterday = (Local::now().date_naive() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let set = serde_json::json!({ "grouping_date": yesterday });
        let (status, _) = send(&fx, "PATCH", &format!("/api/tasks/{id}"), Some(set)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(groups(&page), ["Today", "Yesterday"]);

        // A future grouping date gets its own group after Today, and today's
        // tasks keep the one Today header.
        for (name, date) in [("Future", Some(4)), ("Current", None)] {
            let (_, task) = create_task_json(&fx, serde_json::json!({ "name": name })).await;
            let id = task["id"].as_str().expect("id").to_string();
            let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
            send(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(mv)).await;
            if let Some(days) = date {
                let day = Local::now().date_naive() + Duration::days(days);
                let set =
                    serde_json::json!({ "grouping_date": day.format("%Y-%m-%d").to_string() });
                send(&fx, "PATCH", &format!("/api/tasks/{id}"), Some(set)).await;
            }
        }
        let future = (Local::now().date_naive() + Duration::days(4))
            .format("%A, %-d %B")
            .to_string();
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(groups(&page), ["Today", future.as_str(), "Yesterday"]);
    }

    /// An archiving column with no tasks has no day header, and the first task done today brings
    /// "Today".
    #[tokio::test]
    async fn empty_archiving_column_should_have_no_day_header() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        let (status, _) = send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        assert!(status.is_success(), "swimlane created");
        let board = format!("/b/{}", fx.board_id);
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(page.matches("done-group-label").count(), 0, "empty Done");
        let (_, task) = create_task_json(&fx, serde_json::json!({ "name": "Finished" })).await;
        let id = task["id"].as_str().expect("id").to_string();
        let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
        send(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(mv)).await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(page.matches("done-group-label\">Today<").count(), 1);
    }

    /// A task's grouping_date decides its Done-column group (newest group first), null and "" both
    /// clear it, and a non-date is a 400.
    #[tokio::test]
    async fn grouping_date_should_decide_the_done_group() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        let (status, _) = send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        assert!(status.is_success(), "swimlane created");
        let mut uris = Vec::new();
        for name in ["Older", "Newer"] {
            let (_, task) = create_task_json(&fx, serde_json::json!({ "name": name })).await;
            let id = task["id"].as_str().expect("id").to_string();
            let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
            let (status, _) = send(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(mv)).await;
            assert_eq!(status, axum::http::StatusCode::OK);
            uris.push(format!("/api/tasks/{id}"));
        }
        let groups = |page: &str| -> Vec<String> {
            page.split("done-group-label\">")
                .skip(1)
                .filter_map(|rest| rest.split('<').next())
                .map(str::to_string)
                .collect()
        };
        let board = format!("/b/{}", fx.board_id);
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(groups(&page), ["Today"]);

        let yesterday = (Local::now().date_naive() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let set = serde_json::json!({ "grouping_date": yesterday });
        let (status, _) = send(&fx, "PATCH", &uris[0], Some(set)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(groups(&page), ["Today", "Yesterday"]);
        let newer = page.find("Newer").expect("Newer card");
        let older = page.find("Older").expect("Older card");
        assert!(
            newer < older,
            "the grouped-yesterday card sorts after today's"
        );

        for clear in [serde_json::Value::Null, serde_json::json!("")] {
            let set = serde_json::json!({ "grouping_date": yesterday });
            send(&fx, "PATCH", &uris[0], Some(set)).await;
            let patch = serde_json::json!({ "grouping_date": clear });
            let (status, _) = send(&fx, "PATCH", &uris[0], Some(patch)).await;
            assert_eq!(status, axum::http::StatusCode::OK);
            let task = get_json(&fx, &uris[0]).await;
            assert_eq!(
                task["grouping_date"],
                serde_json::Value::Null,
                "{clear} clears"
            );
            let (_, page) = send(&fx, "GET", &board, None).await;
            assert_eq!(groups(&page), ["Today"]);
        }

        let bad = serde_json::json!({"grouping_date": "garbage"});
        let (status, _) = send(&fx, "PATCH", &uris[0], Some(bad)).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            get_json(&fx, &uris[0]).await["grouping_date"],
            serde_json::Value::Null
        );
    }

    /// Before /setup, the `CHIPFLOW_BOOTSTRAP_TOKEN` env token reaches the API (and still cannot mint
    /// tokens), a wrong token gets 401, and a request without a token is still sent to /setup.
    #[tokio::test]
    async fn env_token_should_work_before_setup() {
        use tower::ServiceExt;

        const ENV_TOKEN: &str = "cf_kf397_env_token_for_tests";
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::connect(
            dir.path()
                .join("kf397-test.redb")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("connect");
        assert_eq!(db.user_count().expect("count"), 0, "setup not done");
        let app = super::router(crate::AppState::new(
            db,
            crate::db::BootstrapToken::new(ENV_TOKEN),
        ));
        let request = |method: &str, uri: &str, token: Option<&str>| {
            let mut builder = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json");
            if let Some(token) = token {
                builder = builder.header("authorization", format!("Bearer {token}"));
            }
            let body = if method == "POST" {
                axum::body::Body::from(r#"{"name":"Headless","scopes":["read"]}"#)
            } else {
                axum::body::Body::empty()
            };
            builder.body(body).expect("request")
        };
        let status = |response: axum::response::Response| response.status();

        let boards = app
            .clone()
            .oneshot(request("GET", "/api/boards", Some(ENV_TOKEN)));
        assert_eq!(
            status(boards.await.expect("oneshot")),
            axum::http::StatusCode::OK
        );
        let created = app
            .clone()
            .oneshot(request("POST", "/api/boards", Some(ENV_TOKEN)));
        assert!(status(created.await.expect("oneshot")).is_success());
        let mint = app
            .clone()
            .oneshot(request("POST", "/api/v1/auth/tokens", Some(ENV_TOKEN)));
        assert_eq!(
            status(mint.await.expect("oneshot")),
            axum::http::StatusCode::FORBIDDEN
        );
        let wrong = app
            .clone()
            .oneshot(request("GET", "/api/boards", Some("cf_wrong")));
        assert_eq!(
            status(wrong.await.expect("oneshot")),
            axum::http::StatusCode::UNAUTHORIZED
        );
        let anonymous = app
            .oneshot(request("GET", "/api/boards", None))
            .await
            .expect("oneshot");
        assert_eq!(anonymous.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(anonymous.headers()["location"], "/setup");
    }

    /// Done drop indexes count on-screen order and land beside a neighbor from the moved task's own
    /// date group.
    #[test]
    fn done_drop_position_should_follow_the_on_screen_group() {
        use super::done_drop_position;

        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 16);
        let older = chrono::NaiveDate::from_ymd_opt(2026, 10, 2);
        let now = today.expect("valid date");

        // Board order without the moved task: A (older), C (today).
        // On screen: Today [C], 2 Oct [A].
        let siblings = [older, today];
        assert_eq!(
            done_drop_position(now, &siblings, today, 0.0, Some(0)),
            1,
            "above C"
        );
        assert_eq!(
            done_drop_position(now, &siblings, today, 1.0, Some(0)),
            2,
            "below C"
        );

        // Dropped among the older group: stays where it was.
        assert_eq!(done_drop_position(now, &siblings, today, 2.0, Some(0)), 0);

        // Arriving from another column with no same-group neighbor at the
        // drop point: top of its group.
        assert_eq!(done_drop_position(now, &siblings, today, 2.0, None), 1);
        assert_eq!(done_drop_position(now, &[], today, 3.0, None), 0);

        // Without grouping the on-screen and board orders agree.
        let same = [today, today, today];
        for at in 0..=3 {
            assert_eq!(done_drop_position(now, &same, today, at as f64, None), at);
        }
    }

    /// Dragging within a Done date group persists the order shown, with board order B, A (grouped
    /// to an older day), C.
    #[tokio::test]
    async fn done_drag_within_a_group_should_persist_on_screen_order() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        let (status, _) = send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        assert!(status.is_success(), "swimlane created");
        let mut ids = std::collections::HashMap::new();
        for name in ["C", "A", "B"] {
            let (_, task) = create_task_json(&fx, serde_json::json!({ "name": name })).await;
            let id = task["id"].as_str().expect("id").to_string();
            let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
            let (status, _) = send(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(mv)).await;
            assert_eq!(status, axum::http::StatusCode::OK);
            ids.insert(name, id);
        }
        let older = (Local::now().date_naive() - Duration::days(14))
            .format("%Y-%m-%d")
            .to_string();
        let set = serde_json::json!({ "grouping_date": older });
        let (status, _) = send(&fx, "PATCH", &format!("/api/tasks/{}", ids["A"]), Some(set)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let board = format!("/b/{}", fx.board_id);
        let shown = |page: &str| -> Vec<String> {
            let mut cards: Vec<(usize, &str)> = ["A", "B", "C"]
                .iter()
                .map(|name| {
                    let marker = format!("data-task-name=\"{name}\"");
                    (page.find(&marker).expect("card"), *name)
                })
                .collect();
            cards.sort();
            cards
                .into_iter()
                .map(|(_, name)| name.to_string())
                .collect()
        };
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(shown(&page), ["B", "C", "A"]);

        let drag = |name: &str, position: usize| {
            let uri = format!("/api/tasks/{}/move", ids[name]);
            let body = serde_json::json!({"column_id": fx.col_done, "position": position});
            (uri, body)
        };
        let (uri, body) = drag("B", 1);
        send(&fx, "POST", &uri, Some(body)).await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(shown(&page), ["C", "B", "A"], "B dropped below C");

        let (uri, body) = drag("B", 0);
        send(&fx, "POST", &uri, Some(body)).await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(shown(&page), ["B", "C", "A"], "B dropped back above C");

        // A card arriving from another column joins the top of Today.
        let (_, task) = create_task_json(&fx, serde_json::json!({"name": "D"})).await;
        let d = task["id"].as_str().expect("id").to_string();
        let mv = serde_json::json!({"column_id": fx.col_done, "position": 3});
        send(&fx, "POST", &format!("/api/tasks/{d}/move"), Some(mv)).await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        let d_at = page.find("data-task-name=\"D\"").expect("D card");
        let b_at = page.find("data-task-name=\"B\"").expect("B card");
        let a_at = page.find("data-task-name=\"A\"").expect("A card");
        assert!(
            d_at < b_at && b_at < a_at,
            "D tops Today, A stays in its group"
        );
    }

    /// Grouping_date must be zero-padded YYYY-MM-DD.
    #[tokio::test]
    async fn grouping_date_should_reject_unpadded_dates() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        for bad in ["2026-1-5", "2026-01-5", "26-01-05", "2026-01-05T00:00:00Z"] {
            let patch = serde_json::json!({ "grouping_date": bad });
            let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{bad}");
        }
        let patch = serde_json::json!({"grouping_date": "2026-01-05"});
        let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(get_json(&fx, &uri).await["grouping_date"], "2026-01-05");
    }

    /// Second 60 is rejected in local and RFC3339 forms whatever the zone, :59 still resolves, and
    /// unpadded local fields still parse.
    #[test]
    fn normalize_due_input_should_reject_second_sixty() {
        use super::normalize_due_input_in;

        let london = chrono_tz::Europe::London;
        let dublin = chrono_tz::Europe::Dublin;
        let utc = chrono_tz::UTC;
        for zone in [&utc, &london, &dublin] {
            for bad in [
                "2026-10-20 08:15:60",
                "2026-10-20T08:15:60",
                "2026-10-21 23:59:60",
                "2026-10-22T10:30:60+02:00",
                "2026-10-22T10:30:60Z",
                "2026-10-25 01:59:60",
            ] {
                let got = normalize_due_input_in(bad, zone);
                assert!(got.is_err(), "{bad} in {zone}: {got:?}");
            }
        }
        let ok = |raw: &str, zone: &chrono_tz::Tz| normalize_due_input_in(raw, zone).expect(raw);
        assert_eq!(ok("2026-10-20 08:15:59", &utc), "2026-10-20T08:15:59+00:00");
        assert_eq!(
            ok("2026-10-20 08:15:59", &london),
            "2026-10-20T08:15:59+01:00"
        );
        assert_eq!(
            ok("2026-10-22T10:30:59+02:00", &london),
            "2026-10-22T10:30:59+02:00"
        );
        assert_eq!(
            due_as_utc(normalize_due_input_in("2026-10-25 01:59:59", &dublin)).as_deref(),
            Some("2026-10-25T00:59:59Z"),
            "the repeated second resolves to the earlier instant"
        );
        assert_eq!(ok("2026-1-5", &utc), "2026-01-05T17:00:00+00:00");
        assert_eq!(ok("2026-10-20 8:5:7", &utc), "2026-10-20T08:05:07+00:00");
    }

    /// POST and PATCH reject a ":60" due_at with 400.
    #[tokio::test]
    async fn task_api_should_reject_second_sixty() {
        let (_dir, fx) = kf221_fixture();
        for bad in ["2026-10-20 08:15:60", "2026-10-22T10:30:60+02:00"] {
            let (status, _) =
                create_task_json(&fx, serde_json::json!({"name": "Leap", "due_at": bad})).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "POST {bad}");
        }
        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        for bad in ["2026-10-21 23:59:60", "2026-10-22T10:30:60+02:00"] {
            let patch = serde_json::json!({ "due_at": bad });
            let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "PATCH {bad}");
        }
        assert_eq!(get_json(&fx, &uri).await["due_at"], serde_json::Value::Null);
        let patch = serde_json::json!({"due_at": "2026-10-22T10:30:59+02:00"});
        let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            get_json(&fx, &uri).await["due_at"],
            "2026-10-22T10:30:59+02:00"
        );
    }

    /// A cross-board drop into a Done column without a swimlane is translated in the target board's
    /// first lane, where the task lands.
    #[tokio::test]
    async fn cross_board_done_drop_should_use_the_target_boards_lane() {
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("xboard.redb");
        let db = crate::db::Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let user_id = db
            .create_user(
                "admin",
                &crate::auth::hash_password("s3cret").expect("hash"),
            )
            .expect("user");
        let cookie = format!(
            "session={}",
            crate::auth::create_session(&db, &user_id).expect("session")
        );
        let from = db.create_board("From").expect("from board");
        let to = db.create_board("To").expect("to board");
        db.ensure_default_swimlane(&from).expect("from lane");
        db.ensure_default_swimlane(&to).expect("to lane");
        let from_lane = db.list_swimlanes(&from).expect("lanes")[0].id.clone();
        let to_lane = db.list_swimlanes(&to).expect("lanes")[0].id.clone();
        let from_col = db.create_column(&from, "Doing", None).expect("column");
        let to_todo = db.create_column(&to, "Todo", None).expect("column");
        let to_done = db.create_column(&to, "Done", None).expect("column");
        db.set_column_done(&to_done, true).expect("done column");
        let older = (Local::now().date_naive() - Duration::days(14))
            .format("%Y-%m-%d")
            .to_string();

        // Target Done cell in board order: Y (grouped older), X (today).
        for (name, grouping) in [("X", None), ("Y", Some(older.as_str()))] {
            let id = db
                .create_task(&to_todo, Some(&to_lane), name, 1, None)
                .expect("task");
            db.move_task(&id, &to_done, Some(&to_lane), 0.0)
                .expect("into done");
            db.set_grouping_date(&id, grouping).expect("grouping");
        }
        let z = db
            .create_task(&from_col, Some(&from_lane), "Z", 1, None)
            .expect("task");
        let app = super::router(crate::AppState::new(db.clone(), None));

        // On screen: Today [X], older [Y]; drop Z below X (index 1).
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/tasks/{z}/move"))
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({"column_id": to_done, "position": 1}).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let order: Vec<String> = db
            .board_tasks(&to)
            .expect("tasks")
            .into_iter()
            .filter(|t| t.column_id == to_done)
            .map(|t| t.name)
            .collect();
        assert_eq!(order, ["Y", "X", "Z"], "Z lands below X in Today");
        let moved = db.get_task(&z).expect("get").expect("task");
        assert_eq!(moved.swimlane_id.as_deref(), Some(to_lane.as_str()));
    }

    /// A collapsed column's header holds only the count and the warning line (overdue here), and
    /// its body cell carries the name.
    #[tokio::test]
    async fn collapsed_column_should_render_count_header_and_body_name() {
        use tower::ServiceExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::connect(
            dir.path()
                .join("kf223-test.redb")
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
        let board_id = db.create_board("KF-223").expect("board");
        db.ensure_default_swimlane(&board_id).expect("lane");
        let lane = db.list_swimlanes(&board_id).expect("lanes")[0].id.clone();
        let col = db.create_column(&board_id, "Queue", None).expect("column");
        db.create_column(&board_id, "Open", None).expect("column");
        let task = db
            .create_task(&col, Some(&lane), "Late", 0, None)
            .expect("task");
        db.set_task_due(&task, Some("2020-01-01T09:00:00Z"), None)
            .expect("due");
        db.set_column_collapsed(&col, true).expect("collapse");
        let app = super::router(crate::AppState::new(db, None));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/b/{board_id}"))
                    .header("cookie", format!("session={token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8");

        let header_start = html
            .find("columnHeader columnHeader--collapsed")
            .expect("collapsed header");
        let header_end = header_start + html[header_start..].find("</th>").expect("th end");
        let header = &html[header_start..header_end];
        assert!(header.contains("data-overdue-count=\"1\""));
        assert!(header.contains("columnHeader-warningLine"));
        assert!(
            !header.contains("columnHeader-overdue"),
            "no overdue text: {header}"
        );
        assert!(
            !header.contains("columnHeader-name"),
            "name left the header"
        );
        assert!(html.contains(&format!(
            "board-cell board-cell--collapsed\" data-column-id=\"{col}\""
        )));
        assert!(html.contains(
            "<span class=\"collapsedColumn-nameBox\"><span class=\"collapsedColumn-name\">Queue</span></span>"
        ));
        assert!(!html.contains("<span class=\"collapsedColumn-name\">Open</span>"));
    }

    /// Due_at needs a four-digit unsigned year and exactly one space or `T` between date and time,
    /// in every zone, and the documented unpadded fields still parse.
    #[test]
    fn normalize_due_input_should_require_four_digit_year_and_one_separator() {
        use super::normalize_due_input_in;

        let utc = chrono_tz::UTC;
        let dublin = chrono_tz::Europe::Dublin;
        for zone in [&utc, &dublin] {
            for bad in [
                "26-10-20",
                "026-10-20",
                "+2026-10-20",
                "-2026-10-20",
                "02026-10-20",
                "2026-10-20\t08:00",
                "2026-10-20  08:00",
                "2026-10-20 \t08:00",
                "2026-10-20 08: 00",
                "2026-10-20 008:00",
                "2026-010-20",
                "+2026-10-20T08:00:00Z",
                "026-10-20T08:00:00Z",
                "26-10-20 08:00",
            ] {
                let got = normalize_due_input_in(bad, zone);
                assert!(got.is_err(), "{bad:?} in {zone}: {got:?}");
            }
        }
        let ok = |raw: &str| normalize_due_input_in(raw, &utc).expect(raw);
        assert_eq!(ok("2026-1-5"), "2026-01-05T17:00:00+00:00");
        assert_eq!(ok("2026-10-20 8:5"), "2026-10-20T08:05:00+00:00");
        assert_eq!(ok("2026-10-20T8:5:7"), "2026-10-20T08:05:07+00:00");
        assert_eq!(ok("2026-10-20 08:00:00"), "2026-10-20T08:00:00+00:00");
        assert_eq!(ok(" 2026-10-20 "), "2026-10-20T17:00:00+00:00");
    }

    /// A date-only due date takes the board's Due date time, 5:00 PM by default. PUT
    /// /api/boards/{id}/config changes it and rejects anything but zero-padded 24-hour "HH:MM".
    #[tokio::test]
    async fn date_only_due_should_take_the_board_due_time() {
        let (_dir, fx) = kf221_fixture();
        let local_hm = |due: &serde_json::Value| {
            let due = DateTime::parse_from_rfc3339(due.as_str().expect("due string"))
                .expect("RFC3339")
                .with_timezone(&Local);
            (due.format("%Y-%m-%d").to_string(), due.hour(), due.minute())
        };
        let (status, task) =
            create_task_json(&fx, serde_json::json!({"due_at": "2026-10-20"})).await;
        assert_eq!(status, axum::http::StatusCode::CREATED);
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let detail = get_json(&fx, &uri).await;
        assert_eq!(
            local_hm(&detail["due_at"]),
            ("2026-10-20".to_string(), 17, 0)
        );

        let config = format!("/api/boards/{}/config", fx.board_id);
        for bad in ["8:30", "25:00", "08:30:00", "noon"] {
            let (status, _) = send(
                &fx,
                "PUT",
                &config,
                Some(serde_json::json!({ "due_time": bad })),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{bad}");
        }
        let (status, body) = send(
            &fx,
            "PUT",
            &config,
            Some(serde_json::json!({"due_time": "08:30"})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(body["due_time"], "08:30");

        let patch = serde_json::json!({"due_at": "2026-10-21"});
        let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let detail = get_json(&fx, &uri).await;
        assert_eq!(
            local_hm(&detail["due_at"]),
            ("2026-10-21".to_string(), 8, 30)
        );

        // A due date with a time keeps its own time.
        let patch = serde_json::json!({"due_at": "2026-10-22 19:15"});
        send(&fx, "PATCH", &uri, Some(patch)).await;
        let detail = get_json(&fx, &uri).await;
        assert_eq!(
            local_hm(&detail["due_at"]),
            ("2026-10-22".to_string(), 19, 15)
        );

        // The board page hands the time to the due dialog.
        let (_, page) = send(&fx, "GET", &format!("/b/{}", fx.board_id), None).await;
        assert!(page.contains("data-default-time=\"08:30\""));
        let (_, settings) = send(
            &fx,
            "GET",
            &format!("/b/{}/settings/task-settings", fx.board_id),
            None,
        )
        .await;
        assert!(settings.contains("id=\"due-time-label\">8:30 AM<"));
    }

    /// The General tab shows the name with an Edit control, the owner, and the Archive and Delete
    /// actions.
    #[tokio::test]
    async fn general_tab_should_list_name_owner_and_actions() {
        let (_dir, fx) = kf221_fixture();
        let (_, html) = send(&fx, "GET", &format!("/b/{}/settings", fx.board_id), None).await;
        for needle in [
            "id=\"board-name-edit\"",
            "kf-setting-label\">Board owner<",
            "<h2 class=\"kf-actions-title\">Actions</h2>",
            "id=\"board-archive-btn\"",
            "id=\"board-delete-btn\"",
        ] {
            assert!(html.contains(needle), "missing {needle}");
        }
    }

    /// The board settings pages and the delete page carry the global top bar (Boards dropdown with
    /// every board, account control).
    #[tokio::test]
    async fn settings_pages_should_render_the_global_top_bar() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        for path in ["settings", "settings/colors", "settings/delete"] {
            let response = fx
                .app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/b/{}/{path}", fx.board_id))
                        .header("cookie", &fx.cookie)
                        .body(axum::body::Body::empty())
                        .expect("request"),
                )
                .await
                .expect("oneshot");
            assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
            let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .expect("body");
            let html = String::from_utf8(body.to_vec()).expect("utf8");
            assert!(
                html.contains("id=\"boards-dropdown-btn\""),
                "{path}: Boards button"
            );
            assert!(
                html.contains(&format!("data-board-id=\"{}\"", fx.board_id))
                    && html.contains("bs-item bs-current"),
                "{path}: current board in the dropdown"
            );
            assert!(
                html.contains("id=\"account-btn\""),
                "{path}: account control"
            );
            assert!(
                html.contains("Signed in as <strong>admin</strong>"),
                "{path}: username"
            );

            // Every dialog closer uses the shared attribute app.js wires.
            assert!(!html.contains("data-close-dlg"), "{path}: stale closer");
        }
    }

    /// A due date's target column is stored per task, limited to the task's board, named in the
    /// card's parenthetical, and cleared by null and by clearing the due date. A due date past the
    /// column's 7-day window keeps its card icon but hides the due line.
    #[tokio::test]
    async fn due_target_column_should_name_the_parenthetical() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        let soon = (Local::now() + Duration::days(2))
            .format("%Y-%m-%d 17:00")
            .to_string();
        let (_, task) = create_task_json(&fx, serde_json::json!({ "due_at": soon })).await;
        let id = task["id"].as_str().expect("id").to_string();
        let uri = format!("/api/tasks/{id}");
        let card_uri = format!("/api/tasks/{id}/card");
        let rename = serde_json::json!({"name": "Shipped"});
        send(
            &fx,
            "PATCH",
            &format!("/api/columns/{}", fx.col_done),
            Some(rename),
        )
        .await;

        let target = serde_json::json!({ "due_target_column_id": fx.col_done });
        let (status, _) = send(&fx, "PATCH", &uri, Some(target)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            get_json(&fx, &uri).await["due_target_column_id"],
            fx.col_done.as_str()
        );
        let (_, card) = send(&fx, "GET", &card_uri, None).await;
        assert!(
            card.contains("<span class=\"task-dueDateColumn\">(Shipped)</span>"),
            "{card}"
        );

        let (_, other) = send(
            &fx,
            "POST",
            "/api/boards",
            Some(serde_json::json!({"name": "Other"})),
        )
        .await;
        let other: serde_json::Value = serde_json::from_str(&other).expect("json");
        let other_columns = get_json(
            &fx,
            &format!(
                "/api/boards/{}/columns",
                other["id"].as_str().expect("board id")
            ),
        )
        .await;
        let foreign = other_columns[0]["id"]
            .as_str()
            .expect("column id")
            .to_string();
        for bad in [foreign.as_str(), "no-such-column"] {
            let patch = serde_json::json!({ "due_target_column_id": bad });
            let (status, _) = send(&fx, "PATCH", &uri, Some(patch)).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(
            get_json(&fx, &uri).await["due_target_column_id"],
            fx.col_done.as_str()
        );

        let clear = serde_json::json!({"due_target_column_id": null});
        send(&fx, "PATCH", &uri, Some(clear)).await;
        assert_eq!(
            get_json(&fx, &uri).await["due_target_column_id"],
            serde_json::Value::Null
        );
        let (_, card) = send(&fx, "GET", &card_uri, None).await;
        assert!(
            !card.contains("task-dueDateColumn"),
            "no target, no parenthetical"
        );

        let target = serde_json::json!({ "due_target_column_id": fx.col_done });
        send(&fx, "PATCH", &uri, Some(target)).await;
        send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({"due_at": null})),
        )
        .await;
        let detail = get_json(&fx, &uri).await;
        assert_eq!(detail["due_target_column_id"], serde_json::Value::Null);

        let far = (Local::now() + Duration::days(40))
            .format("%Y-%m-%d 17:00")
            .to_string();
        send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({ "due_at": far })),
        )
        .await;
        let (_, card) = send(&fx, "GET", &card_uri, None).await;
        assert!(card.contains("data-sec=\"duedates\""), "due icon stays");
        assert!(!card.contains("card-dueline"), "line hidden past 7 days");
    }

    /// The modal renders KanbanFlow's plain due value: the date in bold, the target column in a
    /// normal-weight span, and no icon.
    #[tokio::test]
    async fn modal_due_value_should_bold_only_the_date() {
        let (_dir, fx) = kf221_fixture();
        let soon = (Local::now() + Duration::days(2))
            .format("%Y-%m-%d 17:00")
            .to_string();
        let (_, task) = create_task_json(&fx, serde_json::json!({ "due_at": soon })).await;
        let id = task["id"].as_str().expect("id").to_string();
        let target = serde_json::json!({ "due_target_column_id": fx.col_done });
        send(&fx, "PATCH", &format!("/api/tasks/{id}"), Some(target)).await;
        let (_, modal) = send(&fx, "GET", &format!("/api/tasks/{id}/modal"), None).await;
        let start = modal.find("tm-due-chip").expect("due value");
        let chip = &modal[start..start + modal[start..].find("</button>").expect("end")];
        assert!(chip.contains("<strong>"), "{chip}");
        assert!(
            chip.contains("5:00 PM</strong> <span class=\"tm-due-column\">(Done)</span>"),
            "{chip}"
        );
        assert!(!chip.contains("&#128197;"), "no calendar icon");
    }

    /// A date-only due whose board time a DST gap skips gets an error that names the board's due
    /// date time.
    #[test]
    fn date_only_due_in_a_dst_gap_should_name_the_board_time() {
        use super::normalize_due_input_at;

        let london = chrono_tz::Europe::London;
        let half_past_one = chrono::NaiveTime::from_hms_opt(1, 30, 0).expect("time");
        let err = normalize_due_input_at("2027-03-28", &london, half_past_one).expect_err("gap");
        assert!(err.contains("board's due date time (01:30)"), "{err}");
        assert!(err.contains("daylight-saving gap"), "{err}");
        let err =
            normalize_due_input_at("2027-03-28 01:30", &london, half_past_one).expect_err("gap");
        assert!(err.contains("that local time is skipped"), "{err}");
    }

    /// The board page offers one User filter option per user and renders each card's member ids for
    /// the client-side filter.
    #[tokio::test]
    async fn board_page_should_render_member_filter_options_and_card_members() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        let (_, members) = send(&fx, "GET", "/api/members", None).await;
        let members: serde_json::Value = serde_json::from_str(&members).expect("json");
        let admin = members[0]["id"].as_str().expect("member id").to_string();
        let (_, assigned) = create_task_json(&fx, serde_json::json!({"name": "Assigned"})).await;
        let uri = format!("/api/tasks/{}", assigned["id"].as_str().expect("id"));
        send(
            &fx,
            "PATCH",
            &uri,
            Some(serde_json::json!({ "member_ids": [admin] })),
        )
        .await;
        create_task_json(&fx, serde_json::json!({"name": "Unassigned"})).await;

        let (_, page) = send(&fx, "GET", &format!("/b/{}", fx.board_id), None).await;
        assert!(page.contains(&format!("<option value=\"user:{admin}\">admin</option>")));
        assert!(page.contains(&format!("data-member-ids=\"{admin}\"")));
        assert!(page.contains("data-member-ids=\"\""));
    }

    /// Task sorting orders a column, Column sum "Estimated hours" fills the header badge, and
    /// Archiving groups any column by date (on for the Done column until saved off).
    #[tokio::test]
    async fn column_sorting_sum_and_archiving_should_take_effect() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        for (name, hours) in [("Charlie", 2.0), ("alpha", 1.5), ("Bravo", 0.0)] {
            let body = serde_json::json!({ "name": name, "estimate_hours": hours });
            create_task_json(&fx, body).await;
        }
        let board = format!("/b/{}", fx.board_id);
        let order = |page: &str| -> Vec<String> {
            let mut found: Vec<(usize, &str)> = ["Charlie", "alpha", "Bravo"]
                .iter()
                .map(|n| {
                    (
                        page.find(&format!("data-task-name=\"{n}\"")).expect("card"),
                        *n,
                    )
                })
                .collect();
            found.sort();
            found.into_iter().map(|(_, n)| n.to_string()).collect()
        };
        let config = |cfg: serde_json::Value| serde_json::json!({ "config_json": cfg.to_string() });
        let wip_uri = format!("/api/columns/{}", fx.col_wip);

        // The header badge shows only on columns without a WIP limit.
        send(
            &fx,
            "PATCH",
            &wip_uri,
            Some(serde_json::json!({"wip_limit": null})),
        )
        .await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(order(&page), ["Charlie", "alpha", "Bravo"], "board order");

        let (status, _) = send(
            &fx,
            "PATCH",
            &wip_uri,
            Some(config(
                serde_json::json!({"sorting": "name", "column_sum": "hours"}),
            )),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(
            order(&page),
            ["alpha", "Bravo", "Charlie"],
            "sorted by name"
        );
        assert!(page.contains(
            "<span class=\"columnHeader-countBadge\" data-column-sum=\"hours\">3h 30m</span>"
        ));

        // The board recomputes the sum client-side from these minutes.
        assert!(
            page.contains("data-estimate-minutes=\"210\""),
            "header total"
        );
        for minutes in ["120", "90", "0"] {
            assert!(
                page.contains(&format!("data-estimate-minutes=\"{minutes}\"")),
                "card estimate {minutes}"
            );
        }

        // The empty Done column shows no day header.
        assert_eq!(
            page.matches("done-group-label").count(),
            0,
            "only Done archives yet, and it is empty"
        );

        let archive = config(serde_json::json!({"sorting": "none", "group_by_date": true}));
        send(&fx, "PATCH", &wip_uri, Some(archive)).await;
        let done_uri = format!("/api/columns/{}", fx.col_done);
        let (_, done) = create_task_json(&fx, serde_json::json!({"name": "Finished"})).await;
        let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
        let move_uri = format!("/api/tasks/{}/move", done["id"].as_str().expect("id"));
        send(&fx, "POST", &move_uri, Some(mv)).await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(
            page.matches("done-group-label").count(),
            2,
            "both columns archive"
        );
        assert!(
            !page.contains("columnHeader-countBadge\" data-column-sum"),
            "Column sum None shows no count on expanded headers"
        );
        let count = config(serde_json::json!({"column_sum": "count", "group_by_date": true}));
        send(&fx, "PATCH", &wip_uri, Some(count)).await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert!(page.contains(
            "<span class=\"columnHeader-countBadge\" data-column-sum=\"count\">3</span>"
        ));

        let off = config(serde_json::json!({"group_by_date": false}));
        send(&fx, "PATCH", &done_uri, Some(off)).await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert_eq!(
            page.matches("done-group-label").count(),
            1,
            "Done saved off"
        );
    }

    /// A task created in a column that is then renamed and moved to Done carries each event's
    /// column id, and the export lists the column's former name, so the cumulative flow chart keeps
    /// one band.
    #[tokio::test]
    async fn export_history_should_follow_column_renames() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({"name": "Traveller"})).await;
        let id = task["id"].as_str().expect("id").to_string();
        let rename = serde_json::json!({"name": "Reviewing"});
        send(
            &fx,
            "PATCH",
            &format!("/api/columns/{}", fx.col_wip),
            Some(rename),
        )
        .await;
        let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
        send(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(mv)).await;

        let data = get_json(&fx, &format!("/api/boards/{}/export-data", fx.board_id)).await;
        let column = data["columns"]
            .as_array()
            .expect("columns")
            .iter()
            .find(|c| c["id"] == fx.col_wip.as_str())
            .expect("renamed column")
            .clone();
        assert_eq!(column["name"], "Reviewing");
        assert_eq!(column["former_names"], serde_json::json!(["Wip"]));
        let history = data["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|t| t["id"] == id.as_str())
            .expect("task")["history"]
            .clone();
        let event = |kind: &str| {
            history
                .as_array()
                .expect("history")
                .iter()
                .find(|e| e["kind"] == kind)
                .expect("event")
                .clone()
        };
        assert_eq!(event("created")["detail"], "Task created in Wip");
        assert_eq!(event("created")["column_id"], fx.col_wip.as_str());
        assert_eq!(event("moved")["column_id"], fx.col_done.as_str());
    }

    /// The board's last column cannot be deleted.
    #[tokio::test]
    async fn delete_column_should_refuse_the_last_column() {
        let (_dir, fx) = kf221_fixture();
        let uri = |id: &str| format!("/api/columns/{id}");
        let (status, _) = send(&fx, "DELETE", &uri(&fx.col_wip), None).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let (status, body) = send(&fx, "DELETE", &uri(&fx.col_done), None).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.contains("last column"), "{body}");
        let columns = get_json(&fx, &format!("/api/boards/{}/columns", fx.board_id)).await;
        assert_eq!(columns.as_array().expect("columns").len(), 1);
    }

    /// Two concurrent deletes of a board's only two columns never leave it with none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_column_deletes_should_leave_one_column() {
        use tower::ServiceExt;

        let (_dir, fx) = kf221_fixture();
        for round in 0..40 {
            let board = fx.db.create_board(&format!("Race {round}")).expect("board");
            let first = fx.db.create_column(&board, "A", None).expect("column");
            let second = fx.db.create_column(&board, "B", None).expect("column");
            let delete = |id: String| {
                let app = fx.app.clone();
                let cookie = fx.cookie.clone();
                tokio::spawn(async move {
                    let request = axum::http::Request::builder()
                        .method("DELETE")
                        .uri(format!("/api/columns/{id}"))
                        .header("cookie", cookie)
                        .body(axum::body::Body::empty())
                        .expect("request");
                    app.oneshot(request).await.expect("oneshot").status()
                })
            };
            let (a, b) = (delete(first), delete(second));
            let statuses = [a.await.expect("a"), b.await.expect("b")];
            let left = fx.db.list_columns(&board).expect("columns").len();
            assert_eq!(left, 1, "round {round}: {statuses:?}");
            assert!(statuses.contains(&axum::http::StatusCode::BAD_REQUEST));
        }
    }

    /// Tasks created in a column before and after a rename keep that column's id on their history
    /// once the column is deleted, so the cumulative flow chart gives it one band
    /// (`tests/js/cumulative_flow.js` covers the chart side).
    #[tokio::test]
    async fn export_history_should_keep_deleted_renamed_column_id() {
        let (_dir, fx) = kf221_fixture();
        let column = serde_json::json!({"board_id": fx.board_id, "name": "Temp"});
        let (_, text) = send(&fx, "POST", "/api/columns", Some(column)).await;
        let temp: serde_json::Value = serde_json::from_str(&text).expect("column json");
        let temp = temp["id"].as_str().expect("column id").to_string();
        let in_temp = serde_json::json!({"name": "First", "column_id": temp});
        let (_, first) = create_task_json(&fx, in_temp).await;
        let rename = serde_json::json!({"name": "Temp2"});
        send(&fx, "PATCH", &format!("/api/columns/{temp}"), Some(rename)).await;
        let in_temp2 = serde_json::json!({"name": "Second", "column_id": temp});
        let (_, second) = create_task_json(&fx, in_temp2).await;
        let mut ids = Vec::new();
        for task in [&first, &second] {
            let id = task["id"].as_str().expect("id").to_string();
            let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
            send(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(mv)).await;
            ids.push(id);
        }
        let (status, _) = send(&fx, "DELETE", &format!("/api/columns/{temp}"), None).await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let data = get_json(&fx, &format!("/api/boards/{}/export-data", fx.board_id)).await;
        let columns = data["columns"].as_array().expect("columns");
        assert!(columns.iter().all(|c| c["id"] != temp.as_str()));
        let created: Vec<(String, String)> = data["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .filter(|t| ids.iter().any(|id| t["id"] == id.as_str()))
            .flat_map(|t| t["history"].as_array().expect("history").clone())
            .filter(|e| e["kind"] == "created")
            .map(|e| {
                let detail = e["detail"].as_str().expect("detail").to_string();
                let column = e["column_id"].as_str().expect("column id").to_string();
                (detail, column)
            })
            .collect();
        assert_eq!(created.len(), 2);
        for detail in ["Task created in Temp", "Task created in Temp2"] {
            assert!(
                created.contains(&(detail.to_string(), temp.clone())),
                "{detail} with the column id: {created:?}"
            );
        }
    }

    /// An archiving column renders its newest 20 tasks and hides the rest behind "Load more tasks",
    /// and a sorted column marks its task lists so manual drops snap back.
    #[tokio::test]
    async fn archiving_column_should_show_twenty_tasks_until_load_more() {
        let (_dir, fx) = kf221_fixture();
        let lane = serde_json::json!({"board_id": fx.board_id, "name": "Lane"});
        send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
        for n in 0..23 {
            let (_, task) =
                create_task_json(&fx, serde_json::json!({ "name": format!("Done {n}") })).await;
            let uri = format!("/api/tasks/{}/move", task["id"].as_str().expect("id"));
            let mv = serde_json::json!({"column_id": fx.col_done, "position": 0});
            send(&fx, "POST", &uri, Some(mv)).await;
        }
        let board = format!("/b/{}", fx.board_id);
        let (_, page) = send(&fx, "GET", &board, None).await;
        let cards = page.matches("<article class=\"task-card").count();
        let hidden = page.matches("title=\"Yellow\" hidden>").count();
        assert_eq!(cards, 23);
        assert_eq!(hidden, 3, "past the first 20");
        assert_eq!(page.matches("data-load-more").count(), 1);
        assert!(!page.contains("data-sorted=\"1\""));

        let sorted = serde_json::json!({"config_json": "{\"sorting\":\"name\"}"});
        send(
            &fx,
            "PATCH",
            &format!("/api/columns/{}", fx.col_wip),
            Some(sorted),
        )
        .await;
        let (_, page) = send(&fx, "GET", &board, None).await;
        assert!(page.contains("data-sorted=\"1\""));
    }

    /// Deleting the target column, or moving the task to another board, clears the task's due
    /// target instead of leaving a dangling id.
    #[tokio::test]
    async fn due_target_should_clear_when_its_column_goes_away() {
        let (_dir, fx) = kf221_fixture();
        let due = (Local::now() + Duration::days(2))
            .format("%Y-%m-%d 17:00")
            .to_string();
        let (_, task) = create_task_json(&fx, serde_json::json!({ "due_at": due })).await;
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let (_, extra) = send(
            &fx,
            "POST",
            "/api/columns",
            Some(serde_json::json!({"board_id": fx.board_id, "name": "Extra"})),
        )
        .await;
        let extra: serde_json::Value = serde_json::from_str(&extra).expect("json");
        let extra = extra["id"].as_str().expect("column id").to_string();

        let patch = serde_json::json!({ "due_target_column_id": extra });
        send(&fx, "PATCH", &uri, Some(patch)).await;
        let (status, _) = send(&fx, "DELETE", &format!("/api/columns/{extra}"), None).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let detail = get_json(&fx, &uri).await;
        assert_eq!(detail["due_target_column_id"], serde_json::Value::Null);
        let resend = serde_json::json!({ "due_target_column_id": detail["due_target_column_id"] });
        let (status, _) = send(&fx, "PATCH", &uri, Some(resend)).await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let patch = serde_json::json!({ "due_target_column_id": fx.col_done });
        send(&fx, "PATCH", &uri, Some(patch)).await;
        let (_, other) = send(
            &fx,
            "POST",
            "/api/boards",
            Some(serde_json::json!({"name": "Other"})),
        )
        .await;
        let other: serde_json::Value = serde_json::from_str(&other).expect("json");
        let columns = get_json(
            &fx,
            &format!("/api/boards/{}/columns", other["id"].as_str().expect("id")),
        )
        .await;
        let foreign = columns[0]["id"].as_str().expect("column id");
        let (status, text) = send(
            &fx,
            "POST",
            &format!("{uri}/move"),
            Some(serde_json::json!({ "column_id": foreign, "position": 0 })),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let detail = get_json(&fx, &uri).await;
        assert_eq!(detail["due_target_column_id"], serde_json::Value::Null);
        assert!(detail["due_at"].is_string(), "the due date itself stays");
    }

    /// A template keeps each task's due target column (by name) and the board's due date time.
    #[tokio::test]
    async fn template_should_keep_due_target_and_due_time() {
        let (_dir, fx) = kf221_fixture();
        let config = format!("/api/boards/{}/config", fx.board_id);
        send(
            &fx,
            "PUT",
            &config,
            Some(serde_json::json!({"due_time": "08:30"})),
        )
        .await;
        let due = (Local::now() + Duration::days(2))
            .format("%Y-%m-%d 17:00")
            .to_string();
        let (_, task) = create_task_json(&fx, serde_json::json!({ "due_at": due })).await;
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let patch = serde_json::json!({ "due_target_column_id": fx.col_done });
        send(&fx, "PATCH", &uri, Some(patch)).await;

        let (status, saved) = send(
            &fx,
            "POST",
            &format!("/api/boards/{}/save-as-template", fx.board_id),
            Some(serde_json::json!({"name": "With targets"})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{saved}");
        let saved: serde_json::Value = serde_json::from_str(&saved).expect("json");
        let (status, made) = send(
            &fx,
            "POST",
            "/api/boards",
            Some(serde_json::json!({"name": "Copy", "template_id": saved["id"]})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{made}");
        let made: serde_json::Value = serde_json::from_str(&made).expect("json");
        let copy = made["id"].as_str().expect("board id");

        let columns = get_json(&fx, &format!("/api/boards/{copy}/columns")).await;
        let done = columns
            .as_array()
            .expect("columns")
            .iter()
            .find(|column| column["name"] == "Done")
            .expect("Done column");
        let tasks = get_json(&fx, &format!("/api/boards/{copy}/tasks")).await;
        let tasks = tasks.as_array().expect("tasks");
        assert_eq!(tasks.len(), 1);
        let detail = get_json(
            &fx,
            &format!("/api/tasks/{}", tasks[0]["id"].as_str().expect("id")),
        )
        .await;
        assert_eq!(detail["due_target_column_id"], done["id"]);
        let (_, view) = send(
            &fx,
            "PUT",
            &format!("/api/boards/{copy}/config"),
            Some(serde_json::json!({})),
        )
        .await;
        let view: serde_json::Value = serde_json::from_str(&view).expect("json");
        assert_eq!(view["due_time"], "08:30");
    }

    /// A target-only due change logs the target, not a new date.
    #[tokio::test]
    async fn target_only_due_change_should_log_the_target_column() {
        let (_dir, fx) = kf221_fixture();
        let due = (Local::now() + Duration::days(2))
            .format("%Y-%m-%d 17:00")
            .to_string();
        let (_, task) = create_task_json(&fx, serde_json::json!({ "due_at": due })).await;
        let id = task["id"].as_str().expect("id").to_string();
        let uri = format!("/api/tasks/{id}");
        let patch = serde_json::json!({ "due_target_column_id": fx.col_done });
        send(&fx, "PATCH", &uri, Some(patch)).await;
        let (_, history) = send(&fx, "GET", &format!("{uri}/history-view"), None).await;
        assert!(
            history.contains("Set the due date target column to Done"),
            "{history}"
        );
        assert!(!history.contains("Set the due date to"), "{history}");
        let clear = serde_json::json!({"due_target_column_id": null});
        send(&fx, "PATCH", &uri, Some(clear)).await;
        let (_, history) = send(&fx, "GET", &format!("{uri}/history-view"), None).await;
        assert!(
            history.contains("Cleared the due date target column"),
            "{history}"
        );

        // Both at once log both lines.
        let both = serde_json::json!({
            "due_at": (Local::now() + Duration::days(3)).format("%Y-%m-%d 17:00").to_string(),
            "due_target_column_id": fx.col_done,
        });
        send(&fx, "PATCH", &uri, Some(both)).await;
        let (_, history) = send(&fx, "GET", &format!("{uri}/history-view"), None).await;
        assert_eq!(
            history.matches("Set the due date to").count(),
            1,
            "{history}"
        );
        assert_eq!(
            history
                .matches("Set the due date target column to Done")
                .count(),
            2,
            "{history}"
        );
    }

    /// `GET /api/tasks?board_id=X` lists only X's tasks, each carrying its board id, and an unknown
    /// board is a 404.
    #[tokio::test]
    async fn task_list_should_filter_by_board_id() {
        let (_dir, fx) = kf221_fixture();
        create_task_json(&fx, serde_json::json!({"name": "Here"})).await;
        let (_, other) = send(
            &fx,
            "POST",
            "/api/boards",
            Some(serde_json::json!({"name": "Other"})),
        )
        .await;
        let other: serde_json::Value = serde_json::from_str(&other).expect("json");
        let other = other["id"].as_str().expect("board id").to_string();
        let columns = get_json(&fx, &format!("/api/boards/{other}/columns")).await;
        let (status, _) = send(
            &fx,
            "POST",
            "/api/tasks",
            Some(serde_json::json!({"name": "There", "column_id": columns[0]["id"]})),
        )
        .await;
        assert!(status.is_success());

        let all = get_json(&fx, "/api/tasks").await;
        let mut seen = std::collections::HashSet::new();
        for task in all.as_array().expect("list") {
            seen.insert(task["board_id"].as_str().expect("board_id").to_string());
        }
        assert!(seen.len() >= 2, "unfiltered list spans boards");
        for (board, name) in [(fx.board_id.as_str(), "Here"), (other.as_str(), "There")] {
            let listed = get_json(&fx, &format!("/api/tasks?board_id={board}")).await;
            let listed = listed.as_array().expect("list");
            assert!(listed.iter().any(|task| task["name"] == name), "{board}");
            assert!(
                listed.iter().all(|task| task["board_id"] == board),
                "{board}"
            );
        }
        let (status, text) = send(&fx, "GET", "/api/tasks?board_id=no-such-board", None).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(text, "Board not found");
        let everything = get_json(&fx, "/api/tasks?board_id=").await;
        assert_eq!(everything, all, "an empty board_id is no filter");
    }

    /// Archiving the last unarchived board is a 400 and changes nothing. With another active board
    /// it succeeds, and unarchiving restores the first.
    #[tokio::test]
    async fn archiving_the_last_active_board_should_be_refused() {
        let (_dir, fx) = kf221_fixture();
        let boards = get_json(&fx, "/api/boards").await;
        let ids: Vec<String> = boards
            .as_array()
            .expect("boards")
            .iter()
            .map(|board| board["id"].as_str().expect("id").to_string())
            .collect();
        let archive = serde_json::json!({"archived": true});
        let (last, rest) = ids.split_last().expect("a board");
        for id in rest {
            let (status, _) = send(
                &fx,
                "PUT",
                &format!("/api/boards/{id}/config"),
                Some(archive.clone()),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::OK, "{id}");
        }
        let uri = format!("/api/boards/{last}/config");
        let (status, text) = send(&fx, "PUT", &uri, Some(archive.clone())).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{text}");
        assert!(text.contains("last active board"), "{text}");
        let listed = get_json(&fx, "/api/boards").await;
        assert_eq!(listed.as_array().expect("boards").len(), 1);

        let first = &rest[0];
        let unarchive = serde_json::json!({"archived": false});
        send(
            &fx,
            "PUT",
            &format!("/api/boards/{first}/config"),
            Some(unarchive),
        )
        .await;
        let (status, _) = send(&fx, "PUT", &uri, Some(archive)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    /// Racing a colour delete against a task colour update for 100 rounds never leaves a task on a
    /// deleted colour.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn task_color_update_should_never_store_a_deleted_color() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let task_uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let colors_uri = format!("/api/boards/{}/colors", fx.board_id);
        let fx = std::sync::Arc::new(fx);
        let seeded = get_json(&fx, &colors_uri).await;
        if let Some(cyan) = seeded
            .as_array()
            .expect("colors")
            .iter()
            .find(|c| c["value"] == "cyan")
        {
            let uri = format!("{colors_uri}/{}", cyan["id"].as_str().expect("id"));
            send(&fx, "DELETE", &uri, None).await;
        }
        for round in 0..100 {
            let (_, made) = send(
                &fx,
                "POST",
                &colors_uri,
                Some(serde_json::json!({"value": "cyan"})),
            )
            .await;
            let made: serde_json::Value = serde_json::from_str(&made).expect("json");
            let color = made["id"].as_str().expect("color id").to_string();
            let update = {
                let (fx, uri, color) = (fx.clone(), task_uri.clone(), color.clone());
                tokio::spawn(async move {
                    let patch = serde_json::json!({ "color_id": color });
                    send(&fx, "PATCH", &uri, Some(patch)).await.0
                })
            };
            let delete = {
                let (fx, uri) = (fx.clone(), format!("{colors_uri}/{color}"));
                tokio::spawn(async move { send(&fx, "DELETE", &uri, None).await.0 })
            };
            let (update, delete) = (update.await.expect("update"), delete.await.expect("delete"));
            let stored = get_json(&fx, &task_uri).await["color_id"].clone();
            let listed = get_json(&fx, &colors_uri).await;
            if let Some(stored) = stored.as_str() {
                assert!(
                    listed
                        .as_array()
                        .expect("colors")
                        .iter()
                        .any(|c| c["id"] == stored),
                    "round {round}: task holds deleted colour {stored} (update {update}, delete {delete})"
                );
            }
            send(
                &fx,
                "PATCH",
                &task_uri,
                Some(serde_json::json!({"color_id": ""})),
            )
            .await;
            send(&fx, "DELETE", &format!("{colors_uri}/{color}"), None).await;
        }
    }

    /// The task detail carries `color_id` and `completed_at`.
    #[tokio::test]
    async fn task_detail_should_include_color_id_and_completed_at() {
        let (_dir, fx) = kf221_fixture();
        let colors = get_json(&fx, &format!("/api/boards/{}/colors", fx.board_id)).await;
        let blue = colors
            .as_array()
            .expect("colors")
            .iter()
            .find(|color| color["value"] == "blue")
            .expect("blue")["id"]
            .clone();
        let (_, task) = create_task_json(&fx, serde_json::json!({"color_id": blue})).await;
        let uri = format!("/api/tasks/{}", task["id"].as_str().expect("id"));
        let detail = get_json(&fx, &uri).await;
        assert_eq!(detail["color_id"], blue);
        assert_eq!(detail["completed_at"], serde_json::Value::Null);

        let body = serde_json::json!({"column_id": fx.col_done, "position": 0});
        let (status, _) = send(&fx, "POST", &format!("{uri}/move"), Some(body)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let detail = get_json(&fx, &uri).await;
        assert!(detail["completed_at"].is_string(), "{detail}");
    }

    /// A task moved out of a column whose done flag was turned off is not complete: `completed_at`
    /// clears and its past-due subtask renders overdue.
    #[tokio::test]
    async fn move_out_of_formerly_done_column_should_clear_completed_at() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let id = task["id"].as_str().expect("id").to_string();
        let uri = format!("/api/tasks/{id}");
        let sub = serde_json::json!({"name": "Late", "due_at": "2020-01-01"});
        send(&fx, "POST", &format!("{uri}/subtasks"), Some(sub)).await;

        let into = serde_json::json!({"column_id": fx.col_done, "position": 0});
        send(&fx, "POST", &format!("{uri}/move"), Some(into)).await;
        assert!(get_json(&fx, &uri).await["completed_at"].is_string());

        let off = serde_json::json!({"is_done": false});
        let (status, _) = send(
            &fx,
            "PATCH",
            &format!("/api/columns/{}", fx.col_done),
            Some(off),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let out = serde_json::json!({"column_id": fx.col_wip, "position": 0});
        send(&fx, "POST", &format!("{uri}/move"), Some(out)).await;

        assert_eq!(
            get_json(&fx, &uri).await["completed_at"],
            serde_json::Value::Null
        );
        let (_, modal) = send(&fx, "GET", &format!("{uri}/modal"), None).await;
        let start = modal.find("tm-subtask-due").expect("subtask due");
        let span = &modal[start..start + modal[start..].find("</span>").expect("end")];
        assert!(span.contains("tm-due-overdue"), "{span}");
    }

    /// Toggling a column's done flag re-stamps `completed_at` on the tasks it holds, so a task that
    /// never moves stays consistent.
    #[tokio::test]
    async fn toggling_column_done_should_restamp_completed_at_on_its_tasks() {
        let (_dir, fx) = kf221_fixture();
        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let id = task["id"].as_str().expect("id").to_string();
        let uri = format!("/api/tasks/{id}");
        let into = serde_json::json!({"column_id": fx.col_done, "position": 0});
        send(&fx, "POST", &format!("{uri}/move"), Some(into)).await;
        assert!(get_json(&fx, &uri).await["completed_at"].is_string());

        let column = format!("/api/columns/{}", fx.col_done);
        send(
            &fx,
            "PATCH",
            &column,
            Some(serde_json::json!({"is_done": false})),
        )
        .await;
        assert_eq!(
            get_json(&fx, &uri).await["completed_at"],
            serde_json::Value::Null
        );
        send(
            &fx,
            "PATCH",
            &column,
            Some(serde_json::json!({"is_done": true})),
        )
        .await;
        assert!(get_json(&fx, &uri).await["completed_at"].is_string());
    }

    /// Moving between two done columns keeps the completion stamp, and moving out of a still-done
    /// column clears it.
    #[test]
    fn move_task_should_keep_stamp_between_done_columns_and_clear_it_leaving() {
        let (_dir, fx) = kf221_fixture();
        let second = fx
            .db
            .create_column(&fx.board_id, "Archive", None)
            .expect("column");
        fx.db.set_column_done(&second, true).expect("done");
        let id = fx
            .db
            .create_task(&fx.col_wip, None, "T", 1, None)
            .expect("task");
        fx.db
            .move_task(&id, &fx.col_done, None, 0.0)
            .expect("into done");
        let stamp = fx.completed_at(&id);
        assert!(stamp.is_some());
        fx.db
            .move_task(&id, &second, None, 0.0)
            .expect("between done");
        let kept = fx.completed_at(&id);
        assert_eq!(kept, stamp);
        fx.db.move_task(&id, &fx.col_wip, None, 0.0).expect("out");
        assert!(fx.completed_at(&id).is_none());
    }

    /// A stop naming session A leaves session B running even when B started in the same second as
    /// A.
    #[tokio::test]
    async fn stale_stop_should_not_stop_a_session_started_in_the_same_second() {
        let (_dir, fx) = kf221_fixture();
        let start = serde_json::json!({"mode": "stopwatch"});
        let (_, first) = send(&fx, "POST", "/api/timer/start", Some(start.clone())).await;
        let first: serde_json::Value = serde_json::from_str(&first).expect("json");
        let (_, second) = send(&fx, "POST", "/api/timer/start", Some(start)).await;
        let second: serde_json::Value = serde_json::from_str(&second).expect("json");
        assert_ne!(first["session_id"], second["session_id"]);
        assert!(first["session_id"].is_u64());

        let stale = serde_json::json!({"session_id": first["session_id"]});
        let (status, body) = send(&fx, "POST", "/api/timer/stop", Some(stale)).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(body["already_stopped"], true);
        let status = get_json(&fx, "/api/timer/status").await;
        assert_eq!(status["active"], true);
        assert_eq!(status["started_at"], second["started_at"]);
        assert_eq!(status["session_id"], second["session_id"]);

        let current = serde_json::json!({"session_id": second["session_id"]});
        let (_, body) = send(&fx, "POST", "/api/timer/stop", Some(current)).await;
        let body: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(body["already_stopped"], false);
    }

    /// A burst of starts keeps started_at at the real second, so the countdown never exceeds the
    /// duration and a later session is not discarded as too short.
    #[tokio::test]
    async fn rapid_timer_starts_should_not_run_ahead_of_the_clock() {
        let (_dir, fx) = kf221_fixture();
        let start = serde_json::json!({"mode": "pomodoro"});
        let mut last = serde_json::Value::Null;
        for _ in 0..40 {
            let (_, body) = send(&fx, "POST", "/api/timer/start", Some(start.clone())).await;
            last = serde_json::from_str(&body).expect("json");
        }
        let now = chrono::Utc::now().timestamp();
        assert!(last["started_at"].as_i64().expect("started_at") <= now);
        assert!(last["remaining_seconds"].as_u64() <= last["total_seconds"].as_u64());
        assert!(last["elapsed_seconds"].as_i64().expect("elapsed") >= 0);

        let timer = crate::models::ActiveTimer {
            task_id: None,
            mode: crate::models::TimerMode::Stopwatch,
            started_at: now - 22,
            duration_secs: None,
        };
        fx.db.set_active_timer(&timer).expect("timer");
        let (_, body) = send(&fx, "POST", "/api/timer/stop", Some(serde_json::json!({}))).await;
        let body: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(body["discarded"], false, "{body}");
    }

    /// A session id from before a restart is unknown to the new process, so a stop naming it falls
    /// back to started_at matching.
    #[tokio::test]
    async fn stop_should_ignore_a_session_id_this_process_never_issued() {
        let (_dir, fx) = kf221_fixture();
        let started_at = chrono::Utc::now().timestamp() - 30;
        let timer = crate::models::ActiveTimer {
            task_id: None,
            mode: crate::models::TimerMode::Stopwatch,
            started_at,
            duration_secs: None,
        };
        fx.db.set_active_timer(&timer).expect("timer");
        let stop = serde_json::json!({"session_id": 7, "started_at": started_at});
        let (_, body) = send(&fx, "POST", "/api/timer/stop", Some(stop)).await;
        let body: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(body["already_stopped"], false, "{body}");
    }

    /// Editing a task-less time entry without a task keeps it task-less, and a task entry still
    /// needs its task.
    #[tokio::test]
    async fn editing_a_task_less_entry_should_keep_it_task_less() {
        let (_dir, fx) = kf221_fixture();
        let started = (chrono::Utc::now() - Duration::days(1)).to_rfc3339();
        let entry = fx
            .db
            .create_entry_full(NewTimeEntry {
                task_id: None,
                started_at: &started,
                minutes: 5,
                seconds: 300,
                note: "",
                kind: EntryKind::Stopwatch,
                interrupted: false,
                interrupt_reason: None,
                labels: &[],
                created_by: "a",
                activity_id: None,
            })
            .expect("entry")
            .expect("id");
        let uri = format!("/api/time/entries/{entry}");
        let day = (Local::now() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let edit = serde_json::json!({"date": day, "from": "09:00", "to": "09:30", "note": "x"});
        let (status, text) = send(&fx, "PUT", &uri, Some(edit.clone())).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let detail = get_json(&fx, &uri).await;
        assert_eq!(detail["task_id"], "");
        assert_eq!(detail["minutes"], 30);

        let (_, task) = create_task_json(&fx, serde_json::json!({})).await;
        let mut assigned = edit.clone();
        assigned["task_id"] = task["id"].clone();
        let (status, text) = send(&fx, "PUT", &uri, Some(assigned)).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        assert_eq!(get_json(&fx, &uri).await["task_id"], task["id"]);

        let (status, _) = send(&fx, "PUT", &uri, Some(edit)).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }

    /// A task-less short break with a break activity appears in the timer log and adds its minutes
    /// to that activity's statistics.
    #[tokio::test]
    async fn task_less_break_should_reach_the_log_and_break_statistics() {
        use crate::models::{ActiveTimer, BreakActivity, TimerMode};

        let (_dir, fx) = kf221_fixture();
        let mut settings = fx.db.get_settings().expect("settings");
        settings.break_activities = vec![BreakActivity {
            id: "act-1".to_string(),
            name: "Walk".to_string(),
            description: String::new(),
            daily_goal: 1,
            daily_limit: None,
        }];
        fx.db.update_settings(&settings).expect("settings");
        let before = get_json(&fx, "/api/timer/statistics").await["break_activities"][0]["minutes"]
            .as_i64()
            .expect("minutes");

        let timer = ActiveTimer {
            task_id: None,
            mode: TimerMode::ShortBreak,
            started_at: chrono::Utc::now().timestamp() - 300,
            duration_secs: Some(300),
        };
        fx.db.set_active_timer(&timer).expect("timer");
        let stop = serde_json::json!({"activity_id": "act-1"});
        let (status, text) = send(&fx, "POST", "/api/timer/stop", Some(stop)).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");

        let stats = get_json(&fx, "/api/timer/statistics").await;
        assert_eq!(stats["break_activities"][0]["minutes"], before + 5);
        let log = get_json(&fx, "/api/timer/log?entry_type=short_break").await;
        let entries = log["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 1, "{log}");
        assert_eq!(entries[0]["task_id"], "");
    }

    #[tokio::test]
    async fn create_column_and_swimlane_should_require_board_id() {
        let (_dir, fx) = kf221_fixture();
        let columns = fx.db.list_columns(&fx.board_id).expect("columns").len();
        let lanes = fx.db.list_swimlanes(&fx.board_id).expect("lanes").len();

        for (uri, body) in [
            ("/api/columns", serde_json::json!({"name": "Orphan"})),
            ("/api/swimlanes", serde_json::json!({"name": "Orphan"})),
            (
                "/api/columns",
                serde_json::json!({"board_id": "", "name": "Orphan"}),
            ),
        ] {
            let (status, text) = send(&fx, "POST", uri, Some(body)).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{uri}: {text}");
            assert!(text.contains("board_id"), "{uri}: {text}");
        }
        assert_eq!(
            fx.db.list_columns(&fx.board_id).expect("columns").len(),
            columns
        );
        assert_eq!(
            fx.db.list_swimlanes(&fx.board_id).expect("lanes").len(),
            lanes
        );

        let column = serde_json::json!({"board_id": fx.board_id, "name": "Kept"});
        let (status, _) = send(&fx, "POST", "/api/columns", Some(column)).await;
        assert!(status.is_success());
        assert_eq!(
            fx.db.list_columns(&fx.board_id).expect("columns").len(),
            columns + 1
        );
    }

    /// Sends one Bearer-authenticated request through a clone of `app`.
    async fn send_bearer(
        app: &axum::Router,
        method: &str,
        uri: &str,
        token: &str,
    ) -> (axum::http::StatusCode, String) {
        use tower::ServiceExt;

        let request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("oneshot");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn store_token(
        fx: &Kf221Fixture,
        name: &str,
        scope: AuthScope,
        expires_at: Option<i64>,
    ) -> (ApiTokenRecord, String) {
        let (record, raw) = auth::new_api_token_record(name, scope, expires_at);
        fx.db.create_api_token(&record).expect("store token");
        (record, raw)
    }

    fn ids_of(list: &serde_json::Value) -> Vec<serde_json::Value> {
        let items = list.as_array().expect("array");
        items.iter().map(|item| item["id"].clone()).collect()
    }

    #[tokio::test]
    async fn whoami_should_describe_each_credential_kind() {
        const ENV_TOKEN: &str = "cf_whoami_env_token_for_tests";
        let (_dir, fx) = kf221_fixture();
        let uri = "/api/v1/auth/whoami";
        let far_future = Utc::now().timestamp() + SECONDS_PER_DAY;

        let session = get_json(&fx, uri).await;
        assert_eq!(session["kind"], "session");
        assert_eq!(session["name"], "admin");
        assert_eq!(session["scope"], "admin");
        assert_eq!(session["expires_at"], serde_json::Value::Null);

        let (_, reader) = store_token(&fx, "reader-bot", AuthScope::Read, Some(far_future));
        let (status, text) = send_bearer(&fx.app, "GET", uri, &reader).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let reader_view: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(reader_view["kind"], "token");
        assert_eq!(reader_view["name"], "reader-bot");
        assert_eq!(reader_view["scope"], "read");
        assert_eq!(reader_view["expires_at"], far_future);

        let (writer_record, writer) = store_token(&fx, "writer-bot", AuthScope::Admin, None);
        let (_, text) = send_bearer(&fx.app, "GET", uri, &writer).await;
        let writer_view: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(writer_view["scope"], "admin");
        assert_eq!(writer_view["expires_at"], serde_json::Value::Null);

        let bootstrap_app = router(AppState::new(fx.db.clone(), BootstrapToken::new(ENV_TOKEN)));
        let (status, text) = send_bearer(&bootstrap_app, "GET", uri, ENV_TOKEN).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let bootstrap_view: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(bootstrap_view["kind"], "bootstrap");
        assert_eq!(bootstrap_view["name"], "bootstrap");
        assert_eq!(bootstrap_view["scope"], "admin");
        assert_eq!(bootstrap_view["expires_at"], serde_json::Value::Null);

        fx.db
            .delete_api_token(&writer_record.id)
            .expect("revoke token");
        let (status, _) = send_bearer(&fx.app, "GET", uri, &writer).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED, "revoked");

        let past = Utc::now().timestamp() - SECONDS_PER_MINUTE;
        let (_, expired) = store_token(&fx, "expired-bot", AuthScope::Read, Some(past));
        let (status, _) = send_bearer(&fx.app, "GET", uri, &expired).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED, "expired");

        let (status, _) = send_bearer(&fx.app, "GET", uri, "cf_unknown").await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn get_board_should_list_ordered_columns_and_swimlanes_matching_export_data() {
        let (_dir, fx) = kf221_fixture();
        for name in ["Second lane", "Third lane"] {
            let lane = serde_json::json!({"board_id": fx.board_id, "name": name});
            let (status, _) = send(&fx, "POST", "/api/swimlanes", Some(lane)).await;
            assert!(status.is_success(), "swimlane created");
        }
        let (_, reader) = store_token(&fx, "reader", AuthScope::Read, None);

        let uri = format!("/api/boards/{}", fx.board_id);
        let (status, text) = send_bearer(&fx.app, "GET", &uri, &reader).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{text}");
        let board: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(board["id"], fx.board_id.as_str());
        assert_eq!(board["name"], "KF-221");
        assert_eq!(board["columns"][0]["name"], "Wip");
        assert_eq!(board["columns"][0]["wip_limit"], 3);
        assert_eq!(board["columns"][0]["is_done"], false);
        assert_eq!(board["columns"][1]["name"], "Done");
        assert_eq!(board["columns"][1]["wip_limit"], serde_json::Value::Null);
        assert_eq!(board["columns"][1]["is_done"], true);

        let export = get_json(&fx, &format!("/api/boards/{}/export-data", fx.board_id)).await;
        assert_eq!(board["swimlanes"], export["swimlanes"]);
        assert_eq!(board["swimlanes"].as_array().expect("lanes").len(), 2);
        assert_eq!(ids_of(&board["columns"]), ids_of(&export["columns"]));

        assert_eq!(get_json(&fx, &uri).await, board);
        let (status, _) = send(&fx, "GET", "/api/boards/no-such-board", None).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    }

    async fn raw_request(
        app: &axum::Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> (axum::http::StatusCode, axum::http::HeaderMap, String) {
        use tower::ServiceExt;

        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let body = match body {
            Some(text) => {
                builder = builder.header("content-type", "application/json");
                axum::body::Body::from(text.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("oneshot");
        let status = response.status();
        let response_headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            response_headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn is_json(headers: &axum::http::HeaderMap) -> bool {
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    }

    #[tokio::test]
    async fn bearer_task_routes_should_return_json_without_accept_and_cookie_routes_html() {
        let (_dir, fx) = kf221_fixture();
        let (_, token) = store_token(&fx, "writer", AuthScope::Admin, None);
        let bearer = format!("Bearer {token}");
        let auth = [("authorization", bearer.as_str())];
        let cookie = [("cookie", fx.cookie.as_str())];

        let create =
            serde_json::json!({"name": "Bearer task", "column_id": fx.col_wip}).to_string();
        let (status, headers, body) =
            raw_request(&fx.app, "POST", "/api/tasks", &auth, Some(&create)).await;
        assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
        assert!(is_json(&headers), "create is json");
        let created: serde_json::Value = serde_json::from_str(&body).expect("json");
        let id = created["id"].as_str().expect("id").to_string();
        assert_eq!(created["name"], "Bearer task");

        let (status, headers, body) = raw_request(
            &fx.app,
            "PATCH",
            &format!("/api/tasks/{id}"),
            &auth,
            Some(r#"{"name":"Renamed"}"#),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert!(is_json(&headers), "patch is json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).expect("json")["name"],
            "Renamed"
        );

        let move_body = serde_json::json!({"column_id": fx.col_done, "position": 0.0}).to_string();
        let (status, headers, body) = raw_request(
            &fx.app,
            "POST",
            &format!("/api/tasks/{id}/move"),
            &auth,
            Some(&move_body),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert!(is_json(&headers), "move is json");
        let moved: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(moved["id"], id);
        assert_eq!(moved["column_id"], fx.col_done);

        let time_uri = format!("/api/tasks/{id}/time");
        let (status, headers, body) = raw_request(
            &fx.app,
            "POST",
            &time_uri,
            &auth,
            Some(r#"{"minutes":15,"note":"n"}"#),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert!(is_json(&headers), "log time is json");
        let logged: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(logged[0]["minutes"], 15);
        assert_eq!(logged[0]["note"], "n");
        assert_eq!(logged[0]["kind"], "manual");
        assert!(
            !logged[0]["created_by"]
                .as_str()
                .expect("created_by")
                .is_empty()
        );

        let (status, headers, body) = raw_request(&fx.app, "GET", &time_uri, &auth, None).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(is_json(&headers), "get time is json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).expect("json"),
            logged
        );

        let (status, headers, body) = raw_request(&fx.app, "GET", &time_uri, &cookie, None).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(!is_json(&headers), "cookie time stays html");
        assert!(body.contains("<"), "{body}");

        let (_, headers, body) = raw_request(
            &fx.app,
            "POST",
            &time_uri,
            &cookie,
            Some(r#"{"minutes":5}"#),
        )
        .await;
        assert!(!is_json(&headers), "cookie log time stays html");
        assert!(body.contains("<"), "{body}");

        let (_, headers, body) =
            raw_request(&fx.app, "POST", "/api/tasks", &cookie, Some(&create)).await;
        assert!(!is_json(&headers), "cookie create stays html");
        assert!(body.contains("<"), "{body}");

        let (_, headers, _) = raw_request(
            &fx.app,
            "PATCH",
            &format!("/api/tasks/{id}"),
            &cookie,
            Some(r#"{"name":"Again"}"#),
        )
        .await;
        assert!(!is_json(&headers), "cookie patch stays html");

        let (_, headers, _) = raw_request(
            &fx.app,
            "POST",
            &format!("/api/tasks/{id}/move"),
            &cookie,
            Some(&serde_json::json!({"column_id": fx.col_wip, "position": 0.0}).to_string()),
        )
        .await;
        assert!(!is_json(&headers), "cookie move stays html");
    }

    fn error_json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap_or_else(|error| panic!("{error}: {body}"))
    }

    async fn assert_error(
        fx: &Kf221Fixture,
        token: &str,
        request: (&str, &str, Option<&str>),
        expected: (u16, &str, &str),
    ) {
        let (method, uri, body) = request;
        let (status, code, message) = expected;
        let bearer = format!("Bearer {token}");
        let headers = [("authorization", bearer.as_str())];
        let (actual, response_headers, text) =
            raw_request(&fx.app, method, uri, &headers, body).await;
        assert_eq!(actual.as_u16(), status, "{method} {uri}: {text}");
        assert!(is_json(&response_headers), "{method} {uri}");
        assert_eq!(
            error_json(&text),
            serde_json::json!({"code": code, "message": message}),
            "{method} {uri}"
        );
    }

    #[tokio::test]
    async fn bearer_errors_should_be_json_with_a_code_and_the_same_message() {
        let (_dir, fx) = kf221_fixture();
        let (_, writer) = store_token(&fx, "writer", AuthScope::Admin, None);
        let (_, reader) = store_token(&fx, "reader", AuthScope::Read, None);
        let blank = serde_json::json!({"name": " ", "column_id": fx.col_wip}).to_string();
        let fields = format!("/api/boards/{}/fields", fx.board_id);
        let field = r#"{"key":"points","label":"Points","kind":"number"}"#;
        let (status, _, _) = raw_request(
            &fx.app,
            "POST",
            &fields,
            &[("authorization", &format!("Bearer {writer}"))],
            Some(field),
        )
        .await;
        assert!(status.is_success());

        assert_error(
            &fx,
            &writer,
            ("POST", "/api/tasks", Some(&blank)),
            (400, "invalid_request", "task name is required"),
        )
        .await;
        assert_error(
            &fx,
            &writer,
            ("GET", "/api/tasks/nope/time", None),
            (404, "not_found", "task not found"),
        )
        .await;
        assert_error(
            &fx,
            &writer,
            ("POST", &fields, Some(field)),
            (409, "field_key_exists", "field key exists"),
        )
        .await;
        assert_error(
            &fx,
            &reader,
            ("POST", "/api/tasks", Some("{}")),
            (
                403,
                "insufficient_scope",
                "insufficient token scope: needs work",
            ),
        )
        .await;
        assert_error(
            &fx,
            &writer,
            (
                "POST",
                "/api/v1/auth/tokens",
                Some(r#"{"name":"x","scopes":["read"]}"#),
            ),
            (
                403,
                "session_required",
                "token management requires the browser session; API tokens cannot mint new tokens",
            ),
        )
        .await;
    }

    #[tokio::test]
    async fn api_401_should_carry_discovery_headers_and_tell_invalid_from_expired() {
        let (_dir, fx) = kf221_fixture();
        let past = Utc::now().timestamp() - SECONDS_PER_MINUTE;
        let (_, expired) = store_token(&fx, "old", AuthScope::Read, Some(past));
        let uri = "/api/v1/auth/whoami";
        let cases = [
            (Some("cf_unknown".to_string()), "token_invalid"),
            (Some(expired), "token_expired"),
            (None, "unauthenticated"),
        ];
        for (token, code) in cases {
            let bearer = token.map(|token| format!("Bearer {token}"));
            let headers: Vec<(&str, &str)> = bearer
                .iter()
                .map(|value| ("authorization", value.as_str()))
                .collect();
            let (status, response_headers, body) =
                raw_request(&fx.app, "GET", uri, &headers, None).await;
            assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED, "{code}");
            assert_eq!(response_headers["www-authenticate"], "Bearer");
            assert_eq!(response_headers["link"], "</llms.txt>; rel=\"help\"");
            let value = error_json(&body);
            assert_eq!(value["code"], code);
            assert_eq!(value["help"], "/llms.txt");
        }

        let stale = [("cookie", "session=stale")];
        let (status, headers, body) = raw_request(&fx.app, "GET", uri, &stale, None).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(headers["www-authenticate"], "Bearer");
        assert!(!is_json(&headers));
        assert_eq!(body, "unauthorized");
    }

    #[tokio::test]
    async fn unknown_api_paths_and_wrong_methods_should_answer_json_to_bearer_only() {
        let (_dir, fx) = kf221_fixture();
        let (_, writer) = store_token(&fx, "writer", AuthScope::Admin, None);
        let bearer = format!("Bearer {writer}");
        let auth = [("authorization", bearer.as_str())];

        let (status, headers, body) = raw_request(&fx.app, "GET", "/api/nope", &auth, None).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert!(is_json(&headers));
        let value = error_json(&body);
        assert_eq!(value["code"], "route_not_found");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("/api/v1/openapi.json")
        );

        let (status, headers, body) =
            raw_request(&fx.app, "DELETE", "/api/tasks", &auth, None).await;
        assert_eq!(status, axum::http::StatusCode::METHOD_NOT_ALLOWED);
        assert!(is_json(&headers));
        assert!(headers.contains_key("allow"));
        let value = error_json(&body);
        assert_eq!(value["code"], "method_not_allowed");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("/api/v1/openapi.json")
        );

        let cookie = [("cookie", fx.cookie.as_str())];
        let (status, headers, body) = raw_request(&fx.app, "GET", "/api/nope", &cookie, None).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert!(!is_json(&headers));
        assert_eq!(body, "");
        let (status, _, body) = raw_request(&fx.app, "DELETE", "/api/tasks", &cookie, None).await;
        assert_eq!(status, axum::http::StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body, "");
    }

    #[tokio::test]
    async fn cookie_errors_should_stay_plain_text() {
        let (_dir, fx) = kf221_fixture();
        let cookie = [("cookie", fx.cookie.as_str())];

        let (status, headers, body) = raw_request(
            &fx.app,
            "POST",
            "/api/tasks",
            &cookie,
            Some(&serde_json::json!({"name": " ", "column_id": fx.col_wip}).to_string()),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(!is_json(&headers));
        assert_eq!(body, "task name is required");

        let (status, headers, body) =
            raw_request(&fx.app, "GET", "/api/tasks/nope/time", &cookie, None).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert!(!is_json(&headers));
        assert_eq!(body, "task not found");
    }
    #[tokio::test]
    async fn wrong_method_should_authenticate_before_answering_405() {
        let (_dir, fx) = kf221_fixture();
        let past = Utc::now().timestamp() - SECONDS_PER_MINUTE;
        let (_, expired) = store_token(&fx, "old", AuthScope::Read, Some(past));
        let (_, writer) = store_token(&fx, "writer", AuthScope::Admin, None);
        let expired = format!("Bearer {expired}");
        let writer = format!("Bearer {writer}");

        for method in ["DELETE", "OPTIONS", "HEAD"] {
            let anonymous: [(&str, &str); 0] = [];
            for headers in [
                &anonymous[..],
                &[("cookie", "session=stale")],
                &[("authorization", "Bearer cf_unknown")],
                &[("authorization", expired.as_str())],
            ] {
                let (status, _, _) =
                    raw_request(&fx.app, method, "/api/tasks", headers, None).await;
                assert_eq!(
                    status,
                    axum::http::StatusCode::UNAUTHORIZED,
                    "{method} {headers:?}"
                );
            }
        }

        let (_, headers, body) = raw_request(
            &fx.app,
            "DELETE",
            "/api/tasks",
            &[("cookie", "session=stale")],
            None,
        )
        .await;
        assert!(!is_json(&headers));
        assert_eq!(body, "unauthorized");

        let (status, headers, body) = raw_request(
            &fx.app,
            "DELETE",
            "/api/tasks",
            &[("authorization", writer.as_str())],
            None,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::METHOD_NOT_ALLOWED);
        assert!(is_json(&headers));
        assert!(headers.contains_key("allow"));
        assert_eq!(error_json(&body)["code"], "method_not_allowed");
    }
}
