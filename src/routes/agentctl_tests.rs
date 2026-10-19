use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::BootstrapToken;
use crate::models::{AuthScope, SECONDS_PER_DAY};

struct Fixture {
    app: axum::Router,
    db: crate::db::Db,
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
    let password = crate::auth::hash_password("s3cret").expect("hash password");
    let user_id = db.create_user("admin", &password).expect("create user");
    let session = crate::auth::create_session(&db, &user_id).expect("session");
    let (record, read_token) = new_api_token_record("reader", AuthScope::Read, None);
    db.create_api_token(&record).expect("token");
    let board_id = db.create_board("Agentctl").expect("board");
    let col_todo = db.create_column(&board_id, "Todo", None).expect("column");
    let col_done = db
        .create_column(&board_id, "Shipped", None)
        .expect("column");
    db.set_column_done(&col_done, true).expect("set done");
    let lane_a = db.create_swimlane(&board_id, "A").expect("lane");
    let lane_b = db.create_swimlane(&board_id, "B").expect("lane");
    let fixture = Fixture {
        app: router(
            AppState::new(db.clone(), None).with_limits(super::server::unthrottled_limits()),
        ),
        db,
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
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
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
    use utoipa::OpenApi;

    use super::ApiDoc;

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
    use utoipa::OpenApi;

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
    assert_eq!(names, vec!["id", "If-Claim-Fence"]);
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
    use utoipa::OpenApi;

    let json = super::ApiDoc::openapi().to_json().expect("spec serializes");
    let spec: Value = serde_json::from_str(&json).expect("valid json");
    for path in [
        "/api/boards/{id}/tasks",
        "/api/tasks/{id}/links",
        "/api/tasks/{id}/links/{link_id}",
        "/api/tasks/{id}/claim",
        "/api/tasks/{id}/claim/heartbeat",
        "/api/tasks/{id}/claim/release",
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
        json!(["blocked_by", "blocks", "related"])
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
        !html.contains("modal-metadata"),
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

    let (status, text) = post(a.clone(), json!({"kind": "parent", "target_id": b})).await;
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
    for (task, kind, target) in [
        (&b, "blocked_by", &a),
        (&c, "blocked_by", &a),
        (&a, "blocks", &c),
    ] {
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
    let (_, first) = link(&fx, &a, "blocked_by", &b).await;
    let (_, second) = link(&fx, &a, "related", &b).await;

    let from_target = format!("/api/tasks/{b}/links/{}", first["id"].as_str().expect("id"));
    let (status, _) = call(&fx, "DELETE", &from_target, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!blocked(&fx, &a).await);
    assert_eq!(links_of(&fx, &b).await, vec![("related".into(), a.clone())]);
    let (status, _) = call(&fx, "DELETE", &from_target, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let from_holder = format!(
        "/api/tasks/{a}/links/{}",
        second["id"].as_str().expect("id")
    );
    let (status, _) = call(&fx, "DELETE", &from_holder, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(links_of(&fx, &a).await.is_empty() && links_of(&fx, &b).await.is_empty());

    let (status, _) = call(&fx, "GET", "/api/tasks/nope/links", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{a}/links/nope"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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

/// History event kinds on a task, oldest first.
fn event_kinds(fx: &Fixture, task: &str) -> Vec<String> {
    let row = fx.db.get_task(task).expect("read").expect("task");
    row.history.into_iter().map(|event| event.kind).collect()
}

fn count(kinds: &[String], kind: &str) -> usize {
    kinds.iter().filter(|k| *k == kind).count()
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
    assert_eq!(count(&event_kinds(&fx, &id), "claimed"), 1);
}

#[tokio::test]
async fn claim_should_renew_only_with_the_current_fence_and_bump_it_per_new_claim() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let (_, first) = claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;

    let renew = json!({"claimant": "a", "ttl_seconds": 3600, "fence": 1});
    let (status, renewed) = claim(&fx, &id, renew).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renewed["fence"], 1, "a renewal keeps the fence");
    assert_eq!(renewed["started_at"], first["started_at"]);
    assert!(renewed["expires_at"].as_str() > first["expires_at"].as_str());
    assert_eq!(
        count(&event_kinds(&fx, &id), "claimed"),
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
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let (_, first) = claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;
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
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;
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
    assert_eq!(count(&kinds, "claim_released"), 1);

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
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 1})).await;
    let base = format!("/api/boards/{}/tasks", fx.board_id);
    let (_, claimed) = call_list(&fx, "GET", &format!("{base}?claimed=true"), None).await;
    assert_eq!(ids(&claimed), vec![id.as_str()]);
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(detail["claim"], Value::Null, "reads expire lazily");
    let (_, unclaimed) = call_list(&fx, "GET", &format!("{base}?claimed=false"), None).await;
    assert_eq!(ids(&unclaimed), vec![id.as_str()]);
    assert_eq!(
        count(&event_kinds(&fx, &id), "claim_expired"),
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
        count(&kinds, "claim_expired"),
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
    assert_eq!(count(&kinds, "claim_expired"), 1);
    assert_eq!(count(&kinds, "claim_released"), 0);
    assert_eq!(count(&kinds, "claimed"), 2);
}

#[tokio::test]
async fn a_new_claim_should_log_the_expiry_of_the_old_one_first() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 1})).await;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (status, next) = claim(&fx, &id, json!({"claimant": "b", "ttl_seconds": 60})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["fence"], 2);
    let kinds = event_kinds(&fx, &id);
    let tail: Vec<&str> = kinds
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(String::as_str)
        .collect();
    assert_eq!(tail, vec!["claimed", "claim_expired", "claimed"]);
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
        let row = fx.db.get_task(&id).expect("read").expect("task");
        assert_eq!(row.claim_fence, 1, "round {round}: one fence issued");
        assert_eq!(count(&event_kinds(&fx, &id), "claimed"), 1);
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
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
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
    assert!(html.contains("<span class=\"card-claimant\">builder-i</span>"));
    assert!(html.contains("title=\"Claimed by builder-i until "));
}

/// History events of one kind on a task, as their detail strings.
fn event_details(fx: &Fixture, task: &str, kind: &str) -> Vec<String> {
    let row = fx.db.get_task(task).expect("read").expect("task");
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
        event_details(&fx, &a, "link_added"),
        vec![format!("Added link blocked_by \"Blocker\" ({b})")],
        "a blocks link is held, and logged, by the blocked task"
    );
    assert!(event_details(&fx, &b, "link_added").is_empty());
    link(&fx, &a, "blocked_by", &b).await;
    assert_eq!(
        event_details(&fx, &a, "link_added").len(),
        1,
        "no event for an existing link"
    );

    let uri = format!(
        "/api/tasks/{b}/links/{}",
        created["id"].as_str().expect("id")
    );
    call(&fx, "DELETE", &uri, None).await;
    assert_eq!(
        event_details(&fx, &a, "link_removed"),
        vec![format!("Removed link blocked_by \"Blocker\" ({b})")]
    );
    let row = fx.db.get_task(&a).expect("read").expect("task");
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
        event_details(&fx, &a, "link_removed"),
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
    let before = fx
        .db
        .get_task(&id)
        .expect("read")
        .expect("task")
        .history
        .len();
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
    let row = fx.db.get_task(&id).expect("read").expect("task");
    assert_eq!(row.metadata, None);
    assert_eq!(
        row.history.len(),
        before,
        "metadata changes are not history"
    );
}

fn updated_at(fx: &Fixture, task: &str) -> String {
    let row = fx.db.get_task(task).expect("read").expect("task");
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
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
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
        json!({"column_id": "nope", "name": "X"}),
    ] {
        let (status, _) = request(&fx, As::Admin, "POST", "/api/tasks", Some(body), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
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
        .get_task(task["id"].as_str().expect("id"))
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
    let (status, _) = link(&fx, &a, "parent", &b).await;
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
        let row = fx.db.get_task(&id).expect("read").expect("task");
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
    let stored = fx.db.get_task(&local).expect("read").expect("task");
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
    let before = |id: &str| {
        fx.db
            .get_task(id)
            .expect("read")
            .expect("task")
            .history
            .len()
    };
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
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Starts device pairing anonymously; returns the parsed response.
async fn start_device(fx: &Fixture, body: Value) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device")
        .header("host", "chipflow.test:3350")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("req");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
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
    assert_eq!(
        started["verification_url"],
        "http://chipflow.test:3350/device"
    );
    assert_eq!(started["interval_seconds"], 5);
    assert_eq!(started["expires_in_seconds"], 600);

    for (body, message) in [
        (
            json!({"scopes": ["read"]}),
            "client_name must be 1-80 characters",
        ),
        (
            json!({"client_name": "x".repeat(81), "scopes": ["read"]}),
            "client_name must be 1-80 characters",
        ),
        (
            json!({"client_name": "a"}),
            "scopes must be a non-empty subset of [\"read\", \"work\", \"admin\"]",
        ),
        (
            json!({"client_name": "a", "scopes": []}),
            "scopes must be a non-empty subset of [\"read\", \"work\", \"admin\"]",
        ),
        (
            json!({"client_name": "a", "scopes": ["read", "write"]}),
            "scopes must be a non-empty subset of [\"read\", \"work\", \"admin\"]",
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
    assert!(token.starts_with("cf_"));
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
        status: crate::models::DeviceAuthStatus::Pending,
        created_at: now - 700,
        expires_at: now - 100,
        last_poll_at: None,
        token_expires_at: None,
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
    let (record, writer) = new_api_token_record("writer", AuthScope::Admin, None);
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
        status: crate::models::DeviceAuthStatus::Pending,
        created_at: now - 3 * 86_400,
        expires_at: now - 2 * 86_400,
        last_poll_at: None,
        token_expires_at: None,
    };
    fx.db.create_device_auth(&stale, now).expect("insert");
    assert_eq!(poll_device(&fx, "stale").await.0, StatusCode::GONE);
    for _ in 0..crate::db::Db::MAX_PENDING_DEVICE_AUTHS {
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
    use utoipa::OpenApi;

    let spec = super::ApiDoc::openapi().to_json().expect("json");
    assert!(
        !spec.contains("[`"),
        "OpenAPI descriptions carry no rustdoc link syntax"
    );
}

#[test]
fn the_board_ui_should_force_every_task_move() {
    let app = include_str!("../../static/app.js");
    assert_eq!(app.matches("+ '/move?force=true'").count(), 6);
    let unforced = app
        .lines()
        .filter(|line| line.contains("'/api/tasks/'") && line.contains("+ '/move'"))
        .count();
    assert_eq!(
        unforced, 0,
        "a human drag into a full column must keep working"
    );
}

#[tokio::test]
async fn comments_should_record_how_they_were_written_and_their_kind() {
    let (_dir, fx) = fixture();
    let (record, bot) = new_api_token_record("agentctl-bot", AuthScope::Admin, None);
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
    assert_eq!(comment.actor_kind, crate::models::ActorKind::User);
    assert_eq!(comment.token_name, None);
    assert_eq!(comment.kind, None);
}

#[tokio::test]
async fn attachment_docs_and_schema_should_match_the_served_download_route() {
    use base64::Engine as _;
    use utoipa::OpenApi;

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
    serde_json::to_value(fx.db.get_task(task).expect("read").expect("task")).expect("json")
}

#[tokio::test]
async fn rejected_task_extras_should_leave_the_stored_row_unchanged() {
    use base64::Engine as _;

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
    assert!(attachment["id"].is_string());
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
    use base64::Engine as _;

    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Extras"})).await;
    let base = format!("/api/tasks/{id}");
    let history = |fx: &Fixture| -> Vec<String> {
        let row = fx.db.get_task(&id).expect("read").expect("task");
        row.history.into_iter().map(|e| e.kind).collect()
    };
    let mut expected = history(&fx);
    let mut step = |fx: &Fixture, kind: &str| {
        expected.push(kind.to_string());
        assert_eq!(history(fx), expected, "after {kind}");
    };

    call(
        &fx,
        "POST",
        &format!("{base}/watch"),
        Some(json!({"watched": true})),
    )
    .await;
    step(&fx, "watched");
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
    step(&fx, "time_logged");
    assert_eq!(fx.db.list_entries(&id).expect("entries").len(), 1);
    let (_, sub) = call(
        &fx,
        "POST",
        &format!("{base}/subtasks"),
        Some(json!({"name": "One"})),
    )
    .await;
    step(&fx, "subtask_added");
    let sub_uri = format!("{base}/subtasks/{}", sub["id"].as_str().expect("id"));
    let (_, done) = call(&fx, "PATCH", &sub_uri, Some(json!({"done": true}))).await;
    assert_eq!(done["done"], true);
    step(&fx, "subtask_done");
    let (_, renamed) = call(&fx, "PATCH", &sub_uri, Some(json!({"name": "Uno"}))).await;
    assert_eq!(renamed["name"], "Uno");
    step(&fx, "subtask_renamed");
    call(&fx, "DELETE", &sub_uri, None).await;
    step(&fx, "subtask_deleted");
    let data = base64::engine::general_purpose::STANDARD.encode(b"x");
    let (_, attachment) = call(
        &fx,
        "POST",
        &format!("{base}/attachments"),
        Some(json!({"name": "a.txt", "data": data})),
    )
    .await;
    step(&fx, "attachment_added");
    let attachment_uri = format!(
        "{base}/attachments/{}",
        attachment["id"].as_str().expect("id")
    );
    call(&fx, "DELETE", &attachment_uri, None).await;
    step(&fx, "attachment_deleted");
    let row = fx.db.get_task(&id).expect("read").expect("task");
    assert!(row.watched && row.subtasks.is_empty() && row.attachments.is_empty());
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
    let moved = event_details(&fx, &a, "moved");
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
        event_details(&fx, &b, "moved"),
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
        event_details(&fx, &c, "moved"),
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
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Lapsed"})).await;
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 1})).await;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let before = updated_at(&fx, &id);
    let (status, _) = claim_call(&fx, &id, "/heartbeat", json!({"fence": 1})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        updated_at(&fx, &id) > before,
        "the documented exception: expiry is recorded"
    );
    assert_eq!(count(&event_kinds(&fx, &id), "claim_expired"), 1);
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

    use utoipa::OpenApi;
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
    assert!(html.contains("No longer options: api, rest"));
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
        .header("host", "chipflow.test:3350")
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
        ("origin", "http://chipflow.test:3350"),
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
        let row = fx.db.get_task(&s).expect("read").expect("task");
        assert_eq!(
            row.claim.as_ref().map(|c| c.fence),
            Some(fence),
            "round {round}: the granted claim survived"
        );
        fences.push(fence);
        let (status, _) = claim_call(&fx, &s, "/release", json!({"fence": fence})).await;
        assert_eq!(status, StatusCode::OK);
        let row = fx.db.get_task(&s).expect("read").expect("task");
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
    let row = fx.db.get_task(&s).expect("read").expect("task");
    assert_eq!(row.claim_fence, 60);
    assert_eq!(count(&event_kinds(&fx, &s), "claimed"), 60);
    assert_eq!(count(&event_kinds(&fx, &s), "claim_released"), 60);
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
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Renew"})).await;
    claim(&fx, &id, json!({"claimant": "a", "ttl_seconds": 60})).await;
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
    assert_eq!(count(&event_kinds(&fx, &id), "claimed"), 1);
}

fn token_with(fx: &Fixture, name: &str, scope: AuthScope) -> String {
    let (record, raw) = new_api_token_record(name, scope, None);
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

    for (method, uri) in [
        ("DELETE", format!("/api/boards/{}", fx.board_id)),
        ("DELETE", format!("/api/tasks/{id}")),
        ("PUT", format!("/api/boards/{}/config", fx.board_id)),
        ("GET", format!("/api/boards/{}/calendar-feed", fx.board_id)),
    ] {
        let (status, text) = request(&fx, As::Bearer(&work), method, &uri, None, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            json!({"code": "insufficient_scope", "message": "insufficient token scope: needs admin"}),
            "{method} {uri}"
        );
    }
    let (status, _) = call(&fx, "GET", &format!("/api/boards/{}", fx.board_id), None).await;
    assert_eq!(status, StatusCode::OK, "the board survived");
}

#[tokio::test]
async fn a_read_token_should_be_refused_every_write_and_an_admin_token_none() {
    let (_dir, fx) = fixture();
    let id = create(&fx, json!({"column_id": fx.col_todo, "name": "Job"})).await;
    for (method, uri, needs) in [
        ("POST", "/api/tasks".to_string(), "work"),
        ("PATCH", format!("/api/tasks/{id}"), "work"),
        ("POST", format!("/api/tasks/{id}/move"), "work"),
        ("POST", format!("/api/tasks/{id}/claim"), "work"),
        ("POST", "/api/timer/start".to_string(), "work"),
        ("DELETE", format!("/api/tasks/{id}"), "admin"),
        ("POST", "/api/boards".to_string(), "admin"),
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
        let body: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(body["code"], "insufficient_scope", "{method} {uri}");
        assert_eq!(
            body["message"],
            format!("insufficient token scope: needs {needs}")
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
async fn approving_should_lower_the_scope_and_set_the_expiry() {
    let day = SECONDS_PER_DAY;
    let near = |expires: &Value, days: i64| {
        let want = chrono::Utc::now().timestamp() + days * day;
        (expires.as_i64().expect("expires_at") - want).abs() < 60
    };

    let (status, _, whoami) = approve_with("work", "&scope=read&expires_in_days=30").await;
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

    let (_, _, whoami) = approve_with("admin", "&scope=work&expires_in_days=365").await;
    let whoami = whoami.expect("whoami");
    assert_eq!(whoami["scope"], "work");
    assert!(near(&whoami["expires_at"], 365));

    let (_, _, whoami) = approve_with("read", "&scope=read&expires_in_days=never").await;
    let whoami = whoami.expect("whoami");
    assert_eq!(whoami["scope"], "read");
    assert_eq!(whoami["expires_at"], Value::Null);
}

#[tokio::test]
async fn approving_should_refuse_a_raised_scope_or_an_unoffered_expiry() {
    for form in [
        "&scope=admin",
        "&scope=work",
        "&scope=write",
        "&scope=",
        "&expires_in_days=7",
        "&expires_in_days=0",
        "&expires_in_days=-30",
        "&expires_in_days=",
    ] {
        let (status, page, whoami) = approve_with("read", form).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{form}: {page}");
        assert!(whoami.is_none());
        assert!(
            page.contains("name=\"expires_in_days\""),
            "{form}: the form is shown again"
        );
    }

    let (_dir, fx) = fixture();
    let (device_code, user_code) = paired(&fx, json!(["read"])).await;
    for _ in 0..12 {
        let (status, _) = form_post(
            &fx,
            As::Admin,
            "/device/approve",
            &format!("user_code={user_code}&scope=admin"),
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
        &format!("user_code={user_code}&scope=read"),
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
async fn the_approval_page_should_offer_only_lower_scopes_and_warn_about_admin() {
    for (requested, offered, warned) in [
        ("read", vec!["read"], false),
        ("work", vec!["read", "work"], false),
        ("admin", vec!["read", "work", "admin"], true),
    ] {
        let (_dir, fx) = fixture();
        let (_, user_code) = paired(&fx, json!([requested])).await;
        let (status, page) =
            form_post(&fx, As::Admin, "/device", &format!("user_code={user_code}")).await;
        assert_eq!(status, StatusCode::OK);
        for scope in ["read", "work", "admin"] {
            let option = format!("<option value=\"{scope}\"");
            assert_eq!(
                page.contains(&option),
                offered.contains(&scope),
                "{requested}: {scope}"
            );
        }
        assert!(page.contains(&format!("<option value=\"{requested}\" selected>")));
        assert_eq!(page.contains("device-warning"), warned, "{requested}");
        for expiry in ["30", "90", "365", "never"] {
            assert!(
                page.contains(&format!("<option value=\"{expiry}\"")),
                "{expiry}"
            );
        }
        assert!(page.contains("<option value=\"90\" selected>"));
    }
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
    const BOOTSTRAP: &str = "cf_bootstrap_for_history_test";
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
        AppState::new(fx.db.clone(), BootstrapToken::new(BOOTSTRAP))
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

    let events = |task: &str| -> Vec<(String, Option<String>)> {
        let row = fx.db.get_task(task).expect("read").expect("task");
        row.history
            .into_iter()
            .map(|event| (event.kind, event.token_name))
            .collect()
    };
    let name = Some("claude-code@laptop".to_string());
    let history = events(&id);
    for kind in [
        "claimed",
        "comment_added",
        "time_logged",
        "link_added",
        "attachment_added",
        "claim_released",
    ] {
        assert!(
            history.contains(&(kind.to_string(), name.clone())),
            "{kind} lacks the token name: {history:?}"
        );
    }
    for (kind, token) in events(&other) {
        if kind == "comment_added" {
            assert_eq!(token, None, "cookie and bootstrap comments carry no name");
        }
    }
    assert_eq!(
        events(&other)
            .iter()
            .filter(|(kind, _)| kind == "comment_added")
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
