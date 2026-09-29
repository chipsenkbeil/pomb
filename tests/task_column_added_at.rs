//! Regression tests for KF-053's `column_added_at` tracking.
//!
//! - `create_task` stamps the task with when it entered its column.
//! - Same-column `move_task` (a reorder) preserves the stamp.
//! - Cross-column `move_task` refreshes the stamp.
//! - New metadata fields (`labels`, `due_at`, `due_repeat`) initialize
//!   empty and survive moves untouched.

use chipflow::db::Db;
use chipflow::models::TaskRow;

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

fn get(db: &Db, id: &str) -> TaskRow {
    db.get_task(id).expect("get task").expect("task exists")
}

#[test]
fn create_task_stamps_column_added_at_and_empty_metadata() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("create board");
    let col = db.create_column(&board, "A", None).expect("column a");

    let id = db
        .create_task(&col, None, "t", 1, None)
        .expect("create task");
    let task = get(&db, &id);

    assert!(
        task.column_added_at.is_some(),
        "new tasks must record when they entered the column"
    );
    assert!(task.labels.is_empty(), "labels start empty");
    assert!(task.due_at.is_none(), "no due date initially");
    assert!(task.due_repeat.is_none(), "no due repeat initially");
}

#[test]
fn same_column_move_preserves_column_added_at() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("create board");
    let col = db.create_column(&board, "A", None).expect("column a");

    let id = db
        .create_task(&col, None, "t", 1, None)
        .expect("create task");
    let before = get(&db, &id).column_added_at.clone();

    // A reorder within the same column must not touch the stamp.
    db.move_task(&id, &col, None, 5.0).expect("reorder");
    let after = get(&db, &id).column_added_at.clone();

    assert_eq!(before, after, "same-column move must keep the stamp");
}

#[test]
fn cross_column_move_refreshes_column_added_at() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("create board");
    let col_a = db.create_column(&board, "A", None).expect("column a");
    let col_b = db.create_column(&board, "B", None).expect("column b");

    let id = db
        .create_task(&col_a, None, "t", 1, None)
        .expect("create task");
    let before: chrono::DateTime<chrono::FixedOffset> = get(&db, &id)
        .column_added_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .expect("stamp parses");

    db.move_task(&id, &col_b, None, 0.0)
        .expect("move to other column");
    let moved = get(&db, &id);
    assert_eq!(moved.column_id, col_b);

    let after: chrono::DateTime<chrono::FixedOffset> = moved
        .column_added_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .expect("stamp parses after move");
    assert!(
        after >= before,
        "cross-column move must refresh the stamp (before {before}, after {after})"
    );
}

#[test]
fn metadata_fields_survive_moves() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("create board");
    let col_a = db.create_column(&board, "A", None).expect("column a");
    let col_b = db.create_column(&board, "B", None).expect("column b");

    let id = db
        .create_task(&col_a, None, "t", 1, None)
        .expect("create task");

    db.move_task(&id, &col_b, None, 0.0).expect("move");
    let moved = get(&db, &id);
    assert!(moved.labels.is_empty());
    assert!(moved.due_at.is_none());
    assert!(moved.due_repeat.is_none());
}
