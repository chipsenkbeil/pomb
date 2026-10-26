use axum::Router;
use axum::http::StatusCode;
use chrono::Utc;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::multi_tests::admin_settings;
use super::passkey_tests::{Reply, call};
use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{
    AccountId, AuthScope, InstanceMode, InstanceSettings, PruneSettings, SECONDS_PER_DAY,
};

/// Seconds since its last use for a board still inside the hour that records one use.
const WITHIN_THE_HOUR_SECONDS: i64 = 600;
/// Seconds since its last use for a board past the hour that records one use.
const PAST_THE_HOUR_SECONDS: i64 = 3700;
/// Days without use for a board long past the default inactivity limit.
const LONG_UNUSED_DAYS: u32 = 365;
/// Warning days the settings test stores.
const SAVED_WARNING_DAYS: u32 = 7;

/// A multi-mode instance with its admin and a member, who has a session and a token.
struct Fixture {
    /// Keeps the database directory alive.
    _dir: TempDir,
    /// The database behind the router.
    db: Db,
    /// The router under test.
    app: Router,
    /// The member's account.
    member: AccountId,
    /// The admin's session cookie.
    admin_cookie: String,
    /// The member's session cookie.
    member_cookie: String,
    /// The member's `Authorization` header value.
    member_bearer: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prune.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let cookie = |account: &AccountId| {
            format!("session={}", db.create_session(account).expect("session"))
        };
        let (record, raw) = new_api_token_record(&member, "agent", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let state = AppState::new(db.clone(), None, passkey_mode())
            .with_mode(InstanceMode::Multi)
            .with_limits(unthrottled_limits());
        Self {
            admin_cookie: cookie(&admin),
            member_cookie: cookie(&member),
            member_bearer: format!("Bearer {raw}"),
            app: router(state),
            _dir: dir,
            db,
            member,
        }
    }

    /// Creates a board for the member last used at `last_used`, and returns its id.
    fn board_used_at(&self, name: &str, last_used: i64) -> String {
        let board = self.db.create_board(&self.member, name).expect("board");
        self.db.set_board_last_used(&board, last_used);
        board
    }

    /// Returns when board `board` was last used.
    fn last_used(&self, board: &str) -> i64 {
        self.db.boards_last_used().expect("used")[board]
    }

    /// Sends a request for `uri` in the member's browser session.
    async fn member(&self, uri: &str) -> Reply {
        let cookie = [("cookie", self.member_cookie.as_str())];
        call(&self.app, "GET", uri, &cookie, None).await
    }

    /// Opens `uri` in the member's browser session and returns the page.
    async fn page(&self, uri: &str) -> String {
        let reply = self.member(uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        reply.body.as_str().expect("html").to_string()
    }

    /// Stores the instance settings `body` with the admin's session.
    async fn put_settings(&self, body: Value) -> Reply {
        let cookie = [("cookie", self.admin_cookie.as_str())];
        call(
            &self.app,
            "PUT",
            "/api/v1/admin/settings",
            &cookie,
            Some(body),
        )
        .await
    }
}

fn now() -> i64 {
    Utc::now().timestamp()
}

fn days(n: u32) -> i64 {
    i64::from(n) * SECONDS_PER_DAY
}

#[tokio::test]
async fn board_activity_should_update_from_agent_tokens_and_throttle() {
    let fx = Fixture::new();
    let start = now();
    let recent = fx.board_used_at("Recent", start - WITHIN_THE_HOUR_SECONDS);
    let stale = fx.board_used_at("Stale", start - PAST_THE_HOUR_SECONDS);
    let column = fx.db.create_column(&stale, "Todo", None).expect("column");
    let task = fx
        .db
        .create_task(&column, None, "Task", 1, None)
        .expect("task");
    fx.db.touch_board(&recent, start - WITHIN_THE_HOUR_SECONDS);

    let auth = [("authorization", fx.member_bearer.as_str())];
    let board = call(
        &fx.app,
        "GET",
        &format!("/api/boards/{recent}"),
        &auth,
        None,
    )
    .await;
    assert_eq!(board.status, StatusCode::OK, "{}", board.body);
    let task = call(&fx.app, "GET", &format!("/api/tasks/{task}"), &auth, None).await;
    assert_eq!(task.status, StatusCode::OK, "{}", task.body);

    assert_eq!(
        fx.last_used(&recent),
        start - WITHIN_THE_HOUR_SECONDS,
        "a use within the hour is not recorded again"
    );
    assert!(
        fx.last_used(&stale) >= start,
        "a token resolving a task on the board records its use"
    );
}

#[tokio::test]
async fn calendar_feed_reads_should_count_as_use() {
    let fx = Fixture::new();
    let board = fx.board_used_at("Feed", now());
    let cookie = [("cookie", fx.member_cookie.as_str())];
    let uri = format!("/api/boards/{board}/calendar-feed");
    let feed = call(&fx.app, "POST", &uri, &cookie, None).await;
    assert_eq!(feed.status, StatusCode::OK, "{}", feed.body);
    let path = feed.body["path"].as_str().expect("feed path").to_string();
    let start = now();
    let long_ago = start - days(LONG_UNUSED_DAYS);
    fx.db.set_board_last_used(&board, long_ago);

    let read = call(&fx.app, "GET", &path, &[], None).await;
    assert_eq!(read.status, StatusCode::OK);
    assert!(fx.last_used(&board) >= start);
    assert_eq!(
        fx.db.stored_board_last_used(&board),
        Some(long_ago),
        "the read itself writes nothing"
    );
}

#[tokio::test]
async fn banner_should_show_in_warning_window_and_not_before() {
    let fx = Fixture::new();
    let rules = PruneSettings::default();
    let start = now();
    let window_days = rules.inactive_days - rules.warning_days;
    let warned = fx.board_used_at("Warned", start - days(window_days + 1));
    let quiet = fx.board_used_at("Quiet", start - days(window_days - 1));

    let page = fx.page(&format!("/b/{quiet}")).await;
    assert!(!page.contains("prune-banner"), "{page}");
    assert!(
        page.contains("bs-prune-due"),
        "the board list marks the board in its warning window"
    );

    let page = fx.page(&format!("/b/{warned}")).await;
    assert!(page.contains("prune-banner"), "{page}");
    assert!(page.contains(&format!(
        "went unused for {} days and was due to be deleted on or after",
        window_days + 1
    )));
    assert!(page.contains("Opening it kept it."));

    let page = fx.page(&format!("/b/{warned}")).await;
    assert!(
        !page.contains("prune-banner"),
        "the previous visit counted as a use"
    );
    assert!(!page.contains("bs-prune-due"));
}

#[tokio::test]
async fn prune_settings_should_reject_out_of_range_values() {
    let fx = Fixture::new();
    let valid = admin_settings("invite", InstanceSettings::DEFAULT_PENDING_CAP);
    let with = |key: &str, value: Value| {
        let mut body = valid.clone();
        body["prune"][key] = value;
        body
    };
    let without = |key: &str| {
        let mut body = valid.clone();
        body["prune"].as_object_mut().expect("object").remove(key);
        body
    };
    let mut no_prune = valid.clone();
    no_prune.as_object_mut().expect("object").remove("prune");
    let refused = [
        with("inactive_days", json!(PruneSettings::MIN_INACTIVE_DAYS - 1)),
        with("inactive_days", json!(PruneSettings::MAX_INACTIVE_DAYS + 1)),
        with("warning_days", json!(PruneSettings::MIN_WARNING_DAYS - 1)),
        with("warning_days", json!(PruneSettings::MAX_WARNING_DAYS + 1)),
        with("warning_days", json!(PruneSettings::DEFAULT_INACTIVE_DAYS)),
        with("extra", json!(1)),
        with("enabled", json!("yes")),
        without("enabled"),
        without("warning_days"),
        no_prune,
    ];
    for body in refused {
        let sent = fx.put_settings(body.clone()).await;
        assert_eq!(sent.status, StatusCode::BAD_REQUEST, "{body}");
    }
    let stored = fx.db.instance_settings().expect("read");
    assert_eq!(stored.prune, PruneSettings::default());
    assert_eq!(stored.prune_changed_at, None);

    let saved_settings = PruneSettings {
        enabled: false,
        inactive_days: PruneSettings::MIN_INACTIVE_DAYS,
        warning_days: SAVED_WARNING_DAYS,
    };
    let mut changed = valid.clone();
    changed["prune"] = json!(saved_settings);
    let before = now();
    let saved = fx.put_settings(changed).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["prune"], json!(saved_settings));
    let stored = fx.db.instance_settings().expect("read");
    assert_eq!(stored.prune, saved_settings);
    assert!(stored.prune_changed_at.is_some_and(|at| at >= before));
}
