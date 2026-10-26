use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use crate::AppState;
use crate::auth::{AuthChoice, AuthMode, SignIn, new_api_token_record};
use crate::csrf::REFUSED_BODY;
use crate::db::Db;
use crate::models::{AccountId, AuthScope, BackupSettings};

const MAX_BODY_BYTES: usize = 1 << 20;

struct Reply {
    status: StatusCode,
    body: Value,
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> Reply {
    call_as(app, method, uri, &[], body).await
}

async fn call_as(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
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
    Reply {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
    }
}

fn open_db(dir: &std::path::Path) -> Db {
    let path = dir.join("pomb.redb");
    Db::connect(path.to_str().expect("utf8 path")).expect("connect")
}

fn state_for(db: Db) -> AppState {
    let sign_in =
        SignIn::new(AuthChoice::Chosen(AuthMode::None), "127.0.0.1", None).expect("none mode");
    AppState::new(db, None, sign_in)
}

fn app_for(db: Db) -> axum::Router {
    router(state_for(db))
}

#[tokio::test]
async fn settings_should_default_off_and_persist_across_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = app_for(open_db(dir.path()));

    let fresh = call(&app, "GET", "/api/v1/backups", None).await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    assert_eq!(
        fresh.body["settings"],
        json!({
            "enabled": false,
            "interval_hours": BackupSettings::DEFAULT_INTERVAL_HOURS,
            "keep": BackupSettings::DEFAULT_KEEP,
        })
    );
    assert_eq!(fresh.body["status"], Value::Null);
    assert_eq!(fresh.body["files"], json!([]));

    let wanted = json!({"enabled": true, "interval_hours": 12, "keep": 30});
    let saved = call(
        &app,
        "PUT",
        "/api/v1/backups/settings",
        Some(wanted.clone()),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body, wanted);
    drop(app);

    let restarted = app_for(open_db(dir.path()));
    let again = call(&restarted, "GET", "/api/v1/backups", None).await;
    assert_eq!(again.body["settings"], wanted);
}

#[tokio::test]
async fn settings_should_refuse_values_outside_the_bounds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = app_for(open_db(dir.path()));
    for (hours, keep) in [(0, 7), (169, 7), (24, 0), (24, 91)] {
        let reply = call(
            &app,
            "PUT",
            "/api/v1/backups/settings",
            Some(json!({"enabled": true, "interval_hours": hours, "keep": keep})),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{hours} {keep}");
    }
    for (hours, keep) in [(1, 1), (168, 90)] {
        let reply = call(
            &app,
            "PUT",
            "/api/v1/backups/settings",
            Some(json!({"enabled": true, "interval_hours": hours, "keep": keep})),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{hours} {keep}");
    }
    let stored = call(&app, "GET", "/api/v1/backups", None).await;
    assert_eq!(
        stored.body["settings"]["keep"], 90,
        "a refused value stored nothing"
    );
}

#[tokio::test]
async fn back_up_now_should_write_a_file_and_show_it_in_the_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = app_for(open_db(dir.path()));

    let ran = call(&app, "POST", "/api/v1/backups", None).await;

    assert_eq!(ran.status, StatusCode::OK, "{}", ran.body);
    assert_eq!(ran.body["ok"], true);
    let file = ran.body["file"].as_str().expect("file name").to_string();
    assert!(
        file.starts_with("pomb-") && file.ends_with(".redb"),
        "{file}"
    );
    assert!(dir.path().join("backups").join(&file).is_file());
    let view = call(&app, "GET", "/api/v1/backups", None).await;
    assert_eq!(view.body["status"], ran.body);
    assert_eq!(view.body["files"][0]["name"], file.as_str());
    assert_eq!(view.body["files"][0]["size_bytes"], ran.body["size_bytes"]);
}

#[tokio::test]
async fn back_up_now_should_be_refused_while_a_backup_holds_the_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = state_for(open_db(dir.path()));
    let backups = state.backups.clone();
    let app = router(state);

    let running = backups.hold().expect("lock");
    let refused = call(&app, "POST", "/api/v1/backups", None).await;

    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.body["code"], "conflict");
    assert_eq!(refused.body["message"], "a backup is already running");
    assert!(!dir.path().join("backups").exists(), "nothing was written");
    drop(running);
    let allowed = call(&app, "POST", "/api/v1/backups", None).await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);
}

#[tokio::test]
async fn failed_back_up_now_should_show_the_error_and_keep_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let occupied = dir.path().join("occupied");
    std::fs::write(&occupied, "a file").expect("file");
    let state = AppState::new(
        open_db(dir.path()),
        None,
        SignIn::new(AuthChoice::Chosen(AuthMode::None), "127.0.0.1", None).expect("none mode"),
    )
    .with_backup_dir(occupied.join("backups"));
    let app = router(state);

    let ran = call(&app, "POST", "/api/v1/backups", None).await;

    assert_eq!(ran.status, StatusCode::OK, "{}", ran.body);
    assert_eq!(ran.body["ok"], false);
    let error = ran.body["error"].as_str().expect("error text");
    assert!(error.contains("could not create"), "{error}");
    let view = call(&app, "GET", "/api/v1/backups", None).await;
    assert_eq!(view.status, StatusCode::OK);
    assert_eq!(view.body["status"]["error"], error);
    assert_eq!(view.body["files"], json!([]));
}

#[tokio::test]
async fn backup_operations_should_need_the_browser_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = open_db(dir.path());
    let (record, raw) =
        new_api_token_record(&AccountId::ownerless(), "admin-bot", AuthScope::Admin, None);
    db.create_api_token(&record).expect("token");
    let app = app_for(db);
    let bearer = format!("Bearer {raw}");
    for (method, uri) in [
        ("GET", "/api/v1/backups"),
        ("POST", "/api/v1/backups"),
        ("PUT", "/api/v1/backups/settings"),
    ] {
        let body =
            (method == "PUT").then(|| json!({"enabled": true, "interval_hours": BackupSettings::DEFAULT_INTERVAL_HOURS, "keep": BackupSettings::DEFAULT_KEEP}));
        let reply = call_as(
            &app,
            method,
            uri,
            &[("authorization", bearer.as_str())],
            body,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.body["code"], "session_required", "{method} {uri}");
    }
    assert!(!dir.path().join("backups").exists(), "no backup ran");
}

#[tokio::test]
async fn settings_page_should_show_the_backups_pane() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = app_for(open_db(dir.path()));
    call(&app, "POST", "/api/v1/backups", None).await;

    let page = call(&app, "GET", "/settings", None).await;

    let html = page.body.as_str().expect("html");
    assert_eq!(page.status, StatusCode::OK);
    assert!(html.contains("id=\"backups\""), "backups pane");
    assert!(html.contains("Back up automatically"));
    assert!(html.contains("Back up now"));
    assert!(html.contains(": ok."), "last result");
    assert!(
        html.contains("pomb-") && html.contains(".redb"),
        "kept file listed"
    );
}

#[tokio::test]
async fn settings_should_refuse_a_typo_or_an_omitted_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = app_for(open_db(dir.path()));
    for body in [
        json!({"enabled": true, "interval_hours": 12}),
        json!({"enabled": true, "keep": 12}),
        json!({"interval_hours": 12, "keep": 12}),
        json!({"enabled": true, "interval_hours": 12, "keep": 12, "kept": 3}),
        json!({"enabled": true, "interval_hour": 12, "keep": 12}),
        json!({"enabled": "yes", "interval_hours": 12, "keep": 12}),
    ] {
        let reply = call(&app, "PUT", "/api/v1/backups/settings", Some(body.clone())).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
    }
    let stored = call(&app, "GET", "/api/v1/backups", None).await;
    assert_eq!(stored.body["settings"]["enabled"], false, "nothing stored");
}

#[tokio::test]
async fn cross_site_backup_writes_should_get_the_csrf_refusal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = app_for(open_db(dir.path()));
    let cross_site = [
        ("cookie", "session=stolen"),
        ("sec-fetch-site", "cross-site"),
        ("origin", "https://evil.example"),
    ];
    let settings = json!({"enabled": true, "interval_hours": 1, "keep": 1});

    let started = call_as(&app, "POST", "/api/v1/backups", &cross_site, None).await;
    let saved = call_as(
        &app,
        "PUT",
        "/api/v1/backups/settings",
        &cross_site,
        Some(settings),
    )
    .await;

    for reply in [started, saved] {
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.body, REFUSED_BODY);
    }
    assert!(!dir.path().join("backups").exists(), "no backup ran");
    let stored = call(&app, "GET", "/api/v1/backups", None).await;
    assert_eq!(stored.body["settings"]["enabled"], false, "nothing stored");
}
