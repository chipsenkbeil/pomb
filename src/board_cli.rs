mod table;
mod wire;

use std::collections::HashMap;
use std::path::PathBuf;
use std::{fmt, fs};

use chrono::{DateTime, Utc};
use clap::Subcommand;
use serde_json::{Value, json};
use wire::{
    Attachment, BoardDetail, BoardEntry, Color, Column, Comment, Created, Export, Parent,
    ServerLimits, Subtask, Task, TaskDetail, TaskLabels,
};

use crate::client::{self, Client};

/// The help text after the options of `pomb board`.
pub const AFTER_LONG_HELP: &str = "\
Environment:
  POMB_URL        Base URL of the running server, such as http://127.0.0.1:3000
  POMB_API_TOKEN  API token the requests sign in with
  POMB_BOARD      Board to read, the same as --board

Examples:
  pomb board ls
  pomb board ls --column Doing --label cli
  pomb board show 75ecb314
  pomb board add 'Fix the login page' --column Backlog --size 2 --label ui
  pomb board move 75ecb314 Doing
  pomb board note 75ecb314 'Started on the parser' --kind status
  pomb board outcome 75ecb314 'The parser reads a file'
  pomb board outcome 75ecb314 --tick 1
  pomb board link 75ecb314 blocked_by 1ad5463a
  pomb board label 75ecb314 cli --remove draft
  pomb board attach 75ecb314 ./trace.log";

/// The fewest characters of a card id that name a card.
const MIN_PREFIX: usize = 8;

/// Most characters of a comment's time that show: the date and the minute.
const STAMP_CHARS: usize = 16;

/// The author the server gives a comment whose writer named none.
const DEFAULT_AUTHOR: &str = "api-token";

/// The size a card without a color has.
const UNSIZED: &str = "unsized";

/// The palette word of a card without a color.
const WHITE: &str = "white";

/// The palette word of a card whose color is not in the palette.
const UNKNOWN_COLOR: &str = "unknown";

/// What stands in a cell that has nothing to show.
const BLANK: &str = "-";

/// The `actor_kind` of a comment written by a signed-in person.
const ACTOR_USER: &str = "user";

/// The `actor_kind` of a comment written through an API token.
const ACTOR_API_TOKEN: &str = "api_token";

/// The key under which a card keeps its list of check commands, without the `agentctl.` prefix.
const CHECKS: &str = "checks";

/// The keys of the custom field lists `show` prints, without the `agentctl.` prefix.
const LIST_FIELDS: [&str; 4] = [CHECKS, "allow", "tests", "files"];

/// The size words and the palette color each names.
const SIZES: [(&str, &str); 5] = [
    ("1", "yellow"),
    ("2", "green"),
    ("3", "blue"),
    ("3+", "red"),
    (UNSIZED, WHITE),
];

/// What `pomb board` takes before its verb.
#[derive(Debug, clap::Args)]
pub struct BoardArgs {
    /// Name of the board to read. Optional while the account has exactly one board.
    #[arg(long, env = "POMB_BOARD", value_name = "NAME", global = true)]
    pub board: Option<String>,

    /// The verb to run.
    #[command(subcommand)]
    pub verb: BoardVerb,
}

/// The verbs of `pomb board`.
#[derive(Debug, Subcommand)]
pub enum BoardVerb {
    /// Lists the cards that are not done, one line each.
    ///
    /// Each line holds the first 8 characters of the card id, its column, its size, its labels,
    /// and its name. A ticks column (`ticked/total`) and a holder column appear when any listed
    /// card has outcomes or a live claim. Cards in a done column are left out unless --column
    /// names one.
    Ls(LsArgs),

    /// Shows one card in full.
    ///
    /// Prints its line, its parent and children, its description, its outcomes, its checks,
    /// allow, tests and files lists, and its comments. The card is named by its id or by the
    /// first 8 or more characters of it.
    Show(ShowArgs),

    /// Adds a card.
    ///
    /// The card goes to the first column of the board unless --column names another, in any
    /// case. Prints the first 8 characters of the new card's id and its column.
    Add(AddArgs),

    /// Moves a card to the top of a column.
    ///
    /// The column is named in any case. When the server refuses the move, such as a column at
    /// its WIP limit, its reason is printed and the exit status is 1.
    Move(MoveArgs),

    /// Adds a comment to a card.
    ///
    /// The comment is of kind agent unless --kind names another. The server rejects a
    /// label --as that matches the token's own username.
    Note(NoteArgs),

    /// Adds an outcome to a card, or ticks or unticks one.
    ///
    /// With TEXT the outcome is appended to the card's done-when list. With --tick N or
    /// --untick N the Nth outcome, counted from 1 as show lists them, is marked done or not.
    Outcome(OutcomeArgs),

    /// Links a card to another card of the same board.
    ///
    /// KIND is blocked_by, blocks, related, parent or child. The server's reason is printed
    /// when it refuses the link, such as one that would make a cycle.
    Link(LinkArgs),

    /// Adds labels to a card and removes others.
    ///
    /// A label may be written with a leading +. A label the card stores with a leading + is
    /// removed by that exact name. The card keeps the labels it has besides.
    Label(LabelArgs),

    /// Attaches a file to a card.
    ///
    /// The file goes up under its own name, and the card shows it in `show`.
    Attach(AttachArgs),
}

/// What `pomb board add` creates.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// Name of the new card.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Column to add the card to, by name. The board's first column when left out.
    #[arg(long, value_name = "NAME")]
    pub column: Option<String>,

    /// Size of the card: 1, 2, 3, 3+ or unsized.
    #[arg(long, value_name = "SIZE")]
    pub size: Option<String>,

    /// Label to put on the card. May be given more than once.
    #[arg(long, value_name = "LABEL")]
    pub label: Vec<String>,

    /// Description of the card.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,
}

/// What `pomb board move` moves where.
#[derive(Debug, clap::Args)]
pub struct MoveArgs {
    /// Id of the card, or the first 8 or more characters of it.
    #[arg(value_name = "CARD")]
    pub card: String,

    /// Name of the column to move the card to.
    #[arg(value_name = "COLUMN")]
    pub column: String,
}

/// What `pomb board note` writes.
#[derive(Debug, clap::Args)]
pub struct NoteArgs {
    /// Id of the card, or the first 8 or more characters of it.
    #[arg(value_name = "CARD")]
    pub card: String,

    /// Text of the comment.
    #[arg(value_name = "TEXT")]
    pub text: String,

    /// Kind of the comment, such as decision, question, blocked, status or done.
    #[arg(long, value_name = "KIND", value_enum, default_value_t = CommentKind::Agent)]
    pub kind: CommentKind,

    /// Label the comment is written under, such as "reviewer".
    #[arg(long = "as", value_name = "ROLE")]
    pub role: Option<String>,
}

/// The kinds of comment a card holds.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum CommentKind {
    /// A note from an agent, the default.
    Agent,
    /// A decision made.
    Decision,
    /// A question to answer.
    Question,
    /// A reason the card cannot go on.
    Blocked,
    /// A progress report.
    Status,
    /// A report that the work is finished.
    Done,
}

impl CommentKind {
    /// The word the server and the command line use.
    fn word(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Decision => "decision",
            Self::Question => "question",
            Self::Blocked => "blocked",
            Self::Status => "status",
            Self::Done => "done",
        }
    }
}

/// What `pomb board outcome` does to a card's outcomes.
#[derive(Debug, clap::Args)]
pub struct OutcomeArgs {
    /// Id of the card, or the first 8 or more characters of it.
    #[arg(value_name = "CARD")]
    pub card: String,

    /// Text of the outcome to append.
    #[arg(value_name = "TEXT", required_unless_present_any = ["tick", "untick"])]
    pub text: Option<String>,

    /// Number of the outcome to mark done, counted from 1.
    #[arg(long, value_name = "N", conflicts_with_all = ["text", "untick"])]
    pub tick: Option<usize>,

    /// Number of the outcome to mark not done, counted from 1.
    #[arg(long, value_name = "N", conflicts_with = "text")]
    pub untick: Option<usize>,
}

/// The kinds of link between two cards.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum LinkKind {
    /// The card waits for the other.
    BlockedBy,
    /// The other card waits for this one.
    Blocks,
    /// The cards are related.
    Related,
    /// The other card is the umbrella of this one.
    Parent,
    /// The other card is part of this one.
    Child,
}

impl LinkKind {
    /// The word the server and the command line use.
    fn word(self) -> &'static str {
        match self {
            Self::BlockedBy => "blocked_by",
            Self::Blocks => "blocks",
            Self::Related => "related",
            Self::Parent => "parent",
            Self::Child => "child",
        }
    }
}

/// What `pomb board link` joins.
#[derive(Debug, clap::Args)]
pub struct LinkArgs {
    /// Id of the card, or the first 8 or more characters of it.
    #[arg(value_name = "CARD")]
    pub card: String,

    /// Kind of the link.
    #[arg(value_name = "KIND")]
    pub kind: LinkKind,

    /// Id of the other card, or the first 8 or more characters of it.
    #[arg(value_name = "OTHER")]
    pub other: String,
}

/// What `pomb board label` changes.
#[derive(Debug, clap::Args)]
pub struct LabelArgs {
    /// Id of the card, or the first 8 or more characters of it.
    #[arg(value_name = "CARD")]
    pub card: String,

    /// Labels to add.
    #[arg(value_name = "LABEL")]
    pub add: Vec<String>,

    /// Label to remove. May be given more than once.
    #[arg(long, value_name = "LABEL")]
    pub remove: Vec<String>,
}

/// What `pomb board attach` uploads.
#[derive(Debug, clap::Args)]
pub struct AttachArgs {
    /// Id of the card, or the first 8 or more characters of it.
    #[arg(value_name = "CARD")]
    pub card: String,

    /// Path of the file to attach.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// MIME type of the file. The server stores application/octet-stream when left out.
    #[arg(long, value_name = "TYPE")]
    pub mime: Option<String>,
}

/// What `pomb board ls` filters by.
#[derive(Debug, clap::Args)]
pub struct LsArgs {
    /// Only the cards in this column, in any case. A done column is listed only this way.
    #[arg(long, value_name = "NAME")]
    pub column: Option<String>,

    /// Only the cards with this label.
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,

    /// Only the cards of this size: 1, 2, 3, 3+ or unsized.
    #[arg(long, value_name = "SIZE")]
    pub size: Option<String>,
}

/// What `pomb board show` names.
#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// Id of the card, or the first 8 or more characters of it.
    #[arg(value_name = "CARD")]
    pub card: String,
}

/// Why a board command failed.
#[derive(Debug)]
pub enum Error {
    /// The command line asks for something the board does not hold.
    Usage(String),
    /// The command could not be carried out.
    Failed(String),
}

impl Error {
    /// The exit status of the process: 2 for a usage error, 1 for the rest.
    fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Failed(_) => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(text) | Self::Failed(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for Error {}

impl From<client::Error> for Error {
    fn from(error: client::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

/// Runs the board command, prints its output or its error, and returns the exit status.
pub fn execute(args: &BoardArgs) -> i32 {
    let outcome = Client::from_env()
        .map_err(Error::from)
        .and_then(|client| run(&client, args, Utc::now()));
    match outcome {
        Ok(output) => {
            println!("{output}");
            0
        }
        Err(error) => {
            eprintln!("pomb: {error}");
            error.exit_code()
        }
    }
}

/// Runs the verb against the server `client` reaches and returns what it prints. A claim counts
/// as live when it expires after `now`.
pub fn run(client: &Client, args: &BoardArgs, now: DateTime<Utc>) -> Result<String, Error> {
    match &args.verb {
        BoardVerb::Ls(ls) => list(client, args.board.as_deref(), ls, now),
        BoardVerb::Show(show) => show_card(client, args.board.as_deref(), show, now),
        BoardVerb::Add(add) => add_card(client, args.board.as_deref(), add),
        BoardVerb::Move(target) => move_card(client, args.board.as_deref(), target, now),
        BoardVerb::Note(note) => note_card(client, args.board.as_deref(), note, now),
        BoardVerb::Outcome(outcome) => outcome_card(client, args.board.as_deref(), outcome, now),
        BoardVerb::Link(link) => link_cards(client, args.board.as_deref(), link, now),
        BoardVerb::Label(label) => label_card(client, args.board.as_deref(), label, now),
        BoardVerb::Attach(attach) => attach_file(client, args.board.as_deref(), attach, now),
    }
}

/// Lists the cards `args` selects.
fn list(
    client: &Client,
    board: Option<&str>,
    args: &LsArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    let color = args.size.as_deref().map(size_color).transpose()?;
    let (id, columns) = open_board(client, board)?;
    let want = args
        .column
        .as_deref()
        .map(|name| named_column(&columns, name))
        .transpose()?;
    let cards = load_cards(client, &id, &columns, now)?;
    let shown: Vec<&Card> = cards
        .iter()
        .filter(|card| match want {
            Some(column) => card.task.column_id == column.id,
            None => !card.done,
        })
        .filter(|card| {
            args.label
                .as_ref()
                .is_none_or(|label| card.task.labels.contains(label))
        })
        .filter(|card| color.is_none_or(|color| card.color.as_deref().unwrap_or(WHITE) == color))
        .collect();
    if shown.is_empty() {
        return Ok("no cards".to_string());
    }
    let extras = Extras {
        ticks: shown.iter().any(|card| !card.task.subtasks.is_empty()),
        holders: shown.iter().any(|card| card.holder.is_some()),
    };
    let rows: Vec<Vec<String>> = shown.iter().map(|card| card.cells(extras)).collect();
    Ok(table::lines(&rows).join("\n"))
}

/// Shows the card `args` names, with its family, outcomes, lists and comments.
fn show_card(
    client: &Client,
    board: Option<&str>,
    args: &ShowArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    let card = open_card(client, board, &args.card, now)?;
    let comments: Vec<Comment> = client.get(&format!("/api/tasks/{}/comments", card.task.id))?;
    let detail: TaskDetail = client.get(&format!("/api/tasks/{}", card.task.id))?;

    let description = card.task.description.as_deref().unwrap_or_default().trim();
    let description = if description.is_empty() {
        "no description"
    } else {
        description
    };
    Ok(format!(
        "{}\n{}\n{description}\n\n{}{}{}{}",
        table::lines(&[card.cells(Extras::default())]).join(""),
        kin_lines(&detail),
        outcome_block(&card.task.subtasks),
        list_blocks(&card.task),
        attachment_block(&card.task.attachments),
        comment_lines(&comments),
    ))
}

/// Adds the card `args` describes and returns its short id and column.
fn add_card(client: &Client, board: Option<&str>, args: &AddArgs) -> Result<String, Error> {
    let color = args.size.as_deref().map(size_color).transpose()?;
    let (id, columns) = open_board(client, board)?;
    let column = match args.column.as_deref() {
        Some(name) => named_column(&columns, name)?,
        None => columns
            .first()
            .ok_or_else(|| Error::Failed("the board has no columns".to_string()))?,
    };
    let mut body = json!({"column_id": column.id, "name": args.name});
    if let Some(color) = color {
        let export: Export = client.get(&format!("/api/boards/{id}/export-data"))?;
        let entry = export
            .colors
            .iter()
            .find(|entry| entry.value == color)
            .ok_or_else(|| Error::Failed(format!("the board has no {color} color for the size")))?;
        body["color_id"] = json!(entry.id);
    }
    if !args.label.is_empty() {
        body["labels"] = json!(args.label);
    }
    if let Some(description) = &args.description {
        body["description"] = json!(description);
    }
    let created: Created = client.post("/api/tasks", &body)?;
    Ok(format!("added {} to {}", short(&created.id), column.name))
}

/// The parts of a card that take a POST.
#[derive(Copy, Clone)]
enum CardPart {
    Move,
    Comments,
    Subtasks,
    Links,
}

impl CardPart {
    /// The path segment of the part.
    fn word(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Comments => "comments",
            Self::Subtasks => "subtasks",
            Self::Links => "links",
        }
    }
}

/// Posts `body` to the `part` of the card `id`, such as its comments, and drops the answer.
fn post_to_card(client: &Client, id: &str, part: CardPart, body: &Value) -> Result<(), Error> {
    let _: Value = client.post(&format!("/api/tasks/{id}/{}", part.word()), body)?;
    Ok(())
}

/// Moves the card `args` names to the top of the column it names.
fn move_card(
    client: &Client,
    board: Option<&str>,
    args: &MoveArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    let (columns, cards) = open_cards(client, board, now)?;
    let column = named_column(&columns, &args.column)?;
    let card = resolve(&cards, &args.card)?;
    let body = json!({"column_id": column.id, "position": 0});
    post_to_card(client, &card.task.id, CardPart::Move, &body)?;
    Ok(format!("moved {} to {}", short(&card.task.id), column.name))
}

/// Writes the comment `args` holds on the card it names.
fn note_card(
    client: &Client,
    board: Option<&str>,
    args: &NoteArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    let card = open_card(client, board, &args.card, now)?;
    let mut body = json!({"body": args.text, "kind": args.kind.word()});
    if let Some(role) = &args.role {
        body["author"] = json!(role);
    }
    post_to_card(client, &card.task.id, CardPart::Comments, &body)?;
    Ok(format!("noted {}", short(&card.task.id)))
}

/// Appends, ticks or unticks the outcome `args` names on the card it names.
fn outcome_card(
    client: &Client,
    board: Option<&str>,
    args: &OutcomeArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    let card = open_card(client, board, &args.card, now)?;
    let id = &card.task.id;
    let subtasks = &card.task.subtasks;
    if let Some(text) = &args.text {
        post_to_card(client, id, CardPart::Subtasks, &json!({"name": text}))?;
        return Ok(format!(
            "added outcome {} to {}",
            subtasks.len() + 1,
            short(id)
        ));
    }
    let (number, done) = match (args.tick, args.untick) {
        (Some(number), _) => (number, true),
        (None, Some(number)) => (number, false),
        (None, None) => return Err(Error::Usage("give the text of an outcome".to_string())),
    };
    let outcome = number
        .checked_sub(1)
        .and_then(|index| subtasks.get(index))
        .ok_or_else(|| {
            Error::Usage(format!(
                "{} has no outcome {number}, it has {}",
                short(id),
                subtasks.len()
            ))
        })?;
    let path = format!("/api/tasks/{id}/subtasks/{}", outcome.id);
    let _: Value = client.patch(&path, &json!({"done": done}))?;
    let word = if done { "ticked" } else { "unticked" };
    Ok(format!("{word} outcome {number} of {}", short(id)))
}

/// Links the card `args` names to the other card it names.
fn link_cards(
    client: &Client,
    board: Option<&str>,
    args: &LinkArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    let (_, cards) = open_cards(client, board, now)?;
    let card = resolve(&cards, &args.card)?;
    let other = resolve(&cards, &args.other)?;
    let body = json!({"kind": args.kind.word(), "target_id": other.task.id});
    post_to_card(client, &card.task.id, CardPart::Links, &body)?;
    Ok(format!(
        "linked {} {} {}",
        short(&card.task.id),
        args.kind.word(),
        short(&other.task.id)
    ))
}

/// Returns a label without its leading plus and surrounding space.
fn label_name(raw: &str) -> Result<String, Error> {
    let name = raw.strip_prefix('+').unwrap_or(raw).trim();
    if name.is_empty() {
        return Err(Error::Usage(format!("a label needs a name, got {raw:?}")));
    }
    Ok(name.to_string())
}

/// Adds and removes the labels `args` holds on the card it names.
fn label_card(
    client: &Client,
    board: Option<&str>,
    args: &LabelArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    if args.add.is_empty() && args.remove.is_empty() {
        return Err(Error::Usage(
            "give a label to add or one to remove with --remove".to_string(),
        ));
    }
    let add = args
        .add
        .iter()
        .map(|label| label_name(label))
        .collect::<Result<Vec<_>, _>>()?;
    let stripped = args
        .remove
        .iter()
        .map(|label| label_name(label))
        .collect::<Result<Vec<_>, _>>()?;
    let card = open_card(client, board, &args.card, now)?;
    let remove: Vec<String> = args
        .remove
        .iter()
        .zip(stripped)
        .map(|(raw, name)| {
            let trimmed = raw.trim();
            if card.task.labels.iter().any(|label| label == trimmed) {
                trimmed.to_string()
            } else {
                name
            }
        })
        .collect();
    if let Some(name) = add.iter().find(|name| remove.contains(name)) {
        return Err(Error::Usage(format!(
            "label {name} is both added and removed"
        )));
    }
    let updated: TaskLabels = client.patch(
        &format!("/api/tasks/{}", card.task.id),
        &json!({"labels_add": add, "labels_remove": remove}),
    )?;
    let labels = updated.labels;
    Ok(format!(
        "labels of {}: {}",
        short(&card.task.id),
        if labels.is_empty() {
            BLANK.to_string()
        } else {
            labels.join(",")
        }
    ))
}

/// Uploads the file `args` names to the card it names.
fn attach_file(
    client: &Client,
    board: Option<&str>,
    args: &AttachArgs,
    now: DateTime<Utc>,
) -> Result<String, Error> {
    let name = args
        .file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Usage(format!("{} has no file name", args.file.display())))?;
    let bytes = fs::read(&args.file)
        .map_err(|err| Error::Failed(format!("cannot read {}: {err}", args.file.display())))?;
    let limit = client
        .get::<ServerLimits>("/api/v1/version")?
        .max_attachment_bytes;
    if bytes.len() as u64 > limit {
        return Err(Error::Failed(format!(
            "{} exceeds the {} MiB attachment limit",
            args.file.display(),
            limit / (1024 * 1024)
        )));
    }
    let card = open_card(client, board, &args.card, now)?;
    let mut query = vec![("name", name)];
    if let Some(mime) = &args.mime {
        query.push(("mime", mime));
    }
    let attachment: Attachment = client.post_bytes(
        &format!("/api/tasks/{}/attachments", card.task.id),
        &query,
        &bytes,
    )?;
    Ok(format!(
        "attached {} ({}) to {}",
        attachment.name,
        short(&attachment.id),
        short(&card.task.id)
    ))
}

/// The parent and children lines of a card, each ending in a newline.
fn kin_lines(detail: &TaskDetail) -> String {
    let mut lines = String::new();
    if let Some(Parent { id, name }) = &detail.parent {
        lines.push_str(&format!("parent: {} {name}\n", short(id)));
    }
    if detail.children_total > 0 {
        lines.push_str(&format!(
            "children: {} of {} done\n",
            detail.children_done, detail.children_total
        ));
    }
    lines
}

/// The `done when:` block of a card's outcomes, empty when it has none.
fn outcome_block(subtasks: &[Subtask]) -> String {
    if subtasks.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = subtasks
        .iter()
        .map(|outcome| {
            format!(
                "- [{}] {}",
                if outcome.done { "x" } else { " " },
                outcome.name
            )
        })
        .collect();
    format!("done when:\n{}\n\n", lines.join("\n"))
}

/// The `attachments:` block of a card's files, empty when it has none.
fn attachment_block(attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = attachments
        .iter()
        .map(|file| format!("- {} {} {} bytes", short(&file.id), file.name, file.size))
        .collect();
    format!("attachments:\n{}\n\n", lines.join("\n"))
}

/// The blocks for the card's checks, allow, tests and files lists that are not empty.
fn list_blocks(task: &Task) -> String {
    let mut blocks = String::new();
    for name in LIST_FIELDS {
        let value = task.fields.get(&format!("agentctl.{name}"));
        let items = if name == CHECKS {
            runs(value)
        } else {
            names(value)
        };
        if !items.is_empty() {
            blocks.push_str(&format!("{name}:\n- {}\n\n", items.join("\n- ")));
        }
    }
    blocks
}

/// The `run` command of each row of a table field.
fn runs(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(rows)) = value else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| row.get("run")?.as_str())
        .map(str::to_string)
        .collect()
}

/// The entries of a multiline text field or an array of strings.
fn names(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => text
            .split(['\r', '\n'])
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// One aligned row per comment, with the lines after the first of a body indented under it.
fn comment_lines(comments: &[Comment]) -> String {
    if comments.is_empty() {
        return "no comments".to_string();
    }
    let bodies: Vec<Vec<String>> = comments
        .iter()
        .map(|comment| body_lines(&comment.body))
        .collect();
    let rows: Vec<Vec<String>> = comments
        .iter()
        .zip(&bodies)
        .map(|(comment, body)| {
            let author = comment
                .author
                .as_deref()
                .filter(|author| *author != DEFAULT_AUTHOR);
            vec![
                short(&comment.id),
                comment_actor(comment),
                comment.kind.clone().unwrap_or_else(|| BLANK.to_string()),
                author.unwrap_or(BLANK).to_string(),
                comment
                    .created_at
                    .chars()
                    .take(STAMP_CHARS)
                    .collect::<String>()
                    .replace('T', " "),
                body[0].clone(),
            ]
        })
        .collect();
    let indent_width: usize = table::widths(&rows)[..5]
        .iter()
        .map(|width| width + table::GAP.len())
        .sum();
    let indent = " ".repeat(indent_width);
    let mut out = Vec::new();
    for (line, body) in table::lines(&rows).into_iter().zip(&bodies) {
        out.push(line);
        for rest in &body[1..] {
            out.push(if rest.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{rest}")
            });
        }
    }
    out.join("\n")
}

/// Who wrote the comment: the token's name, else `human`, `agent` or the kind as it is.
fn comment_actor(comment: &Comment) -> String {
    if let Some(name) = &comment.token_name {
        return name.clone();
    }
    match comment.actor_kind.as_deref() {
        Some(ACTOR_USER) => "human".to_string(),
        Some(ACTOR_API_TOKEN) => "agent".to_string(),
        Some(other) => other.to_string(),
        None => "unknown".to_string(),
    }
}

/// The lines of a comment body, with line endings normalised and no trailing blank lines.
fn body_lines(body: &str) -> Vec<String> {
    let normal = body.replace("\r\n", "\n").replace('\r', "\n");
    normal
        .trim_end_matches('\n')
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// The first characters of an id that `ls` and `show` print.
fn short(id: &str) -> String {
    id.chars().take(MIN_PREFIX).collect()
}

/// The palette word of a size word, in any case.
fn size_color(word: &str) -> Result<&'static str, Error> {
    let lowered = word.to_lowercase();
    SIZES
        .iter()
        .find(|(size, _)| *size == lowered)
        .map(|(_, color)| *color)
        .ok_or_else(|| {
            let words: Vec<&str> = SIZES.iter().map(|(size, _)| *size).collect();
            Error::Usage(format!(
                "unknown size {word}, the sizes are {}",
                words.join(", ")
            ))
        })
}

/// The column called `word`, in any case.
fn named_column<'a>(columns: &'a [Column], word: &str) -> Result<&'a Column, Error> {
    let lowered = word.to_lowercase();
    columns
        .iter()
        .find(|column| column.name.to_lowercase() == lowered)
        .ok_or_else(|| {
            let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
            Error::Usage(format!(
                "unknown column {word}, the columns are {}",
                names.join(", ")
            ))
        })
}

/// Reads the columns and cards of the board the command reads.
fn open_cards(
    client: &Client,
    board: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(Vec<Column>, Vec<Card>), Error> {
    let (id, columns) = open_board(client, board)?;
    let cards = load_cards(client, &id, &columns, now)?;
    Ok((columns, cards))
}

/// Reads the card an id or the start of one names.
fn open_card(
    client: &Client,
    board: Option<&str>,
    word: &str,
    now: DateTime<Utc>,
) -> Result<Card, Error> {
    let (_, mut cards) = open_cards(client, board, now)?;
    let index = position(&cards, word)?;
    Ok(cards.swap_remove(index))
}

/// Finds the board the command reads and returns its id and columns.
fn open_board(client: &Client, name: Option<&str>) -> Result<(String, Vec<Column>), Error> {
    let boards: Vec<BoardEntry> = client.get("/api/boards")?;
    let listed = || {
        boards
            .iter()
            .map(|board| board.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let board = match name {
        Some(name) => boards
            .iter()
            .find(|board| board.name == name)
            .ok_or_else(|| {
                Error::Usage(format!(
                    "no board named {name}, the boards are {}",
                    listed()
                ))
            })?,
        None => match boards.as_slice() {
            [only] => only,
            [] => return Err(Error::Failed("the account has no boards".to_string())),
            _ => {
                return Err(Error::Usage(format!(
                    "the account has more than one board, name one with --board: {}",
                    listed()
                )));
            }
        },
    };
    let detail: BoardDetail = client.get(&format!("/api/boards/{}", board.id))?;
    Ok((board.id.clone(), detail.columns))
}

/// Reads the cards of a board, ordered by column and then as the export sends them.
fn load_cards(
    client: &Client,
    board_id: &str,
    columns: &[Column],
    now: DateTime<Utc>,
) -> Result<Vec<Card>, Error> {
    let Export { mut tasks, colors } =
        client.get(&format!("/api/boards/{board_id}/export-data"))?;
    tasks.sort_by_key(|task| {
        columns
            .iter()
            .position(|column| column.id == task.column_id)
            .unwrap_or(usize::MAX)
    });
    let palette: HashMap<&str, &Color> = colors
        .iter()
        .map(|color| (color.id.as_str(), color))
        .collect();
    Ok(tasks
        .into_iter()
        .map(|task| Card::new(task, columns, &palette, now))
        .collect())
}

/// Finds the card an id or the start of one names.
fn resolve<'a>(cards: &'a [Card], word: &str) -> Result<&'a Card, Error> {
    Ok(&cards[position(cards, word)?])
}

/// Finds where in `cards` the card an id or the start of one names sits.
fn position(cards: &[Card], word: &str) -> Result<usize, Error> {
    let prefix = word.to_lowercase();
    if prefix.len() < MIN_PREFIX || !prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(Error::Failed(format!(
            "a card id needs at least {MIN_PREFIX} hex characters: {word}"
        )));
    }
    let mut found = None;
    for (index, card) in cards.iter().enumerate() {
        let id = card.task.id.to_lowercase();
        if id.starts_with(&prefix) {
            if id == prefix {
                return Ok(index);
            }
            if found.is_some() {
                return Err(Error::Failed(format!(
                    "{word} matches more than one card, give more characters"
                )));
            }
            found = Some(index);
        }
    }
    found.ok_or_else(|| Error::Failed(format!("no card starts with {word}")))
}

/// Which optional columns an `ls` line carries.
#[derive(Copy, Clone, Debug, Default)]
struct Extras {
    /// Whether the ticks column shows.
    ticks: bool,
    /// Whether the holder column shows.
    holders: bool,
}

/// A card with the names its ids stand for.
struct Card {
    /// The card as the export sent it.
    task: Task,
    /// Name of the column the card is in.
    column: String,
    /// Whether that column counts as done.
    done: bool,
    /// Size label of the card.
    size: String,
    /// Palette word of the card's color, if it has one.
    color: Option<String>,
    /// Who holds a live claim on the card.
    holder: Option<String>,
}

impl Card {
    /// Names the ids of `task` and decides whether its claim is live at `now`.
    fn new(
        task: Task,
        columns: &[Column],
        palette: &HashMap<&str, &Color>,
        now: DateTime<Utc>,
    ) -> Self {
        let column = columns.iter().find(|column| column.id == task.column_id);
        let color = task.color_id.as_deref().map(|id| palette.get(id));
        Self {
            column: column
                .map_or("?", |column| column.name.as_str())
                .to_string(),
            done: column.is_some_and(|column| column.is_done),
            size: color
                .flatten()
                .map(|color| color.label.as_str())
                .filter(|label| !label.is_empty())
                .unwrap_or(UNSIZED)
                .to_string(),
            color: color.map(|color| {
                color
                    .map_or(UNKNOWN_COLOR, |color| color.value.as_str())
                    .to_string()
            }),
            holder: task
                .claim
                .as_ref()
                .filter(|claim| {
                    DateTime::parse_from_rfc3339(&claim.expires_at).is_ok_and(|at| at > now)
                })
                .map(|claim| claim.claimant.clone()),
            task,
        }
    }

    /// The cells of the card's row, with the optional columns `extras` asks for.
    fn cells(&self, extras: Extras) -> Vec<String> {
        let labels = if self.task.labels.is_empty() {
            BLANK.to_string()
        } else {
            self.task.labels.join(",")
        };
        let mut cells = vec![
            short(&self.task.id),
            self.column.clone(),
            self.size.clone(),
            labels,
        ];
        if extras.ticks {
            cells.push(self.ticks());
        }
        if extras.holders {
            cells.push(self.holder.clone().unwrap_or_else(|| BLANK.to_string()));
        }
        cells.push(self.task.name.clone());
        cells
    }

    /// Ticked outcomes over all outcomes, or a blank when there are none.
    fn ticks(&self) -> String {
        let total = self.task.subtasks.len();
        if total == 0 {
            return BLANK.to_string();
        }
        let ticked = self
            .task
            .subtasks
            .iter()
            .filter(|outcome| outcome.done)
            .count();
        format!("{ticked}/{total}")
    }
}

#[cfg(test)]
mod tests;
