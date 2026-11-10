use axum::http::StatusCode;
use serde_json::{Value, json};

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, AuthScope};

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    admin: AccountId,
    token: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bulk.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "admin", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            admin,
            token,
        }
    }

    fn board(&self, name: &str, columns: &[&str]) -> (String, Vec<String>) {
        let board = self.db.create_board(&self.admin, name).expect("board");
        self.db.create_swimlane(&board, "Lane").expect("lane");
        let ids = columns
            .iter()
            .map(|column| self.db.create_column(&board, column, None).expect("column"))
            .collect();
        (board, ids)
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {}", self.token);
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    async fn task(&self, column: &str, name: &str) -> String {
        let body = json!({"column_id": column, "name": name});
        let reply = self.send("POST", "/api/tasks", Some(body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["id"].as_str().expect("id").to_string()
    }

    async fn bulk(&self, board: &str, body: Value) -> Reply {
        let uri = format!("/api/boards/{board}/tasks/bulk");
        self.send("POST", &uri, Some(body)).await
    }

    fn cell(&self, board: &str, column: &str) -> Vec<String> {
        let mut tasks: Vec<_> = self
            .db
            .board_tasks(board)
            .expect("tasks")
            .into_iter()
            .filter(|task| task.column_id == column)
            .collect();
        tasks.sort_by(|a, b| a.position.total_cmp(&b.position));
        tasks.into_iter().map(|task| task.name).collect()
    }
}

#[tokio::test]
async fn bulk_move_should_pass_a_wip_limit_only_when_forced() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    fx.db.set_column_wip(&columns[1], Some(1)).expect("limit");
    fx.task(&columns[1], "d").await;
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;

    let body = json!({"task_ids": [a, b], "action": "move", "column_id": columns[1]});
    let refused = fx.bulk(&board, body).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(fx.cell(&board, &columns[1]), ["d"]);

    let body =
        json!({"task_ids": [a, b], "action": "move", "column_id": columns[1], "force": true});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[1]), ["d", "a", "b"]);
}

#[tokio::test]
async fn modal_of_a_task_with_no_content_should_be_bare() {
    let fx = Fixture::new();
    let (_, columns) = fx.board("Plans", &["Todo"]);
    let id = fx.task(&columns[0], "empty").await;

    let reply = fx
        .send("GET", &format!("/api/tasks/{id}/modal"), None)
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    let html = reply.body.as_str().expect("html");
    assert!(html.contains(" tm-bare\""), "{html}");
}

#[tokio::test]
async fn modal_of_a_task_with_a_label_should_not_be_bare() {
    let fx = Fixture::new();
    let (_, columns) = fx.board("Plans", &["Todo"]);
    let id = fx.task(&columns[0], "labelled").await;
    let patch = fx
        .send(
            "PATCH",
            &format!("/api/tasks/{id}"),
            Some(json!({"labels": ["ui"]})),
        )
        .await;
    assert_eq!(patch.status, StatusCode::OK, "{}", patch.body);

    let reply = fx
        .send("GET", &format!("/api/tasks/{id}/modal"), None)
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    let html = reply.body.as_str().expect("html");
    assert!(!html.contains(" tm-bare\""), "{html}");
}

#[tokio::test]
async fn estimated_hours_limit_should_refuse_a_move_past_the_hours_and_show_in_the_header() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    let limit = json!({"wip_limit": 3, "config_json": "{\"wip_limit_type\":\"hours\"}"});
    let reply = fx
        .send(
            "PATCH",
            &format!("/api/columns/{}", columns[1]),
            Some(limit),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let sized = |name: &'static str, hours: f64, column: String| {
        let fx = &fx;
        async move {
            let id = fx.task(&column, name).await;
            let body = json!({"estimate_hours": hours});
            let patch = fx
                .send("PATCH", &format!("/api/tasks/{id}"), Some(body))
                .await;
            assert_eq!(patch.status, StatusCode::OK, "{}", patch.body);
            id
        }
    };
    sized("held", 2.0, columns[1].clone()).await;
    let small = sized("small", 1.0, columns[0].clone()).await;
    let big = sized("big", 1.5, columns[0].clone()).await;
    let to_doing = json!({"column_id": columns[1], "position": 0});

    let refused = fx
        .send(
            "POST",
            &format!("/api/tasks/{big}/move"),
            Some(to_doing.clone()),
        )
        .await;
    let fits = fx
        .send("POST", &format!("/api/tasks/{small}/move"), Some(to_doing))
        .await;

    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(fits.status, StatusCode::OK, "{}", fits.body);
    let page = fx.send("GET", &format!("/b/{board}"), None).await;
    let html = page.body.as_str().expect("html");
    assert!(html.contains(">3h / 3h<"), "{html}");
}
