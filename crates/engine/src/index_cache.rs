//! Caches of index contents the engine keeps in memory (vector matrices, full-text
//! postings) for one committed state, kept up to date with later commits:
//! [`Database`](crate::Database) reports each commit's changes ([`IndexCache::published`]),
//! and the first query on a later state applies them instead of building the contents
//! again.

use std::collections::HashMap;
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
}

type WeakSnapshot = Weak<dyn GraphStore + Send + Sync>;

fn is(w: &WeakSnapshot, snapshot: &Snapshot) -> bool {
    std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(snapshot))
}

/// The contents of the indexes of kind `T` in one committed state.
pub(crate) struct IndexCache<T> {
    state: Mutex<CacheState<T>>,
}

impl<T> Default for IndexCache<T> {
    fn default() -> Self {
        IndexCache {
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
    /// Records a commit: `snapshot` is the new committed state, `changes` what the store
    /// says changed since the state published before it.
    pub fn published(&self, snapshot: &Snapshot, changes: Changes) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.latest = Some(Arc::downgrade(snapshot));
        if !st.entries.is_empty() {
            // Drop the entries of indexes that are gone.
            let defs = snapshot.indexes();
            st.entries.retain(|_, e| defs.contains(e.def()));
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
        graphbuff_core::accounting::exempt(|| self.fill(snapshot, def, interrupt))
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
                return Ok(Arc::new(T::build(&**snapshot, def, interrupt)?));
            }
        }
        if let Some(e) = st.entries.get(&def.name)
            && e.def() == def
        {
            return Ok(e.clone());
        }
        let e = Arc::new(T::build(&**snapshot, def, interrupt)?);
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
