use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AuthScope, BoardRole};

const MAX_BODY_BYTES: usize = 1 << 22;

struct Fixture {
    _dir: tempfile::TempDir,
    app: Router,
    board: String,
    column: String,
    cookie: String,
    member_token: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stage.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.create_user("alice").expect("alice");
        let member = db.create_user("bob").expect("bob");
        let (record, member_token) = new_api_token_record(&member, "bob", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let cookie = format!(
            "session={}",
            db.create_session(&owner, "").expect("session")
        );
        let board = db.create_board(&owner, "Stage").expect("board");
        let column = db.create_column(&board, "Doing", None).expect("column");
        db.set_board_member(&board, "bob", BoardRole::Member)
            .expect("join");
        let app = router(AppState::new(db, None, passkey_mode()).with_limits(unthrottled_limits()));
        Self {
            _dir: dir,
            app,
            board,
            column,
            cookie,
            member_token,
        }
    }

    async fn call(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self.app.clone().oneshot(request).await.expect("oneshot");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn patch(&self, member: bool, body: Value) -> (StatusCode, String) {
        let builder = Request::builder()
            .method("PATCH")
            .uri(format!("/api/columns/{}", self.column))
            .header("host", "pomb.test")
            .header("content-type", "application/json");
        let builder = if member {
            builder.header("authorization", format!("Bearer {}", self.member_token))
        } else {
            builder
                .header("cookie", &self.cookie)
                .header("sec-fetch-site", "same-origin")
        };
        self.call(builder.body(Body::from(body.to_string())).expect("request"))
            .await
    }

    async fn get(&self, uri: String) -> (StatusCode, String) {
        let request = Request::builder()
            .uri(uri)
            .header("host", "pomb.test")
            .header("accept", "application/json")
            .header("cookie", &self.cookie)
            .body(Body::empty())
            .expect("request");
        self.call(request).await
    }

    async fn listed_stage(&self) -> Value {
        let (status, text) = self
            .get(format!("/api/boards/{}/columns", self.board))
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let columns: Value = serde_json::from_str(&text).expect("json");
        columns[0]["stage"].clone()
    }
}

#[tokio::test]
async fn should_let_the_owner_set_and_clear_a_column_stage() {
    let fx = Fixture::new();
    assert_eq!(fx.listed_stage().await, Value::Null);

    let (status, text) = fx.patch(false, json!({"stage": "in_progress"})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(fx.listed_stage().await, "in_progress");

    let (status, text) = fx.patch(false, json!({"name": "Working"})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(fx.listed_stage().await, "in_progress");

    let (status, text) = fx.patch(false, json!({"stage": "blocked"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert_eq!(fx.listed_stage().await, "in_progress");

    let (status, text) = fx.patch(true, json!({"stage": "review"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(fx.listed_stage().await, "in_progress");

    let (status, text) = fx.patch(false, json!({"stage": null})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(fx.listed_stage().await, Value::Null);
}

#[tokio::test]
async fn should_offer_the_stage_select_in_the_edit_column_dialog() {
    let fx = Fixture::new();
    let (status, text) = fx.patch(false, json!({"stage": "verify"})).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, page) = fx.get(format!("/b/{}", fx.board)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("id=\"ec-stage\""));
    for stage in ["ready", "in_progress", "review", "verify", "release"] {
        assert!(
            page.contains(&format!("<option value=\"{stage}\">")),
            "{stage}"
        );
    }
    assert!(page.contains("data-stage=\"verify\""));
}
