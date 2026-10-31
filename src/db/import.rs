use std::path::PathBuf;

use redb::WriteTransaction;
use serde_json::{Map, Value};

use super::usage::{
    OverQuota, txn_count_usage, txn_refuse_over_bytes, txn_store_entry, txn_store_owned,
    txn_store_task,
};
use super::{
    COLORS_BY_BOARD, COLUMNS, COLUMNS_BY_BOARD, Db, DbResult, ENTRIES_BY_TASK, FIELD_DEFS,
    FIELDS_BY_BOARD, LINKS_BY_TARGET, SWIMLANES, SWIMLANES_BY_BOARD, TASK_COLORS, TASKS,
    TASKS_BY_COLUMN, mmap_insert, txn_ensure_board_colors, txn_ensure_single_default,
    txn_insert_board, txn_merge_board_config, txn_refuse_over_quota,
};
use crate::models::{
    AccountId, Actor, ColorRow, ColumnRow, FieldDefRow, Quota, SwimlaneRow, TaskRow, TextCap,
    TimeEntryRow,
};

/// The spooled bytes of one attachment's file.
#[derive(Clone, Debug)]
pub struct FileImport {
    /// The id of the attachment of the task row the bytes belong to.
    pub id: String,
    /// The file that holds the bytes.
    pub path: PathBuf,
    /// The file's size in bytes.
    pub size: u64,
}

/// One task of an import, with the files of its attachments and its time entries.
#[derive(Clone, Debug)]
pub struct TaskImport {
    /// The task row, whose column, swimlane, color and link targets are ids of the import.
    pub row: TaskRow,
    /// The file of each attachment of `row`.
    pub files: Vec<FileImport>,
    /// The time entries logged on the task.
    pub entries: Vec<TimeEntryRow>,
}

/// Everything a new board is made of, every id already fresh and every reference inside it
/// resolved to one, except the `board_id` of the rows of the layout, which the write fills in.
#[derive(Clone, Debug)]
pub struct BoardImport {
    /// The board's name, already validated.
    pub name: String,
    /// The board's settings bag, without any secret.
    pub settings: Map<String, Value>,
    /// The board's task colors.
    pub colors: Vec<ColorRow>,
    /// The board's columns.
    pub columns: Vec<ColumnRow>,
    /// The board's swimlanes.
    pub swimlanes: Vec<SwimlaneRow>,
    /// The board's custom field definitions.
    pub fields: Vec<FieldDefRow>,
    /// The board's tasks.
    pub tasks: Vec<TaskImport>,
}

impl Db {
    /// Stores `import` as a new board of `owner`, recording the API token `created_by_token` that
    /// asked for it, or none when the string is empty, and returns the board's id.
    ///
    /// Everything is written in one transaction, through the same checks as the writes that make
    /// each row one at a time. A quota, a text cap or a row cap that the import breaks refuses
    /// the whole board and writes nothing. The tasks are `actor`'s.
    pub(super) fn import_board(
        &self,
        owner: &AccountId,
        import: BoardImport,
        created_by_token: &str,
        actor: Actor<'_>,
    ) -> DbResult<String> {
        let file_ids: Vec<String> = import
            .tasks
            .iter()
            .flat_map(|task| task.files.iter().map(|file| file.id.clone()))
            .collect();
        let stored = self.write_board(owner, import, created_by_token, actor);
        if stored.is_err() {
            self.files.remove(&file_ids);
        }
        stored
    }

    /// Writes the board and its tasks in one transaction.
    fn write_board(
        &self,
        owner: &AccountId,
        import: BoardImport,
        created_by_token: &str,
        actor: Actor<'_>,
    ) -> DbResult<String> {
        let txn = self.db.begin_write()?;
        let board_id = txn_insert_board(
            &txn,
            owner,
            &import.name,
            created_by_token,
            OverQuota::Refuse,
        )?;
        TextCap::CONFIG.check(&Value::Object(import.settings.clone()).to_string())?;
        txn_merge_board_config(&txn, &board_id, import.settings)?;

        for mut color in import.colors {
            color.board_id.clone_from(&board_id);
            txn_store_owned(&txn, TASK_COLORS, &color.id, &color, OverQuota::Refuse)?;
            mmap_insert(&txn, COLORS_BY_BOARD, &board_id, &color.id)?;
        }
        txn_ensure_board_colors(&txn, &board_id)?;
        txn_ensure_single_default(&txn, &board_id)?;

        txn_refuse_over_quota(
            &txn,
            owner.as_str(),
            Quota::ColumnsPerBoard,
            import.columns.len(),
        )?;
        for mut column in import.columns {
            column.board_id.clone_from(&board_id);
            txn_store_owned(&txn, COLUMNS, &column.id, &column, OverQuota::Refuse)?;
            mmap_insert(&txn, COLUMNS_BY_BOARD, &board_id, &column.id)?;
        }
        txn_refuse_over_quota(
            &txn,
            owner.as_str(),
            Quota::SwimlanesPerBoard,
            import.swimlanes.len(),
        )?;
        for mut lane in import.swimlanes {
            lane.board_id.clone_from(&board_id);
            txn_store_owned(&txn, SWIMLANES, &lane.id, &lane, OverQuota::Refuse)?;
            mmap_insert(&txn, SWIMLANES_BY_BOARD, &board_id, &lane.id)?;
        }
        txn_refuse_over_quota(
            &txn,
            owner.as_str(),
            Quota::FieldsPerBoard,
            import.fields.len(),
        )?;
        for mut field in import.fields {
            field.board_id.clone_from(&board_id);
            txn_store_owned(&txn, FIELD_DEFS, &field.id, &field, OverQuota::Refuse)?;
            mmap_insert(&txn, FIELDS_BY_BOARD, &board_id, &field.id)?;
        }

        txn_refuse_over_quota(
            &txn,
            owner.as_str(),
            Quota::TasksPerBoard,
            import.tasks.len(),
        )?;
        for task in import.tasks {
            self.import_task(&txn, owner, task, actor)?;
        }
        txn.commit()?;
        Ok(board_id)
    }

    /// Stores one task of an import with its attachment files and time entries, inside an open
    /// write transaction.
    fn import_task(
        &self,
        txn: &WriteTransaction,
        owner: &AccountId,
        task: TaskImport,
        actor: Actor<'_>,
    ) -> DbResult<()> {
        let TaskImport {
            row,
            files,
            entries,
        } = task;
        let account = owner.as_str();
        txn_refuse_over_quota(txn, account, Quota::CommentsPerTask, row.comments.len())?;
        txn_refuse_over_quota(
            txn,
            account,
            Quota::AttachmentsPerTask,
            row.attachments.len(),
        )?;
        for file in &files {
            txn_refuse_over_bytes(txn, account, file.size, 0)?;
            txn_count_usage(txn, account, file.size, 0)?;
            self.files.copy_from(&file.id, &file.path)?;
        }
        txn_store_task(
            txn,
            &mut txn.open_table(TASKS)?,
            &row,
            OverQuota::Refuse,
            actor,
        )?;
        mmap_insert(txn, TASKS_BY_COLUMN, &row.column_id, &row.id)?;
        for link in &row.links {
            mmap_insert(txn, LINKS_BY_TARGET, &link.target_id, &row.id)?;
        }
        for entry in &entries {
            txn_store_entry(txn, entry, OverQuota::Refuse)?;
            mmap_insert(txn, ENTRIES_BY_TASK, &row.id, &entry.id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use redb::ReadableTable;

    use super::*;
    use crate::db::ACCOUNT_USAGE;
    use crate::db::usage::txn_recount_usage;
    use crate::models::TaskAttachment;

    fn recounted(db: &Db) -> HashMap<String, u64> {
        let txn = db.db.begin_write().expect("write");
        txn_recount_usage(&txn).expect("recount");
        let mut usage = HashMap::new();
        for item in txn
            .open_table(ACCOUNT_USAGE)
            .expect("table")
            .iter()
            .expect("iter")
        {
            let (account, bytes) = item.expect("row");
            usage.insert(account.value().to_string(), bytes.value());
        }
        txn.abort().expect("abort");
        usage
    }

    #[test]
    fn import_board_should_count_everything_it_stores_against_its_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("import.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.create_user("chip").expect("user");
        let column = ColumnRow {
            id: "column".to_string(),
            board_id: String::new(),
            name: "Todo".to_string(),
            position: 0,
            wip_limit: None,
            is_done: false,
            description: String::new(),
            collapsed: false,
            config_json: "{}".to_string(),
            former_names: Vec::new(),
        };
        let file_id = uuid::Uuid::new_v4().to_string();
        let mut row = Db::new_task_row("column", None, "Task", 1, None);
        row.attachments.push(TaskAttachment {
            id: file_id.clone(),
            name: "a.bin".to_string(),
            mime: "application/octet-stream".to_string(),
            size: 4096,
            uploaded_by: "chip".to_string(),
            token_name: None,
            created_at: Utc::now().to_rfc3339(),
        });
        let entry = TimeEntryRow {
            id: "entry".to_string(),
            task_id: row.id.clone(),
            minutes: 5,
            seconds: 300,
            note: "worked".to_string(),
            started_at: Utc::now().to_rfc3339(),
            kind: "manual".to_string(),
            interrupted: false,
            interrupt_reason: None,
            labels: Vec::new(),
            created_by: "chip".to_string(),
            activity_id: None,
            account_id: owner.as_str().to_string(),
            role: None,
            token_name: None,
        };
        let file = dir.path().join("spooled");
        std::fs::write(&file, vec![7u8; 4096]).expect("spool");
        let import = BoardImport {
            name: "Imported".to_string(),
            settings: Map::new(),
            colors: Vec::new(),
            columns: vec![column],
            swimlanes: Vec::new(),
            fields: Vec::new(),
            tasks: vec![TaskImport {
                row,
                files: vec![FileImport {
                    id: file_id.clone(),
                    path: file,
                    size: 4096,
                }],
                entries: vec![entry],
            }],
        };

        let board = db
            .import_board(&owner, import, "", Actor::SYSTEM)
            .expect("import");

        let kept = db.stored_bytes_by_account().expect("usage");
        assert!(kept[owner.as_str()] > 4096, "{kept:?}");
        assert_eq!(kept, recounted(&db));
        assert_eq!(db.board_tasks(&board).expect("tasks").len(), 1);
        assert_eq!(db.account_entries(&owner).expect("entries").len(), 1);
        let colors = db.list_colors(&board).expect("colors");
        assert_eq!(colors.iter().filter(|color| color.is_default).count(), 1);
        assert_eq!(
            db.read_attachment_data(&file_id)
                .expect("read")
                .expect("bytes"),
            vec![7u8; 4096]
        );
    }

    fn task_with_file(position: &str, id: &str, spooled: PathBuf) -> TaskImport {
        let mut row = Db::new_task_row("column", None, position, 1, None);
        row.attachments.push(TaskAttachment {
            id: id.to_string(),
            name: "a.bin".to_string(),
            mime: "application/octet-stream".to_string(),
            size: 16,
            uploaded_by: "chip".to_string(),
            token_name: None,
            created_at: Utc::now().to_rfc3339(),
        });
        TaskImport {
            row,
            files: vec![FileImport {
                id: id.to_string(),
                path: spooled,
                size: 16,
            }],
            entries: Vec::new(),
        }
    }

    #[test]
    fn import_board_should_leave_the_attachment_directory_empty_when_it_fails_after_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("import.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let owner = db.create_user("chip").expect("user");
        let spooled = dir.path().join("spooled");
        std::fs::write(&spooled, [7u8; 16]).expect("spool");
        let first = task_with_file("First", &uuid::Uuid::new_v4().to_string(), spooled);
        let second = task_with_file(
            "Second",
            &uuid::Uuid::new_v4().to_string(),
            dir.path().join("never-spooled"),
        );
        let import = BoardImport {
            name: "Imported".to_string(),
            settings: Map::new(),
            colors: Vec::new(),
            columns: vec![ColumnRow {
                id: "column".to_string(),
                board_id: String::new(),
                name: "Todo".to_string(),
                position: 0,
                wip_limit: None,
                is_done: false,
                description: String::new(),
                collapsed: false,
                config_json: "{}".to_string(),
                former_names: Vec::new(),
            }],
            swimlanes: Vec::new(),
            fields: Vec::new(),
            tasks: vec![first, second],
        };
        let before = db.stored_bytes_by_account().expect("usage");
        let boards = db.owner_boards(&owner).expect("boards").len();

        db.import_board(&owner, import, "", Actor::SYSTEM)
            .expect_err("the second file is missing");

        assert_eq!(db.files.count(), 0);
        assert_eq!(db.owner_boards(&owner).expect("boards").len(), boards);
        assert_eq!(db.stored_bytes_by_account().expect("usage"), before);
    }
}
