//! DRM connector discovery and deterministic startup layout.

use std::time::Duration;

use crate::{
    OutputConfiguration, OutputFootprintProvenance, OutputHead, OutputId, OutputPhysicalSize,
    OutputScale,
    server::{OutputDescriptor, OutputMetrics, ServerOutputDefinition},
    surface::{Extent, LogicalPoint},
};
use anyhow::{Context, Result};
use smithay::{
    backend::drm::DrmDeviceFd,
    output::{Mode as SmithayMode, PhysicalProperties},
    reexports::drm::control::{Device as ControlDevice, Mode, ModeTypeFlags, connector, crtc},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

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

pub(super) fn select_outputs(
    drm: &DrmDeviceFd,
    primary_scale: OutputScale,
) -> Result<Vec<SelectedOutput>> {
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
    connected.sort_by_key(|(connector, _)| {
        let name = connector_name(connector);
        let (external, _) = connector_order(&name);
        (external, name)
    });
    if connected.is_empty() {
        anyhow::bail!("no connected desktop connector with a usable CRTC and mode");
    }

    let mut discovered = connected
        .into_iter()
        .enumerate()
        .map(|(index, (connector, crtc))| {
            let mode_index = preferred_mode_index(
                connector
                    .modes()
                    .iter()
                    .map(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED)),
            )
            .context("selected DRM connector has no mode")?;
            let mode = connector.modes()[mode_index];
            let id = OutputId::new(index as u64 + 1);
            let name = connector_name(&connector);
            let physical_size = connector
                .size()
                .and_then(|(width, height)| OutputPhysicalSize::new(width, height));
            let mode_size = mode.size();
            let extent = Extent::new(u32::from(mode_size.0), u32::from(mode_size.1));
            let scale = if index == 0 {
                primary_scale
            } else {
                OutputScale::default()
            };
            let configuration = OutputConfiguration::new(
                id,
                extent,
                scale,
                LogicalPoint::ZERO,
                index == 0,
                physical_size,
            )?;
            let rate = weld_client::PresentationRate::try_from(u32::try_from(
                SmithayMode::from(mode).refresh,
            )?)
            .map_err(anyhow::Error::msg)?;
            let configuration = configuration.with_presentation_rate(rate);
            Ok::<_, anyhow::Error>((connector, crtc, mode, name, physical_size, configuration))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut configurations = discovered.iter().map(|entry| entry.5).collect::<Vec<_>>();
    center_primary_below_others(&mut configurations)?;

    discovered
        .drain(..)
        .zip(configurations)
        .map(
            |((connector, crtc, mode, name, physical_size, _), configuration)| {
                let id = configuration.id();
                let smithay_mode = SmithayMode::from(mode);
                let metrics = metrics_for_configuration(configuration, mode)?;
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
                    logical_position: (
                        logical_coordinate(configuration.position().x)?,
                        logical_coordinate(configuration.position().y)?,
                    ),
                    primary: configuration.is_primary(),
                };
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
            },
        )
        .collect()
}

pub(super) fn center_primary_below_others(
    configurations: &mut [OutputConfiguration],
) -> Result<()> {
    if configurations.is_empty() {
        return Ok(());
    }
    let primary_index = configurations
        .iter()
        .position(|output| output.is_primary())
        .context("startup output layout has no primary output")?;
    if configurations.len() == 1 {
        configurations[primary_index] = configurations[primary_index]
            .with_position(LogicalPoint::ZERO)?
            .with_footprint_position(0.0, 0.0)?;
        return Ok(());
    }

    let primary = configurations[primary_index];
    let logical_row_width = configurations
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != primary_index)
        .map(|(_, output)| output.logical_width())
        .sum::<f64>();
    let logical_row_height = configurations
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != primary_index)
        .map(|(_, output)| output)
        .map(|output| output.logical_height())
        .fold(0.0, f64::max);
    let logical_width = logical_row_width.max(primary.logical_width());
    let mut logical_x = (logical_width - logical_row_width) / 2.0;
    for (index, output) in configurations.iter_mut().enumerate() {
        if index == primary_index {
            continue;
        }
        let position_x = topology_coordinate(logical_x)?;
        *output = output.with_position(LogicalPoint::new(position_x, 0.0))?;
        logical_x = f64::from(position_x) + output.logical_width();
    }
    configurations[primary_index] = primary.with_position(LogicalPoint::new(
        topology_coordinate((logical_width - primary.logical_width()) / 2.0)?,
        topology_coordinate(logical_row_height)?,
    ))?;

    let physical_row_width = configurations
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != primary_index)
        .map(|(_, output)| output)
        .map(|output| output.footprint().width_millimeters())
        .sum::<f64>();
    let physical_row_height = configurations
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != primary_index)
        .map(|(_, output)| output)
        .map(|output| output.footprint().height_millimeters())
        .fold(0.0, f64::max);
    let physical_width = physical_row_width.max(primary.footprint().width_millimeters());
    let mut physical_x = (physical_width - physical_row_width) / 2.0;
    for (index, output) in configurations.iter_mut().enumerate() {
        if index == primary_index {
            continue;
        }
        *output = output.with_footprint_position(physical_x, 0.0)?;
        physical_x += output.footprint().width_millimeters();
    }
    configurations[primary_index] = configurations[primary_index].with_footprint_position(
        (physical_width - primary.footprint().width_millimeters()) / 2.0,
        physical_row_height,
    )?;
    Ok(())
}

fn topology_coordinate(value: f64) -> Result<f32> {
    if !value.is_finite() || value < f64::from(f32::MIN) || value > f64::from(f32::MAX) {
        anyhow::bail!("output topology coordinate exceeds f32");
    }
    let coordinate = value as f32;
    Ok(if f64::from(coordinate) < value {
        coordinate.next_up()
    } else {
        coordinate
    })
}

pub(super) fn logical_coordinate(value: f32) -> Result<i32> {
    if !value.is_finite() || value < i32::MIN as f32 || value > i32::MAX as f32 {
        anyhow::bail!("output topology coordinate exceeds i32");
    }
    Ok(value.round() as i32)
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

pub(super) fn scale_matching_physical_density(
    target: OutputConfiguration,
    reference: OutputConfiguration,
) -> Option<OutputScale> {
    let target_footprint = target.footprint();
    let reference_footprint = reference.footprint();
    if target_footprint.provenance() != OutputFootprintProvenance::Measured
        || reference_footprint.provenance() != OutputFootprintProvenance::Measured
    {
        return None;
    }
    let target_pixels = f64::from(target.extent().width).hypot(f64::from(target.extent().height));
    let target_millimeters = target_footprint
        .width_millimeters()
        .hypot(target_footprint.height_millimeters());
    let reference_pixels =
        f64::from(reference.extent().width).hypot(f64::from(reference.extent().height));
    let reference_millimeters = reference_footprint
        .width_millimeters()
        .hypot(reference_footprint.height_millimeters());
    let target_density = target_pixels / target_millimeters;
    let reference_logical_density =
        reference_pixels / reference_millimeters / reference.scale().value();
    OutputScale::new(target_density / reference_logical_density).ok()
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

fn connector_order(name: &str) -> (bool, &str) {
    (!is_internal_connector(name), name)
}

fn preferred_mode_index(preferred: impl IntoIterator<Item = bool>) -> Option<usize> {
    let mut first = None;
    for (index, preferred) in preferred.into_iter().enumerate() {
        first.get_or_insert(index);
        if preferred {
            return Some(index);
        }
    }
    first
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
    use super::{
        center_primary_below_others, connector_order, preferred_mode_index, refresh_interval,
        scale_matching_physical_density,
    };
    use crate::{
        OutputConfiguration, OutputId, OutputLayout, OutputScale, OutputTopology,
        input::{InputDelta, InputPosition},
        surface::{Extent, LogicalPoint},
    };
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
    fn connector_order_is_internal_first_then_stable_by_name() {
        let mut names = ["HDMI-A-2", "eDP-2", "DP-1", "eDP-1"];
        names.sort_by_key(|name| connector_order(name));
        assert_eq!(names, ["eDP-1", "eDP-2", "DP-1", "HDMI-A-2"]);
    }

    #[test]
    fn preferred_mode_selection_falls_back_to_the_first_usable_mode() {
        assert_eq!(preferred_mode_index([false, true, false]), Some(1));
        assert_eq!(preferred_mode_index([false, false]), Some(0));
        assert_eq!(preferred_mode_index([]), None);
    }

    #[test]
    fn startup_layout_centers_the_primary_below_the_output_row() {
        let mut outputs = [
            configuration(2, 1_920, 1_080, 1.0, false),
            configuration(1, 2_240, 1_400, 1.25, true),
        ];

        center_primary_below_others(&mut outputs).expect("valid startup layout");

        assert_eq!(outputs[0].position(), LogicalPoint::new(0.0, 0.0));
        assert_eq!(outputs[1].position(), LogicalPoint::new(64.0, 1_080.0));
        assert_eq!(outputs[0].footprint().y_millimeters(), 0.0);
        assert_eq!(outputs[1].footprint().y_millimeters(), 90.0);
    }

    #[test]
    fn non_integral_scale_keeps_the_centered_seam_bidirectional() {
        let mut outputs = [
            configuration(2, 1_920, 1_080, 1.75, false),
            configuration(1, 2_240, 1_400, 1.25, true),
        ];
        center_primary_below_others(&mut outputs).expect("valid non-integral layout");
        let seam = f64::from(outputs[1].position().y);
        let topology = OutputTopology::new(
            OutputLayout::new(1, outputs.to_vec()).expect("non-overlapping layout"),
        );

        let lower = topology.move_pointer(
            InputPosition::new(500.0, seam - 1.0),
            InputDelta::new(0.0, 2.0),
        );
        assert_eq!(topology.output_at(lower), Some(OutputId::new(1)));
        let upper = topology.move_pointer(lower, InputDelta::new(0.0, -2.0));
        assert_eq!(topology.output_at(upper), Some(OutputId::new(2)));
    }

    #[test]
    fn physical_density_matching_preserves_the_reference_logical_pixel_size() {
        let target = configuration_with_physical_size(1, 2_560, 1_600, 1.0, true, 320, 200);
        let reference = configuration_with_physical_size(2, 1_920, 1_080, 1.0, false, 480, 270);

        let scale = scale_matching_physical_density(target, reference)
            .expect("both outputs have measured physical dimensions");

        assert!((scale.value() - 2.0).abs() < 0.001);
    }

    fn configuration(
        id: u64,
        width: u32,
        height: u32,
        scale: f64,
        primary: bool,
    ) -> OutputConfiguration {
        configuration_with_physical_size(id, width, height, scale, primary, 160, 90)
    }

    fn configuration_with_physical_size(
        id: u64,
        width: u32,
        height: u32,
        scale: f64,
        primary: bool,
        physical_width: u32,
        physical_height: u32,
    ) -> OutputConfiguration {
        OutputConfiguration::new(
            OutputId::new(id),
            Extent::new(width, height),
            OutputScale::new(scale).expect("valid scale"),
            LogicalPoint::ZERO,
            primary,
            crate::OutputPhysicalSize::new(physical_width, physical_height),
        )
        .expect("valid output")
    }
}
