use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::sync::{Mutex, PoisonError};

use redb::{
    Database, ReadTransaction, ReadableDatabase, StorageError, TransactionError, WriteTransaction,
};
use tokio::sync::watch;

use super::DbResult;
use super::events::txn_take_wakeups;

/// The redb database, which wakes the waiters on a board's events when a commit wrote some.
#[derive(Debug)]
pub struct Store {
    /// The database every read and write goes through.
    database: Database,
    /// The waiters on each board's events, by board id.
    feeds: Mutex<HashMap<String, watch::Sender<()>>>,
}

impl Store {
    /// Returns the store around `database`.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            database,
            feeds: Mutex::default(),
        }
    }

    /// Returns the database itself, for a test to read its stamp.
    #[cfg(test)]
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Begins a read transaction.
    pub fn begin_read(&self) -> Result<ReadTransaction, TransactionError> {
        self.database.begin_read()
    }

    /// Begins the write transaction, waiting for the one that may be open.
    pub fn begin_write(&self) -> Result<WriteTxn<'_>, TransactionError> {
        Ok(WriteTxn {
            txn: self.database.begin_write()?,
            store: self,
        })
    }

    /// Returns a receiver that sees every commit that wrote events of `board` from now on, and
    /// forgets the feeds nobody waits on.
    #[must_use]
    pub fn board_feed(&self, board: &str) -> watch::Receiver<()> {
        let mut feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        feeds.retain(|_, feed| feed.receiver_count() > 0);
        feeds
            .entry(board.to_string())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe()
    }

    /// Wakes the waiters on the events of `boards`.
    fn wake(&self, boards: &[String]) {
        let feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        for feed in boards.iter().filter_map(|board| feeds.get(board)) {
            feed.send_replace(());
        }
    }
}

/// A write transaction of a [`Store`], whose commit wakes the waiters on the events it wrote.
pub struct WriteTxn<'a> {
    /// The transaction every write goes through.
    txn: WriteTransaction,
    /// The store whose waiters the commit wakes.
    store: &'a Store,
}

impl fmt::Debug for WriteTxn<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteTxn").finish_non_exhaustive()
    }
}

impl WriteTxn<'_> {
    /// Commits the transaction, then wakes the waiters on the events it wrote.
    pub fn commit(self) -> DbResult<()> {
        let boards = txn_take_wakeups(&self.txn)?;
        self.txn.commit()?;
        if !boards.is_empty() {
            self.store.wake(&boards);
        }
        Ok(())
    }

    /// Aborts the transaction, leaving every waiter asleep.
    pub fn abort(self) -> Result<(), StorageError> {
        self.txn.abort()
    }
}

impl Deref for WriteTxn<'_> {
    type Target = WriteTransaction;

    fn deref(&self) -> &WriteTransaction {
        &self.txn
    }
}
