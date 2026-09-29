//! Data-layer tests for the Timer settings / Boards sidebar additions
//! (KF-075, KF-076, KF-083, KF-088).
//!
//! Covers: settings backward compatibility for `break_activities` and
//! `favorite_boards` (absent fields deserialize to empty), activity and
//! favorite round trips through `update_settings`/`get_settings`, and
//! `copy_colors` palette replacement with task color remapping by value.

use chipflow::db::Db;
use chipflow::models::{BreakActivity, Settings};

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

#[test]
fn settings_new_fields_default_to_empty() {
    let settings = Settings::default();
    assert!(settings.break_activities.is_empty());
    assert!(settings.favorite_boards.is_empty());
}

#[test]
fn settings_legacy_json_deserializes_without_new_fields() {
    // A settings row written before break_activities / favorite_boards
    // existed must still load, with the new fields defaulting to empty.
    let legacy = r#"{
        "pomodoro_minutes": 25,
        "short_break_minutes": 5,
        "long_break_minutes": 15,
        "long_break_every": 4,
        "ding_enabled": true,
        "ticking_mode": "never",
        "alarm_sound": "bell",
        "alarm_volume": 70,
        "points_volume": 70,
        "sounds_enabled": true,
        "pip_enabled": true,
        "notifications_enabled": false,
        "interrupt_reasons": ["phone call"]
    }"#;
    let settings: Settings = serde_json::from_str(legacy).expect("legacy settings parse");
    assert!(settings.break_activities.is_empty());
    assert!(settings.favorite_boards.is_empty());
    assert_eq!(settings.interrupt_reasons, vec!["phone call".to_string()]);
}

#[test]
fn break_activities_round_trip() {
    let (_dir, db) = test_db();
    let mut settings = db.get_settings().expect("get settings");
    settings.break_activities = vec![
        BreakActivity {
            id: "act-1".to_string(),
            name: "Coffee".to_string(),
            description: "fresh cup".to_string(),
            daily_goal: 3,
            daily_limit: Some(5),
        },
        BreakActivity {
            id: "act-2".to_string(),
            name: "Stretch".to_string(),
            description: String::new(),
            daily_goal: 1,
            daily_limit: None,
        },
    ];
    db.update_settings(&settings).expect("update settings");

    let loaded = db.get_settings().expect("get settings");
    assert_eq!(loaded.break_activities.len(), 2);
    assert_eq!(loaded.break_activities[0].name, "Coffee");
    assert_eq!(loaded.break_activities[0].daily_limit, Some(5));
    assert_eq!(loaded.break_activities[1].daily_limit, None);
}

#[test]
fn favorite_boards_round_trip() {
    let (_dir, db) = test_db();
    let board_a = db.create_board("Alpha").expect("create board");
    let board_b = db.create_board("Beta").expect("create board");

    let mut settings = db.get_settings().expect("get settings");
    settings.favorite_boards = vec![board_b.clone(), board_a.clone()];
    db.update_settings(&settings).expect("update settings");

    let loaded = db.get_settings().expect("get settings");
    assert_eq!(loaded.favorite_boards, vec![board_b, board_a]);
}

#[test]
fn copy_colors_replaces_palette_and_remaps_task_colors() {
    let (_dir, db) = test_db();
    let source_id = db.create_board("Source").expect("source board");
    let target_id = db.create_board("Target").expect("target board");

    // Customize the source palette: relabel blue, disable green, and make
    // red the default instead of yellow.
    let source_colors = db.list_colors(&source_id).expect("source colors");
    let blue = source_colors
        .iter()
        .find(|c| c.value == "blue")
        .expect("blue seeded");
    let green = source_colors
        .iter()
        .find(|c| c.value == "green")
        .expect("green seeded");
    let red = source_colors
        .iter()
        .find(|c| c.value == "red")
        .expect("red seeded");
    db.update_color(&blue.id, Some("Deep work"), None, None, None)
        .expect("relabel blue");
    db.update_color(&green.id, None, None, Some(false), None)
        .expect("disable green");
    db.update_color(&red.id, None, None, None, Some(true))
        .expect("red becomes default");

    // A task on the target board painted blue keeps its color by value.
    let column_id = db
        .create_column(&target_id, "To-do", None)
        .expect("create column");
    let target_blue = db
        .list_colors(&target_id)
        .expect("target colors")
        .into_iter()
        .find(|c| c.value == "blue")
        .expect("target blue");
    let task_id = db
        .create_task(&column_id, None, "Blue task", 1, Some(&target_blue.id))
        .expect("create task");

    let copied = db.copy_colors(&target_id, &source_id).expect("copy colors");
    let source_len = db.list_colors(&source_id).expect("source len").len();
    assert_eq!(copied, source_len, "all source colors are copied");

    // The target palette now mirrors the source: labels, enabled flags,
    // default, and order — with fresh ids.
    let target_colors = db.list_colors(&target_id).expect("target colors");
    assert_eq!(target_colors.len(), source_len);
    let new_blue = target_colors
        .iter()
        .find(|c| c.value == "blue")
        .expect("copied blue");
    assert_eq!(new_blue.label, "Deep work");
    assert_ne!(new_blue.id, target_blue.id, "copies get fresh ids");
    let new_green = target_colors
        .iter()
        .find(|c| c.value == "green")
        .expect("copied green");
    assert!(!new_green.enabled, "disabled flag is preserved");
    let defaults: Vec<_> = target_colors.iter().filter(|c| c.is_default).collect();
    assert_eq!(defaults.len(), 1, "exactly one default");
    assert_eq!(defaults[0].value, "red", "default follows the source");

    // The task still renders blue: its color id was remapped to the copy.
    let task = db.get_task(&task_id).expect("get task").expect("exists");
    assert_eq!(
        task.color_id.as_deref(),
        Some(new_blue.id.as_str()),
        "task color remapped to the copied row"
    );
}

#[test]
fn copy_colors_rejects_unknown_source() {
    let (_dir, db) = test_db();
    let target_id = db.create_board("Target").expect("target board");
    let err = db
        .copy_colors(&target_id, "no-such-board")
        .expect_err("unknown source must fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("no-such-board"),
        "error names the missing board: {msg}"
    );
    // The failed copy must not have touched the target palette.
    assert_eq!(
        db.list_colors(&target_id).expect("target colors").len(),
        10,
        "target palette untouched"
    );
}
