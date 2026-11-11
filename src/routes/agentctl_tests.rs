use std::collections::BTreeSet;
use std::io::Read;

use axum::http::StatusCode;
use base64::Engine as _;
use serde_json::{Value, json};
use tower::ServiceExt;
use utoipa::OpenApi;

use super::server::passkey_mode;
use super::test_support::{make_epic, make_plain};
use super::{ApiDoc, DeviceApproveForm, device_grant, router};
use crate::auth::new_api_token_record;
use crate::db::{
    BootstrapToken, ChildTokenRefusal, FencedWrite, MAX_API_TOKENS_PER_ACCOUNT, NewComment,
};
use crate::models::{
    AccountId, ActiveTimer, Actor, ActorKind, AgentRoleRow, AuthScope, BoardAccess, DeviceAuthRow,
    DeviceAuthStatus, FieldDefRow, FieldKind, FieldOwner, HistoryKind, SECONDS_PER_DAY,
    SECONDS_PER_HOUR, TimeEntryRow, TimerMode,
};
use crate::permissions::{Permission, STANDARD_ROLES};
use crate::{AppState, BODY_LIMIT_BYTES, markdown};

struct Fixture {
    app: axum::Router,
    db: crate::db::Db,
    user_id: AccountId,
    cookie: String,
    read_token: String,
    board_id: String,
    col_todo: String,
    col_done: String,
    lane_a: String,
    lane_b: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("agentctl.redb");
    let db = crate::db::Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    for role in db.list_agent_roles(user_id.as_str()).expect("roles") {
        db.delete_agent_role(user_id.as_str(), &role.id)
            .expect("delete");
    }
    let session = db.create_session(&user_id, "").expect("session");
    let (record, read_token) = new_api_token_record(&user_id, "reader", AuthScope::Read, None);
    db.create_api_token(&record).expect("token");
    let board_id = db.create_board(&user_id, "Agentctl").expect("board");
    let col_todo = db.create_column(&board_id, "Todo", None).expect("column");
    let col_done = db
        .create_column(&board_id, "Shipped", None)
        .expect("column");
    db.set_column_done(&col_done, true).expect("set done");
    let lane_a = db.create_swimlane(&board_id, "A").expect("lane");
    let lane_b = db.create_swimlane(&board_id, "B").expect("lane");
    let fixture = Fixture {
        app: router(
            AppState::new(db.clone(), None, passkey_mode())
                .with_limits(super::server::unthrottled_limits()),
        ),
        db,
        user_id,
        cookie: format!("session={session}"),
        read_token,
        board_id,
        col_todo,
        col_done,
        lane_a,
        lane_b,
    };
    (dir, fixture)
}

/// Credentials attached to a request.
enum As<'a> {
    Admin,
    Bearer(&'a str),
    Anonymous,
}

/// Sends one request; `json` becomes the body and `accept_json` adds
/// `Accept: application/json`. Returns the status and the raw body.
async fn request(
    fx: &Fixture,
    who: As<'_>,
    method: &str,
    uri: &str,
    json: Option<Value>,
    accept_json: bool,
) -> (StatusCode, String) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    builder = match who {
        As::Admin => builder.header("cookie", &fx.cookie),
        As::Bearer(token) => builder.header("authorization", format!("Bearer {token}")),
        As::Anonymous => builder,
    };
    if accept_json {
        builder = builder.header("accept", "application/json");
    }
    let body = match json {
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

/// Sends an admin JSON request and parses the JSON response body.
async fn call(fx: &Fixture, method: &str, uri: &str, json: Option<Value>) -> (StatusCode, Value) {
    let (status, text) = request(fx, As::Admin, method, uri, json, true).await;
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

/// Creates a task through the API and returns its id.
async fn create(fx: &Fixture, body: Value) -> String {
    let (status, task) = call(fx, "POST", "/api/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    task["id"].as_str().expect("id").to_string()
}

/// Creates an epic in the Todo column through the API and returns its id.
async fn create_epic(fx: &Fixture, name: &str) -> String {
    let body = json!({"column_id": fx.col_todo, "name": name, "kind": "epic"});
    create(fx, body).await
}

/// Like [`call`], but returns a page's `items` instead of the page.
async fn call_list(
    fx: &Fixture,
    method: &str,
    uri: &str,
    json: Option<Value>,
) -> (StatusCode, Value) {
    let (status, page) = call(fx, method, uri, json).await;
    match page.get("items") {
        Some(items) => (status, items.clone()),
        None => (status, page),
    }
}

fn ids(list: &Value) -> Vec<&str> {
    list.as_array()
        .expect("array")
        .iter()
        .map(|task| task["id"].as_str().expect("id"))
        .collect()
}

#[tokio::test]
async fn get_task_should_return_name_description_placement_and_timestamps() {
    let (_dir, fx) = fixture();
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_b, "name": "Write"}),
    )
    .await;
    call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"description": "Draft the notes"})),
    )
    .await;

    let (status, task) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["name"], "Write");
    assert_eq!(task["description"], "Draft the notes");
    assert_eq!(task["column_id"], fx.col_todo);
    assert_eq!(task["swimlane_id"], fx.lane_b);
    let created = task["created_at"].as_str().expect("created_at");
    let updated = task["updated_at"].as_str().expect("updated_at");
    chrono::DateTime::parse_from_rfc3339(created).expect("rfc3339 created_at");
    chrono::DateTime::parse_from_rfc3339(updated).expect("rfc3339 updated_at");
    assert!(
        updated > created,
        "the patch moved updated_at past created_at"
    );
    assert_eq!(task["subtasks"], json!([]), "existing fields stay");

    let (status, _) = call(&fx, "GET", "/api/tasks/missing", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_task_should_accept_description_and_labels_and_return_detail() {
    let (_dir, fx) = fixture();
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({
            "column_id": fx.col_todo,
            "name": "Ship",
            "description": "Cut the release",
            "labels": ["agent", " agent", "infra", ""],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(task["name"], "Ship");
    assert_eq!(task["description"], "Cut the release");
    assert_eq!(task["labels"], json!(["agent", "infra"]));
    assert_eq!(task["column_id"], fx.col_todo);
    assert_eq!(task["swimlane_id"], fx.lane_a, "defaults to the first lane");
    assert!(task["updated_at"].is_string());

    let (status, html) = request(
        &fx,
        As::Admin,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Card"})),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("task-card"),
        "the UI still gets the card fragment"
    );
}

#[tokio::test]
async fn patch_task_should_return_json_detail_only_when_asked() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Old"})).await;
    let uri = format!("/api/tasks/{id}");

    let (status, task) = call(&fx, "PATCH", &uri, Some(json!({"name": "New"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["id"], id.as_str());
    assert_eq!(task["name"], "New");

    let (status, html) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"name": "Html"})),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("task-card"),
        "the UI still gets the card fragment"
    );

    let (status, _) = call(
        &fx,
        "PATCH",
        "/api/tasks/missing",
        Some(json!({"name": "X"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn board_task_list_should_return_summaries_in_board_order() {
    let (_dir, fx) = fixture();
    let first = create(&fx, json!({"column_id": fx.col_todo, "name": "First"})).await;
    let second = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Second", "labels": ["x"]}),
    )
    .await;
    let done = create(&fx, json!({"column_id": fx.col_done, "name": "Done"})).await;

    let (status, list) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids(&list),
        vec![first.as_str(), second.as_str(), done.as_str()]
    );
    let summary = &list[1];
    assert_eq!(summary["name"], "Second");
    assert_eq!(summary["column_id"], fx.col_todo);
    assert_eq!(summary["swimlane_id"], fx.lane_a);
    assert_eq!(summary["position"], 1.0);
    assert_eq!(summary["labels"], json!(["x"]));
    assert_eq!(summary["member_ids"], json!([]));
    assert!(summary["updated_at"].is_string());
    assert!(summary.get("description").is_none(), "summaries stay small");
}

#[tokio::test]
async fn board_task_list_should_filter_by_column_swimlane_labels_and_since() {
    let (_dir, fx) = fixture();
    let plain = create(&fx, json!({"column_id": fx.col_todo, "name": "Plain"})).await;
    let both = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Both", "labels": ["agent", "infra"]}),
    )
    .await;
    let lane_b = create(
        &fx,
        json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_b, "name": "B",
               "labels": ["agent"]}),
    )
    .await;
    let done = create(&fx, json!({"column_id": fx.col_done, "name": "Done"})).await;
    let base = format!("/api/boards/{}/tasks", fx.board_id);
    let list = |query: String| {
        let uri = format!("{base}?{query}");
        let fx = &fx;
        async move {
            let (status, list) = call_list(fx, "GET", &uri, None).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {list}");
            ids(&list)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(
        list(format!("column_id={}", fx.col_done)).await,
        vec![done.clone()]
    );
    assert_eq!(
        list(format!("swimlane_id={}", fx.lane_b)).await,
        vec![lane_b.clone()]
    );
    assert_eq!(
        list("label=agent".into()).await,
        vec![both.clone(), lane_b.clone()]
    );
    assert_eq!(
        list("label=agent&label=infra".into()).await,
        vec![both.clone()]
    );
    assert_eq!(
        list(format!(
            "column_id={}&label=agent&swimlane_id={}",
            fx.col_todo, fx.lane_a
        ))
        .await,
        vec![both.clone()]
    );

    let (_, task) = call(&fx, "GET", &format!("/api/tasks/{done}"), None).await;
    let since = task["updated_at"].as_str().expect("updated_at").to_string();
    std::thread::sleep(std::time::Duration::from_millis(5));
    call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{plain}"),
        Some(json!({"name": "Touched"})),
    )
    .await;
    let encoded = since.replace('+', "%2B");
    assert_eq!(list(format!("since={encoded}")).await, vec![plain.clone()]);
    assert!(list("since=2999-01-01T00:00:00Z".into()).await.is_empty());
}

#[tokio::test]
async fn board_task_list_should_reject_unknown_board_bad_since_and_no_credentials() {
    let (_dir, fx) = fixture();
    let (status, body) = request(&fx, As::Admin, "GET", "/api/boards/nope/tasks", None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "Board not found");

    let uri = format!("/api/boards/{}/tasks", fx.board_id);
    let (status, _) = request(&fx, As::Anonymous, "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("{uri}?since=yesterday"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "invalid since \"yesterday\": use RFC3339");
}

#[tokio::test]
async fn read_token_should_list_tasks_but_not_create_them() {
    let (_dir, fx) = fixture();
    let uri = format!("/api/boards/{}/tasks", fx.board_id);
    let (status, _) = request(&fx, As::Bearer(&fx.read_token), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK);

    let body = json!({"column_id": fx.col_todo, "name": "Nope"});
    let (status, _) = request(
        &fx,
        As::Bearer(&fx.read_token),
        "POST",
        "/api/tasks",
        Some(body),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// The task list path that spans boards.
const TASKS_PATH: &str = "/api/tasks";

/// The board task list path, with its `{id}` placeholder.
const BOARD_TASKS_PATH: &str = "/api/boards/{id}/tasks";

/// Both paged task lists.
const PAGED_LISTS: [&str; 2] = [TASKS_PATH, BOARD_TASKS_PATH];

/// Creates `count` tasks in the Todo column and returns their ids in creation order.
async fn create_many(fx: &Fixture, count: usize) -> Vec<String> {
    let mut created = Vec::new();
    for n in 0..count {
        let body = json!({"column_id": fx.col_todo, "name": format!("Task {n}")});
        created.push(create(fx, body).await);
    }
    created
}

/// The URI of a paged list, narrowed to the fixture board.
fn list_uri(fx: &Fixture, endpoint: &str) -> String {
    match endpoint {
        TASKS_PATH => format!("{endpoint}?board_id={}", fx.board_id),
        _ => endpoint.replace("{id}", &fx.board_id),
    }
}

/// Appends a query pair to `uri`, after `?` or `&` as the URI needs.
fn with_query(uri: &str, query: &str) -> String {
    let sep = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{sep}{query}")
}

/// Reads one page and returns its task ids and its `next_cursor`.
async fn page(fx: &Fixture, uri: &str, limit: usize, cursor: Option<&str>) -> (Vec<String>, Value) {
    let mut uri = with_query(uri, &format!("limit={limit}"));
    if let Some(cursor) = cursor {
        uri = with_query(&uri, &format!("cursor={cursor}"));
    }
    let (status, body) = call(fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    let items = ids(&body["items"])
        .into_iter()
        .map(str::to_string)
        .collect();
    (items, body["next_cursor"].clone())
}

/// Follows `next_cursor` from `cursor` to the last page. Returns every id seen and each page's
/// size.
async fn walk_from(
    fx: &Fixture,
    uri: &str,
    limit: usize,
    mut cursor: Option<String>,
) -> (Vec<String>, Vec<usize>) {
    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    loop {
        let (items, next) = page(fx, uri, limit, cursor.as_deref()).await;
        sizes.push(items.len());
        seen.extend(items);
        match next {
            Value::String(next) => cursor = Some(next),
            Value::Null => return (seen, sizes),
            other => panic!("{uri}: unexpected next_cursor {other}"),
        }
    }
}

/// Follows `next_cursor` from the first page to the last.
async fn walk(fx: &Fixture, uri: &str, limit: usize) -> (Vec<String>, Vec<usize>) {
    walk_from(fx, uri, limit, None).await
}

/// Asserts that a read-token GET of `uri` is a 400 `invalid_request`.
async fn assert_invalid_request(fx: &Fixture, uri: &str) {
    let (status, body) = request(fx, As::Bearer(&fx.read_token), "GET", uri, None, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    let body: Value = serde_json::from_str(&body).expect("json error");
    assert_eq!(body["code"], "invalid_request", "{uri}");
}

#[tokio::test]
async fn task_lists_should_page_450_tasks_in_three_pages_in_the_unpaged_order() {
    let (_dir, fx) = fixture();
    let created = create_many(&fx, 450).await;
    for endpoint in PAGED_LISTS {
        let uri = list_uri(&fx, endpoint);
        let (seen, sizes) = walk(&fx, &uri, 200).await;
        assert_eq!(sizes, vec![200, 200, 50], "{endpoint}");
        let (all, next) = page(&fx, &uri, 1000, None).await;
        assert_eq!(next, Value::Null, "{endpoint}");
        assert_eq!(
            seen, all,
            "{endpoint}: pages concatenate to the unpaged order"
        );
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 450, "{endpoint}: no repeats");
        let mut expected = created.clone();
        expected.sort();
        assert_eq!(unique, expected, "{endpoint}: no gaps");
        if endpoint == BOARD_TASKS_PATH {
            assert_eq!(
                seen, created,
                "{endpoint}: board order is creation order here"
            );
        }
    }
}

#[tokio::test]
async fn task_lists_should_default_to_200_items_and_end_with_a_null_cursor() {
    let (_dir, fx) = fixture();
    create_many(&fx, 201).await;
    for endpoint in PAGED_LISTS {
        let uri = list_uri(&fx, endpoint);
        let (status, first) = call(&fx, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["items"].as_array().expect("items").len(), 200);
        let cursor = first["next_cursor"].as_str().expect("a second page exists");
        let (status, last) = call(
            &fx,
            "GET",
            &with_query(&uri, &format!("cursor={cursor}")),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(last["items"].as_array().expect("items").len(), 1);
        assert_eq!(last["next_cursor"], Value::Null);
    }
}

#[tokio::test]
async fn deletes_between_pages_should_not_repeat_tasks() {
    for endpoint in PAGED_LISTS {
        let (_dir, fx) = fixture();
        let created = create_many(&fx, 450).await;
        let uri = list_uri(&fx, endpoint);
        let (first, next) = page(&fx, &uri, 200, None).await;
        let cursor = next.as_str().expect("cursor").to_string();
        for id in [&first[3], &first[150], &created[449]] {
            let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
            assert_eq!(status, StatusCode::OK, "{id}");
        }
        let (rest, _) = walk_from(&fx, &uri, 200, Some(cursor)).await;
        let mut walked = first;
        walked.extend(rest);
        let mut unique = walked.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), walked.len(), "{endpoint}: no task repeats");
        assert!(
            !walked.contains(&created[449]),
            "{endpoint}: a deleted task is gone"
        );
    }
}

#[tokio::test]
async fn moves_and_creates_between_pages_should_still_end_the_walk_with_known_tasks() {
    let (_dir, fx) = fixture();
    let created = create_many(&fx, 450).await;
    let uri = list_uri(&fx, BOARD_TASKS_PATH);
    let (first, next) = page(&fx, &uri, 200, None).await;
    let cursor = next.as_str().expect("cursor").to_string();
    let body = json!({"column_id": fx.col_done, "position": 0});
    let (status, text) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{}/move?force=true", first[10]),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let fresh = create_many(&fx, 5).await;
    let (rest, sizes) = walk_from(&fx, &uri, 200, Some(cursor)).await;
    assert!(sizes.iter().all(|size| *size <= 200));
    for id in first.iter().chain(&rest) {
        assert!(
            created.contains(id) || fresh.contains(id),
            "{id} is unknown"
        );
    }
}

#[tokio::test]
async fn task_lists_should_reject_a_bad_limit_and_a_bad_cursor() {
    let (_dir, fx) = fixture();
    create_many(&fx, 3).await;
    for endpoint in PAGED_LISTS {
        let uri = list_uri(&fx, endpoint);
        for limit in [
            "0",
            "1001",
            "-1",
            "abc",
            "1.5",
            "",
            "+5",
            "5&limit=5",
            "5&limit=6",
        ] {
            assert_invalid_request(&fx, &with_query(&uri, &format!("limit={limit}"))).await;
        }
        let (status, _) = call(&fx, "GET", &with_query(&uri, "limit=1000"), None).await;
        assert_eq!(status, StatusCode::OK, "{endpoint}: 1000 is the maximum");

        let (_, next) = page(&fx, &uri, 1, None).await;
        let good = next.as_str().expect("cursor").to_string();
        let mut flipped = good.clone();
        flipped.pop();
        flipped.push(if good.ends_with('A') { 'B' } else { 'A' });
        for cursor in ["garbage", "!!!", &good[..good.len() - 4], &flipped] {
            assert_invalid_request(&fx, &with_query(&uri, &format!("cursor={cursor}"))).await;
        }
    }
}

#[tokio::test]
async fn board_task_paging_should_carry_filters_and_reject_a_cursor_from_other_filters() {
    let (_dir, fx) = fixture();
    let mut done = Vec::new();
    for n in 0..5 {
        create(
            &fx,
            json!({"column_id": fx.col_todo, "name": format!("T{n}")}),
        )
        .await;
        let body = json!({"column_id": fx.col_done, "name": format!("D{n}")});
        done.push(create(&fx, body).await);
    }
    let board = list_uri(&fx, BOARD_TASKS_PATH);
    let uri = with_query(&board, &format!("column_id={}", fx.col_done));
    let (seen, _) = walk(&fx, &uri, 2).await;
    assert_eq!(seen, done, "the filter holds on every page");

    let (_, next) = page(&fx, &uri, 2, None).await;
    let cursor = next.as_str().expect("cursor");
    let other = with_query(&board, &format!("column_id={}", fx.col_todo));
    assert_invalid_request(&fx, &with_query(&other, &format!("cursor={cursor}"))).await;
    assert_invalid_request(&fx, &with_query(&board, &format!("cursor={cursor}"))).await;
}

#[tokio::test]
async fn task_list_should_keep_its_board_filter_across_pages() {
    let (_dir, fx) = fixture();
    create_many(&fx, 5).await;
    let (status, other) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Other"}))).await;
    assert_eq!(status, StatusCode::OK, "{other}");
    let uri = list_uri(&fx, TASKS_PATH);
    let (first, next) = page(&fx, &uri, 3, None).await;
    assert_eq!(first.len(), 3);
    let cursor = next.as_str().expect("cursor");
    assert_invalid_request(&fx, &with_query(TASKS_PATH, &format!("cursor={cursor}"))).await;
}

#[test]
fn openapi_should_document_the_page_shape_on_both_task_lists() {
    let json = ApiDoc::openapi().to_json().expect("spec serializes");
    let spec: Value = serde_json::from_str(&json).expect("valid json");
    for (path, schema) in [
        (TASKS_PATH, "TaskNamePage"),
        (BOARD_TASKS_PATH, "TaskSummaryPage"),
    ] {
        let get = &spec["paths"][path]["get"];
        let body = get["responses"]["200"]["content"]["application/json"]["schema"]["$ref"]
            .as_str()
            .expect("page schema");
        assert!(body.ends_with(schema), "{path}: {body}");
        let names: Vec<&str> = get["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .filter_map(|param| param["name"].as_str())
            .collect();
        assert!(
            names.contains(&"limit") && names.contains(&"cursor"),
            "{path}"
        );
        assert!(get["responses"]["400"].is_object(), "{path}");
        let page = &spec["components"]["schemas"][schema]["properties"];
        assert!(
            page["items"].is_object() && page["next_cursor"].is_object(),
            "{schema}"
        );
    }
}

#[test]
fn openapi_should_keep_optional_params_and_the_json_task_responses() {
    let json = super::ApiDoc::openapi().to_json().expect("spec serializes");
    let spec: Value = serde_json::from_str(&json).expect("valid json");
    let list = &spec["paths"]["/api/boards/{id}/tasks"]["get"]["parameters"];
    for param in list.as_array().expect("parameters") {
        if param["in"] == "query" {
            assert_ne!(
                param["required"], true,
                "{} must be optional",
                param["name"]
            );
        }
    }
    let patch = &spec["paths"]["/api/tasks/{id}"]["patch"];
    let ok = &patch["responses"]["200"]["content"];
    assert!(ok.get("application/json").is_some(), "PATCH 200 JSON: {ok}");
    assert!(ok.get("text/html").is_none(), "PATCH 200 has no HTML: {ok}");
    let names: Vec<&str> = patch["parameters"]
        .as_array()
        .expect("parameters")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, vec!["id", "If-Claim-Fence", "If-Owner-Fence"]);
    let create = &spec["paths"]["/api/tasks"]["post"]["responses"];
    assert!(create["201"]["content"].get("application/json").is_some());
    assert!(
        create.get("200").is_none(),
        "POST /api/tasks has no HTML 200"
    );
    let row = spec["components"]["schemas"].to_string();
    assert!(
        !row.contains("[`TaskRow::"),
        "no rustdoc link syntax in schemas"
    );
}

#[test]
fn openapi_should_list_agentctl_paths_and_schemas() {
    let json = super::ApiDoc::openapi().to_json().expect("spec serializes");
    let spec: Value = serde_json::from_str(&json).expect("valid json");
    for path in [
        "/api/boards/{id}/tasks",
        "/api/boards/{id}/epics",
        "/api/tasks/{id}/links",
        "/api/tasks/{id}/links/{link_id}",
        "/api/tasks/{id}/close",
        "/api/tasks/{id}/reopen",
        "/api/tasks/{id}/claim",
        "/api/tasks/{id}/claim/heartbeat",
        "/api/tasks/{id}/claim/release",
        "/api/tasks/{id}/owner",
        "/api/tasks/{id}/owner/renew",
        "/api/tasks/{id}/owner/release",
        "/api/boards/{id}/fields",
        "/api/fields/{id}",
        "/api/v1/auth/device",
        "/api/v1/auth/device/token",
    ] {
        assert!(spec["paths"].get(path).is_some(), "missing path {path}");
    }
    assert!(spec["paths"]["/api/tasks/{id}/links"].get("post").is_some());
    assert!(spec["paths"]["/api/tasks/{id}/links"].get("get").is_some());
    assert!(
        spec["paths"]["/api/tasks/{id}/links/{link_id}"]
            .get("delete")
            .is_some()
    );
    for schema in [
        "TaskDetail",
        "TaskSummary",
        "TaskLink",
        "LinkKind",
        "CreateLinkInput",
        "TaskClaim",
        "ClaimInput",
        "HeartbeatInput",
        "ReleaseInput",
        "FieldDef",
        "FieldKind",
        "FieldOwner",
        "CreateFieldInput",
        "UpdateFieldInput",
    ] {
        assert!(
            spec["components"]["schemas"].get(schema).is_some(),
            "missing schema {schema}"
        );
    }
    let params = spec["paths"]["/api/boards/{id}/tasks"]["get"]["parameters"]
        .as_array()
        .expect("parameters");
    let names: Vec<&str> = params.iter().filter_map(|p| p["name"].as_str()).collect();
    for name in [
        "id",
        "column_id",
        "swimlane_id",
        "label",
        "since",
        "ready",
        "claimed",
    ] {
        assert!(names.contains(&name), "missing parameter {name}");
    }
    let detail = &spec["components"]["schemas"]["TaskDetail"]["properties"];
    for field in [
        "name",
        "description",
        "column_id",
        "swimlane_id",
        "created_at",
        "updated_at",
        "metadata",
        "links",
        "parent",
        "children",
        "children_done",
        "children_total",
        "blocked",
        "claim",
        "fields",
    ] {
        assert!(detail.get(field).is_some(), "TaskDetail lacks {field}");
    }
    let summary = &spec["components"]["schemas"]["TaskSummary"]["properties"];
    assert!(
        summary.get("metadata").is_some(),
        "TaskSummary lacks metadata"
    );
    assert!(
        summary.get("blocked").is_some(),
        "TaskSummary lacks blocked"
    );
    assert!(summary.get("claim").is_some(), "TaskSummary lacks claim");
    let claim_input = &spec["components"]["schemas"]["ClaimInput"];
    assert_eq!(claim_input["required"], json!(["claimant", "ttl_seconds"]));
    let move_params = spec["paths"]["/api/tasks/{id}/move"]["post"]["parameters"].to_string();
    assert!(
        move_params.contains("If-Claim-Fence"),
        "move documents If-Claim-Fence"
    );
    let link = &spec["components"]["schemas"]["TaskLink"]["properties"];
    assert!(link.get("board_id").is_some(), "TaskLink lacks board_id");
    assert_eq!(
        spec["components"]["schemas"]["LinkKind"]["enum"],
        json!(["blocked_by", "blocks", "related", "parent", "child"])
    );
    let input = &spec["components"]["schemas"]["UpdateTaskInput"]["properties"];
    assert!(
        input.get("metadata").is_some(),
        "UpdateTaskInput lacks metadata"
    );
}

#[tokio::test]
async fn patch_metadata_should_merge_top_level_keys_delete_nulls_and_clear() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Meta"})).await;
    let uri = format!("/api/tasks/{id}");
    let (_, fresh) = call(&fx, "GET", &uri, None).await;
    assert_eq!(fresh["metadata"], Value::Null, "unset metadata is null");

    let set = json!({"metadata": {"agentctl": {"run": 1, "state": "queued"}, "owner": "chip"}});
    let (status, task) = call(&fx, "PATCH", &uri, Some(set)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        task["metadata"],
        json!({"agentctl": {"run": 1, "state": "queued"}, "owner": "chip"})
    );

    let replace = json!({"metadata": {"agentctl": {"run": 2}}});
    let (_, task) = call(&fx, "PATCH", &uri, Some(replace)).await;
    assert_eq!(
        task["metadata"],
        json!({"agentctl": {"run": 2}, "owner": "chip"}),
        "only the agentctl key is replaced, wholesale"
    );

    let delete = json!({"metadata": {"owner": null}});
    let (_, task) = call(&fx, "PATCH", &uri, Some(delete)).await;
    assert_eq!(task["metadata"], json!({"agentctl": {"run": 2}}));

    let (_, list) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert_eq!(list[0]["metadata"], json!({"agentctl": {"run": 2}}));

    let (_, task) = call(&fx, "PATCH", &uri, Some(json!({"name": "Renamed"}))).await;
    assert_eq!(
        task["metadata"],
        json!({"agentctl": {"run": 2}}),
        "absent leaves it"
    );

    let (_, task) = call(&fx, "PATCH", &uri, Some(json!({"metadata": null}))).await;
    assert_eq!(task["metadata"], Value::Null);
}

#[tokio::test]
async fn patch_metadata_should_reject_non_objects_without_writing() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Keep"})).await;
    let uri = format!("/api/tasks/{id}");
    for bad in [json!([1, 2]), json!("text"), json!(5), json!(true)] {
        let body = json!({"name": "Changed", "metadata": bad});
        let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(text, "metadata must be a JSON object");
    }
    let (_, task) = call(&fx, "GET", &uri, None).await;
    assert_eq!(task["name"], "Keep", "a rejected patch writes nothing");
}

#[tokio::test]
async fn patch_metadata_should_reject_bodies_and_merged_objects_over_64_kib() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Big"})).await;
    let uri = format!("/api/tasks/{id}");

    let huge = "x".repeat(65 * 1024);
    let body = json!({"metadata": {"blob": huge}});
    let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text, "request body exceeds 64 KiB");

    let half = "y".repeat(40 * 1024);
    let first = json!({"metadata": {"a": half}});
    let (status, _) = request(&fx, As::Admin, "PATCH", &uri, Some(first), true).await;
    assert_eq!(status, StatusCode::OK);
    let second = json!({"name": "Grown", "metadata": {"b": half}});
    let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(second), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text, "metadata exceeds 64 KiB");

    let (_, task) = call(&fx, "GET", &uri, None).await;
    assert_eq!(task["name"], "Big", "a rejected merge writes nothing");
    assert!(task["metadata"].get("b").is_none());
    assert!(task["metadata"].get("a").is_some());
}

#[tokio::test]
async fn patch_metadata_should_count_null_as_metadata_for_the_body_limit() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Keep"})).await;
    let uri = format!("/api/tasks/{id}");
    let padding = "x".repeat(65 * 1024);
    let body = json!({"name": "Changed", "description": padding, "metadata": null});
    let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text, "request body exceeds 64 KiB");
    let (_, task) = call(&fx, "GET", &uri, None).await;
    assert_eq!(task["name"], "Keep", "a rejected patch writes nothing");
}

#[tokio::test]
async fn read_token_should_not_patch_metadata() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Locked"})).await;
    let body = json!({"metadata": {"agentctl": {}}});
    let uri = format!("/api/tasks/{id}");
    let (status, _) = request(
        &fx,
        As::Bearer(&fx.read_token),
        "PATCH",
        &uri,
        Some(body),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn task_modal_should_show_metadata_read_only_when_set() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Modal"})).await;
    let modal = format!("/api/tasks/{id}/modal");
    let (_, html) = request(&fx, As::Admin, "GET", &modal, None, false).await;
    assert!(
        !html.contains(r#"id="modal-metadata""#),
        "no block without metadata"
    );

    let set = json!({"metadata": {"agentctl": {"state": "<running>"}}});
    call(&fx, "PATCH", &format!("/api/tasks/{id}"), Some(set)).await;
    let (status, html) = request(&fx, As::Admin, "GET", &modal, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("id=\"modal-metadata\""));
    assert!(html.contains("&lt;running&gt;"), "metadata is HTML-escaped");
    assert!(!html.contains("<running>"));
}

#[test]
fn stored_rows_without_new_fields_should_still_deserialize() {
    let row = json!({
        "id": "t", "column_id": "c", "swimlane_id": null, "name": "Old",
        "description": "", "size": 1, "position": 0.0,
        "created_at": "2026-09-01T10:00:00Z", "completed_at": null,
        "total_minutes": 0,
    });
    let task: crate::models::TaskRow = serde_json::from_value(row).expect("old row");
    assert_eq!(task.updated_at, None);
    assert_eq!(task.last_updated(), "2026-09-01T10:00:00Z");
    assert_eq!(task.metadata, None);
}

/// Posts a link and returns the status and parsed body.
async fn link(fx: &Fixture, task: &str, kind: &str, target: &str) -> (StatusCode, Value) {
    let body = json!({"kind": kind, "target_id": target});
    call(fx, "POST", &format!("/api/tasks/{task}/links"), Some(body)).await
}

async fn links_of(fx: &Fixture, task: &str) -> Vec<(String, String)> {
    let (status, list) = call(fx, "GET", &format!("/api/tasks/{task}/links"), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list.as_array()
        .expect("array")
        .iter()
        .map(|l| {
            let kind = l["kind"].as_str().expect("kind").to_string();
            (kind, l["target_id"].as_str().expect("target").to_string())
        })
        .collect()
}

async fn blocked(fx: &Fixture, task: &str) -> bool {
    let (_, detail) = call(fx, "GET", &format!("/api/tasks/{task}"), None).await;
    detail["blocked"].as_bool().expect("blocked")
}

#[tokio::test]
async fn blocked_by_link_should_block_until_the_target_reaches_a_done_column() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    assert!(!blocked(&fx, &a).await);

    let (status, created) = link(&fx, &a, "blocked_by", &b).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["kind"], "blocked_by");
    assert_eq!(created["target_id"], b.as_str());
    assert!(created["id"].is_string());
    chrono::DateTime::parse_from_rfc3339(created["created_at"].as_str().expect("created_at"))
        .expect("rfc3339");

    assert_eq!(
        links_of(&fx, &a).await,
        vec![("blocked_by".into(), b.clone())]
    );
    assert_eq!(links_of(&fx, &b).await, vec![("blocks".into(), a.clone())]);
    assert!(blocked(&fx, &a).await);
    assert!(!blocked(&fx, &b).await);
    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{a}"), None).await;
    assert_eq!(detail["links"][0]["id"], created["id"]);

    let move_body = json!({"column_id": fx.col_done, "position": 0});
    let (status, _) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{b}/move"),
        Some(move_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !blocked(&fx, &a).await,
        "the Done flag, not the column name, unblocks"
    );
}

#[tokio::test]
async fn blocks_link_should_be_stored_as_blocked_by_on_the_target() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let (status, created) = link(&fx, &a, "blocks", &b).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["kind"], "blocks");
    assert_eq!(created["target_id"], b.as_str());

    assert_eq!(
        links_of(&fx, &b).await,
        vec![("blocked_by".into(), a.clone())]
    );
    assert_eq!(links_of(&fx, &a).await, vec![("blocks".into(), b.clone())]);
    assert!(blocked(&fx, &b).await);
    assert!(!blocked(&fx, &a).await);

    let (_, again) = link(&fx, &b, "blocked_by", &a).await;
    assert_eq!(
        again["id"], created["id"],
        "the same link is not stored twice"
    );
    assert_eq!(again["kind"], "blocked_by");
}

#[tokio::test]
async fn related_links_should_show_on_both_tasks_and_never_block() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let (status, created) = link(&fx, &a, "related", &b).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(links_of(&fx, &a).await, vec![("related".into(), b.clone())]);
    assert_eq!(links_of(&fx, &b).await, vec![("related".into(), a.clone())]);
    assert!(!blocked(&fx, &a).await && !blocked(&fx, &b).await);
    let (_, again) = link(&fx, &b, "related", &a).await;
    assert_eq!(again["id"], created["id"]);
}

#[tokio::test]
async fn link_should_reject_unknown_kind_self_cycles_and_missing_tasks() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let c = create(&fx, json!({"column_id": fx.col_todo, "name": "C"})).await;
    let post = |task: String, body: Value| {
        let fx = &fx;
        async move {
            let uri = format!("/api/tasks/{task}/links");
            request(fx, As::Admin, "POST", &uri, Some(body), true).await
        }
    };

    let (status, text) = post(a.clone(), json!({"kind": "bogus", "target_id": b})).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "unknown link kind")
    );
    let (status, text) = post(a.clone(), json!({"kind": "blocks", "target_id": a})).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "target is the task itself")
    );

    assert_eq!(link(&fx, &a, "blocked_by", &b).await.0, StatusCode::OK);
    assert_eq!(link(&fx, &b, "blocked_by", &c).await.0, StatusCode::OK);
    for (task, kind, target) in [(&c, "blocked_by", &a), (&a, "blocks", &c)] {
        let (status, text) = post(task.clone(), json!({"kind": kind, "target_id": target})).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, "link would create a cycle"),
            "{kind}"
        );
    }
    assert_eq!(links_of(&fx, &c).await, vec![("blocks".into(), b.clone())]);

    let (status, text) = post(a.clone(), json!({"kind": "related", "target_id": "nope"})).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::NOT_FOUND, "Task or target not found")
    );
    let (status, text) = post("nope".into(), json!({"kind": "related", "target_id": a})).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::NOT_FOUND, "Task or target not found")
    );
}

#[tokio::test]
async fn delete_link_should_work_from_either_side_then_404() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let c = create(&fx, json!({"column_id": fx.col_todo, "name": "C"})).await;
    let (_, first) = link(&fx, &a, "blocked_by", &b).await;
    let (_, second) = link(&fx, &a, "related", &c).await;

    let from_target = format!("/api/tasks/{b}/links/{}", first["id"].as_str().expect("id"));
    let (status, _) = call(&fx, "DELETE", &from_target, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!blocked(&fx, &a).await);
    assert!(links_of(&fx, &b).await.is_empty());
    let (status, _) = call(&fx, "DELETE", &from_target, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let from_holder = format!(
        "/api/tasks/{a}/links/{}",
        second["id"].as_str().expect("id")
    );
    let (status, _) = call(&fx, "DELETE", &from_holder, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(links_of(&fx, &a).await.is_empty() && links_of(&fx, &c).await.is_empty());

    let (status, _) = call(&fx, "GET", "/api/tasks/nope/links", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{a}/links/nope"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Creates a link as `who` and returns its id.
async fn link_as(fx: &Fixture, who: As<'_>, task: &str, kind: &str, target: &str) -> String {
    let body = json!({"kind": kind, "target_id": target});
    let (status, text) = request(
        fx,
        who,
        "POST",
        &format!("/api/tasks/{task}/links"),
        Some(body),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let link: Value = serde_json::from_str(&text).expect("link json");
    link["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn a_work_token_should_delete_a_link_it_created_and_get_403_on_any_other() {
    let (_dir, fx) = fixture();
    let mine = token_with(&fx, "agent", AuthScope::Work);
    let theirs = token_with(&fx, "agent", AuthScope::Work);
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let c = create(&fx, json!({"column_id": fx.col_todo, "name": "C"})).await;

    let own = link_as(&fx, As::Bearer(&mine), &a, "blocked_by", &b).await;
    let other_token = link_as(&fx, As::Bearer(&theirs), &a, "related", &c).await;
    let browser = link_as(&fx, As::Admin, &b, "related", &c).await;

    for (task, link) in [(&a, &other_token), (&b, &browser)] {
        let uri = format!("/api/tasks/{task}/links/{link}");
        let (status, text) = request(&fx, As::Bearer(&mine), "DELETE", &uri, None, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied("links.delete")
        );
    }
    assert_eq!(links_of(&fx, &c).await.len(), 2);

    let from_target = format!("/api/tasks/{b}/links/{own}");
    let (status, text) = request(&fx, As::Bearer(&mine), "DELETE", &from_target, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!blocked(&fx, &a).await);
    let (status, _) = request(&fx, As::Bearer(&mine), "DELETE", &from_target, None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_token_holding_links_delete_should_delete_any_link() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "agent", AuthScope::Work);
    let admin = token_with(&fx, "boss", AuthScope::Admin);
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;

    for who in [As::Bearer(&work), As::Admin, As::Bearer(&admin)] {
        let link = link_as(&fx, who, &a, "related", &b).await;
        let uri = format!("/api/tasks/{a}/links/{link}");
        let (status, text) = request(&fx, As::Bearer(&admin), "DELETE", &uri, None, true).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
    assert!(links_of(&fx, &a).await.is_empty());
}

#[tokio::test]
async fn a_lead_token_should_delete_a_link_made_by_another_token_or_the_browser() {
    let (_dir, fx) = fixture();
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let work = token_with(&fx, "agent", AuthScope::Work);
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;

    for who in [As::Bearer(&work), As::Admin] {
        let link = link_as(&fx, who, &a, "related", &b).await;
        let uri = format!("/api/tasks/{a}/links/{link}");
        let (status, text) = request(&fx, As::Bearer(&lead), "DELETE", &uri, None, true).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
    assert!(links_of(&fx, &a).await.is_empty());
}

/// Creates a subtask as `who` and returns its id.
async fn subtask_as(fx: &Fixture, who: As<'_>, task: &str, name: &str) -> String {
    let (status, text) = request(
        fx,
        who,
        "POST",
        &format!("/api/tasks/{task}/subtasks"),
        Some(json!({"name": name})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let sub: Value = serde_json::from_str(&text).expect("subtask json");
    sub["id"].as_str().expect("id").to_string()
}

async fn subtask_names(fx: &Fixture, task: &str) -> Vec<String> {
    let (_, task) = call(fx, "GET", &format!("/api/tasks/{task}"), None).await;
    task["subtasks"]
        .as_array()
        .expect("subtasks")
        .iter()
        .map(|sub| sub["name"].as_str().expect("name").to_string())
        .collect()
}

#[tokio::test]
async fn a_work_token_should_get_403_deleting_any_subtask_including_its_own() {
    let (_dir, fx) = fixture();
    let mine = token_with(&fx, "agent", AuthScope::Work);
    let theirs = token_with(&fx, "agent", AuthScope::Work);
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;

    let own = subtask_as(&fx, As::Bearer(&mine), &a, "own").await;
    let other_token = subtask_as(&fx, As::Bearer(&theirs), &a, "other token").await;
    let browser = subtask_as(&fx, As::Admin, &a, "browser").await;
    let stored_before = fx
        .db
        .add_subtask(&a, "stored before")
        .expect("subtask")
        .expect("task")
        .id;

    for sub in [&own, &other_token, &browser, &stored_before] {
        let uri = format!("/api/tasks/{a}/subtasks/{sub}");
        let (status, text) = request(&fx, As::Bearer(&mine), "DELETE", &uri, None, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied("subtasks.delete")
        );
    }
    assert_eq!(subtask_names(&fx, &a).await.len(), 4);
}

#[tokio::test]
async fn a_token_holding_subtasks_delete_should_delete_any_subtask() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "agent", AuthScope::Work);
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let admin = token_with(&fx, "boss", AuthScope::Admin);
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;

    for deleter in [&lead, &admin] {
        for who in [
            As::Bearer(&work),
            As::Admin,
            As::Bearer(&lead),
            As::Bearer(&admin),
        ] {
            let sub = subtask_as(&fx, who, &a, "step").await;
            let uri = format!("/api/tasks/{a}/subtasks/{sub}");
            let (status, text) =
                request(&fx, As::Bearer(deleter), "DELETE", &uri, None, true).await;
            assert_eq!(status, StatusCode::OK, "{text}");
        }
    }
    assert!(subtask_names(&fx, &a).await.is_empty());
}
#[tokio::test]
async fn a_work_token_should_still_need_links_delete_to_change_an_umbrella() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "agent", AuthScope::Work);
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let (status, text) = request(
        &fx,
        As::Bearer(&work),
        "PUT",
        &format!("/api/tasks/{a}/umbrella"),
        Some(json!({"target_id": b})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
}

#[tokio::test]
async fn deleting_a_task_should_delete_its_links() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let c = create(&fx, json!({"column_id": fx.col_todo, "name": "C"})).await;
    link(&fx, &a, "blocked_by", &b).await;
    link(&fx, &b, "related", &c).await;

    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{b}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(links_of(&fx, &a).await.is_empty());
    assert!(links_of(&fx, &c).await.is_empty());
    assert!(!blocked(&fx, &a).await);

    let (status, _) = link(&fx, &c, "blocks", &a).await;
    assert_eq!(status, StatusCode::OK, "the index holds no stale entries");
    assert_eq!(links_of(&fx, &c).await, vec![("blocks".into(), a.clone())]);
}

#[tokio::test]
async fn board_task_list_should_report_blocked_and_filter_ready() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    link(&fx, &a, "blocked_by", &b).await;
    let base = format!("/api/boards/{}/tasks", fx.board_id);

    let (_, list) = call_list(&fx, "GET", &base, None).await;
    assert_eq!(list[0]["blocked"], true);
    assert_eq!(list[1]["blocked"], false);
    let (_, ready) = call_list(&fx, "GET", &format!("{base}?ready=true"), None).await;
    assert_eq!(ids(&ready), vec![b.as_str()]);
    let (_, waiting) = call_list(&fx, "GET", &format!("{base}?ready=false"), None).await;
    assert_eq!(ids(&waiting), vec![a.as_str()]);

    let (status, text) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("{base}?ready=yes"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text, "invalid ready \"yes\": use true or false");
}

#[tokio::test]
async fn read_token_should_list_links_but_not_change_them() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let (_, created) = link(&fx, &a, "related", &b).await;
    let token = As::Bearer(&fx.read_token);
    let uri = format!("/api/tasks/{a}/links");
    assert_eq!(
        request(&fx, token, "GET", &uri, None, true).await.0,
        StatusCode::OK
    );
    let body = json!({"kind": "related", "target_id": b});
    let token = As::Bearer(&fx.read_token);
    assert_eq!(
        request(&fx, token, "POST", &uri, Some(body), true).await.0,
        StatusCode::FORBIDDEN
    );
    let one = format!("{uri}/{}", created["id"].as_str().expect("id"));
    let token = As::Bearer(&fx.read_token);
    assert_eq!(
        request(&fx, token, "DELETE", &one, None, true).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn task_card_should_show_a_link_chip_tinted_while_blocked() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "Write docs"})).await;
    let card = |id: String| {
        let fx = &fx;
        async move {
            let uri = format!("/api/tasks/{id}/card");
            request(fx, As::Admin, "GET", &uri, None, false).await.1
        }
    };
    assert!(!card(a.clone()).await.contains("card-ico-links"));

    link(&fx, &a, "blocked_by", &b).await;
    let html = card(a.clone()).await;
    assert!(html.contains("card-ico-links card-links-blocked"));
    assert!(html.contains("data-link-count=\"1\""));
    assert!(html.contains("Blocked by: Write docs"));
    let html = card(b.clone()).await;
    assert!(
        html.contains("class=\"card-ico-links\""),
        "the blocker is not tinted"
    );
    assert!(html.contains("Blocks: A"));
}

#[tokio::test]
async fn deleting_a_board_should_delete_links_into_it() {
    let (_dir, fx) = fixture();
    let (status, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Other"}))).await;
    assert_eq!(status, StatusCode::OK, "{board}");
    let other = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(&fx, "GET", &format!("/api/boards/{other}/columns"), None).await;
    let column = columns[0]["id"].as_str().expect("column id").to_string();
    let remote = create(&fx, json!({"column_id": column, "name": "Remote"})).await;
    let local = create(&fx, json!({"column_id": fx.col_todo, "name": "Local"})).await;
    link(&fx, &local, "blocked_by", &remote).await;
    assert!(blocked(&fx, &local).await);

    let (status, _) = call(&fx, "DELETE", &format!("/api/boards/{other}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(links_of(&fx, &local).await.is_empty());
    assert!(!blocked(&fx, &local).await);
}

/// Posts a claim-family request and returns the status and parsed body.
async fn claim_call(fx: &Fixture, task: &str, path: &str, body: Value) -> (StatusCode, Value) {
    call(
        fx,
        "POST",
        &format!("/api/tasks/{task}/claim{path}"),
        Some(body),
    )
    .await
}

async fn claim(fx: &Fixture, task: &str, body: Value) -> (StatusCode, Value) {
    claim_call(fx, task, "", body).await
}

/// Creates a Todo card named `name`, claims it for `claimant` and returns its id and the claim.
async fn claim_task(fx: &Fixture, name: &str, claimant: &str, ttl_seconds: u64) -> (String, Value) {
    let id = create(fx, json!({"column_id": fx.col_todo, "name": name})).await;
    let body = json!({"claimant": claimant, "ttl_seconds": ttl_seconds});
    let (status, granted) = claim(fx, &id, body).await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    (id, granted)
}

/// History event kinds on a task, oldest first.
fn event_kinds(fx: &Fixture, task: &str) -> Vec<HistoryKind> {
    let row = fx.db.task(task).expect("read").expect("task");
    row.history.into_iter().map(|event| event.kind).collect()
}

fn count(kinds: &[HistoryKind], kind: HistoryKind) -> usize {
    kinds.iter().filter(|k| **k == kind).count()
}

#[tokio::test]
async fn claim_should_grant_a_lease_and_refuse_other_claimants() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let body = json!({
        "claimant": "builder-i", "ttl_seconds": 600, "role": "builder",
        "harness": "claude-code", "model": "opus", "jj_workspace": "ws1",
        "jj_change_id": "abc", "session_id": "s1",
    });
    let (status, granted) = claim(&fx, &id, body).await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    assert_eq!(granted["claimant"], "builder-i");
    assert_eq!(granted["fence"], 1);
    for field in [
        "role",
        "harness",
        "model",
        "jj_workspace",
        "jj_change_id",
        "session_id",
    ] {
        assert!(granted[field].is_string(), "{field}");
    }
    for field in ["started_at", "heartbeat_at", "expires_at"] {
        let at = granted[field].as_str().expect(field);
        chrono::DateTime::parse_from_rfc3339(at).expect("rfc3339");
    }

    let other = json!({"claimant": "builder-j", "ttl_seconds": 60});
    let uri = format!("/api/tasks/{id}/claim");
    let (status, text) = request(&fx, As::Admin, "POST", &uri, Some(other), true).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        text,
        format!(
            "task claimed by builder-i until {}",
            granted["expires_at"].as_str().expect("expires_at")
        )
    );

    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(detail["claim"]["fence"], 1);
    let (_, list) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert_eq!(list[0]["claim"]["claimant"], "builder-i");
    assert_eq!(count(&event_kinds(&fx, &id), HistoryKind::Claimed), 1);
}

#[tokio::test]
async fn claim_should_renew_only_with_the_current_fence_and_bump_it_per_new_claim() {
    let (_dir, fx) = fixture();
    let (id, first) = claim_task(&fx, "Work", "a", 60).await;

    let renew = json!({"claimant": "a", "ttl_seconds": 3600, "fence": 1});
    let (status, renewed) = claim(&fx, &id, renew).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renewed["fence"], 1, "a renewal keeps the fence");
    assert_eq!(renewed["started_at"], first["started_at"]);
    assert!(renewed["expires_at"].as_str() > first["expires_at"].as_str());
    assert_eq!(
        count(&event_kinds(&fx, &id), HistoryKind::Claimed),
        1,
        "renewal is not a claim"
    );

    for stale in [
        json!({"claimant": "a", "ttl_seconds": 60, "fence": 7}),
        json!({"claimant": "a", "ttl_seconds": 60}),
    ] {
        let (status, _) = claim(&fx, &id, stale).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    let (status, _) = claim_call(&fx, &id, "/release", json!({"fence": 1})).await;
    assert_eq!(status, StatusCode::OK);
    let (_, second) = claim(&fx, &id, json!({"claimant": "b", "ttl_seconds": 60})).await;
    assert_eq!(second["fence"], 2);
    claim_call(&fx, &id, "/release", json!({"fence": 2})).await;
    let (_, third) = claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;
    assert_eq!(third["fence"], 3);
}

#[tokio::test]
async fn claim_should_reject_bad_input_and_unknown_tasks() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let uri = format!("/api/tasks/{id}/claim");
    for (body, message) in [
        (json!({"ttl_seconds": 60}), "claimant is required"),
        (
            json!({"claimant": " ", "ttl_seconds": 60}),
            "claimant is required",
        ),
        (json!({"claimant": "a"}), "ttl_seconds is required"),
        (
            json!({"claimant": "a", "ttl_seconds": 0}),
            "ttl_seconds must be between 1 and 86400",
        ),
        (
            json!({"claimant": "a", "ttl_seconds": 86401}),
            "ttl_seconds must be between 1 and 86400",
        ),
    ] {
        let (status, text) = request(&fx, As::Admin, "POST", &uri, Some(body), true).await;
        assert_eq!((status, text.as_str()), (StatusCode::BAD_REQUEST, message));
    }
    let (status, _) = claim(&fx, "nope", json!({"claimant": "a", "ttl_seconds": 60})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 86400})).await;
    assert_eq!(status, StatusCode::OK, "86400 is allowed");

    let body = json!({"claimant": "r", "ttl_seconds": 60});
    let (status, _) = request(
        &fx,
        As::Bearer(&fx.read_token),
        "POST",
        &uri,
        Some(body),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn heartbeat_should_extend_the_live_claim_or_conflict() {
    let (_dir, fx) = fixture();
    let (id, first) = claim_task(&fx, "Work", "a", 60).await;
    std::thread::sleep(std::time::Duration::from_millis(5));

    let (status, beat) = claim_call(
        &fx,
        &id,
        "/heartbeat",
        json!({"fence": 1, "session_id": "s2"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(beat["heartbeat_at"].as_str() > first["heartbeat_at"].as_str());
    assert!(beat["expires_at"].as_str() > first["expires_at"].as_str());
    assert_eq!(beat["session_id"], "s2");
    let lease = |claim: &Value| {
        let at =
            |f: &str| chrono::DateTime::parse_from_rfc3339(claim[f].as_str().expect(f)).expect(f);
        (at("expires_at") - at("heartbeat_at")).num_seconds()
    };
    assert_eq!(lease(&beat), 60, "keeps the previous lease length");
    let (_, longer) = claim_call(
        &fx,
        &id,
        "/heartbeat",
        json!({"fence": 1, "ttl_seconds": 900}),
    )
    .await;
    assert_eq!(lease(&longer), 900);

    let uri = format!("/api/tasks/{id}/claim/heartbeat");
    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"fence": 2})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, "claim fence mismatch or expired")
    );
    let (status, text) = request(&fx, As::Admin, "POST", &uri, Some(json!({})), true).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "fence is required")
    );
    let bad_ttl = json!({"fence": 1, "ttl_seconds": 0});
    let (status, _) = request(&fx, As::Admin, "POST", &uri, Some(bad_ttl), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = claim_call(&fx, "nope", "/heartbeat", json!({"fence": 1})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn release_should_clear_the_claim_and_be_idempotent_for_its_fence() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Work", "a", 60).await;
    let uri = format!("/api/tasks/{id}/claim/release");

    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"fence": 9})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, "claim fence mismatch")
    );
    let (status, _) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"fence": 1})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(detail["claim"], Value::Null);
    let (status, _) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"fence": 1})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "already released by that fence");
    let kinds = event_kinds(&fx, &id);
    assert_eq!(count(&kinds, HistoryKind::ClaimReleased), 1);

    claim(&fx, &id, json!({"claimant": "b", "ttl_seconds": 60})).await;
    let (status, _) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"fence": 1})),
        true,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "fence 1 cannot release fence 2's claim"
    );
    let (status, text) = request(&fx, As::Admin, "POST", &uri, Some(json!({})), true).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "fence is required")
    );
    let (status, _) = claim_call(&fx, "nope", "/release", json!({"fence": 1})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expired_claims_should_read_as_null_and_log_one_expiry_event() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Work", "a", 1).await;
    let base = format!("/api/boards/{}/tasks", fx.board_id);
    let (_, claimed) = call_list(&fx, "GET", &format!("{base}?claimed=true"), None).await;
    assert_eq!(ids(&claimed), vec![id.as_str()]);
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(detail["claim"], Value::Null, "reads expire lazily");
    let (_, unclaimed) = call_list(&fx, "GET", &format!("{base}?claimed=false"), None).await;
    assert_eq!(ids(&unclaimed), vec![id.as_str()]);
    assert_eq!(
        count(&event_kinds(&fx, &id), HistoryKind::ClaimExpired),
        0,
        "reads write nothing"
    );

    let (status, _) = claim_call(&fx, &id, "/heartbeat", json!({"fence": 1})).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an expired claim cannot be extended"
    );
    let (status, _) = claim_call(&fx, &id, "/heartbeat", json!({"fence": 1})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let kinds = event_kinds(&fx, &id);
    assert_eq!(
        count(&kinds, HistoryKind::ClaimExpired),
        1,
        "logged once, by the first call to see it"
    );

    let (status, _) = claim_call(&fx, &id, "/release", json!({"fence": 1})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an expired fence counts as released"
    );
    let (status, next) = claim(&fx, &id, json!({"claimant": "b", "ttl_seconds": 60})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["fence"], 2);
    let kinds = event_kinds(&fx, &id);
    assert_eq!(count(&kinds, HistoryKind::ClaimExpired), 1);
    assert_eq!(count(&kinds, HistoryKind::ClaimReleased), 0);
    assert_eq!(count(&kinds, HistoryKind::Claimed), 2);
}

#[tokio::test]
async fn a_new_claim_should_log_the_expiry_of_the_old_one_first() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Work", "a", 1).await;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (status, next) = claim(&fx, &id, json!({"claimant": "b", "ttl_seconds": 60})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["fence"], 2);
    let kinds = event_kinds(&fx, &id);
    let tail: Vec<HistoryKind> = kinds.iter().rev().take(3).rev().copied().collect();
    assert_eq!(
        tail,
        vec![
            HistoryKind::Claimed,
            HistoryKind::ClaimExpired,
            HistoryKind::Claimed
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claims_should_grant_exactly_one() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    for round in 0..25 {
        let id = create(
            &fx,
            json!({"column_id": fx.col_todo, "name": format!("Race {round}")}),
        )
        .await;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
        let mut handles = Vec::new();
        for racer in 0..16 {
            let (fx, id, barrier) = (fx.clone(), id.clone(), barrier.clone());
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let body = json!({"claimant": format!("racer-{racer}"), "ttl_seconds": 60});
                claim(&fx, &id, body).await.0
            }));
        }
        let mut statuses = Vec::new();
        for handle in handles {
            statuses.push(handle.await.expect("join"));
        }
        let granted = statuses.iter().filter(|s| **s == StatusCode::OK).count();
        let refused = statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count();
        assert_eq!((granted, refused), (1, 15), "round {round}: {statuses:?}");
        let row = fx.db.task(&id).expect("read").expect("task");
        assert_eq!(row.claim_fence, 1, "round {round}: one fence issued");
        assert_eq!(count(&event_kinds(&fx, &id), HistoryKind::Claimed), 1);
    }
}

#[tokio::test]
async fn if_claim_fence_should_guard_move_update_and_comment() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let fenced = |method: &'static str, path: String, body: Value, fence: Option<&'static str>| {
        let fx = &fx;
        async move {
            let mut builder = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("cookie", &fx.cookie)
                .header("accept", "application/json")
                .header("content-type", "application/json");
            if let Some(fence) = fence {
                builder = builder.header("if-claim-fence", fence);
            }
            let response = fx
                .app
                .clone()
                .oneshot(
                    builder
                        .body(axum::body::Body::from(body.to_string()))
                        .expect("req"),
                )
                .await
                .expect("oneshot");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
                .await
                .expect("body");
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    let task = format!("/api/tasks/{id}");
    let mv = format!("/api/tasks/{id}/move");
    let comment = format!("/api/tasks/{id}/comments");
    let patch = json!({"name": "Renamed"});
    let move_body = json!({"column_id": fx.col_todo, "position": 0});
    let note = json!({"body": "progress"});

    let (status, _) = fenced("PATCH", task.clone(), patch.clone(), Some("5")).await;
    assert_eq!(status, StatusCode::OK, "no live claim: any fence passes");

    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;
    for (method, path, body) in [
        ("PATCH", task.clone(), patch.clone()),
        ("POST", mv.clone(), move_body.clone()),
        ("POST", comment.clone(), note.clone()),
    ] {
        let (status, text) = fenced(method, path.clone(), body.clone(), Some("2")).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::CONFLICT, "stale claim fence"),
            "{path}"
        );
        let (status, _) = fenced(method, path.clone(), body.clone(), Some("1")).await;
        assert_eq!(status, StatusCode::OK, "{path} with the live fence");
        let (status, _) = fenced(method, path.clone(), body.clone(), None).await;
        assert_eq!(status, StatusCode::OK, "{path} without the header (humans)");
        let (status, _) = fenced(method, path.clone(), body.clone(), Some("x")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} with a bad header");
    }
}

#[tokio::test]
async fn task_card_should_show_the_live_claimant() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let uri = format!("/api/tasks/{id}/card");
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(!html.contains("card-ico-claim"));
    claim(
        &fx,
        &id,
        json!({"claimant": "builder-i", "ttl_seconds": 600}),
    )
    .await;
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(html.contains("card-circle-owner card-circle-agent"));
    assert!(html.contains("title=\"builder-i\""));
}

/// History events of one kind on a task, as their detail strings.
fn event_details(fx: &Fixture, task: &str, kind: HistoryKind) -> Vec<String> {
    let row = fx.db.task(task).expect("read").expect("task");
    row.history
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.detail)
        .collect()
}

#[tokio::test]
async fn link_add_and_remove_should_log_events_on_the_holding_task() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "Blocker"})).await;

    let (_, created) = link(&fx, &b, "blocks", &a).await;
    assert_eq!(
        event_details(&fx, &a, HistoryKind::LinkAdded),
        vec![format!("Added link blocked_by \"Blocker\" ({b})")],
        "a blocks link is held, and logged, by the blocked task"
    );
    assert!(event_details(&fx, &b, HistoryKind::LinkAdded).is_empty());
    link(&fx, &a, "blocked_by", &b).await;
    assert_eq!(
        event_details(&fx, &a, HistoryKind::LinkAdded).len(),
        1,
        "no event for an existing link"
    );

    let uri = format!(
        "/api/tasks/{b}/links/{}",
        created["id"].as_str().expect("id")
    );
    call(&fx, "DELETE", &uri, None).await;
    assert_eq!(
        event_details(&fx, &a, HistoryKind::LinkRemoved),
        vec![format!("Removed link blocked_by \"Blocker\" ({b})")]
    );
    let row = fx.db.task(&a).expect("read").expect("task");
    let actor = &row.history.last().expect("event").actor;
    assert_eq!(actor, "admin", "the caller is the actor");
}

#[tokio::test]
async fn deleting_a_linked_task_should_log_link_removed_on_the_other_task() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "Gone"})).await;
    link(&fx, &a, "related", &b).await;
    call(&fx, "DELETE", &format!("/api/tasks/{b}"), None).await;
    assert_eq!(
        event_details(&fx, &a, HistoryKind::LinkRemoved),
        vec![format!(
            "Removed link related \"Gone\" ({b}) (task deleted)"
        )]
    );
}

#[tokio::test]
async fn deleting_the_last_metadata_key_should_leave_null_and_log_nothing() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Meta"})).await;
    let uri = format!("/api/tasks/{id}");
    let before = fx.db.task(&id).expect("read").expect("task").history.len();
    call(&fx, "PATCH", &uri, Some(json!({"metadata": {"only": 1}}))).await;
    let (status, task) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"metadata": {"only": null}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["metadata"], Value::Null, "one representation for none");
    let row = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(row.metadata, None);
    assert_eq!(
        row.history.len(),
        before,
        "metadata changes are not history"
    );
}

fn updated_at(fx: &Fixture, task: &str) -> String {
    let row = fx.db.task(task).expect("read").expect("task");
    row.updated_at.expect("updated_at")
}

/// Board task ids updated after `since` that are ready, on `board`.
async fn ready_since(fx: &Fixture, board: &str, since: &str) -> Vec<String> {
    let since = since.replace('+', "%2B");
    let uri = format!("/api/boards/{board}/tasks?since={since}&ready=true");
    let (status, list) = call_list(fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    ids(&list).into_iter().map(str::to_string).collect()
}

fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(5));
}

#[tokio::test]
async fn a_blocker_entering_done_should_bump_only_dependents_whose_readiness_flips() {
    let (_dir, fx) = fixture();
    let blocker = create(&fx, json!({"column_id": fx.col_todo, "name": "Blocker"})).await;
    let other = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Other blocker"}),
    )
    .await;

    // Lane B keeps the dependents out of the blocker's cell, whose
    // position renumbering is a write of its own.
    let in_lane_b =
        |name: &str| json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_b, "name": name});
    let freed = create(&fx, in_lane_b("Freed")).await;
    let still = create(&fx, in_lane_b("Still blocked")).await;
    let bystander = create(&fx, in_lane_b("Bystander")).await;
    link(&fx, &freed, "blocked_by", &blocker).await;
    link(&fx, &still, "blocked_by", &blocker).await;
    link(&fx, &still, "blocked_by", &other).await;
    tick();
    let since = chrono::Utc::now().to_rfc3339();
    let (still_at, bystander_at) = (updated_at(&fx, &still), updated_at(&fx, &bystander));
    tick();

    let reorder = json!({"column_id": fx.col_todo, "position": 0});
    call(
        &fx,
        "POST",
        &format!("/api/tasks/{blocker}/move"),
        Some(reorder),
    )
    .await;
    assert!(
        updated_at(&fx, &freed) < since,
        "a move that keeps readiness bumps no dependent"
    );

    let done = json!({"column_id": fx.col_done, "position": 0});
    let (status, _) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{blocker}/move"),
        Some(done),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        updated_at(&fx, &freed) > since,
        "the freed dependent is bumped"
    );
    assert_eq!(
        updated_at(&fx, &still),
        still_at,
        "another blocker keeps it blocked"
    );
    assert_eq!(updated_at(&fx, &bystander), bystander_at);
    let ready = ready_since(&fx, &fx.board_id, &since).await;
    assert!(ready.contains(&freed), "{ready:?}");
    assert!(!ready.contains(&still));
}

#[tokio::test]
async fn toggling_a_columns_done_flag_should_bump_affected_dependents() {
    let (_dir, fx) = fixture();
    let blocker = create(&fx, json!({"column_id": fx.col_todo, "name": "Blocker"})).await;
    let dependent = create(&fx, json!({"column_id": fx.col_done, "name": "Dependent"})).await;
    let unlinked = create(&fx, json!({"column_id": fx.col_todo, "name": "Unlinked"})).await;
    link(&fx, &dependent, "blocked_by", &blocker).await;
    tick();
    let since = chrono::Utc::now().to_rfc3339();
    let unlinked_at = updated_at(&fx, &unlinked);
    tick();

    let uri = format!("/api/columns/{}", fx.col_todo);
    let (status, _) = call(&fx, "PATCH", &uri, Some(json!({"is_done": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(updated_at(&fx, &dependent) > since);
    assert_eq!(
        updated_at(&fx, &unlinked),
        unlinked_at,
        "tasks in the column are not bumped"
    );
    assert_eq!(
        ready_since(&fx, &fx.board_id, &since).await,
        vec![dependent.clone()]
    );

    tick();
    let again = chrono::Utc::now().to_rfc3339();
    tick();
    call(&fx, "PATCH", &uri, Some(json!({"is_done": true}))).await;
    assert!(updated_at(&fx, &dependent) < again, "no flip, no bump");
    call(&fx, "PATCH", &uri, Some(json!({"is_done": false}))).await;
    assert!(
        updated_at(&fx, &dependent) > again,
        "blocked again, bumped again"
    );
}

#[tokio::test]
async fn deleting_a_blocker_should_bump_its_dependents() {
    let (_dir, fx) = fixture();
    let blocker = create(&fx, json!({"column_id": fx.col_todo, "name": "Blocker"})).await;
    let dependent = create(&fx, json!({"column_id": fx.col_todo, "name": "Dependent"})).await;
    link(&fx, &dependent, "blocked_by", &blocker).await;
    tick();
    let since = chrono::Utc::now().to_rfc3339();
    tick();
    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{blocker}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(updated_at(&fx, &dependent) > since);
    assert_eq!(
        ready_since(&fx, &fx.board_id, &since).await,
        vec![dependent.clone()]
    );
}

#[tokio::test]
async fn a_cross_board_dependent_should_be_bumped_and_listed_on_its_own_board() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Other"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let dependent = create(
        &fx,
        json!({"column_id": other_column, "name": "Remote dependent"}),
    )
    .await;
    let blocker = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Local blocker"}),
    )
    .await;
    link(&fx, &dependent, "blocked_by", &blocker).await;
    tick();
    let since = chrono::Utc::now().to_rfc3339();
    tick();

    let done = json!({"column_id": fx.col_done, "position": 0});
    call(
        &fx,
        "POST",
        &format!("/api/tasks/{blocker}/move"),
        Some(done),
    )
    .await;
    assert!(updated_at(&fx, &dependent) > since);
    assert_eq!(
        ready_since(&fx, &other_board, &since).await,
        vec![dependent.clone()]
    );
}

async fn epic_blocking_a_card_on_another_board(fx: &Fixture) -> (String, String, String) {
    let (_, board) = call(fx, "POST", "/api/boards", Some(json!({"name": "Other"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let dependent = create(
        fx,
        json!({"column_id": other_column, "name": "Remote dependent"}),
    )
    .await;
    let epic = create(
        fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    link(fx, &dependent, "blocked_by", &epic).await;
    (other_board, dependent, epic)
}

#[tokio::test]
async fn closing_an_epic_should_free_a_card_it_blocks_and_reopening_should_block_it_again() {
    let (_dir, fx) = fixture();
    let (other_board, dependent, epic) = epic_blocking_a_card_on_another_board(&fx).await;
    assert!(blocked(&fx, &dependent).await, "an open epic blocks");
    tick();
    let since = chrono::Utc::now().to_rfc3339();
    tick();

    let (status, closed) = call(&fx, "POST", &format!("/api/tasks/{epic}/close"), None).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert!(!blocked(&fx, &dependent).await, "a closed epic is met");
    assert_eq!(
        ready_since(&fx, &other_board, &since).await,
        vec![dependent.clone()]
    );

    tick();
    let reopened_since = chrono::Utc::now().to_rfc3339();
    tick();
    let (status, reopened) = call(&fx, "POST", &format!("/api/tasks/{epic}/reopen"), None).await;
    assert_eq!(status, StatusCode::OK, "{reopened}");
    assert!(blocked(&fx, &dependent).await, "reopening blocks again");
    assert!(updated_at(&fx, &dependent) > reopened_since);
    assert!(
        ready_since(&fx, &other_board, &reopened_since)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn patching_a_closed_epic_to_a_task_should_stamp_the_card_it_blocks() {
    let (_dir, fx) = fixture();
    let epic = create_epic(&fx, "Epic").await;
    let dependent = create(&fx, json!({"column_id": fx.col_todo, "name": "Dependent"})).await;
    link(&fx, &dependent, "blocked_by", &epic).await;
    let (status, closed) = call(&fx, "POST", &format!("/api/tasks/{epic}/close"), None).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert!(!blocked(&fx, &dependent).await, "a closed epic is met");

    tick();
    let since = chrono::Utc::now().to_rfc3339();
    tick();
    let (status, patched) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{epic}"),
        Some(json!({"kind": "task"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert!(blocked(&fx, &dependent).await, "the task blocks again");
    assert!(updated_at(&fx, &dependent) > since);
}

#[tokio::test]
async fn patching_a_closed_epic_title_should_not_stamp_the_card_it_blocks() {
    let (_dir, fx) = fixture();
    let epic = create_epic(&fx, "Epic").await;
    let dependent = create(&fx, json!({"column_id": fx.col_todo, "name": "Dependent"})).await;
    link(&fx, &dependent, "blocked_by", &epic).await;
    let (status, closed) = call(&fx, "POST", &format!("/api/tasks/{epic}/close"), None).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    let stamped = updated_at(&fx, &dependent);

    tick();
    let (status, renamed) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{epic}"),
        Some(json!({"name": "Renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert!(!blocked(&fx, &dependent).await, "a closed epic stays met");
    assert_eq!(updated_at(&fx, &dependent), stamped);
}

/// The full task detail JSON, for before/after comparisons.
async fn detail(fx: &Fixture, task: &str) -> Value {
    let (status, detail) = call(fx, "GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    detail
}

/// Sends a request with an optional `If-Claim-Fence` header.
async fn fenced_request(
    fx: &Fixture,
    method: &str,
    uri: &str,
    body: Value,
    fence: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", &fx.cookie)
        .header("accept", "application/json")
        .header("content-type", "application/json");
    if let Some(fence) = fence {
        builder = builder.header("if-claim-fence", fence);
    }
    let response = fx
        .app
        .clone()
        .oneshot(
            builder
                .body(axum::body::Body::from(body.to_string()))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn rejected_patches_should_leave_the_task_unchanged() {
    let (_dir, fx) = fixture();
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Keep", "labels": ["k"]}),
    )
    .await;
    let uri = format!("/api/tasks/{id}");
    call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"metadata": {"a": "y".repeat(40 * 1024)}})),
    )
    .await;
    let before = detail(&fx, &id).await;
    for body in [
        json!({"metadata": {"z": 1}, "estimate_hours": -5}),
        json!({"name": "Changed", "estimate_hours": -5}),
        json!({"labels": ["new"], "due_at": "garbage"}),
        json!({"name": "Changed", "color_id": "nope"}),
        json!({"description": "d", "grouping_date": "2026-1-5"}),
        json!({"name": "Changed", "metadata": {"b": "y".repeat(40 * 1024)}}),
        json!({"name": "", "description": "d"}),
        json!({"description": "d", "metadata": [1]}),
    ] {
        let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body.clone()), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        assert_eq!(
            detail(&fx, &id).await,
            before,
            "unchanged after a rejected patch"
        );
    }
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;
    let before = detail(&fx, &id).await;
    let (status, _) =
        fenced_request(&fx, "PATCH", &uri, json!({"name": "Changed"}), Some("9")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(detail(&fx, &id).await, before);
}

#[tokio::test]
async fn rejected_creates_should_store_no_task() {
    let (_dir, fx) = fixture();
    let list = format!("/api/boards/{}/tasks", fx.board_id);
    for body in [
        json!({"column_id": fx.col_todo, "name": "X", "estimate_hours": -5}),
        json!({"column_id": fx.col_todo, "name": "X", "description": "d", "due_at": "never"}),
        json!({"column_id": fx.col_todo, "name": "X", "labels": ["a"], "color_id": "nope"}),
    ] {
        let (status, _) = request(&fx, As::Admin, "POST", "/api/tasks", Some(body), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let unknown = json!({"column_id": "nope", "name": "X"});
    let (status, _) = request(&fx, As::Admin, "POST", "/api/tasks", Some(unknown), true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, tasks) = call_list(&fx, "GET", &list, None).await;
    assert_eq!(tasks, json!([]), "no task exists after rejected creates");
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Good", "estimate_hours": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let row = fx
        .db
        .task(task["id"].as_str().expect("id"))
        .expect("read")
        .expect("task");
    assert_eq!(row.estimate_hours, Some(2.0));
    assert_eq!(
        row.history.len(),
        1,
        "one write: the row and its created event"
    );
}

#[tokio::test]
async fn rejected_moves_comments_links_and_claims_should_leave_tasks_unchanged() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let both = || async { (detail(&fx, &a).await, detail(&fx, &b).await) };

    let before = both().await;
    let mv = format!("/api/tasks/{a}/move");
    let (status, _) = call(
        &fx,
        "POST",
        &mv,
        Some(json!({"column_id": "nope", "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let comments = format!("/api/tasks/{a}/comments");
    let (status, _) = call(&fx, "POST", &comments, Some(json!({"body": "  "}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = link(&fx, &a, "bogus", &b).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = link(&fx, &a, "blocked_by", &a).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = claim(&fx, &a, json!({"claimant": "x", "ttl_seconds": 0})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(both().await, before, "nothing changed");

    claim(&fx, &a, json!({"claimant": "x", "ttl_seconds": 60})).await;
    let before = both().await;
    let (status, _) = fenced_request(
        &fx,
        "POST",
        &mv,
        json!({"column_id": fx.col_done, "position": 0}),
        Some("7"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) =
        fenced_request(&fx, "POST", &comments, json!({"body": "late"}), Some("7")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = claim_call(&fx, &a, "/heartbeat", json!({"fence": 7})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = claim_call(&fx, &a, "/release", json!({"fence": 7})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = claim(&fx, &a, json!({"claimant": "y", "ttl_seconds": 60})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(both().await, before, "nothing changed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_rejected_patch_racing_a_valid_one_should_never_land_partially() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Race"})).await;
    let uri = format!("/api/tasks/{id}");
    for round in 0..30 {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for racer in 0..8 {
            let (fx, uri, barrier) = (fx.clone(), uri.clone(), barrier.clone());
            handles.push(tokio::spawn(async move {
                let body = if racer % 2 == 0 {
                    json!({"name": format!("valid {round}.{racer}"), "labels": ["valid"]})
                } else {
                    json!({"name": "bad", "labels": ["bad"], "metadata": {"bad": true},
                           "estimate_hours": -1})
                };
                barrier.wait().await;
                request(&fx, As::Admin, "PATCH", &uri, Some(body), true)
                    .await
                    .0
            }));
        }
        for (racer, handle) in handles.into_iter().enumerate() {
            let expected = if racer % 2 == 0 {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            };
            assert_eq!(handle.await.expect("join"), expected);
        }
        let row = fx.db.task(&id).expect("read").expect("task");
        assert_ne!(row.name, "bad", "round {round}");
        assert_eq!(row.labels, vec!["valid".to_string()], "round {round}");
        assert!(row.metadata.is_none(), "round {round}");
        assert!(
            row.history.iter().all(|e| !e.detail.contains("bad")),
            "round {round}"
        );
    }
}

#[tokio::test]
async fn links_should_report_the_other_tasks_board_and_cards_name_other_boards() {
    let (_dir, fx) = fixture();
    let name = "Ops <b>\"night\"</b>";
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": name}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let local = create(&fx, json!({"column_id": fx.col_todo, "name": "Local"})).await;
    let neighbor = create(&fx, json!({"column_id": fx.col_todo, "name": "Neighbor"})).await;
    let remote = create(&fx, json!({"column_id": other_column, "name": "Remote"})).await;

    let (_, cross) = link(&fx, &local, "blocked_by", &remote).await;
    assert_eq!(
        cross["board_id"],
        other_board.as_str(),
        "POST reports the target's board"
    );
    let (_, same) = link(&fx, &local, "related", &neighbor).await;
    assert_eq!(same["board_id"], fx.board_id.as_str());
    let (_, listed) = call(&fx, "GET", &format!("/api/tasks/{local}/links"), None).await;
    assert_eq!(listed[0]["board_id"], other_board.as_str());
    assert_eq!(listed[1]["board_id"], fx.board_id.as_str());
    let (_, inverse) = call(&fx, "GET", &format!("/api/tasks/{remote}/links"), None).await;
    assert_eq!(inverse[0]["kind"], "blocks");
    assert_eq!(
        inverse[0]["board_id"],
        fx.board_id.as_str(),
        "the blocks view names the blocked task's board"
    );
    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{local}"), None).await;
    assert_eq!(detail["links"][0]["board_id"], other_board.as_str());
    let stored = fx.db.task(&local).expect("read").expect("task");
    assert!(
        stored.links.iter().all(|l| l.board_id.is_none()),
        "board_id is never stored"
    );

    let card = |id: String| {
        let fx = &fx;
        async move {
            request(
                fx,
                As::Admin,
                "GET",
                &format!("/api/tasks/{id}/card"),
                None,
                false,
            )
            .await
            .1
        }
    };
    let html = card(local.clone()).await;
    assert!(
        html.contains("Blocked by: Remote (Ops &lt;b&gt;&quot;night&quot;&lt;/b&gt;)"),
        "escaped other-board name in the tooltip: {html}"
    );
    assert!(html.contains("Related: Neighbor\n") || html.contains("Related: Neighbor\""));
    assert!(
        !html.contains("Neighbor ("),
        "a same-board link names no board"
    );
    let html = card(remote.clone()).await;
    assert!(html.contains("Blocks: Local (Agentctl)"));
}

/// Defines a field on the fixture board; returns the status and body.
async fn define(fx: &Fixture, body: Value) -> (StatusCode, Value) {
    call(
        fx,
        "POST",
        &format!("/api/boards/{}/fields", fx.board_id),
        Some(body),
    )
    .await
}

async fn define_text(fx: &Fixture, body: Value) -> (StatusCode, String) {
    let uri = format!("/api/boards/{}/fields", fx.board_id);
    request(fx, As::Admin, "POST", &uri, Some(body), true).await
}

#[tokio::test]
async fn field_definitions_should_be_created_listed_and_refuse_duplicate_keys() {
    let (_dir, fx) = fixture();
    let (status, checks) = define(
        &fx,
        json!({"key": "agentctl.checks", "label": "Checks", "kind": "multiline", "required": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{checks}");
    assert_eq!(checks["key"], "agentctl.checks");
    assert_eq!(checks["label"], "Checks");
    assert_eq!(checks["kind"], "multiline");
    assert_eq!(checks["options"], json!([]));
    assert_eq!(checks["default"], Value::Null);
    assert_eq!(checks["required"], true);
    assert_eq!(checks["owner"], "user", "owner defaults to user");
    assert_eq!(checks["position"], 0);
    assert!(checks["id"].is_string() && checks["created_at"].is_string());

    let (_, stage) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select",
               "options": ["draft", "review"], "default": "draft", "owner": "system"}),
    )
    .await;
    assert_eq!(stage["position"], 1);
    assert_eq!(stage["owner"], "system");

    let (status, text) = define_text(
        &fx,
        json!({"key": "stage", "label": "Again", "kind": "text"}),
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, "field key exists")
    );

    let (_, list) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/fields", fx.board_id),
        None,
    )
    .await;
    let keys: Vec<&str> = list
        .as_array()
        .expect("array")
        .iter()
        .map(|f| f["key"].as_str().expect("key"))
        .collect();
    assert_eq!(keys, vec!["agentctl.checks", "stage"]);
    let (status, text) =
        request(&fx, As::Admin, "GET", "/api/boards/nope/fields", None, true).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::NOT_FOUND, "Board not found")
    );
    let (status, _) = request(
        &fx,
        As::Admin,
        "POST",
        "/api/boards/nope/fields",
        Some(json!({"key": "a", "label": "A", "kind": "text"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let token = As::Bearer(&fx.read_token);
    let uri = format!("/api/boards/{}/fields", fx.board_id);
    let body = json!({"key": "r", "label": "R", "kind": "text"});
    assert_eq!(
        request(&fx, token, "POST", &uri, Some(body), true).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn field_keys_should_follow_the_dotted_lowercase_rule() {
    let (_dir, fx) = fixture();
    let longest = format!("a{}", "b".repeat(63));
    let too_long = format!("a{}", "b".repeat(64));
    assert_eq!(longest.len(), 64);
    for key in ["a", "agentctl.checks", "a1_b.c2_.d", longest.as_str()] {
        let (status, body) = define(&fx, json!({"key": key, "label": "L", "kind": "text"})).await;
        assert_eq!(status, StatusCode::OK, "{key}: {body}");
    }
    for key in [
        "",
        "A",
        "1a",
        "_a",
        "a..b",
        ".a",
        "a.",
        "a-b",
        "a.B",
        "a.1b",
        "a b",
        "é",
        too_long.as_str(),
    ] {
        let (status, text) =
            define_text(&fx, json!({"key": key, "label": "L", "kind": "text"})).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, "invalid field key"),
            "{key:?}"
        );
    }
    let (status, text) = define_text(&fx, json!({"label": "L", "kind": "text"})).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "invalid field key")
    );
}

#[tokio::test]
async fn field_definitions_should_reject_bad_label_kind_options_default_and_owner() {
    let (_dir, fx) = fixture();
    for (body, message) in [
        (json!({"key": "a", "kind": "text"}), "label is required"),
        (
            json!({"key": "a", "label": " ", "kind": "text"}),
            "label is required",
        ),
        (json!({"key": "a", "label": "A"}), "kind is required"),
        (
            json!({"key": "a", "label": "A", "kind": "date"}),
            "unknown field kind",
        ),
        (
            json!({"key": "a", "label": "A", "kind": "select"}),
            "select fields need options that are non-empty and unique",
        ),
        (
            json!({"key": "a", "label": "A", "kind": "select", "options": ["x", "x"]}),
            "select fields need options that are non-empty and unique",
        ),
        (
            json!({"key": "a", "label": "A", "kind": "text", "options": ["x"]}),
            "options apply only to select and multiselect fields",
        ),
        (
            json!({"key": "a", "label": "A", "kind": "number", "default": "five"}),
            "field \"a\": default: expected a number",
        ),
        (
            json!({"key": "a", "label": "A", "kind": "select", "options": ["x"], "default": "y"}),
            "field \"a\": default: \"y\" is not an option",
        ),
        (
            json!({"key": "a", "label": "A", "kind": "text", "owner": "robot"}),
            "unknown field owner",
        ),
    ] {
        let (status, text) = define_text(&fx, body.clone()).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, message),
            "{body}"
        );
    }
}

#[tokio::test]
async fn field_definitions_should_be_patchable_and_deletable_with_their_values() {
    let (_dir, fx) = fixture();
    let (_, field) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["a", "b"]}),
    )
    .await;
    let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    let (status, patched) = call(
        &fx,
        "PATCH",
        &uri,
        Some(
            json!({"label": "Phase", "options": ["a", "b", "c"], "default": "c",
                    "required": true, "position": 5}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["label"], "Phase");
    assert_eq!(patched["options"], json!(["a", "b", "c"]));
    assert_eq!(patched["default"], "c");
    assert_eq!(patched["required"], true);
    assert_eq!(patched["position"], 5);
    assert_eq!(patched["key"], "stage", "the key is fixed");

    for (body, message) in [
        (json!({"label": ""}), "label is required"),
        (
            json!({"options": ["a"]}),
            "field \"stage\": default: \"c\" is not an option",
        ),
    ] {
        let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
        assert_eq!((status, text.as_str()), (StatusCode::BAD_REQUEST, message));
    }
    let (_, cleared) = call(&fx, "PATCH", &uri, Some(json!({"default": null}))).await;
    assert_eq!(cleared["default"], Value::Null);

    let task = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "T", "fields": {"stage": "b"}}),
    )
    .await;
    let (status, _) = call(&fx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(
        detail["fields"],
        json!({}),
        "deleting the field deletes its values"
    );
    let (status, _) = call(&fx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&fx, "PATCH", &uri, Some(json!({"label": "X"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Defines one field of each kind on the fixture board.
async fn define_every_kind(fx: &Fixture) {
    for body in [
        json!({"key": "note", "label": "Note", "kind": "text"}),
        json!({"key": "agentctl.checks", "label": "Checks", "kind": "multiline", "required": true}),
        json!({"key": "points", "label": "Points", "kind": "number", "default": 3}),
        json!({"key": "urgent", "label": "Urgent", "kind": "bool"}),
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["draft", "review"]}),
        json!({"key": "areas", "label": "Areas", "kind": "multiselect", "options": ["ui", "api", "db"]}),
        json!({"key": "agentctl.run", "label": "Run", "kind": "text", "owner": "system"}),
    ] {
        let (status, body) = define(fx, body).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

#[tokio::test]
async fn task_fields_should_merge_per_key_and_validate_against_the_board() {
    let (_dir, fx) = fixture();
    define_every_kind(&fx).await;
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Typed",
                    "fields": {"note": "hi", "areas": ["api", "ui", "api"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_eq!(
        task["fields"],
        json!({"note": "hi", "areas": ["api", "ui"], "points": 3}),
        "defaults fill absent keys; multiselect choices are deduplicated"
    );
    let id = task["id"].as_str().expect("id").to_string();
    let uri = format!("/api/tasks/{id}");

    let (status, task) = call(
        &fx,
        "PATCH",
        &uri,
        Some(
            json!({"fields": {"agentctl.checks": "cargo test\ncargo clippy",
                               "urgent": true, "stage": "review", "note": null,
                               "agentctl.run": "r-7"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        task["fields"],
        json!({"areas": ["api", "ui"], "points": 3, "agentctl.checks": "cargo test\ncargo clippy",
               "urgent": true, "stage": "review", "agentctl.run": "r-7"}),
        "per-key merge; null clears"
    );
    let (_, list) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert_eq!(list[0]["fields"]["stage"], "review");

    for (fields, message) in [
        (
            json!({"note": "two\nlines"}),
            "field \"note\": text must be a single line; use a multiline field",
        ),
        (json!({"note": 5}), "field \"note\": expected a string"),
        (
            json!({"points": "3"}),
            "field \"points\": expected a number",
        ),
        (
            json!({"urgent": "yes"}),
            "field \"urgent\": expected true or false",
        ),
        (
            json!({"stage": "done"}),
            "field \"stage\": \"done\" is not an option",
        ),
        (
            json!({"areas": "ui"}),
            "field \"areas\": expected an array of options",
        ),
        (
            json!({"areas": ["ui", "mobile"]}),
            "field \"areas\": \"mobile\" is not an option",
        ),
        (json!({"nope": 1}), "unknown field \"nope\""),
    ] {
        let body = json!({"name": "Changed", "fields": fields});
        let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
        assert_eq!((status, text.as_str()), (StatusCode::BAD_REQUEST, message));
    }
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"fields": [1]})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "fields must be a JSON object")
    );
    let body = json!({"column_id": fx.col_todo, "name": "Bad", "fields": {"points": "x"}});
    let (status, _) = request(&fx, As::Admin, "POST", "/api/tasks", Some(body), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, list) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert_eq!(
        list.as_array().expect("array").len(),
        1,
        "a rejected create makes no task"
    );
    let (_, task) = call(&fx, "GET", &uri, None).await;
    assert_eq!(task["name"], "Typed", "a rejected patch writes nothing");
}

#[tokio::test]
async fn tasks_without_required_fields_should_be_created_and_flagged_on_the_card() {
    let (_dir, fx) = fixture();
    define_every_kind(&fx).await;
    let before = |id: &str| fx.db.task(id).expect("read").expect("task").history.len();
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Draft"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "required is a hint, never a 400"
    );
    let id = task["id"].as_str().expect("id").to_string();
    let card = format!("/api/tasks/{id}/card");
    let (_, html) = request(&fx, As::Admin, "GET", &card, None, false).await;
    assert!(html.contains("card-fields-incomplete"));
    assert!(html.contains(">fields incomplete<"));

    let history = before(&id);
    let (status, _) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"fields": {"agentctl.checks": "cargo test"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before(&id), history, "field changes are not history");
    let (_, html) = request(&fx, As::Admin, "GET", &card, None, false).await;
    assert!(!html.contains("card-fields-incomplete"));
}

#[tokio::test]
async fn task_modal_should_render_user_fields_editable_and_system_fields_read_only() {
    let (_dir, fx) = fixture();
    define_every_kind(&fx).await;
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Modal",
               "fields": {"stage": "review", "areas": ["db"], "agentctl.run": "<r-1>"}}),
    )
    .await;
    let (status, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/modal"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("id=\"modal-fields-wrap\""));
    assert!(html.contains(
        "data-field-key=\"agentctl.checks\" data-field-kind=\"multiline\" data-required=\"true\""
    ));
    assert!(html.contains("tm-field tm-field-missing\" data-field-key=\"agentctl.checks\""));
    assert!(html.contains("<textarea"));
    assert!(html.contains("<option value=\"review\" selected>review</option>"));
    assert!(html.contains("<input type=\"checkbox\" value=\"db\" checked>"));
    assert!(html.contains("type=\"number\" step=\"any\" value=\"3\""));
    assert!(html.contains("class=\"tm-field-req\""));
    assert!(html.contains("id=\"modal-system-fields-wrap\""));
    assert!(html.contains("<dd data-field-key=\"agentctl.run\">&lt;r-1&gt;</dd>"));
    let user_block = html
        .split("modal-system-fields-wrap")
        .next()
        .expect("user part");
    assert!(
        !user_block.contains("agentctl.run"),
        "system fields are not editable inputs"
    );
}

#[tokio::test]
async fn task_modal_should_render_tabs_and_the_details_sidebar() {
    let (_dir, fx) = fixture();
    define_every_kind(&fx).await;
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Modal", "fields": {"stage": "review"}}),
    )
    .await;
    let uri = format!("/api/tasks/{id}/modal");
    let (status, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK);
    for tab in [
        "description",
        "comments",
        "subtasks",
        "relations",
        "files",
        "history",
    ] {
        assert!(
            html.contains(&format!("data-tm-tab=\"{tab}\"")),
            "{tab} tab"
        );
        assert!(
            html.contains(&format!("id=\"tm-panel-{tab}\"")),
            "{tab} panel"
        );
    }
    assert!(html.contains("id=\"modal-subtask-tabcount\""));
    assert!(html.contains("id=\"tm-field-groups\""));
    assert!(html.contains("id=\"tm-group-edit-fields\""));
    for group in ["details", "edit-fields"] {
        assert!(
            html.contains(&format!(
                "data-fold-group=\"{group}\" aria-expanded=\"true\" aria-controls=\"tm-body-{group}\""
            )),
            "{group} fold button"
        );
        assert!(
            html.contains(&format!("id=\"tm-body-{group}\"")),
            "{group} body"
        );
    }
    assert!(html.contains("data-sidebar-hide aria-expanded=\"true\" aria-controls=\"tm-side\""));
    assert!(html.contains("data-sidebar-show aria-expanded=\"false\" aria-controls=\"tm-side\""));
}

#[tokio::test]
async fn board_settings_should_list_custom_fields() {
    let (_dir, fx) = fixture();
    define_every_kind(&fx).await;
    let uri = format!("/b/{}/settings/fields", fx.board_id);
    let (status, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<code>agentctl.checks</code>"));
    assert!(
        html.contains("select: draft · review"),
        "options are listed without commas"
    );
    assert!(html.contains("id=\"bf-add\""));
    assert!(html.contains("bshell-nav-item bshell-nav-active\">Custom fields</a>"));
}

#[test]
fn stored_rows_without_claims_or_fields_should_still_deserialize() {
    let row = json!({
        "id": "t", "column_id": "c", "swimlane_id": null, "name": "Old",
        "description": "", "size": 1, "position": 0.0,
        "created_at": "2026-09-01T10:00:00Z", "completed_at": null,
        "total_minutes": 0,
    });
    let task: crate::models::TaskRow = serde_json::from_value(row).expect("old row");
    assert!(task.fields.is_empty());
    assert!(task.claim.is_none());
    assert_eq!(task.claim_fence, 0);
    assert!(task.links.is_empty());
}

#[tokio::test]
async fn a_patch_rejected_for_a_field_value_should_change_nothing() {
    let (_dir, fx) = fixture();
    define_every_kind(&fx).await;
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Keep"})).await;
    let before = detail(&fx, &id).await;
    let uri = format!("/api/tasks/{id}");
    let body = json!({"name": "Changed", "metadata": {"m": 1},
                      "fields": {"note": "ok", "points": "not a number"}});
    let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "field \"points\": expected a number"
        )
    );
    assert_eq!(detail(&fx, &id).await, before);
}

/// Posts a form body as `who`; returns the status and body.
async fn form_post(fx: &Fixture, who: As<'_>, uri: &str, form: &str) -> (StatusCode, String) {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    builder = match who {
        As::Admin => builder.header("cookie", &fx.cookie),
        As::Bearer(token) => builder.header("authorization", format!("Bearer {token}")),
        As::Anonymous => builder,
    };
    let response = fx
        .app
        .clone()
        .oneshot(
            builder
                .body(axum::body::Body::from(form.to_string()))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Starts device pairing anonymously; returns the parsed response.
async fn start_device(fx: &Fixture, body: Value) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device")
        .header("host", "pomb.test:3350")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("req");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn poll_device(fx: &Fixture, device_code: &str) -> (StatusCode, String) {
    let (status, text) = poll_device_raw(fx, device_code).await;
    match serde_json::from_str::<Value>(&text) {
        Ok(body) if body["code"].is_string() => {
            (status, body["code"].as_str().expect("code").to_string())
        }
        _ => (status, text),
    }
}

fn assert_error(status: StatusCode, body: &Value, expected: StatusCode, code: &str, message: &str) {
    assert_eq!(
        (status, &body["code"], &body["message"]),
        (expected, &json!(code), &json!(message))
    );
}

async fn poll_device_raw(fx: &Fixture, device_code: &str) -> (StatusCode, String) {
    let body = json!({"device_code": device_code});
    request(
        fx,
        As::Anonymous,
        "POST",
        "/api/v1/auth/device/token",
        Some(body),
        true,
    )
    .await
}

async fn paired(fx: &Fixture, scopes: Value) -> (String, String) {
    let (status, started) = start_device(
        fx,
        json!({"client_name": "agentctl on laptop", "scopes": scopes}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    (
        started["device_code"]
            .as_str()
            .expect("device_code")
            .to_string(),
        started["user_code"]
            .as_str()
            .expect("user_code")
            .to_string(),
    )
}

#[tokio::test]
async fn device_pairing_should_start_without_auth_and_validate_input() {
    let (_dir, fx) = fixture();
    let (status, started) = start_device(
        &fx,
        json!({"client_name": "agentctl", "scopes": ["work", "read", "read"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert!(started["device_code"].as_str().expect("code").len() >= 43);
    let user_code = started["user_code"].as_str().expect("user code");
    assert_eq!(user_code.len(), 9);
    assert_eq!(&user_code[4..5], "-");
    assert!(
        user_code
            .replace('-', "")
            .chars()
            .all(|c| "BCDFGHJKLMNPQRSTVWXZ".contains(c))
    );
    assert_eq!(started["verification_url"], "http://pomb.test:3350/device");
    assert_eq!(started["interval_seconds"], 5);
    assert_eq!(started["expires_in_seconds"], 600);

    let scopes_message = format!(
        "scopes must be a non-empty subset of {}",
        AuthScope::quoted_list()
    );
    for (body, message) in [
        (
            json!({"scopes": ["read"]}),
            "client_name must be 1-80 characters",
        ),
        (
            json!({"client_name": "x".repeat(81), "scopes": ["read"]}),
            "client_name must be 1-80 characters",
        ),
        (json!({"client_name": "a"}), scopes_message.as_str()),
        (
            json!({"client_name": "a", "scopes": []}),
            scopes_message.as_str(),
        ),
        (
            json!({"client_name": "a", "scopes": ["read", "write"]}),
            scopes_message.as_str(),
        ),
    ] {
        let (status, body) = start_device(&fx, body).await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            message,
        );
    }
}

fn standard_role_permission_names(name: &str) -> String {
    STANDARD_ROLES
        .iter()
        .find(|role| role.name == name)
        .expect("standard role")
        .permissions()
        .iter()
        .map(|permission| permission.name())
        .collect::<Vec<_>>()
        .join(",")
}

#[tokio::test]
async fn device_request_should_accept_a_standard_role() {
    let (_dir, fx) = fixture();
    let (status, started) =
        start_device(&fx, json!({"client_name": "agentctl", "role": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let user_code = started["user_code"].as_str().expect("user code");
    let (status, page) =
        form_post(&fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(&format!(
            "data-requested=\"{}\"",
            standard_role_permission_names("lead")
        )),
        "{page}"
    );
}

#[tokio::test]
async fn device_request_should_refuse_an_unknown_role() {
    let (_dir, fx) = fixture();
    let (status, body) =
        start_device(&fx, json!({"client_name": "agentctl", "role": "scout"})).await;
    assert_error(
        status,
        &body,
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "role must be one of planner, implementer, reviewer, verifier, release, lead",
    );
    for extra in [
        json!({"scopes": ["read"]}),
        json!({"preset": "read"}),
        json!({"permissions": ["tasks.read"]}),
    ] {
        let mut body = json!({"client_name": "agentctl", "role": "lead"});
        body.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let (status, body) = start_device(&fx, body).await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "send exactly one of scopes, preset, permissions and role",
        );
    }
}

#[tokio::test]
async fn device_approval_page_should_preselect_the_named_role() {
    let (_dir, fx) = fixture();
    let permissions = standard_role_permission_names("lead");
    let names: Vec<&str> = permissions.split(',').collect();
    role_of(&fx, "lead", &names);
    let (status, started) =
        start_device(&fx, json!({"client_name": "agentctl", "role": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let user_code = started["user_code"].as_str().expect("user code");
    let (status, page) =
        form_post(&fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("data-role=\"lead\""), "{page}");
}

#[tokio::test]
async fn device_approval_page_should_fall_back_without_the_role() {
    let (_dir, fx) = fixture();
    let (status, started) =
        start_device(&fx, json!({"client_name": "agentctl", "role": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let user_code = started["user_code"].as_str().expect("user code");
    let (status, page) =
        form_post(&fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("data-role=\"\""), "{page}");
    assert!(
        page.contains(&format!(
            "data-requested=\"{}\"",
            standard_role_permission_names("lead")
        )),
        "{page}"
    );
}

#[tokio::test]
async fn device_pairing_should_issue_a_named_revocable_token_once_after_approval() {
    let (_dir, fx) = fixture();
    let (device_code, user_code) = paired(&fx, json!(["read", "admin"])).await;
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::PRECONDITION_REQUIRED, "authorization_pending")
    );
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::TOO_MANY_REQUESTS, "slow_down")
    );

    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device",
        &format!("user_code={}", user_code.to_lowercase()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("agentctl on laptop"),
        "the page names the client"
    );
    assert!(page.contains("the <strong>admin</strong> scope"));
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Approved."));

    // That device polled a moment ago; a second one shows the issuing poll
    // without waiting out the interval.
    let (other_code, other_user) = paired(&fx, json!(["read"])).await;
    form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={other_user}"),
    )
    .await;
    let (status, body) = poll_device(&fx, &other_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let issued: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(issued["name"], "agentctl on laptop");
    assert_eq!(issued["scopes"], json!(["read"]));
    let token = issued["token"].as_str().expect("token").to_string();
    assert!(token.starts_with("pomb_"));
    let (status, text) = poll_device(&fx, &other_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "invalid_grant"),
        "the code is spent"
    );

    let tokens = fx.db.list_api_tokens().expect("tokens");
    assert!(
        tokens
            .iter()
            .any(|t| t.name == "agentctl on laptop" && t.scope == AuthScope::Read)
    );
    let uri = format!("/api/boards/{}/tasks", fx.board_id);
    assert_eq!(
        request(&fx, As::Bearer(&token), "GET", &uri, None, true)
            .await
            .0,
        StatusCode::OK
    );
    let body = json!({"column_id": fx.col_todo, "name": "x"});
    assert_eq!(
        request(
            &fx,
            As::Bearer(&token),
            "POST",
            "/api/tasks",
            Some(body),
            true
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "a read-only device token cannot write"
    );
    let listed = request(&fx, As::Admin, "GET", "/api/v1/auth/tokens", None, true)
        .await
        .1;
    assert!(
        listed.contains("agentctl on laptop"),
        "visible in Settings → API tokens"
    );
}

#[tokio::test]
async fn device_pairing_should_report_denied_expired_and_unknown_codes() {
    let (_dir, fx) = fixture();
    let (device_code, user_code) = paired(&fx, json!(["read"])).await;
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/deny",
        &format!("user_code={user_code}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Denied."));
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::FORBIDDEN, "access_denied")
    );

    let now = chrono::Utc::now().timestamp();
    let expired_code = "expired-device-code";
    let row = crate::models::DeviceAuthRow {
        device_code_hash: crate::auth::hash_device_code(expired_code),
        user_code: "BCDFGHJK".into(),
        client_name: "old".into(),
        scope: AuthScope::Read,
        permissions: AuthScope::Read.permissions(),
        boards: BoardAccess::All,
        status: crate::models::DeviceAuthStatus::Pending,
        created_at: now - 700,
        expires_at: now - 100,
        last_poll_at: None,
        token_expires_at: None,
        account_id: None,
        client_network: None,
        role_id: None,
        requested_role: None,
        requested_boards: Vec::new(),
        requested_hours: None,
        reason: None,
    };
    fx.db.create_device_auth(&row, now).expect("insert");
    let (status, text) = poll_device(&fx, expired_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::GONE, "device_code_expired")
    );
    let (status, _) = form_post(&fx, As::Admin, "/device", "user_code=BCDF-GHJK").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an expired code cannot be approved"
    );

    let (status, text) = poll_device(&fx, "never-issued").await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "invalid_grant")
    );
    let (status, text) = request(
        &fx,
        As::Anonymous,
        "POST",
        "/api/v1/auth/device/token",
        Some(json!({})),
        true,
    )
    .await;
    let body: Value = serde_json::from_str(&text).expect("json error body");
    assert_error(
        status,
        &body,
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "device_code is required",
    );
}

#[tokio::test]
async fn a_bearer_token_should_not_approve_a_device() {
    let (_dir, fx) = fixture();
    let (record, writer) = new_api_token_record(&fx.user_id, "writer", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");
    let (device_code, user_code) = paired(&fx, json!(["admin"])).await;
    for path in ["/device/approve", "/device/deny", "/device"] {
        let (status, _) = form_post(
            &fx,
            As::Bearer(&writer),
            path,
            &format!("user_code={user_code}"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::PRECONDITION_REQUIRED, "authorization_pending")
    );
    let (status, _) = form_post(
        &fx,
        As::Anonymous,
        "/device/approve",
        &format!("user_code={user_code}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "no session: redirected to log in"
    );
}

#[tokio::test]
async fn device_pairing_should_limit_pending_requests_wrong_codes_and_clean_up() {
    let (_dir, fx) = fixture();
    let now = chrono::Utc::now().timestamp();
    let stale = crate::models::DeviceAuthRow {
        device_code_hash: crate::auth::hash_device_code("stale"),
        user_code: "ZZZZZZZZ".into(),
        client_name: "stale".into(),
        scope: AuthScope::Read,
        permissions: AuthScope::Read.permissions(),
        boards: BoardAccess::All,
        status: crate::models::DeviceAuthStatus::Pending,
        created_at: now - 3 * 86_400,
        expires_at: now - 2 * 86_400,
        last_poll_at: None,
        token_expires_at: None,
        account_id: None,
        client_network: None,
        role_id: None,
        requested_role: None,
        requested_boards: Vec::new(),
        requested_hours: None,
        reason: None,
    };
    fx.db.create_device_auth(&stale, now).expect("insert");
    assert_eq!(poll_device(&fx, "stale").await.0, StatusCode::GONE);
    for _ in 0..crate::db::Db::MAX_PENDING_DEVICE_AUTHS_PER_NETWORK {
        paired(&fx, json!(["read"])).await;
    }
    assert_eq!(
        poll_device(&fx, "stale").await,
        (StatusCode::BAD_REQUEST, "invalid_grant".to_string()),
        "a request two days past expiry is deleted lazily"
    );
    let (status, body) =
        start_device(&fx, json!({"client_name": "one more", "scopes": ["read"]})).await;
    assert_error(
        status,
        &body,
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "too many pending device requests",
    );

    for attempt in 0..10 {
        let (status, page) = form_post(&fx, As::Admin, "/device", "user_code=QQQQ-QQQQ").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "attempt {attempt}");
        assert!(page.contains("unknown or has expired"));
    }
    let (status, page) = form_post(&fx, As::Admin, "/device", "user_code=QQQQ-QQQQ").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(page.contains("Too many wrong codes"));
}

#[tokio::test]
async fn moves_into_a_full_column_should_get_409_unless_forced() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Review", Some(2))
        .expect("column");
    let mover = create(&fx, json!({"column_id": fx.col_todo, "name": "Mover"})).await;
    let mv = format!("/api/tasks/{mover}/move");
    let into_review = json!({"column_id": limited, "position": 0});

    create(&fx, json!({"column_id": limited, "name": "R1"})).await;
    let (status, _) = call(&fx, "POST", &mv, Some(into_review.clone())).await;
    assert_eq!(status, StatusCode::OK, "1 of 2 leaves room");
    let back = json!({"column_id": fx.col_todo, "position": 0});
    call(&fx, "POST", &mv, Some(back)).await;
    let second = create(&fx, json!({"column_id": limited, "name": "R2"})).await;

    let before = detail(&fx, &mover).await;
    let (status, text) =
        request(&fx, As::Admin, "POST", &mv, Some(into_review.clone()), true).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, "column WIP limit reached (2/2)")
    );
    assert_eq!(
        detail(&fx, &mover).await,
        before,
        "a refused move changes nothing"
    );

    let reorder = json!({"column_id": limited, "position": 0});
    let (status, _) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{second}/move"),
        Some(reorder),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a move within the full column adds nothing"
    );

    let (status, _) = call(
        &fx,
        "POST",
        &format!("{mv}?force=true"),
        Some(into_review.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "force bypasses the limit");
    assert_eq!(detail(&fx, &mover).await["column_id"], limited.as_str());

    let other = create(&fx, json!({"column_id": fx.col_todo, "name": "Other"})).await;
    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &format!("/api/tasks/{other}/move"),
        Some(into_review),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, "column WIP limit reached (3/2)")
    );
    let unknown = json!({"column_id": "nope", "position": 0});
    let (status, text) = request(&fx, As::Admin, "POST", &mv, Some(unknown), true).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "unknown column"),
        "unchanged"
    );
}

#[tokio::test]
async fn the_column_delete_moves_should_send_a_valid_position() {
    let (_dir, fx) = fixture();
    let doomed = fx
        .db
        .create_column(&fx.board_id, "Doomed", None)
        .expect("column");
    let first = create(&fx, json!({"column_id": doomed, "name": "First"})).await;
    let second = create(&fx, json!({"column_id": doomed, "name": "Second"})).await;
    create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Already there"}),
    )
    .await;

    // The body the column-delete dialog sends for each task
    for task in [&first, &second] {
        let body = json!({"column_id": fx.col_todo, "position": 1e9});
        let uri = format!("/api/tasks/{task}/move?force=true");
        let (status, text) = request(&fx, As::Admin, "POST", &uri, Some(body), false).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
    let (status, _) = call(&fx, "DELETE", &format!("/api/columns/{doomed}"), None).await;
    assert_eq!(status, StatusCode::OK, "the emptied column deletes");
    let (_, list) = call_list(
        &fx,
        "GET",
        &format!(
            "/api/boards/{}/tasks?column_id={}",
            fx.board_id, fx.col_todo
        ),
        None,
    )
    .await;
    let names: Vec<&str> = list
        .as_array()
        .expect("array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        vec!["Already there", "First", "Second"],
        "moved tasks land at the end, in order"
    );
    let app = include_str!("../../static/app.js");
    assert!(app.contains("{ column_id: targetId, position: nextPosition[lane]++ }"));
    assert!(!app.contains("'POST', { column_id: targetId });"));
}

#[test]
fn task_row_schema_text_should_have_no_rustdoc_links() {
    let spec = super::ApiDoc::openapi().to_json().expect("json");
    assert!(
        !spec.contains("[`"),
        "OpenAPI descriptions carry no rustdoc link syntax"
    );
}

#[test]
fn the_board_ui_should_force_a_task_move_only_after_asking() {
    let app = include_str!("../../static/app.js");
    assert_eq!(app.matches("'/move?force=true'").count(), 0);
    assert_eq!(app.matches("(force ? '?force=true' : '')").count(), 2);
    assert_eq!(app.matches("wip_limit_reached").count(), 1);
}

#[tokio::test]
async fn comments_should_record_how_they_were_written_and_their_kind() {
    let (_dir, fx) = fixture();
    let (record, bot) = new_api_token_record(&fx.user_id, "agentctl-bot", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Talk"})).await;
    let uri = format!("/api/tasks/{id}/comments");

    let (status, human) = call(
        &fx,
        "POST",
        &uri,
        Some(json!({"body": "Looks good", "kind": "  "})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(human["actor_kind"], "user");
    assert_eq!(human["token_name"], Value::Null);
    assert_eq!(human["kind"], Value::Null, "a blank kind is untagged");

    let body = json!({"body": "Checks passed", "kind": "agent"});
    let (status, text) = request(&fx, As::Bearer(&bot), "POST", &uri, Some(body), true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let agent: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(agent["actor_kind"], "api_token");
    assert_eq!(agent["token_name"], "agentctl-bot");
    assert_eq!(agent["kind"], "agent");
    assert_eq!(agent["author"], "api-token");

    let spoof = json!({"body": "x", "actor_kind": "user", "token_name": "someone"});
    let (_, text) = request(&fx, As::Bearer(&bot), "POST", &uri, Some(spoof), true).await;
    let spoofed: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(
        spoofed["actor_kind"], "api_token",
        "server-filled, never from the body"
    );
    assert_eq!(spoofed["token_name"], "agentctl-bot");

    let longest = "k".repeat(64);
    let (status, _) = call(
        &fx,
        "POST",
        &uri,
        Some(json!({"body": "x", "kind": longest})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before = detail(&fx, &id).await;
    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"body": "x", "kind": "k".repeat(65)})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "comment kind must be at most 64 characters"
        )
    );
    assert_eq!(detail(&fx, &id).await, before);

    let (_, list) = call(&fx, "GET", &uri, None).await;
    assert_eq!(list[1]["kind"], "agent");
    call(
        &fx,
        "POST",
        &uri,
        Some(json!({"body": "x", "kind": "<review>"})),
    )
    .await;
    let (_, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/modal"),
        None,
        false,
    )
    .await;
    assert!(html.contains("<span class=\"tm-comment-kind\" title=\"agent\">agent</span>"));
    assert!(
        html.contains(">&lt;review&gt;</span>"),
        "the tag is escaped"
    );
}

#[test]
fn stored_comments_without_kinds_should_read_as_user_comments() {
    let old =
        json!({"id": "c", "author": "audit", "body": "hi", "created_at": "2026-09-01T10:00:00Z"});
    let comment: crate::models::TaskComment = serde_json::from_value(old).expect("old comment");
    assert_eq!(comment.actor_kind, ActorKind::User);
    assert_eq!(comment.token_name, None);
    assert_eq!(comment.kind, None);
}

#[tokio::test]
async fn attachment_docs_and_schema_should_match_the_served_download_route() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Files"})).await;
    let data = base64::engine::general_purpose::STANDARD.encode(b"hello");
    let body = json!({"name": "a.txt", "mime": "text/plain", "data": data});
    let (status, uploaded) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/attachments"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        uploaded.get("download_url").is_none(),
        "the upload returns no download_url"
    );
    let file = format!(
        "/api/tasks/{id}/attachments/{}/file",
        uploaded["id"].as_str().expect("id")
    );
    let (status, bytes) = request(&fx, As::Admin, "GET", &file, None, false).await;
    assert_eq!((status, bytes.as_str()), (StatusCode::OK, "hello"));

    let spec: Value =
        serde_json::from_str(&super::ApiDoc::openapi().to_json().expect("json")).expect("spec");
    let attachment = &spec["components"]["schemas"]["TaskAttachment"]["properties"];
    assert!(attachment.get("download_url").is_none());
    assert!(
        spec["paths"]
            .get("/api/tasks/{id}/attachments/{attachment_id}/file")
            .is_some()
    );
}

#[tokio::test]
async fn renaming_an_attachment_should_keep_its_extension_and_log_the_change() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Files"})).await;
    let upload = |name: &'static str| {
        let data = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
        json!({"name": name, "mime": "text/plain", "data": data})
    };
    let post = format!("/api/tasks/{id}/attachments");
    let (_, notes) = call(&fx, "POST", &post, Some(upload("notes.md.txt"))).await;
    let (_, bare) = call(&fx, "POST", &post, Some(upload("README"))).await;
    let uri = |attachment: &Value| {
        format!(
            "/api/tasks/{id}/attachments/{}",
            attachment["id"].as_str().expect("id")
        )
    };

    for (sent, expected) in [
        ("plan", "plan.txt"),
        ("plan.txt", "plan.txt"),
        ("Survey note.md", "Survey note.md.txt"),
        ("plan.pdf", "plan.pdf.txt"),
    ] {
        let (status, renamed) = call(&fx, "PATCH", &uri(&notes), Some(json!({"name": sent}))).await;
        assert_eq!(status, StatusCode::OK, "{sent}: {renamed}");
        assert_eq!(renamed["name"], expected, "{sent}");
        assert_eq!(renamed["id"], notes["id"]);
        assert_eq!(renamed["size"], "notes.md.txt".len());
    }
    let (_, renamed) = call(&fx, "PATCH", &uri(&bare), Some(json!({"name": "LICENSE"}))).await;
    assert_eq!(renamed["name"], "LICENSE");

    let row = fx.db.task(&id).expect("read").expect("task");
    let names: Vec<&str> = row.attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["plan.pdf.txt", "LICENSE"]);
    let renames: Vec<&str> = row
        .history
        .iter()
        .filter(|event| event.kind == HistoryKind::AttachmentRenamed)
        .map(|event| event.detail.as_str())
        .collect();
    assert_eq!(renames.len(), 5);
    assert_eq!(renames[0], "Renamed \"notes.md.txt\" to \"plan.txt\"");

    let file = format!("{}/file", uri(&notes));
    let (status, bytes) = request(&fx, As::Admin, "GET", &file, None, false).await;
    assert_eq!((status, bytes.as_str()), (StatusCode::OK, "notes.md.txt"));
}

#[tokio::test]
async fn retired_agent_docs_should_be_gone() {
    let (_dir, fx) = fixture();
    for path in ["/agents.md", "/agents/skill.md", "/.well-known/agents.json"] {
        for caller in [As::Anonymous, As::Admin] {
            let (status, _) = request(&fx, caller, "GET", path, None, false).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }
    for path in ["/skill.md", "/llms.txt", "/docs/api.md"] {
        let (status, _) = request(&fx, As::Anonymous, "GET", path, None, false).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
}

/// The whole stored row as JSON, history and attachments included.
fn stored(fx: &Fixture, task: &str) -> Value {
    serde_json::to_value(fx.db.task(task).expect("read").expect("task")).expect("json")
}

#[tokio::test]
async fn rejected_task_extras_should_leave_the_stored_row_unchanged() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Extras"})).await;
    let (_, sub) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/subtasks"),
        Some(json!({"name": "One"})),
    )
    .await;
    let sub_id = sub["id"].as_str().expect("sub id").to_string();
    let data = base64::engine::general_purpose::STANDARD.encode(b"x");
    let up = json!({"name": "a.txt", "data": data});
    let (_, attachment) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/attachments"),
        Some(up),
    )
    .await;
    let attachment_id = attachment["id"]
        .as_str()
        .expect("attachment id")
        .to_string();
    let before = stored(&fx, &id);
    let entries = fx.db.list_entries(&id).expect("entries").len();

    let rejected: Vec<(&str, String, Option<Value>, StatusCode)> = vec![
        (
            "POST",
            format!("/api/tasks/{id}/watch"),
            Some(json!({"watch": true})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            format!("/api/tasks/{id}/time"),
            Some(json!({"minutes": 0})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            format!("/api/tasks/{id}/time"),
            Some(json!({"minutes": 1441})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            format!("/api/tasks/{id}/subtasks"),
            Some(json!({"name": " "})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "PATCH",
            format!("/api/tasks/{id}/subtasks/{sub_id}"),
            Some(json!({"name": ""})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "PATCH",
            format!("/api/tasks/{id}/subtasks/nope"),
            Some(json!({"done": true})),
            StatusCode::NOT_FOUND,
        ),
        (
            "DELETE",
            format!("/api/tasks/{id}/subtasks/nope"),
            None,
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/api/tasks/{id}/attachments"),
            Some(json!({"name": "b", "data": "%%%"})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            format!("/api/tasks/{id}/attachments"),
            Some(json!({"name": " ", "data": "eA=="})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "PATCH",
            format!("/api/tasks/{id}/attachments/nope"),
            Some(json!({"name": "b"})),
            StatusCode::NOT_FOUND,
        ),
        (
            "PATCH",
            format!("/api/tasks/{id}/attachments/{attachment_id}"),
            Some(json!({"name": " "})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "PATCH",
            format!("/api/tasks/{id}/attachments/{attachment_id}"),
            Some(json!({"name": "x".repeat(252)})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "DELETE",
            format!("/api/tasks/{id}/attachments/nope"),
            None,
            StatusCode::NOT_FOUND,
        ),
    ];
    for (method, uri, body, expected) in rejected {
        let (status, text) = request(&fx, As::Admin, method, &uri, body, true).await;
        assert_eq!(status, expected, "{method} {uri}: {text}");
        assert_eq!(
            stored(&fx, &id),
            before,
            "{method} {uri} left the row unchanged"
        );
    }
    assert_eq!(
        fx.db.list_entries(&id).expect("entries").len(),
        entries,
        "no time entry written"
    );

    for (method, uri, body) in [
        (
            "POST",
            "/api/tasks/nope/watch",
            Some(json!({"watched": true})),
        ),
        ("POST", "/api/tasks/nope/time", Some(json!({"minutes": 5}))),
        (
            "POST",
            "/api/tasks/nope/subtasks",
            Some(json!({"name": "x"})),
        ),
        (
            "POST",
            "/api/tasks/nope/attachments",
            Some(json!({"name": "a", "data": "eA=="})),
        ),
        (
            "PATCH",
            "/api/tasks/nope/attachments/nope",
            Some(json!({"name": "b"})),
        ),
    ] {
        let (status, _) = request(&fx, As::Admin, method, uri, body, true).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(
        fx.db.list_entries("nope").expect("entries").is_empty(),
        "no orphan time entry"
    );
}

#[tokio::test]
async fn task_extras_should_save_their_change_and_history_event_together() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Extras"})).await;
    let base = format!("/api/tasks/{id}");
    let history = |fx: &Fixture| -> Vec<HistoryKind> {
        let row = fx.db.task(&id).expect("read").expect("task");
        row.history.into_iter().map(|e| e.kind).collect()
    };
    let mut expected = history(&fx);
    let mut step = |fx: &Fixture, kind: HistoryKind| {
        expected.push(kind);
        assert_eq!(history(fx), expected, "after {kind:?}");
    };

    call(
        &fx,
        "POST",
        &format!("{base}/watch"),
        Some(json!({"watched": true})),
    )
    .await;
    step(&fx, HistoryKind::Watched);
    let (status, _) = request(
        &fx,
        As::Admin,
        "POST",
        &format!("{base}/time"),
        Some(json!({"minutes": 15})),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    step(&fx, HistoryKind::TimeLogged);
    assert_eq!(fx.db.list_entries(&id).expect("entries").len(), 1);
    let (_, sub) = call(
        &fx,
        "POST",
        &format!("{base}/subtasks"),
        Some(json!({"name": "One"})),
    )
    .await;
    step(&fx, HistoryKind::SubtaskAdded);
    let sub_uri = format!("{base}/subtasks/{}", sub["id"].as_str().expect("id"));
    let (_, done) = call(&fx, "PATCH", &sub_uri, Some(json!({"done": true}))).await;
    assert_eq!(done["done"], true);
    step(&fx, HistoryKind::SubtaskDone);
    let (_, renamed) = call(&fx, "PATCH", &sub_uri, Some(json!({"name": "Uno"}))).await;
    assert_eq!(renamed["name"], "Uno");
    step(&fx, HistoryKind::SubtaskRenamed);
    call(&fx, "DELETE", &sub_uri, None).await;
    step(&fx, HistoryKind::SubtaskDeleted);
    let data = base64::engine::general_purpose::STANDARD.encode(b"x");
    let (_, attachment) = call(
        &fx,
        "POST",
        &format!("{base}/attachments"),
        Some(json!({"name": "a.txt", "data": data})),
    )
    .await;
    step(&fx, HistoryKind::AttachmentAdded);
    let attachment_uri = format!(
        "{base}/attachments/{}",
        attachment["id"].as_str().expect("id")
    );
    let (_, renamed) = call(&fx, "PATCH", &attachment_uri, Some(json!({"name": "b"}))).await;
    assert_eq!(renamed["name"], "b.txt");
    step(&fx, HistoryKind::AttachmentRenamed);
    call(&fx, "DELETE", &attachment_uri, None).await;
    step(&fx, HistoryKind::AttachmentDeleted);
    let row = fx.db.task(&id).expect("read").expect("task");
    assert!(row.watched && row.subtasks.is_empty() && row.attachments.is_empty());
    let deleted = row
        .history
        .iter()
        .find(|event| event.kind == HistoryKind::AttachmentDeleted)
        .map(|event| event.detail.as_str());
    assert_eq!(deleted, Some("Deleted \"b.txt\""));
}

#[tokio::test]
async fn a_cross_board_move_should_keep_valid_shared_fields_and_record_the_dropped_ones() {
    let (_dir, fx) = fixture();
    for body in [
        json!({"key": "note", "label": "Note", "kind": "text"}),
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["draft", "review"]}),
        json!({"key": "only_here", "label": "Only here", "kind": "text"}),
        json!({"key": "flag", "label": "Flag", "kind": "bool"}),
    ] {
        assert_eq!(define(&fx, body).await.0, StatusCode::OK);
    }
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Other"}))).await;
    let other = board["id"].as_str().expect("board").to_string();
    for body in [
        json!({"key": "note", "label": "Note", "kind": "text"}),
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["draft"]}),
        json!({"key": "flag", "label": "Flag", "kind": "number"}),
    ] {
        let (status, _) = call(
            &fx,
            "POST",
            &format!("/api/boards/{other}/fields"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, columns) = call(&fx, "GET", &format!("/api/boards/{other}/columns"), None).await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let into_other = json!({"column_id": other_column, "position": 0});

    let a = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "A",
               "fields": {"note": "hi", "stage": "review", "only_here": "x", "flag": true}}),
    )
    .await;
    let (status, _) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{a}/move"),
        Some(into_other.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail(&fx, &a).await["fields"],
        json!({"note": "hi"}),
        "only the valid shared key survives"
    );
    let moved = event_details(&fx, &a, HistoryKind::Moved);
    assert_eq!(
        moved.last().expect("moved event"),
        "Moved from Todo to To-do; dropped fields: {\"flag\":true,\"only_here\":\"x\",\"stage\":\"review\"}",
        "undefined keys and values invalid on the target are dropped and recorded"
    );

    let b = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "B", "fields": {"stage": "draft"}}),
    )
    .await;
    call(
        &fx,
        "POST",
        &format!("/api/tasks/{b}/move"),
        Some(into_other),
    )
    .await;
    assert_eq!(detail(&fx, &b).await["fields"], json!({"stage": "draft"}));
    assert_eq!(
        event_details(&fx, &b, HistoryKind::Moved),
        vec!["Moved from Todo to To-do".to_string()]
    );

    let c = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "C", "fields": {"only_here": "x", "stage": "review"}}),
    )
    .await;
    let done = json!({"column_id": fx.col_done, "position": 0});
    call(&fx, "POST", &format!("/api/tasks/{c}/move"), Some(done)).await;
    assert_eq!(
        detail(&fx, &c).await["fields"],
        json!({"only_here": "x", "stage": "review"}),
        "same board: untouched"
    );
    assert_eq!(
        event_details(&fx, &c, HistoryKind::Moved),
        vec!["Moved from Todo to Shipped".to_string()]
    );
}

#[tokio::test]
async fn wrong_codes_on_approve_and_deny_should_count_toward_the_lockout() {
    let (_dir, fx) = fixture();
    let (device_code, user_code) = paired(&fx, json!(["read", "admin"])).await;
    for attempt in 0..10 {
        let path = if attempt % 2 == 0 {
            "/device/approve"
        } else {
            "/device/deny"
        };
        let (status, page) = form_post(&fx, As::Admin, path, "user_code=QQQQ-QQQQ").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "attempt {attempt}");
        assert!(page.contains("no longer waiting for approval"));
    }
    for path in ["/device/approve", "/device/deny", "/device"] {
        let (status, page) =
            form_post(&fx, As::Admin, path, &format!("user_code={user_code}")).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{path}: even the right code is refused"
        );
        assert!(page.contains("Too many wrong codes"));
    }
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::PRECONDITION_REQUIRED, "authorization_pending")
    );
}

#[tokio::test]
async fn each_server_instance_should_keep_its_own_lockout() {
    let (_dir_a, a) = fixture();
    for _ in 0..10 {
        form_post(&a, As::Admin, "/device", "user_code=QQQQ-QQQQ").await;
    }
    assert_eq!(
        form_post(&a, As::Admin, "/device", "user_code=QQQQ-QQQQ")
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let (_dir_b, b) = fixture();
    let (status, _) = form_post(&b, As::Admin, "/device", "user_code=QQQQ-QQQQ").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a second instance starts unlocked"
    );
}

/// Runs `make(round, which)` for two requests at once, `rounds` times.
async fn race_pairs<F, Fut>(rounds: usize, make: F)
where
    F: Fn(usize, usize) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    for round in 0..rounds {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for which in 0..2 {
            let (make, barrier) = (make.clone(), barrier.clone());
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                make(round, which).await;
            }));
        }
        for handle in handles {
            handle.await.expect("join");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_creates_in_one_cell_should_get_distinct_positions() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let mut handles = Vec::new();
    for n in 0..16 {
        let (fx, barrier) = (fx.clone(), barrier.clone());
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            create(
                &fx,
                json!({"column_id": fx.col_todo, "name": format!("T{n}")}),
            )
            .await
        }));
    }
    for handle in handles {
        handle.await.expect("join");
    }
    let (_, list) = call_list(
        &fx,
        "GET",
        &format!(
            "/api/boards/{}/tasks?column_id={}",
            fx.board_id, fx.col_todo
        ),
        None,
    )
    .await;
    let mut positions: Vec<i64> = list
        .as_array()
        .expect("array")
        .iter()
        .map(|t| t["position"].as_f64().expect("pos") as i64)
        .collect();
    positions.sort();
    assert_eq!(
        positions,
        (0..16).collect::<Vec<_>>(),
        "each create read the cell inside its write"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_field_patches_should_never_lose_each_other() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    let (_, field) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["a"]}),
    )
    .await;
    let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    let (fx2, uri2) = (fx.clone(), uri.clone());
    race_pairs(20, move |round, which| {
        let (fx, uri) = (fx2.clone(), uri2.clone());
        async move {
            let body = if which == 0 {
                json!({"label": format!("Stage {round}")})
            } else {
                json!({"options": ["a", format!("o{round}")]})
            };
            let (status, _) = call(&fx, "PATCH", &uri, Some(body)).await;
            assert_eq!(status, StatusCode::OK);
        }
    })
    .await;
    let (_, list) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/fields", fx.board_id),
        None,
    )
    .await;
    assert_eq!(list[0]["label"], "Stage 19");
    assert_eq!(
        list[0]["options"],
        json!(["a", "o19"]),
        "neither patch overwrote the other"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_repeat_only_patch_should_never_restore_an_old_due_date() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Due"})).await;
    let uri = format!("/api/tasks/{id}");
    let (fx2, uri2) = (fx.clone(), uri.clone());
    race_pairs(20, move |round, which| {
        let (fx, uri) = (fx2.clone(), uri2.clone());
        async move {
            let body = if which == 0 {
                json!({"due_at": format!("2026-11-{:02}T10:00:00Z", round + 1)})
            } else {
                json!({"due_repeat": format!("every {round} days")})
            };
            assert_eq!(call(&fx, "PATCH", &uri, Some(body)).await.0, StatusCode::OK);
        }
    })
    .await;
    let task = detail(&fx, &id).await;
    assert_eq!(
        task["due_at"], "2026-11-20T10:00:00Z",
        "the due date of the last round survives"
    );
    assert_eq!(task["due_repeat"], "every 19 days");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_field_value_should_never_outlive_its_deleted_definition() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Race"})).await;
    for round in 0..15 {
        let (_, field) = define(&fx, json!({"key": "note", "label": "Note", "kind": "text"})).await;
        let field_uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
        let task_uri = format!("/api/tasks/{id}");
        let (fx2, f2, t2) = (fx.clone(), field_uri.clone(), task_uri.clone());
        race_pairs(1, move |_, which| {
            let (fx, field_uri, task_uri) = (fx2.clone(), f2.clone(), t2.clone());
            async move {
                if which == 0 {
                    call(&fx, "DELETE", &field_uri, None).await;
                } else {
                    call(
                        &fx,
                        "PATCH",
                        &task_uri,
                        Some(json!({"fields": {"note": "x"}})),
                    )
                    .await;
                }
            }
        })
        .await;
        assert_eq!(detail(&fx, &id).await["fields"], json!({}), "round {round}");
    }
}

#[tokio::test]
async fn heartbeats_should_leave_updated_at_alone_but_claims_and_releases_change_it() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Leased"})).await;
    tick();
    let before_claim = updated_at(&fx, &id);
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;
    let claimed = updated_at(&fx, &id);
    assert!(claimed > before_claim, "a claim changes the task");
    tick();
    let since = chrono::Utc::now().to_rfc3339();
    tick();
    for _ in 0..3 {
        let (status, beat) = claim_call(
            &fx,
            &id,
            "/heartbeat",
            json!({"fence": 1, "ttl_seconds": 120}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            beat["heartbeat_at"].as_str() > Some(since.as_str()),
            "the lease moved"
        );
    }
    assert_eq!(
        updated_at(&fx, &id),
        claimed,
        "heartbeats change only the lease"
    );
    let uri = format!(
        "/api/boards/{}/tasks?since={}",
        fx.board_id,
        since.replace('+', "%2B")
    );
    let (_, list) = call_list(&fx, "GET", &uri, None).await;
    assert_eq!(list, json!([]), "a ?since= poller does not see heartbeats");
    claim_call(&fx, &id, "/release", json!({"fence": 1})).await;
    assert!(updated_at(&fx, &id) > claimed, "a release changes the task");
}

#[tokio::test]
async fn a_heartbeat_that_finds_an_expired_claim_should_record_the_expiry() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Lapsed", "a", 1).await;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let before = updated_at(&fx, &id);
    let (status, _) = claim_call(&fx, &id, "/heartbeat", json!({"fence": 1})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        updated_at(&fx, &id) > before,
        "the documented exception: expiry is recorded"
    );
    assert_eq!(count(&event_kinds(&fx, &id), HistoryKind::ClaimExpired), 1);
}

#[tokio::test]
async fn fields_null_should_be_a_400_on_create_and_patch() {
    let (_dir, fx) = fixture();
    define(&fx, json!({"key": "note", "label": "Note", "kind": "text"})).await;
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Keep", "fields": {"note": "hi"}}),
    )
    .await;
    let before = stored(&fx, &id);
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"name": "X", "fields": null})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "fields must be a JSON object")
    );
    assert_eq!(stored(&fx, &id), before);
    let body = json!({"column_id": fx.col_todo, "name": "New", "fields": null});
    let (status, text) = request(&fx, As::Admin, "POST", "/api/tasks", Some(body), true).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, "fields must be a JSON object")
    );

    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"fields": {"note": "a\rb"}})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "field \"note\": text must be a single line; use a multiline field"
        ),
        "a carriage return is a line break too"
    );

    let spec: Value =
        serde_json::from_str(&super::ApiDoc::openapi().to_json().expect("json")).expect("spec");
    for schema in ["UpdateTaskInput", "CreateTaskInput"] {
        let fields = &spec["components"]["schemas"][schema]["properties"]["fields"];
        assert_eq!(
            fields["type"], "object",
            "{schema}.fields is an object: {fields}"
        );
        assert!(
            fields.get("nullable").is_none() && fields.get("oneOf").is_none(),
            "{schema}.fields is not nullable: {fields}"
        );
        let required = spec["components"]["schemas"][schema]["required"].to_string();
        assert!(
            !required.contains("\"fields\""),
            "{schema}.fields stays optional"
        );
    }
}

#[tokio::test]
async fn fields_non_object_should_be_a_400_before_later_checks_on_create_and_patch() {
    let (_dir, fx) = fixture();
    define(&fx, json!({"key": "note", "label": "Note", "kind": "text"})).await;
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Keep", "fields": {"note": "hi"}}),
    )
    .await;
    let before = stored(&fx, &id);
    let expected = (StatusCode::BAD_REQUEST, "fields must be a JSON object");
    for fields in [json!("x"), json!([1]), json!(5), json!(true)] {
        let body = json!({"column_id": fx.col_todo, "name": "New", "fields": fields});
        let (status, text) = request(&fx, As::Admin, "POST", "/api/tasks", Some(body), true).await;
        assert_eq!((status, text.as_str()), expected, "create with {fields}");
        let body = json!({"name": "X", "estimate_hours": -5, "fields": fields});
        let uri = format!("/api/tasks/{id}");
        let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
        assert_eq!((status, text.as_str()), expected, "patch with {fields}");
    }
    let body = json!({"column_id": fx.col_todo, "name": "New", "due_at": "never", "fields": "x"});
    let (status, text) = request(&fx, As::Admin, "POST", "/api/tasks", Some(body), true).await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "invalid due_at \"never\": use RFC3339, \"YYYY-MM-DD HH:MM[:SS]\", or \"YYYY-MM-DD\""
        ),
        "create checks the due date first"
    );
    let body = json!({"due_at": "never", "fields": "x"});
    let uri = format!("/api/tasks/{id}");
    let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "invalid due_at \"never\": use RFC3339, \"YYYY-MM-DD HH:MM[:SS]\", or \"YYYY-MM-DD\""
        ),
        "patch checks the due date first"
    );
    assert_eq!(stored(&fx, &id), before);
}

#[tokio::test]
async fn stale_multiselect_value_with_comma_should_render_as_one_item() {
    let (_dir, fx) = fixture();
    let (_, areas) = define(
        &fx,
        json!({"key": "areas", "label": "Areas", "kind": "multiselect", "options": ["ui", "a, <b>", "x"]}),
    )
    .await;
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Stale", "fields": {"areas": ["ui", "a, <b>", "x"]}}),
    )
    .await;
    let uri = format!("/api/fields/{}", areas["id"].as_str().expect("id"));
    assert_eq!(
        call(&fx, "PATCH", &uri, Some(json!({"options": ["ui"]})))
            .await
            .0,
        StatusCode::OK
    );
    let (_, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/modal"),
        None,
        false,
    )
    .await;
    assert!(html.contains("<span class=\"tm-field-stale-item\">a, &lt;b&gt;</span>"));
    assert!(html.contains("<span class=\"tm-field-stale-item\">x</span>"));
    assert_eq!(html.matches("tm-field-stale-item").count(), 2);
}

#[tokio::test]
async fn stale_choices_should_show_as_invalid_and_count_as_incomplete() {
    let (_dir, fx) = fixture();
    let (_, stage) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["draft", "review"], "required": true}),
    )
    .await;
    let (_, areas) = define(
        &fx,
        json!({"key": "areas", "label": "Areas", "kind": "multiselect", "options": ["ui", "api, rest"]}),
    )
    .await;
    assert_eq!(
        areas["options"],
        json!(["ui", "api, rest"]),
        "an option may contain a comma"
    );
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Stale", "fields": {"stage": "review", "areas": ["ui", "api, rest"]}}),
    )
    .await;
    let card = format!("/api/tasks/{id}/card");
    assert!(
        !request(&fx, As::Admin, "GET", &card, None, false)
            .await
            .1
            .contains("card-fields-incomplete")
    );

    for (field, options) in [(&stage, json!(["draft"])), (&areas, json!(["ui"]))] {
        let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
        assert_eq!(
            call(&fx, "PATCH", &uri, Some(json!({"options": options})))
                .await
                .0,
            StatusCode::OK
        );
    }
    let (_, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/modal"),
        None,
        false,
    )
    .await;
    assert!(html.contains("tm-field tm-field-missing tm-field-invalid\" data-field-key=\"stage\""));
    assert!(html.contains(
        "<option value=\"review\" selected class=\"tm-field-stale\">review (not an option)</option>"
    ));
    assert!(
        html.contains("No longer options: <span class=\"tm-field-stale-item\">api, rest</span>")
    );
    assert_eq!(html.matches("tm-field-stale-item").count(), 1);
    let (_, html) = request(&fx, As::Admin, "GET", &card, None, false).await;
    assert!(
        html.contains("card-fields-incomplete"),
        "a required field holding a stale choice is incomplete"
    );
    assert_eq!(
        detail(&fx, &id).await["fields"]["stage"],
        "review",
        "the stored value stays until changed"
    );

    let (_, settings) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/b/{}/settings/fields", fx.board_id),
        None,
        false,
    )
    .await;
    assert!(
        settings.contains("<textarea name=\"options\""),
        "options are entered one per line"
    );
    assert!(
        settings.contains("id=\"bf-delete-overlay\""),
        "the delete uses the in-page confirm"
    );
    assert!(!settings.contains("window.confirm"));
}

#[tokio::test]
async fn a_long_comment_kind_should_be_truncated_with_its_full_text_in_the_title() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Kinds"})).await;
    let kind = "W".repeat(64);
    call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/comments"),
        Some(json!({"body": "x", "kind": kind})),
    )
    .await;
    let (_, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/modal"),
        None,
        false,
    )
    .await;
    assert!(html.contains(&format!(
        "<span class=\"tm-comment-kind\" title=\"{kind}\">{kind}</span>"
    )));
    let css = include_str!("../../static/style.css");
    let rule = &css[css.find(".tm-comment-kind {").expect("rule")..];
    let rule = &rule[..rule.find('}').expect("end")];
    assert!(rule.contains("text-overflow: ellipsis") && rule.contains("max-width"));
}

/// Posts a form with the session cookie and the given browser headers.
async fn browser_post(fx: &Fixture, uri: &str, form: &str, headers: &[(&str, &str)]) -> StatusCode {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "pomb.test:3350")
        .header("cookie", &fx.cookie)
        .header("content-type", "application/x-www-form-urlencoded");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(axum::body::Body::from(form.to_string()))
        .expect("req");
    fx.app
        .clone()
        .oneshot(request)
        .await
        .expect("oneshot")
        .status()
}

#[tokio::test]
async fn cross_origin_forms_should_not_approve_a_device() {
    let (_dir, fx) = fixture();
    let (device_code, user_code) = paired(&fx, json!(["read", "admin"])).await;
    let form = format!("user_code={user_code}");
    for path in ["/device", "/device/approve", "/device/deny"] {
        for headers in [
            vec![("origin", "http://127.0.0.1:3999")],
            vec![("origin", "http://evil.example")],
            vec![
                ("sec-fetch-site", "same-site"),
                ("origin", "http://127.0.0.1:3999"),
            ],
            vec![("sec-fetch-site", "cross-site")],
        ] {
            assert_eq!(
                browser_post(&fx, path, &form, &headers).await,
                StatusCode::FORBIDDEN,
                "{path} {headers:?}"
            );
        }
    }
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::PRECONDITION_REQUIRED, "authorization_pending"),
        "still pending"
    );
    let same_origin = [
        ("origin", "http://pomb.test:3350"),
        ("sec-fetch-site", "same-origin"),
    ];
    assert_eq!(
        browser_post(&fx, "/device/approve", &form, &same_origin).await,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn claims_releases_and_patches_should_survive_a_neighbour_moving_in_and_out() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    let s = create(&fx, json!({"column_id": fx.col_todo, "name": "S"})).await;
    let neighbours = [
        create(&fx, json!({"column_id": fx.col_done, "name": "N1"})).await,
        create(&fx, json!({"column_id": fx.col_done, "name": "N2"})).await,
    ];
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Stops the movers even when an assertion below fails, so a failure
    // ends the test instead of hanging it.
    struct StopOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let _stop_guard = StopOnDrop(stop.clone());

    let mut movers = Vec::new();
    for neighbour in neighbours {
        let (fx, stop) = (fx.clone(), stop.clone());
        movers.push(tokio::spawn(async move {
            let mut moves = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for column in [&fx.col_todo, &fx.col_done] {
                    let body = json!({"column_id": column, "position": 0});
                    let (status, _) = call(
                        &fx,
                        "POST",
                        &format!("/api/tasks/{neighbour}/move"),
                        Some(body),
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK);
                    moves += 1;
                }
            }
            moves
        }));
    }

    let patcher = {
        let (fx, s) = (fx.clone(), s.clone());
        tokio::spawn(async move {
            for i in 0..60 {
                let body = json!({"metadata": {format!("k{i}"): i}});
                let (status, _) = call(&fx, "PATCH", &format!("/api/tasks/{s}"), Some(body)).await;
                assert_eq!(status, StatusCode::OK);
            }
        })
    };

    let mut fences = Vec::new();
    for round in 0..60 {
        let (status, granted) = claim(
            &fx,
            &s,
            json!({"claimant": format!("c{round}"), "ttl_seconds": 60}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "round {round}: {granted}");
        let fence = granted["fence"].as_u64().expect("fence");
        let row = fx.db.task(&s).expect("read").expect("task");
        assert_eq!(
            row.claim.as_ref().map(|c| c.fence),
            Some(fence),
            "round {round}: the granted claim survived"
        );
        fences.push(fence);
        let (status, _) = claim_call(&fx, &s, "/release", json!({"fence": fence})).await;
        assert_eq!(status, StatusCode::OK);
        let row = fx.db.task(&s).expect("read").expect("task");
        assert!(row.claim.is_none(), "round {round}: the release stuck");
    }
    patcher.await.expect("patcher");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut moves = 0;
    for mover in movers {
        moves += mover.await.expect("mover");
    }
    assert!(moves > 20, "the neighbours really moved ({moves} moves)");
    assert_eq!(
        fences,
        (1..=60).collect::<Vec<u64>>(),
        "fences strictly increase without repeats"
    );
    let row = fx.db.task(&s).expect("read").expect("task");
    assert_eq!(row.claim_fence, 60);
    assert_eq!(count(&event_kinds(&fx, &s), HistoryKind::Claimed), 60);
    assert_eq!(count(&event_kinds(&fx, &s), HistoryKind::ClaimReleased), 60);
    let metadata = row.metadata.expect("metadata");
    assert_eq!(metadata.len(), 60, "no metadata write was lost");
}

#[tokio::test]
async fn rejections_found_inside_a_transaction_should_leave_the_database_file_untouched() {
    let (dir, fx) = fixture();
    let file = dir.path().join("agentctl.redb");
    let (_, field) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["a"]}),
    )
    .await;
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Bytes"})).await;
    let uri = format!("/api/tasks/{id}");
    call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"metadata": {"a": "y".repeat(40 * 1024)}})),
    )
    .await;
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 600})).await;
    let before = std::fs::read(&file).expect("read db file");

    let field_uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    let checks: Vec<(&str, String, Value)> = vec![
        (
            "PATCH",
            uri.clone(),
            json!({"metadata": {"b": "y".repeat(40 * 1024)}}),
        ),
        ("PATCH", uri.clone(), json!({"fields": {"stage": "zzz"}})),
        (
            "POST",
            format!("{uri}/claim"),
            json!({"claimant": "b", "ttl_seconds": 60}),
        ),
        (
            "POST",
            format!("{uri}/claim/heartbeat"),
            json!({"fence": 9}),
        ),
        ("POST", format!("{uri}/claim/release"), json!({"fence": 9})),
        (
            "PATCH",
            format!("{uri}/subtasks/nope"),
            json!({"done": true}),
        ),
        ("PATCH", field_uri, json!({"default": "zzz"})),
    ];
    for (method, path, body) in checks {
        let (status, _) = request(&fx, As::Admin, method, &path, Some(body), true).await;
        assert!(status.is_client_error(), "{method} {path}: {status}");
        assert!(
            std::fs::read(&file).expect("read db file") == before,
            "{method} {path} changed the file"
        );
    }
    let (status, _) = fenced_request(&fx, "PATCH", &uri, json!({"name": "x"}), Some("9")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        std::fs::read(&file).expect("read db file") == before,
        "a stale fence changed the file"
    );
}

#[tokio::test]
async fn options_and_text_values_should_refuse_every_line_break() {
    let (_dir, fx) = fixture();
    for option in ["a\nb", "a\rb", "a\u{2028}b", "a\u{2029}b", "a\u{85}b"] {
        let body =
            json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["ok", option]});
        let (status, text) = define_text(&fx, body).await;
        assert_eq!(
            (status, text.as_str()),
            (
                StatusCode::BAD_REQUEST,
                "field \"stage\": options must be single lines"
            ),
            "{option:?}"
        );
    }
    let (_, field) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["ok"]}),
    )
    .await;
    let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"options": ["ok", "x\ny"]})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "field \"stage\": options must be single lines"
        )
    );

    define(&fx, json!({"key": "note", "label": "Note", "kind": "text"})).await;
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Lines"})).await;
    for value in ["a\u{2028}b", "a\u{2029}b", "a\u{85}b", "a\rb", "a\nb"] {
        let body = json!({"fields": {"note": value}});
        let (status, text) = request(
            &fx,
            As::Admin,
            "PATCH",
            &format!("/api/tasks/{id}"),
            Some(body),
            true,
        )
        .await;
        assert_eq!(
            (status, text.as_str()),
            (
                StatusCode::BAD_REQUEST,
                "field \"note\": text must be a single line; use a multiline field"
            ),
            "{value:?}"
        );
    }
}

#[tokio::test]
async fn decided_codes_and_double_submits_should_not_count_toward_the_lockout() {
    let (_dir, fx) = fixture();
    let (_, user_code) = paired(&fx, json!(["read"])).await;
    let form = format!("user_code={user_code}");
    assert_eq!(
        form_post(&fx, As::Admin, "/device/approve", &form).await.0,
        StatusCode::OK
    );
    for path in ["/device/approve", "/device/deny", "/device"]
        .iter()
        .cycle()
        .take(15)
    {
        let (status, _) = form_post(&fx, As::Admin, path, &form).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{path}: a decided code is refused"
        );
    }
    for attempt in 0..10 {
        let (status, _) = form_post(&fx, As::Admin, "/device", "user_code=QQQQ-QQQQ").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "attempt {attempt}: the decided code was not counted"
        );
    }
    assert_eq!(
        form_post(&fx, As::Admin, "/device", "user_code=QQQQ-QQQQ")
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS,
        "unknown codes still count"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_loser_of_an_approve_deny_race_should_not_be_counted() {
    let (_dir, fx) = fixture();
    let fx = std::sync::Arc::new(fx);
    for _ in 0..10 {
        let (_, user_code) = paired(&fx, json!(["read"])).await;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for path in ["/device/approve", "/device/deny"] {
            let (fx, barrier, form) = (
                fx.clone(),
                barrier.clone(),
                format!("user_code={user_code}"),
            );
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                form_post(&fx, As::Admin, path, &form).await.0
            }));
        }
        let mut statuses = Vec::new();
        for handle in handles {
            statuses.push(handle.await.expect("join"));
        }
        statuses.sort();
        assert_eq!(statuses, vec![StatusCode::OK, StatusCode::BAD_REQUEST]);
    }
    let (status, _) = form_post(&fx, As::Admin, "/device", "user_code=QQQQ-QQQQ").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "ten lost races left the lockout untouched"
    );
}

#[tokio::test]
async fn a_same_claimant_renewal_should_change_only_the_lease() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Renew", "a", 60).await;
    let claimed = updated_at(&fx, &id);
    tick();
    let (status, renewed) = claim(
        &fx,
        &id,
        json!({"claimant": "a", "ttl_seconds": 600, "fence": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renewed["fence"], 1);
    assert_eq!(
        updated_at(&fx, &id),
        claimed,
        "a renewal is lease-only, like a heartbeat"
    );
    assert_eq!(count(&event_kinds(&fx, &id), HistoryKind::Claimed), 1);
}

fn token_with(fx: &Fixture, name: &str, scope: AuthScope) -> String {
    let (record, raw) = new_api_token_record(&fx.user_id, name, scope, None);
    fx.db.create_api_token(&record).expect("token");
    raw
}

#[tokio::test]
async fn a_work_token_should_do_the_task_workflow_and_nothing_structural() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "worker", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let call_work = |method: &'static str, uri: String, body: Option<Value>| {
        let who = As::Bearer(&work);
        let fx = &fx;
        async move { request(fx, who, method, &uri, body, true).await }
    };

    let (status, text) = call_work(
        "POST",
        "/api/tasks".into(),
        Some(json!({"column_id": fx.col_todo, "name": "Made by work"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{text}");
    let (status, text) = call_work(
        "POST",
        format!("/api/tasks/{id}/claim"),
        Some(json!({"claimant": "worker", "ttl_seconds": 900})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim: {text}");
    let fence = serde_json::from_str::<Value>(&text).expect("json")["fence"].clone();
    let (status, text) = call_work(
        "PATCH",
        format!("/api/tasks/{id}"),
        Some(json!({"name": "Renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch: {text}");
    let (status, text) = call_work(
        "POST",
        format!("/api/tasks/{id}/comments"),
        Some(json!({"body": "progress", "kind": "agent"})),
    )
    .await;
    assert!(status.is_success(), "comment: {status} {text}");
    let (status, text) = call_work(
        "POST",
        format!("/api/tasks/{id}/time"),
        Some(json!({"minutes": 25})),
    )
    .await;
    assert!(status.is_success(), "time: {status} {text}");
    let (status, text) = call_work(
        "POST",
        format!("/api/tasks/{id}/move"),
        Some(json!({"column_id": fx.col_done, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "move: {text}");
    let (status, text) = call_work(
        "POST",
        format!("/api/tasks/{id}/claim/release"),
        Some(json!({"fence": fence})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "release: {text}");

    for (method, uri, permission) in [
        (
            "DELETE",
            format!("/api/boards/{}", fx.board_id),
            "boards.delete",
        ),
        (
            "PUT",
            format!("/api/boards/{}/config", fx.board_id),
            "boards.configure",
        ),
        (
            "GET",
            format!("/api/boards/{}/calendar-feeds", fx.board_id),
            "feeds.manage",
        ),
    ] {
        let (status, text) = request(&fx, As::Bearer(&work), method, &uri, None, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied(permission),
            "{method} {uri}"
        );
    }
    let (status, _) = call(&fx, "GET", &format!("/api/boards/{}", fx.board_id), None).await;
    assert_eq!(status, StatusCode::OK, "the board survived");
}

#[tokio::test]
async fn each_structure_route_should_need_only_its_own_permission() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "worker", AuthScope::Work);
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let (status, text) = bearer_json(
        &fx,
        &work,
        "PUT",
        &format!("/api/tasks/{id}/subtasks/order"),
        json!({"order": []}),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "subtask order: {text}");
    for (method, uri, permission) in [
        ("PATCH", "/api/swimlanes/x", "swimlanes.edit"),
        ("POST", "/api/swimlanes/x/move", "swimlanes.edit"),
        ("PATCH", "/api/columns/x", "columns.edit"),
        ("POST", "/api/boards/x/colors", "colors.create"),
        ("POST", "/api/boards/x/save-as-template", "templates.save"),
    ] {
        let (status, text) = bearer_json(&fx, &work, method, uri, json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied(permission),
            "{method} {uri}"
        );
        let (status, text) = bearer_json(&fx, &lead, method, uri, json!({})).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
    }
}

#[tokio::test]
async fn a_lead_token_should_edit_columns_and_swimlanes_on_a_board_it_did_not_create() {
    let (_dir, fx) = fixture();
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let (status, text) = bearer_json(
        &fx,
        &lead,
        "PATCH",
        &format!("/api/columns/{}", fx.col_todo),
        json!({"name": "Renamed", "wip_limit": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let uri = format!("/api/swimlanes/{}", fx.lane_b);
    let (status, text) = bearer_json(&fx, &lead, "PATCH", &uri, json!({"name": "Lane"})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let uri = format!("/api/swimlanes/{}/move", fx.lane_b);
    let (status, text) = bearer_json(&fx, &lead, "POST", &uri, json!({"position": 0})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn a_work_token_should_be_refused_every_route_that_needs_a_delete_permission() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "worker", AuthScope::Work);
    let denied: Vec<_> = crate::auth::ROUTE_PERMISSIONS
        .iter()
        .filter(|(_, _, permission)| permission.name().ends_with(".delete"))
        .collect();
    assert!(denied.len() >= 9, "{}", denied.len());
    for (method, pattern, permission) in denied {
        let uri: Vec<&str> = pattern
            .split('/')
            .map(|part| if part.starts_with(':') { "x" } else { part })
            .collect();
        let uri = uri.join("/");
        let (status, text) = bearer_json(&fx, &work, method.as_str(), &uri, json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied(permission.name()),
            "{method} {uri}"
        );
    }
}

#[tokio::test]
async fn a_cross_board_move_should_need_only_tasks_move_and_keep_the_task() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "worker", AuthScope::Work);
    let reader = token_with(&fx, "reader", AuthScope::Read);
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let other = board_by(&fx, &lead, "Other").await;
    let to = column_ids_of(&fx, &other).await[0].clone();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let comment = post_comment(&fx, As::Admin, &id, json!({"body": "keep"})).await;
    let uri = format!("/api/tasks/{id}/move");
    let body = json!({"column_id": to, "position": 0});

    let (status, text) = bearer_json(&fx, &reader, "POST", &uri, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        permission_denied("tasks.move")
    );

    let (status, text) = bearer_json(&fx, &work, "POST", &uri, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, task) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(task["column_id"], json!(to));
    assert_eq!(list_comments(&fx, &id).await[0]["id"], comment["id"]);
}

#[tokio::test]
async fn an_unrouted_method_or_path_should_get_the_routers_answer_whatever_the_token_holds() {
    let (_dir, fx) = fixture();
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let admin = token_with(&fx, "boss", AuthScope::Admin);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    for (method, uri, expected) in [
        (
            "PATCH",
            "/api/boards".to_string(),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            "PUT",
            format!("/api/tasks/{id}"),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        ("GET", "/api/not-a-route".to_string(), StatusCode::NOT_FOUND),
    ] {
        let (status, answer) = bearer_json(&fx, &admin, method, &uri, json!({})).await;
        assert_eq!(status, expected, "{method} {uri}: {answer}");
        for token in [&fx.read_token, &lead] {
            let (status, text) = bearer_json(&fx, token, method, &uri, json!({})).await;
            assert_eq!(status, expected, "{method} {uri}: {text}");
            assert_eq!(text, answer, "{method} {uri}");
            assert!(!text.contains("permission_denied"), "{method} {uri}");
        }
    }
    let (_, text) = bearer_json(&fx, &lead, "GET", "/api/not-a-route", json!({})).await;
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json")["code"],
        "route_not_found"
    );
}

#[tokio::test]
async fn a_read_token_should_be_refused_every_write_and_an_admin_token_none() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    for (method, uri, needs) in [
        ("POST", "/api/tasks".to_string(), "tasks.create"),
        ("PATCH", format!("/api/tasks/{id}"), "tasks.edit"),
        ("POST", format!("/api/tasks/{id}/move"), "tasks.move"),
        ("POST", format!("/api/tasks/{id}/claim"), "tasks.claim"),
        ("POST", "/api/timer/start".to_string(), "time.write"),
        ("DELETE", format!("/api/tasks/{id}"), "tasks.bin"),
        ("POST", "/api/boards".to_string(), "boards.create"),
    ] {
        let (status, text) = request(
            &fx,
            As::Bearer(&fx.read_token),
            method,
            &uri,
            Some(json!({})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied(needs),
            "{method} {uri}"
        );
    }
    let (status, _) = request(
        &fx,
        As::Bearer(&fx.read_token),
        "GET",
        &format!("/api/tasks/{id}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let admin = token_with(&fx, "owner", AuthScope::Admin);
    let (status, text) = request(
        &fx,
        As::Bearer(&admin),
        "DELETE",
        &format!("/api/tasks/{id}"),
        None,
        true,
    )
    .await;
    assert!(status.is_success(), "{status} {text}");
}

/// Starts a device request with `extra` merged into the body of a read-scope request, and returns
/// its status, its reply and the stored row.
async fn start_device_with(
    fx: &Fixture,
    extra: Value,
) -> (StatusCode, Value, Option<DeviceAuthRow>) {
    let mut body = json!({"client_name": "agentctl on laptop", "scopes": ["read"]});
    body.as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let (status, started) = start_device(fx, body).await;
    let row = started["user_code"].as_str().map(|code| {
        fx.db
            .pending_device_auth(
                &crate::auth::normalize_user_code(code),
                chrono::Utc::now().timestamp(),
            )
            .expect("read")
            .expect("row")
    });
    (status, started, row)
}

#[tokio::test]
async fn device_request_should_store_boards_expiry_hours_and_reason() {
    let (_dir, fx) = fixture();
    let (status, started, row) = start_device_with(
        &fx,
        json!({"boards": [" Agentctl ", "Agentctl", "Nope", ""], "expires_in_hours": 8, "reason": " to sync cards "}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let row = row.expect("row");
    assert_eq!(row.requested_boards, ["Agentctl", "Nope"]);
    assert_eq!(row.requested_hours, Some(8));
    assert_eq!(row.reason.as_deref(), Some("to sync cards"));

    let (_, _, plain) = start_device_with(&fx, json!({})).await;
    let plain = plain.expect("row");
    assert!(plain.requested_boards.is_empty());
    assert_eq!(plain.requested_hours, None);
    assert_eq!(plain.reason, None);
}

#[tokio::test]
async fn device_request_should_refuse_expires_in_hours_outside_1_to_8760() {
    let (_dir, fx) = fixture();
    for hours in [json!(0), json!(-1), json!(8761), json!(1_000_000)] {
        let (status, body, _) = start_device_with(&fx, json!({"expires_in_hours": hours})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hours}: {body}");
    }
    for hours in [1, 8760] {
        let (status, body, _) = start_device_with(&fx, json!({"expires_in_hours": hours})).await;
        assert_eq!(status, StatusCode::OK, "{hours}: {body}");
    }
}

#[tokio::test]
async fn device_request_should_refuse_more_than_a_day_with_a_delete_permission() {
    let (_dir, fx) = fixture();
    for hours in [25, 8760] {
        let (status, body, _) =
            start_device_with(&fx, json!({"scopes": ["admin"], "expires_in_hours": hours})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hours}: {body}");
    }
}

#[tokio::test]
async fn device_request_should_accept_a_day_with_a_delete_permission_and_longer_without() {
    let (_dir, fx) = fixture();
    for extra in [
        json!({"scopes": ["admin"], "expires_in_hours": 24}),
        json!({"scopes": ["read"], "expires_in_hours": 8760}),
    ] {
        let (status, body, _) = start_device_with(&fx, extra.clone()).await;
        assert_eq!(status, StatusCode::OK, "{extra}: {body}");
    }
}

#[tokio::test]
async fn device_request_should_accept_more_than_a_day_with_links_delete() {
    let (_dir, fx) = fixture();
    let body = json!({
        "client_name": "agentctl on laptop",
        "permissions": ["tasks.read", "links.delete"],
        "expires_in_hours": 2160,
    });
    let (status, started) = start_device(&fx, body).await;
    assert_eq!(status, StatusCode::OK, "{started}");
}

#[tokio::test]
async fn device_request_should_accept_ninety_days_for_the_lead_role_and_preset() {
    let (_dir, fx) = fixture();
    for extra in [
        json!({"role": "lead", "expires_in_hours": 2160}),
        json!({"preset": "lead", "expires_in_hours": 2160}),
    ] {
        let mut body = json!({"client_name": "agentctl on laptop"});
        body.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let (status, started) = start_device(&fx, body).await;
        assert_eq!(status, StatusCode::OK, "{extra}: {started}");
    }
}

#[tokio::test]
async fn device_request_should_refuse_an_overlong_reason_or_too_many_boards() {
    let (_dir, fx) = fixture();
    let long = "r".repeat(201);
    let names: Vec<String> = (0..51).map(|n| format!("board {n}")).collect();
    for extra in [
        json!({"reason": long}),
        json!({"boards": names}),
        json!({"boards": ["b".repeat(201)]}),
    ] {
        let (status, body, _) = start_device_with(&fx, extra.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{extra}: {body}");
    }
    let (status, body, _) = start_device_with(&fx, json!({"reason": "r".repeat(200)})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Approves a device request made with `extra` and `form` and returns the approval status, the page
/// and the whoami of the issued token when it was approved.
async fn approve_asked(extra: Value, form: &str) -> (StatusCode, String, Option<Value>) {
    let (_dir, fx) = fixture();
    let (status, started, _) = start_device_with(&fx, extra).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let device_code = started["device_code"].as_str().expect("device_code");
    let user_code = started["user_code"].as_str().expect("user_code");
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}{form}"),
    )
    .await;
    if status != StatusCode::OK {
        return (status, page, None);
    }
    let (status, body) = poll_device(&fx, device_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = serde_json::from_str::<Value>(&body).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_string();
    let (status, whoami) = request(
        &fx,
        As::Bearer(&token),
        "GET",
        "/api/v1/auth/whoami",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{whoami}");
    (
        status,
        page,
        Some(serde_json::from_str(&whoami).expect("json")),
    )
}

#[tokio::test]
async fn device_approval_should_refuse_an_expiry_longer_than_requested() {
    let extra = json!({"expires_in_hours": 8});
    let (status, page, whoami) = approve_asked(extra.clone(), "&expires_in_hours=9").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(whoami.is_none());
    assert!(
        page.contains("never lengthened beyond the request"),
        "{page}"
    );

    let (status, _, whoami) = approve_asked(extra.clone(), "&expires_in_hours=8").await;
    assert_eq!(status, StatusCode::OK);
    let at = whoami.expect("whoami")["expires_at"]
        .as_i64()
        .expect("expires_at");
    assert!((at - (chrono::Utc::now().timestamp() + 8 * SECONDS_PER_HOUR)).abs() < 60);

    let (_, _, whoami) = approve_asked(extra.clone(), "&expires_in_hours=2").await;
    let at = whoami.expect("whoami")["expires_at"]
        .as_i64()
        .expect("expires_at");
    assert!((at - (chrono::Utc::now().timestamp() + 2 * SECONDS_PER_HOUR)).abs() < 60);

    let (_, _, whoami) = approve_asked(extra, "").await;
    let at = whoami.expect("whoami")["expires_at"]
        .as_i64()
        .expect("expires_at");
    assert!(
        (at - (chrono::Utc::now().timestamp() + 8 * SECONDS_PER_HOUR)).abs() < 60,
        "no field grants the requested hours, below the default"
    );
}

#[tokio::test]
async fn device_page_should_preselect_and_cap_the_expiry_at_the_requested_hours() {
    let (_dir, fx) = fixture();
    for (extra, default, requested) in [
        (json!({"expires_in_hours": 8}), 8, 8),
        (json!({"expires_in_hours": 8760}), 8760, 8760),
        (json!({"scopes": ["admin"], "expires_in_hours": 8}), 8, 8),
        (json!({}), 2160, 0),
        (json!({"scopes": ["admin"]}), 24, 0),
    ] {
        let (status, started, _) = start_device_with(&fx, extra.clone()).await;
        assert_eq!(status, StatusCode::OK, "{started}");
        let user_code = started["user_code"].as_str().expect("user_code");
        let (status, page) =
            form_post(&fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            page.contains(&format!("data-expiry-hours=\"{default}\"")),
            "{extra}: {page}"
        );
        assert!(
            page.contains(&format!("data-requested-hours=\"{requested}\"")),
            "{extra}: {page}"
        );
    }
}

/// Starts a device request with `extra` and returns the confirm page the admin sees for it.
async fn device_page_for(fx: &Fixture, extra: Value) -> String {
    let (status, started, _) = start_device_with(fx, extra).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let user_code = started["user_code"].as_str().expect("user_code");
    let (status, page) =
        form_post(fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page
}

#[tokio::test]
async fn device_page_should_escape_the_reason_and_list_skipped_boards() {
    let (_dir, fx) = fixture();
    let page = device_page_for(
        &fx,
        json!({"reason": "<script>alert(1)</script>", "boards": ["Agentctl", "<b>Nope</b>"], "expires_in_hours": 8}),
    )
    .await;
    assert!(!page.contains("<script>alert(1)</script>"), "{page}");
    assert!(
        page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{page}"
    );
    assert!(page.contains("<li>Agentctl</li>"), "{page}");
    assert!(!page.contains("device-every-board"), "{page}");
    assert!(page.contains("Skipped, not boards of yours:"), "{page}");
    assert!(page.contains("&lt;b&gt;Nope&lt;/b&gt;"), "{page}");
    assert!(!page.contains("<b>Nope</b>"), "{page}");
    assert!(page.contains("After 8 hours"), "{page}");
    assert!(page.contains("data-board-ids=\""), "{page}");
}

#[tokio::test]
async fn device_page_should_offer_no_approval_when_no_named_board_is_the_approvers() {
    let (_dir, fx) = fixture();
    let page = device_page_for(&fx, json!({"boards": ["Nope"]})).await;
    assert!(
        page.contains("None of the boards the request names is a board of yours."),
        "{page}"
    );
    assert!(
        page.contains("Skipped, not boards of yours: Nope."),
        "{page}"
    );
    assert!(!page.contains("btn-primary"), "{page}");
    assert!(!page.contains("device-edit-toggle"), "{page}");
    assert!(!page.contains("device-every-board"), "{page}");
}

#[tokio::test]
async fn device_page_should_highlight_a_request_naming_no_boards() {
    let (_dir, fx) = fixture();
    let page = device_page_for(&fx, json!({})).await;
    assert!(
        page.contains("Every board, including boards made later."),
        "{page}"
    );
    assert!(page.contains("None given"), "{page}");
    assert!(!page.contains("Skipped, not boards of yours:"), "{page}");
}

#[tokio::test]
async fn device_page_should_name_the_preset_of_an_exact_preset_request() {
    let (_dir, fx) = fixture();
    for (extra, scope) in [
        (json!({"scopes": ["work"]}), "work"),
        (json!({"scopes": ["admin"]}), "admin"),
    ] {
        let page = device_page_for(&fx, extra).await;
        assert!(
            page.contains(&format!("the <strong>{scope}</strong> scope")),
            "{page}"
        );
        assert!(!page.contains("with custom permissions"), "{page}");
    }
}

#[tokio::test]
async fn device_page_should_call_a_partial_permission_request_custom() {
    let (_dir, fx) = fixture();
    let (status, started) = start_device(
        &fx,
        json!({"client_name": "agentctl on laptop", "permissions": ["tasks.read"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let user_code = started["user_code"].as_str().expect("user_code");
    let (status, page) =
        form_post(&fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("with custom permissions."), "{page}");
    assert!(!page.contains("the <strong>read</strong> scope"), "{page}");
}

#[tokio::test]
async fn device_approval_should_accept_never_only_when_the_request_names_no_hours() {
    let (status, _, whoami) = approve_asked(json!({}), "&expires_in_hours=never").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(whoami.expect("whoami")["expires_at"], Value::Null);

    for hours in [8, 8760] {
        let (status, page, whoami) = approve_asked(
            json!({"expires_in_hours": hours}),
            "&expires_in_hours=never",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hours}: {page}");
        assert!(whoami.is_none());
    }

    let (_, _, whoami) = approve_asked(json!({}), "").await;
    assert!(whoami.expect("whoami")["expires_at"].is_i64());
}

#[tokio::test]
async fn device_approval_should_skip_unknown_board_names_and_grant_the_rest() {
    let (_dir, fx) = fixture();
    let other = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let (status, started, _) = start_device_with(
        &fx,
        json!({"boards": ["Agentctl", "No such board", "agentctl"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let user_code = started["user_code"].as_str().expect("user_code");
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}&boards={other}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("never widened beyond the request"), "{page}");
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let (status, body) = poll_device(&fx, started["device_code"].as_str().expect("code")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = serde_json::from_str::<Value>(&body).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_string();
    let (_, whoami) = request(
        &fx,
        As::Bearer(&token),
        "GET",
        "/api/v1/auth/whoami",
        None,
        true,
    )
    .await;
    let whoami: Value = serde_json::from_str(&whoami).expect("json");
    let boards = whoami["boards"].as_array().expect("boards");
    assert_eq!(boards.len(), 1, "{whoami}");
    assert_eq!(boards[0]["id"], fx.board_id);
}

#[tokio::test]
async fn device_approval_should_refuse_a_request_whose_board_names_match_none() {
    let (_, page, whoami) = approve_asked(json!({"boards": ["No such board"]}), "").await;
    assert!(whoami.is_none());
    assert!(
        page.contains("None of the boards the request names"),
        "{page}"
    );
}

#[tokio::test]
async fn device_approval_should_grant_every_board_when_the_request_names_none() {
    let (_, _, whoami) = approve_asked(json!({}), "").await;
    assert_eq!(whoami.expect("whoami")["boards"], "all");
    let (_, _, whoami) = approve_asked(json!({"boards": []}), "").await;
    assert_eq!(whoami.expect("whoami")["boards"], "all");
}

/// Returns the approval form fields that grant exactly the permissions of `scope`.
fn scope_form(scope: AuthScope) -> String {
    let names: String = scope
        .permissions()
        .iter()
        .map(|permission| format!("&permissions={}", permission.name()))
        .collect();
    format!("&permissions_listed=1{names}")
}

/// Approves a fresh request for `requested` with `form` fields added, then returns the status and
/// page of the approval and, when it succeeded, the issued token's `whoami`.
async fn approve_with(requested: &str, form: &str) -> (StatusCode, String, Option<Value>) {
    let (_dir, fx) = fixture();
    let (device_code, user_code) = paired(&fx, json!([requested])).await;
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}{form}"),
    )
    .await;
    if status != StatusCode::OK {
        return (status, page, None);
    }
    let (status, body) = poll_device(&fx, &device_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = serde_json::from_str::<Value>(&body).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_string();
    let (status, whoami) = request(
        &fx,
        As::Bearer(&token),
        "GET",
        "/api/v1/auth/whoami",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{whoami}");
    (
        StatusCode::OK,
        page,
        Some(serde_json::from_str(&whoami).expect("json")),
    )
}

#[tokio::test]
async fn approving_should_lower_the_permissions_and_set_the_expiry() {
    let day = SECONDS_PER_DAY;
    let near = |expires: &Value, days: i64| {
        let want = chrono::Utc::now().timestamp() + days * day;
        (expires.as_i64().expect("expires_at") - want).abs() < 60
    };

    let (status, _, whoami) = approve_with(
        "work",
        &format!("{}&expires_in_hours=720", scope_form(AuthScope::Read)),
    )
    .await;
    let whoami = whoami.expect("whoami");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(whoami["scope"], "read");
    assert!(near(&whoami["expires_at"], 30), "{whoami}");

    let (_, _, whoami) = approve_with("work", "").await;
    let whoami = whoami.expect("whoami");
    assert_eq!(
        whoami["scope"], "work",
        "no field keeps the requested scope"
    );
    assert!(near(&whoami["expires_at"], 90), "90 days is the default");

    for (requested, days) in [("provision", 90), ("admin", 1), ("read", 90)] {
        let (_, _, whoami) = approve_with(requested, "").await;
        let whoami = whoami.expect("whoami");
        assert_eq!(whoami["scope"], requested);
        assert!(near(&whoami["expires_at"], days), "{requested}: {whoami}");
    }

    for (requested, lowered, scope, days) in [
        ("admin", AuthScope::Work, "work", 1),
        ("provision", AuthScope::Read, "read", 90),
        ("work", AuthScope::Read, "read", 90),
    ] {
        let form = scope_form(lowered);
        let (_, _, whoami) = approve_with(requested, &form).await;
        let whoami = whoami.expect("whoami");
        assert_eq!(whoami["scope"], scope);
        assert!(
            near(&whoami["expires_at"], days),
            "{requested}{form}: {whoami}"
        );
    }

    let (_, _, whoami) = approve_with("work", "&expires_in_hours=24").await;
    let whoami = whoami.expect("whoami");
    assert!(near(&whoami["expires_at"], 1), "{whoami}");

    let (_, _, whoami) = approve_with(
        "admin",
        &format!("{}&expires_in_hours=8760", scope_form(AuthScope::Work)),
    )
    .await;
    let whoami = whoami.expect("whoami");
    assert_eq!(whoami["scope"], "work");
    assert!(near(&whoami["expires_at"], 365));

    let (_, _, whoami) = approve_with("read", "&expires_in_hours=never").await;
    let whoami = whoami.expect("whoami");
    assert_eq!(whoami["scope"], "read");
    assert_eq!(whoami["expires_at"], Value::Null);

    let (_, _, whoami) = approve_with("read", "&expires_in_hours=1").await;
    let whoami = whoami.expect("whoami");
    let want = chrono::Utc::now().timestamp() + SECONDS_PER_HOUR;
    assert!((whoami["expires_at"].as_i64().expect("expires_at") - want).abs() < 60);
}

#[tokio::test]
async fn approving_should_refuse_raised_permissions_or_an_expiry_outside_1_to_8760_hours() {
    for form in [
        scope_form(AuthScope::Admin),
        scope_form(AuthScope::Work),
        "&expires_in_hours=0".to_string(),
        "&expires_in_hours=-30".to_string(),
        "&expires_in_hours=".to_string(),
        "&expires_in_hours=abc".to_string(),
        "&expires_in_hours=8761".to_string(),
    ] {
        let (status, page, whoami) = approve_with("read", &form).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{form}: {page}");
        assert!(whoami.is_none());
        assert!(
            page.contains("id=\"device-expiry\""),
            "{form}: the form is shown again"
        );
        if form.as_str() == "&expires_in_hours=abc" {
            assert!(
                page.contains("Choose an expiry of 1 to 8760 hours, or never."),
                "{page}"
            );
        }
    }

    let (_dir, fx) = fixture();
    let (device_code, user_code) = paired(&fx, json!(["read"])).await;
    for _ in 0..12 {
        let (status, _) = form_post(
            &fx,
            As::Admin,
            "/device/approve",
            &format!("user_code={user_code}{}", scope_form(AuthScope::Admin)),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a bad choice is not a bad code"
        );
    }
    let (status, _) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}{}", scope_form(AuthScope::Read)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the request is still pending, not locked out"
    );
    let (status, body) = poll_device(&fx, &device_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_approval_page_should_name_the_requested_permissions_and_warn_about_admin() {
    for (requested, warned, hours) in [
        ("read", false, "2160"),
        ("work", false, "2160"),
        ("provision", false, "2160"),
        ("admin", true, "24"),
    ] {
        let (_dir, fx) = fixture();
        let (_, user_code) = paired(&fx, json!([requested])).await;
        let (status, page) =
            form_post(&fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page.contains("device-warning"), warned, "{requested}");
        assert!(
            page.contains(&format!("data-expiry-hours=\"{hours}\"")),
            "{requested}"
        );
        let attribute = page
            .split("data-requested=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("data-requested");
        let names: BTreeSet<&str> = attribute.split(',').collect();
        let expected = AuthScope::parse(requested).expect("scope").permissions();
        assert_eq!(
            names,
            expected
                .iter()
                .map(|permission| permission.name())
                .collect(),
            "{requested}"
        );
        for id in ["device-picker", "device-boards", "device-expiry"] {
            assert!(page.contains(&format!("id=\"{id}\"")), "{id}");
        }
        assert!(!page.contains("name=\"scope\""), "{requested}");
    }
}

fn role_of(fx: &Fixture, name: &str, names: &[&str]) -> AgentRoleRow {
    let permissions = names
        .iter()
        .map(|name| Permission::named(name).expect("permission"))
        .collect();
    fx.db
        .create_agent_role(fx.user_id.as_str(), name, permissions)
        .expect("role")
}

#[tokio::test]
async fn approving_as_a_role_should_issue_a_token_that_carries_the_role_and_its_permissions() {
    let (_dir, fx) = fixture();
    let role = role_of(&fx, "scout", &["tasks.read"]);
    let (device_code, user_code) = paired(&fx, json!(["read"])).await;
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}&role_id={}", role.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("Approved."), "{page}");

    let (status, body) = poll_device(&fx, &device_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let issued: Value = serde_json::from_str(&body).expect("json");
    let mut held: Vec<&str> = issued["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .map(|name| name.as_str().expect("name"))
        .collect();
    held.sort_unstable();
    assert_eq!(held, ["boards.read", "tasks.read"]);

    let record = fx
        .db
        .list_api_tokens()
        .expect("tokens")
        .into_iter()
        .find(|token| token.name == "agentctl on laptop")
        .expect("token");
    assert_eq!(record.role_id.as_deref(), Some(role.id.as_str()));
    assert_eq!(record.role_name.as_deref(), Some("scout"));
    assert_eq!(record.label.as_deref(), Some("agent-1"));
    assert_eq!(
        record.permissions,
        Permission::with_required(&role.permissions)
    );
}

#[tokio::test]
async fn approving_as_a_role_with_chosen_rows_should_keep_the_role_and_drop_the_rest() {
    let (_dir, fx) = fixture();
    let role = role_of(&fx, "scout", &["tasks.read", "tasks.create"]);
    let (device_code, user_code) = paired(&fx, json!(["work"])).await;
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!(
            "user_code={user_code}&permissions_listed=1&permissions=boards.read&permissions=tasks.read&role_id={}",
            role.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let (status, body) = poll_device(&fx, &device_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let record = fx
        .db
        .list_api_tokens()
        .expect("tokens")
        .into_iter()
        .find(|token| token.name == "agentctl on laptop")
        .expect("token");
    assert_eq!(record.role_id.as_deref(), Some(role.id.as_str()));
    let names: BTreeSet<&str> = record.permissions.iter().map(|p| p.name()).collect();
    assert_eq!(names, BTreeSet::from(["boards.read", "tasks.read"]));
}

#[tokio::test]
async fn approving_should_refuse_a_role_or_rows_above_the_request_and_keep_it_pending() {
    let (_dir, fx) = fixture();
    let wide = role_of(&fx, "wide", &["tasks.read", "tasks.delete"]);
    let narrow = role_of(&fx, "narrow", &["tasks.read"]);
    let (device_code, user_code) = paired(&fx, json!(["read"])).await;
    for (form, message) in [
        (
            format!("&role_id={}", wide.id),
            "never raised above the request: it does not ask for tasks.delete",
        ),
        (
            format!(
                "&permissions_listed=1&permissions=tasks.create&role_id={}",
                narrow.id
            ),
            "never raised above the request: it does not ask for tasks.create",
        ),
        (
            "&role_id=no-such-role".to_string(),
            "Choose a role of your account.",
        ),
    ] {
        let (status, page) = form_post(
            &fx,
            As::Admin,
            "/device/approve",
            &format!("user_code={user_code}{form}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{form}: {page}");
        assert!(page.contains(message), "{form}: {page}");
        assert!(page.contains("id=\"device-picker\""), "{form}");
    }
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::PRECONDITION_REQUIRED, "authorization_pending")
    );
}

#[tokio::test]
async fn approving_should_refuse_a_permission_the_chosen_role_gives_no() {
    let (_dir, fx) = fixture();
    let narrow = role_of(&fx, "narrow", &["tasks.read"]);
    let (_, user_code) = paired(&fx, json!(["work"])).await;
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!(
            "user_code={user_code}&permissions_listed=1&permissions=tasks.create&role_id={}",
            narrow.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("The role gives no tasks.create."), "{page}");
}

#[tokio::test]
async fn approving_should_refuse_the_role_of_another_account() {
    let (_dir, fx) = fixture();
    let bob = fx.db.create_user("bob").expect("bob");
    let theirs = fx
        .db
        .create_agent_role(
            bob.as_str(),
            "scout",
            BTreeSet::from([Permission::TasksRead]),
        )
        .expect("role");
    let account = fx.db.account(&fx.user_id);
    let now = chrono::Utc::now().timestamp();
    let row = DeviceAuthRow {
        device_code_hash: "hash".into(),
        user_code: "BCDFGHJK".into(),
        client_name: "limited".into(),
        scope: AuthScope::Read,
        permissions: AuthScope::Read.permissions(),
        boards: BoardAccess::All,
        status: DeviceAuthStatus::Pending,
        created_at: now,
        expires_at: now + 600,
        last_poll_at: None,
        token_expires_at: None,
        account_id: None,
        client_network: None,
        role_id: None,
        requested_role: None,
        requested_boards: Vec::new(),
        requested_hours: None,
        reason: None,
    };
    let form = DeviceApproveForm {
        user_code: "BCDFGHJK".into(),
        role_id: Some(theirs.id),
        ..Default::default()
    };
    let refused = device_grant(&account, &row, &form, now).expect_err("another account's role");
    assert_eq!(refused, "Choose a role of your account.");
}

#[tokio::test]
async fn polling_should_deny_a_request_approved_as_a_role_deleted_since() {
    let (_dir, fx) = fixture();
    let role = role_of(&fx, "scout", &["tasks.read"]);
    let (device_code, user_code) = paired(&fx, json!(["read"])).await;
    let (status, page) = form_post(
        &fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}&role_id={}", role.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    fx.db
        .account(&fx.user_id)
        .delete_agent_role(&role.id)
        .expect("delete");
    let (status, text) = poll_device(&fx, &device_code).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::FORBIDDEN, "access_denied")
    );
    assert!(
        fx.db
            .list_api_tokens()
            .expect("tokens")
            .iter()
            .all(|token| token.name != "agentctl on laptop")
    );
}

#[tokio::test]
async fn device_grant_should_refuse_boards_outside_the_request() {
    let (_dir, fx) = fixture();
    let account = fx.db.account(&fx.user_id);
    let other = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let now = chrono::Utc::now().timestamp();
    let row = DeviceAuthRow {
        device_code_hash: "hash".into(),
        user_code: "BCDFGHJK".into(),
        client_name: "limited".into(),
        scope: AuthScope::Read,
        permissions: AuthScope::Read.permissions(),
        boards: BoardAccess::Only(BTreeSet::from([fx.board_id.clone()])),
        status: DeviceAuthStatus::Pending,
        created_at: now,
        expires_at: now + 600,
        last_poll_at: None,
        token_expires_at: None,
        account_id: None,
        client_network: None,
        role_id: None,
        requested_role: None,
        requested_boards: Vec::new(),
        requested_hours: None,
        reason: None,
    };
    let form = |boards: Option<Vec<String>>| DeviceApproveForm {
        user_code: "BCDFGHJK".into(),
        boards,
        ..Default::default()
    };
    let grant = device_grant(&account, &row, &form(None), now).expect("no field");
    assert_eq!(
        grant.boards,
        BoardAccess::Only(BTreeSet::from([fx.board_id.clone()])),
        "no field keeps the requested boards"
    );
    let refused = device_grant(&account, &row, &form(Some(vec![other])), now).expect_err("outside");
    assert!(
        refused.contains("never widened beyond the request"),
        "{refused}"
    );
    let ids = vec![fx.board_id.clone()];
    let grant = device_grant(&account, &row, &form(Some(ids)), now).expect("inside");
    assert_eq!(
        grant.boards,
        BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]))
    );
}

#[tokio::test]
async fn history_should_name_the_token_that_made_a_change_and_no_name_for_the_cookie() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "claude-code@laptop", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let move_to = json!({"column_id": fx.col_done, "position": 0});

    let (status, text) = request(
        &fx,
        As::Bearer(&work),
        "POST",
        &format!("/api/tasks/{id}/move"),
        Some(move_to),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = request(
        &fx,
        As::Bearer(&work),
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"name": "Renamed"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let back = json!({"column_id": fx.col_todo, "position": 0});
    let (status, _) = call(&fx, "POST", &format!("/api/tasks/{id}/move"), Some(back)).await;
    assert_eq!(status, StatusCode::OK);

    let uri = format!("/api/boards/{}/export-data", fx.board_id);
    let (_, data) = call(&fx, "GET", &uri, None).await;
    let task = data["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|task| task["id"] == id.as_str())
        .expect("task");
    let events = task["history"].as_array().expect("history");
    let named: Vec<_> = events
        .iter()
        .filter_map(|event| Some((event["kind"].as_str()?, event["token_name"].as_str()?)))
        .collect();
    assert_eq!(
        named,
        [
            ("moved", "claude-code@laptop"),
            ("renamed", "claude-code@laptop")
        ]
    );
    for event in events {
        if event["kind"] == "created" || event["actor"] == "admin" {
            assert!(event.get("token_name").is_none(), "{event}");
        }
    }
    assert_eq!(events.last().expect("last")["kind"], "moved");

    let (status, page) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/history-view"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.matches("api-token (claude-code@laptop)").count(), 2);
    assert!(
        page.contains("admin &middot;"),
        "cookie changes show no name"
    );
    assert!(!page.contains("admin ("));
}

#[tokio::test]
async fn every_history_event_should_name_the_token_and_cookie_and_bootstrap_events_none() {
    const BOOTSTRAP: &str = "pomb_bootstrap_for_history_test";
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "claude-code@laptop", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let other = create(&fx, json!({"column_id": fx.col_todo, "name": "Other"})).await;
    let send = |who: As<'static>, method: &'static str, uri: String, body: Value| {
        let fx = &fx;
        async move {
            let (status, text) = request(fx, who, method, &uri, Some(body), true).await;
            assert!(status.is_success(), "{method} {uri}: {status} {text}");
            text
        }
    };
    let token: &'static str = Box::leak(work.into_boxed_str());

    let claim = send(
        As::Bearer(token),
        "POST",
        format!("/api/tasks/{id}/claim"),
        json!({"claimant": "me", "ttl_seconds": 900}),
    )
    .await;
    let fence = serde_json::from_str::<Value>(&claim).expect("json")["fence"].clone();
    for (path, body) in [
        ("comments", json!({"body": "hi"})),
        ("time", json!({"minutes": 5})),
        ("links", json!({"kind": "related", "target_id": other})),
        (
            "attachments",
            json!({"name": "a.txt", "mime": "text/plain", "data": "aGk="}),
        ),
        ("claim/release", json!({"fence": fence})),
    ] {
        send(
            As::Bearer(token),
            "POST",
            format!("/api/tasks/{id}/{path}"),
            body,
        )
        .await;
    }

    let by_cookie = format!("/api/tasks/{other}/comments");
    send(
        As::Admin,
        "POST",
        by_cookie.clone(),
        json!({"body": "browser"}),
    )
    .await;
    let bootstrap_app = router(
        AppState::new(
            fx.db.clone(),
            BootstrapToken::new(BOOTSTRAP),
            passkey_mode(),
        )
        .with_limits(super::server::unthrottled_limits()),
    );
    let response = bootstrap_app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(&by_cookie)
                .header("authorization", format!("Bearer {BOOTSTRAP}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"body": "bootstrap"}).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert!(response.status().is_success());

    let events = |task: &str| -> Vec<(HistoryKind, Option<String>)> {
        let row = fx.db.task(task).expect("read").expect("task");
        row.history
            .into_iter()
            .map(|event| (event.kind, event.token_name))
            .collect()
    };
    let name = Some("claude-code@laptop".to_string());
    let history = events(&id);
    for kind in [
        HistoryKind::Claimed,
        HistoryKind::CommentAdded,
        HistoryKind::TimeLogged,
        HistoryKind::LinkAdded,
        HistoryKind::AttachmentAdded,
        HistoryKind::ClaimReleased,
    ] {
        assert!(
            history.contains(&(kind, name.clone())),
            "{kind:?} lacks the token name: {history:?}"
        );
    }
    for (kind, token) in events(&other) {
        if kind == HistoryKind::CommentAdded {
            assert_eq!(token, None, "cookie and bootstrap comments carry no name");
        }
    }
    assert_eq!(
        events(&other)
            .iter()
            .filter(|(kind, _)| *kind == HistoryKind::CommentAdded)
            .count(),
        2
    );
}

/// Creates a column on the fixture board, moves it to `position`, and returns its id.
async fn add_column(fx: &Fixture, name: &str, position: i64) -> String {
    let body = json!({"board_id": fx.board_id, "name": name});
    let (status, created) = call(fx, "POST", "/api/columns", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().expect("column id").to_string();
    let uri = format!("/api/columns/{id}/move");
    let (status, text) = call(fx, "POST", &uri, Some(json!({"position": position}))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    id
}

/// Creates 5 tasks in each fixture column and returns their ids in board order.
async fn create_in_both_columns(fx: &Fixture) -> Vec<String> {
    let mut created = Vec::new();
    for column in [&fx.col_todo, &fx.col_done] {
        for n in 0..5 {
            let body = json!({"column_id": column, "name": format!("T{n}")});
            created.push(create(fx, body).await);
        }
    }
    created
}

/// Asserts that `first` plus the rest of the walk is exactly `expected`, in order.
async fn assert_walk_continues(
    fx: &Fixture,
    uri: &str,
    first: &[String],
    cursor: String,
    expected: &[String],
) {
    let (rest, _) = walk_from(fx, uri, 3, Some(cursor)).await;
    let mut walked = first.to_vec();
    walked.extend(rest);
    assert_eq!(walked, expected);
}

#[tokio::test]
async fn deleting_an_empty_column_ahead_of_the_cursor_should_repeat_and_skip_nothing() {
    let (_dir, fx) = fixture();
    let ahead = add_column(&fx, "Ahead", 0).await;
    let created = create_in_both_columns(&fx).await;
    let uri = list_uri(&fx, BOARD_TASKS_PATH);
    let (first, next) = page(&fx, &uri, 3, None).await;
    let cursor = next.as_str().expect("cursor").to_string();
    let (status, text) = call(&fx, "DELETE", &format!("/api/columns/{ahead}"), None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_walk_continues(&fx, &uri, &first, cursor, &created).await;
}

#[tokio::test]
async fn inserting_a_column_at_the_front_should_repeat_and_skip_nothing() {
    let (_dir, fx) = fixture();
    let created = create_in_both_columns(&fx).await;
    let uri = list_uri(&fx, BOARD_TASKS_PATH);
    let (first, next) = page(&fx, &uri, 3, None).await;
    let cursor = next.as_str().expect("cursor").to_string();
    add_column(&fx, "Front", 0).await;
    assert_walk_continues(&fx, &uri, &first, cursor, &created).await;
}

#[tokio::test]
async fn deleting_an_empty_lane_should_repeat_and_skip_nothing() {
    let (_dir, fx) = fixture();
    let mut created = Vec::new();
    for n in 0..8 {
        let body =
            json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_b, "name": format!("T{n}")});
        created.push(create(&fx, body).await);
    }
    let uri = list_uri(&fx, BOARD_TASKS_PATH);
    let (first, next) = page(&fx, &uri, 3, None).await;
    let cursor = next.as_str().expect("cursor").to_string();
    let (status, text) = call(
        &fx,
        "DELETE",
        &format!("/api/swimlanes/{}", fx.lane_a),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_walk_continues(&fx, &uri, &first, cursor, &created).await;
}

async fn bearer_json(
    fx: &Fixture,
    token: &str,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, String) {
    request(fx, As::Bearer(token), method, uri, Some(body), true).await
}

fn permission_denied(permission: &str) -> Value {
    json!({
        "code": "permission_denied",
        "message": format!("missing permission: {permission}"),
        "permission": permission,
        "board_id": null,
    })
}

#[tokio::test]
async fn a_provision_token_should_create_and_edit_structure_but_never_delete_it() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);

    let templates = call(&fx, "GET", "/api/templates", None).await.1;
    let template_id = templates[0]["id"]
        .as_str()
        .expect("template id")
        .to_string();
    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        "/api/boards",
        json!({"name": "From template", "template_id": template_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        "/api/boards",
        json!({"name": "Blank"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let board = serde_json::from_str::<Value>(&text).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string();

    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        "/api/columns",
        json!({"board_id": board, "name": "Review"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "column: {text}");
    let column = serde_json::from_str::<Value>(&text).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string();
    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        "/api/swimlanes",
        json!({"board_id": board, "name": "Backend"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "swimlane: {text}");
    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        &format!("/api/boards/{board}/fields"),
        json!({"key": "points", "label": "Points", "kind": "number"}),
    )
    .await;
    assert!(status.is_success(), "field: {status} {text}");

    for (method, uri, permission) in [
        ("DELETE", format!("/api/boards/{board}"), "boards.delete"),
        ("DELETE", format!("/api/columns/{column}"), "columns.delete"),
        (
            "PUT",
            format!("/api/boards/{board}/config"),
            "boards.configure",
        ),
    ] {
        let (status, text) = bearer_json(&fx, &provision, method, &uri, json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied(permission),
            "{method} {uri}"
        );
    }
    let uri = format!("/api/columns/{column}");
    let (status, text) = bearer_json(&fx, &provision, "PATCH", &uri, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        "/api/tasks",
        json!({"column_id": fx.col_todo, "name": "Made by provision"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{text}");
    let (status, text) = request(
        &fx,
        As::Bearer(&provision),
        "GET",
        "/api/v1/auth/whoami",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json")["scope"],
        "provision"
    );
}

#[tokio::test]
async fn a_work_token_should_be_refused_creating_a_board_until_it_holds_provision() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "worker", AuthScope::Work);
    let (status, text) = bearer_json(&fx, &work, "POST", "/api/boards", json!({"name": "B"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        permission_denied("boards.create")
    );
}

#[tokio::test]
async fn the_device_flow_should_request_and_approve_provision_and_lower_admin_to_it() {
    let (status, _, whoami) = approve_with("provision", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(whoami.expect("whoami")["scope"], "provision");

    let (_, _, whoami) = approve_with("admin", &scope_form(AuthScope::Provision)).await;
    assert_eq!(whoami.expect("whoami")["scope"], "provision");

    let (_, _, whoami) = approve_with("provision", &scope_form(AuthScope::Work)).await;
    assert_eq!(whoami.expect("whoami")["scope"], "work");

    let (status, _, _) = approve_with("provision", &scope_form(AuthScope::Admin)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_lead_token_should_set_is_done_on_create_and_on_patch() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        "/api/boards",
        json!({"name": "Blank"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let board = serde_json::from_str::<Value>(&text).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string();

    let mut ids = Vec::new();
    for (name, extra) in [
        ("Absent", json!({})),
        ("Off", json!({"is_done": false})),
        ("On", json!({"is_done": true})),
    ] {
        let mut body = json!({"board_id": board, "name": name});
        body.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let (status, text) = bearer_json(&fx, &provision, "POST", "/api/columns", body).await;
        assert_eq!(status, StatusCode::OK, "{name}: {text}");
        ids.push(
            serde_json::from_str::<Value>(&text).expect("json")["id"]
                .as_str()
                .expect("id")
                .to_string(),
        );
    }

    for value in [json!("true"), json!(1), json!(null)] {
        let body = json!({"board_id": board, "name": "Bad", "is_done": value});
        let (status, text) = bearer_json(&fx, &provision, "POST", "/api/columns", body).await;
        if value.is_null() {
            assert_eq!(status, StatusCode::OK, "{text}");
        } else {
            assert_eq!(status, StatusCode::BAD_REQUEST, "{value}: {text}");
        }
    }

    let (status, columns) = call(&fx, "GET", &format!("/api/boards/{board}/columns"), None).await;
    assert_eq!(status, StatusCode::OK, "{columns}");
    let flag = |id: &str| {
        columns
            .as_array()
            .expect("columns")
            .iter()
            .find(|column| column["id"] == id)
            .expect("column")["is_done"]
            .clone()
    };
    assert_eq!(flag(&ids[0]), false);
    assert_eq!(flag(&ids[1]), false);
    assert_eq!(flag(&ids[2]), true);

    let (status, text) = bearer_json(
        &fx,
        &provision,
        "PATCH",
        &format!("/api/columns/{}", ids[0]),
        json!({"is_done": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (_, columns) = call(&fx, "GET", &format!("/api/boards/{board}/columns"), None).await;
    let patched = columns
        .as_array()
        .expect("columns")
        .iter()
        .find(|column| column["id"] == json!(ids[0]))
        .expect("column");
    assert_eq!(patched["is_done"], true);
}

#[tokio::test]
async fn task_moved_into_a_column_created_done_should_complete() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (status, text) = bearer_json(
        &fx,
        &provision,
        "POST",
        "/api/columns",
        json!({"board_id": fx.board_id, "name": "Closed", "is_done": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let done = serde_json::from_str::<Value>(&text).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string();
    let task = create(&fx, json!({"column_id": fx.col_todo, "name": "Finish"})).await;
    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{task}/move"),
        Some(json!({"column_id": done, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let (status, detail) = call(&fx, "GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail["completed_at"].is_string(), "{detail}");
}

fn checks_table(extra: Value) -> Value {
    let mut body = json!({
        "key": "checks", "label": "Checks", "kind": "table",
        "columns": [
            {"key": "name", "label": "Name", "kind": "text", "required": true},
            {"key": "on", "label": "On", "kind": "select", "options": ["linux", "win"]},
            {"key": "n", "label": "Count", "kind": "number"},
            {"key": "ok", "label": "Passed", "kind": "bool"},
            {"key": "tags", "label": "Tags", "kind": "multiselect", "options": ["a", "b"]},
        ],
    });
    body.as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    body
}

fn stored_field(fx: &Fixture, field: &Value) -> FieldDefRow {
    let id = field["id"].as_str().expect("id");
    fx.db.field(id).expect("read").expect("field")
}

async fn patch_task_fields(fx: &Fixture, task: &str, fields: Value) -> (StatusCode, String) {
    let uri = format!("/api/tasks/{task}");
    request(
        fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"fields": fields})),
        true,
    )
    .await
}

fn quoted_label_table() -> Value {
    json!({
        "key": "checks", "label": "Checks", "kind": "table",
        "columns": [
            {"key": "name", "label": "Say \"hi\"", "kind": "text"},
            {"key": "on", "label": "On", "kind": "select", "options": ["linux", "win"]},
        ],
    })
}

#[tokio::test]
async fn task_modal_should_render_table_columns_and_rows_escaped() {
    let (_dir, fx) = fixture();
    let (status, field) = define(&fx, quoted_label_table()).await;
    assert_eq!(status, StatusCode::OK, "{field}");
    let id = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Modal",
               "fields": {"checks": [{"name": "a\"b", "on": "win"}]}}),
    )
    .await;
    let (status, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/modal"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("data-field-kind=\"table\""));
    assert!(html.contains("class=\"tm-field-table\" data-columns=\""));
    assert!(html.contains("data-columns=\"[{&quot;key&quot;:&quot;name&quot;"));
    assert!(
        html.contains("Say \\&quot;hi\\&quot;"),
        "the quote is escaped"
    );
    assert!(!html.contains("Say \\\"hi\\\""));
    assert!(html.contains(
        "data-rows=\"[{&quot;name&quot;:&quot;a\\&quot;b&quot;,&quot;on&quot;:&quot;win&quot;}]\""
    ));
}

#[tokio::test]
async fn board_settings_should_expose_table_kind_and_columns() {
    let (_dir, fx) = fixture();
    let (status, field) = define(&fx, quoted_label_table()).await;
    assert_eq!(status, StatusCode::OK, "{field}");
    let uri = format!("/b/{}/settings/fields", fx.board_id);
    let (status, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("data-kind=\"table\""));
    assert!(html.contains("data-columns=\"[{&quot;key&quot;:&quot;name&quot;"));
    assert!(html.contains("Say \\&quot;hi\\&quot;"));
    assert!(html.contains("table: Say &quot;hi&quot; · On"));
}

async fn task_fields(fx: &Fixture, task: &str) -> Value {
    let (status, detail) = call(fx, "GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    detail["fields"].clone()
}

#[tokio::test]
async fn table_field_should_be_created_with_columns_and_list_them() {
    let (_dir, fx) = fixture();
    let (status, field) = define(&fx, checks_table(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{field}");
    assert_eq!(field["kind"], "table");
    assert_eq!(field["options"], json!([]));
    assert_eq!(
        field["columns"][0],
        json!({"key": "name", "label": "Name", "kind": "text", "options": [], "required": true})
    );
    assert_eq!(field["columns"][1]["options"], json!(["linux", "win"]));
    assert_eq!(field["columns"][2]["required"], false);

    let (_, other) = define(&fx, json!({"key": "note", "label": "N", "kind": "text"})).await;
    assert_eq!(other["columns"], json!([]), "other kinds carry no columns");
    let (status, list) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/fields", fx.board_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list[0]["columns"].as_array().expect("columns").len(), 5);
}

#[tokio::test]
async fn table_field_definitions_should_reject_malformed_columns() {
    let (_dir, fx) = fixture();
    let table =
        |columns: Value| json!({"key": "t", "label": "T", "kind": "table", "columns": columns});
    for (body, message) in [
        (
            json!({"key": "t", "label": "T", "kind": "table"}),
            "table fields need at least one column",
        ),
        (table(json!([])), "table fields need at least one column"),
        (
            table(json!([{"key": "a", "label": "A", "kind": "text"},
                         {"key": "a", "label": "B", "kind": "number"}])),
            "duplicate column key \"a\"",
        ),
        (
            table(json!([{"key": "Bad Key", "label": "A", "kind": "text"}])),
            "invalid column key",
        ),
        (
            table(json!([{"key": "a", "kind": "text"}])),
            "column \"a\": label is required",
        ),
        (
            table(json!([{"key": "a", "label": "A"}])),
            "column \"a\": kind is required",
        ),
        (
            table(json!([{"key": "a", "label": "A", "kind": "date"}])),
            "column \"a\": unknown column kind; use text, number, bool, select or multiselect",
        ),
        (
            table(json!([{"key": "a", "label": "A", "kind": "multiline"}])),
            "column \"a\": unknown column kind; use text, number, bool, select or multiselect",
        ),
        (
            table(json!([{"key": "a", "label": "A", "kind": "select"}])),
            "column \"a\": select fields need options that are non-empty and unique",
        ),
        (
            table(json!([{"key": "a", "label": "A", "kind": "text", "options": ["x"]}])),
            "column \"a\": options apply only to select and multiselect fields",
        ),
        (
            table(json!([{"key": "a", "label": "A", "kind": "table"}])),
            "column \"a\": a column cannot be a table",
        ),
        (
            json!({"key": "t", "label": "T", "kind": "text",
                   "columns": [{"key": "a", "label": "A", "kind": "text"}]}),
            "columns apply only to table fields",
        ),
        (
            json!({"key": "t", "label": "T", "kind": "table", "options": ["x"],
                   "columns": [{"key": "a", "label": "A", "kind": "text"}]}),
            "options apply only to select and multiselect fields",
        ),
    ] {
        let (status, text) = define_text(&fx, body.clone()).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, message),
            "{body}"
        );
    }
    let (_, list) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/fields", fx.board_id),
        None,
    )
    .await;
    assert_eq!(list, json!([]), "a rejected definition writes nothing");
}

#[tokio::test]
async fn table_values_should_keep_row_order_and_validate_every_cell() {
    let (_dir, fx) = fixture();
    let (status, field) = define(&fx, checks_table(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{field}");
    let rows = json!([
        {"name": "z last alphabetically", "on": "win", "n": 2, "ok": true, "tags": ["b", "a", "b"]},
        {"name": "a first alphabetically"},
        {"name": "m", "n": null},
    ]);
    let task = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "T", "fields": {"checks": rows}}),
    )
    .await;
    assert_eq!(
        task_fields(&fx, &task).await["checks"],
        json!([
            {"name": "z last alphabetically", "on": "win", "n": 2, "ok": true, "tags": ["b", "a"]},
            {"name": "a first alphabetically"},
            {"name": "m"},
        ]),
        "rows keep their order, multiselect cells are deduplicated and null cells dropped"
    );
    let before = task_fields(&fx, &task).await;

    let many = |count: usize| json!(vec![json!({"name": "r"}); count]);
    for (value, message) in [
        (json!("x"), "field \"checks\": expected an array of rows"),
        (
            json!({"name": "x"}),
            "field \"checks\": expected an array of rows",
        ),
        (
            json!([{"name": "a"}, 3]),
            "field \"checks\": rows[1]: expected an object",
        ),
        (
            json!([{"name": "a", "zzz": 1}]),
            "field \"checks\": rows[0].zzz: unknown column",
        ),
        (
            json!([{"name": "a"}, {"name": "b", "n": "3"}]),
            "field \"checks\": rows[1].n: expected a number",
        ),
        (
            json!([{"n": 1}]),
            "field \"checks\": rows[0].name: is required",
        ),
        (
            json!([{"name": null}]),
            "field \"checks\": rows[0].name: is required",
        ),
        (
            json!([{"name": "a"}, {"name": "b", "on": "mac"}]),
            "field \"checks\": rows[1].on: \"mac\" is not an option",
        ),
        (
            json!([{"name": "a", "tags": ["a", "zz"]}]),
            "field \"checks\": rows[0].tags: \"zz\" is not an option",
        ),
        (
            json!([{"name": "a", "ok": "yes"}]),
            "field \"checks\": rows[0].ok: expected true or false",
        ),
        (
            json!([{"name": "two\nlines"}]),
            "field \"checks\": rows[0].name: text must be a single line; use a multiline field",
        ),
        (
            many(201),
            "field \"checks\": at most 200 rows are allowed, got 201",
        ),
    ] {
        let (status, text) = patch_task_fields(&fx, &task, json!({"checks": value})).await;
        assert_eq!((status, text.as_str()), (StatusCode::BAD_REQUEST, message));
        assert_eq!(task_fields(&fx, &task).await, before, "{message}");
    }

    let (status, text) = patch_task_fields(&fx, &task, json!({"checks": many(200)})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        task_fields(&fx, &task).await["checks"]
            .as_array()
            .expect("rows")
            .len(),
        200
    );

    let body = json!({"column_id": fx.col_todo, "name": "Bad", "fields": {"checks": [{"x": 1}]}});
    let (status, text) = request(&fx, As::Admin, "POST", "/api/tasks", Some(body), true).await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "field \"checks\": rows[0].x: unknown column"
        )
    );

    let (status, text) = patch_task_fields(&fx, &task, json!({"checks": null})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        task_fields(&fx, &task)
            .await
            .get("checks")
            .is_none_or(Value::is_null)
    );
}

#[tokio::test]
async fn export_data_should_keep_table_rows_as_arrays() {
    let (_dir, fx) = fixture();
    define(&fx, checks_table(json!({}))).await;
    let rows = json!([{"name": "second"}, {"name": "first", "on": "linux"}]);
    create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "T", "fields": {"checks": rows}}),
    )
    .await;
    let (status, data) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/export-data", fx.board_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{data}");
    assert_eq!(data["tasks"][0]["fields"]["checks"], rows);
}

async fn provision_patch(
    fx: &Fixture,
    token: &str,
    field: &Value,
    body: Value,
) -> (StatusCode, String) {
    let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    bearer_json(fx, token, "PATCH", &uri, body).await
}

#[tokio::test]
async fn a_lead_token_should_need_remove_option_to_drop_an_option_or_column() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (_, table) = define(&fx, checks_table(json!({}))).await;
    let (_, select) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["a", "b"]}),
    )
    .await;
    let columns = table["columns"].clone();
    let with = |index: usize, change: Value| {
        let mut next = columns.clone();
        next[index]
            .as_object_mut()
            .expect("column")
            .extend(change.as_object().expect("object").clone());
        next
    };

    let (status, text) = provision_patch(
        &fx,
        &provision,
        &select,
        json!({"options": ["a", "b", "c"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = provision_patch(
        &fx,
        &provision,
        &select,
        json!({"options": ["a", "x", "b", "c"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let mut grown = with(1, json!({"options": ["linux", "win", "mac"]}));
    grown.as_array_mut().expect("columns").push(
        json!({"key": "extra", "label": "Extra", "kind": "text", "options": [], "required": false}),
    );
    let (status, text) = provision_patch(&fx, &provision, &table, json!({"columns": grown})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let patched: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(patched["columns"].as_array().expect("columns").len(), 6);
    assert_eq!(
        patched["columns"][1]["options"],
        json!(["linux", "win", "mac"])
    );

    let stored = |id: &Value| stored_field(&fx, &json!({ "id": id }));
    let table_before = serde_json::to_value(stored(&table["id"]).columns).expect("json");
    let select_before = stored(&select["id"]).options;
    let mut dropped = without_column(&table_before, 0);
    dropped.as_array_mut().expect("columns").push(
        json!({"key": "n2", "label": "N", "kind": "number", "options": [], "required": false}),
    );
    let renamed = {
        let mut next = table_before.clone();
        next[2]["key"] = json!("renamed");
        next
    };
    let mut relabeled = table_before.clone();
    relabeled[2]["label"] = json!("Other");
    let mut required = table_before.clone();
    required[2]["required"] = json!(true);
    let mut fewer_options = table_before.clone();
    fewer_options[1]["options"] = json!(["linux"]);
    let mut kind_changed = table_before.clone();
    kind_changed[2]["kind"] = json!("text");

    for (field, body) in [
        (&select, json!({"options": ["a", "c"]})),
        (&select, json!({"options": ["c", "x", "a", "b"]})),
        (&table, json!({"columns": dropped})),
        (&table, json!({"columns": renamed})),
        (&table, json!({"columns": relabeled})),
        (&table, json!({"columns": required})),
        (&table, json!({"columns": fewer_options})),
        (&table, json!({"columns": kind_changed})),
    ] {
        let (status, text) = provision_patch(&fx, &provision, field, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied("fields.remove_option"),
            "{body}"
        );
    }
    assert_eq!(
        stored(&select["id"]).options,
        select_before,
        "a refusal writes nothing"
    );
    assert_eq!(
        serde_json::to_value(stored(&table["id"]).columns).expect("json"),
        table_before
    );

    for (field, body) in [
        (&select, json!({"label": "Renamed"})),
        (&select, json!({"required": true})),
        (&select, json!({"position": 9})),
        (&select, json!({"default": "a"})),
    ] {
        let (status, text) = provision_patch(&fx, &provision, field, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}: {text}");
    }
    assert_eq!(stored(&select["id"]).label, "Renamed");
    for (name, message) in [
        ("key", "key cannot be changed"),
        ("owner", "owner cannot be changed"),
    ] {
        let body = json!({ name: "other" });
        let (status, text) = provision_patch(&fx, &provision, &select, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        assert!(text.contains(message), "{text}");
    }

    let (status, text) = call(
        &fx,
        "PATCH",
        &format!("/api/fields/{}", table["id"].as_str().expect("id")),
        Some(json!({"columns": renamed})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin may rename: {text}");
}

fn without_column(columns: &Value, index: usize) -> Value {
    let mut next = columns.clone();
    next.as_array_mut().expect("columns").remove(index);
    next
}

#[tokio::test]
async fn multiline_field_should_convert_to_a_table_with_lines_as_rows() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (_, field) = define(
        &fx,
        json!({"key": "checks", "label": "Checks", "kind": "multiline", "default": "x\n\n y "}),
    )
    .await;
    let lines = create(&fx, json!({"column_id": fx.col_todo, "name": "Lines", "fields": {"checks": "  cargo test \r\n\n\ncargo clippy\n   \nfmt"}})).await;
    let empty = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Empty", "fields": {"checks": " \n \n"}}),
    )
    .await;
    let defaulted = create(&fx, json!({"column_id": fx.col_todo, "name": "Defaulted"})).await;
    let columns = json!([
        {"key": "on", "label": "On", "kind": "select", "options": ["linux"]},
        {"key": "name", "label": "Name", "kind": "text", "required": true},
        {"key": "note", "label": "Note", "kind": "text"},
    ]);
    let (status, text) = provision_patch(
        &fx,
        &provision,
        &field,
        json!({"kind": "table", "columns": columns}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "user-owned field under provision: {text}"
    );
    let converted: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(converted["kind"], "table");
    assert_eq!(converted["default"], json!([{"name": "x"}, {"name": "y"}]));
    assert_eq!(
        task_fields(&fx, &lines).await["checks"],
        json!([{"name": "cargo test"}, {"name": "cargo clippy"}, {"name": "fmt"}])
    );
    assert!(task_fields(&fx, &empty).await.get("checks").is_none());
    assert_eq!(
        task_fields(&fx, &defaulted).await["checks"],
        json!([{"name": "x"}, {"name": "y"}]),
        "a task that took the default holds it as rows"
    );

    let (status, text) = call(
        &fx,
        "PATCH",
        &format!("/api/fields/{}", field["id"].as_str().expect("id")),
        Some(json!({"kind": "multiline"})),
    )
    .await;
    assert_eq!(
        (status, text.to_string().contains("kind cannot be changed")),
        (StatusCode::BAD_REQUEST, true),
        "no way back: {text}"
    );
}

#[tokio::test]
async fn converting_to_a_table_should_change_nothing_when_a_row_fails() {
    let (_dir, fx) = fixture();
    let (_, field) = define(
        &fx,
        json!({"key": "checks", "label": "Checks", "kind": "multiline"}),
    )
    .await;
    let good = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Good", "fields": {"checks": "one\ntwo"}}),
    )
    .await;
    let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    let rows = (0..201)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let long = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Long", "fields": {"checks": rows}}),
    )
    .await;

    let columns = json!([{"key": "name", "label": "Name", "kind": "text"}]);
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"kind": "table", "columns": columns})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            format!(
                "task \"Long\" ({long}): field \"checks\": at most 200 rows are allowed, got 201"
            )
            .as_str()
        )
    );
    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{long}"), None).await;
    assert_eq!(status, StatusCode::OK);

    let needs_other = json!([
        {"key": "name", "label": "Name", "kind": "text"},
        {"key": "must", "label": "Must", "kind": "number", "required": true},
    ]);
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"kind": "table", "columns": needs_other})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        text.starts_with("task \"") && text.contains("field \"checks\": rows[0].must: is required"),
        "{text}"
    );

    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"kind": "table", "columns": [{"key": "n", "label": "N", "kind": "number"}]})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "a table needs a text column to hold the lines"
        )
    );

    assert_eq!(
        task_fields(&fx, &good).await["checks"],
        "one\ntwo",
        "values stay text"
    );
    let stored = stored_field(&fx, &field);
    assert_eq!(
        (stored.kind, stored.columns.len()),
        (FieldKind::Multiline, 0)
    );
}

#[tokio::test]
async fn multiline_field_should_convert_to_a_multiselect_that_keeps_every_line() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (_, field) = define(&fx, json!({"key": "areas", "label": "Areas", "kind": "multiline", "owner": "system", "default": "db"})).await;
    let first = create(&fx, json!({"column_id": fx.col_todo, "name": "First", "fields": {"areas": "ui\n api \nui\n\ndb"}})).await;
    let second = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Second", "fields": {"areas": "ops\nui"}}),
    )
    .await;
    let empty = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Empty", "fields": {"areas": "  "}}),
    )
    .await;
    let (status, text) = provision_patch(
        &fx,
        &provision,
        &field,
        json!({"kind": "multiselect", "options": ["api", "keep"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let converted: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(converted["kind"], "multiselect");
    assert_eq!(
        converted["options"],
        json!(["api", "keep", "ui", "db", "ops"])
    );
    assert_eq!(converted["default"], json!(["db"]));
    assert_eq!(
        task_fields(&fx, &first).await["areas"],
        json!(["ui", "api", "db"])
    );
    assert_eq!(
        task_fields(&fx, &second).await["areas"],
        json!(["ops", "ui"])
    );
    assert!(task_fields(&fx, &empty).await.get("areas").is_none());
}

#[tokio::test]
async fn kind_changes_other_than_the_two_conversions_should_stay_refused() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (_, multiline) = define(&fx, json!({"key": "m", "label": "M", "kind": "multiline"})).await;
    let (_, text_field) = define(&fx, json!({"key": "t", "label": "T", "kind": "text"})).await;
    let (_, table) = define(&fx, checks_table(json!({}))).await;
    for (field, kind) in [
        (&multiline, "select"),
        (&multiline, "text"),
        (&text_field, "table"),
        (&text_field, "multiselect"),
        (&table, "multiline"),
        (&table, "multiselect"),
    ] {
        let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
        let body = json!({"kind": kind});
        let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body.clone()), true).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, "kind cannot be changed"),
            "{body}"
        );
        let (status, text) = provision_patch(&fx, &provision, field, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(text.contains("kind cannot be changed"), "{body}: {text}");
    }
}

#[tokio::test]
async fn provision_should_append_only_optional_columns_after_the_existing_ones() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (_, table) = define(&fx, checks_table(json!({}))).await;
    let columns = table["columns"].as_array().expect("columns").clone();
    let extra =
        json!({"key": "extra", "label": "Extra", "kind": "text", "options": [], "required": false});
    let mut required = extra.clone();
    required["required"] = json!(true);
    let with = |at: usize, column: &Value| {
        let mut next = columns.clone();
        next.insert(at, column.clone());
        json!(next)
    };
    let mut swapped = columns.clone();
    swapped.swap(0, 1);

    for (name, change) in [
        ("inserted mid-list", with(2, &extra)),
        ("required column", with(columns.len(), &required)),
        ("reordered", json!(swapped)),
    ] {
        let (status, text) =
            provision_patch(&fx, &provision, &table, json!({"columns": change})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {text}");
    }
    let stored = stored_field(&fx, &table);
    assert_eq!(
        stored.columns.len(),
        columns.len(),
        "refusals write nothing"
    );

    let (status, text) = provision_patch(
        &fx,
        &provision,
        &table,
        json!({"columns": with(columns.len(), &extra)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn a_lead_token_should_convert_a_field_and_send_a_default() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (_, field) = define(
        &fx,
        json!({"key": "checks", "label": "C", "kind": "multiline"}),
    )
    .await;
    let body = json!({"kind": "table", "default": null,
        "columns": [{"key": "name", "label": "N", "kind": "text"}]});
    let (status, text) = provision_patch(&fx, &provision, &field, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(stored_field(&fx, &field).kind, FieldKind::Table);
}

#[tokio::test]
async fn a_second_conversion_should_be_refused_and_keep_the_rows() {
    let (_dir, fx) = fixture();
    let (_, field) = define(
        &fx,
        json!({"key": "checks", "label": "C", "kind": "multiline"}),
    )
    .await;
    let task = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "T", "fields": {"checks": "one\ntwo"}}),
    )
    .await;
    let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    let body = json!({"kind": "table", "columns": [{"key": "name", "label": "N", "kind": "text"}]});
    let (status, text) = request(&fx, As::Admin, "PATCH", &uri, Some(body.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let rows = task_fields(&fx, &task).await["checks"].clone();
    assert_eq!(rows, json!([{"name": "one"}, {"name": "two"}]));

    let (status, _) = request(&fx, As::Admin, "PATCH", &uri, Some(body), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(task_fields(&fx, &task).await["checks"], rows);
}

#[tokio::test]
async fn a_failed_multiselect_conversion_should_change_nothing() {
    let (_dir, fx) = fixture();
    let (_, field) = define(
        &fx,
        json!({"key": "areas", "label": "A", "kind": "multiline"}),
    )
    .await;
    let task = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "T", "fields": {"areas": "ui\napi"}}),
    )
    .await;
    let uri = format!("/api/fields/{}", field["id"].as_str().expect("id"));
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"kind": "multiselect", "options": ["x", "x"]})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "select fields need options that are non-empty and unique"
        )
    );
    assert_eq!(task_fields(&fx, &task).await["areas"], "ui\napi");
    let stored = stored_field(&fx, &field);
    assert_eq!(
        (stored.kind, stored.options),
        (FieldKind::Multiline, Vec::new())
    );

    let empty = create(&fx, json!({"column_id": fx.col_todo, "name": "E"})).await;
    let (_, blank) = define(
        &fx,
        json!({"key": "blank", "label": "B", "kind": "multiline"}),
    )
    .await;
    let uri = format!("/api/fields/{}", blank["id"].as_str().expect("id"));
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"kind": "multiselect"})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "field \"blank\": no task has a line to become an option; send options"
        )
    );
    assert!(task_fields(&fx, &empty).await.get("blank").is_none());
}

#[tokio::test]
async fn a_lead_token_should_get_the_permission_refusal_before_a_validation_error() {
    let (_dir, fx) = fixture();
    let provision = token_with(&fx, "lead", AuthScope::Provision);
    let (_, field) = define(
        &fx,
        json!({"key": "stage", "label": "Stage", "kind": "select", "options": ["a", "b"]}),
    )
    .await;
    for body in [
        json!({"options": ["a"], "default": 5}),
        json!({"options": ["a", ""]}),
    ] {
        let (status, text) = provision_patch(&fx, &provision, &field, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}: {text}");
    }
    assert_eq!(stored_field(&fx, &field).options, ["a", "b"]);
}

#[test]
fn planning_a_conversion_should_refuse_a_field_that_is_no_longer_multiline() {
    let stored = FieldDefRow {
        id: "f".to_string(),
        board_id: "b".to_string(),
        key: "checks".to_string(),
        label: "Checks".to_string(),
        kind: FieldKind::Table,
        options: Vec::new(),
        columns: Vec::new(),
        default: None,
        required: false,
        owner: FieldOwner::User,
        position: 0,
        created_at: String::new(),
    };
    let input: super::UpdateFieldInput = serde_json::from_value(json!({
        "kind": "table",
        "columns": [{"key": "name", "label": "N", "kind": "text"}],
    }))
    .expect("input");
    let change = super::KindChange::Convert(super::Conversion::Table);
    let error = super::plan_field_update(&stored, &input, None, change, &mut [])
        .expect_err("a table cannot convert again");
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(error.message, "field \"checks\": is no longer multiline");
}

#[tokio::test]
async fn a_comment_author_label_should_be_capped_at_80_characters() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Authors"})).await;
    let uri = format!("/api/tasks/{id}/comments");

    let padded = format!("  {}  ", "a".repeat(80));
    let (status, comment) = call(
        &fx,
        "POST",
        &uri,
        Some(json!({"body": "x", "author": padded})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(comment["author"], "a".repeat(80));

    let before = detail(&fx, &id).await;
    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"body": "x", "author": "a".repeat(81)})),
        true,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "comment author must be at most 80 characters"
        )
    );
    assert_eq!(detail(&fx, &id).await, before);
}

#[tokio::test]
async fn a_comment_author_label_should_not_match_the_accounts_username() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Names"})).await;
    let uri = format!("/api/tasks/{id}/comments");

    let before = detail(&fx, &id).await;
    for label in ["admin", " Admin "] {
        let (status, text) = request(
            &fx,
            As::Admin,
            "POST",
            &uri,
            Some(json!({"body": "x", "author": label})),
            true,
        )
        .await;
        assert_eq!(
            (status, text.as_str()),
            (
                StatusCode::BAD_REQUEST,
                "comment author must not match your username"
            )
        );
    }
    assert_eq!(detail(&fx, &id).await, before);

    let (status, comment) = call(
        &fx,
        "POST",
        &uri,
        Some(json!({"body": "x", "author": "implementer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(comment["author"], "implementer");
}

#[tokio::test]
async fn the_modal_should_label_api_comments_and_offer_a_kind_filter() {
    let (_dir, fx) = fixture();
    let (record, bot) = new_api_token_record(&fx.user_id, "ops<x", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Labels"})).await;
    let uri = format!("/api/tasks/{id}/comments");
    let agent = json!({"body": "ok", "author": "reviewer · glm", "kind": "review"});
    request(&fx, As::Bearer(&bot), "POST", &uri, Some(agent), true).await;
    let odd = json!({"body": "odd", "kind": "a\"b<c"});
    request(&fx, As::Bearer(&bot), "POST", &uri, Some(odd), true).await;
    call(&fx, "POST", &uri, Some(json!({"body": "human"}))).await;

    let modal = format!("/api/tasks/{id}/modal");
    let (_, html) = request(&fx, As::Admin, "GET", &modal, None, false).await;
    assert!(
        !html.contains("ops<x") && !html.contains("a\"b<c"),
        "raw values must not appear"
    );
    assert_eq!(
        html.matches("class=\"tm-comment-via\"").count(),
        2,
        "only token comments"
    );
    assert!(html.contains("via ops&lt;x</small>"), "{html}");
    for attr in ["data-comment-filter", "data-comment-kind"] {
        for kind in ["review", "a&quot;b&lt;c"] {
            assert!(
                html.contains(&format!("{attr}=\"{kind}\"")),
                "{attr} {kind}"
            );
        }
    }
    assert!(
        html.contains("data-comment-filter=\"\""),
        "the untagged chip"
    );
    assert!(html.contains(">untagged</button>"));
    assert_eq!(html.matches("aria-pressed=\"true\"").count(), 3);
    assert_eq!(
        html.matches("class=\"tm-comment tm-comment-agent\"")
            .count(),
        2,
        "only token comments"
    );
}

#[tokio::test]
async fn the_modal_comment_menu_should_list_edit_above_delete() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Menu"})).await;
    let uri = format!("/api/tasks/{id}/comments");
    call(&fx, "POST", &uri, Some(json!({"body": "human"}))).await;

    let modal = format!("/api/tasks/{id}/modal");
    let (_, html) = request(&fx, As::Admin, "GET", &modal, None, false).await;
    let edit = html.find("data-comment-edit=\"").expect("edit button");
    let delete = html.find("data-comment-delete=\"").expect("delete button");
    assert!(edit < delete, "Edit must come above Delete");
    assert!(html.contains(">Edit</button>"));
}

fn token_record(fx: &Fixture, name: &str, scope: AuthScope) -> (String, String) {
    let (record, raw) = new_api_token_record(&fx.user_id, name, scope, None);
    fx.db.create_api_token(&record).expect("token");
    (record.id, raw)
}

async fn board_by(fx: &Fixture, token: &str, name: &str) -> String {
    let (status, text) =
        bearer_json(fx, token, "POST", "/api/boards", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str::<Value>(&text).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string()
}

async fn colors_of(fx: &Fixture, board: &str) -> Value {
    let (status, colors) = call(fx, "GET", &format!("/api/boards/{board}/colors"), None).await;
    assert_eq!(status, StatusCode::OK, "{colors}");
    colors
}

async fn column_ids_of(fx: &Fixture, board: &str) -> Vec<String> {
    let (status, columns) = call(fx, "GET", &format!("/api/boards/{board}/columns"), None).await;
    assert_eq!(status, StatusCode::OK, "{columns}");
    ids(&columns).into_iter().map(str::to_string).collect()
}

/// Mints a token of `scope` assigned only to `boards`, and returns its id and raw value.
fn assigned_token(fx: &Fixture, scope: AuthScope, boards: &[&str]) -> (String, String) {
    let (mut record, raw) = new_api_token_record(&fx.user_id, "assigned", scope, None);
    record.boards = BoardAccess::Only(boards.iter().map(ToString::to_string).collect());
    fx.db.create_api_token(&record).expect("token");
    (record.id, raw)
}

/// Returns the set a token holds, read back from the database.
fn token_boards(fx: &Fixture, id: &str) -> BoardAccess {
    fx.db
        .list_account_api_tokens(&fx.user_id)
        .expect("tokens")
        .into_iter()
        .find(|token| token.id == id)
        .expect("token")
        .boards
}

/// Seeds a second board with a column and a task, and returns all three ids.
fn other_board(fx: &Fixture) -> (String, String, String) {
    let board = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let column = fx.db.create_column(&board, "Todo", None).expect("column");
    let task = fx
        .db
        .create_task(&column, None, "Far task", 1, None)
        .expect("task");
    (board, column, task)
}

fn board_denied(permission: &str, board: &str) -> Value {
    json!({
        "code": "permission_denied",
        "message": format!("token is not assigned to board {board}"),
        "permission": permission,
        "board_id": board,
    })
}

async fn assert_board_denied(answer: (StatusCode, String), permission: &str, board: &str) {
    assert_eq!(answer.0, StatusCode::FORBIDDEN, "{}", answer.1);
    let body: Value = serde_json::from_str(&answer.1).expect("json");
    assert_eq!(body, board_denied(permission, board));
}

#[tokio::test]
async fn a_board_should_join_the_set_of_the_token_that_created_it() {
    let (_dir, fx) = fixture();
    let (id, lead) = assigned_token(&fx, AuthScope::Provision, &[&fx.board_id]);
    let holds = |board: &str| match token_boards(&fx, &id) {
        BoardAccess::Only(ids) => ids.contains(board),
        BoardAccess::All => panic!("the token gained every board"),
    };

    let blank = board_by(&fx, &lead, "Blank").await;
    assert!(holds(&blank));
    let (status, text) = request(
        &fx,
        As::Bearer(&lead),
        "GET",
        &format!("/api/boards/{blank}/columns"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let templates = call(&fx, "GET", "/api/templates", None).await.1;
    let template_id = templates[0]["id"].as_str().expect("template id");
    let (status, text) = bearer_json(
        &fx,
        &lead,
        "POST",
        "/api/boards",
        json!({"name": "From template", "template_id": template_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let body: Value = serde_json::from_str(&text).expect("json");
    assert!(holds(body["id"].as_str().expect("id")));
}

#[tokio::test]
async fn a_board_made_any_other_way_should_stay_outside_the_set() {
    let (_dir, fx) = fixture();
    let (id, lead) = assigned_token(&fx, AuthScope::Provision, &[&fx.board_id]);
    let (_, other_lead) = token_record(&fx, "other", AuthScope::Provision);
    let (_, created) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Browser"}))).await;
    let browser = created["id"].as_str().expect("id").to_string();
    let by_token = board_by(&fx, &other_lead, "Token").await;

    let held = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    assert_eq!(token_boards(&fx, &id), held);
    for board in [browser, by_token] {
        let answer = request(
            &fx,
            As::Bearer(&lead),
            "GET",
            &format!("/api/boards/{board}/columns"),
            None,
            true,
        )
        .await;
        assert_board_denied(answer, "boards.read", &board).await;
    }
}

#[tokio::test]
async fn deleting_a_board_should_remove_it_from_every_token_set() {
    let (_dir, fx) = fixture();
    let (far, _, _) = other_board(&fx);
    let (kept, _) = assigned_token(&fx, AuthScope::Read, &[&fx.board_id, &far]);
    let (emptied, emptied_raw) = assigned_token(&fx, AuthScope::Read, &[&far]);
    let (all, _) = token_record(&fx, "all", AuthScope::Read);

    let (status, body) = call(&fx, "DELETE", &format!("/api/boards/{far}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        token_boards(&fx, &kept),
        BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]))
    );
    assert_eq!(
        token_boards(&fx, &emptied),
        BoardAccess::Only(BTreeSet::new()),
        "an emptied set stays empty and never means every board"
    );
    assert_eq!(token_boards(&fx, &all), BoardAccess::All);
    let (status, text) = request(
        &fx,
        As::Bearer(&emptied_raw),
        "GET",
        "/api/boards",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        json!([])
    );
}

#[tokio::test]
async fn a_token_without_the_permission_should_get_a_null_board_id_on_any_board() {
    let (_dir, fx) = fixture();
    let (far, _, _) = other_board(&fx);
    let (_, work) = assigned_token(&fx, AuthScope::Work, &[&fx.board_id]);
    for board in [&fx.board_id, &far] {
        let answer = request(
            &fx,
            As::Bearer(&work),
            "DELETE",
            &format!("/api/boards/{board}"),
            None,
            true,
        )
        .await;
        assert_eq!(answer.0, StatusCode::FORBIDDEN, "{}", answer.1);
        let body: Value = serde_json::from_str(&answer.1).expect("json");
        assert_eq!(body, permission_denied("boards.delete"));
    }
}

#[tokio::test]
async fn a_cross_board_move_should_need_both_boards_in_the_set() {
    let (_dir, fx) = fixture();
    let (far, far_column, far_task) = other_board(&fx);
    let near_task = create(&fx, json!({"column_id": fx.col_todo, "name": "Near"})).await;
    let (_, work) = assigned_token(&fx, AuthScope::Work, &[&fx.board_id]);

    let answer = bearer_json(
        &fx,
        &work,
        "POST",
        &format!("/api/tasks/{near_task}/move"),
        json!({"column_id": far_column, "position": 0}),
    )
    .await;
    assert_board_denied(answer, "tasks.move", &far).await;

    let answer = bearer_json(
        &fx,
        &work,
        "POST",
        &format!("/api/tasks/{far_task}/move"),
        json!({"column_id": fx.col_todo, "position": 0}),
    )
    .await;
    assert_board_denied(answer, "tasks.move", &far).await;

    let (_, both) = assigned_token(&fx, AuthScope::Work, &[&fx.board_id, &far]);
    let (status, text) = bearer_json(
        &fx,
        &both,
        "POST",
        &format!("/api/tasks/{near_task}/move"),
        json!({"column_id": far_column, "position": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn a_timer_on_a_board_outside_the_set_should_read_idle_and_refuse_every_write() {
    let (_dir, fx) = fixture();
    let (far, _, far_task) = other_board(&fx);
    let near_task = create(&fx, json!({"column_id": fx.col_todo, "name": "Near"})).await;
    let (_, work) = assigned_token(&fx, AuthScope::Work, &[&fx.board_id]);
    let (status, body) = call(
        &fx,
        "POST",
        "/api/timer/start",
        Some(json!({"task_id": far_task, "mode": "stopwatch"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, text) = request(
        &fx,
        As::Bearer(&work),
        "GET",
        "/api/timer/status",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let view: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(view["active"], false, "{text}");
    assert!(!text.contains(&far_task), "{text}");
    assert!(!text.contains("Far task"), "{text}");

    for (uri, permission, body) in [
        (
            "/api/timer/start",
            "time.write",
            json!({"task_id": near_task, "mode": "pomodoro"}),
        ),
        ("/api/timer/stop", "time.write", json!({})),
        (
            "/api/timer/retarget",
            "time.write",
            json!({"task_id": near_task}),
        ),
    ] {
        let answer = bearer_json(&fx, &work, "POST", uri, body).await;
        assert_board_denied(answer, permission, &far).await;
    }
    let (_, view) = call(&fx, "GET", "/api/timer/status", None).await;
    assert_eq!(view["task_id"], far_task.as_str(), "the timer kept running");
}

#[tokio::test]
async fn a_link_that_reaches_outside_the_set_should_stay_hidden_and_refuse_every_write() {
    let (_dir, fx) = fixture();
    let (far, _, far_task) = other_board(&fx);
    make_epic(&fx.db, &[&far_task]);
    let near_task = create(&fx, json!({"column_id": fx.col_todo, "name": "Near"})).await;
    let next_task = create(&fx, json!({"column_id": fx.col_todo, "name": "Next"})).await;
    let (status, link) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{near_task}/links"),
        Some(json!({"kind": "parent", "target_id": far_task})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{link}");
    let link_id = link["id"].as_str().expect("id");
    let (_, admin) = assigned_token(&fx, AuthScope::Admin, &[&fx.board_id]);

    let (status, text) = request(
        &fx,
        As::Bearer(&admin),
        "GET",
        &format!("/api/tasks/{near_task}/links"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        json!([])
    );

    let answer = request(
        &fx,
        As::Bearer(&admin),
        "DELETE",
        &format!("/api/tasks/{near_task}/links/{link_id}"),
        None,
        true,
    )
    .await;
    assert_board_denied(answer, "links.write", &far).await;

    let answer = bearer_json(
        &fx,
        &admin,
        "PUT",
        &format!("/api/tasks/{near_task}/umbrella"),
        json!({"target_id": next_task}),
    )
    .await;
    assert_board_denied(answer, "links.delete", &far).await;

    let (_, links) = call(&fx, "GET", &format!("/api/tasks/{near_task}/links"), None).await;
    assert_eq!(links[0]["target_id"], far_task.as_str(), "the link stayed");
}

#[tokio::test]
async fn a_token_with_a_set_should_keep_time_without_a_task() {
    let (_dir, fx) = fixture();
    let (_, read) = assigned_token(&fx, AuthScope::Read, &[&fx.board_id]);
    let (status, body) = call(
        &fx,
        "POST",
        "/api/timer/start",
        Some(json!({"mode": "stopwatch"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, text) = request(
        &fx,
        As::Bearer(&read),
        "GET",
        "/api/timer/status",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let view: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(view["active"], true, "{text}");
}

async fn whoami_as(fx: &Fixture, token: &str) -> Value {
    let uri = "/api/v1/auth/whoami";
    let (status, text) = request(fx, As::Bearer(token), "GET", uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).expect("json")
}

#[tokio::test]
async fn whoami_should_list_each_assigned_board_with_its_name() {
    let (_dir, fx) = fixture();
    let (far, _, _) = other_board(&fx);
    fx.db
        .create_board(&fx.user_id, "Unassigned")
        .expect("board");
    let (_, token) = assigned_token(&fx, AuthScope::Read, &[&fx.board_id, &far]);

    let view = whoami_as(&fx, &token).await;
    let mut boards = view["boards"].as_array().expect("boards").clone();
    boards.sort_by_key(|board| board["name"].as_str().expect("name").to_string());
    assert_eq!(
        boards,
        [
            json!({"id": fx.board_id, "name": "Agentctl"}),
            json!({"id": far, "name": "Other"}),
        ]
    );
}

#[tokio::test]
async fn whoami_should_show_all_for_the_session_and_every_board_tokens() {
    let (_dir, fx) = fixture();
    let (_, session) = call(&fx, "GET", "/api/v1/auth/whoami", None).await;
    assert_eq!(session["boards"], "all");
    assert_eq!(
        session["permissions"].as_array().expect("list").len(),
        Permission::ALL.len()
    );
    assert_eq!(whoami_as(&fx, &fx.read_token).await["boards"], "all");

    let state = AppState::new(
        fx.db.clone(),
        BootstrapToken::new("pomb_whoami_grant_bootstrap"),
        passkey_mode(),
    );
    let response = router(state)
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/v1/auth/whoami")
                .header("authorization", "Bearer pomb_whoami_grant_bootstrap")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    let bootstrap: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(bootstrap["kind"], "bootstrap");
    assert_eq!(bootstrap["boards"], "all");
    assert_eq!(bootstrap["permissions"], session["permissions"]);
}

#[tokio::test]
async fn whoami_should_list_exactly_the_permissions_of_the_grant() {
    let (_dir, fx) = fixture();
    let expected: Vec<&str> = AuthScope::Work
        .permissions()
        .iter()
        .map(|permission| permission.name())
        .collect();
    let (_, work) = token_record(&fx, "work", AuthScope::Work);
    assert_eq!(whoami_as(&fx, &work).await["permissions"], json!(expected));

    let (mut record, narrow) = new_api_token_record(&fx.user_id, "n", AuthScope::Read, None);
    record.permissions = BTreeSet::from([Permission::TasksRead]);
    fx.db.create_api_token(&record).expect("token");
    assert_eq!(
        whoami_as(&fx, &narrow).await["permissions"],
        json!(["tasks.read"]),
        "a token without settings.read still reads its own grant"
    );
    let uri = "/api/v1/auth/permissions";
    let (status, text) = request(&fx, As::Bearer(&narrow), "GET", uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let uri = "/api/v1/auth/passkeys";
    let (status, text) = request(&fx, As::Bearer(&narrow), "GET", uri, None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
}

#[tokio::test]
async fn a_lead_token_should_edit_the_palette_on_every_board() {
    let (_dir, fx) = fixture();
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (_, other) = token_record(&fx, "other", AuthScope::Provision);
    let theirs = board_by(&fx, &other, "Theirs").await;
    for board in [&theirs, &fx.board_id] {
        let colors = colors_of(&fx, board).await;
        let default = colors
            .as_array()
            .expect("colors")
            .iter()
            .find(|color| color["is_default"] == true)
            .expect("a default color")
            .clone();
        let plain = colors
            .as_array()
            .expect("colors")
            .iter()
            .find(|color| color["is_default"] == false)
            .expect("a non-default color")
            .clone();
        let uri = |color: &Value| {
            format!(
                "/api/boards/{board}/colors/{}",
                color["id"].as_str().expect("id")
            )
        };
        for (color, body) in [
            (
                &plain,
                json!({"label": "Urgent", "description": "Do first"}),
            ),
            (&plain, json!({"enabled": false})),
            (&plain, json!({"enabled": true})),
            (&plain, json!({"is_default": true})),
            (&default, json!({"is_default": false})),
            (&plain, json!({"sort_order": 0})),
        ] {
            let (status, text) = bearer_json(&fx, &lead, "PATCH", &uri(color), body.clone()).await;
            assert_eq!(status, StatusCode::OK, "{board} {body}: {text}");
        }
        let changed = colors_of(&fx, board).await;
        assert_eq!(changed[0]["id"], plain["id"], "{board}: reordered");
        assert_eq!(changed[0]["label"], "Urgent");
        assert_eq!(changed[0]["description"], "Do first");
    }
}

#[tokio::test]
async fn a_lead_token_should_create_colors_and_save_templates_on_every_board() {
    let (_dir, fx) = fixture();
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let colors = colors_of(&fx, &fx.board_id).await;
    let spare = colors
        .as_array()
        .expect("colors")
        .iter()
        .find(|color| color["is_default"] == false)
        .expect("a non-default color")
        .clone();
    let uri = format!(
        "/api/boards/{}/colors/{}",
        fx.board_id,
        spare["id"].as_str().expect("id")
    );
    let (status, gone) = call(&fx, "DELETE", &uri, None).await;
    assert!(status.is_success(), "{status}: {gone}");
    let (status, text) = bearer_json(
        &fx,
        &lead,
        "POST",
        &format!("/api/boards/{}/colors", fx.board_id),
        json!({ "value": spare["value"] }),
    )
    .await;
    assert!(status.is_success(), "{status}: {text}");
    let (status, text) = bearer_json(
        &fx,
        &lead,
        "POST",
        &format!("/api/boards/{}/save-as-template", fx.board_id),
        json!({"name": "Saved"}),
    )
    .await;
    assert!(status.is_success(), "{status}: {text}");
    let (status, text) = bearer_json(
        &fx,
        &lead,
        "POST",
        &format!("/api/boards/{}/colors/copy-from", fx.board_id),
        json!({"source_board_id": fx.board_id}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        permission_denied("colors.copy")
    );
}

#[tokio::test]
async fn a_lead_token_should_move_columns_on_every_board() {
    let (_dir, fx) = fixture();
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (_, other) = token_record(&fx, "other", AuthScope::Provision);
    let theirs = board_by(&fx, &other, "Theirs").await;
    let (status, text) = bearer_json(
        &fx,
        &other,
        "POST",
        "/api/columns",
        json!({"board_id": theirs, "name": "Foreign"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let foreign = serde_json::from_str::<Value>(&text).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_string();

    for (board, column) in [(&theirs, &foreign), (&fx.board_id, &fx.col_done)] {
        let (status, text) = bearer_json(
            &fx,
            &lead,
            "POST",
            &format!("/api/columns/{column}/move"),
            json!({"position": 0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{column}: {text}");
        assert_eq!(column_ids_of(&fx, board).await.first(), Some(column));
    }
}

#[tokio::test]
async fn board_task_list_should_filter_by_color_and_page_the_matches() {
    let (_dir, fx) = fixture();
    let colors = colors_of(&fx, &fx.board_id).await;
    let (red, blue) = (
        colors[0]["id"].as_str().expect("id").to_string(),
        colors[1]["id"].as_str().expect("id").to_string(),
    );
    let make = |column: &str, color: &str, labels: Value| {
        let body = json!({"column_id": column, "name": "T", "color_id": color, "labels": labels});
        create(&fx, body)
    };
    let a = make(&fx.col_todo, &red, json!(["x"])).await;
    let b = make(&fx.col_todo, &red, json!([])).await;
    let c = make(&fx.col_done, &red, json!(["x"])).await;
    let d = make(&fx.col_todo, &blue, json!(["x"])).await;
    let base = format!("/api/boards/{}/tasks", fx.board_id);
    let list = |query: String| {
        let uri = format!("{base}?{query}");
        let fx = &fx;
        async move {
            let (status, list) = call_list(fx, "GET", &uri, None).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {list}");
            ids(&list)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(
        list(format!("color_id={red}")).await,
        vec![a.clone(), b.clone(), c.clone()]
    );
    assert_eq!(list(format!("color_id={blue}")).await, vec![d.clone()]);
    assert_eq!(
        list(format!("color_id={red}&column_id={}", fx.col_todo)).await,
        vec![a.clone(), b.clone()]
    );
    assert_eq!(
        list(format!("color_id={red}&label=x&column_id={}", fx.col_todo)).await,
        vec![a.clone()]
    );
    assert_eq!(
        list(format!("color_id={red}&label=x&swimlane_id={}", fx.lane_b)).await,
        Vec::<String>::new()
    );

    let (seen, sizes) = walk(&fx, &format!("{base}?color_id={red}"), 2).await;
    assert_eq!((seen, sizes), (vec![a, b, c], vec![2, 1]));

    assert_invalid_request(&fx, &format!("{base}?color_id=nope")).await;
    assert_invalid_request(&fx, &format!("{base}?color_id={red}&color_id={blue}")).await;
}

#[tokio::test]
async fn column_entered_at_should_follow_the_task_across_columns_only() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let uri = format!("/api/tasks/{id}");
    let entered = |detail: &Value| {
        detail["column_entered_at"]
            .as_str()
            .expect("stamp")
            .to_string()
    };

    let (_, created) = call(&fx, "GET", &uri, None).await;
    let first = entered(&created);
    chrono::DateTime::parse_from_rfc3339(&first).expect("rfc3339");
    assert_eq!(
        first, created["created_at"],
        "a new task entered at its creation"
    );

    std::thread::sleep(std::time::Duration::from_millis(5));
    let (status, text) = call(
        &fx,
        "POST",
        &format!("{uri}/move"),
        Some(json!({"column_id": fx.col_todo, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(entered(&call(&fx, "GET", &uri, None).await.1), first);

    let (status, moved) = call(
        &fx,
        "POST",
        &format!("{uri}/move"),
        Some(json!({"column_id": fx.col_done, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let second = entered(&call(&fx, "GET", &uri, None).await.1);
    assert!(second > first, "{second} should follow {first}");

    let (_, list) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert_eq!(list[0]["column_entered_at"], second.as_str());
}

#[tokio::test]
async fn column_entered_at_should_read_the_same_on_every_task_view() {
    let (_dir, fx) = fixture();
    let still = create(&fx, json!({"column_id": fx.col_todo, "name": "Still"})).await;
    let moved = create(&fx, json!({"column_id": fx.col_todo, "name": "Moved"})).await;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (status, text) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{moved}/move"),
        Some(json!({"column_id": fx.col_done, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let stamp_in = |items: &Value, id: &str| {
        items
            .as_array()
            .expect("items")
            .iter()
            .find(|task| task["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed"))["column_entered_at"]
            .clone()
    };
    let (_, flat) = call_list(
        &fx,
        "GET",
        &format!("/api/tasks?board_id={}", fx.board_id),
        None,
    )
    .await;
    let (_, board) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    for id in [&still, &moved] {
        let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
        let stamp = detail["column_entered_at"].clone();
        assert!(stamp.is_string(), "{id}: {stamp}");
        assert_eq!(stamp_in(&flat, id), stamp, "{id}: GET /api/tasks");
        assert_eq!(stamp_in(&board, id), stamp, "{id}: board task list");
        if id == &still {
            assert_eq!(stamp, detail["created_at"]);
        } else {
            assert_ne!(stamp, detail["created_at"]);
        }
    }
}

/// Posts a comment as `who` and returns the stored comment.
async fn post_comment(fx: &Fixture, who: As<'_>, task: &str, body: Value) -> Value {
    let uri = format!("/api/tasks/{task}/comments");
    let (status, text) = request(fx, who, "POST", &uri, Some(body), true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).expect("comment json")
}

/// Returns the task's comments as the API lists them.
async fn list_comments(fx: &Fixture, task: &str) -> Value {
    let (status, list) = call(fx, "GET", &format!("/api/tasks/{task}/comments"), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list
}

#[tokio::test]
async fn a_work_or_provision_token_should_edit_and_delete_its_own_comment() {
    for scope in [AuthScope::Work, AuthScope::Provision] {
        let (_dir, fx) = fixture();
        let token = token_with(&fx, "agent", scope);
        let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
        let comment = post_comment(&fx, As::Bearer(&token), &id, json!({"body": "draft"})).await;
        let uri = format!(
            "/api/tasks/{id}/comments/{}",
            comment["id"].as_str().expect("id")
        );

        let (status, text) = request(
            &fx,
            As::Bearer(&token),
            "PATCH",
            &uri,
            Some(json!({"body": "  fixed  "})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scope:?}: {text}");
        let edited: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(edited["body"], "fixed");
        assert!(edited["edited_at"].is_string(), "{edited}");
        assert_eq!(list_comments(&fx, &id).await[0]["body"], "fixed");

        let (status, text) = request(&fx, As::Bearer(&token), "DELETE", &uri, None, true).await;
        assert_eq!(status, StatusCode::OK, "{scope:?}: {text}");
        assert_eq!(list_comments(&fx, &id).await, json!([]));
        let (status, _) = request(&fx, As::Bearer(&token), "DELETE", &uri, None, true).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn a_token_should_get_403_on_another_tokens_comment_even_with_the_same_name() {
    let (_dir, fx) = fixture();
    let mine = token_with(&fx, "agent", AuthScope::Work);
    let theirs = token_with(&fx, "agent", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let comment = post_comment(&fx, As::Bearer(&theirs), &id, json!({"body": "theirs"})).await;
    let uri = format!(
        "/api/tasks/{id}/comments/{}",
        comment["id"].as_str().expect("id")
    );

    for (method, body, permission) in [
        (
            "PATCH",
            Some(json!({"body": "mine now"})),
            "comments.edit_any",
        ),
        ("DELETE", None, "comments.delete"),
    ] {
        let (status, text) = request(&fx, As::Bearer(&mine), method, &uri, body, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied(permission),
            "{method}"
        );
    }
    let list = list_comments(&fx, &id).await;
    assert_eq!(list[0]["body"], "theirs");
    assert_eq!(list[0]["edited_at"], Value::Null);
}

#[tokio::test]
async fn a_token_should_get_403_on_a_comment_written_before_comments_recorded_token_ids() {
    let (_dir, fx) = fixture();
    let token = token_with(&fx, "agent", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let old = fx
        .db
        .add_comment(
            &id,
            NewComment {
                author: "agent",
                body: "old",
                actor_kind: ActorKind::ApiToken,
                token_name: Some("agent"),
                token_id: None,
                kind: None,
            },
            Actor::SYSTEM,
            None,
        )
        .expect("comment");
    let FencedWrite::Written(old) = old else {
        panic!("comment not written: {old:?}");
    };
    let uri = format!("/api/tasks/{id}/comments/{}", old.id);

    let (status, text) = request(&fx, As::Bearer(&token), "DELETE", &uri, None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(
        list_comments(&fx, &id)
            .await
            .as_array()
            .expect("array")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_token_should_get_403_on_a_persons_comment_and_the_browser_keeps_full_rights() {
    let (_dir, fx) = fixture();
    let token = token_with(&fx, "agent", AuthScope::Provision);
    let admin_token = token_with(&fx, "boss", AuthScope::Admin);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let person = post_comment(&fx, As::Admin, &id, json!({"body": "from Chip"})).await;
    let person_uri = format!(
        "/api/tasks/{id}/comments/{}",
        person["id"].as_str().expect("id")
    );

    for (method, body, permission) in [
        (
            "PATCH",
            Some(json!({"body": "hijacked"})),
            "comments.edit_any",
        ),
        ("DELETE", None, "comments.delete"),
    ] {
        let (status, text) =
            request(&fx, As::Bearer(&token), method, &person_uri, body, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied(permission),
            "{method}"
        );
    }
    assert_eq!(list_comments(&fx, &id).await[0]["body"], "from Chip");

    let agent = post_comment(&fx, As::Bearer(&token), &id, json!({"body": "agent"})).await;
    let agent_uri = format!(
        "/api/tasks/{id}/comments/{}",
        agent["id"].as_str().expect("id")
    );
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &agent_uri,
        Some(json!({"body": "moderated"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = request(
        &fx,
        As::Bearer(&admin_token),
        "DELETE",
        &agent_uri,
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = request(&fx, As::Admin, "DELETE", &person_uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(list_comments(&fx, &id).await, json!([]));
}

#[tokio::test]
async fn editing_a_comment_should_never_change_who_wrote_it_or_when() {
    let (_dir, fx) = fixture();
    let token = token_with(&fx, "agent", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let comment = post_comment(
        &fx,
        As::Bearer(&token),
        &id,
        json!({"body": "first", "author": "reviewer", "kind": "review"}),
    )
    .await;
    let before = list_comments(&fx, &id).await[0].clone();
    assert_eq!(before["actor_kind"], "api_token");
    assert_eq!(before["token_name"], "agent");
    let uri = format!(
        "/api/tasks/{id}/comments/{}",
        comment["id"].as_str().expect("id")
    );
    let forged = json!({
        "body": "second",
        "author": "Chip",
        "actor_kind": "user",
        "token_name": null,
        "token_id": "someone-else",
        "kind": "system",
        "created_at": "2001-01-01T00:00:00Z",
        "id": "other",
    });

    for who in [As::Bearer(&token), As::Admin] {
        let (status, text) = request(&fx, who, "PATCH", &uri, Some(forged.clone()), true).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let returned: Value = serde_json::from_str(&text).expect("json");
        let listed = list_comments(&fx, &id).await[0].clone();
        for view in [&returned, &listed] {
            assert_eq!(view["body"], "second");
            for field in [
                "id",
                "author",
                "actor_kind",
                "token_name",
                "token_id",
                "kind",
                "created_at",
            ] {
                assert_eq!(view[field], before[field], "{field} changed: {view}");
            }
        }
    }
}

#[tokio::test]
async fn editing_a_comment_should_refuse_an_empty_or_overlong_body() {
    let (_dir, fx) = fixture();
    let token = token_with(&fx, "agent", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let comment = post_comment(&fx, As::Bearer(&token), &id, json!({"body": "keep"})).await;
    let uri = format!(
        "/api/tasks/{id}/comments/{}",
        comment["id"].as_str().expect("id")
    );

    for body in ["   ".to_string(), "x".repeat(5001)] {
        let (status, text) = request(
            &fx,
            As::Bearer(&token),
            "PATCH",
            &uri,
            Some(json!({"body": body})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    }
    assert_eq!(list_comments(&fx, &id).await[0]["body"], "keep");
}

const CHECKLIST: &str =
    "# Plan\n\n- [ ] one\n  - [x] nested\n\n```\n- [ ] in code\n```\n\n- [ ] two\n";

async fn checklist_task(fx: &Fixture) -> String {
    let body = json!({"column_id": fx.col_todo, "name": "Checklist", "description": CHECKLIST});
    create(fx, body).await
}

async fn post_tick(
    fx: &Fixture,
    who: As<'_>,
    id: &str,
    index: usize,
    body: Value,
) -> (StatusCode, String) {
    let uri = format!("/api/tasks/{id}/checklist/{index}");
    request(fx, who, "POST", &uri, Some(body), true).await
}

fn work_token(fx: &Fixture) -> String {
    let (record, token) = new_api_token_record(&fx.user_id, "worker", AuthScope::Work, None);
    fx.db.create_api_token(&record).expect("token");
    token
}

#[tokio::test]
async fn ticking_a_checklist_box_should_rewrite_only_that_box_and_record_history() {
    let (_dir, fx) = fixture();
    let id = checklist_task(&fx).await;
    let hash = markdown::hash(CHECKLIST);
    let before = stored(&fx, &id);

    let body = json!({"checked": true, "description_hash": hash});
    let (status, text) = post_tick(&fx, As::Admin, &id, 2, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let reply: Value = serde_json::from_str(&text).expect("json");
    let expected = CHECKLIST.replace("- [ ] two", "- [x] two");
    assert_eq!(reply["description"], expected);
    assert_eq!(reply["description_hash"], markdown::hash(&expected));
    assert_eq!(detail(&fx, &id).await["description"], expected);

    let row = fx.db.task(&id).expect("read").expect("task");
    let last = row.history.last().expect("history");
    assert_eq!(last.kind, HistoryKind::DescriptionUpdated);
    assert_eq!(last.detail, "Ticked checklist item 3");
    assert_eq!(
        row.history.len(),
        before["history"].as_array().expect("array").len() + 1
    );

    let next = reply["description_hash"].as_str().expect("hash");
    let body = json!({"checked": false, "description_hash": next});
    let (status, text) = post_tick(&fx, As::Admin, &id, 2, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(detail(&fx, &id).await["description"], CHECKLIST);
    let row = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(
        row.history.last().expect("history").detail,
        "Cleared checklist item 3"
    );
}

#[tokio::test]
async fn ticking_a_box_already_in_that_state_should_write_nothing() {
    let (_dir, fx) = fixture();
    let id = checklist_task(&fx).await;
    let before = stored(&fx, &id);
    let body = json!({"checked": true, "description_hash": markdown::hash(CHECKLIST)});
    let (status, text) = post_tick(&fx, As::Admin, &id, 1, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(stored(&fx, &id), before);
}

#[tokio::test]
async fn ticking_should_refuse_a_missing_box_a_stale_hash_and_a_blank_hash() {
    let (_dir, fx) = fixture();
    let id = checklist_task(&fx).await;
    let before = stored(&fx, &id);
    let hash = markdown::hash(CHECKLIST);

    let body = json!({"checked": true, "description_hash": hash});
    let (status, _) = post_tick(&fx, As::Admin, &id, 3, body).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let stale = markdown::hash("something else");
    let body = json!({"checked": true, "description_hash": stale});
    let (status, _) = post_tick(&fx, As::Admin, &id, 0, body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body = json!({"checked": true, "description_hash": stale});
    let (status, _) = post_tick(&fx, As::Admin, &id, 9, body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a stale hash wins over a missing box"
    );

    for body in [
        json!({"checked": true, "description_hash": ""}),
        json!({"checked": true, "description_hash": "  "}),
        json!({"checked": true}),
        json!({"description_hash": hash}),
    ] {
        let (status, text) = post_tick(&fx, As::Admin, &id, 0, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    }
    let (status, _) = post_tick(
        &fx,
        As::Admin,
        "no-such-task",
        0,
        json!({"checked": true, "description_hash": hash}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(stored(&fx, &id), before);
}

#[tokio::test]
async fn a_tick_should_be_refused_after_the_description_was_edited() {
    let (_dir, fx) = fixture();
    let id = checklist_task(&fx).await;
    let hash = markdown::hash(CHECKLIST);
    let edit = json!({"description": "- [ ] other\n"});
    assert_eq!(
        call(&fx, "PATCH", &format!("/api/tasks/{id}"), Some(edit))
            .await
            .0,
        StatusCode::OK
    );
    let (status, _) = post_tick(
        &fx,
        As::Admin,
        &id,
        0,
        json!({"checked": true, "description_hash": hash}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(detail(&fx, &id).await["description"], "- [ ] other\n");
}

#[tokio::test]
async fn ticking_should_need_the_work_scope() {
    let (_dir, fx) = fixture();
    let id = checklist_task(&fx).await;
    let body = json!({"checked": true, "description_hash": markdown::hash(CHECKLIST)});

    let (status, text) = post_tick(&fx, As::Bearer(&fx.read_token), &id, 0, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(detail(&fx, &id).await["description"], CHECKLIST);

    let token = work_token(&fx);
    let (status, text) = post_tick(&fx, As::Bearer(&token), &id, 0, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        detail(&fx, &id).await["description"],
        CHECKLIST.replacen("- [ ] one", "- [x] one", 1)
    );
    let row = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(row.history.last().expect("history").actor, "api-token");
}

#[tokio::test]
async fn the_modal_should_enable_description_boxes_only_for_a_viewer_who_can_edit() {
    let (_dir, fx) = fixture();
    let id = checklist_task(&fx).await;
    let uri = format!("/api/tasks/{id}/modal");

    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(
        html.contains(r#"<input type="checkbox" data-checklist-index="2">"#),
        "{html}"
    );
    assert!(!html.contains(r#"disabled data-checklist-index"#), "{html}");
    let hash = markdown::hash(CHECKLIST);
    assert!(
        html.contains(&format!(r#"data-description-hash="{hash}""#)),
        "{html}"
    );

    let work = work_token(&fx);
    let (_, html) = request(&fx, As::Bearer(&work), "GET", &uri, None, false).await;
    assert!(
        html.contains(r#"<input type="checkbox" data-checklist-index="0">"#),
        "{html}"
    );

    let (_, html) = request(&fx, As::Bearer(&fx.read_token), "GET", &uri, None, false).await;
    assert!(
        html.contains(r#"<input type="checkbox" disabled data-checklist-index="0">"#),
        "{html}"
    );
    assert!(
        !html.contains(r#"<input type="checkbox" data-checklist-index"#),
        "{html}"
    );
}

#[tokio::test]
async fn comment_boxes_should_stay_disabled_for_an_editor() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Remarks"})).await;
    call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/comments"),
        Some(json!({"body": "- [ ] from a comment"})),
    )
    .await;
    let (_, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{id}/modal"),
        None,
        false,
    )
    .await;
    assert!(
        html.contains(
            r#"<input type="checkbox" disabled data-checklist-index="0"> from a comment"#
        ),
        "{html}"
    );
    assert!(
        !html.contains(r#"<input type="checkbox" data-checklist-index"#),
        "{html}"
    );
}

#[tokio::test]
async fn the_task_view_should_carry_the_hash_the_tick_route_accepts() {
    let (_dir, fx) = fixture();
    let id = checklist_task(&fx).await;
    let view = detail(&fx, &id).await;
    let hash = view["description_hash"].as_str().expect("hash");
    assert_eq!(
        hash,
        markdown::hash(view["description"].as_str().expect("text"))
    );
    let body = json!({"checked": true, "description_hash": hash});
    let (status, text) = post_tick(&fx, As::Admin, &id, 0, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

async fn move_to_done(fx: &Fixture, task: &str) {
    let body = json!({"column_id": fx.col_done, "position": 0});
    let (status, moved) = call(fx, "POST", &format!("/api/tasks/{task}/move"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
}

async fn card_html(fx: &Fixture, task: &str) -> String {
    let uri = format!("/api/tasks/{task}/card");
    request(fx, As::Admin, "GET", &uri, None, false).await.1
}

async fn post_link(fx: &Fixture, task: &str, kind: &str, target: &str) -> (StatusCode, String) {
    let uri = format!("/api/tasks/{task}/links");
    let body = json!({"kind": kind, "target_id": target});
    request(fx, As::Admin, "POST", &uri, Some(body), true).await
}

const PARENT_CYCLE: &str = "the link would put the umbrella under its own child";

#[tokio::test]
async fn parent_and_child_links_should_list_on_both_tasks_and_count_done_children() {
    let (_dir, fx) = fixture();
    let umbrella = create_epic(&fx, "Tokens").await;
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "Slice A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "Slice B"})).await;
    let (status, created) = link(&fx, &a, "parent", &umbrella).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["kind"], "parent");
    let (status, created) = link(&fx, &umbrella, "child", &b).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["kind"], "child");
    assert_eq!(created["target_id"], b.as_str());

    assert_eq!(
        links_of(&fx, &a).await,
        vec![("parent".into(), umbrella.clone())]
    );
    assert_eq!(
        links_of(&fx, &umbrella).await,
        vec![("child".into(), a.clone()), ("child".into(), b.clone())]
    );
    assert!(!blocked(&fx, &a).await && !blocked(&fx, &umbrella).await);

    let view = detail(&fx, &umbrella).await;
    assert_eq!(view["parent"], Value::Null);
    assert_eq!(view["children_total"], 2);
    assert_eq!(view["children_done"], 0);
    assert_eq!(view["children"][0]["name"], "Slice A");
    assert_eq!(view["children"][0]["column_name"], "Todo");
    assert_eq!(view["children"][0]["done"], false);
    let view = detail(&fx, &a).await;
    assert_eq!(view["parent"]["id"], umbrella.as_str());
    assert_eq!(view["parent"]["name"], "Tokens");
    assert_eq!(view["parent"]["board_id"], fx.board_id.as_str());
    assert_eq!(view["children_total"], 0);
    assert_eq!(view["children"], json!([]));

    make_plain(&fx.db, &[&umbrella]);
    move_to_done(&fx, &a).await;
    let view = detail(&fx, &umbrella).await;
    assert_eq!(
        view["children_done"], 1,
        "the Shipped column is done by flag"
    );
    assert_eq!(view["children_total"], 2);
    assert_eq!(view["children"][0]["done"], true);
    assert_eq!(view["children"][0]["column_name"], "Shipped");

    let html = card_html(&fx, &umbrella).await;
    assert!(html.contains("task-card-umbrella"), "{html}");
    assert!(html.contains("1 of 2 done"), "{html}");
    assert!(!html.contains("card-epic-tag"), "{html}");
    let html = card_html(&fx, &a).await;
    assert!(html.contains("card-epic-tag"), "{html}");
    assert!(html.contains("In epic Tokens"), "{html}");
    assert!(
        html.contains(&format!(r#"data-epic-id="{umbrella}""#)),
        "{html}"
    );
    assert!(html.contains(">Tokens</span>"), "{html}");
    assert!(!html.contains("task-card-umbrella"), "{html}");

    let (_, again) = link(&fx, &a, "parent", &umbrella).await;
    assert_eq!(again["kind"], "parent");
    assert_eq!(
        links_of(&fx, &umbrella).await.len(),
        2,
        "the same link is not stored twice"
    );
}

#[tokio::test]
async fn parent_link_should_refuse_a_second_umbrella_and_itself() {
    let (_dir, fx) = fixture();
    let umbrella = create_epic(&fx, "U").await;
    let other = create(&fx, json!({"column_id": fx.col_todo, "name": "V"})).await;
    let child = create(&fx, json!({"column_id": fx.col_todo, "name": "C"})).await;
    assert_eq!(
        link(&fx, &child, "parent", &umbrella).await.0,
        StatusCode::OK
    );

    for (task, kind, target, message) in [
        (
            &child,
            "parent",
            &other,
            "the child already has an umbrella",
        ),
        (&other, "child", &child, "the child already has an umbrella"),
        (&child, "parent", &child, "target is the task itself"),
    ] {
        let (status, text) = post_link(&fx, task, kind, target).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, message),
            "{kind}"
        );
    }
    assert_eq!(
        links_of(&fx, &umbrella).await,
        vec![("child".into(), child.clone())]
    );
    assert!(links_of(&fx, &other).await.is_empty());
}

#[tokio::test]
async fn parent_link_should_refuse_the_reverse_of_an_existing_parent_link() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create_epic(&fx, "B").await;
    assert_eq!(link(&fx, &a, "parent", &b).await.0, StatusCode::OK);
    let (status, text) = post_link(&fx, &b, "parent", &a).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, PARENT_CYCLE)
    );
    let (status, text) = post_link(&fx, &a, "child", &b).await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::BAD_REQUEST, PARENT_CYCLE)
    );
    assert_eq!(links_of(&fx, &a).await, vec![("parent".into(), b.clone())]);
}

#[tokio::test]
async fn parent_link_should_not_reach_a_task_of_another_account() {
    let (_dir, fx) = fixture();
    let bob = fx.db.create_user("bob").expect("bob");
    let bob_board = fx.db.create_board(&bob, "Bob").expect("board");
    let bob_column = fx
        .db
        .create_column(&bob_board, "Todo", None)
        .expect("column");
    let theirs = fx
        .db
        .create_task(&bob_column, None, "Theirs", 1, None)
        .expect("task");
    let mine = create(&fx, json!({"column_id": fx.col_todo, "name": "Mine"})).await;

    for (task, kind, target) in [(&mine, "parent", &theirs), (&mine, "child", &theirs)] {
        let (status, text) = post_link(&fx, task, kind, target).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::NOT_FOUND, "Task or target not found"),
            "{kind}"
        );
    }
    let (status, text) = post_link(&fx, &theirs, "parent", &mine).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    assert!(links_of(&fx, &mine).await.is_empty());
    assert!(
        fx.db
            .account(&bob)
            .list_task_links(&theirs)
            .expect("read")
            .is_some_and(|l| l.is_empty())
    );
}

#[tokio::test]
async fn children_on_other_boards_should_count_by_their_own_done_column() {
    let (_dir, fx) = fixture();
    let board_id = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let open = fx
        .db
        .create_column(&board_id, "Backlog", None)
        .expect("column");
    let done = fx
        .db
        .create_column(&board_id, "Closed", None)
        .expect("column");
    fx.db.set_column_done(&done, true).expect("set done");
    let umbrella = create_epic(&fx, "U").await;
    let near = create(&fx, json!({"column_id": open, "name": "Far open"})).await;
    let far = create(&fx, json!({"column_id": done, "name": "Far done"})).await;
    link(&fx, &near, "parent", &umbrella).await;
    link(&fx, &far, "parent", &umbrella).await;

    let view = detail(&fx, &umbrella).await;
    assert_eq!(view["children_total"], 2);
    assert_eq!(view["children_done"], 1);
    assert_eq!(view["children"][0]["board_id"], board_id.as_str());
    assert_eq!(view["children"][1]["column_name"], "Closed");
    assert_eq!(view["children"][1]["done"], true);
    assert!(card_html(&fx, &umbrella).await.contains("1 of 2 done"));
}

#[tokio::test]
async fn deleting_an_umbrella_or_a_child_should_clear_the_parent_link() {
    let (_dir, fx) = fixture();
    let umbrella = create_epic(&fx, "U").await;
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    link(&fx, &a, "parent", &umbrella).await;
    link(&fx, &b, "parent", &umbrella).await;

    make_plain(&fx.db, &[&umbrella]);
    move_to_done(&fx, &umbrella).await;
    let view = detail(&fx, &umbrella).await;
    assert_eq!(view["children_total"], 2);
    assert_eq!(
        view["children_done"], 0,
        "the umbrella's own column is not counted"
    );
    assert_eq!(detail(&fx, &a).await["parent"]["id"], umbrella.as_str());

    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{umbrella}"), None).await;
    assert_eq!(status, StatusCode::OK);
    for child in [&a, &b] {
        let view = detail(&fx, child).await;
        assert_eq!(view["parent"], Value::Null);
        assert!(view["name"].is_string());
        assert!(links_of(&fx, child).await.is_empty());
        assert!(!card_html(&fx, child).await.contains("card-epic-tag"));
    }

    let again = create_epic(&fx, "U2").await;
    assert_eq!(link(&fx, &a, "parent", &again).await.0, StatusCode::OK);
    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{a}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let view = detail(&fx, &again).await;
    assert_eq!(view["children_total"], 0);
    assert!(links_of(&fx, &again).await.is_empty());
}

async fn modal_html(fx: &Fixture, task: &str) -> String {
    let uri = format!("/api/tasks/{task}/modal");
    let (status, html) = request(fx, As::Admin, "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    html
}

fn description_panel(html: &str) -> &str {
    let start = html
        .find(r#"id="tm-panel-description""#)
        .expect("description panel");
    let rest = &html[start..];
    &rest[..rest.find("</section>").expect("panel end")]
}

#[tokio::test]
async fn the_modal_should_show_the_children_tab_chip_and_rows_for_an_umbrella_only() {
    let (_dir, fx) = fixture();
    let umbrella = create_epic(&fx, "Tokens").await;
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "Slice A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "Slice B"})).await;
    let plain = create(&fx, json!({"column_id": fx.col_todo, "name": "Plain"})).await;
    link(&fx, &a, "parent", &umbrella).await;
    link(&fx, &b, "parent", &umbrella).await;
    make_plain(&fx.db, &[&umbrella]);
    move_to_done(&fx, &a).await;

    let html = modal_html(&fx, &umbrella).await;
    assert!(html.contains(r#"id="tm-tab-children""#), "{html}");
    assert!(html.contains(r#"id="tm-panel-children""#), "{html}");
    assert!(html.contains("Children &middot; 1/2"), "{html}");
    assert!(html.contains("tm-children-chip"), "{html}");
    assert!(html.contains("1 of 2 children done"), "{html}");
    assert!(html.contains("1 of 2 done"), "{html}");
    assert!(!html.contains(r#"data-link-open="child""#), "{html}");
    assert!(
        html.contains(&format!(r#"data-unlink-child="{a}""#)),
        "{html}"
    );
    assert!(html.contains(r#"aria-label="Unlink Slice B""#), "{html}");
    assert!(!html.contains(r#"data-tm-act="link-child""#), "{html}");
    assert!(html.contains(r#"class="tm-child tm-child-done""#), "{html}");
    let description = description_panel(&html);
    assert!(!description.contains("Slice A"), "{description}");
    assert!(!description.contains("tm-child"), "{description}");

    let html = modal_html(&fx, &plain).await;
    for absent in [
        "tm-tab-children",
        "tm-panel-children",
        "tm-children-chip",
        "data-unlink-child",
        "data-umbrella-change",
    ] {
        assert!(!html.contains(absent), "{absent}: {html}");
    }
    assert!(!html.contains(r#"data-tm-act="link-child""#), "{html}");
}

#[tokio::test]
async fn the_modal_should_offer_change_and_remove_on_a_child() {
    let (_dir, fx) = fixture();
    let umbrella = create_epic(&fx, "Tokens").await;
    let child = create(&fx, json!({"column_id": fx.col_todo, "name": "Slice A"})).await;
    link(&fx, &child, "parent", &umbrella).await;

    let html = modal_html(&fx, &child).await;
    assert!(html.contains("Part of: "), "{html}");
    assert!(
        html.contains(&format!(r#"data-open-task="{umbrella}""#)),
        "{html}"
    );
    assert!(html.contains(r#"aria-label="Change umbrella""#), "{html}");
    assert!(
        html.contains(r#"aria-label="Remove from umbrella""#),
        "{html}"
    );
    assert!(!html.contains(r#"data-tm-act="link-child""#), "{html}");
    assert!(!html.contains("tm-tab-children"), "{html}");
}

#[tokio::test]
async fn hide_done_children_should_round_trip_through_settings_and_render_in_the_modal() {
    let (_dir, fx) = fixture();
    let umbrella = create_epic(&fx, "Tokens").await;
    let open = create(&fx, json!({"column_id": fx.col_todo, "name": "Open one"})).await;
    let done = create(&fx, json!({"column_id": fx.col_todo, "name": "Done one"})).await;
    link(&fx, &open, "parent", &umbrella).await;
    link(&fx, &done, "parent", &umbrella).await;
    move_to_done(&fx, &done).await;

    let (_, settings) = call(&fx, "GET", "/api/settings", None).await;
    assert_eq!(settings["modal_folded_groups"], json!([]));
    let html = modal_html(&fx, &umbrella).await;
    assert!(html.contains(r#"aria-pressed="false""#), "{html}");
    assert!(html.contains(">Hide done<"), "{html}");
    assert!(!html.contains(r#"data-child-done="true" hidden"#), "{html}");

    let body = json!({"modal_folded_groups": ["children-done"]});
    let (status, saved) = call(&fx, "PUT", "/api/settings", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["modal_folded_groups"], json!(["children-done"]));
    let (_, reloaded) = call(&fx, "GET", "/api/settings", None).await;
    assert_eq!(reloaded["modal_folded_groups"], json!(["children-done"]));

    let html = modal_html(&fx, &umbrella).await;
    assert!(
        html.contains(r#"data-children-hide-done aria-pressed="true">Show done<"#),
        "{html}"
    );
    assert!(html.contains(r#"data-child-done="true" hidden"#), "{html}");
    assert!(
        !html.contains(r#"data-child-done="false" hidden"#),
        "{html}"
    );
    assert!(html.contains(r#"id="tm-children-note" hidden"#), "{html}");

    let other = call(
        &fx,
        "PUT",
        "/api/settings",
        Some(json!({"pomodoro_minutes": 30})),
    )
    .await;
    assert_eq!(
        other.1["modal_folded_groups"],
        json!(["children-done"]),
        "an unrelated update keeps it"
    );
    let body = json!({"modal_folded_groups": []});
    call(&fx, "PUT", "/api/settings", Some(body)).await;
    let html = modal_html(&fx, &umbrella).await;
    assert!(html.contains(r#"aria-pressed="false""#), "{html}");
}

async fn change_umbrella(fx: &Fixture, task: &str, target: &str) -> (StatusCode, Value) {
    let body = json!({"target_id": target});
    call(
        fx,
        "PUT",
        &format!("/api/tasks/{task}/umbrella"),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn changing_the_umbrella_should_replace_the_parent_link_in_one_call() {
    let (_dir, fx) = fixture();
    let old = create_epic(&fx, "Old").await;
    let new = create_epic(&fx, "New").await;
    let kid = create_epic(&fx, "Kid").await;
    let loner = create(&fx, json!({"column_id": fx.col_todo, "name": "Loner"})).await;
    let member = create(&fx, json!({"column_id": fx.col_todo, "name": "Member"})).await;
    let grand = create(&fx, json!({"column_id": fx.col_todo, "name": "Grand"})).await;
    link(&fx, &kid, "parent", &old).await;
    link(&fx, &member, "parent", &new).await;
    link(&fx, &grand, "parent", &kid).await;

    let (status, moved) = change_umbrella(&fx, &kid, &new).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["kind"], "parent");
    assert_eq!(moved["target_id"], new.as_str());
    assert_eq!(links_of(&fx, &kid).await.len(), 2);
    assert_eq!(detail(&fx, &kid).await["parent"]["id"], new.as_str());
    assert_eq!(detail(&fx, &old).await["children_total"], 0);
    assert_eq!(detail(&fx, &new).await["children_total"], 2);
    assert_eq!(event_details(&fx, &kid, HistoryKind::LinkRemoved).len(), 1);
    assert_eq!(event_details(&fx, &kid, HistoryKind::LinkAdded).len(), 2);

    let (status, same) = change_umbrella(&fx, &kid, &new).await;
    assert_eq!(status, StatusCode::OK, "{same}");
    assert_eq!(same["id"], moved["id"], "the current umbrella is unchanged");

    for (task, target) in [(&kid, &kid), (&loner, &new), (&kid, &grand)] {
        let (status, text) = change_umbrella(&fx, task, target).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    }
    let (status, _) = change_umbrella(&fx, &kid, "missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        detail(&fx, &kid).await["parent"]["id"],
        new.as_str(),
        "a refused change keeps the umbrella"
    );
}

#[tokio::test]
async fn a_parent_link_to_a_plain_task_should_be_refused_as_not_an_epic() {
    let (_dir, fx) = fixture();
    let plain = create(&fx, json!({"column_id": fx.col_todo, "name": "Plain"})).await;
    let epic = create_epic(&fx, "Epic").await;
    let kid = create(&fx, json!({"column_id": fx.col_todo, "name": "Kid"})).await;
    let other = create(&fx, json!({"column_id": fx.col_todo, "name": "Other"})).await;

    let admin = token_with(&fx, "admin", AuthScope::Admin);
    let refused = |method: &'static str, uri: String, body: Value| {
        let who = As::Bearer(&admin);
        let fx = &fx;
        async move {
            let (status, text) = request(fx, who, method, &uri, Some(body), true).await;
            let error: Value = serde_json::from_str(&text).expect("json");
            (status, error)
        }
    };
    for (task, kind, target) in [(&kid, "parent", &plain), (&plain, "child", &kid)] {
        let uri = format!("/api/tasks/{task}/links");
        let body = json!({"kind": kind, "target_id": target});
        let (status, error) = refused("POST", uri, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}: {error}");
        assert_eq!(error["code"], "not_epic", "{kind}");
    }
    assert!(links_of(&fx, &kid).await.is_empty());

    assert_eq!(link(&fx, &kid, "parent", &epic).await.0, StatusCode::OK);
    assert_eq!(link(&fx, &epic, "child", &other).await.0, StatusCode::OK);
    let uri = format!("/api/tasks/{kid}/umbrella");
    let (status, error) = refused("PUT", uri, json!({"target_id": plain})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(error["code"], "not_epic");
    assert_eq!(detail(&fx, &kid).await["parent"]["id"], epic.as_str());
}

async fn nested_tree(fx: &Fixture) -> [String; 4] {
    let root = create_epic(fx, "Root").await;
    let mid = create_epic(fx, "Mid").await;
    let leaf = create(fx, json!({"column_id": fx.col_todo, "name": "Leaf"})).await;
    let free = create(fx, json!({"column_id": fx.col_todo, "name": "Free"})).await;
    assert_eq!(link(fx, &mid, "parent", &root).await.0, StatusCode::OK);
    assert_eq!(link(fx, &leaf, "parent", &mid).await.0, StatusCode::OK);
    [root, mid, leaf, free]
}

#[tokio::test]
async fn parent_links_should_nest_three_levels_from_the_api() {
    let (_dir, fx) = fixture();
    let [root, mid, leaf, _] = nested_tree(&fx).await;
    let top = create_epic(&fx, "Top").await;
    assert_eq!(link(&fx, &top, "child", &root).await.0, StatusCode::OK);

    let view = detail(&fx, &leaf).await;
    assert_eq!(view["parent"]["id"], mid.as_str());
    let path: Vec<&str> = view["parent"]["path"]
        .as_array()
        .expect("path")
        .iter()
        .map(|part| part["id"].as_str().expect("id"))
        .collect();
    assert_eq!(path, [top.as_str(), root.as_str(), mid.as_str()]);
    assert_eq!(view["parent"]["path"][0]["name"], "Top");
    assert_eq!(view["parent"]["path"][2]["name"], "Mid");

    let view = detail(&fx, &root).await;
    assert_eq!(view["children_total"], 1);
    assert_eq!(view["parent"]["path"].as_array().map(Vec::len), Some(1));
    assert_eq!(links_of(&fx, &mid).await.len(), 2);
}

#[tokio::test]
async fn a_parent_link_that_would_form_a_cycle_should_be_refused() {
    let (_dir, fx) = fixture();
    let [root, mid, leaf, free] = nested_tree(&fx).await;

    for (task, kind, target) in [
        (&root, "parent", &leaf),
        (&leaf, "child", &root),
        (&root, "parent", &mid),
        (&mid, "child", &root),
    ] {
        let (status, text) = post_link(&fx, task, kind, target).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, PARENT_CYCLE),
            "{kind}"
        );
    }
    link(&fx, &free, "parent", &leaf).await;
    for target in [&leaf, &free, &mid] {
        let (status, text) = change_umbrella(&fx, &mid, target).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    }
    assert_eq!(detail(&fx, &mid).await["parent"]["id"], root.as_str());
    assert!(
        links_of(&fx, &root)
            .await
            .iter()
            .all(|(kind, _)| kind == "child")
    );
}

#[tokio::test]
async fn the_modal_should_show_the_whole_umbrella_path_and_count_direct_children() {
    let (_dir, fx) = fixture();
    let [root, mid, leaf, _] = nested_tree(&fx).await;
    let twin = create(&fx, json!({"column_id": fx.col_todo, "name": "Twin"})).await;
    link(&fx, &twin, "parent", &mid).await;
    make_plain(&fx.db, &[&root, &mid]);
    move_to_done(&fx, &leaf).await;

    let html = modal_html(&fx, &leaf).await;
    let at = |id: &str| {
        html.find(&format!(r#"data-open-task="{id}""#))
            .expect("path part")
    };
    assert!(at(&root) < at(&mid), "{html}");
    assert!(html.contains("&rsaquo;"), "{html}");
    assert!(html.contains(">Root</span>"), "{html}");
    assert!(!html.contains(r#"data-tm-act="link-child""#), "{html}");

    let html = modal_html(&fx, &mid).await;
    assert!(html.contains("1 of 2 children done"), "{html}");
    assert!(html.contains("Children &middot; 1/2"), "{html}");
    assert!(!html.contains(r#"data-tm-act="link-child""#), "{html}");

    let html = modal_html(&fx, &root).await;
    assert!(html.contains("0 of 1 children done"), "{html}");

    let html = card_html(&fx, &leaf).await;
    assert!(
        html.contains("title=\"In epic Root &rsaquo; Mid\""),
        "{html}"
    );
    assert!(html.contains(&format!("data-epic-id=\"{mid}\"")), "{html}");
    assert!(html.contains(">Mid</span>"), "{html}");
}

#[tokio::test]
async fn the_link_picker_should_offer_nested_umbrellas_minus_the_card_and_its_descendants() {
    let (_dir, fx) = fixture();
    let [root, mid, leaf, free] = nested_tree(&fx).await;

    assert!(picker_ids(&fx, &mid, "mode=parent").await.is_empty());
    assert_eq!(
        picker_ids(&fx, &mid, "mode=child").await,
        std::slice::from_ref(&free)
    );
    assert_eq!(
        picker_ids(&fx, &free, "mode=parent").await,
        [mid.clone(), root.clone()]
    );
    assert_eq!(
        picker_ids(&fx, &free, "mode=child").await,
        std::slice::from_ref(&root)
    );
    assert_eq!(
        picker_ids(&fx, &leaf, "mode=child").await,
        std::slice::from_ref(&free)
    );
    assert!(picker_ids(&fx, &root, "mode=parent").await.is_empty());
}

#[tokio::test]
async fn the_link_picker_should_offer_only_epics_in_parent_mode() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let epic = create_epic(&fx, "Epic").await;
    let plain = create(&fx, json!({"column_id": fx.col_todo, "name": "Plain"})).await;
    let legacy = create_epic(&fx, "Legacy").await;
    make_plain(&fx.db, &[&legacy]);

    assert_eq!(
        picker_ids(&fx, &card, "mode=parent").await,
        std::slice::from_ref(&epic)
    );
    assert_eq!(
        picker_ids(&fx, &card, "mode=child").await,
        [epic, legacy, plain],
        "a child link takes any card"
    );
}

async fn picker_ids(fx: &Fixture, task: &str, query: &str) -> Vec<String> {
    let scope = format!("board%3A{}", fx.board_id);
    let scoped = match query.split_once("q=") {
        Some((head, text)) => format!("{head}q={scope}%20{text}"),
        None => format!("{query}&q={scope}"),
    };
    picker_ids_everywhere(fx, task, &scoped).await
}

async fn picker_ids_everywhere(fx: &Fixture, task: &str, query: &str) -> Vec<String> {
    let uri = format!("/api/tasks/{task}/link-picker?{query}");
    let (status, html) = request(fx, As::Admin, "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    html.split(r#"data-pick-id=""#)
        .skip(1)
        .map(|rest| rest[..rest.find('"').expect("quote")].to_string())
        .collect()
}

#[tokio::test]
async fn the_link_picker_should_list_the_tasks_a_child_link_would_accept() {
    let (_dir, fx) = fixture();
    let card = create_epic(&fx, "Card").await;
    let kid = create(&fx, json!({"column_id": fx.col_todo, "name": "Kid"})).await;
    let umbrella = create_epic(&fx, "Umbrella").await;
    let member = create(&fx, json!({"column_id": fx.col_todo, "name": "Member"})).await;
    let free_b = create(&fx, json!({"column_id": fx.col_done, "name": "bravo"})).await;
    let free_a = create(&fx, json!({"column_id": fx.col_todo, "name": "Alpha"})).await;
    link(&fx, &card, "child", &kid).await;
    link(&fx, &member, "parent", &umbrella).await;

    let listed = picker_ids(&fx, &card, "mode=child").await;
    assert_eq!(
        listed,
        vec![free_a.clone(), umbrella.clone(), free_b.clone()],
        "open cards come before done ones"
    );
    assert_eq!(
        picker_ids(&fx, &card, "mode=child&q=BRA").await,
        vec![free_b.clone()]
    );
    assert!(picker_ids(&fx, &card, "mode=child&q=zzz").await.is_empty());

    let uri = format!("/api/tasks/{card}/link-picker?mode=child&q=zzz");
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(html.contains("No matching tasks."), "{html}");
    let uri = format!("/api/tasks/{card}/link-picker?mode=child");
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(
        html.contains("<span class=\"tm-picker-column\">Agentctl / Shipped</span>"),
        "{html}"
    );

    assert_eq!(
        picker_ids(&fx, &kid, "mode=child").await,
        vec![free_a, umbrella, free_b],
        "a task with an umbrella still takes children, minus that umbrella"
    );
}

#[tokio::test]
async fn the_link_picker_should_find_cards_on_other_boards_until_the_board_term_is_given() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let card = create_epic(&fx, "Card").await;
    let near = create_epic(&fx, "Near").await;
    let far = create(
        &fx,
        json!({"column_id": other_column, "name": "Far umbrella", "kind": "epic"}),
    )
    .await;

    assert_eq!(
        picker_ids_everywhere(
            &fx,
            &card,
            &format!("mode=parent&q=board%3A{}", fx.board_id)
        )
        .await,
        vec![near.clone()]
    );
    assert_eq!(
        picker_ids_everywhere(&fx, &card, "mode=parent&q=far").await,
        vec![far.clone()]
    );
    let all = picker_ids_everywhere(&fx, &card, "mode=parent").await;
    let at = |id: &String| all.iter().position(|x| x == id).expect("listed");
    assert!(at(&far) < at(&near), "{all:?}");
    let uri = format!("/api/tasks/{card}/link-picker?mode=parent&q=far");
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(
        html.contains("<span class=\"tm-picker-column\">Ops / "),
        "{html}"
    );

    link(&fx, &card, "parent", &far).await;
    let html = modal_html(&fx, &card).await;
    assert!(
        html.contains("<span class=\"tm-relation-board\">Ops</span>"),
        "{html}"
    );
    let html = modal_html(&fx, &far).await;
    assert!(
        html.contains("<span class=\"tm-relation-board\">Agentctl</span>"),
        "{html}"
    );
    let html = modal_html(&fx, &near).await;
    assert!(!html.contains("tm-relation-board"), "{html}");

    link(&fx, &card, "child", &near).await;
    let html = modal_html(&fx, &card).await;
    let child = html
        .split(&format!("data-child-id=\"{near}\""))
        .nth(1)
        .and_then(|rest| rest.split("</li>").next())
        .expect("the child row");
    assert!(!child.contains("tm-relation-board"), "{child}");
    let html = modal_html(&fx, &near).await;
    assert_eq!(html.matches("tm-relation-board").count(), 1, "{html}");
}

#[tokio::test]
async fn a_board_limited_token_should_never_see_unreadable_boards_in_the_link_picker() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let near = create_epic(&fx, "Near").await;
    let far = create(
        &fx,
        json!({"column_id": other_column, "name": "Far", "kind": "epic"}),
    )
    .await;

    let (mut record, limited) = new_api_token_record(&fx.user_id, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");

    let uri = format!("/api/tasks/{card}/link-picker?mode=parent");
    let (status, html) = request(&fx, As::Bearer(&limited), "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains(&near), "{html}");
    assert!(!html.contains(&far), "{html}");
    let uri = format!("/api/tasks/{card}/link-picker?mode=parent&q=board%3AOps");
    let (status, html) = request(&fx, As::Bearer(&limited), "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(!html.contains(&far), "{html}");
    assert!(!html.contains("Ops"), "{html}");
}

#[tokio::test]
async fn the_link_picker_should_list_the_tasks_a_parent_link_would_accept() {
    let (_dir, fx) = fixture();
    let card = create_epic(&fx, "Card").await;
    let current = create_epic(&fx, "Current").await;
    let other = create_epic(&fx, "Other").await;
    let sibling = create_epic(&fx, "Sibling").await;
    let child_of_other = create_epic(&fx, "Nested").await;
    link(&fx, &card, "parent", &current).await;
    link(&fx, &sibling, "parent", &current).await;
    link(&fx, &child_of_other, "parent", &other).await;

    let listed = picker_ids(&fx, &card, "mode=parent").await;
    assert_eq!(
        listed,
        vec![child_of_other.clone(), other.clone(), sibling.clone()],
        "any epic but the card and its current umbrella may be chosen"
    );

    let loose = create_epic(&fx, "Loose").await;
    let listed = picker_ids(&fx, &loose, "mode=parent").await;
    assert_eq!(
        listed,
        vec![
            card.clone(),
            current.clone(),
            child_of_other.clone(),
            other.clone(),
            sibling.clone()
        ]
    );

    assert_eq!(
        picker_ids(&fx, &current, "mode=parent").await,
        vec![loose, child_of_other, other],
        "an umbrella cannot go under its own children"
    );
}

#[tokio::test]
async fn the_link_picker_should_cap_the_list_refuse_a_bad_mode_and_stay_in_the_account() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    for n in 1..=22 {
        create(
            &fx,
            json!({"column_id": fx.col_todo, "name": format!("Bulk {n:02}")}),
        )
        .await;
    }
    let listed = picker_ids(&fx, &card, "mode=child").await;
    assert_eq!(listed.len(), 20);
    let filtered = picker_ids(&fx, &card, "mode=child&q=bulk").await;
    assert_eq!(filtered.len(), 20);
    let uri = format!("/api/tasks/{card}/link-picker?mode=child&q=bulk");
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(html.contains("Keep typing to narrow the list."), "{html}");
    let uri = format!("/api/tasks/{card}/link-picker?mode=child&q=bulk%2001");
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(!html.contains("Keep typing"), "{html}");

    let uri = format!("/api/tasks/{card}/link-picker?mode=sideways");
    let (status, text) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert_eq!(
        picker_ids(&fx, &card, "").await.len(),
        20,
        "a missing mode lists children"
    );

    let bob = fx.db.create_user("bob").expect("bob");
    let bob_board = fx.db.create_board(&bob, "Bob").expect("board");
    let bob_column = fx
        .db
        .create_column(&bob_board, "Todo", None)
        .expect("column");
    let theirs = fx
        .db
        .create_task(&bob_column, None, "Bulk theirs", 1, None)
        .expect("task");
    assert!(
        !picker_ids(&fx, &card, "mode=child&q=theirs")
            .await
            .contains(&theirs)
    );
    let uri = format!("/api/tasks/{theirs}/link-picker?mode=child");
    let (status, _) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_modal_should_list_relations_by_type_and_name_another_boards() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let local = create(&fx, json!({"column_id": fx.col_todo, "name": "Local"})).await;
    let neighbor = create(&fx, json!({"column_id": fx.col_todo, "name": "Neighbor"})).await;
    let remote = create(&fx, json!({"column_id": other_column, "name": "Remote"})).await;
    let plain = create(&fx, json!({"column_id": fx.col_todo, "name": "Plain"})).await;
    link(&fx, &local, "blocked_by", &remote).await;
    link(&fx, &local, "related", &neighbor).await;

    let html = modal_html(&fx, &local).await;
    assert!(html.contains(r#"id="tm-relations""#), "{html}");
    assert!(
        html.contains(r#"data-tm-tab="relations" aria-controls="tm-panel-relations" aria-selected="false" tabindex="-1">Relations &middot; 2</button>"#),
        "{html}"
    );
    let panel = html
        .find(r#"<section class="tm-panel tm-relations-sect""#)
        .expect("relations panel");
    let description = html
        .find(r#"id="tm-panel-description""#)
        .expect("description panel");
    assert!(description < panel, "{html}");
    assert!(
        !html[description..panel].contains("tm-relation"),
        "the Description tab must not hold relations: {html}"
    );
    assert!(
        html[panel..].contains("data-tm-act=\"add-relation\">Add relation</button>"),
        "{html}"
    );
    assert!(
        html.contains(r#"Relations <span class="tm-sect-count">2</span>"#),
        "{html}"
    );
    let depends = html.find("Depends on</h4>").expect("depends on group");
    let relates = html.find("Relates to</h4>").expect("relates to group");
    assert!(depends < relates, "{html}");
    assert!(
        html.contains(&format!(
            r#"data-open-task="{remote}" data-open-board="{other_board}""#
        )),
        "{html}"
    );
    assert_eq!(
        html.matches(r#"class="tm-relation-board""#).count(),
        1,
        "{html}"
    );
    assert!(
        html.contains(r#"class="tm-relation-board">Ops</span>"#),
        "{html}"
    );
    assert!(html.contains("data-relation-delete="), "{html}");

    let html = modal_html(&fx, &remote).await;
    assert!(html.contains("Required by</h4>"), "{html}");
    assert!(html.contains("tm-relation-board"), "{html}");

    let html = modal_html(&fx, &plain).await;
    assert!(!html.contains(r#"id="tm-relations""#), "{html}");
    assert!(html.contains("Relations &middot; 0</button>"), "{html}");
    assert!(html.contains("Add relation</button>"), "{html}");
}

#[tokio::test]
async fn a_relation_should_flip_between_depends_on_and_required_by_once_the_old_link_is_gone() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let (_, old) = link(&fx, &a, "blocked_by", &b).await;
    let (status, _) = link(&fx, &a, "blocks", &b).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let old_id = old["id"].as_str().expect("link id");
    let (status, _) = call(
        &fx,
        "DELETE",
        &format!("/api/tasks/{a}/links/{old_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = link(&fx, &a, "blocks", &b).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        links_of(&fx, &b).await,
        vec![("blocked_by".into(), a.clone())]
    );
}

#[tokio::test]
async fn a_second_relation_between_a_pair_should_be_refused_in_either_direction() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    let (status, first) = link(&fx, &a, "related", &b).await;
    assert_eq!(status, StatusCode::OK, "{first}");

    for (task, kind, target) in [
        (&a, "blocked_by", &b),
        (&a, "blocks", &b),
        (&b, "blocked_by", &a),
        (&b, "blocks", &a),
    ] {
        let (status, text) = link(&fx, task, kind, target).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}");
        assert_eq!(
            text,
            json!("the tasks are already related; delete that relation first"),
            "{kind}"
        );
    }
    assert_eq!(links_of(&fx, &a).await, vec![("related".into(), b.clone())]);

    let first_id = first["id"].as_str().expect("link id");
    let (status, _) = call(
        &fx,
        "DELETE",
        &format!("/api/tasks/{a}/links/{first_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = link(&fx, &b, "blocked_by", &a).await;
    assert_eq!(status, StatusCode::OK);
    let (status, text) = link(&fx, &a, "related", &b).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
}

#[tokio::test]
async fn a_reverse_dependency_should_be_refused_as_already_related() {
    let (_dir, fx) = fixture();
    let a = create(&fx, json!({"column_id": fx.col_todo, "name": "A"})).await;
    let b = create(&fx, json!({"column_id": fx.col_todo, "name": "B"})).await;
    assert_eq!(link(&fx, &a, "blocked_by", &b).await.0, StatusCode::OK);
    let (status, text) = link(&fx, &b, "blocked_by", &a).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        text,
        json!("the tasks are already related; delete that relation first")
    );
}

#[tokio::test]
async fn deleting_a_middle_umbrella_should_leave_its_children_loose_and_the_rest_nested() {
    let (_dir, fx) = fixture();
    let [root, mid, leaf, _] = nested_tree(&fx).await;
    let below = create(&fx, json!({"column_id": fx.col_todo, "name": "Below"})).await;
    let body = Some(json!({"kind": "epic"}));
    let (status, made) = call(&fx, "PATCH", &format!("/api/tasks/{leaf}"), body).await;
    assert_eq!(status, StatusCode::OK, "{made}");
    assert_eq!(link(&fx, &below, "parent", &leaf).await.0, StatusCode::OK);

    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{mid}"), None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(detail(&fx, &leaf).await["parent"], Value::Null);
    assert!(
        links_of(&fx, &leaf)
            .await
            .iter()
            .all(|(kind, _)| kind != "parent")
    );
    assert!(links_of(&fx, &root).await.is_empty());
    assert_eq!(detail(&fx, &root).await["children_total"], 0);
    let below_view = detail(&fx, &below).await;
    assert_eq!(below_view["parent"]["id"], leaf.as_str());
    let path: Vec<&str> = below_view["parent"]["path"]
        .as_array()
        .expect("path")
        .iter()
        .map(|part| part["id"].as_str().expect("id"))
        .collect();
    assert_eq!(path, [leaf.as_str()]);
}

#[tokio::test]
async fn time_entries_should_record_role_labels_start_seconds_and_token_name() {
    let (_dir, fx) = fixture();
    let (record, bot) = new_api_token_record(&fx.user_id, "agentctl-bot", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    let uri = format!("/api/tasks/{id}/time");
    let started = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();

    let body = json!({
        "seconds": 90,
        "started_at": started,
        "role": "  reviewer  ",
        "labels": ["audit"],
        "note": "second pass",
    });
    let (status, text) = request(&fx, As::Bearer(&bot), "POST", &uri, Some(body), true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let entries: Value = serde_json::from_str(&text).expect("json");
    let entry = &entries[0];
    assert_eq!(entry["role"], "reviewer");
    assert_eq!(entry["labels"], json!(["audit"]));
    assert_eq!(entry["seconds"], 90);
    assert_eq!(entry["minutes"], 1);
    assert_eq!(entry["token_name"], "agentctl-bot");
    assert_eq!(entry["note"], "second pass");
    let stored = chrono::DateTime::parse_from_rfc3339(entry["started_at"].as_str().expect("start"))
        .expect("rfc3339");
    let sent = chrono::DateTime::parse_from_rfc3339(&started).expect("rfc3339");
    assert_eq!(stored, sent);

    let (status, entries) = call(&fx, "POST", &uri, Some(json!({"minutes": 5}))).await;
    assert_eq!(status, StatusCode::OK);
    let plain = entries
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["seconds"] == 300)
        .expect("the browser entry");
    assert_eq!(plain["role"], Value::Null);
    assert_eq!(plain["token_name"], Value::Null);
    assert_eq!(plain["labels"], json!([]));

    let (status, entries) = call(&fx, "POST", &uri, Some(json!({"minutes": 2, "role": " "}))).await;
    assert_eq!(status, StatusCode::OK);
    let blank = entries
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["seconds"] == 120)
        .expect("the blank-role entry");
    assert_eq!(blank["role"], Value::Null, "a blank role is none");
}

#[tokio::test]
async fn timer_sessions_should_record_the_token_that_stopped_them() {
    let (_dir, fx) = fixture();
    let (record, bot) = new_api_token_record(&fx.user_id, "timer-bot", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    let running = || ActiveTimer {
        task_id: Some(id.clone()),
        mode: TimerMode::Stopwatch,
        started_at: chrono::Utc::now().timestamp() - 120,
        duration_secs: None,
    };

    fx.db
        .set_active_timer(&fx.user_id, &running())
        .expect("timer");
    let (status, text) = request(
        &fx,
        As::Bearer(&bot),
        "POST",
        "/api/timer/stop",
        Some(json!({})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    fx.db
        .set_active_timer(&fx.user_id, &running())
        .expect("timer");
    let (status, _) = call(&fx, "POST", "/api/timer/stop", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);

    let (status, entries) = call(&fx, "GET", &format!("/api/tasks/{id}/time"), None).await;
    assert_eq!(status, StatusCode::OK);
    let mut names: Vec<&Value> = entries
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| &entry["token_name"])
        .collect();
    names.sort_by_key(|name| name.is_null());
    assert_eq!(names, vec![&json!("timer-bot"), &Value::Null]);
}

#[tokio::test]
async fn time_entries_should_refuse_a_bad_duration_start_or_role() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    let uri = format!("/api/tasks/{id}/time");
    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let long_role = "r".repeat(65);
    let bodies = [
        json!({}),
        json!({"minutes": 5, "seconds": 300}),
        json!({"seconds": 0}),
        json!({"seconds": 86401}),
        json!({"seconds": 60, "started_at": "yesterday"}),
        json!({"seconds": 60, "started_at": future}),
        json!({"seconds": 60, "role": long_role}),
    ];
    for body in bodies {
        let (status, reply) = call(&fx, "POST", &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} gave {reply}");
    }
    assert!(fx.db.list_entries(&id).expect("entries").is_empty());

    let full = json!({"seconds": 86400, "role": "r".repeat(64)});
    let (status, reply) = call(&fx, "POST", &uri, Some(full)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

#[tokio::test]
async fn time_spent_should_group_minutes_by_role() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    let uri = format!("/api/tasks/{id}/time");
    for (minutes, role) in [
        (10, Some("reviewer")),
        (20, Some("lead")),
        (5, None),
        (15, Some("reviewer")),
    ] {
        let (status, reply) = call(
            &fx,
            "POST",
            &uri,
            Some(json!({"minutes": minutes, "role": role})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
    }

    let (status, reply) = call(
        &fx,
        "POST",
        &uri,
        Some(json!({"seconds": 30, "role": "quick"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let (status, report) = call(&fx, "GET", "/api/timer/time-spent", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        report["roles"],
        json!([
            {"role": "reviewer", "role_id": null, "minutes": 25, "seconds": 1500},
            {"role": "lead", "role_id": null, "minutes": 20, "seconds": 1200},
            {"role": "quick", "role_id": null, "minutes": 0, "seconds": 30},
            {"role": null, "role_id": null, "minutes": 5, "seconds": 300},
        ])
    );
    assert_eq!(report["total_minutes"], 50);
}

fn role_token(fx: &Fixture, role: &AgentRoleRow, name: &str) -> String {
    let (mut record, raw) = new_api_token_record(&fx.user_id, name, AuthScope::Work, None);
    record.permissions = role.permissions.clone();
    fx.db
        .create_owner_token_as_role(&role.id, record)
        .expect("token")
        .expect("issued as the role");
    raw
}

#[tokio::test]
async fn time_spent_should_group_a_role_under_its_current_name_in_one_line() {
    let (_dir, fx) = fixture();
    let role = role_of(&fx, "reviewer", &["tasks.read", "time.write"]);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    let uri = format!("/api/tasks/{id}/time");
    let before = role_token(&fx, &role, "before-rename");
    let (status, reply) = request(
        &fx,
        As::Bearer(&before),
        "POST",
        &uri,
        Some(json!({"minutes": 10})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    fx.db
        .update_agent_role(fx.user_id.as_str(), &role.id, Some("qa"), None)
        .expect("rename")
        .expect("role");
    let after = role_token(&fx, &role, "after-rename");
    let (status, reply) = request(
        &fx,
        As::Bearer(&after),
        "POST",
        &uri,
        Some(json!({"minutes": 5})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let (status, report) = call(&fx, "GET", "/api/timer/time-spent", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        report["roles"],
        json!([{"role": "qa", "role_id": role.id, "minutes": 15, "seconds": 900}])
    );
}

#[tokio::test]
async fn time_spent_should_keep_entries_without_a_role_id_under_their_stored_name() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    let uri = format!("/api/tasks/{id}/time");
    for (minutes, role) in [(10, Some("reviewer")), (5, None)] {
        let (status, reply) = call(
            &fx,
            "POST",
            &uri,
            Some(json!({"minutes": minutes, "role": role})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
    }

    let (status, report) = call(&fx, "GET", "/api/timer/time-spent", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        report["roles"],
        json!([
            {"role": "reviewer", "role_id": null, "minutes": 10, "seconds": 600},
            {"role": null, "role_id": null, "minutes": 5, "seconds": 300},
        ])
    );
}

#[tokio::test]
async fn time_spent_should_show_the_stored_name_of_a_deleted_role() {
    let (_dir, fx) = fixture();
    let role = role_of(&fx, "reviewer", &["tasks.read", "time.write"]);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    let uri = format!("/api/tasks/{id}/time");
    let token = role_token(&fx, &role, "logger");
    let (status, reply) = request(
        &fx,
        As::Bearer(&token),
        "POST",
        &uri,
        Some(json!({"minutes": 10})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    fx.db
        .delete_agent_role(fx.user_id.as_str(), &role.id)
        .expect("delete")
        .expect("role");

    let (status, report) = call(&fx, "GET", "/api/timer/time-spent", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        report["roles"],
        json!([{"role": "reviewer", "role_id": role.id, "minutes": 10, "seconds": 600}])
    );
}

#[test]
fn time_entries_from_before_roles_should_read_back_without_them() {
    let old = json!({
        "id": "e1",
        "task_id": "t1",
        "minutes": 5,
        "note": "",
        "started_at": "2026-10-01T10:00:00+00:00",
    });
    let row: TimeEntryRow = serde_json::from_value(old).expect("old row");
    assert_eq!(row.role, None);
    assert_eq!(row.token_name, None);
    assert!(row.labels.is_empty());
    assert_eq!(row.duration_seconds(), 300);
}

#[tokio::test]
async fn time_entries_logged_before_roles_should_list_without_them() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Timed"})).await;
    fx.db.create_entry(&id, 7, "old").expect("entry");
    let (status, entries) = call(&fx, "GET", &format!("/api/tasks/{id}/time"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entries[0]["role"], Value::Null);
    assert_eq!(entries[0]["token_name"], Value::Null);
    assert_eq!(entries[0]["minutes"], 7);
}

#[tokio::test]
async fn the_first_added_member_should_become_responsible_and_show_in_the_api_and_modal() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let (_, task) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(task["responsible"], Value::Null);

    let admin = fx.user_id.as_str();
    let (status, task) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"member_ids": [admin]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(task["responsible"], json!({"kind": "member", "id": admin}));

    let html = modal_html(&fx, &id).await;
    assert!(html.contains("tm-member-responsible"), "{html}");
    assert!(html.contains("(Responsible)"), "{html}");

    let (_, listed) = call_list(
        &fx,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert_eq!(
        listed[0]["responsible"],
        json!({"kind": "member", "id": admin})
    );
}

#[tokio::test]
async fn patch_should_set_a_member_or_a_session_responsible_and_clear_it_with_null() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let uri = format!("/api/tasks/{id}");
    let admin = fx.user_id.as_str();
    call(&fx, "PATCH", &uri, Some(json!({"member_ids": [admin]}))).await;

    let (status, task) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "session", "name": "  coordinator  "}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(
        task["responsible"],
        json!({"kind": "session", "name": "coordinator"})
    );

    let (status, task) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "member", "id": admin}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(task["responsible"], json!({"kind": "member", "id": admin}));

    let (_, task) = call(&fx, "PATCH", &uri, Some(json!({"name": "Renamed"}))).await;
    assert_eq!(task["responsible"], json!({"kind": "member", "id": admin}));

    let (status, task) = call(&fx, "PATCH", &uri, Some(json!({"responsible": null}))).await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(task["responsible"], Value::Null);
}

#[tokio::test]
async fn patch_should_reject_a_responsible_member_who_is_not_on_the_task() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let uri = format!("/api/tasks/{id}");
    let admin = fx.user_id.as_str();

    let (status, body) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "member", "id": admin}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "session", "name": "   "}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (_, task) = call(&fx, "GET", &uri, None).await;
    assert_eq!(task["responsible"], Value::Null);
}

#[tokio::test]
async fn patch_should_refuse_a_responsible_body_that_mixes_the_fields_of_both_kinds() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let uri = format!("/api/tasks/{id}");
    let admin = fx.user_id.as_str();
    call(&fx, "PATCH", &uri, Some(json!({"member_ids": [admin]}))).await;

    let (status, body) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "member", "id": admin, "name": "Ada"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "session", "name": "coordinator", "id": admin}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (_, task) = call(&fx, "GET", &uri, None).await;
    assert_eq!(task["responsible"], json!({"kind": "member", "id": admin}));
}

#[tokio::test]
async fn removing_the_responsible_member_should_clear_it_when_no_member_remains() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let uri = format!("/api/tasks/{id}");
    let admin = fx.user_id.as_str();
    call(&fx, "PATCH", &uri, Some(json!({"member_ids": [admin]}))).await;

    let (status, task) = call(&fx, "PATCH", &uri, Some(json!({"member_ids": []}))).await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(task["responsible"], Value::Null);
}

#[tokio::test]
async fn a_responsible_session_should_outlive_the_token_that_set_it() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let uri = format!("/api/tasks/{id}");
    let (record, raw) = new_api_token_record(&fx.user_id, "child", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");

    let (status, body) = request(
        &fx,
        As::Bearer(&raw),
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "session", "name": "coordinator"}})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        fx.db
            .delete_api_token(&fx.user_id, &record.id)
            .expect("revoke")
    );

    let (status, _) = request(&fx, As::Bearer(&raw), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, task) = call(&fx, "GET", &uri, None).await;
    assert_eq!(
        task["responsible"],
        json!({"kind": "session", "name": "coordinator"})
    );
    let html = modal_html(&fx, &id).await;
    assert!(html.contains("tm-member-session"), "{html}");
    assert!(html.contains("coordinator"), "{html}");
}

#[tokio::test]
async fn listing_attachments_should_show_each_with_its_uploader_and_token() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Files"})).await;
    let uri = format!("/api/tasks/{id}/attachments");
    let (status, empty) = call(&fx, "GET", &uri, None).await;
    assert_eq!((status, &empty), (StatusCode::OK, &json!([])));

    let (record, work) = new_api_token_record(&fx.user_id, "uploader", AuthScope::Work, None);
    fx.db.create_api_token(&record).expect("token");
    let first = json!({"name": "a.txt", "mime": "text/plain", "data": "aGVsbG8="});
    let (status, _) = call(&fx, "POST", &uri, Some(first)).await;
    assert_eq!(status, StatusCode::OK);
    let second = json!({"name": "b.bin", "data": "AAEC"});
    let (status, text) = request(&fx, As::Bearer(&work), "POST", &uri, Some(second), true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let uploaded: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(uploaded["uploaded_by"], "admin");
    assert_eq!(uploaded["token_name"], "uploader");

    let (status, listed) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed.as_array().expect("array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "a.txt");
    assert_eq!(rows[0]["mime"], "text/plain");
    assert_eq!(rows[0]["size"], 5);
    assert_eq!(rows[0]["uploaded_by"], "admin");
    assert_eq!(rows[0]["token_name"], Value::Null);
    assert!(rows[0]["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(rows[0]["created_at"].as_str().is_some());
    assert_eq!(rows[1]["name"], "b.bin");
    assert_eq!(rows[1]["size"], 3);
    assert_eq!(rows[1]["uploaded_by"], "admin");
    assert_eq!(rows[1]["token_name"], "uploader");

    let (status, text) = request(&fx, As::Bearer(&fx.read_token), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, _) = request(&fx, As::Anonymous, "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&fx, "GET", "/api/tasks/nope/attachments", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn modal_should_show_the_account_handle_for_a_token_upload() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Files"})).await;
    let uri = format!("/api/tasks/{id}/attachments");
    let (record, work) = new_api_token_record(&fx.user_id, "uploader", AuthScope::Work, None);
    fx.db.create_api_token(&record).expect("token");
    let first = json!({"name": "a.txt", "data": "aGVsbG8="});
    let (status, text) = request(&fx, As::Bearer(&work), "POST", &uri, Some(first), true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let second = json!({"name": "b.txt", "data": "d29ybGQ="});
    let (status, _) = call(&fx, "POST", &uri, Some(second)).await;
    assert_eq!(status, StatusCode::OK);

    let modal = format!("/api/tasks/{id}/modal");
    let (status, body) = request(&fx, As::Admin, "GET", &modal, None, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.matches("tm-attachment-details\">admin, ").count(), 2);
    assert!(
        !body.contains("tm-attachment-details\">api-token"),
        "{body}"
    );
}

#[tokio::test]
async fn listing_attachments_should_show_a_board_limited_token_only_its_boards() {
    let (_dir, fx) = fixture();
    let mine = create(&fx, json!({"column_id": fx.col_todo, "name": "Mine"})).await;
    let other_board = fx
        .db
        .create_board(&fx.user_id, "Other")
        .expect("other board");
    let other_column = fx
        .db
        .create_column(&other_board, "Todo", None)
        .expect("column");
    let theirs = fx
        .db
        .create_task(&other_column, None, "Theirs", 1, None)
        .expect("task");
    let file = json!({"name": "a.txt", "mime": "text/plain", "data": "aGVsbG8="});
    for id in [&mine, &theirs] {
        let uri = format!("/api/tasks/{id}/attachments");
        let (status, _) = call(&fx, "POST", &uri, Some(file.clone())).await;
        assert_eq!(status, StatusCode::OK);
    }

    let (mut record, limited) = new_api_token_record(&fx.user_id, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");

    let uri = format!("/api/tasks/{mine}/attachments");
    let (status, text) = request(&fx, As::Bearer(&limited), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("a.txt"), "{text}");
    let uri = format!("/api/tasks/{theirs}/attachments");
    let (status, text) = request(&fx, As::Bearer(&limited), "GET", &uri, None, true).await;
    assert!(!text.contains("a.txt"), "{text}");
    assert_board_denied((status, text), "attachments.read", &other_board).await;
    let (status, text) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn attachment_list_should_appear_in_the_openapi_spec_and_the_api_docs() {
    let spec: Value =
        serde_json::from_str(&ApiDoc::openapi().to_json().expect("json")).expect("spec");
    let operation = &spec["paths"]["/api/tasks/{id}/attachments"]["get"];
    assert_eq!(operation["operationId"], "list_attachments");
    let (_dir, fx) = fixture();
    let (status, docs) = request(&fx, As::Anonymous, "GET", "/docs/api.md", None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(docs.contains("list_attachments"), "{docs}");
}

#[tokio::test]
async fn only_an_admin_token_should_detach_an_attachment_and_free_its_storage() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "worker", AuthScope::Work);
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let admin = token_with(&fx, "boss", AuthScope::Admin);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let data = base64::engine::general_purpose::STANDARD.encode(vec![7u8; 4096]);
    let (_, attachment) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/attachments"),
        Some(json!({"name": "a.bin", "data": data})),
    )
    .await;
    let uri = format!(
        "/api/tasks/{id}/attachments/{}",
        attachment["id"].as_str().expect("id")
    );
    let stored = |fx: &Fixture| {
        fx.db
            .stored_bytes_by_account()
            .expect("usage")
            .values()
            .sum::<u64>()
    };
    let before = stored(&fx);
    assert!(before >= 4096, "{before}");

    for token in [&work, &lead] {
        let (status, text) = request(&fx, As::Bearer(token), "DELETE", &uri, None, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            permission_denied("attachments.delete")
        );
    }
    assert_eq!(
        fx.db
            .task(&id)
            .expect("read")
            .expect("task")
            .attachments
            .len(),
        1
    );
    assert_eq!(stored(&fx), before);

    let (status, text) = request(&fx, As::Bearer(&admin), "DELETE", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let row = fx.db.task(&id).expect("read").expect("task");
    assert!(row.attachments.is_empty());
    assert!(
        row.history
            .iter()
            .any(|event| event.kind == HistoryKind::AttachmentDeleted)
    );
    assert!(
        stored(&fx) + 4096 <= before,
        "{} from {before}",
        stored(&fx)
    );
}

fn responsible_events(fx: &Fixture, id: &str) -> Vec<String> {
    let row = fx.db.task(id).expect("read").expect("task");
    row.history
        .iter()
        .filter(|event| event.kind == HistoryKind::ResponsibleUpdated)
        .map(|event| event.detail.clone())
        .collect()
}

#[tokio::test]
async fn a_responsible_change_should_show_in_history() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let uri = format!("/api/tasks/{id}");
    let admin = fx.user_id.as_str();
    call(&fx, "PATCH", &uri, Some(json!({"member_ids": [admin]}))).await;
    assert_eq!(responsible_events(&fx, &id).len(), 1);

    call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"responsible": {"kind": "session", "name": "coordinator"}})),
    )
    .await;
    call(&fx, "PATCH", &uri, Some(json!({"name": "Renamed"}))).await;
    call(&fx, "PATCH", &uri, Some(json!({"responsible": null}))).await;

    let events = responsible_events(&fx, &id);
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[1], "Set the responsible to coordinator");
    assert_eq!(events[2], "Cleared the responsible");
}

#[tokio::test]
async fn the_automatic_responsible_change_on_members_should_show_in_history() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Own"})).await;
    let uri = format!("/api/tasks/{id}");
    let admin = fx.user_id.as_str();

    call(&fx, "PATCH", &uri, Some(json!({"member_ids": [admin]}))).await;
    let events = responsible_events(&fx, &id);
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(
        events[0].starts_with("Set the responsible to "),
        "{events:?}"
    );

    call(&fx, "PATCH", &uri, Some(json!({"member_ids": []}))).await;
    let events = responsible_events(&fx, &id);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1], "Cleared the responsible");
}

/// Posts `body` to the token create route as the browser, and returns the reply.
async fn mint(fx: &Fixture, body: Value) -> (StatusCode, Value) {
    call(fx, "POST", "/api/v1/auth/tokens", Some(body)).await
}

/// Posts `token` to the inspect route as `who`, and returns the status and raw reply.
async fn inspect(fx: &Fixture, who: As<'_>, token: &str) -> (StatusCode, String) {
    let body = json!({ "token": token });
    request(
        fx,
        who,
        "POST",
        "/api/v1/auth/tokens/inspect",
        Some(body),
        true,
    )
    .await
}

#[tokio::test]
async fn a_token_should_be_created_from_a_preset_with_boards() {
    let (_dir, fx) = fixture();
    let (other, _, task) = other_board(&fx);
    let (status, token) = mint(
        &fx,
        json!({"name": "lead", "preset": "lead", "boards": [fx.board_id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{token}");
    let raw = token["raw_token"].as_str().expect("raw").to_string();
    assert_eq!(
        token["boards"],
        json!([{"id": fx.board_id, "name": "Agentctl"}])
    );
    assert_eq!(token["scopes"], json!(["provision"]));

    let granted = format!("/api/boards/{}", fx.board_id);
    let (status, text) = request(&fx, As::Bearer(&raw), "GET", &granted, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = request(
        &fx,
        As::Bearer(&raw),
        "GET",
        &format!("/api/tasks/{task}"),
        None,
        true,
    )
    .await;
    assert_board_denied((status, text), "tasks.read", &other).await;
}

#[tokio::test]
async fn a_token_should_be_created_from_a_custom_set_and_hold_only_it() {
    let (_dir, fx) = fixture();
    let (status, token) = mint(
        &fx,
        json!({"name": "custom", "permissions": ["tasks.read", "comments.write"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{token}");
    assert_eq!(
        token["permissions"],
        json!(["tasks.read", "comments.write"])
    );
    assert_eq!(token["boards"], "all");
    let raw = token["raw_token"].as_str().expect("raw");

    let task = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let read = format!("/api/tasks/{task}");
    let (status, text) = request(&fx, As::Bearer(raw), "GET", &read, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let body = json!({"column_id": fx.col_todo, "name": "Refused"});
    let (status, text) = bearer_json(&fx, raw, "POST", "/api/tasks", body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        permission_denied("tasks.create")
    );
}

#[tokio::test]
async fn a_custom_set_should_grant_only_its_own_permissions_at_any_label() {
    let (_dir, fx) = fixture();
    let every_read: Vec<_> = AuthScope::Read
        .permissions()
        .iter()
        .map(|held| held.name())
        .chain(["boards.delete"])
        .collect();
    let (status, token) = mint(&fx, json!({"name": "wide", "permissions": every_read})).await;
    assert_eq!(status, StatusCode::OK, "{token}");
    assert_eq!(token["scopes"], json!(["read"]));
    let stored = fx
        .db
        .find_api_token(token["raw_token"].as_str().expect("raw"))
        .expect("find")
        .expect("token");
    assert_eq!(stored.scope, AuthScope::Read);
    assert!(stored.permissions.contains(&Permission::BoardsDelete));
    assert!(!stored.permissions.contains(&Permission::TasksCreate));
}

#[tokio::test]
async fn token_create_should_take_exactly_one_grant() {
    let (_dir, fx) = fixture();
    for body in [
        json!({"name": "none"}),
        json!({"name": "two", "scopes": ["read"], "preset": "read"}),
        json!({"name": "two", "scopes": ["read"], "permissions": ["tasks.read"]}),
        json!({"name": "two", "preset": "read", "permissions": ["tasks.read"]}),
        json!({"name": "all", "scopes": ["read"], "preset": "read", "permissions": []}),
    ] {
        let (status, reply) = mint(&fx, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
    }
}

#[tokio::test]
async fn token_create_should_refuse_an_unknown_preset_or_an_unusable_set() {
    let (_dir, fx) = fixture();
    for body in [
        json!({"name": "bad", "preset": "provision"}),
        json!({"name": "bad", "preset": "root"}),
        json!({"name": "bad", "permissions": []}),
        json!({"name": "bad", "permissions": ["tasks.read", "tasks.everything"]}),
        json!({"name": "bad", "preset": "read", "boards": []}),
        json!({"name": "bad", "preset": "read", "boards": ["no-such-board"]}),
        json!({"name": "bad", "preset": "read", "boards": "some"}),
    ] {
        let before = fx.db.list_account_api_tokens(&fx.user_id).expect("tokens");
        let (status, reply) = mint(&fx, body.clone()).await;
        assert!(status.is_client_error(), "{body}: {status} {reply}");
        let after = fx.db.list_account_api_tokens(&fx.user_id).expect("tokens");
        assert_eq!(before.len(), after.len(), "{body}");
    }
}

#[tokio::test]
async fn token_create_should_stay_session_only_with_a_grant() {
    let (_dir, fx) = fixture();
    let admin = token_with(&fx, "admin", AuthScope::Admin);
    let body = json!({"name": "sneaky", "preset": "admin"});
    let (status, text) = bearer_json(&fx, &admin, "POST", "/api/v1/auth/tokens", body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
}

#[tokio::test]
async fn inspect_should_show_the_grant_and_never_echo_the_secret() {
    let (_dir, fx) = fixture();
    let (_, raw) = assigned_token(&fx, AuthScope::Work, &[&fx.board_id]);
    let (status, text) = inspect(&fx, As::Admin, &raw).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!text.contains(&raw), "{text}");
    assert!(!text.contains("raw_token"), "{text}");
    let reply: Value = serde_json::from_str(&text).expect("json");
    let expected: Vec<_> = AuthScope::Work
        .permissions()
        .iter()
        .map(|held| held.name())
        .collect();
    assert_eq!(reply["permissions"], json!(expected));
    assert_eq!(
        reply["boards"],
        json!([{"id": fx.board_id, "name": "Agentctl"}])
    );
}

#[tokio::test]
async fn inspect_should_refuse_a_bearer_token_even_on_itself() {
    let (_dir, fx) = fixture();
    let admin = token_with(&fx, "admin", AuthScope::Admin);
    let (status, text) = inspect(&fx, As::Bearer(&admin), &admin).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(!text.contains("permissions"), "{text}");
    let (status, _) = inspect(&fx, As::Anonymous, &admin).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_edit_should_rename_without_a_new_secret_or_a_new_grant() {
    let (_dir, fx) = fixture();
    let (id, raw) = token_record(&fx, "before", AuthScope::Read);
    let uri = format!("/api/v1/auth/tokens/{id}");
    let (status, before) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    let (status, reply) = call(&fx, "PATCH", &uri, Some(json!({"name": "  after  "}))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["name"], "after");
    assert_eq!(reply["permissions"], before["permissions"]);
    assert!(reply.get("raw_token").is_none(), "{reply}");
    assert!(!reply.to_string().contains(&raw), "{reply}");

    let (status, text) = inspect(&fx, As::Admin, &raw).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let inspected: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(inspected["id"], json!(id));
    assert_eq!(inspected["name"], "after");
    assert_eq!(inspected["permissions"], before["permissions"]);

    let (status, shown) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["name"], "after");
    assert_eq!(shown["permissions"], before["permissions"]);
}

#[tokio::test]
async fn token_edit_should_keep_the_boards_and_the_expiry() {
    let (_dir, fx) = fixture();
    let (status, token) = mint(
        &fx,
        json!({"name": "scoped", "preset": "read", "boards": [fx.board_id], "expires_in_days": 30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{token}");
    let uri = format!("/api/v1/auth/tokens/{}", token["id"].as_str().expect("id"));
    let (status, edited) = call(&fx, "PATCH", &uri, Some(json!({"name": "renamed"}))).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["boards"], token["boards"]);
    assert_eq!(edited["expires_at"], token["expires_at"]);
    assert_eq!(edited["permissions"], token["permissions"]);
    assert!(edited["expires_at"].is_number(), "{edited}");
}

#[tokio::test]
async fn token_edit_should_refuse_a_grant_change_and_leave_the_token_unchanged() {
    let (_dir, fx) = fixture();
    let (id, _) = token_record(&fx, "steady", AuthScope::Read);
    let uri = format!("/api/v1/auth/tokens/{id}");
    let (status, before) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    for (field, body) in [
        ("preset", json!({"name": "wider", "preset": "admin"})),
        ("permissions", json!({"permissions": ["tasks.create"]})),
        ("scopes", json!({"scopes": ["admin"]})),
        ("boards", json!({"boards": [fx.board_id]})),
        ("expires_in_days", json!({"expires_in_days": 1})),
        ("expires_at", json!({"expires_at": 1})),
    ] {
        let (status, reply) = call(&fx, "PATCH", &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
        assert!(reply.to_string().contains(field), "{body}: {reply}");
        let (status, shown) = call(&fx, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{shown}");
        assert_eq!(shown, before, "{body}");
    }
}

#[tokio::test]
async fn token_edit_should_keep_names_unique_on_the_account() {
    let (_dir, fx) = fixture();
    token_record(&fx, "Taken", AuthScope::Read);
    let (id, _) = token_record(&fx, "mine", AuthScope::Read);
    let uri = format!("/api/v1/auth/tokens/{id}");
    let (status, reply) = call(&fx, "PATCH", &uri, Some(json!({"name": "taken"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    let (status, shown) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["name"], "mine");
    let (status, reply) = call(&fx, "PATCH", &uri, Some(json!({"name": "MINE"}))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["name"], "MINE");
}

#[tokio::test]
async fn token_edit_should_accept_an_unchanged_name_next_to_a_same_named_token() {
    let (_dir, fx) = fixture();
    token_record(&fx, "ci", AuthScope::Read);
    let (id, _) = token_record(&fx, "ci", AuthScope::Read);
    let uri = format!("/api/v1/auth/tokens/{id}");
    let (status, reply) = call(&fx, "PATCH", &uri, Some(json!({"name": "ci"}))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["child_lifetime_hours"], 1);
}

#[tokio::test]
async fn token_edit_should_refuse_a_bad_name_and_change_nothing() {
    let (_dir, fx) = fixture();
    let (id, _) = token_record(&fx, "steady", AuthScope::Read);
    let uri = format!("/api/v1/auth/tokens/{id}");
    for body in [json!({"name": "   "}), json!({"name": "x".repeat(81)})] {
        let (status, reply) = call(&fx, "PATCH", &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
    }
    let (status, shown) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["name"], "steady");
}

#[tokio::test]
async fn token_edit_should_answer_an_unknown_id_with_404() {
    let (_dir, fx) = fixture();
    let (status, reply) = call(
        &fx,
        "PATCH",
        "/api/v1/auth/tokens/no-such-token",
        Some(json!({"name": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{reply}");
    let (status, reply) = call(&fx, "GET", "/api/v1/auth/tokens/no-such-token", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{reply}");
}

#[tokio::test]
async fn token_edit_should_stay_session_only() {
    let (_dir, fx) = fixture();
    let admin = token_with(&fx, "admin", AuthScope::Admin);
    let (id, _) = token_record(&fx, "target", AuthScope::Read);
    let uri = format!("/api/v1/auth/tokens/{id}");
    let body = json!({"name": "hijack"});
    let (status, text) = bearer_json(&fx, &admin, "PATCH", &uri, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let (status, text) = request(&fx, As::Bearer(&admin), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let (status, _) = request(&fx, As::Anonymous, "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, shown) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["name"], "target");
}

#[tokio::test]
async fn inspect_should_answer_an_unknown_and_a_revoked_token_alike() {
    let (_dir, fx) = fixture();
    let (id, raw) = token_record(&fx, "gone", AuthScope::Read);
    let (status, unknown) = inspect(&fx, As::Admin, "pomb_no_such_token").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");

    let uri = format!("/api/v1/auth/tokens/{id}");
    let (status, text) = request(&fx, As::Admin, "DELETE", &uri, None, true).await;
    assert!(status.is_success(), "{text}");
    let (status, revoked) = inspect(&fx, As::Admin, &raw).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{revoked}");
    assert_eq!(unknown, revoked);
}

#[tokio::test]
async fn inspect_should_refuse_the_bootstrap_token() {
    const BOOTSTRAP: &str = "pomb_inspect_grant_bootstrap";
    let (_dir, fx) = fixture();
    let (status, unknown) = inspect(&fx, As::Admin, "pomb_no_such_token").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");

    let state = AppState::new(
        fx.db.clone(),
        BootstrapToken::new(BOOTSTRAP),
        passkey_mode(),
    );
    let response = router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/auth/tokens/inspect")
                .header("cookie", &fx.cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({ "token": BOOTSTRAP }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert_eq!(text, unknown);
    assert!(!text.contains(BOOTSTRAP), "{text}");
}

#[tokio::test]
async fn a_blocker_outside_the_set_should_not_show_on_the_task() {
    let (_dir, fx) = fixture();
    let (_, _, far_task) = other_board(&fx);
    let near = create(&fx, json!({"column_id": fx.col_todo, "name": "Near"})).await;
    let (status, link) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{near}/links"),
        Some(json!({"kind": "blocked_by", "target_id": far_task})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{link}");
    let uri = format!("/api/tasks/{near}");
    assert_eq!(call(&fx, "GET", &uri, None).await.1["blocked"], true);

    let (_, assigned) = assigned_token(&fx, AuthScope::Read, &[&fx.board_id]);
    let (status, text) = request(&fx, As::Bearer(&assigned), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let task: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(task["blocked"], false);
}

/// Links a new task on the fixture board from a task on another board, and returns that board,
/// its task and the new task.
async fn linked_from_elsewhere(fx: &Fixture) -> (String, String, String) {
    let (far, _, far_task) = other_board(fx);
    let near = create(fx, json!({"column_id": fx.col_todo, "name": "Near"})).await;
    let (status, link) = call(
        fx,
        "POST",
        &format!("/api/tasks/{far_task}/links"),
        Some(json!({"kind": "blocked_by", "target_id": near})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{link}");
    (far, far_task, near)
}

#[tokio::test]
async fn deleting_a_task_linked_from_outside_the_set_should_be_refused() {
    let (_dir, fx) = fixture();
    let (far, far_task, near) = linked_from_elsewhere(&fx).await;
    let (_, admin) = assigned_token(&fx, AuthScope::Admin, &[&fx.board_id]);

    let uri = format!("/api/tasks/{near}");
    let answer = request(&fx, As::Bearer(&admin), "DELETE", &uri, None, true).await;
    assert_board_denied(answer, "tasks.bin", &far).await;

    assert!(fx.db.task(&near).expect("read").is_some());
    let held = fx.db.task(&far_task).expect("read").expect("task");
    assert_eq!(held.links.len(), 1);
}

#[tokio::test]
async fn deleting_a_board_linked_from_outside_the_set_should_be_refused() {
    let (_dir, fx) = fixture();
    let (far, far_task, near) = linked_from_elsewhere(&fx).await;
    let (_, admin) = assigned_token(&fx, AuthScope::Admin, &[&fx.board_id]);

    let uri = format!("/api/boards/{}", fx.board_id);
    let answer = request(&fx, As::Bearer(&admin), "DELETE", &uri, None, true).await;
    assert_board_denied(answer, "boards.delete", &far).await;

    assert!(fx.db.task(&near).expect("read").is_some());
    let held = fx.db.task(&far_task).expect("read").expect("task");
    assert_eq!(held.links.len(), 1);
}

#[tokio::test]
async fn deleting_a_task_linked_only_inside_the_set_should_work() {
    let (_dir, fx) = fixture();
    let first = create(&fx, json!({"column_id": fx.col_todo, "name": "First"})).await;
    let second = create(&fx, json!({"column_id": fx.col_todo, "name": "Second"})).await;
    let (status, link) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{first}/links"),
        Some(json!({"kind": "related", "target_id": second})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{link}");
    let (_, admin) = assigned_token(&fx, AuthScope::Admin, &[&fx.board_id]);

    let uri = format!("/api/tasks/{second}");
    let (status, text) = request(&fx, As::Bearer(&admin), "DELETE", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(fx.db.task(&second).expect("read").is_none());
}

#[tokio::test]
async fn the_board_page_should_hold_the_calendar_dialog_with_its_top_bar() {
    let (_dir, fx) = fixture();
    let (status, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/b/{}", fx.board_id),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dialog = &html[html
        .find("id=\"calendar-dialog\"")
        .expect("calendar dialog")..];
    let dialog = &dialog[..dialog.find("id=\"calendar-grid\"").expect("grid")];
    for id in [
        "calendar-filter-toggle",
        "calendar-feed-toggle",
        "calendar-export",
        "calendar-title",
        "calendar-settings",
        "calendar-prev",
        "calendar-next",
        "calendar-today",
        "calendar-filter-panel",
        "calendar-user-filter",
        "calendar-swimlane-filter",
        "calendar-boards-toggle",
        "calendar-boards-list",
        "calendar-feed-label",
        "calendar-feed-generate",
        "calendar-feed-list",
        "calendar-export-panel",
        "calendar-export-format",
        "calendar-export-from",
        "calendar-export-to",
        "calendar-export-type",
        "calendar-export-run",
        "calendar-settings-panel",
        "calendar-first-day",
        "calendar-show-repeats",
    ] {
        assert!(dialog.contains(&format!("id=\"{id}\"")), "{id}");
    }
    assert!(dialog.contains("<option value=\"__unassigned__\">Unassigned</option>"));
    assert!(!dialog.contains("id=\"calendar-export\" class=\"cal-tool\" disabled"));
    assert!(!dialog.contains("aria-label=\"Calendar settings\" disabled"));
    assert!(dialog.contains("id=\"calendar-close\" class=\"cal-icon\" data-close-dialog"));
    assert!(dialog.contains("class=\"dlg-overlay cal-overlay\" hidden"));
}

#[tokio::test]
async fn the_board_page_should_hold_the_copy_task_dialog_and_its_menu_item() {
    let (_dir, fx) = fixture();
    let (status, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/b/{}", fx.board_id),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dialog = &html[html.find("id=\"copy-task-dialog\"").expect("copy dialog")..];
    let dialog = &dialog[..dialog.find("id=\"cp-copy-btn\"").expect("copy button")];
    for text in [
        "Copy task",
        "Name of copy",
        "Target column",
        "id=\"cp-name\"",
        "id=\"cp-col\"",
    ] {
        assert!(dialog.contains(text), "{text}");
    }
    assert!(html.contains("data-card-act=\"copy\""));
}

#[tokio::test]
async fn the_board_page_should_hold_the_print_board_dialog_with_its_filters() {
    let (_dir, fx) = fixture();
    let (status, html) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/b/{}", fx.board_id),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dialog = &html[html
        .find("id=\"print-board-dialog\"")
        .expect("print dialog")..];
    let dialog = &dialog[..dialog.find("id=\"print-board-sheet\"").expect("sheet")];
    for id in [
        "print-board-filter",
        "print-board-now",
        "print-board-columns",
        "print-board-props",
        "print-board-user",
        "print-board-color",
        "print-board-label",
        "print-board-period",
    ] {
        assert!(dialog.contains(&format!("id=\"{id}\"")), "{id}");
    }
}

#[tokio::test]
async fn export_data_should_say_which_columns_are_done() {
    let (_dir, fx) = fixture();
    let (status, data) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/export-data", fx.board_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{data}");
    let columns = data["columns"].as_array().expect("columns");
    let flag = |id: &str| {
        columns
            .iter()
            .find(|column| column["id"] == id)
            .map(|column| column["is_done"].clone())
    };
    assert_eq!(flag(&fx.col_done), Some(json!(true)));
    assert_eq!(flag(&fx.col_todo), Some(json!(false)));
}

#[tokio::test]
async fn the_calendar_should_no_longer_be_a_page() {
    let (_dir, fx) = fixture();
    let (status, _) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/b/{}/calendar", fx.board_id),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let app = include_str!("../../static/app.js");
    assert!(!app.contains("+ '/calendar';"));
    assert!(app.contains("CalendarPage.open(boardId());"));
}

#[test]
fn the_calendar_grid_should_have_fixed_rows_and_scroll_inside_the_dialog() {
    let css = include_str!("../../static/style.css");
    let rule = |selector: &str| {
        let rest = &css[css.find(selector).expect(selector)..];
        &rest[..rest.find('}').expect("end")]
    };
    let grid = rule(".calendar-grid {");
    assert!(grid.contains("repeat(7, minmax(0, 1fr))"));
    assert!(grid.contains("grid-auto-rows: 120px"));
    assert!(grid.contains("grid-template-rows: 28px"));
    assert!(rule(".log-custom-fields input {").contains("min-width: 0"));
    assert!(rule(".cal-scroll {").contains("overflow: auto"));
    assert!(rule(".cal-day {").contains("height: 120px"));
    assert!(rule(".cal-day.cal-today {").contains("outline: 2px solid var(--today-accent)"));
    assert!(rule(".cal-dialog {").contains("height: 100%"));
}

#[tokio::test]
async fn the_modal_should_offer_take_with_a_timed_menu_on_an_unowned_task() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Free"})).await;
    let html = modal_html(&fx, &id).await;
    assert!(html.contains(r#"id="tm-group-owner""#));
    assert!(html.contains(r#"data-owner-act="owner-take">Take</button>"#));
    assert!(html.contains(r#"data-ttl="14400""#));
    assert!(!html.contains(r#"data-owner-act="release""#));
    assert!(html.contains("No owner"));
}

#[tokio::test]
async fn the_modal_should_offer_take_over_with_a_confirm_mark_for_an_agents_live_claim() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Held"})).await;
    claim(
        &fx,
        &id,
        json!({"claimant": "builder-i", "role": "builder", "ttl_seconds": 600}),
    )
    .await;
    let html = modal_html(&fx, &id).await;
    assert!(
        html.contains(r#"data-owner-act="owner-take" data-over="1" data-live-agent="1">Take over"#)
    );
    assert!(html.contains(r#"title="builder-i""#));
    assert!(html.contains("Until "));
    assert!(!html.contains(r#"data-owner-act="release""#));
}

#[tokio::test]
async fn a_person_should_claim_and_release_an_agents_stuck_claim_from_the_browser() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Stuck", "admin", 14400).await;
    let uri = format!("/api/tasks/{id}/claim");
    let html = modal_html(&fx, &id).await;
    assert!(html.contains("Until "));

    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &uri,
        Some(json!({"claimant": "other", "ttl_seconds": 60})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(text.starts_with("task claimed by admin until"), "{text}");

    let (status, _) = request(
        &fx,
        As::Admin,
        "POST",
        &format!("{uri}/release"),
        Some(json!({"fence": 1})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        modal_html(&fx, &id)
            .await
            .contains(r#"data-owner-act="owner-take">Take</button>"#)
    );
    let kinds = event_kinds(&fx, &id);
    assert_eq!(count(&kinds, HistoryKind::Claimed), 1);
    assert_eq!(count(&kinds, HistoryKind::ClaimReleased), 1);
}

#[tokio::test]
async fn the_modal_should_offer_metadata_edit_or_add_behind_the_editor() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Meta"})).await;
    let html = modal_html(&fx, &id).await;
    assert!(html.contains("No metadata"));
    assert!(html.contains("data-metadata-edit>Add<"));
    assert!(html.contains("data-metadata-save"));
    assert!(!html.contains(r#"id="modal-metadata""#));

    let set = json!({"metadata": {"agentctl": {"state": "<running>"}}});
    call(&fx, "PATCH", &format!("/api/tasks/{id}"), Some(set)).await;
    let html = modal_html(&fx, &id).await;
    assert!(html.contains("data-metadata-edit>Edit<"));
    assert!(html.contains("&lt;running&gt;"));
}

#[tokio::test]
async fn an_edit_patch_should_change_and_drop_only_the_keys_it_names() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Meta"})).await;
    let uri = format!("/api/tasks/{id}");
    let set = json!({"metadata": {"keep": 1, "change": "a", "drop": true}});
    call(&fx, "PATCH", &uri, Some(set)).await;
    let patch = json!({"metadata": {"change": "b", "drop": null, "added": [1]}});
    call(&fx, "PATCH", &uri, Some(patch)).await;
    let (_, detail) = call(&fx, "GET", &uri, None).await;
    assert_eq!(
        detail["metadata"],
        json!({"keep": 1, "change": "b", "added": [1]})
    );
}

#[tokio::test]
async fn an_edit_patch_should_refuse_a_non_object_and_a_value_over_the_cap() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Meta"})).await;
    let uri = format!("/api/tasks/{id}");
    let (status, _) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"metadata": [1]})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let big = "x".repeat(70 * 1024);
    let (status, text) = request(
        &fx,
        As::Admin,
        "PATCH",
        &uri,
        Some(json!({"metadata": {"big": big}})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text.contains("64 KiB"), "{text}");
    let (_, detail) = call(&fx, "GET", &uri, None).await;
    assert!(detail["metadata"].is_null() || detail.get("metadata").is_none());
}

#[tokio::test]
async fn a_board_limited_token_should_never_see_the_board_names_of_linked_cards_in_the_task_modal()
{
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let card = create_epic(&fx, "Card").await;
    let child = create(
        &fx,
        json!({"column_id": other_column, "name": "Remote child"}),
    )
    .await;
    let umbrella = create(
        &fx,
        json!({"column_id": other_column, "name": "Remote umbrella", "kind": "epic"}),
    )
    .await;
    link(&fx, &card, "child", &child).await;
    link(&fx, &card, "parent", &umbrella).await;

    let uri = format!("/api/tasks/{card}/modal");
    let (_, html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(html.contains("Remote child"), "{html}");
    assert!(html.contains("Remote umbrella"), "{html}");
    assert!(html.contains("Ops"), "{html}");

    let (mut record, limited) = new_api_token_record(&fx.user_id, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");
    let (_, html) = request(&fx, As::Bearer(&limited), "GET", &uri, None, false).await;
    for hidden in ["Ops", "Remote child", "Remote umbrella"] {
        assert!(!html.contains(hidden), "{hidden}: {html}");
    }
}

#[tokio::test]
async fn the_task_modal_should_name_its_own_board() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    let here = create(&fx, json!({"column_id": fx.col_todo, "name": "Here"})).await;
    let there = create(&fx, json!({"column_id": other_column, "name": "There"})).await;

    let html = modal_html(&fx, &there).await;
    let own = format!(r#"data-board-id="{other_board}" data-board-name="Ops""#);
    assert!(html.contains(&own), "{html}");
    let html = modal_html(&fx, &here).await;
    let own = format!(
        r#"data-board-id="{}" data-board-name="Agentctl""#,
        fx.board_id
    );
    assert!(html.contains(&own), "{html}");
}

#[tokio::test]
async fn the_link_picker_should_list_admissible_cards_past_the_first_hundred_hits() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let umbrella = create_epic(&fx, "Umbrella").await;
    for n in 1..=100 {
        let kid = create(
            &fx,
            json!({"column_id": fx.col_todo, "name": format!("Kid {n:03}")}),
        )
        .await;
        assert_eq!(link(&fx, &kid, "parent", &umbrella).await.0, StatusCode::OK);
    }
    let last = create(&fx, json!({"column_id": fx.col_done, "name": "Zed"})).await;

    assert_eq!(
        picker_ids(&fx, &card, "mode=child").await,
        vec![umbrella, last],
        "the cards that already have an umbrella do not use up the search cap"
    );
}

#[tokio::test]
async fn a_board_limited_token_should_not_be_offered_cards_the_server_refuses_to_link() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let hidden_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{hidden_board}/columns"),
        None,
    )
    .await;
    let hidden_column = columns[0]["id"].as_str().expect("column").to_string();
    let card = create_epic(&fx, "Card").await;
    let free = create_epic(&fx, "Free").await;
    let held = create(&fx, json!({"column_id": fx.col_todo, "name": "Held"})).await;
    let below = create_epic(&fx, "Below").await;
    let secret_umbrella = create(
        &fx,
        json!({"column_id": hidden_column, "name": "Secret umbrella", "kind": "epic"}),
    )
    .await;
    let secret_middle = create(
        &fx,
        json!({"column_id": hidden_column, "name": "Secret middle", "kind": "epic"}),
    )
    .await;
    assert_eq!(
        link(&fx, &held, "parent", &secret_umbrella).await.0,
        StatusCode::OK
    );
    assert_eq!(
        link(&fx, &card, "child", &secret_middle).await.0,
        StatusCode::OK
    );
    assert_eq!(
        link(&fx, &secret_middle, "child", &below).await.0,
        StatusCode::OK
    );
    assert_eq!(
        link(&fx, &card, "parent", &below).await.0,
        StatusCode::BAD_REQUEST,
        "the server refuses a cycle through the hidden board"
    );
    assert_eq!(
        link(&fx, &card, "child", &held).await.0,
        StatusCode::BAD_REQUEST,
        "the server refuses a child that has an umbrella"
    );

    let (mut record, limited) = new_api_token_record(&fx.user_id, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");

    let html = limited_picker(&fx, &limited, &card, "child").await;
    assert!(html.contains(&free), "{html}");
    for refused in [&held, &below] {
        assert!(
            !html.contains(refused.as_str()),
            "a card whose umbrella is hidden is not offered as a child: {html}"
        );
    }
    let html = limited_picker(&fx, &limited, &card, "parent").await;
    assert!(html.contains(&free), "{html}");
    assert!(
        !html.contains(&below),
        "a card under a hidden child is not offered as an umbrella: {html}"
    );
    assert!(!html.contains("Secret"), "{html}");
    assert!(!html.contains("Ops"), "{html}");

    let uri = format!("/api/tasks/{card}/link-picker?mode=child");
    let (_, owner_html) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(
        owner_html.contains(&secret_umbrella),
        "the account owner still sees the cards the hidden board holds: {owner_html} held={held} below={below} middle={secret_middle} free={free}"
    );
}

async fn limited_picker(fx: &Fixture, token: &str, card: &str, mode: &str) -> String {
    let uri = format!("/api/tasks/{card}/link-picker?mode={mode}");
    let (status, html) = request(fx, As::Bearer(token), "GET", &uri, None, false).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    html
}

#[test]
fn the_phone_board_bar_should_give_the_title_room_by_dropping_the_menu_label() {
    let css = include_str!("../../static/style.css");
    let at = css
        .find("@media (max-width: 639px) {\n  body:has(> .board-layout")
        .expect("phone media block");
    let phone = &css[at..];
    let phone = &phone[..phone.find("\n}\n").expect("end of the media block")];
    let menu = &phone[phone.find("#board-menu-btn {").expect("menu button rule")..];
    let menu = &menu[..menu.find('}').expect("end")];
    assert!(menu.contains("width: 34px"), "{menu}");
    let at = phone
        .find("#board-menu-btn .menu-label {")
        .expect("label rule");
    let label = &phone[at..];
    let label = &label[..label.find('}').expect("end")];
    assert!(label.contains("display: none"), "{label}");
}

#[tokio::test]
async fn a_restricted_token_should_keep_favourites_it_cannot_see() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let hidden = board["id"].as_str().expect("board id").to_string();
    let favourites = |settings: &Value| settings["favorite_boards"].clone();
    let (status, saved) = call(
        &fx,
        "PUT",
        "/api/settings",
        Some(json!({"favorite_boards": [hidden, fx.board_id]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let (mut record, limited) =
        new_api_token_record(&fx.user_id, "limited", AuthScope::Admin, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");
    let bearer = |method: &'static str, body: Option<Value>| {
        let limited = limited.clone();
        let fx = &fx;
        async move {
            let (status, text) = request(
                fx,
                As::Bearer(&limited),
                method,
                "/api/settings",
                body,
                true,
            )
            .await;
            (
                status,
                serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)),
            )
        }
    };

    let (status, read) = bearer("GET", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(favourites(&read), json!([fx.board_id]));
    assert!(!read.to_string().contains(&hidden), "{read}");

    let (status, echoed) = bearer("PUT", Some(json!({"favorite_boards": [fx.board_id]}))).await;
    assert_eq!(status, StatusCode::OK, "{echoed}");
    assert_eq!(favourites(&echoed), json!([fx.board_id]));
    let (status, sent) = bearer(
        "PUT",
        Some(json!({"favorite_boards": [hidden, fx.board_id]})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an id outside the set is ignored: {sent}"
    );
    assert_eq!(favourites(&sent), json!([fx.board_id]));

    let (_, owner) = call(&fx, "GET", "/api/settings", None).await;
    assert_eq!(
        favourites(&owner),
        json!([fx.board_id, hidden]),
        "the account keeps the favourite the token cannot see"
    );

    let (status, cleared) = bearer("PUT", Some(json!({"favorite_boards": []}))).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    let (_, owner) = call(&fx, "GET", "/api/settings", None).await;
    assert_eq!(favourites(&owner), json!([hidden]));
}

#[tokio::test]
async fn a_restricted_token_should_not_read_the_board_a_task_was_binned_and_restored_on() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let hidden = board["id"].as_str().expect("board id").to_string();
    let body = json!({"board_id": hidden, "name": "SecretColumn"});
    let (_, column) = call(&fx, "POST", "/api/columns", Some(body)).await;
    let secret = column["id"].as_str().expect("column id").to_string();
    let task = create(&fx, json!({"column_id": secret, "name": "Traveller"})).await;
    let (status, text) = call(&fx, "DELETE", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let uri = format!("/api/bin/{task}/restore");
    let (status, text) = call(&fx, "POST", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let moved = json!({"column_id": fx.col_todo, "position": 0});
    let uri = format!("/api/tasks/{task}/move?force=true");
    let (status, text) = call(&fx, "POST", &uri, Some(moved)).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (mut record, limited) = new_api_token_record(&fx.user_id, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");
    let (status, text) = request(
        &fx,
        As::Bearer(&limited),
        "GET",
        &format!("/api/tasks/{task}/history-view"),
        None,
        false,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!text.contains("SecretColumn"), "{text}");
    assert!(!text.contains(&secret), "{text}");
    assert!(text.contains("Deleted from another board"), "{text}");
    assert!(text.contains("Restored on another board"), "{text}");
}

#[tokio::test]
async fn a_restricted_token_should_not_read_the_names_of_a_board_outside_its_set() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let hidden = board["id"].as_str().expect("board id").to_string();
    let body = json!({"board_id": hidden, "name": "SecretColumn"});
    let (_, column) = call(&fx, "POST", "/api/columns", Some(body)).await;
    let secret = column["id"].as_str().expect("column id").to_string();
    let task = create(&fx, json!({"column_id": secret, "name": "Traveller"})).await;
    for target in [&fx.col_todo, &secret, &fx.col_todo, &fx.col_done] {
        let moved = json!({"column_id": target, "position": 0});
        let uri = format!("/api/tasks/{task}/move?force=true");
        let (status, text) = call(&fx, "POST", &uri, Some(moved)).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    let (mut record, limited) = new_api_token_record(&fx.user_id, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");
    let bearer = |uri: String| {
        let limited = limited.clone();
        let fx = &fx;
        async move {
            let (status, text) = request(fx, As::Bearer(&limited), "GET", &uri, None, false).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {text}");
            text
        }
    };

    let view = bearer(format!("/api/tasks/{task}/history-view")).await;
    let export = bearer(format!("/api/tasks/{task}/history-export?all=1")).await;
    for text in [&view, &export] {
        assert!(!text.contains("SecretColumn"), "{text}");
        assert!(!text.contains(&secret), "{text}");
        assert!(text.contains("Moved from another board"), "{text}");
        assert!(text.contains("Moved to another board"), "{text}");
        assert!(text.contains("Created on another board"), "{text}");
        assert!(
            text.contains("Shipped"),
            "a move inside the set keeps its names: {text}"
        );
    }
    let (_, seen) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{task}/history-view"),
        None,
        false,
    )
    .await;
    assert!(seen.contains("SecretColumn"), "{seen}");

    let uri = format!("/api/boards/{}/events/wait?after=0&timeout=0", fx.board_id);
    let events = bearer(uri.clone()).await;
    assert!(events.contains(&task), "{events}");
    assert!(!events.contains(&secret), "{events}");
    let (_, owner) = request(&fx, As::Admin, "GET", &uri, None, false).await;
    assert!(owner.contains(&secret), "{owner}");
}

#[tokio::test]
async fn a_restricted_token_archive_should_not_hold_the_names_of_a_board_outside_its_set() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let hidden = board["id"].as_str().expect("board id").to_string();
    let body = json!({"board_id": hidden, "name": "SecretColumn"});
    let (_, column) = call(&fx, "POST", "/api/columns", Some(body)).await;
    let secret = column["id"].as_str().expect("column id").to_string();
    let task = create(&fx, json!({"column_id": secret, "name": "Traveller"})).await;
    for target in [&fx.col_todo, &secret, &fx.col_todo] {
        let moved = json!({"column_id": target, "position": 0});
        let uri = format!("/api/tasks/{task}/move?force=true");
        let (status, text) = call(&fx, "POST", &uri, Some(moved)).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    let (mut record, limited) = new_api_token_record(&fx.user_id, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([fx.board_id.clone()]));
    fx.db.create_api_token(&record).expect("token");
    let request = axum::http::Request::builder()
        .uri(format!("/api/boards/{}/archive", fx.board_id))
        .header("authorization", format!("Bearer {limited}"))
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);
    let gzipped = axum::body::to_bytes(response.into_body(), 16 << 20)
        .await
        .expect("body");
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(&gzipped[..])
        .read_to_end(&mut tar)
        .expect("valid gzip");
    let text = String::from_utf8_lossy(&tar);
    assert!(text.contains("Traveller"), "the task is in the archive");
    assert!(text.contains("Moved from another board"), "{text}");
    assert!(!text.contains("SecretColumn"), "{text}");
    assert!(!text.contains(&secret), "{text}");
}

#[tokio::test]
async fn a_lapsed_person_claim_should_leave_the_modal_unclaimed_and_the_history_should_record_it() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Lapse", "admin", 1).await;
    let held = modal_html(&fx, &id).await;
    assert!(held.contains(r#"data-live-agent="1""#));
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let lapsed = modal_html(&fx, &id).await;
    assert!(lapsed.contains(r#"data-owner-act="owner-take">Take</button>"#));
    assert!(lapsed.contains("Lease lapsed"));
    let (status, _) = claim(&fx, &id, json!({"claimant": "admin", "ttl_seconds": 60})).await;
    assert_eq!(status, StatusCode::OK);
    let kinds = event_kinds(&fx, &id);
    let tail: Vec<HistoryKind> = kinds.iter().rev().take(3).rev().copied().collect();
    assert_eq!(
        tail,
        vec![
            HistoryKind::Claimed,
            HistoryKind::ClaimExpired,
            HistoryKind::Claimed
        ]
    );
}

#[test]
fn token_form_sources_should_not_hard_code_any_permission_name() {
    let sources = [
        (
            "templates/settings.html",
            include_str!("../../templates/settings.html"),
        ),
        ("static/pages.js", include_str!("../../static/pages.js")),
        ("static/app.js", include_str!("../../static/app.js")),
    ];
    for permission in Permission::ALL {
        for (file, text) in sources {
            assert!(
                !text.contains(permission.name()),
                "{file} hard-codes {}",
                permission.name()
            );
        }
    }
}

#[tokio::test]
async fn the_permission_catalog_should_require_only_boards_read() {
    let (_dir, fx) = fixture();
    let uri = "/api/v1/auth/permissions";
    let (status, text) = request(&fx, As::Admin, "GET", uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let catalog: Value = serde_json::from_str(&text).expect("json");
    let required: Vec<&str> = catalog
        .as_array()
        .expect("catalog")
        .iter()
        .filter(|item| item["required"] == true)
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert_eq!(required, ["boards.read"]);
}

#[tokio::test]
async fn the_settings_page_should_hold_the_catalog_form_the_board_picker_and_the_debugger() {
    let (_dir, fx) = fixture();
    let (status, page) = request(&fx, As::Admin, "GET", "/settings", None, false).await;
    assert_eq!(status, StatusCode::OK);
    for id in [
        "token-picker",
        "token-boards",
        "token-expiry",
        "token-inspect-form",
        "token-inspect-result",
    ] {
        assert!(page.contains(&format!("id=\"{id}\"")), "{id}");
    }
    assert!(!page.contains("name=\"token-scope\""), "{page}");
}

/// Starts a device request with `request`, approves it with `form`, and returns the approval
/// status, the page, and the device code.
async fn approve_request(fx: &Fixture, request: Value, form: &str) -> (StatusCode, String, String) {
    let mut body = json!({"client_name": "agentctl on laptop"});
    body.as_object_mut()
        .expect("object")
        .extend(request.as_object().expect("object").clone());
    let (status, started) = start_device(fx, body).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let user_code = started["user_code"].as_str().expect("user_code");
    let (status, page) = form_post(
        fx,
        As::Admin,
        "/device/approve",
        &format!("user_code={user_code}{form}"),
    )
    .await;
    let device_code = started["device_code"].as_str().expect("device_code");
    (status, page, device_code.to_string())
}

/// Polls an approved device request and returns the poll body and the token's whoami.
async fn issued_grant(fx: &Fixture, device_code: &str) -> (Value, Value) {
    let (status, text) = poll_device(fx, device_code).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let polled: Value = serde_json::from_str(&text).expect("json");
    let token = polled["token"].as_str().expect("token");
    (polled.clone(), whoami_as(fx, token).await)
}

#[tokio::test]
async fn a_device_request_should_be_refused_when_the_approver_raises_its_permissions() {
    let (_dir, fx) = fixture();
    let request = json!({"permissions": ["tasks.read", "tasks.create"]});

    let (status, page, _) = approve_request(
        &fx,
        request.clone(),
        "&permissions_listed=1&permissions=tasks.read&permissions=boards.delete",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("never raised above the request"), "{page}");

    let (status, page, _) = approve_request(&fx, request, "&permissions_listed=1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("Grant at least one permission"), "{page}");
}

#[tokio::test]
async fn lowering_the_permissions_of_a_device_request_should_persist_in_the_token() {
    let (_dir, fx) = fixture();
    let (status, _, device_code) = approve_request(
        &fx,
        json!({"permissions": ["tasks.read", "tasks.create", "boards.read"]}),
        "&permissions_listed=1&permissions=tasks.read&permissions=boards.read",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (polled, whoami) = issued_grant(&fx, &device_code).await;

    assert_eq!(polled["permissions"], json!(["boards.read", "tasks.read"]));
    assert_eq!(whoami["permissions"], json!(["boards.read", "tasks.read"]));
}

#[tokio::test]
async fn picking_boards_for_a_device_request_should_persist_in_the_token() {
    let (_dir, fx) = fixture();
    let (status, _, device_code) = approve_request(
        &fx,
        json!({"preset": "work"}),
        &format!("&boards={}", fx.board_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, whoami) = issued_grant(&fx, &device_code).await;

    assert_eq!(whoami["boards"][0]["id"], fx.board_id);
    assert_eq!(whoami["boards"].as_array().expect("boards").len(), 1);

    let (status, page, _) = approve_request(&fx, json!({"preset": "work"}), "&boards=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
}

#[tokio::test]
async fn a_device_client_using_an_old_scope_name_should_still_pair_with_that_preset() {
    let (_dir, fx) = fixture();
    let (status, _, device_code) = approve_request(&fx, json!({"scopes": ["work"]}), "").await;
    assert_eq!(status, StatusCode::OK);

    let (polled, whoami) = issued_grant(&fx, &device_code).await;

    let names: Vec<&str> = AuthScope::Work
        .permissions()
        .iter()
        .map(|permission| permission.name())
        .collect();
    assert_eq!(polled["scopes"], json!(["work"]));
    assert_eq!(polled["permissions"], json!(names));
    assert_eq!(whoami["boards"], "all");

    let (status, started) = start_device(
        &fx,
        json!({"client_name": "both", "scopes": ["work"], "preset": "work"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{started}");
}

#[tokio::test]
async fn the_expiry_of_a_device_request_should_key_on_deletes() {
    let (_dir, fx) = fixture();
    for (request, default) in [
        (json!({"permissions": ["tasks.read", "tasks.delete"]}), "24"),
        (
            json!({"permissions": ["tasks.read", "tasks.create"]}),
            "2160",
        ),
    ] {
        let (_, started) = {
            let mut body = json!({"client_name": "agentctl"});
            body.as_object_mut()
                .expect("object")
                .extend(request.as_object().expect("object").clone());
            start_device(&fx, body).await
        };
        let (status, page) = form_post(
            &fx,
            As::Admin,
            "/device",
            &format!("user_code={}", started["user_code"].as_str().expect("code")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            page.contains(&format!("data-expiry-hours=\"{default}\"")),
            "{request}: {page}"
        );
    }
}

#[tokio::test]
async fn a_comment_reaction_should_toggle_per_account_and_show_as_a_pill() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Talk"})).await;
    let (_, comment) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/comments"),
        Some(json!({"body": "Looks good"})),
    )
    .await;
    let cid = comment["id"].as_str().expect("id").to_string();
    let uri = format!("/api/tasks/{id}/comments/{cid}/reactions");
    let modal = format!("/api/tasks/{id}/modal");

    let (status, reacted) = call(&fx, "POST", &uri, Some(json!({"emoji": "\u{1F44D}"}))).await;
    assert_eq!(status, StatusCode::OK, "{reacted}");
    assert_eq!(
        reacted["reactions"],
        json!([{"emoji": "\u{1F44D}", "handle": "admin"}])
    );
    let (_, html) = request(&fx, As::Admin, "GET", &modal, None, false).await;
    assert!(
        html.contains("tm-reaction tm-reaction-mine\" data-comment-react=\"")
            && html.contains("<span class=\"tm-reaction-count\">1</span>"),
        "{html}"
    );

    let (_, listed) = call(&fx, "GET", &format!("/api/tasks/{id}/comments"), None).await;
    assert_eq!(listed[0]["reactions"].as_array().map(Vec::len), Some(1));

    fx.db
        .toggle_comment_reaction(&id, &cid, "\u{1F44D}", "bob", Actor::SYSTEM)
        .expect("second account");
    let (_, html) = request(&fx, As::Admin, "GET", &modal, None, false).await;
    assert!(
        html.contains("<span class=\"tm-reaction-count\">2</span>"),
        "{html}"
    );

    let (status, removed) = call(&fx, "POST", &uri, Some(json!({"emoji": "\u{1F44D}"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        removed["reactions"],
        json!([{"emoji": "\u{1F44D}", "handle": "bob"}])
    );

    let (status, _) = call(&fx, "POST", &uri, Some(json!({"emoji": "\u{1F680}"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "only the five emoji");
    let missing = format!("/api/tasks/{id}/comments/nope/reactions");
    let (status, _) = call(&fx, "POST", &missing, Some(json!({"emoji": "\u{1F44D}"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn patch_kind(fx: &Fixture, who: As<'_>, task: &str, body: Value) -> (StatusCode, Value) {
    let uri = format!("/api/tasks/{task}");
    let (status, text) = request(fx, who, "PATCH", &uri, Some(body), true).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn epic_with_children(fx: &Fixture, children: usize) -> (String, Vec<String>) {
    let epic = create(
        fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let mut ids = Vec::new();
    for index in 0..children {
        let name = format!("Child {index}");
        let child = create(fx, json!({"column_id": fx.col_todo, "name": name})).await;
        let (status, created) = link(fx, &child, "parent", &epic).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        ids.push(child);
    }
    (epic, ids)
}

#[tokio::test]
async fn creating_an_epic_with_only_a_board_id_should_land_in_the_first_column() {
    let (_dir, fx) = fixture();
    let (status, epic) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"board_id": fx.board_id, "name": "Epic", "kind": "epic"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{epic}");
    assert_eq!(epic["kind"], "epic");
    assert_eq!(epic["closed"], false);
    assert_eq!(epic["column_id"], fx.col_todo.as_str());

    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_done, "name": "Plain"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_eq!(task["kind"], "task");
    assert_eq!(task["closed"], false);
}

#[tokio::test]
async fn creating_a_card_without_a_column_should_be_a_400_unless_an_epic_names_a_board() {
    let (_dir, fx) = fixture();
    let cases = [
        json!({"name": "Both missing", "kind": "epic"}),
        json!({"name": "No column", "board_id": fx.board_id}),
        json!({"name": "Neither"}),
        json!({"name": "Wrong board", "kind": "epic", "board_id": "nope", "column_id": fx.col_todo}),
    ];
    for body in cases {
        let (status, error) = call(&fx, "POST", "/api/tasks", Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {error}");
    }
    let (status, error) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"name": "Ghost", "kind": "epic", "board_id": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
}

#[tokio::test]
async fn a_work_token_should_turn_a_card_into_an_epic_and_only_a_task_back_with_epics_manage() {
    let (_dir, fx) = fixture();
    let work = token_with(&fx, "worker", AuthScope::Work);
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;

    let (status, detail) = patch_kind(&fx, As::Bearer(&work), &id, json!({"kind": "epic"})).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["kind"], "epic");
    assert_eq!(detail["closed"], false);

    let (status, error) = patch_kind(&fx, As::Bearer(&work), &id, json!({"kind": "epic"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");

    let (status, error) = patch_kind(&fx, As::Bearer(&work), &id, json!({"kind": "task"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error, permission_denied("epics.manage"));
    assert_eq!(stored(&fx, &id)["kind"], "epic");

    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let (status, detail) = patch_kind(&fx, As::Bearer(&lead), &id, json!({"kind": "task"})).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["kind"], "task");
    let (status, error) = patch_kind(&fx, As::Bearer(&lead), &id, json!({"kind": "task"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
}

#[tokio::test]
async fn an_epic_with_children_should_not_become_a_task_without_unlink_children() {
    let (_dir, fx) = fixture();
    let (epic, children) = epic_with_children(&fx, 2).await;
    let lead = token_with(&fx, "lead", AuthScope::Provision);

    let body = json!({"kind": "task"});
    let (status, error) = patch_kind(&fx, As::Bearer(&lead), &epic, body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["code"], "epic_has_children");
    assert_eq!(stored(&fx, &epic)["kind"], "epic");
    for child in &children {
        assert_eq!(links_of(&fx, child).await.len(), 1);
    }

    let body = json!({"kind": "task", "unlink_children": false});
    let (status, error) = patch_kind(&fx, As::Admin, &epic, body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
}

#[tokio::test]
async fn unlink_children_should_leave_the_children_unlinked_and_the_epic_a_plain_card() {
    let (_dir, fx) = fixture();
    let (epic, children) = epic_with_children(&fx, 2).await;
    let body = json!({"kind": "task", "unlink_children": true});
    let (status, detail) = patch_kind(&fx, As::Admin, &epic, body).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["kind"], "task");
    assert_eq!(detail["closed"], false);
    assert_eq!(detail["children_total"], 0);
    for child in &children {
        assert!(links_of(&fx, child).await.is_empty());
        let (status, child_detail) = call(&fx, "GET", &format!("/api/tasks/{child}"), None).await;
        assert_eq!(status, StatusCode::OK, "{child_detail}");
        assert!(child_detail["parent"].is_null(), "{child_detail}");
        assert!(event_kinds(&fx, child).contains(&HistoryKind::LinkRemoved));
    }
    assert!(links_of(&fx, &epic).await.is_empty());
}

#[tokio::test]
async fn unlink_children_should_be_a_400_without_kind_task_and_a_refused_patch_should_unlink_nothing()
 {
    let (_dir, fx) = fixture();
    let (epic, children) = epic_with_children(&fx, 1).await;
    let work = token_with(&fx, "worker", AuthScope::Work);

    let (status, error) = patch_kind(&fx, As::Admin, &epic, json!({"unlink_children": true})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let body = json!({"kind": "epic", "unlink_children": true});
    let (status, error) = patch_kind(&fx, As::Admin, &epic, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let body = json!({"kind": "task", "unlink_children": true});
    let (status, error) = patch_kind(&fx, As::Bearer(&work), &epic, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");

    let plain = create(&fx, json!({"column_id": fx.col_todo, "name": "Plain"})).await;
    let body = json!({"kind": "task", "unlink_children": true});
    let (status, error) = patch_kind(&fx, As::Admin, &plain, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(links_of(&fx, &children[0]).await.len(), 1);
    assert_eq!(stored(&fx, &epic)["kind"], "epic");
}

#[tokio::test]
async fn kind_and_closed_should_appear_on_task_detail_the_board_task_list_and_the_spec() {
    let (_dir, fx) = fixture();
    let (epic, children) = epic_with_children(&fx, 1).await;

    let (status, detail) = call(&fx, "GET", &format!("/api/tasks/{epic}"), None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(
        (&detail["kind"], &detail["closed"]),
        (&json!("epic"), &json!(false))
    );
    let (status, detail) = call(&fx, "GET", &format!("/api/tasks/{}", children[0]), None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(
        (&detail["kind"], &detail["closed"]),
        (&json!("task"), &json!(false))
    );

    let uri = list_uri(&fx, BOARD_TASKS_PATH);
    let (status, items) = call_list(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{items}");
    let item = |id: &str| {
        items
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["id"] == id)
            .expect("listed")
            .clone()
    };
    assert_eq!(item(&epic)["kind"], "epic");
    assert_eq!(item(&epic)["closed"], false);
    assert_eq!(item(&children[0])["kind"], "task");
    assert_eq!(item(&children[0])["closed"], false);

    let spec: Value =
        serde_json::from_str(&ApiDoc::openapi().to_json().expect("json")).expect("spec");
    let schemas = &spec["components"]["schemas"];
    for schema in ["TaskDetail", "TaskSummary", "CreateTaskInput"] {
        assert!(
            schemas[schema]["properties"]["kind"].is_object(),
            "{schema}.kind"
        );
    }
    for schema in ["TaskDetail", "TaskSummary"] {
        assert!(
            schemas[schema]["properties"]["closed"].is_object(),
            "{schema}.closed"
        );
    }
    for property in ["kind", "unlink_children"] {
        assert!(
            schemas["UpdateTaskInput"]["properties"][property].is_object(),
            "UpdateTaskInput.{property}"
        );
    }
}

#[tokio::test]
async fn unlink_children_should_refuse_a_token_when_a_child_is_locked_but_not_the_browser() {
    let (_dir, fx) = fixture();
    let (epic, children) = epic_with_children(&fx, 1).await;
    let (status, labelled) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{}", children[0]),
        Some(json!({"labels": ["needs:human"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{labelled}");
    let lead = token_with(&fx, "lead", AuthScope::Provision);
    let body = json!({"kind": "task", "unlink_children": true});

    let (status, error) = patch_kind(&fx, As::Bearer(&lead), &epic, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["code"], "card_locked");
    assert_eq!(links_of(&fx, &children[0]).await.len(), 1);
    assert_eq!(stored(&fx, &epic)["kind"], "epic");

    let (status, detail) = patch_kind(&fx, As::Admin, &epic, body).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(links_of(&fx, &children[0]).await.is_empty());
}

#[tokio::test]
async fn kind_epic_without_the_fence_on_a_claimed_task_should_answer_claim_held() {
    let (_dir, fx) = fixture();
    let (id, _) = claim_task(&fx, "Card", "builder-i", 60).await;
    let work = token_with(&fx, "worker", AuthScope::Work);

    let (status, error) = patch_kind(&fx, As::Bearer(&work), &id, json!({"kind": "epic"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["code"], "claim_held");
    assert_eq!(stored(&fx, &id)["kind"], "task");
}

#[tokio::test]
async fn kind_epic_with_the_live_claims_fence_should_succeed() {
    let (_dir, fx) = fixture();
    let (id, granted) = claim_task(&fx, "Card", "planner-i", 60).await;
    let fence = granted["fence"].to_string();

    let uri = format!("/api/tasks/{id}");
    let body = json!({"kind": "epic"});
    let (status, text) = fenced_request(&fx, "PATCH", &uri, body.clone(), Some("99")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert_eq!(stored(&fx, &id)["kind"], "task");

    let (status, text) = fenced_request(&fx, "PATCH", &uri, body, Some(&fence)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(stored(&fx, &id)["kind"], "epic");
}

#[tokio::test]
async fn unlink_children_should_refuse_a_restricted_token_when_a_child_is_on_a_board_outside_its_set()
 {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let other_column = column_ids_of(&fx, &other_board).await.remove(0);
    let (epic, children) = epic_with_children(&fx, 1).await;
    let remote = create(&fx, json!({"column_id": other_column, "name": "Remote"})).await;
    let (status, linked) = link(&fx, &remote, "parent", &epic).await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    let (_, raw) = assigned_token(&fx, AuthScope::Provision, &[&fx.board_id]);
    let body = json!({"kind": "task", "unlink_children": true});

    let (status, error) = patch_kind(&fx, As::Bearer(&raw), &epic, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["code"], "permission_denied");
    assert_eq!(error["board_id"], other_board.as_str());
    assert_eq!(stored(&fx, &epic)["kind"], "epic");
    assert_eq!(links_of(&fx, &remote).await.len(), 1);
    assert_eq!(links_of(&fx, &children[0]).await.len(), 1);
}

async fn post_epic(fx: &Fixture, who: As<'_>, task: &str, verb: &str) -> (StatusCode, Value) {
    let uri = format!("/api/tasks/{task}/{verb}");
    let (status, text) = request(fx, who, "POST", &uri, None, true).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn the_owner_should_close_and_reopen_an_epic_with_history_events() {
    let (_dir, fx) = fixture();
    let (epic, children) = epic_with_children(&fx, 1).await;
    let before = stored(&fx, &epic);

    let (status, detail) = post_epic(&fx, As::Admin, &epic, "close").await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["kind"], "epic");
    assert_eq!(detail["closed"], true);
    let after = stored(&fx, &epic);
    assert_eq!(after["closed"], true);
    assert_eq!(after["column_id"], before["column_id"]);
    assert_eq!(after["completed_at"], before["completed_at"]);
    assert_eq!(count(&event_kinds(&fx, &epic), HistoryKind::Closed), 1);
    assert_eq!(links_of(&fx, &children[0]).await.len(), 1);

    let (status, detail) = post_epic(&fx, As::Admin, &epic, "reopen").await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["closed"], false);
    assert_eq!(stored(&fx, &epic)["closed"], false);
    assert_eq!(count(&event_kinds(&fx, &epic), HistoryKind::Reopened), 1);
}

#[tokio::test]
async fn closing_a_closed_epic_or_reopening_an_open_one_should_be_a_400_without_an_event() {
    let (_dir, fx) = fixture();
    let (epic, _) = epic_with_children(&fx, 0).await;

    let (status, error) = post_epic(&fx, As::Admin, &epic, "reopen").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let (status, _) = post_epic(&fx, As::Admin, &epic, "close").await;
    assert_eq!(status, StatusCode::OK);
    let (status, error) = post_epic(&fx, As::Admin, &epic, "close").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let kinds = event_kinds(&fx, &epic);
    assert_eq!(count(&kinds, HistoryKind::Closed), 1);
    assert_eq!(count(&kinds, HistoryKind::Reopened), 0);
}

#[tokio::test]
async fn closing_or_reopening_a_plain_card_should_be_a_400_and_a_missing_task_a_404() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    for verb in ["close", "reopen"] {
        let (status, error) = post_epic(&fx, As::Admin, &card, verb).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{verb}: {error}");
        let (status, error) = post_epic(&fx, As::Admin, "missing", verb).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{verb}: {error}");
    }
    assert_eq!(stored(&fx, &card)["closed"], false);
    let kinds = event_kinds(&fx, &card);
    assert_eq!(
        count(&kinds, HistoryKind::Closed) + count(&kinds, HistoryKind::Reopened),
        0
    );
}

#[tokio::test]
async fn a_work_token_should_get_403_naming_epics_manage_on_close_and_reopen_but_a_lead_succeeds() {
    let (_dir, fx) = fixture();
    let (epic, _) = epic_with_children(&fx, 0).await;
    let work = token_with(&fx, "worker", AuthScope::Work);
    let lead = token_with(&fx, "lead", AuthScope::Provision);

    for verb in ["close", "reopen"] {
        let (status, error) = post_epic(&fx, As::Bearer(&work), &epic, verb).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{verb}: {error}");
        assert_eq!(error, permission_denied("epics.manage"));
    }
    assert_eq!(stored(&fx, &epic)["closed"], false);

    let (status, detail) = post_epic(&fx, As::Bearer(&lead), &epic, "close").await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["closed"], true);
    let (status, error) = post_epic(&fx, As::Bearer(&work), &epic, "reopen").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    let (status, detail) = post_epic(&fx, As::Bearer(&lead), &epic, "reopen").await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["closed"], false);
}

#[tokio::test]
async fn an_epic_whose_children_are_all_done_should_stay_open_until_someone_closes_it() {
    let (_dir, fx) = fixture();
    let (epic, children) = epic_with_children(&fx, 2).await;
    for child in &children {
        let move_body = json!({"column_id": fx.col_done, "position": 0});
        let uri = format!("/api/tasks/{child}/move");
        let (status, moved) = call(&fx, "POST", &uri, Some(move_body)).await;
        assert_eq!(status, StatusCode::OK, "{moved}");
    }
    let (status, detail) = call(&fx, "GET", &format!("/api/tasks/{epic}"), None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["closed"], false);
    assert_eq!(stored(&fx, &epic)["closed"], false);
    assert!(!event_kinds(&fx, &epic).contains(&HistoryKind::Closed));
}

#[tokio::test]
async fn closing_a_locked_epic_should_refuse_a_token_but_not_the_browser() {
    let (_dir, fx) = fixture();
    let (epic, _) = epic_with_children(&fx, 0).await;
    let (status, labelled) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{epic}"),
        Some(json!({"labels": ["needs:human"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{labelled}");
    let lead = token_with(&fx, "lead", AuthScope::Provision);

    let (status, error) = post_epic(&fx, As::Bearer(&lead), &epic, "close").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["code"], "card_locked");
    assert_eq!(stored(&fx, &epic)["closed"], false);

    let (status, detail) = post_epic(&fx, As::Admin, &epic, "close").await;
    assert_eq!(status, StatusCode::OK, "{detail}");
}

async fn board_epics(fx: &Fixture, board: &str) -> Value {
    let (status, epics) = call(fx, "GET", &format!("/api/boards/{board}/epics"), None).await;
    assert_eq!(status, StatusCode::OK, "{epics}");
    epics
}

fn epic_entry<'a>(epics: &'a Value, id: &str) -> &'a Value {
    epics
        .as_array()
        .expect("array")
        .iter()
        .find(|epic| epic["id"] == id)
        .expect("epic listed")
}

#[tokio::test]
async fn board_epics_should_count_children_on_other_boards_by_their_done_column_at_request_time() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let (_, columns) = call(
        &fx,
        "GET",
        &format!("/api/boards/{other_board}/columns"),
        None,
    )
    .await;
    let other_column = columns[0]["id"].as_str().expect("column").to_string();
    fx.db
        .set_column_done(&other_column, true)
        .expect("set done");
    let (epic, children) = epic_with_children(&fx, 2).await;
    let remote = create(&fx, json!({"column_id": other_column, "name": "Remote"})).await;
    let (status, linked) = link(&fx, &remote, "parent", &epic).await;
    assert_eq!(status, StatusCode::OK, "{linked}");

    let epics = board_epics(&fx, &fx.board_id).await;
    assert_eq!(epics.as_array().expect("array").len(), 1);
    let entry = epic_entry(&epics, &epic);
    assert_eq!(entry["name"], "Epic");
    assert_eq!(entry["closed"], false);
    assert_eq!(entry["parent_id"], Value::Null);
    assert_eq!(entry["children_total"], 3);
    assert_eq!(entry["children_done"], 1);
    assert!(
        board_epics(&fx, &other_board)
            .await
            .as_array()
            .expect("array")
            .is_empty()
    );

    move_to_done(&fx, &children[0]).await;
    let epics = board_epics(&fx, &fx.board_id).await;
    assert_eq!(epic_entry(&epics, &epic)["children_done"], 2);

    fx.db
        .set_column_done(&other_column, false)
        .expect("clear done");
    let epics = board_epics(&fx, &fx.board_id).await;
    assert_eq!(epic_entry(&epics, &epic)["children_done"], 1);
}

#[tokio::test]
async fn a_nested_epic_should_report_its_parent_epic_and_count_only_direct_children() {
    let (_dir, fx) = fixture();
    let (outer, _) = epic_with_children(&fx, 1).await;
    let (inner, inner_children) = epic_with_children(&fx, 2).await;
    let (status, linked) = link(&fx, &inner, "parent", &outer).await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    move_to_done(&fx, &inner_children[0]).await;

    let epics = board_epics(&fx, &fx.board_id).await;
    let outer_entry = epic_entry(&epics, &outer);
    assert_eq!(outer_entry["parent_id"], Value::Null);
    assert_eq!(outer_entry["children_total"], 2);
    assert_eq!(outer_entry["children_done"], 0);
    let inner_entry = epic_entry(&epics, &inner);
    assert_eq!(inner_entry["parent_id"], outer.as_str());
    assert_eq!(inner_entry["children_total"], 2);
    assert_eq!(inner_entry["children_done"], 1);
}

#[tokio::test]
async fn board_epics_should_skip_plain_cards_and_give_a_card_umbrella_no_parent_id() {
    let (_dir, fx) = fixture();
    let umbrella = create_epic(&fx, "Umbrella").await;
    let epic = create_epic(&fx, "Epic").await;
    let (status, linked) = link(&fx, &epic, "parent", &umbrella).await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    make_plain(&fx.db, &[&umbrella]);

    let epics = board_epics(&fx, &fx.board_id).await;
    assert_eq!(epics.as_array().expect("array").len(), 1);
    let entry = epic_entry(&epics, &epic);
    assert_eq!(entry["parent_id"], Value::Null);
    assert_eq!(entry["children_total"], 0);
    assert_eq!(entry["children_done"], 0);
}

#[tokio::test]
async fn board_epics_should_list_open_epics_by_name_then_closed_ones_by_name() {
    let (_dir, fx) = fixture();
    let mut ids = Vec::new();
    for name in ["Zeta", "alpha", "Beta", "Aardvark"] {
        let epic = create(
            &fx,
            json!({"column_id": fx.col_todo, "name": name, "kind": "epic"}),
        )
        .await;
        ids.push(epic);
    }
    for id in [&ids[3], &ids[2]] {
        let (status, closed) = post_epic(&fx, As::Admin, id, "close").await;
        assert_eq!(status, StatusCode::OK, "{closed}");
    }

    let epics = board_epics(&fx, &fx.board_id).await;
    let names: Vec<&str> = epics
        .as_array()
        .expect("array")
        .iter()
        .map(|epic| epic["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["alpha", "Zeta", "Aardvark", "Beta"]);
    assert_eq!(epics[2]["closed"], true);
    assert_eq!(epics[0]["closed"], false);
}

#[tokio::test]
async fn board_epics_should_be_a_404_for_a_missing_board() {
    let (_dir, fx) = fixture();
    let (status, error) = call(&fx, "GET", "/api/boards/missing/epics", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
}

#[tokio::test]
async fn board_epics_should_leave_out_children_on_boards_a_restricted_token_cannot_reach() {
    let (_dir, fx) = fixture();
    let (_, board) = call(&fx, "POST", "/api/boards", Some(json!({"name": "Ops"}))).await;
    let other_board = board["id"].as_str().expect("board id").to_string();
    let other_column = column_ids_of(&fx, &other_board).await.remove(0);
    let (epic, _) = epic_with_children(&fx, 1).await;
    let remote = create(&fx, json!({"column_id": other_column, "name": "Remote"})).await;
    let (status, linked) = link(&fx, &remote, "parent", &epic).await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    let (_, raw) = assigned_token(&fx, AuthScope::Read, &[&fx.board_id]);

    let uri = format!("/api/boards/{}/epics", fx.board_id);
    let (status, text) = request(&fx, As::Bearer(&raw), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let held: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(epic_entry(&held, &epic)["children_total"], 1);
    assert_eq!(
        epic_entry(&board_epics(&fx, &fx.board_id).await, &epic)["children_total"],
        2
    );

    let uri = format!("/api/boards/{other_board}/epics");
    let (status, text) = request(&fx, As::Bearer(&raw), "GET", &uri, None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
}

#[tokio::test]
async fn an_epic_should_not_count_against_a_column_wip_limit() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Review", Some(2))
        .expect("column");
    create(
        &fx,
        json!({"column_id": limited, "name": "Epic", "kind": "epic"}),
    )
    .await;
    create(&fx, json!({"column_id": limited, "name": "R1"})).await;
    let first = create(&fx, json!({"column_id": fx.col_todo, "name": "First"})).await;
    let second = create(&fx, json!({"column_id": fx.col_todo, "name": "Second"})).await;
    let into_review = json!({"column_id": limited, "position": 0});

    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{first}/move"),
        Some(into_review.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let (status, error) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{second}/move"),
        Some(into_review),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error, "column WIP limit reached (2/2)");
    assert_eq!(fx.db.count_tasks_in_column(&limited).expect("count"), 2);
}

async fn epic_with_child(fx: &Fixture, child_column: &str) -> (String, String) {
    let epic = create(
        fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let child = create(fx, json!({"column_id": child_column, "name": "Child"})).await;
    let (status, link) = call(
        fx,
        "POST",
        &format!("/api/tasks/{child}/links"),
        Some(json!({"kind": "parent", "target_id": epic})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{link}");
    (epic, child)
}

async fn move_to(fx: &Fixture, id: &str, column: &str) -> (StatusCode, Value) {
    call(
        fx,
        "POST",
        &format!("/api/tasks/{id}/move"),
        Some(json!({"column_id": column, "position": 0})),
    )
    .await
}

#[tokio::test]
async fn an_epic_with_every_child_done_should_move_through_any_column_and_close_in_done() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Limited", Some(1))
        .expect("column");
    create(&fx, json!({"column_id": limited, "name": "Held"})).await;
    let (epic, _) = epic_with_child(&fx, &fx.col_done).await;

    let (status, moved) = move_to(&fx, &epic, &limited).await;
    assert_eq!(status, StatusCode::OK, "a full WIP-limited column: {moved}");
    assert_eq!(stored(&fx, &epic)["column_id"], limited.as_str());
    assert_eq!(stored(&fx, &epic)["closed"], false);

    let (status, moved) = move_to(&fx, &epic, &fx.col_done).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(stored(&fx, &epic)["column_id"], fx.col_done.as_str());
    assert_eq!(stored(&fx, &epic)["closed"], true);
}

#[tokio::test]
async fn moving_a_closed_epic_out_of_done_should_reopen_it() {
    let (_dir, fx) = fixture();
    let (epic, _) = epic_with_child(&fx, &fx.col_done).await;
    let (status, moved) = move_to(&fx, &epic, &fx.col_done).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(stored(&fx, &epic)["closed"], true);

    let (status, moved) = move_to(&fx, &epic, &fx.col_todo).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(stored(&fx, &epic)["column_id"], fx.col_todo.as_str());
    assert_eq!(stored(&fx, &epic)["closed"], false);
}

#[tokio::test]
async fn an_epic_with_an_open_child_should_be_refused_a_limited_or_done_column_and_allowed_any_other()
 {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Limited", Some(5))
        .expect("column");
    let open = fx
        .db
        .create_column(&fx.board_id, "Open", None)
        .expect("column");
    let (epic, _) = epic_with_child(&fx, &fx.col_todo).await;

    for column in [&limited, &fx.col_done] {
        let (status, error) = move_to(&fx, &epic, column).await;
        assert_eq!(status, StatusCode::CONFLICT, "{error}");
        assert!(
            error
                .as_str()
                .is_some_and(|text| text.contains("open children")),
            "{error}"
        );
    }
    let plain = create(&fx, json!({"column_id": fx.col_todo, "name": "Plain"})).await;
    let body = json!({"task_ids": [plain, epic], "action": "move", "column_id": fx.col_done});
    let (status, error) = call(
        &fx,
        "POST",
        &format!("/api/boards/{}/tasks/bulk", fx.board_id),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert!(
        error
            .as_str()
            .is_some_and(|text| text.contains("open children")),
        "{error}"
    );
    assert_eq!(stored(&fx, &epic)["column_id"], fx.col_todo.as_str());
    assert_eq!(stored(&fx, &plain)["column_id"], fx.col_todo.as_str());

    let (status, moved) = move_to(&fx, &epic, &open).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(stored(&fx, &epic)["column_id"], open.as_str());
}

#[tokio::test]
async fn a_bulk_move_into_done_should_count_the_children_it_moves_whatever_their_order() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Limited", Some(5))
        .expect("column");
    let (epic, child) = epic_with_child(&fx, &fx.col_todo).await;
    let bulk = |to: &str| json!({"task_ids": [epic, child], "action": "move", "column_id": to});
    let path = format!("/api/boards/{}/tasks/bulk", fx.board_id);

    let (status, error) = call(&fx, "POST", &path, Some(bulk(&limited))).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the child stays open: {error}"
    );
    assert_eq!(stored(&fx, &epic)["column_id"], fx.col_todo.as_str());

    let (status, moved) = call(&fx, "POST", &path, Some(bulk(&fx.col_done))).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(stored(&fx, &epic)["column_id"], fx.col_done.as_str());
    assert_eq!(stored(&fx, &epic)["closed"], true);
    assert_eq!(stored(&fx, &child)["column_id"], fx.col_done.as_str());
}

#[tokio::test]
async fn the_open_child_refusal_should_follow_the_column_settings_after_a_rename() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Limited", Some(5))
        .expect("column");
    let (epic, _) = epic_with_child(&fx, &fx.col_todo).await;
    for (column, name) in [(&limited, "Backlog"), (&fx.col_done, "Archive")] {
        let uri = format!("/api/columns/{column}");
        let (status, body) = call(&fx, "PATCH", &uri, Some(json!({"name": name}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, _) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{}", fx.col_todo),
        Some(json!({"name": "Done"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let elsewhere = fx
        .db
        .create_column(&fx.board_id, "Done", None)
        .expect("column");

    for column in [&limited, &fx.col_done] {
        let (status, error) = move_to(&fx, &epic, column).await;
        assert_eq!(status, StatusCode::CONFLICT, "{error}");
        assert!(
            error
                .as_str()
                .is_some_and(|text| text.contains("open children")),
            "{error}"
        );
    }
    let (status, moved) = move_to(&fx, &epic, &elsewhere).await;
    assert_eq!(status, StatusCode::OK, "a column only named Done: {moved}");
    assert_eq!(stored(&fx, &epic)["closed"], false);
}

#[tokio::test]
async fn claiming_an_epic_with_an_open_child_should_be_a_409_epic_not_claimable() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let child = create(&fx, json!({"column_id": fx.col_todo, "name": "Child"})).await;
    let (status, linked) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{child}/links"),
        Some(json!({"kind": "parent", "target_id": epic})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    let (status, error) = claim(&fx, &epic, json!({"claimant": "agent", "ttl_seconds": 60})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(
        error,
        "an epic can be claimed once every child is done, and 1 child is still open"
    );
    assert!(stored(&fx, &epic)["claim"].is_null());
}

async fn claim_as(fx: &Fixture, token: &str, task: &str) -> (StatusCode, Value) {
    let body = json!({"claimant": "agent", "ttl_seconds": 60});
    let (status, text) = request(
        fx,
        As::Bearer(token),
        "POST",
        &format!("/api/tasks/{task}/claim"),
        Some(body),
        true,
    )
    .await;
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

#[tokio::test]
async fn claiming_an_epic_with_an_open_child_on_an_unseen_board_should_be_a_409_epic_not_claimable()
{
    let (_dir, fx) = fixture();
    let (_, _, far_task) = other_board(&fx);
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let (status, linked) = link(&fx, &far_task, "parent", &epic).await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    let (_, scoped) = assigned_token(&fx, AuthScope::Work, &[&fx.board_id]);

    let (status, error) = claim_as(&fx, &scoped, &epic).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["code"], "epic_not_claimable");
    assert_eq!(
        error["message"],
        "an epic can be claimed once every child is done, and 1 child is still open"
    );
    assert!(stored(&fx, &epic)["claim"].is_null());
}

#[tokio::test]
async fn claiming_an_epic_whose_child_on_an_unseen_board_is_done_should_succeed() {
    let (_dir, fx) = fixture();
    let (board, _, far_task) = other_board(&fx);
    let done = fx
        .db
        .create_column(&board, "Shipped", None)
        .expect("column");
    fx.db.set_column_done(&done, true).expect("set done");
    fx.db.move_task(&far_task, &done, None, 1.0).expect("move");
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let (status, linked) = link(&fx, &far_task, "parent", &epic).await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    let (_, scoped) = assigned_token(&fx, AuthScope::Work, &[&fx.board_id]);

    let (status, claimed) = claim_as(&fx, &scoped, &epic).await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
}

#[tokio::test]
async fn a_column_holding_only_epics_should_delete_and_send_them_to_the_first_column() {
    let (_dir, fx) = fixture();
    let extra = fx
        .db
        .create_column(&fx.board_id, "Extra", None)
        .expect("column");
    let epic = create(
        &fx,
        json!({"column_id": extra, "name": "Epic", "kind": "epic"}),
    )
    .await;

    let (status, deleted) = call(&fx, "DELETE", &format!("/api/columns/{extra}"), None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let row = stored(&fx, &epic);
    assert_eq!(row["column_id"], fx.col_todo.as_str());
    assert_eq!(row["kind"], "epic");
}

#[tokio::test]
async fn deleting_a_column_should_send_an_epic_with_an_open_child_to_a_limited_first_column() {
    let (_dir, fx) = fixture();
    let uri = format!("/api/columns/{}", fx.col_todo);
    let (status, body) = call(&fx, "PATCH", &uri, Some(json!({"wip_limit": 3}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let extra = fx
        .db
        .create_column(&fx.board_id, "Extra", None)
        .expect("column");
    let (epic, _) = epic_with_child(&fx, &fx.col_todo).await;
    let (status, moved) = move_to(&fx, &epic, &extra).await;
    assert_eq!(status, StatusCode::OK, "{moved}");

    let (status, deleted) = call(&fx, "DELETE", &format!("/api/columns/{extra}"), None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let row = stored(&fx, &epic);
    assert_eq!(row["column_id"], fx.col_todo.as_str());
    assert_eq!(row["closed"], false);
}

#[tokio::test]
async fn deleting_a_done_column_should_leave_the_closed_flag_of_its_epic() {
    let (_dir, fx) = fixture();
    let extra = fx
        .db
        .create_column(&fx.board_id, "Extra", None)
        .expect("column");
    fx.db.set_column_done(&extra, true).expect("set done");
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let (status, moved) = move_to(&fx, &epic, &extra).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(stored(&fx, &epic)["closed"], true);

    let (status, deleted) = call(&fx, "DELETE", &format!("/api/columns/{extra}"), None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let row = stored(&fx, &epic);
    assert_eq!(row["column_id"], fx.col_todo.as_str());
    assert_eq!(row["closed"], true);
}

#[tokio::test]
async fn a_column_holding_work_beside_an_epic_should_refuse_deletion_counting_only_work() {
    let (_dir, fx) = fixture();
    let extra = fx
        .db
        .create_column(&fx.board_id, "Extra", None)
        .expect("column");
    let epic = create(
        &fx,
        json!({"column_id": extra, "name": "Epic", "kind": "epic"}),
    )
    .await;
    create(&fx, json!({"column_id": extra, "name": "Work"})).await;

    let (status, error) = call(&fx, "DELETE", &format!("/api/columns/{extra}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(stored(&fx, &epic)["column_id"], extra.as_str());
}

#[tokio::test]
async fn an_epic_should_not_count_against_an_hours_wip_limit() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Review", None)
        .expect("column");
    let limit = json!({"wip_limit": 3, "config_json": "{\"wip_limit_type\":\"hours\"}"});
    let (status, patched) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{limited}"),
        Some(limit),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    create(
        &fx,
        json!({"column_id": limited, "name": "Epic", "kind": "epic", "estimate_hours": 10}),
    )
    .await;
    let first = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "First", "estimate_hours": 2}),
    )
    .await;
    let second = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Second", "estimate_hours": 2}),
    )
    .await;
    let into_review = json!({"column_id": limited, "position": 0});

    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{first}/move"),
        Some(into_review.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let (status, error) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{second}/move"),
        Some(into_review),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
}

#[tokio::test]
async fn an_epic_sent_to_a_done_column_by_a_deletion_should_keep_its_completion_unstamped() {
    let (_dir, fx) = fixture();
    fx.db.set_column_done(&fx.col_todo, true).expect("done");
    let extra = fx
        .db
        .create_column(&fx.board_id, "Extra", None)
        .expect("column");
    let epic = create(
        &fx,
        json!({"column_id": extra, "name": "Epic", "kind": "epic"}),
    )
    .await;

    let (status, deleted) = call(&fx, "DELETE", &format!("/api/columns/{extra}"), None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let row = stored(&fx, &epic);
    assert_eq!(row["column_id"], fx.col_todo.as_str());
    assert!(row["completed_at"].is_null(), "{row}");
}

const CHILDREN: &str = "/api/v1/auth/tokens/children";

/// Posts `body` to the child token route as `parent`, and returns the status and parsed reply.
async fn issue(fx: &Fixture, parent: &str, body: Value) -> (StatusCode, Value) {
    let (status, text) = bearer_json(fx, parent, "POST", CHILDREN, body).await;
    let reply = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, reply)
}

/// Makes a token whose children may live up to `hours`, and returns its secret.
fn long_lived_issuer(fx: &Fixture, name: &str, hours: i64) -> String {
    let (mut record, raw) = new_api_token_record(&fx.user_id, name, AuthScope::Provision, None);
    record.child_lifetime_hours = hours;
    fx.db.create_api_token(&record).expect("token");
    raw
}

fn raw_of(reply: &Value) -> String {
    reply["raw_token"].as_str().expect("raw token").to_string()
}

fn token_count(fx: &Fixture) -> usize {
    fx.db
        .list_account_api_tokens(&fx.user_id)
        .expect("tokens")
        .len()
}

#[tokio::test]
async fn a_lead_token_should_issue_a_reviewer_child_that_ticks_and_an_implementer_that_cannot() {
    let (_dir, fx) = fixture();
    let (lead_id, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let task = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let sub = subtask_as(&fx, As::Admin, &task, "box").await;

    let (status, reviewer) = issue(
        &fx,
        &lead,
        json!({"name": "reviewer", "permissions": ["tasks.read", "subtasks.check"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reviewer}");
    assert_eq!(reviewer["parent_id"], lead_id.as_str());
    assert_eq!(
        reviewer["permissions"],
        json!(["tasks.read", "subtasks.check"])
    );
    let (status, implementer) = issue(
        &fx,
        &lead,
        json!({"name": "implementer", "permissions": ["tasks.read", "tasks.edit"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{implementer}");

    let tick = format!("/api/tasks/{task}/subtasks/{sub}");
    let done = json!({"done": true});
    let (status, text) =
        bearer_json(&fx, &raw_of(&implementer), "PATCH", &tick, done.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        permission_denied("subtasks.edit")
    );
    let (status, text) = bearer_json(&fx, &raw_of(&reviewer), "PATCH", &tick, done).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn a_child_should_be_refused_a_permission_or_board_its_parent_lacks() {
    let (_dir, fx) = fixture();
    let (other, _, _) = other_board(&fx);
    let (_, parent) = assigned_token(&fx, AuthScope::Provision, &[&fx.board_id]);
    let (_, worker) = token_record(&fx, "worker", AuthScope::Work);
    let held = token_count(&fx);

    for body in [
        json!({"name": "x", "permissions": ["boards.delete"]}),
        json!({"name": "x", "preset": "read", "boards": [other]}),
        json!({"name": "x", "preset": "read"}),
        json!({"name": "x", "preset": "read", "boards": "all"}),
    ] {
        let (status, reply) = issue(&fx, &parent, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}: {reply}");
    }
    let (status, reply) = issue(&fx, &worker, json!({"name": "x", "preset": "read"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{reply}");
    assert_eq!(reply, permission_denied("tokens.issue"));
    assert_eq!(token_count(&fx), held);

    let allowed = json!({"name": "ok", "preset": "work", "boards": [fx.board_id]});
    let (status, reply) = issue(&fx, &parent, allowed).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(
        reply["boards"],
        json!([{"id": fx.board_id, "name": "Agentctl"}])
    );
    assert_eq!(token_count(&fx), held + 1);
}

#[tokio::test]
async fn child_for_a_missing_board_should_answer_like_an_unreachable_one() {
    let (_dir, fx) = fixture();
    let (other, _, _) = other_board(&fx);
    let (_, parent) = assigned_token(&fx, AuthScope::Provision, &[&fx.board_id]);
    let (_, everything) = token_record(&fx, "everything", AuthScope::Provision);

    let missing = json!({"name": "x", "preset": "read", "boards": ["no-such-board"]});
    let unreachable = json!({"name": "x", "preset": "read", "boards": [other]});
    let (missing_status, missing_reply) = issue(&fx, &parent, missing).await;
    let (unreachable_status, unreachable_reply) = issue(&fx, &parent, unreachable).await;
    assert_eq!(missing_status, StatusCode::FORBIDDEN, "{missing_reply}");
    assert_eq!(missing_status, unreachable_status);
    assert_eq!(missing_reply, unreachable_reply);

    let reachable = json!({"name": "ok", "preset": "read", "boards": [fx.board_id]});
    let (status, reply) = issue(&fx, &parent, reachable).await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let unknown = json!({"name": "x", "preset": "read", "boards": ["no-such-board"]});
    let (status, reply) = issue(&fx, &everything, unknown).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert_eq!(reply["message"], "unknown board: no-such-board");
}

#[tokio::test]
async fn revoking_a_parent_should_make_every_child_answer_token_invalid() {
    let (_dir, fx) = fixture();
    let (lead_id, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (_, bystander) = token_record(&fx, "bystander", AuthScope::Read);
    let mut children = Vec::new();
    for name in ["one", "two"] {
        let (status, reply) = issue(&fx, &lead, json!({"name": name, "preset": "read"})).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        children.push(raw_of(&reply));
    }
    let boards = "/api/boards";
    for child in &children {
        let (status, text) = request(&fx, As::Bearer(child), "GET", boards, None, true).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    let uri = format!("/api/v1/auth/tokens/{lead_id}");
    let (status, text) = request(&fx, As::Admin, "DELETE", &uri, None, true).await;
    assert!(status.is_success(), "{text}");

    for child in &children {
        let (status, text) = request(&fx, As::Bearer(child), "GET", boards, None, true).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json")["code"],
            "token_invalid"
        );
    }
    let (status, text) = request(&fx, As::Bearer(&bystander), "GET", boards, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let left = fx.db.list_account_api_tokens(&fx.user_id).expect("tokens");
    assert!(
        left.iter().all(|token| token.parent_id.is_none()),
        "{left:?}"
    );
}

const REVOKE_SELF: &str = "/api/v1/auth/tokens/revoke-self";

fn is_token_invalid(status: StatusCode, text: &str) -> bool {
    status == StatusCode::UNAUTHORIZED
        && serde_json::from_str::<Value>(text).expect("json")["code"] == "token_invalid"
}

#[tokio::test]
async fn a_token_should_revoke_itself_without_a_permission_and_then_answer_token_invalid() {
    let (_dir, fx) = fixture();
    let (id, reader) = token_record(&fx, "reader", AuthScope::Read);
    let (status, text) = bare(&fx, &reader, "GET", "/api/boards").await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, text) = bare(&fx, &reader, "POST", REVOKE_SELF).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        json!({"revoked": id})
    );

    for uri in ["/api/boards", REVOKE_SELF] {
        let method = if uri == REVOKE_SELF { "POST" } else { "GET" };
        let (status, text) = bare(&fx, &reader, method, uri).await;
        assert!(is_token_invalid(status, &text), "{uri}: {status} {text}");
    }
}

#[tokio::test]
async fn a_token_revoking_itself_should_make_every_token_it_issued_answer_token_invalid() {
    let (_dir, fx) = fixture();
    let before = token_count(&fx);
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let mut children = Vec::new();
    for name in ["one", "two"] {
        let (status, reply) = issue(&fx, &lead, json!({"name": name, "preset": "read"})).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        children.push(raw_of(&reply));
    }
    for child in &children {
        let (status, text) = bare(&fx, child, "GET", "/api/boards").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    let (status, text) = bare(&fx, &lead, "POST", REVOKE_SELF).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    for token in children.iter().chain([&lead]) {
        let (status, text) = bare(&fx, token, "GET", "/api/boards").await;
        assert!(is_token_invalid(status, &text), "{status} {text}");
    }
    assert_eq!(token_count(&fx), before);
}

#[tokio::test]
async fn a_token_revoking_itself_should_leave_other_tokens_alone_and_refuse_a_browser_session() {
    let (_dir, fx) = fixture();
    let (_, quitter) = token_record(&fx, "quitter", AuthScope::Work);
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (_, bystander) = token_record(&fx, "bystander", AuthScope::Read);
    let (status, reply) = issue(&fx, &lead, json!({"name": "kid", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let kid = raw_of(&reply);
    let held = token_count(&fx);

    let (status, text) = request(&fx, As::Admin, "POST", REVOKE_SELF, None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(token_count(&fx), held);

    let (status, text) = bare(&fx, &quitter, "POST", REVOKE_SELF).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(token_count(&fx), held - 1);
    for token in [&lead, &bystander, &kid] {
        let (status, text) = bare(&fx, token, "GET", "/api/boards").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
}

#[tokio::test]
async fn a_child_should_not_create_browser_tokens_or_issue_without_the_right() {
    let (_dir, fx) = fixture();
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (_, admin) = token_record(&fx, "admin", AuthScope::Admin);
    let (_, worker) = token_record(&fx, "worker", AuthScope::Work);

    let body = json!({"name": "x", "permissions": ["tokens.issue"]});
    let (status, reply) = issue(&fx, &worker, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{reply}");

    let (status, child) = issue(&fx, &lead, json!({"name": "kid", "preset": "work"})).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let kid = raw_of(&child);
    let (status, reply) = issue(&fx, &kid, json!({"name": "grandkid", "preset": "read"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{reply}");
    assert_eq!(reply, permission_denied("tokens.issue"));
    let body = json!({"name": "kid2", "preset": "read"});
    let (status, text) = bearer_json(&fx, &admin, "POST", "/api/v1/auth/tokens", body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json")["code"],
        "session_required"
    );
}

#[tokio::test]
async fn a_child_should_expire_within_hours_and_never_after_its_parent() {
    let (_dir, fx) = fixture();
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let now = chrono::Utc::now().timestamp();

    let (status, child) = issue(&fx, &lead, json!({"name": "default", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let expires = child["expires_at"].as_i64().expect("expires_at");
    assert!((expires - now - 3600).abs() < 60, "{child}");
    let eight = json!({"name": "eight", "preset": "read", "expires_in_hours": 8});
    let (status, reply) = issue(&fx, &lead, eight.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    let long_lived = long_lived_issuer(&fx, "long-lived", 8);
    let (status, child) = issue(&fx, &long_lived, eight).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    for hours in [0, 9, -1] {
        let body = json!({"name": "bad", "preset": "read", "expires_in_hours": hours});
        let (status, reply) = issue(&fx, &long_lived, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hours}: {reply}");
    }

    let soon = now + 3600;
    let (mut record, short) =
        new_api_token_record(&fx.user_id, "short", AuthScope::Provision, Some(soon));
    record.child_lifetime_hours = 8;
    fx.db.create_api_token(&record).expect("token");
    let capped = json!({"name": "capped", "preset": "read", "expires_in_hours": 8});
    let (status, child) = issue(&fx, &short, capped).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    assert_eq!(child["expires_at"], soon);
}

/// Returns the id of the token in an issue reply.
fn id_of(reply: &Value) -> String {
    reply["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn root_should_issue_a_lead_that_can_issue() {
    let (_dir, fx) = fixture();
    let (root_id, root) = token_record(&fx, "root", AuthScope::Admin);

    let (status, lead) = issue(&fx, &root, json!({"name": "lead-one", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{lead}");
    assert_eq!(lead["parent_id"], root_id.as_str());
    assert!(
        lead["permissions"]
            .as_array()
            .expect("permissions")
            .contains(&json!("tokens.issue")),
        "{lead}"
    );

    let body = json!({"name": "sub-one", "preset": "work"});
    let (status, sub) = issue(&fx, &raw_of(&lead), body).await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    assert_eq!(sub["parent_id"], id_of(&lead).as_str());
}

#[tokio::test]
async fn grandchild_should_not_hold_tokens_issue() {
    let (_dir, fx) = fixture();
    let (_, root) = token_record(&fx, "root", AuthScope::Admin);
    let (status, lead) = issue(&fx, &root, json!({"name": "lead-one", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{lead}");
    let lead_raw = raw_of(&lead);
    let held = token_count(&fx);

    for body in [
        json!({"name": "x", "permissions": ["tokens.issue"]}),
        json!({"name": "x", "preset": "lead"}),
    ] {
        let (status, reply) = issue(&fx, &lead_raw, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}: {reply}");
    }
    assert_eq!(token_count(&fx), held);

    let (status, sub) = issue(&fx, &lead_raw, json!({"name": "sub", "preset": "work"})).await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    let (status, reply) = issue(&fx, &raw_of(&sub), json!({"name": "x", "preset": "read"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{reply}");
    assert_eq!(reply, permission_denied("tokens.issue"));
}

#[tokio::test]
async fn child_should_not_outlive_its_issuer() {
    let (_dir, fx) = fixture();
    let (_, root) = token_record(&fx, "root", AuthScope::Admin);
    let (status, lead) = issue(&fx, &root, json!({"name": "lead-one", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{lead}");

    let body = json!({"name": "sub", "preset": "read", "expires_in_hours": 1});
    let (status, sub) = issue(&fx, &raw_of(&lead), body).await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    assert!(
        sub["expires_at"].as_i64() <= lead["expires_at"].as_i64(),
        "{sub}"
    );

    let (status, reply) = issue(
        &fx,
        &root,
        json!({"name": "lead-two", "preset": "lead", "expires_in_hours": 2}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
}

#[tokio::test]
async fn token_create_should_store_the_child_lifetime_and_refuse_one_outside_1_to_8() {
    let (_dir, fx) = fixture();
    let (status, plain) = mint(&fx, json!({"name": "plain", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert_eq!(plain["child_lifetime_hours"], 1, "{plain}");

    let before = token_count(&fx);
    let (status, four) = mint(
        &fx,
        json!({"name": "four", "preset": "lead", "child_lifetime_hours": 4}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{four}");
    assert_eq!(four["child_lifetime_hours"], 4, "{four}");
    let uri = format!("/api/v1/auth/tokens/{}", id_of(&four));
    let (status, shown) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["child_lifetime_hours"], 4, "{shown}");

    for hours in [0, 9, -1] {
        let body = json!({"name": "bad", "preset": "lead", "child_lifetime_hours": hours});
        let (status, reply) = mint(&fx, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hours}: {reply}");
    }
    assert_eq!(token_count(&fx), before + 1);

    let body = json!({"name": "kid", "preset": "read", "expires_in_hours": 4});
    let (status, kid) = issue(&fx, &raw_of(&four), body).await;
    assert_eq!(status, StatusCode::OK, "{kid}");
}

#[tokio::test]
async fn child_token_should_refuse_a_child_lifetime_from_every_issuer() {
    let (_dir, fx) = fixture();
    let (_, root) = token_record(&fx, "root", AuthScope::Admin);
    let before = token_count(&fx);
    for hours in [0, 1, 3, 9] {
        let body = json!({"name": "bad", "preset": "lead", "child_lifetime_hours": hours});
        let (status, reply) = issue(&fx, &root, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hours}: {reply}");
    }
    assert_eq!(token_count(&fx), before);

    let (status, lead) = issue(&fx, &root, json!({"name": "lead-one", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{lead}");
    assert_eq!(lead["child_lifetime_hours"], 1, "{lead}");

    let body = json!({"name": "sub", "preset": "read", "child_lifetime_hours": 1});
    let (status, reply) = issue(&fx, &raw_of(&lead), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
}

#[tokio::test]
async fn token_edit_should_refuse_the_child_lifetime_and_keep_it() {
    let (_dir, fx) = fixture();
    let (lead_id, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let uri = format!("/api/v1/auth/tokens/{lead_id}");
    for body in [
        json!({"child_lifetime_hours": 3}),
        json!({"name": "renamed", "child_lifetime_hours": 3}),
        json!({"child_lifetime_hours": 0}),
    ] {
        let (status, reply) = call(&fx, "PATCH", &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
    }
    let (status, shown) = call(&fx, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["child_lifetime_hours"], 1);
    assert_eq!(shown["name"], "lead");

    let three = json!({"name": "three", "preset": "read", "expires_in_hours": 3});
    let (status, reply) = issue(&fx, &lead, three).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
}

#[tokio::test]
async fn issued_tokens_should_not_count_toward_the_cap() {
    let (_dir, fx) = fixture();
    let (_, root) = token_record(&fx, "root", AuthScope::Admin);
    let hand_made = |fx: &Fixture| {
        fx.db
            .list_account_api_tokens(&fx.user_id)
            .expect("tokens")
            .iter()
            .filter(|token| token.parent_id.is_none())
            .count()
    };
    for n in hand_made(&fx)..MAX_API_TOKENS_PER_ACCOUNT {
        token_record(&fx, &format!("filler-{n}"), AuthScope::Read);
    }
    assert_eq!(hand_made(&fx), MAX_API_TOKENS_PER_ACCOUNT);

    let (status, lead) = issue(&fx, &root, json!({"name": "lead-one", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{lead}");
    let (status, sub) = issue(
        &fx,
        &raw_of(&lead),
        json!({"name": "sub", "preset": "read"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    assert_eq!(token_count(&fx), MAX_API_TOKENS_PER_ACCOUNT + 2);

    let body = json!({"name": "one-too-many", "preset": "read"});
    let (status, reply) = call(&fx, "POST", "/api/v1/auth/tokens", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
}

#[tokio::test]
async fn revoking_a_token_should_revoke_its_whole_subtree() {
    let (_dir, fx) = fixture();
    let before = token_count(&fx);
    let (root_id, root) = token_record(&fx, "root", AuthScope::Admin);
    let (status, lead) = issue(&fx, &root, json!({"name": "lead-one", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{lead}");
    let (status, sub) = issue(
        &fx,
        &raw_of(&lead),
        json!({"name": "sub", "preset": "read"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    assert_eq!(token_count(&fx), before + 3);

    let uri = format!("/api/v1/auth/tokens/{}", id_of(&lead));
    let (status, text) = request(&fx, As::Admin, "DELETE", &uri, None, true).await;
    assert!(status.is_success(), "{text}");
    assert_eq!(token_count(&fx), before + 1);
    let (status, text) = bare(&fx, &raw_of(&sub), "GET", "/api/v1/auth/whoami").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");

    let (status, lead) = issue(&fx, &root, json!({"name": "lead-two", "preset": "lead"})).await;
    assert_eq!(status, StatusCode::OK, "{lead}");
    let (status, sub) = issue(
        &fx,
        &raw_of(&lead),
        json!({"name": "sub-two", "preset": "read"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    let uri = format!("/api/v1/auth/tokens/{root_id}");
    let (status, text) = request(&fx, As::Admin, "DELETE", &uri, None, true).await;
    assert!(status.is_success(), "{text}");
    assert_eq!(token_count(&fx), before);
    let (status, text) = bare(&fx, &raw_of(&sub), "GET", "/api/v1/auth/whoami").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
}

#[tokio::test]
async fn the_upkeep_should_delete_expired_issued_tokens_and_keep_hand_made_ones() {
    let (_dir, fx) = fixture();
    let (lead_id, _) = token_record(&fx, "lead", AuthScope::Provision);
    let (expired, _) = new_api_token_record(&fx.user_id, "expired-hand", AuthScope::Read, Some(1));
    fx.db.create_api_token(&expired).expect("token");
    let now = chrono::Utc::now().timestamp();
    let (mut child, _) = new_api_token_record(&fx.user_id, "old-child", AuthScope::Read, None);
    child.parent_id = Some(lead_id);
    child.parent_name = Some("lead".to_string());
    let past = now - 2 * SECONDS_PER_HOUR;
    let stored = fx
        .db
        .create_child_api_token(child, 1, past)
        .expect("create");
    assert!(stored.is_ok());
    let before = token_count(&fx);

    assert_eq!(fx.db.prune_expired_issued_tokens(now).expect("prune"), 1);
    assert_eq!(token_count(&fx), before - 1);
    assert_eq!(fx.db.prune_expired_issued_tokens(now).expect("prune"), 0);
}

#[tokio::test]
async fn the_token_list_should_name_the_parent_of_each_child() {
    let (_dir, fx) = fixture();
    let (lead_id, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (status, child) = issue(&fx, &lead, json!({"name": "kid", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{child}");

    let (status, list) = call(&fx, "GET", "/api/v1/auth/tokens", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let list = list.as_array().expect("tokens");
    let find = |name: &str| {
        list.iter()
            .find(|token| token["name"] == name)
            .expect("token")
    };
    assert_eq!(find("kid")["parent_id"], lead_id.as_str());
    assert_eq!(find("lead")["parent_id"], Value::Null);

    let (status, page) = request(&fx, As::Admin, "GET", "/settings", None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("class=\"token-child\""), "{page}");
}

#[tokio::test]
async fn only_a_token_should_issue_children() {
    let (_dir, fx) = fixture();
    let before = token_count(&fx);
    let body = json!({"name": "x", "preset": "read"});
    let (status, text) = request(&fx, As::Admin, "POST", CHILDREN, Some(body.clone()), true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let (status, _) = request(&fx, As::Anonymous, "POST", CHILDREN, Some(body), true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(token_count(&fx), before);
}

#[tokio::test]
async fn a_child_should_not_be_stored_once_its_parent_is_gone() {
    let (_dir, fx) = fixture();
    let (mut record, _) = new_api_token_record(&fx.user_id, "orphan", AuthScope::Read, None);
    record.parent_id = Some("revoked-parent".to_string());
    let before = token_count(&fx);
    let now = chrono::Utc::now().timestamp();
    let refusal = fx
        .db
        .create_child_api_token(record, 1, now)
        .expect("create");
    assert_eq!(refusal.map(|_| ()), Err(ChildTokenRefusal::ParentGone));
    assert_eq!(token_count(&fx), before);
}

/// Returns the names in a list of tokens, sorted.
fn names_of(list: &Value) -> Vec<String> {
    let mut names: Vec<String> = list
        .as_array()
        .expect("tokens")
        .iter()
        .map(|token| token["name"].as_str().expect("name").to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_lead_should_list_and_revoke_its_own_children_over_bearer() {
    let (_dir, fx) = fixture();
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let mut raws = Vec::new();
    for name in ["one", "two", "three"] {
        let (status, reply) = issue(&fx, &lead, json!({"name": name, "preset": "read"})).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        raws.push((
            reply["id"].as_str().expect("id").to_string(),
            raw_of(&reply),
        ));
    }

    let (status, text) = bare(&fx, &lead, "GET", CHILDREN).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let list: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(names_of(&list), ["one", "three", "two"]);
    for raw in raws.iter().map(|(_, raw)| raw) {
        assert!(!text.contains(raw.as_str()), "{text}");
    }

    let one = format!("{CHILDREN}/{}", raws[0].0);
    let (status, text) = bare(&fx, &lead, "DELETE", &one).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = request(
        &fx,
        As::Bearer(&raws[0].1),
        "GET",
        "/api/boards",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    let (status, text) = bare(&fx, &lead, "DELETE", &one).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");

    let (status, text) = bare(&fx, &lead, "DELETE", CHILDREN).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        json!({"revoked": 2})
    );
    for (_, raw) in &raws {
        let (status, text) = request(&fx, As::Bearer(raw), "GET", "/api/boards", None, true).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    }
    let (status, text) = request(&fx, As::Bearer(&lead), "GET", "/api/boards", None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = bare(&fx, &lead, "GET", CHILDREN).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(text, "[]");
}

#[tokio::test]
async fn a_lead_should_not_list_or_revoke_a_token_that_is_not_its_child() {
    let (_dir, fx) = fixture();
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (rival_id, rival) = token_record(&fx, "rival", AuthScope::Provision);
    let (bystander_id, bystander) = token_record(&fx, "bystander", AuthScope::Read);
    let (status, child) = issue(&fx, &rival, json!({"name": "theirs", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let theirs = child["id"].as_str().expect("id").to_string();
    let theirs_raw = raw_of(&child);
    let (status, mine) = issue(&fx, &lead, json!({"name": "mine", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let before = token_count(&fx);

    let (status, text) = bare(&fx, &lead, "GET", CHILDREN).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        names_of(&serde_json::from_str(&text).expect("json")),
        ["mine"]
    );
    for victim in [&theirs, &rival_id, &bystander_id, "no-such-token"] {
        let uri = format!("{CHILDREN}/{victim}");
        let (status, text) = bare(&fx, &lead, "DELETE", &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{victim}: {text}");
    }
    assert_eq!(token_count(&fx), before);
    let (status, text) = request(
        &fx,
        As::Bearer(&theirs_raw),
        "GET",
        "/api/boards",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, text) = bare(&fx, &lead, "DELETE", CHILDREN).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(token_count(&fx), before - 1);
    let (status, text) = request(
        &fx,
        As::Bearer(&theirs_raw),
        "GET",
        "/api/boards",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    for who in [&bystander, &theirs_raw] {
        for (method, uri) in [
            ("GET", CHILDREN.to_string()),
            ("DELETE", CHILDREN.to_string()),
            ("DELETE", format!("{CHILDREN}/{theirs}")),
        ] {
            let (status, text) = bare(&fx, who, method, &uri).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
            assert_eq!(
                serde_json::from_str::<Value>(&text).expect("json"),
                permission_denied("tokens.issue")
            );
        }
    }
    let (status, text) = request(&fx, As::Admin, "GET", CHILDREN, None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
}

#[tokio::test]
async fn revoke_all_children_in_the_browser_should_revoke_every_child_of_that_parent() {
    let (_dir, fx) = fixture();
    let (lead_id, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let (_, rival) = token_record(&fx, "rival", AuthScope::Provision);
    let mut kids = Vec::new();
    for name in ["one", "two"] {
        let (status, reply) = issue(&fx, &lead, json!({"name": name, "preset": "read"})).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        kids.push(raw_of(&reply));
    }
    let (status, rival_child) =
        issue(&fx, &rival, json!({"name": "other", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{rival_child}");

    let (status, page) = request(&fx, As::Admin, "GET", "/settings", None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("token-revoke-children"), "{page}");

    let uri = format!("/api/v1/auth/tokens/{lead_id}/children");
    let (status, text) = request(&fx, As::Bearer(&lead), "DELETE", &uri, None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let (status, text) = request(&fx, As::Admin, "DELETE", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("json"),
        json!({"revoked": 2})
    );
    for kid in &kids {
        let (status, text) = request(&fx, As::Bearer(kid), "GET", "/api/boards", None, true).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    }
    let (status, text) = request(&fx, As::Bearer(&lead), "GET", "/api/boards", None, true).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, text) = request(
        &fx,
        As::Bearer(&raw_of(&rival_child)),
        "GET",
        "/api/boards",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let missing = "/api/v1/auth/tokens/no-such-token/children";
    let (status, text) = request(&fx, As::Admin, "DELETE", missing, None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
}

/// Sends a request with no body as `token`.
async fn bare(fx: &Fixture, token: &str, method: &str, uri: &str) -> (StatusCode, String) {
    request(fx, As::Bearer(token), method, uri, None, true).await
}

#[tokio::test]
async fn a_change_by_a_child_should_show_the_child_and_its_parent_in_history() {
    let (_dir, fx) = fixture();
    let (lead_id, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let task = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let body =
        json!({"name": "kid", "permissions": ["tasks.read", "tasks.edit", "comments.write"]});
    let (status, child) = issue(&fx, &lead, body).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let kid = raw_of(&child);
    let parent = json!({"id": lead_id, "name": "lead"});

    let uri = format!("/api/tasks/{task}/comments");
    let (status, text) = request(
        &fx,
        As::Bearer(&kid),
        "POST",
        &uri,
        Some(json!({"body": "hi"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let comment: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(comment["token_name"], "kid");
    assert_eq!(comment["parent_token"], parent);
    let uri = format!("/api/tasks/{task}");
    let (status, text) = request(
        &fx,
        As::Bearer(&kid),
        "PATCH",
        &uri,
        Some(json!({"name": "Renamed"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let uri = format!("/api/tasks/{task}/comments");
    let (_, text) = request(
        &fx,
        As::Bearer(&lead),
        "POST",
        &uri,
        Some(json!({"body": "ok"})),
        true,
    )
    .await;
    let own: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(own["parent_token"], Value::Null);

    let (status, page) = request(
        &fx,
        As::Admin,
        "GET",
        &format!("/api/tasks/{task}/history-view"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("api-token (kid &lt;- lead)"), "{page}");
    let uri = format!("/api/boards/{}/export-data", fx.board_id);
    let (_, data) = call(&fx, "GET", &uri, None).await;
    let exported = data["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|row| row["id"] == task.as_str())
        .expect("task");
    let renamed = exported["history"]
        .as_array()
        .expect("history")
        .iter()
        .find(|event| event["kind"] == "renamed")
        .expect("renamed event");
    assert_eq!(renamed["token_name"], "kid");
    assert_eq!(renamed["parent_token"], parent);
}

#[tokio::test]
async fn a_child_named_like_an_existing_token_should_be_refused() {
    let (_dir, fx) = fixture();
    let (_, lead) = token_record(&fx, "lead", AuthScope::Provision);
    token_record(&fx, "deploy-bot", AuthScope::Read);
    let before = token_count(&fx);

    for name in ["deploy-bot", "Deploy-Bot", "lead", " LEAD "] {
        let (status, reply) = issue(&fx, &lead, json!({"name": name, "preset": "read"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}: {reply}");
    }
    assert_eq!(token_count(&fx), before);

    let (status, reply) = issue(&fx, &lead, json!({"name": "kid", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let (status, reply) = issue(&fx, &lead, json!({"name": "KID", "preset": "read"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(token_count(&fx), before + 1);
}

#[tokio::test]
async fn a_token_should_read_its_own_permissions_boards_and_expiry_over_bearer() {
    let (_dir, fx) = fixture();
    let lead = long_lived_issuer(&fx, "lead", 2);
    let body = json!({"name": "kid", "permissions": ["tasks.read", "subtasks.edit"], "boards": [fx.board_id], "expires_in_hours": 2});
    let (status, child) = issue(&fx, &lead, body).await;
    assert_eq!(status, StatusCode::OK, "{child}");

    let (status, text) = bare(&fx, &raw_of(&child), "GET", "/api/v1/auth/whoami").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let me: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(me["name"], "kid");
    assert_eq!(me["permissions"], json!(["tasks.read", "subtasks.edit"]));
    assert_eq!(
        me["boards"],
        json!([{"id": fx.board_id, "name": "Agentctl"}])
    );
    assert_eq!(me["expires_at"], child["expires_at"]);
}

#[tokio::test]
async fn an_expired_child_should_free_its_name_and_leave_the_child_list() {
    let (_dir, fx) = fixture();
    let (lead_id, lead) = token_record(&fx, "lead", AuthScope::Provision);
    let now = chrono::Utc::now().timestamp();
    let (mut record, _) = new_api_token_record(&fx.user_id, "worker", AuthScope::Read, None);
    record.parent_id = Some(lead_id);
    record.parent_name = Some("lead".to_string());
    let past = now - 2 * SECONDS_PER_HOUR;
    assert!(
        fx.db
            .create_child_api_token(record, 1, past)
            .expect("create")
            .is_ok()
    );

    let (status, reply) = issue(&fx, &lead, json!({"name": "worker", "preset": "read"})).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let (status, text) = bare(&fx, &lead, "GET", CHILDREN).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        names_of(&serde_json::from_str(&text).expect("json")),
        ["worker"]
    );
}

struct PartProbe {
    needs: &'static str,
    method: &'static str,
    uri: String,
    body: Value,
}

async fn part_probes(fx: &Fixture) -> Vec<PartProbe> {
    let task = create(fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    let sub = subtask_as(fx, As::Admin, &task, "box").await;
    let tasks = format!("/api/tasks/{task}");
    let probe = |needs, method, uri: String, body| PartProbe {
        needs,
        method,
        uri,
        body,
    };
    vec![
        probe(
            "tasks.create",
            "POST",
            "/api/tasks".to_string(),
            json!({"column_id": fx.col_todo, "name": "New"}),
        ),
        probe(
            "tasks.edit",
            "PATCH",
            tasks.clone(),
            json!({"name": "Renamed"}),
        ),
        probe(
            "tasks.move",
            "POST",
            format!("{tasks}/move"),
            json!({"column_id": fx.col_done, "position": 0}),
        ),
        probe(
            "tasks.claim",
            "POST",
            format!("{tasks}/claim"),
            json!({"claimant": "agent", "ttl_seconds": 600}),
        ),
        probe(
            "subtasks.edit",
            "PATCH",
            format!("{tasks}/subtasks/{sub}"),
            json!({"name": "Renamed box"}),
        ),
        probe(
            "subtasks.check",
            "PATCH",
            format!("{tasks}/subtasks/{sub}"),
            json!({"done": true}),
        ),
    ]
}

async fn token_holding(fx: &Fixture, permissions: &[&str]) -> String {
    let body = json!({"name": "holder", "permissions": permissions});
    let (status, token) = mint(fx, body).await;
    assert_eq!(status, StatusCode::OK, "{token}");
    token["raw_token"].as_str().expect("raw").to_string()
}

async fn assert_probes_follow(fx: &Fixture, token: &str, held: &[&str]) {
    for probe in part_probes(fx).await {
        let (status, text) =
            bearer_json(fx, token, probe.method, &probe.uri, probe.body.clone()).await;
        let label = format!(
            "{} {} needing {}: {text}",
            probe.method, probe.uri, probe.needs
        );
        if held.contains(&probe.needs) {
            assert!(status.is_success(), "{label}");
            continue;
        }
        assert_eq!(status, StatusCode::FORBIDDEN, "{label}");
        let refusal: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(refusal["code"], "permission_denied", "{label}");
        if probe.needs != "subtasks.check" {
            assert_eq!(refusal["permission"], probe.needs, "{label}");
        }
    }
}

async fn assert_part_needed_only_for(permission: &str) {
    let (_dir, fx) = fixture();
    let held = ["tasks.read", permission];
    let token = token_holding(&fx, &held).await;
    assert_probes_follow(&fx, &token, &held).await;
}

#[tokio::test]
async fn tasks_create_should_be_needed_only_for_creating() {
    assert_part_needed_only_for("tasks.create").await;
}

#[tokio::test]
async fn tasks_edit_should_be_needed_only_for_editing() {
    assert_part_needed_only_for("tasks.edit").await;
}

#[tokio::test]
async fn tasks_move_should_be_needed_only_for_moving() {
    assert_part_needed_only_for("tasks.move").await;
}

#[tokio::test]
async fn tasks_claim_should_be_needed_only_for_claiming() {
    assert_part_needed_only_for("tasks.claim").await;
}

#[tokio::test]
async fn subtasks_edit_should_be_needed_only_for_editing_outcomes() {
    assert_part_needed_only_for("subtasks.edit").await;
}

#[tokio::test]
async fn subtasks_check_should_be_needed_only_for_ticking() {
    assert_part_needed_only_for("subtasks.check").await;
}

async fn assert_standard_role_follows(name: &str) {
    let (_dir, fx) = fixture();
    let role = STANDARD_ROLES
        .iter()
        .find(|role| role.name == name)
        .expect("standard role");
    let permissions = role.permissions();
    let held: Vec<&str> = permissions
        .iter()
        .map(|permission| permission.name())
        .collect();
    let token = token_holding(&fx, &held).await;
    assert_probes_follow(&fx, &token, &held).await;
}

#[tokio::test]
async fn verifier_set_should_tick_but_not_edit() {
    assert_standard_role_follows("verifier").await;
}

#[tokio::test]
async fn planner_set_should_edit_but_not_tick() {
    assert_standard_role_follows("planner").await;
}

#[tokio::test]
async fn implementer_set_should_claim_and_move_but_neither_edit_nor_tick() {
    assert_standard_role_follows("implementer").await;
}

#[tokio::test]
async fn move_should_name_held_and_limit_time_with_units_when_an_hours_column_is_full() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Review", None)
        .expect("column");
    let limit = json!({"wip_limit": 4, "config_json": "{\"wip_limit_type\":\"hours\"}"});
    let (status, patched) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{limited}"),
        Some(limit),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    create(
        &fx,
        json!({"column_id": limited, "name": "Held", "estimate_hours": 3.8333333333}),
    )
    .await;
    let mover = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Mover", "estimate_hours": 1}),
    )
    .await;

    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &format!("/api/tasks/{mover}/move"),
        Some(json!({"column_id": limited, "position": 0})),
        true,
    )
    .await;

    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, "column WIP limit reached (3h 50m/4h)")
    );
}

#[tokio::test]
async fn move_should_keep_the_count_message_for_a_count_column() {
    let (_dir, fx) = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Review", Some(1))
        .expect("column");
    create(&fx, json!({"column_id": limited, "name": "Held"})).await;
    let mover = create(&fx, json!({"column_id": fx.col_todo, "name": "Mover"})).await;

    let (status, text) = request(
        &fx,
        As::Admin,
        "POST",
        &format!("/api/tasks/{mover}/move"),
        Some(json!({"column_id": limited, "position": 0})),
        true,
    )
    .await;

    assert_eq!(
        (status, text.as_str()),
        (StatusCode::CONFLICT, "column WIP limit reached (1/1)")
    );
}
