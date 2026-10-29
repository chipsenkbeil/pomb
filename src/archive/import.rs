use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::PathBuf;
use std::{fmt, fs};

use chrono::{DateTime, NaiveDate};
use flate2::read::GzDecoder;
use serde::Serialize;
use serde_json::{Map, Value};
use utoipa::ToSchema;
use uuid::Uuid;

use super::spool::Spool;
use super::tar::{self, Cap, Data, ReadLimits};
use super::{
    AttachmentInfo, BOARD_JSON, BoardDocument, ColorInfo, ColumnInfo, CommentInfo, EventInfo,
    FORMAT, FieldInfo, LaneInfo, LinkInfo, SubtaskInfo, TaskInfo, TimeInfo, attachment_path,
};
use crate::db::{AccountDb, BoardImport, Db, FileImport, MAX_HISTORY_TEXT_CHARS, TaskImport};
use crate::models::{
    AccountId, BreakActivity, ColorRow, ColumnRow, EntryKind, FieldDefRow, FieldKind, LaneLimit,
    LinkKind, MAIN_SWIMLANE_KEY, Quotas, Subtask, SwimlaneRow, TaskAttachment, TaskComment,
    TaskEvent, TaskLink, TaskRow, TextCap, TimeEntryRow, parse_due_time, standard_color,
    valid_field_key,
};
use crate::routes::{
    LANE_HEIGHT_MAX_PX, LANE_HEIGHT_MIN_PX, MAX_COMMENT_AUTHOR_CHARS, MAX_COMMENT_BYTES,
    MAX_COMMENT_KIND_CHARS,
};

/// Largest gzipped archive an import takes, in bytes: 64 MiB.
pub const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

/// Most bytes an archive may unpack to, in bytes: 256 MiB. The import stops reading a gzip stream
/// the moment it passes this, to bound the cost of a small file that unpacks to gigabytes.
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;

/// Largest `board.json`, in bytes: 32 MiB.
const MAX_BOARD_JSON_BYTES: u64 = 32 * 1024 * 1024;

/// Most files an archive may hold, `board.json` included.
const MAX_FILES: usize = 50_000;

/// Longest time entry the import accepts, in seconds: 7 days.
const MAX_ENTRY_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Key of a task's metadata that keeps the handles of assignees the importing account does not
/// have, as text.
const IMPORTED_ASSIGNEES_KEY: &str = "imported_assignees";

/// Why an archive was refused on import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The archive is not a board archive: it is not a gzipped tar, a path or an entry is outside
    /// the format, `board.json` does not parse, or an id or value in it is not valid.
    Invalid(String),
    /// The archive is over a cap on its size, its unpacked size, its files or what a board holds.
    TooLarge(String),
    /// The server could not store the archive's files while it read them.
    Storage(String),
}

impl ImportError {
    /// Returns the refusal of an archive that is not in the format, for `message`.
    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::TooLarge(message) | Self::Storage(message) => {
                f.write_str(message)
            }
        }
    }
}

impl Error for ImportError {}

/// What an import made and what it left out, as the API reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct ImportCounts {
    /// Columns created.
    pub columns: usize,
    /// Swimlanes created.
    pub swimlanes: usize,
    /// Task colors created.
    pub colors: usize,
    /// Custom field definitions created.
    pub custom_fields: usize,
    /// Tasks created.
    pub tasks: usize,
    /// Subtasks created, across all tasks.
    pub subtasks: usize,
    /// Comments created, across all tasks.
    pub comments: usize,
    /// Attachments created, with their files, across all tasks.
    pub attachments: usize,
    /// Links between tasks of the archive created.
    pub links: usize,
    /// Time entries created, across all tasks.
    pub time_entries: usize,
    /// Links to a task outside the archive, which the import dropped.
    pub dropped_links: usize,
    /// Attachments the archive lists with a null `file`, which the import dropped.
    pub dropped_attachments: usize,
    /// Break activities that time entries named and the account does not have, which the import
    /// dropped from those entries.
    pub dropped_break_activities: usize,
    /// Assignees whose handle is the importing account's, which the import kept as assigned.
    pub assignees_matched: usize,
    /// Assignees with any other handle, which the import kept as text in the task's metadata
    /// under `imported_assignees`.
    pub assignees_kept_as_text: usize,
}

/// The account that imports a board, which decides how people and break activities match.
#[derive(Copy, Clone, Debug)]
pub struct Importer<'a> {
    /// The account's handle, which an assignee of the archive matches case-insensitively.
    pub handle: &'a str,
    /// The account's id, which owns everything the import creates.
    pub account: &'a AccountId,
    /// The account's break activities, which a time entry of the archive names.
    pub activities: &'a [BreakActivity],
}

/// Returns the [`Cap`] of the file at `path`, or [`None`] for a path outside the format.
fn file_cap(path: &str) -> Option<Cap> {
    if path == BOARD_JSON {
        return Some(Cap {
            bytes: MAX_BOARD_JSON_BYTES,
            spooled: false,
        });
    }
    let parts: Vec<&str> = path.split('/').collect();
    matches!(parts.as_slice(), ["attachments", _, _]).then_some(Cap {
        bytes: AccountDb::MAX_ATTACHMENT_BYTES,
        spooled: true,
    })
}

/// Reads the gzipped board archive `gzipped` into what a new board of `importer` is made of,
/// every id fresh, and counts what it holds and what it dropped.
///
/// # Errors
///
/// Returns [`ImportError`] when the archive is not a gzipped tar of the board archive format, a
/// path or entry is outside the format, a cap on its size or contents is broken, `board.json`
/// does not parse, an id in it names no row inside it, or a value breaks a text cap or a limit
/// the API enforces, or when the files cannot be spooled. Nothing is read past the first refusal.
///
/// The attachment files are copied to files of `spool` while the archive is read, so the board
/// holds their paths and `spool` must outlive the board.
pub fn read_board(
    gzipped: &[u8],
    importer: &Importer<'_>,
    spool: &Spool,
) -> Result<(BoardImport, ImportCounts), ImportError> {
    let limits = ReadLimits {
        unpacked_bytes: MAX_UNPACKED_BYTES,
        files: MAX_FILES,
    };
    read_within(gzipped, importer, spool, limits)
}

/// Reads an archive like [`read_board`], under the caps on the unpacked size and the file count that `limits` gives.
fn read_within(
    gzipped: &[u8],
    importer: &Importer<'_>,
    spool: &Spool,
    limits: ReadLimits,
) -> Result<(BoardImport, ImportCounts), ImportError> {
    let mut json = None;
    let mut files: HashMap<String, PathBuf> = HashMap::new();
    for file in tar::read_files(GzDecoder::new(gzipped), limits, spool, file_cap)? {
        match file.data {
            Data::Memory(bytes) if file.path == BOARD_JSON => json = Some(bytes),
            Data::Memory(_) => {}
            Data::Spooled(path) => {
                files.insert(file.path, path);
            }
        }
    }
    let json =
        json.ok_or_else(|| ImportError::invalid(format!("the archive holds no {BOARD_JSON}")))?;
    let document: BoardDocument = serde_json::from_slice(&json).map_err(|error| {
        ImportError::invalid(format!(
            "{BOARD_JSON} is not a board archive document: {error}"
        ))
    })?;
    drop(json);
    Builder {
        importer,
        files,
        counts: ImportCounts::default(),
        columns: IdMap::new(),
        lanes: IdMap::new(),
        colors: IdMap::new(),
        tasks: IdMap::new(),
        attachments: HashSet::new(),
        related: HashSet::new(),
    }
    .build(document)
}

/// The fresh id of each row of the archive, by the id the archive gave it.
type IdMap = HashMap<String, String>;
type People = (Vec<String>, Option<Map<String, Value>>);

/// Returns a new id.
fn fresh_id() -> String {
    Uuid::new_v4().to_string()
}

/// Gives the row `old` a fresh id in `map` and returns it, or refuses an id two rows share.
fn register(map: &mut IdMap, old: &str, what: &str) -> Result<String, ImportError> {
    let new = fresh_id();
    if map.insert(old.to_string(), new.clone()).is_some() {
        return Err(ImportError::invalid(format!(
            "two {what}s share the id {old}"
        )));
    }
    Ok(new)
}

/// Returns the fresh id of the row `old`, or refuses an id that names no row of the archive.
fn resolve(map: &IdMap, old: &str, what: &str) -> Result<String, ImportError> {
    map.get(old)
        .cloned()
        .ok_or_else(|| ImportError::invalid(format!("{what} {old} is not a row of the archive")))
}

/// Returns the fresh id of the row `old` like [`resolve`], or [`None`] for an id that is absent.
fn resolve_opt(map: &IdMap, old: Option<&str>, what: &str) -> Result<Option<String>, ImportError> {
    old.map(|old| resolve(map, old, what)).transpose()
}

/// Refuses `text` that is not an RFC 3339 time.
fn instant(text: &str, what: &str) -> Result<(), ImportError> {
    DateTime::parse_from_rfc3339(text)
        .map(drop)
        .map_err(|_| ImportError::invalid(format!("{what} {text:?} is not an RFC 3339 time")))
}

/// Refuses `text` like [`instant`], and accepts a time that is absent.
fn instant_opt(text: Option<&str>, what: &str) -> Result<(), ImportError> {
    text.map_or(Ok(()), |text| instant(text, what))
}

/// Refuses `text` past `cap`.
fn capped(text: &str, cap: TextCap, what: &str) -> Result<(), ImportError> {
    cap.check(text)
        .map_err(|refusal| ImportError::invalid(format!("{what}: {refusal}")))
}

/// Refuses `text` of more than `max` characters.
fn at_most_chars(text: &str, max: usize, what: &str) -> Result<(), ImportError> {
    if text.len() > max && text.chars().count() > max {
        return Err(ImportError::invalid(format!(
            "{what} must be at most {max} characters"
        )));
    }
    Ok(())
}

/// Refuses a list of `count` rows when the most the admin could ever allow is `max`.
fn ceiling(count: usize, max: u32, what: &str) -> Result<(), ImportError> {
    if count > max as usize {
        return Err(ImportError::TooLarge(format!(
            "the archive holds {count} {what}, and at most {max} are accepted"
        )));
    }
    Ok(())
}

/// Removes the keys of a settings bag that hold a secret.
fn without_secrets(bag: &mut Map<String, Value>) {
    bag.retain(|key, _| !key.ends_with("_token") && !key.contains("secret"));
}

/// Returns a settings bag as the JSON a row stores, refusing one past [`TextCap::CONFIG`].
fn bag_json(bag: Map<String, Value>, what: &str) -> Result<String, ImportError> {
    let json = Value::Object(bag).to_string();
    capped(&json, TextCap::CONFIG, what)?;
    Ok(json)
}

/// Returns whether `raw` is a date written exactly as zero-padded `YYYY-MM-DD`.
fn is_plain_date(raw: &str) -> bool {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .is_ok_and(|date| date.format("%Y-%m-%d").to_string() == raw)
}

/// The `blocked_by` and `parent` links the tasks hold, which must stay acyclic and shallow.
#[derive(Default)]
struct LinkGraph {
    /// The tasks each task is blocked by, by task id.
    blocked_by: HashMap<String, Vec<String>>,
    /// The umbrella each child task is part of, by task id.
    parents: HashMap<String, String>,
}

impl LinkGraph {
    /// Refuses a cycle of `blocked_by` links and a task that is both a child and an umbrella.
    fn check(&self) -> Result<(), ImportError> {
        if self
            .parents
            .values()
            .any(|umbrella| self.parents.contains_key(umbrella))
        {
            return Err(ImportError::invalid(
                "a task is both the child of one task and the parent of another",
            ));
        }
        let mut waiting: HashMap<&str, usize> = HashMap::new();
        for (task, blockers) in &self.blocked_by {
            waiting.entry(task.as_str()).or_default();
            for blocker in blockers {
                *waiting.entry(blocker.as_str()).or_default() += 1;
            }
        }
        let mut ready: Vec<&str> = waiting
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(task, _)| *task)
            .collect();
        let mut visited = 0;
        while let Some(task) = ready.pop() {
            visited += 1;
            for blocker in self.blocked_by.get(task).into_iter().flatten() {
                if let Some(count) = waiting.get_mut(blocker.as_str()) {
                    *count -= 1;
                    if *count == 0 {
                        ready.push(blocker.as_str());
                    }
                }
            }
        }
        if visited < waiting.len() {
            return Err(ImportError::invalid(
                "the blocked_by links of the tasks run in a circle",
            ));
        }
        Ok(())
    }
}

/// Builds the rows of a new board from a parsed `board.json`, checking every reference and every
/// limit, and holds what it has resolved so far.
struct Builder<'a> {
    /// The account the board is for.
    importer: &'a Importer<'a>,
    /// The attachment files of the archive that no attachment has claimed yet, by path.
    files: HashMap<String, PathBuf>,
    /// What the import holds so far.
    counts: ImportCounts,
    /// The fresh id of each column.
    columns: IdMap,
    /// The fresh id of each swimlane.
    lanes: IdMap,
    /// The fresh id of each color.
    colors: IdMap,
    /// The fresh id of each task.
    tasks: IdMap,
    /// The ids of the attachments seen so far.
    attachments: HashSet<String>,
    /// The `related` links seen so far, as the pair of fresh task ids in order.
    related: HashSet<(String, String)>,
}

impl Builder<'_> {
    /// Returns the board `document` describes, with its counts.
    fn build(
        mut self,
        document: BoardDocument,
    ) -> Result<(BoardImport, ImportCounts), ImportError> {
        if document.format != FORMAT {
            return Err(ImportError::invalid(format!(
                "{BOARD_JSON} has the format {:?}, not {FORMAT:?}",
                document.format
            )));
        }
        instant(&document.exported_at, "exported_at")?;
        if document.pomb_version.trim().is_empty() {
            return Err(ImportError::invalid("pomb_version is empty"));
        }
        let name = AccountDb::validate_board_name(&document.board.name)
            .map_err(ImportError::invalid)?
            .to_string();
        if document.columns.is_empty() {
            return Err(ImportError::invalid("a board needs at least one column"));
        }
        ceiling(
            document.columns.len(),
            Quotas::MAX_LAYOUT_PER_BOARD,
            "columns",
        )?;
        ceiling(
            document.swimlanes.len(),
            Quotas::MAX_LAYOUT_PER_BOARD,
            "swimlanes",
        )?;
        ceiling(
            document.custom_fields.len(),
            Quotas::MAX_LAYOUT_PER_BOARD,
            "custom fields",
        )?;
        ceiling(document.tasks.len(), Quotas::MAX_TASKS_PER_BOARD, "tasks")?;

        let colors = self.colors(document.colors)?;
        let columns = self.columns(document.columns)?;
        let mut swimlanes = self.swimlanes(document.swimlanes)?;
        let mut settings = self.settings(document.board.settings)?;
        if swimlanes.is_empty() {
            let id = fresh_id();
            settings.insert(MAIN_SWIMLANE_KEY.to_string(), Value::String(id.clone()));
            swimlanes.push(SwimlaneRow {
                id,
                board_id: String::new(),
                name: "Default".to_string(),
                position: 0,
                description: String::new(),
                limits: Vec::new(),
                collapsed: false,
                height: None,
            });
        }
        let fields = self.fields(document.custom_fields)?;
        let tasks = self.tasks(document.tasks, &fields)?;
        if let Some(path) = self.files.keys().min() {
            return Err(ImportError::invalid(format!(
                "{path} belongs to no attachment of any task"
            )));
        }
        self.counts.columns = columns.len();
        self.counts.swimlanes = swimlanes.len();
        self.counts.colors = colors.len();
        self.counts.custom_fields = fields.len();
        self.counts.tasks = tasks.len();
        let import = BoardImport {
            name,
            settings,
            colors,
            columns,
            swimlanes,
            fields,
            tasks,
        };
        Ok((import, self.counts))
    }

    /// Returns the color rows of `colors`.
    fn colors(&mut self, colors: Vec<ColorInfo>) -> Result<Vec<ColorRow>, ImportError> {
        let mut values = HashSet::new();
        let mut defaults = 0;
        let mut rows = Vec::with_capacity(colors.len());
        for color in colors {
            let id = register(&mut self.colors, &color.id, "color")?;
            if standard_color(&color.value).is_none() {
                return Err(ImportError::invalid(format!(
                    "the color {:?} is not in the palette",
                    color.value
                )));
            }
            if !values.insert(color.value.clone()) {
                return Err(ImportError::invalid(format!(
                    "the color {} appears twice",
                    color.value
                )));
            }
            AccountDb::validate_color_label(&color.label)
                .map_err(|why| ImportError::invalid(format!("the color {}: {why}", color.value)))?;
            capped(
                &color.description,
                TextCap::COLOR_DESCRIPTION,
                "color description",
            )?;
            defaults += usize::from(color.is_default);
            rows.push(
                ColorRow {
                    id,
                    board_id: String::new(),
                    value: color.value,
                    label: color.label,
                    description: color.description,
                    background_hex: String::new(),
                    border_hex: String::new(),
                    light_hex: String::new(),
                    enabled: color.enabled,
                    is_default: color.is_default,
                    sort_order: color.sort_order,
                }
                .with_palette_hexes(),
            );
        }
        if defaults > 1 {
            return Err(ImportError::invalid("more than one color is the default"));
        }
        Ok(rows)
    }

    /// Returns the column rows of `columns`.
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> Result<Vec<ColumnRow>, ImportError> {
        let mut rows = Vec::with_capacity(columns.len());
        for column in columns {
            let id = register(&mut self.columns, &column.id, "column")?;
            let name = column.name.trim().to_string();
            if name.is_empty() {
                return Err(ImportError::invalid("a column has no name"));
            }
            capped(&name, TextCap::COLUMN_NAME, "column name")?;
            if column.wip_limit.is_some_and(|limit| limit < 1) {
                return Err(ImportError::invalid(format!(
                    "the column {name:?} has a WIP limit under 1"
                )));
            }
            capped(
                &column.description,
                TextCap::DESCRIPTION,
                "column description",
            )?;
            if column.former_names.len() > ColumnRow::MAX_FORMER_NAMES {
                return Err(ImportError::invalid(format!(
                    "the column {name:?} remembers more than {} former names",
                    ColumnRow::MAX_FORMER_NAMES
                )));
            }
            for former in &column.former_names {
                capped(former, TextCap::COLUMN_NAME, "former column name")?;
            }
            let mut settings = column.settings;
            without_secrets(&mut settings);
            rows.push(ColumnRow {
                id,
                board_id: String::new(),
                name,
                position: column.position,
                wip_limit: column.wip_limit,
                is_done: column.is_done,
                description: column.description,
                collapsed: column.collapsed,
                config_json: bag_json(settings, "column settings")?,
                former_names: column.former_names,
            });
        }
        Ok(rows)
    }

    /// Returns the swimlane rows of `lanes`.
    fn swimlanes(&mut self, lanes: Vec<LaneInfo>) -> Result<Vec<SwimlaneRow>, ImportError> {
        let mut rows = Vec::with_capacity(lanes.len());
        for lane in lanes {
            let id = register(&mut self.lanes, &lane.id, "swimlane")?;
            let name = lane.name.trim().to_string();
            if name.is_empty() {
                return Err(ImportError::invalid("a swimlane has no name"));
            }
            capped(&name, TextCap::SWIMLANE_NAME, "swimlane name")?;
            capped(
                &lane.description,
                TextCap::SWIMLANE_DESCRIPTION,
                "swimlane description",
            )?;
            if let Some(height) = lane.height
                && !(LANE_HEIGHT_MIN_PX..=LANE_HEIGHT_MAX_PX).contains(&height)
            {
                return Err(ImportError::invalid(format!(
                    "the swimlane {name:?} is not from {LANE_HEIGHT_MIN_PX} to \
                     {LANE_HEIGHT_MAX_PX} pixels high"
                )));
            }
            let mut limited = HashSet::new();
            let mut limits = Vec::with_capacity(lane.limits.len());
            for limit in lane.limits {
                if limit.max < 1 {
                    return Err(ImportError::invalid(format!(
                        "the swimlane {name:?} has a limit under 1"
                    )));
                }
                let column_id = resolve(
                    &self.columns,
                    &limit.column_id,
                    "the column of a swimlane limit",
                )?;
                if !limited.insert(column_id.clone()) {
                    return Err(ImportError::invalid(format!(
                        "the swimlane {name:?} has two limits for one column"
                    )));
                }
                limits.push(LaneLimit {
                    column_id,
                    max: limit.max,
                });
            }
            rows.push(SwimlaneRow {
                id,
                board_id: String::new(),
                name,
                position: lane.position,
                description: lane.description,
                limits,
                collapsed: lane.collapsed,
                height: lane.height,
            });
        }
        Ok(rows)
    }

    /// Returns the settings bag of the board, with its main swimlane remapped and without the
    /// archived flag and every secret.
    fn settings(&self, mut bag: Map<String, Value>) -> Result<Map<String, Value>, ImportError> {
        bag.remove("archived");
        without_secrets(&mut bag);
        let main =
            bag.get(MAIN_SWIMLANE_KEY)
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        ImportError::invalid(format!("{MAIN_SWIMLANE_KEY} is not text"))
                    })
                })
                .transpose()?;
        if let Some(old) = main {
            let lane = resolve(&self.lanes, &old, "the main swimlane")?;
            bag.insert(MAIN_SWIMLANE_KEY.to_string(), Value::String(lane));
        }
        if let Some(due_time) = bag.get("due_time")
            && !due_time
                .as_str()
                .is_some_and(|raw| parse_due_time(raw).is_some())
        {
            return Err(ImportError::invalid("due_time is not a time written HH:MM"));
        }
        capped(
            &Value::Object(bag.clone()).to_string(),
            TextCap::CONFIG,
            "board settings",
        )?;
        Ok(bag)
    }

    /// Returns the field definition rows of `fields`.
    fn fields(&mut self, fields: Vec<FieldInfo>) -> Result<Vec<FieldDefRow>, ImportError> {
        let mut keys = HashSet::new();
        let mut rows = Vec::with_capacity(fields.len());
        for field in fields {
            let key = field.key;
            if !valid_field_key(&key) {
                return Err(ImportError::invalid(format!(
                    "{key:?} is not a valid custom field key"
                )));
            }
            if !keys.insert(key.clone()) {
                return Err(ImportError::invalid(format!(
                    "the custom field {key} appears twice"
                )));
            }
            instant(&field.created_at, "the creation time of a custom field")?;
            if field.kind != FieldKind::Table && !field.columns.is_empty() {
                return Err(ImportError::invalid(format!(
                    "the custom field {key} has columns but is not a table"
                )));
            }
            let mut column_keys = HashSet::new();
            for column in &field.columns {
                if !valid_field_key(&column.key)
                    || !column.kind.is_column_kind()
                    || !column_keys.insert(column.key.as_str())
                {
                    return Err(ImportError::invalid(format!(
                        "the custom field {key} has a column {:?} that is repeated or not valid",
                        column.key
                    )));
                }
            }
            let mut row = FieldDefRow {
                id: fresh_id(),
                board_id: String::new(),
                key,
                label: field.label,
                kind: field.kind,
                options: field.options,
                columns: field.columns,
                default: None,
                required: field.required,
                owner: field.owner,
                position: field.position,
                created_at: field.created_at,
            };
            row.check_caps().map_err(|refusal| {
                ImportError::invalid(format!("the custom field {}: {refusal}", row.key))
            })?;
            row.default = field
                .default_value
                .as_ref()
                .map(|value| row.check_value(value))
                .transpose()
                .map_err(|why| {
                    ImportError::invalid(format!(
                        "the default of the custom field {}: {why}",
                        row.key
                    ))
                })?;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Returns every task of `tasks` with its attachments' files and time entries.
    fn tasks(
        &mut self,
        tasks: Vec<TaskInfo>,
        fields: &[FieldDefRow],
    ) -> Result<Vec<TaskImport>, ImportError> {
        for task in &tasks {
            register(&mut self.tasks, &task.id, "task")?;
        }
        let mut graph = LinkGraph::default();
        let mut imported = Vec::with_capacity(tasks.len());
        for task in tasks {
            imported.push(self.task(task, fields, &mut graph)?);
        }
        graph.check()?;
        Ok(imported)
    }

    /// Returns the task `task` with its attachments' files and time entries.
    fn task(
        &mut self,
        task: TaskInfo,
        fields: &[FieldDefRow],
        graph: &mut LinkGraph,
    ) -> Result<TaskImport, ImportError> {
        let id = resolve(&self.tasks, &task.id, "the task")?;
        let label = format!("the task {:?}", task.name);
        if task.name.trim().is_empty() {
            return Err(ImportError::invalid("a task has no name"));
        }
        if !task.position.is_finite() {
            return Err(ImportError::invalid(format!("{label} has no position")));
        }
        if task
            .estimate_hours
            .is_some_and(|hours| !hours.is_finite() || hours <= 0.0)
        {
            return Err(ImportError::invalid(format!(
                "{label} has an estimate that is not a positive number of hours"
            )));
        }
        for (text, what) in [
            (Some(task.created_at.as_str()), "created_at"),
            (task.updated_at.as_deref(), "updated_at"),
            (task.completed_at.as_deref(), "completed_at"),
            (task.column_entered_at.as_deref(), "column_entered_at"),
            (task.due_at.as_deref(), "due_at"),
        ] {
            instant_opt(text, &format!("{what} of {label}"))?;
        }
        if task
            .grouping_date
            .as_deref()
            .is_some_and(|raw| !is_plain_date(raw))
        {
            return Err(ImportError::invalid(format!(
                "{label} has a grouping_date that is not written YYYY-MM-DD"
            )));
        }
        let column_id = resolve(&self.columns, &task.column_id, "the column of a task")?;
        let swimlane_id = resolve_opt(
            &self.lanes,
            task.swimlane_id.as_deref(),
            "the swimlane of a task",
        )?;
        let color_id = resolve_opt(
            &self.colors,
            task.color_id.as_deref(),
            "the color of a task",
        )?;
        let due_target_column_id = resolve_opt(
            &self.columns,
            task.due_target_column_id.as_deref(),
            "the due target column of a task",
        )?;

        let subtasks = self.subtasks(&label, task.subtasks)?;
        let comments = self.comments(&label, task.comments)?;
        let (attachments, files) = self.attachments(&label, task.attachments)?;
        let links = self.links(&label, &id, task.links, graph)?;
        let history = self.history(&label, task.history)?;
        let entries = self.entries(&label, &id, task.time_entries)?;
        let fields = field_values(&label, task.field_values, fields)?;
        let (member_ids, metadata) = self.people(&label, task.assignees, task.metadata)?;

        let row = TaskRow {
            id,
            column_id,
            swimlane_id,
            name: task.name,
            description: task.description,
            size: task.size_pomodori,
            position: task.position,
            created_at: task.created_at,
            completed_at: task.completed_at,
            total_minutes: 0,
            pomodori_completed: task.pomodori_completed,
            interruptions: task.interruptions,
            color_id,
            subtasks,
            member_ids,
            grouping_date: task.grouping_date,
            watched: task.watching,
            labels: task.labels,
            due_at: task.due_at,
            estimate_hours: task.estimate_hours,
            due_repeat: task.due_repeat,
            due_done: task.due_done,
            due_target_column_id,
            column_entered_at: task.column_entered_at,
            comments,
            attachments,
            history,
            updated_at: task.updated_at,
            metadata,
            links,
            claim: None,
            claim_fence: 0,
            fields,
        };
        row.check_caps(None)
            .map_err(|refusal| ImportError::invalid(format!("{label}: {refusal}")))?;
        for entry in &entries {
            entry.check_caps(None).map_err(|refusal| {
                ImportError::invalid(format!("a time entry of {label}: {refusal}"))
            })?;
        }
        Ok(TaskImport {
            row,
            files,
            entries,
        })
    }

    /// Returns the checklist of a task.
    fn subtasks(
        &mut self,
        label: &str,
        subtasks: Vec<SubtaskInfo>,
    ) -> Result<Vec<Subtask>, ImportError> {
        ceiling(
            subtasks.len(),
            Quotas::MAX_SUBTASKS_PER_TASK,
            &format!("subtasks on {label}"),
        )?;
        self.counts.subtasks += subtasks.len();
        subtasks
            .into_iter()
            .map(|subtask| {
                instant_opt(
                    subtask.due_at.as_deref(),
                    &format!("due_at of a subtask of {label}"),
                )?;
                Ok::<_, ImportError>(Subtask {
                    id: fresh_id(),
                    name: subtask.name,
                    done: subtask.done,
                    due_at: subtask.due_at,
                })
            })
            .collect()
    }

    /// Returns the comments of a task.
    fn comments(
        &mut self,
        label: &str,
        comments: Vec<CommentInfo>,
    ) -> Result<Vec<TaskComment>, ImportError> {
        ceiling(
            comments.len(),
            Quotas::MAX_COMMENTS_PER_TASK,
            &format!("comments on {label}"),
        )?;
        self.counts.comments += comments.len();
        comments
            .into_iter()
            .map(|comment| {
                if comment.body.trim().is_empty() {
                    return Err(ImportError::invalid(format!(
                        "a comment on {label} has no text"
                    )));
                }
                if comment.body.len() > MAX_COMMENT_BYTES {
                    return Err(ImportError::invalid(format!(
                        "a comment on {label} is longer than {MAX_COMMENT_BYTES} bytes"
                    )));
                }
                if comment.author.trim().is_empty() {
                    return Err(ImportError::invalid(format!(
                        "a comment on {label} has no author"
                    )));
                }
                at_most_chars(
                    &comment.author,
                    MAX_COMMENT_AUTHOR_CHARS,
                    "a comment author",
                )?;
                if let Some(kind) = &comment.kind {
                    at_most_chars(kind, MAX_COMMENT_KIND_CHARS, "a comment kind")?;
                }
                if let Some(name) = &comment.token_name {
                    at_most_chars(name, MAX_HISTORY_TEXT_CHARS, "a token name")?;
                }
                instant(
                    &comment.created_at,
                    &format!("created_at of a comment on {label}"),
                )?;
                instant_opt(
                    comment.edited_at.as_deref(),
                    &format!("edited_at of a comment on {label}"),
                )?;
                Ok(TaskComment {
                    id: fresh_id(),
                    author: comment.author,
                    body: comment.body,
                    created_at: comment.created_at,
                    actor_kind: comment.actor_kind,
                    token_name: comment.token_name,
                    token_id: None,
                    kind: comment.kind,
                    edited_at: comment.edited_at,
                })
            })
            .collect()
    }

    /// Returns the attachments of a task whose file the archive holds, with those files.
    fn attachments(
        &mut self,
        label: &str,
        attachments: Vec<AttachmentInfo>,
    ) -> Result<(Vec<TaskAttachment>, Vec<FileImport>), ImportError> {
        ceiling(
            attachments.len(),
            Quotas::MAX_ATTACHMENTS_PER_TASK,
            &format!("attachments on {label}"),
        )?;
        let mut kept = Vec::new();
        let mut files = Vec::new();
        for attachment in attachments {
            if !self.attachments.insert(attachment.id.clone()) {
                return Err(ImportError::invalid(format!(
                    "two attachments share the id {}",
                    attachment.id
                )));
            }
            let Some(path) = attachment.file else {
                self.counts.dropped_attachments += 1;
                continue;
            };
            if path != attachment_path(&attachment.id, &attachment.name) {
                return Err(ImportError::invalid(format!(
                    "the file {path} is not where the attachment {} of {label} belongs",
                    attachment.name
                )));
            }
            let spooled = self.files.remove(&path).ok_or_else(|| {
                ImportError::invalid(format!(
                    "the attachment {} of {label} names the file {path}, which is not in the archive",
                    attachment.name
                ))
            })?;
            let size = fs::metadata(&spooled)
                .map_err(|error| ImportError::Storage(error.to_string()))?
                .len();
            if size != attachment.size {
                return Err(ImportError::invalid(format!(
                    "the attachment {} of {label} says {} bytes, and its file holds {}",
                    attachment.name, attachment.size, size
                )));
            }
            if attachment.name.trim().is_empty() {
                return Err(ImportError::invalid(format!(
                    "an attachment of {label} has no name"
                )));
            }
            capped(
                &attachment.name,
                TextCap::ATTACHMENT_NAME,
                "attachment name",
            )?;
            capped(
                &attachment.mime,
                TextCap::ATTACHMENT_MIME,
                "attachment mime",
            )?;
            at_most_chars(
                &attachment.uploaded_by,
                MAX_HISTORY_TEXT_CHARS,
                "uploaded_by",
            )?;
            instant(
                &attachment.created_at,
                &format!("created_at of an attachment of {label}"),
            )?;
            let id = fresh_id();
            files.push(FileImport {
                id: id.clone(),
                path: spooled,
                size,
            });
            kept.push(TaskAttachment {
                id,
                name: attachment.name,
                mime: attachment.mime,
                size: attachment.size,
                uploaded_by: attachment.uploaded_by,
                created_at: attachment.created_at,
            });
        }
        self.counts.attachments += kept.len();
        Ok((kept, files))
    }

    /// Returns the links task `holder` holds to other tasks of the archive, and counts the ones it
    /// holds to tasks outside it as dropped.
    fn links(
        &mut self,
        label: &str,
        holder: &str,
        links: Vec<LinkInfo>,
        graph: &mut LinkGraph,
    ) -> Result<Vec<TaskLink>, ImportError> {
        let mut kept: Vec<TaskLink> = Vec::new();
        for link in links {
            if !matches!(
                link.kind,
                LinkKind::BlockedBy | LinkKind::Related | LinkKind::Parent
            ) {
                return Err(ImportError::invalid(format!(
                    "{label} holds a {:?} link, which a task never holds",
                    link.kind
                )));
            }
            instant(
                &link.created_at,
                &format!("created_at of a link of {label}"),
            )?;
            let Some(old) = link.target_task_id else {
                self.counts.dropped_links += 1;
                continue;
            };
            let target = resolve(&self.tasks, &old, "the task a link names")?;
            if target == holder {
                return Err(ImportError::invalid(format!("{label} links to itself")));
            }
            let again = kept
                .iter()
                .any(|held| held.kind == link.kind && held.target_id == target);
            let pair = if holder < target.as_str() {
                (holder.to_string(), target.clone())
            } else {
                (target.clone(), holder.to_string())
            };
            if again || (link.kind == LinkKind::Related && !self.related.insert(pair)) {
                return Err(ImportError::invalid(format!(
                    "{label} holds the same link twice"
                )));
            }
            match link.kind {
                LinkKind::BlockedBy => graph
                    .blocked_by
                    .entry(holder.to_string())
                    .or_default()
                    .push(target.clone()),
                LinkKind::Parent
                    if graph
                        .parents
                        .insert(holder.to_string(), target.clone())
                        .is_some() =>
                {
                    return Err(ImportError::invalid(format!("{label} has two parents")));
                }
                _ => {}
            }
            kept.push(TaskLink {
                id: fresh_id(),
                kind: link.kind,
                target_id: target,
                created_at: link.created_at,
                board_id: None,
            });
        }
        self.counts.links += kept.len();
        Ok(kept)
    }

    /// Returns the history of a task.
    fn history(&self, label: &str, history: Vec<EventInfo>) -> Result<Vec<TaskEvent>, ImportError> {
        if history.len() > Db::MAX_HISTORY_EVENTS {
            return Err(ImportError::TooLarge(format!(
                "{label} holds {} history events, and at most {} are accepted",
                history.len(),
                Db::MAX_HISTORY_EVENTS
            )));
        }
        history
            .into_iter()
            .map(|event| {
                for (text, what) in [
                    (Some(event.kind.as_str()), "an event kind"),
                    (Some(event.detail.as_str()), "an event detail"),
                    (Some(event.actor.as_str()), "an event actor"),
                    (event.token_name.as_deref(), "an event token name"),
                ] {
                    if let Some(text) = text {
                        at_most_chars(text, MAX_HISTORY_TEXT_CHARS, what)?;
                    }
                }
                instant(
                    &event.created_at,
                    &format!("created_at of an event of {label}"),
                )?;
                Ok::<_, ImportError>(TaskEvent {
                    id: fresh_id(),
                    kind: event.kind,
                    detail: event.detail,
                    actor: event.actor,
                    created_at: event.created_at,
                    column_id: resolve_opt(
                        &self.columns,
                        event.column_id.as_deref(),
                        "the column of an event",
                    )?,
                    token_name: event.token_name,
                })
            })
            .collect()
    }

    /// Returns the time entries of task `task_id`.
    fn entries(
        &mut self,
        label: &str,
        task_id: &str,
        entries: Vec<TimeInfo>,
    ) -> Result<Vec<TimeEntryRow>, ImportError> {
        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries {
            self.counts.time_entries += 1;
            ceiling(
                self.counts.time_entries,
                Quotas::MAX_TIME_ENTRIES_PER_ACCOUNT,
                "time entries",
            )?;
            let kind = EntryKind::parse(&entry.kind).ok_or_else(|| {
                ImportError::invalid(format!(
                    "a time entry of {label} has the kind {:?}, which no entry has",
                    entry.kind
                ))
            })?;
            instant(
                &entry.started_at,
                &format!("started_at of a time entry of {label}"),
            )?;
            if !(0..=MAX_ENTRY_SECONDS / 60).contains(&entry.minutes)
                || !(0..=MAX_ENTRY_SECONDS).contains(&entry.seconds)
            {
                return Err(ImportError::invalid(format!(
                    "a time entry of {label} is longer than {MAX_ENTRY_SECONDS} seconds or negative"
                )));
            }
            at_most_chars(&entry.created_by, MAX_HISTORY_TEXT_CHARS, "created_by")?;
            let activity_id = match entry.break_activity.as_deref() {
                None => None,
                Some(name) => {
                    let found = self
                        .importer
                        .activities
                        .iter()
                        .find(|activity| activity.name == name);
                    if found.is_none() {
                        self.counts.dropped_break_activities += 1;
                    }
                    found.map(|activity| activity.id.clone())
                }
            };
            rows.push(TimeEntryRow {
                id: fresh_id(),
                task_id: task_id.to_string(),
                minutes: entry.minutes,
                seconds: entry.seconds,
                note: entry.note,
                started_at: entry.started_at,
                kind: kind.as_str().to_string(),
                interrupted: entry.interrupted,
                interrupt_reason: entry.interrupt_reason,
                labels: entry.labels,
                created_by: entry.created_by,
                activity_id,
                account_id: self.importer.account.as_str().to_string(),
            });
        }
        Ok(rows)
    }

    /// Returns the ids of the accounts a task is assigned to, which is the importing account when
    /// its handle is among the assignees, and the task's metadata with the other handles kept as
    /// text.
    fn people(
        &mut self,
        label: &str,
        assignees: Vec<String>,
        metadata: Option<Map<String, Value>>,
    ) -> Result<People, ImportError> {
        let mut member_ids = Vec::new();
        let mut others: Vec<String> = Vec::new();
        for handle in assignees {
            if handle.eq_ignore_ascii_case(self.importer.handle) {
                if member_ids.is_empty() {
                    member_ids.push(self.importer.account.as_str().to_string());
                    self.counts.assignees_matched += 1;
                }
            } else if !others.contains(&handle) {
                others.push(handle);
            }
        }
        let mut bag = metadata.unwrap_or_default();
        if !others.is_empty() {
            let mut names = match bag.remove(IMPORTED_ASSIGNEES_KEY) {
                None => Vec::new(),
                Some(Value::Array(items)) => items,
                Some(_) => {
                    return Err(ImportError::invalid(format!(
                        "the metadata of {label} holds {IMPORTED_ASSIGNEES_KEY}, which is not a list"
                    )));
                }
            };
            for handle in others {
                let name = Value::String(handle);
                if !names.contains(&name) {
                    names.push(name);
                    self.counts.assignees_kept_as_text += 1;
                }
            }
            bag.insert(IMPORTED_ASSIGNEES_KEY.to_string(), Value::Array(names));
        }
        let size = serde_json::to_vec(&bag).map_or(usize::MAX, |raw| raw.len());
        if size > AccountDb::MAX_METADATA_BYTES {
            return Err(ImportError::invalid(format!(
                "the metadata of {label} is over {} bytes",
                AccountDb::MAX_METADATA_BYTES
            )));
        }
        Ok((member_ids, Some(bag).filter(|bag| !bag.is_empty())))
    }
}

/// Returns the custom field values of a task, each checked against its definition in `defs`.
fn field_values(
    label: &str,
    values: Map<String, Value>,
    defs: &[FieldDefRow],
) -> Result<Map<String, Value>, ImportError> {
    let mut stored = Map::new();
    for (key, value) in values {
        let def = defs.iter().find(|def| def.key == key).ok_or_else(|| {
            ImportError::invalid(format!("{label} has a value for the undefined field {key}"))
        })?;
        let value = match def.check_value(&value) {
            Ok(checked) => checked,
            Err(_) if is_stale_choice(def.kind, &value) => value,
            Err(why) => {
                return Err(ImportError::invalid(format!(
                    "{label} has a bad value for the field {key}: {why}"
                )));
            }
        };
        stored.insert(key, value);
    }
    Ok(stored)
}

/// Returns whether `value` is a choice of a `select` or `multiselect` field that the field's
/// options no longer list, which a board keeps on a task after an option goes away.
fn is_stale_choice(kind: FieldKind, value: &Value) -> bool {
    let choice = |text: &Value| {
        text.as_str()
            .is_some_and(|text| TextCap::FIELD_OPTION.check(text).is_ok())
    };
    match (kind, value) {
        (FieldKind::Select, text) => choice(text),
        (FieldKind::Multiselect, Value::Array(items)) => {
            TextCap::FIELD_OPTIONS.check_count(items).is_ok() && items.iter().all(choice)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::{DateTime, TimeZone, Utc};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::archive::Archive;
    use crate::db::{ColumnKind, NewComment};
    use crate::models::{Actor, ActorKind, FieldOwner, InstanceMode};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap()
    }

    fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, data) in files {
            out.extend(tar::entry_header(path, data.len() as u64, 0).expect("header"));
            out.extend_from_slice(data);
            out.extend_from_slice(&tar::ZEROS[..tar::padding(data.len() as u64)]);
        }
        out.extend_from_slice(&tar::ZEROS);
        out
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).expect("write");
        encoder.finish().expect("finish")
    }

    fn archive_of(document: &Value, files: &[(&str, &[u8])]) -> Vec<u8> {
        let json = document.to_string();
        let mut all: Vec<(&str, &[u8])> = vec![(BOARD_JSON, json.as_bytes())];
        all.extend_from_slice(files);
        gzip(&tar_of(&all))
    }

    fn task(id: &str, name: &str) -> Value {
        json!({
            "id": id, "name": name, "description": "", "column_id": "c1", "swimlane_id": null,
            "color_id": null, "position": 0.0, "size_pomodori": 1,
            "created_at": "2026-10-01T10:00:00Z", "updated_at": null, "completed_at": null,
            "column_entered_at": null, "grouping_date": null, "labels": [], "assignees": [],
            "watching": false, "due_at": null, "due_repeat": null, "due_done": false,
            "due_target_column_id": null, "estimate_hours": null, "pomodori_completed": 0,
            "interruptions": 0, "subtasks": [], "comments": [], "attachments": [], "links": [],
            "history": [], "claim": null, "claim_fence": 0, "metadata": null,
            "field_values": {}, "time_entries": []
        })
    }

    fn lane(id: &str) -> Value {
        json!({
            "id": id, "name": id, "position": 0, "description": "", "limits": [],
            "collapsed": false, "height": null
        })
    }

    fn base() -> Value {
        json!({
            "format": "pomb-board-archive",
            "pomb_version": "0.0.0",
            "exported_at": "2026-10-29T12:00:00Z",
            "board": {"id": "b1", "name": "Imported", "position": 0, "owner": "chip", "settings": {}},
            "columns": [{
                "id": "c1", "name": "Todo", "position": 0, "wip_limit": null, "is_done": false,
                "description": "", "collapsed": false, "former_names": [], "settings": {}
            }],
            "swimlanes": [lane("s1")],
            "colors": [],
            "custom_fields": [],
            "tasks": [task("t1", "One")]
        })
    }

    fn link(id: &str, kind: &str, target: Option<&str>) -> Value {
        json!({
            "id": id, "kind": kind, "target_task_id": target, "target_task_name": null,
            "target_board_name": null, "created_at": "2026-10-02T10:00:00Z"
        })
    }

    fn entry(id: &str, started_at: &str) -> Value {
        json!({
            "id": id, "started_at": started_at, "minutes": 5, "seconds": 300, "kind": "manual",
            "note": "", "interrupted": false, "interrupt_reason": null, "labels": [],
            "created_by": "chip", "break_activity": null
        })
    }

    fn read_in(
        document: &Value,
        files: &[(&str, &[u8])],
        activities: &[BreakActivity],
        spool: &Spool,
    ) -> Result<(BoardImport, ImportCounts), ImportError> {
        let account = AccountId::new("acc");
        let importer = Importer {
            handle: "chip",
            account: &account,
            activities,
        };
        read_board(&archive_of(document, files), &importer, spool)
    }

    fn read_as(
        document: &Value,
        files: &[(&str, &[u8])],
        activities: &[BreakActivity],
    ) -> Result<(BoardImport, ImportCounts), ImportError> {
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        read_in(document, files, activities, &spool)
    }

    fn read(document: &Value) -> Result<(BoardImport, ImportCounts), ImportError> {
        read_as(document, &[], &[])
    }

    fn read_bytes(gzipped: &[u8]) -> Result<(BoardImport, ImportCounts), ImportError> {
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let account = AccountId::new("acc");
        let importer = Importer {
            handle: "chip",
            account: &account,
            activities: &[],
        };
        read_board(gzipped, &importer, &spool)
    }

    #[test]
    fn read_board_should_read_a_board_with_fresh_ids() {
        let (import, counts) = read(&base()).expect("a board");
        assert_eq!(import.name, "Imported");
        assert_eq!((counts.columns, counts.tasks), (1, 1));
        assert_ne!(import.columns[0].id, "c1");
        assert_ne!(import.tasks[0].row.id, "t1");
        assert_eq!(import.tasks[0].row.column_id, import.columns[0].id);
    }

    type Edit = Box<dyn Fn(&mut Value)>;

    fn format_breaks() -> Vec<(&'static str, Edit, &'static str)> {
        let long = "n".repeat(501);
        vec![
            (
                "format",
                Box::new(|d: &mut Value| d["format"] = json!("other")),
                "format",
            ),
            (
                "exported_at",
                Box::new(|d: &mut Value| d["exported_at"] = json!("soon")),
                "exported_at",
            ),
            (
                "unknown field",
                Box::new(|d: &mut Value| d["extra"] = json!(1)),
                "unknown field",
            ),
            (
                "board name",
                Box::new(|d: &mut Value| d["board"]["name"] = json!("  ")),
                "Board name",
            ),
            (
                "no columns",
                Box::new(|d: &mut Value| d["columns"] = json!([])),
                "at least one column",
            ),
            (
                "column wip",
                Box::new(|d: &mut Value| d["columns"][0]["wip_limit"] = json!(0)),
                "WIP limit",
            ),
            (
                "column name",
                Box::new(|d: &mut Value| d["columns"][0]["name"] = json!("c".repeat(201))),
                "column name",
            ),
            (
                "task column",
                Box::new(|d: &mut Value| d["tasks"][0]["column_id"] = json!("nope")),
                "is not a row of the archive",
            ),
            (
                "task lane",
                Box::new(|d: &mut Value| d["tasks"][0]["swimlane_id"] = json!("nope")),
                "is not a row of the archive",
            ),
            (
                "task color",
                Box::new(|d: &mut Value| d["tasks"][0]["color_id"] = json!("nope")),
                "is not a row of the archive",
            ),
            (
                "due target",
                Box::new(|d: &mut Value| d["tasks"][0]["due_target_column_id"] = json!("nope")),
                "is not a row of the archive",
            ),
            (
                "main swimlane",
                Box::new(|d: &mut Value| d["board"]["settings"]["main_swimlane_id"] = json!("x")),
                "main swimlane",
            ),
            (
                "duplicate task",
                Box::new(|d: &mut Value| d["tasks"] = json!([task("t1", "A"), task("t1", "B")])),
                "share the id",
            ),
            (
                "bad time",
                Box::new(|d: &mut Value| d["tasks"][0]["created_at"] = json!("yesterday")),
                "RFC 3339",
            ),
            (
                "bad date",
                Box::new(|d: &mut Value| d["tasks"][0]["grouping_date"] = json!("10/29/2026")),
                "YYYY-MM-DD",
            ),
            (
                "task name cap",
                Box::new(move |d: &mut Value| d["tasks"][0]["name"] = json!(long.clone())),
                "name must be at most 500",
            ),
            (
                "blank task",
                Box::new(|d: &mut Value| d["tasks"][0]["name"] = json!(" ")),
                "has no name",
            ),
            (
                "estimate",
                Box::new(|d: &mut Value| d["tasks"][0]["estimate_hours"] = json!(-1.0)),
                "estimate",
            ),
            (
                "history column",
                Box::new(|d: &mut Value| {
                    d["tasks"][0]["history"] = json!([{
                        "id": "e1", "kind": "moved", "detail": "x", "actor": "chip",
                        "created_at": "2026-10-01T10:00:00Z", "column_id": "nope", "token_name": null
                    }]);
                }),
                "is not a row of the archive",
            ),
            (
                "color value",
                Box::new(|d: &mut Value| {
                    d["colors"] = json!([{
                        "id": "k1", "value": "chartreuse", "label": "x", "description": "",
                        "background_hex": "", "border_hex": "", "light_hex": "", "enabled": true,
                        "is_default": true, "sort_order": 1
                    }]);
                }),
                "not in the palette",
            ),
            (
                "comment size",
                Box::new(|d: &mut Value| {
                    d["tasks"][0]["comments"] = json!([{
                        "id": "m1", "author": "chip", "body": "x".repeat(5001),
                        "created_at": "2026-10-01T10:00:00Z", "actor_kind": "user",
                        "token_name": null, "kind": null, "edited_at": null
                    }]);
                }),
                "longer than 5000 bytes",
            ),
            (
                "entry kind",
                Box::new(|d: &mut Value| {
                    let mut nap = entry("n1", "2026-10-01T10:00:00Z");
                    nap["kind"] = json!("nap");
                    d["tasks"][0]["time_entries"] = json!([nap]);
                }),
                "which no entry has",
            ),
            (
                "entry length",
                Box::new(|d: &mut Value| {
                    let mut long = entry("n1", "2026-10-01T10:00:00Z");
                    long["minutes"] = json!(-1);
                    d["tasks"][0]["time_entries"] = json!([long]);
                }),
                "negative",
            ),
            (
                "field value",
                Box::new(|d: &mut Value| d["tasks"][0]["field_values"] = json!({"ghost": 1})),
                "undefined field",
            ),
            (
                "link target",
                Box::new(|d: &mut Value| {
                    d["tasks"][0]["links"] = json!([link("l1", "related", Some("ghost"))]);
                }),
                "is not a row of the archive",
            ),
            (
                "self link",
                Box::new(|d: &mut Value| {
                    d["tasks"][0]["links"] = json!([link("l1", "related", Some("t1"))]);
                }),
                "links to itself",
            ),
            (
                "link kind",
                Box::new(|d: &mut Value| {
                    d["tasks"][0]["links"] = json!([link("l1", "blocks", None)]);
                }),
                "never holds",
            ),
            (
                "attachment path",
                Box::new(|d: &mut Value| {
                    d["tasks"][0]["attachments"] = json!([{
                        "id": "a1", "name": "n.txt", "mime": "text/plain", "size": 1,
                        "uploaded_by": "chip", "created_at": "2026-10-01T10:00:00Z",
                        "file": "attachments/zzz/n.txt"
                    }]);
                }),
                "is not where the attachment",
            ),
            (
                "attachment file",
                Box::new(|d: &mut Value| {
                    d["tasks"][0]["attachments"] = json!([{
                        "id": "a1", "name": "n.txt", "mime": "text/plain", "size": 1,
                        "uploaded_by": "chip", "created_at": "2026-10-01T10:00:00Z",
                        "file": "attachments/a1/n.txt"
                    }]);
                }),
                "is not in the archive",
            ),
        ]
    }

    #[test]
    fn read_board_should_refuse_a_document_that_breaks_the_format() {
        for (label, edit, expected) in format_breaks() {
            let mut document = base();
            edit(&mut document);
            let error = read(&document).expect_err(label);
            assert!(matches!(error, ImportError::Invalid(_)), "{label}: {error}");
            assert!(error.to_string().contains(expected), "{label}: {error}");
        }
    }

    #[test]
    fn read_board_should_refuse_links_that_run_in_a_circle_or_nest_too_deep() {
        let two = |first: Vec<Value>, second: Vec<Value>| {
            let mut document = base();
            let mut a = task("t1", "A");
            let mut b = task("t2", "B");
            a["links"] = json!(first);
            b["links"] = json!(second);
            document["tasks"] = json!([a, b]);
            document
        };
        let cycle = two(
            vec![link("l1", "blocked_by", Some("t2"))],
            vec![link("l2", "blocked_by", Some("t1"))],
        );
        let error = read(&cycle).expect_err("a cycle");
        assert!(error.to_string().contains("in a circle"), "{error}");
        let nested = two(
            vec![link("l1", "parent", Some("t2"))],
            vec![link("l2", "parent", Some("t1"))],
        );
        let error = read(&nested).expect_err("nested");
        assert!(error.to_string().contains("child"), "{error}");
        let again = two(
            vec![
                link("l1", "related", Some("t2")),
                link("l3", "related", Some("t2")),
            ],
            vec![],
        );
        let error = read(&again).expect_err("twice");
        assert!(error.to_string().contains("twice"), "{error}");
        let reciprocal = two(
            vec![link("l1", "related", Some("t2"))],
            vec![link("l2", "related", Some("t1"))],
        );
        assert!(read(&reciprocal).is_err());
        let fine = two(
            vec![link("l1", "blocked_by", Some("t2"))],
            vec![link("l2", "related", Some("t1"))],
        );
        let (import, counts) = read(&fine).expect("acyclic");
        assert_eq!(counts.links, 2);
        let target = |row: &TaskRow| row.links[0].target_id.clone();
        assert_eq!(target(&import.tasks[0].row), import.tasks[1].row.id);
        assert_eq!(target(&import.tasks[1].row), import.tasks[0].row.id);
    }

    #[test]
    fn read_board_should_refuse_two_parents_and_a_parent_that_is_a_child() {
        let mut document = base();
        let mut a = task("t1", "A");
        a["links"] = json!([
            link("l1", "parent", Some("t2")),
            link("l2", "parent", Some("t3"))
        ]);
        document["tasks"] = json!([a, task("t2", "B"), task("t3", "C")]);
        let error = read(&document).expect_err("two parents");
        assert!(error.to_string().contains("two parents"), "{error}");

        let mut chain = base();
        let mut a = task("t1", "A");
        a["links"] = json!([link("l1", "parent", Some("t2"))]);
        let mut b = task("t2", "B");
        b["links"] = json!([link("l2", "parent", Some("t3"))]);
        chain["tasks"] = json!([a, b, task("t3", "C")]);
        let error = read(&chain).expect_err("a chain");
        assert!(error.to_string().contains("both"), "{error}");
    }

    #[test]
    fn read_board_should_keep_every_attachment_file_under_a_fresh_id() {
        let mut document = base();
        document["tasks"][0]["attachments"] = json!([
            {
                "id": "a1", "name": "notes.txt", "mime": "text/plain", "size": 3,
                "uploaded_by": "chip", "created_at": "2026-10-01T10:00:00Z",
                "file": "attachments/a1/notes.txt"
            },
            {
                "id": "a2", "name": "gone.txt", "mime": "text/plain", "size": 9,
                "uploaded_by": "chip", "created_at": "2026-10-01T10:00:00Z", "file": null
            },
            {
                "id": "a3", "name": "a/b.txt", "mime": "text/plain", "size": 2,
                "uploaded_by": "chip", "created_at": "2026-10-01T10:00:00Z",
                "file": "attachments/a3/a_b.txt"
            }
        ]);
        let files: [(&str, &[u8]); 2] = [
            ("attachments/a1/notes.txt", b"abc"),
            ("attachments/a3/a_b.txt", b"hi"),
        ];
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let (import, counts) = read_in(&document, &files, &[], &spool).expect("a board");
        let task = &import.tasks[0];
        assert_eq!((counts.attachments, counts.dropped_attachments), (2, 1));
        assert_eq!(task.row.attachments.len(), 2);
        assert_eq!(fs::read(&task.files[0].path).expect("file"), b"abc");
        assert_eq!(fs::read(&task.files[1].path).expect("file"), b"hi");
        assert_eq!((task.files[0].size, task.files[1].size), (3, 2));
        assert_eq!(task.files[0].path.parent(), spool.file(0).parent());
        for (kept, file) in task.row.attachments.iter().zip(&task.files) {
            assert_eq!(kept.id, file.id);
            assert!(!["a1", "a3"].contains(&kept.id.as_str()));
        }
        assert_eq!(task.row.attachments[1].name, "a/b.txt");
    }

    #[test]
    fn read_board_should_refuse_a_file_no_attachment_claims_and_a_size_that_lies() {
        let error = read_as(&base(), &[("attachments/x/y.txt", b"hi")], &[]).expect_err("extra");
        assert!(
            error.to_string().contains("belongs to no attachment"),
            "{error}"
        );
        let mut document = base();
        document["tasks"][0]["attachments"] = json!([{
            "id": "a1", "name": "n.txt", "mime": "text/plain", "size": 5,
            "uploaded_by": "chip", "created_at": "2026-10-01T10:00:00Z",
            "file": "attachments/a1/n.txt"
        }]);
        let error = read_as(&document, &[("attachments/a1/n.txt", b"abc")], &[]).expect_err("size");
        assert!(error.to_string().contains("says 5 bytes"), "{error}");
    }

    #[test]
    fn read_board_should_count_links_to_tasks_outside_the_archive_as_dropped() {
        let mut document = base();
        let mut a = task("t1", "A");
        a["links"] = json!([
            link("l1", "related", Some("t2")),
            link("l2", "blocked_by", None),
            link("l3", "parent", None)
        ]);
        document["tasks"] = json!([a, task("t2", "B")]);
        let (import, counts) = read(&document).expect("a board");
        assert_eq!((counts.links, counts.dropped_links), (1, 2));
        assert_eq!(import.tasks[0].row.links.len(), 1);
    }

    #[test]
    fn read_board_should_match_the_importing_handle_and_keep_other_names_as_text() {
        let mut document = base();
        document["tasks"][0]["assignees"] = json!(["CHIP", "ann", "bob", "ann"]);
        document["tasks"][0]["metadata"] = json!({"keep": 1});
        let (import, counts) = read(&document).expect("a board");
        let row = &import.tasks[0].row;
        assert_eq!(row.member_ids, ["acc"]);
        assert_eq!(
            row.metadata,
            json!({"keep": 1, "imported_assignees": ["ann", "bob"]})
                .as_object()
                .cloned()
        );
        assert_eq!(
            (counts.assignees_matched, counts.assignees_kept_as_text),
            (1, 2)
        );

        let mut stranger = base();
        stranger["tasks"][0]["assignees"] = json!(["ann"]);
        let (import, _) = read(&stranger).expect("a board");
        assert!(import.tasks[0].row.member_ids.is_empty());
    }

    #[test]
    fn read_board_should_match_a_break_activity_by_name_and_drop_the_rest() {
        let mut document = base();
        let mut walk = entry("n1", "2026-10-01T10:00:00Z");
        walk["kind"] = json!("short_break");
        walk["break_activity"] = json!("Walk");
        let mut stretch = walk.clone();
        stretch["id"] = json!("n2");
        stretch["break_activity"] = json!("Stretch");
        document["tasks"][0]["time_entries"] = json!([walk, stretch]);
        let activities = [BreakActivity {
            id: "act-1".to_string(),
            name: "Walk".to_string(),
            description: String::new(),
            daily_goal: 1,
            daily_limit: None,
        }];
        let (import, counts) = read_as(&document, &[], &activities).expect("a board");
        let entries = &import.tasks[0].entries;
        assert_eq!(entries[0].activity_id.as_deref(), Some("act-1"));
        assert_eq!(entries[1].activity_id, None);
        assert_eq!(counts.dropped_break_activities, 1);
        assert!(entries.iter().all(|entry| entry.account_id == "acc"));
        assert!(
            entries
                .iter()
                .all(|entry| entry.task_id == import.tasks[0].row.id)
        );
    }

    #[test]
    fn read_board_should_drop_claims_and_secrets() {
        let mut document = base();
        document["tasks"][0]["claim"] = json!({
            "claimant": "agent", "role": null, "harness": null, "model": null,
            "jj_workspace": null, "jj_change_id": null, "session_id": null, "fence": 2,
            "started_at": "2026-10-01T10:00:00Z", "heartbeat_at": "2026-10-01T10:00:00Z",
            "expires_at": "2099-10-01T10:00:00Z"
        });
        document["tasks"][0]["claim_fence"] = json!(2);
        document["board"]["settings"] = json!({
            "calendar_feed_token": "feed", "archived": true, "legend_visible": true
        });
        document["columns"][0]["settings"] = json!({"api_secret": "x", "sorting": "name"});
        let (import, _) = read(&document).expect("a board");
        assert!(import.tasks[0].row.claim.is_none());
        assert_eq!(import.tasks[0].row.claim_fence, 0);
        assert_eq!(
            Value::Object(import.settings),
            json!({"legend_visible": true})
        );
        assert_eq!(import.columns[0].config_json, r#"{"sorting":"name"}"#);
    }

    #[test]
    fn read_board_should_keep_a_choice_its_field_no_longer_lists_and_refuse_a_wrong_type() {
        let mut document = base();
        document["custom_fields"] = json!([{
            "id": "f1", "key": "size", "label": "Size", "kind": "select", "options": ["S"],
            "columns": [], "default_value": null, "required": false, "owner": "user",
            "position": 0, "created_at": "2026-10-01T10:00:00Z"
        }]);
        document["tasks"][0]["field_values"] = json!({"size": "XL"});
        let (import, _) = read(&document).expect("a stale choice stays");
        assert_eq!(import.tasks[0].row.fields["size"], json!("XL"));
        document["tasks"][0]["field_values"] = json!({"size": 5});
        let error = read(&document).expect_err("a number is no choice");
        assert!(
            error.to_string().contains("bad value for the field size"),
            "{error}"
        );
    }

    #[test]
    fn read_board_should_refuse_a_path_that_leaves_the_archive_or_a_link() {
        let json = base().to_string();
        for path in [
            "../board.json",
            "/etc/passwd",
            "attachments/../../x",
            "attachments/a/../b",
        ] {
            let archive = gzip(&tar_of(&[(BOARD_JSON, json.as_bytes()), (path, b"x")]));
            let error = read_bytes(&archive).expect_err(path);
            assert!(matches!(error, ImportError::Invalid(_)), "{path}: {error}");
        }
        let mut block = tar::entry_header("attachments/a/link", 0, 0).expect("header");
        block[156] = b'2';
        block[157..168].copy_from_slice(b"/etc/passwd");
        block[148..156].fill(b' ');
        let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
        block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut tar = tar_of(&[(BOARD_JSON, json.as_bytes())]);
        tar.truncate(tar.len() - tar::ZEROS.len());
        tar.extend_from_slice(&block);
        tar.extend_from_slice(&tar::ZEROS);
        let error = read_bytes(&gzip(&tar)).expect_err("a symlink");
        assert!(error.to_string().contains("not a regular file"), "{error}");
        let outside = gzip(&tar_of(&[
            (BOARD_JSON, json.as_bytes()),
            ("notes.txt", b"x"),
        ]));
        let error = read_bytes(&outside).expect_err("outside the format");
        assert!(
            error.to_string().contains("not part of a board archive"),
            "{error}"
        );
    }

    #[test]
    fn read_board_should_stop_a_gzip_bomb_at_the_unpacked_limit() {
        let zeros = vec![0u8; 4 << 20];
        let bomb = gzip(&tar_of(&[(BOARD_JSON, &zeros)]));
        assert!(bomb.len() < 100_000, "{} bytes", bomb.len());
        let account = AccountId::new("acc");
        let importer = Importer {
            handle: "chip",
            account: &account,
            activities: &[],
        };
        let limits = ReadLimits {
            unpacked_bytes: 1 << 20,
            files: 10,
        };
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let error = read_within(&bomb, &importer, &spool, limits).expect_err("a bomb");
        assert!(matches!(error, ImportError::TooLarge(_)), "{error}");
    }

    #[test]
    fn read_board_should_refuse_bytes_that_are_not_a_board_archive() {
        let valid = archive_of(&base(), &[]);
        let cases: [(&str, Vec<u8>); 5] = [
            ("empty", Vec::new()),
            ("text", b"not gzip at all".to_vec()),
            ("cut", valid[..valid.len() / 2].to_vec()),
            ("json", gzip(&tar_of(&[(BOARD_JSON, b"{not json")]))),
            ("no board.json", gzip(&tar_of(&[]))),
        ];
        for (label, bytes) in cases {
            let error = read_bytes(&bytes).expect_err(label);
            assert!(matches!(error, ImportError::Invalid(_)), "{label}: {error}");
        }
    }

    struct World {
        _dir: TempDir,
        db: Db,
    }

    fn world(mode: InstanceMode) -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("import.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.apply_mode(mode).expect("mode");
        World { _dir: dir, db }
    }

    struct Seeded {
        world: World,
        account: AccountId,
        board: String,
    }

    fn seed() -> Seeded {
        let world = world(InstanceMode::Single);
        let db = &world.db;
        let account = db.create_user("chip").expect("user");
        let board = db.create_board(&account, "Work / Plans").expect("board");
        let todo = db.create_column(&board, "Todo", Some(3)).expect("column");
        let done = db
            .create_column_as(&board, "Done", None, ColumnKind::Done)
            .expect("column");
        let lane = db.create_swimlane(&board, "Lane").expect("lane");
        let color = db.list_colors(&board).expect("colors")[1].id.clone();
        let task = db
            .create_task(&todo, Some(&lane), "Write import", 2, Some(&color))
            .expect("task");
        let other = db
            .create_task(&done, None, "Ship import", 1, None)
            .expect("task");
        let child = db
            .create_task(&todo, None, "Child task", 1, None)
            .expect("task");
        db.add_subtask(&task, "Design").expect("subtask");
        db.create_entry(&task, 25, "first block").expect("entry");
        db.create_entry(&other, 5, "second").expect("entry");
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
        db.add_task_link(&child, LinkKind::Parent, &task, Actor::SYSTEM)
            .expect("parent");
        db.add_task_link(&other, LinkKind::BlockedBy, &child, Actor::SYSTEM)
            .expect("blocked");
        for (name, bytes) in [
            ("notes.txt", &b"attached bytes"[..]),
            ("a/b.bin", &[0, 1, 2, 255][..]),
        ] {
            db.add_attachment(&task, name, "text/plain", bytes, Actor::SYSTEM)
                .expect("attachment")
                .expect("task");
        }
        let member = db.owner().expect("read").expect("owner").id;
        db.apply_task_change(&task, Actor::SYSTEM, |row| {
            row.member_ids.push(member);
            row.labels.push("urgent".to_string());
            row.fields.insert("stage".to_string(), json!("draft"));
            ((), true)
        })
        .expect("change");
        Seeded {
            world,
            account,
            board,
        }
    }

    fn export(db: &Db, account: &AccountId, board: &str) -> Vec<u8> {
        let snapshot = db
            .account(account)
            .board_snapshot(board)
            .expect("snapshot")
            .expect("owned board");
        let mut archive = Archive::new(snapshot, now()).expect("archive");
        let mut gzipped = Vec::new();
        while let Some(chunk) = archive.next_chunk().expect("chunk") {
            gzipped.extend_from_slice(&chunk);
        }
        gzipped
    }

    fn document_of(gzipped: &[u8]) -> Value {
        let limits = ReadLimits {
            unpacked_bytes: 1 << 30,
            files: 1000,
        };
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let files = tar::read_files(GzDecoder::new(gzipped), limits, &spool, |_| {
            Some(Cap {
                bytes: 1 << 30,
                spooled: true,
            })
        })
        .expect("a readable archive");
        let json = files
            .iter()
            .find(|file| file.path == BOARD_JSON)
            .expect("board.json");
        let Data::Spooled(path) = &json.data else {
            panic!("spooled");
        };
        serde_json::from_slice(&fs::read(path).expect("file")).expect("json")
    }

    fn import_into(
        db: &Db,
        account: &AccountId,
        handle: &str,
        gzipped: &[u8],
    ) -> Result<(String, ImportCounts), String> {
        let user = db.account(account);
        let settings = user.settings().map_err(|error| error.to_string())?;
        let importer = Importer {
            handle,
            account,
            activities: &settings.break_activities,
        };
        let base = tempfile::tempdir().map_err(|error| error.to_string())?;
        let spool = Spool::create(base.path()).map_err(|error| error.to_string())?;
        let (import, counts) =
            read_board(gzipped, &importer, &spool).map_err(|error| error.to_string())?;
        let id = user
            .import_board(import, "", Actor::SYSTEM)
            .map_err(|error| error.to_string())?;
        Ok((id, counts))
    }

    fn collect_ids(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    match (key.as_str(), inner) {
                        ("id", Value::String(id)) => found.push(id.clone()),
                        _ => collect_ids(inner, found),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect_ids(item, found)),
            _ => {}
        }
    }

    fn normalized(mut doc: Value) -> Value {
        doc.as_object_mut().expect("object").remove("exported_at");
        doc["tasks"]
            .as_array_mut()
            .expect("tasks")
            .sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let index = |doc: &Value, key: &str| -> HashMap<String, usize> {
            doc[key]
                .as_array()
                .expect(key)
                .iter()
                .enumerate()
                .map(|(at, row)| (row["id"].as_str().expect("id").to_string(), at))
                .collect()
        };
        let columns = index(&doc, "columns");
        let lanes = index(&doc, "swimlanes");
        let colors = index(&doc, "colors");
        let tasks = index(&doc, "tasks");
        let swap = |value: &mut Value, map: &HashMap<String, usize>| {
            if let Some(old) = value.as_str() {
                let at = map[old];
                *value = json!(at);
            }
        };
        let strip = |rows: &mut Value| {
            for row in rows.as_array_mut().expect("rows") {
                row.as_object_mut().expect("row").remove("id");
            }
        };
        doc["board"].as_object_mut().expect("board").remove("id");
        swap(&mut doc["board"]["settings"]["main_swimlane_id"], &lanes);
        for key in ["columns", "colors", "custom_fields"] {
            strip(&mut doc[key]);
        }
        for lane in doc["swimlanes"].as_array_mut().expect("lanes") {
            for limit in lane["limits"].as_array_mut().expect("limits") {
                swap(&mut limit["column_id"], &columns);
            }
        }
        strip(&mut doc["swimlanes"]);
        for task in doc["tasks"].as_array_mut().expect("tasks") {
            task.as_object_mut().expect("task").remove("id");
            swap(&mut task["column_id"], &columns);
            swap(&mut task["swimlane_id"], &lanes);
            swap(&mut task["color_id"], &colors);
            swap(&mut task["due_target_column_id"], &columns);
            for key in [
                "subtasks",
                "comments",
                "history",
                "time_entries",
                "links",
                "attachments",
            ] {
                strip(&mut task[key]);
            }
            for event in task["history"].as_array_mut().expect("history") {
                swap(&mut event["column_id"], &columns);
            }
            for link in task["links"].as_array_mut().expect("links") {
                swap(&mut link["target_task_id"], &tasks);
            }
            for attachment in task["attachments"].as_array_mut().expect("attachments") {
                attachment
                    .as_object_mut()
                    .expect("attachment")
                    .remove("file");
            }
            task["time_entries"]
                .as_array_mut()
                .expect("entries")
                .sort_by(|a, b| {
                    (a["started_at"].as_str(), a["note"].as_str())
                        .cmp(&(b["started_at"].as_str(), b["note"].as_str()))
                });
        }
        doc
    }

    #[test]
    fn import_should_recreate_every_row_of_an_export_under_fresh_ids() {
        let seeded = seed();
        let db = &seeded.world.db;
        let original = export(db, &seeded.account, &seeded.board);
        let (id, counts) = import_into(db, &seeded.account, "chip", &original).expect("import");
        assert_ne!(id, seeded.board);
        assert_eq!(
            counts,
            ImportCounts {
                columns: 2,
                swimlanes: 1,
                colors: 10,
                custom_fields: 1,
                tasks: 3,
                subtasks: 1,
                comments: 1,
                attachments: 2,
                links: 3,
                time_entries: 2,
                assignees_matched: 1,
                ..ImportCounts::default()
            }
        );

        let copy = export(db, &seeded.account, &id);
        let (before, after) = (document_of(&original), document_of(&copy));
        assert_eq!(normalized(before.clone()), normalized(after.clone()));

        let mut old_ids = Vec::new();
        collect_ids(&before, &mut old_ids);
        assert!(old_ids.len() > 25, "{} ids", old_ids.len());
        let copied = after.to_string();
        for old in &old_ids {
            assert!(!copied.contains(&format!("\"{old}\"")), "{old} was kept");
        }

        let mut checked = 0;
        for task in after["tasks"].as_array().expect("tasks") {
            for attachment in task["attachments"].as_array().expect("attachments") {
                let bytes = db
                    .read_attachment_data(attachment["id"].as_str().expect("id"))
                    .expect("read")
                    .expect("bytes");
                match attachment["name"].as_str().expect("name") {
                    "notes.txt" => assert_eq!(bytes, b"attached bytes"),
                    "a/b.bin" => assert_eq!(bytes, [0, 1, 2, 255]),
                    other => panic!("{other}"),
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 2);
    }

    #[test]
    fn import_should_make_independent_boards_when_the_same_archive_comes_twice() {
        let seeded = seed();
        let db = &seeded.world.db;
        let original = export(db, &seeded.account, &seeded.board);
        let (first, _) = import_into(db, &seeded.account, "chip", &original).expect("first");
        let (second, _) = import_into(db, &seeded.account, "chip", &original).expect("second");
        assert_eq!(db.owner_boards(&seeded.account).expect("boards").len(), 4);
        let (one, two) = (
            document_of(&export(db, &seeded.account, &first)),
            document_of(&export(db, &seeded.account, &second)),
        );
        let (mut ids_one, mut ids_two) = (Vec::new(), Vec::new());
        collect_ids(&one, &mut ids_one);
        collect_ids(&two, &mut ids_two);
        assert!(ids_one.iter().all(|id| !ids_two.contains(id)));
        assert_eq!(normalized(one), normalized(two.clone()));

        let renamed = db
            .board_tasks(&second)
            .expect("tasks")
            .into_iter()
            .find(|task| task.name == "Ship import")
            .expect("task");
        db.update_task(&renamed.id, Some("Changed"), None, None, None)
            .expect("update");
        let names = |board: &str| -> Vec<String> {
            let mut names: Vec<String> = db
                .board_tasks(board)
                .expect("tasks")
                .into_iter()
                .map(|task| task.name)
                .collect();
            names.sort();
            names
        };
        assert!(names(&first).contains(&"Ship import".to_string()));
        assert!(names(&second).contains(&"Changed".to_string()));
        assert!(names(&seeded.board).contains(&"Ship import".to_string()));
    }

    #[test]
    fn import_should_create_the_board_for_the_importing_account_only() {
        let world = world(InstanceMode::Multi);
        let db = &world.db;
        let admin = db.create_user("admin").expect("admin");
        let ann = db.create_user("ann").expect("member");
        let bob = db.create_user("bob").expect("member");
        let board = db.create_board(&ann, "Ann").expect("board");
        db.create_swimlane(&board, "Lane").expect("lane");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let task = db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        db.create_entry(&task, 10, "work").expect("entry");
        db.add_attachment(&task, "a.txt", "text/plain", b"hi", Actor::SYSTEM)
            .expect("attachment")
            .expect("task");
        let owner = db.account(&ann);
        let before = (
            db.account_entries(&ann).expect("entries").len(),
            db.stored_bytes_by_account().expect("usage")[ann.as_str()],
        );
        let gzipped = export(db, &ann, &board);

        let (id, _) = import_into(db, &bob, "bob", &gzipped).expect("import");
        let boards = db.owner_boards(&bob).expect("boards");
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].id, id);
        assert_eq!(boards[0].owner_id, bob.as_str());
        assert_eq!(db.owner_boards(&ann).expect("boards").len(), 1);
        assert!(owner.board(&id).expect("read").is_none());
        assert!(db.account(&bob).board(&id).expect("read").is_some());
        let usage = db.stored_bytes_by_account().expect("usage");
        assert!(usage[bob.as_str()] > 0);
        assert_eq!(
            (
                db.account_entries(&ann).expect("entries").len(),
                usage[ann.as_str()]
            ),
            before
        );
        let entries = db.account_entries(&bob).expect("entries");
        assert_eq!(entries.len(), 1);
        assert!(db.account_entries(&admin).expect("entries").is_empty());
        let copied = db.board_tasks(&id).expect("tasks");
        assert_eq!(copied.len(), 1);
        assert_eq!(entries[0].task_id, copied[0].id);
        assert!(copied[0].member_ids.is_empty());
        assert_eq!(copied[0].attachments.len(), 1);
    }

    fn state(db: &Db, account: &AccountId) -> (usize, Option<u64>, usize) {
        (
            db.owner_boards(account).expect("boards").len(),
            db.stored_bytes_by_account()
                .expect("usage")
                .get(account.as_str())
                .copied(),
            db.account_entries(account).expect("entries").len(),
        )
    }

    fn member_world(edit: impl FnOnce(&mut Quotas)) -> (World, AccountId) {
        let world = world(InstanceMode::Multi);
        world.db.create_user("admin").expect("admin");
        let bob = world.db.create_user("bob").expect("member");
        world
            .db
            .update_instance_settings(|settings| edit(&mut settings.quotas))
            .expect("quotas");
        (world, bob)
    }

    #[test]
    fn import_should_refuse_a_board_past_a_quota_and_write_nothing() {
        let seeded = seed();
        let original = export(&seeded.world.db, &seeded.account, &seeded.board);
        type Edit = fn(&mut Quotas);
        let cases: [(Edit, &str); 4] = [
            (|q| q.tasks_per_board = 2, "tasks per board"),
            (|q| q.attachments_per_task = 1, "attachments per task"),
            (|q| q.columns_per_board = 1, "columns per board"),
            (
                |q| q.time_entries_per_account = 1,
                "time entries per account",
            ),
        ];
        for (edit, named) in cases {
            let (world, bob) = member_world(edit);
            let before = state(&world.db, &bob);
            let error = import_into(&world.db, &bob, "bob", &original).expect_err(named);
            assert!(error.contains(named), "{named}: {error}");
            assert_eq!(state(&world.db, &bob), before, "{named} left rows behind");
        }
        let (world, bob) = member_world(|q| q.boards_per_account = 1);
        world.db.create_board(&bob, "Held").expect("board");
        let before = state(&world.db, &bob);
        let error = import_into(&world.db, &bob, "bob", &original).expect_err("boards");
        assert!(error.contains("boards per account"), "{error}");
        assert_eq!(state(&world.db, &bob), before);
    }

    fn comment(id: String, body: &str) -> Value {
        json!({
            "id": id, "author": "chip", "body": body, "created_at": "2026-10-01T10:00:00Z",
            "actor_kind": "user", "token_name": null, "kind": null, "edited_at": null
        })
    }

    #[test]
    fn import_should_refuse_layout_and_row_quotas_and_write_nothing() {
        let field = |id: &str| {
            json!({
                "id": id, "key": id, "label": id, "kind": "text", "options": [], "columns": [],
                "default_value": null, "required": false, "owner": "user", "position": 0,
                "created_at": "2026-10-01T10:00:00Z"
            })
        };
        let subtask = |id: &str| json!({"id": id, "name": id, "done": false, "due_at": null});
        let heavy = "x".repeat(4900);
        let rows: Vec<Value> = (0..3)
            .map(|n| {
                let mut task = task(&format!("t{n}"), &format!("Task {n}"));
                let comments: Vec<Value> = (0..150)
                    .map(|m| comment(format!("m{n}-{m}"), &heavy))
                    .collect();
                task["comments"] = json!(comments);
                task
            })
            .collect();
        let mut lanes = base();
        lanes["swimlanes"] = json!([lane("s1"), lane("s2")]);
        let mut fields = base();
        fields["custom_fields"] = json!([field("a"), field("b")]);
        let mut comments = base();
        comments["tasks"][0]["comments"] =
            json!([comment("m1".into(), "a"), comment("m2".into(), "b")]);
        let mut subtasks = base();
        subtasks["tasks"][0]["subtasks"] = json!([subtask("a"), subtask("b")]);
        let mut stored = base();
        stored["tasks"] = json!(rows);
        type Edit = fn(&mut Quotas);
        let cases: [(Value, Edit, &str); 5] = [
            (lanes, |q| q.swimlanes_per_board = 1, "swimlanes per board"),
            (
                fields,
                |q| q.fields_per_board = 1,
                "custom fields per board",
            ),
            (comments, |q| q.comments_per_task = 1, "comments per task"),
            (subtasks, |q| q.subtasks_per_task = 1, "subtasks per task"),
            (
                stored,
                |q| q.stored_mib_per_account = 1,
                "MiB stored per account",
            ),
        ];
        for (document, edit, named) in cases {
            let (world, bob) = member_world(edit);
            let before = state(&world.db, &bob);
            let error =
                import_into(&world.db, &bob, "bob", &archive_of(&document, &[])).expect_err(named);
            assert!(error.contains(named), "{named}: {error}");
            assert_eq!(state(&world.db, &bob), before, "{named} left rows behind");
        }
    }

    #[test]
    fn read_board_should_add_a_default_swimlane_to_a_board_without_one() {
        let mut document = base();
        document["swimlanes"] = json!([]);
        let (import, counts) = read(&document).expect("a board");
        assert_eq!(counts.swimlanes, 1);
        assert_eq!(import.swimlanes.len(), 1);
        assert_eq!(import.swimlanes[0].name, "Default");
        assert_eq!(
            import.settings[MAIN_SWIMLANE_KEY],
            json!(import.swimlanes[0].id)
        );
        assert!(import.tasks[0].row.swimlane_id.is_none());
    }

    #[test]
    fn import_should_refuse_files_past_the_stored_bytes_quota_and_write_nothing() {
        let (world, bob) = member_world(|q| q.stored_mib_per_account = 1);
        let mut document = base();
        document["tasks"][0]["attachments"] = json!([{
            "id": "a1", "name": "big.bin", "mime": "application/octet-stream", "size": 2 << 20,
            "uploaded_by": "chip", "created_at": "2026-10-01T10:00:00Z",
            "file": "attachments/a1/big.bin"
        }]);
        let big = vec![7u8; 2 << 20];
        let gzipped = archive_of(&document, &[("attachments/a1/big.bin", &big)]);
        let before = state(&world.db, &bob);
        let error = import_into(&world.db, &bob, "bob", &gzipped).expect_err("quota");
        assert!(error.contains("MiB stored per account"), "{error}");
        assert_eq!(state(&world.db, &bob), before);
    }

    #[test]
    fn import_should_refuse_more_time_entries_than_a_day_may_start_and_write_nothing() {
        let world = world(InstanceMode::Single);
        let account = world.db.create_user("chip").expect("user");
        let mut document = base();
        let entries: Vec<Value> = (0..501)
            .map(|n| entry(&format!("n{n}"), "2026-10-01T10:00:00Z"))
            .collect();
        document["tasks"][0]["time_entries"] = json!(entries);
        let before = state(&world.db, &account);
        let error = import_into(&world.db, &account, "chip", &archive_of(&document, &[]))
            .expect_err("per day");
        assert!(error.contains("time entries started per day"), "{error}");
        assert_eq!(state(&world.db, &account), before);
        document["tasks"][0]["time_entries"] = json!(&entries[..500]);
        import_into(&world.db, &account, "chip", &archive_of(&document, &[])).expect("500 fit");
    }

    #[test]
    fn import_should_refuse_a_task_past_the_row_cap_and_write_nothing() {
        let world = world(InstanceMode::Single);
        let account = world.db.create_user("chip").expect("user");
        let mut document = base();
        let comments: Vec<Value> = (0..260)
            .map(|n| {
                json!({
                    "id": format!("m{n}"), "author": "chip", "body": "x".repeat(4900),
                    "created_at": "2026-10-01T10:00:00Z", "actor_kind": "user",
                    "token_name": null, "kind": null, "edited_at": null
                })
            })
            .collect();
        document["tasks"][0]["comments"] = json!(comments);
        let before = state(&world.db, &account);
        let error = import_into(&world.db, &account, "chip", &archive_of(&document, &[]))
            .expect_err("row cap");
        assert!(error.contains("bytes in one task"), "{error}");
        assert_eq!(state(&world.db, &account), before);
    }
}
