//! Shared Bevy execution and GPU-image publication for Weld presentation hosts.
//!
//! Hosts supply their event loop, output targets and native image importers.
//! This layer owns the execution policy and render-asset visibility boundary.

mod execution;
mod image;

pub use execution::{PresentationPlugin, presentation_plugins};
pub use image::{publish_image, retire_image};
