use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, AuthScope, HistoryKind, NEEDS_HUMAN_LABEL, TaskRow};

/// Edits a valid question to drop or break one part.
type Breaker = Box<dyn Fn(&mut Value)>;

/// The credential a request carries.
#[derive(Copy, Clone)]
enum Caller {
    Browser,
    Token,
    Lead,
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    user_id: AccountId,
    app: axum::Router,
    cookie: String,
    bearer: String,
    lead: String,
    board_id: String,
    todo: String,
    blocked: String,
    review: String,
    done: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("questions.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user_id = db.create_user("admin").expect("create user");
    let session = db.create_session(&user_id, "").expect("session");
    let (record, raw) = new_api_token_record(&user_id, "agent-7", AuthScope::Work, None);
    db.create_api_token(&record).expect("token");
    let (record, lead) = new_api_token_record(&user_id, "lead-1", AuthScope::Provision, None);
    db.create_api_token(&record).expect("lead token");
    let board_id = db.create_board(&user_id, "Questions").expect("board");
    let todo = db.create_column(&board_id, "Todo", None).expect("column");
    let blocked = db
        .create_column(&board_id, "Waiting", None)
        .expect("column");
    let review = db.create_column(&board_id, "Review", None).expect("column");
    let done = db
        .create_column(&board_id, "Shipped", None)
        .expect("column");
    let app =
        router(AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()));
    Fixture {
        _dir: dir,
        db,
        user_id,
        app,
        cookie: format!("session={session}"),
        bearer: format!("Bearer {raw}"),
        lead: format!("Bearer {lead}"),
        board_id,
        todo,
        blocked,
        review,
        done,
    }
}

async fn send(
    fx: &Fixture,
    as_token: Caller,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("accept", "application/json");
    builder = match as_token {
        Caller::Token => builder.header("authorization", &fx.bearer),
        Caller::Lead => builder.header("authorization", &fx.lead),
        Caller::Browser => builder.header("cookie", &fx.cookie),
    };
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            axum::body::Body::from(value.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = fx
        .app
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn mark_blocked(fx: &Fixture) {
    let (status, view) = send(
        fx,
        Caller::Browser,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"blocked_column_id": fx.blocked})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["blocked_column_id"], json!(fx.blocked));
}

async fn card(fx: &Fixture, body: Value) -> String {
    let mut body = body;
    body["column_id"] = json!(fx.todo);
    body["name"] = json!("Card");
    let (status, task) = send(fx, Caller::Browser, "POST", "/api/tasks", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    task["id"].as_str().expect("id").to_string()
}

fn question(fx: &Fixture) -> Value {
    json!({
        "text": "Ship the migration now?",
        "choices": [
            {"label": "Ship it", "column_id": fx.done},
            {"label": "Review first", "column_id": fx.review},
        ],
        "recommended": 2,
        "why": "The rollback path is untested",
    })
}

async fn ask(fx: &Fixture, task: &str, question: Value) -> (StatusCode, Value) {
    send(
        fx,
        Caller::Token,
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"question": question})),
    )
    .await
}

async fn answer(
    fx: &Fixture,
    as_token: Caller,
    task: &str,
    comment: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        fx,
        as_token,
        "POST",
        &format!("/api/tasks/{task}/comments/{comment}/answer"),
        Some(body),
    )
    .await
}

async fn detail(fx: &Fixture, task: &str) -> Value {
    let (status, detail) = send(
        fx,
        Caller::Browser,
        "GET",
        &format!("/api/tasks/{task}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    detail
}

async fn comments(fx: &Fixture, task: &str) -> Vec<Value> {
    let (status, list) = send(
        fx,
        Caller::Browser,
        "GET",
        &format!("/api/tasks/{task}/comments"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list.as_array().expect("comments").clone()
}

fn row(fx: &Fixture, task: &str) -> TaskRow {
    fx.db
        .account(&fx.user_id)
        .task(task)
        .expect("read")
        .expect("task")
        .row()
        .clone()
}

async fn assert_untouched(fx: &Fixture, task: &str, column: &str) {
    let after = detail(fx, task).await;
    assert_eq!(after["column_id"], json!(column), "{after}");
    assert_eq!(after["labels"], json!([]), "{after}");
    assert!(comments(fx, task).await.is_empty());
}

#[tokio::test]
async fn posting_a_question_should_block_the_card_and_label_it_in_one_write() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({"labels": ["backend"]})).await;

    let (status, comment) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::OK, "{comment}");
    assert_eq!(comment["kind"], "question");
    assert_eq!(comment["body"], "Ship the migration now?");
    assert_eq!(comment["token_name"], "agent-7");
    assert_eq!(comment["question"]["recommended"], 2);
    assert_eq!(comment["question"]["why"], "The rollback path is untested");
    assert_eq!(
        comment["question"]["choices"][1]["column_id"],
        json!(fx.review)
    );
    assert_eq!(comment["question"]["answer"], Value::Null);
    let after = detail(&fx, &task).await;
    assert_eq!(after["column_id"], json!(fx.blocked), "{after}");
    assert_eq!(
        after["labels"],
        json!(["backend", "needs:human"]),
        "{after}"
    );
    assert_eq!(comments(&fx, &task).await.len(), 1);
    let kinds: Vec<HistoryKind> = row(&fx, &task)
        .history
        .iter()
        .map(|event| event.kind)
        .collect();
    for kind in [
        HistoryKind::CommentAdded,
        HistoryKind::LabelsUpdated,
        HistoryKind::Moved,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} missing from {kinds:?}");
    }
}

#[tokio::test]
async fn a_question_missing_a_part_should_be_refused_naming_it() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({})).await;
    let other = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let foreign = fx
        .db
        .create_column(&other, "Elsewhere", None)
        .expect("column");
    let cases: Vec<(&str, Breaker, &str)> = vec![
        (
            "no text",
            Box::new(|q| {
                q.as_object_mut().unwrap().remove("text");
            }),
            "question.text is required",
        ),
        (
            "blank text",
            Box::new(|q| q["text"] = json!("  ")),
            "question.text is required",
        ),
        (
            "no choices",
            Box::new(|q| {
                q.as_object_mut().unwrap().remove("choices");
            }),
            "question.choices needs between 2 and 10",
        ),
        (
            "one choice",
            Box::new(|q| {
                q["choices"].as_array_mut().unwrap().pop();
            }),
            "question.choices needs between 2 and 10",
        ),
        (
            "no label",
            Box::new(|q| {
                q["choices"][1].as_object_mut().unwrap().remove("label");
            }),
            "question.choices[2].label is required",
        ),
        (
            "no column",
            Box::new(|q| {
                q["choices"][0].as_object_mut().unwrap().remove("column_id");
            }),
            "question.choices[1].column_id is required",
        ),
        (
            "unknown column",
            Box::new(|q| q["choices"][0]["column_id"] = json!("nope")),
            "question.choices[1].column_id names a column that is not on this board",
        ),
        (
            "column of another board",
            Box::new({
                let foreign = foreign.clone();
                move |q| q["choices"][1]["column_id"] = json!(foreign)
            }),
            "question.choices[2].column_id names a column that is not on this board",
        ),
        (
            "no recommended",
            Box::new(|q| {
                q.as_object_mut().unwrap().remove("recommended");
            }),
            "question.recommended is required",
        ),
        (
            "recommended past the choices",
            Box::new(|q| q["recommended"] = json!(3)),
            "question.recommended must be a choice number from 1 to 2",
        ),
        (
            "recommended zero",
            Box::new(|q| q["recommended"] = json!(0)),
            "question.recommended must be a choice number from 1 to 2",
        ),
        (
            "no why",
            Box::new(|q| {
                q.as_object_mut().unwrap().remove("why");
            }),
            "question.why is required",
        ),
    ];
    for (name, break_it, message) in cases {
        let mut broken = question(&fx);
        break_it(&mut broken);
        let (status, body) = ask(&fx, &task, broken).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {body}");
        let text = body["message"].as_str().unwrap_or_default();
        assert!(text.contains(message), "{name}: {body}");
        assert_untouched(&fx, &task, &fx.todo).await;
    }

    let (status, body) = send(
        &fx,
        Caller::Token,
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"question": question(&fx), "kind": "agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_untouched(&fx, &task, &fx.todo).await;
}

#[tokio::test]
async fn a_question_on_a_board_without_a_blocked_column_should_be_refused() {
    let fx = fixture();
    let task = card(&fx, json!({})).await;

    let (status, body) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no Blocked column"),
        "{body}"
    );
    assert_untouched(&fx, &task, &fx.todo).await;
}

#[tokio::test]
async fn a_blocked_column_of_another_board_should_be_refused_in_the_config() {
    let fx = fixture();
    let other = fx.db.create_board(&fx.user_id, "Other").expect("board");
    let foreign = fx
        .db
        .create_column(&other, "Elsewhere", None)
        .expect("column");

    let (status, body) = send(
        &fx,
        Caller::Browser,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"blocked_column_id": foreign})),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_question_into_a_full_blocked_column_should_write_nothing() {
    let fx = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Held", Some(1))
        .expect("column");
    let (status, view) = send(
        &fx,
        Caller::Browser,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"blocked_column_id": limited})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let (status, filler) = send(
        &fx,
        Caller::Browser,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": limited, "name": "Filler"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{filler}");
    let task = card(&fx, json!({})).await;

    let (status, body) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "wip_limit_reached", "{body}");
    assert_untouched(&fx, &task, &fx.todo).await;
}

#[tokio::test]
async fn an_epic_with_an_open_child_should_refuse_a_limited_blocked_column() {
    let fx = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Held", Some(5))
        .expect("column");
    let (status, view) = send(
        &fx,
        Caller::Browser,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"blocked_column_id": limited})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let epic = card(&fx, json!({"kind": "epic"})).await;
    let child = card(&fx, json!({})).await;
    let (status, linked) = send(
        &fx,
        Caller::Browser,
        "POST",
        &format!("/api/tasks/{child}/links"),
        Some(json!({"kind": "parent", "target_id": epic})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");

    let (status, body) = ask(&fx, &epic, question(&fx)).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "epic_has_open_children", "{body}");
    assert_untouched(&fx, &epic, &fx.todo).await;
}

#[tokio::test]
async fn a_question_on_an_epic_should_block_it() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let epic = card(&fx, json!({"kind": "epic"})).await;

    let (status, body) = ask(&fx, &epic, question(&fx)).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let after = detail(&fx, &epic).await;
    assert_eq!(after["column_id"], json!(fx.blocked), "{after}");
}

#[tokio::test]
async fn the_browser_answer_should_record_the_choice_move_the_card_and_clear_the_label() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({"labels": ["backend"]})).await;
    let (_, asked) = ask(&fx, &task, question(&fx)).await;
    let comment = asked["id"].as_str().expect("id");

    let (status, answered) = answer(
        &fx,
        Caller::Browser,
        &task,
        comment,
        json!({"choice": 1, "quote": "ignored"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{answered}");
    let stored = &answered["question"]["answer"];
    assert_eq!(stored["choice"], 1);
    assert_eq!(stored["answered_by"], "admin");
    assert_eq!(stored["token_name"], Value::Null);
    assert_eq!(stored["quote"], Value::Null);
    let after = detail(&fx, &task).await;
    assert_eq!(after["column_id"], json!(fx.done), "{after}");
    assert_eq!(after["labels"], json!(["backend"]), "{after}");
    assert_eq!(
        comments(&fx, &task).await[0]["question"]["answer"]["choice"],
        1
    );
}

#[tokio::test]
async fn a_token_answer_should_need_a_quote_and_be_stored_as_relayed() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({})).await;
    let (_, asked) = ask(&fx, &task, question(&fx)).await;
    let comment = asked["id"].as_str().expect("id");

    for body in [json!({"choice": 2}), json!({"choice": 2, "quote": "   "})] {
        let (status, refused) = answer(&fx, Caller::Token, &task, comment, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert!(
            refused["message"]
                .as_str()
                .unwrap_or_default()
                .contains("quote is required"),
            "{refused}"
        );
    }
    let still = detail(&fx, &task).await;
    assert_eq!(still["column_id"], json!(fx.blocked), "{still}");
    assert_eq!(still["labels"], json!(["needs:human"]), "{still}");

    let (status, relayed) = answer(
        &fx,
        Caller::Token,
        &task,
        comment,
        json!({"choice": 2, "quote": " go review it first "}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{relayed}");
    let stored = &relayed["question"]["answer"];
    assert_eq!(stored["choice"], 2);
    assert_eq!(stored["token_name"], "agent-7");
    assert_eq!(stored["quote"], "go review it first");
    assert_eq!(stored["answered_by"], "admin");
    let after = detail(&fx, &task).await;
    assert_eq!(after["column_id"], json!(fx.review), "{after}");
    assert_eq!(after["labels"], json!([]), "{after}");

    let (status, second) = answer(&fx, Caller::Browser, &task, comment, json!({"choice": 1})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{second}");
    let (status, second) = answer(
        &fx,
        Caller::Token,
        &task,
        comment,
        json!({"choice": 1, "quote": "again"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{second}");
    let last = detail(&fx, &task).await;
    assert_eq!(last["column_id"], json!(fx.review), "{last}");
}

#[tokio::test]
async fn an_answer_with_a_bad_choice_or_on_a_plain_comment_should_be_refused() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({})).await;
    let (_, asked) = ask(&fx, &task, question(&fx)).await;
    let comment = asked["id"].as_str().expect("id");
    let (status, plain) = send(
        &fx,
        Caller::Browser,
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"body": "just a note"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plain}");

    for body in [json!({}), json!({"choice": 0}), json!({"choice": 3})] {
        let (status, refused) = answer(&fx, Caller::Browser, &task, comment, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {refused}");
    }
    let (status, refused) = answer(
        &fx,
        Caller::Browser,
        &task,
        plain["id"].as_str().expect("id"),
        json!({"choice": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, refused) =
        answer(&fx, Caller::Browser, &task, "missing", json!({"choice": 1})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    let still = detail(&fx, &task).await;
    assert_eq!(still["column_id"], json!(fx.blocked), "{still}");
    assert_eq!(
        comments(&fx, &task).await[0]["question"]["answer"],
        Value::Null
    );
}

#[tokio::test]
async fn an_answer_should_log_the_same_labels_event_as_a_label_update() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let answered = card(&fx, json!({"labels": ["backend"]})).await;
    let (_, asked) = ask(&fx, &answered, question(&fx)).await;
    let (status, body) = answer(
        &fx,
        Caller::Browser,
        &answered,
        asked["id"].as_str().expect("id"),
        json!({"choice": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let updated = card(&fx, json!({"labels": ["backend"]})).await;
    for labels in [json!(["backend", NEEDS_HUMAN_LABEL]), json!(["backend"])] {
        let (status, body) = send(
            &fx,
            Caller::Browser,
            "PATCH",
            &format!("/api/tasks/{updated}"),
            Some(json!({"labels": labels})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let last_labels_event = |task: &str| {
        row(&fx, task)
            .history
            .into_iter()
            .rev()
            .find(|event| event.kind == HistoryKind::LabelsUpdated)
            .expect("labels event")
    };
    let answer_event = last_labels_event(&answered);
    let update_event = last_labels_event(&updated);
    assert_eq!(answer_event.detail, "Set labels: backend");
    assert_eq!(
        (answer_event.detail, answer_event.from, answer_event.to),
        (update_event.detail, update_event.from, update_event.to)
    );
}

#[tokio::test]
async fn an_answer_into_a_full_column_should_leave_the_question_open() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let full = fx
        .db
        .create_column(&fx.board_id, "Tight", Some(1))
        .expect("column");
    let (status, filler) = send(
        &fx,
        Caller::Browser,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": full, "name": "Filler"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{filler}");
    let task = card(&fx, json!({})).await;
    let mut asked = question(&fx);
    asked["choices"][0]["column_id"] = json!(full);
    let (_, posted) = ask(&fx, &task, asked).await;
    let comment = posted["id"].as_str().expect("id");

    let (status, refused) =
        answer(&fx, Caller::Browser, &task, comment, json!({"choice": 1})).await;

    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let still = detail(&fx, &task).await;
    assert_eq!(still["column_id"], json!(fx.blocked), "{still}");
    assert_eq!(still["labels"], json!(["needs:human"]), "{still}");
    assert_eq!(
        comments(&fx, &task).await[0]["question"]["answer"],
        Value::Null
    );
}

#[tokio::test]
async fn answering_one_of_two_open_questions_should_keep_the_card_blocked() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({})).await;
    let (_, first) = ask(&fx, &task, question(&fx)).await;
    let (status, second) = send(
        &fx,
        Caller::Browser,
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"question": question(&fx)})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let first = first["id"].as_str().expect("id");
    let second = second["id"].as_str().expect("id");

    let (status, answered) = answer(&fx, Caller::Browser, &task, first, json!({"choice": 1})).await;

    assert_eq!(status, StatusCode::OK, "{answered}");
    assert_eq!(answered["question"]["answer"]["choice"], 1);
    let held = detail(&fx, &task).await;
    assert_eq!(held["column_id"], json!(fx.blocked), "{held}");
    assert_eq!(held["labels"], json!(["needs:human"]), "{held}");

    let (status, answered) =
        answer(&fx, Caller::Browser, &task, second, json!({"choice": 2})).await;

    assert_eq!(status, StatusCode::OK, "{answered}");
    let freed = detail(&fx, &task).await;
    assert_eq!(freed["column_id"], json!(fx.review), "{freed}");
    assert_eq!(freed["labels"], json!([]), "{freed}");
}

#[tokio::test]
async fn a_plain_comment_of_kind_question_should_be_refused() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({})).await;

    for kind in ["question", "Question"] {
        let (status, body) = send(
            &fx,
            Caller::Token,
            "POST",
            &format!("/api/tasks/{task}/comments"),
            Some(json!({"body": "no choices", "kind": kind})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("needs a question"),
            "{body}"
        );
    }
    assert_untouched(&fx, &task, &fx.todo).await;
}

async fn blocked_flags(fx: &Fixture) -> Vec<(String, bool)> {
    let (status, columns) = send(
        fx,
        Caller::Browser,
        "GET",
        &format!("/api/boards/{}/columns", fx.board_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{columns}");
    columns
        .as_array()
        .expect("columns")
        .iter()
        .map(|column| {
            (
                column["id"].as_str().expect("id").to_string(),
                column["is_blocked"].as_bool().expect("is_blocked"),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_lead_token_should_set_and_clear_the_blocked_column() {
    let fx = fixture();
    let uri = |id: &str| format!("/api/columns/{id}");

    assert!(blocked_flags(&fx).await.iter().all(|(_, flag)| !flag));

    let (status, body) = send(
        &fx,
        Caller::Lead,
        "PATCH",
        &uri(&fx.blocked),
        Some(json!({"is_blocked": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let flags = blocked_flags(&fx).await;
    assert!(flags.iter().all(|(id, flag)| *flag == (*id == fx.blocked)));

    let (status, body) = send(
        &fx,
        Caller::Lead,
        "PATCH",
        &uri(&fx.review),
        Some(json!({"is_blocked": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        blocked_flags(&fx)
            .await
            .contains(&(fx.blocked.clone(), true))
    );

    let (status, body) = send(
        &fx,
        Caller::Lead,
        "PATCH",
        &uri(&fx.review),
        Some(json!({"is_blocked": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        blocked_flags(&fx)
            .await
            .iter()
            .all(|(id, flag)| *flag == (*id == fx.review))
    );

    let (status, body) = send(
        &fx,
        Caller::Lead,
        "PATCH",
        &uri(&fx.review),
        Some(json!({"is_blocked": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(blocked_flags(&fx).await.iter().all(|(_, flag)| !flag));
}

#[tokio::test]
async fn a_work_token_should_be_refused_the_blocked_column() {
    let fx = fixture();

    let (status, body) = send(
        &fx,
        Caller::Token,
        "PATCH",
        &format!("/api/columns/{}", fx.blocked),
        Some(json!({"is_blocked": true})),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "permission_denied", "{body}");
    assert_eq!(body["permission"], "columns.edit", "{body}");
    assert!(blocked_flags(&fx).await.iter().all(|(_, flag)| !flag));
}

#[tokio::test]
async fn a_question_after_setting_the_blocked_column_by_patch_should_move_the_card_there() {
    let fx = fixture();
    let task = card(&fx, json!({})).await;
    let (status, body) = send(
        &fx,
        Caller::Lead,
        "PATCH",
        &format!("/api/columns/{}", fx.blocked),
        Some(json!({"is_blocked": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(row(&fx, &task).column_id, fx.blocked);
}

#[tokio::test]
async fn question_should_name_held_and_limit_time_with_units_when_an_hours_column_is_full() {
    let fx = fixture();
    let limited = fx
        .db
        .create_column(&fx.board_id, "Held", None)
        .expect("column");
    let (status, patched) = send(
        &fx,
        Caller::Browser,
        "PATCH",
        &format!("/api/columns/{limited}"),
        Some(json!({"wip_limit": 4, "config_json": "{\"wip_limit_type\":\"hours\"}"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    let (status, view) = send(
        &fx,
        Caller::Browser,
        "PUT",
        &format!("/api/boards/{}/config", fx.board_id),
        Some(json!({"blocked_column_id": limited})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let (status, filler) = send(
        &fx,
        Caller::Browser,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": limited, "name": "Filler", "estimate_hours": 3.8333333333})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{filler}");
    let task = card(&fx, json!({"estimate_hours": 1})).await;

    let (status, body) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "wip_limit_reached", "{body}");
    assert_eq!(body["message"], "column WIP limit reached (3h 50m/4h)");
}

async fn locked_card_in(fx: &Fixture, column: &str) -> String {
    let (status, task) = send(
        fx,
        Caller::Browser,
        "POST",
        "/api/tasks",
        Some(json!({"column_id": column, "name": "Locked"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let id = task["id"].as_str().expect("id").to_string();
    fx.db
        .set_task_labels(&id, &[NEEDS_HUMAN_LABEL.to_string()])
        .expect("labels");
    let (status, comment) = send(
        fx,
        Caller::Browser,
        "POST",
        &format!("/api/tasks/{id}/comments"),
        Some(json!({"body": "Hold on"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{comment}");
    id
}

#[tokio::test]
async fn a_token_question_on_a_locked_icebox_card_should_leave_it_in_icebox() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let icebox = fx
        .db
        .create_column(&fx.board_id, "Icebox", None)
        .expect("column");
    let task = locked_card_in(&fx, &icebox).await;
    let before = row(&fx, &task);

    let (status, comment) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::OK, "{comment}");
    assert_eq!(comment["kind"], "question");
    let after = row(&fx, &task);
    assert_eq!(after.column_id, icebox);
    assert_eq!(after.position, before.position);
    assert_eq!(after.labels, [NEEDS_HUMAN_LABEL]);
    assert_eq!(after.comments.len(), 2);
    assert!(
        after
            .history
            .iter()
            .all(|event| event.kind != HistoryKind::Moved)
    );
}

#[tokio::test]
async fn a_token_question_on_a_locked_blocked_card_should_keep_its_position() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = locked_card_in(&fx, &fx.blocked).await;
    let before = row(&fx, &task);

    let (status, comment) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::OK, "{comment}");
    let after = row(&fx, &task);
    assert_eq!(after.column_id, fx.blocked);
    assert_eq!(after.position, before.position);
    assert_eq!(after.labels, [NEEDS_HUMAN_LABEL]);
    assert_eq!(after.comments.len(), 2);
}

#[tokio::test]
async fn a_token_question_on_an_unlabelled_card_should_still_move_it_to_blocked() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = card(&fx, json!({})).await;

    let (status, comment) = ask(&fx, &task, question(&fx)).await;

    assert_eq!(status, StatusCode::OK, "{comment}");
    let after = row(&fx, &task);
    assert_eq!(after.column_id, fx.blocked);
    assert_eq!(after.labels, [NEEDS_HUMAN_LABEL]);
}

#[tokio::test]
async fn a_browser_question_on_a_locked_card_should_still_move_it_to_blocked() {
    let fx = fixture();
    mark_blocked(&fx).await;
    let task = locked_card_in(&fx, &fx.todo).await;

    let (status, comment) = send(
        &fx,
        Caller::Browser,
        "POST",
        &format!("/api/tasks/{task}/comments"),
        Some(json!({"question": question(&fx)})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{comment}");
    assert_eq!(row(&fx, &task).column_id, fx.blocked);
}
