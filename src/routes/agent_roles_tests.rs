use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call};
use super::{history_actor, router};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{ChildTokenRefusal, Db, MAX_AGENT_ROLES_PER_ACCOUNT, RoleTokenRefusal};
use crate::models::{
    AccountId, AgentRoleRow, ApiTokenRecord, AuthScope, BoardAccess, BoardRole, HistoryKind,
};
use crate::permissions::{Permission, Preset, STANDARD_ROLES};

/// Two accounts, each with a browser session, a read token and a board.
struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    alice: AccountId,
    bob: AccountId,
    cookies: [String; 2],
    tokens: [String; 2],
    boards: [String; 2],
}

const ALICE: usize = 0;
const BOB: usize = 1;

fn drop_seeded_roles(db: &Db, account: &AccountId) {
    for role in db.list_agent_roles(account.as_str()).expect("roles") {
        db.delete_agent_role(account.as_str(), &role.id)
            .expect("delete");
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-roles.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let alice = db.create_user("alice").expect("alice");
        let bob = db.create_user("bob").expect("bob");
        drop_seeded_roles(&db, &alice);
        drop_seeded_roles(&db, &bob);
        let cookie = |account: &AccountId| {
            format!(
                "session={}",
                db.create_session(account, "").expect("session")
            )
        };
        let token = |account: &AccountId, scope| {
            let (record, raw) = new_api_token_record(account, "agent", scope, None);
            db.create_api_token(&record).expect("token");
            raw
        };
        let cookies = [cookie(&alice), cookie(&bob)];
        let tokens = [
            token(&alice, AuthScope::Admin),
            token(&bob, AuthScope::Read),
        ];
        let boards = [
            db.create_board(&alice, "Alice").expect("board"),
            db.create_board(&bob, "Bob").expect("board"),
        ];
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            alice,
            bob,
            cookies,
            tokens,
            boards,
        }
    }

    async fn browse(&self, who: usize, method: &str, uri: &str, body: Option<Value>) -> Reply {
        call(
            &self.app,
            method,
            uri,
            &[("cookie", &self.cookies[who])],
            body,
        )
        .await
    }

    async fn bearer(&self, who: usize, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {}", self.tokens[who]);
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    async fn create(&self, who: usize, name: &str) -> Value {
        let body = json!({"name": name, "permissions": ["tasks.read", "tasks.claim"]});
        let made = self
            .browse(who, "POST", "/api/v1/agent-roles", Some(body))
            .await;
        assert_eq!(made.status, StatusCode::OK, "{}", made.body);
        made.body
    }
}

#[tokio::test]
async fn agent_role_create_should_reject_a_bad_name() {
    let fx = Fixture::new();
    let long = "a".repeat(33);
    for name in [
        "",
        "Planner",
        "has space",
        "under_score",
        "dot.name",
        long.as_str(),
    ] {
        let body = json!({"name": name, "permissions": ["tasks.read"]});
        let refused = fx
            .browse(ALICE, "POST", "/api/v1/agent-roles", Some(body))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{name:?}: {}",
            refused.body
        );
    }
    for permissions in [json!([]), json!(["no.such"])] {
        let body = json!({"name": "planner", "permissions": permissions});
        let refused = fx
            .browse(ALICE, "POST", "/api/v1/agent-roles", Some(body))
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    }

    let longest = "a".repeat(32);
    for name in ["planner", "agent-2", longest.as_str()] {
        fx.create(ALICE, name).await;
    }
    let body = json!({"name": "planner", "permissions": ["tasks.read"]});
    let taken = fx
        .browse(ALICE, "POST", "/api/v1/agent-roles", Some(body.clone()))
        .await;
    assert_eq!(taken.status, StatusCode::BAD_REQUEST, "{}", taken.body);
    let other = fx
        .browse(BOB, "POST", "/api/v1/agent-roles", Some(body))
        .await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
}

#[tokio::test]
async fn agent_role_rename_should_keep_the_id() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let other = fx.create(ALICE, "reviewer").await;
    let uri = format!("/api/v1/agent-roles/{}", role["id"].as_str().expect("id"));

    let body = json!({"name": "shaper", "permissions": ["tasks.read", "tasks.edit"]});
    let patched = fx.browse(ALICE, "PATCH", &uri, Some(body)).await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);
    assert_eq!(patched.body["id"], role["id"]);
    assert_eq!(patched.body["name"], "shaper");
    assert_eq!(
        patched.body["permissions"],
        json!(["tasks.read", "tasks.edit"])
    );
    assert_eq!(patched.body["created_at"], role["created_at"]);

    let renamed = fx
        .browse(ALICE, "PATCH", &uri, Some(json!({"name": "shaper-2"})))
        .await;
    assert_eq!(renamed.body["permissions"], patched.body["permissions"]);
    let fetched = fx.bearer(ALICE, "GET", &uri, None).await;
    assert_eq!(fetched.body["name"], "shaper-2");
    assert_eq!(fetched.body["id"], role["id"]);

    let taken = json!({"name": other["name"]});
    let refused = fx.browse(ALICE, "PATCH", &uri, Some(taken)).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let same = fx
        .browse(ALICE, "PATCH", &uri, Some(json!({"name": "shaper-2"})))
        .await;
    assert_eq!(same.status, StatusCode::OK, "{}", same.body);

    let bad = fx
        .browse(ALICE, "PATCH", &uri, Some(json!({"name": "Bad Name"})))
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    let empty = fx
        .browse(ALICE, "PATCH", &uri, Some(json!({"permissions": []})))
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);

    let missing = fx
        .browse(
            ALICE,
            "PATCH",
            "/api/v1/agent-roles/missing",
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(
        (missing.status, missing.code.as_deref()),
        (StatusCode::NOT_FOUND, Some("unknown_role"))
    );
}

#[tokio::test]
async fn agent_role_patch_should_refuse_a_bearer_token() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}");

    let create = json!({"name": "sneaky", "permissions": ["tasks.read"]});
    let made = fx
        .bearer(ALICE, "POST", "/api/v1/agent-roles", Some(create))
        .await;
    assert_eq!(
        (made.status, made.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("session_required")),
        "{}",
        made.body
    );
    for body in [
        json!({"name": "renamed"}),
        json!({"permissions": ["tasks.read", "tokens.issue"]}),
    ] {
        let edited = fx.bearer(ALICE, "PATCH", &uri, Some(body)).await;
        assert_eq!(
            (edited.status, edited.code.as_deref()),
            (StatusCode::FORBIDDEN, Some("session_required")),
            "{}",
            edited.body
        );
    }

    let after = fx.browse(ALICE, "GET", &uri, None).await;
    assert_eq!(after.body, role);
    let roles = fx.bearer(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(roles.body.as_array().expect("list").len(), 1);
}

#[tokio::test]
async fn board_delete_should_drop_role_grants() {
    let fx = Fixture::new();
    let kept = fx.db.create_board(&fx.alice, "Kept").expect("board");
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("id");
    for board in [&fx.boards[ALICE], &kept] {
        assert!(
            fx.db
                .grant_agent_role(fx.alice.as_str(), id, board)
                .expect("grant")
        );
    }
    assert!(
        !fx.db
            .grant_agent_role(fx.alice.as_str(), id, &fx.boards[BOB])
            .expect("grant"),
        "a board of another account takes no grant"
    );
    let uri = format!("/api/v1/agent-roles/{id}");
    let mut held = vec![fx.boards[ALICE].clone(), kept.clone()];
    held.sort();
    let before = fx.bearer(ALICE, "GET", &uri, None).await;
    assert_eq!(before.body["boards"], json!(held));

    fx.db.delete_board(&fx.boards[ALICE]).expect("delete");
    let after = fx.bearer(ALICE, "GET", &uri, None).await;
    assert_eq!(after.body["boards"], json!([kept]));
    assert_eq!(
        fx.db
            .revoke_agent_role(fx.alice.as_str(), id, &kept)
            .expect("revoke"),
        Some(0)
    );
    assert_eq!(
        fx.db
            .revoke_agent_role(fx.alice.as_str(), id, &kept)
            .expect("revoke"),
        None
    );
    let revoked = fx.bearer(ALICE, "GET", &uri, None).await;
    assert_eq!(revoked.body["boards"], json!([]));
}

#[tokio::test]
async fn deleting_an_account_should_drop_its_roles_and_grants() {
    let fx = Fixture::new();
    let role = fx.create(BOB, "planner").await;
    let id = role["id"].as_str().expect("id");
    assert!(
        fx.db
            .grant_agent_role(fx.bob.as_str(), id, &fx.boards[BOB])
            .expect("grant")
    );
    fx.db.delete_account(&fx.bob).expect("delete");
    assert!(
        fx.db
            .agent_role(fx.bob.as_str(), id)
            .expect("read")
            .is_none()
    );
    assert!(
        fx.db
            .list_agent_roles(fx.bob.as_str())
            .expect("list")
            .is_empty()
    );
    assert!(fx.db.agent_role_boards(id).expect("boards").is_empty());
}

#[tokio::test]
async fn agent_roles_should_be_scoped_to_their_account() {
    let fx = Fixture::new();
    let alice_role = fx.create(ALICE, "planner").await;
    let bob_role = fx.create(BOB, "reviewer").await;
    let alice_uri = format!(
        "/api/v1/agent-roles/{}",
        alice_role["id"].as_str().expect("id")
    );

    let listed = fx.bearer(BOB, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.body, json!([bob_role]));

    for reply in [
        fx.bearer(BOB, "GET", &alice_uri, None).await,
        fx.browse(BOB, "GET", &alice_uri, None).await,
        fx.browse(BOB, "PATCH", &alice_uri, Some(json!({"name": "taken"})))
            .await,
    ] {
        assert_eq!(
            (reply.status, reply.code.as_deref()),
            (StatusCode::NOT_FOUND, Some("unknown_role")),
            "{}",
            reply.body
        );
    }
    assert!(
        !fx.db
            .grant_agent_role(
                fx.bob.as_str(),
                alice_role["id"].as_str().expect("id"),
                &fx.boards[BOB]
            )
            .expect("grant")
    );
    let intact = fx.browse(ALICE, "GET", &alice_uri, None).await;
    assert_eq!(intact.body, alice_role);
}

#[tokio::test]
async fn board_owner_change_should_drop_role_grants() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("id");
    let board = &fx.boards[ALICE];
    assert!(
        fx.db
            .grant_agent_role(fx.alice.as_str(), id, board)
            .expect("grant")
    );
    fx.db
        .set_board_member(board, "bob", BoardRole::Member)
        .expect("join");
    fx.db
        .change_board_owner(board, &fx.alice, "bob")
        .expect("hand over");
    let uri = format!("/api/v1/agent-roles/{id}");
    let after = fx.bearer(ALICE, "GET", &uri, None).await;
    assert_eq!(after.body["boards"], json!([]));
}

#[tokio::test]
async fn agent_role_patch_should_refuse_an_empty_body() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let uri = format!("/api/v1/agent-roles/{}", role["id"].as_str().expect("id"));
    let refused = fx.browse(ALICE, "PATCH", &uri, Some(json!({}))).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let after = fx.browse(ALICE, "GET", &uri, None).await;
    assert_eq!(after.body, role);
}

fn issuer(fx: &Fixture) -> ApiTokenRecord {
    let held = fx.db.list_account_api_tokens(&fx.alice).expect("list");
    if let Some(found) = held.into_iter().find(|token| token.name == "issuer") {
        return found;
    }
    let (record, _) = new_api_token_record(&fx.alice, "issuer", AuthScope::Provision, None);
    fx.db.create_api_token(&record).expect("issuer");
    record
}

fn child_as(fx: &Fixture, name: &str) -> ApiTokenRecord {
    let parent = issuer(fx);
    let (mut record, _) = new_api_token_record(&fx.alice, name, AuthScope::Work, None);
    record.parent_id = Some(parent.id);
    record.parent_name = Some(parent.name);
    record
}

fn issue_as(fx: &Fixture, role: &Value, name: &str) -> (ApiTokenRecord, String) {
    issue_as_at(fx, role, name, chrono::Utc::now().timestamp())
}

fn issue_as_at(fx: &Fixture, role: &Value, name: &str, now: i64) -> (ApiTokenRecord, String) {
    let parent = issuer(fx);
    let (mut record, raw) = new_api_token_record(&fx.alice, name, AuthScope::Work, None);
    record.parent_id = Some(parent.id);
    record.parent_name = Some(parent.name);
    let role_id = role["id"].as_str().expect("role id");
    let stored = fx
        .db
        .create_api_token_as_role(role_id, record, 1, now)
        .expect("issue")
        .expect("role");
    (stored, raw)
}

async fn whoami(fx: &Fixture, raw: &str) -> Value {
    let bearer = format!("Bearer {raw}");
    let reply = call(
        &fx.app,
        "GET",
        "/api/v1/auth/whoami",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body
}

async fn listed_token(fx: &Fixture, id: &str) -> Value {
    let listed = fx.browse(ALICE, "GET", "/api/v1/auth/tokens", None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    listed
        .body
        .as_array()
        .expect("array")
        .iter()
        .find(|token| token["id"] == id)
        .expect("listed")
        .clone()
}

#[tokio::test]
async fn token_should_carry_its_role_snapshot() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let (record, _) = issue_as(&fx, &role, "agent-one");

    assert_eq!(record.name, "agent-one");
    assert_eq!(record.role_id.as_deref(), role["id"].as_str());
    assert_eq!(record.role_name.as_deref(), Some("planner"));
    assert_eq!(record.label.as_deref(), Some("agent-1"));
    let stored = fx.db.list_account_api_tokens(&fx.alice).expect("list");
    let stored = stored
        .iter()
        .find(|token| token.id == record.id)
        .expect("stored");
    assert_eq!(stored.role_id, record.role_id);
    assert_eq!(stored.role_name, record.role_name);
    assert_eq!(stored.label, record.label);

    let view = listed_token(&fx, &record.id).await;
    assert_eq!(view["name"], "agent-one");
    assert_eq!(view["role_name"], "planner");
    assert_eq!(view["label"], "agent-1");

    let now = chrono::Utc::now().timestamp();
    let refused = fx
        .db
        .create_api_token_as_role("missing", child_as(&fx, "other"), 1, now)
        .expect("refuse");
    assert_eq!(refused.map(|_| ()), Err(RoleTokenRefusal::UnknownRole));
    let (mut foreign_record, _) = new_api_token_record(&fx.bob, "bobs", AuthScope::Work, None);
    foreign_record.parent_id = Some("any".to_string());
    let foreign = fx
        .db
        .create_api_token_as_role(role["id"].as_str().expect("id"), foreign_record, 1, now)
        .expect("refuse");
    assert_eq!(foreign.map(|_| ()), Err(RoleTokenRefusal::UnknownRole));
}

#[tokio::test]
async fn role_rename_should_not_change_a_live_token_snapshot() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let (record, raw) = issue_as(&fx, &role, "agent-one");
    let uri = format!("/api/v1/agent-roles/{}", role["id"].as_str().expect("id"));

    let body = json!({"name": "shaper", "permissions": ["tasks.read"]});
    let patched = fx.browse(ALICE, "PATCH", &uri, Some(body)).await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);

    let view = listed_token(&fx, &record.id).await;
    assert_eq!(view["role_name"], "planner");
    assert_eq!(view["label"], "agent-1");
    let who = whoami(&fx, &raw).await;
    assert_eq!(who["role_name"], "planner");

    let (later, _) = issue_as(&fx, &patched.body, "agent-two");
    assert_eq!(later.role_name.as_deref(), Some("shaper"));
    assert_eq!(later.label.as_deref(), Some("agent-2"));
}

#[tokio::test]
async fn role_counter_should_not_reuse_a_label() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let other = fx.create(ALICE, "reviewer").await;
    let mut labels = Vec::new();
    let mut ids = Vec::new();
    for n in 0..10 {
        let (record, _) = issue_as(&fx, &role, &format!("agent-{n}-token"));
        labels.push(record.label.expect("label"));
        ids.push(record.id);
    }
    let expected: Vec<String> = (1..=10).map(|n| format!("agent-{n}")).collect();
    assert_eq!(labels, expected);

    for id in &ids {
        assert!(fx.db.delete_api_token(&fx.alice, id).expect("revoke"));
    }
    let (next, _) = issue_as(&fx, &role, "agent-next");
    assert_eq!(next.label.as_deref(), Some("agent-11"));
    let (first_of_other, _) = issue_as(&fx, &other, "other-first");
    assert_eq!(first_of_other.label.as_deref(), Some("agent-1"));
}

#[tokio::test]
async fn roleless_token_should_report_no_role() {
    let fx = Fixture::new();
    let (record, raw) = new_api_token_record(&fx.alice, "plain", AuthScope::Work, None);
    fx.db.create_api_token(&record).expect("token");

    let view = listed_token(&fx, &record.id).await;
    assert_eq!(view["role_name"], Value::Null);
    assert_eq!(view["label"], Value::Null);
    let who = whoami(&fx, &raw).await;
    assert_eq!(who["role_name"], Value::Null);
    assert_eq!(who["label"], Value::Null);
    assert_eq!(who["parent_token"], Value::Null);
}

#[tokio::test]
async fn whoami_should_report_role_label_and_issuer() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let (record, raw) = issue_as(&fx, &role, "agent-one");

    let who = whoami(&fx, &raw).await;
    assert_eq!(who["name"], "agent-one");
    assert_eq!(who["role_name"], "planner");
    assert_eq!(who["label"], "agent-1");
    assert_eq!(
        who["parent_token"],
        json!({"id": record.parent_id, "name": "issuer"})
    );
}

#[tokio::test]
async fn role_counter_should_not_advance_when_the_token_store_fails() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let (first, _) = issue_as(&fx, &role, "agent-first");
    assert_eq!(first.label.as_deref(), Some("agent-1"));

    let now = chrono::Utc::now().timestamp();
    let taken = fx
        .db
        .create_api_token_as_role(
            role["id"].as_str().expect("id"),
            child_as(&fx, "Agent-First"),
            1,
            now,
        )
        .expect("refuse");
    assert_eq!(
        taken.map(|_| ()),
        Err(RoleTokenRefusal::Child(ChildTokenRefusal::NameTaken))
    );
    let (next, _) = issue_as(&fx, &role, "agent-next");
    assert_eq!(next.label.as_deref(), Some("agent-2"));
}

#[test]
fn rows_from_before_the_role_snapshot_should_read_as_no_role() {
    let (record, _) = new_api_token_record(&AccountId::ownerless(), "old", AuthScope::Work, None);
    let mut token = serde_json::to_value(&record).expect("token json");
    for key in ["role_id", "role_name", "label"] {
        token.as_object_mut().expect("object").remove(key);
    }
    let token: ApiTokenRecord = serde_json::from_value(token).expect("old token");
    assert_eq!(
        (token.role_id, token.role_name, token.label),
        (None, None, None)
    );

    let role = json!({
        "id": "r", "account_id": "a", "name": "planner", "permissions": [],
        "created_at": 1, "updated_at": 1,
    });
    let role: AgentRoleRow = serde_json::from_value(role).expect("old role");
    assert_eq!(role.issued, 0);
}

const STANDARD: &str = "/api/v1/agent-roles/standard";
const STANDARD_NAMES: [&str; 6] = [
    "planner",
    "implementer",
    "reviewer",
    "verifier",
    "release",
    "lead",
];

fn names_of(reply: &Value, key: &str) -> Vec<String> {
    reply[key]
        .as_array()
        .expect("array")
        .iter()
        .map(|item| {
            item.as_str()
                .or(item["name"].as_str())
                .expect("name")
                .to_string()
        })
        .collect()
}

fn restricted_bearer(fx: &Fixture, boards: &[&str]) -> String {
    let (mut record, raw) = new_api_token_record(&fx.alice, "narrow", AuthScope::Admin, None);
    record.boards = BoardAccess::Only(boards.iter().map(|id| (*id).to_string()).collect());
    fx.db.create_api_token(&record).expect("token");
    format!("Bearer {raw}")
}

#[tokio::test]
async fn standard_roles_should_be_created_once() {
    let fx = Fixture::new();
    let body = json!({"boards": []});
    let reply = fx.bearer(ALICE, "POST", STANDARD, Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(names_of(&reply.body, "created"), STANDARD_NAMES);
    assert_eq!(names_of(&reply.body, "roles"), STANDARD_NAMES);
    let listed = fx.bearer(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.body.as_array().expect("list").len(), 6);
    let lead = reply.body["roles"][5]["permissions"]
        .as_array()
        .expect("permissions");
    assert!(lead.contains(&json!("tokens.issue")));
}

#[tokio::test]
async fn standard_roles_call_should_be_idempotent() {
    let fx = Fixture::new();
    let body = json!({"boards": [fx.boards[ALICE]]});
    let first = fx.bearer(ALICE, "POST", STANDARD, Some(body.clone())).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let second = fx.bearer(ALICE, "POST", STANDARD, Some(body)).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_eq!(second.body["created"], json!([]));
    assert_eq!(second.body["roles"], first.body["roles"]);
}

#[tokio::test]
async fn standard_roles_should_keep_an_edited_template() {
    let fx = Fixture::new();
    let first = fx
        .bearer(ALICE, "POST", STANDARD, Some(json!({"boards": []})))
        .await;
    let id = first.body["roles"][0]["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}");
    let edited = fx
        .browse(
            ALICE,
            "PATCH",
            &uri,
            Some(json!({"permissions": ["tasks.read"]})),
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);

    let again = fx
        .bearer(ALICE, "POST", STANDARD, Some(json!({"boards": []})))
        .await;
    assert_eq!(again.body["created"], json!([]));
    assert_eq!(again.body["roles"][0]["permissions"], json!(["tasks.read"]));
    let shown = fx.browse(ALICE, "GET", &uri, None).await;
    assert_eq!(shown.body["permissions"], json!(["tasks.read"]));
}

#[tokio::test]
async fn standard_roles_should_reuse_a_renamed_standard_role() {
    let fx = Fixture::new();
    let first = fx
        .bearer(ALICE, "POST", STANDARD, Some(json!({"boards": []})))
        .await;
    let id = first.body["roles"][2]["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}");
    let renamed = fx
        .browse(ALICE, "PATCH", &uri, Some(json!({"name": "checker"})))
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);

    let again = fx
        .bearer(ALICE, "POST", STANDARD, Some(json!({"boards": []})))
        .await;
    assert_eq!(again.body["created"], json!([]));
    assert_eq!(again.body["roles"][2]["id"], json!(id));
    let listed = fx.bearer(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.body.as_array().expect("list").len(), 6);
}

#[tokio::test]
async fn standard_roles_should_grant_every_listed_board() {
    let fx = Fixture::new();
    let second = fx.db.create_board(&fx.alice, "Second").expect("board");
    let mut held = vec![fx.boards[ALICE].clone(), second.clone()];
    held.sort();
    let body = json!({"boards": [fx.boards[ALICE], second, fx.boards[ALICE]]});
    let reply = fx.bearer(ALICE, "POST", STANDARD, Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    for role in reply.body["roles"].as_array().expect("roles") {
        assert_eq!(role["boards"], json!(held), "{}", role["name"]);
    }
}

#[tokio::test]
async fn standard_roles_should_refuse_a_board_the_caller_lacks() {
    let fx = Fixture::new();
    let second = fx.db.create_board(&fx.alice, "Second").expect("board");
    let narrow = restricted_bearer(&fx, &[&fx.boards[ALICE]]);
    let body = json!({"boards": [fx.boards[ALICE], second]});
    let reply = call(
        &fx.app,
        "POST",
        STANDARD,
        &[("authorization", &narrow)],
        Some(body),
    )
    .await;
    assert_eq!(
        (reply.status, reply.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("exceeds_grant")),
        "{}",
        reply.body
    );
    assert!(reply.body.to_string().contains(&second), "{}", reply.body);
    let listed = fx.bearer(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.body, json!([]));

    let foreign = json!({"boards": [fx.boards[BOB]]});
    let reply = fx.bearer(ALICE, "POST", STANDARD, Some(foreign)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    let listed = fx.bearer(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.body, json!([]));
}

#[tokio::test]
async fn role_grant_should_be_idempotent() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[ALICE]);
    for _ in 0..2 {
        let granted = fx.bearer(ALICE, "PUT", &uri, None).await;
        assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
        assert_eq!(granted.body["boards"], json!([fx.boards[ALICE]]));
    }
    let second = fx.db.create_board(&fx.alice, "Second").expect("board");
    let narrow = restricted_bearer(&fx, &[&fx.boards[ALICE]]);
    let uri = format!("/api/v1/agent-roles/{id}/boards/{second}");
    let refused = call(&fx.app, "PUT", &uri, &[("authorization", &narrow)], None).await;
    assert_eq!(
        (refused.status, refused.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("exceeds_grant")),
        "{}",
        refused.body
    );
    let browsed = fx.browse(ALICE, "PUT", &uri, None).await;
    assert_eq!(browsed.status, StatusCode::OK, "{}", browsed.body);
}

#[tokio::test]
async fn role_grant_of_unknown_board_should_be_not_found() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}/boards/no-such-board");
    let reply = fx.bearer(ALICE, "PUT", &uri, None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    let foreign = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[BOB]);
    let reply = fx.bearer(ALICE, "PUT", &foreign, None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    let shown = fx
        .bearer(ALICE, "GET", &format!("/api/v1/agent-roles/{id}"), None)
        .await;
    assert_eq!(shown.body["boards"], json!([]));
}

#[tokio::test]
async fn standard_roles_should_refuse_past_the_role_quota_and_write_nothing() {
    let fx = Fixture::new();
    for n in 0..=(MAX_AGENT_ROLES_PER_ACCOUNT - 5) {
        fx.db
            .create_agent_role(fx.alice.as_str(), &format!("custom-{n}"), read_perms())
            .expect("role");
    }
    let held = MAX_AGENT_ROLES_PER_ACCOUNT - 4;
    let body = json!({"boards": [fx.boards[ALICE]]});
    let reply = fx.bearer(ALICE, "POST", STANDARD, Some(body)).await;
    assert_eq!(
        (reply.status, reply.code.as_deref()),
        (StatusCode::CONFLICT, Some("quota_exceeded")),
        "{}",
        reply.body
    );
    let listed = fx.bearer(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.body.as_array().expect("list").len(), held);
    for role in listed.body.as_array().expect("list") {
        assert_eq!(role["boards"], json!([]));
    }
}

#[tokio::test]
async fn standard_roles_should_reuse_roles_that_hold_a_standard_name() {
    let fx = Fixture::new();
    for name in ["planner", "lead"] {
        fx.db
            .create_agent_role(fx.alice.as_str(), name, read_perms())
            .expect("role");
    }
    let body = json!({"boards": [fx.boards[ALICE]]});
    let reply = fx.bearer(ALICE, "POST", STANDARD, Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(names_of(&reply.body, "roles"), STANDARD_NAMES);
    assert_eq!(
        names_of(&reply.body, "created"),
        ["implementer", "reviewer", "verifier", "release"]
    );
    assert_eq!(reply.body["roles"][0]["permissions"], json!(["tasks.read"]));
    assert_eq!(reply.body["roles"][0]["boards"], json!([fx.boards[ALICE]]));
}

fn read_perms() -> BTreeSet<Permission> {
    BTreeSet::from([Permission::TasksRead])
}

const CHILDREN: &str = "/api/v1/auth/tokens/children";

async fn standard_role(fx: &Fixture, name: &str) -> Value {
    let body = json!({"boards": [fx.boards[ALICE]]});
    let reply = fx.bearer(ALICE, "POST", STANDARD, Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["roles"]
        .as_array()
        .expect("roles")
        .iter()
        .find(|role| role["name"] == name)
        .expect("role")
        .clone()
}

fn issuer_token(
    fx: &Fixture,
    permissions: Option<&[Permission]>,
    boards: BoardAccess,
    expires_at: Option<i64>,
) -> (ApiTokenRecord, String) {
    let (mut record, raw) = new_api_token_record(&fx.alice, "lead", AuthScope::Provision, None);
    if let Some(permissions) = permissions {
        record.permissions = permissions.iter().copied().collect();
    }
    record.boards = boards;
    record.expires_at = expires_at;
    fx.db.create_api_token(&record).expect("lead");
    (record, format!("Bearer {raw}"))
}

async fn issue_child(fx: &Fixture, bearer: &str, body: Value) -> Reply {
    call(
        &fx.app,
        "POST",
        CHILDREN,
        &[("authorization", bearer)],
        Some(body),
    )
    .await
}

fn permission_names(reply: &Value) -> BTreeSet<String> {
    reply["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .map(|name| name.as_str().expect("name").to_string())
        .collect()
}

fn board_ids(reply: &Value) -> BTreeSet<String> {
    reply["boards"]
        .as_array()
        .expect("boards")
        .iter()
        .map(|board| board["id"].as_str().expect("id").to_string())
        .collect()
}

fn token_total(fx: &Fixture) -> usize {
    fx.db
        .list_account_api_tokens(&fx.alice)
        .expect("list")
        .len()
}

fn assert_exceeds_grant(reply: &Reply) {
    assert_eq!(
        (reply.status, reply.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("exceeds_grant")),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn child_token_should_copy_the_role_template() {
    let fx = Fixture::new();
    let role = standard_role(&fx, "implementer").await;
    let (_, lead) = issuer_token(&fx, None, BoardAccess::All, None);

    let body = json!({"name": "impl-one", "role_id": role["id"]});
    let reply = issue_child(&fx, &lead, body).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    let template: BTreeSet<String> = permission_names(&role);
    let held: BTreeSet<String> = Preset::Lead
        .permissions()
        .iter()
        .map(|permission| permission.name().to_string())
        .collect();
    let expected: BTreeSet<String> = template.intersection(&held).cloned().collect();
    let granted = permission_names(&reply.body);
    assert_eq!(granted, expected);
    assert!(granted.contains("tasks.claim") && granted.contains("tasks.move"));
    assert!(!granted.contains("subtasks.check") && !granted.contains("tokens.issue"));
    assert_eq!(
        board_ids(&reply.body),
        BTreeSet::from([fx.boards[ALICE].clone()])
    );
    assert_eq!(reply.body["role_name"], "implementer");
    assert_eq!(reply.body["label"], "agent-1");

    let raw = reply.body["raw_token"].as_str().expect("raw");
    let who = whoami(&fx, raw).await;
    assert_eq!(who["role_name"], "implementer");
    assert_eq!(permission_names(&who), granted);
}

#[tokio::test]
async fn token_as_any_standard_role_should_list_boards_and_read_a_board() {
    let fx = Fixture::new();
    let created = fx
        .bearer(
            ALICE,
            "POST",
            STANDARD,
            Some(json!({"boards": [fx.boards[ALICE]]})),
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let (_, lead) = issuer_token(&fx, None, BoardAccess::All, None);
    let board = &fx.boards[ALICE];
    for role in created.body["roles"].as_array().expect("roles") {
        let name = role["name"].as_str().expect("name");
        assert!(
            permission_names(role).contains("boards.read"),
            "{name} template"
        );
        let body = json!({"name": format!("run-{name}"), "role_id": role["id"]});
        let issued = issue_child(&fx, &lead, body).await;
        assert_eq!(issued.status, StatusCode::OK, "{name}: {}", issued.body);
        let bearer = format!("Bearer {}", issued.body["raw_token"].as_str().expect("raw"));
        for uri in [
            "/api/boards".to_string(),
            format!("/api/boards/{board}/columns"),
            format!("/api/boards/{board}/colors"),
        ] {
            let reply = call(&fx.app, "GET", &uri, &[("authorization", &bearer)], None).await;
            assert_eq!(reply.status, StatusCode::OK, "{name} {uri}: {}", reply.body);
        }
    }
}

#[tokio::test]
async fn child_role_beyond_the_issuer_should_be_refused() {
    let fx = Fixture::new();
    let role = standard_role(&fx, "implementer").await;
    let (_, lead) = issuer_token(
        &fx,
        Some(&[Permission::TokensIssue, Permission::TasksRead]),
        BoardAccess::All,
        None,
    );
    let before = token_total(&fx);

    let cut = issue_child(&fx, &lead, json!({"name": "cut", "role_id": role["id"]})).await;
    assert_eq!(cut.status, StatusCode::OK, "{}", cut.body);
    assert_eq!(
        permission_names(&cut.body),
        BTreeSet::from(["tasks.read".to_string()])
    );
    assert_eq!(token_total(&fx), before + 1);

    for (name, extra) in [
        ("beyond-issuer", json!({"permissions": ["tasks.claim"]})),
        (
            "beyond-both",
            json!({"permissions": ["tasks.read", "tokens.issue"]}),
        ),
        ("all-boards", json!({"boards": "all"})),
    ] {
        let mut body = json!({"name": name, "role_id": role["id"]});
        body.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("extra").clone());
        let refused = issue_child(&fx, &lead, body).await;
        assert_exceeds_grant(&refused);
    }
    assert_eq!(token_total(&fx), before + 1);

    let (_, bare) = issuer_token(
        &fx,
        Some(&[Permission::TokensIssue]),
        BoardAccess::All,
        None,
    );
    let refused = issue_child(&fx, &bare, json!({"name": "empty", "role_id": role["id"]})).await;
    assert_exceeds_grant(&refused);
}

#[tokio::test]
async fn child_boards_should_be_issuer_and_role_grants_overlap() {
    let fx = Fixture::new();
    let role = standard_role(&fx, "implementer").await;
    let role_id = role["id"].as_str().expect("id");
    let second = fx.db.create_board(&fx.alice, "Second").expect("board");
    let third = fx.db.create_board(&fx.alice, "Third").expect("board");
    assert!(
        fx.db
            .grant_agent_role(fx.alice.as_str(), role_id, &second)
            .expect("grant")
    );
    let issuer_boards = BoardAccess::Only(BTreeSet::from([second.clone(), third.clone()]));
    let (_, lead) = issuer_token(&fx, None, issuer_boards, None);

    let (_, apart) = issuer_token(
        &fx,
        None,
        BoardAccess::Only(BTreeSet::from([third.clone()])),
        None,
    );
    let none = issue_child(&fx, &apart, json!({"name": "apart", "role_id": role_id})).await;
    assert_exceeds_grant(&none);

    let made = issue_child(&fx, &lead, json!({"name": "overlap", "role_id": role_id})).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(board_ids(&made.body), BTreeSet::from([second.clone()]));

    for beyond in [&third, &fx.boards[ALICE]] {
        let body = json!({"name": "beyond", "role_id": role_id, "boards": [beyond]});
        assert_exceeds_grant(&issue_child(&fx, &lead, body).await);
    }
    let body = json!({"name": "narrowed", "role_id": role_id, "boards": [second]});
    let narrowed = issue_child(&fx, &lead, body).await;
    assert_eq!(narrowed.status, StatusCode::OK, "{}", narrowed.body);
}

#[tokio::test]
async fn role_token_should_always_expire() {
    let fx = Fixture::new();
    let role = standard_role(&fx, "implementer").await;
    let now = chrono::Utc::now().timestamp();
    let (_, lead) = issuer_token(&fx, None, BoardAccess::All, None);
    let near = |token: &Value, hours: i64| {
        let expires = token["expires_at"].as_i64().expect("expires_at");
        (expires - now - hours * 3600).abs() < 120
    };

    let made = issue_child(
        &fx,
        &lead,
        json!({"name": "default", "role_id": role["id"]}),
    )
    .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert!(near(&made.body, 1), "{}", made.body);

    let body = json!({"name": "long", "role_id": role["id"], "expires_in_hours": 8});
    assert_eq!(
        issue_child(&fx, &lead, body.clone()).await.status,
        StatusCode::BAD_REQUEST
    );
    let (mut long_lived, long_raw) =
        new_api_token_record(&fx.alice, "long-lived", AuthScope::Provision, None);
    long_lived.child_lifetime_hours = 8;
    fx.db.create_api_token(&long_lived).expect("long lived");
    let long = issue_child(&fx, &format!("Bearer {long_raw}"), body).await;
    assert_eq!(long.status, StatusCode::OK, "{}", long.body);
    assert!(near(&long.body, 8), "{}", long.body);
    for hours in [0, 9] {
        let body = json!({"name": "bad", "role_id": role["id"], "expires_in_hours": hours});
        let refused = issue_child(&fx, &lead, body).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    }

    let (mut short, raw) = new_api_token_record(&fx.alice, "short", AuthScope::Provision, None);
    short.expires_at = Some(now + 600);
    fx.db.create_api_token(&short).expect("short");
    let capped = issue_child(
        &fx,
        &format!("Bearer {raw}"),
        json!({"name": "capped", "role_id": role["id"]}),
    )
    .await;
    assert_eq!(capped.status, StatusCode::OK, "{}", capped.body);
    assert_eq!(capped.body["expires_at"], now + 600);

    for token in fx.db.list_account_api_tokens(&fx.alice).expect("list") {
        assert!(
            token.role_id.is_none() || token.expires_at.is_some(),
            "{}",
            token.name
        );
    }
}

#[tokio::test]
async fn template_change_should_not_reach_a_live_child() {
    let fx = Fixture::new();
    let role = standard_role(&fx, "implementer").await;
    let (_, lead) = issuer_token(&fx, None, BoardAccess::All, None);
    let made = issue_child(&fx, &lead, json!({"name": "live", "role_id": role["id"]})).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let issued = permission_names(&made.body);
    let raw = made.body["raw_token"].as_str().expect("raw");

    let uri = format!("/api/v1/agent-roles/{}", role["id"].as_str().expect("id"));
    let patch = json!({"permissions": ["tasks.read"]});
    let edited = fx.browse(ALICE, "PATCH", &uri, Some(patch)).await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);

    let who = whoami(&fx, raw).await;
    assert_eq!(permission_names(&who), issued);
    let next = issue_child(&fx, &lead, json!({"name": "later", "role_id": role["id"]})).await;
    assert_eq!(
        permission_names(&next.body),
        BTreeSet::from(["boards.read", "tasks.read"].map(str::to_string))
    );
}

#[tokio::test]
async fn unknown_role_should_be_refused_at_issue() {
    let fx = Fixture::new();
    let (_, lead) = issuer_token(&fx, None, BoardAccess::All, None);
    let bobs = fx
        .db
        .create_agent_role(
            fx.bob.as_str(),
            "planner",
            BTreeSet::from([Permission::TasksRead]),
        )
        .expect("role");
    let before = token_total(&fx);
    for role_id in ["missing", bobs.id.as_str()] {
        let body = json!({"name": "stray", "role_id": role_id});
        let refused = issue_child(&fx, &lead, body).await;
        assert_eq!(
            (refused.status, refused.code.as_deref()),
            (StatusCode::BAD_REQUEST, Some("unknown_role")),
            "{}",
            refused.body
        );
    }
    assert_eq!(token_total(&fx), before);
}

#[tokio::test]
async fn role_child_should_pass_the_child_checks_in_one_write() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let role_id = role["id"].as_str().expect("id");
    let now = chrono::Utc::now().timestamp();
    let issued = || {
        fx.db
            .agent_role(fx.alice.as_str(), role_id)
            .expect("role")
            .expect("row")
            .issued
    };
    issuer(&fx);
    let before = token_total(&fx);

    let mut orphan = child_as(&fx, "orphan");
    orphan.parent_id = Some("gone".to_string());
    let refused = fx
        .db
        .create_api_token_as_role(role_id, orphan, 1, now)
        .expect("refuse");
    assert_eq!(
        refused.map(|_| ()),
        Err(RoleTokenRefusal::Child(ChildTokenRefusal::ParentGone))
    );

    let refused = fx
        .db
        .create_api_token_as_role(role_id, child_as(&fx, "long"), 2, now)
        .expect("refuse");
    assert_eq!(
        refused.map(|_| ()),
        Err(RoleTokenRefusal::Child(
            ChildTokenRefusal::LifetimeTooLong { allowed_hours: 1 }
        ))
    );

    let (parentless, _) = new_api_token_record(&fx.alice, "parentless", AuthScope::Work, None);
    assert!(
        fx.db
            .create_api_token_as_role(role_id, parentless, 1, now)
            .is_err()
    );
    assert_eq!((issued(), token_total(&fx)), (0, before));

    let (first, _) = issue_as_at(&fx, &role, "first", now);
    assert_eq!(first.expires_at.map(|at| at - now), Some(3600));
    let refused = fx
        .db
        .create_api_token_as_role(role_id, child_as(&fx, "FIRST"), 1, now)
        .expect("refuse");
    assert_eq!(
        refused.map(|_| ()),
        Err(RoleTokenRefusal::Child(ChildTokenRefusal::NameTaken))
    );

    let mut grandchild = child_as(&fx, "grandchild");
    grandchild.parent_id = Some(first.id.clone());
    grandchild.permissions.insert(Permission::TokensIssue);
    let refused = fx
        .db
        .create_api_token_as_role(role_id, grandchild, 1, now)
        .expect("refuse");
    assert_eq!(
        refused.map(|_| ()),
        Err(RoleTokenRefusal::Child(ChildTokenRefusal::IssuerIsIssued))
    );
    assert_eq!(issued(), 1);
}

fn acting_task(fx: &Fixture) -> String {
    let column = fx
        .db
        .create_column(&fx.boards[ALICE], "Todo", None)
        .expect("column");
    fx.db
        .create_task(&column, None, "Card", 1, None)
        .expect("task")
}

async fn act(raw: &str, fx: &Fixture, method: &str, uri: &str, body: Value) -> Reply {
    let bearer = format!("Bearer {raw}");
    call(
        &fx.app,
        method,
        uri,
        &[("authorization", &bearer)],
        Some(body),
    )
    .await
}

#[tokio::test]
async fn history_should_show_role_and_label() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = fx.create(ALICE, "implementer").await;
    let (_, raw) = issue_as(&fx, &role, "impl-one");
    let uri = format!("/api/tasks/{task}/claim");
    let claim = json!({"claimant": "me", "ttl_seconds": 60});
    let reply = act(&raw, &fx, "POST", &uri, claim).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    let row = fx.db.task(&task).expect("read").expect("task");
    let event = row.history.last().expect("event");
    assert_eq!(event.role_id.as_deref(), role["id"].as_str());
    assert_eq!(event.role_name.as_deref(), Some("implementer"));
    assert_eq!(event.label.as_deref(), Some("agent-1"));
    assert_eq!(history_actor(event), "implementer (agent-1)");
    let json = serde_json::to_value(event).expect("json");
    assert_eq!(json["role_name"], "implementer");
    assert_eq!(json["label"], "agent-1");
}

#[tokio::test]
async fn comment_should_record_the_token_snapshot() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = fx.create(ALICE, "implementer").await;
    let (record, raw) = issue_as(&fx, &role, "impl-one");
    let uri = format!("/api/tasks/{task}/comments");
    let reply = act(&raw, &fx, "POST", &uri, json!({"body": "done"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["role_id"], role["id"]);
    assert_eq!(reply.body["role_name"], "implementer");
    assert_eq!(reply.body["label"], "agent-1");

    let row = fx.db.task(&task).expect("read").expect("task");
    let comment = row.comments.last().expect("comment");
    assert_eq!(comment.role_name.as_deref(), Some("implementer"));
    assert_eq!(comment.label, record.label);
    let event = row.history.last().expect("event");
    assert_eq!(event.role_name.as_deref(), Some("implementer"));

    let modal = fx
        .bearer(ALICE, "GET", &format!("/api/tasks/{task}/modal"), None)
        .await;
    let html = modal.body.as_str().expect("html");
    assert!(html.contains(">via implementer</small>"), "{html}");
    assert!(
        html.contains("title=\"impl-one, agent-1, issued by issuer\""),
        "{html}"
    );
}

#[tokio::test]
async fn ten_implementer_tokens_should_each_comment_as_implementer_with_their_own_label() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = fx.create(ALICE, "implementer").await;
    let uri = format!("/api/tasks/{task}/comments");
    for number in 1..=10 {
        let (_, raw) = issue_as(&fx, &role, &format!("impl-{number}"));
        let reply = act(&raw, &fx, "POST", &uri, json!({"body": "hi"})).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }

    let row = fx.db.task(&task).expect("read").expect("task");
    let seen: Vec<(Option<&str>, Option<&str>)> = row
        .comments
        .iter()
        .map(|comment| (comment.role_name.as_deref(), comment.label.as_deref()))
        .collect();
    let expected: Vec<String> = (1..=10).map(|number| format!("agent-{number}")).collect();
    assert_eq!(seen.len(), 10);
    for (found, label) in seen.iter().zip(&expected) {
        assert_eq!(*found, (Some("implementer"), Some(label.as_str())));
    }
}

#[tokio::test]
async fn claim_should_ignore_a_client_role_for_a_role_token() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = fx.create(ALICE, "implementer").await;
    let (_, raw) = issue_as(&fx, &role, "impl-one");
    let uri = format!("/api/tasks/{task}/claim");
    let claim = json!({"claimant": "me", "ttl_seconds": 60, "role": "reviewer"});
    let reply = act(&raw, &fx, "POST", &uri, claim).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["role"], "implementer");
}

#[tokio::test]
async fn time_entry_should_store_the_token_role_id() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = fx.create(ALICE, "implementer").await;
    let (_, raw) = issue_as(&fx, &role, "impl-one");
    let uri = format!("/api/tasks/{task}/time");
    let entry = json!({"minutes": 5, "role": "reviewer"});
    let reply = act(&raw, &fx, "POST", &uri, entry).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let logged = &reply.body[0];
    assert_eq!(logged["role_id"], role["id"]);
    assert_eq!(logged["role_name"], "implementer");
    assert_eq!(logged["role"], "implementer");

    let rows = fx.db.list_entries(&task).expect("entries");
    assert_eq!(rows[0].role_id.as_deref(), role["id"].as_str());
    assert_eq!(rows[0].role_name.as_deref(), Some("implementer"));
}

#[tokio::test]
async fn roleless_token_should_keep_the_free_text_role() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let raw = &fx.tokens[ALICE];

    let claim = json!({"claimant": "me", "ttl_seconds": 60, "role": "reviewer"});
    let uri = format!("/api/tasks/{task}/claim");
    let reply = act(raw, &fx, "POST", &uri, claim).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["role"], "reviewer");

    let uri = format!("/api/tasks/{task}/time");
    let reply = act(
        raw,
        &fx,
        "POST",
        &uri,
        json!({"minutes": 5, "role": "tester"}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body[0]["role"], "tester");
    assert!(reply.body[0].get("role_id").is_none());

    let uri = format!("/api/tasks/{task}/comments");
    let reply = act(raw, &fx, "POST", &uri, json!({"body": "plain"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.get("role_name").is_none());
    assert!(reply.body.get("label").is_none());
    let row = fx.db.task(&task).expect("read").expect("task");
    let event = row.history.last().expect("event");
    assert_eq!(event.role_name, None);
    assert_eq!(history_actor(event), "api-token (agent)");
}

fn lead_bearer(fx: &Fixture, boards: &[&str]) -> String {
    let (mut record, raw) = new_api_token_record(&fx.alice, "lead", AuthScope::Provision, None);
    record.boards = BoardAccess::Only(boards.iter().map(|id| (*id).to_string()).collect());
    fx.db.create_api_token(&record).expect("token");
    format!("Bearer {raw}")
}

fn issue_on(fx: &Fixture, role: &Value, name: &str, boards: &[&str]) -> ApiTokenRecord {
    let parent = issuer(fx);
    let (mut record, _) = new_api_token_record(&fx.alice, name, AuthScope::Work, None);
    record.parent_id = Some(parent.id);
    record.parent_name = Some(parent.name);
    record.boards = BoardAccess::Only(boards.iter().map(|id| (*id).to_string()).collect());
    let role_id = role["id"].as_str().expect("role id");
    let now = chrono::Utc::now().timestamp();
    fx.db
        .create_api_token_as_role(role_id, record, 1, now)
        .expect("issue")
        .expect("role")
}

fn live_names(fx: &Fixture) -> BTreeSet<String> {
    fx.db
        .list_account_api_tokens(&fx.alice)
        .expect("list")
        .into_iter()
        .map(|token| token.name)
        .collect()
}

struct RoleTokens {
    role_id: String,
    other: String,
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_string()).collect()
}

async fn role_with_tokens(fx: &Fixture) -> RoleTokens {
    let other = fx.db.create_board(&fx.alice, "Other").expect("board");
    let role = fx.create(ALICE, "implementer").await;
    let role_id = role["id"].as_str().expect("id").to_string();
    for board in [&fx.boards[ALICE], &other] {
        assert!(
            fx.db
                .grant_agent_role(fx.alice.as_str(), &role_id, board)
                .expect("grant")
        );
    }
    let first = &fx.boards[ALICE];
    let on_first = issue_on(fx, &role, "on-first", &[first]);
    issue_on(fx, &role, "on-other", &[&other]);
    issue_on(fx, &role, "on-both", &[first, &other]);
    let (mut child, _) = new_api_token_record(&fx.alice, "child", AuthScope::Work, None);
    child.parent_id = Some(on_first.id);
    child.expires_at = on_first.expires_at;
    fx.db.create_api_token(&child).expect("child");
    RoleTokens { role_id, other }
}

#[tokio::test]
async fn grant_revoke_should_end_the_role_tokens_on_that_board_and_their_children() {
    let fx = Fixture::new();
    let held = role_with_tokens(&fx).await;
    let uri = format!(
        "/api/v1/agent-roles/{}/boards/{}",
        held.role_id, fx.boards[ALICE]
    );
    let revoked = fx.browse(ALICE, "DELETE", &uri, None).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body, json!({"revoked": 3}));
    assert_eq!(
        live_names(&fx),
        names(&["agent", "issuer", "on-other"]),
        "only the token on the other board stays"
    );
    let role = fx
        .bearer(
            ALICE,
            "GET",
            &format!("/api/v1/agent-roles/{}", held.role_id),
            None,
        )
        .await;
    assert_eq!(role.body["boards"], json!([held.other]));

    let again = fx.browse(ALICE, "DELETE", &uri, None).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.body);
}

#[tokio::test]
async fn grant_revoke_by_a_lead_should_reach_only_boards_it_holds() {
    let fx = Fixture::new();
    let held = role_with_tokens(&fx).await;
    let lead = lead_bearer(&fx, &[&fx.boards[ALICE]]);
    let refused = call(
        &fx.app,
        "DELETE",
        &format!("/api/v1/agent-roles/{}/boards/{}", held.role_id, held.other),
        &[("authorization", &lead)],
        None,
    )
    .await;
    assert_exceeds_grant(&refused);
    assert_eq!(
        live_names(&fx),
        names(&[
            "agent", "child", "issuer", "lead", "on-both", "on-first", "on-other"
        ])
    );

    let revoked = call(
        &fx.app,
        "DELETE",
        &format!(
            "/api/v1/agent-roles/{}/boards/{}",
            held.role_id, fx.boards[ALICE]
        ),
        &[("authorization", &lead)],
        None,
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body, json!({"revoked": 3}));
}

#[tokio::test]
async fn board_token_revoke_should_keep_the_grant() {
    let fx = Fixture::new();
    let held = role_with_tokens(&fx).await;
    let lead = lead_bearer(&fx, &[&fx.boards[ALICE]]);
    let uri = format!(
        "/api/v1/agent-roles/{}/boards/{}/revoke-tokens",
        held.role_id, fx.boards[ALICE]
    );
    let revoked = call(&fx.app, "POST", &uri, &[("authorization", &lead)], None).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body, json!({"revoked": 3}));
    assert_eq!(
        live_names(&fx),
        names(&["agent", "issuer", "lead", "on-other"])
    );
    let role = fx
        .bearer(
            ALICE,
            "GET",
            &format!("/api/v1/agent-roles/{}", held.role_id),
            None,
        )
        .await;
    assert_eq!(role.body["boards"].as_array().map(Vec::len), Some(2));

    let outside = format!(
        "/api/v1/agent-roles/{}/boards/{}/revoke-tokens",
        held.role_id, held.other
    );
    let refused = call(&fx.app, "POST", &outside, &[("authorization", &lead)], None).await;
    assert_exceeds_grant(&refused);
}

#[tokio::test]
async fn role_wide_revoke_should_need_the_owner_session_and_keep_the_role() {
    let fx = Fixture::new();
    let held = role_with_tokens(&fx).await;
    let uri = format!("/api/v1/agent-roles/{}/revoke-tokens", held.role_id);
    let refused = fx.bearer(ALICE, "POST", &uri, None).await;
    assert_eq!(
        (refused.status, refused.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("session_required")),
        "{}",
        refused.body
    );
    assert_eq!(live_names(&fx).len(), 6);

    let foreign = fx.browse(BOB, "POST", &uri, None).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
    assert_eq!(live_names(&fx).len(), 6);

    let revoked = fx.browse(ALICE, "POST", &uri, None).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body, json!({"revoked": 4}));
    assert_eq!(live_names(&fx), names(&["agent", "issuer"]));
    let role = fx
        .bearer(
            ALICE,
            "GET",
            &format!("/api/v1/agent-roles/{}", held.role_id),
            None,
        )
        .await;
    assert_eq!(role.body["boards"].as_array().map(Vec::len), Some(2));
}

fn work_bearer(fx: &Fixture) -> String {
    let (record, raw) = new_api_token_record(&fx.alice, "work", AuthScope::Work, None);
    fx.db.create_api_token(&record).expect("token");
    format!("Bearer {raw}")
}

#[tokio::test]
async fn lead_should_create_standard_roles_and_grant_held_boards() {
    let fx = Fixture::new();
    let lead = lead_bearer(&fx, &[&fx.boards[ALICE]]);
    let body = json!({"boards": [fx.boards[ALICE]]});
    let reply = call(
        &fx.app,
        "POST",
        STANDARD,
        &[("authorization", &lead)],
        Some(body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(names_of(&reply.body, "created"), STANDARD_NAMES);
    for role in reply.body["roles"].as_array().expect("roles") {
        assert_eq!(role["boards"], json!([fx.boards[ALICE]]), "{role}");
    }
    let lead_role = reply.body["roles"][STANDARD_NAMES.len() - 1]["permissions"]
        .as_array()
        .expect("permissions");
    assert!(lead_role.contains(&json!("roles.grant")));
}

#[tokio::test]
async fn lead_should_grant_a_held_board_and_not_one_it_lacks() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("id");
    let second = fx.db.create_board(&fx.alice, "Second").expect("board");
    let lead = lead_bearer(&fx, &[&fx.boards[ALICE]]);

    let uri = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[ALICE]);
    let granted = call(&fx.app, "PUT", &uri, &[("authorization", &lead)], None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    assert_eq!(granted.body["boards"], json!([fx.boards[ALICE]]));

    let uri = format!("/api/v1/agent-roles/{id}/boards/{second}");
    let refused = call(&fx.app, "PUT", &uri, &[("authorization", &lead)], None).await;
    assert_exceeds_grant(&refused);
    let after = fx
        .bearer(ALICE, "GET", &format!("/api/v1/agent-roles/{id}"), None)
        .await;
    assert_eq!(after.body["boards"], json!([fx.boards[ALICE]]));
}

#[tokio::test]
async fn work_token_should_be_refused_both_grant_routes_naming_roles_grant() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("id");
    let work = work_bearer(&fx);
    let grant = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[ALICE]);
    let body = json!({"boards": [fx.boards[ALICE]]});
    for (method, uri, body) in [
        ("POST", STANDARD, Some(body)),
        ("PUT", grant.as_str(), None),
    ] {
        let refused = call(&fx.app, method, uri, &[("authorization", &work)], body).await;
        assert_eq!(
            (refused.status, refused.code.as_deref()),
            (StatusCode::FORBIDDEN, Some("permission_denied")),
            "{method} {uri}: {}",
            refused.body
        );
        assert_eq!(
            refused.body["permission"], "roles.grant",
            "{}",
            refused.body
        );
    }
    let after = fx
        .bearer(ALICE, "GET", &format!("/api/v1/agent-roles/{id}"), None)
        .await;
    assert_eq!(after.body["boards"], json!([]));
}

#[tokio::test]
async fn lead_should_not_create_or_edit_a_role() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let uri = format!("/api/v1/agent-roles/{}", role["id"].as_str().expect("id"));
    let lead = lead_bearer(&fx, &[&fx.boards[ALICE]]);
    let create = json!({"name": "sneaky", "permissions": ["tasks.read"]});
    for (method, uri, body) in [
        ("POST", "/api/v1/agent-roles", Some(create)),
        ("PATCH", uri.as_str(), Some(json!({"name": "renamed"}))),
    ] {
        let refused = call(&fx.app, method, uri, &[("authorization", &lead)], body).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    let after = fx.browse(ALICE, "GET", &uri, None).await;
    assert_eq!(after.body, role);
}

#[tokio::test]
async fn role_delete_should_need_the_owner_session_and_remove_the_role_and_its_tokens() {
    let fx = Fixture::new();
    let held = role_with_tokens(&fx).await;
    let uri = format!("/api/v1/agent-roles/{}", held.role_id);
    let lead = lead_bearer(&fx, &[&fx.boards[ALICE]]);
    let by_lead = call(&fx.app, "DELETE", &uri, &[("authorization", &lead)], None).await;
    assert_eq!(
        (by_lead.status, by_lead.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("permission_denied")),
        "{}",
        by_lead.body
    );
    let refused = fx.bearer(ALICE, "DELETE", &uri, None).await;
    assert_eq!(
        (refused.status, refused.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("session_required")),
        "{}",
        refused.body
    );
    assert_eq!(live_names(&fx).len(), 7);

    let foreign = fx.browse(BOB, "DELETE", &uri, None).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
    assert_eq!(live_names(&fx).len(), 7);

    let deleted = fx.browse(ALICE, "DELETE", &uri, None).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(deleted.body, json!({"deleted": held.role_id, "revoked": 4}));
    assert_eq!(live_names(&fx), names(&["agent", "issuer", "lead"]));
    let gone = fx.bearer(ALICE, "GET", &uri, None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.body);
    let again = fx.browse(ALICE, "DELETE", &uri, None).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.body);
}

#[tokio::test]
async fn role_delete_should_keep_the_role_name_on_history_comments_and_time_entries() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = fx.create(ALICE, "implementer").await;
    let (_, raw) = issue_as(&fx, &role, "impl-one");
    let claim = json!({"claimant": "me", "ttl_seconds": 60});
    let claim_uri = format!("/api/tasks/{task}/claim");
    let claimed = act(&raw, &fx, "POST", &claim_uri, claim).await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.body);
    let comment = json!({"body": "done"});
    let comment_uri = format!("/api/tasks/{task}/comments");
    let said = act(&raw, &fx, "POST", &comment_uri, comment).await;
    assert_eq!(said.status, StatusCode::OK, "{}", said.body);
    let entry = json!({"minutes": 5});
    let time_uri = format!("/api/tasks/{task}/time");
    let logged = act(&raw, &fx, "POST", &time_uri, entry).await;
    assert_eq!(logged.status, StatusCode::OK, "{}", logged.body);

    let uri = format!("/api/v1/agent-roles/{}", role["id"].as_str().expect("id"));
    let deleted = fx.browse(ALICE, "DELETE", &uri, None).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(deleted.body["revoked"], 1);
    assert!(live_names(&fx).iter().all(|name| name != "impl-one"));

    let row = fx.db.task(&task).expect("read").expect("task");
    let stamped = row
        .history
        .iter()
        .filter(|event| event.role_name.is_some())
        .count();
    assert!(stamped >= 2, "{stamped}");
    assert!(
        row.history
            .iter()
            .filter(|event| event.role_name.is_some())
            .all(|event| event.role_name.as_deref() == Some("implementer"))
    );
    let comment = row.comments.last().expect("comment");
    assert_eq!(comment.role_name.as_deref(), Some("implementer"));
    let rows = fx.db.list_entries(&task).expect("entries");
    assert_eq!(rows[0].role_name.as_deref(), Some("implementer"));

    let modal = fx
        .bearer(ALICE, "GET", &format!("/api/tasks/{task}/modal"), None)
        .await;
    let html = modal.body.as_str().expect("html");
    assert!(html.contains(">via implementer</small>"), "{html}");
}

#[tokio::test]
async fn browser_token_should_issue_as_a_role_within_its_template() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let body = json!({
        "name": "made",
        "role_id": role["id"],
        "permissions": ["tasks.read"],
    });
    let made = fx
        .browse(ALICE, "POST", "/api/v1/auth/tokens", Some(body))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let token = listed_token(&fx, made.body["id"].as_str().expect("id")).await;
    assert_eq!(token["role_name"], "planner");
    assert_eq!(token["label"], "agent-1");
    let stored = fx.db.list_account_api_tokens(&fx.alice).expect("list");
    let stored = stored
        .iter()
        .find(|record| record.id == made.body["id"])
        .expect("stored");
    assert_eq!(stored.role_id.as_deref(), role["id"].as_str());

    let whole = fx
        .browse(
            ALICE,
            "POST",
            "/api/v1/auth/tokens",
            Some(json!({"name": "whole", "role_id": role["id"]})),
        )
        .await;
    assert_eq!(whole.status, StatusCode::OK, "{}", whole.body);
    let mut held: Vec<&str> = whole.body["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    held.sort_unstable();
    assert_eq!(held, ["boards.read", "tasks.claim", "tasks.read"]);
    assert_eq!(whole.body["label"], "agent-2");
}

#[tokio::test]
async fn browser_token_should_refuse_a_role_it_cannot_honor() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let above = fx
        .browse(
            ALICE,
            "POST",
            "/api/v1/auth/tokens",
            Some(json!({"name": "above", "role_id": role["id"], "preset": "admin"})),
        )
        .await;
    assert_exceeds_grant(&above);

    for (who, id) in [(ALICE, json!("no-such-role")), (BOB, role["id"].clone())] {
        let unknown = fx
            .browse(
                who,
                "POST",
                "/api/v1/auth/tokens",
                Some(json!({"name": "stray", "role_id": id, "preset": "read"})),
            )
            .await;
        assert_eq!(
            (unknown.status, unknown.code.as_deref()),
            (StatusCode::BAD_REQUEST, Some("unknown_role")),
            "{}",
            unknown.body
        );
    }
    assert_eq!(token_total(&fx), 1, "only the fixture's own token exists");
}

async fn role_without_boards_read(fx: &Fixture) -> Value {
    let body = json!({"name": "reader", "permissions": ["tasks.read", "comments.read"]});
    let made = fx
        .browse(ALICE, "POST", "/api/v1/agent-roles", Some(body))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    made.body
}

#[tokio::test]
async fn browser_token_as_a_role_should_always_hold_the_required_permissions() {
    let fx = Fixture::new();
    let role = role_without_boards_read(&fx).await;
    let picker = json!({
        "name": "picked",
        "role_id": role["id"],
        "permissions": ["tasks.read", "comments.read", "boards.read"],
    });
    let made = fx
        .browse(ALICE, "POST", "/api/v1/auth/tokens", Some(picker))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert!(permission_names(&made.body).contains("boards.read"));
    let token = listed_token(&fx, made.body["id"].as_str().expect("id")).await;
    assert_eq!(token["role_name"], "reader");

    let bare = json!({"name": "bare", "role_id": role["id"]});
    let made = fx
        .browse(ALICE, "POST", "/api/v1/auth/tokens", Some(bare))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(
        permission_names(&made.body),
        BTreeSet::from(["boards.read", "comments.read", "tasks.read"].map(str::to_string))
    );

    let extra = json!({
        "name": "extra",
        "role_id": role["id"],
        "permissions": ["tasks.read", "boards.read", "tasks.claim"],
    });
    let refused = fx
        .browse(ALICE, "POST", "/api/v1/auth/tokens", Some(extra))
        .await;
    assert_exceeds_grant(&refused);
}

#[tokio::test]
async fn child_as_a_role_should_get_a_required_permission_only_the_issuer_holds() {
    let fx = Fixture::new();
    let role = role_without_boards_read(&fx).await;
    let id = role["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[ALICE]);
    let granted = fx.bearer(ALICE, "PUT", &uri, None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);

    let (_, lead) = issuer_token(
        &fx,
        Some(&[
            Permission::TokensIssue,
            Permission::TasksRead,
            Permission::CommentsRead,
            Permission::BoardsRead,
        ]),
        BoardAccess::All,
        None,
    );
    let body = json!({"name": "with", "role_id": role["id"]});
    let child = issue_child(&fx, &lead, body).await;
    assert_eq!(child.status, StatusCode::OK, "{}", child.body);
    assert!(permission_names(&child.body).contains("boards.read"));

    let (_, narrow) = issuer_token(
        &fx,
        Some(&[Permission::TokensIssue, Permission::TasksRead]),
        BoardAccess::All,
        None,
    );
    let body = json!({"name": "without", "role_id": role["id"]});
    let child = issue_child(&fx, &narrow, body).await;
    assert_eq!(child.status, StatusCode::OK, "{}", child.body);
    assert_eq!(
        permission_names(&child.body),
        BTreeSet::from(["tasks.read".to_string()])
    );
    let body = json!({
        "name": "asks",
        "role_id": role["id"],
        "permissions": ["tasks.read", "boards.read"],
    });
    assert_exceeds_grant(&issue_child(&fx, &narrow, body).await);
}

#[tokio::test]
async fn child_as_a_role_should_be_refused_when_the_caller_shares_only_a_required_permission() {
    let fx = Fixture::new();
    let role = role_without_boards_read(&fx).await;
    let id = role["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[ALICE]);
    let granted = fx.bearer(ALICE, "PUT", &uri, None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    let (_, caller) = issuer_token(
        &fx,
        Some(&[Permission::TokensIssue, Permission::BoardsRead]),
        BoardAccess::All,
        None,
    );
    let before = token_total(&fx);
    let body = json!({"name": "idle", "role_id": role["id"]});
    assert_exceeds_grant(&issue_child(&fx, &caller, body).await);
    assert_eq!(token_total(&fx), before);
}

#[tokio::test]
async fn child_as_a_boards_read_only_role_should_hold_boards_read() {
    let fx = Fixture::new();
    let body = json!({"name": "viewer", "permissions": ["boards.read"]});
    let made = fx
        .browse(ALICE, "POST", "/api/v1/agent-roles", Some(body))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let id = made.body["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[ALICE]);
    let granted = fx.bearer(ALICE, "PUT", &uri, None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    let (_, lead) = issuer_token(&fx, None, BoardAccess::All, None);
    let child = issue_child(&fx, &lead, json!({"name": "view", "role_id": id})).await;
    assert_eq!(child.status, StatusCode::OK, "{}", child.body);
    assert_eq!(
        permission_names(&child.body),
        BTreeSet::from(["boards.read".to_string()])
    );
}

#[tokio::test]
async fn child_as_a_role_should_narrow_to_a_listed_required_permission() {
    let fx = Fixture::new();
    let body = json!({"name": "reader", "permissions": ["tasks.read"]});
    let made = fx
        .browse(ALICE, "POST", "/api/v1/agent-roles", Some(body))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let id = made.body["id"].as_str().expect("id");
    let uri = format!("/api/v1/agent-roles/{id}/boards/{}", fx.boards[ALICE]);
    let granted = fx.bearer(ALICE, "PUT", &uri, None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    let (_, caller) = issuer_token(
        &fx,
        Some(&[
            Permission::TokensIssue,
            Permission::TasksRead,
            Permission::BoardsRead,
        ]),
        BoardAccess::All,
        None,
    );
    let body = json!({"name": "narrow", "role_id": id, "permissions": ["boards.read"]});
    let child = issue_child(&fx, &caller, body).await;
    assert_eq!(child.status, StatusCode::OK, "{}", child.body);
    assert_eq!(
        permission_names(&child.body),
        BTreeSet::from(["boards.read".to_string()])
    );
}

#[tokio::test]
async fn child_as_a_role_should_refuse_a_bad_preset_before_an_empty_cut() {
    let fx = Fixture::new();
    let role = role_without_boards_read(&fx).await;
    let (_, caller) = issuer_token(
        &fx,
        Some(&[Permission::TokensIssue, Permission::BoardsRead]),
        BoardAccess::All,
        None,
    );
    let body = json!({"name": "bad", "role_id": role["id"], "preset": "nonsense"});
    let reply = issue_child(&fx, &caller, body).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
}

fn issue_holding(fx: &Fixture, role: &Value, name: &str) -> (ApiTokenRecord, String) {
    let parent = issuer(fx);
    let (mut record, raw) = new_api_token_record(&fx.alice, name, AuthScope::Work, None);
    record.parent_id = Some(parent.id);
    record.parent_name = Some(parent.name);
    record.permissions = permission_names(role)
        .iter()
        .map(|name| Permission::named(name).expect("permission"))
        .collect();
    let role_id = role["id"].as_str().expect("role id");
    let now = chrono::Utc::now().timestamp();
    let stored = fx
        .db
        .create_api_token_as_role(role_id, record, 1, now)
        .expect("issue")
        .expect("role");
    (stored, raw)
}

fn column_of(fx: &Fixture, task: &str) -> String {
    fx.db
        .task(task)
        .expect("read")
        .expect("task")
        .column_id
        .clone()
}

fn labels_of(fx: &Fixture, task: &str) -> Vec<String> {
    fx.db
        .task(task)
        .expect("read")
        .expect("task")
        .labels
        .clone()
}

#[tokio::test]
async fn implementer_should_escalate_a_card_to_the_lead_in_one_write() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = standard_role(&fx, "implementer").await;
    let (record, raw) = issue_holding(&fx, &role, "impl-one");
    assert!(!record.permissions.contains(&Permission::TasksEdit));
    fx.db
        .set_task_labels(&task, &["keep".to_string(), "needs:planner".to_string()])
        .expect("labels");
    let before = column_of(&fx, &task);
    let uri = format!("/api/tasks/{task}/escalate");
    let body = json!({"tier": "lead", "reason": "Stuck on the migration"});
    let reply = act(&raw, &fx, "POST", &uri, body).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["kind"], "blocked");
    assert_eq!(reply.body["body"], "Stuck on the migration");
    assert_eq!(reply.body["role_name"], "implementer");

    let row = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(row.labels, ["keep", "needs:lead"]);
    assert_eq!(row.column_id, before);
    assert_eq!(row.comments.len(), 1);
    assert_eq!(row.comments[0].kind.as_deref(), Some("blocked"));
    let events: Vec<_> = row.history.iter().rev().take(2).collect();
    assert_eq!(events[0].kind, HistoryKind::LabelsUpdated);
    assert_eq!(events[1].kind, HistoryKind::CommentAdded);
    assert!(
        events
            .iter()
            .all(|event| event.role_name.as_deref() == Some("implementer"))
    );
}

#[tokio::test]
async fn escalate_should_set_the_planner_tier_and_drop_the_lead_one() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = standard_role(&fx, "reviewer").await;
    let (_, raw) = issue_holding(&fx, &role, "rev-one");
    fx.db
        .set_task_labels(&task, &["needs:lead".to_string()])
        .expect("labels");
    let uri = format!("/api/tasks/{task}/escalate");
    let body = json!({"tier": "planner", "reason": "The outcome is wrong"});
    let reply = act(&raw, &fx, "POST", &uri, body).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(labels_of(&fx, &task), ["needs:planner"]);

    let again = json!({"tier": "planner", "reason": "Still wrong"});
    let reply = act(&raw, &fx, "POST", &uri, again).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(labels_of(&fx, &task), ["needs:planner"]);
    let row = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(row.comments.len(), 2);
}

#[tokio::test]
async fn escalate_should_log_one_labels_event_with_the_old_and_new_labels() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = standard_role(&fx, "reviewer").await;
    let (_, raw) = issue_holding(&fx, &role, "rev-one");
    fx.db
        .set_task_labels(&task, &["needs:lead".to_string()])
        .expect("labels");
    let uri = format!("/api/tasks/{task}/escalate");
    let body = json!({"tier": "planner", "reason": "The outcome is wrong"});
    let reply = act(&raw, &fx, "POST", &uri, body).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    let row = fx.db.task(&task).expect("read").expect("task");
    let labels_events: Vec<_> = row
        .history
        .iter()
        .filter(|event| event.kind == HistoryKind::LabelsUpdated)
        .collect();
    assert_eq!(labels_events.len(), 1);
    assert_eq!(labels_events[0].from.as_deref(), Some("needs:lead"));
    assert_eq!(labels_events[0].to.as_deref(), Some("needs:planner"));

    let again = json!({"tier": "planner", "reason": "Still wrong"});
    let reply = act(&raw, &fx, "POST", &uri, again).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let row = fx.db.task(&task).expect("read").expect("task");
    let labels_events = row
        .history
        .iter()
        .filter(|event| event.kind == HistoryKind::LabelsUpdated)
        .count();
    assert_eq!(labels_events, 1);
}

#[tokio::test]
async fn escalate_should_refuse_a_bad_tier_or_reason_and_write_nothing() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = standard_role(&fx, "implementer").await;
    let (_, raw) = issue_holding(&fx, &role, "impl-one");
    let uri = format!("/api/tasks/{task}/escalate");
    let long = "x".repeat(5001);
    for body in [
        json!({"tier": "human", "reason": "why"}),
        json!({"tier": "Lead", "reason": "why"}),
        json!({"reason": "why"}),
        json!({"tier": "lead"}),
        json!({"tier": "lead", "reason": "  "}),
        json!({"tier": "lead", "reason": long}),
    ] {
        let reply = act(&raw, &fx, "POST", &uri, body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
    }
    let row = fx.db.task(&task).expect("read").expect("task");
    assert!(row.labels.is_empty() && row.comments.is_empty());
}

#[tokio::test]
async fn escalate_should_refuse_a_locked_card_for_a_token() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let role = standard_role(&fx, "implementer").await;
    let (_, raw) = issue_holding(&fx, &role, "impl-one");
    fx.db
        .set_task_labels(&task, &["needs:human".to_string()])
        .expect("labels");
    let uri = format!("/api/tasks/{task}/escalate");
    let body = json!({"tier": "lead", "reason": "why"});
    let reply = act(&raw, &fx, "POST", &uri, body).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.body["code"], "card_locked");
    let row = fx.db.task(&task).expect("read").expect("task");
    assert_eq!(row.labels, ["needs:human"]);
    assert!(row.comments.is_empty());
}

#[tokio::test]
async fn escalate_should_need_comments_write() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let (mut record, raw) = new_api_token_record(&fx.alice, "reader", AuthScope::Work, None);
    record.permissions = BTreeSet::from([Permission::TasksRead]);
    fx.db.create_api_token(&record).expect("token");
    let uri = format!("/api/tasks/{task}/escalate");
    let body = json!({"tier": "lead", "reason": "why"});
    let reply = act(&raw, &fx, "POST", &uri, body).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert!(labels_of(&fx, &task).is_empty());
}

async fn subtask_call(
    fx: &Fixture,
    raw: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Reply {
    let bearer = format!("Bearer {raw}");
    call(&fx.app, method, uri, &[("authorization", &bearer)], body).await
}

fn assert_subtasks_delete_denied(reply: &Reply, who: &str) {
    assert_eq!(
        (reply.status, reply.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("permission_denied")),
        "{who}: {}",
        reply.body
    );
    assert!(
        reply.body.to_string().contains("subtasks.delete"),
        "{who}: {}",
        reply.body
    );
}

#[tokio::test]
async fn only_the_lead_role_should_delete_or_convert_a_subtask() {
    let fx = Fixture::new();
    let task = acting_task(&fx);
    let tasks = format!("/api/tasks/{task}/subtasks");
    let (_, issuer) = issuer_token(&fx, None, BoardAccess::All, None);
    let mut raws = Vec::new();
    for name in STANDARD_NAMES {
        let role = standard_role(&fx, name).await;
        let body = json!({"name": format!("{name}-agent"), "role_id": role["id"]});
        let issued = issue_child(&fx, &issuer, body).await;
        assert_eq!(issued.status, StatusCode::OK, "{}", issued.body);
        let raw = issued.body["raw_token"].as_str().expect("raw").to_string();
        raws.push((name, raw));
    }
    let browser_made = |name: &str| {
        fx.db
            .add_subtask(&task, name)
            .expect("subtask")
            .expect("task")
            .id
    };

    for (name, raw) in &raws {
        if *name == "lead" {
            continue;
        }
        let foreign = browser_made("foreign");
        let uri = format!("{tasks}/{foreign}");
        let reply = subtask_call(&fx, raw, "DELETE", &uri, None).await;
        assert_subtasks_delete_denied(&reply, name);
    }
    let raw_of = |wanted: &str| {
        &raws
            .iter()
            .find(|(name, _)| *name == wanted)
            .expect("role")
            .1
    };
    let planner = raw_of("planner");
    let own = subtask_call(&fx, planner, "POST", &tasks, Some(json!({"name": "own"}))).await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    let own_uri = format!("{tasks}/{}", own.body["id"].as_str().expect("id"));
    let reply = subtask_call(&fx, planner, "DELETE", &own_uri, None).await;
    assert_subtasks_delete_denied(&reply, "planner own");
    let convert = format!("{own_uri}/convert");
    let reply = subtask_call(&fx, planner, "POST", &convert, Some(json!({}))).await;
    assert_subtasks_delete_denied(&reply, "planner own convert");

    let lead = raw_of("lead");
    let reply = subtask_call(&fx, lead, "DELETE", &own_uri, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let foreign = browser_made("foreign");
    let uri = format!("{tasks}/{foreign}");
    let reply = subtask_call(&fx, lead, "DELETE", &uri, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let convertible = browser_made("convertible");
    let uri = format!("{tasks}/{convertible}/convert");
    let reply = subtask_call(&fx, lead, "POST", &uri, Some(json!({}))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
}

async fn assert_lead_runs_the_cards(fx: &Fixture, raw: &str) -> String {
    let board = &fx.boards[ALICE];
    let todo = fx.db.create_column(board, "Todo", None).expect("column");
    let doing = fx.db.create_column(board, "Doing", None).expect("column");
    let task = fx.db.create_task(&todo, None, "A", 1, None).expect("task");
    let other = fx.db.create_task(&todo, None, "B", 2, None).expect("task");
    let sub = fx
        .db
        .add_subtask(&task, "step")
        .expect("subtask")
        .expect("task")
        .id;
    let planner = standard_role(fx, "planner").await;
    let call_as = |method: &'static str, uri: String, body: Option<Value>| async move {
        subtask_call(fx, raw, method, &uri, body).await
    };

    let allowed = [
        ("GET", "/api/boards".to_string(), None, StatusCode::OK),
        (
            "GET",
            "/api/v1/agent-roles".to_string(),
            None,
            StatusCode::OK,
        ),
        (
            "POST",
            "/api/tasks".to_string(),
            Some(json!({"column_id": todo, "name": "New"})),
            StatusCode::CREATED,
        ),
        (
            "PATCH",
            format!("/api/tasks/{task}"),
            Some(json!({"name": "Renamed"})),
            StatusCode::OK,
        ),
        (
            "POST",
            format!("/api/tasks/{task}/move"),
            Some(json!({"column_id": doing, "position": 0})),
            StatusCode::OK,
        ),
        (
            "POST",
            format!("/api/tasks/{task}/claim"),
            Some(json!({"claimant": "me", "ttl_seconds": 60})),
            StatusCode::OK,
        ),
        (
            "POST",
            format!("/api/tasks/{task}/links"),
            Some(json!({"kind": "related", "target_id": other})),
            StatusCode::OK,
        ),
        (
            "PATCH",
            format!("/api/columns/{doing}"),
            Some(json!({"is_blocked": true})),
            StatusCode::OK,
        ),
        (
            "POST",
            format!("/api/tasks/{task}/subtasks"),
            Some(json!({"name": "lead made"})),
            StatusCode::OK,
        ),
        (
            "PATCH",
            format!("/api/tasks/{task}/subtasks/{sub}"),
            Some(json!({"name": "renamed"})),
            StatusCode::OK,
        ),
        (
            "PUT",
            format!(
                "/api/v1/agent-roles/{}/boards/{board}",
                planner["id"].as_str().expect("id")
            ),
            None,
            StatusCode::OK,
        ),
        (
            "DELETE",
            format!("/api/tasks/{other}"),
            None,
            StatusCode::OK,
        ),
    ];
    for (method, uri, body, status) in allowed {
        let reply = call_as(method, uri.clone(), body).await;
        assert_eq!(reply.status, status, "{method} {uri}: {}", reply.body);
    }
    let reply = call_as("DELETE", format!("/api/tasks/{task}/subtasks/{sub}"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    task
}

#[tokio::test]
async fn lead_role_token_should_run_the_cards_and_be_refused_the_board_structure() {
    let fx = Fixture::new();
    let board = &fx.boards[ALICE];
    let (_, issuer) = issuer_token(&fx, None, BoardAccess::All, None);
    let lead = standard_role(&fx, "lead").await;
    let template = permission_names(&lead);
    assert!(template.contains("tokens.issue") && template.contains("roles.grant"));
    let body = json!({"name": "lead-agent", "role_id": lead["id"]});
    let issued = issue_child(&fx, &issuer, body).await;
    assert_eq!(issued.status, StatusCode::OK, "{}", issued.body);
    let raw = issued.body["raw_token"].as_str().expect("raw");
    let task = assert_lead_runs_the_cards(&fx, raw).await;
    let call_as = |method: &'static str, uri: String, body: Option<Value>| {
        let fx = &fx;
        async move { subtask_call(fx, raw, method, &uri, body).await }
    };
    let refused = [
        ("POST", "/api/boards".to_string(), "boards.create"),
        (
            "PUT",
            format!("/api/boards/{board}/config"),
            "boards.configure",
        ),
        ("POST", "/api/swimlanes".to_string(), "swimlanes.create"),
        (
            "POST",
            format!("/api/boards/{board}/colors"),
            "colors.create",
        ),
        (
            "POST",
            format!("/api/boards/{board}/fields"),
            "fields.create",
        ),
        (
            "POST",
            format!("/api/boards/{board}/save-as-template"),
            "templates.save",
        ),
        ("POST", format!("/api/tasks/{task}/close"), "epics.manage"),
    ];
    for (method, uri, permission) in refused {
        let reply = call_as(method, uri.clone(), Some(json!({}))).await;
        assert_eq!(
            (reply.status, reply.code.as_deref()),
            (StatusCode::FORBIDDEN, Some("permission_denied")),
            "{method} {uri}: {}",
            reply.body
        );
        assert!(
            reply.body.to_string().contains(permission),
            "{uri}: {}",
            reply.body
        );
    }
    let reply = call_as(
        "PATCH",
        format!("/api/tasks/{task}"),
        Some(json!({"kind": "task"})),
    )
    .await;
    assert_eq!(
        (reply.status, reply.code.as_deref()),
        (StatusCode::FORBIDDEN, Some("permission_denied")),
        "{}",
        reply.body
    );
    assert!(
        reply.body.to_string().contains("epics.manage"),
        "{}",
        reply.body
    );
}

fn lead_set_issuer(fx: &Fixture) -> String {
    let lead_set: Vec<Permission> = STANDARD_ROLES
        .iter()
        .find(|role| role.name == "lead")
        .expect("lead")
        .permissions()
        .into_iter()
        .collect();
    issuer_token(fx, Some(&lead_set), BoardAccess::All, None).1
}

#[tokio::test]
async fn lead_role_token_should_mint_each_standard_role_with_its_full_set() {
    let fx = Fixture::new();
    let lead_bearer = lead_set_issuer(&fx);
    for name in ["verifier", "implementer", "planner"] {
        let role = standard_role(&fx, name).await;
        let body = json!({"name": name, "role_id": role["id"]});
        let child = issue_child(&fx, &lead_bearer, body).await;
        assert_eq!(child.status, StatusCode::OK, "{name}: {}", child.body);
        assert_eq!(
            permission_names(&child.body),
            permission_names(&role),
            "{name}"
        );
    }
}

#[tokio::test]
async fn verifier_child_of_a_lead_role_token_should_tick_a_subtask() {
    let fx = Fixture::new();
    let verifier = standard_role(&fx, "verifier").await;
    let lead_bearer = lead_set_issuer(&fx);
    let body = json!({"name": "verifier", "role_id": verifier["id"]});
    let child = issue_child(&fx, &lead_bearer, body).await;
    assert_eq!(child.status, StatusCode::OK, "{}", child.body);
    let raw = child.body["raw_token"].as_str().expect("raw");

    let task = acting_task(&fx);
    let sub = fx
        .db
        .add_subtask(&task, "tick")
        .expect("subtask")
        .expect("task")
        .id;
    let uri = format!("/api/tasks/{task}/subtasks/{sub}");
    let reply = subtask_call(&fx, raw, "PATCH", &uri, Some(json!({"done": true}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn token_holding_the_lead_role_set_should_issue_child_tokens_and_grant_boards() {
    let fx = Fixture::new();
    let lead_set: Vec<Permission> = STANDARD_ROLES
        .iter()
        .find(|role| role.name == "lead")
        .expect("lead")
        .permissions()
        .into_iter()
        .collect();
    let (_, lead) = issuer_token(&fx, Some(&lead_set), BoardAccess::All, None);
    let planner = standard_role(&fx, "planner").await;
    let body = json!({"name": "kid", "role_id": planner["id"]});
    let issued = issue_child(&fx, &lead, body).await;
    assert_eq!(issued.status, StatusCode::OK, "{}", issued.body);
    assert_eq!(issued.body["role_name"], "planner");

    let second = fx.db.create_board(&fx.alice, "Second").expect("board");
    let uri = format!(
        "/api/v1/agent-roles/{}/boards/{second}",
        planner["id"].as_str().expect("id")
    );
    let granted = call(&fx.app, "PUT", &uri, &[("authorization", &lead)], None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    assert!(
        granted.body["boards"]
            .as_array()
            .expect("boards")
            .contains(&json!(second)),
        "{}",
        granted.body
    );
}

#[tokio::test]
async fn browser_issued_lead_role_token_should_run_the_lead_actions_and_issue_and_grant() {
    let fx = Fixture::new();
    let lead = standard_role(&fx, "lead").await;
    let body = json!({"name": "lead-agent", "role_id": lead["id"]});
    let made = fx
        .browse(ALICE, "POST", "/api/v1/auth/tokens", Some(body))
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let held = permission_names(&made.body);
    assert!(held.contains("tokens.issue") && held.contains("roles.grant"));
    let raw = made.body["raw_token"].as_str().expect("raw");
    let bearer = format!("Bearer {raw}");
    assert_lead_runs_the_cards(&fx, raw).await;

    let planner = standard_role(&fx, "planner").await;
    let body = json!({"name": "kid", "role_id": planner["id"]});
    let issued = issue_child(&fx, &bearer, body).await;
    assert_eq!(issued.status, StatusCode::OK, "{}", issued.body);
    assert_eq!(issued.body["role_name"], "planner");

    let second = fx.db.create_board(&fx.alice, "Second").expect("board");
    let uri = format!(
        "/api/v1/agent-roles/{}/boards/{second}",
        planner["id"].as_str().expect("id")
    );
    let granted = call(&fx.app, "PUT", &uri, &[("authorization", &bearer)], None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    assert!(
        granted.body["boards"]
            .as_array()
            .expect("boards")
            .contains(&json!(second)),
        "{}",
        granted.body
    );
}

async fn settings_html(fx: &Fixture) -> String {
    let page = fx.browse(ALICE, "GET", "/settings", None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    page.body.as_str().expect("html").to_string()
}

fn roles_pane(html: &str) -> &str {
    let pane = html.split("id=\"agent-roles\"").nth(1).expect("roles pane");
    pane.split("class=\"settings-pane\"")
        .nth(1)
        .expect("pane end")
}

fn role_uri(role: &Value) -> String {
    format!("/api/v1/agent-roles/{}", role["id"].as_str().expect("id"))
}

#[tokio::test]
async fn settings_should_list_roles_with_boards_and_live_tokens() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    fx.create(ALICE, "reviewer").await;
    let granted = fx
        .browse(
            ALICE,
            "PUT",
            &format!("{}/boards/{}", role_uri(&role), fx.boards[ALICE]),
            None,
        )
        .await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    issue_as(&fx, &role, "agent-a");
    issue_as(&fx, &role, "agent-b");
    let (expired, _) = issue_as_at(&fx, &role, "agent-c", 0);
    assert!(expired.expires_at.is_some());

    let html = settings_html(&fx).await;

    assert!(html.contains("id=\"agent-roles\""), "roles pane");
    assert!(html.contains("planner <span class=\"role-tokens\">2 live tokens</span>"));
    assert!(html.contains("reviewer <span class=\"role-tokens\">0 live tokens</span>"));
    assert!(html.contains("data-boards=\"all\""), "{html}");
    let template: Vec<_> = role["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .map(|name| name.as_str().expect("name"))
        .collect();
    assert!(html.contains(&format!("data-permissions=\"{}\"", template.join(","))));
    assert!(html.contains(&format!("data-board-id=\"{}\"", fx.boards[ALICE])));
    assert!(html.contains("data-board-name=\"Alice\" checked"));
    assert!(!html.contains("data-board-name=\"Bob\""));
    assert!(html.contains("only tokens issued afterwards"));
}

#[tokio::test]
async fn settings_should_show_the_boards_of_a_role_token_limited_to_some_boards() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.alice, "Other").expect("board");
    let role = fx.create(ALICE, "implementer").await;
    let role_id = role["id"].as_str().expect("id").to_string();
    for board in [&fx.boards[ALICE], &other] {
        assert!(
            fx.db
                .grant_agent_role(fx.alice.as_str(), &role_id, board)
                .expect("grant")
        );
    }
    let first = fx.boards[ALICE].clone();
    issue_on(&fx, &role, "on-first", &[&first]);
    issue_on(&fx, &role, "on-both", &[&first, &other]);

    let html = settings_html(&fx).await;
    let pane = roles_pane(&html);

    assert!(pane.contains(&format!("data-boards=\"{first}\"")), "{pane}");
    let mut both = [first.as_str(), other.as_str()];
    both.sort();
    assert!(
        pane.contains(&format!("data-boards=\"{}\"", both.join(","))),
        "{pane}"
    );
    assert!(!pane.contains("data-boards=\"all\""), "{pane}");
}

#[tokio::test]
async fn settings_role_edit_should_save_the_template() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let uri = role_uri(&role);

    let saved = fx
        .browse(
            ALICE,
            "PATCH",
            &uri,
            Some(json!({"name": "architect", "permissions": ["tasks.read"]})),
        )
        .await;

    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let listed = fx.browse(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.body[0]["name"], "architect");
    assert_eq!(listed.body[0]["permissions"], json!(["tasks.read"]));
    let html = settings_html(&fx).await;
    assert!(html.contains("data-name=\"architect\" data-permissions=\"tasks.read\""));
    assert!(html.contains("class=\"kf-btn kf-btn-success role-save\""));
}

#[tokio::test]
async fn settings_role_board_toggle_should_grant_and_revoke() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let uri = role_uri(&role);
    let board_uri = format!("{uri}/boards/{}", fx.boards[ALICE]);
    issue_as(&fx, &role, "agent-a");
    assert!(
        !settings_html(&fx)
            .await
            .contains("data-board-name=\"Alice\" checked")
    );

    let granted = fx.browse(ALICE, "PUT", &board_uri, None).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    assert!(
        settings_html(&fx)
            .await
            .contains("data-board-name=\"Alice\" checked")
    );

    let revoked = fx.browse(ALICE, "DELETE", &board_uri, None).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_eq!(revoked.body["revoked"], 1);
    let html = settings_html(&fx).await;
    assert!(!html.contains("data-board-name=\"Alice\" checked"));
    assert!(html.contains("planner <span class=\"role-tokens\">0 live tokens</span>"));
}

#[tokio::test]
async fn settings_standard_roles_button_should_create_the_six() {
    let fx = Fixture::new();
    let html = settings_html(&fx).await;
    assert!(html.contains("id=\"roles-standard\""));
    assert!(html.contains("No agent roles yet."));

    let first = fx
        .browse(
            ALICE,
            "POST",
            "/api/v1/agent-roles/standard",
            Some(json!({"boards": []})),
        )
        .await;
    let second = fx
        .browse(
            ALICE,
            "POST",
            "/api/v1/agent-roles/standard",
            Some(json!({"boards": []})),
        )
        .await;

    assert_eq!(first.body["created"].as_array().expect("created").len(), 6);
    assert_eq!(second.body["created"], json!([]));
    let html = settings_html(&fx).await;
    assert_eq!(html.matches("class=\"role-card\"").count(), 6);
    for role in STANDARD_ROLES {
        let name = role.name;
        assert!(html.contains(&format!("data-name=\"{name}\"")), "{name}");
    }
}

#[tokio::test]
async fn settings_role_delete_should_ask_for_confirmation() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    issue_as(&fx, &role, "agent-a");
    let html = settings_html(&fx).await;
    assert!(html.contains("class=\"kf-btn role-delete\""));
    assert!(html.contains("class=\"kf-btn role-revoke-all\""));
    assert!(html.contains("id=\"confirm-dialog\""));
    let script = include_str!("../../static/pages.js");
    assert!(script.contains("confirmThen('Delete role'"));
    assert!(script.contains("confirmThen('Revoke all tokens'"));

    let revoked = fx
        .browse(
            ALICE,
            "POST",
            &format!("{}/revoke-tokens", role_uri(&role)),
            None,
        )
        .await;
    assert_eq!(revoked.body["revoked"], 1);
    issue_as(&fx, &role, "agent-b");
    let deleted = fx.browse(ALICE, "DELETE", &role_uri(&role), None).await;

    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(deleted.body["revoked"], 1);
    let listed = fx.browse(ALICE, "GET", "/api/v1/agent-roles", None).await;
    assert_eq!(listed.body, json!([]));
}

#[tokio::test]
async fn settings_should_count_tokens_issued_by_a_role_token() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let (parent, _) = issue_as(&fx, &role, "agent-a");
    let mut child = child_as(&fx, "agent-a-child");
    child.parent_id = Some(parent.id);
    fx.db.create_api_token(&child).expect("child");

    let html = settings_html(&fx).await;

    assert!(html.contains("planner <span class=\"role-tokens\">2 live tokens</span>"));
    let revoked = fx
        .browse(
            ALICE,
            "POST",
            &format!("{}/revoke-tokens", role_uri(&role)),
            None,
        )
        .await;
    assert_eq!(revoked.body["revoked"], 2);
}

fn owner_role_token(fx: &Fixture, role: &Value, name: &str) -> (ApiTokenRecord, String) {
    let id = role["id"].as_str().expect("role id");
    let row = fx
        .db
        .agent_role(fx.alice.as_str(), id)
        .expect("role")
        .expect("exists");
    let (mut record, raw) = new_api_token_record(&fx.alice, name, AuthScope::Work, None);
    record.permissions = Permission::with_required(&row.permissions);
    let stored = fx
        .db
        .create_owner_token_as_role(id, record)
        .expect("issue")
        .expect("role");
    (stored, raw)
}

#[tokio::test]
async fn whoami_should_report_no_drift_for_a_fresh_token() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let (_, raw) = owner_role_token(&fx, &role, "fresh");

    let who = whoami(&fx, &raw).await;

    assert_eq!(who["drift"], json!({"beyond": [], "lacks": []}));

    let patched = fx
        .browse(
            ALICE,
            "PATCH",
            &role_uri(&role),
            Some(json!({"permissions": ["tasks.read", "tasks.edit"]})),
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);
    let (_, after) = owner_role_token(&fx, &role, "after");
    assert_eq!(
        whoami(&fx, &after).await["drift"],
        json!({"beyond": [], "lacks": []})
    );

    let (plain, plain_raw) = new_api_token_record(&fx.alice, "plain", AuthScope::Work, None);
    fx.db.create_api_token(&plain).expect("token");
    assert_eq!(whoami(&fx, &plain_raw).await["drift"], Value::Null);
}

#[tokio::test]
async fn whoami_should_report_drift_after_a_template_change() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let (_, raw) = owner_role_token(&fx, &role, "old");

    let patched = fx
        .browse(
            ALICE,
            "PATCH",
            &role_uri(&role),
            Some(json!({"permissions": ["tasks.read", "tasks.edit"]})),
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);

    let who = whoami(&fx, &raw).await;
    assert_eq!(
        who["drift"],
        json!({"beyond": ["tasks.claim"], "lacks": ["tasks.edit"]})
    );
}

#[tokio::test]
async fn roles_page_should_list_live_tokens_with_issuer_chain() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let lead = issuer(&fx);
    let (mut mid, _) = new_api_token_record(&fx.alice, "mid", AuthScope::Provision, None);
    mid.parent_id = Some(lead.id.clone());
    mid.parent_name = Some(lead.name.clone());
    fx.db.create_api_token(&mid).expect("mid");
    let (mut record, _) = new_api_token_record(&fx.alice, "deep", AuthScope::Work, None);
    record.parent_id = Some(mid.id.clone());
    record.parent_name = Some(mid.name.clone());
    let deep = fx
        .db
        .create_api_token_as_role(
            role["id"].as_str().expect("id"),
            record,
            1,
            chrono::Utc::now().timestamp(),
        )
        .expect("issue")
        .expect("role");
    let (fresh, _) = owner_role_token(&fx, &role, "fresh");
    let (expired, _) = issue_as_at(&fx, &role, "gone", 0);
    assert!(expired.expires_at.is_some());

    let page = settings_html(&fx).await;
    let html = roles_pane(&page);

    assert!(html.contains("<td>deep</td>"), "{html}");
    assert!(html.contains("<td>issuer &gt; mid</td>"));
    assert!(html.contains("<td>agent-1</td>"));
    assert!(html.contains("<td>planner</td>"));
    assert!(html.contains("<td>owner</td>"));
    assert!(html.contains("<strong>Drifted</strong>"));
    assert!(html.contains("beyond: "));
    assert!(!html.contains("<td>gone</td>"));
    assert!(html.contains(&format!(
        "class=\"kf-btn token-revoke\" data-id=\"{}\"",
        deep.id
    )));
    let fresh_row = html.split("<td>fresh</td>").nth(1).expect("fresh row");
    let fresh_row = fresh_row.split("</tr>").next().expect("row end");
    assert!(fresh_row.contains("<td>None</td>"), "{fresh_row}");

    let revoked = fx
        .browse(
            ALICE,
            "DELETE",
            &format!("/api/v1/auth/tokens/{}", fresh.id),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    let after = settings_html(&fx).await;
    assert!(!roles_pane(&after).contains("<td>fresh</td>"));
}

#[test]
fn drift_should_not_be_computed_by_the_auth_middleware() {
    let auth = include_str!("../auth.rs");
    for forbidden in ["Drift", "drift", "agent_role(", "AgentRoleRow"] {
        assert!(
            !auth.contains(forbidden),
            "src/auth.rs mentions {forbidden}, which the request path must not touch"
        );
    }
}

#[tokio::test]
async fn whoami_should_list_a_narrowed_grant_as_lacking() {
    let fx = Fixture::new();
    let role = fx.create(ALICE, "planner").await;
    let id = role["id"].as_str().expect("role id");
    let (mut record, raw) = new_api_token_record(&fx.alice, "narrow", AuthScope::Work, None);
    record.permissions = Permission::with_required(&BTreeSet::from([Permission::TasksRead]));
    fx.db
        .create_owner_token_as_role(id, record)
        .expect("issue")
        .expect("role");

    let who = whoami(&fx, &raw).await;

    assert_eq!(
        who["drift"],
        json!({"beyond": [], "lacks": ["tasks.claim"]})
    );
}
