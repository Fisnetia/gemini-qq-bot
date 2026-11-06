//! Derived group state: mood, topic and the memory seams used by the pipeline.
//!
//! Nothing here decides policy; these modules only *apply* already-validated updates so
//! the pipeline can stay readable. Mood and topic are stored as small JSON documents,
//! which is why `MoodState` has a tolerant parser: a corrupt value must not brick a
//! group, it must fall back to "unknown".

pub mod conversation;
pub mod memory;
pub mod mood;

pub use conversation::ConversationState;
pub use memory::MemoryApplier;
pub use mood::{MoodState, TopicState};
