use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::router;
use super::server::{passkey_mode, unthrottled_limits};
use crate::db::{AccountSlot, Db};
use crate::models::{AccountId, HistoryKind, TaskAttachment};
use crate::{AppState, BODY_LIMIT_BYTES};

struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    state: AppState,
    account: AccountId,
    cookie: String,
    task: String,
    column: String,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("attachments.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let user = db.create_user("admin").expect("user");
    let session = db.create_session(&user, "").expect("session");
    let board = db.create_board(&user, "Files").expect("board");
    let column = db.create_column(&board, "Todo", None).expect("column");
    let state = AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits());
    let app = router(state.clone());
    let mut fixture = Fixture {
        _dir: dir,
        app,
        db,
        state,
        account: user,
        cookie: format!("session={session}"),
        task: String::new(),
        column: column.clone(),
    };
    let (status, task) = fixture
        .json(
            "POST",
            "/api/tasks",
            Some(json!({"column_id": column, "name": "Files"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    fixture.task = task["id"].as_str().expect("id").to_string();
    fixture
}

impl Fixture {
    async fn call(&self, method: &str, uri: &str, json: Option<Value>) -> (StatusCode, String) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", &self.cookie)
            .header("accept", "application/json");
        let body = match json {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                axum::body::Body::from(value.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT_BYTES)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn json(&self, method: &str, uri: &str, json: Option<Value>) -> (StatusCode, Value) {
        let (status, text) = self.call(method, uri, json).await;
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn attachments(&self) -> String {
        format!("/api/tasks/{}/attachments", self.task)
    }
}

#[tokio::test]
async fn attaching_a_link_should_store_it_list_it_and_open_the_address_from_the_modal() {
    let fx = fixture().await;
    let body = json!({"name": "Spec", "url": "  https://example.com/spec?a=1  "});
    let (status, link) = fx
        .json("POST", &format!("{}/link", fx.attachments()), Some(body))
        .await;
    assert_eq!(status, StatusCode::OK, "{link}");
    assert_eq!(link["name"], "Spec");
    assert_eq!(link["url"], "https://example.com/spec?a=1");
    assert_eq!(link["mime"], "text/uri-list");

    let (_, listed) = fx.json("GET", &fx.attachments(), None).await;
    assert_eq!(listed[0]["url"], "https://example.com/spec?a=1");
    let (_, modal) = fx
        .call("GET", &format!("/api/tasks/{}/modal", fx.task), None)
        .await;
    assert!(
        modal.contains(
            "href=\"https://example.com/spec?a=1\" target=\"_blank\" rel=\"noopener noreferrer\""
        ),
        "{modal}"
    );
    assert!(modal.contains(", Link</span>"), "{modal}");

    let file = format!(
        "{}/{}/file",
        fx.attachments(),
        link["id"].as_str().expect("id")
    );
    let (status, bytes) = fx.call("GET", &file, None).await;
    assert_eq!(
        (status, bytes.as_str()),
        (StatusCode::OK, "https://example.com/spec?a=1")
    );
    let row = fx.db.task(&fx.task).expect("read").expect("task");
    assert!(
        row.history
            .iter()
            .any(|event| event.kind == HistoryKind::AttachmentAdded)
    );
}

#[tokio::test]
async fn attaching_a_link_should_refuse_an_address_that_is_not_http_or_a_bad_name() {
    let fx = fixture().await;
    let uri = format!("{}/link", fx.attachments());
    for url in [
        "javascript:alert(1)",
        "ftp://example.com/a",
        "example.com",
        "https://",
        "http:///path",
        "https://exa mple.com",
        "https://example.com/\u{7}",
        "",
    ] {
        let (status, body) = fx
            .json("POST", &uri, Some(json!({"name": "x", "url": url})))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url}: {body}");
    }
    for name in ["", "  ", "a/b", "a\\b"] {
        let body = json!({"name": name, "url": "https://example.com"});
        let (status, body) = fx.json("POST", &uri, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {body}");
    }
    let (_, listed) = fx.json("GET", &fx.attachments(), None).await;
    assert_eq!(listed, json!([]));
}

#[tokio::test]
async fn attaching_the_same_link_twice_should_be_refused_as_a_duplicate() {
    let fx = fixture().await;
    let uri = format!("{}/link", fx.attachments());
    let first = json!({"name": "One", "url": "https://example.com"});
    let (status, _) = fx.json("POST", &uri, Some(first)).await;
    assert_eq!(status, StatusCode::OK);
    let again = json!({"name": "Two", "url": "https://example.com"});
    let (status, body) = fx.json("POST", &uri, Some(again)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string().contains("already holds this file"),
        "{body}"
    );
}

#[tokio::test]
async fn renaming_a_link_should_not_append_an_extension_and_deleting_it_should_remove_it() {
    let fx = fixture().await;
    let body = json!({"name": "example.com", "url": "https://example.com"});
    let (_, link) = fx
        .json("POST", &format!("{}/link", fx.attachments()), Some(body))
        .await;
    let uri = format!("{}/{}", fx.attachments(), link["id"].as_str().expect("id"));

    let (status, renamed) = fx
        .json("PATCH", &uri, Some(json!({"name": "Home page"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["name"], "Home page");
    assert_eq!(renamed["url"], "https://example.com");

    let (status, _) = fx.json("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, listed) = fx.json("GET", &fx.attachments(), None).await;
    assert_eq!(listed, json!([]));
    let (status, _) = fx.call("GET", &format!("{uri}/file"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_stored_attachment_without_an_address_should_read_as_a_file() {
    let old = json!({
        "id": "a", "name": "n.txt", "mime": "text/plain", "size": 1,
        "uploaded_by": "admin", "created_at": "2026-01-01T00:00:00Z"
    });
    let attachment: TaskAttachment = serde_json::from_value(old).expect("old row");
    assert_eq!(attachment.url, None);
    let shown = serde_json::to_value(&attachment).expect("json");
    assert!(shown.get("url").is_none());
}

async fn new_document(fx: &Fixture, name: &str, text: &str) -> Value {
    let uri = format!("{}/document", fx.attachments());
    let (status, document) = fx
        .json("POST", &uri, Some(json!({"name": name, "text": text})))
        .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    document
}

fn document_uri(fx: &Fixture, document: &Value, tail: &str) -> String {
    format!(
        "{}/{}/{tail}",
        fx.attachments(),
        document["id"].as_str().expect("id")
    )
}

fn stored_files(fx: &Fixture) -> usize {
    std::fs::read_dir(fx.db.attachments_dir())
        .expect("dir")
        .count()
}

#[tokio::test]
async fn creating_a_document_should_append_txt_and_store_the_text() {
    let fx = fixture().await;
    let document = new_document(&fx, "Meeting notes", "# Notes").await;
    assert_eq!(document["name"], "Meeting notes.txt");
    assert_eq!(document["mime"], "text/markdown");
    assert_eq!(document["size"], 7);
    assert!(document["edited_at"].as_str().is_some());
    let named = new_document(&fx, "plan.txt", "").await;
    assert_eq!(named["name"], "plan.txt");
    new_document(&fx, "empty", "").await;

    let (status, bytes) = fx
        .call("GET", &document_uri(&fx, &document, "file"), None)
        .await;
    assert_eq!((status, bytes.as_str()), (StatusCode::OK, "# Notes"));
    let (_, modal) = fx
        .call("GET", &format!("/api/tasks/{}/modal", fx.task), None)
        .await;
    assert!(modal.contains("data-document-open="), "{modal}");

    let uri = format!("{}/document", fx.attachments());
    let (status, body) = fx
        .json("POST", &uri, Some(json!({"name": "plan", "text": "x"})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = fx
        .json("POST", &uri, Some(json!({"name": "a/b", "text": "x"})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn saving_a_document_should_keep_each_earlier_text_as_a_version() {
    let fx = fixture().await;
    let document = new_document(&fx, "log", "one").await;
    let save = document_uri(&fx, &document, "document");
    for text in ["two", "three!"] {
        let (status, saved) = fx.json("PUT", &save, Some(json!({"text": text}))).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["size"], text.len());
    }
    let (status, renamed) = fx
        .json(
            "PUT",
            &save,
            Some(json!({"name": "journal", "text": "four"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["name"], "journal.txt");
    assert_eq!(renamed["versions"].as_array().expect("versions").len(), 3);

    let (_, versions) = fx
        .json("GET", &document_uri(&fx, &document, "versions"), None)
        .await;
    let versions = versions.as_array().expect("array");
    let sizes: Vec<u64> = versions
        .iter()
        .map(|v| v["size"].as_u64().expect("size"))
        .collect();
    assert_eq!(sizes, [6, 3, 3]);
    let mut texts = Vec::new();
    for version in versions {
        let tail = format!("versions/{}", version["id"].as_str().expect("id"));
        let (status, text) = fx
            .call("GET", &document_uri(&fx, &document, &tail), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        texts.push(text);
    }
    assert_eq!(texts, ["three!", "two", "one"]);
    let (_, current) = fx
        .call("GET", &document_uri(&fx, &document, "file"), None)
        .await;
    assert_eq!(current, "four");

    let row = fx.db.task(&fx.task).expect("read").expect("task");
    let kinds: Vec<HistoryKind> = row.history.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == HistoryKind::AttachmentEdited)
            .count(),
        3
    );
    assert!(kinds.contains(&HistoryKind::AttachmentRenamed));
    assert_eq!(stored_files(&fx), 4);
}

#[tokio::test]
async fn saving_should_refuse_a_file_and_an_unknown_document() {
    let fx = fixture().await;
    let data = json!({"name": "a.txt", "mime": "text/plain", "data": "aGk="});
    let (_, file) = fx.json("POST", &fx.attachments(), Some(data)).await;
    let (status, _) = fx
        .json(
            "PUT",
            &document_uri(&fx, &file, "document"),
            Some(json!({"text": "x"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let missing = format!("{}/nope/document", fx.attachments());
    let (status, _) = fx.json("PUT", &missing, Some(json!({"text": "x"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = fx
        .json("GET", &document_uri(&fx, &file, "versions/nope"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_document_should_keep_at_most_fifty_versions_and_delete_every_file() {
    let fx = fixture().await;
    let document = new_document(&fx, "long", "0").await;
    let save = document_uri(&fx, &document, "document");
    for n in 1..=52 {
        let (status, _) = fx
            .json("PUT", &save, Some(json!({"text": n.to_string()})))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, versions) = fx
        .json("GET", &document_uri(&fx, &document, "versions"), None)
        .await;
    assert_eq!(versions.as_array().expect("array").len(), 50);
    assert_eq!(stored_files(&fx), 51);

    let delete = format!(
        "{}/{}",
        fx.attachments(),
        document["id"].as_str().expect("id")
    );
    let (status, _) = fx.json("DELETE", &delete, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored_files(&fx), 0);
}

#[tokio::test]
async fn saving_a_document_should_refuse_a_name_another_attachment_holds() {
    let fx = fixture().await;
    new_document(&fx, "a", "x").await;
    let document = new_document(&fx, "b", "y").await;
    let save = document_uri(&fx, &document, "document");
    let (status, body) = fx
        .json("PUT", &save, Some(json!({"name": "a", "text": "z"})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (_, bytes) = fx
        .call("GET", &document_uri(&fx, &document, "file"), None)
        .await;
    assert_eq!(bytes, "y");
    let (status, _) = fx
        .json("PUT", &save, Some(json!({"name": "b", "text": "z"})))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn saving_an_uploaded_markdown_file_should_drop_its_stale_hash() {
    let fx = fixture().await;
    let data = json!({"name": "notes.md", "mime": "text/markdown", "data": "aGk="});
    let (_, uploaded) = fx.json("POST", &fx.attachments(), Some(data)).await;
    assert!(uploaded["sha256"].as_str().is_some(), "{uploaded}");
    let (status, saved) = fx
        .json(
            "PUT",
            &document_uri(&fx, &uploaded, "document"),
            Some(json!({"text": "changed"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(saved.get("sha256").is_none_or(Value::is_null), "{saved}");
    let (status, _) = fx
        .json(
            "POST",
            &fx.attachments(),
            Some(json!({"name": "again.md", "mime": "text/markdown", "data": "aGk="})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_failed_save_should_keep_the_document_text_and_add_no_version() {
    let fx = fixture().await;
    let document = new_document(&fx, "kept", "old text").await;
    let id = document["id"].as_str().expect("id");
    std::fs::create_dir(fx.db.attachments_dir().join(format!("{id}.part"))).expect("block");
    let (status, _) = fx
        .json(
            "PUT",
            &document_uri(&fx, &document, "document"),
            Some(json!({"text": "new text"})),
        )
        .await;
    assert!(status.is_server_error(), "{status}");
    let (status, text) = fx
        .call("GET", &document_uri(&fx, &document, "file"), None)
        .await;
    assert_eq!((status, text.as_str()), (StatusCode::OK, "old text"));
    let (_, versions) = fx
        .json("GET", &document_uri(&fx, &document, "versions"), None)
        .await;
    assert_eq!(versions.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn reading_a_version_should_refuse_an_id_from_another_document() {
    let fx = fixture().await;
    let first = new_document(&fx, "first", "1").await;
    let second = new_document(&fx, "second", "2").await;
    for document in [&first, &second] {
        let (status, _) = fx
            .json(
                "PUT",
                &document_uri(&fx, document, "document"),
                Some(json!({"text": "next"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, versions) = fx
        .json("GET", &document_uri(&fx, &first, "versions"), None)
        .await;
    let version = versions[0]["id"].as_str().expect("version");
    let (status, _) = fx
        .call(
            "GET",
            &document_uri(&fx, &second, &format!("versions/{version}")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, text) = fx
        .call(
            "GET",
            &document_uri(&fx, &first, &format!("versions/{version}")),
            None,
        )
        .await;
    assert_eq!((status, text.as_str()), (StatusCode::OK, "1"));
}

async fn upload(fx: &Fixture, name: &str, mime: &str) -> Value {
    let data = json!({"name": name, "mime": mime, "data": STANDARD.encode(name)});
    let (status, attachment) = fx.json("POST", &fx.attachments(), Some(data)).await;
    assert_eq!(status, StatusCode::OK, "{attachment}");
    attachment
}

#[tokio::test]
async fn setting_a_cover_should_show_it_on_the_card_and_the_row_and_log_the_change() {
    let fx = fixture().await;
    let image = upload(&fx, "shot.png", "image/png").await;
    let cover = document_uri(&fx, &image, "cover");

    let (status, _) = fx.json("PUT", &cover, None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, task) = fx
        .json("GET", &format!("/api/tasks/{}", fx.task), None)
        .await;
    assert_eq!(task["cover_id"], image["id"]);
    let (_, card) = fx
        .call("GET", &format!("/api/tasks/{}/card", fx.task), None)
        .await;
    let file = format!(
        "src=\"{}/{}/file\"",
        fx.attachments(),
        image["id"].as_str().expect("id")
    );
    assert!(
        card.contains("class=\"card-cover\"") && card.contains(&file),
        "{card}"
    );
    let (_, modal) = fx
        .call("GET", &format!("/api/tasks/{}/modal", fx.task), None)
        .await;
    assert!(
        modal.contains("(Cover)") && modal.contains("Remove as cover image"),
        "{modal}"
    );

    let (status, _) = fx.json("PUT", &cover, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = fx.json("DELETE", &cover, None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, task) = fx
        .json("GET", &format!("/api/tasks/{}", fx.task), None)
        .await;
    assert_eq!(task["cover_id"], Value::Null);
    let (_, card) = fx
        .call("GET", &format!("/api/tasks/{}/card", fx.task), None)
        .await;
    assert!(!card.contains("card-cover"), "{card}");

    let row = fx.db.task(&fx.task).expect("read").expect("task");
    let kinds: Vec<HistoryKind> = row.history.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| matches!(k, HistoryKind::CoverSet | HistoryKind::CoverRemoved))
            .count(),
        2
    );
    assert!(kinds.contains(&HistoryKind::CoverSet) && kinds.contains(&HistoryKind::CoverRemoved));
}

#[tokio::test]
async fn a_new_cover_should_replace_the_old_and_deleting_the_cover_should_clear_it() {
    let fx = fixture().await;
    let first = upload(&fx, "a.png", "image/png").await;
    let second = upload(&fx, "b.jpg", "image/jpeg").await;
    fx.json("PUT", &document_uri(&fx, &first, "cover"), None)
        .await;
    fx.json("PUT", &document_uri(&fx, &second, "cover"), None)
        .await;
    let (_, task) = fx
        .json("GET", &format!("/api/tasks/{}", fx.task), None)
        .await;
    assert_eq!(task["cover_id"], second["id"]);

    let delete = format!(
        "{}/{}",
        fx.attachments(),
        second["id"].as_str().expect("id")
    );
    let (status, _) = fx.json("DELETE", &delete, None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, task) = fx
        .json("GET", &format!("/api/tasks/{}", fx.task), None)
        .await;
    assert_eq!(task["cover_id"], Value::Null);
    let row = fx.db.task(&fx.task).expect("read").expect("task");
    assert_eq!(
        row.history
            .iter()
            .filter(|e| e.kind == HistoryKind::CoverRemoved)
            .count(),
        1
    );
}

#[tokio::test]
async fn a_cover_should_refuse_a_file_that_is_not_an_image_and_an_unknown_attachment() {
    let fx = fixture().await;
    let text = upload(&fx, "a.txt", "text/plain").await;
    let (status, _) = fx
        .json("PUT", &document_uri(&fx, &text, "cover"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let missing = format!("{}/nope/cover", fx.attachments());
    let (status, _) = fx.json("PUT", &missing, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, task) = fx
        .json("GET", &format!("/api/tasks/{}", fx.task), None)
        .await;
    assert_eq!(task["cover_id"], Value::Null);
}

async fn upload_bytes(fx: &Fixture, task: &str, name: &str, mime: &str, bytes: &[u8]) -> Value {
    let data = json!({"name": name, "mime": mime, "data": STANDARD.encode(bytes)});
    let uri = format!("/api/tasks/{task}/attachments");
    let (status, attachment) = fx.json("POST", &uri, Some(data)).await;
    assert_eq!(status, StatusCode::OK, "{attachment}");
    attachment
}

async fn image_reply(
    fx: &Fixture,
    task: &str,
    attachment: &Value,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let uri = format!(
        "/api/tasks/{task}/attachments/{}/image",
        attachment["id"].as_str().expect("id")
    );
    let request = axum::http::Request::builder()
        .uri(uri)
        .header("cookie", &fx.cookie)
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, BODY_LIMIT_BYTES)
        .await
        .expect("body");
    (parts.status, parts.headers, bytes.to_vec())
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend([8, 6, 0, 0, 0, 1, 2, 3]);
    bytes
}

#[tokio::test]
async fn the_image_route_should_serve_each_image_type_inline_with_the_sniffed_type() {
    let fx = fixture().await;
    let mut jpeg = vec![
        0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8, 0, 20, 0, 30, 1, 1, 0x11, 0,
    ];
    jpeg.extend([0xFF, 0xD9]);
    let mut gif = b"GIF89a".to_vec();
    gif.extend([30, 0, 20, 0, 0, 0, 0]);
    let mut webp = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2F".to_vec();
    webp.extend((29u32 | (19 << 14)).to_le_bytes());
    let cases = [
        ("a.bin", png(30, 20), "image/png"),
        ("b.bin", jpeg, "image/jpeg"),
        ("c.bin", gif, "image/gif"),
        ("d.bin", webp, "image/webp"),
    ];
    for (name, bytes, expected) in cases {
        let attachment = upload_bytes(&fx, &fx.task, name, "text/html", &bytes).await;
        let (status, headers, body) = image_reply(&fx, &fx.task, &attachment).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        let header = |name: &str| headers.get(name).map(|v| v.to_str().expect("ascii"));
        assert_eq!(header("content-type"), Some(expected), "{name}");
        assert_eq!(header("x-content-type-options"), Some("nosniff"), "{name}");
        assert_eq!(header("content-disposition"), Some("inline"), "{name}");
        assert_eq!(
            header("content-security-policy"),
            Some("sandbox; default-src 'none'"),
            "{name}"
        );
        assert_eq!(body, bytes, "{name}");
    }
}

#[tokio::test]
async fn the_image_route_should_refuse_text_svg_oversized_and_cut_off_images() {
    let fx = fixture().await;
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'/>";
    let cases = [
        ("a.png", "image/png", b"hello".to_vec()),
        ("b.svg", "image/svg+xml", svg.to_vec()),
        ("c.png", "image/png", png(8001, 10)),
        ("d.png", "image/png", png(7000, 6000)),
        ("e.png", "image/png", png(0, 10)),
        ("f.png", "image/png", png(10, 10)[..14].to_vec()),
    ];
    for (name, mime, bytes) in cases {
        let attachment = upload_bytes(&fx, &fx.task, name, mime, &bytes).await;
        let (status, headers, _) = image_reply(&fx, &fx.task, &attachment).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{name}");
        assert_ne!(
            headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("image/png"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn the_image_route_should_answer_404_for_another_tasks_or_a_missing_attachment() {
    let fx = fixture().await;
    let attachment = upload_bytes(&fx, &fx.task, "a.png", "image/png", &png(3, 3)).await;
    let (_, other) = fx
        .json(
            "POST",
            "/api/tasks",
            Some(json!({"column_id": fx.column, "name": "Other"})),
        )
        .await;
    let other = other["id"].as_str().expect("id").to_string();
    let (status, _, _) = image_reply(&fx, &other, &attachment).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let missing = json!({"id": "nope"});
    let (status, _, _) = image_reply(&fx, &fx.task, &missing).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = image_reply(&fx, "nope", &attachment).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_description_and_a_comment_should_show_only_the_images_of_their_own_task() {
    let fx = fixture().await;
    let mine = upload_bytes(&fx, &fx.task, "a.png", "image/png", &png(3, 3)).await;
    let text = upload_bytes(&fx, &fx.task, "n.txt", "text/plain", b"hello").await;
    let (_, other) = fx
        .json(
            "POST",
            "/api/tasks",
            Some(json!({"column_id": fx.column, "name": "Other"})),
        )
        .await;
    let other = other["id"].as_str().expect("id").to_string();
    let foreign = upload_bytes(&fx, &other, "b.png", "image/png", &png(3, 3)).await;
    let mine = mine["id"].as_str().expect("id");
    let text = text["id"].as_str().expect("id");
    let foreign = foreign["id"].as_str().expect("id");
    let source = format!(
        "![shot](attachment:{mine})\n\n![note](attachment:{text})\n\n![theirs](attachment:{foreign})\n\n\
         ![gone](attachment:nope)\n\n<img src=\"/x.png\" onerror=\"alert(1)\">\n\n\
         ![web](http://e.com/p.png)\n\n![data](data:image/png;base64,AAAA)"
    );
    let uri = format!("/api/tasks/{}", fx.task);
    let (status, body) = fx
        .json("PATCH", &uri, Some(json!({"description": source})))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = fx
        .json(
            "POST",
            &format!("{uri}/comments"),
            Some(json!({"body": source})),
        )
        .await;
    assert!(status.is_success(), "{status} {body}");
    let (_, modal) = fx.call("GET", &format!("{uri}/modal"), None).await;
    let wanted =
        format!("<img src=\"{uri}/attachments/{mine}/image\" alt=\"shot\" loading=\"lazy\">");
    assert_eq!(modal.matches(&wanted).count(), 2, "{modal}");
    assert_eq!(modal.matches("/image\"").count(), 2, "{modal}");
    assert!(!modal.contains("<img src=\"/x.png\""), "{modal}");
    assert!(!modal.contains("onerror=\"alert"), "{modal}");
    assert!(!modal.contains("src=\"http://e.com"), "{modal}");
    assert!(!modal.contains("src=\"data:"), "{modal}");
    for alt in ["note", "theirs", "gone", "web", "data"] {
        assert!(modal.contains(alt), "{alt}");
    }
}

#[tokio::test]
async fn the_modal_should_count_the_references_of_an_attachment_and_keep_the_alt_text_after_a_delete()
 {
    let fx = fixture().await;
    let image = upload_bytes(&fx, &fx.task, "a.png", "image/png", &png(3, 3)).await;
    let unused = upload_bytes(&fx, &fx.task, "b.png", "image/png", &png(4, 4)).await;
    let image = image["id"].as_str().expect("id");
    let unused = unused["id"].as_str().expect("id");
    let uri = format!("/api/tasks/{}", fx.task);
    let source = format!("![shot](attachment:{image})");
    let (status, _) = fx
        .json("PATCH", &uri, Some(json!({"description": source})))
        .await;
    assert_eq!(status, StatusCode::OK);
    for body in [
        source.clone(),
        format!("{source} again"),
        "`![shot](attachment:x)`".to_string(),
    ] {
        let (status, _) = fx
            .json(
                "POST",
                &format!("{uri}/comments"),
                Some(json!({"body": body})),
            )
            .await;
        assert!(status.is_success());
    }
    let (_, modal) = fx.call("GET", &format!("{uri}/modal"), None).await;
    assert!(
        modal.contains(&format!("data-attachment-id=\"{image}\"")),
        "{modal}"
    );
    assert!(modal.contains("data-attachment-refs=\"3\""), "{modal}");
    assert!(modal.contains("data-attachment-refs=\"0\""), "{modal}");
    assert!(modal.contains(unused));

    let (status, _) = fx
        .json("DELETE", &format!("{uri}/attachments/{image}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, modal) = fx.call("GET", &format!("{uri}/modal"), None).await;
    assert!(
        !modal.contains(&format!("attachments/{image}/image")),
        "{modal}"
    );
    assert!(modal.contains(">shot</a>"), "{modal}");
    assert!(
        modal.contains(&format!("![shot](attachment:{image})")),
        "{modal}"
    );
}

#[tokio::test]
async fn listing_attachments_should_name_their_addresses_and_return_base64_only_on_request() {
    let fx = fixture().await;
    let bytes = png(3, 3);
    let image = upload_bytes(&fx, &fx.task, "a.png", "image/png", &bytes).await;
    let fake = upload_bytes(&fx, &fx.task, "b.png", "image/png", b"not an image").await;
    let text = upload_bytes(&fx, &fx.task, "c.txt", "text/plain", b"hello").await;
    let (status, _) = fx
        .json(
            "POST",
            &format!("{}/link", fx.attachments()),
            Some(json!({"name": "Spec", "url": "https://example.com"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let base = fx.attachments();
    let image_id = image["id"].as_str().expect("id");

    let (status, plain) = fx.json("GET", &base, None).await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert_eq!(plain[0]["id"], image_id);
    assert_eq!(plain[0]["name"], "a.png");
    assert_eq!(plain[0]["mime"], "image/png");
    assert_eq!(plain[0]["size"], bytes.len());
    assert_eq!(plain[0]["file_url"], format!("{base}/{image_id}/file"));
    assert_eq!(plain[0]["image_url"], format!("{base}/{image_id}/image"));
    assert_eq!(
        plain[2]["file_url"],
        format!("{base}/{}/file", text["id"].as_str().expect("id"))
    );
    assert!(plain[2].get("image_url").is_none(), "{plain}");
    assert!(plain[3].get("file_url").is_none(), "{plain}");
    assert_eq!(plain[3]["url"], "https://example.com");
    assert!(
        plain
            .as_array()
            .expect("array")
            .iter()
            .all(|a| a.get("data_base64").is_none())
    );

    let (status, full) = fx.json("GET", &format!("{base}?base64=true"), None).await;
    assert_eq!(status, StatusCode::OK, "{full}");
    assert_eq!(full[0]["data_base64"], STANDARD.encode(&bytes));
    assert!(full[1].get("data_base64").is_none(), "{full}");
    assert_eq!(full[1]["id"], fake["id"]);
    assert!(full[2].get("data_base64").is_none(), "{full}");
}

fn padded_png(len: usize, tag: u8) -> Vec<u8> {
    let mut bytes = png(3, 3);
    bytes.resize(len, tag);
    bytes
}

#[tokio::test]
async fn listing_base64_should_hold_to_the_per_image_cap_and_the_total_budget() {
    let fx = fixture().await;
    let cap = 2 * 1024 * 1024;
    let mut names = Vec::new();
    for (name, len, tag) in [
        ("over.png", cap + 1, 1),
        ("e1.png", cap, 2),
        ("e2.png", cap, 3),
        ("e3.png", cap, 4),
        ("e4.png", cap, 5),
        ("small.png", 100, 6),
    ] {
        upload_bytes(&fx, &fx.task, name, "image/png", &padded_png(len, tag)).await;
        names.push(name);
    }
    let request = axum::http::Request::builder()
        .uri(format!("{}?base64=true", fx.attachments()))
        .header("cookie", &fx.cookie)
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fx.app.clone().oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 32 << 20)
        .await
        .expect("body");
    let full: Value = serde_json::from_slice(&bytes).expect("json");
    let has: Vec<bool> = full
        .as_array()
        .expect("array")
        .iter()
        .map(|a| a.get("data_base64").is_some())
        .collect();
    assert_eq!(has, [false, true, true, true, true, false], "{names:?}");
}

#[tokio::test(start_paused = true)]
async fn base64_list_should_answer_busy_while_download_slots_are_held() {
    let fx = fixture().await;
    let held = fx
        .state
        .account_slot(AccountSlot::Download, fx.account.as_str())
        .acquire_many_owned(AppState::DOWNLOADS_PER_ACCOUNT_AT_ONCE as u32)
        .await
        .expect("slots");
    let base64 = format!("{}?base64=true", fx.attachments());
    let (status, body) = fx.call("GET", &base64, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("taking as many downloads"), "{body}");
    let (status, body) = fx.call("GET", &fx.attachments(), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    drop(held);
    let (status, body) = fx.call("GET", &base64, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
