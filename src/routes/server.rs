use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ConnectInfo;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;
use utoipa::OpenApi;

use super::docs::{SKILL, render_api_reference};
use super::{ApiDoc, router};
use crate::AppState;
use crate::auth::{create_session, hash_password, new_api_token_record};
use crate::db::{BootstrapToken, Db, TokenTouchThrottle};
use crate::limit::{Buckets, Limits};

const MAX_BODY_BYTES: usize = 1 << 22;
const BURST: usize = Buckets::BURST as usize;
const PER_IP: usize = Limits::PER_IP_PER_MINUTE;
const WINDOW_SECS: u64 = Limits::WINDOW.as_secs();

fn open_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("server-config.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let password = hash_password("s3cret").expect("hash password");
    db.create_user("admin", &password).expect("create user");
    (dir, db)
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    host: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host);
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            axum::body::Body::from(value.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn start_device(app: &axum::Router, host: &str) -> Value {
    let body = json!({"client_name": "agent", "scopes": ["read"]});
    let (status, started) = send(app, "POST", "/api/v1/auth/device", host, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    started
}

#[tokio::test]
async fn device_start_should_use_the_public_url_whatever_the_host_header_says() {
    let (_dir, db) = open_db();
    let state =
        AppState::new(db, None).with_public_url(Some("https://chipflow.example.com".to_string()));
    let app = router(state);

    let started = start_device(&app, "evil.example").await;
    assert_eq!(
        started["verification_url"],
        "https://chipflow.example.com/device"
    );
}

#[tokio::test]
async fn device_start_should_follow_the_host_header_without_a_public_url() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None));

    let started = start_device(&app, "chipflow.test:3350").await;
    assert_eq!(
        started["verification_url"],
        "http://chipflow.test:3350/device"
    );
}

#[tokio::test]
async fn openapi_should_list_the_public_url_as_its_server() {
    let (_dir, db) = open_db();
    let app =
        router(AppState::new(db.clone(), None).with_public_url(Some("https://a.test".to_string())));
    let (status, spec) = send(&app, "GET", "/api/v1/openapi.json", "evil.example", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["servers"], json!([{"url": "https://a.test"}]));

    let app = router(AppState::new(db, None));
    let (_, spec) = send(&app, "GET", "/api/v1/openapi.json", "evil.example", None).await;
    assert_eq!(spec.get("servers"), None);
}

#[tokio::test]
async fn api_reference_should_be_public_markdown_rendered_from_the_spec() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None));
    let request = axum::http::Request::builder()
        .uri("/docs/api.md")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()["content-type"].to_str().expect("ascii");
    assert!(content_type.starts_with("text/markdown"), "{content_type}");
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("body");
    let served = String::from_utf8(bytes.to_vec()).expect("utf8");
    let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec");
    assert_eq!(served, render_api_reference(&spec));
    assert!(served.contains("\n### POST /api/tasks/{id}/claim\n"));
}

#[tokio::test]
async fn llms_txt_should_link_under_the_public_url_or_the_request_host() {
    let (_dir, db) = open_db();
    let public =
        router(AppState::new(db.clone(), None).with_public_url(Some("https://a.test".to_string())));
    let (status, text) = send(&public, "GET", "/llms.txt", "evil.example", None).await;
    assert_eq!(status, StatusCode::OK);
    let text = text.as_str().expect("plain text");
    assert!(text.contains("(https://a.test/skill.md)"), "{text}");
    assert!(
        !text.contains("evil.example") && !text.contains("{base}"),
        "{text}"
    );

    let app = router(AppState::new(db, None));
    let (_, text) = send(&app, "GET", "/llms.txt", "chipflow.test:3350", None).await;
    let text = text.as_str().expect("plain text");
    assert!(
        text.contains("(http://chipflow.test:3350/docs/api.md)"),
        "{text}"
    );
}

async fn poll_token(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/api/v1/auth/device/token",
        "chipflow.test",
        Some(body),
    )
    .await
}

#[tokio::test]
async fn device_poll_should_answer_credential_less_callers_with_json_codes() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None).with_limits(unthrottled_limits()));

    let (status, body) = poll_token(&app, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let (status, body) = poll_token(&app, json!({"device_code": "never-issued"})).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::BAD_REQUEST, &json!("invalid_grant"))
    );
    assert!(body["message"].is_string(), "{body}");

    let started = start_device(&app, "chipflow.test").await;
    let pending = json!({"device_code": started["device_code"]});
    let (status, body) = poll_token(&app, pending.clone()).await;
    assert_eq!(
        (status, &body["code"]),
        (
            StatusCode::PRECONDITION_REQUIRED,
            &json!("authorization_pending")
        )
    );
    let (status, body) = poll_token(&app, pending).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::TOO_MANY_REQUESTS, &json!("slow_down"))
    );
}

#[tokio::test]
async fn credential_less_api_errors_should_be_json_and_cookie_errors_plain_text() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None).with_limits(unthrottled_limits()));

    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/auth/device",
        "chipflow.test",
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::BAD_REQUEST, &json!("invalid_request"))
    );

    let (status, body) = send(&app, "GET", "/api/nothing-here", "chipflow.test", None).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::NOT_FOUND, &json!("route_not_found"))
    );

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device/token")
        .header("host", "chipflow.test")
        .header("cookie", "chipflow_session=gone")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"device_code": "never-issued"}).to_string(),
        ))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("body");
    assert_eq!(&bytes[..], b"invalid_grant");
}

#[tokio::test]
async fn skill_should_be_served_as_markdown_with_its_version() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None));
    let (status, skill) = send(&app, "GET", "/skill.md", "chipflow.test", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(skill, Value::String(SKILL.to_string()));
    let (_, version) = send(&app, "GET", "/api/v1/version", "chipflow.test", None).await;
    let needle = format!(
        "version: \"{}\"",
        version["skill_version"].as_str().expect("v")
    );
    assert!(SKILL.contains(&needle));
}

#[test]
fn token_touch_should_allow_one_write_per_interval_per_token() {
    let throttle = TokenTouchThrottle::default();
    assert!(throttle.claim_write("a", 1000));
    assert!(!throttle.claim_write("a", 1000));
    assert!(!throttle.claim_write("a", 1059));
    assert!(throttle.claim_write("b", 1059));
    assert!(throttle.claim_write("a", 1060));
    assert!(!throttle.claim_write("a", 1119));
    assert!(throttle.claim_write("a", 1120));
}

#[tokio::test]
async fn bearer_gets_should_write_last_used_at_at_most_once_a_minute() {
    const SENTINEL: i64 = 1;
    let (_dir, db) = open_db();
    let (record, raw) = new_api_token_record("reader", vec!["read".into()], None);
    db.create_api_token(&record).expect("token");
    let app = router(AppState::new(db.clone(), None));
    let stored = || {
        db.list_api_tokens()
            .expect("tokens")
            .into_iter()
            .find(|token| token.id == record.id)
            .expect("token row")
            .last_used_at
    };
    let get = || async {
        let request = axum::http::Request::builder()
            .uri("/api/v1/auth/whoami")
            .header("authorization", format!("Bearer {raw}"))
            .body(axum::body::Body::empty())
            .expect("request");
        app.clone()
            .oneshot(request)
            .await
            .expect("oneshot")
            .status()
    };

    assert_eq!(stored(), None);
    assert_eq!(get().await, StatusCode::OK);
    let first = stored().expect("first request writes last_used_at");
    assert!(first > SENTINEL);

    db.touch_api_token(&record.id, SENTINEL).expect("reset");
    for _ in 0..99 {
        assert_eq!(get().await, StatusCode::OK);
    }
    assert_eq!(stored(), Some(SENTINEL), "no further token-table write");
}

struct ManualClock {
    now: Arc<Mutex<Instant>>,
}

impl ManualClock {
    fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(Instant::now())),
        }
    }

    fn limits(&self) -> Limits {
        let now = self.now.clone();
        Limits::with_clock(Arc::new(move || *now.lock().expect("clock")))
    }

    fn advance(&self, by: Duration) {
        *self.now.lock().expect("clock") += by;
    }
}

pub(super) fn unthrottled_limits() -> Limits {
    const HOUR_SECS: u64 = 3600;
    let start = Instant::now();
    let reads = AtomicU64::new(0);
    Limits::with_clock(Arc::new(move || {
        let read = reads.fetch_add(1, Ordering::Relaxed);
        start + Duration::from_secs(read * HOUR_SECS)
    }))
}

struct Sent {
    status: StatusCode,
    retry_after: Option<String>,
    body: String,
}

async fn send_as(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    peer: Option<&str>,
    body: &str,
) -> Sent {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "chipflow.test");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder
        .body(axum::body::Body::from(body.to_string()))
        .expect("request");
    if let Some(peer) = peer {
        let addr = SocketAddr::new(peer.parse().expect("peer ip"), 40000);
        request.extensions_mut().insert(ConnectInfo(addr));
    }
    let response = app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|value| value.to_str().expect("ascii").to_string());
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("body");
    Sent {
        status,
        retry_after,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn bearer(raw: &str) -> [(&'static str, String); 1] {
    [("authorization", format!("Bearer {raw}"))]
}

async fn whoami(app: &axum::Router, raw: &str) -> Sent {
    let [(name, value)] = bearer(raw);
    send_as(
        app,
        "GET",
        "/api/v1/auth/whoami",
        &[(name, &value)],
        None,
        "",
    )
    .await
}

#[tokio::test]
async fn bearer_should_get_429_with_retry_after_on_the_101st_request_in_a_second() {
    let (_dir, db) = open_db();
    let (first, first_raw) = new_api_token_record("first", vec!["read".into()], None);
    let (second, second_raw) = new_api_token_record("second", vec!["read".into()], None);
    db.create_api_token(&first).expect("token");
    db.create_api_token(&second).expect("token");
    let clock = ManualClock::new();
    let app = router(AppState::new(db, None).with_limits(clock.limits()));

    for _ in 0..BURST {
        assert_eq!(whoami(&app, &first_raw).await.status, StatusCode::OK);
    }
    let refused = whoami(&app, &first_raw).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.retry_after.as_deref(), Some("1"));
    let body: Value = serde_json::from_str(&refused.body).expect("json body");
    assert_eq!(body["code"], "rate_limited");

    assert_eq!(whoami(&app, &second_raw).await.status, StatusCode::OK);

    clock.advance(Duration::from_secs(1));
    assert_eq!(whoami(&app, &first_raw).await.status, StatusCode::OK);
}

#[tokio::test]
async fn bootstrap_token_should_be_limited_on_its_own_bucket() {
    let (_dir, db) = open_db();
    let (record, raw) = new_api_token_record("reader", vec!["read".into()], None);
    db.create_api_token(&record).expect("token");
    let bootstrap = BootstrapToken::new("boot-secret");
    let clock = ManualClock::new();
    let app = router(AppState::new(db, bootstrap).with_limits(clock.limits()));

    for _ in 0..BURST {
        assert_eq!(whoami(&app, "boot-secret").await.status, StatusCode::OK);
    }
    assert_eq!(
        whoami(&app, "boot-secret").await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(whoami(&app, &raw).await.status, StatusCode::OK);
}

#[tokio::test]
async fn session_should_stay_served_past_the_bearer_burst() {
    let (_dir, db) = open_db();
    let user = db
        .user_by_username("admin")
        .expect("lookup")
        .expect("admin");
    let session = create_session(&db, &user.id).expect("session");
    let cookie = format!("session={session}");
    let app = router(AppState::new(db, None).with_limits(ManualClock::new().limits()));
    let get = || async {
        let headers = [("cookie", cookie.as_str())];
        send_as(&app, "GET", "/api/v1/auth/tokens", &headers, None, "").await
    };

    for _ in 0..BURST + 50 {
        assert_eq!(get().await.status, StatusCode::OK);
    }
}

const DEVICE_START_BODY: &str = r#"{"client_name": "agent", "scopes": ["read"]}"#;

async fn start_device_from(app: &axum::Router, peer: &str, forwarded_for: Option<&str>) -> Sent {
    let mut headers = vec![("content-type", "application/json")];
    if let Some(forwarded_for) = forwarded_for {
        headers.push(("x-forwarded-for", forwarded_for));
    }
    send_as(
        app,
        "POST",
        "/api/v1/auth/device",
        &headers,
        Some(peer),
        DEVICE_START_BODY,
    )
    .await
}

#[tokio::test]
async fn device_start_should_refuse_the_eleventh_request_from_one_ip_in_a_minute() {
    let (_dir, db) = open_db();
    let clock = ManualClock::new();
    let app = router(AppState::new(db, None).with_limits(clock.limits()));

    for _ in 0..PER_IP {
        let sent = start_device_from(&app, "203.0.113.9", None).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    }
    let refused = start_device_from(&app, "203.0.113.9", None).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused.retry_after.as_deref(),
        Some(WINDOW_SECS.to_string().as_str())
    );
    let body: Value = serde_json::from_str(&refused.body).expect("json error body");
    assert_eq!(body["code"], "rate_limited");
    assert_eq!(
        body["message"],
        format!("rate limit exceeded, retry in {WINDOW_SECS} seconds")
    );

    let other = start_device_from(&app, "203.0.113.10", None).await;
    assert_eq!(other.status, StatusCode::OK);

    clock.advance(Limits::WINDOW);
    let again = start_device_from(&app, "203.0.113.9", None).await;
    assert_eq!(again.status, StatusCode::OK);
}

#[tokio::test]
async fn device_start_should_ignore_forwarded_for_from_a_non_loopback_peer() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None).with_limits(ManualClock::new().limits()));

    for n in 0..PER_IP {
        let spoof = format!("198.51.100.{n}");
        let sent = start_device_from(&app, "203.0.113.9", Some(&spoof)).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    }
    let refused = start_device_from(&app, "203.0.113.9", Some("198.51.100.99")).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn device_start_should_count_the_proxy_appended_client_behind_loopback() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None).with_limits(ManualClock::new().limits()));

    for _ in 0..PER_IP {
        let sent = start_device_from(&app, "127.0.0.1", Some("1.1.1.1, 198.51.100.7")).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    }
    let refused = start_device_from(&app, "127.0.0.1", Some("9.9.9.9, 198.51.100.7")).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    let other = start_device_from(&app, "::1", Some("198.51.100.8")).await;
    assert_eq!(other.status, StatusCode::OK);
}

#[tokio::test]
async fn login_should_refuse_the_eleventh_post_from_one_ip_in_a_minute() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db, None).with_limits(ManualClock::new().limits()));
    let post = || {
        send_as(
            &app,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            Some("203.0.113.9"),
            "user=admin&pass=wrong",
        )
    };

    for _ in 0..PER_IP {
        assert_eq!(post().await.status, StatusCode::SEE_OTHER);
    }
    let refused = post().await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused.body,
        format!("rate limit exceeded, retry in {WINDOW_SECS} seconds")
    );

    let page = send_as(&app, "GET", "/login", &[], Some("203.0.113.9"), "").await;
    assert_eq!(page.status, StatusCode::OK);
}

#[tokio::test]
async fn settings_should_show_the_skill_install_line_for_this_server() {
    let (_dir, db) = open_db();
    let user = db
        .user_by_username("admin")
        .expect("lookup")
        .expect("admin");
    let session = create_session(&db, &user.id).expect("session");
    let cookie = format!("session={session}");
    let app = router(AppState::new(db, None));
    let page = send_as(&app, "GET", "/settings", &[("cookie", &cookie)], None, "").await;
    assert_eq!(page.status, StatusCode::OK);
    let line = "mkdir -p ~/.claude/skills/chipflow &amp;&amp; curl -fsS \
                http://chipflow.test/skill.md -o ~/.claude/skills/chipflow/SKILL.md";
    assert!(page.body.contains(line), "{}", page.body);
    assert!(!page.body.contains("/agents.md"));
}
