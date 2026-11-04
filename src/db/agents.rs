use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use chrono::Utc;
use redb::WriteTransaction;
use uuid::Uuid;

use super::{
    AGENT_ROLE_GRANTS, AGENT_ROLES, AGENT_ROLES_BY_ACCOUNT, BOARDS, Bound, Db, DbResult,
    GRANTS_BY_BOARD, GRANTS_BY_ROLE, MAX_AGENT_ROLES_PER_ACCOUNT, QuotaExceeded, RoleTokenRefusal,
    mmap_get, mmap_insert, mmap_remove, read_one, txn_check_child_token, txn_create_token,
    txn_mmap_get, txn_read_one, txn_remove_token, txn_role_token_ids, write_one,
};
use crate::models::{AgentRoleGrantRow, AgentRoleRow, ApiTokenRecord, BoardRow};
use crate::permissions::{Permission, STANDARD_ROLES};

/// Refusal of an agent role whose name another role of the account already holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AgentRoleNameTaken;

impl fmt::Display for AgentRoleNameTaken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("another role of this account has that name")
    }
}

impl Error for AgentRoleNameTaken {}

/// The standard roles of an account after [`Db::ensure_standard_roles`].
#[derive(Clone, Debug)]
pub struct StandardRoles {
    /// The six standard roles, in the order of [`STANDARD_ROLES`].
    pub roles: Vec<AgentRoleRow>,
    /// The names of the roles the call created.
    pub created: Vec<String>,
}

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

/// Stores the grant of `role_id` on `board_id` for `account` unless it exists, inside an open write
/// transaction.
fn txn_grant(txn: &WriteTransaction, account: &str, role_id: &str, board_id: &str) -> DbResult<()> {
    let key = grant_key(role_id, board_id);
    if txn_read_one::<AgentRoleGrantRow>(txn, AGENT_ROLE_GRANTS, &key)?.is_none() {
        let grant = AgentRoleGrantRow {
            account_id: account.to_string(),
            role_id: role_id.to_string(),
            board_id: board_id.to_string(),
        };
        write_one(txn, AGENT_ROLE_GRANTS, &key, &grant)?;
        mmap_insert(txn, GRANTS_BY_BOARD, board_id, role_id)?;
        mmap_insert(txn, GRANTS_BY_ROLE, role_id, board_id)?;
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

/// Deletes the tokens issued as role `role_id` of `account` that reach `board_id`, or every token
/// issued as it when `board_id` is [`None`], with the tokens they issued, found through the role
/// and board index, inside an open write transaction. Returns how many live tokens it deleted.
pub(super) fn txn_revoke_role_tokens(
    txn: &WriteTransaction,
    account: &str,
    role_id: &str,
    board_id: Option<&str>,
) -> DbResult<u64> {
    let mut removed = 0;
    for id in txn_role_token_ids(txn, role_id, board_id)? {
        removed += txn_remove_token(txn, account, &id)?;
    }
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

    /// Stores `record`, which names its parent, as a child token issued as agent role `role_id` of
    /// the account it belongs to, living `hours` from `now`, with the role's id and current name
    /// and the next `agent-N` label from the role's counter, and returns it as stored. It runs the
    /// checks of [`Db::create_child_api_token`] in the same write, and returns the refusal,
    /// storing nothing, when that account holds no such role or one of them fails. A failed store
    /// leaves the counter where it was, and a label is never issued twice.
    pub fn create_api_token_as_role(
        &self,
        role_id: &str,
        mut record: ApiTokenRecord,
        hours: i64,
        now: i64,
    ) -> DbResult<Result<ApiTokenRecord, RoleTokenRefusal>> {
        let txn = self.db.begin_write()?;
        let Some(mut role) = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, role_id)?
            .filter(|role| role.account_id == record.account_id)
        else {
            return Ok(Err(RoleTokenRefusal::UnknownRole));
        };
        if let Err(refusal) = txn_check_child_token(&txn, &mut record, hours, now)? {
            return Ok(Err(RoleTokenRefusal::Child(refusal)));
        }
        role.issued += 1;
        record.role_id = Some(role.id.clone());
        record.role_name = Some(role.name.clone());
        record.label = Some(format!("agent-{}", role.issued));
        write_one(&txn, AGENT_ROLES, role_id, &role)?;
        txn_create_token(&txn, &record)?;
        txn.commit()?;
        Ok(Ok(record))
    }

    /// Stores `record`, a token the account owner made, as issued as agent role `role_id` of the
    /// account it belongs to, with the role's id and current name and the next `agent-N` label from
    /// the role's counter, and returns it as stored. Returns [`None`], storing nothing, when that
    /// account holds no such role. A refused store leaves the counter where it was.
    pub fn create_owner_token_as_role(
        &self,
        role_id: &str,
        mut record: ApiTokenRecord,
    ) -> DbResult<Option<ApiTokenRecord>> {
        let txn = self.db.begin_write()?;
        let Some(mut role) = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, role_id)?
            .filter(|role| role.account_id == record.account_id)
        else {
            return Ok(None);
        };
        role.issued += 1;
        record.role_id = Some(role.id.clone());
        record.role_name = Some(role.name.clone());
        record.label = Some(format!("agent-{}", role.issued));
        write_one(&txn, AGENT_ROLES, role_id, &role)?;
        txn_create_token(&txn, &record)?;
        txn.commit()?;
        Ok(Some(record))
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
            issued: 0,
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
        txn_grant(&txn, account, role_id, board_id)?;
        txn.commit()?;
        Ok(true)
    }

    /// Creates each standard role `account` lacks from [`STANDARD_ROLES`], leaves every role it
    /// holds as it is, and grants every board in `boards` to all six, in one transaction. Returns
    /// [`None`], storing nothing, when a board is not one the account owns. A role count past
    /// [`MAX_AGENT_ROLES_PER_ACCOUNT`] is refused with [`QuotaExceeded`].
    pub fn ensure_standard_roles(
        &self,
        account: &str,
        boards: &[String],
    ) -> DbResult<Option<StandardRoles>> {
        let txn = self.db.begin_write()?;
        for board_id in boards {
            let owned = txn_read_one::<BoardRow>(&txn, BOARDS, board_id)?
                .is_some_and(|board| board.owner_id == account);
            if !owned {
                return Ok(None);
            }
        }
        let mut held = Vec::new();
        for id in txn_mmap_get(&txn, AGENT_ROLES_BY_ACCOUNT, account)? {
            held.extend(txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, &id)?);
        }
        let mut count = held.len();
        let now = Utc::now().timestamp();
        let mut roles = Vec::new();
        let mut created = Vec::new();
        for standard in STANDARD_ROLES {
            if let Some(role) = held.iter().find(|role| role.name == standard.name) {
                roles.push(role.clone());
                continue;
            }
            if count >= MAX_AGENT_ROLES_PER_ACCOUNT {
                return Err(Box::new(QuotaExceeded {
                    bound: Bound::AgentRoles,
                    limit: MAX_AGENT_ROLES_PER_ACCOUNT as u64,
                }));
            }
            count += 1;
            let role = AgentRoleRow {
                id: Uuid::new_v4().to_string(),
                account_id: account.to_string(),
                name: standard.name.to_string(),
                permissions: standard.permissions(),
                created_at: now,
                updated_at: now,
                issued: 0,
            };
            write_one(&txn, AGENT_ROLES, &role.id, &role)?;
            mmap_insert(&txn, AGENT_ROLES_BY_ACCOUNT, account, &role.id)?;
            created.push(role.name.clone());
            roles.push(role);
        }
        for role in &roles {
            for board_id in boards {
                txn_grant(&txn, account, &role.id, board_id)?;
            }
        }
        txn.commit()?;
        Ok(Some(StandardRoles { roles, created }))
    }

    /// Revokes agent role `role_id` of `account` on board `board_id`, and in the same write
    /// deletes the tokens issued as the role that reach the board and the tokens they issued.
    /// Returns how many live tokens it deleted, or [`None`] when the account holds no such role or
    /// the role held no such grant.
    pub fn revoke_agent_role(
        &self,
        account: &str,
        role_id: &str,
        board_id: &str,
    ) -> DbResult<Option<u64>> {
        let txn = self.db.begin_write()?;
        let held = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, role_id)?
            .is_some_and(|role| role.account_id == account);
        if !held || !txn_remove_grant(&txn, role_id, board_id)? {
            return Ok(None);
        }
        let removed = txn_revoke_role_tokens(&txn, account, role_id, Some(board_id))?;
        txn.commit()?;
        Ok(Some(removed))
    }

    /// Deletes the tokens issued as role `role_id` of `account` that reach `board_id`, or every
    /// token issued as it when `board_id` is [`None`], with the tokens they issued, and keeps the
    /// role and its grants. Returns how many live tokens it deleted, or [`None`] when the account
    /// holds no such role.
    pub fn revoke_role_tokens(
        &self,
        account: &str,
        role_id: &str,
        board_id: Option<&str>,
    ) -> DbResult<Option<u64>> {
        let txn = self.db.begin_write()?;
        let held = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, role_id)?
            .is_some_and(|role| role.account_id == account);
        if !held {
            return Ok(None);
        }
        let removed = txn_revoke_role_tokens(&txn, account, role_id, board_id)?;
        txn.commit()?;
        Ok(Some(removed))
    }

    /// Deletes agent role `role_id` of `account` with its grants and index rows, and in the same
    /// write every token issued as it and the tokens they issued. Returns how many live tokens it
    /// deleted, or [`None`] when the account holds no such role.
    pub fn delete_agent_role(&self, account: &str, role_id: &str) -> DbResult<Option<u64>> {
        let txn = self.db.begin_write()?;
        let held = txn_read_one::<AgentRoleRow>(&txn, AGENT_ROLES, role_id)?
            .is_some_and(|role| role.account_id == account);
        if !held {
            return Ok(None);
        }
        let removed = txn_revoke_role_tokens(&txn, account, role_id, None)?;
        for board_id in txn_mmap_get(&txn, GRANTS_BY_ROLE, role_id)? {
            txn_remove_grant(&txn, role_id, &board_id)?;
        }
        txn.open_table(AGENT_ROLES)?.remove(role_id)?;
        mmap_remove(&txn, AGENT_ROLES_BY_ACCOUNT, account, role_id)?;
        txn.commit()?;
        Ok(Some(removed))
    }
}
