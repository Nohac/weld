//! Iroh-specialized handles for the shared application session owner.
pub type DeviceSession =
    weld_hoist_core::session::client::DeviceSession<crate::IrohDestinationPeer>;
pub type PendingDeviceSession = weld_session::Operation<DeviceSession>;
#[cfg(test)]
mod tests;
