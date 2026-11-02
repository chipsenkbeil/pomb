use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{FromRequestParts, MatchedPath};
use axum::http::request::Parts;
use axum::response::Response;
use chrono::{Local, Utc};
use redb::{ReadTransaction, ReadableTable};
use serde::de::DeserializeOwned;
use tokio::sync::watch;

use super::{
    AccountDeleteOutcome, AppState, BIN, BOARDS, BOARDS_BY_OWNER, BoardImport, BoardSnapshot,
    COLUMNS, Change, ChangeOutcome, ClaimOutcome, ClaimRequest, ColorDeleteOutcome,
    ColumnDeleteOutcome, ColumnKind, ColumnPatch, CommentWrite, CopyTasks, Db, DbResult,
    DeviceGrant, EventRead, FIELD_DEFS, FeedAdd, FencedWrite, FieldCreateOutcome, Incoming,
    LINKS_BY_TARGET, LinkAddOutcome, LinkDeleteOutcome, MoveGuards, MoveOutcome, NewComment,
    NewTaskPlacement, NewTimeEntry, NewWebhook, NotificationPage, PasskeyAddOutcome,
    PasskeyDeleteOutcome, PurgeOutcome, Reads, RestoreOutcome, Rows, SWIMLANES, SessionEndOutcome,
    SessionListing, SessionToken, SwimlaneDeleteOutcome, SwimlaneMergeOutcome, SwimlanePatch,
    SwimlaneUpdateOutcome, TASK_COLORS, TASKS, TIME_ENTRIES, TaskContext, TemplateDeleteOutcome,
    UmbrellaChange, WEBHOOKS, WebhookPatch, first_board, index_in, link_event_target,
    links_blocked_in, no_signed_in_account, owns_board_in, read_in, redacted_history,
};
use crate::auth::{
    AuthUser, TokenGrant, account_pending, new_api_token_record, required_permission,
};
use crate::models::{
    AccountId, AccountStatus, ActiveTimer, Actor, ApiTokenRecord, BinRow, BoardAccess, BoardRow,
    BoardTemplateRow, ColorRow, ColumnRow, DeviceAuthRow, EventKind, FieldDefRow, LinkKind,
    PasskeyRow, PruneNotice, RecoveryCodeRow, Settings, SwimlaneRow, TaskAttachment, TaskBrief,
    TaskCard, TaskComment, TaskEvent, TaskHead, TaskLink, TaskRow, TimeEntryRow, UserRow,
    WebhookRow,
};
use crate::permissions::Permission;

/// A board of the account that lies outside the token's assigned set, which the API answers with
/// a 403 `permission_denied` naming the board.
#[derive(Clone, Debug)]
pub struct BoardDenied {
    /// The board the token is not assigned to.
    pub board_id: String,
    /// The permission the refused route needs.
    pub permission: Permission,
}

impl fmt::Display for BoardDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "token is not assigned to board {}", self.board_id)
    }
}

impl std::error::Error for BoardDenied {}

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

/// A webhook on a board the bound account owns.
pub type OwnedWebhook = Owned<WebhookRow>;

/// A time entry the bound account logged.
pub type OwnedEntry = Owned<TimeEntryRow>;

/// A template the bound account may use: a built-in one, or one it saved.
pub type OwnedTemplate = Owned<BoardTemplateRow>;

/// A deleted task in the recycle bin of a board the bound account owns.
#[derive(Clone, Debug)]
pub struct OwnedBinned {
    /// The stored row.
    row: BinRow,
}

impl OwnedBinned {
    /// Returns the task's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.row.task.id
    }

    /// Returns the id of the board the task was deleted from.
    #[must_use]
    pub fn board_id(&self) -> &str {
        &self.row.board_id
    }

    /// Returns the task as it was when it was deleted.
    #[must_use]
    pub fn task(&self) -> &TaskRow {
        &self.row.task
    }
}

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

/// A child task as its umbrella lists it.
#[derive(Clone, Debug)]
pub struct ChildRef {
    /// The child's id.
    pub id: String,
    /// The child's name.
    pub name: String,
    /// The board the child sits on.
    pub board_id: Option<String>,
    /// The name of that board.
    pub board_name: Option<String>,
    /// The name of the column the child sits in.
    pub column_name: String,
    /// Whether that column is a done column.
    pub done: bool,
}

/// A task another task is related to, as the Relations section lists it.
#[derive(Clone, Debug)]
pub struct RelationRef {
    /// The link that relates the two tasks.
    pub link_id: String,
    /// The relation as the listing task sees it.
    pub kind: LinkKind,
    /// The other task's id.
    pub id: String,
    /// The other task's name.
    pub name: String,
    /// The board the other task sits on.
    pub board_id: Option<String>,
    /// The name of the column the other task sits in.
    pub column_name: String,
    /// The other task's color id.
    pub color_id: Option<String>,
    /// The other task's legacy size.
    pub size: i64,
}

/// The umbrella a child is part of.
#[derive(Clone, Debug)]
pub struct UmbrellaRef {
    /// The umbrella's id.
    pub id: String,
    /// The umbrella's name.
    pub name: String,
    /// The board the umbrella sits on.
    pub board_id: Option<String>,
    /// The umbrellas from the root down to this one, which comes last.
    pub path: Vec<UmbrellaPart>,
}

/// One umbrella on the path above a child.
#[derive(Clone, Debug)]
pub struct UmbrellaPart {
    /// The umbrella's id.
    pub id: String,
    /// The umbrella's name.
    pub name: String,
    /// The board the umbrella sits on.
    pub board_id: Option<String>,
    /// The name of that board.
    pub board_name: Option<String>,
}

/// Returns the id of the umbrella the `parent` link of `head` names, if it has one.
fn parent_target(head: &TaskHead) -> Option<String> {
    head.links
        .iter()
        .find(|link| link.kind == LinkKind::Parent)
        .map(|link| link.target_id.clone())
}

/// The reads of the tasks links name, made through one read transaction for one request, which
/// read each task, column and board once, however many links name it. Only the tasks and boards
/// the bound account owns and its token reaches are found.
pub struct LinkReads {
    /// The read transaction every read goes through.
    txn: ReadTransaction,
    /// The database, which records that a board was used.
    db: Db,
    /// The account whose tasks and boards are found.
    account: AccountId,
    /// The boards found among the account's.
    access: BoardAccess,
    /// Whether reading a task on a board records that the board was used.
    record_use: bool,
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
            Some(column)
                if self.access.allows(&column.board_id)
                    && owns_board_in(&self.txn, &self.account, &column.board_id)? =>
            {
                if self.record_use {
                    self.db
                        .touch_board(&column.board_id, Utc::now().timestamp());
                }
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
            .filter(|board| board.owner_id == self.account.as_str() && self.access.allows(id))
            .map(|board| board.name);
        self.board_names.insert(id.to_string(), name.clone());
        Ok(name)
    }

    /// Returns the name of board `id` when one is given and the account owns it.
    fn named_board(&mut self, id: Option<&str>) -> DbResult<Option<String>> {
        match id {
            Some(id) => self.board_name(id),
            None => Ok(None),
        }
    }

    /// Returns task `id`'s links as seen from it, as [`Db::list_task_links`] lists them, without
    /// a link to a task outside the token's set. A task the account does not own has none.
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
                    kind: link.kind.seen_by_target(),
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
        if matches!(self.access, BoardAccess::Only(_)) {
            links.retain(|link| link.board_id.is_some());
        }
        Ok(links)
    }

    /// Returns whether the token may read `event`: a link event that names a task outside its set
    /// stays hidden.
    fn shows_event(&mut self, event: &TaskEvent) -> DbResult<bool> {
        match link_event_target(event) {
            Some(target) if matches!(self.access, BoardAccess::Only(_)) => {
                Ok(self.head(target)?.is_some())
            }
            _ => Ok(true),
        }
    }

    /// Returns whether column `id` sits on a board of the account that lies outside the token's
    /// set.
    fn column_outside_set(&mut self, id: &str) -> DbResult<bool> {
        let Some(column) = read_in::<ColumnRow>(&self.txn, COLUMNS, id)? else {
            return Ok(false);
        };
        Ok(!self.access.allows(&column.board_id)
            && owns_board_in(&self.txn, &self.account, &column.board_id)?)
    }

    /// Returns the children named by the `child` links among `links`, in link order. A child the
    /// account does not own is left out.
    pub fn children(&mut self, links: &[TaskLink]) -> DbResult<Vec<ChildRef>> {
        let mut children = Vec::new();
        for link in links.iter().filter(|link| link.kind == LinkKind::Child) {
            let Some(head) = self.head(&link.target_id)?.cloned() else {
                continue;
            };
            let Some(column) = read_in::<ColumnRow>(&self.txn, COLUMNS, &head.column_id)? else {
                continue;
            };
            let board_name = self.named_board(link.board_id.as_deref())?;
            children.push(ChildRef {
                id: head.id,
                name: head.name,
                board_id: link.board_id.clone(),
                board_name,
                column_name: column.name,
                done: column.is_done,
            });
        }
        Ok(children)
    }

    /// Returns the tasks the `blocked_by`, `blocks` and `related` links among `links` name, in
    /// link order. A task the account does not own is left out.
    pub fn relations(&mut self, links: &[TaskLink]) -> DbResult<Vec<RelationRef>> {
        let mut relations = Vec::new();
        for link in links.iter().filter(|link| {
            matches!(
                link.kind,
                LinkKind::BlockedBy | LinkKind::Blocks | LinkKind::Related
            )
        }) {
            let Some(head) = self.head(&link.target_id)?.cloned() else {
                continue;
            };
            let Some(column) = read_in::<ColumnRow>(&self.txn, COLUMNS, &head.column_id)? else {
                continue;
            };
            relations.push(RelationRef {
                link_id: link.id.clone(),
                kind: link.kind,
                id: head.id,
                name: head.name,
                board_id: link.board_id.clone(),
                column_name: column.name,
                color_id: head.color_id,
                size: head.size,
            });
        }
        Ok(relations)
    }

    /// Returns the umbrella named by the `parent` link among `links`, when the account owns it.
    pub fn umbrella(&mut self, links: &[TaskLink]) -> DbResult<Option<UmbrellaRef>> {
        let Some(link) = links.iter().find(|link| link.kind == LinkKind::Parent) else {
            return Ok(None);
        };
        let Some(head) = self.head(&link.target_id)?.cloned() else {
            return Ok(None);
        };
        let board_name = self.named_board(link.board_id.as_deref())?;
        let mut path = vec![UmbrellaPart {
            id: head.id.clone(),
            name: head.name.clone(),
            board_id: link.board_id.clone(),
            board_name,
        }];
        let mut seen = HashSet::from([head.id.clone()]);
        let mut above_id = parent_target(&head);
        while let Some(id) = above_id.take() {
            if !seen.insert(id.clone()) {
                break;
            }
            let Some(ancestor) = self.head(&id)?.cloned() else {
                break;
            };
            let board_id = self.task_board(&id)?;
            let board_name = self.named_board(board_id.as_deref())?;
            path.push(UmbrellaPart {
                board_id,
                board_name,
                id: ancestor.id.clone(),
                name: ancestor.name.clone(),
            });
            above_id = parent_target(&ancestor);
        }
        path.reverse();
        Ok(Some(UmbrellaRef {
            id: head.id,
            name: head.name,
            board_id: link.board_id.clone(),
            path,
        }))
    }

    /// Returns true when a `blocked_by` link among `links` names a task in a column that is not
    /// done.
    pub fn blocked(&mut self, links: &[TaskLink]) -> DbResult<bool> {
        if matches!(self.access, BoardAccess::All) {
            return links_blocked_in(&self.txn, links, &mut self.open);
        }
        let mut visible = Vec::new();
        for link in links {
            if self.head(&link.target_id)?.is_some() {
                visible.push(link.clone());
            }
        }
        links_blocked_in(&self.txn, &visible, &mut self.open)
    }
}

/// Data access bound to one account.
///
/// Every getter checks, in the read transaction that loads the row, that the account owns it, and
/// answers [`None`] for another account's row exactly as for a missing one. A row on a board
/// outside the token's set is refused with [`BoardDenied`]. Lists hold only the account's rows on
/// boards in that set. Every mutation takes an [`Owned`] proof or an [`OwnedTask`].
#[derive(Clone, Debug)]
pub struct AccountDb {
    /// The database the handle reads and writes.
    db: Db,
    /// The account every row the handle loads belongs to.
    account: AccountId,
    /// The boards of the account the handle reaches.
    boards: BoardAccess,
    /// The permission a [`BoardDenied`] names.
    permission: Permission,
    /// The API token behind the handle, whose set a board it creates joins.
    token_id: Option<String>,
    /// Serializes this account's timer reads and writes.
    timer_gate: Arc<Mutex<()>>,
}

impl Db {
    /// Returns the handle bound to `account`, which reaches every board it owns.
    #[must_use]
    pub fn account(&self, account: &AccountId) -> AccountDb {
        AccountDb {
            timer_gate: self.timer_gate(account),
            db: self.clone(),
            account: account.clone(),
            boards: BoardAccess::All,
            permission: Permission::BoardsRead,
            token_id: None,
        }
    }

    /// Returns the handle bound to the account of `user`, held to the boards of its credential.
    /// A refusal names `permission`.
    #[must_use]
    pub fn account_of(&self, user: &AuthUser, permission: Permission) -> AccountDb {
        AccountDb {
            boards: user.boards().clone(),
            permission,
            token_id: user.token_id.clone(),
            ..self.account(user.account_id())
        }
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AccountDb {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        match parts.extensions.get::<AuthUser>() {
            Some(user) if user.status == AccountStatus::Active => {
                let permission = parts
                    .extensions
                    .get::<MatchedPath>()
                    .and_then(|path| required_permission(&parts.method, path.as_str()))
                    .unwrap_or(Permission::BoardsRead);
                Ok(state.db.account_of(user, permission))
            }
            Some(_) => Err(account_pending()),
            None => Err(no_signed_in_account()),
        }
    }
}

impl AccountDb {
    /// Maximum stored attachment size: 50 MiB.
    pub const MAX_ATTACHMENT_BYTES: u64 = Db::MAX_ATTACHMENT_BYTES;
    /// Maximum attachment size sent as base64 in a JSON body: 5 MiB.
    pub const MAX_JSON_ATTACHMENT_BYTES: u64 = Db::MAX_JSON_ATTACHMENT_BYTES;
    /// Maximum serialized size of a task's metadata object: 64 KiB.
    pub const MAX_METADATA_BYTES: usize = Db::MAX_METADATA_BYTES;

    /// Returns the most bytes the admin lets one attachment file hold.
    pub fn max_attachment_bytes(&self) -> DbResult<u64> {
        Ok(self.db.instance_settings()?.disk.max_attachment_bytes())
    }

    /// Returns the id of the account the handle is bound to.
    #[must_use]
    pub fn account_id(&self) -> &AccountId {
        &self.account
    }

    /// Returns whether the handle holds its boards to an assigned set.
    fn restricted(&self) -> bool {
        matches!(self.boards, BoardAccess::Only(_))
    }

    /// Returns [`BoardDenied`] when board `board_id` lies outside the token's set.
    fn check_granted(&self, board_id: &str) -> DbResult<()> {
        if self.boards.allows(board_id) {
            return Ok(());
        }
        Err(Box::new(BoardDenied {
            board_id: board_id.to_string(),
            permission: self.permission,
        }))
    }

    /// Returns whether the account owns board `board_id`, refusing a board of the account outside
    /// the token's set with [`BoardDenied`].
    fn reaches(&self, txn: &ReadTransaction, board_id: &str) -> DbResult<bool> {
        if !owns_board_in(txn, &self.account, board_id)? {
            return Ok(false);
        }
        self.check_granted(board_id)?;
        Ok(true)
    }

    /// Returns whether task `id` sits on a board in the token's set.
    fn task_granted_in(&self, txn: &ReadTransaction, id: &str) -> DbResult<bool> {
        let Some(head) = read_in::<TaskHead>(txn, TASKS, id)? else {
            return Ok(false);
        };
        let Some(column) = read_in::<ColumnRow>(txn, COLUMNS, &head.column_id)? else {
            return Ok(false);
        };
        Ok(self.boards.allows(&column.board_id))
    }

    /// Refuses with [`BoardDenied`] when task `id` of the account sits outside the token's set.
    fn check_task_granted(&self, id: &str) -> DbResult<()> {
        if self.restricted() {
            self.owned_head(id)?;
        }
        Ok(())
    }

    /// Refuses with [`BoardDenied`] when task `id` links with a task outside the token's set,
    /// which deleting task `id` would rewrite.
    fn check_links_granted(&self, id: &str) -> DbResult<()> {
        if self.restricted() {
            for link in self.db.list_task_links(id)?.unwrap_or_default() {
                self.check_task_granted(&link.target_id)?;
            }
        }
        Ok(())
    }

    /// Runs `read` when the account owns board `board_id`, and returns the empty value otherwise.
    fn on_own_board<T: Default>(
        &self,
        board_id: &str,
        read: impl FnOnce() -> DbResult<T>,
    ) -> DbResult<T> {
        let txn = self.db.read()?;
        if self.reaches(&txn, board_id)? {
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
        if !self.reaches(&txn, board_of(&row))? {
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
        let mut boards = index_in(&self.db.read()?, BOARDS_BY_OWNER, self.account.as_str())?;
        boards.retain(|id| self.boards.allows(id));
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

    /// Edits the account's settings with `edit` in one write and returns what `edit` returns.
    pub fn update_settings_with<R>(&self, edit: impl FnOnce(&mut Settings) -> R) -> DbResult<R> {
        self.db.update_settings_with(&self.account, edit)
    }

    /// Returns board `id` when the account owns it.
    pub fn board(&self, id: &str) -> DbResult<Option<OwnedBoard>> {
        let txn = self.db.read()?;
        let board = read_in::<BoardRow>(&txn, BOARDS, id)?
            .filter(|board| board.owner_id == self.account.as_str());
        if board.is_some() {
            self.check_granted(id)?;
            self.mark_used(id);
        }
        Ok(board.map(Owned))
    }

    /// Returns a snapshot of board `id` that reads through one transaction, when the account owns
    /// it.
    pub fn board_snapshot(&self, id: &str) -> DbResult<Option<BoardSnapshot>> {
        let snapshot = self.search_snapshot(id)?;
        if snapshot.is_some() {
            self.mark_used(id);
        }
        Ok(snapshot)
    }

    /// Returns a snapshot of board `id` for a search, when the account owns it. Unlike
    /// [`AccountDb::board_snapshot`], reading it does not count as a use of the board.
    pub fn search_snapshot(&self, id: &str) -> DbResult<Option<BoardSnapshot>> {
        let txn = self.db.read()?;
        if !self.reaches(&txn, id)? {
            return Ok(None);
        }
        BoardSnapshot::open(
            txn,
            self.db.files.clone(),
            &self.account,
            self.boards.clone(),
            id,
        )
    }

    /// Returns whether the account owns board `id`.
    pub fn board_exists(&self, id: &str) -> DbResult<bool> {
        Ok(self.board(id)?.is_some())
    }

    /// Returns the ids among `boards` that name a board of the account outside the token's set,
    /// which a handle with no set never has.
    pub fn boards_outside_set(&self, boards: &[String]) -> DbResult<Vec<String>> {
        if !self.restricted() {
            return Ok(Vec::new());
        }
        let owned: HashSet<String> = self
            .db
            .owner_boards(&self.account)?
            .into_iter()
            .map(|board| board.id)
            .collect();
        Ok(boards
            .iter()
            .filter(|id| owned.contains(*id) && !self.boards.allows(id))
            .cloned()
            .collect())
    }

    /// Returns the account's boards in the token's set, ordered by position (then id for
    /// stability).
    pub fn list_boards(&self) -> DbResult<Vec<BoardRow>> {
        let mut boards = self.db.owner_boards(&self.account)?;
        boards.retain(|board| self.boards.allows(&board.id));
        Ok(boards)
    }

    /// Returns the id of the lowest-position unarchived board of [`AccountDb::list_boards`], or of
    /// the lowest-position archived one when every board is archived.
    pub fn first_board_id(&self) -> DbResult<Option<String>> {
        Ok(first_board(&self.list_boards()?))
    }

    /// Trims a board name and checks it is 1-100 characters long.
    pub fn validate_board_name(name: &str) -> Result<&str, &'static str> {
        Db::validate_board_name(name)
    }

    /// Creates a board for the account with the standard color palette, Brown and White off,
    /// which joins the set of the token behind the handle.
    pub fn create_board(&self, name: &str) -> DbResult<String> {
        self.db
            .create_board_by(&self.account, name, self.joining_token())
    }

    /// Returns the id of the token behind the handle, empty when there is none.
    fn joining_token(&self) -> &str {
        self.token_id.as_deref().unwrap_or_default()
    }

    /// Creates a board for the account as a person gets one without picking a template: To-do,
    /// Doing and Done, the standard palette with Brown and White off, which joins the set of the
    /// token behind the handle. The tasks it starts with are `actor`'s.
    pub fn create_simple_board(&self, name: &str, actor: Actor<'_>) -> DbResult<String> {
        self.db
            .create_simple_board_by(&self.account, name, self.joining_token(), actor)
    }

    /// Returns the directory that holds the database file, where an import spools its files.
    #[must_use]
    pub fn data_dir(&self) -> Option<PathBuf> {
        let parent = self.db.path.parent()?;
        Some(if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        })
    }

    /// Stores `import` as a new board of the account, which joins the set of the token behind the
    /// handle, and returns the board's id. A quota or text cap the import breaks refuses all of it
    /// and writes nothing. The tasks are `actor`'s.
    pub fn import_board(&self, import: BoardImport, actor: Actor<'_>) -> DbResult<String> {
        self.db
            .import_board(&self.account, import, self.joining_token(), actor)
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

    /// Adds a calendar feed named `label` with the bearer token `token` to a board.
    pub fn add_board_feed(
        &self,
        board: &OwnedBoard,
        label: &str,
        token: &str,
    ) -> DbResult<FeedAdd> {
        self.db.add_board_feed(&board.row().id, label, token)
    }

    /// Removes a calendar feed from a board, so its URL stops serving at once. Returns false when
    /// the board has no such feed.
    pub fn remove_board_feed(&self, board: &OwnedBoard, feed_id: &str) -> DbResult<bool> {
        self.db.remove_board_feed(&board.row().id, feed_id)
    }

    /// Deletes a board and everything in it, refusing the account's only or last unarchived board.
    pub fn delete_board(&self, board: &OwnedBoard) -> DbResult<()> {
        if self.restricted() {
            for id in self.db.board_task_ids(&board.row().id)? {
                self.check_links_granted(&id)?;
            }
        }
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
        if self.restricted() {
            let mut reads = self.link_reads()?;
            let mut links = Vec::new();
            for link in row.links {
                if reads.head(&link.target_id)?.is_some() {
                    links.push(link);
                }
            }
            row.links = links;
            row.history = self.visible_history(&row.history)?;
        }
        Ok(Some(row))
    }

    /// Returns the distinct labels used on board `board_id`, sorted; empty for another account's
    /// board.
    pub fn board_labels(&self, board_id: &str) -> DbResult<Vec<String>> {
        self.on_own_board(board_id, || self.db.board_labels(board_id))
    }

    /// Returns the events of `board` after cursor `after` whose kind is in `kinds`, at most
    /// `limit` of them. A move from a board outside the token's set does not name the column it
    /// left.
    pub fn board_events(
        &self,
        board: &OwnedBoard,
        after: u64,
        kinds: &[EventKind],
        limit: usize,
    ) -> DbResult<EventRead> {
        let mut read = self.db.board_events(&board.row().id, after, kinds, limit)?;
        if let (EventRead::Page(page), true) = (&mut read, self.restricted()) {
            let mut reads = self.link_reads()?;
            for (_, event) in &mut page.events {
                let outside = match event.from_column_id.as_deref() {
                    Some(column) => reads.column_outside_set(column)?,
                    None => false,
                };
                if outside {
                    event.from_column_id = None;
                }
            }
        }
        Ok(read)
    }

    /// Returns the cursor of the newest event of `board`, or 0 when it has none.
    pub fn board_event_cursor(&self, board: &OwnedBoard) -> DbResult<u64> {
        self.db.board_event_cursor(&board.row().id)
    }

    /// Returns a receiver that sees every commit that wrote events of `board` from now on.
    #[must_use]
    pub fn board_feed(&self, board: &OwnedBoard) -> watch::Receiver<()> {
        self.db.board_feed(&board.row().id)
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

    /// Adds a swimlane to a board at `position`, or at the end when none.
    pub fn create_swimlane_at(
        &self,
        board: &OwnedBoard,
        name: &str,
        position: Option<i64>,
    ) -> DbResult<String> {
        self.db.create_swimlane_at(&board.row().id, name, position)
    }

    /// Applies the fields set in `patch` to a swimlane.
    pub fn update_swimlane(
        &self,
        lane: &OwnedSwimlane,
        patch: &SwimlanePatch<'_>,
    ) -> DbResult<SwimlaneUpdateOutcome> {
        self.db.update_swimlane(&lane.row().id, patch)
    }

    /// Merges a board's swimlanes into its main lane, recording `actor` on each moved task.
    pub fn merge_swimlanes(
        &self,
        board: &OwnedBoard,
        actor: Actor<'_>,
    ) -> DbResult<SwimlaneMergeOutcome> {
        self.db.merge_swimlanes(&board.row().id, actor)
    }

    /// Moves a swimlane within its board.
    pub fn move_swimlane(&self, lane: &OwnedSwimlane, position: i64) -> DbResult<bool> {
        self.db.move_swimlane(&lane.row().id, position)
    }

    /// Deletes a swimlane unless it is its board's last, moving its tasks into a neighboring lane.
    pub fn delete_swimlane(&self, lane: &OwnedSwimlane) -> DbResult<SwimlaneDeleteOutcome> {
        self.db.delete_swimlane(&lane.row().id)
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

    /// Returns template `id` when it is built in or the account saved it. A saved template may
    /// hold the structure of any board, so a token with a set sees only built-in ones.
    pub fn template(&self, id: &str) -> DbResult<Option<OwnedTemplate>> {
        let template = self.db.template(id)?;
        Ok(template.filter(|t| self.sees_template(t)).map(Owned))
    }

    /// Returns whether the handle sees `template`: a built-in one, or one the account saved when
    /// the token has no set.
    fn sees_template(&self, template: &BoardTemplateRow) -> bool {
        template
            .owner_id
            .as_deref()
            .is_none_or(|owner| owner == self.account.as_str() && !self.restricted())
    }

    /// Returns the templates the handle sees, built-ins first, then by name.
    pub fn list_templates(&self) -> DbResult<Vec<BoardTemplateRow>> {
        let mut templates = self.db.list_templates(&self.account)?;
        templates.retain(|template| self.sees_template(template));
        Ok(templates)
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

    /// Builds a new board for the account that copies `source`, which joins the set of the token
    /// behind the handle. The tasks it starts with are `actor`'s.
    pub fn copy_board(
        &self,
        source: &OwnedBoard,
        name: &str,
        tasks: CopyTasks,
        actor: Actor<'_>,
    ) -> DbResult<Option<String>> {
        self.db.copy_board_by(
            &self.account,
            &source.row().id,
            name,
            tasks,
            self.joining_token(),
            actor,
        )
    }

    /// Builds a new board for the account from a template, which joins the set of the token behind
    /// the handle. The tasks it starts with are `actor`'s.
    pub fn apply_template(
        &self,
        template: &OwnedTemplate,
        name: &str,
        actor: Actor<'_>,
    ) -> DbResult<Option<String>> {
        self.db.apply_template_by(
            &self.account,
            &template.row().id,
            name,
            self.joining_token(),
            actor,
        )
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
        if !self.reaches(&txn, &column.board_id)? {
            return Ok(None);
        }
        self.mark_used(&column.board_id);
        let task = OwnedTask {
            row,
            board_id: column.board_id,
        };
        Ok(Some(task))
    }

    /// Returns the board and id of the one task of the account whose id starts with `prefix`, or
    /// [`None`] when none or several do. A task on a board outside the token's set is refused with
    /// [`BoardDenied`].
    pub fn task_by_prefix(&self, prefix: &str) -> DbResult<Option<(String, String)>> {
        let txn = self.db.read()?;
        let tasks = txn.rows(TASKS)?;
        let mut found = None;
        for entry in tasks.range(prefix..)? {
            let (key, value) = entry?;
            if !key.value().starts_with(prefix) {
                break;
            }
            let head: TaskHead = serde_json::from_slice(value.value())?;
            let Some(column) = read_in::<ColumnRow>(&txn, COLUMNS, &head.column_id)? else {
                continue;
            };
            let board_id = column.board_id;
            if !owns_board_in(&txn, &self.account, &board_id)? {
                continue;
            }
            if found.replace((board_id, key.value().to_owned())).is_some() {
                return Ok(None);
            }
        }
        if let Some((board_id, _)) = &found {
            self.check_granted(board_id)?;
            self.mark_used(board_id);
        }
        Ok(found)
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
        if !self.reaches(&txn, &column.board_id)? {
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
        actor: Actor<'_>,
    ) -> DbResult<Result<String, String>> {
        let row = TaskRow {
            column_id: column.row().id.clone(),
            ..row
        };
        self.db.insert_task_checked(row, placement, actor)
    }

    /// Runs `f` on a task in one write transaction, storing the row when `f` reports a change. Any
    /// board event the change makes is `actor`'s.
    pub fn apply_task_change<R, F>(
        &self,
        task: &OwnedTask,
        actor: Actor<'_>,
        f: F,
    ) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow) -> (R, bool),
    {
        self.db.apply_task_change(&task.row().id, actor, f)
    }

    /// Runs `f` on a task with its board context read in the same write transaction.
    pub fn apply_task_change_in_board<R, F>(
        &self,
        task: &OwnedTask,
        actor: Actor<'_>,
        f: F,
    ) -> DbResult<Option<R>>
    where
        F: FnOnce(&mut TaskRow, &TaskContext) -> (R, bool),
    {
        self.db.apply_task_change_in_board(&task.row().id, actor, f)
    }

    /// Moves a task into `column` after the checks in `guards`, all in one write transaction.
    pub fn move_task_logged(
        &self,
        task: &OwnedTask,
        column: &OwnedColumn,
        swimlane_id: Option<&str>,
        position: f64,
        guards: MoveGuards<'_>,
        actor: Actor<'_>,
    ) -> DbResult<MoveOutcome> {
        self.db.move_task_logged(
            &task.row().id,
            &column.row().id,
            swimlane_id,
            position,
            guards,
            actor,
        )
    }

    /// Runs `f` on each of `tasks` in one write transaction, writing nothing when it refuses one
    /// or one is missing.
    pub fn apply_tasks_change<E, F>(
        &self,
        tasks: &[OwnedTask],
        actor: Actor<'_>,
        f: F,
    ) -> DbResult<ChangeOutcome<E>>
    where
        F: FnMut(&mut TaskRow, &TaskContext) -> Result<(), E>,
    {
        let ids: Vec<String> = tasks.iter().map(|task| task.row().id.clone()).collect();
        self.db.apply_tasks_change(&ids, actor, f)
    }

    /// Moves `tasks` into `column` in the order given, all in one write transaction: when any
    /// task is missing, none moves.
    pub fn move_tasks_logged(
        &self,
        tasks: &[OwnedTask],
        column: &OwnedColumn,
        swimlane_id: Option<&str>,
        position: f64,
        actor: Actor<'_>,
    ) -> DbResult<MoveOutcome> {
        let ids: Vec<String> = tasks.iter().map(|task| task.row().id.clone()).collect();
        self.db
            .move_tasks_logged(&ids, &column.row().id, swimlane_id, position, actor)
    }

    /// Moves a task, its time entries and its attachments into the recycle bin and drops its
    /// links.
    pub fn delete_task(&self, task: &OwnedTask, actor: Actor<'_>) -> DbResult<bool> {
        self.check_links_granted(&task.row().id)?;
        self.db.delete_task(&task.row().id, actor)
    }

    /// Removes a task its own creation takes back, leaving nothing in the recycle bin.
    pub fn discard_task(&self, task: &OwnedTask, actor: Actor<'_>) -> DbResult<bool> {
        self.db.discard_task(&task.row().id, actor)
    }

    /// Returns the deleted tasks in the recycle bin of `board`, newest deletion first.
    pub fn binned_tasks(&self, board: &OwnedBoard) -> DbResult<Vec<BinRow>> {
        self.db.binned_tasks(&board.row().id)
    }

    /// Returns the deleted task `id` when the account owns the board it was deleted from.
    pub fn binned_task_board(&self, id: &str) -> DbResult<Option<OwnedBinned>> {
        let txn = self.db.read()?;
        let Some(row) = read_in::<BinRow>(&txn, BIN, id)? else {
            return Ok(None);
        };
        if !self.reaches(&txn, &row.board_id)? {
            return Ok(None);
        }
        self.mark_used(&row.board_id);
        Ok(Some(OwnedBinned { row }))
    }

    /// Deletes a deleted task for good when its board allows it.
    pub fn purge_binned_task(&self, binned: &OwnedBinned) -> DbResult<PurgeOutcome> {
        self.db.purge_binned_task(binned.id())
    }

    /// Puts a deleted task back as of `now` in Unix seconds.
    pub fn restore_task(
        &self,
        binned: &OwnedBinned,
        now: i64,
        actor: Actor<'_>,
    ) -> DbResult<RestoreOutcome> {
        self.db.restore_task(binned.id(), now, actor)
    }

    /// Notifies the account that `actor` deleted `task`, which is gone from the board by now.
    pub fn notify_task_deleted(&self, task: &OwnedTask, actor: Actor<'_>) -> DbResult<()> {
        self.db.notify(Change::TaskDeleted(task.row()), actor)
    }

    /// Notifies the account that `actor` renamed `column` to `name`.
    pub fn notify_column_renamed(
        &self,
        column: &OwnedColumn,
        name: &str,
        actor: Actor<'_>,
    ) -> DbResult<()> {
        let change = Change::ColumnRenamed {
            column: column.row(),
            name,
        };
        self.db.notify(change, actor)
    }

    /// Notifies the account that `actor` deleted `column`.
    pub fn notify_column_deleted(&self, column: &OwnedColumn, actor: Actor<'_>) -> DbResult<()> {
        self.db.notify(Change::ColumnDeleted(column.row()), actor)
    }

    /// Notifies the account that `actor` deleted `board`.
    pub fn notify_board_deleted(&self, board: &OwnedBoard, actor: Actor<'_>) -> DbResult<()> {
        self.db.notify(Change::BoardDeleted(board.row()), actor)
    }

    /// Returns up to `limit` of the account's notifications, newest first, older than the
    /// sequence number `before` when given.
    pub fn notifications(&self, before: Option<u64>, limit: usize) -> DbResult<NotificationPage> {
        self.db.notifications(&self.account, before, limit)
    }

    /// Marks one of the account's notifications read and returns how many remain unread, or
    /// [`None`] when the account has no such notification.
    pub fn mark_notification_read(&self, seq: u64) -> DbResult<Option<usize>> {
        self.db.mark_notification_read(&self.account, seq)
    }

    /// Marks every notification of the account read.
    pub fn mark_all_notifications_read(&self) -> DbResult<()> {
        self.db.mark_all_notifications_read(&self.account)
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

    /// Links `task` to `target`, both on the account's boards, recording the API `token_id` that
    /// asked.
    pub fn add_task_link(
        &self,
        task: &OwnedTask,
        kind: LinkKind,
        target: &OwnedTask,
        token_id: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<LinkAddOutcome> {
        self.db
            .add_task_link(&task.row().id, kind, &target.row().id, token_id, actor)
    }

    /// Moves `task` under `umbrella`, both on the account's boards, in one write.
    pub fn change_umbrella(
        &self,
        task: &OwnedTask,
        umbrella: &OwnedTask,
        actor: Actor<'_>,
    ) -> DbResult<UmbrellaChange> {
        let held = task.row().links.iter();
        if let Some(link) = held.clone().find(|link| link.kind == LinkKind::Parent) {
            self.check_task_granted(&link.target_id)?;
        }
        self.db
            .change_umbrella(&task.row().id, &umbrella.row().id, actor)
    }

    /// Returns the events of `history` the token may read.
    pub fn visible_history(&self, history: &[TaskEvent]) -> DbResult<Vec<TaskEvent>> {
        let mut reads = self.link_reads()?;
        let restricted = self.restricted();
        let mut shown = Vec::new();
        for event in history {
            if reads.shows_event(event)? {
                shown.push(event.clone());
            }
        }
        redacted_history(
            shown,
            |_| Ok(true),
            |column| {
                if restricted {
                    reads.column_outside_set(column)
                } else {
                    Ok(false)
                }
            },
        )
    }

    /// Returns a task's links as seen from it, without a link to a task outside the token's set.
    /// Returns [`None`] when the account does not own the task.
    pub fn list_task_links(&self, task_id: &str) -> DbResult<Option<Vec<TaskLink>>> {
        if self.owned_head(task_id)?.is_none() {
            return Ok(None);
        }
        if self.restricted() {
            return Ok(Some(self.link_reads()?.task_links(task_id)?));
        }
        self.db.list_task_links(task_id)
    }

    /// Returns the board a task sits on; [`None`] when the account does not own the task.
    pub fn task_board_id(&self, task_id: &str) -> DbResult<Option<String>> {
        Ok(self.owned_head(task_id)?.map(|(_, board_id)| board_id))
    }

    /// Deletes a link listed on a task, whichever task holds it. With an `owner`, only a link that
    /// API token created may go. A token with a set must reach both ends of the link.
    pub fn delete_task_link(
        &self,
        task: &OwnedTask,
        link_id: &str,
        owner: Option<&str>,
        actor: Actor<'_>,
    ) -> DbResult<LinkDeleteOutcome> {
        if self.restricted() {
            let links = self.db.list_task_links(&task.row().id)?.unwrap_or_default();
            if let Some(link) = links.iter().find(|link| link.id == link_id) {
                self.check_task_granted(&link.target_id)?;
            }
        }
        self.db
            .delete_task_link(&task.row().id, link_id, owner, actor)
    }

    /// Returns true when a `blocked_by` target of `task` sits in a column that is not done.
    pub fn task_blocked(&self, task: &TaskRow) -> DbResult<bool> {
        self.link_reads()?.blocked(&task.links)
    }

    /// Returns a check of whether a `blocked_by` link among the links it is handed names a task in
    /// a column that is not done. The check reads each named task once, however many lists name
    /// it.
    pub fn blocked_check(&self) -> DbResult<impl FnMut(&[TaskLink]) -> DbResult<bool> + use<>> {
        let mut reads = self.link_reads()?;
        Ok(move |links: &[TaskLink]| reads.blocked(links))
    }

    /// Returns [`LinkReads`] that find every task of the account, whatever the token's set, and
    /// record no board as used. Only the ids of what they find may leave the server, for a check
    /// that a link the token asks for would be refused.
    pub fn unrestricted_link_reads(&self) -> DbResult<LinkReads> {
        Ok(LinkReads {
            access: BoardAccess::All,
            record_use: false,
            ..self.link_reads()?
        })
    }

    /// Returns the reads of linked tasks for one request, see [`LinkReads`].
    pub fn link_reads(&self) -> DbResult<LinkReads> {
        Ok(LinkReads {
            txn: self.db.read()?,
            db: self.db.clone(),
            account: self.account.clone(),
            access: self.boards.clone(),
            record_use: true,
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

    /// Attaches a received file to a task.
    pub fn add_attachment_file(
        &self,
        task: &OwnedTask,
        name: &str,
        mime: &str,
        upload: &Incoming,
        uploaded_by: Actor<'_>,
    ) -> DbResult<Option<TaskAttachment>> {
        self.db
            .add_attachment_file(&task.row().id, name, mime, upload, uploaded_by)
    }

    /// Refuses an upload of `bytes` called `name` to `task` that repeats the name of one of its
    /// attachments, or would leave the task past its attachment count or its owner past the
    /// stored-bytes quota.
    pub fn check_attachment_room(&self, task: &OwnedTask, name: &str, bytes: u64) -> DbResult<()> {
        self.db.check_attachment_room(&task.row().id, name, bytes)
    }

    /// Starts receiving the bytes of an attachment into a file.
    pub fn receive_attachment(&self) -> DbResult<Incoming> {
        self.db.receive_attachment()
    }

    /// Opens the file of one of a task's attachments and returns it with its length, or [`None`] when
    /// the task holds no such attachment or no file holds it.
    pub fn open_attachment(
        &self,
        task: &OwnedTask,
        attachment_id: &str,
    ) -> DbResult<Option<(File, u64)>> {
        if !task.row().attachments.iter().any(|a| a.id == attachment_id) {
            return Ok(None);
        }
        self.db.open_attachment(attachment_id)
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

    /// Returns webhook `id` when the account owns its board.
    pub fn webhook(&self, id: &str) -> DbResult<Option<OwnedWebhook>> {
        self.on_owned_board(WEBHOOKS, id, |hook: &WebhookRow| &hook.board_id)
    }

    /// Returns a board's webhooks, oldest first, and none for another account's board.
    pub fn list_webhooks(&self, board_id: &str) -> DbResult<Vec<WebhookRow>> {
        self.on_own_board(board_id, || self.db.list_webhooks(board_id))
    }

    /// Stores a new webhook on a board, and refuses it while the board holds the most it may.
    pub fn create_webhook(&self, board: &OwnedBoard, hook: NewWebhook) -> DbResult<WebhookRow> {
        let hook = NewWebhook {
            board_id: board.row().id.clone(),
            ..hook
        };
        self.db.create_webhook(hook)
    }

    /// Applies a patch to a webhook and returns it as stored.
    pub fn update_webhook(
        &self,
        hook: &OwnedWebhook,
        patch: WebhookPatch,
    ) -> DbResult<Option<WebhookRow>> {
        self.db.update_webhook(&hook.row().id, patch)
    }

    /// Deletes a webhook.
    pub fn delete_webhook(&self, hook: &OwnedWebhook) -> DbResult<bool> {
        self.db.delete_webhook(&hook.row().id)
    }

    /// Returns time entry `id` when the account logged it, refusing one on a task outside the
    /// token's set with [`BoardDenied`].
    pub fn entry(&self, id: &str) -> DbResult<Option<OwnedEntry>> {
        let txn = self.db.read()?;
        let entry = read_in::<TimeEntryRow>(&txn, TIME_ENTRIES, id)?
            .filter(|entry| entry.account_id == self.account.as_str());
        if let Some(entry) = &entry
            && self.restricted()
            && !entry.task_id.is_empty()
            && self.owned_head(&entry.task_id)?.is_none()
        {
            return Ok(None);
        }
        Ok(entry.map(Owned))
    }

    /// Drops from `entries` those on a task outside the token's set. Time without a task sits on
    /// no board and stays.
    fn reachable_entries(&self, entries: Vec<TimeEntryRow>) -> DbResult<Vec<TimeEntryRow>> {
        if !self.restricted() {
            return Ok(entries);
        }
        let txn = self.db.read()?;
        let mut reachable = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.task_id.is_empty() || self.task_granted_in(&txn, &entry.task_id)? {
                reachable.push(entry);
            }
        }
        Ok(reachable)
    }

    /// Returns a task's time entries, newest first; empty for a task the account does not own.
    pub fn list_entries(&self, task_id: &str) -> DbResult<Vec<TimeEntryRow>> {
        if self.task(task_id)?.is_none() {
            return Ok(Vec::new());
        }
        self.db.list_entries(task_id)
    }

    /// Returns every time entry the account logged that the token reaches, in no set order.
    pub fn all_entries(&self) -> DbResult<Vec<TimeEntryRow>> {
        self.reachable_entries(self.db.account_entries(&self.account)?)
    }

    /// Returns the account's time entries of today that the token reaches, newest first.
    pub fn entries_today(&self) -> DbResult<Vec<TimeEntryRow>> {
        self.reachable_entries(self.db.entries_today(&self.account, &Local)?)
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
                        Some(column)
                            if self.boards.allows(&column.board_id)
                                && owns_board_in(&txn, &self.account, &column.board_id)? =>
                        {
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

    /// Logs `entry` on a task and its history event in one write.
    pub fn log_task_time(
        &self,
        task: &OwnedTask,
        entry: NewTimeEntry<'_>,
        actor: Actor<'_>,
    ) -> DbResult<bool> {
        let task_id = Some(task.row().id.as_str());
        self.db
            .log_task_time(NewTimeEntry { task_id, ..entry }, actor)
    }

    /// Creates a time entry for the account; [`None`] when its task is gone or another account's.
    pub fn create_entry_full(&self, entry: NewTimeEntry<'_>) -> DbResult<Option<String>> {
        if let Some(task_id) = entry.task_id {
            self.check_task_granted(task_id)?;
        }
        self.db.create_entry_full(&self.account, entry)
    }

    /// Creates the time entry `entry` on a task.
    pub fn create_entry_at(
        &self,
        task: &OwnedTask,
        entry: NewTimeEntry<'_>,
    ) -> DbResult<Option<String>> {
        let task_id = Some(task.row().id.as_str());
        self.db.create_entry_at(NewTimeEntry { task_id, ..entry })
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

    /// Returns the account's running timer, if one runs, refusing a timer on a task outside the
    /// token's set with [`BoardDenied`], as stopping or replacing it writes to that task.
    pub fn active_timer(&self) -> DbResult<Option<ActiveTimer>> {
        let timer = self.db.active_timer(&self.account)?;
        if let Some(task_id) = timer.as_ref().and_then(|timer| timer.task_id.as_deref()) {
            self.check_task_granted(task_id)?;
        }
        Ok(timer)
    }

    /// Returns the account's running timer unless it runs on a task outside the token's set, which
    /// reads as no timer.
    pub fn visible_timer(&self) -> DbResult<Option<ActiveTimer>> {
        let timer = self.db.active_timer(&self.account)?;
        if self.restricted()
            && let Some(task_id) = timer.as_ref().and_then(|timer| timer.task_id.as_deref())
            && !self.task_granted_in(&self.db.read()?, task_id)?
        {
            return Ok(None);
        }
        Ok(timer)
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
        self.db.list_account_api_tokens(&self.account)
    }

    /// Mints and stores an API token for the account, returning its record and the raw token,
    /// which is never stored.
    pub fn create_api_token(
        &self,
        name: &str,
        grant: TokenGrant,
        expires_at: Option<i64>,
    ) -> DbResult<(ApiTokenRecord, String)> {
        let (mut record, raw) = new_api_token_record(&self.account, name, grant.scope, expires_at);
        record.permissions = grant.permissions;
        record.boards = grant.boards;
        self.db.create_api_token(&record)?;
        Ok((record, raw))
    }

    /// Returns the live token `raw` when the account holds it. The bootstrap token has no record,
    /// so it is never found.
    pub fn find_own_token(&self, raw: &str) -> DbResult<Option<ApiTokenRecord>> {
        let token = self.db.find_api_token(raw)?;
        Ok(token.filter(|token| token.account_id == self.account.as_str()))
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

    /// Lists the account's sessions, newest first, marking the one of the cookie `current`.
    pub fn sessions(&self, current: Option<SessionToken<'_>>) -> DbResult<Vec<SessionListing>> {
        self.db.list_sessions(&self.account, current)
    }

    /// Ends the account's session named `id` in the sessions list, unless it is the one of the
    /// cookie `current`.
    pub fn end_session(
        &self,
        id: &str,
        current: Option<SessionToken<'_>>,
    ) -> DbResult<SessionEndOutcome> {
        self.db.end_session(&self.account, id, current)
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
