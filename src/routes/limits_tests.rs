use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::passkey_tests::{Reply, call};
use super::server::{passkey_mode, unthrottled_limits};
use super::{ROUTE_ACCESS, Writes, router};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{Db, FencedWrite, NewComment};
use crate::disk::{DiskGuard, DiskProbe};
use crate::limit::Limits;
use crate::models::{
    AccountId, ActiveTimer, Actor, ActorKind, AuthScope, DiskLimits, InstanceMode,
    InstanceSettings, Quota, Quotas, RateLimits, TimerMode,
};

/// Bytes in one MiB.
const MIB: u64 = 1024 * 1024;
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
}

/// Returns a probe that measures a database of `database_mib` and `free_mib` free on every disk.
fn probe(database_mib: u64, free_mib: u64) -> DiskProbe {
    DiskProbe {
        stored: Arc::new(move || Ok(database_mib * MIB)),
        free: Arc::new(move |_| Ok(free_mib * MIB)),
    }
}

/// Returns a disk guard that measures through `probe`.
fn guard(probe: DiskProbe) -> DiskGuard {
    DiskGuard::new(probe, Path::new("."))
}

/// Returns a disk guard over a small database on a roomy disk.
fn roomy() -> DiskGuard {
    guard(probe(1, ROOMY_MIB))
}

/// An instance with its admin and a member, each with a session and an admin-scope token.
struct Fixture {
    /// Keeps the database directory alive.
    _dir: tempfile::TempDir,
    /// The database the router serves.
    db: Db,
    /// The router under test.
    app: Router,
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
            format!("session={}", db.create_session(account).expect("session"))
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
            app: router(state),
            _dir: dir,
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
    let named = format!("{} is {limit}", quota.as_str());
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
    let file = |name: &str| json!({"name": name, "data": "aGVsbG8="});
    let made = fx.member("POST", &uri, Some(file("a.txt"))).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let refused = fx.member("POST", &uri, Some(file("b.txt"))).await;
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
    let named = format!("{} is {LIMIT}", Quota::BoardsPerAccount.as_str());
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
async fn disk_guard_should_resume_writes_once_deletes_free_the_database() {
    const ATTACHMENT_MIB: usize = 9;
    let measured: Arc<Mutex<Option<DiskGuard>>> = Arc::new(Mutex::new(None));
    let kept = measured.clone();
    let fx = Fixture::new(InstanceMode::Multi, unthrottled_limits(), move |db| {
        let live = DiskProbe {
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
    for name in ["a.bin", "b.bin"] {
        fx.db
            .add_attachment(
                &task,
                name,
                "application/octet-stream",
                &vec![0; ATTACHMENT_MIB * MIB as usize],
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
