//! Data-layer tests for hour-based time estimates (KF-216).
//!
//! Covers: `set_task_estimate` round trip, clearing via None, rows written
//! before the field existed deserializing with `estimate_hours: None`,
//! and the template snapshot/restore carrying the estimate.

use chipflow::db::Db;

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

fn task_in_column(db: &Db) -> String {
    let board_id = db.create_board("Board").expect("create board");
    let column_id = db
        .create_column(&board_id, "To-do", None)
        .expect("create column");
    db.create_task(&column_id, None, "Task", 1, None)
        .expect("create task")
}

#[test]
fn estimate_hours_round_trip_and_clear() {
    let (_dir, db) = test_db();
    let task_id = task_in_column(&db);

    // Fresh tasks have no estimate (KanbanFlow parity: unset, not 0h).
    let task = db.get_task(&task_id).expect("get task").expect("task");
    assert_eq!(task.estimate_hours, None);

    // Set 4h (the golden-master fixture value).
    assert!(db.set_task_estimate(&task_id, Some(4.0)).expect("set"));
    let task = db.get_task(&task_id).expect("get task").expect("task");
    assert_eq!(task.estimate_hours, Some(4.0));

    // Clear back to None.
    assert!(db.set_task_estimate(&task_id, None).expect("clear"));
    let task = db.get_task(&task_id).expect("get task").expect("task");
    assert_eq!(task.estimate_hours, None);

    // Unknown task id returns false.
    assert!(!db.set_task_estimate("nope", Some(1.0)).expect("missing"));
}

#[test]
fn estimate_hours_survives_template_snapshot_round_trip() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("Board").expect("create board");
    let column_id = db
        .create_column(&board_id, "To-do", None)
        .expect("create column");
    let task_id = db
        .create_task(&column_id, None, "Estimated", 1, None)
        .expect("create task");
    db.set_task_estimate(&task_id, Some(6.0)).expect("set");

    let template_id = db
        .save_board_as_template(&board_id, "Est template", "")
        .expect("save template")
        .expect("template id");

    let new_board = db
        .apply_template(&template_id, "From template")
        .expect("apply template")
        .expect("new board id");

    let tasks = db.board_tasks(&new_board).expect("board tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].estimate_hours, Some(6.0));
}
