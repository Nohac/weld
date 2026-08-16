//! Wayland output metrics and surface scale advertisement.

use anyhow::{Context, Result, bail};
use smithay::{
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::Transform,
    wayland::{
        compositor::{send_surface_state, with_states},
        fractional_scale::with_fractional_scale,
    },
};

use crate::{OutputId, OutputScale};

pub(crate) struct OutputDescriptor {
    pub(crate) name: String,
    pub(crate) physical_properties: PhysicalProperties,
}

/// One Wayland output global installed when the compositor server starts.
pub(crate) struct ServerOutputDefinition {
    pub(crate) id: OutputId,
    pub(crate) descriptor: OutputDescriptor,
    pub(crate) metrics: OutputMetrics,
    pub(crate) logical_position: (i32, i32),
    pub(crate) primary: bool,
}

impl OutputDescriptor {
    pub(crate) fn nested() -> Self {
        Self {
            name: "weld-nested".to_owned(),
            physical_properties: PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Weld".to_owned(),
                model: "Nested".to_owned(),
                serial_number: "development".to_owned(),
            },
        }
    }
}

/// Physical host extent plus the effective logical scale advertised to
/// clients.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OutputMetrics {
    physical_width: i32,
    physical_height: i32,
    refresh_millihertz: i32,
    scale_factor: f64,
}

impl OutputMetrics {
    pub(crate) fn new(
        physical_width: u32,
        physical_height: u32,
        scale: OutputScale,
    ) -> Result<Self> {
        if physical_width == 0 || physical_height == 0 {
            bail!("output dimensions must be nonzero");
        }
        let metrics = Self {
            physical_width: i32::try_from(physical_width).context("output width exceeds i32")?,
            physical_height: i32::try_from(physical_height).context("output height exceeds i32")?,
            refresh_millihertz: 60_000,
            scale_factor: scale.value(),
        };
        metrics.validate_logical_size()?;
        Ok(metrics)
    }

    pub(crate) fn with_refresh_millihertz(mut self, refresh_millihertz: i32) -> Result<Self> {
        if refresh_millihertz <= 0 {
            bail!("output refresh must be positive");
        }
        self.refresh_millihertz = refresh_millihertz;
        Ok(self)
    }

    pub(crate) fn with_scale(mut self, scale: OutputScale) -> Result<Self> {
        self.scale_factor = scale.value();
        self.validate_logical_size()?;
        Ok(self)
    }

    fn validate_logical_size(self) -> Result<()> {
        if f64::from(self.physical_width) / self.scale_factor < 1.0
            || f64::from(self.physical_height) / self.scale_factor < 1.0
        {
            bail!("output scale leaves less than one logical pixel on an axis");
        }
        Ok(())
    }

    pub(super) fn mode(self) -> OutputMode {
        OutputMode {
            size: (self.physical_width, self.physical_height).into(),
            refresh: self.refresh_millihertz,
        }
    }

    pub(super) fn scale(self) -> Scale {
        Scale::Fractional(self.scale_factor)
    }

    pub(crate) const fn scale_factor(self) -> f64 {
        self.scale_factor
    }

    pub(crate) const fn physical_width(self) -> u32 {
        self.physical_width as u32
    }

    pub(crate) const fn physical_height(self) -> u32 {
        self.physical_height as u32
    }
}

pub(super) fn install_output_metrics(
    output: &Output,
    previous: OutputMetrics,
    next: OutputMetrics,
) {
    let previous_mode = previous.mode();
    let next_mode = next.mode();
    output.change_current_state(Some(next_mode), None, Some(next.scale()), None);
    output.set_preferred(next_mode);
    if previous_mode != next_mode {
        output.delete_mode(previous_mode);
    }
}

pub(super) fn send_preferred_surface_scale(output: &Output, surface: &WlSurface) {
    let output_scale = output.current_scale();
    with_states(surface, |states| {
        send_surface_state(
            surface,
            states,
            output_scale.integer_scale(),
            Transform::Normal,
        );
        with_fractional_scale(states, |fractional_scale| {
            fractional_scale.set_preferred_scale(output_scale.fractional_scale());
        });
    });
}

#[cfg(test)]
mod tests {
    use smithay::{
        output::{Output, PhysicalProperties, Subpixel},
        utils::Transform,
    };

    use super::*;

    fn scale(value: f64) -> OutputScale {
        OutputScale::new(value).expect("valid output scale")
    }

    #[test]
    fn nested_output_metrics_preserve_physical_mode_and_fractional_scale() {
        let metrics = OutputMetrics::new(1200, 800, scale(1.25)).expect("valid output metrics");
        assert_eq!(metrics.mode().size, (1200, 800).into());
        assert_eq!(metrics.scale().fractional_scale(), 1.25);
        assert_eq!(metrics.scale().integer_scale(), 2);
        assert_eq!(metrics.scale_factor(), 1.25);
    }

    #[test]
    fn nested_output_metrics_reject_zero_dimensions() {
        assert!(OutputMetrics::new(0, 800, OutputScale::default()).is_err());
        assert!(OutputMetrics::new(800, 600, scale(1_000.0)).is_err());
    }

    #[test]
    fn replacing_nested_metrics_keeps_one_current_preferred_mode() {
        let initial = OutputMetrics::new(1200, 800, scale(1.25)).expect("valid metrics");
        let resized = OutputMetrics::new(1300, 900, scale(1.25)).expect("valid metrics");
        let resized_again = OutputMetrics::new(1400, 1000, scale(1.25)).expect("valid metrics");
        let scale_only = OutputMetrics::new(1400, 1000, scale(1.5)).expect("valid metrics");
        let output = Output::new(
            "test".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Weld".to_owned(),
                model: "Test".to_owned(),
                serial_number: "test".to_owned(),
            },
        );
        output.change_current_state(
            Some(initial.mode()),
            Some(Transform::Normal),
            Some(initial.scale()),
            Some((0, 0).into()),
        );
        output.set_preferred(initial.mode());

        install_output_metrics(&output, initial, resized);
        install_output_metrics(&output, resized, resized_again);
        assert_eq!(output.modes(), [resized_again.mode()]);
        assert_eq!(output.current_mode(), Some(resized_again.mode()));
        assert_eq!(output.preferred_mode(), Some(resized_again.mode()));

        install_output_metrics(&output, resized_again, scale_only);
        assert_eq!(output.modes(), [scale_only.mode()]);
        assert_eq!(output.current_mode(), Some(scale_only.mode()));
        assert_eq!(output.preferred_mode(), Some(scale_only.mode()));
    }

    #[test]
    fn replacing_scale_preserves_the_physical_mode_and_refresh() {
        let initial = OutputMetrics::new(1920, 1080, scale(1.0))
            .and_then(|metrics| metrics.with_refresh_millihertz(144_000))
            .expect("valid metrics");
        let scaled = initial.with_scale(scale(1.5)).expect("valid scale");

        assert_eq!(scaled.mode(), initial.mode());
        assert_eq!(scaled.scale_factor(), 1.5);
    }
}
