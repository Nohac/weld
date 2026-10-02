//! Validated reparenting and grouping over the shared ECS tree.

use bevy::ecs::{
    entity::Entity,
    observer::On,
    query::With,
    system::{Query, ResMut, SystemParam},
};
use weld_window::{ManagedBy, ManagedWindow};

use crate::{
    MAX_DEPTH, SplitAxis, TileChild, TileCommands, TileParent, TileSide, TileTreeChanged,
    TileTreeEdit, layout::LayoutRequested, operations::TreeEditor,
};

#[derive(SystemParam)]
pub(crate) struct StructuralEditor<'w, 's> {
    editor: TreeEditor<'w, 's>,
    windows: Query<'w, 's, &'static ManagedBy, With<ManagedWindow>>,
}

impl StructuralEditor<'_, '_> {
    /// Validates both directions of every ancestor edge and returns its depth.
    fn depth(&self, mut node: Entity) -> Option<usize> {
        let mut depth = 0;
        while !self.editor.roots.contains(node) {
            let parent = self.editor.parents.get(node).ok()?.entity();
            if !self
                .editor
                .containers
                .get(parent)
                .ok()?
                .children()
                .any(|(child, _)| child == node)
            {
                return None;
            }
            node = parent;
            depth += 1;
            if depth > MAX_DEPTH {
                return None;
            }
        }
        Some(depth)
    }

    /// Counts container levels below this node and validates owned live leaves.
    fn height(&self, node: Entity, remaining: usize) -> Option<usize> {
        if let Ok(container) = self.editor.containers.get(node) {
            let remaining = remaining.checked_sub(1)?;
            let mut height = 0;
            for (child, weight) in container.children() {
                if !weight.is_finite() || weight <= 0.0 {
                    return None;
                }
                if self.editor.parents.get(child).ok()?.entity() != node {
                    return None;
                }
                height = height.max(self.height(child, remaining)?);
            }
            Some(height + 1)
        } else {
            let owner = self.windows.get(node).ok()?.0;
            (self.editor.roots.contains(owner) && self.root(node)? == owner).then_some(0)
        }
    }

    fn root(&self, mut node: Entity) -> Option<Entity> {
        self.depth(node)?;
        while let Ok(parent) = self.editor.parents.get(node) {
            node = parent.entity();
        }
        Some(node)
    }

    fn place(&mut self, node: Entity, anchor: Entity, side: TileSide) -> bool {
        if node == anchor || self.root(node).is_none() || self.root(node) != self.root(anchor) {
            return false;
        }
        let (Ok(source), Ok(destination)) = (
            self.editor.parents.get(node).copied(),
            self.editor.parents.get(anchor).copied(),
        ) else {
            return false;
        };
        let Some(depth) = self.depth(anchor) else {
            return false;
        };
        if self.depth(node).is_none()
            || self.height(anchor, MAX_DEPTH).is_none()
            || self
                .height(node, MAX_DEPTH)
                .is_none_or(|height| depth + height > MAX_DEPTH)
        {
            return false;
        }
        // The destination's ancestry must not contain the node being moved.
        let mut ancestor = destination.entity();
        loop {
            if ancestor == node {
                return false;
            }
            let Ok(parent) = self.editor.parents.get(ancestor) else {
                break;
            };
            ancestor = parent.entity();
        }
        let Ok(container) = self.editor.containers.get(source.entity()) else {
            return false;
        };
        let Some(index) = container
            .children
            .iter()
            .position(|child| child.entity == node)
        else {
            return false;
        };
        let mut edge = container.children[index];
        let Ok(target) = self.editor.containers.get(destination.entity()) else {
            return false;
        };
        let Some(target_index) = target
            .children
            .iter()
            .position(|child| child.entity == anchor)
        else {
            return false;
        };
        let insertion = target_index - usize::from(source == destination && index < target_index)
            + usize::from(side == TileSide::After);
        if source != destination {
            // New arrivals receive the mean existing share; existing siblings
            // retain their relative proportions.
            edge.weight = target
                .children
                .iter()
                .map(|child| child.weight)
                .sum::<f32>()
                / target.children.len() as f32;
        }
        if !edge.weight.is_finite() || edge.weight <= 0.0 {
            return false;
        }
        if let Ok(mut container) = self.editor.containers.get_mut(source.entity()) {
            container.children.remove(index);
        }
        if let Ok(mut container) = self.editor.containers.get_mut(destination.entity()) {
            container.children.insert(insertion, edge);
        }
        if let Ok(mut parent) = self.editor.parents.get_mut(node) {
            *parent = destination;
        }
        self.remove_empty_ancestors(source.entity());
        self.editor.dirty.0 = true;
        true
    }

    fn remove_empty_ancestors(&mut self, mut node: Entity) {
        while !self.editor.roots.contains(node) {
            let Ok(container) = self.editor.containers.get(node) else {
                break;
            };
            if !container.children.is_empty() {
                break;
            }
            let Ok(parent) = self.editor.parents.get(node).copied() else {
                break;
            };
            if let Ok(mut container) = self.editor.containers.get_mut(parent.entity()) {
                container.children.retain(|child| child.entity != node);
            }
            self.editor.history.replace(node, None);
            self.editor.commands.entity(node).despawn();
            node = parent.entity();
        }
    }

    fn wrap_children(&mut self, node: Entity, axis: SplitAxis) -> bool {
        let Some(depth) = self.depth(node) else {
            return false;
        };
        let Some(height) = self.height(node, MAX_DEPTH + 1) else {
            return false;
        };
        if depth + height >= MAX_DEPTH {
            return false;
        }
        let Ok(container) = self.editor.containers.get(node) else {
            return false;
        };
        if container.axis == axis || container.children.is_empty() {
            return false;
        }
        let old_axis = container.axis;
        let children = container.children.clone();
        let Some(group) = self.editor.create_container(old_axis, children.clone()) else {
            return false;
        };
        self.editor.commands.entity(group).insert(TileParent(node));
        // Anchor the new group's history at its most-recent child.
        let recent = self
            .editor
            .history
            .recent()
            .find(|recent| children.iter().any(|child| child.entity == *recent));
        if let Some(recent) = recent {
            self.editor.history.wrap(recent, group);
        }
        for child in children {
            if let Ok(mut parent) = self.editor.parents.get_mut(child.entity) {
                parent.0 = group;
            }
        }
        if let Ok(mut container) = self.editor.containers.get_mut(node) {
            container.axis = axis;
            container.children = vec![TileChild {
                entity: group,
                weight: 1.0,
            }];
        }
        true
    }

    fn flatten(&mut self, node: Entity) -> bool {
        if self.depth(node).is_none() || self.height(node, MAX_DEPTH).is_none() {
            return false;
        }
        let Ok(parent) = self.editor.parents.get(node).copied() else {
            return false;
        };
        let Ok(container) = self.editor.containers.get(node) else {
            return false;
        };
        if container.children.len() != 1 {
            return false;
        }
        let inner = container.children[0].entity;
        let Ok(inner_container) = self.editor.containers.get(inner) else {
            return false;
        };
        let mut children = inner_container.children.clone();
        let Ok(destination) = self.editor.containers.get(parent.entity()) else {
            return false;
        };
        if children.is_empty() || inner_container.axis != destination.axis {
            return false;
        }
        let Some(index) = destination
            .children
            .iter()
            .position(|child| child.entity == node)
        else {
            return false;
        };
        let scale = destination.children[index].weight
            / children.iter().map(|child| child.weight).sum::<f32>();
        for child in &mut children {
            child.weight *= scale;
        }
        if children
            .iter()
            .any(|child| !child.weight.is_finite() || child.weight <= 0.0)
        {
            return false;
        }
        let identities: Vec<_> = children.iter().map(|child| child.entity).collect();
        if let Ok(mut destination) = self.editor.containers.get_mut(parent.entity()) {
            destination.children.splice(index..=index, children);
        }
        for child in &identities {
            if let Ok(mut edge) = self.editor.parents.get_mut(*child) {
                *edge = parent;
            }
        }
        self.editor.history.replace(inner, None);
        self.editor.history.expand(node, &identities);
        self.editor.commands.entity(inner).despawn();
        self.editor.commands.entity(node).despawn();
        self.editor.dirty.0 = true;
        true
    }
}

pub(crate) fn apply_edit(
    event: On<TileTreeEdit>,
    mut tree: StructuralEditor,
    mut pending: ResMut<TileCommands>,
) {
    if tree.editor.roots.is_empty() {
        let _ = pending.defer(*event.event());
        return;
    }
    let changed = match *event.event() {
        TileTreeEdit::Place { node, anchor, side } => tree.place(node, anchor, side),
        TileTreeEdit::WrapChildren { container, axis } => tree.wrap_children(container, axis),
        TileTreeEdit::Flatten { container } => tree.flatten(container),
    };
    if changed {
        tree.editor.commands.trigger(TileTreeChanged);
        tree.editor.commands.trigger(LayoutRequested);
    }
}
