use std::error::Error;
use std::fmt::{self, Write as _};

use chrono::Utc;
use rand::{RngCore, thread_rng};
use redb::WriteTransaction;
use uuid::Uuid;

use super::usage::{OverQuota, txn_remove_owned, txn_store_owned};
use super::{
    Bound, COLUMNS, Db, DbResult, INBOUND_ADDRESSES, INBOUND_BY_BOARD, INBOUND_BY_HASH,
    MAX_INBOUND_PER_BOARD, QuotaExceeded, mmap_get, mmap_insert, mmap_remove, read_in, read_one,
    txn_mmap_get, txn_read_one, txn_refuse_stranded_board,
};
use crate::auth::{constant_time_eq, hash_inbound_address};
use crate::models::{ColumnRow, InboundAddressRow};

/// Bytes of randomness in an inbound address's secret.
const SECRET_BYTES: usize = 32;

/// Most characters in an address prefix.
pub const MAX_PREFIX_CHARS: usize = 32;

/// An inbound address to store, before it has an id, a secret or a creation time.
#[derive(Clone, Debug)]
pub struct NewInbound {
    /// The board the address belongs to.
    pub board_id: String,
    /// The name the settings show.
    pub name: String,
    /// The prefix that starts the address, or empty for none.
    pub prefix: String,
    /// The column a task made from mail lands in.
    pub column_id: String,
}

/// An inbound address as stored, with the address itself, which no read returns again.
#[derive(Clone)]
pub struct IssuedInbound {
    /// The stored row, holding only the hash of the address.
    pub row: InboundAddressRow,
    /// The address, shown once.
    pub address: String,
}

impl fmt::Debug for IssuedInbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedInbound")
            .field("row", &self.row)
            .field("address", &"<redacted>")
            .finish()
    }
}

/// Refusal of an inbound address whose column is not on its board.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InboundColumnMissing;

impl fmt::Display for InboundColumnMissing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("column_id is not a column of this board")
    }
}

impl Error for InboundColumnMissing {}

/// Refuses a column that is not on board `board_id`, read inside an open write transaction.
fn txn_check_column(txn: &WriteTransaction, board_id: &str, column_id: &str) -> DbResult<()> {
    match txn_read_one::<ColumnRow>(txn, COLUMNS, column_id)? {
        Some(column) if column.board_id == board_id => Ok(()),
        _ => Err(Box::new(InboundColumnMissing)),
    }
}

/// Returns a fresh address made of `prefix` and a random secret.
fn new_address(prefix: &str) -> String {
    let mut raw = [0u8; SECRET_BYTES];
    thread_rng().fill_bytes(&mut raw);
    let mut address = String::new();
    if !prefix.is_empty() {
        address.push_str(prefix);
        address.push('-');
    }
    for byte in raw {
        let _ = write!(address, "{byte:02x}");
    }
    address
}

/// Removes inbound address `id` and its hash entry without counting the freed bytes, inside an
/// open write transaction, for the removal of a whole board that counts them once.
pub(super) fn txn_drop_inbound(txn: &WriteTransaction, id: &str) -> DbResult<()> {
    if let Some(row) = txn_read_one::<InboundAddressRow>(txn, INBOUND_ADDRESSES, id)? {
        txn.open_table(INBOUND_BY_HASH)?
            .remove(row.token_hash.as_str())?;
    }
    txn.open_table(INBOUND_ADDRESSES)?.remove(id)?;
    Ok(())
}

/// Removes inbound address `id`, its hash entry and its stored bytes, inside an open write
/// transaction.
pub(super) fn txn_remove_inbound(txn: &WriteTransaction, id: &str) -> DbResult<()> {
    if let Some(row) = txn_remove_owned::<InboundAddressRow>(txn, INBOUND_ADDRESSES, id)? {
        txn.open_table(INBOUND_BY_HASH)?
            .remove(row.token_hash.as_str())?;
    }
    Ok(())
}

impl Db {
    /// Returns inbound address `id`.
    pub fn inbound_address(&self, id: &str) -> DbResult<Option<InboundAddressRow>> {
        read_one(&self.db, INBOUND_ADDRESSES, id)
    }

    /// Returns a board's inbound addresses, oldest first.
    pub fn list_inbound(&self, board_id: &str) -> DbResult<Vec<InboundAddressRow>> {
        let mut rows = Vec::new();
        for id in mmap_get(&self.db, INBOUND_BY_BOARD, board_id)? {
            rows.extend(self.inbound_address(&id)?);
        }
        rows.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(rows)
    }

    /// Stores a new inbound address on its board and returns it with the address, refusing it
    /// with [`QuotaExceeded`] while the board holds [`MAX_INBOUND_PER_BOARD`].
    pub fn create_inbound(&self, new: NewInbound) -> DbResult<IssuedInbound> {
        let txn = self.db.begin_write()?;
        txn_refuse_stranded_board(&txn, &new.board_id)?;
        let held = txn_mmap_get(&txn, INBOUND_BY_BOARD, &new.board_id)?.len();
        if held >= MAX_INBOUND_PER_BOARD {
            return Err(Box::new(QuotaExceeded {
                bound: Bound::InboundPerBoard,
                limit: MAX_INBOUND_PER_BOARD as u64,
            }));
        }
        txn_check_column(&txn, &new.board_id, &new.column_id)?;
        let address = new_address(&new.prefix);
        let row = InboundAddressRow {
            id: Uuid::new_v4().to_string(),
            board_id: new.board_id,
            name: new.name,
            prefix: new.prefix,
            column_id: new.column_id,
            token_hash: hash_inbound_address(&address),
            created_at: Utc::now().to_rfc3339(),
        };
        txn_store_owned(&txn, INBOUND_ADDRESSES, &row.id, &row, OverQuota::Refuse)?;
        txn.open_table(INBOUND_BY_HASH)?
            .insert(row.token_hash.as_str(), row.id.as_str())?;
        mmap_insert(&txn, INBOUND_BY_BOARD, &row.board_id, &row.id)?;
        txn.commit()?;
        Ok(IssuedInbound { row, address })
    }

    /// Gives inbound address `id` a new secret, so the old address stops working at once, and
    /// returns it, or [`None`] when it is unknown.
    pub fn rotate_inbound(&self, id: &str) -> DbResult<Option<IssuedInbound>> {
        let txn = self.db.begin_write()?;
        let Some(mut row) = txn_read_one::<InboundAddressRow>(&txn, INBOUND_ADDRESSES, id)? else {
            return Ok(None);
        };
        txn_refuse_stranded_board(&txn, &row.board_id)?;
        let address = new_address(&row.prefix);
        txn.open_table(INBOUND_BY_HASH)?
            .remove(row.token_hash.as_str())?;
        row.token_hash = hash_inbound_address(&address);
        txn_store_owned(&txn, INBOUND_ADDRESSES, id, &row, OverQuota::Allow)?;
        txn.open_table(INBOUND_BY_HASH)?
            .insert(row.token_hash.as_str(), id)?;
        txn.commit()?;
        Ok(Some(IssuedInbound { row, address }))
    }

    /// Deletes inbound address `id`. Returns `false` when it is unknown.
    pub fn delete_inbound(&self, id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let Some(row) = txn_read_one::<InboundAddressRow>(&txn, INBOUND_ADDRESSES, id)? else {
            return Ok(false);
        };
        txn_remove_inbound(&txn, id)?;
        mmap_remove(&txn, INBOUND_BY_BOARD, &row.board_id, id)?;
        txn.commit()?;
        Ok(true)
    }

    /// Returns the inbound address that `address` names, found by its hash and then confirmed
    /// against the stored hash in constant time.
    pub fn inbound_by_address(&self, address: &str) -> DbResult<Option<InboundAddressRow>> {
        let hash = hash_inbound_address(address);
        let txn = self.read()?;
        let Some(id) = txn
            .open_table(INBOUND_BY_HASH)?
            .get(hash.as_str())?
            .map(|guard| guard.value().to_string())
        else {
            return Ok(None);
        };
        let Some(row) = read_in::<InboundAddressRow>(&txn, INBOUND_ADDRESSES, &id)? else {
            return Ok(None);
        };
        Ok(constant_time_eq(row.token_hash.as_bytes(), hash.as_bytes()).then_some(row))
    }
}
