//! Administrative process-launch records for a running Weld session.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The local administrator supplies exact arguments and an absolute directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub program: String,
    pub arguments: Vec<String>,
    pub directory: PathBuf,
}

/// Process creation completed. Application windows may arrive afterward.
#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchReceipt {
    pub process_id: u32,
}

pub(crate) type LaunchHandler =
    Box<dyn FnMut(LaunchRequest) -> anyhow::Result<LaunchReceipt> + Send>;
