//! Regression test: `db.move_task` position collision.
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
fn move_task_mid_list_drops_should_land_exactly_at_index() {
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

    // Same-column reorder to a later non-end position
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

    // Drop at index 0 (front)
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

    // Positions stay dense 0..n with no collisions after every move
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

    // Out-of-range indices clamp instead of corrupting order
    db.move_task(&y, &col_a, None, 99.0).expect("move y to 99");
    assert_eq!(
        column_task_names(&db, &board, &col_a),
        ["a", "x", "c", "b", "d", "y"],
        "KF-120: oversized index clamps to the end"
    );
}

/// A cross-board move without a swimlane lands in the target board's first lane, so the card is
/// visible on the target board.
#[test]
fn move_task_across_boards_should_take_the_target_boards_lane() {
    let (_dir, db) = test_db();
    let from = db.create_board("From").expect("from board");
    let to = db.create_board("To").expect("to board");
    db.ensure_default_swimlane(&from).expect("from lane");
    db.ensure_default_swimlane(&to).expect("to lane");
    let from_lane = db.list_swimlanes(&from).expect("lanes")[0].id.clone();
    let to_lane = db.list_swimlanes(&to).expect("lanes")[0].id.clone();
    let from_col = db.create_column(&from, "Doing", None).expect("from column");
    let to_col = db.create_column(&to, "Doing", None).expect("to column");
    let task = db
        .create_task(&from_col, Some(&from_lane), "travel", 1, None)
        .expect("task");

    db.move_task(&task, &to_col, None, 1e9).expect("move");

    let moved = db.get_task(&task).expect("get").expect("task exists");
    assert_eq!(moved.column_id, to_col, "column on the target board");
    assert_eq!(
        moved.swimlane_id.as_deref(),
        Some(to_lane.as_str()),
        "lane on the target board"
    );
    assert!(
        db.board_tasks(&to)
            .expect("to tasks")
            .iter()
            .any(|t| t.id == task),
        "listed on the target board"
    );

    // Same-board moves without a lane still keep the current lane
    let second = db.create_swimlane(&to, "Second").expect("second lane");
    db.move_task(&task, &to_col, Some(&second), 0.0)
        .expect("move to second lane");
    db.move_task(&task, &to_col, None, 0.0)
        .expect("move in place");
    let kept = db.get_task(&task).expect("get").expect("task exists");
    assert_eq!(
        kept.swimlane_id.as_deref(),
        Some(second.as_str()),
        "lane kept"
    );
}

/// A cross-board move takes the target board's color of the same value, or the target board's
/// default when it has no such color.
#[test]
fn move_task_across_boards_should_take_the_target_boards_color() {
    let (_dir, db) = test_db();
    let from = db.create_board("From").expect("from board");
    let to = db.create_board("To").expect("to board");
    let from_col = db.create_column(&from, "Doing", None).expect("from column");
    let to_col = db.create_column(&to, "Doing", None).expect("to column");
    let color = |board: &str, value: &str| {
        db.list_colors(board)
            .expect("colors")
            .into_iter()
            .find(|color| color.value == value)
            .expect("color")
    };

    let green = color(&from, "green");
    let task = db
        .create_task(&from_col, None, "travel", 2, Some(&green.id))
        .expect("task");
    db.move_task(&task, &to_col, None, 0.0).expect("move");
    let moved = db.get_task(&task).expect("get").expect("task exists");
    assert_eq!(moved.color_id, Some(color(&to, "green").id));

    let blue = color(&to, "blue");
    db.update_task(&task, None, None, None, Some(Some(&blue.id)))
        .expect("blue");
    db.delete_color(&color(&from, "blue").id)
        .expect("drop blue");
    db.move_task(&task, &from_col, None, 0.0)
        .expect("move back");
    let back = db.get_task(&task).expect("get").expect("task exists");
    let default = db
        .default_color(&from)
        .expect("default")
        .expect("has default");
    assert_eq!(back.color_id, Some(default.id));
}
