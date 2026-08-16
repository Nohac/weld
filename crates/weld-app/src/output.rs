//! Application-facing output state and composition-camera ownership.

use bevy::{
    ecs::{component::Component, entity::Entity},
    math::{UVec2, Vec2},
};
use weld_core::{OutputConfiguration, OutputHead, surface::Extent};
pub use weld_core::{OutputFootprintProvenance, OutputId};

/// An output available to application policy.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[require(OutputPosition)]
pub struct WeldOutput {
    pub id: OutputId,
}

/// Stable connector facts for one output.
///
/// Physical measurements come from display metadata and may be absent or
/// inaccurate. [`OutputPlacement`] records the actual footprint policy chose;
/// consumers should not independently reconstruct placement from this data.
#[derive(Component, Clone, Debug, Eq, PartialEq)]
pub struct OutputInfo {
    name: String,
    physical_size_millimeters: Option<UVec2>,
}

impl OutputInfo {
    pub(crate) fn from_head(head: &OutputHead) -> Self {
        Self {
            name: head.name().to_owned(),
            physical_size_millimeters: head
                .physical_size()
                .map(|size| UVec2::new(size.width_millimeters(), size.height_millimeters())),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn physical_size_millimeters(&self) -> Option<UVec2> {
        self.physical_size_millimeters
    }

    /// Calculates horizontal and vertical pixels per inch from advisory
    /// physical measurements.
    pub fn pixels_per_inch(&self, physical_size: UVec2) -> Option<Vec2> {
        let millimeters = self.physical_size_millimeters?.as_vec2();
        Some(physical_size.as_vec2() * 25.4 / millimeters)
    }
}

/// Marks the output used by untargeted UI and single-output host operations.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrimaryOutput;

/// Pixel dimensions and logical scale of one output.
///
/// Window geometry assigned to this output is expressed in its local logical
/// coordinate space. [`OutputPosition`] locates that space in the wider output
/// topology without affecting local window placement.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct OutputGeometry {
    physical_size: UVec2,
    scale_factor: f32,
}

/// Scale-independent placement used for physical output adjacency.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct OutputPlacement {
    position_millimeters: Vec2,
    size_millimeters: Vec2,
    provenance: OutputFootprintProvenance,
}

impl OutputPlacement {
    pub(crate) fn from_configuration(configuration: OutputConfiguration) -> Self {
        let footprint = configuration.footprint();
        Self {
            position_millimeters: Vec2::new(
                footprint.x_millimeters() as f32,
                footprint.y_millimeters() as f32,
            ),
            size_millimeters: Vec2::new(
                footprint.width_millimeters() as f32,
                footprint.height_millimeters() as f32,
            ),
            provenance: footprint.provenance(),
        }
    }

    pub const fn position_millimeters(self) -> Vec2 {
        self.position_millimeters
    }

    pub const fn size_millimeters(self) -> Vec2 {
        self.size_millimeters
    }

    pub const fn provenance(self) -> OutputFootprintProvenance {
        self.provenance
    }
}

impl OutputGeometry {
    pub fn new(extent: Extent, scale_factor: f64) -> Self {
        Self {
            physical_size: UVec2::new(extent.width, extent.height),
            scale_factor: valid_scale_factor(scale_factor),
        }
    }

    pub fn physical_size(self) -> UVec2 {
        self.physical_size
    }

    pub fn from_physical(physical_size: UVec2, scale_factor: f64) -> Self {
        Self::new(Extent::new(physical_size.x, physical_size.y), scale_factor)
    }

    pub fn scale_factor(self) -> f32 {
        self.scale_factor
    }

    pub fn logical_size(self) -> Vec2 {
        self.physical_size.as_vec2() / self.scale_factor
    }
}

/// Logical origin of an output in the compositor-wide topology.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct OutputPosition(pub Vec2);

/// Associates a shell-owned composition camera with the output it renders.
///
/// Weld owns the one-to-one binding. Replacing it from application policy
/// transfers the relationship but does not retarget the shell's native render
/// destination.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = OutputCompositionCamera)]
pub struct RendersOutput(pub Entity);

/// The composition camera currently associated with an output.
///
/// Despawning the output also despawns its related composition camera.
#[derive(Component, Debug)]
#[relationship_target(relationship = RendersOutput, linked_spawn)]
pub struct OutputCompositionCamera(Entity);

impl OutputCompositionCamera {
    /// Returns the related camera, or `None` while no camera is associated.
    pub fn entity(&self) -> Option<Entity> {
        (self.0 != Entity::PLACEHOLDER).then_some(self.0)
    }
}

fn valid_scale_factor(scale_factor: f64) -> f32 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor as f32
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_core::OutputPhysicalSize;

    #[test]
    fn output_info_calculates_dpi_only_from_complete_physical_metadata() {
        let measured = OutputInfo::from_head(&OutputHead::new(
            OutputId::new(1),
            "DP-1",
            OutputPhysicalSize::new(344, 194),
        ));
        let dpi = measured
            .pixels_per_inch(UVec2::new(1_920, 1_080))
            .expect("complete dimensions should produce DPI");
        assert!((dpi.x - 141.77).abs() < 0.01);
        assert!((dpi.y - 141.40).abs() < 0.01);

        let missing = OutputInfo::from_head(&OutputHead::new(
            OutputId::new(2),
            "DP-2",
            OutputPhysicalSize::new(0, 194),
        ));
        assert_eq!(missing.physical_size_millimeters(), None);
        assert_eq!(missing.pixels_per_inch(UVec2::new(1_920, 1_080)), None);
    }
}
