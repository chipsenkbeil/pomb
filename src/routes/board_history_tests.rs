use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::Value;
use tower::ServiceExt;

use super::board_history::{Filter, Order};
use super::server::{passkey_mode, unthrottled_limits};
use super::{load_history_page, router};
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{
    AccountId, Actor, ActorKind, AuthScope, BIN_RETENTION_DAYS, BoardAccess, HistoryKind,
    SECONDS_PER_DAY, TaskComment, TaskEvent,
};
use crate::{AppState, BODY_LIMIT_BYTES};

struct Fixture {
    app: axum::Router,
    db: Db,
    user_id: AccountId,
    cookie: String,
    board_id: String,
    column: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("history.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id, "").expect("session");
    let board_id = db.create_board(&user_id, "History").expect("board");
    let column = db.create_column(&board_id, "Todo", None).expect("column");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    let fixture = Fixture {
        app,
        db,
        user_id,
        cookie: format!("session={session}"),
        board_id,
        column,
    };
    (dir, fixture)
}

async fn send(fx: &Fixture, method: &str, uri: &str) -> (StatusCode, String) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", &fx.cookie)
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn rows(fx: &Fixture, query: &str) -> Value {
    let uri = format!("/api/boards/{}/history?{query}", fx.board_id);
    let (status, text) = send(fx, "GET", &uri).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).expect("json")
}

async fn send_as(fx: &Fixture, bearer: &str, uri: &str) -> (StatusCode, String) {
    let request = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", bearer)
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn details(page: &Value) -> Vec<String> {
    page["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["detail"].as_str().expect("detail").to_string())
        .collect()
}

fn event(kind: HistoryKind, detail: &str, actor: &str, at: &str) -> TaskEvent {
    TaskEvent {
        id: format!("{}-{at}", kind.as_str()),
        kind,
        detail: detail.to_string(),
        actor: actor.to_string(),
        created_at: at.to_string(),
        column_id: None,
        token_name: None,
        from: None,
        to: None,
        parent_token: None,
        role_id: None,
        role_name: None,
        label: None,
    }
}

fn task_with(fx: &Fixture, name: &str, history: Vec<TaskEvent>) -> String {
    task_in(fx, &fx.column, name, history)
}

fn task_in(fx: &Fixture, column: &str, name: &str, history: Vec<TaskEvent>) -> String {
    let id = fx
        .db
        .create_task(column, None, name, 1, None)
        .expect("task");
    let owned = fx.db.account(&fx.user_id);
    let proof = owned.task(&id).expect("read").expect("task");
    owned
        .apply_task_change(&proof, Actor::SYSTEM, |row| {
            row.history = history;
            ((), true)
        })
        .expect("history")
        .expect("task");
    id
}

fn seeded(fx: &Fixture) {
    let mut moved = event(
        HistoryKind::Moved,
        "Moved from Todo to Doing",
        "ann",
        "2026-10-20T10:00:00Z",
    );
    moved.from = Some("Todo".to_string());
    moved.to = Some("Doing".to_string());
    task_with(
        fx,
        "Alpha",
        vec![
            event(
                HistoryKind::Created,
                "Task created in Todo",
                "ann",
                "2026-10-10T10:00:00Z",
            ),
            moved,
            event(
                HistoryKind::LabelsUpdated,
                "Changed the labels",
                "bob",
                "2026-10-25T10:00:00Z",
            ),
        ],
    );
    task_with(
        fx,
        "Beta",
        vec![
            event(
                HistoryKind::Created,
                "Task created in Todo",
                "bob",
                "2026-10-15T10:00:00Z",
            ),
            event(
                HistoryKind::LinkAdded,
                "Linked to Alpha",
                "ann",
                "2026-10-26T10:00:00Z",
            ),
        ],
    );
}

#[tokio::test]
async fn history_should_list_newest_first_and_oldest_first_on_order() {
    let (_dir, fx) = fixture();
    seeded(&fx);
    let newest = details(&rows(&fx, "").await);
    assert_eq!(newest.len(), 5, "{newest:?}");
    assert_eq!(newest[0], "Linked to Alpha");
    assert_eq!(newest[4], "Task created in Todo");
    let oldest = details(&rows(&fx, "order=oldest").await);
    assert_eq!(oldest[0], "Task created in Todo");
    assert_eq!(oldest[4], "Linked to Alpha");
}

#[tokio::test]
async fn history_should_keep_only_the_rows_in_the_period() {
    let (_dir, fx) = fixture();
    seeded(&fx);
    let page = rows(
        &fx,
        "from=2026-10-16T00:00:00Z&to=2026-10-26T00:00:00Z&order=oldest",
    )
    .await;
    assert_eq!(
        details(&page),
        ["Moved from Todo to Doing", "Changed the labels"]
    );
    let open_end = rows(&fx, "from=2026-10-26T10:00:00Z").await;
    assert_eq!(details(&open_end), ["Linked to Alpha"]);
}

#[tokio::test]
async fn history_should_filter_by_event_property_and_user() {
    let (_dir, fx) = fixture();
    seeded(&fx);
    assert_eq!(
        details(&rows(&fx, "event=created&order=oldest").await),
        ["Task created in Todo", "Task created in Todo"]
    );
    assert_eq!(
        details(&rows(&fx, "event=relation").await),
        ["Linked to Alpha"]
    );
    assert_eq!(
        details(&rows(&fx, "event=changed&order=oldest").await),
        ["Moved from Todo to Doing", "Changed the labels"]
    );
    assert_eq!(
        details(&rows(&fx, "event=changed&property=labels_updated").await),
        ["Changed the labels"]
    );
    let by_bob = rows(&fx, "user=bob&order=oldest").await;
    assert_eq!(
        details(&by_bob),
        ["Task created in Todo", "Changed the labels"]
    );
    assert_eq!(by_bob["users"], serde_json::json!(["ann", "bob"]));
}

#[tokio::test]
async fn history_should_give_the_old_and_new_value_of_a_change() {
    let (_dir, fx) = fixture();
    let mut legacy = event(
        HistoryKind::Moved,
        "Moved from Todo to Done",
        "ann",
        "2026-10-20T10:00:00Z",
    );
    legacy.from = None;
    legacy.to = None;
    task_with(&fx, "Old", vec![legacy]);
    seeded(&fx);
    let page = rows(&fx, "event=changed&property=moved&order=oldest").await;
    let moved = &page["rows"][0];
    assert_eq!(moved["task"], "Old");
    assert_eq!(moved["changed"], true);
    assert_eq!(moved["old_value"], "Todo");
    assert_eq!(moved["new_value"], "Done");
    let alpha = &page["rows"][1];
    assert_eq!(alpha["old_value"], "Todo");
    assert_eq!(alpha["new_value"], "Doing");
    let created = &rows(&fx, "event=created").await["rows"][0];
    assert_eq!(created["changed"], false);
}

#[tokio::test]
async fn history_should_hold_comments_and_deleted_tasks() {
    let (_dir, fx) = fixture();
    let id = task_with(
        &fx,
        "Gone",
        vec![event(
            HistoryKind::Created,
            "Task created in Todo",
            "ann",
            "2026-10-10T10:00:00Z",
        )],
    );
    let (status, _) = send(&fx, "DELETE", &format!("/api/tasks/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let deleted = rows(&fx, "event=deleted").await;
    assert_eq!(deleted["rows"][0]["task"], "Gone");
    assert_eq!(deleted["rows"][0]["user"], "admin");
    let live = task_with(&fx, "Talk", Vec::new());
    let owned = fx.db.account(&fx.user_id);
    let proof = owned.task(&live).expect("read").expect("task");
    owned
        .apply_task_change(&proof, Actor::SYSTEM, |row| {
            row.comments.push(TaskComment {
                id: "c1".to_string(),
                author: "bob".to_string(),
                body: "Looks fine".to_string(),
                created_at: "2026-10-27T10:00:00Z".to_string(),
                actor_kind: ActorKind::default(),
                token_name: None,
                token_id: None,
                kind: None,
                edited_at: None,
                reactions: Vec::new(),
                question: None,
                parent_token: None,
                role_id: None,
                role_name: None,
                label: None,
            });
            ((), true)
        })
        .expect("comment")
        .expect("task");
    let comments = rows(&fx, "event=comment").await;
    assert_eq!(details(&comments), ["Looks fine"]);
    assert_eq!(comments["rows"][0]["user"], "bob");
}

#[tokio::test]
async fn history_export_should_write_only_the_filtered_rows_as_csv_and_excel() {
    let (_dir, fx) = fixture();
    seeded(&fx);
    let uri = format!(
        "/api/boards/{}/history?format=csv&event=relation",
        fx.board_id
    );
    let (status, csv) = send(&fx, "GET", &uri).await;
    assert_eq!(status, StatusCode::OK, "{csv}");
    assert!(
        csv.contains("\"Time\",\"User\",\"Task\",\"Event\""),
        "{csv}"
    );
    assert!(csv.contains("\"Linked to Alpha\""), "{csv}");
    assert!(!csv.contains("Changed the labels"), "{csv}");

    let uri = format!(
        "/api/boards/{}/history?format=xlsx&event=changed",
        fx.board_id
    );
    let request = axum::http::Request::builder()
        .uri(&uri)
        .header("cookie", &fx.cookie)
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .expect("header")
            .contains("board-history.xlsx")
    );
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    assert_eq!(&bytes[..4], b"PK\x03\x04");
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("Changed the labels"));
    assert!(!text.contains("Linked to Alpha"));
}

#[tokio::test]
async fn history_should_refuse_a_bad_query() {
    let (_dir, fx) = fixture();
    for query in [
        "event=nothing",
        "order=sideways",
        "format=pdf",
        "from=yesterday",
        "from=2026-10-27T00:00:00Z&to=2026-10-26T00:00:00Z",
    ] {
        let uri = format!("/api/boards/{}/history?{query}", fx.board_id);
        let (status, text) = send(&fx, "GET", &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {text}");
    }
    let (status, _) = send(&fx, "GET", "/api/boards/missing/history").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn history_should_leave_out_a_deleted_task_past_its_retention() {
    let (_dir, fx) = fixture();
    let id = task_with(
        &fx,
        "Gone",
        vec![event(
            HistoryKind::Created,
            "Task created in Todo",
            "ann",
            "2026-10-10T10:00:00Z",
        )],
    );
    let (status, _) = send(&fx, "DELETE", &format!("/api/tasks/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let board = fx
        .db
        .account(&fx.user_id)
        .board(&fx.board_id)
        .expect("read")
        .expect("board");
    let filter = Filter {
        event: None,
        property: None,
        user: None,
        from: None,
        to: None,
        order: Order::Newest,
    };
    let reader = fx.db.account(&fx.user_id);
    let now = chrono::Utc::now().timestamp();
    let kept = load_history_page(&reader, &board, &filter, now).expect("page");
    assert!(!kept.rows.is_empty());
    let late = now + (BIN_RETENTION_DAYS + 1) * SECONDS_PER_DAY;
    let expired = load_history_page(&reader, &board, &filter, late).expect("page");
    assert!(expired.rows.is_empty(), "{:?}", expired.rows);
}

#[tokio::test]
async fn history_should_not_split_a_legacy_move_whose_column_name_holds_to() {
    let (_dir, fx) = fixture();
    task_with(
        &fx,
        "Odd",
        vec![event(
            HistoryKind::Moved,
            "Moved from Back to backlog to Done",
            "ann",
            "2026-10-20T10:00:00Z",
        )],
    );
    let page = rows(&fx, "event=changed").await;
    assert_eq!(page["rows"][0]["changed"], false);
}

#[tokio::test]
async fn history_page_should_offer_the_events_and_properties() {
    let (_dir, fx) = fixture();
    let (status, html) = send(&fx, "GET", &format!("/b/{}/board-history", fx.board_id)).await;
    assert_eq!(status, StatusCode::OK);
    for option in [
        "value=\"created\">Created",
        "value=\"relation\">Relation",
        "value=\"timeline_updated\">Timeline",
        "value=\"moved\">Column",
    ] {
        assert!(html.contains(option), "{option}");
    }
}

#[tokio::test]
async fn board_history_should_hide_the_other_board_from_a_limited_token() {
    let (_dir, fx) = fixture();
    let kestrel = fx
        .db
        .create_board(&fx.user_id, "Kestrel Board")
        .expect("board");
    let staging = fx
        .db
        .create_column(&kestrel, "Staging Lane", None)
        .expect("column");
    let quarry = task_in(&fx, &staging, "Quarry Plan", Vec::new());
    let mut created = event(
        HistoryKind::Created,
        "Task created in Staging Lane",
        "ann",
        "2026-10-10T10:00:00Z",
    );
    created.column_id = Some(staging.clone());
    let mut moved = event(
        HistoryKind::Moved,
        "Moved from Staging Lane to Todo",
        "bob",
        "2026-10-20T10:00:00Z",
    );
    moved.column_id = Some(fx.column.clone());
    moved.from = Some("Staging Lane".to_string());
    moved.to = Some("Todo".to_string());
    let linked = event(
        HistoryKind::LinkAdded,
        &format!("Added link related \"Quarry Plan\" ({quarry})"),
        "ann",
        "2026-10-26T10:00:00Z",
    );
    task_with(&fx, "Local Card", vec![created, moved, linked]);

    let owner = details(&rows(&fx, "").await);
    assert!(
        owner.contains(&"Task created in Staging Lane".to_string()),
        "{owner:?}"
    );
    assert!(
        owner.contains(&"Moved from Staging Lane to Todo".to_string()),
        "{owner:?}"
    );
    assert!(
        owner.contains(&format!("Added link related \"Quarry Plan\" ({quarry})")),
        "{owner:?}"
    );

    let (mut record, raw) = new_api_token_record(&fx.user_id, "one-board", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");
    let bearer = format!("Bearer {raw}");
    let hidden = [
        "Kestrel Board".to_string(),
        "Staging Lane".to_string(),
        "Quarry Plan".to_string(),
        kestrel,
        staging,
        quarry,
    ];

    let uri = format!("/api/boards/{}/history", fx.board_id);
    let (status, json) = send_as(&fx, &bearer, &uri).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    for name in &hidden {
        assert!(!json.contains(name.as_str()), "{name} in {json}");
    }
    let page: Value = serde_json::from_str(&json).expect("json");
    assert_eq!(page["rows"][0]["task"], "Local Card");
    assert_eq!(
        details(&page),
        ["Moved from another board", "Created on another board"]
    );

    let uri = format!("/api/boards/{}/history?format=csv", fx.board_id);
    let (status, csv) = send_as(&fx, &bearer, &uri).await;
    assert_eq!(status, StatusCode::OK, "{csv}");
    assert!(
        csv.contains("\"Time\",\"User\",\"Task\",\"Event\""),
        "{csv}"
    );
    assert!(csv.contains("Created on another board"), "{csv}");
    for name in &hidden {
        assert!(!csv.contains(name.as_str()), "{name} in {csv}");
    }

    let uri = format!("/api/boards/{}/history?format=xlsx", fx.board_id);
    let (status, xlsx) = send_as(&fx, &bearer, &uri).await;
    assert_eq!(status, StatusCode::OK, "{xlsx}");
    assert!(xlsx.contains("Created on another board"), "{xlsx}");
    for name in &hidden {
        assert!(!xlsx.contains(name.as_str()), "{name} in {xlsx}");
    }

    for format in ["csv", "xlsx"] {
        let uri = format!("/api/boards/{}/history?format={format}", fx.board_id);
        let (status, text) = send(&fx, "GET", &uri).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains("Quarry Plan"), "{format}: {text}");
        assert!(text.contains("Staging Lane"), "{format}: {text}");
    }
}
