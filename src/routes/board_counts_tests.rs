use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::held_disk;
use crate::db::Db;
use crate::{AppState, BODY_LIMIT_BYTES};

struct Fixture {
    app: axum::Router,
    db: Db,
    cookie: String,
    board_id: String,
    hours: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("counts.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id, "").expect("session");
    let board_id = db.create_board(&user_id, "Counts").expect("board");
    let plain = db.create_column(&board_id, "Plain", None).expect("column");
    let hours = db.create_column(&board_id, "Hours", None).expect("column");
    let wip = db
        .create_column(&board_id, "Limited", None)
        .expect("column");
    db.set_column_config(&hours, r#"{"column_sum":"hours"}"#)
        .expect("sum");
    db.set_column_wip(&wip, Some(5)).expect("wip");
    for column in [&plain, &hours, &wip] {
        for name in ["a", "b"] {
            db.create_task(column, None, name, 0, None).expect("task");
        }
    }
    let app = router(
        AppState::new(db.clone(), None, passkey_mode())
            .with_disk(held_disk(&db))
            .with_limits(unthrottled_limits()),
    );
    let fixture = Fixture {
        app,
        db,
        cookie: format!("session={session}"),
        board_id,
        hours,
    };
    (dir, fixture)
}

async fn send(fx: &Fixture, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
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
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn turn_on(fx: &Fixture) {
    let uri = format!("/api/boards/{}/config", fx.board_id);
    let (status, text) = send(fx, "PUT", &uri, Some(json!({"show_card_counts": true}))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

async fn board_page(fx: &Fixture) -> String {
    let (status, page) = send(fx, "GET", &format!("/b/{}", fx.board_id), None).await;
    assert_eq!(status, StatusCode::OK);
    page
}

const COUNT_BADGE: &str =
    "<span class=\"columnHeader-countBadge\" data-column-sum=\"count\">2</span>";

#[tokio::test]
async fn board_should_show_counts_when_the_toggle_is_on() {
    let (_dir, fx) = fixture();

    // Only the plain column gets a count badge
    turn_on(&fx).await;
    let page = board_page(&fx).await;
    assert_eq!(page.matches(COUNT_BADGE).count(), 1, "{page}");

    // Read the config view back
    let uri = format!("/api/boards/{}/config", fx.board_id);
    let (_, text) = send(&fx, "PUT", &uri, Some(json!({}))).await;
    let view: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(view["show_card_counts"], true);

    // The settings page shows the box checked
    let (_, settings) = send(
        &fx,
        "GET",
        &format!("/b/{}/settings/advanced", fx.board_id),
        None,
    )
    .await;
    assert!(
        settings.contains("id=\"show-card-counts\" checked"),
        "{settings}"
    );
}

#[tokio::test]
async fn board_should_hide_counts_by_default() {
    let (_dir, fx) = fixture();

    // No column shows a count badge
    let page = board_page(&fx).await;
    assert!(!page.contains(COUNT_BADGE), "{page}");
    assert!(!page.contains("data-column-sum=\"count\""), "{page}");

    // The config view reports the toggle off
    let uri = format!("/api/boards/{}/config", fx.board_id);
    let (_, text) = send(&fx, "PUT", &uri, Some(json!({}))).await;
    let view: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(view["show_card_counts"], false);
}

#[tokio::test]
async fn hours_column_should_keep_hours_with_counts_on() {
    let (_dir, fx) = fixture();

    // Give the hours column an estimated task
    let first = fx
        .db
        .create_task(&fx.hours, None, "timed", 0, None)
        .expect("task");
    assert!(
        fx.db
            .set_task_estimate(&first, Some(2.0))
            .expect("estimate")
    );

    // Hours and the WIP label stay, and only the plain column counts
    turn_on(&fx).await;
    let page = board_page(&fx).await;
    assert!(
        page.contains("data-column-sum=\"hours\">2h</span>"),
        "{page}"
    );
    assert!(page.contains("2 / 5"), "{page}");
    assert_eq!(page.matches(COUNT_BADGE).count(), 1, "{page}");
}
