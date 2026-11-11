use axum::http::StatusCode;
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::{Value, json};

use super::router;
use super::server::{TEST_PUBLIC_URL, passkey_mode};
use super::test_support::{
    Reply, SoftAuthenticator, add_passkey, call, held_disk, none_mode, session_of, sign_in,
};
use crate::AppState;
use crate::auth::{SESSION_MAX_AGE_SECONDS, new_api_token_record};
use crate::csrf::REFUSED_BODY;
use crate::db::{Db, MAX_PASSKEYS_PER_ACCOUNT};
use crate::models::{AccountId, AuthScope, PasskeyRow};

const USERS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("users");
const USERNAMES: TableDefinition<'_, &str, &str> = TableDefinition::new("usernames");
const META: TableDefinition<'_, &str, u64> = TableDefinition::new("meta");
const SESSIONS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("sessions");
const STALE_SESSION: &str = "5e5510a1-0000-4000-8000-000000000007";

/// Returns the passkeys of the instance's owner, oldest first.
fn owner_passkeys(db: &Db) -> Vec<PasskeyRow> {
    let owner = db.owner().expect("read").expect("owner");
    db.passkeys_of(&owner.account_id()).expect("passkeys")
}

fn open_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("passkeys.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

fn passkey_app(db: Db) -> (axum::Router, String) {
    let sign_in = passkey_mode();
    let code = sign_in
        .relying_party()
        .expect("relying party")
        .issue_setup_code();
    (
        router(AppState::new(db.clone(), None, sign_in).with_disk(held_disk(&db))),
        code,
    )
}

async fn set_up(
    app: &axum::Router,
    code: &str,
    authenticator: &mut SoftAuthenticator,
    mut start: Value,
) -> Reply {
    start["code"] = code.into();
    let started = call(app, "POST", "/api/v1/auth/setup/options", &[], Some(start)).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let credential = authenticator.register(&started.body["options"]);
    call(
        app,
        "POST",
        "/api/v1/auth/setup",
        &[],
        Some(json!({
            "ceremony": started.body["ceremony"],
            "name": "Laptop",
            "credential": credential,
        })),
    )
    .await
}

async fn whoami(app: &axum::Router, headers: &[(&str, &str)]) -> Reply {
    call(app, "GET", "/api/v1/auth/whoami", headers, None).await
}

#[tokio::test]
async fn passkey_setup_should_claim_the_instance_and_sign_in_with_the_same_passkey() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut authenticator = SoftAuthenticator::new(7);

    let page = call(&app, "GET", "/", &[], None).await;
    assert_eq!(page.location.as_deref(), Some("/setup"));
    let login = call(&app, "GET", "/login", &[], None).await;
    assert_eq!(login.location.as_deref(), Some("/setup"));
    let setup_page = call(&app, "GET", "/setup", &[], None).await;
    assert_eq!(setup_page.status, StatusCode::OK);

    let missing = call(
        &app,
        "POST",
        "/api/v1/auth/setup/options",
        &[],
        Some(json!({ "code": code })),
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);

    let setup = set_up(
        &app,
        &code,
        &mut authenticator,
        json!({ "username": "chip" }),
    )
    .await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    assert_eq!(setup.body["redirect"], "/");
    let session = session_of(&setup);
    let me = whoami(&app, &[("cookie", &session)]).await;
    assert_eq!(me.body["kind"], "session");
    assert_eq!(me.body["name"], "chip");

    let setup_page = call(&app, "GET", "/setup", &[], None).await;
    assert_eq!(setup_page.location.as_deref(), Some("/"));
    let again = call(
        &app,
        "POST",
        "/api/v1/auth/setup/options",
        &[],
        Some(json!({ "username": "mallory" })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    let login = call(&app, "GET", "/login", &[], None).await;
    assert_eq!(login.status, StatusCode::OK);
    let anonymous = call(&app, "GET", "/", &[], None).await;
    assert_eq!(anonymous.location.as_deref(), Some("/login"));

    for _ in 0..2 {
        let signed_in = sign_in(&app, &mut authenticator, &[]).await;
        assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
        let session = session_of(&signed_in);
        assert_eq!(
            whoami(&app, &[("cookie", &session)]).await.body["name"],
            "chip"
        );
    }
    let passkeys = owner_passkeys(&db);
    assert_eq!(passkeys.len(), 1);
    assert_eq!(passkeys[0].name, "Laptop");
    assert!(passkeys[0].last_used_at.is_some());
    assert_eq!(passkeys[0].credential["cred"]["counter"], 2);
}

#[tokio::test]
async fn sign_in_should_refuse_a_replayed_or_foreign_passkey() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db);
    let mut owner = SoftAuthenticator::new(7);
    assert_eq!(
        set_up(&app, &code, &mut owner, json!({ "username": "chip" }))
            .await
            .status,
        StatusCode::OK
    );

    let started = call(&app, "POST", "/api/v1/auth/sign-in/options", &[], None).await;
    let credential = owner.sign_in(&started.body["options"]);
    let finish = json!({ "ceremony": started.body["ceremony"], "credential": credential });
    let first = call(
        &app,
        "POST",
        "/api/v1/auth/sign-in",
        &[],
        Some(finish.clone()),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let replay = call(&app, "POST", "/api/v1/auth/sign-in", &[], Some(finish)).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST);
    assert!(replay.cookie.is_none());

    let mut stranger = SoftAuthenticator::new(9);
    stranger.user_handle = owner.user_handle.clone();
    let foreign = sign_in(&app, &mut stranger, &[]).await;
    assert_eq!(foreign.status, StatusCode::UNAUTHORIZED);
    assert!(foreign.cookie.is_none());
}

#[tokio::test]
async fn sign_in_cookie_should_be_secure_only_over_https() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db);
    let mut authenticator = SoftAuthenticator::new(7);
    set_up(
        &app,
        &code,
        &mut authenticator,
        json!({ "username": "chip" }),
    )
    .await;

    let plain = sign_in(&app, &mut authenticator, &[]).await;
    let plain_cookie = plain.cookie.expect("set-cookie");
    assert!(!plain_cookie.contains("Secure"), "{plain_cookie}");
    assert!(plain_cookie.contains("HttpOnly"), "{plain_cookie}");
    let max_age = format!("Max-Age={SESSION_MAX_AGE_SECONDS}");
    assert!(plain_cookie.contains(&max_age), "{plain_cookie}");
    let https = sign_in(&app, &mut authenticator, &[("x-forwarded-proto", "https")]).await;
    let https_cookie = https.cookie.expect("set-cookie");
    assert!(https_cookie.contains("; Secure"), "{https_cookie}");
}

#[tokio::test]
async fn setup_should_let_only_one_of_two_racing_ceremonies_claim_the_instance() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut first = SoftAuthenticator::new(7);
    let mut second = SoftAuthenticator::new(8);
    let start = || {
        call(
            &app,
            "POST",
            "/api/v1/auth/setup/options",
            &[],
            Some(json!({ "username": "chip", "code": code })),
        )
    };
    let (one, two) = (start().await, start().await);
    let finish = |ceremony: &Value, credential: Value| {
        call(
            &app,
            "POST",
            "/api/v1/auth/setup",
            &[],
            Some(json!({ "ceremony": ceremony, "credential": credential })),
        )
    };
    let won = finish(&one.body["ceremony"], first.register(&one.body["options"])).await;
    assert_eq!(won.status, StatusCode::OK);
    let lost = finish(&two.body["ceremony"], second.register(&two.body["options"])).await;
    assert_eq!(lost.status, StatusCode::CONFLICT);
    assert!(lost.cookie.is_none());
    assert_eq!(owner_passkeys(&db).len(), 1);
    assert_eq!(db.user_count().expect("users"), 1);
}

#[tokio::test]
async fn passkeys_should_stop_at_their_cap_per_account() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut laptop = SoftAuthenticator::new(7);
    let session =
        session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let cookie = [("cookie", session.as_str())];
    for n in 1..MAX_PASSKEYS_PER_ACCOUNT {
        let mut key = SoftAuthenticator::new(100 + n as u8);
        let added = add_passkey(&app, &mut key, &cookie, &format!("Key {n}")).await;
        assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    }
    let refused = call(&app, "POST", "/api/v1/auth/passkeys/options", &cookie, None).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("quota_exceeded"));
    assert_eq!(owner_passkeys(&db).len(), MAX_PASSKEYS_PER_ACCOUNT);
}

#[tokio::test]
async fn last_passkey_should_not_be_removable() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut laptop = SoftAuthenticator::new(7);
    let session =
        session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let cookie = [("cookie", session.as_str())];
    let first_id = owner_passkeys(&db)[0].id.clone();

    let refused = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/passkeys/{first_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);

    let duplicate = add_passkey(&app, &mut laptop, &cookie, "Laptop again").await;
    assert_eq!(duplicate.status, StatusCode::BAD_REQUEST);
    let mut phone = SoftAuthenticator::new(8);
    let added = add_passkey(&app, &mut phone, &cookie, "Phone").await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    assert_eq!(added.body["name"], "Phone");
    let listed = call(&app, "GET", "/api/v1/auth/passkeys", &cookie, None).await;
    assert_eq!(listed.body.as_array().expect("list").len(), 2);
    assert!(listed.body[0].get("credential").is_none());

    let removed = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/passkeys/{first_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK);
    let phone_id = added.body["id"].as_str().expect("id").to_string();
    let last = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/passkeys/{phone_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(last.status, StatusCode::CONFLICT);
    assert_eq!(sign_in(&app, &mut phone, &[]).await.status, StatusCode::OK);
    assert_eq!(
        sign_in(&app, &mut laptop, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn removed_passkey_should_free_its_credential_for_registration_again() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut laptop = SoftAuthenticator::new(7);
    let session =
        session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let cookie = [("cookie", session.as_str())];
    let first_id = owner_passkeys(&db)[0].id.clone();
    let mut phone = SoftAuthenticator::new(8);
    let added = add_passkey(&app, &mut phone, &cookie, "Phone").await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let removed = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/passkeys/{first_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);

    let again = add_passkey(&app, &mut laptop, &cookie, "Laptop again").await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
}

#[tokio::test]
async fn removing_a_passkey_should_end_every_other_session() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut laptop = SoftAuthenticator::new(7);
    let here = session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let cookie = [("cookie", here.as_str())];
    let mut phone = SoftAuthenticator::new(8);
    let added = add_passkey(&app, &mut phone, &cookie, "Phone").await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let stolen = session_of(&sign_in(&app, &mut phone, &[]).await);
    let stolen_cookie = [("cookie", stolen.as_str())];
    assert_eq!(whoami(&app, &stolen_cookie).await.status, StatusCode::OK);

    let phone_id = added.body["id"].as_str().expect("id");
    let removed = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/passkeys/{phone_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    assert_eq!(
        whoami(&app, &stolen_cookie).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(whoami(&app, &cookie).await.status, StatusCode::OK);
}

#[tokio::test]
async fn sign_out_other_sessions_should_keep_only_this_browser() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut laptop = SoftAuthenticator::new(7);
    let here = session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let cookie = [("cookie", here.as_str())];
    let other = session_of(&sign_in(&app, &mut laptop, &[]).await);
    let other_cookie = [("cookie", other.as_str())];

    let ended = call(
        &app,
        "POST",
        "/api/v1/auth/sessions/sign-out-others",
        &cookie,
        None,
    )
    .await;
    assert_eq!(ended.status, StatusCode::OK, "{}", ended.body);
    assert_eq!(ended.body["ended"], 1);
    assert_eq!(whoami(&app, &cookie).await.status, StatusCode::OK);
    assert_eq!(
        whoami(&app, &other_cookie).await.status,
        StatusCode::UNAUTHORIZED
    );

    let cross_site = [("cookie", here.as_str()), ("sec-fetch-site", "cross-site")];
    let refused = call(
        &app,
        "POST",
        "/api/v1/auth/sessions/sign-out-others",
        &cross_site,
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let page = call(&app, "GET", "/settings", &cookie, None).await;
    assert!(page.body.to_string().contains("Sign out other sessions"));
}

const FIREFOX_ON_LINUX: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
const SAFARI_ON_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

#[tokio::test]
async fn sessions_list_should_show_each_browser_and_mark_the_current_one() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db);
    let mut laptop = SoftAuthenticator::new(7);
    let first = session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let phone = session_of(&sign_in(&app, &mut laptop, &[("user-agent", SAFARI_ON_IPHONE)]).await);
    let desk = session_of(&sign_in(&app, &mut laptop, &[("user-agent", FIREFOX_ON_LINUX)]).await);

    let listed = call(
        &app,
        "GET",
        "/api/v1/auth/sessions",
        &[("cookie", desk.as_str())],
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.body.as_array().expect("array");
    assert_eq!(rows.len(), 3);
    let devices: Vec<&str> = rows
        .iter()
        .map(|row| row["device"].as_str().unwrap())
        .collect();
    assert!(devices.contains(&"Firefox on Linux"), "{devices:?}");
    assert!(devices.contains(&"Safari on iOS"), "{devices:?}");
    assert!(devices.contains(&""), "{devices:?}");
    let current: Vec<&Value> = rows.iter().filter(|row| row["current"] == true).collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["device"], "Firefox on Linux");
    assert!(
        rows.iter()
            .all(|row| row["created_at"].is_string() && row["id"].is_string())
    );
    assert!(!listed.body.to_string().contains(&first));
    assert!(!listed.body.to_string().contains(&phone));

    let page = call(&app, "GET", "/settings", &[("cookie", desk.as_str())], None).await;
    let html = page.body.to_string();
    assert!(html.contains("Safari on iOS"), "{html}");
    assert!(html.contains("[Your current session]"));
    assert_eq!(html.matches("session-revoke\\\"").count(), 2, "{html}");
}

#[tokio::test]
async fn revoking_a_session_should_sign_out_only_that_browser() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db);
    let mut laptop = SoftAuthenticator::new(7);
    let here = session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let phone = session_of(&sign_in(&app, &mut laptop, &[("user-agent", SAFARI_ON_IPHONE)]).await);
    let third = session_of(&sign_in(&app, &mut laptop, &[]).await);
    let cookie = [("cookie", here.as_str())];
    let phone_cookie = [("cookie", phone.as_str())];
    let third_cookie = [("cookie", third.as_str())];

    let listed = call(&app, "GET", "/api/v1/auth/sessions", &cookie, None).await;
    let rows = listed.body.as_array().expect("array");
    let phone_id = rows
        .iter()
        .find(|row| row["device"] == "Safari on iOS")
        .expect("phone row")["id"]
        .as_str()
        .expect("id")
        .to_string();
    let own_id = rows
        .iter()
        .find(|row| row["current"] == true)
        .expect("own row")["id"]
        .as_str()
        .expect("id")
        .to_string();

    let own = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/sessions/{own_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(own.status, StatusCode::CONFLICT, "{}", own.body);
    let unknown = call(
        &app,
        "DELETE",
        "/api/v1/auth/sessions/nothing",
        &cookie,
        None,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let cross_site = [("cookie", here.as_str()), ("sec-fetch-site", "cross-site")];
    let refused = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/sessions/{phone_id}"),
        &cross_site,
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(whoami(&app, &phone_cookie).await.status, StatusCode::OK);

    let revoked = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/sessions/{phone_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body["revoked"], phone_id);
    assert_eq!(
        whoami(&app, &phone_cookie).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(whoami(&app, &cookie).await.status, StatusCode::OK);
    assert_eq!(whoami(&app, &third_cookie).await.status, StatusCode::OK);
    let again = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/sessions/{phone_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn passkey_management_should_need_the_browser_session() {
    let (_dir, db) = open_db();
    let (record, raw) =
        new_api_token_record(&AccountId::ownerless(), "admin-bot", AuthScope::Admin, None);
    db.create_api_token(&record).expect("token");
    let (app, _) = passkey_app(db);
    let bearer = format!("Bearer {raw}");
    let headers = [("authorization", bearer.as_str())];
    for (method, uri) in [
        ("POST", "/api/v1/auth/passkeys/options"),
        ("GET", "/api/v1/auth/passkeys"),
        ("DELETE", "/api/v1/auth/passkeys/anything"),
        ("GET", "/api/v1/auth/sessions"),
        ("DELETE", "/api/v1/auth/sessions/anything"),
    ] {
        let reply = call(&app, method, uri, &headers, None).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.body["code"], "session_required", "{method} {uri}");
    }
}

#[tokio::test]
async fn password_era_database_should_open_and_register_its_users_first_passkey() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("v7.redb");
    let user_id = "0b5e9a1e-2a41-4a7c-9d3b-5d6e7f809a1b";
    {
        let file = redb::Database::create(&path).expect("create");
        let txn = file.begin_write().expect("write");
        txn.open_table(META)
            .expect("meta")
            .insert("schema_version", 7u64)
            .expect("stamp");
        let row = json!({
            "id": user_id,
            "username": "admin",
            "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
        })
        .to_string();
        txn.open_table(USERS)
            .expect("users")
            .insert(user_id, row.as_bytes())
            .expect("user");
        txn.open_table(USERNAMES)
            .expect("usernames")
            .insert("admin", user_id)
            .expect("username");
        let session = json!({ "user_id": user_id, "created_at": "2026-10-01T00:00:00Z" });
        txn.open_table(SESSIONS)
            .expect("sessions")
            .insert(STALE_SESSION, session.to_string().as_bytes())
            .expect("session");
        txn.commit().expect("commit");
    }

    let path = path.to_str().expect("utf8 path");
    drop(Db::connect(path).expect("upgrade"));
    let stored: Value = {
        let file = redb::ReadOnlyDatabase::open(path).expect("reopen");
        let txn = file.begin_read().expect("read");
        let users = txn.open_table(USERS).expect("users");
        let row = users.get(user_id).expect("get").expect("row");
        serde_json::from_slice(row.value()).expect("json")
    };
    assert_eq!(
        stored,
        json!({
            "id": user_id,
            "username": "admin",
            "role": "admin",
            "status": "active",
            "created_at": stored["created_at"],
        })
    );
    let sessions_left = {
        let file = redb::ReadOnlyDatabase::open(path).expect("reopen");
        let txn = file.begin_read().expect("read");
        let sessions = txn.open_table(SESSIONS).expect("sessions");
        sessions.iter().expect("iter").count()
    };
    assert_eq!(sessions_left, 0);

    let db = Db::connect(path).expect("connect");
    assert_eq!(db.owner().expect("owner").expect("admin").id, user_id);

    let (app, code) = passkey_app(db.clone());
    let stale = format!("session={STALE_SESSION}");
    let stale_cookie = [("cookie", stale.as_str())];
    assert_eq!(
        whoami(&app, &stale_cookie).await.status,
        StatusCode::UNAUTHORIZED
    );
    let refused = call(
        &app,
        "POST",
        "/api/v1/auth/setup/options",
        &stale_cookie,
        Some(json!({})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let setup_page = call(&app, "GET", "/setup", &stale_cookie, None).await;
    assert_eq!(setup_page.status, StatusCode::OK);
    assert!(
        setup_page
            .body
            .as_str()
            .expect("html")
            .contains(r#"id="setup-code""#)
    );
    let mut authenticator = SoftAuthenticator::new(7);
    let setup = set_up(
        &app,
        &code,
        &mut authenticator,
        json!({ "username": "ignored" }),
    )
    .await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    assert_eq!(authenticator.user_handle, uuid_bytes(user_id));
    let session = session_of(&setup);
    assert_eq!(
        whoami(&app, &[("cookie", &session)]).await.body["name"],
        "admin"
    );
    assert_eq!(db.user_count().expect("users"), 1);
    assert_eq!(
        sign_in(&app, &mut authenticator, &[]).await.status,
        StatusCode::OK
    );
    let board = call(&app, "GET", "/", &stale_cookie, None).await;
    assert_eq!(board.location.as_deref(), Some("/login"));
}

fn uuid_bytes(id: &str) -> Vec<u8> {
    uuid::Uuid::parse_str(id).expect("uuid").as_bytes().to_vec()
}

#[tokio::test]
async fn none_mode_should_treat_a_cookieless_browser_as_the_owner() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db.clone(), None, none_mode(None)).with_disk(held_disk(&db)));
    let me = whoami(&app, &[("host", "127.0.0.1:3000")]).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["kind"], "session");
    assert_eq!(me.body["name"], "owner");
    assert_eq!(me.body["scope"], "admin");
    assert_eq!(db.user_count().expect("users"), 1);

    for uri in ["/settings", "/device"] {
        let page = call(&app, "GET", uri, &[("host", "localhost:3000")], None).await;
        assert_eq!(page.status, StatusCode::OK, "{uri}");
    }
    let settings = call(&app, "GET", "/settings", &[], None).await;
    let html = settings.body.as_str().expect("html");
    assert!(!html.contains("Sign out"));
    assert!(html.contains("Adding a passkey needs"));

    let minted = call(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        &[("sec-fetch-site", "same-origin")],
        Some(json!({ "name": "agent", "scopes": ["read"] })),
    )
    .await;
    assert_eq!(minted.status, StatusCode::OK, "{}", minted.body);
    let listed = call(&app, "GET", "/api/v1/auth/tokens", &[], None).await;
    assert_eq!(listed.body.as_array().expect("tokens").len(), 1);
}

#[tokio::test]
async fn none_mode_should_still_check_bearer_tokens() {
    let (_dir, db) = open_db();
    let (record, raw) =
        new_api_token_record(&AccountId::ownerless(), "reader", AuthScope::Read, None);
    db.create_api_token(&record).expect("token");
    let app = router(AppState::new(db.clone(), None, none_mode(None)).with_disk(held_disk(&db)));

    for header in [
        "Bearer pomb_wrong",
        "",
        "Bearer",
        "Bearer ",
        "Basic YWRtaW46cHc=",
        "garbage",
    ] {
        let bad = whoami(&app, &[("authorization", header)]).await;
        assert_eq!(bad.status, StatusCode::UNAUTHORIZED, "{header:?}");
        assert_eq!(bad.body["code"], "token_invalid", "{header:?}");
        let write = call(
            &app,
            "POST",
            "/api/v1/auth/tokens",
            &[("authorization", header), ("sec-fetch-site", "same-origin")],
            Some(json!({ "name": "x", "scopes": ["admin"] })),
        )
        .await;
        assert_eq!(write.status, StatusCode::UNAUTHORIZED, "{header:?}");
    }

    let bearer = format!("Bearer {raw}");
    let reader = [("authorization", bearer.as_str())];
    assert_eq!(whoami(&app, &reader).await.body["kind"], "token");
    let write = call(
        &app,
        "POST",
        "/api/boards",
        &reader,
        Some(json!({ "name": "Nope" })),
    )
    .await;
    assert_eq!(write.status, StatusCode::FORBIDDEN);
    assert_eq!(write.body["code"], "permission_denied");
    assert_eq!(write.body["permission"], "boards.create");
    let mint = call(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        &reader,
        Some(json!({ "name": "x", "scopes": ["read"] })),
    )
    .await;
    assert_eq!(mint.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn none_mode_should_refuse_a_cross_site_write() {
    let (_dir, db) = open_db();
    let app = router(AppState::new(db.clone(), None, none_mode(None)).with_disk(held_disk(&db)));
    for headers in [
        [("sec-fetch-site", "cross-site"), ("host", "127.0.0.1:3000")],
        [
            ("origin", "https://evil.example"),
            ("host", "127.0.0.1:3000"),
        ],
    ] {
        let reply = call(
            &app,
            "POST",
            "/api/v1/auth/tokens",
            &headers,
            Some(json!({ "name": "stolen", "scopes": ["admin"] })),
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{headers:?}");
        assert_eq!(reply.body["message"], REFUSED_BODY);
    }
    assert!(db.list_api_tokens().expect("tokens").is_empty());
}

#[tokio::test]
async fn none_mode_should_refuse_a_host_that_is_not_its_own() {
    let (_dir, db) = open_db();
    let bare = router(AppState::new(db.clone(), None, none_mode(None)).with_disk(held_disk(&db)));
    let rebound = whoami(&bare, &[("host", "evil.example")]).await;
    assert_eq!(rebound.status, StatusCode::FORBIDDEN);
    assert_eq!(rebound.body["code"], "forbidden");
    for host in ["127.0.0.1", "127.1.2.3:3000", "[::1]:3000", "LOCALHOST"] {
        assert_eq!(
            whoami(&bare, &[("host", host)]).await.status,
            StatusCode::OK,
            "{host}"
        );
    }
    let public = router(
        AppState::new(db.clone(), None, none_mode(Some(TEST_PUBLIC_URL))).with_disk(held_disk(&db)),
    );
    assert_eq!(
        whoami(&public, &[("host", "pomb.test")]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        whoami(&public, &[("host", "evil.example")]).await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn none_mode_should_send_login_setup_and_logout_home() {
    let (_dir, db) = open_db();
    let app = router(
        AppState::new(db.clone(), None, none_mode(Some(TEST_PUBLIC_URL))).with_disk(held_disk(&db)),
    );
    for (method, uri) in [("GET", "/login"), ("GET", "/setup"), ("POST", "/logout")] {
        let reply = call(&app, method, uri, &[], None).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{method} {uri}");
        assert_eq!(reply.location.as_deref(), Some("/"), "{method} {uri}");
    }
    for uri in ["/api/v1/auth/sign-in/options", "/api/v1/auth/setup/options"] {
        let reply = call(&app, "POST", uri, &[], Some(json!({}))).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn passkey_mode_login_and_setup_pages_should_follow_the_passkey_count() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db);
    let page = |uri: &'static str| call(&app, "GET", uri, &[], None);
    assert_eq!(page("/setup").await.status, StatusCode::OK);
    assert_eq!(page("/login").await.location.as_deref(), Some("/setup"));
    let mut authenticator = SoftAuthenticator::new(7);
    set_up(
        &app,
        &code,
        &mut authenticator,
        json!({ "username": "chip" }),
    )
    .await;
    let login = page("/login").await;
    assert_eq!(login.status, StatusCode::OK);
    assert!(
        login
            .body
            .as_str()
            .expect("html")
            .contains("Sign in with a passkey")
    );
    assert_eq!(page("/setup").await.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn the_login_and_setup_pages_should_point_agents_to_the_skill() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db);
    let setup = call(&app, "GET", "/setup", &[], None).await;
    let mut authenticator = SoftAuthenticator::new(7);
    set_up(
        &app,
        &code,
        &mut authenticator,
        json!({ "username": "chip" }),
    )
    .await;
    let login = call(&app, "GET", "/login", &[], None).await;
    for page in [setup, login] {
        let html = page.body.as_str().expect("html");
        assert!(
            html.contains(r#"<link rel="help" href="/llms.txt">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<link rel="alternate" type="text/markdown" href="/skill.md">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"For agents: <a href="/skill.md">/skill.md</a>"#),
            "{html}"
        );
    }
}

#[tokio::test]
async fn recovery_should_add_a_passkey_without_sign_in_that_works_with_passkeys() {
    let (_dir, db) = open_db();
    let (first_app, code) = passkey_app(db.clone());
    let mut lost = SoftAuthenticator::new(7);
    set_up(&first_app, &code, &mut lost, json!({ "username": "chip" })).await;

    let open_app = router(
        AppState::new(db.clone(), None, none_mode(Some(TEST_PUBLIC_URL))).with_disk(held_disk(&db)),
    );
    let settings = call(&open_app, "GET", "/settings", &[], None).await;
    assert!(
        settings
            .body
            .as_str()
            .expect("html")
            .contains("passkey-add-form")
    );
    let mut replacement = SoftAuthenticator::new(8);
    let added = add_passkey(&open_app, &mut replacement, &[], "Replacement").await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    assert_eq!(replacement.user_handle, lost.user_handle);

    let (back, _) = passkey_app(db);
    let signed_in = sign_in(&back, &mut replacement, &[]).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    let session = session_of(&signed_in);
    assert_eq!(
        whoami(&back, &[("cookie", &session)]).await.body["name"],
        "chip"
    );
}

#[tokio::test]
async fn setup_should_need_the_setup_code_and_spend_it() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let start = |body: Value| call(&app, "POST", "/api/v1/auth/setup/options", &[], Some(body));

    let without = start(json!({ "username": "chip" })).await;
    assert_eq!(without.status, StatusCode::FORBIDDEN);
    let wrong = start(json!({ "username": "chip", "code": "BCDF-GHJK" })).await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    assert_eq!(db.user_count().expect("users"), 0);

    let typed = code.to_lowercase().replace('-', " ");
    let mut first = SoftAuthenticator::new(7);
    let setup = set_up(&app, &typed, &mut first, json!({ "username": "chip" })).await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);

    let (_dir, other) = open_db();
    other.create_user("chip").expect("user");
    let sign_in = passkey_mode();
    let relying_party = sign_in.relying_party().expect("relying party").clone();
    let stale = relying_party.issue_setup_code();
    let app = router(AppState::new(other.clone(), None, sign_in).with_disk(held_disk(&other)));
    let mut second = SoftAuthenticator::new(8);
    let used = set_up(&app, &stale, &mut second, json!({})).await;
    assert_eq!(used.status, StatusCode::OK, "{}", used.body);
    assert!(!relying_party.setup_code_matches(&stale));
}

#[tokio::test]
async fn setup_should_need_the_code_even_with_a_session_cookie() {
    let (_dir, db) = open_db();
    let user_id = db.create_user("admin").expect("user");
    let session = db.create_session(&user_id, "").expect("session");
    let cookie = format!("session={session}");
    let (app, code) = passkey_app(db.clone());

    let page = call(&app, "GET", "/setup", &[("cookie", &cookie)], None).await;
    assert!(
        page.body
            .as_str()
            .expect("html")
            .contains(r#"id="setup-code""#)
    );
    let refused = call(
        &app,
        "POST",
        "/api/v1/auth/setup/options",
        &[("cookie", &cookie)],
        Some(json!({})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let mut authenticator = SoftAuthenticator::new(7);
    let setup = set_up(&app, &code, &mut authenticator, json!({})).await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    assert_eq!(owner_passkeys(&db).len(), 1);
}
