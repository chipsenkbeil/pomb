use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::passkey_tests::{Reply, call};
use super::server::{passkey_mode, unthrottled_limits};
use super::{SEARCH_BUDGET_SECS, router, run_search};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, AuthScope, BoardAccess};
use crate::search::{BoardView, Query, TimedOut};

const GOLDEN: &str = include_str!("../../.dev/golden-master-board.json");

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    admin: AccountId,
    token: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("search.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "admin", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            admin,
            token,
        }
    }

    fn board(&self, name: &str, columns: &[&str], done: &str) -> (String, Vec<String>) {
        let board = self.db.create_board(&self.admin, name).expect("board");
        self.db.create_swimlane(&board, "Lane").expect("lane");
        let ids = columns
            .iter()
            .map(|column| {
                let id = self.db.create_column(&board, column, None).expect("column");
                if *column == done {
                    self.db.set_column_done(&id, true).expect("done flag");
                }
                id
            })
            .collect();
        (board, ids)
    }

    async fn send(&self, token: &str, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {token}");
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    async fn task(&self, body: Value) -> String {
        let reply = self
            .send(&self.token, "POST", "/api/tasks", Some(body))
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["id"].as_str().expect("id").to_string()
    }

    async fn post(&self, uri: &str, body: Value) {
        let reply = self.send(&self.token, "POST", uri, Some(body)).await;
        assert!(reply.status.is_success(), "{uri}: {}", reply.body);
    }

    async fn search_as(&self, token: &str, query: &str) -> Reply {
        let uri = format!("/api/search?q={}", encode(query));
        self.send(token, "GET", &uri, None).await
    }

    async fn search(&self, query: &str) -> Reply {
        self.search_as(&self.token, query).await
    }

    async fn names(&self, query: &str) -> Vec<String> {
        let reply = self.search(query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.body);
        names_of(&reply)
    }
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn names_of(reply: &Reply) -> Vec<String> {
    reply.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["name"].as_str().expect("name").to_string())
        .collect()
}

/// Two boards: Plans with Todo, Doing and Shipped (done), and Ops, archived, with Queue.
struct Seeded {
    fx: Fixture,
    plans: String,
    ops: String,
}

async fn seeded() -> Seeded {
    let fx = Fixture::new();
    let (plans, columns) = fx.board("Plans", &["Todo", "Doing", "Shipped"], "Shipped");
    let (ops, ops_columns) = fx.board("Ops", &["Queue"], "none");
    let colors = fx.db.list_colors(&plans).expect("colors");
    let red = colors
        .iter()
        .find(|color| color.value == "red")
        .expect("a red color");
    let staging = fx
        .task(json!({
            "column_id": columns[0],
            "name": "Deploy to staging",
            "description": "Ship the release candidate",
            "labels": ["backend"],
            "color_id": red.id,
        }))
        .await;
    fx.post(
        &format!("/api/tasks/{staging}/subtasks"),
        json!({"name": "Write the runbook"}),
    )
    .await;
    fx.post(
        &format!("/api/tasks/{staging}/comments"),
        json!({"body": "Needs a second review"}),
    )
    .await;
    fx.post(
        &format!("/api/tasks/{staging}/subtasks"),
        json!({"name": "Tag the build"}),
    )
    .await;
    fx.post(
        &format!("/api/tasks/{staging}/comments"),
        json!({"body": "Ship it after QA"}),
    )
    .await;
    fx.task(json!({
        "column_id": columns[1],
        "name": "Fix deploy script",
        "labels": ["bug", "backend"],
    }))
    .await;
    fx.task(json!({
        "column_id": columns[2],
        "name": "Deploy docs",
        "description": "Document the deploy",
    }))
    .await;
    fx.task(json!({"column_id": ops_columns[0], "name": "Deploy old stuff"}))
        .await;
    let archived = fx
        .send(
            &fx.token,
            "PUT",
            &format!("/api/boards/{ops}/config"),
            Some(json!({"archived": true})),
        )
        .await;
    assert_eq!(archived.status, StatusCode::OK, "{}", archived.body);
    Seeded { fx, plans, ops }
}

#[tokio::test]
async fn search_should_find_cards_by_fuzzy_words_from_the_first_letter() {
    let Seeded { fx, .. } = seeded().await;
    assert_eq!(
        fx.names("dpl").await.first().map(String::as_str),
        Some("Deploy to staging")
    );
    assert_eq!(fx.names("stg").await, ["Deploy to staging"]);
    assert_eq!(fx.names("DEPLOY").await.len(), 4);
    assert!(fx.names("ploy").await.is_empty());
    assert!(fx.names("zzz").await.is_empty());
}

#[tokio::test]
async fn search_should_report_the_board_column_field_and_ranges() {
    let Seeded { fx, plans, .. } = seeded().await;
    let reply = fx.search("dpl in:title board:Plans column:Todo").await;
    let item = &reply.body["items"][0];
    assert_eq!(item["board_id"], plans);
    assert_eq!(item["board_name"], "Plans");
    assert_eq!(item["column_name"], "Todo");
    assert_eq!(item["status"], "open");
    assert_eq!(item["matched_field"], "title");
    assert_eq!(item["matched_index"], Value::Null);
    assert_eq!(
        item["ranges"],
        json!([{"start": 0, "end": 1}, {"start": 2, "end": 4}])
    );
    assert_eq!(reply.body["truncated"], false);
}

#[tokio::test]
async fn search_should_restrict_fields_with_in() {
    let Seeded { fx, .. } = seeded().await;
    assert_eq!(fx.names("in:title release").await, Vec::<String>::new());
    assert_eq!(
        fx.names("in:description release").await,
        ["Deploy to staging"]
    );
    assert_eq!(fx.names("in:title in:description deploy").await.len(), 4);
    let reply = fx.search("in:subtasks runbook").await;
    assert_eq!(names_of(&reply), ["Deploy to staging"]);
    assert_eq!(reply.body["items"][0]["matched_field"], "subtasks");
    assert_eq!(reply.body["items"][0]["matched_index"], 0);
    let reply = fx.search("in:comments review").await;
    assert_eq!(reply.body["items"][0]["matched_field"], "comments");
    assert_eq!(reply.body["items"][0]["matched_index"], 0);
    assert_eq!(
        reply.body["items"][0]["ranges"],
        json!([{"start": 15, "end": 21}])
    );
}

#[tokio::test]
async fn search_should_filter_by_board_column_label_color_and_status() {
    let Seeded { fx, plans, ops } = seeded().await;
    assert_eq!(fx.names("deploy board:ops").await, ["Deploy old stuff"]);
    assert_eq!(
        fx.names(&format!("deploy board:{ops}")).await,
        ["Deploy old stuff"]
    );
    assert_eq!(
        fx.names(&format!("board:{plans} column:doing")).await,
        ["Fix deploy script"]
    );
    assert_eq!(fx.names("deploy -board:plans").await, ["Deploy old stuff"]);
    assert_eq!(fx.names("label:bug").await, ["Fix deploy script"]);
    assert_eq!(
        fx.names("label:BACKEND label:bug").await,
        ["Fix deploy script"]
    );
    assert_eq!(
        fx.names("deploy -label:bug label:backend").await,
        ["Deploy to staging"]
    );
    assert_eq!(fx.names("color:Red").await, ["Deploy to staging"]);
    assert_eq!(fx.names("deploy is:done").await, ["Deploy docs"]);
    assert_eq!(fx.names("deploy is:archived").await, ["Deploy old stuff"]);
    assert_eq!(
        fx.names("deploy is:done is:archived").await,
        ["Deploy docs", "Deploy old stuff"]
    );
    assert_eq!(
        fx.names("deploy -is:done -is:archived").await,
        ["Deploy to staging", "Fix deploy script"]
    );
    assert_eq!(fx.names("column:\"Queue\"").await, ["Deploy old stuff"]);
}

#[tokio::test]
async fn search_should_report_the_index_of_the_matched_subtask_and_comment() {
    let Seeded { fx, .. } = seeded().await;
    let reply = fx.search("in:subtasks tag").await;
    assert_eq!(names_of(&reply), ["Deploy to staging"]);
    assert_eq!(reply.body["items"][0]["matched_field"], "subtasks");
    assert_eq!(reply.body["items"][0]["matched_index"], 1);
    assert_eq!(
        reply.body["items"][0]["ranges"],
        json!([{"start": 0, "end": 3}])
    );
    let reply = fx.search("in:comments qa").await;
    assert_eq!(reply.body["items"][0]["matched_field"], "comments");
    assert_eq!(reply.body["items"][0]["matched_index"], 1);
}

#[tokio::test]
async fn search_should_and_repeated_labels_and_or_the_other_repeated_filters() {
    let Seeded { fx, .. } = seeded().await;
    assert_eq!(
        fx.names("label:backend label:bug").await,
        ["Fix deploy script"]
    );
    assert_eq!(
        fx.names("label:backend").await,
        ["Deploy to staging", "Fix deploy script"]
    );
    assert!(fx.names("label:bug label:missing").await.is_empty());
    assert_eq!(fx.names("deploy board:plans board:ops").await.len(), 4);
    assert_eq!(
        fx.names("column:todo column:doing").await,
        ["Deploy to staging", "Fix deploy script"]
    );
    assert_eq!(
        fx.names("color:red color:blue deploy").await,
        ["Deploy to staging"]
    );
    assert_eq!(
        fx.names("deploy is:open is:done").await,
        ["Deploy to staging", "Fix deploy script", "Deploy docs"]
    );
}

#[tokio::test]
async fn search_should_drop_cards_with_a_negated_label() {
    let Seeded { fx, .. } = seeded().await;
    assert_eq!(
        fx.names("deploy -label:bug").await,
        ["Deploy to staging", "Deploy docs", "Deploy old stuff"]
    );
    assert_eq!(
        fx.names("-label:backend deploy").await,
        ["Deploy docs", "Deploy old stuff"]
    );
}

#[tokio::test]
async fn search_should_let_plain_words_match_in_different_fields() {
    let Seeded { fx, .. } = seeded().await;
    let reply = fx.search("dpl runbook").await;
    assert_eq!(names_of(&reply), ["Deploy to staging"]);
    assert_eq!(reply.body["items"][0]["matched_field"], "title");
    assert_eq!(
        names_of(&fx.search("stg review").await),
        ["Deploy to staging"]
    );
    assert!(fx.names("stg zzzz").await.is_empty());
}

#[tokio::test]
async fn search_should_match_quoted_phrases_and_exclude_words() {
    let Seeded { fx, .. } = seeded().await;
    assert_eq!(fx.names("\"DEPLOY TO\"").await, ["Deploy to staging"]);
    assert!(fx.names("\"deploy staging\"").await.is_empty());
    assert_eq!(fx.names("deploy -script in:title").await.len(), 3);
    assert_eq!(fx.names("deploy -\"to staging\" in:title").await.len(), 3);
    assert_eq!(
        fx.names("in:comments \"second review\"").await,
        ["Deploy to staging"]
    );
}

#[tokio::test]
async fn search_should_refuse_unknown_and_malformed_terms() {
    let Seeded { fx, .. } = seeded().await;
    for (query, term) in [
        ("deploy foo:bar", "foo:bar"),
        ("is:foo", "is:foo"),
        ("in:foo", "in:foo"),
        ("label:", "label:"),
        ("-in:title", "-in:title"),
        ("\"deploy", "deploy"),
        ("a\"b", "a\"b"),
    ] {
        let reply = fx.search(query).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{query}: {}",
            reply.body
        );
        assert_eq!(reply.code.as_deref(), Some("invalid_request"), "{query}");
        let message = reply.body["message"].as_str().expect("message");
        assert!(message.contains(term), "{query}: {message}");
    }
    for query in ["", "   "] {
        let reply = fx.search(query).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query:?}");
        assert_eq!(reply.code.as_deref(), Some("invalid_request"));
    }
    for uri in [
        "/api/search",
        "/api/search?q=a&limit=0",
        "/api/search?q=a&limit=101",
        "/api/search?q=a&limit=x",
        "/api/search?q=a&q=b",
    ] {
        let reply = fx.send(&fx.token, "GET", uri, None).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            reply.body
        );
        assert_eq!(reply.code.as_deref(), Some("invalid_request"), "{uri}");
    }
}

#[tokio::test]
async fn search_should_rank_title_first_and_open_above_done_and_archived() {
    let fx = Fixture::new();
    let (_, columns) = fx.board("Plans", &["Todo", "Done"], "Done");
    let (old_board, archive_columns) = fx.board("Old", &["Todo"], "none");
    fx.task(json!({"column_id": columns[0], "name": "Plan", "description": "the release soon"}))
        .await;
    fx.task(json!({"column_id": columns[0], "name": "Release notes"}))
        .await;
    fx.task(json!({"column_id": columns[1], "name": "Release shipped"}))
        .await;
    fx.task(json!({"column_id": archive_columns[0], "name": "Release old"}))
        .await;
    let archived = fx
        .send(
            &fx.token,
            "PUT",
            &format!("/api/boards/{old_board}/config"),
            Some(json!({"archived": true})),
        )
        .await;
    assert_eq!(archived.status, StatusCode::OK, "{}", archived.body);
    let reply = fx.search("release").await;
    assert_eq!(
        names_of(&reply),
        ["Release notes", "Plan", "Release shipped", "Release old"]
    );
    let statuses: Vec<&str> = reply.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["status"].as_str().expect("status"))
        .collect();
    assert_eq!(statuses, ["open", "open", "done", "archived"]);
    assert_eq!(reply.body["items"][1]["matched_field"], "description");
}

#[tokio::test]
async fn search_should_cap_results_at_the_limit_and_say_so() {
    let Seeded { fx, .. } = seeded().await;
    let reply = fx
        .send(&fx.token, "GET", "/api/search?q=deploy&limit=2", None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(names_of(&reply), ["Deploy to staging", "Fix deploy script"]);
    assert_eq!(reply.body["truncated"], true);
    let reply = fx
        .send(&fx.token, "GET", "/api/search?q=deploy&limit=4", None)
        .await;
    assert_eq!(reply.body["truncated"], false);
}

#[tokio::test]
async fn search_should_stop_when_the_time_budget_is_spent() {
    let Seeded { fx, .. } = seeded().await;
    let account = fx.db.account(&fx.admin);
    let terms = Query::parse("deploy").expect("a query");
    let spent = run_search(&account, |_| true, &terms, 25, Duration::ZERO)
        .ok()
        .expect("a page");
    assert!(spent.truncated);
    assert!(spent.items.is_empty());
    let roomy = run_search(
        &account,
        |_| true,
        &terms,
        25,
        Duration::from_secs(SEARCH_BUDGET_SECS),
    )
    .ok()
    .expect("a page");
    assert!(!roomy.truncated);
    assert_eq!(roomy.items.len(), 4);
}

#[tokio::test]
async fn search_should_time_out_between_the_segments_of_a_card() {
    let Seeded { fx, plans, .. } = seeded().await;
    let account = fx.db.account(&fx.admin);
    let board = account
        .list_boards()
        .expect("boards")
        .into_iter()
        .find(|board| board.id == plans)
        .expect("the board");
    let snapshot = account
        .search_snapshot(&plans)
        .expect("snapshot")
        .expect("a board");
    let view = BoardView::new(
        &board,
        snapshot.columns().expect("columns"),
        snapshot.colors().expect("colors"),
    );
    let mut checked = 0;
    for id in snapshot.task_ids().expect("ids") {
        let task = snapshot.task(&id).expect("task").expect("a task");
        if task.subtasks.is_empty() {
            continue;
        }
        let terms = Query::parse("dpl").expect("a query");
        let spent = terms.find(&view, &task, Instant::now());
        assert_eq!(spent.err(), Some(TimedOut));
        let roomy = terms.find(&view, &task, Instant::now() + Duration::from_secs(60));
        assert!(matches!(roomy, Ok(Some(_))));
        checked += 1;
    }
    assert_eq!(checked, 1);
}

#[tokio::test]
async fn search_should_show_a_board_limited_token_only_its_boards() {
    let Seeded { fx, plans, ops } = seeded().await;
    let (mut record, raw) = new_api_token_record(&fx.admin, "limited", AuthScope::Read, None);
    record.boards = BoardAccess::Only(BTreeSet::from([plans.clone()]));
    fx.db.create_api_token(&record).expect("token");

    let reply = fx.search_as(&raw, "deploy").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        names_of(&reply),
        ["Deploy to staging", "Fix deploy script", "Deploy docs"]
    );
    let reply = fx.search_as(&raw, "-zzqx").await;
    assert!(
        reply.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .all(|item| item["board_id"] == plans),
        "{}",
        reply.body
    );
    for query in [
        format!("board:{ops}"),
        "board:Ops".to_string(),
        "is:archived".to_string(),
    ] {
        let reply = fx.search_as(&raw, &query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.body);
        assert!(names_of(&reply).is_empty(), "{query}: {}", reply.body);
    }
    assert_eq!(fx.names("deploy").await.len(), 4);
}

#[tokio::test]
async fn search_should_answer_within_the_budget_over_the_golden_board() {
    let fx = Fixture::new();
    let golden: Value = serde_json::from_str(GOLDEN).expect("golden board");
    let board = fx
        .db
        .create_board(
            &fx.admin,
            golden["board_name"].as_str().expect("board name"),
        )
        .expect("board");
    fx.db.create_swimlane(&board, "Lane").expect("lane");
    let column_names: Vec<&str> = golden["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .map(|column| column["name"].as_str().expect("column name"))
        .collect();
    let last = column_names.len() - 1;
    let mut columns = Vec::new();
    for (index, name) in column_names.iter().enumerate() {
        let id = fx.db.create_column(&board, name, None).expect("column");
        if index == last {
            fx.db.set_column_done(&id, true).expect("done flag");
        }
        columns.push((*name, id));
    }
    let colors = fx.db.list_colors(&board).expect("colors");
    let tasks = golden["tasks"].as_array().expect("tasks");
    for task in tasks {
        let column = &columns
            .iter()
            .find(|(name, _)| Some(*name) == task["column"].as_str())
            .expect("a known column")
            .1;
        let color = colors
            .iter()
            .find(|color| Some(color.label.as_str()) == task["color"].as_str())
            .map(|color| color.id.clone());
        let id = fx
            .task(json!({
                "column_id": column,
                "name": task["name"],
                "description": task["description"],
                "labels": task["labels"],
                "color_id": color,
            }))
            .await;
        for subtask in task["subtasks"].as_array().into_iter().flatten() {
            fx.post(
                &format!("/api/tasks/{id}/subtasks"),
                json!({"name": subtask["name"]}),
            )
            .await;
        }
        for comment in task["comments"].as_array().into_iter().flatten() {
            fx.post(
                &format!("/api/tasks/{id}/comments"),
                json!({"body": comment}),
            )
            .await;
        }
    }

    let budget = Duration::from_secs(SEARCH_BUDGET_SECS);
    for (query, expected) in [
        ("pagination", Some("Fix pagination bug in search results")),
        (
            "in:comments idempotency",
            Some("Build payment webhook handler"),
        ),
        (
            "in:subtasks pipeline",
            Some("Set up CI/CD pipeline for staging"),
        ),
        ("is:done", None),
        ("label:bug -is:done", None),
        ("a", None),
    ] {
        let started = Instant::now();
        let reply = fx.search(query).await;
        let elapsed = started.elapsed();
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.body);
        assert!(elapsed < budget, "{query} took {elapsed:?}");
        assert_eq!(reply.body["truncated"], false, "{query}");
        let names = names_of(&reply);
        if let Some(expected) = expected {
            assert!(
                names.iter().any(|name| name == expected),
                "{query}: {names:?}"
            );
        }
    }
    assert_eq!(fx.names("is:done").await.len(), 3);
}
