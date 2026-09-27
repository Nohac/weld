//! Structural edits run exclusively; no presentation system sees half an edit.

use bevy::{
    ecs::{entity::Entity, world::World},
    math::Vec2,
};
use weld_window::{ManagedWindow, WindowCommand, WindowCommandKind, WindowGeometry};

use crate::{ContainerId, Direction, SplitAxis, TileChild, TileContainer, TileParent, TileState};

pub(crate) fn create_container(world: &mut World, axis: SplitAxis) -> Option<Entity> {
    let mut state = world.resource_mut::<TileState>();
    let next = state.next_id.checked_add(1)?;
    let id = ContainerId(state.next_id);
    state.next_id = next;
    Some(
        world
            .spawn(TileContainer {
                id,
                axis,
                children: Vec::new(),
            })
            .id(),
    )
}

pub(crate) fn split(world: &mut World, window: Entity, axis: SplitAxis) {
    let Some(parent) = world.get::<TileParent>(window).copied() else {
        return;
    };
    let Some(container) = world.get::<TileContainer>(parent.0) else {
        return;
    };
    if container.children.len() == 1 {
        if let Some(mut container) = world.get_mut::<TileContainer>(parent.0) {
            container.axis = axis;
        }
        return;
    }
    // Keep traversal and layout recursion bounded even for hostile IPC clients.
    let mut depth = 1;
    let mut ancestor = parent.0;
    while let Some(parent) = world.get::<TileParent>(ancestor) {
        depth += 1;
        ancestor = parent.0;
    }
    if depth >= 64 {
        return;
    }
    let Some(nested) = create_container(world, axis) else {
        return;
    };
    if let Ok(mut entity) = world.get_entity_mut(nested) {
        entity.insert(parent);
    }
    if let Some(mut container) = world.get_mut::<TileContainer>(nested) {
        container.children.push(TileChild {
            entity: window,
            weight: 1.0,
        });
    }
    if let Some(mut container) = world.get_mut::<TileContainer>(parent.0)
        && let Some(child) = container
            .children
            .iter_mut()
            .find(|child| child.entity == window)
    {
        child.entity = nested;
    }
    if let Ok(mut entity) = world.get_entity_mut(window) {
        entity.insert(TileParent(nested));
    }
}

pub(crate) fn compact(world: &mut World, parent: Entity) {
    let Some(children) = world
        .get::<TileContainer>(parent)
        .map(|container| container.children.clone())
    else {
        return;
    };
    let Some(grandparent) = world.get::<TileParent>(parent).copied() else {
        return;
    };
    match children.as_slice() {
        [] => {
            if let Some(mut container) = world.get_mut::<TileContainer>(grandparent.0) {
                container.children.retain(|entry| entry.entity != parent);
            }
            world.despawn(parent);
        }
        [only] => {
            if let Some(mut container) = world.get_mut::<TileContainer>(grandparent.0)
                && let Some(edge) = container
                    .children
                    .iter_mut()
                    .find(|entry| entry.entity == parent)
            {
                edge.entity = only.entity;
            }
            if let Ok(mut entity) = world.get_entity_mut(only.entity) {
                entity.insert(grandparent);
            }
            world.despawn(parent);
        }
        _ => {}
    }
}

pub(crate) fn neighbor(world: &mut World, window: Entity, direction: Direction) -> Option<Entity> {
    let geometry = *world.get::<WindowGeometry>(window)?;
    let center = geometry.position + geometry.size * 0.5;
    let unit = match direction {
        Direction::Left => Vec2::NEG_X,
        Direction::Right => Vec2::X,
        Direction::Up => Vec2::NEG_Y,
        Direction::Down => Vec2::Y,
    };
    world
        .query::<(Entity, &ManagedWindow, &WindowGeometry, &TileParent)>()
        .iter(world)
        .filter_map(|(entity, managed, rect, _)| {
            let delta = rect.position + rect.size * 0.5 - center;
            let forward = delta.dot(unit);
            (entity != window && forward > 0.5).then_some((
                entity,
                delta.length_squared(),
                managed.id,
            ))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1).then(left.2.cmp(&right.2)))
        .map(|(entity, _, _)| entity)
}

pub(crate) fn focus(world: &mut World, window: Entity) {
    world.trigger(WindowCommand {
        window,
        kind: WindowCommandKind::Focus,
    });
}

pub(crate) fn swap(world: &mut World, first: Entity, second: Entity) {
    let Some(first_parent) = world.get::<TileParent>(first).copied() else {
        return;
    };
    let Some(second_parent) = world.get::<TileParent>(second).copied() else {
        return;
    };
    if let Some(mut container) = world.get_mut::<TileContainer>(first_parent.0) {
        for edge in &mut container.children {
            if edge.entity == first {
                edge.entity = second;
            } else if first_parent == second_parent && edge.entity == second {
                edge.entity = first;
            }
        }
    }
    if first_parent != second_parent
        && let Some(mut container) = world.get_mut::<TileContainer>(second_parent.0)
        && let Some(edge) = container
            .children
            .iter_mut()
            .find(|edge| edge.entity == second)
    {
        edge.entity = first;
    }
    if let Ok(mut entity) = world.get_entity_mut(first) {
        entity.insert(second_parent);
    }
    if let Ok(mut entity) = world.get_entity_mut(second) {
        entity.insert(first_parent);
    }
}

pub(crate) fn resize(world: &mut World, window: Entity, axis: SplitAxis, fraction: f32) {
    if !fraction.is_finite() {
        return;
    }
    let mut branch = window;
    while let Some(parent) = world.get::<TileParent>(branch).copied() {
        let Some(mut container) = world.get_mut::<TileContainer>(parent.0) else {
            return;
        };
        if container.axis == axis && container.children.len() > 1 {
            let Some(index) = container
                .children
                .iter()
                .position(|edge| edge.entity == branch)
            else {
                return;
            };
            let other = if index + 1 < container.children.len() {
                index + 1
            } else {
                index - 1
            };
            // Both indices originate from this nonempty slice. Splitting the
            // borrow is unnecessary because updates use copied weights.
            let total = container.children[index].weight + container.children[other].weight;
            let share = (container.children[index].weight / total + fraction).clamp(0.05, 0.95);
            container.children[index].weight = total * share;
            container.children[other].weight = total * (1.0 - share);
            return;
        }
        branch = parent.0;
    }
}
