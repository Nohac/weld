//! Projects domain-owned inventories into one coherent Sway IPC publication.

#[cfg(test)]
#[path = "integration_tests.rs"]
mod tests;

use crate::{
    DesktopPublisher, DesktopSnapshot, Layout, Node, NodeType, Output, Rect, X11Properties,
};
use anyhow::{Context as ContextExt, Result};
use bevy::{
    app::{App, Last, Plugin},
    prelude::*,
};
use std::collections::HashMap;
use weld_app::{
    inspection::{InspectionPlugin, InspectionSystems, OutputInventory},
    output::OutputId,
};
use weld_tile::{
    SplitAxis, TileLayout,
    inspection::{TileInventory, TileNodeId},
};
use weld_window::{
    WindowId,
    fullscreen::FullscreenMode,
    inspection::{WindowInventory, WorkspaceInventory},
};

/// Enable desktop inspection and publish Sway responses after all domain collectors.
pub struct SwayIpcPlugin(pub DesktopPublisher);

#[derive(Resource)]
struct Publisher(DesktopPublisher);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum NodeKey {
    Output(OutputId),
    Tile(TileNodeId),
}

#[derive(Resource, Default)]
struct NodeIds {
    last: u64,
    ids: HashMap<NodeKey, u64>,
}

impl NodeIds {
    fn id(&mut self, key: NodeKey) -> Result<u64> {
        if let Some(id) = self.ids.get(&key) {
            return Ok(*id);
        }
        self.last = self.last.checked_add(1).context("IPC node IDs exhausted")?;
        self.ids.insert(key, self.last);
        Ok(self.last)
    }
}

impl Plugin for SwayIpcPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<InspectionPlugin>() {
            app.add_plugins(InspectionPlugin);
        }
        app.insert_resource(Publisher(self.0.clone()))
            .init_resource::<NodeIds>()
            .add_systems(Last, publish.in_set(InspectionSystems::Publish));
    }
}

fn publish(
    outputs: Res<OutputInventory>,
    windows: Res<WindowInventory>,
    workspaces: Res<WorkspaceInventory>,
    tiles: Option<Res<TileInventory>>,
    publisher: Res<Publisher>,
    mut ids: ResMut<NodeIds>,
) {
    if !publisher.is_added()
        && !outputs.is_changed()
        && !windows.is_changed()
        && !workspaces.is_changed()
        && !tiles.as_ref().is_some_and(|tiles| tiles.is_changed())
    {
        return;
    }
    let empty_tiles = TileInventory::default();
    let desktop = Desktop {
        outputs: &outputs,
        windows: &windows,
        workspaces: &workspaces,
        tiles: tiles.as_deref().unwrap_or(&empty_tiles),
    };
    match desktop.snapshot(&mut ids) {
        Ok(snapshot) => publisher.0.publish(snapshot),
        Err(error) => tracing::warn!(%error, "could not publish desktop IPC snapshot"),
    }
}

struct Desktop<'a> {
    outputs: &'a OutputInventory,
    windows: &'a WindowInventory,
    workspaces: &'a WorkspaceInventory,
    tiles: &'a TileInventory,
}

impl Desktop<'_> {
    fn focused(&self) -> Option<TileNodeId> {
        self.tiles
            .selected()
            .or_else(|| self.windows.focused().map(TileNodeId::Window))
            .or_else(|| self.workspaces.focused().map(TileNodeId::Workspace))
    }

    fn snapshot(&self, ids: &mut NodeIds) -> Result<DesktopSnapshot> {
        let mut snapshot = DesktopSnapshot::default();
        let focused_output = self
            .workspaces
            .focused()
            .and_then(|id| self.workspaces.workspaces().get(&id))
            .and_then(|workspace| workspace.output);
        for (&id, output) in self.outputs.outputs() {
            let key = NodeKey::Output(id);
            let name = output
                .name
                .clone()
                .unwrap_or_else(|| format!("output-{}", id.raw()));
            let rect = rectangle(output.position, output.logical_size);
            let mut node = Node::new(ids.id(key)?, NodeType::Output, rect);
            node.name = Some(name.clone());
            node.layout = Layout::Output;
            let mut current_workspace = None;
            for (&workspace_id, workspace) in self
                .workspaces
                .workspaces()
                .iter()
                .filter(|(_, workspace)| workspace.output == Some(id))
            {
                let mut child =
                    self.node(TileNodeId::Workspace(workspace_id), output.position, ids, 0)?;
                child.kind = NodeType::Workspace;
                child.name = Some(workspace.name.clone());
                child.visible = workspace.visible;
                if workspace.visible {
                    current_workspace = child.name.clone();
                }
                for (&window_id, _) in
                    self.windows.windows().iter().filter(|(_, window)| {
                        window.workspace == Some(workspace_id) && window.floating
                    })
                {
                    child.floating_nodes.push(self.node(
                        TileNodeId::Window(window_id),
                        output.position,
                        ids,
                        0,
                    )?);
                }
                // Keep tiling branches and floating windows together in the workspace's focus list.
                child.urgent |= child.floating_nodes.iter().any(|window| window.urgent);
                let recent = workspace
                    .focus_history
                    .iter()
                    .filter_map(|&id| ids.ids.get(&NodeKey::Tile(TileNodeId::Window(id))).copied());
                child.focus = focus_order(&child, recent.chain(child.focus.iter().copied()));
                node.nodes.push(child);
            }
            let workspace_focus = self.workspaces.focused().and_then(|id| {
                ids.ids
                    .get(&NodeKey::Tile(TileNodeId::Workspace(id)))
                    .copied()
            });
            node.focus = focus_order(&node, workspace_focus);
            node.urgent = node.nodes.iter().any(|workspace| workspace.urgent);
            snapshot.outputs.push(Output {
                name,
                active: true,
                focused: focused_output == Some(id) || (focused_output.is_none() && output.primary),
                primary: output.primary,
                rect,
                scale: output.scale,
                current_workspace,
            });
            snapshot.tree.nodes.push(node);
        }
        ids.ids.retain(|key, _| match key {
            NodeKey::Output(id) => self.outputs.outputs().contains_key(id),
            NodeKey::Tile(TileNodeId::Window(id)) => self.windows.windows().contains_key(id),
            NodeKey::Tile(TileNodeId::Workspace(id)) => {
                self.workspaces.workspaces().contains_key(id)
            }
            NodeKey::Tile(id) => self.tiles.containers().contains_key(id),
        });
        snapshot.tree.rect = union(snapshot.outputs.iter().map(|output| output.rect));
        snapshot.tree.urgent = snapshot.tree.nodes.iter().any(|output| output.urgent);
        snapshot.tree.focus = focus_order(
            &snapshot.tree,
            focused_output.and_then(|id| ids.ids.get(&NodeKey::Output(id)).copied()),
        );
        Ok(snapshot)
    }

    fn node(&self, key: TileNodeId, origin: Vec2, ids: &mut NodeIds, depth: usize) -> Result<Node> {
        anyhow::ensure!(
            depth <= weld_tile::MAX_DEPTH,
            "IPC tree exceeds tiling depth"
        );
        let id = ids.id(NodeKey::Tile(key))?;
        if let TileNodeId::Window(window_id) = key {
            return self.window(window_id, id, origin);
        }
        let mut node = Node::new(id, NodeType::Con, Rect::default());
        node.focused = self.focused() == Some(key);
        let Some(container) = self.tiles.containers().get(&key) else {
            // Floating-only window managers can publish a workspace without a tiling tree.
            if let TileNodeId::Workspace(workspace_id) = key {
                if let Some(output) = self
                    .workspaces
                    .workspaces()
                    .get(&workspace_id)
                    .and_then(|workspace| workspace.output)
                    .and_then(|id| self.outputs.outputs().get(&id))
                {
                    node.rect = rectangle(output.position, output.logical_size);
                }
                for (&window_id, _) in self.windows.windows().iter().filter(|(_, window)| {
                    window.workspace == Some(workspace_id) && !window.floating
                }) {
                    node.nodes.push(self.window(
                        window_id,
                        ids.id(NodeKey::Tile(TileNodeId::Window(window_id)))?,
                        origin,
                    )?);
                }
            }
            node.urgent = node.nodes.iter().any(|child| child.urgent);
            return Ok(node);
        };
        node.rect = rectangle(
            origin + container.geometry.position,
            container.geometry.size,
        );
        node.layout = match container.layout {
            TileLayout::Split(SplitAxis::Horizontal) => Layout::Splith,
            TileLayout::Split(SplitAxis::Vertical) => Layout::Splitv,
            TileLayout::Tabbed => Layout::Tabbed,
            TileLayout::Stacked => Layout::Stacked,
        };
        for &child in &container.children {
            node.nodes.push(self.node(child, origin, ids, depth + 1)?);
        }
        node.urgent = node.nodes.iter().any(|child| child.urgent);
        node.focus = focus_order(
            &node,
            container
                .focus_history
                .iter()
                .filter_map(|key| ids.ids.get(&NodeKey::Tile(*key)).copied()),
        );
        Ok(node)
    }

    fn window(&self, window_id: WindowId, id: u64, origin: Vec2) -> Result<Node> {
        let window = self
            .windows
            .windows()
            .get(&window_id)
            .context("IPC tree references an absent window")?;
        let origin = window
            .output
            .and_then(|id| self.outputs.outputs().get(&id))
            .map_or(origin, |output| output.position);
        let mut node = Node::new(
            id,
            if window.floating {
                NodeType::FloatingCon
            } else {
                NodeType::Con
            },
            rectangle(origin + window.geometry.position, window.geometry.size),
        );
        node.focused = self.focused() == Some(TileNodeId::Window(window_id));
        node.visible = window.visible;
        node.urgent = window.urgent;
        node.sticky = window.sticky;
        node.fullscreen_mode = match window.fullscreen {
            None => 0,
            Some(FullscreenMode::Normal) => 1,
            Some(FullscreenMode::Exclusive) => 2,
        };
        node.window_rect = rectangle(window.content.position, window.content.size);
        if let Some(metadata) = &window.occupant {
            node.pid = Some(metadata.local_process_id().unwrap_or(0));
            node.name = Some(metadata.title().to_owned());
            if let Some(class) = metadata.x11_class() {
                node.window_properties = Some(X11Properties {
                    class: class.to_owned(),
                    instance: metadata.x11_instance().unwrap_or_default().to_owned(),
                    title: metadata.title().to_owned(),
                });
            } else {
                node.app_id = Some(metadata.app_id().to_owned());
            }
        }
        Ok(node)
    }
}

fn rectangle(position: Vec2, size: Vec2) -> Rect {
    let min = position.round().as_ivec2();
    let max = (position + size.max(Vec2::ZERO)).round().as_ivec2();
    Rect {
        x: min.x,
        y: min.y,
        width: max.x.saturating_sub(min.x).max(0),
        height: max.y.saturating_sub(min.y).max(0),
    }
}

fn union(rectangles: impl Iterator<Item = Rect>) -> Rect {
    let mut bounds: Option<(Vec2, Vec2)> = None;
    for rect in rectangles {
        let min = Vec2::new(rect.x as f32, rect.y as f32);
        let max = min + Vec2::new(rect.width as f32, rect.height as f32);
        bounds = Some(bounds.map_or((min, max), |(a, b)| (a.min(min), b.max(max))));
    }
    bounds.map_or_else(Rect::default, |(min, max)| rectangle(min, max - min))
}

fn focus_order(node: &Node, history: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let children: Vec<_> = node.nodes.iter().chain(&node.floating_nodes).collect();
    let mut order = Vec::new();
    for id in history
        .into_iter()
        .chain(children.iter().map(|node| node.id))
    {
        if let Some(child) = children.iter().find(|child| contains(child, id))
            && !order.contains(&child.id)
        {
            order.push(child.id);
        }
    }
    order
}

fn contains(node: &Node, id: u64) -> bool {
    node.id == id
        || node
            .nodes
            .iter()
            .chain(&node.floating_nodes)
            .any(|child| contains(child, id))
}
