use std::collections::HashSet;

use chrono::Utc;
use redb::{ReadableTable, WriteTransaction};
use tokio::sync::watch;

use super::{
    BOARD_EVENTS, COLUMNS, Db, DbResult, EVENT_WAKEUPS, MAX_HISTORY_TEXT_CHARS, cut_text, read_in,
};
use crate::models::{Actor, BoardEventRow, ColumnRow, EventKind, TaskComment, TaskRow};

/// Most events a board keeps. Past it each new event drops the board's oldest.
pub const MAX_EVENTS_PER_BOARD: usize = 1000;

/// Most labels one event names as added, and as removed.
const MAX_EVENT_LABELS: usize = 20;

/// Digits of a sequence number in a key, which sorts a board's events in order.
const SEQ_DIGITS: usize = 20;

/// What a read of a board's events found.
#[derive(Clone, Debug, PartialEq)]
pub enum EventRead {
    /// The events after the cursor, and the cursor to resume from.
    Page(EventPage),
    /// The cursor is older than the events kept, or newer than any written. `oldest` is the
    /// oldest cursor that still resumes with no event missing.
    Expired { oldest: u64 },
}

/// Events of one board, oldest first.
#[derive(Clone, Debug, PartialEq)]
pub struct EventPage {
    /// Each event that matched, with its sequence number.
    pub events: Vec<(u64, BoardEventRow)>,
    /// The sequence number of the last event read, matching or not, or the cursor asked for when
    /// none was after it.
    pub cursor: u64,
}

/// Returns the key of event `seq` of `board`.
fn event_key(board: &str, seq: u64) -> String {
    format!("{board}|{seq:0width$}", width = SEQ_DIGITS)
}

/// Returns the sequence number a key holds.
fn seq_of(key: &str) -> Option<u64> {
    key.rsplit_once('|')?.1.parse().ok()
}

/// Returns the first and last key a board's events can have.
fn board_keys(board: &str) -> (String, String) {
    (event_key(board, 0), event_key(board, u64::MAX))
}

/// Returns the sequence number of the newest event of `board` in `table`, or 0 when it has none.
fn newest_seq(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    board: &str,
) -> DbResult<u64> {
    let (first, last) = board_keys(board);
    Ok(table
        .range(first.as_str()..=last.as_str())?
        .next_back()
        .transpose()?
        .and_then(|(key, _)| seq_of(key.value()))
        .unwrap_or(0))
}

/// Returns the boards that have events written in the open write transaction, and forgets them, so
/// that no committed file holds one.
pub(super) fn txn_take_wakeups(txn: &WriteTransaction) -> DbResult<Vec<String>> {
    let mut table = txn.open_table(EVENT_WAKEUPS)?;
    let boards = table
        .iter()?
        .map(|entry| entry.map(|(board, _)| board.value().to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    for board in &boards {
        table.remove(board.as_str())?;
    }
    Ok(boards)
}

/// Writes the events the change of task `before` into `after` makes, inside the open write
/// transaction that stores `after`. A task with no `before` is new.
pub(super) fn txn_record_events(
    txn: &WriteTransaction,
    before: Option<&TaskRow>,
    after: &TaskRow,
    actor: Actor<'_>,
) -> DbResult<()> {
    let events = events_of(before, after, actor);
    if events.is_empty() {
        return Ok(());
    }
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &after.column_id)? else {
        return Ok(());
    };
    txn_append_events(txn, &column.board_id, &events, MAX_EVENTS_PER_BOARD)?;
    for event in events
        .iter()
        .filter(|event| event.kind == EventKind::TaskMoved)
    {
        let Some(from) = event.from_column_id.as_deref() else {
            continue;
        };
        let Some(source) = read_in::<ColumnRow>(txn, COLUMNS, from)? else {
            continue;
        };
        if source.board_id != column.board_id {
            let left = BoardEventRow {
                column_id: None,
                ..event.clone()
            };
            txn_append_events(txn, &source.board_id, &[left], MAX_EVENTS_PER_BOARD)?;
        }
    }
    Ok(())
}

/// Appends `events` to `board`, inside an open write transaction, and drops its oldest events
/// past `keep`.
pub(super) fn txn_append_events(
    txn: &WriteTransaction,
    board: &str,
    events: &[BoardEventRow],
    keep: usize,
) -> DbResult<()> {
    {
        let mut table = txn.open_table(BOARD_EVENTS)?;
        let mut seq = newest_seq(&table, board)?;
        for event in events {
            seq += 1;
            let raw = serde_json::to_vec(event)?;
            table.insert(event_key(board, seq).as_str(), raw.as_slice())?;
            if let Some(dropped) = seq.checked_sub(keep as u64).filter(|dropped| *dropped > 0) {
                table.remove(event_key(board, dropped).as_str())?;
            }
        }
    }
    let mut wakeups = txn.open_table(EVENT_WAKEUPS)?;
    let held = wakeups.get(board)?.map_or(0, |count| count.value());
    wakeups.insert(board, held + events.len() as u64)?;
    Ok(())
}

/// Removes every event of `board`, inside an open write transaction.
pub(super) fn txn_remove_events(txn: &WriteTransaction, board: &str) -> DbResult<()> {
    let mut table = txn.open_table(BOARD_EVENTS)?;
    let (first, last) = board_keys(board);
    let keys = table
        .range(first.as_str()..=last.as_str())?
        .map(|entry| entry.map(|(key, _)| key.value().to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    for key in keys {
        table.remove(key.as_str())?;
    }
    Ok(())
}

/// Returns the events the change of task `before` into `after` makes. A task with no `before` is
/// new.
fn events_of(before: Option<&TaskRow>, after: &TaskRow, actor: Actor<'_>) -> Vec<BoardEventRow> {
    let actor_kind = actor.kind;
    let token_name = actor.token_name.map(str::to_string);
    let at = Utc::now().to_rfc3339();
    let event = |kind| BoardEventRow::new(kind, after, actor_kind, token_name.clone(), &at);
    let Some(before) = before else {
        return vec![event(EventKind::TaskAdded)];
    };
    let mut events = Vec::new();
    if before.column_id != after.column_id {
        events.push(BoardEventRow {
            from_column_id: Some(before.column_id.clone()),
            ..event(EventKind::TaskMoved)
        });
    }
    let added = label_changes(&after.labels, &before.labels);
    let removed = label_changes(&before.labels, &after.labels);
    let renamed = before.name != after.name;
    let resized = before.size != after.size;
    let described = before.description != after.description;
    if renamed || resized || described || !added.is_empty() || !removed.is_empty() {
        events.push(BoardEventRow {
            name: renamed.then(|| cut_text(&after.name, MAX_HISTORY_TEXT_CHARS)),
            size: resized.then_some(after.size),
            labels_added: added,
            labels_removed: removed,
            ..event(EventKind::TaskUpdated)
        });
    }
    let known: HashSet<&str> = before.comments.iter().map(|c| c.id.as_str()).collect();
    for comment in after
        .comments
        .iter()
        .filter(|c| !known.contains(c.id.as_str()))
    {
        events.push(comment_event(after, comment, &at));
    }
    events
}

/// Returns the event for `comment`, added to `task`, which names the comment's own author.
fn comment_event(task: &TaskRow, comment: &TaskComment, at: &str) -> BoardEventRow {
    BoardEventRow {
        comment_id: Some(comment.id.clone()),
        ..BoardEventRow::new(
            EventKind::CommentAdded,
            task,
            comment.actor_kind,
            comment.token_name.clone(),
            at,
        )
    }
}

/// Returns the labels in `now` that `was` lacks, cut to what an event keeps.
fn label_changes(now: &[String], was: &[String]) -> Vec<String> {
    now.iter()
        .filter(|label| !was.contains(label))
        .take(MAX_EVENT_LABELS)
        .map(|label| cut_text(label, MAX_HISTORY_TEXT_CHARS))
        .collect()
}

impl Db {
    /// Returns the events of `board` after sequence number `after` whose kind is in `kinds`, at
    /// most `limit` of them, or [`EventRead::Expired`] when `after` is not a cursor the board's
    /// events can resume from.
    pub fn board_events(
        &self,
        board: &str,
        after: u64,
        kinds: &[EventKind],
        limit: usize,
    ) -> DbResult<EventRead> {
        let txn = self.read()?;
        let table = txn.open_table(BOARD_EVENTS)?;
        let (first, last) = board_keys(board);
        let oldest = table
            .range(first.as_str()..=last.as_str())?
            .next()
            .transpose()?
            .and_then(|(key, _)| seq_of(key.value()));
        let newest = newest_seq(&table, board)?;
        let resumable = oldest.map_or(newest, |oldest| oldest - 1);
        if after < resumable || after > newest {
            return Ok(EventRead::Expired { oldest: resumable });
        }
        let from = event_key(board, after + 1);
        let mut events = Vec::new();
        let mut cursor = after;
        for entry in table.range(from.as_str()..=last.as_str())? {
            let (key, value) = entry?;
            let Some(seq) = seq_of(key.value()) else {
                continue;
            };
            cursor = seq;
            let row: BoardEventRow = serde_json::from_slice(value.value())?;
            if kinds.contains(&row.kind) {
                events.push((seq, row));
                if events.len() >= limit {
                    break;
                }
            }
        }
        Ok(EventRead::Page(EventPage { events, cursor }))
    }

    /// Returns the sequence number of the newest event of `board`, or 0 when it has none.
    pub fn board_event_cursor(&self, board: &str) -> DbResult<u64> {
        let txn = self.read()?;
        let table = txn.open_table(BOARD_EVENTS)?;
        newest_seq(&table, board)
    }

    /// Returns a receiver that sees every commit that wrote events of `board` from now on, to
    /// wait on.
    #[must_use]
    pub fn board_feed(&self, board: &str) -> watch::Receiver<()> {
        self.db.board_feed(board)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use redb::ReadableTableMetadata;

    use super::*;
    use crate::db::{
        BOARD_EVENTS, EVENT_WAKEUPS, META, MoveGuards, NewComment, NewTaskPlacement,
        SCHEMA_VERSION, SCHEMA_VERSION_KEY, push_event, read_schema_version,
    };
    use crate::models::{API_TOKEN_ACTOR, AccountId, ActorKind};

    /// A board with a Todo and a Done column.
    struct Fixture {
        _dir: tempfile::TempDir,
        db: Db,
        board: String,
        todo: String,
        done: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("test.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let board = db
            .create_board(&AccountId::ownerless(), "Events")
            .expect("board");
        let todo = db.create_column(&board, "Todo", None).expect("column");
        let done = db.create_column(&board, "Done", None).expect("column");
        Fixture {
            _dir: dir,
            db,
            board,
            todo,
            done,
        }
    }

    impl Fixture {
        fn task(&self, name: &str) -> String {
            self.task_by(name, USER)
        }

        fn task_by(&self, name: &str, actor: Actor<'_>) -> String {
            let row = Db::new_task_row(&self.todo, None, name, 0, None);
            self.db
                .insert_task_checked(row, NewTaskPlacement::Bottom, actor)
                .expect("task")
                .expect("fields")
        }

        fn events(&self) -> Vec<(u64, BoardEventRow)> {
            self.events_after(0, &EventKind::ALL)
        }

        fn events_after(&self, after: u64, kinds: &[EventKind]) -> Vec<(u64, BoardEventRow)> {
            match self
                .db
                .board_events(&self.board, after, kinds, usize::MAX)
                .expect("read")
            {
                EventRead::Page(page) => page.events,
                EventRead::Expired { oldest } => panic!("expired, oldest {oldest}"),
            }
        }
    }

    const TOKEN: Actor<'static> = Actor {
        name: API_TOKEN_ACTOR,
        token_name: Some("ci"),
        kind: ActorKind::ApiToken,
    };

    const USER: Actor<'static> = Actor {
        name: "owner",
        token_name: None,
        kind: ActorKind::User,
    };

    #[test]
    fn create_should_write_task_added_with_the_user_actor() {
        let fx = fixture();
        let task = fx.task("Write");
        let events = fx.events();
        assert_eq!(events.len(), 1);
        let (seq, event) = &events[0];
        assert_eq!(*seq, 1);
        assert_eq!(event.kind, EventKind::TaskAdded);
        assert_eq!(event.task_id, task);
        assert_eq!(event.column_id.as_deref(), Some(fx.todo.as_str()));
        assert_eq!(event.from_column_id, None);
        assert_eq!(
            (event.actor_kind, event.token_name.as_deref()),
            (ActorKind::User, None)
        );
    }

    #[test]
    fn move_should_write_task_moved_with_the_column_it_left() {
        let fx = fixture();
        let task = fx.task("Move");
        let guards = MoveGuards {
            event: Some("Moved to Done"),
            ..MoveGuards::default()
        };
        fx.db
            .move_task_logged(&task, &fx.done, None, 0.0, guards, TOKEN)
            .expect("move");
        let events = fx.events_after(1, &EventKind::ALL);
        assert_eq!(events.len(), 1);
        let event = &events[0].1;
        assert_eq!(event.kind, EventKind::TaskMoved);
        assert_eq!(event.from_column_id.as_deref(), Some(fx.todo.as_str()));
        assert_eq!(event.column_id.as_deref(), Some(fx.done.as_str()));
        assert_eq!(event.actor_kind, ActorKind::ApiToken);
        assert_eq!(event.token_name.as_deref(), Some("ci"));
    }

    #[test]
    fn move_to_another_board_should_write_the_event_to_both_boards() {
        let fx = fixture();
        let task = fx.task("Travel");
        let other = fx
            .db
            .create_board(&AccountId::ownerless(), "Other")
            .expect("board");
        let target = fx.db.create_column(&other, "Todo", None).expect("column");
        fx.db
            .move_task_logged(&task, &target, None, 0.0, MoveGuards::default(), USER)
            .expect("move");
        let source = fx.events_after(1, &EventKind::ALL);
        assert_eq!(source.len(), 1);
        let EventRead::Page(page) = fx
            .db
            .board_events(&other, 0, &EventKind::ALL, 10)
            .expect("read")
        else {
            panic!("expired");
        };
        assert_eq!(page.events.len(), 1);
        let (left, entered) = (&source[0].1, &page.events[0].1);
        assert_eq!(left.kind, EventKind::TaskMoved);
        assert_eq!(left.column_id, None);
        assert_eq!(left.from_column_id.as_deref(), Some(fx.todo.as_str()));
        assert_eq!(entered.kind, EventKind::TaskMoved);
        assert_eq!(entered.column_id.as_deref(), Some(target.as_str()));
        assert_eq!(entered.from_column_id.as_deref(), Some(fx.todo.as_str()));
    }

    #[test]
    fn reordering_within_a_column_should_write_no_event() {
        let fx = fixture();
        let first = fx.task("First");
        fx.task("Second");
        fx.db
            .move_task(&first, &fx.todo, None, 1.0)
            .expect("reorder");
        assert_eq!(fx.events().len(), 2, "only the two creations");
    }

    #[test]
    fn update_should_write_task_updated_with_what_changed() {
        let fx = fixture();
        let task = fx.task("Old");
        fx.db
            .set_task_labels(&task, &["a".to_string(), "b".to_string()])
            .expect("labels");
        fx.db
            .apply_task_change(&task, TOKEN, |row| {
                row.name = "New".to_string();
                row.size = 3;
                row.labels = vec!["b".to_string(), "c".to_string()];
                push_event(row, "renamed", "Renamed".to_string(), TOKEN);
                ((), true)
            })
            .expect("change");
        let events = fx.events_after(1, &EventKind::ALL);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1.labels_added, ["a", "b"]);
        assert_eq!(events[0].1.name, None);
        let event = &events[1].1;
        assert_eq!(event.kind, EventKind::TaskUpdated);
        assert_eq!(event.name.as_deref(), Some("New"));
        assert_eq!(event.size, Some(3));
        assert_eq!(event.labels_added, ["c"]);
        assert_eq!(event.labels_removed, ["a"]);
        assert_eq!(event.token_name.as_deref(), Some("ci"));
    }

    #[test]
    fn a_change_to_anything_else_should_write_no_event() {
        let fx = fixture();
        let task = fx.task("Same");
        fx.db
            .apply_task_change(&task, USER, |row| {
                row.due_repeat = Some("weekly".to_string());
                ((), true)
            })
            .expect("change");
        assert_eq!(fx.events().len(), 1);
    }

    #[test]
    fn a_description_only_edit_should_write_a_bare_task_updated() {
        let fx = fixture();
        let task = fx.task("Text");
        fx.db
            .apply_task_change(&task, TOKEN, |row| {
                row.description = "- [x] one".to_string();
                ((), true)
            })
            .expect("change");
        let events = fx.events_after(1, &EventKind::ALL);
        assert_eq!(events.len(), 1);
        let event = &events[0].1;
        assert_eq!(event.kind, EventKind::TaskUpdated);
        assert_eq!(event.name, None);
        assert_eq!(event.size, None);
        assert!(event.labels_added.is_empty() && event.labels_removed.is_empty());
        assert_eq!(event.token_name.as_deref(), Some("ci"));
    }

    #[test]
    fn a_label_only_edit_by_a_token_should_report_the_token() {
        let fx = fixture();
        let task = fx.task("Label");
        fx.db
            .apply_task_change(&task, TOKEN, |row| {
                row.labels = vec!["x".to_string()];
                ((), true)
            })
            .expect("labels");
        let event = &fx.events_after(1, &EventKind::ALL)[0].1;
        assert_eq!(event.kind, EventKind::TaskUpdated);
        assert_eq!(event.actor_kind, ActorKind::ApiToken);
        assert_eq!(event.token_name.as_deref(), Some("ci"));
    }

    #[test]
    fn a_size_edit_by_a_token_should_report_the_token() {
        let fx = fixture();
        let task = fx.task("Size");
        fx.db
            .apply_task_change(&task, TOKEN, |row| {
                row.size = 5;
                ((), true)
            })
            .expect("size");
        let event = &fx.events_after(1, &EventKind::ALL)[0].1;
        assert_eq!(event.size, Some(5));
        assert_eq!(event.actor_kind, ActorKind::ApiToken);
        assert_eq!(event.token_name.as_deref(), Some("ci"));
    }

    #[test]
    fn a_task_a_token_creates_should_report_the_token() {
        let fx = fixture();
        fx.task_by("Made", TOKEN);
        let event = &fx.events()[0].1;
        assert_eq!(event.actor_kind, ActorKind::ApiToken);
        assert_eq!(event.token_name.as_deref(), Some("ci"));
    }

    #[test]
    fn a_change_the_server_makes_should_report_system() {
        let fx = fixture();
        fx.task("Seed");
        let template = fx
            .db
            .save_board_as_template(&AccountId::ownerless(), &fx.board, "Starter", "")
            .expect("save")
            .expect("template");
        let board = fx
            .db
            .apply_template(&AccountId::ownerless(), &template, "Copy")
            .expect("apply")
            .expect("board");
        let EventRead::Page(page) = fx
            .db
            .board_events(&board, 0, &EventKind::ALL, 10)
            .expect("read")
        else {
            panic!("expired");
        };
        assert_eq!(page.events.len(), 1);
        let event = &page.events[0].1;
        assert_eq!(event.kind, EventKind::TaskAdded);
        assert_eq!(event.actor_kind, ActorKind::System);
        assert_eq!(event.token_name, None);
    }

    #[test]
    fn comment_should_write_comment_added_with_the_comment_author() {
        let fx = fixture();
        let task = fx.task("Talk");
        let new = NewComment {
            author: "agent",
            body: "hello",
            actor_kind: ActorKind::ApiToken,
            token_name: Some("ci"),
            token_id: None,
            kind: None,
        };
        fx.db.add_comment(&task, new, TOKEN, None).expect("comment");
        let comment = fx.db.task(&task).expect("read").expect("task").comments[0]
            .id
            .clone();
        let events = fx.events_after(1, &EventKind::ALL);
        assert_eq!(events.len(), 1);
        let event = &events[0].1;
        assert_eq!(event.kind, EventKind::CommentAdded);
        assert_eq!(event.comment_id.as_deref(), Some(comment.as_str()));
        assert_eq!(event.actor_kind, ActorKind::ApiToken);
        assert_eq!(event.token_name.as_deref(), Some("ci"));
    }

    #[test]
    fn sequence_numbers_should_count_up_per_board() {
        let fx = fixture();
        fx.task("One");
        fx.task("Two");
        let other = fixture();
        other.task("Elsewhere");
        let seqs: Vec<u64> = fx.events().iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!(fx.db.board_event_cursor(&fx.board).expect("cursor"), 2);
        assert_eq!(other.events().len(), 1);
    }

    #[test]
    fn kinds_should_filter_and_the_cursor_should_pass_skipped_events() {
        let fx = fixture();
        let task = fx.task("Filter");
        fx.db
            .set_task_labels(&task, &["x".to_string()])
            .expect("labels");
        let page = match fx
            .db
            .board_events(&fx.board, 0, &[EventKind::TaskUpdated], 10)
            .expect("read")
        {
            EventRead::Page(page) => page,
            EventRead::Expired { oldest } => panic!("expired, oldest {oldest}"),
        };
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].0, 2);
        let page = match fx
            .db
            .board_events(&fx.board, 0, &[EventKind::CommentAdded], 10)
            .expect("read")
        {
            EventRead::Page(page) => page,
            EventRead::Expired { oldest } => panic!("expired, oldest {oldest}"),
        };
        assert!(page.events.is_empty());
        assert_eq!(page.cursor, 2, "both events were read and skipped");
    }

    #[test]
    fn limit_should_stop_at_the_last_event_returned() {
        let fx = fixture();
        for name in ["a", "b", "c"] {
            fx.task(name);
        }
        let EventRead::Page(page) = fx
            .db
            .board_events(&fx.board, 0, &EventKind::ALL, 2)
            .expect("read")
        else {
            panic!("expired");
        };
        assert_eq!((page.events.len(), page.cursor), (2, 2));
    }

    fn append(fx: &Fixture, count: usize, keep: usize) {
        let task = fx.task("Seed");
        let row = fx.db.task(&task).expect("read").expect("task");
        let events = events_of(None, &row, USER);
        let txn = fx.db.db.begin_write().expect("write");
        for _ in 0..count {
            txn_append_events(&txn, &fx.board, &events, keep).expect("append");
        }
        txn.commit().expect("commit");
    }

    #[test]
    fn retention_should_drop_the_oldest_events_past_the_cap() {
        let fx = fixture();
        append(&fx, 9, 5);
        let seqs: Vec<u64> = fx
            .events_after(5, &EventKind::ALL)
            .iter()
            .map(|(seq, _)| *seq)
            .collect();
        assert_eq!(seqs, [6, 7, 8, 9, 10]);
        let kept = match fx
            .db
            .board_events(&fx.board, 0, &EventKind::ALL, 100)
            .expect("read")
        {
            EventRead::Expired { oldest } => oldest,
            EventRead::Page(page) => panic!("kept {} events from 0", page.events.len()),
        };
        assert_eq!(kept, 5);
    }

    #[test]
    fn a_cursor_older_or_newer_than_the_events_should_be_expired() {
        let fx = fixture();
        append(&fx, 9, 5);
        for (after, oldest) in [(4, 5), (11, 5)] {
            let read = fx
                .db
                .board_events(&fx.board, after, &EventKind::ALL, 100)
                .expect("read");
            assert_eq!(read, EventRead::Expired { oldest }, "after {after}");
        }
        assert!(matches!(
            fx.db.board_events(&fx.board, 5, &EventKind::ALL, 100),
            Ok(EventRead::Page(_))
        ));
        let empty = fixture();
        assert!(matches!(
            empty.db.board_events(&empty.board, 0, &EventKind::ALL, 100),
            Ok(EventRead::Page(_))
        ));
        assert_eq!(
            empty
                .db
                .board_events(&empty.board, 1, &EventKind::ALL, 100)
                .expect("read"),
            EventRead::Expired { oldest: 0 }
        );
    }

    #[test]
    fn deleting_a_board_should_delete_its_events() {
        let fx = fixture();
        fx.task("Doomed");
        let keeper = fx
            .db
            .create_board(&AccountId::ownerless(), "Keeper")
            .expect("board");
        fx.db.delete_board(&fx.board).expect("delete");
        assert_eq!(fx.db.board_event_cursor(&fx.board).expect("cursor"), 0);
        assert_eq!(fx.db.board_event_cursor(&keeper).expect("cursor"), 0);
    }

    #[test]
    fn a_file_from_schema_24_should_upgrade_and_start_each_board_at_cursor_0() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("v24.redb");
        let path = path.to_str().expect("utf8");
        let (board, todo) = {
            let db = Db::connect(path).expect("connect");
            let board = db
                .create_board(&AccountId::ownerless(), "Legacy")
                .expect("board");
            let todo = db.create_column(&board, "Todo", None).expect("column");
            db.create_task(&todo, None, "Before", 0, None)
                .expect("task");
            let txn = db.db.begin_write().expect("write");
            txn.delete_table(BOARD_EVENTS).expect("drop events");
            txn.delete_table(EVENT_WAKEUPS).expect("drop wakeups");
            txn.open_table(META)
                .expect("meta")
                .insert(SCHEMA_VERSION_KEY, 24)
                .expect("stamp");
            txn.commit().expect("commit");
            (board, todo)
        };
        let db = Db::connect(path).expect("upgrade from version 24");
        assert_eq!(
            read_schema_version(db.db.database()).expect("stamp"),
            Some(SCHEMA_VERSION)
        );
        assert_eq!(db.board_event_cursor(&board).expect("cursor"), 0);
        db.create_task(&todo, None, "After", 0, None).expect("task");
        let EventRead::Page(page) = db
            .board_events(&board, 0, &EventKind::ALL, 10)
            .expect("read")
        else {
            panic!("expired");
        };
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].0, 1);
    }

    #[test]
    fn no_committed_file_should_hold_a_wakeup_row() {
        let fx = fixture();
        fx.task("Clean");
        let txn = fx.db.db.begin_read().expect("read");
        let table = txn.open_table(EVENT_WAKEUPS).expect("table");
        assert_eq!(table.len().expect("len"), 0);
    }

    #[tokio::test]
    async fn a_commit_should_wake_a_waiter_registered_before_it() {
        let fx = fixture();
        let mut feed = fx.db.board_feed(&fx.board);
        fx.task("Wake");
        tokio::time::timeout(Duration::from_secs(5), feed.changed())
            .await
            .expect("woken")
            .expect("feed open");
    }

    #[tokio::test]
    async fn a_commit_should_wake_only_the_waiters_on_its_board() {
        let fx = fixture();
        let other = fx
            .db
            .create_board(&AccountId::ownerless(), "Other")
            .expect("board");
        let mut feed = fx.db.board_feed(&other);
        fx.task("Elsewhere");
        let woken = tokio::time::timeout(Duration::from_millis(100), feed.changed()).await;
        assert!(woken.is_err(), "the other board's waiter stays asleep");
    }

    #[test]
    fn an_aborted_write_should_wake_nobody() {
        let fx = fixture();
        let mut feed = fx.db.board_feed(&fx.board);
        let task = fx.task("First");
        assert!(feed.has_changed().expect("open"));
        feed.mark_unchanged();
        let row = fx.db.task(&task).expect("read").expect("task");
        let txn = fx.db.db.begin_write().expect("write");
        txn_append_events(&txn, &fx.board, &events_of(None, &row, USER), 10).expect("append");
        txn.abort().expect("abort");
        assert!(!feed.has_changed().expect("open"));
        assert_eq!(fx.events().len(), 1);
    }
}
