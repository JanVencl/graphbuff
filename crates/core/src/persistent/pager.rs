//! Page cache and redo log for persistent storage.
//!
//! Every data file is an array of [`PAGE_SIZE`] pages. The [`Pager`] keeps a bounded
//! set of them in memory and evicts the least recently used clean-or-committed page
//! when it needs room.
//!
//! ## Transactions
//! While a transaction is open, the first write to a page saves its before-image and
//! pins the page, so uncommitted changes never reach a data file ("no steal"). Rollback
//! copies the before-images back. Commit appends the after-image of every changed page
//! to the redo log in one checksummed record, after which the pages are unpinned and may
//! be written to their data files at any time. A transaction larger than the cache
//! temporarily grows the cache beyond its capacity, up to its size cap
//! ([`Pager::max_tx_pages`]): changing one page more fails with
//! [`Error::TransactionTooLarge`], and the caller rolls back.
//!
//! A dirty page is never written to its data file before the log record that produced it
//! is on stable storage, so recovery (re-applying every logged page image since the last
//! checkpoint) also repairs data pages that were torn by a crash.
//!
//! ## Versions
//! Every commit makes a new version. A snapshot reader registers the version it reads
//! ([`Pager::register`]) and sets [`Pager::read_at`] while it holds the pager. It then
//! reads a page as of its version: the before-image of a page the open transaction
//! changed, or the image saved in `history` when a later commit changed the page. Commit
//! moves its before-images into `history` while any snapshot is registered; they are
//! dropped once no registered snapshot is old enough to need them.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::codec::crc32;
use crate::{Error, Result};

pub(crate) const PAGE_SIZE: usize = 8192;
/// The most pages one log record can hold: its length is a `u32`, and each page takes
/// its image plus a 9-byte key. 1 MiB is left for the record's metadata header.
pub(crate) const MAX_LOGGED_PAGES: usize = ((u32::MAX as usize) - (1 << 20)) / (PAGE_SIZE + 9);
pub(crate) const WAL_MAGIC: &[u8; 8] = b"GBPWALv9";
/// Log written by format version 8 (metadata without vector indexes).
pub(crate) const WAL_MAGIC_V8: &[u8; 8] = b"GBPWALv8";
/// Log written by format version 7 (no point property values).
pub(crate) const WAL_MAGIC_V7: &[u8; 8] = b"GBPWALv7";
/// Log written by format version 6 (no temporal property values).
pub(crate) const WAL_MAGIC_V6: &[u8; 8] = b"GBPWALv6";
/// Log written by format version 5 (metadata with single-property node indexes only).
pub(crate) const WAL_MAGIC_V5: &[u8; 8] = b"GBPWALv5";
/// Log written by format version 4 (metadata without constraints).
pub(crate) const WAL_MAGIC_V4: &[u8; 8] = b"GBPWALv4";
/// Log written by format version 3 (metadata without index counts).
pub(crate) const WAL_MAGIC_V3: &[u8; 8] = b"GBPWALv3";
/// Log written by format version 2 (metadata without the index key format).
pub(crate) const WAL_MAGIC_V2: &[u8; 8] = b"GBPWALv2";
/// Log written by format version 1 (metadata without the group allocator). It is still
/// replayed on open, then replaced by a version 2 log.
pub(crate) const WAL_MAGIC_V1: &[u8; 8] = b"GBPWALv1";

/// A page of one data file.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PageKey {
    pub file: u8,
    pub page: u64,
}

type Page = Box<[u8]>;

struct Frame {
    data: Page,
    /// Differs from the data file.
    dirty: bool,
    /// Changed by the open transaction; pinned frames are not in the LRU list.
    pinned: bool,
    tick: u64,
}

pub(crate) struct DataFile {
    pub file: File,
    pub path: PathBuf,
    /// Written since the last fsync.
    pub unsynced: bool,
}

pub(crate) struct Pager {
    files: Vec<DataFile>,
    frames: HashMap<PageKey, Frame>,
    /// Unpinned frames by last use; the first entry is the next eviction victim.
    lru: BTreeMap<u64, PageKey>,
    tick: u64,
    capacity: usize,
    /// The most pages one transaction may change (at most [`MAX_LOGGED_PAGES`]).
    pub max_tx_pages: usize,
    /// Before-images (and dirty flags) of the pages the open transaction changed.
    undo: Option<HashMap<PageKey, (Page, bool)>>,
    /// The version reads are made at, while a snapshot reader holds the pager; `None`
    /// reads the current state (including the open transaction's changes).
    pub read_at: Option<u64>,
    /// Commits so far (in this process).
    version: u64,
    /// Older images of pages: `(until, image)` is the page as of every version below
    /// `until`, back to the previous entry's `until`. Entries are in `until` order.
    history: HashMap<PageKey, Vec<(u64, Page)>>,
    /// Registered snapshot versions, with the number of snapshots at each.
    snapshots: BTreeMap<u64, usize>,
    wal: File,
    wal_path: PathBuf,
    wal_len: u64,
    /// The log holds records that were not fsynced yet (`SyncPolicy::OsBuffered`).
    wal_unsynced: bool,
    /// Set when a failed log write could not be undone; the store then refuses writes.
    pub poisoned: Option<String>,
}

/// The error for a transaction that wants to change more than `pages` pages.
fn too_large(pages: usize) -> Error {
    let mib = pages as f64 * PAGE_SIZE as f64 / (1 << 20) as f64;
    Error::TransactionTooLarge(format!(
        "the transaction would change more than {pages} pages ({mib:.1} MiB) of persistent \
         storage, its size cap; split the work into smaller transactions, or raise the \
         database's maxTransactionBytes"
    ))
}

fn io(path: &std::path::Path, what: &str) -> impl FnOnce(std::io::Error) -> Error {
    let ctx = format!("{what} {}", path.display());
    move |e| Error::io(ctx, e)
}

impl Pager {
    pub fn new(
        files: Vec<DataFile>,
        wal: File,
        wal_path: PathBuf,
        wal_len: u64,
        cache_bytes: u64,
        max_tx_pages: usize,
    ) -> Pager {
        Pager {
            files,
            frames: HashMap::new(),
            lru: BTreeMap::new(),
            tick: 0,
            capacity: ((cache_bytes / PAGE_SIZE as u64) as usize).max(16),
            max_tx_pages: max_tx_pages.clamp(1, MAX_LOGGED_PAGES),
            undo: None,
            read_at: None,
            version: 0,
            history: HashMap::new(),
            snapshots: BTreeMap::new(),
            wal,
            wal_path,
            wal_len,
            wal_unsynced: false,
            poisoned: None,
        }
    }

    pub fn wal_len(&self) -> u64 {
        self.wal_len
    }

    pub fn cached_pages(&self) -> usize {
        self.frames.len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    // ---- disk -----------------------------------------------------------------------

    fn read_disk(&mut self, key: PageKey) -> Result<Page> {
        let df = &mut self.files[key.file as usize];
        let mut buf = vec![0u8; PAGE_SIZE].into_boxed_slice();
        df.file
            .seek(SeekFrom::Start(key.page * PAGE_SIZE as u64))
            .map_err(io(&df.path, "seeking in"))?;
        let mut filled = 0;
        while filled < PAGE_SIZE {
            match df.file.read(&mut buf[filled..]) {
                Ok(0) => break, // past the end of the file: the rest reads as zeros
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io(&df.path, "reading")(e)),
            }
        }
        Ok(buf)
    }

    /// Writes a page straight to its data file. The log must already hold it.
    pub fn write_disk(&mut self, key: PageKey, data: &[u8]) -> Result<()> {
        if self.wal_unsynced {
            self.wal
                .sync_data()
                .map_err(io(&self.wal_path, "syncing"))?;
            self.wal_unsynced = false;
        }
        let df = &mut self.files[key.file as usize];
        df.file
            .seek(SeekFrom::Start(key.page * PAGE_SIZE as u64))
            .and_then(|_| df.file.write_all(data))
            .map_err(io(&df.path, "writing"))?;
        df.unsynced = true;
        Ok(())
    }

    pub fn sync_files(&mut self) -> Result<()> {
        for df in &mut self.files {
            if df.unsynced {
                df.file.sync_all().map_err(io(&df.path, "syncing"))?;
                df.unsynced = false;
            }
        }
        Ok(())
    }

    // ---- cache ----------------------------------------------------------------------

    /// Evicts least recently used unpinned frames until at most `limit` remain.
    fn evict_to(&mut self, limit: usize) -> Result<()> {
        while self.frames.len() > limit {
            // Only unpinned frames are in the LRU list; with none left, the cache grows.
            let Some((&tick, &victim)) = self.lru.iter().next() else {
                break;
            };
            let frame = &self.frames[&victim];
            if frame.dirty {
                let data = frame.data.clone();
                self.write_disk(victim, &data)?;
            }
            self.lru.remove(&tick);
            self.frames.remove(&victim);
        }
        Ok(())
    }

    fn load(&mut self, key: PageKey) -> Result<()> {
        if !self.frames.contains_key(&key) {
            self.evict_to(self.capacity - 1)?;
            let data = self.read_disk(key)?;
            self.frames.insert(
                key,
                Frame {
                    data,
                    dirty: false,
                    pinned: false,
                    tick: 0,
                },
            );
        }
        let f = self.frames.get_mut(&key).expect("loaded above");
        // A page used among the last eighth of the cache's accesses is recent enough:
        // moving it in the LRU list again would cost more than the order is worth.
        let recent = f.tick != 0 && self.tick - f.tick < (self.capacity / 8) as u64;
        if !f.pinned && !recent {
            self.tick += 1;
            if f.tick != 0 {
                self.lru.remove(&f.tick);
            }
            f.tick = self.tick;
            self.lru.insert(f.tick, key);
        }
        Ok(())
    }

    /// Runs `f` on the contents of a page: as of [`read_at`](Self::read_at) if set,
    /// else the current contents.
    pub fn read<R>(&mut self, key: PageKey, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        if let Some(at) = self.read_at {
            let old = self
                .history
                .get(&key)
                .and_then(|h| h.iter().find(|(until, _)| *until > at))
                .map(|(_, img)| img)
                .or_else(|| self.undo.as_ref()?.get(&key).map(|(img, _)| img));
            if let Some(img) = old {
                return Ok(f(img));
            }
        }
        self.load(key)?;
        Ok(f(&self.frames[&key].data))
    }

    /// Pages in data file `file`, counting pages so far only in the cache or history.
    pub fn file_pages(&self, file: u8) -> Result<u64> {
        let df = &self.files[file as usize];
        let len = df.file.metadata().map_err(io(&df.path, "reading the size of"))?.len();
        let cached = self
            .frames
            .keys()
            .chain(self.history.keys())
            .filter(|k| k.file == file)
            .map(|k| k.page + 1)
            .max()
            .unwrap_or(0);
        Ok(len.div_ceil(PAGE_SIZE as u64).max(cached))
    }

    /// Copies page `key` as of version `at` (a registered snapshot) into `out`, without
    /// bringing it into the cache, so a backup does not push out the working set.
    pub fn copy_page_at(&mut self, key: PageKey, at: u64, out: &mut [u8]) -> Result<()> {
        let img = self
            .history
            .get(&key)
            .and_then(|h| h.iter().find(|(until, _)| *until > at))
            .map(|(_, img)| img)
            .or_else(|| self.undo.as_ref()?.get(&key).map(|(img, _)| img))
            .or_else(|| self.frames.get(&key).map(|f| &f.data));
        match img {
            Some(img) => out.copy_from_slice(img),
            None => out.copy_from_slice(&self.read_disk(key)?),
        }
        Ok(())
    }

    // ---- versions ---------------------------------------------------------------------

    /// Registers a snapshot of the last committed state; returns its version.
    pub fn register(&mut self) -> u64 {
        *self.snapshots.entry(self.version).or_default() += 1;
        self.version
    }

    /// Ends a snapshot taken with [`register`](Self::register), dropping the page images
    /// no remaining snapshot needs.
    pub fn unregister(&mut self, version: u64) {
        if let Some(n) = self.snapshots.get_mut(&version) {
            *n -= 1;
            if *n == 0 {
                self.snapshots.remove(&version);
            }
        }
        match self.snapshots.keys().next().copied() {
            None => self.history.clear(),
            // A snapshot at `oldest` needs the entries with `until > oldest`.
            Some(oldest) => self.history.retain(|_, h| {
                h.retain(|(until, _)| *until > oldest);
                !h.is_empty()
            }),
        }
    }

    /// Page images kept for snapshots.
    #[cfg(test)]
    pub fn history_pages(&self) -> usize {
        self.history.values().map(Vec::len).sum()
    }

    /// The page, for writing. Inside a transaction the page is pinned and its
    /// before-image saved on first write; a page beyond the transaction's size cap fails
    /// with [`Error::TransactionTooLarge`] and changes nothing.
    pub fn write(&mut self, key: PageKey) -> Result<&mut [u8]> {
        debug_assert!(self.read_at.is_none(), "a write while reading a snapshot");
        if let Some(undo) = &self.undo
            && undo.len() >= self.max_tx_pages
            && !undo.contains_key(&key)
        {
            return Err(too_large(self.max_tx_pages));
        }
        self.load(key)?;
        let f = self.frames.get_mut(&key).expect("loaded above");
        if let Some(undo) = &mut self.undo
            && !f.pinned
        {
            undo.insert(key, (f.data.clone(), f.dirty));
            f.pinned = true;
            self.lru.remove(&f.tick);
        }
        f.dirty = true;
        Ok(&mut f.data)
    }

    fn unpin(&mut self, key: PageKey) {
        if let Some(f) = self.frames.get_mut(&key) {
            f.pinned = false;
            self.tick += 1;
            f.tick = self.tick;
            self.lru.insert(f.tick, key);
        }
    }

    // ---- transactions -----------------------------------------------------------------

    pub fn begin(&mut self) {
        self.undo = Some(HashMap::new());
    }

    pub fn rollback(&mut self) {
        let Some(undo) = self.undo.take() else { return };
        for (key, (img, dirty)) in undo {
            if let Some(f) = self.frames.get_mut(&key) {
                f.data = img;
                f.dirty = dirty;
            }
            self.unpin(key);
        }
        self.shrink();
    }

    /// After a transaction larger than the cache, gives the extra memory back. A failed
    /// write-back keeps the page cached, so it is retried on a later eviction.
    fn shrink(&mut self) {
        let _ = self.evict_to(self.capacity);
    }

    /// Whether the open transaction changed any page.
    pub fn changed(&self) -> bool {
        self.undo.as_ref().is_some_and(|u| !u.is_empty())
    }

    /// Appends one log record — `header` (supplied by the store) followed by the
    /// after-image of every page the transaction changed — and ends the transaction.
    /// On error the pages are rolled back.
    pub fn commit(&mut self, header: &[u8], sync: bool) -> Result<()> {
        let mut keys: Vec<PageKey> = self
            .undo
            .as_ref()
            .map(|u| u.keys().copied().collect())
            .unwrap_or_default();
        keys.sort_unstable();

        let mut payload = Vec::with_capacity(header.len() + 8 + keys.len() * (9 + PAGE_SIZE));
        payload.extend_from_slice(header);
        payload.extend_from_slice(&(keys.len() as u64).to_le_bytes());
        for k in &keys {
            payload.push(k.file);
            payload.extend_from_slice(&k.page.to_le_bytes());
            payload.extend_from_slice(&self.frames[k].data);
        }
        let Ok(len) = u32::try_from(payload.len()) else {
            self.rollback();
            return Err(Error::TransactionTooLarge(
                "the transaction is too large for one log record; split the work into smaller transactions".into(),
            ));
        };
        let mut frame = Vec::with_capacity(8 + payload.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&crc32(&payload).to_le_bytes());
        frame.extend_from_slice(&payload);

        let start = self.wal_len;
        let written = self
            .wal
            .seek(SeekFrom::Start(start))
            .and_then(|_| self.wal.write_all(&frame))
            .and_then(|_| if sync { self.wal.sync_data() } else { Ok(()) });
        match written {
            Ok(()) => {
                self.wal_len += frame.len() as u64;
                self.wal_unsynced |= !sync;
                self.version += 1;
                let undo = self.undo.take().unwrap_or_default();
                if !self.snapshots.is_empty() {
                    // Registered snapshots still read the replaced images.
                    for (k, (img, _)) in undo {
                        self.history.entry(k).or_default().push((self.version, img));
                    }
                }
                for k in keys {
                    self.unpin(k);
                }
                self.shrink();
                Ok(())
            }
            Err(err) => {
                let undone = self.wal.set_len(start).and_then(|_| self.wal.sync_data());
                if let Err(undo_err) = undone {
                    self.poisoned = Some(format!("{err}; then {undo_err}"));
                }
                self.rollback();
                Err(Error::io("writing the redo log", err))
            }
        }
    }

    // ---- checkpoints ------------------------------------------------------------------

    /// Writes every dirty page to its data file and fsyncs the files. Must not be called
    /// while a transaction is open.
    pub fn flush_all(&mut self) -> Result<()> {
        let mut dirty: Vec<PageKey> = self
            .frames
            .iter()
            .filter(|(_, f)| f.dirty && !f.pinned)
            .map(|(k, _)| *k)
            .collect();
        dirty.sort_unstable();
        for k in dirty {
            let data = self.frames[&k].data.clone();
            self.write_disk(k, &data)?;
            self.frames.get_mut(&k).expect("listed above").dirty = false;
        }
        self.sync_files()
    }

    /// Empties the log down to a fresh header. A crash in between leaves an empty or
    /// header-only log, which opens as an empty log.
    pub fn reset_wal(&mut self) -> Result<()> {
        self.wal
            .set_len(0)
            .and_then(|_| self.wal.seek(SeekFrom::Start(0)))
            .and_then(|_| self.wal.write_all(WAL_MAGIC))
            .and_then(|_| self.wal.sync_all())
            .map_err(io(&self.wal_path, "resetting"))?;
        self.wal_len = WAL_MAGIC.len() as u64;
        self.wal_unsynced = false;
        Ok(())
    }

    pub fn set_wal_len(&mut self, len: u64) {
        self.wal_len = len;
    }

    /// Makes buffered log records durable (used on close with `SyncPolicy::OsBuffered`).
    pub fn sync_wal(&mut self) -> Result<()> {
        if self.wal_unsynced {
            self.wal
                .sync_data()
                .map_err(io(&self.wal_path, "syncing"))?;
            self.wal_unsynced = false;
        }
        Ok(())
    }
}
