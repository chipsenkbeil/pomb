//! Regression test for KF-149: a board must always have at least one swimlane.
//!
//! A 2026-09-29 browser pass found that a board created without a template
//! gets zero swimlanes, which leaves every column "+" add-task button
//! silently dead (the add-task form template clones into the first
//! swimlane row's `.task-list` cell; with none present the click handler
//! returns early with no feedback). The fix: `ensure_default_swimlane`
//! backfills a "Default" lane, and the last lane on a board can no longer
//! be deleted.

use chipflow::db::Db;

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

#[test]
fn ensure_default_swimlane_backfills_exactly_one() {
    let (_dir, db) = test_db();
    let board = db.create_board("Blank").expect("create board");

    // A freshly created board has no swimlanes until the ensure runs.
    assert!(
        db.list_swimlanes(&board).expect("lanes").is_empty(),
        "create_board alone seeds no lanes"
    );

    db.ensure_default_swimlane(&board).expect("ensure");
    let lanes = db.list_swimlanes(&board).expect("lanes");
    assert_eq!(lanes.len(), 1, "exactly one lane backfilled");
    assert_eq!(lanes[0].name, "Default", "lane name");

    // Idempotent: a second call must not add another lane.
    db.ensure_default_swimlane(&board).expect("ensure again");
    assert_eq!(
        db.list_swimlanes(&board).expect("lanes").len(),
        1,
        "ensure is idempotent"
    );

    // Boards that already have lanes are untouched.
    db.create_swimlane(&board, "Extra").expect("extra lane");
    db.ensure_default_swimlane(&board).expect("ensure");
    assert_eq!(
        db.list_swimlanes(&board).expect("lanes").len(),
        2,
        "existing lanes untouched"
    );
}

#[test]
fn ensure_default_swimlane_ignores_missing_board() {
    let (_dir, db) = test_db();
    db.ensure_default_swimlane("no-such-board")
        .expect("ensure on missing board is a no-op");
    assert!(
        db.list_swimlanes("no-such-board")
            .expect("lanes")
            .is_empty(),
        "no lane created for a missing board"
    );
}

#[test]
fn deleting_lanes_keeps_at_least_one_per_board() {
    let (_dir, db) = test_db();
    let board = db.create_board("Blank").expect("create board");
    db.ensure_default_swimlane(&board).expect("ensure");
    let first = db.list_swimlanes(&board).expect("lanes")[0].id.clone();
    let second = db.create_swimlane(&board, "Second").expect("second lane");

    // With two lanes, deleting one is fine.
    assert!(db.delete_swimlane(&first).expect("delete first"));
    assert_eq!(
        db.list_swimlanes(&board).expect("lanes").len(),
        1,
        "one lane remains"
    );

    // The route layer must refuse to delete this last lane (KF-149 guard:
    // list_swimlanes(board).len() <= 1). Pin the db-level precondition the
    // guard relies on.
    let remaining = db.list_swimlanes(&board).expect("lanes");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, second);
    assert_eq!(remaining[0].board_id, board);
}
