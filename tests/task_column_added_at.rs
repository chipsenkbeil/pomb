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
fn create_task_should_stamp_column_added_at_and_empty_metadata() {
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
fn same_column_move_should_preserve_column_added_at() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("create board");
    let col = db.create_column(&board, "A", None).expect("column a");

    let id = db
        .create_task(&col, None, "t", 1, None)
        .expect("create task");
    let before = get(&db, &id).column_added_at.clone();

    // A reorder within the same column must not touch the stamp
    db.move_task(&id, &col, None, 5.0).expect("reorder");
    let after = get(&db, &id).column_added_at.clone();

    assert_eq!(before, after, "same-column move must keep the stamp");
}

#[test]
fn cross_column_move_should_refresh_column_added_at() {
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
fn metadata_fields_should_survive_moves() {
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

/// A task created in a done column, directly or from a template, has a completion time, and one
/// created elsewhere does not.
#[test]
fn tasks_created_in_a_done_column_should_be_complete() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("board");
    let todo = db.create_column(&board, "To-do", None).expect("todo");
    let finished = db.create_column(&board, "Finished", None).expect("done");
    db.set_column_done(&finished, true).expect("set done");

    let open = db.create_task(&todo, None, "open", 1, None).expect("open");
    assert!(get(&db, &open).completed_at.is_none());
    let closed = db
        .create_task(&finished, None, "closed", 1, None)
        .expect("closed");
    let closed = get(&db, &closed);
    assert!(closed.completed_at.is_some());
    assert_eq!(closed.completed_at, closed.column_added_at);

    let template = db
        .save_board_as_template(&board, "Copy", "")
        .expect("save")
        .expect("board known");
    let copy = db
        .apply_template(&template, "Copy")
        .expect("apply")
        .expect("template known");
    let tasks = db.board_tasks(&copy).expect("tasks");
    let by_name = |name: &str| {
        tasks
            .iter()
            .find(|task| task.name == name)
            .expect("copied task")
            .clone()
    };
    assert!(by_name("open").completed_at.is_none());
    assert!(by_name("closed").completed_at.is_some());
}
