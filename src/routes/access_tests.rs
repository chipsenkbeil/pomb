use std::path::PathBuf;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::multi_tests::admin_settings;
use super::passkey_tests::{Reply, SoftAuthenticator, add_passkey, call, session_of};
use super::server::{passkey_mode, unthrottled_limits};
use super::{ROUTE_ACCESS, RouteClass, router};
use crate::AppState;
use crate::auth::{PENDING_ROUTES, new_api_token_record};
use crate::db::{
    AccountDb, BootstrapToken, Db, FencedWrite, FieldCreateOutcome, LinkAddOutcome, NewComment,
};
use crate::models::{
    AccountId, Actor, ActorKind, AuthScope, FieldDefRow, FieldKind, FieldOwner, InstanceMode,
    LinkKind,
};

/// Largest response body a test reads.
const MAX_BODY_BYTES: usize = 16 << 20;

/// Fewest Board, Task and Entity routes the cross-account matrix must attack.
const MIN_MATRIX_ROUTES: usize = 60;

/// Fewest routes the router source must yield, so a broken parse cannot pass the coverage test.
const MIN_REGISTERED_ROUTES: usize = 130;

/// Path parameters that name a row inside the board or task the route's main id names.
const SUB_ID_PARAMS: &[&str] = &[
    "sub_id",
    "link_id",
    "comment_id",
    "attachment_id",
    "color_id",
];

/// The ids one account's requests name.
#[derive(Clone, Debug)]
pub(super) struct Ids {
    /// The account itself.
    account: String,
    /// The board every other row sits on.
    board: String,
    /// A second board, for routes that name two.
    board2: String,
    /// A column on [`Ids::board`].
    column: String,
    /// A swimlane on [`Ids::board`].
    lane: String,
    /// A task in [`Ids::column`] and [`Ids::lane`].
    pub(super) task: String,
    /// A second task beside [`Ids::task`].
    task2: String,
    /// A subtask of [`Ids::task`].
    subtask: String,
    /// A link from [`Ids::task`] to [`Ids::task2`].
    link: String,
    /// A comment on [`Ids::task`].
    comment: String,
    /// An attachment on [`Ids::task`].
    attachment: String,
    /// A color in the palette of [`Ids::board`].
    color: String,
    /// A custom field on [`Ids::board`].
    field: String,
    /// A time entry logged on [`Ids::task`].
    entry: String,
    /// A template saved from [`Ids::board`].
    template: String,
    /// The account's API token.
    token: String,
    /// The account's passkey.
    passkey: String,
}

impl Ids {
    /// Returns every id, for blanking them out of an answer.
    fn all(&self) -> [&str; 17] {
        [
            &self.account,
            &self.board,
            &self.board2,
            &self.column,
            &self.lane,
            &self.task,
            &self.task2,
            &self.subtask,
            &self.link,
            &self.comment,
            &self.attachment,
            &self.color,
            &self.field,
            &self.entry,
            &self.template,
            &self.token,
            &self.passkey,
        ]
    }
}

/// Two accounts on one instance: A owns one of everything, and B attacks with its own token and
/// session.
struct Accounts {
    /// Keeps the database directory alive.
    _dir: tempfile::TempDir,
    /// The database file, compared byte for byte around an attack.
    path: PathBuf,
    /// The database behind [`Accounts::app`].
    db: Db,
    /// The router under test.
    app: axum::Router,
    /// Account A, the victim.
    a: AccountId,
    /// Account B, the attacker.
    b: AccountId,
    /// A's admin-scope API token.
    a_token: String,
    /// B's admin-scope API token.
    b_token: String,
    /// A's session cookie.
    a_cookie: String,
    /// B's session cookie.
    b_cookie: String,
    /// The rows A owns.
    a_ids: Ids,
    /// The rows B owns.
    b_ids: Ids,
    /// The session cookie of an account that waits for approval, in multi mode only.
    pending_cookie: Option<String>,
}

/// Credentials a request carries.
#[derive(Copy, Clone, Debug)]
enum Who<'a> {
    /// No credential at all.
    Anonymous,
    /// An API token.
    Token(&'a str),
    /// A session cookie.
    Session(&'a str),
}

/// A request body.
#[derive(Clone, Debug)]
enum Payload {
    /// A JSON body.
    Json(Value),
    /// A URL-encoded form body.
    Form(String),
    /// A raw `application/octet-stream` body.
    Raw(Vec<u8>),
}

/// Links `from` to `to` and returns the link's id.
fn seed_link(owned: &AccountDb, from: &str, to: &str) -> String {
    let from = owned.task(from).expect("read").expect("owned task");
    let to = owned.task(to).expect("read").expect("owned task");
    match owned
        .add_task_link(&from, LinkKind::Related, &to, Actor::SYSTEM)
        .expect("link")
    {
        LinkAddOutcome::Added(link) => link.id,
        other => panic!("link not added: {other:?}"),
    }
}

/// Comments on `task` as `author` and returns the comment's id.
fn seed_comment(owned: &AccountDb, task: &str, author: &str) -> String {
    let task = owned.task(task).expect("read").expect("owned task");
    let comment = NewComment {
        author,
        body: "secret comment",
        actor_kind: ActorKind::User,
        token_name: None,
        token_id: None,
        kind: None,
    };
    match owned
        .add_comment(&task, comment, Actor::SYSTEM, None)
        .expect("comment")
    {
        FencedWrite::Written(comment) => comment.id,
        other => panic!("comment not written: {other:?}"),
    }
}

/// Attaches a file to `task` and returns the attachment's id.
fn seed_attachment(owned: &AccountDb, task: &str) -> String {
    let task = owned.task(task).expect("read").expect("owned task");
    owned
        .add_attachment(&task, "a.txt", "text/plain", b"secret", Actor::SYSTEM)
        .expect("attachment")
        .expect("task")
        .id
}

/// Adds a text field to `board` and returns the field's id.
fn seed_field(db: &Db, board: &str) -> String {
    let field = FieldDefRow {
        id: String::new(),
        board_id: board.to_string(),
        key: "stage".to_string(),
        label: "Stage".to_string(),
        kind: FieldKind::Text,
        options: Vec::new(),
        columns: Vec::new(),
        default: None,
        required: false,
        owner: FieldOwner::User,
        position: 0,
        created_at: String::new(),
    };
    match db.create_field(field).expect("field") {
        FieldCreateOutcome::Created(field) => field.id,
        FieldCreateOutcome::KeyExists => panic!("field key exists"),
    }
}

/// Seeds one of everything for `account`, on two boards, beside its `token` and `passkey`.
pub(super) fn seed(
    db: &Db,
    account: &AccountId,
    name: &str,
    token: String,
    passkey: String,
) -> Ids {
    let board = db.create_board(account, name).expect("board");
    let board2 = db
        .create_board(account, &format!("{name} 2"))
        .expect("board");
    let column = db.create_column(&board, "Todo", None).expect("column");
    db.create_column(&board2, "Todo", None).expect("column");
    let lane = db.create_swimlane(&board, "Lane").expect("lane");
    db.create_swimlane(&board, "Other lane").expect("lane");
    let task = db
        .create_task(&column, Some(&lane), "Task", 1, None)
        .expect("task");
    let task2 = db
        .create_task(&column, Some(&lane), "Task 2", 1, None)
        .expect("task");
    let subtask = db
        .add_subtask(&task, "Sub")
        .expect("subtask")
        .expect("task")
        .id;
    let owned = db.account(account);
    let template = db
        .save_board_as_template(account, &board, &format!("{name} template"), "")
        .expect("template")
        .expect("board");
    Ids {
        account: account.as_str().to_string(),
        link: seed_link(&owned, &task, &task2),
        comment: seed_comment(&owned, &task, name),
        attachment: seed_attachment(&owned, &task),
        color: db.list_colors(&board).expect("colors")[1].id.clone(),
        field: seed_field(db, &board),
        entry: db.create_entry(&task, 5, "secret note").expect("entry"),
        board,
        board2,
        column,
        lane,
        task,
        task2,
        subtask,
        template,
        token,
        passkey,
    }
}

/// A closed database file holding the two accounts, which each test copies.
struct Seeded {
    /// Keeps the seeded file's directory alive.
    _dir: tempfile::TempDir,
    /// The seeded file.
    path: PathBuf,
    /// The mode the instance runs in.
    mode: InstanceMode,
    /// Account A, the victim.
    a: AccountId,
    /// Account B, the attacker.
    b: AccountId,
    /// A's handle, board name, token name and passkey name, which B must never see.
    a_secrets: [&'static str; 4],
    /// A's admin-scope API token.
    a_token: String,
    /// B's admin-scope API token.
    b_token: String,
    /// A's session cookie.
    a_cookie: String,
    /// B's session cookie.
    b_cookie: String,
    /// The rows A owns.
    a_ids: Ids,
    /// The rows B owns.
    b_ids: Ids,
    /// The session cookie of a third account that requested to join and waits for approval, in
    /// multi mode only.
    pending_cookie: Option<String>,
}

/// Mints an admin-scope API token named `name` for `account` and returns its id and raw value.
fn seed_token(db: &Db, account: &AccountId, name: &str) -> (String, String) {
    let (record, raw) = new_api_token_record(account, name, AuthScope::Admin, None);
    db.create_api_token(&record).expect("token");
    (record.id, raw)
}

/// Registers a passkey named `name` through the browser session `cookie` and returns its id.
async fn seed_passkey(
    app: &axum::Router,
    db: &Db,
    account: &AccountId,
    cookie: &str,
    key: u8,
    name: &str,
) -> String {
    let mut authenticator = SoftAuthenticator::new(key);
    let added = add_passkey(app, &mut authenticator, &[("cookie", cookie)], name).await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    db.passkeys_of(account)
        .expect("passkeys")
        .into_iter()
        .find(|passkey| passkey.name == name)
        .expect("seeded passkey")
        .id
}

/// Seeds the two accounts, A first so it is the instance's admin.
async fn seeded() -> Seeded {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("seed.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let a = db.create_user("alice").expect("alice");
    let b = db.create_user("bob").expect("bob");
    let cookie =
        |account: &AccountId| format!("session={}", db.create_session(account).expect("session"));
    let (a_cookie, b_cookie) = (cookie(&a), cookie(&b));
    let (a_token_id, a_token) = seed_token(&db, &a, "Alice agent");
    let (b_token_id, b_token) = seed_token(&db, &b, "Bob agent");
    let app = router(AppState::new(db.clone(), None, passkey_mode()));
    let a_passkey = seed_passkey(&app, &db, &a, &a_cookie, 7, "Alice laptop").await;
    let b_passkey = seed_passkey(&app, &db, &b, &b_cookie, 8, "Bob laptop").await;
    drop(app);
    let a_ids = seed(&db, &a, "Alice", a_token_id, a_passkey);
    let b_ids = seed(&db, &b, "Bob", b_token_id, b_passkey);
    drop(db);
    Seeded {
        _dir: dir,
        path,
        mode: InstanceMode::Single,
        a,
        b,
        a_token,
        b_token,
        a_cookie,
        b_cookie,
        a_secrets: ["alice", "Alice", "Alice agent", "Alice laptop"],
        a_ids,
        b_ids,
        pending_cookie: None,
    }
}

/// Runs a passkey ceremony pair, the options request then the finish with `authenticator`'s new
/// credential, and returns the finish reply.
pub(super) async fn enroll(
    app: &axum::Router,
    authenticator: &mut SoftAuthenticator,
    route: &str,
    start: Value,
) -> Reply {
    enroll_named(app, authenticator, route, start, "Passkey").await
}

/// Runs [`enroll`] for a passkey labelled `name`.
async fn enroll_named(
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
pub(super) async fn mint_token(app: &axum::Router, cookie: &str) -> String {
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

/// Seeds a multi-mode instance through its real paths: the admin through setup, and a member
/// through an invite link and a software passkey. A is the member and B, the attacker, is the
/// admin.
async fn registered() -> Seeded {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("registered.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    db.apply_mode(InstanceMode::Multi).expect("multi mode");
    let sign_in = passkey_mode();
    let code = sign_in
        .relying_party()
        .expect("relying party")
        .issue_setup_code();
    let state = AppState::new(db.clone(), None, sign_in)
        .with_mode(InstanceMode::Multi)
        .with_limits(unthrottled_limits());
    let app = router(state);

    let setup = enroll(
        &app,
        &mut SoftAuthenticator::new(11),
        "/api/v1/auth/setup",
        json!({"username": "admin", "code": code}),
    )
    .await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    let b_cookie = session_of(&setup);
    let invite = call(
        &app,
        "POST",
        "/api/v1/admin/invites",
        &[("cookie", &b_cookie)],
        Some(json!({})),
    )
    .await;
    assert_eq!(invite.status, StatusCode::OK, "{}", invite.body);
    let url = invite.body["url"].as_str().expect("invite url");
    let token = url.rsplit('/').next().expect("invite token");
    let joined = enroll_named(
        &app,
        &mut SoftAuthenticator::new(12),
        "/api/v1/auth/register",
        json!({"invite": token, "handle": "Zephyr"}),
        "Zephyr laptop",
    )
    .await;
    assert_eq!(joined.status, StatusCode::OK, "{}", joined.body);
    let a_cookie = session_of(&joined);
    let opened = call(
        &app,
        "PUT",
        "/api/v1/admin/settings",
        &[("cookie", &b_cookie)],
        Some(admin_settings("request", 10)),
    )
    .await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    let requested = enroll(
        &app,
        &mut SoftAuthenticator::new(13),
        "/api/v1/auth/register",
        json!({"handle": "Waiting"}),
    )
    .await;
    assert_eq!(requested.status, StatusCode::OK, "{}", requested.body);
    let pending_cookie = Some(session_of(&requested));

    let a = db
        .account_by_handle("zephyr")
        .expect("read")
        .expect("member")
        .account_id();
    let b = db.owner().expect("read").expect("admin").account_id();
    let passkey_of = |account: &AccountId| {
        db.passkeys_of(account)
            .expect("passkeys")
            .into_iter()
            .next()
            .expect("registered passkey")
            .id
    };
    let (a_token_id, a_token) = seed_token(&db, &a, "Zephyr agent");
    let (b_token_id, b_token) = seed_token(&db, &b, "Admin agent");
    let seeded = Seeded {
        a_ids: seed(&db, &a, "Zephyr", a_token_id, passkey_of(&a)),
        b_ids: seed(&db, &b, "Admin", b_token_id, passkey_of(&b)),
        a_token,
        b_token,
        a_cookie,
        b_cookie,
        _dir: dir,
        path,
        mode: InstanceMode::Multi,
        a,
        b,
        a_secrets: ["zephyr", "Zephyr", "Zephyr agent", "Zephyr laptop"],
        pending_cookie,
    };
    drop(app);
    drop(db);
    seeded
}

/// Opens a fresh copy of the seeded file behind a passkey-mode app.
fn open(seeded: &Seeded) -> Accounts {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("access.redb");
    std::fs::copy(&seeded.path, &path).expect("copy the seeded file");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let state = AppState::new(db.clone(), None, passkey_mode())
        .with_mode(seeded.mode)
        .with_limits(unthrottled_limits());
    Accounts {
        _dir: dir,
        path,
        db,
        app: router(state),
        a: seeded.a.clone(),
        b: seeded.b.clone(),
        a_token: seeded.a_token.clone(),
        b_token: seeded.b_token.clone(),
        a_cookie: seeded.a_cookie.clone(),
        b_cookie: seeded.b_cookie.clone(),
        a_ids: seeded.a_ids.clone(),
        b_ids: seeded.b_ids.clone(),
        pending_cookie: seeded.pending_cookie.clone(),
    }
}

/// Builds the two accounts on a fresh database.
async fn accounts() -> Accounts {
    open(&seeded().await)
}

/// Sends one request and returns its status and body.
async fn send(
    app: &axum::Router,
    who: Who<'_>,
    method: &Method,
    uri: &str,
    payload: Option<&Payload>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method.clone())
        .uri(uri)
        .header("host", "pomb.test")
        .header("accept", "application/json");
    builder = match who {
        Who::Anonymous => builder,
        Who::Token(token) => builder.header("authorization", format!("Bearer {token}")),
        Who::Session(cookie) => builder
            .header("cookie", cookie)
            .header("sec-fetch-site", "same-origin"),
    };
    let body = match payload {
        Some(Payload::Json(value)) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        Some(Payload::Form(form)) => {
            builder = builder.header("content-type", "application/x-www-form-urlencoded");
            Body::from(form.clone())
        }
        Some(Payload::Raw(bytes)) => {
            builder = builder.header("content-type", "application/octet-stream");
            Body::from(bytes.clone())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Returns the id a path parameter names on a route of `class`: the route's main id from `main`,
/// and a row inside it from `sub`.
fn param<'a>(class: RouteClass, path: &str, name: &str, main: &'a Ids, sub: &'a Ids) -> &'a str {
    match name {
        "board_id" => &main.board,
        "sub_id" => &sub.subtask,
        "link_id" => &sub.link,
        "comment_id" => &sub.comment,
        "attachment_id" => &sub.attachment,
        "color_id" => &sub.color,
        "index" => "0",
        "id" => match class {
            RouteClass::Board => &main.board,
            RouteClass::Task => &main.task,
            RouteClass::Entity if path.starts_with("/api/columns/") => &main.column,
            RouteClass::Entity if path.starts_with("/api/swimlanes/") => &main.lane,
            RouteClass::Entity if path.starts_with("/api/fields/") => &main.field,
            RouteClass::Entity if path.starts_with("/api/time/entries/") => &main.entry,
            RouteClass::Entity if path.starts_with("/api/templates/") => &main.template,
            _ => panic!("{path}: no id for :id on a {class:?} route"),
        },
        _ => panic!("{path}: no id for :{name}"),
    }
}

/// Fills a route pattern's main id from `main` and the ids of rows inside it from `sub`.
fn uri(class: RouteClass, path: &str, main: &Ids, sub: &Ids) -> String {
    path.split('/')
        .map(|segment| match segment.strip_prefix(':') {
            Some(name) => param(class, path, name, main, sub).to_string(),
            None => segment.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Returns a body that passes validation, naming the ids in `body_ids` wherever the body holds
/// one, so a request reaches the row its path names. A write route with none listed sends `{}`.
fn payload(method: &Method, path: &str, body_ids: &Ids) -> Option<Payload> {
    let json = match (method, path) {
        (&Method::POST, "/b/:board_id/settings") => {
            return Some(Payload::Form("name=Renamed".to_string()));
        }
        (&Method::PUT, "/api/boards/:id/config") => json!({"legend_visible": true}),
        (&Method::POST, "/api/boards/:id/fields") => {
            json!({"key": "new", "label": "New", "kind": "text"})
        }
        (&Method::POST, "/api/boards/:id/save-as-template") => json!({"name": "Saved"}),
        (&Method::POST, "/api/boards/:id/colors") => json!({"value": "yellow"}),
        (&Method::PATCH, "/api/boards/:id/colors/:color_id") => json!({"label": "Renamed"}),
        (&Method::POST, "/api/boards/:id/colors/copy-from") => {
            json!({"source_board_id": body_ids.board2})
        }
        (&Method::PATCH, "/api/tasks/:id") => json!({"name": "Renamed"}),
        (&Method::POST, "/api/tasks/:id/move") => {
            json!({"column_id": body_ids.column, "position": 0})
        }
        (&Method::POST, "/api/tasks/:id/watch") => json!({"watched": true}),
        (&Method::POST, "/api/tasks/:id/checklist/:index") => {
            json!({"checked": true, "description_hash": "x"})
        }
        (&Method::POST, "/api/tasks/:id/time") => json!({"minutes": 5}),
        (&Method::POST, "/api/tasks/:id/subtasks") => json!({"name": "Sub"}),
        (&Method::PUT, "/api/tasks/:id/subtasks/order") => json!({"order": []}),
        (&Method::PATCH, "/api/tasks/:id/subtasks/:sub_id") => json!({"done": true}),
        (&Method::POST, "/api/tasks/:id/links") => {
            json!({"kind": "blocks", "target_id": body_ids.task2})
        }
        (&Method::POST, "/api/tasks/:id/claim") => {
            json!({"claimant": "agent", "ttl_seconds": 60})
        }
        (&Method::POST, "/api/tasks/:id/claim/heartbeat" | "/api/tasks/:id/claim/release") => {
            json!({"fence": 1})
        }
        (&Method::POST, "/api/tasks/:id/comments") => json!({"body": "hello"}),
        (&Method::PATCH, "/api/tasks/:id/comments/:comment_id") => json!({"body": "edited"}),
        (&Method::PATCH, "/api/tasks/:id/attachments/:attachment_id") => json!({"name": "b"}),
        (&Method::POST, "/api/tasks/:id/attachments") => {
            json!({"name": "b.txt", "data": "aGk="})
        }
        (&Method::PUT, "/api/time/entries/:id") => json!({
            "task_id": body_ids.task,
            "date": "2020-01-02",
            "from": "10:00",
            "to": "10:30",
        }),
        (&Method::PATCH, "/api/columns/:id" | "/api/swimlanes/:id") => json!({"name": "Renamed"}),
        (&Method::POST, "/api/columns/:id/move" | "/api/swimlanes/:id/move") => {
            json!({"position": 0})
        }
        (&Method::PATCH, "/api/fields/:id") => json!({"label": "Renamed"}),
        (&Method::GET | &Method::DELETE, _) => return None,
        _ => json!({}),
    };
    Some(Payload::Json(json))
}

/// Returns ids no account holds.
fn missing() -> Ids {
    let fresh = || uuid::Uuid::new_v4().to_string();
    Ids {
        account: fresh(),
        board: fresh(),
        board2: fresh(),
        column: fresh(),
        lane: fresh(),
        task: fresh(),
        task2: fresh(),
        subtask: fresh(),
        link: fresh(),
        comment: fresh(),
        attachment: fresh(),
        color: fresh(),
        field: fresh(),
        entry: fresh(),
        template: fresh(),
        token: fresh(),
        passkey: fresh(),
    }
}

/// Makes the answer to a missing id comparable with the answer to another account's: the ids in
/// it are blanked.
fn without_ids(text: &str, ids: &[&Ids]) -> String {
    let mut text = text.to_string();
    for id in ids.iter().flat_map(|ids| ids.all()) {
        text = text.replace(id, "<id>");
    }
    text
}

/// Every Board, Task and Entity route answers B's token and B's session on A's ids with the 404 a
/// missing id gets, leaves the database file byte-identical, and still serves A.
#[tokio::test]
async fn cross_account_matrix_should_answer_not_found_for_every_owned_route() {
    attack_every_owned_route(&seeded().await).await;
}

/// The matrix holds for accounts made through multi mode's real setup and invite registration,
/// with the admin attacking a member: the admin gets none of the member's board content.
#[tokio::test]
async fn cross_account_matrix_should_hold_for_registered_accounts_with_the_admin_attacking() {
    let seeded = registered().await;
    attack_every_owned_route(&seeded).await;
    lists_should_hold_only_own_rows_of(&open(&seeded), &seeded).await;
}

/// Attacks every Board, Task and Entity route of `seeded`'s A as its B.
async fn attack_every_owned_route(seeded: &Seeded) {
    let mut covered = 0;
    let mut assertions = 0;
    for (method, path, class, _) in ROUTE_ACCESS {
        if !matches!(
            class,
            RouteClass::Board | RouteClass::Task | RouteClass::Entity
        ) {
            continue;
        }
        let fx = open(seeded);
        let foreign = uri(*class, path, &fx.a_ids, &fx.a_ids);
        let absent_ids = missing();
        let absent = uri(*class, path, &absent_ids, &absent_ids);
        let attack = payload(method, path, &fx.b_ids);

        let (status, _) = send(
            &fx.app,
            Who::Token(&fx.b_token),
            &Method::GET,
            "/api/boards",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let before = std::fs::read(&fx.path).expect("read db file");
        for who in [Who::Token(&fx.b_token), Who::Session(&fx.b_cookie)] {
            let (status, text) = send(&fx.app, who, method, &foreign, attack.as_ref()).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{method} {path} as B ({who:?}) on A's ids: {text}"
            );
            let (absent_status, absent_text) =
                send(&fx.app, who, method, &absent, attack.as_ref()).await;
            assert_eq!(
                (status, without_ids(&text, &[&fx.a_ids])),
                (absent_status, without_ids(&absent_text, &[&absent_ids])),
                "{method} {path} as B ({who:?}): another account's id must look missing"
            );
            if let Who::Token(_) = who {
                let error: Value = serde_json::from_str(&text).expect("json error");
                assert_eq!(error["code"], "not_found", "{method} {path}: {text}");
                assertions += 1;
            }
            assertions += 2;
        }
        if let Some(cookie) = &fx.pending_cookie {
            assert_pending_refused(&fx.app, cookie, method, path, &foreign).await;
            assertions += 1;
        }
        assert!(
            std::fs::read(&fx.path).expect("read db file") == before,
            "{method} {path}: B's and the pending account's requests changed the database"
        );
        assertions += 1;

        let own = payload(method, path, &fx.a_ids);
        let (status, text) = send(
            &fx.app,
            Who::Token(&fx.a_token),
            method,
            &foreign,
            own.as_ref(),
        )
        .await;
        assert!(
            status != StatusCode::NOT_FOUND && !status.is_server_error(),
            "{method} {path} as A: {status} {text}"
        );
        assertions += 1;
        covered += 1;
    }
    assert!(
        covered > MIN_MATRIX_ROUTES,
        "the matrix covered only {covered} routes"
    );
    println!("cross-account matrix: {covered} routes, {assertions} assertions");
}

/// Asserts that an account waiting for approval gets nowhere with `method` on `uri`, an instance
/// of the route `path`: an API route answers 403 `account_pending`, and a page sends it to
/// `/pending`.
async fn assert_pending_refused(
    app: &axum::Router,
    cookie: &str,
    method: &Method,
    path: &str,
    uri: &str,
) {
    let reply = call(app, method.as_str(), uri, &[("cookie", cookie)], None).await;
    if path.starts_with("/api/") {
        assert_eq!(
            (reply.status, reply.code.as_deref()),
            (StatusCode::FORBIDDEN, Some("account_pending")),
            "{method} {path} as the pending account: {}",
            reply.body
        );
    } else {
        assert_eq!(
            (reply.status, reply.location.as_deref()),
            (StatusCode::SEE_OTHER, Some("/pending")),
            "{method} {path} as the pending account: {}",
            reply.body
        );
    }
}

/// Every route behind the auth layer but the pending page and sign-out refuses an account that
/// waits for approval, and leaves the database file byte-identical.
#[tokio::test]
async fn pending_account_should_reach_no_route_but_the_pending_page() {
    let seeded = registered().await;
    let fx = open(&seeded);
    let cookie = fx.pending_cookie.as_deref().expect("a pending account");
    let before = std::fs::read(&fx.path).expect("read db file");
    let mut covered = 0;
    for (method, path, class, _) in ROUTE_ACCESS {
        if *class == RouteClass::Public || PENDING_ROUTES.contains(path) {
            continue;
        }
        let target = match class {
            RouteClass::Board | RouteClass::Task | RouteClass::Entity => {
                uri(*class, path, &fx.a_ids, &fx.a_ids)
            }
            _ => path.replace(":id", &fx.a_ids.account),
        };
        assert_pending_refused(&fx.app, cookie, method, path, &target).await;
        covered += 1;
    }
    assert!(
        std::fs::read(&fx.path).expect("read db file") == before,
        "the pending account's requests changed the database"
    );
    assert!(covered > MIN_MATRIX_ROUTES, "covered only {covered} routes");
    let page = call(&fx.app, "GET", "/pending", &[("cookie", cookie)], None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
}

/// Returns the stored rows of the board in `ids`: its tasks with their subtasks, comments and
/// attachments, its palette, the links of its task and the bytes of its attachment.
fn board_rows(db: &Db, ids: &Ids) -> String {
    json!({
        "tasks": db.board_tasks(&ids.board).expect("tasks"),
        "colors": db.list_colors(&ids.board).expect("colors"),
        "links": db.list_task_links(&ids.task).expect("links"),
        "attachment": db.read_attachment_data(&ids.attachment).expect("attachment"),
    })
    .to_string()
}

/// A route whose path names B's own board or task and a row of A's inside it answers exactly as
/// it answers a missing row, and changes nothing.
#[tokio::test]
async fn sub_entity_ids_should_not_cross_accounts() {
    let seeded = seeded().await;
    let mut covered = 0;
    for (method, path, class, _) in ROUTE_ACCESS {
        if !SUB_ID_PARAMS
            .iter()
            .any(|name| path.split('/').any(|segment| segment == format!(":{name}")))
        {
            continue;
        }
        let fx = open(&seeded);
        let foreign = uri(*class, path, &fx.b_ids, &fx.a_ids);
        let absent_ids = missing();
        let absent = uri(*class, path, &fx.b_ids, &absent_ids);
        let attack = payload(method, path, &fx.b_ids);
        let before = board_rows(&fx.db, &fx.a_ids);
        for who in [Who::Token(&fx.b_token), Who::Session(&fx.b_cookie)] {
            let (status, text) = send(&fx.app, who, method, &foreign, attack.as_ref()).await;
            let (absent_status, absent_text) =
                send(&fx.app, who, method, &absent, attack.as_ref()).await;
            assert!(
                status.is_client_error(),
                "{method} {path} as B ({who:?}) with A's row: {status} {text}"
            );
            assert_eq!(
                (status, without_ids(&text, &[&fx.a_ids])),
                (absent_status, without_ids(&absent_text, &[&absent_ids])),
                "{method} {path} as B ({who:?}): A's row inside B's must look missing"
            );
        }
        assert_eq!(
            board_rows(&fx.db, &fx.a_ids),
            before,
            "{method} {path}: B's requests changed A's rows"
        );
        covered += 1;
    }
    assert!(
        covered >= SUB_ID_PARAMS.len(),
        "covered only {covered} routes"
    );
}

/// Method-router builders the route parser reads.
const PARSED_METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Calls that add routes in ways the route parser does not read.
const OPAQUE_ROUTER_CALLS: &[&str] = &[
    "merge",
    "nest",
    "nest_service",
    "route_service",
    "any",
    "on",
    "any_service",
    "on_service",
    "get_service",
    "post_service",
    "put_service",
    "patch_service",
    "delete_service",
];

/// Calls that build a router or add a route at all.
const ROUTER_CALLS: &[&str] = &["route", "Router::new", "merge", "nest", "nest_service"];

/// The one opaque call the router may make: the debug build's static files, which no handler
/// serves.
const DEBUG_STATIC_FILES: &str = r#"router.nest_service("/static", ServeDir::new("static"))"#;

/// Returns how many times `text` calls `name`, a call being `name(` after a non-identifier
/// character.
fn calls(text: &str, name: &str) -> usize {
    text.match_indices(&format!("{name}("))
        .filter(|(at, _)| {
            text[..*at]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
        })
        .count()
}

/// Returns `text` without its inline `#[cfg(test)]` modules, each cut from its opening brace to
/// the brace that closes it.
fn without_test_modules(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("#[cfg(test)]\nmod ") {
        let header = &rest[start..];
        let line_end = header.find('\n').map_or(header.len(), |at| at + 1);
        let declaration = &header[line_end
            ..header[line_end..]
                .find('\n')
                .map_or(header.len(), |at| line_end + at)];
        out.push_str(&rest[..start]);
        if !declaration.ends_with('{') {
            out.push_str(&header[..line_end + declaration.len()]);
            rest = &header[line_end + declaration.len()..];
            continue;
        }
        let mut depth = 0usize;
        let mut end = header.len();
        for (at, c) in header.char_indices().skip(line_end + declaration.len() - 1) {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = at + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &header[end..];
    }
    out.push_str(rest);
    out
}

/// Returns the production source of every `.rs` file under `dir`, keyed by path: a file a
/// `#[cfg(test)] mod` line names is left out, and every other file loses its inline test modules.
fn production_sources(dir: &std::path::Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).expect("read src dir") {
            let entry_path = entry.expect("dir entry").path();
            if entry_path.is_dir() {
                pending.push(entry_path);
            } else if entry_path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&entry_path).expect("read source");
                files.push((entry_path, text));
            }
        }
    }
    let test_modules: Vec<String> = files
        .iter()
        .flat_map(|(_, text)| text.split("#[cfg(test)]\nmod ").skip(1))
        .filter_map(|rest| rest.split_once(";\n").map(|(name, _)| format!("{name}.rs")))
        .filter(|name| !name.contains(char::is_whitespace))
        .collect();
    files
        .into_iter()
        .filter(|(file, _)| {
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            !test_modules.iter().any(|module| module == name)
        })
        .map(|(file, text)| {
            let production = without_test_modules(&text);
            (file, production)
        })
        .collect()
}

/// Every route is registered by a `.route(` call inside `router()` with a method router the
/// parser reads, so none can hide from [`registered_routes`] and the coverage test.
#[test]
fn routes_should_be_registered_only_where_the_coverage_parser_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = production_sources(&src);
    let routes_rs = src.join("routes.rs");
    let mut router_body = None;
    for (file, text) in &sources {
        let (outside, inside) = if *file == routes_rs {
            let start = text.find("pub fn router(").expect("router fn");
            let end = start + text[start..].find("\n}\n").expect("router fn end");
            router_body = Some(&text[start..end]);
            (
                format!("{}{}", &text[..start], &text[end..]),
                &text[start..end],
            )
        } else {
            (text.clone(), "")
        };
        for name in ROUTER_CALLS {
            assert_eq!(
                calls(&outside, name),
                0,
                "{}: `{name}(` outside router() builds routes the coverage test cannot see",
                file.display()
            );
        }
        let inside = inside.replacen(DEBUG_STATIC_FILES, "", 1);
        for name in OPAQUE_ROUTER_CALLS {
            assert_eq!(
                calls(&inside, name),
                0,
                "{}: router() calls `{name}(`, which the coverage parser does not read",
                file.display()
            );
        }
    }
    let body = router_body.expect("router() in src/routes.rs");
    assert_eq!(
        body.matches(DEBUG_STATIC_FILES).count(),
        1,
        "router() serves the debug static files once"
    );
    let fallback = body.find(".fallback(").expect("fallback");
    assert_eq!(
        calls(&body[fallback..], "route"),
        0,
        "a route after the fallback is outside the coverage parser"
    );
    for call in body[..fallback].split(".route(").skip(1) {
        let path = call.split('"').nth(1).expect("route path");
        assert!(
            PARSED_METHODS.iter().any(|method| calls(call, method) > 0),
            "{path}: no method router the coverage parser reads"
        );
    }
}

/// Returns every (method, path) the router registers, read from its source, the public routes
/// included.
fn registered_routes() -> Vec<(String, String)> {
    let source = include_str!("../routes.rs");
    let start = source.find("pub fn router(").expect("router fn");
    let end = start + source[start..].find(".fallback(").expect("fallback");
    let mut found = Vec::new();
    for call in source[start..end].split(".route(").skip(1) {
        let path = call.split('"').nth(1).expect("route path");
        let handlers = &call[call.find('"').expect("quote") + path.len() + 2..];
        for method in PARSED_METHODS {
            let needle = format!("{method}(");
            let listed = handlers.match_indices(&needle).any(|(at, _)| {
                handlers[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
            });
            if listed {
                found.push((method.to_uppercase(), path.to_string()));
            }
        }
    }
    found
}

#[test]
fn route_access_should_cover_every_route() {
    let routes = registered_routes();
    assert!(
        routes.len() > MIN_REGISTERED_ROUTES,
        "parsed only {} routes",
        routes.len()
    );
    for (method, path) in &routes {
        let listed = ROUTE_ACCESS
            .iter()
            .filter(|(m, pattern, _, _)| m.as_str() == method && pattern == path)
            .count();
        assert_eq!(
            listed, 1,
            "{method} {path} needs exactly one ROUTE_ACCESS entry"
        );
    }
    for (method, pattern, _, _) in ROUTE_ACCESS {
        assert!(
            routes
                .iter()
                .any(|(m, p)| m.as_str() == method.as_str() && p == pattern),
            "ROUTE_ACCESS lists {method} {pattern}, which the router does not register"
        );
    }
    let count = |wanted: RouteClass| {
        ROUTE_ACCESS
            .iter()
            .filter(|(_, _, class, _)| *class == wanted)
            .count()
    };
    println!(
        "ROUTE_ACCESS: Board {}, Task {}, Entity {}, Account {}, AccountSession {}, Admin {}, \
         Public {}",
        count(RouteClass::Board),
        count(RouteClass::Task),
        count(RouteClass::Entity),
        count(RouteClass::Account),
        count(RouteClass::AccountSession),
        count(RouteClass::Admin),
        count(RouteClass::Public),
    );
}

/// A request body built from one account's ids.
type BodyOf = Box<dyn Fn(&Ids) -> Value>;

/// Returns the requests whose body or query names an id, each with the body built from the ids
/// it names. Each request's path names only B's rows or none.
fn body_id_cases(b: &Ids) -> Vec<(Method, String, BodyOf)> {
    let b_task_uri = |rest: &str| format!("/api/tasks/{}{rest}", b.task);
    let b_column = b.column.clone();
    let b_move_column = b.column.clone();
    vec![
        (
            Method::POST,
            "/api/tasks".to_string(),
            Box::new(|ids: &Ids| json!({"column_id": ids.column, "name": "Sneak"})),
        ),
        (
            Method::POST,
            "/api/tasks".to_string(),
            Box::new(
                move |ids: &Ids| json!({"column_id": b_column, "swimlane_id": ids.lane, "name": "Sneak"}),
            ),
        ),
        (
            Method::POST,
            "/api/timer/start".to_string(),
            Box::new(|ids: &Ids| json!({"task_id": ids.task, "mode": "pomodoro"})),
        ),
        (
            Method::POST,
            "/api/timer/retarget".to_string(),
            Box::new(|ids: &Ids| json!({"task_id": ids.task})),
        ),
        (
            Method::POST,
            "/api/time/manual".to_string(),
            Box::new(
                |ids: &Ids| json!({"task_id": ids.task, "date": "2020-01-02", "from": "10:00", "to": "10:30"}),
            ),
        ),
        (
            Method::POST,
            "/api/columns".to_string(),
            Box::new(|ids: &Ids| json!({"board_id": ids.board, "name": "Sneak"})),
        ),
        (
            Method::POST,
            "/api/swimlanes".to_string(),
            Box::new(|ids: &Ids| json!({"board_id": ids.board, "name": "Sneak"})),
        ),
        (
            Method::POST,
            "/api/boards".to_string(),
            Box::new(|ids: &Ids| json!({"name": "Copy", "template_id": ids.template})),
        ),
        (
            Method::POST,
            b_task_uri("/links"),
            Box::new(|ids: &Ids| json!({"kind": "blocked_by", "target_id": ids.task})),
        ),
        (
            Method::POST,
            b_task_uri("/move"),
            Box::new(|ids: &Ids| json!({"column_id": ids.column, "position": 0})),
        ),
        (
            Method::POST,
            b_task_uri("/move"),
            Box::new(
                move |ids: &Ids| json!({"column_id": b_move_column, "swimlane_id": ids.lane, "position": 0}),
            ),
        ),
        (
            Method::PATCH,
            b_task_uri(""),
            Box::new(
                |ids: &Ids| json!({"due_at": "2030-01-01", "due_target_column_id": ids.column}),
            ),
        ),
        (
            Method::PATCH,
            b_task_uri(""),
            Box::new(|ids: &Ids| json!({"member_ids": [ids.account]})),
        ),
        (
            Method::POST,
            format!("/api/boards/{}/colors/copy-from", b.board),
            Box::new(|ids: &Ids| json!({"source_board_id": ids.board})),
        ),
        (
            Method::PUT,
            format!("/api/time/entries/{}", b.entry),
            Box::new(
                |ids: &Ids| json!({"task_id": ids.task, "date": "2020-01-02", "from": "10:00", "to": "10:30"}),
            ),
        ),
    ]
}

/// A column or swimlane create on a board of another account answers the 404 a missing board
/// answers, whatever the body holds.
#[tokio::test]
async fn board_create_should_look_missing_before_the_body_is_checked() {
    let fx = accounts().await;
    let a = &fx.a_ids;
    let absent = missing();
    for uri in ["/api/columns", "/api/swimlanes"] {
        for name in ["Sneak", "   "] {
            for who in [Who::Token(&fx.b_token), Who::Session(&fx.b_cookie)] {
                for board in [&a.board, &absent.board] {
                    let body = Payload::Json(json!({"board_id": board, "name": name}));
                    let (status, text) = send(&fx.app, who, &Method::POST, uri, Some(&body)).await;
                    assert_eq!(status, StatusCode::NOT_FOUND, "{uri} {name:?}: {text}");
                }
            }
        }
    }
}

type UriOf = Box<dyn Fn(&Ids) -> String>;

fn blank_body(key: &'static str) -> BodyOf {
    Box::new(move |_: &Ids| {
        let mut map = serde_json::Map::new();
        map.insert(key.to_string(), json!("   "));
        Value::Object(map)
    })
}

fn task_uri(rest: &'static str) -> UriOf {
    Box::new(move |ids: &Ids| format!("/api/tasks/{}{rest}", ids.task))
}

fn bad_body_cases() -> Vec<(Method, UriOf, BodyOf)> {
    vec![
        (
            Method::POST,
            Box::new(|_: &Ids| "/api/tasks".to_string()),
            Box::new(|ids: &Ids| json!({"column_id": ids.column, "name": "   "})),
        ),
        (Method::PATCH, task_uri(""), blank_body("name")),
        (
            Method::POST,
            task_uri("/links"),
            Box::new(|ids: &Ids| json!({"kind": "nope", "target_id": ids.task2})),
        ),
        (
            Method::POST,
            task_uri("/claim"),
            Box::new(|_: &Ids| json!({})),
        ),
        (
            Method::POST,
            task_uri("/claim/heartbeat"),
            Box::new(|_: &Ids| json!({})),
        ),
        (
            Method::POST,
            task_uri("/claim/release"),
            Box::new(|_: &Ids| json!({})),
        ),
        (Method::POST, task_uri("/subtasks"), blank_body("name")),
        (
            Method::PATCH,
            Box::new(|ids: &Ids| format!("/api/tasks/{}/subtasks/{}", ids.task, ids.subtask)),
            blank_body("name"),
        ),
        (
            Method::POST,
            task_uri("/checklist/0"),
            Box::new(|_: &Ids| json!({"checked": true, "description_hash": "   "})),
        ),
        (Method::POST, task_uri("/comments"), blank_body("body")),
        (
            Method::PATCH,
            Box::new(|ids: &Ids| format!("/api/tasks/{}/comments/{}", ids.task, ids.comment)),
            blank_body("body"),
        ),
        (
            Method::POST,
            task_uri("/attachments"),
            Box::new(|_: &Ids| json!({"name": "   ", "data": "aGk="})),
        ),
        (
            Method::PATCH,
            Box::new(|ids: &Ids| format!("/api/tasks/{}/attachments/{}", ids.task, ids.attachment)),
            blank_body("name"),
        ),
        (
            Method::POST,
            task_uri("/time"),
            Box::new(|_: &Ids| json!({"minutes": 0})),
        ),
        (
            Method::POST,
            Box::new(|_: &Ids| "/api/time/manual".to_string()),
            Box::new(
                |ids: &Ids| json!({"task_id": ids.task, "date": "nope", "from": "10:00", "to": "10:30"}),
            ),
        ),
        (
            Method::PUT,
            Box::new(|ids: &Ids| format!("/api/time/entries/{}", ids.entry)),
            Box::new(
                |ids: &Ids| json!({"task_id": ids.task, "date": "nope", "from": "10:00", "to": "10:30"}),
            ),
        ),
        (
            Method::PATCH,
            Box::new(|ids: &Ids| format!("/api/fields/{}", ids.field)),
            blank_body("label"),
        ),
        (
            Method::POST,
            Box::new(|ids: &Ids| format!("/api/boards/{}/save-as-template", ids.board)),
            blank_body("name"),
        ),
        (
            Method::PATCH,
            Box::new(|ids: &Ids| format!("/api/boards/{}/colors/{}", ids.board, ids.color)),
            blank_body("label"),
        ),
    ]
}

#[tokio::test]
async fn task_create_should_look_missing_for_a_column_of_another_account() {
    let fx = accounts().await;
    let absent = missing();
    for who in [Who::Token(&fx.b_token), Who::Session(&fx.b_cookie)] {
        let foreign = Payload::Json(json!({"column_id": fx.a_ids.column, "name": "Sneak"}));
        let (status, text) = send(&fx.app, who, &Method::POST, "/api/tasks", Some(&foreign)).await;
        let unknown = Payload::Json(json!({"column_id": absent.column, "name": "Sneak"}));
        let (absent_status, absent_text) =
            send(&fx.app, who, &Method::POST, "/api/tasks", Some(&unknown)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{who:?}: {text}");
        assert_eq!(
            (status, without_ids(&text, &[&fx.a_ids])),
            (absent_status, without_ids(&absent_text, &[&absent])),
            "{who:?}: A's column must look missing"
        );
    }
}

#[tokio::test]
async fn raw_upload_should_look_missing_on_a_task_of_another_account() {
    let fx = accounts().await;
    let absent = missing();
    let upload = |ids: &Ids| format!("/api/tasks/{}/attachments?name=%20%20", ids.task);
    let raw = Payload::Raw(b"hi".to_vec());
    for who in [Who::Token(&fx.b_token), Who::Session(&fx.b_cookie)] {
        let (status, text) =
            send(&fx.app, who, &Method::POST, &upload(&fx.a_ids), Some(&raw)).await;
        let (absent_status, absent_text) =
            send(&fx.app, who, &Method::POST, &upload(&absent), Some(&raw)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{who:?}: {text}");
        assert_eq!(
            (status, without_ids(&text, &[&fx.a_ids])),
            (absent_status, without_ids(&absent_text, &[&absent])),
            "{who:?}: A's task must look missing"
        );
        let (status, text) =
            send(&fx.app, who, &Method::POST, &upload(&fx.b_ids), Some(&raw)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{who:?}: {text}");
    }
}

#[tokio::test]
async fn bad_bodies_should_look_missing_on_rows_of_another_account() {
    let fx = accounts().await;
    let absent = missing();
    for (method, uri, body) in &bad_body_cases() {
        let route = uri(&fx.a_ids);
        for who in [Who::Token(&fx.b_token), Who::Session(&fx.b_cookie)] {
            let foreign_body = Payload::Json(body(&fx.a_ids));
            let (status, text) = send(&fx.app, who, method, &route, Some(&foreign_body)).await;
            let absent_body = Payload::Json(body(&absent));
            let (absent_status, absent_text) =
                send(&fx.app, who, method, &uri(&absent), Some(&absent_body)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {route}: {text}");
            assert_eq!(
                (status, without_ids(&text, &[&fx.a_ids])),
                (absent_status, without_ids(&absent_text, &[&absent])),
                "{method} {route} ({who:?}): a bad body must not tell A's id from a missing one"
            );
            let own_route = uri(&fx.b_ids);
            let own_body = Payload::Json(body(&fx.b_ids));
            let (status, text) = send(&fx.app, who, method, &own_route, Some(&own_body)).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{method} {own_route} ({who:?}): {text}"
            );
        }
    }
}

/// Ids that arrive in a body or a query go through the same ownership check as path ids: B naming
/// A's row gets exactly the answer a missing id gets, and A's rows stay as they were.
#[tokio::test]
async fn body_ids_should_not_cross_accounts() {
    let fx = accounts().await;
    let a = fx.a_ids.clone();
    let b = fx.b_ids.clone();
    let absent = missing();
    let (status, _) = send(
        &fx.app,
        Who::Token(&fx.b_token),
        &Method::GET,
        "/api/boards",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let snapshot = |db: &Db| {
        let owned = db.account(&fx.a);
        let b_task = db.task(&b.task).expect("read").expect("task");
        (
            serde_json::to_string(&owned.board_tasks(&a.board).expect("tasks")).expect("json"),
            serde_json::to_string(&owned.all_entries().expect("entries")).expect("json"),
            serde_json::to_string(&owned.list_columns(&a.board).expect("columns")).expect("json"),
            serde_json::to_string(&owned.list_swimlanes(&a.board).expect("lanes")).expect("json"),
            serde_json::to_string(&owned.list_colors(&a.board).expect("colors")).expect("json"),
            owned.list_boards().expect("boards").len(),
            (b_task.swimlane_id, b_task.member_ids),
            db.board_tasks(&b.board).expect("tasks").len(),
        )
    };
    let before = snapshot(&fx.db);
    for (method, uri, body) in &body_id_cases(&b) {
        let foreign = Payload::Json(body(&a));
        let unknown = Payload::Json(body(&absent));
        for who in [Who::Token(&fx.b_token), Who::Session(&fx.b_cookie)] {
            let (status, text) = send(&fx.app, who, method, uri, Some(&foreign)).await;
            let (absent_status, absent_text) =
                send(&fx.app, who, method, uri, Some(&unknown)).await;
            assert!(
                status.is_client_error(),
                "{method} {uri} ({who:?}): {status} {text}"
            );
            assert_eq!(
                (status, without_ids(&text, &[&a])),
                (absent_status, without_ids(&absent_text, &[&absent])),
                "{method} {uri} ({who:?}): A's id must look missing"
            );
        }
    }
    assert_eq!(snapshot(&fx.db), before, "B's requests changed the rows");
    let own = Payload::Json(json!({"task_id": b.task, "mode": "pomodoro"}));
    let (status, text) = send(
        &fx.app,
        Who::Token(&fx.b_token),
        &Method::POST,
        "/api/timer/start",
        Some(&own),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "B still starts a timer on its own task: {text}"
    );
}

/// Lists show only the caller's own rows.
#[tokio::test]
async fn lists_should_hold_only_own_rows() {
    let seeded = seeded().await;
    lists_should_hold_only_own_rows_of(&open(&seeded), &seeded).await;
}

/// Checks that no list B reads holds a row of A's.
async fn lists_should_hold_only_own_rows_of(fx: &Accounts, seeded: &Seeded) {
    let secrets = [
        fx.a_ids.board.as_str(),
        fx.a_ids.board2.as_str(),
        fx.a_ids.task.as_str(),
        fx.a_ids.template.as_str(),
        fx.a_ids.entry.as_str(),
        fx.a_ids.token.as_str(),
        fx.a_ids.passkey.as_str(),
        fx.a.as_str(),
    ]
    .into_iter()
    .chain(seeded.a_secrets)
    .collect::<Vec<_>>();
    for uri in [
        "/api/boards?include_archived=true",
        "/api/tasks",
        "/api/templates",
        "/api/members",
        "/api/v1/auth/tokens",
        "/api/v1/auth/passkeys",
        "/api/timer/log",
        "/api/timer/today",
        "/api/timer/time-spent",
        "/api/settings",
        "/",
        "/boards/new",
        "/boards/archived",
        "/timer/statistics",
    ] {
        let (status, text) =
            send(&fx.app, Who::Session(&fx.b_cookie), &Method::GET, uri, None).await;
        assert!(
            status.is_success() || status.is_redirection(),
            "{uri}: {status} {text}"
        );
        for secret in &secrets {
            assert!(!text.contains(secret), "{uri} shows A's {secret}: {text}");
        }
    }
    for (uri, own) in [
        ("/api/v1/auth/tokens", &fx.b_ids.token),
        ("/api/v1/auth/passkeys", &fx.b_ids.passkey),
    ] {
        let (_, text) = send(&fx.app, Who::Session(&fx.b_cookie), &Method::GET, uri, None).await;
        let listed: Value = serde_json::from_str(&text).expect("json");
        let ids = listed
            .as_array()
            .expect("list")
            .iter()
            .map(|row| row["id"].as_str().expect("id"))
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [own.as_str()],
            "{uri} lists exactly B's own row: {text}"
        );
    }
    let (_, text) = send(
        &fx.app,
        Who::Session(&fx.b_cookie),
        &Method::GET,
        "/api/boards",
        None,
    )
    .await;
    assert!(
        text.contains(&fx.b_ids.board),
        "B still lists its own board: {text}"
    );
    let (_, text) = send(
        &fx.app,
        Who::Session(&fx.a_cookie),
        &Method::GET,
        "/api/boards",
        None,
    )
    .await;
    assert!(text.contains(&fx.a_ids.board), "A lists its board: {text}");
    assert!(
        !text.contains(&fx.b_ids.board),
        "A does not list B's board: {text}"
    );
}

/// A calendar feed serves its own board's due tasks and nothing of another board.
#[tokio::test]
async fn calendar_feed_should_serve_only_its_board() {
    let fx = accounts().await;
    let owned = fx.db.account(&fx.a);
    for (task, name) in [
        (&fx.a_ids.task, "Feed me"),
        (&fx.a_ids.task2, "Feed me too"),
    ] {
        let proof = owned.task(task).expect("read").expect("task");
        owned
            .apply_task_change(&proof, Actor::SYSTEM, |row| {
                row.name = name.to_string();
                row.due_at = Some("2030-01-01T10:00:00Z".to_string());
                ((), true)
            })
            .expect("due")
            .expect("task");
    }
    let other = fx
        .db
        .create_column(&fx.a_ids.board2, "Other", None)
        .expect("column");
    let hidden = fx
        .db
        .create_task(&other, None, "Other board task", 1, None)
        .expect("task");
    fx.db
        .set_task_due(&hidden, Some("2030-01-01T10:00:00Z"), None)
        .expect("due");
    let link = format!("/api/boards/{}/calendar-feed", fx.a_ids.board);
    let (status, text) = send(&fx.app, Who::Token(&fx.a_token), &Method::POST, &link, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let feed: Value = serde_json::from_str(&text).expect("json");
    let path = feed["path"].as_str().expect("feed path").to_string();

    let (status, ical) = send(&fx.app, Who::Anonymous, &Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ical.contains("SUMMARY:Feed me"), "{ical}");
    assert!(!ical.contains("Other board task"), "{ical}");
    assert!(!ical.contains(&fx.b_ids.task), "{ical}");

    let (status, _) = send(&fx.app, Who::Token(&fx.b_token), &Method::GET, &link, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "B cannot read A's feed link");
}

/// Approving a device request mints the device's token for the account that approved it.
#[tokio::test]
async fn device_approval_should_mint_token_for_approver() {
    let fx = accounts().await;
    let start = Payload::Json(json!({"client_name": "cli", "scopes": ["read"]}));
    let (status, text) = send(
        &fx.app,
        Who::Anonymous,
        &Method::POST,
        "/api/v1/auth/device",
        Some(&start),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let codes: Value = serde_json::from_str(&text).expect("json");
    let user_code = codes["user_code"].as_str().expect("user code").to_string();
    let device_code = codes["device_code"]
        .as_str()
        .expect("device code")
        .to_string();

    let approve = Payload::Form(format!(
        "user_code={user_code}&scope=read&expires_in_days=1"
    ));
    let (status, text) = send(
        &fx.app,
        Who::Session(&fx.b_cookie),
        &Method::POST,
        "/device/approve",
        Some(&approve),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let poll = Payload::Json(json!({"device_code": device_code}));
    let (status, text) = send(
        &fx.app,
        Who::Anonymous,
        &Method::POST,
        "/api/v1/auth/device/token",
        Some(&poll),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let minted: Value = serde_json::from_str(&text).expect("json");
    let token = minted["token"].as_str().expect("token").to_string();

    let (_, text) = send(
        &fx.app,
        Who::Token(&token),
        &Method::GET,
        "/api/v1/auth/whoami",
        None,
    )
    .await;
    let whoami: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(whoami["handle"], "bob", "{text}");
    let held = fx
        .db
        .list_api_tokens()
        .expect("tokens")
        .into_iter()
        .filter(|record| record.name == "cli")
        .map(|record| record.account_id)
        .collect::<Vec<_>>();
    assert_eq!(held, [fx.b.as_str()]);
    let (_, text) = send(
        &fx.app,
        Who::Token(&token),
        &Method::GET,
        "/api/boards",
        None,
    )
    .await;
    assert!(text.contains(&fx.b_ids.board), "{text}");
    assert!(!text.contains(&fx.a_ids.board), "{text}");
}

/// A task request naming the caller's own swimlane from another board gets the answer an unknown
/// swimlane gets.
#[tokio::test]
async fn swimlane_from_another_board_should_look_unknown() {
    let fx = accounts().await;
    let b = &fx.b_ids;
    let other_lane = fx.db.create_swimlane(&b.board2, "Elsewhere").expect("lane");
    let task_uri = format!("/api/tasks/{}/move", b.task);
    for (method, uri, lane_body) in [
        (
            Method::POST,
            "/api/tasks",
            json!({"column_id": b.column, "swimlane_id": other_lane, "name": "Stray"}),
        ),
        (
            Method::POST,
            task_uri.as_str(),
            json!({"column_id": b.column, "swimlane_id": other_lane, "position": 0}),
        ),
    ] {
        let (status, text) = send(
            &fx.app,
            Who::Token(&fx.b_token),
            &method,
            uri,
            Some(&Payload::Json(lane_body)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}: {text}");
        assert!(text.contains("unknown swimlane"), "{method} {uri}: {text}");
    }
}

/// Once the first account exists, a token minted before it and the bootstrap token both act as
/// that account, and nothing writes ownerless rows any more.
#[tokio::test]
async fn ownerless_credentials_should_act_as_the_owner_after_setup() {
    const BOOTSTRAP: &str = "pomb_access_bootstrap_token_for_tests";
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("setup.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let (record, early) =
        new_api_token_record(&AccountId::ownerless(), "early", AuthScope::Admin, None);
    db.create_api_token(&record).expect("token");
    let state = AppState::new(db.clone(), BootstrapToken::new(BOOTSTRAP), passkey_mode())
        .with_limits(unthrottled_limits());
    let app = router(state);
    let (status, text) = send(&app, Who::Token(&early), &Method::GET, "/api/boards", None).await;
    assert_eq!(status, StatusCode::OK, "before setup: {text}");

    let owner = db.ensure_owner("owner").expect("owner").account_id();
    let (status, text) = send(
        &app,
        Who::Token(&early),
        &Method::GET,
        "/api/v1/auth/whoami",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "after setup: {text}");
    let whoami: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(whoami["handle"], "owner", "{text}");
    let held = db.list_api_tokens().expect("tokens");
    assert_eq!(held[0].account_id, owner.as_str());

    let create = Payload::Json(json!({"name": "Bootstrapped"}));
    let (status, text) = send(
        &app,
        Who::Token(BOOTSTRAP),
        &Method::POST,
        "/api/boards",
        Some(&create),
    )
    .await;
    assert!(
        status.is_success(),
        "bootstrap after setup: {status} {text}"
    );
    let created: Value = serde_json::from_str(&text).expect("json");
    let board = db
        .board(created["id"].as_str().expect("board id"))
        .expect("read")
        .expect("board");
    assert_eq!(board.owner_id, owner.as_str());
    assert!(
        db.owner_boards(&AccountId::ownerless())
            .expect("boards")
            .is_empty()
    );
}
