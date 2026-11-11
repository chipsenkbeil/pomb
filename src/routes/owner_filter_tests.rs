use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call, held_disk};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::AuthScope;
use crate::permissions::Permission;

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    cookie: String,
    board: String,
    column: String,
    role_id: String,
    role_token: String,
    bare_token: String,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("filter.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let account = db.create_user("admin").expect("user");
        db.create_user("ann").expect("ann");
        let session = db.create_session(&account, "").expect("session");
        let board = db.create_board(&account, "Filter").expect("board");
        db.create_swimlane(&board, "Lane").expect("lane");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let role = db
            .create_agent_role(account.as_str(), "builder", BTreeSet::<Permission>::new())
            .expect("role");
        let mut record = new_api_token_record(&account, "builder-1", AuthScope::Admin, None);
        record.0.role_id = Some(role.id.clone());
        db.create_api_token(&record.0).expect("token");
        let (bare, bare_token) = new_api_token_record(&account, "bare", AuthScope::Admin, None);
        db.create_api_token(&bare).expect("token");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode())
                .with_disk(held_disk(&db))
                .with_limits(unthrottled_limits()),
        );
        let fx = Self {
            _dir: dir,
            app,
            cookie: format!("session={session}"),
            board,
            column,
            role_id: role.id,
            role_token: record.1,
            bare_token,
        };
        let added = fx
            .send(
                &format!("/api/boards/{}/members", fx.board),
                "PUT",
                json!({"username": "ann", "role": "member"}),
            )
            .await;
        assert_eq!(added.status, StatusCode::OK, "{}", added.body);
        fx
    }

    async fn send(&self, uri: &str, method: &str, body: Value) -> Reply {
        call(
            &self.app,
            method,
            uri,
            &[("cookie", &self.cookie)],
            Some(body),
        )
        .await
    }

    async fn card(&self, name: &str, grabbed_by: Option<&str>) -> String {
        let body = json!({"column_id": self.column, "name": name});
        let bearer = format!("Bearer {}", self.bare_token);
        let made = call(
            &self.app,
            "POST",
            "/api/tasks",
            &[("authorization", &bearer)],
            Some(body),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        let id = made.body["id"].as_str().expect("id").to_string();
        if let Some(token) = grabbed_by {
            let bearer = format!("Bearer {token}");
            let grabbed = call(
                &self.app,
                "POST",
                &format!("/api/tasks/{id}/owner"),
                &[("authorization", &bearer)],
                Some(json!({"name": "worker", "ttl_seconds": 600})),
            )
            .await;
            assert_eq!(grabbed.status, StatusCode::OK, "{}", grabbed.body);
        }
        id
    }

    async fn page(&self) -> String {
        let uri = format!("/b/{}", self.board);
        let page = call(&self.app, "GET", &uri, &[("cookie", &self.cookie)], None).await;
        assert_eq!(page.status, StatusCode::OK);
        page.body.as_str().expect("html").to_string()
    }
}

fn card_owner(page: &str, id: &str) -> String {
    let at = page
        .find(&format!("data-task-id=\"{id}\""))
        .expect("card on the page");
    let rest = &page[at..];
    let from = rest.find("data-owner=\"").expect("data-owner") + "data-owner=\"".len();
    let len = rest[from..].find('"').expect("closing quote");
    rest[from..from + len].to_string()
}

#[tokio::test]
async fn owner_filter_should_list_me_people_roles_and_no_owner() {
    let fx = Fixture::new().await;
    let page = fx.page().await;
    let me = page
        .find("<option value=\"Me\" data-owner=\"user:")
        .expect("Me");
    let ann = page
        .find("<option value=\"ann\" data-owner=\"user:")
        .expect("ann");
    let role = format!(
        "<option value=\"builder (role)\" data-owner=\"role:{}\">",
        fx.role_id
    );
    let role = page.find(&role).expect("role");
    let none = page
        .find("<option value=\"No owner\" data-owner=\"none\">")
        .expect("No owner");
    assert!(me < ann && ann < role && role < none);
    assert!(page.contains("id=\"f-owner\""));
}

#[tokio::test]
async fn owner_filter_should_match_a_role_to_its_agent_owned_cards() {
    let fx = Fixture::new().await;
    let by_role = fx.card("Built", Some(&fx.role_token)).await;
    let by_bare = fx.card("Bare", Some(&fx.bare_token)).await;
    let page = fx.page().await;
    let key = format!("role:{}", fx.role_id);
    assert_eq!(card_owner(&page, &by_role), key);
    assert!(page.contains(&format!("data-owner=\"{key}\">")));
    assert_eq!(card_owner(&page, &by_bare), "agent:worker");
}

#[tokio::test]
async fn owner_filter_should_match_no_owner_to_free_cards() {
    let fx = Fixture::new().await;
    let free = fx.card("Free", None).await;
    let held = fx.card("Held", Some(&fx.role_token)).await;
    let page = fx.page().await;
    assert_eq!(card_owner(&page, &free), "");
    assert_ne!(card_owner(&page, &held), "");
    assert!(page.contains("<option value=\"No owner\" data-owner=\"none\">"));
}
