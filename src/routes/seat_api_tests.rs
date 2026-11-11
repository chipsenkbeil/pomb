use axum::http::StatusCode;
use chrono::Utc;
use serde_json::{Value, json};
use utoipa::OpenApi;

use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call};
use super::{ApiDoc, router};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, Actor, AuthScope, Seat, SeatSlot, SeatVerdict};
use crate::permissions::{Permission, STANDARD_ROLES};

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    account: AccountId,
    cookie: String,
    tokens: [String; 3],
    board: String,
    column: String,
    review_column: String,
}

const OWNER: usize = 0;
const REVIEWER: usize = 1;
const NO_MANAGE: usize = 2;

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("seat-api.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let account = db.create_user("admin").expect("user");
        let session = db.create_session(&account, "").expect("session");
        let board = db.create_board(&account, "Seats").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let review_column = db.create_column(&board, "Review", None).expect("column");
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
        let token = |scope: AuthScope, without: Option<Permission>| {
            let (mut record, raw) = new_api_token_record(&account, "agent", scope, None);
            if let Some(permission) = without {
                record.permissions.remove(&permission);
            }
            db.create_api_token(&record).expect("token");
            raw
        };
        let tokens = [
            token(AuthScope::Work, None),
            token(AuthScope::Work, None),
            token(AuthScope::Admin, Some(Permission::SeatsManage)),
        ];
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            account,
            cookie: format!("session={session}"),
            tokens,
            board,
            column,
            review_column,
        }
    }

    async fn browse(&self, method: &str, uri: &str, body: Value) -> Reply {
        call(
            &self.app,
            method,
            uri,
            &[("cookie", self.cookie.as_str())],
            Some(body),
        )
        .await
    }

    async fn bearer(&self, who: usize, method: &str, uri: &str, body: Value) -> Reply {
        let bearer = format!("Bearer {}", self.tokens[who]);
        call(
            &self.app,
            method,
            uri,
            &[("authorization", bearer.as_str())],
            Some(body),
        )
        .await
    }

    fn role_token(&self, role: &str) -> String {
        let standard = STANDARD_ROLES
            .iter()
            .find(|standard| standard.name == role)
            .expect("standard role");
        let (mut record, raw) = new_api_token_record(&self.account, role, AuthScope::Work, None);
        record.permissions = standard.permissions();
        self.db.create_api_token(&record).expect("token");
        raw
    }

    async fn as_token(&self, raw: &str, uri: &str, body: Value) -> Reply {
        let bearer = format!("Bearer {raw}");
        call(
            &self.app,
            "POST",
            uri,
            &[("authorization", bearer.as_str())],
            Some(body),
        )
        .await
    }

    async fn task(&self) -> String {
        let body = json!({"column_id": self.column, "name": "Card"});
        let made = self.bearer(OWNER, "POST", "/api/tasks", body).await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        made.body["id"].as_str().expect("id").to_string()
    }

    async fn grab(&self, who: usize, task: &str, name: &str) -> Reply {
        let uri = format!("/api/tasks/{task}/owner");
        let body = json!({"name": name, "ttl_seconds": 600});
        self.bearer(who, "POST", &uri, body).await
    }

    async fn take(&self, who: usize, task: &str, seat: &str, name: &str) -> Reply {
        let body = json!({"seat": seat, "name": name, "ttl_seconds": 600});
        self.bearer(who, "POST", &seat_uri(task, "take"), body)
            .await
    }

    async fn answer(
        &self,
        who: usize,
        task: &str,
        seat: &str,
        verdict: &str,
        revision: Option<&str>,
    ) -> Reply {
        let body = json!({
            "seat": seat,
            "name": "reviewer",
            "verdict": verdict,
            "revision": revision,
        });
        self.bearer(who, "POST", &seat_uri(task, "verdict"), body)
            .await
    }

    async fn ask_again(&self, who: usize, task: &str, name: &str) -> Reply {
        let body = json!({"name": name});
        self.bearer(who, "POST", &seat_uri(task, "ask-again"), body)
            .await
    }

    fn seats(&self, task: &str) -> Vec<Seat> {
        self.db
            .account(&self.account)
            .task(task)
            .expect("read")
            .expect("task")
            .row()
            .seats
            .clone()
    }

    fn seat(&self, task: &str, name: &str) -> Seat {
        self.seats(task)
            .into_iter()
            .find(|seat| seat.name == name)
            .expect("seat")
    }

    fn lapse(&self, task: &str, name: &str) {
        self.db
            .apply_task_change(task, Actor::SYSTEM, |row| {
                let seat = row
                    .seats
                    .iter_mut()
                    .find(|seat| seat.name == name)
                    .expect("seat");
                seat.lease_expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
                ((), true)
            })
            .expect("change")
            .expect("task");
    }

    fn lapse_owner(&self, task: &str) {
        self.db
            .apply_task_change(task, Actor::SYSTEM, |row| {
                let owner = row.owner.as_mut().expect("owner");
                owner.lease_expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
                ((), true)
            })
            .expect("change")
            .expect("task");
    }
}

fn seat_uri(task: &str, tail: &str) -> String {
    format!("/api/tasks/{task}/seats/{tail}")
}

fn assert_refused(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.body);
    assert_eq!(reply.code.as_deref(), Some(code), "{}", reply.body);
}

#[tokio::test]
async fn seat_take_second_taker_gets_409() {
    let fx = Fixture::new();
    let task = fx.task().await;

    let first = fx.take(OWNER, &task, "code", "builder").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body[0]["holder"]["name"], "builder");
    assert_eq!(first.body[1]["holder"], Value::Null);
    let second = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_refused(&second, StatusCode::CONFLICT, "seat_held");
    let other = fx.take(REVIEWER, &task, "verify", "reviewer").await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
    let again = fx.take(OWNER, &task, "code", "builder").await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);

    let held = fx.seat(&task, "code");
    assert_eq!(held.holder.expect("holder").label(), "builder");
    let missing = fx.take(OWNER, &task, "nobody", "builder").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
    let unnamed = fx
        .bearer(
            OWNER,
            "POST",
            &seat_uri(&task, "take"),
            json!({"name": "a"}),
        )
        .await;
    assert_eq!(unnamed.status, StatusCode::BAD_REQUEST, "{}", unnamed.body);
    let body = json!({"seat": "code", "name": "a", "take_over": true});
    let forced = fx
        .bearer(REVIEWER, "POST", &seat_uri(&task, "take"), body)
        .await;
    assert_eq!(forced.status, StatusCode::FORBIDDEN, "{}", forced.body);
    let taken = fx
        .browse(
            "POST",
            &seat_uri(&task, "take"),
            json!({"seat": "code", "take_over": true}),
        )
        .await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    assert_eq!(taken.body[0]["holder"]["kind"], "human");
}

#[tokio::test]
async fn seat_verdict_stores_revision_uninterpreted() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let taken = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);

    let stranger = fx
        .answer(OWNER, &task, "code", "approved", Some("abc"))
        .await;
    assert_refused(&stranger, StatusCode::CONFLICT, "seat_not_held");
    let invalid = fx.answer(REVIEWER, &task, "code", "maybe", None).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST, "{}", invalid.body);

    let commit = "9f2c1ab07d - {not parsed}";
    let approved = fx
        .answer(REVIEWER, &task, "code", "approved", Some(commit))
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body[0]["verdict"], "approved");
    assert_eq!(approved.body[0]["revision"], commit);
    let stored = fx.seat(&task, "code");
    assert_eq!(stored.verdict, Some(SeatVerdict::Approved));
    assert_eq!(stored.revision.as_deref(), Some(commit));

    let long = "x".repeat(4097);
    let refused = fx
        .answer(REVIEWER, &task, "code", "approved", Some(&long))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(fx.seat(&task, "code").revision.as_deref(), Some(commit));
}

#[tokio::test]
async fn seat_ask_again_resets_changes_needed_only() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let owned = fx.grab(OWNER, &task, "builder").await;
    assert_eq!(owned.status, StatusCode::OK, "{}", owned.body);
    for seat in ["code", "verify"] {
        let taken = fx.take(REVIEWER, &task, seat, "reviewer").await;
        assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    }
    let changes = fx
        .answer(REVIEWER, &task, "code", "changes_needed", Some("fix it"))
        .await;
    assert_eq!(changes.status, StatusCode::OK, "{}", changes.body);
    let approved = fx
        .answer(REVIEWER, &task, "verify", "approved", Some("abc123"))
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let before = fx.seat(&task, "verify");

    let stranger = fx.ask_again(REVIEWER, &task, "reviewer").await;
    assert_refused(&stranger, StatusCode::CONFLICT, "not_owner");
    assert_eq!(
        fx.seat(&task, "code").verdict,
        Some(SeatVerdict::ChangesNeeded)
    );

    let asked = fx.ask_again(OWNER, &task, "builder").await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);
    let code = fx.seat(&task, "code");
    assert_eq!(code.verdict, None);
    assert_eq!(code.revision, None);
    assert!(code.idle);
    assert_eq!(code.holder.expect("holder").label(), "reviewer");
    assert_eq!(fx.seat(&task, "verify"), before);
}

#[tokio::test]
async fn seat_lease_lapse_frees_seat_keeps_name() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let taken = fx.take(OWNER, &task, "code", "builder").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    let refused = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_refused(&refused, StatusCode::CONFLICT, "seat_held");

    fx.lapse(&task, "code");
    let lapsed = fx.seat(&task, "code");
    assert_eq!(lapsed.holder.as_ref().expect("holder").label(), "builder");
    assert!(!lapsed.idle);
    assert!(lapsed.is_takeable(Utc::now()));
    let late = fx.answer(OWNER, &task, "code", "approved", None).await;
    assert_refused(&late, StatusCode::CONFLICT, "seat_not_held");

    let next = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.body);
    assert_eq!(next.body[0]["holder"]["name"], "reviewer");
}

#[tokio::test]
async fn seat_busy_until_verdict() {
    let fx = Fixture::new();
    let task = fx.task().await;
    assert!(fx.seat(&task, "code").is_takeable(Utc::now()));
    let taken = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    let busy = fx.seat(&task, "code");
    assert!(!busy.idle);
    assert!(!busy.is_takeable(Utc::now()));

    let answered = fx
        .answer(REVIEWER, &task, "code", "changes_needed", None)
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let done = fx.seat(&task, "code");
    assert!(done.idle);
    assert_eq!(done.holder.expect("holder").label(), "reviewer");
    let rival = fx.take(OWNER, &task, "code", "builder").await;
    assert_refused(&rival, StatusCode::CONFLICT, "seat_held");
}

#[tokio::test]
async fn move_into_review_resets_no_seat() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let taken = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    let answered = fx
        .answer(REVIEWER, &task, "code", "changes_needed", Some("again"))
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let before = fx.seats(&task);

    let body = json!({"column_id": fx.review_column, "position": 0});
    let moved = fx
        .bearer(OWNER, "POST", &format!("/api/tasks/{task}/move"), body)
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    assert_eq!(fx.seats(&task), before);
}

#[tokio::test]
async fn seat_drop_without_seats_manage_gets_403() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let uri = format!("/api/tasks/{task}/seats");
    let added = fx
        .bearer(
            NO_MANAGE,
            "POST",
            &uri,
            json!({"name": "security", "role": "reviewer"}),
        )
        .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let worker = fx
        .bearer(
            OWNER,
            "POST",
            &uri,
            json!({"name": "docs", "role": "reviewer"}),
        )
        .await;
    assert_eq!(worker.status, StatusCode::OK, "{}", worker.body);

    for who in [NO_MANAGE, OWNER] {
        let dropped = fx
            .bearer(who, "DELETE", &format!("{uri}?name=security"), json!({}))
            .await;
        assert_eq!(dropped.status, StatusCode::FORBIDDEN, "{}", dropped.body);
        assert_eq!(dropped.body["permission"], "seats.manage");
    }
    assert_eq!(fx.seats(&task).len(), 4);

    let board = format!("/api/boards/{}/seats", fx.board);
    let keep = json!({"seats": [
        {"name": "code", "role": "reviewer"},
        {"name": "verify", "role": "verifier"},
        {"name": "extra", "role": "reviewer"},
    ]});
    let grown = fx.bearer(NO_MANAGE, "PUT", &board, keep).await;
    assert_eq!(grown.status, StatusCode::OK, "{}", grown.body);
    let shrunk = json!({"seats": [{"name": "code", "role": "reviewer"}]});
    let removed = fx.bearer(NO_MANAGE, "PUT", &board, shrunk.clone()).await;
    assert_eq!(removed.status, StatusCode::FORBIDDEN, "{}", removed.body);
    assert_eq!(removed.body["permission"], "seats.manage");

    let dropped = fx
        .browse("DELETE", &format!("{uri}?name=security"), json!({}))
        .await;
    assert_eq!(dropped.status, StatusCode::OK, "{}", dropped.body);
    let removed = fx.browse("PUT", &board, shrunk).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
}

#[tokio::test]
async fn seats_should_let_a_reviewer_role_token_take_a_seat_and_record_a_verdict() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let reviewer = fx.role_token("reviewer");

    let body = json!({"seat": "code", "name": "reviewer", "ttl_seconds": 600});
    let taken = fx.as_token(&reviewer, &seat_uri(&task, "take"), body).await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    let body = json!({"seat": "code", "name": "reviewer", "verdict": "approved"});
    let answered = fx
        .as_token(&reviewer, &seat_uri(&task, "verdict"), body)
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    assert_eq!(fx.seat(&task, "code").verdict, Some(SeatVerdict::Approved));
}

#[tokio::test]
async fn seats_should_refuse_an_implementer_role_token_with_permission_denied() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let implementer = fx.role_token("implementer");

    let take = json!({"seat": "code", "name": "builder"});
    let verdict = json!({"seat": "code", "name": "builder", "verdict": "approved"});
    for (tail, body) in [("take", take), ("verdict", verdict)] {
        let refused = fx
            .as_token(&implementer, &seat_uri(&task, tail), body)
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
        assert_eq!(refused.body["permission"], "seats.take");
    }
    assert_eq!(fx.seat(&task, "code").holder, None);
}

#[tokio::test]
async fn take_task_seat_should_refuse_the_cards_live_owner() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let owned = fx.grab(OWNER, &task, "builder").await;
    assert_eq!(owned.status, StatusCode::OK, "{}", owned.body);

    let same = fx.take(OWNER, &task, "code", "builder").await;
    assert_refused(&same, StatusCode::CONFLICT, "owner_seat");
    let renamed = fx.take(OWNER, &task, "code", "someone else").await;
    assert_refused(&renamed, StatusCode::CONFLICT, "owner_seat");
    assert_eq!(fx.seat(&task, "code").holder, None);

    let other = fx.task().await;
    let uri = format!("/api/tasks/{other}/owner");
    let member = fx.browse("POST", &uri, json!({"ttl_seconds": 600})).await;
    assert_eq!(member.status, StatusCode::OK, "{}", member.body);
    let sat = fx
        .browse("POST", &seat_uri(&other, "take"), json!({"seat": "code"}))
        .await;
    assert_refused(&sat, StatusCode::CONFLICT, "owner_seat");
    assert_eq!(fx.seat(&other, "code").holder, None);
}

#[tokio::test]
async fn take_task_seat_should_let_another_token_take_a_seat_on_an_owned_card() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let owned = fx.grab(OWNER, &task, "builder").await;
    assert_eq!(owned.status, StatusCode::OK, "{}", owned.body);

    let taken = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    assert_eq!(taken.body[0]["holder"]["name"], "reviewer");
    let browser = fx
        .browse("POST", &seat_uri(&task, "take"), json!({"seat": "verify"}))
        .await;
    assert_eq!(browser.status, StatusCode::OK, "{}", browser.body);
}

#[tokio::test]
async fn take_task_seat_should_let_an_owner_whose_lease_lapsed_take_a_seat() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let owned = fx.grab(OWNER, &task, "builder").await;
    assert_eq!(owned.status, StatusCode::OK, "{}", owned.body);
    fx.lapse_owner(&task);

    let taken = fx.take(OWNER, &task, "code", "builder").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
}

#[test]
fn seat_verdict_openapi_should_name_the_wire_values() {
    let spec: Value =
        serde_json::from_str(&ApiDoc::openapi().to_json().expect("json")).expect("spec");
    let description = spec["paths"]["/api/tasks/{id}/seats/verdict"]["post"]["description"]
        .as_str()
        .expect("description");
    for value in ["`approved`", "`changes_needed`"] {
        assert!(description.contains(value), "{description}");
    }
    assert!(!description.contains("Changes needed"), "{description}");
}

#[tokio::test]
async fn ask_again_should_work_for_the_owner_with_tasks_claim_alone() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let implementer = fx.role_token("implementer");
    assert!(
        !STANDARD_ROLES
            .iter()
            .find(|standard| standard.name == "implementer")
            .expect("implementer")
            .permissions()
            .contains(&Permission::TasksEdit)
    );
    let uri = format!("/api/tasks/{task}/owner");
    let body = json!({"name": "builder", "ttl_seconds": 600});
    let owned = fx.as_token(&implementer, &uri, body).await;
    assert_eq!(owned.status, StatusCode::OK, "{}", owned.body);
    let taken = fx.take(REVIEWER, &task, "code", "reviewer").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    let changes = fx
        .answer(REVIEWER, &task, "code", "changes_needed", Some("fix"))
        .await;
    assert_eq!(changes.status, StatusCode::OK, "{}", changes.body);

    let body = json!({"name": "builder"});
    let asked = fx
        .as_token(&implementer, &seat_uri(&task, "ask-again"), body)
        .await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);
    assert_eq!(fx.seat(&task, "code").verdict, None);
}
