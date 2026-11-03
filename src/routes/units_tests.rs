use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::db::Db;

struct Fixture {
    app: axum::Router,
    db: Db,
    cookie: String,
    board_id: String,
    col_todo: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("units.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id, "").expect("session");
    let board_id = db.create_board(&user_id, "Units").expect("board");
    let col_todo = db.create_column(&board_id, "Todo", None).expect("column");
    db.create_swimlane(&board_id, "Main").expect("lane");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    let fixture = Fixture {
        app,
        db,
        cookie: format!("session={session}"),
        board_id,
        col_todo,
    };
    (dir, fixture)
}

async fn call(fx: &Fixture, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("accept", "application/json")
        .header("cookie", &fx.cookie);
    let body = match body {
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
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn new_board(fx: &Fixture, name: &str) -> (String, String) {
    let (status, board) = call(fx, "POST", "/api/boards", Some(json!({"name": name}))).await;
    assert_eq!(status, StatusCode::OK, "{board}");
    let id = board["id"].as_str().expect("board").to_string();
    let (_, columns) = call(fx, "GET", &format!("/api/boards/{id}/columns"), None).await;
    let column = columns[0]["id"].as_str().expect("column").to_string();
    (id, column)
}

async fn add_custom(fx: &Fixture, board: &str, hex: &str, label: &str) -> (StatusCode, Value) {
    call(
        fx,
        "POST",
        &format!("/api/boards/{board}/colors"),
        Some(json!({"hex": hex, "label": label, "description": "Legend text"})),
    )
    .await
}

async fn palette(fx: &Fixture, board: &str) -> Vec<Value> {
    let (_, colors) = call(fx, "GET", &format!("/api/boards/{board}/colors"), None).await;
    colors.as_array().expect("colors").clone()
}

#[tokio::test]
async fn a_custom_color_should_have_a_hex_a_name_a_description_and_an_internal_id() {
    let (_dir, fx) = fixture();
    let (status, created) = add_custom(&fx, &fx.board_id, "#E0F2F1", "Teal").await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let colors = palette(&fx, &fx.board_id).await;
    let color = colors
        .iter()
        .find(|color| color["id"] == created["id"])
        .expect("the new color");
    assert_eq!(color["value"], "custom1");
    assert_eq!(color["bg"], "#e0f2f1");
    assert_eq!(color["label"], "Teal");
    assert_eq!(color["description"], "Legend text");
    assert_eq!(color["enabled"], true);

    let (_, second) = add_custom(&fx, &fx.board_id, "#ffeedd", "Peach").await;
    let colors = palette(&fx, &fx.board_id).await;
    let second = colors
        .iter()
        .find(|color| color["id"] == second["id"])
        .expect("the second color");
    assert_eq!(second["value"], "custom2");
}

#[tokio::test]
async fn a_low_contrast_hex_should_be_refused_with_a_lighter_suggestion() {
    let (_dir, fx) = fixture();
    let (status, error) = add_custom(&fx, &fx.board_id, "#2A9D8F", "Teal").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let message = error.as_str().expect("message");
    assert!(
        message.starts_with("The color #2A9D8F has a low contrast to black and gray text"),
        "{message}"
    );
    let suggested = message.rsplit(' ').next().expect("suggestion");
    assert!(
        suggested.starts_with('#') && suggested.len() == 7,
        "{message}"
    );
    assert_ne!(suggested, "#2a9d8f");

    let (status, created) = add_custom(&fx, &fx.board_id, suggested, "Teal").await;
    assert_eq!(status, StatusCode::OK, "{created}");

    let (status, error) = add_custom(&fx, &fx.board_id, "not-a-hex", "Bad").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
}

#[tokio::test]
async fn a_custom_color_should_be_disabled_but_never_deleted() {
    let (_dir, fx) = fixture();
    let (_, created) = add_custom(&fx, &fx.board_id, "#e0f2f1", "Teal").await;
    let uri = format!(
        "/api/boards/{}/colors/{}",
        fx.board_id,
        created["id"].as_str().expect("id")
    );
    let (status, body) = call(&fx, "PATCH", &uri, Some(json!({"enabled": false}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&fx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let colors = palette(&fx, &fx.board_id).await;
    let kept = colors
        .iter()
        .find(|color| color["id"] == created["id"])
        .expect("still there");
    assert_eq!(kept["enabled"], false);
}

#[tokio::test]
async fn a_refused_color_edit_should_leave_the_hex_alone() {
    let (_dir, fx) = fixture();
    let (_, created) = add_custom(&fx, &fx.board_id, "#e0f2f1", "Teal").await;
    let uri = format!(
        "/api/boards/{}/colors/{}",
        fx.board_id,
        created["id"].as_str().expect("id")
    );
    let (status, body) = call(
        &fx,
        "PATCH",
        &uri,
        Some(json!({"hex": "#ffeedd", "description": "x".repeat(501)})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let colors = palette(&fx, &fx.board_id).await;
    let kept = colors
        .iter()
        .find(|color| color["id"] == created["id"])
        .expect("still there");
    assert_eq!(kept["bg"], "#e0f2f1");
}

#[tokio::test]
async fn a_board_should_hold_ten_custom_colors_and_no_more() {
    let (_dir, fx) = fixture();
    for number in 1..=10 {
        let (status, created) = add_custom(&fx, &fx.board_id, "#e0f2f1", "Teal").await;
        assert_eq!(status, StatusCode::OK, "{number}: {created}");
    }
    let values: Vec<String> = palette(&fx, &fx.board_id)
        .await
        .iter()
        .filter_map(|color| color["value"].as_str())
        .filter(|value| value.starts_with("custom"))
        .map(str::to_string)
        .collect();
    assert_eq!(values.len(), 10);
    assert!(values.contains(&"custom10".to_string()));
    let (status, error) = add_custom(&fx, &fx.board_id, "#e0f2f1", "Eleventh").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
}

#[tokio::test]
async fn a_task_moved_to_another_board_should_keep_the_color_with_the_same_internal_id() {
    let (_dir, fx) = fixture();
    let (other, other_column) = new_board(&fx, "Other").await;
    let (_, here) = add_custom(&fx, &fx.board_id, "#e0f2f1", "Teal").await;
    let (_, there) = add_custom(&fx, &other, "#ffeedd", "Peach").await;
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Mover", "color_id": here["id"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let id = task["id"].as_str().expect("id");
    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{id}/move"),
        Some(json!({"column_id": other_column, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(detail["color_id"], there["id"], "{detail}");
}

#[tokio::test]
async fn a_custom_color_should_paint_the_board_page_and_the_task_modal() {
    let (_dir, fx) = fixture();
    let (_, created) = add_custom(&fx, &fx.board_id, "#e0f2f1", "Teal").await;
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Painted", "color_id": created["id"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let id = task["id"].as_str().expect("id");
    let (_, page) = call(&fx, "GET", &format!("/b/{}", fx.board_id), None).await;
    let page = page.as_str().expect("html");
    assert!(
        page.contains(".taskColor-custom1{background-color:#e0f2f1}"),
        "{page}"
    );
    assert!(
        page.contains("taskColor-custom1 taskBorderColor-custom1"),
        "{page}"
    );
    let (_, modal) = call(&fx, "GET", &format!("/api/tasks/{id}/modal"), None).await;
    let modal = modal.as_str().expect("html");
    assert!(
        modal.contains(".taskColorVars-custom1{--taskColor:#e0f2f1"),
        "{modal}"
    );
}

async fn set_points_mode(fx: &Fixture, on: bool) {
    let (status, config) = call(
        fx,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"estimate_in_points": on})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["estimate_in_points"], on);
}

async fn estimated_task(fx: &Fixture) -> String {
    let (status, task) = call(
        fx,
        "POST",
        "/api/tasks",
        Some(json!({
            "column_id": fx.col_todo,
            "name": "Sized",
            "estimate_hours": 2.0,
            "points": 5.0,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_eq!(task["points"], 5.0);
    task["id"].as_str().expect("id").to_string()
}

async fn page(fx: &Fixture, uri: &str) -> String {
    let (status, body) = call(fx, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_str().expect("html").to_string()
}

#[tokio::test]
async fn the_estimation_unit_should_switch_both_ways_and_keep_both_stored_values() {
    let (_dir, fx) = fixture();
    let id = estimated_task(&fx).await;
    for on in [true, false, true] {
        set_points_mode(&fx, on).await;
        let (_, data) = call(
            &fx,
            "GET",
            &format!("/api/boards/{}/export-data", fx.board_id),
            None,
        )
        .await;
        let task = data["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|task| task["id"] == id.as_str())
            .expect("the task");
        assert_eq!(task["estimate_hours"], 2.0, "{on}: {task}");
        assert_eq!(task["points"], 5.0, "{on}: {task}");
    }
}

#[tokio::test]
async fn a_points_board_should_show_the_badge_not_the_estimate_and_offer_points() {
    let (_dir, fx) = fixture();
    let id = estimated_task(&fx).await;

    let hours = page(&fx, &format!("/b/{}", fx.board_id)).await;
    assert!(!hours.contains("class=\"card-points\""), "{hours}");
    assert!(hours.contains("Add time estimate"));
    assert!(hours.contains("0h / 2h"), "{hours}");
    assert!(!hours.contains("Add points estimate"));

    set_points_mode(&fx, true).await;
    let points = page(&fx, &format!("/b/{}", fx.board_id)).await;
    assert!(
        points.contains("<span class=\"card-points\" title=\"Points estimate\">5</span>"),
        "{points}"
    );
    assert!(points.contains("Add points estimate</button>"));
    assert!(!points.contains("Add time estimate</button>"));
    assert!(!points.contains("0h / 2h"), "{points}");

    let modal = page(&fx, &format!("/api/tasks/{id}/modal")).await;
    assert!(modal.contains("data-estimate-unit=\"points\""), "{modal}");
    assert!(
        modal.contains("<span id=\"modal-estimate\">5 points</span>"),
        "{modal}"
    );
    assert!(modal.contains("Points estimate</button>"), "{modal}");
}

#[tokio::test]
async fn a_points_change_should_log_a_points_estimate_event_and_leave_the_hours() {
    let (_dir, fx) = fixture();
    let id = estimated_task(&fx).await;
    let kinds = || -> Vec<String> {
        fx.db
            .task(&id)
            .expect("read")
            .expect("task")
            .history
            .iter()
            .map(|event| event.kind.clone())
            .collect()
    };
    let before = kinds().len();
    let (status, body) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"points": 8.0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let after = kinds();
    assert_eq!(&after[before..], ["points_updated"], "{after:?}");
    let row = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(row.points, Some(8.0));
    assert_eq!(row.estimate_hours, Some(2.0));

    let (status, body) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(json!({"points": -1.0})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn points_should_not_count_in_the_hour_based_figures() {
    let (_dir, fx) = fixture();
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Pointed", "points": 13.0})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    set_points_mode(&fx, true).await;
    let (_, data) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/export-data", fx.board_id),
        None,
    )
    .await;
    assert!(data["tasks"][0]["estimate_hours"].is_null(), "{data}");
    let board = page(&fx, &format!("/b/{}", fx.board_id)).await;
    assert!(board.contains("data-estimate-minutes=\"0\""), "{board}");
}

async fn put_numbering(fx: &Fixture, numbering: Value) -> (StatusCode, Value) {
    call(
        fx,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({ "numbering": numbering })),
    )
    .await
}

async fn new_task(fx: &Fixture, name: &str) -> String {
    let (status, task) = call(
        fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": name})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    task["id"].as_str().expect("id").to_string()
}

async fn number_of(fx: &Fixture, id: &str) -> Value {
    let (_, task) = call(fx, "GET", &format!("/api/tasks/{id}"), None).await;
    task["number"].clone()
}

#[tokio::test]
async fn activating_numbering_should_give_each_new_task_the_next_number() {
    let (_dir, fx) = fixture();
    let before = new_task(&fx, "Before").await;
    let (status, config) =
        put_numbering(&fx, json!({"name": "Product", "prefix": "PS", "start": 7})).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["numbering"]["example"], "PS-7");
    let first = new_task(&fx, "First").await;
    let second = new_task(&fx, "Second").await;
    assert!(number_of(&fx, &before).await.is_null());
    assert_eq!(number_of(&fx, &first).await, "PS-7");
    assert_eq!(number_of(&fx, &second).await, "PS-8");
    let (_, config) = call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({})),
    )
    .await;
    assert_eq!(config["numbering"]["next"], 9);
}

#[tokio::test]
async fn a_number_should_show_on_the_card_the_modal_the_api_and_the_export() {
    let (_dir, fx) = fixture();
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    let id = new_task(&fx, "Ship it").await;
    assert_eq!(number_of(&fx, &id).await, "PS-1");

    let board = page(&fx, &format!("/b/{}", fx.board_id)).await;
    assert!(
        board.contains(
            "<span class=\"card-number\">PS-1:</span><span class=\"card-title-text\">Ship it</span>"
        ),
        "{board}"
    );
    let modal = page(&fx, &format!("/api/tasks/{id}/modal")).await;
    assert!(
        modal.contains("<span class=\"tm-number\">PS-1:</span>"),
        "{modal}"
    );
    let (_, data) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/export-data", fx.board_id),
        None,
    )
    .await;
    assert_eq!(data["tasks"][0]["number"], "PS-1", "{data}");
}

#[tokio::test]
async fn editing_the_prefix_or_deleting_should_leave_existing_numbers_alone() {
    let (_dir, fx) = fixture();
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    let first = new_task(&fx, "First").await;

    let (status, config) = put_numbering(&fx, json!({"prefix": "XY"})).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["numbering"]["name"], "Product");
    let second = new_task(&fx, "Second").await;
    assert_eq!(number_of(&fx, &first).await, "PS-1");
    assert_eq!(number_of(&fx, &second).await, "XY-2");

    let (status, config) = put_numbering(&fx, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert!(config["numbering"].is_null());
    let third = new_task(&fx, "Third").await;
    assert_eq!(number_of(&fx, &first).await, "PS-1");
    assert_eq!(number_of(&fx, &second).await, "XY-2");
    assert!(number_of(&fx, &third).await.is_null());
}

#[tokio::test]
async fn a_prefix_edit_should_not_hand_out_a_number_the_new_prefix_already_holds() {
    let (_dir, fx) = fixture();
    put_numbering(&fx, json!({"name": "Product", "prefix": "OLD"})).await;
    for name in ["One", "Two", "Three"] {
        new_task(&fx, name).await;
    }
    put_numbering(&fx, Value::Null).await;
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    new_task(&fx, "Four").await;
    let (status, config) = put_numbering(&fx, json!({"prefix": "OLD"})).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let id = new_task(&fx, "Five").await;
    assert_eq!(number_of(&fx, &id).await, "OLD-4");
}

#[tokio::test]
async fn a_number_in_the_recycle_bin_should_not_be_handed_out_again() {
    let (_dir, fx) = fixture();
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    new_task(&fx, "One").await;
    let binned = new_task(&fx, "Two").await;
    let (status, body) = call(&fx, "DELETE", &format!("/api/tasks/{binned}"), None).await;
    assert!(status.is_success(), "{body}");
    put_numbering(&fx, Value::Null).await;
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    let id = new_task(&fx, "Three").await;
    assert_eq!(number_of(&fx, &id).await, "PS-3");
}

#[tokio::test]
async fn a_number_should_never_be_reused_when_numbering_is_activated_again() {
    let (_dir, fx) = fixture();
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    new_task(&fx, "One").await;
    new_task(&fx, "Two").await;
    put_numbering(&fx, Value::Null).await;
    let (status, config) = put_numbering(&fx, json!({"name": "Again", "prefix": "PS"})).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let id = new_task(&fx, "Three").await;
    assert_eq!(number_of(&fx, &id).await, "PS-3");
}

#[tokio::test]
async fn numbering_should_refuse_a_bad_prefix_a_missing_name_and_a_start_after_activation() {
    let (_dir, fx) = fixture();
    for bad in [
        json!({"name": "Product", "prefix": "P S"}),
        json!({"name": "Product", "prefix": ""}),
        json!({"name": "Product", "prefix": "ABCDEFGHIJK"}),
        json!({"prefix": "PS"}),
        json!({"name": "", "prefix": "PS"}),
    ] {
        let (status, body) = put_numbering(&fx, bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    let (status, body) = put_numbering(&fx, json!({"start": 5})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

async fn put_config(fx: &Fixture, board: &str, body: Value) -> (StatusCode, Value) {
    call(
        fx,
        "PUT",
        &format!("/api/boards/{board}/config"),
        Some(body),
    )
    .await
}

async fn move_to(fx: &Fixture, task: &str, column: &str) {
    let (status, moved) = call(
        fx,
        "POST",
        &format!("/api/tasks/{task}/move"),
        Some(json!({"column_id": column, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
}

#[tokio::test]
async fn a_task_moved_into_a_numbered_board_should_take_the_next_number_of_that_board() {
    let (_dir, fx) = fixture();
    let (numbered, numbered_column) = new_board(&fx, "Numbered").await;
    let (_, plain_column) = new_board(&fx, "Plain").await;
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    let (status, config) = put_config(
        &fx,
        &numbered,
        json!({"numbering": {"name": "Other", "prefix": "QR", "start": 5}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let id = new_task(&fx, "Mover").await;
    assert_eq!(number_of(&fx, &id).await, "PS-1");

    move_to(&fx, &id, &numbered_column).await;
    assert_eq!(number_of(&fx, &id).await, "QR-5");
    let (_, config) = put_config(&fx, &numbered, json!({})).await;
    assert_eq!(config["numbering"]["next"], 6, "{config}");

    move_to(&fx, &id, &plain_column).await;
    assert_eq!(number_of(&fx, &id).await, "QR-5");
}

#[tokio::test]
async fn copy_colors_should_not_strand_a_task_on_a_custom_color() {
    let (_dir, fx) = fixture();
    let (source, _) = new_board(&fx, "Source").await;
    let (_, mine) = add_custom(&fx, &fx.board_id, "#e0f2f1", "Teal").await;
    let (status, task) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Painted", "color_id": mine["id"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let id = task["id"].as_str().expect("id");
    let uri = format!("/api/boards/{}/colors/copy-from", fx.board_id);
    let copy = json!({"source_board_id": source});

    let (status, body) = call(&fx, "POST", &uri, Some(copy.clone())).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "no custom color there: {body}"
    );

    add_custom(&fx, &source, "#ffeedd", "Peach").await;
    let (status, body) = call(&fx, "POST", &uri, Some(copy.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "another hex there: {body}");
    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(detail["color_id"], mine["id"], "{detail}");

    let (twin, _) = new_board(&fx, "Twin").await;
    add_custom(&fx, &twin, "#e0f2f1", "Mint").await;
    let (status, body) = call(&fx, "POST", &uri, Some(json!({"source_board_id": twin}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, detail) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    let kept = palette(&fx, &fx.board_id)
        .await
        .into_iter()
        .find(|color| color["id"] == detail["color_id"])
        .expect("the task's color is in the palette");
    assert_eq!(kept["bg"], "#e0f2f1");
}

#[tokio::test]
async fn column_sum_hours_should_follow_the_estimation_unit() {
    let (_dir, fx) = fixture();
    estimated_task(&fx).await;
    let (status, column) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{}", fx.col_todo),
        Some(json!({"config_json": json!({"column_sum": "hours"}).to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{column}");
    let hours = page(&fx, &format!("/b/{}", fx.board_id)).await;
    assert!(
        hours.contains("data-column-sum=\"hours\">2h</span>"),
        "{hours}"
    );

    set_points_mode(&fx, true).await;
    let points = page(&fx, &format!("/b/{}", fx.board_id)).await;
    assert!(!points.contains("data-column-sum=\"hours\""), "{points}");
}

#[tokio::test]
async fn a_refused_config_save_should_store_no_numbering() {
    let (_dir, fx) = fixture();
    let (_, boards) = call(&fx, "GET", "/api/boards", None).await;
    for board in boards.as_array().expect("boards") {
        let id = board["id"].as_str().expect("id");
        if id != fx.board_id {
            let (status, body) = put_config(&fx, id, json!({"archived": true})).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }
    let (status, body) = put_config(
        &fx,
        &fx.board_id,
        json!({"archived": true, "numbering": {"name": "Product", "prefix": "PS"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, config) = put_config(&fx, &fx.board_id, json!({})).await;
    assert!(config["numbering"].is_null(), "{config}");
    assert_eq!(config["archived"], false, "{config}");

    let (status, body) = put_config(
        &fx,
        &fx.board_id,
        json!({"legend_visible": true, "numbering": {"name": "Product", "prefix": "P S"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, config) = put_config(&fx, &fx.board_id, json!({})).await;
    assert_eq!(config["legend_visible"], false, "{config}");
}

#[tokio::test]
async fn numbering_start_at_the_top_of_u64_should_be_refused() {
    let (_dir, fx) = fixture();
    for start in [u64::MAX, 9_007_199_254_740_992] {
        let (status, body) = put_numbering(
            &fx,
            json!({"name": "Product", "prefix": "PS", "start": start}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{start}: {body}");
    }
    let (_, config) = put_config(&fx, &fx.board_id, json!({})).await;
    assert!(config["numbering"].is_null(), "{config}");

    let top = 9_007_199_254_740_991_u64;
    let (status, config) = put_numbering(
        &fx,
        json!({"name": "Product", "prefix": "PS", "start": top}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let id = new_task(&fx, "Last").await;
    assert_eq!(number_of(&fx, &id).await, "PS-9007199254740991");
    let (status, body) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Past the top"})),
    )
    .await;
    assert!(!status.is_success(), "{body}");
}

#[tokio::test]
async fn points_below_a_hundredth_should_be_refused() {
    let (_dir, fx) = fixture();
    let id = estimated_task(&fx).await;
    let uri = format!("/api/tasks/{id}");
    let (status, body) = call(&fx, "PATCH", &uri, Some(json!({"points": 0.004}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let row = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(row.points, Some(5.0));

    let (status, body) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Tiny", "points": 0.004})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = call(&fx, "PATCH", &uri, Some(json!({"points": 0}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(row.points, None);
}

const NUMBERING_TOP: u64 = 9_007_199_254_740_991;

#[tokio::test]
async fn create_task_should_answer_a_conflict_when_the_numbering_has_no_number_left() {
    let (_dir, fx) = fixture();
    let (status, config) = put_numbering(
        &fx,
        json!({"name": "Product", "prefix": "PS", "start": NUMBERING_TOP}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    new_task(&fx, "Last").await;

    let (status, body) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Past the top"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.as_str()
            .is_some_and(|message| message.contains("no number left")),
        "{body}"
    );
    let (_, config) = put_config(&fx, &fx.board_id, json!({})).await;
    assert_eq!(config["numbering"]["next"], NUMBERING_TOP + 1, "{config}");
}

#[tokio::test]
async fn move_task_should_answer_a_conflict_when_the_target_numbering_has_no_number_left() {
    let (_dir, fx) = fixture();
    let (full, full_column) = new_board(&fx, "Full").await;
    let (status, config) = put_config(
        &fx,
        &full,
        json!({"numbering": {"name": "Other", "prefix": "QR", "start": NUMBERING_TOP}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    put_numbering(&fx, json!({"name": "Product", "prefix": "PS"})).await;
    let first = new_task(&fx, "Mover").await;
    move_to(&fx, &first, &full_column).await;
    assert_eq!(number_of(&fx, &first).await, "QR-9007199254740991");

    let second = new_task(&fx, "Stuck").await;
    let (status, body) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{second}/move"),
        Some(json!({"column_id": full_column, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (_, task) = call(&fx, "GET", &format!("/api/tasks/{second}"), None).await;
    assert_eq!(task["column_id"], fx.col_todo, "{task}");
    assert_eq!(task["number"], "PS-2", "{task}");
}

#[tokio::test]
async fn update_board_config_should_store_nothing_when_the_numbering_resume_fails() {
    let (_dir, fx) = fixture();
    let (status, config) = put_numbering(
        &fx,
        json!({"name": "Product", "prefix": "AA", "start": NUMBERING_TOP}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let id = new_task(&fx, "Last").await;
    assert_eq!(number_of(&fx, &id).await, "AA-9007199254740991");
    put_numbering(&fx, Value::Null).await;

    let (status, body) = put_config(
        &fx,
        &fx.board_id,
        json!({"legend_visible": true, "numbering": {"name": "Again", "prefix": "AA"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, config) = put_config(&fx, &fx.board_id, json!({})).await;
    assert_eq!(config["legend_visible"], false, "{config}");
    assert!(config["numbering"].is_null(), "{config}");
}
