use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::db::Db;
use crate::models::{AccountId, ActiveTimer, HistoryKind, TimerMode};

struct Fixture {
    app: axum::Router,
    db: Db,
    owner: AccountId,
    cookie: String,
    board_id: String,
    column: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("values.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id, "").expect("session");
    let board_id = db.create_board(&user_id, "First").expect("board");
    let column = db.create_column(&board_id, "Todo", None).expect("column");
    db.create_swimlane(&board_id, "Main").expect("lane");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    let fixture = Fixture {
        app,
        db,
        owner: user_id,
        cookie: format!("session={session}"),
        board_id,
        column,
    };
    (dir, fixture)
}

async fn call(fx: &Fixture, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("accept", "application/json")
        .header("cookie", &fx.cookie);
    let body = match body {
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
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn new_task(fx: &Fixture) -> String {
    let (status, task) = call(
        fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.column, "name": "Watched"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    task["id"].as_str().expect("id").to_string()
}

/// Returns the `(from, to)` of each event of `kind` on `task`, oldest first.
fn values(fx: &Fixture, task: &str, kind: HistoryKind) -> Vec<(Option<String>, Option<String>)> {
    fx.db
        .task(task)
        .expect("read")
        .expect("task")
        .history
        .iter()
        .filter(|event| event.kind == kind)
        .map(|event| (event.from.clone(), event.to.clone()))
        .collect()
}

fn pair(from: &str, to: &str) -> (Option<String>, Option<String>) {
    (Some(from.to_string()), Some(to.to_string()))
}

#[tokio::test]
async fn a_move_to_another_swimlane_should_log_the_old_and_new_lane() {
    let (_dir, fx) = fixture();
    let main = fx.db.list_swimlanes(&fx.board_id).expect("lanes")[0]
        .id
        .clone();
    let other = fx.db.create_swimlane(&fx.board_id, "Urgent").expect("lane");
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.column, "name": "Lane", "swimlane_id": main})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let id = task["id"].as_str().expect("id").to_string();

    let (status, body) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/move"),
        Some(json!({"column_id": fx.column, "swimlane_id": other, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        values(&fx, &id, HistoryKind::SwimlaneUpdated),
        [pair("Main", "Urgent")]
    );
}

#[tokio::test]
async fn a_move_to_another_board_should_log_the_old_and_new_board() {
    let (_dir, fx) = fixture();
    let id = new_task(&fx).await;
    let (status, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Second"}))).await;
    assert_eq!(status, StatusCode::OK, "{board}");
    let board_id = board["id"].as_str().expect("board");
    let (_, columns) = call(&fx, "GET", &format!("/api/boards/{board_id}/columns"), None).await;
    let column = columns[0]["id"].as_str().expect("column");

    let (status, body) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/move"),
        Some(json!({"column_id": column, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        values(&fx, &id, HistoryKind::BoardUpdated),
        [pair("First", "Second")]
    );
}

#[tokio::test]
async fn logged_time_should_record_the_time_spent_before_and_after() {
    let (_dir, fx) = fixture();
    let id = new_task(&fx).await;
    for (from, to) in [("08:00", "08:30"), ("09:00", "10:00")] {
        let (status, body) = call(
            &fx,
            "POST",
            "/api/time/manual",
            Some(json!({"task_id": id, "date": "2020-01-01", "from": from, "to": to})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(
        values(&fx, &id, HistoryKind::TimeLogged),
        [pair("0m", "30m"), pair("30m", "1h 30m")]
    );
}

#[tokio::test]
async fn deleting_a_time_entry_should_record_the_time_spent_before_and_after() {
    let (_dir, fx) = fixture();
    let id = new_task(&fx).await;
    let (status, entry) = call(
        &fx,
        "POST",
        "/api/time/manual",
        Some(json!({"task_id": id, "date": "2020-01-01", "from": "08:00", "to": "09:00"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    let entry_id = entry["id"].as_str().expect("entry");
    let (status, body) = call(
        &fx,
        "DELETE",
        &format!("/api/time/entries/{entry_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        values(&fx, &id, HistoryKind::TimeDeleted),
        [pair("1h", "0m")]
    );
}

#[tokio::test]
async fn subtask_changes_should_record_the_progress_before_and_after() {
    let (_dir, fx) = fixture();
    let id = new_task(&fx).await;
    let (status, sub) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/subtasks"),
        Some(json!({"name": "Draft"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    let sub_id = sub["id"].as_str().expect("subtask");
    let uri = format!("/api/tasks/{id}/subtasks/{sub_id}");
    let (status, body) = call(&fx, "PATCH", &uri, Some(json!({"done": true}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&fx, "PATCH", &uri, Some(json!({"name": "Final"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&fx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        values(&fx, &id, HistoryKind::SubtaskAdded),
        [(None, Some("0 of 1 done".to_string()))]
    );
    assert_eq!(
        values(&fx, &id, HistoryKind::SubtaskDone),
        [pair("0 of 1 done", "1 of 1 done")]
    );
    assert_eq!(
        values(&fx, &id, HistoryKind::SubtaskRenamed),
        [pair("Draft", "Final")]
    );
    assert_eq!(
        values(&fx, &id, HistoryKind::SubtaskDeleted),
        [(Some("1 of 1 done".to_string()), None)]
    );
}

#[tokio::test]
async fn a_subtask_template_should_record_the_progress_of_each_added_subtask() {
    let (_dir, fx) = fixture();
    let source = new_task(&fx).await;
    for name in ["Build", "Ship"] {
        let (status, body) = call(
            &fx,
            "POST",
            &format!("/api/tasks/{source}/subtasks"),
            Some(json!({"name": name})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, template) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{source}/subtasks/template"),
        Some(json!({"name": "Release"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{template}");
    let target = new_task(&fx).await;
    let (status, body) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{target}/subtasks/from-template"),
        Some(json!({"template_id": template["id"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        values(&fx, &target, HistoryKind::SubtaskAdded),
        [
            (None, Some("0 of 1 done".to_string())),
            pair("0 of 1 done", "0 of 2 done"),
        ]
    );
}

#[tokio::test]
async fn editing_a_time_entry_should_record_the_time_spent_before_and_after() {
    let (_dir, fx) = fixture();
    let id = new_task(&fx).await;
    let (status, entry) = call(
        &fx,
        "POST",
        "/api/time/manual",
        Some(json!({"task_id": id, "date": "2020-01-01", "from": "08:00", "to": "09:00"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    let entry_id = entry["id"].as_str().expect("entry");
    let (status, body) = call(
        &fx,
        "PUT",
        &format!("/api/time/entries/{entry_id}"),
        Some(json!({"task_id": id, "date": "2020-01-01", "from": "08:00", "to": "10:30"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        values(&fx, &id, HistoryKind::TimeEdited),
        [pair("1h", "2h 30m")]
    );
}

/// Returns the `detail` of each event of `kind` on `task`, oldest first.
fn details(fx: &Fixture, task: &str, kind: HistoryKind) -> Vec<String> {
    fx.db
        .task(task)
        .expect("read")
        .expect("task")
        .history
        .iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.detail.clone())
        .collect()
}

#[tokio::test]
async fn logging_time_by_hand_should_record_its_history_event_with_the_entry() {
    let (_dir, fx) = fixture();
    let id = new_task(&fx).await;
    let (status, entry) = call(
        &fx,
        "POST",
        "/api/time/manual",
        Some(json!({"task_id": id, "date": "2020-01-01", "from": "08:00", "to": "09:00"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    assert_eq!(fx.db.list_entries(&id).expect("entries").len(), 1);
    assert_eq!(
        values(&fx, &id, HistoryKind::TimeLogged),
        [pair("0m", "1h")]
    );
    assert_eq!(
        details(&fx, &id, HistoryKind::TimeLogged),
        ["Manually logged 60m"]
    );
}

#[tokio::test]
async fn ending_a_timer_session_should_record_its_history_event_with_the_entry() {
    let (_dir, fx) = fixture();
    let id = new_task(&fx).await;
    fx.db
        .account(&fx.owner)
        .set_active_timer(&ActiveTimer {
            task_id: Some(id.clone()),
            mode: TimerMode::Stopwatch,
            started_at: chrono::Utc::now().timestamp() - 600,
            duration_secs: None,
        })
        .expect("timer");
    let (status, body) = call(&fx, "POST", "/api/timer/stop", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fx.db.list_entries(&id).expect("entries").len(), 1);
    assert_eq!(
        values(&fx, &id, HistoryKind::TimeLogged),
        [pair("0m", "10m")]
    );
    assert_eq!(
        details(&fx, &id, HistoryKind::TimeLogged),
        ["Stopwatch session logged (10m)"]
    );
}
