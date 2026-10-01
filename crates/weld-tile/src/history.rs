//! Selection history for tree nodes, preserved by structural edits.

use bevy::ecs::{
    entity::Entity,
    observer::On,
    query::Changed,
    resource::Resource,
    system::{Query, Res, ResMut},
};
use weld_window::{FocusedWindow, WindowFocusChanged};

use crate::{TileParent, TileState, TileTreeChanged};

/// Most recently selected leaves and their ancestor branches. Policies can
/// filter this order to a container's direct children and descend recursively.
#[derive(Resource, Default)]
pub struct TileFocusHistory(Vec<Entity>);

impl TileFocusHistory {
    pub fn recent(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }

    fn promote(&mut self, node: Entity) {
        if self.0.first() == Some(&node) {
            return;
        }
        self.0.retain(|entry| *entry != node);
        self.0.insert(0, node);
    }

    pub(crate) fn wrap(&mut self, child: Entity, container: Entity) {
        if let Some(index) = self.0.iter().position(|node| *node == child) {
            self.0.insert(index, container);
        }
    }

    /// A promoted child inherits the collapsed branch's position among its
    /// new siblings, even if its former sibling was focused more recently.
    pub(crate) fn replace(&mut self, old: Entity, replacement: Option<Entity>) {
        let Some(index) = self.0.iter().position(|node| *node == old) else {
            return;
        };
        let insertion = self.0[..index]
            .iter()
            .filter(|node| Some(**node) != replacement)
            .count();
        self.0
            .retain(|node| *node != old && Some(*node) != replacement);
        if let Some(replacement) = replacement {
            self.0.insert(insertion, replacement);
        }
    }

    pub(crate) fn expand(&mut self, old: Entity, children: &[Entity]) {
        let Some(index) = self.0.iter().position(|node| *node == old) else {
            return;
        };
        let insertion = self.0[..index]
            .iter()
            .filter(|node| !children.contains(node))
            .count();
        let mut ordered: Vec<_> = self
            .recent()
            .filter(|node| children.contains(node))
            .collect();
        for child in children {
            if !ordered.contains(child) {
                ordered.push(*child);
            }
        }
        self.0
            .retain(|node| *node != old && !children.contains(node));
        self.0.splice(insertion..insertion, ordered);
    }

    fn remember(&mut self, window: Entity, parents: &Query<&TileParent>, root: Entity) {
        let mut ancestor = window;
        while let Ok(parent) = parents.get(ancestor) {
            ancestor = parent.entity();
        }
        if ancestor != root {
            return;
        }
        self.promote(window);
        let mut child = window;
        while let Ok(parent) = parents.get(child) {
            child = parent.entity();
            self.promote(child);
        }
    }
}

pub(crate) fn remember_focus(
    event: On<WindowFocusChanged>,
    parents: Query<&TileParent>,
    state: Res<TileState>,
    mut history: ResMut<TileFocusHistory>,
) {
    if let (Some(window), Some(root)) = (event.window, state.root) {
        history.remember(window, &parents, root);
    }
}

pub(crate) fn remember_tree_change(
    _: On<TileTreeChanged>,
    focus: Res<FocusedWindow>,
    parents: Query<&TileParent>,
    state: Res<TileState>,
    mut history: ResMut<TileFocusHistory>,
) {
    if let (Some(window), Some(root)) = (focus.entity(), state.root) {
        history.remember(window, &parents, root);
    }
}

pub(crate) fn refresh_path(
    focus: Res<FocusedWindow>,
    parents: Query<&TileParent>,
    changed: Query<(), Changed<TileParent>>,
    state: Res<TileState>,
    mut history: ResMut<TileFocusHistory>,
) {
    if !changed.is_empty()
        && let (Some(window), Some(root)) = (focus.entity(), state.root)
    {
        history.remember(window, &parents, root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::world::World;

    #[test]
    fn wrapping_preserves_other_siblings_and_the_childs_recency() {
        let mut world = World::new();
        let newer = world.spawn_empty().id();
        let child = world.spawn_empty().id();
        let older = world.spawn_empty().id();
        let wrapper = world.spawn_empty().id();
        let mut history = TileFocusHistory(vec![newer, child, older]);
        history.wrap(child, wrapper);
        assert_eq!(history.0, [newer, wrapper, child, older]);
    }

    #[test]
    fn collapse_inherits_branch_rank_with_no_duplicate_survivor() {
        let mut world = World::new();
        let branch = world.spawn_empty().id();
        let child = world.spawn_empty().id();
        let sibling = world.spawn_empty().id();
        for entries in [vec![branch, sibling, child], vec![child, branch, sibling]] {
            let mut history = TileFocusHistory(entries);
            history.replace(branch, Some(child));
            assert_eq!(history.0, [child, sibling]);
        }
        let mut history = TileFocusHistory(vec![sibling, child, branch]);
        history.replace(branch, Some(child));
        assert_eq!(history.0, [sibling, child]);
    }

    #[test]
    fn removing_or_replacing_an_unrecorded_node_preserves_existing_order() {
        let mut world = World::new();
        let first = world.spawn_empty().id();
        let second = world.spawn_empty().id();
        let missing = world.spawn_empty().id();
        let mut history = TileFocusHistory(vec![first, second]);
        history.replace(missing, Some(first));
        assert_eq!(history.0, [first, second]);
        history.replace(first, None);
        assert_eq!(history.0, [second]);
    }

    #[test]
    fn expanding_a_group_preserves_its_rank_and_child_focus_order() {
        let mut world = World::new();
        let group = world.spawn_empty().id();
        let first = world.spawn_empty().id();
        let second = world.spawn_empty().id();
        let external = world.spawn_empty().id();
        let mut history = TileFocusHistory(vec![group, external, second, first]);
        history.expand(group, &[first, second]);
        assert_eq!(history.0, [second, first, external]);
    }
}
