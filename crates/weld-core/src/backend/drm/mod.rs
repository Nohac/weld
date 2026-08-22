//! Standalone DRM host boundary.
//!
//! The previous low-level presenter was removed before rebuilding this backend
//! around Smithay's output compositor. Keeping this boundary explicit prevents
//! automatic backend selection from silently falling back to a different host.

use anyhow::{Result, bail};
use calloop::signals::Signals;

use crate::host::{PreparedHost, RunOptions};

pub(crate) fn prepare(_options: RunOptions, _signals: Signals) -> Result<PreparedHost> {
    bail!(
        "the standalone DRM backend is temporarily unavailable while it is rebuilt around Smithay's output compositor; use --backend nested for the working host or scripts/run-smithay-drm-compositor-probe to validate the DRM boundary"
    )
}
