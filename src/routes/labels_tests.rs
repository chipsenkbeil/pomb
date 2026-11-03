use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::passkey_mode;
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::AuthScope;

const MAX_BODY_BYTES: usize = 1 << 22;

struct Fixture {
    _dir: tempfile::TempDir,
    app: Router,
    board: String,
    column: String,
    cookie: String,
    lead: String,
    work: String,
    read: String,
}

#[derive(Copy, Clone)]
enum Who<'a> {
    Session,
    Token(&'a str),
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("labels.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.create_user("alice").expect("alice");
        let mut tokens = [AuthScope::Provision, AuthScope::Work, AuthScope::Read].map(|scope| {
            let (record, token) = new_api_token_record(&owner, "agent", scope, None);
            db.create_api_token(&record).expect("token");
            token
        });
        let cookie = format!(
            "session={}",
            db.create_session(&owner, "").expect("session")
        );
        let board = db.create_board(&owner, "Labels").expect("board");
        db.create_swimlane(&board, "Lane").expect("swimlane");
        let column = db.create_column(&board, "Blocked", None).expect("column");
        let app = router(AppState::new(db, None, passkey_mode()));
        let [lead, work, read] = tokens.each_mut().map(std::mem::take);
        Self {
            _dir: dir,
            app,
            board,
            column,
            cookie,
            lead,
            work,
            read,
        }
    }

    async fn call(
        &self,
        who: Who<'_>,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "pomb.test")
            .header("accept", "application/json");
        builder = match who {
            Who::Token(token) => builder.header("authorization", format!("Bearer {token}")),
            Who::Session => builder
                .header("cookie", &self.cookie)
                .header("sec-fetch-site", "same-origin"),
        };
        let body = match body {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
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
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn settings_uri(&self) -> String {
        format!("/api/boards/{}/label-settings", self.board)
    }

    async fn put(&self, who: Who<'_>, body: Value) -> (StatusCode, String) {
        self.call(who, "PUT", &self.settings_uri(), Some(body))
            .await
    }

    async fn task_with(&self, labels: &[&str]) -> String {
        let body = json!({"column_id": self.column, "name": "Task", "labels": labels});
        let (status, text) = self
            .call(Who::Session, "POST", "/api/tasks", Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
        parse(&text)["id"].as_str().expect("task id").to_string()
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_board_should_start_with_no_highlights() {
    let fx = Fixture::new();
    let (status, text) = fx.call(Who::Session, "GET", &fx.settings_uri(), None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        parse(&text),
        json!({"highlights": [], "show_all": false, "added": []})
    );
}

#[tokio::test]
async fn a_lead_token_should_store_highlights_the_show_all_flag_and_added_labels() {
    let fx = Fixture::new();
    let body = json!({
        "highlights": [{"label": " needs:human ", "color": "#B8461F"}],
        "show_all": true,
        "added": ["idea"],
    });
    let (status, text) = fx.put(Who::Token(&fx.lead), body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let stored = json!({
        "highlights": [{"label": "needs:human", "color": "#b8461f"}],
        "show_all": true,
        "added": ["idea"],
    });
    assert_eq!(parse(&text), stored);
    let (_, text) = fx
        .call(Who::Token(&fx.read), "GET", &fx.settings_uri(), None)
        .await;
    assert_eq!(parse(&text), stored);
}

#[tokio::test]
async fn a_token_without_labels_manage_should_be_refused_and_change_nothing() {
    let fx = Fixture::new();
    for token in [&fx.work, &fx.read] {
        let body = json!({"highlights": [{"label": "x", "color": "#000000"}]});
        let (status, text) = fx.put(Who::Token(token), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert!(text.contains("labels.manage"), "{text}");
    }
    let (_, text) = fx.call(Who::Session, "GET", &fx.settings_uri(), None).await;
    assert_eq!(parse(&text)["highlights"], json!([]));
}

#[tokio::test]
async fn a_bad_color_a_long_label_and_an_unknown_property_should_be_refused() {
    let fx = Fixture::new();
    let bad = [
        json!({"highlights": [{"label": "x", "color": "red"}]}),
        json!({"added": ["x".repeat(51)]}),
        json!({"surprise": true}),
    ];
    for body in bad {
        let (status, text) = fx.put(Who::Session, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
    }
}

#[tokio::test]
async fn an_added_label_should_show_in_the_label_list_with_no_card() {
    let fx = Fixture::new();
    fx.task_with(&["used"]).await;
    let body = json!({"added": ["idea"], "highlights": [{"label": "later", "color": "#2d6cc0"}]});
    let (status, text) = fx.put(Who::Session, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let uri = format!("/api/boards/{}/labels", fx.board);
    let (_, text) = fx.call(Who::Session, "GET", &uri, None).await;
    assert_eq!(parse(&text), json!(["idea", "later", "used"]));
}

#[tokio::test]
async fn the_labels_tab_should_list_each_label_with_its_card_count() {
    let fx = Fixture::new();
    fx.task_with(&["needs:human", "kf-parity"]).await;
    fx.task_with(&["needs:human"]).await;
    let body = json!({
        "highlights": [{"label": "needs:human", "color": "#b8461f"}],
        "show_all": true,
        "added": ["idea"],
    });
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let uri = format!("/b/{}/settings/labels", fx.board);
    let (status, html) = fx.call(Who::Session, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let row = |label: &str, cards: usize| format!("data-label=\"{label}\" data-cards=\"{cards}\"");
    for (label, cards) in [("needs:human", 2), ("kf-parity", 1), ("idea", 0)] {
        assert!(html.contains(&row(label, cards)), "{label}: {html}");
    }
    assert!(html.contains("value=\"#b8461f\""), "{html}");
    assert!(html.contains("id=\"bl-show-all\" checked"), "{html}");
}

impl Fixture {
    async fn board_html(&self) -> String {
        let uri = format!("/b/{}", self.board);
        let (status, html) = self.call(Who::Session, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        html
    }
}

#[tokio::test]
async fn a_board_without_highlights_should_render_no_stripe_flag_or_count() {
    let fx = Fixture::new();
    fx.task_with(&["needs:human"]).await;
    let html = fx.board_html().await;
    for marker in [
        "task-card-hl",
        "card-flag",
        "columnHeader-marks",
        "card-labels-all",
    ] {
        assert!(!html.contains(marker), "{marker}");
    }
}

#[tokio::test]
async fn a_highlighted_label_should_give_its_cards_a_stripe_and_a_flag_and_its_column_a_count() {
    let fx = Fixture::new();
    fx.task_with(&["needs:human"]).await;
    fx.task_with(&["needs:human", "other"]).await;
    fx.task_with(&["other"]).await;
    let body = json!({"highlights": [{"label": "needs:human", "color": "#b8461f"}]});
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let html = fx.board_html().await;
    assert_eq!(html.matches("--hl-stripe: #b8461f").count(), 2, "{html}");
    assert_eq!(html.matches("data-flag-label=\"needs:human\"").count(), 2);
    assert!(html.contains("data-mark-label=\"needs:human\""), "{html}");
    assert!(
        html.contains("<span class=\"columnHeader-markCount\">2</span>"),
        "{html}"
    );
    assert!(!html.contains("card-labels-all"));
}

#[tokio::test]
async fn stripe_should_use_the_first_highlight_color_and_flags_should_show_all() {
    let fx = Fixture::new();
    fx.task_with(&["second", "first"]).await;
    let body = json!({"highlights": [
        {"label": "first", "color": "#2d6cc0"},
        {"label": "second", "color": "#b8461f"},
    ]});
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let html = fx.board_html().await;
    assert_eq!(html.matches("--hl-stripe: #2d6cc0").count(), 1, "{html}");
    assert_eq!(html.matches("--hl-stripe: #b8461f").count(), 0, "{html}");
    assert!(html.contains("data-flag-label=\"first\""));
    assert!(html.contains("data-flag-label=\"second\""));
}

#[tokio::test]
async fn show_all_should_list_every_label_on_a_card_with_the_highlighted_ones_filled() {
    let fx = Fixture::new();
    fx.task_with(&["needs:human", "plain"]).await;
    let body = json!({
        "highlights": [{"label": "needs:human", "color": "#b8461f"}],
        "show_all": true,
    });
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let html = fx.board_html().await;
    assert!(html.contains("card-labels-all"), "{html}");
    assert!(
        html.contains("card-label-hl\" style=\"background: #b8461f\" title=\"Label\">needs:human"),
        "{html}"
    );
    assert!(html.contains("<span class=\"card-label\" title=\"Label\">plain</span>"));
}

#[tokio::test]
async fn a_label_name_should_be_escaped_in_every_attribute_it_reaches() {
    let fx = Fixture::new();
    fx.task_with(&["a\"b<c>"]).await;
    let body = json!({"highlights": [{"label": "a\"b<c>", "color": "#b8461f"}]});
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let html = fx.board_html().await;
    assert!(html.contains("data-mark-label=\""), "{html}");
    assert!(!html.contains("a\"b<c>"), "{html}");
    assert!(html.contains("&lt;c&gt;"), "{html}");
}

#[tokio::test]
async fn a_collapsed_column_should_render_no_count_chips() {
    let fx = Fixture::new();
    fx.task_with(&["needs:human"]).await;
    let body = json!({"highlights": [{"label": "needs:human", "color": "#b8461f"}]});
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let uri = format!("/api/columns/{}", fx.column);
    let (status, text) = fx
        .call(
            Who::Session,
            "PATCH",
            &uri,
            Some(json!({"collapsed": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let html = fx.board_html().await;
    assert!(!html.contains("columnHeader-marks"), "{html}");
}

#[tokio::test]
async fn a_column_without_the_label_should_hide_its_chip() {
    let fx = Fixture::new();
    fx.task_with(&["other"]).await;
    let body = json!({"highlights": [{"label": "needs:human", "color": "#b8461f"}]});
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let html = fx.board_html().await;
    assert!(
        html.contains("<div class=\"columnHeader-marks\" hidden>"),
        "{html}"
    );
    assert!(html.contains("columnHeader-mark\" hidden"), "{html}");
}

#[tokio::test]
async fn a_single_card_render_should_keep_its_stripe_and_flag() {
    let fx = Fixture::new();
    let id = fx.task_with(&["needs:human"]).await;
    let body = json!({"highlights": [{"label": "needs:human", "color": "#b8461f"}]});
    assert_eq!(fx.put(Who::Session, body).await.0, StatusCode::OK);
    let uri = format!("/api/tasks/{id}/card");
    let (status, html) = fx.call(Who::Session, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("--hl-stripe: #b8461f"), "{html}");
    assert!(html.contains("data-flag-label=\"needs:human\""), "{html}");
}
