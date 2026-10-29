//! Connects local administrative RPC to native host operations.

pub(crate) mod launch;
pub(crate) mod windows;

use anyhow::Result;
use bevy::prelude::Resource;
use weld_access::AccessAuthority;
use weld_control::launch::{LaunchReceipt, LaunchRequest};
use weld_core::runtime::drag::{DragRequest, FileDragPayload};

/// Shared administrative authority consumed by Master's emergency binding.
#[derive(Resource, Clone)]
pub(crate) struct SessionAccess(pub AccessAuthority);

pub(crate) fn launch_handler(
    launcher: impl Into<launch::Launcher>,
) -> impl FnMut(LaunchRequest) -> Result<LaunchReceipt> + Send + 'static {
    let launcher = launcher.into();
    move |request| launcher.launch(request)
}

pub(crate) fn native_operations(
    launcher: launch::Launcher,
    drag: weld_core::runtime::drag::DragController,
) -> weld_control::native::NativeOperations {
    weld_control::native::NativeOperations::from(launch_handler(launcher)).with_drag(
        move |request| {
            let request = match request {
                weld_control::native::DragRequest::Files(paths) => {
                    DragRequest::Begin(FileDragPayload::from_paths(&paths)?)
                }
                weld_control::native::DragRequest::Cancel => DragRequest::Cancel,
            };
            drag.request(request)
        },
    )
}
