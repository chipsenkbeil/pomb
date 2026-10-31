use serde::Deserialize;
use serde_json::{Map, Value};

/// One entry of the board list.
#[derive(Debug, Deserialize)]
pub struct BoardEntry {
    /// Id of the board.
    pub id: String,
    /// Name of the board.
    pub name: String,
}

/// One board with its columns.
#[derive(Debug, Deserialize)]
pub struct BoardDetail {
    /// The columns, left to right.
    #[serde(default)]
    pub columns: Vec<Column>,
}

/// One column of a board.
#[derive(Debug, Deserialize)]
pub struct Column {
    /// Id of the column.
    pub id: String,
    /// Name of the column.
    pub name: String,
    /// Whether cards in the column count as done.
    #[serde(default)]
    pub is_done: bool,
}

/// The cards and the size palette of a board.
#[derive(Debug, Deserialize)]
pub struct Export {
    /// The cards, in export order.
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// The palette colors.
    #[serde(default)]
    pub colors: Vec<Color>,
}

/// One color of a board's palette.
#[derive(Debug, Deserialize)]
pub struct Color {
    /// Id of the color.
    pub id: String,
    /// Palette word such as `red`.
    #[serde(default)]
    pub value: String,
    /// Size label the board gives the color.
    #[serde(default)]
    pub label: String,
}

/// One card as the board export sends it.
#[derive(Debug, Deserialize)]
pub struct Task {
    /// Id of the card.
    pub id: String,
    /// Name of the card.
    #[serde(default)]
    pub name: String,
    /// Id of the column the card is in.
    #[serde(default)]
    pub column_id: String,
    /// Description of the card, if it has one.
    #[serde(default)]
    pub description: Option<String>,
    /// Id of the palette color the card has, if any.
    #[serde(default)]
    pub color_id: Option<String>,
    /// Labels of the card.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Outcomes of the card.
    #[serde(default)]
    pub subtasks: Vec<Subtask>,
    /// Files attached to the card.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Lease on the card, live or expired.
    #[serde(default)]
    pub claim: Option<Claim>,
    /// Custom field values by key.
    #[serde(default)]
    pub fields: Map<String, Value>,
}

/// One outcome of a card.
#[derive(Debug, Deserialize)]
pub struct Subtask {
    /// Id of the outcome.
    pub id: String,
    /// Name of the outcome.
    #[serde(default)]
    pub name: String,
    /// Whether the outcome is ticked.
    #[serde(default)]
    pub done: bool,
}

/// One file attached to a card.
#[derive(Debug, Deserialize)]
pub struct Attachment {
    /// Id of the attachment.
    pub id: String,
    /// File name.
    #[serde(default)]
    pub name: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
}

/// The lease on a card.
#[derive(Debug, Deserialize)]
pub struct Claim {
    /// Who holds the lease.
    pub claimant: String,
    /// When the lease ends, as RFC 3339.
    pub expires_at: String,
}

/// The family of a card.
#[derive(Debug, Deserialize)]
pub struct TaskDetail {
    /// The umbrella the card is part of, if any.
    #[serde(default)]
    pub parent: Option<Parent>,
    /// How many children are done.
    #[serde(default)]
    pub children_done: usize,
    /// How many children the card has.
    #[serde(default)]
    pub children_total: usize,
}

/// The umbrella a card is part of.
#[derive(Debug, Deserialize)]
pub struct Parent {
    /// Id of the umbrella card.
    pub id: String,
    /// Name of the umbrella card.
    #[serde(default)]
    pub name: String,
}

/// One comment on a card.
#[derive(Debug, Deserialize)]
pub struct Comment {
    /// Id of the comment.
    pub id: String,
    /// Who wrote it, as the writer named themselves.
    #[serde(default)]
    pub author: Option<String>,
    /// Text of the comment.
    #[serde(default)]
    pub body: String,
    /// When it was written, as RFC 3339.
    #[serde(default)]
    pub created_at: String,
    /// Kind of credential that wrote it: `user`, `api_token` or `system`.
    #[serde(default)]
    pub actor_kind: Option<String>,
    /// Name of the API token that wrote it, if one did.
    #[serde(default)]
    pub token_name: Option<String>,
    /// Kind the writer tagged it with, such as `status`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// The card a create request made.
#[derive(Debug, Deserialize)]
pub struct Created {
    /// Id of the card.
    pub id: String,
}
