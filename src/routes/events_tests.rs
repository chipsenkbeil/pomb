use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::passkey_tests::call;
use super::server::{passkey_mode, unthrottled_limits};
use super::{REQUEST_TIMEOUT_SECS, router};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{Db, EventRead, MoveGuards};
use crate::models::{AccountId, AuthScope, EventKind, InstanceMode};

/// Seconds a test waits for a reply that should come at once.
const PROMPT_SECS: u64 = 5;

/// What a wait answered.
struct Answer {
    status: StatusCode,
    body: Value,
}

/// Sends a GET as `token` and returns the answer.
async fn send(app: Router, token: String, uri: String) -> Answer {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Answer { status, body }
}

/// An instance with an admin who owns a board and a member who owns none.
struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    app: Router,
    admin: AccountId,
    board: String,
    todo: String,
    done: String,
    /// The admin's `work` token, named `agent-1`.
    work: String,
    /// The admin's `read` token.
    read: String,
    /// The member's `work` token.
    member: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let token = |account: &AccountId, name: &str, scope: AuthScope| {
            let (record, raw) = new_api_token_record(account, name, scope, None);
            db.create_api_token(&record).expect("token");
            raw
        };
        let board = db.create_board(&admin, "Events").expect("board");
        let todo = db.create_column(&board, "Todo", None).expect("column");
        let done = db.create_column(&board, "Done", None).expect("column");
        let state = AppState::new(db.clone(), None, passkey_mode())
            .with_mode(InstanceMode::Multi)
            .with_limits(unthrottled_limits());
        Self {
            work: token(&admin, "agent-1", AuthScope::Work),
            read: token(&admin, "reader", AuthScope::Read),
            member: token(&member, "bob-agent", AuthScope::Work),
            app: router(state),
            _dir: dir,
            db,
            admin,
            board,
            todo,
            done,
        }
    }

    fn uri(&self, query: &str) -> String {
        format!("/api/boards/{}/events/wait?{query}", self.board)
    }

    /// Waits as `token` in a task of its own.
    fn wait(&self, token: &str, query: &str) -> JoinHandle<Answer> {
        tokio::spawn(send(self.app.clone(), token.to_string(), self.uri(query)))
    }

    /// Waits as `token` and returns the answer.
    async fn get(&self, token: &str, query: &str) -> Answer {
        send(self.app.clone(), token.to_string(), self.uri(query)).await
    }

    /// Writes an event by creating a task, and returns its id.
    fn add_task(&self, name: &str) -> String {
        self.db
            .create_task(&self.todo, None, name, 0, None)
            .expect("task")
    }
}

/// Lets the tasks the test spawned run up to their first wait.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Returns the answer of `wait`, which must come within [`PROMPT_SECS`].
async fn reply(wait: JoinHandle<Answer>) -> Answer {
    tokio::time::timeout(Duration::from_secs(PROMPT_SECS), wait)
        .await
        .expect("a prompt reply")
        .expect("task")
}

fn kinds(answer: &Answer) -> Vec<&str> {
    answer.body["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["kind"].as_str().expect("kind"))
        .collect()
}

#[tokio::test]
async fn a_wait_with_no_after_should_answer_at_once_with_the_current_cursor() {
    let fx = Fixture::new();
    let fresh = reply(fx.wait(&fx.work, "")).await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    assert_eq!(fresh.body, json!({"events": [], "cursor": "0"}));
    fx.add_task("One");
    fx.add_task("Two");
    let later = reply(fx.wait(&fx.work, "kinds=task_moved")).await;
    assert_eq!(later.body, json!({"events": [], "cursor": "2"}));
}

#[tokio::test]
async fn a_wait_should_return_at_once_with_events_after_the_cursor() {
    let fx = Fixture::new();
    let task = fx.add_task("One");
    let got = reply(fx.wait(&fx.work, "after=0")).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(got.body["cursor"], "1");
    let event = &got.body["events"][0];
    assert_eq!(event["cursor"], "1");
    assert_eq!(event["kind"], "task_added");
    assert_eq!(event["task_id"], task.as_str());
    assert_eq!(event["column_id"], fx.todo.as_str());
    assert_eq!(event["actor_kind"], "user");
    assert_eq!(event["token_name"], Value::Null);
    assert!(event["at"].as_str().is_some_and(|at| at.contains('T')));
    assert!(event.get("from_column_id").is_none());
}

#[tokio::test]
async fn a_wait_should_wake_when_an_event_is_written() {
    let fx = Fixture::new();
    let waiting = fx.wait(&fx.work, "after=0&timeout=60");
    settle().await;
    assert!(!waiting.is_finished(), "nothing to report yet");
    let task = fx.add_task("Wake");
    let got = reply(waiting).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(kinds(&got), ["task_added"]);
    assert_eq!(got.body["events"][0]["task_id"], task.as_str());
    assert_eq!(got.body["cursor"], "1");
}

#[tokio::test]
async fn a_wait_should_report_a_move_with_the_column_it_left() {
    let fx = Fixture::new();
    let task = fx.add_task("Move");
    let waiting = fx.wait(&fx.work, "after=1&kinds=task_moved");
    settle().await;
    let body = json!({"column_id": fx.done, "position": 0});
    let auth = format!("Bearer {}", fx.work);
    let moved = call(
        &fx.app,
        "POST",
        &format!("/api/tasks/{task}/move"),
        &[("authorization", &auth)],
        Some(body),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    let got = reply(waiting).await;
    let event = &got.body["events"][0];
    assert_eq!(event["kind"], "task_moved");
    assert_eq!(event["from_column_id"], fx.todo.as_str());
    assert_eq!(event["column_id"], fx.done.as_str());
    assert_eq!(event["actor_kind"], "api_token");
    assert_eq!(event["token_name"], "agent-1");
}

#[tokio::test]
async fn a_wait_should_report_who_commented_and_skip_kinds_it_was_not_asked_for() {
    let fx = Fixture::new();
    let task = fx.add_task("Talk");
    let waiting = fx.wait(&fx.work, "after=1&kinds=comment_added");
    settle().await;
    fx.db
        .set_task_labels(&task, &["urgent".to_string()])
        .expect("labels");
    settle().await;
    assert!(!waiting.is_finished(), "a label change is not a comment");
    let auth = format!("Bearer {}", fx.work);
    let posted = call(
        &fx.app,
        "POST",
        &format!("/api/tasks/{task}/comments"),
        &[("authorization", &auth)],
        Some(json!({"body": "hello"})),
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    let got = reply(waiting).await;
    assert_eq!(kinds(&got), ["comment_added"]);
    let event = &got.body["events"][0];
    assert_eq!(event["comment_id"], posted.body["id"]);
    assert_eq!(event["actor_kind"], "api_token");
    assert_eq!(event["token_name"], "agent-1");
    assert_eq!(got.body["cursor"], "3", "the skipped label event is passed");
}

#[tokio::test]
async fn a_wait_should_report_what_an_update_changed() {
    let fx = Fixture::new();
    let task = fx.add_task("Old");
    let waiting = fx.wait(&fx.work, "after=1&kinds=task_updated");
    settle().await;
    let auth = format!("Bearer {}", fx.work);
    let patched = call(
        &fx.app,
        "PATCH",
        &format!("/api/tasks/{task}"),
        &[("authorization", &auth)],
        Some(json!({"name": "New", "size": 2, "labels": ["a"]})),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);
    let event = reply(waiting).await.body["events"][0].clone();
    assert_eq!(event["kind"], "task_updated");
    assert_eq!(event["name"], "New");
    assert_eq!(event["size"], 2);
    assert_eq!(event["labels_added"], json!(["a"]));
    assert!(event.get("labels_removed").is_none());
    assert_eq!(event["token_name"], "agent-1");
}

#[tokio::test]
async fn a_wait_should_answer_200_with_no_events_when_the_timeout_passes() {
    let fx = Fixture::new();
    fx.add_task("One");
    let started = Instant::now();
    let got = reply(fx.wait(&fx.work, "after=1&timeout=1")).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(got.body, json!({"events": [], "cursor": "1"}));
    assert!(started.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn a_cursor_the_board_cannot_resume_should_answer_410_with_the_oldest() {
    let fx = Fixture::new();
    fx.add_task("One");
    let gone = fx.get(&fx.work, "after=99").await;
    assert_eq!(gone.status, StatusCode::GONE, "{}", gone.body);
    assert_eq!(gone.body["code"], "cursor_expired");
    assert_eq!(gone.body["oldest_cursor"], "0");
    assert!(gone.body["message"].is_string());
}

#[tokio::test]
async fn a_bad_parameter_should_answer_400() {
    let fx = Fixture::new();
    for query in [
        "after=abc",
        "after=-1",
        "kinds=task_added,nope",
        "timeout=soon",
        "timeout=-5",
    ] {
        let bad = fx.get(&fx.work, query).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}: {}", bad.body);
        assert_eq!(bad.body["code"], "invalid_request", "{query}");
    }
}

#[tokio::test]
async fn another_accounts_board_should_answer_404() {
    let fx = Fixture::new();
    let other = fx.get(&fx.member, "after=0").await;
    assert_eq!(other.status, StatusCode::NOT_FOUND, "{}", other.body);
    let none = fx.get(&fx.member, "").await;
    assert_eq!(none.status, StatusCode::NOT_FOUND, "{}", none.body);
}

#[tokio::test]
async fn read_and_work_tokens_should_wait() {
    let fx = Fixture::new();
    for token in [&fx.read, &fx.work] {
        let got = fx.get(token, "after=0&timeout=0").await;
        assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    }
}

#[tokio::test]
async fn a_token_should_hold_only_so_many_waits_and_a_hang_up_should_free_one() {
    let fx = Fixture::new();
    let held: Vec<_> = (0..AppState::WAITS_PER_TOKEN_AT_ONCE)
        .map(|_| fx.wait(&fx.work, "after=0&timeout=300"))
        .collect();
    settle().await;
    assert!(held.iter().all(|wait| !wait.is_finished()));
    let refused = fx.get(&fx.work, "after=0&timeout=0").await;
    assert_eq!(
        refused.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        refused.body
    );
    assert_eq!(refused.body["code"], "too_many_waits");
    let no_start = fx.get(&fx.work, "").await;
    assert_eq!(
        no_start.status,
        StatusCode::OK,
        "a wait that never starts holds no slot"
    );

    held[0].abort();
    settle().await;
    let freed = fx.get(&fx.work, "after=0&timeout=0").await;
    assert_eq!(freed.status, StatusCode::OK, "{}", freed.body);
    for wait in &held {
        wait.abort();
    }
}

#[tokio::test]
async fn an_account_should_hold_only_so_many_waits_across_its_tokens() {
    let fx = Fixture::new();
    let tokens: Vec<String> = (0..AppState::WAITS_PER_ACCOUNT_AT_ONCE
        / AppState::WAITS_PER_TOKEN_AT_ONCE)
        .map(|n| {
            let (record, raw) =
                new_api_token_record(&fx.admin, &format!("extra-{n}"), AuthScope::Work, None);
            fx.db.create_api_token(&record).expect("token");
            raw
        })
        .collect();
    let mut held = Vec::new();
    for token in &tokens {
        for _ in 0..AppState::WAITS_PER_TOKEN_AT_ONCE {
            held.push(fx.wait(token, "after=0&timeout=300"));
        }
    }
    settle().await;
    assert!(held.iter().all(|wait| !wait.is_finished()));
    let refused = fx.get(&fx.work, "after=0&timeout=0").await;
    assert_eq!(
        refused.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        refused.body
    );
    assert_eq!(refused.body["code"], "too_many_waits");
    for wait in &held {
        wait.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn a_wait_should_outlast_the_request_timeout_and_end_at_its_own() {
    let fx = Fixture::new();
    let waiting = fx.wait(&fx.work, "after=0&timeout=300");
    settle().await;
    tokio::time::advance(Duration::from_secs(REQUEST_TIMEOUT_SECS + 1)).await;
    settle().await;
    assert!(!waiting.is_finished(), "cut off by the ordinary timeout");
    tokio::time::advance(Duration::from_secs(300)).await;
    let got = reply(waiting).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(got.body, json!({"events": [], "cursor": "0"}));
}

#[tokio::test]
async fn a_move_to_another_board_should_wake_a_waiter_on_the_board_it_left() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Other").expect("board");
    let target = fx.db.create_column(&other, "Todo", None).expect("column");
    let task = fx.add_task("Travel");
    let uri = |board: &str| format!("/api/boards/{board}/events/wait?after=1&timeout=60");
    let from = tokio::spawn(send(fx.app.clone(), fx.work.clone(), uri(&fx.board)));
    settle().await;
    assert!(!from.is_finished(), "the wait is parked before the move");
    fx.db
        .move_task_logged(&task, &target, None, 0.0, MoveGuards::default())
        .expect("move");
    let got = reply(from).await;
    assert_eq!(kinds(&got), ["task_moved"]);
    let event = &got.body["events"][0];
    assert_eq!(event["column_id"], target.as_str());
    assert_eq!(event["from_column_id"], fx.todo.as_str());
    let there = fx
        .db
        .board_events(&other, 0, &EventKind::ALL, 10)
        .expect("read");
    let EventRead::Page(page) = there else {
        panic!("expired");
    };
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].1.kind, EventKind::TaskMoved);
}
