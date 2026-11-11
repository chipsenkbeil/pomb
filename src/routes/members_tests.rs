use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call, held_disk};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{AccountAction, Db};
use crate::models::{AccountId, AuthScope, BoardAccess};

/// An instance with an owner who has a board, and two other accounts, each with an API token.
struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    owner_id: AccountId,
    friend_id: AccountId,
    owner: String,
    friend: String,
    stranger: String,
    board: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("members.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.create_user("alice").expect("alice");
        let friend = db.create_user("bob").expect("bob");
        let stranger = db.create_user("carol").expect("carol");
        let board = db.create_board(&owner, "Shared").expect("board");
        db.create_board(&owner, "Other").expect("board");
        let token = |account: &AccountId, scope| {
            let (record, raw) = new_api_token_record(account, "agent", scope, None);
            db.create_api_token(&record).expect("token");
            raw
        };
        let (owner_token, friend_token, stranger_token) = (
            token(&owner, AuthScope::Admin),
            token(&friend, AuthScope::Admin),
            token(&stranger, AuthScope::Admin),
        );
        let app = router(
            AppState::new(db.clone(), None, passkey_mode())
                .with_disk(held_disk(&db))
                .with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            owner_id: owner,
            friend_id: friend,
            owner: owner_token,
            friend: friend_token,
            stranger: stranger_token,
            board,
        }
    }

    async fn send(&self, token: &str, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {token}");
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    fn scoped_token(&self, account: &AccountId) -> String {
        let (mut record, raw) = new_api_token_record(account, "scoped", AuthScope::Admin, None);
        record.boards = BoardAccess::Only(BTreeSet::from(["other-board".to_string()]));
        self.db.create_api_token(&record).expect("token");
        raw
    }

    fn members_uri(&self) -> String {
        format!("/api/boards/{}/members", self.board)
    }

    async fn add(&self, username: &str, role: &str) -> Reply {
        let body = json!({"username": username, "role": role});
        self.send(&self.owner, "PUT", &self.members_uri(), Some(body))
            .await
    }

    async fn roles(&self) -> Vec<(String, String)> {
        let listed = self
            .send(&self.owner, "GET", &self.members_uri(), None)
            .await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
        listed
            .body
            .as_array()
            .expect("array")
            .iter()
            .map(|member| {
                (
                    member["username"].as_str().expect("name").to_string(),
                    member["role"].as_str().expect("role").to_string(),
                )
            })
            .collect()
    }
}

fn pair(name: &str, role: &str) -> (String, String) {
    (name.to_string(), role.to_string())
}

#[tokio::test]
async fn members_api_should_add_list_change_and_remove_members() {
    let fx = Fixture::new();
    assert_eq!(fx.roles().await, [pair("alice", "owner")]);

    let added = fx.add("Bob", "member").await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    assert_eq!(
        (added.body["username"].as_str(), added.body["role"].as_str()),
        (Some("bob"), Some("member"))
    );
    let viewer = fx.add("carol", "viewer").await;
    assert_eq!(viewer.status, StatusCode::OK, "{}", viewer.body);
    assert_eq!(
        fx.roles().await,
        [
            pair("alice", "owner"),
            pair("bob", "member"),
            pair("carol", "viewer")
        ]
    );

    let changed = fx.add("bob", "viewer").await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    assert_eq!(
        fx.roles().await,
        [
            pair("alice", "owner"),
            pair("bob", "viewer"),
            pair("carol", "viewer")
        ]
    );

    let uri = format!("{}?username=BOB", fx.members_uri());
    let removed = fx.send(&fx.owner, "DELETE", &uri, None).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    assert_eq!(
        fx.roles().await,
        [pair("alice", "owner"), pair("carol", "viewer")]
    );
    let again = fx.send(&fx.owner, "DELETE", &uri, None).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.body);
}

#[tokio::test]
async fn board_list_should_include_joined_boards_with_their_role() {
    let fx = Fixture::new();
    let before = fx.send(&fx.friend, "GET", "/api/boards", None).await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    let own = before.body.as_array().expect("array").len();
    assert_eq!(fx.add("bob", "viewer").await.status, StatusCode::OK);

    let listed = fx.send(&fx.friend, "GET", "/api/boards", None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let boards = listed.body.as_array().expect("array");
    assert_eq!(boards.len(), own + 1);
    let shared = boards
        .iter()
        .find(|board| board["id"] == fx.board.as_str())
        .expect("the shared board");
    assert_eq!(shared["role"], "viewer");
    assert!(
        boards
            .iter()
            .filter(|board| board["id"] != fx.board.as_str())
            .all(|board| board["role"] == "owner"),
        "{boards:?}"
    );

    let owner_view = fx.send(&fx.owner, "GET", "/api/boards", None).await;
    let owner_boards = owner_view.body.as_array().expect("array");
    assert!(owner_boards.iter().all(|board| board["role"] == "owner"));
}

#[tokio::test]
async fn members_api_should_refuse_removing_or_re_roling_the_owner() {
    let fx = Fixture::new();
    let uri = format!("{}?username=alice", fx.members_uri());
    let removed = fx.send(&fx.owner, "DELETE", &uri, None).await;
    assert_eq!(removed.status, StatusCode::BAD_REQUEST, "{}", removed.body);
    let demoted = fx.add("alice", "viewer").await;
    assert_eq!(demoted.status, StatusCode::BAD_REQUEST, "{}", demoted.body);
    let promoted = fx.add("bob", "owner").await;
    assert_eq!(
        promoted.status,
        StatusCode::BAD_REQUEST,
        "{}",
        promoted.body
    );
    assert_eq!(fx.roles().await, [pair("alice", "owner")]);
}

#[tokio::test]
async fn members_api_should_answer_not_found_for_a_stranger_and_for_an_unknown_account() {
    let fx = Fixture::new();
    let body = json!({"username": "carol", "role": "member"});
    for method in ["GET", "PUT", "DELETE"] {
        let reply = fx
            .send(&fx.stranger, method, &fx.members_uri(), Some(body.clone()))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{method}: {}",
            reply.body
        );
    }
    assert_eq!(fx.roles().await, [pair("alice", "owner")]);
    let unknown = fx.add("nobody", "member").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    let missing = fx
        .send(&fx.owner, "GET", "/api/boards/missing/members", None)
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
}

#[tokio::test]
async fn members_api_should_refuse_a_token_without_the_configure_permission() {
    let fx = Fixture::new();
    let owner = fx.db.owner().expect("read").expect("admin").account_id();
    let (record, raw) = new_api_token_record(&owner, "worker", AuthScope::Work, None);
    fx.db.create_api_token(&record).expect("token");
    let body = json!({"username": "bob", "role": "member"});
    let put = fx.send(&raw, "PUT", &fx.members_uri(), Some(body)).await;
    assert_eq!(put.status, StatusCode::FORBIDDEN, "{}", put.body);
    let get = fx.send(&raw, "GET", &fx.members_uri(), None).await;
    assert_eq!(get.status, StatusCode::OK, "{}", get.body);
}

#[tokio::test]
async fn members_api_should_not_add_a_suspended_account() {
    let fx = Fixture::new();
    let suspended = fx.db.create_user("dave").expect("dave");
    fx.db
        .apply_account_action(suspended.as_str(), AccountAction::Suspend)
        .expect("suspend");
    let reply = fx.add("dave", "member").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
}

#[tokio::test]
async fn members_api_should_refuse_a_token_assigned_to_other_boards() {
    let fx = Fixture::new();
    let raw = fx.scoped_token(&fx.owner_id);
    let body = json!({"username": "bob", "role": "member"});
    let removal = format!("{}?username=bob", fx.members_uri());
    for (method, uri, body) in [
        ("GET", fx.members_uri(), None),
        ("PUT", fx.members_uri(), Some(body)),
        ("DELETE", removal, None),
    ] {
        let reply = fx.send(&raw, method, &uri, body).await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{method}: {}",
            reply.body
        );
    }
    let listed = fx.send(&raw, "GET", "/api/boards", None).await;
    assert!(listed.body.as_array().expect("array").is_empty());
}

#[tokio::test]
async fn board_list_should_hide_a_joined_board_from_a_token_assigned_to_other_boards() {
    let fx = Fixture::new();
    assert_eq!(fx.add("bob", "member").await.status, StatusCode::OK);
    let raw = fx.scoped_token(&fx.friend_id);
    let listed = fx.send(&raw, "GET", "/api/boards", None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert!(
        listed
            .body
            .as_array()
            .expect("array")
            .iter()
            .all(|board| board["id"] != fx.board.as_str())
    );
}
