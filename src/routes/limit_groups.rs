use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{AppError, found, parse_body};
use crate::auth::AuthUser;
use crate::db::{AccountDb, GroupWrite};
use crate::models::LimitGroup;

/// A card limit shared by a run of adjacent columns of a board.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub(crate) struct LimitGroupView {
    /// The group's id.
    id: String,
    /// The ids of the columns the group covers, in board order.
    column_ids: Vec<String>,
    /// The most cards the columns of the group hold together.
    limit: i64,
    /// The cards the columns of the group hold now.
    count: usize,
}

impl LimitGroupView {
    /// Returns the view of `group` with the cards its columns hold now.
    fn new(db: &AccountDb, group: &LimitGroup) -> Result<Self, AppError> {
        Ok(Self {
            id: group.id.clone(),
            column_ids: group.column_ids.clone(),
            limit: group.limit,
            count: db.limit_group_count(group).map_err(AppError::from)?,
        })
    }

    /// Returns the views of `groups`, in the order the board stores them.
    pub(crate) fn all(db: &AccountDb, groups: &[LimitGroup]) -> Result<Vec<Self>, AppError> {
        groups.iter().map(|group| Self::new(db, group)).collect()
    }
}

/// The columns and limit of a new limit group.
#[derive(Deserialize, ToSchema)]
pub(crate) struct LimitGroupInput {
    /// The ids of the columns to link, at least two, all on the board and next to each other. A
    /// column joins at most one group.
    column_ids: Vec<String>,
    /// The most cards the linked columns hold together, at least 1.
    limit: i64,
}

/// What to change on a limit group, where a field left out stays as it is.
#[derive(Deserialize, ToSchema)]
pub(crate) struct LimitGroupChange {
    /// The new columns, replacing the group's, under the same rules as on create.
    column_ids: Option<Vec<String>>,
    /// The new limit, at least 1.
    limit: Option<i64>,
}

/// Returns the group a write stored as a view, or the error its refusal maps to.
fn stored(db: &AccountDb, write: GroupWrite) -> Result<Json<LimitGroupView>, AppError> {
    match write {
        GroupWrite::Saved(group) => Ok(Json(LimitGroupView::new(db, &group)?)),
        GroupWrite::NoBoard => Err(AppError::not_found("board not found")),
        GroupWrite::NoGroup => Err(AppError::not_found("limit group not found")),
        GroupWrite::Refused(why) => Err(AppError::bad_request(why)),
    }
}

/// Links adjacent columns of a board under one card limit. A card moves into a linked column only
/// when its own column's limit and the group's limit both allow it, unless the move forces past
/// the limits. A group counts cards only, never hours. It needs `columns.edit` and the board's
/// owner, as setting a column's WIP limit does.
#[utoipa::path(
    post,
    path = "/api/boards/{id}/limit-groups",
    operation_id = "create_limit_group",
    tag = "Boards",
    params(("id" = String, Path, description = "Board id")),
    request_body = LimitGroupInput,
    responses(
        (status = 200, description = "The new group", body = LimitGroupView),
        (status = 400, description = "A limit under 1, fewer than two columns, a column listed twice or not on the board, a column another group covers, columns that are not adjacent, or the board holds 20 groups already"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board not found"),
    ),
)]
pub(crate) async fn create_limit_group(
    account: AccountDb,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<LimitGroupView>, AppError> {
    let input: LimitGroupInput = parse_body(&headers, body).await?;
    let board = found(account.board(&id), "board not found")?;
    let write = account
        .add_limit_group(&board, input.column_ids, input.limit)
        .map_err(AppError::from)?;
    stored(&account, write)
}

/// Changes the columns or the limit of a limit group of a board.
#[utoipa::path(
    patch,
    path = "/api/boards/{id}/limit-groups/{group_id}",
    operation_id = "update_limit_group",
    tag = "Boards",
    params(
        ("id" = String, Path, description = "Board id"),
        ("group_id" = String, Path, description = "Limit group id"),
    ),
    request_body = LimitGroupChange,
    responses(
        (status = 200, description = "The changed group", body = LimitGroupView),
        (status = 400, description = "A limit under 1, fewer than two columns, a column listed twice or not on the board, a column another group covers, or columns that are not adjacent"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board or group not found"),
    ),
)]
pub(crate) async fn update_limit_group(
    account: AccountDb,
    Extension(_user): Extension<AuthUser>,
    Path((id, group_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<LimitGroupView>, AppError> {
    let input: LimitGroupChange = parse_body(&headers, body).await?;
    let board = found(account.board(&id), "board not found")?;
    let write = account
        .update_limit_group(&board, &group_id, input.column_ids, input.limit)
        .map_err(AppError::from)?;
    stored(&account, write)
}

/// Unlinks a limit group, which leaves its columns with their own limits and no group.
#[utoipa::path(
    delete,
    path = "/api/boards/{id}/limit-groups/{group_id}",
    operation_id = "delete_limit_group",
    tag = "Boards",
    params(
        ("id" = String, Path, description = "Board id"),
        ("group_id" = String, Path, description = "Limit group id"),
    ),
    responses(
        (status = 200, description = "Group unlinked"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Board or group not found"),
    ),
)]
pub(crate) async fn delete_limit_group(
    account: AccountDb,
    Extension(_user): Extension<AuthUser>,
    Path((id, group_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let board = found(account.board(&id), "board not found")?;
    if account
        .remove_limit_group(&board, &group_id)
        .map_err(AppError::from)?
    {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::not_found("limit group not found"))
    }
}
