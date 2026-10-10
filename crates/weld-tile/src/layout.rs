//! Weighted layout and change-aware geometry publication.

use bevy::{
    ecs::{
        change_detection::DetectChanges,
        component::Component,
        entity::Entity,
        event::Event,
        message::MessageWriter,
        observer::On,
        query::{Changed, With, Without},
        resource::Resource,
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    math::Vec2,
    window::RequestRedraw,
};
use weld_window::{WindowGeometry, fullscreen::WindowFullscreen};

use crate::{
    SplitAxis, TileContainer, TileFocusHistory, TileHeader, TileHeaders, TileLayout, TileParent,
    TilePresentationMetrics, TileSettings, TileWorkspace,
};

#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
/// Output-local rectangle assigned to a node by the tiler.
pub struct LayoutRect(pub WindowGeometry);

#[derive(Resource, Default)]
pub(crate) struct LayoutDirty(pub bool);

#[derive(Event)]
pub(crate) struct LayoutRequested;

type ChangedTileGeometry = (
    With<TileParent>,
    Without<WindowFullscreen>,
    Changed<WindowGeometry>,
);

pub(crate) fn request_layout(
    changed: Query<(&WindowGeometry, &LayoutRect), ChangedTileGeometry>,
    mut dirty: ResMut<LayoutDirty>,
    mut commands: Commands,
    history: Res<TileFocusHistory>,
    headers: Query<(&TileContainer, &TileHeaders)>,
    metrics: Res<TilePresentationMetrics>,
) {
    if metrics.is_changed() {
        dirty.0 = true;
    }
    if history.is_changed()
        && headers.iter().any(|(container, headers)| {
            let active = history.active_child(container);
            headers
                .0
                .iter()
                .any(|header| header.selected != (Some(header.child) == active))
        })
    {
        dirty.0 = true;
    }
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
    roots: Query<'w, 's, Entity, With<TileWorkspace>>,
    settings: Res<'w, TileSettings>,
    metrics: Res<'w, TilePresentationMetrics>,
    dirty: ResMut<'w, LayoutDirty>,
    containers: Query<'w, 's, &'static TileContainer>,
    history: Res<'w, TileFocusHistory>,
    geometry: GeometryWriter<'w, 's>,
    redraw: MessageWriter<'w, RequestRedraw>,
}

#[derive(SystemParam)]
struct GeometryWriter<'w, 's> {
    headers: Query<'w, 's, &'static mut TileHeaders>,
    rectangles: Query<'w, 's, &'static mut LayoutRect>,
    windows:
        Query<'w, 's, &'static mut WindowGeometry, (With<TileParent>, Without<WindowFullscreen>)>,
}

pub(crate) fn apply_layout(_event: On<LayoutRequested>, mut layout: Layout) {
    if !layout.dirty.0 {
        return;
    }
    let Layout {
        roots,
        containers,
        geometry,
        settings,
        metrics,
        history,
        redraw,
        ..
    } = &mut layout;
    for root in roots.iter() {
        let settings = LayoutMetrics {
            inner_gap: settings.inner_gap,
            header_height: metrics.header_height,
            group_border: metrics.group_border,
            outer_group_border: if metrics.hide_solo_group_border
                && crate::workspace::has_single_frame(root, containers)
            {
                0
            } else {
                metrics.group_border
            },
        };
        let Ok(rect) = geometry.rectangles.get(root).map(|rect| rect.0) else {
            continue;
        };
        if geometry.arrange(root, rect, containers, &settings, history, false) {
            redraw.write(RequestRedraw);
        }
    }
    layout.dirty.0 = false;
}

struct LayoutMetrics {
    inner_gap: u16,
    header_height: u16,
    group_border: u16,
    outer_group_border: u16,
}

impl GeometryWriter<'_, '_> {
    fn arrange(
        &mut self,
        entity: Entity,
        rect: WindowGeometry,
        containers: &Query<&TileContainer>,
        settings: &LayoutMetrics,
        history: &TileFocusHistory,
        grouped: bool,
    ) -> bool {
        let mut changed = false;
        if let Ok(mut old) = self.rectangles.get_mut(entity)
            && old.0 != rect
        {
            old.0 = rect;
            changed = true;
        }
        if let Ok(mut geometry) = self.windows.get_mut(entity) {
            if *geometry != rect {
                *geometry = rect;
                changed = true;
            }
            return changed;
        }
        let Ok(container) = containers.get(entity) else {
            return changed;
        };
        match container.layout {
            TileLayout::Split(axis) => {
                if let Ok(mut headers) = self.headers.get_mut(entity)
                    && !headers.0.is_empty()
                {
                    headers.0.clear();
                    changed = true;
                }
                for (child, child_rect) in container.children.iter().zip(partition(
                    rect,
                    axis,
                    container.children.iter().map(|child| child.weight),
                    if grouped {
                        f32::from(settings.group_border)
                    } else {
                        f32::from(settings.inner_gap)
                    },
                )) {
                    changed |= self.arrange(
                        child.entity,
                        child_rect,
                        containers,
                        settings,
                        history,
                        grouped,
                    );
                }
            }
            TileLayout::Tabbed | TileLayout::Stacked => {
                let border = if grouped {
                    0.0
                } else {
                    f32::from(settings.outer_group_border).min(rect.size.min_element() * 0.5)
                };
                let inner = WindowGeometry {
                    position: rect.position + Vec2::splat(border),
                    size: (rect.size - Vec2::splat(2.0 * border)).max(Vec2::ZERO),
                };
                let count = container.children.len();
                let rows = if container.layout == TileLayout::Tabbed {
                    1
                } else {
                    count
                };
                let height = f32::from(settings.header_height)
                    .min(inner.size.y / (rows.max(1) as f32 + 1.0));
                let offset = Vec2::new(0.0, height * rows as f32);
                let body = WindowGeometry {
                    position: inner.position + offset,
                    size: (inner.size - offset).max(Vec2::ZERO),
                };
                let active = history.active_child(container);
                let headers: Vec<_> = container
                    .children
                    .iter()
                    .enumerate()
                    .map(|(index, child)| {
                        let header = if container.layout == TileLayout::Tabbed {
                            let width = inner.size.x / count.max(1) as f32;
                            WindowGeometry {
                                position: inner.position + Vec2::new(width * index as f32, 0.0),
                                size: Vec2::new(width, height),
                            }
                        } else {
                            WindowGeometry {
                                position: inner.position + Vec2::new(0.0, height * index as f32),
                                size: Vec2::new(inner.size.x, height),
                            }
                        };
                        TileHeader {
                            child: child.entity,
                            geometry: header,
                            selected: active == Some(child.entity),
                        }
                    })
                    .collect();
                if let Ok(mut current) = self.headers.get_mut(entity)
                    && current.0 != headers
                {
                    current.0 = headers;
                    changed = true;
                }
                for child in &container.children {
                    changed |=
                        self.arrange(child.entity, body, containers, settings, history, true);
                }
            }
        }
        changed
    }
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
