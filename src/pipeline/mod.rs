//! Batching pipeline: dedupe, quiet-window batching, per-group scheduling and the
//! outbox.

pub mod batcher;
pub mod dedupe;
pub mod outbox;
pub mod scheduler;

pub use batcher::{BatchEntry, BatchLimits, FlushTrigger, should_flush, split_batches};
pub use dedupe::Dedupe;
pub use outbox::{DeliveryResult, Outbox};
pub use scheduler::{BatchExecutor, BatchJob, ExecOutcome, Scheduler};
