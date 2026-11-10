use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, AuthScope, InstanceMode, NotificationKind, NotificationRow};

/// An instance with an admin who owns two boards and an `agent-1` token.
struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    app: Router,
    admin: AccountId,
    board: String,
    todo: String,
    done: String,
    spare: String,
    token: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notifications.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "agent-1", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let board = db.create_board(&admin, "Work").expect("board");
        let todo = db.create_column(&board, "Todo", None).expect("column");
        let done = db.create_column(&board, "Done", None).expect("column");
        let spare = db.create_board(&admin, "Spare").expect("board");
        db.create_column(&spare, "Todo", None).expect("column");
        let state = AppState::new(db.clone(), None, passkey_mode())
            .with_mode(InstanceMode::Multi)
            .with_limits(unthrottled_limits());
        Self {
            app: router(state),
            _dir: dir,
            db,
            admin,
            board,
            todo,
            done,
            spare,
            token,
        }
    }

    /// Returns a task the admin is a member of.
    fn followed(&self, name: &str) -> String {
        let task = self.unfollowed(name);
        self.db
            .set_task_members(&task, &[self.admin.as_str().to_string()])
            .expect("members");
        task
    }

    /// Returns a task nobody follows.
    fn unfollowed(&self, name: &str) -> String {
        self.db
            .create_task(&self.todo, None, name, 1, None)
            .expect("task")
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let auth = format!("Bearer {}", self.token);
        let reply = call(&self.app, method, uri, &[("authorization", &auth)], body).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{method} {uri}: {}",
            reply.body
        );
        reply
    }

    /// Returns the admin's notifications, newest first.
    fn notifications(&self) -> Vec<NotificationRow> {
        self.db
            .notifications(&self.admin, None, 100)
            .expect("notifications")
            .items
            .into_iter()
            .map(|(_, row)| row)
            .collect()
    }
}

#[tokio::test]
async fn a_token_assigning_the_owner_should_notify_them() {
    let fx = Fixture::new();
    let task = fx.unfollowed("Assign me");
    let uri = format!("/api/tasks/{task}");
    fx.send(
        "PATCH",
        &uri,
        Some(json!({"member_ids": [fx.admin.as_str()]})),
    )
    .await;
    let rows = fx.notifications();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.kind, NotificationKind::TaskAssigned);
    assert_eq!(row.task_id.as_deref(), Some(task.as_str()));
    assert_eq!(row.task_name.as_deref(), Some("Assign me"));
    assert_eq!(
        (row.board_id.as_str(), row.board_name.as_str()),
        (fx.board.as_str(), "Work")
    );
    assert_eq!(row.token_name.as_deref(), Some("agent-1"));
    assert!(!row.read);
    fx.send("PATCH", &uri, Some(json!({"name": "Renamed"})))
        .await;
    assert_eq!(fx.notifications().len(), 1, "a rename does not assign");
}

#[tokio::test]
async fn a_token_comment_on_a_followed_task_should_notify_the_owner() {
    let fx = Fixture::new();
    let task = fx.followed("Talk");
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"body": "hello"})),
    )
    .await;
    let rows = fx.notifications();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, NotificationKind::CommentAdded);
    assert_eq!(rows[0].task_name.as_deref(), Some("Talk"));
    assert_eq!(rows[0].token_name.as_deref(), Some("agent-1"));
}

#[tokio::test]
async fn a_token_moving_a_followed_task_should_notify_with_the_column_it_went_to() {
    let fx = Fixture::new();
    let task = fx.followed("Ship");
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/move"),
        Some(json!({"column_id": fx.done, "position": 0})),
    )
    .await;
    let rows = fx.notifications();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, NotificationKind::TaskMoved);
    assert_eq!(rows[0].detail.as_deref(), Some("Done"));
}

#[tokio::test]
async fn a_token_deleting_a_followed_task_should_notify_the_owner() {
    let fx = Fixture::new();
    let task = fx.followed("Doomed");
    fx.send("DELETE", &format!("/api/tasks/{task}"), None).await;
    let rows = fx.notifications();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, NotificationKind::TaskDeleted);
    assert_eq!(rows[0].task_name.as_deref(), Some("Doomed"));
}

#[tokio::test]
async fn a_watched_task_should_notify_like_a_task_with_the_owner_as_member() {
    let fx = Fixture::new();
    let task = fx.unfollowed("Watched");
    fx.db.set_task_watched(&task, true).expect("watch");
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"body": "hello"})),
    )
    .await;
    assert_eq!(fx.notifications().len(), 1);
}

#[tokio::test]
async fn changes_to_a_task_nobody_follows_should_notify_no_one() {
    let fx = Fixture::new();
    let task = fx.unfollowed("Quiet");
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"body": "hello"})),
    )
    .await;
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/move"),
        Some(json!({"column_id": fx.done, "position": 0})),
    )
    .await;
    fx.send("DELETE", &format!("/api/tasks/{task}"), None).await;
    assert!(fx.notifications().is_empty());
}

#[tokio::test]
async fn a_token_renaming_a_column_should_notify_with_the_new_name() {
    let fx = Fixture::new();
    fx.send(
        "PATCH",
        &format!("/api/columns/{}", fx.done),
        Some(json!({"name": "Shipped"})),
    )
    .await;
    let rows = fx.notifications();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, NotificationKind::ColumnRenamed);
    assert_eq!(rows[0].detail.as_deref(), Some("Shipped"));
    fx.send(
        "PATCH",
        &format!("/api/columns/{}", fx.done),
        Some(json!({"name": "Shipped", "collapsed": true})),
    )
    .await;
    assert_eq!(
        fx.notifications().len(),
        1,
        "an unchanged name is no rename"
    );
}

#[tokio::test]
async fn a_token_deleting_a_column_should_notify_with_its_name() {
    let fx = Fixture::new();
    fx.send("DELETE", &format!("/api/columns/{}", fx.done), None)
        .await;
    let rows = fx.notifications();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, NotificationKind::ColumnDeleted);
    assert_eq!(rows[0].detail.as_deref(), Some("Done"));
}

#[tokio::test]
async fn a_token_deleting_a_board_should_notify_with_its_name() {
    let fx = Fixture::new();
    fx.send("DELETE", &format!("/api/boards/{}", fx.spare), None)
        .await;
    let rows = fx.notifications();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, NotificationKind::BoardDeleted);
    assert_eq!(rows[0].board_name, "Spare");
    assert_eq!(rows[0].board_id, fx.spare);
}

#[tokio::test]
async fn the_settings_page_should_list_every_event_with_an_in_app_checkbox() {
    let fx = Fixture::new();
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({"muted_notifications": ["comment_added"]})),
    )
    .await;
    let cookie = format!(
        "session={}",
        fx.db.create_session(&fx.admin, "").expect("session")
    );
    let page = call(&fx.app, "GET", "/settings", &[("cookie", &cookie)], None).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = page.body.as_str().expect("html");
    for kind in NotificationKind::ALL {
        let name = serde_json::to_value(kind).expect("kind");
        let input = format!("data-kind=\"{}\"", name.as_str().expect("name"));
        assert!(html.contains(&input), "{input}");
    }
    for title in ["Task", "Column", "Board"] {
        assert!(
            html.contains(&format!("<legend>{title}</legend>")),
            "{title}"
        );
    }
    let toggle = |kind: &str| {
        html.split(format!("data-kind=\"{kind}\"").as_str())
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .expect("toggle")
            .to_string()
    };
    assert!(
        !toggle("comment_added").contains("checked"),
        "a muted kind is unchecked"
    );
    assert!(
        toggle("task_moved").contains("checked"),
        "an unmuted kind is checked"
    );
    assert!(html.contains("href=\"/settings#notifications\""), "gear");
}

#[tokio::test]
async fn saving_the_settings_form_should_keep_the_muted_kinds() {
    let fx = Fixture::new();
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({"muted_notifications": ["task_moved"]})),
    )
    .await;
    let cookie = format!(
        "session={}",
        fx.db.create_session(&fx.admin, "").expect("session")
    );
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/settings")
        .header("cookie", cookie)
        .header("host", "localhost")
        .header("origin", "http://localhost")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from("pomodoro_minutes=30"))
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    assert!(response.status().is_redirection(), "{}", response.status());
    let settings = fx.db.settings(&fx.admin).expect("settings");
    assert_eq!(settings.pomodoro_minutes, 30);
    assert_eq!(settings.muted_notifications, [NotificationKind::TaskMoved]);
}

#[tokio::test]
async fn muting_a_kind_should_stop_new_notifications_of_that_kind() {
    let fx = Fixture::new();
    let task = fx.followed("Quiet comments");
    let saved = fx
        .send(
            "PUT",
            "/api/settings",
            Some(json!({"muted_notifications": ["comment_added", "comment_added"]})),
        )
        .await;
    assert_eq!(saved.body["muted_notifications"], json!(["comment_added"]));
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"body": "hello"})),
    )
    .await;
    assert!(fx.notifications().is_empty());
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/move"),
        Some(json!({"column_id": fx.done, "position": 0})),
    )
    .await;
    assert_eq!(fx.notifications().len(), 1, "other kinds still notify");
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({"muted_notifications": []})),
    )
    .await;
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"body": "again"})),
    )
    .await;
    assert_eq!(fx.notifications().len(), 2);
}

fn seeded_token(fx: &Fixture, scope: AuthScope) -> String {
    let (record, token) = new_api_token_record(&fx.admin, "scoped", scope, None);
    fx.db.create_api_token(&record).expect("token");
    token
}

async fn over_bearer(fx: &Fixture, token: &str, method: &str, uri: &str) -> Reply {
    let auth = format!("Bearer {token}");
    call(&fx.app, method, uri, &[("authorization", &auth)], None).await
}

#[tokio::test]
async fn a_read_token_should_list_notifications_over_bearer() {
    let fx = Fixture::new();
    let task = fx.followed("Moved");
    fx.send("DELETE", &format!("/api/tasks/{task}"), None).await;
    let read = seeded_token(&fx, AuthScope::Read);
    let reply = over_bearer(&fx, &read, "GET", "/api/notifications").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["unread"], 1, "{}", reply.body);
}

#[tokio::test]
async fn an_admin_token_should_mark_read_and_delete_notifications_over_bearer() {
    let fx = Fixture::new();
    let task = fx.followed("Doomed");
    fx.send("DELETE", &format!("/api/tasks/{task}"), None).await;
    let list = over_bearer(&fx, &fx.token, "GET", "/api/notifications").await;
    let id = list.body["items"][0]["id"].as_u64().expect("id");
    let read_uri = format!("/api/notifications/{id}/read");
    let marked = over_bearer(&fx, &fx.token, "POST", &read_uri).await;
    assert_eq!(marked.status, StatusCode::OK, "{}", marked.body);
    assert_eq!(marked.body["unread"], 0, "{}", marked.body);
    let gone = over_bearer(
        &fx,
        &fx.token,
        "DELETE",
        &format!("/api/notifications/{id}"),
    )
    .await;
    assert_eq!(gone.status, StatusCode::OK, "{}", gone.body);
    assert!(fx.notifications().is_empty());
    let all = over_bearer(&fx, &fx.token, "DELETE", "/api/notifications").await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.body);
}

#[tokio::test]
async fn a_read_token_should_be_denied_writes_over_bearer_with_permission_denied() {
    let fx = Fixture::new();
    let read = seeded_token(&fx, AuthScope::Read);
    for (method, uri) in [
        ("POST", "/api/notifications/read"),
        ("POST", "/api/notifications/1/read"),
        ("DELETE", "/api/notifications"),
        ("DELETE", "/api/notifications/1"),
    ] {
        let reply = over_bearer(&fx, &read, method, uri).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.body["code"], "permission_denied", "{method} {uri}");
    }
}
