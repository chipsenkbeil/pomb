use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Response;

use super::{AppState, Db, DbResult, no_signed_in_account};
use crate::api_error::{ErrorCode, coded_response};
use crate::auth::AuthUser;
use crate::backup::{Backups, RunError};
use crate::models::{AccountRole, BackupSettings, BackupStatus};

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

    /// Takes a backup into `backups` now and returns its outcome.
    pub async fn run_backup(&self, backups: &Backups, now: i64) -> Result<BackupStatus, RunError> {
        backups.run(&self.db, now).await
    }
}
