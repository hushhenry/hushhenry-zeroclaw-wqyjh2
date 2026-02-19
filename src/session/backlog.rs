use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::LazyLock;

const DEFAULT_BACKLOG_CAP: usize = 16;

#[derive(Debug, Default)]
struct BacklogState {
    active_sessions: HashSet<String>,
    queues: HashMap<String, VecDeque<String>>,
}

static BACKLOG_STATE: LazyLock<Mutex<BacklogState>> =
    LazyLock::new(|| Mutex::new(BacklogState::default()));

pub fn try_begin_turn(session_key: &str) -> bool {
    let mut state = BACKLOG_STATE.lock();
    state.active_sessions.insert(session_key.to_string())
}

pub fn finish_turn(session_key: &str) {
    let mut state = BACKLOG_STATE.lock();
    state.active_sessions.remove(session_key);
}

pub fn enqueue(session_key: &str, message: impl Into<String>) {
    enqueue_with_cap(session_key, message.into(), DEFAULT_BACKLOG_CAP);
}

pub fn drain(session_key: &str) -> Vec<String> {
    let mut state = BACKLOG_STATE.lock();
    let Some(queue) = state.queues.remove(session_key) else {
        return Vec::new();
    };
    queue.into_iter().collect()
}

fn enqueue_with_cap(session_key: &str, message: String, cap: usize) {
    if cap == 0 {
        return;
    }

    let mut state = BACKLOG_STATE.lock();
    let queue = state
        .queues
        .entry(session_key.to_string())
        .or_insert_with(VecDeque::new);
    if queue.len() >= cap {
        let _ = queue.pop_front();
    }
    queue.push_back(message);
}

#[cfg(test)]
pub fn clear() {
    let mut state = BACKLOG_STATE.lock();
    state.active_sessions.clear();
    state.queues.clear();
}

#[cfg(test)]
mod tests {
    use super::{clear, drain, enqueue, enqueue_with_cap};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_session_key(prefix: &str) -> String {
        let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{n}")
    }

    #[test]
    fn backlog_enqueue_drain_preserves_order() {
        clear();
        let session_key = unique_session_key("session-a");

        enqueue(&session_key, "first");
        enqueue(&session_key, "second");
        enqueue(&session_key, "third");

        let drained = drain(&session_key);
        assert_eq!(drained, vec!["first", "second", "third"]);
        assert!(drain(&session_key).is_empty());
    }

    #[test]
    fn backlog_enqueue_drops_oldest_when_cap_reached() {
        clear();
        let session_key = unique_session_key("session-b");

        enqueue_with_cap(&session_key, "m1".to_string(), 2);
        enqueue_with_cap(&session_key, "m2".to_string(), 2);
        enqueue_with_cap(&session_key, "m3".to_string(), 2);

        let drained = drain(&session_key);
        assert_eq!(drained, vec!["m2", "m3"]);
    }
}
