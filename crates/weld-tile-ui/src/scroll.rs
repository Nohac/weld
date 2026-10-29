//! Header scrolling selects a group's remembered branch through normal activation.

use super::{HeaderTarget, HeaderTree};
use bevy::{
    app::App,
    ecs::{
        entity::Entity,
        observer::On,
        resource::Resource,
        system::{Commands, Query, Res, ResMut},
    },
    input::{mouse::MouseScrollPixelsPerLine, touch::TouchPhase},
    picking::events::{PointerOut, PointerScroll},
};
use weld_window::{WindowIntent, WindowIntentKind};

#[derive(Resource, Default)]
struct ScrollRemainder {
    container: Option<Entity>,
    lines: f32,
}

pub(super) fn install(app: &mut App) {
    app.init_resource::<ScrollRemainder>()
        .init_resource::<MouseScrollPixelsPerLine>()
        .add_observer(scroll)
        .add_observer(leave);
}

fn leave(
    event: On<PointerOut>,
    headers: Query<&HeaderTarget>,
    mut remainder: ResMut<ScrollRemainder>,
) {
    if headers
        .get(event.entity)
        .is_ok_and(|header| Some(header.container) == remainder.container)
    {
        *remainder = ScrollRemainder::default();
    }
}

fn scroll(
    mut event: On<PointerScroll>,
    headers: Query<&HeaderTarget>,
    tree: HeaderTree,
    pixels_per_line: Res<MouseScrollPixelsPerLine>,
    mut remainder: ResMut<ScrollRemainder>,
    mut commands: Commands,
) {
    let Ok(header) = headers.get(event.entity) else {
        return;
    };
    event.propagate(false);
    let Ok(container) = tree.containers.get(header.container) else {
        return;
    };
    if container.layout().is_split() || tree.camera(header.container).is_none() {
        return;
    }
    if remainder.container != Some(header.container) || event.phase != TouchPhase::Moved {
        remainder.container = Some(header.container);
        remainder.lines = 0.0;
    }
    if matches!(event.phase, TouchPhase::Ended | TouchPhase::Canceled) {
        return;
    }
    let lines = event.to_lines(&pixels_per_line);
    let delta = if lines.y.abs() >= lines.x.abs() {
        lines.y
    } else {
        lines.x
    };
    if !delta.is_finite() {
        return;
    }
    if delta.signum() != remainder.lines.signum() {
        remainder.lines = 0.0;
    }
    remainder.lines += delta;
    let steps = remainder.lines.trunc();
    remainder.lines -= steps;
    if steps == 0.0 {
        return;
    }
    let children: Vec<_> = container.children().map(|(child, _)| child).collect();
    let Some(index) = tree
        .history
        .active_child(container)
        .and_then(|active| children.iter().position(|child| *child == active))
    else {
        return;
    };
    let backwards = steps.rem_euclid(children.len() as f32) as usize;
    let next = (index + children.len() - backwards) % children.len();
    if next != index
        && let Some(window) = tree.leaf(children[next])
    {
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::Activate,
        });
    }
}
