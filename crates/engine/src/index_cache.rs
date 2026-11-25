//! Caches of index contents the engine keeps in memory (vector matrices, full-text
//! postings) for one committed state, kept up to date with later commits:
//! [`Database`](crate::Database) reports each commit's changes ([`IndexCache::published`]),
//! and the first query on a later state applies them instead of building the contents
//! again. Contents that take long to build are also kept in files, next to the store's.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use graphbuff_core::{Changes, GraphStore, IndexDef, Result, Snapshot};

/// The contents of an index of one committed state.
pub(crate) trait Cached: Clone + Send + Sync + Sized {
    /// The index the contents are of.
    fn def(&self) -> &IndexDef;

    /// The contents of index `def` in `store`.
    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Self>;

    /// Brings the contents to `store`, a later committed state, given (a superset of) the
    /// entities changed since, sorted and without repeats.
    fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()>;

    /// The file the contents of index `def` are kept in, if they are.
    fn file_name(_def: &IndexDef) -> Option<String> {
        None
    }

    /// The contents of index `def` in `store`, made from `file`; `None` if that cannot be
    /// used. `file` may be of any earlier or later state, or damaged.
    fn load(
        _file: &Path,
        _store: &dyn GraphStore,
        _def: &IndexDef,
        _interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Option<Self>> {
        Ok(None)
    }

    /// Writes the contents to `file`.
    fn save(&self, _file: &Path) -> io::Result<()> {
        Ok(())
    }

    /// Counts the changes made to the contents since they were loaded or built (none of
    /// the former, all of the latter).
    fn edits(&self) -> u64 {
        0
    }

    /// Whether `unsaved` changes are enough to write the file anew while queries run.
    fn save_due(&self, _unsaved: u64) -> bool {
        false
    }
}

/// Where the contents of indexes are kept, and for each index how many of its contents'
/// [`edits`](Cached::edits) its file has.
struct Files {
    dir: PathBuf,
    saved: Mutex<HashMap<String, u64>>,
}

impl Files {
    fn path<T: Cached>(&self, def: &IndexDef) -> Option<PathBuf> {
        T::file_name(def).map(|f| self.dir.join(f))
    }

    /// Writes the contents `e`, if they have changes their file has not and `due` finds
    /// them enough. Failing only leaves the file as it was: the contents can be built.
    fn save<T: Cached>(&self, e: &T, due: impl FnOnce(u64) -> bool) {
        let Some(path) = self.path::<T>(e.def()) else { return };
        let edits = e.edits();
        {
            let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
            let before = saved.get(&e.def().name).copied().unwrap_or(0);
            if edits <= before || !due(edits - before) {
                return;
            }
            // Noted before it is written, so that no other query writes it too, and one
            // that fails is not tried again by every query.
            saved.insert(e.def().name.clone(), edits);
        }
        let _ = e.save(&path);
    }

    /// Forgets what is saved of index `name`.
    fn forget(&self, name: &str) {
        self.saved.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
    }
}

type WeakSnapshot = Weak<dyn GraphStore + Send + Sync>;

fn is(w: &WeakSnapshot, snapshot: &Snapshot) -> bool {
    std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(snapshot))
}

/// The contents of the indexes of kind `T` in one committed state.
pub(crate) struct IndexCache<T: Cached> {
    state: Mutex<CacheState<T>>,
    files: Option<Files>,
}

impl<T: Cached> Default for IndexCache<T> {
    fn default() -> Self {
        Self::in_dir(None)
    }
}

impl<T: Cached> IndexCache<T> {
    /// A cache that keeps contents in files in `dir`, if there is one.
    pub fn in_dir(dir: Option<&Path>) -> Self {
        IndexCache {
            files: dir.map(|d| Files { dir: d.to_path_buf(), saved: Mutex::default() }),
            state: Mutex::new(CacheState {
                latest: None,
                base: None,
                entries: HashMap::new(),
                log: Vec::new(),
                logged: 0,
            }),
        }
    }
}

struct CacheState<T> {
    /// The last committed state published.
    latest: Option<WeakSnapshot>,
    /// The state the entries are of.
    base: Option<WeakSnapshot>,
    entries: HashMap<String, Arc<T>>,
    /// The states published after `base`, oldest first, with what each commit changed.
    log: Vec<(WeakSnapshot, Changes)>,
    /// The ids in `log`.
    logged: usize,
}

impl<T: Cached> CacheState<T> {
    /// Forgets the entries (and the log, of no use without them).
    fn clear(&mut self) {
        self.base = None;
        self.entries.clear();
        self.log.clear();
        self.logged = 0;
    }

    /// Makes the entries those of `snapshot`, entry `at` of the log, applying the
    /// changes of the commits up to it.
    fn advance(&mut self, snapshot: &Snapshot, at: usize, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let mut changes = Changes::default();
        for (_, c) in self.log.drain(..=at) {
            changes.extend(c);
        }
        self.logged = self.log.iter().map(|(_, c)| c.len()).sum();
        self.base = Some(Arc::downgrade(snapshot));
        if changes.unknown {
            self.entries.clear();
            return Ok(());
        }
        changes.normalize();
        let names: Vec<String> = self.entries.keys().cloned().collect();
        for name in names {
            let e = self.entries.remove(&name).expect("listed above");
            // In place, unless a query still reads it.
            let mut e = Arc::try_unwrap(e).unwrap_or_else(|e| (*e).clone());
            e.apply(&**snapshot, &changes, interrupt)?;
            self.entries.insert(name, Arc::new(e));
        }
        Ok(())
    }
}

impl<T: Cached> IndexCache<T> {
    /// Writes the contents that have changes their files have not.
    pub fn save(&self) {
        let Some(files) = &self.files else { return };
        let entries: Vec<Arc<T>> = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.entries.values().cloned().collect()
        };
        for e in entries {
            files.save(&*e, |_| true);
        }
    }

    /// Removes the files of no index of `defs`: of indexes that were dropped, and ones a
    /// process that stopped while it wrote them left behind.
    pub fn remove_stale_files(&self, defs: &[IndexDef], is_file: impl Fn(&str) -> bool) {
        let Some(files) = &self.files else { return };
        let Ok(entries) = std::fs::read_dir(&files.dir) else { return };
        let keep: Vec<String> = defs.iter().filter_map(T::file_name).collect();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && is_file(name)
                && !keep.iter().any(|k| k == name)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Records a commit: `snapshot` is the new committed state, `changes` what the store
    /// says changed since the state published before it.
    pub fn published(&self, snapshot: &Snapshot, changes: Changes) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.latest = Some(Arc::downgrade(snapshot));
        if !st.entries.is_empty() {
            // Drop the entries of indexes that are gone.
            let defs = snapshot.indexes();
            st.entries.retain(|name, e| {
                let kept = defs.contains(e.def());
                if !kept && let Some(files) = &self.files {
                    files.forget(name);
                    if let Some(path) = files.path::<T>(e.def()) {
                        let _ = std::fs::remove_file(path);
                    }
                }
                kept
            });
        }
        if st.entries.is_empty() || changes.unknown || st.logged + changes.len() > Changes::LIMIT {
            // Nothing to keep up to date, or more changes than are worth applying.
            st.clear();
            return;
        }
        st.logged += changes.len();
        st.log.push((Arc::downgrade(snapshot), changes));
    }

    /// The contents of index `def` in `snapshot`.
    pub fn get(&self, snapshot: &Snapshot, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Arc<T>> {
        // The cache outlives the query that fills it: not the query's memory.
        graphbuff_core::accounting::exempt(|| {
            let e = self.fill(snapshot, def, interrupt)?;
            if let Some(files) = &self.files {
                files.save(&*e, |unsaved| e.save_due(unsaved));
            }
            Ok(e)
        })
    }

    /// The contents of index `def` in `store`, from its file if it has one.
    fn make(&self, store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<T> {
        if let Some(path) = self.files.as_ref().and_then(|f| f.path::<T>(def))
            && let Some(e) = T::load(&path, store, def, interrupt)?
        {
            return Ok(e);
        }
        T::build(store, def, interrupt)
    }

    fn fill(&self, snapshot: &Snapshot, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Arc<T>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.base.as_ref().is_some_and(|b| is(b, snapshot)) {
            if let Some(at) = st.log.iter().position(|(w, _)| is(w, snapshot)) {
                if let Err(e) = st.advance(snapshot, at, interrupt) {
                    st.clear();
                    return Err(e);
                }
            } else if st.latest.as_ref().is_some_and(|l| is(l, snapshot)) {
                st.clear();
                st.base = Some(Arc::downgrade(snapshot));
            } else {
                // An earlier state, read by a query that began before the last commits.
                drop(st);
                return Ok(Arc::new(self.make(&**snapshot, def, interrupt)?));
            }
        }
        if let Some(e) = st.entries.get(&def.name)
            && e.def() == def
        {
            return Ok(e.clone());
        }
        if let Some(files) = &self.files {
            files.forget(&def.name);
        }
        let e = Arc::new(self.make(&**snapshot, def, interrupt)?);
        st.entries.insert(def.name.clone(), e.clone());
        Ok(e)
    }

    /// The number of indexes whose contents are kept.
    pub fn len(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).entries.len()
    }

    /// The entry of index `name`, if one is kept: for tests.
    #[cfg(test)]
    pub fn entry(&self, name: &str) -> Option<Arc<T>> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.entries.get(name).cloned()
    }
}

/// The database is closed: what changed since it was saved is.
impl<T: Cached> Drop for IndexCache<T> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.save();
        }
    }
}
