use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::router;
use super::server::{TEST_PUBLIC_URL, passkey_mode, unthrottled_limits};
use crate::api_error::ErrorCode;
use crate::auth::{AuthChoice, AuthMode, SignIn, new_api_token_record};
use crate::db::Db;
use crate::models::{AccountId, Actor, AuthScope, TaskKind};
use crate::{AppState, BODY_LIMIT_BYTES};

const RP_ID: &str = "pomb.test";
const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_USER_VERIFIED: u8 = 0x04;
const FLAG_ATTESTED_DATA: u8 = 0x40;

/// A software authenticator holding one P-256 passkey, which speaks WebAuthn's JSON the way the
/// browser script sends it.
pub(crate) struct SoftAuthenticator {
    key: SigningKey,
    credential_id: Vec<u8>,
    pub(crate) user_handle: Vec<u8>,
    counter: u32,
    origin: String,
}

impl SoftAuthenticator {
    pub(crate) fn new(seed: u8) -> Self {
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

    pub(crate) fn register(&mut self, options: &Value) -> Value {
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

    pub(crate) fn sign_in(&mut self, options: &Value) -> Value {
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

pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) location: Option<String>,
    pub(crate) cookie: Option<String>,
    pub(crate) body: Value,
    /// The error code the response carries, whether or not its body is JSON.
    pub(crate) code: Option<String>,
}

pub(crate) async fn call(
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
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
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

pub(crate) fn none_mode(public_url: Option<&str>) -> SignIn {
    SignIn::new(AuthChoice::Chosen(AuthMode::None), "127.0.0.1", public_url).expect("none mode")
}

pub(crate) fn make_epic(db: &Db, ids: &[&str]) {
    set_kind(db, ids, TaskKind::Epic);
}

pub(crate) fn make_plain(db: &Db, ids: &[&str]) {
    set_kind(db, ids, TaskKind::Task);
}

fn set_kind(db: &Db, ids: &[&str], kind: TaskKind) {
    for id in ids {
        db.apply_task_change(id, Actor::SYSTEM, |row| {
            row.kind = kind;
            ((), true)
        })
        .expect("kind");
    }
}

pub(crate) fn session_of(reply: &Reply) -> String {
    let cookie = reply.cookie.as_deref().expect("set-cookie");
    cookie.split(';').next().expect("cookie pair").to_string()
}

pub(crate) async fn sign_in(
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

pub(crate) async fn add_passkey(
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

pub(crate) struct Fixture {
    _dir: tempfile::TempDir,
    pub(crate) app: axum::Router,
    pub(crate) db: Db,
    pub(crate) admin: AccountId,
    token: String,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fixture.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "admin", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            admin,
            token,
        }
    }

    pub(crate) fn board(&self, name: &str, columns: &[&str]) -> (String, Vec<String>) {
        let board = self.db.create_board(&self.admin, name).expect("board");
        self.db.create_swimlane(&board, "Lane").expect("lane");
        let ids = columns
            .iter()
            .map(|column| self.db.create_column(&board, column, None).expect("column"))
            .collect();
        (board, ids)
    }

    pub(crate) async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {}", self.token);
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    pub(crate) async fn task(&self, column: &str, name: &str) -> String {
        let body = json!({"column_id": column, "name": name});
        let reply = self.send("POST", "/api/tasks", Some(body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["id"].as_str().expect("id").to_string()
    }

    pub(crate) async fn epic(&self, column: &str, name: &str) -> String {
        let body = json!({"column_id": column, "name": name, "kind": "epic"});
        let reply = self.send("POST", "/api/tasks", Some(body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["id"].as_str().expect("id").to_string()
    }

    pub(crate) async fn bulk(&self, board: &str, body: Value) -> Reply {
        let uri = format!("/api/boards/{board}/tasks/bulk");
        self.send("POST", &uri, Some(body)).await
    }

    pub(crate) async fn top_placement(&self, board: &str) {
        let body = json!({"new_tasks_at_top": true});
        let reply = self
            .send("PUT", &format!("/api/boards/{board}/config"), Some(body))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }

    pub(crate) fn cell(&self, board: &str, column: &str) -> Vec<String> {
        let mut tasks: Vec<_> = self
            .db
            .board_tasks(board)
            .expect("tasks")
            .into_iter()
            .filter(|task| task.column_id == column)
            .collect();
        tasks.sort_by(|a, b| a.position.total_cmp(&b.position));
        tasks.into_iter().map(|task| task.name).collect()
    }
}
