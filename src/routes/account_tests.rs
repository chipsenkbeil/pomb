use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::passkey_tests::{Reply, call};
use super::server::{passkey_mode, unthrottled_limits};
use super::{MAX_AVATAR_BYTES, router};
use crate::AppState;
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, AuthScope, AvatarKind, InstanceMode, TimeFormat};

const MAX_BODY_BYTES: usize = 1 << 20;

/// An instance with an `admin` account that owns a board and an `agent-1` token.
struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    app: Router,
    admin: AccountId,
    board: String,
    todo: String,
    token: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("account.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let admin = db.create_user("admin").expect("admin");
        let (record, token) = new_api_token_record(&admin, "agent-1", AuthScope::Admin, None);
        db.create_api_token(&record).expect("token");
        let board = db.create_board(&admin, "Work").expect("board");
        let todo = db.create_column(&board, "Todo", None).expect("column");
        let state = AppState::new(db.clone(), None, passkey_mode())
            .with_mode(InstanceMode::Multi)
            .with_limits(unthrottled_limits());
        Self {
            app: router(state),
            _dir: dir,
            db,
            admin,
            board,
            todo,
            token,
        }
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let auth = format!("Bearer {}", self.token);
        call(&self.app, method, uri, &[("authorization", &auth)], body).await
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let reply = self.call(method, uri, body).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{method} {uri}: {}",
            reply.body
        );
        reply
    }

    /// Sends `bytes` to `/api/avatar` as `content_type`.
    async fn upload(&self, content_type: &str, bytes: Vec<u8>) -> StatusCode {
        let request = axum::http::Request::builder()
            .method("PUT")
            .uri("/api/avatar")
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", content_type)
            .body(Body::from(bytes))
            .expect("request");
        self.app
            .clone()
            .oneshot(request)
            .await
            .expect("oneshot")
            .status()
    }

    /// Returns the status, the content type and the bytes of `GET /api/avatar`.
    async fn avatar(&self) -> (StatusCode, Option<String>, Vec<u8>) {
        let request = axum::http::Request::builder()
            .uri("/api/avatar")
            .header("authorization", format!("Bearer {}", self.token))
            .body(Body::empty())
            .expect("request");
        let response = self.app.clone().oneshot(request).await.expect("oneshot");
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|value| value.to_str().expect("ascii").to_string());
        let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .expect("body");
        (status, content_type, bytes.to_vec())
    }

    /// Returns the page at `uri` as text.
    async fn page(&self, uri: &str) -> String {
        let reply = self.send("GET", uri, None).await;
        reply.body.as_str().expect("html").to_string()
    }

    /// Returns a task the admin is a member of.
    fn assigned_task(&self) -> String {
        let task = self
            .db
            .create_task(&self.todo, None, "Write", 1, None)
            .expect("task");
        self.db
            .set_task_members(&task, &[self.admin.as_str().to_string()])
            .expect("members");
        task
    }
}

#[tokio::test]
async fn full_name_should_be_trimmed_and_persist() {
    let fx = Fixture::new();
    let saved = fx
        .send(
            "PUT",
            "/api/settings",
            Some(json!({ "full_name": "  Ada Lovelace " })),
        )
        .await;
    assert_eq!(saved.body["full_name"], "Ada Lovelace");
    let read = fx.send("GET", "/api/settings", None).await;
    assert_eq!(read.body["full_name"], "Ada Lovelace");
}

#[tokio::test]
async fn full_name_past_80_characters_should_be_refused() {
    let fx = Fixture::new();
    let reply = fx
        .call(
            "PUT",
            "/api/settings",
            Some(json!({ "full_name": "x".repeat(81) })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
}

#[tokio::test]
async fn members_should_show_the_full_name_or_the_handle() {
    let fx = Fixture::new();
    let bare = fx.send("GET", "/api/members", None).await;
    assert_eq!(bare.body[0]["display_name"], "admin");
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "full_name": "Ada Lovelace" })),
    )
    .await;
    let named = fx.send("GET", "/api/members", None).await;
    assert_eq!(named.body[0]["display_name"], "Ada Lovelace");
    assert_eq!(named.body[0]["username"], "admin");
    fx.send("PUT", "/api/settings", Some(json!({ "full_name": "" })))
        .await;
    let cleared = fx.send("GET", "/api/members", None).await;
    assert_eq!(cleared.body[0]["display_name"], "admin");
}

#[tokio::test]
async fn card_and_modal_should_show_the_full_name_and_its_initials() {
    let fx = Fixture::new();
    let task = fx.assigned_task();
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "full_name": "Ada Lovelace" })),
    )
    .await;
    let card = fx.page(&format!("/api/tasks/{task}/card")).await;
    assert!(card.contains(r#"title="Ada Lovelace""#), "{card}");
    assert!(card.contains(">AL<"), "{card}");
    let modal = fx.page(&format!("/api/tasks/{task}/modal")).await;
    assert!(
        modal.contains(r#"title="Ada Lovelace (Responsible)""#),
        "{modal}"
    );
}

#[tokio::test]
async fn board_and_settings_pages_should_show_the_full_name() {
    let fx = Fixture::new();
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "full_name": "Ada Lovelace" })),
    )
    .await;
    let board = fx.page(&format!("/b/{}", fx.board)).await;
    assert!(
        board.contains(r#"title="Board owner: Ada Lovelace""#),
        "{board}"
    );
    let settings = fx.page("/settings").await;
    assert!(
        settings.contains(r#"id="profile-full-name""#)
            && settings.contains(r#"value="Ada Lovelace""#),
        "{settings}"
    );
}

#[tokio::test]
async fn hints_should_show_until_dismissed_for_good() {
    let fx = Fixture::new();
    let uri = format!("/b/{}", fx.board);
    let fresh = fx.page(&uri).await;
    assert!(
        fresh.contains(r#"class="help-tip" data-hint="members""#),
        "{fresh}"
    );
    assert!(
        fresh.contains(r#"class="help-tip" data-hint="timer""#),
        "{fresh}"
    );
    assert!(!fresh.contains("first-hint"), "{fresh}");
    let dismissed = fx
        .send(
            "PUT",
            "/api/settings",
            Some(json!({ "dismiss_hint": "members" })),
        )
        .await;
    assert_eq!(dismissed.body["dismissed_hints"], json!(["members"]));
    let again = fx
        .send(
            "PUT",
            "/api/settings",
            Some(json!({ "dismiss_hint": "members" })),
        )
        .await;
    assert_eq!(again.body["dismissed_hints"], json!(["members"]));
    let after = fx.page(&uri).await;
    assert!(!after.contains(r#"data-hint="members""#), "{after}");
    assert!(
        after.contains(r#"class="help-tip" data-hint="timer""#),
        "{after}"
    );
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "dismiss_hint": "timer" })),
    )
    .await;
    let none = fx.page(&uri).await;
    assert!(!none.contains(r#"data-hint="#), "{none}");
    assert!(!none.contains(r#"id="help-tips""#), "{none}");
}

#[tokio::test]
async fn an_unknown_hint_should_be_refused() {
    let fx = Fixture::new();
    let reply = fx
        .call(
            "PUT",
            "/api/settings",
            Some(json!({ "dismiss_hint": "tour" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
}

#[tokio::test]
async fn hints_should_not_carry_to_another_account() {
    let fx = Fixture::new();
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "dismiss_hint": "members" })),
    )
    .await;
    let other = fx.db.create_user("grace").expect("grace");
    let settings = fx.db.settings(&other).expect("settings");
    assert!(settings.dismissed_hints.is_empty());
    assert!(settings.full_name.is_empty());
}

/// The first bytes of a PNG, which is all the server reads of an image.
fn png(len: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(len, 7);
    bytes
}

#[tokio::test]
async fn a_text_avatar_should_take_a_text_and_two_colours() {
    let fx = Fixture::new();
    let task = fx.assigned_task();
    let saved = fx
        .send(
            "PUT",
            "/api/settings",
            Some(json!({ "avatar": { "text": "ZZ", "text_color": "#112233", "background": "#ffeedd" } })),
        )
        .await;
    assert_eq!(saved.body["avatar"]["kind"], "text");
    assert_eq!(saved.body["avatar"]["text"], "ZZ");
    assert_eq!(saved.body["avatar"]["text_color"], "#112233");
    assert_eq!(saved.body["avatar"]["background"], "#ffeedd");
    let card = fx.page(&format!("/api/tasks/{task}/card")).await;
    assert!(card.contains("color:#112233;background:#ffeedd;"), "{card}");
    assert!(card.contains(">ZZ<"), "{card}");
    let bar = fx.page(&format!("/b/{}", fx.board)).await;
    assert!(bar.contains("color:#112233;background:#ffeedd;"), "{bar}");
    let members = fx.send("GET", "/api/members", None).await;
    assert_eq!(members.body[0]["avatar"]["text"], "ZZ");
    assert_eq!(
        members.body[0]["avatar"]["style"],
        "color:#112233;background:#ffeedd;"
    );
}

#[tokio::test]
async fn a_text_avatar_should_refuse_a_bad_colour_or_a_long_text() {
    let fx = Fixture::new();
    for avatar in [
        json!({ "text": "AB", "background": "red" }),
        json!({ "text": "AB", "text_color": "#12345" }),
        json!({ "text": "ABCD" }),
    ] {
        let reply = fx
            .call("PUT", "/api/settings", Some(json!({ "avatar": avatar })))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    }
}

#[tokio::test]
async fn an_uploaded_image_should_show_in_the_top_bar_members_and_cards() {
    let fx = Fixture::new();
    let task = fx.assigned_task();
    let image = png(300);
    assert_eq!(fx.upload("image/png", image.clone()).await, StatusCode::OK);
    let settings = fx.send("GET", "/api/settings", None).await;
    assert_eq!(settings.body["avatar"]["kind"], "image");
    let src = format!(
        r#"<img src="/api/avatar?v={}""#,
        settings.body["avatar"]["image_version"]
    );
    let board = fx.page(&format!("/b/{}", fx.board)).await;
    assert!(board.matches(&src).count() >= 2, "{board}");
    let card = fx.page(&format!("/api/tasks/{task}/card")).await;
    assert!(card.contains(&src), "{card}");
    let modal = fx.page(&format!("/api/tasks/{task}/modal")).await;
    assert!(modal.contains(&src), "{modal}");
    let members = fx.send("GET", "/api/members", None).await;
    assert!(
        members.body[0]["avatar"]["image"]
            .as_str()
            .is_some_and(|url| src.contains(url)),
        "{}",
        members.body
    );
    let (status, content_type, bytes) = fx.avatar().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(bytes, image);
}

#[tokio::test]
async fn an_image_of_another_type_or_over_128_kib_should_be_refused() {
    let fx = Fixture::new();
    assert_eq!(
        fx.upload("image/svg+xml", b"<svg onload=alert(1)/>".to_vec())
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fx.upload("image/png", b"not an image".to_vec()).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fx.upload("image/jpeg", png(300)).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fx.upload("image/png", png(MAX_AVATAR_BYTES + 1)).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(fx.avatar().await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        fx.upload("image/png", png(MAX_AVATAR_BYTES)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_text_avatar_should_delete_the_uploaded_image() {
    let fx = Fixture::new();
    assert_eq!(fx.upload("image/png", png(300)).await, StatusCode::OK);
    assert_eq!(fx.avatar().await.0, StatusCode::OK);
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "avatar": { "text": "AB" } })),
    )
    .await;
    assert_eq!(fx.avatar().await.0, StatusCode::NOT_FOUND);
    let members = fx.send("GET", "/api/members", None).await;
    assert!(members.body[0]["avatar"]["image"].is_null());
}

#[tokio::test]
async fn deleting_an_account_should_delete_its_avatar_image() {
    let fx = Fixture::new();
    let other = fx.db.create_user("grace").expect("grace");
    fx.db.set_avatar_image(&other, &png(300)).expect("avatar");
    assert!(fx.db.avatar_image(&other).expect("read").is_some());
    assert!(fx.db.avatar_image(&fx.admin).expect("read").is_none());
    fx.db.delete_account(&other).expect("delete");
    assert!(fx.db.avatar_image(&other).expect("read").is_none());
}

#[tokio::test]
async fn date_and_time_should_default_to_12_hours_and_sunday() {
    let fx = Fixture::new();
    let read = fx.send("GET", "/api/settings", None).await;
    assert_eq!(read.body["time_format"], "12h");
    assert_eq!(read.body["week_start"], "sunday");
}

#[tokio::test]
async fn date_and_time_choices_should_persist() {
    let fx = Fixture::new();
    let saved = fx
        .send(
            "PUT",
            "/api/settings",
            Some(json!({ "time_format": "24h", "week_start": "monday" })),
        )
        .await;
    assert_eq!(saved.body["time_format"], "24h");
    assert_eq!(saved.body["week_start"], "monday");
    let read = fx.send("GET", "/api/settings", None).await;
    assert_eq!(read.body["time_format"], "24h");
    assert_eq!(read.body["week_start"], "monday");
}

#[tokio::test]
async fn unknown_date_and_time_choices_should_be_refused() {
    let fx = Fixture::new();
    for body in [
        json!({ "time_format": "13h" }),
        json!({ "week_start": "friday" }),
    ] {
        let reply = fx.call("PUT", "/api/settings", Some(body)).await;
        assert!(reply.status.is_client_error(), "{}", reply.body);
    }
    let read = fx.send("GET", "/api/settings", None).await;
    assert_eq!(read.body["time_format"], "12h");
}

#[tokio::test]
async fn the_24_hour_clock_should_show_on_the_card_and_modal() {
    let fx = Fixture::new();
    let task = fx.assigned_task();
    let due = (chrono::Utc::now() + chrono::Duration::days(2))
        .format("%Y-%m-%dT15:30:00Z")
        .to_string();
    fx.db.set_task_due(&task, Some(&due), None).expect("due");
    let card = fx.page(&format!("/api/tasks/{task}/card")).await;
    assert!(card.contains("3:30 PM"), "{card}");
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "time_format": "24h" })),
    )
    .await;
    let card = fx.page(&format!("/api/tasks/{task}/card")).await;
    assert!(card.contains("15:30"), "{card}");
    assert!(!card.contains("3:30 PM"), "{card}");
    let modal = fx.page(&format!("/api/tasks/{task}/modal")).await;
    assert!(modal.contains("15:30"), "{modal}");
    assert!(!modal.contains("3:30 PM"), "{modal}");
}

#[tokio::test]
async fn the_top_bar_should_carry_the_clock_and_week_start() {
    let fx = Fixture::new();
    let board = fx.page(&format!("/b/{}", fx.board)).await;
    assert!(board.contains("data-time-format=\"12h\""), "{board}");
    assert!(board.contains("data-week-start=\"sunday\""), "{board}");
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "time_format": "24h", "week_start": "monday" })),
    )
    .await;
    let board = fx.page(&format!("/b/{}", fx.board)).await;
    assert!(board.contains("data-time-format=\"24h\""), "{board}");
    assert!(board.contains("data-week-start=\"monday\""), "{board}");
}

#[tokio::test]
async fn the_clock_should_belong_to_one_account() {
    let fx = Fixture::new();
    let other = fx.db.create_user("grace").expect("grace");
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "time_format": "24h" })),
    )
    .await;
    assert_eq!(
        fx.db.settings(&other).expect("settings").time_format,
        TimeFormat::Hours12
    );
    assert_eq!(
        fx.db.settings(&fx.admin).expect("settings").time_format,
        TimeFormat::Hours24
    );
}

#[tokio::test]
async fn the_settings_page_should_offer_the_date_and_time_dialogs() {
    let fx = Fixture::new();
    let settings = fx.page("/settings").await;
    for needle in [
        "Date &amp; time",
        "Time format",
        "First day of week",
        "Time zone",
    ] {
        assert!(
            settings.contains(needle) || settings.contains(&needle.replace("&amp;", "&")),
            "{needle}: {settings}"
        );
    }
}

#[tokio::test]
async fn a_stale_settings_save_should_keep_the_uploaded_avatar() {
    let fx = Fixture::new();
    let stale = fx.db.settings(&fx.admin).expect("settings");
    assert_eq!(fx.upload("image/png", png(300)).await, StatusCode::OK);
    fx.db.update_settings(&fx.admin, &stale).expect("save");
    assert_eq!(fx.avatar().await.0, StatusCode::OK);
    let kept = fx.db.settings(&fx.admin).expect("settings");
    assert_eq!(kept.avatar.kind, AvatarKind::Image);
    assert_eq!(kept.avatar.image_version, 1);
}
