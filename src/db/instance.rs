use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Response;

use super::{
    AccountAction, AccountSummary, AdminActionOutcome, AppState, Db, DbResult, no_signed_in_account,
};
use crate::api_error::{ErrorCode, coded_response};
use crate::auth::AuthUser;
use crate::backup::{Backups, RunError};
use crate::models::{
    AccountRole, BackupSettings, BackupStatus, InstanceSettings, InviteRow, PruneStatus,
};

/// Instance settings for the admin. It has no board-content methods at all, so holding it never
/// reaches another account's boards.
#[derive(Clone, Debug)]
pub struct InstanceDb {
    /// The database the handle reads and writes.
    db: Db,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for InstanceDb {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        match parts.extensions.get::<AuthUser>() {
            Some(user) if user.role == AccountRole::Admin => Ok(Self {
                db: state.db.clone(),
            }),
            Some(_) => Err(coded_response(
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                "this action needs the instance admin",
            )),
            None => Err(no_signed_in_account()),
        }
    }
}

impl InstanceDb {
    /// Days a request to join waits for the admin before it is deleted.
    pub const PENDING_REQUEST_DAYS: i64 = Db::PENDING_REQUEST_DAYS;

    /// Returns the backup schedule settings, or their defaults when never saved.
    pub fn backup_settings(&self) -> DbResult<BackupSettings> {
        self.db.backup_settings()
    }

    /// Stores the backup schedule settings.
    pub fn update_backup_settings(&self, settings: &BackupSettings) -> DbResult<()> {
        self.db.update_backup_settings(settings)
    }

    /// Returns the outcome of the latest backup attempt, if there was one.
    pub fn backup_status(&self) -> DbResult<Option<BackupStatus>> {
        self.db.backup_status()
    }

    /// Stores an invite under the hash of its token, first deleting expired invites.
    pub fn create_invite(&self, hash: &str, invite: &InviteRow, now: i64) -> DbResult<()> {
        self.db.create_invite(hash, invite, now)
    }

    /// Returns the unused invites, oldest first.
    pub fn list_invites(&self) -> DbResult<Vec<InviteRow>> {
        self.db.list_invites()
    }

    /// Revokes the unused invite `id`. Returns `false` when no unused invite has that id.
    pub fn delete_invite(&self, id: &str) -> DbResult<bool> {
        self.db.delete_invite(id)
    }

    /// Returns every account with when it was last seen and how many boards it owns, oldest
    /// first.
    pub fn account_summaries(&self) -> DbResult<Vec<AccountSummary>> {
        self.db.account_summaries()
    }

    /// Moves the member account `id` along `action`, refusing the admin and a status `action` does
    /// not start from.
    pub fn apply_account_action(
        &self,
        id: &str,
        action: AccountAction,
    ) -> DbResult<AdminActionOutcome> {
        self.db.apply_account_action(id, action)
    }

    /// Deletes the member account `id` and everything it owns, refusing the admin.
    pub fn delete_member(&self, id: &str) -> DbResult<AdminActionOutcome> {
        self.db.delete_member(id)
    }

    /// Returns the instance settings: its mode, its registration mode and its pending-account cap.
    pub fn instance_settings(&self) -> DbResult<InstanceSettings> {
        self.db.instance_settings()
    }

    /// Changes the stored instance settings with `edit` in one transaction and returns them.
    pub fn update_settings(
        &self,
        edit: impl FnOnce(&mut InstanceSettings),
    ) -> DbResult<InstanceSettings> {
        self.db.update_instance_settings(edit)
    }

    /// Returns the outcome of the latest run of the unused-board cleanup, if there was one.
    pub fn prune_status(&self) -> DbResult<Option<PruneStatus>> {
        self.db.prune_status()
    }

    /// Takes a backup into `backups` now and returns its outcome.
    pub async fn run_backup(&self, backups: &Backups, now: i64) -> Result<BackupStatus, RunError> {
        backups.run(&self.db, now).await
    }
}
