//! Administrative process-launch records for a running Weld session.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The local administrator supplies exact arguments and an absolute directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub program: String,
    pub arguments: Vec<String>,
    pub directory: PathBuf,
    #[serde(default)]
    pub options: LaunchOptions,
}

/// Placement and control requested by the local administrator.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchOptions {
    pub workspace: Option<String>,
    pub owner: Option<String>,
    pub control: bool,
    pub headless: bool,
    pub shared_display: bool,
}

impl LaunchOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.shared_display
                || (self.workspace.is_none()
                    && self.owner.is_none()
                    && !self.control
                    && !self.headless),
            "shared-display launches do not support attribution, placement or automatic control"
        );
        anyhow::ensure!(
            !self.headless || self.control,
            "headless launches require control"
        );
        for value in [&self.workspace, &self.owner].into_iter().flatten() {
            anyhow::ensure!(
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
                "launch labels must contain 1–256 bytes without control characters"
            );
        }
        Ok(())
    }
}

/// Process creation completed. Application windows may arrive afterward.
#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchReceipt {
    pub process_id: u32,
    pub launch_id: Option<String>,
}

pub(crate) type LaunchHandler =
    Box<dyn FnMut(LaunchRequest) -> anyhow::Result<LaunchReceipt> + Send>;
