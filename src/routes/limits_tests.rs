use std::collections::BTreeSet;
use std::convert::Infallible;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use flate2::read::GzDecoder;
use http_body::Frame;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::server::{passkey_mode, unthrottled_limits};
use super::slot::SEND_SECS;
use super::test_support::{Reply, call, held_disk};
use super::{
    AppError, REQUEST_TIMEOUT_SECS, ROUTE_ACCESS, SLOW_REQUEST_TIMEOUT_SECS, Writes, router,
};
use crate::AppState;
use crate::api_error::ErrorCode;
use crate::auth::new_api_token_record;
use crate::db::{
    AccountDb, AccountSlot, Bound, Db, DeviceCodeLockout, FencedWrite, MAX_API_TOKENS_PER_ACCOUNT,
    MAX_HISTORY_TEXT_CHARS, MAX_TASK_ROW_BYTES, MAX_TEMPLATE_ROW_BYTES, NewComment,
    NewTaskPlacement, QuotaExceeded,
};
use crate::disk::{Clock as GuardClock, DiskGuard, DiskProbe, MIB};
use crate::limit::Limits;
use crate::models::{
    AccountId, ActiveTimer, Actor, ActorKind, AuthScope, BIN_RETENTION_DAYS, BoardAccess, Cap,
    DiskLimits, HistoryKind, InstanceMode, InstanceSettings, LinkKind, PruneSettings, Quota,
    Quotas, RateLimits, SECONDS_PER_DAY, SubtaskTemplate, TaskCard, TaskClaim, TextCap, TimerMode,
};
use crate::passkey::CeremoniesFull;
use crate::permissions::Permission;

/// Free space of a disk with room to spare, in MiB.
const ROOMY_MIB: u64 = 100_000;
/// A database larger than the default limit, in MiB.
const OVERSIZED_MIB: u64 = DiskLimits::DEFAULT_MAX_DATABASE_MIB as u64 + 1;
/// Free space below the default floor, in MiB.
const CRAMPED_MIB: u64 = DiskLimits::DEFAULT_MIN_FREE_MIB as u64 - 1;

/// A clock the test moves by hand.
#[derive(Clone)]
struct Clock(Arc<Mutex<Instant>>);

impl Clock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn limits(&self) -> Limits {
        let now = self.0.clone();
        Limits::with_clock(Arc::new(move || *now.lock().expect("clock")))
    }

    fn guard(&self) -> GuardClock {
        let now = self.0.clone();
        Arc::new(move || *now.lock().expect("clock"))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().expect("clock") += by;
    }
}

/// Returns a probe that measures a database of `database_mib` and `free_mib` free on every disk.
fn probe(database_mib: u64, free_mib: u64) -> DiskProbe {
    DiskProbe {
        stored: Arc::new(move || Ok(database_mib * MIB)),
        file: Arc::new(move || Ok(database_mib * MIB)),
        free: Arc::new(move |_| Ok(free_mib * MIB)),
    }
}

/// Returns a disk guard that measures through `probe` on a clock that stands still.
fn guard(probe: DiskProbe) -> DiskGuard {
    DiskGuard::with_clock(probe, Path::new("."), Clock::new().guard())
}

/// Returns a disk guard over a small database on a roomy disk.
fn roomy() -> DiskGuard {
    guard(probe(1, ROOMY_MIB))
}

/// An instance with its admin and a member, each with a session and an admin-scope token.
struct Fixture {
    /// Keeps the database directory alive.
    dir: tempfile::TempDir,
    /// The database the router serves.
    db: Db,
    /// The router under test.
    app: Router,
    /// The state the router serves, sharing its slots.
    state: AppState,
    /// The admin's account.
    admin: AccountId,
    /// The member's account.
    member: AccountId,
    /// The admin's session cookie.
    admin_cookie: String,
    /// The member's session cookie.
    member_cookie: String,
    /// The admin's `Authorization` header value.
    admin_bearer: String,
    /// The member's `Authorization` header value.
    member_bearer: String,
}

impl Fixture {
    /// Opens an instance in `mode` that limits requests with `limits` and guards with the guard
    /// `disk` builds for its database.
    fn new(mode: InstanceMode, limits: Limits, disk: impl FnOnce(&Db) -> DiskGuard) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("limits.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.apply_mode(mode).expect("mode");
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let cookie = |account: &AccountId| {
            format!(
                "session={}",
                db.create_session(account, "").expect("session")
            )
        };
        let bearer = |account: &AccountId| {
            let (record, raw) = new_api_token_record(account, "agent", AuthScope::Admin, None);
            db.create_api_token(&record).expect("token");
            format!("Bearer {raw}")
        };
        let state = AppState::new(db.clone(), None, passkey_mode())
            .with_mode(mode)
            .with_limits(limits)
            .with_disk(disk(&db));
        Self {
            admin_cookie: cookie(&admin),
            member_cookie: cookie(&member),
            admin_bearer: bearer(&admin),
            member_bearer: bearer(&member),
            app: router(state.clone()),
            state,
            dir,
            db,
            admin,
            member,
        }
    }

    /// Opens a multi-mode instance without rate limits on a roomy disk.
    fn multi() -> Self {
        Self::new(InstanceMode::Multi, unthrottled_limits(), |_| roomy())
    }

    /// Opens a multi-mode instance without rate limits whose disk guard measures through `probe`.
    fn probed(probe: DiskProbe) -> Self {
        Self::new(InstanceMode::Multi, unthrottled_limits(), |_| guard(probe))
    }

    /// Sends `body` to `uri` as the member's token.
    async fn member(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let auth = [("authorization", self.member_bearer.as_str())];
        call(&self.app, method, uri, &auth, body).await
    }

    /// Sends `body` to `uri` with the admin's session.
    async fn admin(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let cookie = [("cookie", self.admin_cookie.as_str())];
        call(&self.app, method, uri, &cookie, body).await
    }

    /// Stores the instance settings `body` with the admin's session.
    async fn put_settings(&self, body: Value) -> Reply {
        self.admin("PUT", "/api/v1/admin/settings", Some(body))
            .await
    }

    /// Stores `quotas`, `limits` and `disk` with requests to join closed.
    async fn configure(&self, quotas: Quotas, limits: RateLimits, disk: DiskLimits) {
        let saved = self.put_settings(settings(quotas, limits, disk)).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    }

    /// Stores `quotas` and the default limits.
    async fn set_quotas(&self, quotas: Quotas) {
        self.configure(quotas, RateLimits::default(), DiskLimits::default())
            .await;
    }

    /// Stores `disk` and the default quotas and rate limits.
    async fn set_disk(&self, disk: DiskLimits) {
        self.configure(Quotas::default(), RateLimits::default(), disk)
            .await;
    }

    /// Creates a board with one column for `owner` directly, returning the board and column ids.
    fn board(&self, owner: &AccountId, name: &str) -> (String, String) {
        let board = self.db.create_board(owner, name).expect("board");
        let column = self.db.create_column(&board, "Todo", None).expect("column");
        (board, column)
    }

    /// Returns the directory beside the database that holds the attachment files.
    fn attachments_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("limits.redb.attachments")
    }

    /// Creates a task in a fresh board of the member directly, returning its column and task ids.
    fn member_task(&self) -> (String, String) {
        let (_, column) = self.board(&self.member, "Board");
        let task = self
            .db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        (column, task)
    }
}

/// Returns a full `PUT /api/v1/admin/settings` body.
fn settings(quotas: Quotas, limits: RateLimits, disk: DiskLimits) -> Value {
    json!({
        "registration": "invite",
        "pending_cap": InstanceSettings::DEFAULT_PENDING_CAP,
        "quotas": quotas,
        "limits": limits,
        "disk": disk,
        "prune": PruneSettings::default(),
    })
}

/// Returns the `POST /api/tasks` body of a task named `name` in `column`.
fn new_task(column: &str, name: &str) -> Value {
    json!({"column_id": column, "name": name})
}

/// Asserts that `reply` is the 409 for `quota` at `limit`.
fn assert_quota(reply: &Reply, quota: Quota, limit: u32) {
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.code.as_deref(), Some("quota_exceeded"));
    let message = reply.body["message"].as_str().expect("message");
    let named = match quota {
        Quota::StoredMibPerAccount => format!("this account has used its {limit} MiB of storage"),
        _ => format!("at most {limit} {}", Bound::Quota(quota).describe()),
    };
    assert!(message.contains(&named), "{message}");
}

/// Asserts that `reply` is the 507 for a full database or disk.
fn assert_storage_full(reply: &Reply) {
    assert_eq!(
        reply.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        reply.body
    );
    assert_eq!(reply.code.as_deref(), Some("storage_full"));
}

#[tokio::test]
async fn quota_should_word_the_storage_refusal_as_a_sentence() {
    const LIMIT_MIB: u32 = 1;
    const FILE_BYTES: usize = 1000 * 1024;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let refused = fx.upload(&task, "b.bin", vec![2; FILE_BYTES]).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(
        refused.body["message"].as_str(),
        Some("this account has used its 1 MiB of storage")
    );
}

#[tokio::test]
async fn quota_should_refuse_board_over_limit() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        boards_per_account: LIMIT,
        ..Quotas::default()
    })
    .await;
    for n in 0..LIMIT {
        let body = json!({"name": format!("Board {n}")});
        let made = fx.member("POST", "/api/boards", Some(body)).await;
        assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    }
    let refused = fx
        .member("POST", "/api/boards", Some(json!({"name": "Over"})))
        .await;
    assert_quota(&refused, Quota::BoardsPerAccount, LIMIT);
    assert_eq!(
        fx.db.owner_boards(&fx.member).expect("boards").len(),
        LIMIT as usize
    );
}

#[tokio::test]
async fn quota_should_refuse_a_board_copy_over_the_board_limit() {
    const LIMIT: u32 = 1;
    let fx = Fixture::multi();
    let (source, _) = fx.board(&fx.member, "Source");
    fx.set_quotas(Quotas {
        boards_per_account: LIMIT,
        ..Quotas::default()
    })
    .await;
    let body = json!({"name": "Copy", "copy_board_id": source});
    let refused = fx.member("POST", "/api/boards", Some(body)).await;
    assert_quota(&refused, Quota::BoardsPerAccount, LIMIT);
    assert_eq!(
        fx.db.owner_boards(&fx.member).expect("boards").len(),
        LIMIT as usize
    );
}

#[tokio::test]
async fn copying_a_board_should_answer_404_for_another_accounts_board() {
    let fx = Fixture::multi();
    let (theirs, _) = fx.board(&fx.admin, "Theirs");
    let body = json!({"name": "Copy", "copy_board_id": theirs});
    let refused = fx.member("POST", "/api/boards", Some(body)).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.body);
    assert!(fx.db.owner_boards(&fx.member).expect("boards").is_empty());
}

#[tokio::test]
async fn copying_a_board_should_need_the_read_permissions_of_what_it_copies() {
    let fx = Fixture::multi();
    let (source, _) = fx.board(&fx.member, "Source");
    let no_tasks = fx.token_lacking(&[Permission::TasksRead]);
    let body = json!({"name": "Copy", "copy_board_id": source, "copy_tasks": true});
    let refused = fx.with_token(&no_tasks, "POST", "/api/boards", body).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("permission_denied"));

    let body = json!({"name": "Copy", "copy_board_id": source});
    let allowed = fx.with_token(&no_tasks, "POST", "/api/boards", body).await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);

    let no_boards = fx.token_lacking(&[Permission::BoardsRead]);
    let body = json!({"name": "Copy", "copy_board_id": source});
    let refused = fx.with_token(&no_boards, "POST", "/api/boards", body).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
}

#[tokio::test]
async fn quota_should_refuse_a_board_copy_whose_tasks_pass_the_task_limit() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    let (source, column) = fx.board(&fx.member, "Source");
    for n in 0..=LIMIT {
        fx.db
            .create_task(&column, None, &format!("Task {n}"), 1, None)
            .expect("task");
    }
    fx.set_quotas(Quotas {
        tasks_per_board: LIMIT,
        ..Quotas::default()
    })
    .await;
    let body = json!({"name": "Copy", "copy_board_id": source, "copy_tasks": true});
    let refused = fx.member("POST", "/api/boards", Some(body)).await;
    assert_quota(&refused, Quota::TasksPerBoard, LIMIT);
    assert_eq!(fx.db.owner_boards(&fx.member).expect("boards").len(), 1);

    let body = json!({"name": "Empty copy", "copy_board_id": source});
    let made = fx.member("POST", "/api/boards", Some(body)).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
}

#[tokio::test]
async fn quota_should_refuse_tasks_over_limit_including_a_move_into_a_full_board() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        tasks_per_board: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (full, full_column) = fx.board(&fx.member, "Full");
    let (_, other_column) = fx.board(&fx.member, "Other");
    for n in 0..LIMIT {
        let body = new_task(&full_column, &format!("Task {n}"));
        let made = fx.member("POST", "/api/tasks", Some(body)).await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    }
    let refused = fx
        .member("POST", "/api/tasks", Some(new_task(&full_column, "Over")))
        .await;
    assert_quota(&refused, Quota::TasksPerBoard, LIMIT);

    let outside = fx
        .db
        .create_task(&other_column, None, "Mover", 1, None)
        .expect("task");
    let moved = fx
        .member(
            "POST",
            &format!("/api/tasks/{outside}/move"),
            Some(json!({"column_id": full_column, "position": 0})),
        )
        .await;
    assert_quota(&moved, Quota::TasksPerBoard, LIMIT);
    let task = fx.db.task(&outside).expect("read").expect("task");
    assert_eq!(task.column_id, other_column);
    assert_eq!(
        fx.db.board_tasks(&full).expect("tasks").len(),
        LIMIT as usize
    );
}

#[tokio::test]
async fn restoring_a_task_onto_a_board_at_its_task_quota_should_answer_quota_exceeded_and_keep_it_in_the_bin()
 {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        tasks_per_board: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (board, column) = fx.board(&fx.member, "Full");
    let mut ids = Vec::new();
    for n in 0..LIMIT {
        let made = fx
            .member(
                "POST",
                "/api/tasks",
                Some(new_task(&column, &format!("Task {n}"))),
            )
            .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        ids.push(made.body["id"].as_str().expect("id").to_string());
    }
    let deleted = fx
        .member("DELETE", &format!("/api/tasks/{}", ids[0]), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    let refill = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "Refill")))
        .await;
    assert_eq!(refill.status, StatusCode::CREATED, "{}", refill.body);

    let refused = fx
        .member("POST", &format!("/api/bin/{}/restore", ids[0]), None)
        .await;
    assert_quota(&refused, Quota::TasksPerBoard, LIMIT);
    assert_eq!(fx.db.binned_tasks(&board).expect("bin").len(), 1);
    assert_eq!(
        fx.db.board_tasks(&board).expect("tasks").len(),
        LIMIT as usize
    );
}

#[tokio::test]
async fn restoring_a_task_onto_a_board_with_room_should_succeed() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        tasks_per_board: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (board, column) = fx.board(&fx.member, "Room");
    let made = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "Task")))
        .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    let id = made.body["id"].as_str().expect("id").to_string();
    let deleted = fx.member("DELETE", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);

    let restored = fx
        .member("POST", &format!("/api/bin/{id}/restore"), None)
        .await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);
    assert!(fx.db.binned_tasks(&board).expect("bin").is_empty());
}

#[tokio::test]
async fn quota_should_refuse_comments_over_limit() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        comments_per_task: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let uri = format!("/api/tasks/{task}/comments");
    for n in 0..LIMIT {
        let body = json!({"body": format!("Comment {n}")});
        let made = fx.member("POST", &uri, Some(body)).await;
        assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    }
    let refused = fx.member("POST", &uri, Some(json!({"body": "Over"}))).await;
    assert_quota(&refused, Quota::CommentsPerTask, LIMIT);
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(stored.comments.len(), LIMIT as usize);
}

#[tokio::test]
async fn comment_author_should_refuse_only_the_accounts_own_username() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let uri = format!("/api/tasks/{task}/comments");

    for label in ["Bob", " bob "] {
        let body = json!({"body": "x", "author": label});
        let refused = fx.member("POST", &uri, Some(body)).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    }
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert!(stored.comments.is_empty());

    let body = json!({"body": "x", "author": "admin"});
    let made = fx.member("POST", &uri, Some(body)).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(stored.comments.len(), 1);
}

#[tokio::test]
async fn quota_should_refuse_attachments_over_limit() {
    const LIMIT: u32 = 1;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        attachments_per_task: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let uri = format!("/api/tasks/{task}/attachments");
    let file = |name: &str, data: &str| json!({"name": name, "data": data});
    let made = fx
        .member("POST", &uri, Some(file("a.txt", "aGVsbG8=")))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let refused = fx
        .member("POST", &uri, Some(file("b.txt", "d29ybGQ=")))
        .await;
    assert_quota(&refused, Quota::AttachmentsPerTask, LIMIT);
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(stored.attachments.len(), LIMIT as usize);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_should_hold_under_racing_creates() {
    const LIMIT: u32 = 5;
    const RACERS: usize = 24;
    let fx = Arc::new(Fixture::multi());
    fx.set_quotas(Quotas {
        boards_per_account: LIMIT,
        ..Quotas::default()
    })
    .await;
    let racers: Vec<_> = (0..RACERS)
        .map(|n| {
            let fx = fx.clone();
            tokio::spawn(async move {
                let request = Request::builder()
                    .method("POST")
                    .uri("/api/boards")
                    .header("authorization", fx.member_bearer.as_str())
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": format!("B{n}")}).to_string()))
                    .expect("request");
                fx.app
                    .clone()
                    .oneshot(request)
                    .await
                    .expect("oneshot")
                    .status()
            })
        })
        .collect();
    let mut created = 0;
    for racer in racers {
        match racer.await.expect("racer") {
            StatusCode::OK => created += 1,
            StatusCode::CONFLICT => {}
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(created, LIMIT as usize);
    assert_eq!(
        fx.db.owner_boards(&fx.member).expect("boards").len(),
        LIMIT as usize
    );
}

#[tokio::test]
async fn admin_should_be_exempt_from_quotas() {
    const LIMIT: u32 = 1;
    const CREATED: u32 = LIMIT + 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        boards_per_account: LIMIT,
        ..Quotas::default()
    })
    .await;
    let auth = [("authorization", fx.admin_bearer.as_str())];
    for n in 0..CREATED {
        let body = json!({"name": format!("Admin {n}")});
        let made = call(&fx.app, "POST", "/api/boards", &auth, Some(body)).await;
        assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    }
    assert!(fx.db.owner_boards(&fx.admin).expect("boards").len() >= CREATED as usize);
}

#[tokio::test]
async fn single_mode_should_have_no_quotas() {
    const LIMIT: u32 = 1;
    let fx = Fixture::new(InstanceMode::Single, unthrottled_limits(), |_| roomy());
    let quotas = Quotas {
        boards_per_account: LIMIT,
        ..Quotas::default()
    };
    fx.db
        .update_instance_settings(|settings| settings.quotas = quotas)
        .expect("store quotas");
    for n in 0..=LIMIT {
        fx.db
            .create_board(&fx.member, &format!("Single {n}"))
            .expect("no quota in single mode");
    }
    fx.db
        .update_instance_settings(|settings| settings.mode = InstanceMode::Multi)
        .expect("store mode");
    let refused = fx
        .db
        .create_board(&fx.member, "Multi")
        .expect_err("the quota applies in multi mode");
    let named = format!(
        "at most {LIMIT} {}",
        Bound::Quota(Quota::BoardsPerAccount).describe()
    );
    assert!(refused.to_string().contains(&named), "{refused}");
}

#[tokio::test]
async fn limits_should_follow_saved_settings() {
    const LOWERED: u32 = 5;
    let fx = Fixture::new(InstanceMode::Multi, Clock::new().limits(), |_| roomy());
    let whoami = || fx.member("GET", "/api/v1/auth/whoami", None);
    for _ in 0..LOWERED * 2 {
        assert_eq!(whoami().await.status, StatusCode::OK);
    }
    fx.configure(
        Quotas::default(),
        RateLimits {
            token_burst: LOWERED,
            token_per_second: RateLimits::MIN,
            ..RateLimits::default()
        },
        DiskLimits::default(),
    )
    .await;
    for _ in 0..LOWERED {
        assert_eq!(whoami().await.status, StatusCode::OK);
    }
    let refused = whoami().await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.code.as_deref(), Some("rate_limited"));
}

#[tokio::test]
async fn account_bucket_should_be_shared_by_sessions_and_tokens() {
    const CREDENTIALS: u32 = 3;
    const ROUNDS: u32 = 2;
    let fx = Fixture::new(InstanceMode::Multi, Clock::new().limits(), |_| roomy());
    fx.configure(
        Quotas::default(),
        RateLimits {
            account_burst: CREDENTIALS * ROUNDS,
            account_per_second: RateLimits::MIN,
            ..RateLimits::default()
        },
        DiskLimits::default(),
    )
    .await;
    let (record, raw) = new_api_token_record(&fx.member, "second", AuthScope::Read, None);
    fx.db.create_api_token(&record).expect("token");
    let second = format!("Bearer {raw}");
    let uri = "/api/v1/auth/whoami";
    let session = [("cookie", fx.member_cookie.as_str())];
    let first = [("authorization", fx.member_bearer.as_str())];
    let other = [("authorization", second.as_str())];
    for _ in 0..ROUNDS {
        for headers in [&session[..], &first[..], &other[..]] {
            let sent = call(&fx.app, "GET", uri, headers, None).await;
            assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
        }
    }
    for headers in [&session[..], &first[..], &other[..]] {
        let refused = call(&fx.app, "GET", uri, headers, None).await;
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    }
    assert_eq!(fx.admin("GET", uri, None).await.status, StatusCode::OK);
}

/// Returns the status and the three `X-RateLimit-*` header values of a request to `uri`.
async fn rate_headers(
    app: &Router,
    uri: &str,
    header: (&str, &str),
) -> (StatusCode, [Option<String>; 3]) {
    let request = Request::builder()
        .uri(uri)
        .header(header.0, header.1)
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("oneshot");
    let read = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().expect("ascii").to_string())
    };
    let seen = [
        read("x-ratelimit-limit"),
        read("x-ratelimit-remaining"),
        read("x-ratelimit-reset"),
    ];
    (response.status(), seen)
}

/// Returns the header values `[limit, remaining, reset]` as a test expects them.
fn rates(limit: u32, remaining: u32, reset: u32) -> [Option<String>; 3] {
    [limit, remaining, reset].map(|value| Some(value.to_string()))
}

#[tokio::test]
async fn token_response_should_carry_the_rate_limit_headers_of_its_bucket() {
    const BURST: u32 = 5;
    let fx = Fixture::new(InstanceMode::Multi, Clock::new().limits(), |_| roomy());
    fx.configure(
        Quotas::default(),
        RateLimits {
            token_burst: BURST,
            token_per_second: RateLimits::MIN,
            ..RateLimits::default()
        },
        DiskLimits::default(),
    )
    .await;
    let uri = "/api/v1/auth/whoami";
    let auth = ("authorization", fx.member_bearer.as_str());
    for spent in 1..=BURST {
        let (status, seen) = rate_headers(&fx.app, uri, auth).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(seen, rates(BURST, BURST - spent, spent), "request {spent}");
    }
}

#[tokio::test]
async fn token_response_should_report_the_account_bucket_when_it_is_tighter() {
    const BURST: u32 = 3;
    let fx = Fixture::new(InstanceMode::Multi, Clock::new().limits(), |_| roomy());
    fx.configure(
        Quotas::default(),
        RateLimits {
            account_burst: BURST,
            account_per_second: RateLimits::MIN,
            ..RateLimits::default()
        },
        DiskLimits::default(),
    )
    .await;
    let uri = "/api/v1/auth/whoami";
    let auth = ("authorization", fx.member_bearer.as_str());
    let (status, seen) = rate_headers(&fx.app, uri, auth).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen, rates(BURST, BURST - 1, 1));
}

#[tokio::test]
async fn session_response_should_carry_the_account_rate_limit_headers() {
    const BURST: u32 = 4;
    let fx = Fixture::new(InstanceMode::Multi, Clock::new().limits(), |_| roomy());
    fx.configure(
        Quotas::default(),
        RateLimits {
            account_burst: BURST,
            account_per_second: RateLimits::MIN,
            ..RateLimits::default()
        },
        DiskLimits::default(),
    )
    .await;
    let uri = "/api/v1/auth/whoami";
    let cookie = ("cookie", fx.member_cookie.as_str());
    let (status, seen) = rate_headers(&fx.app, uri, cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen, rates(BURST, BURST - 1, 1));
}

#[tokio::test]
async fn permission_refusal_should_carry_the_rate_limit_headers_of_the_bucket_it_spent() {
    let fx = Fixture::multi();
    let no_boards = fx.token_lacking(&[Permission::BoardsRead]);
    let auth = ("authorization", no_boards.as_str());
    let (status, seen) = rate_headers(&fx.app, "/api/boards", auth).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(seen.iter().all(Option::is_some), "{seen:?}");
}

#[tokio::test]
async fn refused_request_should_carry_an_empty_bucket_in_the_rate_limit_headers() {
    const BURST: u32 = 2;
    let fx = Fixture::new(InstanceMode::Multi, Clock::new().limits(), |_| roomy());
    fx.configure(
        Quotas::default(),
        RateLimits {
            token_burst: BURST,
            token_per_second: RateLimits::MIN,
            ..RateLimits::default()
        },
        DiskLimits::default(),
    )
    .await;
    let uri = "/api/v1/auth/whoami";
    let auth = ("authorization", fx.member_bearer.as_str());
    for _ in 0..BURST {
        assert_eq!(rate_headers(&fx.app, uri, auth).await.0, StatusCode::OK);
    }
    let (status, seen) = rate_headers(&fx.app, uri, auth).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(seen, rates(BURST, 0, BURST));
}

#[tokio::test]
async fn disk_guard_should_refuse_writes_that_grow_the_database() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let (column, _) = fx.member_task();
    let refused = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "New")))
        .await;
    assert_storage_full(&refused);
    assert_eq!(
        refused.body["message"],
        format!(
            "storage is full: the database reached its limit of {} MiB",
            DiskLimits::DEFAULT_MAX_DATABASE_MIB
        )
    );
}

#[tokio::test]
async fn disk_guard_should_refuse_writes_to_parameterised_routes() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let (_, task) = fx.member_task();
    let comment = fx
        .db
        .add_comment(
            &task,
            NewComment {
                author: "bob",
                body: "kept",
                actor_kind: ActorKind::User,
                token_name: None,
                token_id: None,
                kind: None,
            },
            Actor::SYSTEM,
            None,
        )
        .expect("comment");
    let FencedWrite::Written(comment) = comment else {
        panic!("comment refused");
    };
    let attachment = json!({"name": "a.txt", "data": "aGVsbG8="});
    for (method, uri, body) in [
        ("POST", format!("/api/tasks/{task}/attachments"), attachment),
        (
            "POST",
            format!("/api/tasks/{task}/comments"),
            json!({"body": "new"}),
        ),
        (
            "PATCH",
            format!("/api/tasks/{task}"),
            json!({"name": "Renamed"}),
        ),
    ] {
        let refused = fx.member(method, &uri, Some(body)).await;
        assert_storage_full(&refused);
    }
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert!(stored.attachments.is_empty());
    assert_eq!(stored.name, "Task");

    let deleted = fx
        .member(
            "DELETE",
            &format!("/api/tasks/{task}/comments/{}", comment.id),
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
}

#[tokio::test]
async fn disk_guard_should_refuse_every_parameterised_write_that_grows_the_database() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let mut covered = 0;
    for (method, pattern, _, writes) in ROUTE_ACCESS {
        if *writes != Writes::Grows || !pattern.contains(':') {
            continue;
        }
        let uri = pattern
            .split('/')
            .map(|segment| {
                if segment.starts_with(':') {
                    "anything"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let session = [("cookie", fx.member_cookie.as_str())];
        let sent = call(&fx.app, method.as_str(), &uri, &session, Some(json!({}))).await;
        assert_eq!(
            sent.status,
            StatusCode::INSUFFICIENT_STORAGE,
            "{method} {pattern}: {}",
            sent.body
        );
        covered += 1;
    }
    assert!(covered > 0);
}

#[tokio::test]
async fn disk_guard_should_refuse_public_writes_that_grow_the_database() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let refused = call(
        &fx.app,
        "POST",
        "/api/v1/auth/register/options",
        &[],
        Some(json!({"handle": "carol"})),
    )
    .await;
    assert_storage_full(&refused);
}

#[tokio::test]
async fn disk_guard_should_answer_anonymous_writes_unauthenticated() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let (column, _) = fx.member_task();
    let anonymous = call(
        &fx.app,
        "POST",
        "/api/tasks",
        &[],
        Some(new_task(&column, "New")),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn disk_guard_should_allow_deletes() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let (_, task) = fx.member_task();
    let (board, _) = fx.board(&fx.member, "Spare");
    let deleted = fx
        .member("DELETE", &format!("/api/tasks/{task}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    let board_gone = fx
        .member("DELETE", &format!("/api/boards/{board}"), None)
        .await;
    assert_eq!(board_gone.status, StatusCode::OK, "{}", board_gone.body);
}

#[tokio::test]
async fn disk_guard_should_allow_sign_in_and_sign_out() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let sign_in = call(
        &fx.app,
        "POST",
        "/api/v1/auth/sign-in/options",
        &[],
        Some(json!({})),
    )
    .await;
    assert_eq!(sign_in.status, StatusCode::OK, "{}", sign_in.body);
    let session = [("cookie", fx.member_cookie.as_str())];
    let logout = call(&fx.app, "POST", "/logout", &session, None).await;
    assert_eq!(logout.status, StatusCode::SEE_OTHER);
    let after = call(&fx.app, "GET", "/api/v1/auth/whoami", &session, None).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn disk_guard_should_allow_account_delete_and_admin_settings() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    fx.set_disk(DiskLimits::default()).await;
    let gone = call(
        &fx.app,
        "POST",
        "/api/v1/account/delete",
        &[("cookie", fx.member_cookie.as_str())],
        Some(json!({"handle": "bob"})),
    )
    .await;
    assert_eq!(gone.status, StatusCode::OK, "{}", gone.body);
}

#[tokio::test]
async fn disk_guard_should_allow_stopping_the_timer() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let (_, task) = fx.member_task();
    let timer = ActiveTimer {
        task_id: Some(task),
        mode: TimerMode::Stopwatch,
        started_at: chrono::Utc::now().timestamp(),
        duration_secs: None,
    };
    fx.db
        .set_active_timer(&fx.member, &timer)
        .expect("start timer");
    let stopped = fx.member("POST", "/api/timer/stop", Some(json!({}))).await;
    assert_ne!(stopped.status, StatusCode::INSUFFICIENT_STORAGE);
    assert!(stopped.status.is_success(), "{}", stopped.body);
}

#[tokio::test]
async fn disk_guard_should_show_a_banner_to_signed_in_users() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let page = call(
        &fx.app,
        "GET",
        "/settings",
        &[("cookie", fx.member_cookie.as_str())],
        None,
    )
    .await;
    let html = page.body.as_str().expect("html");
    assert!(html.contains("storage-banner"), "{html}");
}

#[tokio::test]
async fn disk_guard_should_follow_a_raised_limit_without_a_restart() {
    let fx = Fixture::probed(probe(OVERSIZED_MIB, ROOMY_MIB));
    let (column, _) = fx.member_task();
    fx.set_disk(DiskLimits {
        max_database_mib: Some(DiskLimits::DEFAULT_MAX_DATABASE_MIB * 2),
        ..DiskLimits::default()
    })
    .await;
    let resumed = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "New")))
        .await;
    assert_eq!(resumed.status, StatusCode::CREATED, "{}", resumed.body);
}

#[tokio::test]
async fn disk_guard_should_resume_writes_once_the_bin_purge_frees_the_database() {
    const ATTACHMENT_MIB: usize = 9;
    let measured: Arc<Mutex<Option<DiskGuard>>> = Arc::new(Mutex::new(None));
    let kept = measured.clone();
    let fx = Fixture::new(InstanceMode::Multi, unthrottled_limits(), move |db| {
        let live = DiskProbe {
            file: Arc::new(|| Ok(MIB)),
            free: Arc::new(|_| Ok(ROOMY_MIB * MIB)),
            ..DiskProbe::of(db)
        };
        let made = guard(live);
        *kept.lock().expect("guard") = Some(made.clone());
        made
    });
    let disk = measured.lock().expect("guard").clone().expect("guard");
    fx.set_disk(DiskLimits {
        max_database_mib: Some(DiskLimits::MIN_DATABASE_MIB),
        ..DiskLimits::default()
    })
    .await;
    let (column, task) = fx.member_task();
    for (fill, name) in [(0u8, "a.bin"), (1u8, "b.bin")] {
        fx.db
            .add_attachment(
                &task,
                name,
                "application/octet-stream",
                &vec![fill; ATTACHMENT_MIB * MIB as usize],
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
    }
    disk.refresh();
    let refused = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "Full")))
        .await;
    assert_storage_full(&refused);

    let deleted = fx
        .member("DELETE", &format!("/api/tasks/{task}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    disk.refresh();
    let binned = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "Still full")))
        .await;
    assert_storage_full(&binned);

    let late = chrono::Utc::now().timestamp() + (BIN_RETENTION_DAYS + 1) * SECONDS_PER_DAY;
    fx.db.purge_bin(late).expect("purge");
    disk.refresh();
    let resumed = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "Room")))
        .await;
    assert_eq!(resumed.status, StatusCode::CREATED, "{}", resumed.body);
}

#[tokio::test]
async fn disk_guard_should_stop_a_backup_on_the_same_disk() {
    let fx = Fixture::probed(probe(1, CRAMPED_MIB));
    let ran = fx.admin("POST", "/api/v1/backups", None).await;
    assert_eq!(ran.status, StatusCode::OK, "{}", ran.body);
    assert_eq!(ran.body["ok"], false);
    let error = ran.body["error"].as_str().expect("error");
    let refusal = format!(
        "a backup would leave less than {} MiB of disk space free",
        DiskLimits::DEFAULT_MIN_FREE_MIB
    );
    assert!(error.contains(&refusal), "{error}");
    let stored = fx.db.backup_status().expect("read").expect("status");
    assert!(!stored.ok);
    assert_eq!(stored.error.as_deref(), Some(error));
    let listed = fx.admin("GET", "/api/v1/backups", None).await;
    assert_eq!(listed.body["files"], json!([]));
}

#[tokio::test]
async fn disk_guard_should_measure_the_backup_directory_on_its_own_disk() {
    let elsewhere = tempfile::tempdir().expect("backup dir");
    let roomy = elsewhere.path().to_path_buf();
    let fx = Fixture::new(InstanceMode::Multi, unthrottled_limits(), move |_| {
        cramped_except(&roomy)
    });
    let beside = fx.admin("POST", "/api/v1/backups", None).await;
    assert_eq!(beside.body["ok"], false, "{}", beside.body);

    let state = AppState::new(fx.db.clone(), None, passkey_mode())
        .with_mode(InstanceMode::Multi)
        .with_limits(unthrottled_limits())
        .with_disk(cramped_except(elsewhere.path()))
        .with_backup_dir(elsewhere.path().join("backups"));
    let cookie = [("cookie", fx.admin_cookie.as_str())];
    let written = call(&router(state), "POST", "/api/v1/backups", &cookie, None).await;
    assert_eq!(written.body["ok"], true, "{}", written.body);
}

/// Returns a guard whose disk is cramped everywhere except under `roomy`.
fn cramped_except(roomy: &Path) -> DiskGuard {
    let roomy = roomy.to_path_buf();
    guard(DiskProbe {
        stored: Arc::new(|| Ok(MIB)),
        file: Arc::new(|| Ok(MIB)),
        free: Arc::new(move |path| {
            let free = if path.starts_with(&roomy) {
                ROOMY_MIB
            } else {
                CRAMPED_MIB
            };
            Ok(free * MIB)
        }),
    })
}

#[tokio::test]
async fn single_mode_should_edit_only_the_disk_limits() {
    const FLOOR_MIB: u32 = 1024;
    let fx = Fixture::new(InstanceMode::Single, unthrottled_limits(), |_| roomy());
    let page = fx.admin("GET", "/settings", None).await;
    let html = page.body.as_str().expect("html");
    assert!(html.contains("id=\"storage\""), "{html}");
    let disk = DiskLimits {
        min_free_mib: FLOOR_MIB,
        ..DiskLimits::default()
    };
    let saved = fx.put_settings(json!({"disk": disk})).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        fx.db.instance_settings().expect("read").disk.min_free_mib,
        FLOOR_MIB
    );
    let refused = fx
        .put_settings(settings(
            Quotas::default(),
            RateLimits::default(),
            DiskLimits::default(),
        ))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
}

/// Returns `body` with the value at `path` replaced by `value`, or removed when `value` is
/// absent.
fn edited(body: &Value, path: &[&str], value: Option<Value>) -> Value {
    let mut body = body.clone();
    let (last, parents) = path.split_last().expect("path");
    let node = parents.iter().fold(&mut body, |node, key| &mut node[*key]);
    let object = node.as_object_mut().expect("object");
    match value {
        Some(value) => object.insert((*last).to_string(), value),
        None => object.remove(*last),
    };
    body
}

#[tokio::test]
async fn settings_validation_should_reject_out_of_range_and_unknown_fields() {
    let fx = Fixture::multi();
    let valid = settings(
        Quotas::default(),
        RateLimits::default(),
        DiskLimits::default(),
    );
    let set = |path: &[&str], value: Value| edited(&valid, path, Some(value));
    let drop = |path: &[&str]| edited(&valid, path, None);
    let refused = [
        set(
            &["quotas", Quota::BoardsPerAccount.as_str()],
            json!(Quotas::MIN - 1),
        ),
        set(
            &["quotas", Quota::BoardsPerAccount.as_str()],
            json!(Quotas::MAX_BOARDS_PER_ACCOUNT + 1),
        ),
        set(
            &["quotas", Quota::TasksPerBoard.as_str()],
            json!(Quotas::MAX_TASKS_PER_BOARD + 1),
        ),
        set(
            &["quotas", Quota::CommentsPerTask.as_str()],
            json!(Quotas::MIN - 1),
        ),
        set(
            &["quotas", Quota::AttachmentsPerTask.as_str()],
            json!(Quotas::MAX_ATTACHMENTS_PER_TASK + 1),
        ),
        set(&["limits", "token_burst"], json!(RateLimits::MIN - 1)),
        set(
            &["limits", "account_per_second"],
            json!(RateLimits::MAX_PER_SECOND + 1),
        ),
        set(&["limits", "ip_per_minute"], json!(RateLimits::MIN - 1)),
        set(
            &["disk", "max_database_mib"],
            json!(DiskLimits::MIN_DATABASE_MIB - 1),
        ),
        set(&["disk", "min_free_mib"], json!(DiskLimits::MAX_MIB + 1)),
        set(&["quotas", "extra"], json!(1)),
        set(&["surprise"], json!(true)),
        drop(&["disk"]),
        drop(&["disk", "max_database_mib"]),
        drop(&["limits", "account_burst"]),
    ];
    for body in refused {
        let sent = fx.put_settings(body.clone()).await;
        assert_eq!(sent.status, StatusCode::BAD_REQUEST, "{body}");
    }
    let stored = fx.db.instance_settings().expect("read");
    assert_eq!(stored.quotas, Quotas::default());
    assert_eq!(stored.limits, RateLimits::default());
    assert_eq!(stored.disk, DiskLimits::default());

    let saved = fx
        .put_settings(set(&["disk", "max_database_mib"], Value::Null))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["disk"]["max_database_mib"], Value::Null);
    assert_eq!(
        fx.db
            .instance_settings()
            .expect("read")
            .disk
            .max_database_mib,
        None
    );
}

#[test]
fn every_write_route_should_be_classified() {
    for (method, path, _, writes) in ROUTE_ACCESS {
        if *method == Method::GET {
            assert_eq!(*writes, Writes::No, "GET {path} never writes");
        } else if (method, *path) == (&Method::POST, "/api/markdown/preview") {
            assert_eq!(*writes, Writes::No, "{method} {path} only renders");
        } else {
            assert_ne!(
                *writes,
                Writes::No,
                "{method} {path} needs a writes classification"
            );
        }
    }
    let exempt = [
        (Method::POST, "/logout"),
        (Method::POST, "/api/v1/auth/sign-in"),
        (Method::POST, "/api/v1/auth/setup"),
        (Method::POST, "/api/v1/auth/recover"),
        (Method::POST, "/api/v1/auth/device/token"),
        (Method::POST, "/api/v1/account/delete"),
        (Method::POST, "/api/timer/stop"),
        (Method::POST, "/device/approve"),
        (Method::POST, "/device/deny"),
        (Method::PUT, "/api/v1/admin/settings"),
        (Method::DELETE, "/api/tasks/:id"),
    ];
    let writes_of = |method: &Method, path: &str| {
        ROUTE_ACCESS
            .iter()
            .find(|(m, p, _, _)| m == method && *p == path)
            .map(|entry| entry.3)
    };
    for (method, path) in exempt {
        assert_eq!(
            writes_of(&method, path),
            Some(Writes::Exempt),
            "{method} {path}"
        );
    }
    assert_eq!(writes_of(&Method::POST, "/api/tasks"), Some(Writes::Grows));
}

/// Sends `body` to `uri` as is, with `headers` and, when given, the connection's `peer` address.
async fn send_bytes(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    peer: Option<&str>,
    body: Vec<u8>,
) -> Reply {
    send_body(app, method, uri, headers, peer, Body::from(body)).await
}

/// Sends `body` to `uri` with `headers` and, when given, the connection's `peer` address.
async fn send_body(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    peer: Option<&str>,
    body: Body,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(body).expect("request");
    if let Some(peer) = peer {
        let addr = SocketAddr::new(peer.parse().expect("peer ip"), 40000);
        request.extensions_mut().insert(ConnectInfo(addr));
    }
    let response = app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let (location, cookie) = {
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|value| value.to_str().expect("ascii").to_string())
        };
        (header("location"), header("set-cookie"))
    };
    let tagged = response.extensions().get::<ErrorCode>().copied();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    let code = tagged
        .map(|code| serde_json::to_value(code).expect("code"))
        .or_else(|| body.get("code").cloned())
        .and_then(|code| code.as_str().map(str::to_string));
    Reply {
        status,
        location,
        cookie,
        body,
        code,
    }
}

const FORM_BOUNDARY: &str = "pomb-test-boundary";

/// Returns the multipart body of `files`, after a field that is not a file.
fn form_body(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut body = format!(
        "--{FORM_BOUNDARY}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhello\r\n"
    )
    .into_bytes();
    for (name, file) in files {
        body.extend_from_slice(
            format!(
                "--{FORM_BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(file);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{FORM_BOUNDARY}--\r\n").as_bytes());
    body
}

impl Fixture {
    /// Uploads `files` to `task` as one multipart body with the member's token.
    async fn upload_form(&self, task: &str, files: &[(&str, Vec<u8>)]) -> Reply {
        self.upload_form_bytes(task, form_body(files)).await
    }

    /// Uploads `body`, as [`form_body`] writes it, to `task` with the member's token.
    async fn upload_form_bytes(&self, task: &str, body: Vec<u8>) -> Reply {
        let content_type = format!("multipart/form-data; boundary={FORM_BOUNDARY}");
        let auth = [
            ("authorization", self.member_bearer.as_str()),
            ("content-type", content_type.as_str()),
        ];
        let uri = format!("/api/tasks/{task}/attachments");
        send_bytes(&self.app, "POST", &uri, &auth, None, body).await
    }

    /// Uploads `file` named `name` to `task` as a raw body with the member's token.
    async fn upload(&self, task: &str, name: &str, file: Vec<u8>) -> Reply {
        let auth = [
            ("authorization", self.member_bearer.as_str()),
            ("content-type", "application/octet-stream"),
        ];
        let uri = format!("/api/tasks/{task}/attachments?name={name}");
        send_bytes(&self.app, "POST", &uri, &auth, None, file).await
    }
}

/// Asserts that `reply` is the 400 that names `field` and its cap.
fn assert_too_long(reply: &Reply, field: &str, cap: &str) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(reply.code.as_deref(), Some("invalid_request"));
    let message = reply.body["message"].as_str().expect("message");
    assert!(
        message.contains(field) && message.contains(cap),
        "{message} names neither {field} nor {cap}"
    );
}

#[tokio::test]
async fn text_caps_should_refuse_each_field_past_its_cap_with_a_400() {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Board");
    let task = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let over = |chars: usize| "é".repeat(chars + 1);
    let field = |key: &str, extra: Value| {
        let mut body = json!({"key": key, "label": key, "kind": "text"});
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        body
    };
    let made = fx
        .member(
            "POST",
            &format!("/api/boards/{board}/fields"),
            Some(field("notes", json!({}))),
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let table = json!({
        "kind": "table",
        "columns": [{"key": "step", "label": "Step", "kind": "text"}],
    });
    let made = fx
        .member(
            "POST",
            &format!("/api/boards/{board}/fields"),
            Some(field("steps", table)),
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);

    let task_uri = format!("/api/tasks/{task}");
    let fields_uri = format!("/api/boards/{board}/fields");
    let labels: Vec<String> = (0..51).map(|n| format!("l{n}")).collect();
    let cases = [
        (
            "POST",
            "/api/tasks".to_string(),
            json!({"column_id": column, "name": over(500)}),
            "name",
            "500 characters",
        ),
        (
            "PATCH",
            task_uri.clone(),
            json!({"name": over(500)}),
            "name",
            "500 characters",
        ),
        (
            "PATCH",
            task_uri.clone(),
            json!({"description": "x".repeat(64 * 1024 + 1)}),
            "description",
            "65536 bytes",
        ),
        (
            "PATCH",
            task_uri.clone(),
            json!({"labels": labels}),
            "labels",
            "50 entries",
        ),
        (
            "PATCH",
            task_uri.clone(),
            json!({"labels": [over(50)]}),
            "labels",
            "50 characters",
        ),
        (
            "POST",
            format!("{task_uri}/subtasks"),
            json!({"name": over(500)}),
            "subtask name",
            "500 characters",
        ),
        (
            "POST",
            format!("{task_uri}/comments"),
            json!({"body": "x".repeat(5001)}),
            "comment",
            "max 5000 bytes",
        ),
        (
            "POST",
            "/api/columns".to_string(),
            json!({"board_id": board, "name": over(200)}),
            "column name",
            "200 characters",
        ),
        (
            "POST",
            "/api/swimlanes".to_string(),
            json!({"board_id": board, "name": over(200)}),
            "swimlane name",
            "200 characters",
        ),
        (
            "POST",
            fields_uri.clone(),
            json!({"key": "long", "label": over(200), "kind": "text"}),
            "label",
            "200 characters",
        ),
        (
            "POST",
            fields_uri.clone(),
            json!({"key": "pick", "label": "Pick", "kind": "select", "options": [over(200)]}),
            "options",
            "200 characters",
        ),
        (
            "POST",
            format!("/api/boards/{board}/save-as-template"),
            json!({"name": over(200)}),
            "name",
            "200 characters",
        ),
        (
            "PATCH",
            task_uri.clone(),
            json!({"fields": {"notes": "x".repeat(4097)}}),
            "value",
            "4096 bytes",
        ),
        (
            "PATCH",
            task_uri.clone(),
            json!({"fields": {"steps": [{"step": "x".repeat(4097)}]}}),
            "value",
            "4096 bytes",
        ),
    ];
    for (method, uri, body, field, cap) in cases {
        let refused = fx.member(method, &uri, Some(body)).await;
        assert_too_long(&refused, field, cap);
    }
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(stored.name, "Task");
    assert!(stored.description.is_empty());
    assert!(stored.labels.is_empty() && stored.subtasks.is_empty());

    let at_cap = fx
        .member("PATCH", &task_uri, Some(json!({"name": "é".repeat(500)})))
        .await;
    assert_eq!(at_cap.status, StatusCode::OK, "{}", at_cap.body);
}

#[tokio::test]
async fn list_caps_should_refuse_each_list_past_its_cap_with_a_400() {
    let fx = Fixture::multi();
    let (board, task) = {
        let (board, column) = fx.board(&fx.member, "Board");
        let task = fx
            .db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        (board, task)
    };
    let fields_uri = format!("/api/boards/{board}/fields");
    let options: Vec<String> = (0..201).map(|n| format!("o{n}")).collect();
    let columns: Vec<Value> = (0..51)
        .map(|n| json!({"key": format!("c{n}"), "label": format!("C{n}"), "kind": "text"}))
        .collect();
    let reasons: Vec<String> = (0..51).map(|n| format!("r{n}")).collect();
    let folded: Vec<String> = (0..65).map(|n| format!("fields:g{n}")).collect();
    let activities: Vec<Value> = (0..51)
        .map(|n| json!({"id": "", "name": format!("a{n}")}))
        .collect();
    let cases = [
        (
            "POST",
            fields_uri.clone(),
            json!({"key": "pick", "label": "Pick", "kind": "select", "options": options}),
            "options",
            "200 entries",
        ),
        (
            "POST",
            fields_uri.clone(),
            json!({"key": "grid", "label": "Grid", "kind": "table", "columns": columns}),
            "columns",
            "50 entries",
        ),
        (
            "POST",
            format!("/api/tasks/{task}/attachments"),
            json!({"name": "a.txt", "mime": "m".repeat(201), "data": "aGVsbG8="}),
            "mime",
            "200 characters",
        ),
        (
            "POST",
            format!("/api/tasks/{task}/attachments"),
            json!({"name": "n".repeat(256), "data": "aGVsbG8="}),
            "name",
            "255 bytes",
        ),
        (
            "PUT",
            "/api/settings".to_string(),
            json!({"interrupt_reasons": reasons}),
            "interrupt_reasons",
            "50 entries",
        ),
        (
            "PUT",
            "/api/settings".to_string(),
            json!({"interrupt_reasons": [past(TextCap::INTERRUPT_REASON)]}),
            "interrupt_reasons",
            "80 characters",
        ),
        (
            "PUT",
            "/api/settings".to_string(),
            json!({"break_activities": activities}),
            "break_activities",
            "50 entries",
        ),
        (
            "PUT",
            "/api/settings".to_string(),
            json!({"break_activities": [{"id": "", "name": past(TextCap::BREAK_ACTIVITY_NAME)}]}),
            "break_activities.name",
            "80 characters",
        ),
        (
            "PUT",
            "/api/settings".to_string(),
            json!({"break_activities": [{"id": "", "name": "n", "description": past(TextCap::BREAK_ACTIVITY_DESCRIPTION)}]}),
            "break_activities.description",
            "200 characters",
        ),
        (
            "PUT",
            "/api/settings".to_string(),
            json!({"modal_folded_groups": folded}),
            "modal_folded_groups",
            "64 entries",
        ),
        (
            "PUT",
            "/api/settings".to_string(),
            json!({"modal_folded_groups": [past(TextCap::FOLDED_GROUP)]}),
            "modal_folded_groups",
            "200 characters",
        ),
    ];
    for (method, uri, body, field, cap) in cases {
        let refused = fx.member(method, &uri, Some(body)).await;
        assert_too_long(&refused, field, cap);
    }
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert!(stored.member_ids.is_empty() && stored.attachments.is_empty());
    assert!(fx.db.list_fields(&board).expect("fields").is_empty());
}

#[tokio::test]
async fn history_should_store_at_most_200_characters_of_any_text() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let long = "n".repeat(500);
    let uri = format!("/api/tasks/{task}");
    let renamed = fx.member("PATCH", &uri, Some(json!({"name": long}))).await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    let claimant = "c".repeat(chars_of(TextCap::CLAIMANT));
    let claim = json!({"claimant": claimant, "ttl_seconds": 60});
    let claimed = fx
        .member("POST", &format!("{uri}/claim"), Some(claim))
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.body);
    let release = json!({"fence": claimed.body["fence"]});
    let released = fx
        .member("POST", &format!("{uri}/claim/release"), Some(release))
        .await;
    assert_eq!(released.status, StatusCode::OK, "{}", released.body);

    let stored = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(stored.name, long);
    for kind in [
        HistoryKind::Renamed,
        HistoryKind::Claimed,
        HistoryKind::ClaimReleased,
    ] {
        let event = stored
            .history
            .iter()
            .find(|event| event.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} event"));
        assert_eq!(
            event.detail.chars().count(),
            MAX_HISTORY_TEXT_CHARS,
            "{kind:?}"
        );
        assert!(event.detail.ends_with('…'), "{kind:?}: {}", event.detail);
    }

    let column = stored.column_id.clone();
    let target = fx
        .db
        .create_task(&column, None, &long, 1, None)
        .expect("task");
    let link = json!({"kind": "related", "target_id": target});
    let linked = fx.member("POST", &format!("{uri}/links"), Some(link)).await;
    assert!(linked.status.is_success(), "{}", linked.body);
    let stored = fx.db.task(&task).expect("read").expect("task");
    let event = stored
        .history
        .iter()
        .find(|event| event.kind == HistoryKind::LinkAdded)
        .expect("link_added event");
    assert!(event.detail.chars().count() <= MAX_HISTORY_TEXT_CHARS);
    assert!(
        event.detail.ends_with(&format!("…\" ({target})")),
        "{}",
        event.detail
    );
}

#[tokio::test]
async fn sidebar_folds_should_persist_clean_and_stay_with_their_account() {
    let fx = Fixture::multi();
    let own = [("authorization", fx.member_bearer.as_str())];
    let other = [("authorization", fx.admin_bearer.as_str())];

    let fresh = call(&fx.app, "GET", "/api/settings", &own, None).await;
    assert_eq!(fresh.body["modal_folded_groups"], json!([]));

    let folded = json!({
        "modal_folded_groups": [" details ", "panel", "", "details", "fields:", "   "],
    });
    let saved = call(&fx.app, "PUT", "/api/settings", &own, Some(folded)).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body["modal_folded_groups"],
        json!(["details", "panel", "fields:"])
    );

    let reloaded = call(&fx.app, "GET", "/api/settings", &own, None).await;
    assert_eq!(
        reloaded.body["modal_folded_groups"],
        json!(["details", "panel", "fields:"])
    );

    let theirs = call(&fx.app, "GET", "/api/settings", &other, None).await;
    assert_eq!(theirs.body["modal_folded_groups"], json!([]));

    let unrelated = json!({"pomodoro_minutes": 30});
    let saved = call(&fx.app, "PUT", "/api/settings", &own, Some(unrelated)).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body["modal_folded_groups"],
        json!(["details", "panel", "fields:"])
    );

    let unfolded = json!({"modal_folded_groups": []});
    let saved = call(&fx.app, "PUT", "/api/settings", &own, Some(unfolded)).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let stored = fx.db.settings(&fx.member).expect("settings");
    assert!(stored.modal_folded_groups.is_empty());
}

impl Fixture {
    /// Adds a subtask named `name` to `task` as the member and returns its id.
    async fn add_subtask(&self, task: &str, name: &str) -> String {
        let reply = self
            .member(
                "POST",
                &format!("/api/tasks/{task}/subtasks"),
                Some(json!({"name": name})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        reply.body["id"].as_str().expect("id").to_string()
    }

    /// Saves `task`'s subtasks as the member's template `name` and returns the reply.
    async fn save_template_named(&self, task: &str, name: &str) -> Reply {
        let uri = format!("/api/tasks/{task}/subtasks/template");
        self.member("POST", &uri, Some(json!({"name": name}))).await
    }

    /// Returns a member task holding one subtask, saved as `count` templates named `T0`, `T1`...
    async fn task_with_templates(&self, count: usize) -> String {
        let (_, task) = self.member_task();
        self.add_subtask(&task, "Step").await;
        for n in 0..count {
            let saved = self.save_template_named(&task, &format!("T{n}")).await;
            assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
        }
        task
    }

    /// Deletes every subtask of `task` as the member.
    async fn delete_all_subtasks(&self, task: &str) -> Reply {
        let uri = format!("/api/tasks/{task}/subtasks");
        self.member("DELETE", &uri, None).await
    }

    /// Replaces the member's subtask templates with `templates` and returns the saved list.
    async fn put_templates(&self, templates: Value) -> Vec<Value> {
        let body = json!({"subtask_templates": templates});
        let saved = self.member("PUT", "/api/settings", Some(body)).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
        saved.body["subtask_templates"]
            .as_array()
            .expect("list")
            .clone()
    }

    /// Returns the member's subtask templates as stored.
    fn member_templates(&self) -> Vec<SubtaskTemplate> {
        self.db
            .settings(&self.member)
            .expect("settings")
            .subtask_templates
    }
}

/// Returns the history details of `kind` on `task`.
fn history_details(fx: &Fixture, task: &str, kind: HistoryKind) -> Vec<String> {
    let row = fx.db.task(task).expect("read").expect("task");
    row.history
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.detail)
        .collect()
}

#[tokio::test]
async fn save_subtask_template_should_store_current_subtask_names_under_the_name() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    fx.add_subtask(&task, "Build").await;
    fx.add_subtask(&task, "Ship").await;
    let uri = format!("/api/tasks/{task}/subtasks/template");

    let saved = fx
        .member("POST", &uri, Some(json!({"name": " Release "})))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["name"], "Release");
    assert_eq!(saved.body["subtasks"], json!(["Build", "Ship"]));
    assert!(!saved.body["id"].as_str().expect("id").is_empty());

    let own = fx.member("GET", "/api/settings", None).await;
    assert_eq!(own.body["subtask_templates"], json!([saved.body]));
}

#[tokio::test]
async fn save_subtask_template_should_replace_a_template_of_the_same_name() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    fx.add_subtask(&task, "Build").await;
    let uri = format!("/api/tasks/{task}/subtasks/template");
    let first = fx
        .member("POST", &uri, Some(json!({"name": "Release"})))
        .await;

    fx.add_subtask(&task, "Announce").await;
    let replaced = fx
        .member("POST", &uri, Some(json!({"name": "Release"})))
        .await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.body);
    assert_eq!(replaced.body["id"], first.body["id"]);
    let templates = fx.member_templates();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].subtasks, ["Build", "Announce"]);
}

#[tokio::test]
async fn save_subtask_template_should_refuse_a_task_without_subtasks() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let refused = fx.save_template_named(&task, "Release").await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert!(fx.member_templates().is_empty());
}

#[tokio::test]
async fn save_subtask_template_should_refuse_a_blank_name() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    fx.add_subtask(&task, "Build").await;
    for name in ["  ", ""] {
        let refused = fx.save_template_named(&task, name).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    }
    assert!(fx.member_templates().is_empty());
}

#[tokio::test]
async fn save_subtask_template_should_answer_404_for_an_unknown_task() {
    let fx = Fixture::multi();
    let missing = fx.save_template_named("nope", "Release").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
}

#[tokio::test]
async fn save_subtask_template_should_keep_the_template_with_its_account() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    fx.add_subtask(&task, "Build").await;
    let uri = format!("/api/tasks/{task}/subtasks/template");
    fx.member("POST", &uri, Some(json!({"name": "Release"})))
        .await;

    let other = fx.admin("GET", "/api/settings", None).await;
    assert_eq!(other.body["subtask_templates"], json!([]));
}

#[tokio::test]
async fn save_subtask_template_should_refuse_a_template_past_the_cap() {
    let max = items_of(TextCap::SUBTASK_TEMPLATES);
    let fx = Fixture::multi();
    let task = fx.task_with_templates(max).await;
    let refused = fx.save_template_named(&task, "Over").await;
    assert_too_long(&refused, "subtask_templates", &format!("{max} entries"));
    assert_eq!(fx.member_templates().len(), max);
}

#[tokio::test]
async fn save_subtask_template_should_replace_a_template_at_the_cap() {
    let max = items_of(TextCap::SUBTASK_TEMPLATES);
    let fx = Fixture::multi();
    let task = fx.task_with_templates(max).await;
    let replaced = fx.save_template_named(&task, "T3").await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.body);
    assert_eq!(fx.member_templates().len(), max);
}

#[tokio::test]
async fn delete_all_subtasks_should_write_nothing_for_a_task_without_subtasks() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let before = history_details(&fx, &task, HistoryKind::SubtaskDeleted);
    let removed = fx.delete_all_subtasks(&task).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    assert_eq!(
        history_details(&fx, &task, HistoryKind::SubtaskDeleted),
        before
    );
    assert!(before.is_empty());
}

#[tokio::test]
async fn delete_all_subtasks_should_remove_every_subtask() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    for name in ["One", "Two", "Three"] {
        fx.add_subtask(&task, name).await;
    }
    let removed = fx.delete_all_subtasks(&task).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert!(stored.subtasks.is_empty());
}

#[tokio::test]
async fn delete_all_subtasks_should_log_one_event() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    for name in ["One", "Two", "Three"] {
        fx.add_subtask(&task, name).await;
    }
    fx.delete_all_subtasks(&task).await;
    assert_eq!(
        history_details(&fx, &task, HistoryKind::SubtaskDeleted),
        ["Deleted all 3 subtasks"]
    );
}

#[tokio::test]
async fn delete_all_subtasks_should_answer_404_for_an_unknown_task() {
    let fx = Fixture::multi();
    let missing = fx.delete_all_subtasks("nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
}

/// Saves the member's `source` subtasks as template `name` and returns its id.
async fn save_template(fx: &Fixture, source: &str, name: &str) -> String {
    let saved = fx
        .member(
            "POST",
            &format!("/api/tasks/{source}/subtasks/template"),
            Some(json!({"name": name})),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    saved.body["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn add_subtasks_from_template_should_append_names_to_another_task() {
    let fx = Fixture::multi();
    let (_, source) = fx.member_task();
    let (_, target) = fx.member_task();
    fx.add_subtask(&source, "Build").await;
    fx.add_subtask(&source, "Ship").await;
    let template_id = save_template(&fx, &source, "Release").await;
    fx.add_subtask(&target, "Existing").await;

    let uri = format!("/api/tasks/{target}/subtasks/from-template");
    let added = fx
        .member("POST", &uri, Some(json!({"template_id": template_id})))
        .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let created = added.body.as_array().expect("subtasks");
    assert_eq!(created.len(), 2);
    assert_eq!(created[0]["name"], "Build");
    assert_eq!(created[1]["name"], "Ship");
    assert!(created.iter().all(|sub| sub["done"] == false));

    let stored = fx.db.task(&target).expect("read").expect("task");
    let names: Vec<&str> = stored.subtasks.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["Existing", "Build", "Ship"]);
    assert_eq!(
        stored.subtasks[1].id,
        created[0]["id"].as_str().expect("id")
    );
}

#[tokio::test]
async fn add_subtasks_from_template_should_log_one_added_event_per_name() {
    let fx = Fixture::multi();
    let (_, source) = fx.member_task();
    let (_, target) = fx.member_task();
    fx.add_subtask(&source, "Build").await;
    fx.add_subtask(&source, "Ship").await;
    let template_id = save_template(&fx, &source, "Release").await;

    let uri = format!("/api/tasks/{target}/subtasks/from-template");
    let added = fx
        .member("POST", &uri, Some(json!({"template_id": template_id})))
        .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    assert_eq!(
        history_details(&fx, &target, HistoryKind::SubtaskAdded),
        ["Added subtask \"Build\"", "Added subtask \"Ship\""]
    );
}

#[tokio::test]
async fn add_subtasks_from_template_should_answer_404_for_an_unknown_or_foreign_template() {
    let fx = Fixture::multi();
    let (_, source) = fx.member_task();
    let (_, target) = fx.member_task();
    fx.add_subtask(&source, "Build").await;
    let template_id = save_template(&fx, &source, "Release").await;

    let uri = format!("/api/tasks/{target}/subtasks/from-template");
    let unknown = fx
        .member("POST", &uri, Some(json!({"template_id": "nope"})))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    assert!(
        fx.db
            .task(&target)
            .expect("read")
            .expect("task")
            .subtasks
            .is_empty()
    );

    let (_, theirs) = fx.board(&fx.admin, "Theirs");
    let their_task = fx
        .db
        .create_task(&theirs, None, "Task", 1, None)
        .expect("task");
    let their_uri = format!("/api/tasks/{their_task}/subtasks/from-template");
    let foreign = fx
        .admin(
            "POST",
            &their_uri,
            Some(json!({"template_id": template_id})),
        )
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
    assert!(
        fx.db
            .task(&their_task)
            .expect("read")
            .expect("task")
            .subtasks
            .is_empty()
    );
}

#[tokio::test]
async fn add_subtasks_from_template_should_refuse_names_over_the_subtask_quota() {
    const LIMIT: u32 = 3;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        subtasks_per_task: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (_, source) = fx.member_task();
    let (_, target) = fx.member_task();
    for name in ["A", "B"] {
        fx.add_subtask(&source, name).await;
        fx.add_subtask(&target, name).await;
    }
    let template_id = save_template(&fx, &source, "Pair").await;
    let refused = fx
        .member(
            "POST",
            &format!("/api/tasks/{target}/subtasks/from-template"),
            Some(json!({"template_id": template_id})),
        )
        .await;
    assert_quota(&refused, Quota::SubtasksPerTask, LIMIT);
    let stored = fx.db.task(&target).expect("read").expect("task");
    assert_eq!(stored.subtasks.len(), 2, "a refused template adds none");
}

#[tokio::test]
async fn put_settings_should_clean_subtask_templates() {
    let fx = Fixture::multi();
    let saved = fx
        .put_templates(json!([
            {"id": "", "name": " Release ", "subtasks": [" Build ", "", "Ship"]},
            {"id": "", "name": "Review", "subtasks": ["Read"]},
            {"id": "", "name": "Release", "subtasks": ["Other"]},
            {"id": "", "name": "   ", "subtasks": ["x"]},
            {"id": "", "name": "Empty", "subtasks": ["  "]},
        ]))
        .await;
    assert_eq!(saved.len(), 2, "{saved:?}");
    assert_eq!(saved[0]["name"], "Release");
    assert_eq!(saved[0]["subtasks"], json!(["Build", "Ship"]));
    assert_eq!(saved[1]["name"], "Review");
}

#[tokio::test]
async fn put_settings_should_keep_subtask_templates_on_an_unrelated_patch() {
    let fx = Fixture::multi();
    let saved = fx
        .put_templates(json!([{"id": "", "name": "One", "subtasks": ["a"]}]))
        .await;
    let unrelated = fx
        .member(
            "PUT",
            "/api/settings",
            Some(json!({"pomodoro_minutes": 30})),
        )
        .await;
    assert_eq!(unrelated.body["subtask_templates"], json!(saved));
}

#[tokio::test]
async fn put_settings_should_clear_subtask_templates_on_an_empty_list() {
    let fx = Fixture::multi();
    fx.put_templates(json!([{"id": "", "name": "One", "subtasks": ["a"]}]))
        .await;
    let cleared = fx.put_templates(json!([])).await;
    assert!(cleared.is_empty());
    assert!(fx.member_templates().is_empty());
}

#[tokio::test]
async fn put_settings_should_generate_ids_for_new_subtask_templates() {
    let fx = Fixture::multi();
    let saved = fx
        .put_templates(json!([
            {"id": "chosen", "name": "One", "subtasks": ["a"]},
            {"id": "chosen", "name": "Two", "subtasks": ["b"]},
        ]))
        .await;
    assert_ne!(saved[0]["id"], "chosen");
    assert_ne!(saved[1]["id"], "chosen");
    assert_ne!(saved[0]["id"], saved[1]["id"]);
}

#[tokio::test]
async fn put_settings_should_keep_a_stored_subtask_template_id() {
    let fx = Fixture::multi();
    let stored = fx
        .put_templates(json!([{"id": "", "name": "One", "subtasks": ["a"]}]))
        .await;
    let kept = stored[0]["id"].clone();
    let resaved = fx
        .put_templates(json!([
            {"id": kept, "name": "One", "subtasks": ["a"]},
            {"id": kept, "name": "Two", "subtasks": ["b"]},
        ]))
        .await;
    assert_eq!(resaved[0]["id"], kept, "a stored id is kept once");
    assert_ne!(resaved[1]["id"], kept);
}

#[tokio::test]
async fn subtask_template_caps_should_refuse_each_list_past_its_cap() {
    let max_templates = items_of(TextCap::SUBTASK_TEMPLATES);
    let max_names = items_of(TextCap::SUBTASK_TEMPLATE_NAMES);
    let fx = Fixture::multi();
    let many: Vec<Value> = (0..=max_templates)
        .map(|n| json!({"id": "", "name": format!("T{n}"), "subtasks": ["x"]}))
        .collect();
    let names: Vec<String> = (0..=max_names).map(|n| format!("s{n}")).collect();
    let cases = [
        (
            json!({"subtask_templates": many}),
            "subtask_templates",
            format!("{max_templates} entries"),
        ),
        (
            json!({"subtask_templates": [{"id": "", "name": "T", "subtasks": names}]}),
            "subtask_templates.subtasks",
            format!("{max_names} entries"),
        ),
    ];
    for (body, field, cap) in cases {
        let refused = fx.member("PUT", "/api/settings", Some(body)).await;
        assert_too_long(&refused, field, &cap);
    }
    assert!(fx.member_templates().is_empty());
}

#[tokio::test]
async fn subtask_template_caps_should_accept_names_at_the_cap() {
    let max_names = items_of(TextCap::SUBTASK_TEMPLATE_NAMES);
    let fx = Fixture::multi();
    let names: Vec<String> = (0..max_names).map(|n| format!("s{n}")).collect();
    let saved = fx
        .put_templates(json!([{"id": "", "name": "T", "subtasks": names}]))
        .await;
    assert_eq!(saved.len(), 1);
}

#[tokio::test]
async fn subtask_template_caps_should_refuse_each_text_past_its_cap() {
    let fx = Fixture::multi();
    let cases = [
        (
            json!({"subtask_templates": [{"id": "", "name": past(TextCap::TEMPLATE_NAME), "subtasks": ["x"]}]}),
            "name",
            format!("{} characters", chars_of(TextCap::TEMPLATE_NAME)),
        ),
        (
            json!({"subtask_templates": [{"id": "", "name": "T", "subtasks": [past(TextCap::SUBTASK_NAME)]}]}),
            "subtask name",
            format!("{} characters", chars_of(TextCap::SUBTASK_NAME)),
        ),
    ];
    for (body, field, cap) in cases {
        let refused = fx.member("PUT", "/api/settings", Some(body)).await;
        assert_too_long(&refused, field, &cap);
    }
    assert!(fx.member_templates().is_empty());
}

/// Returns the most entries `cap` allows.
fn items_of(cap: TextCap) -> usize {
    let Cap::Items(max) = cap.cap else {
        panic!("{} is not an entry cap", cap.field);
    };
    max
}

/// Returns the most characters `cap` allows.
fn chars_of(cap: TextCap) -> usize {
    let Cap::Chars(max) = cap.cap else {
        panic!("{} is not a character cap", cap.field);
    };
    max
}

/// Returns a text one character past `cap`.
fn past(cap: TextCap) -> String {
    "x".repeat(chars_of(cap) + 1)
}

#[tokio::test]
async fn claims_should_refuse_texts_past_their_cap() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let uri = format!("/api/tasks/{task}/claim");
    let blank = TaskClaim {
        claimant: String::new(),
        role: None,
        harness: None,
        model: None,
        jj_workspace: None,
        jj_change_id: None,
        session_id: None,
        fence: 0,
        started_at: String::new(),
        heartbeat_at: String::new(),
        expires_at: String::new(),
    };
    for (cap, _) in blank.texts() {
        let mut claim = json!({"claimant": "agent", "ttl_seconds": 60});
        claim[cap.field] = json!(past(cap));
        let refused = fx.member("POST", &uri, Some(claim)).await;
        assert_too_long(&refused, cap.field, &cap.cap.to_string());
    }
    let over = past(TextCap::CLAIM_SESSION_ID);
    let claimed = fx
        .member(
            "POST",
            &uri,
            Some(json!({"claimant": "agent", "ttl_seconds": 60})),
        )
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.body);
    let heartbeat = json!({"fence": claimed.body["fence"], "session_id": over});
    let refused = fx
        .member("POST", &format!("{uri}/heartbeat"), Some(heartbeat))
        .await;
    assert_too_long(&refused, "session_id", "200 characters");
    assert!(
        fx.db
            .task(&task)
            .expect("read")
            .expect("task")
            .claim
            .is_some()
    );
}

#[tokio::test]
async fn quota_should_refuse_subtasks_over_limit() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        subtasks_per_task: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let uri = format!("/api/tasks/{task}/subtasks");
    for n in 0..LIMIT {
        let made = fx
            .member("POST", &uri, Some(json!({"name": format!("Step {n}")})))
            .await;
        assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    }
    let refused = fx.member("POST", &uri, Some(json!({"name": "Over"}))).await;
    assert_quota(&refused, Quota::SubtasksPerTask, LIMIT);
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(stored.subtasks.len(), LIMIT as usize);
}

#[tokio::test]
async fn quota_should_refuse_templates_over_limit() {
    const LIMIT: u32 = 1;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        templates_per_account: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (board, _) = fx.board(&fx.member, "Board");
    let uri = format!("/api/boards/{board}/save-as-template");
    let saved = fx.member("POST", &uri, Some(json!({"name": "One"}))).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let refused = fx.member("POST", &uri, Some(json!({"name": "Two"}))).await;
    assert_quota(&refused, Quota::TemplatesPerAccount, LIMIT);
}

#[tokio::test]
async fn quota_should_refuse_bytes_past_the_stored_board_limit_and_free_them_on_delete() {
    const LIMIT_MIB: u32 = 1;
    const FILE_BYTES: usize = 600 * 1024;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_board: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let refused = fx.upload(&task, "b.bin", vec![2; FILE_BYTES]).await;
    assert_quota(&refused, Quota::StoredMibPerBoard, LIMIT_MIB);

    let attachment = stored.body["id"].as_str().expect("id");
    let deleted = fx
        .member(
            "DELETE",
            &format!("/api/tasks/{task}/attachments/{attachment}"),
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    let fits = fx.upload(&task, "b.bin", vec![2; FILE_BYTES]).await;
    assert_eq!(fits.status, StatusCode::OK, "{}", fits.body);
}

#[tokio::test]
async fn upload_should_attach_every_file_of_a_multipart_body_with_its_own_result() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let sent = fx
        .upload_form(
            &task,
            &[
                ("a.txt", b"one".to_vec()),
                ("b.txt", b"two".to_vec()),
                ("c.txt", b"three".to_vec()),
            ],
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let results = sent.body["results"].as_array().expect("results");
    let names: Vec<_> = results
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["a.txt", "b.txt", "c.txt"]);
    for result in results {
        assert!(result["attachment"]["id"].is_string(), "{result}");
        assert!(result.get("error").is_none(), "{result}");
    }
    assert_eq!(results[2]["attachment"]["size"], 5);
    assert_eq!(results[0]["attachment"]["mime"], "text/plain");
    let held = fx
        .db
        .task(&task)
        .expect("read")
        .expect("task")
        .attachments
        .len();
    assert_eq!(held, 3);
}

#[tokio::test]
async fn upload_should_ignore_a_multipart_file_input_sent_without_a_name() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let sent = fx
        .upload_form(&task, &[("", Vec::new()), ("a.txt", b"one".to_vec())])
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let results = sent.body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1, "{}", sent.body);
    assert_eq!(results[0]["name"], "a.txt");
}

#[tokio::test]
async fn upload_should_report_a_refused_multipart_file_and_store_the_others() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    fx.set_disk(DiskLimits {
        max_attachment_mib: LIMIT_MIB,
        ..DiskLimits::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let sent = fx
        .upload_form(
            &task,
            &[
                ("first.txt", b"one".to_vec()),
                ("big.bin", vec![1; (LIMIT_MIB as usize) * 1024 * 1024 + 1]),
                ("last.txt", b"three".to_vec()),
            ],
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let results = sent.body["results"].as_array().expect("results");
    assert_eq!(results.len(), 3);
    assert!(results[0]["attachment"]["id"].is_string(), "{}", results[0]);
    assert_eq!(results[1]["name"], "big.bin");
    assert_eq!(results[1]["error"]["code"], "invalid_request");
    assert!(
        results[1]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("1 MiB limit")),
        "{}",
        results[1]
    );
    assert!(results[2]["attachment"]["id"].is_string(), "{}", results[2]);
    let held = fx
        .db
        .task(&task)
        .expect("read")
        .expect("task")
        .attachments
        .len();
    assert_eq!(held, 2);
}

#[tokio::test]
async fn upload_should_report_a_multipart_file_past_the_task_quota_and_keep_the_earlier_ones() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        attachments_per_task: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let sent = fx
        .upload_form(
            &task,
            &[
                ("a.txt", b"1".to_vec()),
                ("b.txt", b"2".to_vec()),
                ("c.txt", b"3".to_vec()),
            ],
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let results = sent.body["results"].as_array().expect("results");
    assert_eq!(results[2]["error"]["code"], "quota_exceeded");
    assert!(results[0]["attachment"]["id"].is_string());
    assert!(results[1]["attachment"]["id"].is_string());
}

#[tokio::test]
async fn upload_should_refuse_a_multipart_body_without_a_boundary() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let auth = [
        ("authorization", fx.member_bearer.as_str()),
        ("content-type", "multipart/form-data"),
    ];
    let uri = format!("/api/tasks/{task}/attachments");
    let sent = send_bytes(&fx.app, "POST", &uri, &auth, None, b"x".to_vec()).await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST, "{}", sent.body);
}

#[tokio::test]
async fn upload_should_refuse_a_file_over_the_admin_attachment_limit_and_state_it() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    fx.set_disk(DiskLimits {
        max_attachment_mib: LIMIT_MIB,
        ..DiskLimits::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let fits = fx.upload(&task, "fits.bin", vec![1; 512 * 1024]).await;
    assert_eq!(fits.status, StatusCode::OK, "{}", fits.body);
    let over = fx.upload(&task, "over.bin", vec![1; 1024 * 1024 + 1]).await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST, "{}", over.body);
    assert!(
        over.body["message"]
            .as_str()
            .is_some_and(|message| message.contains("1 MiB limit")),
        "{}",
        over.body
    );
    let version = call(&fx.app, "GET", "/api/v1/version", &[], None).await;
    assert_eq!(version.body["max_attachment_bytes"], MIB);
}

#[tokio::test]
async fn quota_should_refuse_bytes_past_the_stored_limit_and_free_them_on_delete() {
    const LIMIT_MIB: u32 = 1;
    const FILE_BYTES: usize = 1000 * 1024;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let second = fx.upload(&task, "b.bin", vec![2; FILE_BYTES]).await;
    assert_quota(&second, Quota::StoredMibPerAccount, LIMIT_MIB);

    let uri = format!("/api/tasks/{task}");
    let grown = json!({"description": "d".repeat(40 * 1024)});
    let refused = fx.member("PATCH", &uri, Some(grown.clone())).await;
    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    assert!(
        fx.db
            .task(&task)
            .expect("read")
            .expect("task")
            .description
            .is_empty()
    );

    let attachment = stored.body["id"].as_str().expect("id");
    let deleted = fx
        .member("DELETE", &format!("{uri}/attachments/{attachment}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    let fits = fx.member("PATCH", &uri, Some(grown)).await;
    assert_eq!(fits.status, StatusCode::OK, "{}", fits.body);
    let held = fx.db.stored_bytes_by_account().expect("usage")[fx.member.as_str()];
    assert!(held < u64::from(LIMIT_MIB) * MIB, "{held}");

    let admin_task = {
        let (_, column) = fx.board(&fx.admin, "Admin");
        fx.db
            .create_task(&column, None, "Admin task", 1, None)
            .expect("task")
    };
    let admin = fx
        .admin(
            "PATCH",
            &format!("/api/tasks/{admin_task}"),
            Some(json!({"description": "a".repeat(60 * 1024)})),
        )
        .await;
    assert_eq!(admin.status, StatusCode::OK, "the admin has no quota");
}

#[tokio::test]
async fn rename_should_succeed_on_an_account_at_its_stored_limit() {
    const FILE_BYTES: usize = 1000 * 1024;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: 1,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let uri = format!("/api/tasks/{task}");
    let mut length = 100;
    loop {
        let fill = json!({"description": "d".repeat(length)});
        let sent = fx.member("PATCH", &uri, Some(fill)).await;
        if sent.status != StatusCode::OK {
            assert_quota(&sent, Quota::StoredMibPerAccount, 1);
            break;
        }
        length += 100;
    }

    let attachment = stored.body["id"].as_str().expect("id");
    let before = fx.db.stored_bytes_by_account().expect("usage")[fx.member.as_str()];
    let renamed = fx
        .member(
            "PATCH",
            &format!("{uri}/attachments/{attachment}"),
            Some(json!({"name": "n".repeat(240)})),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    assert_eq!(renamed.body["name"], format!("{}.bin", "n".repeat(240)));
    let after = fx.db.stored_bytes_by_account().expect("usage")[fx.member.as_str()];
    let delta = "n".repeat(240).len() - "a".len();
    assert!(
        after >= before + delta as u64,
        "{before} bytes grew to {after}, less than the {delta} the name added"
    );
}

#[tokio::test]
async fn rename_should_refuse_a_name_of_more_than_255_bytes() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; 10]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let attachment = stored.body["id"].as_str().expect("id");
    let uri = format!("/api/tasks/{task}/attachments/{attachment}");
    let refused = fx
        .member("PATCH", &uri, Some(json!({"name": "n".repeat(252)})))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let fits = fx
        .member("PATCH", &uri, Some(json!({"name": "n".repeat(251)})))
        .await;
    assert_eq!(fits.status, StatusCode::OK, "{}", fits.body);
}

#[tokio::test]
async fn rename_should_refuse_a_name_with_a_slash_or_a_backslash() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; 10]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let attachment = stored.body["id"].as_str().expect("id");
    let uri = format!("/api/tasks/{task}/attachments/{attachment}");
    for name in ["dir/plan", "dir\\plan", "/", "\\"] {
        let refused = fx.member("PATCH", &uri, Some(json!({"name": name}))).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{name}: {}",
            refused.body
        );
    }
    let row = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(row.attachments[0].name, "a.bin");
    let fits = fx
        .member("PATCH", &uri, Some(json!({"name": "plan"})))
        .await;
    assert_eq!(fits.status, StatusCode::OK, "{}", fits.body);
}

#[tokio::test]
async fn rename_should_stop_at_the_task_row_cap_with_a_409() {
    const COMMENT_BYTES: usize = 5000;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; 10]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let attachment = stored.body["id"].as_str().expect("id");
    let comment = |body: &'static str| NewComment {
        author: "bob",
        body,
        actor_kind: ActorKind::User,
        token_name: None,
        token_id: None,
        kind: None,
    };
    let big = "c".repeat(COMMENT_BYTES);
    let big = NewComment {
        body: &big,
        ..comment("")
    };
    while fx.db.add_comment(&task, big, Actor::SYSTEM, None).is_ok() {}
    while fx
        .db
        .add_comment(&task, comment("c"), Actor::SYSTEM, None)
        .is_ok()
    {}

    let uri = format!("/api/tasks/{task}/attachments/{attachment}");
    let refused = fx
        .member("PATCH", &uri, Some(json!({"name": "n".repeat(251)})))
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("quota_exceeded"));
    let message = refused.body["message"].as_str().expect("message");
    assert!(message.contains("bytes in one task"), "{message}");
    let row = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(row.attachments[0].name, "a.bin");
    let row_bytes = serde_json::to_vec(&row).expect("json").len();
    assert!(row_bytes <= MAX_TASK_ROW_BYTES, "{row_bytes} bytes");
}

#[tokio::test]
async fn body_limit_should_refuse_a_large_body_everywhere_but_uploads() {
    let fx = Fixture::multi();
    let (column, task) = fx.member_task();
    let padded = json!({"column_id": column, "name": "Big", "padding": "p".repeat(300 * 1024)});
    let refused = fx.member("POST", "/api/tasks", Some(padded)).await;
    assert_eq!(
        refused.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        refused.body
    );
    let uploaded = fx.upload(&task, "big.bin", vec![3; 300 * 1024]).await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.body);
}

#[tokio::test]
async fn upload_should_refuse_a_raw_body_over_the_cap_and_state_the_cap() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let max = AccountDb::MAX_ATTACHMENT_BYTES as usize;
    let stored = fx
        .upload(&task, "small.bin", vec![4; 3 * 1024 * 1024])
        .await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(stored.body["name"], "small.bin");
    let over = fx.upload(&task, "over.bin", vec![4; max + 1]).await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST, "{}", over.body);
    assert!(
        over.body["message"]
            .as_str()
            .is_some_and(|message| message.contains("50 MiB")),
        "{}",
        over.body
    );
    assert_eq!(
        fx.db
            .task(&task)
            .expect("read")
            .expect("task")
            .attachments
            .len(),
        1
    );
    let declared = (max + 1).to_string();
    let announced = send_bytes(
        &fx.app,
        "POST",
        &format!("/api/tasks/{task}/attachments?name=announced.bin"),
        &[
            ("authorization", fx.member_bearer.as_str()),
            ("content-type", "application/octet-stream"),
            ("content-length", declared.as_str()),
        ],
        None,
        vec![1],
    )
    .await;
    assert_eq!(
        announced.status,
        StatusCode::BAD_REQUEST,
        "{}",
        announced.body
    );
    assert!(
        announced.body["message"]
            .as_str()
            .is_some_and(|message| message.contains("50 MiB")),
        "{}",
        announced.body
    );
    let auth = [
        ("authorization", fx.member_bearer.as_str()),
        ("content-type", "application/octet-stream"),
    ];
    let nameless = send_bytes(
        &fx.app,
        "POST",
        &format!("/api/tasks/{task}/attachments"),
        &auth,
        None,
        vec![1],
    )
    .await;
    assert_eq!(
        nameless.status,
        StatusCode::BAD_REQUEST,
        "{}",
        nameless.body
    );
}

/// Returns `len` bytes that differ from one position to the next.
fn patterned(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_u32;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

#[tokio::test]
async fn upload_should_round_trip_a_file_of_the_cap_byte_for_byte() {
    let fx = Fixture::multi();
    let (_, column) = fx.board(&fx.admin, "Files");
    let task = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let max = AccountDb::MAX_ATTACHMENT_BYTES as usize;
    let file = patterned(max);
    let auth = [
        ("authorization", fx.admin_bearer.as_str()),
        ("content-type", "application/octet-stream"),
    ];
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
    let sent = file.clone();
    tokio::spawn(async move {
        for chunk in sent.chunks(1024 * 1024) {
            sender
                .send(Bytes::copy_from_slice(chunk))
                .await
                .expect("send");
        }
    });
    let stored = send_body(
        &fx.app,
        "POST",
        &format!("/api/tasks/{task}/attachments?name=cap.bin"),
        &auth,
        None,
        Body::new(Channeled(receiver)),
    )
    .await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(stored.body["size"], max);
    let id = stored.body["id"].as_str().expect("id");
    let downloaded = get_unread(
        &fx,
        &format!("/api/tasks/{task}/attachments/{id}/file"),
        &fx.admin_bearer,
    )
    .await;
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(
        downloaded
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some(max.to_string().as_str())
    );
    assert!(body_of(downloaded).await[..] == file[..], "bytes differ");
}

#[tokio::test]
async fn upload_should_refuse_a_raw_mime_over_its_cap_before_receiving() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let auth = [
        ("authorization", fx.member_bearer.as_str()),
        ("content-type", "application/octet-stream"),
    ];
    let uri = format!(
        "/api/tasks/{task}/attachments?name=a.bin&mime={}",
        "m".repeat(201)
    );
    let refused = send_bytes(&fx.app, "POST", &uri, &auth, None, vec![1]).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert!(
        refused.body["message"]
            .as_str()
            .is_some_and(|message| message.contains("mime")),
        "{}",
        refused.body
    );
    assert_eq!(
        std::fs::read_dir(fx.attachments_dir())
            .expect("dir")
            .count(),
        0
    );
}

#[tokio::test]
async fn an_empty_file_should_download_as_an_empty_body() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "empty.bin", Vec::new()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let id = stored.body["id"].as_str().expect("id");
    let downloaded = get_unread(
        &fx,
        &format!("/api/tasks/{task}/attachments/{id}/file"),
        &fx.member_bearer,
    )
    .await;
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert!(body_of(downloaded).await.is_empty());
}

#[tokio::test]
async fn upload_should_write_each_chunk_to_disk_before_the_body_ends() {
    let fx = Fixture::multi();
    let (_, column) = fx.board(&fx.admin, "Files");
    let task = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
    let app = fx.app.clone();
    let bearer = fx.admin_bearer.clone();
    let uri = format!("/api/tasks/{task}/attachments?name=slow.bin");
    let request = tokio::spawn(async move {
        let auth = [
            ("authorization", bearer.as_str()),
            ("content-type", "application/octet-stream"),
        ];
        send_body(
            &app,
            "POST",
            &uri,
            &auth,
            None,
            Body::new(Channeled(receiver)),
        )
        .await
    });
    let chunk = Bytes::from(vec![9; 4 * 1024 * 1024]);
    sender.send(chunk.clone()).await.expect("send");
    sender.send(chunk).await.expect("send");
    let dir = fx.attachments_dir();
    let mut on_disk = 0;
    for _ in 0..200 {
        on_disk = std::fs::read_dir(&dir)
            .expect("dir")
            .map(|entry| entry.expect("entry").metadata().expect("meta").len())
            .sum::<u64>();
        if on_disk >= 4 * 1024 * 1024 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(on_disk >= 4 * 1024 * 1024, "{on_disk} bytes on disk");
    assert!(!request.is_finished(), "the request ended before its body");
    drop(sender);
    let stored = request.await.expect("request");
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(stored.body["size"], 8 * 1024 * 1024);
}

#[tokio::test]
async fn an_abandoned_upload_should_leave_no_file() {
    let fx = Fixture::multi();
    let (_, column) = fx.board(&fx.admin, "Files");
    let task = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
    let app = fx.app.clone();
    let bearer = fx.admin_bearer.clone();
    let uri = format!("/api/tasks/{task}/attachments?name=gone.bin");
    let request = tokio::spawn(async move {
        let auth = [
            ("authorization", bearer.as_str()),
            ("content-type", "application/octet-stream"),
        ];
        send_body(
            &app,
            "POST",
            &uri,
            &auth,
            None,
            Body::new(Channeled(receiver)),
        )
        .await
    });
    sender
        .send(Bytes::from(vec![1; 1024 * 1024]))
        .await
        .expect("send");
    settle().await;
    request.abort();
    drop(sender);
    settle().await;
    let dir = fx.attachments_dir();
    assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 0);
    assert!(
        fx.db
            .task(&task)
            .expect("read")
            .expect("task")
            .attachments
            .is_empty()
    );
}

/// Seconds a raw upload gets to be answered without its body.
const REPLY_WAIT_SECS: u64 = 5;

/// Starts a raw upload of `name` to `task` whose body stays open, with `length` as its declared
/// Content-Length when given, and returns the reply once the server answers without it.
async fn answered_before_the_body(
    fx: &Fixture,
    task: &str,
    name: &str,
    length: Option<&str>,
) -> Reply {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
    let app = fx.app.clone();
    let bearer = fx.member_bearer.clone();
    let uri = format!("/api/tasks/{task}/attachments?name={name}");
    let length = length.map(str::to_string);
    let request = tokio::spawn(async move {
        let mut headers = vec![
            ("authorization", bearer.as_str()),
            ("content-type", "application/octet-stream"),
        ];
        if let Some(length) = &length {
            headers.push(("content-length", length.as_str()));
        }
        send_body(
            &app,
            "POST",
            &uri,
            &headers,
            None,
            Body::new(Channeled(receiver)),
        )
        .await
    });
    let answered = tokio::time::timeout(Duration::from_secs(REPLY_WAIT_SECS), request).await;
    drop(sender);
    answered
        .expect("the server waited for the body")
        .expect("request")
}

#[tokio::test]
async fn upload_should_refuse_a_task_at_its_attachment_limit_before_receiving_the_body() {
    const LIMIT: u32 = 1;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        attachments_per_task: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; 16]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);

    let refused = answered_before_the_body(&fx, &task, "b.bin", None).await;

    assert_quota(&refused, Quota::AttachmentsPerTask, LIMIT);
    assert_eq!(
        std::fs::read_dir(fx.attachments_dir())
            .expect("dir")
            .count(),
        1
    );
}

#[tokio::test]
async fn upload_should_refuse_a_declared_length_over_the_quota_before_receiving_the_body() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let declared = (2 * MIB).to_string();

    let refused = answered_before_the_body(&fx, &task, "big.bin", Some(&declared)).await;

    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    assert_eq!(
        std::fs::read_dir(fx.attachments_dir())
            .expect("dir")
            .count(),
        0
    );
}

#[tokio::test]
async fn upload_with_no_declared_length_should_be_refused_after_the_write_and_leave_no_file() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
    let app = fx.app.clone();
    let bearer = fx.member_bearer.clone();
    let uri = format!("/api/tasks/{task}/attachments?name=big.bin");
    let request = tokio::spawn(async move {
        let auth = [
            ("authorization", bearer.as_str()),
            ("content-type", "application/octet-stream"),
        ];
        send_body(
            &app,
            "POST",
            &uri,
            &auth,
            None,
            Body::new(Channeled(receiver)),
        )
        .await
    });
    sender
        .send(Bytes::from(vec![1; 2 * MIB as usize]))
        .await
        .expect("send");
    drop(sender);

    let refused = request.await.expect("request");

    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    assert_eq!(
        std::fs::read_dir(fx.attachments_dir())
            .expect("dir")
            .count(),
        0
    );
}

/// A request body that sends what its channel carries and ends when the channel closes.
struct Channeled(tokio::sync::mpsc::Receiver<Bytes>);

impl HttpBody for Channeled {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|chunk| Ok(Frame::data(chunk))))
    }
}

#[tokio::test]
async fn version_should_state_both_attachment_caps() {
    let fx = Fixture::multi();
    let reply = call(&fx.app, "GET", "/api/v1/version", &[], None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.body["max_attachment_bytes"],
        AccountDb::MAX_ATTACHMENT_BYTES
    );
    assert_eq!(
        reply.body["max_json_attachment_bytes"],
        AccountDb::MAX_JSON_ATTACHMENT_BYTES
    );
}

#[tokio::test(start_paused = true)]
async fn upload_should_answer_busy_while_two_uploads_hold_every_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::connect(dir.path().join("uploads.redb").to_str().expect("utf8")).expect("db");
    let admin = db.create_user("admin").expect("admin");
    let (record, raw) = new_api_token_record(&admin, "agent", AuthScope::Admin, None);
    db.create_api_token(&record).expect("token");
    let board = db.create_board(&admin, "Board").expect("board");
    let column = db.create_column(&board, "Todo", None).expect("column");
    let task = db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let state = AppState::new(db.clone(), None, passkey_mode())
        .with_disk(held_disk(&db))
        .with_limits(unthrottled_limits());
    let slots = state.uploads.clone();
    let app = router(state);
    let bearer = format!("Bearer {raw}");
    let auth = [
        ("authorization", bearer.as_str()),
        ("content-type", "application/octet-stream"),
    ];
    let uri = format!("/api/tasks/{task}/attachments?name=a.bin");

    let held = slots
        .clone()
        .acquire_many_owned(AppState::UPLOADS_AT_ONCE as u32)
        .await
        .expect("slots");
    let busy = send_bytes(&app, "POST", &uri, &auth, None, vec![1; 16]).await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.body
    );
    assert_eq!(busy.code.as_deref(), Some("busy"));
    drop(held);
    let taken = send_bytes(&app, "POST", &uri, &auth, None, vec![1; 16]).await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
}

#[tokio::test]
async fn responses_should_carry_the_security_headers() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "résumé.pdf", b"%PDF".to_vec()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let file = format!(
        "/api/tasks/{task}/attachments/{}/file",
        stored.body["id"].as_str().expect("id")
    );
    for (uri, headers) in [
        ("/login", vec![]),
        ("/settings", vec![("cookie", fx.member_cookie.as_str())]),
        ("/api/v1/version", vec![]),
        (
            "/api/boards",
            vec![("authorization", fx.member_bearer.as_str())],
        ),
        (
            "/api/nope",
            vec![("authorization", fx.member_bearer.as_str())],
        ),
        (
            file.as_str(),
            vec![("authorization", fx.member_bearer.as_str())],
        ),
    ] {
        let response = fx
            .app
            .clone()
            .oneshot({
                let mut builder = Request::builder().uri(uri);
                for (name, value) in &headers {
                    builder = builder.header(*name, *value);
                }
                builder.body(Body::empty()).expect("request")
            })
            .await
            .expect("oneshot");
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|value| value.to_str().expect("ascii").to_string())
        };
        assert_eq!(
            header("x-content-type-options").as_deref(),
            Some("nosniff"),
            "{uri}"
        );
        assert_eq!(
            header("referrer-policy").as_deref(),
            Some("same-origin"),
            "{uri}"
        );
        assert_eq!(
            header("strict-transport-security"),
            None,
            "{uri}: no https public URL"
        );
        if uri == file {
            assert_eq!(
                header("content-disposition").as_deref(),
                Some("attachment; filename=\"r_sum_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf")
            );
        }
    }
}

#[tokio::test]
async fn responses_should_carry_a_strict_script_policy() {
    let fx = Fixture::multi();
    for (uri, headers) in [
        ("/login", vec![]),
        ("/no-such-page", vec![]),
        ("/api/v1/version", vec![]),
        (
            "/api/nope",
            vec![("authorization", fx.member_bearer.as_str())],
        ),
    ] {
        let response = fx
            .app
            .clone()
            .oneshot({
                let mut builder = Request::builder().uri(uri);
                for (name, value) in &headers {
                    builder = builder.header(*name, *value);
                }
                builder.body(Body::empty()).expect("request")
            })
            .await
            .expect("oneshot");
        let policy = response
            .headers()
            .get("content-security-policy")
            .map(|value| value.to_str().expect("ascii"));
        assert_eq!(
            policy,
            Some(
                "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data:; connect-src 'self'; font-src 'self'; form-action 'self'; \
                 frame-ancestors 'none'; object-src 'none'; base-uri 'none'"
            ),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn responses_should_require_https_behind_an_https_public_url() {
    let transport = |public_url: &str| {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("hsts.redb").to_str().expect("utf8")).expect("db");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode())
                .with_disk(held_disk(&db))
                .with_limits(unthrottled_limits())
                .with_public_url(Some(public_url.to_string())),
        );
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/version")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("oneshot");
            drop(dir);
            response
                .headers()
                .get("strict-transport-security")
                .map(|value| value.to_str().expect("ascii").to_string())
        }
    };
    assert_eq!(
        transport("https://pomb.test").await.as_deref(),
        Some("max-age=31536000")
    );
    assert_eq!(transport("http://localhost:3000").await, None);
}

#[tokio::test]
async fn device_code_lockout_should_count_per_client_network() {
    let fx = Fixture::multi();
    let wrong = |peer: &'static str| {
        let cookie = fx.admin_cookie.clone();
        let app = fx.app.clone();
        async move {
            let headers = [
                ("cookie", cookie.as_str()),
                ("content-type", "application/x-www-form-urlencoded"),
            ];
            send_bytes(
                &app,
                "POST",
                "/device",
                &headers,
                Some(peer),
                b"user_code=QQQQ-QQQQ".to_vec(),
            )
            .await
            .status
        }
    };
    for _ in 0..DeviceCodeLockout::MAX_FAILURES {
        assert_eq!(wrong("203.0.113.9").await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(wrong("203.0.113.9").await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(wrong("198.51.100.4").await, StatusCode::BAD_REQUEST);
}

#[test]
fn full_ceremony_pool_should_answer_429_busy() {
    let error: Box<dyn std::error::Error> = Box::new(CeremoniesFull);
    let response = AppError::from(error).into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.extensions().get::<ErrorCode>(),
        Some(&ErrorCode::Busy)
    );
}

#[tokio::test]
async fn disk_guard_should_stop_writes_while_the_file_is_over_the_limit() {
    let fx = Fixture::probed(DiskProbe {
        file: Arc::new(|| Ok(OVERSIZED_MIB * MIB)),
        ..probe(1, ROOMY_MIB)
    });
    let (column, _) = fx.member_task();
    let refused = fx
        .member("POST", "/api/tasks", Some(new_task(&column, "Full")))
        .await;
    assert_storage_full(&refused);
}

/// A request body that never sends a byte.
struct Stalled;

impl HttpBody for Stalled {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Pending
    }
}

/// Times a test yields so every spawned request runs as far as it can.
const SETTLE_YIELDS: usize = 20;

/// Lets every spawned task run as far as it can before the clock moves.
async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

/// Checks that the spawned `request` is still running `secs - 1` seconds in, then answers 408
/// `request_timeout` after 2 more.
async fn assert_times_out_after(request: JoinHandle<Reply>, secs: u64) {
    settle().await;
    tokio::time::advance(Duration::from_secs(secs - 1)).await;
    settle().await;
    assert!(!request.is_finished(), "answered before {secs} seconds");
    tokio::time::advance(Duration::from_secs(2)).await;
    let reply = request.await.expect("request");
    assert_eq!(reply.status, StatusCode::REQUEST_TIMEOUT, "{}", reply.body);
    assert_eq!(reply.code.as_deref(), Some("request_timeout"));
}

/// Seconds a request may wait while the database writer is held, so one that waits for it fails.
const WRITER_WAIT_SECS: u64 = 5;

/// Holds the database writer on its own thread until the returned sender is dropped or sent to.
fn hold_writes(db: &Db) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (release, released) = mpsc::channel::<()>();
    let (ready, wait_ready) = mpsc::channel::<()>();
    let db = db.clone();
    let writer = thread::spawn(move || {
        db.with_writes_held(|| {
            ready.send(()).expect("ready");
            let _ = released.recv();
            Ok(())
        })
        .expect("held");
    });
    wait_ready.recv().expect("writer held");
    (release, writer)
}

#[tokio::test(start_paused = true)]
async fn stalled_bodies_should_get_408_after_their_timeout() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let app = fx.app.clone();
    let bearer = fx.member_bearer.clone();
    let stalled = move |uri: String, content_type: &'static str| {
        let (app, bearer) = (app.clone(), bearer.clone());
        tokio::spawn(async move {
            let headers = [
                ("authorization", bearer.as_str()),
                ("content-type", content_type),
            ];
            send_body(&app, "POST", &uri, &headers, None, Body::new(Stalled)).await
        })
    };
    assert_times_out_after(
        stalled("/api/tasks".to_string(), "application/json"),
        REQUEST_TIMEOUT_SECS,
    )
    .await;
    let upload = format!("/api/tasks/{task}/attachments?name=slow.bin");
    assert_times_out_after(
        stalled(upload, "application/octet-stream"),
        SLOW_REQUEST_TIMEOUT_SECS,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_backup_behind_held_writes_should_get_408_after_the_slow_timeout() {
    let fx = Fixture::multi();
    let (release, writer) = hold_writes(&fx.db);
    let (app, cookie) = (fx.app.clone(), fx.admin_cookie.clone());
    let backup = tokio::spawn(async move {
        let headers = [("cookie", cookie.as_str())];
        send_body(
            &app,
            "POST",
            "/api/v1/backups",
            &headers,
            None,
            Body::empty(),
        )
        .await
    });
    assert_times_out_after(backup, SLOW_REQUEST_TIMEOUT_SECS).await;
    drop(release);
    writer.join().expect("writer");
}

#[tokio::test]
async fn the_admin_page_should_show_the_largest_use_beside_each_storage_limit() {
    const FILE_BYTES: usize = 1536 * 1024;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "big.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let page = fx.admin("GET", "/admin", None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let html = page.body.as_str().expect("html");
    assert!(html.contains("name=\"stored_mib_per_board\""), "{html}");
    assert!(html.contains("name=\"max_attachment_mib\""), "{html}");
    assert!(
        html.contains("Largest attachment right now: 1.5 MiB"),
        "{html}"
    );
    assert!(html.contains("Largest board right now: 1.5 MiB"), "{html}");
}

#[tokio::test]
async fn the_single_mode_storage_pane_should_show_the_use_beside_its_limits() {
    const FILE_BYTES: usize = 1536 * 1024;
    let fx = Fixture::new(InstanceMode::Single, unthrottled_limits(), |_| roomy());
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "big.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);

    let page = fx.admin("GET", "/settings", None).await;

    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let html = page.body.as_str().expect("html");
    assert!(html.contains("id=\"storage-max-attachment\""), "{html}");
    assert!(
        html.contains("Largest attachment right now: 1.5 MiB"),
        "{html}"
    );
    assert!(html.contains("id=\"storage-usage\""), "{html}");
}

#[tokio::test]
async fn a_stale_disk_guard_should_refuse_writes_and_serve_pages_while_writes_are_held() {
    let clock = Clock::new();
    let ticking = clock.clone();
    let fx = Fixture::new(InstanceMode::Multi, unthrottled_limits(), move |db| {
        DiskGuard::with_clock(
            DiskProbe {
                free: Arc::new(|_| Ok(ROOMY_MIB * MIB)),
                ..DiskProbe::of(db)
            },
            Path::new("."),
            ticking.guard(),
        )
    });
    let (column, _) = fx.member_task();
    clock.advance(DiskGuard::STALE_AFTER + Duration::from_secs(1));
    let (release, writer) = hold_writes(&fx.db);
    let wait = Duration::from_secs(WRITER_WAIT_SECS);
    let page = tokio::time::timeout(wait, fx.admin("GET", "/admin", None))
        .await
        .expect("a page never waits for the writer");
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(
        page.body.to_string().contains("out of date"),
        "{}",
        page.body
    );
    let session = [("cookie", fx.member_cookie.as_str())];
    let refused = tokio::time::timeout(
        wait,
        call(
            &fx.app,
            "POST",
            "/api/tasks",
            &session,
            Some(new_task(&column, "Stale")),
        ),
    )
    .await
    .expect("the refusal never waits for the writer");
    assert_storage_full(&refused);
    drop(release);
    writer.join().expect("writer");
}

/// Returns the `POST /api/time/manual` body of 30 minutes on `task`, with `extra` fields.
fn manual_time(task: &str, extra: Value) -> Value {
    let mut body = json!({"task_id": task, "date": "2020-01-02", "from": "10:00", "to": "10:30"});
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    body
}

#[tokio::test]
async fn time_entry_text_should_be_capped() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let long_note = json!({"note": "n".repeat(4 * 1024 + 1)});
    let refused = fx
        .member(
            "POST",
            "/api/time/manual",
            Some(manual_time(&task, long_note.clone())),
        )
        .await;
    assert_too_long(&refused, "note", "4096 bytes");
    let labels: Vec<String> = (0..51).map(|n| format!("l{n}")).collect();
    let many = manual_time(&task, json!({"labels": labels}));
    let refused = fx.member("POST", "/api/time/manual", Some(many)).await;
    assert_too_long(&refused, "labels", "50 entries");
    let logged = fx
        .member(
            "POST",
            &format!("/api/tasks/{task}/time"),
            Some(json!({"minutes": 5, "note": "n".repeat(4 * 1024 + 1)})),
        )
        .await;
    assert_too_long(&logged, "note", "4096 bytes");

    let short = manual_time(&task, json!({"note": "ok"}));
    let made = fx.member("POST", "/api/time/manual", Some(short)).await;
    assert!(made.status.is_success(), "{}", made.body);
    let entry = made.body["id"].as_str().expect("entry id");
    let refused = fx
        .member(
            "PUT",
            &format!("/api/time/entries/{entry}"),
            Some(manual_time(&task, long_note)),
        )
        .await;
    assert_too_long(&refused, "note", "4096 bytes");
}

#[tokio::test]
async fn color_descriptions_should_be_capped() {
    let fx = Fixture::multi();
    let (board, _) = fx.board(&fx.member, "Board");
    let color = fx.db.list_colors(&board).expect("colors")[0].id.clone();
    let refused = fx
        .member(
            "PATCH",
            &format!("/api/boards/{board}/colors/{color}"),
            Some(json!({"description": "d".repeat(501)})),
        )
        .await;
    assert_too_long(&refused, "description", "500 characters");
}

#[tokio::test]
async fn timer_stop_should_record_its_entry_past_the_stored_limit() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; MIB as usize + 1]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let manual = fx
        .member(
            "POST",
            "/api/time/manual",
            Some(manual_time(&task, json!({}))),
        )
        .await;
    assert_quota(&manual, Quota::StoredMibPerAccount, LIMIT_MIB);

    let started_at = chrono::Utc::now().timestamp() - 600;
    fx.db
        .account(&fx.member)
        .set_active_timer(&ActiveTimer {
            task_id: Some(task.clone()),
            mode: TimerMode::Stopwatch,
            started_at,
            duration_secs: None,
        })
        .expect("timer");
    let stopped = fx.member("POST", "/api/timer/stop", Some(json!({}))).await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    let entries = fx.db.list_entries(&task).expect("entries");
    assert_eq!(entries.len(), 1, "the session is recorded");
}

/// Starts `fx`'s member on a pomodoro of `task` that began ten minutes ago.
fn run_pomodoro(fx: &Fixture, task: &str) {
    const RAN_SECS: i64 = 600;
    const POMODORO_SECS: u64 = 1500;
    fx.db
        .account(&fx.member)
        .set_active_timer(&ActiveTimer {
            task_id: Some(task.to_string()),
            mode: TimerMode::Pomodoro,
            started_at: chrono::Utc::now().timestamp() - RAN_SECS,
            duration_secs: Some(POMODORO_SECS),
        })
        .expect("timer");
}

#[tokio::test]
async fn timer_stop_should_refuse_an_over_cap_reason_and_keep_the_timer_running() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    run_pomodoro(&fx, &task);
    let reason = "r".repeat(1025);
    let refused = fx
        .member("POST", "/api/timer/stop", Some(json!({"reason": reason})))
        .await;
    assert_too_long(&refused, "reason", "1024 bytes");
    let account = fx.db.account(&fx.member);
    assert!(account.active_timer().expect("timer").is_some());
    assert!(fx.db.list_entries(&task).expect("entries").is_empty());

    let stopped = fx
        .member(
            "POST",
            "/api/timer/stop",
            Some(json!({"reason": "r".repeat(1024)})),
        )
        .await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    assert!(account.active_timer().expect("timer").is_none());
    assert_eq!(fx.db.list_entries(&task).expect("entries").len(), 1);
}

#[tokio::test]
async fn timer_stop_should_be_refused_once_a_mib_past_the_stored_limit() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx
        .upload(&task, "a.bin", vec![1; 2 * MIB as usize + 1])
        .await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    run_pomodoro(&fx, &task);
    let refused = fx.member("POST", "/api/timer/stop", Some(json!({}))).await;
    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    let account = fx.db.account(&fx.member);
    assert!(account.active_timer().expect("timer").is_some());
    assert!(fx.db.list_entries(&task).expect("entries").is_empty());
}

#[tokio::test]
async fn api_tokens_should_stop_at_their_cap() {
    let fx = Fixture::multi();
    let held = fx
        .db
        .list_api_tokens()
        .expect("tokens")
        .iter()
        .filter(|token| token.account_id == fx.member.as_str())
        .count();
    for n in held..MAX_API_TOKENS_PER_ACCOUNT {
        let (record, _) = new_api_token_record(&fx.member, &format!("t{n}"), AuthScope::Read, None);
        fx.db.create_api_token(&record).expect("token");
    }
    let session = [("cookie", fx.member_cookie.as_str())];
    let body = json!({"name": "one more", "scopes": ["read"]});
    let refused = call(&fx.app, "POST", "/api/v1/auth/tokens", &session, Some(body)).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("quota_exceeded"));
}

#[tokio::test]
async fn quota_should_refuse_time_entries_past_the_stored_limit() {
    const LIMIT_MIB: u32 = 1;
    const HEADROOM_BYTES: u64 = 2048;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let held = fx.db.stored_bytes_by_account().expect("usage")[fx.member.as_str()];
    let file = u64::from(LIMIT_MIB) * MIB - held - HEADROOM_BYTES;
    let stored = fx.upload(&task, "a.bin", vec![1; file as usize]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let body = json!({
        "task_id": task, "date": "2020-01-02", "from": "10:00", "to": "10:30",
        "note": "n".repeat(2 * HEADROOM_BYTES as usize),
    });
    let refused = fx.member("POST", "/api/time/manual", Some(body)).await;
    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
}

#[tokio::test]
async fn quota_should_refuse_time_entries_past_their_count_and_keep_the_timer_running() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        time_entries_per_account: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let manual = |from: &str, to: &str| json!({"task_id": task, "date": "2020-01-02", "from": from, "to": to, "note": ""});
    for (from, to) in [("09:00", "09:30"), ("10:00", "10:30")] {
        let logged = fx
            .member("POST", "/api/time/manual", Some(manual(from, to)))
            .await;
        assert_eq!(logged.status, StatusCode::OK, "{}", logged.body);
    }
    let refused = fx
        .member("POST", "/api/time/manual", Some(manual("11:00", "11:30")))
        .await;
    assert_quota(&refused, Quota::TimeEntriesPerAccount, LIMIT);

    run_pomodoro(&fx, &task);
    let account = fx.db.account(&fx.member);
    let running = account.active_timer().expect("timer").expect("running");
    let stopped = fx.member("POST", "/api/timer/stop", Some(json!({}))).await;
    assert_quota(&stopped, Quota::TimeEntriesPerAccount, LIMIT);
    let kept = account
        .active_timer()
        .expect("timer")
        .expect("still running");
    assert_eq!(kept.started_at, running.started_at);
}

#[tokio::test]
async fn quota_should_refuse_columns_swimlanes_and_fields_over_their_board_limits() {
    const LIMIT: u32 = 2;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        columns_per_board: LIMIT,
        swimlanes_per_board: LIMIT,
        fields_per_board: LIMIT,
        ..Quotas::default()
    })
    .await;
    let (board, _) = fx.board(&fx.member, "Board");
    let column = |n: u32| json!({"board_id": board, "name": format!("Column {n}")});
    let made = fx.member("POST", "/api/columns", Some(column(1))).await;
    assert!(made.status.is_success(), "{}", made.body);
    let refused = fx.member("POST", "/api/columns", Some(column(2))).await;
    assert_quota(&refused, Quota::ColumnsPerBoard, LIMIT);

    for n in 0..LIMIT {
        let lane = json!({"board_id": board, "name": format!("Lane {n}")});
        let made = fx.member("POST", "/api/swimlanes", Some(lane)).await;
        assert!(made.status.is_success(), "{}", made.body);
    }
    let lane = json!({"board_id": board, "name": "Over"});
    let refused = fx.member("POST", "/api/swimlanes", Some(lane)).await;
    assert_quota(&refused, Quota::SwimlanesPerBoard, LIMIT);

    let fields = format!("/api/boards/{board}/fields");
    for n in 0..LIMIT {
        let field = json!({"key": format!("f{n}"), "label": "F", "kind": "text"});
        let made = fx.member("POST", &fields, Some(field)).await;
        assert!(made.status.is_success(), "{}", made.body);
    }
    let field = json!({"key": "over", "label": "Over", "kind": "text"});
    let refused = fx.member("POST", &fields, Some(field)).await;
    assert_quota(&refused, Quota::FieldsPerBoard, LIMIT);
}

/// Returns a GET of `uri` that `bearer` authorizes.
fn bearer_get(uri: &str, bearer: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", bearer)
        .body(Body::empty())
        .expect("request")
}

/// Returns the whole body of `response`.
async fn body_of(response: Response) -> Bytes {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
}

#[tokio::test(start_paused = true)]
async fn unread_downloads_should_hold_their_slots_until_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::connect(dir.path().join("downloads.redb").to_str().expect("utf8")).expect("db");
    let admin = db.create_user("admin").expect("admin");
    let (record, raw) = new_api_token_record(&admin, "agent", AuthScope::Admin, None);
    db.create_api_token(&record).expect("token");
    let board = db.create_board(&admin, "Board").expect("board");
    let column = db.create_column(&board, "Todo", None).expect("column");
    let task = db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let file = db
        .add_attachment(
            &task,
            "a.bin",
            "application/octet-stream",
            b"data",
            Actor::SYSTEM,
        )
        .expect("attach")
        .expect("task");
    let state = AppState::new(db.clone(), None, passkey_mode())
        .with_disk(held_disk(&db))
        .with_limits(unthrottled_limits());
    let others = state
        .downloads
        .clone()
        .acquire_many_owned(AppState::DOWNLOADS_AT_ONCE as u32 - 1)
        .await
        .expect("slots");
    let app = router(state);
    let bearer = format!("Bearer {raw}");
    let uri = format!("/api/tasks/{task}/attachments/{}/file", file.id);

    let response = app
        .clone()
        .oneshot(bearer_get(&uri, &bearer))
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);
    let unread = response;
    let busy = app
        .clone()
        .oneshot(bearer_get(&uri, &bearer))
        .await
        .expect("oneshot");
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&body_of(busy).await).expect("json");
    assert_eq!(body["code"], "busy");
    drop(unread);
    let served = app
        .clone()
        .oneshot(bearer_get(&uri, &bearer))
        .await
        .expect("oneshot");
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(&body_of(served).await[..], b"data");
    drop(others);
}

/// Stores a file on a fresh task of `owner` directly and returns the URI that downloads it.
fn stored_file(fx: &Fixture, owner: &AccountId) -> String {
    let (_, column) = fx.board(owner, "Files");
    let task = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let file = fx
        .db
        .add_attachment(
            &task,
            "a.bin",
            "application/octet-stream",
            b"data",
            Actor::SYSTEM,
        )
        .expect("attach")
        .expect("task");
    format!("/api/tasks/{task}/attachments/{}/file", file.id)
}

#[tokio::test(start_paused = true)]
async fn downloads_should_hold_two_slots_per_account_after_the_owner_check() {
    let fx = Fixture::multi();
    let member_file = stored_file(&fx, &fx.member);
    let admin_file = stored_file(&fx, &fx.admin);
    let mut unread = Vec::new();
    for _ in 0..AppState::DOWNLOADS_PER_ACCOUNT_AT_ONCE {
        let response = get_unread(&fx, &member_file, &fx.member_bearer).await;
        assert_eq!(response.status(), StatusCode::OK);
        unread.push(response);
    }
    let busy = get_unread(&fx, &member_file, &fx.member_bearer).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let other = get_unread(&fx, &admin_file, &fx.admin_bearer).await;
    assert_eq!(
        other.status(),
        StatusCode::OK,
        "another account still gets a slot"
    );
    let foreign = get_unread(&fx, &admin_file, &fx.member_bearer).await;
    assert_eq!(
        foreign.status(),
        StatusCode::NOT_FOUND,
        "another account's file is refused before any slot is taken"
    );
    drop(unread.pop());
    let served = get_unread(&fx, &member_file, &fx.member_bearer).await;
    assert_eq!(served.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn an_upload_should_run_while_another_of_the_account_is_still_slow() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let _slow = fx
        .state
        .account_slot(AccountSlot::Upload, fx.member.as_str())
        .acquire_owned()
        .await
        .expect("slot");
    let stored = fx.upload(&task, "a.bin", vec![1; 16]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn ten_uploads_sent_at_once_should_all_be_stored() {
    const FILES: u8 = 10;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let mut sent = tokio::task::JoinSet::new();
    for n in 0..FILES {
        let app = fx.app.clone();
        let bearer = fx.member_bearer.clone();
        let uri = format!("/api/tasks/{task}/attachments?name=file{n}.bin");
        sent.spawn(async move {
            let auth = [
                ("authorization", bearer.as_str()),
                ("content-type", "application/octet-stream"),
            ];
            send_bytes(&app, "POST", &uri, &auth, None, vec![n; 64 * 1024]).await
        });
    }
    while let Some(reply) = sent.join_next().await {
        let reply = reply.expect("join");
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }
    let stored = fx.db.task(&task).expect("read").expect("task").attachments;
    assert_eq!(stored.len(), usize::from(FILES));
    for n in 0..FILES {
        let name = format!("file{n}.bin");
        let attachment = stored
            .iter()
            .find(|attachment| attachment.name == name)
            .unwrap_or_else(|| panic!("{name} was not stored"));
        let downloaded = get_unread(
            &fx,
            &format!("/api/tasks/{task}/attachments/{}/file", attachment.id),
            &fx.member_bearer,
        )
        .await;
        assert_eq!(downloaded.status(), StatusCode::OK, "{name}");
        assert!(
            body_of(downloaded).await[..] == vec![n; 64 * 1024][..],
            "{name} holds other bytes than were sent"
        );
    }
}

#[tokio::test]
async fn upload_should_report_the_same_file_twice_as_a_duplicate_with_the_existing_id() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let id = first.body["id"].as_str().expect("id");
    let again = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
    assert_eq!(again.code.as_deref(), Some("duplicate_attachment"));
    assert!(
        again.body["message"]
            .as_str()
            .is_some_and(|message| message.contains(id)),
        "{}",
        again.body
    );
    let held = fx
        .db
        .task(&task)
        .expect("read")
        .expect("task")
        .attachments
        .len();
    assert_eq!(held, 1);
}

#[tokio::test]
async fn upload_should_report_the_same_bytes_under_another_name_as_a_duplicate() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let renamed = fx.upload(&task, "copy.bin", vec![1; 32]).await;
    assert_eq!(renamed.status, StatusCode::CONFLICT, "{}", renamed.body);
    assert_eq!(renamed.code.as_deref(), Some("duplicate_attachment"));
}

#[tokio::test]
async fn upload_should_report_a_second_file_with_the_same_name_and_other_bytes_as_a_duplicate() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let changed = fx.upload(&task, "a.bin", vec![2; 32]).await;
    assert_eq!(changed.status, StatusCode::CONFLICT, "{}", changed.body);
    assert_eq!(changed.code.as_deref(), Some("duplicate_attachment"));
    let elsewhere = {
        let (_, other) = fx.member_task();
        fx.upload(&other, "a.bin", vec![1; 32]).await
    };
    assert_eq!(elsewhere.status, StatusCode::OK, "{}", elsewhere.body);
}

#[tokio::test]
async fn rename_should_refuse_a_name_another_attachment_holds() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let second = fx.upload(&task, "b.bin", vec![2; 32]).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    let first_id = first.body["id"].as_str().expect("id");
    let second_id = second.body["id"].as_str().expect("id");
    let refused = fx
        .member(
            "PATCH",
            &format!("/api/tasks/{task}/attachments/{second_id}"),
            Some(json!({"name": "a"})),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("duplicate_attachment"));
    assert!(
        refused.body["message"]
            .as_str()
            .is_some_and(|message| message.contains(first_id)),
        "{}",
        refused.body
    );
    let row = fx.db.task(&task).expect("read").expect("task");
    let names: Vec<&str> = row.attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["a.bin", "b.bin"]);
    assert!(history_details(&fx, &task, HistoryKind::AttachmentRenamed).is_empty());
}

#[tokio::test]
async fn rename_attachment_should_write_nothing_for_a_missing_task_or_attachment() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let first_id = first.body["id"].as_str().expect("id");
    let before = fx.db.task(&task).expect("read").expect("task");
    let before = serde_json::to_vec(&before).expect("json");
    let missing_attachment = fx
        .db
        .rename_attachment(&task, "no-such-attachment", "c", Actor::SYSTEM)
        .expect("rename");
    assert!(missing_attachment.is_none());
    let missing_task = fx
        .db
        .rename_attachment("no-such-task", first_id, "c", Actor::SYSTEM)
        .expect("rename");
    assert!(missing_task.is_none());
    let after = fx.db.task(&task).expect("read").expect("task");
    let after = serde_json::to_vec(&after).expect("json");
    assert_eq!(after, before);
    assert!(history_details(&fx, &task, HistoryKind::AttachmentRenamed).is_empty());
}

#[tokio::test]
async fn rename_should_keep_working_for_its_own_name_and_a_free_name() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let first_id = first.body["id"].as_str().expect("id");
    let uri = format!("/api/tasks/{task}/attachments/{first_id}");
    let own = fx.member("PATCH", &uri, Some(json!({"name": "a"}))).await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    assert_eq!(own.body["name"], "a.bin");
    let free = fx.member("PATCH", &uri, Some(json!({"name": "c"}))).await;
    assert_eq!(free.status, StatusCode::OK, "{}", free.body);
    assert_eq!(free.body["name"], "c.bin");
    let (_, other) = fx.member_task();
    let elsewhere = fx.upload(&other, "b.bin", vec![2; 32]).await;
    assert_eq!(elsewhere.status, StatusCode::OK, "{}", elsewhere.body);
    let elsewhere_id = elsewhere.body["id"].as_str().expect("id");
    let shared = fx
        .member(
            "PATCH",
            &format!("/api/tasks/{other}/attachments/{elsewhere_id}"),
            Some(json!({"name": "c"})),
        )
        .await;
    assert_eq!(shared.status, StatusCode::OK, "{}", shared.body);
    assert_eq!(shared.body["name"], "c.bin");
}

#[tokio::test]
async fn upload_should_name_the_existing_attachment_of_a_duplicate_multipart_file() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    let id = first.body["id"].as_str().expect("id").to_string();
    let sent = fx
        .upload_form(&task, &[("a.bin", vec![1; 32]), ("b.bin", b"new".to_vec())])
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let results = sent.body["results"].as_array().expect("results");
    assert_eq!(results[0]["error"]["code"], "duplicate_attachment");
    assert_eq!(results[0]["error"]["existing_id"], id);
    assert!(results[1]["attachment"]["id"].is_string(), "{}", results[1]);
}

#[tokio::test]
async fn upload_should_report_a_rerun_on_a_full_board_as_a_duplicate() {
    const LIMIT_MIB: u32 = 1;
    const FILE_BYTES: usize = 600 * 1024;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_board: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let id = first.body["id"].as_str().expect("id").to_string();
    let full = fx.upload(&task, "b.bin", vec![2; FILE_BYTES]).await;
    assert_quota(&full, Quota::StoredMibPerBoard, LIMIT_MIB);

    let rerun = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(rerun.code.as_deref(), Some("duplicate_attachment"));

    let renamed = fx
        .upload_form(&task, &[("copy.bin", vec![1; FILE_BYTES])])
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    let error = &renamed.body["results"][0]["error"];
    assert_eq!(error["code"], "duplicate_attachment", "{}", renamed.body);
    assert_eq!(error["existing_id"], id);
}

#[tokio::test]
async fn upload_should_count_a_binned_task_against_its_board_limit() {
    const LIMIT_MIB: u32 = 1;
    const FILE_BYTES: usize = 600 * 1024;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_board: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (column, task) = fx.member_task();
    let other = fx
        .db
        .create_task(&column, None, "Other", 1, None)
        .expect("task");
    let first = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert!(fx.db.delete_task(&task, Actor::SYSTEM).expect("delete"));

    let full = fx.upload(&other, "b.bin", vec![2; FILE_BYTES]).await;
    assert_quota(&full, Quota::StoredMibPerBoard, LIMIT_MIB);
}

#[tokio::test]
async fn upload_should_report_a_rerun_on_a_task_at_its_limit_as_a_duplicate() {
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        attachments_per_task: 1,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let rerun = fx.upload(&task, "a.bin", vec![1; 32]).await;
    assert_eq!(rerun.code.as_deref(), Some("duplicate_attachment"));

    let renamed = fx.upload_form(&task, &[("copy.bin", vec![1; 32])]).await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    let error = &renamed.body["results"][0]["error"];
    assert_eq!(error["code"], "duplicate_attachment", "{}", renamed.body);
}

#[tokio::test]
async fn upload_should_report_a_copy_on_a_board_past_its_limit_as_a_duplicate() {
    const FILE_BYTES: usize = 1536 * 1024;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let first = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    fx.set_quotas(Quotas {
        stored_mib_per_board: 1,
        ..Quotas::default()
    })
    .await;

    let renamed = fx
        .upload_form(&task, &[("copy.bin", vec![1; FILE_BYTES])])
        .await;

    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    let error = &renamed.body["results"][0]["error"];
    assert_eq!(error["code"], "duplicate_attachment", "{}", renamed.body);
    assert_eq!(error["existing_id"], first.body["id"]);

    let fresh = fx
        .upload_form(&task, &[("fresh.bin", vec![2; FILE_BYTES])])
        .await;
    let error = &fresh.body["results"][0]["error"];
    assert_eq!(error["code"], "quota_exceeded", "{}", fresh.body);
    let held = std::fs::read_dir(fx.attachments_dir()).expect("attachments");
    assert_eq!(held.count(), 1);
}

#[tokio::test]
async fn upload_should_name_the_files_stored_before_a_body_breaks_and_report_them_on_a_rerun() {
    const CUT_BYTES: usize = 40;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let body = form_body(&[("a.txt", b"one".to_vec()), ("b.txt", vec![2; 64])]);

    let broken = fx
        .upload_form_bytes(&task, body[..body.len() - CUT_BYTES].to_vec())
        .await;

    assert_eq!(broken.status, StatusCode::BAD_REQUEST, "{}", broken.body);
    let message = broken.body["message"].as_str().expect("message");
    assert!(message.contains("a.txt"), "{message}");
    assert!(!message.contains("b.txt"), "{message}");
    let held = fx.db.task(&task).expect("read").expect("task").attachments;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].name, "a.txt");

    let rerun = fx.upload_form_bytes(&task, body).await;
    assert_eq!(rerun.status, StatusCode::OK, "{}", rerun.body);
    let results = &rerun.body["results"];
    assert_eq!(results[0]["error"]["code"], "duplicate_attachment");
    assert_eq!(results[0]["error"]["existing_id"], held[0].id.as_str());
    assert!(results[1]["attachment"]["id"].is_string(), "{results}");
}

#[tokio::test(start_paused = true)]
async fn uploads_should_hold_one_slot_per_account_after_the_owner_check() {
    let fx = Fixture::multi();
    let (_, member_task) = fx.member_task();
    let (_, admin_column) = fx.board(&fx.admin, "Admin");
    let admin_task = fx
        .db
        .create_task(&admin_column, None, "Task", 1, None)
        .expect("task");
    let held = fx
        .state
        .account_slot(AccountSlot::Upload, fx.member.as_str())
        .acquire_many_owned(AppState::UPLOADS_PER_ACCOUNT_AT_ONCE as u32)
        .await
        .expect("slot");
    let busy = fx.upload(&member_task, "a.bin", vec![1; 16]).await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.body
    );
    assert_eq!(busy.code.as_deref(), Some("busy"));
    let admin_auth = [
        ("authorization", fx.admin_bearer.as_str()),
        ("content-type", "application/octet-stream"),
    ];
    let uri = format!("/api/tasks/{admin_task}/attachments?name=a.bin");
    let other = send_bytes(&fx.app, "POST", &uri, &admin_auth, None, vec![1; 16]).await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
    drop(held);

    let full = fx
        .state
        .uploads
        .clone()
        .acquire_many_owned(AppState::UPLOADS_AT_ONCE as u32)
        .await
        .expect("slots");
    let foreign = fx.upload(&admin_task, "a.bin", vec![1; 16]).await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "another account's task is refused before any slot is taken: {}",
        foreign.body
    );
    drop(full);
    let stored = fx.upload(&member_task, "a.bin", vec![1; 16]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
}

#[test]
fn held_disk_should_never_read_stale() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("held.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let guard = held_disk(&db);
    assert!(guard.latest().is_some_and(|sample| !sample.stale));
}

#[tokio::test]
async fn task_rows_should_stop_growing_at_one_mib_with_a_409() {
    const COMMENT_BYTES: usize = 5000;
    let fx = Fixture::new(InstanceMode::Multi, unthrottled_limits(), held_disk);
    let (_, task) = fx.member_task();
    let body = "c".repeat(COMMENT_BYTES);
    let comment = NewComment {
        author: "bob",
        body: &body,
        actor_kind: ActorKind::User,
        token_name: None,
        token_id: None,
        kind: None,
    };
    let refusal = (0..=MAX_TASK_ROW_BYTES / COMMENT_BYTES)
        .find_map(|_| fx.db.add_comment(&task, comment, Actor::SYSTEM, None).err())
        .expect("the row stops growing");
    let refusal = refusal
        .downcast_ref::<QuotaExceeded>()
        .expect("a quota refusal");
    assert_eq!(refusal.bound, Bound::TaskRowBytes);
    let stored = fx.db.task(&task).expect("read").expect("task");
    let row_bytes = serde_json::to_vec(&stored).expect("json").len();
    assert!(row_bytes <= MAX_TASK_ROW_BYTES, "{row_bytes} bytes");

    let uri = format!("/api/tasks/{task}/comments");
    let refused = fx.member("POST", &uri, Some(json!({"body": body}))).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("quota_exceeded"));
    let message = refused.body["message"].as_str().expect("message");
    assert!(message.contains("bytes in one task"), "{message}");
    let first = &stored.comments[0].id;
    let deleted = fx.member("DELETE", &format!("{uri}/{first}"), None).await;
    assert_eq!(
        deleted.status,
        StatusCode::OK,
        "a write that shrinks the row goes through"
    );
}

#[tokio::test]
async fn quota_should_count_board_layout_against_the_stored_limit() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (board, task) = {
        let (board, column) = fx.board(&fx.member, "Board");
        let task = fx
            .db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        (board, task)
    };
    let stored = fx.upload(&task, "a.bin", vec![1; 1000 * 1024]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let options: Vec<String> = (0..200).map(|n| format!("{n:0>200}")).collect();
    let field = json!({"key": "pick", "label": "Pick", "kind": "select", "options": options});
    let refused = fx
        .member("POST", &format!("/api/boards/{board}/fields"), Some(field))
        .await;
    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    let columns = fx.db.list_columns(&board).expect("columns");
    let described = json!({"description": "d".repeat(60 * 1024)});
    let refused = fx
        .member(
            "PATCH",
            &format!("/api/columns/{}", columns[0].id),
            Some(described),
        )
        .await;
    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    let config = json!({"legend_visible": false});
    let small = fx
        .member("PUT", &format!("/api/boards/{board}/config"), Some(config))
        .await;
    assert_eq!(small.status, StatusCode::OK, "{}", small.body);
}

#[tokio::test]
async fn quota_should_refuse_a_swimlane_edit_past_the_stored_limit_and_keep_the_row() {
    const LIMIT_MIB: u32 = 1;
    const FILLER_UPLOADS: usize = 40;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    let (board, column) = fx.board(&fx.member, "Board");
    let task = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let lane = fx.db.create_swimlane(&board, "Lane").expect("lane");
    let stored = fx.upload(&task, "a.bin", vec![1; 1000 * 1024]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    for n in 0..FILLER_UPLOADS {
        let filler = fx
            .upload(&task, &format!("f{n}.bin"), vec![n as u8 + 2; 1024])
            .await;
        if filler.status == StatusCode::CONFLICT {
            break;
        }
        assert_eq!(filler.status, StatusCode::OK, "{}", filler.body);
    }
    let uri = format!("/api/swimlanes/{lane}");
    let described = json!({"name": "😀".repeat(200), "description": "😀".repeat(500)});
    let refused = fx.member("PATCH", &uri, Some(described)).await;
    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    let row = fx
        .db
        .list_swimlanes(&board)
        .expect("lanes")
        .into_iter()
        .find(|row| row.id == lane)
        .expect("lane");
    assert_eq!(row.description, "");
    assert_eq!(row.name, "Lane");
}

#[tokio::test]
async fn saved_templates_should_stop_at_their_row_cap_with_a_409() {
    const DESCRIPTION_BYTES: usize = 65_536;
    const TASKS_PAST_THE_CAP: usize = 32;
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Board");
    let description = "d".repeat(DESCRIPTION_BYTES);
    for n in 0..MAX_TEMPLATE_ROW_BYTES / DESCRIPTION_BYTES + TASKS_PAST_THE_CAP {
        let task = fx
            .db
            .create_task(&column, None, &format!("Task {n}"), 1, None)
            .expect("task");
        fx.db
            .update_task(&task, None, Some(&description), None, None)
            .expect("description");
    }
    let uri = format!("/api/boards/{board}/save-as-template");
    let refused = fx.member("POST", &uri, Some(json!({"name": "Big"}))).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("quota_exceeded"));
    let message = refused.body["message"].as_str().expect("message");
    assert!(message.contains("bytes in one saved template"), "{message}");
    assert!(
        fx.db
            .list_templates(&fx.member)
            .expect("templates")
            .iter()
            .all(|t| t.name != "Big")
    );
}

#[tokio::test]
async fn timer_start_should_keep_the_running_session_once_a_mib_past_the_stored_limit() {
    const LIMIT_MIB: u32 = 1;
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let stored = fx
        .upload(&task, "a.bin", vec![1; 2 * MIB as usize + 1])
        .await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    fx.set_quotas(Quotas {
        stored_mib_per_account: LIMIT_MIB,
        ..Quotas::default()
    })
    .await;
    run_pomodoro(&fx, &task);
    let account = fx.db.account(&fx.member);
    let running = account.active_timer().expect("timer").expect("running");
    let refused = fx
        .member(
            "POST",
            "/api/timer/start",
            Some(json!({"mode": "pomodoro"})),
        )
        .await;
    assert_quota(&refused, Quota::StoredMibPerAccount, LIMIT_MIB);
    let kept = account
        .active_timer()
        .expect("timer")
        .expect("still running");
    assert_eq!(kept.started_at, running.started_at);
    assert!(fx.db.list_entries(&task).expect("entries").is_empty());
}

#[tokio::test(start_paused = true)]
async fn unread_board_pages_should_hold_their_slots_until_dropped() {
    let fx = Fixture::multi();
    let (member_board, _) = fx.board(&fx.member, "Member");
    let (admin_board, _) = fx.board(&fx.admin, "Admin");
    let mut others = Vec::new();
    for _ in 1..AppState::BOARDS_AT_ONCE {
        others.push(fx.state.boards.clone().acquire_owned().await.expect("slot"));
    }
    let unread = get_unread(&fx, &format!("/b/{member_board}"), &fx.member_bearer).await;
    assert_eq!(unread.status(), StatusCode::OK);

    let admin_page = format!("/b/{admin_board}");
    let busy = get_unread(&fx, &admin_page, &fx.admin_bearer).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&body_of(busy).await).expect("json");
    assert_eq!(body["code"], "busy");
    drop(unread);
    let served = get_unread(&fx, &admin_page, &fx.admin_bearer).await;
    assert_eq!(served.status(), StatusCode::OK);
    let page = String::from_utf8(body_of(served).await.to_vec()).expect("utf8");
    assert!(page.contains("Admin"), "the page renders");
}

#[tokio::test(start_paused = true)]
async fn searches_should_answer_busy_while_every_board_slot_is_taken() {
    let fx = Fixture::multi();
    fx.board(&fx.member, "Member");
    let uri = "/api/search?q=task";
    let served = get_unread(&fx, uri, &fx.member_bearer).await;
    assert_eq!(served.status(), StatusCode::OK);
    let mut held = Vec::new();
    for _ in 0..AppState::BOARDS_AT_ONCE {
        held.push(fx.state.boards.clone().acquire_owned().await.expect("slot"));
    }
    let busy = get_unread(&fx, uri, &fx.member_bearer).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&body_of(busy).await).expect("json");
    assert_eq!(body["code"], "busy");
    drop(held);
    let served = get_unread(&fx, uri, &fx.member_bearer).await;
    assert_eq!(served.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn board_exports_should_stop_sending_after_the_send_limit() {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Board");
    fx.db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let uri = format!("/api/boards/{board}/export-data");
    let response = fx
        .app
        .clone()
        .oneshot(bearer_get(&uri, &fx.member_bearer))
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let head = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
    assert!(matches!(head, Some(Ok(_))), "the head goes out");
    tokio::time::advance(Duration::from_secs(SEND_SECS + 1)).await;
    let late = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
    assert!(
        matches!(late, Some(Err(_))),
        "the send fails once the limit passes"
    );
}

#[tokio::test(start_paused = true)]
async fn board_archives_should_stop_sending_after_the_send_limit() {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Board");
    fx.db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let uri = format!("/api/boards/{board}/archive");
    let response = get_unread(&fx, &uri, &fx.member_bearer).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let first = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
    assert!(matches!(first, Some(Ok(_))), "the first chunk goes out");
    tokio::time::advance(Duration::from_secs(SEND_SECS + 1)).await;
    let late = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
    assert!(
        matches!(late, Some(Err(_))),
        "the send fails once the limit passes"
    );
}

#[tokio::test(start_paused = true)]
async fn board_archives_should_hold_the_whole_board_slot_until_sent() {
    let fx = Fixture::multi();
    let (board, _) = fx.board(&fx.member, "Board");
    let uri = format!("/api/boards/{board}/archive");
    let held = get_unread(&fx, &uri, &fx.member_bearer).await;
    assert_eq!(held.status(), StatusCode::OK);

    let second = get_unread(&fx, &uri, &fx.member_bearer).await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&body_of(second).await).expect("json");
    assert_eq!(body["code"], "busy");

    drop(held);
    let again = get_unread(&fx, &uri, &fx.member_bearer).await;
    assert_eq!(again.status(), StatusCode::OK);
}

#[tokio::test]
async fn board_archives_should_download_as_a_gzipped_tar_named_for_the_board() {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Plans / 2026");
    fx.db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let uri = format!("/api/boards/{board}/archive");
    let response = get_unread(&fx, &uri, &fx.member_bearer).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "application/gzip");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"Plans _ 2026.tar.gz\""
    );
    let mut tar = Vec::new();
    GzDecoder::new(body_of(response).await.as_ref())
        .read_to_end(&mut tar)
        .expect("valid gzip");
    assert_eq!(&tar[..10], b"board.json");
    let size = std::str::from_utf8(&tar[124..135]).expect("utf8");
    let size = usize::from_str_radix(size, 8).expect("octal size");
    let document: Value = serde_json::from_slice(&tar[512..512 + size]).expect("board.json");
    assert_eq!(document["format"], "pomb-board-archive");
    assert_eq!(document["board"]["id"], board);
    assert_eq!(document["tasks"][0]["name"], "Task");
}

#[tokio::test]
async fn a_board_archive_should_name_no_task_outside_the_token_set() {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Granted");
    let (_, far_column) = fx.board(&fx.member, "Hidden board");
    let near = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    let far = fx
        .db
        .create_task(&far_column, None, "Hidden neighbour", 1, None)
        .expect("task");
    let owned = fx.db.account(&fx.member);
    let task = |id: &str| owned.task(id).expect("read").expect("task");
    owned
        .add_task_link(
            &task(&near),
            LinkKind::Related,
            &task(&far),
            None,
            Actor::SYSTEM,
        )
        .expect("link");
    let (mut record, raw) = new_api_token_record(&fx.member, "set", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([board.clone()]));
    fx.db.create_api_token(&record).expect("token");

    let uri = format!("/api/boards/{board}/archive");
    let response = get_unread(&fx, &uri, &format!("Bearer {raw}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut tar = Vec::new();
    GzDecoder::new(body_of(response).await.as_ref())
        .read_to_end(&mut tar)
        .expect("valid gzip");
    let text = String::from_utf8_lossy(&tar);
    assert!(!text.contains("Hidden neighbour"), "{text}");
    assert!(!text.contains("Hidden board"), "{text}");
    assert!(!text.contains("related"), "{text}");
}

/// Sends a GET of `uri` with `authorization` to `fx`'s router and returns the unread response.
async fn get_unread(fx: &Fixture, uri: &str, authorization: &str) -> Response {
    fx.app
        .clone()
        .oneshot(bearer_get(uri, authorization))
        .await
        .expect("oneshot")
}

#[tokio::test(start_paused = true)]
async fn board_pages_should_hold_one_slot_per_account() {
    let fx = Fixture::multi();
    let (member_board, _) = fx.board(&fx.member, "Member");
    let (admin_board, _) = fx.board(&fx.admin, "Admin");
    let member_page = format!("/b/{member_board}");
    let held = get_unread(&fx, &member_page, &fx.member_bearer).await;
    assert_eq!(held.status(), StatusCode::OK);

    let second = get_unread(&fx, &member_page, &fx.member_bearer).await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&body_of(second).await).expect("json");
    assert_eq!(body["code"], "busy");
    let other = get_unread(&fx, &format!("/b/{admin_board}"), &fx.admin_bearer).await;
    assert_eq!(
        other.status(),
        StatusCode::OK,
        "another account still gets a slot"
    );

    drop(held);
    let again = get_unread(&fx, &member_page, &fx.member_bearer).await;
    assert_eq!(again.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn calendar_feeds_should_hold_their_own_slot_apart_from_the_owner_board_pages() {
    const TOKEN: &str = "feed-token";
    let fx = Fixture::multi();
    let (board, _) = fx.board(&fx.member, "Board");
    fx.db
        .add_board_feed(&board, "Feed", TOKEN)
        .expect("feed token");
    let feed = format!("/api/calendar-feed/{TOKEN}.ics");
    let page = format!("/b/{board}");

    let unread_feed = get_public(&fx, &feed).await;
    assert_eq!(unread_feed.status(), StatusCode::OK);
    let busy = get_public(&fx, &feed).await;
    assert_eq!(
        busy.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "one feed of the owner's at once"
    );
    let unread_page = get_unread(&fx, &page, &fx.member_bearer).await;
    assert_eq!(
        unread_page.status(),
        StatusCode::OK,
        "a slow feed reader never holds the owner's board page"
    );
    drop(unread_feed);
    let served = get_public(&fx, &feed).await;
    assert_eq!(
        served.status(),
        StatusCode::OK,
        "the owner's open board page never holds the feed"
    );
    drop(unread_page);
}

#[tokio::test(start_paused = true)]
async fn calendar_feeds_should_take_a_board_slot_after_their_token_checks() {
    const TOKEN: &str = "feed-token";
    let fx = Fixture::multi();
    let (board, _) = fx.board(&fx.member, "Board");
    fx.db
        .add_board_feed(&board, "Feed", TOKEN)
        .expect("feed token");
    let mut full = Vec::new();
    for _ in 0..AppState::BOARDS_AT_ONCE {
        full.push(fx.state.boards.clone().acquire_owned().await.expect("slot"));
    }

    let unknown = call(&fx.app, "GET", "/api/calendar-feed/unknown.ics", &[], None).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    let feed = format!("/api/calendar-feed/{TOKEN}.ics");
    let busy = call(&fx.app, "GET", &feed, &[], None).await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.body
    );
    assert_eq!(busy.code.as_deref(), Some("busy"));

    drop(full.pop());
    let served = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&feed)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(served.status(), StatusCode::OK);
    let text = String::from_utf8(body_of(served).await.to_vec()).expect("utf8");
    assert!(text.starts_with("BEGIN:VCALENDAR"), "{text}");
}

#[tokio::test]
async fn board_pages_should_render_cards_from_their_slim_view() {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Board");
    fx.db.create_swimlane(&board, "Lane").expect("lane");
    let shown = r#"{"prop_labels":"show","prop_subtasks":"show","prop_due_dates":"show"}"#;
    let configured = fx
        .member(
            "PATCH",
            &format!("/api/columns/{column}"),
            Some(json!({"config_json": shown, "wip_limit": 1})),
        )
        .await;
    assert!(configured.status.is_success(), "{}", configured.body);
    let description = format!(
        "start {}TAIL",
        "x".repeat(TaskCard::DESCRIPTION_PREVIEW_CHARS)
    );
    let first = fx
        .member(
            "POST",
            "/api/tasks",
            Some(json!({
                "column_id": column, "name": "First card", "description": description,
                "labels": ["urgent"], "due_at": "2030-01-01",
            })),
        )
        .await;
    assert!(first.status.is_success(), "{}", first.body);
    let task = first.body["id"].as_str().expect("id").to_string();
    let second = fx
        .member(
            "POST",
            "/api/tasks",
            Some(json!({"column_id": column, "name": "Second card", "force": true})),
        )
        .await;
    assert!(second.status.is_success(), "{}", second.body);
    let subtasks = format!("/api/tasks/{task}/subtasks");
    let step = fx
        .member("POST", &subtasks, Some(json!({"name": "Step one"})))
        .await;
    let step = step.body["id"].as_str().expect("subtask id").to_string();
    fx.member("POST", &subtasks, Some(json!({"name": "Step two"})))
        .await;
    fx.member(
        "PATCH",
        &format!("{subtasks}/{step}"),
        Some(json!({"done": true})),
    )
    .await;
    fx.member(
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"body": "hello"})),
    )
    .await;
    let claimed = fx
        .member(
            "POST",
            &format!("/api/tasks/{task}/claim"),
            Some(json!({"claimant": "agent", "ttl_seconds": 600, "role": "builder"})),
        )
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.body);
    let color = fx
        .db
        .list_colors(&board)
        .expect("colors")
        .into_iter()
        .find(|color| !color.is_default && color.enabled)
        .expect("a color");
    let colored = fx
        .member(
            "PATCH",
            &format!("/api/tasks/{task}"),
            Some(json!({"color_id": color.id})),
        )
        .await;
    assert!(colored.status.is_success(), "{}", colored.body);

    let page = get_unread(&fx, &format!("/b/{board}"), &fx.member_bearer).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = String::from_utf8(body_of(page).await.to_vec()).expect("utf8");
    for expected in [
        "First card".to_string(),
        "Second card".to_string(),
        r#"<span class="card-label" title="Label">urgent</span>"#.to_string(),
        format!(r#"data-color-value="{}""#, color.value),
        "Due: <strong>".to_string(),
        r#"data-subtasks-total="2" data-subtasks-done="1""#.to_string(),
        "Step two".to_string(),
        "Has comments".to_string(),
        r#"<span class="card-progress-fill" style="width:50%"></span>"#.to_string(),
        r#"<span class="card-progress-text">1 of 2</span>"#.to_string(),
        r#"class="card-circle card-circle-owner card-circle-agent""#.to_string(),
        r#"data-task-count="2""#.to_string(),
        "columnHeader--limitWarning".to_string(),
    ] {
        assert!(page.contains(&expected), "the page lacks {expected}");
    }
    assert_eq!(
        page.matches("card-progress\"").count(),
        1,
        "only the card with subtasks shows progress"
    );
    assert!(
        !page.contains("start x"),
        "a card shows no description head"
    );
    assert!(!page.contains("TAIL"), "a card shows no description tail");
}

#[tokio::test(start_paused = true)]
async fn board_pages_should_floor_the_subtask_progress_width() {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Board");
    fx.db.create_swimlane(&board, "Lane").expect("lane");
    let shown = r#"{"prop_subtasks":"show"}"#;
    let configured = fx
        .member(
            "PATCH",
            &format!("/api/columns/{column}"),
            Some(json!({"config_json": shown})),
        )
        .await;
    assert!(configured.status.is_success(), "{}", configured.body);
    for (name, done) in [("Two of three", 2), ("One of three", 1)] {
        let created = fx
            .member(
                "POST",
                "/api/tasks",
                Some(json!({"column_id": column, "name": name})),
            )
            .await;
        assert!(created.status.is_success(), "{}", created.body);
        let task = created.body["id"].as_str().expect("id").to_string();
        let subtasks = format!("/api/tasks/{task}/subtasks");
        for step in 0..3 {
            let added = fx
                .member(
                    "POST",
                    &subtasks,
                    Some(json!({"name": format!("Step {step}")})),
                )
                .await;
            let step_id = added.body["id"].as_str().expect("subtask id").to_string();
            if step < done {
                let marked = fx
                    .member(
                        "PATCH",
                        &format!("{subtasks}/{step_id}"),
                        Some(json!({"done": true})),
                    )
                    .await;
                assert!(marked.status.is_success(), "{}", marked.body);
            }
        }
    }

    let page = get_unread(&fx, &format!("/b/{board}"), &fx.member_bearer).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = String::from_utf8(body_of(page).await.to_vec()).expect("utf8");
    for expected in [
        r#"<span class="card-progress-fill" style="width:66%"></span>"#,
        r#"<span class="card-progress-text">2 of 3</span>"#,
        r#"<span class="card-progress-fill" style="width:33%"></span>"#,
        r#"<span class="card-progress-text">1 of 3</span>"#,
    ] {
        assert!(page.contains(expected), "the page lacks {expected}");
    }
}

async fn card_face_page(config_json: Option<&str>, description: &str) -> String {
    let fx = Fixture::multi();
    let (board, column) = fx.board(&fx.member, "Board");
    fx.db.create_swimlane(&board, "Lane").expect("lane");
    if let Some(config_json) = config_json {
        let configured = fx
            .member(
                "PATCH",
                &format!("/api/columns/{column}"),
                Some(json!({"config_json": config_json})),
            )
            .await;
        assert!(configured.status.is_success(), "{}", configured.body);
    }
    let created = fx
        .member(
            "POST",
            "/api/tasks",
            Some(json!({"column_id": column, "name": "Face card", "description": description})),
        )
        .await;
    assert!(created.status.is_success(), "{}", created.body);

    let page = get_unread(&fx, &format!("/b/{board}"), &fx.member_bearer).await;
    assert_eq!(page.status(), StatusCode::OK);
    String::from_utf8(body_of(page).await.to_vec()).expect("utf8")
}

#[tokio::test(start_paused = true)]
async fn board_pages_should_hide_the_description_without_a_stored_choice() {
    let page = card_face_page(None, "**bold** words").await;
    assert!(!page.contains("card-desc tm-md\""), "no description block");
    assert!(!page.contains("<strong>bold</strong>"), "no rendered body");
    let hidden = card_face_page(Some(r#"{"prop_description":"hide"}"#), "**bold** words").await;
    assert!(
        !hidden.contains("card-desc tm-md\""),
        "an explicit hide shows none"
    );
}

#[tokio::test(start_paused = true)]
async fn board_pages_should_render_a_shown_description_as_markdown() {
    let page = card_face_page(
        Some(r#"{"prop_description":"show"}"#),
        "**bold** words\n\n- [x] done",
    )
    .await;
    assert!(
        page.contains(r#"<div class="card-desc tm-md">"#),
        "the block"
    );
    assert!(page.contains("<strong>bold</strong>"), "markdown renders");
    assert!(
        page.contains(r#"<input type="checkbox" disabled checked data-checklist-index="0">"#),
        "the boxes are disabled"
    );
    let empty = card_face_page(Some(r#"{"prop_description":"show"}"#), "").await;
    assert!(
        !empty.contains("card-desc tm-md\""),
        "an empty description shows none"
    );
}

#[tokio::test(start_paused = true)]
async fn board_pages_should_sanitize_a_description_on_the_card_face() {
    let page = card_face_page(
        Some(r#"{"prop_description":"show"}"#),
        "<script>alert(1)</script> [x](javascript:alert(1)) <img src=x onerror=alert(2)>",
    )
    .await;
    assert!(page.contains("card-desc tm-md"), "the block shows");
    assert!(!page.contains("<script>alert"), "script is removed");
    assert!(
        !page.contains(r#"href="javascript:"#),
        "script urls are removed"
    );
    assert!(!page.contains("onerror=alert"), "handlers are removed");
}

#[tokio::test(start_paused = true)]
async fn board_pages_should_cut_a_long_description_on_the_card_face() {
    let long = format!("{}\n{}TAIL", "é".repeat(200), "x".repeat(900));
    let page = card_face_page(Some(r#"{"prop_description":"show"}"#), &long).await;
    assert!(page.contains("card-desc tm-md"), "the block shows");
    assert!(page.contains(&"é".repeat(10)), "the head shows");
    assert!(!page.contains("TAIL"), "the tail is cut");
}

async fn card_face_fragment(description: &str) -> String {
    let fx = Fixture::multi();
    let (_, column) = fx.board(&fx.member, "Board");
    let configured = fx
        .member(
            "PATCH",
            &format!("/api/columns/{column}"),
            Some(json!({"config_json": r#"{"prop_description":"show"}"#})),
        )
        .await;
    assert!(configured.status.is_success(), "{}", configured.body);
    let created = fx
        .member(
            "POST",
            "/api/tasks",
            Some(json!({"column_id": column, "name": "Face card", "description": description})),
        )
        .await;
    assert!(created.status.is_success(), "{}", created.body);
    let task = created.body["id"].as_str().expect("id").to_string();

    let card = get_unread(&fx, &format!("/api/tasks/{task}/card"), &fx.member_bearer).await;
    assert_eq!(card.status(), StatusCode::OK);
    String::from_utf8(body_of(card).await.to_vec()).expect("utf8")
}

#[tokio::test(start_paused = true)]
async fn card_fragments_should_cut_a_multibyte_description_at_a_character() {
    let long = format!("{}é{}TAIL", "x".repeat(599), "y".repeat(400));
    let card = card_face_fragment(&long).await;
    assert!(card.contains(r#"<div class="card-desc tm-md">"#), "{card}");
    assert!(card.contains(&"x".repeat(599)), "the head shows");
    assert!(!card.contains('é'), "the character over the cap is dropped");
    assert!(!card.contains("TAIL"), "the tail is cut");
    assert!(!card.contains('\u{fffd}'), "no broken character");
}

#[tokio::test(start_paused = true)]
async fn card_fragments_should_keep_the_last_line_of_a_cut_on_a_line_boundary() {
    let long = format!("{}\n{}\n{}", "a".repeat(299), "b".repeat(300), "TAIL");
    let card = card_face_fragment(&long).await;
    assert!(card.contains(&"b".repeat(300)), "the last whole line stays");
    assert!(!card.contains("TAIL"), "the tail is cut");
}

#[tokio::test(start_paused = true)]
async fn card_faces_should_show_no_block_for_a_blank_description() {
    let page = card_face_page(Some(r#"{"prop_description":"show"}"#), " \n\t\n  ").await;
    assert!(
        !page.contains("card-desc tm-md\""),
        "the board page has no block"
    );
    let card = card_face_fragment("   \n\n").await;
    assert!(!card.contains("card-desc tm-md\""), "the card has no block");
    let blank_head = format!("{}x", " \n".repeat(400));
    let card = card_face_fragment(&blank_head).await;
    assert!(
        !card.contains("card-desc tm-md\""),
        "a blank head has no block"
    );
}

/// Turns on the calendar feed of `board` with the session `cookie` and returns its path.
async fn feed_path(fx: &Fixture, board: &str, cookie: &str) -> String {
    let session = [("cookie", cookie)];
    let uri = format!("/api/boards/{board}/calendar-feeds");
    let link = call(
        &fx.app,
        "POST",
        &uri,
        &session,
        Some(json!({"label": "Feed"})),
    )
    .await;
    assert_eq!(link.status, StatusCode::OK, "{}", link.body);
    link.body["path"].as_str().expect("feed path").to_string()
}

/// Sends a GET of `uri` without credentials to `fx`'s router and returns the unread response.
async fn get_public(fx: &Fixture, uri: &str) -> Response {
    fx.app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot")
}

#[tokio::test(start_paused = true)]
async fn calendar_feeds_should_take_their_board_owners_feed_slot() {
    let fx = Fixture::multi();
    let (member_board, _) = fx.board(&fx.member, "Member");
    let (admin_board, _) = fx.board(&fx.admin, "Admin");
    let member_feed = feed_path(&fx, &member_board, &fx.member_cookie).await;
    let admin_feed = feed_path(&fx, &admin_board, &fx.admin_cookie).await;
    let held = get_public(&fx, &member_feed).await;
    assert_eq!(held.status(), StatusCode::OK);

    let busy = get_public(&fx, &member_feed).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let text = String::from_utf8(body_of(busy).await.to_vec()).expect("utf8");
    assert!(text.contains("whole-board reads"), "{text}");
    let other = get_public(&fx, &admin_feed).await;
    assert_eq!(
        other.status(),
        StatusCode::OK,
        "another owner's feed still gets a slot"
    );
}

#[tokio::test(start_paused = true)]
async fn calendar_feeds_should_refuse_a_token_deleted_while_they_wait() {
    let fx = Fixture::multi();
    let (board, _) = fx.board(&fx.member, "Board");
    let session = [("cookie", fx.member_cookie.as_str())];
    let uri = format!("/api/boards/{board}/calendar-feeds");
    let created = call(
        &fx.app,
        "POST",
        &uri,
        &session,
        Some(json!({"label": "Feed"})),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let feed = created.body["path"]
        .as_str()
        .expect("feed path")
        .to_string();
    let feed_id = created.body["id"].as_str().expect("feed id").to_string();
    let held = get_public(&fx, &feed).await;
    assert_eq!(held.status(), StatusCode::OK);
    let app = fx.app.clone();
    let target = feed.clone();
    let waiting = tokio::spawn(async move {
        app.oneshot(
            Request::builder()
                .uri(&target)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot")
    });
    settle().await;

    let revoke = format!("{uri}/{feed_id}");
    let revoked = call(&fx.app, "DELETE", &revoke, &session, None).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    drop(held);
    let reply = waiting.await.expect("feed request");
    assert_eq!(reply.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn inspect_should_refuse_a_token_of_another_account() {
    let fx = Fixture::multi();
    let raw = fx.member_bearer.trim_start_matches("Bearer ");
    let inspect = |cookie: String, token: String| {
        let app = fx.app.clone();
        async move {
            let session = [("cookie", cookie.as_str())];
            let body = json!({ "token": token });
            call(
                &app,
                "POST",
                "/api/v1/auth/tokens/inspect",
                &session,
                Some(body),
            )
            .await
        }
    };

    let own = inspect(fx.member_cookie.clone(), raw.to_string()).await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);

    let foreign = inspect(fx.admin_cookie.clone(), raw.to_string()).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
    let unknown = inspect(fx.admin_cookie.clone(), "pomb_no_such_token".to_string()).await;
    assert_eq!(foreign.status, unknown.status);
    assert_eq!(foreign.body, unknown.body);
}

#[tokio::test]
async fn token_create_should_refuse_a_board_of_another_account() {
    let fx = Fixture::multi();
    let (foreign, _) = fx.board(&fx.admin, "Theirs");
    let session = [("cookie", fx.member_cookie.as_str())];
    let mint = |boards: Value| {
        let body = json!({"name": "reach", "preset": "read", "boards": boards});
        call(&fx.app, "POST", "/api/v1/auth/tokens", &session, Some(body))
    };

    let refused = mint(json!([foreign])).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert!(
        !refused.body.to_string().contains("Theirs"),
        "{}",
        refused.body
    );
    let unknown = mint(json!(["no-such-board"])).await;
    assert_eq!(unknown.status, refused.status);
}

struct Convertible {
    board: String,
    column: String,
    other_column: String,
    task: String,
    subtask: String,
}

impl Fixture {
    async fn convertible(&self) -> Convertible {
        let (board, column) = self.board(&self.member, "Board");
        let other_column = self
            .db
            .create_column(&board, "Doing", None)
            .expect("column");
        let task = self
            .db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        let subtask = self.add_subtask(&task, "Step").await;
        Convertible {
            board,
            column,
            other_column,
            task,
            subtask,
        }
    }

    async fn convert(&self, setup: &Convertible, body: Value) -> Reply {
        let uri = format!(
            "/api/tasks/{}/subtasks/{}/convert",
            setup.task, setup.subtask
        );
        self.member("POST", &uri, Some(body)).await
    }

    async fn convert_with(&self, setup: &Convertible, relation: &str) -> String {
        let reply = self.convert(setup, json!({"relation": relation})).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["id"].as_str().expect("id").to_string()
    }
}

impl Fixture {
    async fn assigned_subtask(&self) -> (String, String, Reply) {
        let (_, task) = self.member_task();
        let sub = self.add_subtask(&task, "Step").await;
        let uri = format!("/api/tasks/{task}/subtasks/{sub}");
        let body = json!({"member_ids": [self.member.as_str()]});
        let reply = self.member("PATCH", &uri, Some(body)).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        (task, sub, reply)
    }
}

#[tokio::test]
async fn update_subtask_should_return_the_members() {
    let fx = Fixture::multi();
    let (_, _, updated) = fx.assigned_subtask().await;
    assert_eq!(updated.body["member_ids"], json!([fx.member.as_str()]));
}

#[tokio::test]
async fn update_subtask_should_store_the_members() {
    let fx = Fixture::multi();
    let (task, _, _) = fx.assigned_subtask().await;
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(stored.subtasks[0].member_ids, [fx.member.as_str()]);
}

#[tokio::test]
async fn update_subtask_should_record_a_members_event() {
    let fx = Fixture::multi();
    let (task, _, _) = fx.assigned_subtask().await;
    assert_eq!(
        history_details(&fx, &task, HistoryKind::SubtaskMembersUpdated).len(),
        1
    );
}

#[tokio::test]
async fn update_subtask_should_refuse_an_unknown_member() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let sub = fx.add_subtask(&task, "Step").await;
    let uri = format!("/api/tasks/{task}/subtasks/{sub}");

    let refused = fx
        .member(
            "PATCH",
            &uri,
            Some(json!({"member_ids": [fx.admin.as_str()]})),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let stored = fx.db.task(&task).expect("read").expect("task");
    assert!(stored.subtasks[0].member_ids.is_empty());
}

impl Fixture {
    async fn convert_styled(&self, setup: &Convertible) -> (String, Reply) {
        let colors = self.db.list_colors(&setup.board).expect("colors");
        let color = colors[1].id.clone();
        let body = json!({
            "color_id": color,
            "member_ids": [self.member.as_str()],
            "labels": ["bug", "ui"],
            "relation": "none",
        });
        let converted = self.convert(setup, body).await;
        assert_eq!(converted.status, StatusCode::CREATED, "{}", converted.body);
        (color, converted)
    }
}

#[tokio::test]
async fn convert_subtask_should_name_the_task_after_the_subtask() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let (_, converted) = fx.convert_styled(&setup).await;
    assert_eq!(converted.body["name"], "Step");
}

#[tokio::test]
async fn convert_subtask_should_take_the_chosen_color() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let (color, converted) = fx.convert_styled(&setup).await;
    assert_eq!(converted.body["color_id"], color);
}

#[tokio::test]
async fn convert_subtask_should_take_the_chosen_members() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let (_, converted) = fx.convert_styled(&setup).await;
    assert_eq!(converted.body["member_ids"], json!([fx.member.as_str()]));
}

#[tokio::test]
async fn convert_subtask_should_take_the_chosen_labels() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let (_, converted) = fx.convert_styled(&setup).await;
    assert_eq!(converted.body["labels"], json!(["bug", "ui"]));
}

#[tokio::test]
async fn convert_subtask_should_put_the_task_in_the_target_column() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let body = json!({"column_id": setup.other_column});

    let converted = fx.convert(&setup, body).await;
    assert_eq!(converted.status, StatusCode::CREATED, "{}", converted.body);
    assert_eq!(converted.body["column_id"], setup.other_column);
}

#[tokio::test]
async fn convert_subtask_should_default_to_the_column_of_the_parent() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    let converted = fx.convert(&setup, json!({})).await;
    assert_eq!(converted.status, StatusCode::CREATED, "{}", converted.body);
    assert_eq!(converted.body["column_id"], setup.column);
}

#[tokio::test]
async fn convert_subtask_should_carry_the_due_date_of_the_subtask() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let uri = format!("/api/tasks/{}/subtasks/{}", setup.task, setup.subtask);
    let dated = fx
        .member(
            "PATCH",
            &uri,
            Some(json!({"due_at": "2030-05-01T10:00:00Z"})),
        )
        .await;
    assert_eq!(dated.status, StatusCode::OK, "{}", dated.body);

    let converted = fx.convert(&setup, json!({})).await;
    assert_eq!(converted.status, StatusCode::CREATED, "{}", converted.body);
    assert_eq!(converted.body["due_at"], dated.body["due_at"]);
}

#[tokio::test]
async fn convert_subtask_should_remove_the_subtask_from_the_parent() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    fx.convert_with(&setup, "none").await;
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert!(parent.subtasks.is_empty());
}

#[tokio::test]
async fn convert_subtask_should_record_a_converted_event() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    fx.convert_with(&setup, "none").await;
    assert_eq!(
        history_details(&fx, &setup.task, HistoryKind::SubtaskConverted),
        ["Converted subtask \"Step\" to a task"]
    );
}

#[tokio::test]
async fn convert_subtask_should_link_nothing_for_the_none_relation() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    fx.convert_with(&setup, "none").await;
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert!(parent.links.is_empty());
}

#[tokio::test]
async fn convert_subtask_should_relate_the_new_task_to_the_parent() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    let new_id = fx.convert_with(&setup, "related").await;
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert_eq!(parent.links.len(), 1);
    assert_eq!(parent.links[0].kind, LinkKind::Related);
    assert_eq!(parent.links[0].target_id, new_id);
}

#[tokio::test]
async fn convert_subtask_should_block_the_parent_by_the_new_task() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    let new_id = fx.convert_with(&setup, "parent_depends_on_new").await;
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert_eq!(parent.links.len(), 1);
    assert_eq!(parent.links[0].kind, LinkKind::BlockedBy);
    assert_eq!(parent.links[0].target_id, new_id);
}

#[tokio::test]
async fn convert_subtask_should_block_the_new_task_by_the_parent() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    let new_id = fx.convert_with(&setup, "parent_required_by_new").await;
    let created = fx.db.task(&new_id).expect("read").expect("task");
    assert_eq!(created.links.len(), 1);
    assert_eq!(created.links[0].kind, LinkKind::BlockedBy);
    assert_eq!(created.links[0].target_id, setup.task);
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert!(parent.links.is_empty());
}

#[tokio::test]
async fn convert_subtask_should_refuse_a_subtask_of_another_task() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let other = fx
        .db
        .create_task(&setup.column, None, "Other", 1, None)
        .expect("task");
    let foreign = fx.add_subtask(&other, "Theirs").await;
    let uri = format!("/api/tasks/{}/subtasks/{foreign}/convert", setup.task);

    let refused = fx.member("POST", &uri, Some(json!({}))).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.body);
    let held = fx.db.task(&other).expect("read").expect("task");
    assert_eq!(held.subtasks.len(), 1);
    assert_eq!(fx.db.board_tasks(&setup.board).expect("tasks").len(), 2);
}

#[tokio::test]
async fn convert_subtask_should_refuse_a_column_of_another_account() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let (_, theirs) = fx.board(&fx.admin, "Theirs");

    let refused = fx.convert(&setup, json!({"column_id": theirs})).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.body);
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert_eq!(parent.subtasks.len(), 1);
}

#[tokio::test]
async fn convert_subtask_should_refuse_an_unknown_member() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;

    let refused = fx
        .convert(&setup, json!({"member_ids": [fx.admin.as_str()]}))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert_eq!(parent.subtasks.len(), 1);
}

#[tokio::test]
async fn convert_subtask_should_respect_the_task_quota() {
    const LIMIT: u32 = 1;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        tasks_per_board: LIMIT,
        ..Quotas::default()
    })
    .await;
    let setup = fx.convertible().await;

    let refused = fx.convert(&setup, json!({})).await;
    assert_quota(&refused, Quota::TasksPerBoard, LIMIT);
    let parent = fx.db.task(&setup.task).expect("read").expect("task");
    assert_eq!(parent.subtasks.len(), 1);
    assert_eq!(fx.db.board_tasks(&setup.board).expect("tasks").len(), 1);
}

impl Fixture {
    fn token_lacking(&self, lacking: &[Permission]) -> String {
        let (mut record, raw) =
            new_api_token_record(&self.member, "limited", AuthScope::Admin, None);
        for permission in lacking {
            record.permissions.remove(permission);
        }
        self.db.create_api_token(&record).expect("token");
        format!("Bearer {raw}")
    }

    async fn with_token(&self, authorization: &str, method: &str, uri: &str, body: Value) -> Reply {
        let auth = [("authorization", authorization)];
        call(&self.app, method, uri, &auth, Some(body)).await
    }

    async fn convert_as(&self, authorization: &str, setup: &Convertible, body: Value) -> Reply {
        let uri = format!(
            "/api/tasks/{}/subtasks/{}/convert",
            setup.task, setup.subtask
        );
        self.with_token(authorization, "POST", &uri, body).await
    }

    fn assert_unconverted(&self, setup: &Convertible) {
        let parent = self.db.task(&setup.task).expect("read").expect("task");
        assert_eq!(parent.subtasks.len(), 1);
        assert_eq!(self.db.board_tasks(&setup.board).expect("tasks").len(), 1);
    }
}

#[tokio::test]
async fn convert_subtask_should_refuse_a_token_without_subtasks_edit() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let token = fx.token_lacking(&[Permission::SubtasksEdit]);

    let refused = fx.convert_as(&token, &setup, json!({})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("permission_denied"));
    fx.assert_unconverted(&setup);
}

#[tokio::test]
async fn convert_subtask_should_refuse_a_relation_to_a_token_without_links_write() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let token = fx.token_lacking(&[Permission::LinksWrite]);

    for relation in ["related", "parent_depends_on_new", "parent_required_by_new"] {
        let refused = fx
            .convert_as(&token, &setup, json!({"relation": relation}))
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
        assert_eq!(refused.code.as_deref(), Some("permission_denied"));
    }
    fx.assert_unconverted(&setup);
}

#[tokio::test]
async fn convert_subtask_should_accept_no_relation_from_a_token_without_links_write() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let token = fx.token_lacking(&[Permission::LinksWrite]);

    let converted = fx
        .convert_as(&token, &setup, json!({"relation": "none"}))
        .await;
    assert_eq!(converted.status, StatusCode::CREATED, "{}", converted.body);
}

#[tokio::test]
async fn convert_subtask_should_refuse_a_token_without_subtasks_delete() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let token = fx.token_lacking(&[Permission::SubtasksDelete]);

    let refused = fx.convert_as(&token, &setup, json!({})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("permission_denied"));
    fx.assert_unconverted(&setup);
}

#[tokio::test]
async fn convert_subtask_should_refuse_a_subtask_the_token_made_without_subtasks_delete() {
    let fx = Fixture::multi();
    let setup = fx.convertible().await;
    let token = fx.token_lacking(&[Permission::SubtasksDelete]);
    let uri = format!("/api/tasks/{}/subtasks", setup.task);
    let made = fx
        .with_token(&token, "POST", &uri, json!({"name": "Mine"}))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let own = Convertible {
        subtask: made.body["id"].as_str().expect("id").to_string(),
        ..setup
    };

    let refused = fx.convert_as(&token, &own, json!({})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("permission_denied"));
    let parent = fx.db.task(&own.task).expect("read").expect("task");
    assert_eq!(parent.subtasks.len(), 2);
}

impl Fixture {
    /// Stores a task of the member with the id `id` in a fresh board, returning the board.
    fn member_task_with_id(&self, id: &str) -> String {
        self.task_with_id(&self.member, id)
    }

    /// Stores a task of `owner` with the id `id` in a fresh board, returning the board.
    fn task_with_id(&self, owner: &AccountId, id: &str) -> String {
        let (board, column) = self.board(owner, "Linked");
        let mut row = Db::new_task_row(&column, None, "Linked", 1, None);
        row.id = id.to_string();
        self.db
            .insert_task(row, NewTaskPlacement::Bottom)
            .expect("task");
        board
    }
}

async fn task_link(fx: &Fixture, id: &str, authorization: &str) -> Response {
    get_unread(fx, &format!("/t/{id}"), authorization).await
}

#[tokio::test]
async fn task_link_should_redirect_to_the_board_with_the_card_open() {
    let fx = Fixture::multi();
    let id = "a1b2c3d4-0000-4000-8000-000000000001";
    let board = fx.member_task_with_id(id);
    let wanted = format!("/b/{board}#task-{id}");
    for given in ["a1b2c3", "a1b2c3d4", id] {
        let response = task_link(&fx, given, &fx.member_bearer).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{given}");
        let location = response.headers().get("location").expect("location");
        assert_eq!(location.to_str().expect("ascii"), wanted, "{given}");
    }
}

#[tokio::test]
async fn task_link_should_answer_one_404_for_every_id_it_cannot_open() {
    let fx = Fixture::multi();
    fx.member_task_with_id("a1b2c3d4-0000-4000-8000-000000000001");
    fx.member_task_with_id("a1b2c3d4-0000-4000-8000-000000000002");
    fx.member_task_with_id("ffeedd11-0000-4000-8000-000000000003");
    let ambiguous = task_link(&fx, "a1b2c3d4", &fx.member_bearer).await;
    assert_eq!(ambiguous.status(), StatusCode::NOT_FOUND);
    let missing = body_of(task_link(&fx, "012345", &fx.member_bearer).await).await;
    assert_eq!(body_of(ambiguous).await, missing);

    for rejected in ["ffeed", "ffeedd1!", "ffeedd11%2F"] {
        let response = task_link(&fx, rejected, &fx.member_bearer).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{rejected}");
        assert_eq!(body_of(response).await, missing, "{rejected}");
    }

    let foreign = task_link(&fx, "ffeedd11", &fx.admin_bearer).await;
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_of(foreign).await, missing);
}

#[tokio::test]
async fn task_link_should_ignore_matches_in_other_accounts() {
    let fx = Fixture::multi();
    let own = fx.member_task_with_id("bb000000-0000-4000-8000-000000000001");
    fx.task_with_id(&fx.admin, "bb000000-0000-4000-8000-000000000002");
    let response = task_link(&fx, "bb0000", &fx.member_bearer).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers().get("location").expect("location");
    assert_eq!(
        location.to_str().expect("ascii"),
        format!("/b/{own}#task-bb000000-0000-4000-8000-000000000001")
    );
}

#[tokio::test]
async fn task_responses_should_carry_a_url_that_opens_the_card() {
    let fx = Fixture::multi();
    let (_, task) = fx.member_task();
    let wanted = format!("/t/{task}");
    let detail = fx.member("GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(detail.body["url"], wanted, "{}", detail.body);
    let board = fx
        .db
        .task_board_id(&task)
        .expect("read")
        .expect("task board");
    let listed = fx.member("GET", "/api/tasks", None).await;
    assert_eq!(listed.body["items"][0]["url"], wanted, "{}", listed.body);
    let summaries = fx
        .member("GET", &format!("/api/boards/{board}/tasks"), None)
        .await;
    assert_eq!(
        summaries.body["items"][0]["url"], wanted,
        "{}",
        summaries.body
    );
}

#[tokio::test]
async fn task_link_should_refuse_boards_outside_the_token_set() {
    let fx = Fixture::multi();
    let inside = fx.member_task_with_id("cc000000-0000-4000-8000-000000000001");
    fx.member_task_with_id("cc000000-0000-4000-8000-000000000002");
    fx.member_task_with_id("dd000000-0000-4000-8000-000000000003");
    let (mut record, raw) = new_api_token_record(&fx.member, "narrow", AuthScope::Admin, None);
    record.boards = BoardAccess::Only(BTreeSet::from([inside.clone()]));
    fx.db.create_api_token(&record).expect("token");
    let bearer = format!("Bearer {raw}");

    let shared = task_link(&fx, "cc000000-0000-4000-8000-000000000001", &bearer).await;
    assert_eq!(shared.status(), StatusCode::SEE_OTHER);
    let location = shared.headers().get("location").expect("location");
    assert_eq!(
        location.to_str().expect("ascii"),
        format!("/b/{inside}#task-cc000000-0000-4000-8000-000000000001")
    );

    let denied = task_link(&fx, "dd0000", &bearer).await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let missing = body_of(task_link(&fx, "012345", &bearer).await).await;
    fx.task_with_id(&fx.member, "dd000000-0000-4000-8000-000000000004");
    let mixed = task_link(&fx, "dd0000", &bearer).await;
    assert_eq!(mixed.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_of(mixed).await, missing);
    let upper = task_link(&fx, "CC0000", &bearer).await;
    assert_eq!(upper.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_avatar_image_should_count_against_the_stored_bytes_quota() {
    const FILE_BYTES: usize = 1000 * 1024;
    let fx = Fixture::multi();
    fx.set_quotas(Quotas {
        stored_mib_per_account: 1,
        ..Quotas::default()
    })
    .await;
    let (_, task) = fx.member_task();
    let stored = fx.upload(&task, "a.bin", vec![1; FILE_BYTES]).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    let png = |len: usize| {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(len, 7);
        bytes
    };
    let put = |len: usize| {
        let auth = [
            ("authorization", fx.member_bearer.as_str()),
            ("content-type", "image/png"),
        ];
        let image = png(len);
        let app = fx.app.clone();
        async move { send_bytes(&app, "PUT", "/api/avatar", &auth, None, image).await }
    };
    let usage = || fx.db.stored_bytes_by_account().expect("usage")[fx.member.as_str()];

    let before = usage();
    let refused = put(40_000).await;
    assert_quota(&refused, Quota::StoredMibPerAccount, 1);
    assert_eq!(usage(), before, "a refused image counts nothing");

    let stored = put(10_000).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(usage(), before + 10_000);

    let replaced = put(12_000).await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.body);
    assert_eq!(usage(), before + 12_000, "a replaced image counts once");

    let text = fx
        .member(
            "PUT",
            "/api/settings",
            Some(json!({"avatar": {"text": "BO"}})),
        )
        .await;
    assert_eq!(text.status, StatusCode::OK, "{}", text.body);
    assert_eq!(usage(), before, "a deleted image frees its bytes");
}
