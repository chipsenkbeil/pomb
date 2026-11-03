use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use chrono::Utc;
use redb::WriteTransaction;
use uuid::Uuid;

use super::{
    AGENT_ROLE_GRANTS, AGENT_ROLES, AGENT_ROLES_BY_ACCOUNT, BOARDS, Bound, Db, DbResult,
    GRANTS_BY_BOARD, GRANTS_BY_ROLE, MAX_AGENT_ROLES_PER_ACCOUNT, QuotaExceeded, mmap_get,
    mmap_insert, mmap_remove, read_one, txn_mmap_get, txn_read_one, write_one,
};
use crate::models::{AgentRoleGrantRow, AgentRoleRow, BoardRow};
use crate::permissions::Permission;

/// Refusal of an agent role whose name another role of the account already holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AgentRoleNameTaken;

impl fmt::Display for AgentRoleNameTaken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("another role of this account has that name")
    }
}

impl Error for AgentRoleNameTaken {}

/// Returns the key of the grant of `role_id` on `board_id` in [`AGENT_ROLE_GRANTS`].
fn grant_key(role_id: &str, board_id: &str) -> String {
    format!("{role_id}/{board_id}")
}

/// Refuses with [`AgentRoleNameTaken`] when a role of `account` other than `except` holds `name`,
/// read inside an open write transaction.
fn txn_refuse_taken_name(
    txn: &WriteTransaction,
    account: &str,
    name: &str,
    except: Option<&str>,
) -> DbResult<()> {
    for id in txn_mmap_get(txn, AGENT_ROLES_BY_ACCOUNT, account)? {
        if except == Some(id.as_str()) {
            continue;
        }
        let taken = txn_read_one::<AgentRoleRow>(txn, AGENT_ROLES, &id)?
            .is_some_and(|role| role.name == name);
        if taken {
            return Err(Box::new(AgentRoleNameTaken));
        }
    }
    Ok(())
}

/// Removes the grant of `role_id` on `board_id` and its index entries, inside an open write
/// transaction, and returns whether it existed.
fn txn_remove_grant(txn: &WriteTransaction, role_id: &str, board_id: &str) -> DbResult<bool> {
    let removed = txn
        .open_table(AGENT_ROLE_GRANTS)?
        .remove(grant_key(role_id, board_id).as_str())?
        .is_some();
    mmap_remove(txn, GRANTS_BY_BOARD, board_id, role_id)?;
    mmap_remove(txn, GRANTS_BY_ROLE, role_id, board_id)?;
    Ok(removed)
}

/// Removes every agent role's grant on board `board_id`, inside an open write transaction.
pub(super) fn txn_remove_grants_of_board(txn: &WriteTransaction, board_id: &str) -> DbResult<()> {
    for role_id in txn_mmap_get(txn, GRANTS_BY_BOARD, board_id)? {
        txn_remove_grant(txn, &role_id, board_id)?;
    }
    Ok(())
}

/// Removes every agent role of `account` with its grants, inside an open write transaction.
pub(super) fn txn_remove_agent_roles_of(txn: &WriteTransaction, account: &str) -> DbResult<()> {
    for role_id in txn_mmap_get(txn, AGENT_ROLES_BY_ACCOUNT, account)? {
        for board_id in txn_mmap_get(txn, GRANTS_BY_ROLE, &role_id)? {
            txn_remove_grant(txn, &role_id, &board_id)?;
        }
        txn.open_table(AGENT_ROLES)?.remove(role_id.as_str())?;
    }
    txn.open_multimap_table(AGENT_ROLES_BY_ACCOUNT)?
        .remove_all(account)?;
    Ok(())
}

impl Db {
    /// Returns agent role `id` of `account`, or [`None`] when the account holds no such role.
    pub fn agent_role(&self, account: &str, id: &str) -> DbResult<Option<AgentRoleRow>> {
        let role = read_one::<AgentRoleRow>(&self.db, AGENT_ROLES, id)?;
        Ok(role.filter(|role| role.account_id == account))
    }

    /// Returns the agent roles of `account`, by name.
    pub fn list_agent_roles(&self, account: &str) -> DbResult<Vec<AgentRoleRow>> {
        let mut roles = Vec::new();
        for id in mmap_get(&self.db, AGENT_ROLES_BY_ACCOUNT, account)? {
            roles.extend(self.agent_role(account, &id)?);
        }
        roles.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(roles)
    }

    /// Returns the ids of the boards agent role `id` is granted on, sorted.
    pub fn agent_role_boards(&self, id: &str) -> DbResult<Vec<String>> {
        let mut boards = mmap_get(&self.db, GRANTS_BY_ROLE, id)?;
        boards.sort();
        Ok(boards)
    }

    /// Stores a new agent role for `account` under a fresh id, and refuses it with
    /// [`AgentRoleNameTaken`] while another role holds the name, or with [`QuotaExceeded`] while
    /// the account holds [`MAX_AGENT_ROLES_PER_ACCOUNT`].
    pub fn create_agent_role(
        &self,
        account: &str,
        name: &str,
        permissions: BTreeSet<Permission>,
    ) -> DbResult<AgentRoleRow> {
        let txn = self.db.begin_write()?;
        if txn_mmap_get(&txn, AGENT_ROLES_BY_ACCOUNT, account)?.len() >= MAX_AGENT_ROLES_PER_ACCOUNT
        {
            return Err(Box::new(QuotaExceeded {
                bound: Bound::AgentRoles,
                limit: MAX_AGENT_ROLES_PER_ACCOUNT as u64,
            }));
        }
        txn_refuse_taken_name(&txn, account, name, None)?;
        let now = Utc::now().timestamp();
        let role = AgentRoleRow {
            id: Uuid::new_v4().to_string(),
            account_id: account.to_string(),
            name: name.to_string(),
            permissions,
            created_at: now,
            updated_at: now,
        };
        write_one(&txn, AGENT_ROLES, &role.id, &role)?;
        mmap_insert(&txn, AGENT_ROLES_BY_ACCOUNT, account, &role.id)?;
        txn.commit()?;
        Ok(role)
    }

    /// Renames agent role `id` of `account` and replaces its template where `name` and
    /// `permissions` are set, and returns it as stored, or [`None`] when the account holds no such
    /// role. A name another role holds is refused with [`AgentRoleNameTaken`].
    pub fn update_agent_role(
        &self,
        account: &str,
        id: &str,
        name: Option<&str>,
        permissions: Option<BTreeSet<Permission>>,
    ) -> DbResult<Option<AgentRoleRow>> {
        let txn = self.db.begin_write()?;
        let Some(mut role) = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, id)?
            .filter(|role| role.account_id == account)
        else {
            return Ok(None);
        };
        if let Some(name) = name {
            txn_refuse_taken_name(&txn, account, name, Some(id))?;
            role.name = name.to_string();
        }
        if let Some(permissions) = permissions {
            role.permissions = permissions;
        }
        role.updated_at = Utc::now().timestamp();
        write_one(&txn, AGENT_ROLES, id, &role)?;
        txn.commit()?;
        Ok(Some(role))
    }

    /// Grants agent role `role_id` of `account` on board `board_id` of the same account, and does
    /// nothing when it holds the grant already. Returns `false` when the account holds no such role
    /// or board.
    pub fn grant_agent_role(&self, account: &str, role_id: &str, board_id: &str) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let role = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, role_id)?;
        let board = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)?;
        if !role.is_some_and(|role| role.account_id == account)
            || !board.is_some_and(|board| board.owner_id == account)
        {
            return Ok(false);
        }
        let grant = AgentRoleGrantRow {
            account_id: account.to_string(),
            role_id: role_id.to_string(),
            board_id: board_id.to_string(),
        };
        let key = grant_key(role_id, board_id);
        if txn_read_one::<AgentRoleGrantRow>(&txn, AGENT_ROLE_GRANTS, &key)?.is_none() {
            write_one(&txn, AGENT_ROLE_GRANTS, &key, &grant)?;
            mmap_insert(&txn, GRANTS_BY_BOARD, board_id, role_id)?;
            mmap_insert(&txn, GRANTS_BY_ROLE, role_id, board_id)?;
        }
        txn.commit()?;
        Ok(true)
    }

    /// Revokes agent role `role_id` of `account` on board `board_id`. Returns `false` when the
    /// account holds no such role or the role held no such grant.
    pub fn revoke_agent_role(
        &self,
        account: &str,
        role_id: &str,
        board_id: &str,
    ) -> DbResult<bool> {
        let txn = self.db.begin_write()?;
        let held = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, role_id)?
            .is_some_and(|role| role.account_id == account);
        if !held || !txn_remove_grant(&txn, role_id, board_id)? {
            return Ok(false);
        }
        txn.commit()?;
        Ok(true)
    }
}
