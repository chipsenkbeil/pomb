use axum::http::StatusCode;
use serde_json::{Value, json};

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call, held_disk};
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
    board: String,
    columns: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wip.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "admin", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode())
                .with_disk(held_disk(&db))
                .with_limits(unthrottled_limits()),
        );
        let board = db.create_board(&admin, "Plans").expect("board");
        db.create_swimlane(&board, "Lane").expect("lane");
        let columns = ["Todo", "Doing", "Review"]
            .iter()
            .map(|name| db.create_column(&board, name, None).expect("column"))
            .collect();
        Self {
            _dir: dir,
            app,
            db,
            admin,
            token,
            board,
            columns,
        }
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

    fn limit(&self, column: usize, limit: i64) {
        let column = &self.columns[column];
        self.db.set_column_wip(column, Some(limit)).expect("limit");
    }

    fn held(&self, column: usize) -> usize {
        let column = &self.columns[column];
        let tasks = self.db.board_tasks(&self.board).expect("tasks");
        tasks
            .iter()
            .filter(|task| &task.column_id == column)
            .count()
    }

    async fn full_doing(&self) {
        self.limit(1, 1);
        self.task(&self.columns[1], "held").await;
    }

    async fn bin(&self, id: &str) {
        let reply = self.send("DELETE", &format!("/api/tasks/{id}"), None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }
}

fn assert_refused(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.body["code"], "wip_limit_reached", "{}", reply.body);
}

#[tokio::test]
async fn create_should_be_refused_in_a_full_column_and_create_nothing() {
    let fx = Fixture::new();
    fx.full_doing().await;

    let body = json!({"column_id": fx.columns[1], "name": "extra"});
    let reply = fx.send("POST", "/api/tasks", Some(body)).await;

    assert_refused(&reply);
    assert_eq!(fx.held(1), 1);
}

#[tokio::test]
async fn create_should_succeed_in_a_full_column_with_force_and_show_it_over_the_limit() {
    let fx = Fixture::new();
    fx.full_doing().await;

    let body = json!({"column_id": fx.columns[1], "name": "extra", "force": true});
    let reply = fx.send("POST", "/api/tasks", Some(body)).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(fx.held(1), 2);
}

#[tokio::test]
async fn create_should_succeed_in_a_column_with_room() {
    let fx = Fixture::new();
    fx.limit(1, 2);
    fx.task(&fx.columns[1], "held").await;

    let body = json!({"column_id": fx.columns[1], "name": "extra"});
    let reply = fx.send("POST", "/api/tasks", Some(body)).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(fx.held(1), 2);
}

#[tokio::test]
async fn create_should_be_refused_in_a_full_limit_group_unless_forced() {
    let fx = Fixture::new();
    let columns = vec![fx.columns[1].clone(), fx.columns[2].clone()];
    fx.db.add_limit_group(&fx.board, columns, 1).expect("group");
    fx.task(&fx.columns[1], "held").await;

    let body = json!({"column_id": fx.columns[2], "name": "extra"});
    let refused = fx.send("POST", "/api/tasks", Some(body)).await;
    let body = json!({"column_id": fx.columns[2], "name": "extra", "force": true});
    let forced = fx.send("POST", "/api/tasks", Some(body)).await;

    assert_refused(&refused);
    assert_eq!(forced.status, StatusCode::CREATED, "{}", forced.body);
    assert_eq!(fx.held(1) + fx.held(2), 2);
}

#[tokio::test]
async fn convert_subtask_should_be_refused_in_a_full_column_unless_forced() {
    let fx = Fixture::new();
    fx.full_doing().await;
    let parent = fx.task(&fx.columns[0], "parent").await;
    let uri = format!("/api/tasks/{parent}/subtasks");
    let sub = fx.send("POST", &uri, Some(json!({"name": "part"}))).await;
    let convert = format!("{uri}/{}/convert", sub.body["id"].as_str().expect("id"));

    let body = json!({"column_id": fx.columns[1]});
    let refused = fx.send("POST", &convert, Some(body)).await;
    assert_refused(&refused);
    assert_eq!(fx.held(1), 1);

    let body = json!({"column_id": fx.columns[1], "force": true});
    let forced = fx.send("POST", &convert, Some(body)).await;
    assert_eq!(forced.status, StatusCode::CREATED, "{}", forced.body);
    assert_eq!(fx.held(1), 2);
}

#[tokio::test]
async fn copy_should_be_refused_when_a_column_ends_over_its_limit_unless_forced() {
    let fx = Fixture::new();
    fx.limit(1, 1);
    fx.task(&fx.columns[1], "one").await;
    let over = json!({"column_id": fx.columns[1], "name": "two", "force": true});
    fx.send("POST", "/api/tasks", Some(over)).await;
    let boards = fx.db.list_boards().expect("boards").len();

    let body = json!({"name": "Copy", "copy_board_id": fx.board, "copy_tasks": true});
    let refused = fx.send("POST", "/api/boards", Some(body)).await;
    assert_refused(&refused);
    assert_eq!(fx.db.list_boards().expect("boards").len(), boards);

    let body =
        json!({"name": "Copy", "copy_board_id": fx.board, "copy_tasks": true, "force": true});
    let forced = fx.send("POST", "/api/boards", Some(body)).await;
    assert_eq!(forced.status, StatusCode::OK, "{}", forced.body);
    let copy = forced.body["id"].as_str().expect("id");
    assert_eq!(fx.db.board_tasks(copy).expect("tasks").len(), 2);
}

#[tokio::test]
async fn copy_should_succeed_when_every_column_fits_its_limit() {
    let fx = Fixture::new();
    fx.limit(1, 2);
    fx.task(&fx.columns[1], "one").await;

    let body = json!({"name": "Copy", "copy_board_id": fx.board, "copy_tasks": true});
    let reply = fx.send("POST", "/api/boards", Some(body)).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn bulk_move_should_be_refused_into_a_full_column_and_move_nothing() {
    let fx = Fixture::new();
    fx.full_doing().await;
    let a = fx.task(&fx.columns[0], "a").await;
    let b = fx.task(&fx.columns[0], "b").await;

    let body = json!({"task_ids": [a, b], "action": "move", "column_id": fx.columns[1]});
    let reply = bulk(&fx, body).await;

    assert_refused(&reply);
    assert_eq!(fx.held(0), 2);
    assert_eq!(fx.held(1), 1);
}

#[tokio::test]
async fn bulk_move_should_refuse_when_only_the_last_task_passes_the_limit() {
    let fx = Fixture::new();
    fx.limit(1, 2);
    fx.task(&fx.columns[1], "held").await;
    let a = fx.task(&fx.columns[0], "a").await;
    let b = fx.task(&fx.columns[0], "b").await;

    let body = json!({"task_ids": [a, b], "action": "move", "column_id": fx.columns[1]});
    let reply = bulk(&fx, body).await;

    assert_refused(&reply);
    assert_eq!(fx.held(0), 2);
    assert_eq!(fx.held(1), 1);
}

#[tokio::test]
async fn bulk_move_should_succeed_with_force_and_show_the_column_over_the_limit() {
    let fx = Fixture::new();
    fx.full_doing().await;
    let a = fx.task(&fx.columns[0], "a").await;
    let b = fx.task(&fx.columns[0], "b").await;

    let body =
        json!({"task_ids": [a, b], "action": "move", "column_id": fx.columns[1], "force": true});
    let reply = bulk(&fx, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.held(1), 3);
}

#[tokio::test]
async fn bulk_move_should_succeed_into_a_column_with_room() {
    let fx = Fixture::new();
    fx.limit(1, 2);
    let a = fx.task(&fx.columns[0], "a").await;
    let b = fx.task(&fx.columns[0], "b").await;

    let body = json!({"task_ids": [a, b], "action": "move", "column_id": fx.columns[1]});
    let reply = bulk(&fx, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.held(1), 2);
}

#[tokio::test]
async fn bulk_move_should_be_refused_into_a_full_limit_group_unless_forced() {
    let fx = Fixture::new();
    let columns = vec![fx.columns[1].clone(), fx.columns[2].clone()];
    fx.db.add_limit_group(&fx.board, columns, 1).expect("group");
    fx.task(&fx.columns[1], "held").await;
    let a = fx.task(&fx.columns[0], "a").await;

    let body = json!({"task_ids": [a], "action": "move", "column_id": fx.columns[2]});
    let refused = bulk(&fx, body).await;
    let body =
        json!({"task_ids": [a], "action": "move", "column_id": fx.columns[2], "force": true});
    let forced = bulk(&fx, body).await;

    assert_refused(&refused);
    assert_eq!(forced.status, StatusCode::OK, "{}", forced.body);
    assert_eq!(fx.held(2), 1);
}

#[tokio::test]
async fn bulk_move_to_another_board_should_be_refused_when_its_column_is_full() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Ops").expect("board");
    fx.db.create_swimlane(&other, "Lane").expect("lane");
    let queue = fx.db.create_column(&other, "Queue", None).expect("column");
    fx.db.set_column_wip(&queue, Some(1)).expect("limit");
    fx.task(&queue, "held").await;
    let a = fx.task(&fx.columns[0], "a").await;

    let body = json!({"task_ids": [a], "action": "move", "column_id": queue});
    let refused = bulk(&fx, body).await;
    assert_refused(&refused);

    let body = json!({"task_ids": [a], "action": "move", "column_id": queue, "force": true});
    let forced = bulk(&fx, body).await;
    assert_eq!(forced.status, StatusCode::OK, "{}", forced.body);
    assert_eq!(fx.db.board_tasks(&other).expect("tasks").len(), 2);
}

#[tokio::test]
async fn move_to_another_board_should_be_refused_when_its_column_is_full_unless_forced() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Ops").expect("board");
    fx.db.create_swimlane(&other, "Lane").expect("lane");
    let queue = fx.db.create_column(&other, "Queue", None).expect("column");
    fx.db.set_column_wip(&queue, Some(1)).expect("limit");
    fx.task(&queue, "held").await;
    let a = fx.task(&fx.columns[0], "a").await;
    let body = json!({"column_id": queue});

    let refused = fx
        .send("POST", &format!("/api/tasks/{a}/move"), Some(body.clone()))
        .await;
    assert_refused(&refused);

    let forced = fx
        .send(
            "POST",
            &format!("/api/tasks/{a}/move?force=true"),
            Some(body),
        )
        .await;
    assert_eq!(forced.status, StatusCode::OK, "{}", forced.body);
    assert_eq!(fx.db.board_tasks(&other).expect("tasks").len(), 2);
}

#[tokio::test]
async fn restore_should_be_refused_into_a_full_column_and_keep_the_task_in_the_bin() {
    let fx = Fixture::new();
    fx.limit(1, 1);
    let id = fx.task(&fx.columns[1], "binned").await;
    fx.bin(&id).await;
    fx.task(&fx.columns[1], "held").await;

    let refused = fx
        .send("POST", &format!("/api/bin/{id}/restore"), None)
        .await;

    assert_refused(&refused);
    assert_eq!(fx.held(1), 1);
    let again = fx
        .send("POST", &format!("/api/bin/{id}/restore?force=true"), None)
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(fx.held(1), 2);
}

#[tokio::test]
async fn restore_should_succeed_into_a_column_with_room() {
    let fx = Fixture::new();
    fx.limit(1, 1);
    let id = fx.task(&fx.columns[1], "binned").await;
    fx.bin(&id).await;

    let reply = fx
        .send("POST", &format!("/api/bin/{id}/restore"), None)
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.held(1), 1);
}

#[tokio::test]
async fn inbound_mail_should_be_refused_into_a_full_column_unless_forced() {
    let fx = Fixture::new();
    fx.full_doing().await;
    let body = json!({"name": "Mail", "column_id": fx.columns[1]});
    let uri = format!("/api/boards/{}/inbound-addresses", fx.board);
    let created = fx.send("POST", &uri, Some(body)).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let address = format!(
        "/api/inbound/{}",
        created.body["address"].as_str().expect("address")
    );

    let refused = call(
        &fx.app,
        "POST",
        &address,
        &[],
        Some(json!({"subject": "Hi"})),
    )
    .await;
    assert_refused(&refused);
    assert_eq!(fx.held(1), 1);

    let mail = json!({"subject": "Hi", "force": true});
    let forced = call(&fx.app, "POST", &address, &[], Some(mail)).await;
    assert_eq!(forced.status, StatusCode::CREATED, "{}", forced.body);
    assert_eq!(fx.held(1), 2);
}

async fn bulk(fx: &Fixture, body: Value) -> Reply {
    let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
    fx.send("POST", &uri, Some(body)).await
}

#[tokio::test]
async fn convert_subtask_should_succeed_into_a_column_with_room() {
    let fx = Fixture::new();
    fx.limit(1, 2);
    fx.task(&fx.columns[1], "held").await;
    let parent = fx.task(&fx.columns[0], "parent").await;
    let uri = format!("/api/tasks/{parent}/subtasks");
    let sub = fx.send("POST", &uri, Some(json!({"name": "part"}))).await;
    let convert = format!("{uri}/{}/convert", sub.body["id"].as_str().expect("id"));

    let body = json!({"column_id": fx.columns[1]});
    let reply = fx.send("POST", &convert, Some(body)).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(fx.held(1), 2);
}

#[tokio::test]
async fn inbound_mail_should_succeed_into_a_column_with_room() {
    let fx = Fixture::new();
    fx.limit(1, 2);
    fx.task(&fx.columns[1], "held").await;
    let body = json!({"name": "Mail", "column_id": fx.columns[1]});
    let uri = format!("/api/boards/{}/inbound-addresses", fx.board);
    let created = fx.send("POST", &uri, Some(body)).await;
    let address = format!(
        "/api/inbound/{}",
        created.body["address"].as_str().expect("address")
    );

    let reply = call(
        &fx.app,
        "POST",
        &address,
        &[],
        Some(json!({"subject": "Hi"})),
    )
    .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(fx.held(1), 2);
}

#[tokio::test]
async fn move_to_another_board_should_succeed_into_a_column_with_room() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Ops").expect("board");
    fx.db.create_swimlane(&other, "Lane").expect("lane");
    let queue = fx.db.create_column(&other, "Queue", None).expect("column");
    fx.db.set_column_wip(&queue, Some(2)).expect("limit");
    fx.task(&queue, "held").await;
    let single = fx.task(&fx.columns[0], "single").await;
    let bulk_one = fx.task(&fx.columns[0], "bulk").await;

    let body = json!({"column_id": queue});
    let moved = fx
        .send("POST", &format!("/api/tasks/{single}/move"), Some(body))
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);

    let body = json!({"task_ids": [bulk_one], "action": "move", "column_id": queue});
    let refused = bulk(&fx, body).await;
    assert_refused(&refused);
    assert_eq!(fx.db.board_tasks(&other).expect("tasks").len(), 2);
}

#[tokio::test]
async fn bulk_move_to_another_board_should_succeed_into_a_column_with_room() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Ops").expect("board");
    fx.db.create_swimlane(&other, "Lane").expect("lane");
    let queue = fx.db.create_column(&other, "Queue", None).expect("column");
    fx.db.set_column_wip(&queue, Some(2)).expect("limit");
    let a = fx.task(&fx.columns[0], "a").await;

    let body = json!({"task_ids": [a], "action": "move", "column_id": queue});
    let reply = bulk(&fx, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.db.board_tasks(&other).expect("tasks").len(), 1);
}
