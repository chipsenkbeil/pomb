use std::collections::{HashMap, HashSet};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use redb::{ReadableMultimapTable, ReadableTable, WriteTransaction};

use super::outbox::txn_remove_event;
use super::{
    Db, DbResult, WEBHOOK_OUTBOX, WEBHOOK_OUTBOX_BY_HOOK, WEBHOOKS, mmap_remove, txn_read_one,
    write_one,
};
use crate::models::{OutboxRow, WebhookEvent, WebhookRow};

/// One queued event claimed for a delivery try, with what the try needs from its webhook.
#[derive(Clone, Debug)]
pub struct Delivery {
    /// The outbox key of the event.
    pub key: String,
    /// The webhook the event is for.
    pub webhook_id: String,
    /// What happened.
    pub event: WebhookEvent,
    /// The JSON body to send as stored.
    pub body: String,
    /// Count of failed tries.
    pub attempts: u32,
    /// The URL the webhook calls.
    pub url: String,
    /// Hex of the random bytes that sign the body.
    pub secret: String,
}

/// Returns `at` as the text an outbox row stores.
#[must_use]
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Returns whether `row` is due at `now`.
#[must_use]
fn is_due(row: &OutboxRow, now: DateTime<Utc>) -> bool {
    row.next_attempt_at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .is_none_or(|at| at <= now)
}

/// Removes event `key` and its index entry inside an open write transaction.
fn txn_drop_event(txn: &WriteTransaction, key: &str) -> DbResult<()> {
    if let Some(row) = txn_remove_event(txn, key)? {
        mmap_remove(txn, WEBHOOK_OUTBOX_BY_HOOK, &row.webhook_id, key)?;
    }
    Ok(())
}

/// Removes every index entry whose event is gone, inside an open write transaction.
fn txn_drop_orphan_index_entries(txn: &WriteTransaction) -> DbResult<()> {
    let events = txn.open_table(WEBHOOK_OUTBOX)?;
    let mut index = txn.open_multimap_table(WEBHOOK_OUTBOX_BY_HOOK)?;
    let mut orphans = Vec::new();
    for item in index.iter()? {
        let (hook, values) = item?;
        for value in values {
            let value = value?;
            if events.get(value.value())?.is_none() {
                orphans.push((hook.value().to_string(), value.value().to_string()));
            }
        }
    }
    for (hook, key) in orphans {
        index.remove(hook.as_str(), key.as_str())?;
    }
    Ok(())
}

impl Db {
    /// Handles up to `limit` of the oldest queued events due at `now`, at most one for each
    /// webhook and none for a webhook in `busy`. An event whose webhook is gone or disabled is
    /// removed, and so is one that cannot be decoded, along with any index entry left without an
    /// event. Any other is held back for `lease` and returned, so no second claim takes it while
    /// its try runs or the lease lasts.
    pub fn claim_deliveries(
        &self,
        now: DateTime<Utc>,
        lease: TimeDelta,
        limit: usize,
        busy: &HashSet<String>,
    ) -> DbResult<Vec<Delivery>> {
        let mut due = Vec::new();
        let mut undecodable = Vec::new();
        let mut taken: HashSet<String> = HashSet::new();
        {
            let txn = self.db.begin_read()?;
            let table = txn.open_table(WEBHOOK_OUTBOX)?;
            let hooks = txn.open_table(WEBHOOKS)?;
            let mut enabled: HashMap<String, bool> = HashMap::new();
            for item in table.iter()? {
                if due.len() >= limit {
                    break;
                }
                let (key, guard) = item?;
                let key = key.value().to_string();
                let Ok(row) = serde_json::from_slice::<OutboxRow>(guard.value()) else {
                    undecodable.push(key);
                    continue;
                };
                if !is_due(&row, now) {
                    continue;
                }
                let live = match enabled.get(&row.webhook_id) {
                    Some(live) => *live,
                    None => {
                        let live = match hooks.get(row.webhook_id.as_str())? {
                            Some(guard) => {
                                serde_json::from_slice::<WebhookRow>(guard.value())?.enabled
                            }
                            None => false,
                        };
                        enabled.insert(row.webhook_id.clone(), live);
                        live
                    }
                };
                if !live || (!busy.contains(&row.webhook_id) && taken.insert(row.webhook_id)) {
                    due.push(key);
                }
            }
        }
        if due.is_empty() && undecodable.is_empty() {
            return Ok(Vec::new());
        }
        let held_until = stamp(now + lease);
        let txn = self.db.begin_write()?;
        for key in &undecodable {
            txn.open_table(WEBHOOK_OUTBOX)?.remove(key.as_str())?;
            eprintln!("pomb: dropped a webhook event that could not be read");
        }
        if !undecodable.is_empty() {
            txn_drop_orphan_index_entries(&txn)?;
        }
        let mut claimed = Vec::new();
        for key in due {
            let Some(mut row) = txn_read_one::<OutboxRow>(&txn, WEBHOOK_OUTBOX, &key)? else {
                continue;
            };
            if !is_due(&row, now) {
                continue;
            }
            let hook = txn_read_one::<WebhookRow>(&txn, WEBHOOKS, &row.webhook_id)?
                .filter(|hook| hook.enabled);
            let Some(hook) = hook else {
                txn_drop_event(&txn, &key)?;
                continue;
            };
            row.next_attempt_at = Some(held_until.clone());
            write_one(&txn, WEBHOOK_OUTBOX, &key, &row)?;
            claimed.push(Delivery {
                key,
                webhook_id: row.webhook_id,
                event: row.event,
                body: row.body,
                attempts: row.attempts,
                url: hook.url,
                secret: hook.secret,
            });
        }
        txn.commit()?;
        Ok(claimed)
    }

    /// Removes event `key` once it needs no more tries.
    pub fn finish_delivery(&self, key: &str) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        txn_drop_event(&txn, key)?;
        txn.commit()?;
        Ok(())
    }

    /// Records one more failed try of event `key` and makes `retry_at` its next try time. A missing event
    /// stays missing.
    pub fn retry_delivery(&self, key: &str, retry_at: DateTime<Utc>) -> DbResult<()> {
        let txn = self.db.begin_write()?;
        if let Some(mut row) = txn_read_one::<OutboxRow>(&txn, WEBHOOK_OUTBOX, key)? {
            row.attempts += 1;
            row.next_attempt_at = Some(stamp(retry_at));
            write_one(&txn, WEBHOOK_OUTBOX, key, &row)?;
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewWebhook;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Db,
        column: String,
        board: String,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("claim.redb");
            let db = Db::connect(path.to_str().expect("utf8")).expect("connect");
            let owner = db.create_user("admin").expect("admin");
            let board = db.create_board(&owner, "Hooks").expect("board");
            let column = db.create_column(&board, "Todo", None).expect("column");
            Self {
                _dir: dir,
                db,
                column,
                board,
            }
        }

        fn webhook(&self) -> WebhookRow {
            self.db
                .create_webhook(NewWebhook {
                    board_id: self.board.clone(),
                    name: "Hook".to_string(),
                    url: "https://example.com/hook".to_string(),
                    events: vec![WebhookEvent::TaskCreated],
                    column_id: None,
                    enabled: true,
                })
                .expect("webhook")
        }

        fn queue(&self, name: &str) {
            self.db
                .create_task(&self.column, None, name, 1, None)
                .expect("task");
        }

        fn claim(&self, limit: usize, busy: &HashSet<String>) -> Vec<Delivery> {
            self.db
                .claim_deliveries(Utc::now(), TimeDelta::seconds(30), limit, busy)
                .expect("claim")
        }
    }

    #[test]
    fn claim_deliveries_should_take_one_event_for_each_webhook() {
        let fx = Fixture::new();
        let first = fx.webhook();
        let second = fx.webhook();
        fx.queue("One");
        fx.queue("Two");
        let claimed = fx.claim(10, &HashSet::new());
        let mut hooks: Vec<_> = claimed.iter().map(|d| d.webhook_id.clone()).collect();
        hooks.sort();
        let mut expected = vec![first.id, second.id];
        expected.sort();
        assert_eq!(hooks, expected);
    }

    #[test]
    fn claim_deliveries_should_skip_a_busy_webhook() {
        let fx = Fixture::new();
        let busy = fx.webhook();
        let free = fx.webhook();
        fx.queue("One");
        let claimed = fx.claim(10, &HashSet::from([busy.id.clone()]));
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].webhook_id, free.id);
        assert_eq!(fx.db.webhook_outbox(&busy.id).expect("outbox").len(), 1);
    }

    #[test]
    fn claim_deliveries_should_drop_an_unreadable_event_and_keep_scanning() {
        let fx = Fixture::new();
        let hook = fx.webhook();
        fx.queue("Readable");
        let txn = fx.db.db.begin_write().expect("write");
        txn.open_table(WEBHOOK_OUTBOX)
            .expect("table")
            .insert("00000000000000000000", b"not json".as_slice())
            .expect("insert");
        txn.commit().expect("commit");
        let claimed = fx.claim(10, &HashSet::new());
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].webhook_id, hook.id);
        let txn = fx.db.db.begin_read().expect("read");
        let table = txn.open_table(WEBHOOK_OUTBOX).expect("table");
        assert!(table.get("00000000000000000000").expect("get").is_none());
    }

    #[test]
    fn claim_deliveries_should_drop_the_index_entry_of_an_unreadable_event() {
        let fx = Fixture::new();
        let hook = fx.webhook();
        fx.queue("Readable");
        let txn = fx.db.db.begin_write().expect("write");
        txn.open_table(WEBHOOK_OUTBOX)
            .expect("table")
            .insert("00000000000000000000", b"not json".as_slice())
            .expect("insert");
        txn.open_multimap_table(WEBHOOK_OUTBOX_BY_HOOK)
            .expect("index")
            .insert(hook.id.as_str(), "00000000000000000000")
            .expect("insert");
        txn.commit().expect("commit");
        let claimed = fx.claim(10, &HashSet::new());
        assert_eq!(claimed.len(), 1);
        let txn = fx.db.db.begin_read().expect("read");
        let index = txn
            .open_multimap_table(WEBHOOK_OUTBOX_BY_HOOK)
            .expect("index");
        let keys: Vec<String> = index
            .get(hook.id.as_str())
            .expect("get")
            .map(|guard| guard.expect("value").value().to_string())
            .collect();
        assert_eq!(keys.len(), 1);
        assert_ne!(keys[0], "00000000000000000000");
    }

    #[test]
    fn claim_deliveries_should_drain_a_disabled_webhook_in_one_claim() {
        let fx = Fixture::new();
        let hook = fx.webhook();
        fx.queue("One");
        fx.queue("Two");
        fx.queue("Three");
        let mut off = hook.clone();
        off.enabled = false;
        let txn = fx.db.db.begin_write().expect("write");
        write_one(&txn, WEBHOOKS, &hook.id, &off).expect("write");
        txn.commit().expect("commit");
        let claimed = fx.claim(10, &HashSet::new());
        assert!(claimed.is_empty());
        assert!(fx.db.webhook_outbox(&hook.id).expect("outbox").is_empty());
    }
}
