use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::TEST_BODY_4_MIB_BYTES;
use crate::AppState;
use crate::db::Db;
use crate::models::{AccountId, Seat, SeatSlot};

struct Fixture {
    db: Db,
    user_id: AccountId,
    app: axum::Router,
    cookie: String,
    board_id: String,
    column_id: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("seats.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id, "").expect("session");
    let board_id = db.create_board(&user_id, "Seats").expect("board");
    let column_id = db.create_column(&board_id, "Todo", None).expect("column");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    let fixture = Fixture {
        db,
        user_id,
        app,
        cookie: format!("session={session}"),
        board_id,
        column_id,
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
    let bytes = axum::body::to_bytes(response.into_body(), TEST_BODY_4_MIB_BYTES)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn slot(name: &str, role: &str) -> SeatSlot {
    SeatSlot {
        name: name.to_string(),
        role: role.to_string(),
    }
}

fn seats_of(fx: &Fixture, task_id: &str) -> Vec<Seat> {
    fx.db
        .account(&fx.user_id)
        .task(task_id)
        .expect("read")
        .expect("task")
        .row()
        .seats
        .clone()
}

fn names(seats: &[Seat]) -> Vec<&str> {
    seats.iter().map(|seat| seat.name.as_str()).collect()
}

#[tokio::test]
async fn seat_config_round_trips_per_board() {
    let (_dir, fx) = fixture();
    let other = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let uri = format!("/api/boards/{}/seats", fx.board_id);
    let (status, body) = call(
        &fx,
        "PUT",
        &uri,
        Some(json!({"seats": [
            {"name": " code ", "role": "reviewer"},
            {"name": "verify", "role": "verifier"},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected = json!({"seats": [
        {"name": "code", "role": "reviewer"},
        {"name": "verify", "role": "verifier"},
    ]});
    assert_eq!(body, expected);
    let (status, body) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, expected);
    let (status, body) = call(&fx, "GET", &format!("/api/boards/{other}/seats"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"seats": []}));
    let stored = fx
        .db
        .account(&fx.user_id)
        .board(&fx.board_id)
        .expect("read")
        .expect("board")
        .row()
        .seats
        .clone();
    assert_eq!(
        stored,
        vec![slot("code", "reviewer"), slot("verify", "verifier")]
    );
}

#[tokio::test]
async fn new_card_should_copy_the_board_seats() {
    let (_dir, fx) = fixture();
    let plain = fx
        .db
        .create_task(&fx.column_id, None, "Plain", 1, None)
        .expect("task");
    assert!(seats_of(&fx, &plain).is_empty());
    let (status, body) = call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/seats", fx.board_id),
        Some(json!({"seats": [
            {"name": "code", "role": "reviewer"},
            {"name": "verify", "role": "verifier"},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seated = fx
        .db
        .create_task(&fx.column_id, None, "Seated", 1, None)
        .expect("task");
    assert_eq!(names(&seats_of(&fx, &seated)), ["code", "verify"]);
    assert!(seats_of(&fx, &plain).is_empty());
}

#[tokio::test]
async fn card_can_add_and_drop_a_seat() {
    let (_dir, fx) = fixture();
    let first = fx
        .db
        .create_task(&fx.column_id, None, "First", 1, None)
        .expect("task");
    let second = fx
        .db
        .create_task(&fx.column_id, None, "Second", 1, None)
        .expect("task");
    let uri = format!("/api/tasks/{first}/seats");
    let (status, body) = call(
        &fx,
        "POST",
        &uri,
        Some(json!({"name": "security", "role": "reviewer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["name"], "security");
    assert_eq!(body[0]["role"], "reviewer");
    assert_eq!(body[0]["holder"], Value::Null);
    assert_eq!(names(&seats_of(&fx, &first)), ["security"]);
    assert!(seats_of(&fx, &second).is_empty());
    let board = fx
        .db
        .account(&fx.user_id)
        .board(&fx.board_id)
        .expect("read")
        .expect("board");
    assert!(board.row().seats.is_empty());
    let (status, body) = call(
        &fx,
        "POST",
        &uri,
        Some(json!({"name": "Security", "role": "reviewer"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = call(&fx, "DELETE", &format!("{uri}?name=nobody"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call(&fx, "DELETE", &format!("{uri}?name=security"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));
    assert!(seats_of(&fx, &first).is_empty());
    assert!(seats_of(&fx, &second).is_empty());
}

#[tokio::test]
async fn seat_with_unknown_role_is_refused() {
    let (_dir, fx) = fixture();
    let task = fx
        .db
        .create_task(&fx.column_id, None, "Task", 1, None)
        .expect("task");
    let (status, body) = call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/seats", fx.board_id),
        Some(json!({"seats": [{"name": "code", "role": "wizard"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{task}/seats"),
        Some(json!({"name": "code", "role": "wizard"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(seats_of(&fx, &task).is_empty());
    let board = fx
        .db
        .account(&fx.user_id)
        .board(&fx.board_id)
        .expect("read")
        .expect("board");
    assert!(board.row().seats.is_empty());
}

#[tokio::test]
async fn task_detail_should_list_the_card_seats() {
    let (_dir, fx) = fixture();
    let (status, body) = call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/seats", fx.board_id),
        Some(json!({"seats": [
            {"name": "code", "role": "reviewer"},
            {"name": "verify", "role": "verifier"},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let task = fx
        .db
        .create_task(&fx.column_id, None, "Detail", 1, None)
        .expect("task");
    let (status, body) = call(&fx, "GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seats = body["seats"].as_array().expect("seats array");
    assert_eq!(seats.len(), 2, "{body}");
    for (seat, (name, role)) in seats
        .iter()
        .zip([("code", "reviewer"), ("verify", "verifier")])
    {
        assert_eq!(seat["name"], name);
        assert_eq!(seat["role"], role);
        assert!(seat["holder"].is_null(), "{seat}");
        assert!(seat["lease_expires_at"].is_null(), "{seat}");
        assert!(seat["verdict"].is_null(), "{seat}");
        assert!(seat["revision"].is_null(), "{seat}");
    }
}
