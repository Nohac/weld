//! Carries a presenter's rounded content boundary into each surface layer's draw.

use bevy::{
    asset::Assets,
    ecs::{
        component::Component,
        entity::Entity,
        hierarchy::ChildOf,
        query::With,
        system::{Query, ResMut},
    },
    math::{Vec2, Vec4},
    render::render_resource::ShaderType,
    ui::{CalculatedClip, ComputedNode, Node, UiGlobalTransform},
    ui_render::ui_material::MaterialNode,
};

use super::{MaterialSelectorRegistry, SurfaceUiMaterial};

/// Uses this container's laid-out bounds and radii to clip descendant surface layers.
/// Each layer inherits its nearest marked container; ordinary overflow clips still apply.
#[derive(Component, Clone, Copy, Debug, Default)]
#[require(Node)]
pub struct RoundedSurfaceClip;

#[derive(Clone, Copy, Debug, PartialEq, ShaderType)]
pub(super) struct ClipParameters {
    // The xyz rows map layer UVs into clip-local physical pixels. Each w stores
    // the viewport's half-extent; a negative x.w selects the layer's own shape.
    pub x: Vec4,
    pub y: Vec4,
    pub radii: Vec4,
    pub radii_y: Vec4,
}

impl Default for ClipParameters {
    fn default() -> Self {
        Self {
            x: Vec4::new(0.0, 0.0, 0.0, -1.0),
            y: Vec4::ZERO,
            radii: Vec4::ZERO,
            radii_y: Vec4::ZERO,
        }
    }
}

impl ClipParameters {
    fn new(
        surface: &ComputedNode,
        transform: &UiGlobalTransform,
        clip: &ComputedNode,
        clip_transform: &UiGlobalTransform,
    ) -> Self {
        let Some(inverse) = clip_transform.try_inverse() else {
            return Self {
                x: Vec4::ZERO,
                y: Vec4::ZERO,
                radii: Vec4::ZERO,
                radii_y: Vec4::ZERO,
            };
        };
        let relative = inverse * **transform;
        let x_axis = relative.transform_vector2(Vec2::new(surface.size().x, 0.0));
        let y_axis = relative.transform_vector2(Vec2::new(0.0, surface.size().y));
        let origin = relative.transform_point2(-0.5 * surface.size());
        let radii: [[f32; 4]; 2] = clip.border_radius().into();
        Self {
            x: Vec4::new(x_axis.x, y_axis.x, origin.x, clip.size().x * 0.5),
            y: Vec4::new(x_axis.y, y_axis.y, origin.y, clip.size().y * 0.5),
            radii: Vec4::from_array(radii[0]),
            radii_y: Vec4::from_array(radii[1]),
        }
    }
}

type SurfaceClipNodes<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static MaterialNode<SurfaceUiMaterial>,
        &'static ComputedNode,
        &'static UiGlobalTransform,
        Option<&'static mut CalculatedClip>,
    ),
>;

pub(super) fn sync(
    mut surfaces: SurfaceClipNodes,
    parents: Query<&ChildOf>,
    clips: Query<(&ComputedNode, &UiGlobalTransform), With<RoundedSurfaceClip>>,
    mut materials: ResMut<Assets<SurfaceUiMaterial>>,
    mut selectors: ResMut<MaterialSelectorRegistry>,
) {
    for (entity, handle, node, transform, clip) in &mut surfaces {
        if let Some(mut clip) = clip {
            let expected = crate::pixel_snap::aligned_clip(&clip);
            if *clip != expected {
                *clip = expected;
            }
        }
        let mut ancestor = entity;
        let mut expected = ClipParameters::default();
        while let Ok(parent) = parents.get(ancestor) {
            ancestor = parent.parent();
            if let Ok((clip, clip_transform)) = clips.get(ancestor) {
                expected = ClipParameters::new(node, transform, clip, clip_transform);
                break;
            }
        }
        if materials
            .get(&handle.0)
            .is_some_and(|material| material.parameters.clip != expected)
            && let Some(mut material) = materials.get_mut(&handle.0)
        {
            material.parameters.clip = expected;
            if let Some(selector) = selectors.0.get_mut(&handle.0.id()) {
                selector.parameters = material.parameters.into();
            }
        }
    }
}
