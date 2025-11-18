use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub app: String,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub request_id: String,
    pub context: Context,
    pub recording: bool,
    pub since_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    ClientStop,
    Timeout,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueState {
    queue: VecDeque<Item>,
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }
}

impl QueueState {
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn enqueue(
        &mut self,
        request_id: impl Into<String>,
        context: Context,
        since_ms: u64,
    ) -> usize {
        let item = Item {
            request_id: request_id.into(),
            context,
            recording: false,
            since_ms,
        };
        self.queue.push_back(item);
        self.queue.len() - 1
    }

    /// Marks the head item as recording if any.
    pub fn promote(&mut self) -> Option<&Item> {
        if let Some(front) = self.queue.front_mut() {
            front.recording = true;
            return self.queue.front();
        }
        None
    }

    /// Removes and returns the active item from the front of the queue.
    pub fn stop_active(&mut self, reason: StopReason) -> Option<StoppedItem> {
        if let Some(mut front) = self.queue.pop_front() {
            front.recording = false;
            Some(StoppedItem {
                item: front,
                reason,
            })
        } else {
            None
        }
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let active_request_id = self
            .queue
            .front()
            .filter(|item| item.recording)
            .map(|item| item.request_id.clone());

        StatusSnapshot {
            ok: true,
            active_request_id,
            queue: self.queue.iter().cloned().collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedItem {
    pub item: Item,
    pub reason: StopReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub ok: bool,
    pub active_request_id: Option<String>,
    pub queue: Vec<Item>,
}

#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub decode_ms: u64,
    pub lang: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(app: &str, hint: &str) -> Context {
        Context {
            app: app.to_string(),
            hint: hint.to_string(),
        }
    }

    #[test]
    fn enqueue_preserves_order_and_inactive_by_default() {
        let mut state = QueueState::default();
        state.enqueue("rq_a", ctx("chrome", "omnibox"), 10);
        state.enqueue("rq_b", ctx("vscode", "editor"), 20);

        assert_eq!(state.len(), 2);
        assert!(!state.is_empty());
        let first = state.queue.front().unwrap();
        let second = state.queue.get(1).unwrap();
        assert_eq!(&first.request_id, "rq_a");
        assert!(!first.recording);
        assert_eq!(&second.request_id, "rq_b");
        assert!(!second.recording);
    }

    #[test]
    fn promote_marks_head_recording() {
        let mut state = QueueState::default();
        state.enqueue("rq_a", ctx("chrome", "omnibox"), 10);
        state.enqueue("rq_b", ctx("vscode", "editor"), 20);

        let promoted = state.promote().expect("should have head");
        assert_eq!(promoted.request_id, "rq_a");
        assert!(promoted.recording);
        // ensure internal state reflects promotion
        let snapshot = state.snapshot();
        assert_eq!(snapshot.active_request_id.as_deref(), Some("rq_a"));
    }

    #[test]
    fn stop_active_removes_active_and_allows_next_promotion() {
        let mut state = QueueState::default();
        state.enqueue("rq_a", ctx("chrome", "omnibox"), 10);
        state.enqueue("rq_b", ctx("vscode", "editor"), 20);

        state.promote();
        let stopped = state
            .stop_active(StopReason::ClientStop)
            .expect("active should stop");
        assert_eq!(stopped.item.request_id, "rq_a");
        assert_eq!(stopped.reason, StopReason::ClientStop);
        assert_eq!(state.len(), 1);
        assert_eq!(state.snapshot().active_request_id, None);

        let next = state.promote().expect("next item");
        assert_eq!(next.request_id, "rq_b");
        assert!(next.recording);
    }

    #[test]
    fn snapshot_json_shape_matches_spec() {
        let mut state = QueueState::default();
        state.enqueue("rq_a", ctx("chrome", "omnibox"), 10);
        state.enqueue("rq_b", ctx("vscode", "editor"), 20);
        state.promote();

        let snapshot = state.snapshot();
        let json = serde_json::to_value(&snapshot).expect("serialize snapshot");
        let expected = serde_json::json!({
            "ok": true,
            "active_request_id": "rq_a",
            "queue": [
                {
                    "request_id": "rq_a",
                    "context": {"app": "chrome", "hint": "omnibox"},
                    "recording": true,
                    "since_ms": 10
                },
                {
                    "request_id": "rq_b",
                    "context": {"app": "vscode", "hint": "editor"},
                    "recording": false,
                    "since_ms": 20
                }
            ]
        });
        assert_eq!(json, expected);
    }
}
