//! DRM device, connector, and mode discovery independent of presentation.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use smithay::{
    backend::{
        drm::DrmDeviceFd,
        session::{Session, libseat::LibSeatSession},
        udev::primary_gpu,
    },
    output::{Mode as SmithayOutputMode, PhysicalProperties},
    reexports::{
        drm::control::{Mode, ModeTypeFlags, connector},
        rustix::fs::{Dev, OFlags},
    },
    utils::DeviceFd,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{
    OutputScale,
    server::{OutputDescriptor, OutputMetrics},
};

pub(super) struct DrmDeviceDiscovery {
    pub(super) session: LibSeatSession,
    pub(super) drm: DrmDeviceFd,
    pub(super) device_id: Dev,
    pub(super) device_path: PathBuf,
}

pub(super) struct DrmOutputDiscovery {
    pub(super) scanner: DrmScanner,
    pub(super) connector: connector::Info,
    pub(super) mode: Mode,
}

impl DrmDeviceDiscovery {
    pub(super) fn new<'a>(
        mut session: LibSeatSession,
        devices: impl Iterator<Item = (Dev, &'a Path)>,
    ) -> Result<Self> {
        let primary =
            primary_gpu(session.seat())?.context("no DRM GPU was found for the active seat")?;
        let devices = devices
            .map(|(device_id, path)| (device_id, path.to_path_buf()))
            .collect::<Vec<_>>();
        let (device_id, device_path) = devices
            .iter()
            .find(|(_, path)| *path == primary)
            .cloned()
            .or_else(|| devices.first().cloned())
            .context("udev reported no DRM devices for the active seat")?;
        let fd = session
            .open(
                &device_path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .with_context(|| format!("failed to open DRM device {}", device_path.display()))?;
        let drm = DrmDeviceFd::new(DeviceFd::from(fd));
        Ok(Self {
            session,
            drm,
            device_id,
            device_path,
        })
    }
}

pub(super) fn discover_output(
    drm: &impl smithay::reexports::drm::control::Device,
) -> Result<DrmOutputDiscovery> {
    let mut scanner = DrmScanner::new();
    let (connector, _crtc) = scanner
        .scan_connectors(drm)?
        .into_iter()
        .find_map(|event| match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } if !connector.modes().is_empty() => Some((connector, crtc)),
            _ => None,
        })
        .context("no connected DRM connector with a usable CRTC and mode")?;
    let mode = preferred_mode(&connector)?;
    Ok(DrmOutputDiscovery {
        scanner,
        connector,
        mode,
    })
}

pub(super) fn output_description(
    connector: &connector::Info,
    mode: Mode,
    scale: OutputScale,
) -> Result<(OutputDescriptor, OutputMetrics)> {
    let name = connector_name(connector);
    let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
    let wl_mode = SmithayOutputMode::from(mode);
    let metrics = OutputMetrics::new(
        u32::try_from(wl_mode.size.w).context("negative DRM mode width")?,
        u32::try_from(wl_mode.size.h).context("negative DRM mode height")?,
        scale,
    )?
    .with_refresh_millihertz(wl_mode.refresh)?;
    let descriptor = OutputDescriptor {
        name,
        physical_properties: PhysicalProperties {
            size: (
                i32::try_from(physical_width).context("physical output width exceeds i32")?,
                i32::try_from(physical_height).context("physical output height exceeds i32")?,
            )
                .into(),
            subpixel: connector.subpixel().into(),
            make: "Unknown".to_owned(),
            model: "Unknown".to_owned(),
            serial_number: "Unknown".to_owned(),
        },
    };
    Ok((descriptor, metrics))
}

pub(super) fn preferred_mode(connector: &connector::Info) -> Result<Mode> {
    connector
        .modes()
        .iter()
        .copied()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first().copied())
        .context("DRM connector has no modes")
}

pub(super) fn connector_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    )
}
