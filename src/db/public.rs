use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use webauthn_rs::prelude::Passkey;

use super::{
    AppState, Db, DbResult, DeviceAuthCreateOutcome, DevicePoll, FirstPasskeyOutcome, NewAccount,
    RecoverOutcome, RegisterOutcome,
};
use crate::models::{
    AccountId, AccountStatus, ApiTokenRecord, BoardRow, ColumnRow, DeviceAuthRow, InviteRow,
    PasskeyRow, RecoveryCodeRow, RegistrationMode, TaskRow, UserRow,
};

/// Sign-in, setup, device pairing and the calendar feed, plus the lookups the auth layer makes.
/// It reads no board content except one board's feed, found by its secret token.
#[derive(Clone, Debug)]
pub struct PublicDb {
    /// The database the handle reads and writes.
    db: Db,
}

/// What a calendar feed shows: one board's columns and tasks.
#[derive(Clone, Debug)]
pub struct FeedBoard {
    /// The board the feed token names.
    pub board: BoardRow,
    /// The board's columns, in board order.
    pub columns: Vec<ColumnRow>,
    /// The board's tasks, in board order.
    pub tasks: Vec<TaskRow>,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for PublicDb {
    type Rejection = Infallible;

    async fn from_request_parts(_: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        Ok(state.public())
    }
}

impl PublicDb {
    /// Seconds a device-pairing code stays valid.
    pub const DEVICE_CODE_TTL_SECONDS: i64 = Db::DEVICE_CODE_TTL_SECONDS;
    /// Minimum seconds between token polls for one device code.
    pub const DEVICE_POLL_INTERVAL_SECONDS: i64 = Db::DEVICE_POLL_INTERVAL_SECONDS;

    /// Returns the handle over `db`.
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// Returns how many passkeys are registered.
    pub fn passkey_count(&self) -> DbResult<u64> {
        self.db.passkey_count()
    }

    /// Returns the instance's owner, its admin, if it exists yet.
    pub fn owner(&self) -> DbResult<Option<UserRow>> {
        self.db.owner()
    }

    /// Returns the instance's owner, creating it as the admin `username` when there is none.
    pub fn ensure_owner(&self, username: &str) -> DbResult<UserRow> {
        self.db.ensure_owner(username)
    }

    /// Returns every stored passkey, which a discoverable sign-in matches against.
    pub fn passkeys(&self) -> DbResult<Vec<PasskeyRow>> {
        self.db.passkeys()
    }

    /// Records a sign-in with passkey `id`, storing its updated `credential`.
    pub fn record_passkey_use(&self, id: &str, credential: &Passkey, now: i64) -> DbResult<bool> {
        self.db.record_passkey_use(id, credential, now)
    }

    /// Stores the instance's first passkey and the `recovery` code when one is given, refusing
    /// once any passkey exists.
    pub fn claim_first_passkey(
        &self,
        user: &UserRow,
        passkey: &PasskeyRow,
        recovery: Option<&RecoveryCodeRow>,
    ) -> DbResult<FirstPasskeyOutcome> {
        self.db.claim_first_passkey(user, passkey, recovery)
    }

    /// Opens a session for `user` and returns its token.
    pub fn create_session(&self, user: &AccountId) -> DbResult<String> {
        self.db.create_session(user)
    }

    /// Ends the session with `token`.
    pub fn delete_session(&self, token: &str) -> DbResult<()> {
        self.db.delete_session(token)
    }

    /// Returns the account a session token signs in.
    pub fn user_for_token(&self, token: &str) -> DbResult<Option<UserRow>> {
        self.db.user_for_token(token)
    }

    /// Returns the account with `id`.
    pub fn user(&self, id: &str) -> DbResult<Option<UserRow>> {
        self.db.user(id)
    }

    /// Returns the account whose handle is `handle`, in any case.
    pub fn account_by_handle(&self, handle: &str) -> DbResult<Option<UserRow>> {
        self.db.account_by_handle(handle)
    }

    /// Returns whether an account has the handle `handle`, in any case.
    pub fn handle_taken(&self, handle: &str) -> DbResult<bool> {
        self.db.handle_taken(handle)
    }

    /// Returns the live invite whose token hashes to `hash`.
    pub fn invite(&self, hash: &str, now: i64) -> DbResult<Option<InviteRow>> {
        self.db.invite(hash, now)
    }

    /// Returns how a person without an invite link gets an account.
    pub fn registration(&self) -> DbResult<RegistrationMode> {
        self.db.registration()
    }

    /// Stores a member account registered through an invite, using the invite up, or one that
    /// requests to join, which waits for the admin.
    pub fn register_account(&self, new: &NewAccount<'_>, now: i64) -> DbResult<RegisterOutcome> {
        self.db.register_account(new, now)
    }

    /// Returns `account`'s recovery code row, absent until it gets one.
    pub fn recovery_code(&self, account: &AccountId) -> DbResult<Option<RecoveryCodeRow>> {
        self.db.recovery_code(account)
    }

    /// Adds `passkey` to `account`, replaces its recovery code and ends all its sessions, unless
    /// its stored code is no longer the one hashed as `expected_hash`.
    pub fn recover_account(
        &self,
        account: &AccountId,
        expected_hash: &str,
        passkey: &PasskeyRow,
        recovery: &RecoveryCodeRow,
    ) -> DbResult<RecoverOutcome> {
        self.db
            .recover_account(account, expected_hash, passkey, recovery)
    }

    /// Finds the API token `raw` names.
    pub fn find_api_token(&self, raw: &str) -> DbResult<Option<ApiTokenRecord>> {
        self.db.find_api_token(raw)
    }

    /// Returns whether `raw` is a token a suspension took from its account.
    pub fn is_suspended_token(&self, raw: &str) -> DbResult<bool> {
        self.db.is_suspended_token(raw)
    }

    /// Records that `account` made a request at `now`, at most once an hour and in memory only.
    pub fn touch_account(&self, account: &AccountId, now: i64) {
        self.db.touch_account(account, now);
    }

    /// Stores `now` as the last use of token `id`.
    pub fn touch_api_token(&self, id: &str, now: i64) -> DbResult<()> {
        self.db.touch_api_token(id, now)
    }

    /// Stores a new device-pairing request, first deleting requests past their retention.
    pub fn create_device_auth(
        &self,
        row: &DeviceAuthRow,
        now: i64,
    ) -> DbResult<DeviceAuthCreateOutcome> {
        self.db.create_device_auth(row, now)
    }

    /// Answers one token poll, having `mint` build the token of an approved request.
    pub fn poll_device_auth<F>(
        &self,
        device_code_hash: &str,
        now: i64,
        mint: F,
    ) -> DbResult<DevicePoll>
    where
        F: FnOnce(&DeviceAuthRow) -> ApiTokenRecord,
    {
        self.db.poll_device_auth(device_code_hash, now, mint)
    }

    /// Returns the board whose calendar-feed token is `raw`, with its columns and tasks, when its
    /// owner is active.
    pub fn calendar_feed(&self, raw: &str) -> DbResult<Option<FeedBoard>> {
        let Some(board) = self.db.board_by_feed_token(raw)? else {
            return Ok(None);
        };
        let active = self
            .db
            .user(&board.owner_id)?
            .is_some_and(|owner| owner.status == AccountStatus::Active);
        if !active {
            return Ok(None);
        }
        Ok(Some(FeedBoard {
            columns: self.db.list_columns(&board.id)?,
            tasks: self.db.board_tasks(&board.id)?,
            board,
        }))
    }
}
