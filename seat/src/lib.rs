//! `cursor-seat`: one packet attempt from start to finish.
//!
//! Python keeps gates, router, steer loop, receipts, merge, yield, fence.
//! This crate owns stdin `SeatRequest` + control lines and stdout `SeatEvent`
//! JSONL. See `PROTOCOL.md` (frozen v1).

pub mod clip;
pub mod context;
pub mod inbox;
pub mod jev;
pub mod protocol;
pub mod retry;
pub mod run;
pub mod session;

pub use protocol::*;
