//! Whole-window painter order shared by native composition and ray picking.
//! Related toplevels move independently; popups and client layers stay grouped.
use std::collections::BTreeMap;

const DEPTH_HYSTERESIS: f32 = 0.03;

pub(super) fn pick_in_front(
    order: i32,
    distance: f32,
    previous_order: i32,
    previous_distance: f32,
) -> bool {
    order > previous_order || (order == previous_order && distance < previous_distance)
}

pub(super) struct Layer {
    pub id: u64,
    pub window: u64,
    pub popup_parent: Option<u64>,
    pub root: bool,
    pub stack: i32,
    pub distance: f32,
}

#[derive(Default)]
pub(super) struct Stack {
    families: Vec<u64>,
}
impl Stack {
    /// Higher rank is in front. Near-equal distances keep the previous order,
    /// avoiding changes on tiny head/hand movements or a comparator with cycles.
    pub fn update(&mut self, layers: &[Layer]) -> BTreeMap<u64, i32> {
        let roots: BTreeMap<_, _> = layers
            .iter()
            .filter(|layer| layer.root && layer.distance.is_finite())
            .map(|layer| (layer.window, layer))
            .collect();
        let mut family_for = BTreeMap::new();
        for root in roots.values() {
            let mut ancestor = *root;
            let mut depth = 0;
            while let Some(parent) = ancestor.popup_parent.and_then(|id| roots.get(&id)) {
                if depth >= layers.len() {
                    break;
                }
                depth += 1;
                ancestor = parent;
            }
            if depth < layers.len() {
                family_for.insert(root.window, (ancestor.window, depth));
            }
        }
        let distances: BTreeMap<_, _> = family_for
            .values()
            .filter_map(|(family, _)| roots.get(family).map(|root| (*family, root.distance)))
            .collect();
        self.families.retain(|id| distances.contains_key(id));
        for id in distances.keys() {
            if !self.families.contains(id) {
                self.families.push(*id);
            }
        }
        // Bounded insertion sort, far to near. Ties retain stable admission order.
        for next in 1..self.families.len() {
            let mut index = next;
            while index > 0
                && distances[&self.families[index]]
                    > distances[&self.families[index - 1]] + DEPTH_HYSTERESIS
            {
                self.families.swap(index, index - 1);
                index -= 1;
            }
        }
        let mut ordered: Vec<_> = layers
            .iter()
            .filter_map(|layer| {
                let (family, depth) = family_for.get(&layer.window)?;
                let rank = self.families.iter().position(|id| id == family)?;
                Some((
                    (rank, *depth, layer.window, layer.stack, layer.id),
                    layer.id,
                ))
            })
            .collect();
        ordered.sort_by_key(|(key, _)| *key);
        ordered
            .into_iter()
            .enumerate()
            .map(|(rank, (_, id))| (id, rank as i32))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root(id: u64, distance: f32) -> Layer {
        Layer {
            id,
            window: id,
            popup_parent: None,
            root: true,
            stack: 0,
            distance,
        }
    }
    #[test]
    fn picking_follows_whole_window_order_even_when_planes_intersect() {
        assert!(pick_in_front(-90, 3.0, -91, 2.0));
        assert!(!pick_in_front(-91, 2.0, -90, 3.0));
        assert!(pick_in_front(-90, 2.0, -90, 3.0));
    }
    #[test]
    fn move_reorders_whole_windows_but_small_depth_changes_do_not_flicker() {
        let mut stack = Stack::default();
        let initial = stack.update(&[root(1, 2.0), root(2, 3.0)]);
        assert!(initial[&1] > initial[&2]);
        let tied = stack.update(&[root(1, 3.01), root(2, 3.0)]);
        assert!(tied[&1] > tied[&2]);
        let crossed = stack.update(&[root(1, 3.1), root(2, 3.0)]);
        assert!(crossed[&2] > crossed[&1]);
        assert_eq!(stack.update(&[]).len(), 0);
    }
    #[test]
    fn popups_and_subsurfaces_cannot_interleave_another_window() {
        let mut stack = Stack::default();
        let mut popup = root(3, 0.5);
        popup.popup_parent = Some(1);
        let child = Layer {
            id: 4,
            window: 1,
            popup_parent: None,
            root: false,
            stack: 1,
            distance: 0.5,
        };
        let order = stack.update(&[root(1, 3.0), root(2, 2.0), popup, child]);
        assert!(order[&1] < order[&4] && order[&4] < order[&3] && order[&3] < order[&2]);
        let order = stack.update(&[root(1, 1.0), root(2, 2.0)]);
        assert!(order[&1] > order[&2]);
    }
}
