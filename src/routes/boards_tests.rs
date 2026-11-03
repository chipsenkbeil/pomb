use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::db::{BOARD_COPY_TASK_CAP, ColumnPatch, ColumnWatch, Db};
use crate::models::{AccountId, Actor, BoardRole, Responsible, TaskRow};

struct Fixture {
    app: axum::Router,
    db: Db,
    owner: AccountId,
    cookie: String,
    board_id: String,
    todo: String,
    doing: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("boards.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let owner = db.create_user("admin").expect("create user");
    let session = db.create_session(&owner, "").expect("session");
    let board_id = db.create_board(&owner, "Source").expect("board");
    let todo = db.create_column(&board_id, "Todo", None).expect("column");
    let doing = db
        .create_column(&board_id, "Doing", Some(3))
        .expect("column");
    db.create_swimlane(&board_id, "Team").expect("lane");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    let fixture = Fixture {
        app,
        db,
        owner,
        cookie: format!("session={session}"),
        board_id,
        todo,
        doing,
    };
    (dir, fixture)
}

async fn create(fx: &Fixture, body: Value) -> (StatusCode, Value) {
    send(fx, "POST", "/api/boards", body).await
}

async fn send(fx: &Fixture, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("accept", "application/json")
        .header("cookie", &fx.cookie)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
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

async fn copy(fx: &Fixture, extra: Value) -> String {
    let mut body = json!({"name": "Copy", "copy_board_id": fx.board_id});
    body.as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let (status, made) = create(fx, body).await;
    assert_eq!(status, StatusCode::OK, "{made}");
    made["id"].as_str().expect("id").to_string()
}

fn tasks_of(fx: &Fixture, board_id: &str) -> Vec<TaskRow> {
    fx.db
        .board_task_ids(board_id)
        .expect("ids")
        .iter()
        .map(|id| fx.db.task(id).expect("read").expect("task"))
        .collect()
}

fn crew_task(fx: &Fixture, name: &str) -> String {
    let id = fx
        .db
        .create_task(&fx.todo, None, name, 1, None)
        .expect("task");
    fx.db
        .set_task_members(&id, &["ann".to_string(), "bob".to_string()])
        .expect("members");
    id
}

#[tokio::test]
async fn copying_a_board_should_keep_columns_colors_limits_and_swimlanes() {
    let (_dir, fx) = fixture();
    fx.db
        .create_task(&fx.todo, None, "Stays behind", 1, None)
        .expect("task");
    let id = copy(&fx, json!({})).await;

    let columns = fx.db.list_columns(&id).expect("columns");
    let shape: Vec<(&str, Option<i64>)> = columns
        .iter()
        .map(|column| (column.name.as_str(), column.wip_limit))
        .collect();
    assert_eq!(shape, [("Todo", None), ("Doing", Some(3))]);
    let palette = |board: &str| -> Vec<(String, String, bool, bool)> {
        fx.db
            .list_colors(board)
            .expect("colors")
            .into_iter()
            .map(|color| (color.value, color.label, color.enabled, color.is_default))
            .collect()
    };
    assert!(!palette(&fx.board_id).is_empty());
    assert_eq!(palette(&id), palette(&fx.board_id));
    let lanes = fx.db.list_swimlanes(&id).expect("lanes");
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].name, "Team");
    assert_eq!(fx.db.board(&id).expect("board").expect("row").name, "Copy");
    assert!(tasks_of(&fx, &id).is_empty());
}

#[tokio::test]
async fn copying_tasks_should_stop_at_the_cap() {
    let (_dir, fx) = fixture();
    for n in 0..BOARD_COPY_TASK_CAP + 5 {
        fx.db
            .create_task(&fx.todo, None, &format!("Task {n}"), 1, None)
            .expect("task");
    }
    let id = copy(&fx, json!({"copy_tasks": true})).await;

    assert_eq!(tasks_of(&fx, &id).len(), BOARD_COPY_TASK_CAP);
    assert_eq!(tasks_of(&fx, &fx.board_id).len(), BOARD_COPY_TASK_CAP + 5);
}

#[tokio::test]
async fn copying_tasks_should_place_each_in_its_column() {
    let (_dir, fx) = fixture();
    fx.db
        .create_task(&fx.doing, None, "Under way", 1, None)
        .expect("task");
    let id = copy(&fx, json!({"copy_tasks": true})).await;

    let columns = fx.db.list_columns(&id).expect("columns");
    let tasks = tasks_of(&fx, &id);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].column_id, columns[1].id);
}

#[tokio::test]
async fn copying_tasks_should_keep_members_without_clear_members() {
    let (_dir, fx) = fixture();
    crew_task(&fx, "Crewed");
    let id = copy(&fx, json!({"copy_tasks": true})).await;

    let tasks = tasks_of(&fx, &id);
    assert_eq!(tasks[0].member_ids, ["ann", "bob"]);
    assert_eq!(
        tasks[0].responsible,
        Some(Responsible::Member {
            id: "ann".to_string()
        })
    );
}

#[tokio::test]
async fn clear_members_should_remove_members_from_the_copied_tasks() {
    let (_dir, fx) = fixture();
    let source = crew_task(&fx, "Crewed");
    let id = copy(&fx, json!({"copy_tasks": true, "clear_members": true})).await;

    let tasks = tasks_of(&fx, &id);
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].member_ids.is_empty());
    assert_eq!(tasks[0].responsible, None);
    let kept = fx.db.task(&source).expect("read").expect("task");
    assert_eq!(kept.member_ids, ["ann", "bob"]);
}

#[tokio::test]
async fn clear_members_should_remove_members_from_the_copied_subtasks() {
    let (_dir, fx) = fixture();
    let source = crew_task(&fx, "Crewed");
    fx.db.add_subtask(&source, "Step").expect("subtask");
    fx.db
        .apply_task_change(&source, Actor::SYSTEM, |task| {
            task.subtasks[0].member_ids = vec!["ann".to_string()];
            ((), true)
        })
        .expect("assign");
    let kept = copy(&fx, json!({"copy_tasks": true})).await;
    let cleared = copy(&fx, json!({"copy_tasks": true, "clear_members": true})).await;

    assert_eq!(tasks_of(&fx, &kept)[0].subtasks[0].member_ids, ["ann"]);
    assert!(tasks_of(&fx, &cleared)[0].subtasks[0].member_ids.is_empty());
}

#[tokio::test]
async fn saving_a_template_should_not_carry_members() {
    let (_dir, fx) = fixture();
    crew_task(&fx, "Crewed");
    let template = fx
        .db
        .save_board_as_template(&fx.owner, &fx.board_id, "Starter", "")
        .expect("save")
        .expect("board");
    let (status, made) = create(
        &fx,
        json!({"name": "From template", "template_id": template}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{made}");

    let tasks = tasks_of(&fx, made["id"].as_str().expect("id"));
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].member_ids.is_empty());
}

#[tokio::test]
async fn copying_should_refuse_bad_requests() {
    let (_dir, fx) = fixture();
    let held = fx.db.owner_boards(&fx.owner).expect("boards").len();
    let cases = [
        (
            json!({"name": "A", "copy_board_id": fx.board_id, "template_id": "x"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"name": "B", "copy_tasks": true}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"name": "C", "copy_board_id": fx.board_id, "clear_members": true}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"name": "D", "copy_board_id": "missing"}),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (body, expected) in cases {
        let (status, answer) = create(&fx, body.clone()).await;
        assert_eq!(status, expected, "{body}: {answer}");
    }
    assert_eq!(fx.db.owner_boards(&fx.owner).expect("boards").len(), held);
}

async fn set_default(fx: &Fixture, body: Value) -> (StatusCode, Value) {
    let uri = format!("/api/boards/{}/config", fx.board_id);
    send(fx, "PUT", &uri, body).await
}

async fn add_task(fx: &Fixture, extra: Value) -> TaskRow {
    let mut body = json!({"column_id": fx.todo, "name": "Fresh"});
    body.as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let (status, made) = send(fx, "POST", "/api/tasks", body).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().expect("id");
    fx.db.task(id).expect("read").expect("task")
}

fn owner_member(fx: &Fixture) -> Responsible {
    Responsible::Member {
        id: fx.owner.as_str().to_string(),
    }
}

#[tokio::test]
async fn a_task_without_members_should_get_the_creator_as_default_member() {
    let (_dir, fx) = fixture();
    let (status, config) = set_default(&fx, json!({"default_member": "creator"})).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["default_member"], "creator");

    let task = add_task(&fx, json!({})).await;
    assert_eq!(task.member_ids, [fx.owner.as_str()]);
    assert_eq!(task.responsible, Some(owner_member(&fx)));
}

#[tokio::test]
async fn a_task_without_members_should_get_the_chosen_default_member() {
    let (_dir, fx) = fixture();
    let chosen = fx.owner.as_str();
    set_default(&fx, json!({"default_member": chosen})).await;

    let task = add_task(&fx, json!({})).await;
    assert_eq!(task.member_ids, [chosen]);
}

#[tokio::test]
async fn set_as_responsible_off_should_leave_the_default_member_not_responsible() {
    let (_dir, fx) = fixture();
    let (status, config) = set_default(
        &fx,
        json!({"default_member": "creator", "default_member_responsible": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["default_member_responsible"], false);

    let task = add_task(&fx, json!({})).await;
    assert_eq!(task.member_ids, [fx.owner.as_str()]);
    assert_eq!(task.responsible, None);
}

#[tokio::test]
async fn members_given_on_create_should_replace_the_default() {
    let (_dir, fx) = fixture();
    set_default(&fx, json!({"default_member": "creator"})).await;

    let task = add_task(&fx, json!({"member_ids": []})).await;
    assert!(task.member_ids.is_empty());
}

#[tokio::test]
async fn no_default_member_should_leave_new_tasks_unassigned() {
    let (_dir, fx) = fixture();
    let (_, config) = set_default(&fx, json!({})).await;
    assert_eq!(config["default_member"], "none");
    assert_eq!(config["default_member_responsible"], true);

    assert!(add_task(&fx, json!({})).await.member_ids.is_empty());
}

#[tokio::test]
async fn an_unknown_default_member_should_be_refused() {
    let (_dir, fx) = fixture();
    let (status, _) = set_default(&fx, json!({"default_member": "nobody"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn watch(fx: &Fixture, column: &str, watched: bool) -> StatusCode {
    let uri = format!("/api/columns/{column}");
    send(fx, "PATCH", &uri, json!({"watched": watched})).await.0
}

async fn watched_flags(fx: &Fixture) -> Vec<(String, bool)> {
    let uri = format!("/api/boards/{}/columns", fx.board_id);
    let (status, columns) = send(fx, "GET", &uri, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{columns}");
    columns
        .as_array()
        .expect("columns")
        .iter()
        .map(|column| {
            (
                column["name"].as_str().expect("name").to_string(),
                column["watched"].as_bool().expect("watched"),
            )
        })
        .collect()
}

#[tokio::test]
async fn watching_a_column_should_show_in_the_api_until_it_is_unwatched() {
    let (_dir, fx) = fixture();
    assert_eq!(watched_flags(&fx).await[0], ("Todo".to_string(), false));

    assert_eq!(watch(&fx, &fx.todo, true).await, StatusCode::OK);
    assert_eq!(
        watched_flags(&fx).await,
        [("Todo".to_string(), true), ("Doing".to_string(), false)]
    );

    assert_eq!(watch(&fx, &fx.todo, true).await, StatusCode::OK);
    let column = fx.db.list_columns(&fx.board_id).expect("columns").remove(0);
    assert_eq!(column.watchers, [fx.owner.as_str()]);

    assert_eq!(watch(&fx, &fx.todo, false).await, StatusCode::OK);
    assert_eq!(watched_flags(&fx).await[0], ("Todo".to_string(), false));
}

#[tokio::test]
async fn a_watch_should_belong_to_its_account_alone() {
    let (_dir, fx) = fixture();
    fx.db
        .update_column(
            &fx.todo,
            &ColumnPatch {
                watch: Some(ColumnWatch {
                    account: "someone-else",
                    watched: true,
                }),
                ..Default::default()
            },
        )
        .expect("watch");

    assert_eq!(watched_flags(&fx).await[0], ("Todo".to_string(), false));
    assert_eq!(watch(&fx, &fx.todo, true).await, StatusCode::OK);
    assert_eq!(watch(&fx, &fx.todo, false).await, StatusCode::OK);
    let column = fx.db.list_columns(&fx.board_id).expect("columns").remove(0);
    assert_eq!(column.watchers, ["someone-else"]);
}

#[tokio::test]
async fn a_copied_board_should_not_inherit_column_watches() {
    let (_dir, fx) = fixture();
    watch(&fx, &fx.todo, true).await;
    let id = copy(&fx, json!({})).await;

    let columns = fx.db.list_columns(&id).expect("columns");
    assert!(columns.iter().all(|column| column.watchers.is_empty()));
}

#[tokio::test]
async fn a_foreign_member_given_on_create_should_be_refused() {
    let (_dir, fx) = fixture();
    let body = json!({"column_id": fx.todo, "name": "Fresh", "member_ids": ["stranger"]});
    let (status, _) = send(&fx, "POST", "/api/tasks", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(fx.db.board_task_ids(&fx.board_id).expect("ids").is_empty());
}

#[tokio::test]
async fn a_chosen_default_member_should_follow_set_as_responsible() {
    let (_dir, fx) = fixture();
    let chosen = fx.owner.as_str();
    set_default(
        &fx,
        json!({"default_member": chosen, "default_member_responsible": false}),
    )
    .await;
    assert_eq!(add_task(&fx, json!({})).await.responsible, None);

    set_default(&fx, json!({"default_member_responsible": true})).await;
    assert_eq!(
        add_task(&fx, json!({})).await.responsible,
        Some(owner_member(&fx))
    );
}

#[tokio::test]
async fn repeated_members_given_on_create_should_be_stored_once() {
    let (_dir, fx) = fixture();
    let owner = fx.owner.as_str();
    let task = add_task(&fx, json!({"member_ids": [owner, owner]})).await;
    assert_eq!(task.member_ids, [owner]);
}

fn member_fixture(fx: &Fixture, handle: &str) -> Fixture {
    let account = fx.db.create_user(handle).expect("member");
    fx.db
        .set_board_member(&fx.board_id, handle, BoardRole::Member)
        .expect("join");
    let session = fx.db.create_session(&account, "").expect("session");
    Fixture {
        app: fx.app.clone(),
        db: fx.db.clone(),
        owner: account,
        cookie: format!("session={session}"),
        board_id: fx.board_id.clone(),
        todo: fx.todo.clone(),
        doing: fx.doing.clone(),
    }
}

#[tokio::test]
async fn two_members_of_a_shared_board_should_watch_the_same_column_apart() {
    let (_dir, fx) = fixture();
    let bob = member_fixture(&fx, "bob");
    let carol = member_fixture(&fx, "carol");

    assert_eq!(watch(&bob, &fx.todo, true).await, StatusCode::OK);
    assert_eq!(watch(&carol, &fx.todo, true).await, StatusCode::OK);
    assert_eq!(watched_flags(&bob).await[0], ("Todo".to_string(), true));
    assert_eq!(watched_flags(&carol).await[0], ("Todo".to_string(), true));
    assert_eq!(watched_flags(&fx).await[0], ("Todo".to_string(), false));

    assert_eq!(watch(&bob, &fx.todo, false).await, StatusCode::OK);
    assert_eq!(watched_flags(&bob).await[0], ("Todo".to_string(), false));
    assert_eq!(watched_flags(&carol).await[0], ("Todo".to_string(), true));
    let column = fx.db.list_columns(&fx.board_id).expect("columns").remove(0);
    assert_eq!(column.watchers, [carol.owner.as_str()]);
}
