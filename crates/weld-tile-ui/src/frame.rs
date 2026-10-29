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
        BorderColor, BorderRadius, BoxShadow, GlobalZIndex, Node, Overflow, PositionType, Scene,
        UiRect, UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, bsn},
    window::RequestRedraw,
};
use std::collections::HashMap;
use weld_app::border::BorderUnderlap;
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
    if settings.hide_solo_group_border != style.hide_solo_border {
        settings.hide_solo_group_border = style.hide_solo_border;
    }
    let height = style.title_font.header_height();
    if settings.header_height != height {
        settings.header_height = height;
    }
}

#[derive(Component, Clone, PartialEq)]
pub(super) struct GroupFrame {
    pub(super) container: Entity,
    pub(super) geometry: WindowGeometry,
    camera: Entity,
    colors: FrameColors,
    pub(super) width: f32,
    pub(super) radius: f32,
    z: i32,
    edge_to_edge: bool,
}

fn shadow(frame: &GroupFrame) -> BoxShadow {
    if frame.edge_to_edge {
        BoxShadow::default()
    } else {
        weld_ssd::window_shadow()
    }
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
        overflow: Overflow::clip(),
        ..Default::default()
    }
}

fn scene(frame: GroupFrame) -> impl Scene {
    let node = frame_node(&frame);
    let colors = frame.colors;
    let camera = frame.camera;
    let z = frame.z;
    let shadow = shadow(&frame);
    bsn! {
        #Frame
        BorderUnderlap(#Frame)
        template(move |_| Ok(node.clone()))
        template(move |_| Ok(frame.clone()))
        template(move |_| Ok(UiTargetCamera(camera)))
        GlobalZIndex(z)
        BorderColor::all(colors.border)
        template(move |_| Ok(shadow.clone()))
        Pickable::IGNORE
    }
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(super) struct FrameView {
    entity: Entity,
    frame: &'static mut GroupFrame,
    node: &'static mut Node,
    border: &'static mut BorderColor,
    camera: &'static mut UiTargetCamera,
    z: &'static mut GlobalZIndex,
    shadow: &'static mut BoxShadow,
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
        let edge_to_edge = tree.edge_to_edge(container);
        desired.insert(
            container,
            GroupFrame {
                container,
                geometry: geometry.0,
                camera,
                colors: tree.colors(container, tree.contains_focus(container), false),
                width: if edge_to_edge {
                    0.0
                } else {
                    tree.style
                        .tiled
                        .width()
                        .min(geometry.0.size.min_element() * 0.5)
                },
                radius: if edge_to_edge {
                    0.0
                } else {
                    f32::from(tree.style.corner_radius.min(64))
                },
                z: tree.frame_z(container),
                edge_to_edge,
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
        *view.border = BorderColor::all(next.colors.border);
        *view.camera = UiTargetCamera(next.camera);
        *view.z = GlobalZIndex(next.z);
        *view.shadow = shadow(&next);
        *view.frame = next;
        redraw.write(RequestRedraw);
    }
    for frame in desired.into_values() {
        commands.spawn_scene(scene(frame));
        redraw.write(RequestRedraw);
    }
}
