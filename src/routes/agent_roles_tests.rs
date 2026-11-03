use axum::http::StatusCode;
use serde_json::{Value, json};

use super::passkey_tests::{Reply, call};
use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, AuthScope, BoardRole};

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

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-roles.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let alice = db.create_user("alice").expect("alice");
        let bob = db.create_user("bob").expect("bob");
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
    assert!(
        fx.db
            .revoke_agent_role(fx.alice.as_str(), id, &kept)
            .expect("revoke")
    );
    assert!(
        !fx.db
            .revoke_agent_role(fx.alice.as_str(), id, &kept)
            .expect("revoke")
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
