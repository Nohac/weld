//! Supplies native raster dimensions to screen-space surface snapping.

use bevy::{
    asset::Assets,
    ecs::{
        change_detection::DetectChangesMut,
        system::{Query, Res},
    },
    math::{Affine2, Vec2, Vec4},
    ui::{ComputedNode, UiGlobalTransform},
    ui_render::ui_material::MaterialNode,
};

use super::SurfaceUiMaterial;
use crate::pixel_snap::PixelSnapSize;

pub(super) fn prepare(
    materials: Res<Assets<SurfaceUiMaterial>>,
    mut surfaces: Query<(
        &MaterialNode<SurfaceUiMaterial>,
        &ComputedNode,
        &UiGlobalTransform,
        &mut PixelSnapSize,
    )>,
) {
    for (handle, node, transform, mut target) in &mut surfaces {
        let size = materials.get(&handle.0).and_then(|material| {
            native_extent(material.parameters.source_rect, node.size(), **transform)
        });
        target.set_if_neq(PixelSnapSize(size));
    }
}

fn native_extent(source: Vec4, size: Vec2, transform: Affine2) -> Option<Vec2> {
    let matrix = transform.matrix2;
    let unit =
        matrix.x_axis.abs_diff_eq(Vec2::X, 0.0001) && matrix.y_axis.abs_diff_eq(Vec2::Y, 0.0001);
    let native = Vec2::new(source.z, source.w);
    // Fractional-scale clients round their buffer extent independently of UI
    // layout. Reconcile at most one pixel when presenting an integral texel crop.
    (unit
        && source.is_finite()
        && source.abs_diff_eq(source.round(), 0.001)
        && native.min_element() > 0.0
        && (native - size).abs().max_element() <= 1.001)
        .then_some(native)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_extent_repairs_rounding_but_preserves_scaled_and_fractional_crops() {
        let source = Vec4::new(0., 0., 698., 1298.);
        let size = Vec2::new(697.5, 1297.5);
        assert_eq!(
            native_extent(source, size, Affine2::IDENTITY),
            Some(Vec2::new(698., 1298.))
        );
        assert_eq!(native_extent(source, size * 0.5, Affine2::IDENTITY), None);
        assert_eq!(
            native_extent(source, size, Affine2::from_scale(Vec2::splat(1.5))),
            None
        );
        assert_eq!(
            native_extent(Vec4::new(0.25, 0., 698., 1298.), size, Affine2::IDENTITY),
            None
        );
    }
}
