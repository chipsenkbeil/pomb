use redb::{ReadableDatabase, ReadableTableMetadata};

use super::{
    API_TOKENS, ATTACHMENT_DATA, ApiTokenRecord, BOARD_ACTIVITY, Db, DbResult, META,
    SCHEMA_VERSION_KEY, rows_in,
};

impl Db {
    /// Returns the live API tokens of every account, oldest first.
    pub(crate) fn list_api_tokens(&self) -> DbResult<Vec<ApiTokenRecord>> {
        let txn = self.db.begin_read()?;
        let mut out: Vec<ApiTokenRecord> = rows_in(&txn.open_table(API_TOKENS)?)?;
        out.sort_by_key(|t| t.created_at);
        Ok(out)
    }

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
