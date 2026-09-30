//! Data-layer tests for per-board task colors and board templates.
//!
//! Covers: color CRUD / ordering / enable-disable / default handling, the
//! 50-char label limit (enforced in the DB layer), task color assignment and
//! the default-on-create path, template save/apply round trips, persistence
//! across reconnects, lazy color backfill for older boards, seed idempotency,
//! and delete refusals (default color, in-use color, built-in template).

use chipflow::db::{ColorDeleteOutcome, Db, TemplateDeleteOutcome};

/// Fresh database in a temp dir. The dir is deleted when the guard drops.
fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

fn builtin_template_id(db: &Db) -> String {
    db.list_templates()
        .expect("list templates")
        .into_iter()
        .find(|template| template.built_in && template.name == Db::BUILTIN_TEMPLATE_NAME)
        .expect("built-in Pomodoro template seeded")
        .id
}

#[test]
fn colors_seed_with_pomodoro_defaults() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("Board").expect("create board");

    let colors = db.list_colors(&board_id).expect("list colors");
    assert_eq!(colors.len(), 10, "ten standard colors are backfilled");

    let defaults: Vec<_> = colors.iter().filter(|color| color.is_default).collect();
    assert_eq!(defaults.len(), 1, "exactly one default color");
    assert_eq!(defaults[0].value, "yellow");

    let enabled: Vec<_> = colors
        .iter()
        .filter(|color| color.enabled)
        .map(|color| color.value.as_str())
        .collect();
    // KF-151: KanbanFlow's standard palette is enabled by default.
    assert_eq!(
        enabled,
        vec![
            "yellow", "green", "blue", "red", "orange", "purple", "magenta", "cyan", "brown",
            "white"
        ]
    );

    // Fixed hex values per standard color (KanbanFlow parity).
    let yellow = defaults[0];
    assert_eq!(yellow.background_hex, "#ffffe0");
    assert_eq!(yellow.border_hex, "#f5cc00");
    assert_eq!(yellow.light_hex, "#ffffe0");

    // Dense ordering 1..=10.
    let mut orders: Vec<i64> = colors.iter().map(|color| color.sort_order).collect();
    orders.sort_unstable();
    assert_eq!(orders, (1..=10).collect::<Vec<_>>());

    // Backfill is idempotent: a second call adds nothing.
    db.ensure_board_colors(&board_id).expect("backfill again");
    assert_eq!(db.list_colors(&board_id).expect("list").len(), 10);
}

#[test]
fn color_crud_update_and_combined_default_change() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("Board").expect("create board");

    // All ten values exist after backfill, so free one up first.
    let purple = db
        .list_colors(&board_id)
        .expect("list")
        .into_iter()
        .find(|color| color.value == "purple")
        .expect("purple");
    assert!(matches!(
        db.delete_color(&purple.id).expect("delete purple"),
        ColorDeleteOutcome::Deleted
    ));

    // Create with a custom label/description.
    let id = db
        .create_color(&board_id, "purple", Some("Deep work"), Some("focus blocks"))
        .expect("create color");
    let created = db.get_color(&id).expect("get").expect("exists");
    assert_eq!(created.label, "Deep work");
    assert_eq!(created.description, "focus blocks");
    assert!(created.enabled);
    assert!(!created.is_default);
    assert_eq!(created.background_hex, "#eddbff"); // fixed per standard color

    // Duplicate value on the same board is refused.
    assert!(db.create_color(&board_id, "purple", None, None).is_err());

    // Combined update: rename, disable, and make default in one call.
    assert!(db
        .update_color(
            &id,
            Some("Focus"),
            Some("deep focus"),
            Some(false),
            Some(true)
        )
        .expect("update"));
    let updated = db.get_color(&id).expect("get").expect("exists");
    assert_eq!(updated.label, "Focus");
    assert_eq!(updated.description, "deep focus");
    assert!(!updated.enabled);
    assert!(updated.is_default);

    // Exactly one default across the board after the change.
    let defaults: Vec<_> = db
        .list_colors(&board_id)
        .expect("list")
        .into_iter()
        .filter(|color| color.is_default)
        .collect();
    assert_eq!(defaults.len(), 1);
    assert_eq!(defaults[0].id, id);

    // Unknown color returns false.
    assert!(!db
        .update_color("nope", Some("x"), None, None, None)
        .expect("update unknown"));
}

#[test]
fn color_label_limit_enforced_in_db() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("Board").expect("create board");
    let colors = db.list_colors(&board_id).expect("list");
    let some_id = colors[0].id.clone();

    let long = "x".repeat(51);
    let max = "y".repeat(50);

    // Free a slot so create_color can run.
    let purple = colors
        .into_iter()
        .find(|color| color.value == "purple")
        .expect("purple");
    assert!(matches!(
        db.delete_color(&purple.id).expect("delete"),
        ColorDeleteOutcome::Deleted
    ));

    assert!(db
        .create_color(&board_id, "purple", Some(&long), None)
        .is_err());
    let ok_id = db
        .create_color(&board_id, "purple", Some(&max), None)
        .expect("50 chars is allowed");

    assert!(db
        .update_color(&ok_id, Some(&long), None, None, None)
        .is_err());
    assert!(db
        .update_color(&ok_id, Some("   "), None, None, None)
        .is_err());
    assert!(db
        .update_color(&some_id, Some(&max), None, None, None)
        .expect("50 chars is allowed"));
}

#[test]
fn color_reorder_is_dense() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("Board").expect("create board");

    let colors = db.list_colors(&board_id).expect("list");
    let first_id = colors[0].id.clone();
    assert!(db.move_color(&first_id, 5).expect("move"));
    let colors = db.list_colors(&board_id).expect("list");
    assert_eq!(colors[5].id, first_id);
    let orders: Vec<i64> = colors.iter().map(|color| color.sort_order).collect();
    assert_eq!(orders, (0..10).collect::<Vec<_>>());

    // Out-of-range positions clamp.
    let last_id = colors[9].id.clone();
    assert!(db.move_color(&last_id, 999).expect("move"));
    let colors = db.list_colors(&board_id).expect("list");
    assert_eq!(colors[9].id, last_id);

    assert!(!db.move_color("nope", 0).expect("move unknown"));
}

#[test]
fn delete_color_refusals() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("Board").expect("create board");
    let column_id = db
        .create_column(&board_id, "To-do", None)
        .expect("create column");

    // The default color cannot be deleted.
    let default = db.default_color(&board_id).expect("default").expect("set");
    assert!(matches!(
        db.delete_color(&default.id).expect("delete default"),
        ColorDeleteOutcome::RefusedDefault
    ));

    // A color used by a task cannot be deleted.
    let red = db
        .list_colors(&board_id)
        .expect("list")
        .into_iter()
        .find(|color| color.value == "red")
        .expect("red");
    db.create_task(&column_id, None, "Task", 1, Some(&red.id))
        .expect("create task");
    assert_eq!(db.count_tasks_with_color(&red.id).expect("count"), 1);
    match db.delete_color(&red.id).expect("delete in-use") {
        ColorDeleteOutcome::RefusedInUse(count) => assert_eq!(count, 1),
        other => panic!("expected RefusedInUse, got {other:?}"),
    }

    // Unknown color.
    assert!(matches!(
        db.delete_color("nope").expect("delete unknown"),
        ColorDeleteOutcome::NotFound
    ));
}

#[test]
fn task_color_assignment_and_default_on_create() {
    let (_dir, db) = test_db();
    let board_id = db.create_board("Board").expect("create board");
    let column_id = db
        .create_column(&board_id, "To-do", None)
        .expect("create column");

    let default = db.default_color(&board_id).expect("default").expect("set");
    assert_eq!(default.value, "yellow");

    // Db::create_task is a plain insert: no color given means none stored.
    // (The HTTP handler resolves the board default before calling.)
    let task_id = db
        .create_task(&column_id, None, "Uncolored task", 1, None)
        .expect("create task");
    let task = db.get_task(&task_id).expect("get").expect("exists");
    assert_eq!(task.color_id, None);

    // The handler's default-on-create path: resolve the default, then insert.
    let task_id = db
        .create_task(&column_id, None, "Default task", 1, Some(&default.id))
        .expect("create task");
    let task = db.get_task(&task_id).expect("get").expect("exists");
    assert_eq!(task.color_id.as_deref(), Some(default.id.as_str()));

    // Explicit color assignment.
    let blue = db
        .list_colors(&board_id)
        .expect("list")
        .into_iter()
        .find(|color| color.value == "blue")
        .expect("blue");
    let task_id = db
        .create_task(&column_id, None, "Blue task", 1, Some(&blue.id))
        .expect("create task");
    let task = db.get_task(&task_id).expect("get").expect("exists");
    assert_eq!(task.color_id.as_deref(), Some(blue.id.as_str()));

    // update_task: reassign, then clear back to legacy size coloring.
    assert!(db
        .update_task(&task_id, None, None, None, Some(Some(&default.id)))
        .expect("reassign"));
    let task = db.get_task(&task_id).expect("get").expect("exists");
    assert_eq!(task.color_id.as_deref(), Some(default.id.as_str()));
    assert!(db
        .update_task(&task_id, None, None, None, Some(None))
        .expect("clear"));
    let task = db.get_task(&task_id).expect("get").expect("exists");
    assert_eq!(task.color_id, None);
}

#[test]
fn template_save_apply_round_trip() {
    let (_dir, db) = test_db();

    // Built-in templates exist exactly once each (Pomodoro board + Kanban basics).
    let templates = db.list_templates().expect("list templates");
    let builtins: Vec<_> = templates.iter().filter(|t| t.built_in).collect();
    assert_eq!(builtins.len(), 2);

    // Apply the Pomodoro board template: exact Pomodoro shape — 10 colors,
    // 4 columns, 2 swimlanes.
    let template_id = builtin_template_id(&db);
    let board_id = db
        .apply_template(&template_id, "From template")
        .expect("apply")
        .expect("known template");
    let colors = db.list_colors(&board_id).expect("colors");
    assert_eq!(colors.len(), 10);
    assert_eq!(
        colors.iter().filter(|color| color.is_default).count(),
        1,
        "exactly one default"
    );
    assert_eq!(
        colors
            .iter()
            .find(|color| color.is_default)
            .expect("default")
            .value,
        "yellow"
    );
    assert_eq!(
        db.list_columns(&board_id).expect("columns").len(),
        4,
        "four columns"
    );
    assert_eq!(
        db.list_swimlanes(&board_id).expect("swimlanes").len(),
        2,
        "two swimlanes"
    );

    // Tweak the board, save it as a template, apply the copy elsewhere.
    let blue = colors
        .into_iter()
        .find(|color| color.value == "blue")
        .expect("blue");
    db.update_color(&blue.id, Some("Custom blue"), None, None, None)
        .expect("rename");
    let copy_id = db
        .save_board_as_template(&board_id, "My copy", "a copy")
        .expect("save")
        .expect("known board");
    let copy = db.get_template(&copy_id).expect("get").expect("exists");
    assert!(!copy.built_in);

    let board2 = db
        .apply_template(&copy_id, "From copy")
        .expect("apply copy")
        .expect("known template");
    let colors2 = db.list_colors(&board2).expect("colors");
    assert_eq!(colors2.len(), 10);
    assert!(colors2
        .iter()
        .any(|color| color.value == "blue" && color.label == "Custom blue"));

    // Unknown template/board ids.
    assert!(db
        .apply_template("nope", "x")
        .expect("apply unknown")
        .is_none());
    assert!(db
        .save_board_as_template("nope", "x", "y")
        .expect("save unknown")
        .is_none());
}

#[test]
fn builtin_template_cannot_be_deleted() {
    let (_dir, db) = test_db();
    let template_id = builtin_template_id(&db);
    assert!(matches!(
        db.delete_template(&template_id).expect("delete built-in"),
        TemplateDeleteOutcome::RefusedBuiltIn
    ));
    // Still there afterwards.
    assert!(db.get_template(&template_id).expect("get").is_some());

    assert!(matches!(
        db.delete_template("nope").expect("delete unknown"),
        TemplateDeleteOutcome::NotFound
    ));
}

#[test]
fn persistence_across_reconnect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let path_str = path.to_str().expect("utf8").to_string();

    let board_id;
    let color_id;
    let template_id;
    {
        let db = Db::connect(&path_str).expect("connect");
        board_id = db.create_board("Persistent").expect("board");
        let column_id = db.create_column(&board_id, "To-do", None).expect("column");
        let colors = db.list_colors(&board_id).expect("colors");
        color_id = colors[0].id.clone();
        db.create_task(&column_id, None, "Task", 2, Some(&color_id))
            .expect("task");
        template_id = db
            .save_board_as_template(&board_id, "Saved", "desc")
            .expect("save")
            .expect("saved");
        db.update_color(&color_id, Some("Renamed"), None, None, None)
            .expect("rename");
    } // Db drops here.

    let db = Db::connect(&path_str).expect("reconnect");
    assert!(db.get_board(&board_id).expect("board").is_some());
    let color = db.get_color(&color_id).expect("color").expect("exists");
    assert_eq!(color.label, "Renamed");
    assert_eq!(db.list_colors(&board_id).expect("colors").len(), 10);
    assert!(db.get_template(&template_id).expect("template").is_some());
    // Built-in template seeding is idempotent across reconnects:
    // exactly the two built-ins, no duplicates.
    let builtins = db
        .list_templates()
        .expect("templates")
        .into_iter()
        .filter(|template| template.built_in)
        .count();
    assert_eq!(builtins, 2);
}

#[test]
fn starter_board_comes_from_builtin_template() {
    // A brand-new database seeds the starter "General" board from the
    // built-in Pomodoro template.
    let (_dir, db) = test_db();
    let boards = db.list_boards().expect("boards");
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0].name, "General");
    assert_eq!(db.list_colors(&boards[0].id).expect("colors").len(), 10);
    assert_eq!(db.list_columns(&boards[0].id).expect("columns").len(), 4);
}

#[test]
fn standard_color_hex_values_are_fixed() {
    // Spot-check the KanbanFlow palette; the DB stores these verbatim.
    let cases = [
        ("red", "#ffccd0", "#ff858f", "#ffe0e3"),
        ("yellow", "#ffffe0", "#f5cc00", "#ffffe0"),
        ("green", "#dbffc2", "#59d600", "#e4ffd1"),
        ("blue", "#cce3ff", "#70b0ff", "#d6e9ff"),
    ];
    for (value, bg, border, light) in cases {
        let (got_bg, got_border, got_light, _) =
            chipflow::models::standard_color(value).expect("known color");
        assert_eq!((got_bg, got_border, got_light), (bg, border, light));
    }
    assert!(chipflow::models::standard_color("chartreuse").is_none());
}

#[test]
fn kanban_basics_builtin_template_shape() {
    let (_dir, db) = test_db();

    // KF-155: the "Kanban basics" built-in template exists and is protected.
    let template_id = db
        .list_templates()
        .expect("list templates")
        .into_iter()
        .find(|template| template.built_in && template.name == Db::KANBAN_BASICS_TEMPLATE_NAME)
        .expect("built-in Kanban basics template seeded")
        .id;
    assert!(matches!(
        db.delete_template(&template_id).expect("delete built-in"),
        TemplateDeleteOutcome::RefusedBuiltIn
    ));

    // Apply it: 10 standard colors, 3 columns, 1 swimlane.
    let board_id = db
        .apply_template(&template_id, "Kanban basics board")
        .expect("apply")
        .expect("known template");
    let colors = db.list_colors(&board_id).expect("colors");
    assert_eq!(colors.len(), 10);
    assert_eq!(
        colors.iter().filter(|color| color.is_default).count(),
        1,
        "exactly one default"
    );
    assert!(
        colors.iter().all(|color| color.enabled),
        "all standard colors enabled"
    );
    assert!(colors
        .iter()
        .any(|color| { color.value == "yellow" && color.label == "Yellow" && color.is_default }));
    let columns = db.list_columns(&board_id).expect("columns");
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["To-do", "In progress", "Done"]);
    assert_eq!(
        columns.iter().filter(|c| c.is_done).count(),
        1,
        "exactly one Done column"
    );
    let lanes = db.list_swimlanes(&board_id).expect("swimlanes");
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].name, "Default");
}
