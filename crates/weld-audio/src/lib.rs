//! Application audio routing, capture, and playback through PipeWire.
//!
//! PipeWire objects belong to dedicated sleeping event-loop threads. Consumers
//! receive bounded, owned packets independently of compositor frame delivery.
mod capture;
mod format;
mod output;
mod playback;
pub use playback::Playback;
mod worker;
pub use capture::{Capture, CaptureTarget};
pub use output::{PrivateOutput, application_target, output_name, routing_environment};
