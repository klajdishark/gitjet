//! Running git work off the UI thread.
//!
//! Starting a `git` process costs ~3 ms on macOS/Linux but 50–150 ms on Windows, so any git call
//! made while handling a key or drawing freezes the screen there. Views load data with these
//! helpers instead and apply the result when it arrives (the app redraws at least every 100 ms):
//!
//! - [`Background`]: one job at a time, newest wins (tab data loaded by `View::refresh`).
//! - [`Preview`]: debounced, cached loads keyed by the selection (detail panes and diff previews),
//!   so holding an arrow key loads only the item you stop on.
//!
//! In test builds jobs run inline, so tests can call `refresh()` and assert right away.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::git::Repo;

type Slot<T> = Arc<Mutex<Option<(u64, T)>>>;

/// A single background job whose latest result replaces the previous one.
pub struct Background<T: Send + 'static> {
    generation: u64,
    slot: Slot<T>,
    busy: bool,
}

impl<T: Send + 'static> Default for Background<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> Background<T> {
    pub fn new() -> Self {
        Self {
            generation: 0,
            slot: Arc::new(Mutex::new(None)),
            busy: false,
        }
    }

    /// Start `job` on a background thread. A result from an earlier, still running job is dropped.
    pub fn start(&mut self, repo: &Repo, job: impl FnOnce(&Repo) -> T + Send + 'static) {
        self.generation += 1;
        self.busy = true;
        let generation = self.generation;
        let slot = self.slot.clone();
        let repo = repo.clone();
        let run = move || {
            let value = job(&repo);
            if let Ok(mut s) = slot.lock() {
                // Keep only the newest generation's result.
                if s.as_ref().map_or(true, |(g, _)| *g < generation) {
                    *s = Some((generation, value));
                }
            }
        };
        if cfg!(test) {
            run();
        } else {
            std::thread::spawn(run);
        }
    }

    /// The result of the most recently started job, once it has finished.
    pub fn poll(&mut self) -> Option<T> {
        let mut s = self.slot.lock().ok()?;
        match s.take() {
            Some((g, v)) if g == self.generation => {
                self.busy = false;
                Some(v)
            }
            Some(_) => None,
            None => None,
        }
    }

    /// True between `start` and the `poll` that returns its result.
    pub fn busy(&self) -> bool {
        self.busy
    }
}

type Job<T> = Box<dyn FnOnce() -> T + Send>;

struct PreviewState<K, T> {
    pending: Option<(u64, K, Job<T>)>,
    done: Vec<(u64, K, T)>,
    closed: bool,
}

struct PreviewShared<K, T> {
    state: Mutex<PreviewState<K, T>>,
    wake: Condvar,
}

/// Debounced, cached background loads keyed by the current selection.
///
/// `request` replaces any not-yet-started request; a single worker waits `delay` for the selection
/// to settle, then runs only the newest job. Finished values are cached (most recent first) until
/// [`invalidate`](Self::invalidate) is called, e.g. after a refresh changed the work tree.
pub struct Preview<K: Clone + PartialEq + Send + 'static, T: Clone + Send + 'static> {
    shared: Arc<PreviewShared<K, T>>,
    cache: Vec<(K, T)>,
    capacity: usize,
    generation: u64,
    /// Requests older than this were made before the last invalidate and are ignored.
    valid_from: u64,
    requested: Option<K>,
}

impl<K: Clone + PartialEq + Send + 'static, T: Clone + Send + 'static> Preview<K, T> {
    pub fn new(delay: Duration, capacity: usize) -> Self {
        let shared = Arc::new(PreviewShared {
            state: Mutex::new(PreviewState {
                pending: None,
                done: Vec::new(),
                closed: false,
            }),
            wake: Condvar::new(),
        });
        if !cfg!(test) {
            let worker = shared.clone();
            std::thread::spawn(move || preview_worker(worker, delay));
        }
        Self {
            shared,
            cache: Vec::new(),
            capacity: capacity.max(1),
            generation: 0,
            valid_from: 0,
            requested: None,
        }
    }

    /// The cached value for `key`, collecting finished loads first.
    pub fn get(&mut self, key: &K) -> Option<T> {
        self.collect();
        let i = self.cache.iter().position(|(k, _)| k == key)?;
        let entry = self.cache.remove(i);
        let value = entry.1.clone();
        self.cache.insert(0, entry);
        Some(value)
    }

    /// Load `key` with `job` unless it is cached or already requested.
    pub fn request(&mut self, repo: &Repo, key: K, job: impl FnOnce(&Repo) -> T + Send + 'static) {
        if self.get(&key).is_some() || self.requested.as_ref() == Some(&key) {
            return;
        }
        self.generation += 1;
        self.requested = Some(key.clone());
        let repo = repo.clone();
        let job: Job<T> = Box::new(move || job(&repo));
        if cfg!(test) {
            let value = job();
            self.requested = None;
            self.store(key, value);
            return;
        }
        if let Ok(mut s) = self.shared.state.lock() {
            s.pending = Some((self.generation, key, job));
        }
        self.shared.wake.notify_one();
    }

    /// True while `key` has been requested but has not arrived yet.
    #[allow(dead_code)] // used by tests; views track what they show themselves
    pub fn is_loading(&mut self, key: &K) -> bool {
        self.collect();
        self.requested.as_ref() == Some(key)
    }

    /// Forget cached values (data changed); in-flight results from before are ignored.
    pub fn invalidate(&mut self) {
        self.cache.clear();
        self.requested = None;
        self.valid_from = self.generation + 1;
        if let Ok(mut s) = self.shared.state.lock() {
            s.pending = None;
            s.done.clear();
        }
    }

    fn collect(&mut self) {
        let done = match self.shared.state.lock() {
            Ok(mut s) => std::mem::take(&mut s.done),
            Err(_) => return,
        };
        for (g, k, v) in done {
            if g < self.valid_from {
                continue;
            }
            if self.requested.as_ref() == Some(&k) {
                self.requested = None;
            }
            self.store(k, v);
        }
    }

    fn store(&mut self, key: K, value: T) {
        self.cache.retain(|(k, _)| *k != key);
        self.cache.insert(0, (key, value));
        self.cache.truncate(self.capacity);
    }
}

impl<K: Clone + PartialEq + Send + 'static, T: Clone + Send + 'static> Drop for Preview<K, T> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.shared.state.lock() {
            s.closed = true;
        }
        self.shared.wake.notify_all();
    }
}

fn preview_worker<K: Send, T: Send>(shared: Arc<PreviewShared<K, T>>, delay: Duration) {
    loop {
        // Wait for a request.
        let mut s = match shared.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        while s.pending.is_none() && !s.closed {
            s = match shared.wake.wait(s) {
                Ok(s) => s,
                Err(_) => return,
            };
        }
        if s.closed {
            return;
        }
        // Let the selection settle: restart the wait while newer requests keep arriving.
        let seen = s.pending.as_ref().map(|(g, _, _)| *g);
        drop(s);
        std::thread::sleep(delay);
        let mut s = match shared.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if s.pending.as_ref().map(|(g, _, _)| *g) != seen {
            continue;
        }
        let Some((g, key, job)) = s.pending.take() else {
            continue;
        };
        drop(s);
        let value = job();
        if let Ok(mut s) = shared.state.lock() {
            s.done.push((g, key, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::temp_repo;

    #[test]
    fn background_returns_latest_result() {
        let (_d, repo) = temp_repo();
        let mut bg: Background<u32> = Background::new();
        assert!(bg.poll().is_none());
        bg.start(&repo, |_| 1);
        bg.start(&repo, |_| 2);
        assert!(bg.busy());
        assert_eq!(bg.poll(), Some(2));
        assert!(!bg.busy());
        assert_eq!(bg.poll(), None);
    }

    #[test]
    fn preview_caches_and_invalidates() {
        let (_d, repo) = temp_repo();
        let mut p: Preview<String, usize> = Preview::new(Duration::from_millis(10), 2);
        p.request(&repo, "a".into(), |_| 1);
        assert_eq!(p.get(&"a".into()), Some(1));
        // Cached: the job is not run again.
        p.request(&repo, "a".into(), |_| panic!("should be cached"));
        p.request(&repo, "b".into(), |_| 2);
        p.request(&repo, "c".into(), |_| 3);
        assert_eq!(p.get(&"a".into()), None, "capacity 2 evicts the oldest");
        assert_eq!(p.get(&"c".into()), Some(3));
        p.invalidate();
        assert_eq!(p.get(&"c".into()), None);
        assert!(!p.is_loading(&"c".into()));
    }
}
