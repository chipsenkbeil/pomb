use axum::http::StatusCode;
use serde_json::{Value, json};

use super::passkey_tests::{Reply, call};
use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{ChangeOutcome, Db};
use crate::models::{AccountId, Actor, AuthScope, color_value_to_size};

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
async fn bulk_move_should_keep_the_given_order_in_the_target_cell() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    let a = fx.task(&columns[0], "a").await;
    fx.task(&columns[0], "b").await;
    let c = fx.task(&columns[0], "c").await;
    fx.task(&columns[1], "d").await;
    fx.task(&columns[1], "e").await;

    let body =
        json!({"task_ids": [c, a], "action": "move", "column_id": columns[1], "position": 1});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["count"], 2);
    assert_eq!(fx.cell(&board, &columns[1]), ["d", "c", "a", "e"]);
    assert_eq!(fx.cell(&board, &columns[0]), ["b"]);
}

#[tokio::test]
async fn bulk_move_should_not_count_tasks_of_the_batch_already_in_the_target_cell() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    fx.task(&columns[0], "b").await;
    let c = fx.task(&columns[0], "c").await;
    fx.task(&columns[0], "d").await;

    let body =
        json!({"task_ids": [c, a], "action": "move", "column_id": columns[0], "position": 2});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[0]), ["b", "d", "c", "a"]);
}

#[tokio::test]
async fn bulk_move_should_give_each_task_of_a_group_drag_its_own_place_in_the_same_column() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    fx.task(&columns[0], "b").await;
    let c = fx.task(&columns[0], "c").await;
    fx.task(&columns[0], "d").await;

    let body =
        json!({"task_ids": [c, a], "action": "move", "column_id": columns[0], "position": 0});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[0]), ["c", "a", "b", "d"]);
}

#[tokio::test]
async fn bulk_move_should_not_be_blocked_by_a_wip_limit() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    fx.db.set_column_wip(&columns[1], Some(1)).expect("limit");
    fx.task(&columns[1], "d").await;
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;

    let body = json!({"task_ids": [a, b], "action": "move", "column_id": columns[1]});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[1]), ["a", "b", "d"]);
}

#[tokio::test]
async fn bulk_move_should_write_a_moved_event_for_each_task_that_changes_column() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[1], "b").await;

    let body = json!({"task_ids": [a, b], "action": "move", "column_id": columns[1]});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let details = |id: &str| -> Vec<String> {
        let task = fx.db.task(id).expect("read").expect("task");
        task.history
            .iter()
            .filter(|event| event.kind == "moved")
            .map(|event| event.detail.clone())
            .collect()
    };
    assert_eq!(details(&a), ["Moved from Todo to Doing"]);
    assert!(details(&b).is_empty());
}

#[tokio::test]
async fn bulk_move_should_change_nothing_when_one_task_is_on_another_board() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    let (other_board, other) = fx.board("Ops", &["Queue"]);
    let a = fx.task(&columns[0], "a").await;
    let x = fx.task(&other[0], "x").await;

    let body = json!({"task_ids": [a, x], "action": "move", "column_id": columns[1]});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[0]), ["a"]);
    assert_eq!(fx.cell(&other_board, &other[0]), ["x"]);
}

#[tokio::test]
async fn bulk_move_should_change_nothing_when_a_task_is_missing() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    let a = fx.task(&columns[0], "a").await;

    let body = json!({"task_ids": [a, "missing"], "action": "move", "column_id": columns[1]});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[0]), ["a"]);
}

#[tokio::test]
async fn bulk_move_should_take_tasks_to_another_board_in_order() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let (other_board, other) = fx.board("Ops", &["Queue"]);
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;
    fx.task(&other[0], "x").await;

    let body = json!({"task_ids": [b, a], "action": "move", "column_id": other[0]});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.cell(&other_board, &other[0]), ["b", "a", "x"]);
    assert!(fx.cell(&board, &columns[0]).is_empty());
    let lane = fx.db.list_swimlanes(&other_board).expect("lanes")[0]
        .id
        .clone();
    let moved = fx.db.task(&a).expect("read").expect("task");
    assert_eq!(moved.swimlane_id.as_deref(), Some(lane.as_str()));
}

#[tokio::test]
async fn bulk_move_should_refuse_no_tasks_and_a_repeated_task() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    let a = fx.task(&columns[0], "a").await;

    let none = json!({"task_ids": [], "action": "move", "column_id": columns[1]});
    let repeated = json!({"task_ids": [a, a], "action": "move", "column_id": columns[1]});

    assert_eq!(fx.bulk(&board, none).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        fx.bulk(&board, repeated).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fx.cell(&board, &columns[0]), ["a"]);
}

#[tokio::test]
async fn bulk_color_should_recolor_every_task_and_write_an_event_for_each() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;
    let colors = fx.db.list_colors(&board).expect("colors");
    let red = colors.iter().find(|c| c.value == "red").expect("red");

    let body = json!({"task_ids": [a, b], "action": "color", "color_id": red.id});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    for id in [&a, &b] {
        let task = fx.db.task(id).expect("read").expect("task");
        assert_eq!(task.color_id.as_deref(), Some(red.id.as_str()));
        let events = task
            .history
            .iter()
            .filter(|e| e.kind == "color_updated")
            .count();
        assert_eq!(events, 1);
    }
}

#[tokio::test]
async fn bulk_color_should_change_nothing_for_an_unknown_color() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let before = fx.db.task(&a).expect("read").expect("task").color_id;

    let body = json!({"task_ids": [a], "action": "color", "color_id": "nope"});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(
        fx.db.task(&a).expect("read").expect("task").color_id,
        before
    );
}

#[tokio::test]
async fn bulk_members_should_add_and_remove_on_every_task_with_an_event_each() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;
    let me = fx.admin.as_str().to_string();

    let add = json!({"task_ids": [a, b], "action": "members", "add_member_ids": [me]});
    assert_eq!(fx.bulk(&board, add).await.status, StatusCode::OK);
    for id in [&a, &b] {
        let task = fx.db.task(id).expect("read").expect("task");
        assert_eq!(task.member_ids, std::slice::from_ref(&me));
        let events = task
            .history
            .iter()
            .filter(|e| e.kind == "members_updated")
            .count();
        assert_eq!(events, 1, "{:?}", task.history);
    }

    let remove = json!({"task_ids": [a, b], "action": "members", "remove_member_ids": [me]});
    assert_eq!(fx.bulk(&board, remove).await.status, StatusCode::OK);
    for id in [&a, &b] {
        assert!(
            fx.db
                .task(id)
                .expect("read")
                .expect("task")
                .member_ids
                .is_empty()
        );
    }
}

#[tokio::test]
async fn bulk_unassign_should_clear_the_members_of_every_task() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;
    let me = fx.admin.as_str().to_string();
    let add = json!({"task_ids": [a], "action": "members", "add_member_ids": [me]});
    assert_eq!(fx.bulk(&board, add).await.status, StatusCode::OK);

    let body = json!({"task_ids": [a, b], "action": "unassign_members"});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    for id in [&a, &b] {
        assert!(
            fx.db
                .task(id)
                .expect("read")
                .expect("task")
                .member_ids
                .is_empty()
        );
    }
}

#[tokio::test]
async fn bulk_members_should_change_nothing_for_an_unknown_member() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;

    let body = json!({"task_ids": [a], "action": "members", "add_member_ids": ["ghost"]});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert!(
        fx.db
            .task(&a)
            .expect("read")
            .expect("task")
            .member_ids
            .is_empty()
    );
}

#[tokio::test]
async fn bulk_color_should_set_the_size_of_the_color_without_an_estimate_event() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let colors = fx.db.list_colors(&board).expect("colors");
    let red = colors.iter().find(|c| c.value == "red").expect("red");

    let body = json!({"task_ids": [a], "action": "color", "color_id": red.id});
    assert_eq!(fx.bulk(&board, body).await.status, StatusCode::OK);

    let task = fx.db.task(&a).expect("read").expect("task");
    assert_eq!(task.size, color_value_to_size("red"));
    assert!(
        task.history.iter().all(|e| e.kind != "estimate_updated"),
        "{:?}",
        task.history
    );
}

#[tokio::test]
async fn bulk_members_should_keep_the_responsible_consistent_with_the_members() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let me = fx.admin.as_str().to_string();

    let add = json!({"task_ids": [a], "action": "members", "add_member_ids": [me]});
    assert_eq!(fx.bulk(&board, add).await.status, StatusCode::OK);
    assert!(
        fx.db
            .task(&a)
            .expect("read")
            .expect("task")
            .responsible
            .is_some()
    );

    let unassign = json!({"task_ids": [a], "action": "unassign_members"});
    assert_eq!(fx.bulk(&board, unassign).await.status, StatusCode::OK);
    assert!(
        fx.db
            .task(&a)
            .expect("read")
            .expect("task")
            .responsible
            .is_none()
    );
}

#[tokio::test]
async fn bulk_change_should_write_nothing_when_a_later_task_is_missing() {
    let fx = Fixture::new();
    let (_board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let ids = [a.clone(), "missing".to_string()];

    let outcome = fx
        .db
        .apply_tasks_change(&ids, Actor::SYSTEM, |task, _| {
            task.name = "changed".to_string();
            Ok::<(), ()>(())
        })
        .expect("change");

    assert_eq!(outcome, ChangeOutcome::NotFound);
    assert_eq!(fx.db.task(&a).expect("read").expect("task").name, "a");
}

#[tokio::test]
async fn bulk_change_should_write_nothing_when_a_later_task_is_refused() {
    let fx = Fixture::new();
    let (_board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;
    let ids = [a.clone(), b.clone()];

    let outcome = fx
        .db
        .apply_tasks_change(&ids, Actor::SYSTEM, |task, _| {
            if task.id == b {
                return Err("refused");
            }
            task.name = "changed".to_string();
            Ok(())
        })
        .expect("change");

    assert_eq!(outcome, ChangeOutcome::Refused("refused"));
    assert_eq!(fx.db.task(&a).expect("read").expect("task").name, "a");
}

#[tokio::test]
async fn bulk_should_refuse_a_color_without_an_id_and_an_empty_members_change() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;

    let color = json!({"task_ids": [a], "action": "color"});
    assert_eq!(fx.bulk(&board, color).await.status, StatusCode::BAD_REQUEST);
    let members = json!({"task_ids": [a], "action": "members"});
    assert_eq!(
        fx.bulk(&board, members).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn bulk_color_should_change_nothing_when_a_task_is_on_another_board() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let (_other, other_columns) = fx.board("Other", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let foreign = fx.task(&other_columns[0], "f").await;
    let colors = fx.db.list_colors(&board).expect("colors");
    let red = colors.iter().find(|c| c.value == "red").expect("red");
    let before = fx.db.task(&a).expect("read").expect("task").color_id;

    let body = json!({"task_ids": [a, foreign], "action": "color", "color_id": red.id});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(
        fx.db.task(&a).expect("read").expect("task").color_id,
        before
    );
}

#[tokio::test]
async fn bulk_color_should_change_nothing_for_a_color_of_another_board() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo"]);
    let (other, _) = fx.board("Other", &["Todo"]);
    let a = fx.task(&columns[0], "a").await;
    let foreign = fx.db.list_colors(&other).expect("colors")[1].id.clone();
    let before = fx.db.task(&a).expect("read").expect("task").color_id;

    let body = json!({"task_ids": [a], "action": "color", "color_id": foreign});
    let reply = fx.bulk(&board, body).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(
        fx.db.task(&a).expect("read").expect("task").color_id,
        before
    );
}
