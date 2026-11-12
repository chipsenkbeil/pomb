use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Response;

use super::{
    AccountAction, AccountSummary, AdminActionOutcome, AppState, Db, DbResult, LargestUse,
    no_signed_in_account,
};
use crate::api_error::{ErrorCode, coded_response, permission_denied};
use crate::auth::{AuthUser, CredentialKind};
use crate::backup::{Backups, RunError};
use crate::models::{
    AccountRole, AuthScope, BackupSettings, BackupStatus, InstanceSettings, InviteRow, PruneStatus,
    UnnamedFiles,
};
use crate::permissions::Permission;

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

/// The instance handle for the backup routes: the admin's browser session or a token of the admin
/// account that holds `settings.write`. A member, a lead token and a token below the admin preset
/// get a 403 `permission_denied`.
#[derive(Clone, Debug)]
pub struct SettingsDb(InstanceDb);

impl SettingsDb {
    /// Returns the instance handle the extractor checked.
    #[must_use]
    pub fn instance(&self) -> &InstanceDb {
        &self.0
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for SettingsDb {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let Some(user) = parts.extensions.get::<AuthUser>() else {
            return Err(no_signed_in_account());
        };
        if user.role == AccountRole::Admin && user.has(Permission::SettingsWrite) {
            Ok(Self(InstanceDb {
                db: state.db.clone(),
            }))
        } else {
            Err(permission_denied(Permission::SettingsWrite))
        }
    }
}

/// The instance handle for the invite and sign-up approval routes: the admin's browser session or
/// a root Admin preset token of the admin account, never the bootstrap token. A member, a token
/// below the Admin preset and a child token get a 403 `permission_denied`.
#[derive(Clone, Debug)]
pub struct RootAdminDb(InstanceDb);

impl RootAdminDb {
    /// Returns the instance handle the extractor checked.
    #[must_use]
    pub fn instance(&self) -> &InstanceDb {
        &self.0
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for RootAdminDb {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let Some(user) = parts.extensions.get::<AuthUser>() else {
            return Err(no_signed_in_account());
        };
        if user.role == AccountRole::Admin
            && user.kind != CredentialKind::Bootstrap
            && user.scope == AuthScope::Admin
            && user.parent_token.is_none()
        {
            Ok(Self(InstanceDb {
                db: state.db.clone(),
            }))
        } else {
            Err(permission_denied(Permission::SettingsWrite))
        }
    }
}

impl InstanceDb {
    /// Days a request to join waits for the admin before it is deleted.
    pub const PENDING_REQUEST_DAYS: i64 = Db::PENDING_REQUEST_DAYS;

    /// Returns the most that one account, one board and one attachment file hold.
    pub fn largest_use(&self) -> DbResult<LargestUse> {
        self.db.largest_use()
    }

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

    /// Returns how many attachment files no task names and their bytes.
    pub fn unnamed_attachment_files(&self) -> DbResult<UnnamedFiles> {
        self.db.unnamed_attachment_files()
    }

    /// Deletes the attachment files no task names and returns how many it deleted and their
    /// bytes.
    pub fn remove_unnamed_attachment_files(&self) -> DbResult<UnnamedFiles> {
        self.db.remove_unnamed_attachment_files()
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

#[cfg(test)]
mod tests {
    use axum::http::Request;

    use super::*;
    use crate::auth::{AuthChoice, AuthMode, SignIn, user_for_token};
    use crate::db::PublicDb;

    #[tokio::test]
    async fn instance_should_save_backup_settings_through_the_extractor_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("pomb.redb").to_str().expect("utf8 path"))
            .expect("connect");
        let admin = db.create_user("admin").expect("create user");
        let session = db.create_session(&admin, "").expect("session");
        let sign_in =
            SignIn::new(AuthChoice::Chosen(AuthMode::None), "127.0.0.1", None).expect("none mode");
        let state = AppState::new(db.clone(), None, sign_in);
        let user = user_for_token(&PublicDb::new(db.clone()), &session).expect("admin session");
        let (mut parts, _) = Request::new(()).into_parts();
        parts.extensions.insert(user);

        let settings = SettingsDb::from_request_parts(&mut parts, &state)
            .await
            .expect("settings.write");
        settings
            .instance()
            .update_backup_settings(&BackupSettings {
                enabled: true,
                interval_hours: 12,
                keep: 3,
            })
            .expect("save");
        let stored = db.backup_settings().expect("read");

        assert_eq!(
            (stored.enabled, stored.interval_hours, stored.keep),
            (true, 12, 3)
        );
    }
}
