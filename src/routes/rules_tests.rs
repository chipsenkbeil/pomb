use std::collections::BTreeMap;

use axum::Router;
use axum::body::{Body as HttpBody, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::held_disk;
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{
    BootstrapToken, ColumnKind, ColumnPatch, Db, NewComment, QuestionGuard, QuestionWrite,
};
use crate::models::{
    AccountId, Actor, ActorKind, AuthScope, ColumnStage, CommentQuestion, QuestionChoice,
};

const MAX_BODY_BYTES: usize = 1 << 22;
const BOOTSTRAP_TOKEN: &str = "boot-secret";

#[derive(Copy, Clone, Debug)]
enum Who {
    Session,
    Token,
    Lead,
    Bootstrap,
    Role(&'static str),
}

enum Body {
    Empty,
    Json(Value),
    Raw(Vec<u8>),
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    owner: AccountId,
    app: Router,
    board: String,
    backlog: String,
    ready: String,
    lane: String,
    cookie: String,
    token: String,
    lead_token: String,
    stages: BTreeMap<&'static str, String>,
    role_tokens: BTreeMap<&'static str, String>,
    role_ids: BTreeMap<&'static str, String>,
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rules.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.create_user("alice").expect("alice");
        let (record, token) = new_api_token_record(&owner, "agent", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let (lead_record, lead_token) =
            new_api_token_record(&owner, "lead", AuthScope::Provision, None);
        db.create_api_token(&lead_record).expect("lead token");
        let cookie = format!(
            "session={}",
            db.create_session(&owner, "").expect("session")
        );
        let board = db.create_board(&owner, "Rules").expect("board");
        let backlog = db.create_column(&board, "Backlog", None).expect("backlog");
        let ready = db.create_column(&board, "Ready", None).expect("ready");
        let lane = db.create_swimlane(&board, "Extra").expect("lane");
        let mut stages = BTreeMap::new();
        let stage_patch = |stage| ColumnPatch {
            stage: Some(Some(stage)),
            ..ColumnPatch::default()
        };
        db.update_column(&ready, &stage_patch(ColumnStage::Ready))
            .expect("stage ready");
        for (key, name, stage) in [
            ("progress", "In Progress", ColumnStage::InProgress),
            ("review", "In Review", ColumnStage::Review),
            ("verify", "Verify", ColumnStage::Verify),
            ("release", "Release", ColumnStage::Release),
        ] {
            let id = db.create_column(&board, name, None).expect(name);
            db.update_column(&id, &stage_patch(stage)).expect(name);
            stages.insert(key, id);
        }
        let done = db
            .create_column_as(&board, "Done", None, ColumnKind::Done)
            .expect("done");
        stages.insert("done", done);
        let blocked = db.create_column(&board, "Blocked", None).expect("blocked");
        let mut config = serde_json::Map::new();
        config.insert("blocked_column_id".to_string(), json!(blocked));
        db.set_board_config(&board, &config).expect("config");
        stages.insert("blocked", blocked);
        stages.insert("ready", ready.clone());
        let mut role_tokens = BTreeMap::new();
        let mut role_ids = BTreeMap::new();
        for role in db.list_agent_roles(owner.as_str()).expect("roles") {
            let name = ["implementer", "reviewer", "verifier", "release", "lead"]
                .into_iter()
                .find(|name| *name == role.name);
            let Some(name) = name else { continue };
            let (record, raw) = new_api_token_record(&owner, name, AuthScope::Admin, None);
            db.create_owner_token_as_role(&role.id, record)
                .expect("issue")
                .expect("role");
            role_tokens.insert(name, raw);
            role_ids.insert(name, role.id);
        }
        let app = router(
            AppState::new(
                db.clone(),
                BootstrapToken::new(BOOTSTRAP_TOKEN),
                passkey_mode(),
            )
            .with_disk(held_disk(&db))
            .with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            db,
            owner,
            app,
            board,
            backlog,
            ready,
            lane,
            cookie,
            token,
            lead_token,
            stages,
            role_tokens,
            role_ids,
        }
    }

    fn issue_role_token(&mut self, key: &'static str, role_id: &str) {
        let (record, raw) = new_api_token_record(&self.owner, key, AuthScope::Admin, None);
        self.db
            .create_owner_token_as_role(role_id, record)
            .expect("issue")
            .expect("role");
        self.role_tokens.insert(key, raw);
    }

    async fn call(&self, who: Who, method: &str, uri: &str, body: Body) -> (StatusCode, String) {
        let (status, bytes) = self.call_bytes(who, method, uri, body).await;
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn call_bytes(
        &self,
        who: Who,
        method: &str,
        uri: &str,
        body: Body,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "pomb.test")
            .header("accept", "application/json");
        builder = match who {
            Who::Token => builder.header("authorization", format!("Bearer {}", self.token)),
            Who::Lead => builder.header("authorization", format!("Bearer {}", self.lead_token)),
            Who::Bootstrap => builder.header("authorization", format!("Bearer {BOOTSTRAP_TOKEN}")),
            Who::Role(name) => builder.header(
                "authorization",
                format!("Bearer {}", self.role_tokens[name]),
            ),
            Who::Session => builder
                .header("cookie", &self.cookie)
                .header("sec-fetch-site", "same-origin"),
        };
        let body = match body {
            Body::Empty => HttpBody::empty(),
            Body::Json(value) => {
                builder = builder.header("content-type", "application/json");
                HttpBody::from(value.to_string())
            }
            Body::Raw(bytes) => {
                builder = builder.header("content-type", "application/octet-stream");
                HttpBody::from(bytes)
            }
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("body");
        (status, bytes.to_vec())
    }

    async fn json(&self, who: Who, method: &str, uri: &str, body: Value) -> (StatusCode, String) {
        self.call(who, method, uri, Body::Json(body)).await
    }

    fn rules_uri(&self) -> String {
        format!("/api/boards/{}/rules", self.board)
    }

    async fn put_rules(&self, rules: Value) -> StatusCode {
        self.json(Who::Session, "PUT", &self.rules_uri(), rules)
            .await
            .0
    }

    async fn require_sections(&self, column: &str, sections: &[&str]) {
        let rules = json!({"columns": {column: {"sections": sections}}});
        assert_eq!(self.put_rules(rules).await, StatusCode::OK);
    }

    async fn create(&self, who: Who, column: &str, extra: Value) -> (StatusCode, String) {
        let mut body = json!({"column_id": column, "name": "Task"});
        for (key, value) in extra.as_object().expect("object") {
            body[key] = value.clone();
        }
        self.json(who, "POST", "/api/tasks", body).await
    }

    async fn task(&self, column: &str, extra: Value) -> String {
        let (status, text) = self.create(Who::Session, column, extra).await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
        parse(&text)["id"].as_str().expect("id").to_string()
    }

    async fn locked_task(&self) -> String {
        self.task(&self.backlog, json!({"labels": ["needs:human"]}))
            .await
    }

    #[must_use]
    fn question(&self) -> Value {
        json!({"question": {
            "text": "Ship it?",
            "choices": [
                {"label": "Ship", "column_id": self.stages["done"]},
                {"label": "Review", "column_id": self.stages["review"]},
            ],
            "recommended": 2,
            "why": "Because",
        }})
    }
}

mod role_moves {
    use super::*;

    async fn move_to(fx: &Fixture, who: Who, task: &str, stage: &str) -> (StatusCode, String) {
        let uri = format!("/api/tasks/{task}/move");
        fx.json(who, "POST", &uri, json!({"column_id": fx.stages[stage]}))
            .await
    }

    #[tokio::test]
    async fn forbidden_move_should_name_the_rule() {
        let fx = Fixture::new();
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "verify").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        let body = parse(&text);
        assert_eq!(body["code"], "role_move_forbidden", "{text}");
        let message = body["message"].as_str().expect("message");
        for named in ["implementer", "In Progress", "Verify"] {
            assert!(message.contains(named), "{message}");
        }
    }

    #[tokio::test]
    async fn implementer_should_not_move_to_done() {
        let fx = Fixture::new();
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "done").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "review").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn verifier_should_move_verify_to_release() {
        let fx = Fixture::new();
        let task = fx.task(&fx.stages["verify"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("verifier"), &task, "release").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let other = fx.task(&fx.stages["verify"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("verifier"), &other, "done").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    }

    #[tokio::test]
    async fn standard_moves_should_follow_a_renamed_standard_role() {
        let mut fx = Fixture::new();
        let reviewer = fx.role_ids["reviewer"].clone();
        fx.db
            .update_agent_role(fx.owner.as_str(), &reviewer, Some("checker"), None)
            .expect("rename")
            .expect("role");
        fx.issue_role_token("checker", &reviewer);
        let task = fx.task(&fx.stages["review"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("checker"), &task, "done").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "role_move_forbidden", "{text}");
        let (status, text) = move_to(&fx, Who::Role("checker"), &task, "verify").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn standard_moves_should_ignore_a_custom_role_with_a_standard_name() {
        let mut fx = Fixture::new();
        let standard = fx.role_ids["implementer"].clone();
        let held = fx
            .db
            .agent_role(fx.owner.as_str(), &standard)
            .expect("read")
            .expect("role")
            .permissions;
        fx.db
            .update_agent_role(fx.owner.as_str(), &standard, Some("worker"), None)
            .expect("rename")
            .expect("role");
        let custom = fx
            .db
            .create_agent_role(fx.owner.as_str(), "implementer", held)
            .expect("custom");
        fx.issue_role_token("custom", &custom.id);
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("custom"), &task, "done").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn roleless_token_should_not_be_limited() {
        let fx = Fixture::new();
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Token, &task, "done").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let back = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Session, &back, "done").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let lead = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("lead"), &lead, "done").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn implementer_should_not_reach_done_through_ready() {
        let fx = Fixture::new();
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "ready").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "done").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "role_move_forbidden", "{text}");
    }

    #[tokio::test]
    async fn implementer_should_not_reach_done_by_answering_a_question() {
        let fx = Fixture::new();
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let uri = format!("/api/tasks/{task}/comments");
        let (status, text) = fx
            .json(Who::Role("implementer"), "POST", &uri, fx.question())
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let comment = parse(&text)["id"].as_str().expect("id").to_string();
        let uri = format!("/api/tasks/{task}/comments/{comment}/answer");
        let ship = json!({"choice": 1, "quote": "ship it"});
        let (status, text) = fx.json(Who::Role("implementer"), "POST", &uri, ship).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "role_move_forbidden", "{text}");
    }

    #[tokio::test]
    async fn bulk_move_should_refuse_the_group_when_one_move_is_forbidden() {
        let fx = Fixture::new();
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
        let body = json!({"action": "move", "task_ids": [task], "column_id": fx.stages["done"]});
        let (status, text) = fx.json(Who::Role("implementer"), "POST", &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "role_move_forbidden", "{text}");
    }

    #[tokio::test]
    async fn stored_moves_should_replace_the_standard_ones() {
        let fx = Fixture::new();
        let implementer = fx.role_ids["implementer"].clone();
        let rules = json!({"role_moves": {(implementer.clone()): {
            (fx.stages["progress"].clone()): [fx.stages["done"].clone()],
        }}});
        assert_eq!(fx.put_rules(rules.clone()).await, StatusCode::OK);
        let (status, text) = fx
            .call(Who::Session, "GET", &fx.rules_uri(), Body::Empty)
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(parse(&text)["role_moves"], rules["role_moves"]);
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "review").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "done").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn empty_stored_moves_should_limit_no_role() {
        let fx = Fixture::new();
        assert_eq!(
            fx.put_rules(json!({"role_moves": {}})).await,
            StatusCode::OK
        );
        let task = fx.task(&fx.stages["progress"], json!({})).await;
        let (status, text) = move_to(&fx, Who::Role("implementer"), &task, "done").await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn put_should_refuse_moves_for_an_unknown_role_or_column() {
        let fx = Fixture::new();
        let progress = fx.stages["progress"].clone();
        let implementer = fx.role_ids["implementer"].clone();
        let unknown_role = json!({"role_moves": {"nope": {progress.clone(): []}}});
        assert_eq!(fx.put_rules(unknown_role).await, StatusCode::BAD_REQUEST);
        let unknown_column = json!({"role_moves": {implementer: {progress: ["nope"]}}});
        assert_eq!(fx.put_rules(unknown_column).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn token_should_not_edit_the_moves() {
        let fx = Fixture::new();
        let status = fx
            .json(
                Who::Role("lead"),
                "PUT",
                &fx.rules_uri(),
                json!({"role_moves": {}}),
            )
            .await
            .0;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn settings_page_should_list_the_role_moves() {
        let fx = Fixture::new();
        let uri = format!("/b/{}/settings/rules", fx.board);
        let (status, text) = fx.call(Who::Session, "GET", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains("Role moves"), "{text}");
        assert!(text.contains("Limit moves out of In Progress to"), "{text}");
    }
}

const SECTION: &str = "## Done when\n- it ships\n";

const ACCOUNT_BURST_REQUESTS: usize = 1000;
mod rules_api {
    use super::*;

    #[tokio::test]
    async fn get_should_return_the_defaults_for_a_board_without_rules() {
        let fx = Fixture::new();
        let (status, text) = fx
            .call(Who::Token, "GET", &fx.rules_uri(), Body::Empty)
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(
            parse(&text),
            json!({"locked_labels": ["needs:human"], "columns": {}})
        );
    }

    #[tokio::test]
    async fn get_should_return_what_put_stored() {
        let fx = Fixture::new();
        let owner = json!({"key": "owner", "label": "Owner", "kind": "text"});
        let uri = format!("/api/boards/{}/fields", fx.board);
        let (status, text) = fx.json(Who::Session, "POST", &uri, owner).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let rules = json!({
            "locked_labels": ["hold"],
            "columns": {fx.ready.clone(): {
                "sections": ["Done when"],
                "outcomes_min": 1,
                "outcomes_max": 5,
                "outcomes_locked": false,
                "fields": ["owner"],
            }},
        });
        let (status, text) = fx
            .json(Who::Session, "PUT", &fx.rules_uri(), rules.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(parse(&text), rules);
        for who in [Who::Token, Who::Session] {
            let (status, text) = fx.call(who, "GET", &fx.rules_uri(), Body::Empty).await;
            assert_eq!(status, StatusCode::OK, "{who:?}: {text}");
            assert_eq!(parse(&text), rules, "{who:?}");
        }
    }

    #[tokio::test]
    async fn put_should_be_refused_for_an_admin_token_and_leave_the_rules_alone() {
        let fx = Fixture::new();
        let rules = json!({"locked_labels": [], "columns": {}});
        let (status, text) = fx.json(Who::Token, "PUT", &fx.rules_uri(), rules).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "session_required");
        let (_, text) = fx
            .call(Who::Token, "GET", &fx.rules_uri(), Body::Empty)
            .await;
        assert_eq!(parse(&text)["locked_labels"], json!(["needs:human"]));
    }

    #[tokio::test]
    async fn put_should_reject_a_bad_rule() {
        let fx = Fixture::new();
        let ready = fx.ready.clone();
        let too_many: Vec<String> = (0..21).map(|n| format!("label {n}")).collect();
        let cases = [
            json!({"columns": {"nope": {"sections": ["A"]}}}),
            json!({"columns": {ready.clone(): {"fields": ["ghost"]}}}),
            json!({"columns": {ready.clone(): {"outcomes_min": 3, "outcomes_max": 2}}}),
            json!({"columns": {ready.clone(): {"sections": too_many}}}),
            json!({"locked_labels": ["x".repeat(101)]}),
            json!({"locked_labels": [], "surprise": true}),
            json!({"columns": {ready.clone(): {"surprise": true}}}),
        ];
        for rules in cases {
            let (status, text) = fx
                .json(Who::Session, "PUT", &fx.rules_uri(), rules.clone())
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{rules}: {text}");
        }
    }

    #[tokio::test]
    async fn put_should_trim_dedupe_and_drop_an_empty_rule() {
        let fx = Fixture::new();
        let rules = json!({
            "locked_labels": [" hold ", "hold", ""],
            "columns": {
                fx.ready.clone(): {"sections": [" A ", "A"]},
                fx.backlog.clone(): {},
            },
        });
        let (status, text) = fx.json(Who::Session, "PUT", &fx.rules_uri(), rules).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(
            parse(&text),
            json!({
                "locked_labels": ["hold"],
                "columns": {fx.ready.clone(): {
                    "sections": ["A"],
                    "outcomes_min": null,
                    "outcomes_max": null,
                    "outcomes_locked": false,
                    "fields": [],
                }},
            })
        );
    }

    #[tokio::test]
    async fn settings_page_should_list_the_columns() {
        let fx = Fixture::new();
        let uri = format!("/b/{}/settings/rules", fx.board);
        let (status, text) = fx.call(Who::Session, "GET", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains("Column rules"), "{text}");
        assert!(
            text.contains(&format!("data-column-id=\"{}\"", fx.ready)),
            "{text}"
        );
    }
}

mod entry {
    use super::*;

    #[tokio::test]
    async fn create_should_refuse_a_task_missing_a_section_and_name_it() {
        let fx = Fixture::new();
        fx.require_sections(&fx.ready, &["Done when"]).await;
        let (status, text) = fx.create(Who::Session, &fx.ready, json!({})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("Done when"), "{text}");
        let (status, text) = fx.create(Who::Token, &fx.ready, json!({})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert_eq!(parse(&text)["code"], "column_rule");
        assert!(
            parse(&text)["message"]
                .as_str()
                .unwrap()
                .contains("Done when")
        );
    }

    #[tokio::test]
    async fn create_should_accept_a_task_that_meets_the_rule() {
        let fx = Fixture::new();
        fx.require_sections(&fx.ready, &["Done when"]).await;
        let (status, text) = fx
            .create(Who::Token, &fx.ready, json!({"description": SECTION}))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
        let (status, text) = fx.create(Who::Token, &fx.backlog, json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
    }

    #[tokio::test]
    async fn move_should_refuse_a_task_missing_a_section_until_it_has_one() {
        let fx = Fixture::new();
        fx.require_sections(&fx.ready, &["Done when"]).await;
        let id = fx.task(&fx.backlog, json!({})).await;
        let uri = format!("/api/tasks/{id}/move");
        let to_ready = json!({"column_id": fx.ready, "position": 0});
        for who in [Who::Session, Who::Token] {
            let (status, text) = fx.json(who, "POST", &uri, to_ready.clone()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{who:?}: {text}");
            assert!(text.contains("Done when"), "{who:?}: {text}");
        }
        let edit = json!({"description": SECTION});
        let (status, text) = fx
            .json(Who::Session, "PATCH", &format!("/api/tasks/{id}"), edit)
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = fx.json(Who::Token, "POST", &uri, to_ready).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn bulk_move_should_refuse_the_group_when_one_task_breaks_the_entry_rule() {
        let fx = Fixture::new();
        fx.require_sections(&fx.ready, &["Done when"]).await;
        let ok = fx.task(&fx.backlog, json!({"description": SECTION})).await;
        let bad = fx.task(&fx.backlog, json!({})).await;
        let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
        let body = json!({"task_ids": [ok, bad], "action": "move", "column_id": fx.ready});
        for who in [Who::Session, Who::Token] {
            let (status, text) = fx.json(who, "POST", &uri, body.clone()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{who:?}: {text}");
            assert!(text.contains("Done when"), "{who:?}: {text}");
        }
        let (status, text) = fx
            .json(
                Who::Token,
                "POST",
                &uri,
                json!({"task_ids": [ok], "action": "move", "column_id": fx.ready}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn move_within_a_column_should_not_check_the_entry_rule() {
        let fx = Fixture::new();
        let id = fx.task(&fx.ready, json!({})).await;
        fx.require_sections(&fx.ready, &["Done when"]).await;
        let body = json!({"column_id": fx.ready, "position": 0});
        let (status, text) = fx
            .json(Who::Session, "POST", &format!("/api/tasks/{id}/move"), body)
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn edit_should_refuse_to_break_the_rule_of_the_column_the_task_sits_in() {
        let fx = Fixture::new();
        fx.require_sections(&fx.ready, &["Done when"]).await;
        let id = fx.task(&fx.ready, json!({"description": SECTION})).await;
        let uri = format!("/api/tasks/{id}");
        let (status, text) = fx
            .json(Who::Session, "PATCH", &uri, json!({"description": "gone"}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("Done when"), "{text}");
        let (status, text) = fx
            .json(Who::Token, "PATCH", &uri, json!({"name": "Renamed"}))
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn edit_should_allow_a_task_that_already_broke_a_new_rule() {
        let fx = Fixture::new();
        let id = fx.task(&fx.ready, json!({})).await;
        fx.require_sections(&fx.ready, &["Done when"]).await;
        let (status, text) = fx
            .json(
                Who::Session,
                "PATCH",
                &format!("/api/tasks/{id}"),
                json!({"name": "Renamed"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn outcome_range_should_refuse_entry_below_the_minimum() {
        let fx = Fixture::new();
        let rules = json!({"columns": {fx.ready.clone(): {"outcomes_min": 1}}});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);
        let id = fx.task(&fx.backlog, json!({})).await;
        let to_ready = json!({"column_id": fx.ready, "position": 0});
        let move_uri = format!("/api/tasks/{id}/move");
        let (status, text) = fx
            .json(Who::Session, "POST", &move_uri, to_ready.clone())
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("at least 1 outcomes"), "{text}");
        let sub = json!({"name": "Ship it"});
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &format!("/api/tasks/{id}/subtasks"),
                sub,
            )
            .await;
        assert!(status.is_success(), "{text}");
        let (status, text) = fx.json(Who::Session, "POST", &move_uri, to_ready).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
}

mod locked {
    use super::*;

    struct Ids {
        task: String,
        other: String,
        entry: String,
        link: String,
        field: String,
        source: String,
    }

    async fn seed(fx: &Fixture) -> Ids {
        let task = fx.locked_task().await;
        let other = fx.task(&fx.backlog, json!({})).await;
        let manual = json!({"task_id": task, "date": "2020-01-02", "from": "09:00", "to": "09:30"});
        let (status, text) = fx
            .json(Who::Session, "POST", "/api/time/manual", manual)
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let entry = parse(&text)["id"].as_str().expect("entry id").to_string();
        let link = json!({"kind": "related", "target_id": other});
        let uri = format!("/api/tasks/{task}/links");
        let (status, text) = fx.json(Who::Session, "POST", &uri, link).await;
        assert!(status.is_success(), "{text}");
        let link = parse(&text)["id"].as_str().expect("link id").to_string();
        let key = format!("k{}", &task.replace('-', "")[..8]);
        let field = json!({"key": key, "label": "K", "kind": "multiline"});
        let uri = format!("/api/boards/{}/fields", fx.board);
        let (status, text) = fx.json(Who::Session, "POST", &uri, field).await;
        assert!(status.is_success(), "{text}");
        let field = parse(&text)["id"].as_str().expect("field id").to_string();
        let started = json!({"task_id": task, "mode": "stopwatch"});
        let (status, text) = fx
            .json(Who::Session, "POST", "/api/timer/start", started)
            .await;
        assert!(status.is_success(), "{text}");
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                "/api/boards",
                json!({"name": "Source"}),
            )
            .await;
        assert!(status.is_success(), "{text}");
        let source = parse(&text)["id"].as_str().expect("board id").to_string();
        Ids {
            task,
            other,
            entry,
            link,
            field,
            source,
        }
    }

    fn cases(fx: &Fixture, ids: &Ids) -> Vec<(&'static str, String, Body)> {
        let task = format!("/api/tasks/{}", ids.task);
        let range = json!({"date": "2020-01-02", "from": "10:00", "to": "10:30"});
        let mut update = range.clone();
        update["task_id"] = json!(ids.task);
        vec![
            (
                "PATCH",
                task.clone(),
                Body::Json(json!({"name": "Renamed"})),
            ),
            (
                "POST",
                format!("{task}/move"),
                Body::Json(json!({"column_id": fx.backlog, "position": 0})),
            ),
            ("DELETE", task.clone(), Body::Empty),
            (
                "POST",
                format!("/api/boards/{}/tasks/bulk", fx.board),
                Body::Json(json!({"task_ids": [ids.task], "action": "unassign_members"})),
            ),
            (
                "POST",
                format!("{task}/watch"),
                Body::Json(json!({"watched": true})),
            ),
            (
                "POST",
                format!("{task}/checklist/0"),
                Body::Json(json!({"checked": true, "description_hash": "x"})),
            ),
            (
                "POST",
                format!("{task}/time"),
                Body::Json(json!({"minutes": 5})),
            ),
            (
                "POST",
                format!("{task}/subtasks"),
                Body::Json(json!({"name": "S"})),
            ),
            (
                "PUT",
                format!("{task}/subtasks/order"),
                Body::Json(json!({"order": []})),
            ),
            (
                "PATCH",
                format!("{task}/subtasks/nope"),
                Body::Json(json!({"done": true})),
            ),
            ("DELETE", format!("{task}/subtasks/nope"), Body::Empty),
            ("DELETE", format!("{task}/subtasks"), Body::Empty),
            (
                "POST",
                format!("{task}/subtasks/from-template"),
                Body::Json(json!({"template_id": "nope"})),
            ),
            (
                "POST",
                format!("{task}/subtasks/nope/convert"),
                Body::Json(json!({})),
            ),
            (
                "POST",
                format!("{task}/comments"),
                Body::Json(json!({"body": "hi", "kind": "agent"})),
            ),
            (
                "PATCH",
                format!("{task}/comments/nope"),
                Body::Json(json!({"body": "hi"})),
            ),
            ("DELETE", format!("{task}/comments/nope"), Body::Empty),
            (
                "POST",
                format!("{task}/attachments?name=a.txt"),
                Body::Raw(b"data".to_vec()),
            ),
            (
                "PATCH",
                format!("{task}/attachments/nope"),
                Body::Json(json!({"name": "b.txt"})),
            ),
            ("DELETE", format!("{task}/attachments/nope"), Body::Empty),
            (
                "POST",
                format!("{task}/links"),
                Body::Json(json!({"kind": "related", "target_id": ids.other})),
            ),
            ("DELETE", format!("{task}/links/nope"), Body::Empty),
            ("DELETE", format!("/api/tasks/{}", ids.other), Body::Empty),
            (
                "POST",
                format!("/api/tasks/{}/links", ids.other),
                Body::Json(json!({"kind": "blocks", "target_id": ids.task})),
            ),
            (
                "POST",
                format!("/api/tasks/{}/links", ids.other),
                Body::Json(json!({"kind": "child", "target_id": ids.task})),
            ),
            (
                "PUT",
                format!("/api/tasks/{}/umbrella", ids.other),
                Body::Json(json!({"target_id": ids.task})),
            ),
            (
                "POST",
                "/api/timer/start".to_string(),
                Body::Json(json!({"mode": "stopwatch"})),
            ),
            (
                "POST",
                "/api/timer/start".to_string(),
                Body::Json(json!({"task_id": ids.other, "mode": "stopwatch"})),
            ),
            (
                "DELETE",
                format!("/api/tasks/{}/links/{}", ids.task, ids.link),
                Body::Empty,
            ),
            (
                "POST",
                format!("{task}/claim/heartbeat"),
                Body::Json(json!({"fence": 1, "ttl_seconds": 60})),
            ),
            (
                "POST",
                format!("{task}/claim/release"),
                Body::Json(json!({"fence": 1})),
            ),
            ("POST", "/api/timer/stop".to_string(), Body::Json(json!({}))),
            ("DELETE", format!("/api/fields/{}", ids.field), Body::Empty),
            (
                "PATCH",
                format!("/api/fields/{}", ids.field),
                Body::Json(json!({"kind": "multiselect"})),
            ),
            (
                "DELETE",
                format!("/api/boards/{}/colors/nope", fx.board),
                Body::Empty,
            ),
            (
                "POST",
                format!("/api/boards/{}/colors/copy-from", fx.board),
                Body::Json(json!({"source_board_id": ids.source})),
            ),
            (
                "PUT",
                format!("{task}/umbrella"),
                Body::Json(json!({"target_id": ids.other})),
            ),
            (
                "POST",
                format!("{task}/claim"),
                Body::Json(json!({"claimant": "agent", "ttl_seconds": 60})),
            ),
            (
                "POST",
                "/api/time/manual".to_string(),
                Body::Json(json!({
                    "task_id": ids.task, "date": "2020-01-03", "from": "09:00", "to": "09:30"
                })),
            ),
            (
                "PUT",
                format!("/api/time/entries/{}", ids.entry),
                Body::Json(update),
            ),
            (
                "DELETE",
                format!("/api/time/entries/{}", ids.entry),
                Body::Empty,
            ),
            (
                "POST",
                "/api/timer/start".to_string(),
                Body::Json(json!({"task_id": ids.task, "mode": "stopwatch"})),
            ),
            (
                "POST",
                "/api/timer/retarget".to_string(),
                Body::Json(json!({"task_id": ids.task})),
            ),
            (
                "POST",
                format!("/api/boards/{}/swimlanes/merge", fx.board),
                Body::Empty,
            ),
            ("DELETE", format!("/api/swimlanes/{}", fx.lane), Body::Empty),
            ("DELETE", format!("/api/boards/{}", fx.board), Body::Empty),
        ]
    }

    fn copy(body: &Body) -> Body {
        match body {
            Body::Empty => Body::Empty,
            Body::Json(value) => Body::Json(value.clone()),
            Body::Raw(bytes) => Body::Raw(bytes.clone()),
        }
    }

    #[tokio::test]
    async fn token_should_be_refused_every_write_to_a_locked_task_and_the_session_not() {
        let fx = Fixture::new();
        let count = cases(&fx, &seed(&fx).await).len();
        for index in 0..count {
            let ids = seed(&fx).await;
            let (method, uri, body) = cases(&fx, &ids).swap_remove(index);
            let (status, text) = fx.call(Who::Token, method, &uri, copy(&body)).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "token {method} {uri}: {text}"
            );
            assert_eq!(
                parse(&text)["code"],
                "card_locked",
                "token {method} {uri}: {text}"
            );

            let ids = seed(&fx).await;
            let (method, uri, body) = cases(&fx, &ids).swap_remove(index);
            let (status, text) = fx.call(Who::Session, method, &uri, body).await;
            assert!(
                status != StatusCode::FORBIDDEN && !status.is_server_error(),
                "session {method} {uri}: {status} {text}"
            );
        }
    }

    #[tokio::test]
    async fn session_should_not_be_rate_limited_by_a_burst_beyond_the_account_bucket() {
        let fx = Fixture::new();
        let uri = fx.rules_uri();
        for _ in 0..ACCOUNT_BURST_REQUESTS {
            let (status, text) = fx.call(Who::Session, "GET", &uri, Body::Empty).await;
            assert_eq!(status, StatusCode::OK, "{text}");
        }
    }

    #[tokio::test]
    async fn token_should_link_blocked_by_a_locked_task() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        for kind in ["blocked_by", "related"] {
            let other = fx.task(&fx.backlog, json!({})).await;
            let uri = format!("/api/tasks/{other}/links");
            let link = json!({"kind": kind, "target_id": locked});
            let (status, text) = fx.json(Who::Token, "POST", &uri, link).await;
            assert_eq!(status, StatusCode::OK, "{kind}: {text}");
        }
    }

    #[tokio::test]
    async fn token_should_still_be_refused_a_link_that_changes_the_locked_card() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        for kind in ["blocks", "parent", "child"] {
            let other = fx.task(&fx.backlog, json!({})).await;
            let uri = format!("/api/tasks/{other}/links");
            let link = json!({"kind": kind, "target_id": locked});
            let (status, text) = fx.json(Who::Token, "POST", &uri, link).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{kind}: {text}");
            assert_eq!(parse(&text)["code"], "card_locked", "{kind}: {text}");
        }
        let other = fx.task(&fx.backlog, json!({})).await;
        for kind in ["blocked_by", "blocks", "related", "parent", "child"] {
            let uri = format!("/api/tasks/{locked}/links");
            let link = json!({"kind": kind, "target_id": other});
            let (status, text) = fx.json(Who::Token, "POST", &uri, link).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{kind}: {text}");
            assert_eq!(parse(&text)["code"], "card_locked", "{kind}: {text}");
        }
    }

    async fn link_to(fx: &Fixture, kind: &str, locked: &str) -> (String, String) {
        let holder = if kind == "child" {
            fx.task(&fx.backlog, json!({"kind": "epic"})).await
        } else {
            fx.task(&fx.backlog, json!({})).await
        };
        let uri = format!("/api/tasks/{holder}/links");
        let link = json!({"kind": kind, "target_id": locked});
        let (status, text) = fx.json(Who::Session, "POST", &uri, link).await;
        assert_eq!(status, StatusCode::OK, "{kind}: {text}");
        let link = parse(&text)["id"].as_str().expect("link id").to_string();
        (holder, link)
    }

    #[tokio::test]
    async fn token_should_delete_a_blocked_by_link_to_a_locked_task() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        let (holder, link) = link_to(&fx, "blocked_by", &locked).await;
        let uri = format!("/api/tasks/{holder}/links/{link}");
        let (status, text) = fx.call(Who::Token, "DELETE", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn token_should_delete_a_related_link_to_a_locked_task() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        let (holder, link) = link_to(&fx, "related", &locked).await;
        let uri = format!("/api/tasks/{holder}/links/{link}");
        let (status, text) = fx.call(Who::Token, "DELETE", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (holder, _) = link_to(&fx, "related", &locked).await;
        let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
        let body = json!({
            "task_ids": [holder],
            "action": "relations",
            "remove_links": [{"kind": "related", "target_id": locked}],
        });
        let (status, text) = fx.json(Who::Token, "POST", &uri, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn token_should_be_refused_deleting_a_link_held_by_a_locked_task() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        let (holder, link) = link_to(&fx, "related", &locked).await;
        let uri = format!("/api/tasks/{locked}/links/{link}");
        let (status, text) = fx.call(Who::Token, "DELETE", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "card_locked", "{text}");
        let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
        let body = json!({
            "task_ids": [locked],
            "action": "relations",
            "remove_links": [{"kind": "related", "target_id": holder}],
        });
        let (status, text) = fx.json(Who::Token, "POST", &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "card_locked", "{text}");
    }

    #[tokio::test]
    async fn token_should_be_refused_deleting_a_parent_or_child_link_to_a_locked_task() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        let (holder, link) = link_to(&fx, "child", &locked).await;
        let uri = format!("/api/tasks/{holder}/links/{link}");
        let (status, text) = fx.call(Who::Token, "DELETE", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "card_locked", "{text}");
        let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
        let body = json!({
            "task_ids": [holder],
            "action": "relations",
            "remove_links": [{"kind": "child", "target_id": locked}],
        });
        let (status, text) = fx.json(Who::Token, "POST", &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "card_locked", "{text}");
    }

    #[tokio::test]
    async fn bulk_relations_should_link_blocked_by_a_locked_task() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        let other = fx.task(&fx.backlog, json!({})).await;
        let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
        let body = json!({
            "task_ids": [other],
            "action": "relations",
            "add_links": [{"kind": "blocked_by", "target_id": locked}],
        });
        let (status, text) = fx.json(Who::Token, "POST", &uri, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn bulk_relations_should_still_refuse_a_link_that_changes_the_locked_card() {
        let fx = Fixture::new();
        let locked = fx.locked_task().await;
        let uri = format!("/api/boards/{}/tasks/bulk", fx.board);
        for kind in ["blocks", "parent", "child"] {
            let other = fx.task(&fx.backlog, json!({})).await;
            let body = json!({
                "task_ids": [other],
                "action": "relations",
                "add_links": [{"kind": kind, "target_id": locked}],
            });
            let (status, text) = fx.json(Who::Token, "POST", &uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{kind}: {text}");
            assert_eq!(parse(&text)["code"], "card_locked", "{kind}: {text}");
        }
        let other = fx.task(&fx.backlog, json!({})).await;
        let body = json!({
            "task_ids": [locked],
            "action": "relations",
            "add_links": [{"kind": "related", "target_id": other}],
        });
        let (status, text) = fx.json(Who::Token, "POST", &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "card_locked", "{text}");
    }

    #[tokio::test]
    async fn token_should_write_an_unlocked_task() {
        let fx = Fixture::new();
        let id = fx.task(&fx.backlog, json!({"labels": ["other"]})).await;
        let (status, text) = fx
            .json(
                Who::Token,
                "PATCH",
                &format!("/api/tasks/{id}"),
                json!({"name": "Renamed"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn token_should_post_a_question_when_the_newest_comment_is_the_owners() {
        let fx = Fixture::new();
        let task = fx.locked_task().await;
        let uri = format!("/api/tasks/{task}/comments");
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &uri,
                json!({"body": "Partial answer"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = fx.json(Who::Token, "POST", &uri, fx.question()).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn token_should_be_refused_other_comment_kinds_on_a_locked_task() {
        let fx = Fixture::new();
        let task = fx.locked_task().await;
        let uri = format!("/api/tasks/{task}/comments");
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &uri,
                json!({"body": "Partial answer"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        for body in [
            json!({"body": "Agent note"}),
            json!({"body": "Agent note", "kind": "agent"}),
        ] {
            let (status, text) = fx.json(Who::Token, "POST", &uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
            assert_eq!(parse(&text)["code"], "card_locked", "{text}");
        }
    }

    #[tokio::test]
    async fn token_should_be_refused_a_question_when_the_newest_comment_is_from_a_token() {
        let fx = Fixture::new();
        let task = fx.task(&fx.backlog, json!({})).await;
        let uri = format!("/api/tasks/{task}/comments");
        let (status, text) = fx
            .json(Who::Token, "POST", &uri, json!({"body": "Agent note"}))
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = fx
            .json(
                Who::Session,
                "PATCH",
                &format!("/api/tasks/{task}"),
                json!({"labels": ["needs:human"]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = fx.json(Who::Token, "POST", &uri, fx.question()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "card_locked", "{text}");
    }

    #[tokio::test]
    async fn token_should_be_refused_a_question_when_the_newest_comment_is_from_bootstrap() {
        let fx = Fixture::new();
        let task = fx.task(&fx.backlog, json!({})).await;
        let uri = format!("/api/tasks/{task}/comments");
        let (status, text) = fx
            .json(
                Who::Bootstrap,
                "POST",
                &uri,
                json!({"body": "Bootstrap note"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let stored = fx.db.task(&task).expect("read").expect("task");
        let newest = stored.comments.last().expect("comment");
        assert_eq!(newest.actor_kind, ActorKind::ApiToken);
        assert_eq!(newest.token_id, None);
        let (status, text) = fx
            .json(
                Who::Session,
                "PATCH",
                &format!("/api/tasks/{task}"),
                json!({"labels": ["needs:human"]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = fx.json(Who::Token, "POST", &uri, fx.question()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert_eq!(parse(&text)["code"], "card_locked", "{text}");
    }

    fn ship_question(fx: &Fixture) -> CommentQuestion {
        CommentQuestion {
            text: "Ship it?".to_string(),
            choices: vec![
                QuestionChoice {
                    label: "Ship".to_string(),
                    column_id: fx.stages["done"].clone(),
                },
                QuestionChoice {
                    label: "Review".to_string(),
                    column_id: fx.stages["review"].clone(),
                },
            ],
            recommended: 2,
            why: "Because".to_string(),
            answer: None,
        }
    }

    fn post_owner_last_question(fx: &Fixture, task: &str) -> QuestionWrite {
        let new = NewComment {
            author: "agent",
            body: "Ship it?",
            actor_kind: ActorKind::ApiToken,
            token_name: Some("agent"),
            token_id: Some("token-1"),
            kind: Some("question"),
        };
        let actor = Actor {
            name: "api-token",
            token_name: Some("agent"),
            kind: ActorKind::ApiToken,
            parent_token: None,
            role_id: None,
            role_name: None,
            label: None,
        };
        let guard = QuestionGuard {
            fence: None,
            owner_last: true,
        };
        fx.db
            .post_question(task, new, ship_question(fx), None, guard, actor)
            .expect("post")
    }

    #[tokio::test]
    async fn post_question_should_refuse_a_question_when_the_newest_comment_became_a_token_comment()
    {
        let fx = Fixture::new();
        let task = fx.task(&fx.backlog, json!({})).await;
        let uri = format!("/api/tasks/{task}/comments");
        for (who, body) in [(Who::Session, "Partial answer"), (Who::Token, "Agent note")] {
            let (status, text) = fx.json(who, "POST", &uri, json!({"body": body})).await;
            assert_eq!(status, StatusCode::OK, "{text}");
        }
        let before = fx.db.task(&task).expect("read").expect("task");
        let written = post_owner_last_question(&fx, &task);
        assert!(matches!(written, QuestionWrite::Locked), "{written:?}");
        let after = fx.db.task(&task).expect("read").expect("task");
        assert_eq!(after.comments.len(), before.comments.len());
        assert_eq!(after.history.len(), before.history.len());
    }

    #[tokio::test]
    async fn post_question_should_accept_a_question_when_the_newest_comment_is_the_owners() {
        let fx = Fixture::new();
        let task = fx.locked_task().await;
        let uri = format!("/api/tasks/{task}/comments");
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &uri,
                json!({"body": "Partial answer"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let written = post_owner_last_question(&fx, &task);
        assert!(matches!(written, QuestionWrite::Done(_)), "{written:?}");
        let after = fx.db.task(&task).expect("read").expect("task");
        assert_eq!(after.comments.len(), 2);
        assert_eq!(after.column_id, fx.backlog);
    }

    #[tokio::test]
    async fn label_only_patch_that_removes_the_lock_should_be_allowed_to_a_token() {
        let fx = Fixture::new();
        let id = fx.locked_task().await;
        let (status, text) = fx
            .json(
                Who::Token,
                "PATCH",
                &format!("/api/tasks/{id}"),
                json!({"labels": []}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = fx
            .json(
                Who::Token,
                "PATCH",
                &format!("/api/tasks/{id}"),
                json!({"name": "Renamed"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn labels_remove_of_the_lock_should_be_allowed_to_a_token_in_any_column() {
        let fx = Fixture::new();
        let id = fx
            .task(&fx.ready, json!({"labels": ["needs:human", "keep"]}))
            .await;
        fx.require_sections(&fx.ready, &["Problem"]).await;
        let uri = format!("/api/tasks/{id}");
        let (status, text) = fx
            .json(
                Who::Token,
                "PATCH",
                &uri,
                json!({"labels_remove": ["needs:human"]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(parse(&text)["labels"], json!(["keep"]));
    }

    #[tokio::test]
    async fn labels_add_or_unrelated_labels_remove_should_be_refused_to_a_token_on_a_locked_task() {
        let fx = Fixture::new();
        let cases = [
            json!({"labels_add": ["extra"]}),
            json!({"labels_remove": ["other"]}),
            json!({"labels_remove": ["needs:human"], "labels_add": ["needs:human"]}),
            json!({"labels_remove": ["needs:human"], "name": "Renamed"}),
        ];
        for patch in cases {
            let id = fx.locked_task().await;
            let (status, text) = fx
                .json(
                    Who::Token,
                    "PATCH",
                    &format!("/api/tasks/{id}"),
                    patch.clone(),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{patch}: {text}");
            assert_eq!(parse(&text)["code"], "card_locked", "{patch}");
        }
    }

    #[tokio::test]
    async fn mixed_or_still_locked_patch_should_be_refused_to_a_token() {
        let fx = Fixture::new();
        let cases = [
            json!({"labels": [], "name": "Renamed"}),
            json!({"labels": ["needs:human", "extra"]}),
        ];
        for patch in cases {
            let id = fx.locked_task().await;
            let (status, text) = fx
                .json(
                    Who::Token,
                    "PATCH",
                    &format!("/api/tasks/{id}"),
                    patch.clone(),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{patch}: {text}");
            assert_eq!(parse(&text)["code"], "card_locked", "{patch}");
        }
    }

    #[tokio::test]
    async fn locked_labels_should_come_from_the_board_rules() {
        let fx = Fixture::new();
        let rules = json!({"locked_labels": ["hold"]});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);
        let plain = fx.locked_task().await;
        let held = fx.task(&fx.backlog, json!({"labels": ["hold"]})).await;
        let rename = json!({"name": "Renamed"});
        let (status, text) = fx
            .json(
                Who::Token,
                "PATCH",
                &format!("/api/tasks/{plain}"),
                rename.clone(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (status, text) = fx
            .json(Who::Token, "PATCH", &format!("/api/tasks/{held}"), rename)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    }
}

mod stored {
    use super::*;

    #[tokio::test]
    async fn subtask_changes_should_hold_the_outcome_range_of_the_column() {
        let fx = Fixture::new();
        let id = fx.task(&fx.ready, json!({})).await;
        let subtasks = format!("/api/tasks/{id}/subtasks");
        let (status, text) = fx
            .json(Who::Session, "POST", &subtasks, json!({"name": "One"}))
            .await;
        assert!(status.is_success(), "{text}");
        let one = parse(&text)["id"].as_str().expect("subtask id").to_string();
        let rules = json!({"columns": {fx.ready.clone(): {"outcomes_min": 1, "outcomes_max": 1}}});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);

        let (status, text) = fx
            .json(Who::Session, "POST", &subtasks, json!({"name": "Two"}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("at most 1 outcomes"), "{text}");
        let (status, text) = fx
            .call(
                Who::Token,
                "DELETE",
                &format!("{subtasks}/{one}"),
                Body::Empty,
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert_eq!(parse(&text)["code"], "column_rule");
        assert!(text.contains("at least 1 outcomes"), "{text}");
    }

    #[tokio::test]
    async fn delete_all_subtasks_should_hold_the_outcome_minimum_of_the_column() {
        let fx = Fixture::new();
        let id = fx.task(&fx.ready, json!({})).await;
        let subtasks = format!("/api/tasks/{id}/subtasks");
        let (status, text) = fx
            .json(Who::Session, "POST", &subtasks, json!({"name": "One"}))
            .await;
        assert!(status.is_success(), "{text}");
        let rules = json!({"columns": {fx.ready.clone(): {"outcomes_min": 1}}});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);

        let (status, text) = fx.call(Who::Token, "DELETE", &subtasks, Body::Empty).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert_eq!(parse(&text)["code"], "column_rule");
    }

    #[tokio::test]
    async fn adding_from_a_template_should_hold_the_outcome_maximum_of_the_column() {
        let fx = Fixture::new();
        let source = fx.task(&fx.backlog, json!({})).await;
        let source_subtasks = format!("/api/tasks/{source}/subtasks");
        for name in ["One", "Two"] {
            let (status, text) = fx
                .json(
                    Who::Session,
                    "POST",
                    &source_subtasks,
                    json!({"name": name}),
                )
                .await;
            assert!(status.is_success(), "{text}");
        }
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &format!("{source_subtasks}/template"),
                json!({"name": "Pair"}),
            )
            .await;
        assert!(status.is_success(), "{text}");
        let template = parse(&text)["id"]
            .as_str()
            .expect("template id")
            .to_string();
        let target = fx.task(&fx.ready, json!({})).await;
        let rules = json!({"columns": {fx.ready.clone(): {"outcomes_max": 1}}});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);

        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &format!("/api/tasks/{target}/subtasks/from-template"),
                json!({"template_id": template}),
            )
            .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("at most 1 outcomes"), "{text}");
    }

    #[tokio::test]
    async fn convert_should_refuse_a_new_task_the_target_column_refuses() {
        let fx = Fixture::new();
        let id = fx.task(&fx.backlog, json!({})).await;
        let subtasks = format!("/api/tasks/{id}/subtasks");
        let (_, text) = fx
            .json(Who::Session, "POST", &subtasks, json!({"name": "One"}))
            .await;
        let one = parse(&text)["id"].as_str().expect("subtask id").to_string();
        fx.require_sections(&fx.ready, &["Done when"]).await;

        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &format!("{subtasks}/{one}/convert"),
                json!({"column_id": fx.ready}),
            )
            .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("Done when"), "{text}");
    }

    #[tokio::test]
    async fn convert_should_keep_the_subtask_and_leave_no_task_when_the_parent_column_needs_it() {
        let fx = Fixture::new();
        let id = fx.task(&fx.ready, json!({})).await;
        let subtasks = format!("/api/tasks/{id}/subtasks");
        let (_, text) = fx
            .json(Who::Session, "POST", &subtasks, json!({"name": "One"}))
            .await;
        let one = parse(&text)["id"].as_str().expect("subtask id").to_string();
        let rules = json!({"columns": {fx.ready.clone(): {"outcomes_min": 1}}});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);

        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &format!("{subtasks}/{one}/convert"),
                json!({"column_id": fx.backlog}),
            )
            .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("at least 1 outcomes"), "{text}");
        let (_, text) = fx
            .call(
                Who::Session,
                "GET",
                &format!("/api/tasks?board_id={}", fx.board),
                Body::Empty,
            )
            .await;
        assert!(!text.contains("\"name\":\"One\""), "{text}");
        let (_, text) = fx
            .call(
                Who::Session,
                "GET",
                &format!("/api/tasks/{id}"),
                Body::Empty,
            )
            .await;
        assert_eq!(parse(&text)["subtasks"][0]["id"], one.as_str(), "{text}");
        let (status, text) = fx
            .call(
                Who::Session,
                "GET",
                &format!("/api/boards/{}/bin", fx.board),
                Body::Empty,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(!text.contains("\"name\":\"One\""), "{text}");
    }

    #[tokio::test]
    async fn convert_should_fill_the_field_defaults_of_the_board() {
        let fx = Fixture::new();
        let uri = format!("/api/boards/{}/fields", fx.board);
        let owner = json!({"key": "owner", "label": "Owner", "kind": "text", "default": "alice"});
        let (status, text) = fx.json(Who::Session, "POST", &uri, owner).await;
        assert!(status.is_success(), "{text}");
        let id = fx.task(&fx.backlog, json!({})).await;
        let subtasks = format!("/api/tasks/{id}/subtasks");
        let (_, text) = fx
            .json(Who::Session, "POST", &subtasks, json!({"name": "One"}))
            .await;
        let one = parse(&text)["id"].as_str().expect("subtask id").to_string();

        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &format!("{subtasks}/{one}/convert"),
                json!({}),
            )
            .await;

        assert_eq!(status, StatusCode::CREATED, "{text}");
        assert_eq!(parse(&text)["fields"]["owner"], "alice", "{text}");
    }

    #[tokio::test]
    async fn deleting_a_field_should_drop_it_from_the_stored_rules() {
        let fx = Fixture::new();
        let uri = format!("/api/boards/{}/fields", fx.board);
        let field = json!({"key": "owner", "label": "Owner", "kind": "text"});
        let (status, text) = fx.json(Who::Session, "POST", &uri, field).await;
        assert!(status.is_success(), "{text}");
        let field_id = parse(&text)["id"].as_str().expect("field id").to_string();
        let rules = json!({
            "columns": {
                fx.ready.clone(): {"fields": ["owner"]},
                fx.backlog.clone(): {"sections": ["Done when"], "fields": ["owner"]},
            },
        });
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);

        let (status, text) = fx
            .call(
                Who::Session,
                "DELETE",
                &format!("/api/fields/{field_id}"),
                Body::Empty,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let (_, text) = fx
            .call(Who::Session, "GET", &fx.rules_uri(), Body::Empty)
            .await;
        assert_eq!(
            parse(&text)["columns"],
            json!({fx.backlog.clone(): {
                "sections": ["Done when"],
                "outcomes_min": null,
                "outcomes_max": null,
                "outcomes_locked": false,
                "fields": [],
            }})
        );
    }

    #[tokio::test]
    async fn cross_board_move_should_judge_the_fields_the_task_keeps() {
        let fx = Fixture::new();
        let field = |kind: &str| json!({"key": "owner", "label": "Owner", "kind": kind});
        let uri = format!("/api/boards/{}/fields", fx.board);
        let (status, text) = fx.json(Who::Session, "POST", &uri, field("text")).await;
        assert!(status.is_success(), "{text}");
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                "/api/boards",
                json!({"name": "Target"}),
            )
            .await;
        assert!(status.is_success(), "{text}");
        let target = parse(&text)["id"].as_str().expect("board id").to_string();
        let uri = format!("/api/boards/{target}/fields");
        let (status, text) = fx.json(Who::Session, "POST", &uri, field("number")).await;
        assert!(status.is_success(), "{text}");
        let (_, text) = fx
            .call(
                Who::Session,
                "GET",
                &format!("/api/boards/{target}/columns"),
                Body::Empty,
            )
            .await;
        let column = parse(&text)[0]["id"]
            .as_str()
            .expect("column id")
            .to_string();
        let rules = json!({"columns": {column.clone(): {"fields": ["owner"]}}});
        let uri = format!("/api/boards/{target}/rules");
        let (status, text) = fx.json(Who::Session, "PUT", &uri, rules).await;
        assert_eq!(status, StatusCode::OK, "{text}");

        let id = fx
            .task(&fx.backlog, json!({"fields": {"owner": "someone"}}))
            .await;
        let body = json!({"column_id": column, "position": 0});
        let (status, text) = fx
            .json(Who::Session, "POST", &format!("/api/tasks/{id}/move"), body)
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert!(text.contains("owner"), "{text}");
    }

    #[tokio::test]
    async fn archive_import_should_drop_the_stored_rules() {
        let fx = Fixture::new();
        let rules = json!({"locked_labels": ["hold"]});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);
        let uri = format!("/api/boards/{}/archive", fx.board);
        let (status, archive) = fx.call_bytes(Who::Session, "GET", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::OK);
        let (status, bytes) = fx
            .call_bytes(
                Who::Session,
                "POST",
                "/api/boards/import",
                Body::Raw(archive),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let imported = parse(&String::from_utf8_lossy(&bytes))["id"]
            .as_str()
            .expect("board id")
            .to_string();
        let uri = format!("/api/boards/{imported}/rules");
        let (status, text) = fx.call(Who::Session, "GET", &uri, Body::Empty).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(parse(&text)["locked_labels"], json!(["needs:human"]));
    }
}

mod label_case {
    use super::*;

    #[tokio::test]
    async fn locking_should_ignore_the_case_of_the_label() {
        let fx = Fixture::new();
        for label in ["Needs:Human", "NEEDS:HUMAN", " needs:human "] {
            let id = fx.task(&fx.backlog, json!({"labels": [label]})).await;
            let (status, text) = fx
                .json(
                    Who::Token,
                    "PATCH",
                    &format!("/api/tasks/{id}"),
                    json!({"name": "Renamed"}),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {text}");
            assert_eq!(parse(&text)["code"], "card_locked", "{label}");
        }
    }

    #[tokio::test]
    async fn custom_locked_label_should_match_a_card_label_ignoring_case() {
        let fx = Fixture::new();
        assert_eq!(
            fx.put_rules(json!({"locked_labels": ["Hold"]})).await,
            StatusCode::OK
        );
        let id = fx.task(&fx.backlog, json!({"labels": ["hold"]})).await;
        let (status, text) = fx
            .json(
                Who::Token,
                "PATCH",
                &format!("/api/tasks/{id}"),
                json!({"name": "Renamed"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    }

    #[tokio::test]
    async fn label_only_removal_of_a_case_variant_should_unlock_for_a_token() {
        let fx = Fixture::new();
        let id = fx
            .task(&fx.backlog, json!({"labels": ["Needs:Human"]}))
            .await;
        let uri = format!("/api/tasks/{id}");
        let (status, text) = fx
            .json(
                Who::Token,
                "PATCH",
                &uri,
                json!({"labels": ["NEEDS:HUMAN"]}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        let (status, text) = fx
            .json(Who::Token, "PATCH", &uri, json!({"labels": []}))
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
}

mod outcomes_lock {
    use super::*;

    async fn locked_card(fx: &Fixture) -> (String, String) {
        let id = fx.task(&fx.ready, json!({})).await;
        let (status, text) = fx
            .json(
                Who::Session,
                "POST",
                &format!("/api/tasks/{id}/subtasks"),
                json!({"name": "One"}),
            )
            .await;
        assert!(status.is_success(), "{text}");
        let sub = parse(&text)["id"].as_str().expect("subtask id").to_string();
        let rules = json!({"columns": {fx.ready.clone(): {"outcomes_locked": true}}});
        assert_eq!(fx.put_rules(rules).await, StatusCode::OK);
        (id, sub)
    }

    #[tokio::test]
    async fn outcomes_lock_should_refuse_subtask_edits_without_the_permission() {
        let fx = Fixture::new();
        let (id, sub) = locked_card(&fx).await;
        let subtasks = format!("/api/tasks/{id}/subtasks");
        for (method, uri, body) in [
            ("POST", subtasks.clone(), Body::Json(json!({"name": "Two"}))),
            (
                "PATCH",
                format!("{subtasks}/{sub}"),
                Body::Json(json!({"name": "Renamed"})),
            ),
            (
                "PUT",
                format!("{subtasks}/order"),
                Body::Json(json!({"order": [sub.clone()]})),
            ),
            ("DELETE", format!("{subtasks}/{sub}"), Body::Empty),
            ("DELETE", subtasks.clone(), Body::Empty),
            (
                "POST",
                format!("{subtasks}/from-template"),
                Body::Json(json!({"template_id": "none"})),
            ),
            (
                "POST",
                format!("{subtasks}/{sub}/convert"),
                Body::Json(json!({})),
            ),
        ] {
            let (status, text) = fx.call(Who::Lead, method, &uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {text}");
            let body = parse(&text);
            assert_eq!(body["permission"], "outcomes.edit_locked", "{text}");
            let message = body["message"].as_str().expect("message");
            assert!(message.contains("\"Ready\""), "{message}");
        }
        let (status, text) = fx
            .json(
                Who::Lead,
                "POST",
                &format!("/api/boards/{}/tasks/bulk", fx.board),
                json!({"action": "subtasks", "task_ids": [id.clone()], "add_subtasks": ["Two"]}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        let (_, text) = fx
            .call(
                Who::Session,
                "GET",
                &format!("/api/tasks/{id}"),
                Body::Empty,
            )
            .await;
        assert_eq!(parse(&text)["subtasks"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn outcomes_lock_should_let_the_owner_and_an_admin_token_edit() {
        let fx = Fixture::new();
        let (id, sub) = locked_card(&fx).await;
        let subtasks = format!("/api/tasks/{id}/subtasks");
        for who in [Who::Session, Who::Token] {
            let (status, text) = fx
                .json(
                    who,
                    "PATCH",
                    &format!("{subtasks}/{sub}"),
                    json!({"name": "Renamed"}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{who:?}: {text}");
            let (status, text) = fx
                .json(who, "POST", &subtasks, json!({"name": "Extra"}))
                .await;
            assert_eq!(status, StatusCode::OK, "{who:?}: {text}");
            let (status, text) = fx
                .json(
                    who,
                    "POST",
                    &format!("{subtasks}/from-template"),
                    json!({"template_id": "none"}),
                )
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{who:?}: {text}");
        }
    }

    #[tokio::test]
    async fn outcomes_lock_should_allow_ticks_and_default_off() {
        let fx = Fixture::new();
        let open = fx.task(&fx.ready, json!({})).await;
        let uri = format!("/api/tasks/{open}/subtasks");
        let (status, text) = fx
            .json(Who::Lead, "POST", &uri, json!({"name": "Free"}))
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");

        let (id, sub) = locked_card(&fx).await;
        let (status, text) = fx
            .json(
                Who::Lead,
                "PATCH",
                &format!("/api/tasks/{id}/subtasks/{sub}"),
                json!({"done": true}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(parse(&text)["done"], true, "{text}");
        let other = fx.task(&fx.backlog, json!({})).await;
        let (status, text) = fx
            .json(
                Who::Lead,
                "POST",
                &format!("/api/tasks/{other}/subtasks"),
                json!({"name": "Elsewhere"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }

    #[tokio::test]
    async fn outcomes_lock_should_save_per_column_and_be_served_with_the_rules() {
        let fx = Fixture::new();
        let _ = locked_card(&fx).await;
        let (_, text) = fx
            .call(Who::Lead, "GET", &fx.rules_uri(), Body::Empty)
            .await;
        assert_eq!(parse(&text)["columns"][&fx.ready]["outcomes_locked"], true);
        let uri = format!("/b/{}/settings/rules", fx.board);
        let (_, page) = fx.call(Who::Session, "GET", &uri, Body::Empty).await;
        assert!(page.contains("br-outcomes-locked\" checked"), "{page}");
    }
}
