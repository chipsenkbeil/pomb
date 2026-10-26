use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::http::{Method, StatusCode};
use redb::{
    MultimapTableDefinition, MultimapTableHandle, ReadableDatabase, ReadableMultimapTable,
    ReadableTable, TableDefinition, TableHandle,
};
use serde_json::{Value, json};

use super::access_tests::{enroll, mint_token, seed};
use super::passkey_tests::{Reply, SoftAuthenticator, call, session_of, sign_in};
use super::server::{TEST_PUBLIC_URL, passkey_mode, unthrottled_limits};
use super::{ROUTE_ACCESS, RouteClass, router};
use crate::AppState;
use crate::auth::{hash_invite_token, new_api_token_record, normalize_user_code};
use crate::db::{
    AccountAction, AdminActionOutcome, BootstrapToken, Db, DeviceGrant, ModeSwitch, NewTimeEntry,
};
use crate::models::{
    AccountId, AccountStatus, AuthScope, EntryKind, InstanceMode, InstanceSettings, InviteRow,
    SECONDS_PER_DAY, Settings,
};

/// Fewest Admin-class routes the member refusal test must cover: backups, invites, accounts,
/// settings and the admin page.
const MIN_ADMIN_ROUTES: usize = 13;

/// The bootstrap token the instances under test accept.
const BOOTSTRAP: &str = "pomb_multi_bootstrap_token_for_tests";

/// The tables an account's rows live in, each of which the deleted account fills.
const OWNED_TABLES: [&str; 28] = [
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
    "activity",
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
    for pane in ["id=\"backups\"", "href=\"/admin\""] {
        assert!(!member.contains(pane), "the member's settings show {pane}");
    }
    let admin = page(&fx.admin).await;
    for pane in ["id=\"backups\"", "href=\"/admin\"", "id=\"recovery\""] {
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
        ("GET", "/signup"),
        ("GET", "/pending"),
        ("GET", "/admin"),
        ("GET", "/api/v1/admin/accounts"),
        ("POST", "/api/v1/admin/accounts/anything/approve"),
        ("POST", "/api/v1/admin/accounts/anything/suspend"),
        ("POST", "/api/v1/admin/accounts/anything/unsuspend"),
        ("DELETE", "/api/v1/admin/accounts/anything"),
        ("PUT", "/api/v1/admin/settings"),
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

/// Sets the instance's registration mode to `mode` with the default pending-account cap, with
/// the admin's session `cookie`.
async fn set_registration(app: &axum::Router, cookie: &str, mode: &str) -> Reply {
    set_admin_settings(app, cookie, mode, InstanceSettings::DEFAULT_PENDING_CAP).await
}

/// Sets the instance's registration mode and pending-account cap with the admin's session
/// `cookie`.
async fn set_admin_settings(app: &axum::Router, cookie: &str, mode: &str, cap: u32) -> Reply {
    call(
        app,
        "PUT",
        "/api/v1/admin/settings",
        &[("cookie", cookie)],
        Some(json!({"registration": mode, "pending_cap": cap})),
    )
    .await
}

/// Requests an account for `handle` without an invite, with `authenticator`.
async fn request_account(
    app: &axum::Router,
    handle: &str,
    authenticator: &mut SoftAuthenticator,
) -> Reply {
    enroll(
        app,
        authenticator,
        "/api/v1/auth/register",
        json!({"handle": handle}),
    )
    .await
}

/// Runs an admin action on account `id` with the session `cookie`.
async fn admin_action(app: &axum::Router, cookie: &str, id: &AccountId, action: &str) -> Reply {
    let uri = format!("/api/v1/admin/accounts/{}/{action}", id.as_str());
    call(app, "POST", &uri, &[("cookie", cookie)], None).await
}

/// A pair whose instance takes requests to join, plus `carol`, who requested one.
struct Requested {
    /// The admin, the member and their instance.
    fx: Pair,
    /// Carol's session cookie.
    carol: String,
    /// Carol's account.
    carol_id: AccountId,
}

/// Opens requests to join on a fresh pair and has `Carol` request an account.
async fn requested() -> Requested {
    let fx = pair(None).await;
    let opened = set_registration(&fx.instance.app, &fx.admin, "request").await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    assert_eq!(
        opened.body,
        json!({"registration": "request", "pending_cap": InstanceSettings::DEFAULT_PENDING_CAP})
    );
    let asked = request_account(&fx.instance.app, "Carol", &mut SoftAuthenticator::new(81)).await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);
    recovery_code(&asked);
    Requested {
        carol: session_of(&asked),
        carol_id: fx.instance.account("carol"),
        fx,
    }
}

/// Returns the status `id` has in the store.
fn status_of(instance: &Instance, id: &AccountId) -> AccountStatus {
    instance
        .db
        .user(id.as_str())
        .expect("read")
        .expect("account")
        .status
}

#[tokio::test]
async fn invite_mode_should_refuse_open_signup() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let page = call(app, "GET", "/signup", &[], None).await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    assert!(
        page.body.to_string().contains("Registration is closed"),
        "{}",
        page.body
    );
    let refused = start(app, "/api/v1/auth/register", json!({"handle": "carol"})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("registration_closed"));

    assert_eq!(
        set_registration(app, &fx.admin, "request").await.status,
        StatusCode::OK
    );
    let started = start(app, "/api/v1/auth/register", json!({"handle": "carol"})).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    assert_eq!(
        set_registration(app, &fx.admin, "invite").await.status,
        StatusCode::OK
    );
    let finished = finish(
        app,
        "/api/v1/auth/register",
        &started,
        &mut SoftAuthenticator::new(82),
    )
    .await;
    assert_eq!(finished.status, StatusCode::FORBIDDEN, "{}", finished.body);
    assert_eq!(finished.code.as_deref(), Some("registration_closed"));
    assert!(
        fx.instance
            .db
            .account_by_handle("carol")
            .expect("read")
            .is_none()
    );
}

#[tokio::test]
async fn request_mode_should_create_pending_accounts() {
    let Requested {
        fx,
        carol: _,
        carol_id,
    } = requested().await;
    let app = &fx.instance.app;
    let page = call(app, "GET", "/signup", &[], None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.to_string().contains("Request account"));
    assert_eq!(status_of(&fx.instance, &carol_id), AccountStatus::Pending);

    let token = mint_invite(app, &fx.admin).await;
    let joined = register(app, &token, "dave", &mut SoftAuthenticator::new(83)).await;
    assert_eq!(joined.status, StatusCode::OK, "{}", joined.body);
    let dave = fx.instance.account("dave");
    assert_eq!(
        status_of(&fx.instance, &dave),
        AccountStatus::Active,
        "an invite still makes an active account in request mode"
    );
    let refused = set_registration(app, &fx.admin, "open").await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
}

#[tokio::test]
async fn pending_account_should_see_only_the_pending_page() {
    let Requested { fx, carol, .. } = requested().await;
    let app = &fx.instance.app;
    let boards = get(app, "/api/boards", &carol).await;
    assert_eq!(boards.status, StatusCode::FORBIDDEN, "{}", boards.body);
    assert_eq!(boards.code.as_deref(), Some("account_pending"));
    for page in ["/", "/settings", "/admin", "/device"] {
        let sent = get(app, page, &carol).await;
        assert_eq!(sent.status, StatusCode::SEE_OTHER, "{page}");
        assert_eq!(sent.location.as_deref(), Some("/pending"), "{page}");
    }
    let pending = get(app, "/pending", &carol).await;
    assert_eq!(pending.status, StatusCode::OK);
    assert!(pending.body.to_string().contains("admin will approve"));

    let minted = call(
        app,
        "POST",
        "/api/v1/auth/tokens",
        &[("cookie", &carol)],
        Some(json!({"name": "agent", "scopes": ["read"]})),
    )
    .await;
    assert_eq!(minted.status, StatusCode::FORBIDDEN);
    assert_eq!(minted.code.as_deref(), Some("account_pending"));
    let carol_id = fx.instance.account("carol");
    let (record, _) = new_api_token_record(&carol_id, "agent", AuthScope::Read, None);
    assert!(
        fx.instance.db.create_api_token(&record).is_err(),
        "a pending account's tokens can't be minted at all"
    );

    let out = call(app, "POST", "/logout", &[("cookie", &carol)], None).await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    assert_eq!(
        get(app, "/pending", &carol).await.status,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn approve_should_activate() {
    let Requested {
        fx,
        carol,
        carol_id,
    } = requested().await;
    let app = &fx.instance.app;
    let boards = |db: &Db| db.account(&carol_id).list_boards().expect("boards").len();
    assert_eq!(boards(&fx.instance.db), 0, "a request owns no board");
    let approved = admin_action(app, &fx.admin, &carol_id, "approve").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["status"], "active");
    assert_eq!(status_of(&fx.instance, &carol_id), AccountStatus::Active);
    assert_eq!(
        boards(&fx.instance.db),
        1,
        "approval builds the starter board"
    );
    assert_eq!(approved.body["board_count"], 1);
    assert_eq!(get(app, "/api/boards", &carol).await.status, StatusCode::OK);
    let home = get(app, "/pending", &carol).await;
    assert_eq!(home.location.as_deref(), Some("/"));
    let again = admin_action(app, &fx.admin, &carol_id, "approve").await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
}

#[tokio::test]
async fn suspend_should_revoke_sessions_and_tokens() {
    let mut fx = pair(None).await;
    let app = fx.instance.app.clone();
    let bob = fx.instance.account("bob");
    let bearer = format!("Bearer {}", mint_token(&app, &fx.member).await);
    let boards = fx.instance.db.account(&bob).list_boards().expect("boards");
    assert!(!boards.is_empty());
    let feed = call(
        &app,
        "POST",
        &format!("/api/boards/{}/calendar-feed", boards[0].id),
        &[("cookie", &fx.member)],
        None,
    )
    .await;
    assert_eq!(feed.status, StatusCode::OK, "{}", feed.body);
    let feed = feed.body["path"].as_str().expect("feed path").to_string();
    assert_eq!(
        call(&app, "GET", &feed, &[], None).await.status,
        StatusCode::OK
    );

    let suspended = admin_action(&app, &fx.admin, &bob, "suspend").await;
    assert_eq!(suspended.status, StatusCode::OK, "{}", suspended.body);
    assert_eq!(suspended.body["status"], "suspended");
    let old_session = whoami(&app, &fx.member).await;
    assert_eq!(old_session.status, StatusCode::UNAUTHORIZED);
    let old_token = call(
        &app,
        "GET",
        "/api/boards",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(
        old_token.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        old_token.body
    );
    assert_eq!(old_token.body["code"], "account_suspended");
    assert!(
        fx.instance
            .db
            .list_api_tokens()
            .expect("tokens")
            .iter()
            .all(|token| token.account_id != bob.as_str()),
        "the token row is deleted with the suspension"
    );
    let signed = sign_in(&app, &mut fx.member_key, &[]).await;
    assert_eq!(signed.status, StatusCode::UNAUTHORIZED, "{}", signed.body);
    assert_eq!(signed.code.as_deref(), Some("account_suspended"));
    assert!(signed.cookie.is_none());
    let recovering = start(
        &app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": fx.member_code}),
    )
    .await;
    assert_eq!(recovering.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        fx.instance
            .db
            .account(&bob)
            .list_boards()
            .expect("boards")
            .len(),
        boards.len(),
        "a suspended account keeps its data"
    );
    assert_eq!(
        call(&app, "GET", &feed, &[], None).await.status,
        StatusCode::NOT_FOUND,
        "a suspended account's calendar feed is gone"
    );

    let deleted = call(
        &app,
        "DELETE",
        &format!("/api/v1/admin/accounts/{}", bob.as_str()),
        &[("cookie", &fx.admin)],
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    let gone = call(
        &app,
        "GET",
        "/api/boards",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(gone.body["code"], "token_invalid", "{}", gone.body);
    assert!(fx.instance.db.user(bob.as_str()).expect("read").is_none());
}

#[tokio::test]
async fn unsuspend_should_allow_sign_in() {
    let mut fx = pair(None).await;
    let app = fx.instance.app.clone();
    let bob = fx.instance.account("bob");
    let bearer = format!("Bearer {}", mint_token(&app, &fx.member).await);
    assert_eq!(
        admin_action(&app, &fx.admin, &bob, "suspend").await.status,
        StatusCode::OK
    );
    let lifted = admin_action(&app, &fx.admin, &bob, "unsuspend").await;
    assert_eq!(lifted.status, StatusCode::OK, "{}", lifted.body);
    assert_eq!(lifted.body["status"], "active");
    let signed = sign_in(&app, &mut fx.member_key, &[]).await;
    assert_eq!(signed.status, StatusCode::OK, "{}", signed.body);
    assert_eq!(
        whoami(&app, &session_of(&signed)).await.status,
        StatusCode::OK
    );
    let old_token = call(
        &app,
        "GET",
        "/api/boards",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(old_token.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        old_token.body["code"], "token_invalid",
        "the old token stays deleted"
    );
    let again = admin_action(&app, &fx.admin, &bob, "unsuspend").await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
}

#[tokio::test]
async fn pending_should_expire_after_30_days() {
    let Requested {
        fx,
        carol,
        carol_id,
    } = requested().await;
    let db = &fx.instance.db;
    let requested_at = db
        .user(carol_id.as_str())
        .expect("read")
        .expect("carol")
        .created_at;
    let deadline = requested_at + Db::PENDING_REQUEST_DAYS * SECONDS_PER_DAY;
    assert_eq!(Db::PENDING_REQUEST_DAYS, 30);
    let expire = |at: i64| {
        let mut handles = Vec::new();
        db.expire_pending_requests(at, |handle| handles.push(handle.to_string()))
            .expect("expire");
        handles
    };
    assert!(expire(deadline - 1).is_empty());
    assert!(db.user(carol_id.as_str()).expect("read").is_some());
    assert_eq!(expire(deadline), vec!["carol".to_string()]);
    assert!(db.user(carol_id.as_str()).expect("read").is_none());
    assert!(
        db.account(&carol_id)
            .list_boards()
            .expect("boards")
            .is_empty()
    );
    assert!(!db.handle_taken("carol").expect("read"));
    assert_eq!(
        get(&fx.instance.app, "/pending", &carol)
            .await
            .location
            .as_deref(),
        Some("/login")
    );
    let bob = fx.instance.account("bob");
    assert_eq!(status_of(&fx.instance, &bob), AccountStatus::Active);
}

#[tokio::test]
async fn admin_routes_should_refuse_members() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let admin = fx.instance.account("admin");
    let member_bearer = format!("Bearer {}", mint_token(app, &fx.member).await);
    let admin_bearer = format!("Bearer {}", mint_token(app, &fx.admin).await);
    let mut covered = 0;
    for (method, path, class) in ROUTE_ACCESS {
        if *class != RouteClass::Admin {
            continue;
        }
        let uri = path.replace(":id", admin.as_str());
        let body = (*method != Method::GET && *method != Method::DELETE).then(|| json!({}));
        for (who, header) in [
            ("member session", ("cookie", fx.member.as_str())),
            ("member token", ("authorization", member_bearer.as_str())),
        ] {
            let refused = call(app, method.as_str(), &uri, &[header], body.clone()).await;
            assert_eq!(
                (refused.status, refused.code.as_deref()),
                (StatusCode::FORBIDDEN, Some("forbidden")),
                "{method} {path} as the {who}: {}",
                refused.body
            );
        }
        let token = call(
            app,
            method.as_str(),
            &uri,
            &[("authorization", &admin_bearer)],
            body,
        )
        .await;
        assert_eq!(
            (token.status, token.code.as_deref()),
            (StatusCode::FORBIDDEN, Some("session_required")),
            "{method} {path} with the admin's token: {}",
            token.body
        );
        covered += 1;
    }
    assert!(
        covered >= MIN_ADMIN_ROUTES,
        "covered only {covered} admin routes"
    );
}

#[tokio::test]
async fn admin_view_should_hold_no_board_content() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let bob = fx.instance.account("bob");
    let made = call(
        app,
        "POST",
        "/api/boards",
        &[("cookie", &fx.member)],
        Some(json!({"name": "Bob's secret plans"})),
    )
    .await;
    assert!(made.status.is_success(), "{}", made.body);
    let listed = get(app, "/api/v1/admin/accounts", &fx.admin).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let accounts = listed.body.as_array().expect("account list");
    assert_eq!(accounts.len(), 2);
    for account in accounts {
        let mut fields: Vec<&str> = account
            .as_object()
            .expect("account object")
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "board_count",
                "created_at",
                "handle",
                "id",
                "last_seen_at",
                "role",
                "status"
            ]
        );
    }
    let entry = accounts
        .iter()
        .find(|account| account["id"] == bob.as_str())
        .expect("bob");
    assert_eq!(entry["handle"], "bob");
    assert_eq!(entry["role"], "member");
    assert_eq!(entry["status"], "active");
    assert_eq!(entry["board_count"], 2);
    assert!(entry["last_seen_at"].is_i64(), "{entry}");
    let page = get(app, "/admin", &fx.admin).await;
    assert_eq!(page.status, StatusCode::OK);
    for text in [listed.body.to_string(), page.body.to_string()] {
        assert!(!text.contains("secret plans"), "{text}");
        assert!(!text.contains(made.body["id"].as_str().expect("board id")));
    }
}

#[tokio::test]
async fn admin_should_not_suspend_or_delete_itself() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let admin = fx.instance.account("admin");
    let suspended = admin_action(app, &fx.admin, &admin, "suspend").await;
    assert_eq!(
        suspended.status,
        StatusCode::FORBIDDEN,
        "{}",
        suspended.body
    );
    let deleted = call(
        app,
        "DELETE",
        &format!("/api/v1/admin/accounts/{}", admin.as_str()),
        &[("cookie", &fx.admin)],
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::FORBIDDEN, "{}", deleted.body);
    assert_eq!(status_of(&fx.instance, &admin), AccountStatus::Active);
    assert_eq!(whoami(app, &fx.admin).await.status, StatusCode::OK);
    let unknown = admin_action(app, &fx.admin, &AccountId::new("nobody"), "approve").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pending_cap_should_refuse_registration() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let opened = set_admin_settings(app, &fx.admin, "request", 2).await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    for cap in [0, InstanceSettings::MAX_PENDING_CAP + 1] {
        let refused = set_admin_settings(app, &fx.admin, "request", cap).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "cap {cap}");
    }

    let mut started = Vec::new();
    for (n, handle) in ["carol", "dave", "erin"].into_iter().enumerate() {
        let reply = start(app, "/api/v1/auth/register", json!({"handle": handle})).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        started.push((reply, SoftAuthenticator::new(90 + n as u8)));
    }
    let [mut a, mut b, mut c]: [(Reply, SoftAuthenticator); 3] = started
        .try_into()
        .unwrap_or_else(|_| panic!("three started"));
    let route = "/api/v1/auth/register";
    let (a, b, c) = tokio::join!(
        finish(app, route, &a.0, &mut a.1),
        finish(app, route, &b.0, &mut b.1),
        finish(app, route, &c.0, &mut c.1),
    );
    let replies = [a, b, c];
    let mut statuses: Vec<StatusCode> = replies.iter().map(|reply| reply.status).collect();
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS
        ],
        "racing requests past the cap"
    );
    let full = replies
        .iter()
        .find(|reply| reply.status == StatusCode::TOO_MANY_REQUESTS)
        .expect("refused");
    assert_eq!(full.code.as_deref(), Some("pending_full"));

    let token = mint_invite(app, &fx.admin).await;
    let invited = register(app, &token, "frank", &mut SoftAuthenticator::new(95)).await;
    assert_eq!(
        invited.status,
        StatusCode::OK,
        "an invite ignores the cap: {}",
        invited.body
    );
}

#[tokio::test]
async fn device_poll_should_deny_a_suspended_approver() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let bob = fx.instance.account("bob");
    let device = call(
        app,
        "POST",
        "/api/v1/auth/device",
        &[],
        Some(json!({"client_name": "cli", "scopes": ["read"]})),
    )
    .await;
    assert_eq!(device.status, StatusCode::OK, "{}", device.body);
    let user_code = normalize_user_code(device.body["user_code"].as_str().expect("user code"));
    let grant = DeviceGrant {
        scope: AuthScope::Read,
        expires_at: None,
    };
    let now = chrono::Utc::now().timestamp();
    assert!(
        fx.instance
            .db
            .account(&bob)
            .decide_device_auth(&user_code, Some(&grant), now)
            .expect("approve")
    );
    for action in [AccountAction::Suspend, AccountAction::Unsuspend] {
        assert!(matches!(
            fx.instance
                .db
                .apply_account_action(bob.as_str(), action)
                .expect("status change"),
            AdminActionOutcome::Done(_)
        ));
    }
    let poll = call(
        app,
        "POST",
        "/api/v1/auth/device/token",
        &[],
        Some(json!({"device_code": device.body["device_code"]})),
    )
    .await;
    assert_eq!(poll.status, StatusCode::FORBIDDEN, "{}", poll.body);
    assert_eq!(poll.code.as_deref(), Some("access_denied"));
    assert!(
        fx.instance
            .db
            .list_api_tokens()
            .expect("tokens")
            .iter()
            .all(|token| token.account_id != bob.as_str())
    );
}

#[tokio::test]
async fn recovery_should_refuse_an_account_suspended_after_it_started() {
    let fx = pair(None).await;
    let app = &fx.instance.app;
    let bob = fx.instance.account("bob");
    let started = start(
        app,
        "/api/v1/auth/recover",
        json!({"handle": "bob", "recovery_code": fx.member_code}),
    )
    .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let code_before = fx
        .instance
        .db
        .recovery_code(&bob)
        .expect("read")
        .expect("code")
        .hash;
    let passkeys_before = fx.instance.db.passkey_count().expect("count");
    assert_eq!(
        admin_action(app, &fx.admin, &bob, "suspend").await.status,
        StatusCode::OK
    );
    let finished = finish(
        app,
        "/api/v1/auth/recover",
        &started,
        &mut SoftAuthenticator::new(96),
    )
    .await;
    assert_eq!(
        finished.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        finished.body
    );
    assert_eq!(finished.code.as_deref(), Some("account_suspended"));
    assert!(finished.cookie.is_none());
    assert_eq!(
        fx.instance.db.passkey_count().expect("count"),
        passkeys_before
    );
    assert_eq!(
        fx.instance
            .db
            .recovery_code(&bob)
            .expect("read")
            .expect("code")
            .hash,
        code_before
    );
}

#[test]
fn docs_should_name_the_pending_request_lifetime() {
    let days = format!("{} days", Db::PENDING_REQUEST_DAYS);
    let readme = include_str!("../../README.md").replace('\n', " ");
    assert!(readme.contains(&days), "the README names {days}");
    assert!(
        super::ACCOUNTS_DOC.replace('\n', " ").contains(&days),
        "the Accounts prose names {days}"
    );
    let cap = format!("{} by default", InstanceSettings::DEFAULT_PENDING_CAP);
    assert!(readme.contains(&cap), "the README names {cap}");
    let cap = format!("{} unless set", InstanceSettings::DEFAULT_PENDING_CAP);
    assert!(
        super::ACCOUNTS_DOC.replace('\n', " ").contains(&cap),
        "the Accounts prose names {cap}"
    );
}
