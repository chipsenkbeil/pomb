use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{Db, RestoreOutcome};
use crate::models::{AccountId, Actor, AuthScope, BIN_RETENTION_DAYS, SECONDS_PER_DAY};

struct Fixture {
    app: axum::Router,
    db: Db,
    user_id: AccountId,
    cookie: String,
    board_id: String,
    col_todo: String,
    col_done: String,
}

fn fixture() -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bin.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id).expect("session");
    let board_id = db.create_board(&user_id, "Bin").expect("board");
    let col_todo = db.create_column(&board_id, "Todo", None).expect("column");
    let col_done = db
        .create_column(&board_id, "Shipped", None)
        .expect("column");
    db.set_column_done(&col_done, true).expect("set done");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    let fixture = Fixture {
        app,
        db,
        user_id,
        cookie: format!("session={session}"),
        board_id,
        col_todo,
        col_done,
    };
    (dir, fixture)
}

async fn send(
    fx: &Fixture,
    bearer: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("accept", "application/json");
    builder = match bearer {
        Some(token) => builder.header("authorization", format!("Bearer {token}")),
        None => builder.header("cookie", &fx.cookie),
    };
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn call(fx: &Fixture, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, text) = send(fx, None, method, uri, body).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn task_in(fx: &Fixture, column: &str, name: &str) -> String {
    fx.db
        .create_task(column, None, name, 1, None)
        .expect("task")
}

fn attach(fx: &Fixture, task: &str, bytes: &[u8]) -> String {
    fx.db
        .add_attachment(task, "a.txt", "text/plain", bytes, Actor::SYSTEM)
        .expect("attach")
        .expect("task")
        .id
}

fn work_token(fx: &Fixture, scope: AuthScope) -> String {
    let (record, raw) = new_api_token_record(&fx.user_id, "agent", scope, None);
    fx.db.create_api_token(&record).expect("token");
    raw
}

fn stored(fx: &Fixture) -> u64 {
    fx.db
        .stored_bytes_by_account()
        .expect("usage")
        .get(fx.user_id.as_str())
        .copied()
        .unwrap_or_default()
}

fn kinds(fx: &Fixture, task: &str) -> Vec<String> {
    fx.db
        .task(task)
        .expect("read")
        .expect("task")
        .history
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

#[tokio::test]
async fn deleting_a_task_should_bin_it_and_hide_it_from_every_reader() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Findable");
    fx.db.create_entry(&id, 5, "note").expect("entry");
    let file = attach(&fx, &id, b"keep me");

    let (status, _) = call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, text) = send(
        &fx,
        None,
        "GET",
        &format!("/api/boards/{}/tasks", fx.board_id),
        None,
    )
    .await;
    assert!(!text.contains(&id), "{text}");
    let (_, text) = send(&fx, None, "GET", "/api/search?q=Findable", None).await;
    assert!(!text.contains(&id), "{text}");
    let (_, export) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/export-data", fx.board_id),
        None,
    )
    .await;
    assert_eq!(export["tasks"], json!([]));
    assert_eq!(export["deleted"][0]["name"], "Findable");
    assert_eq!(export["deleted"][0]["column"], "Todo");

    let (status, bin) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/bin", fx.board_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bin["retention_days"], 30);
    assert_eq!(bin["tasks"][0]["id"], id.as_str());
    assert_eq!(bin["tasks"][0]["name"], "Findable");
    assert_eq!(bin["tasks"][0]["location"], "Todo");
    assert_eq!(bin["tasks"][0]["deleted_by"], "admin");
    assert!(bin["tasks"][0]["unrestorable_after"].is_string());
    assert_eq!(
        fx.db.read_attachment_data(&file).expect("read"),
        Some(b"keep me".to_vec())
    );
}

#[tokio::test]
async fn restoring_should_return_the_task_with_its_id_entries_and_attachment() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Back again");
    fx.db.create_entry(&id, 5, "note").expect("entry");
    let file = attach(&fx, &id, b"bytes");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;

    let (status, body) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], id.as_str());
    assert_eq!(body["column_id"], fx.col_todo.as_str());

    let (status, task) = call(&fx, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["column_id"], fx.col_todo.as_str());
    assert_eq!(fx.db.list_entries(&id).expect("entries").len(), 1);
    assert_eq!(
        fx.db.read_attachment_data(&file).expect("read"),
        Some(b"bytes".to_vec())
    );
    let history = kinds(&fx, &id);
    assert!(history.iter().any(|kind| kind == "deleted"), "{history:?}");
    assert!(history.iter().any(|kind| kind == "restored"), "{history:?}");
    let (_, bin) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/bin", fx.board_id),
        None,
    )
    .await;
    assert_eq!(bin["tasks"], json!([]));
    let (status, _) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn restoring_should_use_the_first_column_when_the_old_one_is_gone() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_done, "Shipped once");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    let (status, text) = call(
        &fx,
        "DELETE",
        &format!("/api/columns/{}", fx.col_done),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, body) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["column_id"], fx.col_todo.as_str());
    assert_eq!(body["column_name"], "Todo");
}

#[tokio::test]
async fn restoring_should_hold_a_task_to_the_first_column_rule_when_the_old_one_is_gone() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_done, "No reason given");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    call(
        &fx,
        "DELETE",
        &format!("/api/columns/{}", fx.col_done),
        None,
    )
    .await;
    let rules = format!("/api/boards/{}/rules", fx.board_id);
    let rule = json!({"columns": {fx.col_todo.clone(): {"sections": ["Why"]}}});
    let (status, text) = call(&fx, "PUT", &rules, Some(rule)).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let restore = format!("/api/bin/{id}/restore");
    let (status, body) = call(&fx, "POST", &restore, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("refuses this task"), "{body}");

    let (status, text) = call(&fx, "PUT", &rules, Some(json!({"columns": {}}))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, body) = call(&fx, "POST", &restore, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn restoring_to_the_old_column_should_pass_a_rule_added_while_the_task_was_binned() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Predates the rule");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    let rule = json!({"columns": {fx.col_todo.clone(): {"sections": ["Why"]}}});
    let rules = format!("/api/boards/{}/rules", fx.board_id);
    let (status, text) = call(&fx, "PUT", &rules, Some(rule)).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, body) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_work_token_should_be_refused_the_bin_on_any_task_and_still_restore() {
    let (_dir, fx) = fixture();
    let work = work_token(&fx, AuthScope::Work);
    let plain = task_in(&fx, &fx.col_todo, "Plain");
    let (status, created) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Held", "labels": ["needs:human"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let locked = created["id"].as_str().expect("id").to_string();

    for id in [&plain, &locked] {
        let uri = format!("/api/tasks/{id}");
        let (status, text) = send(&fx, Some(&work), "DELETE", &uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert!(text.contains("permission_denied"), "{text}");
        assert!(text.contains("tasks.bin"), "{text}");
        assert!(fx.db.task(id).expect("read").is_some());
    }

    call(&fx, "DELETE", &format!("/api/tasks/{plain}"), None).await;
    let restore = format!("/api/bin/{plain}/restore");
    let (status, text) = send(&fx, Some(&work), "POST", &restore, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(fx.db.task(&plain).expect("read").is_some());
}

#[tokio::test]
async fn a_lead_token_should_delete_into_the_bin_and_a_work_token_should_restore() {
    let (_dir, fx) = fixture();
    let lead = work_token(&fx, AuthScope::Provision);
    let work = work_token(&fx, AuthScope::Work);
    let read = work_token(&fx, AuthScope::Read);
    let id = task_in(&fx, &fx.col_todo, "Agent job");

    let uri = format!("/api/tasks/{id}");
    let (status, _) = send(&fx, Some(&read), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, text) = send(&fx, Some(&lead), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(fx.db.task(&id).expect("read").is_none());

    let bin_uri = format!("/api/boards/{}/bin", fx.board_id);
    let (status, text) = send(&fx, Some(&read), "GET", &bin_uri, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains(&id));
    let restore = format!("/api/bin/{id}/restore");
    let (status, _) = send(&fx, Some(&read), "POST", &restore, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, text) = send(&fx, Some(&work), "POST", &restore, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[test]
fn a_task_past_retention_should_not_restore_and_the_cleanup_should_purge_it() {
    let (_dir, fx) = fixture();
    let before = stored(&fx);
    let id = task_in(&fx, &fx.col_todo, "Old");
    fx.db.create_entry(&id, 5, "note").expect("entry");
    let file = attach(&fx, &id, &[7; 4096]);
    assert!(fx.db.delete_task(&id, Actor::SYSTEM).expect("delete"));
    assert!(stored(&fx) > before + 4096);

    let now = chrono::Utc::now().timestamp();
    let kept = now + (BIN_RETENTION_DAYS - 1) * SECONDS_PER_DAY;
    assert_eq!(fx.db.purge_bin(kept).expect("purge"), Some(0));
    assert!(fx.db.read_attachment_data(&file).expect("read").is_some());

    let late = now + (BIN_RETENTION_DAYS + 1) * SECONDS_PER_DAY;
    assert_eq!(
        fx.db
            .restore_task(&id, late, Actor::SYSTEM)
            .expect("restore"),
        RestoreOutcome::Expired
    );
    assert_eq!(fx.db.purge_bin(kept + 1).expect("purge"), None);
    assert_eq!(fx.db.purge_bin(late).expect("purge"), Some(1));
    assert_eq!(fx.db.read_attachment_data(&file).expect("read"), None);
    assert!(fx.db.binned_tasks(&fx.board_id).expect("bin").is_empty());
    assert_eq!(stored(&fx), before);
    assert_eq!(
        fx.db
            .restore_task(&id, late, Actor::SYSTEM)
            .expect("restore"),
        RestoreOutcome::NotFound
    );
}

#[test]
fn a_binned_attachment_should_never_count_as_unnamed() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Has a file");
    let file = attach(&fx, &id, b"precious");
    assert!(fx.db.delete_task(&id, Actor::SYSTEM).expect("delete"));

    assert_eq!(fx.db.unnamed_attachment_files().expect("count").count, 0);
    assert_eq!(
        fx.db
            .remove_unnamed_attachment_files()
            .expect("remove")
            .count,
        0
    );
    assert_eq!(
        fx.db.read_attachment_data(&file).expect("read"),
        Some(b"precious".to_vec())
    );
    assert!(
        fx.db
            .missing_attachment_files()
            .expect("missing")
            .is_empty()
    );
}

#[test]
fn deleting_the_board_should_delete_its_binned_tasks_and_their_files() {
    let (_dir, fx) = fixture();
    let other = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let id = task_in(&fx, &fx.col_todo, "Doomed");
    let file = attach(&fx, &id, b"bytes");
    assert!(fx.db.delete_task(&id, Actor::SYSTEM).expect("delete"));

    fx.db.delete_board(&fx.board_id).expect("delete board");

    assert_eq!(fx.db.read_attachment_data(&file).expect("read"), None);
    assert!(fx.db.binned_tasks(&fx.board_id).expect("bin").is_empty());
    assert_eq!(
        fx.db
            .restore_task(&id, chrono::Utc::now().timestamp(), Actor::SYSTEM)
            .expect("restore"),
        RestoreOutcome::NotFound
    );
    assert!(fx.db.board(&other).expect("board").is_some());
}

#[test]
fn restoring_should_place_the_task_last_in_its_column() {
    let (_dir, fx) = fixture();
    let first = task_in(&fx, &fx.col_todo, "First");
    assert!(fx.db.delete_task(&first, Actor::SYSTEM).expect("delete"));
    let last = task_in(&fx, &fx.col_todo, "Third");
    let outcome = fx
        .db
        .restore_task(&first, chrono::Utc::now().timestamp(), Actor::SYSTEM)
        .expect("restore");
    assert!(matches!(outcome, RestoreOutcome::Restored { .. }));
    let position = |id: &str| fx.db.task(id).expect("read").expect("task").position;
    assert!(position(&first) > position(&last));
}

#[tokio::test]
async fn restoring_should_clear_a_color_that_was_deleted_while_the_task_was_binned() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Colored");
    let color_id = fx
        .db
        .list_colors(&fx.board_id)
        .expect("colors")
        .into_iter()
        .next_back()
        .expect("a seeded color")
        .id;
    let (status, text) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(serde_json::json!({"color_id": color_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    let (status, text) = call(
        &fx,
        "DELETE",
        &format!("/api/boards/{}/colors/{color_id}", fx.board_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, body) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restored = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(restored.color_id, None);
}

#[tokio::test]
async fn restoring_should_clear_a_due_target_column_that_was_deleted_while_the_task_was_binned() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Targeted");
    let (status, text) = call(
        &fx,
        "PATCH",
        &format!("/api/tasks/{id}"),
        Some(serde_json::json!({
            "due_at": "2099-01-01T09:00:00+00:00",
            "due_target_column_id": fx.col_done,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        fx.db
            .task(&id)
            .expect("read")
            .expect("task")
            .due_target_column_id,
        Some(fx.col_done.clone())
    );
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    let (status, text) = call(
        &fx,
        "DELETE",
        &format!("/api/columns/{}", fx.col_done),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (status, body) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restored = fx.db.task(&id).expect("read").expect("task");
    assert_eq!(restored.due_target_column_id, None);
}

#[tokio::test]
async fn a_permanent_delete_should_be_refused_until_the_board_allows_it() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Keep");
    let file = attach(&fx, &id, b"still here");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;

    let (status, body) = call(&fx, "DELETE", &format!("/api/bin/{id}"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (_, bin) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/bin", fx.board_id),
        None,
    )
    .await;
    assert_eq!(bin["permanent_delete"], false);
    assert_eq!(bin["tasks"][0]["id"], id.as_str());
    assert!(fx.db.read_attachment_data(&file).expect("read").is_some());
}

#[tokio::test]
async fn the_setting_should_persist_and_let_the_owner_delete_a_binned_task_for_good() {
    let (_dir, fx) = fixture();
    let empty = stored(&fx);
    let id = task_in(&fx, &fx.col_todo, "Gone");
    fx.db.create_entry(&id, 5, "note").expect("entry");
    let file = attach(&fx, &id, &[3; 2048]);
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    assert!(stored(&fx) > empty + 2048);

    let config = format!("/api/boards/{}/config", fx.board_id);
    let (status, view) = call(
        &fx,
        "PUT",
        &config,
        Some(json!({"bin_permanent_delete": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["bin_permanent_delete"], true);
    let (_, view) = call(&fx, "PUT", &config, Some(json!({}))).await;
    assert_eq!(view["bin_permanent_delete"], true);
    let (_, page) = send(
        &fx,
        None,
        "GET",
        &format!("/b/{}/settings/advanced", fx.board_id),
        None,
    )
    .await;
    assert!(
        page.contains("id=\"bin-permanent-delete\" checked"),
        "{page}"
    );

    let (_, bin) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/bin", fx.board_id),
        None,
    )
    .await;
    assert_eq!(bin["permanent_delete"], true);
    let held = stored(&fx);
    let (status, body) = call(&fx, "DELETE", &format!("/api/bin/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, bin) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/bin", fx.board_id),
        None,
    )
    .await;
    assert_eq!(bin["tasks"], json!([]));
    assert_eq!(fx.db.read_attachment_data(&file).expect("read"), None);
    assert!(stored(&fx) < held - 2048);
    let (status, _) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&fx, "DELETE", &format!("/api/bin/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_work_token_should_not_delete_from_the_bin_even_when_the_board_allows_it() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Safe");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"bin_permanent_delete": true})),
    )
    .await;
    let token = work_token(&fx, AuthScope::Work);
    let (status, _) = send(&fx, Some(&token), "DELETE", &format!("/api/bin/{id}"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_read_token_should_not_delete_from_the_bin_even_when_the_board_allows_it() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Safe");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"bin_permanent_delete": true})),
    )
    .await;
    let token = work_token(&fx, AuthScope::Read);
    let (status, _) = send(&fx, Some(&token), "DELETE", &format!("/api/bin/{id}"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_history_should_name_the_task_and_the_column_it_left_and_returned_to() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_todo, "Round trip");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    call(&fx, "POST", &format!("/api/bin/{id}/restore"), None).await;

    let details: Vec<String> = fx
        .db
        .task(&id)
        .expect("read")
        .expect("task")
        .history
        .into_iter()
        .filter(|event| event.kind == "deleted" || event.kind == "restored")
        .map(|event| event.detail)
        .collect();
    assert_eq!(
        details,
        [
            "Task Round trip was deleted from column Todo",
            "Task Round trip was restored to column Todo"
        ]
    );
}

#[tokio::test]
async fn the_bin_should_name_the_column_a_restore_puts_the_task_in() {
    let (_dir, fx) = fixture();
    let id = task_in(&fx, &fx.col_done, "Finished");
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    let uri = format!("/api/boards/{}/bin", fx.board_id);

    let (_, bin) = call(&fx, "GET", &uri, None).await;
    assert_eq!(bin["tasks"][0]["location"], "Shipped");
    assert_eq!(bin["tasks"][0]["restores_to"], "Shipped");

    let (status, _) = call(
        &fx,
        "PATCH",
        &format!("/api/columns/{}", fx.col_done),
        Some(json!({ "name": "Finale" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, bin) = call(&fx, "GET", &uri, None).await;
    assert_eq!(bin["tasks"][0]["location"], "Shipped");
    assert_eq!(bin["tasks"][0]["restores_to"], "Finale");

    let (status, _) = call(
        &fx,
        "DELETE",
        &format!("/api/columns/{}", fx.col_done),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, bin) = call(&fx, "GET", &uri, None).await;
    assert_eq!(bin["tasks"][0]["restores_to"], "Todo");
}

#[tokio::test]
async fn a_permanent_delete_should_remove_only_its_task_and_follow_its_boards_setting() {
    let (_dir, fx) = fixture();
    let first = task_in(&fx, &fx.col_todo, "First");
    let second = task_in(&fx, &fx.col_todo, "Second");
    let kept = attach(&fx, &second, b"survivor");
    let gone = attach(&fx, &first, b"doomed");
    let other_board = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let other_column = fx
        .db
        .create_column(&other_board, "Todo", None)
        .expect("column");
    let foreign = task_in(&fx, &other_column, "Foreign");
    for id in [&first, &second, &foreign] {
        call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    }
    let (status, _) = call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"bin_permanent_delete": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&fx, "DELETE", &format!("/api/bin/{first}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(fx.db.read_attachment_data(&gone).expect("read"), None);
    assert_eq!(
        fx.db.read_attachment_data(&kept).expect("read"),
        Some(b"survivor".to_vec())
    );
    let (_, bin) = call(
        &fx,
        "GET",
        &format!("/api/boards/{}/bin", fx.board_id),
        None,
    )
    .await;
    assert_eq!(bin["tasks"][0]["id"], second.as_str());
    assert_eq!(bin["tasks"].as_array().expect("tasks").len(), 1);
    let (status, _) = call(&fx, "DELETE", &format!("/api/bin/{foreign}"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, bin) = call(&fx, "GET", &format!("/api/boards/{other_board}/bin"), None).await;
    assert_eq!(bin["tasks"][0]["id"], foreign.as_str());
}

#[tokio::test]
async fn a_token_should_not_restore_or_purge_a_binned_task_with_a_locking_label() {
    let (_dir, fx) = fixture();
    let (status, created) = call(
        &fx,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": fx.col_todo, "name": "Held", "labels": ["needs:human"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    call(&fx, "DELETE", &format!("/api/tasks/{id}"), None).await;
    call(
        &fx,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"bin_permanent_delete": true})),
    )
    .await;

    let token = work_token(&fx, AuthScope::Admin);
    let restore = format!("/api/bin/{id}/restore");
    let purge = format!("/api/bin/{id}");
    for (method, uri) in [("POST", &restore), ("DELETE", &purge)] {
        let (status, text) = send(&fx, Some(&token), method, uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}: {text}");
        assert!(text.contains("card_locked"), "{method}: {text}");
    }
    let (status, body) = call(&fx, "POST", &restore, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
