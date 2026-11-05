use std::collections::HashSet;

use chrono::Utc;
use redb::{ReadableTable, WriteTransaction};
use serde_json::{Map, Value, json};

use super::{
    COLUMNS, Db, DbResult, META, USERNAMES, WEBHOOK_OUTBOX, WEBHOOK_OUTBOX_BY_HOOK, WEBHOOKS,
    WEBHOOKS_BY_BOARD, mmap_get, mmap_insert, mmap_remove, read_in, read_one, txn_indexed_rows,
    txn_mmap_get, write_one,
};
use crate::models::{
    Actor, ActorKind, ColumnRow, HistoryKind, OutboxRow, TaskComment, TaskRow, WebhookEvent,
    WebhookRow,
};

/// Most queued events one webhook keeps. Past it each new event drops the webhook's oldest.
const MAX_OUTBOX_PER_WEBHOOK: usize = 1000;

/// Most stored bytes of queued events one webhook keeps, where its newest event always stays.
const MAX_OUTBOX_BYTES_PER_WEBHOOK: u64 = 4 * 1024 * 1024;

/// Key in [`META`] of the highest sequence number handed out, which never decreases.
const OUTBOX_SEQ_KEY: &str = "webhook_outbox_seq";

/// Digits of a sequence number in a key, which sorts the oldest event first.
const SEQ_DIGITS: usize = 20;

/// The history kinds that report a comment rather than a change to the task.
const COMMENT_HISTORY_KINDS: [HistoryKind; 3] = [
    HistoryKind::CommentAdded,
    HistoryKind::CommentEdited,
    HistoryKind::CommentDeleted,
];

/// Something a task write did that a webhook can hear about.
enum Happening<'a> {
    /// The task is new.
    Created,
    /// The task changed from the row held.
    Changed(&'a TaskRow),
    /// The task is gone.
    Deleted,
    /// The task is back from the recycle bin.
    Restored,
    /// The comment is new.
    CommentCreated(&'a TaskComment),
    /// The comment's body changed.
    CommentChanged(&'a TaskComment),
    /// The comment is gone.
    CommentDeleted(&'a TaskComment),
}

impl Happening<'_> {
    /// Returns the event a webhook subscribes to for this.
    fn event(&self) -> WebhookEvent {
        match self {
            Self::Created => WebhookEvent::TaskCreated,
            Self::Changed(_) => WebhookEvent::TaskChanged,
            Self::Deleted => WebhookEvent::TaskDeleted,
            Self::Restored => WebhookEvent::TaskRestored,
            Self::CommentCreated(_) => WebhookEvent::CommentCreated,
            Self::CommentChanged(_) => WebhookEvent::CommentChanged,
            Self::CommentDeleted(_) => WebhookEvent::CommentDeleted,
        }
    }
}

/// Who made a change, as a payload names them.
struct Who {
    /// The account id for a browser session, else the actor's name.
    id: String,
    /// The API token's name when a token made the change, else the actor's name.
    full_name: String,
    /// Id of the agent role the token was issued as, absent for any other actor.
    role_id: Option<String>,
    /// The role's name when the token was issued, absent for any other actor.
    role_name: Option<String>,
    /// The `agent-N` label of the token's issue, absent for any other actor.
    label: Option<String>,
}

/// Returns what the write of task `before` into `after` did, where a task with no `before` is new.
fn happenings<'a>(before: Option<&'a TaskRow>, after: &'a TaskRow) -> Vec<Happening<'a>> {
    let Some(before) = before else {
        return vec![Happening::Created];
    };
    let mut found = Vec::new();
    let known: HashSet<&str> = before
        .history
        .iter()
        .map(|event| event.id.as_str())
        .collect();
    let logged = after.history.iter().any(|event| {
        !known.contains(event.id.as_str()) && !COMMENT_HISTORY_KINDS.contains(&event.kind)
    });
    if logged || tracked(before, "") != tracked(after, "") {
        found.push(Happening::Changed(before));
    }
    for comment in &after.comments {
        match before.comments.iter().find(|old| old.id == comment.id) {
            None => found.push(Happening::CommentCreated(comment)),
            Some(old) if old.body != comment.body || old.edited_at != comment.edited_at => {
                found.push(Happening::CommentChanged(comment));
            }
            Some(_) => {}
        }
    }
    for comment in &before.comments {
        if !after.comments.iter().any(|kept| kept.id == comment.id) {
            found.push(Happening::CommentDeleted(comment));
        }
    }
    found
}

/// Queues the events the write of task `before` into `after` makes for the board's webhooks, inside
/// the open write transaction that stores `after`.
pub(crate) fn txn_queue_events(
    txn: &WriteTransaction,
    before: Option<&TaskRow>,
    after: &TaskRow,
    actor: Actor<'_>,
) -> DbResult<()> {
    txn_queue(txn, after, &happenings(before, after), actor)
}

/// Queues a task deleted event for `task`, inside the open write transaction that deletes it.
pub(crate) fn txn_queue_task_deleted(
    txn: &WriteTransaction,
    task: &TaskRow,
    actor: Actor<'_>,
) -> DbResult<()> {
    txn_queue(txn, task, &[Happening::Deleted], actor)
}

/// Queues a task restored event for `task`, inside the open write transaction that puts it back.
pub(crate) fn txn_queue_task_restored(
    txn: &WriteTransaction,
    task: &TaskRow,
    actor: Actor<'_>,
) -> DbResult<()> {
    txn_queue(txn, task, &[Happening::Restored], actor)
}

/// Queues each of `happenings` on `task` once for every enabled webhook of its board that
/// subscribes to it and watches its column, and trims each webhook's queue.
fn txn_queue(
    txn: &WriteTransaction,
    task: &TaskRow,
    happenings: &[Happening<'_>],
    actor: Actor<'_>,
) -> DbResult<()> {
    if happenings.is_empty() {
        return Ok(());
    }
    let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &task.column_id)? else {
        return Ok(());
    };
    let hooks = txn_hooks_for(txn, &column)?;
    let wanted: Vec<&Happening<'_>> = happenings
        .iter()
        .filter(|happening| {
            hooks
                .iter()
                .any(|hook| hook.events.contains(&happening.event()))
        })
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let who = txn_who(txn, actor)?;
    let now = Utc::now().to_rfc3339();
    let mut bodies = Vec::new();
    for happening in wanted {
        let body = payload(txn, happening, task, &column, &who, &now)?;
        bodies.push((happening.event(), body));
    }
    txn_queue_rows(txn, &hooks, &bodies, &now)
}

/// Returns the enabled webhooks of `column`'s board that watch it, read inside an open write
/// transaction.
fn txn_hooks_for(txn: &WriteTransaction, column: &ColumnRow) -> DbResult<Vec<WebhookRow>> {
    Ok(
        txn_indexed_rows::<WebhookRow>(txn, WEBHOOKS_BY_BOARD, &column.board_id, WEBHOOKS)?
            .into_iter()
            .filter(|hook| {
                hook.enabled && hook.column_id.as_deref().is_none_or(|id| id == column.id)
            })
            .collect(),
    )
}

/// Stores each of `bodies` once for every webhook of `hooks` that subscribes to its event, and
/// trims the queue of each webhook to its bounds.
fn txn_queue_rows(
    txn: &WriteTransaction,
    hooks: &[WebhookRow],
    bodies: &[(WebhookEvent, String)],
    now: &str,
) -> DbResult<()> {
    let mut seq = txn_last_seq(txn)?;
    for hook in hooks {
        for (event, body) in bodies
            .iter()
            .filter(|(event, _)| hook.events.contains(event))
        {
            seq += 1;
            let row = OutboxRow {
                id: format!("{seq:0width$}", width = SEQ_DIGITS),
                webhook_id: hook.id.clone(),
                board_id: hook.board_id.clone(),
                event: *event,
                body: body.clone(),
                created_at: now.to_string(),
                attempts: 0,
                next_attempt_at: None,
            };
            write_one(txn, WEBHOOK_OUTBOX, &row.id, &row)?;
            mmap_insert(txn, WEBHOOK_OUTBOX_BY_HOOK, &hook.id, &row.id)?;
        }
        txn_trim_outbox(txn, &hook.id)?;
    }
    txn.open_table(META)?.insert(OUTBOX_SEQ_KEY, seq)?;
    Ok(())
}

/// Returns who `actor` is in a payload, read inside an open write transaction.
fn txn_who(txn: &WriteTransaction, actor: Actor<'_>) -> DbResult<Who> {
    let account = if actor.kind == ActorKind::User {
        let usernames = txn.open_table(USERNAMES)?;
        usernames.get(actor.name)?.map(|id| id.value().to_string())
    } else {
        None
    };
    Ok(Who {
        id: account.unwrap_or_else(|| actor.name.to_string()),
        full_name: actor.token_name.unwrap_or(actor.name).to_string(),
        role_id: actor.role_id.map(str::to_string),
        role_name: actor.role_name.map(str::to_string),
        label: actor.label.map(str::to_string),
    })
}

/// Returns the highest sequence number handed out, or 0 when none was.
fn txn_last_seq(txn: &WriteTransaction) -> DbResult<u64> {
    let meta = txn.open_table(META)?;
    Ok(meta.get(OUTBOX_SEQ_KEY)?.map_or(0, |seq| seq.value()))
}

/// Drops the oldest events of webhook `webhook_id` until it holds at most
/// [`MAX_OUTBOX_PER_WEBHOOK`] events and [`MAX_OUTBOX_BYTES_PER_WEBHOOK`] stored bytes, keeping
/// its newest event.
fn txn_trim_outbox(txn: &WriteTransaction, webhook_id: &str) -> DbResult<()> {
    let keys = txn_mmap_get(txn, WEBHOOK_OUTBOX_BY_HOOK, webhook_id)?;
    let mut sizes = Vec::with_capacity(keys.len());
    {
        let table = txn.open_table(WEBHOOK_OUTBOX)?;
        for key in &keys {
            sizes.push(
                table
                    .get(key.as_str())?
                    .map_or(0, |raw| raw.value().len() as u64),
            );
        }
    }
    let mut dropped = keys.len().saturating_sub(MAX_OUTBOX_PER_WEBHOOK);
    let mut held: u64 = sizes[dropped..].iter().sum();
    while held > MAX_OUTBOX_BYTES_PER_WEBHOOK && dropped + 1 < keys.len() {
        held -= sizes[dropped];
        dropped += 1;
    }
    for key in &keys[..dropped] {
        txn_remove_event(txn, key)?;
        mmap_remove(txn, WEBHOOK_OUTBOX_BY_HOOK, webhook_id, key)?;
    }
    Ok(())
}

/// Removes event `key` from the outbox table inside an open write transaction and returns it.
pub(crate) fn txn_remove_event(txn: &WriteTransaction, key: &str) -> DbResult<Option<OutboxRow>> {
    let raw = txn
        .open_table(WEBHOOK_OUTBOX)?
        .remove(key)?
        .map(|guard| guard.value().to_vec());
    Ok(raw.map(|raw| serde_json::from_slice(&raw)).transpose()?)
}

/// Removes every event queued for webhook `webhook_id`, inside an open write transaction.
pub(crate) fn txn_remove_webhook_outbox(txn: &WriteTransaction, webhook_id: &str) -> DbResult<()> {
    for key in txn_mmap_get(txn, WEBHOOK_OUTBOX_BY_HOOK, webhook_id)? {
        txn_remove_event(txn, &key)?;
    }
    txn.open_multimap_table(WEBHOOK_OUTBOX_BY_HOOK)?
        .remove_all(webhook_id)?;
    Ok(())
}

/// Returns the JSON body of `happening` on `task`, which sits in `column`.
fn payload(
    txn: &WriteTransaction,
    happening: &Happening<'_>,
    task: &TaskRow,
    column: &ColumnRow,
    who: &Who,
    now: &str,
) -> DbResult<String> {
    let mut body = Map::new();
    body.insert("eventType".to_string(), json!(happening.event()));
    body.insert("userId".to_string(), json!(who.id));
    body.insert("userFullName".to_string(), json!(who.full_name));
    for (key, value) in [
        ("roleId", &who.role_id),
        ("roleName", &who.role_name),
        ("label", &who.label),
    ] {
        if let Some(value) = value {
            body.insert(key.to_string(), json!(value));
        }
    }
    body.insert("timestamp".to_string(), json!(now));
    body.insert("task".to_string(), task_value(task, column));
    match happening {
        Happening::Changed(before) => {
            let changed = changed_properties(txn, before, task, column)?;
            body.insert("changedProperties".to_string(), Value::Object(changed));
        }
        Happening::CommentCreated(comment)
        | Happening::CommentChanged(comment)
        | Happening::CommentDeleted(comment) => {
            body.insert("comment".to_string(), comment_value(comment));
        }
        Happening::Created | Happening::Deleted | Happening::Restored => {}
    }
    Ok(serde_json::to_string(&Value::Object(body))?)
}

/// Returns the task fields a change can report, each under the name the task API gives it, with
/// the name of the column the task sits in.
fn tracked(task: &TaskRow, column_name: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("name", json!(task.name)),
        ("description", json!(task.description)),
        ("column_id", json!(task.column_id)),
        ("column_name", json!(column_name)),
        ("swimlane_id", json!(task.swimlane_id)),
        ("color_id", json!(task.color_id)),
        ("size", json!(task.size)),
        ("estimate_hours", json!(task.estimate_hours)),
        ("labels", json!(task.labels)),
        ("member_ids", json!(task.member_ids)),
        ("responsible", json!(task.responsible)),
        ("due_at", json!(task.due_at)),
        ("due_repeat", json!(task.due_repeat)),
        ("due_done", json!(task.due_done)),
        ("due_target_column_id", json!(task.due_target_column_id)),
        ("grouping_date", json!(task.grouping_date)),
        ("watched", json!(task.watched)),
        ("completed_at", json!(task.completed_at)),
        ("subtasks", json!(task.subtasks)),
        ("links", json!(task.links)),
        ("fields", json!(task.fields)),
        ("metadata", json!(task.metadata)),
    ]
}

/// Returns `task` as a payload shows it: the fields of [`tracked`] with its id, board and times.
fn task_value(task: &TaskRow, column: &ColumnRow) -> Value {
    let mut fields: Map<String, Value> = tracked(task, &column.name)
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    fields.insert("id".to_string(), json!(task.id));
    fields.insert("board_id".to_string(), json!(column.board_id));
    fields.insert("created_at".to_string(), json!(task.created_at));
    fields.insert("updated_at".to_string(), json!(task.last_updated()));
    fields.insert(
        "column_entered_at".to_string(),
        json!(task.column_entered_at()),
    );
    Value::Object(fields)
}

/// Returns each field that differs between `before` and `after` as its old and new value, where
/// `column` is the column `after` sits in, read inside an open write transaction.
fn changed_properties(
    txn: &WriteTransaction,
    before: &TaskRow,
    after: &TaskRow,
    column: &ColumnRow,
) -> DbResult<Map<String, Value>> {
    let old_column = if before.column_id == after.column_id {
        column.name.clone()
    } else {
        read_in::<ColumnRow>(txn, COLUMNS, &before.column_id)?
            .map(|old| old.name)
            .unwrap_or_default()
    };
    let old = tracked(before, &old_column);
    let new = tracked(after, &column.name);
    Ok(old
        .into_iter()
        .zip(new)
        .filter(|((_, old), (_, new))| old != new)
        .map(|((name, old), (_, new))| (name.to_string(), json!({"old": old, "new": new})))
        .collect())
}

/// Returns `comment` as a payload shows it.
fn comment_value(comment: &TaskComment) -> Value {
    json!({
        "id": comment.id,
        "author": comment.author,
        "body": comment.body,
        "created_at": comment.created_at,
        "edited_at": comment.edited_at,
        "kind": comment.kind,
    })
}

impl Db {
    /// Returns the events queued for webhook `webhook_id`, oldest first.
    pub fn webhook_outbox(&self, webhook_id: &str) -> DbResult<Vec<OutboxRow>> {
        let mut rows = Vec::new();
        for key in mmap_get(&self.db, WEBHOOK_OUTBOX_BY_HOOK, webhook_id)? {
            rows.extend(read_one(&self.db, WEBHOOK_OUTBOX, &key)?);
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use redb::ReadableTableMetadata;

    use super::*;
    use crate::db::usage::{OverQuota, txn_store_task};
    use crate::db::{
        FencedWrite, MoveGuards, NewComment, NewTaskPlacement, NewWebhook, TASKS, push_event,
    };
    use crate::models::{AccountId, HistoryKind, InstanceMode, Subtask};

    const USER: Actor<'static> = Actor {
        name: "owner",
        token_name: None,
        kind: ActorKind::User,
        parent_token: None,
        role_id: None,
        role_name: None,
        label: None,
    };

    const TOKEN: Actor<'static> = Actor {
        name: "api-token",
        token_name: Some("ci"),
        kind: ActorKind::ApiToken,
        parent_token: None,
        role_id: None,
        role_name: None,
        label: None,
    };

    const EVERY_EVENT: [WebhookEvent; 6] = [
        WebhookEvent::TaskCreated,
        WebhookEvent::TaskChanged,
        WebhookEvent::TaskDeleted,
        WebhookEvent::CommentCreated,
        WebhookEvent::CommentChanged,
        WebhookEvent::CommentDeleted,
    ];

    const WITH_RESTORE: [WebhookEvent; 7] = [
        WebhookEvent::TaskCreated,
        WebhookEvent::TaskChanged,
        WebhookEvent::TaskDeleted,
        WebhookEvent::TaskRestored,
        WebhookEvent::CommentCreated,
        WebhookEvent::CommentChanged,
        WebhookEvent::CommentDeleted,
    ];

    /// A member's board with a Todo and a Done column.
    struct Fixture {
        #[expect(dead_code, reason = "keeps the database directory alive")]
        dir: tempfile::TempDir,
        db: Db,
        member: AccountId,
        board: String,
        todo: String,
        done: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("outbox.redb").to_str().expect("utf8")).expect("db");
        db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        db.apply_mode(InstanceMode::Multi).expect("mode");
        let board = db.create_board(&member, "Bob").expect("board");
        let todo = db.create_column(&board, "Todo", None).expect("column");
        let done = db.create_column(&board, "Done", None).expect("column");
        Fixture {
            dir,
            db,
            member,
            board,
            todo,
            done,
        }
    }

    impl Fixture {
        fn hook_on(&self, events: &[WebhookEvent], column: Option<&str>, enabled: bool) -> String {
            self.hook_row(events, column, enabled).id
        }

        fn hook_row(
            &self,
            events: &[WebhookEvent],
            column: Option<&str>,
            enabled: bool,
        ) -> WebhookRow {
            self.db
                .create_webhook(NewWebhook {
                    board_id: self.board.clone(),
                    name: "Hook".to_string(),
                    url: "https://example.com/hook".to_string(),
                    events: events.to_vec(),
                    column_id: column.map(str::to_string),
                    enabled,
                })
                .expect("webhook")
        }

        fn hook(&self) -> String {
            self.hook_on(&EVERY_EVENT, None, true)
        }

        fn task_in(&self, column: &str, name: &str) -> String {
            let row = Db::new_task_row(column, None, name, 0, None);
            self.db
                .insert_task_checked(row, NewTaskPlacement::Bottom, USER)
                .expect("task")
                .expect("fields")
        }

        fn task(&self, name: &str) -> String {
            self.task_in(&self.todo, name)
        }

        fn rename(&self, task: &str, name: &str) {
            self.db
                .apply_task_change(task, USER, |row| {
                    row.name = name.to_string();
                    push_event(row, HistoryKind::Renamed, "Renamed".to_string(), USER);
                    ((), true)
                })
                .expect("rename");
        }

        fn move_to(&self, task: &str, column: &str) {
            let guards = MoveGuards {
                event: Some("Moved"),
                ..MoveGuards::default()
            };
            self.db
                .move_task_logged(task, column, None, 0.0, guards, USER)
                .expect("move");
        }

        fn comment(&self, task: &str, body: &str) -> String {
            let new = NewComment {
                author: "owner",
                body,
                actor_kind: ActorKind::User,
                token_name: None,
                token_id: None,
                kind: None,
            };
            match self.db.add_comment(task, new, USER, None).expect("comment") {
                FencedWrite::Written(comment) => comment.id,
                _ => panic!("the comment was not written"),
            }
        }

        fn events(&self, hook: &str) -> Vec<WebhookEvent> {
            self.db
                .webhook_outbox(hook)
                .expect("outbox")
                .iter()
                .map(|row| row.event)
                .collect()
        }

        fn bodies(&self, hook: &str) -> Vec<Value> {
            self.db
                .webhook_outbox(hook)
                .expect("outbox")
                .iter()
                .map(|row| serde_json::from_str(&row.body).expect("json"))
                .collect()
        }

        fn queued(&self) -> u64 {
            let txn = self.db.db.begin_read().expect("read");
            txn.open_table(WEBHOOK_OUTBOX)
                .expect("table")
                .len()
                .expect("len")
        }
    }

    #[test]
    fn should_queue_task_created_with_the_payload_fields() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Write");
        let rows = fx.db.webhook_outbox(&hook).expect("outbox");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, WebhookEvent::TaskCreated);
        assert_eq!(rows[0].webhook_id, hook);
        assert_eq!(rows[0].board_id, fx.board);
        assert_eq!(rows[0].attempts, 0);
        assert_eq!(rows[0].next_attempt_at, None);
        let body = &fx.bodies(&hook)[0];
        assert_eq!(body["eventType"], "taskCreated");
        assert_eq!(body["userId"], "owner");
        assert_eq!(body["userFullName"], "owner");
        assert!(DateTime::parse_from_rfc3339(body["timestamp"].as_str().expect("text")).is_ok());
        assert_eq!(body["task"]["id"], task);
        assert_eq!(body["task"]["name"], "Write");
        assert_eq!(body["task"]["column_id"], fx.todo);
        assert_eq!(body["task"]["column_name"], "Todo");
        assert_eq!(body["task"]["board_id"], fx.board);
    }

    #[test]
    fn should_name_the_account_of_a_browser_user_and_the_token_of_an_api_write() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Write");
        fx.db
            .apply_task_change(
                &task,
                Actor {
                    name: "bob",
                    ..USER
                },
                |row| {
                    row.name = "By bob".to_string();
                    push_event(row, HistoryKind::Renamed, "Renamed".to_string(), USER);
                    ((), true)
                },
            )
            .expect("rename");
        fx.db
            .apply_task_change(&task, TOKEN, |row| {
                row.name = "By token".to_string();
                push_event(row, HistoryKind::Renamed, "Renamed".to_string(), TOKEN);
                ((), true)
            })
            .expect("rename");
        let bodies = fx.bodies(&hook);
        assert_eq!(bodies[1]["userId"], fx.member.as_str());
        assert_eq!(bodies[1]["userFullName"], "bob");
        assert_eq!(bodies[2]["userId"], "api-token");
        assert_eq!(bodies[2]["userFullName"], "ci");
    }

    #[test]
    fn should_queue_task_changed_with_the_old_and_new_name_for_a_rename() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Old");
        fx.rename(&task, "New");
        assert_eq!(
            fx.events(&hook),
            [WebhookEvent::TaskCreated, WebhookEvent::TaskChanged]
        );
        let body = &fx.bodies(&hook)[1];
        assert_eq!(body["eventType"], "taskChanged");
        assert_eq!(body["task"]["name"], "New");
        let changed = body["changedProperties"].as_object().expect("object");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed["name"], json!({"old": "Old", "new": "New"}));
    }

    #[test]
    fn should_queue_task_changed_with_the_old_and_new_column_for_a_move() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Move");
        fx.move_to(&task, &fx.done);
        let body = &fx.bodies(&hook)[1];
        let changed = &body["changedProperties"];
        assert_eq!(
            changed["column_id"],
            json!({"old": fx.todo, "new": fx.done})
        );
        assert_eq!(
            changed["column_name"],
            json!({"old": "Todo", "new": "Done"})
        );
        assert_eq!(body["task"]["column_name"], "Done");
    }

    #[test]
    fn should_queue_task_deleted_with_the_task_in_the_delete_transaction() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Gone");
        assert!(fx.db.delete_task(&task, TOKEN).expect("delete"));
        assert_eq!(
            fx.events(&hook),
            [WebhookEvent::TaskCreated, WebhookEvent::TaskDeleted]
        );
        let body = &fx.bodies(&hook)[1];
        assert_eq!(body["eventType"], "taskDeleted");
        assert_eq!(body["userFullName"], "ci");
        assert_eq!(body["task"]["id"], task);
        assert_eq!(body["task"]["name"], "Gone");
    }

    #[test]
    fn should_queue_only_a_restored_event_when_a_deleted_task_is_restored() {
        let fx = fixture();
        let everything = fx.hook_on(&WITH_RESTORE, None, true);
        let old = fx.hook_on(&EVERY_EVENT, Some(fx.done.as_str()), true);
        let new = fx.hook_on(&[WebhookEvent::TaskRestored], Some(fx.todo.as_str()), true);
        let task = fx.task_in(&fx.done, "Back");
        assert!(fx.db.delete_task(&task, USER).expect("delete"));
        assert!(matches!(
            fx.db.delete_column(&fx.done).expect("column"),
            crate::db::ColumnDeleteOutcome::Deleted
        ));
        fx.db
            .restore_task(&task, chrono::Utc::now().timestamp(), USER)
            .expect("restore");
        assert_eq!(
            fx.events(&everything),
            [
                WebhookEvent::TaskCreated,
                WebhookEvent::TaskDeleted,
                WebhookEvent::TaskRestored
            ]
        );
        let body = &fx.bodies(&everything)[2];
        assert_eq!(body["eventType"], "taskRestored");
        assert_eq!(body["task"]["id"], task);
        assert_eq!(body["task"]["column_id"], fx.todo);
        assert_eq!(fx.events(&new), [WebhookEvent::TaskRestored]);
        assert_eq!(
            fx.events(&old),
            [WebhookEvent::TaskCreated, WebhookEvent::TaskDeleted]
        );
    }
    #[test]
    fn should_queue_comment_created_with_the_comment() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Talk");
        let comment = fx.comment(&task, "hello");
        assert_eq!(
            fx.events(&hook),
            [WebhookEvent::TaskCreated, WebhookEvent::CommentCreated]
        );
        let body = &fx.bodies(&hook)[1];
        assert_eq!(body["eventType"], "commentCreated");
        assert_eq!(body["task"]["id"], task);
        assert_eq!(body["comment"]["id"], comment);
        assert_eq!(body["comment"]["body"], "hello");
    }

    #[test]
    fn should_queue_comment_changed_with_the_new_body() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Talk");
        let comment = fx.comment(&task, "hello");
        fx.db
            .edit_comment(&task, &comment, "hello again", None, USER)
            .expect("edit");
        assert_eq!(
            fx.events(&hook),
            [
                WebhookEvent::TaskCreated,
                WebhookEvent::CommentCreated,
                WebhookEvent::CommentChanged
            ]
        );
        let body = &fx.bodies(&hook)[2];
        assert_eq!(body["eventType"], "commentChanged");
        assert_eq!(body["comment"]["id"], comment);
        assert_eq!(body["comment"]["body"], "hello again");
    }

    #[test]
    fn should_queue_comment_deleted_with_the_comment_that_went() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Talk");
        let comment = fx.comment(&task, "hello");
        fx.db
            .delete_comment(&task, &comment, None, USER)
            .expect("delete");
        assert_eq!(
            fx.events(&hook),
            [
                WebhookEvent::TaskCreated,
                WebhookEvent::CommentCreated,
                WebhookEvent::CommentDeleted
            ]
        );
        let body = &fx.bodies(&hook)[2];
        assert_eq!(body["eventType"], "commentDeleted");
        assert_eq!(body["comment"]["id"], comment);
        assert_eq!(body["comment"]["body"], "hello");
    }

    #[test]
    fn should_queue_only_the_events_a_webhook_subscribes_to() {
        let fx = fixture();
        let hook = fx.hook_on(&[WebhookEvent::TaskDeleted], None, true);
        let task = fx.task("Quiet");
        fx.rename(&task, "Still quiet");
        assert!(fx.events(&hook).is_empty());
        fx.db.delete_task(&task, USER).expect("delete");
        assert_eq!(fx.events(&hook), [WebhookEvent::TaskDeleted]);
    }

    #[test]
    fn should_drop_an_event_the_column_filter_does_not_match_and_accept_one_it_does() {
        let fx = fixture();
        let hook = fx.hook_on(&EVERY_EVENT, Some(fx.done.as_str()), true);
        let elsewhere = fx.task("Todo task");
        fx.rename(&elsewhere, "Renamed in Todo");
        assert!(fx.events(&hook).is_empty());
        let here = fx.task_in(&fx.done, "Done task");
        assert_eq!(fx.events(&hook), [WebhookEvent::TaskCreated]);
        fx.rename(&here, "Renamed in Done");
        assert_eq!(
            fx.events(&hook),
            [WebhookEvent::TaskCreated, WebhookEvent::TaskChanged]
        );
    }

    #[test]
    fn should_match_a_move_by_the_column_the_task_lands_in() {
        let fx = fixture();
        let hook = fx.hook_on(&EVERY_EVENT, Some(fx.done.as_str()), true);
        let task = fx.task("Traveller");
        fx.move_to(&task, &fx.done);
        assert_eq!(fx.events(&hook), [WebhookEvent::TaskChanged]);
        fx.move_to(&task, &fx.todo);
        assert_eq!(
            fx.events(&hook),
            [WebhookEvent::TaskChanged],
            "leaving the filtered column does not match"
        );
    }

    #[test]
    fn should_match_a_deletion_by_the_column_the_task_was_in() {
        let fx = fixture();
        let hook = fx.hook_on(&[WebhookEvent::TaskDeleted], Some(fx.done.as_str()), true);
        let outside = fx.task("Outside");
        let inside = fx.task_in(&fx.done, "Inside");
        fx.db.delete_task(&outside, USER).expect("delete");
        assert!(fx.events(&hook).is_empty());
        fx.db.delete_task(&inside, USER).expect("delete");
        assert_eq!(fx.events(&hook), [WebhookEvent::TaskDeleted]);
    }

    #[test]
    fn should_queue_nothing_for_a_disabled_webhook() {
        let fx = fixture();
        let hook = fx.hook_on(&EVERY_EVENT, None, false);
        let task = fx.task("Muted");
        fx.rename(&task, "Still muted");
        fx.db.delete_task(&task, USER).expect("delete");
        assert!(fx.events(&hook).is_empty());
        assert_eq!(fx.queued(), 0);
    }

    #[test]
    fn should_queue_exactly_one_row_for_each_matching_webhook() {
        let fx = fixture();
        let first = fx.hook();
        let second = fx.hook_on(&[WebhookEvent::TaskCreated], Some(fx.todo.as_str()), true);
        let other = fx.hook_on(&[WebhookEvent::TaskDeleted], None, true);
        fx.task("Shared");
        assert_eq!(fx.events(&first), [WebhookEvent::TaskCreated]);
        assert_eq!(fx.events(&second), [WebhookEvent::TaskCreated]);
        assert!(fx.events(&other).is_empty());
        assert_eq!(fx.queued(), 2);
        let ids: Vec<String> = [&first, &second]
            .iter()
            .map(|hook| fx.db.webhook_outbox(hook).expect("outbox")[0].id.clone())
            .collect();
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn should_queue_nothing_when_no_webhook_is_configured() {
        let fx = fixture();
        let task = fx.task("Nobody listens");
        fx.rename(&task, "Still nobody");
        fx.db.delete_task(&task, USER).expect("delete");
        assert_eq!(fx.queued(), 0);
    }

    #[test]
    fn should_queue_nothing_for_a_write_that_changes_nothing() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Quiet");
        fx.db
            .apply_task_change(&task, USER, |_| ((), true))
            .expect("change");
        assert_eq!(fx.events(&hook), [WebhookEvent::TaskCreated]);
    }

    #[test]
    fn should_queue_task_changed_for_a_fields_only_write() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Fields");
        fx.db
            .apply_task_change(&task, USER, |row| {
                row.fields.insert("team".to_string(), json!("core"));
                ((), true)
            })
            .expect("change");
        assert_eq!(
            fx.events(&hook),
            [WebhookEvent::TaskCreated, WebhookEvent::TaskChanged]
        );
        let changed = &fx.bodies(&hook)[1]["changedProperties"];
        assert_eq!(changed["fields"]["new"], json!({"team": "core"}));
    }

    #[test]
    fn should_queue_task_changed_for_a_metadata_only_change() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Metadata");
        fx.db
            .apply_task_change(&task, USER, |row| {
                let mut metadata = Map::new();
                metadata.insert("source".to_string(), json!("ci"));
                row.metadata = Some(metadata);
                ((), true)
            })
            .expect("change");
        let body = &fx.bodies(&hook)[1];
        assert_eq!(body["eventType"], "taskChanged");
        assert_eq!(
            body["changedProperties"]["metadata"]["new"],
            json!({"source": "ci"})
        );
    }

    #[test]
    fn should_queue_task_changed_for_a_subtask_add() {
        let fx = fixture();
        let hook = fx.hook();
        let task = fx.task("Checklist");
        fx.db
            .apply_task_change(&task, USER, |row| {
                row.subtasks.push(Subtask {
                    id: "sub".to_string(),
                    name: "Step".to_string(),
                    done: false,
                    due_at: None,
                    token_id: None,
                    member_ids: Vec::new(),
                });
                ((), true)
            })
            .expect("change");
        let body = &fx.bodies(&hook)[1];
        assert_eq!(body["eventType"], "taskChanged");
        assert_eq!(body["changedProperties"]["subtasks"]["old"], json!([]));
        assert_eq!(body["task"]["subtasks"][0]["name"], "Step");
    }

    #[test]
    fn should_never_put_the_webhook_secret_in_a_body() {
        let fx = fixture();
        let hook = fx.hook_row(&EVERY_EVENT, None, true);
        assert!(!hook.secret.is_empty());
        let task = fx.task("Secret");
        fx.rename(&task, "Renamed");
        let comment = fx.comment(&task, "hello");
        fx.db
            .edit_comment(&task, &comment, "edited", None, USER)
            .expect("edit");
        fx.db.delete_task(&task, USER).expect("delete");
        let rows = fx.db.webhook_outbox(&hook.id).expect("outbox");
        assert!(rows.len() >= 5);
        assert!(rows.iter().all(|row| !row.body.contains(&hook.secret)));
    }

    #[test]
    fn should_not_reuse_a_key_after_the_newest_rows_are_removed() {
        let fx = fixture();
        let first = fx.hook();
        fx.task("One");
        fx.task("Two");
        let used: Vec<String> = fx
            .db
            .webhook_outbox(&first)
            .expect("outbox")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(used.len(), 2);
        assert!(fx.db.delete_webhook(&first).expect("delete"));
        assert_eq!(fx.queued(), 0);
        let second = fx.hook();
        fx.task("Three");
        let next = &fx.db.webhook_outbox(&second).expect("outbox")[0].id;
        assert_eq!(*next, format!("{:020}", 3));
        assert!(!used.contains(next));
    }

    #[test]
    fn should_drop_the_oldest_events_past_the_byte_budget() {
        let fx = fixture();
        let hook = fx.hook_on(&[WebhookEvent::TaskCreated], None, true);
        let total = 30;
        let txn = fx.db.db.begin_write().expect("write");
        for index in 0..total {
            let mut row = Db::new_task_row(&fx.todo, None, &format!("Big {index}"), 0, None);
            row.description = "x".repeat(200_000);
            txn_queue_events(&txn, None, &row, USER).expect("queue");
        }
        txn.commit().expect("commit");
        let rows = fx.db.webhook_outbox(&hook).expect("outbox");
        let held: u64 = rows.iter().map(|row| row.body.len() as u64).sum();
        assert!(held <= MAX_OUTBOX_BYTES_PER_WEBHOOK);
        assert!(rows.len() >= 15 && rows.len() < total);
        assert_eq!(rows.last().expect("newest").id, format!("{total:020}"));
    }

    #[test]
    fn should_keep_the_newest_event_when_it_alone_is_past_the_byte_budget() {
        let fx = fixture();
        let hook = fx.hook_on(&[WebhookEvent::TaskCreated], None, true);
        let txn = fx.db.db.begin_write().expect("write");
        for _ in 0..2 {
            let mut row = Db::new_task_row(&fx.todo, None, "Huge", 0, None);
            row.description = "x".repeat(5 * 1024 * 1024);
            txn_queue_events(&txn, None, &row, USER).expect("queue");
        }
        txn.commit().expect("commit");
        let rows = fx.db.webhook_outbox(&hook).expect("outbox");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, format!("{:020}", 2));
    }

    #[test]
    fn should_leave_no_row_when_the_write_transaction_aborts() {
        let fx = fixture();
        let hook = fx.hook();
        let txn = fx.db.db.begin_write().expect("write");
        let row = Db::new_task_row(&fx.todo, None, "Rolled back", 0, None);
        {
            let mut tasks = txn.open_table(TASKS).expect("tasks");
            txn_store_task(&txn, &mut tasks, &row, OverQuota::Allow, USER).expect("store");
        }
        let inside = txn
            .open_table(WEBHOOK_OUTBOX)
            .expect("table")
            .len()
            .expect("len");
        assert_eq!(inside, 1, "the row is queued in the task's own transaction");
        txn.abort().expect("abort");
        assert!(fx.events(&hook).is_empty());
        assert_eq!(fx.queued(), 0);
    }

    #[test]
    fn should_drop_the_oldest_events_past_the_cap() {
        let fx = fixture();
        let hook = fx.hook_on(&[WebhookEvent::TaskCreated], None, true);
        let spare = fx.hook_on(&[WebhookEvent::TaskDeleted], None, true);
        let extra = 5;
        let txn = fx.db.db.begin_write().expect("write");
        for index in 0..MAX_OUTBOX_PER_WEBHOOK + extra {
            let row = Db::new_task_row(&fx.todo, None, &format!("Task {index}"), 0, None);
            txn_queue_events(&txn, None, &row, USER).expect("queue");
        }
        txn.commit().expect("commit");
        let rows = fx.db.webhook_outbox(&hook).expect("outbox");
        assert_eq!(rows.len(), MAX_OUTBOX_PER_WEBHOOK);
        assert_eq!(rows[0].id, format!("{:020}", extra + 1));
        assert_eq!(
            rows[MAX_OUTBOX_PER_WEBHOOK - 1].id,
            format!("{:020}", MAX_OUTBOX_PER_WEBHOOK + extra)
        );
        assert!(fx.events(&spare).is_empty());
        assert_eq!(fx.queued(), MAX_OUTBOX_PER_WEBHOOK as u64);
    }

    #[test]
    fn should_number_events_oldest_first_across_webhooks() {
        let fx = fixture();
        let first = fx.hook();
        let second = fx.hook();
        let task = fx.task("Count");
        fx.rename(&task, "Counted");
        let mut keys: Vec<String> = [&first, &second]
            .iter()
            .flat_map(|hook| fx.db.webhook_outbox(hook).expect("outbox"))
            .map(|row| row.id)
            .collect();
        keys.sort();
        let wanted: Vec<String> = (1..=4).map(|seq| format!("{seq:020}")).collect();
        assert_eq!(keys, wanted);
    }

    #[test]
    fn should_remove_the_events_of_a_deleted_webhook() {
        let fx = fixture();
        let gone = fx.hook();
        let kept = fx.hook();
        fx.task("Both");
        assert_eq!(fx.queued(), 2);
        assert!(fx.db.delete_webhook(&gone).expect("delete"));
        assert!(fx.events(&gone).is_empty());
        assert_eq!(fx.events(&kept), [WebhookEvent::TaskCreated]);
        assert_eq!(fx.queued(), 1);
    }

    #[test]
    fn should_remove_the_events_of_a_deleted_board() {
        let fx = fixture();
        let hook = fx.hook();
        fx.task("Queued");
        assert_eq!(fx.queued(), 1);
        fx.db.create_board(&fx.member, "Spare").expect("board");
        fx.db.delete_board(&fx.board).expect("delete board");
        assert!(fx.events(&hook).is_empty());
        assert_eq!(fx.queued(), 0);
    }
}
