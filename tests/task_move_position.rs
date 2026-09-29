//! Regression test for KF-120: `db.move_task` position collision.
//!
//! Dropping a card at a non-end position used to write the client-sent
//! index straight onto the row, colliding with the displaced task's
//! position; the renumber then broke ties by task id, which could persist
//! the card one slot off from the drop point (the DOM looked right until
//! reload). `move_task` now inserts the moved task at the exact requested
//! index with its siblings renumbered densely around it.

use chipflow::db::Db;

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

/// Names of one column's tasks in stored position order.
fn column_task_names(db: &Db, board_id: &str, column_id: &str) -> Vec<String> {
    let mut tasks: Vec<_> = db
        .board_tasks(board_id)
        .expect("board tasks")
        .into_iter()
        .filter(|task| task.column_id == column_id)
        .collect();
    tasks.sort_by(|a, b| a.position.total_cmp(&b.position));
    tasks.into_iter().map(|task| task.name).collect()
}

fn make_task(db: &Db, column_id: &str, name: &str) -> String {
    db.create_task(column_id, None, name, 1, None)
        .expect("create task")
}

#[test]
fn move_task_mid_list_drops_land_exactly_at_index() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("create board");
    let col_a = db.create_column(&board, "A", None).expect("column a");
    let col_b = db.create_column(&board, "B", None).expect("column b");

    for name in ["a", "b", "c", "d"] {
        make_task(&db, &col_a, name);
    }
    for name in ["x", "y"] {
        make_task(&db, &col_b, name);
    }
    assert_eq!(
        column_task_names(&db, &board, &col_a),
        ["a", "b", "c", "d"],
        "setup order"
    );

    // Cross-column drop at a non-end position: the card must land exactly
    // where dropped, not collide with the displaced task's slot.
    let x = db
        .board_tasks(&board)
        .expect("tasks")
        .into_iter()
        .find(|task| task.name == "x")
        .expect("task x")
        .id;
    db.move_task(&x, &col_a, None, 1.0).expect("move x to 1");
    assert_eq!(
        column_task_names(&db, &board, &col_a),
        ["a", "x", "b", "c", "d"],
        "KF-120: cross-column drop at index 1"
    );

    // Same-column reorder to a later non-end position.
    let b = db
        .board_tasks(&board)
        .expect("tasks")
        .into_iter()
        .find(|task| task.name == "b")
        .expect("task b")
        .id;
    db.move_task(&b, &col_a, None, 3.0).expect("move b to 3");
    assert_eq!(
        column_task_names(&db, &board, &col_a),
        ["a", "x", "c", "b", "d"],
        "KF-120: same-column move to index 3"
    );

    // Drop at index 0 (front).
    let y = db
        .board_tasks(&board)
        .expect("tasks")
        .into_iter()
        .find(|task| task.name == "y")
        .expect("task y")
        .id;
    db.move_task(&y, &col_a, None, 0.0).expect("move y to 0");
    assert_eq!(
        column_task_names(&db, &board, &col_a),
        ["y", "a", "x", "c", "b", "d"],
        "KF-120: cross-column drop at index 0"
    );

    // Positions stay dense 0..n with no collisions after every move.
    let mut positions: Vec<i64> = db
        .board_tasks(&board)
        .expect("tasks")
        .into_iter()
        .filter(|task| task.column_id == col_a)
        .map(|task| task.position as i64)
        .collect();
    positions.sort_unstable();
    assert_eq!(
        positions,
        [0, 1, 2, 3, 4, 5],
        "KF-120: positions dense after moves"
    );

    // Out-of-range indices clamp instead of corrupting order.
    db.move_task(&y, &col_a, None, 99.0).expect("move y to 99");
    assert_eq!(
        column_task_names(&db, &board, &col_a),
        ["a", "x", "c", "b", "d", "y"],
        "KF-120: oversized index clamps to the end"
    );
}
