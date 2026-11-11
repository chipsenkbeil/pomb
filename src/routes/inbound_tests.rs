use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call, none_mode};
use crate::auth::{hash_inbound_address, new_api_token_record};
use crate::db::{
    AccountAction, AdminActionOutcome, Db, MAX_INBOUND_PER_BOARD, NewInbound, OwnerChange,
};
use crate::disk::{DiskGuard, DiskProbe};
use crate::limit::Limits;
use crate::models::{AccountId, AuthScope, BoardRole, HistoryKind, InstanceMode, TaskRow};
use crate::{AppState, BODY_LIMIT_BYTES};

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    admin: AccountId,
    token: String,
    work_token: String,
    board: String,
    column: String,
}

impl Fixture {
    fn new() -> Self {
        Self::with_limits(unthrottled_limits())
    }

    fn with_limits(limits: Limits) -> Self {
        Self::open(limits, None)
    }

    fn with_disk(disk: DiskGuard) -> Self {
        Self::open(unthrottled_limits(), Some(disk))
    }

    fn open(limits: Limits, disk: Option<DiskGuard>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("inbound.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "admin", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let (record, work_token) = new_api_token_record(&admin, "work", AuthScope::Work, None);
        db.create_api_token(&record).expect("token");
        let board = db.create_board(&admin, "Mail").expect("board");
        let column = db.create_column(&board, "Inbox", None).expect("column");
        let state = AppState::new(db.clone(), None, passkey_mode()).with_limits(limits);
        let state = match disk {
            Some(disk) => state.with_disk(disk),
            None => state,
        };
        let app = router(state);
        Self {
            _dir: dir,
            app,
            db,
            admin,
            token,
            work_token,
            board,
            column,
        }
    }

    async fn send(&self, token: &str, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {token}");
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    async fn admin(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        self.send(&self.token, method, uri, body).await
    }

    async fn create(&self, body: Value) -> Reply {
        let uri = format!("/api/boards/{}/inbound-addresses", self.board);
        self.admin("POST", &uri, Some(body)).await
    }

    async fn list(&self) -> Reply {
        let uri = format!("/api/boards/{}/inbound-addresses", self.board);
        self.admin("GET", &uri, None).await
    }

    async fn rotate(&self, id: &str) -> Reply {
        self.admin("POST", &format!("/api/inbound-addresses/{id}/rotate"), None)
            .await
    }

    fn address(&self, name: &str) -> Value {
        json!({"name": name, "column_id": self.column})
    }
}

fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

fn text<'a>(reply: &'a Reply, key: &str) -> &'a str {
    reply.body[key].as_str().expect(key)
}

#[tokio::test]
async fn address_should_be_created_listed_rotated_and_deleted() {
    let fx = Fixture::new();
    let created = fx
        .create(with(fx.address(" Support "), "prefix", json!("help")))
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let id = text(&created, "id").to_string();
    assert_eq!(created.body["name"], "Support");
    assert_eq!(created.body["prefix"], "help");
    assert_eq!(created.body["column_id"], json!(fx.column));
    assert_eq!(created.body["board_id"], json!(fx.board));
    let address = text(&created, "address").to_string();
    assert_eq!(text(&created, "path"), format!("/api/inbound/{address}"));

    let listed = fx.list().await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.body.as_array().expect("array").len(), 1);
    assert_eq!(listed.body[0]["id"], json!(id));

    let rotated = fx.rotate(&id).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    assert_eq!(rotated.body["id"], json!(id));
    assert_ne!(text(&rotated, "address"), address);

    let deleted = fx
        .admin("DELETE", &format!("/api/inbound-addresses/{id}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert!(fx.list().await.body.as_array().expect("array").is_empty());
    let again = fx
        .admin("DELETE", &format!("/api/inbound-addresses/{id}"), None)
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn address_should_hold_at_least_128_bits_of_randomness_after_its_prefix() {
    let fx = Fixture::new();
    let plain = fx.create(fx.address("Plain")).await;
    let plain = text(&plain, "address");
    assert!(plain.len() >= 32, "{plain}");
    assert!(plain.chars().all(|c| c.is_ascii_hexdigit()), "{plain}");
    let prefixed = fx
        .create(with(fx.address("Prefixed"), "prefix", json!("team-a")))
        .await;
    let prefixed = text(&prefixed, "address");
    let secret = prefixed.strip_prefix("team-a-").expect("prefix");
    assert!(secret.len() >= 32, "{secret}");
    assert!(secret.chars().all(|c| c.is_ascii_hexdigit()), "{secret}");
    let other = fx.create(fx.address("Other")).await;
    assert_ne!(text(&other, "address"), plain);
}

#[tokio::test]
async fn address_should_be_stored_only_as_a_hash() {
    let fx = Fixture::new();
    let created = fx.create(fx.address("Hashed")).await;
    let id = text(&created, "id");
    let address = text(&created, "address");
    let stored = fx.db.inbound_address(id).expect("read").expect("address");
    assert_eq!(stored.token_hash, hash_inbound_address(address));
    assert!(
        !serde_json::to_string(&stored)
            .expect("json")
            .contains(address)
    );
    assert!(!format!("{stored:?}").contains(&stored.token_hash));
    let found = fx.db.inbound_by_address(address).expect("lookup");
    assert_eq!(found.map(|row| row.id), Some(id.to_string()));
    assert!(fx.db.inbound_by_address("nope").expect("lookup").is_none());
}

#[tokio::test]
async fn address_should_appear_only_in_the_create_and_rotate_responses() {
    let fx = Fixture::new();
    let created = fx.create(fx.address("Once")).await;
    let id = text(&created, "id");
    let address = text(&created, "address");
    assert!(!fx.list().await.body.to_string().contains(address));
    let page = fx
        .admin(
            "GET",
            &format!("/b/{}/settings/add-from-email", fx.board),
            None,
        )
        .await;
    assert!(!page.body.to_string().contains(address));
    let rotated = fx.rotate(id).await;
    let fresh = text(&rotated, "address");
    assert!(!fx.list().await.body.to_string().contains(fresh));
}

#[tokio::test]
async fn rotated_address_should_stop_working_at_once() {
    let fx = Fixture::new();
    let created = fx.create(fx.address("Rotating")).await;
    let id = text(&created, "id").to_string();
    let old = text(&created, "address").to_string();
    assert!(fx.db.inbound_by_address(&old).expect("lookup").is_some());
    let rotated = fx.rotate(&id).await;
    let fresh = text(&rotated, "address").to_string();
    assert!(fx.db.inbound_by_address(&old).expect("lookup").is_none());
    let found = fx.db.inbound_by_address(&fresh).expect("lookup");
    assert_eq!(found.map(|row| row.id), Some(id));
}

#[tokio::test]
async fn rotating_should_keep_the_prefix() {
    let fx = Fixture::new();
    let created = fx
        .create(with(fx.address("Kept"), "prefix", json!("sales")))
        .await;
    let rotated = fx.rotate(text(&created, "id")).await;
    assert!(text(&rotated, "address").starts_with("sales-"));
    assert_eq!(rotated.body["prefix"], "sales");
}

#[tokio::test]
async fn deleted_address_should_stop_working() {
    let fx = Fixture::new();
    let created = fx.create(fx.address("Doomed")).await;
    let address = text(&created, "address").to_string();
    fx.admin(
        "DELETE",
        &format!("/api/inbound-addresses/{}", text(&created, "id")),
        None,
    )
    .await;
    assert!(
        fx.db
            .inbound_by_address(&address)
            .expect("lookup")
            .is_none()
    );
}

#[tokio::test]
async fn twenty_first_address_should_be_refused_and_the_twentieth_accepted() {
    let fx = Fixture::new();
    for n in 0..MAX_INBOUND_PER_BOARD {
        let reply = fx.create(fx.address(&format!("Address {n}"))).await;
        assert_eq!(reply.status, StatusCode::OK, "address {n}: {}", reply.body);
    }
    let refused = fx.create(fx.address("One too many")).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("quota_exceeded"));
    let other = fx.db.create_board(&fx.admin, "Other").expect("board");
    let column = fx.db.create_column(&other, "Inbox", None).expect("column");
    let apart = fx
        .admin(
            "POST",
            &format!("/api/boards/{other}/inbound-addresses"),
            Some(json!({"name": "Apart", "column_id": column})),
        )
        .await;
    assert_eq!(apart.status, StatusCode::OK, "{}", apart.body);
}

#[tokio::test]
async fn invalid_input_should_be_refused_with_a_400() {
    let fx = Fixture::new();
    let long_name = "n".repeat(101);
    let long_prefix = "p".repeat(33);
    for body in [
        json!({"column_id": fx.column}),
        fx.address("   "),
        fx.address(&long_name),
        json!({"name": "No column"}),
        with(fx.address("Upper"), "prefix", json!("Help")),
        with(fx.address("Space"), "prefix", json!("a b")),
        with(fx.address("Dot"), "prefix", json!("a.b")),
        with(fx.address("Lead"), "prefix", json!("-a")),
        with(fx.address("Trail"), "prefix", json!("a-")),
        with(fx.address("Long"), "prefix", json!(long_prefix)),
        with(fx.address("Unknown"), "column_id", json!("nope")),
    ] {
        let reply = fx.create(body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
    }
    assert!(fx.list().await.body.as_array().expect("array").is_empty());
}

#[tokio::test]
async fn empty_or_null_prefix_should_mean_none() {
    let fx = Fixture::new();
    for prefix in [json!(""), Value::Null] {
        let reply = fx.create(with(fx.address("None"), "prefix", prefix)).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.body["prefix"], "");
    }
}

#[tokio::test]
async fn column_of_another_board_should_look_like_a_missing_column() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Other").expect("board");
    let foreign = fx.db.create_column(&other, "Inbox", None).expect("column");
    let foreign = fx
        .create(with(fx.address("X"), "column_id", json!(foreign)))
        .await;
    let missing = fx
        .create(with(fx.address("X"), "column_id", json!("absent")))
        .await;
    assert_eq!(foreign.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        (foreign.status, &foreign.body),
        (missing.status, &missing.body)
    );
}

#[tokio::test]
async fn missing_board_and_address_should_be_404() {
    let fx = Fixture::new();
    let created = fx
        .admin(
            "POST",
            "/api/boards/absent/inbound-addresses",
            Some(fx.address("Lost")),
        )
        .await;
    assert_eq!(created.status, StatusCode::NOT_FOUND);
    let listed = fx
        .admin("GET", "/api/boards/absent/inbound-addresses", None)
        .await;
    assert_eq!(listed.status, StatusCode::NOT_FOUND);
    assert_eq!(fx.rotate("absent").await.status, StatusCode::NOT_FOUND);
    let deleted = fx
        .admin("DELETE", "/api/inbound-addresses/absent", None)
        .await;
    assert_eq!(deleted.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn addresses_should_need_the_inbound_manage_permission() {
    let fx = Fixture::new();
    let created = fx.create(fx.address("Guarded")).await;
    let id = text(&created, "id");
    let address = text(&created, "address").to_string();
    let board_uri = format!("/api/boards/{}/inbound-addresses", fx.board);
    let rotate_uri = format!("/api/inbound-addresses/{id}/rotate");
    let delete_uri = format!("/api/inbound-addresses/{id}");
    for (method, uri, body) in [
        ("GET", board_uri.as_str(), None),
        ("POST", board_uri.as_str(), Some(fx.address("Sneaky"))),
        ("POST", rotate_uri.as_str(), None),
        ("DELETE", delete_uri.as_str(), None),
    ] {
        let reply = fx.send(&fx.work_token, method, uri, body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.code.as_deref(), Some("permission_denied"));
        assert_eq!(reply.body["permission"], "inbound.manage");
    }
    assert!(
        fx.db
            .inbound_by_address(&address)
            .expect("lookup")
            .is_some()
    );
    assert_eq!(fx.list().await.body.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn deleting_a_board_should_remove_its_addresses_and_their_lookups() {
    let fx = Fixture::new();
    let doomed = fx.create(fx.address("Doomed")).await;
    let address = text(&doomed, "address").to_string();
    let id = text(&doomed, "id").to_string();
    let spared_board = fx.db.create_board(&fx.admin, "Spared").expect("board");
    let spared_column = fx
        .db
        .create_column(&spared_board, "In", None)
        .expect("column");
    let spared = fx
        .admin(
            "POST",
            &format!("/api/boards/{spared_board}/inbound-addresses"),
            Some(json!({"name": "Kept", "column_id": spared_column})),
        )
        .await;
    let spared_address = text(&spared, "address").to_string();

    let deleted = fx
        .admin("DELETE", &format!("/api/boards/{}", fx.board), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    assert!(fx.db.inbound_address(&id).expect("read").is_none());
    assert!(
        fx.db
            .inbound_by_address(&address)
            .expect("lookup")
            .is_none()
    );
    assert!(
        fx.db
            .inbound_by_address(&spared_address)
            .expect("lookup")
            .is_some()
    );
}

#[tokio::test]
async fn deleting_a_column_should_delete_the_addresses_that_name_it() {
    let fx = Fixture::new();
    let doomed = fx
        .db
        .create_column(&fx.board, "Doomed", None)
        .expect("column");
    let lost = fx
        .create(with(fx.address("Lost"), "column_id", json!(doomed)))
        .await;
    let kept = fx.create(fx.address("Kept")).await;
    let deleted = fx
        .admin("DELETE", &format!("/api/columns/{doomed}"), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    let lost_address = text(&lost, "address");
    assert!(
        fx.db
            .inbound_by_address(lost_address)
            .expect("lookup")
            .is_none()
    );
    assert!(
        fx.db
            .inbound_address(text(&lost, "id"))
            .expect("read")
            .is_none()
    );
    assert!(
        fx.db
            .inbound_by_address(text(&kept, "address"))
            .expect("lookup")
            .is_some()
    );
}

#[tokio::test]
async fn addresses_should_not_appear_in_the_board_archive() {
    let fx = Fixture::new();
    let created = fx.create(fx.address("Private")).await;
    let address = text(&created, "address");
    let hash = hash_inbound_address(address);
    let archive = fx
        .admin(
            "GET",
            &format!("/api/boards/{}/export-data", fx.board),
            None,
        )
        .await;
    assert_eq!(archive.status, StatusCode::OK, "{}", archive.body);
    let body = archive.body.to_string();
    assert!(!body.contains(address));
    assert!(!body.contains(&hash));
}

#[tokio::test]
async fn addresses_should_count_toward_the_account_stored_bytes() {
    let fx = Fixture::new();
    let held = |fx: &Fixture| fx.db.stored_bytes_by_account().expect("usage")[fx.admin.as_str()];
    let before = held(&fx);
    let created = fx.create(fx.address("Counted")).await;
    assert!(held(&fx) > before);
    fx.admin(
        "DELETE",
        &format!("/api/inbound-addresses/{}", text(&created, "id")),
        None,
    )
    .await;
    assert_eq!(held(&fx), before);
}

struct Browser {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    board: String,
    column: String,
}

impl Browser {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("browser.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.ensure_owner("owner").expect("owner").account_id();
        let board = db.create_board(&owner, "Mail").expect("board");
        let column = db.create_column(&board, "Inbox", None).expect("column");
        let state =
            AppState::new(db.clone(), None, none_mode(None)).with_limits(unthrottled_limits());
        Self {
            _dir: dir,
            app: router(state),
            db,
            board,
            column,
        }
    }

    async fn page(&self) -> Reply {
        let uri = format!("/b/{}/settings/add-from-email", self.board);
        call(&self.app, "GET", &uri, &[], None).await
    }
}

#[tokio::test]
async fn settings_tab_should_list_each_address_with_its_prefix_and_column() {
    let fx = Browser::new();
    let issued = fx
        .db
        .create_inbound(NewInbound {
            board_id: fx.board.clone(),
            name: "Support".to_string(),
            prefix: "help".to_string(),
            column_id: fx.column.clone(),
        })
        .expect("address");
    let page = fx.page().await;
    assert_eq!(page.status, StatusCode::OK);
    let html = page.body.as_str().expect("html");
    assert!(html.contains(&format!("data-inbound-id=\"{}\"", issued.row.id)));
    assert!(html.contains("Support"));
    assert!(html.contains("<code>help</code>"));
    assert!(html.contains("Inbox"));
    assert!(html.contains("id=\"ib-form\""));
    assert!(html.contains("data-page=\"board-settings-inbound\""));
    assert!(!html.contains(&issued.address));
}

#[tokio::test]
async fn settings_tab_should_show_the_empty_state_and_not_the_unsupported_notice() {
    let fx = Browser::new();
    let page = fx.page().await;
    let html = page.body.as_str().expect("html");
    assert!(!html.contains("does not support creating tasks by email"));
    assert!(html.contains("id=\"ib-empty\""));
}

struct Sent {
    status: StatusCode,
    retry_after: Option<String>,
    error_code: Option<String>,
    body: Value,
}

async fn post_mail(
    app: &axum::Router,
    address: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Sent {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/inbound/{address}"));
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response = app
        .clone()
        .oneshot(builder.body(axum::body::Body::from(body)).expect("request"))
        .await
        .expect("oneshot");
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|value| value.to_str().expect("ascii").to_string());
    let error_code = response
        .headers()
        .get("x-pomb-error-code")
        .map(|value| value.to_str().expect("ascii").to_string());
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Sent {
        status,
        retry_after,
        error_code,
        body,
    }
}

async fn post_json(app: &axum::Router, address: &str, mail: &Value) -> Sent {
    post_mail(
        app,
        address,
        Some("application/json"),
        mail.to_string().into_bytes(),
    )
    .await
}

impl Fixture {
    async fn issue(&self) -> String {
        let created = self.create(self.address("Mail")).await;
        assert_eq!(created.status, StatusCode::OK, "{}", created.body);
        text(&created, "address").to_string()
    }

    fn tasks(&self) -> Vec<TaskRow> {
        self.db.board_tasks(&self.board).expect("tasks")
    }

    fn stored_address(&self) -> String {
        let new = NewInbound {
            board_id: self.board.clone(),
            name: "Mail".to_string(),
            prefix: String::new(),
            column_id: self.column.clone(),
        };
        self.db.create_inbound(new).expect("address").address
    }
}

#[tokio::test]
async fn post_should_create_one_task_in_the_column_with_the_subject_and_text() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let sent = post_json(
        &fx.app,
        &address,
        &json!({"subject": "  Fix the printer ", "text": "It jams.\nAgain."}),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    let tasks = fx.tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(sent.body["task_id"], json!(tasks[0].id));
    assert_eq!(tasks[0].name, "Fix the printer");
    assert_eq!(tasks[0].description, "It jams.\nAgain.");
    assert_eq!(tasks[0].column_id, fx.column);
    assert_eq!(tasks[0].history[0].actor, "email");
}

#[tokio::test]
async fn post_should_take_the_field_names_of_the_common_mail_services() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    for mail in [
        json!({"From": "a@example.com", "Subject": "Postmark", "TextBody": "one"}),
        json!({"sender": "a@example.com", "subject": "Mailgun", "body-plain": "two"}),
        json!({"sender": "a@example.com", "subject": "Snake", "text_body": "three"}),
    ] {
        let sent = post_json(&fx.app, &address, &mail).await;
        assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    }
    let mut found: Vec<_> = fx
        .tasks()
        .into_iter()
        .map(|task| (task.name, task.description))
        .collect();
    found.sort();
    assert_eq!(
        found,
        [
            ("Mailgun".to_string(), "two".to_string()),
            ("Postmark".to_string(), "one".to_string()),
            ("Snake".to_string(), "three".to_string()),
        ]
    );
}

#[tokio::test]
async fn post_should_use_the_text_of_the_html_body_when_there_is_no_text_body() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let html = "<style>p{color:red}</style><p>Hello <b>there</b> &amp; welcome</p><SCRIPT>alert(1)</SCRIPT><br>Bye";
    let sent = post_json(&fx.app, &address, &json!({"subject": "Html", "html": html})).await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    let tasks = fx.tasks();
    let description = &tasks[0].description;
    for expected in ["Hello", "there", "& welcome", "Bye"] {
        assert!(description.contains(expected), "{description}");
    }
    for refused in ["alert", "color", "<", "&amp;"] {
        assert!(!description.contains(refused), "{description}");
    }
}

#[tokio::test]
async fn post_with_an_unclosed_script_should_not_take_quadratic_time() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let html = "<script>".repeat(100_000);
    let started = Instant::now();
    let sent = post_json(&fx.app, &address, &json!({"subject": "Slow", "html": html})).await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
}

#[tokio::test]
async fn post_should_name_a_task_without_a_subject_and_cut_a_long_one() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    for mail in [
        json!({"text": "body only"}),
        json!({"subject": "   ", "text": "blank"}),
        json!({"subject": "First line\nSecond line", "text": "lines"}),
        json!({"subject": "x".repeat(900), "text": "long"}),
    ] {
        let sent = post_json(&fx.app, &address, &mail).await;
        assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    }
    let mut names: Vec<_> = fx.tasks().into_iter().map(|task| task.name).collect();
    names.sort_by_key(|name| name.chars().count());
    assert_eq!(names[0], "First line");
    assert_eq!(names[1], "(no subject)");
    assert_eq!(names[2], "(no subject)");
    assert_eq!(names[3].chars().count(), 500);
}

#[tokio::test]
async fn post_should_cut_a_long_body_at_a_character_boundary() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let body = "é".repeat(40_000);
    let sent = post_json(&fx.app, &address, &json!({"subject": "Long", "text": body})).await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    let description = &fx.tasks()[0].description;
    assert!(description.len() <= 64 * 1024, "{}", description.len());
    assert!(description.len() > 64 * 1024 - 4);
    assert!(description.chars().all(|c| c == 'é'));
}

#[tokio::test]
async fn post_to_a_wrong_rotated_or_deleted_secret_should_create_nothing_and_answer_alike() {
    let fx = Fixture::new();
    let created = fx.create(fx.address("Mail")).await;
    let id = text(&created, "id").to_string();
    let old = text(&created, "address").to_string();
    let rotated = fx.rotate(&id).await;
    let current = text(&rotated, "address").to_string();
    let mail = json!({"subject": "Nope", "text": "nope"});
    let mut flipped = current.clone();
    flipped.replace_range(..1, if current.starts_with('0') { "1" } else { "0" });
    let unknown = post_json(&fx.app, "never-issued", &mail).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    for address in [old.as_str(), flipped.as_str(), &"a".repeat(5000)] {
        let sent = post_json(&fx.app, address, &mail).await;
        assert_eq!(sent.status, unknown.status, "{address}");
        assert_eq!(sent.body, unknown.body, "{address}");
    }
    assert!(fx.tasks().is_empty());
    let deleted = fx
        .admin("DELETE", &format!("/api/inbound-addresses/{id}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
    let sent = post_json(&fx.app, &current, &mail).await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert_eq!(sent.body, unknown.body);
    assert!(fx.tasks().is_empty());
}

#[tokio::test]
async fn post_to_an_address_of_a_deleted_column_should_look_missing() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    fx.db
        .create_column(&fx.board, "Other", None)
        .expect("second column");
    let deleted = fx
        .admin("DELETE", &format!("/api/columns/{}", fx.column), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    let sent = post_json(&fx.app, &address, &json!({"subject": "Gone"})).await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert!(fx.tasks().is_empty());
}

#[tokio::test]
async fn posts_to_missing_addresses_should_be_limited_per_client_with_retry_after() {
    let start = Instant::now();
    let fx = Fixture::with_limits(Limits::with_clock(Arc::new(move || start)));
    let address = fx.issue().await;
    let mail = json!({"subject": "Miss"});
    let mut limited = None;
    for _ in 0..40 {
        let sent = post_json(&fx.app, "never-issued", &mail).await;
        if sent.status == StatusCode::TOO_MANY_REQUESTS {
            limited = Some(sent);
            break;
        }
        assert_eq!(sent.status, StatusCode::NOT_FOUND);
    }
    let limited = limited.expect("a 429 within 40 misses");
    assert!(limited.retry_after.is_some());
    assert_eq!(limited.body["code"], "rate_limited");
    let hit = post_json(&fx.app, &address, &mail).await;
    assert_eq!(hit.status, StatusCode::CREATED, "{}", hit.body);
}

#[tokio::test]
async fn posts_past_the_rate_of_one_address_should_get_429_with_retry_after() {
    let start = Instant::now();
    let fx = Fixture::with_limits(Limits::with_clock(Arc::new(move || start)));
    let busy = fx.issue().await;
    let quiet = fx.issue().await;
    let mail = json!({"subject": "Burst"});
    for _ in 0..Limits::INBOUND_BURST {
        let sent = post_json(&fx.app, &busy, &mail).await;
        assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    }
    let refused = post_json(&fx.app, &busy, &mail).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.body["code"], "rate_limited");
    assert!(refused.retry_after.is_some());
    assert_eq!(fx.tasks().len(), Limits::INBOUND_BURST as usize);
    let other = post_json(&fx.app, &quiet, &mail).await;
    assert_eq!(other.status, StatusCode::CREATED, "{}", other.body);
}

#[tokio::test]
async fn post_over_the_body_cap_should_get_413_and_create_nothing() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let big = json!({"subject": "Big", "text": "x".repeat(1024 * 1024 + 1)});
    let sent = post_json(&fx.app, &address, &big).await;
    assert_eq!(sent.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", sent.body);
    assert!(fx.tasks().is_empty());
    let padded = format!(
        "{{\"subject\":\"Big\",\"text\":\"{}\"}}",
        "x".repeat(2 * 1024 * 1024)
    );
    let sent = post_mail(
        &fx.app,
        &address,
        Some("application/json"),
        padded.into_bytes(),
    )
    .await;
    assert_eq!(sent.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", sent.body);
    assert!(fx.tasks().is_empty());
}

#[tokio::test]
async fn post_with_a_bad_content_type_or_body_should_create_nothing() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let body = br#"{"subject":"Plain"}"#.to_vec();
    let sent = post_mail(&fx.app, &address, Some("text/plain"), body.clone()).await;
    assert_eq!(sent.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let sent = post_mail(&fx.app, &address, None, body).await;
    assert_eq!(sent.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    for broken in ["not json", "[1,2]", "{\"subject\": 5}"] {
        let sent = post_mail(&fx.app, &address, Some("application/json"), broken.into()).await;
        assert_eq!(
            sent.status,
            StatusCode::BAD_REQUEST,
            "{broken}: {}",
            sent.body
        );
    }
    assert!(fx.tasks().is_empty());
}

#[tokio::test]
async fn post_should_need_no_credential_and_ignore_a_bearer_token() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let uri = format!("/api/inbound/{address}");
    let reply = call(&fx.app, "POST", &uri, &[], Some(json!({"subject": "Open"}))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    let bearer = format!("Bearer {}", fx.work_token);
    let reply = call(
        &fx.app,
        "POST",
        &uri,
        &[("authorization", &bearer)],
        Some(json!({"subject": "Again"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(fx.tasks().len(), 2);
}

#[tokio::test]
async fn post_from_another_site_in_a_browser_should_be_refused() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let uri = format!("/api/inbound/{address}");
    let reply = call(
        &fx.app,
        "POST",
        &uri,
        &[("sec-fetch-site", "cross-site")],
        Some(json!({"subject": "Forged"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert!(fx.tasks().is_empty());
}

#[tokio::test]
async fn post_should_place_the_task_like_a_browser_task_with_the_board_defaults() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let lane = fx.db.list_swimlanes(&fx.board).expect("lanes");
    let sent = post_json(&fx.app, &address, &json!({"subject": "Lane"})).await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    let task = &fx.tasks()[0];
    assert_eq!(task.swimlane_id, lane.first().map(|lane| lane.id.clone()));
    assert_eq!(task.history.len(), 1);
    assert_eq!(task.history[0].kind, HistoryKind::Created);
}

#[tokio::test]
async fn post_with_a_declared_length_over_the_cap_should_get_413_before_the_body_is_read() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/inbound/{address}"))
        .header("content-type", "application/json")
        .header("content-length", (2 * 1024 * 1024).to_string())
        .body(axum::body::Body::from("{}"))
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(fx.tasks().is_empty());
}

#[tokio::test]
async fn post_between_the_browser_body_limit_and_the_inbound_cap_should_create_a_task() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let mail = json!({"subject": "Medium", "text": "x".repeat(500 * 1024)});
    let sent = post_json(&fx.app, &address, &mail).await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    assert_eq!(fx.tasks().len(), 1);
}

#[tokio::test]
async fn post_to_the_address_of_a_suspended_owner_should_look_missing() {
    let fx = Fixture::new();
    let member = fx.db.create_user("bob").expect("member");
    let board = fx.db.create_board(&member, "Bob mail").expect("board");
    let column = fx.db.create_column(&board, "Inbox", None).expect("column");
    let address = fx
        .db
        .account(&member)
        .create_inbound(
            &fx.db
                .account(&member)
                .board(&board)
                .expect("read")
                .expect("board"),
            NewInbound {
                board_id: board.clone(),
                name: "Bob".to_string(),
                prefix: String::new(),
                column_id: column,
            },
        )
        .expect("address")
        .address;
    let mail = json!({"subject": "Mail"});
    let before = post_json(&fx.app, &address, &mail).await;
    assert_eq!(before.status, StatusCode::CREATED, "{}", before.body);
    let outcome = fx
        .db
        .apply_account_action(member.as_str(), AccountAction::Suspend)
        .expect("suspend");
    assert!(matches!(outcome, AdminActionOutcome::Done(_)));
    let unknown = post_json(&fx.app, "never-issued", &mail).await;
    let after = post_json(&fx.app, &address, &mail).await;
    assert_eq!(after.status, StatusCode::NOT_FOUND);
    assert_eq!(after.body, unknown.body);
    assert_eq!(fx.db.board_tasks(&board).expect("tasks").len(), 1);
}

#[tokio::test]
async fn post_into_a_full_board_should_get_409() {
    const LIMIT: u32 = 2;
    let fx = Fixture::new();
    fx.db.apply_mode(InstanceMode::Multi).expect("mode");
    fx.db.create_user("bob").expect("member");
    fx.db
        .set_board_member(&fx.board, "bob", BoardRole::Member)
        .expect("member");
    let handed = fx
        .db
        .change_board_owner(&fx.board, &fx.admin, "bob")
        .expect("owner");
    assert!(matches!(handed, OwnerChange::Changed(_)));
    fx.db
        .update_instance_settings(|settings| settings.quotas.tasks_per_board = LIMIT)
        .expect("quota");
    for n in 0..LIMIT {
        fx.db
            .create_task(&fx.column, None, &format!("Task {n}"), 1, None)
            .expect("task");
    }
    let address = fx.stored_address();
    let before = fx.tasks().len();
    let refused = post_json(
        &fx.app,
        &address,
        &json!({"subject": "One too many", "text": "It fits nowhere."}),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["code"], "quota_exceeded");
    assert_eq!(fx.tasks().len(), before);
}

#[tokio::test]
async fn post_into_a_rule_guarded_column_should_get_422() {
    let fx = Fixture::new();
    let address = fx.issue().await;
    let column = fx.column.clone();
    let rules = json!({"column_rules": {"columns": {column: {"sections": ["Done when"]}}}});
    fx.db
        .set_board_config(&fx.board, rules.as_object().expect("object"))
        .expect("rules");
    let before = fx.tasks().len();
    let refused = post_json(
        &fx.app,
        &address,
        &json!({"subject": "Refused", "text": "No criteria here."}),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.body
    );
    assert_eq!(refused.body["code"], "column_rule");
    assert_eq!(fx.tasks().len(), before);
}

#[tokio::test]
async fn post_while_the_disk_is_full_should_get_507() {
    let probe = DiskProbe {
        stored: Arc::new(|| Ok(1)),
        file: Arc::new(|| Ok(1)),
        free: Arc::new(|_| Ok(0)),
    };
    let fx = Fixture::with_disk(DiskGuard::new(probe, Path::new(".")));
    let address = fx.stored_address();
    let before = fx.tasks().len();
    let refused = post_json(
        &fx.app,
        &address,
        &json!({"subject": "Full", "text": "No room."}),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        refused.body
    );
    assert_eq!(refused.error_code.as_deref(), Some("storage_full"));
    assert_eq!(fx.tasks().len(), before);
}
