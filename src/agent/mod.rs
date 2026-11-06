//! Agent integration: the CLI adapter, the output contract and prompt rendering.
//!
//! The three submodules enforce the boundary from opposite sides:
//!
//! * `agy` — the process boundary (argv, env allowlist, timeouts, envelopes),
//! * `contract` — the validation boundary (untrusted JSON → typed output),
//! * `prompt` — the input boundary (aliases only, no raw identifiers).

pub mod agy;
pub mod barrier;
pub mod contract;
pub mod prompt;
pub mod resident;
pub mod salvage;
pub mod segments;
pub mod step_stream;

pub use agy::{AgyPermissions, AgyRunner};
pub use contract::{
    AgentOutput, MemoryOp, MemoryOpKind, RejectReason, Rejections, ValidationScope,
};
pub use prompt::{PromptBatch, PromptMessage};
pub use resident::{
    DbStepReader, DeliverReceipt, ResidentConfig, ResidentError, ResidentSession, SessionState,
    StepReader,
};
pub use salvage::{SalvageLayer, SalvageReport};
