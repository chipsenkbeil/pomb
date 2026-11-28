use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use redb::{
    MultimapTableDefinition, MultimapTableHandle, ReadableDatabase, ReadableMultimapTable,
    ReadableTable, TableDefinition, TableHandle,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::router;
use super::server::{TEST_PUBLIC_URL, passkey_mode, unthrottled_limits};
use crate::api_error::ErrorCode;
use crate::auth::{AuthChoice, AuthMode, SignIn, hash_session_token, new_api_token_record};
use crate::db::{BootstrapToken, Db, ModeSwitch};
use crate::disk::{DiskGuard, DiskProbe};
use crate::models::{AccountId, Actor, AuthScope, InstanceMode, TaskKind};
use crate::permissions::{STANDARD_ROLES, StandardRole};
use crate::{AppState, BODY_LIMIT_BYTES};

const RP_ID: &str = "pomb.test";
const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_USER_VERIFIED: u8 = 0x04;
const FLAG_ATTESTED_DATA: u8 = 0x40;
pub(crate) const TEST_ATTACHMENT_3_MIB_BYTES: usize = 3 * 1024 * 1024;
pub(crate) const TEST_BODY_4_MIB_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const TEST_BODY_16_MIB_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const TEST_TODO_COLUMN: &str = "Todo";
pub(crate) const TEST_DOING_COLUMN: &str = "Doing";
pub(crate) const TEST_DONE_COLUMN: &str = "Done";
pub(crate) const TEST_BLOCKED_COLUMN: &str = "Blocked";
pub(crate) const TEST_BACKLOG_COLUMN: &str = "Backlog";
pub(crate) const TEST_READY_COLUMN: &str = "Ready";
pub(crate) const TEST_IN_PROGRESS_COLUMN: &str = "In Progress";
pub(crate) const TEST_IN_REVIEW_COLUMN: &str = "In Review";
pub(crate) const TEST_VERIFY_COLUMN: &str = "Verify";
pub(crate) const TEST_RELEASE_COLUMN: &str = "Release";
pub(crate) const TEST_SHIPPED_COLUMN: &str = "Shipped";

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
        let point = self.key.verifying_key().to_sec1_point(false);
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

/// Runs a passkey ceremony pair, the options request then the finish with `authenticator`'s new
/// credential, and returns the finish reply.
pub(crate) async fn enroll(
    app: &axum::Router,
    authenticator: &mut SoftAuthenticator,
    route: &str,
    start: Value,
) -> Reply {
    enroll_named(app, authenticator, route, start, "Passkey").await
}

/// Runs [`enroll`] for a passkey labelled `name`.
pub(crate) async fn enroll_named(
    app: &axum::Router,
    authenticator: &mut SoftAuthenticator,
    route: &str,
    start: Value,
    name: &str,
) -> Reply {
    let started = call(app, "POST", &format!("{route}/options"), &[], Some(start)).await;
    assert_eq!(started.status, StatusCode::OK, "{route}: {}", started.body);
    let credential = authenticator.register(&started.body["options"]);
    let body =
        json!({"ceremony": started.body["ceremony"], "credential": credential, "name": name});
    call(app, "POST", route, &[], Some(body)).await
}

/// Mints an admin-scope API token with the browser session `cookie` and returns it.
pub(crate) async fn mint_token(app: &axum::Router, cookie: &str) -> String {
    let body = json!({"name": "agent", "scopes": ["admin"]});
    let minted = call(
        app,
        "POST",
        "/api/v1/auth/tokens",
        &[("cookie", cookie)],
        Some(body),
    )
    .await;
    assert_eq!(minted.status, StatusCode::OK, "{}", minted.body);
    minted.body["raw_token"]
        .as_str()
        .expect("raw token")
        .to_string()
}

pub(crate) async fn call_bearer(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Reply {
    let bearer = format!("Bearer {token}");
    call(app, method, uri, &[("authorization", &bearer)], body).await
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

/// Returns a disk guard over `db` whose clock never moves, so its measurement never goes stale.
pub(crate) fn held_disk(db: &Db) -> DiskGuard {
    let born = Instant::now();
    DiskGuard::with_clock(DiskProbe::of(db), Path::new("."), Arc::new(move || born))
}

#[must_use]
pub(crate) fn task_snapshot(db: &Db, id: &str) -> Vec<u8> {
    let row = db.task(id).expect("read").expect("task");
    serde_json::to_vec(&row).expect("json")
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
            AppState::new(db.clone(), None, passkey_mode())
                .with_disk(held_disk(&db))
                .with_limits(unthrottled_limits()),
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

    #[must_use]
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        call_bearer(&self.app, &self.token, method, uri, body).await
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

pub(crate) async fn move_to(app: &axum::Router, cookie: &str, task: &str, column: &str) {
    let reply = call(
        app,
        "POST",
        &format!("/api/tasks/{task}/move"),
        &[("accept", "application/json"), ("cookie", cookie)],
        Some(json!({"column_id": column, "position": 0})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

pub(crate) fn named_standard_role(name: &str) -> &'static StandardRole {
    STANDARD_ROLES
        .iter()
        .find(|standard| standard.name == name)
        .expect("standard role")
}

pub(crate) fn drop_seeded_roles(db: &Db, account: &AccountId) {
    for role in db.list_agent_roles(account.as_str()).expect("roles") {
        db.delete_agent_role(account.as_str(), &role.id)
            .expect("delete");
    }
}

/// The sessions table, which a test rewrites in a closed database file.
pub(crate) const SESSIONS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("sessions");

/// Every row of every table in a closed database file, by table name, as text.
pub(crate) type Dump = BTreeMap<String, Vec<(String, String)>>;

/// Reads every row of every table and multimap in the closed database file at `path`.
#[must_use]
pub(crate) fn dump(path: &Path) -> Dump {
    let db = redb::ReadOnlyDatabase::open(path).expect("read-only open");
    let txn = db.begin_read().expect("read txn");
    let mut tables = Dump::new();
    for handle in txn.list_tables().expect("tables") {
        let name = handle.name().to_string();
        let mut rows = Vec::new();
        if let Ok(tbl) = txn.open_table(TableDefinition::<&str, &[u8]>::new(&name)) {
            for item in tbl.iter().expect("iter") {
                let (key, value) = item.expect("row");
                rows.push((
                    key.value().to_string(),
                    String::from_utf8_lossy(value.value()).into_owned(),
                ));
            }
        } else if let Ok(tbl) = txn.open_table(TableDefinition::<&str, &str>::new(&name)) {
            for item in tbl.iter().expect("iter") {
                let (key, value) = item.expect("row");
                rows.push((key.value().to_string(), value.value().to_string()));
            }
        } else if let Ok(tbl) = txn.open_table(TableDefinition::<&str, u64>::new(&name)) {
            for item in tbl.iter().expect("iter") {
                let (key, value) = item.expect("row");
                rows.push((key.value().to_string(), value.value().to_string()));
            }
        } else {
            let tbl = txn
                .open_table(TableDefinition::<&str, i64>::new(&name))
                .expect("a table of strings, bytes or numbers");
            for item in tbl.iter().expect("iter") {
                let (key, value) = item.expect("row");
                rows.push((key.value().to_string(), value.value().to_string()));
            }
        }
        tables.insert(name, rows);
    }
    for handle in txn.list_multimap_tables().expect("multimaps") {
        let name = handle.name().to_string();
        let mut rows = Vec::new();
        if let Ok(tbl) = txn.open_multimap_table(MultimapTableDefinition::<&str, &str>::new(&name))
        {
            for item in tbl.iter().expect("iter") {
                let (key, values) = item.expect("row");
                for value in values {
                    let value = value.expect("value");
                    rows.push((key.value().to_string(), value.value().to_string()));
                }
            }
        } else {
            let tbl = txn
                .open_multimap_table(MultimapTableDefinition::<i64, &str>::new(&name))
                .expect("a multimap of strings or numbers");
            for item in tbl.iter().expect("iter") {
                let (key, values) = item.expect("row");
                for value in values {
                    let value = value.expect("value");
                    rows.push((key.value().to_string(), value.value().to_string()));
                }
            }
        }
        tables.insert(name, rows);
    }
    tables
}

/// A multi-mode instance on a database file of its own.
pub(crate) struct Instance {
    /// Keeps the database directory alive.
    pub(crate) _dir: tempfile::TempDir,
    /// The database file.
    pub(crate) path: PathBuf,
    /// The database behind [`Instance::app`].
    pub(crate) db: Db,
    /// The router under test.
    pub(crate) app: axum::Router,
    /// The setup code the first passkey needs.
    pub(crate) code: String,
}

/// Returns a multi-mode router over `db` that accepts `bootstrap`, and its setup code.
fn multi_app(db: &Db, bootstrap: Option<&str>) -> (axum::Router, String) {
    let sign_in = passkey_mode();
    let code = sign_in
        .relying_party()
        .expect("relying party")
        .issue_setup_code();
    let state = AppState::new(db.clone(), bootstrap.and_then(BootstrapToken::new), sign_in)
        .with_disk(held_disk(db))
        .with_mode(InstanceMode::Multi)
        .with_public_url(Some(TEST_PUBLIC_URL.to_string()))
        .with_limits(unthrottled_limits());
    (router(state), code)
}

impl Instance {
    /// Opens a fresh database in multi mode behind a router that accepts `bootstrap`.
    pub(crate) fn new(bootstrap: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("multi.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        assert_eq!(
            db.apply_mode(InstanceMode::Multi).expect("mode"),
            ModeSwitch::Switched {
                from: InstanceMode::Single
            }
        );
        let (app, code) = multi_app(&db, bootstrap);
        Self {
            _dir: dir,
            path,
            db,
            app,
            code,
        }
    }

    /// Closes the database, reads every row in it, and opens it again behind a fresh router.
    pub(crate) fn downgrade_to_version_19_and_reopen(self) -> Self {
        let Self {
            _dir,
            path,
            code,
            db,
            app,
        } = self;
        drop(app);
        db.flush_activity().expect("flush activity");
        drop(db);
        {
            let raw = redb::Database::create(&path).expect("raw open");
            let txn = raw.begin_write().expect("write txn");
            txn.delete_table(TableDefinition::<&str, &str>::new("passkeys_by_credential"))
                .expect("drop credential index");
            for index in [
                "tokens_by_account",
                "sessions_by_user",
                "devices_by_account",
            ] {
                txn.delete_multimap_table(MultimapTableDefinition::<&str, &str>::new(index))
                    .expect("drop index");
            }
            txn.open_table(TableDefinition::<&str, u64>::new("meta"))
                .expect("meta")
                .insert("schema_version", 19)
                .expect("stamp");
            txn.commit().expect("commit");
        }
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("upgrade");
        let (app, _) = multi_app(&db, None);
        Self {
            _dir,
            path,
            db,
            app,
            code,
        }
    }

    pub(crate) fn dump_and_reopen(self) -> (Self, Dump) {
        let Self {
            _dir,
            path,
            code,
            db,
            app,
        } = self;
        drop(app);
        db.flush_activity().expect("flush activity");
        drop(db);
        let rows = dump(&path);
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("reconnect");
        let (app, _) = multi_app(&db, None);
        (
            Self {
                _dir,
                path,
                db,
                app,
                code,
            },
            rows,
        )
    }

    /// Closes the database, sets the `verified_at` of each session in `edits` in its file, and
    /// opens it again behind a fresh router.
    pub(crate) fn restamp(self, edits: &[(&str, Option<String>)]) -> Self {
        let Self {
            _dir,
            path,
            code,
            db,
            app,
        } = self;
        drop(app);
        db.flush_activity().expect("flush activity");
        drop(db);
        {
            let file = redb::Database::create(&path).expect("open");
            let txn = file.begin_write().expect("write txn");
            {
                let mut sessions = txn.open_table(SESSIONS).expect("sessions");
                for (token, verified_at) in edits {
                    let raw = sessions
                        .get(hash_session_token(token).as_str())
                        .expect("get")
                        .expect("session")
                        .value()
                        .to_vec();
                    let mut row: Value = serde_json::from_slice(&raw).expect("session row");
                    row["verified_at"] = verified_at.clone().map_or(Value::Null, Value::String);
                    let raw = serde_json::to_vec(&row).expect("json");
                    sessions
                        .insert(hash_session_token(token).as_str(), raw.as_slice())
                        .expect("insert");
                }
            }
            txn.commit().expect("commit");
        }
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("reconnect");
        let (app, _) = multi_app(&db, None);
        Self {
            _dir,
            path,
            db,
            app,
            code,
        }
    }

    /// Returns the id of the account with `handle`.
    #[must_use]
    pub(crate) fn account(&self, handle: &str) -> AccountId {
        self.db
            .account_by_handle(handle)
            .expect("read")
            .expect("account")
            .account_id()
    }
}

/// Runs first-run setup for the admin `Admin`, whose handle is stored as `admin`.
pub(crate) async fn set_up_admin(
    instance: &Instance,
    authenticator: &mut SoftAuthenticator,
) -> Reply {
    let start = json!({"username": "Admin", "code": instance.code});
    enroll(&instance.app, authenticator, "/api/v1/auth/setup", start).await
}

/// Mints an invite link with the admin's session `cookie` and returns its token.
pub(crate) async fn mint_invite(app: &axum::Router, cookie: &str) -> String {
    let minted = call(
        app,
        "POST",
        "/api/v1/admin/invites",
        &[("cookie", cookie)],
        Some(json!({})),
    )
    .await;
    assert_eq!(minted.status, StatusCode::OK, "{}", minted.body);
    let url = minted.body["url"].as_str().expect("invite url");
    assert!(url.starts_with("https://pomb.test/signup/"), "{url}");
    url.rsplit('/').next().expect("token").to_string()
}

/// Registers `handle` through the invite `token` with `authenticator`.
pub(crate) async fn register(
    app: &axum::Router,
    token: &str,
    handle: &str,
    authenticator: &mut SoftAuthenticator,
) -> Reply {
    let start = json!({"invite": token, "handle": handle});
    enroll(app, authenticator, "/api/v1/auth/register", start).await
}

/// Starts a ceremony at `route` and returns its options reply.
pub(crate) async fn start(app: &axum::Router, route: &str, body: Value) -> Reply {
    call(app, "POST", &format!("{route}/options"), &[], Some(body)).await
}

/// Finishes the ceremony `started` at `route` with a new credential from `authenticator`.
pub(crate) async fn finish(
    app: &axum::Router,
    route: &str,
    started: &Reply,
    authenticator: &mut SoftAuthenticator,
) -> Reply {
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let credential = authenticator.register(&started.body["options"]);
    let body = json!({"ceremony": started.body["ceremony"], "credential": credential});
    call(app, "POST", route, &[], Some(body)).await
}

/// Returns the recovery code a setup, registration or recovery showed.
#[must_use]
pub(crate) fn recovery_code(reply: &Reply) -> String {
    reply.body["recovery_code"]
        .as_str()
        .unwrap_or_else(|| panic!("no recovery code: {}", reply.body))
        .to_string()
}

/// A multi-mode instance with its admin `admin` and a member `bob`, each signed in.
pub(crate) struct Pair {
    /// The instance.
    pub(crate) instance: Instance,
    /// The admin's session cookie.
    pub(crate) admin: String,
    /// The admin's recovery code.
    pub(crate) admin_code: String,
    /// The admin's passkey.
    pub(crate) admin_key: SoftAuthenticator,
    /// The member's session cookie.
    pub(crate) member: String,
    /// The member's recovery code.
    pub(crate) member_code: String,
    /// The member's passkey.
    pub(crate) member_key: SoftAuthenticator,
}

/// Sets up the admin, then registers `Bob` through an invite link.
pub(crate) async fn pair(bootstrap: Option<&str>) -> Pair {
    let instance = Instance::new(bootstrap);
    let mut admin_key = SoftAuthenticator::new(21);
    let setup = set_up_admin(&instance, &mut admin_key).await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    let admin = session_of(&setup);
    let token = mint_invite(&instance.app, &admin).await;
    let mut member_key = SoftAuthenticator::new(22);
    let joined = register(&instance.app, &token, "Bob", &mut member_key).await;
    assert_eq!(joined.status, StatusCode::OK, "{}", joined.body);
    Pair {
        admin_code: recovery_code(&setup),
        member_code: recovery_code(&joined),
        member: session_of(&joined),
        admin,
        admin_key,
        member_key,
        instance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn call_bearer_should_send_the_token_as_a_bearer_header() {
        let fx = Fixture::new();
        let bare = call(&fx.app, "GET", "/api/notifications", &[], None).await;
        assert_eq!(bare.status, StatusCode::UNAUTHORIZED, "{}", bare.body);
        let reply = call_bearer(&fx.app, &fx.token, "GET", "/api/notifications", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }
}
