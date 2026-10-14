//! Native key-name resolution requested by a registered input adapter.

use std::sync::mpsc::SyncSender;

use crate::LinuxKeycode;

pub struct ClientKeyboardQuery {
    /// Connection namespace within the registered adapter.
    pub connection: u64,
    pub names: Vec<String>,
    pub reply: SyncSender<Result<Vec<LinuxKeycode>, String>>,
}
