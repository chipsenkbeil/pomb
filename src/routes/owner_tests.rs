use axum::http::StatusCode;
use serde_json::{Value, json};
use utoipa::OpenApi;

use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call, held_disk};
use super::{ApiDoc, router};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{Actor, AuthScope, Owner, OwnerHolder, TaskClaim};

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    cookie: String,
    tokens: [String; 2],
    column: String,
    other_column: String,
    done_column: String,
}

const ALPHA: usize = 0;
const BETA: usize = 1;

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("owner.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let account = db.create_user("admin").expect("user");
        let session = db.create_session(&account, "").expect("session");
        let board = db.create_board(&account, "Owner").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let other_column = db.create_column(&board, "Doing", None).expect("column");
        let done_column = db.create_column(&board, "Done", None).expect("column");
        db.set_column_done(&done_column, true).expect("done");
        let token = || {
            let (record, raw) = new_api_token_record(&account, "agent", AuthScope::Admin, None);
            db.create_api_token(&record).expect("token");
            raw
        };
        let tokens = [token(), token()];
        let app = router(
            AppState::new(db.clone(), None, passkey_mode())
                .with_disk(held_disk(&db))
                .with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            cookie: format!("session={session}"),
            tokens,
            column,
            other_column,
            done_column,
        }
    }

    async fn browse(&self, method: &str, uri: &str, fence: Option<&str>, body: Value) -> Reply {
        let mut headers = vec![("cookie", self.cookie.as_str())];
        if let Some(fence) = fence {
            headers.push(("if-owner-fence", fence));
        }
        call(&self.app, method, uri, &headers, Some(body)).await
    }

    async fn bearer(
        &self,
        who: usize,
        method: &str,
        uri: &str,
        fence: Option<&str>,
        body: Value,
    ) -> Reply {
        let bearer = format!("Bearer {}", self.tokens[who]);
        let mut headers = vec![("authorization", bearer.as_str())];
        if let Some(fence) = fence {
            headers.push(("if-owner-fence", fence));
        }
        call(&self.app, method, uri, &headers, Some(body)).await
    }

    async fn grab(&self, who: usize, task: &str, name: &str, ttl: Option<u64>) -> Reply {
        let body = json!({"name": name, "ttl_seconds": ttl});
        self.bearer(who, "POST", &owner_uri(task, ""), None, body)
            .await
    }

    async fn renew(&self, who: usize, task: &str, name: &str, fence: &str) -> Reply {
        let body = json!({"name": name, "ttl_seconds": 1200});
        let uri = owner_uri(task, "/renew");
        self.bearer(who, "POST", &uri, Some(fence), body).await
    }

    async fn release(&self, who: usize, task: &str, name: &str, fence: Option<&str>) -> Reply {
        let uri = owner_uri(task, "/release");
        self.bearer(who, "POST", &uri, fence, json!({"name": name}))
            .await
    }

    async fn take_over(&self, task: &str) -> Reply {
        let body = json!({"take_over": true});
        self.browse("POST", &owner_uri(task, ""), None, body).await
    }

    async fn task(&self) -> String {
        let body = json!({"column_id": self.column, "name": "Card"});
        self.create(body).await
    }

    async fn create(&self, body: Value) -> String {
        let made = self.bearer(ALPHA, "POST", "/api/tasks", None, body).await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        made.body["id"].as_str().expect("id").to_string()
    }

    async fn child_of(&self, epic: &str) -> String {
        let child = self.task().await;
        let body = json!({"kind": "parent", "target_id": epic});
        let uri = format!("/api/tasks/{child}/links");
        let linked = self.bearer(ALPHA, "POST", &uri, None, body).await;
        assert_eq!(linked.status, StatusCode::OK, "{}", linked.body);
        child
    }

    async fn finish(&self, task: &str) {
        let body = json!({"column_id": self.done_column, "position": 0});
        let uri = format!("/api/tasks/{task}/move");
        let moved = self.bearer(ALPHA, "POST", &uri, None, body).await;
        assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    }

    async fn detail(&self, task: &str) -> Value {
        let uri = format!("/api/tasks/{task}");
        let read = call(&self.app, "GET", &uri, &[("cookie", &self.cookie)], None).await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.body);
        read.body
    }

    fn lapse(&self, task: &str) {
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

fn owner_uri(task: &str, rest: &str) -> String {
    format!("/api/tasks/{task}/owner{rest}")
}

fn fence_of(reply: &Reply) -> String {
    reply.body["fence"].as_u64().expect("fence").to_string()
}

fn assert_refused(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.body);
    assert_eq!(reply.code.as_deref(), Some(code), "{}", reply.body);
}

#[tokio::test]
async fn owner_grab_should_refuse_a_second_taker_with_409() {
    let fx = Fixture::new();
    let task = fx.task().await;

    let first = fx.grab(ALPHA, &task, "builder", Some(60)).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["live"], true);
    assert_eq!(first.body["holder"]["kind"], "agent");
    assert_eq!(first.body["holder"]["name"], "builder");
    let second = fx.grab(BETA, &task, "builder", Some(60)).await;
    assert_refused(&second, StatusCode::CONFLICT, "owner_held");
    let renamed = fx.grab(ALPHA, &task, "other", Some(60)).await;
    assert_refused(&renamed, StatusCode::CONFLICT, "owner_held");

    let again = fx.grab(ALPHA, &task, "builder", Some(60)).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(fence_of(&again), fence_of(&first));
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
    assert_eq!(detail["owner"]["fence"], first.body["fence"]);
}

#[tokio::test]
async fn owner_grab_should_need_a_name_from_a_token_and_a_sane_lease() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let uri = owner_uri(&task, "");
    let long = "a".repeat(201);
    for body in [
        json!({}),
        json!({"name": "  "}),
        json!({"name": long}),
        json!({"name": "builder", "ttl_seconds": 0}),
        json!({"name": "builder", "ttl_seconds": 86401}),
    ] {
        let refused = fx.bearer(ALPHA, "POST", &uri, None, body.clone()).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{body}");
    }
    let body = json!({"name": "builder", "take_over": true});
    let taken = fx.bearer(ALPHA, "POST", &uri, None, body).await;
    assert_eq!(taken.status, StatusCode::FORBIDDEN, "{}", taken.body);
    assert_eq!(fx.detail(&task).await["owner"], Value::Null);

    let human = fx.browse("POST", &uri, None, json!({})).await;
    assert_eq!(human.status, StatusCode::OK, "{}", human.body);
    assert_eq!(human.body["holder"]["kind"], "human");
    assert_eq!(human.body["lease_expires_at"], Value::Null);
    assert_eq!(human.body["live"], true);
}

#[tokio::test]
async fn owner_take_over_should_replace_the_owner() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let grabbed = fx.grab(ALPHA, &task, "builder", Some(600)).await;
    assert_eq!(grabbed.status, StatusCode::OK, "{}", grabbed.body);

    let uri = owner_uri(&task, "");
    let refused = fx.browse("POST", &uri, None, json!({})).await;
    assert_refused(&refused, StatusCode::CONFLICT, "owner_held");
    let taken = fx.take_over(&task).await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    assert_eq!(taken.body["holder"]["kind"], "human");
    assert_eq!(taken.body["live"], true);
    assert_ne!(fence_of(&taken), fence_of(&grabbed));
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["kind"], "human");

    let renew = fx.renew(ALPHA, &task, "builder", &fence_of(&grabbed)).await;
    assert_refused(&renew, StatusCode::CONFLICT, "stale_fence");
}

#[tokio::test]
async fn owner_lease_lapse_should_free_the_card_and_keep_the_name() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let grabbed = fx.grab(ALPHA, &task, "builder", Some(600)).await;
    assert_eq!(grabbed.status, StatusCode::OK, "{}", grabbed.body);
    fx.lapse(&task);

    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
    assert_eq!(detail["owner"]["live"], false);
    let renew = fx.renew(ALPHA, &task, "builder", &fence_of(&grabbed)).await;
    assert_refused(&renew, StatusCode::CONFLICT, "stale_fence");

    let next = fx.grab(BETA, &task, "reviewer", Some(600)).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.body);
    assert_eq!(next.body["holder"]["name"], "reviewer");
    assert!(next.body["fence"].as_u64() > grabbed.body["fence"].as_u64());
}

#[tokio::test]
async fn owner_release_should_end_the_lease_and_keep_the_name() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let grabbed = fx.grab(ALPHA, &task, "builder", Some(600)).await;
    let fence = fence_of(&grabbed);

    let missing = fx.release(ALPHA, &task, "builder", None).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST, "{}", missing.body);
    let stranger = fx.release(BETA, &task, "builder", Some(&fence)).await;
    assert_refused(&stranger, StatusCode::CONFLICT, "owner_held");

    let renewed = fx.renew(ALPHA, &task, "builder", &fence).await;
    assert_eq!(renewed.status, StatusCode::OK, "{}", renewed.body);
    assert_ne!(
        renewed.body["lease_expires_at"],
        grabbed.body["lease_expires_at"]
    );
    let released = fx.release(ALPHA, &task, "builder", Some(&fence)).await;
    assert_eq!(released.status, StatusCode::OK, "{}", released.body);
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
    assert_eq!(detail["owner"]["live"], false);

    let again = fx.release(ALPHA, &task, "builder", Some(&fence)).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    let free = fx.grab(BETA, &task, "reviewer", None).await;
    assert_eq!(free.status, StatusCode::OK, "{}", free.body);
}

#[tokio::test]
async fn owner_stale_fence_should_be_refused() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let grabbed = fx.grab(ALPHA, &task, "builder", Some(600)).await;
    let old = fence_of(&grabbed);
    let current = fence_of(&fx.take_over(&task).await);
    assert_ne!(old, current);

    let edit = format!("/api/tasks/{task}");
    let name = json!({"name": "Late"});
    let stale_edit = fx.bearer(ALPHA, "PATCH", &edit, Some(&old), name).await;
    assert_refused(&stale_edit, StatusCode::CONFLICT, "stale_fence");
    let moving = json!({"column_id": fx.other_column, "position": 0});
    let move_uri = format!("/api/tasks/{task}/move");
    let stale_move = fx
        .bearer(ALPHA, "POST", &move_uri, Some(&old), moving)
        .await;
    assert_refused(&stale_move, StatusCode::CONFLICT, "stale_fence");
    let comment_uri = format!("/api/tasks/{task}/comments");
    let comment = json!({"body": "late"});
    let stale_comment = fx
        .bearer(ALPHA, "POST", &comment_uri, Some(&old), comment)
        .await;
    assert_refused(&stale_comment, StatusCode::CONFLICT, "stale_fence");
    let detail = fx.detail(&task).await;
    assert_eq!(detail["name"], "Card");
    assert_eq!(detail["column_id"], fx.column);

    let fresh = fx
        .browse("PATCH", &edit, Some(&current), json!({"name": "Fresh"}))
        .await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    let bearer = format!("Bearer {}", fx.tokens[ALPHA]);
    let headers = [
        ("authorization", bearer.as_str()),
        ("if-owner-fence", "1"),
        ("if-claim-fence", "2"),
    ];
    let both = json!({"name": "Both"});
    let clash = call(&fx.app, "PATCH", &edit, &headers, Some(both)).await;
    assert_eq!(clash.status, StatusCode::BAD_REQUEST, "{}", clash.body);
}

#[tokio::test]
async fn owner_should_not_restrict_move_or_edit() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let grabbed = fx.grab(ALPHA, &task, "builder", None).await;
    assert_eq!(grabbed.status, StatusCode::OK, "{}", grabbed.body);

    let edit_uri = format!("/api/tasks/{task}");
    let name = json!({"name": "Edited"});
    let edit = fx.bearer(BETA, "PATCH", &edit_uri, None, name).await;
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.body);
    let move_uri = format!("/api/tasks/{task}/move");
    let moving = json!({"column_id": fx.other_column, "position": 0});
    let moved = fx.bearer(BETA, "POST", &move_uri, None, moving).await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    let comment_uri = format!("/api/tasks/{task}/comments");
    let comment = fx
        .browse("POST", &comment_uri, None, json!({"body": "hi"}))
        .await;
    assert_eq!(comment.status, StatusCode::OK, "{}", comment.body);

    let detail = fx.detail(&task).await;
    assert_eq!(detail["name"], "Edited");
    assert_eq!(detail["column_id"], fx.other_column);
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
    assert_eq!(detail["owner"]["live"], true);
}

#[tokio::test]
async fn owner_claim_routes_should_write_the_owner() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let claim = format!("/api/tasks/{task}/claim");
    let body = json!({"claimant": "builder", "ttl_seconds": 60});
    let claimed = fx.bearer(ALPHA, "POST", &claim, None, body).await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.body);
    let fence = claimed.body["fence"].clone();
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
    assert_eq!(detail["owner"]["live"], true);
    assert_eq!(detail["owner"]["fence"], fence);
    assert_eq!(
        detail["owner"]["lease_expires_at"],
        claimed.body["expires_at"]
    );

    let grab = fx.grab(BETA, &task, "reviewer", None).await;
    assert_refused(&grab, StatusCode::CONFLICT, "owner_held");
    let rival = json!({"claimant": "reviewer", "ttl_seconds": 60});
    let rival = fx.bearer(BETA, "POST", &claim, None, rival).await;
    assert_refused(&rival, StatusCode::CONFLICT, "claim_held");

    let beat_uri = format!("{claim}/heartbeat");
    let beat = json!({"fence": fence, "ttl_seconds": 600});
    let beat = fx.bearer(ALPHA, "POST", &beat_uri, None, beat).await;
    assert_eq!(beat.status, StatusCode::OK, "{}", beat.body);
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["lease_expires_at"], beat.body["expires_at"]);

    let release_uri = format!("{claim}/release");
    let release = json!({"fence": fence});
    let released = fx.bearer(ALPHA, "POST", &release_uri, None, release).await;
    assert_eq!(released.status, StatusCode::OK, "{}", released.body);
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
    assert_eq!(detail["owner"]["live"], false);
    let free = fx.grab(BETA, &task, "reviewer", None).await;
    assert_eq!(free.status, StatusCode::OK, "{}", free.body);
}

#[tokio::test]
async fn owner_migrated_owner_should_not_block_a_claim_or_a_fence() {
    let fx = Fixture::new();
    let task = fx.task().await;
    fx.db
        .apply_task_change(&task, Actor::SYSTEM, |row| {
            row.owner = Some(Owner {
                holder: OwnerHolder::Human {
                    id: "member".to_string(),
                },
                lease_expires_at: None,
                role: None,
                since: None,
            });
            ((), true)
        })
        .expect("change")
        .expect("task");

    let edit = format!("/api/tasks/{task}");
    let name = json!({"name": "Edited"});
    let edited = fx.bearer(BETA, "PATCH", &edit, Some("7"), name).await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let claim = format!("/api/tasks/{task}/claim");
    let body = json!({"claimant": "builder", "ttl_seconds": 60});
    let claimed = fx.bearer(ALPHA, "POST", &claim, None, body).await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.body);
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
}

#[tokio::test]
async fn owner_grab_should_refuse_a_live_claim_from_before_the_owner_field() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let seed = |expires_at: &str| {
        let expires_at = expires_at.to_string();
        fx.db
            .apply_task_change(&task, Actor::SYSTEM, |row| {
                row.owner = None;
                row.claim_fence = 1;
                row.claim = Some(TaskClaim {
                    claimant: "builder".to_string(),
                    role: None,
                    harness: None,
                    model: None,
                    jj_workspace: None,
                    jj_change_id: None,
                    session_id: None,
                    fence: 1,
                    started_at: "2020-01-01T00:00:00+00:00".to_string(),
                    heartbeat_at: "2020-01-01T00:00:00+00:00".to_string(),
                    expires_at,
                });
                ((), true)
            })
            .expect("change")
            .expect("task");
    };

    seed("2999-01-01T00:00:00+00:00");
    let held = fx.grab(BETA, &task, "reviewer", Some(60)).await;
    assert_refused(&held, StatusCode::CONFLICT, "owner_held");
    let renamed = fx.grab(ALPHA, &task, "other", Some(60)).await;
    assert_refused(&renamed, StatusCode::CONFLICT, "owner_held");
    let own = fx.grab(ALPHA, &task, "builder", Some(600)).await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    assert_eq!(fence_of(&own), "1");
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["holder"]["name"], "builder");
    assert_eq!(detail["owner"]["live"], true);

    seed("2020-01-02T00:00:00+00:00");
    let next = fx.grab(BETA, &task, "reviewer", Some(60)).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.body);
    assert_eq!(next.body["holder"]["name"], "reviewer");
}

#[tokio::test]
async fn owner_grab_should_return_the_trimmed_role_and_keep_it_on_a_renew() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let body = json!({"name": "builder", "ttl_seconds": 60, "role": "  runner "});
    let first = fx
        .bearer(ALPHA, "POST", &owner_uri(&task, ""), None, body)
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["role"], "runner");

    let renewed = fx.renew(ALPHA, &task, "builder", &fence_of(&first)).await;
    assert_eq!(renewed.status, StatusCode::OK, "{}", renewed.body);
    assert_eq!(renewed.body["role"], "runner");
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["role"], "runner");

    let body = json!({"name": "builder", "ttl_seconds": 60});
    let again = fx
        .bearer(ALPHA, "POST", &owner_uri(&task, ""), None, body)
        .await;
    assert_eq!(again.body["role"], "runner");

    let body = json!({"name": "builder", "ttl_seconds": 60, "role": "checker"});
    let uri = owner_uri(&task, "/renew");
    let changed = fx
        .bearer(ALPHA, "POST", &uri, Some(&fence_of(&first)), body)
        .await;
    assert_eq!(changed.body["role"], "checker");
}

#[tokio::test]
async fn owner_grab_should_refuse_a_role_over_forty_characters() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let body = |role: String| json!({"name": "builder", "ttl_seconds": 60, "role": role});
    let long = fx
        .bearer(
            ALPHA,
            "POST",
            &owner_uri(&task, ""),
            None,
            body("r".repeat(41)),
        )
        .await;
    assert_eq!(long.status, StatusCode::BAD_REQUEST, "{}", long.body);
    assert!(
        long.body["message"]
            .as_str()
            .is_some_and(|e| e.contains("role"))
    );
    let exact = fx
        .bearer(
            ALPHA,
            "POST",
            &owner_uri(&task, ""),
            None,
            body("r".repeat(40)),
        )
        .await;
    assert_eq!(exact.status, StatusCode::OK, "{}", exact.body);
    let blank = fx
        .bearer(
            ALPHA,
            "POST",
            &owner_uri(&task, ""),
            None,
            body("   ".to_string()),
        )
        .await;
    assert_eq!(blank.body["role"], "r".repeat(40));
}

#[tokio::test]
async fn owner_since_should_change_with_the_holder_and_stay_on_a_renew() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let first = fx.grab(ALPHA, &task, "builder", Some(60)).await;
    let since = first.body["since"].as_str().expect("since").to_string();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&since).is_ok(),
        "{since}"
    );

    let renewed = fx.renew(ALPHA, &task, "builder", &fence_of(&first)).await;
    assert_eq!(renewed.body["since"], since.as_str());
    let again = fx.grab(ALPHA, &task, "builder", Some(60)).await;
    assert_eq!(again.body["since"], since.as_str());
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["since"], since.as_str());

    let taken = fx.take_over(&task).await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    let taken_since = taken.body["since"].as_str().expect("since");
    assert_ne!(taken_since, since);
    assert!(taken.body["role"].is_null());
}

#[tokio::test]
async fn owner_since_should_read_null_for_an_owner_stored_without_it() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let first = fx.grab(ALPHA, &task, "builder", Some(60)).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    fx.db
        .apply_task_change(&task, Actor::SYSTEM, |row| {
            let owner = row.owner.as_mut().expect("owner");
            owner.since = None;
            ((), true)
        })
        .expect("change");
    let detail = fx.detail(&task).await;
    assert!(detail["owner"]["since"].is_null());
    assert!(detail["owner"]["role"].is_null());
}

#[test]
fn owner_openapi_should_document_role_and_since() {
    let spec: Value =
        serde_json::from_str(&ApiDoc::openapi().to_json().expect("json")).expect("spec");
    let schemas = &spec["components"]["schemas"];
    for field in ["role", "since"] {
        let property = &schemas["OwnerView"]["properties"][field];
        assert!(property.is_object(), "OwnerView lacks {field}");
        assert!(property.to_string().contains("null"), "{field}: {property}");
        let required = schemas["OwnerView"]["required"].to_string();
        assert!(!required.contains(&format!("\"{field}\"")), "{required}");
    }
    for schema in ["GrabInput", "RenewInput"] {
        let role = &schemas[schema]["properties"]["role"];
        assert!(role.is_object(), "{schema} lacks role");
        assert!(role["description"].is_string(), "{schema}: {role}");
    }
}

#[tokio::test]
async fn owner_claim_by_the_same_holder_should_keep_the_role_and_since() {
    let fx = Fixture::new();
    let task = fx.task().await;
    let body = json!({"name": "builder", "ttl_seconds": 60, "role": "runner"});
    let grab = fx
        .bearer(ALPHA, "POST", &owner_uri(&task, ""), None, body)
        .await;
    assert_eq!(grab.status, StatusCode::OK, "{}", grab.body);

    let claim = format!("/api/tasks/{task}/claim");
    let body = json!({"claimant": "builder", "ttl_seconds": 60});
    let claimed = fx.bearer(ALPHA, "POST", &claim, None, body).await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.body);
    let detail = fx.detail(&task).await;
    assert_eq!(detail["owner"]["role"], "runner");
    assert_eq!(detail["owner"]["since"], grab.body["since"]);

    let release = json!({"fence": claimed.body["fence"]});
    let uri = format!("{claim}/release");
    fx.bearer(ALPHA, "POST", &uri, None, release).await;
    let body = json!({"claimant": "builder", "ttl_seconds": 60});
    fx.bearer(ALPHA, "POST", &claim, None, body).await;
    let detail = fx.detail(&task).await;
    assert!(detail["owner"]["role"].is_null());
    assert_ne!(detail["owner"]["since"], grab.body["since"]);
}

#[tokio::test]
async fn grabbing_an_epic_with_an_open_child_should_be_a_409_epic_not_claimable() {
    let fx = Fixture::new();
    let epic = fx
        .create(json!({"column_id": fx.column, "name": "Epic", "kind": "epic"}))
        .await;
    let finished = fx.child_of(&epic).await;
    fx.finish(&finished).await;
    fx.child_of(&epic).await;

    let refused = fx.grab(ALPHA, &epic, "builder", Some(60)).await;
    assert_refused(&refused, StatusCode::CONFLICT, "epic_not_claimable");
    assert_eq!(
        refused.body["message"],
        "an epic can be claimed once every child is done, and 1 child is still open"
    );
    let detail = fx.detail(&epic).await;
    assert!(detail["owner"].is_null(), "{detail}");
}

#[tokio::test]
async fn grabbing_an_epic_whose_children_are_done_should_be_granted() {
    let fx = Fixture::new();
    let epic = fx
        .create(json!({"column_id": fx.column, "name": "Epic", "kind": "epic"}))
        .await;
    let child = fx.child_of(&epic).await;
    fx.finish(&child).await;

    let granted = fx.grab(ALPHA, &epic, "builder", Some(60)).await;
    assert_eq!(granted.status, StatusCode::OK, "{}", granted.body);
    assert_eq!(granted.body["holder"]["name"], "builder");
}
