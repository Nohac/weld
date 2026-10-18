//! Connects local administrative RPC to the native host's process launcher.

pub(crate) mod launch;
pub(crate) mod windows;

use anyhow::Result;
use bevy::prelude::Resource;
use weld_access::AccessAuthority;
use weld_control::launch::{LaunchReceipt, LaunchRequest};

/// Shared administrative authority consumed by Master's emergency binding.
#[derive(Resource, Clone)]
pub(crate) struct SessionAccess(pub AccessAuthority);

pub(crate) fn launch_handler(
    launcher: impl Into<launch::Launcher>,
) -> impl FnMut(LaunchRequest) -> Result<LaunchReceipt> + Send + 'static {
    let launcher = launcher.into();
    move |request| launcher.launch(request)
}
