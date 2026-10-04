//! Continuous outer frame shared by group headers and the selected content.

use super::{HeaderTree, Invalidations};
use bevy::ecs::change_detection::DetectChanges;
use bevy::{
    ecs::template::template,
    ecs::{
        component::Component,
        entity::Entity,
        message::MessageWriter,
        query::QueryData,
        system::{Commands, Query, Res, ResMut},
    },
    picking::Pickable,
    prelude::{
        BackgroundColor, BorderColor, BorderRadius, GlobalZIndex, Node, PositionType, Scene,
        UiRect, UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, bsn},
    window::RequestRedraw,
};
use std::collections::HashMap;
use weld_ssd::{BorderStyle, FrameColors, SsdSettings};
use weld_tile::{TileGeometry, TileHeaders, TilePresentationMetrics};
use weld_window::WindowGeometry;

pub(super) fn publish_metrics(
    style: Res<SsdSettings>,
    mut settings: ResMut<TilePresentationMetrics>,
) {
    let width = match style.tiled {
        BorderStyle::None => 0,
        BorderStyle::Normal(width) | BorderStyle::Pixel(width) => width.min(64),
    };
    if settings.group_border != width {
        settings.group_border = width;
    }
}

#[derive(Component, Clone, PartialEq)]
pub(super) struct GroupFrame {
    container: Entity,
    geometry: WindowGeometry,
    camera: Entity,
    colors: FrameColors,
    width: f32,
    radius: f32,
    z: i32,
}

fn frame_node(frame: &GroupFrame) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(frame.geometry.position.x),
        top: px(frame.geometry.position.y),
        width: px(frame.geometry.size.x),
        height: px(frame.geometry.size.y),
        border: UiRect::all(px(frame.width)),
        border_radius: BorderRadius::all(px(frame.radius)),
        ..Default::default()
    }
}

fn scene(frame: GroupFrame) -> impl Scene {
    let node = frame_node(&frame);
    let colors = frame.colors;
    let camera = frame.camera;
    let z = frame.z;
    bsn! {
        template(move |_| Ok(node.clone()))
        template(move |_| Ok(frame.clone()))
        template(move |_| Ok(UiTargetCamera(camera)))
        GlobalZIndex(z)
        BackgroundColor({colors.background})
        BorderColor::all(colors.border)
        template(move |_| Ok(weld_ssd::window_shadow()))
        Pickable::IGNORE
    }
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(super) struct FrameView {
    entity: Entity,
    frame: &'static mut GroupFrame,
    node: &'static mut Node,
    background: &'static mut BackgroundColor,
    border: &'static mut BorderColor,
    camera: &'static mut UiTargetCamera,
    z: &'static mut GlobalZIndex,
}

pub(super) fn reconcile(
    mut commands: Commands,
    tree: HeaderTree,
    containers: Query<(Entity, &TileHeaders, &TileGeometry)>,
    mut views: Query<FrameView>,
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
    for (container, headers, geometry) in &containers {
        if headers.0.is_empty() || tree.in_group(container) {
            continue;
        }
        let Some(camera) = tree.camera(container) else {
            continue;
        };
        desired.insert(
            container,
            GroupFrame {
                container,
                geometry: geometry.0,
                camera,
                colors: if tree.contains_focus(container) {
                    tree.style.focused
                } else {
                    tree.style.unfocused
                },
                width: tree
                    .style
                    .tiled
                    .width()
                    .min(geometry.0.size.min_element() * 0.5),
                radius: f32::from(tree.style.corner_radius.min(64)),
                z: tree.frame_z(container),
            },
        );
    }
    for mut view in &mut views {
        let Some(next) = desired.remove(&view.frame.container) else {
            commands.entity(view.entity).despawn();
            redraw.write(RequestRedraw);
            continue;
        };
        if *view.frame == next {
            continue;
        }
        *view.node = frame_node(&next);
        *view.background = BackgroundColor(next.colors.background);
        *view.border = BorderColor::all(next.colors.border);
        *view.camera = UiTargetCamera(next.camera);
        *view.z = GlobalZIndex(next.z);
        *view.frame = next;
        redraw.write(RequestRedraw);
    }
    for frame in desired.into_values() {
        commands.spawn_scene(scene(frame));
        redraw.write(RequestRedraw);
    }
}
