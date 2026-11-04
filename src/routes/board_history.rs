use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;

use super::{NO_VALUE, history_actor};
use crate::models::{BinRow, HistoryKind, TaskComment, TaskEvent, TaskRow, attribution};

/// The titles of the columns of a history export.
const HEADER: [&str; 8] = [
    "Time",
    "User",
    "Task",
    "Event",
    "Property",
    "Old value",
    "New value",
    "Details",
];

/// The kinds the event group `comment` holds.
const COMMENT_KINDS: &[HistoryKind] = &[
    HistoryKind::Comment,
    HistoryKind::CommentAdded,
    HistoryKind::CommentEdited,
    HistoryKind::CommentDeleted,
];
/// The kinds the event group `relation` holds.
const RELATION_KINDS: &[HistoryKind] = &[HistoryKind::LinkAdded, HistoryKind::LinkRemoved];
/// The kinds the event group `attachment` holds.
const ATTACHMENT_KINDS: &[HistoryKind] = &[
    HistoryKind::AttachmentAdded,
    HistoryKind::AttachmentRenamed,
    HistoryKind::AttachmentDeleted,
];

/// A group of history kinds a filter names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum Group {
    /// A task was created.
    Created,
    /// A task was deleted.
    Deleted,
    /// A task was restored.
    Restored,
    /// A property of a task changed.
    Changed,
    /// A comment was written, edited or deleted.
    Comment,
    /// A relation was added or removed.
    Relation,
    /// An attachment was added, renamed or deleted.
    Attachment,
}

impl Group {
    /// Every group, in the order the filter lists them.
    pub(super) const ALL: [Self; 7] = [
        Self::Created,
        Self::Deleted,
        Self::Restored,
        Self::Changed,
        Self::Comment,
        Self::Relation,
        Self::Attachment,
    ];

    /// Returns the name a request uses for the group.
    pub(super) fn value(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Deleted => "deleted",
            Self::Restored => "restored",
            Self::Changed => "changed",
            Self::Comment => "comment",
            Self::Relation => "relation",
            Self::Attachment => "attachment",
        }
    }

    /// Returns the label the filter shows for the group.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Deleted => "Deleted",
            Self::Restored => "Restored",
            Self::Changed => "Changed",
            Self::Comment => "Comment",
            Self::Relation => "Relation",
            Self::Attachment => "Attachment",
        }
    }

    /// Returns the group a request names.
    pub(super) fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|group| group.value() == name)
    }

    /// Returns the group of a history `kind`.
    fn of(kind: HistoryKind) -> Self {
        match kind {
            HistoryKind::Created => Self::Created,
            HistoryKind::Deleted => Self::Deleted,
            HistoryKind::Restored => Self::Restored,
            _ if COMMENT_KINDS.contains(&kind) => Self::Comment,
            _ if RELATION_KINDS.contains(&kind) => Self::Relation,
            _ if ATTACHMENT_KINDS.contains(&kind) => Self::Attachment,
            _ => Self::Changed,
        }
    }
}

/// Returns the `(value, label)` of each group, as the Event filter lists them.
pub(super) fn event_groups() -> Vec<(&'static str, &'static str)> {
    Group::ALL
        .into_iter()
        .map(|group| (group.value(), group.label()))
        .collect()
}

/// Returns the `(kind, label)` pairs of the properties the group `changed` holds.
pub(super) fn changed_properties() -> Vec<(&'static str, &'static str)> {
    HistoryKind::ALL
        .into_iter()
        .filter(|kind| Group::of(*kind) == Group::Changed)
        .map(|kind| (kind.as_str(), kind.label()))
        .collect()
}

/// Which order a history lists its rows in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum Order {
    /// The latest row first.
    Newest,
    /// The earliest row first.
    Oldest,
}

impl Order {
    /// Returns the order a request names.
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "newest" => Some(Self::Newest),
            "oldest" => Some(Self::Oldest),
            _ => None,
        }
    }
}

/// Which rows of a board's history a request keeps.
#[derive(Clone, Debug)]
pub(super) struct Filter {
    /// The event group to keep, or all.
    pub(super) event: Option<Group>,
    /// The kind of change to keep, or all.
    pub(super) property: Option<String>,
    /// The user whose rows to keep, or all.
    pub(super) user: Option<String>,
    /// The first instant to keep.
    pub(super) from: Option<DateTime<Utc>>,
    /// The first instant to leave out.
    pub(super) to: Option<DateTime<Utc>>,
    /// The order of the rows.
    pub(super) order: Order,
}

impl Filter {
    /// Returns whether `row` passes every part of the filter except the user.
    fn keeps(&self, row: &HistoryRow, at: DateTime<Utc>) -> bool {
        self.event.is_none_or(|event| Group::of(row.kind) == event)
            && self
                .property
                .as_deref()
                .is_none_or(|property| row.kind.as_str() == property)
            && self.from.is_none_or(|from| at >= from)
            && self.to.is_none_or(|to| at < to)
    }
}

/// One row of a board's history.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub(super) struct HistoryRow {
    /// RFC3339 time of the event.
    pub(super) time: String,
    /// The user who made the change, followed by the API token's name in parentheses when a token
    /// made it.
    pub(super) user: String,
    /// The name of the task.
    pub(super) task: String,
    /// The readable label of the kind of event.
    pub(super) event: String,
    /// The machine-readable kind of the event.
    pub(super) kind: HistoryKind,
    /// What happened, in one sentence, or the text of a comment.
    pub(super) detail: String,
    /// Whether the event replaced one value with another, so `old_value` and `new_value` hold
    /// them.
    pub(super) changed: bool,
    /// The value the change replaced, or `(none)`.
    pub(super) old_value: String,
    /// The value the change set, or `(none)`.
    pub(super) new_value: String,
    #[serde(skip)]
    actor: String,
}

/// A board's history after a filter, with every user who made a change.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub(super) struct HistoryPage {
    /// The rows that pass the filter, in the order asked.
    pub(super) rows: Vec<HistoryRow>,
    /// Every user with a row on the board before the filter, sorted.
    pub(super) users: Vec<String>,
}

/// Returns the old and new value of a move whose event stores only its sentence.
fn moved_values(detail: &str) -> Option<(String, String)> {
    let rest = detail.strip_prefix("Moved from ")?;
    let (from, to) = rest.split_once(" to ")?;
    (!to.contains(" to ")).then(|| (from.to_string(), to.to_string()))
}

/// Shapes a history `event` of the task `task`.
fn event_row(task: &str, event: &TaskEvent) -> HistoryRow {
    let (from, to) = match (&event.from, &event.to) {
        (None, None) if event.kind == HistoryKind::Moved => match moved_values(&event.detail) {
            Some((from, to)) => (Some(from), Some(to)),
            None => (None, None),
        },
        (from, to) => (from.clone(), to.clone()),
    };
    HistoryRow {
        time: event.created_at.clone(),
        user: history_actor(event),
        task: task.to_string(),
        event: event.kind.label().to_string(),
        kind: event.kind,
        detail: event.detail.clone(),
        changed: from.is_some() || to.is_some(),
        old_value: from.unwrap_or_else(|| NO_VALUE.to_string()),
        new_value: to.unwrap_or_else(|| NO_VALUE.to_string()),
        actor: event.actor.clone(),
    }
}

/// Appends the rows of a task's comments and its visible history `events` to `rows`.
fn task_rows(rows: &mut Vec<HistoryRow>, task: &TaskRow, events: &[TaskEvent]) {
    for event in events
        .iter()
        .filter(|event| event.kind != HistoryKind::CommentAdded)
    {
        rows.push(event_row(&task.name, event));
    }
    for comment in &task.comments {
        rows.push(comment_row(&task.name, comment));
    }
}

/// Shapes a `comment` on the task `task`.
fn comment_row(task: &str, comment: &TaskComment) -> HistoryRow {
    let user = attribution(
        &comment.author,
        comment.token_name.as_deref(),
        comment.parent_token.as_ref(),
        comment.role_name.as_deref(),
        comment.label.as_deref(),
    );
    HistoryRow {
        time: comment.created_at.clone(),
        user,
        task: task.to_string(),
        event: HistoryKind::Comment.label().to_string(),
        kind: HistoryKind::Comment,
        detail: comment.body.clone(),
        changed: false,
        old_value: NO_VALUE.to_string(),
        new_value: NO_VALUE.to_string(),
        actor: comment.author.clone(),
    }
}

/// Collects the rows of the tasks on a board and of those in its recycle bin, given each task's
/// visible history, and keeps the ones `filter` passes.
pub(super) fn page(
    live: &[(TaskRow, Vec<TaskEvent>)],
    binned: &[(BinRow, Vec<TaskEvent>)],
    filter: &Filter,
) -> HistoryPage {
    let mut all = Vec::new();
    for (task, events) in live {
        task_rows(&mut all, task, events);
    }
    for (row, events) in binned {
        task_rows(&mut all, &row.task, events);
    }
    let dated: Vec<(DateTime<Utc>, HistoryRow)> = all
        .into_iter()
        .filter_map(|row| {
            let at = DateTime::parse_from_rfc3339(&row.time).ok()?.to_utc();
            Some((at, row))
        })
        .collect();
    let mut users: Vec<String> = dated.iter().map(|(_, row)| row.actor.clone()).collect();
    users.sort();
    users.dedup();
    let mut timed: Vec<(DateTime<Utc>, HistoryRow)> = dated
        .into_iter()
        .filter(|(at, row)| {
            filter.keeps(row, *at) && filter.user.as_deref().is_none_or(|user| row.actor == user)
        })
        .collect();
    timed.sort_by_key(|(at, _)| *at);
    if filter.order == Order::Newest {
        timed.reverse();
    }
    HistoryPage {
        rows: timed.into_iter().map(|(_, row)| row).collect(),
        users,
    }
}

/// Returns the rows of `page` and the header as the cells of an export.
pub(super) fn export_cells(page: &HistoryPage) -> Vec<Vec<String>> {
    let mut cells = vec![HEADER.iter().map(ToString::to_string).collect()];
    for row in &page.rows {
        let (property, old, new) = if row.changed {
            (
                row.event.clone(),
                row.old_value.clone(),
                row.new_value.clone(),
            )
        } else {
            (String::new(), String::new(), String::new())
        };
        cells.push(vec![
            row.time.clone(),
            row.user.clone(),
            row.task.clone(),
            row.event.clone(),
            property,
            old,
            new,
            row.detail.clone(),
        ]);
    }
    cells
}
