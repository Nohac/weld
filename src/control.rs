//! Connects local administrative RPC to the native host's process launcher.

use anyhow::Result;
use weld_control::launch::{LaunchReceipt, LaunchRequest};
use weld_core::runtime::launch::{ApplicationLaunch, ClientLauncher};

pub(crate) fn launch_handler(
    launcher: ClientLauncher,
) -> impl FnMut(LaunchRequest) -> Result<LaunchReceipt> + Send + 'static {
    move |request| {
        let process_id = launcher.launch(ApplicationLaunch {
            program: request.program.into(),
            arguments: request.arguments.into_iter().map(Into::into).collect(),
            directory: request.directory,
        })?;
        Ok(LaunchReceipt { process_id })
    }
}
