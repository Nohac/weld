//! Paint-only overlap for compositor frame borders.

use bevy::{
    app::{App, PostUpdate},
    ecs::{
        change_detection::{DetectChanges, DetectChangesMut, Ref},
        component::Component,
        entity::Entity,
        lifecycle::RemovedComponents,
        schedule::IntoScheduleConfigs,
        system::{Query, ResMut},
        template::FromTemplate,
    },
    math::Vec2,
    render::{Extract, ExtractSchedule, RenderApp},
    sprite::BorderRect,
    ui::{ComputedNode, ComputedStackIndex, UiSystems},
    ui_render::{
        ExtractedUiItem, ExtractedUiNodes, NodeType, RenderUiSystems, extract_uinode_borders,
        stack_z_offsets,
    },
};

/// Extends each existing solid border edge inward by one physical pixel during rendering.
///
/// The referenced entity owns the frame's backing colour. The border paints above
/// that backing and below its children, letting the client cover the overlap.
#[derive(Component, Clone, Copy, Debug, FromTemplate)]
pub struct BorderUnderlap(pub Entity);

pub(crate) fn install(app: &mut App) {
    app.add_systems(PostUpdate, invalidate_border_paint.after(UiSystems::Stack));
    if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
        render_app.add_systems(
            ExtractSchedule,
            extend_border_paint
                .in_set(RenderUiSystems::ExtractBorders)
                .after(extract_uinode_borders),
        );
    }
}

fn invalidate_border_paint(
    mut frames: Query<(&mut ComputedNode, Ref<BorderUnderlap>)>,
    backgrounds: Query<Ref<ComputedStackIndex>>,
    mut removed: RemovedComponents<BorderUnderlap>,
    mut ordinary: Query<&mut ComputedNode, bevy::ecs::query::Without<BorderUnderlap>>,
) {
    for (mut node, underlap) in &mut frames {
        if underlap.is_changed()
            || backgrounds
                .get(underlap.0)
                .is_ok_and(|stack| stack.is_changed())
        {
            node.set_changed();
        }
    }
    for entity in removed.read() {
        if let Ok(mut node) = ordinary.get_mut(entity) {
            node.set_changed();
        }
    }
}

fn extend_border_paint(
    frames: Extract<Query<(&BorderUnderlap, &ComputedNode)>>,
    backgrounds: Extract<Query<&ComputedStackIndex>>,
    mut nodes: ResMut<ExtractedUiNodes>,
) {
    let ExtractedUiNodes { uinodes, changed } = &mut *nodes;
    for entity in changed.iter() {
        let Some((_, items)) = uinodes.get_mut(entity) else {
            continue;
        };
        for node in items.values_mut() {
            if let Ok((underlap, computed)) = frames.get(entity.id())
                && let Ok(stack) = backgrounds.get(underlap.0)
                && let ExtractedUiItem::Node {
                    border,
                    rect,
                    node_type: NodeType::Border(_),
                    ..
                } = &mut node.item
                && rect.size() == computed.size()
            {
                let scale = Vec2::new(
                    node.transform.matrix2.x_axis.length(),
                    node.transform.matrix2.y_axis.length(),
                );
                if !scale.is_finite() || scale.min_element() <= 0.0 {
                    continue;
                }
                *border = computed.border();
                extend_edges(border, scale.recip());
                node.z_order = stack.0 as f32 + stack_z_offsets::BORDER;
            }
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
