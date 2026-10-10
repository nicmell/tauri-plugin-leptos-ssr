use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

// WebKit cancels the pending `ipc://` requests of a page that navigates, and
// Tauri resends each over postMessage, where its command would run again. A
// call can also end just before its response is cancelled, so finished calls
// stay known for a while.

/// How many finished calls a webview remembers.
const FINISHED: usize = 64;

/// The `fetch` and `ws_open` calls that each webview started, by the id that
/// `fetch.js` gives them.
#[derive(Default)]
pub(crate) struct Calls {
    state: Mutex<HashMap<String, Seen>>,
}

#[derive(Default)]
struct Seen {
    running: HashSet<String>,
    finished: VecDeque<String>,
}

impl Calls {
    /// Starts call `id` of `webview`; `None` when it started before.
    pub(crate) fn start(&self, webview: &str, id: &str) -> Option<Running<'_>> {
        let mut state = self.lock();
        let seen = state.entry(webview.to_owned()).or_default();
        if seen.running.contains(id) || seen.finished.iter().any(|done| done == id) {
            return None;
        }
        seen.running.insert(id.to_owned());
        Some(Running {
            calls: self,
            webview: webview.to_owned(),
            id: id.to_owned(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Seen>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A call that runs; it counts as finished once dropped.
pub(crate) struct Running<'a> {
    calls: &'a Calls,
    webview: String,
    id: String,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut state = self.calls.lock();
        let Some(seen) = state.get_mut(&self.webview) else {
            return;
        };
        seen.running.remove(&self.id);
        if seen.finished.len() == FINISHED {
            seen.finished.pop_front();
        }
        seen.finished.push_back(std::mem::take(&mut self.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_starts_once() {
        let calls = Calls::default();
        let running = calls.start("main", "p.1").expect("a new call");
        assert!(calls.start("main", "p.1").is_none());
        drop(running);
        assert!(calls.start("main", "p.1").is_none());
        assert!(calls.start("main", "p.2").is_some());
    }

    #[test]
    fn calls_belong_to_their_webview() {
        let calls = Calls::default();
        let _running = calls.start("main", "p.1").expect("a new call");
        assert!(calls.start("other", "p.1").is_some());
    }

    #[test]
    fn finished_calls_are_remembered_up_to_a_bound() {
        let calls = Calls::default();
        for n in 0..=FINISHED {
            drop(calls.start("main", &format!("p.{n}")).expect("a new call"));
        }
        assert!(calls.start("main", "p.0").is_some());
        assert!(calls.start("main", &format!("p.{FINISHED}")).is_none());
    }
}
