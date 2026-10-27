use chrono::Utc;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, WriteTransaction};
use serde::de::DeserializeOwned;

use super::activity::txn_mark_boards_used;
use super::usage::OverQuota;
use super::{
    ACCOUNT_SETTINGS, ACCOUNT_USAGE, ACTIVE_TIMER, ACTIVITY, API_TOKENS, BOARDS_BY_OWNER,
    BUILT_IN_TEMPLATES, DEVICE_AUTHS, Db, DbResult, ENTRIES_BY_ACCOUNT, INSTANCE_SETTINGS_KEY,
    INVITES, PASSKEYS_BY_USER, RECOVERY_CODES, Rows, SESSIONS, SETTINGS, STARTER_BOARD_NAME,
    SUSPENDED_TOKENS, TEMPLATES_BY_OWNER, TIME_ENTRIES, USERNAMES, USERS, index_in, read_one,
    rows_in, txn_credential_registered, txn_insert_passkey, txn_insert_user, txn_instance_settings,
    txn_mmap_get, txn_owner_boards, txn_read_one, txn_refuse_stranded, txn_remove_board,
    txn_remove_entry, txn_remove_passkeys_of, txn_remove_template, txn_store_passkey,
    txn_template_snapshot, txn_templates_of, write_one,
};
use crate::auth;
use crate::models::{
    AccountId, AccountRole, AccountStatus, ApiTokenRecord, DeviceAuthRow, DeviceAuthStatus,
    InstanceMode, InstanceSettings, InviteRow, PasskeyRow, RecoveryCodeRow, RegistrationMode,
    SECONDS_PER_DAY, SessionRow, UserRow,
};

/// What [`Db::apply_mode`] did with the stored instance mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ModeSwitch {
    /// The stored mode already was the requested one.
    Kept,
    /// The stored mode was `from` and is now the requested one.
    Switched {
        /// The mode stored before.
        from: InstanceMode,
    },
    /// Refused: single mode serves one account, and `accounts` exist.
    Refused {
        /// How many accounts exist.
        accounts: u64,
    },
}

/// An account that registers through an invite or requests to join, with its first passkey and
/// recovery code.
#[derive(Copy, Clone, Debug)]
pub struct NewAccount<'a> {
    /// The member account, its handle already in canonical form. Its stored status follows from
    /// `invite_hash`.
    pub user: &'a UserRow,
    /// The account's first passkey.
    pub passkey: &'a PasskeyRow,
    /// The account's recovery code.
    pub recovery: &'a RecoveryCodeRow,
    /// The hash of the invite token the registration uses up, absent for a request to join.
    pub invite_hash: Option<&'a str>,
}

/// Typed outcome for [`Db::register_account`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// The account, its passkey and its recovery code are stored with this status, and the invite
    /// is used up: active through an invite, pending for a request.
    Registered(AccountStatus),
    /// Refused: the invite is used, revoked, expired or unknown.
    InviteGone,
    /// Refused: the instance takes no requests to join, only invites.
    RegistrationClosed,
    /// Refused: as many accounts as the instance's cap already wait for approval.
    PendingFull,
    /// Refused: another account has the handle.
    HandleTaken,
    /// Refused: a passkey with this credential id is registered already.
    DuplicatePasskey,
}

/// What a recovery revoked from an account.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Revoked {
    /// API tokens deleted.
    pub tokens: u64,
    /// Passkeys deleted, not counting the one the recovery enrolled.
    pub passkeys: u64,
    /// Sessions ended.
    pub sessions: u64,
}

/// Typed outcome for [`Db::recover_account`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecoverOutcome {
    /// The new passkey and code are stored, and the account's tokens, other passkeys and sessions
    /// are gone.
    Recovered(Revoked),
    /// Refused: the recovery code changed since the ceremony started.
    StaleCode,
    /// Refused: the admin suspended the account since the ceremony started.
    Suspended,
    /// Refused: a passkey with this credential id is registered already.
    DuplicatePasskey,
}

/// Typed outcome for [`Db::delete_account`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AccountDeleteOutcome {
    /// Every row the account owned is gone, and `remaining` accounts are left.
    Deleted {
        /// How many accounts are left.
        remaining: u64,
    },
    /// No account has that id.
    NotFound,
    /// Refused: the admin's account goes last.
    AdminHasAccounts,
}

/// A change the admin makes to a member account's status.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AccountAction {
    /// Lets a pending account in.
    Approve,
    /// Stops an active account: its sessions end and its tokens stop working for good.
    Suspend,
    /// Lets a suspended account sign in again, without its old tokens.
    Unsuspend,
}

impl AccountAction {
    /// Returns the status the action moves an account from, and the one it moves it to.
    #[must_use]
    pub fn transition(self) -> (AccountStatus, AccountStatus) {
        match self {
            Self::Approve => (AccountStatus::Pending, AccountStatus::Active),
            Self::Suspend => (AccountStatus::Active, AccountStatus::Suspended),
            Self::Unsuspend => (AccountStatus::Suspended, AccountStatus::Active),
        }
    }
}

/// Typed outcome for [`Db::apply_account_action`] and [`Db::delete_member`].
#[derive(Clone, Debug)]
pub enum AdminActionOutcome {
    /// The account changed, or was deleted with everything it owned.
    Done(UserRow),
    /// No account has that id.
    NotFound,
    /// Refused: the admin's own account is out of reach of these actions.
    RefusedAdmin,
    /// Refused: the action does not apply to an account with this status.
    WrongStatus(AccountStatus),
}

/// What the admin sees of an account: no board content, only its standing and its size.
#[derive(Clone, Debug)]
pub struct AccountSummary {
    /// The account.
    pub user: UserRow,
    /// Unix seconds the account was last seen, absent when it never was.
    pub last_seen_at: Option<i64>,
    /// How many boards the account owns.
    pub board_count: u64,
    /// Bytes the account stores in task rows, time entries, saved templates and attachment files.
    pub stored_bytes: u64,
}

/// Returns the id of the account whose handle is `handle` in any case, from an open usernames
/// table.
fn handle_owner_in<R>(tbl: &R, handle: &str) -> DbResult<Option<String>>
where
    R: ReadableTable<&'static str, &'static str>,
{
    let handle = handle.trim();
    for item in tbl.iter()? {
        let (name, id) = item?;
        if name.value().eq_ignore_ascii_case(handle) {
            return Ok(Some(id.value().to_owned()));
        }
    }
    Ok(None)
}

/// Deletes every row of `table` that `owned` picks, inside an open write transaction, and returns
/// how many it deleted. A row that does not parse stays.
fn remove_rows<T>(txn: &WriteTransaction, table: Rows, owned: impl Fn(&T) -> bool) -> DbResult<u64>
where
    T: DeserializeOwned,
{
    let mut removed = 0;
    txn.open_table(table)?.retain(|_, raw| {
        let keep = serde_json::from_slice::<T>(raw)
            .map(|row| !owned(&row))
            .unwrap_or(true);
        removed += u64::from(!keep);
        keep
    })?;
    Ok(removed)
}

/// Deletes `user` and every row it owns inside an open write transaction: its boards with
/// everything in them, attachment data, templates, time entries, tokens, device requests,
/// sessions, passkeys, recovery code, invites, settings, timer, activity and handle.
pub(super) fn txn_delete_account(txn: &WriteTransaction, user: &UserRow) -> DbResult<()> {
    let id = user.id.as_str();
    for board in txn_owner_boards(txn, id)? {
        txn_remove_board(txn, &board)?;
    }
    {
        let mut entries = txn.open_table(TIME_ENTRIES)?;
        for entry_id in index_in(txn, ENTRIES_BY_ACCOUNT, id)? {
            txn_remove_entry(txn, &mut entries, &entry_id)?;
        }
    }
    for template_id in index_in(txn, TEMPLATES_BY_OWNER, id)? {
        txn_remove_template(txn, &template_id)?;
    }
    for tokens in [API_TOKENS, SUSPENDED_TOKENS] {
        remove_rows(txn, tokens, |token: &ApiTokenRecord| token.account_id == id)?;
    }
    remove_rows(txn, DEVICE_AUTHS, |request: &DeviceAuthRow| {
        request.account_id.as_deref() == Some(id)
    })?;
    remove_rows(txn, SESSIONS, |session: &SessionRow| session.user_id == id)?;
    txn_remove_passkeys_of(txn, id)?;
    remove_rows(txn, INVITES, |invite: &InviteRow| invite.created_by == id)?;
    for table in [RECOVERY_CODES, ACCOUNT_SETTINGS, ACTIVE_TIMER, USERS] {
        txn.open_table(table)?.remove(id)?;
    }
    txn.open_table(ACTIVITY)?.remove(id)?;
    txn.open_table(ACCOUNT_USAGE)?.remove(id)?;
    txn.open_table(USERNAMES)?.remove(user.username.as_str())?;
    Ok(())
}

/// Builds `account`'s first board from the built-in template, inside an open write transaction.
fn txn_build_starter_board(txn: &WriteTransaction, account: &AccountId) -> DbResult<()> {
    let starter = txn_templates_of(txn, BUILT_IN_TEMPLATES)?
        .into_iter()
        .find(|template| template.name == Db::BUILTIN_TEMPLATE_NAME)
        .ok_or("the built-in board template is missing")?;
    let snapshot = txn_template_snapshot(txn, &starter.id)?
        .ok_or("the built-in board template has no snapshot")?;
    Db::build_from_template(
        txn,
        account,
        &snapshot,
        STARTER_BOARD_NAME,
        "",
        OverQuota::Allow,
    )?;
    Ok(())
}

/// Marks every device request `account` approved but whose token is not minted yet as denied,
/// inside an open write transaction, so lifting a suspension never revives it.
fn txn_deny_approved_devices(txn: &WriteTransaction, account: &str) -> DbResult<()> {
    let mut tbl = txn.open_table(DEVICE_AUTHS)?;
    let mut denied = Vec::new();
    for item in tbl.iter()? {
        let (key, raw) = item?;
        let mut row: DeviceAuthRow = serde_json::from_slice(raw.value())?;
        if row.status == DeviceAuthStatus::Approved && row.account_id.as_deref() == Some(account) {
            row.status = DeviceAuthStatus::Denied;
            denied.push((key.value().to_owned(), serde_json::to_vec(&row)?));
        }
    }
    for (key, raw) in denied {
        tbl.insert(key.as_str(), raw.as_slice())?;
    }
    Ok(())
}

/// Returns how many accounts wait for approval, read inside an open write transaction.
fn txn_pending_count(txn: &WriteTransaction) -> DbResult<u64> {
    let users: Vec<UserRow> = rows_in(&txn.open_table(USERS)?)?;
    Ok(users
        .iter()
        .filter(|user| user.status == AccountStatus::Pending)
        .count() as u64)
}

/// Stores `user` with the status `action` moves it to, inside an open write transaction. An
/// approval builds the account's starter board. A suspension ends the account's sessions, denies
/// its approved device requests and moves its tokens to the suspended ones, which never
/// authenticate. Lifting it deletes those tokens for good.
fn txn_apply_action(txn: &WriteTransaction, user: &UserRow, action: AccountAction) -> DbResult<()> {
    let id = user.id.as_str();
    let (_, to) = action.transition();
    write_one(
        txn,
        USERS,
        id,
        &UserRow {
            status: to,
            ..user.clone()
        },
    )?;
    match action {
        AccountAction::Approve => txn_build_starter_board(txn, &user.account_id())?,
        AccountAction::Suspend => {
            remove_rows(txn, SESSIONS, |session: &SessionRow| session.user_id == id)?;
            txn_deny_approved_devices(txn, id)?;
            let held: Vec<ApiTokenRecord> = rows_in(&txn.open_table(API_TOKENS)?)?;
            let mut live = txn.open_table(API_TOKENS)?;
            let mut suspended = txn.open_table(SUSPENDED_TOKENS)?;
            for token in held.iter().filter(|token| token.account_id == id) {
                live.remove(token.id.as_str())?;
                suspended.insert(token.id.as_str(), serde_json::to_vec(token)?.as_slice())?;
            }
        }
        AccountAction::Unsuspend => {
            remove_rows(txn, SUSPENDED_TOKENS, |token: &ApiTokenRecord| {
                token.account_id == id
            })?;
            txn_mark_boards_used(txn, id, Utc::now().timestamp())?;
        }
    }
    Ok(())
}

impl Db {
    /// Most expired requests one write transaction deletes, so expiry never holds the writer for
    /// long.
    const EXPIRY_BATCH_ACCOUNTS: usize = 50;
    /// Days a request to join waits for the admin before it and everything it owns are deleted.
    pub const PENDING_REQUEST_DAYS: i64 = 30;

    /// Stores `mode` as the instance's mode, refusing single mode while more than one account
    /// exists.
    pub fn apply_mode(&self, mode: InstanceMode) -> DbResult<ModeSwitch> {
        let txn = self.db.begin_write()?;
        let mut settings = txn_instance_settings(&txn)?;
        let from = settings.mode;
        if from == mode {
            return Ok(ModeSwitch::Kept);
        }
        let accounts = txn.open_table(USERS)?.len()?;
        if mode == InstanceMode::Single && accounts > 1 {
            return Ok(ModeSwitch::Refused { accounts });
        }
        settings.mode = mode;
        write_one(&txn, SETTINGS, INSTANCE_SETTINGS_KEY, &settings)?;
        txn.commit()?;
        Ok(ModeSwitch::Switched { from })
    }

    /// Returns the stored instance settings, or those of a single-mode instance nobody has edited
    /// when none are stored.
    pub fn instance_settings(&self) -> DbResult<InstanceSettings> {
        let stored: Option<InstanceSettings> = read_one(&self.db, SETTINGS, INSTANCE_SETTINGS_KEY)?;
        Ok(stored.unwrap_or_default())
    }

    /// Changes the stored instance settings with `edit` in one transaction and returns them.
    pub fn update_instance_settings(
        &self,
        edit: impl FnOnce(&mut InstanceSettings),
    ) -> DbResult<InstanceSettings> {
        let txn = self.db.begin_write()?;
        let mut settings = txn_instance_settings(&txn)?;
        edit(&mut settings);
        write_one(&txn, SETTINGS, INSTANCE_SETTINGS_KEY, &settings)?;
        txn.commit()?;
        Ok(settings)
    }

    /// Returns how a person without an invite link gets an account.
    pub fn registration(&self) -> DbResult<RegistrationMode> {
        Ok(self.instance_settings()?.registration)
    }

    /// Returns the account whose handle is `handle`, in any case.
    pub fn account_by_handle(&self, handle: &str) -> DbResult<Option<UserRow>> {
        let txn = self.db.begin_read()?;
        let Some(id) = handle_owner_in(&txn.open_table(USERNAMES)?, handle)? else {
            return Ok(None);
        };
        let tbl = txn.open_table(USERS)?;
        Ok(tbl
            .get(id.as_str())?
            .map(|guard| serde_json::from_slice(guard.value()))
            .transpose()?)
    }

    /// Returns the live invite whose token hashes to `hash`, absent once it is used, revoked or
    /// past its expiry at `now`.
    pub fn invite(&self, hash: &str, now: i64) -> DbResult<Option<InviteRow>> {
        let invite: Option<InviteRow> = read_one(&self.db, INVITES, hash)?;
        Ok(invite.filter(|invite| !invite.is_expired(now)))
    }

    /// Stores an invite under the hash of its token, first deleting invites past their expiry at
    /// `now`.
    pub fn create_invite(&self, hash: &str, invite: &InviteRow, now: i64) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        remove_rows(&txn, INVITES, |row: &InviteRow| row.is_expired(now))?;
        write_one(&txn, INVITES, hash, invite)?;
        txn.commit()?;
        Ok(())
    }

    /// Returns the unused invites, oldest first.
    pub fn list_invites(&self) -> DbResult<Vec<InviteRow>> {
        let txn = self.db.begin_read()?;
        let mut invites: Vec<InviteRow> = rows_in(&txn.open_table(INVITES)?)?;
        invites.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(invites)
    }

    /// Revokes the unused invite `id`. Returns `false` when no unused invite has that id.
    pub fn delete_invite(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let held = {
            let tbl = txn.open_table(INVITES)?;
            let mut held = None;
            for item in tbl.iter()? {
                let (hash, raw) = item?;
                let invite: InviteRow = serde_json::from_slice(raw.value())?;
                if invite.id == id {
                    held = Some(hash.value().to_owned());
                    break;
                }
            }
            held
        };
        let Some(hash) = held else {
            return Ok(false);
        };
        txn.open_table(INVITES)?.remove(hash.as_str())?;
        txn.commit()?;
        Ok(true)
    }

    /// Stores a member account registered through an invite, with its starter board, using the
    /// invite up in the same transaction, so an invite registers one account.
    pub fn register_account(&self, new: &NewAccount<'_>, now: i64) -> DbResult<RegisterOutcome> {
        let txn = self.db.begin_write()?;
        let status = match new.invite_hash {
            Some(hash) => {
                let invite: Option<InviteRow> = txn_read_one(&txn, INVITES, hash)?;
                if invite.is_none_or(|invite| invite.is_expired(now)) {
                    return Ok(RegisterOutcome::InviteGone);
                }
                AccountStatus::Active
            }
            None => {
                let settings = txn_instance_settings(&txn)?;
                if settings.registration != RegistrationMode::Request {
                    return Ok(RegisterOutcome::RegistrationClosed);
                }
                if txn_pending_count(&txn)? >= u64::from(settings.pending_cap) {
                    return Ok(RegisterOutcome::PendingFull);
                }
                AccountStatus::Pending
            }
        };
        if handle_owner_in(&txn.open_table(USERNAMES)?, &new.user.username)?.is_some() {
            return Ok(RegisterOutcome::HandleTaken);
        }
        if txn_credential_registered(&txn, new.passkey)? {
            return Ok(RegisterOutcome::DuplicatePasskey);
        }
        txn_insert_user(
            &txn,
            &UserRow {
                status,
                ..new.user.clone()
            },
        )?;
        txn_store_passkey(&txn, new.passkey)?;
        write_one(&txn, RECOVERY_CODES, &new.user.id, new.recovery)?;
        if let Some(hash) = new.invite_hash {
            txn.open_table(INVITES)?.remove(hash)?;
        }
        if status == AccountStatus::Active {
            txn_build_starter_board(&txn, &new.user.account_id())?;
        }
        txn.commit()?;
        Ok(RegisterOutcome::Registered(status))
    }

    /// Returns whether an account has the handle `handle`, in any case.
    pub fn handle_taken(&self, handle: &str) -> DbResult<bool> {
        let txn = self.db.begin_read()?;
        Ok(handle_owner_in(&txn.open_table(USERNAMES)?, handle)?.is_some())
    }

    /// Returns `account`'s recovery code row, absent until it gets one.
    pub fn recovery_code(&self, account: &AccountId) -> DbResult<Option<RecoveryCodeRow>> {
        read_one(&self.db, RECOVERY_CODES, account.as_str())
    }

    /// Replaces `account`'s recovery code.
    pub fn set_recovery_code(&self, account: &AccountId, row: &RecoveryCodeRow) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded(&txn, account)?;
        write_one(&txn, RECOVERY_CODES, account.as_str(), row)?;
        txn.commit()?;
        Ok(())
    }

    /// Recovers `account` in one transaction: adds `passkey`, replaces its recovery code with
    /// `recovery`, and revokes every API token, every other passkey and every session of the
    /// account, which the outcome counts. Refuses when the stored code is no longer the one hashed
    /// as `expected_hash`, or when the account is suspended.
    pub fn recover_account(
        &self,
        account: &AccountId,
        expected_hash: &str,
        passkey: &PasskeyRow,
        recovery: &RecoveryCodeRow,
    ) -> DbResult<RecoverOutcome> {
        let txn = self.db.begin_write()?;
        let stored: Option<RecoveryCodeRow> = txn_read_one(&txn, RECOVERY_CODES, account.as_str())?;
        if stored.is_none_or(|stored| stored.hash != expected_hash) {
            return Ok(RecoverOutcome::StaleCode);
        }
        let user: Option<UserRow> = txn_read_one(&txn, USERS, account.as_str())?;
        if user.is_some_and(|user| user.status == AccountStatus::Suspended) {
            return Ok(RecoverOutcome::Suspended);
        }
        if txn_credential_registered(&txn, passkey)? {
            return Ok(RecoverOutcome::DuplicatePasskey);
        }
        let tokens = remove_rows(&txn, API_TOKENS, |token: &ApiTokenRecord| {
            token.account_id == account.as_str()
        })?;
        let passkeys = txn_mmap_get(&txn, PASSKEYS_BY_USER, account.as_str())?.len() as u64;
        txn_remove_passkeys_of(&txn, account.as_str())?;
        txn_insert_passkey(&txn, passkey)?;
        txn_deny_approved_devices(&txn, account.as_str())?;
        write_one(&txn, RECOVERY_CODES, account.as_str(), recovery)?;
        let sessions = remove_rows(&txn, SESSIONS, |session: &SessionRow| {
            session.user_id == account.as_str()
        })?;
        txn.commit()?;
        Ok(RecoverOutcome::Recovered(Revoked {
            tokens,
            passkeys,
            sessions,
        }))
    }

    /// Deletes `account` and every row it owns in one transaction: its boards with everything in
    /// them, attachment data, templates, time entries, tokens, device requests, sessions,
    /// passkeys, recovery code, invites, settings, timer, last-seen time and handle. The admin's account is
    /// refused while other accounts exist.
    pub fn delete_account(&self, account: &AccountId) -> DbResult<AccountDeleteOutcome> {
        let txn = self.db.begin_write()?;
        let Some(user) = txn_read_one::<UserRow>(&txn, USERS, account.as_str())? else {
            return Ok(AccountDeleteOutcome::NotFound);
        };
        let accounts = txn.open_table(USERS)?.len()?;
        if user.role == AccountRole::Admin && accounts > 1 {
            return Ok(AccountDeleteOutcome::AdminHasAccounts);
        }
        txn_delete_account(&txn, &user)?;
        txn.commit()?;
        self.forget_activity(account);
        Ok(AccountDeleteOutcome::Deleted {
            remaining: accounts - 1,
        })
    }

    /// Returns every account with when it was last seen and how many boards it owns, oldest
    /// first.
    pub fn account_summaries(&self) -> DbResult<Vec<AccountSummary>> {
        let seen = self.last_seen()?;
        let stored = self.stored_bytes_by_account()?;
        let txn = self.db.begin_read()?;
        let mut users: Vec<UserRow> = rows_in(&txn.open_table(USERS)?)?;
        users.sort_by(|a, b| (a.created_at, &a.username).cmp(&(b.created_at, &b.username)));
        let boards = txn.open_multimap_table(BOARDS_BY_OWNER)?;
        users
            .into_iter()
            .map(|user| {
                Ok(AccountSummary {
                    last_seen_at: seen.get(&user.id).copied(),
                    board_count: boards.get(user.id.as_str())?.len(),
                    stored_bytes: stored.get(&user.id).copied().unwrap_or_default(),
                    user,
                })
            })
            .collect()
    }

    /// Moves the member account `id` along `action` in one transaction. The admin's account and an
    /// account whose status `action` does not start from are refused.
    pub fn apply_account_action(
        &self,
        id: &str,
        action: AccountAction,
    ) -> DbResult<AdminActionOutcome> {
        let txn = self.db.begin_write()?;
        let Some(user) = txn_read_one::<UserRow>(&txn, USERS, id)? else {
            return Ok(AdminActionOutcome::NotFound);
        };
        if user.role == AccountRole::Admin {
            return Ok(AdminActionOutcome::RefusedAdmin);
        }
        let (from, to) = action.transition();
        if user.status != from {
            return Ok(AdminActionOutcome::WrongStatus(user.status));
        }
        txn_apply_action(&txn, &user, action)?;
        txn.commit()?;
        Ok(AdminActionOutcome::Done(UserRow { status: to, ..user }))
    }

    /// Deletes the member account `id` and every row it owns in one transaction, whatever its
    /// status. The admin's account is refused.
    pub fn delete_member(&self, id: &str) -> DbResult<AdminActionOutcome> {
        let txn = self.db.begin_write()?;
        let Some(user) = txn_read_one::<UserRow>(&txn, USERS, id)? else {
            return Ok(AdminActionOutcome::NotFound);
        };
        if user.role == AccountRole::Admin {
            return Ok(AdminActionOutcome::RefusedAdmin);
        }
        txn_delete_account(&txn, &user)?;
        txn.commit()?;
        self.forget_activity(&user.account_id());
        Ok(AdminActionOutcome::Done(user))
    }

    /// Deletes every request to join older than [`Db::PENDING_REQUEST_DAYS`] at `now`, with
    /// everything it owns, handing each deleted handle to `expired` as soon as its batch commits,
    /// so a later batch that fails loses none of them.
    pub fn expire_pending_requests(&self, now: i64, mut expired: impl FnMut(&str)) -> DbResult<()> {
        let cutoff = now - Self::PENDING_REQUEST_DAYS * SECONDS_PER_DAY;
        let stale =
            |user: &UserRow| user.status == AccountStatus::Pending && user.created_at <= cutoff;
        loop {
            let any = {
                let txn = self.db.begin_read()?;
                rows_in::<UserRow, _>(&txn.open_table(USERS)?)?
                    .iter()
                    .any(stale)
            };
            if !any {
                return Ok(());
            }
            let txn = self.db.begin_write()?;
            let batch: Vec<UserRow> = rows_in::<UserRow, _>(&txn.open_table(USERS)?)?
                .into_iter()
                .filter(stale)
                .take(Self::EXPIRY_BATCH_ACCOUNTS)
                .collect();
            for user in &batch {
                txn_delete_account(&txn, user)?;
            }
            txn.commit()?;
            for user in &batch {
                self.forget_activity(&user.account_id());
                expired(&user.username);
            }
        }
    }

    /// Returns whether `raw` is a token a suspension took from its account.
    pub fn is_suspended_token(&self, raw: &str) -> DbResult<bool> {
        let txn = self.db.begin_read()?;
        let held: Vec<ApiTokenRecord> = rows_in(&txn.open_table(SUSPENDED_TOKENS)?)?;
        Ok(held
            .iter()
            .any(|record| auth::verify_api_token(record, raw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ACCOUNT_GONE, ATTACHMENT_DATA};
    use crate::models::{ActiveTimer, Actor, AuthScope, Settings, TimerMode};

    /// An open database with its admin and a member, both with a board.
    struct Fixture {
        /// Keeps the database directory alive.
        _dir: tempfile::TempDir,
        /// The database.
        db: Db,
        /// The member.
        member: AccountId,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("membership.redb").to_str().expect("utf8"))
            .expect("connect");
        db.create_user("admin").expect("admin");
        let member = db.create_user("bob").expect("member");
        Fixture {
            _dir: dir,
            db,
            member,
        }
    }

    /// Adds a column, a task and an attachment to a fresh board of `account`, returning the
    /// board, the task and the attachment ids.
    fn attached(db: &Db, account: &AccountId, name: &str) -> (String, String, String) {
        let board = db.create_board(account, name).expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let task = db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        let attachment = db
            .add_attachment(&task, "a.txt", "text/plain", b"secret", Actor::SYSTEM)
            .expect("attach")
            .expect("task")
            .id;
        (board, task, attachment)
    }

    #[test]
    fn deleting_a_task_should_remove_its_attachment_bytes() {
        let fx = fixture();
        let (_, task, attachment) = attached(&fx.db, &fx.member, "Bob");
        assert!(
            fx.db
                .read_attachment_data(&attachment)
                .expect("read")
                .is_some()
        );
        assert!(fx.db.delete_task(&task).expect("delete"));
        assert_eq!(fx.db.read_attachment_data(&attachment).expect("read"), None);
    }

    #[test]
    fn deleting_a_board_should_remove_its_attachment_bytes() {
        let fx = fixture();
        let (board, _, attachment) = attached(&fx.db, &fx.member, "Bob");
        fx.db.create_board(&fx.member, "Kept").expect("board");
        fx.db.delete_board(&board).expect("delete");
        assert_eq!(fx.db.read_attachment_data(&attachment).expect("read"), None);
    }

    #[test]
    fn deleting_a_task_then_the_account_should_leave_no_attachment_bytes() {
        let fx = fixture();
        let (_, deleted, early) = attached(&fx.db, &fx.member, "Bob");
        let (_, _, late) = attached(&fx.db, &fx.member, "Bob 2");
        assert!(fx.db.delete_task(&deleted).expect("delete"));
        assert_eq!(
            fx.db.delete_account(&fx.member).expect("delete"),
            AccountDeleteOutcome::Deleted { remaining: 1 }
        );
        let txn = fx.db.db.begin_read().expect("read");
        assert!(
            txn.open_table(ATTACHMENT_DATA)
                .expect("table")
                .is_empty()
                .expect("len")
        );
        for id in [early, late] {
            assert_eq!(fx.db.read_attachment_data(&id).expect("read"), None);
        }
    }

    #[test]
    fn writes_through_a_deleted_accounts_handle_should_be_refused() {
        let fx = fixture();
        let owned = fx.db.account(&fx.member);
        let board = fx.db.create_board(&fx.member, "Bob").expect("board");
        assert_eq!(
            fx.db.delete_account(&fx.member).expect("delete"),
            AccountDeleteOutcome::Deleted { remaining: 1 }
        );
        let refused = |result: DbResult<()>| {
            let error = result.expect_err("a write for a deleted account");
            assert_eq!(error.to_string(), ACCOUNT_GONE);
        };
        refused(owned.create_board("Again", "").map(drop));
        refused(owned.update_settings(&Settings::default()));
        refused(owned.set_active_timer(&ActiveTimer {
            task_id: None,
            mode: TimerMode::Stopwatch,
            started_at: 0,
            duration_secs: None,
        }));
        refused(
            owned
                .create_api_token("agent", AuthScope::Read, None)
                .map(drop),
        );
        refused(fx.db.create_board(&fx.member, "Direct").map(drop));
        assert!(fx.db.create_column(&board, "Todo", None).is_err());
        assert!(
            fx.db
                .list_boards()
                .expect("boards")
                .iter()
                .all(|row| row.owner_id != fx.member.as_str())
        );
    }
}
