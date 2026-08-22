//! Translation from one DRM connector into Weld and Smithay output state.

use std::time::Duration;

use anyhow::{Context, Result};
use smithay::{
    backend::drm::DrmDeviceFd,
    output::{Mode as SmithayMode, PhysicalProperties},
    reexports::drm::control::{Device as ControlDevice, Mode, ModeTypeFlags, connector, crtc},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::warn;

use crate::{
    OutputConfiguration, OutputHead, OutputId, OutputPhysicalSize, OutputScale,
    server::{OutputDescriptor, OutputMetrics, ServerOutputDefinition},
    surface::{Extent, LogicalPoint},
};

pub(super) struct SelectedOutput {
    pub(super) connector: connector::Info,
    pub(super) crtc: crtc::Handle,
    pub(super) mode: Mode,
    pub(super) id: OutputId,
    pub(super) head: OutputHead,
    pub(super) configuration: OutputConfiguration,
    pub(super) definition: ServerOutputDefinition,
    pub(super) frame_interval: Duration,
}

#[derive(Debug, Eq, PartialEq)]
struct OutputCandidate {
    name: String,
    modes: Vec<ModeCandidate>,
}

#[derive(Debug, Eq, PartialEq)]
struct ModeCandidate {
    preferred: bool,
}

pub(super) fn select_output(drm: &DrmDeviceFd, scale: OutputScale) -> Result<SelectedOutput> {
    let mut scanner: DrmScanner = DrmScanner::new();
    let mut connected = scanner
        .scan_connectors(drm)?
        .into_iter()
        .filter_map(|event| match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } if !connector.modes().is_empty() && !is_non_desktop(drm, connector.handle()) => {
                Some((connector, crtc))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let additional_connectors = connected.len().saturating_sub(1);
    let candidates = connected
        .iter()
        .map(|(connector, _)| OutputCandidate {
            name: connector_name(connector),
            modes: connector
                .modes()
                .iter()
                .map(|mode| ModeCandidate {
                    preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let (selected, mode_index) = choose_output_candidate(&candidates)
        .context("no connected desktop connector with a usable CRTC and mode")?;
    let (connector, crtc) = connected.swap_remove(selected);
    let mode = connector
        .modes()
        .get(mode_index)
        .copied()
        .context("selected DRM connector has no mode")?;
    let name = connector_name(&connector);
    let physical_size = connector
        .size()
        .and_then(|(width, height)| OutputPhysicalSize::new(width, height));
    let id = OutputId::new(1);
    let mode_size = mode.size();
    let extent = Extent::new(u32::from(mode_size.0), u32::from(mode_size.1));
    let smithay_mode = SmithayMode::from(mode);
    let metrics = OutputMetrics::new(extent.width, extent.height, scale)?
        .with_refresh_millihertz(smithay_mode.refresh)?;
    let configuration =
        OutputConfiguration::new(id, extent, scale, LogicalPoint::ZERO, true, physical_size)?;
    let head = OutputHead::new(id, name.clone(), physical_size);
    let physical_size_for_protocol = physical_size
        .map(|size| {
            Ok::<_, anyhow::Error>((
                i32::try_from(size.width_millimeters())
                    .context("connector physical width exceeds i32")?,
                i32::try_from(size.height_millimeters())
                    .context("connector physical height exceeds i32")?,
            ))
        })
        .transpose()?
        .unwrap_or_default();
    let definition = ServerOutputDefinition {
        id,
        descriptor: OutputDescriptor {
            name: name.clone(),
            physical_properties: PhysicalProperties {
                size: physical_size_for_protocol.into(),
                subpixel: connector.subpixel().into(),
                make: "Unknown".to_owned(),
                model: name.clone(),
                serial_number: "Unknown".to_owned(),
            },
        },
        metrics,
        logical_position: (0, 0),
        primary: true,
    };
    if additional_connectors > 0 {
        warn!(
            connector = %name,
            additional_connectors,
            "the first Smithay DRM slice enables one connector; additional connectors are deferred"
        );
    }
    Ok(SelectedOutput {
        connector,
        crtc,
        mode,
        id,
        head,
        configuration,
        definition,
        frame_interval: refresh_interval(smithay_mode.refresh)?,
    })
}

pub(super) fn metrics_for_configuration(
    configuration: OutputConfiguration,
    mode: Mode,
) -> Result<OutputMetrics> {
    OutputMetrics::new(
        configuration.extent().width,
        configuration.extent().height,
        configuration.scale(),
    )?
    .with_refresh_millihertz(SmithayMode::from(mode).refresh)
}

fn connector_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    )
}

fn is_internal_connector(name: &str) -> bool {
    ["eDP-", "LVDS-", "DSI-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn choose_output_candidate(candidates: &[OutputCandidate]) -> Option<(usize, usize)> {
    let connector_index = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| !candidate.modes.is_empty())
        .min_by_key(|(_, candidate)| {
            (
                !is_internal_connector(&candidate.name),
                candidate.name.as_str(),
            )
        })
        .map(|(index, _)| index)?;
    let mode_index = candidates[connector_index]
        .modes
        .iter()
        .position(|mode| mode.preferred)
        .unwrap_or_default();
    Some((connector_index, mode_index))
}

fn is_non_desktop(drm: &DrmDeviceFd, connector: connector::Handle) -> bool {
    drm.get_properties(connector)
        .ok()
        .and_then(|properties| {
            properties.into_iter().find_map(|(handle, value)| {
                let info = drm.get_property(handle).ok()?;
                (info.name().to_str() == Ok("non-desktop"))
                    .then(|| info.value_type().convert_value(value).as_boolean())
                    .flatten()
            })
        })
        .unwrap_or(false)
}

fn refresh_interval(refresh_millihertz: i32) -> Result<Duration> {
    let refresh = u64::try_from(refresh_millihertz)
        .ok()
        .filter(|refresh| *refresh > 0)
        .context("output refresh must be positive")?;
    Ok(Duration::from_nanos(1_000_000_000_000_u64 / refresh))
}

#[cfg(test)]
mod tests {
    use super::{ModeCandidate, OutputCandidate, choose_output_candidate, refresh_interval};
    use std::time::Duration;

    #[test]
    fn refresh_interval_uses_millihertz_without_assuming_sixty_hertz() {
        assert_eq!(
            refresh_interval(120_000).expect("valid refresh"),
            Duration::from_nanos(8_333_333)
        );
        assert!(refresh_interval(0).is_err());
    }

    #[test]
    fn connector_selection_prefers_internal_and_its_preferred_mode() {
        let candidates = [
            candidate("HDMI-A-2", &[false]),
            candidate("eDP-2", &[false]),
            candidate("eDP-1", &[false, true]),
        ];
        assert_eq!(choose_output_candidate(&candidates), Some((2, 1)));
    }

    #[test]
    fn connector_selection_uses_stable_name_order_and_skips_modeless_outputs() {
        let candidates = [
            candidate("HDMI-A-2", &[]),
            candidate("HDMI-A-1", &[false]),
            candidate("DP-1", &[false]),
        ];
        assert_eq!(choose_output_candidate(&candidates), Some((2, 0)));
    }

    fn candidate(name: &str, preferred_modes: &[bool]) -> OutputCandidate {
        OutputCandidate {
            name: name.to_owned(),
            modes: preferred_modes
                .iter()
                .map(|preferred| ModeCandidate {
                    preferred: *preferred,
                })
                .collect(),
        }
    }
}
