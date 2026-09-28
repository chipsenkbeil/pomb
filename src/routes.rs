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
    routing::{get, patch, post, put},
    Form, Json, Router,
};
use chrono::{DateTime, Duration, Local};
#[cfg(not(debug_assertions))]
use rust_embed::RustEmbed;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tower_http::services::ServeDir;

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
        .route("/api/tasks/:id", patch(update_task).delete(delete_task))
        .route("/api/tasks/:id/card", get(task_card))
        .route("/api/tasks/:id/modal", get(task_modal))
        .route("/api/tasks/:id/move", post(move_task))
        .route("/api/tasks/:id/time", post(log_time).get(get_task_time))
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
        .route("/api/settings", get(api_settings))
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
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/login", get(login_page).post(login_submit))
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

// ---- View models for templates ----

/// Task shaped for rendering: colors/labels resolved, dates formatted.
#[derive(Debug, Clone)]
struct TaskView {
    id: String,
    column_id: String,
    name: String,
    description: String,
    size: i64,
    color_hex: &'static str,
    size_label: &'static str,
    total_minutes: i64,
    completed_at: Option<String>,
    /// "Sep 28" style rendering of `completed_at`, for cards.
    completed_display: Option<String>,
    /// "Sep 28, 2026" style rendering of `created_at`, for the modal.
    created_display: String,
    pomodori_completed: u32,
    interruptions: u32,
}

impl TaskView {
    fn from_row(row: &TaskRow) -> Self {
        let size = Size::from_i64(row.size);
        Self {
            id: row.id.clone(),
            column_id: row.column_id.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            size: size as i64,
            color_hex: size.color_hex(),
            size_label: size.label(),
            total_minutes: row.total_minutes,
            completed_at: row.completed_at.clone(),
            completed_display: row.completed_at.as_deref().map(format_day),
            created_display: DateTime::parse_from_rfc3339(&row.created_at)
                .map(|dt| dt.with_timezone(&Local).format("%b %d, %Y").to_string())
                .unwrap_or_else(|_| row.created_at.clone()),
            pomodori_completed: row.pomodori_completed,
            interruptions: row.interruptions,
        }
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
    wip_limit: Option<i64>,
    count: usize,
    at_limit: bool,
    is_done: bool,
}

#[derive(Template)]
#[template(path = "board.html")]
struct BoardTemplate {
    board_id: String,
    board_name: String,
    username: String,
    columns: Vec<ColumnHead>,
    bands: Vec<BandView>,
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
    kind_label: String,
    interrupted: bool,
    interrupt_reason: Option<String>,
}

#[derive(Template)]
#[template(path = "time_entries.html")]
struct TimeEntriesTemplate {
    entries: Vec<TimeEntryView>,
}

#[derive(Template)]
#[template(path = "modal.html")]
struct ModalTemplate {
    task: TaskView,
    entries: Vec<TimeEntryView>,
    is_done: bool,
}

// ---- Small DB helpers ----

/// Fetch one task with its total logged minutes, shaped for templates.
fn fetch_task_view(db: &Db, id: &str) -> Result<Option<TaskView>, AppError> {
    let row = db.get_task_with_minutes(id).map_err(AppError::from)?;
    Ok(row.as_ref().map(TaskView::from_row))
}

/// Time entries for a task, newest first, with display-ready timestamps.
fn fetch_entries(db: &Db, task_id: &str) -> Result<Vec<TimeEntryView>, AppError> {
    let rows = db.list_entries(task_id).map_err(AppError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let kind_label = match row.kind.as_str() {
                "pomodoro" => "Pomodoro",
                "stopwatch" => "Stopwatch",
                "short_break" => "Short break",
                "long_break" => "Long break",
                _ => "Manual",
            }
            .to_string();
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
                kind_label,
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
                wip_limit: col.wip_limit,
                count,
                at_limit,
                is_done: col.is_done,
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
                .map(TaskView::from_row)
                .collect();

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
        username: user.username,
        columns: column_heads,
        bands,
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

#[derive(Deserialize)]
struct CreateTaskInput {
    column_id: String,
    swimlane_id: Option<String>,
    name: String,
    size: Option<i64>,
}

/// Create a task; returns the rendered card fragment (for htmx appends).
/// Task name list for the manual-time / edit-entry task autocomplete.
async fn list_tasks(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let tasks = state
        .db
        .all_tasks()
        .map_err(AppError::from)?
        .into_iter()
        .map(|t| serde_json::json!({ "id": t.id, "name": t.name }))
        .collect();
    Ok(Json(tasks))
}

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
    let id = db
        .create_task(&column.id, swimlane_id.as_deref(), &name, size as i64)
        .map_err(AppError::from)?;

    Ok(TaskCardTemplate {
        task: TaskView {
            id,
            column_id: column.id.clone(),
            name,
            description: String::new(),
            size: size as i64,
            color_hex: size.color_hex(),
            size_label: size.label(),
            total_minutes: 0,
            completed_at: None,
            completed_display: None,
            created_display: Local::now().format("%b %d, %Y").to_string(),
            pomodori_completed: 0,
            interruptions: 0,
        },
    })
}

#[derive(Deserialize)]
struct UpdateTaskInput {
    name: Option<String>,
    description: Option<String>,
    size: Option<i64>,
}

/// Patch name/description/size; returns the refreshed card fragment.
async fn update_task(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<TaskCardTemplate, AppError> {
    let input: UpdateTaskInput = parse_body(&headers, body).await?;
    let db = &state.db;

    if db.get_task(&id).map_err(AppError::from)?.is_none() {
        return Err(AppError::not_found("task not found"));
    }

    if let Some(name) = input.name.as_deref() {
        if name.trim().is_empty() {
            return Err(AppError::bad_request("task name is required"));
        }
    }
    let size = input.size.map(Size::from_i64).map(|size| size as i64);
    db.update_task(
        &id,
        input.name.as_deref().map(str::trim),
        input.description.as_deref(),
        size,
    )
    .map_err(AppError::from)?;

    let task = fetch_task_view(db, &id)?.ok_or_else(|| AppError::not_found("task not found"))?;
    Ok(TaskCardTemplate { task })
}

#[derive(Deserialize)]
struct MoveTaskInput {
    column_id: String,
    swimlane_id: Option<String>,
    position: f64,
}

/// Move a task to a new column/swimlane/position. Crossing into the Done
/// column stamps `completed_at`; crossing out clears it.
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

    Ok(StatusCode::OK)
}

/// Delete a task and its time entries.
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
    Ok(ModalTemplate {
        task,
        entries,
        is_done,
    })
}

#[derive(Deserialize)]
struct LogTimeInput {
    minutes: i64,
    note: Option<String>,
}

/// Log minutes on a task; returns the refreshed time-entries fragment.
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
async fn get_time_entry(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
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
    Ok(Json(serde_json::json!({
        "id": entry.id,
        "task_id": entry.task_id,
        "task_name": task_name,
        "minutes": entry.minutes,
        "note": entry.note,
        "started_at": entry.started_at,
    })))
}

#[derive(Deserialize)]
struct ManualTimeInput {
    task_id: String,
    /// YYYY-MM-DD
    date: String,
    /// HH:MM (24h)
    from: String,
    /// HH:MM (24h)
    to: String,
    note: Option<String>,
}

/// "Add time manually" dialog (v3-00001): explicit date + from/to.
/// Duration is auto-computed; future times are rejected with KanbanFlow's
/// exact message and the dialog keeps its state (client-side).
async fn create_manual_time(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
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

    Ok(Json(serde_json::json!({ "id": id, "minutes": minutes })))
}

/// Parse a manual date + from/to range into a start DateTime and duration.
/// Times are interpreted in the server's local timezone.
fn parse_manual_range(
    date: &str,
    from: &str,
    to: &str,
) -> Result<(DateTime<Local>, i64), AppError> {
    use chrono::NaiveDateTime;
    let bad = || AppError::bad_request("invalid date or time");
    let start = NaiveDateTime::parse_from_str(&format!("{date} {from}"), "%Y-%m-%d %H:%M")
        .map_err(|_| bad())?;
    let end = NaiveDateTime::parse_from_str(&format!("{date} {to}"), "%Y-%m-%d %H:%M")
        .map_err(|_| bad())?;
    let start = start
        .and_local_timezone(Local)
        .single()
        .ok_or_else(bad)?;
    let end = end.and_local_timezone(Local).single().ok_or_else(bad)?;
    let minutes = (end - start).num_minutes();
    Ok((start, minutes))
}

#[derive(Deserialize)]
struct UpdateTimeEntryInput {
    task_id: String,
    /// YYYY-MM-DD
    date: String,
    /// HH:MM (24h)
    from: String,
    /// HH:MM (24h)
    to: String,
    note: Option<String>,
}

/// Edit a time entry (v3-01841): date/time, task reassignment, note.
/// Date edits re-bucket the entry via its new started_at.
async fn update_time_entry(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
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

    Ok(Json(serde_json::json!({ "id": id, "minutes": minutes })))
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

#[derive(Deserialize)]
struct CreateColumnInput {
    board_id: Option<String>,
    name: String,
    wip_limit: Option<i64>,
}

/// Create a column at the end of the board's column order; returns its id.
async fn create_column(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
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
    Ok(Json(serde_json::json!({ "id": id })))
}

#[derive(Deserialize)]
struct UpdateColumnInput {
    name: Option<String>,
    /// `Some(None)` (JSON null) clears the limit; absent leaves it alone.
    wip_limit: Option<Option<i64>>,
    is_done: Option<bool>,
}

/// Rename a column, set/clear its WIP limit, and/or flip its done flag.
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
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct MoveColumnInput {
    position: i64,
}

/// Reorder a column within its board; positions are renumbered densely.
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

#[derive(Deserialize)]
struct CreateSwimlaneInput {
    board_id: Option<String>,
    name: String,
}

/// Create a swimlane at the end of the board's swimlane order; returns its id.
async fn create_swimlane(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
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
    Ok(Json(serde_json::json!({ "id": id })))
}

#[derive(Deserialize)]
struct UpdateSwimlaneInput {
    name: Option<String>,
}

/// Rename a swimlane.
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

#[derive(Deserialize)]
struct MoveSwimlaneInput {
    position: i64,
}

/// Reorder a swimlane within its board; positions are renumbered densely.
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
        return Ok(SetupTemplate {
            error: Some(error),
        }
        .into_response());
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
}

async fn settings_page(
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<SettingsTemplate, AppError> {
    let settings = state.db.get_settings().map_err(AppError::from)?;
    let _ = user;
    Ok(SettingsTemplate {
        settings,
        saved: query.contains_key("saved"),
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
    };
    state
        .db
        .update_settings(&settings)
        .map_err(AppError::from)?;
    Ok(Redirect::to("/settings?saved=1").into_response())
}

/// Settings as JSON for the timer JavaScript.
async fn api_settings(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Settings>, AppError> {
    Ok(Json(state.db.get_settings().map_err(AppError::from)?))
}

// ---- Timer ----

/// Timer status for the header pill and popup, with everything the
/// JavaScript needs to render without another round trip.
#[derive(serde::Serialize)]
struct TimerStatusView {
    active: bool,
    mode: Option<String>,
    mode_title: Option<String>,
    task_id: Option<String>,
    task_name: Option<String>,
    started_at: Option<i64>,
    duration_secs: Option<u64>,
}

fn timer_status_view(
    db: &Db,
    timer: Option<ActiveTimer>,
) -> Result<TimerStatusView, AppError> {
    match timer {
        None => Ok(TimerStatusView {
            active: false,
            mode: None,
            mode_title: None,
            task_id: None,
            task_name: None,
            started_at: None,
            duration_secs: None,
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
                    mode: None,
                    mode_title: None,
                    task_id: None,
                    task_name: None,
                    started_at: None,
                    duration_secs: None,
                });
            }
            Ok(TimerStatusView {
                active: true,
                mode: Some(timer.mode.as_str().to_string()),
                mode_title: Some(timer.mode.title().to_string()),
                task_id: timer.task_id,
                task_name,
                started_at: Some(timer.started_at),
                duration_secs: timer.duration_secs,
            })
        }
    }
}

async fn timer_status(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<TimerStatusView>, AppError> {
    let timer = state.db.get_active_timer().map_err(AppError::from)?;
    Ok(Json(timer_status_view(&state.db, timer)?))
}

#[derive(Deserialize)]
struct TimerStartInput {
    task_id: Option<String>,
    mode: String,
}

/// Start a timer, replacing any active one. A replaced session of 20+
/// seconds is logged as interrupted ("Switched task") so no work time
/// silently vanishes; shorter ones are discarded.
async fn timer_start(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TimerStatusView>, AppError> {
    let input: TimerStartInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let mode = TimerMode::from_str(&input.mode)
        .ok_or_else(|| AppError::bad_request("unknown timer mode"))?;
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

#[derive(Deserialize)]
struct TimerStopInput {
    /// True when the timer ran to zero on its own.
    completed: bool,
    /// Why a pomodoro was stopped early ("Why did you stop?").
    reason: Option<String>,
}

/// Stop the active timer and log the session. Sessions under 20 seconds
/// are discarded (KanbanFlow does the same). Durations are minute-truncated,
/// not rounded. A finished pomodoro bumps the task's pomodori counter; an
/// early stop bumps both its pomodori counter (stopped sessions count as
/// Pomodori, verified in KanbanFlow) and its interruptions.
async fn timer_stop(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let input: TimerStopInput = parse_body(&headers, body).await?;
    let db = &state.db;

    let timer = db
        .get_active_timer()
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("no active timer"))?;
    let logged = log_timer_session(db, &timer, input.completed, input.reason.as_deref())?;

    // Remember custom "why did you stop?" reasons for next time.
    if let Some(reason) = input.reason.as_deref() {
        let reason = reason.trim();
        if !reason.is_empty() {
            let mut settings = db.get_settings().map_err(AppError::from)?;
            if !settings
                .interrupt_reasons
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(reason))
            {
                // Keep the fixed list tidy: custom reasons slot in before
                // the trailing "Task done".
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

    Ok(Json(serde_json::json!({
        "discarded": logged.is_none(),
        "minutes": logged,
        "completed": input.completed,
    })))
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
                db.record_pomodoro_complete(task_id).map_err(AppError::from)?;
            }
            TimerMode::Pomodoro => {
                // Stopped sessions count as Pomodori (verified in KanbanFlow)
                // and also record an interruption.
                db.record_pomodoro_complete(task_id).map_err(AppError::from)?;
                db.record_interruption(task_id).map_err(AppError::from)?;
            }
            _ => {}
        }
    }
    Ok(Some(minutes))
}

#[derive(Deserialize)]
struct TimerRetargetInput {
    task_id: Option<String>,
}

/// Point the active timer at a different task ("Change task").
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

#[derive(serde::Serialize)]
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
