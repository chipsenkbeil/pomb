use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::access_tests::mint_token;
use super::multi_tests::{Instance, finish, pair, recovery_code, start};
use super::passkey_tests::{Reply, SoftAuthenticator, add_passkey, call, session_of, sign_in};
use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::REAUTH_WINDOW_SECS;
use crate::db::Db;
use crate::passkey::MAX_CEREMONIES_PER_NETWORK;

const MAX_BODY_BYTES: usize = 1 << 20;

/// Three sessions of one account: one that just proved a passkey, one that did so past the
/// window, and one that never did.
struct Tiers {
    instance: Instance,
    fresh: String,
    stale: String,
    never: String,
}

/// Signs `key` in three times on `instance` and ages two of the sessions.
async fn tiers(instance: Instance, key: &mut SoftAuthenticator) -> Tiers {
    let mut cookies = Vec::new();
    for _ in 0..3 {
        cookies.push(session_of(&sign_in(&instance.app, key, &[]).await));
    }
    let token = |cookie: &str| cookie.trim_start_matches("session=").to_string();
    let past = (Utc::now() - Duration::seconds(REAUTH_WINDOW_SECS + 60)).to_rfc3339();
    let instance = instance.restamp(&[
        (&token(&cookies[1]), Some(past)),
        (&token(&cookies[2]), None),
    ]);
    let [fresh, stale, never]: [String; 3] = cookies.try_into().expect("three sessions");
    Tiers {
        instance,
        fresh,
        stale,
        never,
    }
}

fn assert_reauth_required(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(
        reply.code.as_deref(),
        Some("reauth_required"),
        "{}",
        reply.body
    );
}

/// Asserts that the stale and the never-verified sessions of `tiers` are refused `method` `uri`.
async fn assert_unchecked_refused(tiers: &Tiers, method: &str, uri: &str, body: Option<Value>) {
    for cookie in [&tiers.stale, &tiers.never] {
        let reply = call(
            &tiers.instance.app,
            method,
            uri,
            &[("cookie", cookie)],
            body.clone(),
        )
        .await;
        assert_reauth_required(&reply);
    }
}

/// Answers the reauth ceremony of the session `cookie` with `key`.
async fn reauth(app: &axum::Router, cookie: &str, key: &mut SoftAuthenticator) -> Reply {
    let headers = [("cookie", cookie)];
    let started = call(app, "POST", "/api/v1/auth/reauth/options", &headers, None).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let credential = key.sign_in(&started.body["options"]);
    let body = json!({"ceremony": started.body["ceremony"], "credential": credential});
    call(app, "POST", "/api/v1/auth/reauth", &headers, Some(body)).await
}

/// Posts a URL-encoded `form` to `uri` as the browser session `cookie`.
async fn form_post(
    app: &axum::Router,
    cookie: &str,
    uri: &str,
    form: String,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "pomb.test")
        .header("cookie", cookie)
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn starting_a_passkey_registration_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    assert_unchecked_refused(&t, "POST", "/api/v1/auth/passkeys/options", None).await;
    let started = call(
        &t.instance.app,
        "POST",
        "/api/v1/auth/passkeys/options",
        &[("cookie", &t.fresh)],
        None,
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
}

#[tokio::test]
async fn finishing_a_passkey_registration_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let app = &t.instance.app;
    let started = call(
        app,
        "POST",
        "/api/v1/auth/passkeys/options",
        &[("cookie", &t.fresh)],
        None,
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let credential = SoftAuthenticator::new(80).register(&started.body["options"]);
    let body =
        json!({"ceremony": started.body["ceremony"], "name": "Phone", "credential": credential});
    assert_unchecked_refused(&t, "POST", "/api/v1/auth/passkeys", Some(body.clone())).await;
    let added = call(
        app,
        "POST",
        "/api/v1/auth/passkeys",
        &[("cookie", &t.fresh)],
        Some(body),
    )
    .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
}

#[tokio::test]
async fn removing_a_passkey_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let app = &t.instance.app;
    let cookie = [("cookie", t.fresh.as_str())];
    let added = add_passkey(app, &mut SoftAuthenticator::new(81), &cookie, "Phone").await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let uri = format!(
        "/api/v1/auth/passkeys/{}",
        added.body["id"].as_str().expect("id")
    );
    assert_unchecked_refused(&t, "DELETE", &uri, None).await;
    let removed = call(app, "DELETE", &uri, &cookie, None).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
}

#[tokio::test]
async fn regenerating_a_recovery_code_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    assert_unchecked_refused(&t, "POST", "/api/v1/account/recovery-code", None).await;
    let regenerated = call(
        &t.instance.app,
        "POST",
        "/api/v1/account/recovery-code",
        &[("cookie", &t.fresh)],
        None,
    )
    .await;
    assert_eq!(regenerated.status, StatusCode::OK, "{}", regenerated.body);
}

#[tokio::test]
async fn deleting_an_account_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let body = json!({"handle": "bob"});
    assert_unchecked_refused(&t, "POST", "/api/v1/account/delete", Some(body.clone())).await;
    let deleted = call(
        &t.instance.app,
        "POST",
        "/api/v1/account/delete",
        &[("cookie", &t.fresh)],
        Some(body),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
}

#[tokio::test]
async fn deleting_a_member_as_the_admin_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.admin_key).await;
    let bob = t.instance.account("bob");
    let uri = format!("/api/v1/admin/accounts/{}", bob.as_str());
    assert_unchecked_refused(&t, "DELETE", &uri, None).await;
    let deleted = call(
        &t.instance.app,
        "DELETE",
        &uri,
        &[("cookie", &t.fresh)],
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
}

#[tokio::test]
async fn creating_an_api_token_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let body = json!({"name": "agent", "scopes": ["read"]});
    assert_unchecked_refused(&t, "POST", "/api/v1/auth/tokens", Some(body.clone())).await;
    let minted = call(
        &t.instance.app,
        "POST",
        "/api/v1/auth/tokens",
        &[("cookie", &t.fresh)],
        Some(body),
    )
    .await;
    assert_eq!(minted.status, StatusCode::OK, "{}", minted.body);
}

#[tokio::test]
async fn approving_a_device_should_need_a_fresh_passkey_check() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let app = &t.instance.app;
    let paired = call(
        app,
        "POST",
        "/api/v1/auth/device",
        &[],
        Some(json!({"client_name": "cli", "scopes": ["read"]})),
    )
    .await;
    assert_eq!(paired.status, StatusCode::OK, "{}", paired.body);
    let user_code = paired.body["user_code"].as_str().expect("user code");
    let form = format!("user_code={user_code}&scope=read&expires_in_days=1");
    for cookie in [&t.stale, &t.never] {
        let (status, body) = form_post(app, cookie, "/device/approve", form.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "reauth_required");
    }
    let (status, body) = form_post(app, &t.fresh, "/device/approve", form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn reauth_should_stamp_only_the_session_that_answered() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let app = &t.instance.app;
    let uri = "/api/v1/account/recovery-code";
    let refused = call(app, "POST", uri, &[("cookie", &t.stale)], None).await;
    assert_reauth_required(&refused);

    let checked = reauth(app, &t.stale, &mut fx.member_key).await;
    assert_eq!(checked.status, StatusCode::OK, "{}", checked.body);
    assert_eq!(checked.body["fresh_for_seconds"], REAUTH_WINDOW_SECS);
    let allowed = call(app, "POST", uri, &[("cookie", &t.stale)], None).await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);
    let other = call(app, "POST", uri, &[("cookie", &t.never)], None).await;
    assert_reauth_required(&other);
}

#[tokio::test]
async fn reauth_should_refuse_another_accounts_passkey() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let app = &t.instance.app;
    let refused = reauth(app, &t.stale, &mut fx.admin_key).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{}", refused.body);
    let still = call(
        app,
        "POST",
        "/api/v1/account/recovery-code",
        &[("cookie", &t.stale)],
        None,
    )
    .await;
    assert_reauth_required(&still);
}

#[tokio::test]
async fn reauth_should_refuse_an_api_token() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let bearer = format!("Bearer {}", mint_token(app, &fx.member).await);
    for uri in ["/api/v1/auth/reauth/options", "/api/v1/auth/reauth"] {
        let refused = call(app, "POST", uri, &[("authorization", &bearer)], None).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(refused.body["code"], "session_required", "{uri}");
    }
}

#[tokio::test]
async fn recovery_session_should_add_a_passkey_within_the_window() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let mut replacement = SoftAuthenticator::new(41);
    let started = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": fx.member_code}),
    )
    .await;
    let recovered = finish(app, "/api/v1/auth/recover", &started, &mut replacement).await;
    assert_eq!(recovered.status, StatusCode::OK, "{}", recovered.body);
    assert!(!recovery_code(&recovered).is_empty());
    let session = session_of(&recovered);
    let cookie = [("cookie", session.as_str())];
    let added = add_passkey(app, &mut SoftAuthenticator::new(42), &cookie, "Phone").await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
}

/// Opens a single-mode passkey server with one admin, and returns the server with the cookie of a
/// session of that admin.
fn single_server() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("single.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let admin = db.ensure_owner("admin").expect("admin");
    let session = db.create_session(&admin.account_id()).expect("session");
    let app = router(AppState::new(db, None, passkey_mode()).with_limits(unthrottled_limits()));
    (dir, app, format!("session={session}"))
}

#[tokio::test]
async fn single_mode_should_answer_404_before_any_passkey_check() {
    let (_dir, app, cookie) = single_server();
    let headers = [("cookie", cookie.as_str())];
    let delete = Some(json!({"handle": "admin"}));
    for (method, uri, body) in [
        ("POST", "/api/v1/account/recovery-code", None),
        ("POST", "/api/v1/account/delete", delete),
        ("DELETE", "/api/v1/admin/accounts/any", None),
    ] {
        let refused = call(&app, method, uri, &headers, body).await;
        assert_eq!(
            refused.status,
            StatusCode::NOT_FOUND,
            "{uri}: {}",
            refused.body
        );
    }
}

#[tokio::test]
async fn stale_member_should_be_forbidden_from_deleting_a_member_as_the_admin() {
    let mut fx = pair(None).await;
    let t = tiers(fx.instance, &mut fx.member_key).await;
    let bob = t.instance.account("bob");
    let uri = format!("/api/v1/admin/accounts/{}", bob.as_str());
    let refused = call(
        &t.instance.app,
        "DELETE",
        &uri,
        &[("cookie", &t.stale)],
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(
        refused.code.as_deref(),
        Some("forbidden"),
        "{}",
        refused.body
    );
}

#[tokio::test]
async fn reauth_should_answer_busy_for_the_sixth_start_from_one_network_and_leave_sign_in_alone() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let headers = [("cookie", fx.member.as_str())];
    for _ in 0..MAX_CEREMONIES_PER_NETWORK {
        let started = call(app, "POST", "/api/v1/auth/reauth/options", &headers, None).await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    }
    let busy = call(app, "POST", "/api/v1/auth/reauth/options", &headers, None).await;
    assert_eq!(busy.status, StatusCode::TOO_MANY_REQUESTS, "{}", busy.body);
    assert_eq!(busy.code.as_deref(), Some("busy"), "{}", busy.body);
    let sign_in = call(app, "POST", "/api/v1/auth/sign-in/options", &[], None).await;
    assert_eq!(sign_in.status, StatusCode::OK, "{}", sign_in.body);
}
