//! Batch assembly rules.
//!
//! All of this is pure logic on purpose: the timer lives in the scheduler, so the
//! rules that decide *when* a batch flushes and *how* an oversized group of messages
//! is cut are directly testable without sleeping.
//!
//! Rules from the design:
//!
//! * quiet window 5 s after the last message (no-gate path only; with a gate the model's
//!   score decides, see `app/intake.rs`),
//! * hard flush 30 s after the first message of the batch,
//! * a message that mentions the bot flushes immediately, without breaking per-group FIFO
//!   ordering; owner messages do not, because splitting a thought at its owner's
//!   punctuation would cut the conversation in half,
//! * a batch holds at most 50 messages, 8,000 characters and 4 images; anything
//!   beyond that is cut into sequential batches in arrival order.

/// Hard per-batch caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchLimits {
    pub max_messages: usize,
    pub max_chars: usize,
    pub max_images: usize,
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_messages: 50,
            max_chars: 8_000,
            max_images: 4,
        }
    }
}

/// One queued message as the batcher sees it. It deliberately carries no text: the
/// batcher only needs sizes and flags, so message bodies never leak into this layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchEntry {
    /// `pending_events.id`.
    pub id: i64,
    /// Character count of the plain text.
    pub chars: usize,
    /// Number of image segments in this message.
    pub images: usize,
    /// Whether the message mentions the bot.
    pub mentions_bot: bool,
    /// Whether the sender is the owner.
    ///
    /// Purely informational now: it is passed to the prompt so the agent knows whose
    /// message it is reading, but it no longer affects batching. Treating it as privileged
    /// split one thought into several batches.
    pub from_owner: bool,
    pub received_at: i64,
}

/// Why a batch was released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushTrigger {
    /// Nothing to do.
    Idle,
    /// A privileged message arrived; flush now.
    Immediate,
    /// The quiet window since the last message elapsed.
    Quiet,
    /// The hard age limit since the first message elapsed.
    MaxAge,
    /// Accumulated messages already hit a hard cap.
    Full,
}

impl FlushTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            FlushTrigger::Idle => "idle",
            FlushTrigger::Immediate => "immediate",
            FlushTrigger::Quiet => "quiet",
            FlushTrigger::MaxAge => "max_age",
            FlushTrigger::Full => "full",
        }
    }

    /// Whether this trigger should release a batch.
    pub fn is_flush(self) -> bool {
        !matches!(self, FlushTrigger::Idle)
    }
}

impl BatchEntry {
    /// A privileged entry forces an immediate flush.
    ///
    /// Only an @-mention qualifies. Whether she *replies* to the owner is decided by the
    /// agent from the persona, not by the batcher: making owner messages privileged split
    /// one thought into several batches, so a picture and the @ sent right after it
    /// reached her as two separate conversations.
    pub fn is_privileged(&self) -> bool {
        self.mentions_bot
    }
}

/// The shortest time a burst may live before it can be handed over.
/// Decide whether the queued entries should be flushed now.
///
/// This is the no-gate path: with nobody judging whether a sentence is finished, the only
/// available signal is time, so quiet and max-age carry the whole decision. The gate path
/// in `app/intake.rs` does not call this at all — it has a score and uses it instead.
///
/// `now` and `first_at` are unix seconds. `last_activity` is the timestamp of the
/// most recent queued message.
pub fn should_flush(
    entries: &[BatchEntry],
    first_at: i64,
    last_activity: i64,
    now: i64,
    quiet_seconds: i64,
    max_age_seconds: i64,
    limits: BatchLimits,
) -> FlushTrigger {
    if entries.is_empty() {
        return FlushTrigger::Idle;
    }
    // Hard caps win over everything so a burst can never build an unbounded batch.
    let totals = totals_of(entries);
    if entries.len() >= limits.max_messages
        || totals.1 >= limits.max_chars
        || totals.2 >= limits.max_images
    {
        return FlushTrigger::Full;
    }
    if entries.iter().any(|e| e.is_privileged()) {
        return FlushTrigger::Immediate;
    }
    if now - last_activity >= quiet_seconds {
        return FlushTrigger::Quiet;
    }
    if now - first_at >= max_age_seconds {
        return FlushTrigger::MaxAge;
    }
    FlushTrigger::Idle
}

/// `(messages, chars, images)` totals.
pub fn totals_of(entries: &[BatchEntry]) -> (usize, usize, usize) {
    let mut chars = 0;
    let mut images = 0;
    for e in entries {
        chars += e.chars;
        images += e.images;
    }
    (entries.len(), chars, images)
}

/// Cut one oversized pending list into sequential batches that respect all caps.
///
/// Arrival order is preserved and no entry is ever dropped; a single message that
/// alone exceeds `max_chars` still forms its own batch rather than being truncated.
pub fn split_batches(entries: &[BatchEntry], limits: BatchLimits) -> Vec<Vec<BatchEntry>> {
    let mut batches: Vec<Vec<BatchEntry>> = Vec::new();
    let mut current: Vec<BatchEntry> = Vec::new();
    let mut chars = 0usize;
    let mut images = 0usize;

    for entry in entries {
        let would_exceed = !current.is_empty()
            && (current.len() + 1 > limits.max_messages
                || chars + entry.chars > limits.max_chars
                || images + entry.images > limits.max_images);
        if would_exceed {
            batches.push(std::mem::take(&mut current));
            chars = 0;
            images = 0;
        }
        current.push(*entry);
        chars += entry.chars;
        images += entry.images;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, chars: usize, images: usize) -> BatchEntry {
        BatchEntry {
            id,
            chars,
            images,
            mentions_bot: false,
            from_owner: false,
            received_at: 0,
        }
    }

    fn limits() -> BatchLimits {
        BatchLimits::default()
    }

    #[test]
    fn should_flush_covers_each_trigger() {
        assert_eq!(
            should_flush(&[], 0, 0, 1_000, 5, 30, limits()),
            FlushTrigger::Idle
        );

        let entries = vec![entry(1, 10, 0)];
        // 4 seconds of silence: still waiting.
        assert_eq!(
            should_flush(&entries, 100, 104, 108, 5, 30, limits()),
            FlushTrigger::Idle
        );
        // 5 seconds of silence: flush.
        assert_eq!(
            should_flush(&entries, 100, 104, 109, 5, 30, limits()),
            FlushTrigger::Quiet
        );
        // Message arrived 1s ago, but the batch started 30s ago.
        assert_eq!(
            should_flush(&entries, 100, 130, 131, 5, 30, limits()),
            FlushTrigger::MaxAge
        );

        let mut mentioned = vec![entry(1, 10, 0), entry(2, 10, 0)];
        mentioned[1].mentions_bot = true;
        // With no gate, an @ is the only "send now" signal there is, so it releases the
        // burst at once instead of waiting for the quiet window.
        assert_eq!(
            should_flush(&mentioned, 100, 100, 100, 5, 30, limits()),
            FlushTrigger::Immediate
        );

        // The owner is not privileged: whether she answers is her decision, and splitting
        // the burst would drop the picture that came right before the message.
        let mut owned = vec![entry(1, 10, 0)];
        owned[0].from_owner = true;
        assert_eq!(
            should_flush(&owned, 100, 100, 105, 5, 30, limits()),
            FlushTrigger::Quiet
        );

        let mut full = Vec::new();
        for i in 0..50 {
            full.push(entry(i, 1, 0));
        }
        assert_eq!(
            should_flush(&full, 100, 100, 100, 5, 30, limits()),
            FlushTrigger::Full
        );
    }

    #[test]
    fn split_respects_message_cap_and_keeps_order() {
        let entries: Vec<_> = (0..120).map(|i| entry(i, 1, 0)).collect();
        let batches = split_batches(&entries, limits());
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 50);
        assert_eq!(batches[1].len(), 50);
        assert_eq!(batches[2].len(), 20);
        // Order is strictly preserved across cuts.
        let flat: Vec<i64> = batches.iter().flatten().map(|e| e.id).collect();
        assert_eq!(flat, (0..120).collect::<Vec<i64>>());
    }

    #[test]
    fn split_respects_char_budget() {
        let entries: Vec<_> = (0..5).map(|i| entry(i, 3_000, 0)).collect();
        let batches = split_batches(&entries, limits());
        // 3000 * 2 = 6000 fits, the third would be 9000 > 8000.
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 2);
        assert_eq!(batches[2].len(), 1);
    }

    #[test]
    fn split_respects_image_budget() {
        let entries: Vec<_> = (0..6).map(|i| entry(i, 10, 1)).collect();
        let batches = split_batches(&entries, limits());
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 4);
        assert_eq!(batches[1].len(), 2);
    }

    #[test]
    fn oversized_single_message_still_forms_one_batch() {
        // A 20k character message cannot be split; it must not be truncated either.
        let entries = vec![entry(1, 20_000, 0)];
        let batches = split_batches(&entries, limits());
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[0][0].chars, 20_000);
    }
}
