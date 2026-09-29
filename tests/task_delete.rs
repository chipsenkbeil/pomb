//! Regression test for KF-148: task deletion must actually delete.
//!
//! A 2026-09-29 browser pass reported task deletion "completely broken"
//! (modal Delete + context-menu Delete failing silently). Investigation
//! showed the server-side `DELETE /api/tasks/{id}` works (live curl:
//! 200, subsequent GET 404) and the modal delete path had already been
//! verified end-to-end in real Chromium the same day (KF-094: "Delete
//! deletes (after confirm)") — the silent failure matches a test
//! environment auto-dismissing `window.confirm()`, which aborts both
//! client paths before any request is sent. This test pins the
//! server-side contract so a real regression can't hide: deleting a task
//! removes the row, its column index entry, and its time entries; a
//! second delete reports "not found".

use chipflow::db::Db;

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

#[test]
fn delete_task_removes_row_index_and_time_entries() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("create board");
    let col = db
        .create_column(&board, "Todo", None)
        .expect("create column");
    let task = db
        .create_task(&col, None, "Delete me", 1, None)
        .expect("create task");

    assert!(db.get_task(&task).expect("get").is_some(), "setup");

    // A time entry on the task must be cleaned up by the delete.
    let entry = db.create_entry(&task, 25, "pomodoro").expect("entry");
    assert_eq!(db.list_entries(&task).expect("entries").len(), 1);

    // First delete removes the task.
    assert!(db.delete_task(&task).expect("delete"), "deleted=true");
    assert!(
        db.get_task(&task).expect("get").is_none(),
        "task row gone after delete"
    );
    assert!(
        !db.board_tasks(&board)
            .expect("board tasks")
            .iter()
            .any(|t| t.id == task),
        "task gone from column index"
    );
    assert!(
        db.list_entries(&task).expect("entries").is_empty(),
        "time entries cleaned up"
    );
    assert!(
        db.all_entries()
            .expect("all entries")
            .iter()
            .all(|e| e.id != entry),
        "entry row gone"
    );

    // Deleting again reports not-found rather than erroring.
    assert!(!db.delete_task(&task).expect("re-delete"), "deleted=false");
}

#[test]
fn delete_task_unknown_id_reports_not_found() {
    let (_dir, db) = test_db();
    assert!(
        !db.delete_task("no-such-task").expect("delete"),
        "unknown id -> false"
    );
}
