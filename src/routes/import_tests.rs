use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use super::server::{passkey_mode, unthrottled_limits};
use super::test_support::{call, make_epic};
use super::{
    AppError, CancelOnDrop, ImportedBoard, ROUTE_ACCESS, Writes, import_spool, router, run_import,
};
use crate::AppState;
use crate::archive::{MAX_ARCHIVE_BYTES, SPOOL_PREFIX};
use crate::auth::new_api_token_record;
use crate::db::{Db, LinkAddOutcome};
use crate::disk::{DiskGuard, DiskProbe, MIB};
use crate::models::{
    AccountId, Actor, AuthScope, BoardAccess, CoverChange, DiskLimits, InstanceMode, LinkKind,
};

/// What the router answered.
struct Answer {
    status: StatusCode,
    body: Vec<u8>,
    error_code: Option<String>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn code(&self) -> Option<String> {
        self.json()["code"].as_str().map(str::to_string)
    }

    fn message(&self) -> String {
        self.json()["message"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

/// An instance with an admin who owns a board and a member who owns none.
struct Fixture {
    dir: tempfile::TempDir,
    db: Db,
    app: Router,
    disk: DiskGuard,
    admin: AccountId,
    member: AccountId,
    board: String,
    /// The admin's `read` token.
    admin_read: String,
    /// The member's `provision` token.
    member_provision: String,
    /// The member's `work` token.
    member_work: String,
}

impl Fixture {
    fn new() -> Self {
        Self::with_disk(None)
    }

    fn with_disk(disk: Option<DiskGuard>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("import.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let admin = db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        let token = |account: &AccountId, scope: AuthScope| {
            let (record, raw) = new_api_token_record(account, "agent", scope, None);
            db.create_api_token(&record).expect("token");
            raw
        };
        let board = db.create_board(&admin, "Plans").expect("board");
        db.create_swimlane(&board, "Lane").expect("lane");
        let todo = db.create_column(&board, "Todo", None).expect("column");
        let task = db
            .create_task(&todo, None, "Write it", 1, None)
            .expect("task");
        db.create_entry(&task, 5, "worked").expect("entry");
        db.add_attachment(&task, "a.txt", "text/plain", b"hello", Actor::SYSTEM)
            .expect("attachment")
            .expect("task");
        let disk = disk.unwrap_or_else(|| {
            let born = Instant::now();
            DiskGuard::with_clock(DiskProbe::of(&db), dir.path(), Arc::new(move || born))
        });
        let state = AppState::new(db.clone(), None, passkey_mode())
            .with_mode(InstanceMode::Multi)
            .with_limits(unthrottled_limits())
            .with_disk(disk);
        Self {
            disk: state.disk.clone(),
            admin_read: token(&admin, AuthScope::Read),
            member_provision: token(&member, AuthScope::Provision),
            member_work: token(&member, AuthScope::Work),
            app: router(state),
            dir,
            db,
            admin,
            member,
            board,
        }
    }

    fn spools(&self) -> usize {
        fs::read_dir(self.dir.path())
            .expect("dir")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(SPOOL_PREFIX)
            })
            .count()
    }

    fn run(&self, body: &[u8], cancelled: bool) -> Result<ImportedBoard, AppError> {
        let flag = AtomicBool::new(cancelled);
        let spool = import_spool(self.dir.path(), &self.disk)?;
        run_import(
            &self.db.account(&self.member),
            "bob",
            Actor::SYSTEM,
            body,
            &spool,
            &flag,
        )
    }

    async fn send(&self, method: &str, uri: &str, token: Option<&str>, body: Vec<u8>) -> Answer {
        let bearer = token.map(|token| format!("Bearer {token}"));
        let headers = bearer
            .iter()
            .map(|bearer| ("authorization", bearer.as_str()))
            .collect::<Vec<_>>();
        self.send_with(method, uri, &headers, body).await
    }

    async fn send_with(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if !body.is_empty() {
            builder = builder.header("content-type", "application/gzip");
        }
        let request = builder.body(Body::from(body)).expect("request");
        let response = self.app.clone().oneshot(request).await.expect("oneshot");
        let status = response.status();
        let error_code = response
            .headers()
            .get("x-pomb-error-code")
            .map(|value| value.to_str().expect("ascii").to_string());
        let body = to_bytes(response.into_body(), 2 * MAX_ARCHIVE_BYTES)
            .await
            .expect("body")
            .to_vec();
        Answer {
            status,
            body,
            error_code,
        }
    }

    async fn archive(&self) -> Vec<u8> {
        let uri = format!("/api/boards/{}/archive", self.board);
        let answer = self
            .send("GET", &uri, Some(&self.admin_read), Vec::new())
            .await;
        assert_eq!(answer.status, StatusCode::OK);
        answer.body
    }

    async fn import(&self, token: &str, body: Vec<u8>) -> Answer {
        self.send("POST", "/api/boards/import", Some(token), body)
            .await
    }

    fn member_state(&self) -> (usize, Option<u64>) {
        (
            self.db.owner_boards(&self.member).expect("boards").len(),
            self.db
                .stored_bytes_by_account()
                .expect("usage")
                .get(self.member.as_str())
                .copied(),
        )
    }
}

#[tokio::test]
async fn import_should_create_a_new_board_for_the_caller_from_an_archive() {
    let fx = Fixture::new();
    let archive = fx.archive().await;
    let admin_before = fx.db.owner_boards(&fx.admin).expect("boards").len();

    let made = fx.import(&fx.member_provision, archive).await;

    assert_eq!(made.status, StatusCode::OK, "{}", made.message());
    let reply = made.json();
    let id = reply["id"].as_str().expect("id").to_string();
    assert_ne!(id, fx.board);
    assert_eq!(reply["counts"]["tasks"], 1);
    assert_eq!(reply["counts"]["attachments"], 1);
    assert_eq!(reply["counts"]["time_entries"], 1);
    assert_eq!(reply["counts"]["dropped_links"], 0);
    assert_eq!(reply["counts"]["assignees_matched"], 0);
    let boards = fx.db.owner_boards(&fx.member).expect("boards");
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0].id, id);
    assert_eq!(
        fx.db.owner_boards(&fx.admin).expect("boards").len(),
        admin_before
    );
    let listed = fx
        .send("GET", "/api/boards", Some(&fx.member_provision), Vec::new())
        .await;
    assert!(
        listed
            .json()
            .as_array()
            .expect("boards")
            .iter()
            .any(|board| board["id"] == id.as_str())
    );
    let tasks = fx.db.board_tasks(&id).expect("tasks");
    assert_eq!(tasks.len(), 1);
    let attachment = &tasks[0].attachments[0];
    let bytes = fx
        .db
        .read_attachment_data(&attachment.id)
        .expect("read")
        .expect("bytes");
    assert_eq!(bytes, b"hello");
    assert_eq!(fx.db.account_entries(&fx.member).expect("entries").len(), 1);
    assert_eq!(fx.spools(), 0);
}

#[tokio::test]
async fn import_should_add_the_new_board_to_the_set_of_the_token_that_made_it() {
    let fx = Fixture::new();
    let archive = fx.archive().await;
    let (mut record, raw) = new_api_token_record(&fx.member, "lead", AuthScope::Provision, None);
    record.boards = BoardAccess::Only(Default::default());
    fx.db.create_api_token(&record).expect("token");

    let made = fx.import(&raw, archive).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.message());

    let id = made.json()["id"].as_str().expect("id").to_string();
    let held = fx.db.find_api_token(&raw).expect("find").expect("token");
    assert!(held.boards.allows(&id));
    let uri = format!("/api/boards/{id}");
    let read = fx.send("GET", &uri, Some(&raw), Vec::new()).await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.message());
}

#[tokio::test]
async fn import_should_refuse_what_is_not_a_board_archive_and_write_nothing() {
    let fx = Fixture::new();
    let before = fx.member_state();
    let mut cut = fx.archive().await;
    cut.truncate(cut.len() / 2);
    let bodies: [(&str, Vec<u8>); 3] = [
        ("text", b"definitely not gzip".to_vec()),
        ("truncated", cut),
        ("json", b"{}".to_vec()),
    ];
    for (label, body) in bodies {
        let refused = fx.import(&fx.member_provision, body).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{label}");
        assert_eq!(
            refused.code().as_deref(),
            Some("invalid_request"),
            "{label}"
        );
        assert!(!refused.message().is_empty(), "{label}");
    }
    let empty = fx.import(&fx.member_provision, Vec::new()).await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(fx.member_state(), before);
    assert_eq!(fx.spools(), 0);
}

#[tokio::test]
async fn import_should_need_a_credential_and_the_boards_create_permission() {
    let fx = Fixture::new();
    let archive = fx.archive().await;
    let before = fx.member_state();
    let anonymous = fx
        .send("POST", "/api/boards/import", None, archive.clone())
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let work = fx.import(&fx.member_work, archive.clone()).await;
    assert_eq!(work.status, StatusCode::FORBIDDEN);
    assert_eq!(work.code().as_deref(), Some("permission_denied"));
    let read = fx.import(&fx.admin_read, archive).await;
    assert_eq!(read.status, StatusCode::FORBIDDEN);
    assert_eq!(fx.member_state(), before);
}

#[tokio::test]
async fn import_should_refuse_a_body_over_the_cap_with_a_413() {
    let fx = Fixture::new();
    let before = fx.member_state();
    let body = vec![0u8; MAX_ARCHIVE_BYTES + 1];
    let refused = fx.import(&fx.member_provision, body).await;
    assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(refused.code().as_deref(), Some("payload_too_large"));
    assert!(
        refused
            .message()
            .contains(&format!("over its limit of {MAX_ARCHIVE_BYTES} bytes")),
        "{}",
        refused.message()
    );
    assert_eq!(fx.member_state(), before);
}

#[tokio::test]
async fn import_should_answer_409_past_a_quota_and_write_nothing() {
    let fx = Fixture::new();
    fx.db
        .update_instance_settings(|settings| settings.quotas.attachments_per_task = 1)
        .expect("quotas");
    let uri = format!("/api/boards/{}/archive", fx.board);
    let task = fx.db.board_tasks(&fx.board).expect("tasks")[0].id.clone();
    fx.db
        .add_attachment(&task, "b.txt", "text/plain", b"more", Actor::SYSTEM)
        .expect("attachment")
        .expect("task");
    let archive = fx
        .send("GET", &uri, Some(&fx.admin_read), Vec::new())
        .await
        .body;
    let before = fx.member_state();

    let refused = fx.import(&fx.member_provision, archive).await;

    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "{}",
        refused.message()
    );
    assert_eq!(refused.code().as_deref(), Some("quota_exceeded"));
    assert!(
        refused.message().contains("attachments per task"),
        "{}",
        refused.message()
    );
    assert_eq!(fx.member_state(), before);
}

#[tokio::test]
async fn import_should_be_stopped_by_a_full_disk_and_write_nothing() {
    let probe = DiskProbe {
        stored: Arc::new(|| Ok(1)),
        file: Arc::new(|| Ok((DiskLimits::DEFAULT_MAX_DATABASE_MIB as u64 + 1) * MIB)),
        free: Arc::new(|_| Ok(100_000 * MIB)),
    };
    let fx = Fixture::with_disk(Some(DiskGuard::new(probe, Path::new("."))));
    let archive = fx.archive().await;
    let before = fx.member_state();

    let refused = fx.import(&fx.member_provision, archive).await;

    assert_eq!(refused.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(refused.code().as_deref(), Some("storage_full"));
    assert_eq!(fx.member_state(), before);
}

/// A fixture whose disk has room for less than a board holding a 6 MiB attachment spools.
fn spool_crossing_fixture() -> Fixture {
    const BIG_BYTES: usize = 6 * MIB as usize;
    const SPARE_MIB: u64 = 10;
    const DISK_PER_SPOOLED_BYTE: u64 = 4;
    let floor = u64::from(DiskLimits::DEFAULT_MIN_FREE_MIB);
    let probe = DiskProbe {
        stored: Arc::new(|| Ok(1)),
        file: Arc::new(|| Ok(1)),
        free: Arc::new(move |spool| {
            let spooled: u64 = fs::read_dir(spool)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|file| file.metadata().ok())
                .map(|meta| meta.len())
                .sum();
            Ok((floor + SPARE_MIB) * MIB - DISK_PER_SPOOLED_BYTE * spooled)
        }),
    };
    let fx = Fixture::with_disk(Some(DiskGuard::new(probe, Path::new("."))));
    let task = fx.db.board_tasks(&fx.board).expect("tasks")[0].id.clone();
    fx.db
        .add_attachment(
            &task,
            "big.bin",
            "application/octet-stream",
            &vec![7u8; BIG_BYTES],
            Actor::SYSTEM,
        )
        .expect("attachment")
        .expect("task");
    fx
}

#[tokio::test]
async fn import_should_answer_507_when_the_spool_crosses_the_floor_and_leave_nothing() {
    let fx = spool_crossing_fixture();
    let archive = fx.archive().await;
    let before = fx.member_state();

    let refused = fx.import(&fx.member_provision, archive).await;

    assert_eq!(refused.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(refused.code().as_deref(), Some("storage_full"));
    assert!(
        refused
            .message()
            .contains("an import would leave less than"),
        "{}",
        refused.message()
    );
    assert_eq!(fx.member_state(), before);
    assert_eq!(fx.spools(), 0);
}

#[tokio::test]
async fn import_should_keep_plain_text_and_name_storage_full_in_a_header_for_a_cookie_caller() {
    let fx = spool_crossing_fixture();
    let archive = fx.archive().await;
    let cookie = format!(
        "session={}",
        fx.db.create_session(&fx.member, "").expect("session")
    );
    let headers = [
        ("cookie", cookie.as_str()),
        ("sec-fetch-site", "same-origin"),
    ];

    let refused = fx
        .send_with("POST", "/api/boards/import", &headers, archive)
        .await;

    assert_eq!(refused.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(refused.error_code.as_deref(), Some("storage_full"));
    let text = String::from_utf8_lossy(&refused.body);
    assert!(text.contains("an import would leave less than"), "{text}");
}

#[tokio::test]
async fn import_should_be_stopped_by_a_disk_sample_older_than_the_stale_limit() {
    let born = Instant::now();
    let aged = Arc::new(Mutex::new(born));
    let reads = aged.clone();
    let probe = DiskProbe {
        stored: Arc::new(|| Ok(1)),
        file: Arc::new(|| Ok(1)),
        free: Arc::new(|_| Ok(100_000 * MIB)),
    };
    let guard = DiskGuard::with_clock(
        probe,
        Path::new("."),
        Arc::new(move || *reads.lock().expect("clock")),
    );
    *aged.lock().expect("clock") = born + DiskGuard::STALE_AFTER + Duration::from_secs(1);
    let fx = Fixture::with_disk(Some(guard));
    let archive = fx.archive().await;

    let refused = fx.import(&fx.member_provision, archive).await;

    assert_eq!(refused.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(refused.code().as_deref(), Some("storage_full"));
    assert!(
        refused.message().contains("not measured"),
        "{}",
        refused.message()
    );
}

#[test]
fn import_route_should_be_classified_as_a_write_that_grows_the_database() {
    let entry = ROUTE_ACCESS
        .iter()
        .find(|(method, path, _, _)| *method == Method::POST && *path == "/api/boards/import")
        .expect("a ROUTE_ACCESS entry");
    assert_eq!(entry.3, Writes::Grows);
}

#[tokio::test]
async fn new_board_page_should_offer_the_import() {
    let fx = Fixture::new();
    let cookie = format!(
        "session={}",
        fx.db.create_session(&fx.member, "").expect("session")
    );
    let page = call(&fx.app, "GET", "/boards/new", &[("cookie", &cookie)], None).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = page.body.as_str().expect("html");
    for needle in ["id=\"nb-import-file\"", "id=\"nb-import\"", "Import board"] {
        assert!(html.contains(needle), "{needle}");
    }
}

#[tokio::test]
async fn import_should_store_nothing_and_leave_no_spool_when_the_request_ended() {
    let fx = Fixture::new();
    let archive = fx.archive().await;
    let before = fx.member_state();

    let Err(refused) = fx.run(&archive, true) else {
        panic!("a refusal")
    };

    assert_eq!(refused.status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(fx.member_state(), before);
    assert_eq!(fx.spools(), 0);

    let Ok(stored) = fx.run(&archive, false) else {
        panic!("an import")
    };
    assert_eq!(stored.counts.tasks, 1);
    assert_eq!(fx.spools(), 0);
    assert_ne!(fx.member_state(), before);
}

#[test]
fn dropping_a_request_should_tell_its_blocking_work_to_stop() {
    let flag = Arc::new(AtomicBool::new(false));
    let guard = CancelOnDrop(Arc::clone(&flag));
    assert!(!flag.load(Ordering::Relaxed));
    drop(guard);
    assert!(flag.load(Ordering::Relaxed));
}

#[tokio::test]
async fn import_should_leave_no_spool_when_it_refuses_an_archive() {
    let fx = Fixture::new();
    let mut cut = fx.archive().await;
    cut.truncate(cut.len() * 3 / 4);
    let before = fx.member_state();
    for body in [cut, b"not an archive".to_vec(), Vec::new()] {
        let Err(refused) = fx.run(&body, false) else {
            panic!("a refusal")
        };
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(fx.spools(), 0);
    }
    fx.db
        .update_instance_settings(|settings| settings.quotas.attachments_per_task = 1)
        .expect("quotas");
    let task = fx.db.board_tasks(&fx.board).expect("tasks")[0].id.clone();
    fx.db
        .add_attachment(&task, "b.txt", "text/plain", b"more", Actor::SYSTEM)
        .expect("attachment")
        .expect("task");
    let heavy = fx.archive().await;
    let Err(refused) = fx.run(&heavy, false) else {
        panic!("a quota refusal")
    };
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(fx.spools(), 0);
    assert_eq!(fx.member_state(), before);
}

#[tokio::test]
async fn export_and_import_should_keep_a_three_level_nest() {
    let fx = Fixture::new();
    let todo = fx.db.list_columns(&fx.board).expect("columns")[0]
        .id
        .clone();
    let make = |name: &str| fx.db.create_task(&todo, None, name, 1, None).expect("task");
    let (root, mid, leaf) = (make("Root"), make("Mid"), make("Leaf"));
    make_epic(&fx.db, &[&root, &mid]);
    for (child, umbrella) in [(&mid, &root), (&leaf, &mid)] {
        let outcome = fx
            .db
            .add_task_link(child, LinkKind::Parent, umbrella, None, Actor::SYSTEM)
            .expect("link");
        assert!(matches!(outcome, LinkAddOutcome::Added(_)), "{outcome:?}");
    }

    let made = fx.import(&fx.member_provision, fx.archive().await).await;

    assert_eq!(made.status, StatusCode::OK, "{}", made.message());
    assert_eq!(made.json()["counts"]["dropped_links"], 0);
    let id = made.json()["id"].as_str().expect("id").to_string();
    let tasks = fx.db.board_tasks(&id).expect("tasks");
    let by_name = |name: &str| {
        tasks
            .iter()
            .find(|task| task.name == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    let parent_of = |name: &str| {
        by_name(name)
            .links
            .iter()
            .find(|link| link.kind == LinkKind::Parent)
            .map(|link| link.target_id.clone())
    };
    assert_eq!(parent_of("Root"), None);
    assert_eq!(parent_of("Mid"), Some(by_name("Root").id.clone()));
    assert_eq!(parent_of("Leaf"), Some(by_name("Mid").id.clone()));
    assert_ne!(by_name("Leaf").id, leaf);
}

#[tokio::test]
async fn archive_should_round_trip_a_file_of_the_cap_byte_identical() {
    let fx = Fixture::new();
    fx.db
        .update_instance_settings(|settings| settings.quotas.stored_mib_per_account = 1024)
        .expect("quotas");
    let task = fx.db.board_tasks(&fx.board).expect("tasks")[0].id.clone();
    let mut state = 0x2545_f491_u32;
    let bytes: Vec<u8> = (0..Db::MAX_ATTACHMENT_BYTES)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    fx.db
        .add_attachment(
            &task,
            "cap.bin",
            "application/octet-stream",
            &bytes,
            Actor::SYSTEM,
        )
        .expect("attach")
        .expect("task");
    let archive = fx.archive().await;

    let made = fx.import(&fx.member_provision, archive).await;

    assert_eq!(made.status, StatusCode::OK, "{}", made.message());
    let id = made.json()["id"].as_str().expect("id").to_string();
    let imported = fx.db.board_tasks(&id).expect("tasks");
    let attachment = imported[0]
        .attachments
        .iter()
        .find(|attachment| attachment.name == "cap.bin")
        .expect("attachment");
    assert_eq!(attachment.size, Db::MAX_ATTACHMENT_BYTES);
    let stored = fx
        .db
        .read_attachment_data(&attachment.id)
        .expect("read")
        .expect("bytes");
    assert!(stored == bytes, "imported bytes differ");
}

/// The import limit before it was raised, in bytes.
const FORMER_ARCHIVE_CAP_BYTES: usize = 64 * 1024 * 1024;

#[tokio::test]
async fn archive_should_round_trip_two_files_of_the_cap_byte_identical() {
    let fx = Fixture::new();
    fx.db
        .update_instance_settings(|settings| settings.quotas.stored_mib_per_account = 1024)
        .expect("quotas");
    let task = fx.db.board_tasks(&fx.board).expect("tasks")[0].id.clone();
    let mut files = Vec::new();
    for (name, seed) in [("one.bin", 0x2545_f491_u32), ("two.bin", 0x9e37_79b9_u32)] {
        let mut state = seed;
        let bytes: Vec<u8> = (0..Db::MAX_ATTACHMENT_BYTES)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        fx.db
            .add_attachment(
                &task,
                name,
                "application/octet-stream",
                &bytes,
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
        files.push((name, bytes));
    }
    let archive = fx.archive().await;
    assert!(
        archive.len() > FORMER_ARCHIVE_CAP_BYTES,
        "{}",
        archive.len()
    );
    let made = fx.import(&fx.member_provision, archive).await;

    assert_eq!(made.status, StatusCode::OK, "{}", made.message());
    let id = made.json()["id"].as_str().expect("id").to_string();
    let imported = fx.db.board_tasks(&id).expect("tasks");
    for (name, bytes) in files {
        let attachment = imported[0]
            .attachments
            .iter()
            .find(|attachment| attachment.name == name)
            .expect("attachment");
        let stored = fx
            .db
            .read_attachment_data(&attachment.id)
            .expect("read")
            .expect("bytes");
        assert!(stored == bytes, "{name} differs");
    }
}

#[tokio::test]
async fn archive_should_round_trip_a_documents_versions_and_edited_time() {
    let fx = Fixture::new();
    let task = fx.db.board_tasks(&fx.board).expect("tasks")[0].id.clone();
    let made = fx
        .db
        .add_document(&task, "notes", "one", Actor::SYSTEM)
        .expect("add")
        .expect("task");
    for text in ["two", "three"] {
        fx.db
            .save_document(&task, &made.id, None, text, Actor::SYSTEM)
            .expect("save")
            .expect("document");
    }
    let source = fx.db.board_tasks(&fx.board).expect("tasks")[0]
        .attachments
        .iter()
        .find(|attachment| attachment.id == made.id)
        .expect("document")
        .clone();
    assert_eq!(source.versions.len(), 2);
    let archive = fx.archive().await;
    let before = fx.member_state().1.unwrap_or_default();

    let answer = fx.import(&fx.member_provision, archive).await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.message());
    let held = fx.member_state().1.unwrap_or_default() - before;
    assert!(held >= source.stored_bytes(), "{held} bytes counted");
    let id = answer.json()["id"].as_str().expect("id").to_string();
    let imported = fx.db.board_tasks(&id).expect("tasks");
    let document = imported[0]
        .attachments
        .iter()
        .find(|attachment| attachment.is_document())
        .expect("document");
    assert_eq!(document.edited_at, source.edited_at);
    assert_eq!(document.versions.len(), 2);
    let read = |file: &str| {
        let bytes = fx
            .db
            .read_attachment_data(file)
            .expect("read")
            .expect("bytes");
        String::from_utf8(bytes).expect("text")
    };
    assert_eq!(read(&document.id), "three");
    for (kept, original) in document.versions.iter().zip(&source.versions) {
        assert_ne!(kept.id, original.id);
        assert_eq!(kept.saved_at, original.saved_at);
        assert_eq!(kept.size, original.size);
    }
    assert_eq!(read(&document.versions[0].id), "one");
    assert_eq!(read(&document.versions[1].id), "two");
}

#[tokio::test]
async fn archive_should_round_trip_a_tasks_cover() {
    let fx = Fixture::new();
    let task = fx.db.board_tasks(&fx.board).expect("tasks")[0].id.clone();
    let first = fx
        .db
        .add_attachment(&task, "a.png", "image/png", b"first", Actor::SYSTEM)
        .expect("add")
        .expect("task");
    let second = fx
        .db
        .add_attachment(&task, "b.png", "image/png", b"second", Actor::SYSTEM)
        .expect("add")
        .expect("task");
    fx.db
        .set_cover(&task, &second.id, CoverChange::Set, Actor::SYSTEM)
        .expect("cover")
        .expect("image");
    assert_ne!(first.id, second.id);
    let archive = fx.archive().await;

    let answer = fx.import(&fx.member_provision, archive).await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.message());
    let id = answer.json()["id"].as_str().expect("id").to_string();
    let imported = fx.db.board_tasks(&id).expect("tasks");
    let cover = imported[0].cover_id.as_deref().expect("cover");
    let covered = imported[0]
        .attachments
        .iter()
        .find(|attachment| attachment.id == cover)
        .expect("the cover is an attachment of the task");
    assert_eq!(covered.name, "b.png");
}
