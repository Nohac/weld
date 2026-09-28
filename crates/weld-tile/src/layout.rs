//! Weighted layout and change-aware geometry publication.

use bevy::{
    ecs::{
        component::Component,
        entity::Entity,
        event::Event,
        message::MessageWriter,
        observer::On,
        query::{Changed, With},
        resource::Resource,
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    math::Vec2,
    window::RequestRedraw,
};
use weld_window::WindowGeometry;

use crate::{SplitAxis, TileContainer, TileParent, TileSettings, TileState};

#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct LayoutRect(pub WindowGeometry);

#[derive(Resource, Default)]
pub(crate) struct LayoutDirty(pub bool);

#[derive(Event)]
pub(crate) struct LayoutRequested;

type ChangedTileGeometry = (With<TileParent>, Changed<WindowGeometry>);

pub(crate) fn request_layout(
    changed: Query<(&WindowGeometry, &LayoutRect), ChangedTileGeometry>,
    mut dirty: ResMut<LayoutDirty>,
    mut commands: Commands,
) {
    if changed
        .iter()
        .any(|(geometry, applied)| *geometry != applied.0)
    {
        dirty.0 = true;
    }
    if dirty.0 {
        commands.trigger(LayoutRequested);
    }
}

#[derive(SystemParam)]
pub(crate) struct Layout<'w, 's> {
    state: Res<'w, TileState>,
    settings: Res<'w, TileSettings>,
    dirty: ResMut<'w, LayoutDirty>,
    containers: Query<'w, 's, &'static TileContainer>,
    rectangles: Query<'w, 's, &'static mut LayoutRect>,
    windows: Query<'w, 's, &'static mut WindowGeometry, With<TileParent>>,
    redraw: MessageWriter<'w, RequestRedraw>,
}

pub(crate) fn apply_layout(_event: On<LayoutRequested>, mut layout: Layout) {
    if !layout.dirty.0 {
        return;
    }
    let Some(root) = layout.state.root else {
        return;
    };
    let Ok(rect) = layout.rectangles.get(root).map(|rect| rect.0) else {
        return;
    };
    let gap = f32::from(layout.settings.inner_gap);
    let Layout {
        containers,
        rectangles,
        windows,
        redraw,
        ..
    } = &mut layout;
    if arrange(root, rect, gap, containers, rectangles, windows) {
        redraw.write(RequestRedraw);
    }
    layout.dirty.0 = false;
}

fn arrange(
    entity: Entity,
    rect: WindowGeometry,
    gap: f32,
    containers: &Query<&TileContainer>,
    rectangles: &mut Query<&mut LayoutRect>,
    windows: &mut Query<&mut WindowGeometry, With<TileParent>>,
) -> bool {
    let mut changed = false;
    if let Ok(mut old) = rectangles.get_mut(entity)
        && old.0 != rect
    {
        old.0 = rect;
        changed = true;
    }
    if let Ok(mut geometry) = windows.get_mut(entity) {
        if *geometry != rect {
            *geometry = rect;
            changed = true;
        }
        return changed;
    }
    let Ok(container) = containers.get(entity) else {
        return changed;
    };
    let weights = container.children.iter().map(|child| child.weight);
    for (child, child_rect) in
        container
            .children
            .iter()
            .zip(partition(rect, container.axis, weights, gap))
    {
        changed |= arrange(
            child.entity,
            child_rect,
            gap,
            containers,
            rectangles,
            windows,
        );
    }
    changed
}

fn partition(
    rect: WindowGeometry,
    axis: SplitAxis,
    weights: impl ExactSizeIterator<Item = f32> + Clone,
    gap: f32,
) -> impl Iterator<Item = WindowGeometry> {
    let count = weights.len();
    let extent = match axis {
        SplitAxis::Horizontal => rect.size.x,
        SplitAxis::Vertical => rect.size.y,
    };
    let separators = count.saturating_sub(1) as f32;
    let gap = gap.min(extent / (2.0 * separators.max(1.0)));
    let available = (extent - separators * gap).max(0.0);
    let total: f32 = weights.clone().sum();
    let mut accumulated = 0.0;
    let mut start = 0.0;
    weights.enumerate().map(move |(index, weight)| {
        accumulated += weight;
        let end = if index + 1 == count {
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
            let parts = partition(
                parent,
                SplitAxis::Horizontal,
                [1.0, 2.0, 1.0].into_iter(),
                100.0,
            )
            .collect::<Vec<_>>();
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
