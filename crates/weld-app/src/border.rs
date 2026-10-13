//! Paint-only overlap for compositor frame borders.

use bevy::{
    app::App,
    ecs::{
        component::Component,
        entity::Entity,
        schedule::IntoScheduleConfigs,
        system::{Query, ResMut},
        template::FromTemplate,
    },
    math::Vec2,
    render::{Extract, ExtractSchedule, RenderApp},
    sprite::BorderRect,
    ui::{ComputedNode, ComputedStackIndex},
    ui_render::{ExtractedUiItem, ExtractedUiNodes, NodeType, RenderUiSystems, stack_z_offsets},
};

/// Extends each existing solid border edge inward by one physical pixel during rendering.
///
/// The referenced entity owns the frame's backing colour. The border paints above
/// that backing and below its children, letting the client cover the overlap.
#[derive(Component, Clone, Copy, Debug, FromTemplate)]
pub struct BorderUnderlap(pub Entity);

pub(crate) fn install(app: &mut App) {
    if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
        render_app.add_systems(
            ExtractSchedule,
            extend_border_paint.after(RenderUiSystems::ExtractBorders),
        );
    }
}

fn extend_border_paint(
    frames: Extract<Query<(&BorderUnderlap, &ComputedNode)>>,
    backgrounds: Extract<Query<&ComputedStackIndex>>,
    mut nodes: ResMut<ExtractedUiNodes>,
) {
    for node in &mut nodes.uinodes {
        if let Ok((underlap, computed)) = frames.get(node.main_entity.id())
            && let Ok(stack) = backgrounds.get(underlap.0)
            && let ExtractedUiItem::Node {
                border,
                rect,
                node_type: NodeType::Border(_),
                ..
            } = &mut node.item
            && rect.size() == computed.size()
            && *border == computed.border()
        {
            let scale = Vec2::new(
                node.transform.matrix2.x_axis.length(),
                node.transform.matrix2.y_axis.length(),
            );
            if !scale.is_finite() || scale.min_element() <= 0.0 {
                continue;
            }
            extend_edges(border, scale.recip());
            node.z_order = stack.0 as f32 + stack_z_offsets::BORDER;
        }
    }
}

fn extend_edges(border: &mut BorderRect, pixel: Vec2) {
    for (width, overlap) in [
        (&mut border.min_inset.x, pixel.x),
        (&mut border.min_inset.y, pixel.y),
        (&mut border.max_inset.x, pixel.x),
        (&mut border.max_inset.y, pixel.y),
    ] {
        if *width > 0.0 {
            *width += overlap;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underlap_extends_only_existing_edges() {
        let mut border = BorderRect {
            min_inset: Vec2::new(3.75, 0.),
            max_inset: Vec2::new(2.5, 1.25),
        };
        extend_edges(&mut border, Vec2::ONE);
        assert_eq!(border.min_inset, Vec2::new(4.75, 0.));
        assert_eq!(border.max_inset, Vec2::new(3.5, 2.25));
    }
}
