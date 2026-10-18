//! KF-418: grouping dates stored before the strict YYYY-MM-DD rule are
//! normalized when the database opens.
//! The first test covers the padding function on valid and invalid input.
//! The second plants old-format dates, reconnects, and reads them back.

use chipflow::db::{Db, normalize_grouping_date};

#[test]
fn normalize_grouping_date_should_pad_dates_and_reject_the_rest() {
    assert_eq!(
        normalize_grouping_date("2026-1-5").as_deref(),
        Some("2026-01-05")
    );
    assert_eq!(
        normalize_grouping_date(" 2026-10-05 ").as_deref(),
        Some("2026-10-05")
    );
    assert_eq!(
        normalize_grouping_date("2026-10-05").as_deref(),
        Some("2026-10-05")
    );
    for bad in [
        "",
        "garbage",
        "26-1-5",
        "2026-02-30",
        "2026-001-05",
        "2026-1-5T00:00",
        "+026-1-5",
    ] {
        assert_eq!(normalize_grouping_date(bad), None, "{bad:?}");
    }
}

#[test]
fn opening_the_database_should_normalize_stored_grouping_dates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let path = path.to_str().expect("utf8 path");
    let tasks = {
        let db = Db::connect(path).expect("connect");
        let board = db.create_board("Board").expect("board");
        let column = db.create_column(&board, "Done", None).expect("column");
        let task = |name: &str, grouping: Option<&str>| {
            let id = db.create_task(&column, None, name, 1, None).expect("task");
            db.set_grouping_date(&id, grouping).expect("grouping date");
            id
        };
        [
            task("unpadded", Some("2026-1-5")),
            task("garbage", Some("garbage")),
            task("strict", Some("2026-10-05")),
            task("none", None),
        ]
    };
    let db = Db::connect(path).expect("reconnect");
    let grouping = |id: &str| db.get_task(id).expect("get").expect("task").grouping_date;
    assert_eq!(grouping(&tasks[0]).as_deref(), Some("2026-01-05"));
    assert_eq!(grouping(&tasks[1]), None);
    assert_eq!(grouping(&tasks[2]).as_deref(), Some("2026-10-05"));
    assert_eq!(grouping(&tasks[3]), None);
}
