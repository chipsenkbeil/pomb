use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::Response;
use chrono::{Local, Utc};
use redb::ReadTransaction;
use serde::de::DeserializeOwned;

use super::{
    AccountDeleteOutcome, AppState, BOARDS, BOARDS_BY_OWNER, COLUMNS, ClaimOutcome, ClaimRequest,
    ColorDeleteOutcome, ColumnDeleteOutcome, ColumnKind, ColumnPatch, CommentWrite, Db, DbResult,
    DeviceGrant, FIELD_DEFS, FencedWrite, FieldCreateOutcome, LINKS_BY_TARGET, LinkAddOutcome,
    MoveGuards, MoveOutcome, NewComment, NewTaskPlacement, NewTimeEntry, PasskeyAddOutcome,
    PasskeyDeleteOutcome, Rows, SWIMLANES, TASK_COLORS, TASKS, TIME_ENTRIES, TaskContext,
    TemplateDeleteOutcome, index_in, links_blocked_in, no_signed_in_account, owns_board_in,
    read_in,
};
use crate::auth::{AuthUser, account_pending, new_api_token_record};
use crate::models::{
    AccountId, AccountStatus, ActiveTimer, Actor, ApiTokenRecord, AuthScope, BoardRow,
    BoardTemplateRow, ColorRow, ColumnRow, DeviceAuthRow, FieldDefRow, LinkKind, PasskeyRow,
    PruneNotice, RecoveryCodeRow, Settings, SwimlaneRow, TaskAttachment, TaskBrief, TaskCard,
    TaskComment, TaskHead, TaskLink, TaskRow, TimeEntryRow, UserRow,
};

/// A row the bound account owns, checked in the read transaction that loaded it, so a mutation
/// that takes it can only reach that account's data.
#[derive(Clone, Debug)]
pub struct Owned<T>(T);

impl<T> Owned<T> {
    /// Returns the owned row.
    #[must_use]
    pub fn row(&self) -> &T {
        &self.0
    }

    /// Returns the row, giving up the proof.
    #[must_use]
    pub fn into_row(self) -> T {
        self.0
    }
}

/// A board the bound account owns.
pub type OwnedBoard = Owned<BoardRow>;

/// A column on a board the bound account owns.
pub type OwnedColumn = Owned<ColumnRow>;

/// A swimlane on a board the bound account owns.
pub type OwnedSwimlane = Owned<SwimlaneRow>;

/// A task color on a board the bound account owns.
pub type OwnedColor = Owned<ColorRow>;

/// A custom field on a board the bound account owns.
pub type OwnedField = Owned<FieldDefRow>;

/// A time entry the bound account logged.
pub type OwnedEntry = Owned<TimeEntryRow>;

/// A template the bound account may use: a built-in one, or one it saved.
pub type OwnedTemplate = Owned<BoardTemplateRow>;

/// A task on a board the bound account owns, with that board's id.
#[derive(Clone, Debug)]
pub struct OwnedTask {
    /// The task row.
    row: TaskRow,
    /// The board the task sits on.
    board_id: String,
}

impl OwnedTask {
    /// Returns the id of the board the task sits on.
    #[must_use]
    pub fn board_id(&self) -> &str {
        &self.board_id
    }

    /// Returns the owned task row.
    #[must_use]
    pub fn row(&self) -> &TaskRow {
        &self.row
    }

    /// Returns the task row, giving up the proof.
    #[must_use]
    pub fn into_row(self) -> TaskRow {
        self.row
    }
}

/// The reads of the tasks links name, made through one read transaction for one request, which
/// read each task, column and board once, however many links name it. Only the bound account's
/// tasks and boards are found.
pub struct LinkReads {
    /// The read transaction every read goes through.
    txn: ReadTransaction,
    /// The database, which records that a board was used.
    db: Db,
    /// The account whose tasks and boards are found.
    account: AccountId,
    /// What links read of each task, by task id, absent for a missing task or another account's.
    heads: HashMap<String, Option<TaskHead>>,
    /// The board each column sits on, by column id, absent for another account's.
    boards: HashMap<String, Option<String>>,
    /// The name of each board, by board id, absent for another account's.
    board_names: HashMap<String, Option<String>>,
    /// Whether each task a link names sits in a column that is not done, by task id.
    open: HashMap<String, bool>,
}

impl fmt::Debug for LinkReads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkReads")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl LinkReads {
    /// Returns what links read of task `id`, when the account owns its board.
    pub fn head(&mut self, id: &str) -> DbResult<Option<&TaskHead>> {
        if !self.heads.contains_key(id) {
            let head = match read_in::<TaskHead>(&self.txn, TASKS, id)? {
                Some(head) if self.column_board(&head.column_id)?.is_some() => Some(head),
                _ => None,
            };
            self.heads.insert(id.to_string(), head);
        }
        Ok(self.heads.get(id).and_then(Option::as_ref))
    }

    /// Returns the board column `id` sits on, when the account owns it, and records that the board
    /// was used.
    fn column_board(&mut self, id: &str) -> DbResult<Option<String>> {
        if let Some(board) = self.boards.get(id) {
            return Ok(board.clone());
        }
        let board = match read_in::<ColumnRow>(&self.txn, COLUMNS, id)? {
            Some(column) if owns_board_in(&self.txn, &self.account, &column.board_id)? => {
                self.db
                    .touch_board(&column.board_id, Utc::now().timestamp());
                Some(column.board_id)
            }
            _ => None,
        };
        self.boards.insert(id.to_string(), board.clone());
        Ok(board)
    }

    /// Returns the board task `id` sits on, when the account owns it.
    pub fn task_board(&mut self, id: &str) -> DbResult<Option<String>> {
        let Some(column) = self.head(id)?.map(|head| head.column_id.clone()) else {
            return Ok(None);
        };
        self.column_board(&column)
    }

    /// Returns the name of board `id`, when the account owns it.
    pub fn board_name(&mut self, id: &str) -> DbResult<Option<String>> {
        if let Some(name) = self.board_names.get(id) {
            return Ok(name.clone());
        }
        let name = read_in::<BoardRow>(&self.txn, BOARDS, id)?
            .filter(|board| board.owner_id == self.account.as_str())
            .map(|board| board.name);
        self.board_names.insert(id.to_string(), name.clone());
        Ok(name)
    }

    /// Returns task `id`'s links as seen from it, as [`Db::list_task_links`] lists them. A task
    /// the account does not own has none.
    pub fn task_links(&mut self, id: &str) -> DbResult<Vec<TaskLink>> {
        let Some(mut links) = self.head(id)?.map(|head| head.links.clone()) else {
            return Ok(Vec::new());
        };
        for holder_id in index_in(&self.txn, LINKS_BY_TARGET, id)? {
            let Some(holder) = self.head(&holder_id)? else {
                continue;
            };
            for link in holder.links.iter().filter(|link| link.target_id == id) {
                links.push(TaskLink {
                    kind: match link.kind {
                        LinkKind::BlockedBy => LinkKind::Blocks,
                        other => other,
                    },
                    target_id: holder.id.clone(),
                    ..link.clone()
                });
            }
        }
        links.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for link in &mut links {
            link.board_id = self.task_board(&link.target_id)?;
        }
        Ok(links)
    }

    /// Returns true when a `blocked_by` link among `links` names a task in a column that is not
    /// done.
    pub fn blocked(&mut self, links: &[TaskLink]) -> DbResult<bool> {
        links_blocked_in(&self.txn, links, &mut self.open)
    }
}

/// Data access bound to one account.
///
/// Every getter checks, in the read transaction that loads the row, that the account owns it, and
/// answers [`None`] for another account's row exactly as for a missing one. Lists hold only the
/// account's rows. Every mutation takes an [`Owned`] proof or an [`OwnedTask`].
#[derive(Clone, Debug)]
pub struct AccountDb {
    /// The database the handle reads and writes.
    db: Db,
    /// The account every row the handle loads belongs to.
    account: AccountId,
    /// Serializes this account's timer reads and writes.
    timer_gate: Arc<Mutex<()>>,
}

impl Db {
    /// Returns the handle bound to `account`.
    #[must_use]
    pub fn account(&self, account: &AccountId) -> AccountDb {
        AccountDb {
            timer_gate: self.timer_gate(account),
            db: self.clone(),
            account: account.clone(),
        }
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AccountDb {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        match parts.extensions.get::<AuthUser>() {
            Some(user) if user.status == AccountStatus::Active => {
                Ok(state.db.account(user.account_id()))
            }
            Some(_) => Err(account_pending()),
            None => Err(no_signed_in_account()),
        }
    }
}

impl AccountDb {
    /// Maximum stored attachment size: 5 MiB.
    pub const MAX_ATTACHMENT_BYTES: u64 = Db::MAX_ATTACHMENT_BYTES;
    /// Maximum serialized size of a task's metadata object: 64 KiB.
    pub const MAX_METADATA_BYTES: usize = Db::MAX_METADATA_BYTES;

    /// Returns the id of the account the handle is bound to.
    #[must_use]
    pub fn account_id(&self) -> &AccountId {
        &self.account
    }

    /// Runs `read` when the account owns board `board_id`, and returns the empty value otherwise.
    fn on_own_board<T: Default>(
        &self,
        board_id: &str,
        read: impl FnOnce() -> DbResult<T>,
    ) -> DbResult<T> {
        let txn = self.db.read()?;
        if owns_board_in(&txn, &self.account, board_id)? {
            read()
        } else {
            Ok(T::default())
        }
    }

    /// Loads a row of a board-scoped table, keeping it only when the account owns its board.
    fn on_owned_board<T>(
        &self,
        table: Rows,
        id: &str,
        board_of: impl Fn(&T) -> &str,
    ) -> DbResult<Option<Owned<T>>>
    where
        T: DeserializeOwned,
    {
        let txn = self.db.read()?;
        let Some(row) = read_in::<T>(&txn, table, id)? else {
            return Ok(None);
        };
        if !owns_board_in(&txn, &self.account, board_of(&row))? {
            return Ok(None);
        }
        self.mark_used(board_of(&row));
        Ok(Some(Owned(row)))
    }

    /// Records that this request used board `board_id`, which keeps it from being pruned.
    fn mark_used(&self, board_id: &str) {
        self.db.touch_board(board_id, Utc::now().timestamp());
    }

    /// Returns the warning each of the account's boards shows before it is pruned, keyed by board
    /// id. Boards outside the warning window, the admin's boards and single mode have none.
    pub fn prune_notices(&self) -> DbResult<HashMap<String, PruneNotice>> {
        let boards = index_in(&self.db.read()?, BOARDS_BY_OWNER, self.account.as_str())?;
        self.db
            .prune_notices(&self.account, &boards, Utc::now().timestamp())
    }

    /// Returns board `id` when the account owns it, with the warning it showed before this visit,
    /// which counts as a use.
    pub fn visit_board(&self, id: &str) -> DbResult<Option<(OwnedBoard, Option<PruneNotice>)>> {
        let notice = self.prune_notices()?.remove(id);
        Ok(self.board(id)?.map(|board| (board, notice)))
    }

    /// Returns the account's own user row.
    pub fn user(&self) -> DbResult<Option<UserRow>> {
        self.db.user(self.account.as_str())
    }

    /// Returns the users the account can assign to its tasks: itself.
    pub fn list_users(&self) -> DbResult<Vec<UserRow>> {
        Ok(self.user()?.into_iter().collect())
    }

    /// Returns the account's settings; defaults when never saved.
    pub fn settings(&self) -> DbResult<Settings> {
        self.db.settings(&self.account)
    }

    /// Stores the account's settings.
    pub fn update_settings(&self, settings: &Settings) -> DbResult<()> {
        self.db.update_settings(&self.account, settings)
    }

    /// Returns board `id` when the account owns it.
    pub fn board(&self, id: &str) -> DbResult<Option<OwnedBoard>> {
        let txn = self.db.read()?;
        let board = read_in::<BoardRow>(&txn, BOARDS, id)?
            .filter(|board| board.owner_id == self.account.as_str());
        if board.is_some() {
            self.mark_used(id);
        }
        Ok(board.map(Owned))
    }

    /// Returns whether the account owns board `id`.
    pub fn board_exists(&self, id: &str) -> DbResult<bool> {
        Ok(self.board(id)?.is_some())
    }

    /// Returns the account's boards, ordered by position (then id for stability).
    pub fn list_boards(&self) -> DbResult<Vec<BoardRow>> {
        self.db.owner_boards(&self.account)
    }

    /// Returns the id of the account's lowest-position unarchived board, or of its lowest-position
    /// archived one when every board is archived.
    pub fn first_board_id(&self) -> DbResult<Option<String>> {
        self.db.first_board_id(&self.account)
    }

    /// Trims a board name and checks it is 1-100 characters long.
    pub fn validate_board_name(name: &str) -> Result<&str, &'static str> {
        Db::validate_board_name(name)
    }

    /// Creates a board for the account with the standard color palette, recording the API token
    /// `created_by_token` that made it, or none when the string is empty.
    pub fn create_board(&self, name: &str, created_by_token: &str) -> DbResult<String> {
        self.db
            .create_board_by(&self.account, name, created_by_token)
    }

    /// Renames a board.
    pub fn rename_board(&self, board: &OwnedBoard, name: &str) -> DbResult<bool> {
        self.db.rename_board(&board.row().id, name)
    }

    /// Merges key/value pairs into a board's config bag, refusing to archive the last active board.
    pub fn set_board_config(
        &self,
        board: &OwnedBoard,
        updates: &serde_json::Map<String, serde_json::Value>,
    ) -> DbResult<bool> {
        self.db.set_board_config(&board.row().id, updates)
    }

    /// Stores `candidate` under `key` in a board's config bag unless it holds a string already.
    pub fn ensure_board_config_string(
        &self,
        board: &OwnedBoard,
        key: &str,
        candidate: &str,
    ) -> DbResult<Option<String>> {
        self.db
            .ensure_board_config_string(&board.row().id, key, candidate)
    }

    /// Deletes a board and everything in it, refusing the account's only or last unarchived board.
    pub fn delete_board(&self, board: &OwnedBoard) -> DbResult<()> {
        self.db.delete_board(&board.row().id)
    }

    /// Gives a board without colors the standard palette.
    pub fn ensure_board_colors(&self, board: &OwnedBoard) -> DbResult<()> {
        self.db.ensure_board_colors(&board.row().id)
    }

    /// Gives a board without swimlanes a "Default" lane.
    pub fn ensure_default_swimlane(&self, board: &OwnedBoard) -> DbResult<()> {
        self.db.ensure_default_swimlane(&board.row().id)
    }

    /// Returns every task on board `board_id` with minutes filled in, in board order; empty for
    /// another account's board.
    pub fn board_tasks(&self, board_id: &str) -> DbResult<Vec<TaskRow>> {
        self.on_own_board(board_id, || self.db.board_tasks(board_id))
    }

    /// Returns the ids of the tasks on board `board_id` in the order of
    /// [`AccountDb::board_tasks`], without holding the board whole; empty for another account's
    /// board.
    pub fn board_task_ids(&self, board_id: &str) -> DbResult<Vec<String>> {
        self.on_own_board(board_id, || self.db.board_task_ids(board_id))
    }

    /// Hands every task on board `board_id` to `visit`, one at a time and in no set order; none
    /// for another account's board.
    pub fn visit_board_tasks<T: DeserializeOwned>(
        &self,
        board_id: &str,
        visit: impl FnMut(T) -> DbResult<()>,
    ) -> DbResult<()> {
        self.on_own_board(board_id, || self.db.visit_board_tasks(board_id, visit))
    }

    /// Hands every task on board `board_id` to `visit` as a [`TaskCard`] with its minutes filled
    /// in, one at a time and in no set order; none for another account's board.
    pub fn visit_board_cards(
        &self,
        board_id: &str,
        mut visit: impl FnMut(TaskCard) -> DbResult<()>,
    ) -> DbResult<()> {
        self.visit_board_tasks(board_id, |mut card: TaskCard| {
            card.total_minutes = self.db.task_total_minutes(&card.id)?;
            visit(card)
        })
    }

    /// Returns task `id` with its minutes filled in, as [`AccountDb::board_tasks`] lists it, or
    /// [`None`] when the account does not own it.
    pub fn board_task(&self, id: &str) -> DbResult<Option<TaskRow>> {
        let Some(task) = self.task(id)? else {
            return Ok(None);
        };
        let mut row = task.into_row();
        row.total_minutes = self.db.task_total_minutes(id)?;
        Ok(Some(row))
    }

    /// Returns the distinct labels used on board `board_id`, sorted; empty for another account's
    /// board.
    pub fn board_labels(&self, board_id: &str) -> DbResult<Vec<String>> {
        self.on_own_board(board_id, || self.db.board_labels(board_id))
    }

    /// Replaces the palette of `target` with a copy of the palette of `source`.
    pub fn copy_colors(&self, target: &OwnedBoard, source: &OwnedBoard) -> DbResult<usize> {
        self.db.copy_colors(&target.row().id, &source.row().id)
    }

    /// Returns column `id` when the account owns its board.
    pub fn column(&self, id: &str) -> DbResult<Option<OwnedColumn>> {
        self.on_owned_board(COLUMNS, id, |column: &ColumnRow| &column.board_id)
    }

    /// Returns the columns of board `board_id` in board order; empty for another account's board.
    pub fn list_columns(&self, board_id: &str) -> DbResult<Vec<ColumnRow>> {
        self.on_own_board(board_id, || self.db.list_columns(board_id))
    }

    /// Appends a column to a board.
    pub fn create_column(
        &self,
        board: &OwnedBoard,
        name: &str,
        wip_limit: Option<i64>,
    ) -> DbResult<String> {
        self.db.create_column(&board.row().id, name, wip_limit)
    }

    /// Appends a column of `kind` to a board.
    pub fn create_column_as(
        &self,
        board: &OwnedBoard,
        name: &str,
        wip_limit: Option<i64>,
        kind: ColumnKind,
    ) -> DbResult<String> {
        self.db
            .create_column_as(&board.row().id, name, wip_limit, kind)
    }

    /// Applies every field of `patch` to a column in one transaction.
    pub fn update_column(&self, column: &OwnedColumn, patch: &ColumnPatch<'_>) -> DbResult<bool> {
        self.db.update_column(&column.row().id, patch)
    }

    /// Moves a column within its board.
    pub fn move_column(&self, column: &OwnedColumn, position: i64) -> DbResult<bool> {
        self.db.move_column(&column.row().id, position)
    }

    /// Deletes a column unless it is its board's last or still holds tasks.
    pub fn delete_column(&self, column: &OwnedColumn) -> DbResult<ColumnDeleteOutcome> {
        self.db.delete_column(&column.row().id)
    }

    /// Returns swimlane `id` when the account owns its board.
    pub fn swimlane(&self, id: &str) -> DbResult<Option<OwnedSwimlane>> {
        self.on_owned_board(SWIMLANES, id, |lane: &SwimlaneRow| &lane.board_id)
    }

    /// Returns the swimlanes of board `board_id` in board order; empty for another account's board.
    pub fn list_swimlanes(&self, board_id: &str) -> DbResult<Vec<SwimlaneRow>> {
        self.on_own_board(board_id, || self.db.list_swimlanes(board_id))
    }

    /// Appends a swimlane to a board.
    pub fn create_swimlane(&self, board: &OwnedBoard, name: &str) -> DbResult<String> {
        self.db.create_swimlane(&board.row().id, name)
    }

    /// Renames a swimlane.
    pub fn rename_swimlane(&self, lane: &OwnedSwimlane, name: &str) -> DbResult<bool> {
        self.db.rename_swimlane(&lane.row().id, name)
    }

    /// Moves a swimlane within its board.
    pub fn move_swimlane(&self, lane: &OwnedSwimlane, position: i64) -> DbResult<bool> {
        self.db.move_swimlane(&lane.row().id, position)
    }

    /// Deletes a swimlane, refusing one that still holds tasks.
    pub fn delete_swimlane(&self, lane: &OwnedSwimlane) -> DbResult<bool> {
        self.db.delete_swimlane(&lane.row().id)
    }

    /// Returns how many tasks sit in a swimlane.
    pub fn count_tasks_in_swimlane(&self, lane: &OwnedSwimlane) -> DbResult<usize> {
        self.db.count_tasks_in_swimlane(&lane.row().id)
    }

    /// Returns color `id` when the account owns its board, with the palette's current hexes.
    pub fn color(&self, id: &str) -> DbResult<Option<OwnedColor>> {
        let color = self.on_owned_board(TASK_COLORS, id, |color: &ColorRow| &color.board_id)?;
        Ok(color.map(|color| Owned(color.into_row().with_palette_hexes())))
    }

    /// Returns the colors of board `board_id` in palette order; empty for another account's board.
    pub fn list_colors(&self, board_id: &str) -> DbResult<Vec<ColorRow>> {
        self.on_own_board(board_id, || self.db.list_colors(board_id))
    }

    /// Returns the default color of board `board_id`; none for a board the account does not own.
    pub fn default_color(&self, board_id: &str) -> DbResult<Option<ColorRow>> {
        self.on_own_board(board_id, || self.db.default_color(board_id))
    }

    /// Checks a color label for create and update.
    pub fn validate_color_label(label: &str) -> Result<(), String> {
        Db::validate_color_label(label)
    }

    /// Adds a color slot for a standard value to a board.
    pub fn create_color(
        &self,
        board: &OwnedBoard,
        value: &str,
        label: Option<&str>,
        description: Option<&str>,
    ) -> DbResult<String> {
        self.db
            .create_color(&board.row().id, value, label, description)
    }

    /// Changes a color's label, description, enabled flag or default status in one transaction.
    pub fn update_color(
        &self,
        color: &OwnedColor,
        label: Option<&str>,
        description: Option<&str>,
        enabled: Option<bool>,
        is_default: Option<bool>,
    ) -> DbResult<bool> {
        self.db
            .update_color(&color.row().id, label, description, enabled, is_default)
    }

    /// Moves a color within its board's palette.
    pub fn move_color(&self, color: &OwnedColor, position: i64) -> DbResult<bool> {
        self.db.move_color(&color.row().id, position)
    }

    /// Deletes a color unless it is the default or tasks use it.
    pub fn delete_color(&self, color: &OwnedColor) -> DbResult<ColorDeleteOutcome> {
        self.db.delete_color(&color.row().id)
    }

    /// Returns template `id` when it is built in or the account saved it.
    pub fn template(&self, id: &str) -> DbResult<Option<OwnedTemplate>> {
        let template = self.db.template(id)?;
        Ok(template.filter(|t| self.sees_template(t)).map(Owned))
    }

    /// Returns whether the account sees `template`: a built-in one, or one it saved.
    fn sees_template(&self, template: &BoardTemplateRow) -> bool {
        template
            .owner_id
            .as_deref()
            .is_none_or(|owner| owner == self.account.as_str())
    }

    /// Returns the built-in templates and the account's own, built-ins first, then by name.
    pub fn list_templates(&self) -> DbResult<Vec<BoardTemplateRow>> {
        self.db.list_templates(&self.account)
    }

    /// Saves a board as a template the account owns.
    pub fn save_board_as_template(
        &self,
        board: &OwnedBoard,
        name: &str,
        description: &str,
    ) -> DbResult<Option<String>> {
        self.db
            .save_board_as_template(&self.account, &board.row().id, name, description)
    }

    /// Builds a new board for the account from a template, recording the API token
    /// `created_by_token` that asked for it, or none when the string is empty.
    pub fn apply_template(
        &self,
        template: &OwnedTemplate,
        name: &str,
        created_by_token: &str,
    ) -> DbResult<Option<String>> {
        self.db
            .apply_template_by(&self.account, &template.row().id, name, created_by_token)
    }

    /// Deletes a template the account saved; built-in ones are refused.
    pub fn delete_template(&self, template: &OwnedTemplate) -> DbResult<TemplateDeleteOutcome> {
        self.db.delete_template(&template.row().id)
    }

    /// Returns task `id` when the account owns its board.
    pub fn task(&self, id: &str) -> DbResult<Option<OwnedTask>> {
        let txn = self.db.read()?;
        let Some(row) = read_in::<TaskRow>(&txn, TASKS, id)? else {
            return Ok(None);
        };
        let Some(column) = read_in::<ColumnRow>(&txn, COLUMNS, &row.column_id)? else {
            return Ok(None);
        };
        if !owns_board_in(&txn, &self.account, &column.board_id)? {
            return Ok(None);
        }
        self.mark_used(&column.board_id);
        let task = OwnedTask {
            row,
            board_id: column.board_id,
        };
        Ok(Some(task))
    }

    /// Returns what links read of task `id`, with the board it sits on, when the account owns
    /// that board.
    fn owned_head(&self, id: &str) -> DbResult<Option<(TaskHead, String)>> {
        let txn = self.db.read()?;
        let Some(head) = read_in::<TaskHead>(&txn, TASKS, id)? else {
            return Ok(None);
        };
        let Some(column) = read_in::<ColumnRow>(&txn, COLUMNS, &head.column_id)? else {
            return Ok(None);
        };
        if !owns_board_in(&txn, &self.account, &column.board_id)? {
            return Ok(None);
        }
        self.mark_used(&column.board_id);
        Ok(Some((head, column.board_id)))
    }

    /// Returns what links read of task `id`, when the account owns its board.
    pub fn task_head(&self, id: &str) -> DbResult<Option<TaskHead>> {
        Ok(self.owned_head(id)?.map(|(head, _)| head))
    }

    /// Returns task `id` with its total logged minutes, when the account owns its board.
    pub fn task_with_minutes(&self, id: &str) -> DbResult<Option<OwnedTask>> {
        let Some(mut task) = self.task(id)? else {
            return Ok(None);
        };
        task.row.total_minutes = self.db.task_total_minutes(id)?;
        Ok(Some(task))
    }

    /// Returns a new, unsaved task row in `column`, stamped now.
    #[must_use]
    pub fn new_task_row(
        column: &OwnedColumn,
        swimlane_id: Option<&str>,
        name: &str,
        size: i64,
        color_id: Option<&str>,
    ) -> TaskRow {
        Db::new_task_row(&column.row().id, swimlane_id, name, size, color_id)
    }

    /// Stores a prepared task row in `column`, checking its field values against the board's field
    /// definitions in the same transaction.
    pub fn insert_task_checked(
        &self,
        column: &OwnedColumn,
        row: TaskRow,
        placement: NewTaskPlacement,
    ) -> DbResult<Result<String, String>> {
        let row = TaskRow {
            column_id: column.row().id.clone(),
            ..row
        };
        self.db.insert_task_checked(row, placement)
    }

    /// Runs `f` on a task in one write transaction, storing the row when `f` reports a change.
    pub fn apply_task_change<R, F>(&self, task: &OwnedTask, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow) -> (R, bool),
    {
        self.db.apply_task_change(&task.row().id, f)
    }

    /// Runs `f` on a task with its board context read in the same write transaction.
    pub fn apply_task_change_in_board<R, F>(&self, task: &OwnedTask, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow, &TaskContext) -> (R, bool),
    {
        self.db.apply_task_change_in_board(&task.row().id, f)
    }

    /// Moves a task into `column` after the checks in `guards`, all in one write transaction.
    pub fn move_task_logged(
        &self,
        task: &OwnedTask,
        column: &OwnedColumn,
        swimlane_id: Option<&str>,
        position: f64,
        guards: MoveGuards<'_>,
    ) -> DbResult<MoveOutcome> {
        self.db.move_task_logged(
            &task.row().id,
            &column.row().id,
            swimlane_id,
            position,
            guards,
        )
    }

    /// Deletes a task, its time entries and its links.
    pub fn delete_task(&self, task: &OwnedTask) -> DbResult<bool> {
        self.db.delete_task(&task.row().id)
    }

    /// Reorders a task's subtasks to the given id sequence.
    pub fn reorder_subtasks(&self, task: &OwnedTask, order: &[String]) -> DbResult<bool> {
        self.db.reorder_subtasks(&task.row().id, order)
    }

    /// Appends a comment and its history event in one write.
    pub fn add_comment(
        &self,
        task: &OwnedTask,
        new: NewComment<'_>,
        actor: Actor<'_>,
        fence: Option<u64>,
    ) -> DbResult<FencedWrite<TaskComment>> {
        self.db.add_comment(&task.row().id, new, actor, fence)
    }

    /// Deletes a comment from a task, logging the deletion in the same write. With an `owner`,
    /// only a comment that API token wrote may go.
    pub fn delete_comment(
        &self,
        task: &OwnedTask,
        comment_id: &str,
        owner: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<CommentWrite<()>> {
        self.db
            .delete_comment(&task.row().id, comment_id, owner, actor)
    }

    /// Replaces the body of a comment, logging the edit in the same write. With an `owner`, only
    /// a comment that API token wrote may change.
    pub fn edit_comment(
        &self,
        task: &OwnedTask,
        comment_id: &str,
        body: &str,
        owner: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<CommentWrite<TaskComment>> {
        self.db
            .edit_comment(&task.row().id, comment_id, body, owner, actor)
    }

    /// Links `task` to `target`, both on the account's boards.
    pub fn add_task_link(
        &self,
        task: &OwnedTask,
        kind: LinkKind,
        target: &OwnedTask,
        actor: Actor<'_>,
    ) -> DbResult<LinkAddOutcome> {
        self.db
            .add_task_link(&task.row().id, kind, &target.row().id, actor)
    }

    /// Returns a task's links as seen from it; [`None`] when the account does not own the task.
    pub fn list_task_links(&self, task_id: &str) -> DbResult<Option<Vec<TaskLink>>> {
        if self.owned_head(task_id)?.is_none() {
            return Ok(None);
        }
        self.db.list_task_links(task_id)
    }

    /// Returns the board a task sits on; [`None`] when the account does not own the task.
    pub fn task_board_id(&self, task_id: &str) -> DbResult<Option<String>> {
        Ok(self.owned_head(task_id)?.map(|(_, board_id)| board_id))
    }

    /// Deletes a link listed on a task, whichever task holds it.
    pub fn delete_task_link(
        &self,
        task: &OwnedTask,
        link_id: &str,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        self.db.delete_task_link(&task.row().id, link_id, actor)
    }

    /// Returns true when a `blocked_by` target of `task` sits in a column that is not done.
    pub fn task_blocked(&self, task: &TaskRow) -> DbResult<bool> {
        self.db.task_blocked(task)
    }

    /// Returns a check of whether a `blocked_by` link among the links it is handed names a task in
    /// a column that is not done. The check reads each named task once, however many lists name
    /// it.
    pub fn blocked_check(&self) -> DbResult<impl FnMut(&[TaskLink]) -> DbResult<bool> + use<>> {
        let mut reads = self.link_reads()?;
        Ok(move |links: &[TaskLink]| reads.blocked(links))
    }

    /// Returns the reads of linked tasks for one request, see [`LinkReads`].
    pub fn link_reads(&self) -> DbResult<LinkReads> {
        Ok(LinkReads {
            txn: self.db.read()?,
            db: self.db.clone(),
            account: self.account.clone(),
            heads: HashMap::new(),
            boards: HashMap::new(),
            board_names: HashMap::new(),
            open: HashMap::new(),
        })
    }

    /// Claims a task with a compare-and-set in one write transaction.
    pub fn claim_task(
        &self,
        task: &OwnedTask,
        request: &ClaimRequest,
        actor: Actor<'_>,
    ) -> DbResult<ClaimOutcome> {
        self.db.claim_task(&task.row().id, request, actor)
    }

    /// Extends the live claim holding `fence`.
    pub fn heartbeat_claim(
        &self,
        task: &OwnedTask,
        fence: u64,
        ttl_seconds: Option<i64>,
        session_id: Option<&str>,
    ) -> DbResult<ClaimOutcome> {
        self.db
            .heartbeat_claim(&task.row().id, fence, ttl_seconds, session_id)
    }

    /// Releases the live claim holding `fence`.
    pub fn release_claim(
        &self,
        task: &OwnedTask,
        fence: u64,
        actor: Actor<'_>,
    ) -> DbResult<ClaimOutcome> {
        self.db.release_claim(&task.row().id, fence, actor)
    }

    /// Attaches a file to a task.
    pub fn add_attachment(
        &self,
        task: &OwnedTask,
        name: &str,
        mime: &str,
        bytes: &[u8],
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        self.db
            .add_attachment(&task.row().id, name, mime, bytes, uploaded_by)
    }

    /// Reads the bytes of one of a task's attachments; [`None`] when the task holds no such
    /// attachment.
    pub fn read_attachment_data(
        &self,
        task: &OwnedTask,
        attachment_id: &str,
    ) -> DbResult<Option<Vec<u8>>> {
        if !task.row().attachments.iter().any(|a| a.id == attachment_id) {
            return Ok(None);
        }
        self.db.read_attachment_data(attachment_id)
    }

    /// Renames one of a task's attachments, which keeps its extension, and returns [`None`] if the
    /// task holds no such attachment.
    pub fn rename_attachment(
        &self,
        task: &OwnedTask,
        attachment_id: &str,
        name: &str,
        actor: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        self.db
            .rename_attachment(&task.row().id, attachment_id, name, actor)
    }

    /// Deletes one of a task's attachments.
    pub fn delete_attachment(
        &self,
        task: &OwnedTask,
        attachment_id: &str,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        self.db
            .delete_attachment(&task.row().id, attachment_id, actor)
    }

    /// Appends a history event to a task.
    pub fn log_event(
        &self,
        task: &OwnedTask,
        kind: &str,
        detail: &str,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        self.db.log_event(&task.row().id, kind, detail, actor)
    }

    /// Bumps a task's completed-pomodori counter.
    pub fn record_pomodoro_complete(&self, task: &OwnedTask) -> DbResult<()> {
        self.db.record_pomodoro_complete(&task.row().id)
    }

    /// Bumps a task's interruption counter.
    pub fn record_interruption(&self, task: &OwnedTask) -> DbResult<()> {
        self.db.record_interruption(&task.row().id)
    }

    /// Returns field `id` when the account owns its board.
    pub fn field(&self, id: &str) -> DbResult<Option<OwnedField>> {
        self.on_owned_board(FIELD_DEFS, id, |field: &FieldDefRow| &field.board_id)
    }

    /// Returns a board's custom fields in position order; empty for another account's board.
    pub fn list_fields(&self, board_id: &str) -> DbResult<Vec<FieldDefRow>> {
        self.on_own_board(board_id, || self.db.list_fields(board_id))
    }

    /// Stores a new field at the end of a board's list, refusing a key the board already has.
    pub fn create_field(
        &self,
        board: &OwnedBoard,
        field: FieldDefRow,
    ) -> DbResult<FieldCreateOutcome> {
        let field = FieldDefRow {
            board_id: board.row().id.clone(),
            ..field
        };
        self.db.create_field(field)
    }

    /// Runs `f` on a field definition in one write transaction, storing it when `f` reports a
    /// change.
    pub fn apply_field_change<R, F>(&self, field: &OwnedField, f: F) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut FieldDefRow) -> (R, bool),
    {
        self.db.apply_field_change(&field.row().id, f)
    }

    /// Converts a field and its board's values in one write transaction.
    pub fn convert_field<E, F>(
        &self,
        field: &OwnedField,
        f: F,
    ) -> DbResult<Option<Result<FieldDefRow, E>>>
    where
        F: FnOnce(&FieldDefRow, &mut [TaskRow]) -> Result<FieldDefRow, E>,
    {
        self.db.convert_field(&field.row().id, f)
    }

    /// Deletes a field and its value on every task of its board.
    pub fn delete_field(&self, field: &OwnedField) -> DbResult<bool> {
        self.db.delete_field(&field.row().id)
    }

    /// Returns time entry `id` when the account logged it.
    pub fn entry(&self, id: &str) -> DbResult<Option<OwnedEntry>> {
        let txn = self.db.read()?;
        let entry = read_in::<TimeEntryRow>(&txn, TIME_ENTRIES, id)?
            .filter(|entry| entry.account_id == self.account.as_str());
        Ok(entry.map(Owned))
    }

    /// Returns a task's time entries, newest first; empty for a task the account does not own.
    pub fn list_entries(&self, task_id: &str) -> DbResult<Vec<TimeEntryRow>> {
        if self.task(task_id)?.is_none() {
            return Ok(Vec::new());
        }
        self.db.list_entries(task_id)
    }

    /// Returns every time entry the account logged, in no set order.
    pub fn all_entries(&self) -> DbResult<Vec<TimeEntryRow>> {
        self.db.account_entries(&self.account)
    }

    /// Returns the account's time entries of today, newest first.
    pub fn entries_today(&self) -> DbResult<Vec<TimeEntryRow>> {
        self.db.entries_today(&self.account, &Local)
    }

    /// Returns what the timer views show of each task among `ids` that sits on one of the
    /// account's boards, keyed by task id. Each task is read once and only for those fields, and
    /// an id of a missing task or another account's is left out.
    pub fn task_briefs<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> DbResult<HashMap<String, TaskBrief>> {
        let txn = self.db.read()?;
        let mut boards: HashMap<String, Option<String>> = HashMap::new();
        let mut seen = HashSet::new();
        let mut out = HashMap::new();
        for id in ids {
            if id.is_empty() || !seen.insert(id) {
                continue;
            }
            let Some(mut brief) = read_in::<TaskBrief>(&txn, TASKS, id)? else {
                continue;
            };
            let board = match boards.get(&brief.column_id) {
                Some(board) => board.clone(),
                None => {
                    let column = read_in::<ColumnRow>(&txn, COLUMNS, &brief.column_id)?;
                    let board = match column {
                        Some(column) if owns_board_in(&txn, &self.account, &column.board_id)? => {
                            Some(column.board_id)
                        }
                        _ => None,
                    };
                    boards.insert(brief.column_id.clone(), board.clone());
                    board
                }
            };
            if let Some(board_id) = board {
                brief.board_id = board_id;
                out.insert(id.to_string(), brief);
            }
        }
        Ok(out)
    }

    /// Logs manual minutes on a task and its history event in one write.
    pub fn log_task_time(
        &self,
        task: &OwnedTask,
        minutes: i64,
        note: &str,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        self.db.log_task_time(&task.row().id, minutes, note, actor)
    }

    /// Creates a time entry for the account; [`None`] when its task is gone or another account's.
    pub fn create_entry_full(&self, entry: NewTimeEntry<'_>) -> DbResult<Option<String>> {
        self.db.create_entry_full(&self.account, entry)
    }

    /// Creates a manual time entry on a task with an explicit start time.
    pub fn create_entry_at(
        &self,
        task: &OwnedTask,
        minutes: i64,
        note: &str,
        started_at: &str,
        labels: &[String],
        created_by: &str,
    ) -> DbResult<Option<String>> {
        self.db.create_entry_at(
            &task.row().id,
            minutes,
            note,
            started_at,
            labels,
            created_by,
        )
    }

    /// Updates a time entry, moving it to `task` or making it task-less with [`None`].
    pub fn update_entry(
        &self,
        entry: &OwnedEntry,
        task: Option<&OwnedTask>,
        seconds: i64,
        note: &str,
        started_at: &str,
        labels: &[String],
    ) -> DbResult<bool> {
        let task_id = task.map_or("", |task| task.row().id.as_str());
        self.db
            .update_entry(&entry.row().id, task_id, seconds, note, started_at, labels)
    }

    /// Deletes a time entry.
    pub fn delete_entry(&self, entry: &OwnedEntry) -> DbResult<bool> {
        self.db.delete_entry(&entry.row().id)
    }

    /// Returns the account's running timer, if one runs.
    pub fn active_timer(&self) -> DbResult<Option<ActiveTimer>> {
        self.db.active_timer(&self.account)
    }

    /// Stores the account's running timer.
    pub fn set_active_timer(&self, timer: &ActiveTimer) -> DbResult<()> {
        self.db.set_active_timer(&self.account, timer)
    }

    /// Stops the account's timer without logging it.
    pub fn clear_active_timer(&self) -> DbResult<()> {
        self.db.clear_active_timer(&self.account)
    }

    /// Begins a new timer session and returns its id.
    pub fn begin_session(&self) -> u64 {
        self.db.begin_session(&self.account)
    }

    /// Returns the running session's id; [`None`] when this process did not start it.
    pub fn current_session(&self) -> Option<u64> {
        self.db.current_session(&self.account)
    }

    /// Holds the account's timer gate until dropped, so a start, stop, retarget or status read
    /// sees the stored timer and its session id from one request. Not reentrant; the guard must
    /// not outlive an `.await`.
    pub fn lock_timer(&self) -> MutexGuard<'_, ()> {
        self.timer_gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns the account's API tokens, oldest first.
    pub fn list_api_tokens(&self) -> DbResult<Vec<ApiTokenRecord>> {
        Ok(self
            .db
            .list_api_tokens()?
            .into_iter()
            .filter(|record| record.account_id == self.account.as_str())
            .collect())
    }

    /// Mints and stores an API token for the account, returning its record and the raw token,
    /// which is never stored.
    pub fn create_api_token(
        &self,
        name: &str,
        scope: AuthScope,
        expires_at: Option<i64>,
    ) -> DbResult<(ApiTokenRecord, String)> {
        let (record, raw) = new_api_token_record(&self.account, name, scope, expires_at);
        self.db.create_api_token(&record)?;
        Ok((record, raw))
    }

    /// Revokes one of the account's tokens. Returns false when it holds no such token.
    pub fn delete_api_token(&self, id: &str) -> DbResult<bool> {
        self.db.delete_api_token(&self.account, id)
    }

    /// Returns the account's passkeys, oldest first.
    pub fn passkeys(&self) -> DbResult<Vec<PasskeyRow>> {
        self.db.passkeys_of(&self.account)
    }

    /// Stores another passkey for the account, unless its credential is registered already or the
    /// account holds its most passkeys.
    pub fn add_passkey(&self, passkey: &PasskeyRow) -> DbResult<PasskeyAddOutcome> {
        if passkey.user_id != self.account.as_str() {
            return Err("a passkey for another account".into());
        }
        self.db.add_passkey(passkey)
    }

    /// Removes one of the account's passkeys, refusing to remove its last one, and ends every
    /// session of the account except `keep`.
    pub fn delete_passkey(&self, id: &str, keep: Option<&str>) -> DbResult<PasskeyDeleteOutcome> {
        self.db.delete_passkey(&self.account, id, keep)
    }

    /// Ends every session of the account except `keep`, and returns how many it ended.
    pub fn end_other_sessions(&self, keep: Option<&str>) -> DbResult<u64> {
        self.db.end_other_sessions(&self.account, keep)
    }

    /// Returns the account's recovery code row, absent until it gets one.
    pub fn recovery_code(&self) -> DbResult<Option<RecoveryCodeRow>> {
        self.db.recovery_code(&self.account)
    }

    /// Replaces the account's recovery code.
    pub fn set_recovery_code(&self, row: &RecoveryCodeRow) -> DbResult<()> {
        self.db.set_recovery_code(&self.account, row)
    }

    /// Deletes the account and every row it owns in one transaction, refusing the admin's while
    /// other accounts exist.
    pub fn delete_account(&self) -> DbResult<AccountDeleteOutcome> {
        self.db.delete_account(&self.account)
    }

    /// Returns the live, pending device request with this canonical user code.
    pub fn pending_device_auth(
        &self,
        user_code: &str,
        now: i64,
    ) -> DbResult<Option<DeviceAuthRow>> {
        self.db.pending_device_auth(user_code, now)
    }

    /// Returns whether this canonical user code belongs to a device request that has not expired.
    pub fn device_code_known(&self, user_code: &str, now: i64) -> DbResult<bool> {
        self.db.device_code_known(user_code, now)
    }

    /// Approves a pending device request for the account, or denies it when `grant` is [`None`].
    pub fn decide_device_auth(
        &self,
        user_code: &str,
        grant: Option<&DeviceGrant>,
        now: i64,
    ) -> DbResult<bool> {
        self.db
            .decide_device_auth(&self.account, user_code, grant, now)
    }
}
