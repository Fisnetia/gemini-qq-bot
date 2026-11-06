//! OneBot v11 transport.

pub mod action;
pub mod connection;
pub mod event;
pub mod normalize;
pub mod private;

pub use connection::{ConnectionHandle, ConnectionStatus};
pub use event::{RawEvent, RawMessageSegment};
