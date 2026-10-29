mod import;
mod spool;
mod tar;

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::{fmt, vec};

use chrono::{DateTime, SecondsFormat, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;
pub use import::{ImportCounts, ImportError, Importer, MAX_ARCHIVE_BYTES, read_board};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[cfg(test)]
pub use spool::SPOOL_PREFIX;
pub use spool::Spool;

use crate::db::{BoardSnapshot, DbResult, OfflineDb};
use crate::models::{
    ActorKind, BoardRow, ColorRow, ColumnRow, FieldColumn, FieldDefRow, FieldKind, FieldOwner,
    LaneLimit, LinkKind, SwimlaneRow, TaskClaim, TaskLink, TaskRow, TimeEntryRow,
};
use crate::taskjson::TaskArray;

/// Value of the `format` key of every archive's `board.json`.
const FORMAT: &str = "pomb-board-archive";

/// Name of the JSON document inside an archive.
const BOARD_JSON: &str = "board.json";

/// Bytes of an attachment's file written to the archive at a time, 64 KiB.
const CHUNK_BYTES: usize = 65_536;

/// Longest attachment file name, in bytes, that the archive keeps.
const ATTACHMENT_NAME_BYTES: usize = 255;

/// Longest account or board name, in bytes, that a path or a download name keeps.
const NAME_BYTES: usize = 80;

/// Why an archive could not be built or written.
#[derive(Debug)]
pub struct ArchiveError(String);

impl ArchiveError {
    /// Returns the error that says `message`.
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ArchiveError {}

impl From<io::Error> for ArchiveError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<Box<dyn Error>> for ArchiveError {
    fn from(error: Box<dyn Error>) -> Self {
        Self(error.to_string())
    }
}

/// The start of `board.json`, up to and including its list of tasks.
#[derive(Serialize)]
struct Document<'a> {
    /// Always [`FORMAT`].
    format: &'static str,
    /// The version of pomb that wrote the archive.
    pomb_version: &'static str,
    /// When the archive was written, as RFC 3339.
    exported_at: &'a str,
    /// The board and its settings.
    board: BoardInfo,
    /// The board's columns, in board order.
    columns: Vec<ColumnInfo>,
    /// The board's swimlanes, in board order.
    swimlanes: Vec<LaneInfo>,
    /// The board's task colors, in palette order.
    colors: Vec<ColorInfo>,
    /// The board's custom field definitions, in board order.
    custom_fields: Vec<FieldInfo>,
    /// Every task of the board, grouped by column in board order, which the stream fills in.
    tasks: Vec<TaskInfo>,
}

/// The whole of `board.json` as an import reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardDocument {
    /// Always [`FORMAT`].
    format: String,
    /// The version of pomb that wrote the archive.
    pomb_version: String,
    /// When the archive was written, as RFC 3339.
    exported_at: String,
    /// The board and its settings.
    board: BoardInfo,
    /// The board's columns.
    columns: Vec<ColumnInfo>,
    /// The board's swimlanes.
    swimlanes: Vec<LaneInfo>,
    /// The board's task colors.
    colors: Vec<ColorInfo>,
    /// The board's custom field definitions.
    custom_fields: Vec<FieldInfo>,
    /// Every task of the board.
    tasks: Vec<TaskInfo>,
}

/// A board as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardInfo {
    /// The board's id.
    id: String,
    /// The board's name.
    name: String,
    /// The board's place among its owner's boards.
    position: i64,
    /// The handle of the account that owns the board.
    owner: Option<String>,
    /// The board's settings bag, without any secret.
    settings: Map<String, Value>,
}

/// A column as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColumnInfo {
    /// The column's id.
    id: String,
    /// The column's name.
    name: String,
    /// The column's place on the board.
    position: i64,
    /// The most tasks the column holds before the board flags it, or null for no limit.
    wip_limit: Option<i64>,
    /// Whether a task moved into the column counts as done.
    is_done: bool,
    /// The column's description.
    description: String,
    /// Whether the column hides its tasks.
    collapsed: bool,
    /// The names the column had before each rename, oldest first.
    former_names: Vec<String>,
    /// The column's settings bag.
    settings: Map<String, Value>,
}

/// A swimlane as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaneInfo {
    /// The swimlane's id.
    id: String,
    /// The swimlane's name.
    name: String,
    /// The swimlane's place on the board.
    position: i64,
    /// The swimlane's description.
    description: String,
    /// The most tasks each column holds in this lane.
    limits: Vec<LaneLimit>,
    /// Whether the lane is folded.
    collapsed: bool,
    /// The lane's height in pixels, or null for the default.
    height: Option<i64>,
}

/// A task color as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColorInfo {
    /// The color's id.
    id: String,
    /// The standard color the slot holds, such as `yellow`.
    value: String,
    /// The label the board gives the color.
    label: String,
    /// The legend tooltip text.
    description: String,
    /// The hex of the card background.
    background_hex: String,
    /// The hex of the card border.
    border_hex: String,
    /// The hex of the light variant.
    light_hex: String,
    /// Whether the task color picker offers the color.
    enabled: bool,
    /// Whether new tasks get the color.
    is_default: bool,
    /// The color's place in the palette.
    sort_order: i64,
}

/// A custom field definition as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldInfo {
    /// The field's id.
    id: String,
    /// The key that names the field's value on each task.
    key: String,
    /// The label people see.
    label: String,
    /// The kind of value the field holds.
    kind: FieldKind,
    /// The choices of a `select` or `multiselect` field.
    options: Vec<String>,
    /// The typed columns of a `table` field.
    columns: Vec<FieldColumn>,
    /// The value a new task gets, or null.
    default_value: Option<Value>,
    /// Whether the field must be filled in.
    required: bool,
    /// Whether people or only programs edit the field.
    owner: FieldOwner,
    /// The field's place among the board's fields.
    position: i64,
    /// When the field was created, as RFC 3339.
    created_at: String,
}

/// A task as `board.json` describes it, with its time entries.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInfo {
    /// The task's id.
    id: String,
    /// The task's name.
    name: String,
    /// The task's description, in Markdown.
    description: String,
    /// The id of the column the task sits in.
    column_id: String,
    /// The id of the swimlane the task sits in, or null.
    swimlane_id: Option<String>,
    /// The id of the task's color, or null.
    color_id: Option<String>,
    /// The task's place in its cell.
    position: f64,
    /// The task's legacy size in pomodori.
    size_pomodori: i64,
    /// When the task was created, as RFC 3339.
    created_at: String,
    /// When the task was last written, as RFC 3339, or null for an old row.
    updated_at: Option<String>,
    /// When the task reached a done column, as RFC 3339, or null.
    completed_at: Option<String>,
    /// When the task entered its column, as RFC 3339, or null for an old row.
    column_entered_at: Option<String>,
    /// The date that overrides the task's group in a date-grouped column, or null.
    grouping_date: Option<String>,
    /// The task's labels.
    labels: Vec<String>,
    /// The handles of the accounts the task is assigned to.
    assignees: Vec<String>,
    /// Whether the owner watches the task.
    watching: bool,
    /// The due date, as RFC 3339, or null.
    due_at: Option<String>,
    /// How the due date repeats, as free text, or null.
    due_repeat: Option<String>,
    /// Whether the due date was marked done.
    due_done: bool,
    /// The id of the column the task should reach by its due date, or null.
    due_target_column_id: Option<String>,
    /// The time estimate in hours, or null.
    estimate_hours: Option<f64>,
    /// How many pomodori the task completed.
    pomodori_completed: u32,
    /// How many pomodori were stopped early.
    interruptions: u32,
    /// The task's checklist.
    subtasks: Vec<SubtaskInfo>,
    /// The task's comments, oldest first.
    comments: Vec<CommentInfo>,
    /// The task's attachments, with the archive path of each file.
    attachments: Vec<AttachmentInfo>,
    /// The links the task holds.
    links: Vec<LinkInfo>,
    /// The task's history, oldest first.
    history: Vec<EventInfo>,
    /// The latest claim, which may have expired, or null.
    claim: Option<TaskClaim>,
    /// The fence of the latest claim, 0 when never claimed.
    claim_fence: u64,
    /// The caller-owned metadata object, or null.
    metadata: Option<Map<String, Value>>,
    /// The values of the custom fields, by field key.
    field_values: Map<String, Value>,
    /// The time logged on the task, oldest first.
    time_entries: Vec<TimeInfo>,
}

/// A checklist item as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubtaskInfo {
    /// The item's id.
    id: String,
    /// The item's text.
    name: String,
    /// Whether the item is ticked.
    done: bool,
    /// The item's due date, as RFC 3339, or null.
    due_at: Option<String>,
}

/// A comment as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentInfo {
    /// The comment's id.
    id: String,
    /// Who wrote the comment.
    author: String,
    /// The comment's text, in Markdown.
    body: String,
    /// When the comment was written, as RFC 3339.
    created_at: String,
    /// How the author signed in.
    actor_kind: ActorKind,
    /// The name of the API token that wrote the comment, or null.
    token_name: Option<String>,
    /// The comment's tag, such as `agent`, or null.
    kind: Option<String>,
    /// When the body was last edited, as RFC 3339, or null.
    edited_at: Option<String>,
}

/// An attachment as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentInfo {
    /// The attachment's id.
    id: String,
    /// The file's name.
    name: String,
    /// The file's MIME type.
    mime: String,
    /// The file's size in bytes, as stored with the task.
    size: u64,
    /// Who uploaded the file.
    uploaded_by: String,
    /// When the file was uploaded, as RFC 3339.
    created_at: String,
    /// The path of the file inside the archive, or null when the file's bytes are gone.
    file: Option<String>,
}

/// A link between tasks as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkInfo {
    /// The link's id.
    id: String,
    /// How the tasks relate.
    kind: LinkKind,
    /// The id of the other task when it is on this board, or null.
    target_task_id: Option<String>,
    /// The name of the other task, or null when it is gone.
    target_task_name: Option<String>,
    /// The name of the board the other task is on, set only when it is not this one.
    target_board_name: Option<String>,
    /// When the link was made, as RFC 3339.
    created_at: String,
}

/// A history event as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventInfo {
    /// The event's id.
    id: String,
    /// The machine-readable kind, such as `moved`.
    kind: String,
    /// The one-line description.
    detail: String,
    /// Who caused the event.
    actor: String,
    /// When the event happened, as RFC 3339.
    created_at: String,
    /// The id of the column a `created` or `moved` event put the task in, or null.
    column_id: Option<String>,
    /// The name of the API token behind the event, or null.
    token_name: Option<String>,
}

/// A time entry as `board.json` describes it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeInfo {
    /// The entry's id.
    id: String,
    /// When the session started, as RFC 3339.
    started_at: String,
    /// The entry's length in whole minutes.
    minutes: i64,
    /// The entry's length in seconds, 0 for an old row.
    seconds: i64,
    /// The kind of session, such as `pomodoro` or `manual`.
    kind: String,
    /// The entry's note.
    note: String,
    /// Whether the session was stopped early.
    interrupted: bool,
    /// Why the session was stopped early, or null.
    interrupt_reason: Option<String>,
    /// The entry's labels.
    labels: Vec<String>,
    /// The handle of the account that logged the entry, or of the board owner for an older entry.
    created_by: String,
    /// The name of the break activity the session was attributed to, or null.
    break_activity: Option<String>,
}

/// The ids and names `board.json` resolves its references against.
#[derive(Debug)]
struct Lookup {
    /// The ids of the tasks on the board.
    tasks: HashSet<String>,
    /// The ids of the board's columns.
    columns: HashSet<String>,
    /// The ids of the board's swimlanes.
    lanes: HashSet<String>,
    /// The ids of the board's colors.
    colors: HashSet<String>,
    /// The name of each break activity of the owner, by id.
    activities: HashMap<String, String>,
    /// The handle of each account read so far, by id, absent for one that no longer exists.
    handles: HashMap<String, Option<String>>,
    /// The handle of the board owner, whose account holds every entry on the board.
    owner: Option<String>,
}

impl Lookup {
    /// Returns the handle of account `id`, or [`None`] when it no longer exists.
    fn handle(&mut self, snapshot: &BoardSnapshot, id: &str) -> DbResult<Option<String>> {
        if let Some(known) = self.handles.get(id) {
            return Ok(known.clone());
        }
        let handle = snapshot.user(id)?.map(|user| user.username);
        self.handles.insert(id.to_string(), handle.clone());
        Ok(handle)
    }

    /// Returns `id` when it names a column of the board.
    fn column(&self, id: Option<String>) -> Option<String> {
        id.filter(|id| self.columns.contains(id))
    }

    /// Returns `id` when it names a swimlane of the board.
    fn lane(&self, id: Option<String>) -> Option<String> {
        id.filter(|id| self.lanes.contains(id))
    }

    /// Returns `id` when it names a color of the board.
    fn color(&self, id: Option<String>) -> Option<String> {
        id.filter(|id| self.colors.contains(id))
    }
}

/// Returns a settings bag without the keys that hold a secret.
#[must_use]
fn readable_settings(config_json: &str) -> Map<String, Value> {
    let mut settings: Map<String, Value> = serde_json::from_str(config_json).unwrap_or_default();
    settings.retain(|key, _| !key.ends_with("_token") && !key.contains("secret"));
    settings
}

/// Returns the document's description of `board`, owned by the account with handle `owner`.
fn board_info(board: &BoardRow, owner: Option<String>) -> BoardInfo {
    BoardInfo {
        id: board.id.clone(),
        name: board.name.clone(),
        position: board.position,
        owner,
        settings: readable_settings(&board.config_json),
    }
}

/// Returns the document's description of `column`.
fn column_info(column: ColumnRow) -> ColumnInfo {
    ColumnInfo {
        settings: readable_settings(&column.config_json),
        id: column.id,
        name: column.name,
        position: column.position,
        wip_limit: column.wip_limit,
        is_done: column.is_done,
        description: column.description,
        collapsed: column.collapsed,
        former_names: column.former_names,
    }
}

/// Returns the document's description of `lane`.
fn lane_info(lane: SwimlaneRow) -> LaneInfo {
    LaneInfo {
        id: lane.id,
        name: lane.name,
        position: lane.position,
        description: lane.description,
        limits: lane.limits,
        collapsed: lane.collapsed,
        height: lane.height,
    }
}

/// Returns the document's description of `color`.
fn color_info(color: ColorRow) -> ColorInfo {
    ColorInfo {
        id: color.id,
        value: color.value,
        label: color.label,
        description: color.description,
        background_hex: color.background_hex,
        border_hex: color.border_hex,
        light_hex: color.light_hex,
        enabled: color.enabled,
        is_default: color.is_default,
        sort_order: color.sort_order,
    }
}

/// Returns the document's description of `field`.
fn field_info(field: FieldDefRow) -> FieldInfo {
    FieldInfo {
        id: field.id,
        key: field.key,
        label: field.label,
        kind: field.kind,
        options: field.options,
        columns: field.columns,
        default_value: field.default,
        required: field.required,
        owner: field.owner,
        position: field.position,
        created_at: field.created_at,
    }
}

/// Returns the document's description of `entry`.
fn time_info(lookup: &Lookup, entry: TimeEntryRow) -> TimeInfo {
    TimeInfo {
        break_activity: entry
            .activity_id
            .and_then(|id| lookup.activities.get(&id).cloned()),
        id: entry.id,
        started_at: entry.started_at,
        minutes: entry.minutes,
        seconds: entry.seconds,
        kind: entry.kind,
        note: entry.note,
        interrupted: entry.interrupted,
        interrupt_reason: entry.interrupt_reason,
        labels: entry.labels,
        created_by: if entry.created_by.is_empty() {
            lookup.owner.clone().unwrap_or_default()
        } else {
            entry.created_by
        },
    }
}

/// Returns the document's description of `link`.
fn link_info(snapshot: &BoardSnapshot, lookup: &Lookup, link: TaskLink) -> DbResult<LinkInfo> {
    let inside = lookup.tasks.contains(&link.target_id);
    let target = snapshot.link_target(&link.target_id)?;
    Ok(LinkInfo {
        id: link.id,
        kind: link.kind,
        target_task_id: inside.then_some(link.target_id),
        target_task_name: target.as_ref().map(|target| target.task_name.clone()),
        target_board_name: target.filter(|_| !inside).map(|target| target.board_name),
        created_at: link.created_at,
    })
}

/// Returns the path inside an archive of the file of attachment `id` named `name`.
#[must_use]
fn attachment_path(id: &str, name: &str) -> String {
    format!(
        "attachments/{id}/{}",
        safe_component(name, "file", ATTACHMENT_NAME_BYTES)
    )
}

/// Returns the document's description of `row`, with its time entries.
fn task_info(snapshot: &BoardSnapshot, lookup: &mut Lookup, row: TaskRow) -> DbResult<TaskInfo> {
    let mut assignees: Vec<String> = Vec::new();
    for id in &row.member_ids {
        if let Some(handle) = lookup.handle(snapshot, id)?
            && !assignees.contains(&handle)
        {
            assignees.push(handle);
        }
    }
    let mut attachments = Vec::with_capacity(row.attachments.len());
    for attachment in row.attachments {
        let file = snapshot
            .has_attachment(&attachment.id)?
            .then(|| attachment_path(&attachment.id, &attachment.name));
        attachments.push(AttachmentInfo {
            id: attachment.id,
            name: attachment.name,
            mime: attachment.mime,
            size: attachment.size,
            uploaded_by: attachment.uploaded_by,
            created_at: attachment.created_at,
            file,
        });
    }
    let mut links = Vec::with_capacity(row.links.len());
    for link in row.links {
        links.push(link_info(snapshot, lookup, link)?);
    }
    let time_entries: Vec<TimeInfo> = snapshot
        .time_entries(&row.id)?
        .into_iter()
        .map(|entry| time_info(lookup, entry))
        .collect();
    let subtasks: Vec<SubtaskInfo> = row
        .subtasks
        .into_iter()
        .map(|subtask| SubtaskInfo {
            id: subtask.id,
            name: subtask.name,
            done: subtask.done,
            due_at: subtask.due_at,
        })
        .collect();
    let comments: Vec<CommentInfo> = row
        .comments
        .into_iter()
        .map(|comment| CommentInfo {
            id: comment.id,
            author: comment.author,
            body: comment.body,
            created_at: comment.created_at,
            actor_kind: comment.actor_kind,
            token_name: comment.token_name,
            kind: comment.kind,
            edited_at: comment.edited_at,
        })
        .collect();
    let history: Vec<EventInfo> = row
        .history
        .into_iter()
        .map(|event| EventInfo {
            id: event.id,
            kind: event.kind,
            detail: event.detail,
            actor: event.actor,
            created_at: event.created_at,
            column_id: lookup.column(event.column_id),
            token_name: event.token_name,
        })
        .collect();
    Ok(TaskInfo {
        id: row.id,
        name: row.name,
        description: row.description,
        column_id: row.column_id,
        swimlane_id: lookup.lane(row.swimlane_id),
        color_id: lookup.color(row.color_id),
        position: row.position,
        size_pomodori: row.size,
        created_at: row.created_at,
        updated_at: row.updated_at,
        completed_at: row.completed_at,
        column_entered_at: row.column_entered_at,
        grouping_date: row.grouping_date,
        labels: row.labels,
        assignees,
        watching: row.watched,
        due_at: row.due_at,
        due_repeat: row.due_repeat,
        due_done: row.due_done,
        due_target_column_id: lookup.column(row.due_target_column_id),
        estimate_hours: row.estimate_hours,
        pomodori_completed: row.pomodori_completed,
        interruptions: row.interruptions,
        subtasks,
        comments,
        attachments,
        links,
        history,
        claim: row.claim,
        claim_fence: row.claim_fence,
        metadata: row.metadata,
        field_values: row.fields,
        time_entries,
    })
}

/// Reads task `id` and describes it, or returns [`None`] when it is gone.
fn read_task(
    snapshot: &BoardSnapshot,
    lookup: &mut Lookup,
    id: &str,
) -> DbResult<Option<TaskInfo>> {
    snapshot
        .task(id)?
        .map(|row| task_info(snapshot, lookup, row))
        .transpose()
}

/// An attachment file to write after `board.json`.
#[derive(Debug)]
struct AttachmentFile {
    /// The attachment's id, which names its bytes in the database.
    id: String,
    /// The path of the file inside the archive.
    path: String,
}

/// What the archive writes next.
#[derive(Debug)]
enum Stage {
    /// The header of `board.json`.
    Start,
    /// The pieces of `board.json`.
    Json,
    /// The header of the next attachment file, or the end of the archive.
    Files,
    /// The bytes of an attachment file not yet written.
    File {
        /// The file's bytes.
        data: Vec<u8>,
        /// How many bytes are written.
        offset: usize,
    },
    /// Every byte is out, or a step failed.
    Done,
}

/// A board's archive, a gzipped tar of `board.json` and the files of its attachments, produced
/// as chunks one at a time so neither the board nor its files are held whole.
#[derive(Debug)]
pub struct Archive {
    /// The one transaction every read goes through.
    snapshot: BoardSnapshot,
    /// What the document's references resolve against.
    lookup: Lookup,
    /// The pieces of `board.json` still to write.
    array: TaskArray,
    /// The attachment files still to write.
    files: vec::IntoIter<AttachmentFile>,
    /// The size of `board.json`, which the first pass measured.
    json_size: u64,
    /// How many bytes of `board.json` are written.
    json_written: u64,
    /// The modification time of every file, in Unix seconds.
    mtime: u64,
    /// Compresses the tar stream into a buffer that each chunk empties.
    encoder: GzEncoder<Vec<u8>>,
    /// What the archive writes next.
    stage: Stage,
}

impl Archive {
    /// Returns the archive of `snapshot`'s board, written at `now`.
    ///
    /// # Errors
    ///
    /// Returns [`ArchiveError`] when the board cannot be read.
    pub fn new(snapshot: BoardSnapshot, now: DateTime<Utc>) -> Result<Self, ArchiveError> {
        let columns = snapshot.columns()?;
        let lanes = snapshot.swimlanes()?;
        let colors = snapshot.colors()?;
        let fields = snapshot.fields()?;
        let ids = snapshot.task_ids()?;
        let mut lookup = Lookup {
            tasks: ids.iter().cloned().collect(),
            columns: columns.iter().map(|column| column.id.clone()).collect(),
            lanes: lanes.iter().map(|lane| lane.id.clone()).collect(),
            colors: colors.iter().map(|color| color.id.clone()).collect(),
            activities: snapshot
                .break_activities()?
                .into_iter()
                .map(|activity| (activity.id, activity.name))
                .collect(),
            handles: HashMap::new(),
            owner: None,
        };
        let owner = lookup.handle(&snapshot, &snapshot.board().owner_id)?;
        lookup.owner.clone_from(&owner);
        let exported_at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
        let document = Document {
            format: FORMAT,
            pomb_version: env!("CARGO_PKG_VERSION"),
            exported_at: &exported_at,
            board: board_info(snapshot.board(), owner),
            columns: columns.into_iter().map(column_info).collect(),
            swimlanes: lanes.into_iter().map(lane_info).collect(),
            colors: colors.into_iter().map(color_info).collect(),
            custom_fields: fields.into_iter().map(field_info).collect(),
            tasks: Vec::new(),
        };
        let mut files = Vec::new();
        let mut json_size = 0u64;
        let mut sizing = TaskArray::new(&document, ids.clone())?;
        while let Some(piece) = sizing.next_piece(|id| {
            let task = read_task(&snapshot, &mut lookup, id)?;
            for attachment in task.iter().flat_map(|task| &task.attachments) {
                if let Some(path) = &attachment.file {
                    files.push(AttachmentFile {
                        id: attachment.id.clone(),
                        path: path.clone(),
                    });
                }
            }
            Ok(task)
        })? {
            json_size += piece.len() as u64;
        }
        Ok(Self {
            array: TaskArray::new(&document, ids)?,
            files: files.into_iter(),
            json_size,
            json_written: 0,
            mtime: u64::try_from(now.timestamp()).unwrap_or(0),
            encoder: GzEncoder::new(Vec::new(), Compression::default()),
            stage: Stage::Start,
            snapshot,
            lookup,
        })
    }

    /// Returns the next chunk of the gzipped archive, or [`None`] once the last is out.
    ///
    /// # Errors
    ///
    /// Returns [`ArchiveError`] when the board or an attachment cannot be read. The archive is
    /// over after that.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ArchiveError> {
        loop {
            if matches!(self.stage, Stage::Done) {
                return Ok(None);
            }
            self.step()?;
            let chunk = std::mem::take(self.encoder.get_mut());
            if !chunk.is_empty() {
                return Ok(Some(chunk));
            }
        }
    }

    /// Writes the next piece of the tar stream into the encoder.
    fn step(&mut self) -> Result<(), ArchiveError> {
        match std::mem::replace(&mut self.stage, Stage::Done) {
            Stage::Start => {
                let header = tar::entry_header(BOARD_JSON, self.json_size, self.mtime)?;
                self.encoder.write_all(&header)?;
                self.stage = Stage::Json;
            }
            Stage::Json => {
                let Self {
                    snapshot,
                    lookup,
                    array,
                    ..
                } = &mut *self;
                let piece = array.next_piece(|id| read_task(snapshot, lookup, id))?;
                match piece {
                    Some(piece) => {
                        self.json_written += piece.len() as u64;
                        self.encoder.write_all(&piece)?;
                        self.stage = Stage::Json;
                    }
                    None => {
                        if self.json_written != self.json_size {
                            return Err(ArchiveError::new("board.json changed size while written"));
                        }
                        self.pad(self.json_size)?;
                        self.stage = Stage::Files;
                    }
                }
            }
            Stage::Files => match self.files.next() {
                Some(file) => {
                    let data = self.snapshot.attachment(&file.id)?.ok_or_else(|| {
                        ArchiveError::new(format!("attachment {} lost its bytes", file.id))
                    })?;
                    let header = tar::entry_header(&file.path, data.len() as u64, self.mtime)?;
                    self.encoder.write_all(&header)?;
                    self.stage = Stage::File { data, offset: 0 };
                }
                None => {
                    self.encoder.write_all(&tar::ZEROS)?;
                    self.encoder.try_finish()?;
                }
            },
            Stage::File { data, offset } => {
                let end = data.len().min(offset + CHUNK_BYTES);
                self.encoder.write_all(&data[offset..end])?;
                if end == data.len() {
                    self.pad(data.len() as u64)?;
                    self.stage = Stage::Files;
                } else {
                    self.stage = Stage::File { data, offset: end };
                }
            }
            Stage::Done => {}
        }
        Ok(())
    }

    /// Writes the zero bytes that pad `size` bytes of file data to a whole block.
    fn pad(&mut self, size: u64) -> io::Result<()> {
        self.encoder.write_all(&tar::ZEROS[..tar::padding(size)])
    }
}

/// Returns `name` as one path component: path separators, control characters and the characters
/// Windows refuses become `_`, dots and spaces leave its ends, and it is cut to `max_bytes` on a
/// character boundary. A name that leaves nothing becomes `fallback`.
#[must_use]
fn safe_component(name: &str, fallback: &str, max_bytes: usize) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let ends = |c: char| c == '.' || c.is_whitespace();
    let trimmed = mapped.trim_matches(ends);
    let mut end = trimmed.len().min(max_bytes);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let cut = trimmed[..end].trim_end_matches(ends);
    if cut.is_empty() {
        fallback.to_string()
    } else {
        cut.to_string()
    }
}

/// Returns the file name a browser saves the archive of the board named `board_name` as.
#[must_use]
pub fn download_name(board_name: &str) -> String {
    format!("{}.tar.gz", safe_component(board_name, "board", NAME_BYTES))
}

/// Returns the file name of `board`'s archive inside its owner's directory.
#[must_use]
fn board_file(board: &BoardRow) -> String {
    format!(
        "{}-{}.tar.gz",
        safe_component(&board.name, "board", NAME_BYTES),
        safe_component(&board.id, "board", NAME_BYTES)
    )
}

/// Writes every board of every account in `db` to `out`, as
/// `<account handle>/<board name>-<board id>.tar.gz`, and returns how many it wrote.
///
/// # Errors
///
/// Returns [`ArchiveError`] when a board cannot be read or a file cannot be written. An archive
/// is written beside its final name and renamed, so a failure leaves no half-written archive
/// behind under a final name.
pub fn archive_database(
    db: &OfflineDb,
    out: &Path,
    now: DateTime<Utc>,
) -> Result<usize, ArchiveError> {
    let mut written = 0;
    let mut used = HashSet::new();
    for user in db.accounts()? {
        let account = user.account_id();
        let boards = db.boards(&account)?;
        if boards.is_empty() {
            continue;
        }
        let mut name = safe_component(&user.username, "account", NAME_BYTES);
        if used.contains(&name) {
            name = format!("{name}-{}", safe_component(&user.id, "account", NAME_BYTES));
        }
        used.insert(name.clone());
        let dir = out.join(&name);
        fs::create_dir_all(&dir)
            .map_err(|error| ArchiveError::new(format!("{}: {error}", dir.display())))?;
        for board in boards {
            let Some(snapshot) = db.snapshot(&account, &board.id)? else {
                continue;
            };
            write_archive(Archive::new(snapshot, now)?, &dir.join(board_file(&board)))?;
            written += 1;
        }
    }
    Ok(written)
}

/// Writes `archive` to `path` through a `.part` file beside it.
fn write_archive(mut archive: Archive, path: &Path) -> Result<(), ArchiveError> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    copy_chunks(&mut archive, &part, path).map_err(|error| {
        let _ = fs::remove_file(&part);
        ArchiveError::new(format!("{}: {error}", path.display()))
    })
}

/// Writes every chunk of `archive` to the file `part`, then renames it to `path`.
fn copy_chunks(archive: &mut Archive, part: &Path, path: &Path) -> Result<(), ArchiveError> {
    let mut file = File::create(part)?;
    while let Some(chunk) = archive.next_chunk()? {
        file.write_all(&chunk)?;
    }
    file.flush()?;
    fs::rename(part, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;

    use chrono::TimeZone;
    use flate2::read::GzDecoder;
    use serde_json::json;

    use super::*;
    use crate::db::{ColumnKind, Db, NewComment, SCHEMA_VERSION, SchemaTooNew};
    use crate::models::{AccountId, Actor};

    fn untar(gzipped: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut tar = Vec::new();
        GzDecoder::new(gzipped)
            .read_to_end(&mut tar)
            .expect("valid gzip");
        let mut files = BTreeMap::new();
        let mut at = 0;
        let mut long_path: Option<String> = None;
        while at + tar::BLOCK <= tar.len() {
            let header = &tar[at..at + tar::BLOCK];
            if header.iter().all(|byte| *byte == 0) {
                break;
            }
            let field = |from: usize, to: usize| {
                let text = std::str::from_utf8(&header[from..to]).expect("utf8");
                text.trim_end_matches(['\0', ' ']).to_string()
            };
            let size = usize::from_str_radix(&field(124, 135), 8).expect("octal size");
            let start = at + tar::BLOCK;
            let data = tar[start..start + size].to_vec();
            at = start + size + tar::padding(size as u64);
            if header[156] == b'x' {
                let record = String::from_utf8(data).expect("utf8 record");
                let (_, rest) = record.split_once("path=").expect("path record");
                long_path = Some(rest.trim_end_matches('\n').to_string());
                continue;
            }
            let path = long_path.take().unwrap_or_else(|| field(0, 100));
            files.insert(path, data);
        }
        files
    }

    struct Seeded {
        db: Db,
        account: AccountId,
        board: String,
        task: String,
        attachment: String,
    }

    fn seed(dir: &Path) -> Seeded {
        let path = dir.join("pomb.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let account = db.create_user("chip").expect("user");
        let board = db.create_board(&account, "Work / Plans").expect("board");
        let todo = db.create_column(&board, "Todo", Some(3)).expect("column");
        let done = db
            .create_column_as(&board, "Done", None, ColumnKind::Done)
            .expect("column");
        let lane = db.create_swimlane(&board, "Lane").expect("lane");
        let color = db.list_colors(&board).expect("colors")[1].id.clone();
        let task = db
            .create_task(&todo, Some(&lane), "Write archive", 2, Some(&color))
            .expect("task");
        let other = db
            .create_task(&done, None, "Ship archive", 1, None)
            .expect("task");
        db.add_subtask(&task, "Design").expect("subtask");
        db.create_entry(&task, 25, "first block").expect("entry");
        let field = FieldDefRow {
            id: String::new(),
            board_id: board.clone(),
            key: "stage".to_string(),
            label: "Stage".to_string(),
            kind: FieldKind::Text,
            options: Vec::new(),
            columns: Vec::new(),
            default: None,
            required: false,
            owner: FieldOwner::User,
            position: 0,
            created_at: String::new(),
        };
        db.create_field(field).expect("field");
        let comment = NewComment {
            author: "chip",
            body: "looks good",
            actor_kind: ActorKind::User,
            token_name: None,
            token_id: Some("tok-secret-id"),
            kind: None,
        };
        db.add_comment(&task, comment, Actor::SYSTEM, None)
            .expect("comment");
        db.add_task_link(&task, LinkKind::Related, &other, Actor::SYSTEM)
            .expect("link");
        let attachment = db
            .add_attachment(
                &task,
                "notes.txt",
                "text/plain",
                b"attached bytes",
                Actor::SYSTEM,
            )
            .expect("attachment")
            .expect("task")
            .id;
        let member = db.owner().expect("read").expect("owner").id;
        db.apply_task_change(&task, Actor::SYSTEM, |row| {
            row.member_ids.push(member);
            row.labels.push("urgent".to_string());
            row.fields.insert("stage".to_string(), json!("draft"));
            ((), true)
        })
        .expect("change");
        db.ensure_board_config_string(&board, "calendar_feed_token", "feed-secret-value")
            .expect("feed token");
        Seeded {
            db,
            account,
            board,
            task,
            attachment,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap()
    }

    fn archive_of(seeded: &Seeded) -> BTreeMap<String, Vec<u8>> {
        let snapshot = seeded
            .db
            .account(&seeded.account)
            .board_snapshot(&seeded.board)
            .expect("snapshot")
            .expect("owned board");
        let mut archive = Archive::new(snapshot, now()).expect("archive");
        let mut gzipped = Vec::new();
        while let Some(chunk) = archive.next_chunk().expect("chunk") {
            gzipped.extend_from_slice(&chunk);
        }
        untar(&gzipped)
    }

    fn document(files: &BTreeMap<String, Vec<u8>>) -> Value {
        serde_json::from_slice(&files["board.json"]).expect("board.json parses")
    }

    #[test]
    fn archive_should_hold_board_json_and_every_attachment_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let files = archive_of(&seeded);
        let doc = document(&files);
        assert_eq!(doc["format"], "pomb-board-archive");
        assert_eq!(doc["pomb_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(doc["exported_at"], "2026-10-29T12:00:00Z");
        assert!(doc.get("schema_version").is_none());
        let path = format!("attachments/{}/notes.txt", seeded.attachment);
        assert_eq!(files[&path], b"attached bytes");
        assert_eq!(files.len(), 2);
        let task = doc["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|task| task["id"] == seeded.task)
            .expect("task");
        assert_eq!(task["attachments"][0]["file"], path);
    }

    #[test]
    fn board_json_should_describe_everything_stored_on_the_board() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let doc = document(&archive_of(&seeded));
        assert_eq!(doc["board"]["name"], "Work / Plans");
        assert_eq!(doc["board"]["owner"], "chip");
        assert_eq!(doc["columns"][0]["name"], "Todo");
        assert_eq!(doc["columns"][0]["wip_limit"], 3);
        assert_eq!(doc["columns"][1]["is_done"], true);
        assert_eq!(doc["swimlanes"][0]["name"], "Lane");
        assert!(!doc["colors"].as_array().expect("colors").is_empty());
        assert_eq!(doc["custom_fields"][0]["key"], "stage");
        let tasks = doc["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 2);
        let task = &tasks[0];
        assert_eq!(task["name"], "Write archive");
        assert_eq!(task["assignees"], json!(["chip"]));
        assert_eq!(task["labels"], json!(["urgent"]));
        assert_eq!(task["field_values"], json!({"stage": "draft"}));
        assert_eq!(task["subtasks"][0]["name"], "Design");
        assert_eq!(task["comments"][0]["body"], "looks good");
        assert_eq!(task["time_entries"][0]["note"], "first block");
        assert_eq!(task["time_entries"][0]["minutes"], 25);
        assert_eq!(task["links"][0]["kind"], "related");
        assert!(task["history"].is_array());
    }

    #[test]
    fn board_json_should_name_the_handle_that_logged_each_time_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        seeded
            .db
            .create_entry_at(
                &seeded.task,
                10,
                "by handle",
                "2026-10-29T10:00:00+00:00",
                &[],
                "alice",
            )
            .expect("entry");
        let doc = document(&archive_of(&seeded));
        let entries = doc["tasks"][0]["time_entries"].as_array().expect("entries");
        let created_by = |note: &str| {
            entries
                .iter()
                .find(|entry| entry["note"] == note)
                .map(|entry| entry["created_by"].clone())
        };
        assert_eq!(created_by("by handle"), Some(json!("alice")));
        assert_eq!(created_by("first block"), Some(json!("chip")));
    }

    #[test]
    fn board_json_should_resolve_every_id_inside_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let doc = document(&archive_of(&seeded));
        let ids = |key: &str| -> HashSet<String> {
            doc[key]
                .as_array()
                .expect(key)
                .iter()
                .map(|row| row["id"].as_str().expect("id").to_string())
                .collect()
        };
        let (columns, lanes, colors, tasks) = (
            ids("columns"),
            ids("swimlanes"),
            ids("colors"),
            ids("tasks"),
        );
        let field_keys: HashSet<&str> = doc["custom_fields"]
            .as_array()
            .expect("fields")
            .iter()
            .map(|field| field["key"].as_str().expect("key"))
            .collect();
        let resolves = |set: &HashSet<String>, value: &Value| {
            value.is_null() || set.contains(value.as_str().expect("id"))
        };
        assert!(resolves(
            &lanes,
            &doc["board"]["settings"]["main_swimlane_id"]
        ));
        for task in doc["tasks"].as_array().expect("tasks") {
            assert!(resolves(&columns, &task["column_id"]));
            assert!(resolves(&lanes, &task["swimlane_id"]));
            assert!(resolves(&colors, &task["color_id"]));
            assert!(resolves(&columns, &task["due_target_column_id"]));
            for event in task["history"].as_array().expect("history") {
                assert!(resolves(&columns, &event["column_id"]));
            }
            for link in task["links"].as_array().expect("links") {
                assert!(resolves(&tasks, &link["target_task_id"]));
            }
            for key in task["field_values"].as_object().expect("values").keys() {
                assert!(field_keys.contains(key.as_str()), "{key}");
            }
        }
    }

    #[test]
    fn archive_should_hold_no_token_or_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let files = archive_of(&seeded);
        let json = String::from_utf8(files["board.json"].clone()).expect("utf8");
        for secret in ["feed-secret-value", "calendar_feed_token", "tok-secret-id"] {
            assert!(!json.contains(secret), "{secret} leaked into board.json");
        }
        for key in [
            "token_id",
            "created_by_token",
            "password",
            "passkey",
            "session",
        ] {
            assert!(!json.contains(key), "{key} appears in board.json");
        }
    }

    #[test]
    fn archive_should_name_a_link_to_another_board_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let elsewhere = seeded
            .db
            .create_board(&seeded.account, "Elsewhere")
            .expect("board");
        let column = seeded
            .db
            .create_column(&elsewhere, "Inbox", None)
            .expect("column");
        let far = seeded
            .db
            .create_task(&column, None, "Far task", 1, None)
            .expect("task");
        seeded
            .db
            .add_task_link(&seeded.task, LinkKind::BlockedBy, &far, Actor::SYSTEM)
            .expect("link");
        let doc = document(&archive_of(&seeded));
        let links = doc["tasks"][0]["links"].as_array().expect("links");
        let blocked = links
            .iter()
            .find(|link| link["kind"] == "blocked_by")
            .expect("blocked_by link");
        assert_eq!(blocked["target_task_id"], Value::Null);
        assert_eq!(blocked["target_task_name"], "Far task");
        assert_eq!(blocked["target_board_name"], "Elsewhere");
    }

    #[test]
    fn next_chunk_should_stream_a_large_attachment_in_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let big: Vec<u8> = (0..3 * CHUNK_BYTES + 17)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        seeded
            .db
            .add_attachment(
                &seeded.task,
                "big.bin",
                "application/octet-stream",
                &big,
                Actor::SYSTEM,
            )
            .expect("attachment")
            .expect("task");
        let snapshot = seeded
            .db
            .account(&seeded.account)
            .board_snapshot(&seeded.board)
            .expect("snapshot")
            .expect("owned board");
        let mut archive = Archive::new(snapshot, now()).expect("archive");
        let mut chunks = 0;
        let mut gzipped = Vec::new();
        while let Some(chunk) = archive.next_chunk().expect("chunk") {
            chunks += 1;
            gzipped.extend_from_slice(&chunk);
        }
        assert!(chunks > 1, "one chunk held the whole archive");
        let files = untar(&gzipped);
        let stored = files
            .iter()
            .find(|(path, _)| path.ends_with("/big.bin"))
            .expect("big file");
        assert_eq!(stored.1, &big);
    }

    #[test]
    fn archive_should_carry_a_long_attachment_name_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let name = format!("{}.txt", "n".repeat(200));
        let attachment = seeded
            .db
            .add_attachment(&seeded.task, &name, "text/plain", b"long", Actor::SYSTEM)
            .expect("attachment")
            .expect("task")
            .id;
        let files = archive_of(&seeded);
        assert_eq!(files[&format!("attachments/{attachment}/{name}")], b"long");
    }

    #[test]
    fn board_json_should_keep_the_original_attachment_name_beside_the_archive_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let cases = [
            (".env", "env", b"dotfile".as_slice()),
            (
                "spa ce é日本 file.txt",
                "spa ce é日本 file.txt",
                b"unicode".as_slice(),
            ),
        ];
        let mut ids = Vec::new();
        for (name, _, bytes) in cases {
            let attachment = seeded
                .db
                .add_attachment(&seeded.task, name, "text/plain", bytes, Actor::SYSTEM)
                .expect("attachment")
                .expect("task");
            ids.push(attachment.id);
        }
        let files = archive_of(&seeded);
        let doc = document(&files);
        let attachments = doc["tasks"][0]["attachments"]
            .as_array()
            .expect("attachments");
        for ((name, sanitised, bytes), id) in cases.into_iter().zip(&ids) {
            let entry = attachments
                .iter()
                .find(|attachment| attachment["id"] == id.as_str())
                .expect("attachment entry");
            let path = format!("attachments/{id}/{sanitised}");
            assert_eq!(entry["name"], name);
            assert_eq!(entry["file"], path);
            assert_eq!(files[&path], bytes);
        }
    }

    #[test]
    fn attachment_path_should_keep_the_name_inside_its_directory() {
        assert_eq!(
            attachment_path("a1", "a/b\\c.txt"),
            "attachments/a1/a_b_c.txt"
        );
        assert_eq!(attachment_path("a1", ".."), "attachments/a1/file");
        assert_eq!(attachment_path("a1", ".env"), "attachments/a1/env");
    }

    #[test]
    fn safe_component_should_leave_one_plain_path_component() {
        assert_eq!(safe_component("Work / Plans", "x", 80), "Work _ Plans");
        assert_eq!(safe_component("..", "x", 80), "x");
        assert_eq!(safe_component(" . ", "x", 80), "x");
        assert_eq!(safe_component("a:b*c", "x", 80), "a_b_c");
        assert_eq!(safe_component(&"é".repeat(100), "x", 5), "éé");
    }

    #[test]
    fn download_name_should_end_in_tar_gz() {
        assert_eq!(download_name("My Board"), "My Board.tar.gz");
        assert_eq!(download_name("///"), "___.tar.gz");
        assert_eq!(download_name(""), "board.tar.gz");
    }

    #[test]
    fn archive_database_should_write_one_archive_per_board_of_every_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let second = seeded.db.create_user("Ann/Lee").expect("user");
        let board = seeded.db.create_board(&second, "Home").expect("board");
        let board_id = seeded.board.clone();
        let path = dir.path().join("pomb.redb");
        drop(seeded);
        let out = dir.path().join("out");

        let offline = OfflineDb::open(path.to_str().expect("utf8 path")).expect("open");
        let written = archive_database(&offline, &out, now()).expect("archive");

        let in_dir = |name: &str| fs::read_dir(out.join(name)).expect("dir").count();
        assert_eq!(written, in_dir("chip") + in_dir("Ann_Lee"));
        let first = out
            .join("chip")
            .join(format!("Work _ Plans-{board_id}.tar.gz"));
        let files = untar(&fs::read(first).expect("first archive"));
        assert_eq!(document(&files)["board"]["id"], board_id);
        let second_file = out.join("Ann_Lee").join(format!("Home-{board}.tar.gz"));
        assert!(second_file.is_file());
        assert!(fs::read_dir(out.join("chip")).expect("dir").all(|entry| {
            let name = entry.expect("entry").file_name();
            !name.to_string_lossy().ends_with(".part")
        }));
    }

    #[test]
    fn archive_database_should_leave_the_database_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let path = dir.path().join("pomb.redb");
        drop(seeded);
        let before = fs::read(&path).expect("read db");
        let offline = OfflineDb::open(path.to_str().expect("utf8 path")).expect("open");
        archive_database(&offline, &dir.path().join("out"), now()).expect("archive");
        drop(offline);
        assert!(fs::read(&path).expect("read db") == before);
    }

    #[test]
    fn open_should_refuse_a_file_a_server_holds_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = seed(dir.path());
        let path = dir.path().join("pomb.redb");
        let error = OfflineDb::open(path.to_str().expect("utf8 path")).expect_err("held open");
        assert!(error.to_string().contains("held open"), "{error}");
        drop(seeded);
    }

    #[test]
    fn open_should_refuse_a_file_from_a_newer_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pomb.redb");
        let path = path.to_str().expect("utf8 path");
        drop(seed(dir.path()));
        {
            let raw = redb::Database::create(path).expect("raw open");
            let txn = raw.begin_write().expect("write");
            txn.open_table(redb::TableDefinition::<&str, u64>::new("meta"))
                .expect("meta")
                .insert("schema_version", SCHEMA_VERSION + 1)
                .expect("stamp");
            txn.commit().expect("commit");
        }
        let error = OfflineDb::open(path).expect_err("newer file");
        assert!(error.is::<SchemaTooNew>(), "{error}");
    }
}
