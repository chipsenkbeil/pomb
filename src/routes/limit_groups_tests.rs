use axum::http::StatusCode;
use serde_json::{Value, json};

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::AuthScope;

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    token: String,
    cookie: String,
    board: String,
    columns: [String; 4],
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("limit-groups.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let account = db.create_user("admin").expect("user");
        let board = db.create_board(&account, "Groups").expect("board");
        let columns = ["One", "Two", "Three", "Four"]
            .map(|name| db.create_column(&board, name, None).expect("column"));
        let (record, token) = new_api_token_record(&account, "agent", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let session = db.create_session(&account, "").expect("session");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            token,
            cookie: format!("session={session}"),
            board,
            columns,
        }
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {}", self.token);
        call(
            &self.app,
            method,
            uri,
            &[("authorization", bearer.as_str())],
            body,
        )
        .await
    }

    #[must_use]
    fn groups_uri(&self) -> String {
        format!("/api/boards/{}/limit-groups", self.board)
    }

    async fn group(&self, columns: &[usize], limit: i64) -> Reply {
        let ids: Vec<&String> = columns.iter().map(|at| &self.columns[*at]).collect();
        let body = json!({"column_ids": ids, "limit": limit});
        self.send("POST", &self.groups_uri(), Some(body)).await
    }

    async fn task(&self, column: usize) -> String {
        let body = json!({"column_id": self.columns[column], "name": "Card"});
        let made = self.send("POST", "/api/tasks", Some(body)).await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        made.body["id"].as_str().expect("id").to_string()
    }

    async fn move_to(&self, task: &str, column: usize, forced: bool) -> Reply {
        let uri = format!("/api/tasks/{task}/move?force={forced}");
        let body = json!({"column_id": self.columns[column]});
        self.send("POST", &uri, Some(body)).await
    }

    async fn page(&self, uri: &str) -> String {
        let read = call(
            &self.app,
            "GET",
            uri,
            &[("cookie", self.cookie.as_str())],
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.body);
        read.body.as_str().expect("html").to_string()
    }

    async fn board_detail(&self) -> Value {
        let uri = format!("/api/boards/{}", self.board);
        let read = self.send("GET", &uri, None).await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.body);
        read.body
    }
}

#[tokio::test]
async fn limit_group_should_link_adjacent_columns_and_refuse_a_column_in_two_groups() {
    let fx = Fixture::new();
    let made = fx.group(&[0, 1], 2).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(
        made.body["column_ids"],
        json!([fx.columns[0], fx.columns[1]])
    );
    assert_eq!(made.body["limit"], 2);

    let overlapping = fx.group(&[1, 2], 2).await;
    assert_eq!(overlapping.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        fx.board_detail().await["limit_groups"]
            .as_array()
            .expect("groups")
            .len(),
        1
    );

    let fresh = fx.group(&[2, 3], 1).await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
}

#[tokio::test]
async fn limit_group_should_refuse_columns_that_are_not_adjacent_with_a_400() {
    let fx = Fixture::new();
    let apart = fx.group(&[0, 2], 2).await;
    assert_eq!(apart.status, StatusCode::BAD_REQUEST, "{}", apart.body);

    let made = fx.group(&[0, 1], 2).await;
    let id = made.body["id"].as_str().expect("id");
    let uri = format!("{}/{id}", fx.groups_uri());
    let widened = fx
        .send(
            "PATCH",
            &uri,
            Some(json!({"column_ids": [fx.columns[0], fx.columns[3]]})),
        )
        .await;
    assert_eq!(widened.status, StatusCode::BAD_REQUEST, "{}", widened.body);
}

#[tokio::test]
async fn move_should_be_refused_over_the_group_limit_even_when_the_column_limit_allows_it() {
    let fx = Fixture::new();
    fx.db
        .set_column_wip(&fx.columns[1], Some(5))
        .expect("limit");
    fx.group(&[0, 1], 1).await;
    fx.task(0).await;
    let mover = fx.task(3).await;

    let refused = fx.move_to(&mover, 1, false).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["code"], "wip_limit_reached");
    assert!(
        refused.body["message"]
            .as_str()
            .expect("message")
            .contains("1/1")
    );
}

#[tokio::test]
async fn move_should_be_refused_over_the_column_limit_even_when_the_group_limit_allows_it() {
    let fx = Fixture::new();
    fx.db
        .set_column_wip(&fx.columns[1], Some(1))
        .expect("limit");
    fx.group(&[0, 1], 5).await;
    fx.task(1).await;
    let mover = fx.task(3).await;

    let refused = fx.move_to(&mover, 1, false).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["code"], "wip_limit_reached");
}

#[tokio::test]
async fn move_should_succeed_inside_a_group_and_when_the_limits_allow_it() {
    let fx = Fixture::new();
    fx.group(&[0, 1], 1).await;
    let inside = fx.task(0).await;
    let moved = fx.move_to(&inside, 1, false).await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);

    let outside = fx.task(3).await;
    let moved = fx.move_to(&outside, 2, false).await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
}

#[tokio::test]
async fn move_should_succeed_over_the_group_limit_when_the_move_is_forced() {
    let fx = Fixture::new();
    fx.group(&[0, 1], 1).await;
    fx.task(0).await;
    let mover = fx.task(3).await;

    let moved = fx.move_to(&mover, 1, true).await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
}

#[tokio::test]
async fn board_detail_should_list_each_group_with_its_columns_limit_and_count() {
    let fx = Fixture::new();
    fx.group(&[1, 2], 3).await;
    fx.task(1).await;
    fx.task(2).await;
    fx.task(0).await;

    let detail = fx.board_detail().await;
    let groups = detail["limit_groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0]["column_ids"],
        json!([fx.columns[1], fx.columns[2]])
    );
    assert_eq!(groups[0]["limit"], 3);
    assert_eq!(groups[0]["count"], 2);
    assert!(groups[0]["id"].as_str().is_some());
}

#[tokio::test]
async fn limit_groups_should_report_count_unit() {
    let fx = Fixture::new();
    fx.group(&[0, 1], 2).await;
    let detail = fx.board_detail().await;
    assert_eq!(detail["limit_groups"][0]["limit_unit"], "count");
}

#[tokio::test]
async fn limit_group_should_change_its_limit_and_unlink() {
    let fx = Fixture::new();
    let made = fx.group(&[0, 1], 2).await;
    let uri = format!(
        "{}/{}",
        fx.groups_uri(),
        made.body["id"].as_str().expect("id")
    );

    let changed = fx.send("PATCH", &uri, Some(json!({"limit": 4}))).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    assert_eq!(changed.body["limit"], 4);
    assert_eq!(changed.body["column_ids"], made.body["column_ids"]);

    let gone = fx.send("DELETE", &uri, None).await;
    assert_eq!(gone.status, StatusCode::OK);
    assert!(
        fx.board_detail().await["limit_groups"]
            .as_array()
            .expect("groups")
            .is_empty()
    );
    assert_eq!(
        fx.send("DELETE", &uri, None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn column_move_should_be_refused_when_it_splits_a_group() {
    let fx = Fixture::new();
    fx.group(&[0, 1], 2).await;
    let uri = format!("/api/columns/{}/move", fx.columns[1]);

    let split = fx.send("POST", &uri, Some(json!({"position": 3}))).await;
    assert_eq!(split.status, StatusCode::BAD_REQUEST, "{}", split.body);

    let swapped = fx.send("POST", &uri, Some(json!({"position": 0}))).await;
    assert_eq!(swapped.status, StatusCode::OK, "{}", swapped.body);
}

#[tokio::test]
async fn column_delete_should_take_the_column_out_of_its_group() {
    let fx = Fixture::new();
    fx.group(&[0, 1, 2], 3).await;
    let uri = format!("/api/columns/{}", fx.columns[1]);
    assert_eq!(fx.send("DELETE", &uri, None).await.status, StatusCode::OK);

    let groups = fx.board_detail().await["limit_groups"].clone();
    assert_eq!(
        groups[0]["column_ids"],
        json!([fx.columns[0], fx.columns[2]])
    );

    let uri = format!("/api/columns/{}", fx.columns[0]);
    assert_eq!(fx.send("DELETE", &uri, None).await.status, StatusCode::OK);
    assert!(
        fx.board_detail().await["limit_groups"]
            .as_array()
            .expect("groups")
            .is_empty()
    );
}

#[tokio::test]
async fn column_move_should_report_the_group_columns_in_the_new_board_order() {
    let fx = Fixture::new();
    fx.group(&[0, 1], 2).await;
    let uri = format!("/api/columns/{}/move", fx.columns[0]);
    let moved = fx.send("POST", &uri, Some(json!({"position": 1}))).await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);

    let groups = fx.board_detail().await["limit_groups"].clone();
    assert_eq!(
        groups[0]["column_ids"],
        json!([fx.columns[1], fx.columns[0]])
    );
}

#[tokio::test]
async fn board_page_should_show_a_limit_group_count_against_its_limit_on_its_columns() {
    let fx = Fixture::new();
    let made = fx.group(&[1, 2], 4).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let group = made.body["id"].as_str().expect("id").to_string();
    fx.task(1).await;
    fx.task(2).await;
    fx.task(0).await;

    let page = fx.page(&format!("/b/{}", fx.board)).await;
    assert_eq!(page.matches("columnHeader-groupCount\"").count(), 2);
    assert_eq!(
        page.matches("title=\"Limit group\">2 / 4</span>").count(),
        2
    );
    assert_eq!(
        page.matches(&format!("data-group-id=\"{group}\"")).count(),
        2
    );
    assert_eq!(page.matches("data-group-limit=\"4\"").count(), 2);
}

#[tokio::test]
async fn board_page_should_mark_a_limit_group_over_its_limit() {
    let fx = Fixture::new();
    fx.group(&[1, 2], 2).await;
    for column in [1, 2, 2] {
        let task = fx.task(0).await;
        let moved = fx.move_to(&task, column, true).await;
        assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    }

    let page = fx.page(&format!("/b/{}", fx.board)).await;
    assert_eq!(page.matches("columnHeader-groupCount--over").count(), 2);
    assert!(page.contains("columnHeader-groupCount--over\" title=\"Limit group\">3 / 2</span>"));
}

#[tokio::test]
async fn board_settings_should_list_each_limit_group_with_its_columns_and_limit() {
    let fx = Fixture::new();
    let uri = format!("/b/{}/settings/layout", fx.board);
    assert!(!fx.page(&uri).await.contains("Limit groups"));

    fx.group(&[1, 2], 5).await;
    fx.task(1).await;
    let page = fx.page(&uri).await;
    assert!(page.contains("<h2>Limit groups</h2>"));
    assert!(page.contains("<td>Two, Three</td>"));
    assert!(page.contains("<td>5</td>"));
    assert!(page.contains("<td>1</td>"));
}
