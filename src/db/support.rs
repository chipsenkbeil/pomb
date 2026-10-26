use redb::{ReadableDatabase, ReadableTableMetadata};

use super::{ATTACHMENT_DATA, BOARD_ACTIVITY, Db, META, SCHEMA_VERSION_KEY};

impl Db {
    /// Stores `at` as the time board `board` was last used, even when it is earlier, and drops
    /// the touches recorded in memory for it.
    pub(crate) fn set_board_last_used(&self, board: &str, at: i64) {
        self.forget_board_activity(board);
        let txn = self.db.begin_write().expect("write");
        txn.open_table(BOARD_ACTIVITY)
            .expect("table")
            .insert(board, at)
            .expect("insert");
        txn.commit().expect("commit");
    }

    /// Returns the stored time board `board` was last used, without the touches in memory.
    pub(crate) fn stored_board_last_used(&self, board: &str) -> Option<i64> {
        let txn = self.db.begin_read().expect("read");
        txn.open_table(BOARD_ACTIVITY)
            .expect("table")
            .get(board)
            .expect("get")
            .map(|at| at.value())
    }

    /// Returns how many attachment files the database holds.
    pub(crate) fn attachment_data_count(&self) -> u64 {
        let txn = self.db.begin_read().expect("read");
        txn.open_table(ATTACHMENT_DATA)
            .expect("table")
            .len()
            .expect("len")
    }

    /// Turns the file into one a version 13 binary wrote: no table of board uses, stamped 13.
    pub(crate) fn rewind_to_schema_13(&self) {
        let txn = self.db.begin_write().expect("write");
        txn.delete_table(BOARD_ACTIVITY).expect("drop table");
        txn.open_table(META)
            .expect("meta")
            .insert(SCHEMA_VERSION_KEY, 13)
            .expect("stamp");
        txn.commit().expect("commit");
    }
}
