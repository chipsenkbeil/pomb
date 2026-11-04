use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::server::{passkey_mode, unthrottled_limits};
use super::{EPIC_TEMPLATE, router};
use crate::AppState;
use crate::db::Db;
use crate::models::AccountId;

struct Fixture {
    db: Db,
    user_id: AccountId,
    app: axum::Router,
    cookie: String,
    board_id: String,
    col_todo: String,
    col_limited: String,
    col_moves: String,
    lane_a: String,
    lane_b: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("epics.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id, "").expect("session");
    let board_id = db.create_board(&user_id, "Epics").expect("board");
    let col_todo = db.create_column(&board_id, "Todo", None).expect("column");
    let col_limited = db
        .create_column(&board_id, "Limited", Some(1))
        .expect("column");
    let col_moves = db
        .create_column(&board_id, "Moves", Some(2))
        .expect("column");
    let lane_a = db.create_swimlane(&board_id, "A").expect("lane");
    let lane_b = db.create_swimlane(&board_id, "B").expect("lane");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    let user_id = user_id.clone();
    let fixture = Fixture {
        db,
        user_id,
        app,
        cookie: format!("session={session}"),
        board_id,
        col_todo,
        col_limited,
        col_moves,
        lane_a,
        lane_b,
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
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn create(fx: &Fixture, body: Value) -> String {
    let (status, task) = call(fx, "POST", "/api/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    task["id"].as_str().expect("id").to_string()
}

async fn board_page(fx: &Fixture) -> String {
    let (status, page) = call(fx, "GET", &format!("/b/{}", fx.board_id), None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page.as_str().expect("html").to_string()
}

fn chunk_of<'a>(page: &'a str, opener: &str, needle: &str) -> &'a str {
    page.split(opener)
        .find(|chunk| chunk.contains(needle))
        .unwrap_or_else(|| panic!("no {opener} holding {needle}"))
}

fn attribute(chunk: &str, name: &str) -> String {
    let start = chunk
        .find(&format!("{name}=\""))
        .unwrap_or_else(|| panic!("no {name}"))
        + name.len()
        + 2;
    let rest = &chunk[start..];
    rest[..rest.find('"').expect("closing quote")].to_string()
}

fn header_chunk<'a>(page: &'a str, column: &str) -> &'a str {
    chunk_of(
        page,
        "<th class=\"columnHeader",
        &format!("data-column-id=\"{column}\""),
    )
}

fn lane_label_chunk<'a>(page: &'a str, lane: &str) -> &'a str {
    chunk_of(
        page,
        "<th class=\"swimlane-label",
        &format!("data-swimlane-id=\"{lane}\""),
    )
}

fn cell_label(page: &str, column: &str, lane: &str) -> (String, bool) {
    let chunk = chunk_of(
        page,
        "<td class=\"board-cell",
        &format!("data-column-id=\"{column}\" data-swimlane-id=\"{lane}\""),
    );
    let at = chunk.find("<span class=\"lane-cellCount").expect("count");
    let span = &chunk[at..];
    let open_end = span.find('>').expect("open tag end");
    let text = &span[open_end + 1..span.find("</span>").expect("close")];
    (text.to_string(), span[..open_end].contains("--over"))
}

#[tokio::test]
async fn the_board_page_should_draw_no_epic_and_count_none_in_column_totals() {
    let (_dir, fx) = fixture();
    create(
        &fx,
        json!({
            "column_id": fx.col_limited, "swimlane_id": fx.lane_a, "name": "Epic Card Zed",
            "kind": "epic", "estimate_hours": 10, "due_at": "2020-01-01T10:00:00Z",
        }),
    )
    .await;
    create(
        &fx,
        json!({
            "column_id": fx.col_limited, "swimlane_id": fx.lane_a, "name": "Work Card Zed",
            "estimate_hours": 2,
        }),
    )
    .await;

    let page = board_page(&fx).await;
    assert!(
        page.contains("Work Card Zed"),
        "the board page draws the work card"
    );
    assert!(
        !page.contains("Epic Card Zed"),
        "the board page draws no epic card"
    );
    let head = header_chunk(&page, &fx.col_limited);
    assert_eq!(
        attribute(head, "data-task-count"),
        "1",
        "column badge and data-task-count: the column badge counts work only"
    );
    assert_eq!(
        attribute(head, "data-estimate-minutes"),
        "120",
        "Estimated hours: the column hours total sums work only"
    );
    assert_eq!(
        attribute(head, "data-overdue-count"),
        "0",
        "column overdue count skips the overdue epic"
    );
    assert!(
        !head.contains("limitWarning"),
        "column WIP warning compares work only against the limit of 1"
    );
    assert!(head.contains(">1 / 1<"), "column WIP label shows work only");
}

#[tokio::test]
async fn the_board_page_should_count_no_epic_in_lane_cells_limits_or_lane_totals() {
    let (_dir, fx) = fixture();
    let limits = json!({"limits": [{"column_id": fx.col_todo, "max": 1}]});
    let (status, patched) = call(
        &fx,
        "PATCH",
        &format!("/api/swimlanes/{}", fx.lane_a),
        Some(limits),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    create(
        &fx,
        json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_a, "name": "Epic", "kind": "epic"}),
    )
    .await;
    create(
        &fx,
        json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_a, "name": "Work"}),
    )
    .await;
    create(
        &fx,
        json!({"column_id": fx.col_limited, "swimlane_id": fx.lane_a, "name": "Epic 2", "kind": "epic"}),
    )
    .await;

    let page = board_page(&fx).await;
    let (label, over) = cell_label(&page, &fx.col_todo, &fx.lane_a);
    assert_eq!(label.trim(), "1 / 1", "lane cell count counts work only");
    assert!(
        !over,
        "lane limit warning compares work only against the lane limit of 1"
    );
    let (label, _) = cell_label(&page, &fx.col_limited, &fx.lane_a);
    assert_eq!(
        label.trim(),
        "",
        "an empty cell with an epic: the lane cell label shows nothing"
    );
    assert_eq!(
        attribute(lane_label_chunk(&page, &fx.lane_a), "data-task-count"),
        "1",
        "lane task count counts work only"
    );
    assert_eq!(
        attribute(lane_label_chunk(&page, &fx.lane_b), "data-task-count"),
        "0",
        "an empty lane task count stays 0"
    );
}

#[tokio::test]
async fn a_move_should_ignore_an_epic_in_the_target_columns_wip_count() {
    let (_dir, fx) = fixture();
    create(
        &fx,
        json!({"column_id": fx.col_moves, "name": "Epic", "kind": "epic"}),
    )
    .await;
    create(&fx, json!({"column_id": fx.col_moves, "name": "Held"})).await;
    let first = create(&fx, json!({"column_id": fx.col_todo, "name": "First"})).await;
    let second = create(&fx, json!({"column_id": fx.col_todo, "name": "Second"})).await;
    let target = json!({"column_id": fx.col_moves, "position": 0});

    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{first}/move"),
        Some(target.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "move into a WIP-limited column: the epic is not counted against the limit of 2: {moved}"
    );
    let (status, error) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{second}/move"),
        Some(target),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "move into a WIP-limited column: two work cards fill the limit of 2: {error}"
    );
    assert_eq!(error, "column WIP limit reached (2/2)");
}

#[tokio::test]
async fn a_claim_should_refuse_an_epic_with_an_open_child_and_grant_it_once_all_are_done() {
    let (_dir, fx) = fixture();
    let (status, done) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{}", fx.col_moves),
        Some(json!({"is_done": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let finished = create(&fx, json!({"column_id": fx.col_moves, "name": "Finished"})).await;
    let open = create(&fx, json!({"column_id": fx.col_todo, "name": "Open"})).await;
    adopt(&fx, &finished, &epic).await;
    adopt(&fx, &open, &epic).await;
    let body = json!({"claimant": "agent", "ttl_seconds": 60});
    let path = format!("/api/tasks/{epic}/claim");

    let modal = modal_of(&fx, &epic).await;
    let claim_group = section(&modal, "tm-group-claim");
    assert!(!claim_group.contains("data-claim-take"), "{claim_group}");
    assert!(!claim_group.contains("tm-claim-lease"), "{claim_group}");
    let (status, error) = call(&fx, "POST", &path, Some(body.clone())).await;
    assert_eq!(status, StatusCode::CONFLICT, "claiming an epic: {error}");
    assert_eq!(
        error,
        "an epic can be claimed once every child is done, and 1 child is still open"
    );

    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{open}/move"),
        Some(json!({"column_id": fx.col_moves, "position": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let modal = modal_of(&fx, &epic).await;
    let claim_group = section(&modal, "tm-group-claim");
    assert!(claim_group.contains("data-claim-take"), "{claim_group}");
    assert!(claim_group.contains("tm-claim-lease"), "{claim_group}");
    let (status, claim) = call(&fx, "POST", &path, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "claiming a finished epic: {claim}");
    assert_eq!(claim["claimant"], "agent");
    let modal = modal_of(&fx, &epic).await;
    let claim_group = section(&modal, "tm-group-claim");
    assert!(claim_group.contains("data-claim-release"), "{claim_group}");
}

#[tokio::test]
async fn a_claim_should_still_grant_work() {
    let (_dir, fx) = fixture();
    let work = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let body = json!({"claimant": "agent", "ttl_seconds": 60});
    let (status, claim) = call(&fx, "POST", &format!("/api/tasks/{work}/claim"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "claiming work: {claim}");
}

#[tokio::test]
async fn ready_listing_should_omit_an_epic_while_the_plain_listing_keeps_it() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let work = create(&fx, json!({"column_id": fx.col_todo, "name": "Work"})).await;
    let waiting = create(&fx, json!({"column_id": fx.col_todo, "name": "Waiting"})).await;
    let (status, linked) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{waiting}/links"),
        Some(json!({"kind": "blocked_by", "target_id": work})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    let base = format!("/api/boards/{}/tasks", fx.board_id);

    let (status, ready) = call(&fx, "GET", &format!("{base}?ready=true"), None).await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    let ready_ids: Vec<&str> = ready["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|task| task["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ready_ids,
        [work.as_str()],
        "the ready=true listing leaves the epic out"
    );

    let (status, blocked) = call(&fx, "GET", &format!("{base}?ready=false"), None).await;
    assert_eq!(status, StatusCode::OK, "{blocked}");
    let blocked_ids: Vec<&str> = blocked["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|task| task["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        blocked_ids,
        [waiting.as_str()],
        "the ready=false listing holds blocked work only, not the epic"
    );

    let (status, all) = call(&fx, "GET", &base, None).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    let listed = all["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|task| task["id"] == epic.as_str())
        .expect("the plain listing keeps the epic");
    assert_eq!(listed["kind"], "epic");
    assert_eq!(listed["closed"], false);
}

#[tokio::test]
async fn an_hours_limited_column_should_not_count_an_epics_estimate() {
    let (_dir, fx) = fixture();
    let limit = json!({"wip_limit": 3, "config_json": "{\"wip_limit_type\":\"hours\"}"});
    let (status, patched) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{}", fx.col_limited),
        Some(limit),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    create(
        &fx,
        json!({"column_id": fx.col_limited, "name": "Epic", "kind": "epic", "estimate_hours": 10}),
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
    let target = json!({"column_id": fx.col_limited, "position": 0});

    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{first}/move"),
        Some(target.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "move into an hours-limited column ignores the epic's 10h: {moved}"
    );
    let (status, error) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{second}/move"),
        Some(target),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "move into an hours-limited column refuses 4h of work against 3h: {error}"
    );
}

async fn cell_order(fx: &Fixture, lane: &str, column: &str) -> Vec<String> {
    let (status, page) = call(
        fx,
        "GET",
        &format!(
            "/api/boards/{}/tasks?column_id={column}&swimlane_id={lane}",
            fx.board_id
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|task| task["name"].as_str().expect("name").to_string())
        .collect()
}

#[tokio::test]
async fn a_drop_should_count_its_position_among_work_cards_only() {
    let (_dir, fx) = fixture();
    let cell = |name: &str, kind: &str| json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_a, "name": name, "kind": kind});
    create(&fx, cell("E", "epic")).await;
    create(&fx, cell("A", "task")).await;
    create(&fx, cell("B", "task")).await;
    let dropped = create(
        &fx,
        json!({"column_id": fx.col_moves, "swimlane_id": fx.lane_a, "name": "C"}),
    )
    .await;

    let target = json!({
        "column_id": fx.col_todo, "swimlane_id": fx.lane_a, "position": 1,
    });
    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{dropped}/move"),
        Some(target),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let order = cell_order(&fx, &fx.lane_a, &fx.col_todo).await;
    let work: Vec<&str> = order
        .iter()
        .map(String::as_str)
        .filter(|name| *name != "E")
        .collect();
    assert_eq!(
        work,
        ["A", "C", "B"],
        "a drop at index 1 among the visible cards lands below A, not below the hidden epic"
    );

    let target = json!({"column_id": fx.col_todo, "swimlane_id": fx.lane_a, "position": 0});
    let (status, moved) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{dropped}/move"),
        Some(target),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let order = cell_order(&fx, &fx.lane_a, &fx.col_todo).await;
    let work: Vec<&str> = order
        .iter()
        .map(String::as_str)
        .filter(|name| *name != "E")
        .collect();
    assert_eq!(
        work,
        ["C", "A", "B"],
        "a same-cell reorder to index 0 among the visible cards"
    );
}

#[tokio::test]
async fn board_page_should_draw_the_epics_button_right_after_the_filter_button() {
    let (_dir, fx) = fixture();
    let page = board_page(&fx).await;
    let filter = page.find("id=\"filter-btn\"").expect("filter button");
    let filter_end =
        filter + page[filter..].find("</button>").expect("filter close") + "</button>".len();
    let epics_id = page.find("id=\"epics-btn\"").expect("epics button");
    let epics_tag = page[..epics_id].rfind("<button").expect("epics tag");
    assert!(filter_end <= epics_tag);
    assert_eq!(page[filter_end..epics_tag].trim(), "");
}

#[tokio::test]
async fn board_page_should_draw_a_hidden_empty_epics_panel_before_the_filter_panel() {
    let (_dir, fx) = fixture();
    let page = board_page(&fx).await;
    let panel = page.find("id=\"epics-panel\"").expect("epics panel");
    let filter_panel = page.find("id=\"filter-panel\"").expect("filter panel");
    assert!(panel < filter_panel);
    let tag_start = page[..panel].rfind("<aside").expect("aside");
    let tag_end = panel + page[panel..].find('>').expect("tag end");
    assert!(page[tag_start..tag_end].contains("hidden"));
    assert!(page.contains("id=\"epics-body\""));
    assert!(page.contains("id=\"epics-close\""));
}

async fn modal_of(fx: &Fixture, id: &str) -> String {
    let (status, modal) = call(fx, "GET", &format!("/api/tasks/{id}/modal"), None).await;
    assert_eq!(status, StatusCode::OK, "{modal}");
    modal.as_str().expect("html").to_string()
}

async fn adopt(fx: &Fixture, child: &str, epic: &str) {
    let (status, linked) = call(
        fx,
        "POST",
        &format!("/api/tasks/{child}/links"),
        Some(json!({"kind": "parent", "target_id": epic})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
}

fn section<'a>(modal: &'a str, id: &str) -> &'a str {
    let start = modal
        .find(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("no section {id}"));
    let rest = &modal[start..];
    &rest[..rest.find("</section>").expect("section close")]
}

#[tokio::test]
async fn an_epics_modal_should_open_with_about_progress_its_cards_then_comments() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;

    let about = modal.find(">About</h3>").expect("About heading");
    let cards = modal.find(">Its cards</h3>").expect("Its cards heading");
    let comments = modal.find("id=\"tm-panel-comments\"").expect("comments");
    assert!(about < cards && cards < comments, "{modal}");
    assert!(
        modal.contains("class=\"task-modal taskColorVars-"),
        "{modal}"
    );
    assert!(modal.contains(" tm-epic\""), "{modal}");
    assert!(!modal.contains("role=\"tablist\""), "{modal}");
}

#[tokio::test]
async fn an_epics_about_should_start_from_the_template_headings_and_stay_editable() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    let about = section(&modal, "tm-panel-description");

    for heading in ["Problem", "Goal", "Approach", "Out of scope"] {
        assert!(about.contains(&format!(">{heading}</h2>")), "{about}");
    }
    assert!(
        about.contains(
            "data-description=\"## Problem\n\n## Goal\n\n## Approach\n\n## Out of scope\""
        ),
        "{about}"
    );
    assert!(about.contains("id=\"modal-description-text\""), "{about}");
    assert!(about.contains("aria-label=\"Edit about\""), "{about}");

    let (status, patched) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{epic}"),
        Some(json!({"description": "Just one line"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    let modal = modal_of(&fx, &epic).await;
    let about = section(&modal, "tm-panel-description");
    assert!(about.contains("Just one line"), "{about}");
    assert!(!about.contains("Problem"), "{about}");
}

#[tokio::test]
async fn an_epics_modal_should_show_the_progress_as_n_of_m_done() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    assert!(modal.contains("0 of 0 done"), "{modal}");
    assert!(modal.contains("aria-valuemax=\"0\""), "{modal}");
    assert!(modal.contains("style=\"width: 0%\""), "{modal}");

    let (status, done) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{}", fx.col_moves),
        Some(json!({"is_done": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    for (name, column) in [
        ("One", &fx.col_todo),
        ("Two", &fx.col_todo),
        ("Three", &fx.col_moves),
        ("Four", &fx.col_moves),
    ] {
        let child = create(&fx, json!({"column_id": column, "name": name})).await;
        adopt(&fx, &child, &epic).await;
    }
    let modal = modal_of(&fx, &epic).await;
    assert!(modal.contains("2 of 4 done"), "{modal}");
    assert!(modal.contains("style=\"width: 50%\""), "{modal}");
}

#[tokio::test]
async fn an_epics_cards_should_list_each_with_its_column_a_done_mark_and_a_foreign_board_name() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let near = create(&fx, json!({"column_id": fx.col_todo, "name": "Near card"})).await;
    adopt(&fx, &near, &epic).await;
    let other = fx.db.create_board(&fx.user_id, "Elsewhere").expect("board");
    let shipped = fx
        .db
        .create_column(&other, "Shipped", None)
        .expect("column");
    let (status, done) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{shipped}"),
        Some(json!({"is_done": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let far = fx
        .db
        .create_task(&shipped, None, "Far card", 1, None)
        .expect("task");
    adopt(&fx, &far, &epic).await;

    let modal = modal_of(&fx, &epic).await;
    let cards = section(&modal, "tm-panel-children");
    let near_row = chunk_of(cards, "<li ", "Near card");
    assert!(near_row.contains("Todo</span>"), "{near_row}");
    assert!(!near_row.contains("tm-relation-board"), "{near_row}");
    assert!(!near_row.contains("&#10003;"), "{near_row}");
    let far_row = chunk_of(cards, "<li ", "Far card");
    assert!(far_row.contains("tm-child-done"), "{far_row}");
    assert!(
        far_row.contains("class=\"tm-relation-board\">Elsewhere<"),
        "{far_row}"
    );
    assert!(far_row.contains("&#10003; Shipped</span>"), "{far_row}");
    assert!(
        far_row.contains(&format!("data-open-task=\"{far}\"")),
        "{far_row}"
    );
}

#[tokio::test]
async fn an_empty_epic_should_still_show_its_cards_with_an_empty_note() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    let cards = section(&modal, "tm-panel-children");
    assert!(cards.contains("Its cards"), "{cards}");
    assert!(cards.contains("tm-children-empty"), "{cards}");
    assert!(!cards.contains("<li "), "{cards}");
    assert!(!cards.contains("data-children-hide-done"), "{cards}");
    assert!(
        cards.contains("id=\"tm-children-note\" hidden"),
        "the all-done note stays hidden: {cards}"
    );
}

#[tokio::test]
async fn an_epics_modal_should_omit_the_size_colour_pomodori_and_time_controls() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    for gone in [
        "data-cell=\"color\"",
        "data-tm-act=\"pick-color\"",
        "id=\"modal-color-picker\"",
        "data-cell=\"time-spent\"",
        "data-tm-act=\"start-pomodoro\"",
        "data-tm-act=\"start-stopwatch\"",
        "data-tm-act=\"estimate\"",
        "data-tm-act=\"manual-time\"",
        "data-cell=\"estimate\"",
        "tm-timer-menu",
        "tm-pomodori",
    ] {
        assert!(!modal.contains(gone), "{gone} in an epic's modal");
    }
    assert!(
        !modal.contains("tm-pill-closed"),
        "an open epic shows no Closed pill"
    );
}

#[tokio::test]
async fn a_closed_epic_should_show_a_closed_pill() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let (status, closed) = call(&fx, "POST", &format!("/api/tasks/{epic}/close"), None).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    let modal = modal_of(&fx, &epic).await;
    assert!(
        modal.contains("<span class=\"tm-pill tm-pill-closed\">Closed</span>"),
        "{modal}"
    );
}

#[tokio::test]
async fn a_plain_cards_modal_should_keep_its_controls_and_show_no_about() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let modal = modal_of(&fx, &card).await;
    for kept in [
        "data-cell=\"color\"",
        "data-tm-act=\"pick-color\"",
        "id=\"modal-color-picker\"",
        "data-cell=\"time-spent\"",
        "data-tm-act=\"start-pomodoro\"",
        "data-tm-act=\"estimate\"",
        "data-cell=\"estimate\"",
        "tm-timer-menu",
        "role=\"tablist\"",
        "id=\"tm-tab-description\"",
    ] {
        assert!(
            modal.contains(kept),
            "{kept} missing from a plain card's modal"
        );
    }
    for absent in [
        ">About</h3>",
        "Its cards",
        " tm-epic\"",
        "tm-pill-closed",
        "id=\"tm-panel-children\"",
    ] {
        assert!(!modal.contains(absent), "{absent} in a plain card's modal");
    }
    assert!(modal.contains("Add description"), "{modal}");
}

#[tokio::test]
async fn an_epics_menus_should_omit_the_entries_its_modal_has_no_panel_for() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let entries = [
        "data-tm-act=\"add-subtask\"",
        "data-tm-act=\"add-attachment\"",
        "data-tm-act=\"add-relation\"",
        "data-tm-act=\"history\"",
    ];

    let modal = modal_of(&fx, &epic).await;
    for entry in entries {
        assert!(
            !action_menus(&modal).contains(entry),
            "{entry} in an epic's menus"
        );
    }
    let modal = modal_of(&fx, &card).await;
    for entry in entries {
        assert!(
            action_menus(&modal).contains(entry),
            "{entry} missing from a plain card's menus"
        );
    }
}

fn action_menus(modal: &str) -> &str {
    let start = modal.find("id=\"tm-add-menu\"").expect("add menu");
    let end = modal.find("id=\"tm-more-menu\"").expect("more menu");
    &modal[start..end]
}

#[tokio::test]
async fn a_plain_cards_details_and_more_menu_should_not_offer_link_a_child() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let modal = modal_of(&fx, &card).await;
    for absent in [
        "Link a child",
        "data-tm-act=\"link-child\"",
        "data-link-open=\"child\"",
        "data-new-child-open",
        "Add existing",
    ] {
        assert!(!modal.contains(absent), "{absent} in a plain card's modal");
    }
}

#[tokio::test]
async fn a_plain_card_with_children_should_list_them_without_offering_to_add_more() {
    let (_dir, fx) = fixture();
    let parent = create(&fx, json!({"column_id": fx.col_todo, "name": "Parent"})).await;
    let child = create(&fx, json!({"column_id": fx.col_todo, "name": "Child"})).await;
    adopt(&fx, &child, &parent).await;
    let modal = modal_of(&fx, &parent).await;
    let children = section(&modal, "tm-panel-children");
    assert!(children.contains("data-child-id"), "{children}");
    for absent in [
        "Link a child",
        "data-link-open=\"child\"",
        "data-new-child-open",
    ] {
        assert!(!modal.contains(absent), "{absent} in {modal}");
    }
}

#[tokio::test]
async fn an_epics_cards_head_should_offer_add_existing_and_new_child() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    let cards = section(&modal, "tm-panel-children");
    assert!(
        cards.contains("data-link-open=\"child\">+ Add existing</button>"),
        "{cards}"
    );
    assert!(
        cards.contains("data-new-child-open>+ New child</button>"),
        "{cards}"
    );
    for hooks in [
        "data-new-child-row",
        "data-new-child-input",
        "data-new-child-create",
        "data-new-child-cancel",
    ] {
        assert!(cards.contains(hooks), "{hooks} missing from {cards}");
    }
    assert!(!modal.contains("Link a child"), "{modal}");
    assert!(!modal.contains("data-tm-act=\"link-child\""), "{modal}");
}

#[tokio::test]
async fn an_epics_modal_should_carry_its_own_column_for_a_new_child() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    assert!(
        modal.contains(&format!("data-column-id=\"{}\"", fx.col_todo)),
        "{modal}"
    );
}

async fn post_task(fx: &Fixture, accept: Option<&str>) -> (StatusCode, String) {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri("/api/tasks")
        .header("cookie", &fx.cookie)
        .header("content-type", "application/json");
    if let Some(accept) = accept {
        builder = builder.header("accept", accept);
    }
    let body = json!({"column_id": fx.col_todo, "name": "Made"}).to_string();
    let response = fx
        .app
        .clone()
        .oneshot(builder.body(axum::body::Body::from(body)).expect("request"))
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn creating_a_task_as_the_session_user_should_return_json_only_when_asked_for_json() {
    let (_dir, fx) = fixture();
    let (status, body) = post_task(&fx, Some("application/json")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let task: Value = serde_json::from_str(&body).expect("json body");
    assert!(
        task["id"].as_str().is_some_and(|id| !id.is_empty()),
        "{body}"
    );

    let (status, body) = post_task(&fx, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(serde_json::from_str::<Value>(&body).is_err(), "{body}");
    assert!(body.contains("Made"), "{body}");
    assert!(body.trim_start().starts_with('<'), "{body}");
}

#[tokio::test]
async fn board_page_should_hand_the_epic_template_to_the_epics_panel() {
    let (_dir, fx) = fixture();
    let page = board_page(&fx).await;
    let panel = page.find("id=\"epics-panel\"").expect("epics panel");
    let tag = &page[page[..panel].rfind("<aside").expect("aside")..];
    let tag = &tag[..tag.find('>').expect("tag end")];
    assert_eq!(attribute(tag, "data-epic-template"), EPIC_TEMPLATE);
}

fn more_menu(modal: &str) -> &str {
    let start = modal.find("id=\"tm-more-menu\"").expect("more menu");
    let rest = &modal[start..];
    &rest[..rest.find("\n        </div>").expect("menu close")]
}

fn iconbar_delete(modal: &str) -> &str {
    let start = modal
        .find("class=\"tm-icon-btn\" data-tm-act=\"delete\"")
        .expect("iconbar delete");
    let rest = &modal[start..];
    &rest[..rest.find("</button>").expect("button close")]
}

#[tokio::test]
async fn an_open_epics_more_menu_should_offer_close_delete_and_make_it_a_card() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    let menu = more_menu(&modal);
    for shown in [
        "data-tm-act=\"close-epic\">Close epic<",
        "data-tm-act=\"delete\">Delete epic<",
        "data-tm-act=\"make-card\">Make it a card<",
        "data-tm-act=\"task-url\"",
        "data-tm-act=\"shortcuts\"",
    ] {
        assert!(menu.contains(shown), "{shown} missing from {menu}");
    }
    for hidden in [
        "reopen-epic",
        "Reopen epic",
        "tm-watch-btn",
        "data-tm-act=\"watch\"",
        "data-tm-act=\"copy\"",
        "Copy task",
        "Link a child",
    ] {
        assert!(!menu.contains(hidden), "{hidden} in {menu}");
    }
}

#[tokio::test]
async fn a_closed_epics_more_menu_should_offer_reopen_instead_of_close() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let (status, closed) = call(&fx, "POST", &format!("/api/tasks/{epic}/close"), None).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    let modal = modal_of(&fx, &epic).await;
    let menu = more_menu(&modal);
    assert!(
        menu.contains("data-tm-act=\"reopen-epic\">Reopen epic<"),
        "{menu}"
    );
    assert!(!menu.contains("close-epic"), "{menu}");
    assert!(menu.contains("Delete epic") && menu.contains("Make it a card"));
}

#[tokio::test]
async fn an_epics_modal_should_label_the_iconbar_delete_delete_epic() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    let button = iconbar_delete(&modal);
    assert!(button.contains("aria-label=\"Delete epic\""), "{button}");
    assert!(
        button.contains("<span class=\"tm-icon-label\">Delete epic</span>"),
        "{button}"
    );
}

#[tokio::test]
async fn an_epics_modal_should_carry_its_child_counts_for_the_close_warning() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let child = create(&fx, json!({"column_id": fx.col_todo, "name": "Child"})).await;
    adopt(&fx, &child, &epic).await;
    let modal = modal_of(&fx, &epic).await;
    assert!(
        modal.contains("data-children-done=\"0\" data-children-total=\"1\""),
        "{modal}"
    );
}

#[tokio::test]
async fn a_plain_cards_more_menu_should_keep_watch_and_copy_and_no_epic_actions() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let modal = modal_of(&fx, &card).await;
    let menu = more_menu(&modal);
    for shown in [
        "id=\"tm-watch-btn\"",
        "data-tm-act=\"task-url\"",
        "data-tm-act=\"copy\">Copy task<",
        "data-tm-act=\"shortcuts\"",
    ] {
        assert!(menu.contains(shown), "{shown} missing from {menu}");
    }
    for hidden in ["close-epic", "reopen-epic", "Delete epic", "make-card"] {
        assert!(!modal.contains(hidden), "{hidden} in a plain card's modal");
    }
    let button = iconbar_delete(&modal);
    assert!(button.contains("aria-label=\"Delete\""), "{button}");
    assert!(
        button.contains("<span class=\"tm-icon-label\">Delete</span>"),
        "{button}"
    );
    assert!(!modal.contains("data-children-total"), "{modal}");
}

fn context_menu(page: &str) -> &str {
    let start = page.find("id=\"card-ctx-menu\"").expect("card menu");
    let rest = &page[start..];
    &rest[..rest.find("</div>").expect("menu close")]
}

#[tokio::test]
async fn a_plain_cards_more_menu_should_offer_make_it_an_epic() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let modal = modal_of(&fx, &card).await;
    let menu = more_menu(&modal);
    assert!(
        menu.contains("data-tm-act=\"make-epic\">Make it an epic<"),
        "{menu}"
    );
}

#[tokio::test]
async fn an_epics_more_menu_should_not_offer_make_it_an_epic() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    assert!(!modal.contains("make-epic"), "{modal}");
    assert!(!modal.contains("Make it an epic"), "{modal}");
}

#[tokio::test]
async fn the_board_card_menu_and_dialog_should_offer_make_it_an_epic() {
    let (_dir, fx) = fixture();
    let page = board_page(&fx).await;
    let menu = context_menu(&page);
    assert!(
        menu.contains("data-card-act=\"make-epic\">"),
        "make-epic missing from {menu}"
    );
    assert!(menu.contains("</span>Make it an epic</button>"), "{menu}");
    for id in [
        "id=\"make-epic-dialog\"",
        "id=\"make-epic-message\"",
        "id=\"make-epic-subtasks\"",
        "id=\"make-epic-list\"",
        "id=\"make-epic-ok\"",
    ] {
        assert!(page.contains(id), "{id} missing from the board page");
    }
}

#[tokio::test]
async fn the_board_card_menu_should_offer_add_to_epic_beside_make_it_an_epic() {
    let (_dir, fx) = fixture();
    let page = board_page(&fx).await;
    let menu = context_menu(&page);
    assert!(
        menu.contains("data-card-act=\"add-to-epic\" aria-haspopup=\"menu\">"),
        "add-to-epic missing from {menu}"
    );
    assert!(
        menu.contains("</span>Add to epic <span class=\"menu-arrow\""),
        "{menu}"
    );
    let make = menu.find("data-card-act=\"make-epic\"").expect("make-epic");
    let add = menu
        .find("data-card-act=\"add-to-epic\"")
        .expect("add-to-epic");
    assert!(make < add, "{menu}");
    assert!(page.contains("id=\"card-ctx-submenu\""), "submenu missing");
}

#[tokio::test]
async fn a_cards_more_menu_should_offer_add_to_epic_with_a_hidden_epic_list() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let modal = modal_of(&fx, &card).await;
    let menu = more_menu(&modal);
    assert!(
        menu.contains(
            "data-tm-act=\"add-to-epic\" aria-haspopup=\"menu\" aria-expanded=\"false\">Add to epic"
        ),
        "{menu}"
    );
    assert!(
        menu.contains("id=\"tm-epic-list\" class=\"tm-menu-sub\" hidden"),
        "{menu}"
    );
    assert!(menu.contains("data-tm-act=\"shortcuts\""), "{menu}");
}

#[tokio::test]
async fn an_epics_more_menu_should_offer_add_to_epic_to_nest_it() {
    let (_dir, fx) = fixture();
    let epic = create(
        &fx,
        json!({"column_id": fx.col_todo, "name": "Epic", "kind": "epic"}),
    )
    .await;
    let modal = modal_of(&fx, &epic).await;
    let menu = more_menu(&modal);
    assert!(menu.contains("data-tm-act=\"add-to-epic\""), "{menu}");
    assert!(menu.contains("id=\"tm-epic-list\""), "{menu}");
    assert!(menu.contains("data-tm-act=\"make-card\""), "{menu}");
}

#[tokio::test]
async fn making_a_card_an_epic_should_turn_its_ticked_subtasks_into_the_epics_cards() {
    let (_dir, fx) = fixture();
    let card = create(&fx, json!({"column_id": fx.col_todo, "name": "Card"})).await;
    let mut subs = Vec::new();
    for name in ["Ticked", "Left alone"] {
        let (status, sub) = call(
            &fx,
            "POST",
            &format!("/api/tasks/{card}/subtasks"),
            Some(json!({"name": name})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{sub}");
        subs.push(sub["id"].as_str().expect("sub id").to_string());
    }
    let (status, made) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{card}"),
        Some(json!({"kind": "epic"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, converted) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{card}/subtasks/{}/convert", subs[0]),
        Some(json!({"relation": "none"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{converted}");
    assert_eq!(converted["column_id"], fx.col_todo.as_str(), "{converted}");
    let new_card = converted["id"].as_str().expect("new id").to_string();
    let (status, linked) = call(
        &fx,
        "POST",
        &format!("/api/tasks/{card}/links"),
        Some(json!({"kind": "child", "target_id": new_card})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");

    let modal = modal_of(&fx, &card).await;
    let cards = section(&modal, "tm-panel-children");
    assert!(
        cards.contains(&format!("data-child-id=\"{new_card}\"")),
        "{cards}"
    );
    assert!(cards.contains("Ticked"), "{cards}");
    let (status, epic) = call(&fx, "GET", &format!("/api/tasks/{card}"), None).await;
    assert_eq!(status, StatusCode::OK, "{epic}");
    let left: Vec<&str> = epic["subtasks"]
        .as_array()
        .expect("subtasks")
        .iter()
        .filter_map(|sub| sub["name"].as_str())
        .collect();
    assert_eq!(left, ["Left alone"], "{epic}");
}
