//! Application-facing output state and composition-camera ownership.

use bevy::{
    ecs::{component::Component, entity::Entity},
    math::{UVec2, Vec2},
};
use weld_core::surface::Extent;

/// Stable application-side identity for an output.
///
/// This identity is not yet correlated with the native output descriptors in
/// `weld-core`; the current host exposes one primary output to the application.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OutputId(u64);

impl OutputId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// An output available to application policy.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[require(OutputPosition)]
pub struct WeldOutput {
    pub id: OutputId,
}

/// Marks the output used by untargeted UI and single-output host operations.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrimaryOutput;

/// Physical dimensions and logical scale of one output.
///
/// Window geometry assigned to this output is expressed in its local logical
/// coordinate space. [`OutputPosition`] locates that space in the wider output
/// topology without affecting local window placement.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct OutputGeometry {
    physical_size: UVec2,
    scale_factor: f32,
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
