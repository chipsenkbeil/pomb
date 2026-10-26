use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, WriteTransaction};
use serde::de::DeserializeOwned;

use super::{
    ACCOUNT_SETTINGS, ACTIVE_TIMER, API_TOKENS, DEVICE_AUTHS, Db, DbResult, INSTANCE_SETTINGS_KEY,
    INVITES, PASSKEYS, RECOVERY_CODES, Rows, SESSIONS, SETTINGS, STARTER_BOARD_NAME, TEMPLATES,
    TIME_ENTRIES, USERNAMES, USERS, read_one, rows_in, txn_credential_registered, txn_insert_user,
    txn_owner_boards, txn_read_one, txn_refuse_stranded, txn_remove_board, write_one,
};
use crate::models::{
    AccountId, AccountRole, ApiTokenRecord, BoardTemplateRow, DeviceAuthRow, InstanceMode,
    InstanceSettings, InviteRow, PasskeyRow, RecoveryCodeRow, SessionRow, TimeEntryRow, UserRow,
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

/// An account that registers through an invite, with its first passkey and recovery code.
#[derive(Copy, Clone, Debug)]
pub struct NewAccount<'a> {
    /// The member account, its handle already in canonical form.
    pub user: &'a UserRow,
    /// The account's first passkey.
    pub passkey: &'a PasskeyRow,
    /// The account's recovery code.
    pub recovery: &'a RecoveryCodeRow,
    /// The hash of the invite token the registration uses up.
    pub invite_hash: &'a str,
}

/// Typed outcome for [`Db::register_account`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// The account, its passkey and its recovery code are stored, and the invite is used up.
    Registered,
    /// Refused: the invite is used, revoked, expired or unknown.
    InviteGone,
    /// Refused: another account has the handle.
    HandleTaken,
    /// Refused: a passkey with this credential id is registered already.
    DuplicatePasskey,
}

/// Typed outcome for [`Db::recover_account`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecoverOutcome {
    /// The passkey and the new code are stored, and every session of the account is gone.
    Recovered,
    /// Refused: the recovery code changed since the ceremony started.
    StaleCode,
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

/// Deletes every row of `table` that `owned` picks, inside an open write transaction. A row that
/// does not parse stays.
fn remove_rows<T>(txn: &WriteTransaction, table: Rows, owned: impl Fn(&T) -> bool) -> DbResult<()>
where
    T: DeserializeOwned,
{
    txn.open_table(table)?.retain(|_, raw| {
        serde_json::from_slice::<T>(raw)
            .map(|row| !owned(&row))
            .unwrap_or(true)
    })?;
    Ok(())
}

impl Db {
    /// Stores `mode` as the instance's mode, refusing single mode while more than one account
    /// exists.
    pub fn apply_mode(&self, mode: InstanceMode) -> DbResult<ModeSwitch> {
        let txn = self.db.begin_write()?;
        let stored: Option<InstanceSettings> = txn_read_one(&txn, SETTINGS, INSTANCE_SETTINGS_KEY)?;
        let from = stored.map_or(InstanceMode::Single, |settings| settings.mode);
        if from == mode {
            return Ok(ModeSwitch::Kept);
        }
        let accounts = txn.open_table(USERS)?.len()?;
        if mode == InstanceMode::Single && accounts > 1 {
            return Ok(ModeSwitch::Refused { accounts });
        }
        write_one(
            &txn,
            SETTINGS,
            INSTANCE_SETTINGS_KEY,
            &InstanceSettings { mode },
        )?;
        txn.commit()?;
        Ok(ModeSwitch::Switched { from })
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
        let invite: Option<InviteRow> = txn_read_one(&txn, INVITES, new.invite_hash)?;
        if invite.is_none_or(|invite| invite.is_expired(now)) {
            return Ok(RegisterOutcome::InviteGone);
        }
        if handle_owner_in(&txn.open_table(USERNAMES)?, &new.user.username)?.is_some() {
            return Ok(RegisterOutcome::HandleTaken);
        }
        if txn_credential_registered(&txn, new.passkey)? {
            return Ok(RegisterOutcome::DuplicatePasskey);
        }
        txn_insert_user(&txn, new.user)?;
        write_one(&txn, PASSKEYS, &new.passkey.id, new.passkey)?;
        write_one(&txn, RECOVERY_CODES, &new.user.id, new.recovery)?;
        txn.open_table(INVITES)?.remove(new.invite_hash)?;
        let starter = rows_in::<BoardTemplateRow, _>(&txn.open_table(TEMPLATES)?)?
            .into_iter()
            .find(|template| template.built_in && template.name == Db::BUILTIN_TEMPLATE_NAME)
            .ok_or("the built-in board template is missing")?;
        Db::build_from_template(&txn, &new.user.account_id(), &starter, STARTER_BOARD_NAME)?;
        txn.commit()?;
        Ok(RegisterOutcome::Registered)
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
    /// `recovery` and deletes all its sessions, keeping its other passkeys. Refuses when the stored
    /// code is no longer the one hashed as `expected_hash`.
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
        if txn_credential_registered(&txn, passkey)? {
            return Ok(RecoverOutcome::DuplicatePasskey);
        }
        write_one(&txn, PASSKEYS, &passkey.id, passkey)?;
        write_one(&txn, RECOVERY_CODES, account.as_str(), recovery)?;
        remove_rows(&txn, SESSIONS, |session: &SessionRow| {
            session.user_id == account.as_str()
        })?;
        txn.commit()?;
        Ok(RecoverOutcome::Recovered)
    }

    /// Deletes `account` and every row it owns in one transaction: its boards with everything in
    /// them, attachment data, templates, time entries, tokens, device requests, sessions,
    /// passkeys, recovery code, invites, settings, timer and handle. The admin's account is
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
        for board in txn_owner_boards(&txn, account.as_str())? {
            txn_remove_board(&txn, &board)?;
        }
        let id = account.as_str();
        remove_rows(&txn, TIME_ENTRIES, |entry: &TimeEntryRow| {
            entry.account_id == id
        })?;
        remove_rows(&txn, TEMPLATES, |template: &BoardTemplateRow| {
            template.owner_id.as_deref() == Some(id)
        })?;
        remove_rows(&txn, API_TOKENS, |token: &ApiTokenRecord| {
            token.account_id == id
        })?;
        remove_rows(&txn, DEVICE_AUTHS, |request: &DeviceAuthRow| {
            request.account_id.as_deref() == Some(id)
        })?;
        remove_rows(&txn, SESSIONS, |session: &SessionRow| session.user_id == id)?;
        remove_rows(&txn, PASSKEYS, |passkey: &PasskeyRow| passkey.user_id == id)?;
        remove_rows(&txn, INVITES, |invite: &InviteRow| invite.created_by == id)?;
        for table in [RECOVERY_CODES, ACCOUNT_SETTINGS, ACTIVE_TIMER, USERS] {
            txn.open_table(table)?.remove(id)?;
        }
        txn.open_table(USERNAMES)?.remove(user.username.as_str())?;
        txn.commit()?;
        Ok(AccountDeleteOutcome::Deleted {
            remaining: accounts - 1,
        })
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
        refused(owned.create_board("Again").map(drop));
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
