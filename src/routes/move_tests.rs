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
        let path = dir.path().join("move.redb");
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

    async fn top_placement(&self, board: &str) {
        let body = json!({"new_tasks_at_top": true});
        let reply = self
            .send("PUT", &format!("/api/boards/{board}/config"), Some(body))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
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
async fn single_move_without_a_position_should_follow_the_new_task_position_setting() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    fx.task(&columns[1], "d").await;
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;
    let to_doing = json!({"column_id": columns[1]});

    let at_bottom = fx
        .send(
            "POST",
            &format!("/api/tasks/{a}/move"),
            Some(to_doing.clone()),
        )
        .await;
    fx.top_placement(&board).await;
    let at_top = fx
        .send("POST", &format!("/api/tasks/{b}/move"), Some(to_doing))
        .await;

    assert_eq!(at_bottom.status, StatusCode::OK, "{}", at_bottom.body);
    assert_eq!(at_top.status, StatusCode::OK, "{}", at_top.body);
    assert_eq!(fx.cell(&board, &columns[1]), ["b", "d", "a"]);
}

#[tokio::test]
async fn single_move_with_a_position_should_ignore_the_new_task_position_setting() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    fx.top_placement(&board).await;
    fx.task(&columns[1], "d").await;
    let a = fx.task(&columns[0], "a").await;
    let body = json!({"column_id": columns[1], "position": 1});

    let reply = fx
        .send("POST", &format!("/api/tasks/{a}/move"), Some(body))
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[1]), ["d", "a"]);
}
