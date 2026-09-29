//! Data-layer tests for task subtasks, member assignment, and grouping dates
//! (KanbanFlow parity: KF-058, KF-060, KF-050 grouping date).
//!
//! Covers: subtask add/update/remove round trips, toggling done, subtask
//! name validation surface (empty names rejected at the handler layer),
//! member assignment persistence, grouping-date persistence, and
//! persistence of all three across reconnects.

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
fn subtask_crud_round_trip() {
    let (_dir, db) = test_db();
    let task_id = task_in_column(&db);

    // New tasks start with no subtasks.
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert!(task.subtasks.is_empty());

    // Add two subtasks; ids are unique.
    let first = db
        .add_subtask(&task_id, "First")
        .expect("add")
        .expect("task exists");
    let second = db
        .add_subtask(&task_id, "Second")
        .expect("add")
        .expect("task exists");
    assert_ne!(first.id, second.id);
    assert_eq!(first.name, "First");
    assert!(!first.done);

    let task = db.get_task(&task_id).expect("get").expect("task");
    assert_eq!(task.subtasks.len(), 2);
    assert_eq!(task.subtasks[0].name, "First");

    // Rename the first.
    assert!(db
        .update_subtask(&task_id, &first.id, Some("First!"), None)
        .expect("rename"));
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert_eq!(task.subtasks[0].name, "First!");

    // Toggle done on the second.
    assert!(db
        .update_subtask(&task_id, &second.id, None, Some(true))
        .expect("toggle"));
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert!(task.subtasks[1].done);

    // Remove the first; the second survives.
    assert!(db.remove_subtask(&task_id, &first.id).expect("remove"));
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert_eq!(task.subtasks.len(), 1);
    assert_eq!(task.subtasks[0].id, second.id);

    // Unknown ids are reported, not errors.
    assert!(!db
        .update_subtask(&task_id, "nope", Some("x"), None)
        .expect("update missing"));
    assert!(!db.remove_subtask(&task_id, "nope").expect("remove missing"));
    assert!(db
        .add_subtask("nope", "x")
        .expect("add to missing")
        .is_none());
}

#[test]
fn member_assignment_and_grouping_date_persist() {
    let (dir, db) = test_db();
    let task_id = task_in_column(&db);
    let user_id = db.create_user("chip", "hash").expect("create user");

    // Defaults are empty.
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert!(task.member_ids.is_empty());
    assert!(task.grouping_date.is_none());

    // Assign the member and set a grouping date.
    assert!(db
        .set_task_members(&task_id, std::slice::from_ref(&user_id))
        .expect("assign"));
    assert!(db
        .set_grouping_date(&task_id, Some("2026-10-05"))
        .expect("grouping date"));
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert_eq!(task.member_ids, vec![user_id.clone()]);
    assert_eq!(task.grouping_date.as_deref(), Some("2026-10-05"));

    // Unassign everyone and clear the date.
    assert!(db.set_task_members(&task_id, &[]).expect("unassign"));
    assert!(db.set_grouping_date(&task_id, None).expect("clear"));
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert!(task.member_ids.is_empty());
    assert!(task.grouping_date.is_none());

    // Missing tasks report false instead of erroring.
    assert!(!db.set_task_members("nope", &[]).expect("missing task"));
    assert!(!db
        .set_grouping_date("nope", Some("2026-10-05"))
        .expect("missing task"));

    // State survives a reconnect.
    assert!(db
        .set_task_members(&task_id, std::slice::from_ref(&user_id))
        .expect("assign"));
    assert!(db
        .set_grouping_date(&task_id, Some("2026-10-05"))
        .expect("grouping date"));
    let sub = db
        .add_subtask(&task_id, "Persistent")
        .expect("add")
        .expect("task exists");
    drop(db);
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("reconnect");
    let task = db.get_task(&task_id).expect("get").expect("task");
    assert_eq!(task.member_ids, vec![user_id]);
    assert_eq!(task.grouping_date.as_deref(), Some("2026-10-05"));
    assert_eq!(task.subtasks.len(), 1);
    assert_eq!(task.subtasks[0].id, sub.id);
}
