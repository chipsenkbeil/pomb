use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use redb::{
    MultimapTableDefinition, MultimapTableHandle, ReadableDatabase, ReadableMultimapTable,
    ReadableTable, TableDefinition, TableHandle,
};
use serde_json::{Value, json};

use super::access_tests::{enroll, mint_token, seed};
use super::passkey_tests::{Reply, SoftAuthenticator, call, session_of, sign_in};
use super::router;
use super::server::{TEST_PUBLIC_URL, passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::{hash_invite_token, normalize_user_code};
use crate::db::{BootstrapToken, Db, DeviceGrant, ModeSwitch, NewTimeEntry};
use crate::models::{
    AccountId, AuthScope, EntryKind, InstanceMode, InviteRow, SECONDS_PER_DAY, Settings,
};

/// The bootstrap token the instances under test accept.
const BOOTSTRAP: &str = "pomb_multi_bootstrap_token_for_tests";

/// The tables an account's rows live in, each of which the deleted account fills.
const OWNED_TABLES: [&str; 27] = [
    "users",
    "usernames",
    "sessions",
    "passkeys",
    "recovery_codes",
    "invites",
    "boards",
    "boards_by_owner",
    "columns",
    "columns_by_board",
    "swimlanes",
    "swimlanes_by_board",
    "tasks",
    "tasks_by_column",
    "task_links_by_target",
    "time_entries",
    "entries_by_task",
    "task_colors",
    "colors_by_board",
    "field_defs",
    "fields_by_board",
    "templates",
    "attachment_data",
    "api_tokens",
    "device_auths",
    "account_settings",
    "active_timer",
];

/// Every row of every table in a closed database file, by table name, as text.
type Dump = BTreeMap<String, Vec<(String, String)>>;

/// Reads every row of every table and multimap in the closed database file at `path`.
fn dump(path: &Path) -> Dump {
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
        } else {
            let tbl = txn
                .open_table(TableDefinition::<&str, u64>::new(&name))
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
        let tbl = txn
            .open_multimap_table(MultimapTableDefinition::<&str, &str>::new(&name))
            .expect("multimap");
        let mut rows = Vec::new();
        for item in tbl.iter().expect("iter") {
            let (key, values) = item.expect("row");
            for value in values {
                let value = value.expect("value");
                rows.push((key.value().to_string(), value.value().to_string()));
            }
        }
        tables.insert(name, rows);
    }
    tables
}

/// A multi-mode instance on a database file of its own.
struct Instance {
    /// Keeps the database directory alive.
    _dir: tempfile::TempDir,
    /// The database file.
    path: PathBuf,
    /// The database behind [`Instance::app`].
    db: Db,
    /// The router under test.
    app: axum::Router,
    /// The setup code the first passkey needs.
    code: String,
}

/// Returns a multi-mode router over `db` that accepts `bootstrap`, and its setup code.
fn multi_app(db: &Db, bootstrap: Option<&str>) -> (axum::Router, String) {
    let sign_in = passkey_mode();
    let code = sign_in
        .relying_party()
        .expect("relying party")
        .issue_setup_code();
    let state = AppState::new(db.clone(), bootstrap.and_then(BootstrapToken::new), sign_in)
        .with_mode(InstanceMode::Multi)
        .with_public_url(Some(TEST_PUBLIC_URL.to_string()))
        .with_limits(unthrottled_limits());
    (router(state), code)
}

impl Instance {
    /// Opens a fresh database in multi mode behind a router that accepts `bootstrap`.
    fn new(bootstrap: Option<&str>) -> Self {
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
    fn dump_and_reopen(self) -> (Self, Dump) {
        let Self {
            _dir,
            path,
            code,
            db,
            app,
        } = self;
        drop(app);
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

    /// Returns the id of the account with `handle`.
    fn account(&self, handle: &str) -> AccountId {
        self.db
            .account_by_handle(handle)
            .expect("read")
            .expect("account")
            .account_id()
    }
}

/// Runs first-run setup for the admin `Admin`, whose handle is stored as `admin`.
async fn set_up_admin(instance: &Instance, authenticator: &mut SoftAuthenticator) -> Reply {
    let start = json!({"username": "Admin", "code": instance.code});
    enroll(&instance.app, authenticator, "/api/v1/auth/setup", start).await
}

/// Mints an invite link with the admin's session `cookie` and returns its token.
async fn mint_invite(app: &axum::Router, cookie: &str) -> String {
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
async fn register(
    app: &axum::Router,
    token: &str,
    handle: &str,
    authenticator: &mut SoftAuthenticator,
) -> Reply {
    let start = json!({"invite": token, "handle": handle});
    enroll(app, authenticator, "/api/v1/auth/register", start).await
}

/// Starts a ceremony at `route` and returns its options reply.
async fn start(app: &axum::Router, route: &str, body: Value) -> Reply {
    call(app, "POST", &format!("{route}/options"), &[], Some(body)).await
}

/// Finishes the ceremony `started` at `route` with a new credential from `authenticator`.
async fn finish(
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

/// Returns the credential the session `cookie` signs in.
async fn whoami(app: &axum::Router, cookie: &str) -> Reply {
    call(
        app,
        "GET",
        "/api/v1/auth/whoami",
        &[("cookie", cookie)],
        None,
    )
    .await
}

/// Reads `uri` with the session `cookie`.
async fn get(app: &axum::Router, uri: &str, cookie: &str) -> Reply {
    call(app, "GET", uri, &[("cookie", cookie)], None).await
}

/// Deletes the account behind `cookie`, confirming with `handle`.
async fn delete_account(app: &axum::Router, cookie: &str, handle: &str) -> Reply {
    call(
        app,
        "POST",
        "/api/v1/account/delete",
        &[("cookie", cookie)],
        Some(json!({"handle": handle})),
    )
    .await
}

/// Returns the recovery code a setup, registration or recovery showed.
fn recovery_code(reply: &Reply) -> String {
    reply.body["recovery_code"]
        .as_str()
        .unwrap_or_else(|| panic!("no recovery code: {}", reply.body))
        .to_string()
}

/// A multi-mode instance with its admin `admin` and a member `bob`, each signed in.
struct Pair {
    /// The instance.
    instance: Instance,
    /// The admin's session cookie.
    admin: String,
    /// The admin's recovery code.
    admin_code: String,
    /// The member's session cookie.
    member: String,
    /// The member's recovery code.
    member_code: String,
    /// The member's passkey.
    member_key: SoftAuthenticator,
}

/// Sets up the admin, then registers `Bob` through an invite link.
async fn pair(bootstrap: Option<&str>) -> Pair {
    let instance = Instance::new(bootstrap);
    let setup = set_up_admin(&instance, &mut SoftAuthenticator::new(21)).await;
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
        member_key,
        instance,
    }
}

#[tokio::test]
async fn invite_should_be_single_use_and_expire() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let token = mint_invite(app, &fx.admin).await;

    let carol = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": token, "handle": "carol"}),
    )
    .await;
    let dave = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": token, "handle": "dave"}),
    )
    .await;
    let first = finish(
        app,
        "/api/v1/auth/register",
        &carol,
        &mut SoftAuthenticator::new(31),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let second = finish(
        app,
        "/api/v1/auth/register",
        &dave,
        &mut SoftAuthenticator::new(32),
    )
    .await;
    assert_eq!(second.status, StatusCode::GONE, "{}", second.body);
    assert_eq!(second.body["code"], "gone");
    assert!(
        fx.instance
            .db
            .account_by_handle("dave")
            .expect("read")
            .is_none()
    );

    let again = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": token, "handle": "erin"}),
    )
    .await;
    assert_eq!(again.status, StatusCode::GONE, "{}", again.body);
    assert_eq!(again.body["code"], "gone");
    let page = call(app, "GET", &format!("/signup/{token}"), &[], None).await;
    assert_eq!(page.status, StatusCode::GONE);

    let now = chrono::Utc::now().timestamp();
    let expired = "an-expired-invite-token";
    let row = InviteRow {
        id: uuid::Uuid::new_v4().to_string(),
        created_by: fx.instance.account("admin").as_str().to_string(),
        created_at: now - 8 * SECONDS_PER_DAY,
        expires_at: now - 1,
    };
    fx.instance
        .db
        .create_invite(&hash_invite_token(expired), &row, now - 2)
        .expect("invite");
    let late = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": expired, "handle": "frank"}),
    )
    .await;
    assert_eq!(late.status, StatusCode::GONE, "{}", late.body);
    assert_eq!(late.body["code"], "gone");
    let page = call(app, "GET", &format!("/signup/{expired}"), &[], None).await;
    assert_eq!(page.status, StatusCode::GONE);
    let listed = call(
        app,
        "GET",
        "/api/v1/admin/invites",
        &[("cookie", &fx.admin)],
        None,
    )
    .await;
    assert_eq!(
        listed.body,
        json!([]),
        "used and expired invites are not listed"
    );
}

#[tokio::test]
async fn invite_get_should_never_write() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let token = mint_invite(app, &fx.admin).await;
    let before = std::fs::read(&fx.instance.path).expect("read db file");

    let page = call(app, "GET", &format!("/signup/{token}"), &[], None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .as_str()
            .is_some_and(|html| html.contains("Create account")),
        "{}",
        page.body
    );
    let unknown = call(app, "GET", "/signup/never-minted", &[], None).await;
    assert_eq!(unknown.status, StatusCode::GONE);
    let started = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": token, "handle": "carol"}),
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    assert!(
        std::fs::read(&fx.instance.path).expect("read db file") == before,
        "opening the link or starting a registration wrote to the database"
    );

    let listed = call(
        app,
        "GET",
        "/api/v1/admin/invites",
        &[("cookie", &fx.admin)],
        None,
    )
    .await;
    assert_eq!(
        listed.body.as_array().map(Vec::len),
        Some(1),
        "{}",
        listed.body
    );
}

#[tokio::test]
async fn registration_should_show_recovery_code_once_and_store_only_a_hash() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    for code in [&fx.admin_code, &fx.member_code] {
        let groups: Vec<_> = code.split('-').collect();
        assert_eq!(groups.len(), 8, "{code}");
        assert!(
            groups
                .iter()
                .all(|group| group.len() == 4 && group.chars().all(|c| c.is_ascii_hexdigit())),
            "{code}"
        );
    }
    assert_ne!(fx.admin_code, fx.member_code);
    for uri in [
        "/settings",
        "/api/v1/auth/whoami",
        "/",
        "/api/v1/auth/passkeys",
    ] {
        let reply = call(app, "GET", uri, &[("cookie", &fx.member)], None).await;
        assert!(
            reply.status.is_success() || reply.status.is_redirection(),
            "{uri}: {}",
            reply.status
        );
        assert!(
            !reply.body.to_string().contains(&fx.member_code),
            "{uri} shows the recovery code again"
        );
    }

    let member = fx.instance.account("bob");
    let Instance {
        _dir,
        path,
        db,
        app,
        ..
    } = fx.instance;
    drop(app);
    drop(db);
    let rows = dump(&path);
    let file = std::fs::read(&path).expect("read db file");
    for code in [&fx.admin_code, &fx.member_code] {
        let digits = code.replace('-', "");
        for secret in [code.as_str(), digits.as_str()] {
            assert!(
                !file
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "the database file holds the recovery code {secret}"
            );
        }
    }
    let stored = rows["recovery_codes"]
        .iter()
        .find(|(key, _)| key == member.as_str())
        .map(|(_, value)| serde_json::from_str::<Value>(value).expect("json"))
        .expect("the member's recovery code row");
    assert_eq!(stored["hash"].as_str().map(str::len), Some(64), "{stored}");
    assert_eq!(rows["recovery_codes"].len(), 2);
}

#[tokio::test]
async fn handle_should_be_normalized_and_unique() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    assert_eq!(whoami(app, &fx.member).await.body["handle"], "bob");
    assert_eq!(whoami(app, &fx.admin).await.body["handle"], "admin");

    let token = mint_invite(app, &fx.admin).await;
    for taken in ["BOB", " bob ", "Admin"] {
        let refused = start(
            app,
            "/api/v1/auth/register",
            json!({"invite": token, "handle": taken}),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{taken}: {}",
            refused.body
        );
        assert_eq!(refused.body["code"], "handle_taken", "{taken}");
    }
    for bad in ["ab", "has space", "a.b", "émile", &"x".repeat(33)] {
        let refused = start(
            app,
            "/api/v1/auth/register",
            json!({"invite": token, "handle": bad}),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{bad}: {}",
            refused.body
        );
    }

    let other = mint_invite(app, &fx.admin).await;
    let first = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": token, "handle": "  Carol_9 "}),
    )
    .await;
    let second = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": other, "handle": "carol_9"}),
    )
    .await;
    let won = finish(
        app,
        "/api/v1/auth/register",
        &first,
        &mut SoftAuthenticator::new(33),
    )
    .await;
    assert_eq!(won.status, StatusCode::OK, "{}", won.body);
    assert_eq!(
        whoami(app, &session_of(&won)).await.body["handle"],
        "carol_9"
    );
    let lost = finish(
        app,
        "/api/v1/auth/register",
        &second,
        &mut SoftAuthenticator::new(34),
    )
    .await;
    assert_eq!(lost.status, StatusCode::CONFLICT, "{}", lost.body);
    assert_eq!(lost.body["code"], "handle_taken");
}

#[tokio::test]
async fn recovery_should_replace_code_and_end_sessions() {
    let mut fx = pair(None).await;
    let app = fx.instance.app.clone();
    let second = session_of(&sign_in(&app, &mut fx.member_key, &[]).await);
    assert_eq!(whoami(&app, &second).await.status, StatusCode::OK);

    let mut replacement = SoftAuthenticator::new(41);
    let started = start(
        &app,
        "/api/v1/auth/recover",
        json!({"handle": "Bob", "recovery_code": fx.member_code}),
    )
    .await;
    let recovered = finish(&app, "/api/v1/auth/recover", &started, &mut replacement).await;
    assert_eq!(recovered.status, StatusCode::OK, "{}", recovered.body);
    let new_code = recovery_code(&recovered);
    assert_ne!(new_code, fx.member_code);
    let session = session_of(&recovered);
    assert_eq!(whoami(&app, &session).await.body["handle"], "bob");
    for old in [&fx.member, &second] {
        assert_eq!(whoami(&app, old).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(whoami(&app, &fx.admin).await.status, StatusCode::OK);

    let stale = start(
        &app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": fx.member_code}),
    )
    .await;
    assert_eq!(stale.status, StatusCode::BAD_REQUEST, "{}", stale.body);
    assert_eq!(stale.body["code"], "invalid_recovery");
    let fresh = start(
        &app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": new_code}),
    )
    .await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);

    let passkeys = call(
        &app,
        "GET",
        "/api/v1/auth/passkeys",
        &[("cookie", &session)],
        None,
    )
    .await;
    assert_eq!(
        passkeys.body.as_array().map(Vec::len),
        Some(2),
        "{}",
        passkeys.body
    );
    for authenticator in [&mut fx.member_key, &mut replacement] {
        let signed_in = sign_in(&app, authenticator, &[]).await;
        assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    }
}

#[tokio::test]
async fn recovery_should_answer_alike_for_wrong_handle_and_wrong_code() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let wrong_handle = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": "nobody", "recovery_code": fx.member_code}),
    )
    .await;
    let wrong_code = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": fx.admin_code}),
    )
    .await;
    assert_eq!(wrong_handle.status, StatusCode::BAD_REQUEST);
    assert_eq!(wrong_handle.body["code"], "invalid_recovery");
    assert_eq!(
        (wrong_handle.status, &wrong_handle.body),
        (wrong_code.status, &wrong_code.body)
    );

    let typed = fx.member_code.replace('-', "").to_uppercase();
    let right = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": " BOB ", "recovery_code": typed}),
    )
    .await;
    assert_eq!(right.status, StatusCode::OK, "{}", right.body);
}

#[tokio::test]
async fn delete_account_should_remove_every_owned_row() {
    let instance = Instance::new(None);
    let setup = set_up_admin(&instance, &mut SoftAuthenticator::new(51)).await;
    let admin = session_of(&setup);
    seed(
        &instance.db,
        &instance.account("admin"),
        "Admin",
        String::new(),
        String::new(),
    );
    let token = mint_invite(&instance.app, &admin).await;
    let (instance, before) = instance.dump_and_reopen();

    let mut key = SoftAuthenticator::new(52);
    let joined = register(&instance.app, &token, "bob", &mut key).await;
    assert_eq!(joined.status, StatusCode::OK, "{}", joined.body);
    let cookie = session_of(&joined);
    let bob = instance.account("bob");
    let ids = seed(&instance.db, &bob, "Bob", String::new(), String::new());
    let second = sign_in(&instance.app, &mut key, &[]).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    mint_token(&instance.app, &cookie).await;
    let timer = call(
        &instance.app,
        "POST",
        "/api/timer/start",
        &[("cookie", &cookie)],
        Some(json!({"task_id": ids.task, "mode": "pomodoro"})),
    )
    .await;
    assert_eq!(timer.status, StatusCode::OK, "{}", timer.body);
    let owned = instance.db.account(&bob);
    owned
        .update_settings(&Settings::default())
        .expect("settings");
    owned
        .create_entry_full(NewTimeEntry {
            task_id: None,
            started_at: "2026-01-02T10:00:00Z",
            minutes: 5,
            seconds: 300,
            note: "",
            kind: EntryKind::ShortBreak,
            interrupted: false,
            interrupt_reason: None,
            labels: &[],
            created_by: "bob",
            activity_id: None,
        })
        .expect("entry");
    let device = call(
        &instance.app,
        "POST",
        "/api/v1/auth/device",
        &[],
        Some(json!({"client_name": "cli", "scopes": ["read"]})),
    )
    .await;
    assert_eq!(device.status, StatusCode::OK, "{}", device.body);
    let user_code = normalize_user_code(device.body["user_code"].as_str().expect("user code"));
    let now = chrono::Utc::now().timestamp();
    let grant = DeviceGrant {
        scope: AuthScope::Read,
        expires_at: None,
    };
    assert!(
        owned
            .decide_device_auth(&user_code, Some(&grant), now)
            .expect("approve")
    );
    let invite = InviteRow {
        id: uuid::Uuid::new_v4().to_string(),
        created_by: bob.as_str().to_string(),
        created_at: now,
        expires_at: now + SECONDS_PER_DAY,
    };
    instance
        .db
        .create_invite(&hash_invite_token("bobs-invite"), &invite, now)
        .expect("invite");
    drop(owned);
    let (instance, during) = instance.dump_and_reopen();
    for table in OWNED_TABLES {
        assert_ne!(
            during.get(table),
            before.get(table),
            "{table} holds none of Bob's rows"
        );
    }

    let deleted = delete_account(&instance.app, &cookie, "bob").await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert!(
        deleted
            .cookie
            .as_deref()
            .is_some_and(|cookie| cookie.contains("Max-Age=0")),
        "the session cookie is cleared"
    );
    assert_eq!(
        whoami(&instance.app, &cookie).await.status,
        StatusCode::UNAUTHORIZED
    );
    let (_instance, after) = instance.dump_and_reopen();
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
    for (table, rows) in &before {
        if table == "invites" {
            continue;
        }
        assert_eq!(after.get(table), Some(rows), "{table} changed");
    }
    assert_eq!(before["invites"].len(), 1, "the admin's invite");
    assert!(
        after["invites"].is_empty(),
        "Bob used the admin's invite and his own is gone"
    );
}

#[tokio::test]
async fn delete_account_should_require_handle() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    for wrong in ["", "admin", "bobby", "BOB"] {
        let refused = delete_account(app, &fx.member, wrong).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{wrong:?}");
    }
    let missing = call(
        app,
        "POST",
        "/api/v1/account/delete",
        &[("cookie", &fx.member)],
        Some(json!({})),
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);

    let token = mint_token(app, &fx.member).await;
    let bearer = format!("Bearer {token}");
    let refused = call(
        app,
        "POST",
        "/api/v1/account/delete",
        &[("authorization", &bearer)],
        Some(json!({"handle": "bob"})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["code"], "session_required");
    assert_eq!(whoami(app, &fx.member).await.status, StatusCode::OK);
}

#[tokio::test]
async fn admin_delete_should_refuse_while_others_exist() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let refused = delete_account(app, &fx.admin, "admin").await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert!(
        refused
            .body
            .to_string()
            .contains("other accounts still exist"),
        "{}",
        refused.body
    );
    assert_eq!(whoami(app, &fx.admin).await.status, StatusCode::OK);

    assert_eq!(
        delete_account(app, &fx.member, "bob").await.status,
        StatusCode::OK
    );
    let alone = delete_account(app, &fx.admin, "admin").await;
    assert_eq!(alone.status, StatusCode::OK, "{}", alone.body);
    assert_eq!(fx.instance.db.user_count().expect("count"), 0);
}

#[tokio::test]
async fn member_settings_should_render_without_instance_panes() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let page = |cookie: &str| {
        let cookie = cookie.to_string();
        async move {
            let reply = call(app, "GET", "/settings", &[("cookie", &cookie)], None).await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
            reply.body.as_str().expect("html").to_string()
        }
    };
    let member = page(&fx.member).await;
    for pane in [
        "id=\"timer\"",
        "id=\"passkeys\"",
        "id=\"recovery\"",
        "id=\"delete-account\"",
    ] {
        assert!(member.contains(pane), "the member's settings lack {pane}");
    }
    for pane in ["id=\"backups\"", "id=\"invites\""] {
        assert!(!member.contains(pane), "the member's settings show {pane}");
    }
    let admin = page(&fx.admin).await;
    for pane in ["id=\"backups\"", "id=\"invites\"", "id=\"recovery\""] {
        assert!(admin.contains(pane), "the admin's settings lack {pane}");
    }
    for uri in ["/api/v1/backups", "/api/v1/admin/invites"] {
        let refused = call(app, "GET", uri, &[("cookie", &fx.member)], None).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{uri}");
    }
}

#[tokio::test]
async fn migrated_owner_should_see_the_recovery_banner_until_it_creates_a_code() {
    let instance = Instance::new(None);
    let owner = instance.db.create_user("Chip").expect("owner");
    let cookie = format!(
        "session={}",
        instance.db.create_session(&owner).expect("session")
    );
    let shown = get(&instance.app, "/settings", &cookie).await;
    assert!(
        shown.body.to_string().contains("recovery-banner"),
        "{}",
        shown.body
    );
    let created = call(
        &instance.app,
        "POST",
        "/api/v1/account/recovery-code",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let code = recovery_code(&created);
    let hidden = get(&instance.app, "/settings", &cookie).await;
    assert!(!hidden.body.to_string().contains("recovery-banner"));
    let proven = start(
        &instance.app,
        "/api/v1/auth/recover",
        json!({"handle": "chip", "recovery_code": code}),
    )
    .await;
    assert_eq!(proven.status, StatusCode::OK, "{}", proven.body);
}

#[tokio::test]
async fn bootstrap_token_should_act_as_admin_in_multi_mode() {
    let fx = pair(Some(BOOTSTRAP)).await;
    let app = &fx.instance.app;
    let bearer = format!("Bearer {BOOTSTRAP}");
    let me = call(
        app,
        "GET",
        "/api/v1/auth/whoami",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(me.body["kind"], "bootstrap", "{}", me.body);
    assert_eq!(me.body["handle"], "admin", "{}", me.body);

    let theirs = call(
        app,
        "POST",
        "/api/boards",
        &[("cookie", &fx.member)],
        Some(json!({"name": "Bob's board"})),
    )
    .await;
    assert!(theirs.status.is_success(), "{}", theirs.body);
    let made = call(
        app,
        "POST",
        "/api/boards",
        &[("authorization", &bearer)],
        Some(json!({"name": "Ops"})),
    )
    .await;
    assert!(made.status.is_success(), "{}", made.body);
    let board = fx
        .instance
        .db
        .board(made.body["id"].as_str().expect("board id"))
        .expect("read")
        .expect("board");
    assert_eq!(board.owner_id, fx.instance.account("admin").as_str());
    let listed = call(
        app,
        "GET",
        "/api/boards",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    let text = listed.body.to_string();
    assert!(text.contains("Ops"), "{text}");
    assert!(!text.contains("Bob's board"), "{text}");
    let theirs_id = theirs.body["id"].as_str().expect("board id");
    let foreign = call(
        app,
        "GET",
        &format!("/api/boards/{theirs_id}"),
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
}

/// Returns the ids of the unused invites the admin's `cookie` lists.
async fn invite_ids(app: &axum::Router, cookie: &str) -> Vec<String> {
    let listed = get(app, "/api/v1/admin/invites", cookie).await;
    listed
        .body
        .as_array()
        .expect("invite list")
        .iter()
        .map(|invite| invite["id"].as_str().expect("invite id").to_string())
        .collect()
}

/// Revokes invite `id` with the session `cookie`.
async fn revoke(app: &axum::Router, cookie: &str, id: &str) -> Reply {
    let uri = format!("/api/v1/admin/invites/{id}");
    call(app, "DELETE", &uri, &[("cookie", cookie)], None).await
}

#[tokio::test]
async fn invite_revoke_should_be_admin_only_and_stop_a_started_registration() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let token = mint_invite(app, &fx.admin).await;
    let ids = invite_ids(app, &fx.admin).await;
    assert_eq!(ids.len(), 1);
    assert_eq!(
        revoke(app, &fx.member, &ids[0]).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        revoke(app, &fx.admin, "no-such-invite").await.status,
        StatusCode::NOT_FOUND
    );

    let started = start(
        app,
        "/api/v1/auth/register",
        json!({"invite": token, "handle": "carol"}),
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let revoked = revoke(app, &fx.admin, &ids[0]).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body["revoked"], ids[0].as_str());
    let finished = finish(
        app,
        "/api/v1/auth/register",
        &started,
        &mut SoftAuthenticator::new(35),
    )
    .await;
    assert_eq!(finished.status, StatusCode::GONE, "{}", finished.body);
    assert_eq!(finished.body["code"], "gone");
    assert!(
        fx.instance
            .db
            .account_by_handle("carol")
            .expect("read")
            .is_none()
    );
    assert!(invite_ids(app, &fx.admin).await.is_empty());
}

#[tokio::test]
async fn recovery_should_refuse_a_code_regenerated_after_it_started() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let started = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": fx.member_code}),
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let regenerated = call(
        app,
        "POST",
        "/api/v1/account/recovery-code",
        &[("cookie", &fx.member)],
        None,
    )
    .await;
    assert_eq!(regenerated.status, StatusCode::OK, "{}", regenerated.body);
    let finished = finish(
        app,
        "/api/v1/auth/recover",
        &started,
        &mut SoftAuthenticator::new(42),
    )
    .await;
    assert_eq!(
        finished.status,
        StatusCode::BAD_REQUEST,
        "{}",
        finished.body
    );
    assert_eq!(finished.body["code"], "invalid_recovery");
    let passkeys = get(app, "/api/v1/auth/passkeys", &fx.member).await;
    assert_eq!(
        passkeys.body.as_array().map(Vec::len),
        Some(1),
        "{}",
        passkeys.body
    );
}

#[tokio::test]
async fn regenerating_a_recovery_code_should_spend_the_old_one_and_need_the_session() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let regenerated = call(
        app,
        "POST",
        "/api/v1/account/recovery-code",
        &[("cookie", &fx.member)],
        None,
    )
    .await;
    assert_eq!(regenerated.status, StatusCode::OK, "{}", regenerated.body);
    let new_code = recovery_code(&regenerated);
    assert_ne!(new_code, fx.member_code);
    let old = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": fx.member_code}),
    )
    .await;
    assert_eq!(old.status, StatusCode::BAD_REQUEST, "{}", old.body);
    assert_eq!(old.body["code"], "invalid_recovery");
    let new = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": new_code}),
    )
    .await;
    assert_eq!(new.status, StatusCode::OK, "{}", new.body);

    let bearer = format!("Bearer {}", mint_token(app, &fx.member).await);
    let refused = call(
        app,
        "POST",
        "/api/v1/account/recovery-code",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["code"], "session_required");
}

#[tokio::test]
async fn admin_alone_should_delete_its_account_and_reopen_setup() {
    let instance = Instance::new(None);
    let old_code = instance.code.clone();
    let setup = set_up_admin(&instance, &mut SoftAuthenticator::new(71)).await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    let cookie = session_of(&setup);
    let deleted = delete_account(&instance.app, &cookie, "admin").await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(instance.db.user_count().expect("count"), 0);
    assert_eq!(instance.db.passkey_count().expect("count"), 0);

    let page = call(&instance.app, "GET", "/setup", &[], None).await;
    assert_eq!(page.status, StatusCode::OK, "setup is open again");
    let stale = start(
        &instance.app,
        "/api/v1/auth/setup",
        json!({"username": "again", "code": old_code}),
    )
    .await;
    assert_eq!(stale.status, StatusCode::FORBIDDEN, "{}", stale.body);
}

#[tokio::test]
async fn single_mode_should_keep_the_multi_user_routes_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::connect(dir.path().join("single.redb").to_str().expect("utf8")).expect("connect");
    let sign_in = passkey_mode();
    let code = sign_in
        .relying_party()
        .expect("relying party")
        .issue_setup_code();
    let app = router(AppState::new(db, None, sign_in).with_limits(unthrottled_limits()));
    let setup = enroll(
        &app,
        &mut SoftAuthenticator::new(61),
        "/api/v1/auth/setup",
        json!({"username": "Chip Senkbeil", "code": code}),
    )
    .await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    assert_eq!(
        setup.body,
        json!({"redirect": "/"}),
        "single mode shows no recovery code"
    );
    let cookie = session_of(&setup);
    for (method, uri) in [
        ("GET", "/signup/anything"),
        ("GET", "/recover"),
        ("POST", "/api/v1/auth/register/options"),
        ("POST", "/api/v1/auth/register"),
        ("POST", "/api/v1/auth/recover/options"),
        ("POST", "/api/v1/auth/recover"),
        ("POST", "/api/v1/admin/invites"),
        ("DELETE", "/api/v1/admin/invites/anything"),
        ("GET", "/api/v1/admin/invites"),
        ("POST", "/api/v1/account/recovery-code"),
        ("POST", "/api/v1/account/delete"),
    ] {
        let body = (method == "POST").then(|| json!({"handle": "Chip Senkbeil"}));
        let reply = call(&app, method, uri, &[("cookie", &cookie)], body).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{method} {uri}: {}",
            reply.body
        );
    }
    let login = call(&app, "GET", "/login", &[], None).await;
    assert!(!login.body.to_string().contains("/recover"));
    let settings = call(&app, "GET", "/settings", &[("cookie", &cookie)], None).await;
    let html = settings.body.as_str().expect("html");
    assert!(html.contains("id=\"backups\""));
    assert!(!html.contains("id=\"recovery\"") && !html.contains("id=\"invites\""));
}
