//! Bounded, privacy-safe session evidence and deterministic explanations.
//! Native integrations supply measurements; reports contain local monotonic
//! offsets and a shared session identifier, never assumed synchronized clocks.

mod explain;
mod recorder;
mod report;

pub use explain::{Confidence, Explanation, Finding, explain};
pub use recorder::Recorder;
pub use report::*;
