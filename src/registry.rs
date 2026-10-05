use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// What pages hold open (response bodies, sockets), per webview, by id.
pub(crate) struct Registry<T> {
    last_id: AtomicU64,
    state: Mutex<State<T>>,
}

struct State<T> {
    open: HashMap<(String, u64), Arc<T>>,
    generations: HashMap<String, u64>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self {
            last_id: AtomicU64::new(0),
            state: Mutex::new(State {
                open: HashMap::new(),
                generations: HashMap::new(),
            }),
        }
    }
}

impl<T> Registry<T> {
    /// The page generation of `webview`, to hand to [`Registry::insert`].
    pub(crate) fn generation(&self, webview: &str) -> u64 {
        self.lock().generations.get(webview).copied().unwrap_or(0)
    }

    /// Holds `entry` for `webview` under a new id; `None` when the page of
    /// `generation` is gone. The entry then drops outside the lock.
    pub(crate) fn insert(&self, webview: &str, generation: u64, entry: Arc<T>) -> Option<u64> {
        let mut state = self.lock();
        if state.generations.get(webview).copied().unwrap_or(0) != generation {
            drop(state);
            return None;
        }
        let id = self.last_id.fetch_add(1, Ordering::Relaxed) + 1;
        state.open.insert((webview.to_owned(), id), entry);
        Some(id)
    }

    pub(crate) fn get(&self, webview: &str, id: u64) -> Option<Arc<T>> {
        self.lock().open.get(&(webview.to_owned(), id)).cloned()
    }

    pub(crate) fn remove(&self, webview: &str, id: u64) -> Option<Arc<T>> {
        self.lock().open.remove(&(webview.to_owned(), id))
    }

    /// Takes every entry of `webview`, and refuses the ones its current page
    /// still has in flight.
    pub(crate) fn close_webview(&self, webview: &str) -> Vec<Arc<T>> {
        let mut state = self.lock();
        *state.generations.entry(webview.to_owned()).or_default() += 1;
        let keys: Vec<_> = state
            .open
            .keys()
            .filter(|(label, _)| label == webview)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| state.open.remove(&key))
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
