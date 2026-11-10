use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{Reply, call};
use super::{MAX_AVATAR_BYTES, router};
use crate::auth::new_api_token_record;
use crate::db::Db;
use crate::models::{AccountId, Actor, AuthScope, AvatarKind, InstanceMode, TimeFormat};
use crate::{AppState, BODY_LIMIT_BYTES};

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
        let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
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
    assert!(board.contains(r#"title="Ada Lovelace, owner""#), "{board}");
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

#[tokio::test]
async fn every_page_clock_time_should_carry_its_instant_for_the_browser() {
    let fx = Fixture::new();
    fx.send(
        "PUT",
        "/api/settings",
        Some(json!({ "time_format": "24h" })),
    )
    .await;
    let task = fx.assigned_task();
    let due = "2026-10-01T20:30:00Z";
    fx.db.set_task_due(&task, Some(due), None).expect("due");
    let started = "2026-10-01T20:30:00Z";
    let comment = fx
        .send(
            "POST",
            &format!("/api/tasks/{task}/comments"),
            Some(json!({ "body": "Seen" })),
        )
        .await;
    assert!(comment.status.is_success());
    let sub = fx
        .db
        .add_subtask(&task, "Step")
        .expect("subtask")
        .expect("task");
    fx.send(
        "PATCH",
        &format!("/api/tasks/{task}/subtasks/{}", sub.id),
        Some(json!({ "due_at": due })),
    )
    .await;
    fx.db
        .add_attachment(&task, "a.png", "image/png", b"png", Actor::SYSTEM)
        .expect("attach")
        .expect("task");
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/time"),
        Some(json!({ "minutes": 25, "started_at": started })),
    )
    .await;

    let card = fx.page(&format!("/api/tasks/{task}/card")).await;
    let modal = fx.page(&format!("/api/tasks/{task}/modal")).await;
    let history = fx.page(&format!("/api/tasks/{task}/history-view")).await;
    let entries = fx.page(&format!("/api/tasks/{task}/time-log-view")).await;
    let attr = |format: &str| {
        format!("data-at=\"2026-10-01T20:30:00Z\" data-fmt=\"{format}\" data-clock=\"24h\"")
    };
    assert!(
        card.contains(&format!(
            "<time class=\"local-time\" datetime=\"{due}\" {}",
            attr("due-card")
        )),
        "{card}"
    );
    assert!(
        card.contains(&format!("{} data-attr=\"title\"", attr("datetime"))),
        "{card}"
    );
    assert!(modal.contains(&attr("due-modal")), "{modal}");
    assert!(
        modal.matches("data-fmt=\"comment\"").count() >= 2,
        "{modal}"
    );
    assert!(modal.contains("data-fmt=\"due-modal\""), "{modal}");
    assert!(
        history.contains("data-fmt=\"datetime\" data-clock=\"24h\""),
        "{history}"
    );
    assert!(
        entries.contains("data-fmt=\"range\" data-clock=\"24h\" data-end=\"2026-10-01T20:55:00Z\""),
        "{entries}"
    );
    assert!(card.contains("UTC</time>"), "{card}");
    let log = fx.send("GET", "/api/timer/log", None).await;
    let first = &log.body["entries"][0];
    assert!(
        first["started_at"]
            .as_str()
            .expect("start")
            .starts_with("2026-10-01T20:30:00")
    );
    assert_eq!(first["ended_at"], "2026-10-01T20:55:00Z");
    fx.send(
        "POST",
        &format!("/api/tasks/{task}/time"),
        Some(json!({ "minutes": 5 })),
    )
    .await;
    let today = fx.send("GET", "/api/timer/today", None).await;
    let session = &today.body[0];
    for key in ["started_at", "ended_at"] {
        let instant = session[key].as_str().expect("instant");
        assert!(
            chrono::DateTime::parse_from_rfc3339(instant).is_ok(),
            "{instant}"
        );
    }
}

#[tokio::test]
async fn tips_should_hide_when_the_account_turns_them_off() {
    let fx = Fixture::new();
    let uri = format!("/b/{}", fx.board);
    let off = fx
        .send("PUT", "/api/settings", Some(json!({ "show_tips": false })))
        .await;
    assert_eq!(off.body["show_tips"], json!(false));
    let hidden = fx.page(&uri).await;
    assert!(!hidden.contains(r#"id="help-tips""#), "{hidden}");
    fx.send("PUT", "/api/settings", Some(json!({ "show_tips": true })))
        .await;
    let back = fx.page(&uri).await;
    assert!(back.contains(r#"data-hint="members""#), "{back}");
}

#[tokio::test]
async fn tips_should_hide_for_every_account_when_the_admin_disables_them() {
    let fx = Fixture::new();
    let uri = format!("/b/{}", fx.board);
    fx.db
        .update_instance_settings(|settings| settings.disable_tips = true)
        .expect("disable");
    let disabled = fx.page(&uri).await;
    assert!(!disabled.contains(r#"id="help-tips""#), "{disabled}");
    let own = fx
        .send("PUT", "/api/settings", Some(json!({ "show_tips": true })))
        .await;
    assert_eq!(own.body["show_tips"], json!(true));
    let still = fx.page(&uri).await;
    assert!(!still.contains(r#"id="help-tips""#), "{still}");
    fx.db
        .update_instance_settings(|settings| settings.disable_tips = false)
        .expect("enable");
    let enabled = fx.page(&uri).await;
    assert!(enabled.contains(r#"id="help-tips""#), "{enabled}");
}

#[tokio::test]
async fn tip_switches_should_be_stored() {
    let fx = Fixture::new();
    fx.send("PUT", "/api/settings", Some(json!({ "show_tips": false })))
        .await;
    fx.db
        .update_instance_settings(|settings| settings.disable_tips = true)
        .expect("disable");
    assert!(!fx.db.settings(&fx.admin).expect("settings").show_tips);
    assert!(fx.db.instance_settings().expect("instance").disable_tips);
}

#[tokio::test]
async fn spent_bookmarks_should_persist_and_refuse_past_the_cap() {
    let fx = Fixture::new();
    let filters = json!({"spent-board": "b1", "spent-period": "week"});
    let saved = fx
        .send(
            "PUT",
            "/api/settings",
            Some(json!({"spent_bookmarks": [
                {"name": " Weekly ", "filters": filters},
                {"name": "Weekly", "filters": {}},
                {"name": "  ", "filters": {}},
            ]})),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let expected = json!([{"name": "Weekly", "filters": filters}]);
    assert_eq!(saved.body["spent_bookmarks"], expected);
    let read = fx.send("GET", "/api/settings", None).await;
    assert_eq!(read.body["spent_bookmarks"], expected);

    let unrelated = fx
        .send("PUT", "/api/settings", Some(json!({"full_name": "Ada"})))
        .await;
    assert_eq!(unrelated.body["spent_bookmarks"], expected);

    let many: Vec<Value> = (0..51)
        .map(|n| json!({"name": format!("b{n}"), "filters": {}}))
        .collect();
    let refused = fx
        .call(
            "PUT",
            "/api/settings",
            Some(json!({"spent_bookmarks": many})),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let long = fx
        .call(
            "PUT",
            "/api/settings",
            Some(json!({"spent_bookmarks": [{"name": "x".repeat(61), "filters": {}}]})),
        )
        .await;
    assert_eq!(long.status, StatusCode::BAD_REQUEST, "{}", long.body);
    let kept = fx.send("GET", "/api/settings", None).await;
    assert_eq!(kept.body["spent_bookmarks"], expected);
}
