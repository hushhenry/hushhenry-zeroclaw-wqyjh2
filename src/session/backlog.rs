use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::LazyLock;

const DEFAULT_BACKLOG_CAP: usize = 16;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BacklogItem {
    UserMessage(String),
    Resume { history_json: String },
}

#[derive(Debug, Default)]
struct BacklogState {
    active_sessions: HashSet<String>,
    queues: HashMap<String, VecDeque<BacklogItem>>,
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

pub fn enqueue(session_key: &str, item: BacklogItem) {
    enqueue_with_cap(session_key, item, DEFAULT_BACKLOG_CAP);
}

pub fn enqueue_user_message(session_key: &str, message: impl Into<String>) {
    enqueue(session_key, BacklogItem::UserMessage(message.into()));
}

pub fn drain(session_key: &str) -> Vec<BacklogItem> {
    let mut state = BACKLOG_STATE.lock();
    let Some(queue) = state.queues.remove(session_key) else {
        return Vec::new();
    };
    queue.into_iter().collect()
}

pub fn has_messages(session_key: &str) -> bool {
    let state = BACKLOG_STATE.lock();
    state
        .queues
        .get(session_key)
        .map_or(false, |q| !q.is_empty())
}

fn enqueue_with_cap(session_key: &str, item: BacklogItem, cap: usize) {
    if cap == 0 {
        return;
    }

    let mut state = BACKLOG_STATE.lock();
    let queue = state
        .queues
        .entry(session_key.to_string())
        .or_default();
    if queue.len() >= cap {
        let _ = queue.pop_front();
    }
    queue.push_back(item);
}

#[cfg(test)]
pub fn clear() {
    let mut state = BACKLOG_STATE.lock();
    state.active_sessions.clear();
    state.queues.clear();
}

#[cfg(test)]
mod tests {
    use super::{clear, drain, enqueue_user_message, enqueue_with_cap, BacklogItem};
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

        enqueue_user_message(&session_key, "first");
        enqueue_user_message(&session_key, "second");
        enqueue_user_message(&session_key, "third");

        let drained = drain(&session_key);
        assert_eq!(
            drained,
            vec![
                BacklogItem::UserMessage("first".into()),
                BacklogItem::UserMessage("second".into()),
                BacklogItem::UserMessage("third".into())
            ]
        );
        assert!(drain(&session_key).is_empty());
    }

    #[test]
    fn backlog_enqueue_drops_oldest_when_cap_reached() {
        clear();
        let session_key = unique_session_key("session-b");

        enqueue_with_cap(&session_key, BacklogItem::UserMessage("m1".to_string()), 2);
        enqueue_with_cap(&session_key, BacklogItem::UserMessage("m2".to_string()), 2);
        enqueue_with_cap(&session_key, BacklogItem::UserMessage("m3".to_string()), 2);

        let drained = drain(&session_key);
        assert_eq!(
            drained,
            vec![
                BacklogItem::UserMessage("m2".into()),
                BacklogItem::UserMessage("m3".into())
            ]
        );
    }
}
