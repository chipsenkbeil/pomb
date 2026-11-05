use axum::http::StatusCode;
use serde_json::{Value, json};

use super::passkey_tests::call;
use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, Actor, AuthScope, SeatSlot};

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    account: AccountId,
    cookie: String,
    board: String,
    column: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("card-face.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let account = db.create_user("admin").expect("user");
        let session = db.create_session(&account, "").expect("session");
        let board = db.create_board(&account, "Face").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let slots = vec![
            SeatSlot {
                name: "code".to_string(),
                role: "reviewer".to_string(),
            },
            SeatSlot {
                name: "verify".to_string(),
                role: "verifier".to_string(),
            },
        ];
        db.set_board_seats(&board, slots).expect("seats");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            account,
            cookie: format!("session={session}"),
            board,
            column,
        }
    }

    fn token(&self, role: Option<&str>) -> String {
        let (record, raw) = new_api_token_record(&self.account, "agent", AuthScope::Work, None);
        match role {
            Some(name) => {
                let role = self
                    .db
                    .list_agent_roles(self.account.as_str())
                    .expect("roles")
                    .into_iter()
                    .find(|role| role.name == name)
                    .expect("standard role");
                self.db
                    .create_owner_token_as_role(&role.id, record)
                    .expect("store")
                    .expect("role of the account");
            }
            None => self.db.create_api_token(&record).expect("token"),
        }
        raw
    }

    async fn browse(&self, method: &str, uri: &str, body: Option<Value>) -> Value {
        let reply = call(
            &self.app,
            method,
            uri,
            &[("cookie", self.cookie.as_str())],
            body,
        )
        .await;
        assert!(reply.status.is_success(), "{uri}: {}", reply.body);
        reply.body
    }

    async fn agent(&self, raw: &str, uri: &str, body: Value) {
        let bearer = format!("Bearer {raw}");
        let reply = call(
            &self.app,
            "POST",
            uri,
            &[("authorization", bearer.as_str())],
            Some(body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
    }

    async fn task(&self) -> String {
        let body = json!({"column_id": self.column, "name": "Card"});
        let bearer = format!("Bearer {}", self.token(None));
        let made = call(
            &self.app,
            "POST",
            "/api/tasks",
            &[("authorization", bearer.as_str())],
            Some(body),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        made.body["id"].as_str().expect("id").to_string()
    }

    async fn modal(&self, task: &str) -> String {
        let html = self
            .browse("GET", &format!("/api/tasks/{task}/modal"), None)
            .await;
        html.as_str().expect("html").to_string()
    }

    async fn face(&self, task: &str) -> String {
        let html = self
            .browse("GET", &format!("/api/tasks/{task}/card"), None)
            .await;
        html.as_str().expect("html").to_string()
    }
}

fn circles(html: &str, class: &str) -> usize {
    html.matches(&format!("class=\"card-circle {class}"))
        .count()
}

#[tokio::test]
async fn card_face_should_show_owner_and_busy_seat_circles() {
    let fx = Fixture::new();
    let task = fx.task().await;
    assert_eq!(circles(&fx.face(&task).await, "card-circle-"), 0);

    fx.browse("POST", &format!("/api/tasks/{task}/owner"), Some(json!({})))
        .await;
    let reviewer = fx.token(None);
    let take = format!("/api/tasks/{task}/seats/take");
    let body = json!({"seat": "code", "name": "rev-bot", "ttl_seconds": 600});
    fx.agent(&reviewer, &take, body).await;

    let html = fx.face(&task).await;
    assert!(html.contains("card-circle-owner\" title=\"admin\" style=\"\">AD</span>"));
    assert_eq!(circles(&html, "card-circle-seat"), 1, "{html}");
    assert!(
        html.contains(
            "card-circle-seat card-circle-agent\" title=\"rev-bot\" style=\"\">RE</span>"
        )
    );

    let verdict = format!("/api/tasks/{task}/seats/verdict");
    let body = json!({"seat": "code", "name": "rev-bot", "verdict": "approved"});
    fx.agent(&reviewer, &verdict, body).await;
    let html = fx.face(&task).await;
    assert_eq!(
        circles(&html, "card-circle-seat"),
        0,
        "an answered seat is not busy"
    );
    assert_eq!(circles(&html, "card-circle-owner"), 1);
}

#[tokio::test]
async fn card_face_should_use_role_initials_and_mark_for_an_agent_owner() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let builder = fx.token(Some("implementer"));
    let grab = format!("/api/tasks/{task}/owner");
    fx.agent(
        &builder,
        &grab,
        json!({"name": "ship-it", "ttl_seconds": 600}),
    )
    .await;

    let html = fx.face(&task).await;
    assert!(
        html.contains(
            "card-circle-owner card-circle-agent\" title=\"ship-it\" style=\"\">IM</span>"
        ),
        "{html}"
    );
}

#[tokio::test]
async fn card_face_should_grey_an_idle_owner() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let grab = format!("/api/tasks/{task}/owner");
    let builder = fx.token(None);
    fx.agent(
        &builder,
        &grab,
        json!({"name": "ship-it", "ttl_seconds": 600}),
    )
    .await;
    assert_eq!(
        circles(&fx.face(&task).await, "card-circle-owner card-circle-idle"),
        0
    );

    fx.db
        .apply_task_change(&task, Actor::SYSTEM, |row| {
            let owner = row.owner.as_mut().expect("owner");
            owner.lease_expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
            ((), true)
        })
        .expect("change")
        .expect("task");

    let html = fx.face(&task).await;
    assert_eq!(
        circles(
            &html,
            "card-circle-owner card-circle-agent card-circle-idle"
        ),
        1,
        "{html}"
    );
}

#[tokio::test]
async fn card_face_should_show_only_the_owner_on_a_done_card() {
    let fx = Fixture::new();
    let done = fx
        .db
        .create_column(&fx.board, "Done", None)
        .expect("column");
    fx.db.set_column_done(&done, true).expect("done");
    let task = fx.task().await;
    fx.browse("POST", &format!("/api/tasks/{task}/owner"), Some(json!({})))
        .await;
    let reviewer = fx.token(None);
    let take = format!("/api/tasks/{task}/seats/take");
    let body = json!({"seat": "code", "name": "rev-bot", "ttl_seconds": 600});
    fx.agent(&reviewer, &take, body).await;
    assert_eq!(circles(&fx.face(&task).await, "card-circle-seat"), 1);

    fx.db.move_task(&task, &done, None, 1.0).expect("move");

    let html = fx.face(&task).await;
    assert_eq!(circles(&html, "card-circle-seat"), 0, "{html}");
    assert_eq!(circles(&html, "card-circle-owner"), 1, "{html}");
}

#[tokio::test]
async fn panel_should_show_owner_and_reviewer_rows() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let html = fx.modal(&task).await;
    assert!(html.contains("No owner"), "{html}");
    assert!(html.contains(r#"data-owner-act="owner-take">Take</button>"#));
    assert_eq!(html.matches("data-seat-row=").count(), 2, "{html}");
    assert_eq!(html.matches("tm-seat-state\">Open<").count(), 2, "{html}");
    assert!(html.contains(r#"data-owner-act="seat-add""#));
    assert!(html.contains(r#"data-owner-act="seat-drop" data-seat="code""#));
    assert!(!html.contains(r#"data-owner-act="release""#));
    assert!(!html.contains("Ask again"));

    fx.browse("POST", &format!("/api/tasks/{task}/owner"), Some(json!({})))
        .await;
    let reviewer = fx.token(None);
    let take = format!("/api/tasks/{task}/seats/take");
    let body = json!({"seat": "code", "name": "rev-bot", "ttl_seconds": 600});
    fx.agent(&reviewer, &take, body).await;

    let html = fx.modal(&task).await;
    assert!(
        html.contains("card-circle-owner\" title=\"admin\" style=\"\">AD</span>"),
        "{html}"
    );
    assert!(
        html.contains(r#"data-owner-act="release" data-fence="1""#),
        "{html}"
    );
    assert!(!html.contains(r#"data-owner-act="owner-take""#));
    assert!(html.contains("card-circle-seat card-circle-agent\" title=\"rev-bot\""));
    assert!(html.contains("tm-seat-state\">Working<"), "{html}");
    assert!(html.contains(
        r#"data-owner-act="seat-take" data-seat="code" data-over="1" data-live-agent="1">Take over"#
    ));
    assert!(!html.contains("Ask again"));

    let verdict = format!("/api/tasks/{task}/seats/verdict");
    let body = json!({"seat": "code", "name": "rev-bot", "verdict": "changes_needed", "revision": "abc123"});
    fx.agent(&reviewer, &verdict, body).await;
    let html = fx.modal(&task).await;
    assert!(html.contains("tm-seat-state\">Changes needed<"), "{html}");
    assert!(html.contains(">abc123</code>"), "{html}");
    assert!(html.contains(r#"data-owner-act="ask-again""#), "{html}");
}

#[tokio::test]
async fn panel_should_offer_a_plain_take_for_a_lapsed_owner() {
    let fx = Fixture::new();
    let task = fx.task().await;
    fx.browse("POST", &format!("/api/tasks/{task}/owner"), Some(json!({})))
        .await;
    fx.db
        .apply_task_change(&task, Actor::SYSTEM, |row| {
            let owner = row.owner.as_mut().expect("owner");
            owner.lease_expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
            ((), true)
        })
        .expect("change")
        .expect("task");

    let html = fx.modal(&task).await;
    assert!(html.contains("Lease lapsed"), "{html}");
    assert!(
        html.contains(r#"data-owner-act="owner-take">Take</button>"#),
        "{html}"
    );
    assert!(!html.contains(r#"data-owner-act="release""#), "{html}");
}
