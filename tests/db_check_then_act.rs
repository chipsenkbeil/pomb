use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use chipflow::db::{ColorDeleteOutcome, Db};

const ROUNDS: usize = 100;

fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

/// Run `f(i)` on `n` threads released together.
fn race<T: Send + 'static>(n: usize, f: impl Fn(usize) -> T + Send + Sync + 'static) -> Vec<T> {
    let barrier = Arc::new(Barrier::new(n));
    let f = Arc::new(f);
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            let f = Arc::clone(&f);
            thread::spawn(move || {
                barrier.wait();
                f(i)
            })
        })
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect()
}

#[test]
fn create_task_should_give_concurrent_tasks_distinct_positions() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("board");
    for round in 0..ROUNDS {
        let column = db
            .create_column(&board, &format!("C{round}"), None)
            .expect("column");
        {
            let db = db.clone();
            let column = column.clone();
            race(4, move |i| {
                db.create_task(&column, None, &format!("t{i}"), 1, None)
                    .expect("create")
            });
        }
        let mut positions: Vec<i64> = db
            .board_tasks(&board)
            .expect("tasks")
            .iter()
            .filter(|task| task.column_id == column)
            .map(|task| task.position as i64)
            .collect();
        positions.sort_unstable();
        assert_eq!(positions, [0, 1, 2, 3], "round {round}");
    }
}

#[test]
fn update_color_should_leave_one_default_under_concurrent_changes() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("board");
    let colors = db.list_colors(&board).expect("colors");
    let keep = colors[0].id.clone();
    let other = colors[1].id.clone();
    let spare = colors.last().expect("colors").clone();
    db.delete_color(&spare.id).expect("free a value");
    for round in 0..ROUNDS {
        {
            let db = db.clone();
            let (board, keep, other, value) = (
                board.clone(),
                keep.clone(),
                other.clone(),
                spare.value.clone(),
            );

            // Several writers set a default while one adds a color and
            // defaults it, so some writer reads the board's colors before
            // the new one commits and writes after it defaulted.
            race(5, move |i| match i {
                0 | 2 | 3 => {
                    let id = if i == 2 { &other } else { &keep };
                    db.update_color(id, None, None, None, Some(true))
                        .expect("default existing");
                }
                _ => {
                    if i == 4 {
                        thread::sleep(Duration::from_micros(200));
                    }
                    let id = db.create_color(&board, &value, None, None);
                    if let Ok(id) = id {
                        db.update_color(&id, None, None, None, Some(true))
                            .expect("default new");
                    }
                }
            });
        }
        let colors = db.list_colors(&board).expect("colors");
        let defaults = colors.iter().filter(|color| color.is_default).count();
        assert_eq!(defaults, 1, "round {round}");
        db.update_color(&keep, None, None, None, Some(true))
            .expect("reset default");
        let added = colors
            .iter()
            .find(|color| color.value == spare.value)
            .expect("added color");
        assert_eq!(
            db.delete_color(&added.id).expect("delete"),
            ColorDeleteOutcome::Deleted
        );
    }
}

#[test]
fn delete_color_should_never_leave_a_task_on_a_deleted_color() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("board");
    let column = db.create_column(&board, "A", None).expect("column");
    for round in 0..ROUNDS {
        let color = db
            .list_colors(&board)
            .expect("colors")
            .into_iter()
            .find(|color| !color.is_default)
            .expect("non-default color");
        {
            let db = db.clone();
            let (column, color_id) = (column.clone(), color.id.clone());
            race(2, move |i| match i {
                0 => {
                    db.delete_color(&color_id).expect("delete");
                }
                _ => {
                    let _ = db.create_task(&column, None, &format!("r{round}"), 1, Some(&color_id));
                }
            });
        }
        let dangling = db
            .board_tasks(&board)
            .expect("tasks")
            .iter()
            .any(|task| task.color_id.as_deref() == Some(color.id.as_str()))
            && db.get_color(&color.id).expect("get").is_none();
        assert!(!dangling, "round {round}: a task kept a deleted color");
        if db.get_color(&color.id).expect("get").is_some() {
            for task in db.board_tasks(&board).expect("tasks") {
                db.delete_task(&task.id).expect("delete task");
            }
            assert_eq!(
                db.delete_color(&color.id).expect("delete"),
                ColorDeleteOutcome::Deleted
            );
        }
        db.create_color(&board, &color.value, None, None)
            .expect("recreate color");
    }
}

#[test]
fn create_entry_should_never_log_onto_a_deleted_task() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("board");
    let column = db.create_column(&board, "A", None).expect("column");
    for round in 0..ROUNDS {
        let task = db
            .create_task(&column, None, &format!("r{round}"), 1, None)
            .expect("task");
        {
            let db = db.clone();
            let task = task.clone();
            race(2, move |i| match i {
                0 => {
                    db.delete_task(&task).expect("delete");
                }
                _ => {
                    db.create_entry_at(&task, 5, "", "2026-10-17T09:00:00+00:00", &[], "")
                        .expect("entry");
                }
            });
        }
        let orphans = db
            .all_entries()
            .expect("entries")
            .into_iter()
            .filter(|entry| entry.task_id == task)
            .count();
        assert_eq!(orphans, 0, "round {round}: an entry outlived its task");
    }
}

#[test]
fn ensure_default_swimlane_should_add_one_lane_under_concurrent_calls() {
    let (_dir, db) = test_db();
    for round in 0..ROUNDS {
        let board = db.create_board(&format!("Board {round}")).expect("board");
        {
            let db = db.clone();
            let board = board.clone();
            race(6, move |_| {
                db.ensure_default_swimlane(&board).expect("ensure")
            });
        }
        assert_eq!(
            db.list_swimlanes(&board).expect("lanes").len(),
            1,
            "round {round}"
        );
    }
}

#[test]
fn copy_colors_should_copy_a_consistent_source() {
    let (_dir, db) = test_db();
    let source = db.create_board("Source").expect("source");
    let palette = db.create_board("Palette").expect("palette");
    let full = db.list_colors(&source).expect("colors").len();
    let done = Arc::new(AtomicBool::new(false));

    // Six threads keep replacing the source palette while twelve copy it into
    // their own boards, so a copy that reads the source outside its write
    // transaction meets a replacement part-way through.
    let replacers: Vec<_> = (0..2)
        .map(|_| {
            let db = db.clone();
            let (source, palette) = (source.clone(), palette.clone());
            let done = Arc::clone(&done);
            thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    db.copy_colors(&source, &palette)
                        .expect("replace source palette");
                }
            })
        })
        .collect();
    let targets: Vec<String> = (0..12)
        .map(|i| db.create_board(&format!("Target {i}")).expect("target"))
        .collect();
    let broken = {
        let db = db.clone();
        let source = source.clone();
        race(12, move |i| {
            let mut broken = Vec::new();
            for round in 0..ROUNDS {
                // Spread the reads across the replacers' commit cycle.
                thread::sleep(Duration::from_micros(
                    ((round * 977 + i * 331) % 20_000) as u64,
                ));
                db.copy_colors(&targets[i], &source).expect("copy");
                let colors = db.list_colors(&targets[i]).expect("colors");
                let defaults = colors.iter().filter(|color| color.is_default).count();
                if colors.len() != full || defaults != 1 {
                    broken.push(round);
                }
            }
            broken
        })
    };
    done.store(true, Ordering::Relaxed);
    for replacer in replacers {
        replacer.join().expect("replacer");
    }
    assert!(
        broken.iter().all(Vec::is_empty),
        "partial or default-less copies by round: {broken:?}"
    );
}

#[test]
fn apply_template_should_never_show_a_half_built_board() {
    let (_dir, db) = test_db();
    let template = db
        .list_templates()
        .expect("templates")
        .into_iter()
        .find(|template| template.built_in)
        .expect("built-in template");
    let columns = template.snapshot["columns"]
        .as_array()
        .expect("columns")
        .len();
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let db = db.clone();
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let mut half_built = 0usize;
            while !done.load(Ordering::Relaxed) {
                for board in db.list_boards().expect("boards") {
                    if board.name == "From template"
                        && db.list_columns(&board.id).expect("columns").len() != columns
                    {
                        half_built += 1;
                    }
                }
            }
            half_built
        })
    };
    for _ in 0..ROUNDS {
        db.apply_template(&template.id, "From template")
            .expect("apply")
            .expect("known template");
    }
    done.store(true, Ordering::Relaxed);
    assert_eq!(reader.join().expect("reader"), 0);
}
