//! Host-owned listeners attribute every admitted connection to its launch.

use std::{ffi::OsString, os::unix::fs::PermissionsExt, sync::Arc};

use anyhow::{Context, Result};
use calloop::{LoopHandle, RegistrationToken};
use smithay::wayland::socket::ListeningSocketSource;

use super::{ClientState, ServerState};

pub(crate) fn listen(
    handle: &LoopHandle<'static, ServerState>,
    session: &std::ffi::OsStr,
    launch: u64,
) -> Result<(OsString, RegistrationToken)> {
    let name = std::path::Path::new(session).with_file_name(format!("weld-run-{launch:08x}"));
    let listener = ListeningSocketSource::with_name(
        name.to_str().context("launch socket path must be UTF-8")?,
    )
    .context("could not bind attributed launch socket")?;
    let path = if name.is_absolute() {
        name
    } else {
        xdg::BaseDirectories::new()
            .get_runtime_directory()?
            .join(name)
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context("could not restrict launch socket permissions")?;
    let name = listener.socket_name().to_owned();
    let token = handle
        .insert_source(listener, move |stream, _, server| {
            let Some(id) = server.allocate_client_id() else {
                tracing::warn!("launch connection rejected: client identity exhausted");
                return;
            };
            let mut data = ClientState::new(id);
            data.launch_id = Some(launch);
            if let Err(error) = server.display_handle.insert_client(stream, Arc::new(data)) {
                tracing::warn!(%error, "launch connection rejected");
            }
        })
        .context("could not register attributed launch socket")?;
    Ok((name, token))
}
