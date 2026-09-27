//! Geometry calculation has no client-buffer or configure ownership.

use bevy::{
    ecs::{component::Component, entity::Entity, world::World},
    math::Vec2,
};
use weld_window::WindowGeometry;

use crate::{SplitAxis, TileContainer};

#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayoutRect(pub WindowGeometry);

pub(crate) fn arrange(world: &mut World, entity: Entity, rect: WindowGeometry, gap: f32) -> bool {
    let mut changed = world
        .get::<LayoutRect>(entity)
        .is_none_or(|old| old.0 != rect);
    let Ok(mut target) = world.get_entity_mut(entity) else {
        return false;
    };
    if changed {
        target.insert(LayoutRect(rect));
    }
    if let Some(mut geometry) = world.get_mut::<WindowGeometry>(entity) {
        if *geometry != rect {
            *geometry = rect;
            changed = true;
        }
        return changed;
    }
    let Some(container) = world.get::<TileContainer>(entity) else {
        return changed;
    };
    let axis = container.axis;
    let children = container.children.clone();
    let weights: Vec<_> = children.iter().map(|child| child.weight).collect();
    for (child, child_rect) in children.iter().zip(partition(rect, axis, &weights, gap)) {
        changed |= arrange(world, child.entity, child_rect, gap);
    }
    changed
}

fn partition(
    rect: WindowGeometry,
    axis: SplitAxis,
    weights: &[f32],
    gap: f32,
) -> Vec<WindowGeometry> {
    if weights.is_empty() {
        return Vec::new();
    }
    let extent = match axis {
        SplitAxis::Horizontal => rect.size.x,
        SplitAxis::Vertical => rect.size.y,
    };
    let separators = weights.len().saturating_sub(1) as f32;
    let gap = gap.min(extent / (2.0 * separators.max(1.0)));
    let available = (extent - separators * gap).max(0.0);
    let total: f32 = weights.iter().sum();
    let mut accumulated = 0.0;
    let mut start = 0.0;
    weights
        .iter()
        .enumerate()
        .map(|(index, weight)| {
            accumulated += weight;
            let end = if index + 1 == weights.len() {
                extent
            } else {
                (available * accumulated / total)
                    .round()
                    .clamp(0.0, available)
                    + index as f32 * gap
            };
            let (offset, size) = match axis {
                SplitAxis::Horizontal => (
                    Vec2::new(start, 0.0),
                    Vec2::new((end - start).max(0.0), rect.size.y),
                ),
                SplitAxis::Vertical => (
                    Vec2::new(0.0, start),
                    Vec2::new(rect.size.x, (end - start).max(0.0)),
                ),
            };
            start = end + gap;
            WindowGeometry {
                position: rect.position + offset,
                size,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn odd_extents_and_large_gaps_stay_inside_the_parent() {
        for extent in [0.0, 0.5, 0.75, 1.0, 1.5, 2.5, 3.0, 801.0] {
            let parent = WindowGeometry {
                position: Vec2::ZERO,
                size: Vec2::new(extent, 50.0),
            };
            let parts = partition(parent, SplitAxis::Horizontal, &[1.0, 2.0, 1.0], 100.0);
            for part in &parts {
                assert!(part.position.x >= 0.0);
                assert!(part.size.x >= 0.0);
                assert!(part.position.x + part.size.x <= extent + 0.001);
            }
            let last = parts.last().expect("three partitions");
            assert!((last.position.x + last.size.x - extent).abs() < 0.001);
        }
    }
}
