use std::error::Error;
use std::fmt::{self, Write as _};

use chrono::Utc;
use rand::{RngCore, thread_rng};
use redb::WriteTransaction;
use uuid::Uuid;

use super::outbox::txn_remove_webhook_outbox;
use super::usage::{OverQuota, txn_remove_owned, txn_store_owned};
use super::{
    Bound, COLUMNS, Db, DbResult, MAX_WEBHOOKS_PER_BOARD, QuotaExceeded, WEBHOOKS,
    WEBHOOKS_BY_BOARD, mmap_get, mmap_insert, mmap_remove, read_one, txn_mmap_get, txn_read_one,
    txn_refuse_stranded_board,
};
use crate::models::{ColumnRow, WebhookEvent, WebhookRow};

/// Bytes of randomness in a webhook's secret.
const SECRET_BYTES: usize = 32;

/// A webhook to store, before it has an id, a secret or a creation time.
#[derive(Clone, Debug)]
pub struct NewWebhook {
    /// The board the webhook belongs to.
    pub board_id: String,
    /// The name the settings show.
    pub name: String,
    /// The URL the webhook calls.
    pub url: String,
    /// The events the webhook receives.
    pub events: Vec<WebhookEvent>,
    /// The column whose events the webhook receives, or `None` for every column.
    pub column_id: Option<String>,
    /// Whether the webhook receives events.
    pub enabled: bool,
}

/// Webhook fields one update may change, where `None` leaves a field alone.
#[derive(Clone, Debug, Default)]
pub struct WebhookPatch {
    /// The new name.
    pub name: Option<String>,
    /// The new URL.
    pub url: Option<String>,
    /// The new events.
    pub events: Option<Vec<WebhookEvent>>,
    /// The new column filter, where `Some(None)` clears it.
    pub column_id: Option<Option<String>>,
    /// The new enabled flag.
    pub enabled: Option<bool>,
}

/// Refusal of a webhook whose column filter names a column that is not on its board.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WebhookColumnMissing;

impl fmt::Display for WebhookColumnMissing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("column_id is not a column of this board")
    }
}

impl Error for WebhookColumnMissing {}

/// Refuses a column filter that names no column of board `board_id`, read inside an open write
/// transaction.
fn txn_check_filter(
    txn: &WriteTransaction,
    board_id: &str,
    column_id: Option<&str>,
) -> DbResult<()> {
    let Some(column_id) = column_id else {
        return Ok(());
    };
    match txn_read_one::<ColumnRow>(txn, COLUMNS, column_id)? {
        Some(column) if column.board_id == board_id => Ok(()),
        _ => Err(Box::new(WebhookColumnMissing)),
    }
}

/// Returns a fresh secret as lowercase hex.
fn new_secret() -> String {
    let mut raw = [0u8; SECRET_BYTES];
    thread_rng().fill_bytes(&mut raw);
    raw.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

impl Db {
    /// Returns webhook `id`.
    pub fn webhook(&self, id: &str) -> DbResult<Option<WebhookRow>> {
        read_one(&self.db, WEBHOOKS, id)
    }

    /// Returns a board's webhooks, oldest first.
    pub fn list_webhooks(&self, board_id: &str) -> DbResult<Vec<WebhookRow>> {
        let mut hooks = Vec::new();
        for id in mmap_get(&self.db, WEBHOOKS_BY_BOARD, board_id)? {
            hooks.extend(self.webhook(&id)?);
        }
        hooks.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(hooks)
    }

    /// Stores a new webhook on its board, giving it an id, a secret and a creation time, and
    /// refuses it with [`QuotaExceeded`] while the board holds [`MAX_WEBHOOKS_PER_BOARD`].
    pub fn create_webhook(&self, new: NewWebhook) -> DbResult<WebhookRow> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded_board(&txn, &new.board_id)?;
        let held = txn_mmap_get(&txn, WEBHOOKS_BY_BOARD, &new.board_id)?.len();
        if held >= MAX_WEBHOOKS_PER_BOARD {
            return Err(Box::new(QuotaExceeded {
                bound: Bound::WebhooksPerBoard,
                limit: MAX_WEBHOOKS_PER_BOARD as u64,
            }));
        }
        txn_check_filter(&txn, &new.board_id, new.column_id.as_deref())?;
        let hook = WebhookRow {
            id: Uuid::new_v4().to_string(),
            board_id: new.board_id,
            name: new.name,
            url: new.url,
            events: new.events,
            column_id: new.column_id,
            enabled: new.enabled,
            secret: new_secret(),
            created_at: Utc::now().to_rfc3339(),
        };
        txn_store_owned(&txn, WEBHOOKS, &hook.id, &hook, OverQuota::Refuse)?;
        mmap_insert(&txn, WEBHOOKS_BY_BOARD, &hook.board_id, &hook.id)?;
        txn.commit()?;
        Ok(hook)
    }

    /// Applies `patch` to webhook `id` and returns it as stored, or [`None`] when it is unknown.
    pub fn update_webhook(&self, id: &str, patch: WebhookPatch) -> DbResult<Option<WebhookRow>> {
        let txn = self.db.begin_write()?;
        let Some(mut hook) = txn_read_one::<WebhookRow>(&txn, WEBHOOKS, id)? else {
            return Ok(None);
        };
        if let Some(name) = patch.name {
            hook.name = name;
        }
        if let Some(url) = patch.url {
            hook.url = url;
        }
        if let Some(events) = patch.events {
            hook.events = events;
        }
        if let Some(column_id) = patch.column_id {
            txn_check_filter(&txn, &hook.board_id, column_id.as_deref())?;
            hook.column_id = column_id;
        }
        if let Some(enabled) = patch.enabled {
            hook.enabled = enabled;
        }
        txn_store_owned(&txn, WEBHOOKS, id, &hook, OverQuota::Refuse)?;
        txn.commit()?;
        Ok(Some(hook))
    }

    /// Deletes webhook `id` and the events queued for it. Returns false when it is unknown.
    pub fn delete_webhook(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(hook) = txn_remove_owned::<WebhookRow>(&txn, WEBHOOKS, id)? else {
            return Ok(false);
        };
        mmap_remove(&txn, WEBHOOKS_BY_BOARD, &hook.board_id, id)?;
        txn_remove_webhook_outbox(&txn, id)?;
        txn.commit()?;
        Ok(true)
    }
}
