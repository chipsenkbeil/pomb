use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use crate::AppState;
use crate::auth::{hash_password, new_api_token_record};
use crate::db::{Db, TokenTouchThrottle};

const MAX_BODY_BYTES: usize = 1 << 22;

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
