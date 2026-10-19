use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use chipflow::db::Db;

const WRITERS: usize = 4;
const WRITES_PER_WRITER: usize = 60;
const MOVERS: usize = 2;

fn test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    (dir, db)
}

#[test]
fn move_task_should_keep_every_concurrent_neighbour_write() {
    let (_dir, db) = test_db();
    let board = db.create_board("Board").expect("board");
    let col_a = db.create_column(&board, "A", None).expect("column a");
    let col_b = db.create_column(&board, "B", None).expect("column b");
    let target = db
        .create_task(&col_a, None, "target", 1, None)
        .expect("target");
    db.create_task(&col_a, None, "other", 1, None)
        .expect("other");
    let movers: Vec<String> = (0..MOVERS)
        .map(|i| {
            db.create_task(&col_b, None, &format!("mover {i}"), 1, None)
                .expect("mover")
        })
        .collect();

    let done = Arc::new(AtomicBool::new(false));
    let mover_threads: Vec<_> = movers
        .into_iter()
        .map(|mover| {
            let db = db.clone();
            let (col_a, col_b) = (col_a.clone(), col_b.clone());
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut moves = 0usize;
                while !done.load(Ordering::Relaxed) {
                    let column = if moves.is_multiple_of(2) {
                        &col_a
                    } else {
                        &col_b
                    };
                    db.move_task(&mover, column, None, 0.0).expect("move");
                    moves += 1;
                }
                moves
            })
        })
        .collect();

    let writer_threads: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let db = db.clone();
            let target = target.clone();
            thread::spawn(move || {
                let mut committed = Vec::new();
                for write in 0..WRITES_PER_WRITER {
                    let name = format!("w{writer}-{write}");
                    if db.add_subtask(&target, &name).expect("add").is_some() {
                        committed.push(name);
                    }
                }
                committed
            })
        })
        .collect();

    let committed: Vec<String> = writer_threads
        .into_iter()
        .flat_map(|handle| handle.join().expect("writer"))
        .collect();
    done.store(true, Ordering::Relaxed);
    let moves: usize = mover_threads
        .into_iter()
        .map(|handle| handle.join().expect("mover"))
        .sum();

    let stored: Vec<String> = db
        .task(&target)
        .expect("get")
        .expect("target exists")
        .subtasks
        .into_iter()
        .map(|sub| sub.name)
        .collect();
    let lost: Vec<&String> = committed
        .iter()
        .filter(|name| !stored.contains(name))
        .collect();
    assert_eq!(committed.len(), WRITERS * WRITES_PER_WRITER);
    assert!(moves > 0, "the movers ran");
    assert!(
        lost.is_empty(),
        "{} of {} committed writes lost across {moves} moves: {lost:?}",
        lost.len(),
        committed.len()
    );
}
