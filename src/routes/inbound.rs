use std::fmt;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{
    AppError, check_members, check_webhook_column, ensure_entry_allowed, found, new_task_color,
    new_task_placement, parse_body, required,
};
use crate::db::{
    AccountDb, IssuedInbound, MAX_PREFIX_CHARS, NewInbound, PublicDb, push_column_event,
};
use crate::limit::{ClientIp, IpScope, rate_limited_response};
use crate::models::{Actor, ActorKind, Cap, HistoryKind, InboundAddressRow, Responsible, TextCap};
use crate::{AppState, api_error};

/// Returns the path an inbound address receives mail on.
fn receiving_path(address: &str) -> String {
    format!("/api/inbound/{address}")
}

/// An inbound address as the API returns it, without the address itself.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub(super) struct InboundAddressView {
    /// The inbound address's id.
    id: String,
    /// The board the address belongs to.
    board_id: String,
    /// The name the settings show.
    name: String,
    /// The prefix that starts the address, or an empty string for none.
    prefix: String,
    /// The column a task made from mail lands in.
    column_id: String,
    /// RFC3339.
    created_at: String,
}

impl From<InboundAddressRow> for InboundAddressView {
    fn from(row: InboundAddressRow) -> Self {
        Self {
            id: row.id,
            board_id: row.board_id,
            name: row.name,
            prefix: row.prefix,
            column_id: row.column_id,
            created_at: row.created_at,
        }
    }
}

/// An inbound address just created or rotated, with the address that receives its mail. No other
/// response carries the address.
#[derive(Clone, Serialize, ToSchema)]
pub(super) struct IssuedInboundView {
    #[serde(flatten)]
    inbound: InboundAddressView,
    /// The prefix, a hyphen when there is a prefix, and 64 lowercase hex characters, returned
    /// only by this response.
    address: String,
    /// The path that receives the forwarded mail, `/api/inbound/` followed by the address.
    path: String,
}

impl fmt::Debug for IssuedInboundView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedInboundView")
            .field("inbound", &self.inbound)
            .field("address", &"<redacted>")
            .field("path", &"<redacted>")
            .finish()
    }
}

impl From<IssuedInbound> for IssuedInboundView {
    fn from(issued: IssuedInbound) -> Self {
        Self {
            path: receiving_path(&issued.address),
            address: issued.address,
            inbound: InboundAddressView::from(issued.row),
        }
    }
}

/// The body that creates an inbound address.
#[derive(Deserialize, ToSchema)]
pub(super) struct CreateInboundInput {
    /// Required, 1 to 100 characters once trimmed.
    #[schema(value_type = String, required = true)]
    name: Option<String>,
    /// Optional, 1 to 32 lowercase letters, digits and hyphens that neither start nor end with a
    /// hyphen. Absent, null or empty means no prefix.
    prefix: Option<String>,
    /// Required, a column of this board.
    #[schema(value_type = String, required = true)]
    column_id: Option<String>,
}

/// Returns an address name trimmed, or a 400 when it is empty or past its cap.
fn check_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    TextCap::INBOUND_NAME.check(name)?;
    Ok(name.to_string())
}

/// Returns a prefix as stored, empty for none, or a 400 when it is not lowercase letters, digits
/// and inner hyphens within [`MAX_PREFIX_CHARS`].
fn check_prefix(prefix: Option<&str>) -> Result<String, AppError> {
    let prefix = prefix.map(str::trim).unwrap_or_default();
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    if prefix.chars().count() > MAX_PREFIX_CHARS
        || !prefix.chars().all(allowed)
        || prefix.starts_with('-')
        || prefix.ends_with('-')
    {
        return Err(AppError::bad_request(format!(
            "prefix must be at most {MAX_PREFIX_CHARS} lowercase letters, digits and hyphens, \
             without a hyphen at either end"
        )));
    }
    Ok(prefix.to_string())
}

/// Creates an inbound address on a board and returns it with the address, which no other response
/// shows again.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/inbound-addresses",
    operation_id = "create_inbound_address",
    tag = "Inbound",
    params(("id" = String, Path, description = "Board id")),
    request_body = CreateInboundInput,
    responses(
        (status = 200, description = "The new inbound address, with the address that receives its mail", body = IssuedInboundView),
        (status = 400, description = "Missing or overlong name, a prefix with a character that is not a lowercase letter, digit or hyphen, or a column_id that is not a column of the board"),
        (status = 403, description = "`permission_denied`: the token lacks `inbound.manage`"),
        (status = 404, description = "Board not found"),
        (status = 409, description = "`quota_exceeded`: the board already holds 20 inbound addresses"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
pub(super) async fn create_inbound_address(
    account: AccountDb,
    Path(board_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IssuedInboundView>, AppError> {
    let input: CreateInboundInput = parse_body(&headers, body).await?;
    let board = found(account.board(&board_id), "Board not found")?;
    let name = check_name(required(input.name.as_deref(), "name")?)?;
    let prefix = check_prefix(input.prefix.as_deref())?;
    let column_id =
        check_webhook_column(&account, &board_id, required(input.column_id, "column_id")?)?;
    let new = NewInbound {
        board_id,
        name,
        prefix,
        column_id,
    };
    let issued = account
        .create_inbound(&board, new)
        .map_err(AppError::from)?;
    Ok(Json(IssuedInboundView::from(issued)))
}

/// Lists a board's inbound addresses, oldest first, without their addresses.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/inbound-addresses",
    operation_id = "list_inbound_addresses",
    tag = "Inbound",
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "The board's inbound addresses", body = Vec<InboundAddressView>),
        (status = 403, description = "`permission_denied`: the token lacks `inbound.manage`"),
        (status = 404, description = "Board not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
pub(super) async fn list_inbound_addresses(
    account: AccountDb,
    Path(board_id): Path<String>,
) -> Result<Json<Vec<InboundAddressView>>, AppError> {
    if !account.board_exists(&board_id).map_err(AppError::from)? {
        return Err(AppError::not_found("Board not found"));
    }
    let rows = account
        .list_inbound(&board_id)
        .map_err(AppError::from)?
        .into_iter()
        .map(InboundAddressView::from)
        .collect();
    Ok(Json(rows))
}

/// Gives an inbound address a new secret and returns the new address. The old address stops
/// receiving mail at once.
#[utoipa::path(
    post,
    path = "/api/inbound-addresses/{id}/rotate",
    operation_id = "rotate_inbound_address",
    tag = "Inbound",
    params(("id" = String, Path, description = "Inbound address id")),
    responses(
        (status = 200, description = "The inbound address with its new address", body = IssuedInboundView),
        (status = 403, description = "`permission_denied`: the token lacks `inbound.manage`"),
        (status = 404, description = "inbound address not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
pub(super) async fn rotate_inbound_address(
    account: AccountDb,
    Path(id): Path<String>,
) -> Result<Json<IssuedInboundView>, AppError> {
    let stored = found(account.inbound_address(&id), "inbound address not found")?;
    let issued = account
        .rotate_inbound(&stored)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::not_found("inbound address not found"))?;
    Ok(Json(IssuedInboundView::from(issued)))
}

/// Deletes an inbound address, so mail sent to it creates nothing.
#[utoipa::path(
    delete,
    path = "/api/inbound-addresses/{id}",
    operation_id = "delete_inbound_address",
    tag = "Inbound",
    params(("id" = String, Path, description = "Inbound address id")),
    responses(
        (status = 200, description = "Inbound address deleted"),
        (status = 403, description = "`permission_denied`: the token lacks `inbound.manage`"),
        (status = 404, description = "inbound address not found"),
        (status = 401, description = "Missing or invalid credentials"),
    ),
)]
pub(super) async fn delete_inbound_address(
    account: AccountDb,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let stored = found(account.inbound_address(&id), "inbound address not found")?;
    if account.delete_inbound(&stored).map_err(AppError::from)? {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::not_found("inbound address not found"))
    }
}

/// Largest request body an inbound address takes, in bytes.
const MAX_INBOUND_BODY_BYTES: usize = 1024 * 1024;

/// Most bytes of an address the receiving route looks up (a longer one cannot be a stored address).
const MAX_ADDRESS_BYTES: usize = 128;

/// The name of a task made from mail that has no subject.
const NO_SUBJECT: &str = "(no subject)";

/// The mail an inbound-mail service forwards, in the field names of the common services.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub(super) struct InboundMail {
    /// The subject, which becomes the task's name.
    #[serde(default, alias = "Subject")]
    subject: Option<String>,
    /// The plain-text body, which becomes the task's description.
    #[serde(default, alias = "TextBody", alias = "body-plain", alias = "text_body")]
    text: Option<String>,
    /// The HTML body, used only when there is no plain-text body.
    #[serde(default, alias = "HtmlBody", alias = "body-html", alias = "html_body")]
    html: Option<String>,
}

/// The task a mail made.
#[derive(Debug, Serialize, ToSchema)]
pub(super) struct ReceivedMail {
    /// The new task's id.
    task_id: String,
}

/// Returns a task name from a mail subject: the first line, trimmed and cut to the name cap, or
/// [`NO_SUBJECT`] when nothing is left.
fn task_name(subject: Option<&str>) -> String {
    let line = subject
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let name: String = line
        .chars()
        .filter(|c| !c.is_control())
        .take(limit_of(TextCap::TASK_NAME))
        .collect();
    let name = name.trim();
    if name.is_empty() {
        NO_SUBJECT.to_string()
    } else {
        name.to_string()
    }
}

/// Returns `text` cut to at most `max` bytes at a character boundary.
fn cut_to_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Returns the plain text of an HTML body: its script and style blocks dropped, its tags removed
/// and its common entities decoded.
fn text_of_html(html: &str) -> String {
    const BLOCKS: [&str; 2] = ["script", "style"];
    let lower = html.to_ascii_lowercase();
    let mut unclosed = [false; BLOCKS.len()];
    let mut text = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(found) = html[at..].find('<') {
        let open = at + found;
        text.push_str(&html[at..open]);
        let block = BLOCKS
            .iter()
            .position(|tag| lower[open + 1..].starts_with(tag));
        let skipped = block.filter(|&index| !unclosed[index]).and_then(|index| {
            let closing = format!("</{}", BLOCKS[index]);
            let end = lower[open..].find(&closing).and_then(|close| {
                html[open + close..]
                    .find('>')
                    .map(|end| open + close + end + 1)
            });
            unclosed[index] = end.is_none();
            end
        });
        match skipped.or_else(|| html[open..].find('>').map(|end| open + end + 1)) {
            Some(end) => {
                text.push(' ');
                at = end;
            }
            None => at = html.len(),
        }
    }
    text.push_str(&html[at..]);
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the description a mail makes: its plain-text body, else the text of its HTML body,
/// cut to the description cap.
fn description_of(mail: &InboundMail) -> String {
    let text = mail.text.as_deref().map(str::trim).unwrap_or_default();
    let body = if text.is_empty() {
        text_of_html(mail.html.as_deref().unwrap_or_default())
    } else {
        text.to_string()
    };
    cut_to_bytes(&body, limit_of(TextCap::DESCRIPTION)).to_string()
}

/// Returns the request body of an inbound post, refusing with a 413 one past
/// [`MAX_INBOUND_BODY_BYTES`], by its declared length before any byte is read and by the bytes
/// read when it declares none.
async fn read_capped(headers: &HeaderMap, body: Body) -> Result<Bytes, AppError> {
    let too_large = || {
        AppError::with_status(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the request body is over its limit of {MAX_INBOUND_BODY_BYTES} bytes"),
        )
    };
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(|length| length > MAX_INBOUND_BODY_BYTES) {
        return Err(too_large());
    }
    axum::body::to_bytes(body, MAX_INBOUND_BODY_BYTES)
        .await
        .map_err(|_| too_large())
}

/// Turns one forwarded mail into a task in the column its inbound address names. The secret in the
/// path is the only credential: a miss is answered like any unknown route, and posts that miss are
/// limited per client address, while the posts of one address are limited by its own bucket.
#[utoipa::path(
    post,
    path = "/api/inbound/{address}",
    operation_id = "receive_inbound_mail",
    tag = "Inbound",
    params(("address" = String, Path, description = "The inbound address, as the create or rotate response returned it")),
    request_body = InboundMail,
    responses(
        (status = 201, description = "The mail became a task", body = ReceivedMail),
        (status = 400, description = "The body is not a JSON object of the mail fields"),
        (status = 404, description = "No inbound address has this address, which is also the answer for a rotated or deleted one"),
        (status = 409, description = "`quota_exceeded`: the owner's board or storage quota is full"),
        (status = 413, description = "The body is over 1 MiB, and nothing was created"),
        (status = 415, description = "The body is not `application/json`"),
        (status = 422, description = "`column_rule`: the column's entry rules refuse the task"),
        (status = 429, description = "`rate_limited`: the address or the client sent too many posts (wait `Retry-After` seconds)"),
        (status = 507, description = "`storage_full`: the database or its disk is full"),
    ),
    security(()),
)]
pub(super) async fn receive_inbound_mail(
    State(state): State<AppState>,
    public: PublicDb,
    client: ClientIp,
    uri: Uri,
    Path(address): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    let grant = if address.len() > MAX_ADDRESS_BYTES {
        None
    } else {
        public.inbound_grant(&address).map_err(AppError::from)?
    };
    let Some(grant) = grant else {
        return Ok(
            match state.limits.check_ip(IpScope::InboundMiss, client.0) {
                Ok(()) => api_error::route_not_found(uri).await,
                Err(refusal) => rate_limited_response(refusal),
            },
        );
    };
    if let Err(refusal) = state.limits.check_inbound(&grant.row().id) {
        return Ok(rate_limited_response(refusal));
    }
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"));
    if !is_json {
        return Err(AppError::with_status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the body must be application/json".to_string(),
        ));
    }
    let bytes = read_capped(&headers, body).await?;
    let mail: InboundMail = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::bad_request(format!("invalid JSON: {error}")))?;

    let account = public.inbound_account(&grant);
    let Some(column) = account
        .column(&grant.row().column_id)
        .map_err(AppError::from)?
    else {
        return Ok(api_error::route_not_found(uri).await);
    };
    let board_id = column.row().board_id.clone();
    let swimlane_id = account
        .list_swimlanes(&board_id)
        .map_err(AppError::from)?
        .into_iter()
        .next()
        .map(|lane| lane.id);
    let (size, color_id) = new_task_color(&account, &board_id, None, None)?;
    let placement = new_task_placement(&account, &board_id)?;
    let mut row = AccountDb::new_task_row(
        &column,
        swimlane_id.as_deref(),
        &task_name(mail.subject.as_deref()),
        size,
        color_id.as_deref(),
    );
    row.description = description_of(&mail);
    for def in account.list_fields(&board_id).map_err(AppError::from)? {
        if let Some(default) = def.default {
            row.fields.entry(def.key).or_insert(default);
        }
    }
    let board = found(account.board(&board_id), "board not found")?;
    let defaults = board
        .row()
        .default_member_ids(account.account_id().as_str());
    if check_members(&account, &defaults).is_ok() {
        row.set_members(defaults);
        if !board.row().default_member_responsible()
            && matches!(row.responsible, Some(Responsible::Member { .. }))
        {
            row.responsible = None;
        }
    }
    ensure_entry_allowed(&account, column.row(), &board_id, &row)?;
    let actor = Actor {
        name: "email",
        token_name: None,
        kind: ActorKind::System,
        parent_token: None,
        role_id: None,
        role_name: None,
        label: None,
    };
    push_column_event(
        &mut row,
        HistoryKind::Created,
        format!("Task created in {}", column.row().name),
        actor,
        Some(&column.row().id),
    );
    let task_id = account
        .insert_task_checked(&column, row, placement, actor)
        .map_err(AppError::from)?
        .map_err(AppError::bad_request)?;
    Ok((StatusCode::CREATED, Json(ReceivedMail { task_id })).into_response())
}

/// Returns the most a text cap allows.
fn limit_of(cap: TextCap) -> usize {
    match cap.cap {
        Cap::Chars(max) | Cap::Bytes(max) | Cap::Items(max) => max,
    }
}
