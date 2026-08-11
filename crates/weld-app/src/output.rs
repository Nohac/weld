//! Application-facing output geometry shared by presentation plugins.

use bevy::{
    ecs::resource::Resource,
    math::{UVec2, Vec2},
};
use weld_core::surface::Extent;

#[derive(Resource, Clone, Copy, Debug, PartialEq)]
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

    pub(crate) fn update(&mut self, extent: Extent, scale_factor: f64) {
        *self = Self::new(extent, scale_factor);
    }
}

fn valid_scale_factor(scale_factor: f64) -> f32 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor as f32
    } else {
        1.0
    }
}
