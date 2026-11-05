use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use super::DbResult;
use crate::models::UnnamedFiles;

const PART_SUFFIX: &str = ".part";
const DIR_SUFFIX: &str = ".attachments";

/// An attachment being received chunk by chunk into a `.part` file, which is deleted when this is
/// dropped before the file is placed.
#[derive(Debug)]
pub struct Incoming {
    /// The id the attachment will have.
    id: String,
    /// The `.part` file that holds the bytes so far.
    part: PathBuf,
    /// The open handle the chunks are written to.
    file: tokio::fs::File,
    /// The bytes written so far.
    size: u64,
    /// The SHA-256 of the bytes written so far.
    hasher: Sha256,
}

impl Incoming {
    /// Returns the id the attachment will have.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns how many bytes have been written.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Appends `chunk` to the file.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the write fails.
    pub async fn push(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.file.write_all(chunk).await?;
        self.size += chunk.len() as u64;
        self.hasher.update(chunk);
        Ok(())
    }

    /// Returns the SHA-256 of the bytes written so far as lowercase hex.
    pub fn sha256(&self) -> String {
        format!("{:x}", self.hasher.clone().finalize())
    }

    /// Flushes the file to disk, after the last chunk.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the flush fails.
    pub async fn finish(&mut self) -> io::Result<()> {
        self.file.flush().await?;
        self.file.sync_all().await
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        drop(fs::remove_file(&self.part));
    }
}

/// The directory beside the database file that holds the bytes of every attachment, one file per
/// attachment named by its id.
#[derive(Clone, Debug)]
pub struct AttachmentFiles {
    dir: Arc<Path>,
    /// Ids of the attachment files whose delete failed and that the next remove or bytes call
    /// retries.
    stuck: Arc<Mutex<HashSet<String>>>,
    /// Ids of the files an upload placed whose task row is not stored yet, which a removal of
    /// unnamed files leaves alone.
    held: Arc<Mutex<HashSet<String>>>,
}

/// A claim on an attachment file that no task row names yet, released when dropped.
#[derive(Debug)]
pub(crate) struct Held {
    /// The id of the file.
    id: String,
    /// The set the id is released from.
    held: Arc<Mutex<HashSet<String>>>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl AttachmentFiles {
    /// Returns the store for the database file at `database`, creating its directory.
    pub(crate) fn open(database: &Path) -> DbResult<Self> {
        let files = Self::beside(database);
        fs::create_dir_all(&*files.dir)?;
        let parent = files
            .dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        File::open(parent.unwrap_or(Path::new(".")))?.sync_all()?;
        Ok(files)
    }

    /// Returns the store for the database file at `database`, without creating anything.
    pub(crate) fn beside(database: &Path) -> Self {
        let name = database
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("pomb.redb");
        let dir = database
            .parent()
            .unwrap_or(Path::new(""))
            .join(format!("{name}{DIR_SUFFIX}"));
        Self {
            dir: dir.into(),
            stuck: Arc::default(),
            held: Arc::default(),
        }
    }

    /// Returns the store an offline reader uses for the database file at `database`: the
    /// directory beside it, or for a Pomb backup `pomb-<stamp>.redb` the `pomb-<stamp>.attachments`
    /// directory that backup wrote, when only that one exists.
    pub(crate) fn beside_or_backup(database: &Path) -> Self {
        let files = Self::beside(database);
        if files.dir.is_dir() || database.extension() != Some(OsStr::new("redb")) {
            return files;
        }
        let backup = database.with_extension(DIR_SUFFIX.trim_start_matches('.'));
        if backup.is_dir() {
            return Self {
                dir: backup.into(),
                stuck: files.stuck,
                held: files.held,
            };
        }
        files
    }

    /// Returns where the attachment `id` is stored, or an error when `id` is not a canonical
    /// UUID, so no stored name can climb out of the directory.
    fn path_of(&self, id: &str) -> DbResult<PathBuf> {
        match Uuid::try_parse(id) {
            Ok(uuid) if uuid.hyphenated().to_string() == id => Ok(self.dir.join(id)),
            _ => Err(format!("attachment id {id:?} is not a generated id").into()),
        }
    }

    /// Checks that `id` is a generated id.
    pub(crate) fn check_id(&self, id: &str) -> DbResult<()> {
        self.path_of(id).map(drop)
    }

    /// Stores `bytes` as attachment `id`.
    pub(crate) fn write(&self, id: &str, bytes: &[u8]) -> DbResult<()> {
        self.place(id, |part| part.write_all(bytes))
    }

    /// Stores the file at `source` as attachment `id`.
    pub(crate) fn copy_from(&self, id: &str, source: &Path) -> DbResult<()> {
        let mut source = File::open(source)?;
        self.place(id, |part| io::copy(&mut source, part).map(drop))
    }

    /// Writes the file of `id` through a `.part` file that `fill` writes, flushed before it is
    /// renamed into place.
    fn place(&self, id: &str, fill: impl FnOnce(&mut File) -> io::Result<()>) -> DbResult<()> {
        let target = self.path_of(id)?;
        let part = self.dir.join(format!("{id}{PART_SUFFIX}"));
        let written = File::create(&part).and_then(|mut file| {
            fill(&mut file)?;
            file.sync_all()
        });
        let placed = written.and_then(|()| self.settle(&part, &target));
        if placed.is_err() {
            let renamed = !part.exists();
            drop(fs::remove_file(&part));
            if renamed {
                drop(fs::remove_file(&target));
            }
        }
        Ok(placed?)
    }

    /// Renames the finished `part` file to `target` and flushes the directory entry.
    fn settle(&self, part: &Path, target: &Path) -> io::Result<()> {
        fs::rename(part, target)?;
        File::open(&*self.dir)?.sync_all()
    }

    /// Starts receiving a new attachment into a `.part` file under a fresh id.
    pub(crate) fn receive(&self) -> DbResult<Incoming> {
        let id = Uuid::new_v4().to_string();
        let part = self.dir.join(format!("{id}{PART_SUFFIX}"));
        let file = tokio::fs::File::from_std(File::create(&part)?);
        Ok(Incoming {
            id,
            part,
            file,
            size: 0,
            hasher: Sha256::new(),
        })
    }

    /// Moves the finished `upload` to its final name.
    pub(crate) fn place_incoming(&self, upload: &Incoming) -> DbResult<()> {
        let target = self.path_of(&upload.id)?;
        let placed = self.settle(&upload.part, &target);
        if placed.is_err() {
            drop(fs::remove_file(&target));
        }
        Ok(placed?)
    }

    /// Opens attachment `id` for reading and returns it with its length, or [`None`] when no file
    /// holds it.
    pub(crate) fn open_file(&self, id: &str) -> DbResult<Option<(File, u64)>> {
        match File::open(self.path_of(id)?) {
            Ok(file) => {
                let len = file.metadata()?.len();
                Ok(Some((file, len)))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Returns the bytes of attachment `id`, or [`None`] when no file holds them.
    pub(crate) fn read(&self, id: &str) -> DbResult<Option<Vec<u8>>> {
        let mut bytes = Vec::new();
        match File::open(self.path_of(id)?) {
            Ok(mut file) => file.read_to_end(&mut bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(bytes))
    }

    /// Returns whether a file holds attachment `id`.
    pub(crate) fn exists(&self, id: &str) -> DbResult<bool> {
        Ok(self.path_of(id)?.try_exists()?)
    }

    /// Deletes the files of `ids` after the transaction that dropped their rows committed, once it
    /// has retried every file an earlier call could not delete. A file that fails is tried again
    /// at the next call and at the next [`Self::bytes`].
    pub(crate) fn remove(&self, ids: &[String]) {
        self.retry_stuck();
        for id in ids {
            if let Err(error) = self.delete(id) {
                eprintln!("pomb: cannot delete attachment file {id}: {error}");
                self.stuck().insert(id.clone());
            }
        }
    }

    /// Deletes the file of `id`, and succeeds when none exists.
    fn delete(&self, id: &str) -> io::Result<()> {
        let Ok(path) = self.path_of(id) else {
            return Ok(());
        };
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Deletes again every file an earlier delete could not.
    fn retry_stuck(&self) {
        let ids: Vec<String> = self.stuck().iter().cloned().collect();
        for id in ids {
            if self.delete(&id).is_ok() {
                self.stuck().remove(&id);
            }
        }
    }

    /// Locks the ids of the files whose delete failed.
    fn stuck(&self) -> MutexGuard<'_, HashSet<String>> {
        self.stuck.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the bytes the files in the directory hold, after retrying every delete that failed.
    pub(crate) fn bytes(&self) -> DbResult<u64> {
        self.retry_stuck();
        let mut bytes = 0;
        for entry in fs::read_dir(&*self.dir)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().ends_with(PART_SUFFIX) {
                continue;
            }
            match entry.metadata() {
                Ok(meta) => bytes += meta.len(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(bytes)
    }

    /// Deletes every leftover `.part` file and returns how many it deleted.
    pub(crate) fn sweep_parts(&self) -> DbResult<usize> {
        let mut swept = 0;
        for entry in fs::read_dir(&*self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.file_name().to_string_lossy().ends_with(PART_SUFFIX)
            {
                fs::remove_file(entry.path())?;
                swept += 1;
            }
        }
        Ok(swept)
    }

    /// Returns the complete files named with a generated id that are not in `named`, with their
    /// sizes, in id order, leaving out a file an upload is still placing.
    pub(crate) fn unnamed(&self, named: &HashSet<String>) -> DbResult<Vec<(String, u64)>> {
        self.scan_unnamed(named, &self.held())
    }

    /// Lists the unnamed files that are not in `held`.
    fn scan_unnamed(
        &self,
        named: &HashSet<String>,
        held: &HashSet<String>,
    ) -> DbResult<Vec<(String, u64)>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&*self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type()?.is_file()
                || name.ends_with(PART_SUFFIX)
                || named.contains(&name)
                || held.contains(&name)
                || self.path_of(&name).is_err()
            {
                continue;
            }
            match entry.metadata() {
                Ok(meta) => files.push((name, meta.len())),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        files.sort_unstable();
        Ok(files)
    }

    /// Deletes the complete files that are not in `named` and returns how many it deleted with
    /// their bytes.
    pub(crate) fn remove_unnamed(&self, named: &HashSet<String>) -> DbResult<UnnamedFiles> {
        let held = self.held();
        let mut removed = UnnamedFiles::default();
        for (id, size) in self.scan_unnamed(named, &held)? {
            self.delete(&id)?;
            removed.count += 1;
            removed.bytes += size;
        }
        Ok(removed)
    }

    /// Marks the file of `id` as being placed until the returned claim is dropped.
    pub(crate) fn hold(&self, id: &str) -> Held {
        self.held().insert(id.to_string());
        Held {
            id: id.to_string(),
            held: Arc::clone(&self.held),
        }
    }

    /// Locks the ids of the files that are being placed.
    fn held(&self) -> MutexGuard<'_, HashSet<String>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the ids in `named` that no file holds, in id order.
    pub(crate) fn missing(&self, named: &HashSet<String>) -> DbResult<Vec<String>> {
        let mut ids = Vec::new();
        for id in named {
            if !self.exists(id)? {
                ids.push(id.clone());
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Returns the directory that holds the files.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Copies every complete file into the directory `dest`, creating it, flushes each copy and
    /// the directory, and returns how many bytes it copied.
    pub(crate) fn copy_to(&self, dest: &Path) -> io::Result<u64> {
        fs::create_dir_all(dest)?;
        let mut copied = 0;
        for entry in fs::read_dir(&*self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.file_name().to_string_lossy().ends_with(PART_SUFFIX)
            {
                continue;
            }
            let target = dest.join(entry.file_name());
            match fs::copy(entry.path(), &target) {
                Ok(bytes) => copied += bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            File::open(&target)?.sync_all()?;
        }
        File::open(dest)?.sync_all()?;
        Ok(copied)
    }

    /// Copies the attachment files of the database at `from` to the store of the database at `to`.
    #[cfg(test)]
    pub fn copy_beside(from: &Path, to: &Path) {
        let (from, to) = (Self::beside(from), Self::open(to).expect("open"));
        for entry in fs::read_dir(&*from.dir).expect("dir") {
            let path = entry.expect("entry").path();
            fs::copy(&path, to.dir.join(path.file_name().expect("name"))).expect("copy");
        }
    }

    /// Returns how many files the directory holds.
    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        fs::read_dir(&*self.dir).expect("dir").count()
    }
}

#[cfg(test)]
mod tests {
    use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

    use super::*;
    use crate::db::{Db, DuplicateAttachment, LEGACY_ATTACHMENT_DATA, OfflineDb, TASKS, rows_in};
    use crate::models::{Actor, TaskAttachment, TaskRow};

    fn store() -> (tempfile::TempDir, AttachmentFiles) {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = AttachmentFiles::open(&dir.path().join("pomb.redb")).expect("open");
        (dir, files)
    }

    #[test]
    fn write_should_store_the_bytes_and_leave_no_part_file() {
        let (_dir, files) = store();
        let id = Uuid::new_v4().to_string();
        files.write(&id, b"hello").expect("write");
        assert_eq!(files.read(&id).expect("read"), Some(b"hello".to_vec()));
        assert_eq!(files.count(), 1);
    }

    #[test]
    fn read_should_refuse_an_id_that_is_not_a_generated_one() {
        let (dir, files) = store();
        fs::write(dir.path().join("secret"), b"secret").expect("secret");
        for id in [
            "../secret",
            "..",
            "/etc/passwd",
            "a/b",
            "",
            "ABCDEF01-2345-6789-ABCD-EF0123456789",
            "abcdef0123456789abcdef0123456789",
        ] {
            assert!(files.read(id).is_err(), "{id:?}");
            assert!(files.write(id, b"x").is_err(), "{id:?}");
        }
        assert_eq!(files.count(), 0);
    }

    #[test]
    fn remove_should_not_touch_a_file_outside_the_directory() {
        let (dir, files) = store();
        let outside = dir.path().join("secret");
        fs::write(&outside, b"secret").expect("secret");
        files.remove(&["../secret".to_string()]);
        assert!(outside.exists());
    }

    fn fail_unlink(files: &AttachmentFiles, id: &str) -> PathBuf {
        let path = files.dir.join(id);
        fs::create_dir(&path).expect("blocker");
        files.remove(&[id.to_string()]);
        assert!(path.exists());
        path
    }

    fn clear_blocker(path: &Path, bytes: &[u8]) {
        fs::remove_dir(path).expect("unblock");
        fs::write(path, bytes).expect("file");
    }

    #[test]
    fn remove_should_retry_a_failed_unlink_on_the_next_remove() {
        let (_dir, files) = store();
        let stuck = Uuid::new_v4().to_string();
        let path = fail_unlink(&files, &stuck);
        clear_blocker(&path, b"left behind");

        let other = Uuid::new_v4().to_string();
        files.write(&other, b"x").expect("write");
        files.remove(&[other]);

        assert!(!path.exists());
        assert_eq!(files.count(), 0);
    }

    #[test]
    fn bytes_should_retry_a_failed_unlink_and_stop_counting_the_file() {
        let (_dir, files) = store();
        let stuck = Uuid::new_v4().to_string();
        let path = fail_unlink(&files, &stuck);
        clear_blocker(&path, b"left behind");

        assert_eq!(files.bytes().expect("bytes"), 0);
        assert!(!path.exists());
    }

    #[test]
    fn remove_should_keep_retrying_a_file_whose_unlink_keeps_failing() {
        let (_dir, files) = store();
        let stuck = Uuid::new_v4().to_string();
        let path = fail_unlink(&files, &stuck);
        files.remove(&[]);
        assert!(path.exists());
        clear_blocker(&path, b"left behind");

        assert_eq!(files.bytes().expect("bytes"), 0);
        assert!(!path.exists());
    }

    #[test]
    fn sweep_parts_should_delete_part_files_and_keep_every_complete_file() {
        let (_dir, files) = store();
        let kept = Uuid::new_v4().to_string();
        let unnamed = Uuid::new_v4().to_string();
        files.write(&kept, b"a").expect("write");
        files.write(&unnamed, b"b").expect("write");
        fs::write(files.dir.join(format!("{kept}{PART_SUFFIX}")), b"c").expect("part");
        assert_eq!(files.sweep_parts().expect("sweep"), 1);
        assert_eq!(files.read(&kept).expect("read"), Some(b"a".to_vec()));
        assert_eq!(files.read(&unnamed).expect("read"), Some(b"b".to_vec()));
        let named = HashSet::from([kept]);
        assert_eq!(files.unnamed(&named).expect("unnamed"), vec![(unnamed, 1)]);
    }

    #[test]
    fn remove_unnamed_should_delete_only_files_no_row_names() {
        let (_dir, files) = store();
        let kept = Uuid::new_v4().to_string();
        let first = Uuid::new_v4().to_string();
        let second = Uuid::new_v4().to_string();
        files.write(&kept, b"a").expect("write");
        files.write(&first, b"bb").expect("write");
        files.write(&second, b"ccc").expect("write");
        let named = HashSet::from([kept.clone()]);

        let removed = files.remove_unnamed(&named).expect("remove");

        assert_eq!(removed, UnnamedFiles { count: 2, bytes: 5 });
        assert_eq!(files.read(&kept).expect("read"), Some(b"a".to_vec()));
        assert_eq!(files.read(&first).expect("read"), None);
        assert_eq!(files.read(&second).expect("read"), None);
        assert_eq!(files.bytes().expect("bytes"), 1);
    }

    #[test]
    fn unnamed_should_leave_out_a_file_whose_name_is_not_a_generated_id() {
        let (_dir, files) = store();
        let orphan = Uuid::new_v4().to_string();
        files.write(&orphan, b"bb").expect("write");
        let stray = files.dir.join("stray.txt");
        fs::write(&stray, b"keep me").expect("stray");

        let listed = files.unnamed(&HashSet::new()).expect("unnamed");
        let removed = files.remove_unnamed(&HashSet::new()).expect("remove");

        assert_eq!(listed, vec![(orphan, 2)]);
        assert_eq!(removed, UnnamedFiles { count: 1, bytes: 2 });
        assert_eq!(fs::read(&stray).expect("stray"), b"keep me");
        assert_eq!(files.unnamed(&HashSet::new()).expect("unnamed"), Vec::new());
    }

    #[test]
    fn remove_unnamed_should_leave_a_file_an_upload_is_still_placing() {
        let (_dir, files) = store();
        let placing = Uuid::new_v4().to_string();
        files.write(&placing, b"abc").expect("write");
        let held = files.hold(&placing);

        let removed = files.remove_unnamed(&HashSet::new()).expect("remove");

        assert_eq!(removed, UnnamedFiles::default());
        assert_eq!(files.read(&placing).expect("read"), Some(b"abc".to_vec()));
        drop(held);
        let removed = files.remove_unnamed(&HashSet::new()).expect("remove");
        assert_eq!(removed, UnnamedFiles { count: 1, bytes: 3 });
    }

    #[test]
    fn missing_should_list_the_named_ids_that_no_file_holds() {
        let (_dir, files) = store();
        let present = Uuid::new_v4().to_string();
        let gone = Uuid::new_v4().to_string();
        files.write(&present, b"a").expect("write");
        let named = HashSet::from([present, gone.clone()]);
        assert_eq!(files.missing(&named).expect("missing"), vec![gone]);
    }

    #[test]
    fn copy_to_should_copy_complete_files_and_skip_part_files() {
        let (dir, files) = store();
        let id = Uuid::new_v4().to_string();
        files.write(&id, b"abc").expect("write");
        fs::write(files.dir.join(format!("{id}{PART_SUFFIX}")), b"half").expect("part");
        let dest = dir.path().join("copy");
        assert_eq!(files.copy_to(&dest).expect("copy"), 3);
        assert_eq!(fs::read(dest.join(&id)).expect("copied"), b"abc");
        assert_eq!(fs::read_dir(&dest).expect("dir").count(), 1);
    }
    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        db: Db,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pomb.redb");
        let db = Db::connect(path.to_str().expect("utf8")).expect("connect");
        Fixture {
            _dir: dir,
            path,
            db,
        }
    }

    fn task_of(db: &Db) -> (String, String) {
        let account = db.create_user("chip").expect("user");
        let board = db.create_board(&account, "Board").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let task = db
            .create_task(&column, None, "Task", 1, None)
            .expect("task");
        (board, task)
    }

    #[test]
    fn add_attachment_should_store_the_bytes_in_a_file_that_reads_back_identical() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let bytes: Vec<u8> = (0..=255).cycle().take(100_000).collect();
        let added = fx
            .db
            .add_attachment(
                &task,
                "a.bin",
                "application/octet-stream",
                &bytes,
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
        assert_eq!(
            fx.db.read_attachment_data(&added.id).expect("read"),
            Some(bytes.clone())
        );
        let on_disk = fx
            .path
            .with_file_name("pomb.redb.attachments")
            .join(&added.id);
        assert_eq!(fs::read(on_disk).expect("file"), bytes);
    }

    #[test]
    fn add_attachment_should_leave_no_file_when_the_task_is_unknown() {
        let fx = fixture();
        let unknown = fx
            .db
            .add_attachment("missing", "a.txt", "text/plain", b"x", Actor::SYSTEM)
            .expect("attach");
        assert!(unknown.is_none());
        assert_eq!(fx.db.attachment_data_count(), 0);
    }

    #[test]
    fn removing_unnamed_files_should_free_their_bytes_and_stop_the_startup_report() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let kept = fx
            .db
            .add_attachment(&task, "a.txt", "text/plain", b"abc", Actor::SYSTEM)
            .expect("attach")
            .expect("task");
        let orphan = Uuid::new_v4().to_string();
        fx.db.files.write(&orphan, b"orphaned").expect("write");
        let before = fx.db.stored_bytes().expect("bytes");
        assert_eq!(
            fx.db.unnamed_attachment_files().expect("unnamed"),
            UnnamedFiles { count: 1, bytes: 8 }
        );

        let removed = fx.db.remove_unnamed_attachment_files().expect("remove");

        assert_eq!(removed, UnnamedFiles { count: 1, bytes: 8 });
        assert_eq!(fx.db.stored_bytes().expect("bytes"), before - 8);
        assert_eq!(fx.db.read_attachment_data(&orphan).expect("read"), None);
        assert_eq!(
            fx.db.read_attachment_data(&kept.id).expect("read"),
            Some(b"abc".to_vec())
        );
        let path = fx.path.to_str().expect("utf8").to_string();
        let Fixture { _dir, db, .. } = fx;
        drop(db);
        let reopened = Db::connect(&path).expect("reopen");
        assert_eq!(
            reopened.unnamed_attachment_files().expect("unnamed"),
            UnnamedFiles::default()
        );
    }

    #[test]
    fn detach_should_remove_the_file_after_its_commit() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let added = fx
            .db
            .add_attachment(&task, "a.txt", "text/plain", b"abc", Actor::SYSTEM)
            .expect("attach")
            .expect("task");
        assert_eq!(fx.db.attachment_data_count(), 1);
        assert!(
            fx.db
                .delete_attachment(&task, &added.id, Actor::SYSTEM)
                .expect("detach")
        );
        assert_eq!(fx.db.attachment_data_count(), 0);
        assert_eq!(fx.db.read_attachment_data(&added.id).expect("read"), None);
    }

    #[test]
    fn reopening_after_a_crash_between_rename_and_commit_should_keep_the_file_and_drop_the_part() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let kept = fx
            .db
            .add_attachment(&task, "a.txt", "text/plain", b"abc", Actor::SYSTEM)
            .expect("attach")
            .expect("task");
        let crashed = Uuid::new_v4().to_string();
        fx.db
            .files
            .write(&crashed, b"renamed, never committed")
            .expect("write");
        let part = Uuid::new_v4().to_string();
        let dir = fx.path.with_file_name("pomb.redb.attachments");
        fs::write(dir.join(format!("{part}{PART_SUFFIX}")), b"half").expect("part");
        assert_eq!(fx.db.attachment_data_count(), 3);

        let path = fx.path.to_str().expect("utf8").to_string();
        let Fixture { _dir, db, .. } = fx;
        drop(db);
        let db = Db::connect(&path).expect("reopen");

        assert_eq!(
            db.read_attachment_data(&crashed).expect("read"),
            Some(b"renamed, never committed".to_vec())
        );
        assert_eq!(db.attachment_data_count(), 2);
        assert_eq!(
            db.read_attachment_data(&kept.id).expect("read"),
            Some(b"abc".to_vec())
        );
        let txn = db.db.begin_read().expect("read");
        let rows: Vec<TaskRow> = rows_in(&txn.open_table(TASKS).expect("table")).expect("rows");
        assert!(
            rows.iter()
                .all(|row| row.attachments.iter().all(|a| a.id != crashed))
        );
    }

    #[test]
    fn a_stored_id_that_climbs_out_of_the_directory_should_not_read_or_delete_anything() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let outside = fx.path.with_file_name("secret");
        fs::write(&outside, b"secret").expect("secret");
        let txn = fx.db.db.begin_write().expect("write");
        {
            let mut tasks = txn.open_table(TASKS).expect("table");
            let raw = tasks
                .get(task.as_str())
                .expect("get")
                .expect("task")
                .value()
                .to_vec();
            let mut row: TaskRow = serde_json::from_slice(&raw).expect("row");
            row.attachments.push(TaskAttachment {
                id: "../secret".to_string(),
                name: "a.txt".to_string(),
                mime: "text/plain".to_string(),
                size: 6,
                uploaded_by: "chip".to_string(),
                token_name: None,
                role_id: None,
                role_name: None,
                label: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                sha256: None,
                url: None,
                edited_at: None,
                versions: Vec::new(),
            });
            let raw = serde_json::to_vec(&row).expect("json");
            tasks.insert(task.as_str(), raw.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");

        assert!(fx.db.read_attachment_data("../secret").is_err());
        assert!(
            fx.db
                .delete_attachment(&task, "../secret", Actor::SYSTEM)
                .expect("detach")
        );
        assert_eq!(fs::read(&outside).expect("secret"), b"secret");
    }

    #[test]
    fn add_attachment_should_match_a_row_with_no_hash_by_name_alone() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let added = fx
            .db
            .add_attachment(&task, "a.txt", "text/plain", b"abc", Actor::SYSTEM)
            .expect("attach")
            .expect("task");
        let txn = fx.db.db.begin_write().expect("write");
        {
            let mut tasks = txn.open_table(TASKS).expect("table");
            let raw = tasks
                .get(task.as_str())
                .expect("get")
                .expect("task")
                .value()
                .to_vec();
            let mut row: TaskRow = serde_json::from_slice(&raw).expect("row");
            row.attachments[0].sha256 = None;
            let raw = serde_json::to_vec(&row).expect("json");
            tasks.insert(task.as_str(), raw.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");

        let renamed = fx
            .db
            .add_attachment(&task, "copy.txt", "text/plain", b"abc", Actor::SYSTEM)
            .expect("attach")
            .expect("task");
        assert_ne!(renamed.id, added.id);

        let same_name = fx
            .db
            .add_attachment(&task, "a.txt", "text/plain", b"other", Actor::SYSTEM)
            .expect_err("duplicate");
        let duplicate = same_name
            .downcast_ref::<DuplicateAttachment>()
            .expect("duplicate refusal");
        assert_eq!(duplicate.existing_id, added.id);
    }

    #[test]
    fn usage_should_count_the_recorded_size_of_each_attachment() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let before = fx.db.stored_bytes_by_account().expect("usage");
        let added = fx
            .db
            .add_attachment(
                &task,
                "a.bin",
                "application/octet-stream",
                &[0; 5000],
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task");
        let after = fx.db.stored_bytes_by_account().expect("usage");
        let grown: u64 = after.values().sum::<u64>() - before.values().sum::<u64>();
        assert!(grown >= 5000);
        fx.db
            .delete_attachment(&task, &added.id, Actor::SYSTEM)
            .expect("detach");
        let freed = fx.db.stored_bytes_by_account().expect("usage");
        assert!(freed.values().sum::<u64>() < after.values().sum::<u64>() - 4999);
    }

    fn attach(fx: &Fixture, task: &str, bytes: &[u8]) -> String {
        let id = fx
            .db
            .add_attachment(
                task,
                &format!(
                    "{}.bin",
                    fx.db
                        .task(task)
                        .expect("read")
                        .expect("task")
                        .attachments
                        .len()
                ),
                "application/octet-stream",
                bytes,
                Actor::SYSTEM,
            )
            .expect("attach")
            .expect("task")
            .id;
        fs::remove_file(stored(fx.path.to_str().expect("utf8")).dir.join(&id)).expect("file");
        id
    }

    fn close(fx: Fixture) -> (tempfile::TempDir, String) {
        let Fixture { _dir, path, db } = fx;
        drop(db);
        (_dir, path.to_str().expect("utf8").to_string())
    }

    fn put_legacy_rows(path: &str, rows: &[(&str, &[u8])]) {
        let raw = redb::Database::create(path).expect("raw open");
        let txn = raw.begin_write().expect("write");
        {
            let mut table = txn.open_table(LEGACY_ATTACHMENT_DATA).expect("table");
            for (id, bytes) in rows {
                table.insert(*id, *bytes).expect("insert");
            }
        }
        txn.commit().expect("commit");
    }

    fn legacy_row_count(path: &str) -> Option<u64> {
        let raw = redb::ReadOnlyDatabase::open(path).expect("read-only open");
        let txn = raw.begin_read().expect("read");
        match txn.open_table(LEGACY_ATTACHMENT_DATA) {
            Ok(table) => Some(table.len().expect("len")),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(error) => panic!("{error}"),
        }
    }

    fn stored(path: &str) -> AttachmentFiles {
        AttachmentFiles::beside(Path::new(path))
    }

    #[test]
    fn open_should_move_every_row_of_the_old_table_to_a_byte_identical_file_and_drop_the_table() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let first: Vec<u8> = (0..=255).cycle().take(70_000).collect();
        let second = b"second".to_vec();
        let (a, b) = (attach(&fx, &task, &first), attach(&fx, &task, &second));
        let (_dir, path) = close(fx);
        put_legacy_rows(&path, &[(&a, &first), (&b, &second)]);

        let db = Db::connect(&path).expect("reopen");

        assert_eq!(db.read_attachment_data(&a).expect("read"), Some(first));
        assert_eq!(db.read_attachment_data(&b).expect("read"), Some(second));
        drop(db);
        assert_eq!(legacy_row_count(&path), None);
    }

    #[test]
    fn open_should_finish_a_move_that_crashed_after_placing_some_files() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let (a, b) = (attach(&fx, &task, b"one"), attach(&fx, &task, b"two"));
        let (_dir, path) = close(fx);
        put_legacy_rows(&path, &[(&a, b"one"), (&b, b"two")]);
        let files = stored(&path);
        files.write(&a, b"one").expect("placed before the crash");
        fs::write(files.dir.join(format!("{b}{PART_SUFFIX}")), b"t").expect("half written");

        let db = Db::connect(&path).expect("reopen");

        assert_eq!(
            db.read_attachment_data(&a).expect("read"),
            Some(b"one".to_vec())
        );
        assert_eq!(
            db.read_attachment_data(&b).expect("read"),
            Some(b"two".to_vec())
        );
        assert_eq!(db.files.count(), 2);
        drop(db);
        assert_eq!(legacy_row_count(&path), None);
    }

    #[test]
    fn startup_should_keep_a_moved_file_and_a_stray_one() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let a = attach(&fx, &task, b"kept");
        let (_dir, path) = close(fx);
        put_legacy_rows(&path, &[(&a, b"kept")]);
        let stray = Uuid::new_v4().to_string();
        stored(&path).write(&stray, b"stray").expect("stray");

        let db = Db::connect(&path).expect("reopen");

        assert_eq!(
            db.read_attachment_data(&a).expect("read"),
            Some(b"kept".to_vec())
        );
        assert_eq!(
            db.read_attachment_data(&stray).expect("read"),
            Some(b"stray".to_vec())
        );
    }

    #[test]
    fn open_should_keep_files_newer_than_a_restored_backup_and_report_missing_ones() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let named = attach(&fx, &task, b"named");
        let newer = Uuid::new_v4().to_string();
        fx.db
            .files
            .write(&newer, b"after the backup")
            .expect("newer");
        let (_dir, path) = close(fx);
        stored(&path).remove(std::slice::from_ref(&named));

        let db = Db::connect(&path).expect("reopen");

        assert_eq!(db.missing_attachment_files().expect("missing"), vec![named]);
        assert_eq!(
            db.read_attachment_data(&newer).expect("read"),
            Some(b"after the backup".to_vec())
        );
    }

    #[test]
    fn open_should_fail_on_a_refused_id_and_leave_the_row_in_place() {
        let fx = fixture();
        let (_, task) = task_of(&fx.db);
        let a = attach(&fx, &task, b"one");
        let (_dir, path) = close(fx);
        put_legacy_rows(&path, &[(&a, b"one"), ("../escape", b"bytes")]);

        let error = Db::connect(&path).expect_err("refused id");

        assert!(error.to_string().contains("../escape"), "{error}");
        assert_eq!(legacy_row_count(&path), Some(2));
    }

    #[test]
    fn open_should_drop_a_row_that_no_task_names_with_the_table() {
        let fx = fixture();
        let (_dir, path) = close(fx);
        let orphan = Uuid::new_v4().to_string();
        put_legacy_rows(&path, &[(&orphan, b"orphan")]);

        let db = Db::connect(&path).expect("reopen");

        assert_eq!(db.files.count(), 0);
        drop(db);
        assert_eq!(legacy_row_count(&path), None);
    }

    #[test]
    fn fresh_database_should_not_create_the_old_table() {
        let fx = fixture();
        let (_dir, path) = close(fx);
        assert_eq!(legacy_row_count(&path), None);
    }

    #[test]
    fn offline_open_should_refuse_a_file_that_still_holds_the_old_table() {
        let fx = fixture();
        let (_dir, path) = close(fx);
        put_legacy_rows(&path, &[]);
        let error = OfflineDb::open(&path).expect_err("old table");
        assert!(error.to_string().contains("start pomb"), "{error}");
        drop(Db::connect(&path).expect("open"));
        OfflineDb::open(&path).expect("moved");
    }
}
