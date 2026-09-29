//! Data-layer and model tests for timer settings and board deletion.
//!
//! Covers: Settings defaults for older stored JSON (backward compatibility),
//! Settings::default() values, board deletion cascades, and the last-board
//! deletion refusal.

use chipflow::db::Db;
use chipflow::models::Settings;

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

#[test]
fn settings_defaults_match_kanbanflow_parity() {
    let s = Settings::default();
    assert_eq!(s.pomodoro_minutes, 25);
    assert_eq!(s.short_break_minutes, 5);
    assert_eq!(s.long_break_minutes, 15);
    assert_eq!(s.ticking_mode, "never");
    assert_eq!(s.alarm_sound, "bell");
    assert_eq!(s.alarm_volume, 70);
    assert_eq!(s.points_volume, 70);
    assert!(s.sounds_enabled);
    assert!(s.pip_enabled);
}

#[test]
fn settings_deserialize_older_json() {
    // Older stored JSON lacks the six new sound/PiP fields; serde defaults
    // must fill them in so old databases keep working.
    let json = r#"{
        "pomodoro_minutes": 25,
        "short_break_minutes": 5,
        "long_break_minutes": 15,
        "long_break_every": 4,
        "ding_enabled": true,
        "notifications_enabled": true,
        "interrupt_reasons": ["Phone call"]
    }"#;
    let s: Settings = serde_json::from_str(json).expect("deserialize older settings");
    assert_eq!(s.pomodoro_minutes, 25);
    assert_eq!(s.ticking_mode, "never");
    assert_eq!(s.alarm_sound, "bell");
    assert_eq!(s.alarm_volume, 70);
    assert_eq!(s.points_volume, 70);
    assert!(s.sounds_enabled);
    assert!(s.pip_enabled);
}

#[test]
fn settings_round_trip_preserves_new_fields() {
    let (_dir, db) = test_db();
    let s = Settings {
        ticking_mode: "always".to_string(),
        alarm_sound: "chime".to_string(),
        alarm_volume: 80,
        points_volume: 60,
        sounds_enabled: false,
        pip_enabled: false,
        ..Settings::default()
    };
    db.update_settings(&s).expect("update settings");
    let loaded = db.get_settings().expect("get settings");
    assert_eq!(loaded.ticking_mode, "always");
    assert_eq!(loaded.alarm_sound, "chime");
    assert_eq!(loaded.alarm_volume, 80);
    assert_eq!(loaded.points_volume, 60);
    assert!(!loaded.sounds_enabled);
    assert!(!loaded.pip_enabled);
}

#[test]
fn delete_board_cascades() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("To Delete").expect("create board");
    let other_id = db.create_board("Keep Me").expect("create other board");

    // Create some data on the board to be deleted.
    let col_id = db
        .create_column(&board_id, "Col", None)
        .expect("create column");
    let task_id = db
        .create_task(&col_id, None, "Task", 0, None)
        .expect("create task");
    db.create_entry(&task_id, 25, "pomodoro").expect("log time");

    db.delete_board(&board_id).expect("delete board");

    // Board is gone.
    assert!(db.get_board(&board_id).expect("get board").is_none());
    // Columns are gone.
    assert!(db.list_columns(&board_id).expect("list columns").is_empty());
    // Task is gone.
    assert!(db.get_task(&task_id).expect("get task").is_none());
    // Time entries are gone.
    assert!(db
        .all_entries()
        .expect("entries")
        .iter()
        .all(|e| e.task_id != task_id));
    // Colors are gone.
    assert!(db.list_colors(&board_id).expect("list colors").is_empty());

    // The other board is untouched.
    assert!(db.get_board(&other_id).expect("get other").is_some());
}

#[test]
fn delete_board_refuses_last_board() {
    let (_dir, db) = test_db();
    let boards = db.list_boards().expect("list boards");
    assert_eq!(boards.len(), 1, "fresh DB seeds exactly one board");
    let err = db.delete_board(&boards[0].id).expect_err("must refuse");
    assert!(
        err.to_string().contains("last"),
        "error mentions last board: {err}"
    );
    // Board still exists.
    assert!(db.get_board(&boards[0].id).expect("get").is_some());
}

#[test]
fn delete_board_refuses_unknown_board() {
    let (_dir, db) = test_db();
    let err = db
        .delete_board("does-not-exist")
        .expect_err("must refuse unknown");
    assert!(
        err.to_string().contains("not found"),
        "error mentions not found: {err}"
    );
}
