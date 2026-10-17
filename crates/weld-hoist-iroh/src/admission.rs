//! Owned transport attempts and direct-service connection handoff.
use iroh::endpoint::Connection;
pub(crate) struct Accepted {
    pub pending: PendingConnection,
    pub adb_route: Option<iroh_base::CustomAddr>,
}
pub(crate) type IncomingQueue = async_channel::Receiver<crate::connection::IrohConnection>;

/// An established attempt remains close-on-drop until transferred into a live peer.
pub(crate) struct PendingConnection {
    pub connection: Connection,
    armed: bool,
}
impl PendingConnection {
    pub(crate) fn new(connection: Connection) -> Self {
        Self {
            connection,
            armed: true,
        }
    }
    pub fn hand_off(&mut self) {
        self.armed = false;
    }
}
impl Drop for PendingConnection {
    fn drop(&mut self) {
        if self.armed {
            self.connection
                .close(1_u32.into(), b"weld connection owner ended");
        }
    }
}
