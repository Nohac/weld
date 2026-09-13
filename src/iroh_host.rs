//! Distribution binding policy shared by nested and presentation-free hosts.
use anyhow::Result;
use weld_hoist_iroh::{IrohDeviceIdentity, IrohHost};

use crate::AppArguments;

pub(crate) fn bind(arguments: &AppArguments) -> Result<IrohHost> {
    let network = arguments.hoist_iroh_network.unwrap_or_default().into();
    let host = match &arguments.hoist_iroh_device_dir {
        Some(directory) => {
            IrohHost::bind_with_identity(network, &IrohDeviceIdentity::load_or_create(directory)?)?
        }
        None => IrohHost::bind(network)?,
    };
    if let Some(path) = &arguments.hoist_iroh_publish_profile {
        host.connection_profile()?.save_new(path)?;
    }
    Ok(host)
}
