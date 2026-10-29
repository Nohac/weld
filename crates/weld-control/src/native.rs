//! Session-scoped native administration supplied by the distribution.

use crate::launch::{LaunchHandler, LaunchReceipt, LaunchRequest};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub enum DragRequest {
    Files(Vec<PathBuf>),
    Cancel,
}

pub struct NativeOperations {
    pub(crate) launch: LaunchHandler,
    pub(crate) drag: Box<dyn FnMut(DragRequest) -> Result<()> + Send>,
}
impl<F: FnMut(LaunchRequest) -> Result<LaunchReceipt> + Send + 'static> From<F>
    for NativeOperations
{
    fn from(launch: F) -> Self {
        Self {
            launch: Box::new(launch),
            drag: Box::new(|_| anyhow::bail!("file dragging requires a local desktop presenter")),
        }
    }
}
impl NativeOperations {
    pub fn with_drag(
        mut self,
        drag: impl FnMut(DragRequest) -> Result<()> + Send + 'static,
    ) -> Self {
        self.drag = Box::new(drag);
        self
    }
}
