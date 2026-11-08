use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call, none_mode};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::{Db, MAX_WEBHOOKS_PER_BOARD, NewWebhook};
use crate::models::{AccountId, AuthScope, WebhookEvent, WebhookRow};
use crate::webhook::{Lookup, Prober, is_public_address};

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    admin: AccountId,
    token: String,
    work_token: String,
    board: String,
    column: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("webhooks.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "admin", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let (record, work_token) = new_api_token_record(&admin, "work", AuthScope::Work, None);
        db.create_api_token(&record).expect("token");
        let board = db.create_board(&admin, "Hooks").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            admin,
            token,
            work_token,
            board,
            column,
        }
    }

    async fn send(&self, token: &str, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {token}");
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    async fn admin(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        self.send(&self.token, method, uri, body).await
    }

    async fn create(&self, body: Value) -> Reply {
        let uri = format!("/api/boards/{}/webhooks", self.board);
        self.admin("POST", &uri, Some(body)).await
    }

    async fn list(&self) -> Reply {
        let uri = format!("/api/boards/{}/webhooks", self.board);
        self.admin("GET", &uri, None).await
    }

    async fn patch(&self, id: &str, body: Value) -> Reply {
        self.admin("PATCH", &format!("/api/webhooks/{id}"), Some(body))
            .await
    }
}

fn hook(name: &str) -> Value {
    json!({"name": name, "url": "https://example.com/hook", "events": ["taskCreated"]})
}

fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

#[tokio::test]
async fn webhook_should_be_created_listed_updated_disabled_and_deleted() {
    let fx = Fixture::new();
    let created = fx
        .create(json!({
            "name": " Deploy ",
            "url": "https://example.com/hooks/deploy?x=1",
            "events": ["taskCreated", "commentChanged"],
            "column_id": fx.column,
        }))
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let id = created.body["id"].as_str().expect("id").to_string();
    assert_eq!(created.body["name"], "Deploy");
    assert_eq!(created.body["url"], "https://example.com/hooks/deploy?x=1");
    assert_eq!(
        created.body["events"],
        json!(["taskCreated", "commentChanged"])
    );
    assert_eq!(created.body["column_id"], json!(fx.column));
    assert_eq!(created.body["enabled"], json!(true));
    assert_eq!(created.body["board_id"], json!(fx.board));

    let listed = fx.list().await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.body.as_array().expect("array").len(), 1);
    assert_eq!(listed.body[0]["id"], json!(id));

    let updated = fx
        .patch(
            &id,
            json!({
                "name": "Renamed",
                "url": "http://example.org/other",
                "events": ["taskDeleted", "taskRestored"],
                "column_id": null,
                "enabled": false,
            }),
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.body["name"], "Renamed");
    assert_eq!(updated.body["url"], "http://example.org/other");
    assert_eq!(
        updated.body["events"],
        json!(["taskDeleted", "taskRestored"])
    );
    assert_eq!(updated.body["column_id"], Value::Null);
    assert_eq!(updated.body["enabled"], json!(false));
    assert_eq!(fx.list().await.body[0]["enabled"], json!(false));

    let deleted = fx
        .admin("DELETE", &format!("/api/webhooks/{id}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert!(fx.list().await.body.as_array().expect("array").is_empty());
    let again = fx
        .admin("DELETE", &format!("/api/webhooks/{id}"), None)
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webhook_should_be_enabled_without_a_column_filter_by_default() {
    let fx = Fixture::new();
    let created = fx.create(hook("Plain")).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert_eq!(created.body["enabled"], json!(true));
    assert_eq!(created.body["column_id"], Value::Null);
}

#[tokio::test]
async fn webhook_update_should_leave_unsent_fields_alone() {
    let fx = Fixture::new();
    let created = fx
        .create(with(hook("Keep"), "column_id", json!(fx.column)))
        .await;
    let id = created.body["id"].as_str().expect("id");
    let updated = fx.patch(id, json!({"enabled": false})).await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.body["name"], "Keep");
    assert_eq!(updated.body["url"], "https://example.com/hook");
    assert_eq!(updated.body["events"], json!(["taskCreated"]));
    assert_eq!(updated.body["column_id"], json!(fx.column));
}

#[tokio::test]
async fn secret_should_be_returned_only_by_the_create_response() {
    let fx = Fixture::new();
    let created = fx.create(hook("Signed")).await;
    let secret = created.body["secret"].as_str().expect("secret");
    assert_eq!(secret.len(), 64);
    assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
    let other = fx.create(hook("Other")).await;
    assert_ne!(other.body["secret"], created.body["secret"]);

    let id = created.body["id"].as_str().expect("id");
    let listed = fx.list().await;
    for item in listed.body.as_array().expect("array") {
        assert!(item.get("secret").is_none(), "{item}");
    }
    let updated = fx.patch(id, json!({"name": "Renamed"})).await;
    assert!(updated.body.get("secret").is_none(), "{}", updated.body);
    assert!(!listed.body.to_string().contains(secret));
    assert!(!updated.body.to_string().contains(secret));
    let stored = fx.db.webhook(id).expect("read").expect("webhook");
    assert_eq!(stored.secret, secret);
}

#[tokio::test]
async fn patch_should_not_change_the_secret() {
    let fx = Fixture::new();
    let created = fx.create(hook("Fixed")).await;
    let id = created.body["id"].as_str().expect("id");
    let reply = fx
        .patch(id, json!({"secret": "0".repeat(64), "name": "Renamed"}))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let stored = fx.db.webhook(id).expect("read").expect("webhook");
    assert_eq!(
        stored.secret,
        created.body["secret"].as_str().expect("secret")
    );
}

#[tokio::test]
async fn twenty_first_webhook_should_be_refused_and_the_twentieth_accepted() {
    let fx = Fixture::new();
    for n in 0..MAX_WEBHOOKS_PER_BOARD {
        let reply = fx.create(hook(&format!("Hook {n}"))).await;
        assert_eq!(reply.status, StatusCode::OK, "hook {n}: {}", reply.body);
    }
    let refused = fx.create(hook("One too many")).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.code.as_deref(), Some("quota_exceeded"));
    assert!(
        refused.body["message"]
            .as_str()
            .is_some_and(|message| message.contains("20 webhooks per board")),
        "{}",
        refused.body
    );
    let listed = fx.list().await;
    assert_eq!(
        listed.body.as_array().expect("array").len(),
        MAX_WEBHOOKS_PER_BOARD
    );

    let first = listed.body[0]["id"].as_str().expect("id");
    fx.admin("DELETE", &format!("/api/webhooks/{first}"), None)
        .await;
    let room = fx.create(hook("Back to twenty")).await;
    assert_eq!(room.status, StatusCode::OK, "{}", room.body);
}

#[tokio::test]
async fn the_webhook_limit_should_count_each_board_apart() {
    let fx = Fixture::new();
    for n in 0..MAX_WEBHOOKS_PER_BOARD {
        fx.create(hook(&format!("Hook {n}"))).await;
    }
    let other = fx.db.create_board(&fx.admin, "Other").expect("board");
    let reply = fx
        .admin(
            "POST",
            &format!("/api/boards/{other}/webhooks"),
            Some(hook("Elsewhere")),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn unknown_event_should_be_refused_with_a_clear_message() {
    let fx = Fixture::new();
    let reply = fx
        .create(with(
            hook("Bad"),
            "events",
            json!(["taskCreated", "boardCreated"]),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    let message = reply.body["message"].as_str().expect("message");
    assert!(
        message.contains("unknown variant `boardCreated`"),
        "{message}"
    );
    assert!(message.contains("commentDeleted"), "{message}");
    assert!(fx.list().await.body.as_array().expect("array").is_empty());

    let created = fx.create(hook("Good")).await;
    let id = created.body["id"].as_str().expect("id");
    let reply = fx.patch(id, json!({"events": ["nope"]})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
}

#[tokio::test]
async fn every_listed_event_should_be_accepted() {
    let fx = Fixture::new();
    let events = [
        "taskCreated",
        "taskChanged",
        "taskDeleted",
        "taskRestored",
        "commentCreated",
        "commentChanged",
        "commentDeleted",
    ];
    let reply = fx.create(with(hook("All"), "events", json!(events))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["events"], json!(events));
}

#[tokio::test]
async fn empty_or_missing_events_should_be_refused() {
    let fx = Fixture::new();
    let empty = fx.create(with(hook("Empty"), "events", json!([]))).await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);
    let missing = fx
        .create(json!({"name": "None", "url": "https://example.com/hook"}))
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST, "{}", missing.body);
    let created = fx.create(hook("Good")).await;
    let id = created.body["id"].as_str().expect("id");
    let cleared = fx.patch(id, json!({"events": []})).await;
    assert_eq!(cleared.status, StatusCode::BAD_REQUEST, "{}", cleared.body);
}

#[tokio::test]
async fn repeated_events_should_be_stored_once() {
    let fx = Fixture::new();
    let reply = fx
        .create(with(
            hook("Twice"),
            "events",
            json!(["taskChanged", "taskCreated", "taskChanged"]),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["events"], json!(["taskChanged", "taskCreated"]));
}

#[tokio::test]
async fn url_should_be_refused_unless_it_is_http_or_https_without_credentials() {
    let fx = Fixture::new();
    for url in [
        "ftp://example.com/hook",
        "javascript:alert(1)",
        "file:///etc/passwd",
        "example.com/hook",
        "not a url",
        "",
        "https://user:pass@example.com/hook",
        "https://user@example.com/hook",
    ] {
        let reply = fx.create(with(hook("Bad"), "url", json!(url))).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{url}: {}",
            reply.body
        );
    }
    let long = format!("https://example.com/{}", "a".repeat(2048));
    let reply = fx.create(with(hook("Long"), "url", json!(long))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    let missing = fx
        .create(json!({"name": "No url", "events": ["taskCreated"]}))
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST, "{}", missing.body);
    assert!(fx.list().await.body.as_array().expect("array").is_empty());

    let created = fx.create(hook("Good")).await;
    let id = created.body["id"].as_str().expect("id");
    let reply = fx.patch(id, json!({"url": "ftp://example.com"})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    let stored = fx.db.webhook(id).expect("read").expect("webhook");
    assert_eq!(stored.url, "https://example.com/hook");
}

#[tokio::test]
async fn url_of_exactly_2048_characters_should_be_accepted() {
    let fx = Fixture::new();
    let prefix = "https://example.com/";
    let url = format!("{prefix}{}", "a".repeat(2048 - prefix.len()));
    let reply = fx.create(with(hook("Edge"), "url", json!(url))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn name_should_be_trimmed_and_between_1_and_100_characters() {
    let fx = Fixture::new();
    for name in ["", "   ", &"n".repeat(101)] {
        let reply = fx.create(with(hook("x"), "name", json!(name))).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{name:?}: {}",
            reply.body
        );
    }
    let missing = fx
        .create(json!({"url": "https://example.com/hook", "events": ["taskCreated"]}))
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST, "{}", missing.body);

    let longest = "n".repeat(100);
    let reply = fx.create(with(hook("x"), "name", json!(longest))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let id = reply.body["id"].as_str().expect("id");
    let blank = fx.patch(id, json!({"name": "  "})).await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST, "{}", blank.body);
    let trimmed = fx.patch(id, json!({"name": "  Padded  "})).await;
    assert_eq!(trimmed.body["name"], "Padded");
}

#[tokio::test]
async fn column_of_another_board_should_be_refused() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Other").expect("board");
    let foreign = fx
        .db
        .create_column(&other, "Elsewhere", None)
        .expect("column");
    let reply = fx
        .create(with(hook("Wrong"), "column_id", json!(foreign)))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);

    let created = fx.create(hook("Right")).await;
    let id = created.body["id"].as_str().expect("id");
    let reply = fx.patch(id, json!({"column_id": foreign})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    let stored = fx.db.webhook(id).expect("read").expect("webhook");
    assert_eq!(stored.column_id, None);
}

#[tokio::test]
async fn unknown_column_should_look_the_same_as_a_column_of_another_board() {
    let fx = Fixture::new();
    let other = fx.db.create_board(&fx.admin, "Other").expect("board");
    let foreign = fx
        .db
        .create_column(&other, "Elsewhere", None)
        .expect("column");
    let foreign_reply = fx
        .create(with(hook("A"), "column_id", json!(foreign)))
        .await;
    let absent = uuid::Uuid::new_v4().to_string();
    let absent_reply = fx.create(with(hook("A"), "column_id", json!(absent))).await;
    assert_eq!(foreign_reply.status, absent_reply.status);
    assert_eq!(foreign_reply.body, absent_reply.body);
}

#[tokio::test]
async fn column_filter_should_be_settable_and_clearable_by_patch() {
    let fx = Fixture::new();
    let created = fx.create(hook("Filtered")).await;
    let id = created.body["id"].as_str().expect("id");
    let set = fx.patch(id, json!({"column_id": fx.column})).await;
    assert_eq!(set.body["column_id"], json!(fx.column));
    let cleared = fx.patch(id, json!({"column_id": null})).await;
    assert_eq!(cleared.body["column_id"], Value::Null);
}

#[tokio::test]
async fn missing_board_and_webhook_should_be_404() {
    let fx = Fixture::new();
    let absent = uuid::Uuid::new_v4().to_string();
    let created = fx
        .admin(
            "POST",
            &format!("/api/boards/{absent}/webhooks"),
            Some(hook("Lost")),
        )
        .await;
    assert_eq!(created.status, StatusCode::NOT_FOUND);
    let listed = fx
        .admin("GET", &format!("/api/boards/{absent}/webhooks"), None)
        .await;
    assert_eq!(listed.status, StatusCode::NOT_FOUND);
    let patched = fx.patch(&absent, json!({"enabled": false})).await;
    assert_eq!(patched.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webhooks_should_need_the_webhooks_manage_permission() {
    let fx = Fixture::new();
    let created = fx.create(hook("Guarded")).await;
    let id = created.body["id"].as_str().expect("id");
    let board_uri = format!("/api/boards/{}/webhooks", fx.board);
    let hook_uri = format!("/api/webhooks/{id}");
    for (method, uri, body) in [
        ("GET", board_uri.as_str(), None),
        ("POST", board_uri.as_str(), Some(hook("Sneaky"))),
        ("PATCH", hook_uri.as_str(), Some(json!({"enabled": false}))),
        ("DELETE", hook_uri.as_str(), None),
    ] {
        let reply = fx.send(&fx.work_token, method, uri, body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.code.as_deref(), Some("permission_denied"));
        assert_eq!(reply.body["permission"], "webhooks.manage");
    }
    let stored = fx.db.webhook(id).expect("read").expect("webhook");
    assert!(stored.enabled);
    assert_eq!(fx.list().await.body.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn deleting_a_board_should_remove_its_webhooks() {
    let fx = Fixture::new();
    let created = fx.create(hook("Doomed")).await;
    let id = created.body["id"].as_str().expect("id").to_string();
    let spared_board = fx.db.create_board(&fx.admin, "Spared").expect("board");
    let spared = fx
        .admin(
            "POST",
            &format!("/api/boards/{spared_board}/webhooks"),
            Some(hook("Kept")),
        )
        .await;
    let spared_id = spared.body["id"].as_str().expect("id");

    let deleted = fx
        .admin("DELETE", &format!("/api/boards/{}", fx.board), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    assert!(fx.db.webhook(&id).expect("read").is_none());
    assert!(fx.db.list_webhooks(&fx.board).expect("list").is_empty());
    assert!(fx.db.webhook(spared_id).expect("read").is_some());
}

#[tokio::test]
async fn webhooks_should_not_appear_in_the_board_archive() {
    let fx = Fixture::new();
    let created = fx.create(hook("Private")).await;
    let secret = created.body["secret"].as_str().expect("secret");
    let archive = fx
        .admin(
            "GET",
            &format!("/api/boards/{}/export-data", fx.board),
            None,
        )
        .await;
    assert_eq!(archive.status, StatusCode::OK, "{}", archive.body);
    assert!(!archive.body.to_string().contains(secret));
    assert!(!archive.body.to_string().contains("example.com/hook"));
}

#[tokio::test]
async fn deleting_a_column_should_clear_and_disable_the_webhooks_that_filter_on_it() {
    let fx = Fixture::new();
    let doomed = fx
        .db
        .create_column(&fx.board, "Doomed", None)
        .expect("column");
    let filtered = fx
        .create(with(hook("Filtered"), "column_id", json!(doomed)))
        .await;
    let kept = fx
        .create(with(hook("Kept"), "column_id", json!(fx.column)))
        .await;
    let deleted = fx
        .admin("DELETE", &format!("/api/columns/{doomed}"), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);

    let filtered = fx
        .db
        .webhook(filtered.body["id"].as_str().expect("id"))
        .expect("read")
        .expect("webhook");
    assert_eq!(filtered.column_id, None);
    assert!(!filtered.enabled);
    let kept = fx
        .db
        .webhook(kept.body["id"].as_str().expect("id"))
        .expect("read")
        .expect("webhook");
    assert_eq!(kept.column_id.as_deref(), Some(fx.column.as_str()));
    assert!(kept.enabled);
}

#[tokio::test]
async fn webhook_should_not_be_stored_on_a_board_that_is_gone() {
    let fx = Fixture::new();
    let absent = uuid::Uuid::new_v4().to_string();
    let refused = fx.db.create_webhook(NewWebhook {
        board_id: absent.clone(),
        name: "Lost".to_string(),
        url: "https://example.com/hook".to_string(),
        events: vec![WebhookEvent::TaskCreated],
        column_id: None,
        enabled: true,
    });
    assert!(refused.is_err());
    assert!(fx.db.list_webhooks(&absent).expect("list").is_empty());
}

#[tokio::test]
async fn url_should_be_refused_when_its_normalized_form_passes_2048_characters() {
    let fx = Fixture::new();
    let url = format!("https://example.com/{}", "é".repeat(700));
    let reply = fx.create(with(hook("Encoded"), "url", json!(url))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
}

#[tokio::test]
async fn debug_output_should_redact_the_secret() {
    let fx = Fixture::new();
    let created = fx.create(hook("Quiet")).await;
    let secret = created.body["secret"].as_str().expect("secret");
    let stored = fx
        .db
        .webhook(created.body["id"].as_str().expect("id"))
        .expect("read")
        .expect("webhook");
    let shown = format!("{stored:?}");
    assert!(!shown.contains(secret), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
}

const REFUSED_URLS: [&str; 22] = [
    "http://localhost/hook",
    "http://LOCALHOST:8080/hook",
    "http://api.localhost/hook",
    "http://127.0.0.1/hook",
    "http://127.1/hook",
    "http://0x7f.1/hook",
    "http://0177.0.0.1/hook",
    "http://2130706433/hook",
    "http://0/hook",
    "http://[::1]/hook",
    "http://[::ffff:127.0.0.1]/hook",
    "http://169.254.169.254/latest",
    "http://[fe80::1]/hook",
    "http://10.0.0.5/hook",
    "http://172.16.0.1/hook",
    "http://192.168.1.1/hook",
    "http://[fd00::1]/hook",
    "http://100.64.0.1/hook",
    "http://198.18.0.1/hook",
    "http://224.0.0.1/hook",
    "http://255.255.255.255/hook",
    "https://[64:ff9b::a00:1]/hook",
];

#[tokio::test]
async fn create_webhook_should_refuse_every_non_public_address_class() {
    let fx = Fixture::new();
    for url in REFUSED_URLS {
        let reply = fx.create(with(hook("Bad"), "url", json!(url))).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{url}: {}",
            reply.body
        );
        assert!(
            reply.body.to_string().contains("public host"),
            "{url}: {}",
            reply.body
        );
    }
    let listed = fx.list().await;
    assert_eq!(listed.body.as_array().expect("array").len(), 0);
}

#[tokio::test]
async fn update_webhook_should_refuse_every_non_public_address_class() {
    let fx = Fixture::new();
    let created = fx.create(hook("Good")).await;
    let id = created.body["id"].as_str().expect("id").to_string();
    for url in REFUSED_URLS {
        let reply = fx.patch(&id, json!({"url": url})).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{url}: {}",
            reply.body
        );
    }
    let listed = fx.list().await;
    assert_eq!(listed.body[0]["url"], "https://example.com/hook");
}

#[tokio::test]
async fn create_webhook_should_accept_public_addresses_and_names() {
    let fx = Fixture::new();
    for url in [
        "https://example.com/hook",
        "http://8.8.8.8/hook",
        "https://[2606:4700:4700::1111]/hook",
        "http://notlocalhost.example/hook",
    ] {
        let reply = fx.create(with(hook("Fine"), "url", json!(url))).await;
        assert_eq!(reply.status, StatusCode::OK, "{url}: {}", reply.body);
    }
}

const SAME_ORIGIN: [(&str, &str); 1] = [("sec-fetch-site", "same-origin")];

fn allow_all(_: IpAddr) -> bool {
    true
}

fn resolving_to(addresses: Vec<[u8; 4]>) -> Lookup {
    Arc::new(move |_, port| {
        Ok(addresses
            .iter()
            .map(|&octets| SocketAddr::from((octets, port)))
            .collect())
    })
}

fn nothing_resolves() -> Lookup {
    Arc::new(|_, _| Err(io::Error::from(io::ErrorKind::NotFound)))
}

struct Callback {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Callback {
    fn start(response: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let request = read_request(&stream);
                shared.lock().expect("lock").push(request);
                let mut stream = stream;
                let reply = format!(
                    "HTTP/1.1 {response}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Self { port, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }
}

fn read_request(mut stream: &TcpStream) -> String {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).expect("read");
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&raw).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if body.len() >= length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

fn header_of<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request
        .split("\r\n\r\n")
        .next()?
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

struct Browser {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    board: String,
    column: String,
    token: String,
}

impl Browser {
    fn new(prober: Option<Prober>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("browser.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.ensure_owner("owner").expect("owner").account_id();
        let (record, token) = new_api_token_record(&owner, "admin", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let board = db.create_board(&owner, "Hooks").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let mut state =
            AppState::new(db.clone(), None, none_mode(None)).with_limits(unthrottled_limits());
        if let Some(prober) = prober {
            state = state.with_prober(prober);
        }
        Self {
            _dir: dir,
            app: router(state),
            db,
            board,
            column,
            token,
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        call(&self.app, "GET", uri, &[], None).await
    }

    async fn post(&self, uri: &str, body: Value) -> Reply {
        call(&self.app, "POST", uri, &SAME_ORIGIN, Some(body)).await
    }

    async fn as_token(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {}", self.token);
        call(
            &self.app,
            method,
            uri,
            &[("authorization", &bearer), SAME_ORIGIN[0]],
            body,
        )
        .await
    }

    fn webhook(&self, name: &str, column: Option<&str>, enabled: bool) -> WebhookRow {
        self.db
            .create_webhook(NewWebhook {
                board_id: self.board.clone(),
                name: name.to_string(),
                url: format!("https://example.com/{name}"),
                events: vec![WebhookEvent::TaskCreated, WebhookEvent::TaskRestored],
                column_id: column.map(str::to_string),
                enabled,
            })
            .expect("webhook")
    }

    fn uri(&self, tail: &str) -> String {
        format!("/api/boards/{}/webhooks/{tail}", self.board)
    }

    fn page(&self, tail: &str) -> String {
        format!("/b/{}/settings/api-webhooks{tail}", self.board)
    }
}

fn html_of(reply: &Reply) -> &str {
    reply.body.as_str().expect("html")
}

#[tokio::test]
async fn settings_page_should_list_each_webhook_with_its_column_events_and_state() {
    let fx = Browser::new(None);
    let filtered = fx.webhook("filtered", Some(&fx.column), true);
    let plain = fx.webhook("plain", None, false);
    let page = fx.get(&fx.page("")).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = html_of(&page);
    assert!(html.contains(&format!("data-webhook-id=\"{}\"", filtered.id)));
    assert!(html.contains(&format!("data-webhook-id=\"{}\"", plain.id)));
    assert!(html.contains("https://example.com/filtered"));
    assert!(html.contains("Task created, Task restored"));
    assert!(html.contains("Todo"));
    assert!(html.contains("All columns"));
    assert!(html.contains("Enabled"));
    assert!(html.contains("Disabled"));
    assert!(html.contains("Show secret"));
    assert!(html.contains("id=\"wh-add\""));
    assert!(html.contains(&format!("/settings/api-webhooks/{}\"", plain.id)));
    assert!(html.contains("data-page=\"board-settings-webhooks\""));
    assert!(html.contains("id=\"wh-delete-overlay\""));
    assert!(!html.contains("does not support outgoing webhooks"));
}

#[tokio::test]
async fn settings_page_should_show_the_docs_button_and_the_token_list_above_the_webhooks() {
    let fx = Browser::new(None);
    let page = fx.get(&fx.page("")).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = html_of(&page);
    assert!(html.contains("id=\"api-docs\""));
    assert!(html.contains("href=\"/docs/api.md\""));
    assert!(html.contains("id=\"tk-add\""));
    for class in ["tk-show", "tk-edit", "tk-delete"] {
        assert!(html.contains(class), "{class}");
    }
    assert!(html.contains("id=\"tk-edit-name\""));
    assert!(!html.contains("tk-edit-child-hours"));
    let tokens = html.find("Personal tokens").expect("tokens heading");
    let webhooks = html.find("<h3>Webhooks</h3>").expect("webhooks heading");
    assert!(tokens < webhooks);
    assert!(html.contains("data-name=\"admin\""));
    assert!(!html.contains(&fx.token), "the secret is never rendered");
}

#[tokio::test]
async fn settings_page_should_say_so_when_the_board_has_no_webhook() {
    let fx = Browser::new(None);
    let page = fx.get(&fx.page("")).await;
    assert!(html_of(&page).contains("id=\"wh-empty\""));
}

#[tokio::test]
async fn add_page_should_render_the_general_and_filter_fields() {
    let fx = Browser::new(None);
    let page = fx.get(&fx.page("/new")).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = html_of(&page);
    assert!(html.contains("Add webhook"));
    for id in [
        "wh-name",
        "wh-url",
        "wh-test-dns",
        "wh-test-request",
        "wh-column",
        "wh-save",
        "wh-tab-general",
        "wh-tab-filter",
        "wh-secret-overlay",
    ] {
        assert!(html.contains(&format!("id=\"{id}\"")), "{id}");
    }
    for event in WebhookEvent::ALL {
        assert!(
            html.contains(&format!("name=\"event\" value=\"{}\"", event.name())),
            "{}",
            event.name()
        );
        assert!(html.contains(event.label()), "{}", event.label());
    }
    assert!(html.contains("value=\"taskRestored\""));
    assert!(html.contains("<option value=\"\" selected>All columns</option>"));
    assert!(html.contains(&format!("<option value=\"{}\">Todo</option>", fx.column)));
    assert!(!html.contains(" checked>"));
    assert!(html.contains("data-page=\"board-settings-webhook-form\""));
}

#[tokio::test]
async fn edit_page_should_prefill_the_webhook() {
    let fx = Browser::new(None);
    let hook = fx.webhook("deploy", Some(&fx.column), true);
    let page = fx.get(&fx.page(&format!("/{}", hook.id))).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = html_of(&page);
    assert!(html.contains("Edit webhook"));
    assert!(html.contains("value=\"deploy\""));
    assert!(html.contains("value=\"https://example.com/deploy\""));
    assert!(html.contains(&format!("data-webhook-id=\"{}\"", hook.id)));
    assert!(html.contains("value=\"taskCreated\" checked>"));
    assert!(html.contains("value=\"taskRestored\" checked>"));
    assert!(html.contains("value=\"taskChanged\">"));
    assert_eq!(html.matches(" checked>").count(), 2);
    assert!(html.contains(&format!(
        "<option value=\"{}\" selected>Todo</option>",
        fx.column
    )));
}

#[tokio::test]
async fn edit_page_should_look_missing_for_a_webhook_of_another_board() {
    let fx = Browser::new(None);
    let owner = fx.db.owner().expect("read").expect("owner").account_id();
    let other = fx.db.create_board(&owner, "Other").expect("board");
    let hook = fx
        .db
        .create_webhook(NewWebhook {
            board_id: other,
            name: "elsewhere".to_string(),
            url: "https://example.com/e".to_string(),
            events: vec![WebhookEvent::TaskCreated],
            column_id: None,
            enabled: true,
        })
        .expect("webhook");
    let wrong = fx.get(&fx.page(&format!("/{}", hook.id))).await;
    let absent = fx.get(&fx.page("/no-such-webhook")).await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND);
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn secret_route_should_return_the_secret_to_a_cookie_session() {
    let fx = Browser::new(None);
    let hook = fx.webhook("signed", None, true);
    let reply = fx.get(&format!("/api/webhooks/{}/secret", hook.id)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["secret"], json!(hook.secret));
    assert_eq!(reply.body.as_object().expect("object").len(), 1);
}

#[tokio::test]
async fn secret_route_should_refuse_a_bearer_token() {
    let fx = Browser::new(None);
    let hook = fx.webhook("signed", None, true);
    let reply = fx
        .as_token("GET", &format!("/api/webhooks/{}/secret", hook.id), None)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.code.as_deref(), Some("session_required"));
    assert!(!reply.body.to_string().contains(&hook.secret));
}

#[tokio::test]
async fn secret_route_should_refuse_another_accounts_webhook() {
    let fx = Browser::new(None);
    let hook = fx.webhook("signed", None, true);
    let other = fx.db.create_user("other").expect("user");
    let (record, token) = new_api_token_record(&other, "other", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");
    let bearer = format!("Bearer {token}");
    let theirs = call(
        &fx.app,
        "GET",
        &format!("/api/webhooks/{}/secret", hook.id),
        &[("authorization", &bearer)],
        None,
    )
    .await;
    let absent = call(
        &fx.app,
        "GET",
        "/api/webhooks/no-such-webhook/secret",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND, "{}", theirs.body);
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    assert_eq!(theirs.body["message"], absent.body["message"]);
    assert!(!theirs.body.to_string().contains(&hook.secret));
}

#[tokio::test]
async fn secret_should_appear_nowhere_else_in_the_pages_or_the_api() {
    let fx = Browser::new(None);
    let created = fx
        .as_token(
            "POST",
            &format!("/api/boards/{}/webhooks", fx.board),
            Some(hook("Shown once")),
        )
        .await;
    let secret = created.body["secret"].as_str().expect("secret").to_string();
    let id = created.body["id"].as_str().expect("id").to_string();
    for uri in [
        fx.page(""),
        fx.page("/new"),
        fx.page(&format!("/{id}")),
        format!("/b/{}/settings", fx.board),
        format!("/b/{}", fx.board),
    ] {
        let page = fx.get(&uri).await;
        assert_eq!(page.status, StatusCode::OK, "{uri}");
        assert!(!html_of(&page).contains(&secret), "{uri}");
    }
    let listed = fx
        .as_token("GET", &format!("/api/boards/{}/webhooks", fx.board), None)
        .await;
    assert!(!listed.body.to_string().contains(&secret));
}

#[tokio::test]
async fn send_test_should_deliver_an_unsigned_body_and_show_the_status() {
    let callback = Callback::start("200 OK");
    let prober = Prober::with_parts(
        allow_all,
        resolving_to(vec![[127, 0, 0, 1]]),
        Duration::from_secs(5),
    );
    let fx = Browser::new(Some(prober));
    let url = format!("http://hook.test:{}/hook", callback.port);
    let reply = fx
        .post(&fx.uri("test-request"), json!({ "url": url }))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["status"], json!(200));
    assert_eq!(reply.body["error"], Value::Null);

    let seen = callback.requests();
    assert_eq!(seen.len(), 1);
    let request = &seen[0];
    assert!(request.starts_with("POST /hook "), "{request}");
    assert_eq!(header_of(request, "content-type"), Some("application/json"));
    assert_eq!(header_of(request, "user-agent"), Some("Pomb"));
    assert_eq!(header_of(request, "x-pomb-event"), Some("test"));
    assert_eq!(header_of(request, "x-pomb-signature"), None);
    let body: Value =
        serde_json::from_str(request.split("\r\n\r\n").nth(1).expect("body")).expect("json");
    assert_eq!(body["eventType"], "test");
    assert_eq!(body["boardId"], json!(fx.board));
    assert!(
        chrono::DateTime::parse_from_rfc3339(body["timestamp"].as_str().expect("time")).is_ok()
    );
    assert_eq!(body.as_object().expect("object").len(), 3);
}

#[tokio::test]
async fn send_test_should_show_the_status_a_failing_callback_answers_with() {
    let callback = Callback::start("503 Service Unavailable");
    let prober = Prober::with_parts(
        allow_all,
        resolving_to(vec![[127, 0, 0, 1]]),
        Duration::from_secs(5),
    );
    let fx = Browser::new(Some(prober));
    let url = format!("http://hook.test:{}/hook", callback.port);
    let reply = fx
        .post(&fx.uri("test-request"), json!({ "url": url }))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["status"], json!(503));
}

#[tokio::test]
async fn send_test_should_not_follow_a_redirect() {
    let callback = Callback::start("302 Found\r\nLocation: /other");
    let prober = Prober::with_parts(
        allow_all,
        resolving_to(vec![[127, 0, 0, 1]]),
        Duration::from_secs(5),
    );
    let fx = Browser::new(Some(prober));
    let url = format!("http://hook.test:{}/hook", callback.port);
    let reply = fx
        .post(&fx.uri("test-request"), json!({ "url": url }))
        .await;
    assert_eq!(reply.body["status"], json!(302), "{}", reply.body);
    let seen = callback.requests();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].starts_with("POST /hook "));
}

#[tokio::test]
async fn send_test_should_refuse_a_loopback_url_under_the_real_policy() {
    let callback = Callback::start("200 OK");
    let fx = Browser::new(None);
    let literal = format!("http://127.0.0.1:{}/hook", callback.port);
    let reply = fx
        .post(&fx.uri("test-request"), json!({ "url": literal }))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);

    let rebinding = Browser::new(Some(Prober::with_parts(
        is_public_address,
        resolving_to(vec![[127, 0, 0, 1]]),
        Duration::from_secs(5),
    )));
    let named = format!("http://internal.test:{}/hook", callback.port);
    let reply = rebinding
        .post(&rebinding.uri("test-request"), json!({ "url": named }))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["status"], Value::Null);
    assert_eq!(reply.body["error"], "refused: not a public address");
    assert!(!reply.body.to_string().contains("127.0.0.1"));
    assert!(callback.requests().is_empty());
}

#[tokio::test]
async fn send_test_should_say_it_could_not_connect_without_resolution_detail() {
    let fx = Browser::new(Some(Prober::with_parts(
        allow_all,
        nothing_resolves(),
        Duration::from_secs(5),
    )));
    let reply = fx
        .post(
            &fx.uri("test-request"),
            json!({ "url": "https://gone.example.com/hook" }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["error"], "could not connect");
    assert_eq!(reply.body["status"], Value::Null);
}

#[tokio::test]
async fn send_test_should_time_out_on_a_callback_that_never_answers() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let held = thread::spawn(move || {
        let accepted = listener.accept();
        thread::sleep(Duration::from_secs(2));
        drop(accepted);
    });
    let fx = Browser::new(Some(Prober::with_parts(
        allow_all,
        resolving_to(vec![[127, 0, 0, 1]]),
        Duration::from_millis(400),
    )));
    let reply = fx
        .post(
            &fx.uri("test-request"),
            json!({ "url": format!("http://slow.test:{port}/hook") }),
        )
        .await;
    assert_eq!(reply.body["error"], "timed out", "{}", reply.body);
    held.join().expect("listener thread");
}

#[tokio::test]
async fn probe_routes_should_validate_the_url_like_creation_does() {
    let fx = Browser::new(None);
    for uri in [fx.uri("test-request"), fx.uri("test-dns")] {
        for body in [
            json!({}),
            json!({ "url": "" }),
            json!({ "url": "ftp://example.com/x" }),
            json!({ "url": "https://user:pw@example.com/x" }),
            json!({ "url": "http://localhost/x" }),
            json!({ "url": "http://10.0.0.5/x" }),
        ] {
            let reply = fx.post(&uri, body.clone()).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri} {body}");
        }
    }
}

#[tokio::test]
async fn probe_routes_should_refuse_a_bearer_token_and_send_nothing() {
    let callback = Callback::start("200 OK");
    let fx = Browser::new(Some(Prober::with_parts(
        allow_all,
        resolving_to(vec![[127, 0, 0, 1]]),
        Duration::from_secs(5),
    )));
    let url = format!("http://hook.test:{}/hook", callback.port);
    for tail in ["test-request", "test-dns"] {
        let reply = fx
            .as_token("POST", &fx.uri(tail), Some(json!({ "url": url })))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{tail}: {}",
            reply.body
        );
        assert_eq!(reply.code.as_deref(), Some("session_required"), "{tail}");
    }
    assert!(callback.requests().is_empty());
}

#[tokio::test]
async fn dns_check_should_report_each_verdict_without_leaking_addresses() {
    let cases = [
        (
            vec![[8, 8, 8, 8]],
            true,
            "Resolves to public addresses only",
        ),
        (
            vec![[8, 8, 8, 8], [10, 1, 2, 3]],
            false,
            "Resolves to a non-public address",
        ),
        (
            vec![[127, 0, 0, 1]],
            false,
            "Resolves to a non-public address",
        ),
        (vec![], false, "Does not resolve"),
    ];
    for (addresses, ok, message) in cases {
        let fx = Browser::new(Some(Prober::with_parts(
            is_public_address,
            resolving_to(addresses),
            Duration::from_secs(5),
        )));
        let reply = fx
            .post(
                &fx.uri("test-dns"),
                json!({ "url": "https://hook.example.com/x" }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.body["ok"], json!(ok), "{message}");
        assert_eq!(reply.body["message"], json!(message));
        assert_eq!(reply.body.as_object().expect("object").len(), 2);
        let text = reply.body.to_string();
        for leaked in ["8.8.8.8", "10.1.2.3", "127.0.0.1"] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }
    let fx = Browser::new(Some(Prober::with_parts(
        is_public_address,
        nothing_resolves(),
        Duration::from_secs(5),
    )));
    let reply = fx
        .post(
            &fx.uri("test-dns"),
            json!({ "url": "https://gone.example.com/x" }),
        )
        .await;
    assert_eq!(reply.body["message"], "Does not resolve");
}

#[tokio::test]
async fn probe_routes_should_answer_429_while_both_slots_are_busy() {
    let lookup: Lookup = Arc::new(|_, port| {
        thread::sleep(Duration::from_millis(1500));
        Ok(vec![SocketAddr::from(([8, 8, 8, 8], port))])
    });
    let fx = Browser::new(Some(Prober::with_parts(
        is_public_address,
        lookup,
        Duration::from_secs(5),
    )));
    let body = json!({ "url": "https://hook.example.com/x" });
    let uri = fx.uri("test-dns");
    let slow = || call(&fx.app, "POST", &uri, &SAME_ORIGIN, Some(body.clone()));
    let probing = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut refused = Vec::new();
        for tail in ["test-dns", "test-request"] {
            refused.push((tail, fx.post(&fx.uri(tail), body.clone()).await));
        }
        refused
    };
    let (first, second, refused) = tokio::join!(slow(), slow(), probing);
    for (tail, busy) in refused {
        assert_eq!(
            busy.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{tail}: {}",
            busy.body
        );
        assert_eq!(busy.code.as_deref(), Some("busy"), "{tail}");
    }
    for reply in [first, second] {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }
    let after = fx.post(&fx.uri("test-dns"), body).await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.body);
}

#[tokio::test]
async fn probe_routes_should_look_missing_for_a_board_of_another_account() {
    let fx = Browser::new(None);
    let other = fx.db.create_user("other").expect("user");
    let (record, token) = new_api_token_record(&other, "other", AuthScope::Admin, None);
    fx.db.create_api_token(&record).expect("token");
    let bearer = format!("Bearer {token}");
    for tail in ["test-request", "test-dns"] {
        let reply = call(
            &fx.app,
            "POST",
            &fx.uri(tail),
            &[("authorization", &bearer), SAME_ORIGIN[0]],
            Some(json!({ "url": "https://example.com/x" })),
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{tail}: {}",
            reply.body
        );
    }
}
