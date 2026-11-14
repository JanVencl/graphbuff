//! Backups and consistency checks ([`GraphStore::backup`](crate::GraphStore::backup),
//! [`GraphStore::check`](crate::GraphStore::check)).
//!
//! A backup is a directory that opens as a store: a persistent store's backup holds its
//! data files and metadata, an in-memory or hybrid store's backup holds a hybrid
//! snapshot file. While a backup is written, the directory holds a [`BACKUP_MARKER`]
//! file; stores refuse to open a directory that still has it, so an interrupted backup
//! is never mistaken for a complete one.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Present in a backup directory until the backup is complete.
pub const BACKUP_MARKER: &str = "BACKUP_INCOMPLETE";

/// What a backup wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupInfo {
    /// The directory written.
    pub dir: PathBuf,
    /// The storage format of the backup: `persistent` or `hybrid`.
    pub format: &'static str,
    pub nodes: u64,
    pub relationships: u64,
    /// Bytes written.
    pub bytes: u64,
}

/// The findings of a consistency check. A store is consistent when no errors were
/// found; `warnings` name things that waste space but do not make answers wrong (such
/// as unreachable blocks).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub nodes: u64,
    pub relationships: u64,
    /// Index entries checked (label index and property indexes).
    pub index_entries: u64,
    /// The first [`CheckReport::MAX_LISTED`] errors.
    pub errors: Vec<String>,
    /// All errors found, including those not listed.
    pub error_count: u64,
    pub warnings: Vec<String>,
}

impl CheckReport {
    /// Errors (and warnings) kept in the lists; further errors are only counted.
    pub const MAX_LISTED: usize = 100;

    pub fn is_consistent(&self) -> bool {
        self.error_count == 0
    }

    pub(crate) fn error(&mut self, e: impl Into<String>) {
        self.error_count += 1;
        if self.errors.len() < Self::MAX_LISTED {
            self.errors.push(e.into());
        }
    }

    pub(crate) fn warning(&mut self, w: impl Into<String>) {
        if self.warnings.len() < Self::MAX_LISTED {
            self.warnings.push(w.into());
        }
    }
}

impl fmt::Display for CheckReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "checked {} nodes, {} relationships, {} index entries",
            self.nodes, self.relationships, self.index_entries
        )?;
        for e in &self.errors {
            writeln!(f, "error: {e}")?;
        }
        if self.error_count > self.errors.len() as u64 {
            writeln!(f, "... and {} more errors", self.error_count - self.errors.len() as u64)?;
        }
        for w in &self.warnings {
            writeln!(f, "warning: {w}")?;
        }
        if self.is_consistent() {
            write!(f, "consistent")
        } else {
            write!(f, "INCONSISTENT: {} errors", self.error_count)
        }
    }
}

fn io(path: &Path, what: &str) -> impl FnOnce(std::io::Error) -> Error {
    let ctx = format!("{what} {}", path.display());
    move |e| Error::io(ctx, e)
}

/// Creates the backup directory `dest`, which must not exist or be empty, and marks it
/// incomplete.
pub(crate) fn start_backup(dest: &Path) -> Result<()> {
    match fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::Runtime(format!(
                    "backup directory {} is not empty",
                    dest.display()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(dest).map_err(io(dest, "creating"))?;
        }
        Err(e) => return Err(io(dest, "reading")(e)),
    }
    let marker = dest.join(BACKUP_MARKER);
    fs::write(&marker, b"").map_err(io(&marker, "creating"))
}

/// Removes the incomplete marker: the backup in `dest` is complete. Every file must
/// already be synced.
pub(crate) fn finish_backup(dest: &Path) -> Result<()> {
    let marker = dest.join(BACKUP_MARKER);
    fs::remove_file(&marker).map_err(io(&marker, "removing"))?;
    crate::persistent::sync_dir(dest)
}

/// Fails if `dir` holds a backup that did not finish.
pub(crate) fn refuse_incomplete_backup(dir: &Path) -> Result<()> {
    if dir.join(BACKUP_MARKER).exists() {
        return Err(Error::Storage(format!(
            "{} holds a backup that did not finish; delete it and back up again",
            dir.display()
        )));
    }
    Ok(())
}

/// Writes `bytes` to a new file and syncs it.
pub(crate) fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = fs::File::create(path).map_err(io(path, "creating"))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(io(path, "writing"))
}
