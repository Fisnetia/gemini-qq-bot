//! Strike counting for prompt-level content-filter rejections.
//!
//! An upstream filter that refuses a prompt refuses it deterministically:
//! the same text gets the same answer. The turn path used to retire the
//! session and hand the batch back to the pool, so the next spawn
//! resubmitted the identical material and was refused again — one
//! poisoned batch burned a fresh session per retry until its messages
//! aged into the stale drop (2026-11-06, group 943: thirteen refusals in
//! five minutes). This tracker counts consecutive rejections per group;
//! when a batch that overlaps the previously refused one is refused a
//! second time, the caller drops the covered events instead of
//! releasing them.
//!
//! In memory on purpose, like the respawn ladder: losing the count on
//! restart degrades to the pre-fix release-and-retry behaviour for one
//! extra round, nothing worse.

use std::collections::HashMap;

/// One group's last refused batch.
struct Rejection {
    /// Event ids the refused turn covered (sorted, deduplicated).
    event_ids: Vec<i64>,
    /// Consecutive refusals whose batches overlap this one.
    consecutive: u32,
}

#[derive(Default)]
pub struct PromptRejections {
    groups: HashMap<String, Rejection>,
}

impl PromptRejections {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one prompt-level rejection of `event_ids` in `group`.
    /// Returns true when the batch should be dropped: it is the second
    /// consecutive rejection and the batch still covers at least one
    /// event the previous rejected batch covered. A drop consumes the
    /// record, so the next rejection starts a fresh count.
    pub fn note(&mut self, group: &str, event_ids: &[i64]) -> bool {
        if event_ids.is_empty() {
            return false;
        }
        let mut ids: Vec<i64> = event_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let consecutive = match self.groups.get(group) {
            Some(prev) if prev.event_ids.iter().any(|id| ids.contains(id)) => prev.consecutive + 1,
            _ => 1,
        };
        if consecutive >= 2 {
            self.groups.remove(group);
            return true;
        }
        self.groups.insert(
            group.to_string(),
            Rejection {
                event_ids: ids,
                consecutive,
            },
        );
        false
    }

    /// Forget a group's record: a batch was consumed normally, so an
    /// earlier rejection is no longer consecutive with what comes next.
    pub fn clear(&mut self, group: &str) {
        self.groups.remove(group);
    }
}

impl super::ResidentManager {
    /// Record a prompt-level rejection for the manager's strike count.
    /// True means the covered events are poison: drop them.
    pub async fn note_prompt_rejection(&self, group_id: &str, event_ids: &[i64]) -> bool {
        self.prompt_rejections
            .lock()
            .await
            .note(group_id, event_ids)
    }

    /// Clear a group's strike count after a batch was consumed.
    pub async fn clear_prompt_rejections(&self, group_id: &str) {
        self.prompt_rejections.lock().await.clear(group_id);
    }
}

#[cfg(test)]
mod tests;
