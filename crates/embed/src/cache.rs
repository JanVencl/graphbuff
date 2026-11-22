//! Loaded models, shared by concurrent queries: each is loaded on first use, once, and
//! the least recently used ones are dropped beyond a limit. A query that still uses a
//! dropped model keeps its copy until it finishes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// One model's place in the cache.
struct Slot<T> {
    /// Locked while the model loads, so a second query waits instead of loading again.
    model: Mutex<Option<Arc<T>>>,
    last_used: AtomicU64,
}

pub struct ModelCache<T> {
    max_loaded: usize,
    slots: Mutex<HashMap<String, Arc<Slot<T>>>>,
    clock: AtomicU64,
    loads: AtomicUsize,
}

fn lock<V>(m: &Mutex<V>) -> MutexGuard<'_, V> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<T> ModelCache<T> {
    /// A cache that keeps at most `max_loaded` models (at least 1).
    pub fn new(max_loaded: usize) -> Self {
        ModelCache { max_loaded: max_loaded.max(1), slots: Mutex::default(), clock: AtomicU64::new(0), loads: AtomicUsize::new(0) }
    }

    /// The model `name`, loaded with `load` if it is not loaded.
    pub fn get(&self, name: &str, load: impl FnOnce() -> Result<T, String>) -> Result<Arc<T>, String> {
        let slot = lock(&self.slots).entry(name.to_string()).or_insert_with(|| Arc::new(Slot { model: Mutex::new(None), last_used: AtomicU64::new(0) })).clone();
        slot.last_used.store(self.clock.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        let mut model = lock(&slot.model);
        if let Some(m) = &*model {
            return Ok(m.clone());
        }
        let loaded = Arc::new(load()?);
        self.loads.fetch_add(1, Ordering::Relaxed);
        *model = Some(loaded.clone());
        drop(model);
        self.evict(name);
        Ok(loaded)
    }

    /// Drops the least recently used models beyond the limit, but not `keep` and not one
    /// that is loading.
    fn evict(&self, keep: &str) {
        let slots = lock(&self.slots);
        let mut loaded: Vec<(u64, &String, &Arc<Slot<T>>)> = slots
            .iter()
            .filter(|(_, s)| s.model.try_lock().map(|m| m.is_some()).unwrap_or(false))
            .map(|(n, s)| (s.last_used.load(Ordering::Relaxed), n, s))
            .collect();
        loaded.sort_by_key(|(t, ..)| *t);
        let mut excess = loaded.len().saturating_sub(self.max_loaded);
        for (_, name, slot) in loaded {
            if excess == 0 {
                break;
            }
            if name != keep
                && let Ok(mut m) = slot.model.try_lock()
            {
                *m = None;
                excess -= 1;
            }
        }
    }

    /// Whether `name` is loaded now.
    pub fn is_loaded(&self, name: &str) -> bool {
        lock(&self.slots).get(name).is_some_and(|s| s.model.try_lock().map(|m| m.is_some()).unwrap_or(false))
    }

    /// How many times a model was loaded.
    pub fn loads(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_once_and_evicts_the_oldest() {
        let c = ModelCache::<String>::new(2);
        for _ in 0..3 {
            assert_eq!(*c.get("a", || Ok("A".into())).unwrap(), "A");
        }
        assert_eq!(c.loads(), 1);
        c.get("b", || Ok("B".into())).unwrap();
        c.get("a", || unreachable!()).unwrap(); // a is now newer than b
        let held_b = c.get("b", || unreachable!()).unwrap();
        c.get("a", || unreachable!()).unwrap();
        c.get("c", || Ok("C".into())).unwrap();
        assert!(c.is_loaded("a") && c.is_loaded("c") && !c.is_loaded("b"));
        assert_eq!(*held_b, "B", "a dropped model stays usable by whoever holds it");
        assert!(c.get("x", || Err("broken".into())).is_err());
        assert!(!c.is_loaded("x"));
        assert_eq!(c.loads(), 3);
    }

    #[test]
    fn concurrent_gets_share_one_load() {
        let c = Arc::new(ModelCache::<u32>::new(1));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || {
                    *c.get("m", || {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        Ok(7)
                    })
                    .unwrap()
                })
            })
            .collect();
        for t in threads {
            assert_eq!(t.join().unwrap(), 7);
        }
        assert_eq!(c.loads(), 1);
    }
}
