use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use uuid::Uuid;

/// Prefix of the name of every spool directory.
pub const SPOOL_PREFIX: &str = "pomb-import-";

/// Age after which a spool directory that a crashed import left behind is removed, in seconds.
const STALE_SECS: u64 = 24 * 60 * 60;

/// A fresh directory of temporary files that an import fills while it reads an archive, removed
/// with everything in it when the value is dropped, on every exit path.
#[derive(Debug)]
pub struct Spool {
    /// The directory the files live in.
    dir: PathBuf,
}

impl Spool {
    /// Creates a spool directory inside `base`, readable by its owner only, and removes the spool
    /// directories an earlier crash left in `base`.
    ///
    /// # Errors
    ///
    /// Returns the [`io::Error`] of the directory that could not be created.
    pub fn create(base: &Path) -> io::Result<Self> {
        remove_stale(base);
        let dir = base.join(format!("{SPOOL_PREFIX}{}", Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;

            builder.mode(0o700);
        }
        builder.create(&dir)?;
        Ok(Self { dir })
    }

    /// Returns the path of the file `index` of the spool.
    #[must_use]
    pub fn file(&self, index: usize) -> PathBuf {
        self.dir.join(index.to_string())
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Removes the spool directories of `base` that are older than [`STALE_SECS`], ignoring any that
/// cannot be read or removed.
fn remove_stale(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let named = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(SPOOL_PREFIX));
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(STALE_SECS));
        if named && old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_should_remove_its_directory_when_dropped() {
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let dir = spool.dir.clone();
        fs::write(spool.file(0), b"bytes").expect("write");
        assert!(dir.is_dir());
        assert!(dir.starts_with(base.path()));
        drop(spool);
        assert!(!dir.exists());
    }

    #[test]
    fn spool_should_remove_its_directory_when_the_holder_panics() {
        let base = tempfile::tempdir().expect("tempdir");
        let path = base.path().to_path_buf();
        let outcome = std::panic::catch_unwind(move || {
            let spool = Spool::create(&path).expect("spool");
            fs::write(spool.file(0), b"bytes").expect("write");
            panic!("the import failed");
        });
        assert!(outcome.is_err());
        assert_eq!(fs::read_dir(base.path()).expect("dir").count(), 0);
    }

    #[test]
    fn create_should_keep_a_young_spool_of_another_import() {
        let base = tempfile::tempdir().expect("tempdir");
        let first = Spool::create(base.path()).expect("spool");
        let second = Spool::create(base.path()).expect("spool");
        assert!(first.dir.is_dir());
        assert!(second.dir.is_dir());
        assert_ne!(first.dir, second.dir);
    }
}
