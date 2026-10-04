//! Split edges inside a shared frame use the ordinary content-border palette.

use super::{HeaderTree, Invalidations};
use bevy::{
    ecs::{
        change_detection::DetectChanges,
        component::Component,
        entity::Entity,
        message::MessageWriter,
        query::QueryData,
        system::{Commands, Query},
        template::template,
    },
    math::Vec2,
    picking::Pickable,
    prelude::{
        BackgroundColor, Color, GlobalZIndex, Node, PositionType, Scene, UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, bsn},
    window::RequestRedraw,
};
use std::collections::HashMap;
use weld_ssd::WindowFrameColors;
use weld_tile::{SplitAxis, TileGeometry, TileLayout};
use weld_window::WindowGeometry;

#[derive(Component, Clone, PartialEq)]
pub(super) struct PaneDivider {
    container: Entity,
    leading: Entity,
    geometry: WindowGeometry,
    camera: Entity,
    color: Color,
    z: i32,
}

fn node(rect: WindowGeometry) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(rect.position.x),
        top: px(rect.position.y),
        width: px(rect.size.x),
        height: px(rect.size.y),
        ..Default::default()
    }
}

fn scene(divider: PaneDivider) -> impl Scene {
    let rect = node(divider.geometry);
    bsn! {
        template(move |_| Ok(rect.clone()))
        template(move |_| Ok(UiTargetCamera(divider.camera)))
        BackgroundColor({divider.color})
        GlobalZIndex({divider.z})
        template(move |_| Ok(divider.clone()))
        Pickable::IGNORE
    }
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(super) struct DividerView {
    entity: Entity,
    divider: &'static mut PaneDivider,
    node: &'static mut Node,
    color: &'static mut BackgroundColor,
    camera: &'static mut UiTargetCamera,
    z: &'static mut GlobalZIndex,
}

pub(super) fn reconcile(
    mut commands: Commands,
    tree: HeaderTree,
    colors: WindowFrameColors,
    rectangles: Query<(Entity, &TileGeometry)>,
    mut views: Query<DividerView>,
    mut invalidations: Invalidations,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if !invalidations.changed()
        && !tree.focus.is_changed()
        && !tree.style.is_changed()
        && !tree.history.is_changed()
    {
        return;
    }
    let mut desired = HashMap::new();
    // Each divider occupies the space reserved between adjacent child rectangles.
    for (container, bounds) in &rectangles {
        let Ok(state) = tree.containers.get(container) else {
            continue;
        };
        let TileLayout::Split(axis) = state.layout() else {
            continue;
        };
        if !tree.in_group(container) {
            continue;
        }
        let Some(camera) = tree.camera(container) else {
            continue;
        };
        let mut previous: Option<(Entity, WindowGeometry)> = None;
        for (child, _) in state.children() {
            let Ok((_, rect)) = rectangles.get(child) else {
                previous = None;
                continue;
            };
            if let Some((leading, left)) = previous {
                let end = left.position + left.size;
                let geometry = match axis {
                    SplitAxis::Horizontal => WindowGeometry {
                        position: Vec2::new(end.x, bounds.0.position.y),
                        size: Vec2::new((rect.0.position.x - end.x).max(0.0), bounds.0.size.y),
                    },
                    SplitAxis::Vertical => WindowGeometry {
                        position: Vec2::new(bounds.0.position.x, end.y),
                        size: Vec2::new(bounds.0.size.x, (rect.0.position.y - end.y).max(0.0)),
                    },
                };
                if geometry.size.min_element() > 0.0 {
                    let side = if tree.contains_focus(child) {
                        child
                    } else {
                        leading
                    };
                    if let Some(window) = tree.leaf(side) {
                        desired.insert(
                            (container, leading),
                            PaneDivider {
                                container,
                                leading,
                                geometry,
                                camera,
                                color: colors.content_border(window),
                                z: tree.frame_z(container),
                            },
                        );
                    }
                }
            }
            previous = Some((child, rect.0));
        }
    }
    for mut view in &mut views {
        let Some(next) = desired.remove(&(view.divider.container, view.divider.leading)) else {
            commands.entity(view.entity).despawn();
            redraw.write(RequestRedraw);
            continue;
        };
        if *view.divider == next {
            continue;
        }
        *view.node = node(next.geometry);
        *view.color = BackgroundColor(next.color);
        *view.camera = UiTargetCamera(next.camera);
        *view.z = GlobalZIndex(next.z);
        *view.divider = next;
        redraw.write(RequestRedraw);
    }
    for divider in desired.into_values() {
        commands.spawn_scene(scene(divider));
        redraw.write(RequestRedraw);
    }
}
