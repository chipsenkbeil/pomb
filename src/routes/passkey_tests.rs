use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::router;
use super::server::{TEST_PUBLIC_URL, passkey_mode};
use crate::AppState;
use crate::api_error::ErrorCode;
use crate::auth::{AuthChoice, AuthMode, SESSION_MAX_AGE_SECONDS, SignIn, new_api_token_record};
use crate::csrf::REFUSED_BODY;
use crate::db::Db;
use crate::models::{AccountId, AuthScope};

const MAX_BODY_BYTES: usize = 1 << 20;
const RP_ID: &str = "pomb.test";
const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_USER_VERIFIED: u8 = 0x04;
const FLAG_ATTESTED_DATA: u8 = 0x40;
const USERS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("users");
const USERNAMES: TableDefinition<'_, &str, &str> = TableDefinition::new("usernames");
const META: TableDefinition<'_, &str, u64> = TableDefinition::new("meta");
const SESSIONS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("sessions");
const STALE_SESSION: &str = "5e5510a1-0000-4000-8000-000000000007";

/// A software authenticator holding one P-256 passkey, which speaks WebAuthn's JSON the way the
/// browser script sends it.
pub(super) struct SoftAuthenticator {
    key: SigningKey,
    credential_id: Vec<u8>,
    user_handle: Vec<u8>,
    counter: u32,
    origin: String,
}

impl SoftAuthenticator {
    pub(super) fn new(seed: u8) -> Self {
        Self {
            key: SigningKey::from_slice(&[seed; 32]).expect("signing key"),
            credential_id: vec![seed; 16],
            user_handle: Vec::new(),
            counter: 0,
            origin: TEST_PUBLIC_URL.to_string(),
        }
    }

    fn authenticator_data(&self, flags: u8) -> Vec<u8> {
        let mut data = Sha256::digest(RP_ID.as_bytes()).to_vec();
        data.push(flags);
        data.extend_from_slice(&self.counter.to_be_bytes());
        data
    }

    fn client_data(&self, kind: &str, options: &Value) -> Vec<u8> {
        json!({
            "type": kind,
            "challenge": options["publicKey"]["challenge"],
            "origin": self.origin,
            "crossOrigin": false,
        })
        .to_string()
        .into_bytes()
    }

    fn cose_key(&self) -> Vec<u8> {
        let point = self.key.verifying_key().to_encoded_point(false);
        let mut key = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        key.extend_from_slice(point.x().expect("x"));
        key.extend_from_slice(&[0x22, 0x58, 0x20]);
        key.extend_from_slice(point.y().expect("y"));
        key
    }

    pub(super) fn register(&mut self, options: &Value) -> Value {
        let user_id = options["publicKey"]["user"]["id"]
            .as_str()
            .expect("user id");
        self.user_handle = URL_SAFE_NO_PAD.decode(user_id).expect("user id base64url");
        let mut auth_data =
            self.authenticator_data(FLAG_USER_PRESENT | FLAG_USER_VERIFIED | FLAG_ATTESTED_DATA);
        auth_data.extend_from_slice(&[0; 16]);
        auth_data.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
        auth_data.extend_from_slice(&self.credential_id);
        auth_data.extend_from_slice(&self.cose_key());

        let mut attestation = vec![0xa3, 0x63];
        attestation.extend_from_slice(b"fmt");
        attestation.push(0x64);
        attestation.extend_from_slice(b"none");
        attestation.push(0x67);
        attestation.extend_from_slice(b"attStmt");
        attestation.push(0xa0);
        attestation.push(0x68);
        attestation.extend_from_slice(b"authData");
        attestation.extend_from_slice(&[0x58, auth_data.len() as u8]);
        attestation.extend_from_slice(&auth_data);

        let id = URL_SAFE_NO_PAD.encode(&self.credential_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "clientDataJSON": URL_SAFE_NO_PAD.encode(self.client_data("webauthn.create", options)),
                "attestationObject": URL_SAFE_NO_PAD.encode(attestation),
                "transports": ["internal"],
            },
            "extensions": {},
        })
    }

    pub(super) fn sign_in(&mut self, options: &Value) -> Value {
        self.counter += 1;
        let auth_data = self.authenticator_data(FLAG_USER_PRESENT | FLAG_USER_VERIFIED);
        let client_data = self.client_data("webauthn.get", options);
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(&client_data));
        let signature: Signature = self.key.sign(&signed);
        let id = URL_SAFE_NO_PAD.encode(&self.credential_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "clientDataJSON": URL_SAFE_NO_PAD.encode(client_data),
                "authenticatorData": URL_SAFE_NO_PAD.encode(auth_data),
                "signature": URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes()),
                "userHandle": URL_SAFE_NO_PAD.encode(&self.user_handle),
            },
            "extensions": {},
        })
    }
}

pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) location: Option<String>,
    pub(super) cookie: Option<String>,
    pub(super) body: Value,
    /// The error code the response carries, whether or not its body is JSON.
    pub(super) code: Option<String>,
}

pub(super) async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
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
    let header = |name| {
        response
            .headers()
            .get(name)
            .map(|value: &axum::http::HeaderValue| value.to_str().expect("ascii").to_string())
    };
    let location = header("location");
    let cookie = header("set-cookie");
    let status = response.status();
    let tagged = response.extensions().get::<ErrorCode>().copied();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
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
    (router(AppState::new(db, None, sign_in)), code)
}

fn none_mode(public_url: Option<&str>) -> SignIn {
    SignIn::new(AuthChoice::Chosen(AuthMode::None), "127.0.0.1", public_url).expect("none mode")
}

pub(super) fn session_of(reply: &Reply) -> String {
    let cookie = reply.cookie.as_deref().expect("set-cookie");
    cookie.split(';').next().expect("cookie pair").to_string()
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

pub(super) async fn sign_in(
    app: &axum::Router,
    authenticator: &mut SoftAuthenticator,
    headers: &[(&str, &str)],
) -> Reply {
    let started = call(app, "POST", "/api/v1/auth/sign-in/options", headers, None).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let credential = authenticator.sign_in(&started.body["options"]);
    call(
        app,
        "POST",
        "/api/v1/auth/sign-in",
        headers,
        Some(json!({ "ceremony": started.body["ceremony"], "credential": credential })),
    )
    .await
}

pub(super) async fn add_passkey(
    app: &axum::Router,
    authenticator: &mut SoftAuthenticator,
    headers: &[(&str, &str)],
    name: &str,
) -> Reply {
    let started = call(app, "POST", "/api/v1/auth/passkeys/options", headers, None).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let credential = authenticator.register(&started.body["options"]);
    call(
        app,
        "POST",
        "/api/v1/auth/passkeys",
        headers,
        Some(json!({
            "ceremony": started.body["ceremony"],
            "name": name,
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
    let passkeys = db.passkeys().expect("passkeys");
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
    assert_eq!(db.passkeys().expect("passkeys").len(), 1);
    assert_eq!(db.user_count().expect("users"), 1);
}

#[tokio::test]
async fn last_passkey_should_not_be_removable() {
    let (_dir, db) = open_db();
    let (app, code) = passkey_app(db.clone());
    let mut laptop = SoftAuthenticator::new(7);
    let session =
        session_of(&set_up(&app, &code, &mut laptop, json!({ "username": "chip" })).await);
    let cookie = [("cookie", session.as_str())];
    let first_id = db.passkeys().expect("passkeys")[0].id.clone();

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
    let app = router(AppState::new(db.clone(), None, none_mode(None)));
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
    let app = router(AppState::new(db, None, none_mode(None)));

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
    assert_eq!(write.body["code"], "insufficient_scope");
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
    let app = router(AppState::new(db.clone(), None, none_mode(None)));
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
    let bare = router(AppState::new(db.clone(), None, none_mode(None)));
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
    let public = router(AppState::new(db, None, none_mode(Some(TEST_PUBLIC_URL))));
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
    let app = router(AppState::new(db, None, none_mode(Some(TEST_PUBLIC_URL))));
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
async fn recovery_should_add_a_passkey_without_sign_in_that_works_with_passkeys() {
    let (_dir, db) = open_db();
    let (first_app, code) = passkey_app(db.clone());
    let mut lost = SoftAuthenticator::new(7);
    set_up(&first_app, &code, &mut lost, json!({ "username": "chip" })).await;

    let open_app = router(AppState::new(
        db.clone(),
        None,
        none_mode(Some(TEST_PUBLIC_URL)),
    ));
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
    let app = router(AppState::new(other.clone(), None, sign_in));
    let mut second = SoftAuthenticator::new(8);
    let used = set_up(&app, &stale, &mut second, json!({})).await;
    assert_eq!(used.status, StatusCode::OK, "{}", used.body);
    assert!(!relying_party.setup_code_matches(&stale));
}

#[tokio::test]
async fn setup_should_need_the_code_even_with_a_session_cookie() {
    let (_dir, db) = open_db();
    let user_id = db.create_user("admin").expect("user");
    let session = db.create_session(&user_id).expect("session");
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
    assert_eq!(db.passkeys().expect("passkeys").len(), 1);
}
